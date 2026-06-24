# DeviceNode and block-device naming

This document specifies the `DeviceNode` kernel object — the
architecture-independent representation of a discovered device — its resource
descriptor, and how block devices are named and resolved through the namespace.
Design context: [`drivers-and-irps.md`](../architecture/drivers-and-irps.md)
§ "Device discovery and enumeration".

**Status:** Pre-stabilization. Introduced with the storage slice (Phase 2
slice 5). PCI(e) is the only discovery source in Phase 2; partitions become
DeviceNodes in slice 6.

## The DeviceNode object

`DeviceNode` is a handle-accessible kernel object (`KObjectType::DeviceNode = 12`,
already reserved). One node represents one device: a PCI(e) function discovered by
ECAM enumeration (Phase 2), and — slice 6 — a partition layered over a block
device. A node is **architecture-independent**: on aarch64 the same nodes would
come from a Device Tree Blob rather than PCI/ACPI.

A node carries:

- an **identity** (what the device is),
- a **resource descriptor** (the MMIO/IO windows, the interrupt, the bus
  address) a driver needs to drive it, and
- a **device class** that decides which operations the node accepts and how it
  appears in the namespace.

The hardware-facing fields (BAR physical addresses, the raw GSI) are **not**
crossed by userspace — a `DeviceNode` handle held by userspace is an opaque
capability. A userspace driver would obtain register access only via
[`sys_device_map_mmio`](syscall-abi.md) (deferred with userspace drivers); the
in-kernel Tier 1 drivers read the descriptor directly.

### Device class

```rust
#[repr(u32)]
pub enum DeviceClass {
    Other = 0,    // discovered but unclaimed / no Nitrox driver
    Block = 1,    // accepts block Read/Write IoOps via sys_io_submit
}
```

A **block-class** node is the resource a block `sys_io_submit` targets (see
§ "Block devices"). Phase 2 defines only `Other` and `Block`; `Char`, `Net`, etc.
are added with their first driver. The class is set by the driver that claims the
node (AHCI marks its disk `Block`); an unclaimed node stays `Other`.

## Resource descriptor

The descriptor is the kernel-internal record a driver consumes. It is **not** an
ABI-hash input (no module crosses it in Phase 2) but its shape is fixed here.

```rust
#[repr(C)]
pub struct DeviceIdentity {
    pub vendor:   u16,   // PCI vendor id
    pub device:   u16,   // PCI device id
    pub class:    u8,    // PCI base class   (0x01 = mass storage)
    pub subclass: u8,    // PCI subclass     (0x06 = SATA/AHCI)
    pub prog_if:  u8,    // PCI programming interface (0x01 = AHCI 1.0)
    pub revision: u8,
}

#[repr(C)]
pub struct BarWindow {
    pub base:  u64,      // physical base of the window (0 = absent)
    pub size:  u64,      // bytes (0 = absent)
    pub kind:  u32,      // 0 = none, 1 = MMIO, 2 = port-I/O
    pub flags: u32,      // bit0 = 64-bit, bit1 = prefetchable
}

#[repr(C)]
pub struct InterruptSpec {
    pub gsi:      u32,           // resolved global system interrupt (0 until routed)
    pub trigger:  u32,           // arch::TriggerMode (filled at routing)
    pub polarity: u32,           // arch::Polarity (filled at routing)
    pub line:     u8,            // raw PCI interrupt line (config 0x3C)
    pub pin:      u8,            // raw PCI interrupt pin: 1..=4 = INTA..D, 0 = none
    pub present:  u8,            // 1 iff pin != 0
    pub _pad:     u8,
}

#[repr(C)]
pub struct ResourceDescriptor {
    pub identity:  DeviceIdentity,
    pub bars:      [BarWindow; 6],   // PCI has six BAR slots
    pub interrupt: InterruptSpec,
    pub seg:       u16,              // PCIe segment group
    pub bus:       u8,
    pub dev:       u8,
    pub func:      u8,
}
```

- **BARs** come from PCI config space (the ECAM window the kernel already maps):
  the kernel sizes each BAR (write-all-ones / read-back) and records its physical
  base, length, and kind. A 64-bit BAR consumes two adjacent slots; the upper
  slot is recorded as absent.
- **The interrupt** is the device's routed GSI. Phase 2 uses IOAPIC-routed,
  non-shared line interrupts: the kernel resolves the PCI interrupt pin to a GSI
  (from the legacy line in config space, refined by the ACPI `_PRT` when AML
  parsing exists — deferred; the QEMU AHCI line is sufficient meanwhile) and the
  AHCI driver routes it via [`arch::IrqRouter`](../../kernel/src/arch/irq_router.rs).
- A driver maps a BAR into **kernel** space (uncached, via the
  `PageFlags::NO_CACHE` path the arch paging layer already supports) to reach the
  controller's registers — there is no userspace MMIO mapping in Phase 2.

## Block devices

A block-class `DeviceNode` is the unit the async I/O core operates on:
[`sys_io_submit`](io-operation.md) on a block-`DeviceNode` handle issues a block
`Read`/`Write`. This is why no separate "BlockDevice" `KObjectType` exists — a
block device **is** a `DeviceNode`, whether it is a whole disk (AHCI) or a
partition (GPT, slice 6, layered as a second IRP stack frame over the disk).

A block node additionally exposes its geometry to the I/O core:

```rust
#[repr(C)]
pub struct BlockGeometry {
    pub logical_block_size: u32,   // bytes per LBA (512 or 4096)
    pub block_count:        u64,   // total addressable blocks
}
```

`IoOp.offset` and `IoOp.length` must be multiples of `logical_block_size`, and
`offset + length` must lie within `block_count * logical_block_size`, else the
operation completes `InvalidArgument` (see [`io-operation.md`](io-operation.md)).

## Naming and namespace resolution

Discovered devices are **dynamic** — count and identity are known only at
enumeration — but the in-kernel resource-server registry
([`kernel_server.rs`](../../kernel/src/object/kernel_server.rs)) is a static
`KernelServerId` enum. Bridging the two:

- A **single** `KernelServerId::BlockDevice` variant is added. Its server
  receives the lookup *suffix* (the path past the binding prefix) and consults a
  **kernel block-device registry** — a small table populated at enumeration,
  mapping a device name to its `DeviceNode` — returning a handle to the matching
  node, or `NotFound`.
- The supervisor (init, via `BIND_NAMESPACE`) binds `KernelServerId::BlockDevice`
  at **`/dev/blk`** in the root namespace at boot. A lookup of `/dev/blk0`
  resolves with suffix `0`; the server finds the disk registered under index `0`
  and hands back its node. (This reuses the existing `BindingTarget::KernelServer`
  longest-prefix resolution; one binding covers every disk.)
- The `/dev/blk` binding is created **unconditionally** — uniform with
  `/dev/entropy` / `/initramfs` — and the **registry carries liveness**: there is
  no per-server enable switch. `/dev/blk0` resolves iff a disk is registered there
  (a driver matched and populated it, below), else `NotFound`; if no block driver
  matched at all, the server is bound but inert. The only conditionally-live thing
  is the *driver*, enabled by device matching — see § "Discovery and driver
  matching". (Liveness model: `namespace-and-resource-servers.md` § "Liveness".)

### The naming scheme

- **Whole disks: `/dev/blk0`, `/dev/blk1`, …** — enumeration-order indices.
  Order is not stable across boots (it follows PCI/port discovery), so these are
  *enumeration* names, not *identity* names — fine for Phase 2's single QEMU disk
  and for low-level tools.
- **Content-stable names — `/dev/disk/by-partuuid/*`, `/dev/disk/by-partlabel/*`
  — are slice 6.** They are derived from GPT partition metadata, so they are
  order-independent and are what `init.toml` mount specs reference. The raw
  `/dev/blkN` whole-disk nodes are not what a manifest should name.

`Char`/other device families get their own prefix and registry when they arrive;
the `/dev/blk` registry is block-only.

### Rights at the binding

The `/dev/blk` binding is created **read-only** (`READ` without `WRITE`) in
Phase 2: `fs-server-ext4` mounts read-only (`docs/planning/implementation-plan.md`
slice 7), so a write `IoOp` is rejected at the lookup-rights gate before any IRP
is built. Read-write block access lands with RW filesystems (Phase 3).

## Discovery and driver matching (Phase 2)

1. At boot the kernel reads the ACPI MCFG (already parsed) to locate the PCIe
   ECAM windows ([`arch::platform::pcie_ecam_regions`](../../kernel/src/arch/platform.rs)),
   walks every bus/device/function, and builds a `DeviceNode` per present
   function with its `ResourceDescriptor`.
2. A built-in **match table** maps identity → Tier 1 driver
   (`class 0x01 / subclass 0x06 / prog_if 0x01` → AHCI). The matched driver
   claims the node, marks any disks it finds `Block`, registers them in the block
   registry, and routes their interrupt.
3. The supervisor binds `/dev/blk` so userspace can resolve the disks.

**Drivers, not servers, are what's conditional.** AHCI and NVMe are *drivers*
(`drivers-and-irps.md` § "Three concepts, kept distinct"), not Kernel Servers —
there is one block *server* (`BlockDevice`) regardless of which controller a disk
came from. A driver is enabled purely by **matching**: with both compiled in (via
their Tier 1 Cargo features), enumeration on an AHCI-only machine produces an AHCI
node the AHCI driver claims, while the NVMe driver's predicate never fires and its
code stays cold — hardware presence is the enable, no flag needed. Both feed the
same registry + `/dev/blk`, so the client sees one block namespace either way. The
driver-to-node matching graduates to a userspace **device manager** with Tier 2
(deferred); see `namespace-and-resource-servers.md` § "Liveness".

The userspace **driver manager** (matching nodes to Tier 2 modules, handing a
driver process a `Handle<DeviceNode>`) is deferred with the module loader
(`deferred-decisions.md`).

## Deferred

- `/dev/disk/by-partuuid/*` and partition DeviceNodes (slice 6).
- `sys_device_map_mmio` / userspace drivers / IOMMU (with Tier 2).
- A device-enumeration syscall (`ENUMERATE`) and a `/dev` directory listing —
  there is no consumer yet (`deferred-decisions.md` § "Resource servers").
- ACPI `_PRT`-based interrupt routing (needs AML; the legacy line suffices for
  QEMU AHCI).
- Non-block device classes (`Char`, `Net`, …).

## Where to read more

- [Drivers and IRPs](../architecture/drivers-and-irps.md)
- [IoOp](io-operation.md) / [IRP layout](irp-layout.md)
- [Namespace and resource servers](../architecture/namespace-and-resource-servers.md)
