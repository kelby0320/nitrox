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
original design in `docs/history/os-design-v5.1.md` § "Driver Subsystem".

> **Status.** This is a Phase 2 design doc. Almost nothing here is built yet:
> Phase 1 shipped the LAPIC timer and the `sys_wait` wait machinery, but the
> IOAPIC, the DPC queue, `PendingOperation`, `InterruptObject`, `DeviceNode`,
> and the IRP framework are all Phase 2 work (the latter four are currently
> `KObjectType` tags with no implementation). Each section names the prereq item
> or slice that implements it. See the § "Phase 2 scope" table at the end.

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

> **Reconciliation with Phase 1.** Phase 1 deferred the DPC queue and instead
> wakes `sys_wait`ers *directly* from the timer-tick handler under the rank-1
> scheduler lock (see the decision log, 2026-06-08). The `phase-2/dpc` item
> builds the DPC queue described here; the existing timer-tick wakeup migrates
> onto it at that point, and device ISRs become its first external producers.

## Interrupts

### Routing: GSI → vector → ISR

On x86_64, the local APIC handles the per-CPU timer (Phase 1), but **device**
interrupts arrive through the **IOAPIC**, which must be located and configured
from the ACPI **MADT**. The IOAPIC routes a hardware *Global System Interrupt*
(GSI) to an IDT vector on a chosen CPU; the vector's stub enters a registered
kernel ISR. (`phase-2/ioapic`, building on `phase-2/acpi-tables`.)

Phase 2 uses **IOAPIC-routed, non-shared** interrupts — enough for the QEMU AHCI
controller. **Deferred:** MSI / MSI-X (message-signalled interrupts) and shared
PCI INTx (the "chain of handlers, each returns *mine* / *not mine*" model);
MSI/MSI-X are never shared.

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
forwards). **Deferred:** transparent **filter drivers** (encryption, compression,
logging inserted into a stack), IRP **cancellation**, and the 30-second
completion **timeout**.

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
buffers (AHCI command lists and PRDTs, for example). The `phase-2/dma-alloc`
item adds that allocation path. IOMMU-constrained DMA (so a userspace driver can
only touch memory it legitimately holds) is **deferred** with userspace drivers.

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
- `docs/history/decision-log.md` (2026-06-11) — the decisions recorded here.
- `docs/planning/implementation-plan.md` (Phase 2) — slice ordering and the
  prerequisite band.
