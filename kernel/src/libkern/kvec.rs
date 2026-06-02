//! [`KVec<T>`] — the kernel's fallible growable array.

use core::mem;
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};

use crate::libkern::AllocError;
use crate::mm::slab::{kfree, kmalloc};

/// A fallible, growable, heap-backed array — the kernel's analogue of
/// `alloc::vec::Vec`.
///
/// Every operation that can allocate ([`try_push`](KVec::try_push),
/// [`try_reserve`](KVec::try_reserve),
/// [`try_extend_from_slice`](KVec::try_extend_from_slice)) returns
/// `Result<_, `[`AllocError`]`>` rather than aborting on exhaustion. That
/// is why the kernel uses `KVec` in place of `alloc`'s `Vec` — see the
/// decision log entry of 2026-05-20.
///
/// An empty `KVec` holds no allocation; the backing buffer is acquired
/// lazily on first growth and released on drop. A zero-sized element type
/// never allocates.
pub struct KVec<T> {
    /// Backing buffer. Dangling when `cap == 0` or `T` is zero-sized.
    ptr: NonNull<T>,
    /// Number of initialised elements, occupying `[0, len)`.
    len: usize,
    /// Number of elements the buffer can hold. `usize::MAX` for a
    /// zero-sized `T`, whose capacity is conceptually unbounded.
    cap: usize,
}

// SAFETY: `KVec<T>` owns its elements just as a bare `[T]` would. Sending
// the vec is sound when `T: Send`; sharing a `&KVec` yields only `&[T]`,
// sound when `T: Sync`.
unsafe impl<T: Send> Send for KVec<T> {}
unsafe impl<T: Sync> Sync for KVec<T> {}

impl<T> KVec<T> {
    /// Create an empty vector. No allocation happens until the first
    /// element is inserted, so this is usable in `const` context.
    pub const fn new() -> Self {
        KVec {
            ptr: NonNull::dangling(),
            len: 0,
            cap: 0,
        }
    }

    /// Number of elements currently stored.
    pub fn len(&self) -> usize {
        self.len
    }

    /// `true` when the vector holds no elements.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of elements the current allocation can hold without
    /// growing.
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// Append `val` to the end, growing the buffer if necessary.
    ///
    /// Returns [`AllocError`] if a required reallocation fails; the
    /// vector is left unchanged in that case.
    pub fn try_push(&mut self, val: T) -> Result<(), AllocError> {
        if self.len == self.cap {
            self.grow(1)?;
        }
        // SAFETY: `grow` guaranteed `len < cap`, so the slot at `len` is
        // allocated and uninitialised; the write initialises one more
        // element. For a ZST the slot is a dangling no-op target.
        unsafe { ptr::write(self.ptr.as_ptr().add(self.len), val) };
        self.len += 1;
        Ok(())
    }

    /// Remove and return the last element, or `None` when empty.
    pub fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: the element at the new `len` was initialised and is now
        // logically removed; `read` moves ownership out without dropping
        // the original in place.
        Some(unsafe { ptr::read(self.ptr.as_ptr().add(self.len)) })
    }

    /// Remove and return the element at `index`, shifting every later
    /// element down by one. `O(len - index)`. Panics if `index >= len`.
    ///
    /// This is the front-dequeue the scheduler's round-robin run queue
    /// needs (`remove(0)`); it never reallocates, so it is safe to call
    /// while holding a lock that forbids allocation.
    pub fn remove(&mut self, index: usize) -> T {
        assert!(index < self.len, "KVec::remove index out of bounds");
        // SAFETY: `index < len`, so the slot is initialised; `read` moves
        // it out. The `copy` shifts the `len - index - 1` trailing elements
        // down by one over the now-vacated slot (overlapping ranges, hence
        // `copy` not `copy_nonoverlapping`); decrementing `len` then drops
        // the duplicated last slot from the live range without re-dropping.
        unsafe {
            let base = self.ptr.as_ptr();
            let hole = base.add(index);
            let val = ptr::read(hole);
            ptr::copy(hole.add(1), hole, self.len - index - 1);
            self.len -= 1;
            val
        }
    }

    /// Ensure room for at least `additional` further elements so the
    /// next inserts will not reallocate.
    pub fn try_reserve(&mut self, additional: usize) -> Result<(), AllocError> {
        self.grow(additional)
    }

    /// Append every element of `src` by bytewise copy.
    pub fn try_extend_from_slice(&mut self, src: &[T]) -> Result<(), AllocError>
    where
        T: Copy,
    {
        self.grow(src.len())?;
        // SAFETY: `grow` ensured `cap - len >= src.len()`, so the
        // destination range is allocated and uninitialised; `src` is a
        // disjoint slice and `T: Copy`, so a bytewise copy fully
        // initialises the appended elements.
        unsafe {
            ptr::copy_nonoverlapping(
                src.as_ptr(),
                self.ptr.as_ptr().add(self.len),
                src.len(),
            );
        }
        self.len += src.len();
        Ok(())
    }

    /// Drop every element, keeping the allocated capacity.
    pub fn clear(&mut self) {
        let initialised: *mut [T] =
            ptr::slice_from_raw_parts_mut(self.ptr.as_ptr(), self.len);
        // Zero `len` before dropping so a panicking destructor cannot
        // observe — or double-drop — a partially cleared vector.
        self.len = 0;
        // SAFETY: `initialised` covers exactly the `len` elements that
        // were live; `drop_in_place` runs each destructor once.
        unsafe { ptr::drop_in_place(initialised) };
    }

    /// Grow the buffer so it can hold at least `len + additional`
    /// elements. A no-op when the current capacity already suffices.
    fn grow(&mut self, additional: usize) -> Result<(), AllocError> {
        let required = self.len.checked_add(additional).ok_or(AllocError)?;
        if required <= self.cap {
            return Ok(());
        }
        // Amortise growth by doubling, but jump straight to `required`
        // when a single reservation asks for more than a doubling.
        let new_cap = if self.cap == 0 {
            required.max(4)
        } else {
            required.max(self.cap.saturating_mul(2))
        };
        self.realloc_to(new_cap)
    }

    /// Move the elements into a fresh `new_cap`-element buffer and free
    /// the old one. `new_cap` must be `>= len`.
    fn realloc_to(&mut self, new_cap: usize) -> Result<(), AllocError> {
        debug_assert!(new_cap >= self.len);

        if mem::size_of::<T>() == 0 {
            // A zero-sized element type occupies no storage; record an
            // unbounded capacity once and never call the allocator.
            self.cap = usize::MAX;
            return Ok(());
        }

        let new_bytes = new_cap
            .checked_mul(mem::size_of::<T>())
            .ok_or(AllocError)?;
        let raw = kmalloc(new_bytes, mem::align_of::<T>()) as *mut T;
        let new_ptr = NonNull::new(raw).ok_or(AllocError)?;

        if self.cap != 0 {
            // SAFETY: the old buffer holds `len` initialised elements and
            // the new buffer has room for at least `len` (`new_cap >=
            // len`); the two allocations do not overlap. After the move
            // the old buffer holds no live elements, so freeing it is
            // sound.
            unsafe {
                ptr::copy_nonoverlapping(
                    self.ptr.as_ptr(),
                    new_ptr.as_ptr(),
                    self.len,
                );
                kfree(self.ptr.as_ptr() as *mut u8);
            }
        }

        self.ptr = new_ptr;
        self.cap = new_cap;
        Ok(())
    }
}

impl<T> Deref for KVec<T> {
    type Target = [T];

    fn deref(&self) -> &[T] {
        // SAFETY: the first `len` elements are initialised and contiguous
        // from `ptr`, which is non-null and aligned (a dangling pointer
        // is a valid base when `len == 0`).
        unsafe { core::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> DerefMut for KVec<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as `deref`, with exclusive access proven by `&mut self`.
        unsafe { core::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl<T> Drop for KVec<T> {
    fn drop(&mut self) {
        // Run every element's destructor first.
        self.clear();
        // A backing buffer was allocated only when `cap != 0` and `T` is
        // not zero-sized.
        if self.cap != 0 && mem::size_of::<T>() != 0 {
            // SAFETY: in that case `ptr` came from `kmalloc` and `clear`
            // left no live elements behind.
            kfree(self.ptr.as_ptr() as *mut u8);
        }
    }
}

impl<T> Default for KVec<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use core::sync::atomic::{AtomicUsize, Ordering};

    /// Bumps a caller-owned counter when dropped.
    struct DropFlag<'a>(&'a AtomicUsize);

    impl Drop for DropFlag<'_> {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn new_is_empty_and_unallocated() {
        let v: KVec<u32> = KVec::new();
        assert!(v.is_empty());
        assert_eq!(v.len(), 0);
        assert_eq!(v.capacity(), 0);
    }

    #[test]
    fn push_then_read_back() {
        init_global_heap();
        let mut v = KVec::new();
        v.try_push(10u32).unwrap();
        v.try_push(20).unwrap();
        v.try_push(30).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(&v[..], &[10, 20, 30]);
    }

    #[test]
    fn growth_preserves_elements() {
        init_global_heap();
        let mut v = KVec::new();
        for i in 0..1000u32 {
            v.try_push(i).unwrap();
        }
        assert_eq!(v.len(), 1000);
        assert!(v.capacity() >= 1000);
        for (i, &x) in v.iter().enumerate() {
            assert_eq!(x, i as u32);
        }
    }

    #[test]
    fn growth_crosses_slab_to_buddy_boundary() {
        init_global_heap();
        // 800 * 8 bytes = 6400 bytes — well past the 2048-byte largest
        // slab bucket, so the backing buffer routes through `large_alloc`.
        let mut v = KVec::new();
        for i in 0..800u64 {
            v.try_push(i).unwrap();
        }
        assert_eq!(v.len(), 800);
        assert_eq!(v[0], 0);
        assert_eq!(v[799], 799);
    }

    #[test]
    fn remove_front_is_fifo_and_shifts_down() {
        init_global_heap();
        let mut v = KVec::new();
        for i in 0..5u32 {
            v.try_push(i).unwrap();
        }
        // Round-robin dequeue from the front preserves order.
        assert_eq!(v.remove(0), 0);
        assert_eq!(v.remove(0), 1);
        assert_eq!(&v[..], &[2, 3, 4]);
        // Removing from the middle shifts the tail down.
        assert_eq!(v.remove(1), 3);
        assert_eq!(&v[..], &[2, 4]);
        // Removing the last element empties it.
        assert_eq!(v.remove(1), 4);
        assert_eq!(v.remove(0), 2);
        assert!(v.is_empty());
    }

    #[test]
    fn remove_runs_no_destructor_on_shifted_elements() {
        init_global_heap();
        let count = AtomicUsize::new(0);
        let mut v = KVec::new();
        for _ in 0..4 {
            v.try_push(DropFlag(&count)).unwrap();
        }
        // Removing returns ownership of exactly one element; dropping the
        // returned value runs exactly one destructor, and the three shifted
        // survivors are not dropped by the shift.
        let taken = v.remove(0);
        drop(taken);
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(v.len(), 3);
        drop(v);
        assert_eq!(count.load(Ordering::SeqCst), 4, "no double-drop on shift");
    }

    #[test]
    fn pop_is_lifo() {
        init_global_heap();
        let mut v = KVec::new();
        v.try_push(1u8).unwrap();
        v.try_push(2).unwrap();
        assert_eq!(v.pop(), Some(2));
        assert_eq!(v.pop(), Some(1));
        assert_eq!(v.pop(), None);
        assert!(v.is_empty());
    }

    #[test]
    fn extend_from_slice_appends() {
        init_global_heap();
        let mut v = KVec::new();
        v.try_push(1u32).unwrap();
        v.try_extend_from_slice(&[2, 3, 4]).unwrap();
        assert_eq!(&v[..], &[1, 2, 3, 4]);
    }

    #[test]
    fn try_reserve_avoids_later_growth() {
        init_global_heap();
        let mut v: KVec<u32> = KVec::new();
        v.try_reserve(64).unwrap();
        let cap = v.capacity();
        assert!(cap >= 64);
        for i in 0..64 {
            v.try_push(i).unwrap();
        }
        assert_eq!(v.capacity(), cap, "no reallocation within reserved cap");
    }

    #[test]
    fn drop_runs_every_element_destructor() {
        init_global_heap();
        let count = AtomicUsize::new(0);
        {
            let mut v = KVec::new();
            for _ in 0..16 {
                v.try_push(DropFlag(&count)).unwrap();
            }
        }
        assert_eq!(count.load(Ordering::SeqCst), 16);
    }

    #[test]
    fn clear_drops_elements_keeps_capacity() {
        init_global_heap();
        let count = AtomicUsize::new(0);
        let mut v = KVec::new();
        for _ in 0..8 {
            v.try_push(DropFlag(&count)).unwrap();
        }
        let cap = v.capacity();
        v.clear();
        assert_eq!(count.load(Ordering::SeqCst), 8);
        assert!(v.is_empty());
        assert_eq!(v.capacity(), cap);
    }

    #[test]
    fn zero_sized_elements_never_allocate() {
        let mut v: KVec<()> = KVec::new();
        for _ in 0..1000 {
            v.try_push(()).unwrap();
        }
        assert_eq!(v.len(), 1000);
        assert_eq!(v.pop(), Some(()));
        assert_eq!(v.len(), 999);
    }
}
