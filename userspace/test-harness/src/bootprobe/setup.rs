//! The setup message a stage starts with: what becomes of the handles it is given.

use super::*;
use libstream::setup::{Streams, pipe, send_setup_full};
use libstream::wire::{Record, WireError};

/// **A setup message that cannot be made closes the handles it was given**, and one sent moves
/// them: either way they are gone, so a caller never closes one. An argv past one message left them
/// with the caller, which closed only the terminal.
pub(super) fn a_refused_setup_message_closes_its_handles_test() -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: setup handles FAIL: ").s(why).end();
        false
    };
    let (Ok((ours, theirs)), Ok((rx, tx)), Ok((t, peer))) = (pipe(4), pipe(4), pipe(4)) else {
        return fail(b"no channel");
    };
    let long = "x".repeat(5000);
    let streams = Streams { stdin: None, stdout: Some(tx), stderr: None };
    let refused = send_setup_full(ours, streams, Some(t), &[&long], &Record::default());
    let gone = stat(tx).is_none() && stat(t).is_none();
    let streams = Streams { stdin: None, stdout: Some(rx), stderr: None };
    let sent = send_setup_full(ours, streams, None, &["stage"], &Record::default());
    let moved = stat(rx).is_none();
    [ours, theirs, peer].into_iter().for_each(close);
    // The kernel refusing the send, the stage's end gone, before it takes anything.
    let (Ok((dead, theirs)), Ok((x, x_peer))) = (pipe(4), pipe(4)) else {
        return fail(b"no channel");
    };
    close(theirs);
    let streams = Streams { stdin: None, stdout: Some(x), stderr: None };
    let peerless = send_setup_full(dead, streams, None, &["stage"], &Record::default());
    let closed = stat(x).is_none();
    [dead, x_peer].into_iter().for_each(close);
    if peerless.is_ok() || !closed {
        return fail(b"a send the kernel refused left its handle open");
    }
    if refused != Err(WireError::SetupTooLarge) {
        return fail(b"a 5,000-byte argv was not refused SetupTooLarge");
    }
    if !gone {
        return fail(b"a refused message left its handles open");
    }
    if sent.is_err() || !moved {
        return fail(b"a message that fits did not go, or left its handle");
    }
    Line::new().s(b"boot-probe: a refused setup message closes its handles ok").end();
    true
}
