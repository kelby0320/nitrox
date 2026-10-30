//! Test-and-set spin locks: a plain [`SpinLock`] and an interrupt-safe [`IrqSpinLock`].
//! See `kernel/docs/lock-ordering.md` for where each lock sits in the rank ordering.
//!
//! [`SpinLock`] does **not** mask interrupts. It is correct for data never touched from an
//! interrupt handler (the allocators, the kernel vmap, the handle table, address spaces): a
//! timer IRQ that preempts a holder never tries to take it.
//!
//! [`IrqSpinLock`] is for data shared between thread and IRQ context (`SCHED`, `SERIAL`). It
//! captures the prior interrupt-enable state and `cli`s **before** spinning, so an IRQ cannot
//! fire while the lock is held and a handler can never find it held by the context it
//! interrupted.

use crate::libkern::lockrank::{LockRank, Taken};
use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A test-and-set spin lock guarding a `T`, reachable only through the guard
/// [`SpinLock::lock`] returns. `SpinLock::new` is `const`.
pub struct SpinLock<T> {
    locked: AtomicBool,
    /// Where this lock sits in the acquisition order. Checked on every acquire in debug
    /// builds; inert in release builds. See `kernel/docs/lock-ordering.md`.
    rank: LockRank,
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
    /// Construct a new lock around `value` at `rank`. Available in `const` context.
    ///
    /// The rank is **mandatory**: a lock with no declared position in the order is a lock
    /// nobody has reasoned about (`kernel/docs/lock-ordering.md` § Adding a new lock).
    pub const fn new(rank: LockRank, value: T) -> Self {
        Self { locked: AtomicBool::new(false), rank, data: UnsafeCell::new(value) }
    }

    /// Acquire the lock, spinning until it becomes available.
    ///
    /// The critical section is a **no-preemption region**
    /// ([`sched::preempt_disable`](crate::sched::preempt_disable), raised before the acquire
    /// attempt so the winner cannot be descheduled between winning and recording; lowered by the
    /// guard drop after release). That is the SMP holder-liveness invariant: a holder descheduled
    /// mid-section strands every spinner — forever when the spinner has interrupts masked (it
    /// cannot tick to reschedule the holder, nor ack a TLB shootdown) or when the holder is the
    /// idle thread. With preemption off, a spinner always waits on a *running* holder.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        crate::sched::preempt_disable();
        loop {
            if self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                crate::libkern::lockrank::acquired(self.rank, Taken::Waiting);
                return SpinLockGuard { lock: self, _here: PhantomData };
            }
            // Spin on a relaxed read, not the CAS, so the cacheline is not bus-locked.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    fn release(&self) {
        self.locked.store(false, Ordering::Release);
    }

    /// Borrow the protected value **without locking**, given exclusive access: `&mut self`
    /// proves no other reference exists, so the atomic and the rank check would be theatre. No
    /// critical section, so none of [`lock`](Self::lock)'s preemption discipline applies.
    ///
    /// **It exists because taking the lock is not always harmless**: a `Drop` impl that locks
    /// itself and then releases the objects it owns acquires a *second* lock of the same rank from
    /// inside the first, a genuine ordering violation.
    pub fn get_mut(&mut self) -> &mut T {
        // SAFETY: `&mut self` is exclusive access to the whole lock, so no guard can exist
        // and no other CPU can hold a reference to the contents.
        unsafe { &mut *self.data.get() }
    }
}

/// RAII guard returned by [`SpinLock::lock`]; proves the lock is held and grants `&T` /
/// `&mut T` to the inner value.
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
    /// Not `Send`: it is released on the CPU whose preemption count and lock ranks it raised.
    _here: PhantomData<*const ()>,
}

// SAFETY: a shared guard gives out `&T` only, so it may be shared where `T` may.
unsafe impl<T: Sync> Sync for SpinLockGuard<'_, T> {}

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
        // Drop the rank first: `preempt_enable` below can replay a deferred reschedule,
        // which takes `SCHED` — that acquire must not see this lock still recorded.
        crate::libkern::lockrank::releasing(self.lock.rank);
        // Release first, then re-enable preemption: a deferred reschedule replayed by
        // `preempt_enable` must not run while this lock is held.
        self.lock.release();
        crate::sched::preempt_enable();
    }
}

/// Interrupt-flag backend for [`IrqSpinLock`].
///
/// On target, this drives the real CPU interrupt flag through the arch layer. Under
/// `cfg(test)` (host ring 3) the real `cli`/`sti` would `#GP`, so a mock models `IF` in a
/// **per-thread** `Cell<bool>`, enough to exercise the lock's save/restore *logic*.
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
    std::thread_local! {
        /// Models `RFLAGS.IF` for host tests, **one flag per thread**. Starts enabled (the
        /// boot default once preemption is armed).
        ///
        /// Per-thread because `IF` *is* per-CPU: a process-global flag is a machine where any
        /// core's `cli` masks interrupts on all of them, and a host test thread stands in for a
        /// core. `cargo test` runs tests concurrently, so a global flag let one test's write fail
        /// `irq_lock_masks_while_held_and_restores_on_drop`.
        static MOCK_IF: core::cell::Cell<bool> = const { core::cell::Cell::new(true) };
    }

    pub(super) fn disable() -> bool {
        MOCK_IF.with(|f| f.replace(false))
    }

    pub(super) fn restore(prev: bool) {
        MOCK_IF.with(|f| f.set(prev));
    }

    /// The modelled flag, for tests asserting on it.
    pub(super) fn peek() -> bool {
        MOCK_IF.with(|f| f.get())
    }

    /// Establish a prior interrupt state before acquiring, for tests that need one.
    pub(super) fn force(enabled: bool) {
        MOCK_IF.with(|f| f.set(enabled));
    }
}

/// A spin lock that masks interrupts for the duration of the critical section: the guard's
/// `Drop` releases the lock and **then** restores the prior interrupt state.
pub struct IrqSpinLock<T> {
    locked: AtomicBool,
    /// Where this lock sits in the acquisition order (see `SpinLock::rank`).
    rank: LockRank,
    data: UnsafeCell<T>,
}

// SAFETY: identical reasoning to `SpinLock` — the `locked` AtomicBool
// serialises access to the inner cell; only the winning context holds the
// guard. Masking interrupts additionally excludes IRQ-context reentrancy.
unsafe impl<T: Send> Sync for IrqSpinLock<T> {}
// SAFETY: as `SpinLock` — no CPU-local state is captured in the lock.
unsafe impl<T: Send> Send for IrqSpinLock<T> {}

impl<T> IrqSpinLock<T> {
    /// Construct a new lock around `value` at `rank`. Available in `const` context.
    /// The rank is mandatory for the reason given on [`SpinLock::new`].
    pub const fn new(rank: LockRank, value: T) -> Self {
        Self { locked: AtomicBool::new(false), rank, data: UnsafeCell::new(value) }
    }

    /// Acquire the lock with interrupts masked. Masks **before** spinning so the hold window
    /// is fully interrupt-free (and the lock may be taken from IRQ context). Returns a guard that
    /// releases the lock and restores the prior interrupt state when dropped.
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T> {
        // Mask first: no window exists where the lock is held with interrupts enabled, so an
        // IRQ handler can never find it held by the context it interrupted.
        let prev_if = irq_backend::disable();
        loop {
            if self.locked.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
                crate::libkern::lockrank::acquired(self.rank, Taken::Waiting);
                return IrqSpinLockGuard { lock: self, prev_if, _here: PhantomData };
            }
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
    }

    /// Try to acquire the lock **without spinning**. Masks interrupts first (like
    /// [`lock`](Self::lock)); on contention restores the prior interrupt state and returns
    /// `None`. Used by `klog::push` so teeing kernel output into the log ring can never deadlock
    /// against a fault that strikes while the ring lock is held.
    ///
    /// **Recorded by the rank tracker but not checked against the order**, because an
    /// acquisition that cannot wait cannot deadlock — see `lockrank` § Only an acquisition
    /// that waits is ordered. That is what lets the panic path tee from under any lock.
    pub fn try_lock(&self) -> Option<IrqSpinLockGuard<'_, T>> {
        let prev_if = irq_backend::disable();
        if self.locked.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_ok() {
            crate::libkern::lockrank::acquired(self.rank, Taken::WithoutWaiting);
            Some(IrqSpinLockGuard { lock: self, prev_if, _here: PhantomData })
        } else {
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

/// RAII guard returned by [`IrqSpinLock::lock`]; its `Drop` releases the lock and restores
/// the interrupt state captured at `lock()`.
pub struct IrqSpinLockGuard<'a, T> {
    lock: &'a IrqSpinLock<T>,
    /// Interrupt-enable state captured at acquire, restored on drop.
    prev_if: bool,
    /// Not `Send`: it restores the interrupt state of the CPU it was taken on.
    _here: PhantomData<*const ()>,
}

// SAFETY: as `SpinLockGuard`'s.
unsafe impl<T: Sync> Sync for IrqSpinLockGuard<'_, T> {}

impl<'a, T> IrqSpinLockGuard<'a, T> {
    /// Release the lock but **keep interrupts masked**, returning the prior interrupt state
    /// the caller must later hand to
    /// [`Cpu::interrupts_restore`](crate::arch::cpu::ArchCpu::interrupts_restore).
    ///
    /// This is the scheduler's switch-window primitive: the run-queue lock must be dropped
    /// before a `context_switch`, yet interrupts must stay masked **across** the stack switch
    /// (a timer IRQ mid-switch would corrupt a half-swapped stack).
    pub fn release_keeping_irqs_masked(self) -> bool {
        let prev = self.prev_if;
        // This path skips `Drop`, so it must drop the rank itself; missing it would leak a
        // held rank on every context switch and report phantom inversions.
        crate::libkern::lockrank::releasing(self.lock.rank);
        self.lock.release();
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
        // Drop the rank before the lock, so an IRQ taken the instant interrupts come back
        // sees an accurate held set.
        crate::libkern::lockrank::releasing(self.lock.rank);
        // Release the lock FIRST, then restore interrupts: restoring IF first would let an
        // IRQ take this same lock while we still hold it.
        self.lock.release();
        irq_backend::restore(self.prev_if);
    }
}

/// Fail the build if `$t` implements `$tr`: the probe's call is ambiguous exactly when it does.
macro_rules! assert_not_impl {
    ($t:ty: $tr:path) => {
        const _: fn() = || {
            trait AmbiguousIfImpl<A> {
                fn probe() {}
            }
            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
            struct Implements;
            impl<T: ?Sized + $tr> AmbiguousIfImpl<Implements> for T {}
            <$t as AmbiguousIfImpl<_>>::probe();
        };
    };
}
assert_not_impl!(SpinLockGuard<'static, u32>: Send);
assert_not_impl!(IrqSpinLockGuard<'static, u32>: Send);
assert_not_impl!(SpinLockGuard<'static, core::cell::Cell<u32>>: Sync);
assert_not_impl!(IrqSpinLockGuard<'static, core::cell::Cell<u32>>: Sync);
const _: fn() = || {
    fn shared<T: Sync>() {}
    shared::<SpinLockGuard<'static, u32>>();
    shared::<IrqSpinLockGuard<'static, u32>>();
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_grants_exclusive_access_in_single_thread() {
        let lock = SpinLock::new(LockRank::Leaf, 0u32);
        {
            let mut g = lock.lock();
            *g = 42;
        }
        let g = lock.lock();
        assert_eq!(*g, 42);
    }

    #[test]
    fn guard_drop_releases_lock() {
        let lock = SpinLock::new(LockRank::Leaf, ());
        let g = lock.lock();
        drop(g);
        let _g2 = lock.lock();
    }

    #[test]
    fn mutates_through_deref_mut() {
        let lock = SpinLock::new(LockRank::Leaf, [0u8; 4]);
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
        static S: SpinLock<u32> = SpinLock::new(LockRank::Leaf, 7);
        let g = S.lock();
        assert_eq!(*g, 7);
    }

    // The contested path (one CPU spinning while another holds the lock) is not exercised
    // by host tests: the std-thread version would have different semantics from the on-target
    // one. An integration test under QEMU with SMP is the way to cover it.

    // --- IrqSpinLock -------------------------------------------------------
    //
    // These exercise the lock's interrupt-flag save/restore *logic* against the mock IF
    // backend; the real `cli`/`sti` asm is QEMU-only. The flag is per-thread and libtest
    // spawns a fresh OS thread per test, so tests never share one.

    fn reset_mock_if(enabled: bool) {
        irq_backend::force(enabled);
    }
    fn mock_if() -> bool {
        irq_backend::peek()
    }

    #[test]
    fn irq_lock_masks_while_held_and_restores_on_drop() {
        reset_mock_if(true);
        let lock = IrqSpinLock::new(LockRank::Leaf, 0u32);
        {
            let mut g = lock.lock();
            assert!(!mock_if(), "IF must be masked while the lock is held");
            *g = 7;
        }
        assert!(mock_if(), "IF must be restored to its prior state on drop");
        assert_eq!(*lock.lock(), 7);
        assert!(mock_if());
    }

    #[test]
    fn irq_lock_restores_masked_when_prior_was_masked() {
        reset_mock_if(false);
        let lock = IrqSpinLock::new(LockRank::Leaf, ());
        {
            let _g = lock.lock();
            assert!(!mock_if());
        }
        assert!(!mock_if(), "prior masked state must be preserved");
    }

    #[test]
    fn irq_lock_guard_drop_releases() {
        reset_mock_if(true);
        let lock = IrqSpinLock::new(LockRank::Leaf, ());
        let g = lock.lock();
        drop(g);
        let _g2 = lock.lock();
    }

    #[test]
    fn release_keeping_irqs_masked_frees_lock_but_keeps_if_masked() {
        reset_mock_if(true);
        let lock = IrqSpinLock::new(LockRank::Leaf, 5u32);
        let prev = {
            let g = lock.lock();
            assert!(!mock_if());
            g.release_keeping_irqs_masked()
        };
        assert!(prev, "release_keeping_irqs_masked returns the prior IF state");
        // ... but interrupts remain masked (NOT restored) — the caller owns it.
        assert!(!mock_if(), "interrupts must stay masked after release_keeping_irqs_masked");
        let g2 = lock.lock();
        assert_eq!(*g2, 5);
        drop(g2);
    }

    #[test]
    fn irq_lock_mutates_through_deref_mut_and_const_in_static() {
        reset_mock_if(true);
        static S: IrqSpinLock<u32> = IrqSpinLock::new(LockRank::Leaf, 3);
        {
            let mut g = S.lock();
            *g += 4;
        }
        assert_eq!(*S.lock(), 7);
    }
}
