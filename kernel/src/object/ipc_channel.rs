//! The [`IpcChannel`] kernel object — one **endpoint** of a bidirectional IPC channel.
//!
//! A channel is a **pair** of endpoint objects, each with its own receive ring and recv-waiter
//! list, linked by a mutual raw `peer` pointer: a send pushes into the *peer's* ring, a recv
//! drains the endpoint's *own*. See `docs/spec/ipc-message-format.md`.
//!
//! All interior state lives in an [`UnsafeCell`] touched **only while the rank-1 `SCHED` lock
//! is held** (see `kernel/docs/lock-ordering.md`); the `pub(crate) unsafe fn` accessors take a
//! type-erased `*mut ()`, so no aliasing `&mut IpcChannel` is formed.
//!
//! When an endpoint's last handle closes, [`IpcChannel::drop`] runs
//! [`crate::sched::ipc_endpoint_closing`]: under `SCHED` it nulls the *surviving* peer's `peer`
//! pointer and wakes its blocked receivers (they observe `PeerClosed`). The survivor is alive
//! (pinned by its handle or a waiter); the second endpoint to drop reads its own `peer` as
//! already-null and skips. **Invariant:** endpoint references are released only in
//! syscall/boot context, **never under `SCHED`**, so `drop` may itself take `SCHED`.

use crate::sched::{SlotHold, Slots};
use core::cell::UnsafeCell;

use crate::libkern::handle::{KObjectType, Rights};
use crate::libkern::ipc::{IPC_HANDLE_MAX, IPC_MAX_QUEUE_DEPTH, IPC_MSG_SIZE, IPC_PAYLOAD_SIZE, IpcMsgHeader};
use crate::libkern::{AllocError, KBox, KVec};
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;

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

const _: () = assert!(core::mem::size_of::<StoredMsg>() == IPC_MSG_SIZE);
// `IpcMsg` and `StoredMsg` share field offsets (`IpcMsg`'s page alignment adds no padding).
const _: () = assert!(core::mem::offset_of!(StoredMsg, payload) == 24);

impl StoredMsg {
    /// An all-zero message (a valid bit pattern).
    pub fn zeroed() -> Self {
        // SAFETY: `StoredMsg` is `#[repr(C)]` over `u32`/`u8`/`u16`/`u64`
        // (arrays included); the all-zero bit pattern is a valid value.
        unsafe { core::mem::zeroed() }
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
        // SAFETY: `#[repr(C)]`, size 4096 (asserted), no interior padding (the
        // three regions tile the size exactly — see `libkern::ipc`), every byte
        // initialised, so reinterpreting as `[u8; 4096]` exposes only defined bytes.
        unsafe { &*(self as *const Self as *const [u8; IPC_MSG_SIZE]) }
    }

    /// The 4096 wire bytes, mutably, for `copy_slice_from_user`.
    pub fn as_bytes_mut(&mut self) -> &mut [u8; IPC_MSG_SIZE] {
        // SAFETY: as [`as_bytes`](Self::as_bytes); `&mut self` proves exclusivity.
        unsafe { &mut *(self as *mut Self as *mut [u8; IPC_MSG_SIZE]) }
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
    po: ObjectRef,
    /// Set when a `BlockBounded` deadline elapsed: `po` was already completed `TimedOut`, so the
    /// entry is swept out at the next `promote_pending_send` (or at close), not delivered.
    cancelled: bool,
}

/// The droppable parts of a cancelled [`PendingSend`], handed back by
/// [`promote_pending_send`](IpcChannel::promote_pending_send) for release **outside** `SCHED`:
/// their `Drop`s may take lower-rank locks (the buddy allocator) that must not nest under it.
pub struct ReclaimedSend {
    pub po: ObjectRef,
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
    header: KObjectHeader,
    magic: u64,
    inner: UnsafeCell<Inner>,
    /// Its sends' deadline-heap slots, given back after `Drop` takes their entries out.
    _slots: SlotHold,
}

struct Inner {
    recv: MsgRing,
    /// Threads blocked recv-ing (type-erased, non-owning; removed before a waiter unparks).
    /// Pre-reserved to [`IpcChannel::MAX_WAITERS`] so `add_waiter` never allocates under `SCHED`.
    recv_waiters: KVec<*mut ()>,
    /// Blocking senders awaiting space in **this** ring, FIFO, promoted into `recv` when a recv
    /// frees a slot. Pre-reserved to [`IpcChannel::MAX_PENDING_SENDS`]: no `SCHED` allocation.
    pending_sends: KVec<PendingSend>,
    peer: *mut IpcChannel,
    /// Non-null iff this is the **kernel's** end of a Userspace Server channel: a non-owning
    /// back-pointer to the owning [`UserspaceServerReg`](crate::object::UserspaceServerReg), which
    /// *owns* this endpoint. A reply sent here (by the server, on its peer) is a forwarded-lookup
    /// reply the kernel completes inline rather than enqueues.
    us_reg: *mut (),
}

// SAFETY: identical reasoning to `Timer`/`NotificationChannel` — the header
// refcount is atomic and every access to `inner` (including the raw `peer`
// pointer) is serialised under the single-CPU `SCHED` lock.
unsafe impl Send for IpcChannel {}
// SAFETY: as `Send`.
unsafe impl Sync for IpcChannel {}

impl IpcChannel {
    /// Sentinel written into [`IpcChannel::magic`] at construction.
    pub const MAGIC: u64 = 0x49_70_63_43_68_21_21_21; // "IpcCh!!!"

    /// Maximum simultaneous receivers blocked on one endpoint; bounds the pre-reserved vector.
    pub const MAX_WAITERS: usize = 4;

    /// Maximum messages in one endpoint's pending-send queue; bounds the pre-reserved vector,
    /// beyond which a blocking send gets `WouldBlock` back-pressure.
    pub const MAX_PENDING_SENDS: usize = 4;

    /// Create a connected endpoint **pair**, each with a `depth`-slot receive ring (clamped to
    /// `1..=IPC_MAX_QUEUE_DEPTH`). Each returned `KBox` holds one creation reference.
    pub fn try_new_pair(depth: u32) -> Result<(KBox<Self>, KBox<Self>), AllocError> {
        let cap = depth.clamp(1, IPC_MAX_QUEUE_DEPTH) as usize;
        let mut a = Self::try_new_endpoint(cap)?;
        let mut b = Self::try_new_endpoint(cap)?;
        // Both boxes are still private here: plain writes are sound; no `SCHED` needed.
        let a_ptr = (&*a as *const Self) as *mut Self;
        let b_ptr = (&*b as *const Self) as *mut Self;
        a.inner.get_mut().peer = b_ptr;
        b.inner.get_mut().peer = a_ptr;
        Ok((a, b))
    }

    /// Allocate one unconnected endpoint, refcount one. Ring and waiter vector are reserved up
    /// front (the only fallible growth), so later pushes never allocate under `SCHED`.
    fn try_new_endpoint(cap: usize) -> Result<KBox<Self>, AllocError> {
        let slots = SlotHold::reserve(Slots::Endpoint)?;
        let recv = MsgRing::with_capacity(cap)?;
        let mut recv_waiters: KVec<*mut ()> = KVec::new();
        recv_waiters.try_reserve(Self::MAX_WAITERS)?;
        let mut pending_sends: KVec<PendingSend> = KVec::new();
        pending_sends.try_reserve(Self::MAX_PENDING_SENDS)?;
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::IpcChannel),
            magic: Self::MAGIC,
            inner: UnsafeCell::new(Inner {
                recv,
                recv_waiters,
                pending_sends,
                peer: core::ptr::null_mut(),
                us_reg: core::ptr::null_mut(),
            }),
            _slots: slots,
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    // --- Scheduler-only accessors --------------------------------------
    //
    // SAFETY (shared by all): `obj` addresses a live `IpcChannel` (pinned by an
    // `ObjectRef` the caller holds), and the caller holds `SCHED`, which —
    // single-CPU — serialises all access to `inner`.

    /// Borrow the interior mutably (no aliasing; `SCHED` held).
    ///
    /// # Safety
    /// See the accessor contract above.
    #[allow(clippy::mut_from_ref, reason = "this accessor's contract makes the `&mut` the only reference")]
    unsafe fn inner<'a>(obj: *mut ()) -> &'a mut Inner {
        // SAFETY: forming a shared `&IpcChannel` to reach the `UnsafeCell`, then
        // a `&mut Inner` through it, is the interior-mutability contract — sound
        // while `SCHED` serialises access.
        let c = unsafe { &*(obj as *const IpcChannel) };
        // SAFETY: as above.
        unsafe { &mut *c.inner.get() }
    }

    /// The peer endpoint pointer (type-erased), or null if the peer has closed.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn peer_of(obj: *mut ()) -> *mut () {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.peer as *mut ()
    }

    /// Null this endpoint's peer pointer (the peer is closing).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn clear_peer(obj: *mut ()) {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.peer = core::ptr::null_mut();
    }

    /// Mark this endpoint as the kernel's end of a Userspace Server channel, recording `reg`.
    /// Set once, when a supervisor binds the endpoint as a `UserspaceServer`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_us_reg(obj: *mut (), reg: *mut ()) {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.us_reg = reg;
    }

    /// `true` iff this endpoint's receive side holds an endpoint, in a queued message or a held
    /// send. A popped slot's transfers are taken, so every slot can be read.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn holds_an_endpoint(obj: *mut ()) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        let endpoint =
            |t: &Option<TransferRef>| t.as_ref().is_some_and(|t| t.obj.object_type() == KObjectType::IpcChannel);
        inner.recv.slots.iter().any(|s| s.transfers.iter().any(endpoint))
            || inner.pending_sends.iter().any(|p| p.transfers.iter().any(endpoint))
    }

    /// The owning [`UserspaceServerReg`](crate::object::UserspaceServerReg)
    /// back-pointer for this endpoint (type-erased), or null if this is an ordinary
    /// channel endpoint.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn us_reg_of(obj: *mut ()) -> *mut () {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.us_reg
    }

    /// Push `msg` (and **move** any `transfers`) into the **peer**'s ring; `Sent`'s `woke_edge`
    /// reports an empty→non-empty edge. On `Full`/`PeerClosed` the `transfers` are left untouched.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn send_push(
        obj: *mut (),
        msg: &StoredMsg,
        transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    ) -> SendOutcome {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let peer = unsafe { Self::inner(obj) }.peer;
        if peer.is_null() {
            return SendOutcome::PeerClosed;
        }
        // SAFETY: the accessor contract, for `peer`: the other endpoint, live while its pointer is
        // set, which closing clears under `SCHED`; a distinct object from `obj`, whose borrow has ended.
        let peer_inner = unsafe { Self::inner(peer as *mut ()) };
        let was_empty = peer_inner.recv.is_empty();
        if peer_inner.recv.push_from(msg, transfers) {
            SendOutcome::Sent { woke_edge: was_empty }
        } else {
            SendOutcome::Full
        }
    }

    /// Inspect this endpoint's receive side without dequeuing.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn recv_peek(obj: *mut ()) -> RecvState {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if !inner.recv.is_empty() {
            RecvState::HasMsg
        } else if inner.peer.is_null() {
            RecvState::PeerClosed
        } else {
            RecvState::Empty
        }
    }

    /// Pop the oldest message into `dst` and **move** its transferred-handle
    /// references into `out`. Returns `false` if empty.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn recv_pop_into(
        obj: *mut (),
        dst: &mut StoredMsg,
        out: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    ) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.recv.pop_into(dst, out)
    }

    /// `true` iff a recv would return something or report a closed peer — the "signaled" predicate.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn already_signaled(obj: *mut ()) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        !inner.recv.is_empty() || inner.peer.is_null()
    }

    /// Register `thread` as a receiver. `Err(())` at [`MAX_WAITERS`](Self::MAX_WAITERS).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn add_waiter(obj: *mut (), thread: *mut ()) -> Result<(), ()> {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if inner.recv_waiters.len() < Self::MAX_WAITERS {
            inner
                .recv_waiters
                // `push_within_capacity`, not `try_push`: this runs under `SCHED` and `try_push`
                // grows (`kmalloc` under the rank-1 lock). Construction reserves `MAX_WAITERS`, so
                // it cannot fail; the type should say so, since a new waiter list copies this
                // shape.
                .push_within_capacity(thread)
                .expect("within reserved waiter capacity");
            Ok(())
        } else {
            Err(())
        }
    }

    /// Remove `thread` from the receiver set if present (idempotent).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn remove_waiter(obj: *mut (), thread: *mut ()) {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if let Some(i) = inner.recv_waiters.iter().position(|&w| w == thread) {
            inner.recv_waiters.remove(i);
        }
    }

    /// Drain every receiver into `out`, returning the count; `out` is at least `MAX_WAITERS` long.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn take_waiters(obj: *mut (), out: &mut [*mut ()]) -> usize {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        let n = inner.recv_waiters.len();
        debug_assert!(out.len() >= n);
        for (i, &w) in inner.recv_waiters.iter().enumerate() {
            out[i] = w;
        }
        inner.recv_waiters.clear();
        n
    }

    /// Blocking-send entry: deliver `msg` straight into the **peer**'s ring, or if full **hold**
    /// it in the peer's pending-send queue with a cloned reference to the caller's
    /// `PendingOperation` `po`. `Sent`/`Queued` take the `transfers`; the rest leave them.
    ///
    /// # Safety
    /// See the accessor contract above; `po` references a live `PendingOperation`.
    pub(crate) unsafe fn send_or_queue(
        obj: *mut (),
        msg: &StoredMsg,
        transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
        po: &ObjectRef,
    ) -> BlockSendOutcome {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let peer = unsafe { Self::inner(obj) }.peer;
        if peer.is_null() {
            return BlockSendOutcome::PeerClosed;
        }
        // SAFETY: the accessor contract, for `peer`: the other endpoint, live while its pointer is
        // set, which closing clears under `SCHED`; a distinct object from `obj`, whose borrow has ended.
        let peer_inner = unsafe { Self::inner(peer as *mut ()) };
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
        // `po.clone()` bumps an atomic refcount: sound under `SCHED`. Non-growing: `try_push` drops
        // its argument when growth fails, and its `ObjectRef` would drop under `SCHED`. Reserved to
        // `MAX_PENDING_SENDS` and length-checked above, so it cannot refuse.
        peer_inner
            .pending_sends
            .push_within_capacity(PendingSend { msg: *msg, transfers: held, po: po.clone(), cancelled: false })
            .unwrap_or_else(|held| {
                // Unreachable: reserved at construction and length-checked above. Do not let
                // `held` drop here — it owns an `ObjectRef` and `SCHED` is held.
                core::mem::forget(held);
                panic!("pending-send queue: reserve exhausted despite the length check above");
            });
        BlockSendOutcome::Queued
    }

    /// Mark the held sender whose `PendingOperation` is `po` **cancelled** (a `BlockBounded`
    /// deadline elapsed). It stays queued — `po` was already completed `TimedOut`, and a drop must
    /// not run under `SCHED` — until `promote_pending_send` or close reclaims it.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn cancel_pending_send(obj: *mut (), po: *mut ()) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if let Some(p) = inner.pending_sends.iter_mut().find(|p| p.po.as_ptr() == po) {
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
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn promote_pending_send(
        obj: *mut (),
        reclaimed: &mut [Option<ReclaimedSend>; Self::MAX_PENDING_SENDS],
    ) -> Option<ObjectRef> {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
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

    /// Copy every held sender's `PendingOperation` pointer into `out` (for the closing path to
    /// complete them `PeerClosed`), returning the count. They stay queued, released with `Inner`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn pending_send_pos(obj: *mut (), out: &mut [*mut ()]) -> usize {
        // SAFETY: the accessor contract: `obj` is a live `IpcChannel` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        let mut n = 0;
        for p in inner.pending_sends.iter() {
            if n < out.len() {
                out[n] = p.po.as_ptr();
                n += 1;
            }
        }
        n
    }
}

impl Drop for IpcChannel {
    /// Unlink from the peer and wake its blocked receivers (they observe `PeerClosed`), then assert
    /// this endpoint has no receivers of its own (a live waiter pins it via its `ObjectRef`).
    fn drop(&mut self) {
        // SAFETY: `self` is being dropped: its memory whole, its last reference gone.
        unsafe { crate::sched::ipc_endpoint_closing(self as *mut Self as *mut ()) };
        debug_assert!(self.inner.get_mut().recv_waiters.is_empty(), "IpcChannel dropped with live receivers");
    }
}

#[cfg(test)]
mod tests;
