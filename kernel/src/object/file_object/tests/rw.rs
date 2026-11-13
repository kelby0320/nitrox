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
