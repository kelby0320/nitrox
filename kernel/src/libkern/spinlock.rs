//! Plain test-and-set spin lock.
//!
//! Phase 1 of Nitrox runs with interrupts disabled throughout: the IDT,
//! PIC, and APIC are not yet brought up, and the scheduler does not exist.
//! There is exactly one execution context — the boot path — so a spin
//! lock that simply busy-waits on an `AtomicBool` is sufficient and
//! correct. The lock does not mask interrupts because there are none to
//! mask. See `kernel/docs/lock-ordering.md` for where this lock sits in
//! the kernel's rank ordering (rank 6, allocator locks).
//!
//! When interrupts are enabled (the upcoming IDT slice), data structures
//! that are touched from both thread and IRQ context must switch to an
//! `IrqSpinLock` variant that pushes `RFLAGS` + `cli` on acquire and
//! restores on release. That variant does not exist yet; until it does,
//! no IRQ handler may take any `SpinLock`. The IDT slice will introduce
//! the variant, audit existing call sites, and update this comment.
//!
//! ## Why not just a `core::sync::Mutex`?
//!
//! `core` does not provide a `Mutex` (that lives in `std::sync`), and the
//! kernel is `#![no_std]`. A spin lock is also a better fit for a
//! single-CPU bring-up where blocking has no scheduler to yield to.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A test-and-set spin lock guarding a `T`.
///
/// `T` is owned by the lock; access is only possible through a guard
/// returned by [`SpinLock::lock`]. The guard's `Drop` releases the lock.
///
/// `SpinLock::new` is `const`, so a `SpinLock<T>` can be stored in a
/// `static` when `T` is const-constructible.
pub struct SpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: A `SpinLock<T>` serialises access to its `T` through the
// `locked` AtomicBool. Only one CPU can observe the false → true
// transition; the resulting guard gives that CPU exclusive access to the
// inner cell until it is dropped. `T` need only be `Send` because the
// value can be reached from any CPU that wins the lock — but never from
// more than one at a time.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: Sending the whole lock between CPUs is exactly as safe as
// sending the inner `T` because no CPU-local state is captured.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Construct a new lock around `value`. Available in `const` context.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, spinning until it becomes available. Returns a
    /// guard that releases the lock when dropped.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        loop {
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return SpinLockGuard { lock: self };
            }
            // Spin without bus-locking the cacheline: a relaxed read plus
            // `core::hint::spin_loop` lets the CPU back off so the lock
            // holder isn't slowed down by our pestering.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    /// Release the lock. Called from `SpinLockGuard::drop`; not exposed.
    fn release(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

/// RAII guard returned by [`SpinLock::lock`]. While alive, the guard
/// proves the lock is held and grants `&T` / `&mut T` to the inner value.
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: The guard's existence proves the lock is held; no other
        // CPU can be inside the cell while `self.lock.locked == true`.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: As above; the `&mut self` receiver further proves no
        // overlapping `&Self` reborrow is outstanding.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_grants_exclusive_access_in_single_thread() {
        let lock = SpinLock::new(0u32);
        {
            let mut g = lock.lock();
            *g = 42;
        }
        let g = lock.lock();
        assert_eq!(*g, 42);
    }

    #[test]
    fn guard_drop_releases_lock() {
        let lock = SpinLock::new(());
        let g = lock.lock();
        drop(g);
        // Must not deadlock — the previous guard's Drop released the bit.
        let _g2 = lock.lock();
    }

    #[test]
    fn mutates_through_deref_mut() {
        let lock = SpinLock::new([0u8; 4]);
        {
            let mut g = lock.lock();
            g[0] = 1;
            g[3] = 9;
        }
        let g = lock.lock();
        assert_eq!(*g, [1, 0, 0, 9]);
    }

    #[test]
    fn const_constructs_in_static() {
        static S: SpinLock<u32> = SpinLock::new(7);
        let g = S.lock();
        assert_eq!(*g, 7);
    }

    // The contested path (one CPU spinning while another holds the lock)
    // is not exercised by host tests: `cargo test` runs single-threaded
    // here, and even if it did, the std-thread version of contention has
    // different semantics than the on-target one (a yielding scheduler vs.
    // an HLT-less spin). An integration test under QEMU with SMP will
    // cover this once a second CPU is brought up in Phase 3.
}
