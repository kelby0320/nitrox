# Typed Stream Format (TSM1) Specification

This document specifies the wire format of typed structured streams — the format used by programs to communicate via stdin/stdout pipelines. The format is identified by the magic bytes `TSM1` (Typed Stream Magic, version 1).

**Status:** Pre-stabilization. The envelope and structural types are committed; the `TypeTag` byte values are now **pinned** (below), implemented in `userspace/libstream/src/wire.rs` (the canonical source for byte-level details). The scalar types plus `String`/`Bytes`/`Handle` and the **collection cells** `List` (a persistent, `Arc`-backed `Value::List`) and `Record` (nested sub-schema + values, `Value::Record`) are all encoded; a whole table is the stream itself (`Value::Table`, no cell tag). Only the value-level `Error` tag remains reserved-but-unsupported.

### Terminology: three things called "record"

The word "record" appears at three different levels; keep them distinct:

- **`TypedRecord`** — the *Rust trait* (`#[derive(TypedRecord)]`): "this struct maps to the wire." At the top level it defines a stream's columns; nested, it is a field value.
- **data record** (record tag `0x01`) — a *row*: one instance of the schema. `Body := Record*` means "a sequence of rows."
- **`Record`** (the `TypeTag` below) — the *nested-struct field encoding* (a sub-schema + values), used when a `TypedRecord` appears as a field value.

The underlying model separates a **`WireValue`** (anything with a `TypeTag` and an encoding — the scalars, `String`, `Bytes`, `Handle`, `List<V>`, `Record<R>`) from a **`TypedRecord`** (a struct = ordered named `WireValue` fields = a schema; *as a value* it is a `Record`). So `Vec<V: WireValue>` maps to `List` uniformly (both `Vec<i64>` and `Vec<Thread>` work).

## Overall structure

```
Stream     := Header Body
Header     := magic flags Schema
Body       := Record* Terminator
```

Where:
- `magic`: 4 bytes, `0x54534D31` (ASCII `"TSM1"`)
- `flags`: 4 bytes, `StreamFlags` bitfield
- `Schema`: schema definition (see below)
- `Record`: zero or more records (see below)
- `Terminator`: end-of-stream marker with exit status

## Header

```
Offset  Size  Field
─────── ────  ───────────
   0      4   magic       (0x54534D31, "TSM1")
   4      4   flags       (StreamFlags)
   8     ...  schema      (variable length)
```

### StreamFlags

```rust
bitflags! {
    pub struct StreamFlags: u32 {
        const COMPRESSED = 1 << 0;  // body is compressed (deferred)
        const TEXT_FALLBACK = 1 << 1;  // this is text wrapped as Table<String> with column "line"
        // bits 2..31 reserved
    }
}
```

`TEXT_FALLBACK` is set by the runtime when wrapping a text-emitting program's output. Tools that consume streams may render `TEXT_FALLBACK` streams differently (skip table chrome, render lines directly).

`COMPRESSED` is reserved; not honored in initial implementation.

## Schema

A schema describes the shape of records in the stream. Format:

```
Schema     := field_count Field*
field_count: u32
Field      := name_len name type modifiers
name_len   : u16
name       : <name_len> bytes of UTF-8 (no null terminator)
type       : TypeTag (1 byte)
modifiers  : u8 (TypeModifiers bitfield)
```

A schema with zero fields is valid; it indicates a record-free stream (just a terminator).

### TypeTag

The set of structural types is fixed:

| Tag | Type | v1 | Wire encoding |
|---|---|---|---|
| `0x00` | `Null` | ✓ | zero bytes |
| `0x01` | `Bool` | ✓ | 1 byte (0 or 1) |
| `0x02` | `Int` | ✓ | 8 bytes, little-endian i64 |
| `0x03` | `Float` | ✓ | 8 bytes, IEEE 754 binary64 (LE) |
| `0x04` | `String` | ✓ | `length: u32` + `length` bytes of UTF-8 |
| `0x05` | `Bytes` | ✓ | `length: u32` + `length` raw bytes |
| `0x06` | `Handle` | ✓ | 8 bytes (`RawHandle`, LE) |
| `0x07` | `List` | ✓ | `count: u32` + `count` × (`tag: u8` + that value's encoding) |
| `0x08` | `Record` | ✓ | nested sub-schema (as a Schema) + one row of values |
| `0x09` | `Error` | — | nested error structure, value-level (see below) |

The `TypeTag` byte values are pinned as above; `userspace/libstream/src/wire.rs` is the canonical source. The **v1** column marks what libstream encodes today; only the value-level `Error` tag remains recognised-but-unsupported.

A `List` is **self-describing per element**: each element carries its own `TypeTag` byte before its value, so a list may be heterogeneous (`List<Value>`) and may nest arbitrarily (a list of lists, or a list of records). An empty list stores no element type. A `Record` value likewise carries its own sub-schema, so it is self-describing. Both decode to the persistent, `Arc`-backed `Value::List`/`Value::Record`.

There is **no `Table` value tag** by design: a table is a whole *stream* (the `Schema` header + data records + terminator described by this document), not a nested cell. libstream's in-memory `Value::Table` serialises via that stream form (`Table::encode`/`decode`), and a `Value`'s `type_tag()` is therefore `Option<TypeTag>` — `None` for a table. This keeps "a `Value` is exactly what TSM1 can represent" honest: the scalar/`String`/`Bytes`/`Handle`/`List`/`Record` cell values are the tagged `WireValue`s; a table is the stream that contains them.

### TypeModifiers

```rust
bitflags! {
    pub struct TypeModifiers: u8 {
        const NULLABLE = 1 << 0;  // value may be Null even if type is not Null
        // bits 1..7 reserved
    }
}
```

A field with `NULLABLE` set may have a single-byte tag (0 or 1) prefixed to its value indicating presence. Absent (tag byte 0) values are represented as zero-length bytes following.

## Body: records

After the schema, the body is a sequence of records terminated by a terminator marker. Each record begins with a record tag byte:

```
RecordTag  := u8
   0x01 = data record
   0x02 = error record
   0xFF = terminator (end of stream)
```

### Data record (`0x01`)

```
DataRecord := tag(0x01) field_value*
```

`field_value` for each field in the schema, in declaration order, encoded per the field's `TypeTag`.

For variable-length types (`String`, `Bytes`), the wire encoding includes a length prefix as specified above; a `List` is a `u32` count followed by self-describing elements, and a `Record` is a nested sub-schema plus its values. For fixed-length types (`Int`, `Float`, `Handle`, `Bool`), the value is written directly.

For nullable fields (TypeModifiers includes `NULLABLE`), the value is preceded by a single byte: `0` for absent, `1` for present. If absent, no value bytes follow for that field.

### Error record (`0x02`)

```
ErrorRecord := tag(0x02) ErrorBody
ErrorBody   :=
    code: i32             (KError discriminant or 0 if not a kernel error)
    msg_len: u32
    msg: <msg_len> bytes UTF-8
    field_name_len: u16   (name of field that caused the error, or 0)
    field_name: <field_name_len> bytes UTF-8
```

Used to embed a structured error in the middle of a data stream — e.g., `filter` encountering a record that fails its predicate's type check produces an error record but continues processing.

Generic operators handle error records by passing them through unmodified; the consuming end (typically `display`) renders them.

Record tag `0x03` was reserved for a **widget record** (structured UI embedded in a stream). It has been dropped: TSM1 is a *data* format, and structured UI is a compositor concern, not a stream record type (see `docs/history/nitrox-ui-composition-model-v1.md` §1 "TSM1 stays data-only" and the decision log, 2026-07-23). A `0x03` record tag is now a decode error (`BadRecordTag`).

### Terminator (`0xFF`)

```
Terminator := tag(0xFF) exit_status
exit_status: i32
```

`exit_status` is the producing program's exit status. `0` for success; non-zero for failure with implementation-defined meaning (typically a process-exit-code-like integer).

## Text fallback

Programs that produce raw text (via `println!` and similar) have their output automatically wrapped by the runtime:

- Schema: one field, name `"line"`, type `String`
- One data record per output line, with the line's text (newline stripped)
- Terminator on EOF or program exit

The wrapping sets `StreamFlags::TEXT_FALLBACK`. Consumers may use this to render differently — for example, `display` may render text-fallback streams as plain text without table chrome.

## Schema inference via `#[derive(TypedRecord)]`

The `libstream` derive macro produces schema and encode/decode code from a Rust struct:

```rust
#[derive(TypedRecord)]
struct ProcessInfo {
    pid: u64,
    name: String,
    cpu: f64,
    handle: RawHandle,
}

let mut tw = TableWriter::new(stdout_handle);
tw.write_schema_for::<ProcessInfo>()?;
for p in processes {
    tw.write_record(&p)?;
}
tw.finish_with_status(0)?;
```

The macro reflects the struct's field names and types at compile time, generating the appropriate schema and per-field encoding calls. No registry, no coordination.

Field types map to `WireValue`s (a `Vec<V>` field is a `List` of `V`; a nested `#[derive(TypedRecord)]` struct is a `Record`). The full target set: primitive scalars (`bool`, `i*`, `u*`, `f32`, `f64`), `String`, `Bytes`/`Vec<u8>`, `Vec<V: WireValue>` → `List`, nested `#[derive(TypedRecord)]` structs → `Record`, `Option<T>` → nullable field, `RawHandle`. Deferred: enums (tagged unions), generics beyond `Vec<T>`, lifetimes beyond `'static`.

**v1 (flat records)** implements the scalar types + `String`/`Bytes`/`Option`/`RawHandle` only; `Vec` (→ `List`) and nested structs (→ `Record`) land with their first consumer (the shell rendering list/nested columns).

## Streaming model

A stream may be of indeterminate length. The producer writes header, then records as they become available, then terminator. The consumer reads incrementally; partial streams (no terminator yet) are valid and processable.

For interactive streams, the producer may keep the stream open indefinitely, writing records as events occur. The consumer's `display` (or other terminal) renders each record as it arrives.

The terminator is not optional but may be delivered out-of-band in some cases (e.g., when the stream is truncated by program crash, the kernel detects the closed handle and synthesizes a terminator with exit_status = `KError::PeerClosed`).

## Endianness

All multi-byte integers are little-endian (native on x86_64 and aarch64).

## Versioning

The `TSM1` magic encodes version 1. Future incompatible changes bump to `TSM2`, etc. Compatible additions (new TypeTag values, new flags) within version 1 are signaled by the field-by-field schema declaration; consumers reading an unknown TypeTag should produce an error record and skip the field.

## Tooling

`libstream` provides:
- `TableWriter` — writes header and records to a stream destination
- `TableReader` — reads header and iterates records
- `record_read` — reads a single record from a structured input
- `#[derive(TypedRecord)]` — auto-generates schema for a struct

The wire format is deliberately not exposed in the typical application API; programs work with typed Rust structs and the runtime handles encoding.

## Where to read more

- [Shell and typed streams architecture](../architecture/shell-and-streams.md)
- [TypedRecord usage in libstream](../reference/libstream-reference.md) (TBD)
