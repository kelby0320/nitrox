//! [`ArrayVec`]: a list of at most `N` values, held inline.

use core::mem::MaybeUninit;
use core::ops::{Deref, DerefMut};

/// At most `N` values, held inline: a push past `N` is refused, never grown, so one may run under
/// any lock or in an interrupt, and its capacity is its type.
#[derive(Clone, Copy)]
pub struct ArrayVec<T: Copy, const N: usize> {
    len: usize,
    items: [MaybeUninit<T>; N],
}

impl<T: Copy, const N: usize> ArrayVec<T, N> {
    /// An empty list.
    pub const fn new() -> Self {
        Self { len: 0, items: [const { MaybeUninit::uninit() }; N] }
    }

    /// Append `val`, or hand it back when `N` are held.
    pub fn try_push(&mut self, val: T) -> Result<(), T> {
        let Some(slot) = self.items.get_mut(self.len) else { return Err(val) };
        slot.write(val);
        self.len += 1;
        Ok(())
    }

    /// Remove and return the last value.
    pub fn pop(&mut self) -> Option<T> {
        let last = *self.last()?;
        self.len -= 1;
        Some(last)
    }

    /// Remove and return the value at `index`, shifting those after it down. Panics past the end.
    pub fn remove(&mut self, index: usize) -> T {
        let val = self[index];
        self.items.copy_within(index + 1..self.len, index);
        self.len -= 1;
        val
    }

    /// Remove every value.
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl<T: Copy, const N: usize> Default for ArrayVec<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy, const N: usize> Deref for ArrayVec<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        // SAFETY: the first `len` items are written: `try_push` writes an item before counting it,
        // `remove` moves written items down over the one it takes, and nothing else raises `len`.
        unsafe { core::slice::from_raw_parts(self.items.as_ptr().cast::<T>(), self.len) }
    }
}

impl<T: Copy, const N: usize> DerefMut for ArrayVec<T, N> {
    fn deref_mut(&mut self) -> &mut [T] {
        // SAFETY: as `deref`, and `&mut self` makes the borrow unique.
        unsafe { core::slice::from_raw_parts_mut(self.items.as_mut_ptr().cast::<T>(), self.len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_push_past_the_capacity_is_handed_back() {
        let mut v = ArrayVec::<u32, 3>::new();
        for i in 0..3 {
            assert_eq!(v.try_push(i), Ok(()));
        }
        assert_eq!(v.try_push(9), Err(9));
        assert_eq!(&v[..], &[0, 1, 2]);
        assert_eq!(v.pop(), Some(2));
        assert_eq!(v.try_push(7), Ok(()));
        assert_eq!(&v[..], &[0, 1, 7]);
    }

    #[test]
    fn remove_keeps_the_order_of_the_rest() {
        let mut v = ArrayVec::<u32, 4>::new();
        for i in 10..14 {
            v.try_push(i).unwrap();
        }
        assert_eq!(v.remove(1), 11);
        assert_eq!(&v[..], &[10, 12, 13]);
        assert_eq!(v.remove(2), 13);
        assert_eq!(v.remove(0), 10);
        assert_eq!(&v[..], &[12]);
        v.clear();
        assert!(v.is_empty());
        assert_eq!(v.pop(), None);
    }

    #[test]
    #[should_panic]
    fn remove_past_the_end_panics() {
        let mut v = ArrayVec::<u32, 4>::new();
        v.try_push(1).unwrap();
        v.remove(1);
    }

    #[test]
    fn a_copy_is_its_own_list() {
        let mut v = ArrayVec::<u32, 2>::new();
        v.try_push(1).unwrap();
        let mut w = v;
        v.clear();
        w[0] = 5;
        assert_eq!((&v[..], &w[..]), (&[][..], &[5][..]));
    }

    #[test]
    fn a_zero_capacity_list_takes_nothing() {
        let mut v = ArrayVec::<u32, 0>::new();
        assert_eq!(v.try_push(1), Err(1));
        assert!(v.is_empty());
    }
}
