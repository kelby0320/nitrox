use super::ipc::*;
use super::*;
use alloc::vec;
use core::mem::MaybeUninit;
use libkern::{IPC_HEADER_SIZE, IPC_PAYLOAD_SIZE, RawHandle, Rights};
use libos::{Carried, Handle, Msg as WireMsg, mock};
use librsproto::{OP_TTY_INTERRUPT, RS_FLAG_ERROR, RS_MAX_BODY};

type End = Handle<Channel, Only>;

fn pair() -> (End, End) {
    mock::reset();
    End::pair(16).unwrap()
}

fn put(to: &End, op: u16, id: u64, flags: u32, body: &[u8]) {
    let mut m = WireMsg::new();
    let n = librsproto::encode(m.payload_mut(), op, id, flags, body, 0).unwrap();
    m.set_len(n).unwrap();
    to.send(&m, &mut Carried::new()).unwrap();
}

fn ack(to: &End) {
    put(to, 0, 1, 0, b"");
}

fn got(from: &End) -> (u16, u64, Vec<u8>) {
    let (op, id, _, body) = receive(from).unwrap().expect("a message was queued");
    (op, id, body)
}

/// Every payload byte of a message whose guard has run, read where it lay.
fn payload_after_drop(m: &mut MaybeUninit<Scrubbed>) -> Vec<u8> {
    // SAFETY: initialised by the caller and dropped once, in place; the bytes are plain data.
    unsafe {
        core::ptr::drop_in_place(m.as_mut_ptr());
        let all = &*(m.as_ptr() as *const [u8; core::mem::size_of::<WireMsg>()]);
        all[IPC_HEADER_SIZE..IPC_HEADER_SIZE + IPC_PAYLOAD_SIZE].to_vec()
    }
}

#[test]
fn a_request_arrives_and_a_password_in_the_buffer_does_not_outlive_the_send() {
    let (a, b) = pair();
    let mut buf = MaybeUninit::new(Scrubbed(WireMsg::new()));
    // SAFETY: initialised on the line above, and dropped only in `payload_after_drop`.
    let guard = unsafe { &mut *buf.as_mut_ptr() };
    assert!(send_from(a.as_ref(), guard, 0x0301, 7, b"hunter2", &mut Carried::new()));
    assert!(guard.0.payload().windows(7).any(|w| w == b"hunter2"), "it crossed this buffer");
    assert_eq!(got(&b), (0x0301, 7, b"hunter2".to_vec()), "and arrived");
    assert!(payload_after_drop(&mut buf).iter().all(|&x| x == 0));
}

#[test]
fn a_password_received_does_not_outlive_the_receive() {
    let (a, b) = pair();
    put(&b, 5, 1, 0, b"hunter2");
    let mut buf = MaybeUninit::new(Scrubbed(WireMsg::new()));
    // SAFETY: as above.
    let guard = unsafe { &mut *buf.as_mut_ptr() };
    let m = receive_into(a.as_ref(), guard).unwrap().unwrap();
    assert_eq!(m, (5, 1, false, b"hunter2".to_vec()), "the caller has its copy");
    assert!(guard.0.payload().windows(7).any(|w| w == b"hunter2"), "it crossed this buffer");
    assert!(payload_after_drop(&mut buf).iter().all(|&x| x == 0));
}

#[test]
fn a_message_that_does_not_fit_is_refused_and_keeps_its_handles() {
    let (a, b) = pair();
    let (c, _d) = End::pair(2).unwrap();
    let c = c.into_raw().0;
    let body = vec![b'x'; RS_MAX_BODY + 1];
    assert!(!send(a.raw().0, 1, 1, &body, &[c]));
    assert!(is_open(c) && receive(&b).unwrap().is_none(), "nothing sent, nothing lost");
}

/// More handles than a message carries is refused before the kernel, every one still the caller's.
#[test]
fn a_send_of_more_handles_than_a_message_carries_keeps_them_all() {
    let (a, b) = pair();
    let ends: Vec<_> = (0..=libkern::IPC_HANDLE_MAX).map(|_| End::pair(2).unwrap()).collect();
    let raw: Vec<u64> = ends.iter().map(|(c, _)| c.raw().0).collect();
    assert!(!send(a.raw().0, 1, 1, b"", &raw));
    assert!(raw.iter().all(|&h| is_open(h)), "none closed by the shim");
    assert_eq!(b.recv(&mut WireMsg::new()).err().unwrap().code(), libkern::KError::WouldBlock, "and none sent");
    drop(ends);
    assert_eq!(mock::stray_closes(), 0);
}

#[test]
fn a_send_moves_its_handles_and_a_refused_one_hands_them_back() {
    let (a, b) = pair();
    let (c, _d) = End::pair(2).unwrap();
    let c = c.into_raw().0;
    assert!(send(a.raw().0, 1, 1, b"", &[c]));
    assert!(!is_open(c), "the kernel took it");
    let (e, _f) = End::pair(2).unwrap();
    let e = e.into_raw().0;
    drop(b);
    assert!(!send(a.raw().0, 1, 1, b"", &[e]), "the peer has gone");
    assert!(is_open(e), "still the caller's, not closed by the shim");
    assert_eq!(mock::stray_closes(), 0);
}

#[test]
fn a_receive_says_nothing_queued_peer_gone_or_not_ours() {
    let (a, b) = pair();
    assert_eq!(receive(&a).unwrap(), None);
    b.send(&WireMsg::new(), &mut Carried::new()).unwrap();
    assert_eq!(receive(&a).unwrap(), None, "bytes that are no envelope");
    put(&b, 9, 4, RS_FLAG_ERROR, b"no");
    assert_eq!(recv(a.raw().0), Ok(Some((9, 4, true, b"no".to_vec()))));
    drop(b);
    assert_eq!(receive(&a).err().unwrap().code(), libkern::KError::PeerClosed);
    assert_eq!(recv(a.raw().0), Err(()));
}

#[test]
fn a_reply_is_found_among_others_and_a_gone_peer_ends_the_wait() {
    let (a, b) = pair();
    put(&b, 1, 2, 0, b"other");
    put(&b, 1, 7, RS_FLAG_ERROR, b"mine");
    assert_eq!(call(a.raw().0, 3, 7, b"q", &[]), Some((true, b"mine".to_vec())));
    assert_eq!(got(&b), (3, 7, b"q".to_vec()));
    drop(b);
    assert_eq!(answer(a.raw().0, 7, u64::MAX), None);
}

#[test]
fn a_wait_past_its_deadline_gives_up() {
    let (a, _b) = pair();
    assert_eq!(answer(a.raw().0, 7, 1), None);
    assert_eq!(wait_until(&[a.raw().0], 1), None);
}

#[test]
fn a_wait_names_the_handle_that_is_ready_and_holds_to_four() {
    let (a, b) = pair();
    let (c, d) = End::pair(4).unwrap();
    d.send(&WireMsg::new(), &mut Carried::new()).unwrap();
    assert_eq!(wait(&[a.raw().0, c.raw().0]), Some(1));
    let five = [a.raw().0, c.raw().0, b.raw().0, d.raw().0, 99];
    assert_eq!(wait(&five), None);
}

#[test]
fn a_lookup_gives_the_endpoint_or_zero() {
    mock::reset();
    let (ns, _peer) = mock::answering("/sys/broker");
    let h = lookup(ns, b"/sys/broker", Rights::SEND.bits());
    assert!(h != 0 && is_open(h));
    assert_eq!(lookup(ns, b"/sys/other", Rights::SEND.bits()), 0);
    assert_eq!(lookup(ns, b"/sys/\xff", Rights::SEND.bits()), 0);
    // SAFETY: the mock's handle, open for the test.
    let ns = unsafe { Ref::<libos::Namespace, libos::NsReadOnly>::from_raw(RawHandle(ns), Rights::LOOKUP) };
    assert!(lookup_in(ns, "/sys/broker", Rights::SEND).is_some());
    assert!(lookup_in(ns, "/sys/other", Rights::SEND).is_none());
    close(h);
    assert!(!is_open(h));
    close(0);
}

#[test]
fn a_password_is_read_with_echo_off_and_echo_restored() {
    let (a, b) = pair();
    for body in [&b""[..], b"", b"hunter2", b"", b""] {
        put(&b, 0, 1, 0, body);
    }
    let mut asked = 0;
    assert_eq!(ask_password_then_on(&a, b"pw: ", &mut || asked += 1), Some(b"hunter2".to_vec()));
    assert_eq!(asked, 1);
    let sent: Vec<_> = (0..5).map(|_| got(&b)).collect();
    let ops = [OP_TTY_SET_MODE, OP_TTY_WRITE, OP_TTY_READ_LINE, OP_TTY_SET_MODE, OP_TTY_WRITE];
    assert_eq!(sent.iter().map(|m| m.0).collect::<Vec<_>>(), ops);
    assert_eq!((sent[0].2.clone(), sent[1].2.clone()), (vec![0], b"pw: ".to_vec()));
    assert_eq!((sent[3].2.clone(), sent[4].2.clone()), (vec![TTY_MODE_ECHO], b"\r\n".to_vec()));
}

#[test]
fn a_line_is_read_with_echo_on_and_a_refusal_or_interrupt_is_none() {
    let (a, b) = pair();
    for body in [&b""[..], b"", b"alice"] {
        put(&b, 0, 1, 0, body);
    }
    assert_eq!(ask_line(a.raw().0, b"user: "), Some(b"alice".to_vec()));

    let (a, b) = pair();
    put(&b, 0, 1, 0, b"");
    put(&b, 0, 1, 0, b"");
    put(&b, 0, 1, RS_FLAG_ERROR, b"eof");
    assert_eq!(ask_line_on(&a, b"user: "), None);

    let (a, b) = pair();
    ack(&b);
    put(&b, OP_TTY_INTERRUPT, 0, 0, b"");
    ack(&b);
    put(&b, 0, 1, 0, b"never");
    ack(&b);
    ack(&b);
    assert_eq!(ask_password(a.raw().0, b"pw: "), None, "Ctrl-C is never mind, whatever was typed");

    let (a, b) = pair();
    drop(b);
    assert_eq!(ask_line_on(&a, b"user: "), None, "no terminal");
}

#[test]
fn a_new_password_is_asked_twice_and_must_match() {
    let (a, b) = pair();
    for line in [&b"hunter2"[..], b"hunter2"] {
        for body in [&b""[..], b"", line, b"", b""] {
            put(&b, 0, 1, 0, body);
        }
    }
    let mut seen = vec![];
    let got = ask_new_password_then(a.raw().0, b"new: ", &mut |n| seen.push(n));
    assert_eq!(got, Ok(b"hunter2".to_vec()));
    assert_eq!(seen, [1, 2]);

    let (a, b) = pair();
    for line in [&b"hunter2"[..], b"hunter3"] {
        for body in [&b""[..], b"", line, b"", b""] {
            put(&b, 0, 1, 0, body);
        }
    }
    assert_eq!(ask_new_password_on(&a, b"new: "), Err(NewPassword::Mismatch));
}

fn is_open(h: u64) -> bool {
    mock::is_open(h)
}
