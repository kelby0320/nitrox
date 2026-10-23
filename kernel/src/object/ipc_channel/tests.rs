use super::*;
use crate::libkern::ipc::IPC_PAYLOAD_SIZE;
use crate::mm::test_support::init_global_heap;
use crate::object::ObjectRef;
use crate::object::header::test_probe;

fn new_pair() -> (*mut (), *mut ()) {
    let (a, b) = IpcChannel::try_new_pair(4).unwrap();
    (KBox::into_raw(a).as_ptr() as *mut (), KBox::into_raw(b).as_ptr() as *mut ())
}

fn drop_endpoint(obj: *mut ()) {
    // SAFETY: adopts the endpoint's creation reference, to drop it.
    drop(unsafe { ObjectRef::from_raw(obj, KObjectType::IpcChannel) });
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
///
/// # Safety
///
/// As `IpcChannel::send_push`.
unsafe fn push_no_xfer(obj: *mut (), msg: &StoredMsg) -> SendOutcome {
    let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    // SAFETY: this function's contract, forwarded.
    unsafe { IpcChannel::send_push(obj, msg, &mut t) }
}

/// `recv_pop_into` discarding any (absent) transfers.
///
/// # Safety
///
/// As `IpcChannel::recv_pop_into`.
unsafe fn pop_no_xfer(obj: *mut (), dst: &mut StoredMsg) -> bool {
    let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    // SAFETY: this function's contract, forwarded.
    unsafe { IpcChannel::recv_pop_into(obj, dst, &mut t) }
}

#[test]
fn pair_has_mutual_peers() {
    init_global_heap();
    let (a, b) = new_pair();
    // SAFETY: live endpoints, single-threaded test stands in for SCHED.
    unsafe {
        assert_eq!(IpcChannel::peer_of(a), b);
        assert_eq!(IpcChannel::peer_of(b), a);
    }
    drop_endpoint(a);
    drop_endpoint(b);
}

#[test]
fn send_routes_to_peer_inbox_recv_drains_fifo() {
    init_global_heap();
    let (a, b) = new_pair();
    // SAFETY: live endpoints, single-threaded test.
    unsafe {
        assert!(!IpcChannel::already_signaled(a));
        let edge = push_no_xfer(a, &a_msg(0x11, 3));
        assert_eq!(edge, SendOutcome::Sent { woke_edge: true });
        let edge2 = push_no_xfer(a, &a_msg(0x22, 5));
        assert_eq!(edge2, SendOutcome::Sent { woke_edge: false });
        assert!(!IpcChannel::already_signaled(a));
        assert!(IpcChannel::already_signaled(b));
        assert_eq!(IpcChannel::recv_peek(b), RecvState::HasMsg);
        let mut got = StoredMsg::zeroed();
        assert!(pop_no_xfer(b, &mut got));
        assert_eq!(got.header.payload_len, 3);
        assert_eq!(got.payload[0], 0x11);
        assert!(pop_no_xfer(b, &mut got));
        assert_eq!(got.header.payload_len, 5);
        assert_eq!(got.payload[0], 0x22);
        assert!(!pop_no_xfer(b, &mut got));
        assert_eq!(IpcChannel::recv_peek(b), RecvState::Empty);
    }
    drop_endpoint(a);
    drop_endpoint(b);
}

#[test]
fn send_full_returns_full() {
    init_global_heap();
    let (a, b) = new_pair(); // depth 4
    // SAFETY: live endpoints, single-threaded test.
    unsafe {
        for _ in 0..4 {
            assert!(matches!(push_no_xfer(a, &a_msg(0xAB, 1)), SendOutcome::Sent { .. }));
        }
        assert_eq!(push_no_xfer(a, &a_msg(0xAB, 1)), SendOutcome::Full);
    }
    drop_endpoint(a);
    drop_endpoint(b);
}

#[test]
fn ring_wraps_around() {
    init_global_heap();
    let (a, b) = new_pair(); // depth 4
    // SAFETY: live endpoints, single-threaded test.
    unsafe {
        let mut got = StoredMsg::zeroed();
        for i in 0..3u8 {
            push_no_xfer(a, &a_msg(i, 1));
        }
        for i in 0..2u8 {
            assert!(pop_no_xfer(b, &mut got));
            assert_eq!(got.payload[0], i);
        }
        for i in 3..6u8 {
            assert!(matches!(push_no_xfer(a, &a_msg(i, 1)), SendOutcome::Sent { .. }));
        }
        for i in 2..6u8 {
            assert!(pop_no_xfer(b, &mut got));
            assert_eq!(got.payload[0], i);
        }
        assert!(!pop_no_xfer(b, &mut got));
    }
    drop_endpoint(a);
    drop_endpoint(b);
}

#[test]
fn closing_one_end_marks_peer_closed() {
    init_global_heap();
    let (a, b) = new_pair();
    // SAFETY: live endpoints, single-threaded test.
    unsafe {
        push_no_xfer(a, &a_msg(0x7E, 2));
    }
    drop_endpoint(a);
    // SAFETY: b is still live.
    unsafe {
        assert!(IpcChannel::peer_of(b).is_null());
        assert_eq!(IpcChannel::recv_peek(b), RecvState::HasMsg);
        let mut got = StoredMsg::zeroed();
        assert!(pop_no_xfer(b, &mut got));
        assert_eq!(got.payload[0], 0x7E);
        assert_eq!(IpcChannel::recv_peek(b), RecvState::PeerClosed);
        assert!(IpcChannel::already_signaled(b));
        assert_eq!(push_no_xfer(b, &a_msg(0, 1)), SendOutcome::PeerClosed);
    }
    drop_endpoint(b);
}

#[test]
fn waiters_add_remove_take_caps_at_max() {
    init_global_heap();
    let (a, b) = new_pair();
    // SAFETY: live endpoints, single-threaded test.
    unsafe {
        let ths: [*mut (); IpcChannel::MAX_WAITERS] = core::array::from_fn(|i| (0x1000 + i) as *mut ());
        for &t in &ths {
            assert!(IpcChannel::add_waiter(a, t).is_ok());
        }
        assert!(IpcChannel::add_waiter(a, 0xDEAD as *mut ()).is_err());
        IpcChannel::remove_waiter(a, ths[0]);
        assert!(IpcChannel::add_waiter(a, 0xBEEF as *mut ()).is_ok());
        let mut buf = [core::ptr::null_mut(); IpcChannel::MAX_WAITERS];
        assert_eq!(IpcChannel::take_waiters(a, &mut buf), IpcChannel::MAX_WAITERS);
    }
    drop_endpoint(a);
    drop_endpoint(b);
}

#[test]
fn dispatch_destroy_runs_ipc_arm() {
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair();
    assert_eq!(test_probe::ipc_channel_destroys(), 0);
    drop_endpoint(a);
    drop_endpoint(b);
    assert_eq!(test_probe::ipc_channel_destroys(), 2);
}

#[test]
fn transfer_moves_with_message_and_releases_on_drop() {
    use crate::object::Process;
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair();
    let p = KBox::into_raw(Process::try_new(99).unwrap()).as_ptr() as *mut ();
    // SAFETY: adopts the process's creation reference.
    let pref = unsafe { ObjectRef::from_raw(p, KObjectType::Process) };
    // SAFETY: live endpoints, single-threaded test.
    unsafe {
        let mut send_t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
        send_t[0] = Some(TransferRef { obj: pref, rights: Rights::empty() });
        let mut msg = a_msg(0, 0);
        msg.header.handle_count = 1;
        assert!(matches!(IpcChannel::send_push(a, &msg, &mut send_t), SendOutcome::Sent { .. }));
        assert!(send_t[0].is_none(), "transfer moved into the queue");
        let mut got = StoredMsg::zeroed();
        let mut recv_t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
        assert!(IpcChannel::recv_pop_into(b, &mut got, &mut recv_t));
        assert_eq!(got.header.handle_count, 1);
        let tr = recv_t[0].take().unwrap();
        assert_eq!(tr.obj.as_ptr(), p);
        assert_eq!(test_probe::process_destroys(), 0);
        drop(tr);
        assert_eq!(test_probe::process_destroys(), 1);
    }
    drop_endpoint(a);
    drop_endpoint(b);
}

#[test]
fn undelivered_transfer_released_when_endpoint_destroyed() {
    use crate::object::Process;
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair();
    let p = KBox::into_raw(Process::try_new(7).unwrap()).as_ptr() as *mut ();
    // SAFETY: adopts the process's creation reference.
    let pref = unsafe { ObjectRef::from_raw(p, KObjectType::Process) };
    // SAFETY: live endpoints, single-threaded test.
    unsafe {
        let mut t: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
        t[0] = Some(TransferRef { obj: pref, rights: Rights::empty() });
        let mut msg = a_msg(0, 0);
        msg.header.handle_count = 1;
        IpcChannel::send_push(a, &msg, &mut t);
    }
    assert_eq!(test_probe::process_destroys(), 0);
    drop_endpoint(a);
    drop_endpoint(b);
    assert_eq!(test_probe::process_destroys(), 1);
}

#[test]
fn payload_size_is_one_page_worth() {
    assert_eq!(IPC_PAYLOAD_SIZE, 4008);
    assert_eq!(core::mem::size_of::<StoredMsg>(), 4096);
}

fn make_po() -> ObjectRef {
    // SAFETY: `into_raw` yields the single creation reference; adopt it.
    unsafe {
        ObjectRef::from_raw(
            KBox::into_raw(crate::object::PendingOperation::try_new().unwrap()).as_ptr() as *mut (),
            KObjectType::PendingOperation,
        )
    }
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
    let po = make_po();
    // SAFETY: live endpoints/PO; single-threaded test stands in for SCHED.
    unsafe {
        let mut t = empty_xfer();
        assert_eq!(IpcChannel::send_or_queue(a, &a_msg(7, 1), &mut t, &po), BlockSendOutcome::Sent { woke_edge: true });
        let mut d = a_msg(0, 0);
        assert!(pop_no_xfer(b, &mut d));
        assert_eq!(d.payload[0], 7);
        assert!(IpcChannel::promote_pending_send(b, &mut empty_reclaim()).is_none());
    }
    drop(po);
    drop_endpoint(a);
    drop_endpoint(b);
}

#[test]
fn blocking_send_to_full_ring_queues_then_recv_promotes_fifo() {
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair(); // cap 4; sending on a fills b's ring
    let po = make_po();
    // SAFETY: live endpoints/PO; single-threaded test stands in for SCHED.
    unsafe {
        for i in 0..4u8 {
            assert!(matches!(push_no_xfer(a, &a_msg(i, 1)), SendOutcome::Sent { .. }));
        }
        let mut t = empty_xfer();
        assert_eq!(IpcChannel::send_or_queue(a, &a_msg(99, 1), &mut t, &po), BlockSendOutcome::Queued);
        let mut d = a_msg(0, 0);
        assert!(pop_no_xfer(b, &mut d));
        assert_eq!(d.payload[0], 0);
        let promoted = IpcChannel::promote_pending_send(b, &mut empty_reclaim());
        assert!(promoted.is_some());
        for expect in [1u8, 2, 3, 99] {
            let mut dd = a_msg(0, 0);
            assert!(pop_no_xfer(b, &mut dd));
            assert_eq!(dd.payload[0], expect, "FIFO order");
        }
        assert!(IpcChannel::promote_pending_send(b, &mut empty_reclaim()).is_none());
        drop(promoted); // the queue's PO reference (clone)
    }
    drop(po); // creation reference → last ref, PO destroyed
    assert_eq!(test_probe::pending_op_destroys(), 1);
    drop_endpoint(a);
    drop_endpoint(b);
}

#[test]
fn closing_receiver_releases_a_held_pending_send() {
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair();
    let po = make_po();
    // SAFETY: live endpoints/PO; single-threaded test stands in for SCHED.
    unsafe {
        for i in 0..4u8 {
            push_no_xfer(a, &a_msg(i, 1));
        }
        let mut t = empty_xfer();
        assert_eq!(IpcChannel::send_or_queue(a, &a_msg(99, 1), &mut t, &po), BlockSendOutcome::Queued);
        let mut pos = [core::ptr::null_mut(); IpcChannel::MAX_PENDING_SENDS];
        assert_eq!(IpcChannel::pending_send_pos(b, &mut pos), 1);
        assert_eq!(pos[0], po.as_ptr());
    }
    drop_endpoint(b);
    assert_eq!(test_probe::pending_op_destroys(), 0, "creation ref still held");
    drop(po); // last ref → PO destroyed
    assert_eq!(test_probe::pending_op_destroys(), 1);
    drop_endpoint(a);
}

#[test]
fn cancel_marks_the_matching_held_send() {
    init_global_heap();
    let (a, b) = new_pair();
    let po = make_po();
    // SAFETY: live endpoints/PO; single-threaded test stands in for SCHED.
    unsafe {
        for i in 0..4u8 {
            push_no_xfer(a, &a_msg(i, 1));
        }
        let mut t = empty_xfer();
        assert_eq!(IpcChannel::send_or_queue(a, &a_msg(99, 1), &mut t, &po), BlockSendOutcome::Queued);
        assert!(!IpcChannel::cancel_pending_send(b, 0xDEAD as *mut ()));
        assert!(IpcChannel::cancel_pending_send(b, po.as_ptr()));
    }
    drop(po);
    drop_endpoint(a);
    drop_endpoint(b);
}

#[test]
fn recv_sweeps_cancelled_then_promotes_the_live_send() {
    init_global_heap();
    test_probe::reset();
    let (a, b) = new_pair();
    let po1 = make_po(); // will be cancelled (timed out)
    let po2 = make_po(); // will be delivered
    // SAFETY: live endpoints/POs; single-threaded test stands in for SCHED.
    unsafe {
        for i in 0..4u8 {
            push_no_xfer(a, &a_msg(i, 1));
        }
        let mut t1 = empty_xfer();
        assert_eq!(IpcChannel::send_or_queue(a, &a_msg(50, 1), &mut t1, &po1), BlockSendOutcome::Queued);
        let mut t2 = empty_xfer();
        assert_eq!(IpcChannel::send_or_queue(a, &a_msg(51, 1), &mut t2, &po2), BlockSendOutcome::Queued);
        assert!(IpcChannel::cancel_pending_send(b, po1.as_ptr()));
        let mut d = a_msg(0, 0);
        assert!(pop_no_xfer(b, &mut d));
        let mut rc = empty_reclaim();
        let promoted = IpcChannel::promote_pending_send(b, &mut rc);
        assert_eq!(promoted.as_ref().map(|p| p.as_ptr()), Some(po2.as_ptr()));
        assert_eq!(rc[0].as_ref().map(|r| r.po.as_ptr()), Some(po1.as_ptr()));
        assert!(rc[1].is_none(), "only the one cancelled send was swept");
        drop(promoted); // release po2's queue ref
        drop(rc); // release po1's queue ref (the reclaimed entry)
    }
    drop(po1);
    drop(po2);
    assert_eq!(test_probe::pending_op_destroys(), 2);
    drop_endpoint(a);
    drop_endpoint(b);
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
