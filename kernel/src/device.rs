//! The kernel device table: every [`DeviceNode`] the kernel has.
//!
//! Architecture-independent: the table is seeded by PCI(e) enumeration ([`crate::pci`]; on aarch64
//! it would be a Device Tree Blob) and drivers append what they publish after it: disks, their
//! partitions, the RAM disk, the console, the i8042's keyboard and mouse, and the USB devices the
//! hub thread enumerates. Each entry is an owning reference, so a registered node lives for the
//! kernel's lifetime, a USB device that leaves included.
//!
//! **A device that leaves is departed, not removed**: an id is a record's place, and
//! `/dev/registry/<id>`, `device-mgr`'s `usb-<id>` and an owner's `Departed` all name a device by
//! it. A departed entry keeps its fields and its served index, which no later device takes; its
//! paths stop resolving; and its children depart with it. **Every change bumps the table's
//! generation**, which the snapshot carries and `/dev/registry/changes` answers a waiting read
//! with, so a reader that waits past the generation of the snapshot it holds misses nothing.
//!
//! **What the table knows that a node does not** is kept beside it: the node's kind, which its
//! `DeviceClass` is too coarse to say (the console and both i8042 nodes are all `Char`), **the
//! index its path serves it at**, and the node it belongs to. `/dev/blk` and `/dev/input/raw`
//! resolve through that served index, and `/dev/registry` reports it, so the two cannot disagree. A
//! USB device's IDs, class, port, speed and name are kept there too, since its node is a bare
//! `Other`.
//!
//! [`DeviceNode`]: crate::object::DeviceNode

use core::fmt;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::libkern::block::{BlockKind, MAX_DEVICE_NAME};
use crate::libkern::device::{
    BOOT, CacheCounts, DEPARTED, DeviceKind, DeviceRecord, IO_MAGIC, IO_VERSION, IoHeader, IoRecord, MAX_DRIVER_NAME,
    NO_PARENT, NOT_SERVED, OUTCOME_CLAIMED, OUTCOME_DECLINED, OUTCOME_NONE, REGISTRY_MAGIC, REGISTRY_VERSION,
    RegistryHeader,
};
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, KVec, SpinLock};
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
    /// For a USB device, what its record carries that its node cannot say.
    usb: Option<UsbFacts>,
    /// The device has left: its record says so, and its paths answer `NotFound`.
    departed: bool,
    /// **The disk the machine started from**: its GPT's GUID is the one Limine
    /// loaded the modules from.
    boot: bool,
}

/// **What a USB device's record carries** that its node does not. The node is a
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
    /// Bumped once by every change: a registration, or a departure with its children.
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

    /// The table's generation: how many changes it has seen.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// **Depart node `id` and every node whose parent chain reaches it**: a USB
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

    /// **The records under `id` still present**, in table order: a disk's partitions, which a
    /// rescan departs one by one and publishes again. `None` if memory ran out.
    pub fn children(&self, id: u32) -> Option<KVec<u32>> {
        let mut out = KVec::new();
        for (i, e) in self.entries.iter().enumerate() {
            if e.parent == id && i as u32 != id && !e.departed {
                out.try_push(i as u32).ok()?;
            }
        }
        Some(out)
    }

    /// **Flag `disk` as the disk the machine started from**. One change, so one
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
        let parent =
            self.entries.iter().position(|e| e.node.as_ptr() == parent.as_ptr()).map_or(NO_PARENT, |i| i as u32);
        self.add_block_under(node, parent, driver)
    }

    /// **Append a block node under the entry `parent`**, by id: a USB disk under its `UsbDevice`
    /// record, whose address names no PCI function.
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
    pub fn add_char(&mut self, node: ObjectRef, kind: DeviceKind, served: u32, driver: &'static str) -> bool {
        self.push(Entry { node, kind, served, parent: NO_PARENT, driver, usb: None, departed: false, boot: false })
    }

    /// Append a keyboard or mouse a USB device provides, under `parent`, the
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
        self.push(Entry { node, kind, served, parent, driver, usb: None, departed: false, boot: false })
            .then_some(served)
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

    /// The block node `/dev/blk/<index>` serves. **None for a departed one**.
    pub fn block(&self, index: u32) -> Option<ObjectRef> {
        self.entries
            .iter()
            .find(|e| !e.departed && node_of(e).class() == DeviceClass::Block && e.served == index)
            .map(|e| e.node.clone())
    }

    /// **The live block node with the lowest served index from `index` on**, and that index: a
    /// walk of every block device, which a departed one's index, never served again, cannot end.
    pub fn block_from(&self, index: u32) -> Option<(u32, ObjectRef)> {
        self.entries
            .iter()
            .filter(|e| !e.departed && node_of(e).class() == DeviceClass::Block && e.served >= index)
            .min_by_key(|e| e.served)
            .map(|e| (e.served, e.node.clone()))
    }

    /// The input node `/dev/input/raw/<index>` serves. **None for a departed one**.
    pub fn input(&self, index: u32) -> Option<ObjectRef> {
        self.entries
            .iter()
            .find(|e| !e.departed && matches!(e.kind, DeviceKind::Keyboard | DeviceKind::Mouse) && e.served == index)
            .map(|e| e.node.clone())
    }

    /// Node `id` — its place in the table. **None for a departed one**.
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

impl Registry {
    /// **Every disk's and RAM disk's counters**, a record each in table order,
    /// departed ones flagged — a partition has none, its I/O being its disk's.
    pub fn io_records(&self) -> Result<KVec<IoRecord>, AllocError> {
        let mut out: KVec<IoRecord> = KVec::new();
        for (id, e) in self.entries.iter().enumerate() {
            if !matches!(e.kind, DeviceKind::Disk | DeviceKind::RamDisk) {
                continue;
            }
            let flags = if e.departed { DEPARTED as u32 } else { 0 };
            out.try_push(node_of_ref(&e.node).io().record_for(id as u32, flags))?;
        }
        Ok(out)
    }
}

/// The bytes `/dev/registry/io` serves: an [`IoHeader`] carrying the page cache's counters and the
/// count, then the records. `records` is [`Registry::io_records`]; `cache` the page cache's.
pub fn io_bytes(records: &[IoRecord], cache: CacheCounts) -> Result<KVec<u8>, AllocError> {
    let header = IoHeader {
        magic: IO_MAGIC,
        version: IO_VERSION,
        count: records.len() as u32,
        record_size: core::mem::size_of::<IoRecord>() as u32,
        cache,
    };
    let mut out: KVec<u8> = KVec::new();
    out.try_reserve(core::mem::size_of::<IoHeader>() + core::mem::size_of_val(records))?;
    out.try_extend_from_slice(header.as_bytes())?;
    for r in records.iter() {
        out.try_extend_from_slice(r.as_bytes())?;
    }
    Ok(out)
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
        Ok(node) => *CHANGES.lock() = Some(crate::drivers::adopt_node(node)),
        Err(_) => crate::kprintln!("device: no memory for /dev/registry/changes; nothing will learn of a change"),
    }
}

/// Number of devices in the table.
pub fn count() -> usize {
    DEVICES.lock().len()
}

/// A snapshot of the device table: a cloned owning reference per device, taken under the lock and
/// returned, so a caller (driver matching) can iterate and allocate **without** holding the device
/// lock across a lock-ordering boundary.
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
/// table takes ownership of `node`. Its id (the `<id>` of `/dev/registry/<id>` and of
/// `device-mgr`'s `usb-<id>`), or `None` when the table could not take it. From the hub thread,
/// never a DPC: the table allocates while locked.
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

/// **Append a disk a USB device provides**, under its device's record `parent`. Its
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
/// could not take it. From the hub thread.
pub fn register_input(node: ObjectRef, kind: DeviceKind, parent: Option<u32>, driver: &'static str) -> Option<u32> {
    let served = DEVICES.lock().add_input(node, kind, parent, driver);
    if served.is_none() {
        crate::kprintln!("device: table full; dropping a {:?}", kind);
    }
    announce();
    served
}

/// **Depart node `id` and every node whose parent chain reaches it** — a USB
/// device and its keyboards and mice — as one change, and answer the reads waiting on
/// `/dev/registry/changes`. From the hub thread, never a DPC: the answers complete operations.
pub fn depart(id: u32) {
    if DEVICES.lock().depart(id) {
        announce();
    }
}

/// **The records under `id` still present** — a disk's partitions — for a rescan.
/// Empty if memory ran out, which a rescan then says.
pub fn children(id: u32) -> KVec<u32> {
    DEVICES.lock().children(id).unwrap_or_default()
}

/// **Flag `disk` as the disk the machine started from**, as one change. Whether
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

/// [`Registry::block_from`]: the next live block device from `index` on, and its index.
pub fn find_block_device_from(index: usize) -> Option<(usize, ObjectRef)> {
    DEVICES.lock().block_from(index as u32).map(|(i, node)| (i as usize, node))
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

/// The node `/dev/registry/changes` serves, or `None` if it could not be made.
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
    DEVICES.lock().snapshot_bytes(|d| outcomes.iter().find(|(a, _)| *a == address(d)).map(|&(_, o)| o))
}

/// What `/dev/registry/io` serves: the page cache's counters and every disk's.
pub fn io_snapshot() -> Result<KVec<u8>, AllocError> {
    let records = DEVICES.lock().io_records()?;
    let s = crate::object::file_object::fill_stats::snapshot();
    let cache = CacheCounts {
        fills: s.fills,
        fill_ns: s.fill_ns,
        hole_fills: s.hole_fills,
        hits: s.hits,
        writeback_pages: s.writeback_pages,
        writeback_irps: s.writeback_irps,
        writeback_ns: s.writeback_ns,
        fill_pages: s.fill_pages,
    };
    io_bytes(&records, cache)
}

// --- `/dev/registry/changes` -----------------------------------
//
// **How a reader learns that the table changed**: a char node whose `Read` waits until the table's
// generation is past the read's `offset`, then answers with the generation, eight bytes. A reader
// that is behind is answered at once. The device manager reads a snapshot, then waits past its
// generation, so a change between the two answers the wait at once and none is missed. This is the
// async model as it is (`sys_io_submit`, a `PendingOperation`, `sys_wait`) and needs no way for the
// kernel to name the manager, which a notification would have.

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
    // SAFETY: `po` pins the request's `PendingOperation`.
    unsafe { crate::sched::complete_pending_op(po.as_ptr(), 0, 8) };
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

/// [`changes_read`] against `waiting` and `generation`: the statics in a boot, and a host test's
/// own, so the read's two refusals are held through the read rather than through [`Waiters`] alone.
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

// --- What the drivers did with each function -----------------
//
// A driver that matched a controller and gave it up is a different finding from no driver matching
// at all ("ahci declined: no SATA disk" points at port detection, "no driver" at the match
// table), so drivers report both, and the table keeps them beside the functions they describe.

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
static OUTCOMES: SpinLock<KVec<(Address, Outcome)>> = SpinLock::new(LockRank::Registry, KVec::new());

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
mod tests;
