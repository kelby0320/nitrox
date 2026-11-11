//! Host-test builders for user threads, which a kernel stack would put out of a host's reach.

use super::*;

/// A user process of `pid` around a fresh address space, its single reference adopted.
pub(crate) fn user_process_ref(pid: u32) -> (Ref<Process>, crate::mm::PhysAddr) {
    use crate::mm::addr_space::AddressSpace;
    let asp = AddressSpace::new().unwrap();
    let root = asp.root();
    let p = Process::try_new_user(pid, asp, crate::libkern::SysCaps::empty()).unwrap();
    (Ref::new(p), root)
}

/// A user thread built **without** a kernel stack, avoiding the QEMU-only `fabricate_frame`,
/// which writes a kernel-vmap address that is not host memory.
pub(crate) fn user_thread_no_stack(tid: u32, process: Ref<Process>, entry: u64, user_sp: u64) -> KBox<Thread> {
    let fpu = Some(new_fpu_area().expect("fpu area"));
    KBox::try_new(Thread {
        parked: Parked { arch: UnsafeCell::new(ArchThreadContext::new()), fpu },
        ..Thread::user(tid, process, entry, user_sp, [0; 4])
    })
    .unwrap()
}
