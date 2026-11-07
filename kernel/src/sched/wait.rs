//! Blocking on objects, and waking.

use super::*;
use crate::syscall::error::{KError, SysResult};

/// Move a `Blocked` thread from `blocked` to `ready` (state `Ready`). Caller holds
/// `SCHED`. Returns `false` (no-op) if `thread` is not in `blocked` — the backstop for a
/// second waker after the wake-CAS already claimed it. The `ObjectRef` moves
/// `blocked`→`ready` with no refcount change.
pub(super) fn make_runnable(g: &mut SchedState, thread: *mut ()) -> bool {
    let Some(i) = g.blocked.iter().position(|r| r.as_ptr() == thread) else {
        return false;
    };
    let r = g.blocked.remove(i);
    // SAFETY: `r` pins `thread`; `SCHED` held.
    unsafe { Thread::set_state(thread, ThreadState::Ready) };
    // Re-home onto its CPU (a full home falls back to the least-loaded permitted queue with
    // room).
    place_woken(g, r, "wake placement (every queue the woken thread may go to)");
    true
}

/// Outcome of [`wait_on`].
pub enum WaitResult {
    /// At least one object signaled. `signaled[i]` corresponds to `objs[i]`; set bits mark
    /// the handles to report.
    Signaled([bool; MAX_WAIT_HANDLES]),
    /// Nothing signaled before the deadline (or a poll found nothing ready).
    TimedOut,
    /// Registration exceeded a per-object or heap reserve.
    OutOfMemory,
    /// The thread's process is exiting, so it does not block: its caller unwinds to the return to
    /// ring 3, where it ends. A wait the exit found blocked is woken, and answers as a timeout.
    /// Never from [`wait_on_io`].
    Ended,
}

/// Register the current thread as a waiter on every object in `objs` (type-erased
/// addresses) with an optional `deadline_ns` (absolute monotonic; `0` = poll, `u64::MAX` =
/// no timeout), block until one fires, then unregister and report which signaled. `now` is
/// the current monotonic time. Registration and block happen under a single `SCHED` hold, so
/// a waker cannot slip between them — no lost wakeup.
///
/// # Safety
///
/// Each of `objs` is the address of a live `Timer`, `NotificationChannel`, `IpcChannel`,
/// `PendingOperation` or `InterruptObject`, pinned by a reference the caller holds until this
/// returns.
pub unsafe fn wait_on(objs: &[usize], deadline_ns: u64, now: u64) -> WaitResult {
    // SAFETY: the caller's contract.
    unsafe { wait(objs, deadline_ns, now, true) }
}

/// `sys_wait` on no handles: sleep until `deadline_ns`, on the deadline slot every thread holds,
/// so it is never refused, and answer `TimedOut`. A poll or a wait for ever on nothing is
/// `InvalidArgument`.
pub fn sleep_until(deadline_ns: u64, now: u64) -> SysResult {
    if deadline_ns == 0 || deadline_ns == u64::MAX {
        return Err(KError::InvalidArgument);
    }
    // SAFETY: an empty wait set has no object to pin.
    match unsafe { wait_on(&[], deadline_ns, now) } {
        WaitResult::OutOfMemory => Err(KError::OutOfMemory),
        _ => Err(KError::TimedOut),
    }
}

/// [`wait_on`] for a wait that I/O in flight depends on: it never answers [`WaitResult::Ended`],
/// and one the process's exit woke answers as a timeout, for the caller to wait again.
///
/// # Safety
///
/// As [`wait_on`].
pub unsafe fn wait_on_io(objs: &[usize], deadline_ns: u64, now: u64) -> WaitResult {
    // SAFETY: the caller's contract.
    unsafe { wait(objs, deadline_ns, now, false) }
}

/// [`wait_on`], with its contract, answering [`WaitResult::Ended`] only if `heed_exit`.
unsafe fn wait(objs: &[usize], deadline_ns: u64, now: u64, heed_exit: bool) -> WaitResult {
    debug_assert!(objs.len() <= MAX_WAIT_HANDLES);
    let me_ptr;
    {
        let mut g = SCHED.lock();
        me_ptr = g.cur_ref().as_ref().expect("current set when a thread runs").as_ptr();
        // Under the hold `end_siblings` marks in: a thread marked before this blocks nowhere,
        // and one marked after is in `blocked`, where the marking wakes it.
        // SAFETY: live Thread (current), `SCHED` held.
        if heed_exit && unsafe { Thread::is_ending(me_ptr) } {
            return WaitResult::Ended;
        }

        // Fast path: any object already signaled?
        let mut signaled = [false; MAX_WAIT_HANDLES];
        let mut any = false;
        for (i, &o) in objs.iter().enumerate() {
            // SAFETY: `o` is a live waitable pinned by the caller's `ObjectRef`;
            // `SCHED` held. Dispatches by the object's header type.
            if unsafe { obj_already_signaled(o as *mut (), now) } {
                signaled[i] = true;
                any = true;
            }
        }
        if any {
            return WaitResult::Signaled(signaled);
        }
        if deadline_ns == 0 {
            // Poll with nothing ready.
            return WaitResult::TimedOut;
        }

        // Register the wait set on the thread, then on each object, all under this one hold
        // so it is atomic w.r.t. any waker.
        let has_deadline = deadline_ns != u64::MAX;
        // SAFETY: live Thread (current), `SCHED` held.
        unsafe { Thread::wait_register(me_ptr, objs, has_deadline) };
        let mut registered = 0usize;
        let mut failed = false;
        for &o in objs {
            // SAFETY: live waitable, `SCHED` held.
            if unsafe { obj_add_waiter(o as *mut (), me_ptr) }.is_err() {
                failed = true;
                break;
            }
            registered += 1;
        }
        if !failed && has_deadline {
            failed = deadline::push_promised(
                &mut g,
                deadline::Entry { deadline_ns, target: deadline::Target::Thread(me_ptr as usize) },
            )
            .is_err();
        }
        if failed {
            // Unwind the partial registration and bail without blocking.
            for &o in &objs[..registered] {
                // SAFETY: live Timer, `SCHED` held.
                unsafe { obj_remove_waiter(o as *mut (), me_ptr) };
            }
            // SAFETY: live Thread, `SCHED` held.
            unsafe { Thread::wait_clear(me_ptr) };
            return WaitResult::OutOfMemory;
        }

        // Block. Registration above + this block are one uninterrupted hold, so no waker slips
        // between; this resumes, lock-free, once a waker's `make_runnable` and a pick run it.
        let me = take_current(&mut g);
        switch_away(g, me, Park::Blocked);
    }

    // Resumed (woken by a signal or the deadline). Unregister everything under a fresh
    // hold, and read which slots fired.
    let signaled;
    {
        let mut g = SCHED.lock();
        // SAFETY: live Thread (we run on it), `SCHED` held.
        let bits = unsafe { Thread::wait_signaled(me_ptr) };
        for &o in objs {
            // SAFETY: live Timer (caller still holds its `ObjectRef`), `SCHED` held.
            unsafe { obj_remove_waiter(o as *mut (), me_ptr) };
        }
        // SAFETY: live Thread, `SCHED` held.
        if unsafe { Thread::wait_has_deadline(me_ptr) } {
            deadline::remove(&mut g.deadlines, deadline::Target::Thread(me_ptr as usize));
        }
        // SAFETY: live Thread, `SCHED` held.
        unsafe { Thread::wait_clear(me_ptr) };
        signaled = bits.contains(&true).then_some(bits);
    }
    match signaled {
        Some(bits) => WaitResult::Signaled(bits),
        None => WaitResult::TimedOut,
    }
}
