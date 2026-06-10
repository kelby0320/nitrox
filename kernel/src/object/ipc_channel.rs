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

use crate::libkern::handle::KObjectType;
use crate::libkern::ipc::{IPC_MAX_QUEUE_DEPTH, IPC_MSG_SIZE, IPC_PAYLOAD_SIZE, IpcMsgHeader};
use crate::libkern::{AllocError, KBox, KVec};
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

/// A fixed-capacity ring of [`StoredMsg`] slots. The backing [`KVec`] is
/// pre-filled with `cap` zeroed slots at construction (the only fallible
/// growth); `push_from`/`pop_into` then move messages in place — no element
/// shifting, no reallocation, no large stack temporaries — so they are safe to
/// run under `SCHED`.
struct MsgRing {
    /// Exactly `cap` storage slots; `slots.len()` is the capacity.
    slots: KVec<StoredMsg>,
    /// Index of the oldest queued message.
    head: usize,
    /// Number of queued messages, `0..=cap`.
    len: usize,
}

impl MsgRing {
    /// A ring holding `cap` (`≥ 1`) pre-allocated, initially empty slots.
    fn with_capacity(cap: usize) -> Result<Self, AllocError> {
        let mut slots: KVec<StoredMsg> = KVec::new();
        slots.try_reserve(cap)?;
        for _ in 0..cap {
            // Within the reserved capacity — never reallocates.
            slots.try_push(StoredMsg::zeroed())?;
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

    /// Copy `src` into the tail slot. Returns `false` if full.
    fn push_from(&mut self, src: &StoredMsg) -> bool {
        if self.is_full() {
            return false;
        }
        let cap = self.cap();
        let idx = (self.head + self.len) % cap;
        self.slots[idx] = *src;
        self.len += 1;
        true
    }

    /// Copy the head slot into `dst` and advance. Returns `false` if empty.
    fn pop_into(&mut self, dst: &mut StoredMsg) -> bool {
        if self.is_empty() {
            return false;
        }
        *dst = self.slots[self.head];
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
    /// The other endpoint, or null once it has closed.
    peer: *mut IpcChannel,
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
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::IpcChannel),
            magic: Self::MAGIC,
            inner: UnsafeCell::new(Inner { recv, recv_waiters, peer: core::ptr::null_mut() }),
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

    /// Push `msg` into the **peer**'s receive ring. The `woke_edge` flag on
    /// `Sent` reports whether the peer went empty→non-empty.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn send_push(obj: *mut (), msg: &StoredMsg) -> SendOutcome {
        let peer = unsafe { Self::inner(obj) }.peer;
        if peer.is_null() {
            return SendOutcome::PeerClosed;
        }
        // Distinct object from `obj` (the two endpoints differ), so this second
        // borrow does not alias the first (which has ended).
        let peer_inner = unsafe { Self::inner(peer as *mut ()) };
        let was_empty = peer_inner.recv.is_empty();
        if peer_inner.recv.push_from(msg) {
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

    /// Pop the oldest message into `dst`. Returns `false` if empty.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn recv_pop_into(obj: *mut (), dst: &mut StoredMsg) -> bool {
        unsafe { Self::inner(obj) }.recv.pop_into(dst)
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
            let edge = IpcChannel::send_push(a, &a_msg(0x11, 3));
            assert_eq!(edge, SendOutcome::Sent { woke_edge: true });
            let edge2 = IpcChannel::send_push(a, &a_msg(0x22, 5));
            assert_eq!(edge2, SendOutcome::Sent { woke_edge: false });
            // a's own inbox is still empty; b's has two messages.
            assert!(!IpcChannel::already_signaled(a));
            assert!(IpcChannel::already_signaled(b));
            assert_eq!(IpcChannel::recv_peek(b), RecvState::HasMsg);
            // FIFO drain on b.
            let mut got = StoredMsg::zeroed();
            assert!(IpcChannel::recv_pop_into(b, &mut got));
            assert_eq!(got.header.payload_len, 3);
            assert_eq!(got.payload[0], 0x11);
            assert!(IpcChannel::recv_pop_into(b, &mut got));
            assert_eq!(got.header.payload_len, 5);
            assert_eq!(got.payload[0], 0x22);
            assert!(!IpcChannel::recv_pop_into(b, &mut got));
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
                    IpcChannel::send_push(a, &a_msg(0xAB, 1)),
                    SendOutcome::Sent { .. }
                ));
            }
            // Fifth send: b's inbox (depth 4) is full.
            assert_eq!(IpcChannel::send_push(a, &a_msg(0xAB, 1)), SendOutcome::Full);
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
                IpcChannel::send_push(a, &a_msg(i, 1));
            }
            for i in 0..2u8 {
                assert!(IpcChannel::recv_pop_into(b, &mut got));
                assert_eq!(got.payload[0], i);
            }
            for i in 3..6u8 {
                assert!(matches!(IpcChannel::send_push(a, &a_msg(i, 1)), SendOutcome::Sent { .. }));
            }
            // Remaining: 2,3,4,5 in FIFO order (1 left from first batch + 3 new).
            for i in 2..6u8 {
                assert!(IpcChannel::recv_pop_into(b, &mut got));
                assert_eq!(got.payload[0], i);
            }
            assert!(!IpcChannel::recv_pop_into(b, &mut got));
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
            IpcChannel::send_push(a, &a_msg(0x7E, 2));
        }
        // Close a (drops its only reference → `drop` nulls b's peer pointer).
        drop_endpoint(a);
        // SAFETY: b is still live.
        unsafe {
            assert!(IpcChannel::peer_of(b).is_null());
            // b still drains its queued message first ...
            assert_eq!(IpcChannel::recv_peek(b), RecvState::HasMsg);
            let mut got = StoredMsg::zeroed();
            assert!(IpcChannel::recv_pop_into(b, &mut got));
            assert_eq!(got.payload[0], 0x7E);
            // ... then reports the closed peer (and a closed peer is "signaled"
            // so a blocked recv wakes to see it).
            assert_eq!(IpcChannel::recv_peek(b), RecvState::PeerClosed);
            assert!(IpcChannel::already_signaled(b));
            // Sending from b now fails: the peer is gone.
            assert_eq!(IpcChannel::send_push(b, &a_msg(0, 1)), SendOutcome::PeerClosed);
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
    fn payload_size_is_one_page_worth() {
        // Guard the reconciled constant from drifting.
        assert_eq!(IPC_PAYLOAD_SIZE, 4008);
        assert_eq!(core::mem::size_of::<StoredMsg>(), 4096);
    }
}
