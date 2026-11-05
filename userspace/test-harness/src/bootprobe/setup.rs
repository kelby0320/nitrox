//! The setup message a stage starts with: what becomes of the handles it is given.

use super::*;
use libstream::setup::{StageStreams, open_pipe, send_stage_setup};
use libstream::wire::{Record, WireError};

/// **A setup message that cannot be made closes the handles it was given**, and one sent moves
/// them: either way they are gone, so a caller never closes one. An argv past one message left them
/// with the caller, which closed only the terminal.
pub(super) fn a_refused_setup_message_closes_its_handles_test() -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: setup handles FAIL: ").s(why).end();
        false
    };
    let (Ok((ours, theirs)), Ok((rx, tx)), Ok((t, peer))) = (open_pipe(4), open_pipe(4), open_pipe(4)) else {
        return fail(b"no channel");
    };
    let long = "x".repeat(5000);
    let streams = StageStreams { stdin: None, stdout: Some(tx), stderr: None };
    let refused = send_stage_setup(&ours, streams, Some(t), &[&long], &Record::default(), Deadline::NEVER);
    // The handles gone are the ones whose peers see them close.
    let gone = peer_gone_by(&rx, clock_ns() + 2_000 * MS) && peer_gone_by(&peer, clock_ns() + 2_000 * MS);
    drop(rx);
    // Moved, not copied: the peer of the end sent sees it close once the stage's goes.
    let Ok((y, y_peer)) = open_pipe(4) else {
        return fail(b"no channel");
    };
    let streams = StageStreams { stdin: None, stdout: Some(y), stderr: None };
    let sent = send_stage_setup(&ours, streams, None, &["stage"], &Record::default(), Deadline::NEVER);
    let received = theirs.recv(&mut Msg::new()).ok();
    let one = received.as_ref().is_some_and(|carried| carried.len() == 1);
    drop(received);
    let moved = one && peer_gone_by(&y_peer, clock_ns() + 2_000 * MS);
    // The kernel refusing the send, the stage's end gone, before it takes anything.
    let (Ok((dead, theirs)), Ok((x, x_peer))) = (open_pipe(4), open_pipe(4)) else {
        return fail(b"no channel");
    };
    drop(theirs);
    let streams = StageStreams { stdin: None, stdout: Some(x), stderr: None };
    let peerless = send_stage_setup(&dead, streams, None, &["stage"], &Record::default(), Deadline::NEVER);
    let closed = peer_gone_by(&x_peer, clock_ns() + 2_000 * MS);
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
