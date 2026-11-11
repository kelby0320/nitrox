//! Waitable objects: waiters added and removed, and objects signalled.

use super::*;

/// One of the kinds a thread can wait on, borrowed from the reference that pins it.
enum Waitable<'a> {
    Timer(&'a Timer),
    Channel(&'a NotificationChannel),
    Endpoint(&'a IpcChannel),
    Op(&'a PendingOperation),
    Interrupt(&'a InterruptObject),
}

/// `obj` as a kind a thread can wait on, or `None` for a kind no thread waits on.
fn waitable(obj: &ObjectRef) -> Option<Waitable<'_>> {
    Some(match obj.object_type() {
        KObjectType::Timer => Waitable::Timer(obj.downcast_ref()?),
        KObjectType::NotificationChannel => Waitable::Channel(obj.downcast_ref()?),
        KObjectType::IpcChannel => Waitable::Endpoint(obj.downcast_ref()?),
        KObjectType::PendingOperation => Waitable::Op(obj.downcast_ref()?),
        KObjectType::InterruptObject => Waitable::Interrupt(obj.downcast_ref()?),
        _ => return None,
    })
}

/// Is this waitable currently signaled? A kind no thread waits on never is.
pub(super) fn obj_already_signaled(tok: &SchedToken, obj: &ObjectRef, now: u64) -> bool {
    match waitable(obj) {
        Some(Waitable::Timer(t)) => t.already_signaled(tok, now),
        Some(Waitable::Channel(c)) => c.already_signaled(tok),
        Some(Waitable::Endpoint(e)) => e.already_signaled(tok),
        Some(Waitable::Op(p)) => p.already_signaled(tok),
        Some(Waitable::Interrupt(i)) => i.already_signaled(tok),
        None => false,
    }
}

/// Register the current thread as a waiter. `Err` over the object's reserve, or for a kind no
/// thread waits on.
pub(super) fn obj_add_waiter(tok: &mut SchedToken, obj: &ObjectRef, th: *mut ()) -> Result<(), ()> {
    match waitable(obj) {
        Some(Waitable::Timer(t)) => t.add_waiter(tok, th),
        Some(Waitable::Channel(c)) => c.add_waiter(tok, th),
        Some(Waitable::Endpoint(e)) => e.add_waiter(tok, th),
        Some(Waitable::Op(p)) => p.add_waiter(tok, th),
        Some(Waitable::Interrupt(i)) => i.add_waiter(tok, th),
        None => Err(()),
    }
}

/// Unregister a waiter (idempotent).
pub(super) fn obj_remove_waiter(tok: &mut SchedToken, obj: &ObjectRef, th: *mut ()) {
    match waitable(obj) {
        Some(Waitable::Timer(t)) => t.remove_waiter(tok, th),
        Some(Waitable::Channel(c)) => c.remove_waiter(tok, th),
        Some(Waitable::Endpoint(e)) => e.remove_waiter(tok, th),
        Some(Waitable::Op(p)) => p.remove_waiter(tok, th),
        Some(Waitable::Interrupt(i)) => i.remove_waiter(tok, th),
        None => {}
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
    let Some(c) = process.notification_channel() else { return false };
    // The queue is pre-reserved, so no allocation happens here.
    let _edge = c.enqueue(&mut g.tok, Notification::terminate_requested());
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
        // current thread, which a scheduler list pins until it removes itself under `SCHED`,
        // which the caller holds; so each is a live, pinned thread.
        let th = unsafe { &*th.cast::<Thread>() };
        th.wait_mark_signaled(&mut g.tok, target as usize);
        if th.wait_try_wake() {
            make_runnable(g, th);
            woke = true;
        }
    }
    woke
}

/// Wake every thread blocked on `channel` (its queue just went non-empty). Caller holds
/// `SCHED`. No allocation, no blocking — safe from the fault path.
pub(super) fn signal_channel(g: &mut SchedState, channel: &NotificationChannel) -> bool {
    let waiters = channel.take_waiters(&mut g.tok);
    wake_all(g, waiters, addr(channel))
}

/// Complete a [`PendingOperation`] with `status` and wake every thread blocked on it.
/// One-shot (the first completion wins), so it is safe from both the delivery path and a
/// timeout. Caller holds `SCHED`. No allocation, no blocking, no `ObjectRef` drop.
pub(super) fn signal_pending_op(g: &mut SchedState, po: &PendingOperation, status: i32) {
    signal_pending_op_with_result(g, po, status, 0);
}

/// As [`signal_pending_op`], but the completion also carries a `result` (a namespace
/// lookup's resolved handle). Same constraints.
pub(super) fn signal_pending_op_with_result(g: &mut SchedState, po: &PendingOperation, status: i32, result: u64) {
    if !po.signal_with_result(&mut g.tok, status, result) {
        return; // already completed — its waiters were handled the first time
    }
    let waiters = po.take_waiters(&mut g.tok);
    wake_all(g, waiters, addr(po));
}

/// Read a completed [`PendingOperation`]'s `(status, result)` under one `SCHED` hold, for
/// `sys_wait` building the `IoResult`.
pub fn pending_op_completion(po: &PendingOperation) -> (i32, u64) {
    let g = SCHED.lock();
    (po.status(&g.tok), po.result(&g.tok))
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
    if po.already_signaled(&g.tok) {
        return parked;
    }
    po.park(&mut g.tok, parked);
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
    let mut g = SCHED.lock();
    match po.take_parked(&mut g.tok) {
        Err(()) => Completion::Installing,
        Ok(Some((obj, rights))) => Completion::Install(obj, rights),
        Ok(None) => Completion::Read(po.status(&g.tok), po.result(&g.tok)),
    }
}

/// End a [`Completion::Install`]: the handle made, or the error the lookup now fails with.
pub fn pending_op_installed(po: &PendingOperation, outcome: Result<u64, i32>) {
    po.installed(&mut SCHED.lock().tok, outcome);
}

/// Complete a [`PendingOperation`] from **syscall context** (not a wakeup path), taking
/// `SCHED`. A namespace direct-handle lookup **pre-signals** the PO it returns: resolution
/// finished synchronously, the PO has no waiters yet, and the caller's later `sys_wait`
/// takes the already-signalled fast path. One-shot.
pub fn complete_pending_op(po: &PendingOperation, status: i32, result: u64) {
    signal_pending_op_with_result(&mut SCHED.lock(), po, status, result);
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
    // SAFETY: the contract keeps the PO whole until it is signalled, here, under `SCHED`; this
    // borrow ends with the call below, before the guard releases the lock.
    let po = unsafe { &*po.cast::<PendingOperation>() };
    signal_pending_op_with_result(&mut g, po, status, result);
}

/// Record an interrupt on `irq` and wake every thread blocked on it. Latching: with no
/// current waiter the pending count increments and the next waiter gets it. Takes `SCHED`,
/// so it is callable from a DPC handler (the path a device ISR's completion takes). No
/// allocation, no blocking, no `ObjectRef` drop.
pub fn signal_interrupt(irq: &InterruptObject) {
    signal_interrupt_in(&mut SCHED.lock(), irq);
}

/// [`signal_interrupt`] for a caller holding `SCHED`.
pub(super) fn signal_interrupt_in(g: &mut SchedState, irq: &InterruptObject) {
    irq.signal(&mut g.tok);
    let waiters = irq.take_waiters(&mut g.tok);
    wake_all(g, waiters, addr(irq));
}

/// Consume one pending interrupt on `irq` — called by `sys_wait` on returning for an
/// [`InterruptObject`], so a driver's wait→service→wait loop wakes once per interrupt.
pub fn interrupt_consume(irq: &InterruptObject) {
    irq.consume(&mut SCHED.lock().tok);
}

/// `true` iff `irq` has an unconsumed pending interrupt. Used by the boot self-test.
pub fn interrupt_pending(irq: &InterruptObject) -> bool {
    irq.already_signaled(&SCHED.lock().tok)
}

/// `true` iff `po` has completed; lets a boot self-test poll without a thread to `sys_wait`.
pub fn pending_op_is_signaled(po: &PendingOperation) -> bool {
    po.already_signaled(&SCHED.lock().tok)
}
