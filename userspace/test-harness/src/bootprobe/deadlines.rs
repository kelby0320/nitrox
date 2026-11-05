//! The deadline heap as a process sees it: one holding many armed timers starves no one's deadline.

use super::MS;
use super::refused::deadlines::wait_status;
use alloc::vec::Vec;
use libkern::KError;
use libos::log::{self, Line};
use libos::{Carried, Channel, Deadline, Handle, Msg, Only, Timer, WaitSet, time};

/// More than the 520 slots the heap holds for threads, and forty times the 16 it once held in all.
const TIMERS: usize = 640;

/// Whether a `BlockBounded` send to a channel whose ring is full, given 20 ms, completes `TimedOut`
/// within a second.
fn a_bounded_send_to_a_full_channel_times_out() -> bool {
    let Ok((a, _b)) = Handle::<Channel, Only>::pair(1) else { return false };
    let msg = Msg::new();
    if a.send(&msg, &mut Carried::new()).is_err() {
        return false;
    }
    let Ok(mut op) = Deadline::after(20 * MS).and_then(|by| a.send_by(&msg, &mut Carried::new(), by)) else {
        return false;
    };
    let signalled = {
        let mut set = WaitSet::<1>::new();
        if set.add_op(&op).is_err() {
            return false;
        }
        Deadline::after(1_000 * MS).and_then(|by| set.wait(by))
    };
    matches!(signalled.map(|s| op.completion_in(&s)), Ok(Some(Err(e))) if e.code() == KError::TimedOut)
}

/// **Holding many armed timers starves no one's deadline.** With [`TIMERS`] armed for an hour, every
/// one arms, a wait with a deadline times out, a poll answers `WouldBlock`, and a bounded send's
/// deadline still fires.
pub(super) fn many_timers_test() -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: many timers FAIL: ").s(why).end();
        false
    };
    let far = time::now().unwrap_or(0) + 3_600_000 * MS;
    let mut timers = Vec::with_capacity(TIMERS);
    while timers.len() < TIMERS {
        let Ok(t) = Handle::<Timer, Only>::create() else { break };
        if t.set(far).is_err() {
            break;
        }
        timers.push(t);
    }
    let armed = timers.len();
    let unarmed = Handle::<Timer, Only>::create();
    let (waited, polled) = match &unarmed {
        Ok(t) => (wait_status(t.as_ref(), time::now().unwrap_or(0) + 10 * MS), wait_status(t.as_ref(), 0)),
        Err(e) => (Err(i64::from(e.code().as_i32())), Err(i64::from(e.code().as_i32()))),
    };
    let sent = a_bounded_send_to_a_full_channel_times_out();
    drop(unarmed);
    drop(timers);
    let timed_out = KError::TimedOut.as_i32();
    if armed < TIMERS {
        Line::new().s(b"boot-probe: many timers FAIL: only ").u(armed as u64).s(b" of 640 armed").end();
        return false;
    }
    if waited != Err(i64::from(timed_out)) {
        return fail(b"a wait with a deadline did not time out");
    }
    if polled != Err(i64::from(KError::WouldBlock.as_i32())) {
        return fail(b"a poll that found nothing did not answer WouldBlock");
    }
    if !sent {
        return fail(b"a bounded send to a full channel did not time out");
    }
    log::write(b"boot-probe: 640 armed timers leave every other deadline its slot ok\n");
    true
}
