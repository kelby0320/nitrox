use super::*;
use crate::mm::test_support::init_global_heap;
use crate::object::device_node::{BlockGeometry, DeviceClass, ResourceDescriptor};
use crate::object::namespace::SubtreeBase;
use crate::object::userspace_server::{FILE_OPS_MAX, FILE_OPS_PER_PROCESS};
use crate::object::{DeviceNode, IpcChannel, Producer};
use crate::sched::{pending_op_completion, pending_op_is_signaled};

fn registration() -> Ref<UserspaceServerReg> {
    let (ep, peer) = IpcChannel::try_new_pair(4).unwrap();
    drop(peer);
    UserspaceServerReg::try_new(ep).unwrap()
}

fn file(reg: &Ref<UserspaceServerReg>, file_id: u64) -> Ref<FileObject> {
    let device =
        Ref::new(DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap());
    let producer = Producer::FsServerBlocks { device, block_size: 4096, reg: reg.clone(), file_id };
    Ref::new(FileObject::try_new(PAGE_SIZE, producer).unwrap())
}

fn cached(reg: &Ref<UserspaceServerReg>) -> Ref<FileObject> {
    FileObject::cache_in(reg, file(reg, 7)).unwrap()
}

fn flush(pid: u32, file: Option<Ref<FileObject>>) -> QueuedFlush {
    QueuedFlush { file, po: Ref::new(PendingOperation::try_new().unwrap()), pid }
}

/// `Ok(true)` to run now, `Ok(false)` queued.
fn admit(reg: &Ref<UserspaceServerReg>, f: QueuedFlush) -> Result<bool, KError> {
    reg.admit_flush(f).map(|now| now.is_some()).map_err(|(e, _)| e)
}

/// Every place taken, by processes `100..` four flushes each: the flushes' pids.
fn fill(reg: &Ref<UserspaceServerReg>) -> std::vec::Vec<u32> {
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
    assert_eq!(full.flush_done(running[0]).map(|f| f.pid), Some(200), "first in, first run");
    assert_eq!(full.flush_done(running[1]).map(|f| f.pid), Some(201));
    assert!(full.flush_done(running[2]).is_none(), "none waits: the place is given back");
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
    assert_eq!(reg.flush_done(running[0]).map(|f| f.pid), Some(300));
    assert_eq!(admit(&reg, flush(running[0], None)), Ok(false), "a share given back, queued behind 300's");
    assert_eq!(admit(&reg, flush(300, None)), Err(KError::WouldBlock), "one running and three queued");
}

#[test]
fn a_flush_lets_go_of_its_file_and_place_before_it_completes() {
    init_global_heap();
    let reg = registration();
    let mine = reg.admit_flush(flush(400, Some(cached(&reg)))).ok().unwrap().unwrap();
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
    Flush::run(KBox::try_new(Flush { reg: Some(reg.clone()), first: Some(first) }).unwrap());
    assert!(pos.iter().all(|po| pending_op_is_signaled(po) && pending_op_completion(po) == (0, 0)));
    assert_eq!(admit(&reg, flush(502, None)), Ok(true), "fifteen run, the thread's place given back");
    assert_eq!(admit(&reg, flush(503, None)), Ok(false));
}

fn writable(pid: u32, f: &Ref<FileObject>) -> RawHandle {
    global::get().install(pid, f.clone().erase(), Rights::MAP_READ | Rights::MAP_WRITE).unwrap()
}

fn submit(pid: u32, h: RawHandle, buffer: u64) -> SysResult {
    let op = IoOp { opcode: IoOpcode::Flush as u32, flags: 0, buffer, buf_offset: 0, offset: 0, length: 0 };
    super::super::submit::submit_flush(h.0, &op, pid)
}

/// The operation the handle `h` names, its handle closed.
fn operation_of(pid: u32, h: isize) -> Ref<PendingOperation> {
    let h = RawHandle(h as u64);
    let found = global::get().lookup(h, pid, Rights::empty()).unwrap();
    drop(global::get().close(h, pid).unwrap());
    found.object.downcast::<PendingOperation>().ok().unwrap()
}

#[test]
fn a_flush_is_queued_on_a_full_mount_and_refused_past_its_processs_share() {
    const PID: u32 = 4495;
    init_global_heap();
    global::init_for_tests();
    let (reg, ns) = (registration(), Namespace::try_new().unwrap());
    ns.bind_userspace_server(b"/m", reg.clone(), Rights::LOOKUP, SubtreeBase::empty()).ok().unwrap();
    let running = fill(&reg);
    let queued = operation_of(PID, mount(&ns, b"/m", true, PID).unwrap());
    assert!(!pending_op_is_signaled(&queued), "queued, not answered as a held");
    let f = cached(&reg);
    let h = writable(PID, &f);
    let file_queued = operation_of(PID, submit(PID, h, 0).unwrap());
    for _ in 2..FILE_OPS_PER_PROCESS {
        assert!(mount(&ns, b"/m", true, PID).is_ok());
    }
    assert_eq!(mount(&ns, b"/m", true, PID).err(), Some(KError::WouldBlock));
    assert_eq!(submit(PID, h, 0).err(), Some(KError::WouldBlock));
    let count = operation_of(PID, mount(&ns, b"/m", false, PID).unwrap());
    assert_eq!(pending_op_completion(&count), (0, 1), "a held takes no place, and counts the file");
    let next = reg.flush_done(running[0]).unwrap();
    assert!(next.file.is_none() && core::ptr::eq(&*next.po, &*queued), "the mount's flush first");
    let next = reg.flush_done(running[1]).unwrap();
    assert!(next.file.is_some() && core::ptr::eq(&*next.po, &*file_queued));
    drop(global::get().close(h, PID).unwrap());
}

#[test]
fn a_files_flush_counts_in_its_mounts_budget_forgotten_or_not_and_whatever_its_id() {
    const PID: u32 = 4496;
    init_global_heap();
    global::init_for_tests();
    let reg = registration();
    fill(&reg);
    let forgotten = cached(&reg);
    let _ = forgotten.forget(&Ref::new(PendingOperation::try_new().unwrap()));
    for f in [forgotten, file(&reg, 0)] {
        let h = writable(PID, &f);
        let po = operation_of(PID, submit(PID, h, 0).unwrap());
        assert!(!pending_op_is_signaled(&po), "queued behind the mount's sixteen");
        drop(global::get().close(h, PID).unwrap());
    }
    let stub = Ref::new(FileObject::try_new(PAGE_SIZE, Producer::Stub { base: 0 }).unwrap());
    let h = writable(PID, &stub);
    let po = operation_of(PID, submit(PID, h, 0).unwrap());
    assert_eq!(pending_op_completion(&po), (KError::IoError as i32, 0), "nothing behind it to write to");
    drop(global::get().close(h, PID).unwrap());
}

#[test]
fn a_files_flush_needs_map_write_and_names_nothing_else() {
    const PID: u32 = 4498;
    init_global_heap();
    global::init_for_tests();
    let reg = registration();
    let f = cached(&reg);
    let read_only = global::get().install(PID, f.clone().erase(), Rights::MAP_READ).unwrap();
    assert_eq!(submit(PID, read_only, 0).err(), Some(KError::NoAccess));
    let h = writable(PID, &f);
    assert_eq!(submit(PID, h, 7).err(), Some(KError::InvalidArgument));
    fill(&reg);
    for h in [read_only, h] {
        drop(global::get().close(h, PID).unwrap());
    }
}

#[test]
fn a_flush_or_held_of_a_path_to_no_server_answers_through_its_operation() {
    const PID: u32 = 4499;
    init_global_heap();
    global::init_for_tests();
    let ns = Namespace::try_new().unwrap();
    let direct = ObjectRef::new(crate::object::MemoryObject::try_new(PAGE_SIZE).unwrap());
    ns.bind(b"/d", direct, Rights::LOOKUP).ok().unwrap();
    for flush in [true, false] {
        let none = operation_of(PID, mount(&ns, b"/x", flush, PID).unwrap());
        assert_eq!(pending_op_completion(&none), (KError::NotFound as i32, 0), "flush {flush}");
        let direct = operation_of(PID, mount(&ns, b"/d", flush, PID).unwrap());
        assert_eq!(pending_op_completion(&direct), (KError::Unsupported as i32, 0), "flush {flush}");
    }
}
