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
//! **Scalars + collections.** [`Value`] and [`read_value`]/[`write_value`] implement
//! the scalar + `String`/`Bytes`/`Handle` types plus the two nested-cell collections:
//! `List` ([`Value::List`], an `Arc<[Value]>`) and `Record` ([`Value::Record`], a
//! sub-schema + values). A whole stream is [`Value::Table`] — the in-memory form of a
//! header + rows + terminator, serialised via [`Table::encode`]/[`Table::decode`], not
//! as a nested cell (a table is a stream, not a value tag). The collection variants are
//! persistent: each is `Arc`-shared, so cloning a [`Value`] is a refcount bump, and the
//! shell's copy-on-write "mutation" is a cheap rebind (see the shell design doc §9d).
//! Only the value-level `Error` tag remains reserved (returns [`WireError::Unsupported`]);
//! `Null`, nullable fields, error records, and the terminator are all implemented.

use alloc::string::String;
use alloc::sync::Arc;
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
    /// `u32` count, then each element as a `u8` tag + that value's bytes
    /// (self-describing, so a list may be heterogeneous). Decodes to [`Value::List`].
    List = 0x07,
    /// A nested sub-schema (as [`Schema::encode`]) + one row of values. Decodes to
    /// [`Value::Record`].
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
    /// A recognised but not-yet-implemented type (the value-level `Error` tag).
    Unsupported(TypeTag),
    /// A [`Value::Table`] was used where a nested **cell** value is required (a row
    /// field, list element, or record field). A table is a *stream*, not a cell: it has
    /// no [`TypeTag`] and serialises via [`Table::encode`], never [`write_value`].
    NestedTable,
    /// The sink refused the write (e.g. a fixed frame is full).
    SinkFull,
    /// A record's values don't match the schema: wrong field count, a `Null` in a
    /// non-nullable field, a value whose type differs from its column, or a write in
    /// the wrong order (a row before the schema / after the terminator).
    SchemaMismatch,
    /// A typed read (`read_record`) hit an in-stream error record; carries its code.
    StreamError(i32),
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

/// Forwarding impl so a `&mut S` can be handed to something that takes a `ByteSink`
/// by value (e.g. `TableWriter::new(&mut sink)`), keeping ownership with the caller.
impl<S: ByteSink + ?Sized> ByteSink for &mut S {
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        (**self).put(bytes)
    }
}

/// A [`ByteSink`] over a caller-owned fixed buffer — the practical transport primitive:
/// a program encodes a stream straight into an `IpcMsg` body, then `sys_channel_send`s
/// [`as_bytes`](SliceSink::as_bytes). Overflowing the buffer fails with
/// [`WireError::SinkFull`] (backpressure) rather than truncating.
pub struct SliceSink<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> SliceSink<'a> {
    /// Wrap a fixed buffer; writing starts at offset 0.
    pub fn new(buf: &'a mut [u8]) -> Self {
        SliceSink { buf, len: 0 }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` if nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The written prefix — what to send.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl ByteSink for SliceSink<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self.len + bytes.len();
        if end > self.buf.len() {
            return Err(WireError::SinkFull);
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
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
    /// A list of values ([`TypeTag::List`]). Persistent: `Arc`-shared, so cloning is a
    /// refcount bump. May be heterogeneous (each element is self-describing on the wire).
    List(Arc<[Value]>),
    /// A record — named, typed fields with a value each ([`TypeTag::Record`]). Persistent
    /// (`Arc`-shared). See [`Record`].
    Record(Arc<Record>),
    /// A whole table/stream ([`Table`]). Persistent (`Arc`-shared). Not a nested cell —
    /// it has no [`TypeTag`] and serialises via [`Table::encode`], so [`type_tag`](Self::type_tag)
    /// reports `None` for it and [`write_value`] refuses it ([`WireError::NestedTable`]).
    Table(Arc<Table>),
}

/// A **record value**: named, typed fields (a sub-schema) with one value each — the
/// in-memory form of the [`TypeTag::Record`] nested-cell encoding. Persistent: shared
/// via [`Value::Record`]'s `Arc`, never mutated in place (the shell rebinds copies).
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Record {
    /// The fields' names, types, and modifiers, in wire order.
    pub schema: Schema,
    /// One value per [`schema`](Self::schema) field, positionally aligned.
    pub values: Vec<Value>,
}

/// A **table value**: a schema and its rows — the in-memory form of a whole TSM1 stream
/// (header + data records + terminator). Persistent: shared via [`Value::Table`]'s `Arc`.
/// Unlike [`Value::List`]/[`Value::Record`] a table is a *stream*, not a nested cell: it
/// has no [`TypeTag`] and round-trips through [`Table::encode`]/[`Table::decode`].
#[derive(Clone, PartialEq, Debug, Default)]
pub struct Table {
    /// Stream-level flags (from the header).
    pub flags: StreamFlags,
    /// The column definitions.
    pub schema: Schema,
    /// The rows; each has one value per [`schema`](Self::schema) field.
    pub rows: Vec<Vec<Value>>,
}

impl Value {
    /// The [`TypeTag`] this value encodes as, or `None` for [`Value::Table`] (a stream,
    /// not a cell — it has no tag and cannot appear as a field/element value). `Null`
    /// reports [`TypeTag::Null`]; a nullable field carries its declared tag regardless.
    pub fn type_tag(&self) -> Option<TypeTag> {
        Some(match self {
            Value::Null => TypeTag::Null,
            Value::Bool(_) => TypeTag::Bool,
            Value::Int(_) => TypeTag::Int,
            Value::Float(_) => TypeTag::Float,
            Value::Str(_) => TypeTag::String,
            Value::Bytes(_) => TypeTag::Bytes,
            Value::Handle(_) => TypeTag::Handle,
            Value::List(_) => TypeTag::List,
            Value::Record(_) => TypeTag::Record,
            Value::Table(_) => return None,
        })
    }

    /// `true` for [`Value::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// The boolean, if this is a [`Value::Bool`] — for ergonomic `from_values` impls.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }
    /// The integer, if this is a [`Value::Int`].
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    /// The float, if this is a [`Value::Float`].
    pub fn as_float(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            _ => None,
        }
    }
    /// The string, if this is a [`Value::Str`].
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
    /// The bytes, if this is a [`Value::Bytes`].
    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }
    /// The handle value, if this is a [`Value::Handle`].
    pub fn as_handle(&self) -> Option<u64> {
        match self {
            Value::Handle(h) => Some(*h),
            _ => None,
        }
    }
    /// The elements, if this is a [`Value::List`].
    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }
    /// The record, if this is a [`Value::Record`].
    pub fn as_record(&self) -> Option<&Record> {
        match self {
            Value::Record(r) => Some(r),
            _ => None,
        }
    }
    /// The table, if this is a [`Value::Table`].
    pub fn as_table(&self) -> Option<&Table> {
        match self {
            Value::Table(t) => Some(t),
            _ => None,
        }
    }
}

/// Encode a value's bytes (no tag byte — the tag lives in the schema, and any
/// `NULLABLE` presence byte is written by the record layer). A [`Value::List`] /
/// [`Value::Record`] carries its own inner types self-describingly (see [`TypeTag`]);
/// a [`Value::Table`] is a stream, not a cell, so it is refused with
/// [`WireError::NestedTable`] — serialise it via [`Table::encode`] instead.
pub fn write_value(sink: &mut impl ByteSink, v: &Value) -> Result<()> {
    match v {
        Value::Null => Ok(()),
        Value::Bool(b) => put_u8(sink, *b as u8),
        Value::Int(i) => sink.put(&i.to_le_bytes()),
        Value::Float(f) => sink.put(&f.to_le_bytes()),
        Value::Str(s) => put_lenpfx(sink, s.as_bytes()),
        Value::Bytes(b) => put_lenpfx(sink, b),
        Value::Handle(h) => sink.put(&h.to_le_bytes()),
        Value::List(items) => write_list(sink, items),
        Value::Record(r) => {
            r.schema.encode(sink)?;
            write_row_values(sink, &r.schema, &r.values)
        }
        Value::Table(_) => Err(WireError::NestedTable),
    }
}

/// Encode a list body: `u32` count, then each element as a `u8` [`TypeTag`] + that
/// value's bytes (self-describing, so heterogeneous lists round-trip). A [`Value::Table`]
/// element is refused ([`WireError::NestedTable`]) — tables are streams, not cells.
fn write_list(sink: &mut impl ByteSink, items: &[Value]) -> Result<()> {
    put_u32(sink, items.len() as u32)?;
    for e in items {
        let tag = e.type_tag().ok_or(WireError::NestedTable)?;
        put_u8(sink, tag.to_u8())?;
        write_value(sink, e)?;
    }
    Ok(())
}

/// Decode a value of the given `tag`. Does not consume a `NULLABLE` presence byte —
/// that is the record layer's job (it reads presence, then calls this if present).
/// Handles `List`/`Record` (as [`Value::List`]/[`Value::Record`]); the value-level
/// `Error` tag is still [`WireError::Unsupported`].
pub fn read_value(src: &mut ByteSource, tag: TypeTag) -> Result<Value> {
    Ok(match tag {
        TypeTag::Null => Value::Null,
        TypeTag::Bool => Value::Bool(src.u8()? != 0),
        TypeTag::Int => Value::Int(src.i64()?),
        TypeTag::Float => Value::Float(src.f64()?),
        TypeTag::String => Value::Str(src.string()?),
        TypeTag::Bytes => Value::Bytes(src.bytes()?),
        TypeTag::Handle => Value::Handle(src.u64()?),
        TypeTag::List => read_list(src)?,
        TypeTag::Record => {
            let schema = Schema::decode(src)?;
            let values = read_row_values(src, &schema)?;
            Value::Record(Arc::new(Record { schema, values }))
        }
        TypeTag::Error => return Err(WireError::Unsupported(tag)),
    })
}

/// Decode a list body written by [`write_list`].
fn read_list(src: &mut ByteSource) -> Result<Value> {
    let n = src.u32()? as usize;
    // Cap the pre-allocation so a bogus huge count can't OOM us; each element read is
    // still bounds-checked, so an over-large count simply errors out on EOF.
    let mut items = Vec::with_capacity(n.min(SCHEMA_FIELD_HINT_CAP));
    for _ in 0..n {
        let tb = src.u8()?;
        let tag = TypeTag::from_u8(tb).ok_or(WireError::BadTypeTag(tb))?;
        items.push(read_value(src, tag)?);
    }
    Ok(Value::List(Arc::from(items)))
}

/// Encode a row of `values` against `schema`: for each field, a `NULLABLE` presence byte
/// where the column declares it, then the value's bytes. Errors on a count / type /
/// nullness mismatch. Shared by data rows ([`TableWriter`](crate::table::TableWriter)),
/// [`Value::Record`], and [`Table`] rows so all three frame values identically.
pub fn write_row_values(
    sink: &mut impl ByteSink,
    schema: &Schema,
    values: &[Value],
) -> Result<()> {
    if values.len() != schema.fields.len() {
        return Err(WireError::SchemaMismatch);
    }
    for (f, value) in schema.fields.iter().zip(values) {
        let nullable = f.modifiers.contains(TypeModifiers::NULLABLE);
        let is_null = matches!(value, Value::Null);
        if nullable {
            put_u8(sink, (!is_null) as u8)?;
            if is_null {
                continue;
            }
        } else if is_null {
            return Err(WireError::SchemaMismatch);
        }
        // A present value's type must match its column (`Null` columns take `Null`).
        if value.type_tag() != Some(f.ty) {
            return Err(WireError::SchemaMismatch);
        }
        write_value(sink, value)?;
    }
    Ok(())
}

/// Decode a row of values against `schema` (inverse of [`write_row_values`]): reads a
/// `NULLABLE` presence byte where declared, then the value. Shared by the same three
/// call sites as [`write_row_values`].
pub fn read_row_values(src: &mut ByteSource, schema: &Schema) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(schema.fields.len().min(SCHEMA_FIELD_HINT_CAP));
    for f in &schema.fields {
        let nullable = f.modifiers.contains(TypeModifiers::NULLABLE);
        let value = if nullable && src.u8()? == 0 {
            Value::Null
        } else {
            read_value(src, f.ty)?
        };
        out.push(value);
    }
    Ok(out)
}

impl Table {
    /// Serialise as a complete TSM1 stream: header (`flags` + `schema`), one `REC_DATA`
    /// record per row, then the terminator (exit status `0` — a table *value* carries no
    /// pipeline status; that rides separately, see the shell design doc §9f).
    pub fn encode(&self, sink: &mut impl ByteSink) -> Result<()> {
        encode_header(sink, self.flags, &self.schema)?;
        for row in &self.rows {
            put_u8(sink, REC_DATA)?;
            write_row_values(sink, &self.schema, row)?;
        }
        encode_terminator(sink, 0)
    }

    /// Parse a complete TSM1 stream into a table (inverse of [`encode`](Self::encode)).
    /// Stops at the terminator (its exit status is discarded); an embedded error record
    /// or an unknown record tag is a decode error — a table value is pure rows.
    pub fn decode(buf: &[u8]) -> Result<Table> {
        let mut src = ByteSource::new(buf);
        let (flags, schema) = decode_header(&mut src)?;
        let mut rows = Vec::new();
        loop {
            match src.u8()? {
                REC_DATA => rows.push(read_row_values(&mut src, &schema)?),
                REC_TERMINATOR => {
                    let _exit_status = src.i32()?;
                    break;
                }
                other => return Err(WireError::BadRecordTag(other)),
            }
        }
        Ok(Table {
            flags,
            schema,
            rows,
        })
    }
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
        let tag = v.type_tag().expect("cell value has a tag");
        let mut buf = Vec::new();
        write_value(&mut buf, &v).unwrap();
        let mut src = ByteSource::new(&buf);
        let got = read_value(&mut src, tag).unwrap();
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
    fn list_values_round_trip() {
        // Empty, homogeneous, heterogeneous, and nested lists.
        value_round_trip(Value::List(Arc::from(&[][..])));
        value_round_trip(Value::List(Arc::from(
            &[Value::Int(1), Value::Int(2), Value::Int(3)][..],
        )));
        value_round_trip(Value::List(Arc::from(
            &[Value::Int(7), Value::Str(String::from("mix")), Value::Bool(true), Value::Null][..],
        )));
        value_round_trip(Value::List(Arc::from(
            &[Value::List(Arc::from(&[Value::Int(1)][..])), Value::List(Arc::from(&[][..]))][..],
        )));
    }

    #[test]
    fn record_values_round_trip() {
        let schema = Schema::new()
            .field("name", TypeTag::String, TypeModifiers::NONE)
            .field("size", TypeTag::Int, TypeModifiers::NONE)
            .field("note", TypeTag::String, TypeModifiers::NULLABLE);
        // Present nullable field.
        value_round_trip(Value::Record(Arc::new(Record {
            schema: schema.clone(),
            values: vec![Value::Str(String::from("a")), Value::Int(10), Value::Str(String::from("ok"))],
        })));
        // Absent nullable field (presence byte = 0).
        value_round_trip(Value::Record(Arc::new(Record {
            schema,
            values: vec![Value::Str(String::from("b")), Value::Int(20), Value::Null],
        })));
        // A record whose field is itself a list (nested collection cell).
        let nested = Schema::new().field("tags", TypeTag::List, TypeModifiers::NONE);
        value_round_trip(Value::Record(Arc::new(Record {
            schema: nested,
            values: vec![Value::List(Arc::from(
                &[Value::Str(String::from("x")), Value::Str(String::from("y"))][..],
            ))],
        })));
    }

    #[test]
    fn table_value_round_trips_as_a_stream() {
        let schema = Schema::new()
            .field("pid", TypeTag::Int, TypeModifiers::NONE)
            .field("name", TypeTag::String, TypeModifiers::NONE)
            .field("parent", TypeTag::Handle, TypeModifiers::NULLABLE);
        let table = Table {
            flags: StreamFlags::NONE,
            schema,
            rows: vec![
                vec![Value::Int(1), Value::Str(String::from("init")), Value::Null],
                vec![Value::Int(2), Value::Str(String::from("fs")), Value::Handle(0x40)],
            ],
        };
        let mut buf = Vec::new();
        table.encode(&mut buf).unwrap();
        assert_eq!(&buf[..4], b"TSM1"); // a table serialises as a whole stream
        assert_eq!(Table::decode(&buf).unwrap(), table);
    }

    #[test]
    fn table_is_not_a_cell_value() {
        // A table has no cell tag and cannot be written as a nested value…
        let table = Value::Table(Arc::new(Table::default()));
        assert_eq!(table.type_tag(), None);
        let mut buf = Vec::new();
        assert_eq!(write_value(&mut buf, &table), Err(WireError::NestedTable));
        // …including as a list element.
        let list = Value::List(Arc::from(&[table][..]));
        assert_eq!(write_value(&mut Vec::new(), &list), Err(WireError::NestedTable));
    }

    #[test]
    fn error_value_tag_is_unsupported() {
        let buf = [0u8; 8];
        let mut src = ByteSource::new(&buf);
        assert_eq!(
            read_value(&mut src, TypeTag::Error),
            Err(WireError::Unsupported(TypeTag::Error))
        );
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
