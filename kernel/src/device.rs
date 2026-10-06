//! The kernel device table — every [`DeviceNode`] the kernel has.
//!
//! Architecture-independent: the table is seeded by PCI(e) enumeration ([`crate::pci`]) — on
//! aarch64 it would be a Device Tree Blob — and drivers append what they publish after it: disks,
//! the partitions found on them, the RAM disk, the console, the i8042's keyboard and mouse, and
//! the USB devices the hub thread enumerates. Each entry is an owning reference, so a registered
//! node lives for the kernel's lifetime — a USB device that leaves included.
//!
//! **A device that leaves is departed, not removed** (Phase 6 Part C): an id is a record's place,
//! and `/dev/registry/<id>`, `device-mgr`'s `usb-<id>` and an owner's `Departed` all name a device
//! by it. A departed entry keeps its fields and its served index, which no later device takes; its
//! paths stop resolving; and its children depart with it. **Every change bumps the table's
//! generation**, which the snapshot carries and `/dev/registry/changes` answers a waiting read
//! with, so a reader that waits past the generation of the snapshot it holds misses nothing.
//!
//! **What the table knows that a node does not** is kept beside it: the node's kind, which its
//! `DeviceClass` is too coarse to say (the console and both i8042 nodes are all `Char`), **the
//! index its path serves it at**, and the node it belongs to. `/dev/blk` and `/dev/input/raw`
//! resolve through that served index, and `/dev/registry` reports it, so the two cannot disagree
//! (administration Part B). A USB device's IDs, class, port, speed and name are kept there too,
//! since its node is a bare `Other` (Phase 6 Part A.3).
//!
//! [`DeviceNode`]: crate::object::DeviceNode

use core::fmt;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::libkern::device::{
    BOOT, DEPARTED, DeviceKind, DeviceRecord, MAX_DRIVER_NAME, NO_PARENT, NOT_SERVED, OUTCOME_CLAIMED,
    OUTCOME_DECLINED, OUTCOME_NONE, REGISTRY_MAGIC, REGISTRY_VERSION, RegistryHeader,
};
use crate::libkern::block::{BlockKind, MAX_DEVICE_NAME};
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KVec, SpinLock};
use crate::libkern::handle::KObjectType;
use crate::object::device_node::{CharBackend, DeviceClass, DeviceNode, ResourceDescriptor};
use crate::object::{MemoryObject, ObjectRef};
use crate::syscall::error::KError;

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
    /// For a USB device, what its record carries that its node cannot say (Phase 6 Part A.3).
    usb: Option<UsbFacts>,
    /// The device has left (Phase 6 Part C): its record says so, and its paths answer `NotFound`.
    departed: bool,
    /// **The disk the machine started from** (Phase 6 Part D): its GPT's GUID is the one Limine
    /// loaded the modules from.
    boot: bool,
}

/// **What a USB device's record carries** that its node does not (Phase 6 Part A.3). The node is a
/// plain `Other` with the zero descriptor — vendor `0xFFFF`, which the table reads as "not a PCI
/// function", where a USB device's zero address would find the host bridge — and these fill its
/// record instead.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct UsbFacts {
    /// `idVendor`.
    pub vendor: u16,
    /// `idProduct`.
    pub product: u16,
    /// Its class triple: the device descriptor's, or its first interface's when that is zero.
    pub class: (u8, u8, u8),
    /// The root port, numbered from 1.
    pub port: u8,
    /// Its speed, as the xHCI's default speed IDs number them.
    pub speed: u8,
    /// Its product and serial strings, printable, or `vvvv:pppp` when it has none.
    pub name: [u8; MAX_DEVICE_NAME],
    /// How many bytes of [`name`](Self::name) are meaningful.
    pub name_len: usize,
}

/// The table as a value, so a host test can build one without the kernel's.
pub struct Registry {
    entries: KVec<Entry>,
    /// Bumped once by every change: a registration, or a departure with its children (Phase 6
    /// Part C).
    generation: u64,
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
        Self { entries: KVec::new(), generation: 0 }
    }

    /// The table's generation: how many changes it has seen (Phase 6 Part C).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// **Depart node `id` and every node whose parent chain reaches it** (Phase 6 Part C): a USB
    /// device's keyboard and mouse with it, a disk's partitions with the disk. One change, so one
    /// generation. Whether anything departed that had not: an id the table does not hold, or one
    /// already departed with all of its children, changes nothing.
    pub fn depart(&mut self, id: u32) -> bool {
        let mut gone: KVec<u32> = KVec::new();
        let mut changed = false;
        // A parent is registered before its children, so one pass in table order from `id` finds
        // every descendant: a node is in the set exactly when its parent already is.
        for i in id as usize..self.entries.len() {
            let e = &self.entries[i];
            if i != id as usize && !gone.contains(&e.parent) {
                continue;
            }
            if gone.try_push(i as u32).is_err() {
                break;
            }
            let e = &mut self.entries[i];
            changed |= !e.departed;
            e.departed = true;
        }
        if changed {
            self.generation += 1;
        }
        changed
    }

    /// **Flag `disk` as the disk the machine started from** (Phase 6 Part D). One change, so one
    /// generation. Whether it changed anything: a node the table does not hold, or one flagged
    /// already, does not.
    pub fn mark_boot(&mut self, disk: &ObjectRef) -> bool {
        let Some(e) = self.entries.iter_mut().find(|e| e.node.as_ptr() == disk.as_ptr()) else {
            return false;
        };
        if e.boot {
            return false;
        }
        e.boot = true;
        self.generation += 1;
        true
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
        let pushed = self.entries.try_push(entry).is_ok();
        if pushed {
            self.generation += 1;
        }
        pushed
    }

    /// Append a PCI function `pci` enumerated.
    pub fn add_pci(&mut self, node: ObjectRef) -> bool {
        self.push(Entry {
            node,
            kind: DeviceKind::PciFunction,
            served: NOT_SERVED,
            parent: NO_PARENT,
            driver: "",
            usb: None,
            departed: false,
            boot: false,
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

    /// **Append a block node under the entry `parent`**, by id: a USB disk under its `UsbDevice`
    /// record (Phase 6 Part D), whose address names no PCI function.
    pub fn add_block_under(&mut self, node: ObjectRef, parent: u32, driver: &'static str) -> bool {
        let served = self.entries.iter().filter(|e| node_of(e).class() == DeviceClass::Block).count() as u32;
        let kind = match BlockKind::from_u32(node_of_ref(&node).block_info().kind) {
            BlockKind::Disk => DeviceKind::Disk,
            BlockKind::Partition => DeviceKind::Partition,
            BlockKind::RamDisk => DeviceKind::RamDisk,
            BlockKind::Unknown => DeviceKind::Unknown,
        };
        self.push(Entry { node, kind, served, parent, driver, usb: None, departed: false, boot: false })
    }

    /// Append a character node: the console, or an i8042 device at `/dev/input/raw/<served>`.
    pub fn add_char(
        &mut self,
        node: ObjectRef,
        kind: DeviceKind,
        served: u32,
        driver: &'static str,
    ) -> bool {
        self.push(Entry { node, kind, served, parent: NO_PARENT, driver, usb: None, departed: false, boot: false })
    }

    /// Append a keyboard or mouse a USB device provides (Phase 6 Part B.2), under `parent`, the
    /// device's record. **Its served index is the next after every input node's**: the i8042 keeps
    /// 0 and 1, and a machine without one starts at 0. A count of input nodes would collide where
    /// the i8042 has a mouse and no keyboard. The index, or `None` when the table could not take it.
    pub fn add_input(
        &mut self,
        node: ObjectRef,
        kind: DeviceKind,
        parent: Option<u32>,
        driver: &'static str,
    ) -> Option<u32> {
        let served = self.next_input_index();
        let parent = parent.unwrap_or(NO_PARENT);
        self.push(Entry { node, kind, served, parent, driver, usb: None, departed: false, boot: false }).then_some(served)
    }

    /// The index after every input node's: the next `/dev/input/raw/<n>`.
    fn next_input_index(&self) -> u32 {
        self.entries
            .iter()
            .filter(|e| matches!(e.kind, DeviceKind::Keyboard | DeviceKind::Mouse))
            .map(|e| e.served + 1)
            .max()
            .unwrap_or(0)
    }

    /// Append a USB device under the PCI function at `controller`'s address, carrying `facts`. Its
    /// id, which is its place, or `None` when the table could not take it.
    pub fn add_usb(
        &mut self,
        node: ObjectRef,
        controller: &ResourceDescriptor,
        driver: &'static str,
        facts: UsbFacts,
    ) -> Option<u32> {
        let parent = self.pci_parent(controller);
        let id = self.entries.len() as u32;
        let usb = Some(facts);
        let kind = DeviceKind::UsbDevice;
        let entry = Entry { node, kind, served: NOT_SERVED, parent, driver, usb, departed: false, boot: false };
        self.push(entry).then_some(id)
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

    /// The block node `/dev/blk/<index>` serves. **None for a departed one** (Phase 6 Part C).
    pub fn block(&self, index: u32) -> Option<ObjectRef> {
        self.entries
            .iter()
            .find(|e| !e.departed && node_of(e).class() == DeviceClass::Block && e.served == index)
            .map(|e| e.node.clone())
    }

    /// The input node `/dev/input/raw/<index>` serves. **None for a departed one** (Phase 6 Part C).
    pub fn input(&self, index: u32) -> Option<ObjectRef> {
        self.entries
            .iter()
            .find(|e| !e.departed && matches!(e.kind, DeviceKind::Keyboard | DeviceKind::Mouse) && e.served == index)
            .map(|e| e.node.clone())
    }

    /// Node `id` — its place in the table. **None for a departed one** (Phase 6 Part C).
    pub fn node(&self, id: u32) -> Option<ObjectRef> {
        self.entries.get(id as usize).filter(|e| !e.departed).map(|e| e.node.clone())
    }

    /// Whether a node of `kind` is registered and has not departed.
    pub fn has(&self, kind: DeviceKind) -> bool {
        self.entries.iter().any(|e| !e.departed && e.kind == kind)
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
            if e.departed {
                r.flags |= DEPARTED;
            }
            if e.boot {
                r.flags |= BOOT;
            }
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
            if let Some(u) = &e.usb {
                r.vendor = u.vendor;
                r.device = u.product;
                (r.pci_class, r.subclass, r.prog_if) = u.class;
                r.port = u.port;
                r.speed = u.speed;
                r.name[..u.name_len].copy_from_slice(&u.name[..u.name_len]);
                r.name_len = u.name_len as u32;
            } else if n.class() == DeviceClass::Block {
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
            generation: self.generation,
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
    let backend = CharBackend { submit_read: changes_read, submit_write: None, ctx: core::ptr::null_mut() };
    match DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend) {
        Ok(node) => *CHANGES.lock() = Some(crate::drivers::adopt(node, KObjectType::DeviceNode)),
        Err(_) => crate::kprintln!("device: no memory for /dev/registry/changes; nothing will learn of a change"),
    }
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
    announce();
}

/// Append a partition of `disk`. The table takes ownership of `node`.
pub fn register_partition(node: ObjectRef, disk: &ObjectRef, driver: &'static str) {
    if !DEVICES.lock().add_block_child(node, disk, driver) {
        crate::kprintln!("device: table full; dropping a registered partition");
    }
    announce();
}

/// Append a character node — the console, or an i8042 device served at
/// `/dev/input/raw/<served>`. The table takes ownership of `node`.
pub fn register_char(node: ObjectRef, kind: DeviceKind, served: u32, driver: &'static str) {
    if !DEVICES.lock().add_char(node, kind, served, driver) {
        crate::kprintln!("device: table full; dropping a registered {:?}", kind);
    }
    announce();
}

/// Append a USB device under the PCI function at `controller`'s address, carrying `facts`. The
/// table takes ownership of `node`. Its id — the `<id>` of `/dev/registry/<id>` and of
/// `device-mgr`'s `usb-<id>` — or `None` when the table could not take it. From the hub thread
/// (Phase 6 Part A.3), never a DPC: the table allocates while locked.
pub fn register_usb(
    node: ObjectRef,
    controller: &ResourceDescriptor,
    driver: &'static str,
    facts: UsbFacts,
) -> Option<u32> {
    let id = DEVICES.lock().add_usb(node, controller, driver, facts);
    if id.is_none() {
        crate::kprintln!("device: table full; dropping a USB device");
    }
    announce();
    id
}

/// **Append a disk a USB device provides**, under its device's record `parent` (Phase 6 Part D). Its
/// id, the place its partitions are registered under, or `None` if the table could not take it.
pub fn register_block_under(node: ObjectRef, parent: u32, driver: &'static str) -> Option<u32> {
    let id = {
        let mut d = DEVICES.lock();
        let id = d.len() as u32;
        d.add_block_under(node, parent, driver).then_some(id)
    };
    if id.is_none() {
        crate::kprintln!("device: table full; dropping a registered disk");
    }
    announce();
    id
}

/// Append a keyboard or mouse a USB device provides, under its device's record `parent`, at the next
/// `/dev/input/raw/<n>`. The table takes ownership of `node`. The `<n>`, or `None` when the table
/// could not take it. From the hub thread (Phase 6 Part B.2).
pub fn register_input(node: ObjectRef, kind: DeviceKind, parent: Option<u32>, driver: &'static str) -> Option<u32> {
    let served = DEVICES.lock().add_input(node, kind, parent, driver);
    if served.is_none() {
        crate::kprintln!("device: table full; dropping a {:?}", kind);
    }
    announce();
    served
}

/// **Depart node `id` and every node whose parent chain reaches it** (Phase 6 Part C) — a USB
/// device and its keyboards and mice — as one change, and answer the reads waiting on
/// `/dev/registry/changes`. From the hub thread, never a DPC: the answers complete operations.
pub fn depart(id: u32) {
    if DEVICES.lock().depart(id) {
        announce();
    }
}

/// **Flag `disk` as the disk the machine started from** (Phase 6 Part D), as one change. Whether
/// it was not flagged before.
pub fn mark_boot(disk: &ObjectRef) -> bool {
    let marked = DEVICES.lock().mark_boot(disk);
    if marked {
        announce();
    }
    marked
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

/// The node `/dev/registry/changes` serves (Phase 6 Part C), or `None` if it could not be made.
pub fn changes_node() -> Option<ObjectRef> {
    CHANGES.lock().clone()
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

// --- `/dev/registry/changes` (Phase 6 Part C) -----------------------------------
//
// **How a reader learns that the table changed**: a char node whose `Read` waits until the table's
// generation is past the read's `offset`, then answers with the generation, eight bytes. A reader
// that is behind is answered at once. The device manager reads a snapshot, then waits past its
// generation, so a change between the two answers the wait at once and none is missed. It is the
// async model as it is — `sys_io_submit`, a `PendingOperation`, `sys_wait` — and needs no way for
// the kernel to name the manager, which a notification would have (the plan's detail pass).

/// Reads that may wait on `/dev/registry/changes` at once: the device manager's one, with room to
/// spare. A fifth is refused, `WouldBlock`.
pub const WAITERS_MAX: usize = 4;

/// **The reads waiting for the table to pass a generation**, as a value a host test can drive.
pub struct Waiters<T> {
    slots: [Option<(u64, T)>; WAITERS_MAX],
}

impl<T> Default for Waiters<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Waiters<T> {
    /// None waiting.
    pub const fn new() -> Self {
        Self { slots: [const { None }; WAITERS_MAX] }
    }

    /// Wait `item` until the generation is past `after`, or hand it back when every slot is taken.
    pub fn park(&mut self, after: u64, item: T) -> Result<(), T> {
        match self.slots.iter_mut().find(|s| s.is_none()) {
            Some(slot) => {
                *slot = Some((after, item));
                Ok(())
            }
            None => Err(item),
        }
    }

    /// **Every item `generation` is past**, taken out to be answered; the rest go on waiting.
    pub fn take_passed(&mut self, generation: u64) -> [Option<T>; WAITERS_MAX] {
        let mut out = [const { None }; WAITERS_MAX];
        for (slot, o) in self.slots.iter_mut().zip(out.iter_mut()) {
            if slot.as_ref().is_some_and(|(after, _)| generation > *after) {
                *o = slot.take().map(|(_, item)| item);
            }
        }
        out
    }
}

/// A read waiting on the change node: what answers it.
struct WaitingRead {
    po: ObjectRef,
    buffer: ObjectRef,
    buf_offset: u64,
}

/// The generation the waiting reads are judged against: the table's, stored after every change
/// and before the waiting reads are looked at, so a read that finds it old has parked in time for
/// the change to find it.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// The reads waiting. A leaf: nothing is completed or dropped while it is held.
static WAITING: SpinLock<Waiters<WaitingRead>> = SpinLock::new(LockRank::Leaf, Waiters::new());

/// The node `/dev/registry/changes` serves, made by [`init`].
static CHANGES: SpinLock<Option<ObjectRef>> = SpinLock::new(LockRank::Leaf, None);

/// **The table changed**: store its generation and answer every waiting read it is past. Thread
/// context, with no lock held — answering completes an operation, which takes the scheduler's.
fn announce() {
    let generation = DEVICES.lock().generation();
    // `fetch_max`: two changes announcing out of order must not move the generation back.
    GENERATION.fetch_max(generation, Ordering::AcqRel);
    let passed = WAITING.lock().take_passed(GENERATION.load(Ordering::Acquire));
    for read in passed.into_iter().flatten() {
        answer(&read.buffer, &read.po, read.buf_offset, GENERATION.load(Ordering::Acquire));
    }
}

/// Write `generation` into `buffer` at `buf_offset` and complete `po` with its eight bytes.
fn answer(buffer: &ObjectRef, po: &ObjectRef, buf_offset: u64, generation: u64) {
    // SAFETY: `buffer` is a live `MemoryObject` reference, held across this — `sys_io_submit`
    // checked its type and the range.
    let mo: &MemoryObject = unsafe { &*(buffer.as_ptr() as *const MemoryObject) };
    mo.copy_in(buf_offset as usize, &generation.to_le_bytes());
    crate::sched::complete_pending_op(po.as_ptr(), 0, 8);
}

/// [`CharBackend::submit_read`] for `/dev/registry/changes`: **answer at once if the table is past
/// `offset`, the generation the reader holds, or wait until it is.** At least eight bytes, or
/// `InvalidArgument`; a fifth reader waiting is refused, `WouldBlock`.
fn changes_read(
    buffer: &ObjectRef,
    po: &ObjectRef,
    buf_offset: u64,
    offset: u64,
    max_len: u64,
    _ctx: *mut (),
) -> Result<(), KError> {
    read_changes(&WAITING, &GENERATION, buffer, po, buf_offset, offset, max_len)
}

/// [`changes_read`] against `waiting` and `generation`: the statics in a boot, and a host test's own
/// (PR #361 review), so the read's two refusals are held through the read rather than through
/// [`Waiters`] alone.
fn read_changes(
    waiting: &SpinLock<Waiters<WaitingRead>>,
    generation: &AtomicU64,
    buffer: &ObjectRef,
    po: &ObjectRef,
    buf_offset: u64,
    offset: u64,
    max_len: u64,
) -> Result<(), KError> {
    if max_len < 8 {
        return Err(KError::InvalidArgument);
    }
    enum Then {
        Answer(u64),
        Waiting,
        Full(WaitingRead),
    }
    let then = {
        let mut waiting = waiting.lock();
        let generation = generation.load(Ordering::Acquire);
        if generation > offset {
            Then::Answer(generation)
        } else {
            match waiting.park(offset, WaitingRead { po: po.clone(), buffer: buffer.clone(), buf_offset }) {
                Ok(()) => Then::Waiting,
                Err(read) => Then::Full(read),
            }
        }
    };
    match then {
        Then::Answer(generation) => {
            answer(buffer, po, buf_offset, generation);
            Ok(())
        }
        Then::Waiting => Ok(()),
        Then::Full(read) => {
            // Dropped here, with the lock let go: its references may be the last.
            drop(read);
            Err(KError::WouldBlock)
        }
    }
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
    fn no_read(_: &ObjectRef, _: &ObjectRef, _: u64, _: u64, _: u64, _: *mut ()) -> Result<(), KError> {
        Err(KError::Unsupported)
    }

    fn block(desc: ResourceDescriptor, kind: BlockKind, name: &[u8], blocks: u64) -> ObjectRef {
        let backend = BlockBackend { submit: no_submit, poll: no_poll, ctx: core::ptr::null_mut(), max_frags: 1 };
        let geometry = BlockGeometry { logical_block_size: 512, block_count: blocks };
        adopt(DeviceNode::try_new_block(desc, geometry, kind, name, backend).unwrap())
    }

    fn char_node() -> ObjectRef {
        let backend = CharBackend { submit_read: no_read, submit_write: None, ctx: core::ptr::null_mut() };
        adopt(DeviceNode::try_new_char(ResourceDescriptor::ZERO, backend).unwrap())
    }

    /// The boot's order, which `drivers::probe` sets: PCI functions — the host bridge at 00:00.0
    /// first, as on every PC — then a disk on the AHCI controller, then a RAM disk (published
    /// before the GPT pass, so it can be scanned like a disk), then the disk's partition from that
    /// pass, then the console and the i8042's keyboard and mouse. It was disk, partition, RAM disk
    /// until the PR #333 review found no boot does that.
    fn booted() -> (Registry, ObjectRef, ObjectRef) {
        let mut r = Registry::new();
        assert!(r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, pci_at(0, 0, 0), BlockGeometry::ZERO).unwrap())));
        assert!(r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, pci_at(0, 0x1f, 2), BlockGeometry::ZERO).unwrap())));
        assert!(r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, pci_at(0, 0x02, 0), BlockGeometry::ZERO).unwrap())));
        let disk = block(pci_at(0, 0x1f, 2), BlockKind::Disk, b"QEMU HARDDISK (QM00001)", 1 << 20);
        assert!(r.add_block(disk.clone(), "ahci"));
        assert!(r.add_block(block(ResourceDescriptor::ZERO, BlockKind::RamDisk, b"module 0 (root.img)", 64), "ramdisk"));
        assert!(r.add_block_child(block(pci_at(0, 0x1f, 2), BlockKind::Partition, b"nitrox-root", 1 << 19), &disk, "gpt"));
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
                (4, DeviceKind::RamDisk, 1, NO_PARENT),
                (5, DeviceKind::Partition, 2, 3),
                (6, DeviceKind::Console, NOT_SERVED, NO_PARENT),
                (7, DeviceKind::Keyboard, 0, NO_PARENT),
                (8, DeviceKind::Mouse, 1, NO_PARENT),
            ]
        );
        assert_eq!(text(&recs[3].name, recs[3].name_len), "QEMU HARDDISK (QM00001)");
        assert_eq!((recs[3].logical_block_size, recs[3].block_count), (512, 1 << 20));
        assert_eq!(text(&recs[5].driver, 3), "gpt");
        assert_eq!(text(&recs[7].name, recs[7].name_len), "keyboard");
        assert_eq!(recs[4].vendor, 0xFFFF, "the RAM disk is no PCI device");
    }

    /// **A USB keyboard or mouse takes the next index after every input node's** (Phase 6 Part
    /// B.2): 2 and 3 beside the i8042's two, 0 and 1 on a machine without one, and 2 where the
    /// i8042 has a mouse and no keyboard — where counting input nodes would give the mouse's 1.
    #[test]
    fn a_usb_input_node_takes_the_next_index_after_every_input_nodes() {
        init_global_heap();
        let (mut r, _, _) = booted();
        assert_eq!(r.add_input(char_node(), DeviceKind::Keyboard, Some(3), "usb-hid"), Some(2));
        assert_eq!(r.add_input(char_node(), DeviceKind::Mouse, None, "usb-hid"), Some(3));
        let recs = r.records(|_| None).unwrap();
        let k = &recs[9];
        assert_eq!((DeviceKind::from_u32(k.kind), k.served, k.parent), (DeviceKind::Keyboard, 2, 3));
        assert_eq!(text(&k.driver, 7), "usb-hid");
        assert_eq!(text(&k.name, k.name_len), "keyboard", "the kind word, as the i8042's");
        assert_eq!(recs[10].parent, NO_PARENT);
        assert_eq!(r.input(3).unwrap().as_ptr(), r.node(10).unwrap().as_ptr(), "/dev/input/raw/3 is the mouse");

        let mut bare = Registry::new();
        assert_eq!(bare.add_input(char_node(), DeviceKind::Keyboard, None, "usb-hid"), Some(0), "no i8042");
        let mut mouse_only = Registry::new();
        assert!(mouse_only.add_char(char_node(), DeviceKind::Mouse, 1, "i8042"));
        assert_eq!(mouse_only.add_input(char_node(), DeviceKind::Keyboard, None, "usb-hid"), Some(2));
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

    /// **A PCI function carries what its driver did** — claimed, declined, or nothing — and a
    /// function with no outcome says so. Declined is what `/dev/devices` renders as
    /// `ahci (declined)`, and until the PR #333 review nothing between the kernel's mapping and
    /// that text was tested.
    #[test]
    fn a_pci_record_carries_its_drivers_outcome() {
        init_global_heap();
        let (r, _, _) = booted();
        let claimed = Outcome::Claimed { driver: "ahci", signal: Signal::Msi { vector: 0x32 } };
        let declined = Outcome::Declined { driver: "nvme", why: "no namespace" };
        let recs = r
            .records(|d| match address(d) {
                (0, 0, 0x1f, 2) => Some(claimed),
                (0, 0, 0x02, 0) => Some(declined),
                _ => None,
            })
            .unwrap();
        assert_eq!(recs[1].outcome, OUTCOME_CLAIMED);
        assert_eq!(text(&recs[1].driver, 4), "ahci");
        assert_eq!(recs[2].outcome, OUTCOME_DECLINED);
        assert_eq!(text(&recs[2].driver, 4), "nvme", "the driver that declined it");
        assert_eq!(recs[0].outcome, OUTCOME_NONE);
        assert_eq!(recs[0].driver[0], 0, "no driver, no name");
    }

    /// **A USB device's record is filled from its entry**, its node being a bare `Other` with the
    /// zero descriptor (Phase 6 Part A.3): the USB IDs where a PCI function's go, its class triple,
    /// its port and speed, its name and its driver — and **its parent is the controller's function,
    /// found by the controller's address**, not the host bridge its own zero address would match.
    /// A controller the table does not hold gives no parent.
    #[test]
    fn a_usb_record_carries_its_ids_port_speed_and_name_under_its_controller() {
        init_global_heap();
        let (mut r, _, _) = booted();
        let at3 = pci_at(0, 3, 0);
        let xhci = ResourceDescriptor {
            identity: DeviceIdentity { class: 0x0c, subclass: 0x03, prog_if: 0x30, ..at3.identity },
            ..at3
        };
        assert!(r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, xhci, BlockGeometry::ZERO).unwrap())));
        let usb =
            || adopt(DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap());
        let mut name = [0u8; MAX_DEVICE_NAME];
        name[..18].copy_from_slice(b"QEMU USB Keyboard ");
        let kbd = UsbFacts { vendor: 0x0627, product: 0x0001, class: (3, 1, 1), port: 9, speed: 3, name, name_len: 17 };
        assert_eq!(r.add_usb(usb(), &xhci, "xhci", kbd), Some(10), "its id is its place");
        let elsewhere = UsbFacts { port: 2, ..kbd };
        assert_eq!(r.add_usb(usb(), &pci_at(0, 9, 0), "xhci", elsewhere), Some(11));

        let recs = r.records(|_| None).unwrap();
        let k = &recs[10];
        assert_eq!(DeviceKind::from_u32(k.kind), DeviceKind::UsbDevice);
        assert_eq!((k.class, k.served, k.parent), (DeviceClass::Other as u32, NOT_SERVED, 9));
        assert_eq!((k.vendor, k.device), (0x0627, 0x0001));
        assert_eq!((k.pci_class, k.subclass, k.prog_if), (3, 1, 1));
        assert_eq!((k.port, k.speed), (9, 3));
        assert_eq!(text(&k.name, k.name_len), "QEMU USB Keyboard");
        assert_eq!(text(&k.driver, 4), "xhci");
        assert_eq!(k.outcome, OUTCOME_NONE, "an outcome is a PCI function's");
        assert_eq!(recs[11].parent, NO_PARENT, "no controller at 00:09.0");
        assert_eq!((recs[8].port, recs[8].speed), (0, 0), "zero for every other kind");
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
        let generation = u64::from_le_bytes(bytes[16..24].try_into().unwrap());
        assert_eq!(generation, 9, "one change per node registered (Phase 6 Part C)");
    }

    /// A booted table with two USB devices under an xHCI at 00:03.0: a keyboard on port 9 with its
    /// keyboard node, then a mouse on port 10 with its mouse node. The USB devices' ids, and the
    /// input nodes'.
    fn with_usb(r: &mut Registry) -> (u32, u32, u32, u32) {
        let at3 = pci_at(0, 3, 0);
        let xhci = ResourceDescriptor {
            identity: DeviceIdentity { class: 0x0c, subclass: 0x03, prog_if: 0x30, ..at3.identity },
            ..at3
        };
        assert!(r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, xhci, BlockGeometry::ZERO).unwrap())));
        let usb =
            || adopt(DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap());
        let name = [0; MAX_DEVICE_NAME];
        let facts = UsbFacts { vendor: 0x0627, product: 1, class: (3, 1, 1), port: 9, speed: 3, name, name_len: 0 };
        let kbd = r.add_usb(usb(), &xhci, "xhci", facts).unwrap();
        r.add_input(char_node(), DeviceKind::Keyboard, Some(kbd), "usb-hid").unwrap();
        let mouse = r.add_usb(usb(), &xhci, "xhci", UsbFacts { port: 10, class: (3, 1, 2), ..facts }).unwrap();
        r.add_input(char_node(), DeviceKind::Mouse, Some(mouse), "usb-hid").unwrap();
        (kbd, kbd + 1, mouse, mouse + 1)
    }

    /// **A departure takes its children, and nothing else** (Phase 6 Part C): the USB keyboard's
    /// keyboard node with it, the mouse beside it and every boot device untouched. The records stay,
    /// marked rather than omitted, so ids still name places. One change, so one generation; a second
    /// departure of the same device changes nothing.
    #[test]
    fn a_departure_takes_its_children_and_nothing_else() {
        init_global_heap();
        let (mut r, _, _) = booted();
        let (kbd, kbd_node, mouse, mouse_node) = with_usb(&mut r);
        let before = r.generation();
        let count = r.records(|_| None).unwrap().len();
        assert!(r.depart(kbd));
        assert_eq!(r.generation(), before + 1, "one change for the device and its node");
        let recs = r.records(|_| None).unwrap();
        assert_eq!(recs.len(), count, "marked, not omitted");
        let departed: KVec<u32> = {
            let mut v = KVec::new();
            for x in recs.iter().filter(|x| x.flags & DEPARTED != 0) {
                v.try_push(x.id).unwrap();
            }
            v
        };
        assert_eq!(&departed[..], &[kbd, kbd_node]);
        assert!(r.node(mouse).is_some() && r.node(mouse_node).is_some(), "the mouse beside it stays");
        assert!(!r.depart(kbd), "departed already");
        assert_eq!(r.generation(), before + 1);
        assert!(!r.depart(99), "an id the table does not hold");
    }

    /// **A departed device's paths refuse it, and its served index is never reissued** (Phase 6 Part
    /// C): `/dev/input/raw/2` and `/dev/registry/<id>` answer nothing once the keyboard has gone.
    /// With the mouse gone too — the highest index, which is the case a reissue would show in —
    /// the next keyboard takes 4, not 3 and not 2.
    #[test]
    fn a_departed_device_is_found_by_no_path_and_its_index_stays_taken() {
        init_global_heap();
        let (mut r, _, _) = booted();
        let (kbd, kbd_node, mouse, _) = with_usb(&mut r);
        assert!(r.input(2).is_some() && r.node(kbd_node).is_some());
        assert!(r.has(DeviceKind::UsbDevice));
        r.depart(kbd);
        assert!(r.input(2).is_none(), "/dev/input/raw/2");
        assert!(r.node(kbd).is_none() && r.node(kbd_node).is_none(), "/dev/registry/<id>");
        assert!(r.input(3).is_some(), "the mouse still answers");
        r.depart(mouse);
        assert!(!r.has(DeviceKind::UsbDevice), "what the hardware report and the i8042 ask");
        assert!(r.has(DeviceKind::Keyboard), "the i8042's keyboard is still here");
        assert_eq!(r.add_input(char_node(), DeviceKind::Keyboard, None, "usb-hid"), Some(4), "not 3, nor 2");
        // A block device the same way, for Part D: the partition departs with its disk.
        let (mut b, _, _) = booted();
        b.depart(3);
        assert!(b.block(0).is_none() && b.block(2).is_none(), "the disk and its partition");
        assert!(b.block(1).is_some(), "the RAM disk is no child of the disk");
        // **And two levels down** (PR #361 review): Part D's USB disk is a device, its disk and its
        // partition. The controller stands in for the device here, the partition its grandchild.
        let (mut c, _, _) = booted();
        c.depart(1);
        assert!(c.block(0).is_none() && c.block(2).is_none(), "the controller's disk, and that disk's partition");
        assert!(c.block(1).is_some() && c.node(0).is_some() && c.node(2).is_some(), "and nothing else");
    }

    /// **A disk under a USB device names the device as its parent** (Phase 6 Part D), and its
    /// partition the disk: a departure of the device takes both.
    #[test]
    fn a_disk_under_a_usb_device_departs_with_it() {
        init_global_heap();
        let (mut r, _, _) = booted();
        let (kbd, ..) = with_usb(&mut r);
        let stick = block(ResourceDescriptor::ZERO, BlockKind::Disk, b"QEMU QEMU HARDDISK", 1 << 15);
        let at = r.len() as u32;
        assert!(r.add_block_under(stick.clone(), kbd, "usb-storage"));
        let part = block(ResourceDescriptor::ZERO, BlockKind::Partition, b"partition 1 (unlabelled)", 1 << 14);
        assert!(r.add_block_child(part, &stick, "mbr"));
        let recs = r.records(|_| None).unwrap();
        assert_eq!((recs[at as usize].parent, recs[at as usize + 1].parent), (kbd, at));
        r.depart(kbd);
        assert!(r.node(at).is_none() && r.node(at + 1).is_none(), "the disk and its partition depart with it");
    }

    /// **The boot disk is flagged, once, and nothing else is** (Phase 6 Part D): the disk's record
    /// carries `BOOT` and its partition's does not, the flag is one change, and flagging it again,
    /// or a node the table does not hold, changes nothing.
    #[test]
    fn the_boot_disk_is_flagged_once_and_nothing_else() {
        init_global_heap();
        let (mut r, disk, _) = booted();
        let before = r.generation();
        assert!(r.mark_boot(&disk));
        assert_eq!(r.generation(), before + 1, "one change");
        let recs = r.records(|_| None).unwrap();
        let flagged: KVec<u32> = {
            let mut v = KVec::new();
            for x in recs.iter().filter(|x| x.flags & BOOT != 0) {
                v.try_push(x.id).unwrap();
            }
            v
        };
        assert_eq!(&flagged[..], &[3], "the disk, not its partition");
        assert!(!r.mark_boot(&disk), "flagged already");
        assert!(!r.mark_boot(&char_node()), "a node the table does not hold");
        assert_eq!(r.generation(), before + 1);
    }

    /// **Every change counts once**: each registration, and a departure with its children.
    #[test]
    fn the_generation_counts_every_change() {
        init_global_heap();
        let mut r = Registry::new();
        assert_eq!(r.generation(), 0);
        r.add_char(char_node(), DeviceKind::Keyboard, 0, "i8042");
        r.add_char(char_node(), DeviceKind::Mouse, 1, "i8042");
        assert_eq!(r.generation(), 2);
        r.depart(0);
        assert_eq!(r.generation(), 3);
    }

    /// **A waiting read is answered by the change past its generation, and not before** (Phase 6
    /// Part C), and the slots run out at four: a fifth is handed back to be refused.
    #[test]
    fn a_waiting_read_is_answered_past_its_generation_and_not_before() {
        let mut w: Waiters<char> = Waiters::new();
        assert!(w.park(5, 'a').is_ok());
        assert!(w.park(7, 'b').is_ok());
        assert_eq!(w.take_passed(5), [None; WAITERS_MAX], "5 is not past 5");
        assert_eq!(w.take_passed(6), [Some('a'), None, None, None]);
        assert_eq!(w.take_passed(7), [None; WAITERS_MAX], "taken once");
        assert_eq!(w.take_passed(9), [None, Some('b'), None, None]);
        for c in ['c', 'd', 'e', 'f'] {
            assert!(w.park(1, c).is_ok());
        }
        assert_eq!(w.park(1, 'g'), Err('g'), "a fifth");
    }

    /// **A change read shorter than its answer is refused, one that is behind is answered at once,
    /// and a fifth waiting is refused** (PR #361 review): `io-operation.md`'s two refusals, through
    /// the read itself. Seven bytes and eight, either side of the bound.
    #[test]
    fn a_change_read_is_refused_short_answered_behind_and_refused_a_fifth_place() {
        init_global_heap();
        let waiting = SpinLock::new(LockRank::Leaf, Waiters::new());
        let generation = AtomicU64::new(5);
        let page =
            crate::drivers::adopt(MemoryObject::try_new(crate::mm::PAGE_SIZE).unwrap(), KObjectType::MemoryObject);
        let po = || {
            crate::drivers::adopt(crate::object::PendingOperation::try_new().unwrap(), KObjectType::PendingOperation)
        };
        let read = |po: &ObjectRef, offset, len| read_changes(&waiting, &generation, &page, po, 0, offset, len);

        let behind = po();
        assert_eq!(read(&behind, 4, 7), Err(KError::InvalidArgument), "shorter than its answer");
        assert!(!crate::sched::pending_op_is_signaled(behind.as_ptr()));
        assert_eq!(read(&behind, 4, 8), Ok(()));
        assert_eq!(crate::sched::pending_op_completion(behind.as_ptr()), (0, 8), "behind, so answered at once");

        let ahead: [ObjectRef; WAITERS_MAX] = core::array::from_fn(|_| po());
        for p in &ahead {
            assert_eq!(read(p, 5, 8), Ok(()));
            assert!(!crate::sched::pending_op_is_signaled(p.as_ptr()), "5 is not past 5: it waits");
        }
        assert_eq!(read(&po(), 5, 8), Err(KError::WouldBlock), "a fifth place to wait");
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
