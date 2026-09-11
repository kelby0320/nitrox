# Drivers and IRPs

Hardware drivers in Nitrox are **kernel** code — but only the mechanism lives in
the kernel. The model is borrowed from Windows NT, which got this part right:
I/O flows through driver stacks as **I/O Request Packets (IRPs)**, work is split
across three execution contexts (IRQ → DPC → thread), and completion is
**asynchronous** — it signals a `PendingOperation` that a thread is waiting on
with `sys_wait`, never a blocking call inside a driver.

This document is design-level (the "what, and how the pieces relate"). The
normative contracts — the `Irp` layout, the driver/MMIO syscall ABIs, the
block-resource-server wire protocol — are `docs/spec/` material that lands with
the implementing slices; this doc cross-links forward to them. Background:
`docs/architecture/overview.md` (§ "Drivers and IRPs"),
`docs/architecture/notifications.md` (async event delivery),
`docs/architecture/handle-system.md` (kernel objects and handles), and the
original design in `docs/archive/os-design-v5.1.md` § "Driver Subsystem".

> **Status.** Implemented (Phase 2, complete). Every component this document once
> listed as unbuilt now exists: the IOAPIC (`kernel/src/arch/x86_64/ioapic.rs`), the
> DPC queue (`kernel/src/dpc.rs`), `PendingOperation`, `InterruptObject` and
> `DeviceNode` (`kernel/src/object/`), and the IRP framework (`kernel/src/io/irp.rs`),
> which carries real block I/O for `fs-server-ext4`. Individual deferrals — IRP
> cancellation, the module loader, driver-process `Handle<DeviceNode>` — are marked
> inline and in `deferred-decisions.md`. The § "Phase 2 scope" table at the end is a
> record of the original plan. Verified 2026-08-05; the DMA section below re-checked
> 2026-09-11, when `BlockBackend` gained `max_frags`, and § "Interrupts" rewritten the
> same day, when Phase 5 Part A made **MSI** the path a PCI driver takes and left INTx
> as the fallback.

## Three concepts, kept distinct

- **Kernel module** — a deployable unit of Rust code. *Tier 1* modules are
  compiled into the kernel ELF (selected by Cargo features); *Tier 2* modules
  are loaded at runtime. See § "Module tiers".
- **Device driver** — code that manages specific hardware (AHCI, NVMe, a NIC).
  A driver is delivered *by* a module but is a distinct idea.
- **Kernel resource server** — a subsystem that exposes resources through the
  namespace (e.g. a block device under `/dev`). A driver and a resource server
  often pair up (the AHCI driver registers a block resource server), but the
  driver manages hardware and the resource server speaks the namespace
  protocol.

Filesystem drivers are **not** in this picture at all — they are ordinary
userspace processes (`fs-server-ext4`), regardless of module tier.

## Execution contexts: IRQ > DPC > Thread

All driver work is partitioned across three contexts, in strict precedence
order. This is the foundation everything else rests on.

- **IRQ (interrupt) context** — the interrupt service routine (ISR). Does the
  *minimum*: acknowledge the device, capture status, and queue a **DPC** (or
  signal an `InterruptObject`). No allocation, no blocking; a brief spinlock at
  most. It runs with interrupts effectively masked and must return fast.
- **DPC (Deferred Procedure Call) context** — still cannot block, but runs the
  real completion work: advance or complete an IRP, run completion routines back
  up a driver stack, and signal the IRP's `PendingOperation` and any
  `InterruptObject` waiters. DPCs are drained after the ISR returns, before
  control goes back to the interrupted thread — a software-interrupt ("softirq")
  tier above thread priority. A DPC may take a spinlock briefly but never sleeps.
- **Thread context** — everything else: a thread *initiates* an IRP, then blocks
  in `sys_wait`. The bulk of a driver's logic that *can* block lives here (for a
  userspace driver, this is the driver process's own threads).

A **`DpcNode`** is an inline field of its owning structure (an `Irp`, a `Timer`)
— a linked-list node plus a handler pointer — so queuing a DPC on the completion
fast path allocates nothing. This matters: per `kernel/CLAUDE.md`, allocation in
IRQ/DPC context is forbidden.

> **Reconciliation with Phase 1.** Phase 1 wakes `sys_wait`ers *directly* from
> the timer-tick handler under the rank-1 scheduler lock (see the decision log,
> 2026-06-08). The `phase-2/dpc` item builds the DPC queue described here and
> drains it at the interrupt-dispatch tail; **device ISRs** are its producers.
> The timer's *own* deadline-firing stays inline — it is the timekeeping
> subsystem's tick work, already at the right point (bounded, before the
> reschedule) — rather than migrating onto the queue; the DPC queue serves
> device-ISR deferred work, which is what the IRQ>DPC>Thread model exists for.
> (An earlier draft of this doc said the timer wakeup would migrate onto the
> queue; see the decision log, 2026-06-12.) On a device IRQ the dispatcher runs
> the registered ISR, EOIs, then drains pending DPCs before returning.

## Interrupts

### Routing: GSI → vector → ISR

On x86_64 the local APIC handles the per-CPU timer (Phase 1). A **device**
interrupt reaches a CPU by one of two routes, and a Tier 1 driver asks for one
through [`ArchIrqInstall`](../../kernel/src/arch/irq_install.rs) — a facility of
its own rather than a method on the router or the local controller, because
installing an interrupt spans both plus the handler registry.

- **MSI**, preferred wherever a PCI function advertises the capability. The
  device is handed an address and a value and raises the interrupt by writing
  them itself, so nothing routes it: no IOAPIC entry, no ACPI `_PRT`, and no
  two devices sharing a vector. The **capability's layout** is PCI-SIG and lives
  in [`pci`](../../kernel/src/pci/mod.rs) (`read_msi`, `program_msi`); the
  **message's contents** are architectural and come from `install_msi`. That
  split is why the arch half yields a message rather than programming a device.
- **INTx**, the fallback for a function advertising no MSI capability. The
  **IOAPIC**, located and configured from the ACPI **MADT**, routes a hardware
  *Global System Interrupt* (GSI) to an IDT vector on a chosen CPU; the vector's
  stub enters a registered kernel ISR. (`phase-2/ioapic`, building on
  `phase-2/acpi-tables`.) The GSI comes from the PCI interrupt-line register,
  which is the part that does not survive contact with real hardware — and the
  reason MSI is preferred.

**Deferred:** MSI-X, and shared PCI INTx (the "chain of handlers, each returns
*mine* / *not mine*" model). Neither MSI nor MSI-X is ever shared, so the second
matters only for a device that has neither.

### `InterruptObject` — an IRQ source as a waitable

A hardware IRQ source is exposed as an **`InterruptObject`** kernel object. It is
a **waitable**: the ISR *signals* it, and a driver thread blocked in `sys_wait`
on its handle wakes. This is the single programming model that works for both
in-kernel (Tier 1) and future userspace (Tier 2) drivers — "hold a handle to the
interrupt, wait on it" — and is why the kernel can hand a userspace driver an
`InterruptObject` and let it service hardware without any in-kernel driver code.

Two usage patterns sit on top of the same ISR→DPC base:

1. **Block-on-`InterruptObject`** (primary, userspace-compatible): a driver
   thread sleeps in `sys_wait`; the ISR signals the object via a DPC, waking the
   thread, which then does the device work in thread context.
2. **DPC completion routine** (in-kernel only): a Tier 1 driver completes an IRP
   directly from the DPC, with no dedicated driver thread — lower latency for the
   boot-path block driver.

Signalling reuses the Phase 1 wait machinery exactly as a `Timer` or
`NotificationChannel` does (see below); `InterruptObject` is simply a new arm in
the scheduler's waitable dispatch. (`InterruptObject` lands with `phase-2/ioapic`
/ the storage slice.)

## The IRP model

An **`Irp`** (I/O Request Packet) is the unit of I/O. It is **kernel-internal** —
not a handle-accessible kernel object — like a VMA or a page-table entry. Shape
(normative layout lands in `docs/spec/` with the framework):

```rust
struct Irp {
    operation:  IrpOp,             // Read, Write, ...
    initiator:  ProcessId,
    completion: PendingOperation,  // signalled when the IRP completes
    buffer:     /* KBox<[u8]> or a MemoryObject reference for bulk data */,
    offset:     u64,
    params:     IrpParams,
    stack:      IrpStack,          // the driver stack this IRP descends
    status:     IrpStatus,
    dpc:        DpcNode,           // inline — no heap alloc to queue completion
}
```

**Lifecycle:**

1. A thread (or an upper-layer driver) **initiates** an IRP and gets back the
   IRP's `PendingOperation` handle.
2. The IRP flows **down** the driver stack. Each layer either completes it
   immediately or forwards it to the layer below. The bottom layer programs the
   hardware and returns *Pending*.
3. The thread `sys_wait`s on the `PendingOperation` (alongside any other
   waitables) — it does **not** block inside the I/O call.
4. The completion **IRQ** fires; the ISR acknowledges the device and queues the
   IRP's `DpcNode`.
5. The **DPC** runs the stack's completion routines **up** from the bottom — this
   is where Rust's ownership model pays off: a completion routine cannot hold a
   reference to a stack frame that has already returned. The DPC signals the
   IRP's `PendingOperation`.
6. The initiator's `sys_wait` returns; it reads the result.

Phase 2 stacks are shallow: AHCI is a single layer (request → hardware → done),
and the GPT partition driver over the block device is the first real two-layer
stack (GPT translates a partition-relative request into a disk-absolute one and
forwards). **As built** (slice 6), that two-layer stack is realised by
**`BlockBackend` delegation** — a partition's backend rebases the IRP's offset and
forwards to the disk's backend (`io::block::Partition`) — rather than by walking
the `Irp`'s `stack_index`/`IrpStackFrame` array frame-by-frame. The frame array +
per-frame completion routines remain part of the (hashed) `Irp` layout, designed
ahead for the case that needs them. **Deferred:** transparent **filter drivers**
(encryption, compression, logging inserted into a stack) — the first genuine user
of formal multi-frame descent — IRP **cancellation**, and the 30-second completion
**timeout**.

## Async completion and `sys_wait`

The driver framework is the archetype of the system's async-first rule (see
`docs/rationale/why-async-syscalls.md`): every potentially-blocking operation
returns a **`PendingOperation`** handle, and a thread blocks only by calling
`sys_wait` on it. The same `sys_wait` waits on timers, IPC, child-exit
notifications, and I/O completion — there is no blocking `read()`.

For I/O, the `PendingOperation` is owned by the `Irp`; completing the IRP
signals it. A `PendingOperation` is a **waitable**, added to the scheduler's
dispatch like every other.

**Synchronous fast path:** an operation that completes immediately (a cache hit,
a zero-length request) still returns a `PendingOperation` — but a **pre-signalled**
one, so the caller's next `sys_wait` returns without ever blocking. Callers thus
have one code path regardless of whether the work was sync or async.

> **Reconciliation with Phase 1.** The wait mechanism is the one Phase 1
> actually built — a pre-reserved **waiter list per waitable** plus a fixed
> 8-slot wait array on each `Thread` (`MAX_WAIT_HANDLES`), dispatched by `match`
> on `KObjectType` in `kernel/src/sched.rs` (`obj_already_signaled` /
> `obj_add_waiter` / `obj_remove_waiter`; the `_ => …` arms are the extension
> seam). It is **not** the intrusive `WaitNode` list the v5.1 doc sketched.
> Adding `PendingOperation` and `InterruptObject` as waitables means adding arms
> to those three match sites plus a signal path — no new wait infrastructure.
> (`PendingOperation` lands with `phase-2/pending-operation`.)

## Device discovery and enumeration

Hardware is discovered through firmware tables and represented uniformly:

- **ACPI MCFG** locates PCIe ECAM, and the kernel enumerates the PCI(e) bus,
  reading each function's config space (vendor/device id, class, BARs, interrupt
  line/pin). **ACPI MADT** provides the interrupt-routing topology (IOAPICs,
  GSIs). Both come from `phase-2/acpi-tables`; PCI enumeration is part of the
  storage slice.
- Each discovered device becomes a **`DeviceNode`** kernel object —
  architecture-independent (on aarch64 the same nodes would come from a Device
  Tree Blob). A `DeviceNode` carries a **resource descriptor**: its MMIO regions
  (BARs), its interrupt (GSI), and identity.

**Driver matching.** Phase 2 matches in-kernel: a built-in table maps a
`DeviceNode`'s identity/class to a compiled-in **Tier 1** driver, which claims
the node. The userspace **driver manager** — matching nodes to Tier 2 modules
and handing a driver process a `Handle<DeviceNode>` — is **deferred** (it needs
the Tier 2 loader).

**MMIO mapping.** A driver maps a device's register window with

```rust
sys_device_map_mmio(device: RawHandle, region_idx: u32, flags: MmioFlags) -> RawHandle
```

which consults the `DeviceNode`'s resource descriptor and returns a
`MemoryObject` over the BAR (mapped uncached). For a userspace driver the kernel
would simultaneously program the IOMMU — **deferred**, since Phase 2 has no
userspace drivers.

**DMA.** Bus-mastering devices need physically-contiguous, suitably-aligned
buffers (AHCI command lists and PRDTs, for example) and their **physical
address**. That path exists: [`mm::dma::DmaBuffer`](../../kernel/src/mm/dma.rs)
(the `phase-2/dma-alloc` item) — a zeroed, page-aligned contiguous block from the
buddy allocator that exposes both a CPU (HHDM) pointer and `phys()`. (x86 DMA is
snoop-coherent, so no cache maintenance; a non-coherent arch will add an `ArchDma`
clean/invalidate hook.) IOMMU-constrained DMA (so a userspace driver can only
touch memory it legitimately holds) is **deferred** with userspace drivers.

**A bus-mastering device has to be *told* it may master the bus** — PCI command
register bit 2 — and the driver sets it ([`pci::enable_bus_master`](../../kernel/src/pci/mod.rs)).
Until Phase 5 Part A nothing did, and DMA worked anyway because every firmware we
had booted under leaves the bit set; that is a property of those firmwares rather
than of the machine, so the driver no longer inherits it. It reports which
happened, because "already enabled by firmware" and "enabled by the driver" are
different facts about a new machine.

**It has to be set before the driver's own bring-up DMAs**, which is narrower than
it sounds: AHCI starts FIS receive and runs `IDENTIFY DEVICE` before it has a disk
to publish, and both are bus-master transactions. Setting the bit after them works
on every machine whose firmware had already set it and on no other — so it would
pass every gate we own and lose the disk on the first machine that needed it. It
is set immediately after the controller's port is chosen, ahead of every
allocation and every register write that starts the port.

**A `DmaBuffer` is for the driver's own structures, not for the data.** The transfer
itself DMAs straight into the client's `MemoryObject` frames: `io::block::build_frags`
describes the requested byte range as a physical fragment list, one fragment per page
touched, and the driver turns that into the device's scatter-gather form (for AHCI, a
PRDT). No bounce buffer, no copy.

**How many fragments one command can describe is a device property, and the device
publishes it** — [`BlockBackend::max_frags`](../../kernel/src/io/block.rs). AHCI's is
its command table's PRDT capacity: one page, 128 bytes of command FIS, 16 bytes per
entry, so **248** entries ≈ 992 KiB of page-aligned transfer. `dispatch_block_irp`
refuses a larger request with `InvalidArgument`; a partition inherits its disk's limit
rather than declaring one; the ramdisk declares none (`u32::MAX`), having nothing
fixed-size to overrun.

**This is a bound, not a policy, and it was once absent.** `sys_io_submit` bounds
`buf_offset + length` against the buffer's size and nothing else, so before 2026-09-11
a caller holding a block `DeviceNode` handle could make the driver write PRDT entries
past its own command table — after which the controller read descriptors from whatever
followed and DMAed to the addresses it found there. Splitting a large transfer across
several commands is the better answer and is `TODO(block-transfer-split)`.

## Module tiers

| Tier | Where | Examples | Phase 2 |
|---|---|---|---|
| **Tier 1** | compiled into the kernel ELF, gated by Cargo features | `pci` (always on), `ahci`, `gpt` (always on), `nvme` (later) | **yes** |
| **Tier 2** | loaded at runtime by a userspace driver manager (`SysCaps::LOAD_MODULE`) | hot-pluggable / optional hardware, debug tools | **deferred** |

**Rule:** boot-path drivers are Tier 1; hot-pluggable or optional ones are
Tier 2. Tier 2 requires the kernel-module loader — an `export!` table, ELF
relocation, and **ABI-hash** enforcement so a module is refused unless it was
built against this exact kernel (see `docs/spec/abi-version-hash.md`). That
machinery is **deferred**; everything Phase 2 needs (PCI, AHCI, GPT) is Tier 1.

## Block-device drivers as resource servers

A Tier 1 block driver does two things: it drives the controller (IRPs to
hardware), and it **registers a block-device resource server** so the rest of
the system can issue reads without knowing about AHCI. The GPT partition driver
sits above it (translating partition-relative offsets), and `fs-server-ext4`
(userspace) ultimately reads file data through the same path:

```
fs-server-ext4 (userspace)  →  block resource server  →  GPT  →  AHCI  →  disk
        ↑ rsproto over IPC            ↑ namespace             ↑ IRP stack
```

The block resource server's wire protocol and namespace binding belong to the
namespace / resource-server slice; this doc only fixes the relationship.

## Phase 2 scope vs deferred

| Lands in Phase 2 | Deferred (and to where) |
|---|---|
| IRQ → DPC → Thread context model | Tier 2 / LKM loading + ABI-hash enforcement |
| IOAPIC-routed, non-shared interrupts | MSI / MSI-X; shared PCI INTx chaining |
| `InterruptObject` (waitable) | IOMMU + userspace drivers |
| The DPC queue (timer-tick migrates onto it) | IRP cancellation; 30 s completion timeout |
| The IRP framework (shallow stacks) | Filter drivers (encryption / compression / logging) |
| `PendingOperation` + `sys_wait` integration | NVMe (AHCI first) |
| PCI(e) ECAM enumeration; `DeviceNode` | Writeback IRPs (Phase 3, with RW fs) |
| AHCI (Tier 1); GPT; block resource server | — |
| `sys_device_map_mmio`; DMA allocation | — |

All deferrals are tracked in `docs/rationale/deferred-decisions.md`.

## Cross-references

- `phase-2/acpi-tables` — RSDP → XSDT/RSDT → **MADT** (interrupt routing) +
  **MCFG** (PCIe ECAM); the table layer this whole doc depends on.
- `phase-2/ioapic` — implements § "Interrupts" (GSI routing, the ISR path,
  `InterruptObject`).
- `phase-2/dpc` — implements § "Execution contexts" and the DPC half of
  interrupts and IRP completion.
- `phase-2/pending-operation` — implements § "Async completion".
- The **storage slice** — implements § "The IRP model", § "Device discovery",
  and § "Block-device drivers as resource servers" (PCI, AHCI, GPT, `DeviceNode`,
  the block resource server).
- `docs/decision-log.md` (2026-06-11) — the decisions recorded here.
- `docs/planning/implementation-plan.md` (Phase 2) — slice ordering and the
  prerequisite band.
