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

// `Timer` above is the kernel object (`crate::object::Timer`); the hardware
// monotonic clock is reached via the full path `crate::arch::Timer::read_ns()`
// (the `ArchTimer` trait, imported above, provides `read_ns`). The two names
// live in different paths — see `arch/timer.rs`.

/// Per-CPU run-queue capacity, reserved once at [`init`]/[`ap_init`].
/// Enqueueing beyond this is refused (spawn) or fatal (wake, after every
/// permitted CPU's queue is full — see [`place_thread`]) rather than an
/// allocation under the rank-1 lock. Raised 16 → 32 for Phase 4 headroom
/// (multi-threaded processes + affinity pinning concentrate runnable threads;
/// review finding F6): 32 refs = 256 bytes per CPU.
const READY_RESERVE: usize = 32;

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
///
/// **Global, not per-CPU** — unlike [`READY_RESERVE`] — so this is the number of
/// threads the *whole system* may have parked in `sys_wait` at once. Every
/// resource server spends its life there, so it scales with the process count
/// and not with anything else.
///
/// Raised 16 → 64 on 2026-08-13, when M5 Part C added one process (`nxterm`
/// spawning the shell it hosts) and the boot panicked on
/// `blocked list within reserve` (the message reads `blocked list: reserve
/// exhausted — …` since 2026-08-14). Sixteen had been one process away from its
/// limit for some time; nothing said so, because the failure mode is a panic at
/// whatever moment the high-water mark is crossed rather than a warning as it is
/// approached.
const BLOCKED_RESERVE: usize = 64;

/// Pre-reserved capacity for the exited-thread reap list. Holds the current thread plus any
/// sibling threads a process exit tears down.
///
/// **Raised 16 → [`READY_RESERVE`] on 2026-08-14, because the comment here already said it
/// was that.** It claimed to be "sized like the run queue"; the run queue went 16 → 32 for
/// Phase 4 (F6) and this did not follow. Nothing noticed while an over-full push merely
/// *grew* — badly, allocating under `SCHED` (F11), but silently. Since pushes now refuse, the
/// number is load-bearing: `exit_process` sweeps every sibling of the exiting process into
/// this list in one lock hold, so it is the cap on threads-per-process at exit
/// (PR #205 review, finding 2).
///
/// **That cap is userspace-reachable and aborts the machine when crossed**, because
/// `sys_thread_create` has no per-process limit: 33 threads that block and then exit will
/// trip it. Refusing is still right — growing here is the forbidden pattern, and the
/// alternative to the panic is not "grow" but "reap in batches", which is a real change to
/// the exit path. Tracked in `docs/rationale/deferred-decisions.md`; until then this number
/// is the bound and it is stated rather than implied.
const REAP_RESERVE: usize = READY_RESERVE;

/// Pre-reserved capacity for [`SchedState::deferred_drops`] — `ObjectRef`s a
/// lock-held or IRQ-context path must release but cannot drop in place (a drop
/// can reach the plain-spinlock allocator, forbidden under `SCHED` and in IRQ
/// context — decision log 2026-07-21, F2/F11). Sized to twice the only current
/// producer's burst (the entropy seed wake parks ≤ [`crate::entropy::SEED_WAITERS_MAX`]
/// refs, once per boot); [`reap_pending`] drains it in thread context.
const DEFERRED_DROP_RESERVE: usize = 2 * crate::entropy::SEED_WAITERS_MAX;
/// Handle tables the [reaper](reaper_body) has closed, reported as `reaper_closed=` in
/// `/proc/sched/stats`.
///
/// Exposed because the failure mode is **silence**: a reaper that never woke would leave
/// the pre-existing per-CPU `process_ended` sweep to cover for it, and everything would
/// look fine until a system busy enough to starve that sweep hung a shell. A counter the
/// guest can read turns "the reaper is alive and being scheduled" into something a test
/// can assert.
pub static REAPER_CLOSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Handles closed by a process **in its own exit syscall**, reported as `exit_closed=` in
/// `/proc/sched/stats`.
///
/// The counterpart to [`REAPER_CLOSED`], and exposed for the same reason: exit-context
/// teardown is an optimisation over a mechanism that already works, so if it silently
/// stopped running the reaper would cover for it and nothing would look wrong — except
/// that every peer's `PeerClosed` would quietly go back to arriving late.
pub static EXIT_CLOSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The deadline min-heap: armed-timer and `sys_wait`-deadline expiries keyed by
/// absolute monotonic ns, drained on each periodic tick. A pure binary heap in
/// a [`KVec`], living in [`SchedState`] under `SCHED`; host-tested.
mod deadline;

/// Scheduler statistics: the per-CPU event counters behind `/proc/sched/stats`,
/// their point-in-time snapshot, and the pure text formatter.
///
/// The surface follows the **capture → format → synthesize** discipline for
/// kernel-state snapshots (see `docs/architecture/scheduler.md` § "The stats
/// surface"): [`stats_snapshot`](super::stats_snapshot) copies plain `Copy`
/// data under one `SCHED` hold, [`format`] renders it to text with no lock
/// held (allocation never happens under the rank-1 lock), and the
/// `/proc/sched/stats` kernel server wraps the bytes in a read-only
/// `MemoryObject`.
///
/// The counters live in [`SchedState`](super::SchedState) as plain `u64`s —
/// no atomics — because every increment site already holds `SCHED`.
pub mod stats;

/// The kernel/boot page-table root, captured once at [`init`]. Threads with
/// no per-process address space (`addr_space_root == None`) run on this root;
/// the scheduler loads it on switch-in so a dying user thread's address space
/// can be freed safely (CR3 is the boot root before the reap).
static BOOT_ROOT: AtomicU64 = AtomicU64::new(0);

/// Lock-free bitmask of online CPUs (dense index → bit), kept so subsystems that must not
/// take the top-rank `SCHED` lock can read the online set without a lock — notably TLB
/// shootdown ([`crate::tlb`]), which fires from the mm layer under lower locks. Each CPU
/// sets its own bit as it comes online.
///
/// **It mirrors the `SCHED`-guarded `cpu_online[]` on the way up but not on the way down**,
/// because [`leave_online`] clears a bit here and does not touch `cpu_online[]` — that needs
/// `SCHED`, which the parking CPU may already hold. So the two disagree after a park, and
/// that divergence is reachable on a machine that otherwise keeps running: `idt.rs`'s
/// `dump_and_halt` parks on **any ring-0 fault**, a kernel `#PF` on an AP say, and that CPU
/// is fully online at the time. (The AP-init park is clean by contrast: `ap_init` sets
/// `cpu_online[]` only after its last fallible step.)
///
/// **Each consumer therefore has to decide which question it is asking**, and they do not
/// all give the same answer:
///
/// - **Giving work** ([`cpu_accepts_work`], used by [`pick_target_cpu`] and
///   [`pick_wake_cpu`]) requires *both*. A thread placed on a parked CPU is not delayed, it
///   never runs.
/// - **Taking work** (`steal_one`, `steal_available`) requires only `cpu_online[]`, so a
///   parked CPU's queue stays drainable. That is how the threads it was already holding get
///   rescued; gating the victim side would strand exactly them.
/// - **TLB shootdown** ([`crate::tlb`]) reads this mask alone, and treats a CPU's departure
///   from it as "will never acknowledge" rather than "not yet".
///
/// The asymmetry is deliberate in each direction. Do not add a reader that assumes the two
/// agree, and do not "make the scheduler consistent" — the tests that pin this are
/// `a_parked_cpu_takes_no_new_work_but_can_still_be_drained` and, for the shootdown half,
/// `waiting_ends_on_acknowledgement_or_departure_but_never_on_lateness`.
///
/// **A bit is cleared only by [`leave_online`], when a CPU parks forever.** There is
/// still no CPU hot-unplug; this is the terminal case — a panic, a failed AP init, or
/// the harness verdict — where the CPU stops executing entirely.
///
/// **Two backings behind one interface.** On the machine this is a process-global atomic,
/// as it must be. Under `cfg(test)` it is **per-thread** — and unlike the mock `IF` flag in
/// [`crate::libkern::spinlock`], that is *not* a fidelity argument: the online set is
/// machine-global by definition, and a host thread is not a machine. It is test isolation,
/// which is reason enough on its own. Cargo runs tests concurrently by default, so one set
/// is the D.2 hazard that already produced the 22 % failure rate across `sched::tests`
/// recorded at [`cpu_accepts_work`], when the placement tests read this global. Production
/// codegen is unchanged — the `cfg(not(test))` arm is the original atomic, verbatim.
mod online_set;

/// The kernel must be built with `panic = "abort"`.
///
/// **Not for the reason an earlier version of this comment gave.** It claimed the wake paths
/// hold an `ObjectRef` temporary across their `panic!`, so unwinding would free it under
/// `SCHED`. That is false — an `if` condition is its own temporary scope, so the ref dropped
/// *before* the panic on every profile — and the real defect it was papering over is fixed at
/// those two sites instead (PR #204 review, blocking 1).
///
/// The rule it does enforce is the documented one: `kernel/CLAUDE.md`, "`panic = "abort"` —
/// no stack unwinding in the kernel." A freestanding kernel has no unwinder to run.
///
/// **What actually guarantees it today is the target, not the manifest.**
/// `x86_64-unknown-none` carries `panic="abort"` in its spec
/// (`rustc --target x86_64-unknown-none --print cfg`), so rustc forces abort whatever a
/// profile says — setting `panic = "unwind"` in `kernel/Cargo.toml` changes nothing and the
/// kernel still builds. The manifest lines are belt and braces, and this assertion's trigger
/// is therefore a **target** change, which `kernel/CLAUDE.md` contemplates ("if we ever need
/// a feature the built-in spec doesn't expose, we switch to a custom JSON"): a hand-written
/// spec omitting `panic-strategy: abort` is what it is here to stop.
///
#[cfg(not(test))]
const _: () = assert!(
    cfg!(panic = "abort"),
    "the kernel requires panic = \"abort\" (kernel/CLAUDE.md): a freestanding kernel has \
     no unwinder — if this fires, a target spec has dropped panic-strategy: abort"
);

/// Push onto a pre-reserved list held under `SCHED`, refusing rather than growing.
///
/// **`try_push` grows, and growth under this lock is the F11 deadlock.** `KVec::try_push`
/// calls `kmalloc`/`kfree` when it is at capacity, so at the reserve boundary a push under
/// the rank-1 IRQ-acquired `SCHED` lock reaches the allocator's plain spinlock — which
/// `kernel/docs/lock-ordering.md` forbids without qualification. The `debug_assert!(len <
/// capacity)` that sat beside several of these did not prevent it: the growth happens first
/// and *succeeds*, so the assertion never fires and the inversion is silent. The `.expect()`
/// fires only if the allocation itself fails (kernel audit C.1(c), 2026-08-14).
///
/// Every list here is sized so this cannot happen, and reaching it is a bug in that sizing —
/// so this still ends the machine, exactly as the `.expect()` it replaces did. What changes
/// is that it ends it *without having allocated first*, and with a message naming the list.
///
/// **The value is `forget`ten rather than dropped**, for the reason the wake paths now
/// `forget` theirs: an `ObjectRef` drop reaches `dispatch_destroy` → `SlabCache::free`, the
/// same plain spinlock under the same held lock. Nothing outlives the abort.
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
    /// **Per-CPU** ready run queues (indexed by `current_cpu()` via
    /// [`ready_slot`](SchedState::ready_slot)); each entry holds one refcount on its
    /// `Thread`, keeping it (and its kernel stack) alive while queued. A preempted
    /// thread re-homes to **its own** CPU's queue, so threads do not freely migrate;
    /// movement happens only via explicit placement (spawn/wake) and work stealing.
    /// Pre-reserved per CPU (see [`READY_RESERVE`]). The whole array is still guarded
    /// by the one `SCHED` lock — per-CPU *queues*, not per-CPU *locks* (slice 3).
    ready: [KVec<ObjectRef>; MAX_CPUS],
    /// Which CPUs' scheduler state is initialized (and whose `ready`/`reap` queues are
    /// reserved). The BSP sets bit 0 in [`init`]; each AP sets its own bit in [`ap_init`]
    /// (in the same critical section that reserves its queues). Placement and work
    /// stealing only ever target a CPU whose flag is set. A **mask** (not a count) —
    /// APs run `ap_init` in arbitrary order, so the online set is not a dense prefix.
    cpu_online: [bool; MAX_CPUS],
    /// The currently running thread, **per CPU** (indexed by `current_cpu()` via
    /// [`cur_slot`](SchedState::cur_slot)). A CPU's slot is `None` only before
    /// that CPU's init. Each CPU pulls from its own [`ready`] queue.
    current: [Option<ObjectRef>; MAX_CPUS],
    /// Exited threads awaiting reclamation, **per CPU** (indexed by `current_cpu()`
    /// via [`reap_slot`](SchedState::reap_slot)). Dropped — freeing their kernel
    /// stacks — by the next scheduler entry **on the same CPU**, never by a thread
    /// itself (it is still running on its stack at exit time) and never cross-CPU:
    /// a thread parks itself here and then `switch_into`s off its stack, so freeing
    /// it from another CPU mid-switch would be a use-after-free. Same-CPU reaping
    /// guarantees the switch has completed (the thread is off-CPU) first. A **list**
    /// (not one slot) so a process exit can reap its torn-down siblings alongside
    /// the caller. Pre-reserved per CPU (see [`REAP_RESERVE`]).
    reap: [KVec<ObjectRef>; MAX_CPUS],
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
    /// **Per-CPU** ticks remaining in that CPU's current slice; reset to
    /// [`QUANTUM_TICKS`] on each reschedule. Scheduler **policy**, so it lives
    /// here rather than on `Thread` (no `Thread` layout/ABI change). Per-CPU
    /// because every CPU's tick decrements its own slice — a single shared
    /// counter was only benign while `QUANTUM_TICKS == 1` (each tick reset it);
    /// any longer quantum would have had CPUs consuming each other's slices
    /// (review finding F7).
    quantum: [u32; MAX_CPUS],
    /// **Per-CPU** monotonic TimeShared vruntime floor (≈ that CPU's run queue's
    /// smallest vruntime), advanced to the picked thread's vruntime on each dequeue.
    /// A thread placed on CPU `c` is seeded against `min_vruntime[c]` (new → the
    /// floor; waking → `floor - slice` for latency), so vruntime comparisons stay
    /// meaningful within a queue even though each CPU advances its floor independently.
    min_vruntime: [u64; MAX_CPUS],
    /// The idle thread, **per CPU** (indexed by `current_cpu()` via
    /// [`idle_slot`](SchedState::idle_slot)), parked here whenever it is **not**
    /// this CPU's current thread. Kept out of `ready`/`reap`; runs (`hlt`) only
    /// when nothing else is ready on that CPU. `None` before [`init`] or while
    /// idle is current.
    idle: [Option<ObjectRef>; MAX_CPUS],
    /// Each CPU's idle-thread object address — its stable identity (the `idle`
    /// slot is empty while idle runs). Stored as `usize` (not a raw pointer) so
    /// `SchedState` stays `Send`. `0` before [`init`].
    idle_addr: [usize; MAX_CPUS],
    /// Threads blocked in `sys_wait`, parked off the run queue. Each holds one
    /// refcount on its `Thread` (keeping it and its kernel stack alive); a
    /// waker moves it back to `ready`. Pre-reserved (see [`BLOCKED_RESERVE`]).
    blocked: KVec<ObjectRef>,
    /// The deadline min-heap (armed timers + `sys_wait` deadlines), drained on
    /// each periodic tick. Pre-reserved (see [`deadline::HEAP_RESERVE`]).
    deadlines: KVec<deadline::Entry>,
    /// **Per-CPU** scheduler event counters (see [`stats::Counters`]),
    /// incremented at their event sites — all of which already hold this lock —
    /// and copied out by [`stats_snapshot`] for the `/proc/sched/stats` surface.
    stats: [stats::Counters; MAX_CPUS],
    /// `ObjectRef`s parked for a **thread-context** drop by [`reap_pending`]:
    /// refs a lock-held or IRQ-context path must release but may not drop in
    /// place, because an `ObjectRef` drop can reach the plain-spinlock allocator
    /// — which must never run under this IRQ-acquired lock (cross-CPU deadlock)
    /// nor in IRQ context (same-CPU self-deadlock against an interrupted
    /// allocator holder). Producers *move* refs in within the reserve (see
    /// [`DEFERRED_DROP_RESERVE`]); the only producer today is the entropy seed
    /// wake ([`wake_entropy_seed_waiters`]). Decision log 2026-07-21, F2.
    deferred_drops: KVec<ObjectRef>,
    /// Processes whose last thread has exited and whose **handle table** still needs
    /// closing. Pushed by [`exit_process`] under `SCHED`; drained by the
    /// [reaper thread](reaper_body) and, opportunistically, by [`reap_pending`].
    ///
    /// **CPU-agnostic, unlike [`reap`](Self::reap).** That list is per-CPU because a
    /// thread parks itself there and then switches off its own stack, so freeing it from
    /// another CPU would be a use-after-free. Closing a *handle table* is keyed on pid and
    /// touches no stack, so any CPU may do it — which is what lets one reaper thread serve
    /// the whole machine.
    ended_pids: KVec<u32>,
    /// The reaper thread while it is parked. `None` while it is running or queued —
    /// the same shape as [`idle`](Self::idle), and the reason a wake is a `take()`.
    reaper: Option<ObjectRef>,
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
        stats: [stats::Counters::ZERO; MAX_CPUS],
        deferred_drops: KVec::new(),
        ended_pids: KVec::new(),
        reaper: None,
    },
);

impl SchedState {
    /// Dense index of the running CPU's per-CPU scheduler slots (`current`/`idle`/
    /// `idle_addr`). Reads the neutral `current_cpu()` (arch-internal RDTSCP); `0`
    /// on the single live CPU until APs start (slice 1).
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

    /// This CPU's reap list (mutable). Per-CPU so a dying thread is reclaimed only
    /// by the CPU it died on — after its `switch_into` completed (off-stack).
    #[inline]
    fn reap_slot(&mut self) -> &mut KVec<ObjectRef> {
        &mut self.reap[Self::this_cpu()]
    }

    /// This CPU's ready run queue (mutable). A preempted thread re-homes here, so it
    /// stays on the CPU it ran on (no free migration).
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

/// Cooperatively yield to the next ready thread, round-robin. Returns
/// immediately (still current) if no other thread is ready — it does **not**
/// yield to the idle thread, so the boot drainer's [`ready_is_empty`] gate
/// still works. Resumes here, lock-free, when this thread is scheduled again.
pub fn yield_now() {
    // Reclaim any previously-exited thread's stack first (outside the lock).
    reap_pending();

    let g = SCHED.lock();
    if g.ready[SchedState::this_cpu()].is_empty() {
        return; // nothing else ready on this CPU — keep running (guard drops, IF restored)
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
    // Charge this CPU's running TimeShared thread a tick of virtual runtime
    // (nice-weighted) before deciding whether to reschedule.
    accrue_vruntime(&mut g);
    let me = SchedState::this_cpu();
    g.stats[me].ticks += 1;
    let ready_here = !g.ready[me].is_empty();
    let (new_quantum, reschedule) = tick_quantum(g.quantum[me], ready_here);
    g.quantum[me] = new_quantum;
    // A preemption-critical window (the running thread may hold a plain lock
    // other CPUs spin on — see [`preempt_disable`]): do not deschedule it. The
    // bookkeeping above (deadlines, wakes, vruntime, quantum) still ran; latch
    // the skipped switch for `preempt_enable` / the next tick.
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
    // Idle-CPU work stealing: if this CPU is running its idle thread while a busier
    // CPU holds a thread we may run, switch — `pick_next` (empty local queue) steals
    // it. Gated on `current_is_idle` so a busy thread isn't preempted to steal.
    if !ready_here && current_is_idle(&g) && steal_available(&g, me) {
        switch_to_next(g);
    }
    // else: guard drops here — IF was already 0 (IRQ context), stays 0 until iretq.
}

/// Resume an idle CPU into the scheduler if a thread has just been made runnable on it —
/// the shared **same-CPU-wake liveness** primitive. Under `SCHED` (the caller passes the
/// held guard): honor the preempt latch (a preemption-critical window — e.g. the idle
/// thread mid-reap holding the shootdown/allocator locks — latches the wake for
/// `preempt_enable` instead of descheduling), and otherwise switch to the next runnable
/// thread **only if this CPU is idle** and there is work (our own queue or stealable). A
/// running `TimeShared` thread is left to its quantum; the woken thread is already
/// enqueued and will be picked in turn or stolen. Consumes the guard.
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

/// Handle a reschedule IPI: another CPU made a thread runnable on this CPU (a
/// cross-CPU wake or a placement onto this otherwise-idle CPU) and poked us to run
/// it now instead of waiting for the next periodic tick. If this CPU is idle and
/// there is runnable work — on our own queue or stealable from a peer — switch to
/// it; otherwise return (a busy thread keeps running until its own quantum tick).
///
/// Called from the arch reschedule-IPI dispatcher, which already EOI'd (mirroring
/// the timer path, since the switch here may not return promptly). Runs with IF=0.
pub fn on_reschedule_ipi() {
    let mut g = SCHED.lock();
    g.stats[SchedState::this_cpu()].resched_ipis += 1;
    resched_if_idle_locked(g);
}

/// Resume an idle CPU into the scheduler after a **same-CPU wake** that sent no reschedule
/// IPI — the device-IRQ counterpart of [`on_reschedule_ipi`]. Called from the device-IRQ
/// dispatch tail: an I/O-completion DPC (e.g. AHCI `irp_complete_dpc` → `complete_pending_op`)
/// can make a thread runnable on *this* CPU, and `place_thread` sends an IPI only for
/// *cross-CPU* placements, so a same-CPU wake has no other scheduling point on that path —
/// without this, an idle CPU `iretq`s straight back into `hlt` and the woken thread waits
/// for the next periodic tick (~5 ms). On a 1-vCPU guest that serialises every block I/O on
/// the tick clock, which is what the fs-server "I/O hang" actually was (see the decision
/// log, 2026-07-23 fs-server I/O wake latency). Same discipline as the IPI handler: honors
/// the preempt latch, only ever preempts the idle thread. Runs with IF=0 in IRQ context;
/// the device stub is a frame-parking stub, so a delayed return resumes into its epilogue
/// and `iretq`s exactly as the timer/resched stubs do.
pub fn resched_if_idle() {
    resched_if_idle_locked(SCHED.lock());
}

/// Capture a consistent point-in-time copy of every CPU's scheduler statistics
/// under one `SCHED` hold — the **capture** step of the capture → format →
/// synthesize snapshot discipline behind `/proc/sched/stats` (see
/// [`stats`] and `docs/architecture/scheduler.md` § "The stats surface").
/// Copies plain `Copy` data only; the caller runs [`stats::format`] (which
/// allocates) with the lock released.
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
        // Re-assert this CPU's `KERNEL_GS_BASE`. A user thread that **migrated**
        // here must `swapgs` into *this* CPU's block on its next syscall (so the
        // entry stub reads this CPU's freshly-armed `kstack_top`); without this a
        // migrated thread can syscall onto a stale block → `#DF` in `syscall_entry`.
        crate::arch::arm_user_entry_cpu_base();
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
    mut g: IrqSpinLockGuard<'_, SchedState>,
    out_slot: *mut u64,
    out_obj: *mut (),
    next_obj: *mut (),
) {
    g.stats[SchedState::this_cpu()].switches += 1;
    // Sample the **outgoing** thread's kernel-stack high-water mark. Every thread that
    // runs passes through here, including servers that spend their lives blocked and so
    // never sit in a run queue — which is why this is the sample point rather than a walk
    // of the ready lists. Instrumentation only, and O(1) unless the record actually moves.
    #[cfg(feature = "test-harness")]
    // SAFETY: `out_obj` is the pinned outgoing thread and `SCHED` is held, satisfying the
    // `Thread` accessor contract; `kstack_top` yields the top of its live painted stack
    // (`None` for a thread with no kernel stack of its own).
    if let Some(top) = unsafe { Thread::kstack_top(out_obj) } {
        // SAFETY: `out_obj` is pinned under `SCHED` (accessor contract), and `top` is that
        // live stack's exclusive top, as `sample` requires.
        unsafe {
            let pid = Thread::owner_pid_of(out_obj);
            crate::mm::kstack::watermark::sample(top, pid);
        }
    }
    // Wait out the incoming thread's mid-switch-out guard before touching its
    // parked context. A waker can re-home a thread to a **third** CPU's queue
    // (an affinity-diverted wake/resume) in the window between its old CPU
    // releasing `SCHED` and `context_switch` committing `saved_sp` — and
    // `dequeue_front`, unlike `stealable_to`, does not filter on the guard, so
    // that CPU could otherwise resume a not-yet-committed context (the Bug-4
    // double-run through a different door — F5, decision log 2026-07-21).
    // Bounded and deadlock-free even though `SCHED` is held: the owning CPU is
    // in straight-line post-release code and clears the guard without any lock.
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
    // Raise the outgoing thread's mid-switch-out guard **while still holding
    // `SCHED`** (so it is set before the lock release makes any `ready` entry
    // for `out_obj` visible to a stealer). `context_switch` clears it after it
    // commits `out_obj`'s `saved_sp`, at which point the parked context is valid
    // to resume elsewhere. Prevents a cross-CPU steal from loading a not-yet-
    // written `saved_sp` and double-running the thread (the SMP `on_cpu`
    // invariant; see `Thread::on_cpu` and `steal_one`).
    // SAFETY: `out_obj` is the pinned outgoing thread; `SCHED` still held.
    unsafe { Thread::set_on_cpu(out_obj, true) };
    // SAFETY: as above.
    let out_on_cpu = unsafe { Thread::on_cpu_ptr(out_obj) };
    // Drop the lock but keep interrupts masked across the switch. `saved_if`
    // is this thread's prior interrupt state, restored when it next resumes.
    let saved_if = g.release_keeping_irqs_masked();
    // Every context switch in the kernel passes through here, which makes this the one
    // place that can check the lock tracker's per-CPU model still holds: a thread may only
    // be switched away with nothing held and no interrupt scope open. That is already true
    // by construction (a plain-lock holder has preemption disabled, an `IrqSpinLock` holder
    // has interrupts masked, and blocking with a lock held is forbidden) — this is what
    // turns "already true" into "checked". Debug builds only; inert otherwise.
    crate::libkern::lockrank::assert_switch_safe();
    // The same idea for the *other* per-CPU invariant, which the lock check cannot see: the
    // involuntary paths (`on_timer_tick`, `resched_if_idle_locked`) latch into
    // `RESCHED_PENDING` rather than switching, and nothing checked the **voluntary** ones —
    // `yield_now` and everything that blocks — until kernel audit B.5(c). The reasoning is on
    // the function, which is also where its test points; one copy, so the two cannot drift.
    assert_switch_preempt_safe();
    // SAFETY: `next_obj` is the pinned incoming thread (re-arm its trap/syscall stack).
    unsafe { arm_kernel_stack_for(next_obj) };
    // SAFETY: `next_root` is a fully-formed PML4 (boot root, or a process root
    // with the kernel half inherited); all kernel stacks are mapped in every
    // root, so switching CR3 before the stack swap is sound. Loading it before
    // the switch also ensures a dying thread leaves CR3 on the incoming root
    // before it is reaped (its `AddressSpace::Drop` may free the old PML4).
    unsafe { Paging::set_page_table(next_root) };
    // Swap the floating-point / SIMD register file. Eager, not lazy: leaving the
    // outgoing thread's registers live behind a `CR0.TS` trap is CVE-2018-3665,
    // and Nitrox's premise is that nothing leaks between processes (see
    // `arch/../fpu.rs` for the full rationale).
    //
    // Placement is load-bearing on both ends. The **save** must precede
    // `context_switch`, because that is what clears `out_obj`'s `on_cpu` guard —
    // the FP image is part of the parked context, so it has to be complete
    // before another CPU is allowed to resume the thread and restore it. The
    // **restore** must follow the `is_on_cpu(next_obj)` spin above, for the
    // mirror-image reason: until that guard clears, the incoming thread's image
    // may still be mid-write on its old CPU.
    //
    // Restoring `next` before the stack swap rather than after is correct
    // because the FP register file is CPU state, not stack state: once
    // `context_switch` lands us on `next`'s stack we are already running with
    // `next`'s registers loaded. Neither call allocates, so both are legal in
    // this lock-free, IRQ-masked window.
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
    // Resumed (cooperative path): restore the interrupt state this thread had
    // on entry. On the preemptive path the resume returns into the timer-stub
    // epilogue (which `iretq`s IF back) and `saved_if` is false → a redundant `cli` (it was
    // a no-op until `interrupts_restore` became unconditional; same effect either way). A
    // terminal caller (`finish_exit`) never reaches this.
    // SAFETY: ring-0; restoring this thread's own captured interrupt state.
    unsafe { Cpu::interrupts_restore(saved_if) };
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
    if q == 0 { (QUANTUM_TICKS, ready_nonempty) } else { (q, false) }
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
    let next = match pick_next(&mut g) {
        Some(n) => n,
        // Only reachable if a caller violates the precondition; idle is the
        // safe fallback when it is parked (i.e. not the current thread).
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

    // Re-home prev: the idle thread parks in its slot (never in `ready`); every
    // other thread re-enqueues on **this CPU's** queue (it stays where it ran — no
    // free migration; movement happens only via placement/stealing).
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
        let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
        let obj = cur.as_ptr();
        // SAFETY: `current` is pinned alive and we hold the lock.
        match unsafe { Thread::user_entry(obj) } {
            Some((entry, user_sp)) => {
                // SAFETY: as above.
                let ktop = unsafe { Thread::kstack_top(obj) }.expect("a user thread has a kernel stack");
                // SAFETY: as above.
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
mod tests;
