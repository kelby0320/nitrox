//! [`KBox<T>`] — the kernel's fallible owned heap allocation.

use core::mem;
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};

use crate::libkern::AllocError;
use crate::mm::slab::{kfree, kmalloc};

/// A fallible, owned heap allocation — the kernel's analogue of
/// `alloc::boxed::Box`.
///
/// The kernel does not use the `alloc` crate (see the decision log entry
/// of 2026-05-20): `alloc`'s `Box::new` aborts on allocation failure,
/// which a kernel cannot tolerate. [`KBox::try_new`] instead reports
/// exhaustion as [`AllocError`].
///
/// `KBox<T>` owns exactly one heap-allocated `T`, dereferences to it, and
/// on drop runs its destructor and releases the backing storage. A
/// zero-sized `T` is handled without ever touching the allocator.
pub struct KBox<T> {
    ptr: NonNull<T>,
}

// SAFETY: a `KBox<T>` owns its `T` outright, exactly as a bare `T` would.
// Moving the box between threads is sound when `T: Send`; sharing a
// `&KBox` (which only yields `&T`) is sound when `T: Sync`.
unsafe impl<T: Send> Send for KBox<T> {}
unsafe impl<T: Sync> Sync for KBox<T> {}

impl<T> KBox<T> {
    /// Allocate space for a `T` on the kernel heap, move `val` into it,
    /// and return the owning box.
    ///
    /// Returns [`AllocError`] if the heap cannot satisfy the request.
    ///
    /// # Panics
    ///
    /// Aborts the kernel if called before `mm::slab::slab_init` has run.
    /// That is a use-before-init bug rather than an out-of-memory
    /// condition, and the abort is a deliberate tripwire — see
    /// [`kmalloc`](crate::mm::slab::kmalloc).
    pub fn try_new(val: T) -> Result<Self, AllocError> {
        let ptr = if mem::size_of::<T>() == 0 {
            // A zero-sized `T` needs no storage; a dangling-but-aligned
            // pointer is a valid place to "hold" it and the allocator is
            // never involved.
            NonNull::<T>::dangling()
        } else {
            let raw = kmalloc(mem::size_of::<T>(), mem::align_of::<T>()) as *mut T;
            NonNull::new(raw).ok_or(AllocError)?
        };
        // SAFETY: `ptr` is aligned for `T` and addresses `size_of::<T>()`
        // writable bytes that nothing else aliases — a no-op write target
        // for a ZST, freshly allocated storage otherwise. The write moves
        // `val` in, initialising the allocation.
        unsafe { ptr::write(ptr.as_ptr(), val) };
        Ok(KBox { ptr })
    }

    /// Consume the box and yield its raw pointer, suppressing the
    /// destructor. The caller takes ownership of the allocation and is
    /// responsible for reconstituting it via [`KBox::from_raw`] (or
    /// freeing the storage directly).
    ///
    /// Used by intrusive containers that thread the allocation through
    /// raw pointers in their links and reconstruct the box on removal.
    pub fn into_raw(boxed: Self) -> NonNull<T> {
        let ptr = boxed.ptr;
        mem::forget(boxed);
        ptr
    }

    /// Reconstruct a box from a raw pointer previously yielded by
    /// [`KBox::into_raw`]. The reconstructed box owns the allocation as
    /// if it had been returned by `try_new`.
    ///
    /// # Safety
    /// `ptr` must have come from a prior [`KBox::into_raw`] for a `T`
    /// of the same type, and must not have been reconstructed already.
    /// The pointee must still be initialised and not aliased.
    pub unsafe fn from_raw(ptr: NonNull<T>) -> Self {
        Self { ptr }
    }
}

impl<T> Deref for KBox<T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: the box owns an initialised `T`; `&self` ties the
        // returned shared reference to the box's lifetime.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for KBox<T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: the box owns an initialised `T`; `&mut self` proves no
        // other reference to it is live.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for KBox<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        (**self).fmt(f)
    }
}

impl<T> Drop for KBox<T> {
    fn drop(&mut self) {
        // SAFETY: the box owns an initialised `T` that nothing else can
        // reach; `drop_in_place` runs its destructor exactly once.
        unsafe { ptr::drop_in_place(self.ptr.as_ptr()) };
        if mem::size_of::<T>() != 0 {
            // SAFETY: for a non-ZST the pointer came from `kmalloc`;
            // `kfree` recovers the size class from the slab descriptor,
            // so no layout needs to be threaded through.
            kfree(self.ptr.as_ptr() as *mut u8);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Bumps a caller-owned counter when dropped, so tests can observe
    /// that `KBox` runs destructors exactly once.
    struct DropFlag<'a>(&'a AtomicUsize);

    impl Drop for DropFlag<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn try_new_then_deref_reads_value() {
        init_global_heap();
        let b = KBox::try_new(42u32).unwrap();
        assert_eq!(*b, 42);
    }

    #[test]
    fn deref_mut_mutates_in_place() {
        init_global_heap();
        let mut b = KBox::try_new(1u64).unwrap();
        *b += 99;
        assert_eq!(*b, 100);
    }

    #[test]
    fn drop_runs_destructor_exactly_once() {
        init_global_heap();
        let count = AtomicUsize::new(0);
        {
            let _b = KBox::try_new(DropFlag(&count)).unwrap();
            assert_eq!(count.load(Ordering::SeqCst), 0);
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn zero_sized_type_constructs_and_drops() {
        init_global_heap();
        // A plain ZST must construct and drop without touching the slab.
        let _unit = KBox::try_new(()).unwrap();

        // A ZST carrying a destructor must still have its drop run.
        struct ZstDrop<'a>(&'a AtomicUsize);
        impl Drop for ZstDrop<'_> {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let count = AtomicUsize::new(0);
        {
            let _b = KBox::try_new(ZstDrop(&count)).unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn into_raw_then_from_raw_round_trips_without_double_free() {
        init_global_heap();
        let count = AtomicUsize::new(0);
        let raw = KBox::into_raw(KBox::try_new(DropFlag(&count)).unwrap());
        // `into_raw` must suppress the destructor — no drop yet.
        assert_eq!(count.load(Ordering::SeqCst), 0);
        // SAFETY: `raw` came from the matching `into_raw` above and
        // has not been reconstructed yet.
        let _restored = unsafe { KBox::from_raw(raw) };
        assert_eq!(count.load(Ordering::SeqCst), 0);
        drop(_restored);
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn distinct_allocations_do_not_alias() {
        init_global_heap();
        let a = KBox::try_new(0xAAu8).unwrap();
        let b = KBox::try_new(0xBBu8).unwrap();
        let pa: *const u8 = &*a;
        let pb: *const u8 = &*b;
        assert_ne!(pa, pb);
        assert_eq!(*a, 0xAA);
        assert_eq!(*b, 0xBB);
    }
}
