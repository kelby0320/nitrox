//! The view broker test.

use super::*;
use libos::{ReadOnly, Resource};

/// Two handles, in a `Carried` for a send to move.
fn carrying2<A, B, C, D>(a: Handle<A, B>, b: Handle<C, D>) -> Carried {
    let mut carried = carry(a);
    let _ = carried.push(b);
    carried
}

/// Three handles, in a `Carried` for a send to move.
fn carrying3<A, B, C, D, E, F>(a: Handle<A, B>, b: Handle<C, D>, c: Handle<E, F>) -> Carried {
    let mut carried = carrying2(a, b);
    let _ = carried.push(c);
    carried
}

/// **The view broker, through its own protocol**, before any shell or
/// `with` exists to drive it. `boot-probe` holds the unscoped root namespace, so it can be both a
/// supervisor — opening a session for the demo account — and a client in that session, at
/// `/svc/views/s/<id>`, which is exactly what a session's `/dev/views` reaches.
///
/// What it proves, in the order it runs:
/// 1. a request the policy allows asks for a password;
/// 2. a wrong password is refused with a retry, and the right one, offered straight after, is
///    **held for the session's delay** before it is answered;
/// 3. **the grant arrived**: `nxinstall` is sent a copy of this namespace with `/dev/blk`
///    removed, and still exits 0 ("listed the devices it can see"; 1 would be "none");
/// 4. a client the broker lets in can start its program with its exit heard, even when the
///    broker has let in all it can;
/// 5. **one guess per delay, however many requests**: passwords queued on two requests during
///    one delay are answered a delay apart;
/// 6. a program outside a rule's `run` is refused by the policy;
/// 7. `with --check`'s op refuses a policy nobody could administer, and a handle sent with an op
///    that takes none is closed rather than kept;
/// 8. once the session is closed, its base names nothing.
pub(super) fn view_broker_test(root: Root<'_>) -> bool {
    use librsproto::views::*;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: view broker: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let mut exited = Vec::new();

    let Ok(sup) = lookup::<Channel, Only, _>(root, "/svc/views/session", chan) else {
        return fail(b"no supervisor channel at /svc/views/session");
    };
    let session = match views_call(&sup, OP_VIEWS_OPEN_SESSION, 1, DEMO_USER, Carried::new(), &mut exited) {
        Some((false, body)) => match parse_session_id(&body) {
            Some(id) => id,
            None => return fail(b"OpenSession's reply"),
        },
        _ => return fail(b"OpenSession"),
    };
    let client_path = alloc::format!("/svc/views/s/{session}");
    let Ok(cli) = lookup::<Channel, Only, _>(root, &client_path, chan) else {
        return fail(b"no client channel at the session's base");
    };

    // A copy of this namespace with every disk removed, so a 0 from `nxinstall` can only be the
    // grant's doing.
    let Ok(copy) = root.copy() else {
        return fail(b"derive");
    };
    let blk = "/dev/blk";
    let _ = copy.unbind(blk);
    if lookup::<Resource, ReadOnly, _>(&copy, "/dev/blk/0", Rights::READ).is_ok() {
        return fail(b"precondition: the copy still reaches /dev/blk/0");
    }

    // **Keep a duplicate of what is sent**, the way a caller hoping to reach the view would: the
    // broker must build the view in a namespace of its own, so nothing it grants reaches this.
    let Ok(kept) = copy.duplicate(Rights::from_bits(u64::MAX)) else {
        return fail(b"duplicate the copy");
    };

    let mut req = [0u8; 256];
    let Some(n) = build_request(&mut req, 0, b"admin", b"nxinstall", &[], b"", None) else {
        return fail(b"build a request");
    };
    match views_call(&cli, OP_VIEWS_REQUEST, 2, &req[..n], carry(copy), &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::NeedPassword, _))) => {}
        _ => return fail(b"an allowed request did not ask for a password"),
    }
    match views_call(&cli, OP_VIEWS_PASSWORD, 3, b"not the password", Carried::new(), &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Denied { retry: true }, _))) => {}
        _ => return fail(b"a wrong password was not refused with a retry"),
    }
    let refused_at = clock_ns();
    match views_call(&cli, OP_VIEWS_PASSWORD, 4, DEMO_PASSWORD, Carried::new(), &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Started, _))) => {}
        _ => return fail(b"the right password did not start the program"),
    }
    // **Held, not merely slow.** The delay is two seconds and the scheduler's tick ten
    // milliseconds, so past 1.9 s is the broker holding the check, and much sooner is not.
    let held = clock_ns().saturating_sub(refused_at);
    if held < 1_900_000_000 {
        return fail(b"the password after a wrong one was answered without the session's delay");
    }
    // The program is running with its grant; the namespace this process sent must not have it.
    let kst = lookup::<Resource, ReadOnly, _>(&kept, "/dev/blk/0", Rights::READ);
    let reached = kst.is_ok();
    drop(kst);
    drop(kept);
    if reached {
        return fail(b"the grant was bound into the namespace the caller sent, which it still holds");
    }
    let code = match exited.pop().or_else(|| views_receive(&cli, 0, &mut exited).map(|r| r.1)) {
        Some(body) => parse_exited(&body),
        None => None,
    };
    if code != Some((0, false)) {
        Line::new().s(b"boot-probe: view broker: nxinstall exited ").i(code.map_or(-99, |c| c.0 as i64)).end();
        return fail(b"the program did not see the granted disks");
    }
    // The request is done.
    drop(cli);

    // 3b. **What `disks` leaves out.** The broker asks the storage service `InUse` first.
    // `nxinstall` with no operand writes the devices it can reach to stdout as a table, so a
    // pipe goes with the request and the listing is held to the registry: **the ESP is in it,
    // and the disk holding `init`'s root, the root itself and the mounted scratch disk are not**.
    //
    // **The listing, not an exit code.** `nxinstall` refuses each in-use device by its own rules
    // whether or not the grant withheld it, so a control that handed over every disk would
    // still exit 1.
    let Some(registry) = registry_records(root) else {
        return fail(b"the registry does not read");
    };
    use libkern::device::DeviceKind;
    let path = |r: &libkern::device::DeviceRecord| r.block_index().map(|n| alloc::format!("/dev/blk/{n}"));
    let root_part = registry.iter().find(|r| r.kind() == DeviceKind::Partition && r.name() == b"nitrox-root");
    let esp = registry.iter().find(|r| r.kind() == DeviceKind::Partition && r.name() == b"NITROX_ESP").and_then(path);
    let root_path = root_part.and_then(path);
    let disk = root_part.and_then(|p| registry.iter().find(|r| r.id == p.parent)).and_then(path);
    let scratch = registry.iter().find(|r| r.kind() == DeviceKind::RamDisk).and_then(path);
    let (Some(esp), Some(root_path), Some(disk), Some(scratch)) = (esp, root_path, disk, scratch) else {
        return fail(b"the registry has no ESP, root, root disk and scratch disk to hold the grant to");
    };
    let cli = lookup::<Channel, Only, _>(root, &client_path, chan);
    let copy = root.copy();
    let pipe = Handle::<Channel, Only>::pair(16);
    let (Ok(cli), Ok(copy), Ok((out_w, out_r))) = (cli, copy, pipe) else {
        return fail(b"a client, a copy and a pipe to ask what disks leaves out");
    };
    let _ = copy.unbind(blk);
    let Some(n) = build_request(&mut req, REQ_STDOUT, b"admin", b"nxinstall", &[], b"", None) else {
        return fail(b"build a request");
    };
    match views_call(&cli, OP_VIEWS_REQUEST, 5, &req[..n], carrying2(copy, out_w), &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::NeedPassword, _))) => {}
        _ => return fail(b"a request with a stdout did not ask for a password"),
    }
    match views_call(&cli, OP_VIEWS_PASSWORD, 6, DEMO_PASSWORD, Carried::new(), &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Started, _))) => {}
        _ => return fail(b"nxinstall with a stdout did not start"),
    }
    let listing = libstream::channel::ChannelReceiver::new(libstream::channel::IpcPort::owning(out_r)).receive(1 << 20);
    let code = match exited.pop().or_else(|| views_receive(&cli, 0, &mut exited).map(|r| r.1)) {
        Some(body) => parse_exited(&body),
        None => None,
    };
    // The request is done.
    drop(cli);
    let mut granted: Vec<alloc::string::String> = Vec::new();
    let decoded = listing.ok().is_some_and(|bytes| {
        let Ok(mut tr) = libstream::table::TableReader::new(&bytes) else {
            return false;
        };
        loop {
            match tr.next() {
                Some(Ok(libstream::table::Item::Row(vals))) => match vals.first() {
                    Some(libstream::wire::Value::Str(p)) => granted.push(p.clone()),
                    _ => return false,
                },
                Some(Ok(libstream::table::Item::End(_))) | None => return true,
                _ => return false,
            }
        }
    });
    if !decoded || code != Some((0, false)) {
        return fail(b"nxinstall's listing in the admin view did not read");
    }
    for (what, p) in [
        (&b"the disk holding init's root"[..], &disk),
        (b"init's root", &root_path),
        (b"the mounted scratch disk", &scratch),
    ] {
        if granted.contains(p) {
            Line::new()
                .s(b"boot-probe: view broker: disks granted ")
                .s(what)
                .s(b", ")
                .s(p.as_bytes())
                .s(b", which is in use")
                .end();
            return fail(b"the disks grant handed over a device in use");
        }
    }
    if !granted.contains(&esp) {
        return fail(b"the disks grant withheld the ESP, which nothing has mounted");
    }

    // 4. **A client let in can always start its program.** Open clients until the broker refuses
    // one, then start a program on the last it let in: its exit must still be heard (admitting by
    // what was open left its life channel no slot).
    let mut filled: Vec<Handle<Channel, Only>> = Vec::new();
    let refused = loop {
        match lookup::<Channel, Only, _>(root, &client_path, chan) {
            Ok(ch) => filled.push(ch),
            Err(e) => break e.code().as_i32(),
        }
        if filled.len() > libkern::MAX_WAIT_HANDLES {
            return fail(b"the broker never refused a client");
        }
    };
    if refused != libkern::KError::WouldBlock.as_i32() {
        Line::new()
            .s(b"boot-probe: view broker: client ")
            .u(filled.len() as u64 + 1)
            .s(b" refused with ")
            .i(refused as i64)
            .end();
        return fail(b"a client was refused for something other than room");
    }
    let Some(last) = filled.last() else {
        return fail(b"no client was let in");
    };
    let copy = root.copy();
    let Some(n) = build_request(&mut req, 0, b"admin", b"nxinstall", &[], b"", None) else {
        return fail(b"build a request");
    };
    let sent = copy.ok().map(|copy| views_call(last, OP_VIEWS_REQUEST, 8, &req[..n], carry(copy), &mut exited));
    if !matches!(sent, Some(Some((false, _)))) {
        return fail(b"the last client's request");
    }
    match views_call(last, OP_VIEWS_PASSWORD, 9, DEMO_PASSWORD, Carried::new(), &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Started, _))) => {}
        _ => return fail(b"the last client's program did not start"),
    }
    let heard = exited.pop().or_else(|| views_receive(last, 0, &mut exited).map(|r| r.1));
    if heard.as_deref().and_then(parse_exited).is_none() {
        Line::new().s(b"boot-probe: view broker: ").u(filled.len() as u64).s(b" clients let in").end();
        return fail(b"the exit of the last client's program was never heard");
    }
    drop(filled);

    // 5. Two more requests, `b` and `c`. `b` fails; then a password goes on each before the
    // delay ends. Both are held, and when the first fails it holds the second, so they are
    // not answered back to back.
    let mut pair: Vec<Handle<Channel, Only>> = Vec::new();
    for k in 0..2 {
        let ch = lookup::<Channel, Only, _>(root, &client_path, chan);
        let copy = root.copy();
        let (Ok(ch), Ok(copy)) = (ch, copy) else {
            return fail(b"a client channel and a copy for the paced pair");
        };
        let Some(n) = build_request(&mut req, 0, b"admin", b"nxinstall", &[], b"", None) else {
            return fail(b"build a request");
        };
        match views_call(&ch, OP_VIEWS_REQUEST, 10 + k as u64, &req[..n], carry(copy), &mut exited) {
            Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::NeedPassword, _))) => {}
            _ => return fail(b"the paced pair's request did not ask for a password"),
        }
        pair.push(ch);
    }
    let Ok([b, c]) = <[Handle<Channel, Only>; 2]>::try_from(pair) else {
        return fail(b"a client channel and a copy for the paced pair");
    };
    let retry = |r: Option<(bool, Vec<u8>)>| matches!(r, Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Denied { retry: true }, _))));
    if !retry(views_call(&b, OP_VIEWS_PASSWORD, 20, b"wrong", Carried::new(), &mut exited)) {
        return fail(b"the paced pair's first wrong password");
    }
    let t0 = clock_ns();
    if !rs_send(&b, OP_VIEWS_PASSWORD, 21, b"wrong again", Carried::new())
        || !rs_send(&c, OP_VIEWS_PASSWORD, 22, b"wrong", Carried::new())
    {
        return fail(b"send the paced pair's passwords");
    }
    if !retry(views_receive(&b, 21, &mut exited)) {
        return fail(b"the first held password's answer");
    }
    let t1 = clock_ns();
    if !retry(views_receive(&c, 22, &mut exited)) {
        return fail(b"the second held password's answer");
    }
    let t2 = clock_ns();
    if t1.saturating_sub(t0) < 1_900_000_000 || t2.saturating_sub(t1) < 1_900_000_000 {
        Line::new()
            .s(b"boot-probe: view broker: held answers after ")
            .u((t1 - t0) / 1_000_000)
            .s(b" ms and ")
            .u(t2.saturating_sub(t1) / 1_000_000)
            .s(b" ms")
            .end();
        return fail(b"two passwords held in one delay were checked together");
    }
    // The broker drops what they asked for.
    drop(b);
    drop(c);

    let Ok(cli2) = lookup::<Channel, Only, _>(root, &client_path, chan) else {
        return fail(b"a second client channel");
    };
    let copy2 = root.copy().map_or_else(|_| Carried::new(), carry);
    let Some(n) = build_request(&mut req, 0, b"install", b"nxsh", &[], b"", None) else {
        return fail(b"build a request");
    };
    match views_call(&cli2, OP_VIEWS_REQUEST, 5, &req[..n], copy2, &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Denied { retry: false }, _))) => {}
        _ => return fail(b"a program outside the rule's `run` was not refused"),
    }
    let orphaned = b"[profile.admin]\ngrants = [\"disks\"]\n";
    match views_call(&cli2, OP_VIEWS_CHECK, 6, orphaned, Carried::new(), &mut exited) {
        Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Denied { .. }, _))) => {}
        _ => return fail(b"a policy nobody could administer passed the check"),
    }
    // **A handle sent with anything but a request is closed.** Each op carries one end of a new
    // channel; the broker closing it is what the other end sees as `PeerClosed` — a leak would
    // leave it open, and any program in any session can send handles.
    for (k, op) in [OP_VIEWS_STOP, OP_VIEWS_LIST, OP_VIEWS_CHECK].into_iter().enumerate() {
        let Ok((ours, theirs)) = Handle::<Channel, Only>::pair(1) else {
            return fail(b"a channel to send the broker");
        };
        if views_call(&cli2, op, 30 + k as u64, b"", carry(theirs), &mut exited).is_none() {
            return fail(b"no answer to an op carrying a handle");
        }
        let rr = ours.recv(&mut Msg::new()).err().map(|e| e.code());
        drop(ours);
        if rr != Some(libkern::KError::PeerClosed) {
            Line::new().s(b"boot-probe: view broker: op ").u(op as u64).s(b" kept the handle sent with it").end();
            return fail(b"a handle sent with an op that takes none was kept");
        }
    }
    drop(cli2);

    let mut id = [0u8; 8];
    let n = build_session_id(&mut id, session).unwrap_or(0);
    if !matches!(views_call(&sup, OP_VIEWS_CLOSE_SESSION, 7, &id[..n], Carried::new(), &mut exited), Some((false, _))) {
        return fail(b"CloseSession");
    }
    let gone = lookup::<Channel, Only, _>(root, &client_path, chan);
    if gone.is_ok() {
        return fail(b"a closed session's base still resolves");
    }
    // `NotFound`, not merely a failure: an error body the kernel cannot read is `KernelError`.
    let st = status(&gone);
    if st != libkern::KError::NotFound.as_i32() {
        Line::new().s(b"boot-probe: view broker: a closed session's base answered ").i(st as i64).end();
        return fail(b"a closed session's base was refused for something other than being gone");
    }
    drop(sup);
    log::write(b"boot-probe: view broker: password held, one per delay, grant arrived, exit heard when full, policy refused, stray handles closed, session closed ok\n");
    true
}

/// **A view's grants end with its session, and with its program.** `test-stage grants` runs in an
/// admin view sent with no `/dev/blk` and no `/dev/log`, so all eight of the grants it reports are
/// the broker's, and it never reads its notifications, so it ignores a request to stop. It must see
/// all eight first, which shows it can see them. Then none once the session is closed under it.
/// And a second run, which leaves a child on its namespace and exits, must have its child see none
/// once the broker has reported the exit.
pub(super) fn view_grants_end_test(root: Root<'_>) -> bool {
    use librsproto::views::*;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: view grants: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let mut exited = Vec::new();
    let Ok(sup) = lookup::<Channel, Only, _>(root, "/svc/views/session", chan) else {
        return fail(b"no supervisor channel");
    };
    let mask_on =
        |ch: &Handle<Channel, Only>| receive(ch, clock_ns() + 10_000_000_000).and_then(|m| m.body.first().copied());
    let mut ok = true;
    for exit_case in [false, true] {
        let session = match views_call(&sup, OP_VIEWS_OPEN_SESSION, 1, DEMO_USER, Carried::new(), &mut exited) {
            Some((false, body)) => parse_session_id(&body),
            _ => None,
        };
        let Some(session) = session else {
            return fail(b"OpenSession");
        };
        let cli = lookup::<Channel, Only, _>(root, &alloc::format!("/svc/views/s/{session}"), chan);
        let copy = root.copy();
        let stdin = Handle::<Channel, Only>::pair(4);
        let stdout = stdin.is_ok().then(|| Handle::<Channel, Only>::pair(4));
        let (Ok(cli), Ok(copy), Ok((in_w, in_r)), Some(Ok((out_w, out_r)))) = (cli, copy, stdin, stdout) else {
            return fail(b"a client, a copy and two pipes");
        };
        for at in ["/dev/blk", "/dev/log"] {
            let _ = copy.unbind(at);
        }
        let mut req = [0u8; 256];
        let Some(n) = build_request(&mut req, REQ_STDIN | REQ_STDOUT, b"admin", b"test-stage", &[b"grants"], b"", None)
        else {
            return fail(b"build a request");
        };
        let started = matches!(
            views_call(&cli, OP_VIEWS_REQUEST, 2, &req[..n], carrying3(copy, in_r, out_w), &mut exited),
            Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::NeedPassword, _)))
        ) && matches!(
            views_call(&cli, OP_VIEWS_PASSWORD, 3, DEMO_PASSWORD, Carried::new(), &mut exited),
            Some((false, body)) if matches!(parse_outcome(&body), Some((Outcome::Started, _)))
        );
        let before = (started && rs_send(&in_w, 1, 1, &[], Carried::new())).then(|| mask_on(&out_r)).flatten();
        if before != Some(0xFF) {
            Line::new()
                .s(b"boot-probe: view grants: started ")
                .i(started as i64)
                .s(b", resolving as bits ")
                .i(before.map_or(-1, i64::from))
                .end();
            return fail(b"the program in the view did not see all eight grants");
        }
        let mut close_body = [0u8; 8];
        let closed = build_session_id(&mut close_body, session).is_some();
        let after = if exit_case {
            // The child's channel, then the broker's word that the program exited, after which the
            // child asks what still resolves.
            let child = rs_send(&in_w, 2, 2, &[], Carried::new())
                .then(|| receive(&out_r, clock_ns() + 10_000_000_000))
                .flatten();
            let child = child.and_then(|mut m| m.take::<Channel, Only>(0));
            let heard = exited.pop().or_else(|| views_receive(&cli, 0, &mut exited).map(|r| r.1)).is_some();
            let mask =
                child.as_ref().filter(|child| heard && rs_send(*child, 1, 1, &[], Carried::new())).and_then(mask_on);
            drop(child);
            let _ = views_call(&sup, OP_VIEWS_CLOSE_SESSION, 9, &close_body, Carried::new(), &mut exited);
            mask
        } else {
            let _ = views_call(&sup, OP_VIEWS_CLOSE_SESSION, 9, &close_body, Carried::new(), &mut exited);
            let mask = rs_send(&in_w, 1, 4, &[], Carried::new()).then(|| mask_on(&out_r)).flatten();
            // Leaves a child waiting on a channel this closes below, and exits.
            let _ = rs_send(&in_w, 2, 5, &[], Carried::new()).then(|| receive(&out_r, clock_ns() + 10_000_000_000));
            mask
        };
        drop(in_w);
        drop(out_r);
        drop(cli);
        if !closed || after != Some(0) {
            Line::new().s(b"boot-probe: view grants: still resolving, as bits: ").i(after.map_or(-1, i64::from)).end();
            ok = fail(if exit_case {
                b"a child of a program that exited"
            } else {
                b"a program whose session was closed"
            });
        }
    }
    if ok {
        log::write(b"boot-probe: a view's grants end with its session and with its program ok\n");
    }
    drop(sup);
    ok
}
