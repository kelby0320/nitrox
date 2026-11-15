//! Handles moved over a channel one to a message, read positionally.

use super::{Endpoint, READY_TIMEOUT_NS, now_ns, send_msg};
use libos::{Carried, Deadline, Handle, Msg, WaitSet, log};

/// Receive one handle from init's handoff channel; `None` if the message was empty or the receive
/// failed. **Bounded**: an unbounded wait would hang the supervisor on a message never coming.
pub(super) fn recv_handoff<T, M>(ctrl: &Endpoint) -> Option<Handle<T, M>> {
    let mut set = WaitSet::<1>::new();
    let deadline = Deadline::at(now_ns().saturating_add(READY_TIMEOUT_NS));
    if !set.add(ctrl).and_then(|_| set.wait(deadline)).is_ok_and(|s| !s.is_empty()) {
        log::write(b"service-mgr: handoff timeout\n");
        return None;
    }
    // The one handle a handoff brings; any more close as `sent` drops.
    ctrl.recv(&mut Msg::new()).ok()?.take(0)
}

/// Move `handle` to a child over its control channel; one refused is closed, and as `None`
/// sends **an empty message**: the receiver reads the handoffs positionally.
pub(super) fn send_handle<T, M>(ctrl: &Endpoint, handle: Option<Handle<T, M>>) {
    let mut carried = Carried::new();
    if let Some(handle) = handle
        && carried.push(handle).is_ok()
        && send_msg(ctrl, &[], &mut carried)
    {
        return;
    }
    drop(carried);
    send_msg(ctrl, &[], &mut Carried::new());
}
