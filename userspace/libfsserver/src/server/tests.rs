//! The loop's rules, through libos's model of the kernel: the peer of each channel under test is
//! an endpoint the test reads.

use super::*;
use crate::serve::session_tests::{Never, Spy};
use libkern::RawHandle;
use libos::mock;
use librsproto::error::parse_error;
use librsproto::meta::{FS_SETUP_READ_ONLY, fs_setup, parse_ready};
use librsproto::namespace::{RESOLVE_FILE_LAZY, parse_resolve_reply, resolve_request};
use librsproto::{OP_READY, decode, encode};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::vec::Vec;

/// A handle the model has just made, which nothing else holds.
fn own<T, M>(raw: u64) -> Handle<T, M> {
    // SAFETY: `raw` is fresh from the model and named nowhere else.
    unsafe { Handle::from_raw(RawHandle(raw), Rights::empty()) }
}

fn pair() -> (Endpoint, Endpoint) {
    Endpoint::pair(4).unwrap()
}

/// Receive the one message waiting on `ch`.
fn take(ch: &Endpoint) -> (Msg, Carried) {
    let mut msg = Msg::new();
    let brought = ch.recv(&mut msg).unwrap();
    (msg, brought)
}

fn send(ch: &Endpoint, payload: &[u8], handles: &mut Carried) {
    let mut msg = Msg::new();
    msg.set(payload).unwrap();
    ch.send(&msg, handles).unwrap();
}

fn state(serve_end: Endpoint, device: Device<'_>, control: Option<Endpoint>) -> State<'_> {
    State {
        serve_end,
        device,
        control,
        out: Outbox::new(),
        forget: Outbox::new(),
        content: [0; MAX_FILE],
        listing: [0; DIR_BODY_CAP],
        sessions: Sessions::new(),
    }
}

/// The error a reply carries: its request id, op and code.
fn error_of(msg: &Msg) -> (u64, u16, i32) {
    let m = decode(msg.payload()).unwrap();
    assert!(m.is_error(), "an error reply");
    (m.request_id, m.op, parse_error(m.body).unwrap().kerror)
}

fn resolve(id: u64, flags: u32) -> Vec<u8> {
    let mut body = [0u8; 64];
    let n = resolve_request(&mut body, 0, flags, b"f").unwrap();
    let mut req = [0u8; 128];
    let len = encode(&mut req, OP_NS_RESOLVE, id, 0, &body[..n], 0).unwrap();
    req[..len].to_vec()
}

/// A forwarded rename of `/a` to `/<to>`, laid out as the kernel lays it out.
fn rename(id: u64, to: &[u8], flags: u16) -> Vec<u8> {
    let mut body = [0u8; 64];
    let n = resolve_request(&mut body, 0, RESOLVE_FILE_LAZY | RESOLVE_RENAME, b"a").unwrap();
    let tail = [&(to.len() as u16).to_le_bytes()[..], &flags.to_le_bytes(), to].concat();
    body[n..n + tail.len()].copy_from_slice(&tail);
    let mut req = [0u8; 128];
    let len = encode(&mut req, OP_NS_RESOLVE, id, 0, &body[..n + tail.len()], 0).unwrap();
    req[..len].to_vec()
}

#[test]
fn a_setup_message_gives_the_device_and_the_flag() {
    for (flags, read_only) in [(FS_SETUP_READ_ONLY, true), (0, false)] {
        let (control, supervisor) = pair();
        let device = mock::with_rights(mock::device(), Rights::READ | Rights::WRITE | Rights::TRANSFER);
        let spare = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
        let spare_raw = spare.raw().0;
        let mut body = [0u8; 1];
        fs_setup(&mut body, flags).unwrap();
        let mut sent = Carried::new();
        sent.push(own::<Resource, ReadWrite>(device)).ok().unwrap();
        sent.push(spare).ok().unwrap();
        send(&supervisor, &body, &mut sent);

        let (got, ro) = receive_setup(&control).unwrap();
        assert_eq!(ro, read_only);
        assert!(mock::is_open(got.raw().0), "the device is the server's");
        assert!(!mock::is_open(spare_raw), "a handle it did not ask for is closed, not kept");
        assert_eq!(mock::stray_closes(), 0);
    }
}

/// The disk is made over the device the message brought, and owns it.
#[test]
fn a_disk_is_made_over_the_device_the_setup_brought() {
    let (control, supervisor) = pair();
    let device = mock::with_rights(mock::device(), Rights::READ | Rights::WRITE | Rights::TRANSFER);
    let mut sent = Carried::new();
    sent.push(own::<Resource, ReadWrite>(device)).ok().unwrap();
    send(&supervisor, &[FS_SETUP_READ_ONLY], &mut sent);
    let (got, read_only) = open_disk(&control, Ok);
    let raw = got.raw().0;
    assert!(read_only && mock::is_open(raw));
    drop(got);
    assert!(!mock::is_open(raw), "the disk owns its device");
}

#[test]
fn a_setup_that_never_came_is_none() {
    let (control, supervisor) = pair();
    drop(supervisor);
    assert!(receive_setup(&control).is_none(), "a supervisor that went away sent no device");
    let (control, supervisor) = pair();
    send(&supervisor, &[0], &mut Carried::new());
    assert!(receive_setup(&control).is_none(), "a setup with no device");
}

#[test]
fn ready_carries_the_kernel_end_and_a_refusal_carries_nothing() {
    let (control, supervisor) = pair();
    let (kernel_end, _serve_end) = pair();
    let raw = kernel_end.raw().0;
    let mut out = Outbox::new();
    assert!(out.ready(control.as_ref(), b"fs-test", kernel_end));
    assert!(!mock::is_open(raw), "the kernel end left with the message");
    let (msg, brought) = take(&supervisor);
    let m = decode(msg.payload()).unwrap();
    assert_eq!((m.op, parse_ready(m.body)), (OP_READY, Some(&b"fs-test"[..])));
    assert_eq!(brought.len(), 1);

    assert!(out.refusal(control.as_ref(), &Never));
    let (msg, brought) = take(&supervisor);
    let m = decode(msg.payload()).unwrap();
    assert_eq!((m.op, m.is_error(), brought.len()), (OP_READY, true, 0));
}

#[test]
fn a_reply_the_kernel_did_not_take_is_answered_with_an_error_and_its_handles_closed() {
    let (serve_end, kernel) = pair();
    let mut out = Outbox::new();
    let untransferable = Handle::<Memory, MapReadWrite>::create(4096).unwrap().duplicate(Rights::MAP_READ).unwrap();
    let raw = untransferable.raw().0;
    let n = encode(out.payload(), OP_NS_RESOLVE, 77, RS_FLAG_REPLY, &[], 1).unwrap();
    out.stage(n);
    out.attach(untransferable);
    out.forwarded(serve_end.as_ref(), 77, OP_NS_RESOLVE);

    assert!(!mock::is_open(raw), "the handle the kernel did not take is closed");
    let (msg, brought) = take(&kernel);
    assert_eq!(error_of(&msg), (77, OP_NS_RESOLVE, KError::KernelError.as_i32()));
    assert!(brought.is_empty());
    assert_eq!(kernel.recv(&mut Msg::new()).err().unwrap().code(), KError::WouldBlock, "one answer, no more");
    assert_eq!(mock::stray_closes(), 0);

    let taken = Handle::<Memory, MapReadWrite>::create(4096).unwrap();
    let n = encode(out.payload(), OP_NS_RESOLVE, 78, RS_FLAG_REPLY, &[], 1).unwrap();
    out.stage(n);
    out.attach(taken);
    out.forwarded(serve_end.as_ref(), 78, OP_NS_RESOLVE);
    let (msg, brought) = take(&kernel);
    assert_eq!((decode(msg.payload()).unwrap().request_id, brought.len()), (78, 1), "a reply taken is not doubled");
    assert_eq!(kernel.recv(&mut Msg::new()).err().unwrap().code(), KError::WouldBlock);
}

/// The same through the loop: a lazy resolve whose duplicate of the device cannot be sent.
#[test]
fn a_lazy_resolve_with_an_unsendable_device_is_answered_with_an_error() {
    let mut vol = Spy::default();
    vol.file = true;
    let without = Rights::READ | Rights::WRITE | Rights::DUPLICATE;
    let undoubled = Rights::READ | Rights::WRITE | Rights::TRANSFER;
    for (rights, taken) in [(without, false), (undoubled, false), (without | Rights::TRANSFER, true)] {
        let (serve_end, kernel) = pair();
        let dev = own::<Resource, ReadWrite>(mock::with_rights(mock::device(), rights));
        let mut s = state(serve_end, dev.as_ref(), None);
        s.forwarded(&vol, &resolve(5, RESOLVE_FILE_LAZY));
        let (msg, brought) = take(&kernel);
        if taken {
            let m = decode(msg.payload()).unwrap();
            assert_eq!((m.request_id, m.is_error(), brought.len()), (5, false, 1), "the duplicate went");
        } else {
            assert_eq!(error_of(&msg), (5, OP_NS_RESOLVE, KError::KernelError.as_i32()));
            assert!(brought.is_empty());
        }
        drop((s, brought));
        assert_eq!(mock::stray_closes(), 0);
    }
}

/// An eager resolve hands the kernel a read-only object holding the file, and unmaps its own view.
#[test]
fn an_eager_resolve_replies_a_read_only_copy_of_the_file() {
    let mut vol = Spy::default();
    vol.file = true;
    let (serve_end, kernel) = pair();
    let dev = own::<Resource, ReadWrite>(mock::device());
    let mut s = state(serve_end, dev.as_ref(), None);
    s.forwarded(&vol, &resolve(6, 0));
    let (msg, mut brought) = take(&kernel);
    assert!(!decode(msg.payload()).unwrap().is_error());
    let file: Handle<Memory, MapRead> = brought.take(0).unwrap();
    assert!(file.map(4096).is_ok() && file.map(4097).is_err(), "a page for five bytes");
    assert_eq!(file.duplicate(Rights::MAP_READ).err().unwrap().code(), KError::NoAccess, "only what a read needs");
    let mut seen = [0u8; 5];
    file.map(5).unwrap().read_into(0, &mut seen).unwrap();
    assert_eq!(&seen, b"hello");
    assert_eq!(mock::mappings(), 0, "the server's own view is gone");
}

/// A resolve whose object cannot be made is answered with why, and carries nothing.
#[test]
fn an_eager_resolve_whose_object_cannot_be_made_is_answered_out_of_memory() {
    let mut vol = Spy::default();
    vol.file = true;
    let (serve_end, kernel) = pair();
    let dev = own::<Resource, ReadWrite>(mock::device());
    let mut s = state(serve_end, dev.as_ref(), None);
    let open = mock::open_handles();
    mock::fail_next_create(KError::OutOfMemory.as_i32());
    s.forwarded(&vol, &resolve(6, 0));
    let (msg, brought) = take(&kernel);
    assert_eq!(error_of(&msg), (6, OP_NS_RESOLVE, KError::OutOfMemory.as_i32()));
    assert!(brought.is_empty());
    assert_eq!((mock::open_handles(), mock::mappings()), (open, 0));
}

/// A forwarded rename is the volume's, never a directory open, and every one is answered: a
/// refusal with its code, a malformed one, and one done with no object. A file it replaced is
/// forgotten before it is freed.
#[test]
fn a_rename_resolve_is_answered_whatever_comes_of_it() {
    let dev = own::<Resource, ReadWrite>(mock::device());
    let (serve_end, kernel) = pair();
    let mut vol = Spy::default();
    let mut s = state(serve_end, dev.as_ref(), None);
    s.forwarded(&vol, &rename(7, b"b", 0));
    assert_eq!(error_of(&take(&kernel).0), (7, OP_NS_RESOLVE, KError::NotFound.as_i32()));
    assert_eq!(*vol.calls.borrow(), ["rename_path /a /b"]);
    s.forwarded(&vol, &rename(8, b"", 0));
    assert_eq!(error_of(&take(&kernel).0), (8, OP_NS_RESOLVE, KError::InvalidArgument.as_i32()));

    (vol.renames, vol.orphan) = (true, Some(42));
    s.forwarded(&vol, &rename(9, b"c", RENAME_REPLACE));
    let (forget, _) = take(&kernel);
    let m = decode(forget.payload()).unwrap();
    assert_eq!((m.op, m.body), (OP_FILE_FORGET, &42u64.to_le_bytes()[..]));
    let (reply, brought) = take(&kernel);
    let m = decode(reply.payload()).unwrap();
    assert_eq!((m.request_id, m.is_error(), brought.len()), (9, false, 0));
    assert_eq!(parse_resolve_reply(m.body).unwrap().object_kind, OBJECT_KIND_NONE);
    assert_eq!(vol.calls.borrow().last().unwrap(), "rename_path! /a /c");
    assert_eq!(kernel.recv(&mut Msg::new()).err().unwrap().code(), KError::WouldBlock, "one answer each");
}

/// A touch stamps the file it names and is never answered, a malformed one too: nothing waits on
/// either.
#[test]
fn a_touch_is_the_volumes_and_is_not_answered() {
    let vol = Spy::default();
    let dev = own::<Resource, ReadWrite>(mock::device());
    let (serve_end, kernel) = pair();
    let mut s = state(serve_end, dev.as_ref(), None);
    let mut req = [0u8; 64];
    let len = encode(&mut req, OP_FILE_TOUCH, 0, 0, &42u64.to_le_bytes(), 0).unwrap();
    s.forwarded(&vol, &req[..len]);
    let len = encode(&mut req, OP_FILE_TOUCH, 0, 0, &[1, 2], 0).unwrap();
    s.forwarded(&vol, &req[..len]);
    assert_eq!(*vol.calls.borrow(), ["touch 42"]);
    assert_eq!(kernel.recv(&mut Msg::new()).err().unwrap().code(), KError::WouldBlock);
}

/// A directory session whose reply does not go is undone, and the resolve answered: the kernel
/// took nothing, so the lookup would wait for ever.
#[test]
fn a_session_whose_reply_did_not_go_is_undone_and_answered() {
    let vol = Spy::default();
    let dev = own::<Resource, ReadWrite>(mock::device());
    let (serve_end, kernel) = pair();
    let mut s = state(serve_end, dev.as_ref(), None);
    let open = mock::open_handles();
    mock::fail_next_send(KError::OutOfMemory.as_i32());
    s.forwarded(&vol, &resolve(11, 0));
    let (msg, brought) = take(&kernel);
    assert_eq!(error_of(&msg), (11, OP_NS_RESOLVE, KError::KernelError.as_i32()));
    assert!(brought.is_empty());
    assert!(s.sessions.0.iter().all(Option::is_none), "no session left in a slot");
    assert_eq!((mock::open_handles(), mock::stray_closes()), (open, 0), "both ends closed, once");
}

#[test]
fn a_forget_is_complete_when_the_kernel_has_it() {
    let mut out = Outbox::new();
    let (serve_end, kernel) = pair();
    assert!(forget(&mut out, serve_end.as_ref(), 42));
    let (msg, _) = take(&kernel);
    let m = decode(msg.payload()).unwrap();
    assert_eq!((m.op, u64::from_le_bytes(m.body[..8].try_into().unwrap())), (OP_FILE_FORGET, 42));

    let forwarding = own::<Channel, Only>(mock::forwarding());
    assert!(forget(&mut out, forwarding.as_ref(), 43), "the kernel takes it within the call");
    assert_eq!(mock::stray_closes(), 0, "and no wait on handle 0 follows");

    mock::answer_next_send(KError::InvalidArgument.as_i32());
    assert!(!forget(&mut out, forwarding.as_ref(), 45), "a forget refused is not one done");
    mock::answer_next_send(KError::InvalidArgument.as_i32());
    assert!(!forget(&mut out, serve_end.as_ref(), 46));

    drop(kernel);
    assert!(!forget(&mut out, serve_end.as_ref(), 44), "a kernel that is gone has not forgotten");
}

#[test]
fn a_file_is_kept_when_the_kernel_cannot_be_asked_to_forget_it() {
    let vol = Spy::default();
    let (serve_end, kernel) = pair();
    drop(kernel);
    forget_then_release(&vol, &mut Outbox::new(), serve_end.as_ref(), 5);
    assert!(mock::logged().ends_with(b"the kernel did not answer a forget; the file is kept\n"));
}

#[test]
fn ending_a_directory_ends_only_the_sessions_on_it() {
    let mut sessions = Sessions::new();
    let mut peers = Vec::new();
    for (slot, dir) in [(0, 7), (1, 8), (5, 7)] {
        let (ours, theirs) = pair();
        sessions.0[slot] = Some(Session { ch: ours, dir });
        peers.push(theirs);
    }
    let raw = sessions.0[1].as_ref().unwrap().ch.raw().0;
    assert_eq!(sessions.slot_of(raw), Some(1));
    sessions.end_on(7);
    assert!(sessions.0[0].is_none() && sessions.0[5].is_none() && sessions.0[1].is_some());
    assert_eq!(peers[0].recv(&mut Msg::new()).err().unwrap().code(), KError::PeerClosed, "its endpoint closed");
    assert_eq!(peers[1].recv(&mut Msg::new()).err().unwrap().code(), KError::WouldBlock);
}

/// Every session the table admits has a place in the one wait, with the control channel open or
/// not, and one past the table is answered `WouldBlock` without making a channel.
#[test]
fn the_sessions_and_the_channels_fit_one_wait_at_every_capacity() {
    let vol = Spy::default();
    let dev = own::<Resource, ReadWrite>(mock::device());
    for control_open in [true, false] {
        let (serve_end, kernel) = pair();
        let (control, _supervisor) = pair();
        let mut s = state(serve_end, dev.as_ref(), control_open.then_some(control));
        let room = s.session_capacity();
        assert_eq!(room, MAX_SESSIONS - control_open as usize);
        let mut clients = Vec::new();
        for i in 0..room {
            s.forwarded(&vol, &resolve(100 + i as u64, 0));
            let (_, mut brought) = take(&kernel);
            clients.push(brought.take::<Channel, Only>(0).expect("a session's client end"));
            let (set, raw) = s.watched();
            assert_eq!(set.len(), 1 + control_open as usize + i + 1);
            assert!(raw[..set.len()].iter().all(|&h| h != 0));
        }
        assert_eq!(s.watched().0.len(), MAX_WAIT_HANDLES, "the table fills the wait exactly");
        let open = mock::open_handles();
        s.forwarded(&vol, &resolve(999, 0));
        let (msg, _) = take(&kernel);
        assert_eq!(error_of(&msg), (999, OP_NS_RESOLVE, KError::WouldBlock.as_i32()));
        assert_eq!(mock::open_handles(), open, "a full table makes no channel");
    }
}

#[test]
fn a_sessions_requests_are_answered_on_its_endpoint_and_a_removal_ends_the_others() {
    let vol = Spy::default();
    let dev = own::<Resource, ReadWrite>(mock::device());
    let (serve_end, _kernel) = pair();
    let mut s = state(serve_end, dev.as_ref(), None);
    let (mine, my_client) = pair();
    let (other, other_client) = pair();
    let (raw_mine, raw_other) = (mine.raw().0, other.raw().0);
    s.sessions.0[0] = Some(Session { ch: mine, dir: 7 });
    s.sessions.0[1] = Some(Session { ch: other, dir: 99 });

    let mut body = [0u8; 64];
    let n = librsproto::file::name_request(&mut body, b"d").unwrap();
    let mut req = [0u8; 128];
    let len = encode(&mut req, librsproto::OP_FILE_RMDIR, 4, 0, &body[..n], 0).unwrap();
    send(&my_client, &req[..len], &mut Carried::new());
    let mut inbox = Inbox(Msg::new());
    s.serve_session(&mut inbox, &vol, raw_mine);

    let (reply, _) = take(&my_client);
    let m = decode(reply.payload()).unwrap();
    assert_eq!((m.op, m.request_id, m.is_error()), (librsproto::OP_FILE_RMDIR, 4, false), "answered on its own");
    assert_eq!(*vol.calls.borrow(), ["rmdir d"]);
    assert_eq!(s.sessions.slot_of(raw_mine), Some(0), "and still open");
    assert_eq!(s.sessions.slot_of(raw_other), None, "the session on the removed directory ended");
    assert_eq!(other_client.recv(&mut Msg::new()).err().unwrap().code(), KError::PeerClosed);

    drop(my_client);
    s.serve_session(&mut inbox, &vol, raw_mine);
    assert!(s.sessions.0[0].is_none(), "a closed client frees its slot");
    assert_eq!(mock::stray_closes(), 0);
}

#[test]
fn an_unmount_is_answered_before_the_server_exits_and_a_closed_control_leaves_the_wait() {
    let vol = Spy::default();
    let dev = own::<Resource, ReadWrite>(mock::device());
    let (serve_end, _kernel) = pair();
    let (control, supervisor) = pair();
    let mut s = state(serve_end, dev.as_ref(), Some(control));
    let mut inbox = Inbox(Msg::new());

    let mut req = [0u8; 64];
    let len = encode(&mut req, OP_UNMOUNT, 3, 0, &[], 0).unwrap();
    send(&supervisor, &req[..len], &mut Carried::new());
    let exited = catch_unwind(AssertUnwindSafe(|| s.serve_control(&mut inbox, &vol)));
    assert!(exited.is_err(), "the server exits once it has said so");
    let (reply, _) = take(&supervisor);
    let m = decode(reply.payload()).unwrap();
    assert_eq!((m.op, m.request_id, m.flags & RS_FLAG_REPLY != 0, m.is_error()), (OP_UNMOUNT, 3, true, false));

    drop(supervisor);
    s.serve_control(&mut inbox, &vol);
    assert!(s.control.is_none(), "a closed control channel is dropped");
    assert_eq!(s.session_capacity(), MAX_SESSIONS);
}
