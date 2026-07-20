# Typed Stream Format (TSM1) Specification

This document specifies the wire format of typed structured streams — the format used by programs to communicate via stdin/stdout pipelines. The format is identified by the magic bytes `TSM1` (Typed Stream Magic, version 1).

**Status:** Pre-stabilization. The envelope and structural types are committed; the `TypeTag` byte values are now **pinned** (below), implemented in `userspace/libstream/src/wire.rs` (the canonical source for byte-level details). **v1 implements flat records** — the scalar types plus `String`/`Bytes`/`Handle`; `List` (a `Vec`) and `Record` (nested struct) reserve their tags but are not yet encoded (they land with their first consumer, the shell).

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
| `0x07` | `List` | — | `length: u32` + `length` × inner-type encoding |
| `0x08` | `Record` | — | nested sub-schema + values |
| `0x09` | `Error` | — | nested error structure, value-level (see below) |

The `TypeTag` byte values are pinned as above; `userspace/libstream/src/wire.rs` is the canonical source. The **v1** column marks what libstream encodes today; `List`/`Record`/`Error` are recognised on the wire but return an "unsupported" error until implemented.

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
   0x03 = widget record
   0xFF = terminator (end of stream)
```

### Data record (`0x01`)

```
DataRecord := tag(0x01) field_value*
```

`field_value` for each field in the schema, in declaration order, encoded per the field's `TypeTag`.

For variable-length types (`String`, `Bytes`, `List`), the wire encoding includes a length prefix as specified above. For fixed-length types (`Int`, `Float`, `Handle`, `Bool`), the value is written directly.

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

### Widget record (`0x03`)

```
WidgetRecord := tag(0x03) WidgetBody
WidgetBody   :=
    widget_type: u8       (WidgetType enum)
    style_len: u32
    style: <style_len> bytes (StyleDescriptor encoded as record)
    data_stream_handle: RawHandle    (8 bytes)
    control_channel_handle: RawHandle (8 bytes)
    actions_count: u16
    actions: <actions_count> × ActionSpec
```

`WidgetType`:
```rust
#[repr(u8)]
pub enum WidgetType {
    Table   = 0,
    Chart   = 1,
    Form    = 2,
    Tree    = 3,
    Canvas  = 4,
}
```

`ActionSpec` is a small record describing user-invokable actions on the widget; details TBD when widget rendering is implemented.

The handles in `data_stream_handle` and `control_channel_handle` are transferred via the IPC mechanism that delivers the stream (typically inline in the IPC message's handle list, with the wire encoding referring to slot indices in that list — TBD).

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
