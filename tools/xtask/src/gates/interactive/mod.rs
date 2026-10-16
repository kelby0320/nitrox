//! `test-interactive`: the release image booted, and a real login and shell driven.

use crate::*;

mod admin;

pub(crate) use admin::*;

/// The shell's diagnostics reach a real terminal in the design's colour.
///
/// **On the bytes, because nothing else can see this** (desktop refresh, Part F; PR #324 review,
/// blocking 1). `libterm` swallows SGR, so the graphical gate reads the same grid text whether
/// or not a diagnostic was painted — and every host test is of a function, not of the path a
/// message actually takes. This gate is the one place a diagnostic is *printed to a terminal*
/// and the raw bytes are kept.
///
/// The first version of this part painted `Host::diag`, which no interactive path calls: the
/// branch was dead and four documents said the shell coloured its diagnostics. This is what
/// would have caught it.
fn check_diagnostic_colour(transcript: &str) -> R<()> {
    let Some(at) = transcript.find("\x1b[91m") else {
        return Err("no diagnostic was printed in the design's colour: the transcript holds no \
                    `ESC[91m`, although this gate provokes several errors at a real prompt. \
                    Painting a path nothing calls looks exactly like this"
            .into());
    };
    // **And the reset closes before the line ends**, for an error and for a warning, which since
    // the laptop polish's Part E is painted the same way in a colour of its own (20b(d) requires
    // one). `tty_write_crlf` makes a chunk of anything after a `\n`, so a reset on the far side of
    // one is a blank line with `ESC[0m` at the head of the next. Checked here because the shape
    // only goes wrong on the wire.
    let warning = transcript.find("\x1b[93m");
    for at in core::iter::once(at).chain(warning) {
        let rest = &transcript[at + "\x1b[91m".len()..];
        let end = rest.find("\x1b[0m").ok_or("a diagnostic was opened in colour and never reset")?;
        let body = &rest[..end];
        if body.contains('\n') || body.contains('\r') {
            return Err(format!(
                "a diagnostic's colour spans a line ending ({body:?}): the reset belongs before it, \
                 or `tty_write_crlf` prints a blank line with `ESC[0m` on the next one"
            )
            .into());
        }
    }
    println!("  ok: a diagnostic reached the terminal in colour, reset before its line ended");
    Ok(())
}

/// **Interactive-session tests: drive the real login and shell over the serial console.**
///
/// This boots `BuildMode::Normal` — the **release image**, which nothing else ever boots.
///
/// **Why it exists**, in the past tense since 2026-08-21: `session-mgr` used to auto-log-in
/// and run a fixed script under `test-harness`, so the `login:` prompt, a typed password, a
/// real shell prompt and `exit` were all `#[cfg(not(feature = "test-harness"))]` code that CI
/// compiled and never executed. Every interactive bug this project has had lived exactly
/// there — the console read using the wrong rights, a `cd` guard refusing a builtin that
/// existed, a login that could not be repeated, a password prompt landing on the username's
/// line. Retrofit Part B deleted that substitution: `session-mgr` now has one `login()` in
/// every build, and this gate is what exercises it.
///
/// It still boots the only release image any gate boots, and that is still the point — the
/// `test-harness` image differs by a service declaration and by `init`'s remaining cfgs
/// (retrofit Part C), so "the same code" is a claim this gate is the only one able to test.
///
/// **Expect-driven, not sleep-driven.** Each step waits for the text that says the guest is
/// ready for it, so the run is paced by the guest rather than by guessed delays. That is
/// the difference between a test and a flake.
///
/// One boot serves every scenario: the shell returns to `login:`, so the session sequence
/// continues rather than paying ~15 s of boot per case.
pub(crate) fn cmd_test_interactive(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;
    let ovmf = locate_ovmf()?;

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, None)?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        // **`signal=off` is what lets the guest see `Ctrl-C`.** QEMU's `stdio` chardev
        // defaults to `signal=on`, where `0x03` on stdin is QEMU's own interrupt and never
        // reaches the guest — so the harness could type every key except the one §11h is
        // about. Spelled as an explicit chardev because `-serial stdio` has no way to say
        // it.
        .arg("-chardev")
        .arg("stdio,id=hostserial,signal=off")
        .arg("-serial")
        .arg("chardev:hostserial")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    println!("xtask: interactive session tests (release image, expect-driven)…\n");
    let mut session = Session::spawn(cmd, "test-interactive")?;
    let result = run_interactive_scenarios(&mut session);
    let transcript = session.finish();

    match result {
        Ok(n) => {
            check_diagnostic_colour(&transcript)?;
            // **No password typed at a prompt reached the console** (administration Part D.4).
            // Every prompt here turns echo off — the login, `with`'s, `account`'s — so a password
            // in the transcript is a prompt that did not, and it would be in every log that keeps
            // this one.
            if let Some(p) = TYPED_PASSWORDS.iter().position(|p| transcript.contains(p)) {
                return Err(format!(
                    "password {p} of TYPED_PASSWORDS appears in the serial transcript: a prompt echoed it"
                )
                .into());
            }
            println!("\nxtask: interactive tests PASSED ({n} steps)");
            Ok(())
        }
        Err(e) => {
            // The transcript is the diagnosis — without it a failure says only "expected X".
            println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
            Err(e)
        }
    }
}

/// **`with admin COMMAND`, asked for the password** — after `with --forget`, so the prompt is there
/// whatever the steps before left remembered (the laptop polish's Part A). For a step whose subject
/// is not the grace window: no gate waits on five minutes passing, or not, under TCG. The audit is
/// waited for rather than the shell's prompt, since the two reach the console by different paths.
pub(crate) fn with_admin_asked(s: &mut Session, command: &str) -> R<()> {
    s.send("with --forget")?;
    s.expect(&format!("view: {DEMO_USER} — remembered passwords forgotten"))?;
    s.send(&format!("with admin {command}"))?;
    s.expect("[with admin] password (1 of 3): ")?;
    s.send(DEMO_PASSWORD)?;
    Ok(())
}

/// The scenarios, in one boot. Returns the number of steps that passed.
fn run_interactive_scenarios(s: &mut Session) -> R<usize> {
    let mut steps = 0usize;

    // 1. The machine reaches a login prompt at all — the release image's first claim.
    //
    //    **And `service-mgr` asked the device manager for an info-only endpoint** (administration
    //    Part B.4, and `service-mgr`'s since Part E.1) — the one it couriers for every session's
    //    `/dev/devices`. This is the only place a gate sees that: the sessions below would list the
    //    same tables through a duplicate of the root endpoint, and only an info-only one keeps
    //    `desktop-shell`, which could bind it with no base, from subscribing through it. The
    //    manager logs before replying, so the order is causal: the bind, the mint straight after
    //    it — asked for each time the manager comes up, since Part E.1b — then `service-mgr`
    //    binding it behind the route the sessions are handed, then the login.
    s.expect("service-mgr: device-mgr bound at /svc/devices")?;
    s.expect("device-mgr: an info-only endpoint minted")?;
    s.expect("service-mgr: sessions reach device-mgr through its info-endpoint")?;
    s.expect("nitrox login:")?;
    steps += 1;

    // 2. A wrong password is refused and the prompt comes back. Tested before the good
    //    one so a broken denial cannot hide behind a successful login.
    s.send("alice")?;
    s.expect("password:")?;
    s.send(LOGIN_WRONG_PASSWORD)?;
    s.expect("login incorrect")?;
    s.expect("nitrox login:")?;
    steps += 1;

    // 3. The password prompt begins its own line. `read_line` used to return on CR/LF
    //    without echoing it, so the cursor never left the username's line and the prompt
    //    rendered as `alicepassword:`.
    s.send("alice")?;
    s.expect("\npassword:")?;
    steps += 1;

    // 4. A correct password reaches a shell prompt.
    s.send(DEMO_PASSWORD)?;
    // **Through the shared session core**, which is what makes this a check on M7 Part B
    // rather than only on the login. `session-mgr` no longer spawns the shell itself: it
    // calls `libsession::spawn_leader`, and this line is that function's. Without it the
    // refactor could be inert — a crate that compiles, is linked, and is not on the path —
    // which is exactly the shape of the title cap PR #233 shipped. The line names the
    // program, so a supervisor that went back to spawning `nxsh` directly would be silent
    // here rather than passing.
    //
    // **And a session with the view broker, bound in** (administration Part A.4): `session-mgr`
    // opens one before it builds the namespace, and the namespace binds `/dev/views` at its base.
    // Reported from what was bound, so a session built without it says so.
    s.expect("view-broker: session ")?;
    let opened = s.rest_of_line()?;
    if !opened.ends_with("opened") {
        return Err(format!("expected the broker to open a session, saw `{opened}`").into());
    }
    s.expect("session-mgr: session namespace built (")?;
    let built = s.rest_of_line()?;
    if !built.contains("/dev/views") {
        return Err(format!("the serial session was built without /dev/views: ({built}").into());
    }
    // **And the device manager's tables** (administration Part B.4), on the same line.
    if !built.contains("/dev/devices") {
        return Err(format!("the serial session was built without /dev/devices: ({built}").into());
    }
    // **And the storage service's filesystems and table** (administration Part C.6).
    if !built.contains("/storage") {
        return Err(format!("the serial session was built without /storage: ({built}").into());
    }
    s.expect("libsession: nxsh spawned into the session namespace")?;
    // **The banner names the build** (the laptop polish's Part D): the commit this process built
    // from, which every terminal now says before its first prompt.
    let commit = env::var(COMMIT_ENV).unwrap_or_default();
    s.expect(&format!("nxsh: interactive shell, Nitrox {commit} "))?;
    s.expect("/home>")?;
    steps += 1;

    // 5. A program from the profile runs — `/bin` is bound in the session namespace and
    //    the shell can spawn through it.
    s.send("whoami")?;
    // **And it was handed a terminal of its own** (administration Part A.2): the shell asks the
    // tty server for a sibling of its terminal before each stage, and the server says how many
    // terminals are open once it has made one. Kept, and compared at step 8.
    s.expect("tty-server: terminal opened beside another, ")?;
    let open_at_5 = s.rest_of_line()?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 5a. **The session's environment is what `session-mgr` built** — the shell starts in
    //     the principal's home, and `$env.HOME` names it.
    //
    //     Steps 5a–5c are the login proof, moved here from a script `session-mgr` ran under
    //     `test-harness` after auto-logging-in. It asserted the same three things in a build
    //     where the typed login above did **not exist** — `login()`, `tty_open` and the whole
    //     `tty_*` layer were `#[cfg(not(feature = "test-harness"))]`. Proving them against the
    //     release image is the point of `docs/planning/test-path-retrofit.md`; they land here
    //     *before* the script is deleted, so no coverage is lost in between.
    s.send("format(\"athome={}\", ($env.PWD == $env.HOME))")?;
    s.expect("athome=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 5b. **Home is writable, and what was written reads back.** The fs endpoint is bound
    //     subtree-scoped to the principal's home, so this is the sandbox working rather than
    //     a filesystem working: the same shell cannot reach anything above it.
    s.send("[1, 2] | save ./nx-login.txt")?;
    s.expect("/home>")?;
    s.send("format(\"rows={}\", (open ./nx-login.txt | count))")?;
    s.expect("rows=2")?;
    s.expect("/home>")?;
    steps += 1;

    // 5c. **A directory read of the same bind finds it.** The script asserted `list . | count`
    //     was at least one, which a count cannot express here; naming the file is stronger
    //     anyway — a count of one passes on a home holding some *other* file.
    s.send("list .")?;
    s.expect("nx-login.txt")?;
    s.expect("/home>")?;
    steps += 1;

    // 5d. **The machine's devices, as typed tables** (administration Part B.4). The session's
    //     `/dev/devices` is the device manager at the base `/info`: `list` names a file per
    //     device — the disk and both input devices among them, in whatever order it sorts — and
    //     `open` decodes one into a table that a pipeline filters with no device code of its own.
    //     The disk is matched on its **model**, which the command does not contain, so the echo
    //     of what was typed cannot satisfy it.
    s.send("list /dev/devices")?;
    s.expect_all(&["all.tsm", "blk-0.tsm", "input-0.tsm", "input-1.tsm"])?;
    s.expect("/home>")?;
    s.send("open /dev/devices/all.tsm | filter kind == \"disk\"")?;
    s.expect("QEMU HARDDISK")?;
    s.expect("/home>")?;
    // **And nothing to take a device with.** The base `/info` means `/dev/devices/block` reaches
    //     the manager as `info/block` — a table name without `.tsm`, which it refuses — rather
    //     than as `block`, the subscription that would hand this session every disk. And
    //     `/dev/registry` is bound in the root namespace only: what a session may know of the
    //     devices is the manager's tables, never the kernel's nodes.
    s.send("open /dev/devices/block")?;
    s.expect("nxsh: cannot open /dev/devices/block")?;
    s.expect("/home>")?;
    s.send("open /dev/registry")?;
    s.expect("nxsh: cannot open /dev/registry")?;
    s.expect("/home>")?;
    steps += 1;

    // 5d'. **`time PIPELINE`** (Phase 6 Part H.2): the pipeline's value comes through — the listing,
    //     the device manager's `io.tsm` in it — and when the pipeline ends, before the value is
    //     shown, a line saying how long it took and what the disks did, read from that table. The
    //     line is held to its shape: `time: `, which the typed command does not contain, then a
    //     number and a unit.
    let before = s.transcript().len();
    s.send("time list /dev/devices")?;
    s.expect("io.tsm")?;
    s.expect("/home>")?;
    let out = s.transcript()[before..].to_string();
    let line = out.split("time: ").nth(1).and_then(|rest| rest.lines().next()).unwrap_or("");
    let timed = line.starts_with(|c: char| c.is_ascii_digit())
        && [" us", " ms", " s"].iter().any(|u| line.split(';').next().is_some_and(|t| t.ends_with(u)));
    if !timed || out.find("time: ") > out.rfind("io.tsm") {
        return Err(format!("`time` printed no line of its shape before the listing: {out:?}").into());
    }
    steps += 1;

    // 5e. **The machine's filesystems** (administration Part C.6). The session's `/dev/storage` is
    //     the storage service at the base `/info`: `list` names a table per device, and the row
    //     mounted at `/` is `init`'s root, matched on `blk-2` and `init`, which the command does not
    //     contain. `/storage` is the service at the base `/fs`: a release boot has nothing for the
    //     service to mount, so it lists empty — and without an error, which is what says the
    //     binding reached a directory rather than nothing.
    s.send("list /dev/storage")?;
    s.expect_all(&["all.tsm", "blk-0.tsm", "blk-2.tsm"])?;
    s.expect("/home>")?;
    s.send("open /dev/storage/all.tsm | filter mounted == \"/\"")?;
    s.expect_all(&["blk-2", "init"])?;
    s.expect("/home>")?;
    let before = s.transcript().len();
    s.send("list /storage")?;
    s.expect("/home>")?;
    let listed = s.transcript()[before..].to_string();
    if listed.contains("cannot") || listed.contains("nxsh:") {
        return Err(format!("`list /storage` failed where it should list nothing: {listed:?}").into());
    }
    // **And it is the filesystems, not the tables.** Both bindings are one endpoint at two bases;
    // `/storage` at the tables' base would list without an error too.
    if listed.contains(".tsm") {
        return Err(format!("`list /storage` listed tables — it is bound at the wrong base: {listed:?}").into());
    }
    steps += 1;

    steps += interactive_shell_language(s, &open_at_5)?;
    steps += interactive_clipboard_and_completion(s)?;
    steps += interactive_administration(s)?;
    steps += interactive_services_and_relogin(s)?;
    Ok(steps)
}

/// `test-interactive`'s steps 6 to 19: a failing stage, state across lines, history and search,
/// `break`, `parse`, errors, sequences, `capture` and Ctrl-C. The number of steps it ran.
fn interactive_shell_language(s: &mut Session, open_at_5: &str) -> R<usize> {
    let mut steps = 0usize;
    // 6. A *failing* stage reports and returns to the prompt rather than hanging. This is
    //    the shape that hung the shell for an afternoon: a stage that exits without
    //    writing, whose pipe only closes once its process is reclaimed.
    s.send("list /nope")?;
    s.expect("filesystem unreachable")?;
    s.expect("/home>")?;
    steps += 1;

    // 7. The interpreter keeps state across lines — a `def` typed at the prompt is
    //    callable afterwards.
    //
    //    **The answer is wrapped, for the reason steps 12 and 13 give.** It was `add(2, 3)`
    //    asserted with a bare `expect("5")`, which is the weakest pattern in the gate: a
    //    single character, in a stream that also carries heartbeat and service-manager
    //    logging. A transcript of one passing run contains 22 occurrences of `5`, fourteen
    //    of them before this step even runs — so the step passed on a `seq=` or `uptime_ns=`
    //    digit as readily as on the shell's answer, and was observed doing exactly that.
    //
    //    **The call is nested now**, which is what this step is really for: `format` is a
    //    generic operator and `add` a user `def`, so one line proves the two dispatch
    //    together through the argument list. It could not be written until 2026-09-10 —
    //    a call inside an argument list was a parse error, and the two-line workaround
    //    that stood here carried a comment saying so (`shell-nested-call`).
    s.send("def add(a, b) { a + b }")?;
    s.expect("/home>")?;
    s.send("format(\"add={}\", add(2, 3))")?;
    s.expect("add=5")?;
    steps += 1;

    // 8. **History.** `\x1b[A` is Up; `send` appends the Enter. Recalling `whoami` and
    //    running it proves the *buffer* was replaced, which is the part that matters —
    //    the erase-and-redraw is bytes on a wire that only a real terminal renders, so
    //    asserting on appearance would assert on the capture rather than the shell.
    s.send("whoami")?;
    // **The same command, so the same number of terminals** — unless one leaked. Every stage
    // since step 5 was handed a terminal, and each should have been freed when its stage exited.
    // There are fifteen, so a leak here would leave every stage after a dozen commands without
    // one, silently: a stage is run without a terminal rather than refused.
    s.expect("tty-server: terminal opened beside another, ")?;
    let open_at_8 = s.rest_of_line()?;
    if open_at_8 != open_at_5 {
        return Err(
            format!("a stage's terminal outlived its stage: {open_at_5} at step 5, {open_at_8} at step 8").into()
        );
    }
    s.expect("alice")?;
    s.expect("/home>")?;
    s.send("\x1b[A")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 9. Two Ups walk further back: with `date` most recent, the second Up reaches
    //    `whoami` again. This is what distinguishes a working cursor from a history that
    //    only ever returns its newest entry.
    s.send("date")?;
    s.expect("unix")?;
    s.expect("/home>")?;
    s.send("\x1b[A\x1b[A")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 10. **Reverse-search.** `\x12` is Ctrl-R; the query narrows to `whoami`, and Enter
    //     runs it. Asserting on the command's *output* again, not on the search prompt —
    //     the redraw is erase-and-rewrite, which only a real terminal renders.
    s.send("\x12whoa")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 11. **Cancelling a search restores what was being typed.** Type `date`, search for
    //     something else, abandon it with Ctrl-G, press Enter — `date` must run. This is
    //     the property that makes Ctrl-R safe to press by accident.
    s.send("date\x12who\x07")?;
    s.expect("unix")?;
    s.expect("/home>")?;
    steps += 1;

    // 12. **`break` at a real prompt** — and the multi-line path with it. The body holds
    //     two statements, which are newline-separated (§9a), so the loop spans two lines
    //     and the REPL's unclosed-brace continuation carries it (§11b). The `mut` survives
    //     from the line before, which is what makes the count observable at all. Host
    //     tests cover the semantics; this covers the path they cannot — every expensive
    //     bug this shell has had was interactive-only.
    //
    //
    //     Two things here were got wrong first and are worth keeping right. **The
    //     assertion is `n=3`, not `3`**: `expect` scans forward until it matches, so a
    //     bare digit finds the echo of the line that was just typed. And **the increment
    //     comes before the test**, so that "the loop stopped" and "the rest of the body
    //     was skipped" give different answers — with the increment last, a `break` that
    //     merely abandoned the iteration would leave `n` at 3 as well, and the step would
    //     pass against a broken implementation. Both were found by breaking `break` and
    //     watching this step keep passing.
    s.send("mut n = 0")?;
    s.expect("/home>")?;
    s.send("for x in 0..9 { n = n + 1\n if n == 3 { break } }")?;
    s.expect("/home>")?;
    s.send("format(\"n={}\", n)")?;
    s.expect("n=3")?;
    s.expect("/home>")?;
    steps += 1;

    // 13. **`parse` at a real prompt**, both directions of §6's contract: text becomes a
    //     number and takes part in arithmetic, and text that is not a number fails loud
    //     rather than becoming zero. `format` wraps the answer for the same reason as the
    //     step above — `42` on its own would match the echo of what was typed, `sum=42`
    //     cannot.
    s.send("format(\"sum={}\", (\"40\" | parse Int) + 2)")?;
    s.expect("sum=42")?;
    s.expect("/home>")?;
    s.send("\"abc\" | parse Int")?;
    s.expect("cannot parse")?;
    s.expect("/home>")?;
    steps += 1;

    // 14. **Errors are constructible and recoverable, at a real prompt.** `fail` raises
    //     with a message a script chose, `catch` binds it as an ordinary record, and `try`
    //     in value position supplies a fallback — the three halves of §2 that did not
    //     exist before Part C. `n=8080` rather than `8080` for the reason step 12 gives.
    //
    //     **The catch block formats its answer** so the assertion cannot be satisfied by the
    //     terminal's echo of the command. `expect("boom")` matched the echoed line — the
    //     word is in the text being typed — and so passed with the `try`/`catch` removed
    //     from the command entirely. `caught=boom` appears only if the block ran.
    s.send("try { fail \"boom\" } catch (e) { format(\"caught={}\", e.message) }")?;
    s.expect("caught=boom")?;
    s.expect("/home>")?;
    s.send("let n = try { \"x\" | parse Int } catch { 8080 }")?;
    s.expect("/home>")?;
    s.send("format(\"n={}\", n)")?;
    s.expect("n=8080")?;
    s.expect("/home>")?;
    steps += 1;

    // 15. **Sequences and reduction** at a real prompt: a String is a sequence of its
    //     characters (so `count` is its length), and a Range is a sequence of its values
    //     (so it can be summed without first being written out as a list). Both were
    //     "expected a Table or a List" before Part D.
    s.send("format(\"len={}\", (\"hello\" | count))")?;
    s.expect("len=5")?;
    s.expect("/home>")?;
    s.send("format(\"sum={}\", (1..=10 | sum))")?;
    s.expect("sum=55")?;
    s.expect("/home>")?;
    // …and Part E's breadth: text taken apart and put back, and membership.
    s.send("\"a,b,c\" | split \",\" | join \"-\"")?;
    s.expect("a-b-c")?;
    s.expect("/home>")?;
    s.send("format(\"hit={}\", (2 in [1, 2]))")?;
    s.expect("hit=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 16. **`capture` at a real prompt**, the shape §10b writes: match, take a group,
    //     convert it. Text that matched could only be tested before, never taken apart.
    //     The second line pins the alternation fix — `~=` with a second branch that has to
    //     win was false for the entire life of the engine.
    s.send("let g = \"port 8080\" | capture /(\\d+)/")?;
    s.expect("/home>")?;
    s.send("format(\"port={}\", (g[1] | parse Int))")?;
    s.expect("port=8080")?;
    s.expect("/home>")?;
    s.send("format(\"alt={}\", (\"b\" ~= /a|b/))")?;
    s.expect("alt=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 17. **`Ctrl-C` interrupts a running evaluation** (§11h) — the hazard that section
    //     exists for. `while true { }` has no exit and, on a system with no SIGINT, no way
    //     to be stopped: it meant a reboot.
    //
    //     The loop and the interrupt go in **one write** deliberately. Sent separately it
    //     is a race the test loses: an empty loop reaches the ten-million-iteration
    //     backstop in well under a second, so the run would end on its own and the step
    //     would pass without the interrupt ever arriving. One write puts the byte in the
    //     guest before evaluation starts, and the tty holds it until the shell's next
    //     checkpoint asks.
    s.send_raw("while true { }\n\x03")?;
    s.expect("interrupted")?;
    s.expect("/home>")?;
    // …and the shell is still usable afterwards, which is the actual claim.
    s.send("format(\"alive={}\", 1 + 1)")?;
    s.expect("alive=2")?;
    s.expect("/home>")?;
    steps += 1;

    // 18. **`Ctrl-C` at a prompt discards the line** rather than leaving it half-typed —
    //     the same event, and the shell decides what it means from what it was doing.
    s.send_raw("garbage-that-should-vanish\x03")?;
    s.expect("^C")?;
    s.expect("/home>")?;
    s.send("format(\"clean={}\", 2 + 2)")?;
    s.expect("clean=4")?;
    s.expect("/home>")?;
    steps += 1;

    // 19. **`Ctrl-C` stops a running *stage*** (§11h, G2).
    //
    //     The ordering is made deterministic rather than assumed, and that took two goes.
    //     Sending `sleep 60\n\x03` in one write does **not** test this: the interrupt is
    //     already queued when the line is read, so the shell's statement checkpoint fires
    //     before the pipeline ever starts and nothing is ever spawned — it passes without
    //     a stage being involved. Sending the interrupt separately is a race the test can
    //     only lose silently.
    //
    //     So the guest supplies the anchor. Two lines go in one write; `started=1` is
    //     printed by the first, which proves the second is *now running*, and only then is
    //     the interrupt sent. Sixty seconds is far past the 45s expect timeout, so this can
    //     only pass by `sleep` actually being cut short.
    s.send_raw("format(\"started={}\", 1)\nsleep 60\n")?;
    s.expect("started=1")?;
    s.send_raw("\x03")?;
    s.expect("interrupted")?;
    s.expect("/home>")?;
    steps += 1;

    // 19. **`list [x]` returns a prompt instead of locking the shell.** The word-mode
    //     scanner produced an empty token at `[` without advancing, so the argument loop
    //     bumped it forever — a user-reachable hang on a path that predates every part of
    //     Milestone 4, and one with no escape until Ctrl-C exists (§11h). It is asserted
    //     in guest because "the shell is still there" is the whole claim.
    s.send("list [x]")?;
    s.expect("cannot begin a bareword")?;
    s.expect("/home>")?;
    steps += 1;

    Ok(steps)
}

/// `test-interactive`'s steps 19c to 20: the clipboard from a pipeline, tab completion, and
/// `exit N`. The number of steps it ran.
fn interactive_clipboard_and_completion(s: &mut Session) -> R<usize> {
    let mut steps = 0usize;
    // 19c. **The clipboard, from a pipeline** — M12 Part E and decision 4, whose whole point is
    //      that this resource is not graphical-only. Four claims in order: a session's namespace
    //      carries `/dev/clipboard`; a pipeline can push into the ring; a paste comes back out
    //      as text; and the ring is a *ring*, so what was copied first is still reachable at
    //      index 1 after a second copy.
    //
    //      **Asserted from the serial column deliberately.** The graphical gate proves copy in
    //      one application and paste in another; this proves the same server answers a process
    //      with no window at all, which is the half a windowed test cannot see.
    s.send("\"clip-one\" | clip --copy")?;
    s.expect("/home>")?;
    s.send("clip")?;
    s.expect("clip-one")?;
    s.expect("/home>")?;
    s.send("\"clip-two\" | clip --copy")?;
    s.expect("/home>")?;
    s.send("clip")?;
    s.expect("clip-two")?;
    s.expect("/home>")?;
    // Index 1 is the one before it — the property that makes this a kill ring rather than a
    // slot, and the one a single-slot implementation would fail while passing everything above.
    s.send("clip 1")?;
    s.expect("clip-one")?;
    s.expect("/home>")?;
    // …and the listing says how much is there without saying what it is.
    s.send("clip --list | count")?;
    s.expect("2")?;
    s.expect("/home>")?;
    steps += 1;

    // 19d. **Tab completion** (§11c), asserted through what the shell *ran* rather than
    //      through what appeared. A completion is erase-and-rewrite bytes on a wire, so
    //      matching on them would assert on the capture; pressing Enter afterwards and
    //      checking the result asserts that the **buffer** was replaced, which is the part
    //      that matters. Step 8 makes the same argument for history recall.
    //
    //      `send_raw` because Tab is a key, not a line: `send` would submit `who` as well.
    s.send_raw("who\t")?;
    s.send("")?;
    s.expect("alice")?;
    s.expect("/home>")?;

    //      A path, and the one place a completion has to know something a name lookup does
    //      not: `Pictures` is a directory, so it keeps its trailing slash and gets no space
    //      — which is what lets a second Tab descend into it. The prompt is the `PWD`, so
    //      it says whether the whole path arrived.
    s.send_raw("cd /home/Pic\t")?;
    s.send("")?;
    s.expect("/home/Pictures")?;
    s.send("cd /home")?;
    s.expect("/home>")?;

    //      An ambiguous prefix shows the choice instead of guessing. `Do` matches two of
    //      the three staged folders, so nothing can be typed for you and the list is the
    //      only useful answer; one more character then makes it unique.
    s.send_raw("cd /home/Do\t")?;
    s.expect("Documents/")?;
    s.expect("Downloads/")?;
    s.send_raw("c\t")?;
    s.send("")?;
    s.expect("/home/Documents")?;
    s.send("cd /home")?;
    s.expect("/home>")?;

    //      **`..` completes**, which is path syntax rather than a directory entry — nothing
    //      in a listing is called `..`, so it can only come from completion knowing what a
    //      path is.
    //
    //      **Two Tabs, and the first one is the test.** `cd ..` has to become `cd ../`,
    //      which is not directly observable — so the second Tab lists through it, and
    //      `../bin/` is a token that can appear nowhere else. Written as one Tab on `cd ../`
    //      this step passed with the dot entries removed: an already-slashed word has an
    //      *empty* fragment and never asks about `..` at all, so it was testing path
    //      resolution and reporting it as completion.
    s.send_raw("cd ..\t")?;
    s.send_raw("\t")?;
    s.expect("../bin/")?;
    s.send_raw("\x03")?;
    s.expect("/home>")?;

    //      **A continuation line completes as part of the statement it continues.** The
    //      discipline is reset per physical line while the shell accumulates the earlier
    //      ones, so a completion handed only the physical line sees column 0 and offers
    //      command names inside an argument list. Nothing that starts with `Do` is a
    //      command, so without the accumulated half this produces no listing at all.
    s.send("format(\"{}\",")?;
    s.expect("... ")?;
    s.send_raw("Do\t")?;
    s.expect("Documents/")?;
    s.send_raw("\x03")?;
    s.expect("/home>")?;

    //      And a word that matches nothing is **left alone**. This was the bug the `..`
    //      report found, and it was never about `..`: the common prefix of no candidates is
    //      the empty string, so Tab replaced the word with it and deleted what you had
    //      typed. If that came back, the line here would be empty and the shell would answer
    //      a bare Enter with a prompt rather than with this.
    s.send_raw("zzz\t")?;
    s.send("")?;
    s.expect("could not resolve `zzz`")?;
    s.expect("/home>")?;
    steps += 1;

    // 20. **`exit N` sets the status**, which `session-mgr` logs — so the argument form is
    //     observable rather than merely "the shell left". Before Part C the driver matched
    //     the literal line `exit`, so `exit 3` missed it entirely and came back as
    //     "`exit` is handled by the shell's driver".
    s.send("exit 3")?;
    s.expect("shell exit 3")?;
    s.expect("nitrox login:")?;
    s.send("alice")?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    steps += 1;

    Ok(steps)
}
