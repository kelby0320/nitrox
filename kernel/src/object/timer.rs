//! The [`Timer`] kernel object — a waitable monotonic-deadline source.
//!
//! A `Timer` holds an armed `deadline_ns` (absolute monotonic nanoseconds, the
//! same clock as [`arch::Timer::read_ns`](crate::arch::Timer)) and an
//! `interval_ns` (`0` = one-shot, else the re-arm period after each fire), plus
//! the set of threads currently blocked in `sys_wait` on it. `sys_timer_set`
//! arms it; the periodic scheduler tick fires it when the deadline elapses,
//! waking its waiters (see [`crate::sched`]).
//!
//! ## Mutation discipline
//!
//! Like [`Thread`](crate::object::Thread), a `Timer` is shared through an
//! [`ObjectRef`](crate::object::ObjectRef) yet its deadline/interval/waiter set
//! are mutated by the scheduler. That is sound only because **all** of a
//! Timer's interior state lives in an [`UnsafeCell`] touched **exclusively
//! while the rank-1 `SCHED` lock is held** (single-CPU serialisation; see
//! `kernel/docs/lock-ordering.md`). The `pub(crate) unsafe fn` accessors below
//! take a type-erased `*mut ()` and reach the interior through that cell — the
//! same raw-pointer discipline the `Thread` scheduler accessors use — so no
//! aliasing `&mut Timer` is ever formed.

use core::cell::UnsafeCell;

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec};
use crate::object::header::KObjectHeader;

/// A timer kernel object.
///
/// `#[repr(C)]` with [`KObjectHeader`] first — see [`crate::object::header`].
#[repr(C)]
pub struct Timer {
    header: KObjectHeader,
    /// Self-check sentinel; a live object always reads [`Timer::MAGIC`].
    magic: u64,
    /// All mutable state, reached only under `SCHED` (see "Mutation discipline").
    inner: UnsafeCell<TimerInner>,
}

/// A `Timer`'s scheduler-owned interior.
struct TimerInner {
    /// Absolute monotonic-ns deadline; `0` = not armed.
    deadline_ns: u64,
    /// Re-arm period after each fire; `0` = one-shot.
    interval_ns: u64,
    /// `true` while this timer has a live entry in the scheduler deadline heap.
    in_heap: bool,
    /// Threads blocked on this timer, as type-erased `Thread` object pointers
    /// (non-owning — each waiter is kept alive by its own parked `ObjectRef`
    /// in the scheduler `blocked` list, and is removed from here before it
    /// unparks). Pre-reserved at [`Timer::try_new`] to [`Timer::MAX_WAITERS`];
    /// `add_waiter` never grows it under the lock.
    waiters: KVec<*mut ()>,
}

// SAFETY: identical reasoning to `ObjectRef`/`Thread` — the header refcount is
// atomic, and every access to `inner` is serialised under the single-CPU
// `SCHED` lock, so sharing/moving a `Timer` across contexts cannot race.
unsafe impl Send for Timer {}
// SAFETY: as `Send`.
unsafe impl Sync for Timer {}

impl Timer {
    /// Sentinel written into [`Timer::magic`] at construction.
    pub const MAGIC: u64 = 0x54_69_6d_65_72_21_21_21; // "Timer!!!"

    /// Maximum simultaneous waiters on one timer. Bounds the pre-reserved
    /// waiter vector so `add_waiter` never allocates under `SCHED`; a timer
    /// almost always has exactly one waiter.
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an unarmed `Timer` with a refcount of one. The waiter vector is
    /// reserved up front (the only fallible growth) so later `add_waiter`s stay
    /// within capacity and never allocate under the scheduler lock.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let mut waiters: KVec<*mut ()> = KVec::new();
        waiters.try_reserve(Self::MAX_WAITERS)?;
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Timer),
            magic: Self::MAGIC,
            inner: UnsafeCell::new(TimerInner {
                deadline_ns: 0,
                interval_ns: 0,
                in_heap: false,
                waiters,
            }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    // --- Scheduler-only accessors --------------------------------------
    //
    // SAFETY (shared by all): `obj` addresses a live `Timer` (pinned by an
    // `ObjectRef` the caller holds), and the caller holds the `SCHED` lock,
    // which — single-CPU — serialises all access to `inner`.

    /// Borrow the interior mutably. The returned reference must not outlive the
    /// `SCHED` hold and must not be aliased.
    ///
    /// # Safety
    /// See the accessor contract above.
    #[allow(clippy::mut_from_ref)]
    unsafe fn inner<'a>(obj: *mut ()) -> &'a mut TimerInner {
        // SAFETY: `obj` is a live Timer; forming a shared `&Timer` to reach the
        // `UnsafeCell`, then a `&mut TimerInner` through it, is exactly the
        // interior-mutability contract — sound while `SCHED` serialises access.
        let t = unsafe { &*(obj as *const Timer) };
        unsafe { &mut *t.inner.get() }
    }

    /// Arm (or disarm, with `deadline_ns == 0`) the timer.
    /// # Safety: see the accessor contract.
    pub(crate) unsafe fn set_armed(obj: *mut (), deadline_ns: u64, interval_ns: u64) {
        let inner = unsafe { Self::inner(obj) };
        inner.deadline_ns = deadline_ns;
        inner.interval_ns = interval_ns;
    }

    /// The armed deadline (`0` = unarmed). Test-only (production reads the
    /// deadline via the heap, not the object).
    /// # Safety: see the accessor contract.
    #[cfg(test)]
    pub(crate) unsafe fn deadline(obj: *mut ()) -> u64 {
        unsafe { Self::inner(obj) }.deadline_ns
    }

    /// The re-arm interval (`0` = one-shot).
    /// # Safety: see the accessor contract.
    pub(crate) unsafe fn interval(obj: *mut ()) -> u64 {
        unsafe { Self::inner(obj) }.interval_ns
    }

    /// Set the "present in the deadline heap" flag.
    /// # Safety: see the accessor contract.
    pub(crate) unsafe fn set_in_heap(obj: *mut (), v: bool) {
        unsafe { Self::inner(obj) }.in_heap = v;
    }

    /// `true` iff this timer has a live deadline-heap entry.
    /// # Safety: see the accessor contract.
    pub(crate) unsafe fn in_heap(obj: *mut ()) -> bool {
        unsafe { Self::inner(obj) }.in_heap
    }

    /// Register `thread` as a waiter. `Err(())` if already at
    /// [`MAX_WAITERS`](Self::MAX_WAITERS) (caller maps to `OutOfMemory`); never
    /// grows the vector under the lock.
    /// # Safety: see the accessor contract.
    pub(crate) unsafe fn add_waiter(obj: *mut (), thread: *mut ()) -> Result<(), ()> {
        let inner = unsafe { Self::inner(obj) };
        if inner.waiters.len() < Self::MAX_WAITERS {
            inner
                .waiters
                .try_push(thread)
                .expect("within reserved waiter capacity");
            Ok(())
        } else {
            Err(())
        }
    }

    /// Remove `thread` from the waiter set if present (idempotent).
    /// # Safety: see the accessor contract.
    pub(crate) unsafe fn remove_waiter(obj: *mut (), thread: *mut ()) {
        let inner = unsafe { Self::inner(obj) };
        if let Some(i) = inner.waiters.iter().position(|&w| w == thread) {
            inner.waiters.remove(i);
        }
    }

    /// `true` iff the timer is armed and its deadline is at or before `now`.
    /// # Safety: see the accessor contract.
    pub(crate) unsafe fn already_signaled(obj: *mut (), now_ns: u64) -> bool {
        let inner = unsafe { Self::inner(obj) };
        inner.deadline_ns != 0 && inner.deadline_ns <= now_ns
    }

    /// Drain every waiter into `out` (clearing the set), returning the count.
    /// `out` must be at least [`MAX_WAITERS`](Self::MAX_WAITERS) long.
    /// # Safety: see the accessor contract.
    pub(crate) unsafe fn take_waiters(obj: *mut (), out: &mut [*mut ()]) -> usize {
        let inner = unsafe { Self::inner(obj) };
        let n = inner.waiters.len();
        debug_assert!(out.len() >= n);
        for (i, &w) in inner.waiters.iter().enumerate() {
            out[i] = w;
        }
        inner.waiters.clear();
        n
    }
}

impl Drop for Timer {
    /// A live waiter pins this timer (each holds an `ObjectRef` on it across
    /// `sys_wait`), so the last reference cannot drop while waiters remain —
    /// the assert documents that invariant. The `KVec` storage frees with the
    /// `KBox`.
    fn drop(&mut self) {
        debug_assert!(
            self.inner.get_mut().waiters.is_empty(),
            "Timer dropped with live waiters"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;
    use crate::object::ObjectRef;
    use crate::object::header::test_probe;

    #[test]
    fn try_new_is_unarmed_with_magic() {
        init_global_heap();
        let t = Timer::try_new().unwrap();
        assert!(t.magic_ok());
        let obj = KBox::into_raw(t).as_ptr() as *mut ();
        // SAFETY: live Timer, single-threaded test (stands in for SCHED).
        unsafe {
            assert_eq!(Timer::deadline(obj), 0);
            assert_eq!(Timer::interval(obj), 0);
            assert!(!Timer::in_heap(obj));
            assert!(!Timer::already_signaled(obj, u64::MAX));
            drop(KBox::<Timer>::from_raw(core::ptr::NonNull::new_unchecked(
                obj as *mut Timer,
            )));
        }
    }

    #[test]
    fn arm_and_signal_predicate() {
        init_global_heap();
        let obj = KBox::into_raw(Timer::try_new().unwrap()).as_ptr() as *mut ();
        // SAFETY: live Timer, single-threaded test.
        unsafe {
            Timer::set_armed(obj, 1000, 0);
            assert_eq!(Timer::deadline(obj), 1000);
            assert!(!Timer::already_signaled(obj, 999));
            assert!(Timer::already_signaled(obj, 1000));
            assert!(Timer::already_signaled(obj, 1001));
            Timer::set_armed(obj, 0, 0); // disarm
            assert!(!Timer::already_signaled(obj, u64::MAX));
            drop(KBox::<Timer>::from_raw(core::ptr::NonNull::new_unchecked(
                obj as *mut Timer,
            )));
        }
    }

    #[test]
    fn waiters_add_remove_take_caps_at_max() {
        init_global_heap();
        let obj = KBox::into_raw(Timer::try_new().unwrap()).as_ptr() as *mut ();
        // SAFETY: live Timer, single-threaded test.
        unsafe {
            let ths: [*mut (); Timer::MAX_WAITERS] =
                core::array::from_fn(|i| (0x1000 + i) as *mut ());
            for &t in &ths {
                assert!(Timer::add_waiter(obj, t).is_ok());
            }
            // Over capacity -> Err, no growth.
            assert!(Timer::add_waiter(obj, 0xDEAD as *mut ()).is_err());
            // Remove one, re-add succeeds.
            Timer::remove_waiter(obj, ths[1]);
            assert!(Timer::add_waiter(obj, 0xBEEF as *mut ()).is_ok());
            // Drain.
            let mut buf = [core::ptr::null_mut(); Timer::MAX_WAITERS];
            let n = Timer::take_waiters(obj, &mut buf);
            assert_eq!(n, Timer::MAX_WAITERS);
            // Empty after drain (so Drop's assert holds).
            let mut buf2 = [core::ptr::null_mut(); Timer::MAX_WAITERS];
            assert_eq!(Timer::take_waiters(obj, &mut buf2), 0);
            drop(KBox::<Timer>::from_raw(core::ptr::NonNull::new_unchecked(
                obj as *mut Timer,
            )));
        }
    }

    #[test]
    fn dispatch_destroy_runs_timer_arm() {
        init_global_heap();
        test_probe::reset();
        let obj = KBox::into_raw(Timer::try_new().unwrap()).as_ptr() as *mut ();
        // SAFETY: `obj` carries the single creation reference.
        let r = unsafe { ObjectRef::from_raw(obj, KObjectType::Timer) };
        assert_eq!(test_probe::timer_destroys(), 0);
        drop(r);
        assert_eq!(test_probe::timer_destroys(), 1);
    }
}
