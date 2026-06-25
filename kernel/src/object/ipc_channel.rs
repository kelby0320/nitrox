//! The [`IpcChannel`] kernel object — one **endpoint** of a bidirectional IPC
//! channel.
//!
//! A channel is a **pair** of `IpcChannel` endpoint objects (both
//! [`KObjectType::IpcChannel`]), each owning its own receive ring and
//! recv-waiter list, linked by a mutual raw `peer` pointer. A send on one
//! endpoint pushes into the *peer*'s receive ring; a recv drains the endpoint's
//! *own* ring. (`docs/spec/ipc-message-format.md` describes "two endpoint
//! handles, separate queues per direction" — this is that, with one kobject per
//! endpoint. A single shared object can't be used because a handle→object
//! pointer carries no per-handle tag to tell the two ends apart for the
//! asymmetric routing.)
//!
//! ## Mutation discipline
//!
//! Exactly like [`Timer`](crate::object::Timer) /
//! [`NotificationChannel`](crate::object::NotificationChannel): all interior
//! state lives in an [`UnsafeCell`] touched **only while the rank-1 `SCHED`
//! lock is held** (single-CPU serialisation; see `kernel/docs/lock-ordering.md`).
//! The `pub(crate) unsafe fn` accessors take a type-erased `*mut ()` and reach
//! the interior through that cell — no aliasing `&mut IpcChannel` is formed.
//!
//! ## Dead-peer / close
//!
//! When an endpoint's last handle closes, its refcount hits zero and
//! [`IpcChannel::drop`] runs [`crate::sched::ipc_endpoint_closing`]: under
//! `SCHED` it nulls the *surviving* peer's `peer` pointer and wakes the peer's
//! blocked receivers (so they observe `PeerClosed`). The survivor is alive
//! (pinned by its own handle / a waiter's `ObjectRef`); the second endpoint to
//! drop reads its own `peer` as already-null and skips — no use-after-free, safe
//! under single-CPU `SCHED` serialisation.
//!
//! **Invariant:** `IpcChannel` endpoint references are released only in
//! syscall/boot context (handle close, the `sys_wait`/lookup `ObjectRef`s),
//! **never under `SCHED`** — so `drop` may itself take `SCHED`.

use core::cell::UnsafeCell;

use crate::libkern::handle::{KObjectType, Rights};
use crate::libkern::ipc::{
    IPC_HANDLE_MAX, IPC_MAX_QUEUE_DEPTH, IPC_MSG_SIZE, IPC_PAYLOAD_SIZE, IpcMsgHeader,
};
use crate::libkern::{AllocError, KBox, KVec};
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;

/// A queued message in kernel storage: the byte-identical, natural-alignment
/// twin of [`IpcMsg`](crate::libkern::IpcMsg) (no page alignment — avoids
/// over-aligned heap allocations for the queue slots). Field offsets match the
/// wire `IpcMsg` exactly, so a byte view round-trips through `copy_*_user`.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct StoredMsg {
    /// The 24-byte header (kernel stamps `sender_pid` / `timestamp` at send).
    pub header: IpcMsgHeader,
    /// Inline payload bytes.
    pub payload: [u8; IPC_PAYLOAD_SIZE],
    /// Transferable-handle slots (unused until handle transfer lands; zeroed).
    pub handles: [u64; crate::libkern::ipc::IPC_HANDLE_MAX],
}

const _: () = assert!(core::mem::size_of::<StoredMsg>() == IPC_MSG_SIZE);
// `IpcMsg` and `StoredMsg` share field offsets (the only difference is `IpcMsg`'s
// page alignment, which adds no interior padding); the byte view below is sound.
const _: () = assert!(core::mem::offset_of!(StoredMsg, payload) == 24);

impl StoredMsg {
    /// An all-zero message (a valid bit pattern: every field is an integer or
    /// integer array).
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

/// One handle moved through IPC, held by the kernel while the message is queued
/// ("in flight"): the object reference (pinning it) plus the rights to install it
/// with in the receiver's table (captured from the sender's handle at send). The
/// receiver consumes this at `sys_channel_recv`; an undelivered transfer's ref is
/// released when the queue (the endpoint) is destroyed.
pub struct TransferRef {
    /// The pinned transferred object.
    pub obj: ObjectRef,
    /// Rights to install the receiver's new handle with (the sender's, by move).
    pub rights: Rights,
}

/// A queued message slot: the wire bytes plus the in-flight transferred-handle
/// references (`None` per slot for a no-handle message). Not `Copy` — the
/// `TransferRef`s are **moved** in/out, never bytewise-copied.
struct RingSlot {
    msg: StoredMsg,
    transfers: [Option<TransferRef>; IPC_HANDLE_MAX],
}

impl RingSlot {
    fn empty() -> Self {
        RingSlot {
            msg: StoredMsg::zeroed(),
            transfers: core::array::from_fn(|_| None),
        }
    }
}

/// A fixed-capacity ring of message slots. The backing [`KVec`] is pre-filled
/// with `cap` empty slots at construction (the only fallible growth);
/// `push_from`/`pop_into` then move messages (bytes copied, transfer references
/// moved) in place — no element shifting, no reallocation, no large stack
/// temporaries — so they are safe to run under `SCHED`. (No `ObjectRef` *Drop*
/// ever runs here; only moves, which never touch a refcount.)
struct MsgRing {
    /// Exactly `cap` storage slots; `slots.len()` is the capacity.
    slots: KVec<RingSlot>,
    /// Index of the oldest queued message.
    head: usize,
    /// Number of queued messages, `0..=cap`.
    len: usize,
}

impl MsgRing {
    /// A ring holding `cap` (`≥ 1`) pre-allocated, initially empty slots.
    fn with_capacity(cap: usize) -> Result<Self, AllocError> {
        let mut slots: KVec<RingSlot> = KVec::new();
        slots.try_reserve(cap)?;
        for _ in 0..cap {
            // Within the reserved capacity — never reallocates.
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

    /// Copy `src` into the tail slot and **move** any `transfers` into it
    /// (`take`-ing them from the caller's array). Returns `false` if full (and
    /// leaves `transfers` untouched for the caller to reclaim).
    fn push_from(
        &mut self,
        src: &StoredMsg,
        transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    ) -> bool {
        if self.is_full() {
            return false;
        }
        let cap = self.cap();
        let idx = (self.head + self.len) % cap;
        let slot = &mut self.slots[idx];
        slot.msg = *src;
        for i in 0..IPC_HANDLE_MAX {
            slot.transfers[i] = transfers[i].take();
        }
        self.len += 1;
        true
    }

    /// Copy the head slot's bytes into `dst`, **move** its transfers into `out`,
    /// and advance. Returns `false` if empty.
    fn pop_into(
        &mut self,
        dst: &mut StoredMsg,
        out: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    ) -> bool {
        if self.is_empty() {
            return false;
        }
        let slot = &mut self.slots[self.head];
        *dst = slot.msg;
        for i in 0..IPC_HANDLE_MAX {
            out[i] = slot.transfers[i].take();
        }
        self.head = (self.head + 1) % self.cap();
        self.len -= 1;
        true
    }
}

/// Outcome of a [`send_push`](IpcChannel::send_push).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SendOutcome {
    /// Queued. `woke_edge` is `true` iff the peer's receive ring went
    /// empty→non-empty (so the peer's blocked receivers must be woken).
    Sent { woke_edge: bool },
    /// The peer's receive ring is full (NoBlock → `WouldBlock`).
    Full,
    /// The peer endpoint has closed.
    PeerClosed,
}

/// Outcome of a blocking send ([`send_or_queue`](IpcChannel::send_or_queue)) —
/// the `Block` / `BlockBounded` path. Unlike [`SendOutcome`] there is no plain
/// "full": a full receive ring means the message is **held** (`Queued`) until a
/// recv frees space; the only hard failure is the bounded pending-send queue
/// itself overflowing.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum BlockSendOutcome {
    /// Delivered straight into the peer's receive ring. `woke_edge` is `true`
    /// iff the ring went empty→non-empty (wake the peer's receivers). The
    /// caller's [`PendingOperation`] is completed immediately (pre-signalled).
    Sent { woke_edge: bool },
    /// The peer's ring was full; the message (and its transfers + a reference to
    /// the caller's `PendingOperation`) is held in the peer's pending-send queue
    /// and will be delivered — completing the PO — when the peer next receives.
    Queued,
    /// The peer's pending-send queue is at capacity — back-pressure. The
    /// transfers are left untaken for the caller to reclaim (→ `WouldBlock`).
    PendingFull,
    /// The peer endpoint has closed.
    PeerClosed,
}

/// A blocking send held in a receiving endpoint's pending-send queue: the copied
/// message, the in-flight transferred-handle references it carries, and an owning
/// reference to the sender's [`PendingOperation`] (kept alive even if the sender
/// closes its handle early). Delivered — moving `msg`/`transfers` into the ring
/// and completing `po` (status 0) — when the endpoint next receives, or completed
/// with `PeerClosed` (and its `transfers` reclaimed) when the endpoint closes.
struct PendingSend {
    msg: StoredMsg,
    transfers: [Option<TransferRef>; IPC_HANDLE_MAX],
    po: ObjectRef,
    /// Set when a `BlockBounded` delivery deadline elapsed before delivery: the
    /// `po` was already completed `TimedOut`, so this entry is **not** delivered
    /// — it is swept out and reclaimed at the next `promote_pending_send` (or at
    /// close). Always `false` for `Block`. See [`cancel_pending_send`].
    ///
    /// [`cancel_pending_send`]: IpcChannel::cancel_pending_send
    cancelled: bool,
}

/// The droppable parts of a cancelled (timed-out) [`PendingSend`], handed back by
/// [`promote_pending_send`](IpcChannel::promote_pending_send) for the caller to
/// release **outside** `SCHED` (the `Drop`s of `po` / a transferred object may
/// take lower-rank locks — e.g. the buddy allocator — which must not nest under
/// the rank-1 scheduler lock). The message bytes are `Copy` and need no reclaim.
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
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see [`crate::object::header`].
#[repr(C)]
pub struct IpcChannel {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`IpcChannel::MAGIC`].
    magic: u64,
    /// All mutable state, reached only under `SCHED`.
    inner: UnsafeCell<Inner>,
}

struct Inner {
    /// This endpoint's inbox: messages the peer sent here, awaiting recv.
    recv: MsgRing,
    /// Threads blocked recv-ing on this endpoint (type-erased `Thread` pointers,
    /// non-owning; removed before a waiter unparks). Pre-reserved to
    /// [`IpcChannel::MAX_WAITERS`] so `add_waiter` never allocates under `SCHED`.
    recv_waiters: KVec<*mut ()>,
    /// Blocking senders whose message is waiting for space in **this** endpoint's
    /// receive ring (`Block` / `BlockBounded`). FIFO; promoted into `recv` when a
    /// recv frees a slot. Pre-reserved to [`IpcChannel::MAX_PENDING_SENDS`] so
    /// `send_or_queue` never allocates under `SCHED`.
    pending_sends: KVec<PendingSend>,
    /// The other endpoint, or null once it has closed.
    peer: *mut IpcChannel,
    /// Non-null iff this endpoint is the **kernel's** end of a Userspace Server
    /// channel: a back-pointer (type-erased, non-owning) to the owning
    /// [`UserspaceServerReg`](crate::object::UserspaceServerReg). A reply sent to
    /// this endpoint (by the server, on its peer) is a forwarded-lookup reply the
    /// kernel completes inline rather than enqueues. Null on every ordinary
    /// channel. The registration *owns* this endpoint (the only reference), so the
    /// back-pointer is valid for as long as the endpoint is alive.
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

    /// Maximum simultaneous receivers blocked on one endpoint. Bounds the
    /// pre-reserved waiter vector. Matches [`Timer::MAX_WAITERS`].
    ///
    /// [`Timer::MAX_WAITERS`]: crate::object::Timer::MAX_WAITERS
    pub const MAX_WAITERS: usize = 4;

    /// Maximum messages held in one endpoint's pending-send queue (blocking
    /// senders whose message awaits space in this endpoint's receive ring).
    /// Bounds the pre-reserved `pending_sends` vector so `send_or_queue` never
    /// allocates under `SCHED`; beyond it, a blocking send gets `WouldBlock`
    /// back-pressure.
    pub const MAX_PENDING_SENDS: usize = 4;

    /// Create a connected endpoint **pair**, each with a `depth`-slot receive
    /// ring (clamped to `1..=IPC_MAX_QUEUE_DEPTH`). The two returned `KBox`es
    /// each hold one creation reference; the caller installs them into handles.
    pub fn try_new_pair(depth: u32) -> Result<(KBox<Self>, KBox<Self>), AllocError> {
        let cap = depth.clamp(1, IPC_MAX_QUEUE_DEPTH) as usize;
        let mut a = Self::try_new_endpoint(cap)?;
        let mut b = Self::try_new_endpoint(cap)?;
        // Link the peers. Both boxes are still private to this function (their
        // handles are not yet allocated), so no other context can observe them;
        // plain writes through the cell are sound, no `SCHED` needed yet.
        let a_ptr = (&*a as *const Self) as *mut Self;
        let b_ptr = (&*b as *const Self) as *mut Self;
        a.inner.get_mut().peer = b_ptr;
        b.inner.get_mut().peer = a_ptr;
        Ok((a, b))
    }

    /// Allocate one unconnected endpoint (`peer == null`), refcount one. The
    /// receive ring + waiter vector are reserved up front — the only fallible
    /// growth — so later `push`/`add_waiter` never allocate under `SCHED`.
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
    #[allow(clippy::mut_from_ref)]
    unsafe fn inner<'a>(obj: *mut ()) -> &'a mut Inner {
        // SAFETY: forming a shared `&IpcChannel` to reach the `UnsafeCell`, then
        // a `&mut Inner` through it, is the interior-mutability contract — sound
        // while `SCHED` serialises access.
        let c = unsafe { &*(obj as *const IpcChannel) };
        unsafe { &mut *c.inner.get() }
    }

    /// The peer endpoint pointer (type-erased), or null if the peer has closed.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn peer_of(obj: *mut ()) -> *mut () {
        unsafe { Self::inner(obj) }.peer as *mut ()
    }

    /// Null this endpoint's peer pointer (the peer is closing).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn clear_peer(obj: *mut ()) {
        unsafe { Self::inner(obj) }.peer = core::ptr::null_mut();
    }

    /// Mark this endpoint as the kernel's end of a Userspace Server channel,
    /// recording `reg` (the owning
    /// [`UserspaceServerReg`](crate::object::UserspaceServerReg)). Set once, when a
    /// supervisor binds the endpoint as a `UserspaceServer`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn set_us_reg(obj: *mut (), reg: *mut ()) {
        unsafe { Self::inner(obj) }.us_reg = reg;
    }

    /// The owning [`UserspaceServerReg`](crate::object::UserspaceServerReg)
    /// back-pointer for this endpoint (type-erased), or null if this is an ordinary
    /// channel endpoint.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn us_reg_of(obj: *mut ()) -> *mut () {
        unsafe { Self::inner(obj) }.us_reg
    }

    /// Push `msg` (and **move** any `transfers` it carries) into the **peer**'s
    /// receive ring. The `woke_edge` flag on `Sent` reports whether the peer went
    /// empty→non-empty. On `Full`/`PeerClosed` the `transfers` are left untouched
    /// (the caller reclaims them).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn send_push(
        obj: *mut (),
        msg: &StoredMsg,
        transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
    ) -> SendOutcome {
        let peer = unsafe { Self::inner(obj) }.peer;
        if peer.is_null() {
            return SendOutcome::PeerClosed;
        }
        // Distinct object from `obj` (the two endpoints differ), so this second
        // borrow does not alias the first (which has ended).
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
        unsafe { Self::inner(obj) }.recv.pop_into(dst, out)
    }

    /// `true` iff a recv would return something (a queued message) or report a
    /// closed peer — the waitable "signaled" predicate.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn already_signaled(obj: *mut ()) -> bool {
        let inner = unsafe { Self::inner(obj) };
        !inner.recv.is_empty() || inner.peer.is_null()
    }

    /// Register `thread` as a receiver. `Err(())` if already at
    /// [`MAX_WAITERS`](Self::MAX_WAITERS); never grows under the lock.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn add_waiter(obj: *mut (), thread: *mut ()) -> Result<(), ()> {
        let inner = unsafe { Self::inner(obj) };
        if inner.recv_waiters.len() < Self::MAX_WAITERS {
            inner
                .recv_waiters
                .try_push(thread)
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
        let inner = unsafe { Self::inner(obj) };
        if let Some(i) = inner.recv_waiters.iter().position(|&w| w == thread) {
            inner.recv_waiters.remove(i);
        }
    }

    /// Drain every receiver into `out` (clearing the set), returning the count.
    /// `out` must be at least [`MAX_WAITERS`](Self::MAX_WAITERS) long.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn take_waiters(obj: *mut (), out: &mut [*mut ()]) -> usize {
        let inner = unsafe { Self::inner(obj) };
        let n = inner.recv_waiters.len();
        debug_assert!(out.len() >= n);
        for (i, &w) in inner.recv_waiters.iter().enumerate() {
            out[i] = w;
        }
        inner.recv_waiters.clear();
        n
    }

    // --- Blocking-send (pending-sender) accessors ----------------------

    /// Blocking-send entry: try to deliver `msg` straight into the **peer**'s
    /// receive ring; if the ring is full, **hold** it in the peer's pending-send
    /// queue together with a cloned reference to the caller's `PendingOperation`
    /// `po`. On `Sent`/`Queued` the `transfers` are moved out (taken); on
    /// `PendingFull`/`PeerClosed` they are left for the caller to reclaim.
    ///
    /// # Safety
    /// See the accessor contract above; `po` references a live `PendingOperation`.
    pub(crate) unsafe fn send_or_queue(
        obj: *mut (),
        msg: &StoredMsg,
        transfers: &mut [Option<TransferRef>; IPC_HANDLE_MAX],
        po: &ObjectRef,
    ) -> BlockSendOutcome {
        let peer = unsafe { Self::inner(obj) }.peer;
        if peer.is_null() {
            return BlockSendOutcome::PeerClosed;
        }
        // Distinct object from `obj`; the first borrow has ended.
        let peer_inner = unsafe { Self::inner(peer as *mut ()) };
        let was_empty = peer_inner.recv.is_empty();
        if peer_inner.recv.push_from(msg, transfers) {
            return BlockSendOutcome::Sent { woke_edge: was_empty };
        }
        // Ring full — hold the message if the pending-send queue has room.
        if peer_inner.pending_sends.len() >= Self::MAX_PENDING_SENDS {
            return BlockSendOutcome::PendingFull; // transfers left for the caller
        }
        let mut held: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
        for i in 0..IPC_HANDLE_MAX {
            held[i] = transfers[i].take();
        }
        // `po.clone()` bumps the refcount (atomic; no alloc/drop) — sound under SCHED.
        peer_inner
            .pending_sends
            .try_push(PendingSend {
                msg: *msg,
                transfers: held,
                po: po.clone(),
                cancelled: false,
            })
            .expect("within reserved pending-send capacity");
        BlockSendOutcome::Queued
    }

    /// Mark the held sender whose `PendingOperation` is `po` as **cancelled** (a
    /// `BlockBounded` delivery deadline elapsed). The entry is not removed here —
    /// its `po` was already completed `TimedOut` by the caller, and its message +
    /// refs are reclaimed at the next [`promote_pending_send`] / at close (a drop
    /// must not run under `SCHED`). Returns `true` if a matching held send was
    /// found (idempotent; a no-op if it was already delivered/removed).
    ///
    /// [`promote_pending_send`]: IpcChannel::promote_pending_send
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn cancel_pending_send(obj: *mut (), po: *mut ()) -> bool {
        let inner = unsafe { Self::inner(obj) };
        if let Some(p) = inner.pending_sends.iter_mut().find(|p| p.po.as_ptr() == po) {
            p.cancelled = true;
            true
        } else {
            false
        }
    }

    /// After a recv frees a slot, sweep out any **cancelled** (timed-out) held
    /// sends — oldest first — collecting their droppable parts into `reclaimed`
    /// for the caller to release **outside** `SCHED`, then promote the oldest
    /// **live** sender into this endpoint's receive ring (FIFO): move its message
    /// + transfers into `recv` and return its `PendingOperation` reference (the
    /// caller completes it status 0 under `SCHED`, drops it outside). Returns
    /// `None` if no live sender was promoted (the ring stays one slot freer).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn promote_pending_send(
        obj: *mut (),
        reclaimed: &mut [Option<ReclaimedSend>; Self::MAX_PENDING_SENDS],
    ) -> Option<ObjectRef> {
        let inner = unsafe { Self::inner(obj) };
        let mut r = 0;
        loop {
            // Peek the front entry's `cancelled` flag (Copy) so the immutable
            // borrow ends before the mutating `remove` below.
            let cancelled = match inner.pending_sends.first() {
                None => return None, // queue drained
                Some(p) => p.cancelled,
            };
            if cancelled {
                // Reclaim it (move its refs out for outside-SCHED drop); the
                // message bytes are Copy and discarded.
                let ps = inner.pending_sends.remove(0);
                debug_assert!(r < reclaimed.len());
                reclaimed[r] = Some(ReclaimedSend { po: ps.po, transfers: ps.transfers });
                r += 1;
                continue;
            }
            // The oldest live entry: deliver it into the freed ring slot.
            if inner.recv.is_full() {
                return None; // no freed slot to promote into
            }
            let mut ps = inner.pending_sends.remove(0);
            let pushed = inner.recv.push_from(&ps.msg, &mut ps.transfers);
            debug_assert!(pushed, "promote into a non-full ring must succeed");
            return Some(ps.po);
        }
    }

    /// Copy the `PendingOperation` pointers of every held sender into `out` (for
    /// the closing path to complete them `PeerClosed`), returning the count. Does
    /// **not** remove them — their references are released when this endpoint's
    /// `Inner` drops, outside `SCHED`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn pending_send_pos(obj: *mut (), out: &mut [*mut ()]) -> usize {
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
    /// Unlink from the peer and wake its blocked receivers (so they observe
    /// `PeerClosed`), then assert this endpoint has no receivers of its own (a
    /// live waiter pins it via its parked `ObjectRef`, so the last reference
    /// cannot drop while receivers remain). Reaches `SCHED`; sound because
    /// endpoint references are released only outside `SCHED` (see module docs).
    fn drop(&mut self) {
        crate::sched::ipc_endpoint_closing(self as *mut Self as *mut ());
        debug_assert!(
            self.inner.get_mut().recv_waiters.is_empty(),
            "IpcChannel dropped with live receivers"
        );
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
        (
            KBox::into_raw(a).as_ptr() as *mut (),
            KBox::into_raw(b).as_ptr() as *mut (),
        )
    }

    /// Adopt the creation reference and drop it (running `IpcChannel::drop`,
    /// which unlinks the peer under `SCHED`).
    fn drop_endpoint(obj: *mut ()) {
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
    /// # Safety: as `IpcChannel::send_push`.
    unsafe fn push_no_xfer(obj: *mut (), msg: &StoredMsg) -> SendOutcome {
        let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
        unsafe { IpcChannel::send_push(obj, msg, &mut t) }
    }

    /// `recv_pop_into` discarding any (absent) transfers.
    /// # Safety: as `IpcChannel::recv_pop_into`.
    unsafe fn pop_no_xfer(obj: *mut (), dst: &mut StoredMsg) -> bool {
        let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
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
            // a is empty; sending on a targets b's inbox, so a stays empty.
            assert!(!IpcChannel::already_signaled(a));
            let edge = push_no_xfer(a, &a_msg(0x11, 3));
            assert_eq!(edge, SendOutcome::Sent { woke_edge: true });
            let edge2 = push_no_xfer(a, &a_msg(0x22, 5));
            assert_eq!(edge2, SendOutcome::Sent { woke_edge: false });
            // a's own inbox is still empty; b's has two messages.
            assert!(!IpcChannel::already_signaled(a));
            assert!(IpcChannel::already_signaled(b));
            assert_eq!(IpcChannel::recv_peek(b), RecvState::HasMsg);
            // FIFO drain on b.
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
                assert!(matches!(
                    push_no_xfer(a, &a_msg(0xAB, 1)),
                    SendOutcome::Sent { .. }
                ));
            }
            // Fifth send: b's inbox (depth 4) is full.
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
            // Push 3, pop 2, push 3 more → wraps past the end; drain in order.
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
            // Remaining: 2,3,4,5 in FIFO order (1 left from first batch + 3 new).
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
            // Queue one message into b before closing a.
            push_no_xfer(a, &a_msg(0x7E, 2));
        }
        // Close a (drops its only reference → `drop` nulls b's peer pointer).
        drop_endpoint(a);
        // SAFETY: b is still live.
        unsafe {
            assert!(IpcChannel::peer_of(b).is_null());
            // b still drains its queued message first ...
            assert_eq!(IpcChannel::recv_peek(b), RecvState::HasMsg);
            let mut got = StoredMsg::zeroed();
            assert!(pop_no_xfer(b, &mut got));
            assert_eq!(got.payload[0], 0x7E);
            // ... then reports the closed peer (and a closed peer is "signaled"
            // so a blocked recv wakes to see it).
            assert_eq!(IpcChannel::recv_peek(b), RecvState::PeerClosed);
            assert!(IpcChannel::already_signaled(b));
            // Sending from b now fails: the peer is gone.
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
            let ths: [*mut (); IpcChannel::MAX_WAITERS] =
                core::array::from_fn(|i| (0x1000 + i) as *mut ());
            for &t in &ths {
                assert!(IpcChannel::add_waiter(a, t).is_ok());
            }
            assert!(IpcChannel::add_waiter(a, 0xDEAD as *mut ()).is_err());
            IpcChannel::remove_waiter(a, ths[0]);
            assert!(IpcChannel::add_waiter(a, 0xBEEF as *mut ()).is_ok());
            // Drain before dropping (a dropped-with-waiters debug-asserts).
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
        // An object to transfer (a Process; refcount 1 via its creation ref).
        let p = KBox::into_raw(Process::try_new(99).unwrap()).as_ptr() as *mut ();
        let pref = unsafe { ObjectRef::from_raw(p, KObjectType::Process) };
        // SAFETY: live endpoints, single-threaded test.
        unsafe {
            let mut send_t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
            send_t[0] = Some(TransferRef { obj: pref, rights: Rights::empty() });
            let mut msg = a_msg(0, 0);
            msg.header.handle_count = 1;
            // Send on a → b's inbox; the transfer moves into the queued slot.
            assert!(matches!(
                IpcChannel::send_push(a, &msg, &mut send_t),
                SendOutcome::Sent { .. }
            ));
            assert!(send_t[0].is_none(), "transfer moved into the queue");
            // Pop on b → the transfer moves back out.
            let mut got = StoredMsg::zeroed();
            let mut recv_t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
            assert!(IpcChannel::recv_pop_into(b, &mut got, &mut recv_t));
            assert_eq!(got.header.handle_count, 1);
            let tr = recv_t[0].take().unwrap();
            assert_eq!(tr.obj.as_ptr(), p);
            // The reference is still held (not destroyed); dropping it destroys
            // the object (refcount 1 → 0).
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
        let pref = unsafe { ObjectRef::from_raw(p, KObjectType::Process) };
        // SAFETY: live endpoints, single-threaded test.
        unsafe {
            let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
            t[0] = Some(TransferRef { obj: pref, rights: Rights::empty() });
            let mut msg = a_msg(0, 0);
            msg.header.handle_count = 1;
            // Queue the transfer into b's inbox but never receive it.
            IpcChannel::send_push(a, &msg, &mut t);
        }
        assert_eq!(test_probe::process_destroys(), 0);
        // Destroying the endpoints (b holds the queued message) drops the
        // undelivered transfer's reference → the object is released.
        drop_endpoint(a);
        drop_endpoint(b);
        assert_eq!(test_probe::process_destroys(), 1);
    }

    #[test]
    fn payload_size_is_one_page_worth() {
        // Guard the reconciled constant from drifting.
        assert_eq!(IPC_PAYLOAD_SIZE, 4008);
        assert_eq!(core::mem::size_of::<StoredMsg>(), 4096);
    }

    // --- Blocking send (pending-sender queue) --------------------------

    /// A fresh `PendingOperation`, adopting its creation reference.
    fn make_po() -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(crate::object::PendingOperation::try_new().unwrap()).as_ptr()
                    as *mut (),
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
            // Delivered straight into b's ring; nothing held.
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
            // 5th send: ring full → held in b's pending-send queue.
            let mut t = empty_xfer();
            assert_eq!(
                IpcChannel::send_or_queue(a, &a_msg(99, 1), &mut t, &po),
                BlockSendOutcome::Queued
            );
            // Receive one (frees a slot): FIFO byte 0.
            let mut d = a_msg(0, 0);
            assert!(pop_no_xfer(b, &mut d));
            assert_eq!(d.payload[0], 0);
            // Promote the held sender into b's ring; returns its PO reference.
            let promoted = IpcChannel::promote_pending_send(b, &mut empty_reclaim());
            assert!(promoted.is_some());
            // Drain the rest: 1, 2, 3, then the promoted 99 (FIFO preserved).
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
            assert_eq!(
                IpcChannel::send_or_queue(a, &a_msg(99, 1), &mut t, &po),
                BlockSendOutcome::Queued
            );
            // The held sender is visible to the closing path.
            let mut pos = [core::ptr::null_mut(); IpcChannel::MAX_PENDING_SENDS];
            assert_eq!(IpcChannel::pending_send_pos(b, &mut pos), 1);
            assert_eq!(pos[0], po.as_ptr());
        }
        // Dropping b (the receiver holding the pending send) releases the queued
        // PO reference (and the held transfers) via its `Inner` drop.
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
            assert_eq!(
                IpcChannel::send_or_queue(a, &a_msg(99, 1), &mut t, &po),
                BlockSendOutcome::Queued
            );
            // Cancel finds the held send by its PO; an unknown PO is a no-op.
            assert!(!IpcChannel::cancel_pending_send(b, 0xDEAD as *mut ()));
            assert!(IpcChannel::cancel_pending_send(b, po.as_ptr()));
        }
        // The cancelled (un-promoted) send is reclaimed via b's Inner drop.
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
            assert_eq!(
                IpcChannel::send_or_queue(a, &a_msg(50, 1), &mut t1, &po1),
                BlockSendOutcome::Queued
            );
            let mut t2 = empty_xfer();
            assert_eq!(
                IpcChannel::send_or_queue(a, &a_msg(51, 1), &mut t2, &po2),
                BlockSendOutcome::Queued
            );
            // po1 times out.
            assert!(IpcChannel::cancel_pending_send(b, po1.as_ptr()));
            // A recv frees a slot: promote sweeps the cancelled po1 (reclaimed)
            // and delivers the live po2.
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
        // Both creation refs now the last → both POs destroyed.
        drop(po1);
        drop(po2);
        assert_eq!(test_probe::pending_op_destroys(), 2);
        drop_endpoint(a);
        drop_endpoint(b);
    }
}
