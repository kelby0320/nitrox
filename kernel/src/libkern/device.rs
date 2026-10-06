//! What `/dev/registry` serves: the kernel's device table, as records (administration Part B).
//!
//! **Why a table and not a directory.** Every userspace enumeration of devices used to be a probe
//! — look up `/dev/blk/0`, `1`, … until one is missing — because a kernel server answers lookups
//! and nothing else, and the probe says nothing about *what* each device is. The device manager
//! needs every node, its kind, and which path serves it, in one read: that is a snapshot of the
//! table, and a snapshot is what `/dev/log` already is.
//!
//! **The header's count is the length, not the object's size.** A memory object is page-rounded,
//! so a reader that divided its size by [`DeviceRecord`]'s would read the zero padding as devices
//! — class `Other`, kind `Unknown` — and a comparison that looked only at block devices would not
//! notice them.
//!
//! `#[repr(C)]` and mirrored in `userspace/libkern/src/device.rs`: the layout asserts on both
//! sides keep the structs in step, and `cargo xtask abi-sync-check` the discriminants and
//! constants. Specified in `docs/spec/device-node.md` § *The registry*.

use core::mem::{align_of, offset_of, size_of};

use super::block::MAX_DEVICE_NAME;

/// What a node is, as the registry reports it — finer than its `DeviceClass`, which calls the
/// console, the keyboard and the mouse all `Char`.
#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DeviceKind {
    /// Registered without saying — a block driver that published no kind.
    #[default]
    Unknown = 0,
    /// A PCI(e) function `device::init` enumerated, claimed by a driver or not.
    PciFunction = 1,
    /// A whole disk.
    Disk = 2,
    /// One partition of a disk.
    Partition = 3,
    /// Memory published as a disk: a bootloader module.
    RamDisk = 4,
    /// A keyboard.
    Keyboard = 5,
    /// A mouse, or anything that reports like one — the laptop's trackpad arrives on the i8042's
    /// aux port.
    Mouse = 6,
    /// The serial console.
    Console = 7,
    /// A USB device on a root port of the xHCI controller, its parent (Phase 6 Part A.3). Its
    /// record's `vendor` and `device` are its USB IDs, its class fields its class triple, and its
    /// `port` and `speed` say where and how fast it is attached.
    UsbDevice = 8,
}

impl DeviceKind {
    /// The discriminant, for the wire struct.
    pub const fn as_u32(self) -> u32 {
        self as u32
    }

    /// Decode a discriminant. A value this kernel does not name reads as `Unknown`, the safe
    /// answer for a reader that refuses what it does not recognise.
    pub const fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::PciFunction,
            2 => Self::Disk,
            3 => Self::Partition,
            4 => Self::RamDisk,
            5 => Self::Keyboard,
            6 => Self::Mouse,
            7 => Self::Console,
            8 => Self::UsbDevice,
            _ => Self::Unknown,
        }
    }
}

/// The first four bytes of a snapshot, `"DREG"` read as a little-endian `u32`.
pub const REGISTRY_MAGIC: u32 = 0x4745_5244;
/// The snapshot layout this kernel writes: 2 since Phase 6 Part C, whose header carries the table's
/// generation.
pub const REGISTRY_VERSION: u32 = 2;
/// [`DeviceRecord::served`] for a node no indexed path serves (a PCI function, the console).
pub const NOT_SERVED: u32 = 0xFFFF_FFFF;
/// [`DeviceRecord::parent`] for a node that belongs to nothing else.
pub const NO_PARENT: u32 = 0xFFFF_FFFF;
/// [`DeviceRecord::outcome`]: no driver reported anything about this node.
pub const OUTCOME_NONE: u32 = 0;
/// [`DeviceRecord::outcome`]: a driver took the function.
pub const OUTCOME_CLAIMED: u32 = 1;
/// [`DeviceRecord::outcome`]: a driver matched the function and gave it up.
pub const OUTCOME_DECLINED: u32 = 2;
/// Longest driver name served, in bytes.
pub const MAX_DRIVER_NAME: usize = 16;
/// [`DeviceRecord::flags`]: **the device has departed** (Phase 6 Part C). Its record keeps its id,
/// its fields and its served index, which no later device takes; its paths answer `NotFound`. A
/// departed device's children — a USB device's keyboard, a disk's partitions — are departed too.
pub const DEPARTED: u8 = 0x01;
/// [`DeviceRecord::flags`]: **the disk the machine started from** (Phase 6 Part D) — its GPT's GUID
/// is the one Limine loaded the modules from. On a live boot it is the stick; on an installed
/// machine the internal disk, whose root is `init`'s.
pub const BOOT: u8 = 0x02;

/// The start of a snapshot: then `count` [`DeviceRecord`]s, then zero padding to the page.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RegistryHeader {
    /// [`REGISTRY_MAGIC`].
    pub magic: u32,
    /// [`REGISTRY_VERSION`].
    pub version: u32,
    /// How many records follow. **The length** — see the module docs.
    pub count: u32,
    /// `size_of::<DeviceRecord>()` as the kernel wrote it, so a reader built against another
    /// layout refuses rather than misreads.
    pub record_size: u32,
    /// **The table's generation** (Phase 6 Part C): bumped once by every change — a device
    /// registered, or one departed with its children — so two snapshots with the same generation
    /// say the same thing. `/dev/registry/changes` answers in the same terms.
    pub generation: u64,
}

const _: () = assert!(size_of::<RegistryHeader>() == 24);
const _: () = assert!(align_of::<RegistryHeader>() == 8);
const _: () = assert!(offset_of!(RegistryHeader, magic) == 0);
const _: () = assert!(offset_of!(RegistryHeader, version) == 4);
const _: () = assert!(offset_of!(RegistryHeader, count) == 8);
const _: () = assert!(offset_of!(RegistryHeader, record_size) == 12);
const _: () = assert!(offset_of!(RegistryHeader, generation) == 16);

/// One node of the device table.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceRecord {
    /// Its place in the table, stable for the life of the boot: `/dev/registry/<id>` is this node.
    pub id: u32,
    /// Its `DeviceClass` discriminant.
    pub class: u32,
    /// Its [`DeviceKind`] discriminant.
    pub kind: u32,
    /// **The index its path serves it at** — the `<n>` of `/dev/blk/<n>` or `/dev/input/raw/<n>`
    /// — or [`NOT_SERVED`]. The same field those servers resolve through, so it cannot disagree
    /// with them.
    pub served: u32,
    /// The `id` of the node it belongs to — a partition's disk, a disk's or a USB device's
    /// controller — or [`NO_PARENT`].
    pub parent: u32,
    /// For a PCI function, what its driver did with it: [`OUTCOME_NONE`], [`OUTCOME_CLAIMED`]
    /// or [`OUTCOME_DECLINED`].
    pub outcome: u32,
    /// **Which IDs these are depends on [`kind`](Self::kind)**: a PCI function's vendor id, a USB
    /// device's `idVendor` (Phase 6 Part A.3), or `0xFFFF` for a node that is neither. A vendor
    /// other than `0xFFFF` does not mean a PCI function — read the kind.
    pub vendor: u16,
    /// A PCI function's device id, or a USB device's `idProduct`.
    pub device: u16,
    /// A PCI function's base class, or a USB device's class (its first interface's when its own
    /// is zero).
    pub pci_class: u8,
    /// The subclass, as for [`pci_class`](Self::pci_class).
    pub subclass: u8,
    /// A PCI function's programming interface, or a USB device's protocol.
    pub prog_if: u8,
    /// A PCI function's revision; zero for a USB device.
    pub revision: u8,
    /// PCIe segment group; zero for a node that is not a PCI function.
    pub seg: u16,
    /// PCI bus; zero for a node that is not a PCI function.
    pub bus: u8,
    /// PCI device; zero for a node that is not a PCI function.
    pub dev: u8,
    /// PCI function; zero for a node that is not a PCI function.
    pub func: u8,
    /// For a USB device, the root port it is on, numbered from 1; zero for every other kind
    /// (Phase 6 Part A.3, from bytes that were reserved — the layout and the version are unchanged).
    pub port: u8,
    /// For a USB device, its speed: 1 full, 2 low, 3 high, 4 SuperSpeed, 5 SuperSpeedPlus (the
    /// xHCI's default speed IDs); zero for every other kind.
    pub speed: u8,
    /// What has happened to it, and what it is: [`DEPARTED`] (Phase 6 Part C, from the byte that
    /// was reserved — the layout is unchanged) and [`BOOT`] (Part D).
    pub flags: u8,
    /// Bytes per logical block, for a block device; zero otherwise.
    pub logical_block_size: u32,
    /// Bytes of [`name`](Self::name) that are meaningful.
    pub name_len: u32,
    /// Logical blocks, for a block device; zero otherwise.
    pub block_count: u64,
    /// The driver that published the node or took the function, NUL-padded.
    pub driver: [u8; MAX_DRIVER_NAME],
    /// What to call it: a disk's model and serial, a partition's label, a module's path, a USB
    /// device's product and serial (or `vvvv:pppp` when it has no strings).
    pub name: [u8; MAX_DEVICE_NAME],
}

const _: () = assert!(size_of::<DeviceRecord>() == 144);
const _: () = assert!(align_of::<DeviceRecord>() == 8);
const _: () = assert!(offset_of!(DeviceRecord, id) == 0);
const _: () = assert!(offset_of!(DeviceRecord, class) == 4);
const _: () = assert!(offset_of!(DeviceRecord, kind) == 8);
const _: () = assert!(offset_of!(DeviceRecord, served) == 12);
const _: () = assert!(offset_of!(DeviceRecord, parent) == 16);
const _: () = assert!(offset_of!(DeviceRecord, outcome) == 20);
const _: () = assert!(offset_of!(DeviceRecord, vendor) == 24);
const _: () = assert!(offset_of!(DeviceRecord, device) == 26);
const _: () = assert!(offset_of!(DeviceRecord, pci_class) == 28);
const _: () = assert!(offset_of!(DeviceRecord, seg) == 32);
const _: () = assert!(offset_of!(DeviceRecord, bus) == 34);
const _: () = assert!(offset_of!(DeviceRecord, func) == 36);
const _: () = assert!(offset_of!(DeviceRecord, port) == 37);
const _: () = assert!(offset_of!(DeviceRecord, speed) == 38);
const _: () = assert!(offset_of!(DeviceRecord, logical_block_size) == 40);
const _: () = assert!(offset_of!(DeviceRecord, name_len) == 44);
const _: () = assert!(offset_of!(DeviceRecord, block_count) == 48);
const _: () = assert!(offset_of!(DeviceRecord, driver) == 56);
const _: () = assert!(offset_of!(DeviceRecord, name) == 72);

impl Default for DeviceRecord {
    fn default() -> Self {
        Self {
            id: 0,
            class: 0,
            kind: DeviceKind::Unknown.as_u32(),
            served: NOT_SERVED,
            parent: NO_PARENT,
            outcome: OUTCOME_NONE,
            vendor: 0xFFFF,
            device: 0,
            pci_class: 0,
            subclass: 0,
            prog_if: 0,
            revision: 0,
            seg: 0,
            bus: 0,
            dev: 0,
            func: 0,
            port: 0,
            speed: 0,
            flags: 0,
            logical_block_size: 0,
            name_len: 0,
            block_count: 0,
            driver: [0; MAX_DRIVER_NAME],
            name: [0; MAX_DEVICE_NAME],
        }
    }
}

impl RegistryHeader {
    /// The header's bytes, as they go on the wire.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `RegistryHeader` is `repr(C)`, four `u32`s then a `u64` at offset 16, so no
        // padding, fully initialised.
        unsafe { core::slice::from_raw_parts((self as *const Self).cast::<u8>(), size_of::<Self>()) }
    }
}

impl DeviceRecord {
    /// The record's bytes, as they go on the wire.
    pub fn as_bytes(&self) -> &[u8] {
        // SAFETY: `DeviceRecord` is `repr(C)` and its `flags` byte leaves no implicit padding
        // (the offset asserts above account for every byte), so every byte is initialised.
        unsafe { core::slice::from_raw_parts((self as *const Self).cast::<u8>(), size_of::<Self>()) }
    }
}
