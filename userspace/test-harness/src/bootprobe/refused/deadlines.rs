//! A wait's answer where libos folds or makes no call: a timer's `TimedOut` and `WouldBlock`, and a
//! wait on no handles.

use crate::MS;
use libkern::{KError, SYS_WAIT, syscall4};
use libos::log::{self, Line};
use libos::{Only, Ref, Timer, time};

/// Wait on `timer` until `deadline`: `Ok(status)` of its result, else `Err` with what the wait
/// answered, a `WouldBlock` or `TimedOut` libos reads as an empty wait.
pub(crate) fn wait_status(timer: Ref<'_, Timer, Only>, deadline: u64) -> Result<i32, i64> {
    let handles = [timer.raw().0];
    let mut r = [0u8; 24];
    // SAFETY: one handle this process holds, and room for one result.
    let n = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, r.as_mut_ptr() as u64, deadline) };
    if n == 1 { Ok(i32::from_le_bytes([r[8], r[9], r[10], r[11]])) } else { Err(n) }
}

/// **A wait on no handles sleeps until its deadline**, and a poll or a wait for ever on none is
/// refused. Raw throughout: libos makes no call for a poll or a wait for ever on nothing.
pub(crate) fn wait_on_nothing_test() -> bool {
    // SAFETY: no handles and no results, so the kernel reads and writes neither.
    let wait = |deadline: u64| unsafe { syscall4(SYS_WAIT, 0, 0, 0, deadline) };
    let invalid = i64::from(KError::InvalidArgument.as_i32());
    let (poll, forever) = (wait(0), wait(u64::MAX));
    let start = time::now().unwrap_or(0);
    let slept = wait(start + 20 * MS);
    let took = time::now().unwrap_or(0) - start;
    if poll != invalid || forever != invalid {
        Line::new()
            .s(b"boot-probe: a wait on nothing FAIL: a poll answered ")
            .i(poll)
            .s(b", for ever ")
            .i(forever)
            .end();
        return false;
    }
    if slept != i64::from(KError::TimedOut.as_i32()) || took < 20 * MS {
        Line::new()
            .s(b"boot-probe: a wait on nothing FAIL: 20 ms answered ")
            .i(slept)
            .s(b" in ")
            .u(took / MS)
            .s(b" ms")
            .end();
        return false;
    }
    log::write(b"boot-probe: a wait on no handles sleeps until its deadline ok\n");
    true
}
