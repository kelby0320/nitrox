//! The [`UserspaceServerReg`] kernel object — the kernel's registration record
//! for a **Userspace Server** (the second kind of resource server; the first,
//! a Kernel Server, is in [`crate::object::kernel_server`]).
//!
//! A [`BindingTarget::UserspaceServer`](crate::object::namespace::BindingTarget)
//! binding holds an `ObjectRef` to one of these. It owns the kernel's private
//! end of an IPC channel (an [`IpcChannel`] whose *peer* the server process
//! services) and the small **pending-lookup table** that correlates an
//! in-flight forwarded `sys_ns_lookup` with the reply that completes it. See
//! `docs/architecture/namespace-and-resource-servers.md` § "Userspace Servers"
//! and `docs/spec/rsproto-namespace-ops.md`.
//!
//! ## How a forwarded lookup flows
//!
//! 1. `sys_ns_lookup` resolves a path to a `UserspaceServer` binding and calls
//!    [`crate::sched::us_forward_originate`], which — under `SCHED` —
//!    [`begin`](UserspaceServerReg::begin)s a pending entry (assigning a
//!    `request_id`) and pushes a `Namespace::Resolve` request into the server's
//!    inbox (the peer of [`endpoint`](UserspaceServerReg::endpoint_ptr)). The
//!    lookup's `PendingOperation` is left **uncompleted**.
//! 2. The server replies on its endpoint. The kernel detects that the send's peer
//!    is this registration's endpoint (the `IpcChannel` back-pointer), drains the
//!    reply inline in the send syscall,
//!    [`take_pending_matching`](UserspaceServerReg::take_pending_matching) the
//!    request, cross-context-installs the transferred `MemoryObject`, and
//!    completes the lookup PO.
//!
//! ## Mutation discipline
//!
//! Exactly like [`IpcChannel`] / [`Timer`](crate::object::Timer): all interior
//! state lives in an [`UnsafeCell`] touched **only while the rank-1 `SCHED` lock
//! is held**. The owned `endpoint` / pending `po` `ObjectRef`s are released only
//! when the registration is destroyed (the `KBox` drop, run by `dispatch_destroy`
//! **outside** `SCHED`) or moved out of a `take_pending_*` return for the caller
//! to drop outside `SCHED` — never dropped under the lock.
//!
//! Slice 7 sizes the pending table at **N = 1** (a single outstanding lookup per
//! server): the milestone init path issues lookups one at a time, and N = 1 makes
//! request correlation trivial. Raising it to a small fixed array is a localized
//! change (Part 4, if boot issues overlapping lookups).

use core::cell::UnsafeCell;

use crate::libkern::handle::{KObjectType, Rights};
use crate::libkern::{AllocError, KBox};
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;

/// One outstanding forwarded lookup: everything needed to complete its
/// `PendingOperation` when the server's reply arrives. Moved out of the table by
/// [`take_pending_matching`](UserspaceServerReg::take_pending_matching) /
/// [`take_pending_any`](UserspaceServerReg::take_pending_any); its `po` is then
/// dropped by the caller **outside** `SCHED`.
pub struct PendingLookup {
    /// The `request_id` the kernel stamped on the Resolve request; the reply must
    /// echo it.
    pub(crate) request_id: u64,
    /// The lookup's `PendingOperation` (a clone of the handle the client holds),
    /// pinning it so the reply can complete it even if the client closed its handle.
    pub(crate) po: ObjectRef,
    /// The client process to install the resolved handle into (cross-context).
    pub(crate) owner_pid: u32,
    /// The `Rights` the lookup requested; the installed handle's rights are
    /// `requested ∩ (the rights the server granted on the transferred object)`.
    pub(crate) requested: Rights,
}

struct Inner {
    /// The kernel's IPC endpoint; its peer is the endpoint the server services.
    endpoint: ObjectRef,
    /// The single outstanding forwarded lookup (slice 7 N = 1), or `None` when idle.
    pending: Option<PendingLookup>,
    /// Monotonic `request_id` stamp.
    next_id: u64,
}

/// The kernel's registration record for one Userspace Server.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see [`crate::object::header`].
#[repr(C)]
pub struct UserspaceServerReg {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`UserspaceServerReg::MAGIC`].
    magic: u64,
    /// All mutable state, reached only under `SCHED`.
    inner: UnsafeCell<Inner>,
}

// SAFETY: identical reasoning to `IpcChannel` — the header refcount is atomic and
// every access to `inner` is serialised under the single-CPU `SCHED` lock.
unsafe impl Send for UserspaceServerReg {}
// SAFETY: as `Send`.
unsafe impl Sync for UserspaceServerReg {}

impl UserspaceServerReg {
    /// Sentinel written into [`UserspaceServerReg::magic`] at construction.
    pub const MAGIC: u64 = 0x55_73_53_72_76_52_67_21; // "UsSrvRg!"

    /// Allocate a registration owning `endpoint` (the kernel's IPC endpoint),
    /// refcount one, with an empty pending table. The caller installs the back-
    /// pointer from `endpoint` to this object (under `SCHED`) and binds the
    /// returned object as a `UserspaceServer` target.
    pub fn try_new(endpoint: ObjectRef) -> Result<KBox<Self>, AllocError> {
        debug_assert_eq!(endpoint.object_type(), KObjectType::IpcChannel);
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::UserspaceServerReg),
            magic: Self::MAGIC,
            inner: UnsafeCell::new(Inner { endpoint, pending: None, next_id: 1 }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    // --- Scheduler-only accessors --------------------------------------
    //
    // SAFETY (shared by all): `reg` addresses a live `UserspaceServerReg` (pinned
    // by an `ObjectRef` the caller holds), and the caller holds `SCHED`, which —
    // single-CPU — serialises all access to `inner`.

    /// Borrow the interior mutably (no aliasing; `SCHED` held).
    ///
    /// # Safety
    /// See the accessor contract above.
    #[allow(clippy::mut_from_ref)]
    unsafe fn inner<'a>(reg: *mut ()) -> &'a mut Inner {
        // SAFETY: forming a shared `&UserspaceServerReg` to reach the `UnsafeCell`,
        // then a `&mut Inner` through it, is the interior-mutability contract —
        // sound while `SCHED` serialises access.
        let r = unsafe { &*(reg as *const UserspaceServerReg) };
        unsafe { &mut *r.inner.get() }
    }

    /// The kernel-held endpoint pointer (type-erased) — the object a forwarded
    /// Resolve request is sent on (it lands in the *peer*'s inbox).
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn endpoint_ptr(reg: *mut ()) -> *mut () {
        unsafe { Self::inner(reg) }.endpoint.as_ptr()
    }

    /// Reserve the (single) pending-lookup slot for a new forwarded lookup,
    /// assigning and returning its `request_id`; `None` if a lookup is already
    /// outstanding (N = 1 — the caller fails the new lookup `WouldBlock`). Stores a
    /// clone of `po` (an atomic bump, sound under `SCHED`) so the reply can
    /// complete it later.
    ///
    /// # Safety
    /// See the accessor contract above; `po` references a live `PendingOperation`.
    pub(crate) unsafe fn begin(
        reg: *mut (),
        po: &ObjectRef,
        owner_pid: u32,
        requested: Rights,
    ) -> Option<u64> {
        let inner = unsafe { Self::inner(reg) };
        if inner.pending.is_some() {
            return None; // already busy (N = 1)
        }
        let request_id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1);
        inner.pending = Some(PendingLookup {
            request_id,
            po: po.clone(),
            owner_pid,
            requested,
        });
        Some(request_id)
    }

    /// Take the outstanding pending lookup **iff** its `request_id` matches
    /// `request_id` (a reply's echoed id); `None` on a mismatch or an empty slot
    /// (a duplicate / stale reply). The returned [`PendingLookup`]'s `po` is
    /// dropped by the caller **outside** `SCHED`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn take_pending_matching(
        reg: *mut (),
        request_id: u64,
    ) -> Option<PendingLookup> {
        let inner = unsafe { Self::inner(reg) };
        match &inner.pending {
            Some(p) if p.request_id == request_id => inner.pending.take(),
            _ => None,
        }
    }

    /// Take the outstanding pending lookup unconditionally (used to fail it on a
    /// dead peer / origination rollback). `None` if the slot is empty. The returned
    /// `po` is dropped by the caller **outside** `SCHED`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn take_pending_any(reg: *mut ()) -> Option<PendingLookup> {
        unsafe { Self::inner(reg) }.pending.take()
    }
}

// No `Drop` impl: the `KBox` drop (run by `dispatch_destroy`, outside any lock)
// drops `inner` → the owned `endpoint` `ObjectRef` (releasing the kernel endpoint,
// whose `IpcChannel::drop` unlinks its peer under `SCHED` — sound because this
// runs outside `SCHED`) and any pending `po`. A lookup still pending at teardown
// simply never completes (the binding is going away — typically with its client).

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::header::test_probe;
    use crate::object::{IpcChannel, PendingOperation};

    /// A live `IpcChannel` endpoint adopted into an `ObjectRef` (the registration's
    /// kernel end; its peer is dropped immediately — irrelevant to these unit
    /// tests, which never send).
    fn endpoint() -> ObjectRef {
        let (a, b) = IpcChannel::try_new_pair(4).unwrap();
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        let a_ref = unsafe {
            ObjectRef::from_raw(KBox::into_raw(a).as_ptr() as *mut (), KObjectType::IpcChannel)
        };
        // Drop the peer's creation reference (nulls `a`'s peer — fine here).
        drop(unsafe {
            ObjectRef::from_raw(KBox::into_raw(b).as_ptr() as *mut (), KObjectType::IpcChannel)
        });
        a_ref
    }

    fn make_po() -> ObjectRef {
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(PendingOperation::try_new().unwrap()).as_ptr() as *mut (),
                KObjectType::PendingOperation,
            )
        }
    }

    /// Adopt a registration into an `ObjectRef` (the form a binding holds).
    fn reg() -> ObjectRef {
        let r = UserspaceServerReg::try_new(endpoint()).unwrap();
        assert!(r.magic_ok());
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(
                KBox::into_raw(r).as_ptr() as *mut (),
                KObjectType::UserspaceServerReg,
            )
        }
    }

    #[test]
    fn begin_assigns_monotonic_ids_and_caps_at_one() {
        init_global_heap();
        let r = reg();
        let po = make_po();
        // First begin succeeds with an id.
        let id0 = unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 5, Rights::MAP_READ) };
        assert_eq!(id0, Some(1));
        // Second begin while one is outstanding → busy (N = 1).
        let id1 = unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 5, Rights::MAP_READ) };
        assert_eq!(id1, None);
        // Take it; the next begin gets the next id.
        let taken = unsafe { UserspaceServerReg::take_pending_any(r.as_ptr()) };
        assert_eq!(taken.as_ref().map(|p| p.request_id), Some(1));
        drop(taken);
        let id2 = unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 9, Rights::MAP_READ) };
        assert_eq!(id2, Some(2));
        // Clean up the outstanding entry before dropping.
        drop(unsafe { UserspaceServerReg::take_pending_any(r.as_ptr()) });
        drop(po);
        drop(r);
    }

    #[test]
    fn take_matching_correlates_by_request_id() {
        init_global_heap();
        let r = reg();
        let po = make_po();
        let id = unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 7, Rights::MAP_READ) }.unwrap();
        // A mismatched id leaves the entry in place.
        assert!(unsafe { UserspaceServerReg::take_pending_matching(r.as_ptr(), id ^ 0xFF) }.is_none());
        // The matching id takes it, carrying the recorded fields.
        let taken = unsafe { UserspaceServerReg::take_pending_matching(r.as_ptr(), id) }.unwrap();
        assert_eq!(taken.request_id, id);
        assert_eq!(taken.owner_pid, 7);
        assert_eq!(taken.requested, Rights::MAP_READ);
        // Now empty: a second take (matching or not) is None.
        assert!(unsafe { UserspaceServerReg::take_pending_matching(r.as_ptr(), id) }.is_none());
        drop(taken);
        drop(po);
        drop(r);
    }

    #[test]
    fn dropping_registration_routes_through_dispatch_destroy() {
        init_global_heap();
        let r = reg();
        // Reset AFTER construction: `reg()`/`endpoint()` drop the peer endpoint
        // (one `ipc_channel` destroy) while wiring up the pair.
        test_probe::reset();
        assert_eq!(test_probe::userspace_server_reg_destroys(), 0);
        assert_eq!(test_probe::ipc_channel_destroys(), 0);
        // The kernel endpoint is owned by the registration; dropping the last ref
        // runs the destructor, which cascades to the owned endpoint `ObjectRef`.
        drop(r);
        assert_eq!(test_probe::userspace_server_reg_destroys(), 1, "reg destructor ran");
        assert_eq!(test_probe::ipc_channel_destroys(), 1, "owned endpoint cascaded");
    }

    #[test]
    fn pending_po_is_released_when_registration_drops() {
        init_global_heap();
        test_probe::reset();
        let r = reg();
        let po = make_po();
        // Leave a lookup outstanding, then drop the registration.
        unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 1, Rights::MAP_READ) };
        assert_eq!(test_probe::pending_op_destroys(), 0);
        drop(r); // releases the pending entry's PO clone
        assert_eq!(test_probe::pending_op_destroys(), 0, "creation ref still held by `po`");
        drop(po); // last ref → PO destroyed
        assert_eq!(test_probe::pending_op_destroys(), 1);
    }
}
