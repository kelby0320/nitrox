//! Binds, derived namespaces, and the calls a missing syscap refuses.

use super::*;

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

/// **Every mapping needs `MAP_READ`**, since a mapped page reads whatever bits were asked: a
/// handle without it, mapped asking for none, is refused `NoAccess`; with it, mapped.
pub(super) fn map_needs_read_test() -> bool {
    use libkern::{KError, RIGHT_INSPECT};
    // SAFETY: register-only syscall; it creates an object and touches no memory of ours.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if mem <= 0 {
        Line::new().s(b"boot-probe: map FAIL: no memory object").end();
        return false;
    }
    // SAFETY: register-only syscall on a handle this process holds.
    let blind = unsafe { syscall2(SYS_HANDLE_DUPLICATE, mem as u64, RIGHT_INSPECT | RIGHT_TRANSFER) };
    if blind <= 0 {
        close(mem as u64);
        Line::new().s(b"boot-probe: map FAIL: no handle without MAP_READ").end();
        return false;
    }
    // SAFETY: register-only syscalls on handles this process holds; nothing is read through them.
    let (refused, mapped) = unsafe {
        (
            syscall4(SYS_MEMORY_MAP, blind as u64, 0, PAGE, 0),
            syscall4(SYS_MEMORY_MAP, mem as u64, 0, PAGE, RIGHT_MAP_READ),
        )
    };
    if mapped > 0 {
        // SAFETY: unmapping the page this call mapped.
        unsafe { syscall2(SYS_MEMORY_UNMAP, mapped as u64, PAGE) };
    }
    if refused > 0 {
        // SAFETY: as above.
        unsafe { syscall2(SYS_MEMORY_UNMAP, refused as u64, PAGE) };
    }
    close(blind as u64);
    close(mem as u64);
    if refused != KError::NoAccess.as_i32() as i64 {
        Line::new().s(b"boot-probe: map FAIL: a handle without MAP_READ was mapped").end();
        return false;
    }
    if mapped <= 0 {
        Line::new().s(b"boot-probe: map FAIL: a handle with MAP_READ was not mapped").end();
        return false;
    }
    kprint(b"boot-probe: every mapping needs MAP_READ ok\n");
    true
}

/// **A message carries nothing past its payload**: the kernel zeroes the rest, so a buffer a
/// sender reuses cannot hand the receiver its stale bytes, a typed password among them.
pub(super) fn payload_sealed_test() -> bool {
    use libkern::{IPC_PAYLOAD_SIZE, SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_SEND};
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    if unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, 4, 0) } != 0 {
        Line::new().s(b"boot-probe: payload FAIL: no channel").end();
        return false;
    }
    let mut msg = [0xA5u8; 4096];
    msg[..24].fill(0);
    msg[4..8].copy_from_slice(&4u32.to_le_bytes());
    // SAFETY: a valid message buffer; no handles.
    let sent = unsafe { syscall5(SYS_CHANNEL_SEND, a, msg.as_ptr() as u64, 0, 0, SENDMODE_NOBLOCK) } == 0;
    let mut got = [0u8; IPC_MSG_SIZE];
    let rr = libkern::channel_recv(b, &mut got).is_ok();
    close(a);
    close(b);
    let payload = &got[24..24 + IPC_PAYLOAD_SIZE];
    if !sent || !rr || payload[..4] != [0xA5; 4] {
        Line::new().s(b"boot-probe: payload FAIL: the message did not arrive whole").end();
        return false;
    }
    if payload[4..].iter().any(|&x| x != 0) {
        Line::new().s(b"boot-probe: payload FAIL: bytes past the payload reached the receiver").end();
        return false;
    }
    kprint(b"boot-probe: a message carries nothing past its payload ok\n");
    true
}

/// **A spawn copies a handle to the child only with `DUPLICATE`**: a grant in duplicate mode
/// leaves the parent its handle and gives the child another, so without the right it is refused
/// `NoAccess`, as `sys_handle_duplicate` refuses. The same handle moved is taken.
pub(super) fn spawn_copy_needs_duplicate_test(root_ns: u64) -> bool {
    use libkern::abi::SpawnArgs;
    use libkern::{KError, RIGHT_INSPECT, SYS_PROCESS_SPAWN};
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: spawn grant FAIL: ").s(why).end();
        false
    };
    let (st, img) = ns_lookup(root_ns, b"/bin/test-stage", RIGHT_MAP_READ);
    // SAFETY: register-only syscall; it creates an object and touches no memory of ours.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if st != 0 || img == 0 || mem <= 0 {
        return fail(b"no /bin/test-stage or memory object");
    }
    // SAFETY: register-only syscall on a handle this process holds.
    let kept = unsafe { syscall2(SYS_HANDLE_DUPLICATE, mem as u64, RIGHT_INSPECT | RIGHT_TRANSFER) };
    close(mem as u64);
    if kept <= 0 {
        close(img);
        return fail(b"no handle without DUPLICATE");
    }
    let spawn = |move_mask: u32| {
        let args = SpawnArgs {
            image: img,
            handle_count: 1,
            move_mask,
            arg0: 0, // test-stage exits at once
            handles: [kept as u64, 0, 0, 0],
            rights: [u64::MAX; 4],
            namespace: 0,
            syscaps: 0,
        };
        // SAFETY: a valid argument block naming handles this process holds.
        let p = unsafe { syscall1(SYS_PROCESS_SPAWN, (&raw const args) as u64) };
        if p > 0 {
            close(p as u64);
        }
        p
    };
    let refused = spawn(0) == KError::NoAccess.as_i32() as i64;
    let taken = spawn(1) > 0;
    if !taken {
        close(kept as u64);
    }
    close(img);
    if !refused {
        return fail(b"a copy without DUPLICATE was not refused NoAccess");
    }
    if !taken {
        return fail(b"the same handle moved was not taken");
    }
    kprint(b"boot-probe: a spawn copies a handle only with DUPLICATE, and moves it without ok\n");
    true
}

/// **A send's handle list may not name a handle twice, nor the endpoint the message is for**:
/// the first would copy a handle its holder cannot duplicate, the second leave that endpoint
/// holding itself in its own ring, never freed. A list with neither is taken.
pub(super) fn transfer_list_test() -> bool {
    use libkern::{KError, SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_SEND};
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: transfers FAIL: ").s(why).end();
        false
    };
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let made = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, 4, 0) } == 0;
    // SAFETY: register-only syscall; it creates an object and touches no memory of ours.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if !made || mem <= 0 {
        return fail(b"no channel or memory object");
    }
    let mem = mem as u64;
    let msg = [0u8; 4096];
    let send = |hs: &[u64]| {
        // SAFETY: a valid message buffer and handle array.
        unsafe {
            syscall5(SYS_CHANNEL_SEND, a, msg.as_ptr() as u64, hs.as_ptr() as u64, hs.len() as u64, SENDMODE_NOBLOCK)
        }
    };
    let refused = KError::InvalidArgument.as_i32() as i64;
    let twice = send(&[mem, mem]) == refused;
    let itself = send(&[b]) == refused;
    let taken = send(&[mem]) == 0;
    if !taken {
        close(mem);
    }
    close(a);
    close(b);
    if !twice {
        return fail(b"a handle listed twice was not refused InvalidArgument");
    }
    if !itself {
        return fail(b"the endpoint the message is for was not refused InvalidArgument");
    }
    if !taken {
        return fail(b"a list with neither was not taken");
    }
    kprint(b"boot-probe: a send refuses a handle twice and the endpoint it is for ok\n");
    true
}

/// **A lookup's PO stays with its caller**: it holds the object it resolves, which could be the
/// endpoint whose ring it is sent to. Its duplicate and its send are refused `NoAccess`.
pub(super) fn lookup_po_stays_test(root_ns: u64) -> bool {
    use libkern::{
        KError, RIGHT_INSPECT, RIGHT_WAIT, SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_SEND, SYS_NS_LOOKUP,
    };
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: lookup PO FAIL: ").s(why).end();
        false
    };
    let path = b"/bin/test-stage";
    // SAFETY: a valid path and a namespace handle this process holds.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, root_ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ) };
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let made = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, 4, 0) } == 0;
    if po <= 0 || !made {
        return fail(b"no lookup or channel");
    }
    let po = po as u64;
    // SAFETY: register-only syscall on a handle this process holds.
    let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, po, RIGHT_WAIT | RIGHT_INSPECT) };
    let msg = [0u8; 4096];
    let hs = [po];
    // SAFETY: a valid message buffer and handle array.
    let sent = unsafe { syscall5(SYS_CHANNEL_SEND, a, msg.as_ptr() as u64, hs.as_ptr() as u64, 1, SENDMODE_NOBLOCK) };
    close(a);
    close(b);
    if dup > 0 {
        close(dup as u64);
    }
    let refused = KError::NoAccess.as_i32() as i64;
    // Read last, its handle closed: a PO the send took would be gone, its wait failing.
    let (st, img) = po_wait(po);
    if st == 0 && img != 0 {
        close(img);
    }
    if dup != refused {
        return fail(b"its duplicate was not refused NoAccess");
    }
    if sent != refused {
        return fail(b"its send was not refused NoAccess");
    }
    if st != 0 || img == 0 {
        return fail(b"the lookup it stayed for did not resolve");
    }
    kprint(
        b"boot-probe: a lookup's PO is neither duplicated nor sent ok
",
    );
    true
}

/// **A direct-handle bind needs `TRANSFER`**, since every resolve of the binding mints a handle
/// to the object for whoever asked: without it, `init` could publish its system-control object,
/// whose handle carries no `TRANSFER` so that it cannot leave `init`.
///
/// A memory object's handle, duplicated without `TRANSFER`, is refused `NoAccess`, and the
/// original, with it, is taken — in the same namespace, so the refusal is the right's.
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

/// Prove **subtree-scoped namespace binding** end to end. `mount_one` bound the fs endpoint a
/// second time at `/subtreetest` scoped to base `/system`, so a lookup of
/// `/subtreetest/current-generation` must forward `system/current-generation` to the server and
/// resolve to the *same* file as `/system/current-generation`; the leading bytes of both must
/// match.
pub(super) fn subtree_bind_test(root_ns: u64) -> bool {
    fn map_first_page(root_ns: u64, path: &[u8]) -> u64 {
        let (st, fh) = ns_lookup(root_ns, path, RIGHT_MAP_READ);
        if st != 0 || fh == 0 {
            return 0;
        }
        // SAFETY: mapping a file handle we hold, read-only; the result is checked below
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, PAGE, RIGHT_MAP_READ) };
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
    // SAFETY: unmapping our two mappings, tidied before the checks that follow.
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

/// Prove `auth-service` serves **more than one client**.
///
/// `session-mgr` resolves `/svc/auth` at startup and holds its session for the machine's life,
/// so there is already one client when this runs; resolving a second is the whole assertion.
/// It is tested here because the second consumer, `desktop-session-mgr`, comes later: a
/// capability reachable by nobody is untested by its component alone.
pub(super) fn auth_multi_client_test(root_ns: u64) -> bool {
    let (st, ch) = ns_lookup(root_ns, b"/svc/auth", libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT);
    if st != 0 || ch == 0 {
        kprint(b"boot-probe: /svc/auth second session FAIL\n");
        return false;
    }
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
    // across the second resolve, so the numbers differ whatever the server did.
    if st2 != 0 || ch2 == 0 {
        kprint(b"boot-probe: /svc/auth third session FAIL (not minted per caller)\n");
        return false;
    }
    kprint(b"boot-probe: /svc/auth mints a session per caller (session-mgr + 2) ok\n");
    true
}

/// Prove `sys_ns_derive` through the syscall, not only through `Namespace::try_derive`, whose host
/// tests cannot see the rights check, the handle the syscall allocates, or the rights it carries.
///
/// Four claims, each one a view depends on:
/// 1. the copy resolves what its source resolves;
/// 2. it can be sent — `TRANSFER` — which the `LOOKUP`-only root this process was spawned with
///    cannot, and it can be pruned, `UNBIND`;
/// 3. pruning the copy leaves the source alone;
/// 4. a namespace handle without `LOOKUP` cannot be copied.
///
/// `/subtreetest` is the test image's own `[[bind]]`, already resolved by `subtree_bind_test`.
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

/// **A program in the store is not writable through `/bin`**: a lookup asking for `MAP_WRITE`
/// gets a handle without it, and a writable mapping of the program is refused. A binary's
/// page-cache object is every later spawn's, and a sync writes it to disk.
pub(super) fn store_is_read_only_test(root_ns: u64) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: store read-only FAIL: ").s(why).end();
        false
    };
    let (st, h) = ns_lookup(root_ns, b"/bin/list", RIGHT_MAP_READ | RIGHT_MAP_WRITE | libkern::RIGHT_INSPECT);
    let info = if st == 0 { stat(h) } else { None };
    let writable = info.and_then(|i| map_file(h, i.size, RIGHT_MAP_READ | RIGHT_MAP_WRITE).map(|a| (a, i.size)));
    if let Some((addr, size)) = writable {
        // SAFETY: unmapping the mapping just made, which nothing wrote through.
        unsafe { syscall2(SYS_MEMORY_UNMAP, addr, size) };
    }
    close(h);
    let Some(info) = info else {
        return fail(b"/bin/list did not resolve");
    };
    if info.rights & RIGHT_MAP_WRITE != 0 {
        return fail(b"a lookup through /bin was granted MAP_WRITE");
    }
    if writable.is_some() {
        return fail(b"a program in the store mapped writable");
    }
    kprint(b"boot-probe: the store is read-only through /bin ok\n");
    true
}
