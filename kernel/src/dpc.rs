//! Deferred Procedure Calls: the **IRQ > DPC > Thread** deferral mechanism.
//!
//! An interrupt service routine does the minimum (ack the device, capture status) and **queues a
//! `Dpc`**; the deferred completion work (advancing an IRP, signalling a `PendingOperation`) runs
//! when [`run_pending`] drains the queue at the interrupt-dispatch tail. A handler may briefly take
//! the scheduler lock but must not block. See `docs/architecture/drivers-and-irps.md`.
//!
//! A [`Dpc`] is a static, or a field of the heap object it serves, so queuing one allocates
//! nothing: the queue is an inline list of their addresses.
//!
//! ## One queue, drained by every CPU
//!
//! The queue is global and every CPU drains it at its interrupt tails (the timer's and a device's),
//! so a DPC runs on whichever CPU gets to it first. `queued` is cleared before the handler runs, so
//! a DPC queued again while it runs can run on a second CPU at the same time: a handler must
//! tolerate a concurrent instance of itself, or run its body through [`OneAtATime`].

use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::libkern::lockrank::LockRank;
use crate::libkern::{ArrayVec, IrqSpinLock};

/// Capacity of the DPC queue. Each `Dpc` is queued at most once (its `queued` flag), so the bound
/// is the number of distinct `Dpc` objects, and a small fixed capacity is plenty.
const DPC_CAPACITY: usize = 64;

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

#[derive(Clone, Copy)]
struct Queued(NonNull<Dpc>);

// SAFETY: a `Queued` is only ever read as a `&Dpc`, and a `Dpc` is `Sync`.
unsafe impl Send for Queued {}

/// The queue, and so the list each drain takes: one type, so a drain holds all it takes.
type Queue = ArrayVec<Queued, DPC_CAPACITY>;

/// The pending-DPC queue. A leaf `IrqSpinLock`, only ever held alone (push on enqueue; the drain
/// takes the whole list under it, then runs handlers with it **released**), so it never nests with
/// `SCHED`. See `kernel/docs/lock-ordering.md`.
static DPC_QUEUE: IrqSpinLock<Queue> = IrqSpinLock::new(LockRank::Leaf, ArrayVec::new());

/// Queue `dpc` to run at the next [`run_pending`]; idempotent, and safe from IRQ context.
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
    unsafe { enqueue_into(&DPC_QUEUE, dpc) };
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
    #[cfg(test)]
    assert_exclusive();
    run_pending_in(&DPC_QUEUE);
}

#[cfg(test)]
static EXCLUSIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
std::thread_local! {
    static HOLDS_EXCLUSIVE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// **A host test that drains the queue or completes an IRP holds this**, or another test's drain
/// could finish its IRPs, stack ones included, after its frame is gone.
#[cfg(test)]
pub struct Exclusive {
    _held: std::sync::MutexGuard<'static, ()>,
}

/// Take [`Exclusive`], waiting for any other test holding it.
#[cfg(test)]
pub fn exclusive() -> Exclusive {
    let guard = EXCLUSIVE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    HOLDS_EXCLUSIVE.with(|h| h.set(true));
    Exclusive { _held: guard }
}

#[cfg(test)]
impl Drop for Exclusive {
    fn drop(&mut self) {
        HOLDS_EXCLUSIVE.with(|h| h.set(false));
        match std::thread::panicking() {
            true => crate::io::irp::forget_completed(),
            false => assert!(crate::io::irp::none_completed(), "a test drains the IRPs it completes"),
        }
    }
}

#[cfg(test)]
impl Exclusive {
    /// Let a scoped thread of the same test drain and complete under this hold, while it borrows it.
    pub fn share(&self) -> Shared<'_> {
        HOLDS_EXCLUSIVE.with(|h| h.set(true));
        Shared(core::marker::PhantomData)
    }
}

/// A thread's share of a test's [`Exclusive`].
#[cfg(test)]
pub struct Shared<'a>(core::marker::PhantomData<&'a Exclusive>);

#[cfg(test)]
impl Drop for Shared<'_> {
    fn drop(&mut self) {
        HOLDS_EXCLUSIVE.with(|h| h.set(false));
    }
}

/// Panic unless this test thread holds [`Exclusive`].
#[cfg(test)]
pub fn assert_exclusive() {
    assert!(HOLDS_EXCLUSIVE.with(|h| h.get()), "a test that drains DPCs or completes an IRP holds dpc::exclusive()");
}

// The core logic takes the queue as a parameter so a host test can use a fresh local one.
//
// # Safety
// `dpc` stays valid until its handler has run.
unsafe fn enqueue_into(queue: &IrqSpinLock<Queue>, dpc: NonNull<Dpc>) {
    // SAFETY: valid, by the contract.
    if unsafe { dpc.as_ref() }.queued.swap(true, Ordering::AcqRel) {
        return; // already queued
    }
    if queue.lock().try_push(Queued(dpc)).is_err() {
        // Losing a DPC would strand whatever it was going to complete, so this is fatal.
        panic!("DPC queue full — more distinct DPCs than it holds");
    }
}

fn run_pending_in(queue: &IrqSpinLock<Queue>) {
    loop {
        let batch = core::mem::take(&mut *queue.lock());
        if batch.is_empty() {
            break;
        }
        for &Queued(dpc) in batch.iter() {
            // SAFETY: queued by `enqueue`, so `'static`, or by `enqueue_raw`, so valid until this
            // handler has run; its fields never change but the atomic `queued`. Clear `queued`
            // BEFORE running so the handler may re-enqueue this same DPC.
            let dpc = unsafe { dpc.as_ref() };
            dpc.queued.store(false, Ordering::Release);
            (dpc.handler)(dpc.ctx);
        }
    }
}

/// **A handler's body run on one CPU at a time**, for one whose work must stay in order. An
/// instance that finds another running asks it to go round again, and returns: it never spins.
pub struct OneAtATime {
    running: AtomicBool,
    again: AtomicBool,
}

impl OneAtATime {
    /// Nothing running.
    pub const fn new() -> Self {
        OneAtATime { running: AtomicBool::new(false), again: AtomicBool::new(false) }
    }

    /// Run `body` round after round while others ask, or have the running instance do it.
    pub fn run(&self, mut body: impl FnMut()) {
        // `SeqCst`: each side stores then loads (ask, then check running; release, then re-check),
        // and only one order of all four keeps both instances from missing the other.
        self.again.store(true, Ordering::SeqCst);
        while !self.running.swap(true, Ordering::SeqCst) {
            while self.again.swap(false, Ordering::SeqCst) {
                body();
            }
            self.running.store(false, Ordering::SeqCst);
            if !self.again.load(Ordering::SeqCst) {
                break;
            }
        }
    }
}

impl Default for OneAtATime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::sync::atomic::AtomicU32;

    /// **An instance arriving while the body runs is served by a second round, not beside it.**
    #[test]
    fn an_instance_arriving_meanwhile_is_run_after_the_first_not_beside_it() {
        use core::cell::{Cell, RefCell};
        fn instance(s: &OneAtATime, depth: &Cell<u32>, rounds: &RefCell<Vec<u32>>, arrive: bool) {
            s.run(|| {
                depth.set(depth.get() + 1);
                rounds.borrow_mut().push(depth.get());
                if arrive && rounds.borrow().len() == 1 {
                    instance(s, depth, rounds, false);
                }
                depth.set(depth.get() - 1);
            });
        }
        let (s, depth, rounds) = (OneAtATime::new(), Cell::new(0), RefCell::new(Vec::new()));
        instance(&s, &depth, &rounds, true);
        assert_eq!(*rounds.borrow(), [1, 1], "a round each, one after the other");
        instance(&s, &depth, &rounds, false);
        assert_eq!(rounds.borrow().len(), 3, "and the next instance runs alone");
    }

    /// **Two CPUs, each running it after its work arrived**: no two rounds overlap, which Miri's
    /// threads find without it, and a round after the last arrival takes all of it.
    #[test]
    fn every_instances_work_is_taken_by_a_round_after_it() {
        use std::sync::atomic::AtomicUsize;
        let s = OneAtATime::new();
        let (posted, taken, inside) = (AtomicUsize::new(0), AtomicUsize::new(0), AtomicBool::new(false));
        std::thread::scope(|t| {
            for _ in 0..2 {
                t.spawn(|| {
                    for _ in 0..3 {
                        posted.fetch_add(1, Ordering::Relaxed);
                        s.run(|| {
                            assert!(!inside.swap(true, Ordering::Relaxed), "two rounds at once");
                            taken.store(posted.load(Ordering::Relaxed), Ordering::Relaxed);
                            inside.store(false, Ordering::Relaxed);
                        });
                    }
                });
            }
        });
        assert_eq!(taken.load(Ordering::Relaxed), 6, "work arrived after the last round");
    }

    fn fresh_queue() -> IrqSpinLock<Queue> {
        IrqSpinLock::new(LockRank::Leaf, ArrayVec::new())
    }

    fn enqueue_into(q: &IrqSpinLock<Queue>, d: &Dpc) {
        // SAFETY: drained while `d` lives.
        unsafe { super::enqueue_into(q, NonNull::from(d)) }
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
        struct Stamp<'a> {
            clock: &'a AtomicU32,
            at: AtomicU32,
        }
        fn stamp(ctx: *mut ()) {
            // SAFETY: the test passes its live `Stamp`.
            let s = unsafe { &*(ctx as *const Stamp<'_>) };
            s.at.store(s.clock.fetch_add(1, Ordering::Relaxed) + 1, Ordering::Relaxed);
        }
        let q = fresh_queue();
        let clock = AtomicU32::new(0);
        let stamps: [Stamp<'_>; 3] = core::array::from_fn(|_| Stamp { clock: &clock, at: AtomicU32::new(0) });
        let dpcs = stamps.each_ref().map(|s| Dpc::new(stamp, s as *const Stamp<'_> as *mut ()));
        for d in &dpcs {
            enqueue_into(&q, d);
        }
        run_pending_in(&q);
        assert_eq!(stamps.each_ref().map(|s| s.at.load(Ordering::Relaxed)), [1, 2, 3], "in the order queued");
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
    fn a_queue_of_its_capacity_drains_whole() {
        let q = fresh_queue();
        let counters: [AtomicU32; DPC_CAPACITY] = core::array::from_fn(|_| AtomicU32::new(0));
        let dpcs = counters.each_ref().map(dpc_for);
        for d in &dpcs {
            enqueue_into(&q, d);
        }
        run_pending_in(&q);
        assert!(counters.iter().all(|c| c.load(Ordering::Relaxed) == 1));
    }

    #[test]
    #[should_panic(expected = "DPC queue full")]
    fn a_dpc_past_the_capacity_is_fatal() {
        let q = fresh_queue();
        let c = AtomicU32::new(0);
        let dpcs: [Dpc; DPC_CAPACITY + 1] = core::array::from_fn(|_| dpc_for(&c));
        for d in &dpcs {
            enqueue_into(&q, d);
        }
    }

    /// **A DPC a handler queues runs in the same drain**, not at the next interrupt.
    #[test]
    fn a_dpc_a_handler_queues_runs_in_the_same_drain() {
        struct Chain<'a> {
            q: &'a IrqSpinLock<Queue>,
            then: &'a Dpc,
        }
        fn chain(ctx: *mut ()) {
            // SAFETY: the test passes its live `Chain`, which outlives the drain.
            let c = unsafe { &*(ctx as *const Chain<'_>) };
            enqueue_into(c.q, c.then);
        }
        let q = fresh_queue();
        let c = AtomicU32::new(0);
        let then = dpc_for(&c);
        let link = Chain { q: &q, then: &then };
        let first = Dpc::new(chain, &link as *const Chain<'_> as *mut ());
        enqueue_into(&q, &first);
        run_pending_in(&q);
        assert_eq!(c.load(Ordering::Relaxed), 1, "the DPC its handler queued ran");
        assert_eq!(q.lock().len(), 0);
    }

    #[test]
    fn drain_empty_queue_is_noop() {
        let q = fresh_queue();
        run_pending_in(&q); // must not panic
        assert_eq!(q.lock().len(), 0);
    }
}
