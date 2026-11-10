use super::*;
use crate::libkern::ipc::IPC_PAYLOAD_SIZE;
use crate::mm::test_support::init_global_heap;
use crate::object::ObjectRef;
use crate::object::header::test_probe;

fn new_pair() -> (Ref<IpcChannel>, Ref<IpcChannel>) {
    let (a, b) = IpcChannel::try_new_pair(4).unwrap();
    (a, b)
}

fn a_msg(byte: u8, len: u32) -> StoredMsg {
    let mut m = StoredMsg::zeroed();
    m.header.payload_len = len;
    for i in 0..(len as usize) {
        m.payload[i] = byte;
    }
    m
}

/// `send_push` with no transferred handles (the common test case).
fn push_no_xfer(tok: &mut SchedToken, ep: &IpcChannel, msg: &StoredMsg) -> SendOutcome {
    let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    ep.send_push(tok, msg, &mut t)
}

/// `recv_pop_into` discarding any (absent) transfers.
fn pop_no_xfer(tok: &mut SchedToken, ep: &IpcChannel, dst: &mut StoredMsg) -> bool {
    let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    ep.recv_pop_into(tok, dst, &mut t)
}

#[test]
fn pair_has_mutual_peers() {
    init_global_heap();
    let (a, b) = new_pair();
    let tok = &mut SchedToken::for_test();
    assert_eq!(a.peer_addr(tok), b.as_ptr().cast_const());
    assert_eq!(b.peer_addr(tok), a.as_ptr().cast_const());
    drop(a);
    drop(b);
}

#[test]
fn send_routes_to_peer_inbox_recv_drains_fifo() {
    init_global_heap();
    let (a, b) = new_pair();
    let tok = &mut SchedToken::for_test();
    assert!(!a.already_signaled(tok));
    let edge = push_no_xfer(tok, &a, &a_msg(0x11, 3));
    assert_eq!(edge, SendOutcome::Sent { woke_edge: true });
    let edge2 = push_no_xfer(tok, &a, &a_msg(0x22, 5));
    assert_eq!(edge2, SendOutcome::Sent { woke_edge: false });
    assert!(!a.already_signaled(tok));
    assert!(b.already_signaled(tok));
    assert_eq!(b.recv_peek(tok), RecvState::HasMsg);
    let mut got = StoredMsg::zeroed();
    assert!(pop_no_xfer(tok, &b, &mut got));
    assert_eq!(got.header.payload_len, 3);
    assert_eq!(got.payload[0], 0x11);
    assert!(pop_no_xfer(tok, &b, &mut got));
    assert_eq!(got.header.payload_len, 5);
    assert_eq!(got.payload[0], 0x22);
    assert!(!pop_no_xfer(tok, &b, &mut got));
    assert_eq!(b.recv_peek(tok), RecvState::Empty);
    drop(a);
    drop(b);
}

#[test]
fn send_full_returns_full() {
    init_global_heap();
    let tok = &mut SchedToken::for_test();
    let (a, b) = new_pair(); // depth 4
    for _ in 0..4 {
        assert!(matches!(push_no_xfer(tok, &a, &a_msg(0xAB, 1)), SendOutcome::Sent { .. }));
    }
    assert_eq!(push_no_xfer(tok, &a, &a_msg(0xAB, 1)), SendOutcome::Full);
    drop(a);
    drop(b);
}

#[test]
fn ring_wraps_around() {
    init_global_heap();
    let tok = &mut SchedToken::for_test();
    let (a, b) = new_pair(); // depth 4
    let mut got = StoredMsg::zeroed();
    for i in 0..3u8 {
        push_no_xfer(tok, &a, &a_msg(i, 1));
    }
    for i in 0..2u8 {
        assert!(pop_no_xfer(tok, &b, &mut got));
        assert_eq!(got.payload[0], i);
    }
    for i in 3..6u8 {
        assert!(matches!(push_no_xfer(tok, &a, &a_msg(i, 1)), SendOutcome::Sent { .. }));
    }
    for i in 2..6u8 {
        assert!(pop_no_xfer(tok, &b, &mut got));
        assert_eq!(got.payload[0], i);
    }
    assert!(!pop_no_xfer(tok, &b, &mut got));
    drop(a);
    drop(b);
}

#[test]
fn closing_one_end_marks_peer_closed() {
    init_global_heap();
    let (a, b) = new_pair();
    let tok = &mut SchedToken::for_test();
    push_no_xfer(tok, &a, &a_msg(0x7E, 2));
    drop(a);
    assert!(b.peer_addr(tok).is_null());
    assert_eq!(b.recv_peek(tok), RecvState::HasMsg);
    let mut got = StoredMsg::zeroed();
    assert!(pop_no_xfer(tok, &b, &mut got));
    assert_eq!(got.payload[0], 0x7E);
    assert_eq!(b.recv_peek(tok), RecvState::PeerClosed);
    assert!(b.already_signaled(tok));
    assert_eq!(push_no_xfer(tok, &b, &a_msg(0, 1)), SendOutcome::PeerClosed);
    drop(b);
}

#[test]
fn waiters_add_remove_take_caps_at_max() {
    init_global_heap();
    let (a, b) = new_pair();
    let tok = &mut SchedToken::for_test();
    let ths: [*mut (); IpcChannel::MAX_WAITERS] = core::array::from_fn(|i| (0x1000 + i) as *mut ());
    for &t in &ths {
        assert!(a.add_waiter(tok, t).is_ok());
    }
    assert!(a.add_waiter(tok, 0xDEAD as *mut ()).is_err());
    a.remove_waiter(tok, ths[0]);
    assert!(a.add_waiter(tok, 0xBEEF as *mut ()).is_ok());
    assert_eq!(&a.take_waiters(tok)[..], &[ths[1], ths[2], ths[3], 0xBEEF as *mut ()]);
    assert!(a.take_waiters(tok).is_empty());
    drop(a);
    drop(b);
}

#[test]
fn dispatch_destroy_runs_ipc_arm() {
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair();
    assert_eq!(test_probe::ipc_channel_destroys(), 0);
    drop(a);
    drop(b);
    assert_eq!(test_probe::ipc_channel_destroys(), 2);
}

#[test]
fn transfer_moves_with_message_and_releases_on_drop() {
    use crate::object::Process;
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair();
    let tok = &mut SchedToken::for_test();
    let pref = ObjectRef::new(Process::try_new(99).unwrap());
    let p = pref.as_ptr();
    let mut send_t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    send_t[0] = Some(TransferRef { obj: pref, rights: Rights::empty() });
    let mut msg = a_msg(0, 0);
    msg.header.handle_count = 1;
    assert!(matches!(a.send_push(tok, &msg, &mut send_t), SendOutcome::Sent { .. }));
    assert!(send_t[0].is_none(), "transfer moved into the queue");
    let mut got = StoredMsg::zeroed();
    let mut recv_t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    assert!(b.recv_pop_into(tok, &mut got, &mut recv_t));
    assert_eq!(got.header.handle_count, 1);
    let tr = recv_t[0].take().unwrap();
    assert_eq!(tr.obj.as_ptr(), p);
    assert_eq!(test_probe::process_destroys(), 0);
    drop(tr);
    assert_eq!(test_probe::process_destroys(), 1);
    drop(a);
    drop(b);
}

#[test]
fn undelivered_transfer_released_when_endpoint_destroyed() {
    use crate::object::Process;
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair();
    let tok = &mut SchedToken::for_test();
    let pref = ObjectRef::new(Process::try_new(7).unwrap());
    let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    t[0] = Some(TransferRef { obj: pref, rights: Rights::empty() });
    let mut msg = a_msg(0, 0);
    msg.header.handle_count = 1;
    a.send_push(tok, &msg, &mut t);
    assert_eq!(test_probe::process_destroys(), 0);
    drop(a);
    drop(b);
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
fn payload_size_is_one_page_worth() {
    assert_eq!(IPC_PAYLOAD_SIZE, 4008);
    assert_eq!(core::mem::size_of::<StoredMsg>(), 4096);
}

fn make_po() -> Ref<PendingOperation> {
    Ref::new(PendingOperation::try_new().unwrap())
}

fn empty_xfer() -> [Option<TransferRef>; IPC_HANDLE_MAX] {
    core::array::from_fn(|_| None)
}

fn empty_reclaim() -> [Option<ReclaimedSend>; IpcChannel::MAX_PENDING_SENDS] {
    core::array::from_fn(|_| None)
}

#[test]
fn blocking_send_to_ring_with_space_delivers_immediately() {
    init_global_heap();
    let (a, b) = new_pair();
    let tok = &mut SchedToken::for_test();
    let po = make_po();
    let mut t = empty_xfer();
    assert_eq!(a.send_or_queue(tok, &a_msg(7, 1), &mut t, &po), BlockSendOutcome::Sent { woke_edge: true });
    let mut d = a_msg(0, 0);
    assert!(pop_no_xfer(tok, &b, &mut d));
    assert_eq!(d.payload[0], 7);
    assert!(b.promote_pending_send(tok, &mut empty_reclaim()).is_none());
    drop(po);
    drop(a);
    drop(b);
}

#[test]
fn blocking_send_to_full_ring_queues_then_recv_promotes_fifo() {
    init_global_heap();
    test_probe::reset();
    let tok = &mut SchedToken::for_test();
    let (a, b) = new_pair(); // cap 4; sending on a fills b's ring
    let po = make_po();
    for i in 0..4u8 {
        assert!(matches!(push_no_xfer(tok, &a, &a_msg(i, 1)), SendOutcome::Sent { .. }));
    }
    let mut t = empty_xfer();
    assert_eq!(a.send_or_queue(tok, &a_msg(99, 1), &mut t, &po), BlockSendOutcome::Queued);
    let mut d = a_msg(0, 0);
    assert!(pop_no_xfer(tok, &b, &mut d));
    assert_eq!(d.payload[0], 0);
    let promoted = b.promote_pending_send(tok, &mut empty_reclaim());
    assert!(promoted.is_some());
    for expect in [1u8, 2, 3, 99] {
        let mut dd = a_msg(0, 0);
        assert!(pop_no_xfer(tok, &b, &mut dd));
        assert_eq!(dd.payload[0], expect, "FIFO order");
    }
    assert!(b.promote_pending_send(tok, &mut empty_reclaim()).is_none());
    drop(promoted); // the queue's PO reference (clone)
    drop(po); // creation reference → last ref, PO destroyed
    assert_eq!(test_probe::pending_op_destroys(), 1);
    drop(a);
    drop(b);
}

#[test]
fn closing_receiver_releases_a_held_pending_send() {
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair();
    let tok = &mut SchedToken::for_test();
    let po = make_po();
    for i in 0..4u8 {
        push_no_xfer(tok, &a, &a_msg(i, 1));
    }
    let mut t = empty_xfer();
    assert_eq!(a.send_or_queue(tok, &a_msg(99, 1), &mut t, &po), BlockSendOutcome::Queued);
    let held = b.held_sends(tok);
    assert_eq!(held[0].as_ref().map(Ref::as_ptr), Some(po.as_ptr()));
    assert!(held[1].is_none());
    drop((held, b));
    assert_eq!(test_probe::pending_op_destroys(), 0, "creation ref still held");
    drop(po); // last ref → PO destroyed
    assert_eq!(test_probe::pending_op_destroys(), 1);
    drop(a);
}

#[test]
fn cancel_marks_the_matching_held_send() {
    init_global_heap();
    let (a, b) = new_pair();
    let tok = &mut SchedToken::for_test();
    let po = make_po();
    for i in 0..4u8 {
        push_no_xfer(tok, &a, &a_msg(i, 1));
    }
    let mut t = empty_xfer();
    assert_eq!(a.send_or_queue(tok, &a_msg(99, 1), &mut t, &po), BlockSendOutcome::Queued);
    assert!(!b.cancel_pending_send(tok, 0xDEAD as *const PendingOperation));
    assert!(b.cancel_pending_send(tok, po.as_ptr()));
    drop(po);
    drop(a);
    drop(b);
}

#[test]
fn recv_sweeps_cancelled_then_promotes_the_live_send() {
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair();
    let tok = &mut SchedToken::for_test();
    let po1 = make_po(); // will be cancelled (timed out)
    let po2 = make_po(); // will be delivered
    for i in 0..4u8 {
        push_no_xfer(tok, &a, &a_msg(i, 1));
    }
    let mut t1 = empty_xfer();
    assert_eq!(a.send_or_queue(tok, &a_msg(50, 1), &mut t1, &po1), BlockSendOutcome::Queued);
    let mut t2 = empty_xfer();
    assert_eq!(a.send_or_queue(tok, &a_msg(51, 1), &mut t2, &po2), BlockSendOutcome::Queued);
    assert!(b.cancel_pending_send(tok, po1.as_ptr()));
    let mut d = a_msg(0, 0);
    assert!(pop_no_xfer(tok, &b, &mut d));
    let mut rc = empty_reclaim();
    let promoted = b.promote_pending_send(tok, &mut rc);
    assert_eq!(promoted.as_ref().map(|p| p.as_ptr()), Some(po2.as_ptr()));
    assert_eq!(rc[0].as_ref().map(|r| r.po.as_ptr()), Some(po1.as_ptr()));
    assert!(rc[1].is_none(), "only the one cancelled send was swept");
    drop(promoted); // release po2's queue ref
    drop(rc); // release po1's queue ref (the reclaimed entry)
    drop(po1);
    drop(po2);
    assert_eq!(test_probe::pending_op_destroys(), 2);
    drop(a);
    drop(b);
}

#[test]
fn a_sealed_message_carries_nothing_past_its_payload() {
    let mut m = StoredMsg::zeroed();
    m.payload.fill(0xA5);
    m.header.payload_len = 28;
    (m.header._pad1, m.header._pad2, m.header.flags) = (0xA5, [0xA5; 4], 0xA5A5);
    assert_eq!(m.seal(), Some(28));
    assert!(m.payload[..28].iter().all(|&b| b == 0xA5) && m.payload[28..].iter().all(|&b| b == 0));
    assert_eq!((m.header._pad1, m.header._pad2), (0, [0; 4]), "the header's padding");
    assert_eq!(m.header.flags, 0xA5A5, "flags are the sender's");
    m.header.payload_len = IPC_PAYLOAD_SIZE as u32;
    assert_eq!(m.seal(), Some(IPC_PAYLOAD_SIZE));
    m.header.payload_len = IPC_PAYLOAD_SIZE as u32 + 1;
    assert_eq!(m.seal(), None);
}
