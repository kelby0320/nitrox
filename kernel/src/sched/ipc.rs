//! IPC delivery: sends, receives, and forwarding to a userspace server.

use super::*;

/// Send `msg` from `endpoint` (into its peer's receive ring) under `SCHED`,
/// **moving** any `transfers` it carries into the queued slot and waking the
/// peer's blocked receivers if the ring went empty→non-empty. The caller has
/// already copied the message in from user memory (no copy under the lock). On
/// `Full`/`PeerClosed` the `transfers` are left with the caller (untaken), to be
/// dropped **outside** `SCHED`. Returns the [`SendOutcome`].
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

/// Outcome of [`us_forward_originate`] — how a forwarded `sys_ns_lookup` fared
/// when the kernel tried to hand its `Namespace::Resolve` request to the server.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum ForwardOutcome {
    /// The request was delivered; the lookup's `PendingOperation` is left
    /// **pending** and will complete when the server replies.
    Pending,
    /// A forwarded lookup is already outstanding on this server (N = 1) — the
    /// caller fails the new lookup (`WouldBlock`).
    Busy,
    /// The server's inbox ring is full — the caller fails the lookup
    /// (`WouldBlock`).
    Full,
    /// The server endpoint has closed — the caller fails the lookup
    /// (`PeerClosed`).
    PeerClosed,
}

/// Push a **fire-and-forget** message into a registration's server inbox: no pending slot,
/// no `request_id`, no reply expected. `true` if it was queued.
///
/// The sibling of [`us_forward_originate`] for the one message the kernel sends a server
/// without asking it anything — `File::Touch` after a Model A writeback. Nothing is
/// reserved, so nothing needs rolling back: a full ring or a dead server just drops the
/// notification, which degrades to the behaviour that existed before it (a stale `mtime`)
/// rather than failing the caller's `sys_file_sync`, whose data is already on the device.
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

/// Originate a forwarded namespace lookup: under one `SCHED` hold, reserve the
/// registration's (single) pending-lookup slot — assigning a `request_id` and
/// recording the lookup `po` / `owner_pid` / requested `Rights` — stamp that id
/// into the already-built `Namespace::Resolve` request `msg`, and push the request
/// into the server's inbox (the peer of the registration's kernel endpoint),
/// waking the server's blocked receivers. The lookup PO is left **uncompleted**
/// (the server's reply completes it later, inline in its send — see
/// [`us_forward_take_pending`]). On `Busy` nothing was reserved; on `Full` /
/// `PeerClosed` the reserved slot is rolled back and its `po` clone dropped
/// outside `SCHED`. `reg` is the [`UserspaceServerReg`] the lookup resolved to;
/// the caller pins it (and `po`) for the call.
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
    // Stamp the assigned id into the request envelope (the body was built by the
    // caller with a zero placeholder).
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
            // Roll back the slot we just reserved (by its `request_id`); drop its
            // `po` clone **outside** `SCHED`.
            // SAFETY: `reg` live, `SCHED` held.
            let pl = unsafe { UserspaceServerReg::take_pending_matching(reg, request_id) };
            drop(g);
            drop(pl);
            if outcome == SendOutcome::Full { ForwardOutcome::Full } else { ForwardOutcome::PeerClosed }
        }
    }
}

/// Originate a forwarded page-cache **fill**: the page-fault sibling of
/// [`us_forward_originate`]. Under one `SCHED` hold, reserve the registration's
/// (single) pending-fill slot — recording the fill `po` / `file_obj` / `frame` /
/// page `index` and assigning a `request_id` — stamp that id into the already-built
/// `File::ReadRange` request `msg`, and push it into the server's inbox. The fill
/// PO is left **uncompleted** (the server's `ReadRange` reply completes it inline in
/// its send — see [`us_forward_take_pending_fill`]). On `Busy` nothing is reserved;
/// on `Full` / `PeerClosed` the reserved slot is rolled back and its references
/// dropped outside `SCHED`. `reg` is the [`UserspaceServerReg`] the faulted
/// `FileObject`'s producer points at; the caller pins it (and `po` / `file_obj`).
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
            // Roll back the slot we just reserved (by its `request_id`); drop the
            // fill's `po` / `file_obj` clones **outside** `SCHED`.
            // SAFETY: `reg` live, `SCHED` held.
            let pf = unsafe { UserspaceServerReg::take_pending_fill_matching(reg, request_id) };
            drop(g);
            drop(pf);
            if outcome == SendOutcome::Full { ForwardOutcome::Full } else { ForwardOutcome::PeerClosed }
        }
    }
}

/// Take the forwarded **fill** outstanding on `reg` whose `request_id` matches a
/// `File::ReadRange` reply's, for the caller to land (copy the bytes into the
/// frame, mark the page ready, complete the PO). `None` if it does not correlate
/// (duplicate / stale). Takes `SCHED`; the returned [`PendingFill`]'s `po` /
/// `file_obj` are dropped by the caller **outside** `SCHED`. The caller pins `reg`: the
/// reference [`us_forward_reg_for_send`] returned.
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

/// If a send on `send_endpoint` targets the kernel's end of a Userspace Server
/// channel — i.e. its **peer** carries a
/// [`UserspaceServerReg`](crate::object::UserspaceServerReg) back-pointer — return
/// a new reference to that registration (the send is a forwarded-lookup *reply* the kernel
/// completes inline). `None` for an ordinary channel send, a closed peer, or a registration
/// already being destroyed. Acquired under `SCHED`: the back-pointer is uncounted, and once
/// `SCHED` is released another CPU may drop the registration's last binding.
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

/// Detach a dying [`UserspaceServerReg`] from its endpoint, completing anything
/// still outstanding on it. Called from that type's `Drop`, before its fields drop.
///
/// Two jobs, both of which must happen while the registration is still intact:
///
/// 1. **Complete outstanding forwarded lookups** `PeerClosed`, so their waiters wake
///    rather than hang. The endpoint is still alive here, so the wake is well-formed.
/// 2. **Clear the endpoint's back-pointer to us.** `IpcChannel.us_reg` is raw and
///    uncounted (a counted edge would cycle with the registration's ownership of the
///    endpoint), and until this existed nothing ever cleared it — so an endpoint that
///    outlived its registration left every consumer of that pointer reading freed
///    memory. See the `Drop` impl for the full list.
///
/// Takes `SCHED`, which is sound for the same reason `ipc_endpoint_closing` may:
/// registration references are released only outside `SCHED`.
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
        // Clear only a pointer that still names *this* registration. `us_server_attach`
        // can retarget an endpoint at a different one, and stomping a live successor's
        // pointer would silently break its reply routing.
        // SAFETY: `endpoint` is pinned by the reference this registration still holds
        // (released by the field drops that follow this call); `SCHED` held.
        if unsafe { IpcChannel::us_reg_of(endpoint) } == reg {
            // SAFETY: as above.
            unsafe { IpcChannel::set_us_reg(endpoint, core::ptr::null_mut()) };
        }
    }
    // Release `SCHED` before dropping the orphaned references: an `ObjectRef` drop can
    // reach the allocator, which must not happen under the rank-1 lock.
    drop(g);
    drop(orphans);
}

/// Record the endpoint → registration back-pointer that makes `endpoint` the
/// kernel's end of a Userspace Server channel: a reply sent to it (by the server,
/// on its peer) is then completed inline rather than enqueued. Called by
/// `sys_ns_bind` once the `UserspaceServer` binding owns the registration, with a reference of
/// its own still held: an unbind on another thread may drop the binding's. Takes `SCHED` (the
/// accessor contract). The caller pins `endpoint` (its bound handle).
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
/// reference** to it (bump-and-adopt) so an additional namespace binding can share
/// the one connection — *bind-mount* semantics: one server endpoint, many names,
/// each with its own subtree base + rights on the binding. `None` if the endpoint is
/// not yet registered (the caller mints a fresh registration). Reusing the existing
/// registration is what keeps reply routing correct: the endpoint→reg back-pointer
/// stays a single, consistent target rather than being clobbered by a rival reg.
/// Takes `SCHED`; the caller pins `endpoint` (its bound handle).
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
    // Acquire rather than `bump`: the old code asserted "the existing binding still
    // holds a reference (refcount >= 1)" and incremented unconditionally, which
    // resurrects an object whose count has already reached zero — handing out an owned
    // reference to memory that is being destroyed. `try_acquire` refuses at zero, which
    // is the `Arc::upgrade` semantics this lookup actually needs. (`us_reg_detach` now
    // clears the back-pointer at teardown, so reaching a dead registration here should
    // be impossible; this makes the race unrepresentable rather than merely unlikely.)
    // SAFETY: `reg` addresses a `UserspaceServerReg` whose header is at offset 0;
    // `SCHED` held.
    let header = unsafe { &*(reg as *const crate::object::header::KObjectHeader) };
    if !header.try_acquire() {
        return None;
    }
    // SAFETY: the successful `try_acquire` above balances this adoption.
    Some(unsafe { ObjectRef::from_raw(reg, KObjectType::UserspaceServerReg) })
}

/// Take the forwarded lookup outstanding on `reg` whose `request_id` matches a
/// reply's, for the caller to complete (cross-context install + signal). `None`
/// if the reply does not correlate (duplicate / stale). Takes `SCHED`; the
/// returned [`PendingLookup`]'s `po` is dropped by the caller **outside** `SCHED`.
/// The caller pins `reg`: the reference [`us_forward_reg_for_send`] returned.
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

/// Blocking send (`Block` / `BlockBounded`): deliver `msg` into the peer's
/// receive ring, or — if it is full — **hold** it in the peer's pending-send
/// queue with a reference to the caller's `PendingOperation` `po`, to be
/// delivered (completing `po`) when the peer next receives. On immediate
/// delivery `po` is **pre-completed** (status 0) here so the caller's `sys_wait`
/// returns at once. The caller has copied the message in and holds `po`'s
/// creation reference + the endpoint reference. Returns the [`BlockSendOutcome`];
/// on `PendingFull`/`PeerClosed` the `transfers` are left for the caller to drop.
/// `deadline_ns` is the `BlockBounded` delivery deadline (absolute monotonic ns);
/// `u64::MAX` for plain `Block` (no deadline). On a `Queued` outcome with a finite
/// deadline, a `PendingSend` deadline-heap entry is registered against the PO (so
/// the timer tick can time the held message out — see `fire_expired_deadlines`).
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
            // `BlockBounded`: register the delivery deadline against the PO. The
            // held send lives on the peer (receiving) endpoint.
            // SAFETY: the send queued, proving the peer is non-null; `SCHED` held.
            let peer = unsafe { IpcChannel::peer_of(endpoint) };
            // Heap-full degrades to an unbounded `Block` (the message still
            // delivers, just without a timeout) — the reserve (16) far exceeds
            // realistic pending sends, so this is a pathological edge only.
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

/// Inspect `endpoint`'s receive side under `SCHED` without dequeuing — so the
/// empty-poll `WouldBlock` path allocates no bounce buffer.
///
/// # Safety
///
/// `endpoint` points at a live `IpcChannel`, pinned for the call by a reference the caller holds.
pub unsafe fn ipc_recv_peek(endpoint: *mut ()) -> RecvState {
    let _g = SCHED.lock();
    // SAFETY: the caller holds an `ObjectRef` pinning `endpoint`; `SCHED` held.
    unsafe { IpcChannel::recv_peek(endpoint) }
}

/// Pop the oldest message from `endpoint`'s inbox into `dst` under `SCHED`,
/// **moving** its transferred-handle references into `out`. Returns `false` if it
/// was drained between the peek and now. The caller copies `dst` out to user
/// memory and installs/drops the `out` transfers **after** this returns (never
/// under the lock — `ObjectRef` Drop / `allocate` must not run under `SCHED`).
/// Returns `(popped, promoted_po)`: `popped` is `false` if the inbox was drained
/// between peek and pop. Popping frees a ring slot, so a held blocking sender (a
/// `Block`/`BlockBounded` send waiting for space) is **promoted** into the ring
/// and its `PendingOperation` completed (status 0) under `SCHED`; its returned
/// reference must be dropped by the caller **outside** `SCHED` (no `ObjectRef`
/// Drop under the lock).
/// Pop into `dst`; on success, sweep any **timed-out** held sends into
/// `reclaimed` (the caller drops them outside `SCHED`) and promote the oldest
/// **live** held sender into the freed slot, completing its `PendingOperation`
/// (and dropping its pending-send deadline). Returns `(popped, promoted_po)`; the
/// `promoted_po` and the `reclaimed` entries are released by the caller outside
/// `SCHED`.
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
        // Delivered before its deadline: drop its pending-send deadline entry
        // (idempotent — a plain `Block` send registered none), then complete it.
        deadline::remove(&mut g.deadlines, po.as_ptr() as usize, deadline::DeadlineKind::PendingSend);
        signal_pending_op(&mut g, po.as_ptr(), 0);
    }
    (true, promoted)
}

/// An IPC endpoint is being destroyed (its last handle closed). Under `SCHED`,
/// null the surviving peer's back-pointer to us and wake its blocked receivers
/// (which then observe `PeerClosed`). Called only from
/// [`IpcChannel::drop`](crate::object::IpcChannel) — see that type's docs for
/// the no-use-after-free argument and the "refs released only outside `SCHED`"
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
    // A Userspace Server channel is losing its server: fail any forwarded lookup
    // pending on the kernel-forwarding endpoint with `PeerClosed`. Two cases —
    // **this** endpoint is the kernel end (its registration is being torn down),
    // or its **peer** is (the server process just died). Each taken PO is signalled
    // here; its reference is dropped **outside** `SCHED` (below).
    // A registration's endpoint is shared by many bindings (bind-mount semantics),
    // so it can hold up to `US_PENDING_MAX` forwarded lookups in flight — drain *all*
    // of them, failing each `PeerClosed`. Each taken PO is signalled here and
    // collected into `orphans`; the references are dropped **outside** `SCHED` below.
    let mut orphans: [Option<ObjectRef>; 2 * US_PENDING_MAX] = core::array::from_fn(|_| None);
    let mut n_orphans = 0usize;
    // SAFETY: `endpoint` is valid memory; `SCHED` held.
    let reg_self = unsafe { IpcChannel::us_reg_of(endpoint) };
    // `us_reg` is a raw back-pointer that **nothing clears**, so non-null does not
    // mean live. In the ordinary teardown the registration owns this endpoint and is
    // mid-drop right now — its `Inner` declares `endpoint` before `pending`, so the
    // endpoint's `Drop` lands here with the pending table still intact, which is what
    // makes the drain below correct. But when anything else holds a reference to this
    // endpoint (a server handle, a blocked waiter), the registration is freed *first*
    // and this pointer dangles; draining it then reads a recycled allocation and hands
    // `signal_pending_op` a `po` built from whatever now occupies that memory —
    // observed as the kernel-stack paint poison, log text, and x86 instruction bytes,
    // crashing with `#GP`/`#PF` about one boot in four under load.
    //
    // The `magic` sentinel was written for exactly this and had never been read.
    // A stale registration fails it, and skipping the drain is the correct response:
    // that registration's `pending` array was dropped with it, so its lookups have
    // already been released — there is nothing left here to complete.
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
        // Same unchecked back-pointer as `reg_self` above: `peer` being live says
        // nothing about *its* registration still existing, since nothing clears the
        // pointer when a registration is destroyed.
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
    // Complete every blocking sender held on THIS endpoint with `PeerClosed`:
    // our receive ring is gone, so their messages can never be delivered. We
    // only *signal* them here (waking the senders); each held entry's message,
    // transfers, and PO reference are released when this endpoint's `Inner`
    // drops, immediately after this returns — outside `SCHED`.
    let mut pos = [core::ptr::null_mut(); IpcChannel::MAX_PENDING_SENDS];
    // SAFETY: `endpoint` is valid; `SCHED` held.
    let n = unsafe { IpcChannel::pending_send_pos(endpoint, &mut pos) };
    for &po in &pos[..n] {
        // Drop each held send's pending-send deadline entry **before** the
        // endpoint is freed — otherwise a still-live `BlockBounded` deadline could
        // fire later and dereference this now-dead `channel`. Idempotent (a
        // plain `Block` send, or one already timed out, has no live entry).
        deadline::remove(&mut g.deadlines, po as usize, deadline::DeadlineKind::PendingSend);
        signal_pending_op(&mut g, po, crate::syscall::error::KError::PeerClosed as i32);
    }
    // Release `SCHED` before dropping any orphaned forwarded-lookup PO references
    // (an `ObjectRef` Drop must not run under the rank-1 lock).
    drop(g);
    drop(orphans);
}
