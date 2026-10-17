//! The service, restart, power-endpoint and log tests.

use super::*;

/// **A restarted server is reached again at its path.**
///
/// `restart-probe` is a test image's server (`policy = "always"`). It answers `id` with a token
/// unique to the instance, and exits when asked for `exit`. So: take its token, have it exit,
/// and resolve the same path until a different token comes back.
///
/// **What this proves is the registry.** The root's `/svc/restart-probe` is bound once, to
/// `service-mgr`'s route for the server; only the registry binding changes on a restart. A root
/// binding to the server's own endpoint would reach the dead one, which answers `PeerClosed`.
///
/// **And that a binding made before the restart reaches the new instance**, in a copy of the
/// root made before it: a session's case, whose bindings nothing rebinds. A restart that rebound
/// the root's path alone passes the first half and fails this one.
pub(super) fn restart_test(root_ns: u64) -> bool {
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: restart: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let token = |ns: u64| -> Option<u64> {
        let (st, ch) = ns_lookup(ns, b"/svc/restart-probe/id", chan);
        if st != 0 || ch == 0 {
            return None;
        }
        let m = receive(ch, clock_ns() + 5_000_000_000);
        close(ch);
        let m = m?;
        m.handles.iter().for_each(|&h| close(h));
        Some(u64::from_le_bytes(m.body.get(..8)?.try_into().ok()?))
    };
    let Some(first) = token(root_ns) else {
        return fail(b"/svc/restart-probe answered no token");
    };
    // **A namespace bound before the restart, and never touched again** — a session's. A copy
    // of the root holds the same binding objects, and later changes to the root do not reach it.
    // SAFETY: a namespace handle this process holds; the copy is its own to close.
    let copy = unsafe { syscall1(SYS_NS_DERIVE, root_ns) };
    if copy <= 0 {
        return fail(b"a copy of the root could not be made");
    }
    let copy = copy as u64;
    if token(copy) != Some(first) {
        close(copy);
        return fail(b"the copy of the root did not reach the instance the root does");
    }
    let (st, h) = ns_lookup(root_ns, b"/svc/restart-probe/exit", chan);
    close(h);
    if st == 0 {
        return fail(b"asking restart-probe to exit was not refused, as the probe answers it");
    }
    // Its backoff is 100 ms. Resolve until the next instance answers, or give up — pausing
    // twenty milliseconds between tries on a timer, since a wait on no handles is refused at once.
    // SAFETY: register-only syscall; a one-shot timer this process owns.
    let timer = unsafe { syscall1(libkern::SYS_TIMER_CREATE, 0) };
    let deadline = clock_ns() + 10_000_000_000;
    let second = loop {
        match token(root_ns) {
            Some(t) if t != first => break Some(t),
            _ if clock_ns() > deadline => break None,
            _ if timer > 0 => {
                let at = clock_ns() + 20_000_000;
                let handles = [timer as u64];
                let mut results = [0u8; 24];
                // SAFETY: arming our own timer, then a one-entry wait on it, bounded besides.
                unsafe {
                    syscall4(libkern::SYS_TIMER_SET, timer as u64, at, 0, 0);
                    syscall4(
                        libkern::SYS_WAIT,
                        handles.as_ptr() as u64,
                        1,
                        results.as_mut_ptr() as u64,
                        at + 1_000_000_000,
                    );
                }
            }
            _ => {}
        }
    };
    close(if timer > 0 { timer as u64 } else { 0 });
    let Some(second) = second else {
        close(copy);
        return fail(b"no new instance answered at /svc/restart-probe within 10 s of the old one exiting");
    };
    let in_copy = token(copy);
    close(copy);
    if in_copy != Some(second) {
        return fail(b"the copy of the root, made before the restart, did not reach the new instance");
    }
    kprint(b"boot-probe: restart: a server that exited was restarted, and its path reached the new instance ok\n");
    kprint(b"boot-probe: restart: a namespace bound before the restart reached it too ok\n");
    true
}

/// **Starting, stopping and restarting services**, on an admin session opened as the view
/// broker's `services` grant opens one: the endpoint `/svc/services/admin-endpoint` mints, bound
/// in a namespace the probe builds, and resolved there.
/// 1. `/svc/services/all.tsm` lists the declarations, `clipboard-server` running.
/// 2. **A stop is answered once it has happened**: the table then says `stopped`, and
///    `/dev/clipboard` answers `NotFound` — its registry entry went with the server.
/// 3. A start and a restart are answered once the server is up, `/dev/clipboard` opens again,
///    and the table counts the restart.
/// 4. The refusals: an essential service's stop and restart, a name nothing declares, a start of a
///    running service.
/// 5. **A restart whose session closes at once still starts the service again**: only the
///    answer is lost with it.
pub(super) fn services_test(root_ns: u64) -> bool {
    use libkern::{KError, SYS_NS_BIND, SYS_NS_CREATE};
    use librsproto::services::{OP_SERVICES_RESTART, OP_SERVICES_START, OP_SERVICES_STOP};
    use libstream::wire::{Table, Value};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: services: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT | libkern::RIGHT_DUPLICATE;
    let row = |name: &str| -> Option<(alloc::string::String, i64)> {
        let (st, h) = ns_lookup(root_ns, b"/svc/services/all.tsm", RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
        let bytes = if st == 0 { read_all(h) } else { None };
        close(h);
        let t = Table::decode(&bytes?).ok()?;
        let col = |n: &str| t.schema.fields.iter().position(|f| f.name == n);
        let (cn, cs, cr) = (col("name")?, col("state")?, col("restarts")?);
        let r = t.rows.iter().find(|r| r[cn] == Value::Str(alloc::string::String::from(name)))?;
        match (&r[cs], &r[cr]) {
            (Value::Str(s), Value::Int(n)) => Some((s.clone(), *n)),
            _ => None,
        }
    };
    let state = |name: &str| row(name).map(|r| r.0);
    let clipboard_opens = || {
        let (st, h) = ns_lookup(root_ns, b"/dev/clipboard", chan);
        close(h);
        st
    };

    if state("clipboard-server").as_deref() != Some("running") || state("auth-service").is_none() {
        return fail(b"all.tsm does not list clipboard-server running, and auth-service");
    }

    let (st, endpoint) = ns_lookup(root_ns, b"/svc/services/admin-endpoint", chan);
    if st != 0 || endpoint == 0 {
        return fail(b"no admin endpoint at /svc/services/admin-endpoint");
    }
    // SAFETY: register-only syscall (its argument is unused); returns a fresh namespace handle.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns <= 0 {
        close(endpoint);
        return fail(b"no namespace to bind the admin endpoint into");
    }
    let ns = ns as u64;
    let at = b"/dev/services/admin";
    // SAFETY: a namespace this process created, a valid path, and an endpoint it holds.
    let bound = unsafe { syscall6(SYS_NS_BIND, ns, at.as_ptr() as u64, at.len() as u64, endpoint, 0, 0) };
    close(endpoint);
    let (st, admin) = if bound == 0 { ns_lookup(ns, at, chan) } else { (-1, 0) };
    if st != 0 || admin == 0 {
        close(ns);
        return fail(b"/dev/services/admin opened no admin session");
    }
    let mut next = 0u64;
    let mut ask = |op: u16, body: &[u8]| -> Option<Received> {
        next += 1;
        if !rs_send(admin, op, next, body, &[]) {
            return None;
        }
        let deadline = clock_ns() + 40_000_000_000;
        loop {
            let m = receive(admin, deadline)?;
            m.handles.iter().for_each(|&h| close(h));
            if m.request_id == next {
                return Some(m);
            }
        }
    };
    let refused = |m: &Option<Received>, err: KError| {
        m.as_ref().is_some_and(|m| {
            m.error && librsproto::error::parse_error(&m.body).is_some_and(|e| e.kerror == err.as_i32())
        })
    };
    let answered = |m: &Option<Received>| m.as_ref().is_some_and(|m| !m.error);
    let finish = |ok: bool| {
        close(admin);
        close(ns);
        ok
    };

    if !answered(&ask(OP_SERVICES_STOP, b"clipboard-server")) {
        return finish(fail(b"a stop of clipboard-server was not answered"));
    }
    if state("clipboard-server").as_deref() != Some("stopped") {
        return finish(fail(b"clipboard-server stopped, and the table does not say so"));
    }
    if clipboard_opens() != KError::NotFound.as_i32() {
        return finish(fail(b"/dev/clipboard did not answer NotFound with its server stopped"));
    }
    if !answered(&ask(OP_SERVICES_START, b"clipboard-server")) {
        return finish(fail(b"a start of clipboard-server was not answered"));
    }
    if state("clipboard-server").as_deref() != Some("running") || clipboard_opens() != 0 {
        return finish(fail(b"clipboard-server started, and is not running at /dev/clipboard"));
    }
    let before = row("clipboard-server").map_or(-1, |r| r.1);
    if !answered(&ask(OP_SERVICES_RESTART, b"clipboard-server")) {
        return finish(fail(b"a restart of clipboard-server was not answered"));
    }
    let after = row("clipboard-server");
    if after.as_ref().map(|r| (r.0.as_str(), r.1)) != Some(("running", before + 1)) || clipboard_opens() != 0 {
        return finish(fail(b"clipboard-server restarted, and is not running, counted once, at /dev/clipboard"));
    }
    if !refused(&ask(OP_SERVICES_STOP, b"auth-service"), KError::NoAccess)
        || !refused(&ask(OP_SERVICES_RESTART, b"auth-service"), KError::NoAccess)
    {
        return finish(fail(b"an essential service's stop or restart was not refused NoAccess"));
    }
    if !refused(&ask(OP_SERVICES_STOP, b"no-such-service"), KError::NotFound) {
        return finish(fail(b"a stop of a name nothing declares was not refused NotFound"));
    }
    if !refused(&ask(OP_SERVICES_START, b"clipboard-server"), KError::AlreadyExists) {
        return finish(fail(b"a start of a running service was not refused AlreadyExists"));
    }
    if state("auth-service").as_deref() != Some("running") {
        return finish(fail(b"auth-service is not running after its refused stop"));
    }
    // 6. A restart asked on a session that closes before its answer. The request and the close are
    //    handled in one pass: the kernel hands out what is queued before it reports the peer gone.
    let before = row("clipboard-server").map_or(-1, |r| r.1);
    let (st, second) = ns_lookup(ns, at, chan);
    if st != 0 || second == 0 || !rs_send(second, OP_SERVICES_RESTART, 1, b"clipboard-server", &[]) {
        close(second);
        return finish(fail(b"a second admin session would not take a restart"));
    }
    close(second);
    // Twenty milliseconds between reads, on a timer: a wait on no handles is refused at once.
    // SAFETY: register-only syscall; a one-shot timer this process owns.
    let timer = unsafe { syscall1(libkern::SYS_TIMER_CREATE, 0) };
    let deadline = clock_ns() + 15_000_000_000;
    let restarted = loop {
        match row("clipboard-server") {
            Some((s, n)) if s == "running" && n == before + 1 => break true,
            _ if clock_ns() > deadline => break false,
            _ if timer > 0 => {
                let at = clock_ns() + 20_000_000;
                let handles = [timer as u64];
                let mut results = [0u8; 24];
                // SAFETY: arming our own timer, then a one-entry wait on it, bounded besides.
                unsafe {
                    syscall4(libkern::SYS_TIMER_SET, timer as u64, at, 0, 0);
                    syscall4(
                        libkern::SYS_WAIT,
                        handles.as_ptr() as u64,
                        1,
                        results.as_mut_ptr() as u64,
                        at + 1_000_000_000,
                    );
                }
            }
            _ => {}
        }
    };
    close(if timer > 0 { timer as u64 } else { 0 });
    if !restarted {
        return finish(fail(b"a restart whose session closed at once did not start clipboard-server again"));
    }
    kprint(
        b"boot-probe: services: listed, clipboard-server stopped, started and restarted, each answered once done ok\n",
    );
    kprint(b"boot-probe: services: an essential stop, an unknown name and a second start refused ok\n");
    kprint(b"boot-probe: services: a restart whose session closed at once still started it again ok\n");
    finish(true)
}

/// **The power endpoint answers nothing but a well-formed shutdown**, on a session opened as the
/// view broker's `power` grant opens one, and an admin session answers no shutdown at all. A
/// well-formed one on a power session would stop this machine, so sending it is
/// `check-shutdown`'s:
/// - `Start` on a power session, refused `NoAccess`;
/// - `Shutdown` with no body, two bytes, or a byte that names neither halt nor reboot, refused
///   `InvalidArgument`;
/// - `Shutdown` on an admin session, refused `NoAccess`;
/// - a third power endpoint while two are held, refused `WouldBlock`, and another taken at once
///   when one is let go, which is `serve_services` answering closes before resolves. **The view
///   broker holds the first** by then: `view_broker_test` has had it build `admin` views, whose
///   `power` grant takes one for the boot.
pub(super) fn power_endpoint_test(root_ns: u64) -> bool {
    use libkern::{KError, SYS_NS_BIND, SYS_NS_CREATE};
    use librsproto::services::{OP_SERVICES_SHUTDOWN, OP_SERVICES_START, SHUTDOWN_HALT, SHUTDOWN_REBOOT};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: power: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT | libkern::RIGHT_DUPLICATE;
    // A session on the endpoint `path` mints, as a grant's binding reaches it: bound in a namespace
    // of this probe's own, and resolved there. The namespace holds the endpoint until it is closed.
    let open = |path: &[u8]| -> Option<(u64, u64)> {
        let (st, endpoint) = ns_lookup(root_ns, path, chan);
        if st != 0 || endpoint == 0 {
            return None;
        }
        // SAFETY: register-only syscall (its argument is unused); returns a fresh namespace handle.
        let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
        if ns <= 0 {
            close(endpoint);
            return None;
        }
        let ns = ns as u64;
        let at = b"/grant";
        // SAFETY: a namespace this process created, a valid path, and an endpoint it holds.
        let bound = unsafe { syscall6(SYS_NS_BIND, ns, at.as_ptr() as u64, at.len() as u64, endpoint, 0, 0) };
        close(endpoint);
        let (st, session) = if bound == 0 { ns_lookup(ns, at, chan) } else { (-1, 0) };
        if st != 0 || session == 0 {
            close(ns);
            return None;
        }
        Some((ns, session))
    };
    let ask = |session: u64, id: u64, op: u16, body: &[u8]| -> Option<i32> {
        if !rs_send(session, op, id, body, &[]) {
            return None;
        }
        let deadline = clock_ns() + 10_000_000_000;
        loop {
            let m = receive(session, deadline)?;
            m.handles.iter().for_each(|&h| close(h));
            if m.request_id == id {
                return Some(if m.error {
                    librsproto::error::parse_error(&m.body).map_or(-1, |e| e.kerror)
                } else {
                    0
                });
            }
        }
    };
    let Some((power_ns, power)) = open(b"/svc/services/power-endpoint") else {
        return fail(b"no power session through /svc/services/power-endpoint");
    };
    let Some((admin_ns, admin)) = open(b"/svc/services/admin-endpoint") else {
        close(power);
        close(power_ns);
        return fail(b"no admin session through /svc/services/admin-endpoint");
    };
    let (no_access, invalid) = (Some(KError::NoAccess.as_i32()), Some(KError::InvalidArgument.as_i32()));
    let refusals: [(Option<i32>, Option<i32>, &[u8]); 5] = [
        (ask(power, 1, OP_SERVICES_START, b"clipboard-server"), no_access, b"a start on a power session"),
        (ask(power, 2, OP_SERVICES_SHUTDOWN, &[]), invalid, b"a shutdown with no body"),
        (ask(power, 3, OP_SERVICES_SHUTDOWN, &[SHUTDOWN_HALT, SHUTDOWN_REBOOT]), invalid, b"a shutdown with two bytes"),
        (ask(power, 4, OP_SERVICES_SHUTDOWN, &[2]), invalid, b"a shutdown naming neither halt nor reboot"),
        (ask(admin, 5, OP_SERVICES_SHUTDOWN, &[SHUTDOWN_HALT]), no_access, b"a shutdown on an admin session"),
    ];
    // Two power endpoints are held now: the broker's, and this probe's, by `power_ns`'s binding.
    let (second, extra) = ns_lookup(root_ns, b"/svc/services/power-endpoint", chan);
    close(extra);
    for h in [power, power_ns, admin, admin_ns] {
        close(h);
    }
    let (again, taken) = ns_lookup(root_ns, b"/svc/services/power-endpoint", chan);
    close(taken);
    for (answer, expected, what) in refusals {
        if answer != expected {
            Line::new().s(b"boot-probe: power: ").s(what).s(b" was answered ").i(answer.unwrap_or(1) as i64).end();
            return fail(b"a request was not refused as it should be");
        }
    }
    if second != KError::WouldBlock.as_i32() {
        return fail(b"a third power endpoint was not refused while the broker's and this probe's were held");
    }
    if again != 0 || taken == 0 {
        return fail(b"no power endpoint at once after the one was let go");
    }
    kprint(b"boot-probe: power endpoint refused a start, three malformed shutdowns and one on an admin session, and a third endpoint ok\n");
    true
}

/// **Reading the log back**, through a read endpoint bound as the
/// `logs` grant binds it:
/// - a record this probe writes on a log source of its own is read back, principal, source, level
///   and message as written, and stamped with the wall clock;
/// - a `Read` whose body is not twelve bytes is refused `InvalidArgument`, and an op that is not
///   `Read` `Unsupported`;
/// - **two sessions at once, and a third refused** `WouldBlock`; one let go, and another opened
///   at once — so a closed session is retired, not waited on;
/// - **two endpoints at once**, the broker's and this probe's: a third refused, and one minted
///   at once when this probe's is let go.
///
/// **After `power_endpoint_test`, and so after `storage_grant_test`**, whose admin views make the
/// broker resolve its read endpoint for the `logs` grant and keep it for the boot.
pub(super) fn logs_test(root_ns: u64) -> bool {
    use libkern::{KError, SYS_NS_BIND, SYS_NS_CREATE};
    use librsproto::log::{LEVEL_INFO, OP_LOG_READ, TIER_SYSTEM, parse_read_reply, read_request};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: logs: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT | libkern::RIGHT_DUPLICATE;
    const MESSAGE: &str = "boot-probe: a record to read back";
    liblog::open_source(root_ns, b"/log/system/boot-probe/logs-test").info(MESSAGE);

    let (st, endpoint) = ns_lookup(root_ns, b"/log/read-endpoint", chan);
    if st != 0 || endpoint == 0 {
        return fail(b"no read endpoint through /log/read-endpoint");
    }
    // SAFETY: register-only syscall (its argument is unused); returns a fresh namespace handle.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns <= 0 {
        close(endpoint);
        return fail(b"no namespace to bind the read endpoint in");
    }
    let ns = ns as u64;
    let at = b"/logs";
    // SAFETY: a namespace this process created, a valid path, and an endpoint it holds.
    let bound = unsafe { syscall6(SYS_NS_BIND, ns, at.as_ptr() as u64, at.len() as u64, endpoint, 0, 0) };
    close(endpoint);
    if bound != 0 {
        close(ns);
        return fail(b"the read endpoint did not bind");
    }
    let (st, session) = ns_lookup(ns, at, chan);
    if st != 0 || session == 0 {
        close(ns);
        return fail(b"no read session through the endpoint");
    }
    let ask = |id: u64, op: u16, body: &[u8]| -> Option<Received> {
        if !rs_send(session, op, id, body, &[]) {
            return None;
        }
        let deadline = clock_ns() + 5_000_000_000;
        loop {
            let m = receive(session, deadline)?;
            m.handles.iter().for_each(|&h| close(h));
            if m.request_id == id {
                return Some(m);
            }
        }
    };
    // **The record, read back.** The append and the `Read` arrive on different channels, so the
    // first reads may come before the record does: read on from where the last reply ended until
    // it is there, for a few seconds.
    let (mut after, mut id, mut found) = (0u64, 10u64, false);
    let deadline = clock_ns() + 5_000_000_000;
    while !found && clock_ns() < deadline {
        id += 1;
        let Some(m) = ask(id, OP_LOG_READ, &read_request(after, 0)) else {
            close(session);
            close(ns);
            return fail(b"no answer to a Read");
        };
        let Some(reply) = (!m.error).then(|| parse_read_reply(&m.body)).flatten() else {
            close(session);
            close(ns);
            return fail(b"a Read's answer did not read");
        };
        for rec in reply.records() {
            after = rec.sequence;
            found |= rec.principal == b"boot-probe"
                && rec.source == Some(b"logs-test".as_slice())
                && rec.message == MESSAGE.as_bytes()
                && rec.level == LEVEL_INFO
                && rec.tier == TIER_SYSTEM
                && rec.time.is_some();
        }
    }
    let invalid = ask(90, OP_LOG_READ, &read_request(0, 0)[..11]);
    let unsupported = ask(91, OP_LOG_READ + 1, &read_request(0, 0));
    let refused_as = |m: Option<Received>, e: KError| {
        m.is_some_and(|m| m.error && librsproto::error::parse_error(&m.body).is_some_and(|b| b.kerror == e.as_i32()))
    };
    let (short_refused, op_refused) =
        (refused_as(invalid, KError::InvalidArgument), refused_as(unsupported, KError::Unsupported));
    let (st2, second) = ns_lookup(ns, at, chan);
    let (st3, third) = ns_lookup(ns, at, chan);
    close(third);
    close(second);
    let (st4, fourth) = ns_lookup(ns, at, chan);
    close(fourth);
    close(session);
    // Two endpoints: the broker's and this probe's, by `ns`'s binding; a third refused. This
    // probe's let go, and another minted at once.
    let (e3, extra) = ns_lookup(root_ns, b"/log/read-endpoint", chan);
    close(extra);
    close(ns);
    let (e4, again) = ns_lookup(root_ns, b"/log/read-endpoint", chan);
    close(again);
    if !found {
        return fail(b"the record this probe wrote was not read back as written");
    }
    if !short_refused || !op_refused {
        return fail(b"a Read of eleven bytes, or an op that is not Read, was not refused as it should be");
    }
    if st2 != 0 || st3 != KError::WouldBlock.as_i32() || st4 != 0 {
        return fail(b"two read sessions, a third refused, and one at once after one was let go");
    }
    if e3 != KError::WouldBlock.as_i32() || e4 != 0 || again == 0 {
        // The statuses say whether the third was granted or the one after the let go was
        // refused, which the failure alone does not.
        Line::new()
            .s(b"boot-probe: logs: third endpoint status ")
            .i(e3 as i64)
            .s(b", after the let go ")
            .i(e4 as i64)
            .end();
        return fail(b"a third read endpoint refused, and one at once after this probe's was let go");
    }
    kprint(b"boot-probe: logs read a record back, refused two malformed requests, and held sessions and endpoints to two ok\n");
    true
}
