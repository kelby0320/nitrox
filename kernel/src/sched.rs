//! Preemptive round-robin scheduler for kernel and user threads, with per-CPU run queues.
//!
//! A thread runs until it calls [`yield_now`]/[`exit`] (the **cooperative** path) or the timer
//! IRQ's [`on_timer_tick`] expires its quantum (the **preemptive** path). Both funnel through
//! [`switch_to_next`] over [`context_switch`](crate::arch::context_switch) and the [`Thread`]
//! kernel object. An [idle thread](idle_body) runs (`hlt`) whenever nothing else is ready.
//!
//! ## Blocking and deadlines (wait queues)
//!
//! In `sys_wait`, [`wait_on`] registers the thread as a waiter on each target object and
//! (optionally) on the deadline min-heap ([`deadline`]), then [`block_current_and_switch`]
//! parks it in [`SchedState::blocked`]. A waker calls [`make_runnable`]. Deadlines are
//! checked on the tick ([`on_timer_tick`] → [`fire_expired_deadlines`]) and waiters woken
//! **directly** under `SCHED`, no DPC. Wait, timer and blocked state all live under the one
//! rank-1 lock, so there is no lost-wakeup window (`kernel/docs/lock-ordering.md`).
//!
//! ## The run-queue lock, interrupts, and the switch
//!
//! [`SCHED`] is the **rank-1** run-queue lock (`kernel/docs/lock-ordering.md`), an
//! [`IrqSpinLock`](crate::libkern::IrqSpinLock): it `cli`s before acquiring, so a thread
//! holding it cannot be preempted and the timer handler can never find it held by the
//! context it interrupted.
//!
//! The lock is **dropped before every [`context_switch`]** and re-acquired fresh on resume —
//! never carried across a stack switch. Interrupts must stay masked **across** the switch (a
//! timer mid-switch would corrupt a half-swapped stack), so the switch core drops the lock
//! via [`release_keeping_irqs_masked`] and restores the interrupt state on resume. The
//! preemptive path is already IF=0 (the timer gate clears it) and resumes by returning into
//! the timer-stub epilogue, which `iretq`s the original interrupt state back.
//!
//! Allocation never happens under the lock: [`init`] installs a pre-reserved run queue (and
//! the idle thread), and the heavy work in [`spawn`] (stack allocation, frame fabrication)
//! runs before the enqueue lock is taken. Reaping an exited thread's stack (rank-6 allocator
//! locks via [`KernelStack`](crate::mm)'s `Drop`) runs outside the rank-1 lock, and never
//! from the timer handler, which performs no allocation.
//!
//! [`release_keeping_irqs_masked`]: crate::libkern::IrqSpinLockGuard::release_keeping_irqs_masked

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::arch::cpu::ArchCpu;
use crate::arch::paging::ArchPaging;
use crate::arch::timer::ArchTimer;
use crate::arch::{Cpu, MAX_CPUS, Paging, context_switch};
use crate::libkern::handle::{KObjectType, Rights};
use crate::libkern::ipc::IPC_HANDLE_MAX;
use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, IrqSpinLock, IrqSpinLockGuard, KBox, KVec};
use crate::libkern::{ExitKind, ExitStatus, Notification};
use crate::mm::PhysAddr;
use crate::object::userspace_server::{PendingFill, PendingLookup, US_PENDING_MAX};
use crate::object::{
    BlockSendOutcome, InterruptObject, IpcChannel, MAX_WAIT_HANDLES, NotificationChannel, ObjectRef, PendingOperation,
    ReclaimedSend, RecvState, SchedClass, SendOutcome, StoredMsg, Thread, ThreadEntry, ThreadState, Timer, TransferRef,
    UserspaceServerReg,
};

mod current;
mod exit;
mod ipc;
mod objects;
mod online;
mod placement;
mod preempt;
mod spawn;
mod timers;
mod wait;

pub use current::*;
pub use exit::*;
pub use ipc::*;
pub use objects::*;
pub use online::*;
use placement::*;
pub use preempt::*;
pub use spawn::*;
pub use timers::*;
pub use wait::*;

// `Timer` above is the kernel object; the hardware monotonic clock is
// `crate::arch::Timer::read_ns()` (the `ArchTimer` trait provides `read_ns`).

/// The most threads the scheduler holds at once, the idle threads aside: a spawn past it is
/// refused. **Every list a thread can be on is reserved to hold this many** ([`thread_list`]):
/// a run queue, since affinity can pin every thread to one CPU, and a reap list, since an exit
/// sweeps a whole process into one. A thread is on one list at a time, so a wake, a block, a
/// fault or an exit never finds its list full — none of them can refuse, and growing a list
/// under the rank-1 lock is an allocation.
const MAX_THREADS: usize = 512;

/// Periodic scheduler tick: 10 ms (100 Hz), matching the PIT calibration window.
pub const TICK_NS: u64 = 10_000_000;

/// Ticks per scheduling quantum: one tick, the simplest correct round-robin policy.
const QUANTUM_TICKS: u32 = 1;

/// Thread id for the idle thread, a sentinel outside the `next_tid` range. Idle identity is
/// by object address (`SchedState::idle_addr`).
const IDLE_TID: u32 = u32::MAX;

/// Pre-reserved capacity for [`SchedState::deferred_drops`]. Sized to twice the only
/// producer's burst (the entropy seed wake parks ≤ [`crate::entropy::SEED_WAITERS_MAX`]
/// refs, once per boot); [`reap_pending`] drains it in thread context.
const DEFERRED_DROP_RESERVE: usize = 2 * crate::entropy::SEED_WAITERS_MAX;
/// Handle tables the [reaper](reaper_body) has closed, reported as `reaper_closed=` in
/// `/proc/sched/stats`. Exposed because the failure mode is **silence**: a reaper that
/// never woke would be covered by the per-CPU `process_ended` sweep until a busy system
/// starved that sweep. A counter a test can read makes "the reaper is alive" assertable.
pub static REAPER_CLOSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Handles closed by a process **in its own exit syscall**, reported as `exit_closed=` in
/// `/proc/sched/stats`. Like [`REAPER_CLOSED`], exposed because if exit-context teardown
/// silently stopped, the reaper would cover for it and every peer's `PeerClosed` would
/// quietly go back to arriving late.
pub static EXIT_CLOSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The deadline min-heap: armed-timer and `sys_wait`-deadline expiries keyed by absolute
/// monotonic ns, drained on each tick. A pure binary heap, host-tested.
mod deadline;
pub use deadline::{SlotHold, Slots};

/// Scheduler statistics: the per-CPU event counters behind `/proc/sched/stats`, their
/// snapshot, and the pure text formatter. Follows the **capture → format → synthesize**
/// discipline (`docs/architecture/scheduler.md` § "The stats surface"): the snapshot copies
/// plain `Copy` data under one `SCHED` hold, [`format`] renders it with no lock held. The
/// counters are plain `u64`s because every increment site already holds `SCHED`.
pub mod stats;

/// The kernel/boot page-table root, captured once at [`init`]. Threads with no per-process
/// address space run on it; it is loaded on switch-in so a dying user thread's address
/// space can be freed safely.
static BOOT_ROOT: AtomicU64 = AtomicU64::new(0);

/// Lock-free bitmask of online CPUs (dense index → bit), for subsystems that must not
/// take the top-rank `SCHED` lock — notably TLB shootdown ([`crate::tlb`]), which fires
/// from the mm layer under lower locks. Each CPU sets its own bit as it comes online.
///
/// **It mirrors the `SCHED`-guarded `cpu_online[]` on the way up but not on the way down**:
/// [`leave_online`] clears a bit here and does not touch `cpu_online[]`, which needs
/// `SCHED`, which the parking CPU may already hold. So the two disagree after a park, and
/// that is reachable on a machine that otherwise keeps running: `idt.rs`'s
/// `dump_and_halt` parks on **any ring-0 fault**, a kernel `#PF` on an AP say, and that CPU
/// is fully online at the time.
///
/// **Each consumer therefore has to decide which question it is asking**:
///
/// - **Giving work** ([`cpu_accepts_work`], used by [`pick_target_cpu`] and
///   [`pick_wake_cpu`]) requires *both*. A thread placed on a parked CPU never runs.
/// - **Taking work** (`steal_one`, `steal_available`) requires only `cpu_online[]`, so a
///   parked CPU's queue stays drainable; gating the victim side would strand its threads.
/// - **TLB shootdown** ([`crate::tlb`]) reads this mask alone, and treats a CPU's departure
///   from it as "will never acknowledge" rather than "not yet".
///
/// The asymmetry is deliberate. Do not add a reader that assumes the two agree, and do not
/// "make the scheduler consistent" — the tests that pin this are
/// `a_parked_cpu_takes_no_new_work_but_can_still_be_drained` and, for the shootdown half,
/// `waiting_ends_on_acknowledgement_or_departure_but_never_on_lateness`.
///
/// A bit is cleared only by [`leave_online`], when a CPU parks forever (a panic, a failed
/// AP init, or the harness verdict); there is no CPU hot-unplug. Under `cfg(test)` the set
/// is **per-thread** for test isolation, since cargo runs tests concurrently.
mod online_set;

/// The kernel must be built with `panic = "abort"` (`kernel/CLAUDE.md`): a freestanding
/// kernel has no unwinder. The target spec (`x86_64-unknown-none`) is what guarantees it,
/// not the manifest, so this assertion's trigger is a **target** change — a hand-written
/// spec omitting `panic-strategy: abort`.
#[cfg(not(test))]
const _: () = assert!(
    cfg!(panic = "abort"),
    "the kernel requires panic = \"abort\" (kernel/CLAUDE.md): a freestanding kernel has \
     no unwinder — if this fires, a target spec has dropped panic-strategy: abort"
);

/// Push onto a pre-reserved list held under `SCHED`, refusing rather than growing.
///
/// **`try_push` grows, and growth under this lock is a deadlock.** At capacity it calls
/// `kmalloc`/`kfree`, so a push under the rank-1 IRQ-acquired `SCHED` lock reaches the
/// allocator's plain spinlock, which `kernel/docs/lock-ordering.md` forbids. A
/// `debug_assert!(len < capacity)` would not prevent it: the growth happens first and
/// *succeeds*, so the assertion never fires and the inversion is silent.
///
/// Every list here is sized so this cannot happen, so reaching it is a sizing bug: this ends
/// the machine, but *without having allocated first*, and with a message naming the list.
/// **The value is `forget`ten rather than dropped**: an `ObjectRef` drop reaches
/// `dispatch_destroy` → `SlabCache::free`, the same plain spinlock under the same held lock.
#[track_caller]
fn push_reserved<T>(list: &mut KVec<T>, val: T, what: &str) {
    if let Err(returned) = list.push_within_capacity(val) {
        core::mem::forget(returned);
        panic!("{what}: reserve exhausted — this list is sized so that cannot happen");
    }
}

/// The page-table root a thread should run under: its own process root if it
/// has one, else the boot root. Caller holds the run-queue lock.
///
/// # Safety
/// `obj` is a live, pinned `Thread`.
unsafe fn resolve_root(obj: *mut ()) -> PhysAddr {
    // SAFETY: this function's contract: `obj` is a live, pinned `Thread`.
    match unsafe { Thread::addr_space_root(obj) } {
        Some(root) => root,
        None => PhysAddr::new(BOOT_ROOT.load(Ordering::Relaxed)),
    }
}

/// Scheduler state behind the rank-1 run-queue lock.
struct SchedState {
    /// **Per-CPU** ready run queues (indexed via [`ready_slot`](SchedState::ready_slot));
    /// each entry holds one refcount on its `Thread`, keeping it and its kernel stack alive
    /// while queued. A preempted thread re-homes to **its own** CPU's queue; movement
    /// happens only via placement (spawn/wake) and stealing. Pre-reserved per CPU (see
    /// [`MAX_THREADS`]), all under the one `SCHED` lock.
    ready: [KVec<ObjectRef>; MAX_CPUS],
    /// Which CPUs' scheduler state is initialized (and whose queues are reserved). The BSP
    /// sets bit 0 in [`init`]; each AP sets its own in [`ap_init`] in the same critical
    /// section that reserves its queues. Placement and stealing only target a CPU whose flag
    /// is set. A **mask**: APs come up in arbitrary order, so it is not a dense prefix.
    cpu_online: [bool; MAX_CPUS],
    /// The currently running thread, **per CPU** (indexed via
    /// [`cur_slot`](SchedState::cur_slot)). `None` only before that CPU's init.
    current: [Option<ObjectRef>; MAX_CPUS],
    /// Exited threads awaiting reclamation, **per CPU** (indexed via
    /// [`reap_slot`](SchedState::reap_slot)). Dropped — freeing their kernel stacks — by the
    /// next scheduler entry **on the same CPU**, never by the thread itself (still running
    /// on its stack at exit) and never cross-CPU: a thread parks itself here and then
    /// `switch_into`s off its stack, so freeing it from another CPU mid-switch would be a
    /// use-after-free. A **list** because several threads can end on a CPU before its next
    /// scheduler entry. Pre-reserved per CPU (see [`MAX_THREADS`]).
    reap: [KVec<ObjectRef>; MAX_CPUS],
    /// Threads suspended after a ring-3 fault, off the run queue with their `ExceptionFrame`
    /// on their kernel stacks. `sys_exception_resume` moves one back to `ready` or marks it
    /// for termination. Pre-reserved (see [`MAX_THREADS`]).
    suspended: KVec<ObjectRef>,
    /// Monotonic thread-id source.
    next_tid: u32,
    /// Monotonic process-id source. The boot parent takes pid 1; spawned children take
    /// 2, 3, …. No pid reuse.
    next_pid: u32,
    /// **Per-CPU** ticks remaining in that CPU's current slice, reset to [`QUANTUM_TICKS`]
    /// on each reschedule. Per-CPU because every CPU's tick decrements its own slice.
    quantum: [u32; MAX_CPUS],
    /// **Per-CPU** monotonic TimeShared vruntime floor (≈ that queue's smallest vruntime),
    /// advanced to the picked thread's vruntime on each dequeue. A thread placed on CPU `c`
    /// is seeded against `min_vruntime[c]` (new → the floor; waking → `floor - slice` for
    /// latency), so comparisons stay meaningful within a queue.
    min_vruntime: [u64; MAX_CPUS],
    /// The idle thread, **per CPU**, parked here whenever it is **not** that CPU's current
    /// thread; kept out of `ready`/`reap`. `None` before [`init`] or while idle is current.
    idle: [Option<ObjectRef>; MAX_CPUS],
    /// Each CPU's idle-thread object address, its stable identity (the `idle` slot is empty
    /// while idle runs). A `usize` so `SchedState` stays `Send`.
    idle_addr: [usize; MAX_CPUS],
    /// Threads blocked in `sys_wait`, off the run queue, each holding one refcount on its
    /// `Thread`. Pre-reserved (see [`MAX_THREADS`]).
    blocked: KVec<ObjectRef>,
    /// The deadline min-heap, drained on each tick. Never fuller than its promised slots.
    deadlines: KVec<deadline::Entry>,
    /// The heap slots promised, by [`deadline::DeadlineKind`]: the threads' at [`init`], each
    /// timer's and endpoint's in its [`SlotHold`]. The heap's capacity covers their sum.
    deadline_slots: [usize; 3],
    /// **Per-CPU** event counters, incremented at sites that all hold this lock.
    stats: [stats::Counters; MAX_CPUS],
    /// `ObjectRef`s parked for a **thread-context** drop by [`reap_pending`]: refs a
    /// lock-held or IRQ-context path must release but may not drop in place, because a drop
    /// can reach the plain-spinlock allocator — never legal under this IRQ-acquired lock
    /// (cross-CPU deadlock) or in IRQ context (self-deadlock against an interrupted
    /// allocator holder). Producers *move* refs in within the reserve (see
    /// [`DEFERRED_DROP_RESERVE`]); the only one is [`wake_entropy_seed_waiters`].
    deferred_drops: KVec<ObjectRef>,
    /// Processes whose last thread has exited and whose **handle table** still needs
    /// closing. Pushed by [`end_process_if_last`] under `SCHED`; drained by the
    /// [reaper thread](reaper_body) and, opportunistically, by [`reap_pending`].
    /// **CPU-agnostic, unlike [`reap`](Self::reap)**: closing a handle table is keyed on pid
    /// and touches no stack, so one reaper thread can serve the whole machine.
    ended_pids: KVec<u32>,
    /// The reaper thread while it is parked. `None` while it is running or queued, like
    /// [`idle`](Self::idle), so a wake is a `take()`.
    reaper: Option<ObjectRef>,
    /// Threads held, the idle threads aside: counted from a thread's first placement (or
    /// adoption at boot) until its reap is drained. Bounded by [`MAX_THREADS`].
    threads: usize,
}

static SCHED: IrqSpinLock<SchedState> = IrqSpinLock::new(
    LockRank::Sched,
    SchedState {
        ready: [const { KVec::new() }; MAX_CPUS],
        cpu_online: [false; MAX_CPUS],
        current: [const { None }; MAX_CPUS],
        reap: [const { KVec::new() }; MAX_CPUS],
        suspended: KVec::new(),
        next_tid: 1,
        next_pid: 2,
        quantum: [QUANTUM_TICKS; MAX_CPUS],
        min_vruntime: [0; MAX_CPUS],
        idle: [const { None }; MAX_CPUS],
        idle_addr: [0; MAX_CPUS],
        blocked: KVec::new(),
        deadlines: KVec::new(),
        deadline_slots: [0; 3],
        stats: [stats::Counters::ZERO; MAX_CPUS],
        deferred_drops: KVec::new(),
        ended_pids: KVec::new(),
        reaper: None,
        threads: 0,
    },
);

impl SchedState {
    /// Dense index of the running CPU's per-CPU scheduler slots.
    #[inline]
    fn this_cpu() -> usize {
        use crate::arch::smp::ArchSmp;
        crate::arch::Smp::current_cpu() as usize
    }

    /// This CPU's `current`-thread slot (mutable).
    #[inline]
    fn cur_slot(&mut self) -> &mut Option<ObjectRef> {
        &mut self.current[Self::this_cpu()]
    }

    /// This CPU's `current`-thread slot (shared).
    #[inline]
    fn cur_ref(&self) -> &Option<ObjectRef> {
        &self.current[Self::this_cpu()]
    }

    /// This CPU's idle-thread parking slot (mutable).
    #[inline]
    fn idle_slot(&mut self) -> &mut Option<ObjectRef> {
        &mut self.idle[Self::this_cpu()]
    }

    /// This CPU's reap list (mutable). Per-CPU so a dying thread is reclaimed only by the
    /// CPU it died on, after its `switch_into` completed.
    #[inline]
    fn reap_slot(&mut self) -> &mut KVec<ObjectRef> {
        &mut self.reap[Self::this_cpu()]
    }

    /// This CPU's ready run queue (mutable). A preempted thread re-homes here.
    #[inline]
    fn ready_slot(&mut self) -> &mut KVec<ObjectRef> {
        &mut self.ready[Self::this_cpu()]
    }

    /// This CPU's idle-thread stable address.
    #[inline]
    fn idle_addr(&self) -> usize {
        self.idle_addr[Self::this_cpu()]
    }

    /// Set this CPU's idle-thread stable address.
    #[inline]
    fn set_idle_addr(&mut self, addr: usize) {
        let cpu = Self::this_cpu();
        self.idle_addr[cpu] = addr;
    }
}

/// Allocate the next process id (monotonic, no reuse). Takes only the rank-1 lock briefly;
/// `sys_process_spawn` calls this before touching the rank-3 handle table.
pub fn alloc_pid() -> u32 {
    let mut g = SCHED.lock();
    let pid = g.next_pid;
    g.next_pid = g.next_pid.wrapping_add(1);
    pid
}

/// Adopt a freshly created `Thread` into an [`ObjectRef`] without a refcount change.
fn into_objref(t: KBox<Thread>) -> ObjectRef {
    let ptr = KBox::into_raw(t).as_ptr() as *mut ();
    // SAFETY: `KBox::into_raw` yielded the single creation reference (the
    // header starts at refcount 1); `from_raw` adopts it without bumping.
    unsafe { ObjectRef::from_raw(ptr, KObjectType::Thread) }
}

/// Cooperatively yield to the next ready thread, round-robin. Returns immediately (still
/// current) if no other thread is ready — it does **not** yield to the idle thread, so the
/// boot drainer's [`ready_is_empty`] gate still works.
pub fn yield_now() {
    // Reclaim any previously-exited thread's stack first (outside the lock).
    reap_pending();

    let g = SCHED.lock();
    if g.ready[SchedState::this_cpu()].is_empty() {
        return; // nothing else ready on this CPU — keep running (guard drops, IF restored)
    }
    switch_to_next(g);
}

/// Periodic timer-tick entry, called from the timer IRQ dispatcher with interrupts masked.
/// On quantum expiry it reschedules round-robin via [`switch_to_next`] if another thread is
/// ready.
pub fn on_timer_tick() {
    let mut g = SCHED.lock();
    // Fire elapsed deadlines FIRST, so a just-woken thread is in `ready` and the
    // reschedule below can pick it: direct wakeup, no DPC (see the module docs).
    let now = crate::arch::Timer::read_ns();
    fire_expired_deadlines(&mut g, now);
    wake_entropy_seed_waiters(&mut g);
    // Charge this CPU's running TimeShared thread a nice-weighted tick of vruntime.
    accrue_vruntime(&mut g);
    let me = SchedState::this_cpu();
    g.stats[me].ticks += 1;
    let ready_here = !g.ready[me].is_empty();
    let (new_quantum, reschedule) = tick_quantum(g.quantum[me], ready_here);
    g.quantum[me] = new_quantum;
    // A preemption-critical window (the running thread may hold a plain lock other CPUs
    // spin on — see [`preempt_disable`]): do not deschedule it. The bookkeeping above
    // still ran; latch the skipped switch for `preempt_enable` / the next tick.
    if with_preempt_off(me, |c| c.load(Ordering::Relaxed)) > 0 {
        if reschedule || (!ready_here && current_is_idle(&g) && steal_available(&g, me)) {
            with_resched_pending(me, |c| c.store(true, Ordering::Release));
        }
        return; // guard drops — IF stays 0 (IRQ context) until iretq
    }
    if reschedule {
        switch_to_next(g); // consumes the guard; switches with IF masked
        return;
    }
    // Idle-CPU work stealing: if this CPU runs its idle thread while a busier CPU holds a
    // thread we may run, switch — `pick_next` (empty local queue) steals it. Gated on
    // `current_is_idle` so a busy thread isn't preempted to steal.
    if !ready_here && current_is_idle(&g) && steal_available(&g, me) {
        switch_to_next(g);
    }
    // else: guard drops here — IF was already 0 (IRQ context), stays 0 until iretq.
}

/// Resume an idle CPU into the scheduler if a thread has just been made runnable on it —
/// the shared **same-CPU-wake liveness** primitive. Caller passes the held guard. A
/// preemption-critical window (e.g. the idle thread mid-reap holding the shootdown or
/// allocator locks) latches the wake for `preempt_enable` instead of descheduling;
/// otherwise it switches **only if this CPU is idle** and there is work (own queue or
/// stealable). A running `TimeShared` thread is left to its quantum. Consumes the guard.
fn resched_if_idle_locked(g: IrqSpinLockGuard<'_, SchedState>) {
    let me = SchedState::this_cpu();
    if with_preempt_off(me, |c| c.load(Ordering::Relaxed)) > 0 {
        with_resched_pending(me, |c| c.store(true, Ordering::Release));
        return; // guard drops
    }
    if current_is_idle(&g) && (!g.ready[me].is_empty() || steal_available(&g, me)) {
        switch_to_next(g); // consumes the guard; switches with IF masked
    }
    // else: guard drops here — IF stays as the caller left it until iretq.
}

/// Handle a reschedule IPI: another CPU made a thread runnable on this CPU (a cross-CPU
/// wake or a placement onto this idle CPU) and poked us to run it now rather than at the
/// next tick. Switches only if this CPU is idle and has work; a busy thread keeps running
/// until its quantum tick. Called from the arch dispatcher, which already EOI'd (as the
/// timer path does, since the switch may not return promptly). Runs with IF=0.
pub fn on_reschedule_ipi() {
    let mut g = SCHED.lock();
    g.stats[SchedState::this_cpu()].resched_ipis += 1;
    resched_if_idle_locked(g);
}

/// Resume an idle CPU into the scheduler after a **same-CPU wake** that sent no reschedule
/// IPI — the device-IRQ counterpart of [`on_reschedule_ipi`], called from the device-IRQ
/// dispatch tail. An I/O-completion DPC (the IRP's `on_complete` →
/// `complete_pending_op`) can make a thread runnable on *this* CPU, and `place_thread`
/// sends an IPI only for *cross-CPU* placements, so without this an idle CPU `iretq`s
/// straight back into `hlt` and the woken thread waits for the next tick, serialising every
/// block I/O on the tick clock on a 1-vCPU guest (`docs/decision-log.md`, 2026-07-23). Same
/// discipline as the IPI handler: honors the preempt latch, only ever preempts the idle
/// thread. The device stub is a frame-parking stub, so a delayed return resumes into its
/// epilogue and `iretq`s exactly as the timer/resched stubs do.
pub fn resched_if_idle() {
    resched_if_idle_locked(SCHED.lock());
}

/// Capture a consistent point-in-time copy of every CPU's scheduler statistics under one
/// `SCHED` hold (see [`stats`]). Copies plain `Copy` data only; the caller runs
/// [`stats::format`] (which allocates) with the lock released.
pub fn stats_snapshot() -> stats::Snapshot {
    let g = SCHED.lock();
    let mut cpus = [stats::CpuSnapshot::OFFLINE; MAX_CPUS];
    for (c, slot) in cpus.iter_mut().enumerate() {
        *slot = stats::CpuSnapshot {
            online: g.cpu_online[c],
            idle: g.current[c].as_ref().is_some_and(|t| t.as_ptr() as usize == g.idle_addr[c]),
            ready: g.ready[c].len() as u32,
            counters: g.stats[c],
        };
    }
    stats::Snapshot {
        cpus,
        reaper_closed: REAPER_CLOSED.load(Ordering::Relaxed),
        exit_closed: EXIT_CLOSED.load(Ordering::Relaxed),
    }
}

/// Point the ring-0 trap stack (`TSS.RSP0`) and the per-CPU syscall stack at `obj`'s kernel
/// stack, so a ring-3 → ring-0 transition from this thread lands on **its** kernel stack,
/// not a sibling's. Called on every switch-in; a no-op for stackless boot/idle threads that
/// never trap from ring 3. Caller holds `SCHED`; the writes are arch register/per-CPU
/// stores, no locks.
unsafe fn arm_kernel_stack_for(obj: *mut ()) {
    // SAFETY: `obj` is the pinned incoming thread; `kstack_top` reads its field
    // under `SCHED`. Setting TSS.RSP0 / the syscall stack are arch stores.
    if let Some(ktop) = unsafe { Thread::kstack_top(obj) } {
        Cpu::set_kernel_stack(ktop);
        crate::arch::set_syscall_kernel_stack(ktop);
        // Re-assert this CPU's `KERNEL_GS_BASE`. A user thread that **migrated** here must
        // `swapgs` into *this* CPU's block on its next syscall (so the entry stub reads this
        // CPU's freshly-armed `kstack_top`); otherwise it can syscall onto a stale block →
        // `#DF` in `syscall_entry`.
        crate::arch::arm_user_entry_cpu_base();
    }
}

/// The common context-switch tail shared by every voluntary/involuntary switch:
/// [`block_current_and_switch`], [`switch_to_next`], [`finish_exit`], and
/// [`suspend_with_fault`]. The caller must have already re-homed the outgoing thread (onto
/// `ready` / `blocked` / `suspended` / `reap`), set the incoming thread's state to
/// `Running`, and installed it as `current`, all under the held `SCHED` guard `g`. This
/// consumes `g`.
///
/// `out_slot` is the outgoing thread's saved-SP slot (where the switch stashes its kernel
/// RSP); `next_obj` is the pinned incoming thread just made current.
///
/// It drops the lock keeping interrupts masked (see the module docs and
/// [`release_keeping_irqs_masked`](IrqSpinLockGuard::release_keeping_irqs_masked)),
/// re-arms the incoming thread's trap/syscall kernel stack, loads its page-table root
/// **before** switching away (so a dying thread's CR3 is off its soon-to-be-freed root),
/// and performs the stack switch. On a resuming caller, control returns here and its own
/// captured interrupt state is restored; a terminal caller ([`finish_exit`]) never
/// switches back.
///
/// Factoring this once means the four parking dispositions cannot drift apart — a
/// divergence (e.g. forgetting `arm_kernel_stack_for` or the CR3 load on one path) would be
/// a latent `#DF`/corruption bug.
///
/// # Safety
/// `out_slot` must be the saved-SP slot of the (pinned) outgoing thread and
/// `next_obj` the pinned incoming thread the caller just made `current`, both
/// under the `SCHED` hold being released here. Single-CPU: nothing else
/// touches either thread's saved SP across the switch.
unsafe fn switch_into(
    mut g: IrqSpinLockGuard<'_, SchedState>,
    out_slot: *mut u64,
    out_obj: *mut (),
    next_obj: *mut (),
) {
    g.stats[SchedState::this_cpu()].switches += 1;
    // Sample the **outgoing** thread's kernel-stack high-water mark. Every thread that runs
    // passes through here, including servers that spend their lives blocked and so never
    // sit in a run queue, which is why this is the sample point. Instrumentation only.
    #[cfg(feature = "test-harness")]
    // SAFETY: `out_obj` is the pinned outgoing thread and `SCHED` is held, satisfying the
    // `Thread` accessor contract; `kstack_top` yields the top of its live painted stack
    // (`None` for a thread with no kernel stack of its own).
    if let Some(top) = unsafe { Thread::kstack_top(out_obj) } {
        // SAFETY: `out_obj` is pinned under `SCHED` (accessor contract), and `top` is that
        // live stack's exclusive top, as `sample` requires.
        unsafe {
            let pid = Thread::owner_pid_of(out_obj);
            crate::mm::kstack::watermark::sample(top.get(), pid);
        }
    }
    // Wait out the incoming thread's mid-switch-out guard before touching its parked
    // context. A waker can re-home a thread to a **third** CPU's queue (an
    // affinity-diverted wake/resume) in the window between its old CPU releasing `SCHED` and
    // `context_switch` committing `saved_sp` — and `dequeue_front`, unlike `stealable_to`,
    // does not filter on the guard, so that CPU could otherwise resume a not-yet-committed
    // context (a double-run). Bounded and deadlock-free even with `SCHED` held: the owning
    // CPU is in straight-line post-release code and clears the guard without any lock.
    // SAFETY: `next_obj` is pinned (now `current`).
    while unsafe { Thread::is_on_cpu(next_obj) } {
        core::hint::spin_loop();
    }
    // SAFETY: `next_obj` is pinned (now `current`) and `SCHED` is still held
    // here, satisfying the Thread accessor contract for these reads.
    let next_sp = unsafe { Thread::saved_sp(next_obj) };
    // SAFETY: as above.
    let next_root = unsafe { resolve_root(next_obj) };
    // Record the CPU this thread is about to run on, so a later wake re-homes it here
    // (cache-warm, minimal migration). SAFETY: `next_obj` pinned (now current); `SCHED`
    // still held.
    unsafe { Thread::set_last_cpu(next_obj, SchedState::this_cpu() as u8) };
    // SAFETY: as above.
    let ending = unsafe { Thread::is_ending(next_obj) };
    with_ending(SchedState::this_cpu(), |e| e.store(ending, Ordering::Release));
    // Raise the outgoing thread's mid-switch-out guard **while still holding `SCHED`** (so
    // it is set before the lock release makes any `ready` entry for `out_obj` visible to a
    // stealer). `context_switch` clears it after it commits `out_obj`'s `saved_sp`, at which
    // point the parked context is valid to resume elsewhere. Prevents a cross-CPU steal from
    // loading a not-yet-written `saved_sp` and double-running the thread (see
    // `Thread::on_cpu` and `steal_one`).
    // SAFETY: `out_obj` is the pinned outgoing thread; `SCHED` still held.
    unsafe { Thread::set_on_cpu(out_obj, true) };
    // SAFETY: as above.
    let out_on_cpu = unsafe { Thread::on_cpu_ptr(out_obj) };
    // Drop the lock but keep interrupts masked across the switch; `saved_if` is restored
    // when this thread next resumes.
    let saved_if = g.release_keeping_irqs_masked();
    // Every context switch passes through here, so this is the one place that can check the
    // lock tracker's per-CPU model still holds: a thread may only be switched away with
    // nothing held and no interrupt scope open. True by construction (a plain-lock holder
    // has preemption disabled, an `IrqSpinLock` holder has interrupts masked, and blocking
    // with a lock held is forbidden); this checks it. Debug builds only.
    crate::libkern::lockrank::assert_switch_safe();
    // The other per-CPU invariant: the involuntary paths (`on_timer_tick`,
    // `resched_if_idle_locked`) latch into `RESCHED_PENDING` rather than switching; this
    // checks the **voluntary** ones too. The reasoning is on the function.
    assert_switch_preempt_safe();
    // SAFETY: `next_obj` is the pinned incoming thread (re-arm its trap/syscall stack).
    unsafe { arm_kernel_stack_for(next_obj) };
    // SAFETY: `next_root` is a fully-formed PML4 (boot root, or a process root
    // with the kernel half inherited); all kernel stacks are mapped in every
    // root, so switching CR3 before the stack swap is sound. Loading it before
    // the switch also ensures a dying thread leaves CR3 on the incoming root
    // before it is reaped (its `AddressSpace::Drop` may free the old PML4).
    unsafe { Paging::set_page_table(next_root) };
    // Swap the floating-point / SIMD register file. Eager, not lazy: leaving the outgoing
    // thread's registers live behind a `CR0.TS` trap is CVE-2018-3665 (see
    // `arch/../fpu.rs`).
    //
    // Placement is load-bearing on both ends. The **save** must precede `context_switch`,
    // because that clears `out_obj`'s `on_cpu` guard — the FP image is part of the parked
    // context, so it must be complete before another CPU may resume the thread. The
    // **restore** must follow the `is_on_cpu(next_obj)` spin above, for the mirror-image
    // reason: until that guard clears, the incoming image may still be mid-write on its old
    // CPU. Restoring before the stack swap is correct because the FP register file is CPU
    // state, not stack state. Neither call allocates, so both are legal in this lock-free,
    // IRQ-masked window.
    //
    // SAFETY: both areas are live, 64-byte-aligned images belonging to pinned
    // threads, and every CPU ran `fpu_init_cpu` during bring-up. A null area
    // means an inert thread that is never scheduled — skip it.
    unsafe {
        let out_fpu = Thread::fpu_area_ptr(out_obj);
        if !out_fpu.is_null() {
            crate::arch::fpu_save(out_fpu);
        }
        let next_fpu = Thread::fpu_area_ptr(next_obj);
        if !next_fpu.is_null() {
            crate::arch::fpu_restore(next_fpu);
        }
    }
    // SAFETY: lock released; interrupts masked; `out_slot` points into the
    // outgoing thread (pinned) and `next_sp` is the saved RSP of the now-current
    // thread (pinned). `out_on_cpu` is the outgoing thread's guard byte, cleared
    // by the switch once `out_slot` is committed.
    unsafe { context_switch(out_slot, next_sp, out_on_cpu) };
    // Resumed (cooperative path): restore the interrupt state this thread had on entry. On
    // the preemptive path the resume returns into the timer-stub epilogue (which `iretq`s IF
    // back) and `saved_if` is false, so this is a redundant `cli`. A terminal caller
    // (`finish_exit`) never reaches this.
    // SAFETY: ring-0; restoring this thread's own captured interrupt state.
    unsafe { Cpu::interrupts_restore(saved_if) };
}

/// Decide the quantum update and whether to reschedule on a timer tick. Pure (no lock, no
/// switch) so it is host-testable.
///
/// Decrement the quantum; on expiry reset it to [`QUANTUM_TICKS`] and reschedule **iff
/// another thread is ready** — if nothing else is runnable the current thread (worker or
/// idle) keeps running, since switching to idle (or idle→idle) would be pointless churn.
fn tick_quantum(quantum: u32, ready_nonempty: bool) -> (u32, bool) {
    let q = quantum.saturating_sub(1);
    if q == 0 { (QUANTUM_TICKS, ready_nonempty) } else { (q, false) }
}

/// The shared switch core used by [`yield_now`] and [`on_timer_tick`]: the outgoing
/// `current` is re-homed (re-enqueued, or re-parked if it is the idle thread) and the next
/// thread (front of `ready`, else the idle thread) becomes current. Consumes the guard.
///
/// Callers ensure there is something else to run. Reusing
/// [`context_switch`](crate::arch::context_switch) is sound from the preemptive path too:
/// the interrupted thread's full register frame is already on its kernel stack (pushed by
/// the timer stub) *below* the callee-saved frame this switch parks, so on resume it
/// returns up into the timer-stub epilogue, which `iretq`s the original context back.
fn switch_to_next(mut g: IrqSpinLockGuard<'_, SchedState>) {
    let next = match pick_next(&mut g) {
        Some(n) => n,
        // Only reachable if a caller violates the precondition; idle is the safe fallback
        // when it is parked.
        None => g.idle_slot().take().expect("a runnable thread (ready or idle)"),
    };
    let prev = g.cur_slot().take().expect("current set after init");
    let prev_obj = prev.as_ptr();
    let next_obj = next.as_ptr();
    // SAFETY: both pinned alive (prev re-homed, next becoming current) and we
    // hold the run-queue lock — the Thread accessor contract.
    unsafe {
        Thread::set_state(prev_obj, ThreadState::Ready);
        Thread::set_state(next_obj, ThreadState::Running);
    }
    // SAFETY: as above.
    let prev_slot = unsafe { Thread::saved_sp_mut_ptr(prev_obj) };

    // Re-home prev: the idle thread parks in its slot (never in `ready`); every other
    // thread re-enqueues on **this CPU's** queue (movement happens only via
    // placement/stealing).
    if g.idle_addr() == prev_obj as usize {
        debug_assert!(g.idle_slot().is_none());
        *g.idle_slot() = Some(prev);
    } else {
        push_reserved(g.ready_slot(), prev, "run queue (switch-out)");
    }
    *g.cur_slot() = Some(next);

    // SAFETY: `prev_slot` is the re-homed outgoing (now-`Ready`, pinned)
    // thread's saved-SP slot; `prev_obj`/`next_obj` are the pinned outgoing /
    // incoming threads.
    unsafe { switch_into(g, prev_slot, prev_obj, next_obj) };
}

/// `true` when no thread other than the current one is ready to run.
pub fn ready_is_empty() -> bool {
    // Globally empty: no CPU has a runnable thread queued. Used by the boot drainers.
    SCHED.lock().ready.iter().all(|q| q.is_empty())
}

/// Rust entry reached from [`thread_trampoline`](crate::arch::thread_trampoline) the first
/// time a thread is scheduled. Runs the thread body, then exits cleanly if it returns.
/// Never returns.
///
/// Reached with the run-queue lock **not** held (the switching thread released it before
/// [`context_switch`]).
pub(crate) extern "C" fn thread_enter() -> ! {
    // Capture (under the lock) whether the current thread descends to ring 3 and its params.
    let descent: Option<(u64, u64, [u64; 4])> = {
        let g = SCHED.lock();
        let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
        let obj = cur.as_ptr();
        // SAFETY: `current` is pinned alive and we hold the lock.
        match unsafe { Thread::user_entry(obj) } {
            // A thread whose process began exiting before it first ran never reaches ring 3.
            // SAFETY: as above.
            Some(_) if unsafe { Thread::is_ending(obj) } => {
                drop(g);
                exit_thread(ExitStatus { kind: ExitKind::Normal as u32, code: 0 });
            }
            Some((entry, user_sp)) => {
                // SAFETY: as above.
                assert!(unsafe { Thread::kstack_top(obj) }.is_some(), "a user thread has a kernel stack");
                // Armed under `SCHED`, where reading the CPU and writing its slots cannot straddle a
                // migration; a switch-in re-arms them wherever the thread resumes.
                // SAFETY: as above.
                unsafe { arm_kernel_stack_for(obj) };
                // SAFETY: as above.
                let boot = unsafe { Thread::user_boot_args(obj) };
                Some((entry, user_sp, boot))
            }
            None => None,
        }
    };

    match descent {
        Some((entry, user_sp, boot)) => {
            // SAFETY: `entry`/`user_sp` are canonical user VAs mapped X / W
            // in the active address space; the syscall fast-path is armed.
            // `boot` seeds rdi/rsi/rdx/rcx — the spawn hand-off (notif, root
            // namespace, installed endpoint, arg0).
            unsafe { crate::arch::enter_user(entry, user_sp, boot[0], boot[1], boot[2], boot[3]) }
        }
        None => {
            // Kernel thread: run the body in ring 0, then exit cleanly. No owning process,
            // so no `ChildExited`.
            let (entry, arg) = current_entry();
            entry(arg);
            exit_thread(ExitStatus { kind: ExitKind::Normal as u32, code: 0 });
        }
    }
}

#[cfg(test)]
mod tests;
