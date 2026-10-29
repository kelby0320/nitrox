use super::*;
use crate::mm::test_support::init_global_heap;
use crate::object::{IpcChannel, MemoryObject, UserspaceServerReg};

fn adopt<T>(b: KBox<T>, t: KObjectType) -> ObjectRef {
    // SAFETY: `into_raw` yields the box's single creation reference, of an object of type `t`.
    unsafe { ObjectRef::from_raw(KBox::into_raw(b).as_ptr() as *mut (), t) }
}

fn pair(depth: u32) -> (ObjectRef, ObjectRef) {
    let (a, b) = IpcChannel::try_new_pair(depth).unwrap();
    (adopt(a, KObjectType::IpcChannel), adopt(b, KObjectType::IpcChannel))
}

type Moving = (RawHandle, [Option<TransferRef>; IPC_HANDLE_MAX], [Option<Claim<'static>>; IPC_HANDLE_MAX]);

/// A memory object's handle in `pid`'s table, claimed to move as a send would claim it.
fn moving(pid: u32) -> Moving {
    let mem = KBox::into_raw(MemoryObject::try_new(PAGE_SIZE).unwrap()).as_ptr() as *mut ();
    let h = global::get().allocate(pid, mem, KObjectType::MemoryObject, Rights::MAP_READ | Rights::TRANSFER).unwrap();
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
    let (k, server_end) = pair(4);
    let reg = adopt(UserspaceServerReg::try_new(k.clone()).unwrap(), KObjectType::UserspaceServerReg);
    // SAFETY: `k` is pinned by the test's reference.
    assert!(unsafe { crate::sched::us_server_attach(k.as_ptr(), &reg) }.is_none());
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
    crate::handle::table::FAIL_NEXT_ALLOCATE.with(|f| f.set(true));
    assert_eq!(deliver(&a, &msg, &mut t, &mut c, SendMode::Block, 0, PID), Err(KError::OutOfMemory));
    // SAFETY: `b` is pinned by the test's reference, and the test is single-threaded.
    assert!(!unsafe { IpcChannel::already_signaled(b.as_ptr()) }, "no message for the peer");
    drop((t, c));
    assert!(held(h, PID), "its handle not moved");
    global::close_all_owned_by(PID);
}
