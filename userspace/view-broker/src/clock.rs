//! The clock node: moved here by `service-mgr` at the start, and bound into each view the `clock`
//! grant makes, at `/dev/clock`, where a program sets the wall clock by writing to it.

use libos::system::CLOCK_PATH;
use libos::{Channel, Deadline, Handle, Msg, Namespace, NsMutable, Only, Resource, WaitSet, WriteOnly, log};

/// The clock node.
pub(super) type Clock = Handle<Resource, WriteOnly>;

/// How long the start waits for the clock: `service-mgr` sends it before this process runs.
const RECEIVE_NS: u64 = 5_000_000_000;

/// **The clock node `service-mgr` moves here** as the first message on `control`, or `None`, said,
/// if it sent none or an empty one: then the `clock` grant is refused.
pub(super) fn receive(control: Option<&Handle<Channel, Only>>) -> Option<Clock> {
    let mut set = WaitSet::<1>::new();
    let came = control.is_some_and(|c| {
        Deadline::after(RECEIVE_NS).is_ok_and(|d| set.add(c).and_then(|_| set.wait(d)).is_ok_and(|s| !s.is_empty()))
    });
    let clock = control.filter(|_| came).and_then(|c| c.recv(&mut Msg::new()).ok()?.take(0));
    if clock.is_none() {
        log::write(b"view-broker: no clock node from service-mgr -- the clock grant is refused\n");
    }
    clock
}

/// **The clock node bound at `/dev/clock` in `view_ns`**, a direct handle a program there writes
/// the time to; refused when this process holds none.
pub(super) fn bind(clock: Option<&Clock>, view_ns: &Handle<Namespace, NsMutable>) -> Result<(), &'static str> {
    let clock = clock.ok_or("the clock is not here to grant")?;
    view_ns.bind(CLOCK_PATH, clock).map_err(|_| "the clock could not be granted")
}
