//! The machine itself: stopping it, setting its clock, and a test run's verdict.

use libkern::{
    CLOCK_SET_KEPT, CLOCK_SET_THIS_BOOT, IO_OPCODE_WRITE, IoOp, KError, POWER_HALT, POWER_REBOOT, TEST_EXIT_FAILURE,
    TEST_EXIT_SUCCESS,
};

use crate::error::{Error, Result};
use crate::exec::{Op, block_on};
use crate::handle::{CanWrite, Handle, MapReadWrite, Memory, Ref, Resource};
use crate::sys;

/// How to stop the machine.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Turn it off.
    Halt,
    /// Start it again.
    Reboot,
}

/// **The power node, and a page of the two stops' bytes**, made early so a stop needs no memory.
pub struct Power<M> {
    node: Handle<Resource, M>,
    stops: Option<Handle<Memory, MapReadWrite>>,
}

impl<M: CanWrite> Power<M> {
    /// Take `node`, the power node, and make the page now, or at the stop if memory is short.
    pub fn new(node: Handle<Resource, M>) -> Power<M> {
        Power { node, stops: stops_page().ok() }
    }

    /// Whether the page a stop writes from is made, so the stop needs no memory.
    pub fn ready(&self) -> bool {
        self.stops.is_some()
    }

    /// Stop the machine as `how` says: this returns only with why it could not.
    pub fn stop(&self, how: Stop) -> Error {
        let mut made = None;
        let stops = match &self.stops {
            Some(page) => page,
            None => match stops_page() {
                Ok(page) => &*made.insert(page),
                Err(e) => return e,
            },
        };
        let at = match how {
            Stop::Halt => 0,
            Stop::Reboot => 1,
        };
        let op =
            IoOp { opcode: IO_OPCODE_WRITE, flags: 0, buffer: stops.raw().0, buf_offset: at, offset: 0, length: 1 };
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

fn stops_page() -> Result<Handle<Memory, MapReadWrite>> {
    let stops = Handle::<Memory, MapReadWrite>::create(1)?;
    stops.map(2)?.write_from(0, &[POWER_HALT as u8, POWER_REBOOT as u8])?;
    Ok(stops)
}

/// How far a set of the clock reached.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ClockSet {
    /// The hardware clock holds it, so the next boot starts from it.
    Kept,
    /// This boot's clock alone: the hardware clock would not take it.
    ThisBoot,
}

/// Where a view the `clock` grant made binds the clock node.
pub const CLOCK_PATH: &str = "/dev/clock";

/// Set the wall clock to `ns` since the Unix epoch by a write to `clock`, the clock node: refused
/// `NoAccess` without `WRITE`, and `InvalidArgument` outside 2000 to 2099.
pub fn set_clock<'a, M: CanWrite + 'a>(clock: impl Into<Ref<'a, Resource, M>>, ns: u64) -> Result<ClockSet> {
    let page = Handle::<Memory, MapReadWrite>::create(8)?;
    page.map(8)?.write_from(0, &ns.to_le_bytes())?;
    match block_on(clock.into().write(&page, 0, 0, 8))? as u64 {
        CLOCK_SET_KEPT => Ok(ClockSet::Kept),
        CLOCK_SET_THIS_BOOT => Ok(ClockSet::ThisBoot),
        // A write some other node took: not a set.
        _ => Err(Error::from_status(KError::InvalidArgument as i32)),
    }
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
        let clock = control(mock::clock_node());
        let ns = 1_900_000_000_000_000_000;
        assert_eq!(set_clock(&clock, ns).map_err(|e| e.code()), Ok(ClockSet::Kept));
        assert!(crate::time::wall().unwrap() >= ns, "the wall clock reads it");
        mock::answer_next_clock_set(CLOCK_SET_THIS_BOOT as i64);
        assert_eq!(set_clock(&clock, ns).map_err(|e| e.code()), Ok(ClockSet::ThisBoot));
        assert_eq!(set_clock(&clock, 0).map_err(|e| e.code()), Err(KError::InvalidArgument), "before 2000");
        let unwritable = control(mock::with_rights(mock::clock_node(), Rights::INSPECT));
        assert_eq!(set_clock(&unwritable, ns).map_err(|e| e.code()), Err(KError::NoAccess), "without WRITE");
        let device = control(mock::device());
        assert_eq!(set_clock(&device, ns).map_err(|e| e.code()), Err(KError::InvalidArgument), "a write, no set");
    }

    #[test]
    fn the_clock_node_refuses_a_read_and_takes_an_empty_write_as_every_node_does() {
        mock::reset();
        let clock = mock::clock_node();
        let page = Handle::<Memory, MapReadWrite>::create(8).unwrap();
        let op = |opcode, length| IoOp { opcode, flags: 0, buffer: page.raw().0, buf_offset: 0, offset: 0, length };
        assert_eq!(sys::io_submit(clock, &op(libkern::IO_OPCODE_READ, 8)), KError::NoAccess as i64);
        let empty = sys::io_submit(clock, &op(IO_OPCODE_WRITE, 0));
        assert!(empty > 0, "an operation");
        let done = crate::exec::block_on(Op::status(empty as u64)).unwrap();
        assert_eq!(done.result, 0, "set nothing, and said nothing of the clock");
    }

    fn power(raw: u64) -> Power<WriteOnly> {
        Power::new(control(raw))
    }

    #[test]
    #[should_panic(expected = "power(0)")]
    fn a_node_whose_page_could_not_be_made_is_kept_and_still_stops() {
        mock::reset();
        let node = mock::power_node();
        mock::fail_next_create(KError::OutOfMemory as i32);
        let power = power(node);
        assert!(!power.ready() && mock::is_open(node), "the node kept, and no page");
        let _ = power.stop(Stop::Halt);
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
