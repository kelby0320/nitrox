//! Deferred Procedure Calls — the **IRQ > DPC > Thread** deferral mechanism.
//!
//! An interrupt service routine does the minimum (ack the device, capture
//! status) and **queues a `Dpc`**; the deferred completion work — advancing or
//! completing an IRP, running completion routines, signalling a
//! `PendingOperation` to wake its waiters — runs later, when the queue is
//! drained at the interrupt-dispatch tail by [`run_pending`]. DPCs run
//! non-blocking; a handler may briefly take the scheduler lock (to make a thread
//! runnable) but must not block. See `docs/architecture/drivers-and-irps.md`.
//!
//! A [`Dpc`] is embedded **inline** in its owning struct (a future IRP /
//! `InterruptObject`), so queuing one allocates nothing — the queue is a
//! pre-reserved list of node pointers.
//!
//! ## Single-CPU stand-in (SMP trajectory)
//!
//! DPCs are inherently per-CPU (a DPC queued by an ISR runs on the CPU that took
//! the interrupt). The single global queue here stands in for per-CPU queues,
//! exactly like the single global `SCHED`/`current` in [`crate::sched`]: the
//! public API ([`enqueue`] / [`run_pending`]) always means "the current CPU's
//! queue", so the Phase-3 SMP refactor that makes the scheduler per-CPU makes
//! this per-CPU too — a storage change, not an API change.

use core::sync::atomic::{AtomicBool, Ordering};

use crate::libkern::{AllocError, IrqSpinLock, KVec};

/// Capacity of the DPC queue. Bounded by the number of distinct `Dpc` objects
/// (each is queued at most once, via its `queued` flag), so a small fixed
/// reserve is plenty for Phase 2 (a handful of device IRPs / `InterruptObject`s).
const DPC_RESERVE: usize = 64;

/// A deferred procedure call: `handler(ctx)`, run when the queue is drained.
///
/// Embed it inline in an owning struct and `'static`-place it (or own it within
/// a heap object that outlives any queued reference). [`Dpc::new`] is `const` so
/// a `static Dpc` is possible.
pub struct Dpc {
    handler: fn(*mut ()),
    ctx: *mut (),
    /// Set while queued — dedups a double-`enqueue`; cleared just before the
    /// handler runs (so a handler may re-`enqueue` the same DPC).
    queued: AtomicBool,
}

// SAFETY: a `Dpc` is enqueued and run only on the single CPU that owns the
// queue (interrupt context, IF=0 — no concurrent access); the thread-safety of
// the raw `ctx` pointer is the handler's own concern. These impls let a `Dpc`
// be a `static` and be shared by `&Dpc` across the enqueue/run boundary.
unsafe impl Sync for Dpc {}
unsafe impl Send for Dpc {}

impl Dpc {
    /// A DPC that runs `handler(ctx)` when drained.
    pub const fn new(handler: fn(*mut ()), ctx: *mut ()) -> Self {
        Self {
            handler,
            ctx,
            queued: AtomicBool::new(false),
        }
    }
}

/// The current CPU's pending-DPC queue (single-CPU stand-in — see the module
/// docs). A leaf `IrqSpinLock`: only ever held alone (push on enqueue; the drain
/// snapshots-and-clears under it, then runs handlers with it **released**), so
/// it never nests with `SCHED`. See `kernel/docs/lock-ordering.md`.
static DPC_QUEUE: IrqSpinLock<KVec<usize>> = IrqSpinLock::new(KVec::new());

/// Reserve the DPC queue's backing storage. Call once at boot, after the
/// allocator is up and **before** interrupts are armed (the first ISR may
/// `enqueue`). The only allocation the queue ever makes — `enqueue` stays within
/// the reserve, so it never allocates in interrupt context.
pub fn init() -> Result<(), AllocError> {
    DPC_QUEUE.lock().try_reserve(DPC_RESERVE)
}

/// Queue `dpc` to run at the next [`run_pending`]. Idempotent — a `Dpc` already
/// queued is not queued twice. Safe to call from IRQ context.
pub fn enqueue(dpc: &Dpc) {
    enqueue_into(&DPC_QUEUE, dpc);
}

/// Run every pending DPC on the current CPU's queue, looping until empty so a
/// DPC that enqueues another is caught. Call at the interrupt-dispatch tail.
///
/// Runs with IF=0 (interrupt context — the gate masks interrupts), so no nested
/// IRQ can enqueue mid-drain and no re-entrancy guard is needed. Each handler
/// may take `SCHED` briefly (the queue lock is released before it runs) but must
/// not block.
pub fn run_pending() {
    run_pending_in(&DPC_QUEUE);
}

// --- Core logic, parameterised over the queue so it is host-testable on a
//     fresh local queue (the production one is a global static). ---

// The queue stores each `&Dpc` as a `usize` address (a raw `*mut Dpc` is not
// `Send`, so a `KVec<*mut Dpc>` static would not be `Sync`); this is the same
// fn-pointer-as-usize idiom the IDT device-handler registry uses.
fn enqueue_into(queue: &IrqSpinLock<KVec<usize>>, dpc: &Dpc) {
    if dpc.queued.swap(true, Ordering::AcqRel) {
        return; // already queued
    }
    let addr = dpc as *const Dpc as usize;
    let mut q = queue.lock();
    // The queue is bounded by the number of distinct DPCs (dedup above), so it
    // stays within the reserve and `try_push` never allocates under the lock.
    debug_assert!(q.len() < q.capacity(), "DPC queue capacity exceeded");
    q.try_push(addr).expect("DPC queue within reserve");
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
            // SAFETY: `addr` is a live `&Dpc` registered by `enqueue`; single-CPU
            // and IF=0, so it is not concurrently mutated. Clear `queued` BEFORE
            // running so the handler may re-enqueue this same DPC.
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
        let q = IrqSpinLock::new(KVec::new());
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
        // Queue is empty afterwards; a second drain is a no-op.
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
        // `queued` was cleared during the drain, so it can be queued again.
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
