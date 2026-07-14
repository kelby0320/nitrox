//! The async core: the [`Op`] future over `sys_wait`, and [`block_on`].
//!
//! An [`Op`] wraps an in-flight `PendingOperation`; polling it does a non-blocking
//! `sys_wait` on the PO. If not ready, it registers the PO with the running
//! [`block_on`] reactor (via the `Waker`) and yields `Pending`; [`block_on`] then does
//! one blocking `sys_wait` on the registered handles and re-polls. This is the
//! single-task executor — the same poll→wait→re-poll loop a multi-task version would
//! run, without a ready queue (deferred; see `docs/architecture/libos.md`).

use core::cell::Cell;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use libkern::{IoOp, IoResult, RawHandle};

use crate::error::{Error, Result};
use crate::sys;

/// Max handles a single `sys_wait` (and thus one poll round) can register — mirrors
/// the kernel's `MAX_WAIT_HANDLES`.
const MAX_WAIT: usize = 8;

/// An `[IoResult; N]` of zeroes (no `Copy`/`Default` bound needed).
fn zeroed_results<const N: usize>() -> [IoResult; N] {
    core::array::from_fn(|_| IoResult {
        handle: 0,
        status: 0,
        reserved: 0,
        result: 0,
    })
}

// --- the reactor: which handles a Pending future wants waited on ----------

#[derive(Clone, Copy)]
struct Registry {
    handles: [u64; MAX_WAIT],
    count: usize,
}

/// Per-`block_on` set of waitable handles the current poll round registered.
/// `Cell`-based interior mutability: single-threaded, shared through the `Waker`'s
/// data pointer *and* `block_on`, mutated by neither via `&mut`.
struct Reactor {
    reg: Cell<Registry>,
}

impl Reactor {
    fn new() -> Self {
        Reactor {
            reg: Cell::new(Registry {
                handles: [0; MAX_WAIT],
                count: 0,
            }),
        }
    }

    /// Register a waitable handle for this poll round (deduplicated; capped at
    /// `MAX_WAIT` — excess is dropped, which only costs an extra poll round).
    fn register(&self, h: u64) {
        let mut r = self.reg.get();
        if r.handles[..r.count].iter().any(|&x| x == h) {
            return;
        }
        if r.count < MAX_WAIT {
            r.handles[r.count] = h;
            r.count += 1;
            self.reg.set(r);
        }
    }

    /// Take and clear the round's registrations.
    fn take(&self) -> Registry {
        let r = self.reg.get();
        self.reg.set(Registry {
            handles: [0; MAX_WAIT],
            count: 0,
        });
        r
    }
}

// --- the Waker: a thin handle onto the reactor ----------------------------
//
// libos owns both the `Op` future and `block_on`, so the Waker's data pointer
// references the `block_on` `Reactor` by convention; `Op::poll` reads it back via
// `cx.waker().data()`. `wake`/`clone`/`drop` are no-ops: `block_on` re-polls after
// every `sys_wait` (the wait is the wake), and the Waker never escapes `block_on`.

static VTABLE: RawWakerVTable = RawWakerVTable::new(waker_clone, waker_noop, waker_noop, waker_noop);

unsafe fn waker_clone(data: *const ()) -> RawWaker {
    RawWaker::new(data, &VTABLE)
}

unsafe fn waker_noop(_data: *const ()) {}

// --- Op: the core future --------------------------------------------------

/// A future over an in-flight `PendingOperation`. Resolves to the operation's
/// [`IoResult`] (or an [`Error`] if it completed with a negative status). Closes the
/// PO on drop.
#[must_use = "an Op does nothing unless awaited or passed to block_on"]
#[derive(Debug)]
pub struct Op {
    po: u64,
    /// Set once resolved, so a stray re-poll after `Ready` is a no-op.
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
        Ok(Op::from_po(po as u64))
    }

    /// Wrap an already-submitted `PendingOperation` handle as a future. Used by
    /// syscalls that return a PO directly (e.g. `sys_ns_lookup`) rather than via
    /// `io_submit`.
    pub(crate) fn from_po(po: u64) -> Op {
        Op { po, done: false }
    }
}

impl Future for Op {
    type Output = Result<IoResult>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut(); // `Op: Unpin` — no self-referential state.
        // Non-blocking check: is the PO complete?
        let mut r = zeroed_results::<1>();
        let n = sys::wait(&[this.po], &mut r, 0);
        if n >= 1 {
            this.done = true;
            let res = r[0];
            if res.status < 0 {
                return Poll::Ready(Err(Error::from_status(res.status)));
            }
            return Poll::Ready(Ok(res));
        }
        // Not ready: register the PO so block_on's blocking `sys_wait` covers it.
        // SAFETY: libos's `block_on` is the only thing that constructs the Waker, and
        // it always sets the data pointer to a live `Reactor` for the poll's duration.
        let reactor = unsafe { &*(cx.waker().data() as *const Reactor) };
        reactor.register(this.po);
        Poll::Pending
    }
}

impl Drop for Op {
    fn drop(&mut self) {
        sys::handle_close(self.po);
    }
}

// --- block_on: the single-task driver -------------------------------------

/// Drive one future to completion on the current thread, blocking in `sys_wait`
/// between polls. Alloc-free.
///
/// Polls `fut`; on `Pending`, does one blocking `sys_wait` on the handles the pending
/// `Op`s registered this round, then re-polls. Assumes futures do not persist the
/// `Waker` beyond the call (true for libos `Op`s); a multi-task executor with a
/// refcounted waker is the deferred generalization.
pub fn block_on<F: Future>(fut: F) -> F::Output {
    let reactor = Reactor::new();
    let raw = RawWaker::new(&reactor as *const Reactor as *const (), &VTABLE);
    // SAFETY: `VTABLE`'s functions are valid; `reactor` (a local) outlives `waker`,
    // which never escapes this function.
    let waker = unsafe { Waker::from_raw(raw) };
    let mut cx = Context::from_waker(&waker);
    let mut fut = core::pin::pin!(fut);
    loop {
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => return v,
            Poll::Pending => {
                let reg = reactor.take();
                let mut results = zeroed_results::<MAX_WAIT>();
                if reg.count == 0 {
                    // Pending but nothing registered (a non-I/O yield). Poll-yield
                    // rather than spin tightly; libos `Op`s always register, so this
                    // path is only reachable from a hand-written future.
                    sys::wait(&[], &mut results, 0);
                } else {
                    sys::wait(&reg.handles[..reg.count], &mut results, u64::MAX);
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
        IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: 0,
            buf_offset: 0,
            offset: 0,
            length: 0,
        }
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
        // poll(check:not-ready) + blocking wait + poll(check:ready) = 3 waits.
        assert!(waits >= 2, "expected a poll + a blocking wait, got {waits}");
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
        // Proves the `async`/`await` desugaring works through block_on with a
        // deferred completion — the shape init/eshell will use.
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
