//! The [`Timer`] kernel object — a waitable monotonic-deadline source.
//!
//! Holds an armed `deadline_ns` (absolute monotonic ns, the clock of
//! [`arch::Timer::read_ns`](crate::arch::Timer)), an `interval_ns` (`0` = one-shot) and the
//! threads blocked in `sys_wait` on it. The scheduler tick fires it when the deadline elapses,
//! waking them (see [`crate::sched`]).
//!
//! ## Mutation discipline
//!
//! A `Timer` is shared through an [`ObjectRef`](crate::object::ObjectRef) yet mutated by the
//! scheduler, which is sound only because **all** its interior state lives in an
//! [`UnsafeCell`] touched **exclusively under the rank-1 `SCHED` lock**
//! (`kernel/docs/lock-ordering.md`). The `pub(crate) unsafe fn` accessors reach the cell
//! through a type-erased `*mut ()`, so no aliasing `&mut Timer` is ever formed.

use core::cell::UnsafeCell;

use crate::libkern::handle::KObjectType;
use crate::libkern::{AllocError, KBox, KVec};
use crate::object::header::KObjectHeader;

/// A timer kernel object (`#[repr(C)]`, [`KObjectHeader`] first).
#[repr(C)]
pub struct Timer {
    header: KObjectHeader,
    magic: u64,
    inner: UnsafeCell<TimerInner>,
}

struct TimerInner {
    /// Absolute monotonic-ns deadline; `0` = not armed.
    deadline_ns: u64,
    /// Re-arm period after each fire; `0` = one-shot.
    interval_ns: u64,
    /// `true` while this timer has a live entry in the scheduler deadline heap.
    in_heap: bool,
    /// Blocked threads, non-owning (each is kept alive by its own parked `ObjectRef` and removed
    /// from here before it unparks); pre-reserved to [`Timer::MAX_WAITERS`].
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

    /// Maximum simultaneous waiters; the reserve keeps `add_waiter` from allocating under `SCHED`.
    pub const MAX_WAITERS: usize = 4;

    /// Allocate an unarmed `Timer` (refcount one), reserving the waiter vector up front.
    pub fn try_new() -> Result<KBox<Self>, AllocError> {
        let mut waiters: KVec<*mut ()> = KVec::new();
        waiters.try_reserve(Self::MAX_WAITERS)?;
        KBox::try_new(Self {
            header: KObjectHeader::new(KObjectType::Timer),
            magic: Self::MAGIC,
            inner: UnsafeCell::new(TimerInner { deadline_ns: 0, interval_ns: 0, in_heap: false, waiters }),
        })
    }

    /// `true` iff the self-check sentinel is intact.
    pub fn magic_ok(&self) -> bool {
        self.magic == Self::MAGIC
    }

    // SAFETY (shared by all): `obj` is a live `Timer` pinned by the caller's `ObjectRef`, and
    // the caller holds `SCHED`, which serialises all access to `inner`.

    /// Borrow the interior mutably; must not outlive the `SCHED` hold nor alias.
    ///
    /// # Safety
    /// See the accessor contract above.
    #[allow(clippy::mut_from_ref, reason = "this accessor's contract makes the `&mut` the only reference")]
    unsafe fn inner<'a>(obj: *mut ()) -> &'a mut TimerInner {
        // SAFETY: `obj` is a live Timer; forming a shared `&Timer` to reach the
        // `UnsafeCell`, then a `&mut TimerInner` through it, is exactly the
        // interior-mutability contract — sound while `SCHED` serialises access.
        let t = unsafe { &*(obj as *const Timer) };
        // SAFETY: as above.
        unsafe { &mut *t.inner.get() }
    }

    /// Arm (or disarm, with `deadline_ns == 0`) the timer.
    ///
    /// # Safety
    ///
    /// See the accessor contract.
    pub(crate) unsafe fn set_armed(obj: *mut (), deadline_ns: u64, interval_ns: u64) {
        // SAFETY: the accessor contract: `obj` is a live `Timer` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        inner.deadline_ns = deadline_ns;
        inner.interval_ns = interval_ns;
    }

    /// The armed deadline (`0` = unarmed). Test-only; production reads it from the heap.
    ///
    /// # Safety
    ///
    /// See the accessor contract.
    #[cfg(test)]
    pub(crate) unsafe fn deadline(obj: *mut ()) -> u64 {
        // SAFETY: the accessor contract: `obj` is a live `Timer` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.deadline_ns
    }

    /// The re-arm interval (`0` = one-shot).
    ///
    /// # Safety
    ///
    /// See the accessor contract.
    pub(crate) unsafe fn interval(obj: *mut ()) -> u64 {
        // SAFETY: the accessor contract: `obj` is a live `Timer` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.interval_ns
    }

    /// Set the "present in the deadline heap" flag.
    ///
    /// # Safety
    ///
    /// See the accessor contract.
    pub(crate) unsafe fn set_in_heap(obj: *mut (), v: bool) {
        // SAFETY: the accessor contract: `obj` is a live `Timer` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.in_heap = v;
    }

    /// `true` iff this timer has a live deadline-heap entry.
    ///
    /// # Safety
    ///
    /// See the accessor contract.
    pub(crate) unsafe fn in_heap(obj: *mut ()) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `Timer` its caller pins, and `SCHED` is held.
        unsafe { Self::inner(obj) }.in_heap
    }

    /// Register `thread` as a waiter; `Err(())` at the cap, which callers map to `OutOfMemory`.
    ///
    /// # Safety
    ///
    /// See the accessor contract.
    pub(crate) unsafe fn add_waiter(obj: *mut (), thread: *mut ()) -> Result<(), ()> {
        // SAFETY: the accessor contract: `obj` is a live `Timer` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if inner.waiters.len() < Self::MAX_WAITERS {
            inner
                .waiters
                // `push_within_capacity`, not `try_push`: this runs under `SCHED`, and `try_push`
                // grows, which is `kmalloc` under the rank-1 lock. Construction reserves
                // `MAX_WAITERS`, so `len < MAX ⟹ len < cap`; the type should say so.
                .push_within_capacity(thread)
                .expect("within reserved waiter capacity");
            Ok(())
        } else {
            Err(())
        }
    }

    /// Remove `thread` from the waiter set if present (idempotent).
    ///
    /// # Safety
    ///
    /// See the accessor contract.
    pub(crate) unsafe fn remove_waiter(obj: *mut (), thread: *mut ()) {
        // SAFETY: the accessor contract: `obj` is a live `Timer` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        if let Some(i) = inner.waiters.iter().position(|&w| w == thread) {
            inner.waiters.remove(i);
        }
    }

    /// `true` iff the timer is armed and its deadline is at or before `now`, or it is a one-shot
    /// the tick has already fired: out of the heap with its deadline kept, the one state that
    /// means so. A waiter's `now` is read before it takes `SCHED`, and may predate that fire.
    ///
    /// # Safety
    ///
    /// See the accessor contract.
    pub(crate) unsafe fn already_signaled(obj: *mut (), now_ns: u64) -> bool {
        // SAFETY: the accessor contract: `obj` is a live `Timer` its caller pins, and `SCHED` is held.
        let inner = unsafe { Self::inner(obj) };
        let fired = inner.interval_ns == 0 && !inner.in_heap;
        inner.deadline_ns != 0 && (inner.deadline_ns <= now_ns || fired)
    }

    /// Drain every waiter into `out` (at least [`MAX_WAITERS`](Self::MAX_WAITERS) long).
    ///
    /// # Safety
    ///
    /// See the accessor contract.
    pub(crate) unsafe fn take_waiters(obj: *mut (), out: &mut [*mut ()]) -> usize {
        // SAFETY: the accessor contract: `obj` is a live `Timer` its caller pins, and `SCHED` is held.
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
    /// Take the timer out of the deadline heap, which names it by address. A live waiter pins
    /// it (it holds an `ObjectRef` across `sys_wait`), so the last reference cannot drop while
    /// waiters remain; the assert documents that.
    fn drop(&mut self) {
        // SAFETY: `self` is being dropped: its memory whole, its last reference gone.
        unsafe { crate::sched::timer_closing(self as *mut Self as *mut ()) };
        debug_assert!(self.inner.get_mut().waiters.is_empty(), "Timer dropped with live waiters");
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
            drop(KBox::<Timer>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Timer)));
        }
    }

    #[test]
    fn arm_and_signal_predicate() {
        init_global_heap();
        let obj = KBox::into_raw(Timer::try_new().unwrap()).as_ptr() as *mut ();
        // SAFETY: live Timer, single-threaded test.
        unsafe {
            Timer::set_armed(obj, 1000, 0); // armed as `timer_arm` arms: into the heap
            Timer::set_in_heap(obj, true);
            assert_eq!(Timer::deadline(obj), 1000);
            assert!(!Timer::already_signaled(obj, 999));
            assert!(Timer::already_signaled(obj, 1000));
            assert!(Timer::already_signaled(obj, 1001));
            Timer::set_armed(obj, 0, 0); // disarm
            Timer::set_in_heap(obj, false);
            assert!(!Timer::already_signaled(obj, u64::MAX));
            drop(KBox::<Timer>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Timer)));
        }
    }

    #[test]
    fn waiters_add_remove_take_caps_at_max() {
        init_global_heap();
        let obj = KBox::into_raw(Timer::try_new().unwrap()).as_ptr() as *mut ();
        // SAFETY: live Timer, single-threaded test.
        unsafe {
            let ths: [*mut (); Timer::MAX_WAITERS] = core::array::from_fn(|i| (0x1000 + i) as *mut ());
            for &t in &ths {
                assert!(Timer::add_waiter(obj, t).is_ok());
            }
            assert!(Timer::add_waiter(obj, 0xDEAD as *mut ()).is_err());
            Timer::remove_waiter(obj, ths[1]);
            assert!(Timer::add_waiter(obj, 0xBEEF as *mut ()).is_ok());
            let mut buf = [core::ptr::null_mut(); Timer::MAX_WAITERS];
            let n = Timer::take_waiters(obj, &mut buf);
            assert_eq!(n, Timer::MAX_WAITERS);
            let mut buf2 = [core::ptr::null_mut(); Timer::MAX_WAITERS];
            assert_eq!(Timer::take_waiters(obj, &mut buf2), 0);
            drop(KBox::<Timer>::from_raw(core::ptr::NonNull::new_unchecked(obj as *mut Timer)));
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
