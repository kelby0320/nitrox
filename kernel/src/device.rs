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
