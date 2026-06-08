//! Minimal cooperative round-robin scheduler for kernel threads.
//!
//! Phase 1 is single-CPU with interrupts masked everywhere (IF=0, no
//! timer, no preemption), so scheduling is **cooperative**: a thread runs
//! until it calls [`yield_now`] or [`exit`]. This module owns the run
//! queue, the current-thread pointer, and the switch sequencing on top of
//! the arch primitive [`context_switch`](crate::arch::context_switch) and
//! the [`Thread`] kernel object. Preemptive, multi-class, per-CPU
//! scheduling arrives with the timer/IRQ and SMP slices.
//!
//! ## The run-queue lock and the switch
//!
//! [`SCHED`] is the **rank-1** run-queue lock (`kernel/docs/lock-ordering.md`).
//! The one hard rule: the lock is **dropped before every
//! [`context_switch`] and re-acquired fresh on resume** — it is never
//! carried across a stack switch. Carrying it would have the *resumed*
//! thread drop a guard it never acquired. Every resume point
//! ([`yield_now`] after the switch, and [`thread_enter`]) therefore runs
//! with the lock not held. The brief window between publishing the next
//! thread and executing the register switch is safe under single-CPU /
//! IF=0 / no-preemption: nothing else can run.
//!
//! Allocation never happens under the lock: [`init`] installs a
//! pre-reserved run queue, and the heavy work in [`spawn`]
//! (stack allocation, frame fabrication) runs before the enqueue lock is
//! taken. Reaping an exited thread's stack (which takes rank-6 allocator
//! locks via [`KernelStack`](crate::mm)'s `Drop`) likewise runs outside
//! the rank-1 lock.

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::cpu::ArchCpu;
use crate::arch::paging::ArchPaging;
use crate::arch::{Cpu, Paging, context_switch};
use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec, SpinLock};
use crate::mm::PhysAddr;
use crate::object::{ObjectRef, Thread, ThreadEntry, ThreadState};

/// Run-queue capacity reserved once at [`init`]. Phase 1 runs a handful of
/// kernel threads; enqueueing beyond this is a logic error (debug-asserted)
/// rather than an allocation under the rank-1 lock.
const READY_RESERVE: usize = 16;

/// The kernel/boot page-table root, captured once at [`init`]. Threads with
/// no per-process address space (`addr_space_root == None`) run on this root;
/// the scheduler loads it on switch-in so a dying user thread's address space
/// can be freed safely (CR3 is the boot root before the reap).
static BOOT_ROOT: AtomicU64 = AtomicU64::new(0);

/// The page-table root a thread should run under: its own process root if it
/// has one, else the boot root. Caller holds the run-queue lock.
///
/// # Safety
/// `obj` is a live, pinned `Thread`.
unsafe fn resolve_root(obj: *mut ()) -> PhysAddr {
    match unsafe { Thread::addr_space_root(obj) } {
        Some(root) => root,
        None => PhysAddr::new(BOOT_ROOT.load(Ordering::Relaxed)),
    }
}

/// Scheduler state behind the rank-1 run-queue lock.
struct SchedState {
    /// Ready threads in round-robin order; each holds one refcount on its
    /// `Thread`, keeping it (and its kernel stack) alive while queued.
    ready: KVec<ObjectRef>,
    /// The currently running thread. `None` only before [`init`].
    current: Option<ObjectRef>,
    /// An exited thread awaiting reclamation. Dropped — freeing its kernel
    /// stack — by the next scheduler entry, never by the thread itself
    /// (it is still running on that stack at [`exit`] time).
    reap: Option<ObjectRef>,
    /// Monotonic thread-id source.
    next_tid: u32,
}

static SCHED: SpinLock<SchedState> = SpinLock::new(SchedState {
    ready: KVec::new(),
    current: None,
    reap: None,
    next_tid: 1,
});

/// Adopt a freshly created `Thread` into an [`ObjectRef`], transferring the
/// `KBox` creation reference without a refcount change.
fn into_objref(t: KBox<Thread>) -> ObjectRef {
    let ptr = KBox::into_raw(t).as_ptr() as *mut ();
    // SAFETY: `KBox::into_raw` yielded the single creation reference (the
    // header starts at refcount 1); `from_raw` adopts it without bumping.
    unsafe { ObjectRef::from_raw(ptr, KObjectType::Thread) }
}

/// Initialise the scheduler, adopting the running boot context as the first
/// (current) thread so the first [`context_switch`] has a valid slot to
/// save into. Must run once, after the allocators and paging are up and
/// before any [`spawn`]/[`yield_now`].
pub fn init() -> Result<(), AllocError> {
    // Cache the kernel/boot page-table root for the CR3 hook (see
    // `resolve_root`). `active_root` reads CR3 — a ring-0 op only reached at
    // real boot, never in host tests (which never call `init`).
    BOOT_ROOT.store(Paging::active_root().as_u64(), Ordering::Relaxed);

    // Build the pre-reserved run queue OUTSIDE the lock (the only growth).
    let mut ready: KVec<ObjectRef> = KVec::new();
    ready.try_reserve(READY_RESERVE)?;
    let boot = Thread::try_new_boot(0, 0)?;
    let boot_ref = into_objref(boot);

    let mut g = SCHED.lock();
    g.ready = ready;
    g.current = Some(boot_ref);
    Ok(())
}

/// Create a runnable kernel thread that will run `entry(arg)` and enqueue
/// it. Returns the new thread id. The stack allocation and frame
/// fabrication happen before the (brief) enqueue lock is taken.
pub fn spawn(entry: ThreadEntry, arg: usize) -> Result<u32, AllocError> {
    let tid = {
        let mut g = SCHED.lock();
        let t = g.next_tid;
        g.next_tid = g.next_tid.wrapping_add(1);
        t
    };
    // Heavy work outside the lock.
    let thread = Thread::try_new_runnable(tid, 0, entry, arg)?;
    let r = into_objref(thread);

    {
        let mut g = SCHED.lock();
        // Refuse rather than grow the queue under the rank-1 lock: growth
        // would allocate, and a failed `try_push` would drop `r` (running
        // `KernelStack`'s rank-6 reclamation) here, under the lock. Bail
        // first so `r` drops below, lock-free.
        if g.ready.len() < g.ready.capacity() {
            // Within the reserve: this push cannot grow and cannot fail.
            g.ready
                .try_push(r)
                .expect("push within reserved capacity is infallible");
            return Ok(tid);
        }
        // else: fall through with the lock dropped at the block's end.
    }
    // Over capacity: `r` drops here (lock released), releasing the thread's
    // last reference and freeing its kernel stack off the rank-1 lock.
    Err(AllocError)
}

/// Create a **user** thread for `process` that descends to ring 3 at
/// `entry` with stack `user_sp`, and enqueue it. Returns its thread id. The
/// `process` reference is moved into the thread (keeping its address space
/// alive). The kernel stack + frame fabrication happen before the (brief)
/// enqueue lock.
pub fn spawn_user(process: ObjectRef, entry: u64, user_sp: u64) -> Result<u32, AllocError> {
    let tid = {
        let mut g = SCHED.lock();
        let t = g.next_tid;
        g.next_tid = g.next_tid.wrapping_add(1);
        t
    };
    // Heavy work outside the lock (consumes `process` on success).
    let thread = Thread::try_new_user(tid, process, entry, user_sp)?;
    let r = into_objref(thread);

    {
        let mut g = SCHED.lock();
        if g.ready.len() < g.ready.capacity() {
            g.ready
                .try_push(r)
                .expect("push within reserved capacity is infallible");
            return Ok(tid);
        }
    }
    // Over capacity: `r` drops here (lock released) — releasing the thread's
    // last reference, freeing its kernel stack, and releasing the Process.
    Err(AllocError)
}

/// Cooperatively yield to the next ready thread, round-robin. Returns
/// immediately (still current) if no other thread is ready. Resumes here,
/// lock-free, when this thread is scheduled again.
pub fn yield_now() {
    // Reclaim any previously-exited thread's stack first (outside the lock).
    reap_pending();

    let switch: Option<(*mut u64, u64, PhysAddr)> = {
        let mut g = SCHED.lock();
        match dequeue_front(&mut g) {
            None => None, // nothing else ready — keep running
            Some(next) => {
                let prev = g.current.take().expect("current set after init");
                let prev_obj = prev.as_ptr();
                let next_obj = next.as_ptr();
                // SAFETY: both threads are pinned alive (prev about to be
                // re-enqueued, next becoming current) and we hold the
                // run-queue lock, satisfying the Thread accessor contract.
                unsafe {
                    Thread::set_state(prev_obj, ThreadState::Ready);
                    Thread::set_state(next_obj, ThreadState::Running);
                }
                let prev_slot = unsafe { Thread::saved_sp_mut_ptr(prev_obj) };
                let next_sp = unsafe { Thread::saved_sp(next_obj) };
                let next_root = unsafe { resolve_root(next_obj) };
                debug_assert!(g.ready.len() < g.ready.capacity());
                // Re-enqueue prev at the tail, then install next as current.
                g.ready
                    .try_push(prev)
                    .expect("run queue within reserve");
                g.current = Some(next);
                Some((prev_slot, next_sp, next_root))
            }
        }
        // Guard dropped here — lock released before the switch.
    };

    if let Some((prev_slot, next_sp, next_root)) = switch {
        // Make the incoming thread's address space active before swapping
        // stacks. Safe to switch CR3 here: every kernel stack (incl. both
        // threads' and this code) lives in the shared kernel half, mapped
        // identically in every address space.
        // SAFETY: `next_root` is a fully-formed PML4 (boot root or a process
        // root with the kernel half inherited).
        unsafe { Paging::set_page_table(next_root) };
        // SAFETY: the lock is released; `prev_slot` points into the prev
        // thread (pinned in `ready`) and `next_sp` is the saved RSP of the
        // now-current thread (pinned in `current`). Single-CPU: no other
        // context touches either across the switch.
        unsafe { context_switch(prev_slot, next_sp) };
        // Resumed: we are current again and the lock is not held.
    }
}

/// Terminate the current thread and switch away forever. The thread's
/// `Thread` (and its kernel stack) are parked for reclamation by the next
/// scheduler entry — they cannot be freed here because this code is still
/// running on that stack.
pub fn exit() -> ! {
    // Reclaim any earlier exited thread first, so `reap` is free for us.
    reap_pending();

    let switch: Option<(*mut u64, u64, PhysAddr)> = {
        let mut g = SCHED.lock();
        let me = g.current.take().expect("current set");
        let me_obj = me.as_ptr();
        // SAFETY: `me` is the running thread, pinned, lock held.
        unsafe { Thread::set_state(me_obj, ThreadState::Exited) };
        let me_slot = unsafe { Thread::saved_sp_mut_ptr(me_obj) };
        // Park self for deferred reclamation (reap is empty after the
        // reap_pending above).
        debug_assert!(g.reap.is_none());
        g.reap = Some(me);

        match dequeue_front(&mut g) {
            Some(next) => {
                let next_obj = next.as_ptr();
                // SAFETY: next is pinned, becoming current; lock held.
                unsafe { Thread::set_state(next_obj, ThreadState::Running) };
                let next_sp = unsafe { Thread::saved_sp(next_obj) };
                let next_root = unsafe { resolve_root(next_obj) };
                g.current = Some(next);
                Some((me_slot, next_sp, next_root))
            }
            None => None,
        }
    };

    match switch {
        Some((me_slot, next_sp, next_root)) => {
            // Load the incoming thread's root BEFORE switching away. When a
            // user thread exits, `next_root` is the boot root, so CR3 is
            // restored to the kernel table before this (parked-in-`reap`)
            // thread is reaped — its `AddressSpace::Drop` frees the PML4 CR3
            // would otherwise still reference.
            // SAFETY: `next_root` is a fully-formed PML4; all kernel stacks
            // are mapped identically across roots.
            unsafe { Paging::set_page_table(next_root) };
            // SAFETY: lock released. We switch away from this stack forever;
            // `me_slot` (our own, now-Exited thread, pinned in `reap`) is
            // written by the switch and never read again. `next_sp` is the
            // incoming thread's saved RSP (pinned in `current`).
            unsafe { context_switch(me_slot, next_sp) };
            unreachable!("switched away from an exited thread");
        }
        // No other thread to run: we cannot free our own stack, and there
        // is nothing to switch to. Halt — a defensive tripwire; in the
        // Phase 1 demo the boot thread is always queued when a worker
        // exits, so this branch is not reached.
        None => Cpu::halt_loop(),
    }
}

/// Drop the pending reaped thread, if any, **outside** the run-queue lock
/// (its `KernelStack` `Drop` takes rank-6 allocator locks). Idempotent;
/// called at the top of [`yield_now`]/[`exit`] and by the boot drainer.
pub fn reap_pending() {
    let reaped = {
        let mut g = SCHED.lock();
        g.reap.take()
    };
    drop(reaped);
}

/// `true` when no thread other than the current one is ready to run.
pub fn ready_is_empty() -> bool {
    SCHED.lock().ready.is_empty()
}

/// Pop the front of the ready queue (round-robin order), or `None` if
/// empty. Caller holds the run-queue lock.
fn dequeue_front(g: &mut SchedState) -> Option<ObjectRef> {
    if g.ready.is_empty() {
        None
    } else {
        Some(g.ready.remove(0))
    }
}

/// Read the current thread's entry point and argument. Used by
/// [`thread_enter`] when a freshly scheduled thread first runs.
fn current_entry() -> (ThreadEntry, usize) {
    let g = SCHED.lock();
    let cur = g.current.as_ref().expect("current set when a thread runs");
    // SAFETY: `current` is pinned alive and we hold the lock.
    unsafe { Thread::entry_and_arg(cur.as_ptr()) }
}

/// The pid of the process that owns the currently running thread.
///
/// Valid during a syscall: the current thread is the calling user thread, so
/// this is the `caller_pid` the handle table's `lookup`/`close`/`restrict`/
/// `stat`/`duplicate` need. Takes only the rank-1 run-queue lock and releases
/// it before returning — handle syscalls call this **first**, then take the
/// rank-3 handle-table lock, never nesting the two.
pub fn current_owner_pid() -> u32 {
    let g = SCHED.lock();
    let cur = g.current.as_ref().expect("current set when a thread runs");
    // SAFETY: `current` is pinned alive (it holds a refcount on the `Thread`)
    // and we hold the run-queue lock, which — single-CPU — serialises all
    // access to the `Thread`. Forming a shared `&Thread` to read `owner_pid`
    // is sound; no `&mut` is taken anywhere under this lock.
    unsafe { &*(cur.as_ptr() as *const crate::object::Thread) }.owner_pid()
}

/// The [`Process`](crate::object::Process) owning the currently running
/// thread, if it has one (`None` for kernel/boot threads). Valid during a
/// syscall: the current thread is the calling user thread, so this is the
/// process whose address space and handles the call operates on. Takes only
/// the rank-1 run-queue lock; the returned [`ObjectRef`](crate::object::ObjectRef)
/// is cloned under the lock and outlives it. Handle/memory syscalls call this
/// first, then take lower-rank locks (handle-table rank-3, AS rank-4), never
/// nesting upward.
pub fn current_process() -> Option<ObjectRef> {
    let g = SCHED.lock();
    let cur = g.current.as_ref().expect("current set when a thread runs");
    // SAFETY: as `current_owner_pid` — `current` is pinned and the run-queue
    // lock serialises Thread access; `process_ref` clones the stored
    // `ObjectRef` (bumping the process refcount) under the lock.
    unsafe { &*(cur.as_ptr() as *const crate::object::Thread) }.process_ref()
}

/// Rust entry reached from
/// [`thread_trampoline`](crate::arch::thread_trampoline) the first time a
/// thread is scheduled. Runs the thread body, then exits cleanly if it
/// returns. Never returns.
///
/// Reached with the run-queue lock **not** held (the switching thread
/// released it before [`context_switch`]).
pub(crate) extern "C" fn thread_enter() -> ! {
    // Capture (under the lock) whether the current thread descends to ring 3
    // and, if so, its descent params + kernel-stack top.
    let descent: Option<(u64, u64, u64)> = {
        let g = SCHED.lock();
        let cur = g.current.as_ref().expect("current set when a thread runs");
        let obj = cur.as_ptr();
        // SAFETY: `current` is pinned alive and we hold the lock.
        match unsafe { Thread::user_entry(obj) } {
            Some((entry, user_sp)) => {
                let ktop = unsafe { Thread::kstack_top(obj) }
                    .expect("a user thread has a kernel stack");
                Some((entry, user_sp, ktop))
            }
            None => None,
        }
    };

    match descent {
        Some((entry, user_sp, ktop)) => {
            // Point the ring0 trap stack (TSS.RSP0) and the per-CPU syscall
            // stack at THIS thread's kernel stack before dropping to ring 3,
            // so syscalls/traps from ring 3 land on it. CR3 is already the
            // process address space (loaded by the scheduler on switch-in).
            Cpu::set_kernel_stack(ktop);
            crate::arch::set_syscall_kernel_stack(ktop);
            // SAFETY: `entry`/`user_sp` are canonical user VAs mapped X / W
            // in the active address space; the syscall fast-path is armed.
            unsafe { crate::arch::enter_user(entry, user_sp) }
        }
        None => {
            // Kernel thread: run the body in ring 0, then exit cleanly.
            let (entry, arg) = current_entry();
            entry(arg);
            exit();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::header::test_probe;

    // These tests exercise the run-queue bookkeeping and refcount handling
    // at the data-structure level. They do NOT call `context_switch`,
    // `thread_trampoline`, or run a thread body — those manipulate real
    // registers/stacks and are validated only under QEMU (the `xtask qemu`
    // serial trace) until `xtask test-qemu` exists.

    extern "C" fn noop(_arg: usize) {}

    /// Build an inert Thread ObjectRef without a kernel stack (so no real
    /// paging is needed on the host).
    fn inert_ref(tid: u32) -> ObjectRef {
        into_objref(Thread::try_new(tid, 0).unwrap())
    }

    #[test]
    fn dequeue_front_is_round_robin() {
        init_global_heap();
        let mut st = SchedState {
            ready: KVec::new(),
            current: None,
            reap: None,
            next_tid: 1,
        };
        st.ready.try_reserve(READY_RESERVE).unwrap();
        for tid in 1..=3 {
            st.ready.try_push(inert_ref(tid)).unwrap();
        }
        // Dequeue front, re-enqueue at back: classic round-robin rotation.
        let a = dequeue_front(&mut st).unwrap();
        // SAFETY: pinned, single-threaded test.
        let a_tid = unsafe { &*(a.as_ptr() as *const Thread) }.tid();
        assert_eq!(a_tid, 1);
        st.ready.try_push(a).unwrap();
        let b = dequeue_front(&mut st).unwrap();
        let b_tid = unsafe { &*(b.as_ptr() as *const Thread) }.tid();
        assert_eq!(b_tid, 2, "round-robin must pick the next, not repeat");
        st.ready.try_push(b).unwrap();
    }

    #[test]
    fn dequeue_front_empty_is_none() {
        init_global_heap();
        let mut st = SchedState {
            ready: KVec::new(),
            current: None,
            reap: None,
            next_tid: 1,
        };
        assert!(dequeue_front(&mut st).is_none());
    }

    #[test]
    fn queue_drop_releases_every_thread_exactly_once() {
        init_global_heap();
        test_probe::reset();
        {
            let mut st = SchedState {
                ready: KVec::new(),
                current: None,
                reap: None,
                next_tid: 1,
            };
            st.ready.try_reserve(READY_RESERVE).unwrap();
            for tid in 1..=4 {
                st.ready.try_push(inert_ref(tid)).unwrap();
            }
            st.current = Some(inert_ref(5));
            st.reap = Some(inert_ref(6));
            // No destroys while the refs are held.
            assert_eq!(test_probe::thread_destroys(), 0);
        } // st dropped here — every ObjectRef drops its one reference.
        assert_eq!(
            test_probe::thread_destroys(),
            6,
            "each queued/current/reaped thread destroyed exactly once",
        );
    }

    #[test]
    fn reaped_thread_destroyed_when_dropped() {
        init_global_heap();
        test_probe::reset();
        let r = inert_ref(1);
        assert_eq!(test_probe::thread_destroys(), 0);
        drop(r); // simulates reap_pending dropping the parked thread
        assert_eq!(test_probe::thread_destroys(), 1);
    }

    #[test]
    fn into_objref_preserves_single_reference() {
        init_global_heap();
        test_probe::reset();
        // Spawn-style: create runnable would need paging, so use inert.
        let r = into_objref(Thread::try_new(9, 0).unwrap());
        // entry/arg accessor reads back the inert defaults.
        // SAFETY: pinned, single-threaded.
        let (_entry, arg) = unsafe { Thread::entry_and_arg(r.as_ptr()) };
        assert_eq!(arg, 0);
        drop(r);
        assert_eq!(test_probe::thread_destroys(), 1);
        let _ = noop; // silence dead-code in case the demo path is cfg'd out
    }
}
