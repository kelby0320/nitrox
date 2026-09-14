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

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::libkern::{KVec, SpinLock};
use crate::object::ObjectRef;
use crate::object::device_node::{DeviceClass, DeviceNode, ResourceDescriptor};
use crate::libkern::lockrank::LockRank;

/// The discovered devices. Written once at boot by [`init`]; read thereafter.
/// Lock rank: a leaf — never held across another lock.
static DEVICES: SpinLock<KVec<ObjectRef>> = SpinLock::new(LockRank::Registry, KVec::new());

/// Enumerate hardware and populate the device table. Boot-time; call once, after
/// the allocators, the HHDM, the kvmap, and `arch::Platform::init` are up.
pub fn init() {
    let nodes = crate::pci::enumerate();
    let count = nodes.len();
    *DEVICES.lock() = nodes;
    ENUMERATED.store(count, Ordering::Release);
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

// --- What the drivers did with each function (Phase 5 Part D.1) ---------------
//
// On a machine with no debugger the question after "what is there" is "what took it", and it
// has three answers, not two. A driver that matched a controller and gave it up is a different
// finding from no driver matching at all — on the laptop, "ahci declined: no SATA disk" points
// at port detection and "no driver" at the match table — so drivers report both, and the table
// keeps them beside the functions they describe.

/// The functions [`init`] enumerated: the first this-many entries of [`DEVICES`]. Everything
/// after them was registered by a driver (a disk, a partition), and has no outcome of its own.
static ENUMERATED: AtomicUsize = AtomicUsize::new(0);

/// How a claimed function's interrupts reach the kernel.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Signal {
    /// A message-signalled interrupt on `vector`.
    Msi { vector: u8 },
    /// The function's legacy interrupt pin, routed from `gsi` to `vector`.
    Intx { gsi: u32, vector: u8 },
}

/// What a driver did with a function it matched.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// `driver` took the function, and its interrupts arrive by `signal`.
    Claimed { driver: &'static str, signal: Signal },
    /// `driver` matched the function and gave it up, because `why`.
    Declined { driver: &'static str, why: &'static str },
}

/// A function's identity as outcomes are keyed: its PCI segment, bus, device and function.
type Address = (u16, u8, u8, u8);

fn address(desc: &ResourceDescriptor) -> Address {
    (desc.seg, desc.bus, desc.dev, desc.func)
}

/// Every outcome a driver has reported. Lock rank: `Registry` (it allocates while held), like
/// [`DEVICES`], and never held with it.
static OUTCOMES: SpinLock<KVec<(Address, Outcome)>> =
    SpinLock::new(LockRank::Registry, KVec::new());

/// Record what a driver did with the function `desc` describes. Boot-time, from
/// `drivers::probe`.
pub fn record_outcome(desc: &ResourceDescriptor, outcome: Outcome) {
    let pushed = OUTCOMES.lock().try_push((address(desc), outcome)).is_ok();
    if !pushed {
        let line = OutcomeLine { desc, outcome: Some(outcome) };
        crate::kprintln!("{line}, and there was no memory to record it");
    }
}

/// Log one line per enumerated function saying what became of it: claimed and how, declined
/// and why, or no driver. Boot-time, after every driver has probed.
pub fn log_outcomes() {
    let enumerated = ENUMERATED.load(Ordering::Acquire);
    let devices = snapshot();
    for node in devices.iter().take(enumerated) {
        // SAFETY: every table entry pins a live `DeviceNode`.
        let desc = *unsafe { &*(node.as_ptr() as *const DeviceNode) }.descriptor();
        let outcome = OUTCOMES.lock().iter().find(|(a, _)| *a == address(&desc)).map(|&(_, o)| o);
        crate::kprintln!("{}", OutcomeLine { desc: &desc, outcome });
    }
}

/// `drivers: 00:1f.2 claimed by ahci, MSI vec 0x32`, `drivers: 00:1f.2 declined by ahci: no
/// SATA disk on any implemented port`, or `drivers: 00:02.0 no driver`.
struct OutcomeLine<'a> {
    desc: &'a ResourceDescriptor,
    outcome: Option<Outcome>,
}

impl fmt::Display for OutcomeLine<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let d = self.desc;
        write!(f, "drivers: {:02x}:{:02x}.{} ", d.bus, d.dev, d.func)?;
        match self.outcome {
            Some(Outcome::Claimed { driver, signal: Signal::Msi { vector } }) => {
                write!(f, "claimed by {driver}, MSI vec {vector:#04x}")
            }
            Some(Outcome::Claimed { driver, signal: Signal::Intx { gsi, vector } }) => {
                write!(f, "claimed by {driver}, INTx GSI {gsi} vec {vector:#04x}")
            }
            Some(Outcome::Declined { driver, why }) => write!(f, "declined by {driver}: {why}"),
            None => f.write_str("no driver"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(bus: u8, dev: u8, func: u8) -> ResourceDescriptor {
        ResourceDescriptor { bus, dev, func, ..ResourceDescriptor::ZERO }
    }

    #[test]
    fn each_of_the_three_outcomes_reads_as_itself() {
        let ahci = at(0, 0x1f, 2);
        let claimed = Outcome::Claimed { driver: "ahci", signal: Signal::Msi { vector: 0x32 } };
        assert_eq!(
            format!("{}", OutcomeLine { desc: &ahci, outcome: Some(claimed) }),
            "drivers: 00:1f.2 claimed by ahci, MSI vec 0x32"
        );
        let intx =
            Outcome::Claimed { driver: "ahci", signal: Signal::Intx { gsi: 16, vector: 0x40 } };
        assert_eq!(
            format!("{}", OutcomeLine { desc: &ahci, outcome: Some(intx) }),
            "drivers: 00:1f.2 claimed by ahci, INTx GSI 16 vec 0x40"
        );
        let declined =
            Outcome::Declined { driver: "ahci", why: "no SATA disk on any implemented port" };
        assert_eq!(
            format!("{}", OutcomeLine { desc: &ahci, outcome: Some(declined) }),
            "drivers: 00:1f.2 declined by ahci: no SATA disk on any implemented port"
        );
        assert_eq!(
            format!("{}", OutcomeLine { desc: &at(0, 0x14, 0), outcome: None }),
            "drivers: 00:14.0 no driver"
        );
    }
}
