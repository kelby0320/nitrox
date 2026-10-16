use super::*;

// --- Waitable dispatch (Timer | NotificationChannel) -------------------
//
// `wait_on` works over type-erased object pointers; it dispatches the three
// waitable operations by the kobject type read from the `KObjectHeader` at
// offset 0 (every kobject is `#[repr(C)]` with the header first). `sys_wait`
// has already rejected non-waitables before these run.

/// The kobject type at the head of a type-erased object pointer.
///
/// # Safety
///
/// `obj` is a live kobject (header at offset 0); `SCHED` held.
unsafe fn obj_type(obj: *mut ()) -> KObjectType {
    // SAFETY: every kobject begins with a `KObjectHeader` (see object::header).
    unsafe { (*(obj as *const crate::object::header::KObjectHeader)).object_type() }
}

/// Is this waitable currently signaled? (Timer deadline elapsed / channel non-empty.)
///
/// # Safety
///
/// Live waitable, `SCHED` held.
pub(super) unsafe fn obj_already_signaled(obj: *mut (), now: u64) -> bool {
    // SAFETY: this function's contract: `obj` is a live waitable, of the type its header names, and
    // `SCHED` is held.
    unsafe {
        match obj_type(obj) {
            KObjectType::Timer => Timer::already_signaled(obj, now),
            KObjectType::NotificationChannel => NotificationChannel::already_signaled(obj),
            KObjectType::IpcChannel => IpcChannel::already_signaled(obj),
            KObjectType::PendingOperation => PendingOperation::already_signaled(obj),
            KObjectType::InterruptObject => InterruptObject::already_signaled(obj),
            _ => false,
        }
    }
}

/// Register the current thread as a waiter. `Err` over the object's reserve.
///
/// # Safety
///
/// Live waitable, `SCHED` held.
pub(super) unsafe fn obj_add_waiter(obj: *mut (), th: *mut ()) -> Result<(), ()> {
    // SAFETY: this function's contract: `obj` is a live waitable, of the type its header names, and
    // `SCHED` is held.
    unsafe {
        match obj_type(obj) {
            KObjectType::Timer => Timer::add_waiter(obj, th),
            KObjectType::NotificationChannel => NotificationChannel::add_waiter(obj, th),
            KObjectType::IpcChannel => IpcChannel::add_waiter(obj, th),
            KObjectType::PendingOperation => PendingOperation::add_waiter(obj, th),
            KObjectType::InterruptObject => InterruptObject::add_waiter(obj, th),
            _ => Err(()),
        }
    }
}

/// Unregister a waiter (idempotent).
///
/// # Safety
///
/// Live waitable, `SCHED` held.
pub(super) unsafe fn obj_remove_waiter(obj: *mut (), th: *mut ()) {
    // SAFETY: this function's contract: `obj` is a live waitable, of the type its header names, and
    // `SCHED` is held.
    unsafe {
        match obj_type(obj) {
            KObjectType::Timer => Timer::remove_waiter(obj, th),
            KObjectType::NotificationChannel => NotificationChannel::remove_waiter(obj, th),
            KObjectType::IpcChannel => IpcChannel::remove_waiter(obj, th),
            KObjectType::PendingOperation => PendingOperation::remove_waiter(obj, th),
            KObjectType::InterruptObject => InterruptObject::remove_waiter(obj, th),
            _ => {}
        }
    }
}

/// Ask `process` to exit: enqueue [`KIND_TERMINATE_REQUESTED`] on **its own**
/// notification channel and wake anything blocked there (§11h).
///
/// **The kernel does nothing else.** It does not stop threads, unwind stacks, or touch the
/// target's execution in any way — so there is no safe-point question and no second
/// teardown path beside [`exit_process`]. A process that listens exits itself; one that
/// does not keeps running, which is what "there is no forcible kill in this system" means
/// in practice.
///
/// `false` when the target has no notification channel attached — it can never be asked,
/// which the caller reports rather than silently succeeding at.
///
/// # Safety
///
/// `process` points at a live `Process`, pinned for the call by a reference the caller holds.
pub unsafe fn request_terminate(process: *mut ()) -> bool {
    let mut g = SCHED.lock();
    // SAFETY: the caller pins `process` through its Process handle for this call.
    let chan = unsafe { &*(process as *const crate::object::Process) }.notification_channel_ptr();
    let Some(c) = chan else { return false };
    let notif = Notification::terminate_requested();
    // SAFETY: `c` is the channel that Process owns a reference on, so it outlives this
    // call; `SCHED` held; the queue is pre-reserved, so no allocation happens here.
    let _edge = unsafe { NotificationChannel::enqueue(c, notif) };
    signal_channel(&mut g, c);
    true
}

/// Wake every thread blocked on `channel` (its queue just went non-empty).
/// Caller holds `SCHED`. No allocation, no blocking — safe from the fault path.
/// Mirrors [`fire_timer`]'s waiter-drain.
pub(super) fn signal_channel(g: &mut SchedState, channel: *mut ()) {
    let mut buf = [core::ptr::null_mut(); NotificationChannel::MAX_WAITERS];
    // SAFETY: live channel, `SCHED` held — drains its waiter list.
    let n = unsafe { NotificationChannel::take_waiters(channel, &mut buf) };
    for &th in &buf[..n] {
        // SAFETY: each waiter is a thread blocked in `wait_on`, pinned in
        // `blocked`; `SCHED` held.
        unsafe {
            Thread::wait_mark_signaled(th, channel as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(g, th);
            }
        }
    }
}

/// Wake every thread blocked recv-ing on `endpoint` (a message just arrived in
/// its inbox, or its peer just closed). Caller holds `SCHED`. No allocation, no
/// blocking. Mirrors [`signal_channel`] but drains an [`IpcChannel`]'s
/// recv-waiter list.
pub(super) fn signal_ipc_endpoint(g: &mut SchedState, endpoint: *mut ()) {
    let mut buf = [core::ptr::null_mut(); IpcChannel::MAX_WAITERS];
    // SAFETY: live endpoint, `SCHED` held — drains its recv-waiter list.
    let n = unsafe { IpcChannel::take_waiters(endpoint, &mut buf) };
    for &th in &buf[..n] {
        // SAFETY: each waiter is a thread blocked in `wait_on`, pinned in
        // `blocked`; `SCHED` held.
        unsafe {
            Thread::wait_mark_signaled(th, endpoint as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(g, th);
            }
        }
    }
}

/// Complete a [`PendingOperation`] with `status` and wake every thread blocked
/// on it. One-shot: a second call is a no-op (the first completion wins), so it
/// is safe to call from both the delivery path and a timeout. Caller holds
/// `SCHED`. No allocation, no blocking, no `ObjectRef` drop. Mirrors
/// [`signal_ipc_endpoint`] but drains a `PendingOperation`'s waiter list.
pub(super) fn signal_pending_op(g: &mut SchedState, po: *mut (), status: i32) {
    signal_pending_op_with_result(g, po, status, 0);
}

/// As [`signal_pending_op`], but the completion also carries a `result` payload
/// (a namespace lookup's resolved handle). `signal_pending_op` is the
/// `result = 0` wrapper for the status-only call sites (blocking IPC). Caller
/// holds `SCHED`. No allocation, no blocking, no `ObjectRef` drop.
pub(super) fn signal_pending_op_with_result(g: &mut SchedState, po: *mut (), status: i32, result: u64) {
    // SAFETY: live PO, `SCHED` held. One-shot; a re-signal returns `false`.
    if !unsafe { PendingOperation::signal_with_result(po, status, result) } {
        return; // already completed — its waiters were handled the first time
    }
    let mut buf = [core::ptr::null_mut(); PendingOperation::MAX_WAITERS];
    // SAFETY: live PO, `SCHED` held — drains its waiter list.
    let n = unsafe { PendingOperation::take_waiters(po, &mut buf) };
    for &th in &buf[..n] {
        // SAFETY: each waiter is a thread blocked in `wait_on`, pinned in
        // `blocked`; `SCHED` held.
        unsafe {
            Thread::wait_mark_signaled(th, po as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(g, th);
            }
        }
    }
}

/// Read a completed [`PendingOperation`]'s `(status, result)` under `SCHED`.
/// Called by `sys_wait` when building the `IoResult` for a signaled PO; both
/// values are stable after the one-shot completion, but the lock is taken to
/// honor the accessor contract (and to read both under one hold). The caller's
/// handle pins `po` for the duration.
///
/// # Safety
///
/// `po` points at a live `PendingOperation`, pinned for the call by a reference the caller holds.
pub unsafe fn pending_op_completion(po: *mut ()) -> (i32, u64) {
    let _g = SCHED.lock();
    // SAFETY: `po` is a live `PendingOperation` pinned by the caller's handle;
    // `SCHED` held — both reads see the same one-shot-stable completion.
    unsafe { (PendingOperation::status(po), PendingOperation::result(po)) }
}

/// Complete a [`PendingOperation`] with `(status, result)` from **syscall
/// context** (not a wakeup path), taking `SCHED` to do so. A namespace
/// direct-handle lookup uses this to **pre-signal** the PO it is about to return:
/// resolution finished synchronously, so the PO is completed in place. The PO has
/// no waiters yet (it was just created), so the wake loop is a no-op; the caller's
/// later `sys_wait` takes the already-signalled fast path. One-shot.
///
/// # Safety
///
/// `po` points at a live `PendingOperation`, pinned for the call by a reference the caller holds.
pub unsafe fn complete_pending_op(po: *mut (), status: i32, result: u64) {
    let mut g = SCHED.lock();
    signal_pending_op_with_result(&mut g, po, status, result);
}

/// Record an interrupt on `irq` (an [`InterruptObject`]) and wake every thread
/// blocked on it. Latching: an interrupt with no current waiter increments the
/// object's pending count and is delivered to the next waiter. Taking `SCHED`,
/// so it is callable from a DPC handler (interrupt-dispatch tail) — the path a
/// device ISR's completion takes. No allocation, no blocking, no `ObjectRef`
/// drop. The caller's reference (or a registered handler) pins `irq`.
///
/// # Safety
///
/// `irq` points at a live `InterruptObject`, pinned for the call by a reference the caller holds.
pub unsafe fn signal_interrupt(irq: *mut ()) {
    let mut g = SCHED.lock();
    // SAFETY: live `InterruptObject`, `SCHED` held — latches a pending interrupt.
    unsafe { InterruptObject::signal(irq) };
    let mut buf = [core::ptr::null_mut(); InterruptObject::MAX_WAITERS];
    // SAFETY: live object, `SCHED` held — drains its waiter list.
    let n = unsafe { InterruptObject::take_waiters(irq, &mut buf) };
    for &th in &buf[..n] {
        // SAFETY: each waiter is a thread blocked in `wait_on`, pinned in
        // `blocked`; `SCHED` held.
        unsafe {
            Thread::wait_mark_signaled(th, irq as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(&mut g, th);
            }
        }
    }
}

/// Consume one pending interrupt on `irq` — called by `sys_wait` when it returns
/// for an [`InterruptObject`], so a driver's wait→service→wait loop wakes once
/// per interrupt. Takes `SCHED` (the accessor contract). The caller's handle
/// pins `irq`.
///
/// # Safety
///
/// `irq` points at a live `InterruptObject`, pinned for the call by a reference the caller holds.
pub unsafe fn interrupt_consume(irq: *mut ()) {
    let _g = SCHED.lock();
    // SAFETY: live `InterruptObject` pinned by the caller's handle; `SCHED` held.
    unsafe { InterruptObject::consume(irq) };
}

/// `true` iff `irq` (an [`InterruptObject`]) has an unconsumed pending interrupt.
/// Takes `SCHED` (the accessor contract). Used by the boot self-test.
///
/// # Safety
///
/// `irq` points at a live `InterruptObject`, pinned for the call by a reference the caller holds.
pub unsafe fn interrupt_pending(irq: *mut ()) -> bool {
    let _g = SCHED.lock();
    // SAFETY: live `InterruptObject` pinned by the caller's reference; `SCHED` held.
    unsafe { InterruptObject::already_signaled(irq) }
}

/// `true` iff `po` (a [`PendingOperation`]) has completed. Takes `SCHED` (the
/// accessor contract); used by a boot self-test to poll for an async completion
/// without a thread to `sys_wait`.
///
/// # Safety
///
/// `po` points at a live `PendingOperation`, pinned for the call by a reference the caller holds.
pub unsafe fn pending_op_is_signaled(po: *mut ()) -> bool {
    let _g = SCHED.lock();
    // SAFETY: live `PendingOperation` pinned by the caller's reference; `SCHED` held.
    unsafe { PendingOperation::already_signaled(po) }
}
