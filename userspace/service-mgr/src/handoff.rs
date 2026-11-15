//! Handles moved over a channel one to a message, read positionally.

use libos::{Carried, Channel, Deadline, Handle, Msg, Only, Resource, WaitSet, WriteOnly, log};

/// A channel end.
pub type Endpoint = Handle<Channel, Only>;
/// The clock node.
pub type Clock = Handle<Resource, WriteOnly>;

/// Receive one handle from `ctrl` within `wait_ns`, or `None`: an unbounded wait would hang the
/// supervisor on a message never coming.
pub fn recv_handoff<T, M>(ctrl: &Endpoint, wait_ns: u64) -> Option<Handle<T, M>> {
    let mut set = WaitSet::<1>::new();
    // A clock that cannot be read counts from boot, so the wait still ends.
    let deadline = Deadline::at(libos::time::now().unwrap_or(0).saturating_add(wait_ns));
    if !set.add(ctrl).and_then(|_| set.wait(deadline)).is_ok_and(|s| !s.is_empty()) {
        log::write(b"service-mgr: handoff timeout\n");
        return None;
    }
    // The one handle a handoff brings; any more close as `sent` drops.
    ctrl.recv(&mut Msg::new()).ok()?.take(0)
}

/// Move `handle` to a child over its control channel; one refused is closed, and as `None`
/// sends **an empty message**: the receiver reads the handoffs positionally.
pub fn send_handle<T, M>(ctrl: &Endpoint, handle: Option<Handle<T, M>>) {
    let mut carried = Carried::new();
    if let Some(handle) = handle
        && carried.push(handle).is_ok()
        && send(ctrl, &mut carried)
    {
        return;
    }
    drop(carried);
    send(ctrl, &mut Carried::new());
}

/// **What a service is handed after its spawn, in order**: its log endpoint, if it is not a
/// server; then the clock node if it declares `clock`, moved, so a later declarer gets none.
pub fn hand_on(
    ours: &Endpoint,
    server: bool,
    log_ep: Option<Endpoint>,
    declares_clock: bool,
    clock: &mut Option<Clock>,
) {
    if !server {
        send_handle(ours, log_ep);
    }
    if declares_clock {
        send_handle(ours, clock.take());
    }
}

fn send(ch: &Endpoint, carried: &mut Carried) -> bool {
    ch.send(&Msg::new(), carried).is_ok()
}

#[cfg(test)]
mod tests;
