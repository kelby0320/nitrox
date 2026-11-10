//! Blocking on objects, and waking.

use super::*;
use crate::syscall::error::{KError, SysResult};

/// Move a `Blocked` thread from `blocked` to `ready` (state `Ready`). Caller holds
/// `SCHED`. Returns `false` (no-op) if `thread` is not in `blocked` — the backstop for a
/// second waker after the wake-CAS already claimed it. The reference moves `blocked`→`ready`
/// with no refcount change. `thread` is only compared, never followed.
pub(super) fn make_runnable(g: &mut SchedState, thread: *const Thread) -> bool {
    let Some(i) = g.blocked.iter().position(|r| core::ptr::eq(&**r, thread)) else {
        return false;
    };
    let r = g.blocked.remove(i);
    r.set_state(&mut g.tok, ThreadState::Ready);
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
    /// Registration exceeded a per-object or heap reserve, or named a kind no thread waits on.
    OutOfMemory,
    /// The thread's process is exiting, so it does not block: its caller unwinds to the return to
    /// ring 3, where it ends. A wait the exit found blocked is woken, and answers as a timeout.
    /// Never from [`wait_on_io`].
    Ended,
}

/// Register the current thread as a waiter on every object in `objs`, of any kind, with an
/// optional `deadline_ns` (absolute monotonic; `0` = poll, `u64::MAX` = no timeout), block until
/// one fires, then unregister and report which signaled. `now` is the current monotonic time.
/// Registration and block happen under a single `SCHED` hold, so a waker cannot slip between them —
/// no lost wakeup. Each object's borrow pins it until this returns. A kind no thread waits on is
/// never ready: a poll finds it so, and a wait that would block on it is refused as over a reserve.
pub fn wait_on(objs: &[&ObjectRef], deadline_ns: u64, now: u64) -> WaitResult {
    wait(objs, deadline_ns, now, true)
}

/// `sys_wait` on no handles: sleep until `deadline_ns`, on the deadline slot every thread holds,
/// so it is never refused, and answer `TimedOut`. A poll or a wait for ever on nothing is
/// `InvalidArgument`.
pub fn sleep_until(deadline_ns: u64, now: u64) -> SysResult {
    if deadline_ns == 0 || deadline_ns == u64::MAX {
        return Err(KError::InvalidArgument);
    }
    match wait_on(&[], deadline_ns, now) {
        WaitResult::OutOfMemory => Err(KError::OutOfMemory),
        _ => Err(KError::TimedOut),
    }
}

/// [`wait_on`] for a wait that I/O in flight depends on: it never answers [`WaitResult::Ended`],
/// and one the process's exit woke answers as a timeout, for the caller to wait again.
pub fn wait_on_io(objs: &[&ObjectRef], deadline_ns: u64, now: u64) -> WaitResult {
    wait(objs, deadline_ns, now, false)
}

/// [`wait_on`], answering [`WaitResult::Ended`] only if `heed_exit`.
fn wait(refs: &[&ObjectRef], deadline_ns: u64, now: u64, heed_exit: bool) -> WaitResult {
    if refs.len() > MAX_WAIT_HANDLES {
        return WaitResult::OutOfMemory;
    }
    let mut addrs = [0usize; MAX_WAIT_HANDLES];
    for (a, o) in addrs.iter_mut().zip(refs) {
        *a = o.as_ptr() as usize;
    }
    let objs = &addrs[..refs.len()];
    let me_ptr;
    {
        let mut g = SCHED.lock();
        let (me, tok) = g.running();
        me_ptr = addr(me);
        // Under the hold `end_siblings` marks in: a thread marked before this blocks nowhere,
        // and one marked after is in `blocked`, where the marking wakes it.
        if heed_exit && me.is_ending(tok) {
            return WaitResult::Ended;
        }

        // Fast path: any object already signaled?
        let mut signaled = [false; MAX_WAIT_HANDLES];
        let mut any = false;
        for (i, o) in refs.iter().enumerate() {
            if obj_already_signaled(&g.tok, o, now) {
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
        let (me, tok) = g.running();
        me.wait_register(tok, objs, has_deadline);
        let mut registered = 0usize;
        let mut failed = false;
        for o in refs {
            if obj_add_waiter(&mut g.tok, o, me_ptr).is_err() {
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
            let (me, tok) = g.running();
            for o in &refs[..registered] {
                obj_remove_waiter(tok, o, me_ptr);
            }
            me.wait_clear(tok);
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
        // This CPU's `current` is the thread that blocked: it runs on its own stack.
        let (me, tok) = g.running();
        debug_assert_eq!(addr(me), me_ptr);
        let bits = me.wait_signaled(tok);
        for o in refs {
            obj_remove_waiter(tok, o, me_ptr);
        }
        let has_deadline = me.wait_has_deadline(tok);
        me.wait_clear(tok);
        if has_deadline {
            deadline::remove(&mut g.deadlines, deadline::Target::Thread(me_ptr as usize));
        }
        signaled = bits.contains(&true).then_some(bits);
    }
    match signaled {
        Some(bits) => WaitResult::Signaled(bits),
        None => WaitResult::TimedOut,
    }
}
