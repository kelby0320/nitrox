//! The async core: the [`Op`] future over `sys_wait`, and [`block_on`].
//!
//! An [`Op`] wraps an in-flight `PendingOperation`. Under [`block_on`] its first poll registers the
//! PO with the reactor (via the `Waker`) and yields `Pending`; [`block_on`] does one blocking
//! `sys_wait` on the registered handles, keeps what it read, and re-polls, and the `Op` takes its
//! result from there: **one wait an operation**. A poll that finds nothing kept, and every poll
//! under another executor, does a non-blocking `sys_wait` on the PO. There is no ready queue
//! (deferred; see `docs/architecture/libos.md`).

use core::future::Future;
use core::pin::Pin;
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use libkern::{IoOp, IoResult, KError, RawHandle};

use crate::error::{Error, Result};
use crate::sys;
use crate::wait::Signalled;

/// Max handles one poll round registers, under the kernel's 32 to keep `block_on`'s stack small.
const MAX_WAIT: usize = 8;

/// How long `block_on` sleeps with nothing to wait on, before it polls again.
const IDLE_NS: u64 = 1_000_000;

/// Polls that left a pending `Op` registered with no reactor, so `block_on` must poll again.
static UNWATCHED: AtomicUsize = AtomicUsize::new(0);

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

/// Per-`block_on` set of waitable handles the current poll round registered, reached through the
/// `Waker`'s data pointer; atomic, as the `Waker` is `Sync`.
struct Reactor {
    handles: [AtomicU64; MAX_WAIT],
    /// Slots claimed this round, past `MAX_WAIT` if a handle was left out.
    claimed: AtomicUsize,
    /// What the round's wait read, `[handle, status and reserved, result]` each, for the poll
    /// after it alone; a handle of `0` is a slot empty or taken.
    read: [[AtomicU64; 3]; MAX_WAIT],
}

impl Reactor {
    fn new() -> Self {
        let zero = || AtomicU64::new(0);
        Reactor {
            handles: core::array::from_fn(|_| zero()),
            claimed: AtomicUsize::new(0),
            read: core::array::from_fn(|_| [zero(), zero(), zero()]),
        }
    }

    /// Keep what a wait read, in place of what the last one did.
    fn keep(&self, read: &[IoResult]) {
        for (i, slot) in self.read.iter().enumerate() {
            let r = read.get(i).copied().unwrap_or(IoResult { handle: 0, status: 0, reserved: 0, result: 0 });
            slot[1].store(((r.reserved as u64) << 32) | r.status as u32 as u64, Relaxed);
            slot[2].store(r.result, Relaxed);
            slot[0].store(r.handle, Relaxed);
        }
    }

    /// Take what the round's wait read for `po`, if it read it.
    fn take_read(&self, po: u64) -> Option<IoResult> {
        let slot = self.read.iter().find(|slot| slot[0].load(Relaxed) == po)?;
        slot[0].store(0, Relaxed);
        let packed = slot[1].load(Relaxed);
        Some(IoResult {
            handle: po,
            status: packed as u32 as i32,
            reserved: (packed >> 32) as u32,
            result: slot[2].load(Relaxed),
        })
    }

    /// Register a waitable handle for this poll round (deduplicated; capped at `MAX_WAIT`).
    fn register(&self, h: u64) {
        let n = self.claimed.load(Relaxed).min(MAX_WAIT);
        if self.handles[..n].iter().any(|s| s.load(Relaxed) == h) {
            return;
        }
        if let Some(slot) = self.handles.get(self.claimed.fetch_add(1, Relaxed)) {
            slot.store(h, Relaxed);
        }
    }

    /// Take and clear the round's registrations.
    fn take(&self) -> Registry {
        let n = self.claimed.swap(0, Relaxed);
        let mut r = Registry { handles: [0; MAX_WAIT], count: n.min(MAX_WAIT), dropped: n > MAX_WAIT };
        for (to, from) in r.handles.iter_mut().zip(&self.handles) {
            *to = from.swap(0, Relaxed);
        }
        r
    }
}

/// A deadline [`IDLE_NS`] from now; a poll if the clock cannot be read.
fn soon() -> u64 {
    crate::time::now().map_or(0, |now| now.saturating_add(IDLE_NS))
}

/// Sleep [`IDLE_NS`], in a wait on no handles, which the kernel never refuses.
fn sleep() {
    sys::wait(&[], &mut [], soon());
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
/// Its drop **waits out an unfinished I/O**, whose device may still use the buffer, then closes it.
#[must_use = "an Op does nothing unless awaited or passed to block_on"]
#[derive(Debug)]
pub struct Op {
    po: u64,
    kind: Kind,
    finished: bool,
    /// Whether `poll` has returned `Ready`, so its result is handed out once.
    done: bool,
    /// Whether it has been registered with a `block_on`, whose wait would then have read it.
    registered: bool,
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
        Ok(Op::of(po as u64, Kind::Io))
    }

    /// Wrap a lookup's PO; its drop waits for nothing, as the kernel frees a lookup closed unread.
    pub(crate) fn from_po(po: u64) -> Op {
        Op::of(po, Kind::Lookup)
    }

    /// Wrap a PO whose completion is its status alone, such as a held send's; `0`, a send the
    /// kernel delivered within the call, is complete already.
    pub(crate) fn status(po: u64) -> Op {
        let mut op = Op::of(po, Kind::Status);
        op.finished = po == 0;
        op
    }

    fn of(po: u64, kind: Kind) -> Op {
        Op { po, kind, finished: false, done: false, registered: false }
    }

    /// The `PendingOperation` a [`WaitSet`](crate::WaitSet) waits on for this operation.
    pub(crate) fn po(&self) -> u64 {
        self.po
    }

    /// Take this operation's completion from what a wait on a [`WaitSet`](crate::WaitSet) it was
    /// added to read, if that wait found it signalled; `None` leaves it in flight. Once taken it
    /// is handed out, as a `Ready` poll's is, and its drop waits for nothing.
    pub fn completion_in<const N: usize>(&mut self, signalled: &Signalled<N>) -> Option<Result<IoResult>> {
        if self.done {
            return None;
        }
        let res = *signalled.iter().find(|(_, r)| r.handle == self.po)?.1;
        (self.finished, self.done) = (true, true);
        Some(if res.status < 0 { Err(Error::from_status(res.status)) } else { Ok(res) })
    }

    /// Wait for the operation to finish, here: its completion, whatever status it carries, or the
    /// error that refused the wait for good, when the operation may still be running.
    ///
    /// Where [`block_on`] answers both as an `Err`, this tells a device's failed I/O, after which
    /// the buffer it borrowed is the caller's again, from a wait that failed, after which it may
    /// still be the device's.
    pub fn wait_out(mut self) -> Result<IoResult> {
        let mut r = zeroed_results::<1>();
        loop {
            match sys::wait(&[self.po], &mut r, u64::MAX) {
                1 => break,
                n if n == KError::OutOfMemory as i64 => sleep(),
                n => {
                    // The drop must not wait for it again: that wait is what was just refused.
                    (self.finished, self.done) = (true, true);
                    return Err(Error::from_status(n as i32));
                }
            }
        }
        (self.finished, self.done) = (true, true);
        Ok(r[0])
    }
}

/// What a PO's drop has to see to: an I/O may still use its buffer, a lookup read may have
/// installed a handle, and a status is only itself.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
    Io,
    Lookup,
    Status,
}

impl Future for Op {
    type Output = Result<IoResult>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut(); // `Op: Unpin` — no self-referential state.
        if this.done {
            // Handed out already: a second `Ok` would give its handle two owners.
            return Poll::Ready(Err(Error::from_status(KError::InvalidHandle as i32)));
        }
        if this.po == 0 {
            this.done = true;
            return Poll::Ready(Ok(IoResult { handle: 0, status: 0, reserved: 0, result: 0 }));
        }
        let waker = cx.waker();
        // SAFETY: only `block_on` makes a waker with `VTABLE`, its data a `Sync` `Reactor` that
        // lives for the poll.
        let reactor = core::ptr::eq(waker.vtable(), &VTABLE).then(|| unsafe { &*(waker.data() as *const Reactor) });
        // What `block_on`'s wait read for the PO, or else a poll's; a first poll under `block_on`
        // only registers, as the wait after it reads a PO done already.
        let read = match reactor.and_then(|r| r.take_read(this.po)) {
            Some(res) => Some(res),
            None if reactor.is_some() && !this.registered => None,
            None => {
                let mut r = zeroed_results::<1>();
                match sys::wait(&[this.po], &mut r, 0) {
                    n if n == KError::WouldBlock as i64 => None,
                    n if n < 1 => {
                        this.done = true;
                        return Poll::Ready(Err(Error::from_status(n as i32)));
                    }
                    _ => Some(r[0]),
                }
            }
        };
        if let Some(res) = read {
            this.finished = true;
            this.done = true;
            return Poll::Ready(if res.status < 0 { Err(Error::from_status(res.status)) } else { Ok(res) });
        }
        let Some(reactor) = reactor else {
            // Another executor's waker, or a clone of `block_on`'s: be polled again, unregistered.
            UNWATCHED.fetch_add(1, Relaxed);
            waker.wake_by_ref();
            return Poll::Pending;
        };
        this.registered = true;
        reactor.register(this.po);
        Poll::Pending
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        if self.kind == Kind::Io && !self.finished {
            // A full waiter queue refuses the wait for now; any other refusal is for good.
            while sys::wait(&[self.po], &mut zeroed_results::<1>(), u64::MAX) == KError::OutOfMemory as i64 {
                sleep();
            }
        } else if self.kind == Kind::Lookup && !self.done {
            // A wait that read the lookup installed its handle, which nothing else would close.
            let mut r = zeroed_results::<1>();
            if sys::wait(&[self.po], &mut r, 0) == 1 && r[0].status == 0 {
                sys::handle_close(r[0].result);
            }
        }
        if self.po != 0 {
            sys::handle_close(self.po);
        }
    }
}

// --- block_on: the single-task driver -------------------------------------

/// Drive one future to completion on the current thread, blocking in `sys_wait`
/// between polls. Alloc-free.
///
/// On `Pending`, waits on the handles this round's `Op`s registered, then re-polls; it sleeps
/// [`IDLE_NS`] with none or a failed wait, and waits no longer when an `Op` went unregistered.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let reactor = Reactor::new();
    let raw = RawWaker::new(&reactor as *const Reactor as *const (), &VTABLE);
    // SAFETY: `VTABLE`'s functions are valid; `reactor` (a local) outlives `waker`,
    // which never escapes this function.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = core::pin::pin!(fut);
    loop {
        let unwatched = UNWATCHED.load(Relaxed);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {
                reactor.keep(&[]);
                let reg = reactor.take();
                let handles = &reg.handles[..reg.count];
                let mut results = zeroed_results::<MAX_WAIT>();
                let short = reg.dropped || UNWATCHED.load(Relaxed) != unwatched;
                let deadline = if short { soon() } else { u64::MAX };
                let waited = reg.count > 0 && {
                    let n = sys::wait(handles, &mut results, deadline);
                    reactor.keep(&results[..(n.max(0) as usize).min(MAX_WAIT)]);
                    n >= 0 || n == KError::TimedOut as i64
                };
                if !waited {
                    sleep();
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

    /// **An operation under `block_on` is one wait**: its first poll only registers, and the poll
    /// after the wait takes what the wait read, a failure's status too.
    #[test]
    fn a_deferred_op_is_one_wait_whatever_it_completes_with() {
        for (status, result) in [(0, 7), (KError::IoError as i32, 0)] {
            sys::reset();
            sys::script_next(status, result, false); // completes on the blocking wait
            let got = block_on(Op::submit(RawHandle(1), &read_op()).unwrap());
            assert_eq!(
                got.map(|r| r.result).map_err(|e| e.code() as i32),
                if status == 0 { Ok(7) } else { Err(status) }
            );
            assert_eq!(sys::counts().1, 1, "one blocking wait, whose result the poll after it takes");
        }
    }

    /// Two operations polled in one future are read by one wait, each taking its own result.
    #[test]
    fn two_ops_polled_together_are_read_by_one_wait() {
        sys::reset();
        sys::script_next(0, 7, false);
        sys::script_next(KError::IoError as i32, 0, false);
        let (mut a, mut b) =
            (Op::submit(RawHandle(1), &read_op()).unwrap(), Op::submit(RawHandle(1), &read_op()).unwrap());
        let (mut ra, mut rb) = (None, None);
        let mut first = true;
        block_on(core::future::poll_fn(|cx| {
            // The wait reads them in the order they registered; the poll after takes them in the
            // other, so each must take its own.
            for i in if first { [0, 1] } else { [1, 0] } {
                let (op, got) = if i == 0 { (&mut a, &mut ra) } else { (&mut b, &mut rb) };
                if got.is_none()
                    && let Poll::Ready(r) = Pin::new(op).poll(cx)
                {
                    *got = Some(r.map(|r| r.result));
                }
            }
            first = false;
            if ra.is_some() && rb.is_some() { Poll::Ready(()) } else { Poll::Pending }
        }));
        assert_eq!(ra.unwrap().ok(), Some(7));
        assert_eq!(rb.unwrap().err().map(|e| e.code()), Some(KError::IoError));
        assert_eq!(sys::counts().1, 1);
    }

    /// A lookup under `block_on` is one wait, and its handle is installed once.
    #[test]
    fn a_lookup_is_one_wait_and_one_handle() {
        sys::reset();
        // SAFETY: the mock's namespace, open for the test.
        let ns = unsafe {
            crate::Ref::<crate::Namespace, crate::NsReadOnly>::from_raw(
                RawHandle(sys::served("/s")),
                libkern::Rights::empty(),
            )
        };
        let (waits, open) = (sys::counts().1, sys::open_handles());
        let got: crate::Handle<crate::Memory, crate::MapRead> =
            block_on(ns.lookup("/s/x", libkern::Rights::MAP_READ)).unwrap();
        assert_eq!(sys::counts().1, waits + 1);
        assert_eq!(sys::open_handles(), open + 1, "the handle it resolved to, and its PO closed");
        drop(got);
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
        let unwatched = UNWATCHED.load(Relaxed);
        assert!(Pin::new(&mut op).poll(&mut Context::from_waker(&kept)).is_pending());
        assert_ne!(UNWATCHED.load(Relaxed), unwatched);
    }

    /// **An `Op` polled through another waker is polled again**, where `block_on` waited for ever
    /// on those that registered.
    #[test]
    fn an_op_polled_through_another_waker_is_polled_again() {
        sys::reset();
        sys::script_next(0, 11, false);
        let mut a = Op::submit(RawHandle(1), &read_op()).unwrap();
        sys::script_never();
        let mut b = Op::submit(RawHandle(1), &read_op()).unwrap();
        let got = block_on(core::future::poll_fn(|cx| {
            let clone = cx.waker().clone();
            if let Poll::Ready(r) = Pin::new(&mut a).poll(&mut Context::from_waker(&clone)) {
                return Poll::Ready(r);
            }
            Pin::new(&mut b).poll(cx)
        }));
        assert_eq!(got.unwrap().result, 11);
        core::mem::forget(b);
    }

    /// **Ops polled on other threads with `block_on`'s waker**, which is `Sync`, race on nothing.
    #[test]
    fn ops_polled_on_other_threads_race_on_nothing() {
        sys::reset();
        block_on(core::future::poll_fn(|cx| {
            let waker = cx.waker();
            std::thread::scope(|s| {
                for _ in 0..2 {
                    s.spawn(|| {
                        sys::script_next(0, 1, false);
                        let mut op = Op::submit(RawHandle(1), &read_op()).unwrap();
                        assert!(Pin::new(&mut op).poll(&mut Context::from_waker(waker)).is_pending());
                    });
                }
            });
            Poll::Ready(())
        }));
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
        drop(op);
        assert_eq!(sys::counts().1, waits, "a finished I/O's drop waits on nothing");
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
        let ns = sys::served("/x");
        drop(Op::from_po(sys::ns_lookup(ns, b"/x", 0) as u64));
        assert_eq!(sys::closed_in_flight(), 1);
    }

    /// **A completion is told from a failed wait**: an I/O the device failed is finished, its
    /// status carried, where a wait refused for good says it may still run.
    #[test]
    fn waiting_out_an_op_tells_a_failed_io_from_a_failed_wait() {
        sys::reset();
        sys::script_next(KError::IoError as i32, 3, false);
        let done = Op::submit(RawHandle(1), &read_op()).unwrap().wait_out().unwrap();
        assert_eq!((done.status, done.result), (KError::IoError as i32, 3));
        sys::script_next(0, 1, false);
        let op = Op::submit(RawHandle(1), &read_op()).unwrap();
        sys::fail_next_wait(KError::OutOfMemory as i64);
        assert_eq!(op.wait_out().unwrap().result, 1, "a full waiter queue is waited out");
        sys::script_never();
        let op = Op::submit(RawHandle(1), &read_op()).unwrap();
        sys::fail_next_wait(KError::InvalidHandle as i64);
        let closes = sys::counts().2;
        assert_eq!(op.wait_out().err().unwrap().code(), KError::InvalidHandle);
        assert_eq!(sys::counts().2, closes + 1, "closed, and not waited on again");
    }

    /// **A dropped I/O whose wait is refused for now waits again**, rather than closing in flight.
    #[test]
    fn a_dropped_io_whose_wait_is_refused_for_now_waits_again() {
        sys::reset();
        sys::script_next(0, 1, false);
        let op = Op::submit(RawHandle(1), &read_op()).unwrap();
        sys::fail_next_wait(KError::OutOfMemory as i64);
        drop(op);
        assert_eq!(sys::closed_in_flight(), 0);
        assert_eq!(sys::sleeps(), 1);
    }

    /// **A dropped lookup closes the handle a wait installed**, which nothing else would close.
    #[test]
    fn a_dropped_lookup_closes_the_handle_a_wait_installed() {
        sys::reset();
        let ns = sys::served("/x");
        let mut lookup = Op::from_po(sys::ns_lookup(ns, b"/x", 0) as u64);
        sys::script_next(0, 5, false);
        let mut io = Op::submit(RawHandle(1), &read_op()).unwrap();
        let mut first = true;
        block_on(core::future::poll_fn(|cx| {
            if core::mem::take(&mut first) {
                assert!(Pin::new(&mut lookup).poll(cx).is_pending());
            }
            Pin::new(&mut io).poll(cx)
        }))
        .unwrap();
        let open = sys::open_handles();
        drop(lookup);
        assert_eq!(sys::open_handles(), open - 2, "its PO, and the handle its wait installed");
        let closes = sys::counts().2;
        drop(Op::from_po(sys::ns_lookup(ns, b"/y", 0) as u64));
        assert_eq!(sys::counts().2, closes + 1, "a failed lookup's PO, and no handle");
    }

    /// **A ninth operation is polled again**, where `block_on` waited for ever on its first eight.
    #[test]
    fn a_ninth_operation_is_polled_again() {
        sys::reset();
        let mut ops = eight_never_and_a_ninth();
        let first = block_on(core::future::poll_fn(|cx| first_ready(&mut ops, cx)));
        assert_eq!(first.unwrap().result, 9);
        assert_eq!(sys::sleeps(), 1, "a deadline wait that timed out, which is no failure");
        // The other eight never finish, so their drops would wait for ever.
        core::mem::forget(ops);
    }

    /// **A failed wait in a round that left a handle out sleeps** before the next poll too.
    #[test]
    fn a_failed_wait_past_the_eighth_sleeps_before_the_next_poll() {
        sys::reset();
        let mut ops = eight_never_and_a_ninth();
        sys::fail_next_wait(-4);
        let mut slept = std::vec::Vec::new();
        block_on(core::future::poll_fn(|cx| {
            slept.push(sys::sleeps());
            first_ready(&mut ops, cx)
        }))
        .unwrap();
        assert_eq!(slept, [0, 1]);
        core::mem::forget(ops);
    }

    /// Nine `Op`s, the first eight never signalling and the ninth on the next wait.
    fn eight_never_and_a_ninth() -> std::vec::Vec<Op> {
        (0..9)
            .map(|i| {
                if i < 8 {
                    sys::script_never()
                } else {
                    sys::script_next(0, 9, false)
                }
                Op::submit(RawHandle(1), &read_op()).unwrap()
            })
            .collect()
    }

    fn first_ready(ops: &mut [Op], cx: &mut Context<'_>) -> Poll<Result<IoResult>> {
        ops.iter_mut()
            .find_map(|op| match Pin::new(op).poll(cx) {
                Poll::Ready(r) => Some(Poll::Ready(r)),
                Poll::Pending => None,
            })
            .unwrap_or(Poll::Pending)
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
        assert_eq!(sys::last_wait_count(), 0, "a sleep on no handle, which none can refuse");
    }

    /// **An `Op` polled twice in a round is waited on once**, its handle registered once.
    #[test]
    fn an_op_polled_twice_in_a_round_is_waited_on_once() {
        sys::reset();
        sys::script_next(0, 3, false);
        let mut op = Op::submit(RawHandle(1), &read_op()).unwrap();
        let mut handed = std::vec::Vec::new();
        let r = block_on(core::future::poll_fn(|cx| {
            handed.push(sys::last_wait_count());
            match Pin::new(&mut op).poll(cx) {
                Poll::Ready(r) => Poll::Ready(r),
                Poll::Pending => Pin::new(&mut op).poll(cx),
            }
        }));
        assert_eq!(r.unwrap().result, 3);
        assert_eq!(handed, [0, 1]);
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
