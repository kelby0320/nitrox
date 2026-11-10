use super::*;
use crate::mm::test_support::init_global_heap;
use crate::object::{IpcChannel, MemoryObject, UserspaceServerReg};

fn pair(depth: u32) -> (Ref<IpcChannel>, Ref<IpcChannel>) {
    let (a, b) = IpcChannel::try_new_pair(depth).unwrap();
    (Ref::new(a), Ref::new(b))
}

type Moving = (RawHandle, [Option<TransferRef>; IPC_HANDLE_MAX], [Option<Claim<'static>>; IPC_HANDLE_MAX]);

/// A memory object's handle in `pid`'s table, claimed to move as a send would claim it.
fn moving(pid: u32) -> Moving {
    let h = global::get()
        .install(pid, ObjectRef::new(MemoryObject::try_new(PAGE_SIZE).unwrap()), Rights::MAP_READ | Rights::TRANSFER)
        .unwrap();
    let (ok, claim) = global::get().claim(h, pid, Rights::TRANSFER).unwrap();
    let mut transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    transfers[0] = Some(TransferRef { obj: ok.object, rights: ok.rights });
    let mut claims: [Option<Claim<'static>>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    claims[0] = Some(claim);
    (h, transfers, claims)
}

fn held(h: RawHandle, pid: u32) -> bool {
    global::get().lookup(h, pid, Rights::empty()).is_ok()
}

fn message() -> KBox<StoredMsg> {
    KBox::<StoredMsg>::try_new_zeroed().unwrap()
}

/// **A send closes the handles it moves once its message is queued**, and only then.
#[test]
fn a_send_closes_the_handles_it_moves_once_its_message_is_queued() {
    const PID: u32 = 4461;
    init_global_heap();
    global::init_for_tests();
    let ((a, _b), msg) = (pair(1), message());
    let (h, mut t, mut c) = moving(PID);
    assert_eq!(deliver(&a, &msg, &mut t, &mut c, SendMode::NoBlock, 0, PID), Ok(0));
    assert!(!held(h, PID), "moved with its message");
    let (h, mut t, mut c) = moving(PID);
    assert_eq!(deliver(&a, &msg, &mut t, &mut c, SendMode::NoBlock, 0, PID), Err(KError::WouldBlock));
    drop((t, c));
    assert!(held(h, PID), "the ring was full: still its owner's");
    assert!(global::get().claim(h, PID, Rights::TRANSFER).is_ok(), "and free to move again");
    global::close_all_owned_by(PID);
}

#[test]
fn a_blocking_send_closes_the_handles_it_moves_once_committed() {
    const PID: u32 = 4462;
    init_global_heap();
    global::init_for_tests();
    let ((a, _b), msg) = (pair(1), message());
    let (h, mut t, mut c) = moving(PID);
    let po = deliver(&a, &msg, &mut t, &mut c, SendMode::Block, 0, PID).unwrap();
    assert!(!held(h, PID), "moved with its message");
    assert!(held(RawHandle(po as u64), PID), "the send's PO");
    global::close_all_owned_by(PID);
}

/// A forwarded reply is consumed whatever it says, and the handles it carried go with it.
#[test]
fn a_forwarded_reply_closes_the_handles_it_carries() {
    const PID: u32 = 4463;
    init_global_heap();
    global::init_for_tests();
    let (k, server_end) = {
        let (a, b) = IpcChannel::try_new_pair(4).unwrap();
        (Ref::new(a), Ref::new(b))
    };
    let reg = Ref::new(UserspaceServerReg::try_new(k.clone()).unwrap());
    assert!(crate::sched::us_server_attach(&reg).is_none());
    let (h, mut t, mut c) = moving(PID);
    assert_eq!(deliver(&server_end, &message(), &mut t, &mut c, SendMode::NoBlock, 0, PID), Ok(0));
    assert!(!held(h, PID), "closed with the reply");
    global::close_all_owned_by(PID);
}

/// **A blocking send that cannot make its PO's handle commits nothing**, so a retry sends it once.
#[test]
fn a_blocking_send_whose_handle_cannot_be_made_sends_nothing() {
    const PID: u32 = 4464;
    init_global_heap();
    global::init_for_tests();
    let ((a, b), msg) = (pair(1), message());
    let (h, mut t, mut c) = moving(PID);
    crate::handle::table::fail_installs_after(Some(0));
    assert_eq!(deliver(&a, &msg, &mut t, &mut c, SendMode::Block, 0, PID), Err(KError::OutOfMemory));
    assert!(!b.already_signaled(&crate::sched::SchedToken::for_test()), "no message for the peer");
    drop((t, c));
    assert!(held(h, PID), "its handle not moved");
    global::close_all_owned_by(PID);
}
