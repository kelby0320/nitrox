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
//! ## Blocking and deadlines (wait queues)
//!
//! A thread can also **block** in `sys_wait`: [`wait_on`] registers it as a
//! waiter on each target object and (optionally) on the deadline min-heap
//! ([`deadline`]), then [`block_current_and_switch`] parks it in
//! [`SchedState::blocked`] (state `Blocked`) — like [`switch_to_next`] but
//! without re-enqueuing. A waker calls [`make_runnable`] to move it back to
//! `ready`. Timer/`sys_wait` deadlines are checked on the periodic tick
//! ([`on_timer_tick`] → [`fire_expired_deadlines`]) and waiters woken
//! **directly** under `SCHED` — no DPC. The wait/timer/blocked state all live
//! under the rank-1 `SCHED` lock for Phase 1 (single lock domain → no
//! lost-wakeup window; see `kernel/docs/lock-ordering.md`).
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
use crate::arch::timer::ArchTimer;
use crate::arch::{Cpu, Paging, context_switch};
use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, IrqSpinLock, IrqSpinLockGuard, KBox, KVec};
use crate::mm::PhysAddr;
use crate::libkern::Notification;
use crate::object::{
    MAX_WAIT_HANDLES, NotificationChannel, ObjectRef, Thread, ThreadEntry, ThreadState, Timer,
};

// `Timer` above is the kernel object (`crate::object::Timer`); the hardware
// monotonic clock is reached via the full path `crate::arch::Timer::read_ns()`
// (the `ArchTimer` trait, imported above, provides `read_ns`). The two names
// live in different paths — see `arch/timer.rs`.

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

/// Pre-reserved capacity for the blocked-thread parking list. Like
/// [`READY_RESERVE`], blocking beyond this is refused rather than allocating
/// under the rank-1 lock.
const BLOCKED_RESERVE: usize = 16;

/// The deadline min-heap: armed-timer and `sys_wait`-deadline expiries keyed by
/// absolute monotonic ns, drained on each periodic tick. A pure binary heap in
/// a [`KVec`], living in [`SchedState`] under `SCHED`; host-tested.
mod deadline {
    use crate::libkern::{AllocError, KVec};

    /// One pending deadline. `is_thread` distinguishes a [`Timer`] fire
    /// (`target` = the Timer object address) from a `sys_wait` thread-deadline
    /// (`target` = the waiting Thread object address, woken directly → timeout).
    ///
    /// [`Timer`]: crate::object::Timer
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(super) struct Entry {
        pub deadline_ns: u64,
        pub target: usize,
        pub is_thread: bool,
    }

    /// Pre-reserved heap capacity (one entry per armed timer / pending wait
    /// deadline). Reserved at [`init`](super::init) outside the lock.
    pub(super) const HEAP_RESERVE: usize = 16;

    /// The earliest entry, or `None` if empty.
    pub(super) fn peek(h: &KVec<Entry>) -> Option<Entry> {
        h.first().copied()
    }

    /// Insert `e`, sifting up by `deadline_ns`. `Err` if at reserve (the caller
    /// maps it to an out-of-memory failure; never grows under the lock).
    pub(super) fn push(h: &mut KVec<Entry>, e: Entry) -> Result<(), AllocError> {
        if h.len() >= h.capacity() {
            return Err(AllocError);
        }
        h.try_push(e).expect("within reserved heap capacity");
        let mut i = h.len() - 1;
        while i > 0 {
            let parent = (i - 1) / 2;
            if h[i].deadline_ns < h[parent].deadline_ns {
                h.swap(i, parent);
                i = parent;
            } else {
                break;
            }
        }
        Ok(())
    }

    /// Remove and return the earliest entry, sifting the moved-up last element
    /// down. `None` if empty.
    pub(super) fn pop_min(h: &mut KVec<Entry>) -> Option<Entry> {
        let n = h.len();
        if n == 0 {
            return None;
        }
        h.swap(0, n - 1);
        let min = h.pop();
        sift_down(h, 0);
        min
    }

    /// Remove the first entry matching `(target, is_thread)`. Returns `true` if
    /// one was removed. Used when a `sys_wait` resumes (drop its deadline) or a
    /// timer is re-armed (drop its stale entry).
    pub(super) fn remove(h: &mut KVec<Entry>, target: usize, is_thread: bool) -> bool {
        let Some(i) = h
            .iter()
            .position(|e| e.target == target && e.is_thread == is_thread)
        else {
            return false;
        };
        let n = h.len();
        h.swap(i, n - 1);
        h.pop();
        // The element now at `i` may be too small (sift up) or too large (down).
        if i < h.len() {
            if i > 0 && h[i].deadline_ns < h[(i - 1) / 2].deadline_ns {
                sift_up(h, i);
            } else {
                sift_down(h, i);
            }
        }
        true
    }

    fn sift_up(h: &mut KVec<Entry>, mut i: usize) {
        while i > 0 {
            let parent = (i - 1) / 2;
            if h[i].deadline_ns < h[parent].deadline_ns {
                h.swap(i, parent);
                i = parent;
            } else {
                break;
            }
        }
    }

    fn sift_down(h: &mut KVec<Entry>, mut i: usize) {
        let n = h.len();
        loop {
            let l = 2 * i + 1;
            let r = 2 * i + 2;
            let mut smallest = i;
            if l < n && h[l].deadline_ns < h[smallest].deadline_ns {
                smallest = l;
            }
            if r < n && h[r].deadline_ns < h[smallest].deadline_ns {
                smallest = r;
            }
            if smallest == i {
                break;
            }
            h.swap(i, smallest);
            i = smallest;
        }
    }
}

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
    /// Threads blocked in `sys_wait`, parked off the run queue. Each holds one
    /// refcount on its `Thread` (keeping it and its kernel stack alive); a
    /// waker moves it back to `ready`. Pre-reserved (see [`BLOCKED_RESERVE`]).
    blocked: KVec<ObjectRef>,
    /// The deadline min-heap (armed timers + `sys_wait` deadlines), drained on
    /// each periodic tick. Pre-reserved (see [`deadline::HEAP_RESERVE`]).
    deadlines: KVec<deadline::Entry>,
}

static SCHED: IrqSpinLock<SchedState> = IrqSpinLock::new(SchedState {
    ready: KVec::new(),
    current: None,
    reap: None,
    next_tid: 1,
    quantum: QUANTUM_TICKS,
    idle: None,
    idle_addr: 0,
    blocked: KVec::new(),
    deadlines: KVec::new(),
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

    // Build the pre-reserved run queue + blocked list + deadline heap OUTSIDE
    // the lock (the only growth). Blocking and timer arming stay within these
    // reserves, never allocating under the rank-1 lock.
    let mut ready: KVec<ObjectRef> = KVec::new();
    ready.try_reserve(READY_RESERVE)?;
    let mut blocked: KVec<ObjectRef> = KVec::new();
    blocked.try_reserve(BLOCKED_RESERVE)?;
    let mut deadlines: KVec<deadline::Entry> = KVec::new();
    deadlines.try_reserve(deadline::HEAP_RESERVE)?;
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
    g.blocked = blocked;
    g.deadlines = deadlines;
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
    // Fire any deadlines that have elapsed FIRST, so a just-woken thread is in
    // `ready` and the reschedule below can pick it. This is the direct-wakeup
    // path: no DPC, all under the one scheduler lock (see the module docs).
    let now = crate::arch::Timer::read_ns();
    fire_expired_deadlines(&mut g, now);
    let (new_quantum, reschedule) = tick_quantum(g.quantum, !g.ready.is_empty());
    g.quantum = new_quantum;
    if reschedule {
        switch_to_next(g); // consumes the guard; switches with IF masked
    }
    // else: guard drops here — IF was already 0 (IRQ context), stays 0 until iretq.
}

/// Drain every deadline at or before `now`, firing each: a timer entry signals
/// + wakes its waiters (and re-arms if periodic); a `sys_wait` thread-deadline
/// entry wakes that thread directly (its wait slots stay un-signaled → the
/// thread observes a timeout). Caller holds `SCHED`. Performs **no allocation**
/// and **no blocking** — safe in the timer IRQ.
fn fire_expired_deadlines(g: &mut SchedState, now: u64) {
    while let Some(top) = deadline::peek(&g.deadlines) {
        if top.deadline_ns > now {
            break;
        }
        deadline::pop_min(&mut g.deadlines);
        if top.is_thread {
            // A `sys_wait` deadline: wake the waiting thread directly.
            let th = top.target as *mut ();
            // SAFETY: a heaped thread-deadline names a thread blocked in
            // `wait_on`, pinned in `blocked`; `SCHED` held.
            if unsafe { Thread::wait_try_wake(th) } {
                make_runnable(g, th);
            }
        } else {
            let timer = top.target as *mut ();
            // SAFETY: a heaped timer is kept alive by its waiter(s)' `sys_wait`
            // `ObjectRef`s (or the owner's handle); `SCHED` held.
            unsafe { Timer::set_in_heap(timer, false) };
            fire_timer(g, timer, now);
        }
    }
}

/// Fire one timer: signal + wake all its waiters, then re-arm if periodic.
/// Caller holds `SCHED`.
fn fire_timer(g: &mut SchedState, timer: *mut (), now: u64) {
    let mut buf = [core::ptr::null_mut(); Timer::MAX_WAITERS];
    // SAFETY: live Timer, `SCHED` held — drains its waiter list.
    let n = unsafe { Timer::take_waiters(timer, &mut buf) };
    for &th in &buf[..n] {
        // SAFETY: each waiter is a thread blocked in `wait_on`, pinned in
        // `blocked`; `SCHED` held. Mark this timer's slot signaled, then claim
        // the thread for wakeup (CAS); the first claimer makes it runnable.
        unsafe {
            Thread::wait_mark_signaled(th, timer as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(g, th);
            }
        }
    }
    // SAFETY: live Timer, `SCHED` held.
    let interval = unsafe { Timer::interval(timer) };
    if interval > 0 {
        let next = now.saturating_add(interval);
        // SAFETY: live Timer, `SCHED` held — re-arm the periodic timer.
        unsafe {
            Timer::set_armed(timer, next, interval);
            Timer::set_in_heap(timer, true);
        }
        // Re-push: a periodic timer that just had ≥1 waiter keeps a heap slot
        // free (its entry was just popped), so this stays within reserve.
        let _ = deadline::push(
            &mut g.deadlines,
            deadline::Entry { deadline_ns: next, target: timer as usize, is_thread: false },
        );
    } else {
        // SAFETY: live Timer, `SCHED` held — one-shot: disarm.
        unsafe { Timer::set_armed(timer, 0, 0) };
    }
}

// --- Waitable dispatch (Timer | NotificationChannel) -------------------
//
// `wait_on` works over type-erased object pointers; it dispatches the three
// waitable operations by the kobject type read from the `KObjectHeader` at
// offset 0 (every kobject is `#[repr(C)]` with the header first). `sys_wait`
// has already rejected non-waitables before these run.

/// The kobject type at the head of a type-erased object pointer.
/// # Safety: `obj` is a live kobject (header at offset 0); `SCHED` held.
unsafe fn obj_type(obj: *mut ()) -> KObjectType {
    // SAFETY: every kobject begins with a `KObjectHeader` (see object::header).
    unsafe { (*(obj as *const crate::object::header::KObjectHeader)).object_type() }
}

/// Is this waitable currently signaled? (Timer deadline elapsed / channel non-empty.)
/// # Safety: live waitable, `SCHED` held.
unsafe fn obj_already_signaled(obj: *mut (), now: u64) -> bool {
    match unsafe { obj_type(obj) } {
        KObjectType::Timer => unsafe { Timer::already_signaled(obj, now) },
        KObjectType::NotificationChannel => unsafe {
            NotificationChannel::already_signaled(obj)
        },
        _ => false,
    }
}

/// Register the current thread as a waiter. `Err` over the object's reserve.
/// # Safety: live waitable, `SCHED` held.
unsafe fn obj_add_waiter(obj: *mut (), th: *mut ()) -> Result<(), ()> {
    match unsafe { obj_type(obj) } {
        KObjectType::Timer => unsafe { Timer::add_waiter(obj, th) },
        KObjectType::NotificationChannel => unsafe { NotificationChannel::add_waiter(obj, th) },
        _ => Err(()),
    }
}

/// Unregister a waiter (idempotent).
/// # Safety: live waitable, `SCHED` held.
unsafe fn obj_remove_waiter(obj: *mut (), th: *mut ()) {
    match unsafe { obj_type(obj) } {
        KObjectType::Timer => unsafe { Timer::remove_waiter(obj, th) },
        KObjectType::NotificationChannel => unsafe { NotificationChannel::remove_waiter(obj, th) },
        _ => {}
    }
}

/// Wake every thread blocked on `channel` (its queue just went non-empty).
/// Caller holds `SCHED`. No allocation, no blocking — safe from the fault path.
/// Mirrors [`fire_timer`]'s waiter-drain.
fn signal_channel(g: &mut SchedState, channel: *mut ()) {
    let mut buf = [core::ptr::null_mut(); NotificationChannel::MAX_WAITERS];
    // SAFETY: live channel, `SCHED` held — drains its waiter list.
    let n = unsafe { NotificationChannel::take_waiters(channel, &mut buf) };
    for &th in &buf[..n] {
        // SAFETY: each waiter is a thread blocked in `wait_on`, pinned in
        // `blocked`; `SCHED` held.
        unsafe {
            Thread::wait_mark_signaled(th, channel as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(g, th);
            }
        }
    }
}

/// Park the current thread (set `Blocked`, move into `blocked`) and switch to
/// the next runnable thread. Mirrors [`switch_to_next`]'s IF-bracket exactly,
/// but does **not** re-enqueue the outgoing thread — the caller ([`wait_on`])
/// has already registered it on its objects' wait queues / the deadline heap
/// under this same `SCHED` hold, so there is no lost-wakeup window. Resumes
/// here (lock-free) when a waker calls [`make_runnable`] and the scheduler
/// later picks this thread. Consumes the guard.
fn block_current_and_switch(mut g: IrqSpinLockGuard<'_, SchedState>) {
    let next = match dequeue_front(&mut g) {
        Some(n) => n,
        None => g.idle.take().expect("idle thread exists after init"),
    };
    let prev = g.current.take().expect("current set");
    let prev_obj = prev.as_ptr();
    let next_obj = next.as_ptr();
    // SAFETY: both pinned (prev parked in `blocked`, next becoming current);
    // `SCHED` held — the Thread accessor contract. (The idle thread never
    // blocks, so `prev` is never the idle thread.)
    unsafe {
        Thread::set_state(prev_obj, ThreadState::Blocked);
        Thread::set_state(next_obj, ThreadState::Running);
    }
    let prev_slot = unsafe { Thread::saved_sp_mut_ptr(prev_obj) };
    let next_sp = unsafe { Thread::saved_sp(next_obj) };
    let next_root = unsafe { resolve_root(next_obj) };

    // Park prev in `blocked` (NOT ready/idle) — its `ObjectRef` keeps it alive.
    debug_assert!(g.blocked.len() < g.blocked.capacity());
    g.blocked.try_push(prev).expect("blocked list within reserve");
    g.current = Some(next);

    // Identical IF-bracket to `switch_to_next`.
    let saved_if = g.release_keeping_irqs_masked();
    // SAFETY: as `switch_to_next`.
    unsafe { Paging::set_page_table(next_root) };
    // SAFETY: as `switch_to_next`.
    unsafe { context_switch(prev_slot, next_sp) };
    // Resumed: a waker moved us back to `ready` and the scheduler switched us
    // in. Restore our own captured interrupt state (cooperative resume).
    // SAFETY: ring-0.
    unsafe { Cpu::interrupts_restore(saved_if) };
}

/// Move a `Blocked` thread from `blocked` to `ready` (state `Ready`). Caller
/// holds `SCHED`. Returns `false` (no-op) if `thread` is not in `blocked` —
/// the backstop for a second waker after the wake-CAS already claimed it. The
/// `ObjectRef` moves `blocked`→`ready` with no refcount change.
fn make_runnable(g: &mut SchedState, thread: *mut ()) -> bool {
    let Some(i) = g.blocked.iter().position(|r| r.as_ptr() == thread) else {
        return false;
    };
    let r = g.blocked.remove(i);
    // SAFETY: `r` pins `thread`; `SCHED` held.
    unsafe { Thread::set_state(thread, ThreadState::Ready) };
    debug_assert!(g.ready.len() < g.ready.capacity());
    g.ready.try_push(r).expect("ready within reserve");
    true
}

/// Outcome of [`wait_on`].
pub enum WaitResult {
    /// At least one object signaled. `signaled[i]` corresponds to `objs[i]`
    /// (the input order); set bits mark the handles to report.
    Signaled([bool; MAX_WAIT_HANDLES]),
    /// Nothing signaled before the deadline (or a poll found nothing ready).
    TimedOut,
    /// Registration exceeded a per-object or heap reserve.
    OutOfMemory,
}

/// Register the current thread as a waiter on every object in `objs` (their
/// type-erased addresses) with an optional `deadline_ns` (absolute monotonic;
/// `0` = poll, `u64::MAX` = no timeout), block until one fires, then
/// unregister and report which signaled. `now` is the current monotonic time.
///
/// Registration + block happen under a single `SCHED` hold, so a waker cannot
/// slip between them (single-CPU, interrupts masked) — no lost wakeup.
pub fn wait_on(objs: &[usize], deadline_ns: u64, now: u64) -> WaitResult {
    debug_assert!(objs.len() <= MAX_WAIT_HANDLES);
    let me_ptr;
    {
        let mut g = SCHED.lock();
        me_ptr = g.current.as_ref().expect("current set when a thread runs").as_ptr();

        // Fast path: any object already signaled?
        let mut signaled = [false; MAX_WAIT_HANDLES];
        let mut any = false;
        for (i, &o) in objs.iter().enumerate() {
            // SAFETY: `o` is a live waitable pinned by the caller's `ObjectRef`;
            // `SCHED` held. Dispatches by the object's header type.
            if unsafe { obj_already_signaled(o as *mut (), now) } {
                signaled[i] = true;
                any = true;
            }
        }
        if any {
            return WaitResult::Signaled(signaled);
        }
        if deadline_ns == 0 {
            // Poll with nothing ready.
            return WaitResult::TimedOut;
        }

        // Register the wait set on the thread, then on each object. All under
        // this one hold → atomic w.r.t. any waker.
        let has_deadline = deadline_ns != u64::MAX;
        // SAFETY: live Thread (current), `SCHED` held.
        unsafe { Thread::wait_register(me_ptr, objs, has_deadline) };
        let mut registered = 0usize;
        let mut failed = false;
        for &o in objs {
            // SAFETY: live waitable, `SCHED` held.
            if unsafe { obj_add_waiter(o as *mut (), me_ptr) }.is_err() {
                failed = true;
                break;
            }
            registered += 1;
        }
        if !failed && has_deadline {
            failed = deadline::push(
                &mut g.deadlines,
                deadline::Entry { deadline_ns, target: me_ptr as usize, is_thread: true },
            )
            .is_err();
        }
        if failed {
            // Unwind the partial registration and bail without blocking.
            for &o in &objs[..registered] {
                // SAFETY: live Timer, `SCHED` held.
                unsafe { obj_remove_waiter(o as *mut (), me_ptr) };
            }
            // SAFETY: live Thread, `SCHED` held.
            unsafe { Thread::wait_clear(me_ptr) };
            return WaitResult::OutOfMemory;
        }

        // Block. Registration above + this block are one uninterrupted hold.
        block_current_and_switch(g); // consumes g; resumes here when woken
    }

    // Resumed (woken by a signal or the deadline). Unregister everything under
    // a fresh hold, snapshot which slots fired, then build the result.
    let mut snap = [(0usize, false); MAX_WAIT_HANDLES];
    let signaled;
    {
        let mut g = SCHED.lock();
        // SAFETY: live Thread (we run on it), `SCHED` held.
        let n = unsafe { Thread::wait_snapshot(me_ptr, &mut snap) };
        for &o in objs {
            // SAFETY: live Timer (caller still holds its `ObjectRef`), `SCHED` held.
            unsafe { obj_remove_waiter(o as *mut (), me_ptr) };
        }
        // SAFETY: live Thread, `SCHED` held.
        if unsafe { Thread::wait_has_deadline(me_ptr) } {
            deadline::remove(&mut g.deadlines, me_ptr as usize, true);
        }
        // SAFETY: live Thread, `SCHED` held.
        unsafe { Thread::wait_clear(me_ptr) };

        let mut bits = [false; MAX_WAIT_HANDLES];
        let mut any = false;
        for i in 0..n {
            bits[i] = snap[i].1;
            any |= snap[i].1;
        }
        signaled = if any { Some(bits) } else { None };
    }
    match signaled {
        Some(bits) => WaitResult::Signaled(bits),
        None => WaitResult::TimedOut,
    }
}

/// Arm (or, with `deadline_ns == 0`, disarm) a timer: set its
/// deadline/interval and (re)insert its deadline-heap entry. `deadline_ns` is
/// absolute monotonic ns. Returns `Err(())` if the heap is at reserve.
pub fn timer_arm(timer: *mut (), deadline_ns: u64, interval_ns: u64) -> Result<(), ()> {
    let mut g = SCHED.lock();
    // Drop any stale heap entry (re-arm).
    // SAFETY: live Timer (caller holds its `ObjectRef`), `SCHED` held.
    if unsafe { Timer::in_heap(timer) } {
        deadline::remove(&mut g.deadlines, timer as usize, false);
        unsafe { Timer::set_in_heap(timer, false) };
    }
    // SAFETY: live Timer, `SCHED` held.
    unsafe { Timer::set_armed(timer, deadline_ns, interval_ns) };
    if deadline_ns != 0 {
        if deadline::push(
            &mut g.deadlines,
            deadline::Entry { deadline_ns, target: timer as usize, is_thread: false },
        )
        .is_err()
        {
            // SAFETY: live Timer, `SCHED` held — undo the arm on heap overflow.
            unsafe { Timer::set_armed(timer, 0, 0) };
            return Err(());
        }
        // SAFETY: live Timer, `SCHED` held.
        unsafe { Timer::set_in_heap(timer, true) };
    }
    Ok(())
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

/// The tid of the currently running thread. Used by the exception path to fill
/// the `thread` field of a fault notification. Takes only the rank-1 lock.
pub fn current_tid() -> u32 {
    let g = SCHED.lock();
    let cur = g.current.as_ref().expect("current set when a thread runs");
    // SAFETY: `current` is pinned alive and the run-queue lock serialises
    // Thread access; a shared `&Thread` read of `tid` is sound.
    unsafe { &*(cur.as_ptr() as *const crate::object::Thread) }.tid()
}

/// Deliver a fault `notif` to the **current** (faulting) process's notification
/// channel, wake the channel's waiters, then terminate the current thread.
/// Never returns. Called from the exception dispatchers on a ring-3 fault.
///
/// The faulting thread *is* `current` (the exception entered on its kernel
/// stack), so termination reuses [`exit`] verbatim. If the process has no
/// channel (or the faulter is a kernel thread with no process), the fault is
/// still contained — we just `exit()`. The supervisor that holds the channel's
/// `ObjectRef` keeps it (and the enqueued notification) alive past this reap.
pub fn deliver_fault_and_exit(notif: Notification) -> ! {
    {
        let mut g = SCHED.lock();
        // Find the current thread's process channel pointer (borrowed, never an
        // owned ObjectRef — we must not run a destructor under SCHED).
        let chan: Option<*mut ()> = g.current.as_ref().and_then(|cur| {
            // SAFETY: `current` pinned, SCHED held — shared `&Thread` read.
            let th = unsafe { &*(cur.as_ptr() as *const crate::object::Thread) };
            th.process_ref().and_then(|p| {
                // SAFETY: `p` pins a live Process; read its channel pointer.
                let proc = unsafe { &*(p.as_ptr() as *const crate::object::Process) };
                proc.notification_channel_ptr()
                // `p` (a cloned ObjectRef, never the last) drops here: a plain
                // atomic decrement, no destructor, safe under SCHED.
            })
        });
        if let Some(c) = chan {
            // SAFETY: `c` is the channel the current Process owns a ref on (so
            // it is alive at least until this thread is reaped); SCHED held. No
            // allocation (queue/waiters pre-reserved).
            let _edge = unsafe { NotificationChannel::enqueue(c, notif) };
            signal_channel(&mut g, c);
        }
        // guard drops here — never held across the exit() switch below.
    }
    exit()
}

/// Pop one notification from `channel` under `SCHED` (or synthesize a dropped
/// notice). The lock-guarded core of `sys_notif_recv` and the in-kernel
/// supervisor; the caller copies the result out **after** this returns.
pub fn notif_try_recv(channel: *mut ()) -> Option<Notification> {
    let _g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning the channel; SCHED held.
    unsafe { NotificationChannel::try_recv(channel) }
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
            blocked: KVec::new(),
            deadlines: KVec::new(),
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

    // --- Deadline min-heap (pure) -------------------------------------

    fn entry(deadline_ns: u64, target: usize) -> deadline::Entry {
        deadline::Entry { deadline_ns, target, is_thread: false }
    }

    #[test]
    fn heap_pops_in_deadline_order() {
        init_global_heap();
        let mut h: KVec<deadline::Entry> = KVec::new();
        h.try_reserve(deadline::HEAP_RESERVE).unwrap();
        for &d in &[50u64, 10, 30, 20, 40] {
            deadline::push(&mut h, entry(d, d as usize)).unwrap();
        }
        let mut got = [0u64; 5];
        for slot in got.iter_mut() {
            *slot = deadline::pop_min(&mut h).unwrap().deadline_ns;
        }
        assert_eq!(got, [10, 20, 30, 40, 50]);
        assert!(deadline::pop_min(&mut h).is_none());
        assert!(deadline::peek(&h).is_none());
    }

    #[test]
    fn heap_peek_is_min_and_remove_targets() {
        init_global_heap();
        let mut h: KVec<deadline::Entry> = KVec::new();
        h.try_reserve(deadline::HEAP_RESERVE).unwrap();
        deadline::push(&mut h, entry(30, 0xC)).unwrap();
        deadline::push(&mut h, entry(10, 0xA)).unwrap();
        deadline::push(&mut h, entry(20, 0xB)).unwrap();
        assert_eq!(deadline::peek(&h).unwrap().target, 0xA);
        // Remove the middle target; ordering of the rest is preserved.
        assert!(deadline::remove(&mut h, 0xB, false));
        assert!(!deadline::remove(&mut h, 0xB, false)); // gone
        assert!(!deadline::remove(&mut h, 0xA, true)); // wrong is_thread
        assert_eq!(deadline::pop_min(&mut h).unwrap().target, 0xA);
        assert_eq!(deadline::pop_min(&mut h).unwrap().target, 0xC);
        assert!(deadline::pop_min(&mut h).is_none());
    }

    #[test]
    fn heap_push_refuses_over_reserve() {
        init_global_heap();
        let mut h: KVec<deadline::Entry> = KVec::new();
        h.try_reserve(2).unwrap();
        // Fill to the actual capacity (try_reserve may round up), then the next
        // push must refuse rather than grow under the (eventual) lock.
        let cap = h.capacity();
        for i in 0..cap {
            assert!(deadline::push(&mut h, entry(i as u64 + 1, i + 1)).is_ok());
        }
        assert!(deadline::push(&mut h, entry(9999, 9999)).is_err());
        assert_eq!(h.len(), cap);
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
