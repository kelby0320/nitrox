//! [`KObjectHeader`], [`ObjectRef`] and [`Ref`] — the reference-counting substrate every kernel
//! object is built on, and the types that tie an object's tag to its pointer.
//!
//! Each kernel object type is a [`KObject`], whose **first field** is a [`Header<Self>`], so a
//! type-erased `*mut ()` from the handle table can be read as `*const KObjectHeader` without
//! knowing the concrete type (see `docs/architecture/overview.md` § "Kernel objects" and
//! `docs/archive/os-design-v5.1.md` § "Object Header and Dispatch"). The header's tag is the
//! type's [`KObject::TYPE`], fixed by the header's own type, and an [`ObjectRef`] reads it there.
//! The concrete type is needed only to downcast and to run the right destructor on the last
//! release, dispatched via `match` on [`KObjectType`](crate::libkern::handle::KObjectType), never
//! `dyn` (`kernel/CLAUDE.md` § "Kernel object dispatch").
//!
//! ## Ownership model
//!
//! Each live handle-table entry owns exactly one refcount; each [`ObjectRef`] or [`Ref`] owns
//! exactly one: `refcount == (live handles to O) + (live references to O)`. The count reaches
//! zero exactly once, firing exactly one destroy. An `ObjectRef` returned by `lookup` pins the
//! object across the `lookup`→`allocate` gap, closing the `HandleTable::duplicate` TOCTOU
//! described in `docs/architecture/handle-system.md`.
//!
//! ## Memory ordering
//!
//! Standard `Arc` discipline: increments are `Relaxed` (the pointer being cloned already
//! established happens-before), the decrement is `Release`, and the thread that sees the
//! count reach zero issues an `Acquire` fence before destroying.

use core::marker::PhantomData;
use core::ops::Deref;
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

/// A kernel object type, every one of whose headers is tagged [`TYPE`](Self::TYPE).
///
/// Implemented by [`kobject!`] beside each type, which asserts what the compiler can check of it.
///
/// # Safety
/// `Self`'s field at offset 0 is a [`Header<Self>`], and `Self` is the type [`dispatch_destroy`]
/// frees an object tagged `TYPE` as: no other type has the tag.
pub unsafe trait KObject: Send + Sync + Sized {
    /// The tag of every object of this type.
    const TYPE: KObjectType;
}

/// A kernel object that a shared `&Self` may be formed to while a reference pins it.
///
/// # Safety
/// Once an object of this type is shared, nothing writes its fields except through a cell, an
/// atomic or a lock, so no `&Self` overlaps a write made through a raw pointer.
pub unsafe trait Shared: KObject {}

/// Make `$t` a [`KObject`] tagged `KObjectType::$t`, asserting at compile time that it is
/// `crate::object::$t`, the type [`dispatch_destroy`] frees for that tag, and that its field at
/// offset 0 is its `header`, a [`Header<$t>`].
macro_rules! kobject {
    ($t:ident) => {
        // SAFETY: the assertions below make `$t` the `crate::object::$t` that `dispatch_destroy`
        // frees for `KObjectType::$t`, so no other type takes the tag, and put a `Header<$t>` at
        // offset 0.
        unsafe impl $crate::object::header::KObject for $t {
            const TYPE: $crate::libkern::handle::KObjectType = $crate::libkern::handle::KObjectType::$t;
        }
        const _: fn(&$t) -> &$crate::object::$t = |object| object;
        const _: () = assert!(::core::mem::offset_of!($t, header) == 0);
        const _: fn(&$t) -> &$crate::object::header::Header<$t> = |object| &object.header;
    };
}
pub(crate) use kobject;

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
    const fn new(object_type: KObjectType) -> Self {
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

/// The [`KObjectHeader`] of a `T`, whose tag its type sets to `T::TYPE`, so no constructor
/// writes one.
#[repr(transparent)]
pub struct Header<T> {
    raw: KObjectHeader,
    _type: PhantomData<fn() -> T>,
}

impl<T: KObject> Header<T> {
    /// A header for a fresh `T`, refcount one: the reference its creator holds.
    #[allow(clippy::new_without_default, reason = "a header is made once per object, as its first field")]
    pub const fn new() -> Self {
        Self { raw: KObjectHeader::new(T::TYPE), _type: PhantomData }
    }
}

/// An owning reference to a kernel object of any type: one refcount, released on drop.
///
/// Its tag is the object's header's, so the destructor its last release runs is the one the
/// object's type fixed. [`downcast_ref`](Self::downcast_ref) and [`downcast`](Self::downcast)
/// check it against a type.
#[derive(Debug)]
pub struct ObjectRef {
    header: NonNull<KObjectHeader>,
}

// SAFETY: the refcount is atomic, and the object behind the pointer is `Send + Sync`, as every
// `KObject` is: `new` takes only a `KObject`'s box, and `from_raw` and `try_acquire` a pointer to
// a kernel object. Destruction runs only on the last release, after an `Acquire` fence ordering
// every other holder's final access before the drop.
unsafe impl Send for ObjectRef {}
// SAFETY: as `Send`.
unsafe impl Sync for ObjectRef {}

impl ObjectRef {
    /// Adopt a fresh object's box as the reference its creator holds.
    pub fn new<T: KObject>(object: KBox<T>) -> Self {
        Self { header: KBox::into_raw(object).cast() }
    }

    /// Acquire a reference to the object at `obj`, or `None` if its count was already zero.
    ///
    /// # Safety
    /// `obj` must point at a live kernel object. In the handle table this is guaranteed by
    /// `lookup` having observed `entry.object` non-null under a grace read-guard before this call
    /// (step 6 precedes step 7).
    pub unsafe fn try_acquire(obj: *mut ()) -> Option<ObjectRef> {
        // SAFETY: `obj` addresses a live kernel object, so it is non-null, and its header is at
        // offset 0. The pointer is `obj` itself, whose provenance spans the whole object, not
        // the header a reference to it would cover.
        let header = unsafe { NonNull::new_unchecked(obj.cast::<KObjectHeader>()) };
        // SAFETY: as above.
        if unsafe { header.as_ref() }.try_acquire() { Some(ObjectRef { header }) } else { None }
    }

    fn header(&self) -> &KObjectHeader {
        // SAFETY: the reference this holds keeps the object, and so its header, live.
        unsafe { self.header.as_ref() }
    }

    /// The type-erased object pointer.
    pub fn as_ptr(&self) -> *mut () {
        self.header.as_ptr().cast()
    }

    /// The concrete type of the referenced object, read from its header.
    pub fn object_type(&self) -> KObjectType {
        self.header().object_type()
    }

    /// `true` iff the object is a `T`.
    pub fn is<T: KObject>(&self) -> bool {
        self.object_type() == T::TYPE
    }

    /// The object as a `T`, if its header says it is one; borrowed for as long as this reference.
    pub fn downcast_ref<T: Shared>(&self) -> Option<&T> {
        if !self.is::<T>() {
            return None;
        }
        // SAFETY: the header says the object is a `T`, which `KObject` places at the header's
        // address; this reference keeps it live for the borrow, and `T: Shared` lets it be shared.
        Some(unsafe { self.header.cast::<T>().as_ref() })
    }

    /// This reference as a [`Ref<T>`], if the object's header says it is a `T`; else itself back.
    pub fn downcast<T: KObject>(self) -> Result<Ref<T>, ObjectRef> {
        if self.is::<T>() { Ok(Ref { object: self, _type: PhantomData }) } else { Err(self) }
    }

    /// Consume the `ObjectRef`, yielding its raw `(pointer, type)` and handing the held reference
    /// **out** without decrementing; the recipient must account for it via
    /// [`ObjectRef::from_raw`] or a handle entry.
    pub fn into_raw(self) -> (*mut (), KObjectType) {
        let raw = (self.as_ptr(), self.object_type());
        core::mem::forget(self);
        raw
    }

    /// Reconstruct an `ObjectRef` from a pointer from [`into_raw`](ObjectRef::into_raw) or a
    /// handle entry; adopts that reference and does **not** increment.
    ///
    /// # Safety
    /// `ptr` must own exactly one outstanding reference to a live kernel object, and that
    /// reference must not be reclaimed twice.
    pub unsafe fn from_raw(ptr: *mut ()) -> ObjectRef {
        debug_assert!(!ptr.is_null(), "ObjectRef::from_raw on null pointer");
        // SAFETY: caller guarantees `ptr` is non-null and owns a ref.
        ObjectRef { header: unsafe { NonNull::new_unchecked(ptr.cast()) } }
    }
}

impl Clone for ObjectRef {
    fn clone(&self) -> Self {
        // We already hold a reference, so the count is at least one; `bump` cannot resurrect a
        // zero.
        self.header().bump();
        ObjectRef { header: self.header }
    }
}

impl Drop for ObjectRef {
    fn drop(&mut self) {
        // Any reference, not only the last: one dropped under `SCHED` is the last whenever another
        // CPU lets go first, and a destructor reaches the allocator (`kernel/docs/lock-ordering.md`).
        #[cfg(debug_assertions)]
        assert!(
            !crate::libkern::lockrank::sched_held_by_this_thread(),
            "a {:?} reference dropped under SCHED",
            self.object_type()
        );
        if self.header().release() {
            // Acquire so every other holder's final writes are visible to the destructor.
            fence(Ordering::Acquire);
            // SAFETY: this was the last reference, so we have exclusive access.
            unsafe { dispatch_destroy(self.header) };
        }
    }
}

/// An owning reference to a kernel object of type `T`: an [`ObjectRef`] whose tag is `T::TYPE`.
/// It derefs to `T` where `T` is [`Shared`].
pub struct Ref<T: KObject> {
    object: ObjectRef,
    _type: PhantomData<T>,
}

impl<T: KObject> Ref<T> {
    /// Adopt a fresh `T`'s box as the reference its creator holds.
    pub fn new(object: KBox<T>) -> Self {
        Self { object: ObjectRef::new(object), _type: PhantomData }
    }

    /// Acquire a reference to the `T` at `obj`, or `None` if its count was already zero.
    ///
    /// # Safety
    /// `obj` must point at a live `T`.
    pub unsafe fn try_acquire(obj: *mut T) -> Option<Self> {
        // SAFETY: a live `T` is a live kernel object, by the contract.
        unsafe { ObjectRef::try_acquire(obj.cast()) }.map(|object| Self { object, _type: PhantomData })
    }

    /// The reference, its type forgotten.
    pub fn erase(self) -> ObjectRef {
        self.object
    }

    /// The reference as an [`ObjectRef`], for what takes any object.
    pub fn as_object(&self) -> &ObjectRef {
        &self.object
    }

    /// The object's pointer.
    pub fn as_ptr(&self) -> *mut T {
        self.object.header.as_ptr().cast()
    }
}

impl<T: KObject> Clone for Ref<T> {
    fn clone(&self) -> Self {
        Self { object: self.object.clone(), _type: PhantomData }
    }
}

impl<T: Shared> Deref for Ref<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the object is a `T`, its tag checked or set when this was made; this reference
        // keeps it live for the borrow, and `T: Shared` lets it be shared.
        unsafe { &*self.as_ptr() }
    }
}

impl<T: KObject> core::fmt::Debug for Ref<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("Ref").field(&T::TYPE).field(&self.as_ptr()).finish()
    }
}

/// Run the destructor for the kernel object at `header`: rebuild the owning `KBox` for the type
/// its tag names and drop it, returning the storage to the slab. Dispatch is a `match` on the
/// tag (`kernel/CLAUDE.md` § "Kernel object dispatch") over every variant, each kind's arm
/// freeing `crate::object`'s type of the same name, so a kind with no arm does not compile.
///
/// # Safety
/// `header` must be the last live reference to a kernel object.
unsafe fn dispatch_destroy(header: NonNull<KObjectHeader>) {
    // SAFETY: the object is live until it is freed below, by the contract.
    let ty = unsafe { header.as_ref() }.object_type();
    macro_rules! by_tag {
        ($($t:ident),*) => {
            match ty {
                $(KObjectType::$t => destroy::<$t>(header),)*
                KObjectType::Invalid | KObjectType::IoRing => {
                    debug_assert!(false, "dispatch_destroy on a tag no kernel object has: {ty:?}")
                }
            }
        };
    }
    // SAFETY: each arm frees the type whose tag it matches, the one `kobject!` lets take it, which
    // `KObject` places at `header`, made by `ObjectRef::new` from that type's `KBox`; the reference
    // is the last, by the contract.
    unsafe {
        by_tag!(
            Process,
            Thread,
            MemoryObject,
            Timer,
            NotificationChannel,
            IpcChannel,
            PendingOperation,
            Namespace,
            EntropyObject,
            DeviceNode,
            InterruptObject,
            UserspaceServerReg,
            FileObject,
            SystemControl
        )
    }
}

/// Free a `T` by dropping its rebuilt box, which runs its `Drop`.
///
/// # Safety
/// `header` is the last live reference to a `T` whose box `KBox::into_raw` gave up.
unsafe fn destroy<T: KObject>(header: NonNull<KObjectHeader>) {
    // SAFETY: the last reference to a `T` from `KBox::<T>::into_raw`, by the contract.
    drop(unsafe { KBox::<T>::from_raw(header.cast()) });
    #[cfg(test)]
    test_probe::note(T::TYPE);
}

/// Per-thread destructor-dispatch counters, so tests can verify which `match` arm fired.
/// Per-thread so the parallel test runner cannot mix counts; the destructor runs on whichever
/// thread drops the last reference, so multi-thread tests sum their workers' counts.
#[cfg(test)]
pub(crate) mod test_probe {
    use crate::libkern::handle::KObjectType;
    use core::cell::Cell;

    /// One more than the largest tag.
    const TAGS: usize = KObjectType::SystemControl as usize + 1;

    std::thread_local! {
        static DESTROYS: [Cell<usize>; TAGS] = const { [const { Cell::new(0) }; TAGS] };
    }

    pub(crate) fn note(ty: KObjectType) {
        DESTROYS.with(|d| d[ty as usize].set(d[ty as usize].get() + 1));
    }

    /// The destroys of objects tagged `ty` on this thread since its last [`reset`].
    pub(crate) fn destroys(ty: KObjectType) -> usize {
        DESTROYS.with(|d| d[ty as usize].get())
    }

    pub(crate) fn process_destroys() -> usize {
        destroys(KObjectType::Process)
    }

    pub(crate) fn thread_destroys() -> usize {
        destroys(KObjectType::Thread)
    }

    pub(crate) fn memory_object_destroys() -> usize {
        destroys(KObjectType::MemoryObject)
    }

    pub(crate) fn timer_destroys() -> usize {
        destroys(KObjectType::Timer)
    }

    pub(crate) fn notification_channel_destroys() -> usize {
        destroys(KObjectType::NotificationChannel)
    }

    pub(crate) fn ipc_channel_destroys() -> usize {
        destroys(KObjectType::IpcChannel)
    }

    pub(crate) fn pending_op_destroys() -> usize {
        destroys(KObjectType::PendingOperation)
    }

    pub(crate) fn namespace_destroys() -> usize {
        destroys(KObjectType::Namespace)
    }

    pub(crate) fn entropy_object_destroys() -> usize {
        destroys(KObjectType::EntropyObject)
    }

    pub(crate) fn system_control_destroys() -> usize {
        destroys(KObjectType::SystemControl)
    }

    pub(crate) fn device_node_destroys() -> usize {
        destroys(KObjectType::DeviceNode)
    }

    pub(crate) fn interrupt_object_destroys() -> usize {
        destroys(KObjectType::InterruptObject)
    }

    pub(crate) fn userspace_server_reg_destroys() -> usize {
        destroys(KObjectType::UserspaceServerReg)
    }

    pub(crate) fn file_object_destroys() -> usize {
        destroys(KObjectType::FileObject)
    }

    pub(crate) fn reset() {
        DESTROYS.with(|d| d.iter().for_each(|c| c.set(0)));
    }
}

#[cfg(test)]
mod tests;
