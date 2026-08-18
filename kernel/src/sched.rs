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
use crate::libkern::{AllocError, IrqSpinLock, IrqSpinLockGuard, KBox, KVec};
use crate::mm::PhysAddr;
use crate::libkern::{ExitKind, ExitStatus, Notification};
use crate::libkern::ipc::IPC_HANDLE_MAX;
use crate::object::{
    BlockSendOutcome, InterruptObject, IpcChannel, MAX_WAIT_HANDLES, NotificationChannel,
    ObjectRef, PendingOperation, ReclaimedSend, RecvState, SchedClass, SendOutcome, StoredMsg,
    Thread, ThreadEntry, ThreadState, Timer, TransferRef, UserspaceServerReg,
};
use crate::object::userspace_server::{PendingFill, PendingLookup, US_PENDING_MAX};
use crate::libkern::lockrank::LockRank;

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
/// Pre-reserved capacity for [`SchedState::ended_pids`]. Exit pushes into it under
/// `SCHED`, so it must never allocate while held; the reaper drains it promptly, and the
/// existing per-CPU sites drain it too, so the depth only has to cover a burst of exits
/// between two scheduler entries.
const ENDED_PIDS_RESERVE: usize = 64;
/// Thread id of the reaper — like [`IDLE_TID`], an identity marker rather than a real tid.
const REAPER_TID: u32 = u32::MAX - 1;
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
        super::push_reserved(h, e, "deadline heap");
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
pub mod stats {
    use core::fmt::Write;

    use crate::arch::MAX_CPUS;
    use crate::libkern::{AllocError, KString};

    /// One CPU's monotonic scheduler event counters. Owned by
    /// [`SchedState`](super::SchedState) under the rank-1 `SCHED` lock;
    /// snapshot-copied by [`stats_snapshot`](super::stats_snapshot).
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct Counters {
        /// Context switches performed by this CPU — every [`switch_into`]
        /// (super), across all four parking dispositions (ready / blocked /
        /// suspended / reap). Nonzero on ≥2 CPUs is the Phase 3 milestone's
        /// "two CPUs visibly active" witness.
        pub switches: u64,
        /// Threads this CPU stole from a peer's ready queue (`steal_one`).
        pub steals: u64,
        /// Runnable threads placed **onto** this CPU's ready queue by spawn
        /// placement or wake re-homing (`place_thread`) — placements *received*,
        /// counted against the target CPU, possibly performed by another.
        pub placed: u64,
        /// Reschedule IPIs handled by this CPU (`on_reschedule_ipi`).
        pub resched_ipis: u64,
        /// Periodic scheduler ticks taken by this CPU (`on_timer_tick`).
        pub ticks: u64,
    }

    impl Counters {
        /// All-zero counters (`const` — usable in the `SCHED` static initializer).
        pub const ZERO: Counters = Counters {
            switches: 0,
            steals: 0,
            placed: 0,
            resched_ipis: 0,
            ticks: 0,
        };
    }

    /// One CPU's row in a [`Snapshot`]: its [`Counters`] plus instantaneous
    /// state read at capture time.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct CpuSnapshot {
        /// Whether this CPU's scheduler state is initialized (`cpu_online`).
        pub online: bool,
        /// Whether this CPU was running its idle thread at capture time.
        pub idle: bool,
        /// This CPU's ready-queue length at capture time.
        pub ready: u32,
        /// The monotonic event counters.
        pub counters: Counters,
    }

    impl CpuSnapshot {
        /// A never-onlined CPU's row (`const` — the snapshot array initializer).
        pub const OFFLINE: CpuSnapshot = CpuSnapshot {
            online: false,
            idle: false,
            ready: 0,
            counters: Counters::ZERO,
        };
    }

    /// A consistent point-in-time copy of every CPU's scheduler statistics,
    /// captured under one `SCHED` hold by [`stats_snapshot`](super::stats_snapshot).
    #[derive(Clone, Copy, Debug)]
    pub struct Snapshot {
        /// Per-CPU rows, indexed by dense CPU index.
        pub cpus: [CpuSnapshot; MAX_CPUS],
        /// Handle tables the reaper has closed since boot. Machine-wide, not per-CPU —
        /// there is one reaper. Carried *in the snapshot* rather than read inside
        /// [`format`], which is documented pure and host-tested as such.
        pub reaper_closed: u64,
        /// Handles closed by processes in their own exit syscall (exit-context teardown).
        pub exit_closed: u64,
    }

    impl Snapshot {
        /// The number of online CPUs in this snapshot.
        pub fn cpus_online(&self) -> usize {
            self.cpus.iter().filter(|c| c.online).count()
        }
    }

    /// Render `snap` as the `/proc/sched/stats` text: a `cpus_online=N` header
    /// line, then one `name=value` row per **online** CPU (offline CPUs are
    /// omitted — their counters are zero by construction). Booleans render as
    /// `0`/`1`. Pure (host-tested); the only failure is heap exhaustion.
    ///
    /// ```text
    /// cpus_online=2
    /// reaper_closed=17
    /// exit_closed=214
    /// cpu=0 online=1 switches=1342 steals=3 placed=57 ipis=12 ticks=4096 ready=1 idle=0
    /// cpu=1 online=1 switches=987 steals=11 placed=40 ipis=9 ticks=4080 ready=0 idle=1
    /// ```
    pub fn format(snap: &Snapshot) -> Result<KString, AllocError> {
        let mut out = KString::new();
        // `fmt::Error` from a `KString` sink only ever means allocation failure
        // (see `KString`'s `Write` impl).
        (|| -> Result<(), core::fmt::Error> {
            writeln!(out, "cpus_online={}", snap.cpus_online())?;
            writeln!(out, "reaper_closed={}", snap.reaper_closed)?;
            writeln!(out, "exit_closed={}", snap.exit_closed)?;
            for (c, cpu) in snap.cpus.iter().enumerate() {
                if !cpu.online {
                    continue;
                }
                writeln!(
                    out,
                    "cpu={} online=1 switches={} steals={} placed={} ipis={} ticks={} ready={} idle={}",
                    c,
                    cpu.counters.switches,
                    cpu.counters.steals,
                    cpu.counters.placed,
                    cpu.counters.resched_ipis,
                    cpu.counters.ticks,
                    cpu.ready,
                    cpu.idle as u8,
                )?;
            }
            Ok(())
        })()
        .map_err(|_| AllocError)?;
        Ok(out)
    }
}

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
/// **Two backings behind one interface.** On the machine this is a process-global atomic,
/// as it must be. Under `cfg(test)` it is **per-thread**, because a host test thread stands
/// in for a whole machine here and cargo runs tests concurrently by default: one shared set
/// is the D.2 hazard that already produced a 33 % failure rate at `--test-threads=16` when
/// the placement tests wrote it. Production codegen is unchanged — the `cfg(not(test))` arm
/// is the original atomic, verbatim.
mod online_set {
    #[cfg(not(test))]
    mod backing {
        use core::sync::atomic::{AtomicU64, Ordering};

        static ONLINE_MASK: AtomicU64 = AtomicU64::new(0);

        pub(in crate::sched) fn load() -> u64 {
            ONLINE_MASK.load(Ordering::Acquire)
        }
        pub(in crate::sched) fn add(cpu: usize) {
            ONLINE_MASK.fetch_or(1u64 << cpu, Ordering::Release);
        }
        pub(in crate::sched) fn remove(cpu: usize) {
            ONLINE_MASK.fetch_and(!(1u64 << cpu), Ordering::AcqRel);
        }
    }

    #[cfg(test)]
    mod backing {
        std::thread_local! {
            static ONLINE_MASK: core::cell::Cell<u64> = const { core::cell::Cell::new(0) };
        }

        pub(in crate::sched) fn load() -> u64 {
            ONLINE_MASK.with(|m| m.get())
        }
        pub(in crate::sched) fn add(cpu: usize) {
            ONLINE_MASK.with(|m| m.set(m.get() | (1u64 << cpu)));
        }
        pub(in crate::sched) fn remove(cpu: usize) {
            ONLINE_MASK.with(|m| m.set(m.get() & !(1u64 << cpu)));
        }
        /// Establish a starting set. Test-only: the machine reaches its set by booting.
        pub(in crate::sched) fn force(mask: u64) {
            ONLINE_MASK.with(|m| m.set(mask));
        }
    }

    pub(in crate::sched) use backing::*;
}

/// The set of online CPUs as a bitmask (dense index → bit), read lock-free.
/// Used by [`crate::tlb`] to pick TLB-shootdown IPI targets without taking `SCHED`.
pub fn online_mask() -> u64 {
    online_set::load()
}

/// Drop this CPU from the online set, because it is about to stop executing forever.
///
/// **Every permanent park must call this, and the reason is a whole-machine deadlock.**
/// `online_mask` is the target set for TLB-shootdown IPIs, and [`crate::tlb::shootdown`]
/// waits for an acknowledgement from *every* CPU in it. A CPU that halts while still
/// counted online can never send that acknowledgement, so the next shootdown — which is
/// any `sys_memory_unmap`, i.e. any large `free` in any process — spins forever, taking
/// the initiating thread with it and, through the shootdown lock, every later one.
///
/// That is not hypothetical: it is exactly how a parked CPU froze `nxterm` mid-repaint
/// under `check-terminal` (2026-08-13). The CPU had halted with interrupts disabled in
/// `debug_exit`'s device-absent fallback ~6,000 ticks earlier, and nothing noticed until
/// a glyph outline was freed.
///
/// **Does nothing unless this CPU's identity is its own.** A CPU that has not yet
/// established one reports index `0` (x86's `IA32_TSC_AUX` reset default), so an
/// unchecked version of this clears *the BSP's* bit when an AP with an unbound APIC id
/// parks — leaving the running BSP out of the shootdown set, where the slow path's
/// self-IPI is the only thing that invalidates its own TLB. The result is a freed frame
/// still reachable through a stale translation: silent memory corruption, strictly worse
/// than the deadlock this function exists to prevent (PR #198 review, blocking 1).
/// [`ArchSmp::identity_bound`](crate::arch::smp::ArchSmp::identity_bound) is that check;
/// the range check below is a separate, weaker guard and does not cover this — an unbound
/// CPU's `0` is in range.
///
/// **The check errs toward doing nothing, including once where it arguably should act.**
/// The BSP's bit is set by [`init`] (`main.rs:353`) before `bind_cpu_identity(0, …)`
/// (`main.rs:432`), so a BSP panic in that window finds its own identity unbound and leaves
/// its bit set. That is harmless rather than lucky: APs are not launched until after the
/// binding, so in that window no other CPU exists to initiate a shootdown and wait on it.
/// The asymmetry is deliberate — failing to clear a bit costs a hang on a machine that has
/// already panicked, while clearing the wrong one costs silent memory corruption on a
/// healthy one.
pub fn leave_online() {
    use crate::arch::smp::ArchSmp;
    clear_online_bit(crate::arch::Smp::identity_bound(), crate::arch::Smp::current_cpu() as usize);
}

/// The decision half of [`leave_online`], split out so the rule that cost a review round is
/// pinned by a host test rather than only by a comment.
fn clear_online_bit(identity_bound: bool, me: usize) {
    if !identity_bound || me >= MAX_CPUS {
        return;
    }
    online_set::remove(me);
}

/// Assert that a thread arriving from ring 3 has preemption enabled.
///
/// **The mirror of the underflow guard in [`preempt_enable`], for the imbalance that escapes
/// every other check.** A *leaked* `preempt_disable` wedges a CPU exactly as the wrap did:
/// `on_timer_tick` latches and returns forever, and a later enable goes 2 → 1 and never
/// reaches 0, so neither the underflow assert nor the replay ever fires. `switch_into`'s
/// assert catches it at the thread's next voluntary switch — but a thread that returns to
/// ring 3 and makes only non-blocking syscalls never reaches `switch_into`, and that CPU
/// simply stops preempting with nothing said.
///
/// This is the boundary where the truth is free, and it is the same argument
/// `assert_user_entry_safe` already makes for locks one line above the call site: a thread
/// that was executing in user mode cannot hold a kernel lock, and cannot have disabled
/// kernel preemption either. Debug builds only (PR #203 review, optional 1).
pub fn assert_user_entry_preempt_safe() {
    debug_assert_eq!(
        PREEMPT_OFF[SchedState::this_cpu()].load(Ordering::Relaxed),
        0,
        "entered a syscall from ring 3 with preemption disabled — a kernel path leaked a \
         `preempt_disable`, and this CPU will stop preempting until something notices"
    );
}

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

/// Per-CPU preemption-disable depth (see [`preempt_disable`]). Written only by
/// the thread running on that CPU (which, while nonzero, cannot migrate — that
/// is the point); read by the same CPU's tick / reschedule-IPI handlers.
static PREEMPT_OFF: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(0) }; MAX_CPUS];

/// Set when a reschedule opportunity (tick expiry or reschedule IPI) was
/// skipped because [`PREEMPT_OFF`] was raised; consumed by [`preempt_enable`],
/// which replays the reschedule. Backstop: the CPU's next periodic tick.
static RESCHED_PENDING: [AtomicBool; MAX_CPUS] = [const { AtomicBool::new(false) }; MAX_CPUS];

/// Disable preemption on the current CPU (nestable). While the depth is
/// nonzero, the tick and the reschedule IPI will not deschedule the running
/// thread (interrupts themselves stay enabled — handlers run; only the
/// *switch* is deferred, latched in [`RESCHED_PENDING`]).
///
/// This exists for **preemption-critical windows**: code holding a plain
/// spinlock other CPUs may spin on — the TLB-shootdown serialiser, the
/// allocator locks during reclamation. A preempted *normal* holder sits in a
/// ready queue and is rescheduled within a tick (slow, but live); the **idle
/// thread** is the fatal case — it parks in `idle_slot` and runs only when its
/// CPU has nothing else, so if it is descheduled while holding such a lock,
/// the spinners themselves keep every CPU busy and the holder is starved
/// forever (F12, decision log 2026-07-21 — an all-CPUs-spinning-on-the-
/// shootdown-lock deadlock found by the exit-storm stress).
///
/// Contract: pair with [`preempt_enable`] on the same thread; the window must
/// be bounded (no blocking, no `sys_wait`) and hold no `SCHED`-ordered lock.
///
/// [`SpinLock`](crate::libkern::SpinLock) calls this in `lock()` (and the
/// matching enable in its guard drop), making **every plain-spinlock critical
/// section a no-preemption region**: a holder is never descheduled, so a
/// spinner — even one with interrupts masked, which cannot tick — always waits
/// on a *running* holder and the wait is bounded by the critical section.
pub fn preempt_disable() {
    // Mask IRQs across read-index + increment: an interrupt between them could
    // deschedule/migrate this thread and the increment would land on the wrong
    // CPU's counter (sticking it nonzero — that CPU would never preempt again).
    // Once the count is raised, descheduling is impossible, which is what makes
    // the per-CPU counter track the thread for the window's duration.
    let prev = preempt_irqs_mask();
    PREEMPT_OFF[SchedState::this_cpu()].fetch_add(1, Ordering::Relaxed);
    preempt_irqs_restore(prev);
}

/// Re-enable preemption (see [`preempt_disable`]); at depth zero, replays a
/// reschedule that was skipped during the window. (If the thread is descheduled
/// or migrates between the depth reaching zero and the replay, the replay's
/// under-lock re-check makes it a no-op; the origin CPU's next tick covers any
/// consumed-but-unserviced wake.)
pub fn preempt_enable() {
    let prev = preempt_irqs_mask();
    let me = SchedState::this_cpu();
    // **Saturating, not wrapping.** An unbalanced enable at depth 0 would set the counter to
    // `u32::MAX`, and that CPU would never preempt again — the exact failure
    // `preempt_disable`'s own comment names, reached from the other direction. `[profile.dev]`
    // sets `overflow-checks = true` and it does not help here: an atomic RMW wraps by
    // definition, so the dev profile is as silent as release (kernel audit B.5(e)).
    //
    // The `debug_assert` is the loud half and the saturation is the safe half: a build that
    // checks catches the unbalanced enable at the moment it happens, and one that does not
    // still refuses to brick the CPU.
    //
    // **Do not "simplify" this to a plain load/store.** The obvious argument for it — the
    // counter is per-CPU and interrupts are masked here, so the RMW is redundant — holds on
    // target and fails under `cfg(test)`, where `preempt_irqs_mask` is a no-op and
    // `this_cpu()` collapses every host-test thread onto index 0. The atomicity is
    // load-bearing for the host configuration even though it looks redundant on the one the
    // comment above describes.
    let depth_before = PREEMPT_OFF[me]
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |d| Some(d.saturating_sub(1)))
        // unwrap_or: the closure is infallible, so `fetch_update` cannot return `Err` here.
        .unwrap_or(0);
    debug_assert_ne!(
        depth_before, 0,
        "preempt_enable with no matching preempt_disable — the counter would have wrapped \
         to u32::MAX and this CPU would never preempt again"
    );
    let replay = depth_before == 1 && RESCHED_PENDING[me].swap(false, Ordering::AcqRel);
    preempt_irqs_restore(prev);
    if replay {
        // Mirror `on_reschedule_ipi`: resume an idle CPU into the scheduler; a
        // busy thread is left to its quantum (the next tick reschedules it).
        let g = SCHED.lock();
        let cpu = SchedState::this_cpu();
        if current_is_idle(&g) && (!g.ready[cpu].is_empty() || steal_available(&g, cpu)) {
            switch_to_next(g);
        }
    }
}

/// Mask interrupts for the preempt-counter update window. Host tests run ring-3
/// single-threaded (a real `cli` would `#GP`), and have no ticks to race — a
/// no-op there, mirroring the `IrqSpinLock` test backend.
#[inline]
fn preempt_irqs_mask() -> bool {
    #[cfg(not(test))]
    {
        // SAFETY: ring-0; a microseconds-bounded masked window, restored below.
        unsafe { Cpu::interrupts_disable() }
    }
    #[cfg(test)]
    {
        false
    }
}

/// Restore the interrupt state captured by [`preempt_irqs_mask`].
#[inline]
fn preempt_irqs_restore(prev: bool) {
    #[cfg(not(test))]
    // SAFETY: ring-0; restoring the state captured by the paired mask.
    unsafe {
        Cpu::interrupts_restore(prev)
    };
    #[cfg(test)]
    let _ = prev;
}

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

static SCHED: IrqSpinLock<SchedState> = IrqSpinLock::new(LockRank::Sched, SchedState {
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
});

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
    let mut deferred_drops: KVec<ObjectRef> = KVec::new();
    deferred_drops.try_reserve(DEFERRED_DROP_RESERVE)?;
    let boot = Thread::try_new_boot(0, 0)?;
    let boot_ref = into_objref(boot);

    // The idle thread: a runnable kernel thread with its own stack that just
    // halts. Built outside the lock (it allocates a kernel stack). It is never
    // enqueued or reaped — its body loops forever.
    let idle = Thread::try_new_runnable(IDLE_TID, 0, idle_body, 0)?;
    let idle_ref = into_objref(idle);
    let idle_addr = idle_ref.as_ptr() as usize;

    // The reaper: **one** for the machine, not one per CPU. It closes handle tables, which
    // are keyed on pid and touch no thread stack, so any CPU may do it. (The per-CPU
    // `reap` list is a different job with a different rule — see `reaper_body`.)
    // Allocated outside the lock, like the idle thread; it starts runnable and parks
    // itself the first time it finds nothing to do.
    let reaper = Thread::try_new_runnable(REAPER_TID, 0, reaper_body, 0)?;
    let reaper_ref = into_objref(reaper);
    let mut ended_pids: KVec<u32> = KVec::new();
    ended_pids.try_reserve(ENDED_PIDS_RESERVE)?;

    let mut g = SCHED.lock();
    g.ready[0] = ready; // BSP is logical CPU 0; APs reserve their own in `ap_init`.
    g.cpu_online[0] = true; // the BSP; each AP sets its own bit in `ap_init`.
    online_set::add(0); // lock-free mirror (BSP = bit 0)
    g.blocked = blocked;
    g.suspended = suspended;
    g.reap[0] = reap;
    g.deadlines = deadlines;
    g.deferred_drops = deferred_drops;
    g.ended_pids = ended_pids;
    push_reserved(&mut g.ready[0], reaper_ref, "run queue (reaper install)");
    *g.cur_slot() = Some(boot_ref);
    *g.idle_slot() = Some(idle_ref);
    g.set_idle_addr(idle_addr);
    g.quantum = [QUANTUM_TICKS; MAX_CPUS];
    Ok(())
}

/// Bring an application processor into the scheduler. Run on the AP itself after
/// its architecture bring-up ([`crate::arch::ap_cpu_init`]) and timer arming.
/// Creates this CPU's **boot thread** (adopting the running Limine context) and
/// its **idle thread**, installs them in this CPU's `current`/`idle` slots, enables
/// preemption, and retires the boot thread — leaving the AP running idle and
/// pulling runnable threads from the **shared** `ready` queue. Diverges.
///
/// The global run/blocked/reap/deadline structures are *not* (re)built here —
/// [`init`] did that once on the BSP; an AP only adds its own per-CPU slots.
pub fn ap_run() -> ! {
    if ap_init().is_err() {
        // Out of memory building this CPU's scheduler context — park the AP
        // rather than corrupt the run queue.
        crate::kprintln!("smp: AP scheduler init FAILED — parking CPU");
        Cpu::halt_loop();
    }
    // SAFETY: this CPU's `current` (boot) and `idle` slots are set, so a timer
    // tick can schedule. Arm preemption, then retire the boot thread into the
    // run loop (the AP continues as its idle thread until `ready` has work).
    unsafe { Cpu::interrupts_enable() };
    exit_thread(ExitStatus {
        kind: ExitKind::Normal as u32,
        code: 0,
    });
}

/// Per-CPU scheduler-context setup for an AP: a boot thread (adopting the running
/// context) as `current`, plus this CPU's idle thread. Mirrors the per-CPU half of
/// [`init`] without the one-time global reserves.
fn ap_init() -> Result<(), AllocError> {
    // A fresh tid for this AP's (transient) boot thread; the idle thread reuses
    // `IDLE_TID` (idle identity is its object address, via `idle_addr`).
    let boot_tid = {
        let mut g = SCHED.lock();
        let t = g.next_tid;
        g.next_tid = g.next_tid.wrapping_add(1);
        t
    };
    let boot = Thread::try_new_boot(boot_tid, 0)?;
    let boot_ref = into_objref(boot);
    let idle = Thread::try_new_runnable(IDLE_TID, 0, idle_body, 0)?;
    let idle_ref = into_objref(idle);
    let idle_addr = idle_ref.as_ptr() as usize;
    // This CPU's own reap list + ready queue — reserved outside the lock so neither
    // `finish_exit`'s reap push nor a placement onto this CPU allocates under `SCHED`.
    let mut reap: KVec<ObjectRef> = KVec::new();
    reap.try_reserve(REAP_RESERVE)?;
    let mut ready: KVec<ObjectRef> = KVec::new();
    ready.try_reserve(READY_RESERVE)?;

    let mut g = SCHED.lock();
    *g.cur_slot() = Some(boot_ref);
    *g.idle_slot() = Some(idle_ref);
    g.set_idle_addr(idle_addr);
    *g.reap_slot() = reap;
    *g.ready_slot() = ready;
    // This AP is now schedulable — mark *its* bit (set in the same critical section
    // that reserved its queues, so no placement targets it before it is reserved).
    let cpu = SchedState::this_cpu();
    g.cpu_online[cpu] = true;
    online_set::add(cpu); // lock-free mirror
    Ok(())
}

/// Create a runnable kernel thread that will run `entry(arg)` in the default
/// **TimeShared** class and enqueue it. Returns the new thread id.
pub fn spawn(entry: ThreadEntry, arg: usize) -> Result<u32, AllocError> {
    spawn_with_class(entry, arg, SchedClass::TimeShared, 0, 0)
}

/// The number of CPUs whose scheduler state is online (BSP + initialized APs).
pub fn online_cpus() -> usize {
    SCHED.lock().cpu_online.iter().filter(|&&o| o).count()
}

/// Set a thread's CPU **affinity** mask (`sys_thread_set_affinity`). Takes effect at
/// the thread's next placement / steal / wake; a thread already running on a now-
/// excluded CPU finishes its slice there and moves on its next reschedule (affinity is
/// advisory for the in-flight slice). The caller pins `thread` via a held handle ref.
pub fn set_thread_affinity(thread: *mut (), mask: u8) {
    let _g = SCHED.lock();
    // SAFETY: `thread` pins a live Thread (caller holds a handle ref); `SCHED` held.
    unsafe { Thread::set_cpu_mask(thread, mask) };
}

/// As [`spawn`] but pinned to the CPUs in `cpu_mask` (bit `c` ⇒ may run on CPU `c`).
/// Kernel-internal helper for affinity demos/tests; user threads use
/// `sys_thread_set_affinity`.
pub fn spawn_with_affinity(entry: ThreadEntry, arg: usize, cpu_mask: u8) -> Result<u32, AllocError> {
    spawn_inner(entry, arg, SchedClass::TimeShared, 0, 0, cpu_mask)
}

/// As [`spawn`], but in scheduling `class` with `rt_priority` (RealTime) / `nice`
/// (TimeShared). Kernel-internal: trusted callers set the class directly (the
/// `REAL_TIME` syscap gate for *user* threads lands with the capability system).
/// The stack allocation + frame fabrication happen before the (brief) enqueue lock.
pub fn spawn_with_class(
    entry: ThreadEntry,
    arg: usize,
    class: SchedClass,
    rt_priority: u8,
    nice: i8,
) -> Result<u32, AllocError> {
    spawn_inner(entry, arg, class, rt_priority, nice, u8::MAX)
}

/// The shared body of every kernel-thread spawn: fabricate the thread, set its
/// scheduling parameters + affinity, and place it on a CPU's run queue. `cpu_mask`
/// `u8::MAX` means no affinity restriction.
fn spawn_inner(
    entry: ThreadEntry,
    arg: usize,
    class: SchedClass,
    rt_priority: u8,
    nice: i8,
    cpu_mask: u8,
) -> Result<u32, AllocError> {
    let tid = {
        let mut g = SCHED.lock();
        let t = g.next_tid;
        g.next_tid = g.next_tid.wrapping_add(1);
        t
    };
    // Heavy work outside the lock.
    let thread = Thread::try_new_runnable(tid, 0, entry, arg)?;
    let r = into_objref(thread);
    let obj = r.as_ptr();
    // SAFETY: `obj` is the new thread, exclusively owned via `r` and not yet
    // enqueued — no other context can observe it, so setting its scheduling
    // parameters here (before the lock) is sound.
    unsafe {
        Thread::set_sched(obj, class, rt_priority, nice);
        Thread::set_cpu_mask(obj, cpu_mask);
    }

    let leftover = {
        let mut g = SCHED.lock();
        // Place on the least-loaded CPU's queue (seeding vruntime at its floor).
        // `place_thread` refuses rather than grow under the rank-1 lock, handing `r`
        // back so it drops below, lock-free (its `KernelStack` is rank-6 reclaimed).
        match place_thread(&mut g, r, false) {
            Ok(()) => return Ok(tid),
            Err(returned) => returned, // carry out of the locked block
        }
    };
    // Lock released: `leftover` drops here, releasing the thread's last reference and
    // freeing its kernel stack off the rank-1 lock.
    drop(leftover);
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
    // Default scheduling: TimeShared, nice 0, no affinity restriction — the behavior
    // every caller had before scheduling params were exposed via `ThreadArgs`.
    spawn_user_sched(
        process,
        entry,
        user_sp,
        boot_args,
        SchedClass::TimeShared,
        0,
        0,
        u8::MAX,
    )
}

/// As [`spawn_user`], but with explicit scheduling parameters (from a user thread's
/// `ThreadArgs`). The `RealTime` class is `REAL_TIME`-gated by the caller
/// (`sys_thread_create`); this sets `class`/`rt_priority`/`nice`/`cpu_mask` on the
/// fresh thread **before** it is enqueued, so it never runs a scheduler tick under the
/// wrong class. `cpu_mask` `u8::MAX` ⇒ no affinity restriction.
#[allow(clippy::too_many_arguments)]
pub fn spawn_user_sched(
    process: ObjectRef,
    entry: u64,
    user_sp: u64,
    boot_args: [u64; 4],
    class: SchedClass,
    rt_priority: u8,
    nice: i8,
    cpu_mask: u8,
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
    let obj = r.as_ptr();
    // SAFETY: `obj` is the new thread, exclusively owned via `r` and not yet enqueued —
    // no other context can observe it, so setting its scheduling parameters here
    // (before the enqueue lock) is sound. Mirrors `spawn_inner`.
    unsafe {
        Thread::set_sched(obj, class, rt_priority, nice);
        Thread::set_cpu_mask(obj, cpu_mask);
    }
    // Clone the caller's handle before the enqueue moves `r` into `ready`.
    let handle = r.clone();

    let leftover = {
        let mut g = SCHED.lock();
        match place_thread(&mut g, r, false) {
            Ok(()) => return Ok(handle),
            Err(returned) => returned, // carry out of the locked block
        }
    };
    // Over capacity: lock released; `leftover` (the thread ref) and `handle` drop
    // here — releasing the thread's references, freeing its kernel stack, and
    // releasing the Process off the rank-1 lock.
    drop(leftover);
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
    if PREEMPT_OFF[me].load(Ordering::Relaxed) > 0 {
        if reschedule || (!ready_here && current_is_idle(&g) && steal_available(&g, me)) {
            RESCHED_PENDING[me].store(true, Ordering::Release);
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
        return;
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
    if PREEMPT_OFF[me].load(Ordering::Relaxed) > 0 {
        RESCHED_PENDING[me].store(true, Ordering::Release);
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

/// `true` if this CPU's current thread is its idle thread (its object address equals
/// this CPU's `idle_addr`). Caller holds `SCHED`.
fn current_is_idle(g: &SchedState) -> bool {
    g.cur_ref()
        .as_ref()
        .is_some_and(|c| c.as_ptr() as usize == g.idle_addr())
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
            idle: g.current[c]
                .as_ref()
                .is_some_and(|t| t.as_ptr() as usize == g.idle_addr[c]),
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

/// Complete any `PendingOperation`s parked on the entropy pool becoming seeded (the
/// unseeded `sys_entropy_read` path). Gated by a cheap lock-free check so the common
/// already-seeded / no-waiters case costs one atomic load. Caller holds `SCHED`.
///
/// The entropy subsystem owns the waiter refs (the IPC-`Block` pattern); we move
/// them out, signal each, and **park the refs in [`SchedState::deferred_drops`]**
/// for [`reap_pending`] to drop in thread context. They must not drop here: an
/// `ObjectRef` drop can reach the plain-spinlock allocator, which may neither run
/// under the IRQ-acquired `SCHED` lock (a cross-CPU deadlock against an allocator
/// holder whose own tick spins on `SCHED`) nor anywhere in this tick's IRQ context
/// (a same-CPU self-deadlock against an allocator holder this tick interrupted).
/// See the decision log (2026-07-21, F2) and `kernel/docs/lock-ordering.md`.
fn wake_entropy_seed_waiters(g: &mut SchedState) {
    if !crate::entropy::seed_wake_pending() {
        return;
    }
    let mut buf: [Option<ObjectRef>; crate::entropy::SEED_WAITERS_MAX] =
        [const { None }; crate::entropy::SEED_WAITERS_MAX];
    let n = crate::entropy::drain_seed_waiters(&mut buf);
    for slot in buf[..n].iter_mut() {
        if let Some(po) = slot.take() {
            signal_pending_op_with_result(g, po.as_ptr(), 0, 0);
            // A move within the reserve — no drop, no allocation under the lock.
            // Bounded: waiters queue only pre-seed and the drain fires post-seed,
            // so the lifetime total is ≤ SEED_WAITERS_MAX (< the reserve).
            push_reserved(&mut g.deferred_drops, po, "deferred-drop list");
        }
    }
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
        KObjectType::InterruptObject => unsafe { InterruptObject::already_signaled(obj) },
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
        KObjectType::InterruptObject => unsafe { InterruptObject::add_waiter(obj, th) },
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
        KObjectType::InterruptObject => unsafe { InterruptObject::remove_waiter(obj, th) },
        _ => {}
    }
}

/// Wake every thread blocked on `channel` (its queue just went non-empty).
/// Caller holds `SCHED`. No allocation, no blocking — safe from the fault path.
/// Mirrors [`fire_timer`]'s waiter-drain.
/// Ask `process` to exit: enqueue [`KIND_TERMINATE_REQUESTED`] on **its own**
/// notification channel and wake anything blocked there (§11h).
///
/// **The kernel does nothing else.** It does not stop threads, unwind stacks, or touch the
/// target's execution in any way — so there is no safe-point question and no second
/// teardown path beside [`exit_process`]. A process that listens exits itself; one that
/// does not keeps running, which is what "there is no forcible kill in this system" means
/// in practice.
///
/// `false` when the target has no notification channel attached — it can never be asked,
/// which the caller reports rather than silently succeeding at.
pub fn request_terminate(process: *mut ()) -> bool {
    let mut g = SCHED.lock();
    // SAFETY: the caller pins `process` through its Process handle for this call.
    let chan = unsafe { &*(process as *const crate::object::Process) }.notification_channel_ptr();
    let Some(c) = chan else { return false };
    let notif = Notification::terminate_requested();
    // SAFETY: `c` is the channel that Process owns a reference on, so it outlives this
    // call; `SCHED` held; the queue is pre-reserved, so no allocation happens here.
    let _edge = unsafe { NotificationChannel::enqueue(c, notif) };
    signal_channel(&mut g, c);
    true
}

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

/// Record an interrupt on `irq` (an [`InterruptObject`]) and wake every thread
/// blocked on it. Latching: an interrupt with no current waiter increments the
/// object's pending count and is delivered to the next waiter. Taking `SCHED`,
/// so it is callable from a DPC handler (interrupt-dispatch tail) — the path a
/// device ISR's completion takes. No allocation, no blocking, no `ObjectRef`
/// drop. The caller's reference (or a registered handler) pins `irq`.
pub fn signal_interrupt(irq: *mut ()) {
    let mut g = SCHED.lock();
    // SAFETY: live `InterruptObject`, `SCHED` held — latches a pending interrupt.
    unsafe { InterruptObject::signal(irq) };
    let mut buf = [core::ptr::null_mut(); InterruptObject::MAX_WAITERS];
    // SAFETY: live object, `SCHED` held — drains its waiter list.
    let n = unsafe { InterruptObject::take_waiters(irq, &mut buf) };
    for &th in &buf[..n] {
        // SAFETY: each waiter is a thread blocked in `wait_on`, pinned in
        // `blocked`; `SCHED` held.
        unsafe {
            Thread::wait_mark_signaled(th, irq as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(&mut g, th);
            }
        }
    }
}

/// Consume one pending interrupt on `irq` — called by `sys_wait` when it returns
/// for an [`InterruptObject`], so a driver's wait→service→wait loop wakes once
/// per interrupt. Takes `SCHED` (the accessor contract). The caller's handle
/// pins `irq`.
pub fn interrupt_consume(irq: *mut ()) {
    let _g = SCHED.lock();
    // SAFETY: live `InterruptObject` pinned by the caller's handle; `SCHED` held.
    unsafe { InterruptObject::consume(irq) };
}

/// `true` iff `irq` (an [`InterruptObject`]) has an unconsumed pending interrupt.
/// Takes `SCHED` (the accessor contract). Used by the boot self-test.
pub fn interrupt_pending(irq: *mut ()) -> bool {
    let _g = SCHED.lock();
    // SAFETY: live `InterruptObject` pinned by the caller's reference; `SCHED` held.
    unsafe { InterruptObject::already_signaled(irq) }
}

/// `true` iff `po` (a [`PendingOperation`]) has completed. Takes `SCHED` (the
/// accessor contract); used by a boot self-test to poll for an async completion
/// without a thread to `sys_wait`.
pub fn pending_op_is_signaled(po: *mut ()) -> bool {
    let _g = SCHED.lock();
    // SAFETY: live `PendingOperation` pinned by the caller's reference; `SCHED` held.
    unsafe { PendingOperation::already_signaled(po) }
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

/// Outcome of [`us_forward_originate`] — how a forwarded `sys_ns_lookup` fared
/// when the kernel tried to hand its `Namespace::Resolve` request to the server.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ForwardOutcome {
    /// The request was delivered; the lookup's `PendingOperation` is left
    /// **pending** and will complete when the server replies.
    Pending,
    /// A forwarded lookup is already outstanding on this server (N = 1) — the
    /// caller fails the new lookup (`WouldBlock`).
    Busy,
    /// The server's inbox ring is full — the caller fails the lookup
    /// (`WouldBlock`).
    Full,
    /// The server endpoint has closed — the caller fails the lookup
    /// (`PeerClosed`).
    PeerClosed,
}

/// Originate a forwarded namespace lookup: under one `SCHED` hold, reserve the
/// registration's (single) pending-lookup slot — assigning a `request_id` and
/// recording the lookup `po` / `owner_pid` / requested `Rights` — stamp that id
/// into the already-built `Namespace::Resolve` request `msg`, and push the request
/// into the server's inbox (the peer of the registration's kernel endpoint),
/// waking the server's blocked receivers. The lookup PO is left **uncompleted**
/// (the server's reply completes it later, inline in its send — see
/// [`us_forward_take_pending`]). On `Busy` nothing was reserved; on `Full` /
/// `PeerClosed` the reserved slot is rolled back and its `po` clone dropped
/// outside `SCHED`. `reg` is the [`UserspaceServerReg`] the lookup resolved to;
/// the caller pins it (and `po`) for the call.
/// Push a **fire-and-forget** message into a registration's server inbox: no pending slot,
/// no `request_id`, no reply expected. `true` if it was queued.
///
/// The sibling of [`us_forward_originate`] for the one message the kernel sends a server
/// without asking it anything — `File::Touch` after a Model A writeback. Nothing is
/// reserved, so nothing needs rolling back: a full ring or a dead server just drops the
/// notification, which degrades to the behaviour that existed before it (a stale `mtime`)
/// rather than failing the caller's `sys_file_sync`, whose data is already on the device.
pub fn us_forward_notify(reg: *mut (), msg: &mut StoredMsg) -> bool {
    let mut g = SCHED.lock();
    // SAFETY: `reg` is a live `UserspaceServerReg` pinned by the caller; `SCHED` held, so
    // the registration's kernel endpoint is pinned with it.
    let endpoint = unsafe { UserspaceServerReg::endpoint_ptr(reg) };
    let mut no_transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    // SAFETY: `endpoint` is the registration's live kernel endpoint; `SCHED` held.
    match unsafe { IpcChannel::send_push(endpoint, msg, &mut no_transfers) } {
        SendOutcome::Sent { woke_edge } => {
            if woke_edge {
                // SAFETY: a successful send proves the peer is non-null; `SCHED` held.
                let peer = unsafe { IpcChannel::peer_of(endpoint) };
                if !peer.is_null() {
                    signal_ipc_endpoint(&mut g, peer);
                }
            }
            true
        }
        SendOutcome::Full | SendOutcome::PeerClosed => false,
    }
}

pub fn us_forward_originate(
    reg: *mut (),
    msg: &mut StoredMsg,
    po: &ObjectRef,
    owner_pid: u32,
    requested: Rights,
    suffix: &[u8],
) -> ForwardOutcome {
    let mut g = SCHED.lock();
    // SAFETY: `reg` is a live `UserspaceServerReg` pinned by the caller; `SCHED`
    // held. Reserve the pending slot + assign a request id (None ⇒ already busy).
    let request_id =
        match unsafe { UserspaceServerReg::begin(reg, po, owner_pid, requested, suffix) } {
            Some(id) => id,
        None => return ForwardOutcome::Busy,
    };
    // Stamp the assigned id into the request envelope (the body was built by the
    // caller with a zero placeholder).
    crate::rsproto::stamp_request_id(&mut msg.payload, request_id);
    // SAFETY: `reg` live, `SCHED` held — the kernel endpoint is pinned by `reg`.
    let endpoint = unsafe { UserspaceServerReg::endpoint_ptr(reg) };
    let mut no_transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    // SAFETY: `endpoint` is the registration's live kernel endpoint; `SCHED` held.
    let outcome = unsafe { IpcChannel::send_push(endpoint, msg, &mut no_transfers) };
    match outcome {
        SendOutcome::Sent { woke_edge } => {
            if woke_edge {
                // SAFETY: a successful send proves the peer is non-null; `SCHED` held.
                let peer = unsafe { IpcChannel::peer_of(endpoint) };
                if !peer.is_null() {
                    signal_ipc_endpoint(&mut g, peer);
                }
            }
            ForwardOutcome::Pending
        }
        SendOutcome::Full | SendOutcome::PeerClosed => {
            // Roll back the slot we just reserved (by its `request_id`); drop its
            // `po` clone **outside** `SCHED`.
            // SAFETY: `reg` live, `SCHED` held.
            let pl = unsafe { UserspaceServerReg::take_pending_matching(reg, request_id) };
            drop(g);
            drop(pl);
            if outcome == SendOutcome::Full {
                ForwardOutcome::Full
            } else {
                ForwardOutcome::PeerClosed
            }
        }
    }
}

/// Originate a forwarded page-cache **fill**: the page-fault sibling of
/// [`us_forward_originate`]. Under one `SCHED` hold, reserve the registration's
/// (single) pending-fill slot — recording the fill `po` / `file_obj` / `frame` /
/// page `index` and assigning a `request_id` — stamp that id into the already-built
/// `File::ReadRange` request `msg`, and push it into the server's inbox. The fill
/// PO is left **uncompleted** (the server's `ReadRange` reply completes it inline in
/// its send — see [`us_forward_take_pending_fill`]). On `Busy` nothing is reserved;
/// on `Full` / `PeerClosed` the reserved slot is rolled back and its references
/// dropped outside `SCHED`. `reg` is the [`UserspaceServerReg`] the faulted
/// `FileObject`'s producer points at; the caller pins it (and `po` / `file_obj`).
pub fn us_forward_originate_fill(
    reg: *mut (),
    msg: &mut StoredMsg,
    po: &ObjectRef,
    file_obj: &ObjectRef,
    frame: crate::mm::PhysAddr,
    index: usize,
) -> ForwardOutcome {
    let mut g = SCHED.lock();
    // SAFETY: `reg` is a live `UserspaceServerReg` pinned by the caller; `SCHED`
    // held. Reserve the pending-fill slot + assign a request id (None ⇒ busy).
    let request_id =
        match unsafe { UserspaceServerReg::begin_fill(reg, po, file_obj, frame, index) } {
            Some(id) => id,
            None => return ForwardOutcome::Busy,
        };
    crate::rsproto::stamp_request_id(&mut msg.payload, request_id);
    // SAFETY: `reg` live, `SCHED` held — the kernel endpoint is pinned by `reg`.
    let endpoint = unsafe { UserspaceServerReg::endpoint_ptr(reg) };
    let mut no_transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    // SAFETY: `endpoint` is the registration's live kernel endpoint; `SCHED` held.
    let outcome = unsafe { IpcChannel::send_push(endpoint, msg, &mut no_transfers) };
    match outcome {
        SendOutcome::Sent { woke_edge } => {
            if woke_edge {
                // SAFETY: a successful send proves the peer is non-null; `SCHED` held.
                let peer = unsafe { IpcChannel::peer_of(endpoint) };
                if !peer.is_null() {
                    signal_ipc_endpoint(&mut g, peer);
                }
            }
            ForwardOutcome::Pending
        }
        SendOutcome::Full | SendOutcome::PeerClosed => {
            // Roll back the slot we just reserved (by its `request_id`); drop the
            // fill's `po` / `file_obj` clones **outside** `SCHED`.
            // SAFETY: `reg` live, `SCHED` held.
            let pf = unsafe { UserspaceServerReg::take_pending_fill_matching(reg, request_id) };
            drop(g);
            drop(pf);
            if outcome == SendOutcome::Full {
                ForwardOutcome::Full
            } else {
                ForwardOutcome::PeerClosed
            }
        }
    }
}

/// Take the forwarded **fill** outstanding on `reg` whose `request_id` matches a
/// `File::ReadRange` reply's, for the caller to land (copy the bytes into the
/// frame, mark the page ready, complete the PO). `None` if it does not correlate
/// (duplicate / stale). Takes `SCHED`; the returned [`PendingFill`]'s `po` /
/// `file_obj` are dropped by the caller **outside** `SCHED`. The caller pins `reg`
/// (via the send's endpoint peer, valid for the call).
pub fn us_forward_take_pending_fill(reg: *mut (), request_id: u64) -> Option<PendingFill> {
    let _g = SCHED.lock();
    // SAFETY: `reg` is a live registration (pinned through the live peer endpoint);
    // `SCHED` held.
    unsafe { UserspaceServerReg::take_pending_fill_matching(reg, request_id) }
}

/// If a send on `send_endpoint` targets the kernel's end of a Userspace Server
/// channel — i.e. its **peer** carries a
/// [`UserspaceServerReg`](crate::object::UserspaceServerReg) back-pointer — return
/// that registration (the send is a forwarded-lookup *reply* the kernel completes
/// inline). `None` for an ordinary channel send or a closed peer. Takes `SCHED`
/// (the accessor contract). The caller pins `send_endpoint` (its send handle).
pub fn us_forward_reg_for_send(send_endpoint: *mut ()) -> Option<*mut ()> {
    let _g = SCHED.lock();
    // SAFETY: `send_endpoint` is pinned by the caller; `SCHED` held.
    let peer = unsafe { IpcChannel::peer_of(send_endpoint) };
    if peer.is_null() {
        return None;
    }
    // SAFETY: `peer` is the surviving endpoint (the kernel end); `SCHED` held.
    let reg = unsafe { IpcChannel::us_reg_of(peer) };
    if reg.is_null() { None } else { Some(reg) }
}

/// Detach a dying [`UserspaceServerReg`] from its endpoint, completing anything
/// still outstanding on it. Called from that type's `Drop`, before its fields drop.
///
/// Two jobs, both of which must happen while the registration is still intact:
///
/// 1. **Complete outstanding forwarded lookups** `PeerClosed`, so their waiters wake
///    rather than hang. The endpoint is still alive here, so the wake is well-formed.
/// 2. **Clear the endpoint's back-pointer to us.** `IpcChannel.us_reg` is raw and
///    uncounted (a counted edge would cycle with the registration's ownership of the
///    endpoint), and until this existed nothing ever cleared it — so an endpoint that
///    outlived its registration left every consumer of that pointer reading freed
///    memory. See the `Drop` impl for the full list.
///
/// Takes `SCHED`, which is sound for the same reason `ipc_endpoint_closing` may:
/// registration references are released only outside `SCHED`.
pub fn us_reg_detach(reg: *mut ()) {
    let mut g = SCHED.lock();
    // SAFETY: called from `Drop` with the last reference gone but the object whole;
    // `SCHED` held, satisfying the accessor contract.
    let endpoint = unsafe { UserspaceServerReg::endpoint_ptr(reg) };
    // Bounded by the pending table's size, so the drain cannot overrun this.
    let mut orphans: [Option<ObjectRef>; US_PENDING_MAX] = core::array::from_fn(|_| None);
    let mut n_orphans = 0usize;
    // SAFETY: as above; `SCHED` held.
    while let Some(pl) = unsafe { UserspaceServerReg::take_pending_next(reg) } {
        signal_pending_op(
            &mut g,
            pl.po.as_ptr(),
            crate::syscall::error::KError::PeerClosed as i32,
        );
        orphans[n_orphans] = Some(pl.po);
        n_orphans += 1;
    }
    if !endpoint.is_null() {
        // Clear only a pointer that still names *this* registration. `us_server_attach`
        // can retarget an endpoint at a different one, and stomping a live successor's
        // pointer would silently break its reply routing.
        // SAFETY: `endpoint` is pinned by the reference this registration still holds
        // (released by the field drops that follow this call); `SCHED` held.
        if unsafe { IpcChannel::us_reg_of(endpoint) } == reg {
            // SAFETY: as above.
            unsafe { IpcChannel::set_us_reg(endpoint, core::ptr::null_mut()) };
        }
    }
    // Release `SCHED` before dropping the orphaned references: an `ObjectRef` drop can
    // reach the allocator, which must not happen under the rank-1 lock.
    drop(g);
    drop(orphans);
}

/// Record the endpoint → registration back-pointer that makes `endpoint` the
/// kernel's end of a Userspace Server channel: a reply sent to it (by the server,
/// on its peer) is then completed inline rather than enqueued. Called by
/// `sys_ns_bind` once the `UserspaceServer` binding owns the registration. Takes
/// `SCHED` (the accessor contract). The caller pins `endpoint` (its bound handle).
pub fn us_server_attach(endpoint: *mut (), reg: *mut ()) {
    let _g = SCHED.lock();
    // SAFETY: `endpoint` is a live `IpcChannel` pinned by the caller; `SCHED` held.
    unsafe { IpcChannel::set_us_reg(endpoint, reg) };
}

/// If `endpoint` already backs a Userspace Server registration, return a **new owned
/// reference** to it (bump-and-adopt) so an additional namespace binding can share
/// the one connection — *bind-mount* semantics: one server endpoint, many names,
/// each with its own subtree base + rights on the binding. `None` if the endpoint is
/// not yet registered (the caller mints a fresh registration). Reusing the existing
/// registration is what keeps reply routing correct: the endpoint→reg back-pointer
/// stays a single, consistent target rather than being clobbered by a rival reg.
/// Takes `SCHED`; the caller pins `endpoint` (its bound handle).
pub fn us_forward_existing_reg(endpoint: *mut ()) -> Option<ObjectRef> {
    let _g = SCHED.lock();
    // SAFETY: `endpoint` is a live `IpcChannel` pinned by the caller; `SCHED` held.
    let reg = unsafe { IpcChannel::us_reg_of(endpoint) };
    if reg.is_null() {
        return None;
    }
    // Acquire rather than `bump`: the old code asserted "the existing binding still
    // holds a reference (refcount >= 1)" and incremented unconditionally, which
    // resurrects an object whose count has already reached zero — handing out an owned
    // reference to memory that is being destroyed. `try_acquire` refuses at zero, which
    // is the `Arc::upgrade` semantics this lookup actually needs. (`us_reg_detach` now
    // clears the back-pointer at teardown, so reaching a dead registration here should
    // be impossible; this makes the race unrepresentable rather than merely unlikely.)
    // SAFETY: `reg` addresses a `UserspaceServerReg` whose header is at offset 0;
    // `SCHED` held.
    let header = unsafe { &*(reg as *const crate::object::header::KObjectHeader) };
    if !header.try_acquire() {
        return None;
    }
    // SAFETY: the successful `try_acquire` above balances this adoption.
    Some(unsafe { ObjectRef::from_raw(reg, KObjectType::UserspaceServerReg) })
}

/// Take the forwarded lookup outstanding on `reg` whose `request_id` matches a
/// reply's, for the caller to complete (cross-context install + signal). `None`
/// if the reply does not correlate (duplicate / stale). Takes `SCHED`; the
/// returned [`PendingLookup`]'s `po` is dropped by the caller **outside** `SCHED`.
/// The caller pins `reg` (via the send's endpoint peer, valid for the call).
pub fn us_forward_take_pending(reg: *mut (), request_id: u64) -> Option<PendingLookup> {
    let _g = SCHED.lock();
    // SAFETY: `reg` is a live registration (pinned through the live peer endpoint);
    // `SCHED` held.
    unsafe { UserspaceServerReg::take_pending_matching(reg, request_id) }
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
    // A Userspace Server channel is losing its server: fail any forwarded lookup
    // pending on the kernel-forwarding endpoint with `PeerClosed`. Two cases —
    // **this** endpoint is the kernel end (its registration is being torn down),
    // or its **peer** is (the server process just died). Each taken PO is signalled
    // here; its reference is dropped **outside** `SCHED` (below).
    // A registration's endpoint is shared by many bindings (bind-mount semantics),
    // so it can hold up to `US_PENDING_MAX` forwarded lookups in flight — drain *all*
    // of them, failing each `PeerClosed`. Each taken PO is signalled here and
    // collected into `orphans`; the references are dropped **outside** `SCHED` below.
    let mut orphans: [Option<ObjectRef>; 2 * US_PENDING_MAX] = core::array::from_fn(|_| None);
    let mut n_orphans = 0usize;
    // SAFETY: `endpoint` is valid memory; `SCHED` held.
    let reg_self = unsafe { IpcChannel::us_reg_of(endpoint) };
    // `us_reg` is a raw back-pointer that **nothing clears**, so non-null does not
    // mean live. In the ordinary teardown the registration owns this endpoint and is
    // mid-drop right now — its `Inner` declares `endpoint` before `pending`, so the
    // endpoint's `Drop` lands here with the pending table still intact, which is what
    // makes the drain below correct. But when anything else holds a reference to this
    // endpoint (a server handle, a blocked waiter), the registration is freed *first*
    // and this pointer dangles; draining it then reads a recycled allocation and hands
    // `signal_pending_op` a `po` built from whatever now occupies that memory —
    // observed as the kernel-stack paint poison, log text, and x86 instruction bytes,
    // crashing with `#GP`/`#PF` about one boot in four under load.
    //
    // The `magic` sentinel was written for exactly this and had never been read.
    // A stale registration fails it, and skipping the drain is the correct response:
    // that registration's `pending` array was dropped with it, so its lookups have
    // already been released — there is nothing left here to complete.
    // SAFETY: non-null and, being a kernel-heap allocation, mapped and readable.
    if !reg_self.is_null() && unsafe { UserspaceServerReg::is_live(reg_self) } {
        // SAFETY: `reg_self` is this endpoint's owning registration (the endpoint
        // is being dropped *because* the registration is — `reg_self` is still
        // valid memory mid-drop); `SCHED` held.
        while let Some(pl) = unsafe { UserspaceServerReg::take_pending_next(reg_self) } {
            signal_pending_op(&mut g, pl.po.as_ptr(), crate::syscall::error::KError::PeerClosed as i32);
            orphans[n_orphans] = Some(pl.po);
            n_orphans += 1;
        }
    }
    if !peer.is_null() {
        // SAFETY: `peer` is the surviving endpoint, kept alive by its own handle
        // / a waiter's `ObjectRef`; `SCHED` held.
        unsafe { IpcChannel::clear_peer(peer) };
        signal_ipc_endpoint(&mut g, peer);
        // SAFETY: `peer` is the live surviving endpoint; `SCHED` held.
        let reg_peer = unsafe { IpcChannel::us_reg_of(peer) };
        // Same unchecked back-pointer as `reg_self` above: `peer` being live says
        // nothing about *its* registration still existing, since nothing clears the
        // pointer when a registration is destroyed.
        // SAFETY: non-null and, being a kernel-heap allocation, mapped and readable.
        if !reg_peer.is_null() && unsafe { UserspaceServerReg::is_live(reg_peer) } {
            // SAFETY: `reg_peer` is pinned by the live `peer` (its owned endpoint);
            // `SCHED` held.
            while let Some(pl) = unsafe { UserspaceServerReg::take_pending_next(reg_peer) } {
                signal_pending_op(&mut g, pl.po.as_ptr(), crate::syscall::error::KError::PeerClosed as i32);
                orphans[n_orphans] = Some(pl.po);
                n_orphans += 1;
            }
        }
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
    // Release `SCHED` before dropping any orphaned forwarded-lookup PO references
    // (an `ObjectRef` Drop must not run under the rank-1 lock).
    drop(g);
    drop(orphans);
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
    // The same idea for the *other* per-CPU invariant, which the lock check cannot see.
    // A plain-lock holder trips `assert_switch_safe` because it holds a rank; a bare
    // `preempt_disable()` — the `tlb`, `handle::global` and `reap_pending` windows — holds
    // nothing, so a yield or block inside one would switch away from a CPU that has asked
    // not to be descheduled, and leave the counter raised on a thread no longer running.
    //
    // The involuntary paths already refuse: `on_timer_tick` and `resched_if_idle_locked`
    // latch into `RESCHED_PENDING` instead of switching. Nothing checked the **voluntary**
    // ones — `yield_now` and everything that blocks — which is the gap this closes (kernel
    // audit B.5(c), decision log 2026-08-14). No reachable violation exists today; this is
    // what keeps that true.
    debug_assert_eq!(
        PREEMPT_OFF[SchedState::this_cpu()].load(Ordering::Relaxed),
        0,
        "context switch with preemption disabled — a voluntary switch inside a \
         preempt-critical region deschedules a CPU that asked not to be, and strands the \
         raised counter on a thread that is no longer running"
    );
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
    let next = match pick_next(&mut g) {
        Some(n) => n,
        None => g.idle_slot().take().expect("idle thread exists after init"),
    };
    let prev = g.cur_slot().take().expect("current set");
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
    push_reserved(&mut g.blocked, prev, "blocked list");
    *g.cur_slot() = Some(next);

    // Switch into `next`; we resume here (lock-free) when a waker moves us back
    // to `ready` and the scheduler later picks us.
    // SAFETY: `prev_slot` is the outgoing (now-`Blocked`, pinned-in-`blocked`)
    // thread's saved-SP slot; `prev_obj`/`next_obj` are the pinned outgoing /
    // incoming threads.
    unsafe { switch_into(g, prev_slot, prev_obj, next_obj) };
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
    // Re-home onto its CPU (cache-warm; a full home falls back to the
    // least-loaded permitted queue with room — F6). This fails only when
    // *every* permitted queue is at reserve — e.g. a thread pinned to a single
    // CPU whose queue is full — which is genuine reserve exhaustion, still fatal.
    if let Err(returned) = place_thread(g, r, true) {
        // **Do not let `returned` drop here.** `SCHED` is held, and an `ObjectRef` drop can
        // reach `dispatch_destroy` → `SlabCache::free` — a plain spinlock under the rank-1
        // lock, which is F2 (`kernel/docs/lock-ordering.md`). The two spawn callers avoid it
        // by carrying the ref out of the locked block with a `match`; here there is nowhere
        // to carry it *to*, because the next statement ends the machine.
        //
        // `if place_thread(..).is_err()` used to do exactly the forbidden thing: an `if`
        // condition is its own temporary scope, so the `Result` — and the `ObjectRef` inside
        // it — dropped at the end of the condition, *before* the panic, with `SCHED` still
        // held. An earlier version of this comment asserted the opposite and credited
        // `panic = "abort"` with saving it; both were wrong (PR #204 review, blocking 1).
        // The consequence was not theoretical: reserve exhaustion is the state this panic
        // exists for, and the drop would spin on the allocator lock while another CPU's tick
        // spins on `SCHED` with IRQs masked — a hang instead of the diagnostic.
        //
        // Leaked deliberately: `panic = "abort"` means nothing outlives this call, so a
        // `forget` costs nothing and is honest about it. `deferred_drops` would be the
        // documented alternative but is worse here — pushing to it can itself grow under
        // `SCHED` (audit C.1(c)) and nothing will ever drain it.
        core::mem::forget(returned);
        panic!(
            "wake placement: no ready queue would take the thread — every affinity-permitted \
             queue is at reserve; or affinity permits no CPU that accepts work and every CPU \
             that does is at reserve; or no CPU accepts work and this one is at reserve"
        );
    }
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
        me_ptr = g.cur_ref().as_ref().expect("current set when a thread runs").as_ptr();

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

/// `true` if any thread belonging to process `my_pid` is still live anywhere the
/// scheduler tracks: a per-CPU `ready` queue, `blocked`, `suspended`, **or running
/// as another CPU's `current`**. Caller holds `SCHED`. The exiting thread has been
/// taken out of its own CPU's `current`; under SMP a sibling may be `current` on
/// another CPU, so the `current[]` scan is required (else a `false` here would
/// wrongly declare the caller the last live thread and tear the process down out
/// from under a running sibling).
fn has_live_siblings(g: &SchedState, my_pid: u32) -> bool {
    // SAFETY: each entry pins a live Thread; `SCHED` held — a shared read of
    // `owner_pid` is sound (no `&mut` taken).
    let same = |r: &ObjectRef| unsafe { &*(r.as_ptr() as *const Thread) }.owner_pid() == my_pid;
    g.ready.iter().any(|q| q.iter().any(same))
        || g.blocked.iter().any(same)
        || g.suspended.iter().any(same)
        || g.current.iter().flatten().any(same)
}

/// Reap every thread of process `my_pid` parked in `list` (a `ready` or
/// `suspended` queue — neither registers on wait objects): mark each `Exited`
/// and move its `ObjectRef` into `reap`. Caller holds `SCHED`.
fn reap_matching(list: &mut KVec<ObjectRef>, reap: &mut KVec<ObjectRef>, my_pid: u32) {
    // SAFETY (loop): each entry pins a live Thread; `SCHED` held.
    while let Some(i) = list.iter().position(|r| {
        // SAFETY: `r` pins a live Thread; `SCHED` held.
        let th = unsafe { &*(r.as_ptr() as *const Thread) };
        // Never reap a per-CPU **idle** thread: it is scheduler infrastructure, not
        // a sibling of the exiting process, and it may be *running right now* on
        // another CPU (or this one) — reclaiming it frees a live kernel stack (a
        // use-after-free `#DF`). Idle threads carry `owner_pid()==0`, so a `pid==0`
        // exit would otherwise sweep them all up.
        th.owner_pid() == my_pid && th.tid() != IDLE_TID
    }) {
        // Wait out a mid-switch-out guard before reclaiming: the sweep can see
        // a sibling parked on another CPU's queue whose switch-out has not yet
        // committed — that CPU is still executing on the sibling's kernel stack
        // for a few more instructions, and queueing it for the stack free here
        // would be a use-after-free (F5, decision log 2026-07-21). Bounded: the
        // owning CPU clears the guard from post-release straight-line code, no
        // lock needed.
        // SAFETY: the entry pins a live Thread; `SCHED` held.
        while unsafe { Thread::is_on_cpu(list[i].as_ptr()) } {
            core::hint::spin_loop();
        }
        let r = list.remove(i);
        // SAFETY: `r` pins the thread; `SCHED` held.
        unsafe { Thread::set_state(r.as_ptr(), ThreadState::Exited) };
        push_reserved(reap, r, "reap list");
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
    while let Some(i) = blocked.iter().position(|r| {
        // SAFETY: `r` pins a live Thread; `SCHED` held.
        let th = unsafe { &*(r.as_ptr() as *const Thread) };
        // Never reap an idle thread (see `reap_matching`).
        th.owner_pid() == my_pid && th.tid() != IDLE_TID
    }) {
        // Wait out a mid-switch-out guard before reclaiming (see
        // [`reap_matching`] — a just-blocked sibling's switch-out may not have
        // committed; its CPU is still on the sibling's stack).
        // SAFETY: the entry pins a live Thread; `SCHED` held.
        while unsafe { Thread::is_on_cpu(blocked[i].as_ptr()) } {
            core::hint::spin_loop();
        }
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
        push_reserved(reap, r, "reap list");
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
    push_reserved(g.reap_slot(), me, "reap list (self)");

    // Switch to the next ready thread, else the idle thread (which always
    // exists post-init and is parked here, since `me` was current, not idle).
    let next = match pick_next(&mut g) {
        Some(n) => n,
        None => g.idle_slot().take().expect("idle thread exists after init"),
    };
    let next_obj = next.as_ptr();
    // SAFETY: next is pinned, becoming current; lock held.
    unsafe { Thread::set_state(next_obj, ThreadState::Running) };
    *g.cur_slot() = Some(next);

    // Switch away forever. `switch_into` loads the incoming root before the
    // stack swap, so when the last user thread exits CR3 is restored to the
    // boot root before this (parked-in-`reap`) thread is reaped — its
    // `AddressSpace::Drop` frees the PML4 CR3 would otherwise still reference.
    // SAFETY: `me_slot` is our own (now-`Exited`, pinned-in-`reap`) saved-SP
    // slot — written by the switch and never read again; `me_obj`/`next_obj` is the
    // pinned incoming thread. We never resume, so the restore inside
    // `switch_into` is never reached.
    unsafe { switch_into(g, me_slot, me_obj, next_obj) };
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
    let me = g.cur_slot().take().expect("current set");
    let me_obj = me.as_ptr();

    // SAFETY: `me` is the running thread, pinned, lock held.
    let me_pid = unsafe { &*(me_obj as *const Thread) }.owner_pid();
    // SAFETY: same.
    let has_process = unsafe { &*(me_obj as *const Thread) }.process_ref().is_some();
    if has_process && !has_live_siblings(&g, me_pid) {
        // Last thread out: the process is ending, so its handles must be closed.
        // Deferred to the reaper for the same reason as in `exit_process`.
        // SAFETY: `me_obj` is the running thread, pinned, lock held.
        unsafe { Thread::set_process_ended(me_obj) };
        deliver_child_exited(&mut g, me_obj, status);
    }

    finish_exit(g, me);
}

/// Terminate the **whole process** of the current thread with exit `status`:
/// tear down every sibling thread (scan `ready`/`blocked`/`suspended` by
/// `owner_pid`, unregistering blocked siblings from their waits first), then
/// exit the current thread **with** a `ChildExited`. Used by `sys_process_exit`.
///
/// The per-process thread list that would let an external killer find these threads
/// without a scan lands later; the `owner_pid` scan reaps siblings parked on any CPU's
/// `ready` queue, `suspended`, or `blocked`. **SMP gap (deferred):** a sibling that is
/// *running* as another CPU's `current` is not reaped here — terminating it needs a
/// cross-CPU deschedule IPI (the same machinery as TLB shootdown, slice 3b). Not
/// triggered by today's workloads (multi-threaded processes don't `sys_process_exit`
/// with a sibling live on another CPU). A kernel/boot thread (no process) degrades to a
/// plain [`exit_thread`]-style exit (no siblings, no notification).
pub fn exit_process(status: ExitStatus) -> ! {
    // Reclaim any earlier exited thread first, so `reap` has room.
    reap_pending();

    let mut g = SCHED.lock();
    let me = g.cur_slot().take().expect("current set");
    let me_obj = me.as_ptr();

    // SAFETY: `me` is the running thread, pinned, lock held.
    let me_pid = unsafe { &*(me_obj as *const Thread) }.owner_pid();
    // SAFETY: same.
    let has_process = unsafe { &*(me_obj as *const Thread) }.process_ref().is_some();
    if has_process {
        // Reborrow the guard once as `&mut SchedState` so the field borrows
        // below are disjoint (through the guard's `Deref` each `&mut g.field`
        // would borrow the whole guard, conflicting in one call).
        let cpu = SchedState::this_cpu();
        let st: &mut SchedState = &mut g;
        // Torn-down siblings are parked (off-CPU) on *some* CPU's ready queue, in
        // `suspended`, or `blocked`; reaping them into this CPU's reap list is safe
        // (they are not on any CPU's stack). Sweep every per-CPU ready queue. The
        // `reap[cpu]` index is precomputed for disjoint field borrows.
        for c in 0..MAX_CPUS {
            reap_matching(&mut st.ready[c], &mut st.reap[cpu], me_pid);
        }
        reap_matching(&mut st.suspended, &mut st.reap[cpu], me_pid);
        reap_blocked_matching(&mut st.blocked, &mut st.deadlines, &mut st.reap[cpu], me_pid);
        // The process is ending: always deliver ChildExited (we are now its
        // last thread), and mark this thread so the reaper closes the process's
        // handles. The sweep cannot happen here — it drops object references
        // (destructors reach the rank-6 allocator, and an IPC endpoint's takes
        // rank-1 `SCHED` to wake blocked receivers), and we hold `SCHED` and never
        // return from `finish_exit`.
        // SAFETY: `me_obj` is the running thread, pinned, lock held.
        unsafe { Thread::set_process_ended(me_obj) };
        // Hand the handle table to the reaper. This is the half that peers are waiting
        // on: closing an IPC endpoint is what nulls its peer and wakes anyone blocked
        // there, so leaving it to "whenever some CPU next reaches idle" makes a blocked
        // reader's liveness depend on unrelated scheduling. `set_process_ended` above
        // still marks the thread, so the per-CPU sweep remains a correct fallback if the
        // queue is full.
        if st.ended_pids.len() < st.ended_pids.capacity() {
            let _ = st.ended_pids.push_within_capacity(me_pid);
            wake_reaper(st);
        }
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
        let me = g.cur_slot().take().expect("current set");
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
        push_reserved(&mut g.suspended, me, "suspended list");

        // Switch to the next runnable thread (mirrors block_current_and_switch).
        // If nothing is runnable, this fault **stranded the scheduler**: no thread
        // remained to receive the notification and `sys_exception_resume` us, so the
        // system would idle forever (a silent hang — notably an init/pid-1 crash).
        // Emit a last-ditch diagnostic. A serviced fault (the supervisor's waiter
        // was just woken by `signal_channel` above) takes the `Some` branch and
        // stays quiet — so this fires only for genuinely-unhandled faults.
        let next = match pick_next(&mut g) {
            Some(n) => n,
            None => {
                report_stranded_fault(me_obj, &notif);
                g.idle_slot().take().expect("idle thread exists after init")
            }
        };
        let next_obj = next.as_ptr();
        // SAFETY: next is pinned, becoming current; lock held.
        unsafe { Thread::set_state(next_obj, ThreadState::Running) };
        *g.cur_slot() = Some(next);

        // Switch into `next`; we resume here when `sys_exception_resume` moves
        // us `suspended`→`ready` and the scheduler switches us in.
        // SAFETY: `me_slot` is our own (now-`Suspended`, pinned-in-`suspended`)
        // saved-SP slot; `me_obj`/`next_obj` are the pinned outgoing / incoming
        // threads.
        unsafe { switch_into(g, me_slot, me_obj, next_obj) };
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
    // Re-home on its CPU (a wake; a full home falls back to a permitted queue
    // with room — F6). Fails only when every permitted queue is at reserve.
    if let Err(returned) = place_thread(&mut g, r, true) {
        // **Do not let `returned` drop here.** `SCHED` is held, and an `ObjectRef` drop can
        // reach `dispatch_destroy` → `SlabCache::free` — a plain spinlock under the rank-1
        // lock, which is F2 (`kernel/docs/lock-ordering.md`). The two spawn callers avoid it
        // by carrying the ref out of the locked block with a `match`; here there is nowhere
        // to carry it *to*, because the next statement ends the machine.
        //
        // `if place_thread(..).is_err()` used to do exactly the forbidden thing: an `if`
        // condition is its own temporary scope, so the `Result` — and the `ObjectRef` inside
        // it — dropped at the end of the condition, *before* the panic, with `SCHED` still
        // held. An earlier version of this comment asserted the opposite and credited
        // `panic = "abort"` with saving it; both were wrong (PR #204 review, blocking 1).
        // The consequence was not theoretical: reserve exhaustion is the state this panic
        // exists for, and the drop would spin on the allocator lock while another CPU's tick
        // spins on `SCHED` with IRQs masked — a hang instead of the diagnostic.
        //
        // Leaked deliberately: `panic = "abort"` means nothing outlives this call, so a
        // `forget` costs nothing and is honest about it. `deferred_drops` would be the
        // documented alternative but is worse here — pushing to it can itself grow under
        // `SCHED` (audit C.1(c)) and nothing will ever drain it.
        core::mem::forget(returned);
        panic!(
            "resume placement: no ready queue would take the thread — every affinity-permitted \
             queue is at reserve; or affinity permits no CPU that accepts work and every CPU \
             that does is at reserve; or no CPU accepts work and this one is at reserve"
        );
    }
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

/// Pop up to `buf.len()` parked refs — this CPU's `reap` list first, then the
/// global [`SchedState::deferred_drops`] — into `buf`. **Moves only**: no drop,
/// no allocation, and the lists keep their reserved buffers. (The previous
/// `mem::take` drain swapped in a zero-capacity `KVec`, so every later exit-path
/// push *allocated under `SCHED`* via `KVec::try_push` growth — the F11 deadlock
/// hazard, decision log 2026-07-21.) Returns the count. Caller holds `SCHED` and
/// drops the moved refs after releasing it, in thread context.
fn drain_pending_drops(g: &mut SchedState, buf: &mut [Option<ObjectRef>]) -> usize {
    let mut n = 0;
    while n < buf.len() {
        let Some(r) = g.reap_slot().pop() else { break };
        buf[n] = Some(r);
        n += 1;
    }
    while n < buf.len() {
        let Some(r) = g.deferred_drops.pop() else { break };
        buf[n] = Some(r);
        n += 1;
    }
    n
}

/// Drop every pending reaped thread — and every deferred-drop ref — **outside**
/// the run-queue lock, in thread context (a reaped thread's `KernelStack` `Drop`
/// takes rank-6 allocator locks and initiates a TLB shootdown). Idempotent;
/// called at the top of [`yield_now`]/[`exit_thread`]/[`exit_process`]/
/// [`suspend_with_fault`], by the idle loop, and by the boot drainer. Only this
/// CPU's reap list is drained — a thread is reclaimed by the CPU it died on,
/// after its `switch_into` completed (so its stack is no longer in use) — plus
/// the CPU-agnostic [`SchedState::deferred_drops`]. Draining moves refs into a
/// fixed local buffer under a brief hold ([`drain_pending_drops`] — preserving
/// the lists' reserved capacity), then drops them with the lock released.
pub fn reap_pending() {
    // Completed block IRPs, whose boxes their DPC deliberately did not free. Same rule as
    // the `deferred_drops` below and the same reason: a free reaches the plain-spinlock
    // allocator, which must not happen in IRQ or DPC context. Done first so the frees
    // happen even if there is nothing else to reap.
    crate::io::block::reclaim_completed();
    // The char drivers park their completed reads for the same reason and need the same
    // thread-context drop (decision log, 2026-08-06).
    crate::drivers::console::reclaim_completed();
    crate::drivers::ps2::reclaim_completed();
    loop {
        let mut buf: [Option<ObjectRef>; REAP_RESERVE + DEFERRED_DROP_RESERVE] =
            [const { None }; REAP_RESERVE + DEFERRED_DROP_RESERVE];
        let n = {
            let mut g = SCHED.lock();
            drain_pending_drops(&mut g, &mut buf)
        };
        // Close the handles of any process whose last thread is in this batch —
        // **before** the drops below, so its pipes close (waking peers) as promptly
        // as the reap runs. This is the sweep's home: thread context, no `SCHED`
        // held, not IRQ context. The exiting thread could not do it itself; it
        // holds `SCHED` and never returns from `finish_exit`.
        for slot in buf[..n].iter() {
            let Some(r) = slot else { continue };
            if r.object_type() != KObjectType::Thread {
                continue;
            }
            // SAFETY: the `ObjectRef` pins this `Thread`; nothing else mutates a
            // reaped thread's fields.
            let (ended, pid) = unsafe {
                let t = &*(r.as_ptr() as *const Thread);
                (Thread::process_ended(r.as_ptr()), t.owner_pid())
            };
            // `pid == 0` is a kernel/idle thread, which owns no handles and whose
            // pid must never be swept.
            if ended && pid != 0 {
                crate::handle::global::close_all_owned_by(pid);
            }
        }

        // Drop with preemption disabled: the drops take the allocator locks and
        // (for a kernel stack) run a TLB shootdown — plain locks other CPUs may
        // spin on. Being descheduled while holding one starves the spinners for
        // a scheduling round — or forever, when the reaper is the **idle**
        // thread, which is never re-picked while the spinners keep every CPU
        // busy (F12, decision log 2026-07-21). Interrupts stay enabled; only
        // the switch is deferred.
        preempt_disable();
        for slot in buf[..n].iter_mut() {
            drop(slot.take());
        }
        preempt_enable();
        if n < buf.len() {
            return; // drained everything that was parked when we looked
        }
        // Filled the buffer (defensive; both lists stay within their reserves,
        // which the buffer covers exactly) — go around for any remainder.
    }
}

/// `true` when no thread other than the current one is ready to run.
pub fn ready_is_empty() -> bool {
    // Globally empty: no CPU has a runnable thread queued. Used by the boot drainers.
    SCHED.lock().ready.iter().all(|q| q.is_empty())
}

/// `NICE_0_WEIGHT` and the Linux-style nice→weight table (nice `-20..=19`).
/// TimeShared vruntime accrues as `slice * NICE_0_WEIGHT / weight(nice)`, so a
/// lower (heavier-weighted) nice accrues *slower* and is picked more often.
const NICE_0_WEIGHT: u64 = 1024;
#[rustfmt::skip]
const NICE_WEIGHTS: [u64; 40] = [
    /* -20..-16 */ 88761, 71755, 56483, 46273, 36291,
    /* -15..-11 */ 29154, 23254, 18705, 14949, 11916,
    /* -10..-6  */  9548,  7620,  6100,  4904,  3906,
    /*  -5..-1  */  3121,  2501,  1991,  1586,  1277,
    /*   0..4   */  1024,   820,   655,   526,   423,
    /*   5..9   */   335,   272,   215,   172,   137,
    /*  10..14  */   110,    87,    70,    56,    45,
    /*  15..19  */    36,    29,    23,    18,    15,
];

/// The nice→weight table entry for `nice` (clamped to `-20..=19`).
#[inline]
fn nice_weight(nice: i8) -> u64 {
    NICE_WEIGHTS[(nice as i32 + 20).clamp(0, 39) as usize]
}

/// The per-tick virtual-runtime increment for a TimeShared thread with `nice`.
#[inline]
fn vruntime_delta(nice: i8) -> u64 {
    TICK_NS * NICE_0_WEIGHT / nice_weight(nice)
}

/// The scheduler's pick key for a runnable thread — **lower sorts first**:
/// `(class_rank, rt_priority, vruntime)`. RealTime (rank 0, by priority) precedes
/// TimeShared (rank 1, by vruntime); Idle (rank 2) never appears in `ready`.
///
/// # Safety
/// `obj` is a live, pinned `Thread`; the caller holds `SCHED`.
unsafe fn sched_key(obj: *mut ()) -> (u8, u8, u64) {
    match unsafe { Thread::sched_class(obj) } {
        SchedClass::RealTime => (0, unsafe { Thread::rt_priority(obj) }, 0),
        SchedClass::TimeShared => (1, 0, unsafe { Thread::vruntime(obj) }),
        SchedClass::Idle => (2, 0, u64::MAX),
    }
}

/// Pick the next thread by **scheduling-class precedence** and remove it from
/// `ready`, or `None` if empty. RealTime (lowest `rt_priority`, FIFO within a
/// priority) beats TimeShared (smallest `vruntime`). An O(n) scan — ample at our
/// thread counts; per-class heaps/buckets are a later optimization
/// (`docs/architecture/scheduler.md`). Caller holds the run-queue lock.
fn dequeue_front(g: &mut SchedState) -> Option<ObjectRef> {
    let cpu = SchedState::this_cpu();
    let n = g.ready[cpu].len();
    if n == 0 {
        return None;
    }
    let mut best_i = 0usize;
    // SAFETY: each `ready` entry pins a live Thread; `SCHED` held.
    let mut best_key = unsafe { sched_key(g.ready[cpu][0].as_ptr()) };
    for i in 1..n {
        // SAFETY: as above. `<` (strict) keeps the earliest entry on ties (FIFO).
        let key = unsafe { sched_key(g.ready[cpu][i].as_ptr()) };
        if key < best_key {
            best_i = i;
            best_key = key;
        }
    }
    // Advance this CPU's monotonic TimeShared floor to the picked thread (the
    // smallest vruntime on this queue), so `min_vruntime[cpu]` tracks its leftmost.
    if best_key.0 == 1 {
        g.min_vruntime[cpu] = g.min_vruntime[cpu].max(best_key.2);
    }
    Some(g.ready[cpu].remove(best_i))
}

/// Whether `cpu` may be **given** work: the scheduler has it online *and* it has not
/// parked forever.
///
/// **Both, because the two disagree after a park.** [`leave_online`] clears the lock-free
/// mask but cannot clear `cpu_online[]` — that needs `SCHED`, which the parking CPU may
/// already hold — so a CPU that halted in `dump_and_halt` on a ring-0 fault stays `true`
/// there while the rest of the machine runs on. Placing on it is not a slowdown but a hang:
/// nothing on that queue will ever be picked, and the thread waits forever on a machine
/// that otherwise looks healthy.
///
/// **Deliberately not applied to the *victim* side of work-stealing.** `steal_one` and
/// `steal_available` ask which CPU to take work *from*, and a parked CPU is exactly the one
/// you want drained — gating them on this would strand the threads already queued there,
/// which is the opposite of the fix. The asymmetry is the point; do not "make it
/// consistent".
/// **`online` is passed in, not read here**, so the decision is a function of its inputs.
/// Reading the global made every placement test share process-wide mutable state with every
/// other, and `cargo test` runs them concurrently: on a 16-core host that was a 22 % failure
/// rate across `sched::tests`, which CI happened not to hit (PR #199 review, blocking 1).
/// Callers read [`online_mask`] once per placement decision, which also keeps a single scan
/// from seeing two different online sets.
fn cpu_accepts_work(g: &SchedState, cpu: usize, online: u64) -> bool {
    g.cpu_online[cpu] && online & (1u64 << cpu) != 0
}

/// The least-loaded online CPU (fewest `ready` entries) that `obj`'s affinity
/// permits — the placement target for a new thread.
///
/// **When affinity permits no CPU that accepts work, it places on one that does anyway**,
/// outside the affinity mask; see the fallback for why that beats both the alternatives.
/// That case is reachable two ways, and an earlier version of this comment denied the
/// second: a permitted CPU can *park* after the fact, and `sys_thread_set_affinity` does
/// **not** reject an affinity naming no online CPU — it validates only that the mask is
/// non-empty within `MAX_CPUS`. (The kernel audit refuted the claim that it did — evidence
/// in the decision log, 2026-08-14.)
///
/// Caller holds `SCHED`.
fn pick_target_cpu(g: &SchedState, obj: *mut (), online: u64) -> usize {
    // SAFETY: `obj` is a pinned Thread; `SCHED` held.
    let mask = unsafe { Thread::cpu_mask(obj) };
    let mut best = usize::MAX;
    let mut best_len = usize::MAX;
    // The same least-loaded scan, ignoring affinity — the fallback below wants a CPU that
    // can *run* the thread, and "least loaded" is as much a part of that as "accepts work":
    // a queue at `READY_RESERVE` makes `place_thread` return `Err`, which both wake callers
    // turn into a `panic!`. Tracked in the same pass so the fallback is not load-blind
    // (PR #201 review, finding 2); one loop rather than two, since every candidate is
    // already being visited.
    let mut spare = usize::MAX;
    let mut spare_len = usize::MAX;
    for c in 0..MAX_CPUS {
        if !cpu_accepts_work(g, c, online) {
            continue;
        }
        let len = g.ready[c].len();
        if len < spare_len {
            spare_len = len;
            spare = c;
        }
        if mask & (1 << c) == 0 {
            continue;
        }
        if len < best_len {
            best_len = len;
            best = c;
        }
    }
    if best != usize::MAX {
        return best;
    }
    // **Affinity permits no CPU that accepts work.** Prefer *runnable* over
    // *affinity-correct*: a thread placed where nothing will ever schedule it hangs
    // forever, while a thread running outside its affinity is a policy violation it
    // survives — and `dequeue_front` applies no affinity filter anyway, so that
    // distinction is already best-effort at the other end.
    //
    // **Falling back to CPU 0 unconditionally — what this did until 2026-08-14 —
    // reintroduced the exact hang `cpu_accepts_work` exists to prevent, one line below
    // it.** A ring-0 fault parks the BSP, a wake whose affinity excludes every remaining
    // CPU lands on it, and the thread never runs again. Found by the kernel audit and
    // written up in the decision log, 2026-08-14; the
    // commit that added the guard even documented this fallback as newly reachable and
    // then left it unguarded.
    //
    // Erroring instead is not available: both wake callers `panic!` on `Err`, so a
    // parked BSP would become a kernel panic rather than a misplaced thread.
    if spare != usize::MAX {
        return spare;
    }
    // Nothing at all accepts work. We are executing this code, so this CPU is the
    // least-wrong answer that exists — and if even that is false the machine is already
    // gone.
    SchedState::this_cpu()
}

/// The CPU to place a **waking** thread on: its home CPU (`last_cpu`) when that CPU
/// is online, affinity-permitted, **and has queue room** (cache-warm, and avoids
/// needless migration), otherwise the least-loaded permitted CPU. The room check
/// (F6, decision log 2026-07-21) keeps a full home queue from being a fatal wake:
/// the fallback's least-loaded pick has room unless *every* permitted queue is full.
///
/// **That is one of three ways [`place_thread`] refuses.** This comment claimed it was the
/// only one until 2026-08-14 (kernel audit C.3(c)), and the correction that day named two;
/// `pick_target_cpu` has three exits and all three reach `Err` (PR #204 review, finding 3):
///
/// 1. `best` — the least-loaded CPU that is both affinity-permitted and accepting work, when
///    every such queue is at reserve. The case this paragraph describes.
/// 2. `spare` — reached when affinity permits *no* CPU that accepts work, so placement leaves
///    the mask: a thread pinned to a CPU that has parked, while every CPU still accepting
///    work is full. Note what the panic must **not** say here — "every affinity-permitted
///    queue is at reserve" is false, since the only permitted queue may be empty and merely
///    parked.
/// 3. `this_cpu()` — the last resort, when *nothing* accepts work, returned without checking
///    its queue. Deliberately not load-checked: there is no queue worth preferring when
///    nothing will run, and this CPU is the only one known to be executing.
///
/// Caller holds `SCHED`.
fn pick_wake_cpu(g: &SchedState, obj: *mut (), online: u64) -> usize {
    // SAFETY: `obj` is a pinned Thread; `SCHED` held.
    let home = unsafe { Thread::last_cpu(obj) } as usize;
    let mask = unsafe { Thread::cpu_mask(obj) };
    if home < MAX_CPUS
        && cpu_accepts_work(g, home, online)
        && mask & (1 << home) != 0
        && g.ready[home].len() < g.ready[home].capacity()
    {
        return home;
    }
    pick_target_cpu(g, obj, online)
}

/// Enqueue a runnable thread `r` on a chosen CPU's run queue, seeding its vruntime
/// against that CPU's floor. `wake` selects the placement policy (home CPU vs
/// least-loaded) and the seeding (latency boost vs plain floor). Returns `Err(r)` —
/// handing the ref back so the caller can drop it **outside** the lock — if the
/// chosen queue is at its reserved capacity. Caller holds `SCHED`.
fn place_thread(g: &mut SchedState, r: ObjectRef, wake: bool) -> Result<(), ObjectRef> {
    let obj = r.as_ptr();
    // Read once for this whole placement decision, rather than per candidate CPU.
    let online = online_mask();
    let cpu = if wake {
        // A wake re-homes to the thread's home CPU (`last_cpu`) when possible —
        // cache-warm, and it avoids a needless migration.
        pick_wake_cpu(g, obj, online)
    } else {
        // A newly spawned thread — **user or kernel** — is placed on the
        // least-loaded permitted CPU, so userspace uses the APs from the start.
        // Migrating an already-running user thread is now safe: its CR3 and
        // per-CPU kernel-stack arming (TSS.RSP0 / syscall stack / `KERNEL_GS_BASE`)
        // are re-established on every switch-in (`switch_into` → `resolve_root` +
        // `arm_kernel_stack_for`), the syscall MSRs are re-armed at each ring-3
        // descent, dense CPU indices are unique by construction, the shared
        // kernel-vmap is kept coherent by TLB shootdown, and the switch-out race
        // is closed by the `on_cpu` guard. See the decision log (2026-07-01).
        pick_target_cpu(g, obj, online)
    };
    if g.ready[cpu].len() >= g.ready[cpu].capacity() {
        return Err(r);
    }
    seed_vruntime(g, obj, cpu, wake);
    push_reserved(&mut g.ready[cpu], r, "run queue (placement)");
    g.stats[cpu].placed += 1;
    // If the thread landed on a *different* CPU, poke that CPU with a reschedule
    // IPI so it runs the newcomer promptly (resuming it if idle) instead of waiting
    // for its next periodic tick — the timer is not a dependable wake for a halted
    // CPU, so cross-CPU wake delivery must be explicit. Same-CPU placement needs no
    // IPI: whichever path made this thread runnable reaches a scheduling point before
    // returning to userspace — the timer tail (`on_timer_tick`), the syscall return,
    // or, for a wake from a device-completion DPC, the device-IRQ tail's
    // `resched_if_idle` (added 2026-07-23; without it a same-CPU I/O-completion wake on
    // an idle CPU stalled until the next tick — the fs-server "I/O hang").
    if cpu != SchedState::this_cpu() {
        crate::arch::send_reschedule_ipi(cpu);
    }
    Ok(())
}

/// Steal one runnable thread for this CPU: from the **busiest** other online CPU
/// that actually holds a thread this CPU may run (affinity permits `me`, not
/// mid-switch-out) — so an otherwise-idle CPU picks up slack. Victims are ranked
/// by queue length, but a busier victim with **no** stealable thread does not
/// shadow a smaller one that has one: `steal_one` succeeds exactly when
/// [`steal_available`] is true. (The idle-steal paths are gated on
/// `steal_available` and fall back to taking the idle slot on a `None` — with
/// idle already current that slot is empty, so a mismatch was a panic: F4,
/// decision log 2026-07-21.) The thread is removed from the victim's queue and
/// returned. `None` if no other CPU has a thread this CPU may run. Caller holds
/// `SCHED`.
fn steal_one(g: &mut SchedState) -> Option<ObjectRef> {
    let me = SchedState::this_cpu();
    let mut victim = usize::MAX;
    let mut victim_pos = 0usize;
    let mut victim_len = 0usize;
    for c in 0..MAX_CPUS {
        if c == me || !g.cpu_online[c] || g.ready[c].len() <= victim_len {
            continue;
        }
        // The first thread on this candidate that is stealable to this CPU.
        // SAFETY: each entry pins a live Thread; `SCHED` held.
        let stealable =
            (0..g.ready[c].len()).find(|&i| unsafe { stealable_to(g.ready[c][i].as_ptr(), me) });
        if let Some(pos) = stealable {
            victim = c;
            victim_pos = pos;
            victim_len = g.ready[c].len();
        }
    }
    if victim == usize::MAX {
        return None;
    }
    g.stats[me].steals += 1;
    Some(g.ready[victim].remove(victim_pos))
}

/// `true` if some other online CPU holds a thread this CPU (`me`) may run — the cheap
/// precondition the idle-tick path checks before triggering a steal. Caller holds `SCHED`.
fn steal_available(g: &SchedState, me: usize) -> bool {
    (0..MAX_CPUS).any(|c| {
        c != me
            && g.cpu_online[c]
            // SAFETY: each entry pins a live Thread; `SCHED` held.
            && g.ready[c].iter().any(|r| unsafe { stealable_to(r.as_ptr(), me) })
    })
}

/// `true` if the thread `obj` may be **stolen** to CPU `me`: its affinity includes
/// `me` and it is not still mid-switch-out. **User and kernel threads alike** are
/// stealable — migrating an already-running user thread is safe now that its CR3 and
/// per-CPU kernel-stack state are re-armed on every switch-in and the switch-out race
/// is closed (see [`place_thread`] and the 3b fixes).
///
/// # Safety
/// `obj` is a live, pinned `Thread`; the caller holds `SCHED`.
unsafe fn stealable_to(obj: *mut (), me: usize) -> bool {
    // Skip a thread still mid-switch-out on another CPU (`on_cpu` set): its
    // parked `saved_sp` is not yet committed, so resuming it here would load a
    // stale/garbage frame and double-run it. It stays on the victim's queue and
    // becomes stealable on the next round once the switch completes (the SMP
    // `on_cpu` invariant; see `Thread::on_cpu` / `switch_into`).
    unsafe { !Thread::is_on_cpu(obj) && Thread::cpu_mask(obj) & (1 << me) != 0 }
}

/// Pick the next thread to run on this CPU: its own queue first, else **steal** from
/// the busiest peer. A stolen thread is re-seeded against this CPU's vruntime floor
/// (its vruntime referenced the victim's) so it is neither favored nor starved. `None`
/// only when no CPU has runnable work. Caller holds `SCHED`.
fn pick_next(g: &mut SchedState) -> Option<ObjectRef> {
    if let Some(t) = dequeue_front(g) {
        return Some(t);
    }
    let stolen = steal_one(g)?;
    seed_vruntime(g, stolen.as_ptr(), SchedState::this_cpu(), true);
    Some(stolen)
}

/// Charge the running thread (on this CPU) one tick of virtual runtime, scaled by
/// its nice weight — but only if it is **TimeShared** (RealTime is fixed-priority,
/// not fair-scheduled; Idle never accrues). Caller holds `SCHED`.
fn accrue_vruntime(g: &mut SchedState) {
    let obj = match g.cur_ref().as_ref() {
        Some(c) => c.as_ptr(),
        None => return,
    };
    // SAFETY: `obj` is the running thread (pinned), `SCHED` held — the accessor
    // contract for these field reads/writes.
    unsafe {
        if Thread::sched_class(obj) == SchedClass::TimeShared {
            let v = Thread::vruntime(obj).saturating_add(vruntime_delta(Thread::nice(obj)));
            Thread::set_vruntime(obj, v);
        }
    }
}

/// Seed a freshly-runnable TimeShared thread's vruntime so it joins fairly:
/// `new` (spawned) → the current floor; `woken` → `min_vruntime - slice` (a small
/// latency boost) but never below its own accrued vruntime. No-op for non-TS or a
/// thread already at/above the floor. Caller holds `SCHED`.
fn seed_vruntime(g: &SchedState, obj: *mut (), cpu: usize, woken: bool) {
    // SAFETY: `obj` is a pinned Thread; `SCHED` held.
    if unsafe { Thread::sched_class(obj) } != SchedClass::TimeShared {
        return;
    }
    let base = g.min_vruntime[cpu];
    let floor = if woken {
        base.saturating_sub(TICK_NS)
    } else {
        base
    };
    // SAFETY: as above. A spawned thread (vruntime 0) jumps to the floor; a waker
    // keeps its (higher) accrued vruntime if already ahead of the boosted floor.
    unsafe {
        if Thread::vruntime(obj) < floor {
            Thread::set_vruntime(obj, floor);
        }
    }
}

/// Read the current thread's entry point and argument. Used by
/// [`thread_enter`] when a freshly scheduled thread first runs.
fn current_entry() -> (ThreadEntry, usize) {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
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
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
    // SAFETY: `current` is pinned alive (it holds a refcount on the `Thread`)
    // and we hold the run-queue lock, which — the one global `SCHED` lock — serialises all
    // access to the `Thread`. Forming a shared `&Thread` to read `owner_pid`
    // is sound; no `&mut` is taken anywhere under this lock.
    unsafe { &*(cur.as_ptr() as *const crate::object::Thread) }.owner_pid()
}

/// The calling thread's numeric identity `(pid, tid)`, read under one `SCHED`
/// hold — the capture step of the `/proc/self/status` snapshot (see
/// `object::kernel_server` and `docs/architecture/scheduler.md` § "The stats
/// surface"). `None` when no thread is current yet, or when the current thread
/// is a kernel/boot thread with no owning process (mirroring
/// [`current_process`]'s `None` arm, which the other `/proc/self` leaves map to
/// *not found*).
pub fn current_pid_tid() -> Option<(u32, u32)> {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref()?;
    // SAFETY: `current` is pinned alive and the run-queue lock serialises
    // Thread access; shared `&Thread` reads of the identity fields are sound.
    // `has_process` touches no refcount, so no `ObjectRef` clone/drop happens
    // under the rank-1 lock.
    let t = unsafe { &*(cur.as_ptr() as *const crate::object::Thread) };
    if !t.has_process() {
        return None;
    }
    Some((t.owner_pid(), t.tid()))
}

/// The tid of the currently running thread. Used by the exception path to fill
/// the `thread` field of a fault notification. Takes only the rank-1 lock.
pub fn current_tid() -> u32 {
    let g = SCHED.lock();
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
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
    let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
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
    g.cur_ref().clone()
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
/// The reaper thread body: close the handle tables of processes that have exited, then
/// park until there are more.
///
/// **Why a thread at all.** Deferred reclamation used to run only at
/// `exit_*`/`yield_now`/`suspend_with_fault` and in the idle loop. On a system where no
/// thread exits or yields and no CPU reaches idle — one busy-looping service is enough —
/// nothing ran it, so no exited process was reclaimed and every pipe they held stayed
/// open. That is not a leak, it is a liveness bug: a peer blocked on a channel waits for a
/// `PeerClosed` that only reclamation can deliver. Twice in one day a spinning supervisor
/// hung an unrelated shell that way.
///
/// A normal schedulable thread removes the dependency on scheduling luck: a spinner shares
/// the CPU with the reaper instead of starving it. This is the shape Linux uses for the
/// same problem — the bulk of teardown happens in the dying task, and what must be
/// deferred is drained by dedicated kernel threads (`ksoftirqd`, `rcuo`, `kworker`) rather
/// than by whoever happens along.
///
/// It closes **handle tables only**. Reaped threads' kernel stacks stay with the per-CPU
/// [`reap`](SchedState::reap) path, because a thread parks itself there and then switches
/// off its own stack — freeing that from another CPU is a use-after-free, and the reaper
/// runs wherever it is scheduled.
extern "C" fn reaper_body(_arg: usize) {
    loop {
        // Take one pid at a time: `close_all_owned_by` drops object references, which must
        // happen with `SCHED` released, so the lock cannot be held across the close.
        let next = {
            let mut g = SCHED.lock();
            g.ended_pids.pop()
        };
        match next {
            Some(pid) => {
                crate::handle::global::close_all_owned_by(pid);
                REAPER_CLOSED.fetch_add(1, Ordering::Relaxed);
            }
            None => park_reaper(),
        }
    }
}

/// Park the reaper until [`wake_reaper`] runs. Re-checks the queue **under `SCHED`** before
/// parking, which is what closes the lost-wakeup window: an exit that pushes a pid between
/// the reaper's last drain and this park would otherwise wake nothing and leave the work
/// sitting until some unrelated scheduler entry noticed it.
fn park_reaper() {
    let mut g = SCHED.lock();
    if !g.ended_pids.is_empty() {
        return; // work arrived while we were draining — go round again
    }
    let me = g.cur_slot().take().expect("current set");
    let me_obj = me.as_ptr();
    let next = match pick_next(&mut g) {
        Some(n) => n,
        None => g.idle_slot().take().expect("idle thread exists after init"),
    };
    let next_obj = next.as_ptr();
    // SAFETY: both pinned (self parked in the reaper slot, next becoming current);
    // `SCHED` held — the Thread accessor contract.
    unsafe {
        Thread::set_state(me_obj, ThreadState::Blocked);
        Thread::set_state(next_obj, ThreadState::Running);
    }
    let me_slot = unsafe { Thread::saved_sp_mut_ptr(me_obj) };
    // Park in the reaper slot — not `blocked`, so no generic waker can find it, and not
    // `ready`, so it does not spin. `wake_reaper` is the only way back.
    debug_assert!(g.reaper.is_none());
    g.reaper = Some(me);
    *g.cur_slot() = Some(next);
    // Resumes here, lock-free, when `wake_reaper` re-queues us.
    // SAFETY: `me_slot` is our own (now-parked) saved-SP slot; both threads pinned.
    unsafe { switch_into(g, me_slot, me_obj, next_obj) };
}

/// Make the reaper runnable, if it is parked. Caller holds `SCHED`.
///
/// A no-op when it is already running or queued — the wake is level-triggered off
/// [`SchedState::ended_pids`], so a missed edge costs nothing.
///
/// **A fifth placement site, and the only one that bypasses the policy.** It pushes onto
/// `ready[this_cpu()]` directly: no affinity check and no [`cpu_accepts_work`]. Its capacity
/// check is [`push_reserved`]'s refusal, which is a real one — the `debug_assert!` that used
/// to sit here was not, since the growth beside it happened first and succeeded. Sound today for a reason worth writing down rather than
/// rediscovering — a CPU executing this code is by definition not parked, so the CPU it
/// chooses always accepts work — but it is a fifth opinion on placement, in the same shape
/// as the extra opinions on `online_mask` the audit found in § A. If placement policy grows
/// a rule, this is the site that will not have it (kernel audit C.3(e), 2026-08-14).
fn wake_reaper(g: &mut SchedState) {
    let Some(r) = g.reaper.take() else {
        return;
    };
    // SAFETY: `r` is the parked reaper, pinned by the slot's reference; `SCHED` held.
    unsafe { Thread::set_state(r.as_ptr(), ThreadState::Ready) };
    let cpu = SchedState::this_cpu();
    push_reserved(&mut g.ready[cpu], r, "run queue (reaper wake)");
}

extern "C" fn idle_body(_arg: usize) {
    loop {
        reap_pending();
        // SAFETY: ring-0. `idle_halt` enables interrupts as it parks (`sti; hlt`),
        // so the CPU always sleeps with IF=1 — the periodic timer or a reschedule
        // IPI can then wake it, regardless of the inbound IF state.
        unsafe { Cpu::idle_halt() };
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
        let cur = g.cur_ref().as_ref().expect("current set when a thread runs");
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
    // --- leave_online's decision half ------------------------------------------
    //
    // **Three tests, not one.** They were a single `#[test]` because `ONLINE_MASK` was a
    // process-global and separate tests run concurrently, so splitting them would have made
    // them race — the old comment said exactly that. The set is now per-thread under
    // `cfg(test)` (see `online_set`), so each case establishes its own starting set and fails
    // on its own name.
    //
    // Measured rather than assumed, because the interesting part is how *weak* the natural
    // signal is: split against the shared global these three passed **40 runs out of 40**.
    // They would have been isolated by being fast, which is not isolation. Forcing the
    // interleaving with sleeps — one test asserting 300 ms after its own write, another
    // writing 100 ms in — fails `a_park_clears_its_own_bit_and_only_its_own` deterministically
    // on the shared backing and passes on this one. Same shape, same result, as the mock `IF`
    // flag in `libkern::spinlock`.

    /// A permanent park must clear **its own** bit.
    #[test]
    fn a_park_clears_its_own_bit_and_only_its_own() {
        super::online_set::force(0b1011);
        super::clear_online_bit(true, 1);
        assert_eq!(super::online_mask(), 0b1001);
    }

    /// …and must clear **nobody's** when it has no identity of its own.
    ///
    /// This is the case that matters. `current_cpu()` reports `0` for a CPU that never
    /// established an identity (x86's `IA32_TSC_AUX` reset default), so an unguarded park
    /// clears the *running BSP's* bit — dropping it from the TLB-shootdown set, where the
    /// slow path's self-IPI is the only thing that invalidates its own TLB. A freed frame
    /// stays reachable through a stale translation. Note the range check does not catch it:
    /// `0` is perfectly in range (PR #198 review, blocking 1).
    #[test]
    fn a_park_with_no_identity_clears_nobodys_bit() {
        super::online_set::force(0b1011);
        super::clear_online_bit(false, 0);
        assert_eq!(super::online_mask(), 0b1011, "an unbound CPU cleared someone else's bit");
    }

    /// An out-of-range index is ignored — and this is asserted at **64**, not `MAX_CPUS`.
    ///
    /// `MAX_CPUS` is 8, so deleting the range check leaves `0b1011 & !(1 << 8)` = `0b1011`
    /// and an assertion at `MAX_CPUS` passes for the broken implementation too. A shift only
    /// misbehaves at the width of the type: `1u64 << 64` panics in debug and wraps to
    /// `1 << 0` in release — clearing **CPU 0's** bit, the same silent corruption the
    /// identity guard exists to prevent. Caught by a reviewer's negative control.
    #[test]
    fn a_park_with_an_out_of_range_index_clears_nobodys_bit() {
        super::online_set::force(0b1011);
        super::clear_online_bit(true, 64);
        assert_eq!(super::online_mask(), 0b1011, "an out-of-range index cleared a bit");
    }

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
            ready: [const { KVec::new() }; MAX_CPUS],
            cpu_online: {
                let mut o = [false; MAX_CPUS];
                o[0] = true;
                o
            },
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
        }
    }

    #[test]
    fn dequeue_front_is_round_robin() {
        init_global_heap();
        let mut st = test_state();
        st.ready[0].try_reserve(READY_RESERVE).unwrap();
        for tid in 1..=3 {
            st.ready[0].try_push(inert_ref(tid)).unwrap();
        }
        // Dequeue front, re-enqueue at back: classic round-robin rotation.
        let a = dequeue_front(&mut st).unwrap();
        // SAFETY: pinned, single-threaded test.
        let a_tid = unsafe { &*(a.as_ptr() as *const Thread) }.tid();
        assert_eq!(a_tid, 1);
        st.ready[0].try_push(a).unwrap();
        let b = dequeue_front(&mut st).unwrap();
        let b_tid = unsafe { &*(b.as_ptr() as *const Thread) }.tid();
        assert_eq!(b_tid, 2, "round-robin must pick the next, not repeat");
        st.ready[0].try_push(b).unwrap();
    }

    #[test]
    fn dequeue_front_empty_is_none() {
        init_global_heap();
        let mut st = test_state();
        assert!(dequeue_front(&mut st).is_none());
    }

    fn tid_of(r: &ObjectRef) -> u32 {
        // SAFETY: pinned, single-threaded test.
        unsafe { &*(r.as_ptr() as *const Thread) }.tid()
    }

    #[test]
    fn nice_weight_and_delta_are_sane() {
        assert_eq!(nice_weight(0), NICE_0_WEIGHT);
        assert!(nice_weight(-20) > nice_weight(0));
        assert!(nice_weight(19) < nice_weight(0));
        // Out-of-range nice clamps to the table ends.
        assert_eq!(nice_weight(-128), nice_weight(-20));
        assert_eq!(nice_weight(127), nice_weight(19));
        // nice 0 accrues exactly one slice per tick; lower nice accrues slower.
        assert_eq!(vruntime_delta(0), TICK_NS);
        assert!(vruntime_delta(-20) < vruntime_delta(0));
        assert!(vruntime_delta(19) > vruntime_delta(0));
    }

    #[test]
    fn dequeue_prefers_realtime_by_priority_then_timeshared() {
        init_global_heap();
        let mut st = test_state();
        st.ready[0].try_reserve(READY_RESERVE).unwrap();
        let ts = inert_ref(1); // TimeShared (default), vruntime 0
        let rt_lo = inert_ref(2); // RealTime, priority 20
        let rt_hi = inert_ref(3); // RealTime, priority 5 (higher)
        // SAFETY: pinned, single-threaded test.
        unsafe {
            Thread::set_sched(rt_lo.as_ptr(), SchedClass::RealTime, 20, 0);
            Thread::set_sched(rt_hi.as_ptr(), SchedClass::RealTime, 5, 0);
        }
        st.ready[0].try_push(ts).unwrap();
        st.ready[0].try_push(rt_lo).unwrap();
        st.ready[0].try_push(rt_hi).unwrap();
        // RealTime beats TimeShared; lower rt_priority value wins within RealTime.
        assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 3, "RealTime prio 5 first");
        assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 2, "RealTime prio 20 next");
        assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 1, "TimeShared last");
    }

    #[test]
    fn dequeue_timeshared_picks_min_vruntime_and_advances_floor() {
        init_global_heap();
        let mut st = test_state();
        st.ready[0].try_reserve(READY_RESERVE).unwrap();
        let a = inert_ref(1);
        let b = inert_ref(2);
        let c = inert_ref(3);
        // SAFETY: pinned, single-threaded test.
        unsafe {
            Thread::set_vruntime(a.as_ptr(), 500);
            Thread::set_vruntime(b.as_ptr(), 100); // smallest
            Thread::set_vruntime(c.as_ptr(), 300);
        }
        st.ready[0].try_push(a).unwrap();
        st.ready[0].try_push(b).unwrap();
        st.ready[0].try_push(c).unwrap();
        assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 2, "smallest vruntime first");
        assert_eq!(st.min_vruntime[0], 100, "floor advanced to the picked vruntime");
        assert_eq!(tid_of(&dequeue_front(&mut st).unwrap()), 3, "next-smallest vruntime");
        assert_eq!(st.min_vruntime[0], 300);
    }

    #[test]
    fn seed_vruntime_floors_new_and_boosts_woken() {
        init_global_heap();
        let mut st = test_state();
        let base = 2 * TICK_NS;
        st.min_vruntime[0] = base;
        // A new TimeShared thread (vruntime 0) jumps up to the floor.
        let n = inert_ref(1);
        seed_vruntime(&st, n.as_ptr(), 0, false);
        // SAFETY: pinned, single-threaded test.
        assert_eq!(unsafe { Thread::vruntime(n.as_ptr()) }, base);
        // A waking thread is boosted to `floor - slice` (latency), not the bare floor.
        let w = inert_ref(2);
        seed_vruntime(&st, w.as_ptr(), 0, true);
        assert_eq!(unsafe { Thread::vruntime(w.as_ptr()) }, base - TICK_NS);
        // A thread already ahead of the floor keeps its (larger) vruntime.
        let ahead = inert_ref(3);
        unsafe { Thread::set_vruntime(ahead.as_ptr(), 3 * TICK_NS) };
        seed_vruntime(&st, ahead.as_ptr(), 0, false);
        assert_eq!(unsafe { Thread::vruntime(ahead.as_ptr()) }, 3 * TICK_NS);
    }

    /// Bring `n` CPUs online (0..n) with reserved ready queues, for placement tests.
    fn online_n(st: &mut SchedState, n: usize) {
        for c in 0..n {
            st.cpu_online[c] = true;
            st.ready[c].try_reserve(READY_RESERVE).unwrap();
        }
    }

    /// The online set a placement test passes in: `n` CPUs up, none parked.
    ///
    /// A value rather than the `ONLINE_MASK` global, deliberately — see
    /// [`cpu_accepts_work`]. Writing the global here made these tests race each other.
    fn all_up(n: usize) -> u64 {
        (1u64 << n) - 1
    }

    /// A CPU that has parked forever must not be given work — and its queue must still be
    /// drainable, which is how the threads it already holds get rescued.
    ///
    /// The two halves are the whole point. `leave_online` cannot clear `cpu_online[]` (that
    /// needs `SCHED`, which the parking CPU may hold), so after a ring-0 fault parks an AP
    /// the two disagree: placing on it hangs the thread forever, while draining its queue is
    /// the rescue.
    ///
    /// **The steal half asserts against an emptied peer set on purpose.** The first version
    /// of this test left a thread on CPU 1, so `steal_available` was satisfied by *that*
    /// queue and the assertion passed even with the victim side wrongly gated — the exact
    /// "consistency cleanup" the doc comments warn against would have gone green
    /// (PR #199 review, blocking 2). CPU 2 must be the only stealable queue for this to mean
    /// anything, and `steal_one` is checked too rather than inferred from `steal_available`.
    #[test]
    fn a_parked_cpu_takes_no_new_work_but_can_still_be_drained() {
        init_global_heap();
        let mut st = test_state();
        online_n(&mut st, 4);
        let t = inert_ref(1);

        // Load 0 and 1 so the least-loaded pick would otherwise land on CPU 2.
        st.ready[0].try_push(inert_ref(10)).unwrap();
        st.ready[1].try_push(inert_ref(11)).unwrap();
        assert_eq!(pick_target_cpu(&st, t.as_ptr(), all_up(4)), 2);

        // CPU 2 parks: gone from the online set, still `true` in `cpu_online[]`.
        let parked = all_up(4) & !(1u64 << 2);
        assert!(st.cpu_online[2], "the park cannot clear this, which is why the bug existed");
        assert_eq!(
            pick_target_cpu(&st, t.as_ptr(), parked),
            3,
            "placed on a CPU that will never run"
        );

        // A waking thread whose home is the parked CPU goes elsewhere too.
        unsafe { Thread::set_last_cpu(t.as_ptr(), 2) };
        assert_ne!(pick_wake_cpu(&st, t.as_ptr(), parked), 2, "woken onto a parked home CPU");

        // Now the rescue half. Empty every other queue so the parked CPU is the *only*
        // source a steal could come from — otherwise this passes without testing anything.
        drop(st.ready[0].pop().unwrap());
        drop(st.ready[1].pop().unwrap());
        for c in 0..MAX_CPUS {
            assert!(st.ready[c].is_empty(), "cpu {c} must start this half empty");
        }
        st.ready[2].try_push(inert_ref(12)).unwrap();

        // Host `this_cpu()` is 0. Both paths must reach a parked CPU's queue: gating either
        // on `cpu_accepts_work` strands the thread sitting on CPU 2 forever.
        assert!(
            steal_available(&st, 0),
            "a parked CPU's queue must stay drainable, or this fix strands what it holds"
        );
        let stolen = steal_one(&mut st).expect("steal_one must reach a parked CPU's queue");
        assert_eq!(unsafe { &*(stolen.as_ptr() as *const Thread) }.tid(), 12);
        assert!(st.ready[2].is_empty(), "the parked CPU's queue was not drained");
    }

    /// When affinity permits no CPU that accepts work, placement must still land somewhere
    /// that will run the thread — **not** on CPU 0 because it happened to be the fallback.
    ///
    /// This is the hang `cpu_accepts_work` exists to prevent, reintroduced one line below it
    /// by `if best == usize::MAX { 0 }` (kernel audit; decision log 2026-08-14). Reachable
    /// when a ring-0 fault parks
    /// the BSP while APs keep running and a pinned thread wakes.
    ///
    /// Asserted as "the chosen CPU accepts work" rather than "the chosen CPU is 1 or 2", so
    /// the test states the invariant instead of the current tie-break.
    #[test]
    fn placement_falls_back_to_a_cpu_that_accepts_work_not_to_cpu_zero() {
        init_global_heap();
        let mut st = test_state();
        online_n(&mut st, 4);
        // Pinned to CPU 3; CPUs 0 and 3 have both parked. Nothing permitted accepts work,
        // and the old fallback answered 0 — which is parked.
        let t = inert_ref(1);
        unsafe { Thread::set_cpu_mask(t.as_ptr(), 1 << 3) };
        let parked = all_up(4) & !(1u64 << 0) & !(1u64 << 3);

        let chosen = pick_target_cpu(&st, t.as_ptr(), parked);
        assert!(
            cpu_accepts_work(&st, chosen, parked),
            "fallback picked cpu {chosen}, which does not accept work — the thread would \
             never be scheduled"
        );

        // **And the fallback is least-loaded, not first-found.** A CPU that accepts work but
        // whose queue is at `READY_RESERVE` is not runnable either: `place_thread` returns
        // `Err` and both wake callers turn that into a `panic!`. Filling CPU 1 must push the
        // choice to CPU 2 rather than onto the full queue beside it (PR #201 review,
        // finding 2).
        for i in 0..READY_RESERVE {
            st.ready[1].try_push(inert_ref(100 + i as u32)).unwrap();
        }
        let chosen = pick_target_cpu(&st, t.as_ptr(), parked);
        assert!(
            cpu_accepts_work(&st, chosen, parked),
            "fallback picked cpu {chosen}, which does not accept work"
        );
        assert!(
            st.ready[chosen].len() < st.ready[chosen].capacity(),
            "fallback picked cpu {chosen}, whose queue is at reserve — place_thread would \
             return Err and the wake callers panic"
        );

        // The wake path delegates to the same fallback. (Its own `cpu_accepts_work` check on
        // the home CPU is covered by `a_parked_cpu_takes_no_new_work_but_can_still_be_drained`
        // — here the affinity mask rejects home 0 before that check is reached, so this
        // asserts the delegation and nothing more.)
        unsafe { Thread::set_last_cpu(t.as_ptr(), 0) };
        let woken = pick_wake_cpu(&st, t.as_ptr(), parked);
        assert!(
            cpu_accepts_work(&st, woken, parked),
            "wake fallback picked cpu {woken}, which does not accept work"
        );
    }

    #[test]
    fn placement_least_loaded_and_respects_affinity() {
        init_global_heap();
        let mut st = test_state();
        online_n(&mut st, 4);
        // No affinity → least-loaded. With all queues empty, CPU 0 (first min) wins.
        let a = inert_ref(1);
        assert_eq!(pick_target_cpu(&st, a.as_ptr(), all_up(4)), 0);
        // Load CPU 0 and 1; the least-loaded becomes CPU 2.
        st.ready[0].try_push(inert_ref(10)).unwrap();
        st.ready[1].try_push(inert_ref(11)).unwrap();
        assert_eq!(pick_target_cpu(&st, a.as_ptr(), all_up(4)), 2);
        // Pinned to CPU 3 → must target CPU 3 regardless of load.
        unsafe { Thread::set_cpu_mask(a.as_ptr(), 1 << 3) };
        assert_eq!(pick_target_cpu(&st, a.as_ptr(), all_up(4)), 3);
        // Pinned to a CPU that is not online (5) → falls back **outside** the mask, to the
        // least-loaded CPU that accepts work. This asserted CPU 0 unconditionally until
        // 2026-08-14, when that turned out to be a hang rather than a defensive default:
        // once a CPU can park, "CPU 0" is not necessarily a CPU that runs anything. Stated
        // as the two properties rather than an index, so a tie-break change does not read
        // as a contract change.
        unsafe { Thread::set_cpu_mask(a.as_ptr(), 1 << 5) };
        let chosen = pick_target_cpu(&st, a.as_ptr(), all_up(4));
        assert!(cpu_accepts_work(&st, chosen, all_up(4)), "fallback chose cpu {chosen}");
        assert_eq!(
            st.ready[chosen].len(),
            0,
            "fallback chose loaded cpu {chosen} while an empty one accepted work"
        );
    }

    #[test]
    fn wake_placement_prefers_home_cpu() {
        init_global_heap();
        let mut st = test_state();
        online_n(&mut st, 4);
        let t = inert_ref(1);
        // Home CPU 2 (last ran there) + affinity allows it → wake returns CPU 2.
        unsafe { Thread::set_last_cpu(t.as_ptr(), 2) };
        assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 2);
        // Affinity now excludes the home → falls back to least-loaded (CPU 0).
        unsafe { Thread::set_cpu_mask(t.as_ptr(), 1 << 0) };
        assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 0);
    }

    #[test]
    fn stealable_respects_affinity() {
        init_global_heap();
        let t = inert_ref(1); // kernel thread (no user_entry), default mask = all
        unsafe {
            assert!(stealable_to(t.as_ptr(), 0));
            assert!(stealable_to(t.as_ptr(), 3));
            // Pin to CPU 0 only: stealable to 0, not to 1.
            Thread::set_cpu_mask(t.as_ptr(), 1 << 0);
            assert!(stealable_to(t.as_ptr(), 0));
            assert!(!stealable_to(t.as_ptr(), 1));
        }
    }

    #[test]
    fn wake_placement_falls_back_when_home_queue_is_full() {
        init_global_heap();
        let mut st = test_state();
        online_n(&mut st, 2);
        let t = inert_ref(1);
        unsafe { Thread::set_last_cpu(t.as_ptr(), 0) };
        // Fill CPU 0's queue to its reserve: the home pick must divert (F6 — a
        // full home queue used to be a fatal wake).
        for tid in 100..(100 + READY_RESERVE as u32) {
            st.ready[0].try_push(inert_ref(tid)).unwrap();
        }
        assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 1);
        // With room at home again, the cache-warm home pick returns.
        drop(st.ready[0].pop().unwrap());
        assert_eq!(pick_wake_cpu(&st, t.as_ptr(), all_up(4)), 0);
    }

    #[test]
    fn steal_one_skips_busiest_victim_with_no_stealable_thread() {
        init_global_heap();
        let mut st = test_state();
        online_n(&mut st, 3);
        // CPU 1 (busiest, 2 threads) — both pinned to CPU 1: not stealable to 0.
        for tid in 10..12 {
            let t = inert_ref(tid);
            unsafe { Thread::set_cpu_mask(t.as_ptr(), 1 << 1) };
            st.ready[1].try_push(t).unwrap();
        }
        // CPU 2 (smaller, 1 thread) — default all-CPU mask: stealable.
        st.ready[2].try_push(inert_ref(20)).unwrap();
        // Host `this_cpu()` is 0. `steal_available` says true; the F4 bug was
        // `steal_one` returning None here (it searched only the busiest victim),
        // which panics the idle-steal paths on the empty idle slot.
        assert!(steal_available(&st, 0));
        let stolen = steal_one(&mut st).expect("steal_one must succeed when steal_available");
        let stolen_tid = unsafe { &*(stolen.as_ptr() as *const Thread) }.tid();
        assert_eq!(stolen_tid, 20);
        assert!(st.ready[2].is_empty());
    }

    #[test]
    fn steal_one_prefers_the_busiest_stealable_victim() {
        init_global_heap();
        let mut st = test_state();
        online_n(&mut st, 3);
        st.ready[1].try_push(inert_ref(10)).unwrap();
        for tid in 20..22 {
            st.ready[2].try_push(inert_ref(tid)).unwrap();
        }
        // Both victims have stealable threads; CPU 2 is busier — steal from it.
        let stolen = steal_one(&mut st).unwrap();
        let stolen_tid = unsafe { &*(stolen.as_ptr() as *const Thread) }.tid();
        assert_eq!(stolen_tid, 20);
        assert_eq!(st.ready[2].len(), 1);
        assert_eq!(st.ready[1].len(), 1);
    }

    #[test]
    fn drain_pending_drops_moves_all_and_preserves_capacity() {
        init_global_heap();
        let mut st = test_state();
        st.reap[0].try_reserve(REAP_RESERVE).unwrap();
        st.deferred_drops.try_reserve(DEFERRED_DROP_RESERVE).unwrap();
        let reap_cap = st.reap[0].capacity();
        let dd_cap = st.deferred_drops.capacity();
        for tid in 1..=3 {
            st.reap[0].try_push(inert_ref(tid)).unwrap();
        }
        for tid in 4..=5 {
            st.deferred_drops.try_push(inert_ref(tid)).unwrap();
        }
        let mut buf: [Option<ObjectRef>; REAP_RESERVE + DEFERRED_DROP_RESERVE] =
            [const { None }; REAP_RESERVE + DEFERRED_DROP_RESERVE];
        let n = drain_pending_drops(&mut st, &mut buf);
        assert_eq!(n, 5);
        assert!(st.reap[0].is_empty());
        assert!(st.deferred_drops.is_empty());
        // The reserves survive the drain — the F11 hazard was `mem::take` zeroing
        // them, making the next exit-path push allocate under `SCHED`.
        assert_eq!(st.reap[0].capacity(), reap_cap);
        assert_eq!(st.deferred_drops.capacity(), dd_cap);
        drop(buf); // the moved refs release their threads here (outside any lock)
    }

    // --- stats: the pure format step of capture → format → synthesize. The
    // increment sites and `stats_snapshot` need a running scheduler (they read
    // `this_cpu()`) and are validated under QEMU (Part D's selftest gates the
    // verdict on two-CPU activity).

    /// A snapshot with the given rows online (all other CPUs offline).
    fn snap_with(rows: &[(usize, stats::CpuSnapshot)]) -> stats::Snapshot {
        let mut s = stats::Snapshot {
            cpus: [stats::CpuSnapshot::OFFLINE; MAX_CPUS],
            reaper_closed: 0,
            exit_closed: 0,
        };
        for &(c, row) in rows {
            s.cpus[c] = row;
        }
        s
    }

    #[test]
    fn stats_format_renders_header_and_one_row_per_online_cpu() {
        init_global_heap();
        let snap = snap_with(&[
            (
                0,
                stats::CpuSnapshot {
                    online: true,
                    idle: false,
                    ready: 1,
                    counters: stats::Counters {
                        switches: 1342,
                        steals: 3,
                        placed: 57,
                        resched_ipis: 12,
                        ticks: 4096,
                    },
                },
            ),
            (
                1,
                stats::CpuSnapshot {
                    online: true,
                    idle: true,
                    ready: 0,
                    counters: stats::Counters {
                        switches: 987,
                        steals: 11,
                        placed: 40,
                        resched_ipis: 9,
                        ticks: 4080,
                    },
                },
            ),
        ]);
        assert_eq!(snap.cpus_online(), 2);
        let text = stats::format(&snap).unwrap();
        assert_eq!(
            text.as_str(),
            "cpus_online=2\n\
             reaper_closed=0\n\
             exit_closed=0\n\
             cpu=0 online=1 switches=1342 steals=3 placed=57 ipis=12 ticks=4096 ready=1 idle=0\n\
             cpu=1 online=1 switches=987 steals=11 placed=40 ipis=9 ticks=4080 ready=0 idle=1\n"
        );
    }

    #[test]
    fn stats_format_omits_offline_cpus_preserving_indices() {
        init_global_heap();
        let row = stats::CpuSnapshot {
            online: true,
            idle: true,
            ready: 0,
            counters: stats::Counters::ZERO,
        };
        // CPUs 0 and 2 online, 1 offline: rows keep their dense indices.
        let snap = snap_with(&[(0, row), (2, row)]);
        let text = stats::format(&snap).unwrap();
        assert_eq!(
            text.as_str(),
            "cpus_online=2\n\
             reaper_closed=0\n\
             exit_closed=0\n\
             cpu=0 online=1 switches=0 steals=0 placed=0 ipis=0 ticks=0 ready=0 idle=1\n\
             cpu=2 online=1 switches=0 steals=0 placed=0 ipis=0 ticks=0 ready=0 idle=1\n"
        );
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
            st.ready[0].try_reserve(READY_RESERVE).unwrap();
            for tid in 1..=4 {
                st.ready[0].try_push(inert_ref(tid)).unwrap();
            }
            *st.cur_slot() = Some(inert_ref(5));
            st.reap[0].try_reserve(REAP_RESERVE).unwrap();
            st.reap[0].try_push(inert_ref(6)).unwrap();
            st.reap[0].try_push(inert_ref(7)).unwrap();
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
        st.ready[0].try_reserve(READY_RESERVE).unwrap();
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
        st.ready[0].try_push(r).unwrap();

        assert!(st.suspended.is_empty());
        assert_eq!(st.ready[0].len(), 1);
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
        st.ready[0].try_reserve(READY_RESERVE).unwrap();
        st.blocked.try_reserve(BLOCKED_RESERVE).unwrap();
        st.suspended.try_reserve(BLOCKED_RESERVE).unwrap();

        // pid 1 has a sibling parked in `suspended`; pid 2 has none anywhere.
        st.ready[0].try_push(inert_user_ref(1, 1)).unwrap();
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
            st.ready[0].try_reserve(READY_RESERVE).unwrap();
            st.reap[0].try_reserve(REAP_RESERVE).unwrap();
            st.ready[0].try_push(inert_user_ref(1, 1)).unwrap();
            st.ready[0].try_push(inert_user_ref(2, 2)).unwrap();
            st.ready[0].try_push(inert_user_ref(3, 1)).unwrap();

            reap_matching(&mut st.ready[0], &mut st.reap[0], 1);
            assert_eq!(st.reap[0].len(), 2, "both pid-1 threads reaped");
            assert_eq!(st.ready[0].len(), 1, "the pid-2 thread stays");
            // SAFETY: pinned, single-threaded.
            let left = unsafe { &*(st.ready[0].iter().next().unwrap().as_ptr() as *const Thread) };
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
