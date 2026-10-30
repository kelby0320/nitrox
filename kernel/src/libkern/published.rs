//! [`Published`]: a value boxed once and shared for the kernel's lifetime.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, Ordering};

use super::KBox;

/// **A value published once and never withdrawn**, as a Tier 1 controller's state is: a reader
/// before the publish sees nothing, and a second publish is refused, its box handed back.
pub struct Published<T: Sync + 'static>(AtomicPtr<T>);

impl<T: Sync + 'static> Published<T> {
    /// Nothing published yet.
    pub const fn new() -> Self {
        Published(AtomicPtr::new(core::ptr::null_mut()))
    }

    /// **Publish `boxed`, for good**, or hand it back if a value was published already.
    pub fn publish(&self, boxed: KBox<T>) -> Result<&'static T, KBox<T>> {
        let p = KBox::into_raw(boxed);
        match self.0.compare_exchange(core::ptr::null_mut(), p.as_ptr(), Ordering::AcqRel, Ordering::Acquire) {
            // SAFETY: the box's value, never freed now: nothing takes it back out of `self`.
            Ok(_) => Ok(unsafe { p.as_ref() }),
            // SAFETY: `p` came from `into_raw` above and was not stored, so the box is still ours.
            Err(_) => Err(unsafe { KBox::from_raw(p) }),
        }
    }

    /// What was published, if anything yet.
    pub fn get(&self) -> Option<&'static T> {
        // SAFETY: stored once by `publish`, a box never freed; `Acquire` pairs with its `Release`.
        NonNull::new(self.0.load(Ordering::Acquire)).map(|p| unsafe { p.as_ref() })
    }
}

impl<T: Sync + 'static> Default for Published<T> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **One publish takes; a second is refused, its box handed back**, and the first stays.
    #[test]
    fn a_value_is_published_once_and_a_second_is_refused() {
        crate::mm::test_support::init_global_heap();
        static P: Published<u64> = Published::new();
        assert!(P.get().is_none(), "nothing before the publish");
        let first = P.publish(KBox::try_new(7).unwrap()).expect("the first publish takes");
        assert_eq!(*first, 7);
        let second = P.publish(KBox::try_new(8).unwrap()).expect_err("a second is refused");
        assert_eq!(*second, 8, "its box back, unchanged");
        assert_eq!(P.get().copied(), Some(7), "the first stays");
    }
}
