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
use crate::libkern::{ExitKind, ExitStatus, Notification};
use crate::libkern::ipc::IPC_HANDLE_MAX;
use crate::object::{
    BlockSendOutcome, IpcChannel, MAX_WAIT_HANDLES, NotificationChannel, ObjectRef,
    PendingOperation, ReclaimedSend, RecvState, SendOutcome, StoredMsg, Thread, ThreadEntry,
    ThreadState, Timer, TransferRef,
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

/// Pre-reserved capacity for the exited-thread reap list. Holds the current
/// thread plus any sibling threads a process exit tears down; sized like the
/// run queue so a whole process's threads fit without allocating under the lock.
const REAP_RESERVE: usize = 16;

/// The deadline min-heap: armed-timer and `sys_wait`-deadline expiries keyed by
/// absolute monotonic ns, drained on each periodic tick. A pure binary heap in
/// a [`KVec`], living in [`SchedState`] under `SCHED`; host-tested.
mod deadline {
    use crate::libkern::{AllocError, KVec};

    /// What a deadline-heap entry fires when its `deadline_ns` elapses.
    ///
    /// [`Timer`]: crate::object::Timer
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(super) enum DeadlineKind {
        /// A `sys_wait` thread-deadline: `target` is the waiting Thread object,
        /// woken directly (its wait slots stay un-signaled → it sees a timeout).
        Thread,
        /// A [`Timer`] fire: `target` is the Timer object address.
        Timer,
        /// A `BlockBounded` IPC send's delivery deadline: `target` is the
        /// [`PendingOperation`](crate::object::PendingOperation), `channel` the
        /// receiving endpoint holding the (undelivered) send.
        PendingSend,
    }

    /// One pending deadline, keyed by `(target, kind)` for removal.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(super) struct Entry {
        pub deadline_ns: u64,
        pub target: usize,
        pub kind: DeadlineKind,
        /// The receiving endpoint, for [`DeadlineKind::PendingSend`]; `0` else.
        pub channel: usize,
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

    /// Remove the first entry matching `(target, kind)`. Returns `true` if one
    /// was removed. Used when a `sys_wait` resumes (drop its deadline), a timer
    /// is re-armed (drop its stale entry), or a `BlockBounded` send is delivered
    /// early / its endpoint closes (drop its pending-send deadline).
    pub(super) fn remove(h: &mut KVec<Entry>, target: usize, kind: DeadlineKind) -> bool {
        let Some(i) = h
            .iter()
            .position(|e| e.target == target && e.kind == kind)
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
    /// Exited threads awaiting reclamation. Dropped — freeing their kernel
    /// stacks — by the next scheduler entry, never by a thread itself (it is
    /// still running on its stack at exit time). A **list** (not one slot) so a
    /// process exit can reap its torn-down sibling threads alongside the caller.
    /// Pre-reserved (see [`REAP_RESERVE`]).
    reap: KVec<ObjectRef>,
    /// Threads suspended after a ring-3 fault, parked off the run queue with
    /// their `ExceptionFrame` preserved on their kernel stacks. A supervisor's
    /// `sys_exception_resume` moves one back to `ready` (resume) or marks it for
    /// termination. Pre-reserved (see [`BLOCKED_RESERVE`]).
    suspended: KVec<ObjectRef>,
    /// Monotonic thread-id source.
    next_tid: u32,
    /// Monotonic process-id source. The boot parent takes pid 1; spawned
    /// children take 2, 3, …. (Phase 1 has no pid reuse; recycling lands with a
    /// real process table.)
    next_pid: u32,
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
    reap: KVec::new(),
    suspended: KVec::new(),
    next_tid: 1,
    next_pid: 2,
    quantum: QUANTUM_TICKS,
    idle: None,
    idle_addr: 0,
    blocked: KVec::new(),
    deadlines: KVec::new(),
});

/// Allocate the next process id (monotonic; no reuse in Phase 1). Takes only
/// the rank-1 lock briefly — `sys_process_spawn` calls this before touching the
/// rank-3 handle table.
pub fn alloc_pid() -> u32 {
    let mut g = SCHED.lock();
    let pid = g.next_pid;
    g.next_pid = g.next_pid.wrapping_add(1);
    pid
}

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
    let mut suspended: KVec<ObjectRef> = KVec::new();
    suspended.try_reserve(BLOCKED_RESERVE)?;
    let mut reap: KVec<ObjectRef> = KVec::new();
    reap.try_reserve(REAP_RESERVE)?;
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
    g.suspended = suspended;
    g.reap = reap;
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
/// `entry` with stack `user_sp`, and enqueue it. Returns a **cloned**
/// [`ObjectRef`] to the new thread (the enqueued `ready` entry holds its own
/// reference) so the caller can install a thread handle (`sys_thread_create`);
/// the thread id is `Thread::tid` of the returned object. The `process`
/// reference is moved into the thread (keeping its address space alive).
/// `boot_args` are the `[rdi, rsi, rdx, rcx]` register values seeded at the
/// thread's first ring-3 entry (the spawn hand-off — notification channel, root
/// namespace, first installed handle, `arg0`; `[0; 4]` for the boot/`hello`
/// path). The kernel stack + frame fabrication happen before the (brief) enqueue
/// lock.
pub fn spawn_user(
    process: ObjectRef,
    entry: u64,
    user_sp: u64,
    boot_args: [u64; 4],
) -> Result<ObjectRef, AllocError> {
    let tid = {
        let mut g = SCHED.lock();
        let t = g.next_tid;
        g.next_tid = g.next_tid.wrapping_add(1);
        t
    };
    // Heavy work outside the lock (consumes `process` on success).
    let thread = Thread::try_new_user(tid, process, entry, user_sp, boot_args)?;
    let r = into_objref(thread);
    // Clone the caller's handle before the enqueue moves `r` into `ready`.
    let handle = r.clone();

    {
        let mut g = SCHED.lock();
        if g.ready.len() < g.ready.capacity() {
            g.ready
                .try_push(r)
                .expect("push within reserved capacity is infallible");
            return Ok(handle);
        }
    }
    // Over capacity: `r` and `handle` drop here (lock released) — releasing the
    // thread's references, freeing its kernel stack, and releasing the Process.
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
    wake_entropy_seed_waiters(&mut g);
    let (new_quantum, reschedule) = tick_quantum(g.quantum, !g.ready.is_empty());
    g.quantum = new_quantum;
    if reschedule {
        switch_to_next(g); // consumes the guard; switches with IF masked
    }
    // else: guard drops here — IF was already 0 (IRQ context), stays 0 until iretq.
}

/// Complete any `PendingOperation`s parked on the entropy pool becoming seeded (the
/// unseeded `sys_entropy_read` path). Gated by a cheap lock-free check so the common
/// already-seeded / no-waiters case costs one atomic load. Caller holds `SCHED`.
///
/// The entropy subsystem owns the waiter refs (the IPC-`Block` pattern); we move
/// them out, signal each via its raw pointer, and let the local array drop them.
/// Dropping a `PendingOperation` ref under `SCHED` is sound: its `Drop` touches only
/// the allocator (acquired *below* `SCHED` in the lock order — the legal direction),
/// never re-entering `SCHED`. See `kernel/docs/lock-ordering.md` § The entropy lock.
fn wake_entropy_seed_waiters(g: &mut SchedState) {
    if !crate::entropy::seed_wake_pending() {
        return;
    }
    let mut buf: [Option<ObjectRef>; crate::entropy::SEED_WAITERS_MAX] =
        [const { None }; crate::entropy::SEED_WAITERS_MAX];
    let n = crate::entropy::drain_seed_waiters(&mut buf);
    for slot in buf[..n].iter() {
        if let Some(po) = slot {
            signal_pending_op_with_result(g, po.as_ptr(), 0, 0);
        }
    }
    // `buf` (the moved-out waiter refs) drops here — see the doc comment for why
    // dropping a `PendingOperation` ref under `SCHED` is sound.
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
        match top.kind {
            deadline::DeadlineKind::Thread => {
                // A `sys_wait` deadline: wake the waiting thread directly.
                let th = top.target as *mut ();
                // SAFETY: a heaped thread-deadline names a thread blocked in
                // `wait_on`, pinned in `blocked`; `SCHED` held.
                if unsafe { Thread::wait_try_wake(th) } {
                    make_runnable(g, th);
                }
            }
            deadline::DeadlineKind::Timer => {
                let timer = top.target as *mut ();
                // SAFETY: a heaped timer is kept alive by its waiter(s)'
                // `sys_wait` `ObjectRef`s (or the owner's handle); `SCHED` held.
                unsafe { Timer::set_in_heap(timer, false) };
                fire_timer(g, timer, now);
            }
            deadline::DeadlineKind::PendingSend => {
                // A `BlockBounded` send's delivery deadline elapsed: cancel the
                // held send and complete its `PendingOperation` with `TimedOut`.
                // No `ObjectRef` drop here (rank-1 `SCHED` held — a transferred
                // object's `Drop` could take the buddy lock); the message + refs
                // are reclaimed on the next recv / at close (see `ipc_channel`).
                let channel = top.channel as *mut ();
                let po = top.target as *mut ();
                // SAFETY: `channel` is still open — `ipc_endpoint_closing`
                // removes its held sends' deadline entries, so a fired
                // pending-send deadline always names a live endpoint; `SCHED`
                // held. `cancel_pending_send` only sets a flag (no drop).
                unsafe { IpcChannel::cancel_pending_send(channel, po) };
                signal_pending_op(g, po, crate::syscall::error::KError::TimedOut as i32);
            }
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
            deadline::Entry {
                deadline_ns: next,
                target: timer as usize,
                kind: deadline::DeadlineKind::Timer,
                channel: 0,
            },
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
        KObjectType::IpcChannel => unsafe { IpcChannel::already_signaled(obj) },
        KObjectType::PendingOperation => unsafe { PendingOperation::already_signaled(obj) },
        _ => false,
    }
}

/// Register the current thread as a waiter. `Err` over the object's reserve.
/// # Safety: live waitable, `SCHED` held.
unsafe fn obj_add_waiter(obj: *mut (), th: *mut ()) -> Result<(), ()> {
    match unsafe { obj_type(obj) } {
        KObjectType::Timer => unsafe { Timer::add_waiter(obj, th) },
        KObjectType::NotificationChannel => unsafe { NotificationChannel::add_waiter(obj, th) },
        KObjectType::IpcChannel => unsafe { IpcChannel::add_waiter(obj, th) },
        KObjectType::PendingOperation => unsafe { PendingOperation::add_waiter(obj, th) },
        _ => Err(()),
    }
}

/// Unregister a waiter (idempotent).
/// # Safety: live waitable, `SCHED` held.
unsafe fn obj_remove_waiter(obj: *mut (), th: *mut ()) {
    match unsafe { obj_type(obj) } {
        KObjectType::Timer => unsafe { Timer::remove_waiter(obj, th) },
        KObjectType::NotificationChannel => unsafe { NotificationChannel::remove_waiter(obj, th) },
        KObjectType::IpcChannel => unsafe { IpcChannel::remove_waiter(obj, th) },
        KObjectType::PendingOperation => unsafe { PendingOperation::remove_waiter(obj, th) },
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

/// Wake every thread blocked recv-ing on `endpoint` (a message just arrived in
/// its inbox, or its peer just closed). Caller holds `SCHED`. No allocation, no
/// blocking. Mirrors [`signal_channel`] but drains an [`IpcChannel`]'s
/// recv-waiter list.
fn signal_ipc_endpoint(g: &mut SchedState, endpoint: *mut ()) {
    let mut buf = [core::ptr::null_mut(); IpcChannel::MAX_WAITERS];
    // SAFETY: live endpoint, `SCHED` held — drains its recv-waiter list.
    let n = unsafe { IpcChannel::take_waiters(endpoint, &mut buf) };
    for &th in &buf[..n] {
        // SAFETY: each waiter is a thread blocked in `wait_on`, pinned in
        // `blocked`; `SCHED` held.
        unsafe {
            Thread::wait_mark_signaled(th, endpoint as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(g, th);
            }
        }
    }
}

/// Complete a [`PendingOperation`] with `status` and wake every thread blocked
/// on it. One-shot: a second call is a no-op (the first completion wins), so it
/// is safe to call from both the delivery path and a timeout. Caller holds
/// `SCHED`. No allocation, no blocking, no `ObjectRef` drop. Mirrors
/// [`signal_ipc_endpoint`] but drains a `PendingOperation`'s waiter list.
fn signal_pending_op(g: &mut SchedState, po: *mut (), status: i32) {
    signal_pending_op_with_result(g, po, status, 0);
}

/// As [`signal_pending_op`], but the completion also carries a `result` payload
/// (a namespace lookup's resolved handle). `signal_pending_op` is the
/// `result = 0` wrapper for the status-only call sites (blocking IPC). Caller
/// holds `SCHED`. No allocation, no blocking, no `ObjectRef` drop.
fn signal_pending_op_with_result(g: &mut SchedState, po: *mut (), status: i32, result: u64) {
    // SAFETY: live PO, `SCHED` held. One-shot; a re-signal returns `false`.
    if !unsafe { PendingOperation::signal_with_result(po, status, result) } {
        return; // already completed — its waiters were handled the first time
    }
    let mut buf = [core::ptr::null_mut(); PendingOperation::MAX_WAITERS];
    // SAFETY: live PO, `SCHED` held — drains its waiter list.
    let n = unsafe { PendingOperation::take_waiters(po, &mut buf) };
    for &th in &buf[..n] {
        // SAFETY: each waiter is a thread blocked in `wait_on`, pinned in
        // `blocked`; `SCHED` held.
        unsafe {
            Thread::wait_mark_signaled(th, po as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(g, th);
            }
        }
    }
}

/// Read a completed [`PendingOperation`]'s `(status, result)` under `SCHED`.
/// Called by `sys_wait` when building the `IoResult` for a signaled PO; both
/// values are stable after the one-shot completion, but the lock is taken to
/// honor the accessor contract (and to read both under one hold). The caller's
/// handle pins `po` for the duration.
pub fn pending_op_completion(po: *mut ()) -> (i32, u64) {
    let _g = SCHED.lock();
    // SAFETY: `po` is a live `PendingOperation` pinned by the caller's handle;
    // `SCHED` held — both reads see the same one-shot-stable completion.
    unsafe { (PendingOperation::status(po), PendingOperation::result(po)) }
}

/// Complete a [`PendingOperation`] with `(status, result)` from **syscall
/// context** (not a wakeup path), taking `SCHED` to do so. A namespace
/// direct-handle lookup uses this to **pre-signal** the PO it is about to return:
/// resolution finished synchronously, so the PO is completed in place. The PO has
/// no waiters yet (it was just created), so the wake loop is a no-op; the caller's
/// later `sys_wait` takes the already-signalled fast path. One-shot.
pub fn complete_pending_op(po: *mut (), status: i32, result: u64) {
    let mut g = SCHED.lock();
    signal_pending_op_with_result(&mut g, po, status, result);
}

/// Send `msg` from `endpoint` (into its peer's receive ring) under `SCHED`,
/// **moving** any `transfers` it carries into the queued slot and waking the
/// peer's blocked receivers if the ring went empty→non-empty. The caller has
/// already copied the message in from user memory (no copy under the lock). On
/// `Full`/`PeerClosed` the `transfers` are left with the caller (untaken), to be
/// dropped **outside** `SCHED`. Returns the [`SendOutcome`].
pub fn ipc_send_push(
    endpoint: *mut (),
    msg: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
) -> SendOutcome {
    let mut g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning `endpoint`; `SCHED` held.
    let outcome = unsafe { IpcChannel::send_push(endpoint, msg, transfers) };
    if let SendOutcome::Sent { woke_edge: true } = outcome {
        // SAFETY: a successful send proves the peer is non-null; `SCHED` held.
        let peer = unsafe { IpcChannel::peer_of(endpoint) };
        if !peer.is_null() {
            signal_ipc_endpoint(&mut g, peer);
        }
    }
    outcome
}

/// Blocking send (`Block` / `BlockBounded`): deliver `msg` into the peer's
/// receive ring, or — if it is full — **hold** it in the peer's pending-send
/// queue with a reference to the caller's `PendingOperation` `po`, to be
/// delivered (completing `po`) when the peer next receives. On immediate
/// delivery `po` is **pre-completed** (status 0) here so the caller's `sys_wait`
/// returns at once. The caller has copied the message in and holds `po`'s
/// creation reference + the endpoint reference. Returns the [`BlockSendOutcome`];
/// on `PendingFull`/`PeerClosed` the `transfers` are left for the caller to drop.
/// `deadline_ns` is the `BlockBounded` delivery deadline (absolute monotonic ns);
/// `u64::MAX` for plain `Block` (no deadline). On a `Queued` outcome with a finite
/// deadline, a `PendingSend` deadline-heap entry is registered against the PO (so
/// the timer tick can time the held message out — see `fire_expired_deadlines`).
pub fn ipc_send_push_blocking(
    endpoint: *mut (),
    msg: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    po: &ObjectRef,
    deadline_ns: u64,
) -> BlockSendOutcome {
    let mut g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning `endpoint` and `po`; `SCHED` held.
    let outcome = unsafe { IpcChannel::send_or_queue(endpoint, msg, transfers, po) };
    match outcome {
        BlockSendOutcome::Sent { woke_edge } => {
            if woke_edge {
                // SAFETY: a successful send proves the peer is non-null; `SCHED` held.
                let peer = unsafe { IpcChannel::peer_of(endpoint) };
                if !peer.is_null() {
                    signal_ipc_endpoint(&mut g, peer);
                }
            }
            // Delivered synchronously — complete the PO now (no waiters yet).
            signal_pending_op(&mut g, po.as_ptr(), 0);
        }
        BlockSendOutcome::Queued if deadline_ns != u64::MAX => {
            // `BlockBounded`: register the delivery deadline against the PO. The
            // held send lives on the peer (receiving) endpoint.
            // SAFETY: the send queued, proving the peer is non-null; `SCHED` held.
            let peer = unsafe { IpcChannel::peer_of(endpoint) };
            // Heap-full degrades to an unbounded `Block` (the message still
            // delivers, just without a timeout) — the reserve (16) far exceeds
            // realistic pending sends, so this is a pathological edge only.
            let _ = deadline::push(
                &mut g.deadlines,
                deadline::Entry {
                    deadline_ns,
                    target: po.as_ptr() as usize,
                    kind: deadline::DeadlineKind::PendingSend,
                    channel: peer as usize,
                },
            );
        }
        _ => {}
    }
    outcome
}

/// Inspect `endpoint`'s receive side under `SCHED` without dequeuing — so the
/// empty-poll `WouldBlock` path allocates no bounce buffer.
pub fn ipc_recv_peek(endpoint: *mut ()) -> RecvState {
    let _g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning `endpoint`; `SCHED` held.
    unsafe { IpcChannel::recv_peek(endpoint) }
}

/// Pop the oldest message from `endpoint`'s inbox into `dst` under `SCHED`,
/// **moving** its transferred-handle references into `out`. Returns `false` if it
/// was drained between the peek and now. The caller copies `dst` out to user
/// memory and installs/drops the `out` transfers **after** this returns (never
/// under the lock — `ObjectRef` Drop / `allocate` must not run under `SCHED`).
/// Returns `(popped, promoted_po)`: `popped` is `false` if the inbox was drained
/// between peek and pop. Popping frees a ring slot, so a held blocking sender (a
/// `Block`/`BlockBounded` send waiting for space) is **promoted** into the ring
/// and its `PendingOperation` completed (status 0) under `SCHED`; its returned
/// reference must be dropped by the caller **outside** `SCHED` (no `ObjectRef`
/// Drop under the lock).
/// Pop into `dst`; on success, sweep any **timed-out** held sends into
/// `reclaimed` (the caller drops them outside `SCHED`) and promote the oldest
/// **live** held sender into the freed slot, completing its `PendingOperation`
/// (and dropping its pending-send deadline). Returns `(popped, promoted_po)`; the
/// `promoted_po` and the `reclaimed` entries are released by the caller outside
/// `SCHED`.
pub fn ipc_recv_pop_into(
    endpoint: *mut (),
    dst: &mut StoredMsg,
    out: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    reclaimed: &mut [Option<ReclaimedSend>; IpcChannel::MAX_PENDING_SENDS],
) -> (bool, Option<ObjectRef>) {
    let mut g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning `endpoint`; `SCHED` held.
    let popped = unsafe { IpcChannel::recv_pop_into(endpoint, dst, out) };
    if !popped {
        return (false, None);
    }
    // SAFETY: `endpoint` is pinned; `SCHED` held. A slot just freed — sweep
    // timed-out sends and promote the oldest live one.
    let promoted = unsafe { IpcChannel::promote_pending_send(endpoint, reclaimed) };
    if let Some(ref po) = promoted {
        // Delivered before its deadline: drop its pending-send deadline entry
        // (idempotent — a plain `Block` send registered none), then complete it.
        deadline::remove(
            &mut g.deadlines,
            po.as_ptr() as usize,
            deadline::DeadlineKind::PendingSend,
        );
        signal_pending_op(&mut g, po.as_ptr(), 0);
    }
    (true, promoted)
}

/// An IPC endpoint is being destroyed (its last handle closed). Under `SCHED`,
/// null the surviving peer's back-pointer to us and wake its blocked receivers
/// (which then observe `PeerClosed`). Called only from
/// [`IpcChannel::drop`](crate::object::IpcChannel) — see that type's docs for
/// the no-use-after-free argument and the "refs released only outside `SCHED`"
/// invariant that makes taking the lock here sound.
pub fn ipc_endpoint_closing(endpoint: *mut ()) {
    let mut g = SCHED.lock();
    // SAFETY: `endpoint` is the object being dropped (still valid memory);
    // `SCHED` held.
    let peer = unsafe { IpcChannel::peer_of(endpoint) };
    if !peer.is_null() {
        // SAFETY: `peer` is the surviving endpoint, kept alive by its own handle
        // / a waiter's `ObjectRef`; `SCHED` held.
        unsafe { IpcChannel::clear_peer(peer) };
        signal_ipc_endpoint(&mut g, peer);
    }
    // Complete every blocking sender held on THIS endpoint with `PeerClosed`:
    // our receive ring is gone, so their messages can never be delivered. We
    // only *signal* them here (waking the senders); each held entry's message,
    // transfers, and PO reference are released when this endpoint's `Inner`
    // drops, immediately after this returns — outside `SCHED`.
    let mut pos = [core::ptr::null_mut(); IpcChannel::MAX_PENDING_SENDS];
    // SAFETY: `endpoint` is valid; `SCHED` held.
    let n = unsafe { IpcChannel::pending_send_pos(endpoint, &mut pos) };
    for &po in &pos[..n] {
        // Drop each held send's pending-send deadline entry **before** the
        // endpoint is freed — otherwise a still-live `BlockBounded` deadline could
        // fire later and dereference this now-dead `channel`. Idempotent (a
        // plain `Block` send, or one already timed out, has no live entry).
        deadline::remove(&mut g.deadlines, po as usize, deadline::DeadlineKind::PendingSend);
        signal_pending_op(&mut g, po, crate::syscall::error::KError::PeerClosed as i32);
    }
}

/// Point the ring-0 trap stack (`TSS.RSP0`) and the per-CPU syscall stack at
/// `obj`'s kernel stack, so a ring-3 → ring-0 transition (syscall / trap / IRQ)
/// from this thread lands on **its** kernel stack — not a sibling's. Called on
/// every switch-in (all three switch sites); a no-op for stackless boot/idle
/// threads that never trap from ring 3. With a single user thread this was set
/// once by `thread_enter`; multiple user threads require re-arming on each
/// switch-in (or a resumed thread traps onto a stale stack → `#DF`). Caller
/// holds `SCHED`; the writes are arch register/per-CPU stores, no locks.
unsafe fn arm_kernel_stack_for(obj: *mut ()) {
    // SAFETY: `obj` is the pinned incoming thread; `kstack_top` reads its field
    // under `SCHED`. Setting TSS.RSP0 / the syscall stack are arch stores.
    if let Some(ktop) = unsafe { Thread::kstack_top(obj) } {
        Cpu::set_kernel_stack(ktop);
        crate::arch::set_syscall_kernel_stack(ktop);
    }
}

/// The common context-switch tail shared by every voluntary/involuntary
/// switch: [`block_current_and_switch`], [`switch_to_next`], [`finish_exit`],
/// and [`suspend_with_fault`]. The caller must have already re-homed the
/// outgoing thread (onto `ready` / `blocked` / `suspended` / `reap`), set the
/// incoming thread's state to `Running`, and installed it as `current`, all
/// under the held `SCHED` guard `g`. This consumes `g`.
///
/// `out_slot` is the outgoing thread's saved-SP slot (where the switch stashes
/// its kernel RSP); `next_obj` is the pinned incoming thread just made current.
///
/// It drops the lock keeping interrupts masked (the cardinal
/// no-IRQ-mid-switch rule — see the module docs and
/// [`release_keeping_irqs_masked`](IrqSpinLockGuard::release_keeping_irqs_masked)),
/// re-arms the incoming thread's trap/syscall kernel stack, loads its
/// page-table root **before** switching away (so a dying thread's CR3 is off
/// its soon-to-be-freed root), and performs the stack switch. On a resuming
/// caller, control returns here and the caller's own captured interrupt state
/// is restored; a terminal caller ([`finish_exit`]) never switches back, so
/// the restore is simply never reached.
///
/// Factoring this once means the four parking dispositions cannot drift apart
/// — a divergence in this sequence (e.g. forgetting `arm_kernel_stack_for` or
/// the CR3 load on one path) would be a latent `#DF`/corruption bug.
///
/// # Safety
/// `out_slot` must be the saved-SP slot of the (pinned) outgoing thread and
/// `next_obj` the pinned incoming thread the caller just made `current`, both
/// under the `SCHED` hold being released here. Single-CPU: nothing else
/// touches either thread's saved SP across the switch.
unsafe fn switch_into(
    g: IrqSpinLockGuard<'_, SchedState>,
    out_slot: *mut u64,
    next_obj: *mut (),
) {
    // SAFETY: `next_obj` is pinned (now `current`) and `SCHED` is still held
    // here, satisfying the Thread accessor contract for these reads.
    let next_sp = unsafe { Thread::saved_sp(next_obj) };
    let next_root = unsafe { resolve_root(next_obj) };
    // Drop the lock but keep interrupts masked across the switch. `saved_if`
    // is this thread's prior interrupt state, restored when it next resumes.
    let saved_if = g.release_keeping_irqs_masked();
    // SAFETY: `next_obj` is the pinned incoming thread (re-arm its trap/syscall stack).
    unsafe { arm_kernel_stack_for(next_obj) };
    // SAFETY: `next_root` is a fully-formed PML4 (boot root, or a process root
    // with the kernel half inherited); all kernel stacks are mapped in every
    // root, so switching CR3 before the stack swap is sound. Loading it before
    // the switch also ensures a dying thread leaves CR3 on the incoming root
    // before it is reaped (its `AddressSpace::Drop` may free the old PML4).
    unsafe { Paging::set_page_table(next_root) };
    // SAFETY: lock released; interrupts masked; `out_slot` points into the
    // re-homed outgoing thread (pinned) and `next_sp` is the saved RSP of the
    // now-current thread (pinned). Single-CPU: nothing else touches either.
    unsafe { context_switch(out_slot, next_sp) };
    // Resumed (cooperative path): restore the interrupt state this thread had
    // on entry. On the preemptive path the resume returns into the timer-stub
    // epilogue (which `iretq`s IF back) and `saved_if` is false → no-op. A
    // terminal caller (`finish_exit`) never reaches this.
    // SAFETY: ring-0; restoring this thread's own captured interrupt state.
    unsafe { Cpu::interrupts_restore(saved_if) };
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

    // Park prev in `blocked` (NOT ready/idle) — its `ObjectRef` keeps it alive.
    debug_assert!(g.blocked.len() < g.blocked.capacity());
    g.blocked.try_push(prev).expect("blocked list within reserve");
    g.current = Some(next);

    // Switch into `next`; we resume here (lock-free) when a waker moves us back
    // to `ready` and the scheduler later picks us.
    // SAFETY: `prev_slot` is the outgoing (now-`Blocked`, pinned-in-`blocked`)
    // thread's saved-SP slot; `next_obj` is the pinned incoming thread.
    unsafe { switch_into(g, prev_slot, next_obj) };
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
                deadline::Entry {
                    deadline_ns,
                    target: me_ptr as usize,
                    kind: deadline::DeadlineKind::Thread,
                    channel: 0,
                },
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
            deadline::remove(&mut g.deadlines, me_ptr as usize, deadline::DeadlineKind::Thread);
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
        deadline::remove(&mut g.deadlines, timer as usize, deadline::DeadlineKind::Timer);
        unsafe { Timer::set_in_heap(timer, false) };
    }
    // SAFETY: live Timer, `SCHED` held.
    unsafe { Timer::set_armed(timer, deadline_ns, interval_ns) };
    if deadline_ns != 0 {
        if deadline::push(
            &mut g.deadlines,
            deadline::Entry {
                deadline_ns,
                target: timer as usize,
                kind: deadline::DeadlineKind::Timer,
                channel: 0,
            },
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

    // SAFETY: `prev_slot` is the re-homed outgoing (now-`Ready`, pinned)
    // thread's saved-SP slot; `next_obj` is the pinned incoming thread.
    unsafe { switch_into(g, prev_slot, next_obj) };
}

/// The action a supervisor's `sys_exception_resume` requests for a thread
/// suspended on a fault, returned by [`suspend_with_fault`] when it resumes.
/// Phase 1 has the two terminal dispositions; `ResumeSkip`/`ModifyAndResume`,
/// the auto-terminate timeout, and the debugger priority chain are Phase 2.
pub enum ResumeDisposition {
    /// Re-enter the faulting instruction (tag `0`). The stub `iretq`s the
    /// unmodified frame — without fault-fixing it simply re-faults, so this is
    /// the mechanism for a supervisor that has repaired the fault's cause.
    Resume,
    /// Terminate the thread with `code` (tag `2`): the resume path calls
    /// [`exit_thread`] with a crashed status carrying `code`.
    Terminate(i32),
}

/// Deliver a `ChildExited { pid, status }` to `me_obj`'s process's parent
/// channel, if it has one, and wake that channel's waiters. Caller holds
/// `SCHED`. Borrows the channel pointer (never an owned `ObjectRef` — no
/// destructor under `SCHED`); enqueue is allocation-free. Shared by
/// [`exit_thread`] (last thread) and [`exit_process`].
fn deliver_child_exited(g: &mut SchedState, me_obj: *mut (), status: ExitStatus) {
    let info: Option<(*mut (), u32)> = {
        // SAFETY: `me_obj` is the running thread, pinned, lock held.
        let th = unsafe { &*(me_obj as *const Thread) };
        th.process_ref().and_then(|p| {
            // SAFETY: `p` pins a live Process; read its parent channel + pid.
            let proc = unsafe { &*(p.as_ptr() as *const crate::object::Process) };
            proc.parent_notif_ptr().map(|chan| (chan, proc.pid_u32()))
            // `p` (a cloned ObjectRef, never the last) drops here: a plain
            // atomic decrement, no destructor, safe under SCHED.
        })
    };
    if let Some((chan, child_pid)) = info {
        let notif = Notification::child_exited(child_pid, status);
        // SAFETY: `chan` is the parent's channel, kept alive by this process's
        // `parent_notif` reference (still held — `me` is not yet reaped); SCHED
        // held; no allocation (queue pre-reserved).
        let _edge = unsafe { NotificationChannel::enqueue(chan, notif) };
        signal_channel(g, chan);
    }
}

/// `true` if any thread in `ready`/`blocked`/`suspended` belongs to process
/// `my_pid`. Caller holds `SCHED`. The exiting thread has already been taken
/// out of `current`, and single-CPU only one thread is `current`, so a `false`
/// here means the caller is genuinely the process's last live thread.
fn has_live_siblings(g: &SchedState, my_pid: u32) -> bool {
    // SAFETY: each entry pins a live Thread; `SCHED` held — a shared read of
    // `owner_pid` is sound (no `&mut` taken).
    let same = |r: &ObjectRef| unsafe { &*(r.as_ptr() as *const Thread) }.owner_pid() == my_pid;
    g.ready.iter().any(same) || g.blocked.iter().any(same) || g.suspended.iter().any(same)
}

/// Reap every thread of process `my_pid` parked in `list` (a `ready` or
/// `suspended` queue — neither registers on wait objects): mark each `Exited`
/// and move its `ObjectRef` into `reap`. Caller holds `SCHED`.
fn reap_matching(list: &mut KVec<ObjectRef>, reap: &mut KVec<ObjectRef>, my_pid: u32) {
    // SAFETY (loop): each entry pins a live Thread; `SCHED` held.
    while let Some(i) = list
        .iter()
        .position(|r| unsafe { &*(r.as_ptr() as *const Thread) }.owner_pid() == my_pid)
    {
        let r = list.remove(i);
        // SAFETY: `r` pins the thread; `SCHED` held.
        unsafe { Thread::set_state(r.as_ptr(), ThreadState::Exited) };
        reap.try_push(r).expect("reap within reserve");
    }
}

/// Reap every `blocked` thread of process `my_pid`, first unregistering it from
/// the wait objects + deadline heap it is parked on (so no waiter list or heap
/// entry is left dangling at a reaped thread). Caller holds `SCHED`.
fn reap_blocked_matching(
    blocked: &mut KVec<ObjectRef>,
    deadlines: &mut KVec<deadline::Entry>,
    reap: &mut KVec<ObjectRef>,
    my_pid: u32,
) {
    // SAFETY (loop): each entry pins a live Thread; `SCHED` held.
    while let Some(i) = blocked
        .iter()
        .position(|r| unsafe { &*(r.as_ptr() as *const Thread) }.owner_pid() == my_pid)
    {
        let r = blocked.remove(i);
        let obj = r.as_ptr();
        // Unregister from every object this thread was waiting on, mirroring
        // `wait_on`'s resume-side cleanup.
        let mut snap = [(0usize, false); MAX_WAIT_HANDLES];
        // SAFETY: live Thread, `SCHED` held.
        let n = unsafe { Thread::wait_snapshot(obj, &mut snap) };
        for &(o, _) in &snap[..n] {
            // SAFETY: the wait object is kept alive by this thread's `sys_wait`
            // `ObjectRef`s (released only when it unblocks); `SCHED` held.
            unsafe { obj_remove_waiter(o as *mut (), obj) };
        }
        // SAFETY: live Thread, `SCHED` held.
        if unsafe { Thread::wait_has_deadline(obj) } {
            deadline::remove(deadlines, obj as usize, deadline::DeadlineKind::Thread);
        }
        // SAFETY: live Thread, `SCHED` held.
        unsafe {
            Thread::wait_clear(obj);
            Thread::set_state(obj, ThreadState::Exited);
        }
        reap.try_push(r).expect("reap within reserve");
    }
}

/// Mark `me` (the current thread, already taken out of `current`) `Exited`,
/// park it in `reap` for deferred stack reclamation, and switch away forever.
/// Shared tail of [`exit_thread`] and [`exit_process`]. The `Thread` and its
/// kernel stack cannot be freed here — this code is still running on that stack
/// — so the next scheduler entry reaps them.
fn finish_exit(mut g: IrqSpinLockGuard<'_, SchedState>, me: ObjectRef) -> ! {
    let me_obj = me.as_ptr();
    // SAFETY: `me` is the running thread, pinned, lock held. (The idle thread
    // never exits, so `me` is never the idle thread.)
    unsafe { Thread::set_state(me_obj, ThreadState::Exited) };
    let me_slot = unsafe { Thread::saved_sp_mut_ptr(me_obj) };
    // Park self for deferred reclamation.
    g.reap.try_push(me).expect("reap within reserve");

    // Switch to the next ready thread, else the idle thread (which always
    // exists post-init and is parked here, since `me` was current, not idle).
    let next = match dequeue_front(&mut g) {
        Some(n) => n,
        None => g.idle.take().expect("idle thread exists after init"),
    };
    let next_obj = next.as_ptr();
    // SAFETY: next is pinned, becoming current; lock held.
    unsafe { Thread::set_state(next_obj, ThreadState::Running) };
    g.current = Some(next);

    // Switch away forever. `switch_into` loads the incoming root before the
    // stack swap, so when the last user thread exits CR3 is restored to the
    // boot root before this (parked-in-`reap`) thread is reaped — its
    // `AddressSpace::Drop` frees the PML4 CR3 would otherwise still reference.
    // SAFETY: `me_slot` is our own (now-`Exited`, pinned-in-`reap`) saved-SP
    // slot — written by the switch and never read again; `next_obj` is the
    // pinned incoming thread. We never resume, so the restore inside
    // `switch_into` is never reached.
    unsafe { switch_into(g, me_slot, next_obj) };
    unreachable!("switched away from an exited thread");
}

/// Terminate the **current thread** with exit `status` and switch away forever.
/// Used by `sys_thread_exit`, the resume-`Terminate` fault path, and kernel
/// thread bodies / the boot thread.
///
/// A `ChildExited { pid, status }` is delivered to the process's parent channel
/// **iff this is its last thread** (no live sibling remains) — a process with
/// other running threads has not exited, so no notification fires. Kernel/boot
/// threads have no process and produce none. Delivery happens here, before
/// parking, so a parent blocked in `sys_wait` wakes promptly.
pub fn exit_thread(status: ExitStatus) -> ! {
    // Reclaim any earlier exited thread first, so `reap` has room for us.
    reap_pending();

    let mut g = SCHED.lock();
    let me = g.current.take().expect("current set");
    let me_obj = me.as_ptr();

    // SAFETY: `me` is the running thread, pinned, lock held.
    let me_pid = unsafe { &*(me_obj as *const Thread) }.owner_pid();
    // SAFETY: same.
    let has_process = unsafe { &*(me_obj as *const Thread) }.process_ref().is_some();
    if has_process && !has_live_siblings(&g, me_pid) {
        deliver_child_exited(&mut g, me_obj, status);
    }

    finish_exit(g, me);
}

/// Terminate the **whole process** of the current thread with exit `status`:
/// tear down every sibling thread (scan `ready`/`blocked`/`suspended` by
/// `owner_pid`, unregistering blocked siblings from their waits first), then
/// exit the current thread **with** a `ChildExited`. Used by `sys_process_exit`.
///
/// The per-process thread list that would let an external killer find these
/// threads without a scan lands in Phase 2; the `owner_pid` scan is correct for
/// self-exit now (single-CPU, all sibling threads are parked off the run queue
/// while this one runs). A kernel/boot thread (no process) degrades to a plain
/// [`exit_thread`]-style exit (no siblings, no notification).
pub fn exit_process(status: ExitStatus) -> ! {
    // Reclaim any earlier exited thread first, so `reap` has room.
    reap_pending();

    let mut g = SCHED.lock();
    let me = g.current.take().expect("current set");
    let me_obj = me.as_ptr();

    // SAFETY: `me` is the running thread, pinned, lock held.
    let me_pid = unsafe { &*(me_obj as *const Thread) }.owner_pid();
    // SAFETY: same.
    let has_process = unsafe { &*(me_obj as *const Thread) }.process_ref().is_some();
    if has_process {
        // Reborrow the guard once as `&mut SchedState` so the field borrows
        // below are disjoint (through the guard's `Deref` each `&mut g.field`
        // would borrow the whole guard, conflicting in one call).
        let st: &mut SchedState = &mut g;
        reap_matching(&mut st.ready, &mut st.reap, me_pid);
        reap_matching(&mut st.suspended, &mut st.reap, me_pid);
        reap_blocked_matching(&mut st.blocked, &mut st.deadlines, &mut st.reap, me_pid);
        // The process is ending: always deliver ChildExited (we are now its
        // last thread).
        deliver_child_exited(st, me_obj, status);
    }

    finish_exit(g, me);
}

/// Suspend the **current thread** after a ring-3 fault: deliver `notif` to its
/// process's notification channel (waking the supervisor), record the
/// `ExceptionFrame` at `frame_ptr` (its address on this thread's kernel stack),
/// park the thread in `suspended`, and switch away — mirroring
/// [`block_current_and_switch`], but parked for `sys_exception_resume` rather
/// than a waker. Returns the [`ResumeDisposition`] the supervisor chose once the
/// thread is made runnable again. Called from the exception dispatchers.
/// Emit a last-ditch kernel diagnostic for a ring-3 fault that **stranded the
/// scheduler** — one that left no runnable thread to receive the fault notification
/// and call `sys_exception_resume`, so the system would idle forever. Without this
/// it is a silent hang (notably an init/pid-1 crash). Called only from the
/// no-runnable-thread branch of [`suspend_with_fault`], so a serviced fault (whose
/// supervisor waiter was just made runnable) never reaches here.
///
/// Uses the unsynchronized emergency serial writer — lock-free, mirroring the
/// ring-0 `dump_and_halt` path. Sound here: a userspace fault never holds `SERIAL`,
/// and the caller holds `SCHED` so interrupts are masked. Reads only neutral data
/// (pid/tid from the thread; kind/addr from the `Notification`), staying out of the
/// arch-private `ExceptionFrame`.
fn report_stranded_fault(me_obj: *mut (), notif: &Notification) {
    use crate::libkern::notification::{
        KIND_DIVIDE_BY_ZERO, KIND_ILLEGAL_INSN, KIND_SEG_FAULT, KIND_STACK_OVERFLOW,
    };
    use core::fmt::Write;
    // SAFETY: `me_obj` is the running (faulting) thread, pinned, `SCHED` held.
    let th = unsafe { &*(me_obj as *const Thread) };
    let kind = match notif.kind() {
        KIND_SEG_FAULT => "segfault",
        KIND_ILLEGAL_INSN => "illegal-instruction",
        KIND_DIVIDE_BY_ZERO => "divide-by-zero",
        KIND_STACK_OVERFLOW => "stack-overflow",
        _ => "fault",
    };
    let mut w = crate::arch::serial::emergency_writer();
    let _ = writeln!(
        w,
        "\n*** unhandled ring-3 fault (no thread left to resume it): pid {} tid {} {} @ {:#018x} ***",
        th.owner_pid(),
        th.tid(),
        kind,
        notif.fault_addr(),
    );
}

pub fn suspend_with_fault(frame_ptr: usize, notif: Notification) -> ResumeDisposition {
    // Reclaim any earlier exited thread first (we may be the only scheduler
    // entry for a while if we suspend), off the rank-1 lock.
    reap_pending();

    let me_obj;
    {
        let mut g = SCHED.lock();
        let me = g.current.take().expect("current set");
        me_obj = me.as_ptr();

        // Deliver the fault notification to the faulting process's channel
        // (borrowed pointer — no `ObjectRef` destructor under `SCHED`).
        let chan: Option<*mut ()> = {
            // SAFETY: `me` is the running thread, pinned, lock held.
            let th = unsafe { &*(me_obj as *const Thread) };
            th.process_ref().and_then(|p| {
                // SAFETY: `p` pins a live Process; read its channel pointer.
                let proc = unsafe { &*(p.as_ptr() as *const crate::object::Process) };
                proc.notification_channel_ptr()
            })
        };
        if let Some(c) = chan {
            // SAFETY: `c` is the channel the current Process owns a ref on (alive
            // at least until this thread is reaped); SCHED held; queue
            // pre-reserved.
            let _edge = unsafe { NotificationChannel::enqueue(c, notif) };
            signal_channel(&mut g, c);
        }

        // Record the frame, mark Suspended, park off the run queue. The
        // `ExceptionFrame` stays on this (now-frozen) kernel stack until resume.
        // SAFETY: `me` is the running thread, pinned, lock held.
        unsafe {
            Thread::set_exception_frame(me_obj, frame_ptr);
            Thread::set_state(me_obj, ThreadState::Suspended);
        }
        let me_slot = unsafe { Thread::saved_sp_mut_ptr(me_obj) };
        debug_assert!(g.suspended.len() < g.suspended.capacity());
        g.suspended.try_push(me).expect("suspended list within reserve");

        // Switch to the next runnable thread (mirrors block_current_and_switch).
        // If nothing is runnable, this fault **stranded the scheduler**: no thread
        // remained to receive the notification and `sys_exception_resume` us, so the
        // system would idle forever (a silent hang — notably an init/pid-1 crash).
        // Emit a last-ditch diagnostic. A serviced fault (the supervisor's waiter
        // was just woken by `signal_channel` above) takes the `Some` branch and
        // stays quiet — so this fires only for genuinely-unhandled faults.
        let next = match dequeue_front(&mut g) {
            Some(n) => n,
            None => {
                report_stranded_fault(me_obj, &notif);
                g.idle.take().expect("idle thread exists after init")
            }
        };
        let next_obj = next.as_ptr();
        // SAFETY: next is pinned, becoming current; lock held.
        unsafe { Thread::set_state(next_obj, ThreadState::Running) };
        g.current = Some(next);

        // Switch into `next`; we resume here when `sys_exception_resume` moves
        // us `suspended`→`ready` and the scheduler switches us in.
        // SAFETY: `me_slot` is our own (now-`Suspended`, pinned-in-`suspended`)
        // saved-SP slot; `next_obj` is the pinned incoming thread.
        unsafe { switch_into(g, me_slot, next_obj) };
    }

    // Read (and clear) the disposition the resume stored on us.
    let (tag, code) = {
        let _g = SCHED.lock();
        // SAFETY: we run on `me_obj` again (back in `current`), `SCHED` held.
        unsafe { Thread::take_disposition(me_obj) }
    };
    match tag {
        2 => ResumeDisposition::Terminate(code),
        _ => ResumeDisposition::Resume,
    }
}

/// Resume a thread suspended on a fault: record the supervisor's disposition
/// (`disp_tag`/`disp_code`) on it and move it `suspended`→`ready`. Returns
/// `false` (no-op) if `thread` is not currently `Suspended` — the backstop
/// `sys_exception_resume` maps to `InvalidArgument`. Caller need not hold
/// `SCHED` (taken here).
pub fn resume_suspended(thread: *mut (), disp_tag: u8, disp_code: i32) -> bool {
    let mut g = SCHED.lock();
    let Some(i) = g.suspended.iter().position(|r| r.as_ptr() == thread) else {
        return false;
    };
    let r = g.suspended.remove(i);
    // SAFETY: `r` pins `thread`; `SCHED` held. The disposition is read by
    // `suspend_with_fault` when this thread next runs.
    unsafe {
        Thread::set_disposition(thread, disp_tag, disp_code);
        Thread::set_state(thread, ThreadState::Ready);
    }
    debug_assert!(g.ready.len() < g.ready.capacity());
    g.ready.try_push(r).expect("ready within reserve");
    true
}

/// The kernel-stack address of `thread`'s captured `ExceptionFrame`, or `None`
/// if it is not currently `Suspended`. Used by `sys_thread_get_registers`; the
/// thread stays parked while suspended, so the frame is stable to read after
/// this returns (the caller drops `SCHED` before the user copy-out).
pub fn thread_exception_frame(thread: *mut ()) -> Option<usize> {
    let _g = SCHED.lock();
    // SAFETY: the caller pins `thread` via its Thread handle; `SCHED` held.
    if unsafe { Thread::state_now(thread) } != ThreadState::Suspended {
        return None;
    }
    // SAFETY: same.
    unsafe { Thread::exception_frame(thread) }
}

/// Drop every pending reaped thread **outside** the run-queue lock (their
/// `KernelStack` `Drop` takes rank-6 allocator locks). Idempotent; called at the
/// top of [`yield_now`]/[`exit_thread`]/[`exit_process`]/[`suspend_with_fault`]
/// and by the boot drainer. The list (rather than one slot) lets a process exit
/// reap its torn-down sibling threads alongside the caller; we drain it under a
/// brief lock into a local `KVec`, then drop them all with the lock released.
pub fn reap_pending() {
    let reaped = {
        let mut g = SCHED.lock();
        core::mem::take(&mut g.reap)
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

/// The currently running [`Thread`](crate::object::Thread) object, cloned as an
/// [`ObjectRef`](crate::object::ObjectRef) that outlives the lock. During a
/// syscall this is the calling user thread — the `/proc/self/thread` kernel
/// server uses it to hand the caller a handle to itself. Takes only the rank-1
/// run-queue lock. `None` only before the first thread runs (never during a
/// syscall). Unlike [`current_process`], `current` *is* the Thread `ObjectRef`,
/// so it is cloned directly.
pub fn current_thread() -> Option<ObjectRef> {
    let g = SCHED.lock();
    // SAFETY-equivalent note: `ObjectRef::clone` is an atomic refcount bump (no
    // `&mut`, no drop), sound to perform under the run-queue lock.
    g.current.clone()
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
    let descent: Option<(u64, u64, u64, [u64; 4])> = {
        let g = SCHED.lock();
        let cur = g.current.as_ref().expect("current set when a thread runs");
        let obj = cur.as_ptr();
        // SAFETY: `current` is pinned alive and we hold the lock.
        match unsafe { Thread::user_entry(obj) } {
            Some((entry, user_sp)) => {
                let ktop = unsafe { Thread::kstack_top(obj) }
                    .expect("a user thread has a kernel stack");
                let boot = unsafe { Thread::user_boot_args(obj) };
                Some((entry, user_sp, ktop, boot))
            }
            None => None,
        }
    };

    match descent {
        Some((entry, user_sp, ktop, boot)) => {
            // Point the ring0 trap stack (TSS.RSP0) and the per-CPU syscall
            // stack at THIS thread's kernel stack before dropping to ring 3,
            // so syscalls/traps from ring 3 land on it. CR3 is already the
            // process address space (loaded by the scheduler on switch-in).
            Cpu::set_kernel_stack(ktop);
            crate::arch::set_syscall_kernel_stack(ktop);
            // Re-assert the per-CPU base for the upcoming first `syscall`: a
            // thread that blocked mid-syscall is switched away with the entry
            // GS-swap still in effect, so a fresh descent here must restore it
            // or the child's first syscall faults. See `arm_user_entry_cpu_base`.
            crate::arch::arm_user_entry_cpu_base();
            // SAFETY: `entry`/`user_sp` are canonical user VAs mapped X / W
            // in the active address space; the syscall fast-path is armed.
            // `boot` seeds rdi/rsi/rdx/rcx — the spawn hand-off (notif, root
            // namespace, installed endpoint, arg0).
            unsafe { crate::arch::enter_user(entry, user_sp, boot[0], boot[1], boot[2], boot[3]) }
        }
        None => {
            // Kernel thread: run the body in ring 0, then exit cleanly. Kernel
            // threads have no owning process, so no `ChildExited` is produced.
            let (entry, arg) = current_entry();
            entry(arg);
            exit_thread(ExitStatus { kind: ExitKind::Normal as u32, code: 0 });
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
            reap: KVec::new(),
            suspended: KVec::new(),
            next_tid: 1,
            next_pid: 2,
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
        deadline::Entry {
            deadline_ns,
            target,
            kind: deadline::DeadlineKind::Timer,
            channel: 0,
        }
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
        assert!(deadline::remove(&mut h, 0xB, deadline::DeadlineKind::Timer));
        assert!(!deadline::remove(&mut h, 0xB, deadline::DeadlineKind::Timer)); // gone
        assert!(!deadline::remove(&mut h, 0xA, deadline::DeadlineKind::Thread)); // wrong kind
        assert_eq!(deadline::pop_min(&mut h).unwrap().target, 0xA);
        assert_eq!(deadline::pop_min(&mut h).unwrap().target, 0xC);
        assert!(deadline::pop_min(&mut h).is_none());
    }

    #[test]
    fn heap_remove_distinguishes_kind_for_same_target() {
        init_global_heap();
        let mut h: KVec<deadline::Entry> = KVec::new();
        h.try_reserve(deadline::HEAP_RESERVE).unwrap();
        // The same `target` can hold entries of different kinds — each removable
        // independently by `(target, kind)`.
        deadline::push(
            &mut h,
            deadline::Entry {
                deadline_ns: 10,
                target: 0xA,
                kind: deadline::DeadlineKind::Thread,
                channel: 0,
            },
        )
        .unwrap();
        deadline::push(
            &mut h,
            deadline::Entry {
                deadline_ns: 20,
                target: 0xA,
                kind: deadline::DeadlineKind::PendingSend,
                channel: 0xBEEF,
            },
        )
        .unwrap();
        // Neither is a Timer.
        assert!(!deadline::remove(&mut h, 0xA, deadline::DeadlineKind::Timer));
        // Remove the Thread entry; the PendingSend one survives, channel intact.
        assert!(deadline::remove(&mut h, 0xA, deadline::DeadlineKind::Thread));
        let e = deadline::peek(&h).unwrap();
        assert_eq!(e.kind, deadline::DeadlineKind::PendingSend);
        assert_eq!(e.channel, 0xBEEF);
        assert!(deadline::remove(&mut h, 0xA, deadline::DeadlineKind::PendingSend));
        assert!(deadline::peek(&h).is_none());
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
            st.reap.try_reserve(REAP_RESERVE).unwrap();
            st.reap.try_push(inert_ref(6)).unwrap();
            st.reap.try_push(inert_ref(7)).unwrap();
            // No destroys while the refs are held.
            assert_eq!(test_probe::thread_destroys(), 0);
        } // st dropped here — every ObjectRef drops its one reference.
        assert_eq!(
            test_probe::thread_destroys(),
            7,
            "each queued/current/reaped thread destroyed exactly once",
        );
    }

    /// Build an inert Thread tagged with `owner_pid = pid` (and no owning
    /// Process / kernel stack, so no host paging is needed). The teardown tests
    /// only read `owner_pid`, so this is enough to exercise the `pid` scans.
    fn inert_user_ref(tid: u32, pid: u32) -> ObjectRef {
        into_objref(Thread::try_new(tid, pid).unwrap())
    }

    #[test]
    fn resume_suspended_moves_to_ready_and_sets_disposition() {
        init_global_heap();
        let mut st = test_state();
        st.ready.try_reserve(READY_RESERVE).unwrap();
        st.suspended.try_reserve(BLOCKED_RESERVE).unwrap();
        let th = inert_user_ref(1, 1);
        let obj = th.as_ptr();
        // SAFETY: pinned, single-threaded test; mimic a suspended thread.
        unsafe {
            Thread::set_state(obj, ThreadState::Suspended);
            Thread::set_exception_frame(obj, 0xdead_beef);
        }
        st.suspended.try_push(th).unwrap();

        // resume_suspended works against the global SCHED, so drive the list ops
        // directly here (mirroring its body) to keep the test lock-free.
        let i = st.suspended.iter().position(|r| r.as_ptr() == obj).unwrap();
        let r = st.suspended.remove(i);
        // SAFETY: pinned, single-threaded.
        unsafe {
            Thread::set_disposition(obj, 2, 7);
            Thread::set_state(obj, ThreadState::Ready);
        }
        st.ready.try_push(r).unwrap();

        assert!(st.suspended.is_empty());
        assert_eq!(st.ready.len(), 1);
        // SAFETY: pinned. take_disposition reads (tag, code) and clears the frame.
        let (tag, code) = unsafe { Thread::take_disposition(obj) };
        assert_eq!((tag, code), (2, 7));
        // SAFETY: pinned — the frame was cleared by take_disposition.
        assert!(unsafe { Thread::exception_frame(obj) }.is_none());
    }

    #[test]
    fn has_live_siblings_scans_all_parked_lists() {
        init_global_heap();
        let mut st = test_state();
        st.ready.try_reserve(READY_RESERVE).unwrap();
        st.blocked.try_reserve(BLOCKED_RESERVE).unwrap();
        st.suspended.try_reserve(BLOCKED_RESERVE).unwrap();

        // pid 1 has a sibling parked in `suspended`; pid 2 has none anywhere.
        st.ready.try_push(inert_user_ref(1, 1)).unwrap();
        st.suspended.try_push(inert_user_ref(2, 1)).unwrap();
        st.blocked.try_push(inert_user_ref(3, 3)).unwrap();

        assert!(has_live_siblings(&st, 1));
        assert!(has_live_siblings(&st, 3));
        assert!(!has_live_siblings(&st, 2), "no pid-2 thread is parked");
    }

    #[test]
    fn reap_matching_moves_only_same_pid_threads() {
        init_global_heap();
        test_probe::reset();
        {
            let mut st = test_state();
            st.ready.try_reserve(READY_RESERVE).unwrap();
            st.reap.try_reserve(REAP_RESERVE).unwrap();
            st.ready.try_push(inert_user_ref(1, 1)).unwrap();
            st.ready.try_push(inert_user_ref(2, 2)).unwrap();
            st.ready.try_push(inert_user_ref(3, 1)).unwrap();

            reap_matching(&mut st.ready, &mut st.reap, 1);
            assert_eq!(st.reap.len(), 2, "both pid-1 threads reaped");
            assert_eq!(st.ready.len(), 1, "the pid-2 thread stays");
            // SAFETY: pinned, single-threaded.
            let left = unsafe { &*(st.ready.iter().next().unwrap().as_ptr() as *const Thread) };
            assert_eq!(left.owner_pid(), 2);
            // No destroys yet — the refs are alive in `reap`.
            assert_eq!(test_probe::thread_destroys(), 0);
        } // st dropped — reap + ready release their refs.
        assert_eq!(test_probe::thread_destroys(), 3);
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
