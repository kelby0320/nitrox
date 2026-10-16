use super::*;

// === the power op, from a process without the object (administration Part E.3) ===================

/// **`sys_power` from anything but `init` is refused**, and the machine goes on: with no handle,
/// with a namespace this process may only look up in, and with a disk it may write — a `WRITE`
/// handle, so it is the object's type that refuses it, not a missing right.
///
/// The halt it would otherwise be is what makes a wrong answer loud: the boot stops before the
/// verdict, and `test-qemu` times out.
pub(super) fn power_refused_test(root_ns: u64) -> bool {
    use libkern::{KError, POWER_HALT, RIGHT_WRITE, SYS_POWER};
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: power FAIL: ").s(why).end();
        false
    };
    // SAFETY: register-only syscall; refused before anything happens, which is the point.
    let power = |h: u64| unsafe { syscall2(SYS_POWER, h, POWER_HALT) };
    if power(0) != KError::InvalidHandle.as_i32() as i64 {
        return fail(b"no handle was not refused InvalidHandle");
    }
    if power(root_ns) != KError::NoAccess.as_i32() as i64 {
        return fail(b"a lookup-only namespace was not refused NoAccess");
    }
    // The root partition, as `flush_test` finds it: the installed disk's, or the live stick's.
    let labels: [&[u8]; 2] = [b"nitrox-root", b"nitrox-live"];
    let root = |records: alloc::vec::Vec<libkern::device::DeviceRecord>| {
        records.iter().find(|r| labels.contains(&r.name())).and_then(|r| r.block_index())
    };
    let Some(index) = registry_records(root_ns).and_then(root) else {
        return fail(b"no root partition in the registry");
    };
    let path = alloc::format!("/dev/blk/{index}");
    let (st, dev) = ns_lookup(root_ns, path.as_bytes(), RIGHT_READ | RIGHT_WRITE);
    if st != 0 || dev == 0 {
        return fail(b"the root partition does not open read-write");
    }
    let refused = power(dev);
    close(dev);
    if refused != KError::InvalidArgument.as_i32() as i64 {
        return fail(b"a writable disk was not refused InvalidArgument");
    }
    kprint(b"boot-probe: sys_power refused no handle, a namespace and a writable disk ok\n");
    true
}

// === setting the clock, without SYSTEM_CLOCK (administration Part E.5) ===========================

/// **`sys_clock_set` is refused without `SYSTEM_CLOCK`**, which this process does not hold, and
/// the clock does not move: a set to 2031 refused `NoAccess`, and the clock read either side of it
/// within a minute of itself. Before the authority, the arguments: `Monotonic` never steps, and
/// 1999 is before what the hardware clock holds — both `InvalidArgument`.
pub(super) fn clock_set_refused_test() -> bool {
    use libkern::{CLOCK_MONOTONIC, CLOCK_REALTIME, KError, SYS_CLOCK_READ, SYS_CLOCK_SET};
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: clock FAIL: ").s(why).end();
        false
    };
    let read = || {
        let mut ns = 0u64;
        // SAFETY: a valid `u64` out-param on this stack.
        let r = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut ns) as u64) };
        (r == 0).then_some(ns)
    };
    let set = |clock: u64, secs: u64| {
        // SAFETY: register-only syscall.
        unsafe { syscall2(SYS_CLOCK_SET, clock, secs * 1_000_000_000) }
    };
    let in_2031 = 1_925_078_400; // 2031-01-02T00:00:00Z
    if set(CLOCK_MONOTONIC, in_2031) != KError::InvalidArgument.as_i32() as i64 {
        return fail(b"setting the monotonic clock was not refused InvalidArgument");
    }
    if set(CLOCK_REALTIME, 946_684_799) != KError::InvalidArgument.as_i32() as i64 {
        return fail(b"a time in 1999 was not refused InvalidArgument");
    }
    let before = read();
    if set(CLOCK_REALTIME, in_2031) != KError::NoAccess.as_i32() as i64 {
        return fail(b"a set without SYSTEM_CLOCK was not refused NoAccess");
    }
    let (Some(before), Some(after)) = (before, read()) else {
        return fail(b"the wall clock does not read");
    };
    if after < before || after - before > 60_000_000_000 {
        return fail(b"the wall clock moved across a refused set");
    }
    kprint(b"boot-probe: sys_clock_set refused the monotonic clock, 1999 and no SYSTEM_CLOCK ok\n");
    true
}

/// **A direct-handle bind needs `TRANSFER`** (PR #342 review), since every resolve of the binding
/// mints a handle to the object for whoever asked. Without it, `init` could publish its
/// system-control object, whose handle carries no `TRANSFER` so that it cannot leave `init`.
///
/// This process holds no such object, so it makes one of its own shape: a memory object's handle,
/// duplicated without `TRANSFER`, is refused `NoAccess`, and the original, with it, is taken — in
/// the same namespace, so the refusal is the right's and not the bind's.
pub(super) fn bind_needs_transfer_test() -> bool {
    use libkern::{KError, RIGHT_INSPECT, RIGHT_MAP_READ, SYS_NS_BIND, SYS_NS_CREATE};
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: bind FAIL: ").s(why).end();
        false
    };
    // SAFETY: register-only syscall; it creates an object and touches no memory of ours.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    // SAFETY: as above.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if ns <= 0 || mem <= 0 {
        return fail(b"no namespace or memory object to bind");
    }
    let (ns, mem) = (ns as u64, mem as u64);
    // SAFETY: register-only syscall on a handle this process holds.
    let kept = unsafe { syscall2(SYS_HANDLE_DUPLICATE, mem, RIGHT_MAP_READ | RIGHT_INSPECT) };
    let bind = |at: &[u8], h: u64| {
        // SAFETY: `at` is a valid path buffer; `ns` and `h` are handles this process holds.
        unsafe { syscall6(SYS_NS_BIND, ns, at.as_ptr() as u64, at.len() as u64, h, 0, 0) }
    };
    let refused = kept > 0 && bind(b"/kept", kept as u64) == KError::NoAccess.as_i32() as i64;
    let taken = bind(b"/given", mem) == 0;
    for h in [ns, mem, kept.max(0) as u64] {
        close(h);
    }
    if !refused {
        return fail(b"a handle without TRANSFER was not refused NoAccess");
    }
    if !taken {
        return fail(b"a handle with TRANSFER was not bound");
    }
    kprint(b"boot-probe: a direct bind needs TRANSFER: refused without it, taken with it ok\n");
    true
}

// === a resolve continued in another namespace (administration Part C.4) ===========================

/// Reply `SUBNAMESPACE` to `request_id` on `ch`: the first `consumed` bytes of the suffix stand for
/// `base` in `ns`, a duplicate of which is transferred (with `LOOKUP`, which the kernel requires).
pub(super) fn reply_subnamespace(ch: u64, request_id: u64, consumed: u16, base: &[u8], ns: u64) -> bool {
    // SAFETY: register-only syscall on a handle this process holds.
    let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, ns, RIGHT_LOOKUP | RIGHT_TRANSFER) };
    if dup < 0 {
        return false;
    }
    let mut body = [0u8; librsproto::namespace::SUBNAMESPACE_PREFIX_LEN + 64];
    let mut msg = [0u8; 4096];
    let encoded = librsproto::namespace::subnamespace_reply(&mut body, consumed, base).and_then(|n| {
        librsproto::encode(
            &mut msg[24..],
            librsproto::OP_NS_RESOLVE,
            request_id,
            librsproto::RS_FLAG_REPLY,
            &body[..n],
            1,
        )
    });
    let Some(len) = encoded else {
        close(dup as u64);
        return false;
    };
    msg[4..8].copy_from_slice(&(len as u32).to_le_bytes());
    msg[8] = 1;
    let handles = [dup as u64];
    // SAFETY: valid message buffer and one transferred handle.
    let sr = unsafe {
        syscall5(
            libkern::SYS_CHANNEL_SEND,
            ch,
            msg.as_ptr() as u64,
            handles.as_ptr() as u64,
            1,
            libkern::SENDMODE_NOBLOCK,
        )
    };
    sr == 0
}

/// auth+session Part B milestone (selftest): prove **subtree-scoped namespace
/// binding** end to end. `mount_one` bound the fs endpoint a second time at
/// `/subtreetest` scoped to base `/system` (sharing the server's registration), so a
/// lookup of `/subtreetest/current-generation` must forward `system/current-generation`
/// to the server and resolve to the *same* file as `/system/current-generation`. Read
/// the leading bytes of both and confirm they match — the kernel prepended the base to
/// the forwarded suffix, and the shared registration routed both replies correctly.
pub(super) fn subtree_bind_test(root_ns: u64) -> bool {
    // Resolve + map the first page of `path` read-only; returns its address or 0.
    fn map_first_page(root_ns: u64, path: &[u8]) -> u64 {
        let (st, fh) = ns_lookup(root_ns, path, RIGHT_MAP_READ);
        if st != 0 || fh == 0 {
            return 0;
        }
        // SAFETY: mapping a file handle we hold, read-only; the result is checked below
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, PAGE, RIGHT_MAP_READ) };
        // The mapping pins its own reference to the object; close the handle.
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        if addr < 0 { 0 } else { addr as u64 }
    }

    let direct = map_first_page(root_ns, b"/system/current-generation");
    let via_sub = map_first_page(root_ns, b"/subtreetest/current-generation");
    if direct == 0 || via_sub == 0 {
        kprint(b"boot-probe: subtree resolve FAIL\n");
        return false;
    }
    // Compare the leading bytes (the file is a short text line; the page tail is
    // zero-padded, so the head suffices).
    let mut same = true;
    for i in 0..64u64 {
        // SAFETY: both addresses map a full page; `i < 64 < PAGE`.
        let a = unsafe { ((direct + i) as *const u8).read_volatile() };
        // SAFETY: `via_sub` maps a full page and `i < 64 < PAGE`
        let b = unsafe { ((via_sub + i) as *const u8).read_volatile() };
        if a != b {
            same = false;
            break;
        }
    }
    // SAFETY: unmap our two mappings. `init` ran this and never exits, so this said
    // "init runs forever — don't leak"; `boot-probe` exits a few lines below and the address
    // space goes with it either way. Still right to tidy before the checks that follow — and
    // a file mapping released inside `AddressSpace::drop` is exactly what exposed the
    // lock-order violation this move found.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, direct, PAGE);
        syscall2(SYS_MEMORY_UNMAP, via_sub, PAGE);
    }
    if same {
        kprint(b"boot-probe: subtree bind (/subtreetest -> /system) resolves + matches ok\n");
        true
    } else {
        kprint(b"boot-probe: subtree bind MISMATCH\n");
        false
    }
}

/// Prove `auth-service` serves **more than one client** — which is what M7 Part C claims and
/// what nothing else here would check.
///
/// `session-mgr` resolves `/svc/auth` at startup and holds its session for the machine's life,
/// so by the time this runs there is already one client. Resolving a second is the whole
/// assertion: before Part C `auth-service` minted one channel pair at startup and handed the
/// only client end to `session-mgr`, so this could not have succeeded however it was asked.
///
/// **Without this the claim rides on `desktop-session-mgr`, which does not exist until Part
/// D.** A capability specified, reasoned about, and reachable by nobody is exactly what the
/// title cap in PR #233 turned out to be, and the fix there was to test the path rather than
/// the component. This is that, one part earlier.
pub(super) fn auth_multi_client_test(root_ns: u64) -> bool {
    let (st, ch) = ns_lookup(root_ns, b"/svc/auth", libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT);
    if st != 0 || ch == 0 {
        kprint(b"boot-probe: /svc/auth second session FAIL\n");
        return false;
    }
    // A second resolve, so the answer is not "one spare slot" but "it mints per caller".
    let (st2, ch2) = ns_lookup(root_ns, b"/svc/auth", libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT);
    // SAFETY: closing handles this process owns.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, ch);
        if ch2 != 0 {
            syscall1(SYS_HANDLE_CLOSE, ch2);
        }
    }
    // **The count is the assertion, not the handle values.** Two *successful* resolves while
    // `session-mgr` already holds one means three concurrent sessions, which one spare slot
    // cannot supply. Comparing `ch2 == ch` would be decoration: the probe holds `ch` open
    // across the second resolve and `HandleTable::allocate` never re-issues a live slot, so
    // the numbers differ whatever the server did (PR #235 review, finding 4).
    if st2 != 0 || ch2 == 0 {
        kprint(b"boot-probe: /svc/auth third session FAIL (not minted per caller)\n");
        return false;
    }
    kprint(b"boot-probe: /svc/auth mints a session per caller (session-mgr + 2) ok\n");
    true
}

/// Prove `sys_ns_derive` through the syscall, not only through `Namespace::try_derive`, whose host
/// tests cannot see the rights check, the handle the syscall allocates, or the rights it carries
/// (administration Part A.1).
///
/// Four claims, each one a view depends on:
/// 1. the copy resolves what its source resolves;
/// 2. it can be sent — `TRANSFER` — which the `LOOKUP`-only root this process was spawned with
///    cannot, and it can be pruned, `UNBIND`;
/// 3. pruning the copy leaves the source alone, the snapshot rule seen from the side that matters;
/// 4. a namespace handle without `LOOKUP` cannot be copied.
///
/// `/subtreetest` is the test image's own `[[bind]]`, which `subtree_bind_test` above already
/// resolves, so this borrows a binding that is known to be there rather than adding one.
pub(super) fn ns_derive_test(root_ns: u64) -> bool {
    const PATH: &[u8] = b"/subtreetest/current-generation";
    // SAFETY: a namespace handle this process holds.
    let d = unsafe { syscall1(SYS_NS_DERIVE, root_ns) };
    if d <= 0 {
        Line::new().s(b"boot-probe: ns derive FAIL (").i(d as i64).s(b")").end();
        return false;
    }
    let d = d as u64;
    let close = |h: u64| {
        if h != 0 {
            // SAFETY: closing a handle this process owns.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        }
    };
    let mut ok = true;

    let (st, fh) = ns_lookup(d, PATH, RIGHT_MAP_READ);
    close(fh);
    if st != 0 || fh == 0 {
        kprint(b"boot-probe: ns derive: the copy does not resolve what the root does FAIL\n");
        ok = false;
    }

    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `info` is a writable 24-byte `HandleInfo`, the layout the kernel writes.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, d, (&raw mut info) as u64) };
    let want = RIGHT_TRANSFER | RIGHT_UNBIND;
    if sr != 0 || info.rights & want != want {
        kprint(b"boot-probe: ns derive: the copy cannot be sent or pruned FAIL\n");
        ok = false;
    }

    let bind = b"/subtreetest";
    // SAFETY: a namespace handle this process holds, and a valid path.
    let ur = unsafe { syscall4(SYS_NS_UNBIND, d, bind.as_ptr() as u64, bind.len() as u64, 0) };
    let (cst, cfh) = ns_lookup(d, PATH, RIGHT_MAP_READ);
    let (rst, rfh) = ns_lookup(root_ns, PATH, RIGHT_MAP_READ);
    close(cfh);
    close(rfh);
    if ur != 0 || cst == 0 {
        kprint(b"boot-probe: ns derive: unbinding in the copy did not take FAIL\n");
        ok = false;
    }
    if rst != 0 {
        kprint(b"boot-probe: ns derive: unbinding in the copy reached the root FAIL\n");
        ok = false;
    }

    // A handle to the same copy with everything but `LOOKUP`.
    // SAFETY: `d` carries `DUPLICATE`; the result is a handle this process owns.
    let blind = unsafe { syscall2(SYS_HANDLE_DUPLICATE, d, !RIGHT_LOOKUP) };
    // SAFETY: as above.
    let refused = blind > 0 && unsafe { syscall1(SYS_NS_DERIVE, blind as u64) } < 0;
    if blind > 0 {
        close(blind as u64);
    }
    if !refused {
        kprint(b"boot-probe: ns derive: copied a handle without LOOKUP FAIL\n");
        ok = false;
    }

    close(d);
    if ok {
        kprint(b"boot-probe: ns derive: a sendable snapshot, pruned without touching the root ok\n");
    }
    ok
}
