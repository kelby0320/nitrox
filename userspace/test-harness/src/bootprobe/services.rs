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
pub(super) fn restart_test(root: Root<'_>) -> bool {
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: restart: ").s(what).s(b" FAIL").end();
        false
    };
    const CHAN: Rights = Rights::from_bits(libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT);
    fn token<M: CanLookup>(ns: Ref<'_, Namespace, M>) -> Option<u64> {
        let ch = lookup::<Channel, Only, _>(ns, "/svc/restart-probe/id", CHAN).ok()?;
        let m = receive(&ch, clock_ns() + 5_000_000_000);
        drop(ch);
        let m = m?;
        Some(u64::from_le_bytes(m.body.get(..8)?.try_into().ok()?))
    }
    let Some(first) = token(root) else {
        return fail(b"/svc/restart-probe answered no token");
    };
    // **A namespace bound before the restart, and never touched again** — a session's. A copy
    // of the root holds the same binding objects, and later changes to the root do not reach it.
    let Ok(copy) = root.copy() else {
        return fail(b"a copy of the root could not be made");
    };
    if token(copy.as_ref()) != Some(first) {
        return fail(b"the copy of the root did not reach the instance the root does");
    }
    if lookup::<Channel, Only, _>(root, "/svc/restart-probe/exit", CHAN).is_ok() {
        return fail(b"asking restart-probe to exit was not refused, as the probe answers it");
    }
    // Its backoff is 100 ms. Resolve until the next instance answers, or give up.
    let deadline = clock_ns() + 10_000_000_000;
    let second = loop {
        match token(root) {
            Some(t) if t != first => break Some(t),
            _ if clock_ns() > deadline => break None,
            _ => nap(20_000_000),
        }
    };
    let Some(second) = second else {
        return fail(b"no new instance answered at /svc/restart-probe within 10 s of the old one exiting");
    };
    let in_copy = token(copy.as_ref());
    drop(copy);
    if in_copy != Some(second) {
        return fail(b"the copy of the root, made before the restart, did not reach the new instance");
    }
    log::write(b"boot-probe: restart: a server that exited was restarted, and its path reached the new instance ok\n");
    log::write(b"boot-probe: restart: a namespace bound before the restart reached it too ok\n");
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
pub(super) fn services_test(root: Root<'_>) -> bool {
    use libkern::KError;
    use librsproto::services::{OP_SERVICES_RESTART, OP_SERVICES_START, OP_SERVICES_STOP};
    use libstream::wire::{Table, Value};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: services: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT | Rights::DUPLICATE;
    let row = |name: &str| -> Option<(alloc::string::String, i64)> {
        let h = lookup::<File, MapRead, _>(root, "/svc/services/all.tsm", Rights::MAP_READ | Rights::INSPECT).ok()?;
        let t = Table::decode(&read_all(&h)?).ok()?;
        let col = |n: &str| t.schema.fields.iter().position(|f| f.name == n);
        let (cn, cs, cr) = (col("name")?, col("state")?, col("restarts")?);
        let r = t.rows.iter().find(|r| r[cn] == Value::Str(alloc::string::String::from(name)))?;
        match (&r[cs], &r[cr]) {
            (Value::Str(s), Value::Int(n)) => Some((s.clone(), *n)),
            _ => None,
        }
    };
    let state = |name: &str| row(name).map(|r| r.0);
    let clipboard_opens = || status(&lookup::<Channel, Only, _>(root, "/dev/clipboard", chan));

    if state("clipboard-server").as_deref() != Some("running") || state("auth-service").is_none() {
        return fail(b"all.tsm does not list clipboard-server running, and auth-service");
    }

    let Ok(endpoint) = lookup::<Channel, Only, _>(root, "/svc/services/admin-endpoint", chan) else {
        return fail(b"no admin endpoint at /svc/services/admin-endpoint");
    };
    let Ok(ns) = Handle::<Namespace, NsMutable>::create() else {
        return fail(b"no namespace to bind the admin endpoint into");
    };
    let at = "/dev/services/admin";
    let bound = ns.bind_server(at, endpoint.as_ref(), "");
    drop(endpoint);
    let admin = if bound.is_ok() { lookup::<Channel, Only, _>(&ns, at, chan).ok() } else { None };
    let Some(admin) = admin else {
        return fail(b"/dev/services/admin opened no admin session");
    };
    let mut next = 0u64;
    let mut ask = |op: u16, body: &[u8]| -> Option<Received> {
        next += 1;
        if !rs_send(&admin, op, next, body, Carried::new()) {
            return None;
        }
        let deadline = clock_ns() + 40_000_000_000;
        loop {
            let m = receive(&admin, deadline)?;
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

    if !answered(&ask(OP_SERVICES_STOP, b"clipboard-server")) {
        return fail(b"a stop of clipboard-server was not answered");
    }
    if state("clipboard-server").as_deref() != Some("stopped") {
        return fail(b"clipboard-server stopped, and the table does not say so");
    }
    if clipboard_opens() != KError::NotFound.as_i32() {
        return fail(b"/dev/clipboard did not answer NotFound with its server stopped");
    }
    if !answered(&ask(OP_SERVICES_START, b"clipboard-server")) {
        return fail(b"a start of clipboard-server was not answered");
    }
    if state("clipboard-server").as_deref() != Some("running") || clipboard_opens() != 0 {
        return fail(b"clipboard-server started, and is not running at /dev/clipboard");
    }
    let before = row("clipboard-server").map_or(-1, |r| r.1);
    if !answered(&ask(OP_SERVICES_RESTART, b"clipboard-server")) {
        return fail(b"a restart of clipboard-server was not answered");
    }
    let after = row("clipboard-server");
    if after.as_ref().map(|r| (r.0.as_str(), r.1)) != Some(("running", before + 1)) || clipboard_opens() != 0 {
        return fail(b"clipboard-server restarted, and is not running, counted once, at /dev/clipboard");
    }
    if !refused(&ask(OP_SERVICES_STOP, b"auth-service"), KError::NoAccess)
        || !refused(&ask(OP_SERVICES_RESTART, b"auth-service"), KError::NoAccess)
    {
        return fail(b"an essential service's stop or restart was not refused NoAccess");
    }
    if !refused(&ask(OP_SERVICES_STOP, b"no-such-service"), KError::NotFound) {
        return fail(b"a stop of a name nothing declares was not refused NotFound");
    }
    if !refused(&ask(OP_SERVICES_START, b"clipboard-server"), KError::AlreadyExists) {
        return fail(b"a start of a running service was not refused AlreadyExists");
    }
    if state("auth-service").as_deref() != Some("running") {
        return fail(b"auth-service is not running after its refused stop");
    }
    // 6. A restart asked on a session that closes before its answer. The request and the close are
    //    handled in one pass: the kernel hands out what is queued before it reports the peer gone.
    let before = row("clipboard-server").map_or(-1, |r| r.1);
    let sent = lookup::<Channel, Only, _>(&ns, at, chan)
        .is_ok_and(|second| rs_send(&second, OP_SERVICES_RESTART, 1, b"clipboard-server", Carried::new()));
    if !sent {
        return fail(b"a second admin session would not take a restart");
    }
    let deadline = clock_ns() + 15_000_000_000;
    let restarted = loop {
        match row("clipboard-server") {
            Some((s, n)) if s == "running" && n == before + 1 => break true,
            _ if clock_ns() > deadline => break false,
            _ => nap(20_000_000),
        }
    };
    if !restarted {
        return fail(b"a restart whose session closed at once did not start clipboard-server again");
    }
    log::write(
        b"boot-probe: services: listed, clipboard-server stopped, started and restarted, each answered once done ok\n",
    );
    log::write(b"boot-probe: services: an essential stop, an unknown name and a second start refused ok\n");
    log::write(b"boot-probe: services: a restart whose session closed at once still started it again ok\n");
    true
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
pub(super) fn power_endpoint_test(root: Root<'_>) -> bool {
    use libkern::KError;
    use librsproto::services::{OP_SERVICES_SHUTDOWN, OP_SERVICES_START, SHUTDOWN_HALT, SHUTDOWN_REBOOT};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: power: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT | Rights::DUPLICATE;
    // A session on the endpoint `path` mints, as a grant's binding reaches it: bound in a namespace
    // of this probe's own, and resolved there. The namespace holds the endpoint until it is closed.
    let open = |path: &str| -> Option<(Handle<Namespace, NsMutable>, Handle<Channel, Only>)> {
        let endpoint = lookup::<Channel, Only, _>(root, path, chan).ok()?;
        let ns = Handle::<Namespace, NsMutable>::create().ok()?;
        let at = "/grant";
        let bound = ns.bind_server(at, endpoint.as_ref(), "");
        drop(endpoint);
        let session = if bound.is_ok() { lookup::<Channel, Only, _>(&ns, at, chan).ok() } else { None };
        Some((ns, session?))
    };
    let ask = |session: &Handle<Channel, Only>, id: u64, op: u16, body: &[u8]| -> Option<i32> {
        if !rs_send(session, op, id, body, Carried::new()) {
            return None;
        }
        let deadline = clock_ns() + 10_000_000_000;
        loop {
            let m = receive(session, deadline)?;
            if m.request_id == id {
                return Some(if m.error {
                    librsproto::error::parse_error(&m.body).map_or(-1, |e| e.kerror)
                } else {
                    0
                });
            }
        }
    };
    let Some((power_ns, power)) = open("/svc/services/power-endpoint") else {
        return fail(b"no power session through /svc/services/power-endpoint");
    };
    let Some((admin_ns, admin)) = open("/svc/services/admin-endpoint") else {
        return fail(b"no admin session through /svc/services/admin-endpoint");
    };
    let (no_access, invalid) = (Some(KError::NoAccess.as_i32()), Some(KError::InvalidArgument.as_i32()));
    let refusals: [(Option<i32>, Option<i32>, &[u8]); 5] = [
        (ask(&power, 1, OP_SERVICES_START, b"clipboard-server"), no_access, b"a start on a power session"),
        (ask(&power, 2, OP_SERVICES_SHUTDOWN, &[]), invalid, b"a shutdown with no body"),
        (
            ask(&power, 3, OP_SERVICES_SHUTDOWN, &[SHUTDOWN_HALT, SHUTDOWN_REBOOT]),
            invalid,
            b"a shutdown with two bytes",
        ),
        (ask(&power, 4, OP_SERVICES_SHUTDOWN, &[2]), invalid, b"a shutdown naming neither halt nor reboot"),
        (ask(&admin, 5, OP_SERVICES_SHUTDOWN, &[SHUTDOWN_HALT]), no_access, b"a shutdown on an admin session"),
    ];
    // Two power endpoints are held now: the broker's, and this probe's, by `power_ns`'s binding.
    let second = status(&lookup::<Channel, Only, _>(root, "/svc/services/power-endpoint", chan));
    drop(power);
    drop(power_ns);
    drop(admin);
    drop(admin_ns);
    let again = status(&lookup::<Channel, Only, _>(root, "/svc/services/power-endpoint", chan));
    for (answer, expected, what) in refusals {
        if answer != expected {
            Line::new().s(b"boot-probe: power: ").s(what).s(b" was answered ").i(answer.unwrap_or(1) as i64).end();
            return fail(b"a request was not refused as it should be");
        }
    }
    if second != KError::WouldBlock.as_i32() {
        return fail(b"a third power endpoint was not refused while the broker's and this probe's were held");
    }
    if again != 0 {
        return fail(b"no power endpoint at once after the one was let go");
    }
    log::write(b"boot-probe: power endpoint refused a start, three malformed shutdowns and one on an admin session, and a third endpoint ok\n");
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
pub(super) fn logs_test(root: Root<'_>) -> bool {
    use libkern::KError;
    use librsproto::log::{LEVEL_INFO, OP_LOG_READ, TIER_SYSTEM, parse_read_reply, read_request};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: logs: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT | Rights::DUPLICATE;
    const MESSAGE: &str = "boot-probe: a record to read back";
    liblog::Logger::open(root, "/log/system/boot-probe/logs-test").info(MESSAGE);

    let Ok(endpoint) = lookup::<Channel, Only, _>(root, "/log/read-endpoint", chan) else {
        return fail(b"no read endpoint through /log/read-endpoint");
    };
    let Ok(ns) = Handle::<Namespace, NsMutable>::create() else {
        return fail(b"no namespace to bind the read endpoint in");
    };
    let at = "/logs";
    let bound = ns.bind_server(at, endpoint.as_ref(), "");
    drop(endpoint);
    if bound.is_err() {
        return fail(b"the read endpoint did not bind");
    }
    let Ok(session) = lookup::<Channel, Only, _>(&ns, at, chan) else {
        return fail(b"no read session through the endpoint");
    };
    let ask = |id: u64, op: u16, body: &[u8]| -> Option<Received> {
        if !rs_send(&session, op, id, body, Carried::new()) {
            return None;
        }
        let deadline = clock_ns() + 5_000_000_000;
        loop {
            let m = receive(&session, deadline)?;
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
            return fail(b"no answer to a Read");
        };
        let Some(reply) = (!m.error).then(|| parse_read_reply(&m.body)).flatten() else {
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
    let second = lookup::<Channel, Only, _>(&ns, at, chan);
    let third = lookup::<Channel, Only, _>(&ns, at, chan);
    let (st2, st3) = (status(&second), status(&third));
    drop(third);
    drop(second);
    let st4 = status(&lookup::<Channel, Only, _>(&ns, at, chan));
    drop(session);
    // Two endpoints: the broker's and this probe's, by `ns`'s binding; a third refused. This
    // probe's let go, and another minted at once.
    let e3 = status(&lookup::<Channel, Only, _>(root, "/log/read-endpoint", chan));
    drop(ns);
    let e4 = status(&lookup::<Channel, Only, _>(root, "/log/read-endpoint", chan));
    if !found {
        return fail(b"the record this probe wrote was not read back as written");
    }
    if !short_refused || !op_refused {
        return fail(b"a Read of eleven bytes, or an op that is not Read, was not refused as it should be");
    }
    if st2 != 0 || st3 != KError::WouldBlock.as_i32() || st4 != 0 {
        return fail(b"two read sessions, a third refused, and one at once after one was let go");
    }
    if e3 != KError::WouldBlock.as_i32() || e4 != 0 {
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
    log::write(b"boot-probe: logs read a record back, refused two malformed requests, and held sessions and endpoints to two ok\n");
    true
}
