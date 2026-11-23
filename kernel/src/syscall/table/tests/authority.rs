//! Each syscall's own guards, run as a user process holding the syscaps a case gives it.

use super::*;
use crate::handle::table::fail_installs_after;
use crate::libkern::SysCaps;
use crate::libkern::ipc::SendMode;
use crate::libkern::pod::Pod;
use crate::libkern::spawn::SpawnArgs;
use crate::mm::addr_space::AddressSpace;
use crate::object::ipc_channel::StoredMsg;
use crate::object::thread::test_support::user_thread_no_stack;
use crate::object::{IpcChannel, MemoryObject, Namespace, PendingOperation};

/// Run `f` as a thread of user process `pid`, holding `syscaps`.
fn as_user<R>(pid: u32, syscaps: SysCaps, f: impl FnOnce() -> R) -> R {
    let process = Ref::new(Process::try_new_user(pid, AddressSpace::new().unwrap(), syscaps).unwrap());
    crate::sched::with_current(Ref::new(user_thread_no_stack(1, process, 0x40_0000, 0x7fff_f000)), f)
}

fn user_page(pid: u32, rights: Rights) -> RawHandle {
    install(pid, ObjectRef::new(MemoryObject::try_new(PAGE_SIZE).unwrap()), rights).unwrap()
}

fn user_namespace(pid: u32, rights: Rights) -> RawHandle {
    install(pid, ObjectRef::new(Namespace::try_new().unwrap()), rights).unwrap()
}

fn rights_without(rights: Rights, gone: Rights) -> Rights {
    Rights::from_bits_truncate(rights.bits() & !gone.bits())
}

/// **A bind needs `BIND_NAMESPACE`, and a direct one a handle that may leave and takes no base**,
/// each refused where the call makes it.
#[test]
fn a_bind_needs_the_syscap_and_a_direct_one_a_handle_that_may_leave() {
    const PID: u32 = 4481;
    init_global_heap();
    global::init_for_tests();
    let ns = user_namespace(PID, namespace_rights());
    let movable = user_page(PID, Rights::MAP_READ | Rights::TRANSFER | Rights::DUPLICATE);
    let bind = |at: &[u8], h: RawHandle, base: &[u8]| {
        sys_ns_bind(ns.bits(), at.as_ptr() as u64, at.len(), h.bits(), base.as_ptr() as u64, base.len())
    };
    let binder = SysCaps::BIND_NAMESPACE;
    assert_eq!(as_user(PID, SysCaps::empty(), || bind(b"/a", movable, b"")), Err(KError::NoAccess));
    assert_eq!(as_user(PID, binder, || bind(b"/a", movable, b"")), Ok(0));
    for kept in [Rights::TRANSFER, Rights::DUPLICATE] {
        let h = user_page(PID, rights_without(Rights::MAP_READ | Rights::TRANSFER | Rights::DUPLICATE, kept));
        assert_eq!(as_user(PID, binder, || bind(b"/b", h, b"")), Err(KError::NoAccess), "without {kept:?}");
    }
    assert_eq!(as_user(PID, binder, || bind(b"/c", movable, b"/base")), Err(KError::InvalidArgument), "a base");
    global::close_all_owned_by(PID);
}

/// **A copy of a namespace needs `LOOKUP` on it**: a copy binds nothing its caller could not resolve.
#[test]
fn a_copy_of_a_namespace_needs_lookup_on_it() {
    const PID: u32 = 4482;
    init_global_heap();
    global::init_for_tests();
    let blind = user_namespace(PID, rights_without(namespace_rights(), Rights::LOOKUP));
    let seeing = user_namespace(PID, namespace_rights());
    assert_eq!(as_user(PID, SysCaps::empty(), || sys_ns_create(blind.bits())), Err(KError::NoAccess));
    assert!(as_user(PID, SysCaps::empty(), || sys_ns_create(seeing.bits())).is_ok());
    global::close_all_owned_by(PID);
}

/// **A lookup gets what it asked and the binding holds, no more**: of a direct binding, and of a
/// server's memory-object reply.
#[test]
fn a_lookup_gets_only_what_both_its_ask_and_its_grant_allow() {
    const PID: u32 = 4483;
    init_global_heap();
    global::init_for_tests();
    let ns = user_namespace(PID, namespace_rights());
    let read_only = user_page(PID, Rights::MAP_READ | Rights::TRANSFER | Rights::DUPLICATE);
    let at = b"/m";
    let bind = || sys_ns_bind(ns.bits(), at.as_ptr() as u64, at.len(), read_only.bits(), 0, 0);
    assert_eq!(as_user(PID, SysCaps::BIND_NAMESPACE, bind), Ok(0));
    let asked = (Rights::MAP_READ | Rights::MAP_WRITE).bits();
    let po = as_user(PID, SysCaps::empty(), || sys_ns_lookup(ns.bits(), at.as_ptr() as u64, at.len(), asked, 0));
    let po = lookup_as::<PendingOperation>(po.unwrap() as u64, PID, Rights::WAIT).unwrap();
    let (status, h) = read_completion(&po, PID);
    assert_eq!(status, 0);
    assert!(global::get().lookup(RawHandle(h), PID, Rights::MAP_READ).is_ok());
    assert!(global::get().lookup(RawHandle(h), PID, Rights::MAP_WRITE).is_err(), "the binding's rights cap it");
    assert!(global::get().lookup(RawHandle(h), PID, Rights::TRANSFER).is_err(), "and the ask does");

    let mut f = forwarded_memobj_lookup_asking(Rights::MAP_READ | Rights::MAP_WRITE);
    assert_eq!(complete_resolve_reply(&f.reg, &f.reply, &mut f.transfers, 36), Ok(0));
    let (status, h) = read_completion(&f.po, PID);
    assert_eq!(status, 0);
    assert!(global::get().lookup(RawHandle(h), PID, Rights::MAP_READ).is_ok());
    assert!(global::get().lookup(RawHandle(h), PID, Rights::MAP_WRITE).is_err(), "the server's grant caps it");
    assert!(global::get().lookup(RawHandle(h), PID, Rights::TRANSFER).is_err(), "and the ask does");
    global::close_all_owned_by(PID);
}

/// **A send moves only what may leave, carries no more than a payload, and is its sender's**: the
/// pid a message arrives with is the kernel's, whatever the sender wrote there.
#[test]
fn a_send_moves_only_what_may_leave_and_arrives_stamped_with_its_sender() {
    const PID: u32 = 4484;
    init_global_heap();
    global::init_for_tests();
    let (a, b) = IpcChannel::try_new_pair(4).unwrap();
    let (a, b) = (
        install(PID, a.erase(), channel_endpoint_rights()).unwrap(),
        install(PID, b.erase(), channel_endpoint_rights()).unwrap(),
    );
    let mut msg = KBox::<StoredMsg>::try_new_zeroed().unwrap();
    let send = |msg: &StoredMsg, moved: &[RawHandle]| {
        let (m, hs) = (&raw const *msg as u64, moved.as_ptr() as u64);
        as_user(PID, SysCaps::empty(), || {
            sys_channel_send(a.bits(), m, hs, moved.len() as u64, SendMode::NoBlock as u64, 0)
        })
    };
    let kept = user_page(PID, Rights::MAP_READ);
    assert_eq!(send(&msg, &[kept]), Err(KError::NoAccess), "a handle without TRANSFER");
    msg.header.payload_len = IPC_PAYLOAD_SIZE as u32 + 1;
    assert_eq!(send(&msg, &[]), Err(KError::InvalidArgument), "a payload past its end");
    msg.header.payload_len = IPC_PAYLOAD_SIZE as u32;
    msg.header.sender_pid = PID + 1;
    assert_eq!(send(&msg, &[user_page(PID, Rights::MAP_READ | Rights::TRANSFER)]), Ok(0));
    let mut out = KBox::<StoredMsg>::try_new_zeroed().unwrap();
    let (mut handles, mut count) = ([0u64; IPC_HANDLE_MAX], IPC_HANDLE_MAX);
    let (o, hs, c) = (&raw mut *out as u64, handles.as_mut_ptr() as u64, &raw mut count as u64);
    assert_eq!(as_user(PID, SysCaps::empty(), || sys_channel_recv(b.bits(), o, hs, c)), Ok(0));
    assert_eq!((out.header.sender_pid, count), (PID, 1), "stamped by the kernel, and one handle moved");
    global::close_all_owned_by(PID);
}

/// **A map takes no more than its object holds, nor more than its handle allows**: two pages of a
/// one-page object, or a writable or executable map of a handle that reads, are refused.
#[test]
fn a_map_takes_no_more_than_its_object_holds() {
    const PID: u32 = 4485;
    init_global_heap();
    global::init_for_tests();
    let h = user_page(PID, Rights::MAP_READ);
    let map = |size: usize| {
        as_user(PID, SysCaps::empty(), || sys_memory_map(h.bits(), 0, size as u64, Rights::MAP_READ.bits(), 0))
    };
    assert_eq!(map(2 * PAGE_SIZE), Err(KError::InvalidArgument));
    assert!(map(PAGE_SIZE).is_ok());
    for (mapped, as_what) in [(Rights::MAP_WRITE, "writable"), (Rights::MAP_EXEC, "executable")] {
        let asked = (Rights::MAP_READ | mapped).bits();
        let map = || sys_memory_map(h.bits(), 0, PAGE_SIZE as u64, asked, 0);
        assert_eq!(as_user(PID, SysCaps::empty(), map), Err(KError::NoAccess), "{as_what} of a read-only handle");
    }
    global::close_all_owned_by(PID);
}

/// **A child gets no syscap its parent lacks**, whatever it asks for: `parent & requested`. Its
/// first install is refused, so it never starts, which a host test cannot let it do.
#[test]
fn a_child_gets_none_of_the_syscaps_its_parent_lacks() {
    const PID: u32 = 4486;
    init_global_heap();
    global::init_for_tests();
    let image = MemoryObject::try_new_filled(&crate::mm::elf::a_test_image()).unwrap();
    let mut args = SpawnArgs::zeroed();
    args.image = install(PID, ObjectRef::new(image), Rights::MAP_READ).unwrap();
    args.namespace = user_namespace(PID, namespace_rights());
    args.syscaps = (SysCaps::BIND_NAMESPACE | SysCaps::REAL_TIME).bits();
    let parent = SysCaps::BIND_NAMESPACE | SysCaps::AUDIT_CONTROL;
    let spawned = as_user(PID, parent, || {
        fail_installs_after(Some(0));
        let r = sys_process_spawn(&raw const args as u64);
        fail_installs_after(None);
        r
    });
    assert_eq!(spawned, Err(KError::OutOfMemory), "refused at its first install");
    let child = crate::object::process::MADE_WITH.with(core::cell::Cell::get);
    assert_eq!(child, SysCaps::BIND_NAMESPACE, "what both hold, and nothing the parent lacks");
    global::close_all_owned_by(PID);
}

/// **A child's handle has what its parent held and asked to give it, no more**: `held & asked`,
/// read from the install the spawn asked for, which is refused so the child never starts.
#[test]
fn a_childs_handle_has_only_what_its_parent_held_and_asked_to_give() {
    use crate::handle::table::last_install_asked;
    const PID: u32 = 4487;
    init_global_heap();
    global::init_for_tests();
    let image = MemoryObject::try_new_filled(&crate::mm::elf::a_test_image()).unwrap();
    let mut args = SpawnArgs::zeroed();
    args.image = install(PID, ObjectRef::new(image), Rights::MAP_READ).unwrap();
    args.namespace = user_namespace(PID, namespace_rights());
    args.handle_count = 1;
    args.handles[0] = user_page(PID, Rights::MAP_READ | Rights::TRANSFER | Rights::DUPLICATE);
    args.rights[0] = (Rights::MAP_READ | Rights::MAP_WRITE).bits();
    let spawned = as_user(PID, SysCaps::empty(), || {
        fail_installs_after(Some(2));
        let r = sys_process_spawn(&raw const args as u64);
        fail_installs_after(None);
        r
    });
    assert_eq!(spawned, Err(KError::OutOfMemory), "refused at the handle's install");
    let (owner, rights) = last_install_asked().unwrap();
    assert_ne!(owner, PID, "an install into the child's table");
    assert_eq!(rights, Rights::MAP_READ, "neither what it held alone, nor what was asked");
    global::close_all_owned_by(PID);
}
