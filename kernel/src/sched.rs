//! Single-CPU preemptive round-robin scheduler for kernel and user threads.
//!
//! A thread runs until either (a) it voluntarily calls [`yield_now`]/[`exit`]
//! — the **cooperative** path — or (b) the periodic timer IRQ fires
//! [`on_timer_tick`], whose quantum-expiry **preempts** it. Both paths funnel
//! through the same [`switch_to_next`] core on top of the arch primitive
//! [`context_switch`](crate::arch::context_switch) and the [`Thread`] kernel
//! object. An [idle thread](idle_body) runs (`hlt`) whenever nothing else is
//! ready. Multi-class scheduling and per-CPU/SMP arrive in later slices.
//!
//! ## The run-queue lock, interrupts, and the switch
//!
//! [`SCHED`] is the **rank-1** run-queue lock (`kernel/docs/lock-ordering.md`).
//! It is now an [`IrqSpinLock`](crate::libkern::IrqSpinLock): it `cli`s before
//! acquiring, so a thread holding it cannot be preempted — the timer handler
//! can never find it already held by the context it interrupted (single-CPU
//! deadlock-freedom).
//!
//! The cardinal rule still holds: the lock is **dropped before every
//! [`context_switch`]** and re-acquired fresh on resume — never carried across
//! a stack switch. But interrupts must stay masked **across** the switch (a
//! timer mid-switch would corrupt a half-swapped stack), so the switch core
//! drops the lock via [`release_keeping_irqs_masked`] (release the lock, keep
//! IF=0) and restores the interrupt state on resume. The preemptive path is
//! already IF=0 (the timer interrupt gate clears it) and resumes by returning
//! into the timer-stub epilogue, which `iretq`s the original interrupt state
//! back.
//!
//! Allocation never happens under the lock: [`init`] installs a pre-reserved
//! run queue (and the idle thread) and the heavy work in [`spawn`] (stack
//! allocation, frame fabrication) runs before the enqueue lock is taken.
//! Reaping an exited thread's stack (rank-6 allocator locks via
//! [`KernelStack`](crate::mm)'s `Drop`) runs outside the rank-1 lock — and
//! never from the timer handler (which performs no allocation).
//!
//! [`release_keeping_irqs_masked`]: crate::libkern::IrqSpinLockGuard::release_keeping_irqs_masked

use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::cpu::ArchCpu;
use crate::arch::paging::ArchPaging;
use crate::arch::{Cpu, Paging, context_switch};
use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, IrqSpinLock, IrqSpinLockGuard, KBox, KVec};
use crate::mm::PhysAddr;
use crate::object::{ObjectRef, Thread, ThreadEntry, ThreadState};

/// Run-queue capacity reserved once at [`init`]. Phase 1 runs a handful of
/// kernel threads; enqueueing beyond this is a logic error (debug-asserted)
/// rather than an allocation under the rank-1 lock.
const READY_RESERVE: usize = 16;

/// Periodic scheduler tick: 10 ms (100 Hz). Matches the PIT calibration
/// window; fine-grained enough for round-robin without excessive IRQ overhead.
pub const TICK_NS: u64 = 10_000_000;

/// Ticks per scheduling quantum. One tick — reschedule on every tick — is the
/// simplest correct round-robin policy. The field stays (see [`SchedState`]) so
/// a later slice can lengthen slices without re-plumbing the tick path.
const QUANTUM_TICKS: u32 = 1;

/// Thread id for the idle thread — a reserved sentinel distinct from the
/// monotonic `next_tid` range (which starts at 1 and would need ~4 billion
/// spawns to reach this). Used only for diagnostics; idle identity is by
/// object address (`SchedState::idle_addr`).
const IDLE_TID: u32 = u32::MAX;

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
    /// Ticks remaining in the current thread's slice; reset to
    /// [`QUANTUM_TICKS`] on each reschedule. Scheduler **policy**, so it lives
    /// here rather than on `Thread` (no `Thread` layout/ABI change).
    quantum: u32,
    /// The idle thread, parked here whenever it is **not** the current thread.
    /// Kept out of `ready`/`reap`; runs (`hlt`) only when nothing else is
    /// ready. `None` only before [`init`] or while idle is current.
    idle: Option<ObjectRef>,
    /// The idle thread's object address — its stable identity (the `idle` slot
    /// is empty while idle runs). Stored as `usize` (not a raw pointer) so
    /// `SchedState` stays `Send`. `0` before [`init`].
    idle_addr: usize,
}

static SCHED: IrqSpinLock<SchedState> = IrqSpinLock::new(SchedState {
    ready: KVec::new(),
    current: None,
    reap: None,
    next_tid: 1,
    quantum: QUANTUM_TICKS,
    idle: None,
    idle_addr: 0,
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

    // The idle thread: a runnable kernel thread with its own stack that just
    // halts. Built outside the lock (it allocates a kernel stack). It is never
    // enqueued or reaped — its body loops forever.
    let idle = Thread::try_new_runnable(IDLE_TID, 0, idle_body, 0)?;
    let idle_ref = into_objref(idle);
    let idle_addr = idle_ref.as_ptr() as usize;

    let mut g = SCHED.lock();
    g.ready = ready;
    g.current = Some(boot_ref);
    g.idle = Some(idle_ref);
    g.idle_addr = idle_addr;
    g.quantum = QUANTUM_TICKS;
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
/// immediately (still current) if no other thread is ready — it does **not**
/// yield to the idle thread, so the boot drainer's [`ready_is_empty`] gate
/// still works. Resumes here, lock-free, when this thread is scheduled again.
pub fn yield_now() {
    // Reclaim any previously-exited thread's stack first (outside the lock).
    reap_pending();

    let g = SCHED.lock();
    if g.ready.is_empty() {
        return; // nothing else ready — keep running (guard drops, IF restored)
    }
    switch_to_next(g);
}

/// Periodic timer-tick entry, called from the timer IRQ dispatcher with
/// interrupts masked (the timer interrupt gate cleared IF). Decrements the
/// current quantum; on expiry it resets the quantum and, if another thread is
/// ready, reschedules round-robin (reusing [`switch_to_next`]).
pub fn on_timer_tick() {
    let mut g = SCHED.lock();
    let (new_quantum, reschedule) = tick_quantum(g.quantum, !g.ready.is_empty());
    g.quantum = new_quantum;
    if reschedule {
        switch_to_next(g); // consumes the guard; switches with IF masked
    }
    // else: guard drops here — IF was already 0 (IRQ context), stays 0 until iretq.
}

/// Decide the quantum update and whether to reschedule on a timer tick. Pure
/// (no lock, no switch) so it is host-testable.
///
/// Decrement the quantum; on expiry reset it to [`QUANTUM_TICKS`] and reschedule
/// **iff another thread is ready** — if nothing else is runnable, the current
/// thread (worker or idle) keeps running, since switching to the idle thread (or
/// idle→idle) would be pointless churn.
fn tick_quantum(quantum: u32, ready_nonempty: bool) -> (u32, bool) {
    let q = quantum.saturating_sub(1);
    if q == 0 {
        (QUANTUM_TICKS, ready_nonempty)
    } else {
        (q, false)
    }
}

/// The shared switch core used by [`yield_now`] and [`on_timer_tick`]: the
/// outgoing `current` is re-homed (re-enqueued, or re-parked if it is the idle
/// thread) and the next thread (front of `ready`, else the idle thread) becomes
/// current. Consumes the run-queue guard.
///
/// Callers ensure there is genuinely something else to run (`ready` non-empty,
/// or the current thread is idle and `ready` is non-empty). Drops the lock but
/// holds interrupts masked **across** the `context_switch` via
/// [`release_keeping_irqs_masked`](IrqSpinLockGuard::release_keeping_irqs_masked),
/// restoring the prior interrupt state on resume. Reusing
/// [`context_switch`](crate::arch::context_switch) is sound from the preemptive
/// path too: the interrupted thread's full register frame is already on its
/// kernel stack (pushed by the timer stub) *below* the callee-saved frame this
/// switch parks, so on resume it returns up into the timer-stub epilogue, which
/// `iretq`s the original context back.
fn switch_to_next(mut g: IrqSpinLockGuard<'_, SchedState>) {
    let next = match dequeue_front(&mut g) {
        Some(n) => n,
        // Only reachable if a caller violates the precondition; idle is the
        // safe fallback when it is parked (i.e. not the current thread).
        None => g.idle.take().expect("a runnable thread (ready or idle)"),
    };
    let prev = g.current.take().expect("current set after init");
    let prev_obj = prev.as_ptr();
    let next_obj = next.as_ptr();
    // SAFETY: both pinned alive (prev re-homed, next becoming current) and we
    // hold the run-queue lock — the Thread accessor contract.
    unsafe {
        Thread::set_state(prev_obj, ThreadState::Ready);
        Thread::set_state(next_obj, ThreadState::Running);
    }
    let prev_slot = unsafe { Thread::saved_sp_mut_ptr(prev_obj) };
    let next_sp = unsafe { Thread::saved_sp(next_obj) };
    let next_root = unsafe { resolve_root(next_obj) };

    // Re-home prev: the idle thread parks in its slot (never in `ready`);
    // every other thread re-enqueues at the tail.
    if g.idle_addr == prev_obj as usize {
        debug_assert!(g.idle.is_none());
        g.idle = Some(prev);
    } else {
        debug_assert!(g.ready.len() < g.ready.capacity());
        g.ready.try_push(prev).expect("run queue within reserve");
    }
    g.current = Some(next);

    // Drop the lock but keep interrupts masked across the switch (cardinal rule
    // + the no-IRQ-mid-switch invariant). `saved_if` is this thread's prior
    // interrupt state, restored when *this* thread next resumes here.
    let saved_if = g.release_keeping_irqs_masked();
    // SAFETY: `next_root` is a fully-formed PML4 (boot root, or a process root
    // with the kernel half inherited); all kernel stacks are mapped in every
    // root, so switching CR3 before the stack swap is sound.
    unsafe { Paging::set_page_table(next_root) };
    // SAFETY: lock released; interrupts masked; `prev_slot` points into the
    // re-homed prev thread (pinned) and `next_sp` is the saved RSP of the
    // now-current thread (pinned). Single-CPU: nothing else touches either.
    unsafe { context_switch(prev_slot, next_sp) };
    // Resumed (cooperative path): restore the interrupt state this thread had
    // on entry. On the preemptive path the resume instead returns into the
    // timer-stub epilogue (which `iretq`s IF back), and `saved_if` is false →
    // this is a no-op.
    // SAFETY: ring-0; restoring this thread's own captured interrupt state.
    unsafe { Cpu::interrupts_restore(saved_if) };
}

/// Terminate the current thread and switch away forever. The thread's
/// `Thread` (and its kernel stack) are parked for reclamation by the next
/// scheduler entry — they cannot be freed here because this code is still
/// running on that stack.
pub fn exit() -> ! {
    // Reclaim any earlier exited thread first, so `reap` is free for us.
    reap_pending();

    let mut g = SCHED.lock();
    let me = g.current.take().expect("current set");
    let me_obj = me.as_ptr();
    // SAFETY: `me` is the running thread, pinned, lock held. (The idle thread
    // never exits, so `me` is never the idle thread.)
    unsafe { Thread::set_state(me_obj, ThreadState::Exited) };
    let me_slot = unsafe { Thread::saved_sp_mut_ptr(me_obj) };
    // Park self for deferred reclamation (reap is empty after reap_pending).
    debug_assert!(g.reap.is_none());
    g.reap = Some(me);

    // Switch to the next ready thread, else the idle thread (which always
    // exists post-init and is parked here, since `me` was current, not idle).
    let next = match dequeue_front(&mut g) {
        Some(n) => n,
        None => g.idle.take().expect("idle thread exists after init"),
    };
    let next_obj = next.as_ptr();
    // SAFETY: next is pinned, becoming current; lock held.
    unsafe { Thread::set_state(next_obj, ThreadState::Running) };
    let next_sp = unsafe { Thread::saved_sp(next_obj) };
    let next_root = unsafe { resolve_root(next_obj) };
    g.current = Some(next);

    // Drop the lock but keep interrupts masked across the final switch; we
    // never resume, so the captured prior state is discarded (the incoming
    // thread restores its own).
    let _ = g.release_keeping_irqs_masked();
    // Load the incoming thread's root BEFORE switching away. When a user thread
    // exits, `next_root` is the boot root, so CR3 is restored to the kernel
    // table before this (parked-in-`reap`) thread is reaped — its
    // `AddressSpace::Drop` frees the PML4 CR3 would otherwise still reference.
    // SAFETY: `next_root` is a fully-formed PML4; all kernel stacks are mapped
    // identically across roots.
    unsafe { Paging::set_page_table(next_root) };
    // SAFETY: lock released. We switch away from this stack forever; `me_slot`
    // (our own now-Exited thread, pinned in `reap`) is written by the switch and
    // never read again; `next_sp` is the incoming thread's saved RSP (pinned).
    unsafe { context_switch(me_slot, next_sp) };
    unreachable!("switched away from an exited thread");
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

/// The idle thread body: reap any exited thread, then `hlt` until the next
/// interrupt. Runs with IF=1 (set by
/// [`thread_trampoline`](crate::arch::thread_trampoline)), so the periodic
/// timer wakes it; if a thread became ready, the tick reschedules to it,
/// otherwise control returns here and halts again. Never returns, so the idle
/// thread is never enqueued or reaped.
///
/// Reaping here (outside any IRQ context, holding no other lock) is the safe
/// home for draining the `reap` slot when the system would otherwise sit idle
/// — e.g. the boot thread parked in `reap` after it `exit`s at end of boot.
extern "C" fn idle_body(_arg: usize) {
    loop {
        reap_pending();
        // SAFETY: ring-0; IF=1 here, so the periodic timer (or any IRQ) wakes
        // the CPU and drives a reschedule when a thread becomes ready.
        unsafe { Cpu::halt() };
    }
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

    /// A fresh `SchedState` for data-structure tests (no idle thread; no real
    /// paging needed since the refs are inert).
    fn test_state() -> SchedState {
        SchedState {
            ready: KVec::new(),
            current: None,
            reap: None,
            next_tid: 1,
            quantum: QUANTUM_TICKS,
            idle: None,
            idle_addr: 0,
        }
    }

    #[test]
    fn dequeue_front_is_round_robin() {
        init_global_heap();
        let mut st = test_state();
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
        let mut st = test_state();
        assert!(dequeue_front(&mut st).is_none());
    }

    #[test]
    fn tick_quantum_decrements_then_reschedules_on_expiry() {
        // A multi-tick quantum counts down and only reschedules at 0.
        assert_eq!(tick_quantum(3, true), (2, false));
        assert_eq!(tick_quantum(2, true), (1, false));
        // Expiry with a ready thread → reset + reschedule.
        assert_eq!(tick_quantum(1, true), (QUANTUM_TICKS, true));
        // Expiry with nothing ready → reset but do NOT reschedule (keep running
        // the current thread; switching to idle would be pointless churn).
        assert_eq!(tick_quantum(1, false), (QUANTUM_TICKS, false));
    }

    #[test]
    fn tick_quantum_one_tick_quantum_reschedules_every_tick_when_ready() {
        // With QUANTUM_TICKS == 1 the live config reschedules each tick.
        assert_eq!(tick_quantum(QUANTUM_TICKS, true).1, QUANTUM_TICKS == 1);
    }

    #[test]
    fn queue_drop_releases_every_thread_exactly_once() {
        init_global_heap();
        test_probe::reset();
        {
            let mut st = test_state();
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
