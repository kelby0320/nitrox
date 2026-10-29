use super::*;
use crate::libkern::KBox;
use crate::libkern::handle::KObjectType;
use crate::mm::test_support::init_global_heap;
use crate::object::Process;

/// **A block map the page cache cannot use is refused**, each bound at the values either
/// side of it: block sizes 256..8192 (and 0, and a non-power of two), and a run whose file
/// end, device end or device end in bytes is `u64::MAX` (taken) or one past (refused).
#[test]
fn a_block_map_the_page_cache_cannot_use_is_refused() {
    init_global_heap();
    let body = |bs: u32, runs: &[(u64, u64, u32)]| {
        let mut b = vec![0u8; 32 + 24 * runs.len()];
        b[8..12].copy_from_slice(&bs.to_le_bytes());
        b[12..16].copy_from_slice(&(runs.len() as u32).to_le_bytes());
        for (i, &(file_block, device_lba, length)) in runs.iter().enumerate() {
            let at = 32 + 24 * i;
            b[at..at + 8].copy_from_slice(&file_block.to_le_bytes());
            b[at + 8..at + 16].copy_from_slice(&device_lba.to_le_bytes());
            b[at + 16..at + 20].copy_from_slice(&length.to_le_bytes());
        }
        b
    };
    let used = |b: &[u8]| -> bool {
        let Ok((h, runs)) = block_map(b) else {
            return false;
        };
        let bs = h.block_size as u64;
        let _first_block_of_a_page = PAGE_SIZE as u64 / bs;
        for r in runs.iter().filter(|r| r.length > 0) {
            let last = r.file_block + r.length as u64 - 1;
            let _byte = crate::object::file_object::device_block_in(&runs, last) * bs;
        }
        true
    };
    let ok_run = [(0, 2048, 8)];
    for (bs, taken) in
        [(0, false), (256, false), (511, false), (512, true), (1024, true), (3000, false), (4096, true), (8192, false)]
    {
        assert_eq!(used(&body(bs, &ok_run)), taken, "block size {bs}");
    }
    let top = u64::MAX;
    let in_bytes = top / 512; // the last device block whose end in bytes fits
    for (run, taken, what) in [
        ((top - 15, 2048, 8), true, "a file end at the last page below u64::MAX"),
        ((top - 7, 2048, 8), false, "a page past"),
        ((0, top - 8, 8), false, "a device end of u64::MAX, which no block size fits in bytes"),
        ((0, top - 7, 8), false, "a device end one past u64::MAX"),
        ((0, in_bytes - 8, 8), true, "a device end in bytes of u64::MAX, to the sector"),
        ((0, in_bytes - 7, 8), false, "one past"),
    ] {
        assert_eq!(used(&body(512, &[run])), taken, "{what}");
    }
    assert_eq!(block_map(&body(512, &[(0, 2048, 8)])[..32 + 23]).err(), Some(KError::KernelError), "a run cut short");
    // A page lies in one run: written back from its first block, its rest would land past the run.
    for (runs, taken, what) in [
        (&[(0, 100, 4), (4, 500, 4)][..], true, "1 KiB blocks, runs of whole pages"),
        (&[(0, 100, 2), (2, 500, 2)], false, "a boundary inside a page"),
        (&[(0, 100, 3)], false, "a run ending inside a page"),
        (&[(1, 100, 4)], false, "a run starting inside one"),
        (&[(3, 100, 0), (0, 200, 4)], true, "a run of no blocks, wherever it starts"),
    ] {
        assert_eq!(used(&body(1024, runs)), taken, "{what}");
    }
    // 200,000 runs is 4.8 MB, past the buddy's largest block: refused before it is reserved.
    let mut claims_more = body(512, &[(0, 2048, 8)]);
    claims_more[12..16].copy_from_slice(&200_000u32.to_le_bytes());
    assert_eq!(block_map(&claims_more).err(), Some(KError::KernelError), "a count past the body");
}

#[test]
fn unknown_number_is_unsupported() {
    assert_eq!(dispatch(0xDEAD, 0, 0, 0, 0, 0, 0), KError::Unsupported.as_isize());
}

#[test]
fn join_subtree_prepends_base_stripping_leading_slash() {
    let mut out = [0u8; NS_PATH_MAX];
    assert_eq!(join_subtree(b"/home/alice", b"notes.txt", &mut out), Some(&b"home/alice/notes.txt"[..]));
    assert_eq!(join_subtree(b"/home/alice", b"", &mut out), Some(&b"home/alice"[..]));
    assert_eq!(join_subtree(b"/a/b", b"c/d/e", &mut out), Some(&b"a/b/c/d/e"[..]));
}

#[test]
fn join_subtree_rejects_overflow() {
    let mut tiny = [0u8; 4];
    assert_eq!(join_subtree(b"/home/alice", b"notes.txt", &mut tiny), None);
    let mut exact = [0u8; 10];
    assert_eq!(join_subtree(b"/home/alice", b"", &mut exact), Some(&b"home/alice"[..]));
    let mut one_short = [0u8; 9];
    assert_eq!(join_subtree(b"/home/alice", b"", &mut one_short), None);
}

#[test]
fn clock_read_invalid_selector_is_invalid_argument() {
    assert_eq!(sys_clock_read(99, 0xDEAD_BEEF), Err(KError::InvalidArgument));
    assert_eq!(dispatch(SYS_CLOCK_READ, 99, 0xDEAD_BEEF, 0, 0, 0, 0), KError::InvalidArgument.as_isize(),);
}

#[test]
fn clock_read_unsupported_clocks_are_unsupported() {
    let valid_out = 0x1000u64; // page-aligned, u64-aligned, user-half
    assert_eq!(sys_clock_read(1, valid_out), Err(KError::Unsupported)); // Realtime
    assert_eq!(sys_clock_read(2, valid_out), Err(KError::Unsupported)); // ProcessCpu
    assert_eq!(sys_clock_read(3, valid_out), Err(KError::Unsupported)); // ThreadCpu
}

/// **`sys_clock_set`'s arguments are refused before its authority is asked**: each refusal
/// proves it, as `require_syscap` panics on the host.
#[test]
fn clock_set_refuses_its_arguments_before_asking_for_system_clock() {
    let ok_ns = 1_790_605_800u64 * 1_000_000_000; // 2026-09-28T14:30:00Z
    for clock in [0, 2, 3, 4, u64::MAX] {
        assert_eq!(sys_clock_set(clock, ok_ns), Err(KError::InvalidArgument), "clock {clock}");
    }
    assert_eq!(sys_clock_set(1, 946_684_799 * 1_000_000_000), Err(KError::InvalidArgument));
    assert_eq!(sys_clock_set(1, 4_102_444_800 * 1_000_000_000), Err(KError::InvalidArgument));
    assert_eq!(dispatch(SYS_CLOCK_SET, 0, ok_ns, 0, 0, 0, 0), KError::InvalidArgument.as_isize(),);
}

#[test]
fn timer_create_rejects_unknown_flags() {
    assert_eq!(sys_timer_create(1), Err(KError::InvalidArgument));
    assert_eq!(dispatch(SYS_TIMER_CREATE, 0x8000_0000, 0, 0, 0, 0, 0), KError::InvalidArgument.as_isize(),);
}

#[test]
fn wait_rejects_bad_count() {
    assert_eq!(sys_wait(0xDEAD, 0, 0xBEEF, 0), Err(KError::InvalidArgument));
    assert_eq!(sys_wait(0xDEAD, MAX_WAIT_HANDLES + 1, 0xBEEF, 0), Err(KError::TooLarge),);
}

#[test]
fn wait_rejects_bad_pointers() {
    let bad = USER_VIRT_END;
    assert_eq!(sys_wait(bad, 1, 0x1000, 1000), Err(KError::FaultFromUser));
    assert_eq!(sys_wait(0x1000, 1, bad, 1000), Err(KError::FaultFromUser));
}

#[test]
fn timer_rights_are_allocatable_on_a_timer() {
    init_global_heap();
    let t = HandleTable::try_new(0x1234_5678_9ABC_DEF0).unwrap();
    let timer = crate::object::Timer::try_new().unwrap();
    let ptr = KBox::into_raw(timer).as_ptr() as *mut ();
    let h = t.allocate(1, ptr, KObjectType::Timer, timer_rights()).expect("timer_rights must be valid for a Timer");
    let co = t.close(h, 1).unwrap();
    // SAFETY: the table's reference, which `close` handed back.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
}

#[test]
fn notif_recv_rejects_bad_pointer() {
    assert_eq!(sys_notif_recv(0xDEAD, USER_VIRT_END), Err(KError::FaultFromUser));
}

#[test]
fn notif_channel_rights_are_allocatable_on_a_channel() {
    init_global_heap();
    let t = HandleTable::try_new(0x0FED_CBA9_8765_4321).unwrap();
    let chan = crate::object::NotificationChannel::try_new().unwrap();
    let ptr = KBox::into_raw(chan).as_ptr() as *mut ();
    let rights = Rights::WAIT | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER;
    let h = t
        .allocate(1, ptr, KObjectType::NotificationChannel, rights)
        .expect("WAIT + generic rights must be valid for a NotificationChannel");
    let co = t.close(h, 1).unwrap();
    // SAFETY: the table's reference, which `close` handed back.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
}

#[test]
fn namespace_rights_are_allocatable_on_a_namespace() {
    init_global_heap();
    let t = HandleTable::try_new(0x1357_9BDF_0246_8ACE).unwrap();
    let ns = Namespace::try_new().unwrap();
    let ptr = KBox::into_raw(ns).as_ptr() as *mut ();
    let h = t
        .allocate(1, ptr, KObjectType::Namespace, namespace_rights())
        .expect("namespace_rights must be valid for a Namespace");
    let co = t.close(h, 1).unwrap();
    // SAFETY: the table's reference, which `close` handed back.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
}

#[test]
fn entropy_rights_are_allocatable_on_an_entropy_object() {
    init_global_heap();
    let t = HandleTable::try_new(0x2468_ACE0_1357_9BDF).unwrap();
    let e = EntropyObject::try_new().unwrap();
    let ptr = KBox::into_raw(e).as_ptr() as *mut ();
    let h = t
        .allocate(1, ptr, KObjectType::EntropyObject, entropy_rights())
        .expect("entropy_rights must be valid for an EntropyObject");
    let co = t.close(h, 1).unwrap();
    // SAFETY: the table's reference, which `close` handed back.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
}

#[test]
fn the_system_control_handle_cannot_be_given_away() {
    use crate::object::SystemControl;
    use crate::object::system_control::INIT_RIGHTS;
    init_global_heap();
    let t = HandleTable::try_new(0x1357_9BDF_2468_ACE0).unwrap();
    let ptr = KBox::into_raw(SystemControl::try_new().unwrap()).as_ptr() as *mut ();
    let h = t
        .allocate(1, ptr, KObjectType::SystemControl, INIT_RIGHTS)
        .expect("INIT_RIGHTS must be valid for a SystemControl");
    assert!(t.lookup(h, 1, Rights::WRITE).is_ok());
    assert!(t.duplicate(h, 1, Rights::WRITE).is_err(), "a duplicate is refused");
    let held = t.lookup(h, 1, Rights::empty()).unwrap().rights;
    assert_eq!(direct_bindable(held), Err(KError::NoAccess), "and so is a bind");
    assert_eq!(direct_bindable(held | Rights::TRANSFER), Ok(()), "which TRANSFER would allow");
    let co = t.close(h, 1).unwrap();
    // SAFETY: `co` carries the handle's one reference, released here once.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
}

#[test]
fn entropy_read_rejects_zero_and_oversize_len() {
    assert_eq!(sys_entropy_read(0xDEAD, 0x1000, 0), Ok(0));
    assert_eq!(sys_entropy_read(0xDEAD, 0x1000, ENTROPY_READ_MAX + 1), Err(KError::TooLarge),);
}

#[test]
fn map_ns_err_maps_each_variant() {
    assert_eq!(map_ns_err(NsError::InvalidPath), KError::InvalidArgument);
    assert_eq!(map_ns_err(NsError::AlreadyBound), KError::AlreadyExists);
    assert_ne!(map_ns_err(NsError::AlreadyBound), map_ns_err(NsError::InvalidPath));
    assert_eq!(map_ns_err(NsError::NotBound), KError::NotFound);
    assert_eq!(map_ns_err(NsError::OutOfMemory), KError::OutOfMemory);
}

#[test]
fn copy_ns_path_rejects_zero_and_oversize_len() {
    let mut buf = [0u8; NS_PATH_MAX];
    assert_eq!(copy_ns_path(0x1000, 0, &mut buf), Err(KError::InvalidArgument));
    assert_eq!(copy_ns_path(0x1000, NS_PATH_MAX + 1, &mut buf), Err(KError::TooLarge),);
}

#[test]
fn channel_create_rejects_bad_pointer_and_oversize_depth() {
    assert_eq!(sys_channel_create(USER_VIRT_END, 0x1000, 4), Err(KError::FaultFromUser),);
    assert_eq!(sys_channel_create(0x1000, USER_VIRT_END, 4), Err(KError::FaultFromUser),);
    assert_eq!(sys_channel_create(0x1000, 0x1000, IPC_MAX_QUEUE_DEPTH as u64 + 1), Err(KError::InvalidArgument),);
}

#[test]
fn channel_send_rejects_mode_and_oversize_count_before_pointers() {
    assert_eq!(sys_channel_send(0xDEAD, 0xDEAD, 0, 0, 99, 0), Err(KError::InvalidArgument),);
    assert_eq!(
        sys_channel_send(0xDEAD, 0xDEAD, 0, 0, 1 << 32 | 1, 0),
        Err(KError::InvalidArgument),
        "NoBlock, high bits set"
    );
    assert_eq!(sys_channel_send(0xDEAD, 0xDEAD, 0, (IPC_HANDLE_MAX + 1) as u64, 1, 0), Err(KError::TooLarge),);
}

/// Each handle moves once: one listed twice would arrive as two, though its holder may not
/// duplicate it. Refused before anything is looked up: past that, a host test has no process.
#[test]
fn a_handle_listed_twice_is_refused_by_a_send_and_a_spawn() {
    let send = |hs: [u64; 2]| sys_channel_send(0xDEAD, 0x1000, hs.as_ptr() as u64, 2, 1, 0);
    assert_eq!(send([5, 5]), Err(KError::InvalidArgument));
    let spawn = |hs: [u64; 2]| {
        // SAFETY: `SpawnArgs` is plain `#[repr(C)]` data; all-zero is a valid value.
        let mut args: crate::libkern::spawn::SpawnArgs = unsafe { core::mem::zeroed() };
        args.handle_count = 2;
        args.handles[..2].copy_from_slice(&hs.map(RawHandle));
        sys_process_spawn(&raw const args as u64)
    };
    assert_eq!(spawn([5, 5]), Err(KError::InvalidArgument));
}

#[test]
fn an_image_too_large_for_one_allocation_is_too_large_not_out_of_memory() {
    assert_eq!(image_fits(MAX_IMAGE_BYTES), Ok(()));
    assert_eq!(image_fits(MAX_IMAGE_BYTES + 1), Err(KError::TooLarge));
}

#[test]
fn channel_send_rejects_bad_message_pointer() {
    assert_eq!(sys_channel_send(0xDEAD, USER_VIRT_END, 0, 0, 1, 0), Err(KError::FaultFromUser),);
}

#[test]
fn channel_recv_rejects_bad_pointers() {
    assert_eq!(sys_channel_recv(0xDEAD, USER_VIRT_END, 0, 0x1000), Err(KError::FaultFromUser));
    assert_eq!(sys_channel_recv(0xDEAD, 0x1000, 0, USER_VIRT_END), Err(KError::FaultFromUser));
}

#[test]
fn channel_endpoint_rights_are_allocatable_on_a_channel() {
    init_global_heap();
    let t = HandleTable::try_new(0x1357_9BDF_0246_8ACE).unwrap();
    let (a, b) = IpcChannel::try_new_pair(4).unwrap();
    let a_ptr = KBox::into_raw(a).as_ptr() as *mut ();
    let h = t
        .allocate(1, a_ptr, KObjectType::IpcChannel, channel_endpoint_rights())
        .expect("channel_endpoint_rights must be valid for an IpcChannel");
    let co = t.close(h, 1).unwrap();
    // SAFETY: the table's reference, which `close` handed back.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
    drop(b);
}

#[test]
fn process_spawn_rejects_bad_args_pointer() {
    assert_eq!(sys_process_spawn(USER_VIRT_END), Err(KError::FaultFromUser));
    assert_eq!(dispatch(SYS_PROCESS_SPAWN, USER_VIRT_END, 0, 0, 0, 0, 0), KError::FaultFromUser.as_isize(),);
}

#[test]
fn thread_create_rejects_bad_args_pointer() {
    assert_eq!(sys_thread_create(USER_VIRT_END), Err(KError::FaultFromUser));
    assert_eq!(dispatch(SYS_THREAD_CREATE, USER_VIRT_END, 0, 0, 0, 0, 0), KError::FaultFromUser.as_isize(),);
}

#[test]
fn thread_get_registers_rejects_bad_out_pointer() {
    assert_eq!(sys_thread_get_registers(0, USER_VIRT_END), Err(KError::FaultFromUser),);
    assert_eq!(dispatch(SYS_THREAD_GET_REGISTERS, 0, USER_VIRT_END, 0, 0, 0, 0), KError::FaultFromUser.as_isize(),);
}

#[test]
fn exception_resume_rejects_unknown_disposition() {
    assert_eq!(sys_exception_resume(0, 1, 0), Err(KError::Unsupported));
    assert_eq!(sys_exception_resume(0, 9, 0), Err(KError::Unsupported));
    assert_eq!(dispatch(SYS_EXCEPTION_RESUME, 0, 5, 0, 0, 0, 0), KError::Unsupported.as_isize(),);
}

#[test]
fn kprint_zero_len_is_ok_without_touching_memory() {
    assert_eq!(dispatch(SYS_DEBUG_KPRINT, 0xDEAD_BEEF, 0, 0, 0, 0, 0), 0);
    assert_eq!(sys_kprint(0xDEAD_BEEF, 0), Ok(0));
}

#[test]
fn kprint_oversize_is_too_large_without_touching_memory() {
    let too_big = (KPRINT_MAX + 1) as u64;
    assert_eq!(dispatch(SYS_DEBUG_KPRINT, 0xDEAD_BEEF, too_big, 0, 0, 0, 0), KError::TooLarge.as_isize(),);
    assert_eq!(sys_kprint(0xDEAD_BEEF, KPRINT_MAX + 1), Err(KError::TooLarge));
}

fn fresh_table() -> HandleTable {
    init_global_heap();
    HandleTable::try_new(0x0123_4567_89AB_CDEF).unwrap()
}

fn mk_process(pid: u32) -> *mut () {
    KBox::into_raw(Process::try_new(pid).unwrap()).as_ptr() as *mut ()
}

fn full() -> Rights {
    Rights::DUPLICATE | Rights::INSPECT | Rights::SIGNAL | Rights::TERMINATE
}

fn cont(suffix: &[u8], consumed: usize, base: &[u8]) -> Result<std::vec::Vec<u8>, KError> {
    let mut out = [0u8; NS_PATH_MAX];
    continued_path(suffix, consumed, base, &mut out).map(|p| p.to_vec())
}

#[test]
fn a_continued_path_joins_the_rest_of_the_suffix_onto_the_base() {
    let s = b"fs/nitrox-root/home/alice/notes.txt";
    assert_eq!(cont(s, 14, b"/").unwrap(), b"/home/alice/notes.txt");
    assert_eq!(cont(s, 14, b"/mnt/x").unwrap(), b"/mnt/x/home/alice/notes.txt");
    assert_eq!(cont(b"fs/nitrox-root", 14, b"/").unwrap(), b"/", "the label itself is the root");
    assert_eq!(cont(b"fs/nitrox-root", 14, b"/mnt").unwrap(), b"/mnt");
    assert_eq!(cont(b"a/b", 0, b"/").unwrap(), b"/a/b", "nothing consumed: all of it continues");
    assert_eq!(cont(b"a/b", 0, b"/m").unwrap(), b"/m/a/b");
    assert_eq!(cont(b"", 0, b"/m").unwrap(), b"/m", "a mount's own root");
}

#[test]
fn a_continued_path_refuses_what_a_server_got_wrong() {
    let s = b"fs/nitrox-rootx/y";
    assert_eq!(cont(s, 14, b"/"), Err(KError::InvalidArgument), "mid-component");
    assert_eq!(cont(b"fs/a", 5, b"/"), Err(KError::InvalidArgument), "past the suffix");
    assert_eq!(cont(b"fs/a/b", 4, b"home"), Err(KError::InvalidArgument), "a relative base");
    assert_eq!(cont(b"fs/a/b", 4, b"/m/"), Err(KError::InvalidArgument), "a trailing slash");
    assert_eq!(cont(b"fs/a/b", 4, b"/m/../n"), Err(KError::InvalidArgument), "a dot-dot base");
    assert_eq!(cont(b"fs/a/b", 4, b""), Err(KError::InvalidArgument), "an empty base is not the root");
    let mut small = [0u8; 8];
    assert_eq!(continued_path(b"fs/a/bcdefgh", 4, b"/m", &mut small), Err(KError::TooLarge));
    let mut exact = [0u8; 12];
    assert_eq!(continued_path(b"fs/a/bcdefgh", 4, b"/m", &mut exact), Ok(&b"/m/bcdefgh"[..]));
    let mut one_short = [0u8; 9];
    assert_eq!(continued_path(b"fs/a/bcdefgh", 4, b"/m", &mut one_short), Err(KError::TooLarge));
}

#[test]
fn a_rename_continues_its_destination_only_within_the_prefix() {
    let dest = |d: &[u8]| {
        let mut out = [0u8; NS_PATH_MAX];
        continued_dest(b"fs/nitrox-root/a", d, 14, b"/", &mut out).map(|p| p.to_vec())
    };
    assert_eq!(dest(b"fs/nitrox-root/dir/b").unwrap(), b"/dir/b");
    assert_eq!(dest(b"fs/nitrox-root").unwrap(), b"/", "onto the mount's root");
    assert_eq!(dest(b"fs/usb-stick/b"), Err(KError::Unsupported), "another mount");
    assert_eq!(dest(b"fs/nitrox-rootx/b"), Err(KError::Unsupported), "a label that starts the same");
    assert_eq!(dest(b"fs/nitrox"), Err(KError::Unsupported), "shorter than the prefix");
    let mut out = [0u8; NS_PATH_MAX];
    assert_eq!(continued_dest(b"a", b"b/c", 0, b"/m", &mut out), Ok(&b"/m/b/c"[..]), "nothing consumed");
}

#[test]
fn map_handle_err_collapses_notowner_to_invalid() {
    assert_eq!(map_handle_err(HandleError::NullHandle), KError::InvalidHandle);
    assert_eq!(map_handle_err(HandleError::InvalidHandle), KError::InvalidHandle);
    assert_eq!(map_handle_err(HandleError::NotOwner), KError::InvalidHandle);
    assert_eq!(map_handle_err(HandleError::NoAccess), KError::NoAccess);
    assert_eq!(map_handle_err(HandleError::OutOfHandles), KError::OutOfHandles);
    assert_eq!(map_handle_err(HandleError::OutOfMemory), KError::OutOfMemory);
    assert_eq!(map_handle_err(HandleError::BadRights), KError::InvalidArgument);
}

#[test]
fn close_on_success_invalidates_handle() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, full()).unwrap();
    assert_eq!(close_on(&t, h, 1), Ok(0));
    assert_eq!(t.lookup(h, 1, Rights::empty()).unwrap_err(), HandleError::InvalidHandle,);
}

#[test]
fn close_on_wrong_owner_is_invalid_handle() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, full()).unwrap();
    assert_eq!(close_on(&t, h, 2), Err(KError::InvalidHandle));
    assert_eq!(close_on(&t, h, 1), Ok(0));
}

#[test]
fn close_on_null_handle_is_invalid() {
    let t = fresh_table();
    assert_eq!(close_on(&t, RawHandle::NULL, 1), Err(KError::InvalidHandle));
}

#[test]
fn duplicate_on_returns_new_handle_with_intersected_rights() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, full()).unwrap();
    let ret = duplicate_on(&t, h, 1, Rights::DUPLICATE | Rights::INSPECT).unwrap();
    assert!(ret >= 0, "a valid handle value is non-negative");
    let dup = RawHandle(ret as u64);
    assert_ne!(dup, h);
    let info = stat_on(&t, dup, 1).unwrap();
    assert_eq!(info.rights, (Rights::DUPLICATE | Rights::INSPECT).bits());
    close_on(&t, dup, 1).unwrap();
    close_on(&t, h, 1).unwrap();
}

#[test]
fn duplicate_on_without_duplicate_right_is_no_access() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, Rights::INSPECT | Rights::SIGNAL).unwrap();
    assert_eq!(duplicate_on(&t, h, 1, Rights::INSPECT), Err(KError::NoAccess));
    close_on(&t, h, 1).unwrap();
}

#[test]
fn restrict_on_attenuates_in_place() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, full()).unwrap();
    assert_eq!(restrict_on(&t, h, 1, Rights::INSPECT), Ok(0));
    let info = stat_on(&t, h, 1).unwrap();
    assert_eq!(info.rights, Rights::INSPECT.bits());
    close_on(&t, h, 1).unwrap();
}

#[test]
fn restrict_on_cannot_amplify() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, Rights::INSPECT).unwrap();
    assert_eq!(restrict_on(&t, h, 1, Rights::SIGNAL | Rights::INSPECT), Ok(0));
    let info = stat_on(&t, h, 1).unwrap();
    assert_eq!(info.rights, Rights::INSPECT.bits());
    close_on(&t, h, 1).unwrap();
}

#[test]
fn stat_on_reports_type_rights_generation() {
    let t = fresh_table();
    let rights = Rights::INSPECT | Rights::SIGNAL;
    let h = t.allocate(1, mk_process(1), KObjectType::Process, rights).unwrap();
    let info = stat_on(&t, h, 1).unwrap();
    assert_eq!(info.object_type, KObjectType::Process as u32);
    assert_eq!(info.rights, rights.bits());
    let (_, _, generation) = h.decode();
    assert_eq!(info.generation, generation);
    close_on(&t, h, 1).unwrap();
}

#[test]
fn stat_on_without_inspect_is_no_access() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, Rights::SIGNAL).unwrap();
    assert_eq!(stat_on(&t, h, 1), Err(KError::NoAccess));
    close_on(&t, h, 1).unwrap();
}

#[test]
fn stat_on_wrong_owner_is_invalid_handle() {
    let t = fresh_table();
    let h = t.allocate(1, mk_process(1), KObjectType::Process, Rights::INSPECT).unwrap();
    assert_eq!(stat_on(&t, h, 2), Err(KError::InvalidHandle));
    close_on(&t, h, 1).unwrap();
}

#[test]
fn round_up_page_rounds_and_saturates() {
    assert_eq!(round_up_page(0), 0);
    assert_eq!(round_up_page(1), PAGE_SIZE as u64);
    assert_eq!(round_up_page(PAGE_SIZE as u64), PAGE_SIZE as u64);
    assert_eq!(round_up_page(PAGE_SIZE as u64 + 1), 2 * PAGE_SIZE as u64);
    let big = round_up_page(u64::MAX);
    assert_eq!(big & (PAGE_SIZE as u64 - 1), 0);
}

#[test]
fn map_mem_map_err_mapping() {
    assert_eq!(map_mem_map_err(MapError::Overlap), KError::InvalidArgument);
    assert_eq!(map_mem_map_err(MapError::NotCanonical), KError::InvalidArgument);
    assert_eq!(map_mem_map_err(MapError::NotUserHalf), KError::InvalidArgument);
    assert_eq!(map_mem_map_err(MapError::OutOfMemory), KError::OutOfMemory);
}

#[test]
fn full_mem_rights_are_allocatable_on_a_memory_object() {
    init_global_heap();
    let t = HandleTable::try_new(0xFEED_FACE_CAFE_BEEF).unwrap();
    let m = crate::object::MemoryObject::try_new(PAGE_SIZE).unwrap();
    let ptr = KBox::into_raw(m).as_ptr() as *mut ();
    let h = t
        .allocate(1, ptr, KObjectType::MemoryObject, full_mem_rights())
        .expect("full_mem_rights must be valid for a MemoryObject");
    let co = t.close(h, 1).unwrap();
    // SAFETY: the table's reference, which `close` handed back.
    drop(unsafe { ObjectRef::from_raw(co.0, co.1) });
}

/// A fresh kernel object's reference; each caller names the type it boxed.
fn adopt<T>(b: KBox<T>, t: KObjectType) -> ObjectRef {
    // SAFETY: `into_raw` yields the box's single creation reference, of an object of type `t`.
    unsafe { ObjectRef::from_raw(KBox::into_raw(b).as_ptr() as *mut (), t) }
}

/// A file server's registration, its peer endpoint dropped.
fn file_server_reg() -> ObjectRef {
    use crate::object::{IpcChannel, UserspaceServerReg};
    let (a, b) = IpcChannel::try_new_pair(4).unwrap();
    drop(adopt(b, KObjectType::IpcChannel));
    let ep = adopt(a, KObjectType::IpcChannel);
    adopt(UserspaceServerReg::try_new(ep).unwrap(), KObjectType::UserspaceServerReg)
}

fn block_device() -> ObjectRef {
    use crate::object::DeviceNode;
    use crate::object::device_node::{BlockGeometry, DeviceClass, ResourceDescriptor};
    let dev = DeviceNode::try_new(DeviceClass::Other, ResourceDescriptor::ZERO, BlockGeometry::ZERO).unwrap();
    adopt(dev, KObjectType::DeviceNode)
}

const FILE_CLIENT: u32 = 4242;

/// `reg`'s server answers a lookup asking `MAP_READ | MAP_WRITE` with a file-blocks reply for
/// `file_id` (`0` uncached): 512-byte blocks, one run of a page, `device` sent with `rights`.
fn file_blocks_reply(reg: &ObjectRef, file_id: u64, device: ObjectRef, rights: Rights) -> (i32, u64) {
    use crate::object::PendingOperation;
    use crate::object::userspace_server::{ForwardOp, LOOKUP_SUFFIX_MAX, PendingLookup};
    let mut msg = std::vec![0u8; 28 + 32 + 24];
    msg[20..24].copy_from_slice(&56u32.to_le_bytes());
    msg[28 + 8..28 + 12].copy_from_slice(&512u32.to_le_bytes());
    msg[28 + 12..28 + 16].copy_from_slice(&1u32.to_le_bytes());
    msg[28 + 16..28 + 24].copy_from_slice(&file_id.to_le_bytes());
    msg[28 + 32 + 8..28 + 32 + 16].copy_from_slice(&100u64.to_le_bytes());
    msg[28 + 32 + 16..28 + 32 + 20].copy_from_slice(&8u32.to_le_bytes());
    let po = adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
    let pl = PendingLookup {
        request_id: 1,
        po,
        requested: Rights::MAP_READ | Rights::MAP_WRITE,
        suffix: [0; LOOKUP_SUFFIX_MAX],
        suffix_len: 0,
        op: ForwardOp::Plain,
        dest: [0; LOOKUP_SUFFIX_MAX],
        dest_len: 0,
        depth: 0,
    };
    let mut transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    transfers[0] = Some(TransferRef { obj: device, rights });
    let (status, parked) = build_and_install_file_blocks(reg, &pl, 4096, &msg, &mut transfers);
    // SAFETY: `pl.po` pins the lookup's `PendingOperation`.
    drop(unsafe { crate::sched::complete_lookup(pl.po.as_ptr(), status, parked) });
    read_completion(&pl.po, FILE_CLIENT)
}

/// A file-blocks reply's file is as writable as its server's hold on the device it sent: the
/// kernel writes the file back to that device, and the server may not have been let write it.
#[test]
fn a_file_blocks_reply_installs_a_file_no_more_writable_than_its_server_holds_its_device() {
    init_global_heap();
    global::init_for_tests();
    let reg = file_server_reg();
    let rw = Rights::READ | Rights::WRITE | Rights::TRANSFER;
    for (device, writable) in [(rw, Some(true)), (Rights::READ | Rights::TRANSFER, Some(false)), (Rights::WRITE, None)]
    {
        let (status, h) = file_blocks_reply(&reg, 0, block_device(), device);
        match writable {
            None => assert_eq!(status, KError::NoAccess as i32, "a server without READ on its device"),
            Some(w) => {
                assert_eq!(status, 0);
                let write = global::get().lookup(RawHandle(h), FILE_CLIENT, Rights::MAP_WRITE).is_ok();
                assert_eq!(write, w, "device rights {:?}", device);
                close_and_release(RawHandle(h), FILE_CLIENT);
            }
        }
    }
}

/// **A cached file keeps the device it was made with**, and is written back to it, so a later
/// reply for the file naming another device is refused: its rights would decide `MAP_WRITE` for
/// writes that go to the first. The same device sent with `WRITE` is its server's to write.
#[test]
fn a_reply_for_a_cached_file_naming_another_device_is_refused() {
    init_global_heap();
    global::init_for_tests();
    let reg = file_server_reg();
    let first = block_device();
    let writable = |h: u64| global::get().lookup(RawHandle(h), FILE_CLIENT, Rights::MAP_WRITE).is_ok();

    let (status, a) = file_blocks_reply(&reg, 7, first.clone(), Rights::READ | Rights::TRANSFER);
    assert_eq!(status, 0);
    assert!(!writable(a));
    let (status, _) = file_blocks_reply(&reg, 7, block_device(), Rights::READ | Rights::WRITE | Rights::TRANSFER);
    assert_eq!(status, KError::KernelError as i32, "another device for a cached file");
    let (status, b) = file_blocks_reply(&reg, 7, first, Rights::READ | Rights::WRITE | Rights::TRANSFER);
    assert_eq!(status, 0, "its own device again");
    assert!(writable(b));
    close_and_release(RawHandle(a), FILE_CLIENT);
    close_and_release(RawHandle(b), FILE_CLIENT);
}

/// A forwarded lookup, and a reply carrying a memory object built for it.
struct Forwarded {
    reg: ObjectRef,
    po: ObjectRef,
    reply: crate::object::ipc_channel::StoredMsg,
    transfers: [Option<TransferRef>; IPC_HANDLE_MAX],
    _server_end: ObjectRef,
}

fn forwarded_memobj_lookup() -> Forwarded {
    use crate::object::ipc_channel::StoredMsg;
    use crate::object::userspace_server::Forward;
    use crate::object::{IpcChannel, MemoryObject, PendingOperation, UserspaceServerReg};
    use crate::rsproto::{OBJECT_KIND_MEMOBJ, build_resolve_request};
    let (k, s) = IpcChannel::try_new_pair(4).unwrap();
    let (k, _server_end) = (adopt(k, KObjectType::IpcChannel), adopt(s, KObjectType::IpcChannel));
    let reg = adopt(UserspaceServerReg::try_new(k).unwrap(), KObjectType::UserspaceServerReg);
    let po = adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
    let mut msg = StoredMsg::zeroed();
    msg.header.payload_len = build_resolve_request(&mut msg.payload, Rights::MAP_READ.bits(), b"f").unwrap() as u32;
    // SAFETY: `reg` and `po` are pinned by the test.
    let forwarded = unsafe {
        crate::sched::us_forward_originate(reg.as_ptr(), &mut msg, &po, Rights::MAP_READ, b"f", Forward::PLAIN)
    };
    assert_eq!(forwarded, crate::sched::ForwardOutcome::Pending);
    // The request's envelope turned round, as a server's reply: flags REPLY, a MEMOBJ body.
    let mut reply = StoredMsg::zeroed();
    reply.payload[..28].copy_from_slice(&msg.payload[..28]);
    reply.payload[16..20].copy_from_slice(&1u32.to_le_bytes());
    reply.payload[20..24].copy_from_slice(&8u32.to_le_bytes());
    reply.payload[28..30].copy_from_slice(&OBJECT_KIND_MEMOBJ.to_le_bytes());
    reply.payload[32..36].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    let mem = adopt(MemoryObject::try_new(PAGE_SIZE).unwrap(), KObjectType::MemoryObject);
    let mut transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    transfers[0] = Some(TransferRef { obj: mem, rights: Rights::MAP_READ | Rights::TRANSFER });
    Forwarded { reg, po, reply, transfers, _server_end }
}

/// **A lookup closed before its reply leaves nothing behind**: no handle that nothing knows of in
/// the client's table, and the object, freed with the PO, not pinned for the life of the client.
#[test]
fn a_reply_to_a_lookup_its_client_closed_makes_no_handle() {
    // A pid of its own: the global table is every test's.
    const PID: u32 = 4451;
    use crate::object::header::test_probe;
    init_global_heap();
    global::init_for_tests();
    test_probe::reset();
    let mut f = forwarded_memobj_lookup();
    drop(f.po);
    assert_eq!(complete_resolve_reply(&f.reg, &f.reply, &mut f.transfers, 36), Ok(0));
    assert_eq!(test_probe::memory_object_destroys(), 1, "freed with the PO");
    assert_eq!(global::close_all_owned_by(PID), 0, "no handle in the client's table");
}

/// A lookup's handle is made by its first read, into the reader's table, and is every read's after.
#[test]
fn a_lookup_reply_is_installed_when_its_completion_is_read() {
    const PID: u32 = 4452;
    init_global_heap();
    global::init_for_tests();
    let mut f = forwarded_memobj_lookup();
    assert_eq!(complete_resolve_reply(&f.reg, &f.reply, &mut f.transfers, 36), Ok(0));
    let po = &f.po;
    assert_eq!(global::close_all_owned_by(PID), 0, "nothing installed before the read");
    let (status, h) = read_completion(po, PID);
    assert_eq!(status, 0);
    assert!(global::get().lookup(RawHandle(h), PID, Rights::MAP_READ).is_ok());
    assert_eq!(read_completion(po, PID), (0, h), "one handle, however often it is read");
    assert_eq!(global::close_all_owned_by(PID), 1);
}

/// A second reader while the first installs waits for its handle, rather than finding none.
#[test]
fn a_lookup_read_while_another_installs_it_waits_for_that_handle() {
    use crate::sched::{Completion, complete_lookup, pending_op_installed, pending_op_take};
    init_global_heap();
    let po = adopt(crate::object::PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
    let mem = adopt(crate::object::MemoryObject::try_new(PAGE_SIZE).unwrap(), KObjectType::MemoryObject);
    // SAFETY: `po` pins the `PendingOperation`, and the test is single-threaded.
    unsafe {
        assert!(complete_lookup(po.as_ptr(), 0, Some((mem, Rights::MAP_READ))).is_none());
        let Completion::Install(obj, _) = pending_op_take(po.as_ptr()) else { panic!("its object") };
        assert!(matches!(pending_op_take(po.as_ptr()), Completion::Installing));
        drop(obj);
        pending_op_installed(po.as_ptr(), Ok(7));
        assert!(matches!(pending_op_take(po.as_ptr()), Completion::Read(0, 7)));
    }
}

/// A lookup whose reader's table cannot take its handle fails for good, its object released.
#[test]
fn a_lookup_read_into_a_full_table_fails_for_good() {
    const PID: u32 = 4453;
    use crate::object::header::test_probe;
    init_global_heap();
    global::init_for_tests();
    test_probe::reset();
    let po = adopt(crate::object::PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
    let mem = adopt(crate::object::MemoryObject::try_new(PAGE_SIZE).unwrap(), KObjectType::MemoryObject);
    // SAFETY: `po` pins the `PendingOperation`.
    assert!(unsafe { crate::sched::complete_lookup(po.as_ptr(), 0, Some((mem, Rights::MAP_READ))) }.is_none());
    crate::handle::table::FAIL_NEXT_ALLOCATE.with(|f| f.set(true));
    let failed = (map_handle_err(HandleError::OutOfMemory) as i32, 0);
    assert_eq!(read_completion(&po, PID), failed);
    assert_eq!(test_probe::memory_object_destroys(), 1, "released");
    assert_eq!(read_completion(&po, PID), failed, "the same error, every read");
    assert_eq!(global::close_all_owned_by(PID), 0);
}

/// A PO completed once keeps its first object; a second completion's comes back to its caller.
#[test]
fn a_lookup_completed_twice_keeps_its_first_object() {
    const PID: u32 = 4454;
    init_global_heap();
    global::init_for_tests();
    let po = adopt(crate::object::PendingOperation::try_new().unwrap(), KObjectType::PendingOperation);
    let mem = || adopt(crate::object::MemoryObject::try_new(PAGE_SIZE).unwrap(), KObjectType::MemoryObject);
    let (first, second) = (mem(), mem());
    let first_at = first.as_ptr();
    // SAFETY: `po` pins the `PendingOperation`.
    unsafe {
        assert!(crate::sched::complete_lookup(po.as_ptr(), 0, Some((first, Rights::MAP_READ))).is_none());
        let back = crate::sched::complete_lookup(po.as_ptr(), 0, Some((second, Rights::MAP_READ)));
        assert!(back.is_some_and(|(o, _)| o.as_ptr() != first_at), "the second's own object");
    }
    let (status, h) = read_completion(&po, PID);
    assert_eq!(status, 0);
    let read = global::get().lookup(RawHandle(h), PID, Rights::MAP_READ).unwrap();
    assert_eq!(read.object.as_ptr(), first_at);
    drop(read);
    assert_eq!(global::close_all_owned_by(PID), 1);
}

/// The wire carries a size in 32 bits and rename flags in 16: one past either is refused, before
/// anything is looked up, never cut to fit.
#[test]
fn a_file_size_or_rename_flags_past_what_the_wire_carries_are_refused() {
    let size = |n: u64| ResolveOp::Size(n, SizeChange::Truncate).forward_op();
    assert!(matches!(size(u32::MAX as u64), Ok(ForwardOp::Size(u32::MAX, _))));
    assert_eq!(size(1 << 32).err(), Some(KError::TooLarge));
    let rename = |f: u64| ResolveOp::Rename { dest_ptr: 0, dest_len: 0, flags: f }.forward_op();
    assert!(matches!(rename(u16::MAX as u64), Ok(ForwardOp::Rename { flags: u16::MAX })));
    assert_eq!(rename(1 << 16).err(), Some(KError::InvalidArgument));
    let path = b"/f";
    let just_over_4_gib = 0x1_0000_000A;
    let truncate = dispatch(SYS_FILE_TRUNCATE, 0xDEAD, path.as_ptr() as u64, 2, 0, just_over_4_gib, 0);
    assert_eq!(truncate, KError::TooLarge.as_isize(), "taken as 10 bytes, it would free the rest");
}

/// **A fill's reply readies its own page**, not one a resize let a fault reserve its index for.
#[test]
fn a_stale_fill_reply_leaves_its_indexs_next_page_loading() {
    use crate::libkern::KVec;
    use crate::object::file_object::Fault;
    use crate::object::ipc_channel::StoredMsg;
    use crate::object::{FileObject, IpcChannel, MemoryObject, PageState, Producer, UserspaceServerReg};
    use crate::rsproto::{REQUEST_ID_OFFSET, build_read_range_request};
    init_global_heap();
    // SAFETY: each pointer is a fresh `into_raw`, carrying its single creation reference.
    let adopt = |p: *mut (), t| unsafe { ObjectRef::from_raw(p, t) };
    let (k, s) = IpcChannel::try_new_pair(4).unwrap();
    let k = adopt(KBox::into_raw(k).as_ptr() as *mut (), KObjectType::IpcChannel);
    let _server_end = adopt(KBox::into_raw(s).as_ptr() as *mut (), KObjectType::IpcChannel);
    let reg = adopt(
        KBox::into_raw(UserspaceServerReg::try_new(k).unwrap()).as_ptr() as *mut (),
        KObjectType::UserspaceServerReg,
    );
    let file = FileObject::try_new(PAGE_SIZE, Producer::Stub { base: 0 }).unwrap();
    let file = adopt(KBox::into_raw(file).as_ptr() as *mut (), KObjectType::FileObject);
    // SAFETY: `file` pins the `FileObject`.
    let fo = unsafe { &*(file.as_ptr() as *const FileObject) };
    let Fault::Fill(frame, stale) = fo.reserve_fault(0) else { panic!("a fill") };
    // SAFETY: `reg` pins the registration, and the test is single-threaded.
    let id = unsafe { UserspaceServerReg::begin_fill(reg.as_ptr(), &stale, &file, frame, 0) }.unwrap();
    fo.resize(0, KVec::new()).unwrap();
    fo.resize(PAGE_SIZE, KVec::new()).unwrap();
    let Fault::Fill(next, _its_fill) = fo.reserve_fault(0) else { panic!("a fill afresh") };
    // The request's envelope turned round, as a server's reply: flags REPLY, the page's length.
    let mut reply = StoredMsg::zeroed();
    build_read_range_request(&mut reply.payload, 0, PAGE_SIZE as u32, b"f").unwrap();
    reply.payload[REQUEST_ID_OFFSET..REQUEST_ID_OFFSET + 8].copy_from_slice(&id.to_le_bytes());
    reply.payload[16..20].copy_from_slice(&1u32.to_le_bytes());
    reply.payload[20..24].copy_from_slice(&8u32.to_le_bytes());
    reply.payload[28..32].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    let bytes =
        adopt(KBox::into_raw(MemoryObject::try_new(PAGE_SIZE).unwrap()).as_ptr() as *mut (), KObjectType::MemoryObject);
    let mut transfers: [Option<TransferRef>; IPC_HANDLE_MAX] = core::array::from_fn(|_| None);
    transfers[0] = Some(TransferRef { obj: bytes, rights: Rights::MAP_READ | Rights::TRANSFER });
    assert_eq!(complete_read_range_reply(&reg, &reply, &mut transfers, 36), Ok(0));
    assert_eq!(fo.lookup(0), Some((next, PageState::Loading)));
}

/// **A borrowed memory object is refused wherever the kernel reaches its frames itself.**
#[test]
fn a_borrowed_memory_object_is_refused_where_the_kernel_reaches_its_frames() {
    use crate::mm::{Caching, PAGE_SIZE, PhysAddr, heap};
    use crate::object::{MemoryObject, PendingOperation};
    init_global_heap();
    let adopt = |mo| {
        // SAFETY: a fresh `MemoryObject`, under its own tag.
        unsafe { crate::drivers::adopt(mo, KObjectType::MemoryObject) }
    };
    // SAFETY: a range the test heap never holds; nothing maps it, and a read of it would fault.
    let aperture = unsafe { MemoryObject::try_new_borrowed(PhysAddr::new(0xF000_0000), PAGE_SIZE, Caching::Normal) };
    let aperture = adopt(aperture.unwrap());
    let owned = adopt(MemoryObject::try_new_filled(b"\x7fELF").unwrap());
    let cache = adopt(MemoryObject::try_new(PAGE_SIZE).unwrap());
    // SAFETY: `cache` pins the live `MemoryObject`.
    let frame = unsafe { &*(cache.as_ptr() as *const MemoryObject) }.frames()[0];
    // SAFETY: a fresh `PendingOperation`, under its own tag.
    let po = unsafe { crate::drivers::adopt(PendingOperation::try_new().unwrap(), KObjectType::PendingOperation) };

    assert_eq!(spawn::image_bytes(&aperture).err(), Some(KError::InvalidArgument));
    assert_eq!(&spawn::image_bytes(&owned).unwrap()[..4], b"\x7fELF");
    assert_eq!(submit::io_buffer_size(&aperture), Err(KError::InvalidArgument));
    assert_eq!(submit::io_buffer_size(&owned), Ok(PAGE_SIZE as u64));
    assert_eq!(fill::copy_fill_into_frame(&aperture, frame, 4), Err(KError::Unsupported));
    assert_eq!(fill::copy_fill_into_frame(&po, frame, 4), Err(KError::Unsupported));
    assert_eq!(fill::copy_fill_into_frame(&owned, frame, 4), Ok(()));
    // SAFETY: the cache frame is the test heap's, reachable through its HHDM offset.
    let filled = unsafe { core::slice::from_raw_parts((frame.as_u64() + heap::hhdm_offset()) as *const u8, 4) };
    assert_eq!(filled, b"\x7fELF");
}
