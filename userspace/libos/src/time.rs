//! The clocks, a [`Deadline`], and sleeping.

use libkern::{CLOCK_MONOTONIC, CLOCK_REALTIME, Rights};

use crate::error::{Result, check};
use crate::handle::{Handle, Only, Timer};
use crate::sys;

/// When a wait gives up: a monotonic time in nanoseconds, or never, or at once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Deadline(u64);

impl Deadline {
    /// Look, and do not wait.
    pub const POLL: Deadline = Deadline(0);
    /// Wait for as long as it takes.
    pub const NEVER: Deadline = Deadline(u64::MAX);

    /// At monotonic time `ns`: one already past gives up at the first look, never as a poll's
    /// `WouldBlock`, and one at the end of time is [`NEVER`](Self::NEVER).
    pub const fn at(ns: u64) -> Deadline {
        Deadline(if ns == 0 { 1 } else { ns })
    }

    /// `ns` from now.
    pub fn after(ns: u64) -> Result<Deadline> {
        Ok(Deadline::at(now()?.saturating_add(ns)))
    }

    /// The word `sys_wait` takes.
    pub const fn ns(self) -> u64 {
        self.0
    }
}

/// Nanoseconds since boot, on the clock nothing sets.
pub fn now() -> Result<u64> {
    read(CLOCK_MONOTONIC)
}

/// Nanoseconds since the Unix epoch: `Unsupported` on a machine whose clock could not be read
/// and has not been set. It can step backwards; measure an interval with [`now`].
pub fn wall() -> Result<u64> {
    read(CLOCK_REALTIME)
}

fn read(clock: u64) -> Result<u64> {
    let mut ns = 0;
    check(sys::clock_read(clock, &mut ns))?;
    Ok(ns)
}

/// Sleep for `ns`, to the kernel's tick.
pub fn sleep(ns: u64) -> Result<()> {
    sleep_until(Deadline::after(ns)?);
    Ok(())
}

impl Handle<Timer, Only> {
    /// A timer not yet set, which nothing wakes for.
    pub fn create() -> Result<Self> {
        let h = check(sys::timer_create())?;
        Ok(Handle::fresh(h, Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER))
    }

    /// Fire at monotonic time `at`, signalled from then until it is set again.
    pub fn set(&self, at: u64) -> Result<()> {
        check(sys::timer_set(self.raw().0, at.max(1), 0)).map(|_| ())
    }

    /// Not fire: a wait on it ends only by its deadline.
    pub fn disarm(&self) -> Result<()> {
        check(sys::timer_set(self.raw().0, 0, 0)).map(|_| ())
    }
}

/// Sleep until `deadline`, in a wait on no handles, which the kernel never refuses; at once for
/// [`Deadline::POLL`], and never past [`Deadline::NEVER`]'s last representable tick.
pub fn sleep_until(deadline: Deadline) {
    if deadline != Deadline::POLL {
        sys::wait(&[], &mut [], deadline.0.min(u64::MAX - 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_deadline_at_zero_is_no_poll() {
        assert_eq!(Deadline::at(0).ns(), 1);
        assert_eq!(Deadline::at(u64::MAX), Deadline::NEVER);
        assert_ne!(Deadline::at(0), Deadline::POLL);
    }

    #[test]
    fn a_sleep_waits_on_no_handles_until_its_deadline() {
        sys::reset();
        sleep(5_000_000).unwrap();
        assert_eq!((sys::sleeps(), sys::last_wait_count()), (1, 0));
        let waits = sys::counts().1;
        sleep_until(Deadline::POLL);
        assert_eq!(sys::counts().1, waits, "a poll sleeps for nothing, and makes no call");
        sleep_until(Deadline::NEVER);
        assert_eq!(sys::sleeps(), 2, "for ever is the last tick, which the kernel takes");
    }

    #[test]
    fn a_deadline_past_the_end_of_time_is_never() {
        sys::reset();
        assert_eq!(Deadline::after(u64::MAX).unwrap(), Deadline::NEVER);
    }

    #[test]
    fn a_timer_set_ends_a_wait_at_its_time_and_reads_signalled_until_set_again() {
        sys::reset();
        let timer = Handle::<Timer, Only>::create().unwrap();
        let mut set = crate::WaitSet::<1>::new();
        set.add(&timer).unwrap();
        assert!(set.wait(Deadline::POLL).unwrap().is_empty(), "unset, it is not signalled");
        let at = now().unwrap() + 50_000_000;
        timer.set(at).unwrap();
        assert!(set.wait(Deadline::after(10_000_000).unwrap()).unwrap().is_empty(), "not before its time");
        assert!(set.wait(Deadline::after(5_000_000_000).unwrap()).unwrap().contains(0));
        assert!(now().unwrap() >= at, "woken at its time, not before");
        assert!(set.wait(Deadline::POLL).unwrap().contains(0), "a wait that comes late ends at once");
        timer.disarm().unwrap();
        assert!(set.wait(Deadline::POLL).unwrap().is_empty());
        timer.set(0).unwrap();
        assert!(set.wait(Deadline::POLL).unwrap().contains(0), "a time at zero is past, not a disarm");
    }

    #[test]
    fn the_wall_clock_is_unsupported_until_set() {
        sys::reset();
        assert_eq!(wall().err().unwrap().code(), libkern::KError::Unsupported);
        sys::set_realtime(1_700_000_000_000_000_000);
        assert!(wall().unwrap() >= 1_700_000_000_000_000_000);
        assert!(now().unwrap() < now().unwrap(), "monotonic moves");
    }
}
