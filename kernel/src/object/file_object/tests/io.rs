use super::*;

fn po() -> Ref<PendingOperation> {
    Ref::new(PendingOperation::try_new().unwrap())
}

fn read_one(f: &FileObject, index: usize, block_size: u32) -> u64 {
    match f.begin_fill(index, PhysAddr(0), block_size, 0, &po(), &mut KVec::new()) {
        ReadStep::Go(block, 1) => block,
        ReadStep::Hole => 0,
        other => panic!("{other:?}"),
    }
}

fn two_block_file(reg: &Ref<UserspaceServerReg>, id: u64) -> Ref<FileObject> {
    let f = resolve(reg, id, 2 * PAGE_SIZE);
    let mut runs = KVec::new();
    runs.try_push(BlockRun { file_block: 0, device_lba: 100, length: 2, flags: 0 }).unwrap();
    f.resize(2 * PAGE_SIZE, runs).unwrap();
    fill(&f, 0, 0xA0);
    fill(&f, 1, 0xA1);
    f
}

/// **A page of blocks a grow just allocated fills as zero without a read, and is written back
/// to them**: one lookup for both would read the flag as a hole and skip the write. A page
/// holding one block the server did not promise is read whole.
#[test]
fn a_page_a_grow_just_allocated_fills_as_zero_without_a_read_and_is_written_back() {
    init_global_heap();
    let reg = registration();
    let run = |file_block, device_lba, length, flags| BlockRun { file_block, device_lba, length, flags };
    // Four-KiB blocks: page 0 the file's old block, pages 1 and 2 a grow's.
    let f = resolve(&reg, 7, 3 * PAGE_SIZE);
    let mut runs = KVec::new();
    runs.try_push(run(0, 100, 1, 0)).unwrap();
    runs.try_push(run(1, 101, 2, RUN_UNWRITTEN)).unwrap();
    f.resize(3 * PAGE_SIZE, runs).unwrap();
    assert_eq!(read_one(&f, 0, 4096), 100, "the old block is read");
    f.end_io();
    assert_eq!(read_one(&f, 1, 4096), 0, "a grown block is not");
    assert_eq!(read_one(&f, 2, 4096), 0);
    fill(&f, 1, 0xB1);
    assert!(matches!(f.begin_write(1, 4096, 1, &mut KVec::new()), WriteStep::Go(101, 1)), "but is written back");
    f.end_io();

    // One-KiB blocks, four to a page: page 0 holds the old block and three of the grow's, page 1
    // four of the grow's.
    let g = FileObject::cache_in(&reg, candidate_in(&reg, 8, 2 * PAGE_SIZE, 1024)).unwrap();
    let mut runs = KVec::new();
    runs.try_push(run(0, 200, 1, 0)).unwrap();
    runs.try_push(run(1, 201, 7, RUN_UNWRITTEN)).unwrap();
    g.resize(2 * PAGE_SIZE, runs).unwrap();
    assert_eq!(read_one(&g, 0, 1024), 200, "a page with one old block is read whole");
    g.end_io();
    assert_eq!(read_one(&g, 1, 1024), 0, "a page of grown blocks is not");

    // Grown blocks beside a hole in one page read nothing; the flag alone does not make a hole.
    let mut runs = KVec::new();
    runs.try_push(run(0, 300, 2, RUN_UNWRITTEN)).unwrap();
    runs.try_push(run(2, 0, 2, 0)).unwrap();
    assert_eq!(fill_block_in(&runs, 0, 4), 0);
    assert_eq!(device_block_in(&runs, 1), 301, "a write-back's lookup ignores the flag");
}

fn mapped_file(
    reg: &Ref<UserspaceServerReg>,
    id: u64,
    pages: usize,
    block_size: u32,
    map: &[(u64, u64, u32)],
    resident: impl Iterator<Item = usize>,
) -> (Ref<FileObject>, std::collections::BTreeMap<usize, PhysAddr>) {
    let f = FileObject::cache_in(reg, candidate_in(reg, id, pages * PAGE_SIZE, block_size)).unwrap();
    let mut runs = KVec::new();
    for &(file_block, device_lba, length) in map {
        runs.try_push(BlockRun { file_block, device_lba, length, flags: 0 }).unwrap();
    }
    f.resize(pages * PAGE_SIZE, runs).unwrap();
    let frames = resident.map(|i| (i, fill(&f, i, i as u8))).collect();
    (f, frames)
}

type Irp = (std::vec::Vec<PhysAddr>, u64, u64);

fn write_runs(f: &FileObject, block_size: u32, max: usize) -> (WriteBack, std::vec::Vec<Irp>) {
    let mut irps = std::vec::Vec::new();
    let w = f.write_runs(block_size, max, |frames, offset, length| {
        irps.push((frames.to_vec(), offset, length));
        0
    });
    (w, irps)
}

fn irp(
    frames: &std::collections::BTreeMap<usize, PhysAddr>,
    pages: core::ops::Range<usize>,
    block: u64,
    block_size: u64,
) -> Irp {
    let length = (pages.len() * PAGE_SIZE) as u64;
    (pages.map(|i| frames[&i]).collect(), block * block_size, length)
}

#[test]
fn a_write_back_sends_a_run_of_pages_an_irp_up_to_the_devices_limit() {
    init_global_heap();
    let reg = registration();
    let (f, frames) = mapped_file(&reg, 7, 100, 4096, &[(0, 1000, 100)], (0..100).rev());
    let (w, irps) = write_runs(&f, 4096, 64);
    assert_eq!(w, WriteBack::Written);
    assert_eq!(irps, [irp(&frames, 0..64, 1000, 4096), irp(&frames, 64..100, 1064, 4096)]);
    let (_, irps) = write_runs(&f, 4096, RUN_MAX_PAGES);
    assert_eq!(irps, [irp(&frames, 0..100, 1000, 4096)]);
    assert!(matches!(f.forget(&po()), Forgotten::Now), "nothing in flight");
}

#[test]
fn a_run_ends_at_a_gap_on_the_device_a_hole_a_missing_or_filling_page_and_the_end() {
    init_global_heap();
    let reg = registration();
    // Pages 0-9 at device block 1000, 10-19 at 2000, 20-23 a hole, 24-39 at 3000.
    let map = [(0, 1000, 10), (10, 2000, 10), (20, 0, 4), (24, 3000, 16)];
    let (f, mut frames) = mapped_file(&reg, 7, 40, 4096, &map, (0..40).filter(|&i| i != 30 && i != 33));
    let fo = &f;
    assert!(matches!(fo.reserve(33), Reserve::New(_)), "page 33 filling");
    // The file cut to 38 pages, and pages 38 and 39 faulted in again past its end.
    let mut runs = KVec::new();
    for &(file_block, device_lba, length) in &map {
        runs.try_push(BlockRun { file_block, device_lba, length, flags: 0 }).unwrap();
    }
    fo.resize(38 * PAGE_SIZE, runs).unwrap();
    frames.insert(38, fill(fo, 38, 38));
    frames.insert(39, fill(fo, 39, 39));
    let (w, irps) = write_runs(fo, 4096, 64);
    assert_eq!(w, WriteBack::Written);
    assert_eq!(
        irps,
        [
            irp(&frames, 0..10, 1000, 4096),
            irp(&frames, 10..20, 2000, 4096),
            irp(&frames, 24..30, 3000, 4096),
            irp(&frames, 31..33, 3007, 4096),
            irp(&frames, 34..38, 3010, 4096),
        ]
    );
}

#[test]
fn a_forget_or_a_refusal_mid_write_back_stops_it_at_the_next_run() {
    init_global_heap();
    let reg = registration();
    let (f, _) = mapped_file(&reg, 7, 100, 4096, &[(0, 1000, 100)], 0..100);
    let fo = &f;
    let (mut issued, mut forgotten) = (0, None);
    let w = fo.write_runs(4096, 10, |_, _, _| {
        issued += 1;
        if issued == 2 {
            forgotten = Some(fo.forget(&po()));
        }
        0
    });
    assert_eq!((w, issued), (WriteBack::Written, 2));
    assert!(matches!(forgotten, Some(Forgotten::Later(_))), "the second run was in flight");
    assert!(matches!(fo.forget(&po()), Forgotten::Now), "and its end answered it");

    for (id, status, outcome) in [
        (8, crate::syscall::error::KError::PeerClosed as i32, WriteBack::Gone),
        (9, crate::syscall::error::KError::InvalidArgument as i32, WriteBack::Failed),
    ] {
        let (g, _) = mapped_file(&reg, id, 100, 4096, &[(0, 2000, 100)], 0..100);
        let mut issued = 0;
        let w = g.write_runs(4096, 10, |_, _, _| {
            issued += 1;
            if issued == 2 { status } else { 0 }
        });
        assert_eq!((w, issued), (outcome, 2));
        assert!(matches!(g.forget(&po()), Forgotten::Now), "the refused run ended");
    }
}

#[test]
fn a_run_of_pages_in_one_kib_blocks_is_one_irp_while_the_device_is_contiguous() {
    init_global_heap();
    let reg = registration();
    let (f, frames) = mapped_file(&reg, 7, 8, 1024, &[(0, 400, 32)], 0..8);
    assert_eq!(write_runs(&f, 1024, 64).1, [irp(&frames, 0..8, 400, 1024)]);
    let (g, frames) = mapped_file(&reg, 8, 8, 1024, &[(0, 400, 16), (16, 900, 16)], 0..8);
    assert_eq!(write_runs(&g, 1024, 64).1, [irp(&frames, 0..4, 400, 1024), irp(&frames, 4..8, 900, 1024)]);
}

#[test]
fn a_run_holds_the_devices_fragments_at_most_run_max_pages_and_at_least_one() {
    assert_eq!(run_pages(Some(64)), 64);
    assert_eq!(run_pages(Some(248)), 248);
    assert_eq!(run_pages(Some(RUN_MAX_PAGES as u32)), RUN_MAX_PAGES);
    assert_eq!(run_pages(Some(RUN_MAX_PAGES as u32 + 1)), RUN_MAX_PAGES);
    assert_eq!(run_pages(Some(u32::MAX)), RUN_MAX_PAGES);
    assert_eq!(run_pages(Some(0)), 1);
    assert_eq!(run_pages(None), 1);
}

fn miss(f: &FileObject, index: usize) -> (PhysAddr, Ref<PendingOperation>) {
    match f.reserve_fault(index) {
        Fault::Fill(frame, po) => (frame, po),
        _ => panic!("page {index} was not a miss"),
    }
}

fn fill_ahead(f: &FileObject, index: usize, block_size: u32, ahead: usize) -> (ReadStep, std::vec::Vec<PhysAddr>) {
    let (frame, fill_po) = miss(f, index);
    let mut frames = KVec::new();
    let step = f.begin_fill(index, frame, block_size, ahead, &fill_po, &mut frames);
    let pages = match step {
        ReadStep::Go(_, pages) => {
            f.end_io();
            pages
        }
        _ => 1,
    };
    for k in 0..pages {
        f.settle(index + k, &fill_po, true);
    }
    (step, frames.to_vec())
}

/// A run's end in bytes fits a `u64`, but where the page after it would lie need not.
#[test]
fn a_page_that_would_lie_past_the_top_of_the_device_ends_a_run() {
    init_global_heap();
    let reg = registration();
    let d = u64::MAX / 512 - 10;
    let map = [(0, d, 8), (8, d + 8, 1), (16, 7000, 8)];
    let (f, _) = mapped_file(&reg, 7, 3, 512, &map, 0..0);
    assert_eq!(fill_ahead(&f, 0, 512, 31).0, ReadStep::Go(d, 2));
    let (g, _) = mapped_file(&reg, 8, 3, 512, &map, 0..3);
    assert!(matches!(g.begin_write(0, 512, 3, &mut KVec::new()), WriteStep::Go(at, 2) if at == d));
}

#[test]
fn a_fill_reads_ahead_in_one_irp_until_the_device_or_the_file_stops_it() {
    init_global_heap();
    let reg = registration();
    let (f, _) = mapped_file(&reg, 7, 100, 4096, &[(0, 1000, 40), (40, 5000, 60)], 0..0);
    let fo = &f;
    let (frame, fill_po) = miss(fo, 0);
    let mut frames = KVec::new();
    assert_eq!(fo.begin_fill(0, frame, 4096, 31, &fill_po, &mut frames), ReadStep::Go(1000, 32));
    assert_eq!(frames[0], frame, "the faulted page's frame first");
    for (k, &fr) in frames.iter().enumerate().skip(1) {
        assert_eq!(fo.lookup(k), Some((fr, PageState::Loading)), "page {k} reserved into its frame");
    }
    assert!(
        matches!(fo.reserve_fault(5), Fault::Wait(w) if w.as_ptr() == fill_po.as_ptr()),
        "a fault on one waits on the fill"
    );
    assert!(matches!(fo.forget(&po()), Forgotten::Later(_)), "one IRP in flight");
    let (f, _) = mapped_file(&reg, 8, 100, 4096, &[(0, 1000, 40), (40, 5000, 64)], 0..0);
    let fo = &f;
    assert_eq!(fill_ahead(fo, 0, 4096, 31).0, ReadStep::Go(1000, 32));
    assert_eq!(fo.lookup(31).map(|(_, s)| s), Some(PageState::Ready), "settled with the faulted page");
    assert_eq!(fill_ahead(fo, 32, 4096, 31).0, ReadStep::Go(1032, 8), "to the jump on the device");
    assert_eq!(fill_ahead(fo, 40, 4096, 31).0, ReadStep::Go(5000, 32));
    assert_eq!(fill_ahead(fo, 96, 4096, 31).0, ReadStep::Go(5056, 4), "to the file's end");
    assert!(matches!(fo.forget(&po()), Forgotten::Now), "each fill one IRP, each ended");
}

#[test]
fn read_ahead_stops_at_a_resident_page_blocks_that_read_as_zero_and_the_maps_end() {
    init_global_heap();
    let reg = registration();
    let f = resolve(&reg, 7, 40 * PAGE_SIZE);
    let run = |file_block, device_lba, length, flags| BlockRun { file_block, device_lba, length, flags };
    let mut runs = KVec::new();
    for r in [run(0, 1000, 20, 0), run(20, 1020, 4, RUN_UNWRITTEN), run(24, 0, 4, 0), run(28, 1028, 10, 0)] {
        runs.try_push(r).unwrap();
    }
    let fo = &f;
    fo.resize(40 * PAGE_SIZE, runs).unwrap();
    fill(fo, 5, 5);
    assert!(matches!(fo.reserve(9), Reserve::New(_)), "page 9 filling, by another path");
    assert_eq!(fill_ahead(fo, 0, 4096, 31).0, ReadStep::Go(1000, 5), "to the ready page 5");
    assert_eq!(fill_ahead(fo, 6, 4096, 31).0, ReadStep::Go(1006, 3), "to the loading page 9");
    assert_eq!(fill_ahead(fo, 10, 4096, 31).0, ReadStep::Go(1010, 10), "to the grown blocks");
    assert_eq!(fill_ahead(fo, 20, 4096, 31).0, ReadStep::Hole, "which fill without a read");
    assert_eq!(fill_ahead(fo, 24, 4096, 31).0, ReadStep::Hole, "as a hole does");
    assert_eq!(fill_ahead(fo, 28, 4096, 31).0, ReadStep::Go(1028, 10), "to the map's end");
    assert_eq!(fill_ahead(fo, 38, 4096, 31).0, ReadStep::Hole, "past it");
    let (g, _) = mapped_file(&reg, 8, 10, 4096, &[(0, 2000, 10)], 0..0);
    assert_eq!(fill_ahead(&g, 0, 4096, 0).0, ReadStep::Go(2000, 1), "nothing ahead");
}

#[test]
fn read_ahead_in_one_kib_blocks_and_never_for_a_forgotten_file() {
    init_global_heap();
    let reg = registration();
    let (f, _) = mapped_file(&reg, 7, 16, 1024, &[(0, 400, 64)], 0..0);
    assert_eq!(fill_ahead(&f, 0, 1024, 31).0, ReadStep::Go(400, 16));
    let (g, _) = mapped_file(&reg, 8, 16, 1024, &[(0, 400, 64)], 0..0);
    let _ = g.forget(&po());
    let (step, frames) = fill_ahead(&g, 0, 1024, 31);
    assert_eq!((step, frames.len()), (ReadStep::Hole, 0));
    assert_eq!(g.resident_pages(), 1, "nothing reserved ahead");
}

#[test]
fn a_fill_reads_its_page_and_those_ahead_in_one_irp_within_the_devices_limit() {
    use crate::io::block::BlockBackend;
    use crate::io::irp::{Irp, PhysFrag};
    use crate::libkern::block::BlockKind;
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
    let geometry = BlockGeometry { logical_block_size: 512, block_count: 1 << 20 };
    let node =
        DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"disk", backend).unwrap();
    let device = Ref::new(node);
    let reg = registration();
    let producer = Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id: 7 };
    let cand = FileObject::try_new(20 * PAGE_SIZE, producer).unwrap();
    let cand = Ref::new(cand);
    let f = FileObject::cache_in(&reg, cand).unwrap();
    let mut runs = KVec::new();
    runs.try_push(BlockRun { file_block: 0, device_lba: 1000, length: 20, flags: 0 }).unwrap();
    f.resize(20 * PAGE_SIZE, runs).unwrap();

    let (frame, fill_po) = miss(&f, 0);
    assert_eq!(f.start_fill(&f, 0, frame, &fill_po, true), FillStart::DeviceIo(8));
    let (irp, frags) = seen.0.borrow_mut().pop().expect("one IRP");
    assert!(seen.0.borrow().is_empty());
    // SAFETY: the IRP is live until it is completed below.
    let (op, offset, length) = unsafe { ((*irp).op, (*irp).offset, (*irp).length) };
    assert_eq!((op, offset, length), (crate::io::irp::IrpOp::Read as u32, 1000 * 4096, 8 * PAGE_SIZE as u64));
    let expected: std::vec::Vec<u64> = (0..8).map(|i| f.lookup(i).unwrap().0.as_u64()).collect();
    assert_eq!(frags.iter().map(|fr| fr.base).collect::<std::vec::Vec<_>>(), expected, "a fragment a page, in order");
    assert_eq!(expected[0], frame.as_u64());
    // SAFETY: as above; the device's side of a completion, a success of the whole length.
    unsafe { ((*irp).status, (*irp).transferred) = (0, length) };
    // SAFETY: `irp` is the recording device's in-flight IRP.
    unsafe { crate::io::block::complete_for_test(irp) };
    let completed = crate::sched::pending_op_completion(&fill_po);
    assert_eq!(completed, (0, length), "the fault's PO completed");

    let (frame, fill_po) = miss(&f, 8);
    assert_eq!(f.start_fill(&f, 8, frame, &fill_po, false), FillStart::DeviceIo(1));
    let (irp, frags) = seen.0.borrow_mut().pop().expect("one IRP");
    // SAFETY: the IRP is live until it is completed below.
    let (offset, length) = unsafe { ((*irp).offset, (*irp).length) };
    assert_eq!((offset, length, frags.len()), (1008 * 4096, PAGE_SIZE as u64, 1), "page 8 alone");
    assert_eq!(f.lookup(9), None, "nothing reserved after it");
    // SAFETY: as above.
    unsafe { ((*irp).status, (*irp).transferred) = (0, length) };
    // SAFETY: `irp` is the recording device's in-flight IRP.
    unsafe { crate::io::block::complete_for_test(irp) };
    crate::io::block::reclaim_completed();
}

/// A fill or write-back wait answers what its I/O came to, the process exiting or not.
#[test]
fn a_fill_wait_answers_the_fill_not_the_exit() {
    init_global_heap();
    let po = PendingOperation::try_new().unwrap();
    let po = Ref::new(po);
    crate::sched::complete_pending_op(&po, 0, 0);
    let answers =
        crate::sched::with_current(crate::sched::an_ending_thread(), || (wait_for_fill(&po), wait_status(&po)));
    assert_eq!(answers, (true, 0));
}

struct Held(core::cell::RefCell<std::vec::Vec<*mut crate::io::irp::Irp>>);

fn held_disk_file(held: &Held) -> Ref<FileObject> {
    use crate::io::block::BlockBackend;
    use crate::io::irp::Irp;
    use crate::libkern::block::BlockKind;
    fn disk_submit(irp: *mut Irp, ctx: *mut ()) {
        // SAFETY: `ctx` is the test's `Held`.
        unsafe { (*(ctx as *const Held)).0.borrow_mut().push(irp) };
    }
    fn disk_poll(_: *mut ()) {}
    let backend =
        BlockBackend { submit: disk_submit, poll: disk_poll, ctx: held as *const Held as *mut (), max_frags: 8 };
    let geometry = BlockGeometry { logical_block_size: 512, block_count: 1 << 20 };
    let node =
        DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"disk", backend).unwrap();
    let device = Ref::new(node);
    let reg = registration();
    let producer = Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id: 7 };
    let cand = FileObject::try_new(20 * PAGE_SIZE, producer).unwrap();
    let cand = Ref::new(cand);
    let f = FileObject::cache_in(&reg, cand).unwrap();
    let mut runs = KVec::new();
    runs.try_push(BlockRun { file_block: 0, device_lba: 1000, length: 20, flags: 0 }).unwrap();
    f.resize(20 * PAGE_SIZE, runs).unwrap();
    f
}

fn complete_held(
    held: &Held,
    reads: &core::cell::RefCell<std::vec::Vec<(u64, u64)>>,
    bad: u64,
    po: &Ref<PendingOperation>,
) -> bool {
    let irps: std::vec::Vec<_> = held.0.borrow_mut().drain(..).collect();
    for irp in irps {
        // SAFETY: the IRP is live until it is completed here; this is the device's side.
        unsafe {
            let (first, pages) = ((*irp).offset / 4096 - 1000, (*irp).length / PAGE_SIZE as u64);
            reads.borrow_mut().push((first, pages));
            ((*irp).status, (*irp).transferred) = if (first..first + pages).contains(&bad) {
                (crate::syscall::error::KError::IoError as i32, 0)
            } else {
                (0, (*irp).length)
            };
        }
        // SAFETY: `irp` is the recording device's in-flight IRP.
        unsafe { crate::io::block::complete_for_test(irp) };
    }
    assert!(reads.borrow().len() < 16, "a fault that never gives up");
    crate::sched::pending_op_completion(po).0 == 0
}

#[test]
fn a_failed_read_ahead_fails_only_the_faults_of_the_page_that_failed() {
    use core::cell::RefCell;
    init_global_heap();
    let held = Held(RefCell::new(std::vec::Vec::new()));
    let f = held_disk_file(&held);
    let reads = RefCell::new(std::vec::Vec::new());
    let disk = |po: &Ref<PendingOperation>| complete_held(&held, &reads, 5, po);

    let (mut page_3, mut page_5) = (None, None);
    let mut first = true;
    let mut wait_0 = |po: &Ref<PendingOperation>| -> bool {
        let ok = disk(po);
        if core::mem::take(&mut first) {
            for k in [3, 5] {
                match f.reserve_fault(k) {
                    Fault::Wait(on) => assert_eq!(on.as_ptr(), po.as_ptr(), "page {k} waits on page 0's read"),
                    _ => panic!("page {k} is not being read"),
                }
            }
            page_3 = FileObject::fault_in_page_with(&f, 3, &mut |po| disk(po), &mut || panic!());
            page_5 = FileObject::fault_in_page_with(&f, 5, &mut |po| disk(po), &mut || panic!());
        }
        ok
    };
    let page_0 = FileObject::fault_in_page_with(&f, 0, &mut wait_0, &mut || panic!());

    assert_eq!(*reads.borrow(), [(0, 8), (3, 1), (5, 1), (0, 1)], "the read ahead, then each page alone");
    for (k, got) in [(0, page_0), (3, page_3)] {
        assert!(got.is_some(), "page {k}'s fault failed for page 5's error");
        assert_eq!(f.lookup(k), got.map(|frame| (frame, PageState::Ready)));
    }
    assert_eq!(page_5, None, "page 5 does not read");
    assert_eq!(f.lookup(5), None, "and is left for the next fault to try");
    crate::io::block::reclaim_completed();
}

#[test]
fn a_fault_fails_at_its_second_failed_fill() {
    use core::cell::RefCell;
    init_global_heap();
    let held = Held(RefCell::new(std::vec::Vec::new()));
    let f = held_disk_file(&held);
    let reads = RefCell::new(std::vec::Vec::new());
    let fo = &f;

    let (frame, read_0) = miss(fo, 0);
    assert_eq!(fo.start_fill(&f, 0, frame, &read_0, true), FillStart::DeviceIo(8));
    let mut read_5 = None;
    let mut waits = 0;
    let mut wait = |po: &Ref<PendingOperation>| -> bool {
        waits += 1;
        let ok = complete_held(&held, &reads, 5, po);
        if waits == 1 {
            assert!(!ok);
            fo.finish_io();
            for k in 0..8 {
                fo.settle(k, &read_0, false);
            }
            let (frame, po) = miss(fo, 5);
            assert_eq!(fo.start_fill(&f, 5, frame, &po, true), FillStart::DeviceIo(8));
            read_5 = Some(po);
        }
        ok
    };
    assert_eq!(FileObject::fault_in_page_with(&f, 5, &mut wait, &mut || panic!()), None);
    assert_eq!(*reads.borrow(), [(0, 8), (5, 8)], "two reads, both failed, and no third");
    fo.finish_io();
    let read_5 = read_5.unwrap();
    for k in 5..13 {
        fo.settle(k, &read_5, false);
    }
    crate::io::block::reclaim_completed();
}

#[test]
fn a_forgotten_file_is_never_written_back_and_leaves_the_cache() {
    init_global_heap();
    let reg = registration();
    let a = two_block_file(&reg, 7);
    let first = a.as_ptr();
    FileObject::writable_mapped(&a);
    a.writable_unmapped();
    assert!(a.is_dirty());
    assert!(matches!(a.begin_write(0, 4096, 1, &mut KVec::new()), WriteStep::Go(100, 1)));
    assert!(a.end_io().is_none(), "nothing forgotten yet");

    assert!(matches!(reg.forget_file(7, &po()), Forgotten::Now), "nothing in flight");
    assert_eq!(reg.cached_files(), 0, "so its entry went at once");
    assert!(a.is_dead());
    assert!(!a.is_dirty(), "the pin is gone");
    assert!(matches!(a.begin_write(0, 4096, 1, &mut KVec::new()), WriteStep::Dead));
    assert_eq!(read_one(&a, 0, 4096), 0, "a fill reads a hole, not a freed block");
    assert_eq!(FileObject::touch_target(&a).map(|(_, id)| id), None, "and no touch names it");
    let again = resolve(&reg, 7, PAGE_SIZE);
    assert_ne!(again.as_ptr(), first, "a new object for the id");
    test_probe::reset();
    drop(a);
    assert_eq!(test_probe::file_object_destroys(), 1, "unpinned: it went with its user");
}

#[test]
fn a_forget_mid_write_back_is_answered_after_the_irp_in_flight() {
    init_global_heap();
    let reg = registration();
    let a = two_block_file(&reg, 7);
    let fo = &a;
    assert!(matches!(fo.begin_write(0, 4096, 1, &mut KVec::new()), WriteStep::Go(100, 1)), "page 0's IRP is issued");
    let answer = po();
    let Forgotten::Later(wait_on) = fo.forget(&answer) else { panic!("an IRP is in flight") };
    assert_eq!(wait_on.as_ptr(), answer.as_ptr());
    assert!(matches!(fo.begin_write(1, 4096, 1, &mut KVec::new()), WriteStep::Dead), "page 1 is not written");
    let Forgotten::Later(second) = fo.forget(&po()) else { panic!() };
    assert_eq!(second.as_ptr(), answer.as_ptr());
    let done = fo.end_io().expect("the last IRP ending hands the answer back");
    assert_eq!(done.as_ptr(), answer.as_ptr());
    assert!(fo.end_io().is_none(), "and only once");
}

/// **A second `Forget` through the registration waits on the first one's answer**: the first
/// must not take the entry out, or a second is answered at once with the IRP still in flight.
#[test]
fn a_second_forget_waits_on_the_first_ones_answer() {
    init_global_heap();
    let reg = registration();
    let a = two_block_file(&reg, 7);
    let fo = &a;
    assert!(matches!(fo.begin_write(0, 4096, 1, &mut KVec::new()), WriteStep::Go(100, 1)), "page 0's IRP is issued");
    let answer = po();
    let Forgotten::Later(first) = reg.forget_file(7, &answer) else { panic!("an IRP is in flight") };
    assert_eq!(first.as_ptr(), answer.as_ptr());
    let Forgotten::Later(second) = reg.forget_file(7, &po()) else {
        panic!("a second Forget found nothing to wait on")
    };
    assert_eq!(second.as_ptr(), answer.as_ptr(), "the first one's answer");
    assert_eq!(fo.end_io().map(|p| p.as_ptr()), Some(answer.as_ptr()));
    assert!(matches!(reg.forget_file(7, &po()), Forgotten::Now), "nothing in flight now");
    assert_eq!(reg.cached_files(), 0, "and the entry has gone");
}

#[test]
fn a_file_forgotten_mid_io_is_found_by_no_resolve_or_sync() {
    init_global_heap();
    let reg = registration();
    let a = two_block_file(&reg, 7);
    assert_eq!(read_one(&a, 1, 4096), 101);
    let answer = po();
    assert!(matches!(reg.forget_file(7, &answer), Forgotten::Later(_)));
    assert_eq!(reg.cache_objects().unwrap().len(), 0, "no sync sees it");
    let again = resolve(&reg, 7, PAGE_SIZE);
    assert_ne!(again.as_ptr(), a.as_ptr(), "a resolve gets a new object");
    assert!(matches!(reg.forget_file(7, &po()), Forgotten::Now), "the new file has nothing in flight");
    assert!(again.is_dead(), "and it is the one forgotten");
    assert_eq!(a.end_io().map(|p| p.as_ptr()), Some(answer.as_ptr()), "the old answer still waited for its IRP");
}

#[test]
fn a_fill_in_flight_holds_the_forget_answer() {
    init_global_heap();
    let reg = registration();
    let a = two_block_file(&reg, 7);
    let fo = &a;
    assert_eq!(read_one(fo, 1, 4096), 101);
    let answer = po();
    assert!(matches!(fo.forget(&answer), Forgotten::Later(_)));
    assert_eq!(read_one(fo, 0, 4096), 0, "no read starts after it");
    assert_eq!(fo.end_io().map(|p| p.as_ptr()), Some(answer.as_ptr()));
}

#[test]
fn the_held_count_is_what_a_sync_leaves_alive() {
    init_global_heap();
    let reg = registration();
    let count = || reg.cache_objects().unwrap().len();
    let held = two_block_file(&reg, 7);
    let dirty = two_block_file(&reg, 8);
    let gone = two_block_file(&reg, 9);
    assert_eq!(count(), 3);
    drop(gone);
    assert_eq!(count(), 2, "a file nothing holds leaves the cache");

    FileObject::writable_mapped(&dirty);
    dirty.writable_unmapped();
    let mark = dirty.clean_mark().expect("no writable mapping remains");
    let dirty_at = dirty.as_ptr();
    drop(dirty);
    assert_eq!(count(), 2, "a dirty file holds itself");
    let live = reg.cache_objects().unwrap();
    let pinned = live.iter().find(|f| f.as_ptr() == dirty_at).unwrap().clone();
    drop(live);
    FileObject::unpin_if_clean(&pinned, mark);
    drop(pinned);
    assert_eq!(count(), 1, "cleaned, it goes");

    assert_eq!(read_one(&held, 1, 4096), 101);
    let answer = po();
    assert!(matches!(reg.forget_file(7, &answer), Forgotten::Later(_)));
    assert_eq!(reg.cached_files(), 1, "the entry stays while the read is in flight");
    assert_eq!(count(), 0, "a forgotten file is not counted, though something holds it");
    assert!(held.end_io().is_some());
    drop(held);
}

/// **A write-back its device refused `PeerClosed` lets the file go**, and no other failure
/// does: [`FileObject::let_go`] leaves the file alive, so its next fill still asks its device
/// and fails, rather than reading a hole.
#[test]
fn a_write_back_refused_for_good_lets_the_file_go_and_no_other() {
    init_global_heap();
    assert_eq!(WriteBack::of(0), WriteBack::Written);
    assert_eq!(WriteBack::of(crate::syscall::error::KError::PeerClosed as i32), WriteBack::Gone);
    assert_eq!(
        WriteBack::of(crate::syscall::error::KError::IoError as i32),
        WriteBack::Failed,
        "a disk that may answer again"
    );
    let reg = registration();
    let a = two_block_file(&reg, 7);
    FileObject::writable_mapped(&a);
    a.writable_unmapped();
    assert!(a.is_dirty());
    assert_eq!(FileObject::let_go(&a), 2, "both pages resident, unwritten");
    assert!(!a.is_dirty(), "the pin is gone");
    assert!(!a.is_dead(), "but not forgotten");
    assert_ne!(read_one(&a, 1, 4096), 0, "a fill still reads its block, not a hole");
}

#[test]
fn a_forgotten_file_is_never_pinned() {
    init_global_heap();
    let reg = registration();
    let a = two_block_file(&reg, 7);
    let _ = reg.forget_file(7, &po());
    FileObject::writable_mapped(&a);
    assert!(!a.is_dirty());
    a.writable_unmapped();
}

fn fault(f: &FileObject, index: usize) -> (&'static str, Option<PhysAddr>, Option<Ref<PendingOperation>>) {
    match f.reserve_fault(index) {
        Fault::Hit(fr) => ("hit", Some(fr), None),
        Fault::Fill(fr, po) => ("fill", Some(fr), Some(po)),
        Fault::Wait(po) => ("wait", None, Some(po)),
        Fault::Busy => ("busy", None, None),
        Fault::Oom => ("oom", None, None),
    }
}

#[test]
fn a_second_faulter_waits_on_the_fill_and_either_can_settle_it() {
    init_global_heap();
    let f = fobj(2 * PAGE_SIZE);
    let ("fill", Some(frame), Some(po)) = fault(&f, 1) else { panic!("a miss fills") };
    let ("wait", None, Some(waits_on)) = fault(&f, 1) else { panic!("a second faulter waits") };
    assert_eq!(waits_on.as_ptr(), po.as_ptr(), "on the fill's own PO");
    f.settle(1, &waits_on, true);
    assert_eq!(f.lookup(1), Some((frame, PageState::Ready)));
    f.settle(1, &po, true);
    assert!(matches!(fault(&f, 1), ("hit", Some(fr), None) if fr == frame));
    let _ = f.reserve(0);
    assert!(matches!(fault(&f, 0), ("busy", None, None)));
}

#[test]
fn a_failed_fill_leaves_the_page_for_the_next_fault() {
    init_global_heap();
    let f = fobj(PAGE_SIZE);
    let ("fill", Some(_), Some(po)) = fault(&f, 0) else { panic!() };
    f.settle(0, &po, false);
    assert_eq!(f.lookup(0), None);
    let ("fill", Some(_), Some(again)) = fault(&f, 0) else { panic!("filled afresh") };
    assert_ne!(again.as_ptr(), po.as_ptr());
}

#[test]
fn a_fill_retired_mid_flight_does_not_settle_the_next_page() {
    init_global_heap();
    let f = fobj(2 * PAGE_SIZE);
    let ("fill", Some(first), Some(po1)) = fault(&f, 1) else { panic!() };
    f.resize(PAGE_SIZE, KVec::new()).unwrap(); // retires page 1, still loading
    f.resize(2 * PAGE_SIZE, KVec::new()).unwrap();
    let ("fill", Some(second), Some(po2)) = fault(&f, 1) else { panic!() };
    assert_ne!(first, second);
    f.settle(1, &po1, true); // the first fill completes
    assert_eq!(f.lookup(1), Some((second, PageState::Loading)), "the second is still filling");
    f.settle(1, &po1, false); // nor does its failure take the second page away
    assert_eq!(f.lookup(1), Some((second, PageState::Loading)));
    f.settle(1, &po2, true);
    assert_eq!(f.lookup(1), Some((second, PageState::Ready)));
}

#[test]
fn drop_frees_cached_frames_no_leak() {
    // Build a FileObject, resident-fault several pages, drop — repeated enough
    // that a frame leak would exhaust the 16 MiB test heap.
    init_global_heap();
    for _ in 0..64 {
        let f = fobj(16 * PAGE_SIZE);
        for i in 0..8 {
            assert!(matches!(f.reserve(i), Reserve::New(_)));
        }
        assert_eq!(f.resident_pages(), 8);
    }
}

#[test]
fn dispatch_destroy_runs_file_object_arm() {
    init_global_heap();
    test_probe::reset();
    let f = fobj(PAGE_SIZE);
    let _ = f.reserve(0);
    let r = ObjectRef::new(f);
    assert_eq!(test_probe::file_object_destroys(), 0);
    drop(r);
    assert_eq!(test_probe::file_object_destroys(), 1);
}
