use super::*;
use crate::object::MemoryObject;
use crate::object::file_object::io::Source;
use crate::syscall::error::KError;

const P: u64 = PAGE_SIZE as u64;

/// A block file of `size` bytes, every page on the device but those in `holes`.
fn on_device(reg: &Ref<UserspaceServerReg>, id: u64, size: usize, holes: &[u64]) -> Ref<FileObject> {
    let mut runs = KVec::new();
    for page in (0..size.div_ceil(PAGE_SIZE) as u64).filter(|p| !holes.contains(p)) {
        runs.try_push(BlockRun { file_block: page, device_lba: 100 + page, length: 1, flags: 0 }).unwrap();
    }
    let device = DEVICE.with(Ref::clone);
    let producer = Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id: id };
    let f = Ref::new(FileObject::try_new_with_runs(size, producer, runs).unwrap());
    if id == 0 { f } else { FileObject::cache_in(reg, f).unwrap() }
}

/// The byte page `index` of a file reads as, by the faulter below.
fn byte_of(index: usize) -> u8 {
    0x40 + index as u8
}

/// Fault a page in as a fill from the device would, each byte [`byte_of`] its index, and note it.
fn fault_in(f: &Ref<FileObject>, index: usize, seen: &mut std::vec::Vec<(usize, usize)>) -> Option<PhysAddr> {
    seen.push((f.as_ptr() as usize, index));
    match f.reserve(index) {
        Reserve::Ready(p) => Some(p),
        Reserve::New(p) => {
            let g = f.inner.lock();
            g.pages.iter().find(|pg| pg.index == index).unwrap().view().fill(0, PAGE_SIZE, byte_of(index));
            drop(g);
            f.mark_ready(index);
            Some(p)
        }
        _ => None,
    }
}

fn memory(len: usize, byte: u8) -> Ref<MemoryObject> {
    let mo = Ref::new(MemoryObject::try_new(len).unwrap());
    mo.copy_in(0, &std::vec![byte; len]);
    mo
}

fn bytes(mo: &MemoryObject, at: usize, len: usize) -> std::vec::Vec<u8> {
    let mut out = std::vec![0; len];
    mo.copy_out(at, &mut out);
    out
}

#[test]
fn a_read_lands_its_bytes_from_any_offset_and_is_short_at_the_end() {
    init_global_heap();
    let reg = registration();
    let f = on_device(&reg, 0, 2 * PAGE_SIZE + 100, &[]);
    let into = memory(4 * PAGE_SIZE, 0);
    let mut seen = std::vec::Vec::new();
    let (status, done) = FileObject::read_into(&f, 100, 3 * P, &into, 10, &mut |f, i| fault_in(f, i, &mut seen));
    assert_eq!((status, done), (0, 2 * P), "short at the end");
    let got = bytes(&into, 10, done as usize);
    let want: std::vec::Vec<u8> = (100..100 + done as usize).map(|at| byte_of(at / PAGE_SIZE)).collect();
    assert_eq!(got, want);
    assert_eq!(bytes(&into, 0, 10), [0; 10], "nothing before the buffer's offset");
    let past = FileObject::read_into(&f, 3 * P, P, &into, 0, &mut |f, i| fault_in(f, i, &mut seen));
    assert_eq!(past, (0, 0));
}

#[test]
fn a_write_lands_and_a_whole_page_skips_its_fill() {
    init_global_heap();
    let reg = registration();
    let f = on_device(&reg, 5, 3 * PAGE_SIZE, &[]);
    let from = Source::Memory(memory(3 * PAGE_SIZE, 0xAA));
    let mut seen = std::vec::Vec::new();
    let written = FileObject::write_from(&f, P, 2 * P, &from, 0, &mut |f, i| fault_in(f, i, &mut seen));
    assert_eq!(written, (0, 2 * P));
    assert!(seen.is_empty(), "two whole pages, neither filled: {seen:?}");
    let part = FileObject::write_from(&f, 10, 20, &from, 0, &mut |f, i| fault_in(f, i, &mut seen));
    assert_eq!(part, (0, 20));
    assert_eq!(seen.iter().map(|&(_, i)| i).collect::<std::vec::Vec<_>>(), [0], "a part page is filled first");
    let back = memory(3 * PAGE_SIZE, 0);
    FileObject::read_into(&f, 0, 3 * P, &back, 0, &mut |f, i| fault_in(f, i, &mut seen));
    assert_eq!(bytes(&back, 0, 40), [[byte_of(0); 10], [0xAA; 10], [0xAA; 10], [byte_of(0); 10]].concat());
    assert!(bytes(&back, PAGE_SIZE, 2 * PAGE_SIZE).iter().all(|&b| b == 0xAA));
    assert!(f.is_dirty(), "a write is dirty until a flush");
}

#[test]
fn a_write_past_the_end_over_a_hole_or_of_a_file_with_no_blocks_is_refused() {
    init_global_heap();
    let reg = registration();
    let f = on_device(&reg, 6, 3 * PAGE_SIZE, &[1]);
    assert_eq!(f.writable_range(3 * P - 1, 2), Err(KError::InvalidArgument));
    assert_eq!(f.writable_range(P + 5, 1), Err(KError::Unsupported), "a hole");
    assert_eq!(f.writable_range(0, P), Ok(()));
    let from = Source::Memory(memory(3 * PAGE_SIZE, 1));
    let mut seen = std::vec::Vec::new();
    let stopped = FileObject::write_from(&f, 0, 3 * P, &from, 0, &mut |f, i| fault_in(f, i, &mut seen));
    assert_eq!(stopped, (KError::Unsupported as i32, P), "stopped at the hole, the page before it written");
    let stub = Ref::new(FileObject::try_new(PAGE_SIZE, Producer::Stub { base: 0 }).unwrap());
    assert_eq!(stub.writable_range(0, 1), Err(KError::Unsupported));
    let _ = f.forget(&Ref::new(PendingOperation::try_new().unwrap()));
    assert_eq!(f.writable_range(0, 1), Err(KError::Unsupported), "a forgotten file");
}

#[test]
fn a_copy_faults_its_sources_page_in_before_the_destinations_is_reserved() {
    init_global_heap();
    let reg = registration();
    let (src, dst) = (on_device(&reg, 7, 2 * PAGE_SIZE, &[]), on_device(&reg, 8, 2 * PAGE_SIZE, &[]));
    let mut seen = std::vec::Vec::new();
    let copied = FileObject::write_from(&dst, 0, 2 * P, &Source::File(src.clone()), 0, &mut |f, i| {
        if f.as_ptr() == src.as_ptr() {
            assert!(dst.lookup(i).is_none(), "the destination's page {i} reserved before the source's");
        }
        fault_in(f, i, &mut seen)
    });
    assert_eq!(copied, (0, 2 * P));
    let back = memory(2 * PAGE_SIZE, 0);
    FileObject::read_into(&dst, 0, 2 * P, &back, 0, &mut |f, i| fault_in(f, i, &mut seen));
    assert_eq!(bytes(&back, PAGE_SIZE - 1, 2), [byte_of(0), byte_of(1)]);
}

#[test]
fn a_write_counts_as_a_writable_mapping_so_a_write_back_beside_it_leaves_the_file_dirty() {
    init_global_heap();
    let reg = registration();
    let f = on_device(&reg, 9, PAGE_SIZE, &[]);
    let mark = f.clean_mark().unwrap();
    let mut seen = std::vec::Vec::new();
    FileObject::write_from(&f, 0, 8, &Source::Memory(memory(PAGE_SIZE, 3)), 0, &mut |f, i| {
        assert_eq!(f.writable_maps(), 1, "mapped while it runs");
        fault_in(f, i, &mut seen)
    });
    assert_eq!(f.writable_maps(), 0);
    FileObject::unpin_if_clean(&f, mark);
    assert!(f.is_dirty(), "a write-back begun before the write cleans nothing");
}

#[test]
fn a_truncate_or_a_failed_page_stops_an_op_with_the_bytes_done() {
    init_global_heap();
    let reg = registration();
    let f = on_device(&reg, 10, 3 * PAGE_SIZE, &[]);
    let into = memory(3 * PAGE_SIZE, 0);
    let mut seen = std::vec::Vec::new();
    let mut shrink = |file: &Ref<FileObject>, i| {
        if i == 1 {
            file.resize(PAGE_SIZE, KVec::new()).unwrap();
        }
        fault_in(file, i, &mut seen)
    };
    assert_eq!(FileObject::read_into(&f, 0, 3 * P, &into, 0, &mut shrink), (0, P), "short at the new end");
    let g = on_device(&reg, 11, 3 * PAGE_SIZE, &[]);
    let fail = |file: &Ref<FileObject>, i| if i == 2 { None } else { fault_in(file, i, &mut std::vec::Vec::new()) };
    let from = Source::Memory(memory(3 * PAGE_SIZE, 9));
    assert_eq!(FileObject::write_from(&g, 1, 3 * P - 2, &from, 0, &mut { fail }), (KError::IoError as i32, 2 * P - 1));
    assert_eq!(FileObject::read_into(&g, 0, 3 * P, &into, 0, &mut { fail }), (KError::IoError as i32, 2 * P));
}

#[test]
fn a_window_goes_only_with_nothing_using_its_frames_and_no_write_begun_since_its_mark() {
    init_global_heap();
    let reg = registration();
    let f = on_device(&reg, 12, 6 * PAGE_SIZE, &[]);
    let into = memory(6 * PAGE_SIZE, 0);
    let mut seen = std::vec::Vec::new();
    FileObject::read_into(&f, 0, 5 * P, &into, 0, &mut |f, i| fault_in(f, i, &mut seen));
    let mark = f.clean_mark().unwrap();
    assert_eq!(f.let_window_go(0..2, mark), 2);
    assert!(f.lookup(0).is_none() && f.lookup(2).is_some());
    f.mapped();
    assert_eq!(f.let_window_go(2..3, mark), 0, "a mapping");
    f.unmapped();
    let running = crate::object::file_object::io::Running::begin(&f);
    assert_eq!(f.let_window_go(2..3, mark), 0, "another op holding its frames");
    drop(running);
    FileObject::writable_mapped(&f);
    f.writable_unmapped();
    assert_eq!(f.let_window_go(2..3, mark), 0, "a write begun since the mark");
    let mark = f.clean_mark().unwrap();
    assert_eq!(f.let_window_go(2..3, mark), 0, "dirty");
    FileObject::unpin_if_clean(&f, mark);
    assert!(matches!(f.reserve(5), Reserve::New(_)));
    assert_eq!(f.let_window_go(2..6, mark), 3, "the ready pages, and not the one loading");
    assert!(f.lookup(5).is_some());
}

#[test]
fn a_write_back_clears_the_unwritten_mark_on_the_blocks_it_wrote_and_no_more() {
    init_global_heap();
    let run = |file_block, device_lba, length, flags| BlockRun { file_block, device_lba, length, flags };
    let mut runs = KVec::new();
    runs.try_push(run(0, 100, 4, RUN_UNWRITTEN)).unwrap();
    runs.try_push(run(4, 0, 2, 0)).unwrap();
    runs.try_push(run(6, 200, 4, RUN_UNWRITTEN)).unwrap();
    let clear = crate::object::file_object::io::clear_unwritten;
    let got = |runs: &KVec<BlockRun>| -> std::vec::Vec<_> {
        runs.iter().map(|r| (r.file_block, r.device_lba, r.length, r.flags)).collect()
    };
    let u = RUN_UNWRITTEN;
    let cleared = clear(&runs, 2, 8, 102).unwrap();
    assert_eq!(got(&cleared), [(0, 100, 2, u), (2, 102, 2, 0), (4, 0, 2, 0), (6, 200, 4, u)], "6.. is not at 106");
    let cleared = clear(&cleared, 6, 8, 200).unwrap();
    assert_eq!(got(&cleared), [(0, 100, 2, u), (2, 102, 2, 0), (4, 0, 2, 0), (6, 200, 2, 0), (8, 202, 2, u)]);
    assert!(clear(&cleared, 2, 8, 102).is_none(), "nothing left marked there");
}

#[test]
fn a_page_over_blocks_still_marked_unwritten_is_not_let_go_until_written() {
    init_global_heap();
    let reg = registration();
    let mut runs = KVec::new();
    runs.try_push(BlockRun { file_block: 0, device_lba: 100, length: 2, flags: RUN_UNWRITTEN }).unwrap();
    let device = DEVICE.with(Ref::clone);
    let producer = Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id: 13 };
    let f = Ref::new(FileObject::try_new_with_runs(2 * PAGE_SIZE, producer, runs).unwrap());
    let mut seen = std::vec::Vec::new();
    FileObject::write_from(&f, 0, 2 * P, &Source::Memory(memory(2 * PAGE_SIZE, 5)), 0, &mut |f, i| {
        fault_in(f, i, &mut seen)
    });
    let mark = f.clean_mark().unwrap();
    FileObject::unpin_if_clean(&f, mark);
    assert!(!f.is_dirty(), "clean, as a write-back leaves it");
    assert_eq!(f.let_window_go(0..2, mark), 0, "a fill would read it as zero");
    f.written(0, 1, 100);
    assert_eq!(f.let_window_go(0..2, mark), 1, "the page written back, and not the other");
}

#[test]
fn a_copys_source_is_held_so_no_other_op_lets_its_frames_go_mid_copy() {
    init_global_heap();
    let reg = registration();
    let (src, dst) = (on_device(&reg, 20, PAGE_SIZE, &[]), on_device(&reg, 21, PAGE_SIZE, &[]));
    let mut seen = std::vec::Vec::new();
    let mut gone = 0;
    let copied = FileObject::write_from(&dst, 10, 100, &Source::File(src.clone()), 0, &mut |f, i| {
        if f.as_ptr() == dst.as_ptr() {
            gone += src.let_window_go(0..1, src.clean_mark().unwrap());
        }
        fault_in(f, i, &mut seen)
    });
    assert_eq!((copied, gone), ((0, 100), 0), "the source's page stays while the copy reads it");
    let back = memory(PAGE_SIZE, 0);
    FileObject::read_into(&dst, 10, 100, &back, 0, &mut |f, i| fault_in(f, i, &mut seen));
    assert_eq!(bytes(&back, 0, 100), [byte_of(0); 100]);
}

#[test]
fn a_write_back_leaves_marked_what_a_resize_moved_while_it_wrote() {
    init_global_heap();
    let reg = registration();
    let f = on_device(&reg, 22, 2 * PAGE_SIZE, &[]);
    let mut seen = std::vec::Vec::new();
    FileObject::write_from(&f, 0, 2 * P, &Source::Memory(memory(2 * PAGE_SIZE, 1)), 0, &mut |f, i| {
        fault_in(f, i, &mut seen)
    });
    let moved = || {
        let mut runs = KVec::new();
        runs.try_push(BlockRun { file_block: 0, device_lba: 500, length: 2, flags: RUN_UNWRITTEN }).unwrap();
        runs
    };
    let wrote = f.write_runs(4096, 64, |_, _, _| {
        f.resize(0, KVec::new()).unwrap();
        f.resize(2 * PAGE_SIZE, moved()).unwrap();
        0
    });
    assert!(matches!(wrote, WriteBack::Written));
    assert!(f.inner.lock().runs.iter().all(|r| r.flags & RUN_UNWRITTEN != 0), "blocks 500.. were never written");
}

#[test]
fn a_copy_of_a_file_onto_its_own_overlapping_bytes_is_refused_and_onto_the_same_ones_is_none() {
    init_global_heap();
    let reg = registration();
    let f = on_device(&reg, 23, PAGE_SIZE, &[]);
    let mut seen = std::vec::Vec::new();
    let mut fault = |f: &Ref<FileObject>, i| fault_in(f, i, &mut seen);
    let itself = Source::File(f.clone());
    assert_eq!(FileObject::write_from(&f, 8, 64, &itself, 0, &mut fault), (KError::InvalidArgument as i32, 0));
    assert_eq!(FileObject::write_from(&f, 0, 64, &itself, 8, &mut fault), (KError::InvalidArgument as i32, 0));
    assert_eq!(FileObject::write_from(&f, 16, 64, &itself, 16, &mut fault), (0, 64), "the same range");
    assert_eq!(FileObject::write_from(&f, 64, 32, &itself, 0, &mut fault), (0, 32), "apart");
    let back = memory(PAGE_SIZE, 0);
    FileObject::read_into(&f, 0, 96, &back, 0, &mut fault);
    assert_eq!(bytes(&back, 0, 96), [byte_of(0); 96]);
}

#[test]
fn a_whole_page_with_no_frame_for_it_is_out_of_memory_and_counted_nothing() {
    init_global_heap();
    let reg = registration();
    let f = on_device(&reg, 24, PAGE_SIZE, &[]);
    let from = Source::Memory(memory(PAGE_SIZE, 2));
    crate::mm::frame::fail_frames_after(Some(0));
    let written = FileObject::write_from(&f, 0, P, &from, 0, &mut |_, _| None);
    crate::mm::frame::fail_frames_after(None);
    assert_eq!(written, (KError::OutOfMemory as i32, 0));
    assert!(f.lookup(0).is_none());
}

#[test]
fn a_read_past_the_end_reserves_nothing_and_one_cut_by_a_truncate_into_its_page_stops_short() {
    init_global_heap();
    let reg = registration();
    let f = on_device(&reg, 25, 2 * PAGE_SIZE, &[]);
    let into = memory(2 * PAGE_SIZE, 0);
    let mut seen = std::vec::Vec::new();
    assert_eq!(FileObject::read_into(&f, 2 * P, P, &into, 0, &mut |f, i| fault_in(f, i, &mut seen)), (0, 0));
    assert!(seen.is_empty() && f.lookup(2).is_none(), "no page past the end faulted in");
    let mut cut = |file: &Ref<FileObject>, i| {
        file.resize(PAGE_SIZE + 50, KVec::new()).unwrap();
        fault_in(file, i, &mut seen)
    };
    assert_eq!(FileObject::read_into(&f, P + 100, 100, &into, 0, &mut cut), (0, 0), "its byte now past the end");
}
