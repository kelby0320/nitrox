//! The deadline heap as a process sees it: one holding many armed timers starves no one's deadline.

use super::*;
use libkern::{
    KError, SENDMODE_BLOCKBOUNDED, SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_SEND, SYS_TIMER_CREATE,
    SYS_TIMER_SET,
};

/// More than the 520 slots the heap holds for threads, and forty times the 16 it once held in all.
const TIMERS: usize = 640;

/// Wait on `h` until `deadline`: `Ok(status)` of its result, else `Err` with what the wait answered.
fn wait_status(h: u64, deadline: u64) -> Result<i32, i64> {
    let handles = [h];
    let mut r = [0u8; 24];
    // SAFETY: one handle this process holds, and room for one result.
    let n = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, r.as_mut_ptr() as u64, deadline) };
    if n == 1 { Ok(i32::from_le_bytes([r[8], r[9], r[10], r[11]])) } else { Err(n) }
}

/// What a `BlockBounded` send to a channel whose ring is full comes to, given 20 ms.
fn bounded_send_to_a_full_channel() -> Result<i32, i64> {
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    if unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, 1, 0) } != 0 {
        return Err(-1);
    }
    let msg = [0u8; 4096];
    let send = |mode: u64, deadline: u64| {
        // SAFETY: a valid message buffer and no handles.
        unsafe { syscall6(SYS_CHANNEL_SEND, a, msg.as_ptr() as u64, 0, 0, mode, deadline) }
    };
    let filled = send(SENDMODE_NOBLOCK, 0) == 0;
    let po = send(SENDMODE_BLOCKBOUNDED, clock_ns() + 20 * MS);
    let outcome = if !filled || po <= 0 { Err(po) } else { wait_status(po as u64, clock_ns() + 1_000 * MS) };
    close(po.max(0) as u64);
    close(a);
    close(b);
    outcome
}

/// **Holding many armed timers starves no one's deadline.** With [`TIMERS`] armed for an hour, every
/// one arms, a wait with a deadline times out, and a bounded send's deadline still fires.
pub(super) fn many_timers_test() -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: many timers FAIL: ").s(why).end();
        false
    };
    let far = clock_ns() + 3_600_000 * MS;
    let mut timers = [0u64; TIMERS];
    let mut armed = 0;
    for t in timers.iter_mut() {
        // SAFETY: register-only syscalls on a timer this process creates.
        let h = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
        if h <= 0 {
            break;
        }
        *t = h as u64;
        // SAFETY: as above.
        if unsafe { syscall4(SYS_TIMER_SET, *t, far, 0, 0) } != 0 {
            break;
        }
        armed += 1;
    }
    // SAFETY: as above.
    let unarmed = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    let waited = if unarmed > 0 { wait_status(unarmed as u64, clock_ns() + 10 * MS) } else { Err(unarmed) };
    let sent = bounded_send_to_a_full_channel();
    close(unarmed.max(0) as u64);
    timers.iter().for_each(|&t| close(t));
    let timed_out = KError::TimedOut.as_i32();
    if armed < TIMERS {
        Line::new().s(b"boot-probe: many timers FAIL: only ").u(armed as u64).s(b" of 640 armed").end();
        return false;
    }
    if waited != Err(i64::from(timed_out)) {
        return fail(b"a wait with a deadline did not time out");
    }
    if sent != Ok(timed_out) {
        return fail(b"a bounded send to a full channel did not time out");
    }
    kprint(b"boot-probe: 640 armed timers leave every other deadline its slot ok\n");
    true
}
