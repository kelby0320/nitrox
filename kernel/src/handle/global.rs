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

/// Fixed Phase 1 PRNG seed for the table's per-segment free-list shuffles.
///
/// TODO(entropy): seed from the timestamp counter at init, then from
/// RDRAND/RDSEED once the entropy slice lands. A fixed seed is sound — it
/// only affects free-list scan order, not correctness or the unforgeability
/// of handle values (the per-slot generation counters provide that).
const PHASE1_SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// Initialise the global handle table exactly once. Must run **after the
/// heap is up** (it eagerly allocates segment 0) and **before** any
/// userspace can issue a handle syscall. Returns `Err` if the table
/// allocation fails.
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
    let table = HandleTable::try_new(PHASE1_SEED)?;
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
