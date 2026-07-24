//! The single process-wide kernel handle table and its one-time init.
//!
//! The handle table is **global** — one globally-numbered segmented table
//! with a per-entry `owner_pid` checked on every lookup (per-process tables
//! are rejected; see `docs/rationale/rejected-approaches.md`). This module
//! owns the single instance.
//!
//! It is stored inline (no `Box::leak` — forbidden by `kernel/CLAUDE.md`) in
//! a once-init cell. [`init`] runs exactly once in early boot after the heap
//! is up; [`get`] returns a shared `&'static HandleTable` whose `&self`
//! methods carry their own interior synchronisation (per-entry seqlock for
//! lookups + the rank-3 alloc lock). No coarse lock wraps the table, so
//! lookups stay lock-free.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, Ordering};

use super::table::{HandleError, HandleTable};

const UNINIT: u8 = 0;
const INITIALISING: u8 = 1;
const READY: u8 = 2;

struct GlobalTable {
    state: AtomicU8,
    slot: UnsafeCell<MaybeUninit<HandleTable>>,
}

// SAFETY: the inner `HandleTable` is published only after `state` reaches
// `READY` with a `Release` store; `get` reads `state` with `Acquire`, so any
// reader that observes `READY` also observes the fully-initialised table.
// After `READY` the table is never mutated through the cell (its own `&self`
// methods provide all interior mutability) and is never moved or dropped for
// the kernel's lifetime, so handing out `&'static` shared borrows is sound.
// `HandleTable` is itself `Sync`; the `UnsafeCell` only mediates the one-time
// initialisation, which single-CPU boot cannot race.
unsafe impl Sync for GlobalTable {}

static GLOBAL: GlobalTable = GlobalTable {
    state: AtomicU8::new(UNINIT),
    slot: UnsafeCell::new(MaybeUninit::uninit()),
};

/// Initialise the global handle table exactly once. Must run **after the
/// heap is up** (it eagerly allocates segment 0), **after** the entropy
/// subsystem is keyed ([`crate::entropy::init`] — the free-list shuffle seed is
/// drawn from the CSPRNG), and **before** any userspace can issue a handle
/// syscall. Returns `Err` if the table allocation fails.
pub fn init() -> Result<(), HandleError> {
    // Single-CPU boot can't actually race, but a CAS makes the
    // initialise-once invariant explicit (and SMP-ready).
    if GLOBAL
        .state
        .compare_exchange(UNINIT, INITIALISING, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        debug_assert!(false, "handle::global::init called more than once");
        return Ok(());
    }
    // Seed the per-segment free-list shuffle from the CSPRNG (keyed by
    // `entropy::init`, which boot runs first). The seed only randomizes free-list
    // scan order — defence-in-depth on handle-value unpredictability, atop the
    // owner-PID check + per-slot generation counter that actually unforge handles.
    let table = HandleTable::try_new(crate::entropy::seed_u64())?;
    // SAFETY: we won the `UNINIT -> INITIALISING` transition, so we have
    // exclusive access to the cell; no reader can be in `get` because `state`
    // is not yet `READY`.
    unsafe {
        (*GLOBAL.slot.get()).write(table);
    }
    GLOBAL.state.store(READY, Ordering::Release);
    Ok(())
}

/// The global handle table. Panics in debug builds if called before [`init`].
pub fn get() -> &'static HandleTable {
    debug_assert_eq!(
        GLOBAL.state.load(Ordering::Acquire),
        READY,
        "handle::global::get before init",
    );
    // SAFETY: `READY` (observed with `Acquire`) was published with a `Release`
    // store after the table was fully written; the table is never moved or
    // dropped for the kernel's lifetime, so a `'static` shared borrow is
    // sound. See the `Sync` impl above.
    unsafe { (*GLOBAL.slot.get()).assume_init_ref() }
}

/// Handles closed per batch by [`close_all_owned_by`].
///
/// Each entry is a pointer + a type tag, so the batch is ~32 bytes on the sweeping
/// thread's kernel stack. Small enough to be free, large enough that a process with a
/// handful of handles is reclaimed in one pass.
const SWEEP_BATCH: usize = 16;

/// Close **every** handle owned by `pid` and release the objects they pinned.
///
/// This is what makes a process's handles die with it. Without it a dead process's
/// entries persist — the table is global with a per-entry `owner_pid`, and nothing else
/// sweeps it — pinning every object they reference. The visible consequence is that the
/// process's end of a pipe never closes, so a peer blocked on it never observes
/// `PeerClosed` (see the decision log, 2026-07-24).
///
/// # Context
///
/// Must run in **thread context, outside `SCHED`, and not in IRQ context**: releasing the
/// references runs object destructors, which reach the rank-6 allocator and — for an IPC
/// endpoint — take rank-1 `SCHED` to wake blocked receivers. Holding rank-1 or rank-3
/// across that would invert the ranking. The sweep therefore works in batches: entries are
/// unlinked under the rank-3 lock, and each batch's references are dropped after that lock
/// is released. Preemption is disabled across the drops for the same reason
/// [`reap_pending`](crate::sched::reap_pending) does it — a descheduled holder of an
/// allocator spinlock starves every CPU spinning on it (F12, decision log 2026-07-21).
///
/// Safe against pid reuse because pids are monotonic and never reused
/// ([`alloc_pid`](crate::sched::alloc_pid)).
pub fn close_all_owned_by(pid: u32) {
    use super::table::{ClosedObject, SweepCursor};
    use crate::object::ObjectRef;

    let mut cursor = SweepCursor::START;
    loop {
        let mut batch: [Option<ClosedObject>; SWEEP_BATCH] = [None; SWEEP_BATCH];
        let (n, more) = get().close_owned_batch(pid, &mut cursor, &mut batch);
        crate::sched::preempt_disable();
        for co in batch[..n].iter().flatten() {
            // SAFETY: each `ClosedObject` carries exactly the one reference its
            // handle-table entry held; the entry is already unlinked, so this
            // accounts for it once and only once.
            drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
        }
        crate::sched::preempt_enable();
        if !more {
            return;
        }
    }
}
