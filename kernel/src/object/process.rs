//! The [`Process`] kernel object.
//!
//! An address space (optional: [`try_new`](Process::try_new) builds none, for a `Process`
//! needed only as a refcounted object), a notification channel for fault notifications
//! (`docs/architecture/notifications.md`), a root namespace and ambient syscaps. See
//! `docs/architecture/overview.md`.

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, ExitStatus, KBox, SysCaps};
use crate::mm::PhysAddr;
use crate::mm::addr_space::AddressSpace;
use crate::object::ObjectRef;
use crate::object::header::KObjectHeader;
use core::sync::atomic::{AtomicU64, Ordering};

/// A process kernel object (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct Process {
    header: KObjectHeader,
    pid: u32,
    /// Self-check sentinel; a live `Process` always reads [`Process::MAGIC`].
    magic: u64,
    /// Owned: dropped with the `Process` on the last `ObjectRef` release (`dispatch_destroy`),
    /// tearing down the VMAs and freeing the top-level page table.
    address_space: Option<AddressSpace>,
    /// The notification channel the exception path delivers fault notifications to. The channel
    /// does **not** back-reference the `Process`, so there is no refcount cycle.
    notification_channel: Option<ObjectRef>,
    /// The **parent's** notification channel, where `ChildExited` is delivered when this process
    /// exits; `None` for the boot parent. A held `ObjectRef`, so it outlives this child; no cycle.
    parent_notif: Option<ObjectRef>,
    /// This process's **root namespace** (`docs/architecture/namespace-and-resource-servers.md`);
    /// owned, so it outlives the process's handle to it. `None` until attached. No cycle.
    namespace: Option<ObjectRef>,
    /// **Ambient system capabilities** (`docs/architecture/syscaps.md`): granted at spawn
    /// (`child = parent & args.syscaps`, never amplified), **immutable**, checked at the boundary.
    syscaps: SysCaps,
    /// `0` while the process runs; once a thread has asked it to exit, that exit's status, packed
    /// by [`pack`]. Its other threads end themselves, and the last out reports this status.
    ending: AtomicU64,
}

const ENDING: u64 = 1 << 63;

fn pack(status: ExitStatus) -> u64 {
    ENDING | (u64::from(status.kind) & 0x7fff_ffff) << 32 | u64::from(status.code as u32)
}

impl Process {
    /// Sentinel written into [`Process::magic`] at construction.
    pub const MAGIC: u64 = 0x5072_6f63_4f62_6a21; // "ProcObj!"

    /// Allocate a process object with **no** address space, refcount one.
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
            ending: AtomicU64::new(0),
        })
    }

    /// Allocate a userspace process around an already-populated address space, holding
    /// `syscaps`: init gets [`SysCaps::all`], a spawned child `parent.syscaps & args.syscaps`.
    pub fn try_new_user(pid: u32, address_space: AddressSpace, syscaps: SysCaps) -> Result<KBox<Self>, AllocError> {
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Process),
            pid,
            magic: Self::MAGIC,
            address_space: Some(address_space),
            notification_channel: None,
            parent_notif: None,
            namespace: None,
            syscaps,
            ending: AtomicU64::new(0),
        })
    }

    /// This process's ambient system capabilities. Immutable after construction.
    pub fn syscaps(&self) -> SysCaps {
        self.syscaps
    }

    /// Record that the process is exiting with `status`, unless a thread asked first, and answer
    /// the status it exits with.
    pub fn begin_ending(&self, status: ExitStatus) -> ExitStatus {
        let _ = self.ending.compare_exchange(0, pack(status), Ordering::AcqRel, Ordering::Acquire);
        self.ending().unwrap_or(status)
    }

    /// The status the process is exiting with, once a thread has asked it to.
    pub fn ending(&self) -> Option<ExitStatus> {
        let v = self.ending.load(Ordering::Acquire);
        (v != 0).then_some(ExitStatus { kind: (v >> 32) as u32 & 0x7fff_ffff, code: v as u32 as i32 })
    }

    /// The process identifier.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    /// Physical root of the address space (the page-table root a thread loads), if any.
    pub fn address_space_root(&self) -> Option<PhysAddr> {
        self.address_space.as_ref().map(|a| a.root())
    }

    /// This process's address space, if any, reached through an `ObjectRef` without ownership.
    pub fn address_space(&self) -> Option<&AddressSpace> {
        self.address_space.as_ref()
    }

    /// Attach the notification channel; the box is not yet shared, so `&mut self` is exclusive.
    pub fn set_notification_channel(&mut self, chan: ObjectRef) {
        self.notification_channel = Some(chan);
    }

    /// The type-erased pointer to the attached channel. The fault path must **not** clone/drop an
    /// `ObjectRef` under the scheduler lock, so it borrows this; the channel stays alive because
    /// this `Process` owns a reference to it.
    pub fn notification_channel_ptr(&self) -> Option<*mut ()> {
        self.notification_channel.as_ref().map(|r| r.as_ptr())
    }

    /// Clone the channel reference; spawn gives it to a child as its `parent_notif`.
    pub fn notification_channel_ref(&self) -> Option<ObjectRef> {
        self.notification_channel.clone()
    }

    /// Attach the root [`Namespace`](crate::object::Namespace); the box is not yet shared, so
    /// `&mut self` is exclusive.
    pub fn set_namespace(&mut self, ns: ObjectRef) {
        self.namespace = Some(ns);
    }

    /// Clone the root-namespace reference; spawn gives a child its inherited namespace this way.
    pub fn namespace_ref(&self) -> Option<ObjectRef> {
        self.namespace.clone()
    }

    /// Attach the **parent's** notification channel at spawn; the boot parent never sets it.
    pub fn set_parent_notif(&mut self, chan: ObjectRef) {
        self.parent_notif = Some(chan);
    }

    /// The type-erased pointer to the parent's channel, borrowed by the reap path to enqueue
    /// `ChildExited`; like [`notification_channel_ptr`](Self::notification_channel_ptr), it must
    /// not clone/drop an `ObjectRef` under the scheduler lock.
    pub fn parent_notif_ptr(&self) -> Option<*mut ()> {
        self.parent_notif.as_ref().map(|r| r.as_ptr())
    }

    /// This process's pid, the `child` of the `ChildExited` the reap path builds.
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
    fn the_first_exit_asked_for_is_the_one_a_process_ends_with() {
        init_global_heap();
        let p = Process::try_new(7).unwrap();
        assert_eq!(p.ending(), None);
        let first = ExitStatus { kind: 2, code: -11 };
        assert_eq!(p.begin_ending(first), first);
        assert_eq!(p.begin_ending(ExitStatus { kind: 0, code: 3 }), first);
        assert_eq!(p.ending(), Some(first));
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
        let chan = KBox::into_raw(NotificationChannel::try_new().unwrap()).as_ptr() as *mut ();
        // SAFETY: into_raw yielded the single creation ref; adopt it.
        let chan_ref = unsafe { ObjectRef::from_raw(chan, KObjectType::NotificationChannel) };
        p.set_notification_channel(chan_ref);
        assert_eq!(p.notification_channel_ptr(), Some(chan));
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
        let ns = KBox::into_raw(Namespace::try_new().unwrap()).as_ptr() as *mut ();
        // SAFETY: into_raw yielded the single creation ref; adopt it.
        let ns_ref = unsafe { ObjectRef::from_raw(ns, KObjectType::Namespace) };
        p.set_namespace(ns_ref);
        let cloned = p.namespace_ref().expect("namespace attached");
        assert_eq!(cloned.as_ptr(), ns);
        drop(cloned);
        assert_eq!(test_probe::namespace_destroys(), 0);
        drop(p);
        assert_eq!(test_probe::namespace_destroys(), 1);
    }

    #[test]
    fn drop_tears_down_the_address_space() {
        init_global_heap();
        for pid in 0..16u32 {
            let asp = AddressSpace::new().unwrap();
            let _ = Process::try_new_user(pid, asp, crate::libkern::SysCaps::empty()).unwrap();
        }
    }
}
