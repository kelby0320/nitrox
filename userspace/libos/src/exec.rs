//! The async core: the [`Op`] future over `sys_wait`, and [`block_on`].
//!
//! An [`Op`] wraps an in-flight `PendingOperation`; polling it does a non-blocking
//! `sys_wait` on the PO. If not ready, it registers the PO with the running
//! [`block_on`] reactor (via the `Waker`) and yields `Pending`; [`block_on`] then does
//! one blocking `sys_wait` on the registered handles and re-polls. There is no ready
//! queue (deferred; see `docs/architecture/libos.md`).

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use libkern::{IoOp, IoResult, KError, RawHandle};

use crate::error::{Error, Result};
use crate::sys;

/// Max handles a single `sys_wait` (and thus one poll round) can register — mirrors
/// the kernel's `MAX_WAIT_HANDLES`.
const MAX_WAIT: usize = 8;

/// How long `block_on` sleeps with nothing to wait on, before it polls again.
const IDLE_NS: u64 = 1_000_000;

/// An `[IoResult; N]` of zeroes (no `Copy`/`Default` bound needed).
fn zeroed_results<const N: usize>() -> [IoResult; N] {
    core::array::from_fn(|_| IoResult { handle: 0, status: 0, reserved: 0, result: 0 })
}

// --- the reactor: which handles a Pending future wants waited on ----------

#[derive(Clone, Copy)]
struct Registry {
    handles: [u64; MAX_WAIT],
    count: usize,
    /// Whether a handle past the `MAX_WAIT`th was left out.
    dropped: bool,
}

/// Per-`block_on` set of waitable handles the current poll round registered. A `Cell`
/// because it is shared through the `Waker`'s data pointer *and* `block_on`.
struct Reactor {
    reg: Cell<Registry>,
}

impl Reactor {
    fn new() -> Self {
        Reactor { reg: Cell::new(Registry { handles: [0; MAX_WAIT], count: 0, dropped: false }) }
    }

    /// Register a waitable handle for this poll round (deduplicated; capped at `MAX_WAIT`).
    fn register(&self, h: u64) {
        let mut r = self.reg.get();
        if r.handles[..r.count].contains(&h) {
            return;
        }
        if r.count < MAX_WAIT {
            r.handles[r.count] = h;
            r.count += 1;
        } else {
            r.dropped = true;
        }
        self.reg.set(r);
    }

    /// Take and clear the round's registrations.
    fn take(&self) -> Registry {
        self.reg.replace(Registry { handles: [0; MAX_WAIT], count: 0, dropped: false })
    }
}

/// A timer `block_on` sleeps on, made the first time it has nothing to wait on.
struct Idle {
    timer: Option<u64>,
}

impl Idle {
    /// A deadline [`IDLE_NS`] from now; a poll if the clock cannot be read.
    fn soon() -> u64 {
        sys::monotonic_ns().map_or(0, |now| now.saturating_add(IDLE_NS))
    }

    /// Sleep [`IDLE_NS`]; not at all if no timer can be made.
    fn sleep(&mut self) {
        if self.timer.is_none() {
            let t = sys::timer_create();
            self.timer = (t >= 0).then_some(t as u64);
        }
        if let Some(t) = self.timer {
            sys::wait(&[t], &mut zeroed_results::<1>(), Self::soon());
        }
    }
}

impl Drop for Idle {
    fn drop(&mut self) {
        if let Some(t) = self.timer {
            sys::handle_close(t);
        }
    }
}

// --- the Waker: a thin handle onto the reactor ----------------------------
//
// The Waker's data pointer references the `block_on` `Reactor`; `Op::poll` reads it
// back via `cx.waker().data()`, only from a waker with `VTABLE`, which only `block_on` makes; a
// clone may outlive the reactor, so it gets `CLONE_VTABLE`. Waking is a no-op: the wait wakes.

static VTABLE: RawWakerVTable = RawWakerVTable::new(waker_clone, waker_noop, waker_noop, waker_noop);
static CLONE_VTABLE: RawWakerVTable = RawWakerVTable::new(waker_clone, waker_noop, waker_noop, waker_noop);

unsafe fn waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &CLONE_VTABLE)
}

unsafe fn waker_noop(_data: *const ()) {}

// --- Op: the core future --------------------------------------------------

/// A future over an in-flight `PendingOperation`, resolving to its [`IoResult`] or an [`Error`].
/// Closes the PO on drop, **first waiting out an unfinished I/O**: the kernel cannot cancel one,
/// and its device may still be using the buffer the caller gets back.
#[must_use = "an Op does nothing unless awaited or passed to block_on"]
#[derive(Debug)]
pub struct Op {
    po: u64,
    /// Whether the PO is an I/O's, whose drop must wait for it, rather than a lookup's.
    io: bool,
    finished: bool,
    /// Whether `poll` has returned `Ready`, so its result is handed out once.
    done: bool,
}

impl Op {
    /// Submit an I/O operation on `resource`, returning the in-flight future.
    ///
    /// Issues `sys_io_submit` (which never blocks). A synchronous submit error (bad
    /// argument / permission) is returned here; device/medium errors arrive later
    /// through the future's `IoResult.status`.
    pub fn submit(resource: RawHandle, op: &IoOp) -> Result<Op> {
        let po = sys::io_submit(resource.0, op);
        if po < 0 {
            return Err(Error::from_status(po as i32));
        }
        Ok(Op { po: po as u64, io: true, finished: false, done: false })
    }

    /// Wrap a lookup's PO; its drop closes it at once, as the kernel frees a lookup closed unread.
    pub(crate) fn from_po(po: u64) -> Op {
        Op { po, io: false, finished: false, done: false }
    }
}

impl Future for Op {
    type Output = Result<IoResult>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut(); // `Op: Unpin` — no self-referential state.
        if this.done {
            // Handed out already: a second `Ok` would give its handle two owners.
            return Poll::Ready(Err(Error::from_status(KError::InvalidHandle as i32)));
        }
        let mut r = zeroed_results::<1>();
        let n = sys::wait(&[this.po], &mut r, 0);
        if n >= 1 {
            this.finished = true;
            this.done = true;
            let res = r[0];
            if res.status < 0 {
                return Poll::Ready(Err(Error::from_status(res.status)));
            }
            return Poll::Ready(Ok(res));
        }
        if n != KError::WouldBlock as i64 {
            this.done = true;
            return Poll::Ready(Err(Error::from_status(n as i32)));
        }
        let waker = cx.waker();
        if !core::ptr::eq(waker.vtable(), &VTABLE) {
            // Another executor's waker, or a clone of `block_on`'s: be polled again, unregistered.
            waker.wake_by_ref();
            return Poll::Pending;
        }
        // SAFETY: only `block_on` makes a waker with `VTABLE`, its data a `Reactor` that lives
        // for the poll.
        let reactor = unsafe { &*(waker.data() as *const Reactor) };
        reactor.register(this.po);
        Poll::Pending
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        if self.io && !self.finished {
            sys::wait(&[self.po], &mut zeroed_results::<1>(), u64::MAX);
        }
        sys::handle_close(self.po);
    }
}

// --- block_on: the single-task driver -------------------------------------

/// Drive one future to completion on the current thread, blocking in `sys_wait`
/// between polls. Alloc-free.
///
/// On `Pending`, waits on the handles this round's `Op`s registered, then re-polls; it sleeps
/// [`IDLE_NS`] with none or a failed wait, and waits no longer when one was left out.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let reactor = Reactor::new();
    let raw = RawWaker::new(&reactor as *const Reactor as *const (), &VTABLE);
    // SAFETY: `VTABLE`'s functions are valid; `reactor` (a local) outlives `waker`,
    // which never escapes this function.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = core::pin::pin!(fut);
    let mut idle = Idle { timer: None };
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {
                let reg = reactor.take();
                let handles = &reg.handles[..reg.count];
                let mut results = zeroed_results::<MAX_WAIT>();
                if reg.count == 0 {
                    idle.sleep();
                } else if reg.dropped {
                    sys::wait(handles, &mut results, Idle::soon());
                } else if sys::wait(handles, &mut results, u64::MAX) < 0 {
                    idle.sleep();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use libkern::{IO_OPCODE_READ, IoOp};

    fn read_op() -> IoOp {
        IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 }
    }

    #[test]
    fn pre_signalled_op_completes_without_blocking() {
        sys::reset();
        sys::script_next(0, 42, true); // ready immediately
        let op = Op::submit(RawHandle(1), &read_op()).unwrap();
        let r = block_on(op).unwrap();
        assert_eq!(r.result, 42);
        let (subs, _waits, closes) = sys::counts();
        assert_eq!(subs, 1);
        assert_eq!(closes, 1, "block_on must close the PO on drop");
    }

    #[test]
    fn deferred_op_polls_then_waits_then_completes() {
        sys::reset();
        sys::script_next(0, 7, false); // completes on the blocking wait
        let op = Op::submit(RawHandle(1), &read_op()).unwrap();
        let r = block_on(op).unwrap();
        assert_eq!(r.result, 7);
        let (_subs, waits, _closes) = sys::counts();
        assert!(waits >= 2, "expected a poll + a blocking wait, got {waits}");
    }

    /// **A wait takes no more handles than its results hold**: two ready operations, room for one.
    #[test]
    fn a_wait_takes_no_more_handles_than_its_results_hold() {
        sys::reset();
        sys::script_next(0, 1, true);
        sys::script_next(0, 2, true);
        let a = Op::submit(RawHandle(1), &read_op()).unwrap();
        let b = Op::submit(RawHandle(1), &read_op()).unwrap();
        let mut one = zeroed_results::<1>();
        assert_eq!(sys::wait(&[a.po, b.po], &mut one, 0), 1);
        assert_eq!(sys::last_wait_count(), 1);
    }

    /// **An `Op` reads no waker's data but `block_on`'s**: another executor's waker keeps its
    /// data untouched, and a clone kept past `block_on` reads no reactor gone with it.
    #[test]
    fn an_op_reads_no_wakers_data_but_block_ons() {
        static FOREIGN: RawWakerVTable = RawWakerVTable::new(foreign_clone, waker_noop, waker_noop, waker_noop);
        unsafe fn foreign_clone(data: *const ()) -> RawWaker {
            RawWaker::new(data, &FOREIGN)
        }
        sys::reset();
        sys::script_next(0, 1, false);
        let mut op = Op::submit(RawHandle(1), &read_op()).unwrap();
        let state = [0u64; 9];
        // SAFETY: `FOREIGN`'s functions are no-ops, and `state` outlives the waker.
        let foreign = unsafe { Waker::from_raw(RawWaker::new(state.as_ptr().cast(), &FOREIGN)) };
        assert!(Pin::new(&mut op).poll(&mut Context::from_waker(&foreign)).is_pending());
        assert_eq!(state, [0; 9]);

        let mut kept = None;
        block_on(core::future::poll_fn(|cx| {
            kept = Some(cx.waker().clone());
            Poll::Ready(())
        }));
        let kept = kept.unwrap();
        assert!(Pin::new(&mut op).poll(&mut Context::from_waker(&kept)).is_pending());
    }

    /// **A completed `Op` hands its result out once**, and waits on nothing for a second poll.
    #[test]
    fn a_completed_op_hands_its_result_out_once() {
        sys::reset();
        sys::script_next(0, 777, true);
        let mut op = Op::submit(RawHandle(1), &read_op()).unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(Pin::new(&mut op).poll(&mut cx), Poll::Ready(Ok(r)) if r.result == 777));
        let (_s, waits, _c) = sys::counts();
        assert!(matches!(Pin::new(&mut op).poll(&mut cx), Poll::Ready(Err(_))));
        assert_eq!(sys::counts().1, waits);
    }

    /// **An `Op` whose wait fails completes with the error**, where it was polled again for ever.
    #[test]
    fn an_op_whose_wait_fails_completes_with_the_error() {
        sys::reset();
        sys::script_next(0, 1, false);
        let mut op = Op::submit(RawHandle(1), &read_op()).unwrap();
        sys::handle_close(op.po);
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(Pin::new(&mut op).poll(&mut cx), Poll::Ready(Err(_))));
        let mut op = Op::submit(RawHandle(1), &read_op()).unwrap();
        sys::handle_close(op.po);
        assert!(block_on(&mut op).is_err());
    }

    /// **A dropped I/O waits for its device**, and a dropped lookup does not.
    #[test]
    fn a_dropped_io_is_waited_out_and_a_lookup_is_not() {
        sys::reset();
        sys::script_next(0, 1, false);
        drop(Op::submit(RawHandle(1), &read_op()).unwrap());
        assert_eq!(sys::closed_in_flight(), 0);
        sys::script_next(0, 2, false);
        drop(Op::from_po(sys::ns_lookup(1, b"/x", 0) as u64));
        assert_eq!(sys::closed_in_flight(), 1);
    }

    /// **A ninth operation is polled again**, where `block_on` waited for ever on its first eight.
    #[test]
    fn a_ninth_operation_is_polled_again() {
        sys::reset();
        let mut ops: std::vec::Vec<Op> = (0..9)
            .map(|i| {
                if i < 8 {
                    sys::script_never()
                } else {
                    sys::script_next(0, 9, false)
                }
                Op::submit(RawHandle(1), &read_op()).unwrap()
            })
            .collect();
        let first = block_on(core::future::poll_fn(|cx| {
            ops.iter_mut()
                .find_map(|op| match Pin::new(op).poll(cx) {
                    Poll::Ready(r) => Some(Poll::Ready(r)),
                    Poll::Pending => None,
                })
                .unwrap_or(Poll::Pending)
        }));
        assert_eq!(first.unwrap().result, 9);
        // The other eight never finish, so their drops would wait for ever.
        core::mem::forget(ops);
    }

    /// **`block_on` sleeps with nothing to wait on**, where it spun on a wait the kernel refuses.
    #[test]
    fn block_on_sleeps_with_nothing_to_wait_on() {
        sys::reset();
        let mut polls = 0;
        block_on(core::future::poll_fn(|_| {
            polls += 1;
            if polls < 3 { Poll::Pending } else { Poll::Ready(()) }
        }));
        assert_eq!(sys::sleeps(), 2);
        assert_eq!(sys::counts().2, 1, "its timer closed");
    }

    /// **A failed wait sleeps before the next poll**, rather than polling again at once.
    #[test]
    fn a_failed_wait_sleeps_before_the_next_poll() {
        sys::reset();
        sys::script_next(0, 5, false);
        sys::fail_next_wait(-4);
        let op = Op::submit(RawHandle(1), &read_op()).unwrap();
        assert_eq!(block_on(op).unwrap().result, 5);
        assert_eq!(sys::sleeps(), 1);
    }

    #[test]
    fn negative_status_maps_to_error_kind() {
        sys::reset();
        sys::script_next(-10, 0, true); // KError::NotFound
        let op = Op::submit(RawHandle(1), &read_op()).unwrap();
        let e = block_on(op).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::NotFound);
    }

    #[test]
    fn submit_error_is_returned_synchronously() {
        sys::reset();
        sys::fail_next_submit(-2); // KError::NoAccess
        let e = Op::submit(RawHandle(1), &read_op()).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::PermissionDenied);
        let (_subs, _waits, closes) = sys::counts();
        assert_eq!(closes, 0, "a failed submit yields no PO to close");
    }

    #[test]
    fn async_await_drives_through_block_on() {
        sys::reset();
        sys::script_next(0, 99, false);
        let fut = async {
            let op = Op::submit(RawHandle(1), &read_op())?;
            let done = op.await?;
            Ok::<u64, Error>(done.result)
        };
        let v = block_on(fut).unwrap();
        assert_eq!(v, 99);
    }

    #[test]
    fn sequential_awaits_each_complete() {
        sys::reset();
        sys::script_next(0, 1, false);
        sys::script_next(0, 2, true);
        let fut = async {
            let a = Op::submit(RawHandle(1), &read_op())?.await?;
            let b = Op::submit(RawHandle(2), &read_op())?.await?;
            Ok::<(u64, u64), Error>((a.result, b.result))
        };
        let (a, b) = block_on(fut).unwrap();
        assert_eq!((a, b), (1, 2));
    }
}
