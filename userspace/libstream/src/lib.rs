//! `libstream` — typed structured streams for Nitrox userspace.
//!
//! Programs communicate over pipelines (and log/IPC channels) as **typed tables of
//! records** rather than byte streams: a schema (named, typed columns) followed by a
//! run of data records (rows), all in the `TSM1` wire format. Plain-text output is
//! wrapped as a single-column `Table<String>`.
//!
//! See [`docs/spec/typed-stream-format.md`](../../../docs/spec/typed-stream-format.md)
//! for the format.
//!
//! ## Layering
//!
//! The **wire core** ([`wire`]) is `core + alloc` only: the byte-level codec over the
//! transport-agnostic [`wire::ByteSink`] / [`wire::ByteSource`] seams. `TableWriter`/
//! `TableReader` layer on top; the channel adapter, the only part that touches a syscall,
//! is separate again.
//!
//! ## Terminology (three things the spec calls "record")
//!
//! - **`TypedRecord`** — the *Rust trait*: "this struct maps to the wire." At the top
//!   level it defines a stream's columns; nested, it's a field value.
//! - **data record** (wire tag `0x01`) — a *row*: one instance of the schema.
//! - **`Record`** ([`wire::TypeTag::Record`]) — the *nested-struct field encoding*
//!   (a sub-schema + values), used when a `TypedRecord` appears as a field value.
//!
//! A whole table is the stream itself ([`wire::Value::Table`], serialised via
//! [`wire::Table`], not a cell tag). Only the value-level `Error` tag is reserved.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod channel;
pub mod diag;
pub mod record;
pub mod setup;
pub mod table;
pub mod wire;

pub use record::TypedRecord;
pub use table::{Item, TableReader, TableWriter, write_text_fallback};
pub use wire::{
    ByteSink, ByteSource, FieldDef, Record, Schema, SliceSink, StreamFlags, Table, TypeModifiers, TypeTag, Value,
    WireError, WireErrorRecord,
};
