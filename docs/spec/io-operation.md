# IoOp — `sys_io_submit` operation descriptor

This document specifies `IoOp`, the `#[repr(C)]` block a caller passes to
[`sys_io_submit`](syscall-abi.md) by `UserPtr<IoOp>` to describe one
asynchronous I/O operation, and `IoOpcode`, the operation selector. Both are
**kernel-ABI version-hash inputs** (`abi-version-hash.md` § "IoOp and IoResult
layouts").

**Status:** Pre-stabilization. Introduced with the storage slice (Phase 2
slice 5). The layout below is the Phase 2 form; opcodes beyond `Read`/`Write`
are reserved (see § Deferred).

## The async I/O core

`sys_io_submit` is the system's single generic asynchronous-I/O entry point. It
is the archetype of the async-first rule (`docs/rationale/why-async-syscalls.md`):
it **never blocks**. It validates the request, initiates an
[`Irp`](irp-layout.md) down the target's driver stack, and returns a
`PendingOperation` handle. The caller blocks — if it chooses — by passing that
handle to `sys_wait`, alongside any other waitables. The same `IoOp` is what the
high-throughput [`IoRing`](syscall-abi.md#high-throughput-ring) submits per
entry, so the descriptor is defined once here and reused by both paths.

```rust
fn sys_io_submit(resource: RawHandle, op: UserPtr<IoOp>) -> isize
fn sys_io_cancel(pending: RawHandle) -> isize
```

`resource` is the handle the operation targets. In Phase 2 this is a **block
[`DeviceNode`](device-node.md)** (a whole disk, or — slice 6 — a partition); the
opcode set is block read/write. The descriptor and syscall are deliberately
resource-agnostic so future resource kinds (char devices, sockets) reuse them.

- **Returns** a `PendingOperation` handle (a positive value) on a successfully
  *initiated* operation. The operation's outcome is delivered through that PO:
  `sys_wait` writes `IoResult.status` (`0` = success, negative = a `KError`) and
  `IoResult.result` (the **bytes transferred**). A synchronous fast path (a
  zero-length request, a cache hit) still returns a PO — **pre-signalled** — so
  the caller has one code path regardless.
- **Returns a negative `KError` synchronously**, with **no** PO created, for
  *argument*, *permission*, and *allocation* failures: a bad `resource` handle or
  one lacking the required right, a malformed `IoOp` (unknown opcode, reserved
  flag set, misaligned `offset`/`length` for a block device), a `buffer` that is
  not a `MemoryObject` or is too small for `buf_offset + length`, or PO/handle
  exhaustion. *Device/medium* failures (the disk NAKs, a bad sector) are
  **operation** outcomes and are delivered through the PO, not synchronously —
  the same split the namespace lookup uses (`syscall-abi.md` § Namespace).

`sys_io_cancel` requests cancellation of an in-flight operation. **Phase 2
returns `Unsupported`** — IRP cancellation is deferred (`deferred-decisions.md`
§ "Drivers and interrupts"); the syscall number is reserved now so the ABI is
stable when cancellation lands.

## Layout

```rust
#[repr(C)]
pub struct IoOp {
    pub opcode:     u32,        // offset 0  — IoOpcode discriminant
    pub flags:      u32,        // offset 4  — reserved; must be 0
    pub buffer:     RawHandle,  // offset 8  — MemoryObject for the data (u64)
    pub buf_offset: u64,        // offset 16 — byte offset within `buffer`
    pub offset:     u64,        // offset 24 — byte offset within the resource
    pub length:     u64,        // offset 32 — bytes to transfer
}                               // total 40 bytes, 8-byte aligned, no padding
```

Total size 40 bytes, 8-byte aligned, no interior padding. The offsets are pinned
by compile-time `offset_of!`/`size_of` asserts on both the kernel
(`kernel/src/libkern/io_op.rs`) and `libkern` sides.

## Fields

- **`opcode`** — an [`IoOpcode`](#ioopcode). An unrecognised value returns
  `InvalidArgument` synchronously.
- **`flags`** — reserved for per-operation modifiers (e.g. a future
  force-unit-access / no-cache bit). No flag is defined yet, so any set bit
  returns `InvalidArgument`.
- **`buffer`** — a `MemoryObject` handle providing the data buffer: the
  destination for `Read`, the source for `Write`. The handle must carry the
  matching `MAP_*` right (`MAP_WRITE` for `Read` — the kernel writes device data
  into it; `MAP_READ` for `Write`). The kernel addresses the object's frames
  directly for DMA; the caller need not have it mapped. A `buffer` of
  `RawHandle::NULL` is valid only for a zero-`length` request.
- **`buf_offset`** — byte offset into `buffer` where the transfer begins.
  `buf_offset + length` must not exceed the object's size, else
  `InvalidArgument`.
- **`offset`** — byte offset **within the resource** (the device) at which to
  read or write. For a block device it must be a multiple of the device's
  logical block size (`offset` past the device's end completes the PO with
  `InvalidArgument`); the block layer is byte-addressed at the ABI and converts
  to LBA internally, so the descriptor stays resource-agnostic.
- **`length`** — number of bytes to transfer. For a block device it must be a
  multiple of the logical block size. `0` is a legal no-op that completes a
  pre-signalled PO with `result = 0` (lets a caller probe a resource's
  readiness/rights cheaply). The per-call ceiling is `MAX_USER_COPY_SIZE`
  (16 MiB), matching the rest of the syscall surface.

## Device classes (block vs. char)

`sys_io_submit` dispatches on the resource's **device class**
([`device-node.md`](device-node.md)):

- **Block** devices (disks, partitions) follow the rules above: `offset`/`length`
  are logical-block multiples, translated into an [`Irp`](#relationship-to-the-irp).
- **Char/stream** devices (the **serial console**, `/dev/console`) accept a `Read`
  only (input). The block-alignment rules **do not apply**: `offset` is ignored
  (a stream has no addressable position), and `length` is the **maximum** bytes to
  read — the PO completes with `result` = the bytes actually delivered (≥ 1, ≤
  `length`), which arrive when the device's RX interrupt fires (or immediately, if
  bytes are already buffered). The bytes land in `buffer` exactly as for a block
  read. `Write` to a char device is `Unsupported` in Phase 2 (output stays on the
  kernel log path; symmetric console write is deferred). The `buffer` is still a
  `MemoryObject` with `MAP_WRITE`, and `buf_offset + length <= buffer.size()` still
  holds. The completion is delivered through the same `PendingOperation`; no
  device-specific syscall exists.

## IoOpcode

```rust
#[repr(u32)]
pub enum IoOpcode {
    Read  = 0,   // device → buffer
    Write = 1,   // buffer → device
}
```

Defined in `kernel/src/libkern/io_op.rs` (mirrored in `libkern`). The
discriminant set is part of the ABI version hash; adding a variant changes the
hash.

## Rights

`Read` requires the `READ` right on `resource`; `Write` requires `WRITE`. A
block `DeviceNode` bound read-only into a namespace (the Phase 2 default — see
[`device-node.md`](device-node.md)) therefore rejects `Write` at the lookup-rights
gate, before any IRP is built. The buffer-side rights (`MAP_READ`/`MAP_WRITE` on
`buffer`) are checked independently, as above.

## Relationship to the IRP

`sys_io_submit` translates an `IoOp` into an [`Irp`](irp-layout.md): `opcode` →
`IrpOp`, `offset`/`length` → the IRP's `offset`/`length`, and `buffer` (resolved
to its physical frames) → the IRP's data reference. The IRP descends the target
`DeviceNode`'s driver stack; its completion DPC signals the PO this syscall
returned. The `IoOp` is the **userspace-facing** request; the `Irp` is its
**kernel-internal** realisation.

## Deferred

- Opcodes beyond `Read`/`Write` — `Flush` (barrier/FUA), `Trim`/`Discard`,
  device-specific control. Added with a consumer (RW filesystems, SSD trim).
- The `flags` modifiers (force-unit-access, no-cache).
- `sys_io_cancel` semantics (returns `Unsupported` until IRP cancellation lands).
- Scatter/gather over multiple buffers in one `IoOp` (Phase 2 is single-buffer;
  the IRP's PRDT already supports multiple physical fragments of one object).

## Where to read more

- [Syscall ABI](syscall-abi.md) (§ "I/O Core")
- [IRP layout](irp-layout.md)
- [DeviceNode](device-node.md)
- [Why async-first syscalls](../rationale/why-async-syscalls.md)
- [Drivers and IRPs](../architecture/drivers-and-irps.md)
