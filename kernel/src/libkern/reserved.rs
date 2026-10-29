//! [`Reserved`]: a list whose capacity is allocated when it is made, and which never grows.

use core::ops::{Deref, DerefMut};

use crate::libkern::{AllocError, KVec};

/// A list holding at most the capacity it was made with, a push past it handed back, so one may run
/// under any lock or in an interrupt. No `Default`, so no `mem::take` leaves one of no room.
pub struct Reserved<T>(KVec<T>);

impl<T> Reserved<T> {
    /// A list of no room, refusing every push: a `static`'s value until one is reserved.
    pub const fn unreserved() -> Self {
        Self(KVec::new())
    }

    /// A list of room for at least `n`, allocated now.
    pub fn with_capacity(n: usize) -> Result<Self, AllocError> {
        let mut list = KVec::new();
        list.try_reserve(n)?;
        Ok(Self(list))
    }

    /// Append `val`, or hand it back when the list is full.
    pub fn push(&mut self, val: T) -> Result<(), T> {
        self.0.push_within_capacity(val)
    }

    /// Remove and return the last value.
    pub fn pop(&mut self) -> Option<T> {
        self.0.pop()
    }

    /// Remove and return the value at `index`, shifting those after it down. Panics past the end.
    pub fn remove(&mut self, index: usize) -> T {
        self.0.remove(index)
    }

    /// How many it can hold.
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }
}

impl<T> Deref for Reserved<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.0
    }
}

impl<T> DerefMut for Reserved<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn a_push_past_the_capacity_is_handed_back_and_nothing_grows() {
        init_global_heap();
        let mut list = Reserved::with_capacity(2).unwrap();
        let cap = list.capacity();
        for i in 0..cap {
            assert_eq!(list.push(i), Ok(()));
        }
        assert_eq!(list.push(99), Err(99));
        assert_eq!((list.len(), list.capacity()), (cap, cap));
        assert_eq!(list.remove(0), 0);
        assert_eq!(list.pop(), Some(cap - 1));
        assert_eq!(list.push(7), Ok(()));
    }

    #[test]
    fn an_unreserved_list_refuses_every_push() {
        let mut list = Reserved::<u32>::unreserved();
        assert_eq!(list.push(1), Err(1));
        assert_eq!((list.len(), list.capacity()), (0, 0));
    }
}
