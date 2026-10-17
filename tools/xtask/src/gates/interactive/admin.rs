//! `test-interactive`'s administration: `with`, `disk`, policy, accounts, services, the log.

use crate::*;

/// `test-interactive`'s steps 20b to 20e: `with`, `disk`, the policy changed and put back, and
/// an account's life. The number of steps it ran.
pub(crate) fn interactive_administration(s: &mut Session) -> R<usize> {
    let mut steps = 0usize;
    // 20b. **`with`: a program in a view**, at the real prompt, each claim depending on the last.
    //      (a) **Without `with`, no disks**: `nxinstall`'s own answer when its session holds
    //          none, the baseline the grant is measured against.
    s.send("nxinstall")?;
    s.expect("no block devices in this view")?;
    s.expect("/home>")?;
    //      (b) **Refused by the policy, and nothing asked.** `install` lets alice run only
    //          `nxinstall`; `nxsh` is refused before any password, naming which half was wrong.
    s.send("with install nxsh")?;
    s.expect("`install` does not let alice run `nxsh`")?;
    s.expect("/home>")?;
    //      (c) **`with --list`**, as a table: the seeded `install` row names its one program.
    s.send("with --list")?;
    s.expect("nxinstall")?;
    s.expect("/home>")?;
    //      (d) **A wrong password, then the right one — held for the session's delay — and the
    //          grant arrived.** The right password is typed the moment the second prompt appears;
    //          the broker holds its check until two seconds after the failure, so the listing
    //          cannot come sooner. It names the ESP, `/dev/blk/1`, which (a) could not.
    //          **And what the grant leaves out**: `/dev/blk/0` holds `init`'s root, which the
    //          broker asks the storage service about first, so the listing never names it.
    const WRONG: &str = WITH_WRONG_PASSWORD;
    s.send("with admin nxinstall")?;
    s.expect("[with admin] password (1 of 3): ")?;
    s.send(WRONG)?;
    s.expect("with: wrong password")?;
    let refused = s.matched_at();
    s.expect("[with admin] password (2 of 3): ")?;
    let listing_from = s.transcript().len();
    s.send(DEMO_PASSWORD)?;
    s.expect("/dev/blk/1")?;
    let held = s.matched_at().saturating_duration_since(refused);
    if held < std::time::Duration::from_millis(1500) {
        return Err(format!(
            "the password after a wrong one was answered {held:?} after the refusal — the broker did \
             not hold it for the session's delay"
        )
        .into());
    }
    s.expect("/home>")?;
    let listing = s.transcript()[listing_from..].to_string();
    // **Never a row, and the disk named as what it is.** The installer says, on `stderr`, why a
    // disk it cannot have is missing, and the serial terminal shows `stderr` beside the table, so
    // the claim is per line: a line naming a withheld device must be that message, and
    // `/dev/blk/0`'s must be there. The root partition, `/dev/blk/2`, is never named at all.
    for withheld in ["/dev/blk/0", "/dev/blk/2"] {
        if let Some(row) = listing.lines().find(|l| l.contains(withheld) && !l.contains("holds the running system")) {
            return Err(format!(
                "`with admin nxinstall` listed {withheld}, which is in use (init's root, or the disk \
                 holding it): {row:?}"
            )
            .into());
        }
    }
    if !listing.lines().any(|l| l.contains("/dev/blk/0 (") && l.contains("holds the running system")) {
        return Err(
            format!("`with admin nxinstall` did not say /dev/blk/0 holds the running system: {listing:?}").into()
        );
    }
    //          **And it said so on the terminal, not on the kernel log.** On this serial
    //          console the two look alike: a program with no `stderr` writes to `kprint`, which
    //          is COM1 too, so the line above passed while every program `with` ran had none.
    //          What tells them apart is the shell: it paints each error and warning it drains,
    //          and `kprint` paints nothing. **A warning's colour**: the disk is not offered, and
    //          that is not a failure.
    if !listing.contains("\x1b[93m/dev/blk/0 (") {
        return Err(format!(
            "`with admin nxinstall`'s message about /dev/blk/0 is not in the warning colour: \
             either it reached the console by `kprint` — the program was given no `stderr`, and \
             on a machine with no serial port nobody would see it — or it was not sent as a \
             warning: {listing:?}"
        )
        .into());
    }
    println!("  ok: and a program in a view reaches the terminal through its `stderr`");
    //      (d2) **Remembered, for five minutes, on this terminal**: the same view again starts
    //           without asking, and the audit says why. `with --forget` closes the window, and
    //           (e)'s first prompt is the proof. Audit lines are waited for, not the prompt: they
    //           reach the console through the logging service, in either order with it.
    s.send("with admin whoami")?;
    s.expect_all(&[
        "view: alice admin whoami — allowed, within the grace period",
        "view: alice admin whoami — exited, code 0",
    ])?;
    s.send("with --forget")?;
    s.expect("view: alice — remembered passwords forgotten")?;
    println!("  ok: a password remembered for this terminal and view, and forgotten when asked");
    //      (e) **Three wrong passwords end a request**; each check after the first waits the delay.
    s.send("with admin whoami")?;
    for n in 1..=3 {
        s.expect(&format!("[with admin] password ({n} of 3): "))?;
        s.send(WRONG)?;
    }
    s.expect("with: three wrong passwords")?;
    s.expect("/home>")?;
    //      (f) **`Ctrl-C` stops a program started with `with`**: the shell asks `with`, which asks
    //          the broker, which asks `sleep`. Sixty seconds is far past what this waits.
    with_admin_asked(s, "sleep 60")?;
    s.expect("view: alice admin sleep — started")?;
    let asked = std::time::Instant::now();
    s.send_raw("\x03")?;
    // **Unordered, because two processes say them.** The audit record reaches the console through
    // the logging service and the prompt through the tty server; the shell's prompt can land
    // first, and a sequence of two `expect`s would then scan past it.
    s.expect_all(&["view: alice admin sleep — exited", "/home>"])?;
    if asked.elapsed() > std::time::Duration::from_secs(20) {
        return Err("`with admin sleep 60` took more than 20 s to stop after Ctrl-C".into());
    }
    //      (g) **An audit record for each, and no password in any**: the prompts turned echo off.
    let t = s.transcript();
    for line in [
        "view: alice install nxsh — denied: `install` does not let alice run `nxsh`",
        "view: alice admin nxinstall — wrong password (1 of 3)",
        "view: alice admin nxinstall — started",
        "view: alice admin whoami — wrong password (3 of 3)",
    ] {
        if !t.contains(line) {
            return Err(format!("no audit record `{line}`").into());
        }
    }
    for secret in [DEMO_PASSWORD, WRONG] {
        if t.contains(secret) {
            return Err(format!("a password reached the console: `{secret}`").into());
        }
    }
    steps += 1;

    // 20c. **`disk`, with and without the grant.**
    //      (a) `--list` is the storage service's table, from any session: the row mounted at `/`
    //          is `init`'s root, matched on words the command does not contain.
    let row_from = s.transcript().len();
    s.send("disk --list | filter mounted == \"/\"")?;
    s.expect_all(&["blk-2", "init"])?;
    s.expect("/home>")?;
    //          **And its empty cell is blank**: `init`'s root is mounted writable, so its `clean`
    //          is null, which the shell drew as `null`; the typed command holds none.
    let row = s.transcript()[row_from..].to_string();
    if row.contains("null") {
        return Err(format!("`disk --list` drew an empty cell as `null`: {row:?}").into());
    }
    //          **And a disk is named as the installer names it**: the `description` column
    //          carries the SATA disk's model and serial, which the typed command does not.
    s.send("disk --list | filter name == \"blk-0\"")?;
    s.expect("QEMU HARDDISK (QM00001)")?;
    s.expect("/home>")?;
    //          **And `--help` is an answer, not an error**: text on `stdout`, not `stderr`, which
    //          the shell paints as an error. Drawn plain first, then counted: on `stderr`, a
    //          pipeline's `count` sees no rows and says 0.
    let help_from = s.transcript().len();
    s.send("disk --help")?;
    s.expect("usage: disk --list")?;
    s.expect("/home>")?;
    let help = s.transcript()[help_from..].to_string();
    if help.contains("\x1b[91m") || help.contains("\x1b[93m") {
        return Err(format!("`disk --help` was drawn as a diagnostic, not as output: {help:?}").into());
    }
    s.send("format(\"help-rows={}\", (disk --help | count))")?;
    s.expect("help-rows=21")?;
    s.expect("/home>")?;
    //      (b) **`--mount` without the grant is refused before the service is asked**: the
    //          session's `/dev/storage/admin` has nothing answering, and `disk` names `with`.
    s.send("disk --mount /dev/blk/1")?;
    s.expect("need the storage grant")?;
    s.expect("/home>")?;
    //      (c) **With it, the request reaches the service**, whose own reason comes back: the ESP
    //          holds a FAT its server refuses for its 512-byte clusters. On a release boot nothing
    //          could mount, so the refusal is the evidence the grant arrived.
    with_admin_asked(s, "disk --mount /dev/blk/1")?;
    s.expect("it holds no filesystem this service can serve")?;
    s.expect("/home>")?;
    steps += 1;

    // 20d. **The policy, changed and put back.**
    //      (a) Outside a view with the `views` grant there is no `/dev/policy`; `with` names one.
    s.send("with --show")?;
    s.expect("with: cannot show the policy without the views grant")?;
    s.expect("/home>")?;
    let admin = |s: &mut Session, command: &str| -> R<()> { with_admin_asked(s, command) };
    //      (b) A copy of the policy, to put back at the end.
    admin(s, "with --show ./policy.txt")?;
    //          **And said as a notice**: the line on `stderr` names the file, where the console's
    //          says "a copy", so finding it proves the path — and it is drawn plain, not as an
    //          error. The two arrive in no order the guest promises.
    s.expect_all(&["with: wrote the policy to a copy", "with: wrote the policy to ./policy.txt ("])?;
    s.expect("/home>")?;
    if s.transcript().contains("\x1b[91mwith: wrote the policy to") {
        return Err("`with --show FILE`'s notice was drawn in the error colour".into());
    }
    //          **And with no file, on `stdout`**: the policy as text a pipeline can take. Counted
    //          against the copy just written, so the rows are the policy's own rather than any at
    //          all: on `stderr`, `count` sees none. Asked for its password, as every step here is,
    //          rather than leaning on (b)'s window lasting under TCG.
    s.send("with --forget")?;
    s.expect(&format!("view: {DEMO_USER} — remembered passwords forgotten"))?;
    s.send("format(\"shown={} saved={}\", (with admin with --show | count), (open ./policy.txt | count))")?;
    s.expect("[with admin] password (1 of 3): ")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("shown=")?;
    let counts = s.rest_of_line()?;
    let read =
        |label: &str| counts.split_whitespace().find_map(|w| w.strip_prefix(label)).and_then(|n| n.parse::<u64>().ok());
    match (read(""), read("saved=")) {
        (Some(shown), Some(saved)) if shown == saved && shown > 0 => {
            println!("  ok: `with --show` wrote the policy to `stdout`, {shown} lines as its saved copy");
        }
        _ => {
            return Err(format!(
                "`with --show` did not write the policy to `stdout` as its saved copy's rows: shown={counts}"
            )
            .into());
        }
    }
    s.expect("/home>")?;
    //      (c) **An edited copy, one view more**, typed as lists of lines — `save` writes a list of
    //          strings a line each. **In parts, each under the console's 256-byte input ring**
    //          (`kernel/src/drivers/console.rs`): a longer line can overrun it, lose its end and
    //          its newline, and never reach the shell. The parts are joined by `open`.
    let lines = |ls: &[&str]| {
        let quoted: Vec<String> = ls.iter().map(|l| format!("\"{}\"", l.replace('"', "\\\""))).collect();
        format!("[{}]", quoted.join(", "))
    };
    let part = |s: &mut Session, file: &str, ls: &[&str]| -> R<()> {
        let command = format!("{} | save ./{file}", lines(ls));
        if command.len() >= 240 {
            return Err(format!(
                "a typed line of {} bytes would overrun the console's 256-byte input ring",
                command.len()
            )
            .into());
        }
        s.send(&command)?;
        s.expect("/home>")?;
        Ok(())
    };
    part(
        s,
        "e1.txt",
        &[
            "[profile.admin]",
            "grants = [\"disks\", \"storage\", \"views\", \"accounts\"]",
            "[profile.install]",
            "grants = [\"disks\"]",
            "[profile.d2test]",
            "grants = []",
        ],
    )?;
    part(s, "e2.txt", &["[[rule]]", "who = [\"alice\"]", "use = [\"admin\"]", "run = [\"*\"]", "auth = \"password\""])?;
    part(
        s,
        "e3.txt",
        &["[[rule]]", "who = [\"alice\"]", "use = [\"install\"]", "run = [\"nxinstall\"]", "auth = \"password\""],
    )?;
    part(s, "e4.txt", &["[[rule]]", "who = [\"alice\"]", "use = [\"d2test\"]", "run = [\"nxsh\"]", "auth = \"none\""])?;
    s.send("open ./e1.txt ./e2.txt ./e3.txt ./e4.txt | map { |r| r.line } | save ./edited.txt")?;
    s.expect("/home>")?;
    admin(s, "with --install ./edited.txt")?;
    s.expect("with: install: the policy is installed")?;
    s.expect("/home>")?;
    //      (d) The broker reads the new policy for the next request, so the new view is there.
    let rows = "format(\"d2test-rows={}\", (with --list | filter view == \"d2test\" | count))";
    s.send(rows)?;
    s.expect("d2test-rows=1")?;
    s.expect("/home>")?;
    //      (e) **A copy nobody could administer is refused**: its admin profile has lost `views`.
    part(
        s,
        "orphaned.txt",
        &[
            "[profile.admin]",
            "grants = [\"disks\", \"storage\"]",
            "[[rule]]",
            "who = [\"alice\"]",
            "use = [\"admin\"]",
            "run = [\"*\"]",
            "auth = \"password\"",
        ],
    )?;
    admin(s, "with --install ./orphaned.txt")?;
    s.expect("no account that exists could use `views` for every program")?;
    s.expect("/home>")?;
    //      (f) The original, put back, and the view gone with it.
    admin(s, "with --install ./policy.txt")?;
    s.expect("with: install: the policy is installed")?;
    s.expect("/home>")?;
    s.send(rows)?;
    s.expect("d2test-rows=0")?;
    s.expect("/home>")?;
    steps += 1;

    // 20e. **An account's life at the real prompt**: `account`, a person's way to the accounts
    //      the view broker fronts. Bob is a fixture this step makes and removes; nothing persists.
    //      (a) `account --list` from any session: alice, administering, in this one.
    let rows = |what: &str| format!("format(\"{what}={{}}\", (account --list | filter name == \"{what}\" | count))");
    s.send("format(\"alice-row={}\", (account --list | filter name == \"alice\" | filter administers == true | filter sessions == 1 | count))")?;
    s.expect("alice-row=1")?;
    s.expect("/home>")?;
    s.send(&rows("bob"))?;
    s.expect("bob=0")?;
    s.expect("/home>")?;
    //      (b) **Added through the `accounts` grant**: alice's password for the view, then bob's
    //          twice, echo off, on the terminal `with` handed on.
    with_admin_asked(s, "account --add bob")?;
    s.expect("new password for bob: ")?;
    s.send(BOB_PASSWORD)?;
    s.expect("again: ")?;
    s.send(BOB_PASSWORD)?;
    s.expect("account: added bob, with a new home at /home/bob")?;
    s.expect("/home>")?;
    s.send(&rows("bob"))?;
    s.expect("bob=1")?;
    s.expect("/home>")?;
    //      (c) **Bob logs in** at alice's prompt, into the home the broker made, its folders in it.
    s.send("exit")?;
    s.expect("nitrox login:")?;
    s.send("bob")?;
    s.expect("password:")?;
    s.send(BOB_PASSWORD)?;
    s.expect("/home>")?;
    s.send("whoami")?;
    s.expect("bob")?;
    s.expect("/home>")?;
    s.send("list .")?;
    s.expect_all(&["Documents", "Downloads", "Pictures"])?;
    s.expect("/home>")?;
    //      (d) **His own password**, with no view: the current one, then a new one twice. A
    //          mismatch first, which changes nothing and is said before anything is sent.
    s.send("account --password")?;
    s.expect("current password: ")?;
    s.send(BOB_PASSWORD)?;
    s.expect("new password: ")?;
    s.send(BOB_NEW_PASSWORD)?;
    s.expect("again: ")?;
    s.send(BOB_MISTYPED_PASSWORD)?;
    s.expect("account: the two passwords did not match; nothing was changed")?;
    s.expect("/home>")?;
    s.send("account --password")?;
    s.expect("current password: ")?;
    s.send(BOB_PASSWORD)?;
    s.expect("new password: ")?;
    s.send(BOB_NEW_PASSWORD)?;
    s.expect("again: ")?;
    s.send(BOB_NEW_PASSWORD)?;
    s.expect("account: your password is changed")?;
    s.expect("/home>")?;
    s.send("exit")?;
    s.expect("nitrox login:")?;
    //      (e) The old password refused, and the new one taken.
    s.send("bob")?;
    s.expect("password:")?;
    s.send(BOB_PASSWORD)?;
    s.expect("login incorrect")?;
    s.expect("nitrox login:")?;
    s.send("bob")?;
    s.expect("password:")?;
    s.send(BOB_NEW_PASSWORD)?;
    s.expect("/home>")?;
    s.send("exit")?;
    s.expect("nitrox login:")?;
    //      (f) **Back as alice, bob removed** — he is logged out, so the broker lets it happen.
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    with_admin_asked(s, "account --remove bob --home")?;
    s.expect("account: removed bob, and /home/bob with it")?;
    s.expect("/home>")?;
    s.send(&rows("bob"))?;
    s.expect("bob=0")?;
    s.expect("/home>")?;
    s.send("exit")?;
    s.expect("nitrox login:")?;
    s.send("bob")?;
    s.expect("password:")?;
    s.send(BOB_NEW_PASSWORD)?;
    s.expect("login incorrect")?;
    s.expect("nitrox login:")?;
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    steps += 1;

    Ok(steps)
}

/// `test-interactive`'s steps 20f to 21: `service`, `date --set`, `log`, and a bare `exit` back
/// to the login prompt. The number of steps it ran.
pub(crate) fn interactive_services_and_relogin(s: &mut Session) -> R<usize> {
    let mut steps = 0usize;
    // 20f. **`service`, and a restart the session survives**.
    //      (a) `--list` is `service-mgr`'s table, from any session: the clipboard running.
    s.send("service --list | filter name == \"clipboard-server\"")?;
    s.expect("running")?;
    s.expect("/home>")?;
    //      (b) **A stop without the grant is refused before `service-mgr` is asked**: the
    //          session's `/dev/services/admin` is its session endpoint, where nothing answers.
    s.send("service --stop clipboard-server")?;
    s.expect("need the services grant")?;
    s.expect("/home>")?;
    //      (c) **An essential service is refused by `service-mgr` itself**, grant or not.
    with_admin_asked(s, "service --stop auth-service")?;
    s.expect("it is essential")?;
    s.expect("/home>")?;
    //      (d) **The clipboard restarted, from this session**, answered once the new one is up.
    with_admin_asked(s, "service --restart clipboard-server")?;
    s.expect("service: restarted clipboard-server")?;
    s.expect("/home>")?;
    //      (e) **And the same session copies and pastes through it.** It bound `/dev/clipboard` at
    //          login, before the restart, and what it reaches is `service-mgr`'s route into
    //          whichever clipboard server is running; bound to the server's own, `clip` finds
    //          it gone.
    s.send("\"after-restart\" | clip --copy")?;
    s.expect("/home>")?;
    s.send("clip")?;
    s.expect("after-restart")?;
    s.expect("/home>")?;
    steps += 1;

    // 20g. **`date --set`, and the clock it leaves.**
    //      (a) **Refused without the grant**, by the kernel: a session's programs are spawned
    //          without `SYSTEM_CLOCK`.
    s.send("date --set 2031-01-02T00:00:00Z")?;
    s.expect("needs the clock grant")?;
    s.expect("/home>")?;
    //      (b) **Set through the `clock` grant**: the kernel says the RTC took it, and `date`
    //          reports the clock after. The epoch value is matched, since the typed command
    //          holds the date.
    with_admin_asked(s, "date --set 2031-01-02T00:00:00Z --unix")?;
    s.expect("wall clock: set to 1925078400 (Unix epoch seconds, UTC); the RTC holds it")?;
    s.expect("1925078")?;
    s.expect("/home>")?;
    //      (c) **And another program reads it**, outside the view: the kernel's clock.
    s.send("date --unix")?;
    s.expect("1925078")?;
    s.expect("/home>")?;
    //      (d) **A shell in the view passes it on**: `nxsh` spawns its stages with what it holds.
    //          A day later, so the kernel's line is this set's and not (b)'s.
    with_admin_asked(s, "nxsh")?;
    s.expect("/home>")?;
    s.send("date --set 2031-01-03T00:00:00Z --unix")?;
    s.expect("wall clock: set to 1925164800 (Unix epoch seconds, UTC); the RTC holds it")?;
    s.expect("/home>")?;
    s.send("exit")?;
    s.expect("/home>")?;
    steps += 1;

    // 20h. **`log`, and the audit an earlier step wrote.**
    //      (a) **Refused without the grant**: a session has no `/dev/logs`, and `log` names it.
    s.send("log")?;
    s.expect("needs the logs grant")?;
    s.expect("/home>")?;
    //      (b) **Read back through the `logs` grant**: the view broker's records as a table. The
    //          same record went to the console when it was written, before this was typed, so
    //          what is matched here can only be the row.
    with_admin_asked(s, "log view-broker")?;
    s.expect("view: alice admin date — started")?;
    s.expect("/home>")?;
    //      (c) **The kernel's log**: refused without the `kernel-log` grant, and through it a row a
    //          line from the start of the kernel's log. That line went to the console at boot, long
    //          before anything here was typed, so a match can only be `log`'s row.
    s.send("log --kernel")?;
    s.expect("needs the kernel-log grant")?;
    s.expect("/home>")?;
    with_admin_asked(s, "log --kernel | take 4")?;
    s.expect("CPU tables installed (GDT/TSS/IDT)")?;
    s.expect("/home>")?;
    //          **And its last row is its last line**, 20g(d)'s clock, whose own echo the wait there
    //          consumed. The snapshot's last page is padded with NULs, which `log` must trim.
    with_admin_asked(s, "log --kernel | last 1")?;
    s.expect("wall clock: set to 1925164800")?;
    s.expect("/home>")?;
    steps += 1;

    // 21. A bare `exit` still returns to the login prompt, and logging in again works.
    //     **And the broker hears both**: the session that ended is closed, and the next login gets
    //     a *new* id. Ids are never reused — a program that ignored its session's end still holds
    //     a namespace with the old base, and a reused id would give its requests the next login's
    //     identity.
    s.send("exit")?;
    s.expect("view-broker: session ")?;
    let ended = s.rest_of_line()?;
    let ended_id = match ended.split_once(' ') {
        Some((id, "ended")) => id.parse::<u64>().ok(),
        _ => None,
    }
    .ok_or_else(|| format!("expected the broker to end the session, saw `{ended}`"))?;
    s.expect("nitrox login:")?;
    s.send("alice")?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("view-broker: session ")?;
    let reopened = s.rest_of_line()?;
    let new_id = match reopened.split_once(' ') {
        Some((id, "opened")) => id.parse::<u64>().ok(),
        _ => None,
    }
    .ok_or_else(|| format!("expected the broker to open a session, saw `{reopened}`"))?;
    if new_id <= ended_id {
        return Err(format!(
            "the broker reused a session id: session {ended_id} ended and the next login got {new_id}"
        )
        .into());
    }
    s.expect("/home>")?;
    steps += 1;

    Ok(steps)
}
