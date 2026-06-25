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
