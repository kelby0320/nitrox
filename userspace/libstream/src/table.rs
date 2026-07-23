//! Record-level typed streams: [`TableWriter`] (schema → rows → terminator) and
//! [`TableReader`] (iterate the records back), composed over the [`wire`](crate::wire)
//! codec. This is the surface application code uses; the wire bytes stay hidden.
//!
//! **v1 works with dynamic [`Value`] rows.** `#[derive(TypedRecord)]` — writing/reading
//! typed structs directly — layers on top in a later part; it lowers to these same
//! calls.

use alloc::string::String;
use alloc::vec::Vec;

use crate::wire::{
    ByteSink, ByteSource, REC_DATA, REC_ERROR, REC_TERMINATOR, Result, Schema, StreamFlags,
    TypeModifiers, TypeTag, Value, WireError, WireErrorRecord, decode_header, encode_header,
    encode_terminator, put_u8, read_row_values, write_row_values,
};

/// Writes a typed stream to a [`ByteSink`]: one header (magic + flags + schema), then a
/// run of data/error records, then a terminator. Enforces that rows match the schema.
///
/// ```
/// use libstream::{Schema, StreamFlags, TableWriter, TypeModifiers, TypeTag, Value};
/// # fn demo() -> Result<(), libstream::WireError> {
/// let schema = Schema::new()
///     .field("pid", TypeTag::Int, TypeModifiers::NONE)
///     .field("name", TypeTag::String, TypeModifiers::NONE);
/// let mut buf = Vec::new();
/// let mut tw = TableWriter::new(&mut buf);
/// tw.write_schema(StreamFlags::NONE, &schema)?;
/// tw.write_row(&[Value::Int(7), Value::Str("init".into())])?;
/// tw.finish_with_status(0)?;
/// # Ok(())
/// # }
/// ```
pub struct TableWriter<W: ByteSink> {
    sink: W,
    schema: Schema,
    begun: bool,
    finished: bool,
}

impl<W: ByteSink> TableWriter<W> {
    /// Wrap a sink. Nothing is written until [`write_schema`](Self::write_schema).
    pub fn new(sink: W) -> Self {
        TableWriter {
            sink,
            schema: Schema::new(),
            begun: false,
            finished: false,
        }
    }

    /// Write the header (magic + `flags` + `schema`). Must be called exactly once,
    /// before any row. The schema is retained to validate + frame subsequent rows.
    pub fn write_schema(&mut self, flags: StreamFlags, schema: &Schema) -> Result<()> {
        if self.begun {
            return Err(WireError::SchemaMismatch);
        }
        encode_header(&mut self.sink, flags, schema)?;
        self.schema = schema.clone();
        self.begun = true;
        Ok(())
    }

    /// Write one data record. `values` must have exactly one entry per column, each
    /// matching the column's type (a `Null` only where the column is `NULLABLE`).
    pub fn write_row(&mut self, values: &[Value]) -> Result<()> {
        if !self.begun || self.finished {
            return Err(WireError::SchemaMismatch);
        }
        put_u8(&mut self.sink, REC_DATA)?;
        // The value framing (count/type checks + `NULLABLE` presence bytes) is shared
        // with `Value::Record`/`Table` rows — see `wire::write_row_values`.
        write_row_values(&mut self.sink, &self.schema, values)
    }

    /// Write a structured error record into the stream.
    pub fn write_error(&mut self, rec: &WireErrorRecord) -> Result<()> {
        if !self.begun || self.finished {
            return Err(WireError::SchemaMismatch);
        }
        rec.encode(&mut self.sink)
    }

    /// Write the terminator with the producer's `exit_status`, closing the stream.
    pub fn finish_with_status(&mut self, exit_status: i32) -> Result<()> {
        if !self.begun || self.finished {
            return Err(WireError::SchemaMismatch);
        }
        encode_terminator(&mut self.sink, exit_status)?;
        self.finished = true;
        Ok(())
    }

    /// Recover the underlying sink (e.g. a `SliceSink` to read `as_bytes`).
    pub fn into_sink(self) -> W {
        self.sink
    }
}

/// One item yielded by [`TableReader::next`].
#[derive(Clone, PartialEq, Debug)]
pub enum Item {
    /// A data record: one value per schema column (`Null` for an absent nullable field).
    Row(Vec<Value>),
    /// A structured error embedded in the stream.
    Error(WireErrorRecord),
    /// The terminator, carrying the producer's exit status. No items follow.
    End(i32),
}

/// Reads a typed stream from a complete in-memory buffer: the header up front (its
/// schema + flags exposed), then [`next`](Self::next) iterates the records until the
/// terminator. (Incremental reading of a still-arriving stream is a later refinement;
/// v1 receivers reassemble a whole stream first.)
pub struct TableReader<'a> {
    src: ByteSource<'a>,
    schema: Schema,
    flags: StreamFlags,
    ended: bool,
}

impl<'a> TableReader<'a> {
    /// Parse the header of `buf`, or fail (bad magic / truncation / unknown type tag).
    pub fn new(buf: &'a [u8]) -> Result<Self> {
        let mut src = ByteSource::new(buf);
        let (flags, schema) = decode_header(&mut src)?;
        Ok(TableReader {
            src,
            schema,
            flags,
            ended: false,
        })
    }

    /// The stream's flags (e.g. [`StreamFlags::TEXT_FALLBACK`]).
    pub fn flags(&self) -> StreamFlags {
        self.flags
    }

    /// The stream's schema.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// Read the next record. `None` once the terminator has been returned (or on a
    /// prior error). A decode failure is reported once, then the reader is spent.
    pub fn next(&mut self) -> Option<Result<Item>> {
        if self.ended {
            return None;
        }
        let tag = match self.src.u8() {
            Ok(t) => t,
            Err(e) => {
                self.ended = true;
                return Some(Err(e));
            }
        };
        let item = match tag {
            REC_DATA => self.read_row().map(Item::Row),
            REC_ERROR => WireErrorRecord::decode_body(&mut self.src).map(Item::Error),
            REC_TERMINATOR => {
                self.ended = true;
                return Some(self.src.i32().map(Item::End));
            }
            other => Err(WireError::BadRecordTag(other)),
        };
        if item.is_err() {
            self.ended = true;
        }
        Some(item)
    }

    fn read_row(&mut self) -> Result<Vec<Value>> {
        // Shares its framing with `Value::Record`/`Table` rows — see `wire::read_row_values`.
        read_row_values(&mut self.src, &self.schema)
    }
}

/// Write a **text-fallback** stream: [`StreamFlags::TEXT_FALLBACK`], a one-column schema
/// `{ line: String }`, one row per line, then the terminator. This is how a plain-text
/// producer's output is wrapped so it flows through the same typed pipeline (the "Unix
/// floor" — every generic operator still works on it).
pub fn write_text_fallback(
    sink: &mut impl ByteSink,
    lines: &[&str],
    exit_status: i32,
) -> Result<()> {
    let schema = Schema::new().field("line", TypeTag::String, TypeModifiers::NONE);
    let mut tw = TableWriter::new(sink);
    tw.write_schema(StreamFlags::TEXT_FALLBACK, &schema)?;
    for line in lines {
        tw.write_row(&[Value::Str(String::from(*line))])?;
    }
    tw.finish_with_status(exit_status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::SliceSink;
    use alloc::vec;

    fn demo_schema() -> Schema {
        Schema::new()
            .field("pid", TypeTag::Int, TypeModifiers::NONE)
            .field("name", TypeTag::String, TypeModifiers::NONE)
            .field("healthy", TypeTag::Bool, TypeModifiers::NONE)
            .field("parent", TypeTag::Handle, TypeModifiers::NULLABLE)
    }

    #[test]
    fn writer_reader_round_trip_with_nullable() {
        let schema = demo_schema();
        let rows = [
            vec![
                Value::Int(1),
                Value::Str(String::from("init")),
                Value::Bool(true),
                Value::Handle(0x40),
            ],
            vec![
                Value::Int(2),
                Value::Str(String::from("svc-mgr")),
                Value::Bool(false),
                Value::Null, // absent nullable handle
            ],
        ];

        let mut buf = Vec::new();
        {
            let mut tw = TableWriter::new(&mut buf);
            tw.write_schema(StreamFlags::NONE, &schema).unwrap();
            for r in &rows {
                tw.write_row(r).unwrap();
            }
            tw.finish_with_status(0).unwrap();
        }

        let mut tr = TableReader::new(&buf).unwrap();
        assert_eq!(tr.schema(), &schema);
        assert_eq!(tr.next().unwrap().unwrap(), Item::Row(rows[0].clone()));
        assert_eq!(tr.next().unwrap().unwrap(), Item::Row(rows[1].clone()));
        assert_eq!(tr.next().unwrap().unwrap(), Item::End(0));
        assert!(tr.next().is_none());
    }

    #[test]
    fn error_record_in_stream() {
        let schema = Schema::new().field("x", TypeTag::Int, TypeModifiers::NONE);
        let err = WireErrorRecord {
            code: -13,
            message: String::from("bad row"),
            field_name: String::from("x"),
        };
        let mut buf = Vec::new();
        {
            let mut tw = TableWriter::new(&mut buf);
            tw.write_schema(StreamFlags::NONE, &schema).unwrap();
            tw.write_row(&[Value::Int(1)]).unwrap();
            tw.write_error(&err).unwrap();
            tw.write_row(&[Value::Int(2)]).unwrap();
            tw.finish_with_status(0).unwrap();
        }
        let mut tr = TableReader::new(&buf).unwrap();
        assert_eq!(tr.next().unwrap().unwrap(), Item::Row(vec![Value::Int(1)]));
        assert_eq!(tr.next().unwrap().unwrap(), Item::Error(err));
        assert_eq!(tr.next().unwrap().unwrap(), Item::Row(vec![Value::Int(2)]));
        assert_eq!(tr.next().unwrap().unwrap(), Item::End(0));
    }

    #[test]
    fn schema_mismatch_is_caught() {
        let schema = Schema::new()
            .field("a", TypeTag::Int, TypeModifiers::NONE)
            .field("b", TypeTag::String, TypeModifiers::NONE);
        let mut buf = Vec::new();
        let mut tw = TableWriter::new(&mut buf);
        // Row before schema.
        assert_eq!(tw.write_row(&[Value::Int(1)]), Err(WireError::SchemaMismatch));
        tw.write_schema(StreamFlags::NONE, &schema).unwrap();
        // Wrong field count.
        assert_eq!(tw.write_row(&[Value::Int(1)]), Err(WireError::SchemaMismatch));
        // Wrong type in column b (Int where String expected).
        assert_eq!(
            tw.write_row(&[Value::Int(1), Value::Int(2)]),
            Err(WireError::SchemaMismatch)
        );
        // Null in a non-nullable column.
        assert_eq!(
            tw.write_row(&[Value::Null, Value::Str(String::from("x"))]),
            Err(WireError::SchemaMismatch)
        );
    }

    #[test]
    fn slice_sink_fills_and_overflows() {
        let schema = Schema::new().field("x", TypeTag::Int, TypeModifiers::NONE);

        // Roomy buffer: encode fully, then read back from `as_bytes`.
        let mut store = [0u8; 128];
        let bytes_len;
        {
            let mut tw = TableWriter::new(SliceSink::new(&mut store));
            tw.write_schema(StreamFlags::NONE, &schema).unwrap();
            tw.write_row(&[Value::Int(0x1122_3344)]).unwrap();
            tw.finish_with_status(0).unwrap();
            bytes_len = tw.into_sink().len();
        }
        let mut tr = TableReader::new(&store[..bytes_len]).unwrap();
        assert_eq!(
            tr.next().unwrap().unwrap(),
            Item::Row(vec![Value::Int(0x1122_3344)])
        );
        assert_eq!(tr.next().unwrap().unwrap(), Item::End(0));

        // Tiny buffer: the header alone overflows → SinkFull (backpressure, no truncation).
        let mut tiny = [0u8; 4];
        let mut tw = TableWriter::new(SliceSink::new(&mut tiny));
        assert_eq!(
            tw.write_schema(StreamFlags::NONE, &schema),
            Err(WireError::SinkFull)
        );
    }

    #[test]
    fn text_fallback_round_trips() {
        let mut buf = Vec::new();
        write_text_fallback(&mut buf, &["hello", "world"], 0).unwrap();

        let mut tr = TableReader::new(&buf).unwrap();
        assert!(tr.flags().contains(StreamFlags::TEXT_FALLBACK));
        assert_eq!(tr.schema().fields.len(), 1);
        assert_eq!(tr.schema().fields[0].name, "line");
        assert_eq!(
            tr.next().unwrap().unwrap(),
            Item::Row(vec![Value::Str(String::from("hello"))])
        );
        assert_eq!(
            tr.next().unwrap().unwrap(),
            Item::Row(vec![Value::Str(String::from("world"))])
        );
        assert_eq!(tr.next().unwrap().unwrap(), Item::End(0));
    }

    #[test]
    fn truncated_body_reports_error_once() {
        // Valid header + a data-record tag but no field bytes.
        let schema = Schema::new().field("x", TypeTag::Int, TypeModifiers::NONE);
        let mut buf = Vec::new();
        encode_header(&mut buf, StreamFlags::NONE, &schema).unwrap();
        put_u8(&mut buf, REC_DATA).unwrap();
        let mut tr = TableReader::new(&buf).unwrap();
        assert_eq!(tr.next(), Some(Err(WireError::UnexpectedEof)));
        assert!(tr.next().is_none()); // spent after the error
    }
}
