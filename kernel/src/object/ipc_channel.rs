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
mod tests {
    use super::*;
    use crate::libkern::ipc::IPC_PAYLOAD_SIZE;
    use crate::mm::test_support::init_global_heap;
    use crate::object::ObjectRef;
    use crate::object::header::test_probe;

    fn new_pair() -> (*mut (), *mut ()) {
        let (a, b) = IpcChannel::try_new_pair(4).unwrap();
        (KBox::into_raw(a).as_ptr() as *mut (), KBox::into_raw(b).as_ptr() as *mut ())
    }

    fn drop_endpoint(obj: *mut ()) {
        // SAFETY: adopts the endpoint's creation reference, to drop it.
        drop(unsafe { ObjectRef::from_raw(obj, KObjectType::IpcChannel) });
    }

    fn a_msg(byte: u8, len: u32) -> StoredMsg {
        let mut m = StoredMsg::zeroed();
        m.header.payload_len = len;
        for i in 0..(len as usize) {
            m.payload[i] = byte;
        }
        m
    }

    /// `send_push` with no transferred handles (the common test case).
    ///
    /// # Safety
    ///
    /// As `IpcChannel::send_push`.
    unsafe fn push_no_xfer(obj: *mut (), msg: &StoredMsg) -> SendOutcome {
        let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
        // SAFETY: this function's contract, forwarded.
        unsafe { IpcChannel::send_push(obj, msg, &mut t) }
    }

    /// `recv_pop_into` discarding any (absent) transfers.
    ///
    /// # Safety
    ///
    /// As `IpcChannel::recv_pop_into`.
    unsafe fn pop_no_xfer(obj: *mut (), dst: &mut StoredMsg) -> bool {
        let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
        // SAFETY: this function's contract, forwarded.
        unsafe { IpcChannel::recv_pop_into(obj, dst, &mut t) }
    }

    #[test]
    fn pair_has_mutual_peers() {
        init_global_heap();
        let (a, b) = new_pair();
        // SAFETY: live endpoints, single-threaded test stands in for SCHED.
        unsafe {
            assert_eq!(IpcChannel::peer_of(a), b);
            assert_eq!(IpcChannel::peer_of(b), a);
        }
        drop_endpoint(a);
        drop_endpoint(b);
    }

    #[test]
    fn send_routes_to_peer_inbox_recv_drains_fifo() {
        init_global_heap();
        let (a, b) = new_pair();
        // SAFETY: live endpoints, single-threaded test.
        unsafe {
            assert!(!IpcChannel::already_signaled(a));
            let edge = push_no_xfer(a, &a_msg(0x11, 3));
            assert_eq!(edge, SendOutcome::Sent { woke_edge: true });
            let edge2 = push_no_xfer(a, &a_msg(0x22, 5));
            assert_eq!(edge2, SendOutcome::Sent { woke_edge: false });
            assert!(!IpcChannel::already_signaled(a));
            assert!(IpcChannel::already_signaled(b));
            assert_eq!(IpcChannel::recv_peek(b), RecvState::HasMsg);
            let mut got = StoredMsg::zeroed();
            assert!(pop_no_xfer(b, &mut got));
            assert_eq!(got.header.payload_len, 3);
            assert_eq!(got.payload[0], 0x11);
            assert!(pop_no_xfer(b, &mut got));
            assert_eq!(got.header.payload_len, 5);
            assert_eq!(got.payload[0], 0x22);
            assert!(!pop_no_xfer(b, &mut got));
            assert_eq!(IpcChannel::recv_peek(b), RecvState::Empty);
        }
        drop_endpoint(a);
        drop_endpoint(b);
    }

    #[test]
    fn send_full_returns_full() {
        init_global_heap();
        let (a, b) = new_pair(); // depth 4
        // SAFETY: live endpoints, single-threaded test.
        unsafe {
            for _ in 0..4 {
                assert!(matches!(push_no_xfer(a, &a_msg(0xAB, 1)), SendOutcome::Sent { .. }));
            }
            assert_eq!(push_no_xfer(a, &a_msg(0xAB, 1)), SendOutcome::Full);
        }
        drop_endpoint(a);
        drop_endpoint(b);
    }

    #[test]
    fn ring_wraps_around() {
        init_global_heap();
        let (a, b) = new_pair(); // depth 4
        // SAFETY: live endpoints, single-threaded test.
        unsafe {
            let mut got = StoredMsg::zeroed();
            for i in 0..3u8 {
                push_no_xfer(a, &a_msg(i, 1));
            }
            for i in 0..2u8 {
                assert!(pop_no_xfer(b, &mut got));
                assert_eq!(got.payload[0], i);
            }
            for i in 3..6u8 {
                assert!(matches!(push_no_xfer(a, &a_msg(i, 1)), SendOutcome::Sent { .. }));
            }
            for i in 2..6u8 {
                assert!(pop_no_xfer(b, &mut got));
                assert_eq!(got.payload[0], i);
            }
            assert!(!pop_no_xfer(b, &mut got));
        }
        drop_endpoint(a);
        drop_endpoint(b);
    }

    #[test]
    fn closing_one_end_marks_peer_closed() {
        init_global_heap();
        let (a, b) = new_pair();
        // SAFETY: live endpoints, single-threaded test.
        unsafe {
            push_no_xfer(a, &a_msg(0x7E, 2));
        }
        drop_endpoint(a);
        // SAFETY: b is still live.
        unsafe {
            assert!(IpcChannel::peer_of(b).is_null());
            assert_eq!(IpcChannel::recv_peek(b), RecvState::HasMsg);
            let mut got = StoredMsg::zeroed();
            assert!(pop_no_xfer(b, &mut got));
            assert_eq!(got.payload[0], 0x7E);
            assert_eq!(IpcChannel::recv_peek(b), RecvState::PeerClosed);
            assert!(IpcChannel::already_signaled(b));
            assert_eq!(push_no_xfer(b, &a_msg(0, 1)), SendOutcome::PeerClosed);
        }
        drop_endpoint(b);
    }

    #[test]
    fn waiters_add_remove_take_caps_at_max() {
        init_global_heap();
        let (a, b) = new_pair();
        // SAFETY: live endpoints, single-threaded test.
        unsafe {
            let ths: [*mut (); IpcChannel::MAX_WAITERS] = core::array::from_fn(|i| (0x1000 + i) as *mut ());
            for &t in &ths {
                assert!(IpcChannel::add_waiter(a, t).is_ok());
            }
            assert!(IpcChannel::add_waiter(a, 0xDEAD as *mut ()).is_err());
            IpcChannel::remove_waiter(a, ths[0]);
            assert!(IpcChannel::add_waiter(a, 0xBEEF as *mut ()).is_ok());
            let mut buf = [core::ptr::null_mut(); IpcChannel::MAX_WAITERS];
            assert_eq!(IpcChannel::take_waiters(a, &mut buf), IpcChannel::MAX_WAITERS);
        }
        drop_endpoint(a);
        drop_endpoint(b);
    }

    #[test]
    fn dispatch_destroy_runs_ipc_arm() {
        init_global_heap();
        test_probe::reset();
        let (a, b) = new_pair();
        assert_eq!(test_probe::ipc_channel_destroys(), 0);
        drop_endpoint(a);
        drop_endpoint(b);
        assert_eq!(test_probe::ipc_channel_destroys(), 2);
    }

    #[test]
    fn transfer_moves_with_message_and_releases_on_drop() {
        use crate::object::Process;
        init_global_heap();
        test_probe::reset();
        let (a, b) = new_pair();
        let p = KBox::into_raw(Process::try_new(99).unwrap()).as_ptr() as *mut ();
        // SAFETY: adopts the process's creation reference.
        let pref = unsafe { ObjectRef::from_raw(p, KObjectType::Process) };
        // SAFETY: live endpoints, single-threaded test.
        unsafe {
            let mut send_t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
            send_t[0] = Some(TransferRef { obj: pref, rights: Rights::empty() });
            let mut msg = a_msg(0, 0);
            msg.header.handle_count = 1;
            assert!(matches!(IpcChannel::send_push(a, &msg, &mut send_t), SendOutcome::Sent { .. }));
            assert!(send_t[0].is_none(), "transfer moved into the queue");
            let mut got = StoredMsg::zeroed();
            let mut recv_t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
            assert!(IpcChannel::recv_pop_into(b, &mut got, &mut recv_t));
            assert_eq!(got.header.handle_count, 1);
            let tr = recv_t[0].take().unwrap();
            assert_eq!(tr.obj.as_ptr(), p);
            assert_eq!(test_probe::process_destroys(), 0);
            drop(tr);
            assert_eq!(test_probe::process_destroys(), 1);
        }
        drop_endpoint(a);
        drop_endpoint(b);
    }

    #[test]
    fn undelivered_transfer_released_when_endpoint_destroyed() {
        use crate::object::Process;
        init_global_heap();
        test_probe::reset();
        let (a, b) = new_pair();
        let p = KBox::into_raw(Process::try_new(7).unwrap()).as_ptr() as *mut ();
        // SAFETY: adopts the process's creation reference.
        let pref = unsafe { ObjectRef::from_raw(p, KObjectType::Process) };
        // SAFETY: live endpoints, single-threaded test.
        unsafe {
            let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
            t[0] = Some(TransferRef { obj: pref, rights: Rights::empty() });
            let mut msg = a_msg(0, 0);
            msg.header.handle_count = 1;
            IpcChannel::send_push(a, &msg, &mut t);
        }
        assert_eq!(test_probe::process_destroys(), 0);
        drop_endpoint(a);
        drop_endpoint(b);
        assert_eq!(test_probe::process_destroys(), 1);
    }

    #[test]
    fn payload_size_is_one_page_worth() {
        assert_eq!(IPC_PAYLOAD_SIZE, 4008);
        assert_eq!(core::mem::size_of::<StoredMsg>(), 4096);
    }

    fn make_po() -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(crate::object::PendingOperation::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::PendingOperation,
            )
        }
    }

    fn empty_xfer() -> [Option<TransferRef>; IPC_HANDLE_MAX] {
        core::array::from_fn(|_| None)
    }

    fn empty_reclaim() -> [Option<ReclaimedSend>; IpcChannel::MAX_PENDING_SENDS] {
        core::array::from_fn(|_| None)
    }

    #[test]
    fn blocking_send_to_ring_with_space_delivers_immediately() {
        init_global_heap();
        let (a, b) = new_pair();
        let po = make_po();
        // SAFETY: live endpoints/PO; single-threaded test stands in for SCHED.
        unsafe {
            let mut t = empty_xfer();
            assert_eq!(
                IpcChannel::send_or_queue(a, &a_msg(7, 1), &mut t, &po),
                BlockSendOutcome::Sent { woke_edge: true }
            );
            let mut d = a_msg(0, 0);
            assert!(pop_no_xfer(b, &mut d));
            assert_eq!(d.payload[0], 7);
            assert!(IpcChannel::promote_pending_send(b, &mut empty_reclaim()).is_none());
        }
        drop(po);
        drop_endpoint(a);
        drop_endpoint(b);
    }

    #[test]
    fn blocking_send_to_full_ring_queues_then_recv_promotes_fifo() {
        init_global_heap();
        test_probe::reset();
        let (a, b) = new_pair(); // cap 4; sending on a fills b's ring
        let po = make_po();
        // SAFETY: live endpoints/PO; single-threaded test stands in for SCHED.
        unsafe {
            for i in 0..4u8 {
                assert!(matches!(push_no_xfer(a, &a_msg(i, 1)), SendOutcome::Sent { .. }));
            }
            let mut t = empty_xfer();
            assert_eq!(IpcChannel::send_or_queue(a, &a_msg(99, 1), &mut t, &po), BlockSendOutcome::Queued);
            let mut d = a_msg(0, 0);
            assert!(pop_no_xfer(b, &mut d));
            assert_eq!(d.payload[0], 0);
            let promoted = IpcChannel::promote_pending_send(b, &mut empty_reclaim());
            assert!(promoted.is_some());
            for expect in [1u8, 2, 3, 99] {
                let mut dd = a_msg(0, 0);
                assert!(pop_no_xfer(b, &mut dd));
                assert_eq!(dd.payload[0], expect, "FIFO order");
            }
            assert!(IpcChannel::promote_pending_send(b, &mut empty_reclaim()).is_none());
            drop(promoted); // the queue's PO reference (clone)
        }
        drop(po); // creation reference → last ref, PO destroyed
        assert_eq!(test_probe::pending_op_destroys(), 1);
        drop_endpoint(a);
        drop_endpoint(b);
    }

    #[test]
    fn closing_receiver_releases_a_held_pending_send() {
        init_global_heap();
        test_probe::reset();
        let (a, b) = new_pair();
        let po = make_po();
        // SAFETY: live endpoints/PO; single-threaded test stands in for SCHED.
        unsafe {
            for i in 0..4u8 {
                push_no_xfer(a, &a_msg(i, 1));
            }
            let mut t = empty_xfer();
            assert_eq!(IpcChannel::send_or_queue(a, &a_msg(99, 1), &mut t, &po), BlockSendOutcome::Queued);
            let mut pos = [core::ptr::null_mut(); IpcChannel::MAX_PENDING_SENDS];
            assert_eq!(IpcChannel::pending_send_pos(b, &mut pos), 1);
            assert_eq!(pos[0], po.as_ptr());
        }
        drop_endpoint(b);
        assert_eq!(test_probe::pending_op_destroys(), 0, "creation ref still held");
        drop(po); // last ref → PO destroyed
        assert_eq!(test_probe::pending_op_destroys(), 1);
        drop_endpoint(a);
    }

    #[test]
    fn cancel_marks_the_matching_held_send() {
        init_global_heap();
        let (a, b) = new_pair();
        let po = make_po();
        // SAFETY: live endpoints/PO; single-threaded test stands in for SCHED.
        unsafe {
            for i in 0..4u8 {
                push_no_xfer(a, &a_msg(i, 1));
            }
            let mut t = empty_xfer();
            assert_eq!(IpcChannel::send_or_queue(a, &a_msg(99, 1), &mut t, &po), BlockSendOutcome::Queued);
            assert!(!IpcChannel::cancel_pending_send(b, 0xDEAD as *mut ()));
            assert!(IpcChannel::cancel_pending_send(b, po.as_ptr()));
        }
        drop(po);
        drop_endpoint(a);
        drop_endpoint(b);
    }

    #[test]
    fn recv_sweeps_cancelled_then_promotes_the_live_send() {
        init_global_heap();
        test_probe::reset();
        let (a, b) = new_pair();
        let po1 = make_po(); // will be cancelled (timed out)
        let po2 = make_po(); // will be delivered
        // SAFETY: live endpoints/POs; single-threaded test stands in for SCHED.
        unsafe {
            for i in 0..4u8 {
                push_no_xfer(a, &a_msg(i, 1));
            }
            let mut t1 = empty_xfer();
            assert_eq!(IpcChannel::send_or_queue(a, &a_msg(50, 1), &mut t1, &po1), BlockSendOutcome::Queued);
            let mut t2 = empty_xfer();
            assert_eq!(IpcChannel::send_or_queue(a, &a_msg(51, 1), &mut t2, &po2), BlockSendOutcome::Queued);
            assert!(IpcChannel::cancel_pending_send(b, po1.as_ptr()));
            let mut d = a_msg(0, 0);
            assert!(pop_no_xfer(b, &mut d));
            let mut rc = empty_reclaim();
            let promoted = IpcChannel::promote_pending_send(b, &mut rc);
            assert_eq!(promoted.as_ref().map(|p| p.as_ptr()), Some(po2.as_ptr()));
            assert_eq!(rc[0].as_ref().map(|r| r.po.as_ptr()), Some(po1.as_ptr()));
            assert!(rc[1].is_none(), "only the one cancelled send was swept");
            drop(promoted); // release po2's queue ref
            drop(rc); // release po1's queue ref (the reclaimed entry)
        }
        drop(po1);
        drop(po2);
        assert_eq!(test_probe::pending_op_destroys(), 2);
        drop_endpoint(a);
        drop_endpoint(b);
    }
}
