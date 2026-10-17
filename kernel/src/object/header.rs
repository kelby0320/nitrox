//! [`KObjectHeader`] and [`ObjectRef`] — the reference-counting substrate
//! every kernel object is built on.
//!
//! Each kernel object is `#[repr(C)]` with a [`KObjectHeader`] as its **first field**, so a
//! type-erased `*mut ()` from the handle table can be read as `*const KObjectHeader` without
//! knowing the concrete type (see `docs/architecture/overview.md` § "Kernel objects" and
//! `docs/archive/os-design-v5.1.md` § "Object Header and Dispatch"). The concrete type is
//! needed only to run the right destructor on the last release, dispatched via `match` on
//! [`KObjectType`](crate::libkern::handle::KObjectType), never `dyn` (`kernel/CLAUDE.md`
//! § "Kernel object dispatch").
//!
//! ## Ownership model
//!
//! Each live handle-table entry owns exactly one refcount; each [`ObjectRef`] owns exactly one:
//! `refcount == (live handles to O) + (live ObjectRefs to O)`. The count reaches zero exactly
//! once, firing exactly one destroy. An `ObjectRef` returned by `lookup` pins the object
//! across the `lookup`→`allocate` gap, closing the `HandleTable::duplicate` TOCTOU described
//! in `docs/architecture/handle-system.md`.
//!
//! ## Memory ordering
//!
//! Standard `Arc` discipline: increments are `Relaxed` (the pointer being cloned already
//! established happens-before), the decrement is `Release`, and the thread that sees the
//! count reach zero issues an `Acquire` fence before destroying.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering, fence};

use crate::libkern::KBox;
use crate::libkern::handle::KObjectType;
use crate::object::{
    DeviceNode, EntropyObject, FileObject, InterruptObject, IpcChannel, MemoryObject, Namespace, NotificationChannel,
    PendingOperation, Process, SystemControl, Thread, Timer, UserspaceServerReg,
};

/// Upper bound on the refcount; exceeding it means ~2^62 leaked references, so abort rather
/// than wrap into a use-after-free (as `Arc` does with `isize::MAX`).
const MAX_REFCOUNT: usize = usize::MAX / 2;

/// The common header every kernel object begins with.
///
/// ABI-critical: its layout (`#[repr(C)]`, `AtomicUsize` then
/// `KObjectType`) contributes to the kernel ABI version hash — see
/// `docs/spec/abi-version-hash.md` § "KObjectHeader layout". Never
/// reorder the fields or change their types without bumping the hash.
#[repr(C)]
pub struct KObjectHeader {
    refcount: AtomicUsize,
    object_type: KObjectType,
}

impl KObjectHeader {
    /// A header for a fresh object, refcount one: the reference the creator holds.
    pub const fn new(object_type: KObjectType) -> Self {
        Self { refcount: AtomicUsize::new(1), object_type }
    }

    /// The object's type tag.
    pub fn object_type(&self) -> KObjectType {
        self.object_type
    }

    /// Add a reference unless the count is already zero (the object is being torn down), which
    /// makes a racing `lookup` safe against a concurrent last `close`.
    pub fn try_acquire(&self) -> bool {
        let mut cur = self.refcount.load(Ordering::Relaxed);
        loop {
            if cur == 0 {
                return false;
            }
            if cur > MAX_REFCOUNT {
                panic!("kobject refcount overflow");
            }
            match self.refcount.compare_exchange_weak(cur, cur + 1, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(actual) => cur = actual,
            }
        }
    }

    /// Add a reference unconditionally; sound only when the caller already holds one.
    pub fn bump(&self) {
        let old = self.refcount.fetch_add(1, Ordering::Relaxed);
        if old > MAX_REFCOUNT {
            panic!("kobject refcount overflow");
        }
    }

    /// Drop a reference; `true` iff it was the last, in which case the caller must issue an
    /// `Acquire` fence and run the destructor ([`dispatch_destroy`]), so the header knows nothing
    /// of concrete types.
    #[must_use]
    pub fn release(&self) -> bool {
        self.refcount.fetch_sub(1, Ordering::Release) == 1
    }

    /// The current refcount; racy outside a quiescent point.
    #[cfg(test)]
    pub(crate) fn refcount(&self) -> usize {
        self.refcount.load(Ordering::Relaxed)
    }
}

/// An owning reference to a kernel object: one refcount, released on drop.
///
/// The pointer is type-erased; the recorded type selects the destructor on the last release.
#[derive(Debug)]
pub struct ObjectRef {
    obj: NonNull<()>,
    object_type: KObjectType,
}

// SAFETY: the refcount is atomic, and the concrete objects behind the erased pointer keep
// their mutable state behind atomics or locks, so they are `Send + Sync`. Destruction runs
// only on the last release, after an `Acquire` fence ordering every other holder's final
// access before the drop.
unsafe impl Send for ObjectRef {}
// SAFETY: as `Send`.
unsafe impl Sync for ObjectRef {}

impl ObjectRef {
    /// Acquire a reference to the object at `obj`, or `None` if its count was already zero.
    ///
    /// # Safety
    /// `obj` must point at a live kernel object of type `ty` (header at
    /// offset 0). In the handle table this is guaranteed by `lookup`
    /// having observed `entry.object` non-null under a grace read-guard
    /// before this call (step 6 precedes step 7).
    pub unsafe fn try_acquire(obj: *mut (), ty: KObjectType) -> Option<ObjectRef> {
        // SAFETY: caller guarantees `obj` addresses a live object whose
        // first field is a `KObjectHeader`.
        let header = unsafe { &*(obj as *const KObjectHeader) };
        if header.try_acquire() {
            // SAFETY: `obj` is non-null per the caller's contract.
            Some(ObjectRef { obj: unsafe { NonNull::new_unchecked(obj) }, object_type: ty })
        } else {
            None
        }
    }

    /// The type-erased object pointer.
    pub fn as_ptr(&self) -> *mut () {
        self.obj.as_ptr()
    }

    /// The concrete type of the referenced object.
    pub fn object_type(&self) -> KObjectType {
        self.object_type
    }

    /// Consume the `ObjectRef`, yielding its raw `(pointer, type)` and handing the held reference
    /// **out** without decrementing; the recipient must account for it via
    /// [`ObjectRef::from_raw`] or a handle entry.
    pub fn into_raw(self) -> (*mut (), KObjectType) {
        let raw = (self.obj.as_ptr(), self.object_type);
        core::mem::forget(self);
        raw
    }

    /// Reconstruct an `ObjectRef` from a pair from [`into_raw`](ObjectRef::into_raw) or a handle
    /// entry; adopts that reference and does **not** increment.
    ///
    /// # Safety
    /// `ptr` must own exactly one outstanding reference to a live object
    /// of type `ty`, and that reference must not be reclaimed twice.
    pub unsafe fn from_raw(ptr: *mut (), ty: KObjectType) -> ObjectRef {
        debug_assert!(!ptr.is_null(), "ObjectRef::from_raw on null pointer");
        ObjectRef {
            // SAFETY: caller guarantees `ptr` is non-null and owns a ref.
            obj: unsafe { NonNull::new_unchecked(ptr) },
            object_type: ty,
        }
    }
}

impl Clone for ObjectRef {
    fn clone(&self) -> Self {
        // SAFETY: we already hold a reference, so the header is live and
        // its count is at least one; `bump` cannot resurrect a zero.
        let header = unsafe { &*(self.obj.as_ptr() as *const KObjectHeader) };
        header.bump();
        ObjectRef { obj: self.obj, object_type: self.object_type }
    }
}

impl Drop for ObjectRef {
    fn drop(&mut self) {
        // SAFETY: we hold a reference, so the header is live.
        let header = unsafe { &*(self.obj.as_ptr() as *const KObjectHeader) };
        if header.release() {
            // Acquire so every other holder's final writes are visible to the destructor.
            fence(Ordering::Acquire);
            // SAFETY: this was the last reference, so we have exclusive
            // access; `object_type` records the concrete type, and the
            // pointer originated from a `KBox::<T>::into_raw` for that T.
            unsafe { dispatch_destroy(self.obj.as_ptr(), self.object_type) };
        }
    }
}

/// Run the destructor for the kernel object at `ptr`: rebuild the owning `KBox` for its
/// concrete type and drop it, returning the storage to the slab. Dispatch is a `match` on the
/// tag (`kernel/CLAUDE.md` § "Kernel object dispatch").
///
/// # Safety
/// `ptr` must be the last live reference to an object of type `ty`,
/// originally produced by `KBox::<Concrete>::into_raw`.
unsafe fn dispatch_destroy(ptr: *mut (), ty: KObjectType) {
    match ty {
        KObjectType::Process => {
            // SAFETY: last ref to a `Process` produced by KBox::into_raw.
            drop(unsafe { KBox::<Process>::from_raw(NonNull::new_unchecked(ptr as *mut Process)) });
            #[cfg(test)]
            test_probe::note(KObjectType::Process);
        }
        KObjectType::Thread => {
            // SAFETY: last ref to a `Thread` produced by KBox::into_raw.
            drop(unsafe { KBox::<Thread>::from_raw(NonNull::new_unchecked(ptr as *mut Thread)) });
            #[cfg(test)]
            test_probe::note(KObjectType::Thread);
        }
        KObjectType::MemoryObject => {
            // SAFETY: last ref to a `MemoryObject` produced by KBox::into_raw.
            // Dropping the box runs `MemoryObject::Drop`, freeing its frames.
            drop(unsafe { KBox::<MemoryObject>::from_raw(NonNull::new_unchecked(ptr as *mut MemoryObject)) });
            #[cfg(test)]
            test_probe::note(KObjectType::MemoryObject);
        }
        KObjectType::Timer => {
            // SAFETY: last ref to a `Timer` produced by KBox::into_raw.
            drop(unsafe { KBox::<Timer>::from_raw(NonNull::new_unchecked(ptr as *mut Timer)) });
            #[cfg(test)]
            test_probe::note(KObjectType::Timer);
        }
        KObjectType::NotificationChannel => {
            // SAFETY: last ref to a `NotificationChannel` produced by KBox::into_raw.
            drop(unsafe {
                KBox::<NotificationChannel>::from_raw(NonNull::new_unchecked(ptr as *mut NotificationChannel))
            });
            #[cfg(test)]
            test_probe::note(KObjectType::NotificationChannel);
        }
        KObjectType::IpcChannel => {
            // SAFETY: last ref to an `IpcChannel` produced by KBox::into_raw.
            // Dropping the box runs `IpcChannel::Drop`, which unlinks the peer.
            drop(unsafe { KBox::<IpcChannel>::from_raw(NonNull::new_unchecked(ptr as *mut IpcChannel)) });
            #[cfg(test)]
            test_probe::note(KObjectType::IpcChannel);
        }
        KObjectType::PendingOperation => {
            // SAFETY: last ref to a `PendingOperation` produced by KBox::into_raw.
            drop(unsafe { KBox::<PendingOperation>::from_raw(NonNull::new_unchecked(ptr as *mut PendingOperation)) });
            #[cfg(test)]
            test_probe::note(KObjectType::PendingOperation);
        }
        KObjectType::Namespace => {
            // SAFETY: last ref to a `Namespace` produced by KBox::into_raw.
            // Dropping the box releases every binding's target `ObjectRef`.
            drop(unsafe { KBox::<Namespace>::from_raw(NonNull::new_unchecked(ptr as *mut Namespace)) });
            #[cfg(test)]
            test_probe::note(KObjectType::Namespace);
        }
        KObjectType::EntropyObject => {
            // SAFETY: last ref to an `EntropyObject` produced by KBox::into_raw.
            // The object owns nothing; the box drop frees its allocation.
            drop(unsafe { KBox::<EntropyObject>::from_raw(NonNull::new_unchecked(ptr as *mut EntropyObject)) });
            #[cfg(test)]
            test_probe::note(KObjectType::EntropyObject);
        }
        KObjectType::DeviceNode => {
            // SAFETY: last ref to a `DeviceNode` produced by KBox::into_raw.
            // The object owns nothing out-of-line; the box drop frees it.
            drop(unsafe { KBox::<DeviceNode>::from_raw(NonNull::new_unchecked(ptr as *mut DeviceNode)) });
            #[cfg(test)]
            test_probe::note(KObjectType::DeviceNode);
        }
        KObjectType::InterruptObject => {
            // SAFETY: last ref to an `InterruptObject` produced by KBox::into_raw.
            drop(unsafe { KBox::<InterruptObject>::from_raw(NonNull::new_unchecked(ptr as *mut InterruptObject)) });
            #[cfg(test)]
            test_probe::note(KObjectType::InterruptObject);
        }
        KObjectType::UserspaceServerReg => {
            // SAFETY: last ref to a `UserspaceServerReg` produced by KBox::into_raw.
            // Dropping the box releases its owned endpoint `ObjectRef` (whose
            // `IpcChannel::drop` unlinks the peer) and any pending lookup's PO.
            drop(unsafe {
                KBox::<UserspaceServerReg>::from_raw(NonNull::new_unchecked(ptr as *mut UserspaceServerReg))
            });
            #[cfg(test)]
            test_probe::note(KObjectType::UserspaceServerReg);
        }
        KObjectType::FileObject => {
            // SAFETY: last ref to a `FileObject` produced by KBox::into_raw.
            // Dropping the box runs `FileObject::Drop`, freeing its cached frames.
            drop(unsafe { KBox::<FileObject>::from_raw(NonNull::new_unchecked(ptr as *mut FileObject)) });
            #[cfg(test)]
            test_probe::note(KObjectType::FileObject);
        }
        KObjectType::SystemControl => {
            // SAFETY: last ref to a `SystemControl` produced by KBox::into_raw.
            // The object owns nothing; the box drop frees its allocation.
            drop(unsafe { KBox::<SystemControl>::from_raw(NonNull::new_unchecked(ptr as *mut SystemControl)) });
            #[cfg(test)]
            test_probe::note(KObjectType::SystemControl);
        }
        _ => debug_assert!(false, "dispatch_destroy on unimplemented kobject type {ty:?}"),
    }
}

/// Per-thread destructor-dispatch counters, so tests can verify which `match` arm fired.
/// Per-thread so the parallel test runner cannot mix counts; the destructor runs on whichever
/// thread drops the last reference, so multi-thread tests sum their workers' counts.
#[cfg(test)]
pub(crate) mod test_probe {
    use crate::libkern::handle::KObjectType;
    use core::cell::Cell;

    std::thread_local! {
        static PROCESS_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static THREAD_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static MEMORY_OBJECT_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static TIMER_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static NOTIFICATION_CHANNEL_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static IPC_CHANNEL_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static PENDING_OP_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static NAMESPACE_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static ENTROPY_OBJECT_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static DEVICE_NODE_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static INTERRUPT_OBJECT_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static USERSPACE_SERVER_REG_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static FILE_OBJECT_DESTROYS: Cell<usize> = const { Cell::new(0) };
        static SYSTEM_CONTROL_DESTROYS: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn note(ty: KObjectType) {
        match ty {
            KObjectType::Process => PROCESS_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::Thread => THREAD_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::MemoryObject => MEMORY_OBJECT_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::Timer => TIMER_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::NotificationChannel => NOTIFICATION_CHANNEL_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::IpcChannel => IPC_CHANNEL_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::PendingOperation => PENDING_OP_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::Namespace => NAMESPACE_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::EntropyObject => ENTROPY_OBJECT_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::DeviceNode => DEVICE_NODE_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::InterruptObject => INTERRUPT_OBJECT_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::UserspaceServerReg => USERSPACE_SERVER_REG_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::FileObject => FILE_OBJECT_DESTROYS.with(|c| c.set(c.get() + 1)),
            KObjectType::SystemControl => SYSTEM_CONTROL_DESTROYS.with(|c| c.set(c.get() + 1)),
            _ => {}
        }
    }

    pub(crate) fn process_destroys() -> usize {
        PROCESS_DESTROYS.with(Cell::get)
    }

    pub(crate) fn thread_destroys() -> usize {
        THREAD_DESTROYS.with(Cell::get)
    }

    pub(crate) fn memory_object_destroys() -> usize {
        MEMORY_OBJECT_DESTROYS.with(Cell::get)
    }

    pub(crate) fn timer_destroys() -> usize {
        TIMER_DESTROYS.with(Cell::get)
    }

    pub(crate) fn notification_channel_destroys() -> usize {
        NOTIFICATION_CHANNEL_DESTROYS.with(Cell::get)
    }

    pub(crate) fn ipc_channel_destroys() -> usize {
        IPC_CHANNEL_DESTROYS.with(Cell::get)
    }

    pub(crate) fn pending_op_destroys() -> usize {
        PENDING_OP_DESTROYS.with(Cell::get)
    }

    pub(crate) fn namespace_destroys() -> usize {
        NAMESPACE_DESTROYS.with(Cell::get)
    }

    pub(crate) fn entropy_object_destroys() -> usize {
        ENTROPY_OBJECT_DESTROYS.with(Cell::get)
    }

    pub(crate) fn system_control_destroys() -> usize {
        SYSTEM_CONTROL_DESTROYS.with(Cell::get)
    }

    pub(crate) fn device_node_destroys() -> usize {
        DEVICE_NODE_DESTROYS.with(Cell::get)
    }

    pub(crate) fn interrupt_object_destroys() -> usize {
        INTERRUPT_OBJECT_DESTROYS.with(Cell::get)
    }

    pub(crate) fn userspace_server_reg_destroys() -> usize {
        USERSPACE_SERVER_REG_DESTROYS.with(Cell::get)
    }

    pub(crate) fn file_object_destroys() -> usize {
        FILE_OBJECT_DESTROYS.with(Cell::get)
    }

    pub(crate) fn reset() {
        PROCESS_DESTROYS.with(|c| c.set(0));
        THREAD_DESTROYS.with(|c| c.set(0));
        MEMORY_OBJECT_DESTROYS.with(|c| c.set(0));
        TIMER_DESTROYS.with(|c| c.set(0));
        NOTIFICATION_CHANNEL_DESTROYS.with(|c| c.set(0));
        IPC_CHANNEL_DESTROYS.with(|c| c.set(0));
        PENDING_OP_DESTROYS.with(|c| c.set(0));
        NAMESPACE_DESTROYS.with(|c| c.set(0));
        ENTROPY_OBJECT_DESTROYS.with(|c| c.set(0));
        DEVICE_NODE_DESTROYS.with(|c| c.set(0));
        INTERRUPT_OBJECT_DESTROYS.with(|c| c.set(0));
        USERSPACE_SERVER_REG_DESTROYS.with(|c| c.set(0));
        FILE_OBJECT_DESTROYS.with(|c| c.set(0));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::{Process, Thread};

    fn fresh_process(pid: u32) -> *mut () {
        init_global_heap();
        KBox::into_raw(Process::try_new(pid).unwrap()).as_ptr() as *mut ()
    }

    fn fresh_thread(tid: u32, owner_pid: u32) -> *mut () {
        init_global_heap();
        KBox::into_raw(Thread::try_new(tid, owner_pid).unwrap()).as_ptr() as *mut ()
    }

    fn header_of(obj: *mut ()) -> &'static KObjectHeader {
        // SAFETY: each test object is a kernel object, its header at offset 0, freed only after its last read.
        unsafe { &*(obj as *const KObjectHeader) }
    }

    #[test]
    fn header_new_starts_refcount_one() {
        init_global_heap();
        let obj = fresh_process(7);
        assert_eq!(header_of(obj).refcount(), 1);
        assert_eq!(header_of(obj).object_type(), KObjectType::Process);
        // SAFETY: adopts the object's creation reference.
        drop(unsafe { ObjectRef::from_raw(obj, KObjectType::Process) });
    }

    #[test]
    fn try_acquire_increments_above_zero() {
        let obj = fresh_process(1);
        assert!(header_of(obj).try_acquire());
        assert_eq!(header_of(obj).refcount(), 2);
        assert!(!header_of(obj).release()); // 2 -> 1
        // SAFETY: adopts the object's creation reference.
        drop(unsafe { ObjectRef::from_raw(obj, KObjectType::Process) }); // 1 -> 0, destroy
    }

    #[test]
    fn try_acquire_fails_at_zero() {
        let obj = fresh_process(1);
        // Release the creation ref via the header: the count reaches zero without a destroy.
        assert!(header_of(obj).release()); // 1 -> 0
        assert!(!header_of(obj).try_acquire());
        assert_eq!(header_of(obj).refcount(), 0);
        // Free the storage by hand: the count is zero and nothing else references it.
        // SAFETY: the count is zero and nothing else holds the storage, which came from a `KBox`.
        drop(unsafe { KBox::<Process>::from_raw(NonNull::new_unchecked(obj as *mut Process)) });
    }

    #[test]
    fn release_reports_last_ref_exactly_once() {
        let obj = fresh_process(1);
        for _ in 0..3 {
            assert!(header_of(obj).try_acquire());
        }
        assert!(!header_of(obj).release());
        assert!(!header_of(obj).release());
        assert!(!header_of(obj).release());
        assert!(header_of(obj).release());
        // SAFETY: the count is zero and nothing else holds the storage, which came from a `KBox`.
        drop(unsafe { KBox::<Process>::from_raw(NonNull::new_unchecked(obj as *mut Process)) });
    }

    #[test]
    fn objectref_clone_then_drop_balances_and_destroys_once() {
        test_probe::reset();
        let obj = fresh_process(1);
        // SAFETY: adopts the object's creation reference.
        let r = unsafe { ObjectRef::from_raw(obj, KObjectType::Process) };
        let r2 = r.clone(); // count 2
        assert_eq!(header_of(obj).refcount(), 2);
        drop(r); // count 1, no destroy
        assert_eq!(test_probe::process_destroys(), 0);
        drop(r2); // count 0, destroy
        assert_eq!(test_probe::process_destroys(), 1);
    }

    #[test]
    fn drop_runs_correct_destructor_per_type() {
        test_probe::reset();
        // SAFETY: adopts the object's creation reference.
        let p = unsafe { ObjectRef::from_raw(fresh_process(1), KObjectType::Process) };
        // SAFETY: adopts the object's creation reference.
        let t = unsafe { ObjectRef::from_raw(fresh_thread(1, 1), KObjectType::Thread) };
        drop(p);
        assert_eq!(test_probe::process_destroys(), 1);
        assert_eq!(test_probe::thread_destroys(), 0, "wrong arm ran for Process");
        drop(t);
        assert_eq!(test_probe::process_destroys(), 1);
        assert_eq!(test_probe::thread_destroys(), 1, "Thread arm did not run");
    }

    #[test]
    fn into_raw_then_from_raw_no_double_decrement() {
        test_probe::reset();
        // SAFETY: adopts the object's creation reference.
        let r = unsafe { ObjectRef::from_raw(fresh_process(1), KObjectType::Process) };
        let (ptr, ty) = r.into_raw(); // forget, no decrement
        assert_eq!(header_of(ptr).refcount(), 1);
        assert_eq!(test_probe::process_destroys(), 0);
        // SAFETY: re-adopts the reference `into_raw` gave up.
        let r2 = unsafe { ObjectRef::from_raw(ptr, ty) }; // adopt, no increment
        assert_eq!(header_of(ptr).refcount(), 1);
        drop(r2); // count 0, destroy once
        assert_eq!(test_probe::process_destroys(), 1);
    }

    #[test]
    #[should_panic(expected = "refcount overflow")]
    fn refcount_overflow_guard_panics() {
        let obj = fresh_process(1);
        header_of(obj).refcount.store(MAX_REFCOUNT + 1, Ordering::Relaxed);
        header_of(obj).bump();
    }
}
