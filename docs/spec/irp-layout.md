# IRP layout

This document specifies the layout of `Irp` (I/O Request Packet) and its
sub-types — the kernel-internal unit of I/O. The design rationale and the
IRQ → DPC → Thread execution model live in
[`docs/architecture/drivers-and-irps.md`](../architecture/drivers-and-irps.md);
this is the normative layout contract.

**Status:** Pre-stabilization. Introduced with the storage slice (Phase 2
slice 5). The form below is the Phase 2 shape (shallow stacks; no cancellation,
no timeout, no filter drivers — see § Deferred).

## Why this is a hashed contract

An `Irp` is **kernel-internal** — not a handle-accessible kernel object, like a
VMA or a page-table entry; userspace never sees one. But a Tier 2 loadable
kernel module (a future driver) is handed `&mut Irp` and walks its fields, so the
layout is a **kernel-ABI version-hash input** (`abi-version-hash.md` § "IRP
layout"): `Irp`, `IrpStack`, `IrpStackFrame`, and the `IrpOp`/`IrpStatus` enums.
Any field/offset/size change invalidates the hash and forces all modules to
rebuild. Phase 2 has only Tier 1 (compiled-in) drivers, so nothing is loaded
against the hash yet, but the layout is fixed here so it is stable when the Tier 2
loader lands. The offsets are pinned by compile-time asserts in
`kernel/src/io/irp.rs` when the type is implemented (Part 2).

## Irp

```rust
#[repr(C)]
pub struct Irp {
    pub op:          u32,            // offset 0   — IrpOp
    pub status:      i32,            // offset 4   — IrpStatus (valid once complete)
    pub flags:       u32,            // offset 8   — reserved; must be 0
    pub stack_count: u32,            // offset 12  — populated stack frames (≤ IRP_MAX_STACK)
    pub stack_index: u32,            // offset 16  — current frame (descends high→low)
    pub _pad:        u32,            // offset 20
    pub initiator:   u64,            // offset 24  — ProcessId that submitted it
    pub offset:      u64,            // offset 32  — byte offset within the bottom device
    pub length:      u64,            // offset 40  — bytes to transfer
    pub transferred: u64,            // offset 48  — bytes actually transferred (set on completion)
    pub buffer:      IrpBuffer,      // offset 56  — the data buffer (physical reference, 16 B)
    pub completion:  u64,            // offset 72  — *mut PendingOperation (kernel ptr; 0 if none)
    pub dpc:         DpcNode,        // offset 80  — inline; queues completion with no alloc
    pub stack:       IrpStack,       // after dpc  — the per-layer frames (size depends on DpcNode)
}
```

`IRP_MAX_STACK = 4`. Phase 2 stacks are at most two layers deep
(GPT-over-AHCI); the headroom costs little and avoids a reallocation seam. The
trailing `stack` array makes `Irp` a fixed-size, single-allocation object: a
driver allocates one `Irp` in thread context (never in IRQ/DPC context) and it
carries everything down and back up.

### Fields

- **`op`** — an [`IrpOp`](#irpop), set from the submitting `IoOp.opcode`.
- **`status`** — an [`IrpStatus`](#irpstatus). Meaningful **only after** the IRP
  completes (the `completion` PO is signalled); while pending it is
  `IrpStatus::PENDING`.
- **`flags`** — reserved; must be 0.
- **`stack_count`** — number of populated `stack` frames, set when the IRP is
  built for a particular device stack (1 for a bare AHCI disk; 2 for a partition
  over it).
- **`stack_index`** — the frame the IRP is *currently at*. An IRP descends from
  `stack_count - 1` down to `0` (the bottom hardware layer); completion routines
  run **up** as the index rises back. This mirrors NT's `CurrentStackLocation`.
- **`initiator`** — the `ProcessId` that called `sys_io_submit`, for accounting
  and (future) cancellation-on-exit.
- **`offset`/`length`** — the request as seen by the **bottom** device, in bytes.
  A partition layer rewrites `offset` (adding the partition's start) as it
  forwards the IRP down (see `drivers-and-irps.md` § "The IRP model").
- **`transferred`** — bytes actually moved; copied into `IoResult.result` for the
  initiator. Equal to `length` on full success.
- **`buffer`** — the data buffer as a physical reference (see
  [`IrpBuffer`](#irpbuffer)). DMA targets it directly.
- **`completion`** — a kernel pointer to the `PendingOperation` that
  `sys_io_submit` returned and the completion DPC signals. `0` for an internal
  IRP with no userspace waiter (none in Phase 2).
- **`dpc`** — an **inline** [`DpcNode`](../../kernel/src/dpc.rs). The completion
  ISR queues *this* node (no heap allocation in IRQ context, per
  `kernel/CLAUDE.md`); the drained DPC runs the completion routines and signals
  `completion`.
- **`stack`** — the [`IrpStack`](#irpstack) of per-layer frames.

## IrpOp

```rust
#[repr(u32)]
pub enum IrpOp {
    Read  = 0,
    Write = 1,
}
```

The Phase 2 set mirrors [`IoOpcode`](io-operation.md#ioopcode) (the two are kept
numerically aligned for a trivial translation). Internal-only ops (a future
`Flush` barrier, partition-table re-read) are added here without necessarily
having an `IoOpcode` peer.

## IrpStatus

```rust
#[repr(i32)]
pub enum IrpStatus {
    Pending = 1,    // in flight; `status` not yet meaningful
    Success = 0,    // completed; `transferred` bytes moved
    // negative values are KError discriminants delivered to the initiator
}
```

A completed IRP carries either `Success` (`0`) or a negative `KError`
discriminant (e.g. `IoError`, `InvalidArgument`), so `Irp::status` maps directly
onto `IoResult::status` with no translation. `Pending` (`1`) is the in-flight
sentinel and never reaches `IoResult` (the PO is unsignalled while pending).

## IrpBuffer

```rust
#[repr(C)]
pub struct IrpBuffer {
    pub kind:  u32,   // offset 0  — 0 = none, 1 = MemoryObject frames
    pub count: u32,   // offset 4  — number of physical fragments
    pub frags: u64,   // offset 8  — *const PhysFrag (kernel ptr to the fragment list)
}                     // 16 bytes
```

The buffer is described as a list of physically-contiguous fragments
(`PhysFrag { base: PhysAddr, len: u64 }`) derived from the `MemoryObject`'s
frames — exactly the form an AHCI PRDT or an NVMe PRP list consumes. A
single-frame object is one fragment; a larger object is one fragment per
contiguous run. The fragment list is owned by the IRP's allocation context
(kernel-resident for the IRP's lifetime); DMA reads/writes it directly. Bulk DMA
staging buffers come from [`mm::dma::DmaBuffer`](../../kernel/src/mm/dma.rs).

## IrpStack

```rust
#[repr(C)]
pub struct IrpStackFrame {
    pub device:     u64,   // offset 0  — *DeviceNode this frame targets (kernel ptr)
    pub completion: u64,   // offset 8  — completion-routine fn ptr (0 = none)
    pub context:    u64,   // offset 16 — per-layer scratch (e.g. a partition's base LBA)
}                          // 24 bytes

// Irp.stack is `[IrpStackFrame; IRP_MAX_STACK]` (IRP_MAX_STACK = 4).
```

Each frame names the [`DeviceNode`](device-node.md) that layer drives, an
optional **completion routine** run (in DPC context) as the IRP unwinds upward,
and a per-layer `context` word. The partition (GPT) layer stores the partition's
starting offset in `context`, adds it to `Irp.offset` on the way down, and needs
no completion routine; the AHCI layer at the bottom programs the hardware and
returns `Pending`. A completion routine **cannot** reference a returned stack
frame — Rust's ownership model enforces what NT documents by convention.

## Lifecycle

The numbered lifecycle (initiate → descend → hardware → IRQ → DPC unwind →
`sys_wait` returns) is in
[`drivers-and-irps.md`](../architecture/drivers-and-irps.md) § "The IRP model".
In layout terms:

1. `sys_io_submit` allocates an `Irp`, fills `op`/`offset`/`length`/`buffer`/
   `initiator`, sets `completion` to the new PO, builds `stack[0..stack_count]`
   for the target `DeviceNode`'s stack, sets `stack_index = stack_count - 1`,
   `status = Pending`.
2. Each layer, at `stack_index`, either completes the IRP or adjusts it
   (e.g. partition rebases `offset`), decrements `stack_index`, and forwards.
3. The bottom layer programs the device (DMA against `buffer.frags`) and leaves
   the IRP `Pending`.
4. The completion IRQ's ISR acknowledges the device and queues `irp.dpc`.
5. The drained DPC runs completion routines up (`stack_index` rising), sets
   `status`/`transferred`, and signals `completion`.
6. The initiator's `sys_wait` returns; `IoResult.status = irp.status`,
   `IoResult.result = irp.transferred`.

## Deferred

Tracked in `docs/rationale/deferred-decisions.md` § "Drivers and interrupts":

- **Cancellation** — no cancel field / cancel routine; `sys_io_cancel` is
  `Unsupported`. Adds a `cancel` fn ptr + a cancellable flag when it lands.
- **Completion timeout** — the 30 s force-complete is not built.
- **Filter drivers** — transparent stack insertion; the stack is fixed at build
  time in Phase 2.
- **Stacks deeper than `IRP_MAX_STACK`** — re-evaluated if a real stack exceeds
  four layers (none does in Phase 2/3).

## Where to read more

- [Drivers and IRPs](../architecture/drivers-and-irps.md) — the model and rationale
- [IoOp](io-operation.md) — the userspace request that becomes an IRP
- [DeviceNode](device-node.md) — what an IRP's stack frames target
- [ABI version hash](abi-version-hash.md) — why this layout is hashed
