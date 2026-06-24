//! The [`DeviceNode`] kernel object — the architecture-independent representation
//! of a discovered device.
//!
//! One node represents one device: in Phase 2 a PCI(e) function found by ECAM
//! enumeration ([`crate::pci`]); later a partition layered over a block device.
//! A node carries an **identity** (what it is), a **resource descriptor** (the
//! MMIO windows, interrupt, and bus address a driver needs), a **device class**
//! (what operations it accepts), and — for block devices — its **geometry**.
//!
//! A `DeviceNode` is a handle-accessible object (`KObjectType::DeviceNode`), but
//! its hardware-facing fields never cross to userspace: a userspace-held handle
//! is an opaque capability. The block I/O core ([`crate::pci`] /
//! `sys_io_submit`, later parts) reads block-class nodes; in-kernel Tier 1
//! drivers read the descriptor directly. Normative shapes:
//! `docs/spec/device-node.md`.

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox};
use crate::object::header::KObjectHeader;

/// What a device is, from its PCI configuration header.
///
/// `#[repr(C)]`, 8 bytes. Kernel-internal (not an ABI-hash input); the field
/// set mirrors `docs/spec/device-node.md`.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct DeviceIdentity {
    /// PCI vendor id (`0xFFFF` ⇒ no device present).
    pub vendor: u16,
    /// PCI device id.
    pub device: u16,
    /// PCI base class (`0x01` = mass storage).
    pub class: u8,
    /// PCI subclass (`0x06` = SATA).
    pub subclass: u8,
    /// PCI programming interface (`0x01` = AHCI 1.0 under class/subclass above).
    pub prog_if: u8,
    /// PCI revision id.
    pub revision: u8,
}

/// `BarWindow::kind` — no window in this BAR slot.
pub const BAR_NONE: u32 = 0;
/// `BarWindow::kind` — a memory-mapped (MMIO) window.
pub const BAR_MMIO: u32 = 1;
/// `BarWindow::kind` — a port-I/O window.
pub const BAR_IO: u32 = 2;

/// `BarWindow::flags` bit: the BAR is 64-bit (consumed two PCI BAR slots).
pub const BAR_FLAG_64: u32 = 1 << 0;
/// `BarWindow::flags` bit: the BAR is prefetchable.
pub const BAR_FLAG_PREFETCH: u32 = 1 << 1;

/// One PCI base address register, decoded and sized.
///
/// `#[repr(C)]`, 24 bytes. A 64-bit BAR occupies one `BarWindow` (its high slot
/// is recorded as [`BAR_NONE`]).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BarWindow {
    /// Physical base of the window (`0` if absent).
    pub base: u64,
    /// Size of the window in bytes (`0` if absent).
    pub size: u64,
    /// One of [`BAR_NONE`] / [`BAR_MMIO`] / [`BAR_IO`].
    pub kind: u32,
    /// [`BAR_FLAG_64`] | [`BAR_FLAG_PREFETCH`].
    pub flags: u32,
}

impl BarWindow {
    /// An absent BAR slot, for array initialisation.
    pub const ZERO: BarWindow = BarWindow {
        base: 0,
        size: 0,
        kind: BAR_NONE,
        flags: 0,
    };
}

/// The device's interrupt, as far as enumeration can determine it.
///
/// `#[repr(C)]`, 16 bytes. At enumeration only the raw PCI `line`/`pin` are
/// known; `gsi`/`trigger`/`polarity` are resolved when a driver routes the
/// interrupt (the AHCI part). `pin` of `0` means the function asserts no legacy
/// interrupt.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct InterruptSpec {
    /// Resolved global system interrupt — `0` and `present == 0` until a driver
    /// routes the pin (`docs/spec/device-node.md`; ACPI `_PRT` routing deferred).
    pub gsi: u32,
    /// `arch::TriggerMode` value, filled at routing (`0` until then).
    pub trigger: u32,
    /// `arch::Polarity` value, filled at routing (`0` until then).
    pub polarity: u32,
    /// Raw PCI interrupt line (config `0x3C`).
    pub line: u8,
    /// Raw PCI interrupt pin: `1..=4` = INTA..INTD, `0` = none (config `0x3D`).
    pub pin: u8,
    /// `1` iff the function asserts a legacy interrupt (`pin != 0`).
    pub present: u8,
    /// Padding to 16 bytes.
    pub _pad: u8,
}

impl InterruptSpec {
    /// A function with no interrupt assigned.
    pub const NONE: InterruptSpec = InterruptSpec {
        gsi: 0,
        trigger: 0,
        polarity: 0,
        line: 0,
        pin: 0,
        present: 0,
        _pad: 0,
    };
}

/// The full resource descriptor a driver consumes to drive a device.
///
/// `#[repr(C)]`. Kernel-internal (not crossed by modules in Phase 2; not an
/// ABI-hash input).
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ResourceDescriptor {
    /// What the device is.
    pub identity: DeviceIdentity,
    /// The six PCI BAR slots (a 64-bit BAR fills one slot; its high slot is
    /// [`BAR_NONE`]).
    pub bars: [BarWindow; 6],
    /// The device's interrupt.
    pub interrupt: InterruptSpec,
    /// PCIe segment group.
    pub seg: u16,
    /// PCI bus number.
    pub bus: u8,
    /// PCI device number (`0..32`).
    pub dev: u8,
    /// PCI function number (`0..8`).
    pub func: u8,
    /// Padding.
    pub _pad: [u8; 3],
}

/// What a device node is and which operations it accepts.
///
/// `#[repr(u32)]`. A `Block` node accepts block read/write through the I/O core;
/// `Other` is discovered-but-unclaimed. Other classes (`Char`, `Net`, …) arrive
/// with their first driver.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DeviceClass {
    /// Discovered but unclaimed / no Nitrox driver.
    Other = 0,
    /// A block device: accepts block `Read`/`Write` I/O operations.
    Block = 1,
}

impl DeviceClass {
    /// Decode a `u32` discriminant, or `None` if unrecognised.
    pub const fn from_u32(v: u32) -> Option<Self> {
        match v {
            0 => Some(Self::Other),
            1 => Some(Self::Block),
            _ => None,
        }
    }
}

/// A block device's addressable geometry. Meaningful only for [`DeviceClass::Block`]
/// nodes; zeroed otherwise. Set by the driver that claims the node.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct BlockGeometry {
    /// Bytes per logical block (512 or 4096); `0` if not a block device.
    pub logical_block_size: u32,
    /// Total addressable logical blocks.
    pub block_count: u64,
}

impl BlockGeometry {
    /// Zero geometry, for a non-block node.
    pub const ZERO: BlockGeometry = BlockGeometry {
        logical_block_size: 0,
        block_count: 0,
    };
}

/// A discovered device.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see [`crate::object::header`].
#[repr(C)]
pub struct DeviceNode {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`DeviceNode::MAGIC`].
    magic: u64,
    /// Device class (mutated to `Block` by a claiming driver).
    class: DeviceClass,
    /// The hardware resource descriptor (immutable after enumeration).
    descriptor: ResourceDescriptor,
    /// Block geometry (zeroed until a block driver claims the node).
    geometry: BlockGeometry,
}

impl DeviceNode {
    /// Sentinel written into [`DeviceNode::magic`] at construction.
    pub const MAGIC: u64 = 0x4465_7669_6365_4e21; // "DeviceN!"

    /// Allocate a device node with a refcount of one.
    pub fn try_new(
        class: DeviceClass,
        descriptor: ResourceDescriptor,
        geometry: BlockGeometry,
    ) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::DeviceNode),
            magic: Self::MAGIC,
            class,
            descriptor,
            geometry,
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// The device's class.
    pub fn class(&self) -> DeviceClass {
        self.class
    }

    /// The device's resource descriptor.
    pub fn descriptor(&self) -> &ResourceDescriptor {
        &self.descriptor
    }

    /// The device's block geometry (zeroed for non-block nodes).
    pub fn geometry(&self) -> BlockGeometry {
        self.geometry
    }
}

// No `Drop`: a `DeviceNode` owns no out-of-line resources (the descriptor is
// inline, the MMIO windows are not owned), so the `KBox` drop run by
// `dispatch_destroy` suffices.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::ObjectRef;
    use crate::object::header::test_probe;

    fn sample_descriptor() -> ResourceDescriptor {
        ResourceDescriptor {
            identity: DeviceIdentity {
                vendor: 0x8086,
                device: 0x2922,
                class: 0x01,
                subclass: 0x06,
                prog_if: 0x01,
                revision: 0x02,
            },
            bars: [BarWindow::ZERO; 6],
            interrupt: InterruptSpec::NONE,
            seg: 0,
            bus: 0,
            dev: 0x1f,
            func: 2,
            _pad: [0; 3],
        }
    }

    #[test]
    fn try_new_has_magic_and_fields() {
        init_global_heap();
        let n = DeviceNode::try_new(DeviceClass::Other, sample_descriptor(), BlockGeometry::ZERO)
            .unwrap();
        assert!(n.magic_ok());
        assert_eq!(n.class(), DeviceClass::Other);
        assert_eq!(n.descriptor().identity.vendor, 0x8086);
        assert_eq!(n.descriptor().dev, 0x1f);
    }

    #[test]
    fn device_class_round_trips() {
        assert_eq!(DeviceClass::from_u32(0), Some(DeviceClass::Other));
        assert_eq!(DeviceClass::from_u32(1), Some(DeviceClass::Block));
        assert_eq!(DeviceClass::from_u32(2), None);
    }

    #[test]
    fn dropping_last_objectref_routes_through_dispatch_destroy() {
        init_global_heap();
        test_probe::reset();
        // SAFETY: `into_raw` yields the single creation reference; adopt it as
        // the path a real handle release takes, then drop it.
        let r = unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(
                    DeviceNode::try_new(DeviceClass::Other, sample_descriptor(), BlockGeometry::ZERO)
                        .unwrap(),
                )
                .as_ptr() as *mut (),
                KObjectType::DeviceNode,
            )
        };
        assert_eq!(test_probe::device_node_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::device_node_destroys(), 1);
    }
}
