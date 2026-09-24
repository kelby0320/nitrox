//! The kernel device table — every [`DeviceNode`] the kernel has.
//!
//! Architecture-independent: the table is seeded by PCI(e) enumeration ([`crate::pci`]) — on
//! aarch64 it would be a Device Tree Blob — and drivers append what they publish after it: disks,
//! the partitions found on them, the RAM disk, the console, and the i8042's keyboard and mouse.
//! Each entry is an owning reference, so a registered node lives for the kernel's lifetime.
//!
//! **What the table knows that a node does not** is kept beside it: the node's kind, which its
//! `DeviceClass` is too coarse to say (the console and both i8042 nodes are all `Char`), **the
//! index its path serves it at**, and the node it belongs to. `/dev/blk` and `/dev/input/raw`
//! resolve through that served index, and `/dev/registry` reports it, so the two cannot disagree
//! (administration Part B).
//!
//! [`DeviceNode`]: crate::object::DeviceNode

use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::libkern::device::{
    DeviceKind, DeviceRecord, MAX_DRIVER_NAME, NO_PARENT, NOT_SERVED, OUTCOME_CLAIMED,
    OUTCOME_DECLINED, OUTCOME_NONE, REGISTRY_MAGIC, REGISTRY_VERSION, RegistryHeader,
};
use crate::libkern::block::BlockKind;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KVec, SpinLock};
use crate::object::ObjectRef;
use crate::object::device_node::{DeviceClass, DeviceNode, ResourceDescriptor};

/// One node, and what the table knows about it.
struct Entry {
    node: ObjectRef,
    kind: DeviceKind,
    /// The `<n>` its path serves it at, or [`NOT_SERVED`].
    served: u32,
    /// The index of the entry it belongs to, or [`NO_PARENT`].
    parent: u32,
    /// The driver that published it; empty for a PCI function, whose driver is its outcome's.
    driver: &'static str,
}

/// The table as a value, so a host test can build one without the kernel's.
pub struct Registry {
    entries: KVec<Entry>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// The node an entry holds.
fn node_of(e: &Entry) -> &DeviceNode {
    // SAFETY: every entry pins a live `DeviceNode` through its owning reference.
    unsafe { &*(e.node.as_ptr() as *const DeviceNode) }
}

impl Registry {
    /// An empty table.
    pub const fn new() -> Self {
        Self { entries: KVec::new() }
    }

    /// How many nodes it holds.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether it holds none.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn push(&mut self, entry: Entry) -> bool {
        // On failure the entry — and its reference — drops here, which is what the caller asked
        // for by handing the reference over.
        self.entries.try_push(entry).is_ok()
    }

    /// Append a PCI function `pci` enumerated.
    pub fn add_pci(&mut self, node: ObjectRef) -> bool {
        self.push(Entry {
            node,
            kind: DeviceKind::PciFunction,
            served: NOT_SERVED,
            parent: NO_PARENT,
            driver: "",
        })
    }

    /// Append a block node a driver published. Its kind is the one the driver gave it, **its
    /// served index is the number of block nodes before it** — `/dev/blk/<n>` in publication
    /// order, as it has always been — and its parent is the PCI function at its address, if it
    /// has one: a disk's controller.
    pub fn add_block(&mut self, node: ObjectRef, driver: &'static str) -> bool {
        let parent = self.pci_parent(node_of_ref(&node).descriptor());
        self.add_block_under(node, parent, driver)
    }

    /// Append a block node that belongs to `parent`, a node already in the table: a partition and
    /// its disk. A `parent` the table does not hold records no parent.
    pub fn add_block_child(&mut self, node: ObjectRef, parent: &ObjectRef, driver: &'static str) -> bool {
        let parent = self
            .entries
            .iter()
            .position(|e| e.node.as_ptr() == parent.as_ptr())
            .map_or(NO_PARENT, |i| i as u32);
        self.add_block_under(node, parent, driver)
    }

    fn add_block_under(&mut self, node: ObjectRef, parent: u32, driver: &'static str) -> bool {
        let served = self.entries.iter().filter(|e| node_of(e).class() == DeviceClass::Block).count() as u32;
        let kind = match BlockKind::from_u32(node_of_ref(&node).block_info().kind) {
            BlockKind::Disk => DeviceKind::Disk,
            BlockKind::Partition => DeviceKind::Partition,
            BlockKind::RamDisk => DeviceKind::RamDisk,
            BlockKind::Unknown => DeviceKind::Unknown,
        };
        self.push(Entry { node, kind, served, parent, driver })
    }

    /// Append a character node: the console, or an i8042 device at `/dev/input/raw/<served>`.
    pub fn add_char(
        &mut self,
        node: ObjectRef,
        kind: DeviceKind,
        served: u32,
        driver: &'static str,
    ) -> bool {
        self.push(Entry { node, kind, served, parent: NO_PARENT, driver })
    }

    /// The index of the PCI function at `desc`'s address, if `desc` names one.
    fn pci_parent(&self, desc: &ResourceDescriptor) -> u32 {
        // `0xFFFF` is `ResourceDescriptor::ZERO`'s vendor: no PCI device, and its all-zero
        // address would otherwise match the host bridge at 00:00.0.
        if desc.identity.vendor == 0xFFFF {
            return NO_PARENT;
        }
        self.entries
            .iter()
            .position(|e| e.kind == DeviceKind::PciFunction && address(node_of(e).descriptor()) == address(desc))
            .map_or(NO_PARENT, |i| i as u32)
    }

    /// The block node `/dev/blk/<index>` serves.
    pub fn block(&self, index: u32) -> Option<ObjectRef> {
        self.entries
            .iter()
            .find(|e| node_of(e).class() == DeviceClass::Block && e.served == index)
            .map(|e| e.node.clone())
    }

    /// The input node `/dev/input/raw/<index>` serves.
    pub fn input(&self, index: u32) -> Option<ObjectRef> {
        self.entries
            .iter()
            .find(|e| matches!(e.kind, DeviceKind::Keyboard | DeviceKind::Mouse) && e.served == index)
            .map(|e| e.node.clone())
    }

    /// Node `id` — its place in the table.
    pub fn node(&self, id: u32) -> Option<ObjectRef> {
        self.entries.get(id as usize).map(|e| e.node.clone())
    }

    /// Whether a node of `kind` is registered.
    pub fn has(&self, kind: DeviceKind) -> bool {
        self.entries.iter().any(|e| e.kind == kind)
    }

    /// Every node as a record, in table order. `outcome_of` says what a driver did with a PCI
    /// function, keyed by its descriptor.
    pub fn records(
        &self,
        outcome_of: impl Fn(&ResourceDescriptor) -> Option<Outcome>,
    ) -> Result<KVec<DeviceRecord>, AllocError> {
        let mut out: KVec<DeviceRecord> = KVec::new();
        out.try_reserve(self.entries.len())?;
        for (id, e) in self.entries.iter().enumerate() {
            let n = node_of(e);
            let d = n.descriptor();
            let mut r = DeviceRecord {
                id: id as u32,
                class: n.class() as u32,
                kind: e.kind.as_u32(),
                served: e.served,
                parent: e.parent,
                vendor: d.identity.vendor,
                device: d.identity.device,
                pci_class: d.identity.class,
                subclass: d.identity.subclass,
                prog_if: d.identity.prog_if,
                revision: d.identity.revision,
                seg: d.seg,
                bus: d.bus,
                dev: d.dev,
                func: d.func,
                ..DeviceRecord::default()
            };
            let mut driver = e.driver;
            if e.kind == DeviceKind::PciFunction {
                match outcome_of(d) {
                    Some(Outcome::Claimed { driver: by, .. }) => {
                        r.outcome = OUTCOME_CLAIMED;
                        driver = by;
                    }
                    Some(Outcome::Declined { driver: by, .. }) => {
                        r.outcome = OUTCOME_DECLINED;
                        driver = by;
                    }
                    None => r.outcome = OUTCOME_NONE,
                }
            }
            let dn = driver.as_bytes();
            let dl = dn.len().min(MAX_DRIVER_NAME);
            r.driver[..dl].copy_from_slice(&dn[..dl]);
            if n.class() == DeviceClass::Block {
                let info = n.block_info();
                r.logical_block_size = info.logical_block_size;
                r.block_count = info.block_count;
                r.name_len = info.name_len;
                r.name = info.name;
            } else {
                let word: &[u8] = match e.kind {
                    DeviceKind::Keyboard => b"keyboard",
                    DeviceKind::Mouse => b"mouse",
                    DeviceKind::Console => b"console",
                    _ => b"",
                };
                r.name[..word.len()].copy_from_slice(word);
                r.name_len = word.len() as u32;
            }
            out.try_push(r).expect("within reserved capacity");
        }
        Ok(out)
    }

    /// The bytes `/dev/registry` serves: a [`RegistryHeader`] carrying the count, then the
    /// records. The caller pads to the page by putting it in a memory object.
    pub fn snapshot_bytes(
        &self,
        outcome_of: impl Fn(&ResourceDescriptor) -> Option<Outcome>,
    ) -> Result<KVec<u8>, AllocError> {
        let records = self.records(outcome_of)?;
        let header = RegistryHeader {
            magic: REGISTRY_MAGIC,
            version: REGISTRY_VERSION,
            count: records.len() as u32,
            record_size: core::mem::size_of::<DeviceRecord>() as u32,
        };
        let mut out: KVec<u8> = KVec::new();
        out.try_reserve(core::mem::size_of::<RegistryHeader>() + records.len() * core::mem::size_of::<DeviceRecord>())?;
        out.try_extend_from_slice(header.as_bytes())?;
        for r in records.iter() {
            out.try_extend_from_slice(r.as_bytes())?;
        }
        Ok(out)
    }
}

/// The node `node` pins, for a reference not yet in a table.
fn node_of_ref(node: &ObjectRef) -> &DeviceNode {
    // SAFETY: callers pass references to `DeviceNode`s — every registration site builds one.
    unsafe { &*(node.as_ptr() as *const DeviceNode) }
}

/// The kernel's table. Seeded by [`init`], appended to by drivers during boot, read thereafter.
/// Lock rank: `Registry` — it allocates while held — and never held with another lock.
static DEVICES: SpinLock<Registry> = SpinLock::new(LockRank::Registry, Registry::new());

/// Enumerate hardware and seed the table. Boot-time; call once, after the allocators, the
/// HHDM, the kvmap, and `arch::Platform::init` are up.
pub fn init() {
    let nodes = crate::pci::enumerate();
    let count = nodes.len();
    let mut table = DEVICES.lock();
    for node in nodes.iter() {
        if !table.add_pci(node.clone()) {
            crate::kprintln!("device: table full; dropping an enumerated function");
        }
    }
    drop(table);
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
    for e in table.entries.iter() {
        out.try_push(e.node.clone()).expect("within reserved capacity");
    }
    out
}

/// Append a block node a driver published — a disk or the RAM disk. The table takes ownership
/// of `node`.
pub fn register(node: ObjectRef, driver: &'static str) {
    if !DEVICES.lock().add_block(node, driver) {
        crate::kprintln!("device: table full; dropping a registered node");
    }
}

/// Append a partition of `disk`. The table takes ownership of `node`.
pub fn register_partition(node: ObjectRef, disk: &ObjectRef, driver: &'static str) {
    if !DEVICES.lock().add_block_child(node, disk, driver) {
        crate::kprintln!("device: table full; dropping a registered partition");
    }
}

/// Append a character node — the console, or an i8042 device served at
/// `/dev/input/raw/<served>`. The table takes ownership of `node`.
pub fn register_char(node: ObjectRef, kind: DeviceKind, served: u32, driver: &'static str) {
    if !DEVICES.lock().add_char(node, kind, served, driver) {
        crate::kprintln!("device: table full; dropping a registered {:?}", kind);
    }
}

/// The node `/dev/blk/<index>` serves, as a cloned owning reference (the table keeps its own).
/// Resolved through each entry's served index, which is also what `/dev/registry` reports.
pub fn find_block_device(index: usize) -> Option<ObjectRef> {
    DEVICES.lock().block(index as u32)
}

/// The node `/dev/input/raw/<index>` serves.
pub fn find_input_device(index: usize) -> Option<ObjectRef> {
    DEVICES.lock().input(index as u32)
}

/// Node `id`, for `/dev/registry/<id>`.
pub fn node(id: u32) -> Option<ObjectRef> {
    DEVICES.lock().node(id)
}

/// Whether a node of `kind` is registered.
pub fn has(kind: DeviceKind) -> bool {
    DEVICES.lock().has(kind)
}

/// What `/dev/registry` serves: the header and every node's record.
pub fn registry_snapshot() -> Result<KVec<u8>, AllocError> {
    // Outcomes are copied out before the table is locked, because the two locks share a rank
    // and are never held together.
    let mut outcomes: KVec<(Address, Outcome)> = KVec::new();
    {
        let recorded = OUTCOMES.lock();
        outcomes.try_reserve(recorded.len())?;
        for &o in recorded.iter() {
            outcomes.try_push(o).expect("within reserved capacity");
        }
    }
    DEVICES.lock().snapshot_bytes(|d| {
        outcomes.iter().find(|(a, _)| *a == address(d)).map(|&(_, o)| o)
    })
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
    use crate::io::block::BlockBackend;
    use crate::io::irp::Irp;
    use crate::libkern::KBox;
    use crate::libkern::handle::KObjectType;
    use crate::mm::test_support::init_global_heap;
    use crate::object::device_node::{BlockGeometry, CharBackend, DeviceIdentity};

    fn at(bus: u8, dev: u8, func: u8) -> ResourceDescriptor {
        ResourceDescriptor { bus, dev, func, ..ResourceDescriptor::ZERO }
    }

    /// A PCI function at `bus:dev.func` — a real vendor, so an address can match it.
    fn pci_at(bus: u8, dev: u8, func: u8) -> ResourceDescriptor {
        ResourceDescriptor {
            identity: DeviceIdentity {
                vendor: 0x8086,
                device: 0x2922,
                class: 0x01,
                subclass: 0x06,
                prog_if: 0x01,
                revision: 0x02,
            },
            ..at(bus, dev, func)
        }
    }

    fn adopt(node: KBox<DeviceNode>) -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; the test adopts it.
        unsafe { ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode) }
    }

    fn no_submit(_: *mut Irp, _: *mut ()) {}
    fn no_poll(_: *mut ()) {}
    fn no_read(_: &ObjectRef, _: &ObjectRef, _: u64, _: u64, _: *mut ()) -> Result<(), crate::syscall::error::KError> {
        Err(crate::syscall::error::KError::Unsupported)
    }

    fn block(desc: ResourceDescriptor, kind: BlockKind, name: &[u8], blocks: u64) -> ObjectRef {
        let backend = BlockBackend { submit: no_submit, poll: no_poll, ctx: core::ptr::null_mut(), max_frags: 1 };
        let geometry = BlockGeometry { logical_block_size: 512, block_count: blocks };
        adopt(DeviceNode::try_new_block(desc, geometry, kind, name, backend).unwrap())
    }

    fn char_node() -> ObjectRef {
        let backend = CharBackend { submit_read: no_read, ctx: core::ptr::null_mut() };
        adopt(DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend).unwrap())
    }

    /// The boot's order: PCI functions — the host bridge at 00:00.0 first, as on every PC — then a
    /// disk on the AHCI controller, its partition, a RAM disk, the console, and the i8042's
    /// keyboard and mouse.
    fn booted() -> (Registry, ObjectRef, ObjectRef) {
        let mut r = Registry::new();
        assert!(r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, pci_at(0, 0, 0), BlockGeometry::ZERO).unwrap())));
        assert!(r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, pci_at(0, 0x1f, 2), BlockGeometry::ZERO).unwrap())));
        assert!(r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, pci_at(0, 0x02, 0), BlockGeometry::ZERO).unwrap())));
        let disk = block(pci_at(0, 0x1f, 2), BlockKind::Disk, b"QEMU HARDDISK (QM00001)", 1 << 20);
        assert!(r.add_block(disk.clone(), "ahci"));
        assert!(r.add_block_child(block(pci_at(0, 0x1f, 2), BlockKind::Partition, b"nitrox-root", 1 << 19), &disk, "gpt"));
        assert!(r.add_block(block(ResourceDescriptor::ZERO, BlockKind::RamDisk, b"module 0 (root.img)", 64), "ramdisk"));
        assert!(r.add_char(char_node(), DeviceKind::Console, NOT_SERVED, "console"));
        let keyboard = char_node();
        assert!(r.add_char(keyboard.clone(), DeviceKind::Keyboard, 0, "i8042"));
        assert!(r.add_char(char_node(), DeviceKind::Mouse, 1, "i8042"));
        (r, disk, keyboard)
    }

    fn text(bytes: &[u8], len: u32) -> &str {
        core::str::from_utf8(&bytes[..len as usize]).unwrap()
    }

    /// **Every node, in table order, with what the table knows about it.** A disk's parent is its
    /// controller by address, a partition's is its disk, and the RAM disk — whose descriptor's
    /// all-zero address is the host bridge's — has none. **The keyboard is served at 0 and the
    /// mouse at 1** although the console, also `Char`, registered before them: a served index
    /// counted within `DeviceClass` would call the keyboard `input-1` (PR #332 review, finding 4).
    #[test]
    fn a_record_per_node_with_its_kind_served_index_and_parent() {
        init_global_heap();
        let (r, _, _) = booted();
        let recs = r.records(|_| None).unwrap();
        let summary: KVec<(u32, DeviceKind, u32, u32)> = {
            let mut v = KVec::new();
            for x in recs.iter() {
                v.try_push((x.id, DeviceKind::from_u32(x.kind), x.served, x.parent)).unwrap();
            }
            v
        };
        assert_eq!(
            &summary[..],
            &[
                (0, DeviceKind::PciFunction, NOT_SERVED, NO_PARENT),
                (1, DeviceKind::PciFunction, NOT_SERVED, NO_PARENT),
                (2, DeviceKind::PciFunction, NOT_SERVED, NO_PARENT),
                (3, DeviceKind::Disk, 0, 1),
                (4, DeviceKind::Partition, 1, 3),
                (5, DeviceKind::RamDisk, 2, NO_PARENT),
                (6, DeviceKind::Console, NOT_SERVED, NO_PARENT),
                (7, DeviceKind::Keyboard, 0, NO_PARENT),
                (8, DeviceKind::Mouse, 1, NO_PARENT),
            ]
        );
        assert_eq!(text(&recs[3].name, recs[3].name_len), "QEMU HARDDISK (QM00001)");
        assert_eq!((recs[3].logical_block_size, recs[3].block_count), (512, 1 << 20));
        assert_eq!(text(&recs[4].driver, 3), "gpt");
        assert_eq!(text(&recs[7].name, recs[7].name_len), "keyboard");
        assert_eq!(recs[5].vendor, 0xFFFF, "the RAM disk is no PCI device");
    }

    /// **The paths resolve through the same served index the records report.**
    #[test]
    fn the_paths_serve_what_the_records_say() {
        init_global_heap();
        let (r, disk, keyboard) = booted();
        assert_eq!(r.block(0).unwrap().as_ptr(), disk.as_ptr(), "/dev/blk/0 is the disk");
        assert!(r.block(3).is_none(), "three block devices, not four");
        assert_eq!(r.input(0).unwrap().as_ptr(), keyboard.as_ptr(), "/dev/input/raw/0 is the keyboard");
        assert!(r.input(2).is_none());
        assert_eq!(r.node(7).unwrap().as_ptr(), keyboard.as_ptr(), "/dev/registry/7 is the keyboard");
        assert!(r.node(9).is_none());
    }

    /// **A PCI function carries what its driver did**, and a function with no outcome says so.
    #[test]
    fn a_pci_record_carries_its_drivers_outcome() {
        init_global_heap();
        let (r, _, _) = booted();
        let claimed = Outcome::Claimed { driver: "ahci", signal: Signal::Msi { vector: 0x32 } };
        let recs = r.records(|d| (address(d) == (0, 0, 0x1f, 2)).then_some(claimed)).unwrap();
        assert_eq!(recs[1].outcome, OUTCOME_CLAIMED);
        assert_eq!(text(&recs[1].driver, 4), "ahci");
        assert_eq!(recs[2].outcome, OUTCOME_NONE);
        assert_eq!(recs[2].driver[0], 0, "no driver, no name");
    }

    /// **The header's count is the number of records**, and the bytes are exactly the header and
    /// those records — the padding is the memory object's, added after.
    #[test]
    fn the_snapshot_is_a_header_counting_its_records() {
        init_global_heap();
        let (r, _, _) = booted();
        let bytes = r.snapshot_bytes(|_| None).unwrap();
        let header_len = core::mem::size_of::<RegistryHeader>();
        let record_len = core::mem::size_of::<DeviceRecord>();
        assert_eq!(bytes.len(), header_len + 9 * record_len);
        let word = |off: usize| u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
        assert_eq!((word(0), word(4), word(8), word(12)), (REGISTRY_MAGIC, REGISTRY_VERSION, 9, record_len as u32));
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
