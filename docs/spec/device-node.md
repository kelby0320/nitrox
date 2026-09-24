# DeviceNode and block-device naming

This document specifies the `DeviceNode` kernel object — the
architecture-independent representation of a discovered device — its resource
descriptor, and how block devices are named and resolved through the namespace.
Design context: [`drivers-and-irps.md`](../architecture/drivers-and-irps.md)
§ "Device discovery and enumeration".

**Status:** Pre-stabilization. Introduced with the storage slice (Phase 2
slice 5). PCI(e) is the only discovery source in Phase 2; partitions became
DeviceNodes in slice 6, and `Char` nodes arrived with the console and the i8042.

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
    Other = 0,    // a PCI function, claimed or not
    Block = 1,    // accepts block Read/Write IoOps via sys_io_submit
    Char = 2,     // accepts byte-stream Read IoOps: the console, the i8042's devices
}
```

A **block-class** node is the resource a block `sys_io_submit` targets (see
§ "Block devices"). A driver publishes its own node with its class — AHCI a `Block` node per
disk, the console and the i8042 a `Char` node each — and a PCI function stays `Other` whether a
driver claims it or not. `Net` and others arrive with their first driver. The class is coarse:
the console, the keyboard and the mouse are all `Char`, which is why the registry adds a
**kind** (§ "The registry").

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
- **The interrupt** descriptor records the device's legacy line and pin, and
  since Phase 5 Part A that is the **fallback** rather than the path. A driver
  prefers **MSI** wherever the function advertises the capability: the device is
  handed an address and a value and raises the interrupt by writing them itself,
  so no GSI is resolved and nothing is routed. Where there is no MSI capability,
  the kernel resolves the interrupt pin to a GSI — from the legacy line in config
  space, refined by the ACPI `_PRT` when AML parsing exists (deferred) — and
  routes it via [`arch::IrqRouter`](../../kernel/src/arch/irq_router.rs). Both
  installs are reached through
  [`arch::IrqInstall`](../../kernel/src/arch/irq_install.rs). **`InterruptSpec`
  is unchanged either way**: its `gsi`/`trigger`/`polarity` stay zero until
  something routes a pin, and on the MSI path nothing ever does.
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

**`length` is also bounded by what one command can describe** — the device's
scatter-gather capacity, counted in the pages the buffer range touches rather than
in bytes, and refused **synchronously** rather than completed. That limit is not in
`BlockGeometry`: it is a property of the driver's command structures rather than of
the medium, and a device node does not publish it. See
[`io-operation.md`](io-operation.md) § `length` for the rule and the AHCI figure.

## Naming and namespace resolution

Discovered devices are **dynamic** — count and identity are known only at
enumeration — but the in-kernel resource-server registry
([`kernel_server.rs`](../../kernel/src/object/kernel_server.rs)) is a static
`KernelServerId` enum. Bridging the two:

- A **single** `KernelServerId::BlockDevice` variant is added. Its server
  receives the lookup *suffix* (the path past the binding prefix) and consults the
  **kernel device table** ([`device.rs`](../../kernel/src/device.rs)), returning the block node
  whose **served index** is the suffix, or `NotFound`. A block node's served index is the number
  of block nodes published before it, recorded when it is registered, and it is the same field
  `/dev/registry` reports (§ "The registry").
- The supervisor (init, via `BIND_NAMESPACE`) binds `KernelServerId::BlockDevice`
  at **`/dev/blk`** in the root namespace at boot. A lookup of `/dev/blk/0`
  resolves with suffix `0`; the server finds the disk registered under index `0`
  and hands back its node. (This reuses the existing `BindingTarget::KernelServer`
  longest-prefix resolution; one binding covers every disk.)
- The `/dev/blk` binding is created **unconditionally** — uniform with
  `/dev/entropy` / `/initramfs` — and the **registry carries liveness**: there is
  no per-server enable switch. `/dev/blk/0` resolves iff a disk is registered there
  (a driver matched and populated it, below), else `NotFound`; if no block driver
  matched at all, the server is bound but inert. The only conditionally-live thing
  is the *driver*, enabled by device matching — see § "Discovery and driver
  matching". (Liveness model: `namespace-and-resource-servers.md` § "Liveness".)

### The naming scheme

- **Block devices: `/dev/blk/0`, `/dev/blk/1`, …** — enumeration-order indices.
  Namespace prefix matching is **component-boundary** (`/dev/blk` covers
  `/dev/blk/0` with suffix `0`, *not* `/dev/blk0`), so the index is a path
  component under the `/dev/blk` subtree. Order is not stable across boots (it
  follows PCI/port discovery), so these are *enumeration* names, not *identity*
  names.
- **Not "whole disks", which this document said until 2026-09-16.** The registry holds every
  block node: whole disks, the **partitions** the GPT scan publishes on them, and memory published
  as a disk (a bootloader module). An index alone therefore says nothing about what it names, and a
  program that assumed otherwise would write a partition table over a mounted filesystem.
- **`/dev/blk/<n>/info`** answers that, as `/dev/framebuffer/info` does for the display: a
  read-only `MemoryObject` holding one `BlockDeviceInfo` (`kernel/src/libkern/block.rs`,
  mirrored in `userspace/libkern/src/abi.rs`) — the device's **kind** (disk, partition, RAM disk
  or unknown), its logical block size and block count, and a **name** for a person to recognise it
  by: a disk's model and serial, a partition's label, a module's path. `sys_handle_stat` reports
  the same capacity as the handle's `size`.
- **Content-stable names — `/dev/disk/by-partuuid/*`, `/dev/disk/by-partlabel/*`**, since
  slice 6. They are derived from GPT partition metadata, so they are order-independent and are
  what `init.toml` mount specs reference. The raw `/dev/blk/N` nodes are not what a manifest
  should name.

`Char` devices now have both shapes: `/dev/console` is a **leaf** binding to the single
serial console, and `/dev/input/raw/<n>` is the first **indexed char registry** — a subtree
binding whose suffix indexes the nodes a driver published, served exactly as `/dev/blk/<n>`
serves block devices. Other device families get their own prefix and registry when they
arrive; the `/dev/blk` registry remains block-only.

### Rights at the binding

The `/dev/blk` binding grants **`READ | WRITE | MAP_READ`** plus the generic band. It was
read-only in Phase 2, when `fs-server-ext4` mounted read-only and a write `IoOp` was meant to be
rejected at the lookup-rights gate; `WRITE` came with read-write filesystems in Phase 3 and this
section said otherwise until 2026-09-16. `MAP_READ` is for the `<n>/info` leaf, which answers with
a `MemoryObject`: a lookup attenuates to the binding's rights, so without it the record resolves
and cannot be read. It means nothing on a `DeviceNode`, which is not mappable.

**Reaching the binding at all is authority.** It lives in init's root namespace and
`libsession::build_namespace` deliberately omits it, so an ordinary session cannot resolve
`/dev/blk` however its rights read — see [`administration.md`](../planning/administration.md).

## The registry: `/dev/registry`

The whole device table, read from userspace — every `DeviceNode` the kernel has: the PCI functions
enumeration found, then what drivers published after them (disks, partitions, the RAM disk, the
console, the keyboard and the mouse), in that order. `KernelServerId::Registry`, bound by the
kernel in **the root namespace only**, with `/dev/blk`'s rights. (Administration Part B.)

- **`/dev/registry`** — a fresh read-only `MemoryObject`: a `RegistryHeader`, then `count`
  `DeviceRecord`s, then zero padding to the page (`kernel/src/libkern/device.rs`, mirrored in
  `userspace/libkern/src/device.rs`, whose `records` is the reader).
- **`/dev/registry/<id>`** — node `id` itself, the handle `/dev/blk/<n>` or
  `/dev/input/raw/<n>` would give for the same device.

```rust
#[repr(C)]
pub struct RegistryHeader {   // 16 bytes
    pub magic: u32,           // REGISTRY_MAGIC, "DREG" little-endian
    pub version: u32,         // REGISTRY_VERSION = 1
    pub count: u32,           // how many records follow: THE LENGTH
    pub record_size: u32,     // size_of::<DeviceRecord>() = 144
}

#[repr(C)]
pub struct DeviceRecord {     // 144 bytes, align 8
    pub id: u32,              // its place in the table: /dev/registry/<id>
    pub class: u32,           // DeviceClass
    pub kind: u32,            // DeviceKind, below
    pub served: u32,          // the <n> of /dev/blk/<n> or /dev/input/raw/<n>, else NOT_SERVED
    pub parent: u32,          // the id it belongs to — a partition's disk, a disk's controller — else NO_PARENT
    pub outcome: u32,         // for a PCI function: OUTCOME_NONE / _CLAIMED / _DECLINED
    pub vendor: u16, pub device: u16,                       // 0xFFFF vendor: not a PCI function
    pub pci_class: u8, pub subclass: u8, pub prog_if: u8, pub revision: u8,
    pub seg: u16, pub bus: u8, pub dev: u8, pub func: u8, pub _pad: [u8; 3],
    pub logical_block_size: u32,                            // block devices; else 0
    pub name_len: u32,
    pub block_count: u64,                                   // block devices; else 0
    pub driver: [u8; 16],                                   // the publishing or claiming driver
    pub name: [u8; 72],                                     // model and serial, label, module path, or "keyboard"
}
```

`DeviceKind`: `Unknown` 0, `PciFunction` 1, `Disk` 2, `Partition` 3, `RamDisk` 4, `Keyboard` 5,
`Mouse` 6, `Console` 7. A value a reader does not name reads as `Unknown`.

**The count is the length, not the object's size.** The object is page-rounded, so its tail is
zeros, which a reader dividing the size would take for records of class `Other` and kind
`Unknown`. A reader must also refuse a header whose count the bytes cannot hold, and a magic,
version or record size it does not know.

**A record's served index and its path are one field.** `/dev/blk/<n>` resolves the block node
whose served index is `n`, and `/dev/input/raw/<n>` the keyboard or mouse whose served index is
`n` — the keyboard 0 and the mouse 1, the i8042 driver's own numbering, **not** a count within
`Char`, where the console registered first. The console and PCI functions are served at no index.

**Ids are stable for the life of a boot** and never reused, because the table only grows.

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
driver process a `Handle<DeviceNode>`) is Phase 6's, and extends the device
manager the administration phase builds (`docs/planning/phase-6-usb.md`).

## Deferred

- `sys_device_map_mmio` / userspace drivers / IOMMU (with Tier 2).
- ACPI `_PRT`-based interrupt routing (needs AML; AHCI takes MSI, with the
  IOAPIC-routed line as its fallback).
- Device classes beyond `Block`, `Char` and `Other` (`Net`, …).

## Where to read more

- [Drivers and IRPs](../architecture/drivers-and-irps.md)
- [IoOp](io-operation.md) / [IRP layout](irp-layout.md)
- [Namespace and resource servers](../architecture/namespace-and-resource-servers.md)
