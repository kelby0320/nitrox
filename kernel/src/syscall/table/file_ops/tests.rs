use super::super::flush::held;
use super::*;
use crate::mm::test_support::init_global_heap;
use crate::object::device_node::{BlockGeometry, DeviceClass, ResourceDescriptor};
use crate::object::userspace_server::{FILE_OPS_MAX, FILE_OPS_PER_PROCESS, FileWork, QueuedOp};
use crate::object::{DeviceNode, IpcChannel, Producer};
use crate::sched::{pending_op_completion, pending_op_is_signaled};

pub(in crate::syscall::table) fn registration() -> Ref<UserspaceServerReg> {
    let (ep, peer) = IpcChannel::try_new_pair(4).unwrap();
    drop(peer);
    UserspaceServerReg::try_new(ep).unwrap()
}

pub(in crate::syscall::table) fn file(reg: &Ref<UserspaceServerReg>, file_id: u64) -> Ref<FileObject> {
    let device =
        Ref::new(DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap());
    let producer = Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id };
    Ref::new(FileObject::try_new(PAGE_SIZE, producer).unwrap())
}

pub(in crate::syscall::table) fn cached(reg: &Ref<UserspaceServerReg>) -> Ref<FileObject> {
    FileObject::cache_in(reg, file(reg, 7)).unwrap()
}

pub(in crate::syscall::table) fn flush(pid: u32, file: Option<Ref<FileObject>>) -> QueuedOp {
    let work = file.map_or(FileWork::FlushMount, FileWork::Flush);
    QueuedOp { work, po: Ref::new(PendingOperation::try_new().unwrap()), pid }
}

/// `Ok(true)` to run now, `Ok(false)` queued.
fn admit(reg: &Ref<UserspaceServerReg>, f: QueuedOp) -> Result<bool, KError> {
    reg.admit_op(f).map(|now| now.is_some()).map_err(|(e, _)| e)
}

/// Every place taken, by processes `100..` four flushes each: the flushes' pids.
pub(in crate::syscall::table) fn fill(reg: &Ref<UserspaceServerReg>) -> std::vec::Vec<u32> {
    let pids: std::vec::Vec<u32> = (0..FILE_OPS_MAX).map(|i| 100 + i / FILE_OPS_PER_PROCESS).collect();
    for &pid in &pids {
        assert_eq!(admit(reg, flush(pid, None)), Ok(true));
    }
    pids
}

#[test]
fn a_mount_runs_sixteen_flushes_queues_the_rest_in_turn_and_another_mount_is_its_own() {
    init_global_heap();
    let (full, other) = (registration(), registration());
    let running = fill(&full);
    assert_eq!(admit(&full, flush(200, None)), Ok(false));
    assert_eq!(admit(&full, flush(201, None)), Ok(false));
    assert_eq!(admit(&other, flush(200, None)), Ok(true), "another mount's own");
    assert_eq!(full.op_done(running[0]).map(|f| f.pid), Some(200), "first in, first run");
    assert_eq!(full.op_done(running[1]).map(|f| f.pid), Some(201));
    assert!(full.op_done(running[2]).is_none(), "none waits: the place is given back");
    assert_eq!(admit(&full, flush(202, None)), Ok(true));
    assert_eq!(admit(&full, flush(203, None)), Ok(false), "sixteen run again");
}

#[test]
fn a_process_has_at_most_four_of_a_mounts_flushes_running_or_queued() {
    init_global_heap();
    let reg = registration();
    let running = fill(&reg);
    for _ in 0..FILE_OPS_PER_PROCESS {
        assert_eq!(admit(&reg, flush(300, None)), Ok(false));
    }
    assert_eq!(admit(&reg, flush(300, None)), Err(KError::WouldBlock), "four queued");
    assert_eq!(admit(&reg, flush(running[0], None)), Err(KError::WouldBlock), "four running");
    assert_eq!(reg.op_done(running[0]).map(|f| f.pid), Some(300));
    assert_eq!(admit(&reg, flush(running[0], None)), Ok(false), "a share given back, queued behind 300's");
    assert_eq!(admit(&reg, flush(300, None)), Err(KError::WouldBlock), "one running and three queued");
}

#[test]
fn a_flush_lets_go_of_its_file_and_place_before_it_completes() {
    init_global_heap();
    let reg = registration();
    let mine = reg.admit_op(flush(400, Some(cached(&reg)))).ok().unwrap().unwrap();
    let (done, status, written, next) = finish(&reg, mine);
    assert!(!pending_op_is_signaled(&done), "not yet completed");
    assert_eq!(held(&reg), Ok(0), "its file let go");
    assert!(next.is_none());
    fill(&reg);
    assert_eq!((status, written), (0, 0), "a file with nothing to write");
}

#[test]
fn a_flushs_thread_runs_each_flush_its_place_passes_to_and_completes_each() {
    init_global_heap();
    let reg = registration();
    let running = fill(&reg);
    let (first, a, b) = (flush(running[0], None), flush(500, None), flush(501, None));
    let pos = [first.po.clone(), a.po.clone(), b.po.clone()];
    assert_eq!((admit(&reg, a), admit(&reg, b)), (Ok(false), Ok(false)));
    FileOps::run(KBox::try_new(FileOps { reg: Some(reg.clone()), first: Some(first) }).unwrap());
    assert!(pos.iter().all(|po| pending_op_is_signaled(po) && pending_op_completion(po) == (0, 0)));
    assert_eq!(admit(&reg, flush(502, None)), Ok(true), "fifteen run, the thread's place given back");
    assert_eq!(admit(&reg, flush(503, None)), Ok(false));
}

/// A block file of `pages` pages on the device, but for `holes`.
fn on_device(reg: &Ref<UserspaceServerReg>, pages: u64, holes: &[u64]) -> Ref<FileObject> {
    let mut runs = crate::libkern::KVec::new();
    for page in (0..pages).filter(|p| !holes.contains(p)) {
        runs.try_push(crate::object::BlockRun { file_block: page, device_lba: 100 + page, length: 1, flags: 0 })
            .unwrap();
    }
    let device =
        Ref::new(DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap());
    let producer = Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id: 0 };
    Ref::new(FileObject::try_new_with_runs(pages as usize * PAGE_SIZE, producer, runs).unwrap())
}

fn io(
    pid: u32,
    file: &Ref<FileObject>,
    rights: Rights,
    opcode: IoOpcode,
    buffer: RawHandle,
    at: (u64, u64, u64),
) -> SysResult {
    let op = IoOp { opcode: opcode as u32, flags: 0, buffer: buffer.0, buf_offset: at.0, offset: at.1, length: at.2 };
    submit(file.clone(), rights, &op, opcode, pid)
}

#[test]
fn a_files_read_or_write_is_refused_without_its_rights_its_range_or_a_buffer_that_holds_it() {
    const PID: u32 = 4491;
    init_global_heap();
    global::init_for_tests();
    let reg = registration();
    fill(&reg);
    let f = on_device(&reg, 2, &[1]);
    let both = Rights::MAP_READ | Rights::MAP_WRITE;
    let mo =
        |rights| global::get().install(PID, ObjectRef::new(MemoryObject::try_new(PAGE_SIZE).unwrap()), rights).unwrap();
    let (rw, ro) = (mo(both), mo(Rights::MAP_READ));
    let page = PAGE_SIZE as u64;
    assert_eq!(io(PID, &f, Rights::MAP_WRITE, IoOpcode::Read, rw, (0, 0, 1)), Err(KError::NoAccess));
    assert_eq!(io(PID, &f, Rights::MAP_READ, IoOpcode::Write, rw, (0, 0, 1)), Err(KError::NoAccess));
    assert_eq!(io(PID, &f, both, IoOpcode::Read, ro, (0, 0, 1)), Err(KError::NoAccess), "a read's buffer is written");
    assert_eq!(io(PID, &f, both, IoOpcode::Read, rw, (1, 0, page)), Err(KError::InvalidArgument), "past the buffer");
    assert_eq!(
        io(PID, &f, both, IoOpcode::Write, rw, (0, 2 * page - 1, 2)),
        Err(KError::InvalidArgument),
        "past the end"
    );
    assert_eq!(io(PID, &f, both, IoOpcode::Write, rw, (0, page, 1)), Err(KError::Unsupported), "over a hole");
    let src = global::get().install(PID, on_device(&reg, 1, &[]).erase(), Rights::MAP_READ).unwrap();
    assert_eq!(io(PID, &f, both, IoOpcode::Write, src, (1, 0, page)), Err(KError::InvalidArgument), "past the source");
    let writable_file = global::get().install(PID, on_device(&reg, 1, &[]).erase(), both).unwrap();
    let into_file = io(PID, &f, both, IoOpcode::Read, writable_file, (0, 0, 1));
    assert_eq!(into_file, Err(KError::InvalidArgument), "a read into a file");
    let big = on_device(&reg, (MAX_USER_COPY_SIZE / page) + 2, &[]);
    let big_src = global::get().install(PID, big.clone().erase(), Rights::MAP_READ).unwrap();
    let past_window = (0, 0, MAX_USER_COPY_SIZE + 1);
    assert_eq!(io(PID, &big, both, IoOpcode::Write, big_src, past_window), Err(KError::InvalidArgument), "a window");
    let flagged = IoOp { opcode: IoOpcode::Read as u32, flags: 1, buffer: rw.0, buf_offset: 0, offset: 0, length: 1 };
    assert_eq!(submit(f.clone(), both, &flagged, IoOpcode::Read, PID), Err(KError::InvalidArgument));
    let stub = Ref::new(FileObject::try_new(PAGE_SIZE, Producer::Stub { base: 0 }).unwrap());
    assert_eq!(io(PID, &stub, both, IoOpcode::Read, rw, (0, 0, 1)), Err(KError::Unsupported), "no server's");
    assert_eq!(global::close_all_owned_by(PID), 5);
}

#[test]
fn a_read_at_the_end_completes_at_once_and_a_full_mount_queues_the_rest_in_turn() {
    const PID: u32 = 4492;
    init_global_heap();
    global::init_for_tests();
    let reg = registration();
    let f = on_device(&reg, 1, &[]);
    let both = Rights::MAP_READ | Rights::MAP_WRITE;
    let rw = global::get().install(PID, ObjectRef::new(MemoryObject::try_new(PAGE_SIZE).unwrap()), both).unwrap();
    let operation_of = |h: isize| {
        let found = global::get().lookup(RawHandle(h as u64), PID, Rights::empty()).unwrap();
        found.object.downcast::<PendingOperation>().ok().unwrap()
    };
    let running = fill(&reg);
    let at_end = operation_of(io(PID, &f, both, IoOpcode::Read, rw, (0, PAGE_SIZE as u64, 1)).unwrap());
    assert_eq!(crate::sched::pending_op_completion(&at_end), (0, 0), "answered, not queued");
    let read = operation_of(io(PID, &f, both, IoOpcode::Read, rw, (0, 0, 8)).unwrap());
    let write = operation_of(io(PID, &f, both, IoOpcode::Write, rw, (0, 0, 8)).unwrap());
    assert!(!crate::sched::pending_op_is_signaled(&read) && !crate::sched::pending_op_is_signaled(&write));
    let next = reg.op_done(running[0]).unwrap();
    assert!(matches!(next.work, FileWork::Read(..)) && core::ptr::eq(&*next.po, &*read), "in turn");
    let next = reg.op_done(running[1]).unwrap();
    assert!(matches!(next.work, FileWork::Write(..)) && core::ptr::eq(&*next.po, &*write));
    global::close_all_owned_by(PID);
}
