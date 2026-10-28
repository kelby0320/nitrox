//! The single process-wide kernel handle table and its one-time init.
//!
//! The handle table is **global**: one segmented table with a per-entry
//! `owner_pid` checked on every lookup (per-process tables are rejected; see
//! `docs/rationale/rejected-approaches.md`). It is stored inline in a once-init
//! cell: [`init`] runs once in early boot after the heap is up, and [`get`]
//! returns a shared `&'static HandleTable` whose `&self` methods synchronise
//! themselves (seqlock lookups, the rank-3 alloc lock). No coarse lock wraps it,
//! so lookups stay lock-free.

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
// `READY` with a `Release` store, and `get` reads `state` with `Acquire`. After
// `READY` the table is never mutated through the cell (its `&self` methods
// provide all interior mutability), moved or dropped, so `&'static` shared
// borrows are sound; the one-time init is guarded by a CAS.
unsafe impl Sync for GlobalTable {}

static GLOBAL: GlobalTable = GlobalTable { state: AtomicU8::new(UNINIT), slot: UnsafeCell::new(MaybeUninit::uninit()) };

/// Initialise the global handle table exactly once. Must run **after the
/// heap is up** (it eagerly allocates segment 0), **after** the entropy
/// subsystem is keyed ([`crate::entropy::init`] — the free-list shuffle seed is
/// drawn from the CSPRNG), and **before** any userspace can issue a handle
/// syscall. Returns `Err` if the table allocation fails.
pub fn init() -> Result<(), HandleError> {
    // The CAS makes the initialise-once invariant explicit.
    if GLOBAL.state.compare_exchange(UNINIT, INITIALISING, Ordering::AcqRel, Ordering::Acquire).is_err() {
        debug_assert!(false, "handle::global::init called more than once");
        return Ok(());
    }
    // The seed only randomizes free-list scan order: defence in depth on
    // handle-value unpredictability, atop the owner-PID check and per-slot
    // generation that actually unforge handles.
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

/// [`init`] for the host tests that reach the global table, once however many ask, after the
/// entropy it seeds from, as the boot orders them.
#[cfg(test)]
pub fn init_for_tests() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        crate::entropy::key_for_tests();
        init().expect("the global handle table");
    });
}

/// The global handle table. Panics if called before [`init`].
pub fn get() -> &'static HandleTable {
    GLOBAL.table()
}

impl GlobalTable {
    fn table(&self) -> &HandleTable {
        // In a release build too: before `init` the slot is uninitialised memory.
        assert_eq!(self.state.load(Ordering::Acquire), READY, "handle::global::get before init");
        // SAFETY: `READY` (observed with `Acquire`) was published with a `Release`
        // store after the table was fully written; the table is never moved or
        // dropped for the kernel's lifetime, so a `'static` shared borrow is
        // sound. See the `Sync` impl above.
        unsafe { (*self.slot.get()).assume_init_ref() }
    }
}

/// Handles closed per batch by [`close_all_owned_by`]; each entry is a pointer
/// plus a type tag, so a batch is small on the sweeping thread's kernel stack.
const SWEEP_BATCH: usize = 16;

/// Close **every** handle owned by `pid` and release the objects they pinned;
/// otherwise a dead process's entries pin their objects forever (its end of a
/// pipe never closes, so a peer never sees `PeerClosed`).
///
/// # Context
///
/// Thread context only: outside `SCHED`, not in IRQ context. Releasing
/// references runs object destructors, which reach the rank-6 allocator and,
/// for an IPC endpoint, take rank-1 `SCHED`; holding rank-1 or rank-3 across
/// that would invert the ranking. So entries are unlinked in batches under the
/// rank-3 lock and each batch is dropped after it is released, with preemption
/// disabled across the drops as [`reap_pending`](crate::sched::reap_pending)
/// does: a descheduled holder of an allocator spinlock starves every CPU
/// spinning on it.
///
/// Safe against pid reuse: pids are monotonic and never reused
/// ([`alloc_pid`](crate::sched::alloc_pid)). Returns how many handles it closed,
/// which tells "nothing left" from "never ran".
pub fn close_all_owned_by(pid: u32) -> usize {
    use super::table::{ClosedObject, SweepCursor};
    use crate::object::ObjectRef;

    let mut closed = 0usize;
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
        closed += n;
        if !more {
            return closed;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Refused in every build, not only where `debug_assert` runs.
    #[test]
    #[should_panic(expected = "before init")]
    fn the_table_is_refused_before_init() {
        let cell = GlobalTable { state: AtomicU8::new(UNINIT), slot: UnsafeCell::new(MaybeUninit::uninit()) };
        let _ = cell.table();
    }
}
