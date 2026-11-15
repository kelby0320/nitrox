//! The machine itself: stopping it, setting its clock, and a test run's verdict.

use libkern::{
    CLOCK_SET_THIS_BOOT, IO_OPCODE_WRITE, IoOp, KError, POWER_HALT, POWER_REBOOT, TEST_EXIT_FAILURE, TEST_EXIT_SUCCESS,
};

use crate::error::{Error, Result, check};
use crate::exec::Op;
use crate::handle::{CanWrite, Handle, MapReadWrite, Memory, Resource};
use crate::sys;

/// How to stop the machine.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Turn it off.
    Halt,
    /// Start it again.
    Reboot,
}

/// **The power node, and a page of the two stops' bytes**, made when it is taken so that a stop
/// needs no memory then: the machine stopping for want of a page is the worse outcome.
pub struct Power<M> {
    node: Handle<Resource, M>,
    stops: Handle<Memory, MapReadWrite>,
}

impl<M: CanWrite> Power<M> {
    /// Take `node`, the power node, and make the page a stop writes from.
    pub fn new(node: Handle<Resource, M>) -> Result<Power<M>> {
        let stops = Handle::<Memory, MapReadWrite>::create(1)?;
        stops.map(2)?.write_from(0, &[POWER_HALT as u8, POWER_REBOOT as u8])?;
        Ok(Power { node, stops })
    }

    /// Stop the machine as `how` says, a byte written to the node, which needs `WRITE`: this
    /// returns only if the kernel refused, with why.
    pub fn stop(&self, how: Stop) -> Error {
        let at = match how {
            Stop::Halt => 0,
            Stop::Reboot => 1,
        };
        let op = IoOp {
            opcode: IO_OPCODE_WRITE,
            flags: 0,
            buffer: self.stops.raw().0,
            buf_offset: at,
            offset: 0,
            length: 1,
        };
        match Op::submit(self.node.raw(), &op) {
            Err(refused) => refused,
            // A write some other node took: not a stop.
            Ok(taken) => {
                drop(taken);
                Error::from_status(KError::InvalidArgument as i32)
            }
        }
    }
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

    fn power(raw: u64) -> Power<WriteOnly> {
        Power::new(control(raw)).unwrap()
    }

    #[test]
    #[should_panic(expected = "power(1)")]
    fn a_reboot_writes_the_reboot_byte() {
        mock::reset();
        let _ = power(mock::power_node()).stop(Stop::Reboot);
    }

    #[test]
    #[should_panic(expected = "power(0)")]
    fn a_halt_writes_the_halt_byte() {
        mock::reset();
        let _ = power(mock::power_node()).stop(Stop::Halt);
    }

    #[test]
    fn a_stop_without_the_node_or_its_right_is_refused_and_returns() {
        mock::reset();
        let unwritable = power(mock::with_rights(mock::power_node(), Rights::INSPECT));
        assert_eq!(unwritable.stop(Stop::Halt).code(), KError::NoAccess);
        let device = power(mock::device());
        assert_eq!(device.stop(Stop::Halt).code(), KError::InvalidArgument, "a write it took, and no stop");
    }

    /// The model refuses as the kernel does, before any operation: a byte naming no stop, or more
    /// than one.
    #[test]
    fn a_write_to_the_power_node_of_another_byte_or_length_is_refused() {
        mock::reset();
        let node = mock::power_node();
        let page = Handle::<Memory, MapReadWrite>::create(1).unwrap();
        page.map(2).unwrap().write_from(0, &[7, 0]).unwrap();
        let write = |at, length| {
            let op =
                IoOp { opcode: IO_OPCODE_WRITE, flags: 0, buffer: page.raw().0, buf_offset: at, offset: 0, length };
            sys::io_submit(node, &op)
        };
        assert_eq!(write(0, 1), KError::InvalidArgument as i64, "a byte naming no stop");
        assert_eq!(write(1, 2), KError::InvalidArgument as i64, "two bytes");
    }

    #[test]
    fn a_verdict_is_reported_and_returns_where_no_harness_listens() {
        mock::reset();
        test_verdict(false);
        test_verdict(true);
        assert_eq!(mock::verdicts(), [TEST_EXIT_FAILURE as u64, TEST_EXIT_SUCCESS as u64]);
    }
}
