//! Waiting on several handles at once, and learning which of them signalled.

use core::marker::PhantomData;

use libkern::{IoResult, KError, MAX_WAIT_HANDLES};

use crate::error::{Error, Result};
use crate::handle::Ref;
use crate::sys;
use crate::time::Deadline;

/// Up to `N` handles, borrowed for `'a` so none closes while it is waited on.
pub struct WaitSet<'a, const N: usize> {
    handles: [u64; N],
    len: usize,
    _b: PhantomData<&'a ()>,
}

impl<const N: usize> Default for WaitSet<'_, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a, const N: usize> WaitSet<'a, N> {
    const FITS: () = assert!(N <= MAX_WAIT_HANDLES, "the kernel waits on at most MAX_WAIT_HANDLES");

    /// None yet.
    pub const fn new() -> Self {
        let () = Self::FITS;
        WaitSet { handles: [0; N], len: 0, _b: PhantomData }
    }

    /// Add `h`, answering its index: `TooLarge` once `N` are in, `InvalidArgument` if it is in.
    pub fn add<T: 'a, M: 'a>(&mut self, h: impl Into<Ref<'a, T, M>>) -> Result<usize> {
        let raw = h.into().raw().0;
        if self.handles[..self.len].contains(&raw) {
            return Err(Error::from(KError::InvalidArgument));
        }
        let slot = self.handles.get_mut(self.len).ok_or(Error::from(KError::TooLarge))?;
        *slot = raw;
        self.len += 1;
        Ok(self.len - 1)
    }

    /// How many are in.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether none is.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Wait until one signals or `deadline` passes: which did, none at the deadline or a poll
    /// that found nothing.
    pub fn wait(&self, deadline: Deadline) -> Result<Signalled<N>> {
        let mut s = Signalled {
            results: [IoResult { handle: 0, status: 0, reserved: 0, result: 0 }; N],
            index: [0; N],
            count: 0,
        };
        let handles = &self.handles[..self.len];
        let n = sys::wait(handles, &mut s.results[..self.len], deadline.ns());
        match n {
            n if n >= 0 => s.count = (n as usize).min(self.len),
            n if n == KError::TimedOut as i64 || n == KError::WouldBlock as i64 => {}
            n => return Err(Error::from_status(n as i32)),
        }
        for k in 0..s.count {
            s.index[k] = handles.iter().position(|&h| h == s.results[k].handle).unwrap_or(usize::MAX);
        }
        Ok(s)
    }
}

/// The handles of a [`WaitSet`] a wait found signalled, each with its completion.
pub struct Signalled<const N: usize> {
    results: [IoResult; N],
    index: [usize; N],
    count: usize,
}

impl<const N: usize> Signalled<N> {
    /// How many signalled.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether none did.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Each that signalled: its index in the set, and what the kernel wrote for it.
    pub fn iter(&self) -> impl Iterator<Item = (usize, &IoResult)> {
        self.index[..self.count].iter().copied().zip(&self.results[..self.count])
    }

    /// Whether the handle at `index` signalled.
    pub fn contains(&self, index: usize) -> bool {
        self.index[..self.count].contains(&index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Channel, Handle, Handles, Msg, Only};

    #[test]
    fn a_wait_answers_which_signalled() {
        sys::reset();
        let (a, a2) = Handle::<Channel, Only>::pair(4).unwrap();
        let (b, b2) = Handle::<Channel, Only>::pair(4).unwrap();
        b2.send(&Msg::new(), &mut Handles::new()).unwrap();
        let mut set = WaitSet::<4>::new();
        assert_eq!((set.add(&a).unwrap(), set.add(&b).unwrap()), (0, 1));
        let s = set.wait(Deadline::POLL).unwrap();
        assert_eq!(s.iter().map(|(i, _)| i).collect::<std::vec::Vec<_>>(), [1]);
        assert!(s.contains(1) && !s.contains(0));
        drop(a2);
        let s = set.wait(Deadline::NEVER).unwrap();
        assert_eq!(s.len(), 2, "a closed peer signals too");
    }

    #[test]
    fn a_set_holds_no_more_than_its_room_and_each_once() {
        sys::reset();
        let (a, b) = Handle::<Channel, Only>::pair(4).unwrap();
        let mut set = WaitSet::<1>::new();
        set.add(&a).unwrap();
        assert_eq!(set.add(&a).err().unwrap().code(), KError::InvalidArgument);
        assert_eq!(set.add(&b).err().unwrap().code(), KError::TooLarge);
    }

    #[test]
    fn a_wait_that_finds_nothing_is_no_error() {
        sys::reset();
        let (a, _b) = Handle::<Channel, Only>::pair(4).unwrap();
        let mut set = WaitSet::<2>::new();
        set.add(&a).unwrap();
        assert!(set.wait(Deadline::POLL).unwrap().is_empty());
        assert!(set.wait(Deadline::after(1).unwrap()).unwrap().is_empty());
        assert_eq!(sys::sleeps(), 1);
    }
}
