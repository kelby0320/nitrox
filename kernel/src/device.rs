//! The kernel device table — the set of [`DeviceNode`]s discovered at boot.
//!
//! Architecture-independent: today the table is populated by PCI(e) enumeration
//! ([`crate::pci`]); on aarch64 it would be populated from a Device Tree Blob.
//! Each entry is an owning reference, so discovered devices live for the
//! kernel's lifetime. Later parts read this table — driver matching (the AHCI
//! part) iterates it; the block resource server (Part 4) resolves block-class
//! nodes through it.
//!
//! [`DeviceNode`]: crate::object::DeviceNode

use crate::libkern::{KVec, SpinLock};
use crate::object::ObjectRef;
use crate::object::device_node::{DeviceClass, DeviceNode};

/// The discovered devices. Written once at boot by [`init`]; read thereafter.
/// Lock rank: a leaf — never held across another lock.
static DEVICES: SpinLock<KVec<ObjectRef>> = SpinLock::new(KVec::new());

/// Enumerate hardware and populate the device table. Boot-time; call once, after
/// the allocators, the HHDM, the kvmap, and `arch::Platform::init` are up.
pub fn init() {
    let nodes = crate::pci::enumerate();
    let count = nodes.len();
    *DEVICES.lock() = nodes;
    crate::kprintln!("device: {} node(s) registered", count);
}

/// Number of devices in the table.
pub fn count() -> usize {
    DEVICES.lock().len()
}

/// A snapshot of the device table: a cloned owning reference per device. Taken
/// under the lock and returned, so a caller (driver matching) can iterate and
/// allocate **without** holding the device lock across a lock-ordering boundary.
/// The table keeps its own references; the caller drops the snapshot when done.
pub fn snapshot() -> KVec<ObjectRef> {
    let table = DEVICES.lock();
    let mut out: KVec<ObjectRef> = KVec::new();
    if out.try_reserve(table.len()).is_err() {
        return KVec::new();
    }
    for node in table.iter() {
        out.try_push(node.clone()).expect("within reserved capacity");
    }
    out
}

/// Append an already-built device node (e.g. a disk a driver discovered) to the
/// table. The table takes ownership of `node`.
pub fn register(node: ObjectRef) {
    let mut table = DEVICES.lock();
    if table.try_push(node).is_err() {
        crate::kprintln!("device: table full; dropping a registered node");
    }
}

/// The `index`-th [`DeviceClass::Block`] device in the table, as a cloned owning
/// reference (the table keeps its own). This **is** the block-device registry the
/// `/dev/blk` Kernel Server resolves against: block disks are indexed in the
/// order drivers published them. `None` if fewer than `index + 1` block devices
/// exist. The clone is an atomic refcount bump under the lock — no nested lock.
///
/// [`DeviceClass::Block`]: crate::object::device_node::DeviceClass::Block
pub fn find_block_device(index: usize) -> Option<ObjectRef> {
    let table = DEVICES.lock();
    let mut seen = 0usize;
    for node in table.iter() {
        // SAFETY: every table entry pins a live `DeviceNode`.
        let dn: &DeviceNode = unsafe { &*(node.as_ptr() as *const DeviceNode) };
        if dn.class() == DeviceClass::Block {
            if seen == index {
                return Some(node.clone());
            }
            seen += 1;
        }
    }
    None
}
