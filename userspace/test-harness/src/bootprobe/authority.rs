//! Binds, derived namespaces, and the calls a missing syscap refuses.

use super::*;
use libos::{MapReadWrite, Memory, NsMutable, ReadWrite, Resource};

/// **`sys_power` from anything but `init` is refused**, and the machine goes on: with no handle,
/// with a namespace this process may only look up in, and with a disk it may write — a `WRITE`
/// handle, so it is the object's type that refuses it, not a missing right.
///
/// The halt it would otherwise be is what makes a wrong answer loud: the boot stops before the
/// verdict, and `test-qemu` times out.
pub(super) fn power_refused_test(root: Root<'_>) -> bool {
    use libkern::KError;
    use libos::system::{Stop, stop};
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: power FAIL: ").s(why).end();
        false
    };
    if refused::authority::power(0) != KError::InvalidHandle.as_i32() as i64 {
        return fail(b"no handle was not refused InvalidHandle");
    }
    if refused::authority::power(root.raw().0) != KError::NoAccess.as_i32() as i64 {
        return fail(b"a lookup-only namespace was not refused NoAccess");
    }
    // The root partition, as `flush_test` finds it: the installed disk's, or the live stick's.
    let labels: [&[u8]; 2] = [b"nitrox-root", b"nitrox-live"];
    let root_index = |records: alloc::vec::Vec<libkern::device::DeviceRecord>| {
        records.iter().find(|r| labels.contains(&r.name())).and_then(|r| r.block_index())
    };
    let Some(index) = registry_records(root).and_then(root_index) else {
        return fail(b"no root partition in the registry");
    };
    let path = alloc::format!("/dev/blk/{index}");
    let Ok(dev) = lookup::<Resource, ReadWrite, _>(root, &path, Rights::READ | Rights::WRITE) else {
        return fail(b"the root partition does not open read-write");
    };
    let refused = stop(&dev, Stop::Halt);
    drop(dev);
    if refused.code() != KError::InvalidArgument {
        return fail(b"a writable disk was not refused InvalidArgument");
    }
    log::write(b"boot-probe: sys_power refused no handle, a namespace and a writable disk ok\n");
    true
}

/// **`sys_clock_set` is refused without `SYSTEM_CLOCK`**, which this process does not hold, and
/// the clock does not move: a set to 2031 refused `NoAccess`, and the clock read either side of it
/// within a minute of itself. Before the authority, the arguments: `Monotonic` never steps, and
/// 1999 is before what the hardware clock holds — both `InvalidArgument`.
pub(super) fn clock_set_refused_test() -> bool {
    use libkern::KError;
    use libos::system::set_clock;
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: clock FAIL: ").s(why).end();
        false
    };
    let set = |secs: u64| set_clock(secs * 1_000_000_000).map_err(|e| e.code());
    let in_2031 = 1_925_078_400; // 2031-01-02T00:00:00Z
    if refused::authority::set_monotonic_clock(in_2031 * 1_000_000_000) != KError::InvalidArgument.as_i32() as i64 {
        return fail(b"setting the monotonic clock was not refused InvalidArgument");
    }
    if set(946_684_799) != Err(KError::InvalidArgument) {
        return fail(b"a time in 1999 was not refused InvalidArgument");
    }
    let before = time::wall().ok();
    if set(in_2031) != Err(KError::NoAccess) {
        return fail(b"a set without SYSTEM_CLOCK was not refused NoAccess");
    }
    let (Some(before), Some(after)) = (before, time::wall().ok()) else {
        return fail(b"the wall clock does not read");
    };
    if after < before || after - before > 60_000_000_000 {
        return fail(b"the wall clock moved across a refused set");
    }
    log::write(b"boot-probe: sys_clock_set refused the monotonic clock, 1999 and no SYSTEM_CLOCK ok\n");
    true
}

/// **Every mapping needs `MAP_READ`**, since a mapped page reads whatever bits were asked: a
/// handle without it, mapped asking for none, is refused `NoAccess`; with it, mapped.
pub(super) fn map_needs_read_test() -> bool {
    use libkern::KError;
    let Ok(mem) = Handle::<Memory, MapReadWrite>::create(PAGE as usize) else {
        Line::new().s(b"boot-probe: map FAIL: no memory object").end();
        return false;
    };
    let Ok(blind) = mem.duplicate(Rights::INSPECT | Rights::TRANSFER) else {
        Line::new().s(b"boot-probe: map FAIL: no handle without MAP_READ").end();
        return false;
    };
    let refused = refused::authority::map_asking(&blind, 0, PAGE, 0);
    let mapped = mem.map(PAGE as usize);
    if refused != KError::NoAccess.as_i32() as i64 {
        Line::new().s(b"boot-probe: map FAIL: a handle without MAP_READ was mapped").end();
        return false;
    }
    if mapped.is_err() {
        Line::new().s(b"boot-probe: map FAIL: a handle with MAP_READ was not mapped").end();
        return false;
    }
    log::write(b"boot-probe: every mapping needs MAP_READ ok\n");
    true
}

/// **A message carries nothing past its payload**: the kernel zeroes the rest, so a buffer a
/// sender reuses cannot hand the receiver its stale bytes, a typed password among them.
pub(super) fn payload_sealed_test() -> bool {
    let Ok((a, b)) = Handle::<Channel, Only>::pair(4) else {
        Line::new().s(b"boot-probe: payload FAIL: no channel").end();
        return false;
    };
    let mut msg = Msg::new();
    msg.payload_mut().fill(0xA5);
    let sent = msg.set_len(4).is_ok() && a.send(&msg, &mut Carried::new()).is_ok();
    let mut got = Msg::new();
    let rr = b.recv(&mut got).is_ok();
    drop((a, b));
    let payload = got.payload_mut();
    if !sent || !rr || payload[..4] != [0xA5; 4] {
        Line::new().s(b"boot-probe: payload FAIL: the message did not arrive whole").end();
        return false;
    }
    if payload[4..].iter().any(|&x| x != 0) {
        Line::new().s(b"boot-probe: payload FAIL: bytes past the payload reached the receiver").end();
        return false;
    }
    log::write(b"boot-probe: a message carries nothing past its payload ok\n");
    true
}

/// **A spawn copies a handle to the child only with `DUPLICATE`**: a grant in duplicate mode
/// leaves the parent its handle and gives the child another, so without the right it is refused
/// `NoAccess`, as `sys_handle_duplicate` refuses. The same handle moved is taken.
pub(super) fn spawn_copy_needs_duplicate_test(root: Root<'_>) -> bool {
    use libkern::KError;
    use libos::Spawn;
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: spawn grant FAIL: ").s(why).end();
        false
    };
    let img = lookup::<File, MapRead, _>(root, "/bin/test-stage", Rights::MAP_READ | Rights::INSPECT);
    let mem = Handle::<Memory, MapReadWrite>::create(PAGE as usize);
    let (Ok(img), Ok(mem)) = (img, mem) else {
        return fail(b"no /bin/test-stage or memory object");
    };
    let kept = mem.duplicate(Rights::INSPECT | Rights::TRANSFER);
    drop(mem);
    let Ok(kept) = kept else {
        return fail(b"no handle without DUPLICATE");
    };
    // test-stage exits at once, with its `arg0` of none.
    let mut copy = Spawn::new(&img);
    let refused = copy.grant_copy(&kept, ALL).is_ok()
        && matches!(copy.spawn(), Err(refused) if refused.error.code() == KError::NoAccess);
    let mut moved = Spawn::new(&img);
    let taken = moved.grant(kept, ALL).is_ok() && moved.spawn().is_ok();
    drop(img);
    if !refused {
        return fail(b"a copy without DUPLICATE was not refused NoAccess");
    }
    if !taken {
        return fail(b"the same handle moved was not taken");
    }
    log::write(b"boot-probe: a spawn copies a handle only with DUPLICATE, and moves it without ok\n");
    true
}

/// **A send's handle list may not name a handle twice, nor the endpoint the message is for**:
/// the first would copy a handle its holder cannot duplicate, the second leave that endpoint
/// holding itself in its own ring, never freed. A list with neither is taken.
pub(super) fn transfer_list_test() -> bool {
    use libkern::KError;
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: transfers FAIL: ").s(why).end();
        false
    };
    let channel = Handle::<Channel, Only>::pair(4);
    let mem = Handle::<Memory, MapReadWrite>::create(PAGE as usize);
    let (Ok((a, b)), Ok(mem)) = (channel, mem) else {
        return fail(b"no channel or memory object");
    };
    let msg = Msg::new();
    let refused = KError::InvalidArgument;
    let twice = refused::authority::send_listing(&a, &[mem.raw().0, mem.raw().0]) == refused.as_i32() as i64;
    let mut listed = carry(b);
    let itself = matches!(a.send(&msg, &mut listed), Err(e) if e.code() == refused);
    // The refused send left the endpoint in `listed`, and the channel is whole while it is held.
    let b = listed.take::<Channel, Only>(0);
    let taken = a.send(&msg, &mut carry(mem)).is_ok();
    drop((a, b));
    if !twice {
        return fail(b"a handle listed twice was not refused InvalidArgument");
    }
    if !itself {
        return fail(b"the endpoint the message is for was not refused InvalidArgument");
    }
    if !taken {
        return fail(b"a list with neither was not taken");
    }
    log::write(b"boot-probe: a send refuses a handle twice and the endpoint it is for ok\n");
    true
}

/// **A lookup's PO stays with its caller**: it holds the object it resolves, which could be the
/// endpoint whose ring it is sent to. Its duplicate and its send are refused `NoAccess`.
pub(super) fn lookup_po_stays_test(root: Root<'_>) -> bool {
    use libkern::KError;
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: lookup PO FAIL: ").s(why).end();
        false
    };
    let Some(stays) = refused::authority::lookup_po_stays(root, "/bin/test-stage") else {
        return fail(b"no lookup or channel");
    };
    let refused = KError::NoAccess.as_i32() as i64;
    if stays.duplicate != refused {
        return fail(b"its duplicate was not refused NoAccess");
    }
    if stays.send != refused {
        return fail(b"its send was not refused NoAccess");
    }
    if !stays.resolved {
        return fail(b"the lookup it stayed for did not resolve");
    }
    log::write(
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
    use libkern::KError;
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: bind FAIL: ").s(why).end();
        false
    };
    let ns = Handle::<Namespace, NsMutable>::create();
    let mem = Handle::<Memory, MapReadWrite>::create(PAGE as usize);
    let (Ok(ns), Ok(mem)) = (ns, mem) else {
        return fail(b"no namespace or memory object to bind");
    };
    let kept = mem.duplicate(Rights::MAP_READ | Rights::INSPECT);
    let refused =
        kept.as_ref().is_ok_and(|kept| matches!(ns.bind("/kept", kept), Err(e) if e.code() == KError::NoAccess));
    let taken = ns.bind("/given", &mem).is_ok();
    drop((ns, mem, kept));
    if !refused {
        return fail(b"a handle without TRANSFER was not refused NoAccess");
    }
    if !taken {
        return fail(b"a handle with TRANSFER was not bound");
    }
    log::write(b"boot-probe: a direct bind needs TRANSFER: refused without it, taken with it ok\n");
    true
}

/// Reply `SUBNAMESPACE` to `request_id` on `ch`: the first `consumed` bytes of the suffix stand for
/// `base` in `ns`, a duplicate of which is transferred (with `LOOKUP`, which the kernel requires).
pub(super) fn reply_subnamespace(
    ch: &Handle<Channel, Only>,
    request_id: u64,
    consumed: u16,
    base: &[u8],
    ns: &Handle<Namespace, NsMutable>,
) -> bool {
    let Ok(dup) = ns.duplicate(Rights::LOOKUP | Rights::TRANSFER) else {
        return false;
    };
    let mut body = [0u8; librsproto::namespace::SUBNAMESPACE_PREFIX_LEN + 64];
    let mut msg = Msg::new();
    let encoded = librsproto::namespace::subnamespace_reply(&mut body, consumed, base).and_then(|n| {
        librsproto::encode(
            msg.payload_mut(),
            librsproto::OP_NS_RESOLVE,
            request_id,
            librsproto::RS_FLAG_REPLY,
            &body[..n],
            1,
        )
    });
    let Some(len) = encoded else {
        return false;
    };
    msg.set_len(len).is_ok() && ch.send(&msg, &mut carry(dup)).is_ok()
}

/// Prove **subtree-scoped namespace binding** end to end. `mount_one` bound the fs endpoint a
/// second time at `/subtreetest` scoped to base `/system`, so a lookup of
/// `/subtreetest/current-generation` must forward `system/current-generation` to the server and
/// resolve to the *same* file as `/system/current-generation`; the leading bytes of both must
/// match.
pub(super) fn subtree_bind_test(root: Root<'_>) -> bool {
    fn map_first_page(root: Root<'_>, path: &str) -> Option<Mapping<MapRead>> {
        let fh = lookup::<File, MapRead, _>(root, path, Rights::MAP_READ).ok()?;
        fh.map(PAGE as usize).ok()
    }

    let direct = map_first_page(root, "/system/current-generation");
    let via_sub = map_first_page(root, "/subtreetest/current-generation");
    let (Some(direct), Some(via_sub)) = (direct, via_sub) else {
        log::write(b"boot-probe: subtree resolve FAIL\n");
        return false;
    };
    // Compare the leading bytes (the file is a short text line; the page tail is
    // zero-padded, so the head suffices).
    let (mut a, mut b) = ([0u8; 64], [0u8; 64]);
    let same = direct.read_into(0, &mut a).is_ok() && via_sub.read_into(0, &mut b).is_ok() && a == b;
    drop((direct, via_sub));
    if same {
        log::write(b"boot-probe: subtree bind (/subtreetest -> /system) resolves + matches ok\n");
        true
    } else {
        log::write(b"boot-probe: subtree bind MISMATCH\n");
        false
    }
}

/// **`libusers::valid_home` is the kernel's rule for a subtree base**: each home is bound as one,
/// and the kernel's answer must be the library's. Not the empty base, which binds unscoped.
pub(super) fn home_rules_are_the_kernels_test() -> bool {
    let mut longest = [b'a'; libusers::HOME_MAX];
    longest[0] = b'/';
    let mut one_past = [b'a'; libusers::HOME_MAX + 1];
    one_past[0] = b'/';
    let homes: [&[u8]; 13] = [
        b"/home/alice",
        b"/home",
        b"/a/b/c",
        &longest,
        &one_past,
        b"/",
        b"relative",
        b"/home/",
        b"/home//a",
        b"/home/./a",
        b"/home/../a",
        b"/home/a/.",
        b"/..",
    ];
    let ns = Handle::<Namespace, NsMutable>::create();
    let Some((endpoint, _peer)) = channel() else {
        return false;
    };
    let mut agree = ns.is_ok();
    for (i, home) in homes.iter().enumerate() {
        let at = alloc::format!("/h{i}");
        let bound = match (&ns, core::str::from_utf8(home)) {
            (Ok(ns), Ok(base)) => ns.bind_server(&at, endpoint.as_ref(), base).is_ok(),
            _ => false,
        };
        if bound != libusers::valid_home(home) {
            Line::new().s(b"boot-probe: homes: the kernel disagrees on ").s(home).end();
            agree = false;
        }
    }
    if agree {
        log::write(b"boot-probe: a home is what the kernel takes as a subtree base ok\n");
    }
    agree
}

/// Prove `auth-service` serves **more than one client**.
///
/// `session-mgr` resolves `/svc/auth` at startup and holds its session for the machine's life,
/// so there is already one client when this runs; resolving a second is the whole assertion.
/// It is tested here because the second consumer, `desktop-session-mgr`, comes later: a
/// capability reachable by nobody is untested by its component alone.
pub(super) fn auth_multi_client_test(root: Root<'_>) -> bool {
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let Ok(ch) = lookup::<Channel, Only, _>(root, "/svc/auth", chan) else {
        log::write(b"boot-probe: /svc/auth second session FAIL\n");
        return false;
    };
    let second = lookup::<Channel, Only, _>(root, "/svc/auth", chan);
    drop(ch);
    // **The count is the assertion, not the handle values.** Two *successful* resolves while
    // `session-mgr` already holds one means three concurrent sessions, which one spare slot
    // cannot supply. Comparing `ch2 == ch` would be decoration: the probe holds `ch` open
    // across the second resolve, so the numbers differ whatever the server did.
    if second.is_err() {
        log::write(b"boot-probe: /svc/auth third session FAIL (not minted per caller)\n");
        return false;
    }
    log::write(b"boot-probe: /svc/auth mints a session per caller (session-mgr + 2) ok\n");
    true
}

/// Prove `sys_ns_create(from)` through the syscall, not only through `Namespace::try_copy`, whose host
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
pub(super) fn ns_copy_test(root: Root<'_>) -> bool {
    const PATH: &str = "/subtreetest/current-generation";
    let d = match root.copy() {
        Ok(d) => d,
        Err(e) => {
            Line::new().s(b"boot-probe: ns copy FAIL (").i(i64::from(e.code().as_i32())).s(b")").end();
            return false;
        }
    };
    let mut ok = true;

    if lookup::<File, MapRead, _>(&d, PATH, Rights::MAP_READ).is_err() {
        log::write(b"boot-probe: ns copy: the copy does not resolve what the root does FAIL\n");
        ok = false;
    }

    let want = Rights::TRANSFER.bits() | Rights::UNBIND.bits();
    if !d.stat().is_ok_and(|info| info.rights & want == want) {
        log::write(b"boot-probe: ns copy: the copy cannot be sent or pruned FAIL\n");
        ok = false;
    }

    let unbound = d.unbind("/subtreetest");
    let in_copy = lookup::<File, MapRead, _>(&d, PATH, Rights::MAP_READ);
    let in_root = lookup::<File, MapRead, _>(root, PATH, Rights::MAP_READ);
    let (copy_resolves, root_resolves) = (in_copy.is_ok(), in_root.is_ok());
    drop((in_copy, in_root));
    if unbound.is_err() || copy_resolves {
        log::write(b"boot-probe: ns copy: unbinding in the copy did not take FAIL\n");
        ok = false;
    }
    if !root_resolves {
        log::write(b"boot-probe: ns copy: unbinding in the copy reached the root FAIL\n");
        ok = false;
    }

    // A handle to the same copy with everything but `LOOKUP`.
    let refused = d.duplicate(Rights::from_bits(!Rights::LOOKUP.bits())).is_ok_and(|blind| blind.copy().is_err());
    if !refused {
        log::write(b"boot-probe: ns copy: copied a handle without LOOKUP FAIL\n");
        ok = false;
    }

    drop(d);
    if ok {
        log::write(b"boot-probe: ns copy: a sendable snapshot, pruned without touching the root ok\n");
    }
    ok
}

/// **A program in the store is not writable through `/bin`**: a lookup asking for `MAP_WRITE`
/// gets a handle without it, and a writable mapping of the program is refused. A binary's
/// page-cache object is every later spawn's, and a sync writes it to disk.
pub(super) fn store_is_read_only_test(root: Root<'_>) -> bool {
    let fail = |why: &[u8]| {
        Line::new().s(b"boot-probe: store read-only FAIL: ").s(why).end();
        false
    };
    let h = lookup::<File, MapReadWrite, _>(root, "/bin/list", Rights::MAP_READ | Rights::MAP_WRITE | Rights::INSPECT);
    let info = h.as_ref().ok().and_then(|h| h.stat().ok());
    let writable = h.as_ref().ok().zip(info).and_then(|(h, i)| h.map(i.size as usize).ok());
    let mapped_writable = writable.is_some();
    drop(writable);
    drop(h);
    let Some(info) = info else {
        return fail(b"/bin/list did not resolve");
    };
    if info.rights & Rights::MAP_WRITE.bits() != 0 {
        return fail(b"a lookup through /bin was granted MAP_WRITE");
    }
    if mapped_writable {
        return fail(b"a program in the store mapped writable");
    }
    log::write(b"boot-probe: the store is read-only through /bin ok\n");
    true
}
