//! The [`Process`] kernel object.
//!
//! A process is conceptually an address space, a namespace handle, a
//! current-working-directory handle, a list of owned handles, a syscap
//! bitmask, and a set of threads (see `docs/architecture/overview.md`).
//! This slice adds the **address space** (a userspace process needs one to
//! run in ring 3) and an optional **notification channel** (the kernel delivers
//! fault notifications here; see `docs/architecture/notifications.md`); the
//! namespace, handle table, syscaps, and thread set arrive with their
//! respective later slices.
//!
//! The address space is optional: [`try_new`](Process::try_new) builds a
//! process with none (used where a `Process` is needed only as a
//! refcounted kernel object — e.g. handle-table tests), while
//! [`try_new_user`](Process::try_new_user) builds one around an
//! already-populated [`AddressSpace`] (the ELF loader fills it first).

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, SysCaps};
use crate::mm::PhysAddr;
use crate::mm::addr_space::AddressSpace;
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;

/// A process kernel object.
///
/// `#[repr(C)]` with [`KObjectHeader`] first so the type-erased object
/// pointer in a handle entry can be read as `*const KObjectHeader` at
/// offset 0 — see [`crate::object::header`].
#[repr(C)]
pub struct Process {
    header: KObjectHeader,
    pid: u32,
    /// Self-check sentinel. A live `Process` always reads
    /// [`Process::MAGIC`] here; a use-after-free reads freed or reused
    /// memory. Used by the concurrency torture tests; cheap enough to
    /// keep unconditionally as a defensive tripwire.
    magic: u64,
    /// The process's virtual address space, if it has one. Owned: dropped
    /// with the `Process` (which fires on the last `ObjectRef` release —
    /// see `dispatch_destroy` in [`crate::object::header`]), tearing down
    /// the VMAs and freeing the top-level page table.
    address_space: Option<AddressSpace>,
    /// This process's notification channel, if one is attached. The process
    /// owns this reference; a supervisor may hold another. The channel does
    /// **not** back-reference the `Process`, so there is no refcount cycle.
    /// The exception path delivers fault notifications here.
    notification_channel: Option<ObjectRef>,
    /// The **parent's** notification channel, where this process's
    /// `ChildExited` is delivered when it exits. `None` for the boot parent
    /// (the root has no parent). A held `ObjectRef`, so the parent's channel
    /// outlives this child; no cycle (the parent's channel does not reference
    /// the child).
    parent_notif: Option<ObjectRef>,
    /// This process's **root namespace** — the `Namespace` it resolves names
    /// against (`docs/architecture/namespace-and-resource-servers.md`). The
    /// process owns this reference, so the namespace outlives the handle the
    /// process holds to it; `None` until one is attached (the boot root is set
    /// at pid-1 construction; a child's is derived from its parent's at spawn —
    /// Part D). No cycle: the namespace does not back-reference the `Process`.
    namespace: Option<ObjectRef>,
    /// This process's **ambient system capabilities** — the second axis of authority
    /// (`docs/architecture/syscaps.md`), distinct from per-handle `Rights`. Granted at
    /// spawn (`child = parent & args.syscaps`, never amplified) and **immutable**
    /// thereafter; init boots with the full set. Checked at the syscall boundary
    /// (`require_syscap`).
    syscaps: SysCaps,
}

impl Process {
    /// Sentinel written into [`Process::magic`] at construction.
    pub const MAGIC: u64 = 0x5072_6f63_4f62_6a21; // "ProcObj!"

    /// Allocate a process object with **no** address space, refcount one.
    /// For uses that need only a refcounted `Process` kernel object.
    pub fn try_new(pid: u32) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Process),
            pid,
            magic: Self::MAGIC,
            address_space: None,
            notification_channel: None,
            parent_notif: None,
            namespace: None,
            syscaps: SysCaps::empty(),
        })
    }

    /// Allocate a userspace process around an already-populated address
    /// space (the ELF loader fills it before this is called), holding the ambient
    /// `syscaps`. Refcount one. init is created with [`SysCaps::all`] (the boot grant);
    /// a spawned child with `parent.syscaps & args.syscaps`.
    pub fn try_new_user(
        pid: u32,
        address_space: AddressSpace,
        syscaps: SysCaps,
    ) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Process),
            pid,
            magic: Self::MAGIC,
            address_space: Some(address_space),
            notification_channel: None,
            parent_notif: None,
            namespace: None,
            syscaps,
        })
    }

    /// This process's ambient system capabilities. Immutable after construction.
    pub fn syscaps(&self) -> SysCaps {
        self.syscaps
    }

    /// The process identifier.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Physical root of this process's address space — the value a thread
    /// of this process loads as the page-table root when it runs. `None`
    /// for a process created without an address space.
    pub fn address_space_root(&self) -> Option<PhysAddr> {
        self.address_space.as_ref().map(|a| a.root())
    }

    /// This process's address space, if it has one. Lets a syscall reach the
    /// interior-mutable [`AddressSpace`] through an [`ObjectRef`] (e.g. to
    /// map a memory object) without taking ownership.
    ///
    /// [`ObjectRef`]: crate::object::ObjectRef
    pub fn address_space(&self) -> Option<&AddressSpace> {
        self.address_space.as_ref()
    }

    /// Attach this process's notification channel (the process takes ownership
    /// of `chan`). Called on the `KBox<Process>` before it is wrapped into an
    /// `ObjectRef`, so `&mut self` is exclusive.
    pub fn set_notification_channel(&mut self, chan: ObjectRef) {
        self.notification_channel = Some(chan);
    }

    /// The type-erased pointer to the attached notification channel, or `None`.
    /// For the exception fault path, which must **not** clone/drop an
    /// `ObjectRef` under the scheduler lock — it borrows the pointer, and the
    /// channel stays alive because this `Process` owns a reference to it.
    pub fn notification_channel_ptr(&self) -> Option<*mut ()> {
        self.notification_channel.as_ref().map(|r| r.as_ptr())
    }

    /// Clone this process's notification-channel reference (bumping its
    /// refcount), or `None` if it has none. `sys_process_spawn` uses this to
    /// give a child a held reference to its **parent's** channel (the child's
    /// `parent_notif`), so the parent's channel outlives the child and receives
    /// its `ChildExited`.
    pub fn notification_channel_ref(&self) -> Option<ObjectRef> {
        self.notification_channel.clone()
    }

    /// Attach this process's root [`Namespace`](crate::object::Namespace) (the
    /// process takes ownership of `ns`). Called on the `KBox<Process>` before it
    /// is wrapped into an `ObjectRef`, so `&mut self` is exclusive. The boot code
    /// sets pid 1's root here; child inheritance (Part D) sets a derived one.
    pub fn set_namespace(&mut self, ns: ObjectRef) {
        self.namespace = Some(ns);
    }

    /// Clone this process's root-namespace reference (bumping its refcount), or
    /// `None` if it has none. Used by spawn (Part D) to give a child a reference
    /// to its inherited namespace, so the namespace outlives the parent.
    pub fn namespace_ref(&self) -> Option<ObjectRef> {
        self.namespace.clone()
    }

    /// Attach the **parent's** notification channel (where this process's
    /// `ChildExited` is delivered on exit). Called on the `KBox<Process>` at
    /// spawn, before it is wrapped into an `ObjectRef`. The boot parent never
    /// sets this (it is the root).
    pub fn set_parent_notif(&mut self, chan: ObjectRef) {
        self.parent_notif = Some(chan);
    }

    /// The type-erased pointer to the parent's notification channel, or `None`.
    /// The reap path borrows this to enqueue `ChildExited` — like
    /// [`notification_channel_ptr`](Self::notification_channel_ptr), it must not
    /// clone/drop an `ObjectRef` under the scheduler lock; the parent's channel
    /// stays alive because this `Process` holds a reference to it.
    pub fn parent_notif_ptr(&self) -> Option<*mut ()> {
        self.parent_notif.as_ref().map(|r| r.as_ptr())
    }

    /// This process's pid as a `u32` — the `child` field of a `ChildExited`
    /// notification the reap path builds for the parent.
    pub fn pid_u32(&self) -> u32 {
        self.pid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::addr_space::AddressSpace;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn try_new_has_no_address_space() {
        init_global_heap();
        let p = Process::try_new(7).unwrap();
        assert_eq!(p.pid(), 7);
        assert!(p.magic_ok());
        assert!(p.address_space_root().is_none());
    }

    #[test]
    fn try_new_user_carries_a_loadable_address_space() {
        init_global_heap();
        let asp = AddressSpace::new().unwrap();
        let root = asp.root();
        let p = Process::try_new_user(1, asp, crate::libkern::SysCaps::empty()).unwrap();
        assert_eq!(p.pid(), 1);
        assert!(p.magic_ok());
        assert_eq!(p.address_space_root(), Some(root));
        assert!(root.is_page_aligned() && root.as_u64() != 0);
    }

    #[test]
    fn notification_channel_attach_and_read_ptr() {
        use crate::object::NotificationChannel;
        use crate::object::header::test_probe;
        init_global_heap();
        test_probe::reset();
        let mut p = Process::try_new(1).unwrap();
        assert!(p.notification_channel_ptr().is_none());
        // Attach a channel; the Process takes one ref.
        let chan = KBox::into_raw(NotificationChannel::try_new().unwrap()).as_ptr() as *mut ();
        // SAFETY: into_raw yielded the single creation ref; adopt it.
        let chan_ref = unsafe { ObjectRef::from_raw(chan, KObjectType::NotificationChannel) };
        p.set_notification_channel(chan_ref);
        assert_eq!(p.notification_channel_ptr(), Some(chan));
        // Dropping the Process releases its channel ref exactly once.
        assert_eq!(test_probe::notification_channel_destroys(), 0);
        drop(p);
        assert_eq!(test_probe::notification_channel_destroys(), 1);
    }

    #[test]
    fn namespace_attach_is_owned_and_released_on_drop() {
        use crate::object::Namespace;
        use crate::object::header::test_probe;
        init_global_heap();
        test_probe::reset();
        let mut p = Process::try_new(1).unwrap();
        assert!(p.namespace_ref().is_none());
        // Attach a root namespace; the Process takes one ref.
        let ns = KBox::into_raw(Namespace::try_new().unwrap()).as_ptr() as *mut ();
        // SAFETY: into_raw yielded the single creation ref; adopt it.
        let ns_ref = unsafe { ObjectRef::from_raw(ns, KObjectType::Namespace) };
        p.set_namespace(ns_ref);
        // `namespace_ref` clones it (refcount → 2); drop that extra clone.
        let cloned = p.namespace_ref().expect("namespace attached");
        assert_eq!(cloned.as_ptr(), ns);
        drop(cloned);
        // The Process still owns its ref, so nothing is destroyed yet.
        assert_eq!(test_probe::namespace_destroys(), 0);
        drop(p);
        assert_eq!(test_probe::namespace_destroys(), 1);
    }

    #[test]
    fn drop_tears_down_the_address_space() {
        // Build + drop a process-with-AS repeatedly; a leak of the PML4 or
        // leaf frames would exhaust the 16 MiB test heap over many rounds.
        init_global_heap();
        for pid in 0..16u32 {
            let asp = AddressSpace::new().unwrap();
            let _ = Process::try_new_user(pid, asp, crate::libkern::SysCaps::empty()).unwrap();
            // Process (and its AddressSpace) dropped at end of iteration.
        }
    }
}
