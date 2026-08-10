//! Lock ranks, and the debug-build tracker that enforces them.
//!
//! `kernel/docs/lock-ordering.md` defines a total order over every long-lived lock: a
//! lock may only be taken while holding locks of *strictly higher* rank (smaller number).
//! Until now that order was "enforced by code review", and three deadlocks (F1, F2, F12 —
//! decision log 2026-07-21) got through it, each found by hand or by bisecting a boot loop
//! that hung one time in three. This turns that class of bug into an immediate, located
//! panic naming both locks.
//!
//! ## What is checked
//!
//! On acquire, the new lock's rank must be **strictly greater** than every rank held *in
//! the current interrupt scope*. That single rule catches inversions (taking a high-ranked
//! lock under a low-ranked one) *and* same-rank nesting, which the ordering document
//! forbids separately. The document also claims one same-rank exception exists but never
//! names it; both places it discusses nesting say "never nested". So the rule is enforced
//! without an exception, and if a legitimate one exists the tracker will find it rather
//! than the next reader having to.
//!
//! ## Interrupt scopes
//!
//! **The acquisition order restarts at every interrupt entry**, and modelling that is not
//! optional — it is what makes the tracker sound at all. A plain [`SpinLock`] is held with
//! interrupts *enabled*, so a timer tick or an IPI routinely lands on a thread holding, say,
//! `Buddy` or `HandleTable`, and the handler legitimately takes `SCHED`. Ranked flat that
//! reads as an inversion, and it is not one: the handler releases everything it takes before
//! the interrupted section resumes, so the two nestings never actually interleave.
//!
//! [`SpinLock`]: crate::libkern::SpinLock
//!
//! So each CPU keeps a **floor** alongside its held-rank stack. [`enter_interrupt`] raises
//! the floor to the current depth and its guard lowers it again at the handler's return;
//! the ordering check only looks at entries at or above the floor. A handler therefore
//! starts from an empty view and is checked in full against *its own* acquisitions, while
//! the interrupted context's holds stay recorded (and stay enforced once it resumes).
//!
//! This was the reason the first attempt at this tracker was withdrawn from the Slice D PR
//! (decision log 2026-07-29): it ranked interrupt-side locks at the bottom of a single flat
//! order and hoped that covered it. It does not — `SCHED` is the *top* rank and an interrupt
//! handler is exactly the thing that takes it under anything.
//!
//! Every interrupt entry must open a scope. Missing one does not fail loudly, it just
//! reports phantom inversions from that vector, so the entry points do not rely on anyone
//! remembering: the `irq_dispatcher!` macro in `arch/x86_64/idt.rs` is the only way to
//! define an interrupt dispatcher, it opens the scope itself, and `cargo xtask
//! check-irq-scope` fails the build if a naked entry stub calls anything else.
//!
//! ## Per-CPU, not per-thread
//!
//! The held-rank stack is per-CPU, which is sound because a lock holder cannot migrate: a
//! plain `SpinLock` section is a no-preemption region (the F12 fix) and an `IrqSpinLock`
//! masks interrupts. So whatever this CPU holds, this thread holds.
//!
//! The place that could break it is a context switch, which moves threads between CPUs
//! underneath the per-CPU state. It does not break it, because a thread can only be
//! switched away with **nothing held and no scope open** — a plain-lock holder has
//! preemption disabled, an `IrqSpinLock` holder has interrupts masked, and blocking with a
//! lock held is forbidden outright. That is a precondition rather than a hope, so
//! [`assert_switch_safe`] checks it at the one choke point every switch passes through
//! ([`crate::sched::switch_into`]). If it ever fires, the per-CPU model has stopped
//! holding and the tracker says so instead of quietly corrupting.
//!
//! ## Cost and safety
//!
//! Debug builds only (`cfg(debug_assertions)`) — a release kernel compiles this away
//! entirely, so there is no production cost to argue about. The tracker itself takes no
//! lock and allocates nothing (it would deadlock against the very thing it instruments):
//! it is a fixed per-CPU array of atomics, mirroring `sched`'s `PREEMPT_OFF`.
//!
//! ## Not under host `cfg(test)`
//!
//! Also compiled out for the host unit tests, and this is a correctness requirement, not a
//! cost saving. `cargo test` runs test functions on **many OS threads**, all of which
//! report the same `current_cpu()` — so a per-CPU stack becomes a stack shared by
//! unrelated threads, and two of them legitimately holding unrelated locks reads as an
//! inversion. (Found the direct way: wiring this up hung the host suite with two test
//! binaries spinning at >800 % CPU.) The per-CPU model is sound only where a holder cannot
//! migrate, which is a property of the real kernel's no-preemption regions and interrupt
//! masking — neither of which exists in a hosted test process.
//!
//! The scope arithmetic itself does not depend on any of that, so it is unit-tested
//! host-side against a plain local state struct — see the tests at the bottom.

/// Where a lock sits in the acquisition order. **Smaller is acquired first** — a lock may
/// only be taken while every rank already held is strictly smaller.
///
/// The numbering is sparse on purpose, so a new lock can be slotted between two existing
/// ranks without renumbering (and without the renumbering silently changing an unrelated
/// pair's relative order). Mirrors the table in `kernel/docs/lock-ordering.md`; the two
/// are meant to be read together.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum LockRank {
    /// Rank 1 — the scheduler runqueue (`SCHED`). Outermost.
    Sched = 10,
    /// Rank 3 — handle-table segment allocation.
    HandleTable = 30,
    /// Rank 4 — kernel-object internals: `AddressSpace`, `Namespace`, the `FileObject`
    /// page cache. Independent of each other and **never nested** in either order.
    KernelObject = 40,
    /// Rank 5 — subsystem registries that **allocate while held**: the PCI device table
    /// (`DEVICES`), the GPT partition table (`PARTITIONS`). They push or reserve a `KVec`
    /// inside their critical section, so they must rank *above* the allocators; they are
    /// not leaves, however much they look like one. Taken top-level (nothing else held)
    /// and never from interrupt context, so ranking them here costs nothing.
    Registry = 50,
    /// Rank 6a — a slab cache's free list. Above the buddy because a slab refill calls
    /// into it.
    SlabCache = 60,
    /// Rank 6b — the buddy frame allocator.
    Buddy = 62,
    /// Rank 6c — the kernel-half PML4 template.
    KernelPmlTemplate = 64,
    /// Rank 6d — the kernel vmap bump pointer.
    KernelVmap = 66,
    /// Rank 7 — the serial port.
    Serial = 70,
    /// Below the serial port: the kernel log ring, teed from inside the serial
    /// `write_str` **while `SERIAL` is held**, which is what fixes it here rather than
    /// among the leaves. (It uses `try_lock` for a different reason — re-entry from a
    /// fault that strikes mid-push.)
    Klog = 72,
    /// The TLB-shootdown serialiser (`tlb::LOCK`), which is held with **interrupts
    /// enabled** — the F1 fix (decision log 2026-07-21): an initiator spinning for
    /// acknowledgements must keep taking incoming shootdown IPIs, or two initiators
    /// deadlock.
    ///
    /// That property is why the first tracker had to exempt this lock from ordering
    /// altogether: interrupt work runs beneath it (a DPC drain at an interrupt tail, a
    /// timer tick taking `SCHED`) and no single number can express "the order restarts
    /// here". Interrupt scopes express it directly, so the exemption is gone and this is an
    /// ordinary rank — the interrupt work beneath it is checked in its own scope, and the
    /// lock is checked in the initiator's.
    ///
    /// It also carries a stricter contract the ranking alone would not catch: the caller
    /// must hold **no other lock** when taking it (see `kernel/docs/lock-ordering.md` § F1).
    /// [`acquired`] checks that separately.
    TlbShootdown = 85,
    /// Interrupt-side and bottom-of-order locks, taking nothing while held: the DPC queue,
    /// the entropy pool, the console input buffer, the AHCI pending ring, the completed-IRP
    /// reclaim list.
    Leaf = 90,
}

/// Debug-build enforcement. Compiled out in release builds, and in host unit tests (see
/// the module docs — the per-CPU model does not hold there).
#[cfg(all(debug_assertions, not(test)))]
pub use tracker::{
    IrqScope, acquired, assert_switch_safe, assert_user_entry_safe, enter_interrupt, releasing,
};

/// Release builds and host tests: every hook vanishes.
#[cfg(not(all(debug_assertions, not(test))))]
mod inert {
    use super::LockRank;

    /// Release builds and host tests: an empty guard, so the entry points type-check
    /// identically whether or not the tracker is compiled in.
    pub struct IrqScope;

    #[inline(always)]
    pub fn acquired(_rank: LockRank) {}
    #[inline(always)]
    pub fn releasing(_rank: LockRank) {}
    #[inline(always)]
    pub fn enter_interrupt() -> IrqScope {
        IrqScope
    }
    #[inline(always)]
    pub fn assert_switch_safe() {}
    #[inline(always)]
    pub fn assert_user_entry_safe() {}
}

#[cfg(not(all(debug_assertions, not(test))))]
pub use inert::{
    IrqScope, acquired, assert_switch_safe, assert_user_entry_safe, enter_interrupt, releasing,
};

#[cfg(all(debug_assertions, not(test)))]
mod tracker {
    use super::LockRank;
    use crate::arch::Cpu;
    use crate::arch::cpu::ArchCpu;
    use crate::arch::smp::MAX_CPUS;
    use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    /// Mask interrupts across a tracker update, returning the prior state.
    ///
    /// The per-CPU depth is a **read-modify-write**, and a plain `SpinLock` acquire leaves
    /// interrupts *enabled* (only `preempt_disable` has run, which stops migration, not
    /// interruption). An IRQ landing between the read and the write does its own
    /// acquire/release, and the outer write then clobbers the depth with a stale value —
    /// corrupting the stack and producing phantom violations from then on. `sched`'s
    /// `preempt_disable` masks around its counter for exactly this reason; this is the
    /// same hazard on a different counter.
    #[inline]
    fn mask() -> bool {
        // SAFETY: ring-0; the window is a handful of instructions, restored below.
        unsafe { Cpu::interrupts_disable() }
    }

    #[inline]
    fn unmask(prev: bool) {
        // SAFETY: ring-0; restoring the state `mask` captured.
        unsafe { Cpu::interrupts_restore(prev) };
    }

    /// Deepest lock nesting the tracker records per CPU. Overflow is not a violation —
    /// the tracker stops recording past this and says so — so a legitimately deep path
    /// degrades to "unchecked" rather than to a false panic.
    const MAX_HELD: usize = 8;

    /// Ranks currently held on each CPU, innermost last.
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: AtomicU8 = AtomicU8::new(0);
    #[allow(clippy::declare_interior_mutable_const)]
    const EMPTY: [AtomicU8; MAX_HELD] = [ZERO; MAX_HELD];
    static HELD: [[AtomicU8; MAX_HELD]; MAX_CPUS] = [EMPTY; MAX_CPUS];

    /// How many entries of `HELD[cpu]` are live. May exceed `MAX_HELD`; the excess is
    /// counted but not recorded (see `MAX_HELD`).
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO_DEPTH: AtomicUsize = AtomicUsize::new(0);
    static DEPTH: [AtomicUsize; MAX_CPUS] = [ZERO_DEPTH; MAX_CPUS];

    /// Index below which held ranks belong to an **interrupted** context and are invisible
    /// to the ordering check. Raised by [`enter_interrupt`], lowered by its guard. See the
    /// module docs § Interrupt scopes.
    static FLOOR: [AtomicUsize; MAX_CPUS] = [ZERO_DEPTH; MAX_CPUS];

    /// Set once a violation has been reported, so the panic path — which takes `SERIAL`,
    /// and may take locks on the way — cannot recurse into a second report.
    static REPORTING: AtomicUsize = AtomicUsize::new(0);

    #[inline]
    fn this_cpu() -> usize {
        use crate::arch::smp::ArchSmp;
        let c = crate::arch::Smp::current_cpu() as usize;
        // Defensive: an out-of-range id (very early boot, before SMP is up) must not
        // index out of bounds. Fold it onto CPU 0, which is where that code runs.
        if c < MAX_CPUS { c } else { 0 }
    }

    /// Record that `rank` has just been acquired on this CPU, and panic if doing so
    /// inverted the order.
    ///
    /// Called *after* the lock is held, so the report can name a real acquisition rather
    /// than a speculative one — and so a failed `try_lock` records nothing.
    pub fn acquired(rank: LockRank) {
        let irq = mask();
        let cpu = this_cpu();
        let floor = FLOOR[cpu].load(Ordering::Relaxed);
        let depth = DEPTH[cpu].load(Ordering::Relaxed);
        // Check against everything held **in this interrupt scope** — entries below the
        // floor belong to a context this handler interrupted and will not resume until the
        // handler has released everything it took. Find the violation under the mask but
        // report outside it: `report` panics, and the panic path prints.
        let mut violation = None;
        let mut contract = None;
        for i in floor..depth.min(MAX_HELD) {
            let held = HELD[cpu][i].load(Ordering::Relaxed);
            if held == 0 {
                continue;
            }
            // The shootdown lock's own contract is stricter than its rank: *nothing* may be
            // held when it is taken. Checked here because this is the machinery that knows.
            if matches!(rank, LockRank::TlbShootdown) {
                contract = Some(held);
                break;
            }
            if (rank as u8) <= held {
                violation = Some(held);
                break;
            }
        }
        if depth < MAX_HELD {
            HELD[cpu][depth].store(rank as u8, Ordering::Relaxed);
        }
        DEPTH[cpu].store(depth + 1, Ordering::Relaxed);
        unmask(irq);
        if let Some(held) = contract {
            report_contract(held);
        }
        if let Some(held) = violation {
            report(held, rank as u8);
        }
    }

    /// Record that `rank` is about to be released on this CPU.
    ///
    /// Called *before* the release, so the stack is already correct if the release path
    /// goes on to take another lock (a guard drop can replay a deferred reschedule, which
    /// takes `SCHED`).
    pub fn releasing(_rank: LockRank) {
        let irq = mask();
        let cpu = this_cpu();
        let depth = DEPTH[cpu].load(Ordering::Relaxed);
        // Never pop below the floor: those entries belong to the interrupted context, and
        // an unbalanced release inside a handler must not eat them. `depth == 0` is the
        // same case at the outermost scope (a lock acquired before the tracker saw this
        // CPU).
        if depth > FLOOR[cpu].load(Ordering::Relaxed) {
            let depth = depth - 1;
            if depth < MAX_HELD {
                HELD[cpu][depth].store(0, Ordering::Relaxed);
            }
            DEPTH[cpu].store(depth, Ordering::Relaxed);
        }
        unmask(irq);
    }

    /// Open a fresh lock-ordering scope for an interrupt handler: ranks held by the
    /// interrupted context stay recorded but stop being checked against, so a handler is
    /// ordered against its own acquisitions only. The guard restores the previous scope.
    ///
    /// Called at every interrupt entry — see the module docs § Interrupt scopes for why
    /// that is mandatory rather than an optimisation, and `irq_dispatcher!` in
    /// `arch/x86_64/idt.rs` for how "every" is enforced.
    ///
    /// Interrupts are already masked here (the IDT gate clears `IF` and no dispatcher
    /// re-enables), so the update needs no mask of its own.
    #[must_use = "the scope is restored when the guard drops; dropping it immediately \
                  leaves the handler checked against the interrupted context"]
    pub fn enter_interrupt() -> IrqScope {
        let cpu = this_cpu();
        let entry_depth = DEPTH[cpu].load(Ordering::Relaxed);
        let saved_floor = FLOOR[cpu].swap(entry_depth, Ordering::Relaxed);
        IrqScope { saved_floor, entry_depth }
    }

    /// Guard returned by [`enter_interrupt`]; its `Drop` closes the interrupt scope.
    pub struct IrqScope {
        /// Floor to restore — non-zero only for a nested interrupt.
        saved_floor: usize,
        /// Depth at entry. The handler must return to it, or it leaked a lock.
        entry_depth: usize,
    }

    impl Drop for IrqScope {
        fn drop(&mut self) {
            // Re-read the CPU rather than caching it at entry: a handler that reschedules
            // (the timer tail, a device-IRQ completion) can resume on a *different* CPU.
            // That is only consistent because a switch requires an empty stack and no open
            // scope on both ends — `assert_switch_safe` enforces exactly that, so both the
            // saved floor and the entry depth are zero whenever this can happen.
            let cpu = this_cpu();
            let depth = DEPTH[cpu].load(Ordering::Relaxed);
            if depth != self.entry_depth {
                report_leak(self.entry_depth, depth);
            }
            FLOOR[cpu].store(self.saved_floor, Ordering::Relaxed);
        }
    }

    /// Assert that this CPU is safe to context-switch on: nothing held, no interrupt scope
    /// open. Called from [`crate::sched::switch_into`] once `SCHED` has been released, the
    /// single point every context switch passes through.
    ///
    /// This is the per-CPU model's soundness precondition (module docs § Per-CPU, not
    /// per-thread). Checking it is what lets the rest of the tracker treat "this CPU holds"
    /// and "this thread holds" as the same statement.
    pub fn assert_switch_safe() {
        assert_empty(Site::ContextSwitch);
    }

    /// Assert that this CPU holds nothing as a thread enters the kernel from ring 3.
    ///
    /// A `syscall` is **not** an interrupt and gets no scope: the acquisition order does not
    /// restart here, it *begins* — the calling thread was in user mode, so it can hold no
    /// kernel lock. That makes the boundary a free, exact statement of ground truth, and
    /// therefore the best place to notice an unbalanced acquire/release that would otherwise
    /// accumulate silently and misattribute itself to some later, innocent acquire.
    pub fn assert_user_entry_safe() {
        assert_empty(Site::UserEntry);
    }

    /// Which boundary [`assert_empty`] is checking — only used to word the report.
    #[derive(Copy, Clone)]
    enum Site {
        ContextSwitch,
        UserEntry,
    }

    fn assert_empty(site: Site) {
        let cpu = this_cpu();
        let depth = DEPTH[cpu].load(Ordering::Relaxed);
        let floor = FLOOR[cpu].load(Ordering::Relaxed);
        if depth != 0 || floor != 0 {
            report_not_empty(site, depth, floor);
        }
    }

    /// Name a rank for the report. A bare number sends the reader to a table; the name
    /// usually identifies the lock outright.
    fn rank_name(rank: u8) -> &'static str {
        match rank {
            10 => "Sched",
            30 => "HandleTable",
            40 => "KernelObject (AddressSpace/Namespace/FileObject)",
            50 => "Registry (allocates while held)",
            60 => "SlabCache",
            62 => "Buddy",
            64 => "KernelPmlTemplate",
            66 => "KernelVmap",
            70 => "Serial",
            72 => "Klog",
            85 => "TlbShootdown",
            90 => "Leaf",
            _ => "unknown",
        }
    }

    /// Latch the first report. A violation panic prints, which takes `SERIAL` — and the
    /// panic path may itself acquire locks. A second caller simply returns: the first panic
    /// is already tearing the kernel down, and spinning here instead would replace a
    /// diagnosable panic with a hang.
    fn latch() -> bool {
        REPORTING.swap(1, Ordering::SeqCst) == 0
    }

    /// Report the shootdown lock taken with something already held — a breach of its caller
    /// contract rather than of the rank order, hence its own message.
    #[cold]
    #[inline(never)]
    fn report_contract(held: u8) {
        if !latch() {
            return;
        }
        panic!(
            "lock contract violation: took the TLB-shootdown lock while holding {} \
             (rank {}) — it is held with interrupts enabled, so its contract is that no \
             other lock is held when it is taken; see kernel/docs/lock-ordering.md § F1",
            rank_name(held),
            held
        );
    }

    #[cold]
    #[inline(never)]
    fn report(held: u8, taking: u8) {
        if !latch() {
            return;
        }
        panic!(
            "lock-order violation: acquiring {} (rank {}) while holding {} (rank {}) \
             — see kernel/docs/lock-ordering.md",
            rank_name(taking),
            taking,
            rank_name(held),
            held
        );
    }

    /// Report an interrupt handler returning with locks it did not release.
    #[cold]
    #[inline(never)]
    fn report_leak(entry_depth: usize, depth: usize) {
        if !latch() {
            return;
        }
        let leaked = depth.wrapping_sub(entry_depth);
        panic!(
            "interrupt handler returned holding {leaked} lock(s) it did not release \
             (depth {depth} at exit, {entry_depth} at entry) — an interrupt handler must \
             release everything it takes before the interrupted context resumes",
        );
    }

    /// Report a boundary reached with locks held or an interrupt scope open.
    #[cold]
    #[inline(never)]
    fn report_not_empty(site: Site, depth: usize, floor: usize) {
        if !latch() {
            return;
        }
        let what = match site {
            Site::ContextSwitch => {
                "context switch — switching away mid-critical-section strands every spinner, \
                 and breaks the tracker's per-CPU model"
            }
            Site::UserEntry => {
                "ring-3 entry — the calling thread was in user mode and can hold nothing, so \
                 this is a leaked acquire from earlier on this CPU"
            }
        };
        panic!(
            "{what}: reached with {depth} lock(s) held (scope floor {floor}); \
             see kernel/docs/lock-ordering.md",
        );
    }
}

/// Host tests for the scope arithmetic.
///
/// The *tracker* is compiled out under `cfg(test)` (module docs § Not under host
/// `cfg(test)`) because its per-CPU state is invalid there. The arithmetic it performs is
/// not — so it is reproduced here over a plain local struct, with no atomics, no CPU index
/// and no interrupt masking, and exercised directly. This is the part that got the design
/// wrong the first time; it should not go untested just because the plumbing cannot run on
/// the host.
#[cfg(test)]
mod tests {
    use super::LockRank;

    const MAX_HELD: usize = 8;

    /// One CPU's tracker state, with the same update rules as `tracker`.
    #[derive(Default)]
    struct State {
        held: [u8; MAX_HELD],
        depth: usize,
        floor: usize,
    }

    impl State {
        /// `Ok(())`, or `Err(held_rank)` for the entry that the acquire inverted against.
        fn acquired(&mut self, rank: LockRank) -> Result<(), u8> {
            let mut violation = None;
            for i in self.floor..self.depth.min(MAX_HELD) {
                let held = self.held[i];
                if held != 0 && (rank as u8) <= held {
                    violation = Some(held);
                    break;
                }
            }
            if self.depth < MAX_HELD {
                self.held[self.depth] = rank as u8;
            }
            self.depth += 1;
            match violation {
                Some(h) => Err(h),
                None => Ok(()),
            }
        }

        fn releasing(&mut self) {
            if self.depth > self.floor {
                self.depth -= 1;
                if self.depth < MAX_HELD {
                    self.held[self.depth] = 0;
                }
            }
        }

        /// Returns the floor to restore at the matching exit.
        fn enter_interrupt(&mut self) -> usize {
            let saved = self.floor;
            self.floor = self.depth;
            saved
        }

        fn exit_interrupt(&mut self, saved_floor: usize) {
            self.floor = saved_floor;
        }

        fn switch_safe(&self) -> bool {
            self.depth == 0 && self.floor == 0
        }
    }

    #[test]
    fn ascending_ranks_nest_freely() {
        let mut s = State::default();
        assert_eq!(s.acquired(LockRank::Sched), Ok(()));
        assert_eq!(s.acquired(LockRank::KernelObject), Ok(()));
        assert_eq!(s.acquired(LockRank::Buddy), Ok(()));
        assert_eq!(s.depth, 3);
    }

    #[test]
    fn inversion_is_caught_and_names_the_held_lock() {
        let mut s = State::default();
        s.acquired(LockRank::Buddy).unwrap();
        assert_eq!(
            s.acquired(LockRank::Sched),
            Err(LockRank::Buddy as u8),
            "taking the outermost lock under an allocator lock is the F12/F1 shape"
        );
    }

    #[test]
    fn same_rank_nesting_is_caught() {
        let mut s = State::default();
        s.acquired(LockRank::KernelObject).unwrap();
        assert_eq!(s.acquired(LockRank::KernelObject), Err(LockRank::KernelObject as u8));
    }

    /// The regression the whole change exists for: a timer tick landing on a thread that
    /// holds an allocator lock, and taking `SCHED`. Flat, that is the exact panic that got
    /// D4 withdrawn; scoped, it passes.
    #[test]
    fn interrupt_scope_permits_sched_under_a_held_allocator_lock() {
        let mut s = State::default();
        s.acquired(LockRank::Buddy).unwrap(); // thread context, interrupts enabled

        let saved = s.enter_interrupt(); // timer tick
        assert_eq!(s.acquired(LockRank::Sched), Ok(()), "the handler starts from an empty view");
        s.releasing();
        s.exit_interrupt(saved);

        // The interrupted context's hold survived the handler untouched, and is enforced
        // again now that it has resumed.
        assert_eq!(s.depth, 1);
        assert_eq!(s.acquired(LockRank::Sched), Err(LockRank::Buddy as u8));
    }

    #[test]
    fn a_handler_is_still_ordered_against_its_own_acquisitions() {
        let mut s = State::default();
        s.acquired(LockRank::Buddy).unwrap();
        let saved = s.enter_interrupt();
        s.acquired(LockRank::Leaf).unwrap();
        // Inside the handler the order still applies in full.
        assert_eq!(s.acquired(LockRank::Sched), Err(LockRank::Leaf as u8));
        s.releasing();
        s.releasing();
        s.exit_interrupt(saved);
        assert_eq!(s.depth, 1);
    }

    #[test]
    fn nested_interrupt_scopes_restore_in_order() {
        let mut s = State::default();
        s.acquired(LockRank::HandleTable).unwrap();
        let outer = s.enter_interrupt();
        s.acquired(LockRank::Serial).unwrap();
        let inner = s.enter_interrupt();
        assert_eq!(s.floor, 2);
        assert_eq!(s.acquired(LockRank::Sched), Ok(()), "innermost handler sees nothing held");
        s.releasing();
        s.exit_interrupt(inner);
        assert_eq!(s.floor, 1);
        s.releasing();
        s.exit_interrupt(outer);
        assert_eq!((s.floor, s.depth), (0, 1));
    }

    #[test]
    fn a_handler_cannot_pop_the_interrupted_contexts_holds() {
        let mut s = State::default();
        s.acquired(LockRank::Buddy).unwrap();
        let saved = s.enter_interrupt();
        // An unbalanced release inside the handler (a bug) must not eat the Buddy entry.
        s.releasing();
        s.releasing();
        s.exit_interrupt(saved);
        assert_eq!(s.depth, 1, "the interrupted context still holds Buddy");
        assert_eq!(s.held[0], LockRank::Buddy as u8);
    }

    #[test]
    fn overflow_degrades_to_unchecked_rather_than_to_a_false_report() {
        let mut s = State::default();
        // Descend past MAX_HELD with strictly increasing ranks, then keep going.
        for r in [
            LockRank::Sched,
            LockRank::HandleTable,
            LockRank::KernelObject,
            LockRank::Registry,
            LockRank::SlabCache,
            LockRank::Buddy,
            LockRank::KernelPmlTemplate,
            LockRank::KernelVmap,
        ] {
            s.acquired(r).unwrap();
        }
        assert_eq!(s.depth, MAX_HELD);
        // Past the recorded window: counted, unrecorded, and — the point — not reported.
        assert_eq!(s.acquired(LockRank::Serial), Ok(()));
        assert_eq!(s.depth, MAX_HELD + 1);
        s.releasing();
        assert_eq!(s.depth, MAX_HELD);
    }

    #[test]
    fn switch_safety_holds_only_with_an_empty_stack_and_no_open_scope() {
        let mut s = State::default();
        assert!(s.switch_safe(), "an idle CPU is switchable");

        s.acquired(LockRank::Sched).unwrap();
        assert!(!s.switch_safe(), "SCHED must be released before the switch");
        s.releasing();
        assert!(s.switch_safe());

        // The preemptive path: a tick on an idle CPU opens a scope at depth 0, takes and
        // drops SCHED, and switches — still safe, which is why the assert can be this
        // strict.
        let saved = s.enter_interrupt();
        s.acquired(LockRank::Sched).unwrap();
        s.releasing();
        assert!(s.switch_safe());
        s.exit_interrupt(saved);
    }
}
