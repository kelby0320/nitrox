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
//! On acquire, the new lock's rank must be **strictly greater** than every rank this CPU
//! already holds. That single rule catches inversions (taking a high-ranked lock under a
//! low-ranked one) *and* same-rank nesting, which the ordering document forbids
//! separately. The document also claims one same-rank exception exists but never names it;
//! both places it discusses nesting say "never nested". So the rule is enforced without an
//! exception, and if a legitimate one exists the tracker will find it rather than the
//! next reader having to.
//!
//! ## Per-CPU, not per-thread
//!
//! The held-rank stack is per-CPU, which is sound because a lock holder cannot migrate: a
//! plain `SpinLock` section is a no-preemption region (the F12 fix) and an `IrqSpinLock`
//! masks interrupts. So whatever this CPU holds, this thread holds.
//!
//! Interrupts are the wrinkle. A handler can interrupt a thread holding a rank-4 lock and
//! take its own locks, which is legal — the handler will release them before the
//! interrupted section resumes — but it looks like nesting to a naive tracker. Rather than
//! model interrupt context separately (Linux's lockdep does; it is a lot of machinery),
//! the ranks are a total order with the interrupt-side locks at the **bottom**, so a
//! handler taking one under an interrupted rank-4 holder is an increase and passes.
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
    /// **Not ordered — exempt from the check.** The TLB-shootdown serialiser
    /// (`tlb::LOCK`) is held with **interrupts enabled**, which is the F1 fix (decision
    /// log 2026-07-21): an initiator spinning for acknowledgements must keep taking
    /// incoming shootdown IPIs, or two initiators deadlock.
    ///
    /// That makes it unrankable in a single total order, and the tracker said so in two
    /// steps as this landed: first `Leaf` under `Leaf` (a DPC drain at an interrupt tail
    /// inside a shootdown window), then `Sched` under it (a timer tick doing the same).
    /// Both are correct behaviour — interrupt work genuinely runs beneath this lock, with
    /// its own full ordering — and no single number can express "the order restarts here".
    ///
    /// Its discipline is a *different* invariant, and one the ranking cannot check: the
    /// documented caller contract is **no other lock held** when it is taken (see
    /// `kernel/docs/lock-ordering.md` § F1). So it neither records nor checks.
    ///
    /// The general fix is per-interrupt-context tracking — save the held set at interrupt
    /// entry and restore it at exit, so handlers get a fresh scope — which is what Linux's
    /// lockdep does and what would let this lock be ranked normally. Filed as
    /// `TODO(lockdep-irq-context)`; it needs every interrupt entry/exit hooked, and missing
    /// one would silently corrupt the tracker, so it wants its own change.
    IrqEnabledHold = 80,
    /// Interrupt-side and bottom-of-order locks, taking nothing while held: the DPC queue,
    /// the entropy pool, the console input buffer, the AHCI pending ring. Last, so a
    /// handler may take one while it has interrupted a holder of anything above — including
    /// a shootdown initiator.
    Leaf = 90,
}

/// Debug-build enforcement. Compiled out in release builds, and in host unit tests (see
/// the module docs — the per-CPU model does not hold there).
#[cfg(all(debug_assertions, not(test)))]
pub use tracker::{acquired, releasing};

/// Release builds and host tests: both hooks vanish.
#[cfg(not(all(debug_assertions, not(test))))]
#[inline(always)]
pub fn acquired(_rank: LockRank) {}
/// Release builds and host tests: both hooks vanish.
#[cfg(not(all(debug_assertions, not(test))))]
#[inline(always)]
pub fn releasing(_rank: LockRank) {}

#[cfg(all(debug_assertions, not(test)))]
mod tracker {
    use super::LockRank;
    use crate::arch::smp::MAX_CPUS;
    use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

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
        // Exempt from *ordering*, because interrupt work legitimately nests beneath it and
        // recording it would make every such nesting look like an inversion. But its own
        // documented contract — **no other lock held when it is taken** — is checkable with
        // exactly this machinery, so check that instead of checking nothing. This is the
        // part of the coverage lost to the exemption that can be recovered without
        // per-interrupt-context tracking (`TODO(lockdep-irq-context)`).
        if matches!(rank, LockRank::IrqEnabledHold) {
            let cpu = this_cpu();
            let held = DEPTH[cpu].load(Ordering::Relaxed);
            if held != 0 {
                let inner = HELD[cpu][held.min(MAX_HELD) - 1].load(Ordering::Relaxed);
                report_contract(inner);
            }
            return;
        }
        let cpu = this_cpu();
        let depth = DEPTH[cpu].load(Ordering::Relaxed);
        // Check against everything already held on this CPU.
        let recorded = depth.min(MAX_HELD);
        for i in 0..recorded {
            let held = HELD[cpu][i].load(Ordering::Relaxed);
            if held != 0 && (rank as u8) <= held {
                report(held, rank as u8);
            }
        }
        if depth < MAX_HELD {
            HELD[cpu][depth].store(rank as u8, Ordering::Relaxed);
        }
        DEPTH[cpu].store(depth + 1, Ordering::Relaxed);
    }

    /// Record that `rank` is about to be released on this CPU.
    ///
    /// Called *before* the release, so the stack is already correct if the release path
    /// goes on to take another lock (a guard drop can replay a deferred reschedule, which
    /// takes `SCHED`).
    pub fn releasing(_rank: LockRank) {
        if matches!(_rank, LockRank::IrqEnabledHold) {
            return; // never recorded, so nothing to pop
        }
        let cpu = this_cpu();
        let depth = DEPTH[cpu].load(Ordering::Relaxed);
        if depth == 0 {
            return; // unbalanced release (a lock acquired before the tracker saw this CPU)
        }
        let depth = depth - 1;
        if depth < MAX_HELD {
            HELD[cpu][depth].store(0, Ordering::Relaxed);
        }
        DEPTH[cpu].store(depth, Ordering::Relaxed);
    }

    /// Report an inversion and stop the kernel. Named separately so the `#[cold]` path
    /// stays out of the hot acquire.
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
            80 => "IrqEnabledHold (exempt)",
            90 => "Leaf",
            _ => "unknown",
        }
    }

    /// Report an `IrqEnabledHold` lock taken with something already held — a breach of its
    /// caller contract rather than of the rank order, hence its own message.
    #[cold]
    #[inline(never)]
    fn report_contract(held: u8) {
        if REPORTING.swap(1, Ordering::SeqCst) != 0 {
            return;
        }
        panic!(
            "lock contract violation: took an IrqEnabledHold lock while holding {} \
             (rank {}) — such a lock is held with interrupts enabled, so its contract is \
             that no other lock is held when it is taken; see \
             kernel/docs/lock-ordering.md",
            rank_name(held),
            held
        );
    }

    #[cold]
    #[inline(never)]
    fn report(held: u8, taking: u8) {
        // A violation panic prints, which takes `SERIAL` — and the panic path may itself
        // acquire locks. Latch so a violation *inside* the reporting path does not recurse.
        // A second caller simply returns: the first panic is already tearing the kernel
        // down, and spinning here instead would replace a diagnosable panic with a hang.
        if REPORTING.swap(1, Ordering::SeqCst) != 0 {
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
}
