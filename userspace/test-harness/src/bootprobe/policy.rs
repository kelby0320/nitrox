//! The policy and grace-period tests.

use super::*;

/// **A remembered password, and everything that must not use it**.
///
/// A session opened as a supervisor does, a real terminal from `/dev/tty`, and the build's fixture
/// password open a window for that terminal and `admin`. Then, in order:
/// - a second real token from the same terminal starts without asking;
/// - **each of these is asked**: a made-up token (the forgery control), a token already
///   redeemed, no token, another view, and a token from a terminal on a backend of its own,
///   attached as `nxterm` attaches a window's;
/// - **a real token still starts without asking**, so none of those was asked because the
///   window had closed;
/// - after `Forget`, a real token is asked.
///
/// None of the requests that are asked is answered: a refused password would forget the window,
/// and then the refusals after it would prove nothing.
pub(super) fn grace_test(root_ns: u64) -> bool {
    use librsproto::views::*;
    use librsproto::{OP_TTY_ATTACH_BACKEND, OP_TTY_TOKEN, TTY_TOKEN_LEN};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: grace: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let mut exited = alloc::vec::Vec::new();
    let (st, sup) = ns_lookup(root_ns, b"/svc/views/session", chan);
    if st != 0 || sup == 0 {
        return fail(b"no supervisor channel at /svc/views/session");
    }
    let session = match views_call(sup, OP_VIEWS_OPEN_SESSION, 1, DEMO_USER, &[], &mut exited) {
        Some((false, body)) => parse_session_id(&body),
        _ => None,
    };
    let Some(session) = session else {
        close(sup);
        return fail(b"OpenSession");
    };
    let (tst, tty) = ns_lookup(root_ns, b"/dev/tty", chan);
    if tst != 0 || tty == 0 {
        close(sup);
        return fail(b"no terminal at /dev/tty");
    }
    let token = |t: u64| -> Option<[u8; TTY_TOKEN_LEN]> {
        if !rs_send(t, OP_TTY_TOKEN, 0x7470, &[], &[]) {
            return None;
        }
        let deadline = clock_ns() + 5_000_000_000;
        loop {
            let m = receive(t, deadline)?;
            if m.op == OP_TTY_TOKEN && m.request_id == 0x7470 {
                return if m.error { None } else { m.body.as_slice().try_into().ok() };
            }
        }
    };
    // A request for `program` in `view`, carrying `token`, on a client channel of its own: the
    // outcome, and the channel, still open.
    let ask = |view: &[u8],
               program: &[u8],
               args: &[&[u8]],
               token: Option<[u8; TTY_TOKEN_LEN]>,
               exited: &mut alloc::vec::Vec<alloc::vec::Vec<u8>>|
     -> Option<(Outcome, u64)> {
        let (cst, cli) = ns_lookup(root_ns, alloc::format!("/svc/views/s/{session}").as_bytes(), chan);
        if cst != 0 || cli == 0 {
            return None;
        }
        // SAFETY: a namespace handle this process holds.
        let copy = unsafe { syscall1(SYS_NS_DERIVE, root_ns) };
        if copy <= 0 {
            close(cli);
            return None;
        }
        let mut req = [0u8; 256];
        let n = build_request(&mut req, 0, view, program, args, b"", token.as_ref())?;
        match views_call(cli, OP_VIEWS_REQUEST, 2, &req[..n], &[copy as u64], exited) {
            Some((false, body)) => parse_outcome(&body).map(|(o, _)| (o, cli)),
            _ => {
                close(cli);
                None
            }
        }
    };
    let finish = |ok: bool, extra: &[u64]| {
        let mut id = [0u8; 8];
        let n = build_session_id(&mut id, session).unwrap_or(0);
        let _ = views_call(sup, OP_VIEWS_CLOSE_SESSION, 9, &id[..n], &[], &mut alloc::vec::Vec::new());
        for &h in extra {
            close(h);
        }
        close(tty);
        close(sup);
        ok
    };
    let sleep: &[&[u8]] = &[b"0"];
    let started = |o: Option<(Outcome, u64)>, exited: &mut alloc::vec::Vec<alloc::vec::Vec<u8>>| -> bool {
        match o {
            Some((Outcome::Started, cli)) => {
                if exited.pop().is_none() {
                    let _ = views_receive(cli, 0, exited);
                }
                close(cli);
                true
            }
            Some((_, cli)) => {
                close(cli);
                false
            }
            None => false,
        }
    };
    let asked = |o: Option<(Outcome, u64)>| -> bool {
        match o {
            Some((Outcome::NeedPassword, cli)) => {
                close(cli);
                true
            }
            Some((_, cli)) => {
                close(cli);
                false
            }
            None => false,
        }
    };

    let Some(first) = token(tty) else {
        return finish(fail(b"the terminal gave no token"), &[]);
    };
    let opened = match ask(b"admin", b"sleep", sleep, Some(first), &mut exited) {
        Some((Outcome::NeedPassword, cli)) => {
            let ok = matches!(
                views_call(cli, OP_VIEWS_PASSWORD, 3, DEMO_PASSWORD, &[], &mut exited),
                Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::Started, _)))
            );
            started(Some((Outcome::Started, cli)), &mut exited) && ok
        }
        other => {
            let _ = asked(other);
            false
        }
    };
    if !opened {
        return finish(fail(b"a first request with a token was not asked and then started"), &[]);
    }
    let Some(second) = token(tty) else {
        return finish(fail(b"no second token"), &[]);
    };
    if !started(ask(b"admin", b"sleep", sleep, Some(second), &mut exited), &mut exited) {
        return finish(fail(b"a second request from the same terminal, inside the window, was asked"), &[]);
    }
    // 3. **The forgery control**: a token nobody minted.
    let forged = [0x5a; TTY_TOKEN_LEN];
    if !asked(ask(b"admin", b"sleep", sleep, Some(forged), &mut exited)) {
        return finish(fail(b"a made-up token was not asked"), &[]);
    }
    if !asked(ask(b"admin", b"sleep", sleep, Some(second), &mut exited)) {
        return finish(fail(b"a token used twice was not asked"), &[]);
    }
    if !asked(ask(b"admin", b"sleep", sleep, None, &mut exited)) {
        return finish(fail(b"a request with no token was not asked"), &[]);
    }
    // 6. Another view: the window is `admin`'s.
    let Some(other_view) = token(tty) else {
        return finish(fail(b"no token for the other view"), &[]);
    };
    if !asked(ask(b"install", b"nxinstall", &[], Some(other_view), &mut exited)) {
        return finish(fail(b"another view, from the same terminal, was not asked"), &[]);
    }
    // 7. **Another terminal**: one with a backend of its own, as a window has.
    let (wst, window) = ns_lookup(root_ns, b"/dev/tty", chan);
    let (mut ours, mut theirs) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let made =
        unsafe { syscall4(libkern::SYS_CHANNEL_CREATE, (&raw mut ours) as u64, (&raw mut theirs) as u64, 4, 0) } == 0;
    if wst != 0 || window == 0 || !made {
        return finish(fail(b"a second terminal and a backend channel"), &[window, ours, theirs]);
    }
    let attached = matches!(
        rs_send(window, OP_TTY_ATTACH_BACKEND, 0x6174, &[], &[theirs]).then(|| {
            let deadline = clock_ns() + 5_000_000_000;
            loop {
                match receive(window, deadline) {
                    Some(m) if m.request_id == 0x6174 => break Some(m.error),
                    Some(_) => continue,
                    None => break None,
                }
            }
        }),
        Some(Some(false))
    );
    if !attached {
        return finish(fail(b"the second terminal would not take a backend"), &[window, ours]);
    }
    let Some(elsewhere) = token(window) else {
        return finish(fail(b"no token from the second terminal"), &[window, ours]);
    };
    if !asked(ask(b"admin", b"sleep", sleep, Some(elsewhere), &mut exited)) {
        return finish(fail(b"a token from another terminal was not asked"), &[window, ours]);
    }
    // 8. **And the window was open all along**: a real token from the first terminal still starts.
    let Some(still) = token(tty) else {
        return finish(fail(b"no token after the refusals"), &[window, ours]);
    };
    if !started(ask(b"admin", b"sleep", sleep, Some(still), &mut exited), &mut exited) {
        return finish(
            fail(b"the window closed while the refusals were asked, so they prove nothing"),
            &[window, ours],
        );
    }
    // 8b. **A refused password forgets every window of the session** : a request from
    //     the other terminal, which has no window and so is asked, is answered wrong, and the
    //     first terminal's window is gone with it, so a real token from there is asked too.
    let Some(other) = token(window) else {
        return finish(fail(b"no token from the second terminal for the wrong password"), &[window, ours]);
    };
    let refused = match ask(b"admin", b"sleep", sleep, Some(other), &mut exited) {
        Some((Outcome::NeedPassword, cli)) => {
            let denied = matches!(
                views_call(cli, OP_VIEWS_PASSWORD, 6, b"not the password", &[], &mut exited),
                Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::Denied { .. }, _)))
            );
            close(cli);
            denied
        }
        other => {
            let _ = asked(other);
            false
        }
    };
    if !refused {
        return finish(fail(b"a wrong password at the second terminal was not refused"), &[window, ours]);
    }
    let Some(after_wrong) = token(tty) else {
        return finish(fail(b"no token after the wrong password"), &[window, ours]);
    };
    if !asked(ask(b"admin", b"sleep", sleep, Some(after_wrong), &mut exited)) {
        return finish(
            fail(b"a refused password at another terminal left this terminal's window open"),
            &[window, ours],
        );
    }
    // **Opened again**, so that `Forget` below has a window to forget — and shown to be open.
    let Some(again) = token(tty) else {
        return finish(fail(b"no token to open the window again"), &[window, ours]);
    };
    let reopened = match ask(b"admin", b"sleep", sleep, Some(again), &mut exited) {
        Some((Outcome::NeedPassword, cli)) => {
            let ok = matches!(
                views_call(cli, OP_VIEWS_PASSWORD, 7, DEMO_PASSWORD, &[], &mut exited),
                Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::Started, _)))
            );
            started(Some((Outcome::Started, cli)), &mut exited) && ok
        }
        other => {
            let _ = asked(other);
            false
        }
    };
    let Some(open) = token(tty) else {
        return finish(fail(b"no token to show the window open again"), &[window, ours]);
    };
    if !reopened || !started(ask(b"admin", b"sleep", sleep, Some(open), &mut exited), &mut exited) {
        return finish(fail(b"the window would not open again after the wrong password"), &[window, ours]);
    }
    let (fst, fcli) = ns_lookup(root_ns, alloc::format!("/svc/views/s/{session}").as_bytes(), chan);
    let forgot = fst == 0 && matches!(views_call(fcli, OP_VIEWS_FORGET, 4, &[], &[], &mut exited), Some((false, _)));
    close(fcli);
    if !forgot {
        return finish(fail(b"Forget"), &[window, ours]);
    }
    let Some(after) = token(tty) else {
        return finish(fail(b"no token after Forget"), &[window, ours]);
    };
    if !asked(ask(b"admin", b"sleep", sleep, Some(after), &mut exited)) {
        return finish(fail(b"a request after Forget was not asked"), &[window, ours]);
    }
    // 10. **A refused token names no terminal, so a password typed with one opens no window.** A
    //     broker that gave every refused token the same terminal — reading a refusal as an id, or
    //     falling back to a default — would open a window under it here, and the next made-up
    //     token would use it.
    let opened_none = match ask(b"admin", b"sleep", sleep, Some([0x11; TTY_TOKEN_LEN]), &mut exited) {
        Some((Outcome::NeedPassword, cli)) => {
            let ok = matches!(
                views_call(cli, OP_VIEWS_PASSWORD, 5, DEMO_PASSWORD, &[], &mut exited),
                Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::Started, _)))
            );
            started(Some((Outcome::Started, cli)), &mut exited) && ok
        }
        other => {
            let _ = asked(other);
            false
        }
    };
    if !opened_none {
        return finish(fail(b"a made-up token answered with the password did not start"), &[window, ours]);
    }
    if !asked(ask(b"admin", b"sleep", sleep, Some([0x22; TTY_TOKEN_LEN]), &mut exited)) {
        return finish(fail(b"a made-up token used a window another made-up token's password opened"), &[window, ours]);
    }
    kprint(
        b"boot-probe: grace: a password remembered for one terminal and view; a made-up token, a reused one, \
          none, another view and another terminal all asked; the window open throughout; a wrong password \
          elsewhere closed it; asked again after Forget; and a made-up token's password opened no window ok\n",
    );
    finish(true, &[window, ours])
}

/// **The policy endpoint: `Show` and `Install`, and what an administrator is**.
///
/// Asked on the channel the `views` grant's `/dev/policy` resolves to — reached here as
/// `/svc/views/policy/<id>`, the root namespace's boundary (`TODO(svc-auth-ungated)`), for a
/// session the probe opens as a supervisor does.
/// - `Show` is the file as the device holds it.
/// - **Two policies no existing account could administer are refused**, by `Check` and by
///   `Install` alike, and the file stays as it was: one whose `admin` profile has lost `views`,
///   and one naming only an account that does not exist.
/// - A policy with one more view is installed, `Show` and the device hold it, and `List` names the
///   new view. The original is then installed again.
/// - A closed session's policy channel is closed with it, and a session that is not open has no
///   policy endpoint.
pub(super) fn policy_test(root_ns: u64) -> bool {
    use libkern::KError;
    use librsproto::views::*;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: policy: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let mut exited = alloc::vec::Vec::new();
    let (st, sup) = ns_lookup(root_ns, b"/svc/views/session", chan);
    if st != 0 || sup == 0 {
        return fail(b"no supervisor channel at /svc/views/session");
    }
    let session = match views_call(sup, OP_VIEWS_OPEN_SESSION, 1, DEMO_USER, &[], &mut exited) {
        Some((false, body)) => parse_session_id(&body),
        _ => None,
    };
    let Some(session) = session else {
        close(sup);
        return fail(b"OpenSession");
    };
    let close_session = |sup: u64| {
        let mut id = [0u8; 8];
        let n = build_session_id(&mut id, session).unwrap_or(0);
        let _ = views_call(sup, OP_VIEWS_CLOSE_SESSION, 9, &id[..n], &[], &mut alloc::vec::Vec::new());
    };
    let finish = |ok: bool| {
        close_session(sup);
        close(sup);
        ok
    };
    match ns_lookup(root_ns, b"/svc/views/policy/999999", chan) {
        (st, _) if st == KError::NotFound.as_i32() => {}
        _ => return finish(fail(b"a session that is not open has a policy endpoint")),
    }
    let (sp, pol) = ns_lookup(root_ns, alloc::format!("/svc/views/policy/{session}").as_bytes(), chan);
    let (sc, cli) = ns_lookup(root_ns, alloc::format!("/svc/views/s/{session}").as_bytes(), chan);
    if sp != 0 || pol == 0 || sc != 0 || cli == 0 {
        return finish(fail(b"no policy channel and client channel for the session"));
    }
    let Some(dev) = RootDevice::open(root_ns) else {
        close(pol);
        close(cli);
        return finish(fail(b"the root device would not open"));
    };
    let finish = |ok: bool| {
        close(pol);
        close(cli);
        finish(ok)
    };
    let on_device = || -> Option<alloc::vec::Vec<u8>> {
        let size = fs_server_ext4::ext4::stat_file(&dev, b"/system/views.toml").ok()?;
        dev.read_file(b"/system/views.toml", 0, size)
    };
    let next = core::cell::Cell::new(10u64);
    let mut ask = |ch: u64, op: u16, body: &[u8]| {
        next.set(next.get() + 1);
        views_call(ch, op, next.get(), body, &[], &mut exited)
    };
    let outcome_of = |r: Option<(bool, alloc::vec::Vec<u8>)>| {
        r.filter(|(err, _)| !err)
            .and_then(|(_, b)| parse_outcome(&b).map(|(o, why)| (o == Outcome::Started, alloc::vec::Vec::from(why))))
    };
    let says = |text: &[u8], what: &[u8]| text.windows(what.len()).any(|w| w == what);

    let Some((false, original)) = ask(pol, OP_VIEWS_SHOW, &[]) else {
        return finish(fail(b"Show was not answered"));
    };
    if on_device().as_deref() != Some(&original[..]) {
        return finish(fail(b"Show is not the file on the device"));
    }
    if !says(&original, b"\"views\"") {
        return finish(fail(b"the seeded admin profile does not grant views"));
    }
    if !ask(cli, OP_VIEWS_SHOW, &[]).is_some_and(|(err, _)| err) {
        return finish(fail(b"a client channel answered Show"));
    }

    // Two policies nobody could administer, refused by `Check` and by `Install`, the file kept.
    let lost_views = b"[profile.admin]\ngrants = [\"disks\", \"storage\"]\n\n[[rule]]\nwho = [\"alice\"]\nuse = [\"admin\"]\nrun = [\"*\"]\nauth = \"password\"\n";
    let nobody = b"[profile.admin]\ngrants = [\"disks\", \"storage\", \"views\"]\n\n[[rule]]\nwho = [\"kelby\"]\nuse = [\"admin\"]\nrun = [\"*\"]\nauth = \"password\"\n";
    for (bad, what) in [
        (&lost_views[..], &b"an admin profile without views"[..]),
        (&nobody[..], b"a rule naming no account that exists"),
    ] {
        match outcome_of(ask(cli, OP_VIEWS_CHECK, bad)) {
            Some((false, why)) if says(&why, b"views") => {}
            _ => return finish(fail(b"Check passed a policy nobody could administer")),
        }
        match outcome_of(ask(pol, OP_VIEWS_INSTALL, bad)) {
            Some((false, why)) if says(&why, b"views") => {}
            _ => {
                Line::new().s(b"boot-probe: policy: ").s(what).s(b" was installed").end();
                return finish(fail(b"Install took a policy nobody could administer"));
            }
        }
        if on_device().as_deref() != Some(&original[..]) {
            return finish(fail(b"a refused Install changed the file"));
        }
    }
    if outcome_of(ask(cli, OP_VIEWS_CHECK, &original)).map(|o| o.0) != Some(true) {
        return finish(fail(b"Check refused the seeded policy"));
    }

    let mut more = original.clone();
    more.extend_from_slice(b"\n[profile.d2probe]\ngrants = []\n\n[[rule]]\nwho = [\"alice\"]\nuse = [\"d2probe\"]\nrun = [\"nxsh\"]\nauth = \"none\"\n");
    if outcome_of(ask(pol, OP_VIEWS_INSTALL, &more)).map(|o| o.0) != Some(true) {
        return finish(fail(b"a policy with one more view was not installed"));
    }
    if ask(pol, OP_VIEWS_SHOW, &[]).map(|(_, b)| b).as_deref() != Some(&more[..])
        || on_device().as_deref() != Some(&more[..])
    {
        return finish(fail(b"the installed policy is not what Show and the device hold"));
    }
    let lists = |r: Option<(bool, alloc::vec::Vec<u8>)>, view: &[u8]| {
        r.filter(|(err, _)| !err).is_some_and(|(_, b)| {
            let mut found = false;
            parse_rows(&b, |row| found |= row.view == view).is_some() && found
        })
    };
    if !lists(ask(cli, OP_VIEWS_LIST, &[]), b"d2probe") {
        return finish(fail(b"List does not name the installed view"));
    }

    if outcome_of(ask(pol, OP_VIEWS_INSTALL, &original)).map(|o| o.0) != Some(true)
        || on_device().as_deref() != Some(&original[..])
    {
        return finish(fail(b"the original policy did not go back"));
    }
    if lists(ask(cli, OP_VIEWS_LIST, &[]), b"d2probe") {
        return finish(fail(b"List still names the view after the original went back"));
    }

    close_session(sup);
    let answered = ask(pol, OP_VIEWS_SHOW, &[]).is_some();
    close(pol);
    close(cli);
    close(sup);
    if answered {
        return fail(b"a closed session's policy channel still answered");
    }
    kprint(b"boot-probe: policy: Show is the device's file; a policy with no views administrator and one naming no account refused by Check and Install; one more view installed, listed, and put back; a closed session's channel closed ok\n");
    true
}
