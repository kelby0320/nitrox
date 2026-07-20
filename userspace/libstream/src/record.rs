//! [`TypedRecord`] — mapping a Rust struct to a typed stream's schema + rows, so
//! application code writes/reads structs directly instead of assembling `Value`s.
//!
//! In v1 the trait is **implemented by hand** (see the tests for the shape); the
//! `#[derive(TypedRecord)]` procedural macro that generates these impls is a deferred
//! follow-on. Either way it lowers to the dynamic [`Value`]/[`TableWriter`] API — the
//! wire framing lives in one place.

use alloc::vec::Vec;

use crate::table::{Item, TableReader, TableWriter};
use crate::wire::{ByteSink, Result, Schema, StreamFlags, Value, WireError};

/// A Rust type that maps to a typed stream: it declares a [`Schema`] and converts
/// to/from a row of [`Value`]s (in schema order).
///
/// Manual impls (and the eventual derive) provide three things:
/// - [`schema`](TypedRecord::schema) — the columns (names, types, modifiers);
/// - [`to_values`](TypedRecord::to_values) — this record's field values, in order;
/// - [`from_values`](TypedRecord::from_values) — rebuild from decoded values, erroring
///   on a count/type mismatch (a stream whose schema differs from `Self`'s).
pub trait TypedRecord: Sized {
    /// The schema for this record type.
    fn schema() -> Schema;

    /// This record's field values, in schema (column) order.
    fn to_values(&self) -> Vec<Value>;

    /// Reconstruct from a decoded row's values (schema order).
    fn from_values(values: &[Value]) -> Result<Self>;
}

impl<W: ByteSink> TableWriter<W> {
    /// Write the header using `T`'s schema (no flags). Shorthand for
    /// `write_schema(StreamFlags::NONE, &T::schema())`.
    pub fn write_schema_for<T: TypedRecord>(&mut self) -> Result<()> {
        self.write_schema(StreamFlags::NONE, &T::schema())
    }

    /// Write one typed record as a data row (lowers to [`write_row`](TableWriter::write_row)).
    pub fn write_record<T: TypedRecord>(&mut self, rec: &T) -> Result<()> {
        self.write_row(&rec.to_values())
    }
}

impl<'a> TableReader<'a> {
    /// Read the next **data** record decoded as `T`. `None` at the terminator (end of
    /// rows). An in-stream error record surfaces as [`WireError::StreamError`]; a decode
    /// failure surfaces as its wire error. For streams that interleave error/data records
    /// and need to distinguish them, iterate [`next`](TableReader::next) directly.
    pub fn read_record<T: TypedRecord>(&mut self) -> Option<Result<T>> {
        Some(match self.next()? {
            Ok(Item::Row(values)) => T::from_values(&values),
            Ok(Item::End(_)) => return None,
            Ok(Item::Error(e)) => Err(WireError::StreamError(e.code)),
            Err(e) => Err(e),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Schema, TypeModifiers, TypeTag, Value};
    use alloc::string::String;
    use alloc::vec;

    /// A hand-written `TypedRecord` — the pattern the derive macro will generate. Has a
    /// nullable column (`note: Option<String>`) to exercise the presence-byte path.
    #[derive(Clone, PartialEq, Debug)]
    struct Beat {
        seq: u64,
        source: String,
        healthy: bool,
        note: Option<String>,
    }

    impl TypedRecord for Beat {
        fn schema() -> Schema {
            Schema::new()
                .field("seq", TypeTag::Int, TypeModifiers::NONE)
                .field("source", TypeTag::String, TypeModifiers::NONE)
                .field("healthy", TypeTag::Bool, TypeModifiers::NONE)
                .field("note", TypeTag::String, TypeModifiers::NULLABLE)
        }

        fn to_values(&self) -> Vec<Value> {
            vec![
                Value::Int(self.seq as i64),
                Value::Str(self.source.clone()),
                Value::Bool(self.healthy),
                match &self.note {
                    Some(s) => Value::Str(s.clone()),
                    None => Value::Null,
                },
            ]
        }

        fn from_values(v: &[Value]) -> Result<Self> {
            if v.len() != 4 {
                return Err(WireError::SchemaMismatch);
            }
            Ok(Beat {
                seq: v[0].as_int().ok_or(WireError::SchemaMismatch)? as u64,
                source: String::from(v[1].as_str().ok_or(WireError::SchemaMismatch)?),
                healthy: v[2].as_bool().ok_or(WireError::SchemaMismatch)?,
                note: if v[3].is_null() {
                    None
                } else {
                    Some(String::from(v[3].as_str().ok_or(WireError::SchemaMismatch)?))
                },
            })
        }
    }

    #[test]
    fn typed_write_read_round_trip() {
        let beats = [
            Beat {
                seq: 1,
                source: String::from("worker"),
                healthy: true,
                note: Some(String::from("ok")),
            },
            Beat {
                seq: 2,
                source: String::from("worker"),
                healthy: false,
                note: None,
            },
        ];

        let mut buf = Vec::new();
        {
            let mut tw = TableWriter::new(&mut buf);
            tw.write_schema_for::<Beat>().unwrap();
            for b in &beats {
                tw.write_record(b).unwrap();
            }
            tw.finish_with_status(0).unwrap();
        }

        let mut tr = TableReader::new(&buf).unwrap();
        assert_eq!(tr.schema(), &Beat::schema());
        assert_eq!(tr.read_record::<Beat>().unwrap().unwrap(), beats[0]);
        assert_eq!(tr.read_record::<Beat>().unwrap().unwrap(), beats[1]);
        assert!(tr.read_record::<Beat>().is_none()); // terminator → end of rows
    }

    #[test]
    fn from_values_rejects_mismatch() {
        // A `Str` where `seq` (Int) is expected.
        let bad = [
            Value::Str(String::from("x")),
            Value::Str(String::from("s")),
            Value::Bool(true),
            Value::Null,
        ];
        assert_eq!(Beat::from_values(&bad), Err(WireError::SchemaMismatch));
        // Wrong field count.
        assert_eq!(Beat::from_values(&[Value::Int(1)]), Err(WireError::SchemaMismatch));
    }
}
