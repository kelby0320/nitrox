//! TSM1 wire layer — the byte-level codec for typed streams.
//!
//! This module is the **canonical source** for the parts the spec
//! (`docs/spec/typed-stream-format.md`) leaves "to `libstream`": the [`TypeTag`] byte
//! values and the exact value/schema/header/terminator encodings. Everything is
//! little-endian (native on x86_64/aarch64); the `TSM1` magic is the literal bytes
//! `b"TSM1"`.
//!
//! The codec is transport-agnostic: it writes through [`ByteSink`] and reads through
//! [`ByteSource`], so the same code buffers into a `Vec<u8>` (host tests, or an IPC
//! message frame) or streams straight to a channel adapter (a later part).
//!
//! **v1 scope: flat records.** [`Value`] and [`read_value`]/[`write_value`] implement
//! the scalar + `String`/`Bytes`/`Handle` types. The `List` (a `Vec`) and `Record`
//! (nested-struct) tags are pinned and recognised but return [`WireError::Unsupported`]
//! when encoded/decoded — they land with their first consumer (the shell). `Null`,
//! nullable fields, error records, and the terminator are all implemented.

use alloc::string::String;
use alloc::vec::Vec;

/// The 4-byte stream magic — ASCII `"TSM1"` (Typed Stream Magic, version 1).
pub const MAGIC: [u8; 4] = *b"TSM1";

/// Upper bound used only as a `Vec::with_capacity` *hint* when decoding a schema's
/// field list, so a malformed huge `field_count` cannot trigger a giant up-front
/// allocation. The actual count is still honoured (each field read is bounds-checked
/// against the input, so an over-large count simply errors out on EOF).
const SCHEMA_FIELD_HINT_CAP: usize = 64;

// --- Type tags --------------------------------------------------------------

/// Structural type of a schema field / value. The byte values are **pinned here** —
/// the spec defers them to `libstream`. v1 encodes `Null`..=`Handle`; `List`/`Record`
/// (and the value-level `Error`) are reserved: recognised on the wire but not yet
/// encoded/decoded (see the module docs).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum TypeTag {
    /// Absent value; zero bytes on the wire.
    Null = 0x00,
    /// 1 byte, `0` or `1`.
    Bool = 0x01,
    /// 8 bytes, little-endian `i64`.
    Int = 0x02,
    /// 8 bytes, IEEE-754 binary64 (little-endian).
    Float = 0x03,
    /// `u32` length + that many bytes of UTF-8.
    String = 0x04,
    /// `u32` length + that many raw bytes.
    Bytes = 0x05,
    /// 8 bytes, little-endian `RawHandle` (`u64`). The value here is the numeric
    /// handle; making it valid in the receiver is the transport's job (IPC handle
    /// transfer), not this codec's.
    Handle = 0x06,
    /// `u32` count + that many inner-type encodings. **Reserved in v1.**
    List = 0x07,
    /// Nested sub-schema + values. **Reserved in v1.**
    Record = 0x08,
    /// Nested error structure (as a field value, distinct from an error *record*).
    /// **Reserved in v1.**
    Error = 0x09,
}

impl TypeTag {
    /// Decode a tag byte, or `None` if it is not a known tag.
    pub fn from_u8(b: u8) -> Option<Self> {
        Some(match b {
            0x00 => TypeTag::Null,
            0x01 => TypeTag::Bool,
            0x02 => TypeTag::Int,
            0x03 => TypeTag::Float,
            0x04 => TypeTag::String,
            0x05 => TypeTag::Bytes,
            0x06 => TypeTag::Handle,
            0x07 => TypeTag::List,
            0x08 => TypeTag::Record,
            0x09 => TypeTag::Error,
            _ => return None,
        })
    }

    /// The tag's wire byte.
    pub fn to_u8(self) -> u8 {
        self as u8
    }
}

/// Record tag: a **data record** (a row).
pub const REC_DATA: u8 = 0x01;
/// Record tag: an **error record** (a structured error embedded mid-stream).
pub const REC_ERROR: u8 = 0x02;
/// Record tag: a **widget record** (structured UI; reserved for the GUI era).
pub const REC_WIDGET: u8 = 0x03;
/// Record tag: the **terminator** (end of stream, carries the producer's exit status).
pub const REC_TERMINATOR: u8 = 0xFF;

// --- Flag / modifier bitfields (hand-rolled; no `bitflags` crate) -----------

/// Stream-level flags in the header (`StreamFlags` in the spec).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StreamFlags(pub u32);

impl StreamFlags {
    /// No flags set.
    pub const NONE: StreamFlags = StreamFlags(0);
    /// Body is compressed. **Reserved** — not honoured by v1.
    pub const COMPRESSED: StreamFlags = StreamFlags(1 << 0);
    /// This stream is text wrapped as `Table<String>` with column `"line"`.
    pub const TEXT_FALLBACK: StreamFlags = StreamFlags(1 << 1);

    /// `true` if every bit of `f` is set in `self`.
    pub const fn contains(self, f: StreamFlags) -> bool {
        self.0 & f.0 == f.0
    }

    /// The union of two flag sets.
    pub const fn union(self, f: StreamFlags) -> StreamFlags {
        StreamFlags(self.0 | f.0)
    }
}

/// Per-field type modifiers (`TypeModifiers` in the spec).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TypeModifiers(pub u8);

impl TypeModifiers {
    /// No modifiers.
    pub const NONE: TypeModifiers = TypeModifiers(0);
    /// The field's value may be `Null` even if its `TypeTag` is not `Null` — on the
    /// wire the value is preceded by a presence byte (`0` absent, `1` present).
    pub const NULLABLE: TypeModifiers = TypeModifiers(1 << 0);

    /// `true` if every bit of `m` is set in `self`.
    pub const fn contains(self, m: TypeModifiers) -> bool {
        self.0 & m.0 == m.0
    }
}

// --- Errors -----------------------------------------------------------------

/// A wire encode/decode failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum WireError {
    /// Ran off the end of the input (truncated stream).
    UnexpectedEof,
    /// The header did not start with the `TSM1` magic.
    BadMagic,
    /// A schema/value carried a `TypeTag` byte this build does not recognise.
    BadTypeTag(u8),
    /// A record began with a tag byte this build does not recognise.
    BadRecordTag(u8),
    /// A `String` field was not valid UTF-8.
    BadUtf8,
    /// A recognised but not-yet-implemented type (`List`/`Record`/`Error` in v1).
    Unsupported(TypeTag),
    /// The sink refused the write (e.g. a fixed frame is full).
    SinkFull,
}

/// Codec result.
pub type Result<T> = core::result::Result<T, WireError>;

// --- Byte sink / source seams ----------------------------------------------

/// A byte destination for the encoder. `Vec<u8>` implements it (buffering / host
/// tests); a channel adapter implements it over an IPC frame + `sys_channel_send`.
pub trait ByteSink {
    /// Append `bytes`, or fail with [`WireError::SinkFull`] if it cannot.
    fn put(&mut self, bytes: &[u8]) -> Result<()>;
}

impl ByteSink for Vec<u8> {
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        self.extend_from_slice(bytes);
        Ok(())
    }
}

/// A bounds-checked cursor over an in-memory byte buffer for the decoder.
pub struct ByteSource<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> ByteSource<'a> {
    /// Wrap a buffer.
    pub fn new(buf: &'a [u8]) -> Self {
        ByteSource { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// `true` once the whole buffer has been consumed.
    pub fn is_empty(&self) -> bool {
        self.pos >= self.buf.len()
    }

    /// Borrow the next `n` bytes and advance, or [`WireError::UnexpectedEof`].
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(WireError::UnexpectedEof);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    /// Read one byte.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.arr::<1>()?[0])
    }
    /// Read a little-endian `u16`.
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.arr()?))
    }
    /// Read a little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    /// Read a little-endian `i32`.
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.arr()?))
    }
    /// Read a little-endian `i64`.
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_le_bytes(self.arr()?))
    }
    /// Read a little-endian `u64`.
    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    /// Read a little-endian IEEE-754 `f64`.
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_le_bytes(self.arr()?))
    }

    /// Read a `u32`-length-prefixed UTF-8 string.
    pub fn string(&mut self) -> Result<String> {
        let n = self.u32()? as usize;
        let bytes = self.take(n)?;
        let s = core::str::from_utf8(bytes).map_err(|_| WireError::BadUtf8)?;
        Ok(String::from(s))
    }

    /// Read a `u32`-length-prefixed byte blob.
    pub fn bytes(&mut self) -> Result<Vec<u8>> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }
}

// --- Primitive writers ------------------------------------------------------

/// Write one byte.
pub fn put_u8(s: &mut impl ByteSink, v: u8) -> Result<()> {
    s.put(&[v])
}
/// Write a little-endian `u16`.
pub fn put_u16(s: &mut impl ByteSink, v: u16) -> Result<()> {
    s.put(&v.to_le_bytes())
}
/// Write a little-endian `u32`.
pub fn put_u32(s: &mut impl ByteSink, v: u32) -> Result<()> {
    s.put(&v.to_le_bytes())
}
/// Write a little-endian `i32`.
pub fn put_i32(s: &mut impl ByteSink, v: i32) -> Result<()> {
    s.put(&v.to_le_bytes())
}

/// Write a `u32`-length-prefixed byte blob.
pub fn put_lenpfx(s: &mut impl ByteSink, bytes: &[u8]) -> Result<()> {
    put_u32(s, bytes.len() as u32)?;
    s.put(bytes)
}

// --- Values -----------------------------------------------------------------

/// A single typed value — the payload of one field in a data record. v1 covers the
/// scalar + `String`/`Bytes`/`Handle` types; `List`/`Record` values are reserved.
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    /// Absent (a `NULLABLE` field with no value, or a `Null`-typed field).
    Null,
    /// A boolean.
    Bool(bool),
    /// A signed 64-bit integer.
    Int(i64),
    /// A 64-bit float.
    Float(f64),
    /// A UTF-8 string.
    Str(String),
    /// A raw byte blob.
    Bytes(Vec<u8>),
    /// A raw handle value (numeric; transport makes it live in the receiver).
    Handle(u64),
}

impl Value {
    /// The [`TypeTag`] this value encodes as. `Null` reports [`TypeTag::Null`]; a
    /// nullable field carries its declared tag in the schema regardless.
    pub fn type_tag(&self) -> TypeTag {
        match self {
            Value::Null => TypeTag::Null,
            Value::Bool(_) => TypeTag::Bool,
            Value::Int(_) => TypeTag::Int,
            Value::Float(_) => TypeTag::Float,
            Value::Str(_) => TypeTag::String,
            Value::Bytes(_) => TypeTag::Bytes,
            Value::Handle(_) => TypeTag::Handle,
        }
    }
}

/// Encode a value's bytes (no tag byte — the tag lives in the schema, and any
/// `NULLABLE` presence byte is written by the record layer).
pub fn write_value(sink: &mut impl ByteSink, v: &Value) -> Result<()> {
    match v {
        Value::Null => Ok(()),
        Value::Bool(b) => put_u8(sink, *b as u8),
        Value::Int(i) => sink.put(&i.to_le_bytes()),
        Value::Float(f) => sink.put(&f.to_le_bytes()),
        Value::Str(s) => put_lenpfx(sink, s.as_bytes()),
        Value::Bytes(b) => put_lenpfx(sink, b),
        Value::Handle(h) => sink.put(&h.to_le_bytes()),
    }
}

/// Decode a value of the given `tag`. Does not consume a `NULLABLE` presence byte —
/// that is the record layer's job (it reads presence, then calls this if present).
/// Returns [`WireError::Unsupported`] for the reserved `List`/`Record`/`Error` tags.
pub fn read_value(src: &mut ByteSource, tag: TypeTag) -> Result<Value> {
    Ok(match tag {
        TypeTag::Null => Value::Null,
        TypeTag::Bool => Value::Bool(src.u8()? != 0),
        TypeTag::Int => Value::Int(src.i64()?),
        TypeTag::Float => Value::Float(src.f64()?),
        TypeTag::String => Value::Str(src.string()?),
        TypeTag::Bytes => Value::Bytes(src.bytes()?),
        TypeTag::Handle => Value::Handle(src.u64()?),
        TypeTag::List | TypeTag::Record | TypeTag::Error => {
            return Err(WireError::Unsupported(tag));
        }
    })
}

// --- Schema -----------------------------------------------------------------

/// One column of a schema: a name, a type, and modifiers.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct FieldDef {
    /// UTF-8 column name.
    pub name: String,
    /// The column's structural type.
    pub ty: TypeTag,
    /// Per-field modifiers (e.g. [`TypeModifiers::NULLABLE`]).
    pub modifiers: TypeModifiers,
}

/// A schema: the ordered columns of a stream. A zero-field schema is valid (a
/// record-free stream — just a terminator).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Schema {
    /// The columns, in declaration (and wire) order.
    pub fields: Vec<FieldDef>,
}

impl Schema {
    /// An empty schema.
    pub fn new() -> Self {
        Schema { fields: Vec::new() }
    }

    /// Builder: append a column and return `self`.
    pub fn field(mut self, name: &str, ty: TypeTag, modifiers: TypeModifiers) -> Self {
        self.fields.push(FieldDef {
            name: String::from(name),
            ty,
            modifiers,
        });
        self
    }

    /// Encode `field_count` then each field (`name_len`, `name`, `type`, `modifiers`).
    pub fn encode(&self, sink: &mut impl ByteSink) -> Result<()> {
        put_u32(sink, self.fields.len() as u32)?;
        for f in &self.fields {
            put_u16(sink, f.name.len() as u16)?;
            sink.put(f.name.as_bytes())?;
            put_u8(sink, f.ty.to_u8())?;
            put_u8(sink, f.modifiers.0)?;
        }
        Ok(())
    }

    /// Decode a schema (inverse of [`encode`](Schema::encode)).
    pub fn decode(src: &mut ByteSource) -> Result<Schema> {
        let count = src.u32()? as usize;
        let mut fields = Vec::with_capacity(count.min(SCHEMA_FIELD_HINT_CAP));
        for _ in 0..count {
            let name_len = src.u16()? as usize;
            let name = core::str::from_utf8(src.take(name_len)?)
                .map_err(|_| WireError::BadUtf8)?
                .into();
            let ty_byte = src.u8()?;
            let ty = TypeTag::from_u8(ty_byte).ok_or(WireError::BadTypeTag(ty_byte))?;
            let modifiers = TypeModifiers(src.u8()?);
            fields.push(FieldDef { name, ty, modifiers });
        }
        Ok(Schema { fields })
    }
}

// --- Header -----------------------------------------------------------------

/// Encode the stream header: `TSM1` magic, `flags`, then the `schema`.
pub fn encode_header(sink: &mut impl ByteSink, flags: StreamFlags, schema: &Schema) -> Result<()> {
    sink.put(&MAGIC)?;
    put_u32(sink, flags.0)?;
    schema.encode(sink)
}

/// Decode the stream header, returning the flags and schema.
pub fn decode_header(src: &mut ByteSource) -> Result<(StreamFlags, Schema)> {
    if src.take(4)? != MAGIC {
        return Err(WireError::BadMagic);
    }
    let flags = StreamFlags(src.u32()?);
    let schema = Schema::decode(src)?;
    Ok((flags, schema))
}

// --- Terminator -------------------------------------------------------------

/// Write the terminator record: tag `0xFF` + the producer's `exit_status`.
pub fn encode_terminator(sink: &mut impl ByteSink, exit_status: i32) -> Result<()> {
    put_u8(sink, REC_TERMINATOR)?;
    put_i32(sink, exit_status)
}

// --- Error record -----------------------------------------------------------

/// A structured error embedded mid-stream (record tag [`REC_ERROR`]). Generic
/// operators pass these through unchanged; the consuming end renders them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WireErrorRecord {
    /// `KError` discriminant, or `0` if not a kernel error.
    pub code: i32,
    /// Human-readable message.
    pub message: String,
    /// Name of the field that caused the error, or empty.
    pub field_name: String,
}

impl WireErrorRecord {
    /// Encode the record **including** its `0x02` tag.
    pub fn encode(&self, sink: &mut impl ByteSink) -> Result<()> {
        put_u8(sink, REC_ERROR)?;
        put_i32(sink, self.code)?;
        put_lenpfx(sink, self.message.as_bytes())?;
        put_u16(sink, self.field_name.len() as u16)?;
        sink.put(self.field_name.as_bytes())
    }

    /// Decode the record **body**, i.e. after the `0x02` tag has already been read
    /// (the record loop reads the tag to dispatch, then calls this).
    pub fn decode_body(src: &mut ByteSource) -> Result<WireErrorRecord> {
        let code = src.i32()?;
        let message = src.string()?;
        let field_name_len = src.u16()? as usize;
        let field_name = core::str::from_utf8(src.take(field_name_len)?)
            .map_err(|_| WireError::BadUtf8)?
            .into();
        Ok(WireErrorRecord {
            code,
            message,
            field_name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn type_tag_round_trips_and_rejects_unknown() {
        for tag in [
            TypeTag::Null,
            TypeTag::Bool,
            TypeTag::Int,
            TypeTag::Float,
            TypeTag::String,
            TypeTag::Bytes,
            TypeTag::Handle,
            TypeTag::List,
            TypeTag::Record,
            TypeTag::Error,
        ] {
            assert_eq!(TypeTag::from_u8(tag.to_u8()), Some(tag));
        }
        assert_eq!(TypeTag::from_u8(0x0A), None);
        assert_eq!(TypeTag::from_u8(0xFF), None);
    }

    #[test]
    fn flags_and_modifiers() {
        let f = StreamFlags::TEXT_FALLBACK.union(StreamFlags::COMPRESSED);
        assert!(f.contains(StreamFlags::TEXT_FALLBACK));
        assert!(f.contains(StreamFlags::COMPRESSED));
        assert!(!StreamFlags::NONE.contains(StreamFlags::TEXT_FALLBACK));
        assert!(TypeModifiers::NULLABLE.contains(TypeModifiers::NULLABLE));
        assert!(!TypeModifiers::NONE.contains(TypeModifiers::NULLABLE));
    }

    fn value_round_trip(v: Value) {
        let mut buf = Vec::new();
        write_value(&mut buf, &v).unwrap();
        let mut src = ByteSource::new(&buf);
        let got = read_value(&mut src, v.type_tag()).unwrap();
        assert_eq!(got, v);
        assert!(src.is_empty(), "value left trailing bytes");
    }

    #[test]
    fn values_round_trip() {
        value_round_trip(Value::Bool(true));
        value_round_trip(Value::Bool(false));
        value_round_trip(Value::Int(-9_000_000_000));
        value_round_trip(Value::Int(i64::MIN));
        value_round_trip(Value::Float(3.5));
        value_round_trip(Value::Float(-0.0));
        value_round_trip(Value::Str(String::from("héllo, TSM1")));
        value_round_trip(Value::Str(String::new()));
        value_round_trip(Value::Bytes(vec![0, 1, 2, 254, 255]));
        value_round_trip(Value::Handle(0xDEAD_BEEF_0000_0042));
    }

    #[test]
    fn reserved_value_tags_are_unsupported() {
        let buf = [0u8; 8];
        for tag in [TypeTag::List, TypeTag::Record, TypeTag::Error] {
            let mut src = ByteSource::new(&buf);
            assert_eq!(read_value(&mut src, tag), Err(WireError::Unsupported(tag)));
        }
    }

    #[test]
    fn header_and_schema_round_trip() {
        let schema = Schema::new()
            .field("pid", TypeTag::Int, TypeModifiers::NONE)
            .field("name", TypeTag::String, TypeModifiers::NONE)
            .field("parent", TypeTag::Handle, TypeModifiers::NULLABLE);
        let flags = StreamFlags::TEXT_FALLBACK;

        let mut buf = Vec::new();
        encode_header(&mut buf, flags, &schema).unwrap();

        // Sanity: the stream literally starts with "TSM1".
        assert_eq!(&buf[..4], b"TSM1");

        let mut src = ByteSource::new(&buf);
        let (got_flags, got_schema) = decode_header(&mut src).unwrap();
        assert_eq!(got_flags, flags);
        assert_eq!(got_schema, schema);
        assert!(src.is_empty());
    }

    #[test]
    fn empty_schema_is_valid() {
        let mut buf = Vec::new();
        encode_header(&mut buf, StreamFlags::NONE, &Schema::new()).unwrap();
        let mut src = ByteSource::new(&buf);
        let (_, schema) = decode_header(&mut src).unwrap();
        assert!(schema.fields.is_empty());
    }

    #[test]
    fn bad_magic_is_rejected() {
        let buf = *b"XSM1\x00\x00\x00\x00";
        let mut src = ByteSource::new(&buf);
        assert_eq!(decode_header(&mut src), Err(WireError::BadMagic));
    }

    #[test]
    fn truncated_input_is_eof_not_panic() {
        // A header claiming one field but cut off mid-name.
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        put_u32(&mut buf, 0).unwrap(); // flags
        put_u32(&mut buf, 1).unwrap(); // field_count = 1
        put_u16(&mut buf, 5).unwrap(); // name_len = 5, but no name bytes follow
        let mut src = ByteSource::new(&buf);
        assert_eq!(decode_header(&mut src), Err(WireError::UnexpectedEof));
    }

    #[test]
    fn unknown_type_tag_in_schema_is_rejected() {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        put_u32(&mut buf, 0).unwrap(); // flags
        put_u32(&mut buf, 1).unwrap(); // one field
        put_u16(&mut buf, 1).unwrap();
        buf.push(b'x'); // name "x"
        buf.push(0x7E); // bogus type tag
        buf.push(0); // modifiers
        let mut src = ByteSource::new(&buf);
        assert_eq!(decode_header(&mut src), Err(WireError::BadTypeTag(0x7E)));
    }

    #[test]
    fn terminator_round_trips() {
        let mut buf = Vec::new();
        encode_terminator(&mut buf, -7).unwrap();
        let mut src = ByteSource::new(&buf);
        assert_eq!(src.u8().unwrap(), REC_TERMINATOR);
        assert_eq!(src.i32().unwrap(), -7);
        assert!(src.is_empty());
    }

    #[test]
    fn error_record_round_trips() {
        let rec = WireErrorRecord {
            code: -13,
            message: String::from("type mismatch"),
            field_name: String::from("cpu"),
        };
        let mut buf = Vec::new();
        rec.encode(&mut buf).unwrap();
        let mut src = ByteSource::new(&buf);
        assert_eq!(src.u8().unwrap(), REC_ERROR); // record loop reads the tag...
        assert_eq!(WireErrorRecord::decode_body(&mut src).unwrap(), rec); // ...then the body
        assert!(src.is_empty());
    }
}
