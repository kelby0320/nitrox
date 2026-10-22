//! IPC delivery: sends, receives, and forwarding to a userspace server.

use super::*;

/// Whether `transfers` holds the endpoint a send on `endpoint` delivers to: it would hold a
/// reference to itself in its own ring, and never be freed. A cycle through two channels is not
/// caught: TODO(ipc-handle-cycles).
///
/// # Safety
///
/// `endpoint` points at a live `IpcChannel`, pinned for the call by a reference the caller holds.
pub unsafe fn ipc_sends_its_receiver(endpoint: *mut (), transfers: &[Option<TransferRef>]) -> bool {
    let _g = SCHED.lock();
    // SAFETY: the caller pins `endpoint`; `SCHED` held. A peer only ever turns null.
    let peer = unsafe { IpcChannel::peer_of(endpoint) };
    transfers.iter().flatten().any(|t| t.obj.as_ptr() == peer)
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
        SendOutcome::Full | SendOutcome::PeerClosed => false,
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
    owner_pid: u32,
    requested: Rights,
    suffix: &[u8],
    fwd: crate::object::userspace_server::Forward<'_>,
) -> ForwardOutcome {
    let mut g = SCHED.lock();
    // SAFETY: `reg` is a live `UserspaceServerReg` pinned by the caller; `SCHED`
    // held. Reserve the pending slot + assign a request id (None ⇒ already busy).
    let request_id = match unsafe { UserspaceServerReg::begin_forward(reg, po, owner_pid, requested, suffix, fwd) } {
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
        SendOutcome::Full | SendOutcome::PeerClosed => {
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
        SendOutcome::Full | SendOutcome::PeerClosed => {
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
/// 1. **Complete outstanding forwarded lookups** `PeerClosed`, so their waiters wake. The
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
    // Bounded by the pending table's size, so the drain cannot overrun this.
    let mut orphans: [Option<ObjectRef>; US_PENDING_MAX] = core::array::from_fn(|_| None);
    let mut n_orphans = 0usize;
    // SAFETY: as above; `SCHED` held.
    while let Some(pl) = unsafe { UserspaceServerReg::take_pending_next(reg) } {
        signal_pending_op(&mut g, pl.po.as_ptr(), crate::syscall::error::KError::PeerClosed as i32);
        orphans[n_orphans] = Some(pl.po);
        n_orphans += 1;
    }
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

/// Record the endpoint → registration back-pointer that makes `endpoint` the kernel's end of
/// a Userspace Server channel, so a reply sent to it (by the server, on its peer) is
/// completed inline rather than enqueued. Called by `sys_ns_bind` once the binding owns the
/// registration, with a reference of its own still held since an unbind on another thread
/// may drop the binding's. Takes `SCHED`.
///
/// # Safety
///
/// `endpoint` points at a live `IpcChannel`, pinned for the call by a reference the caller holds,
/// and `reg` is a `UserspaceServerReg`.
pub unsafe fn us_server_attach(endpoint: *mut (), reg: &ObjectRef) {
    let _g = SCHED.lock();
    // SAFETY: `endpoint` is a live `IpcChannel` pinned by the caller; `SCHED` held.
    unsafe { IpcChannel::set_us_reg(endpoint, reg.as_ptr()) };
}

/// If `endpoint` already backs a Userspace Server registration, return a **new owned
/// reference** to it so another namespace binding can share the connection (*bind-mount*
/// semantics: one endpoint, many names). `None` if not yet registered. Reusing the
/// registration keeps reply routing correct: the endpoint→reg back-pointer stays a single
/// target rather than being clobbered by a rival. Takes `SCHED`.
///
/// # Safety
///
/// `endpoint` points at a live `IpcChannel`, pinned for the call by a reference the caller holds.
pub unsafe fn us_forward_existing_reg(endpoint: *mut ()) -> Option<ObjectRef> {
    let _g = SCHED.lock();
    // SAFETY: `endpoint` is a live `IpcChannel` pinned by the caller; `SCHED` held.
    let reg = unsafe { IpcChannel::us_reg_of(endpoint) };
    if reg.is_null() {
        return None;
    }
    // Acquire rather than `bump`: an unconditional increment would resurrect an object whose
    // count already reached zero. `try_acquire` refuses at zero (`Arc::upgrade` semantics);
    // together with `us_reg_detach` clearing the back-pointer at teardown this makes the
    // race unrepresentable rather than merely unlikely.
    // SAFETY: `reg` addresses a `UserspaceServerReg` whose header is at offset 0;
    // `SCHED` held.
    let header = unsafe { &*(reg as *const crate::object::header::KObjectHeader) };
    if !header.try_acquire() {
        return None;
    }
    // SAFETY: the successful `try_acquire` above balances this adoption.
    Some(unsafe { ObjectRef::from_raw(reg, KObjectType::UserspaceServerReg) })
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
            // Heap-full degrades to an unbounded `Block` (the message still delivers, just
            // without a timeout); the reserve far exceeds realistic pending sends.
            let _ = deadline::push(
                &mut g.deadlines,
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
    // A Userspace Server channel is losing its server: fail forwarded lookups pending on
    // the kernel-forwarding endpoint with `PeerClosed`, whether **this** endpoint is the
    // kernel end (registration being torn down) or its **peer** is (server process died).
    // An endpoint is shared by many bindings, so up to `US_PENDING_MAX` lookups may be in
    // flight: drain *all*. Each PO is signalled here and collected into `orphans`; the
    // references are dropped **outside** `SCHED` below.
    let mut orphans: [Option<ObjectRef>; 2 * US_PENDING_MAX] = core::array::from_fn(|_| None);
    let mut n_orphans = 0usize;
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
        while let Some(pl) = unsafe { UserspaceServerReg::take_pending_next(reg_self) } {
            signal_pending_op(&mut g, pl.po.as_ptr(), crate::syscall::error::KError::PeerClosed as i32);
            orphans[n_orphans] = Some(pl.po);
            n_orphans += 1;
        }
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
            while let Some(pl) = unsafe { UserspaceServerReg::take_pending_next(reg_peer) } {
                signal_pending_op(&mut g, pl.po.as_ptr(), crate::syscall::error::KError::PeerClosed as i32);
                orphans[n_orphans] = Some(pl.po);
                n_orphans += 1;
            }
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

    #[test]
    fn a_send_carrying_the_endpoint_it_delivers_to_is_caught() {
        init_global_heap();
        let (a, b) = IpcChannel::try_new_pair(1).unwrap();
        let a = into_objref_of(a, KObjectType::IpcChannel);
        let b = into_objref_of(b, KObjectType::IpcChannel);
        let carrying = |r: &ObjectRef| [Some(TransferRef { obj: r.clone(), rights: Rights::empty() }), None];
        // SAFETY: `a` and `b` pin the pair for each call.
        unsafe {
            assert!(ipc_sends_its_receiver(a.as_ptr(), &carrying(&b)), "a send on `a` delivers to `b`");
            assert!(!ipc_sends_its_receiver(a.as_ptr(), &carrying(&a)), "`b` holding `a` is no cycle");
            assert!(ipc_sends_its_receiver(b.as_ptr(), &carrying(&a)));
        }
    }

    fn into_objref_of<T>(b: KBox<T>, ty: KObjectType) -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe { ObjectRef::from_raw(KBox::into_raw(b).as_ptr() as *mut (), ty) }
    }
}
