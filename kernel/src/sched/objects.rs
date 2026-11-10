//! Waitable objects: waiters added and removed, and objects signalled.

use super::*;

// --- Waitable dispatch ---
//
// `wait_on` dispatches the waitable operations on each object's address by the kobject type
// read from the `KObjectHeader` at offset 0 (every kobject is `#[repr(C)]` with the header
// first). A kind no thread waits on falls to each dispatch's last arm.

/// The kobject type at the head of a type-erased object pointer.
///
/// # Safety
///
/// `obj` is a live kobject (header at offset 0); `SCHED` held.
unsafe fn obj_type(obj: *mut ()) -> KObjectType {
    // SAFETY: every kobject begins with a `KObjectHeader` (see object::header).
    unsafe { (*(obj as *const crate::object::header::KObjectHeader)).object_type() }
}

/// Is this waitable currently signaled?
///
/// # Safety
///
/// A live kobject of any kind, `SCHED` held.
pub(super) unsafe fn obj_already_signaled(obj: *mut (), now: u64) -> bool {
    // SAFETY: this function's contract: `obj` is a live kobject, of the type its header names, and
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
/// A live kobject of any kind, `SCHED` held.
pub(super) unsafe fn obj_add_waiter(obj: *mut (), th: *mut ()) -> Result<(), ()> {
    // SAFETY: this function's contract: `obj` is a live kobject, of the type its header names, and
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
/// A live kobject of any kind, `SCHED` held.
pub(super) unsafe fn obj_remove_waiter(obj: *mut (), th: *mut ()) {
    // SAFETY: this function's contract: `obj` is a live kobject, of the type its header names, and
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

/// Ask `process` to exit: enqueue [`KIND_TERMINATE_REQUESTED`] on **its own** notification
/// channel and wake anything blocked there.
///
/// **The kernel does nothing else.** It does not stop threads or touch the target's
/// execution, so there is no safe-point question and no second teardown path beside
/// [`exit_process`]. A process that listens exits itself; one that does not keeps running
/// ("there is no forcible kill in this system"). `false` when the target has no notification
/// channel: it can never be asked, which the caller reports.
pub fn request_terminate(process: &Process) -> bool {
    let mut g = SCHED.lock();
    let Some(c) = process.notification_channel().map(addr) else { return false };
    let notif = Notification::terminate_requested();
    // SAFETY: `c` is the channel the borrowed `Process` holds, so it outlives this call; `SCHED`
    // held; the queue is pre-reserved, so no allocation happens here.
    let _edge = unsafe { NotificationChannel::enqueue(c, notif) };
    signal_channel(&mut g, c);
    true
}

/// Wake every thread in `waiters`, just taken from `target`'s queue: mark `target` signalled in
/// each one's wait, and make runnable each that this wake is the first to claim. Whether any was.
/// Caller holds `SCHED`. No allocation, no blocking — safe from the fault path and the tick.
pub(super) fn wake_all<const N: usize>(g: &mut SchedState, waiters: Waiters<N>, target: *mut ()) -> bool {
    let mut woke = false;
    for &th in waiters.iter() {
        // SAFETY: only a `WaitQueue` makes `Waiters`, and only `wait_on` adds to one: the
        // current thread, parked in `blocked` until it removes itself under `SCHED`, which the
        // caller holds; so each is a live, pinned thread.
        unsafe {
            Thread::wait_mark_signaled(th, target as usize);
            if Thread::wait_try_wake(th) {
                make_runnable(g, th);
                woke = true;
            }
        }
    }
    woke
}

/// Wake every thread blocked on `channel` (its queue just went non-empty). Caller holds
/// `SCHED`. No allocation, no blocking — safe from the fault path.
pub(super) fn signal_channel(g: &mut SchedState, channel: *mut ()) -> bool {
    // SAFETY: live channel, `SCHED` held — drains its waiter list.
    let waiters = unsafe { NotificationChannel::take_waiters(channel) };
    wake_all(g, waiters, channel)
}

/// Wake every thread blocked recv-ing on `endpoint` (a message just arrived in its inbox,
/// or its peer just closed). Caller holds `SCHED`. No allocation, no blocking.
pub(super) fn signal_ipc_endpoint(g: &mut SchedState, endpoint: *mut ()) {
    // SAFETY: live endpoint, `SCHED` held — drains its recv-waiter list.
    let waiters = unsafe { IpcChannel::take_waiters(endpoint) };
    wake_all(g, waiters, endpoint);
}

/// Complete a [`PendingOperation`] with `status` and wake every thread blocked on it.
/// One-shot (the first completion wins), so it is safe from both the delivery path and a
/// timeout. Caller holds `SCHED`. No allocation, no blocking, no `ObjectRef` drop.
pub(super) fn signal_pending_op(g: &mut SchedState, po: *mut (), status: i32) {
    signal_pending_op_with_result(g, po, status, 0);
}

/// As [`signal_pending_op`], but the completion also carries a `result` (a namespace
/// lookup's resolved handle). Same constraints.
pub(super) fn signal_pending_op_with_result(g: &mut SchedState, po: *mut (), status: i32, result: u64) {
    // SAFETY: live PO, `SCHED` held. One-shot; a re-signal returns `false`.
    if !unsafe { PendingOperation::signal_with_result(po, status, result) } {
        return; // already completed — its waiters were handled the first time
    }
    // SAFETY: live PO, `SCHED` held — drains its waiter list.
    let waiters = unsafe { PendingOperation::take_waiters(po) };
    wake_all(g, waiters, po);
}

/// Read a completed [`PendingOperation`]'s `(status, result)` under one `SCHED` hold, for
/// `sys_wait` building the `IoResult`.
pub fn pending_op_completion(po: &PendingOperation) -> (i32, u64) {
    let _g = SCHED.lock();
    let po = addr(po);
    // SAFETY: `po` is pinned by the borrow; `SCHED` held — both reads see the same
    // one-shot-stable completion.
    unsafe { (PendingOperation::status(po), PendingOperation::result(po)) }
}

/// Complete a lookup's [`PendingOperation`] with `status` and its object, **parked** for
/// [`pending_op_take`]'s reader; one a PO already completed comes back, to drop outside `SCHED`.
#[must_use]
pub fn complete_lookup(
    po: &PendingOperation,
    status: i32,
    parked: Option<(ObjectRef, Rights)>,
) -> Option<(ObjectRef, Rights)> {
    let mut g = SCHED.lock();
    let po = addr(po);
    // SAFETY: `po` is pinned by the borrow; `SCHED` held.
    if unsafe { PendingOperation::already_signaled(po) } {
        return parked;
    }
    // SAFETY: as above.
    unsafe { PendingOperation::park(po, parked) };
    signal_pending_op_with_result(&mut g, po, status, 0);
    None
}

/// What [`pending_op_take`] found in a completed [`PendingOperation`].
pub enum Completion {
    /// Its `(status, result)`.
    Read(i32, u64),
    /// A lookup's object, for the reader to install and end with [`pending_op_installed`].
    Install(ObjectRef, Rights),
    /// Another reader is installing it; read again.
    Installing,
}

/// Read a completed [`PendingOperation`] for `sys_wait`, under one `SCHED` hold.
pub fn pending_op_take(po: &PendingOperation) -> Completion {
    let _g = SCHED.lock();
    let po = addr(po);
    // SAFETY: `po` is pinned by the borrow; `SCHED` held.
    unsafe {
        match PendingOperation::take_parked(po) {
            Err(()) => Completion::Installing,
            Ok(Some((obj, rights))) => Completion::Install(obj, rights),
            Ok(None) => Completion::Read(PendingOperation::status(po), PendingOperation::result(po)),
        }
    }
}

/// End a [`Completion::Install`]: the handle made, or the error the lookup now fails with.
pub fn pending_op_installed(po: &PendingOperation, outcome: Result<u64, i32>) {
    let _g = SCHED.lock();
    // SAFETY: `po` is pinned by the borrow; `SCHED` held.
    unsafe { PendingOperation::installed(addr(po), outcome) };
}

/// Complete a [`PendingOperation`] from **syscall context** (not a wakeup path), taking
/// `SCHED`. A namespace direct-handle lookup **pre-signals** the PO it returns: resolution
/// finished synchronously, the PO has no waiters yet, and the caller's later `sys_wait`
/// takes the already-signalled fast path. One-shot.
pub fn complete_pending_op(po: &PendingOperation, status: i32, result: u64) {
    let mut g = SCHED.lock();
    signal_pending_op_with_result(&mut g, addr(po), status, result);
}

/// [`complete_pending_op`] for a completer holding only the address of a `PendingOperation` its
/// waiter pins. The waiter may let go once it sees the completion, which can be before this
/// returns, and a borrow would be held for the whole call, past the release of `SCHED`.
///
/// # Safety
///
/// `po` is a live `PendingOperation`, pinned by a reference its waiter lets go of only once it sees
/// a completion.
pub unsafe fn complete_pending_op_at(po: *mut (), status: i32, result: u64) {
    let mut g = SCHED.lock();
    signal_pending_op_with_result(&mut g, po, status, result);
}

/// Record an interrupt on `irq` and wake every thread blocked on it. Latching: with no
/// current waiter the pending count increments and the next waiter gets it. Takes `SCHED`,
/// so it is callable from a DPC handler (the path a device ISR's completion takes). No
/// allocation, no blocking, no `ObjectRef` drop.
pub fn signal_interrupt(irq: &InterruptObject) {
    signal_interrupt_in(&mut SCHED.lock(), addr(irq));
}

/// [`signal_interrupt`] for a caller holding `SCHED`, `irq` a live `InterruptObject`.
pub(super) fn signal_interrupt_in(g: &mut SchedState, irq: *mut ()) {
    // SAFETY: live `InterruptObject`, `SCHED` held — latches a pending interrupt.
    unsafe { InterruptObject::signal(irq) };
    // SAFETY: live object, `SCHED` held — drains its waiter list.
    let waiters = unsafe { InterruptObject::take_waiters(irq) };
    wake_all(g, waiters, irq);
}

/// Consume one pending interrupt on `irq` — called by `sys_wait` on returning for an
/// [`InterruptObject`], so a driver's wait→service→wait loop wakes once per interrupt.
pub fn interrupt_consume(irq: &InterruptObject) {
    let _g = SCHED.lock();
    // SAFETY: pinned by the borrow; `SCHED` held.
    unsafe { InterruptObject::consume(addr(irq)) };
}

/// `true` iff `irq` has an unconsumed pending interrupt. Used by the boot self-test.
pub fn interrupt_pending(irq: &InterruptObject) -> bool {
    let _g = SCHED.lock();
    // SAFETY: pinned by the borrow; `SCHED` held.
    unsafe { InterruptObject::already_signaled(addr(irq)) }
}

/// `true` iff `po` has completed; lets a boot self-test poll without a thread to `sys_wait`.
pub fn pending_op_is_signaled(po: &PendingOperation) -> bool {
    let _g = SCHED.lock();
    // SAFETY: pinned by the borrow; `SCHED` held.
    unsafe { PendingOperation::already_signaled(addr(po)) }
}
