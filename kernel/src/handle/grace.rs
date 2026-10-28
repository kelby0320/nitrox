//! RCU-style grace tracking for deferred handle reclamation.
//!
//! A closed handle's object pointer is nulled at once, but its slot does **not**
//! rejoin the segment freelist until every concurrent lookup that may hold a
//! reference has reached a quiescent state; otherwise a reader that just passed
//! the `entry.object` non-null check (spec step 6) could hold a stale pointer
//! when the slot is recycled.
//!
//! Keyed by a fixed number of *contexts* ([`MAX_CTX`]): `current_ctx_id()` is
//! the calling CPU's id and each CPU writes its own slot. A close waits for
//! every context to be quiescent or to have observed a later epoch. State is
//! atomic, the reader hot path lock-free, and reclamation bookkeeping runs under
//! the rank-3 lock.

use core::sync::atomic::{AtomicU64, Ordering, fence};

/// Distinct read-side contexts the tracker can tell apart (one per CPU).
pub const MAX_CTX: usize = 256;

/// High bit of `ctx_observed[i]`. Set: context `i` is quiescent. Clear: the low
/// 63 bits are the epoch it entered its critical section under.
const QUIESCED_BIT: u64 = 1 << 63;

/// Quiescent at epoch 0.
const INITIAL_OBSERVED: u64 = QUIESCED_BIT;

/// Lock-free RCU-style grace tracker, one per
/// [`HandleTable`](super::table::HandleTable).
pub(crate) struct GraceTracker {
    /// Bumped when the table starts a new grace period, typically on each drain.
    current_epoch: AtomicU64,
    /// One slot per context: the epoch on entry (clearing [`QUIESCED_BIT`]), the
    /// bit set on exit.
    ctx_observed: [AtomicU64; MAX_CTX],
}

impl GraceTracker {
    /// A tracker with every context quiescent at epoch 0.
    pub(crate) const fn new() -> Self {
        Self { current_epoch: AtomicU64::new(0), ctx_observed: [const { AtomicU64::new(INITIAL_OBSERVED) }; MAX_CTX] }
    }

    /// Enter a read-side critical section (`ctx_id < MAX_CTX`); the guard marks
    /// the context quiescent on drop. The store is `Release` so a reclamation
    /// walk with `Acquire` loads sees it before freeing a deferred close.
    pub(crate) fn enter_read(&self, ctx_id: u32) -> ReadGuard<'_> {
        debug_assert!((ctx_id as usize) < MAX_CTX);
        // The context is this CPU's, so a section that moved CPU would close another's.
        #[cfg(not(test))]
        debug_assert!(
            !<crate::arch::Cpu as crate::arch::cpu::ArchCpu>::interrupts_enabled()
                || crate::sched::preemption_disabled(),
            "a handle read section that can migrate"
        );
        let epoch = self.current_epoch.load(Ordering::Acquire);
        self.ctx_observed[ctx_id as usize].store(epoch, Ordering::Release);
        // Dekker, with `synchronize`'s: either this store is seen there, or this section sees the
        // closer's store. An Acquire fence orders no store before a later load.
        fence(Ordering::SeqCst);
        ReadGuard { tracker: self, ctx_id }
    }

    /// Wait until every read section open now has closed: no reader keeps a pointer it read before
    /// the caller's unpublishing store. Call with no lock held.
    pub(crate) fn synchronize(&self) {
        #[cfg(test)]
        assert_eq!(crate::libkern::lockrank::held_by_this_thread(), 0, "a grace wait with a lock held");
        let epoch = self.unpublished();
        while !self.is_grace_period_past(epoch) {
            yield_for_grace();
        }
    }

    /// Start a new epoch after the caller's unpublishing store, returning the one a reader that
    /// may have missed that store entered under.
    fn unpublished(&self) -> u64 {
        fence(Ordering::SeqCst);
        self.advance_epoch()
    }

    /// Mark a context quiescent without a guard
    /// ([`HandleTable::quiesce`](super::table::HandleTable::quiesce)); no
    /// production path calls it.
    pub(crate) fn mark_quiescent(&self, ctx_id: u32) {
        debug_assert!((ctx_id as usize) < MAX_CTX);
        let v = self.ctx_observed[ctx_id as usize].load(Ordering::Relaxed);
        self.ctx_observed[ctx_id as usize].store(v | QUIESCED_BIT, Ordering::Release);
    }

    /// The current epoch. A close scheduled at it is safe to reclaim once every
    /// context has quiesced at or after it or observed a strictly later epoch.
    pub(crate) fn current_epoch(&self) -> u64 {
        self.current_epoch.load(Ordering::Acquire)
    }

    /// Bump the global epoch, returning the previous value.
    pub(crate) fn advance_epoch(&self) -> u64 {
        self.current_epoch.fetch_add(1, Ordering::Release)
    }

    /// `true` if a close scheduled at `deferred_epoch` is safe to reclaim:
    /// every context is quiescent or re-entered at a strictly later epoch.
    pub(crate) fn is_grace_period_past(&self, deferred_epoch: u64) -> bool {
        (0..MAX_CTX).all(|i| {
            let v = self.ctx_observed[i].load(Ordering::Acquire);
            if v & QUIESCED_BIT != 0 { true } else { (v & !QUIESCED_BIT) > deferred_epoch }
        })
    }
}

/// Backoff while readers finish. Tests yield to the host scheduler so a reader stuck in
/// `read_snapshot` can finish; production emits a `PAUSE`-style hint and the caller spins.
#[cfg(test)]
pub(crate) fn yield_for_grace() {
    std::thread::yield_now();
}

#[cfg(not(test))]
pub(crate) fn yield_for_grace() {
    core::hint::spin_loop();
}

/// Guard from [`GraceTracker::enter_read`]; marks the context quiescent on drop.
pub(crate) struct ReadGuard<'a> {
    tracker: &'a GraceTracker,
    ctx_id: u32,
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        // OR-in the bit, preserving the entry epoch, so a drain racing this
        // drop sees both "last active at epoch X" and "now quiesced".
        let v = self.tracker.ctx_observed[self.ctx_id as usize].load(Ordering::Relaxed);
        self.tracker.ctx_observed[self.ctx_id as usize].store(v | QUIESCED_BIT, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_tracker_is_globally_quiescent() {
        let t = GraceTracker::new();
        assert_eq!(t.current_epoch(), 0);
        assert!(t.is_grace_period_past(0), "no readers, no deferrals — always safe");
    }

    #[test]
    fn read_guard_blocks_reclaim_at_current_epoch() {
        let t = GraceTracker::new();
        let deferred = t.current_epoch();
        let _g = t.enter_read(3);
        assert!(!t.is_grace_period_past(deferred));
    }

    #[test]
    fn dropping_guard_releases_reclaim() {
        let t = GraceTracker::new();
        let deferred = t.current_epoch();
        {
            let _g = t.enter_read(7);
            assert!(!t.is_grace_period_past(deferred));
        }
        assert!(t.is_grace_period_past(deferred));
    }

    #[test]
    fn advancing_epoch_lets_reader_at_new_epoch_clear_old_deferrals() {
        let t = GraceTracker::new();
        let deferred = t.current_epoch();
        let _g_old = t.enter_read(0);
        assert!(!t.is_grace_period_past(deferred));
        drop(_g_old);
        t.advance_epoch();
        let _g_new = t.enter_read(0);
        assert!(t.is_grace_period_past(deferred));
    }

    #[test]
    fn multiple_contexts_independent() {
        let t = GraceTracker::new();
        let deferred = t.current_epoch();
        let g0 = t.enter_read(0);
        let g1 = t.enter_read(1);
        assert!(!t.is_grace_period_past(deferred));
        drop(g0);
        assert!(!t.is_grace_period_past(deferred));
        drop(g1);
        assert!(t.is_grace_period_past(deferred));
    }

    /// **A reader and a closer never both miss the other**: x86 lets a store pass a later load,
    /// so without both fences a close can free what a reader holds.
    #[test]
    fn a_reader_and_a_closer_never_both_miss() {
        use std::sync::atomic::{AtomicBool, AtomicU32};
        const ROUNDS: u32 = if cfg!(miri) { 300 } else { 1_000_000 };
        let t = GraceTracker::new();
        let (start, read, checked) = (AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0));
        let (published, seen, past) = (AtomicBool::new(true), AtomicBool::new(false), AtomicBool::new(false));
        let wait = |a: &AtomicU32, r: u32| {
            while a.load(Ordering::Acquire) != r {
                core::hint::spin_loop();
            }
        };
        let mut both_missed = 0;
        std::thread::scope(|s| {
            s.spawn(|| {
                for r in 1..=ROUNDS {
                    wait(&start, r);
                    let g = t.enter_read(0);
                    seen.store(published.load(Ordering::Acquire), Ordering::Relaxed);
                    wait(&checked, r);
                    drop(g);
                    read.store(r, Ordering::Release);
                }
            });
            s.spawn(|| {
                for r in 1..=ROUNDS {
                    wait(&start, r);
                    published.store(false, Ordering::Release);
                    let epoch = t.unpublished();
                    past.store(t.is_grace_period_past(epoch), Ordering::Relaxed);
                    checked.store(r, Ordering::Release);
                }
            });
            for r in 1..=ROUNDS {
                published.store(true, Ordering::Release);
                start.store(r, Ordering::Release);
                wait(&read, r);
                if seen.load(Ordering::Relaxed) && past.load(Ordering::Relaxed) {
                    both_missed += 1;
                }
            }
        });
        assert_eq!(both_missed, 0, "rounds of {ROUNDS} in which neither saw the other");
    }

    #[test]
    fn mark_quiescent_works_without_guard() {
        let t = GraceTracker::new();
        let _g = t.enter_read(5);
        let deferred = t.current_epoch();
        core::mem::forget(_g);
        assert!(!t.is_grace_period_past(deferred));
        t.mark_quiescent(5);
        assert!(t.is_grace_period_past(deferred));
    }
}
