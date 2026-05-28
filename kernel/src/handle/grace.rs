//! RCU-style grace tracking for deferred handle reclamation.
//!
//! When a handle is closed, the entry's object pointer is nulled
//! immediately, but the slot does **not** return to the segment's
//! freelist until every concurrent lookup that may already hold a
//! reference to the object has reached a quiescent state. Otherwise a
//! reader that has just passed the `entry.object` non-null check (spec
//! step 6) could be holding a stale pointer when the slot is recycled.
//!
//! The mechanism is keyed by a small fixed number of *contexts*
//! ([`MAX_CTX`]). What a context represents is intentionally vague:
//!
//! - Phase 1 (single CPU, no preemption, no IRQs): every operation
//!   runs in context 0. A close is safe to reclaim as soon as
//!   `current_ctx_id()` reports a quiescent state, which the
//!   `ReadGuard::drop` does immediately at the end of every lookup.
//! - SMP (Phase 3): `current_ctx_id()` returns the calling CPU id and
//!   each CPU writes its own slot. A close waits for every CPU to
//!   either be quiescent or to have observed a later epoch.
//! - Per-process (post-Process): `current_ctx_id()` returns the
//!   calling process's `ctx_id`. The mechanism is unchanged.
//!
//! All state is atomic; the tracker takes no lock. Bookkeeping at
//! reclamation time happens under the handle table's rank-3 lock, but
//! the reader hot path is lock-free.

use core::sync::atomic::{AtomicU64, Ordering, fence};

/// Maximum number of distinct read-side contexts the tracker can
/// distinguish. Phase 1 needs 1; SMP needs `num_cpus`; per-process
/// scaling needs more, at which point we revisit the cap.
pub const MAX_CTX: usize = 256;

/// High bit of `ctx_observed[i]`. When set, context `i` is currently
/// quiescent — not inside any read-side critical section. When clear,
/// the low 63 bits hold the epoch the context entered the critical
/// section under.
const QUIESCED_BIT: u64 = 1 << 63;

/// Sentinel initial value for a context's `ctx_observed` slot:
/// quiescent at epoch 0.
const INITIAL_OBSERVED: u64 = QUIESCED_BIT;

/// Lock-free RCU-style grace tracker.
///
/// Constructed once per [`HandleTable`](super::table::HandleTable);
/// outlives every [`ReadGuard`] handed out.
pub(crate) struct GraceTracker {
    /// Monotonically increasing; bumped each time the table decides a
    /// new grace period has started — typically on each
    /// [`drain_expired`](Self::drain_expired) call.
    current_epoch: AtomicU64,
    /// One slot per context. A reader entering a read-side critical
    /// section writes the current epoch (clearing [`QUIESCED_BIT`]);
    /// on exit it sets [`QUIESCED_BIT`]. Reclamation walks the array
    /// to decide whether a deferred close is safe to free.
    ctx_observed: [AtomicU64; MAX_CTX],
}

impl GraceTracker {
    /// Construct a tracker with every context marked quiescent at
    /// epoch 0 and the global epoch at 0.
    pub(crate) const fn new() -> Self {
        // `[const { ... }; N]` is the stable-Rust way to construct an
        // array of non-`Copy` const-initialised values.
        Self {
            current_epoch: AtomicU64::new(0),
            ctx_observed: [const { AtomicU64::new(INITIAL_OBSERVED) }; MAX_CTX],
        }
    }

    /// Enter a read-side critical section. Returns a guard that marks
    /// the context quiescent on drop. The caller's `ctx_id` must be
    /// `< MAX_CTX`.
    ///
    /// The store carries `Release` so a subsequent reclamation walk
    /// that uses `Acquire` loads sees this update before deciding
    /// whether to free a deferred close.
    pub(crate) fn enter_read(&self, ctx_id: u32) -> ReadGuard<'_> {
        debug_assert!((ctx_id as usize) < MAX_CTX);
        let epoch = self.current_epoch.load(Ordering::Acquire);
        // Clear QUIESCED_BIT, record the entered epoch.
        self.ctx_observed[ctx_id as usize].store(epoch, Ordering::Release);
        // Pairs with the Release in `drain_expired` so the read-side
        // section observes the closes scheduled at earlier epochs.
        fence(Ordering::Acquire);
        ReadGuard {
            tracker: self,
            ctx_id,
        }
    }

    /// Mark a context quiescent without holding a guard. Used by
    /// [`HandleTable::quiesce`](super::table::HandleTable::quiesce) at
    /// syscall exit (today: a no-op because every `lookup` already
    /// drops its `ReadGuard`; reserved for non-lookup paths added by
    /// later slices).
    pub(crate) fn mark_quiescent(&self, ctx_id: u32) {
        debug_assert!((ctx_id as usize) < MAX_CTX);
        let v = self.ctx_observed[ctx_id as usize].load(Ordering::Relaxed);
        self.ctx_observed[ctx_id as usize].store(v | QUIESCED_BIT, Ordering::Release);
    }

    /// Snapshot the current epoch. A close scheduled at this epoch
    /// becomes safe to reclaim once every context has either
    /// quiesced at-or-after that epoch or observed a strictly later
    /// epoch.
    pub(crate) fn current_epoch(&self) -> u64 {
        self.current_epoch.load(Ordering::Acquire)
    }

    /// Bump the global epoch, returning the previous value.
    /// Called by [`drain_expired`](super::table::HandleTable::drain_expired)
    /// once it has decided which deferred closes are reclaimable.
    pub(crate) fn advance_epoch(&self) -> u64 {
        self.current_epoch.fetch_add(1, Ordering::Release)
    }

    /// `true` if a close scheduled at `deferred_epoch` is safe to
    /// reclaim: every context has either marked itself quiescent or
    /// re-entered a read-side section at a strictly later epoch.
    pub(crate) fn is_grace_period_past(&self, deferred_epoch: u64) -> bool {
        (0..MAX_CTX).all(|i| {
            let v = self.ctx_observed[i].load(Ordering::Acquire);
            if v & QUIESCED_BIT != 0 {
                true
            } else {
                (v & !QUIESCED_BIT) > deferred_epoch
            }
        })
    }
}

/// RAII guard returned by [`GraceTracker::enter_read`]. Marks the
/// context quiescent on drop.
pub(crate) struct ReadGuard<'a> {
    tracker: &'a GraceTracker,
    ctx_id: u32,
}

impl Drop for ReadGuard<'_> {
    fn drop(&mut self) {
        // OR-in QUIESCED_BIT, preserving the epoch we entered at so a
        // drain that races our drop can still see "this context was
        // last active at epoch X" while also seeing "and has now
        // quiesced".
        let v = self.tracker.ctx_observed[self.ctx_id as usize].load(Ordering::Relaxed);
        self.tracker.ctx_observed[self.ctx_id as usize]
            .store(v | QUIESCED_BIT, Ordering::Release);
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
        // While the guard is held at epoch 0, a close scheduled at
        // epoch 0 is *not* safe.
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
        // Guard dropped → context 7 is quiescent again.
        assert!(t.is_grace_period_past(deferred));
    }

    #[test]
    fn advancing_epoch_lets_reader_at_new_epoch_clear_old_deferrals() {
        let t = GraceTracker::new();
        let deferred = t.current_epoch();
        // First close happens at epoch 0; reader enters at epoch 0.
        let _g_old = t.enter_read(0);
        assert!(!t.is_grace_period_past(deferred));
        drop(_g_old);
        t.advance_epoch();
        // A new reader entering at the bumped epoch must not block
        // reclamation of the old close.
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
        // ctx 1 still active.
        assert!(!t.is_grace_period_past(deferred));
        drop(g1);
        assert!(t.is_grace_period_past(deferred));
    }

    #[test]
    fn mark_quiescent_works_without_guard() {
        let t = GraceTracker::new();
        // Imitate a reader entering and then a non-guard path
        // declaring the context quiescent (e.g. syscall exit on a
        // path that didn't take a guard).
        let _g = t.enter_read(5);
        let deferred = t.current_epoch();
        // Drop happens by core::mem::forget; we manually call mark_quiescent
        core::mem::forget(_g);
        assert!(!t.is_grace_period_past(deferred));
        t.mark_quiescent(5);
        assert!(t.is_grace_period_past(deferred));
    }
}
