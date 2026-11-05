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
pub(super) fn grace_test(root: Root<'_>) -> bool {
    use librsproto::views::*;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: grace: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let mut exited = Vec::new();
    let Ok(sup) = lookup::<Channel, Only, _>(root, "/svc/views/session", chan) else {
        return fail(b"no supervisor channel at /svc/views/session");
    };
    let session = match views_call(&sup, OP_VIEWS_OPEN_SESSION, 1, DEMO_USER, Carried::new(), &mut exited) {
        Some((false, body)) => parse_session_id(&body),
        _ => None,
    };
    let Some(session) = session else {
        return fail(b"OpenSession");
    };
    let Ok(tty) = lookup::<Channel, Only, _>(root, "/dev/tty", chan) else {
        return fail(b"no terminal at /dev/tty");
    };
    let mut extras = None;
    let ok = grace_session(root, session, &tty, &mut extras, &mut exited);
    let mut id = [0u8; 8];
    let n = build_session_id(&mut id, session).unwrap_or(0);
    let _ = views_call(&sup, OP_VIEWS_CLOSE_SESSION, 9, &id[..n], Carried::new(), &mut Vec::new());
    drop(extras);
    drop(tty);
    drop(sup);
    ok
}

/// What `grace_test` asks of the broker for `session`, with `tty` its first terminal. The second
/// terminal and its backend are left in `extras`, to close after the session.
fn grace_session(
    root: Root<'_>,
    session: u64,
    tty: &Handle<Channel, Only>,
    extras: &mut Option<Pipe>,
    exited: &mut Vec<Vec<u8>>,
) -> bool {
    use librsproto::views::*;
    use librsproto::{OP_TTY_ATTACH_BACKEND, OP_TTY_TOKEN, TTY_TOKEN_LEN};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: grace: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let token = |t: &Handle<Channel, Only>| -> Option<[u8; TTY_TOKEN_LEN]> {
        if !rs_send(t, OP_TTY_TOKEN, 0x7470, &[], Carried::new()) {
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
               exited: &mut Vec<Vec<u8>>|
     -> Option<(Outcome, Handle<Channel, Only>)> {
        let cli = lookup::<Channel, Only, _>(root, &alloc::format!("/svc/views/s/{session}"), chan).ok()?;
        let copy = root.copy().ok()?;
        let mut req = [0u8; 256];
        let n = build_request(&mut req, 0, view, program, args, b"", token.as_ref())?;
        match views_call(&cli, OP_VIEWS_REQUEST, 2, &req[..n], carry(copy), exited) {
            Some((false, body)) => parse_outcome(&body).map(|(o, _)| (o, cli)),
            _ => None,
        }
    };
    let sleep: &[&[u8]] = &[b"0"];
    let started = |o: Option<(Outcome, Handle<Channel, Only>)>, exited: &mut Vec<Vec<u8>>| -> bool {
        match o {
            Some((Outcome::Started, cli)) => {
                if exited.pop().is_none() {
                    let _ = views_receive(&cli, 0, exited);
                }
                true
            }
            Some(_) => false,
            None => false,
        }
    };
    let asked = |o: Option<(Outcome, Handle<Channel, Only>)>| -> bool { matches!(o, Some((Outcome::NeedPassword, _))) };

    let Some(first) = token(tty) else {
        return fail(b"the terminal gave no token");
    };
    let opened = match ask(b"admin", b"sleep", sleep, Some(first), exited) {
        Some((Outcome::NeedPassword, cli)) => {
            let ok = matches!(
                views_call(&cli, OP_VIEWS_PASSWORD, 3, DEMO_PASSWORD, Carried::new(), exited),
                Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::Started, _)))
            );
            started(Some((Outcome::Started, cli)), exited) && ok
        }
        other => {
            let _ = asked(other);
            false
        }
    };
    if !opened {
        return fail(b"a first request with a token was not asked and then started");
    }
    let Some(second) = token(tty) else {
        return fail(b"no second token");
    };
    if !started(ask(b"admin", b"sleep", sleep, Some(second), exited), exited) {
        return fail(b"a second request from the same terminal, inside the window, was asked");
    }
    // 3. **The forgery control**: a token nobody minted.
    let forged = [0x5a; TTY_TOKEN_LEN];
    if !asked(ask(b"admin", b"sleep", sleep, Some(forged), exited)) {
        return fail(b"a made-up token was not asked");
    }
    if !asked(ask(b"admin", b"sleep", sleep, Some(second), exited)) {
        return fail(b"a token used twice was not asked");
    }
    if !asked(ask(b"admin", b"sleep", sleep, None, exited)) {
        return fail(b"a request with no token was not asked");
    }
    // 6. Another view: the window is `admin`'s.
    let Some(other_view) = token(tty) else {
        return fail(b"no token for the other view");
    };
    if !asked(ask(b"install", b"nxinstall", &[], Some(other_view), exited)) {
        return fail(b"another view, from the same terminal, was not asked");
    }
    // 7. **Another terminal**: one with a backend of its own, as a window has.
    let window = lookup::<Channel, Only, _>(root, "/dev/tty", chan);
    let backend = Handle::<Channel, Only>::pair(4);
    let (Ok(window), Ok((ours, theirs))) = (window, backend) else {
        return fail(b"a second terminal and a backend channel");
    };
    let (window, _) = &*extras.insert((window, ours));
    let attached = matches!(
        rs_send(window, OP_TTY_ATTACH_BACKEND, 0x6174, &[], carry(theirs)).then(|| {
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
        return fail(b"the second terminal would not take a backend");
    }
    let Some(elsewhere) = token(window) else {
        return fail(b"no token from the second terminal");
    };
    if !asked(ask(b"admin", b"sleep", sleep, Some(elsewhere), exited)) {
        return fail(b"a token from another terminal was not asked");
    }
    // 8. **And the window was open all along**: a real token from the first terminal still starts.
    let Some(still) = token(tty) else {
        return fail(b"no token after the refusals");
    };
    if !started(ask(b"admin", b"sleep", sleep, Some(still), exited), exited) {
        return fail(b"the window closed while the refusals were asked, so they prove nothing");
    }
    // 8b. **A refused password forgets every window of the session** : a request from
    //     the other terminal, which has no window and so is asked, is answered wrong, and the
    //     first terminal's window is gone with it, so a real token from there is asked too.
    let Some(other) = token(window) else {
        return fail(b"no token from the second terminal for the wrong password");
    };
    let refused = match ask(b"admin", b"sleep", sleep, Some(other), exited) {
        Some((Outcome::NeedPassword, cli)) => {
            let denied = matches!(
                views_call(&cli, OP_VIEWS_PASSWORD, 6, b"not the password", Carried::new(), exited),
                Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::Denied { .. }, _)))
            );
            drop(cli);
            denied
        }
        other => {
            let _ = asked(other);
            false
        }
    };
    if !refused {
        return fail(b"a wrong password at the second terminal was not refused");
    }
    let Some(after_wrong) = token(tty) else {
        return fail(b"no token after the wrong password");
    };
    if !asked(ask(b"admin", b"sleep", sleep, Some(after_wrong), exited)) {
        return fail(b"a refused password at another terminal left this terminal's window open");
    }
    // **Opened again**, so that `Forget` below has a window to forget — and shown to be open.
    let Some(again) = token(tty) else {
        return fail(b"no token to open the window again");
    };
    let reopened = match ask(b"admin", b"sleep", sleep, Some(again), exited) {
        Some((Outcome::NeedPassword, cli)) => {
            let ok = matches!(
                views_call(&cli, OP_VIEWS_PASSWORD, 7, DEMO_PASSWORD, Carried::new(), exited),
                Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::Started, _)))
            );
            started(Some((Outcome::Started, cli)), exited) && ok
        }
        other => {
            let _ = asked(other);
            false
        }
    };
    let Some(open) = token(tty) else {
        return fail(b"no token to show the window open again");
    };
    if !reopened || !started(ask(b"admin", b"sleep", sleep, Some(open), exited), exited) {
        return fail(b"the window would not open again after the wrong password");
    }
    let forgot = lookup::<Channel, Only, _>(root, &alloc::format!("/svc/views/s/{session}"), chan).is_ok_and(|fcli| {
        matches!(views_call(&fcli, OP_VIEWS_FORGET, 4, &[], Carried::new(), exited), Some((false, _)))
    });
    if !forgot {
        return fail(b"Forget");
    }
    let Some(after) = token(tty) else {
        return fail(b"no token after Forget");
    };
    if !asked(ask(b"admin", b"sleep", sleep, Some(after), exited)) {
        return fail(b"a request after Forget was not asked");
    }
    // 10. **A refused token names no terminal, so a password typed with one opens no window.** A
    //     broker that gave every refused token the same terminal — reading a refusal as an id, or
    //     falling back to a default — would open a window under it here, and the next made-up
    //     token would use it.
    let opened_none = match ask(b"admin", b"sleep", sleep, Some([0x11; TTY_TOKEN_LEN]), exited) {
        Some((Outcome::NeedPassword, cli)) => {
            let ok = matches!(
                views_call(&cli, OP_VIEWS_PASSWORD, 5, DEMO_PASSWORD, Carried::new(), exited),
                Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::Started, _)))
            );
            started(Some((Outcome::Started, cli)), exited) && ok
        }
        other => {
            let _ = asked(other);
            false
        }
    };
    if !opened_none {
        return fail(b"a made-up token answered with the password did not start");
    }
    if !asked(ask(b"admin", b"sleep", sleep, Some([0x22; TTY_TOKEN_LEN]), exited)) {
        return fail(b"a made-up token used a window another made-up token's password opened");
    }
    log::write(
        b"boot-probe: grace: a password remembered for one terminal and view; a made-up token, a reused one, \
          none, another view and another terminal all asked; the window open throughout; a wrong password \
          elsewhere closed it; asked again after Forget; and a made-up token's password opened no window ok\n",
    );
    true
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
/// - A policy with three more views is installed, `Show` and the device hold it, and `List` names
///   one. Requests for the other two wait for a password while a policy without the first and with
///   the second granting more is installed. Each password, though right, is refused: one rule is
///   gone, and the other's view grants something else. Then the original goes back.
/// - A closed session's policy channel is closed with it, and a session that is not open has no
///   policy endpoint.
pub(super) fn policy_test(root: Root<'_>) -> bool {
    use librsproto::views::*;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: policy: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let mut exited = Vec::new();
    let Ok(sup) = lookup::<Channel, Only, _>(root, "/svc/views/session", chan) else {
        return fail(b"no supervisor channel at /svc/views/session");
    };
    let session = match views_call(&sup, OP_VIEWS_OPEN_SESSION, 1, DEMO_USER, Carried::new(), &mut exited) {
        Some((false, body)) => parse_session_id(&body),
        _ => None,
    };
    let Some(session) = session else {
        return fail(b"OpenSession");
    };
    let close_session = || {
        let mut id = [0u8; 8];
        let n = build_session_id(&mut id, session).unwrap_or(0);
        let _ = views_call(&sup, OP_VIEWS_CLOSE_SESSION, 9, &id[..n], Carried::new(), &mut Vec::new());
    };
    let closed = core::cell::Cell::new(false);
    let ok = policy_session(root, &sup, session, &closed, &mut exited);
    if !closed.get() {
        close_session();
    }
    ok
}

/// What `policy_test` asks of the broker for `session`, its policy and client channels open here.
fn policy_session(
    root: Root<'_>,
    sup: &Handle<Channel, Only>,
    session: u64,
    closed: &core::cell::Cell<bool>,
    exited: &mut Vec<Vec<u8>>,
) -> bool {
    use libkern::KError;
    use librsproto::views::*;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: policy: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = Rights::SEND | Rights::RECV | Rights::WAIT;
    let close_session = || {
        let mut id = [0u8; 8];
        let n = build_session_id(&mut id, session).unwrap_or(0);
        let _ = views_call(sup, OP_VIEWS_CLOSE_SESSION, 9, &id[..n], Carried::new(), &mut Vec::new());
    };
    if !matches!(
        lookup::<Channel, Only, _>(root, "/svc/views/policy/999999", chan),
        Err(e) if e.code() == KError::NotFound
    ) {
        return fail(b"a session that is not open has a policy endpoint");
    }
    let pol = lookup::<Channel, Only, _>(root, &alloc::format!("/svc/views/policy/{session}"), chan);
    let cli = lookup::<Channel, Only, _>(root, &alloc::format!("/svc/views/s/{session}"), chan);
    let (Ok(pol), Ok(cli)) = (pol, cli) else {
        return fail(b"no policy channel and client channel for the session");
    };
    let (pol, cli) = (&pol, &cli);
    let Some(dev) = RootDevice::open(root) else {
        return fail(b"the root device would not open");
    };
    let on_device = || -> Option<Vec<u8>> {
        let size = fs_server_ext4::ext4::stat_file(&dev, b"/system/views.toml").ok()?;
        dev.read_file(b"/system/views.toml", 0, size)
    };
    let next = core::cell::Cell::new(10u64);
    let mut ask = |ch: &Handle<Channel, Only>, op: u16, body: &[u8]| {
        next.set(next.get() + 1);
        views_call(ch, op, next.get(), body, Carried::new(), exited)
    };
    let outcome_of = |r: Option<(bool, Vec<u8>)>| {
        r.filter(|(err, _)| !err)
            .and_then(|(_, b)| parse_outcome(&b).map(|(o, why)| (o == Outcome::Started, Vec::from(why))))
    };
    let says = |text: &[u8], what: &[u8]| text.windows(what.len()).any(|w| w == what);

    let Some((false, original)) = ask(pol, OP_VIEWS_SHOW, &[]) else {
        return fail(b"Show was not answered");
    };
    if on_device().as_deref() != Some(&original[..]) {
        return fail(b"Show is not the file on the device");
    }
    if !says(&original, b"\"views\"") {
        return fail(b"the seeded admin profile does not grant views");
    }
    if !ask(cli, OP_VIEWS_SHOW, &[]).is_some_and(|(err, _)| err) {
        return fail(b"a client channel answered Show");
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
            _ => return fail(b"Check passed a policy nobody could administer"),
        }
        match outcome_of(ask(pol, OP_VIEWS_INSTALL, bad)) {
            Some((false, why)) if says(&why, b"views") => {}
            _ => {
                Line::new().s(b"boot-probe: policy: ").s(what).s(b" was installed").end();
                return fail(b"Install took a policy nobody could administer");
            }
        }
        if on_device().as_deref() != Some(&original[..]) {
            return fail(b"a refused Install changed the file");
        }
    }
    if outcome_of(ask(cli, OP_VIEWS_CHECK, &original)).map(|o| o.0) != Some(true) {
        return fail(b"Check refused the seeded policy");
    }

    let mut more = original.clone();
    more.extend_from_slice(b"\n[profile.d2probe]\ngrants = []\n\n[[rule]]\nwho = [\"alice\"]\nuse = [\"d2probe\"]\nrun = [\"nxsh\"]\nauth = \"none\"\n");
    let d4 = |grants: &[u8]| {
        let mut s = Vec::from(&b"\n[profile.d4probe]\ngrants = ["[..]);
        s.extend_from_slice(grants);
        s.extend_from_slice(
            b"]\n\n[[rule]]\nwho = [\"alice\"]\nuse = [\"d4probe\"]\nrun = [\"test-stage\"]\nauth = \"password\"\n",
        );
        s
    };
    let mut changed = original.clone();
    changed.extend_from_slice(&d4(b"\"power\""));
    more.extend_from_slice(b"\n[profile.d3probe]\ngrants = []\n\n[[rule]]\nwho = [\"alice\"]\nuse = [\"d3probe\"]\nrun = [\"test-stage\"]\nauth = \"password\"\n");
    more.extend_from_slice(&d4(b""));
    if outcome_of(ask(pol, OP_VIEWS_INSTALL, &more)).map(|o| o.0) != Some(true) {
        return fail(b"a policy with one more view was not installed");
    }
    if ask(pol, OP_VIEWS_SHOW, &[]).map(|(_, b)| b).as_deref() != Some(&more[..])
        || on_device().as_deref() != Some(&more[..])
    {
        return fail(b"the installed policy is not what Show and the device hold");
    }
    let lists = |r: Option<(bool, Vec<u8>)>, view: &[u8]| {
        r.filter(|(err, _)| !err).is_some_and(|(_, b)| {
            let mut found = false;
            parse_rows(&b, |row| found |= row.view == view).is_some() && found
        })
    };
    if !lists(ask(cli, OP_VIEWS_LIST, &[]), b"d2probe") {
        return fail(b"List does not name the installed view");
    }
    // Two requests the installed policy allows, waiting for their passwords across the install of
    // `changed`, which drops `d3probe` and has `d4probe` grant `power`.
    let outcome =
        |r: Option<(bool, Vec<u8>)>| r.filter(|(err, _)| !err).and_then(|(_, b)| parse_outcome(&b).map(|(o, _)| o));
    let wait_for_password = |view: &[u8]| -> Option<Handle<Channel, Only>> {
        let waiting = lookup::<Channel, Only, _>(root, &alloc::format!("/svc/views/s/{session}"), chan).ok()?;
        let copy = root.copy().ok()?;
        let mut req = [0u8; 128];
        let n = build_request(&mut req, 0, view, b"test-stage", &[], b"", None)?;
        let asked = outcome(views_call(&waiting, OP_VIEWS_REQUEST, 1, &req[..n], carry(copy), &mut Vec::new()));
        (asked == Some(Outcome::NeedPassword)).then_some(waiting)
    };
    let (Some(gone), Some(regranted)) = (wait_for_password(b"d3probe"), wait_for_password(b"d4probe")) else {
        return fail(b"two requests the installed policy allows did not wait for a password");
    };
    if outcome_of(ask(pol, OP_VIEWS_INSTALL, &changed)).map(|o| o.0) != Some(true) {
        return fail(b"the policy without d3probe was not installed");
    }
    for (waiting, what) in [(gone, &b"whose rule is gone"[..]), (regranted, b"whose view grants more")] {
        let answered =
            outcome(views_call(&waiting, OP_VIEWS_PASSWORD, 2, DEMO_PASSWORD, Carried::new(), &mut Vec::new()));
        drop(waiting);
        if answered != Some(Outcome::Denied { retry: false }) {
            Line::new().s(b"boot-probe: policy: the request ").s(what).s(b" was started").end();
            return fail(b"a password started a request the policy no longer allows as asked");
        }
    }

    if outcome_of(ask(pol, OP_VIEWS_INSTALL, &original)).map(|o| o.0) != Some(true)
        || on_device().as_deref() != Some(&original[..])
    {
        return fail(b"the original policy did not go back");
    }
    if lists(ask(cli, OP_VIEWS_LIST, &[]), b"d2probe") {
        return fail(b"List still names the view after the original went back");
    }

    close_session();
    closed.set(true);
    let answered = ask(pol, OP_VIEWS_SHOW, &[]).is_some();
    if answered {
        return fail(b"a closed session's policy channel still answered");
    }
    log::write(b"boot-probe: policy: Show is the device's file; a policy with no views administrator and one naming no account refused by Check and Install; views installed, one listed, two requests waiting for a password refused once one rule was gone and one view granted more, the original put back; a closed session's channel closed ok\n");
    true
}
