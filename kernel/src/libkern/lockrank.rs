//! Lock ranks, and the debug-build tracker that enforces them.
//!
//! `kernel/docs/lock-ordering.md` defines a total order over every long-lived lock: a lock may
//! only be taken while holding locks of *strictly higher* rank (smaller number). On acquire the
//! new lock's rank must be **strictly greater** than every rank held *in the current interrupt
//! scope*; that catches inversions and same-rank nesting, with no same-rank exception.
//!
//! ### Only an acquisition that waits is ordered
//!
//! **A `try_lock` is recorded but not checked** ([`Taken::WithoutWaiting`]): a deadlock is a
//! cycle of threads each *waiting*, and a `try_lock` never waits. Every `lock()` is checked
//! against everything held, `try_lock` holds included, so taking `A` without waiting and then
//! waiting for `B` below it is reported at the wait.
//!
//! **The rule keys on the call.** A `try_lock` retried in a loop *is* waiting, and the tracker
//! cannot tell: a bounded retry that gives up (`fbcon::with_console`) can delay but not
//! deadlock; an unbounded one must be a `lock()`.
//!
//! Checking `try_lock` would break the panic path, which tees into the log ring and the
//! framebuffer console with `try_lock` from whatever context faulted: a panic under a `Leaf`
//! lock reported `lock-order violation: acquiring Klog (rank 72) while holding Leaf (rank 90)`
//! in place of its own message. The shootdown lock's contract still binds both kinds of
//! acquisition: its hazard is holding other locks while it spins.
//!
//! ## Interrupt scopes
//!
//! **The acquisition order restarts at every interrupt entry**; the tracker is unsound without
//! it. A plain [`SpinLock`] is held with interrupts *enabled*, so a timer tick or an IPI
//! routinely lands on a thread holding, say, `Buddy` or `HandleTable`, and the handler
//! legitimately takes `SCHED` (the *top* rank). The handler releases everything it takes before
//! the interrupted section resumes, so the two nestings never interleave.
//!
//! [`SpinLock`]: crate::libkern::SpinLock
//!
//! Each CPU keeps a **floor** alongside its held-rank stack. [`enter_interrupt`] raises it to
//! the current depth and its guard lowers it at the handler's return; the check only looks at
//! entries at or above the floor. A handler starts from an empty view and is checked in full
//! against *its own* acquisitions, while the interrupted context's holds stay recorded (and
//! enforced once it resumes).
//!
//! Every interrupt entry must open a scope; missing one reports phantom inversions from that
//! vector. The `irq_dispatcher!` macro in `arch/x86_64/idt.rs` is the only way to define an
//! interrupt dispatcher and opens the scope itself, and `cargo xtask check-irq-scope` fails the
//! build if a naked entry stub calls anything else.
//!
//! ## Per-CPU, not per-thread
//!
//! The held-rank stack is per-CPU, which is sound because a lock holder cannot migrate: a plain
//! `SpinLock` section is a no-preemption region and an `IrqSpinLock` masks interrupts. A context
//! switch moves threads between CPUs underneath the per-CPU state, so a thread may only be
//! switched away with **nothing held and no scope open** (blocking with a lock held is
//! forbidden). [`assert_switch_safe`] checks it at the one choke point every switch passes
//! through ([`crate::sched::switch_into`]).
//!
//! ## Cost, and not under host `cfg(test)`
//!
//! Debug builds only (`cfg(debug_assertions)`). The tracker takes no lock and allocates nothing
//! (it would deadlock against what it instruments): a fixed per-CPU array of atomics, mirroring
//! `sched`'s `PREEMPT_OFF`. It is also compiled out for host unit tests, as a correctness
//! requirement: `cargo test` runs tests on **many OS threads**, all reporting the same
//! `current_cpu()`, so the per-CPU stack would be shared by unrelated threads and read as
//! inversions.
//!
//! The *rule* is unit-tested host-side through [`classify`], which the tracker calls; the
//! *bookkeeping* against a plain local state struct (see the tests at the bottom).

/// Where a lock sits in the acquisition order. **Smaller is acquired first** — a lock may
/// only be taken while every rank already held is strictly smaller.
///
/// The numbering is sparse so a new lock can be slotted between two existing ranks without
/// renumbering. Mirrors the table in `kernel/docs/lock-ordering.md`; read them together.
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
    /// not leaves. Taken top-level and never from interrupt context.
    Registry = 50,
    /// Rank 6a — a slab cache's free list. Above the buddy because a slab refill calls
    /// into it.
    SlabCache = 60,
    /// Rank 6b — the buddy frame allocator.
    Buddy = 62,
    /// Rank 6c — the kernel-half PML4 template.
    KernelPmlTemplate = 64,
    /// Rank 6d — the kernel vmap.
    KernelVmap = 66,
    /// Rank 7 — the serial port.
    Serial = 70,
    /// Below the serial port: the kernel log ring, teed from inside the serial
    /// `write_str` **while `SERIAL` is held**, which fixes its rank here rather than among the
    /// leaves. (Its `try_lock` is for re-entry from a fault that strikes mid-push.)
    Klog = 72,
    /// Below the log ring, as it is below the serial port: the framebuffer console
    /// (`crate::fbcon`) is teed from the serial `write_str` with `SERIAL` held, after `KLOG` has
    /// been released. Its `try_lock` is for faults that strike mid-paint.
    Fbcon = 74,
    /// The TLB-shootdown serialiser (`tlb::LOCK`), held with **interrupts enabled**: an
    /// initiator spinning for acknowledgements must keep taking incoming shootdown IPIs, or two
    /// initiators deadlock (`kernel/docs/lock-ordering.md` § F1).
    ///
    /// Interrupt work runs beneath it (a DPC drain at an interrupt tail, a timer tick taking
    /// `SCHED`); interrupt scopes check that work in its own scope and the lock in the
    /// initiator's.
    ///
    /// It also carries a stricter contract the ranking alone would not catch: the caller
    /// must hold **no other lock** when taking it. [`acquired`] checks that separately.
    TlbShootdown = 85,
    /// Interrupt-side and bottom-of-order locks, taking nothing while held: the DPC queue,
    /// the entropy pool, the console input buffer, the AHCI port lock, the completed-IRP
    /// reclaim list.
    Leaf = 90,
}

/// How a lock was acquired, which decides whether the order applies to it at all.
///
/// See the module docs § Only an acquisition that waits is ordered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    /// `lock()`: spun until the lock was free. The only kind of acquisition that can be part
    /// of a deadlock, so the only kind checked against the order.
    Waiting,
    /// `try_lock()`: taken because it was free at that instant, or not taken. Recorded as held,
    /// so what is waited for afterwards is checked against it; never itself checked.
    WithoutWaiting,
}

/// What acquiring `rank` means, given what this CPU already holds **in the current
/// interrupt scope**.
#[derive(Debug, PartialEq, Eq)]
pub enum Acquisition {
    /// Nothing broken.
    Ok,
    /// `tlb::LOCK`'s contract, which is stricter than its rank: *nothing* may be held when
    /// it is taken. The rank cannot express this — `TlbShootdown` is 85, so taking it under
    /// `Sched` (10) is not an inversion — so only this branch catches it.
    ContractBroken { held: u8 },
    /// An ordinary rank inversion: `rank` is not strictly above something already held.
    Inverted { held: u8 },
}

/// The acquisition decision, as a function of its inputs.
///
/// **Split out so the host tests exercise the tracker's own logic rather than a
/// reimplementation of it** — a copy had silently lost the `ContractBroken` branch, so the
/// one rule the rank order cannot express had no test.
///
/// `held` is the scope's slice — entries below the floor belong to an interrupted context
/// and are not the caller's to violate. `taken` is how the lock was acquired: only a
/// [`Taken::Waiting`] acquisition can invert the order.
pub fn classify(rank: LockRank, held: &[u8], taken: Taken) -> Acquisition {
    for &h in held {
        if h == 0 {
            continue;
        }
        if matches!(rank, LockRank::TlbShootdown) {
            return Acquisition::ContractBroken { held: h };
        }
        if taken == Taken::Waiting && (rank as u8) <= h {
            return Acquisition::Inverted { held: h };
        }
    }
    Acquisition::Ok
}

/// Debug-build enforcement. Compiled out in release builds, and in host unit tests (the
/// per-CPU model does not hold there).
#[cfg(all(debug_assertions, not(test)))]
pub use tracker::{IrqScope, acquired, assert_switch_safe, assert_user_entry_safe, enter_interrupt, releasing};

/// Release builds: every hook vanishes. Host tests: each thread counts the locks it holds.
#[cfg(not(all(debug_assertions, not(test))))]
mod inert {
    use super::LockRank;

    /// Release builds and host tests: an empty guard, so the entry points type-check
    /// identically whether or not the tracker is compiled in.
    pub struct IrqScope;

    #[inline(always)]
    pub fn acquired(_rank: LockRank, _taken: super::Taken) {
        #[cfg(test)]
        HELD.with(|h| h.set(h.get() + 1));
    }
    #[inline(always)]
    pub fn releasing(_rank: LockRank) {
        #[cfg(test)]
        HELD.with(|h| h.set(h.get() - 1));
    }

    #[cfg(test)]
    std::thread_local! {
        static HELD: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    }

    /// The locks this thread holds: libtest gives each test a thread of its own.
    #[cfg(test)]
    pub fn held_by_this_thread() -> usize {
        HELD.with(core::cell::Cell::get)
    }
    #[inline(always)]
    pub fn enter_interrupt() -> IrqScope {
        IrqScope
    }
    #[inline(always)]
    pub fn assert_switch_safe() {}
    #[inline(always)]
    pub fn assert_user_entry_safe() {}
}

#[cfg(test)]
pub use inert::held_by_this_thread;
#[cfg(not(all(debug_assertions, not(test))))]
pub use inert::{IrqScope, acquired, assert_switch_safe, assert_user_entry_safe, enter_interrupt, releasing};

#[cfg(all(debug_assertions, not(test)))]
mod tracker {
    use super::LockRank;
    use crate::arch::Cpu;
    use crate::arch::cpu::ArchCpu;
    use crate::arch::smp::MAX_CPUS;
    use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

    /// Mask interrupts across a tracker update, returning the prior state.
    /// The per-CPU depth is a **read-modify-write**, and a plain `SpinLock` acquire leaves
    /// interrupts *enabled* (`preempt_disable` stops migration, not interruption). An IRQ
    /// landing between the read and the write does its own acquire/release, and the outer
    /// write then clobbers the depth with a stale value, corrupting the stack.
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

    /// Deepest lock nesting the tracker records per CPU. Past this it stops recording rather
    /// than panicking, so a legitimately deep path degrades to "unchecked".
    const MAX_HELD: usize = 8;

    /// Ranks currently held on each CPU, innermost last.
    #[allow(clippy::declare_interior_mutable_const, reason = "an array initializer, copied for each element")]
    const ZERO: AtomicU8 = AtomicU8::new(0);
    #[allow(clippy::declare_interior_mutable_const, reason = "an array initializer, copied for each CPU")]
    const EMPTY: [AtomicU8; MAX_HELD] = [ZERO; MAX_HELD];
    static HELD: [[AtomicU8; MAX_HELD]; MAX_CPUS] = [EMPTY; MAX_CPUS];

    /// How many entries of `HELD[cpu]` are live. May exceed `MAX_HELD`; the excess is
    /// counted but not recorded (see `MAX_HELD`).
    #[allow(clippy::declare_interior_mutable_const, reason = "an array initializer, copied for each CPU")]
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
        // Out of range (very early boot, before SMP is up) folds onto CPU 0, where that code
        // runs.
        if c < MAX_CPUS { c } else { 0 }
    }

    /// Record that `rank` has just been acquired on this CPU, and panic if doing so
    /// inverted the order — which only a [`Taken::Waiting`](super::Taken::Waiting)
    /// acquisition can.
    ///
    /// Called *after* the lock is held, so the report can name a real acquisition rather
    /// than a speculative one — and so a failed `try_lock` records nothing.
    pub fn acquired(rank: LockRank, taken: super::Taken) {
        let irq = mask();
        let cpu = this_cpu();
        let floor = FLOOR[cpu].load(Ordering::Relaxed);
        let depth = DEPTH[cpu].load(Ordering::Relaxed);
        // Check against everything held **in this interrupt scope**: entries below the floor
        // belong to a context this handler interrupted. Decide with `super::classify`, the same
        // function the host tests call; find the violation under the mask but report outside it,
        // because `report` panics and the panic path prints.
        let mut scope = [0u8; MAX_HELD];
        let n = depth.min(MAX_HELD).saturating_sub(floor);
        for k in 0..n {
            scope[k] = HELD[cpu][floor + k].load(Ordering::Relaxed);
        }
        let (mut violation, mut contract) = (None, None);
        match super::classify(rank, &scope[..n], taken) {
            super::Acquisition::Ok => {}
            super::Acquisition::ContractBroken { held } => contract = Some(held),
            super::Acquisition::Inverted { held } => violation = Some(held),
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
        // Never pop below the floor: those entries belong to the interrupted context. `depth == 0`
        // is the same case at the outermost scope (a lock acquired before the tracker saw this
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
    /// interrupted context stay recorded but stop being checked against. The guard restores
    /// the previous scope.
    ///
    /// Called at every interrupt entry (module docs § Interrupt scopes; `irq_dispatcher!` in
    /// `arch/x86_64/idt.rs` enforces "every"). Interrupts are already masked here (the IDT gate
    /// clears `IF` and no dispatcher re-enables), so the update needs no mask of its own.
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
            // (the timer tail, a device-IRQ completion) can resume on a *different* CPU. That is
            // consistent because a switch requires an empty stack and no open scope on both ends
            // (`assert_switch_safe`), so the saved floor and entry depth are then zero.
            let cpu = this_cpu();
            let depth = DEPTH[cpu].load(Ordering::Relaxed);
            if depth != self.entry_depth {
                report_leak(self.entry_depth, depth);
            }
            FLOOR[cpu].store(self.saved_floor, Ordering::Relaxed);
        }
    }

    /// Assert that this CPU is safe to context-switch on: nothing held, no interrupt scope
    /// open. Called from [`crate::sched::switch_into`] once `SCHED` has been released.
    ///
    /// This is the per-CPU model's soundness precondition (module docs § Per-CPU, not
    /// per-thread): it lets the tracker treat "this CPU holds" and "this thread holds" as the
    /// same statement.
    pub fn assert_switch_safe() {
        assert_empty(Site::ContextSwitch);
    }

    /// Assert that this CPU holds nothing as a thread enters the kernel from ring 3.
    ///
    /// A `syscall` is **not** an interrupt and gets no scope: the acquisition order *begins*
    /// here, since the caller was in user mode and can hold no kernel lock. An unbalanced
    /// acquire/release would otherwise accumulate silently and misattribute itself to a later,
    /// innocent acquire.
    pub fn assert_user_entry_safe() {
        assert_empty(Site::UserEntry);
    }

    /// Which boundary [`assert_empty`] is checking; only used to word the report.
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

    /// Name a rank for the report; the name usually identifies the lock outright.
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
            74 => "Fbcon",
            85 => "TlbShootdown",
            90 => "Leaf",
            _ => "unknown",
        }
    }

    /// Latch the first report. A violation panic prints, which takes `SERIAL`, and the panic
    /// path may itself acquire locks. A second caller returns: the first panic is already
    /// tearing the kernel down, and spinning would replace a diagnosable panic with a hang.
    fn latch() -> bool {
        REPORTING.swap(1, Ordering::SeqCst) == 0
    }

    /// Report the shootdown lock taken with something already held: a breach of its caller
    /// contract rather than of the rank order.
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

/// Host tests for the scope arithmetic. The *tracker* is compiled out under `cfg(test)`, so the
/// tests call [`classify`], the function the tracker calls, and model only the bookkeeping (stack,
/// depth, floor) in `State`. Add a new rule to `classify`; a rule in `State` is invisible to the
/// kernel.
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
        /// `Ok(())`, or `Err(held_rank)` for the entry the acquire broke against.
        fn acquired(&mut self, rank: LockRank) -> Result<(), u8> {
            self.taken(rank, super::Taken::Waiting)
        }

        fn taken(&mut self, rank: LockRank, taken: super::Taken) -> Result<(), u8> {
            let hi = self.depth.min(MAX_HELD);
            let scope = if hi > self.floor { &self.held[self.floor..hi] } else { &[][..] };
            let verdict = super::classify(rank, scope, taken);
            if self.depth < MAX_HELD {
                self.held[self.depth] = rank as u8;
            }
            self.depth += 1;
            match verdict {
                super::Acquisition::Ok => Ok(()),
                super::Acquisition::ContractBroken { held } | super::Acquisition::Inverted { held } => Err(held),
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

    /// `tlb::LOCK`'s contract is stricter than its rank: `TlbShootdown` is 85, so taking it under
    /// `Sched` (10) is not an inversion, yet *nothing* may be held (F1).
    #[test]
    fn the_shootdown_lock_refuses_any_held_lock_even_one_it_outranks() {
        use super::{Acquisition, LockRank, Taken::Waiting, classify};

        assert_eq!(classify(LockRank::TlbShootdown, &[], Waiting), Acquisition::Ok);
        assert_eq!(classify(LockRank::TlbShootdown, &[0, 0], Waiting), Acquisition::Ok);

        let sched = LockRank::Sched as u8;
        assert!(
            (LockRank::TlbShootdown as u8) > sched,
            "the premise: this is not a rank inversion, which is why the branch exists"
        );
        assert_eq!(classify(LockRank::TlbShootdown, &[sched], Waiting), Acquisition::ContractBroken { held: sched });

        assert_eq!(classify(LockRank::Sched, &[sched], Waiting), Acquisition::Inverted { held: sched });
    }

    /// The panic path's shape: a driver panics holding its `Leaf` lock, and the panic tees
    /// into the log ring (rank 72) with `try_lock`.
    #[test]
    fn an_acquisition_that_does_not_wait_is_not_ordered() {
        use super::{Acquisition, LockRank, Taken, classify};
        let leaf = LockRank::Leaf as u8;
        assert_eq!(
            classify(LockRank::Klog, &[leaf], Taken::WithoutWaiting),
            Acquisition::Ok,
            "a try_lock cannot be an edge of a deadlock cycle"
        );
        assert_eq!(
            classify(LockRank::Klog, &[leaf], Taken::Waiting),
            Acquisition::Inverted { held: leaf },
            "the same acquisition, waiting, still is"
        );
        let klog = LockRank::Klog as u8;
        assert_eq!(
            classify(LockRank::Klog, &[klog], Taken::WithoutWaiting),
            Acquisition::Ok,
            "nor is a same-rank try_lock"
        );
        let sched = LockRank::Sched as u8;
        assert_eq!(
            classify(LockRank::TlbShootdown, &[sched], Taken::WithoutWaiting),
            Acquisition::ContractBroken { held: sched }
        );
    }

    /// Not checking a `try_lock` must not mean not *recording* it: the deadlock it could be
    /// half of is completed by a later wait, and that wait is checked against it.
    #[test]
    fn a_lock_taken_without_waiting_still_orders_what_is_waited_for_under_it() {
        use super::Taken::WithoutWaiting;
        let mut s = State::default();
        s.acquired(LockRank::Leaf).unwrap();
        assert_eq!(s.taken(LockRank::Klog, WithoutWaiting), Ok(()));
        assert_eq!(s.depth, 2, "recorded");
        assert_eq!(
            s.acquired(LockRank::Serial),
            Err(LockRank::Leaf as u8),
            "waiting for Serial under either hold is still an inversion"
        );
        s.releasing();
        s.releasing();
        s.releasing();
        s.taken(LockRank::Klog, WithoutWaiting).unwrap();
        assert_eq!(s.acquired(LockRank::Serial), Err(LockRank::Klog as u8));
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

    /// A timer tick landing on a thread that holds an allocator lock and taking `SCHED`:
    /// flat, a false inversion; scoped, it passes.
    #[test]
    fn interrupt_scope_permits_sched_under_a_held_allocator_lock() {
        let mut s = State::default();
        s.acquired(LockRank::Buddy).unwrap(); // thread context, interrupts enabled

        let saved = s.enter_interrupt(); // timer tick
        assert_eq!(s.acquired(LockRank::Sched), Ok(()), "the handler starts from an empty view");
        s.releasing();
        s.exit_interrupt(saved);

        assert_eq!(s.depth, 1);
        assert_eq!(s.acquired(LockRank::Sched), Err(LockRank::Buddy as u8));
    }

    #[test]
    fn a_handler_is_still_ordered_against_its_own_acquisitions() {
        let mut s = State::default();
        s.acquired(LockRank::Buddy).unwrap();
        let saved = s.enter_interrupt();
        s.acquired(LockRank::Leaf).unwrap();
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

        // A tick on an idle CPU opens a scope at depth 0, takes and drops SCHED, and switches.
        let saved = s.enter_interrupt();
        s.acquired(LockRank::Sched).unwrap();
        s.releasing();
        assert!(s.switch_safe());
        s.exit_interrupt(saved);
    }
}
