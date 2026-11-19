//! The transport and the setup message, through libos's model of the kernel.

use crate::channel::{ChannelReceiver, ChannelSink, IpcPort, IpcPortRef, MsgPort};
use crate::setup::{STREAM_STDIN, StageStreams, answer_help, open_pipe, receive_setup, say_help, send_stage_setup};
use crate::wire::{Record, WireError};
use alloc::vec::Vec;
use libkern::KError;
use libos::{Carried, Channel, Deadline, Handle, Msg, Only, mock};

fn pair() -> (Handle<Channel, Only>, Handle<Channel, Only>) {
    Handle::pair(4).unwrap()
}

fn end() -> Handle<Channel, Only> {
    pair().0
}

#[test]
fn a_frame_sent_arrives_with_its_marker() {
    mock::reset();
    let (a, b) = pair();
    let mut port = IpcPort::owning(a);
    port.send(b"one", false).unwrap();
    port.send(b"two", true).unwrap();
    let mut msg = Msg::new();
    b.recv(&mut msg).unwrap();
    assert_eq!((msg.payload(), msg.flags()), (&b"one"[..], 0));
    b.recv(&mut msg).unwrap();
    assert_eq!((msg.payload(), msg.flags()), (&b"two"[..], 1));
    assert_eq!(mock::stray_closes(), 0);
}

#[test]
fn a_stream_round_trips_over_two_ports() {
    mock::reset();
    let (a, b) = pair();
    let mut sink = ChannelSink::new(IpcPort::owning(a), 5);
    crate::wire::ByteSink::put(&mut sink, b"twelve bytes").unwrap();
    sink.finish().unwrap();
    let got = ChannelReceiver::new(IpcPort::owning(b)).receive(usize::MAX).unwrap();
    assert_eq!(got, b"twelve bytes");
}

#[test]
fn a_port_closes_the_endpoint_it_owns() {
    mock::reset();
    let (a, _b) = pair();
    let raw = a.raw().0;
    drop(IpcPort::owning(a));
    assert!(!mock::is_open(raw));
    assert_eq!(mock::stray_closes(), 0);
}

#[test]
fn a_lent_port_frames_as_an_owning_one_and_closes_nothing() {
    mock::reset();
    let (a, b) = pair();
    let mut sink = ChannelSink::new(IpcPortRef::new(&a), 5);
    crate::wire::ByteSink::put(&mut sink, b"twelve bytes").unwrap();
    sink.finish().unwrap();
    drop(sink);
    assert_eq!(ChannelReceiver::new(IpcPortRef::new(&b)).receive(usize::MAX).unwrap(), b"twelve bytes");
    assert!(mock::is_open(a.raw().0) && mock::is_open(b.raw().0), "both ends still their owners'");
    IpcPortRef::new(&a).send(b"more", false).unwrap();
    drop(b);
    assert_eq!(IpcPortRef::new(&a).send(b"gone", false), Err(WireError::PeerClosed));
}

#[test]
fn a_receive_on_a_closed_peer_is_peer_closed_after_what_was_sent() {
    mock::reset();
    let (a, b) = pair();
    IpcPort::owning(a).send(b"partial", false).unwrap();
    assert_eq!(ChannelReceiver::new(IpcPort::owning(b)).receive(usize::MAX), Err(WireError::PeerClosed));
}

#[test]
fn a_send_to_a_closed_peer_is_peer_closed() {
    mock::reset();
    let (a, b) = pair();
    drop(b);
    assert_eq!(IpcPort::owning(a).send(b"x", false), Err(WireError::PeerClosed));
}

#[test]
fn a_frame_past_a_message_is_refused_unsent() {
    mock::reset();
    let (a, b) = pair();
    let mut port = IpcPort::owning(a);
    let long = alloc::vec![0u8; libkern::IPC_PAYLOAD_SIZE + 1];
    assert_eq!(port.send(&long, false), Err(WireError::SinkFull));
    assert_eq!(b.recv(&mut Msg::new()).err().unwrap().code(), KError::WouldBlock);
}

#[test]
fn a_receive_waits_to_its_deadline_and_a_frame_brings_no_handle_home() {
    mock::reset();
    let (a, b) = pair();
    let mut port = IpcPort::owning(b);
    let mut out = Vec::new();
    assert_eq!(port.recv_by(&mut out, Deadline::after(1_000_000).unwrap()), Ok(None));
    let carried = end();
    let raw = carried.raw().0;
    let mut with = Carried::new();
    with.push(carried).ok().unwrap();
    let mut msg = Msg::new();
    msg.set(b"f").unwrap();
    msg.set_flags(1);
    a.send(&msg, &mut with).unwrap();
    assert_eq!(port.recv_by(&mut out, Deadline::NEVER), Ok(Some(true)));
    assert_eq!(out, b"f");
    assert!(!mock::is_open(raw), "what a frame carried is closed, not leaked into the table");
}

#[test]
fn a_send_by_gives_up_when_the_ring_stays_full() {
    mock::reset();
    let (a, _b) = pair();
    let mut port = IpcPort::owning(a);
    for _ in 0..4 {
        port.send_by(b"x", false, Deadline::POLL).unwrap();
    }
    assert_eq!(port.send_by(b"x", false, Deadline::after(1_000_000).unwrap()), Err(WireError::SinkFull));
}

#[test]
fn a_setup_message_moves_its_handles_and_the_stage_receives_them() {
    mock::reset();
    let (shell, stage) = pair();
    let (stdout, stdout_peer) = pair();
    let (stderr, _stderr_peer) = pair();
    let terminal = end();
    let (out_raw, err_raw, term_raw) = (stdout.raw().0, stderr.raw().0, terminal.raw().0);
    let streams = StageStreams { stdout: Some(stdout), stderr: Some(stderr), shown: true, ..Default::default() };
    send_stage_setup(&shell, streams, Some(terminal), &["nxsh", "-c"], &Record::default(), Deadline::NEVER).unwrap();
    assert!(![out_raw, err_raw, term_raw].iter().any(|&h| mock::is_open(h)), "moved, not kept");

    let got = receive_setup(&stage, Deadline::POLL).unwrap().unwrap();
    assert_eq!(got.argv, ["nxsh", "-c"]);
    assert!(got.streams.stdin.is_none() && got.terminal.is_some() && got.streams.shown);
    let stdout = got.streams.stdout.unwrap();
    let mut m = Msg::new();
    stdout.send(&m, &mut Carried::new()).unwrap();
    m.set(b"").unwrap();
    assert!(stdout_peer.recv(&mut m).is_ok(), "the stage's stdout reaches the shell's pipe");
    drop((got.streams.stderr, got.terminal, stdout));
    assert_eq!(mock::stray_closes(), 0);
}

#[test]
fn a_refused_setup_send_closes_every_handle_it_was_given() {
    mock::reset();
    let (shell, stage) = pair();
    drop(stage);
    let (stdin, _p) = pair();
    let terminal = end();
    let (i, t) = (stdin.raw().0, terminal.raw().0);
    let streams = StageStreams { stdin: Some(stdin), ..Default::default() };
    let r = send_stage_setup(&shell, streams, Some(terminal), &["x"], &Record::default(), Deadline::NEVER);
    assert_eq!(r, Err(WireError::PeerClosed));
    assert!(!mock::is_open(i) && !mock::is_open(t), "gone on every return");
    assert_eq!(mock::stray_closes(), 0, "each closed once");
}

#[test]
fn a_setup_too_large_to_send_closes_its_handles_and_says_so() {
    mock::reset();
    let (shell, _stage) = pair();
    let terminal = end();
    let t = terminal.raw().0;
    let big = alloc::format!("{:>5000}", "x");
    let r =
        send_stage_setup(&shell, StageStreams::default(), Some(terminal), &[&big], &Record::default(), Deadline::NEVER);
    assert_eq!(r, Err(WireError::SetupTooLarge));
    assert!(!mock::is_open(t));
}

/// A setup message made by hand, `extra` handles beyond the ones its payload names.
fn handmade(streams: u32, terminal: bool, handles: usize) -> (Handle<Channel, Only>, Vec<u64>) {
    let (shell, stage) = pair();
    let payload = crate::setup::SetupPayload { streams, terminal, ..Default::default() };
    let mut msg = Msg::new();
    msg.set(&payload.encode().unwrap()).unwrap();
    let mut carried = Carried::new();
    let mut raw = Vec::new();
    for _ in 0..handles {
        let h = end();
        raw.push(h.raw().0);
        carried.push(h).ok().unwrap();
    }
    shell.send(&msg, &mut carried).unwrap();
    (stage, raw)
}

#[test]
fn handles_beyond_the_ones_a_setup_names_are_closed() {
    mock::reset();
    let (stage, _) = handmade(STREAM_INDEX, false, 3);
    let before = mock::open_handles();
    let got = receive_setup(&stage, Deadline::POLL).unwrap().unwrap();
    assert!(got.streams.stdin.is_some() && got.terminal.is_none());
    assert_eq!(mock::open_handles(), before + 1, "the stream it named is the stage's, the other two closed");
    drop(got);
    assert_eq!(mock::open_handles(), before);
    assert_eq!(mock::stray_closes(), 0);
}

const STREAM_INDEX: u32 = STREAM_STDIN;

#[test]
fn a_setup_naming_more_handles_than_it_brought_is_refused_and_closes_what_came() {
    mock::reset();
    let (stage, _sent) = handmade(0, true, 0);
    assert_eq!(receive_setup(&stage, Deadline::POLL).err(), Some(WireError::SchemaMismatch));
    let (stage, sent) = handmade(STREAM_INDEX, true, 1);
    assert_eq!(receive_setup(&stage, Deadline::POLL).err(), Some(WireError::SchemaMismatch));
    assert!(!mock::is_open(sent[0]), "the one that came is closed with the refusal");
    let (stage, _) = handmade(1 << 5, false, 0);
    assert_eq!(receive_setup(&stage, Deadline::POLL).err(), Some(WireError::SchemaMismatch), "a stream bit unknown");
}

#[test]
fn a_setup_waited_for_to_a_deadline_is_none_and_a_closed_peer_is_peer_closed() {
    mock::reset();
    let (shell, stage) = pair();
    assert!(receive_setup(&stage, Deadline::after(1_000_000).unwrap()).unwrap().is_none());
    drop(shell);
    assert_eq!(receive_setup(&stage, Deadline::NEVER).err(), Some(WireError::PeerClosed));
}

#[test]
fn an_open_pipe_is_a_connected_pair_closed_with_its_ends() {
    mock::reset();
    let (a, b) = open_pipe(2).unwrap();
    let mut m = Msg::new();
    m.set(b"hi").unwrap();
    a.send(&m, &mut Carried::new()).unwrap();
    assert_eq!(b.recv(&mut Msg::new()).map(|_| ()), Ok(()));
    let (ra, rb) = (a.raw().0, b.raw().0);
    drop((a, b));
    assert!(!mock::is_open(ra) && !mock::is_open(rb));
}

const HELP: &[&str] = &["usage: app [PATH]", "", "  --help  show this help and exit"];

/// The rows a shell reads from `far`, the other end of a stage's `stdout`.
fn rows_from(far: Handle<Channel, Only>) -> Vec<alloc::string::String> {
    let bytes = ChannelReceiver::new(IpcPort::owning(far)).receive(usize::MAX).unwrap();
    let table = crate::wire::Table::decode(&bytes).unwrap();
    assert!(table.flags.contains(crate::wire::StreamFlags::TEXT_FALLBACK), "text, a line a row");
    table.rows.iter().map(|r| alloc::string::String::from(r[0].as_str().unwrap())).collect()
}

/// The one message on `far`, the other end of `stderr`, as its level and text.
fn diag_from(far: &Handle<Channel, Only>) -> (crate::diag::Level, Vec<u8>) {
    let mut m = Msg::new();
    far.recv(&mut m).expect("a diagnostic");
    let (level, text) = crate::diag::parse(m.payload());
    (level, text.to_vec())
}

#[test]
fn help_goes_to_stdout_as_text_and_not_to_stderr() {
    mock::reset();
    let ((out, out_far), (err, err_far)) = (pair(), pair());
    say_help(StageStreams { stdout: Some(out), stderr: Some(err), ..Default::default() }, HELP);
    assert_eq!(rows_from(out_far), HELP);
    // Closed with nothing queued: no message came before the close.
    assert_eq!(err_far.recv(&mut Msg::new()).err().unwrap().code(), KError::PeerClosed);
}

#[test]
fn help_with_no_stdout_is_a_notice() {
    mock::reset();
    let (err, err_far) = pair();
    say_help(StageStreams { stderr: Some(err), ..Default::default() }, HELP);
    assert_eq!(diag_from(&err_far), (crate::diag::Level::Notice, format!("{}\n", HELP.join("\n")).into_bytes()));
}

#[test]
fn help_whose_stdout_has_gone_is_a_notice() {
    mock::reset();
    let ((out, out_far), (err, err_far)) = (pair(), pair());
    drop(out_far);
    say_help(StageStreams { stdout: Some(out), stderr: Some(err), ..Default::default() }, HELP);
    assert_eq!(diag_from(&err_far).0, crate::diag::Level::Notice);
}

#[test]
fn help_is_answered_only_when_asked_and_then_ends_the_program() {
    mock::reset();
    let argv = |a: &[&str]| a.iter().map(|s| alloc::string::String::from(*s)).collect::<Vec<_>>();
    answer_help(&argv(&["app", "--", "--help"]), || panic!("taken unasked"), HELP);
    answer_help(&argv(&["app", "--helpful"]), || panic!("taken unasked"), HELP);

    let (out, out_far) = pair();
    let asked = argv(&["app", "x", "--help"]);
    let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        answer_help(&asked, || Some(StageStreams { stdout: Some(out), ..Default::default() }), HELP)
    }));
    let why = ended.expect_err("it ends the program");
    assert_eq!(why.downcast_ref::<alloc::string::String>().map(|s| s.as_str()), Some("exit(0)"));
    assert_eq!(rows_from(out_far), HELP);
}
