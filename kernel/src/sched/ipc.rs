//! IPC delivery: sends, receives, and forwarding to a userspace server.

use super::*;
use crate::object::userspace_server::Outstanding;

/// Whether a send on `endpoint` carrying `transfers` could close a cycle of endpoints holding
/// each other through their rings, which nothing would free: it carries the endpoint it is for,
/// or one already holding an endpoint, as the last move of every such cycle must. A cycle
/// through a namespace binding is not caught: TODO(ipc-handle-cycles).
/// A lookup's PO, which can hold an endpoint, cannot move at all.
fn closes_a_cycle(tok: &SchedToken, endpoint: &IpcChannel, transfers: &[Option<TransferRef>]) -> bool {
    if transfers.iter().all(Option::is_none) {
        return false;
    }
    let peer = endpoint.peer_addr(tok);
    transfers.iter().flatten().any(|t| {
        core::ptr::eq(t.obj.as_ptr().cast::<IpcChannel>(), peer)
            || t.obj.downcast_ref::<IpcChannel>().is_some_and(|ep| ep.holds_an_endpoint(tok))
    })
}

/// Wake the receivers blocked on `endpoint`'s peer, whose ring a send just made non-empty.
pub(super) fn wake_peer(g: &mut SchedState, endpoint: &IpcChannel) {
    let peer = endpoint.peer(&mut g.tok).map(|(peer, tok)| (peer.take_waiters(tok), addr(peer)));
    if let Some((waiters, key)) = peer {
        wake_all(g, waiters, key);
    }
}

/// Send `msg` from `endpoint` into its peer's receive ring under `SCHED`, **moving** any
/// `transfers` into the queued slot and waking the peer's blocked receivers if the ring went
/// empty→non-empty. The message is already copied in from user memory. On
/// `Full`/`PeerClosed` the `transfers` stay with the caller, to be dropped **outside**
/// `SCHED`.
pub fn ipc_send_push(
    endpoint: &IpcChannel,
    msg: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
) -> SendOutcome {
    let mut g = SCHED.lock();
    if closes_a_cycle(&g.tok, endpoint, transfers) {
        return SendOutcome::Cycle;
    }
    let outcome = endpoint.send_push(&mut g.tok, msg, transfers);
    if let SendOutcome::Sent { woke_edge: true } = outcome {
        wake_peer(&mut g, endpoint);
    }
    outcome
}

/// Outcome of [`us_forward_originate`]: how a forwarded `sys_ns_lookup` fared.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ForwardOutcome {
    /// Delivered; the lookup's `PendingOperation` stays **pending** until the server replies.
    Pending,
    /// Every pending slot on this server is in flight: `WouldBlock`.
    Busy,
    /// The server's inbox ring is full — the caller fails the lookup (`WouldBlock`).
    Full,
    /// The server endpoint has closed — the caller fails the lookup (`PeerClosed`).
    PeerClosed,
}

/// Push a **fire-and-forget** message into a registration's server inbox: no pending slot,
/// no `request_id`, no reply. `true` if it was queued. Used for `File::Touch` after a Model
/// A writeback; nothing is reserved so nothing is rolled back, and a full ring or dead
/// server just leaves a stale `mtime` rather than failing `sys_file_sync`, whose data is
/// already on the device.
pub fn us_forward_notify(reg: &UserspaceServerReg, msg: &mut StoredMsg) -> bool {
    let mut g = SCHED.lock();
    let mut no_transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    match reg.endpoint().send_push(&mut g.tok, msg, &mut no_transfers) {
        SendOutcome::Sent { woke_edge } => {
            if woke_edge {
                wake_peer(&mut g, reg.endpoint());
            }
            true
        }
        SendOutcome::Full | SendOutcome::PeerClosed | SendOutcome::Cycle => false,
    }
}

/// Originate a forwarded namespace lookup: under one `SCHED` hold, reserve a free pending-lookup
/// slot of the registration, stamp its `request_id` into the built `Namespace::Resolve`
/// request `msg`, and push it into the server's inbox, waking its receivers. The lookup PO
/// stays **uncompleted**; the server's reply completes it inline (see
/// [`us_forward_take_pending`]). On `Busy` nothing was reserved; on `Full` / `PeerClosed`
/// the slot is rolled back and its `po` clone dropped after `SCHED`. The caller pins
/// `reg` and `po`.
pub fn us_forward_originate(
    reg: &UserspaceServerReg,
    msg: &mut StoredMsg,
    po: &Ref<PendingOperation>,
    requested: Rights,
    suffix: &[u8],
    fwd: crate::object::userspace_server::Forward<'_>,
) -> ForwardOutcome {
    let mut bag = Deferred::<1>::new();
    let mut g = bag.lock();
    // Reserve the pending slot and assign a request id (`None`: already busy).
    let Some(request_id) = reg.begin_forward(&mut g.tok, po, requested, suffix, fwd) else {
        return ForwardOutcome::Busy;
    };
    // Stamp the assigned id into the request envelope (built with a zero placeholder).
    crate::rsproto::stamp_request_id(&mut msg.payload, request_id);
    let mut no_transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    let outcome = reg.endpoint().send_push(&mut g.tok, msg, &mut no_transfers);
    match outcome {
        SendOutcome::Sent { woke_edge } => {
            if woke_edge {
                wake_peer(&mut g, reg.endpoint());
            }
            ForwardOutcome::Pending
        }
        SendOutcome::Full | SendOutcome::PeerClosed | SendOutcome::Cycle => {
            // Roll back the reserved slot.
            if let Some(pl) = reg.take_pending_matching(&mut g.tok, request_id) {
                g.defer(pl.po.erase());
            }
            if outcome == SendOutcome::Full { ForwardOutcome::Full } else { ForwardOutcome::PeerClosed }
        }
    }
}

/// Originate a forwarded page-cache **fill**: the page-fault sibling of
/// [`us_forward_originate`], reserving a free pending-fill slot of the registration and
/// pushing a `File::ReadRange` request. The fill PO stays **uncompleted**; the server's
/// reply completes it inline (see [`us_forward_take_pending_fill`]). On `Busy` nothing is
/// reserved; on `Full` / `PeerClosed` the slot is rolled back and its references dropped
/// after `SCHED`. The caller pins `reg`, `po` and `file_obj`.
pub fn us_forward_originate_fill(
    reg: &UserspaceServerReg,
    msg: &mut StoredMsg,
    po: &Ref<PendingOperation>,
    file_obj: &Ref<FileObject>,
    index: usize,
) -> ForwardOutcome {
    let mut bag = Deferred::<2>::new();
    let mut g = bag.lock();
    // Reserve the pending-fill slot and assign a request id (`None`: busy).
    let Some(request_id) = reg.begin_fill(&mut g.tok, po, file_obj, index) else {
        return ForwardOutcome::Busy;
    };
    crate::rsproto::stamp_request_id(&mut msg.payload, request_id);
    let mut no_transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    let outcome = reg.endpoint().send_push(&mut g.tok, msg, &mut no_transfers);
    match outcome {
        SendOutcome::Sent { woke_edge } => {
            if woke_edge {
                wake_peer(&mut g, reg.endpoint());
            }
            ForwardOutcome::Pending
        }
        SendOutcome::Full | SendOutcome::PeerClosed | SendOutcome::Cycle => {
            // Roll back the reserved slot.
            if let Some(pf) = reg.take_pending_fill_matching(&mut g.tok, request_id) {
                g.defer(pf.po.erase());
                g.defer(pf.file_obj.erase());
            }
            if outcome == SendOutcome::Full { ForwardOutcome::Full } else { ForwardOutcome::PeerClosed }
        }
    }
}

/// Take the forwarded **fill** outstanding on `reg` whose `request_id` matches a
/// `File::ReadRange` reply's, for the caller to land. `None` if it does not correlate
/// (duplicate / stale). The returned [`PendingFill`]'s references are dropped by the caller
/// **outside** `SCHED`. The caller pins `reg` (from [`us_forward_reg_for_send`]).
pub fn us_forward_take_pending_fill(reg: &UserspaceServerReg, request_id: u64) -> Option<PendingFill> {
    reg.take_pending_fill_matching(&mut SCHED.lock().tok, request_id)
}

/// If a send on `send_endpoint` targets the kernel's end of a Userspace Server channel (its
/// **peer** carries a [`UserspaceServerReg`](crate::object::UserspaceServerReg)
/// back-pointer), return a new reference to that registration: the send is a
/// forwarded-lookup *reply* the kernel completes inline. `None` for an ordinary send, a
/// closed peer, or a registration being destroyed. Acquired under `SCHED`: the back-pointer
/// is uncounted, and once `SCHED` is released another CPU may drop the last binding.
pub fn us_forward_reg_for_send(send_endpoint: &IpcChannel) -> Option<Ref<UserspaceServerReg>> {
    let mut g = SCHED.lock();
    let (peer, tok) = send_endpoint.peer(&mut g.tok)?;
    peer.us_reg_upgrade(tok)
}

/// A [`UserspaceServerReg`]'s last reference has gone: complete what it has outstanding
/// `PeerClosed`, so its waiters wake, and clear its endpoint's uncounted back-pointer to it, so
/// nothing follows it once it is freed. Its closing, run before its box is rebuilt, with its
/// endpoint still held. Takes `SCHED`, sound as references are released only outside it.
pub(crate) fn us_reg_detach(reg: &UserspaceServerReg, _: Closing) {
    let mut bag = Deferred::<ORPHANS_PER_REG>::new();
    let mut g = bag.lock();
    let outstanding = reg.take_outstanding(&mut g.tok);
    fail_outstanding(&mut g, outstanding);
    // Clear only a pointer that still names *this* registration: `us_server_attach` can
    // retarget an endpoint, and stomping a live successor's pointer breaks its routing.
    reg.endpoint().clear_us_reg_if(&mut g.tok, reg);
}

/// A registration's tables hold at most a lookup's PO, and a fill's PO and file, a slot.
const ORPHANS_PER_REG: usize = 3 * US_PENDING_MAX;

/// Fail everything a registration had outstanding `PeerClosed`, its fills too: a faulter waits on
/// one with no deadline, and its file may hold the registration, a cycle its drop outside `SCHED`
/// ends.
fn fail_outstanding<const N: usize>(g: &mut Held<'_, N>, outstanding: Outstanding) {
    let closed = crate::syscall::error::KError::PeerClosed as i32;
    for po in outstanding.lookups.into_iter().flatten() {
        signal_pending_op(g, &po, closed);
        g.defer(po.erase());
    }
    for (po, file_obj) in outstanding.fills.into_iter().flatten() {
        signal_pending_op(g, &po, closed);
        g.defer(po.erase());
        g.defer(file_obj.erase());
    }
}

/// Record the back-pointer from `reg`'s own endpoint to `reg` that makes the endpoint the kernel's
/// end of a Userspace Server channel, so a reply sent to it (by the server, on its peer) is
/// completed inline rather than enqueued, unless a live registration is there already: that one
/// comes back to share, from the same `SCHED` hold, so two binds cannot each make one. Only that
/// endpoint is attached, the one `us_reg_detach` clears when `reg` drops.
pub fn us_server_attach(reg: &Ref<UserspaceServerReg>) -> Option<Ref<UserspaceServerReg>> {
    let mut g = SCHED.lock();
    let endpoint = reg.endpoint();
    // A registration mid-drop has a zero count, refuses the upgrade, and is replaced.
    if let Some(shared) = endpoint.us_reg_upgrade(&g.tok) {
        return Some(shared);
    }
    // SAFETY: kept in the endpoint's cell, behind `SCHED`, and `reg`'s closing (`us_reg_detach`)
    // clears it there before the registration is freed.
    endpoint.set_us_reg(&mut g.tok, unsafe { Weak::new(reg.as_ptr()) });
    None
}

/// Take the forwarded lookup outstanding on `reg` whose `request_id` matches a reply's, for
/// the caller to complete. `None` if it does not correlate (duplicate / stale). The returned
/// [`PendingLookup`]'s `po` is dropped by the caller **outside** `SCHED`. The caller pins
/// `reg` (from [`us_forward_reg_for_send`]).
pub fn us_forward_take_pending(reg: &UserspaceServerReg, request_id: u64) -> Option<PendingLookup> {
    reg.take_pending_matching(&mut SCHED.lock().tok, request_id)
}

/// Blocking send (`Block` / `BlockBounded`): deliver `msg` into the peer's receive ring, or
/// — if it is full — **hold** it in the peer's pending-send queue with a reference to the
/// caller's `PendingOperation` `po`, delivered (completing `po`) when the peer next
/// receives. On immediate delivery `po` is **pre-completed** (status 0) so `sys_wait`
/// returns at once. On `PendingFull`/`PeerClosed` the `transfers` stay with the caller.
/// `deadline_ns` is the `BlockBounded` delivery deadline (`u64::MAX` for plain `Block`); on
/// `Queued` with a finite deadline a `PendingSend` heap entry is registered against the PO
/// so the tick can time the held message out (see `fire_expired_deadlines`).
pub fn ipc_send_push_blocking(
    endpoint: &IpcChannel,
    msg: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    po: &Ref<PendingOperation>,
    deadline_ns: u64,
) -> BlockSendOutcome {
    let mut g = SCHED.lock();
    if closes_a_cycle(&g.tok, endpoint, transfers) {
        return BlockSendOutcome::Cycle;
    }
    let outcome = endpoint.send_or_queue(&mut g.tok, msg, transfers, po);
    match outcome {
        BlockSendOutcome::Sent { woke_edge } => {
            if woke_edge {
                wake_peer(&mut g, endpoint);
            }
            // Delivered synchronously — complete the PO now (no waiters yet).
            signal_pending_op(&mut g, po, 0);
        }
        BlockSendOutcome::Queued if deadline_ns != u64::MAX => {
            // `BlockBounded`: register the delivery deadline against the PO; the held send
            // lives on the peer (receiving) endpoint, which a queued send proves is there.
            let channel = endpoint.peer_addr(&g.tok) as usize;
            // Into a slot the receiving endpoint holds, one for each send it can hold.
            let _ = deadline::push_promised(
                &mut g,
                deadline::Entry {
                    deadline_ns,
                    target: deadline::Target::PendingSend { po: addr(&**po) as usize, channel },
                },
            );
        }
        _ => {}
    }
    outcome
}

/// Inspect `endpoint`'s receive side under `SCHED` without dequeuing, so the empty-poll
/// `WouldBlock` path allocates no bounce buffer.
pub fn ipc_recv_peek(endpoint: &IpcChannel) -> RecvState {
    endpoint.recv_peek(&SCHED.lock().tok)
}

/// Pop the oldest message from `endpoint`'s inbox into `dst` under `SCHED`, **moving** its
/// transferred-handle references into `out`. The caller copies `dst` out and installs/drops
/// the transfers **after** this returns (an `ObjectRef` drop or `allocate` must not run
/// under `SCHED`).
///
/// Popping frees a ring slot, so on success this sweeps **timed-out** held sends into
/// `reclaimed` and promotes the oldest **live** held blocking sender into it, completing
/// its `PendingOperation` and dropping its pending-send deadline. Returns
/// `(popped, promoted_po)`; `popped` is `false` if the inbox drained between peek and pop.
/// The caller releases `promoted_po` and `reclaimed` **outside** `SCHED`.
pub fn ipc_recv_pop_into(
    endpoint: &IpcChannel,
    dst: &mut StoredMsg,
    out: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    reclaimed: &mut [Option<ReclaimedSend>; IpcChannel::MAX_PENDING_SENDS],
) -> (bool, Option<Ref<PendingOperation>>) {
    let mut g = SCHED.lock();
    if !endpoint.recv_pop_into(&mut g.tok, dst, out) {
        return (false, None);
    }
    // A slot just freed — sweep timed-out sends and promote the oldest live one.
    let promoted = endpoint.promote_pending_send(&mut g.tok, reclaimed);
    if let Some(ref po) = promoted {
        // Delivered before its deadline: drop its pending-send deadline (idempotent), then
        // complete it.
        let target = deadline::Target::PendingSend { po: addr(&**po) as usize, channel: addr(endpoint) as usize };
        deadline::remove(&mut g.deadlines, target);
        signal_pending_op(&mut g, po, 0);
    }
    (true, promoted)
}

/// An IPC endpoint's last reference has gone: under `SCHED`, clear the surviving peer's
/// back-pointer to it and wake the peer's blocked receivers (they observe `PeerClosed`), fail
/// forwarded requests a Userspace Server channel leaves, and fail the sends held on this
/// endpoint. Its closing, run before its box is rebuilt; the endpoint's docs give the
/// "references released only outside `SCHED`" invariant that makes taking the lock here sound.
pub(crate) fn ipc_endpoint_closing(endpoint: &IpcChannel, _: Closing) {
    let mut bag = Deferred::<{ ORPHANS_PER_REG + IpcChannel::MAX_PENDING_SENDS }>::new();
    let mut g = bag.lock();
    // A Userspace Server channel is losing its server, if the **peer** is the kernel end: fail the
    // forwarded requests pending on its registration `PeerClosed`. An endpoint is shared by many
    // bindings, so up to `US_PENDING_MAX` lookups may be in flight: drain *all*. This endpoint is
    // never a kernel end itself: a registration holds its own, and its closing detaches it first.
    let peer = endpoint.peer(&mut g.tok).map(|(peer, tok)| {
        peer.clear_peer(tok);
        let waiters = peer.take_waiters(tok);
        let theirs = peer.us_reg(tok).map(|(reg, tok)| reg.take_outstanding(tok));
        (waiters, addr(peer), theirs)
    });
    endpoint.clear_peer(&mut g.tok);
    if let Some((waiters, key, theirs)) = peer {
        wake_all(&mut g, waiters, key);
        if let Some(theirs) = theirs {
            fail_outstanding(&mut g, theirs);
        }
    }
    // Complete every blocking sender held on THIS endpoint with `PeerClosed`: our receive
    // ring is gone, so their messages can never be delivered. The sends stay queued, released
    // with the endpoint outside `SCHED`; the references here go with the bag.
    for po in endpoint.held_sends(&g.tok).into_iter().flatten() {
        // Drop each held send's pending-send deadline **before** the endpoint is freed, or a
        // live `BlockBounded` deadline could fire later and follow this dead `channel`.
        // Idempotent (a plain `Block` send, or a timed-out one, has no entry).
        let target = deadline::Target::PendingSend { po: addr(&*po) as usize, channel: addr(endpoint) as usize };
        deadline::remove(&mut g.deadlines, target);
        signal_pending_op(&mut g, &po, crate::syscall::error::KError::PeerClosed as i32);
        g.defer(po.erase());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::KBox;
    use crate::mm::test_support::init_global_heap;
    use crate::object::header::test_probe;

    fn pair() -> (Ref<IpcChannel>, Ref<IpcChannel>) {
        let (a, b) = IpcChannel::try_new_pair(4).unwrap();
        (a, b)
    }

    /// A pair whose first end is typed, for a registration to own.
    fn kernel_end() -> (Ref<IpcChannel>, Ref<IpcChannel>) {
        pair()
    }

    fn carrying(r: &Ref<IpcChannel>) -> [Option<TransferRef>; IPC_HANDLE_MAX] {
        core::array::from_fn(|i| (i == 0).then(|| TransferRef { obj: r.as_object().clone(), rights: Rights::empty() }))
    }

    /// The last move of any cycle of endpoints holding each other carries an endpoint already
    /// holding one, or the endpoint it is for.
    #[test]
    fn a_send_that_could_close_a_cycle_of_endpoints_is_refused() {
        init_global_heap();
        test_probe::reset();
        let msg = KBox::<StoredMsg>::try_new_zeroed().unwrap();
        let ((a, b), (c, d)) = (pair(), pair());
        let po = po();
        assert_eq!(ipc_send_push(&a, &msg, &mut carrying(&b)), SendOutcome::Cycle, "B into its own ring");
        assert!(matches!(ipc_send_push(&c, &msg, &mut carrying(&b)), SendOutcome::Sent { .. }));
        assert_eq!(ipc_send_push(&a, &msg, &mut carrying(&d)), SendOutcome::Cycle, "D, holding B");
        let blocking = ipc_send_push_blocking(&a, &msg, &mut carrying(&d), &po, u64::MAX);
        assert_eq!(blocking, BlockSendOutcome::Cycle);
        drop((a, b, c, d));
        assert_eq!(test_probe::ipc_channel_destroys(), 4, "an endpoint left holding another");
    }

    fn po() -> Ref<PendingOperation> {
        Ref::new(PendingOperation::try_new().unwrap())
    }

    /// A registration over the kernel's end of a fresh pair, attached; and the server's end.
    fn served() -> (Ref<UserspaceServerReg>, Ref<IpcChannel>) {
        let (k, s) = kernel_end();
        let reg = UserspaceServerReg::try_new(k.clone()).unwrap();
        assert!(us_server_attach(&reg).is_none());
        (reg, s)
    }

    /// A lookup and a fill outstanding on `reg`, their POs, and the fill's file.
    fn outstanding(
        reg: &Ref<UserspaceServerReg>,
        producer: crate::object::Producer,
    ) -> (Ref<PendingOperation>, Ref<PendingOperation>, Ref<FileObject>) {
        let (lookup, fill) = (po(), po());
        let file = Ref::new(FileObject::try_new(4096, producer).unwrap());
        let tok = &mut SchedToken::for_test();
        reg.begin(tok, &lookup, Rights::MAP_READ, b"f").unwrap();
        reg.begin_fill(tok, &fill, &file, 0).unwrap();
        (lookup, fill, file)
    }

    fn failed(po: &Ref<PendingOperation>) -> bool {
        pending_op_is_signaled(po) && pending_op_completion(po).0 == crate::syscall::error::KError::PeerClosed as i32
    }

    /// **A server's death fails its fills as well as its lookups**: a thread faulting a page waits
    /// on its fill with no deadline, and the fill holds its file, which holds the registration.
    #[test]
    fn a_dying_server_fails_its_fills_and_frees_their_cycle() {
        init_global_heap();
        let (reg, server_end) = served();
        let fs = crate::object::Producer::FsServer { reg: reg.clone(), suffix: crate::libkern::KString::new() };
        let (lookup, fill, file) = outstanding(&reg, fs);
        test_probe::reset();
        // The fill's reference the last, so its drop, outside `SCHED`, destroys the file.
        drop(file);
        drop(server_end);
        assert!(failed(&lookup) && failed(&fill), "the fill's faulter would wait for ever");
        assert_eq!(test_probe::file_object_destroys(), 1, "the file, held by the fill alone");
        drop((reg, lookup, fill));
        assert_eq!(test_probe::userspace_server_reg_destroys(), 1, "the registration, held by the file");
    }

    /// **Two binds of one endpoint share one registration**, which every reply routes to.
    #[test]
    fn a_second_attach_of_an_endpoint_gets_the_first_registration() {
        init_global_heap();
        let (k, _server_end) = kernel_end();
        let new_reg = || UserspaceServerReg::try_new(k.clone()).unwrap();
        let (first, second) = (new_reg(), new_reg());
        assert!(us_server_attach(&first).is_none());
        let shared = us_server_attach(&second).expect("the first registration");
        assert_eq!(shared.as_ptr(), first.as_ptr());
        let routed = k.us_reg_addr(&SchedToken::for_test());
        assert_eq!(routed, first.as_ptr().cast_const(), "replies route to it");
    }

    #[test]
    fn a_registration_dropped_fails_its_fills() {
        init_global_heap();
        let (reg, _server_end) = served();
        let (lookup, fill, _file) = outstanding(&reg, crate::object::Producer::Stub { base: 0 });
        drop(reg);
        assert!(failed(&lookup) && failed(&fill));
    }

    #[test]
    fn an_endpoint_holding_another_in_a_held_send_is_refused_too() {
        init_global_heap();
        let msg = KBox::<StoredMsg>::try_new_zeroed().unwrap();
        let ((a, b), (c, d)) = (pair(), pair());
        let po = po();
        let mut none: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
        while matches!(ipc_send_push(&c, &msg, &mut none), SendOutcome::Sent { .. }) {}
        let held = ipc_send_push_blocking(&c, &msg, &mut carrying(&b), &po, u64::MAX);
        assert_eq!(held, BlockSendOutcome::Queued, "D's ring is full: B waits in its held sends");
        assert_eq!(ipc_send_push(&a, &msg, &mut carrying(&d)), SendOutcome::Cycle);
    }

    /// **A bounded send's deadline goes with its send**: one a receive delivers, and one its
    /// endpoint's close fails, each leaves no entry to fire on the endpoint it names.
    #[test]
    fn a_held_sends_deadline_goes_when_it_is_delivered_or_its_endpoint_closes() {
        init_global_heap();
        let msg = KBox::<StoredMsg>::try_new_zeroed().unwrap();
        let none = || -> [Option<TransferRef>; IPC_HANDLE_MAX] { core::array::from_fn(|_| None) };
        let named = |po: &Ref<PendingOperation>| {
            let po = po.as_object().as_ptr() as usize;
            SCHED
                .lock()
                .deadlines
                .iter()
                .any(|e| matches!(e.target, deadline::Target::PendingSend { po: p, .. } if p == po))
        };
        for closes in [false, true] {
            let ((a, b), held) = (pair(), po());
            while matches!(ipc_send_push(&a, &msg, &mut none()), SendOutcome::Sent { .. }) {}
            let queued = ipc_send_push_blocking(&a, &msg, &mut none(), &held, u64::MAX - 1);
            assert_eq!(queued, BlockSendOutcome::Queued);
            assert!(named(&held), "a bounded send waits with its deadline");
            if closes {
                drop(b);
                assert!(!named(&held), "failed by its endpoint's close");
                assert!(failed(&held), "its sender told so");
            } else {
                let mut dst = KBox::<StoredMsg>::try_new_zeroed().unwrap();
                let mut reclaimed = core::array::from_fn(|_| None);
                let (popped, promoted) = ipc_recv_pop_into(&b, &mut dst, &mut none(), &mut reclaimed);
                assert!(popped);
                assert_eq!(promoted.as_ref().map(Ref::as_ptr), Some(held.as_ptr()), "delivered");
                assert!(!named(&held), "delivered by a receive");
                drop((promoted, b));
            }
            drop(a);
        }
    }

    /// **A forward its server cannot take, its ring full or the server gone, gives its slot back**,
    /// letting go of what the slot held once `SCHED` is released.
    #[test]
    fn a_forward_its_server_cannot_take_lets_go_of_its_slot() {
        use crate::object::userspace_server::Forward;
        init_global_heap();
        for gone in [false, true] {
            let (reg, server_end) = served();
            let mut msg = KBox::<StoredMsg>::try_new_zeroed().unwrap();
            let server_end = if gone {
                drop(server_end);
                None
            } else {
                while us_forward_notify(&reg, &mut msg) {}
                Some(server_end)
            };
            let (lookup, fill) = (po(), po());
            let file = Ref::new(FileObject::try_new(4096, crate::object::Producer::Stub { base: 0 }).unwrap());
            let refused = if gone { ForwardOutcome::PeerClosed } else { ForwardOutcome::Full };
            let originated = us_forward_originate(&reg, &mut msg, &lookup, Rights::MAP_READ, b"f", Forward::PLAIN);
            assert_eq!(originated, refused);
            assert_eq!(us_forward_originate_fill(&reg, &mut msg, &fill, &file, 0), refused);
            test_probe::reset();
            drop((lookup, fill, file));
            assert_eq!(test_probe::pending_op_destroys(), 2, "gone={gone}: no slot keeps a PO");
            assert_eq!(test_probe::file_object_destroys(), 1, "gone={gone}: nor the fill's file");
            drop((reg, server_end));
        }
    }
}
