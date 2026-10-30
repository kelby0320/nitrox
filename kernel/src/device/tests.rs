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

/// The boot's order, which `drivers::probe` sets: PCI functions (the host bridge at 00:00.0
/// first, as on every PC), a disk on the AHCI controller, a RAM disk (published before the GPT
/// pass, so it can be scanned like a disk), the disk's partition from that pass, then the
/// console and the i8042's keyboard and mouse.
fn booted() -> (Registry, ObjectRef, ObjectRef) {
    let mut r = Registry::new();
    assert!(r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, pci_at(0, 0, 0), BlockGeometry::ZERO).unwrap())));
    assert!(
        r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, pci_at(0, 0x1f, 2), BlockGeometry::ZERO).unwrap()))
    );
    assert!(
        r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, pci_at(0, 0x02, 0), BlockGeometry::ZERO).unwrap()))
    );
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
/// controller by address, a partition's is its disk, and the RAM disk, whose descriptor's
/// all-zero address is the host bridge's, has none. **The keyboard is served at 0 and the mouse
/// at 1** although the console, also `Char`, registered before them: an index counted within
/// `DeviceClass` would call the keyboard `input-1`.
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

/// **A USB keyboard or mouse takes the next index after every input node's**: 2 and 3 beside
/// the i8042's two, 0 and 1 on a machine without one, and 2 where the i8042 has a mouse and no
/// keyboard, where counting input nodes would give the mouse's 1.
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
/// zero descriptor: the USB IDs, class triple, port, speed, name and driver, and **its parent
/// is the controller's function, found by the controller's address**, not the host bridge its
/// own zero address would match. A controller the table does not hold gives no parent.
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
    let usb = || adopt(DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap());
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

/// The bytes are exactly the header and the records: the padding is the memory object's.
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

/// A booted table with two USB devices under an xHCI at 00:03.0: a keyboard on port 9, then a
/// mouse on port 10, each with its input node. The devices' ids, and the nodes'.
fn with_usb(r: &mut Registry) -> (u32, u32, u32, u32) {
    let at3 = pci_at(0, 3, 0);
    let xhci = ResourceDescriptor {
        identity: DeviceIdentity { class: 0x0c, subclass: 0x03, prog_if: 0x30, ..at3.identity },
        ..at3
    };
    assert!(r.add_pci(adopt(DeviceNode::try_new(DeviceClass::Other, xhci, BlockGeometry::ZERO).unwrap())));
    let usb = || adopt(DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap());
    let name = [0; MAX_DEVICE_NAME];
    let facts = UsbFacts { vendor: 0x0627, product: 1, class: (3, 1, 1), port: 9, speed: 3, name, name_len: 0 };
    let kbd = r.add_usb(usb(), &xhci, "xhci", facts).unwrap();
    r.add_input(char_node(), DeviceKind::Keyboard, Some(kbd), "usb-hid").unwrap();
    let mouse = r.add_usb(usb(), &xhci, "xhci", UsbFacts { port: 10, class: (3, 1, 2), ..facts }).unwrap();
    r.add_input(char_node(), DeviceKind::Mouse, Some(mouse), "usb-hid").unwrap();
    (kbd, kbd + 1, mouse, mouse + 1)
}

/// **A departure takes its children, and nothing else**: the USB keyboard's node with it, the
/// mouse beside it and every boot device untouched. The records stay, marked, so ids still name
/// places. One change, one generation; a second departure changes nothing.
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

#[test]
fn a_disks_children_are_its_present_partitions() {
    init_global_heap();
    let (mut r, disk, _) = booted();
    let disk_id = 3;
    assert_eq!(&r.children(disk_id).unwrap()[..], &[5], "the booted disk's one partition");
    assert!(r.children(4).unwrap().is_empty(), "the RAM disk has none");
    assert!(r.depart(5));
    assert!(r.node(disk_id).is_some(), "departing a partition leaves its disk");
    assert!(r.children(disk_id).unwrap().is_empty(), "a departed partition is no child");
    assert!(r.add_block_child(block(pci_at(0, 0x1f, 2), BlockKind::Partition, b"new", 1 << 18), &disk, "gpt"));
    let after = r.children(disk_id).unwrap();
    assert_eq!(after.len(), 1);
    assert!(after[0] > 8, "published afresh, at a new id: {}", after[0]);
}

/// **A departed device's paths refuse it, and its served index is never reissued**:
/// `/dev/input/raw/2` and `/dev/registry/<id>` answer nothing once the keyboard has gone. With
/// the mouse gone too (the highest index, the case a reissue would show in) the next keyboard
/// takes 4, not 3 and not 2.
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
    // A block device the same way: the partition departs with its disk.
    let (mut b, _, _) = booted();
    b.depart(3);
    assert!(b.block(0).is_none() && b.block(2).is_none(), "the disk and its partition");
    assert!(b.block(1).is_some(), "the RAM disk is no child of the disk");
    // **And two levels down**: a USB disk is a device, its disk and its partition. The
    // controller stands in for the device here, the partition its grandchild.
    let (mut c, _, _) = booted();
    c.depart(1);
    assert!(c.block(0).is_none() && c.block(2).is_none(), "the controller's disk, and that disk's partition");
    assert!(c.block(1).is_some() && c.node(0).is_some() && c.node(2).is_some(), "and nothing else");
}

/// **A walk of the block devices passes a departed one**, whose index is never served again:
/// the shutdown's flush walked from 0 to the first index nothing answered, so it flushed
/// nothing after a departed disk, here every disk.
#[test]
fn a_walk_of_the_block_devices_passes_a_departed_one() {
    init_global_heap();
    let (mut r, _, _) = booted();
    r.depart(3);
    let (kbd, ..) = with_usb(&mut r);
    let stick = block(ResourceDescriptor::ZERO, BlockKind::Disk, b"QEMU QEMU HARDDISK", 1 << 15);
    assert!(r.add_block_under(stick, kbd, "usb-storage"));
    let mut walked: KVec<u32> = KVec::new();
    let mut next = 0;
    while let Some((index, _)) = r.block_from(next) {
        walked.try_push(index).unwrap();
        next = index + 1;
    }
    assert_eq!(&walked[..], [1, 3], "the RAM disk and the stick, past the departed disk and partition");
}

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

/// **`/dev/registry/io`: a record per disk and RAM disk, with what its node counted**, none for
/// a partition (its disk's driver counts its I/O), a departed disk's kept and flagged. The
/// header counts the records and carries the page cache's counters.
#[test]
fn the_io_records_are_every_disks_counters_and_no_partitions() {
    init_global_heap();
    let (mut r, disk, _) = booted();
    let counted = crate::io::stats::Counted::new();
    counted.attach(&disk);
    counted.record(crate::io::irp::IrpOp::Read as u32, true, 4096, 100);
    counted.record(crate::io::irp::IrpOp::Write as u32, true, 512, 50);
    let recs = r.io_records().unwrap();
    assert_eq!(recs.iter().map(|x| x.id).collect::<std::vec::Vec<_>>(), [3, 4], "the disk and the RAM disk");
    assert_eq!((recs[0].reads, recs[0].read_bytes, recs[0].writes, recs[0].busy_ns), (1, 4096, 1, 150));
    assert_eq!((recs[1].reads, recs[1].flags), (0, 0));

    r.depart(3);
    let recs = r.io_records().unwrap();
    assert_eq!((recs[0].flags, recs[0].reads), (DEPARTED as u32, 1), "departed, its counts kept");

    let cache = CacheCounts { fills: 7, writeback_pages: 3, fill_pages: 40, ..CacheCounts::default() };
    let bytes = io_bytes(&recs, cache).unwrap();
    let word = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
    assert_eq!((word(0), word(4), word(8), word(12)), (IO_MAGIC, IO_VERSION, 2, 64));
    assert_eq!(bytes.len(), 80 + 2 * 64);
    assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), 7, "the cache's fills");
    assert_eq!(u64::from_le_bytes(bytes[72..80].try_into().unwrap()), 40, "and the pages they brought in");
    assert_eq!(&bytes[80..144], recs[0].as_bytes());
}

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
/// and a fifth waiting is refused** (`io-operation.md`'s two refusals), through the read
/// itself. Seven bytes and eight, either side of the bound.
#[test]
fn a_change_read_is_refused_short_answered_behind_and_refused_a_fifth_place() {
    init_global_heap();
    let waiting = SpinLock::new(LockRank::Leaf, Waiters::new());
    let generation = AtomicU64::new(5);
    let page =
        // SAFETY: a fresh `MemoryObject`, adopted under its own tag.
        unsafe { crate::drivers::adopt(MemoryObject::try_new(crate::mm::PAGE_SIZE).unwrap(), KObjectType::MemoryObject) };
    let po = || {
        // SAFETY: a fresh `PendingOperation`, adopted under its own tag.
        unsafe {
            crate::drivers::adopt(crate::object::PendingOperation::try_new().unwrap(), KObjectType::PendingOperation)
        }
    };
    let read = |po: &ObjectRef, offset, len| read_changes(&waiting, &generation, &page, po, 0, offset, len);

    let behind = po();
    assert_eq!(read(&behind, 4, 7), Err(KError::InvalidArgument), "shorter than its answer");
    // SAFETY: `behind` is a live `PendingOperation` reference held across the call.
    assert!(!unsafe { crate::sched::pending_op_is_signaled(behind.as_ptr()) });
    assert_eq!(read(&behind, 4, 8), Ok(()));
    // SAFETY: `behind` is a live `PendingOperation` reference held across the call.
    let answered = unsafe { crate::sched::pending_op_completion(behind.as_ptr()) };
    assert_eq!(answered, (0, 8), "behind, so answered at once");

    let ahead: [ObjectRef; WAITERS_MAX] = core::array::from_fn(|_| po());
    for p in &ahead {
        assert_eq!(read(p, 5, 8), Ok(()));
        // SAFETY: `p` is a live `PendingOperation` reference held across the call.
        assert!(!unsafe { crate::sched::pending_op_is_signaled(p.as_ptr()) }, "5 is not past 5: it waits");
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
    let intx = Outcome::Claimed { driver: "ahci", signal: Signal::Intx { gsi: 16, vector: 0x40 } };
    assert_eq!(
        format!("{}", OutcomeLine { desc: &ahci, outcome: Some(intx) }),
        "drivers: 00:1f.2 claimed by ahci, INTx GSI 16 vec 0x40"
    );
    let declined = Outcome::Declined { driver: "ahci", why: "no SATA disk on any implemented port" };
    assert_eq!(
        format!("{}", OutcomeLine { desc: &ahci, outcome: Some(declined) }),
        "drivers: 00:1f.2 declined by ahci: no SATA disk on any implemented port"
    );
    assert_eq!(format!("{}", OutcomeLine { desc: &at(0, 0x14, 0), outcome: None }), "drivers: 00:14.0 no driver");
}
