//! Test-and-set spin locks: a plain [`SpinLock`] and an interrupt-safe
//! [`IrqSpinLock`].
//!
//! A spin lock busy-waits on an `AtomicBool`; on single-CPU the only real
//! contention is between a thread and an interrupt handler that touch the
//! same data. See `kernel/docs/lock-ordering.md` for where each lock sits in
//! the kernel's rank ordering.
//!
//! ## `SpinLock` vs `IrqSpinLock`
//!
//! [`SpinLock`] does **not** mask interrupts. It is correct for data that is
//! never touched from an interrupt handler (the allocators, the kernel vmap,
//! the handle table, address spaces) — a timer IRQ that preempts a thread
//! holding such a lock never tries to take it, so there is no reentrancy.
//!
//! [`IrqSpinLock`] is the variant for data shared between thread and IRQ
//! context (the scheduler run queue `SCHED`, the serial port `SERIAL`). It
//! captures the prior interrupt-enable state and `cli`s **before** spinning,
//! so the whole critical section runs with interrupts masked; the guard
//! restores the prior state on drop. On single-CPU this makes such data
//! deadlock-free: an IRQ cannot fire while the lock is held, so a handler can
//! never find the lock already held by the context it interrupted.
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

/// Interrupt-flag backend for [`IrqSpinLock`].
///
/// On target, this drives the real CPU interrupt flag through the arch layer.
/// Under `cfg(test)` (host `cargo test`, ring 3) the real `cli`/`sti` would
/// `#GP`, so a single-threaded mock models `IF` in an `AtomicBool` — enough to
/// exercise the lock's save/restore *logic* (the asm itself is QEMU-only).
#[cfg(not(test))]
mod irq_backend {
    use crate::arch::Cpu;
    use crate::arch::cpu::ArchCpu;

    #[inline]
    pub(super) fn disable() -> bool {
        // SAFETY: ring-0; the IrqSpinLock contract bounds the masked window
        // (the lock is held only briefly and never across a blocking wait).
        unsafe { Cpu::interrupts_disable() }
    }

    #[inline]
    pub(super) fn restore(prev: bool) {
        // SAFETY: ring-0; restoring an interrupt state captured by `disable`.
        unsafe { Cpu::interrupts_restore(prev) }
    }
}

#[cfg(test)]
mod irq_backend {
    use core::sync::atomic::{AtomicBool, Ordering};

    /// Models `RFLAGS.IF` for host tests. Starts enabled (the boot default
    /// once preemption is armed). Host tests are single-threaded.
    pub(super) static MOCK_IF: AtomicBool = AtomicBool::new(true);

    pub(super) fn disable() -> bool {
        MOCK_IF.swap(false, Ordering::SeqCst)
    }

    pub(super) fn restore(prev: bool) {
        MOCK_IF.store(prev, Ordering::SeqCst);
    }
}

/// A spin lock that masks interrupts for the duration of the critical section.
///
/// On [`lock`](IrqSpinLock::lock) it captures the prior interrupt-enable state
/// and `cli`s **before** spinning, so the entire hold window runs with
/// interrupts masked; the guard's `Drop` releases the lock and **then**
/// restores the prior state. Use this for data shared between thread and IRQ
/// context (`SCHED`, `SERIAL`); see the module docs for the deadlock-freedom
/// argument.
pub struct IrqSpinLock<T> {
    locked: AtomicBool,
    data: UnsafeCell<T>,
}

// SAFETY: identical reasoning to `SpinLock` — the `locked` AtomicBool
// serialises access to the inner cell; only the winning context holds the
// guard. Masking interrupts additionally excludes IRQ-context reentrancy.
unsafe impl<T: Send> Sync for IrqSpinLock<T> {}
// SAFETY: as `SpinLock` — no CPU-local state is captured in the lock.
unsafe impl<T: Send> Send for IrqSpinLock<T> {}

impl<T> IrqSpinLock<T> {
    /// Construct a new lock around `value`. Available in `const` context.
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            data: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock with interrupts masked. Masks **before** spinning so
    /// the hold window is fully interrupt-free (and the lock may be taken from
    /// IRQ context). Returns a guard that releases the lock and restores the
    /// prior interrupt state when dropped.
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T> {
        // Mask first: no window exists where the lock is held with interrupts
        // enabled, so an IRQ handler can never find it held by the context it
        // interrupted (single-CPU deadlock-freedom).
        let prev_if = irq_backend::disable();
        loop {
            if self
                .locked
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return IrqSpinLockGuard { lock: self, prev_if };
            }
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    /// Try to acquire the lock **without spinning**. Masks interrupts first (like
    /// [`lock`](Self::lock)); on success returns the guard, on contention restores
    /// the prior interrupt state and returns `None`. Used by `klog::push` so teeing
    /// kernel output into the log ring can never deadlock against a fault that
    /// strikes while the ring lock is held (the panic/exception path tees too).
    pub fn try_lock(&self) -> Option<IrqSpinLockGuard<'_, T>> {
        let prev_if = irq_backend::disable();
        if self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            Some(IrqSpinLockGuard { lock: self, prev_if })
        } else {
            // Contended: restore the interrupt state we masked above and give up.
            irq_backend::restore(prev_if);
            None
        }
    }

    /// Release the lock. Called from the guard's `Drop` and from
    /// [`release_keeping_irqs_masked`](IrqSpinLockGuard::release_keeping_irqs_masked).
    fn release(&self) {
        self.locked.store(false, Ordering::Release);
    }
}

/// RAII guard returned by [`IrqSpinLock::lock`]. While alive it proves the
/// lock is held and grants `&T` / `&mut T`; its `Drop` releases the lock and
/// restores the interrupt state captured at `lock()`.
pub struct IrqSpinLockGuard<'a, T> {
    lock: &'a IrqSpinLock<T>,
    /// Interrupt-enable state captured at acquire, restored on drop.
    prev_if: bool,
}

impl<'a, T> IrqSpinLockGuard<'a, T> {
    /// Release the lock but **keep interrupts masked**, returning the prior
    /// interrupt state the caller must later hand to
    /// [`Cpu::interrupts_restore`](crate::arch::cpu::ArchCpu::interrupts_restore).
    ///
    /// This is the scheduler's switch-window primitive: the run-queue lock
    /// must be dropped before a `context_switch` (the cardinal rank-1 rule),
    /// yet interrupts must stay masked **across** the stack switch (a timer
    /// IRQ mid-switch would corrupt a half-swapped stack). The caller restores
    /// the interrupt state once the switch completes (on resume).
    pub fn release_keeping_irqs_masked(self) -> bool {
        let prev = self.prev_if;
        self.lock.release();
        // Skip the `Drop` so interrupts are NOT restored here.
        core::mem::forget(self);
        prev
    }
}

impl<T> Deref for IrqSpinLockGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: the guard's existence proves the lock is held; no other
        // context can be inside the cell while `locked == true`.
        unsafe { &*self.lock.data.get() }
    }
}

impl<T> DerefMut for IrqSpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as `Deref`; `&mut self` proves no overlapping reborrow.
        unsafe { &mut *self.lock.data.get() }
    }
}

impl<T> Drop for IrqSpinLockGuard<'_, T> {
    fn drop(&mut self) {
        // Release the lock FIRST, then restore interrupts: if we restored IF
        // first, an IRQ could fire and legitimately take this same lock while
        // we still hold it — a self-deadlock on single-CPU.
        self.lock.release();
        irq_backend::restore(self.prev_if);
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

    // --- IrqSpinLock -------------------------------------------------------
    //
    // These exercise the lock's interrupt-flag save/restore *logic* against
    // the mock IF backend (`irq_backend::MOCK_IF`). The real `cli`/`sti` asm
    // is QEMU-only. The tests run serially and reset MOCK_IF first, since they
    // share the one global mock flag.

    use core::sync::atomic::Ordering;

    fn reset_mock_if(enabled: bool) {
        irq_backend::MOCK_IF.store(enabled, Ordering::SeqCst);
    }
    fn mock_if() -> bool {
        irq_backend::MOCK_IF.load(Ordering::SeqCst)
    }

    #[test]
    fn irq_lock_masks_while_held_and_restores_on_drop() {
        reset_mock_if(true);
        let lock = IrqSpinLock::new(0u32);
        {
            let mut g = lock.lock();
            // Interrupts masked for the whole critical section.
            assert!(!mock_if(), "IF must be masked while the lock is held");
            *g = 7;
        }
        // Prior state (enabled) restored on drop.
        assert!(mock_if(), "IF must be restored to its prior state on drop");
        assert_eq!(*lock.lock(), 7);
        // After this last guard drops, IF restored again.
        assert!(mock_if());
    }

    #[test]
    fn irq_lock_restores_masked_when_prior_was_masked() {
        // If interrupts were already off at acquire, drop must leave them off.
        reset_mock_if(false);
        let lock = IrqSpinLock::new(());
        {
            let _g = lock.lock();
            assert!(!mock_if());
        }
        assert!(!mock_if(), "prior masked state must be preserved");
    }

    #[test]
    fn irq_lock_guard_drop_releases() {
        reset_mock_if(true);
        let lock = IrqSpinLock::new(());
        let g = lock.lock();
        drop(g);
        // Must not deadlock — the bit was released.
        let _g2 = lock.lock();
    }

    #[test]
    fn release_keeping_irqs_masked_frees_lock_but_keeps_if_masked() {
        reset_mock_if(true);
        let lock = IrqSpinLock::new(5u32);
        let prev = {
            let g = lock.lock();
            assert!(!mock_if());
            g.release_keeping_irqs_masked()
        };
        // The captured prior state is "enabled" ...
        assert!(prev, "release_keeping_irqs_masked returns the prior IF state");
        // ... but interrupts remain masked (NOT restored) — the caller owns it.
        assert!(!mock_if(), "interrupts must stay masked after release_keeping_irqs_masked");
        // The lock itself is free (re-lockable without deadlock).
        let g2 = lock.lock();
        assert_eq!(*g2, 5);
        drop(g2);
        // Tidy up the shared mock flag for any later test.
        reset_mock_if(true);
    }

    #[test]
    fn irq_lock_mutates_through_deref_mut_and_const_in_static() {
        reset_mock_if(true);
        static S: IrqSpinLock<u32> = IrqSpinLock::new(3);
        {
            let mut g = S.lock();
            *g += 4;
        }
        assert_eq!(*S.lock(), 7);
    }
}
