//! Host-test builders for user threads, which a kernel stack would put out of a host's reach.

use super::*;

/// A user process of `pid` around a fresh address space, its single reference adopted.
pub(crate) fn user_process_ref(pid: u32) -> (ObjectRef, crate::mm::PhysAddr) {
    use crate::mm::addr_space::AddressSpace;
    let asp = AddressSpace::new().unwrap();
    let root = asp.root();
    let p = Process::try_new_user(pid, asp, crate::libkern::SysCaps::empty()).unwrap();
    let ptr = KBox::into_raw(p).as_ptr() as *mut ();
    // SAFETY: into_raw yielded the single creation ref; adopt it.
    (unsafe { ObjectRef::from_raw(ptr, KObjectType::Process) }, root)
}

/// A user thread built **without** a kernel stack, avoiding the QEMU-only `fabricate_frame`,
/// which writes a kernel-vmap address that is not host memory.
pub(crate) fn user_thread_no_stack(tid: u32, process: ObjectRef, entry: u64, user_sp: u64) -> KBox<Thread> {
    let (owner_pid, root) = {
        let p = process.as_ptr() as *const Process;
        // SAFETY: `process` is a live Process ref this test holds.
        unsafe { ((*p).pid(), (*p).address_space_root().unwrap()) }
    };
    KBox::try_new(Thread {
        header: KObjectHeader::new(KObjectType::Thread),
        tid,
        owner_pid,
        arch: ArchThreadContext::new(),
        state: ThreadState::Ready,
        entry: inert_entry,
        arg: 0,
        stack: None,
        addr_space_root: Some(root),
        user_entry: Some((entry, user_sp)),
        user_boot_args: [0; 4],
        process_ended: false,
        ending: false,
        process: Some(process),
        wait_objs: [0; MAX_WAIT_HANDLES],
        wait_signaled: [false; MAX_WAIT_HANDLES],
        wait_count: 0,
        wait_has_deadline: false,
        wait_phase: AtomicU8::new(WaitPhase::Running as u8),
        fault_regs: crate::arch::RegisterValues::default(),
        disp_tag: 0,
        disp_code: 0,
        class: SchedClass::TimeShared,
        rt_priority: 0,
        nice: 0,
        vruntime: 0,
        last_cpu: 0,
        cpu_mask: u8::MAX,
        on_cpu: AtomicBool::new(false),
        fpu: Some(new_fpu_area().expect("fpu area")),
    })
    .unwrap()
}
