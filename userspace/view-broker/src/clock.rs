//! The clock node, moved here by `service-mgr` and bound at `/dev/clock` for the `clock` grant.

use libkern::{KError, KOBJ_DEVICE_NODE, Rights};
use libos::system::CLOCK_PATH;
use libos::{Channel, Deadline, Handle, Msg, Namespace, NsMutable, Only, Resource, WaitSet, WriteOnly, log};

/// The clock node.
pub type Clock = Handle<Resource, WriteOnly>;

const RECEIVE_NS: u64 = 5_000_000_000;

/// **The clock node `service-mgr` moves here**, the first message on `control`, or `None`, said.
pub fn receive(control: Option<&Handle<Channel, Only>>) -> Option<Clock> {
    let mut set = WaitSet::<1>::new();
    let came = control.is_some_and(|c| {
        Deadline::after(RECEIVE_NS).is_ok_and(|d| set.add(c).and_then(|_| set.wait(d)).is_ok_and(|s| !s.is_empty()))
    });
    let taken: Option<Clock> = control.filter(|_| came).and_then(|c| c.recv(&mut Msg::new()).ok()?.take(0));
    let is_it = |h: &Clock| {
        h.stat().is_ok_and(|info| info.object_type == KOBJ_DEVICE_NODE && info.rights & Rights::WRITE.bits() != 0)
    };
    let clock = taken.filter(is_it);
    if clock.is_none() {
        log::write(b"view-broker: no clock node from service-mgr -- the clock grant is refused\n");
    }
    clock
}

/// **The clock node bound at `/dev/clock` in `view_ns`**: `false` if the view keeps its caller's,
/// a view made inside one the grant made.
pub fn bind(clock: Option<&Clock>, view_ns: &Handle<Namespace, NsMutable>) -> Result<bool, &'static str> {
    let clock = clock.ok_or("the clock is not here to grant")?;
    match view_ns.bind(CLOCK_PATH, clock) {
        Ok(()) => Ok(true),
        Err(e) if e.code() == KError::AlreadyExists => Ok(false),
        Err(_) => Err("the clock could not be granted"),
    }
}

#[cfg(test)]
mod tests;
