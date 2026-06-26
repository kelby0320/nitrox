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
use crate::mm::PhysAddr;
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;

/// Largest lookup suffix a [`PendingLookup`] stores inline (so a lazy `File`
/// resolve can name the file in its page-cache producer without allocating under
/// `SCHED`). A longer suffix is recorded with its true `suffix_len` but a
/// truncated buffer — harmless for an eager `MEMOBJ` reply (the suffix is unused),
/// but a `FILE` reply for such a path fails `TooLarge` (see the completion path).
/// 256 bytes covers every milestone path; a heap-backed suffix is a later concern.
pub const LOOKUP_SUFFIX_MAX: usize = 256;

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
    /// The lookup's path suffix, stored inline (`suffix[..suffix_len.min(MAX)]`).
    /// A `FILE` reply uses it to name the file in the page-cache producer; an eager
    /// `MEMOBJ` reply ignores it. `suffix_len` is the *true* length even if it
    /// exceeds [`LOOKUP_SUFFIX_MAX`] (then the buffer is truncated and a `FILE`
    /// reply fails `TooLarge`).
    pub(crate) suffix: [u8; LOOKUP_SUFFIX_MAX],
    pub(crate) suffix_len: u16,
}

impl PendingLookup {
    /// The stored suffix bytes, or `None` if the true length overran
    /// [`LOOKUP_SUFFIX_MAX`] (the inline buffer is then incomplete — a `FILE` reply
    /// for it cannot recover the path).
    pub(crate) fn suffix(&self) -> Option<&[u8]> {
        let n = self.suffix_len as usize;
        if n > LOOKUP_SUFFIX_MAX {
            None
        } else {
            Some(&self.suffix[..n])
        }
    }
}

/// One outstanding forwarded **page-cache fill** (`File::ReadRange`): everything the
/// reply needs to land the page and wake the faulting thread. Parallels
/// [`PendingLookup`] but for the fill seam — the kernel originates the range-read
/// when a `FileObject` page faults, parks the faulter on `po`, and the reply copies
/// the bytes into `frame`, marks the page ready, and completes `po`. Moved out by
/// [`take_pending_fill_matching`](UserspaceServerReg::take_pending_fill_matching);
/// its `po` / `file_obj` are dropped by the caller **outside** `SCHED`.
pub struct PendingFill {
    /// The `request_id` stamped on the `ReadRange` request; the reply echoes it.
    pub(crate) request_id: u64,
    /// The fill's `PendingOperation` (the faulting thread blocks on it).
    pub(crate) po: ObjectRef,
    /// The `FileObject` being filled (pins it; the reply marks its page ready).
    pub(crate) file_obj: ObjectRef,
    /// The cache frame to copy the replied bytes into.
    pub(crate) frame: PhysAddr,
    /// The page index within the file.
    pub(crate) index: usize,
}

struct Inner {
    /// The kernel's IPC endpoint; its peer is the endpoint the server services.
    endpoint: ObjectRef,
    /// The single outstanding forwarded lookup (N = 1), or `None` when idle.
    pending: Option<PendingLookup>,
    /// The single outstanding forwarded page-cache fill (N = 1), or `None`. Separate
    /// from `pending` — a lookup and a fill correlate by distinct `request_id`s and
    /// route by reply op; the milestone never overlaps them, but the slots are
    /// independent so it would be correct if it did.
    pending_fill: Option<PendingFill>,
    /// Monotonic `request_id` stamp (shared across lookups and fills).
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
            inner: UnsafeCell::new(Inner {
                endpoint,
                pending: None,
                pending_fill: None,
                next_id: 1,
            }),
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
        suffix: &[u8],
    ) -> Option<u64> {
        let inner = unsafe { Self::inner(reg) };
        if inner.pending.is_some() {
            return None; // already busy (N = 1)
        }
        let request_id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1);
        // Copy the suffix inline (a memcpy, no allocation — sound under `SCHED`);
        // record the *true* length so an overrun is detectable later.
        let mut sbuf = [0u8; LOOKUP_SUFFIX_MAX];
        let n = suffix.len().min(LOOKUP_SUFFIX_MAX);
        sbuf[..n].copy_from_slice(&suffix[..n]);
        inner.pending = Some(PendingLookup {
            request_id,
            po: po.clone(),
            owner_pid,
            requested,
            suffix: sbuf,
            suffix_len: suffix.len() as u16,
        });
        Some(request_id)
    }

    /// Reserve the (single) pending-**fill** slot for a new forwarded
    /// `File::ReadRange`, assigning and returning its `request_id`; `None` if a fill
    /// is already outstanding (N = 1). Stores clones of `po` and `file_obj` (atomic
    /// bumps, sound under `SCHED`) so the reply can land the page even if the
    /// faulting thread's references changed.
    ///
    /// # Safety
    /// See the accessor contract above; `po` / `file_obj` reference live objects.
    pub(crate) unsafe fn begin_fill(
        reg: *mut (),
        po: &ObjectRef,
        file_obj: &ObjectRef,
        frame: PhysAddr,
        index: usize,
    ) -> Option<u64> {
        let inner = unsafe { Self::inner(reg) };
        if inner.pending_fill.is_some() {
            return None; // already busy (N = 1)
        }
        let request_id = inner.next_id;
        inner.next_id = inner.next_id.wrapping_add(1);
        inner.pending_fill = Some(PendingFill {
            request_id,
            po: po.clone(),
            file_obj: file_obj.clone(),
            frame,
            index,
        });
        Some(request_id)
    }

    /// Take the outstanding pending fill **iff** its `request_id` matches (a
    /// `ReadRange` reply's echoed id); `None` on a mismatch or an empty slot. The
    /// returned [`PendingFill`]'s `po` / `file_obj` are dropped by the caller
    /// **outside** `SCHED`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn take_pending_fill_matching(
        reg: *mut (),
        request_id: u64,
    ) -> Option<PendingFill> {
        let inner = unsafe { Self::inner(reg) };
        match &inner.pending_fill {
            Some(p) if p.request_id == request_id => inner.pending_fill.take(),
            _ => None,
        }
    }

    /// Take the outstanding pending fill unconditionally (origination rollback /
    /// dead-peer fail). `None` if empty. The returned references drop **outside**
    /// `SCHED`.
    ///
    /// # Safety
    /// See the accessor contract above.
    pub(crate) unsafe fn take_pending_fill_any(reg: *mut ()) -> Option<PendingFill> {
        unsafe { Self::inner(reg) }.pending_fill.take()
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
// runs outside `SCHED`), any pending lookup `po`, and any pending fill's `po` /
// `file_obj`. A lookup or fill still pending at teardown simply never completes
// (the binding is going away — typically with its client).

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
        let id0 = unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 5, Rights::MAP_READ, b"sys/gen") };
        assert_eq!(id0, Some(1));
        // Second begin while one is outstanding → busy (N = 1).
        let id1 = unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 5, Rights::MAP_READ, b"sys/gen") };
        assert_eq!(id1, None);
        // Take it; the next begin gets the next id.
        let taken = unsafe { UserspaceServerReg::take_pending_any(r.as_ptr()) };
        assert_eq!(taken.as_ref().map(|p| p.request_id), Some(1));
        drop(taken);
        let id2 = unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 9, Rights::MAP_READ, b"sys/gen") };
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
        let id = unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 7, Rights::MAP_READ, b"sys/gen") }.unwrap();
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

    /// A `FileObject` adopted into an `ObjectRef` (the form a `PendingFill` holds).
    fn file_obj() -> ObjectRef {
        use crate::object::{FileObject, Producer};
        let f = FileObject::try_new(4096, Producer::Stub { base: 0 }).unwrap();
        // SAFETY: `into_raw` yields the single creation reference; adopt it.
        unsafe {
            ObjectRef::from_raw(KBox::into_raw(f).as_ptr() as *mut (), KObjectType::FileObject)
        }
    }

    #[test]
    fn begin_records_the_lookup_suffix() {
        init_global_heap();
        let r = reg();
        let po = make_po();
        unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 3, Rights::MAP_READ, b"a/b/c") };
        let taken = unsafe { UserspaceServerReg::take_pending_any(r.as_ptr()) }.unwrap();
        assert_eq!(taken.suffix(), Some(&b"a/b/c"[..]));
        drop(taken);
        drop(po);
        drop(r);
    }

    #[test]
    fn fill_slot_is_independent_and_correlates_by_id() {
        init_global_heap();
        let r = reg();
        let po = make_po();
        let fo = file_obj();
        let frame = PhysAddr::new(0x5000);
        // A lookup and a fill can coexist (separate slots, shared id counter).
        let lid = unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 1, Rights::MAP_READ, b"x") }
            .unwrap();
        let fid =
            unsafe { UserspaceServerReg::begin_fill(r.as_ptr(), &po, &fo, frame, 2) }.unwrap();
        assert_ne!(lid, fid); // distinct request ids
        // A second fill while one is outstanding → busy (N = 1).
        assert!(unsafe { UserspaceServerReg::begin_fill(r.as_ptr(), &po, &fo, frame, 3) }.is_none());
        // A mismatched id leaves the fill in place; the matching id takes it.
        assert!(
            unsafe { UserspaceServerReg::take_pending_fill_matching(r.as_ptr(), fid ^ 0xFF) }
                .is_none()
        );
        let pf =
            unsafe { UserspaceServerReg::take_pending_fill_matching(r.as_ptr(), fid) }.unwrap();
        assert_eq!(pf.request_id, fid);
        assert_eq!(pf.frame, frame);
        assert_eq!(pf.index, 2);
        // The lookup slot is untouched by fill operations.
        let pl = unsafe { UserspaceServerReg::take_pending_any(r.as_ptr()) }.unwrap();
        assert_eq!(pl.request_id, lid);
        drop(pf);
        drop(pl);
        drop(fo);
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
        unsafe { UserspaceServerReg::begin(r.as_ptr(), &po, 1, Rights::MAP_READ, b"sys/gen") };
        assert_eq!(test_probe::pending_op_destroys(), 0);
        drop(r); // releases the pending entry's PO clone
        assert_eq!(test_probe::pending_op_destroys(), 0, "creation ref still held by `po`");
        drop(po); // last ref → PO destroyed
        assert_eq!(test_probe::pending_op_destroys(), 1);
    }
}
