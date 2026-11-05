//! The machine itself: stopping it, setting its clock, and a test run's verdict.

use libkern::{CLOCK_SET_THIS_BOOT, KError, POWER_HALT, POWER_REBOOT, TEST_EXIT_FAILURE, TEST_EXIT_SUCCESS};

use crate::error::{Error, Result, check};
use crate::handle::{CanWrite, Ref, Resource};
use crate::sys;

/// How to stop the machine.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Turn it off.
    Halt,
    /// Start it again.
    Reboot,
}

/// Stop the machine as `how` says, with the system-control object, which needs `WRITE`: this
/// returns only if the kernel refused, with why.
pub fn stop<'a, M: CanWrite + 'a>(control: impl Into<Ref<'a, Resource, M>>, how: Stop) -> Error {
    // TODO(surface-shape): one byte written to the power node, once the object is a char node.
    let op = match how {
        Stop::Halt => POWER_HALT,
        Stop::Reboot => POWER_REBOOT,
    };
    let r = sys::power(control.into().raw().0, op);
    Error::from_status(if r < 0 { r as i32 } else { KError::KernelError as i32 })
}

/// How far a set of the clock reached.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClockSet {
    /// The hardware clock holds it, so the next boot starts from it.
    Kept,
    /// This boot's clock alone: the hardware clock would not take it.
    ThisBoot,
}

/// Set the wall clock to `ns` since the Unix epoch, stepping it either way: refused `NoAccess`
/// without the `SYSTEM_CLOCK` grant, and `InvalidArgument` for a time outside 2000 to 2099.
pub fn set_clock(ns: u64) -> Result<ClockSet> {
    // TODO(surface-shape): a write to the clock node, once the clock is a char node.
    let r = check(sys::clock_set(ns))?;
    Ok(if r == CLOCK_SET_THIS_BOOT { ClockSet::ThisBoot } else { ClockSet::Kept })
}

/// Report a test run's verdict to the harness that booted the machine, which ends the run: this
/// returns where none listens.
// TODO(test-verdict-authority): any process can end a test run; a handle should be the authority.
pub fn test_verdict(pass: bool) {
    let code = if pass { TEST_EXIT_SUCCESS } else { TEST_EXIT_FAILURE };
    sys::test_exit(code as u64);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sys::mock;
    use crate::{Handle, WriteOnly};
    use libkern::{RawHandle, Rights};

    fn control(raw: u64) -> Handle<Resource, WriteOnly> {
        // SAFETY: a handle the model just made, owned here alone.
        unsafe { Handle::from_raw(RawHandle(raw), Rights::empty()) }
    }

    #[test]
    fn a_clock_set_says_how_far_it_reached() {
        mock::reset();
        let ns = 1_900_000_000_000_000_000;
        assert_eq!(set_clock(ns).map_err(|e| e.code()), Ok(ClockSet::Kept));
        assert!(crate::time::wall().unwrap() >= ns, "the wall clock reads it");
        mock::answer_next_clock_set(CLOCK_SET_THIS_BOOT as i64);
        assert_eq!(set_clock(ns).map_err(|e| e.code()), Ok(ClockSet::ThisBoot));
        mock::answer_next_clock_set(KError::NoAccess as i64);
        assert_eq!(set_clock(ns).map_err(|e| e.code()), Err(KError::NoAccess), "without the grant");
        assert_eq!(set_clock(0).map_err(|e| e.code()), Err(KError::InvalidArgument), "before 2000");
    }

    #[test]
    #[should_panic(expected = "power(1)")]
    fn a_reboot_asks_the_kernel_to_reset() {
        mock::reset();
        let _ = stop(&control(mock::system_control()), Stop::Reboot);
    }

    #[test]
    #[should_panic(expected = "power(0)")]
    fn a_halt_asks_the_kernel_to_turn_off() {
        mock::reset();
        let _ = stop(&control(mock::system_control()), Stop::Halt);
    }

    #[test]
    fn a_stop_without_the_object_or_its_right_is_refused_and_returns() {
        mock::reset();
        let unwritable = control(mock::with_rights(mock::system_control(), Rights::INSPECT));
        assert_eq!(stop(&unwritable, Stop::Halt).code(), KError::NoAccess);
        let device = control(mock::device());
        assert_ne!(stop(&device, Stop::Halt).code(), KError::KernelError, "refused for its type");
    }

    #[test]
    fn a_verdict_is_reported_and_returns_where_no_harness_listens() {
        mock::reset();
        test_verdict(false);
        test_verdict(true);
        assert_eq!(mock::verdicts(), [TEST_EXIT_FAILURE as u64, TEST_EXIT_SUCCESS as u64]);
    }
}
