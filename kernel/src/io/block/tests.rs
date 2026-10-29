use super::*;
use crate::mm::test_support::init_global_heap;

#[test]
fn partition_rebase_maps_relative_to_absolute() {
    let start = 2048u64;
    let count = 1000u64;
    let ss = 512u64;
    assert_eq!(partition_rebase(0, 512, start, count, ss), Some(2048 * 512));
    assert_eq!(partition_rebase(8192, 512, start, count, ss), Some(2048 * 512 + 8192));
    assert_eq!(partition_rebase((count - 1) * ss, ss, start, count, ss), Some((start + count - 1) * ss));
    assert_eq!(partition_rebase((count - 1) * ss, ss + 1, start, count, ss), None);
    assert_eq!(partition_rebase(count * ss, ss, start, count, ss), None);
}

/// A window refuses a rescan itself, and once retired refuses everything with
/// `PeerClosed`.
#[test]
fn a_window_refuses_a_rescan_and_once_retired_everything() {
    use core::sync::atomic::{AtomicBool, Ordering};
    fn disk_submit(_: *mut Irp, _: *mut ()) {}
    fn disk_poll(_: *mut ()) {}
    let p = Partition {
        disk: BlockBackend { submit: disk_submit, poll: disk_poll, ctx: core::ptr::null_mut(), max_frags: 8 },
        start_lba: 2048,
        block_count: 100,
        sector_size: 512,
        retired: AtomicBool::new(false),
        entering: core::sync::atomic::AtomicU32::new(0),
    };
    let answer = |op: IrpOp| window_refusal(&p, op as u32);
    assert_eq!(answer(IrpOp::Read), None, "a read goes down");
    assert_eq!(answer(IrpOp::Write), None);
    assert_eq!(answer(IrpOp::Flush), None, "and a flush");
    assert_eq!(answer(IrpOp::Rescan), Some(KError::Unsupported), "a rescan is the disk's alone");
    assert_eq!(answer(IrpOp::Eject), Some(KError::Unsupported), "and an eject");
    p.retired.store(true, Ordering::Release);
    for op in [IrpOp::Read, IrpOp::Write, IrpOp::Flush, IrpOp::Rescan, IrpOp::Eject] {
        assert_eq!(answer(op), Some(KError::PeerClosed), "retired: {op:?}");
    }
}

/// The window's own submit and retirement, driven through `partition_submit`:
/// a read reaches the disk rebased and a rescan does not; once retired,
/// nothing more reaches the disk and no submit is left counted in.
#[test]
fn a_window_node_refuses_a_rescan_and_once_retired_forwards_nothing() {
    let _exclusive = crate::dpc::exclusive();
    use crate::libkern::block::BlockKind;
    use crate::object::device_node::{BlockGeometry, ResourceDescriptor};
    use core::cell::RefCell;
    init_global_heap();
    // The global queue's reserve, as the boot makes it: a refusal completes through a DPC.
    /// What reached the disk: each IRP's op and disk-absolute offset.
    struct Seen(RefCell<std::vec::Vec<(u32, u64)>>);
    fn disk_submit(irp: *mut Irp, ctx: *mut ()) {
        // SAFETY: `ctx` is the test's `Seen`; `irp` is the test's live IRP.
        unsafe { (*(ctx as *const Seen)).0.borrow_mut().push(((*irp).op, (*irp).offset)) };
    }
    fn disk_poll(_: *mut ()) {}
    let seen = Seen(RefCell::new(std::vec::Vec::new()));
    let adopt = |node: KBox<DeviceNode>| {
        // SAFETY: `into_raw` yields the single creation reference, which the test adopts.
        unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(node).as_ptr() as *mut (),
                crate::libkern::handle::KObjectType::DeviceNode,
            )
        }
    };
    let geometry = |blocks| BlockGeometry { logical_block_size: 512, block_count: blocks };
    let backend =
        BlockBackend { submit: disk_submit, poll: disk_poll, ctx: &seen as *const Seen as *mut (), max_frags: 8 };
    let disk = adopt(
        DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry(8192), BlockKind::Disk, b"disk", backend).unwrap(),
    );
    let window = Partition::new(&disk, 2048, 100, 512).unwrap();
    let wb = partition_backend(window);
    let part = adopt(
        DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry(100), BlockKind::Partition, b"p", wb).unwrap(),
    );
    // An IRP through the partition node's backend; returns its status after
    // any refusal's completion has run.
    let submit = |op: IrpOp, offset: u64| {
        let mut irp = Irp::new_block(op, core::ptr::null(), offset, 512, IrpBuffer::NONE, core::ptr::null_mut(), 0);
        // SAFETY: `part` pins the node; its backend's context is the leaked window.
        let b = unsafe { &*(part.as_ptr() as *const DeviceNode) }.block_backend().unwrap();
        (b.submit)(&mut irp, b.ctx);
        crate::dpc::run_pending();
        irp.status
    };
    let pending = crate::io::irp::IrpStatus::Pending as i32;
    assert_eq!(submit(IrpOp::Read, 512), pending, "forwarded, for the disk to complete");
    assert_eq!(seen.0.borrow().as_slice(), &[(IrpOp::Read as u32, 2049 * 512)], "rebased to the disk");
    assert_eq!(submit(IrpOp::Rescan, 0), KError::Unsupported as i32, "a rescan is the disk's alone");
    assert_eq!(submit(IrpOp::Eject, 0), KError::Unsupported as i32, "and an eject");
    assert_eq!(seen.0.borrow().len(), 1, "and neither reached the disk");

    assert!(!retire_window(&disk), "a disk's node is no window");
    assert!(retire_window(&part));
    for op in [IrpOp::Read, IrpOp::Write, IrpOp::Flush, IrpOp::Rescan] {
        assert_eq!(submit(op, 0), KError::PeerClosed as i32, "retired: {op:?}");
    }
    assert_eq!(seen.0.borrow().len(), 1, "nothing more reached the disk");
    assert!(!window_busy(&part), "every submit counted out");
    assert!(!window_busy(&disk));
}

/// Page-cache frames go to the device as one IRP, in order; more than it takes
/// is refused `InvalidArgument` before anything reaches it.
#[test]
fn frames_go_to_the_device_as_one_irp_and_more_than_it_takes_are_refused() {
    use crate::libkern::block::BlockKind;
    use crate::libkern::handle::KObjectType;
    use crate::object::PendingOperation;
    use crate::object::device_node::{BlockGeometry, ResourceDescriptor};
    use core::cell::RefCell;
    init_global_heap();
    /// Each IRP that reached the device, with its fragments.
    struct Seen(RefCell<std::vec::Vec<(*mut Irp, std::vec::Vec<PhysFrag>)>>);
    fn disk_submit(irp: *mut Irp, ctx: *mut ()) {
        // SAFETY: `ctx` is the test's `Seen`; `irp` is a live IRP whose buffer describes `count`
        // fragments.
        unsafe {
            let b = (*irp).buffer;
            let frags = core::slice::from_raw_parts(b.frags as *const PhysFrag, b.count as usize).to_vec();
            (*(ctx as *const Seen)).0.borrow_mut().push((irp, frags));
        }
    }
    fn disk_poll(_: *mut ()) {}
    let seen = Seen(RefCell::new(std::vec::Vec::new()));
    let backend =
        BlockBackend { submit: disk_submit, poll: disk_poll, ctx: &seen as *const Seen as *mut (), max_frags: 8 };
    let geometry = BlockGeometry { logical_block_size: 512, block_count: 8192 };
    let node =
        DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"disk", backend).unwrap();
    // SAFETY: `into_raw` yields each single creation reference, which the test adopts.
    let (disk, po) = unsafe {
        (
            ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode),
            ObjectRef::from_raw(
                KBox::into_raw(PendingOperation::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::PendingOperation,
            ),
        )
    };
    let frames: std::vec::Vec<PhysAddr> = (0..9).map(|i| PhysAddr(0x10_0000 + i * 0x3000)).collect();
    let page = PAGE_SIZE as u64;
    let write = |frames: &[PhysAddr], length| {
        dispatch_block_irp_into_frames(&disk, frames, disk.clone(), &po, IoOpcode::Write, 4096, length)
    };
    assert_eq!(write(&frames, 9 * page), Err(KError::InvalidArgument), "nine frames, eight fragments");
    assert_eq!(write(&frames[..8], 9 * page), Err(KError::InvalidArgument), "a length past the frames");
    assert!(seen.0.borrow().is_empty(), "nothing reached the device");
    write(&frames[..8], 8 * page).unwrap();
    let (irp, frags) = seen.0.borrow_mut().pop().unwrap();
    let expected: std::vec::Vec<PhysFrag> = frames[..8].iter().map(|f| PhysFrag { base: f.0, len: page }).collect();
    assert_eq!(frags, expected, "a fragment a frame, in order");
    // SAFETY: the IRP is live until it is completed below.
    unsafe { assert_eq!(((*irp).op, (*irp).offset, (*irp).length), (IrpOp::Write as u32, 4096, 8 * page)) };
    // Completed as the device would complete it, and its box reclaimed.
    // SAFETY: the IRP of a box `dispatch_block_irp_into_frames` made, completed once.
    unsafe { finish(irp) };
    reclaim_completed();
}

/// **A block request off its device's blocks reaches no driver**, through both dispatches.
#[test]
fn a_block_request_off_its_devices_blocks_reaches_no_driver() {
    let _exclusive = crate::dpc::exclusive();
    use crate::libkern::block::BlockKind;
    use crate::libkern::handle::KObjectType;
    use crate::object::device_node::{BlockGeometry, ResourceDescriptor};
    use crate::object::{MemoryObject, PendingOperation};
    use core::cell::RefCell;
    init_global_heap();
    /// Each IRP that reached the device.
    struct Seen(RefCell<std::vec::Vec<*mut Irp>>);
    fn disk_submit(irp: *mut Irp, ctx: *mut ()) {
        // SAFETY: `ctx` is the test's `Seen`.
        unsafe { (*(ctx as *const Seen)).0.borrow_mut().push(irp) };
    }
    fn disk_poll(_: *mut ()) {}
    let seen = Seen(RefCell::new(std::vec::Vec::new()));
    let backend =
        BlockBackend { submit: disk_submit, poll: disk_poll, ctx: &seen as *const Seen as *mut (), max_frags: 8 };
    let geometry = BlockGeometry { logical_block_size: 512, block_count: 64 };
    let node =
        DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"disk", backend).unwrap();
    // SAFETY: `into_raw` yields each single creation reference, which the test adopts.
    let adopt = |p: *mut (), kind| unsafe { ObjectRef::from_raw(p, kind) };
    let disk = adopt(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode);
    let buffer = adopt(
        KBox::into_raw(MemoryObject::try_new(2 * PAGE_SIZE).unwrap()).as_ptr() as *mut (),
        KObjectType::MemoryObject,
    );
    let po = || {
        adopt(KBox::into_raw(PendingOperation::try_new().unwrap()).as_ptr() as *mut (), KObjectType::PendingOperation)
    };
    let frames = [PhysAddr(0x10_0000), PhysAddr(0x20_0000)];
    // Each request through both entry points, its operation's status once settled.
    let both = |offset: u64, length: u64| {
        let (a, b) = (po(), po());
        let r = dispatch_block_irp(&disk, &buffer, &a, IoOpcode::Read, offset, 0, length);
        let s = dispatch_block_irp_into_frames(&disk, &frames, disk.clone(), &b, IoOpcode::Write, offset, length);
        crate::dpc::run_pending();
        reclaim_completed();
        // SAFETY: each pins its live `PendingOperation`; a single-threaded test stands in for `SCHED`.
        let status = |p: &ObjectRef| unsafe {
            PendingOperation::already_signaled(p.as_ptr()).then(|| PendingOperation::status(p.as_ptr()))
        };
        ((r, status(&a)), (s, status(&b)))
    };
    let refused = (Err(KError::InvalidArgument), None);
    for (offset, length) in [(0, 100), (100, 512), (0, 600), (1000, 1024)] {
        assert_eq!(both(offset, length), (refused, refused), "part of a block: ({offset}, {length})");
    }
    let past = (Ok(()), Some(KError::InvalidArgument as i32));
    let span = 64 * 512;
    for (offset, length) in [(span - 512, 1024), (span, 512), (u64::MAX - 511, 512)] {
        assert_eq!(both(offset, length), (past, past), "past the end: ({offset}, {length})");
    }
    assert!(seen.0.borrow().is_empty(), "nothing reached the device");
    let last = (Ok(()), None);
    assert_eq!(both(span - 512, 512), (last, last), "the last block goes down, for the device to complete");
    for irp in seen.0.borrow_mut().drain(..) {
        // SAFETY: each the IRP of a box a dispatch made, completed once.
        unsafe { finish(irp) };
    }
    reclaim_completed();
}

/// **A blocking read succeeds only once its device completed it.**
#[test]
fn a_blocking_read_its_device_did_not_complete_fails() {
    let _exclusive = crate::dpc::exclusive();
    use crate::libkern::block::BlockKind;
    use crate::libkern::handle::KObjectType;
    use crate::object::device_node::{BlockGeometry, ResourceDescriptor};
    init_global_heap();
    fn kept(_: *mut Irp, _: *mut ()) {}
    fn completes(irp: *mut Irp, _: *mut ()) {
        // SAFETY: the test's live IRP, this backend's to complete.
        unsafe {
            crate::io::irp::complete(irp, crate::io::irp::IrpStatus::Success as i32, (*irp).length);
        }
    }
    fn idle(_: *mut ()) {}
    fn runs(_: *mut ()) {
        crate::dpc::run_pending();
    }
    let read = |submit: fn(*mut Irp, *mut ()), poll: fn(*mut ()), lba| {
        let backend = BlockBackend { submit, poll, ctx: core::ptr::null_mut(), max_frags: 8 };
        let geometry = BlockGeometry { logical_block_size: 512, block_count: 64 };
        let node =
            DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"disk", backend).unwrap();
        // SAFETY: `into_raw` yields the single creation reference, which the test adopts.
        let disk = unsafe { ObjectRef::from_raw(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode) };
        read_blocking(&disk, lba, 1, &mut [0u8; 512])
    };
    assert!(read(completes, runs, 0), "a read its device completed");
    assert!(!read(kept, runs, 0), "a command its device kept");
    assert!(!read(completes, idle, 64), "past the end, refused through a DPC no poll ran");
    crate::dpc::run_pending();
    reclaim_completed();
}

#[test]
fn frags_cover_aligned_range_one_per_page() {
    init_global_heap();
    let frames = [PhysAddr(0x1000), PhysAddr(0x5000), PhysAddr(0x9000)];
    let frags = build_frags(&frames, 0, 3 * PAGE_SIZE as u64).unwrap();
    assert_eq!(frags.len(), 3);
    assert_eq!(frags[0], PhysFrag { base: 0x1000, len: PAGE_SIZE as u64 });
    assert_eq!(frags[1], PhysFrag { base: 0x5000, len: PAGE_SIZE as u64 });
    assert_eq!(frags[2], PhysFrag { base: 0x9000, len: PAGE_SIZE as u64 });
}

#[test]
fn frags_split_unaligned_head_and_tail() {
    init_global_heap();
    let frames = [PhysAddr(0x1000), PhysAddr(0x2000)];
    let frags = build_frags(&frames, 256, 512).unwrap();
    assert_eq!(frags.len(), 1);
    assert_eq!(frags[0], PhysFrag { base: 0x1000 + 256, len: 512 });
    let frags = build_frags(&frames, PAGE_SIZE as u64 - 100, 200).unwrap();
    assert_eq!(frags.len(), 2);
    assert_eq!(frags[0], PhysFrag { base: 0x1000 + PAGE_SIZE as u64 - 100, len: 100 });
    assert_eq!(frags[1], PhysFrag { base: 0x2000, len: 100 });
}

/// One fragment per page, with **no ceiling of its own**, which is why the
/// caller must check the device's: `sys_io_submit` bounds the range only
/// against the buffer's size.
#[test]
fn the_fragment_count_grows_with_the_transfer_and_is_never_clamped() {
    init_global_heap();
    let frames: KVec<PhysAddr> = {
        let mut v = KVec::new();
        v.try_reserve(600).unwrap();
        for i in 0..600u64 {
            v.try_push(PhysAddr(0x10_0000 + i * PAGE_SIZE as u64)).unwrap();
        }
        v
    };
    for pages in [1usize, 248, 249, 600] {
        let frags = build_frags(&frames, 0, pages as u64 * PAGE_SIZE as u64).unwrap();
        assert_eq!(frags.len(), pages, "{pages} pages should be {pages} fragments");
    }
}

/// An unaligned start costs **one extra fragment**: why the dispatch check
/// counts *fragments*, not a byte ceiling.
#[test]
fn an_unaligned_start_costs_an_extra_fragment() {
    init_global_heap();
    let frames: KVec<PhysAddr> = {
        let mut v = KVec::new();
        v.try_reserve(4).unwrap();
        for i in 0..4u64 {
            v.try_push(PhysAddr(0x10_0000 + i * PAGE_SIZE as u64)).unwrap();
        }
        v
    };
    let two_pages = 2 * PAGE_SIZE as u64;
    assert_eq!(build_frags(&frames, 0, two_pages).unwrap().len(), 2);
    assert_eq!(build_frags(&frames, 1, two_pages).unwrap().len(), 3);
}

/// **More refused requests than the DPC queue holds, between two drains, each complete**: every
/// IRP queued its own DPC, so the 65th refusal before a drain panicked the kernel.
#[test]
fn more_refusals_than_the_dpc_queue_holds_each_complete() {
    let _exclusive = crate::dpc::exclusive();
    use crate::libkern::block::BlockKind;
    use crate::libkern::handle::KObjectType;
    use crate::object::device_node::{BlockGeometry, ResourceDescriptor};
    use crate::object::{MemoryObject, PendingOperation};
    init_global_heap();
    fn disk_submit(_: *mut Irp, _: *mut ()) {}
    fn disk_poll(_: *mut ()) {}
    let backend = BlockBackend { submit: disk_submit, poll: disk_poll, ctx: core::ptr::null_mut(), max_frags: 8 };
    let geometry = BlockGeometry { logical_block_size: 512, block_count: 64 };
    let node =
        DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"disk", backend).unwrap();
    // SAFETY: `into_raw` yields each single creation reference, which the test adopts.
    let adopt = |p: *mut (), kind| unsafe { ObjectRef::from_raw(p, kind) };
    let disk = adopt(KBox::into_raw(node).as_ptr() as *mut (), KObjectType::DeviceNode);
    let buffer =
        adopt(KBox::into_raw(MemoryObject::try_new(PAGE_SIZE).unwrap()).as_ptr() as *mut (), KObjectType::MemoryObject);
    let pos: std::vec::Vec<ObjectRef> = (0..100)
        .map(|_| {
            adopt(
                KBox::into_raw(PendingOperation::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::PendingOperation,
            )
        })
        .collect();
    for po in &pos {
        assert_eq!(dispatch_block_irp(&disk, &buffer, po, IoOpcode::Read, 64 * 512, 0, 512), Ok(()));
    }
    // SAFETY: each pins its live `PendingOperation`; a single-threaded test stands in for `SCHED`.
    let settled = |po: &ObjectRef| unsafe {
        PendingOperation::already_signaled(po.as_ptr()).then(|| PendingOperation::status(po.as_ptr()))
    };
    crate::dpc::run_pending();
    reclaim_completed();
    for po in &pos {
        assert_eq!(settled(po), Some(KError::InvalidArgument as i32));
    }
}
