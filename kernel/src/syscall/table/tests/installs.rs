//! Each syscall that installs handles and can fail after one: its installs refused in turn, and a
//! later step refused, the caller's tables are as they were.

use super::*;
use crate::handle::table::{fail_installs_after, net_installed};
use crate::io::block::BlockBackend;
use crate::io::irp::Irp;
use crate::libkern::block::BlockKind;
use crate::libkern::io_op::{IoOp, IoOpcode};
use crate::libkern::ipc::SendMode;
use crate::libkern::spawn::SpawnArgs;
use crate::object::device_node::{BlockGeometry, ResourceDescriptor};
use crate::object::header::test_probe;
use crate::object::ipc_channel::{StoredMsg, TransferRef};
use crate::object::{DeviceNode, IpcChannel, MemoryObject, Namespace, Thread};

/// Run `f` as a thread of `pid`, every install from the `n`th on refused.
fn refusing_from<R>(pid: u32, n: Option<usize>, f: impl FnOnce() -> R) -> R {
    let me = ObjectRef::new(Thread::try_new(1, pid).unwrap());
    fail_installs_after(n);
    let r = crate::sched::with_current(me, f);
    fail_installs_after(None);
    r
}

fn page() -> ObjectRef {
    ObjectRef::new(MemoryObject::try_new(PAGE_SIZE).unwrap())
}

/// A channel neither of whose ends can be installed leaves neither: both are released.
#[test]
fn a_channel_with_an_end_refused_leaves_no_handle() {
    const PID: u32 = 4470;
    init_global_heap();
    global::init_for_tests();
    test_probe::reset();
    let (mut e0, mut e1) = (RawHandle::NULL, RawHandle::NULL);
    let (e0, e1) = (&raw mut e0 as u64, &raw mut e1 as u64);
    let create = || sys_channel_create(e0, e1, 0);
    for n in 0..2 {
        let before = net_installed();
        assert_eq!(refusing_from(PID, Some(n), create), Err(KError::OutOfMemory), "install {n} refused");
        assert_eq!(net_installed(), before, "install {n} refused");
    }
    assert_eq!(test_probe::ipc_channel_destroys(), 4, "both ends, both times");
    assert_eq!(refusing_from(PID, None, create), Ok(0));
    assert_eq!(global::close_all_owned_by(PID), 2);
}

/// A channel whose receiving end is in `pid`'s table.
fn endpoints(pid: u32) -> (Ref<IpcChannel>, RawHandle) {
    let (a, b) = IpcChannel::try_new_pair(2).unwrap();
    (a, install(pid, b.erase(), channel_endpoint_rights()).unwrap())
}

/// Send a message on `a` carrying two fresh pages.
fn send_two(a: &Ref<IpcChannel>, sender: u32) {
    let mut msg = KBox::<StoredMsg>::try_new_zeroed().unwrap();
    msg.header.handle_count = 2;
    let mut transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    let rights = Rights::MAP_READ | Rights::TRANSFER;
    transfers[..2].fill_with(|| Some(TransferRef { obj: page(), rights }));
    let mut claims: [Option<Claim<'static>>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    assert_eq!(send::deliver(a, &msg, &mut transfers, &mut claims, SendMode::NoBlock, 0, sender), Ok(0));
}

/// **A receive keeps a message's handles only once they are copied out**: a copy that faults after
/// both are installed leaves neither, and one that completes keeps both, each the handle the
/// caller is told.
#[test]
fn a_receive_keeps_its_handles_only_once_they_are_copied_out() {
    const SENDER: u32 = 4476;
    const PID: u32 = 4477;
    init_global_heap();
    global::init_for_tests();
    test_probe::reset();
    let (a, b) = endpoints(PID);
    let mut out = KBox::<StoredMsg>::try_new_zeroed().unwrap();
    let mut handles = [RawHandle::NULL; IPC_HANDLE_MAX];
    let mut count = 0usize;
    let (out, count) = (&raw mut *out as u64, &raw mut count);
    let recv = |into: u64| sys_channel_recv(b.bits(), out, into, count as u64);

    send_two(&a, SENDER);
    let before = net_installed();
    let faulted = refusing_from(PID, None, || recv(USER_VIRT_END - 1));
    assert_eq!(faulted, Err(KError::FaultFromUser), "the handles' copy-out");
    assert_eq!(net_installed(), before, "a fault after both installs");
    assert_eq!(test_probe::memory_object_destroys(), 2, "both carried objects released");

    send_two(&a, SENDER);
    let before = net_installed();
    assert_eq!(refusing_from(PID, None, || recv(&raw mut handles as u64)), Ok(0));
    // SAFETY: `count` is this test's own `usize`, which the receive wrote.
    assert_eq!(unsafe { *count }, 2);
    assert_eq!(net_installed(), before + 2, "both kept");
    assert!(handles[..2].iter().all(|&h| global::get().lookup(h, PID, Rights::MAP_READ).is_ok()), "as told");
    assert_eq!(global::close_all_owned_by(PID), 3, "the two and the receiving end");
}

/// A received message whose handles cannot all be installed leaves none of them installed.
#[test]
fn a_receive_with_a_handle_refused_leaves_none() {
    const SENDER: u32 = 4471;
    const PID: u32 = 4472;
    init_global_heap();
    global::init_for_tests();
    test_probe::reset();
    let (a, b) = endpoints(PID);
    let mut out = KBox::<StoredMsg>::try_new_zeroed().unwrap();
    let mut handles = [RawHandle::NULL; IPC_HANDLE_MAX];
    let mut count = 0usize;
    for n in 0..2 {
        send_two(&a, SENDER);
        let before = net_installed();
        let recv = || sys_channel_recv(b.bits(), &raw mut *out as u64, &raw mut handles as u64, &raw mut count as u64);
        assert_eq!(refusing_from(PID, Some(n), recv), Err(KError::OutOfMemory), "install {n} refused");
        assert_eq!(net_installed(), before, "install {n} refused");
        assert_eq!(test_probe::memory_object_destroys(), 2 * (n + 1), "both carried objects released");
    }
    assert_eq!(global::close_all_owned_by(PID), 1, "the receiving end alone");
}

/// A spawn with any of its installs refused, into the child's table or the parent's, leaves
/// neither table holding anything new, and the parent's grants where they were, the moved one
/// as the copied one.
#[test]
fn a_spawn_with_an_install_refused_leaves_neither_table_changed() {
    const PID: u32 = 4473;
    init_global_heap();
    global::init_for_tests();
    test_probe::reset();
    let image = MemoryObject::try_new_filled(&crate::mm::elf::a_test_image()).unwrap();
    let image = install(PID, ObjectRef::new(image), Rights::MAP_READ).unwrap();
    let grants =
        [(); 2].map(|_| install(PID, page(), Rights::MAP_READ | Rights::TRANSFER | Rights::DUPLICATE).unwrap());
    let root = install(PID, ObjectRef::new(Namespace::try_new().unwrap()), namespace_rights()).unwrap();
    // SAFETY: `SpawnArgs` is plain `#[repr(C)]` data; all-zero is a valid value.
    let mut args: SpawnArgs = unsafe { core::mem::zeroed() };
    args.image = image;
    args.namespace = root;
    args.handle_count = 2;
    args.handles[..2].copy_from_slice(&grants);
    args.rights[..2].fill(u64::MAX);
    args.move_mask = 0b10;
    // The child's channel, root and two grants, then the parent's handle to the child: refusing
    // the sixth would let the child start, which needs a kernel stack a host test has not.
    for n in 0..5 {
        let before = net_installed();
        let spawn = || sys_process_spawn(&raw const args as u64);
        assert_eq!(refusing_from(PID, Some(n), spawn), Err(KError::OutOfMemory), "install {n} refused");
        assert_eq!(net_installed(), before, "install {n} refused");
        assert_eq!(test_probe::notification_channel_destroys(), n + 1, "the child's channel released");
    }
    assert!(grants.iter().all(|&h| global::get().lookup(h, PID, Rights::DUPLICATE).is_ok()), "grants kept");
    assert!(global::get().claim(grants[1], PID, Rights::TRANSFER).is_ok(), "the moved one free to move again");
    assert_eq!(global::close_all_owned_by(PID), 4, "the image, the root and the grants alone");
}

/// A blocking send refused after its operation's handle was installed closes that handle.
#[test]
fn a_blocking_send_refused_after_its_handle_closes_it() {
    const PID: u32 = 4474;
    init_global_heap();
    global::init_for_tests();
    test_probe::reset();
    let (a, b) = IpcChannel::try_new_pair(1).unwrap();
    drop(b);
    let msg = KBox::<StoredMsg>::try_new_zeroed().unwrap();
    let mut transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    let mut claims: [Option<Claim<'static>>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    let before = net_installed();
    let sent = send::deliver(&a, &msg, &mut transfers, &mut claims, SendMode::Block, 0, PID);
    assert_eq!(sent, Err(KError::PeerClosed));
    assert_eq!(net_installed(), before);
    assert_eq!(test_probe::pending_op_destroys(), 1, "its operation released");
}

/// An I/O request its device refuses after the operation's handle was installed closes that
/// handle: a transfer that is not whole blocks.
#[test]
fn an_io_request_refused_after_its_handle_closes_it() {
    const PID: u32 = 4475;
    fn no_submit(_: *mut Irp, _: *mut ()) {}
    fn no_poll(_: *mut ()) {}
    init_global_heap();
    global::init_for_tests();
    test_probe::reset();
    let backend = BlockBackend { submit: no_submit, poll: no_poll, ctx: core::ptr::null_mut(), max_frags: 1 };
    let geometry = BlockGeometry { logical_block_size: 512, block_count: 64 };
    let disk = DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"d", backend).unwrap();
    let disk = install(PID, ObjectRef::new(disk), Rights::READ).unwrap();
    let buffer = install(PID, page(), Rights::MAP_WRITE).unwrap();
    let op =
        IoOp { opcode: IoOpcode::Read as u32, flags: 0, buffer: buffer.bits(), buf_offset: 0, offset: 0, length: 100 };
    let before = net_installed();
    let submitted = refusing_from(PID, None, || sys_io_submit(disk.bits(), &raw const op as u64));
    assert_eq!(submitted, Err(KError::InvalidArgument));
    assert_eq!(net_installed(), before);
    assert_eq!(test_probe::pending_op_destroys(), 1, "its operation released");
    assert_eq!(global::close_all_owned_by(PID), 2);
}

/// **A request its device takes keeps its operation's handle**: one that moves nothing, a block
/// read and a flush each answer a handle that is still the caller's.
#[test]
fn an_io_request_its_device_takes_keeps_its_handle() {
    const PID: u32 = 4478;
    fn completes(irp: *mut Irp, _: *mut ()) {
        // SAFETY: the test's live IRP, this backend's to complete.
        unsafe { crate::io::irp::complete(irp, crate::io::irp::IrpStatus::Success as i32, (*irp).length) };
    }
    fn runs(_: *mut ()) {
        crate::dpc::run_pending();
    }
    let _exclusive = crate::dpc::exclusive();
    init_global_heap();
    global::init_for_tests();
    let backend = BlockBackend { submit: completes, poll: runs, ctx: core::ptr::null_mut(), max_frags: 1 };
    let geometry = BlockGeometry { logical_block_size: 512, block_count: 64 };
    let disk = DeviceNode::try_new_block(ResourceDescriptor::ZERO, geometry, BlockKind::Disk, b"d", backend).unwrap();
    let disk = install(PID, ObjectRef::new(disk), Rights::READ | Rights::WRITE).unwrap();
    let buffer = install(PID, page(), Rights::MAP_WRITE).unwrap();
    let op = |opcode: IoOpcode, buffer: u64, length| IoOp {
        opcode: opcode as u32,
        flags: 0,
        buffer,
        buf_offset: 0,
        offset: 0,
        length,
    };
    let requests = [
        ("nothing moved", op(IoOpcode::Read, buffer.bits(), 0)),
        ("a block read", op(IoOpcode::Read, buffer.bits(), 512)),
        ("a flush", op(IoOpcode::Flush, 0, 0)),
    ];
    for (what, op) in requests {
        let before = net_installed();
        let po = refusing_from(PID, None, || sys_io_submit(disk.bits(), &raw const op as u64)).expect(what);
        assert_eq!(net_installed(), before + 1, "{what}");
        assert!(global::get().lookup(RawHandle(po as u64), PID, Rights::empty()).is_ok(), "{what}: its handle");
    }
    crate::dpc::run_pending();
    crate::io::block::reclaim_completed();
    assert_eq!(global::close_all_owned_by(PID), 5, "the disk, the buffer and the three operations");
}
