//! The [`IpcChannel`] kernel object — one **endpoint** of a bidirectional IPC channel.
//!
//! A channel is a **pair** of endpoint objects, each with its own receive ring and recv-waiter
//! list, linked by a mutual uncounted `peer` pointer: a send pushes into the *peer's* ring, a recv
//! drains the endpoint's *own*. See `docs/spec/ipc-message-format.md`.
//!
//! All interior state lives in a [`SchedCell`], borrowed only with the token the rank-1 `SCHED`
//! lock keeps (see `kernel/docs/lock-ordering.md`).
//!
//! When an endpoint's last reference goes, its closing runs [`crate::sched::ipc_endpoint_closing`]
//! before the box is rebuilt: under `SCHED` it clears the *surviving* peer's `peer` pointer and
//! wakes its blocked receivers (they observe `PeerClosed`). The survivor is alive (pinned by its
//! handle or a waiter); the second endpoint to close finds its own `peer` already cleared.
//! **Invariant:** endpoint references are released only in syscall/boot context, **never under
//! `SCHED`**, so the closing may itself take `SCHED`.

use crate::sched::{SchedCell, SchedToken, SlotHold, Slots, follow};

use crate::libkern::handle::{KObjectType, Rights};
use crate::libkern::ipc::{IPC_HANDLE_MAX, IPC_MAX_QUEUE_DEPTH, IPC_MSG_SIZE, IPC_PAYLOAD_SIZE, IpcMsgHeader};
use crate::libkern::{AllocError, KBox, KVec, Reserved};
use crate::object::header::{Header, Ref, Shared, Weak, kobject};
use crate::object::{ObjectRef, PendingOperation, UserspaceServerReg};
use crate::object::{WaitQueue, Waiters};

/// A queued message in kernel storage: the natural-alignment twin of
/// [`IpcMsg`](crate::libkern::IpcMsg), so a byte view round-trips through `copy_*_user`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct StoredMsg {
    /// The 24-byte header (kernel stamps `sender_pid` / `timestamp` at send).
    pub header: IpcMsgHeader,
    /// Inline payload bytes.
    pub payload: [u8; IPC_PAYLOAD_SIZE],
    /// Transferable-handle slots.
    pub handles: [u64; crate::libkern::ipc::IPC_HANDLE_MAX],
}
crate::libkern::pod::pod!(StoredMsg { header, payload, handles });

const _: () = assert!(core::mem::size_of::<StoredMsg>() == IPC_MSG_SIZE);
// `IpcMsg` and `StoredMsg` share field offsets (`IpcMsg`'s page alignment adds no padding).
const _: () = assert!(core::mem::offset_of!(StoredMsg, payload) == 24);

impl StoredMsg {
    /// An all-zero message.
    pub fn zeroed() -> Self {
        <Self as crate::libkern::Pod>::zeroed()
    }

    /// Zero what a receiver is promised zeroed, the header's padding and the payload past
    /// `header.payload_len`: a sender's buffer can carry stale bytes there, a typed password among
    /// them. `flags` is the sender's, and passes. `None` if the length claims more than the
    /// payload holds.
    pub fn seal(&mut self) -> Option<usize> {
        let len = self.header.payload_len as usize;
        self.payload.get_mut(len..)?.fill(0);
        self.header._pad1 = 0;
        self.header._pad2 = [0; 4];
        Some(len)
    }

    /// The 4096 wire bytes, for `copy_slice_to_user`.
    pub fn as_bytes(&self) -> &[u8; IPC_MSG_SIZE] {
        crate::libkern::pod::byte_array_of(self)
    }

    /// The 4096 wire bytes, mutably, for `copy_slice_from_user`.
    pub fn as_bytes_mut(&mut self) -> &mut [u8; IPC_MSG_SIZE] {
        crate::libkern::pod::byte_array_of_mut(self)
    }
}

/// One handle moved through IPC, held by the kernel while the message is queued: the object
/// reference (pinning it) plus the rights to install it with. An undelivered transfer's ref is
/// released when the endpoint is destroyed.
pub struct TransferRef {
    /// The pinned transferred object.
    pub obj: ObjectRef,
    /// Rights to install the receiver's new handle with (the sender's, by move).
    pub rights: Rights,
}

/// A queued message slot: the wire bytes plus in-flight transfer references, **moved** in/out.
struct RingSlot {
    msg: StoredMsg,
    transfers: [Option<TransferRef>; IPC_HANDLE_MAX],
}

// A ring is one allocation: at the deepest queue it must fit the buddy's largest block, less
// a page for the slab's descriptor, or every channel that deep is refused.
const _: () = assert!(
    IPC_MAX_QUEUE_DEPTH as usize * core::mem::size_of::<RingSlot>()
        <= (crate::mm::PAGE_SIZE << crate::mm::buddy::MAX_ORDER) - crate::mm::PAGE_SIZE
);

impl RingSlot {
    fn empty() -> Self {
        RingSlot { msg: StoredMsg::zeroed(), transfers: core::array::from_fn(|_| None) }
    }
}

/// A fixed-capacity ring of message slots, pre-filled at construction (the only fallible
/// growth); `push_from`/`pop_into` move messages in place, so they are safe under `SCHED`.
struct MsgRing {
    slots: KVec<RingSlot>,
    head: usize,
    len: usize,
}

impl MsgRing {
    fn with_capacity(cap: usize) -> Result<Self, AllocError> {
        let mut slots: KVec<RingSlot> = KVec::new();
        slots.try_reserve(cap)?;
        for _ in 0..cap {
            slots.try_push(RingSlot::empty())?;
        }
        Ok(MsgRing { slots, head: 0, len: 0 })
    }

    fn cap(&self) -> usize {
        self.slots.len()
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len == self.cap()
    }

    fn push_from(&mut self, src: &StoredMsg, transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX]) -> bool {
        if self.is_full() {
            return false;
        }
        let cap = self.cap();
        let idx = (self.head + self.len) % cap;
        let slot = &mut self.slots[idx];
        slot.msg = *src;
        for (dst, src) in slot.transfers.iter_mut().zip(transfers.iter_mut()) {
            *dst = src.take();
        }
        self.len += 1;
        true
    }

    fn pop_into(&mut self, dst: &mut StoredMsg, out: &mut [Option<TransferRef>; IPC_HANDLE_MAX]) -> bool {
        if self.is_empty() {
            return false;
        }
        let slot = &mut self.slots[self.head];
        *dst = slot.msg;
        for (dst, src) in out.iter_mut().zip(slot.transfers.iter_mut()) {
            *dst = src.take();
        }
        self.head = (self.head + 1) % self.cap();
        self.len -= 1;
        true
    }
}

/// Outcome of a [`send_push`](IpcChannel::send_push).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SendOutcome {
    /// Queued. `woke_edge` is `true` iff the peer's ring went empty→non-empty (wake its receivers).
    Sent { woke_edge: bool },
    /// The peer's receive ring is full (NoBlock → `WouldBlock`).
    Full,
    /// The peer endpoint has closed.
    PeerClosed,
    /// Refused: it carries the endpoint it is for, or an endpoint holding one, and could leave
    /// endpoints holding each other, which nothing frees. Only the syscall's sends check.
    Cycle,
}

/// Outcome of a blocking send ([`send_or_queue`](IpcChannel::send_or_queue)): a full ring means
/// the message is **held**; the only hard failure is the pending-send queue overflowing.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BlockSendOutcome {
    /// Delivered into the peer's ring; the caller's [`PendingOperation`] completes immediately.
    Sent { woke_edge: bool },
    /// The peer's ring was full: the message (with its transfers and a reference to the caller's
    /// PO) is held in the peer's pending-send queue and delivered when the peer next receives.
    Queued,
    /// Pending-send queue full — back-pressure; transfers are left for the caller (`WouldBlock`).
    PendingFull,
    /// The peer endpoint has closed.
    PeerClosed,
    /// Refused, as [`SendOutcome::Cycle`].
    Cycle,
}

/// A blocking send held in a receiving endpoint's pending-send queue: the message, its
/// in-flight transfers, and an owning reference to the sender's [`PendingOperation`] (alive even
/// if the sender closes early). Delivered, completing `po` with status 0, when the endpoint
/// next receives; completed `PeerClosed` (transfers reclaimed) when it closes.
struct PendingSend {
    msg: StoredMsg,
    transfers: [Option<TransferRef>; IPC_HANDLE_MAX],
    po: Ref<PendingOperation>,
    /// Set when a `BlockBounded` deadline elapsed: `po` was already completed `TimedOut`, so the
    /// entry is swept out at the next `promote_pending_send` (or at close), not delivered.
    cancelled: bool,
}

/// The droppable parts of a cancelled [`PendingSend`], handed back by
/// [`promote_pending_send`](IpcChannel::promote_pending_send) for release **outside** `SCHED`:
/// their `Drop`s may take lower-rank locks (the buddy allocator) that must not nest under it.
pub struct ReclaimedSend {
    pub po: Ref<PendingOperation>,
    pub transfers: [Option<TransferRef>; IPC_HANDLE_MAX],
}

/// State of an endpoint's own receive side, for [`recv_peek`](IpcChannel::recv_peek).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RecvState {
    /// At least one message is queued.
    HasMsg,
    /// No message, but the peer is still open (→ `WouldBlock`).
    Empty,
    /// No message and the peer has closed (→ `PeerClosed`).
    PeerClosed,
}

/// One endpoint of an IPC channel.
#[repr(C)]
pub struct IpcChannel {
    header: Header<Self>,
    magic: u64,
    inner: SchedCell<Inner>,
    /// Its sends' deadline-heap slots, given back after its closing takes their entries out.
    _slots: SlotHold,
}

kobject!(IpcChannel, closing = crate::sched::ipc_endpoint_closing);
// SAFETY: an endpoint changes once shared only in `inner`, a `SchedCell`.
unsafe impl Shared for IpcChannel {}

struct Inner {
    recv: MsgRing,
    /// Threads blocked recv-ing (type-erased, non-owning; removed before a waiter unparks), at most
    /// [`IpcChannel::MAX_WAITERS`].
    recv_waiters: WaitQueue<{ IpcChannel::MAX_WAITERS }>,
    /// Blocking senders awaiting space in **this** ring, FIFO, promoted into `recv` when a recv
    /// frees a slot. Pre-reserved to [`IpcChannel::MAX_PENDING_SENDS`]: no `SCHED` allocation.
    pending_sends: Reserved<PendingSend>,
    /// The other endpoint, until its closing clears this.
    peer: Option<Weak<IpcChannel>>,
    /// Set iff this is the **kernel's** end of a Userspace Server channel: the registration that
    /// owns this endpoint, until its closing clears this. A reply sent here (by the server, on its
    /// peer) is a forwarded-lookup reply the kernel completes inline rather than enqueues.
    us_reg: Option<Weak<UserspaceServerReg>>,
}

impl IpcChannel {
    /// Sentinel written into [`IpcChannel::magic`] at construction.
    pub const MAGIC: u64 = 0x49_70_63_43_68_21_21_21; // "IpcCh!!!"

    /// Maximum simultaneous receivers blocked on one endpoint; bounds the pre-reserved vector.
    pub const MAX_WAITERS: usize = 4;

    /// Maximum messages in one endpoint's pending-send queue; bounds the pre-reserved vector,
    /// beyond which a blocking send gets `WouldBlock` back-pressure.
    pub const MAX_PENDING_SENDS: usize = 4;

    /// Create a connected endpoint **pair**, each with a `depth`-slot receive ring (clamped to
    /// `1..=IPC_MAX_QUEUE_DEPTH`). Each returned `Ref` is its creation reference.
    pub fn try_new_pair(depth: u32) -> Result<(Ref<Self>, Ref<Self>), AllocError> {
        let cap = depth.clamp(1, IPC_MAX_QUEUE_DEPTH) as usize;
        let (a, b) = (Self::try_new_endpoint(cap)?, Self::try_new_endpoint(cap)?);
        let (a, b) = (KBox::into_raw(a), KBox::into_raw(b));
        // Each peer is the other box's own pointer, so no later borrow of that box retires it.
        // SAFETY: both endpoints are this function's alone, so `SCHED` is not needed, and each
        // write goes through its box's own pointer; each box is rebuilt from the pointer it gave.
        // Each `Weak` is kept in its endpoint's cell, behind `SCHED`, and its target's closing
        // clears it there before the target is freed.
        unsafe {
            (*SchedCell::raw_get(&raw const (*a.as_ptr()).inner)).peer = Some(Weak::new(b.as_ptr()));
            (*SchedCell::raw_get(&raw const (*b.as_ptr()).inner)).peer = Some(Weak::new(a.as_ptr()));
            Ok((Ref::new(KBox::from_raw(a)), Ref::new(KBox::from_raw(b))))
        }
    }

    /// Allocate one unconnected endpoint, refcount one. Its ring and held sends are reserved up
    /// front, so later pushes never allocate under `SCHED`.
    fn try_new_endpoint(cap: usize) -> Result<KBox<Self>, AllocError> {
        let slots = SlotHold::reserve(Slots::Endpoint)?;
        let recv = MsgRing::with_capacity(cap)?;
        let pending_sends = Reserved::with_capacity(Self::MAX_PENDING_SENDS)?;
        KBox::try_new(Self {
            header: Header::new(),
            magic: Self::MAGIC,
            inner: SchedCell::new(Inner {
                recv,
                recv_waiters: WaitQueue::new(),
                pending_sends,
                peer: None,
                us_reg: None,
            }),
            _slots: slots,
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    // --- What the scheduler reaches, with its token ---------------------

    /// The peer endpoint, until it closes, with the token lent back.
    pub(crate) fn peer<'t>(&self, tok: &'t mut SchedToken) -> Option<(&'t IpcChannel, &'t mut SchedToken)> {
        follow(&self.inner, tok, |i| i.peer.as_ref())
    }

    /// The peer's address, null once it has closed: a key, never followed.
    pub(crate) fn peer_addr(&self, tok: &SchedToken) -> *const IpcChannel {
        self.inner.read(tok).peer.as_ref().map_or(core::ptr::null(), |p| p.as_ptr())
    }

    /// Forget the peer, which is closing.
    pub(crate) fn clear_peer(&self, tok: &mut SchedToken) {
        self.inner.get(tok).peer = None;
    }

    /// Make this endpoint the kernel's end of `reg`'s Userspace Server channel.
    pub(crate) fn set_us_reg(&self, tok: &mut SchedToken, reg: Weak<UserspaceServerReg>) {
        self.inner.get(tok).us_reg = Some(reg);
    }

    /// Forget `reg`, which is closing, if this endpoint still names it: another may have replaced it.
    pub(crate) fn clear_us_reg_if(&self, tok: &mut SchedToken, reg: &UserspaceServerReg) {
        let us_reg = &mut self.inner.get(tok).us_reg;
        if us_reg.as_ref().is_some_and(|r| r.names(reg)) {
            *us_reg = None;
        }
    }

    /// The registration this endpoint is the kernel's end for, with the token lent back.
    pub(crate) fn us_reg<'t>(&self, tok: &'t mut SchedToken) -> Option<(&'t UserspaceServerReg, &'t mut SchedToken)> {
        follow(&self.inner, tok, |i| i.us_reg.as_ref())
    }

    /// The registration's address, null with none: a key, never followed.
    #[cfg(test)]
    pub(crate) fn us_reg_addr(&self, tok: &SchedToken) -> *const UserspaceServerReg {
        self.inner.read(tok).us_reg.as_ref().map_or(core::ptr::null(), |r| r.as_ptr())
    }

    /// A new reference to that registration; `None` with none, or one whose count is zero.
    pub(crate) fn us_reg_upgrade(&self, tok: &SchedToken) -> Option<Ref<UserspaceServerReg>> {
        self.inner.read(tok).us_reg.as_ref()?.upgrade()
    }

    /// `true` iff this endpoint's receive side holds an endpoint, in a queued message or a held
    /// send. A popped slot's transfers are taken, so every slot can be read.
    pub(crate) fn holds_an_endpoint(&self, tok: &SchedToken) -> bool {
        let inner = self.inner.read(tok);
        let endpoint =
            |t: &Option<TransferRef>| t.as_ref().is_some_and(|t| t.obj.object_type() == KObjectType::IpcChannel);
        inner.recv.slots.iter().any(|s| s.transfers.iter().any(endpoint))
            || inner.pending_sends.iter().any(|p| p.transfers.iter().any(endpoint))
    }

    /// Push `msg` (and **move** any `transfers`) into the **peer**'s ring; `Sent`'s `woke_edge`
    /// reports an empty→non-empty edge. On `Full`/`PeerClosed` the `transfers` are left untouched.
    pub(crate) fn send_push(
        &self,
        tok: &mut SchedToken,
        msg: &StoredMsg,
        transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    ) -> SendOutcome {
        let Some((peer, tok)) = self.peer(tok) else { return SendOutcome::PeerClosed };
        let recv = &mut peer.inner.get(tok).recv;
        let was_empty = recv.is_empty();
        if recv.push_from(msg, transfers) { SendOutcome::Sent { woke_edge: was_empty } } else { SendOutcome::Full }
    }

    /// Inspect this endpoint's receive side without dequeuing.
    pub(crate) fn recv_peek(&self, tok: &SchedToken) -> RecvState {
        let inner = self.inner.read(tok);
        if !inner.recv.is_empty() {
            RecvState::HasMsg
        } else if inner.peer.is_none() {
            RecvState::PeerClosed
        } else {
            RecvState::Empty
        }
    }

    /// Pop the oldest message into `dst` and **move** its transferred-handle
    /// references into `out`. Returns `false` if empty.
    pub(crate) fn recv_pop_into(
        &self,
        tok: &mut SchedToken,
        dst: &mut StoredMsg,
        out: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    ) -> bool {
        self.inner.get(tok).recv.pop_into(dst, out)
    }

    /// `true` iff a recv would return something or report a closed peer — the "signaled" predicate.
    pub(crate) fn already_signaled(&self, tok: &SchedToken) -> bool {
        let inner = self.inner.read(tok);
        !inner.recv.is_empty() || inner.peer.is_none()
    }

    /// Register `thread` as a receiver. `Err(())` at [`MAX_WAITERS`](Self::MAX_WAITERS).
    pub(crate) fn add_waiter(&self, tok: &mut SchedToken, thread: *mut ()) -> Result<(), ()> {
        self.inner.get(tok).recv_waiters.add(thread)
    }

    /// Remove `thread` from the receiver set if present (idempotent).
    pub(crate) fn remove_waiter(&self, tok: &mut SchedToken, thread: *mut ()) {
        self.inner.get(tok).recv_waiters.remove(thread);
    }

    /// Take every receiver, leaving none.
    pub(crate) fn take_waiters(&self, tok: &mut SchedToken) -> Waiters<{ IpcChannel::MAX_WAITERS }> {
        self.inner.get(tok).recv_waiters.take()
    }

    /// Blocking-send entry: deliver `msg` straight into the **peer**'s ring, or if full **hold**
    /// it in the peer's pending-send queue with a cloned reference to the caller's
    /// `PendingOperation` `po`. `Sent`/`Queued` take the `transfers`; the rest leave them.
    pub(crate) fn send_or_queue(
        &self,
        tok: &mut SchedToken,
        msg: &StoredMsg,
        transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
        po: &Ref<PendingOperation>,
    ) -> BlockSendOutcome {
        let Some((peer, tok)) = self.peer(tok) else { return BlockSendOutcome::PeerClosed };
        let peer_inner = peer.inner.get(tok);
        let was_empty = peer_inner.recv.is_empty();
        if peer_inner.recv.push_from(msg, transfers) {
            return BlockSendOutcome::Sent { woke_edge: was_empty };
        }
        if peer_inner.pending_sends.len() >= Self::MAX_PENDING_SENDS {
            return BlockSendOutcome::PendingFull; // transfers left for the caller
        }
        let mut held: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
        for i in 0..IPC_HANDLE_MAX {
            held[i] = transfers[i].take();
        }
        // `po.clone()` bumps an atomic refcount: sound under `SCHED`. Reserved to
        // `MAX_PENDING_SENDS` and length-checked above, so it cannot refuse.
        let send = PendingSend { msg: *msg, transfers: held, po: po.clone(), cancelled: false };
        crate::sched::push_reserved(&mut peer_inner.pending_sends, send, "pending-send queue");
        BlockSendOutcome::Queued
    }

    /// Mark the held sender whose `PendingOperation` is `po` **cancelled** (a `BlockBounded`
    /// deadline elapsed). It stays queued — `po` was already completed `TimedOut`, and a drop must
    /// not run under `SCHED` — until `promote_pending_send` or close reclaims it.
    pub(crate) fn cancel_pending_send(&self, tok: &mut SchedToken, po: *const PendingOperation) -> bool {
        let inner = self.inner.get(tok);
        if let Some(p) = inner.pending_sends.iter_mut().find(|p| core::ptr::eq(p.po.as_ptr(), po)) {
            p.cancelled = true;
            true
        } else {
            false
        }
    }

    /// After a recv frees a slot, sweep out **cancelled** held sends (their droppable parts into
    /// `reclaimed`, for release **outside** `SCHED`), then promote the oldest **live** sender into
    /// the ring (FIFO) and return its `PendingOperation` ref: the caller completes it status 0
    /// under `SCHED` and drops it outside. `None` if no live sender was promoted.
    pub(crate) fn promote_pending_send(
        &self,
        tok: &mut SchedToken,
        reclaimed: &mut [Option<ReclaimedSend>; Self::MAX_PENDING_SENDS],
    ) -> Option<Ref<PendingOperation>> {
        let inner = self.inner.get(tok);
        let mut r = 0;
        loop {
            let cancelled = {
                let p = inner.pending_sends.first()?;
                p.cancelled
            };
            if cancelled {
                let ps = inner.pending_sends.remove(0);
                debug_assert!(r < reclaimed.len());
                reclaimed[r] = Some(ReclaimedSend { po: ps.po, transfers: ps.transfers });
                r += 1;
                continue;
            }
            if inner.recv.is_full() {
                return None; // no freed slot to promote into
            }
            let mut ps = inner.pending_sends.remove(0);
            let pushed = inner.recv.push_from(&ps.msg, &mut ps.transfers);
            debug_assert!(pushed, "promote into a non-full ring must succeed");
            return Some(ps.po);
        }
    }

    /// A reference to every held sender's `PendingOperation`, for the closing path to complete them
    /// `PeerClosed` and drop outside `SCHED`. The sends stay queued, released with the endpoint.
    pub(crate) fn held_sends(&self, tok: &SchedToken) -> [Option<Ref<PendingOperation>>; Self::MAX_PENDING_SENDS] {
        let held = &self.inner.read(tok).pending_sends;
        core::array::from_fn(|i| held.get(i).map(|p| p.po.clone()))
    }
}

impl Drop for IpcChannel {
    /// A live waiter pins this endpoint through its `ObjectRef`, so none is left at the last drop;
    /// and its closing, run first, unlinked it.
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        debug_assert!(inner.recv_waiters.is_empty(), "IpcChannel dropped with live receivers");
        debug_assert!(inner.peer.is_none() && inner.us_reg.is_none(), "IpcChannel dropped still linked");
    }
}

#[cfg(test)]
mod tests;
