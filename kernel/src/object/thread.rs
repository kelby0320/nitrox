//! The [`Thread`] kernel object.
//!
//! A thread is a register state, a kernel stack, scheduling state, and an
//! entry point. This slice (threading-and-context-switch) adds those
//! fields for **kernel** threads driven by the cooperative scheduler in
//! [`crate::sched`]; the FPU/XSAVE context, TLS `fs_base`, wait state, and
//! the owning-`Process` pointer arrive with later slices (see the deferred
//! notes on the fields below and `docs/planning/implementation-plan.md`).
//! The owning process is still referenced by id, not pointer, to avoid a
//! refcount cycle.
//!
//! ## Mutation discipline
//!
//! [`Thread`] is shared through an [`ObjectRef`](crate::object::ObjectRef),
//! yet the scheduler mutates `arch`, `state`, `entry`, and `arg`. That is
//! sound only because Phase 1 is single-CPU and the scheduler touches
//! those fields exclusively while holding its run-queue lock, which
//! serialises every access. The scheduler-only accessors below take a
//! type-erased `*mut ()` (an `ObjectRef::as_ptr()`) and reach individual
//! fields through raw pointers — never forming a `&mut Thread` that would
//! alias the atomically-accessed [`KObjectHeader`].

use crate::arch::{ArchThreadContext, fabricate_frame, thread_trampoline};
use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox};
use crate::mm::kstack::KernelStack;
use crate::object::header::KObjectHeader;

/// A kernel thread's entry point. `extern "C"` so the
/// [`thread_trampoline`](crate::arch::thread_trampoline) → `thread_enter`
/// path can call it with a stable ABI; the `usize` argument is opaque.
pub type ThreadEntry = extern "C" fn(arg: usize);

/// Lifecycle state of a thread, tracked by the scheduler.
///
/// `Blocked` is intentionally absent — there is no wait-queue subsystem
/// this slice; it arrives with IPC / `sys_wait`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ThreadState {
    /// On the run queue, not currently executing.
    Ready,
    /// The currently executing thread.
    Running,
    /// Body returned or [`exit`](crate::sched::exit) called; awaiting
    /// reclamation by the next scheduler entry.
    Exited,
}

/// A thread kernel object.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see
/// [`crate::object::header`]. The fields after the identity pair are
/// scheduler-owned (see the module's "Mutation discipline").
#[repr(C)]
pub struct Thread {
    header: KObjectHeader,
    tid: u32,
    owner_pid: u32,
    /// Saved kernel register state (just the resume RSP this slice).
    arch: ArchThreadContext,
    /// Scheduler lifecycle state.
    state: ThreadState,
    /// Entry point run by `thread_enter` on first schedule.
    entry: ThreadEntry,
    /// Opaque argument passed to `entry`.
    arg: usize,
    /// Owned kernel stack. `None` for the boot thread (it runs on the
    /// bootloader-provided stack) and for non-schedulable threads created
    /// purely as handle-table kernel objects in tests.
    stack: Option<KernelStack>,
    // Deferred to later slices:
    //   fpu_context  — kernel is soft-float; lands with userspace threads.
    //   fs_base/TLS  — no userspace, no `sys_thread_set_tls` yet.
    //   wait_*       — no wait-queue subsystem (`Blocked`) yet.
    //   *Process     — owner referenced by `owner_pid`; pointer waits for
    //                  the address-space-switch slice.
}

/// A non-scheduling default entry, used for threads that exist only as
/// refcounted kernel objects (e.g. handle-table tests) and are never run.
extern "C" fn inert_entry(_arg: usize) {}

impl Thread {
    /// Allocate an **inert** thread object (no kernel stack, never
    /// scheduled) with a refcount of one. Used where a `Thread` is needed
    /// purely as a handle-table kernel object; the caller transfers the
    /// reference to a handle via `KBox::into_raw` + `HandleTable::allocate`.
    pub fn try_new(tid: u32, owner_pid: u32) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Thread),
            tid,
            owner_pid,
            arch: ArchThreadContext::new(),
            state: ThreadState::Ready,
            entry: inert_entry,
            arg: 0,
            stack: None,
        })
    }

    /// Allocate the **boot** thread: the already-running boot context,
    /// adopted so the first [`context_switch`](crate::arch::context_switch)
    /// has a valid slot to save into. It owns no fabricated frame and no
    /// kernel stack (it runs on the bootloader stack); its `arch.rsp` is
    /// written by that first switch-out before it is ever read. State is
    /// [`Running`](ThreadState::Running).
    pub fn try_new_boot(tid: u32, owner_pid: u32) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Thread),
            tid,
            owner_pid,
            arch: ArchThreadContext::new(),
            state: ThreadState::Running,
            entry: inert_entry,
            arg: 0,
            stack: None,
        })
    }

    /// Allocate a **runnable** thread: a fresh kernel stack with a
    /// fabricated initial frame so the first switch-in runs `entry(arg)`
    /// via the trampoline. State is [`Ready`](ThreadState::Ready).
    pub fn try_new_runnable(
        tid: u32,
        owner_pid: u32,
        entry: ThreadEntry,
        arg: usize,
    ) -> Result<KBox<Self>, AllocError> {
        // Kernel threads share the active (boot) PML4; the stack maps into
        // the shared kernel vmap, visible from every address space.
        let stack = KernelStack::new(crate::arch::active_root())?;
        let top = stack.top().as_u64();
        // SAFETY: a freshly allocated `KernelStack` has its top
        // `KERNEL_STACK_PAGES` pages mapped writable in the shared kernel
        // vmap; `fabricate_frame` writes only the seven `u64` slots in
        // `[top - 56, top)`. `top` is page-aligned, hence 8-aligned. The
        // returned `ArchThreadContext` is stored opaquely.
        let arch = unsafe { fabricate_frame(top, thread_trampoline as *const () as u64) };
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Thread),
            tid,
            owner_pid,
            arch,
            state: ThreadState::Ready,
            entry,
            arg,
            stack: Some(stack),
        })
    }

    /// The thread identifier.
    pub fn tid(&self) -> u32 {
        self.tid
    }

    /// The id of the process this thread belongs to.
    pub fn owner_pid(&self) -> u32 {
        self.owner_pid
    }

    // --- Scheduler-only field accessors --------------------------------
    //
    // Each takes the type-erased object pointer (`ObjectRef::as_ptr()`) and
    // reaches one field through a raw pointer, never forming `&mut Thread`.
    // SAFETY (shared by all): `obj` addresses a live `Thread` (pinned by an
    // `ObjectRef` the caller holds), and the scheduler invokes these only
    // while holding its run-queue lock, which — single-CPU — serialises all
    // access to these fields.

    /// Read the scheduler lifecycle state. Test-only: production code sets
    /// state but never reads it back this slice.
    ///
    /// # Safety
    /// See the accessor contract above.
    #[cfg(test)]
    pub(crate) unsafe fn state(obj: *mut ()) -> ThreadState {
        let p = obj as *mut Thread;
        unsafe { core::ptr::read(&raw const (*p).state) }
    }

    /// Set the scheduler lifecycle state.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_state(obj: *mut (), s: ThreadState) {
        let p = obj as *mut Thread;
        unsafe { core::ptr::write(&raw mut (*p).state, s) }
    }

    /// Read the saved stack pointer of this thread's parked context.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn saved_sp(obj: *mut ()) -> u64 {
        let p = obj as *mut Thread;
        // SAFETY: `obj` is a live Thread; the arch layer owns the
        // representation of the saved stack pointer.
        unsafe { ArchThreadContext::saved_sp(&raw const (*p).arch) }
    }

    /// A raw pointer to the parked context's saved-stack-pointer word, for
    /// `context_switch`'s store into the outgoing thread. The pointee
    /// outlives the call because the `Thread` is refcount-pinned by the
    /// caller for the duration of the switch.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn saved_sp_mut_ptr(obj: *mut ()) -> *mut u64 {
        let p = obj as *mut Thread;
        // SAFETY: as `saved_sp`; the arch layer yields the slot pointer.
        unsafe { ArchThreadContext::sp_slot(&raw mut (*p).arch) }
    }

    /// Read the entry point and its argument.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn entry_and_arg(obj: *mut ()) -> (ThreadEntry, usize) {
        let p = obj as *mut Thread;
        unsafe {
            (
                core::ptr::read(&raw const (*p).entry),
                core::ptr::read(&raw const (*p).arg),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn inert_thread_has_no_stack_and_is_ready() {
        init_global_heap();
        let t = Thread::try_new(7, 3).unwrap();
        assert_eq!(t.tid(), 7);
        assert_eq!(t.owner_pid(), 3);
        assert!(t.stack.is_none());
        assert_eq!(t.state, ThreadState::Ready);
    }

    #[test]
    fn boot_thread_is_running_with_no_stack() {
        init_global_heap();
        let t = Thread::try_new_boot(0, 0).unwrap();
        assert_eq!(t.state, ThreadState::Running);
        assert!(t.stack.is_none());
        // The zeroed initial context is covered by `ArchThreadContext`'s
        // own tests; the boot thread's saved sp is written by the first
        // switch-out before it is ever read.
    }

    #[test]
    fn state_accessors_round_trip() {
        init_global_heap();
        let t = Thread::try_new(1, 1).unwrap();
        let obj = KBox::into_raw(t).as_ptr() as *mut ();
        // SAFETY: `obj` is a live Thread; this test is single-threaded and
        // holds the only reference, satisfying the accessor contract.
        unsafe {
            assert_eq!(Thread::state(obj), ThreadState::Ready);
            Thread::set_state(obj, ThreadState::Running);
            assert_eq!(Thread::state(obj), ThreadState::Running);
            Thread::set_state(obj, ThreadState::Exited);
            assert_eq!(Thread::state(obj), ThreadState::Exited);
            // Reclaim the leaked allocation.
            drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(
                obj as *mut Thread,
            )));
        }
    }

    #[test]
    fn saved_sp_accessor_reads_and_writes_through_slot() {
        init_global_heap();
        let t = Thread::try_new(1, 1).unwrap();
        let obj = KBox::into_raw(t).as_ptr() as *mut ();
        // SAFETY: as above.
        unsafe {
            assert_eq!(Thread::saved_sp(obj), 0);
            *Thread::saved_sp_mut_ptr(obj) = 0xDEAD_0000;
            assert_eq!(Thread::saved_sp(obj), 0xDEAD_0000);
            drop(KBox::<Thread>::from_raw(core::ptr::NonNull::new_unchecked(
                obj as *mut Thread,
            )));
        }
    }
}
