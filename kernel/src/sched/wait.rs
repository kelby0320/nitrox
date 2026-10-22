//! Blocking on objects, and waking.

use super::*;

/// Park the current thread (`Blocked`, into `blocked`) and switch to the next runnable one,
/// mirroring [`switch_to_next`]'s IF-bracket but **not** re-enqueuing it. The caller
/// ([`wait_on`]) has already registered it on its objects and the deadline heap under this
/// same `SCHED` hold, so there is no lost-wakeup window. Resumes lock-free when a waker
/// calls [`make_runnable`] and the scheduler picks it. Consumes the guard.
fn block_current_and_switch(mut g: IrqSpinLockGuard<'_, SchedState>) {
    let next = match pick_next(&mut g) {
        Some(n) => n,
        None => g.idle_slot().take().expect("idle thread exists after init"),
    };
    let prev = g.cur_slot().take().expect("current set");
    let prev_obj = prev.as_ptr();
    let next_obj = next.as_ptr();
    // SAFETY: both pinned (prev parked in `blocked`, next becoming current);
    // `SCHED` held — the Thread accessor contract. (The idle thread never
    // blocks, so `prev` is never the idle thread.)
    unsafe {
        Thread::set_state(prev_obj, ThreadState::Blocked);
        Thread::set_state(next_obj, ThreadState::Running);
    }
    // SAFETY: as above.
    let prev_slot = unsafe { Thread::saved_sp_mut_ptr(prev_obj) };

    // Park prev in `blocked` (NOT ready/idle) — its `ObjectRef` keeps it alive.
    push_reserved(&mut g.blocked, prev, "blocked list");
    *g.cur_slot() = Some(next);

    // Switch into `next`; we resume here (lock-free) when a waker moves us back
    // to `ready` and the scheduler later picks us.
    // SAFETY: `prev_slot` is the outgoing (now-`Blocked`, pinned-in-`blocked`)
    // thread's saved-SP slot; `prev_obj`/`next_obj` are the pinned outgoing /
    // incoming threads.
    unsafe { switch_into(g, prev_slot, prev_obj, next_obj) };
}

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
    // room). Failing means every permitted queue is full, which the thread cap rules out.
    if let Err(returned) = place_thread(g, r, true) {
        // **Do not let `returned` drop here.** `SCHED` is held, and an `ObjectRef` drop can
        // reach `dispatch_destroy` → `SlabCache::free` — a plain spinlock under the rank-1
        // lock (`kernel/docs/lock-ordering.md`). The spawn callers avoid it by carrying the
        // ref out of the locked block with a `match`; here there is nowhere to carry it
        // *to*, because the next statement ends the machine.
        //
        // `if place_thread(..).is_err()` would do exactly the forbidden thing: an `if`
        // condition is its own temporary scope, so the `Result` — and the `ObjectRef` inside
        // it — would drop at the end of the condition, *before* the panic, with `SCHED`
        // still held. Reserve exhaustion is the state this panic exists for, and the drop
        // would spin on the allocator lock while another CPU's tick spins on `SCHED` with
        // IRQs masked — a hang instead of the diagnostic.
        //
        // Leaked deliberately: `panic = "abort"` means nothing outlives this call.
        // `deferred_drops` is worse here: pushing to it can itself grow under `SCHED`, and
        // nothing will ever drain it.
        core::mem::forget(returned);
        panic!(
            "wake placement: no ready queue would take the thread — every affinity-permitted \
             queue is at reserve; or affinity permits no CPU that accepts work and every CPU \
             that does is at reserve; or no CPU accepts work and this one is at reserve"
        );
    }
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
    debug_assert!(objs.len() <= MAX_WAIT_HANDLES);
    let me_ptr;
    {
        let mut g = SCHED.lock();
        me_ptr = g.cur_ref().as_ref().expect("current set when a thread runs").as_ptr();

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
            failed = deadline::push(
                &mut g.deadlines,
                deadline::Entry {
                    deadline_ns,
                    target: me_ptr as usize,
                    kind: deadline::DeadlineKind::Thread,
                    channel: 0,
                },
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

        // Block. Registration above + this block are one uninterrupted hold.
        block_current_and_switch(g); // consumes g; resumes here when woken
    }

    // Resumed (woken by a signal or the deadline). Unregister everything under a fresh
    // hold, snapshot which slots fired, then build the result.
    let mut snap = [(0usize, false); MAX_WAIT_HANDLES];
    let signaled;
    {
        let mut g = SCHED.lock();
        // SAFETY: live Thread (we run on it), `SCHED` held.
        let n = unsafe { Thread::wait_snapshot(me_ptr, &mut snap) };
        for &o in objs {
            // SAFETY: live Timer (caller still holds its `ObjectRef`), `SCHED` held.
            unsafe { obj_remove_waiter(o as *mut (), me_ptr) };
        }
        // SAFETY: live Thread, `SCHED` held.
        if unsafe { Thread::wait_has_deadline(me_ptr) } {
            deadline::remove(&mut g.deadlines, me_ptr as usize, deadline::DeadlineKind::Thread);
        }
        // SAFETY: live Thread, `SCHED` held.
        unsafe { Thread::wait_clear(me_ptr) };

        let mut bits = [false; MAX_WAIT_HANDLES];
        let mut any = false;
        for i in 0..n {
            bits[i] = snap[i].1;
            any |= snap[i].1;
        }
        signaled = if any { Some(bits) } else { None };
    }
    match signaled {
        Some(bits) => WaitResult::Signaled(bits),
        None => WaitResult::TimedOut,
    }
}
