use super::super::file_ops::tests::{cached, file, fill, registration};
use super::*;
use crate::mm::test_support::init_global_heap;
use crate::object::Producer;
use crate::object::namespace::SubtreeBase;
use crate::object::userspace_server::FILE_OPS_PER_PROCESS;
use crate::sched::{pending_op_completion, pending_op_is_signaled};

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
    let next = reg.op_done(running[0]).unwrap();
    assert!(matches!(next.work, FileWork::FlushMount) && core::ptr::eq(&*next.po, &*queued), "the mount's first");
    let next = reg.op_done(running[1]).unwrap();
    assert!(matches!(next.work, FileWork::Flush(_)) && core::ptr::eq(&*next.po, &*file_queued));
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
