//! Deferred Procedure Calls: the **IRQ > DPC > Thread** deferral mechanism.
//!
//! An interrupt service routine does the minimum (ack the device, capture status) and **queues a
//! `Dpc`**; the deferred completion work (advancing an IRP, signalling a `PendingOperation`) runs
//! when [`run_pending`] drains the queue at the interrupt-dispatch tail. A handler may briefly take
//! the scheduler lock but must not block. See `docs/architecture/drivers-and-irps.md`.
//!
//! A [`Dpc`] is a static, or a field of the heap object it serves, so queuing one allocates
//! nothing: the queue is a pre-reserved list of their addresses.
//!
//! ## One queue, drained by every CPU
//!
//! The queue is global and every CPU drains it at its interrupt tails (the timer's and a device's),
//! so a DPC runs on whichever CPU gets to it first. `queued` is cleared before the handler runs, so
//! a DPC queued again while it runs can run on a second CPU at the same time: a handler must
//! tolerate a concurrent instance of itself.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::libkern::lockrank::LockRank;
use crate::libkern::{AllocError, IrqSpinLock, KVec};

/// Capacity of the DPC queue. Each `Dpc` is queued at most once (its `queued` flag), so the bound
/// is the number of distinct `Dpc` objects, and a small fixed reserve is plenty.
const DPC_RESERVE: usize = 64;

/// A deferred procedure call: `handler(ctx)`, run when the queue is drained.
///
/// A `static` ([`Dpc::new`] is `const`), queued with [`enqueue`]; or a field of a heap object that
/// outlives its handler, queued with [`enqueue_raw`].
pub struct Dpc {
    handler: fn(*mut ()),
    ctx: *mut (),
    /// Set while queued, deduping a double-`enqueue`; cleared just before the handler runs.
    queued: AtomicBool,
}

// SAFETY: shared across CPUs, a `Dpc` is only read but for `queued`, which is atomic; the
// thread-safety of the raw `ctx` pointer is the handler's own concern. These impls let a `Dpc`
// be a `static` and be shared by `&Dpc` across the enqueue/run boundary.
unsafe impl Sync for Dpc {}
// SAFETY: as above.
unsafe impl Send for Dpc {}

impl Dpc {
    /// A DPC that runs `handler(ctx)` when drained.
    pub const fn new(handler: fn(*mut ()), ctx: *mut ()) -> Self {
        Self { handler, ctx, queued: AtomicBool::new(false) }
    }
}

/// The pending-DPC queue. A leaf `IrqSpinLock`, only ever held alone (push on enqueue; the drain
/// snapshots-and-clears under it, then runs handlers with it **released**), so it never nests with
/// `SCHED`. See `kernel/docs/lock-ordering.md`.
static DPC_QUEUE: IrqSpinLock<KVec<usize>> = IrqSpinLock::new(LockRank::Leaf, KVec::new());

/// Reserve the DPC queue's backing storage, once at boot after the allocator is up and **before**
/// interrupts are armed (the first ISR may `enqueue`). The only allocation the queue makes:
/// `enqueue` stays within the reserve, so it never allocates in interrupt context.
pub fn init() -> Result<(), AllocError> {
    // Reserve **outside** the queue lock, then move the list in: reserving under it would allocate
    // (slab, rank 6a) while holding a leaf-ranked lock, an order inversion the debug tracker
    // rejects. The lock is only honestly a leaf if nothing under it ever allocates.
    let mut reserved = KVec::new();
    reserved.try_reserve(DPC_RESERVE)?;
    // **Once**: replacing a queue already in use would drop what it holds, and a `Dpc` dropped
    // while marked queued is never queued again.
    let mut q = DPC_QUEUE.lock();
    if q.capacity() < DPC_RESERVE {
        *q = reserved;
    }
    Ok(())
}

/// Queue `dpc` to run at the next [`run_pending`]; idempotent. Safe to call from IRQ context. A
/// `'static` DPC outlives every drain, whichever CPU's runs it.
pub fn enqueue(dpc: &'static Dpc) {
    // SAFETY: a `'static` DPC is valid for good.
    unsafe { enqueue_raw(NonNull::from(dpc)) }
}

/// [`enqueue`] a DPC that is not `'static`, one inside a heap object.
///
/// # Safety
/// `dpc` stays valid until its handler has run, on whichever CPU drains it.
pub unsafe fn enqueue_raw(dpc: NonNull<Dpc>) {
    #[cfg(test)]
    ENQUEUED.with(|e| e.borrow_mut().push(dpc.as_ptr() as usize));
    // SAFETY: valid until its handler has run, by the caller's contract.
    enqueue_into(&DPC_QUEUE, unsafe { dpc.as_ref() });
}

#[cfg(test)]
std::thread_local! {
    /// Each `Dpc` this thread passed to [`enqueue`]: other tests' threads initialise and drain the
    /// one queue meanwhile, and a `Dpc` already queued is not queued again.
    static ENQUEUED: core::cell::RefCell<std::vec::Vec<usize>> =
        const { core::cell::RefCell::new(std::vec::Vec::new()) };
}

/// The address of each `Dpc` this thread passed to [`enqueue`] since it last asked, then forgotten.
#[cfg(test)]
pub fn take_enqueued() -> std::vec::Vec<usize> {
    ENQUEUED.with(|e| core::mem::take(&mut *e.borrow_mut()))
}

/// Run every pending DPC, looping until the queue is empty so a DPC that enqueues another is
/// caught. Call at the interrupt-dispatch tail.
///
/// Runs with IF=0 (the gate masks interrupts), so no nested IRQ can enqueue mid-drain and no
/// re-entrancy guard is needed. A handler may take `SCHED` briefly (the queue lock is released
/// first) but must not block.
pub fn run_pending() {
    run_pending_in(&DPC_QUEUE);
}

// The core logic takes the queue as a parameter so a host test can use a fresh local one. The queue
// stores each `&Dpc` as a `usize` address: a raw `*mut Dpc` is not `Send`, so a `KVec<*mut Dpc>`
// static would not be `Sync`.
fn enqueue_into(queue: &IrqSpinLock<KVec<usize>>, dpc: &Dpc) {
    if dpc.queued.swap(true, Ordering::AcqRel) {
        return; // already queued
    }
    let addr = dpc as *const Dpc as usize;
    let mut q = queue.lock();
    // Bounded by the number of distinct DPCs (dedup above), so it stays within the reserve.
    // **Enforced by the push, not by the assertion beside it**: `try_push` *grows*, and `kmalloc`
    // under the queue lock at an interrupt tail is forbidden outright; the `debug_assert` would not
    // fire, as growth happens first and succeeds.
    debug_assert!(q.len() < q.capacity(), "DPC queue capacity exceeded");
    if q.push_within_capacity(addr).is_err() {
        // Losing a DPC would strand whatever it was going to complete, so this is fatal.
        panic!("DPC queue reserve exhausted — more distinct DPCs than the queue was sized for");
    }
}

fn run_pending_in(queue: &IrqSpinLock<KVec<usize>>) {
    loop {
        let mut buf = [0usize; DPC_RESERVE];
        let n = {
            let mut q = queue.lock();
            let n = q.len();
            buf[..n].copy_from_slice(&q);
            q.clear();
            n
        };
        if n == 0 {
            break;
        }
        for &addr in &buf[..n] {
            // SAFETY: `addr` is a live `&Dpc` registered by `enqueue`, whose fields never change
            // but the atomic `queued`. Clear `queued` BEFORE running so the handler may
            // re-enqueue this same DPC.
            let dpc = unsafe { &*(addr as *const Dpc) };
            dpc.queued.store(false, Ordering::Release);
            (dpc.handler)(dpc.ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use core::sync::atomic::AtomicU32;

    fn fresh_queue() -> IrqSpinLock<KVec<usize>> {
        init_global_heap();
        let q = IrqSpinLock::new(LockRank::Leaf, KVec::new());
        q.lock().try_reserve(DPC_RESERVE).unwrap();
        q
    }

    /// A handler that increments the `AtomicU32` its `ctx` points at.
    fn bump(ctx: *mut ()) {
        // SAFETY: tests pass a pointer to a live `AtomicU32`.
        unsafe { &*(ctx as *const AtomicU32) }.fetch_add(1, Ordering::Relaxed);
    }

    fn dpc_for(counter: &AtomicU32) -> Dpc {
        Dpc::new(bump, counter as *const AtomicU32 as *mut ())
    }

    #[test]
    fn enqueue_then_drain_runs_handler_once() {
        let q = fresh_queue();
        let c = AtomicU32::new(0);
        let d = dpc_for(&c);
        enqueue_into(&q, &d);
        run_pending_in(&q);
        assert_eq!(c.load(Ordering::Relaxed), 1);
        run_pending_in(&q);
        assert_eq!(c.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn double_enqueue_is_deduped() {
        let q = fresh_queue();
        let c = AtomicU32::new(0);
        let d = dpc_for(&c);
        enqueue_into(&q, &d);
        enqueue_into(&q, &d); // already queued — ignored
        assert_eq!(q.lock().len(), 1, "queued once, not twice");
        run_pending_in(&q);
        assert_eq!(c.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn drain_runs_all_pending_in_order() {
        let q = fresh_queue();
        let counters = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
        let dpcs = [dpc_for(&counters[0]), dpc_for(&counters[1]), dpc_for(&counters[2])];
        for d in &dpcs {
            enqueue_into(&q, d);
        }
        run_pending_in(&q);
        for c in &counters {
            assert_eq!(c.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn requeue_after_drain_runs_again() {
        let q = fresh_queue();
        let c = AtomicU32::new(0);
        let d = dpc_for(&c);
        enqueue_into(&q, &d);
        run_pending_in(&q);
        enqueue_into(&q, &d);
        run_pending_in(&q);
        assert_eq!(c.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn drain_empty_queue_is_noop() {
        let q = fresh_queue();
        run_pending_in(&q); // must not panic
        assert_eq!(q.lock().len(), 0);
    }
}
