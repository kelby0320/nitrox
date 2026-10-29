//! IPC delivery: sends, receives, and forwarding to a userspace server.

use super::*;

/// Whether a send on `endpoint` carrying `transfers` could close a cycle of endpoints holding
/// each other through their rings, which nothing would free: it carries the endpoint it is for,
/// or one already holding an endpoint, as the last move of every such cycle must. A cycle
/// through a namespace binding is not caught: TODO(ipc-handle-cycles).
/// A lookup's PO, which can hold an endpoint, cannot move at all.
///
/// # Safety
///
/// `endpoint` is a live `IpcChannel` the caller pins; the caller holds `SCHED`.
unsafe fn closes_a_cycle(endpoint: *mut (), transfers: &[Option<TransferRef>]) -> bool {
    if transfers.iter().all(Option::is_none) {
        return false;
    }
    // SAFETY: the caller's contract. A peer only ever turns null.
    let peer = unsafe { IpcChannel::peer_of(endpoint) };
    transfers.iter().flatten().any(|t| {
        let ep = t.obj.as_ptr();
        // SAFETY: an `IpcChannel` the transfer pins; `SCHED` held.
        ep == peer || (t.obj.object_type() == KObjectType::IpcChannel && unsafe { IpcChannel::holds_an_endpoint(ep) })
    })
}

/// Send `msg` from `endpoint` into its peer's receive ring under `SCHED`, **moving** any
/// `transfers` into the queued slot and waking the peer's blocked receivers if the ring went
/// empty→non-empty. The message is already copied in from user memory. On
/// `Full`/`PeerClosed` the `transfers` stay with the caller, to be dropped **outside**
/// `SCHED`.
///
/// # Safety
///
/// `endpoint` points at a live `IpcChannel`, pinned for the call by a reference the caller holds.
pub unsafe fn ipc_send_push(
    endpoint: *mut (),
    msg: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
) -> SendOutcome {
    let mut g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning `endpoint`; `SCHED` held.
    if unsafe { closes_a_cycle(endpoint, transfers) } {
        return SendOutcome::Cycle;
    }
    // SAFETY: as above.
    let outcome = unsafe { IpcChannel::send_push(endpoint, msg, transfers) };
    if let SendOutcome::Sent { woke_edge: true } = outcome {
        // SAFETY: a successful send proves the peer is non-null; `SCHED` held.
        let peer = unsafe { IpcChannel::peer_of(endpoint) };
        if !peer.is_null() {
            signal_ipc_endpoint(&mut g, peer);
        }
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
///
/// # Safety
///
/// `reg` points at a live `UserspaceServerReg`, pinned for the call by a reference the caller
/// holds.
pub unsafe fn us_forward_notify(reg: *mut (), msg: &mut StoredMsg) -> bool {
    let mut g = SCHED.lock();
    // SAFETY: `reg` is a live `UserspaceServerReg` pinned by the caller; `SCHED` held, so
    // the registration's kernel endpoint is pinned with it.
    let endpoint = unsafe { UserspaceServerReg::endpoint_ptr(reg) };
    let mut no_transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    // SAFETY: `endpoint` is the registration's live kernel endpoint; `SCHED` held.
    match unsafe { IpcChannel::send_push(endpoint, msg, &mut no_transfers) } {
        SendOutcome::Sent { woke_edge } => {
            if woke_edge {
                // SAFETY: a successful send proves the peer is non-null; `SCHED` held.
                let peer = unsafe { IpcChannel::peer_of(endpoint) };
                if !peer.is_null() {
                    signal_ipc_endpoint(&mut g, peer);
                }
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
/// the slot is rolled back and its `po` clone dropped outside `SCHED`. The caller pins
/// `reg` and `po`.
///
/// # Safety
///
/// `reg` points at a live `UserspaceServerReg`, pinned for the call by a reference the caller
/// holds.
pub unsafe fn us_forward_originate(
    reg: *mut (),
    msg: &mut StoredMsg,
    po: &ObjectRef,
    requested: Rights,
    suffix: &[u8],
    fwd: crate::object::userspace_server::Forward<'_>,
) -> ForwardOutcome {
    let mut g = SCHED.lock();
    // SAFETY: `reg` is a live `UserspaceServerReg` pinned by the caller; `SCHED`
    // held. Reserve the pending slot + assign a request id (None ⇒ already busy).
    let request_id = match unsafe { UserspaceServerReg::begin_forward(reg, po, requested, suffix, fwd) } {
        Some(id) => id,
        None => return ForwardOutcome::Busy,
    };
    // Stamp the assigned id into the request envelope (built with a zero placeholder).
    crate::rsproto::stamp_request_id(&mut msg.payload, request_id);
    // SAFETY: `reg` live, `SCHED` held — the kernel endpoint is pinned by `reg`.
    let endpoint = unsafe { UserspaceServerReg::endpoint_ptr(reg) };
    let mut no_transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    // SAFETY: `endpoint` is the registration's live kernel endpoint; `SCHED` held.
    let outcome = unsafe { IpcChannel::send_push(endpoint, msg, &mut no_transfers) };
    match outcome {
        SendOutcome::Sent { woke_edge } => {
            if woke_edge {
                // SAFETY: a successful send proves the peer is non-null; `SCHED` held.
                let peer = unsafe { IpcChannel::peer_of(endpoint) };
                if !peer.is_null() {
                    signal_ipc_endpoint(&mut g, peer);
                }
            }
            ForwardOutcome::Pending
        }
        SendOutcome::Full | SendOutcome::PeerClosed | SendOutcome::Cycle => {
            // Roll back the reserved slot; drop its `po` clone **outside** `SCHED`.
            // SAFETY: `reg` live, `SCHED` held.
            let pl = unsafe { UserspaceServerReg::take_pending_matching(reg, request_id) };
            drop(g);
            drop(pl);
            if outcome == SendOutcome::Full { ForwardOutcome::Full } else { ForwardOutcome::PeerClosed }
        }
    }
}

/// Originate a forwarded page-cache **fill**: the page-fault sibling of
/// [`us_forward_originate`], reserving a free pending-fill slot of the registration and
/// pushing a `File::ReadRange` request. The fill PO stays **uncompleted**; the server's
/// reply completes it inline (see [`us_forward_take_pending_fill`]). On `Busy` nothing is
/// reserved; on `Full` / `PeerClosed` the slot is rolled back and its references dropped
/// outside `SCHED`. The caller pins `reg`, `po` and `file_obj`.
///
/// # Safety
///
/// `reg` points at a live `UserspaceServerReg`, pinned for the call by a reference the caller
/// holds.
pub unsafe fn us_forward_originate_fill(
    reg: *mut (),
    msg: &mut StoredMsg,
    po: &ObjectRef,
    file_obj: &ObjectRef,
    frame: crate::mm::PhysAddr,
    index: usize,
) -> ForwardOutcome {
    let mut g = SCHED.lock();
    // SAFETY: `reg` is a live `UserspaceServerReg` pinned by the caller; `SCHED`
    // held. Reserve the pending-fill slot + assign a request id (None ⇒ busy).
    let request_id = match unsafe { UserspaceServerReg::begin_fill(reg, po, file_obj, frame, index) } {
        Some(id) => id,
        None => return ForwardOutcome::Busy,
    };
    crate::rsproto::stamp_request_id(&mut msg.payload, request_id);
    // SAFETY: `reg` live, `SCHED` held — the kernel endpoint is pinned by `reg`.
    let endpoint = unsafe { UserspaceServerReg::endpoint_ptr(reg) };
    let mut no_transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    // SAFETY: `endpoint` is the registration's live kernel endpoint; `SCHED` held.
    let outcome = unsafe { IpcChannel::send_push(endpoint, msg, &mut no_transfers) };
    match outcome {
        SendOutcome::Sent { woke_edge } => {
            if woke_edge {
                // SAFETY: a successful send proves the peer is non-null; `SCHED` held.
                let peer = unsafe { IpcChannel::peer_of(endpoint) };
                if !peer.is_null() {
                    signal_ipc_endpoint(&mut g, peer);
                }
            }
            ForwardOutcome::Pending
        }
        SendOutcome::Full | SendOutcome::PeerClosed | SendOutcome::Cycle => {
            // Roll back the reserved slot; drop the fill's clones **outside** `SCHED`.
            // SAFETY: `reg` live, `SCHED` held.
            let pf = unsafe { UserspaceServerReg::take_pending_fill_matching(reg, request_id) };
            drop(g);
            drop(pf);
            if outcome == SendOutcome::Full { ForwardOutcome::Full } else { ForwardOutcome::PeerClosed }
        }
    }
}

/// Take the forwarded **fill** outstanding on `reg` whose `request_id` matches a
/// `File::ReadRange` reply's, for the caller to land. `None` if it does not correlate
/// (duplicate / stale). The returned [`PendingFill`]'s references are dropped by the caller
/// **outside** `SCHED`. The caller pins `reg` (from [`us_forward_reg_for_send`]).
///
/// # Safety
///
/// `reg` points at a live `UserspaceServerReg`, pinned for the call by a reference the caller
/// holds.
pub unsafe fn us_forward_take_pending_fill(reg: *mut (), request_id: u64) -> Option<PendingFill> {
    let _g = SCHED.lock();
    // SAFETY: `reg` is a live registration the caller's reference pins; `SCHED` held.
    unsafe { UserspaceServerReg::take_pending_fill_matching(reg, request_id) }
}

/// If a send on `send_endpoint` targets the kernel's end of a Userspace Server channel (its
/// **peer** carries a [`UserspaceServerReg`](crate::object::UserspaceServerReg)
/// back-pointer), return a new reference to that registration: the send is a
/// forwarded-lookup *reply* the kernel completes inline. `None` for an ordinary send, a
/// closed peer, or a registration being destroyed. Acquired under `SCHED`: the back-pointer
/// is uncounted, and once `SCHED` is released another CPU may drop the last binding.
///
/// # Safety
///
/// `send_endpoint` points at a live `IpcChannel`, pinned for the call by a reference the caller
/// holds.
pub unsafe fn us_forward_reg_for_send(send_endpoint: *mut ()) -> Option<ObjectRef> {
    let _g = SCHED.lock();
    // SAFETY: `send_endpoint` is pinned by the caller; `SCHED` held.
    let peer = unsafe { IpcChannel::peer_of(send_endpoint) };
    if peer.is_null() {
        return None;
    }
    // SAFETY: `peer` is the surviving endpoint (the kernel end); `SCHED` held.
    let reg = unsafe { IpcChannel::us_reg_of(peer) };
    if reg.is_null() {
        return None;
    }
    // SAFETY: a registration clears this back-pointer under `SCHED` before it is freed, so
    // `reg` is whole while `SCHED` is held; `try_acquire` refuses one already at zero.
    unsafe { ObjectRef::try_acquire(reg, KObjectType::UserspaceServerReg) }
}

/// Detach a dying [`UserspaceServerReg`] from its endpoint, completing anything still
/// outstanding on it. Called from that type's `Drop`, before its fields drop, because two
/// jobs must happen while the registration is intact:
///
/// 1. **Complete outstanding forwarded requests** `PeerClosed`, so their waiters wake. The
///    endpoint is still alive here, so the wake is well-formed.
/// 2. **Clear the endpoint's back-pointer to us.** `IpcChannel.us_reg` is raw and uncounted
///    (a counted edge would cycle with the registration's ownership of the endpoint), so an
///    endpoint outliving its registration would leave readers following freed memory.
///
/// Takes `SCHED`, sound for the same reason `ipc_endpoint_closing` may: registration
/// references are released only outside `SCHED`.
///
/// # Safety
///
/// `reg` is the `UserspaceServerReg` being dropped: its memory whole, its last reference gone.
pub unsafe fn us_reg_detach(reg: *mut ()) {
    let mut g = SCHED.lock();
    // SAFETY: called from `Drop` with the last reference gone but the object whole;
    // `SCHED` held, satisfying the accessor contract.
    let endpoint = unsafe { UserspaceServerReg::endpoint_ptr(reg) };
    let mut orphans = Orphans::<ORPHANS_PER_REG>::new();
    // SAFETY: as above; `SCHED` held.
    unsafe { fail_outstanding(&mut g, reg, &mut orphans) };
    if !endpoint.is_null() {
        // Clear only a pointer that still names *this* registration: `us_server_attach` can
        // retarget an endpoint, and stomping a live successor's pointer breaks its routing.
        // SAFETY: `endpoint` is pinned by the reference this registration still holds
        // (released by the field drops that follow this call); `SCHED` held.
        if unsafe { IpcChannel::us_reg_of(endpoint) } == reg {
            // SAFETY: as above.
            unsafe { IpcChannel::set_us_reg(endpoint, core::ptr::null_mut()) };
        }
    }
    // Release `SCHED` before dropping the orphans: an `ObjectRef` drop can reach the
    // allocator, which must not happen under the rank-1 lock.
    drop(g);
    drop(orphans);
}

/// The references a teardown takes from registrations' tables, dropped once `SCHED` is released.
struct Orphans<const N: usize> {
    refs: [Option<ObjectRef>; N],
    n: usize,
}

impl<const N: usize> Orphans<N> {
    fn new() -> Self {
        Self { refs: core::array::from_fn(|_| None), n: 0 }
    }

    fn keep(&mut self, r: ObjectRef) {
        self.refs[self.n] = Some(r);
        self.n += 1;
    }
}

/// A registration's tables hold at most a lookup's PO, and a fill's PO and file, a slot.
const ORPHANS_PER_REG: usize = 3 * US_PENDING_MAX;

/// Fail everything `reg` has outstanding `PeerClosed`, its fills too: a faulter waits on one with
/// no deadline, and its file may hold `reg`, a cycle its drop outside `SCHED` ends.
///
/// # Safety
///
/// `reg` is a whole `UserspaceServerReg`, live or mid-drop, and `g` is `SCHED` held.
unsafe fn fail_outstanding<const N: usize>(g: &mut SchedState, reg: *mut (), orphans: &mut Orphans<N>) {
    let closed = crate::syscall::error::KError::PeerClosed as i32;
    // SAFETY: forwarded from this function's contract.
    while let Some(pl) = unsafe { UserspaceServerReg::take_pending_next(reg) } {
        signal_pending_op(g, pl.po.as_ptr(), closed);
        orphans.keep(pl.po);
    }
    // SAFETY: as above.
    while let Some(pf) = unsafe { UserspaceServerReg::take_pending_fill_next(reg) } {
        signal_pending_op(g, pf.po.as_ptr(), closed);
        orphans.keep(pf.po);
        orphans.keep(pf.file_obj);
    }
}

/// Record the endpoint → registration back-pointer that makes `endpoint` the kernel's end of
/// a Userspace Server channel, so a reply sent to it (by the server, on its peer) is
/// completed inline rather than enqueued, unless a live registration is there already: that one
/// comes back to share, from the same `SCHED` hold, so two binds cannot each make one.
///
/// # Safety
///
/// `endpoint` points at a live `IpcChannel`, pinned for the call by a reference the caller holds,
/// and `reg` is a `UserspaceServerReg`.
pub unsafe fn us_server_attach(endpoint: *mut (), reg: &ObjectRef) -> Option<ObjectRef> {
    let _g = SCHED.lock();
    // SAFETY: `endpoint` is a live `IpcChannel` pinned by the caller; `SCHED` held.
    let existing = unsafe { IpcChannel::us_reg_of(endpoint) };
    if !existing.is_null() {
        // `try_acquire`, not `bump`: one mid-drop has a zero count, and is replaced.
        // SAFETY: `existing` addresses a `UserspaceServerReg` whose header is at offset 0;
        // `SCHED` held.
        let header = unsafe { &*(existing as *const crate::object::header::KObjectHeader) };
        if header.try_acquire() {
            // SAFETY: the successful `try_acquire` above balances this adoption.
            return Some(unsafe { ObjectRef::from_raw(existing, KObjectType::UserspaceServerReg) });
        }
    }
    // SAFETY: `endpoint` is a live `IpcChannel` pinned by the caller; `SCHED` held.
    unsafe { IpcChannel::set_us_reg(endpoint, reg.as_ptr()) };
    None
}

/// Take the forwarded lookup outstanding on `reg` whose `request_id` matches a reply's, for
/// the caller to complete. `None` if it does not correlate (duplicate / stale). The returned
/// [`PendingLookup`]'s `po` is dropped by the caller **outside** `SCHED`. The caller pins
/// `reg` (from [`us_forward_reg_for_send`]).
///
/// # Safety
///
/// `reg` points at a live `UserspaceServerReg`, pinned for the call by a reference the caller
/// holds.
pub unsafe fn us_forward_take_pending(reg: *mut (), request_id: u64) -> Option<PendingLookup> {
    let _g = SCHED.lock();
    // SAFETY: `reg` is a live registration the caller's reference pins; `SCHED` held.
    unsafe { UserspaceServerReg::take_pending_matching(reg, request_id) }
}

/// Blocking send (`Block` / `BlockBounded`): deliver `msg` into the peer's receive ring, or
/// — if it is full — **hold** it in the peer's pending-send queue with a reference to the
/// caller's `PendingOperation` `po`, delivered (completing `po`) when the peer next
/// receives. On immediate delivery `po` is **pre-completed** (status 0) so `sys_wait`
/// returns at once. On `PendingFull`/`PeerClosed` the `transfers` stay with the caller.
/// `deadline_ns` is the `BlockBounded` delivery deadline (`u64::MAX` for plain `Block`); on
/// `Queued` with a finite deadline a `PendingSend` heap entry is registered against the PO
/// so the tick can time the held message out (see `fire_expired_deadlines`).
///
/// # Safety
///
/// `endpoint` points at a live `IpcChannel`, pinned for the call by a reference the caller holds.
pub unsafe fn ipc_send_push_blocking(
    endpoint: *mut (),
    msg: &StoredMsg,
    transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    po: &ObjectRef,
    deadline_ns: u64,
) -> BlockSendOutcome {
    let mut g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning `endpoint` and `po`; `SCHED` held.
    if unsafe { closes_a_cycle(endpoint, transfers) } {
        return BlockSendOutcome::Cycle;
    }
    // SAFETY: as above.
    let outcome = unsafe { IpcChannel::send_or_queue(endpoint, msg, transfers, po) };
    match outcome {
        BlockSendOutcome::Sent { woke_edge } => {
            if woke_edge {
                // SAFETY: a successful send proves the peer is non-null; `SCHED` held.
                let peer = unsafe { IpcChannel::peer_of(endpoint) };
                if !peer.is_null() {
                    signal_ipc_endpoint(&mut g, peer);
                }
            }
            // Delivered synchronously — complete the PO now (no waiters yet).
            signal_pending_op(&mut g, po.as_ptr(), 0);
        }
        BlockSendOutcome::Queued if deadline_ns != u64::MAX => {
            // `BlockBounded`: register the delivery deadline against the PO; the held send
            // lives on the peer (receiving) endpoint.
            // SAFETY: the send queued, proving the peer is non-null; `SCHED` held.
            let peer = unsafe { IpcChannel::peer_of(endpoint) };
            // Into a slot the receiving endpoint holds, one for each send it can hold.
            let _ = deadline::push_promised(
                &mut g,
                deadline::Entry {
                    deadline_ns,
                    target: po.as_ptr() as usize,
                    kind: deadline::DeadlineKind::PendingSend,
                    channel: peer as usize,
                },
            );
        }
        _ => {}
    }
    outcome
}

/// Inspect `endpoint`'s receive side under `SCHED` without dequeuing, so the empty-poll
/// `WouldBlock` path allocates no bounce buffer.
///
/// # Safety
///
/// `endpoint` points at a live `IpcChannel`, pinned for the call by a reference the caller holds.
pub unsafe fn ipc_recv_peek(endpoint: *mut ()) -> RecvState {
    let _g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning `endpoint`; `SCHED` held.
    unsafe { IpcChannel::recv_peek(endpoint) }
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
///
/// # Safety
///
/// `endpoint` points at a live `IpcChannel`, pinned for the call by a reference the caller holds.
pub unsafe fn ipc_recv_pop_into(
    endpoint: *mut (),
    dst: &mut StoredMsg,
    out: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    reclaimed: &mut [Option<ReclaimedSend>; IpcChannel::MAX_PENDING_SENDS],
) -> (bool, Option<ObjectRef>) {
    let mut g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning `endpoint`; `SCHED` held.
    let popped = unsafe { IpcChannel::recv_pop_into(endpoint, dst, out) };
    if !popped {
        return (false, None);
    }
    // SAFETY: `endpoint` is pinned; `SCHED` held. A slot just freed — sweep
    // timed-out sends and promote the oldest live one.
    let promoted = unsafe { IpcChannel::promote_pending_send(endpoint, reclaimed) };
    if let Some(ref po) = promoted {
        // Delivered before its deadline: drop its pending-send deadline (idempotent), then
        // complete it.
        deadline::remove(&mut g.deadlines, po.as_ptr() as usize, deadline::DeadlineKind::PendingSend);
        signal_pending_op(&mut g, po.as_ptr(), 0);
    }
    (true, promoted)
}

/// An IPC endpoint is being destroyed (its last handle closed). Under `SCHED`, null the
/// surviving peer's back-pointer to us and wake its blocked receivers (who observe
/// `PeerClosed`). Called only from [`IpcChannel::drop`](crate::object::IpcChannel), whose
/// docs give the no-use-after-free argument and the "refs released only outside `SCHED`"
/// invariant that makes taking the lock here sound.
///
/// # Safety
///
/// `endpoint` is the `IpcChannel` being dropped: its memory whole, its last reference gone.
pub unsafe fn ipc_endpoint_closing(endpoint: *mut ()) {
    let mut g = SCHED.lock();
    // SAFETY: `endpoint` is the object being dropped (still valid memory);
    // `SCHED` held.
    let peer = unsafe { IpcChannel::peer_of(endpoint) };
    // A Userspace Server channel is losing its server: fail forwarded requests pending on
    // the kernel-forwarding endpoint with `PeerClosed`, whether **this** endpoint is the
    // kernel end (registration being torn down) or its **peer** is (server process died).
    // An endpoint is shared by many bindings, so up to `US_PENDING_MAX` lookups may be in
    // flight: drain *all*. Each PO is signalled here and collected into `orphans`; the
    // references are dropped **outside** `SCHED` below.
    let mut orphans = Orphans::<{ 2 * ORPHANS_PER_REG }>::new();
    // SAFETY: `endpoint` is valid memory; `SCHED` held.
    let reg_self = unsafe { IpcChannel::us_reg_of(endpoint) };
    // Non-null does not prove live: `us_reg_detach` clears the back-pointer when a
    // registration drops, but this is a raw read without a count. In the ordinary teardown
    // the registration owns this endpoint and is mid-drop — its `Inner` declares `endpoint`
    // before `pending`, so this `Drop` lands with the pending table intact, which makes the
    // drain correct. A stale registration would be a recycled allocation yielding a garbage
    // `po` (`#GP`/`#PF`); the `magic` sentinel catches it, and skipping the drain is right
    // since its lookups were released with it.
    // SAFETY: non-null and, being a kernel-heap allocation, mapped and readable.
    if !reg_self.is_null() && unsafe { UserspaceServerReg::is_live(reg_self) } {
        // SAFETY: `reg_self` is this endpoint's owning registration (the endpoint
        // is being dropped *because* the registration is — `reg_self` is still
        // valid memory mid-drop); `SCHED` held.
        unsafe { fail_outstanding(&mut g, reg_self, &mut orphans) };
    }
    if !peer.is_null() {
        // SAFETY: `peer` is the surviving endpoint, kept alive by its own handle
        // / a waiter's `ObjectRef`; `SCHED` held.
        unsafe { IpcChannel::clear_peer(peer) };
        signal_ipc_endpoint(&mut g, peer);
        // SAFETY: `peer` is the live surviving endpoint; `SCHED` held.
        let reg_peer = unsafe { IpcChannel::us_reg_of(peer) };
        // Same unchecked back-pointer as `reg_self`: a live `peer` says nothing about its
        // registration still existing.
        // SAFETY: non-null and, being a kernel-heap allocation, mapped and readable.
        if !reg_peer.is_null() && unsafe { UserspaceServerReg::is_live(reg_peer) } {
            // SAFETY: `reg_peer` is pinned by the live `peer` (its owned endpoint);
            // `SCHED` held.
            unsafe { fail_outstanding(&mut g, reg_peer, &mut orphans) };
        }
    }
    // Complete every blocking sender held on THIS endpoint with `PeerClosed`: our receive
    // ring is gone, so their messages can never be delivered. We only *signal* them here;
    // each held entry's message, transfers and PO reference are released when this
    // endpoint's `Inner` drops just after this returns, outside `SCHED`.
    let mut pos = [core::ptr::null_mut(); IpcChannel::MAX_PENDING_SENDS];
    // SAFETY: `endpoint` is valid; `SCHED` held.
    let n = unsafe { IpcChannel::pending_send_pos(endpoint, &mut pos) };
    for &po in &pos[..n] {
        // Drop each held send's pending-send deadline **before** the endpoint is freed, or a
        // live `BlockBounded` deadline could fire later and dereference this dead `channel`.
        // Idempotent (a plain `Block` send, or a timed-out one, has no entry).
        deadline::remove(&mut g.deadlines, po as usize, deadline::DeadlineKind::PendingSend);
        signal_pending_op(&mut g, po, crate::syscall::error::KError::PeerClosed as i32);
    }
    // Release `SCHED` before dropping the orphans (an `ObjectRef` drop must not run under
    // the rank-1 lock).
    drop(g);
    drop(orphans);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::header::test_probe;

    fn adopt<T>(b: KBox<T>, ty: KObjectType) -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(KBox::into_raw(b).as_ptr() as *mut (), ty) }
    }

    fn pair() -> (ObjectRef, ObjectRef) {
        let (a, b) = IpcChannel::try_new_pair(4).unwrap();
        (adopt(a, KObjectType::IpcChannel), adopt(b, KObjectType::IpcChannel))
    }

    fn carrying(r: &ObjectRef) -> [Option<TransferRef>; IPC_HANDLE_MAX] {
        core::array::from_fn(|i| (i == 0).then(|| TransferRef { obj: r.clone(), rights: Rights::empty() }))
    }

    /// The last move of any cycle of endpoints holding each other carries an endpoint already
    /// holding one, or the endpoint it is for.
    #[test]
    fn a_send_that_could_close_a_cycle_of_endpoints_is_refused() {
        init_global_heap();
        test_probe::reset();
        let msg = KBox::<StoredMsg>::try_new_zeroed().unwrap();
        let ((a, b), (c, d)) = (pair(), pair());
        let po = adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
        // SAFETY: every endpoint and the PO are pinned by the test's references.
        unsafe {
            assert_eq!(ipc_send_push(a.as_ptr(), &msg, &mut carrying(&b)), SendOutcome::Cycle, "B into its own ring");
            assert!(matches!(ipc_send_push(c.as_ptr(), &msg, &mut carrying(&b)), SendOutcome::Sent { .. }));
            assert_eq!(ipc_send_push(a.as_ptr(), &msg, &mut carrying(&d)), SendOutcome::Cycle, "D, holding B");
            let blocking = ipc_send_push_blocking(a.as_ptr(), &msg, &mut carrying(&d), &po, u64::MAX);
            assert_eq!(blocking, BlockSendOutcome::Cycle);
        }
        drop((a, b, c, d));
        assert_eq!(test_probe::ipc_channel_destroys(), 4, "an endpoint left holding another");
    }

    fn po() -> ObjectRef {
        adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation)
    }

    /// A registration over the kernel's end of a fresh pair, attached; and the server's end.
    fn served() -> (ObjectRef, ObjectRef) {
        let (k, s) = pair();
        let reg = adopt(UserspaceServerReg::try_new(k.clone()).unwrap(), KObjectType::UserspaceServerReg);
        // SAFETY: `k` is pinned by the test's reference.
        assert!(unsafe { us_server_attach(k.as_ptr(), &reg) }.is_none());
        (reg, s)
    }

    /// A lookup and a fill outstanding on `reg`, their POs, and the fill's file.
    fn outstanding(reg: &ObjectRef, producer: crate::object::Producer) -> (ObjectRef, ObjectRef, ObjectRef) {
        let (lookup, fill) = (po(), po());
        let file = adopt(crate::object::FileObject::try_new(4096, producer).unwrap(), KObjectType::FileObject);
        // SAFETY: `reg` pins the registration, and the test is single-threaded.
        unsafe {
            UserspaceServerReg::begin(reg.as_ptr(), &lookup, Rights::MAP_READ, b"f").unwrap();
            UserspaceServerReg::begin_fill(reg.as_ptr(), &fill, &file, crate::mm::PhysAddr::new(0x5000), 0).unwrap();
        }
        (lookup, fill, file)
    }

    fn failed(po: &ObjectRef) -> bool {
        // SAFETY: `po` pins the `PendingOperation`, and the test is single-threaded.
        unsafe {
            pending_op_is_signaled(po.as_ptr())
                && pending_op_completion(po.as_ptr()).0 == crate::syscall::error::KError::PeerClosed as i32
        }
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
        let (k, _server_end) = pair();
        let new_reg = || adopt(UserspaceServerReg::try_new(k.clone()).unwrap(), KObjectType::UserspaceServerReg);
        let (first, second) = (new_reg(), new_reg());
        // SAFETY: `k` is pinned by the test's reference, and the test is single-threaded.
        unsafe {
            assert!(us_server_attach(k.as_ptr(), &first).is_none());
            let shared = us_server_attach(k.as_ptr(), &second).expect("the first registration");
            assert_eq!(shared.as_ptr(), first.as_ptr());
            assert_eq!(IpcChannel::us_reg_of(k.as_ptr()), first.as_ptr(), "replies route to it");
        }
    }

    #[test]
    fn a_registration_dropped_fails_its_fills() {
        init_global_heap();
        let (reg, _server_end) = served();
        let (lookup, fill, _file) = outstanding(&reg, crate::object::Producer::Stub { base: 0 });
        drop(reg);
        assert!(failed(&lookup) && failed(&fill));
    }

    // TODO(miri-provenance-tests): it fails only under Miri, which no CI job runs yet.
    #[test]
    fn an_endpoint_holding_another_in_a_held_send_is_refused_too() {
        init_global_heap();
        let msg = KBox::<StoredMsg>::try_new_zeroed().unwrap();
        let ((a, b), (c, d)) = (pair(), pair());
        let po = adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
        let mut none: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
        // SAFETY: every endpoint and the PO are pinned by the test's references.
        unsafe {
            while matches!(ipc_send_push(c.as_ptr(), &msg, &mut none), SendOutcome::Sent { .. }) {}
            let held = ipc_send_push_blocking(c.as_ptr(), &msg, &mut carrying(&b), &po, u64::MAX);
            assert_eq!(held, BlockSendOutcome::Queued, "D's ring is full: B waits in its held sends");
            assert_eq!(ipc_send_push(a.as_ptr(), &msg, &mut carrying(&d)), SendOutcome::Cycle);
        }
    }
}
