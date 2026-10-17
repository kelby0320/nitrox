//! The account and authentication-administration tests.

use super::*;

/// **Accounts, fronted by the broker.**
///
/// **First the grant, as a view builds it**: `list /dev`, run in the admin view, names `accounts`
/// there, and `policy` beside it. Then the ops, asked at the protocol on `/svc/views/accounts/<id>`
/// for a session the probe opens as a supervisor does, under a principal no account has, so no
/// guard sees the probe itself as logged in:
/// - a client channel refuses `AddAccount`, and a name that is not one is refused without a home;
/// - `d3probe` added with its home and folders, and it authenticates; `Accounts` names it with
///   no session, then with the one the probe opens for it, and its removal is refused while
///   that session is open;
/// - `ChangePassword`: a wrong current password refused, the right one answered only once the
///   session's delay has passed; the new password authenticates and the old does not;
/// - `SetPassword` from the accounts channel;
/// - removing `alice`, the only administrator, refused for that reason, with her logged out;
/// - `d3probe` removed keeping its home, added again adopting it, and removed with it;
/// - a closed session's accounts channel answers nothing.
pub(super) fn accounts_test(root_ns: u64) -> bool {
    use libkern::KError;
    use librsproto::auth::{build_account_request, build_authenticate_request, parse_authenticate_reply};
    use librsproto::views::*;
    use libstream::wire::Value;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: accounts: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let (st, sup) = ns_lookup(root_ns, b"/svc/views/session", chan);
    if st != 0 || sup == 0 {
        return fail(b"no supervisor channel at /svc/views/session");
    }
    let open = |who: &[u8]| match views_call(sup, OP_VIEWS_OPEN_SESSION, 1, who, &[], &mut alloc::vec::Vec::new()) {
        Some((false, body)) => parse_session_id(&body),
        _ => None,
    };
    let close_session = |id: u64| {
        let mut b = [0u8; 8];
        let n = build_session_id(&mut b, id).unwrap_or(0);
        let _ = views_call(sup, OP_VIEWS_CLOSE_SESSION, 9, &b[..n], &[], &mut alloc::vec::Vec::new());
    };

    let Some(as_alice) = open(DEMO_USER) else {
        close(sup);
        return fail(b"OpenSession for alice");
    };
    let path = alloc::format!("/svc/views/s/{as_alice}");
    let listed = in_admin_view(root_ns, &path, b"list", &[b"/dev"], &mut alloc::vec::Vec::new());
    close_session(as_alice);
    let names = |rows: &[alloc::vec::Vec<Value>], what: &str| {
        rows.iter().any(|r| r.first() == Some(&Value::Str(alloc::string::String::from(what))))
    };
    if !listed.is_some_and(|(code, rows, _)| code == 0 && names(&rows, "accounts") && names(&rows, "policy")) {
        close(sup);
        return fail(b"list /dev in the admin view does not name accounts and policy");
    }

    let Some(probe) = open(b"boot-probe") else {
        close(sup);
        return fail(b"OpenSession for the probe");
    };
    let (sa, acc) = ns_lookup(root_ns, alloc::format!("/svc/views/accounts/{probe}").as_bytes(), chan);
    let (sc, cli) = ns_lookup(root_ns, alloc::format!("/svc/views/s/{probe}").as_bytes(), chan);
    let (so, oracle) = ns_lookup(root_ns, b"/svc/auth", chan);
    let finish = |ok: bool| {
        close(acc);
        close(cli);
        close(oracle);
        close_session(probe);
        close(sup);
        ok
    };
    if sa != 0 || acc == 0 || sc != 0 || cli == 0 || so != 0 || oracle == 0 {
        return finish(fail(b"no accounts channel, client channel and oracle session"));
    }
    match ns_lookup(root_ns, b"/svc/views/accounts/999999", chan) {
        (st, _) if st == KError::NotFound.as_i32() => {}
        _ => return finish(fail(b"a session that is not open has an accounts endpoint")),
    }

    let next = core::cell::Cell::new(100u64);
    let ask = |ch: u64, op: u16, body: &[u8]| {
        next.set(next.get() + 1);
        views_call(ch, op, next.get(), body, &[], &mut alloc::vec::Vec::new())
    };
    // `Some((started, reason))` for an outcome, `None` for anything else.
    let outcome = |r: Option<(bool, alloc::vec::Vec<u8>)>| {
        r.filter(|(err, _)| !err)
            .and_then(|(_, b)| parse_outcome(&b).map(|(o, why)| (o == Outcome::Started, alloc::vec::Vec::from(why))))
    };
    let says = |text: &[u8], what: &[u8]| text.windows(what.len()).any(|w| w == what);
    let refused_for = |r: Option<(bool, alloc::vec::Vec<u8>)>, what: &[u8]| {
        outcome(r).is_some_and(|(started, why)| !started && says(&why, what))
    };
    let pair = |a: &[u8], b: &[u8]| {
        let mut x = [0u8; 256];
        let n = build_account_request(&mut x, a, b).unwrap_or(0);
        x[..n].to_vec()
    };
    let removal = |name: &[u8], home: bool| {
        let mut x = [0u8; 64];
        let n = build_remove_account(&mut x, name, home).unwrap_or(0);
        x[..n].to_vec()
    };
    let change = |current: &[u8], new: &[u8]| {
        let mut x = [0u8; 256];
        let n = build_password_change(&mut x, current, new).unwrap_or(0);
        x[..n].to_vec()
    };
    let authenticates = |name: &[u8], pw: &[u8]| {
        let mut b = [0u8; 256];
        let n = build_authenticate_request(&mut b, name, pw).unwrap_or(0);
        ask(oracle, librsproto::OP_AUTHENTICATE, &b[..n])
            .is_some_and(|(err, body)| !err && parse_authenticate_reply(&body).is_some_and(|r| r.is_authenticated()))
    };
    // Each account `Accounts` names: `(sessions, administers)`.
    let shown = |name: &[u8]| -> Option<(u16, bool)> {
        let (err, body) = ask(cli, OP_VIEWS_ACCOUNTS, &[])?;
        let mut found = None;
        parse_accounts(&body, |r| {
            if r.name == name {
                found = Some((r.sessions, r.administers));
            }
        })
        .filter(|_| !err)?;
        found
    };
    const NAME: &[u8] = b"d3probe";
    const HOME: &[u8] = b"/home/d3probe";
    let homed = || {
        libfs::is_dir(root_ns, HOME)
            && libfs::HOME_FOLDERS.iter().all(|f| libfs::is_dir(root_ns, libfs::join(HOME, f.as_bytes()).as_bytes()))
    };

    // Only an accounts channel adds, and a name has to be one.
    if !ask(cli, OP_VIEWS_ADD_ACCOUNT, &pair(NAME, b"first secret")).is_some_and(|(err, _)| err) {
        return finish(fail(b"a client channel answered AddAccount"));
    }
    if !refused_for(ask(acc, OP_VIEWS_ADD_ACCOUNT, &pair(b"../d3", b"first secret")), b"a name is 1 to 32 bytes")
        || libfs::is_dir(root_ns, b"/d3")
    {
        return finish(fail(b"a name that is not one was not refused, or made a directory"));
    }

    match outcome(ask(acc, OP_VIEWS_ADD_ACCOUNT, &pair(NAME, b"first secret"))) {
        Some((true, why)) if says(&why, b"with a new home at /home/d3probe") => {}
        _ => return finish(fail(b"AddAccount did not add d3probe with a new home")),
    }
    if !homed() || !authenticates(NAME, b"first secret") {
        return finish(fail(b"d3probe has no home with its folders, or does not authenticate"));
    }
    if !refused_for(ask(acc, OP_VIEWS_ADD_ACCOUNT, &pair(NAME, b"other")), b"already exists") {
        return finish(fail(b"a second add of d3probe was not refused"));
    }
    if shown(NAME) != Some((0, false)) || shown(DEMO_USER) != Some((0, true)) {
        return finish(fail(b"Accounts does not show d3probe with no session, and alice administering"));
    }

    let Some(as_d3) = open(NAME) else {
        return finish(fail(b"OpenSession for d3probe"));
    };
    let (sd, d3) = ns_lookup(root_ns, alloc::format!("/svc/views/s/{as_d3}").as_bytes(), chan);
    let finish = |ok: bool| {
        close(d3);
        close_session(as_d3);
        finish(ok)
    };
    if sd != 0 || d3 == 0 {
        return finish(fail(b"no client channel for d3probe's session"));
    }
    if shown(NAME) != Some((1, false)) {
        return finish(fail(b"Accounts does not show d3probe's session"));
    }
    if !refused_for(ask(acc, OP_VIEWS_REMOVE_ACCOUNT, &removal(NAME, true)), b"d3probe is logged in")
        || !authenticates(NAME, b"first secret")
        || !homed()
    {
        return finish(fail(b"d3probe was removed while logged in"));
    }

    // Its own password: a wrong current one refused, and the next check held for the delay.
    if !refused_for(ask(d3, OP_VIEWS_CHANGE_PASSWORD, &change(b"wrong", b"second secret")), b"wrong password") {
        return finish(fail(b"ChangePassword took a wrong current password"));
    }
    let asked_at = clock_ns();
    let changed = outcome(ask(d3, OP_VIEWS_CHANGE_PASSWORD, &change(b"first secret", b"second secret")));
    let waited = clock_ns().saturating_sub(asked_at);
    if !changed.is_some_and(|(started, _)| started) {
        return finish(fail(b"ChangePassword with the right current password was not answered Started"));
    }
    // The broker's delay is two seconds (`view_broker::pacing::FAIL_DELAY_NS`); unpaced, the
    // answer takes milliseconds.
    if waited < 1_500_000_000 {
        return finish(fail(b"ChangePassword after a wrong one was answered without the session's delay"));
    }
    if !authenticates(NAME, b"second secret") || authenticates(NAME, b"first secret") {
        return finish(fail(b"after ChangePassword, the new password is refused or the old one taken"));
    }
    close(d3);
    close_session(as_d3);
    let finish = |ok: bool| {
        close(acc);
        close(cli);
        close(oracle);
        close_session(probe);
        close(sup);
        ok
    };
    if shown(NAME) != Some((0, false)) {
        return finish(fail(b"Accounts still shows d3probe's closed session"));
    }

    if !outcome(ask(acc, OP_VIEWS_SET_PASSWORD, &pair(NAME, b"third secret"))).is_some_and(|(s, _)| s)
        || !authenticates(NAME, b"third secret")
    {
        return finish(fail(b"SetPassword did not set d3probe's password"));
    }

    let why: &[u8] = b"removing alice would leave no account that could use `views` for every program";
    if !refused_for(ask(acc, OP_VIEWS_REMOVE_ACCOUNT, &removal(DEMO_USER, false)), why)
        || !authenticates(DEMO_USER, DEMO_PASSWORD)
    {
        return finish(fail(b"removing alice, the only administrator, was not refused for that reason"));
    }

    match outcome(ask(acc, OP_VIEWS_REMOVE_ACCOUNT, &removal(NAME, false))) {
        Some((true, why)) if says(&why, b"keeping /home/d3probe") => {}
        _ => return finish(fail(b"RemoveAccount without the home did not remove d3probe")),
    }
    if authenticates(NAME, b"third secret") || !homed() {
        return finish(fail(b"a removed d3probe still authenticates, or its kept home went"));
    }
    match outcome(ask(acc, OP_VIEWS_ADD_ACCOUNT, &pair(NAME, b"fourth secret"))) {
        Some((true, why)) if says(&why, b"adopting /home/d3probe") => {}
        _ => return finish(fail(b"adding d3probe again did not adopt its home")),
    }
    match outcome(ask(acc, OP_VIEWS_REMOVE_ACCOUNT, &removal(NAME, true))) {
        Some((true, why)) if says(&why, b"and /home/d3probe with it") => {}
        _ => return finish(fail(b"RemoveAccount with the home did not remove both")),
    }
    if libfs::is_dir(root_ns, HOME) || authenticates(NAME, b"fourth secret") {
        return finish(fail(b"d3probe's home or record outlived its removal"));
    }
    if !refused_for(ask(acc, OP_VIEWS_REMOVE_ACCOUNT, &removal(NAME, true)), b"no account is named d3probe") {
        return finish(fail(b"a second removal of d3probe was not refused"));
    }

    close_session(probe);
    let answered = ask(acc, OP_VIEWS_SET_PASSWORD, &pair(NAME, b"fifth secret")).is_some();
    close(acc);
    close(cli);
    close(oracle);
    close(sup);
    if answered {
        return fail(b"a closed session's accounts channel still answered");
    }
    kprint(b"boot-probe: accounts: the grant binds /dev/accounts; an account added with its home, shown with its session, kept while logged in, its password changed after the delay and set; the only administrator kept; a home kept, adopted and removed; a closed session's channel closed ok\n");
    true
}

/// **`auth-service`'s admin session, and the file it writes.**
///
/// Asked directly, as the view broker will ask it, on a session resolved at `/svc/auth/admin`:
/// an account added and authenticated against on an ordinary oracle session, its password set
/// (the old one refused, the new accepted), and removed (refused altogether).
///
/// **After each write, the file is read from the device**, raw, through the ext4 library: a
/// re-resolve would read the page cache, and what is at stake is that the rename landed. `alice`'s
/// line is the same bytes throughout. Each refusal is its own, and a suffix the service does not
/// serve is `NotFound`.
pub(super) fn auth_admin_test(root_ns: u64) -> bool {
    use libkern::KError;
    use librsproto::auth::{
        OP_AUTH_ADD, OP_AUTH_LIST, OP_AUTH_REMOVE, OP_AUTH_SET_PASSWORD, build_account_request,
        build_authenticate_request, parse_account_list, parse_authenticate_reply,
    };
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: auth admin: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    match ns_lookup(root_ns, b"/svc/auth/nope", chan) {
        (st, _) if st == KError::NotFound.as_i32() => {}
        _ => return fail(b"a suffix auth-service does not serve was not NotFound"),
    }
    let (st, admin) = ns_lookup(root_ns, b"/svc/auth/admin", chan);
    let (so, oracle) = ns_lookup(root_ns, b"/svc/auth", chan);
    if st != 0 || admin == 0 || so != 0 || oracle == 0 {
        return fail(b"no admin session and oracle session at /svc/auth");
    }
    let Some(dev) = RootDevice::open(root_ns) else {
        return fail(b"the root device would not open");
    };
    let finish = |ok: bool| {
        close(admin);
        close(oracle);
        ok
    };

    let next = core::cell::Cell::new(0u64);
    let ask = |ch: u64, op: u16, body: &[u8]| -> Option<Received> {
        next.set(next.get() + 1);
        let id = next.get();
        if !rs_send(ch, op, id, body, &[]) {
            return None;
        }
        let deadline = clock_ns() + 10_000_000_000;
        loop {
            let m = receive(ch, deadline)?;
            if m.request_id == id {
                return Some(m);
            }
        }
    };
    let account = |name: &[u8], pw: &[u8]| {
        let mut b = [0u8; 256];
        let n = build_account_request(&mut b, name, pw).unwrap_or(0);
        b[..n].to_vec()
    };
    let refused_with = |m: Option<Received>| {
        m.filter(|m| m.error).and_then(|m| librsproto::error::parse_error(&m.body).map(|e| e.kerror))
    };
    let authenticates = |name: &[u8], pw: &[u8]| -> Option<alloc::vec::Vec<u8>> {
        let mut b = [0u8; 256];
        let n = build_authenticate_request(&mut b, name, pw)?;
        let m = ask(oracle, librsproto::OP_AUTHENTICATE, &b[..n])?;
        let r = parse_authenticate_reply(&m.body)?;
        r.is_authenticated().then(|| r.home.to_vec())
    };
    let on_device = || -> Option<alloc::vec::Vec<u8>> {
        let size = fs_server_ext4::ext4::stat_file(&dev, b"/system/users").ok()?;
        dev.read_file(b"/system/users", 0, size)
    };
    let line_of = |file: &[u8], name: &[u8]| -> Option<alloc::vec::Vec<u8>> {
        file.split(|&b| b == b'\n')
            .find(|l| l.len() > name.len() && l.starts_with(name) && l[name.len()] == b':')
            .map(|l| l.to_vec())
    };
    let Some(before) = on_device() else {
        return finish(fail(b"/system/users would not read from the device"));
    };
    let Some(alice) = line_of(&before, b"alice") else {
        return finish(fail(b"alice is not in /system/users on the device"));
    };
    let alice_unchanged = |file: &[u8]| line_of(file, b"alice").as_deref() == Some(&alice[..]);

    let listed = |m: Option<Received>, name: &[u8]| {
        m.filter(|m| !m.error)
            .is_some_and(|m| parse_account_list(&m.body).is_some_and(|l| l.iter().any(|(n, _)| n == name)))
    };
    if !listed(ask(admin, OP_AUTH_LIST, &[]), b"alice") {
        return finish(fail(b"List does not name alice"));
    }

    const NAME: &[u8] = b"d1probe";
    let added = ask(admin, OP_AUTH_ADD, &account(NAME, b"first secret"));
    if !added.as_ref().is_some_and(|m| !m.error) {
        return finish(fail(b"Add was not answered"));
    }
    if authenticates(NAME, b"first secret").as_deref() != Some(&b"/home/d1probe"[..]) {
        return finish(fail(b"the added account does not authenticate, with its home"));
    }
    let Some(file) = on_device() else {
        return finish(fail(b"/system/users would not read after the add"));
    };
    let Some(first) = line_of(&file, NAME).filter(|l| l.ends_with(b":/home/d1probe")) else {
        return finish(fail(b"the added account is not on the device"));
    };
    if !alice_unchanged(&file) {
        return finish(fail(b"alice's line changed when another account was added"));
    }

    if refused_with(ask(admin, OP_AUTH_ADD, &account(NAME, b"x"))) != Some(KError::AlreadyExists.as_i32()) {
        return finish(fail(b"a second Add of one name was not AlreadyExists"));
    }
    if refused_with(ask(admin, OP_AUTH_ADD, &account(b"Bad Name", b"x"))) != Some(KError::InvalidArgument.as_i32()) {
        return finish(fail(b"a bad name was not InvalidArgument"));
    }
    if refused_with(ask(admin, librsproto::OP_AUTHENTICATE, &[])) != Some(KError::Unsupported.as_i32()) {
        return finish(fail(b"an admin session authenticated"));
    }
    if !ask(oracle, OP_AUTH_LIST, &[]).is_some_and(|m| m.error) {
        return finish(fail(b"an oracle session answered List"));
    }

    // A new password: the old refused, the new accepted, and the device's line another.
    if !ask(admin, OP_AUTH_SET_PASSWORD, &account(NAME, b"second secret")).is_some_and(|m| !m.error) {
        return finish(fail(b"SetPassword was not answered"));
    }
    if authenticates(NAME, b"first secret").is_some() || authenticates(NAME, b"second secret").is_none() {
        return finish(fail(b"after SetPassword the old password still works, or the new one does not"));
    }
    let Some(file) = on_device() else {
        return finish(fail(b"/system/users would not read after SetPassword"));
    };
    if line_of(&file, NAME).is_none_or(|l| l == first) || !alice_unchanged(&file) {
        return finish(fail(b"the new password is not on the device, or alice's line moved"));
    }

    // Removed: refused, gone from the device and the list, and a second removal NotFound.
    if !ask(admin, OP_AUTH_REMOVE, NAME).is_some_and(|m| !m.error) {
        return finish(fail(b"Remove was not answered"));
    }
    if authenticates(NAME, b"second secret").is_some() {
        return finish(fail(b"a removed account still authenticates"));
    }
    let Some(file) = on_device() else {
        return finish(fail(b"/system/users would not read after Remove"));
    };
    if line_of(&file, NAME).is_some() || !alice_unchanged(&file) {
        return finish(fail(b"the removed account is still on the device, or alice's line moved"));
    }
    if listed(ask(admin, OP_AUTH_LIST, &[]), NAME) {
        return finish(fail(b"List still names the removed account"));
    }
    if refused_with(ask(admin, OP_AUTH_REMOVE, NAME)) != Some(KError::NotFound.as_i32()) {
        return finish(fail(b"a second Remove was not NotFound"));
    }
    if authenticates(b"alice", DEMO_PASSWORD).is_none() {
        return finish(fail(b"alice no longer authenticates"));
    }
    kprint(b"boot-probe: auth admin: an account added, authenticated, its password changed and removed, each on the device with alice's line untouched, each refusal its own ok\n");
    finish(true)
}
