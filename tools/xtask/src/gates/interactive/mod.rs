//! `test-interactive`: the release image booted, and a real login and shell driven.

use crate::*;

mod admin;

pub(crate) use admin::*;

/// The shell's diagnostics reach a real terminal in the design's colour.
///
/// **On the bytes, because nothing else can see this.** `libterm` swallows SGR, so the graphical
/// gate reads the same grid text whether or not a diagnostic was painted; this gate is the one
/// place a diagnostic is printed to a terminal and the raw bytes are kept.
fn check_diagnostic_colour(transcript: &str) -> R<()> {
    let Some(at) = transcript.find("\x1b[91m") else {
        return Err("no diagnostic was printed in the design's colour: the transcript holds no \
                    `ESC[91m`, although this gate provokes several errors at a real prompt. \
                    Painting a path nothing calls looks exactly like this"
            .into());
    };
    // **The reset closes before the line ends**, for an error and for a warning: `tty_write_crlf`
    // makes a chunk of anything after a `\n`, so a reset on the far side of one is a blank line
    // with `ESC[0m` at the head of the next. Only wrong on the wire, so checked here.
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
/// This boots `BuildMode::Normal` — the **release image**, which nothing else ever boots; the
/// `test-harness` image differs by a service declaration and by `init`'s remaining cfgs.
///
/// **Expect-driven, not sleep-driven**: each step waits for the text that says the guest is
/// ready for it. One boot serves every scenario: the shell returns to `login:`.
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
        // **`signal=off` is what lets the guest see `Ctrl-C`**: QEMU's `stdio` chardev defaults to
        // `signal=on`, where `0x03` is QEMU's own interrupt. `-serial stdio` cannot say it.
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
            // **No password typed at a prompt reached the console**: every prompt turns echo
            // off, so one in the transcript is a prompt that did not.
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
            println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
            Err(e)
        }
    }
}

/// **`with admin COMMAND`, asked for the password** — after `with --forget`, so the prompt is there
/// whatever the steps before left remembered. For a step whose subject is not the grace window: no
/// gate waits on five minutes passing, or not, under TCG. The audit is waited for rather than the
/// shell's prompt, since the two reach the console by different paths.
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
    //    **And `service-mgr` asked the device manager for an info-only endpoint**, the one it
    //    couriers for every session's `/dev/devices`: only an info-only one keeps `desktop-shell`,
    //    which could bind it with no base, from subscribing through it. The manager logs before
    //    replying, so the order is causal: bind, mint, route, login.
    s.expect("service-mgr: device-mgr bound at /svc/devices")?;
    s.expect("device-mgr: an info-only endpoint minted")?;
    s.expect("service-mgr: sessions reach device-mgr through its info-endpoint")?;
    s.expect("nitrox login:")?;
    steps += 1;

    // 2. A wrong password is refused and the prompt comes back, tested before the good one so a
    //    broken denial cannot hide behind a successful login.
    s.send("alice")?;
    s.expect("password:")?;
    s.send(LOGIN_WRONG_PASSWORD)?;
    s.expect("login incorrect")?;
    s.expect("nitrox login:")?;
    steps += 1;

    // 3. The password prompt begins its own line: it once rendered as `alicepassword:`, the
    //    cursor never having left the username's line.
    s.send("alice")?;
    s.expect("\npassword:")?;
    steps += 1;

    // 4. A correct password reaches a shell prompt.
    s.send(DEMO_PASSWORD)?;
    // **Through the shared session core**: `session-mgr` calls `libsession::spawn_leader`, and this
    // line is that function's. The line names the program, so a supervisor that went back to
    // spawning `nxsh` directly would be silent here rather than passing.
    // **And a session with the view broker, bound in**: the namespace binds `/dev/views` at its
    // base. Reported from what was bound, so a session built without it says so.
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
    // **And the device manager's tables**, on the same line.
    if !built.contains("/dev/devices") {
        return Err(format!("the serial session was built without /dev/devices: ({built}").into());
    }
    // **And the storage service's filesystems and table.**
    if !built.contains("/storage") {
        return Err(format!("the serial session was built without /storage: ({built}").into());
    }
    s.expect("libsession: nxsh spawned into the session namespace")?;
    // **The banner names the build**: the commit this process built from.
    let commit = env::var(COMMIT_ENV).unwrap_or_default();
    s.expect(&format!("nxsh: interactive shell, Nitrox {commit} "))?;
    s.expect("/home>")?;
    steps += 1;

    // 5. A program from the profile runs: `/bin` is bound and the shell can spawn through it.
    s.send("whoami")?;
    // **And it was handed a terminal of its own**: the shell asks the tty server for a sibling
    // before each stage, and the server says how many are open. Kept for step 8.
    s.expect("tty-server: terminal opened beside another, ")?;
    let open_at_5 = s.rest_of_line()?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 5a. **The environment is what `session-mgr` built**: the shell starts in the user's home.
    s.send("format(\"athome={}\", ($env.PWD == $env.HOME))")?;
    s.expect("athome=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 5b. **Home is writable, and what was written reads back.** The fs endpoint is bound
    //     subtree-scoped to the principal's home: the sandbox working, not just a filesystem.
    s.send("[1, 2] | save ./nx-login.txt")?;
    s.expect("/home>")?;
    s.send("format(\"rows={}\", (open ./nx-login.txt | count))")?;
    s.expect("rows=2")?;
    s.expect("/home>")?;
    steps += 1;

    // 5c. **A directory read of the same bind finds it.** Naming the file is stronger than a
    //     count: a count of one passes on a home holding some *other* file.
    s.send("list .")?;
    s.expect("nx-login.txt")?;
    s.expect("/home>")?;
    steps += 1;

    // 5d. **The machine's devices, as typed tables.** `list` names a file per device and `open`
    //     decodes one into a table that a pipeline filters. The disk is matched on its **model**,
    //     which the command does not contain, so the echo of what was typed cannot satisfy it.
    s.send("list /dev/devices")?;
    s.expect_all(&["all.tsm", "blk-0.tsm", "input-0.tsm", "input-1.tsm"])?;
    s.expect("/home>")?;
    s.send("open /dev/devices/all.tsm | filter kind == \"disk\"")?;
    s.expect("QEMU HARDDISK")?;
    s.expect("/home>")?;
    // **And nothing to take a device with.** The base `/info` means `/dev/devices/block` reaches
    // the manager as `info/block`, a table name it refuses, rather than as `block`, the
    // subscription that would hand this session every disk. `/dev/registry` is root-only.
    s.send("open /dev/devices/block")?;
    s.expect("nxsh: cannot open `/dev/devices/block`")?;
    s.expect("/home>")?;
    s.send("open /dev/registry")?;
    s.expect("nxsh: cannot open `/dev/registry`")?;
    s.expect("/home>")?;
    steps += 1;

    // 5d'. **`time PIPELINE`**: before the value, a line saying how long it took, held to its
    //     shape — `time: `, which the typed command does not contain, then a number and a unit.
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

    // 5e. **The machine's filesystems.** The row of `/dev/storage` mounted at `/` is `init`'s
    //     root, matched on `blk-2` and `init`, which the command does not contain. `/storage` is
    //     the service at the base `/fs`: a release boot has nothing for it to mount, so it lists
    //     empty — and without an error, which says the binding reached a directory.
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
    steps += interactive_paths(s)?;
    steps += interactive_clipboard_and_completion(s)?;
    steps += interactive_administration(s)?;
    steps += interactive_services_and_relogin(s)?;
    Ok(steps)
}

/// Run `command` and wait for `prompt`, refusing any diagnostic in between: a `prompt` alone also
/// follows a failure, which would then surface steps later. A diagnostic is printed in red; a
/// child `nxsh`'s plain `nxsh: up` line is not one.
fn quietly(s: &mut Session, command: &str, prompt: &str) -> R<()> {
    let from = s.transcript().len();
    s.send(command)?;
    s.expect(prompt)?;
    let said = &s.transcript()[from..];
    if said.contains("\x1b[91m") {
        return Err(format!("`{command}` was refused:\n{said}").into());
    }
    Ok(())
}

/// Write the lines `body`, an nxsh list literal, to `path` (`.nx`), as text renamed: `save`
/// writes only `.txt` and `.tsm`.
fn write_script(s: &mut Session, body: &str, path: &str, prompt: &str) -> R<()> {
    let txt = format!("{}.txt", path.trim_end_matches(".nx"));
    quietly(s, &format!("{body} | save {txt}"), prompt)?;
    quietly(s, &format!("rename {txt} {path}"), prompt)
}

/// `test-interactive`'s steps 19a to 19c-3: the shell's own paths, from a real prompt.
fn interactive_paths(s: &mut Session) -> R<usize> {
    const AT: &str = "/home/Pictures>";
    // 19a. **`~` is HOME**, reached from another directory.
    quietly(s, "cd /home/Pictures", AT)?;
    quietly(s, "cd ~", "/home>")?;

    // 19b. **A script runs by its path, with arguments, and `use`s the module beside it.** Run
    //      from a sibling directory, so a `use` resolved against `PWD` reads nothing.
    quietly(s, "mkdir ./c1", "/home>")?;
    quietly(s, "mkdir ./c1/lib", "/home>")?;
    write_script(s, r#"["pub def twice(n) { n * 2 }"]"#, "./c1/lib/m.nx", "/home>")?;
    let main = concat!(
        r#"["use \"./lib/m.nx\" { twice }", "format(\"twice={} n={} b={}\", twice(21), "#,
        r#"($args | count), ($args | filter { |a| a == \"b\" } | count)) | display"]"#
    );
    write_script(s, main, "./c1/main.nx", "/home>")?;
    quietly(s, "cd /home/Pictures", AT)?;
    s.send("../c1/main.nx a b")?;
    s.expect("twice=42 n=2 b=1")?;
    s.expect(AT)?;
    //      And by `~`, which expands before the path resolves.
    s.send("~/c1/main.nx x")?;
    s.expect("twice=42 n=1 b=0")?;
    s.expect(AT)?;
    //      `-c` binds what follows its source, and a script that `use`s itself is refused.
    s.send(r#"nxsh -c "format(\"argc={}\", ($args | count)) | display" a b"#)?;
    s.expect("argc=2")?;
    s.expect(AT)?;
    write_script(s, r#"["use \"./self.nx\" as m"]"#, "~/c1/self.nx", AT)?;
    s.send("~/c1/self.nx")?;
    s.expect("is already being loaded")?;
    s.expect(AT)?;

    // 19c. **A `#!` line names the interpreter**: the host spawns `/bin/nxsh` with the file. Its
    //      path resolves as a head's does, so `//` folds.
    let bang = r##"["#!/bin//nxsh", "format(\"bang n={}\", ($args | count)) | display"]"##;
    write_script(s, bang, "~/c1/bang.nx", AT)?;
    s.send("~/c1/bang.nx p q r")?;
    s.expect("bang n=3")?;
    s.expect(AT)?;
    //      Refused: an interpreter that is not ELF, one that is not there, and none at all.
    write_script(s, r##"["#!/home/c1/lib/m.nx"]"##, "~/c1/nonelf.nx", AT)?;
    s.send("~/c1/nonelf.nx")?;
    s.expect("runs `/home/c1/lib/m.nx`, which is not itself a program")?;
    s.expect(AT)?;
    write_script(s, r##"["#!/nope/x"]"##, "~/c1/absent.nx", AT)?;
    s.send("~/c1/absent.nx")?;
    s.expect("`/nope/x` does not resolve")?;
    s.expect(AT)?;
    write_script(s, r##"["#!"]"##, "~/c1/bare.nx", AT)?;
    s.send("~/c1/bare.nx")?;
    s.expect("the `#!` line names no interpreter")?;
    s.expect(AT)?;

    // 19c-1. **A program by its path, and a directory is not one.**
    s.send("/bin/whoami")?;
    s.expect("alice")?;
    s.expect(AT)?;
    s.send("/home")?;
    s.expect("`/home` is not a program")?;
    s.expect(AT)?;

    // 19c-2. **An empty file is an empty script**: it runs, and says nothing.
    quietly(s, "touch ~/c1/e", AT)?;
    quietly(s, "~/c1/e", AT)?;

    // 19c-3. Tidied, from home.
    quietly(s, "cd ~", "/home>")?;
    quietly(s, "remove -r ./c1", "/home>")?;
    Ok(6)
}

/// `test-interactive`'s steps 6 to 19: shell language, history and Ctrl-C. The number of steps run.
fn interactive_shell_language(s: &mut Session, open_at_5: &str) -> R<usize> {
    let mut steps = 0usize;
    // 6. A *failing* stage reports and returns to the prompt: one exiting without writing once hung
    //    the shell, its pipe closing only when its process was reclaimed.
    s.send("list /nope")?;
    s.expect("filesystem unreachable")?;
    s.expect("/home>")?;
    steps += 1;

    // 7. The interpreter keeps state across lines — a `def` typed at the prompt is callable
    //    afterwards. **The answer is wrapped**, for the reason steps 12 and 13 give: a bare
    //    `expect("5")` passes on a `seq=` or `uptime_ns=` digit in the heartbeat logging as
    //    readily as on the shell's answer. **The call is nested**, so one line proves a
    //    generic operator and a user `def` dispatch together through the argument list.
    s.send("def add(a, b) { a + b }")?;
    s.expect("/home>")?;
    s.send("format(\"add={}\", add(2, 3))")?;
    s.expect("add=5")?;
    steps += 1;

    // 8. **History.** `\x1b[A` is Up; `send` appends the Enter. Recalling `whoami` proves the
    //    *buffer* was replaced; asserting on the redraw bytes would assert on the capture.
    s.send("whoami")?;
    // **The same command, so the same number of terminals** — unless one leaked. Each stage
    // since step 5 was handed a terminal; there are fifteen, so a leak would leave later stages
    // without one, silently: a stage is run without a terminal rather than refused.
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

    // 9. Two Ups reach `whoami` past `date`, which a history returning only its newest entry fails.
    s.send("date")?;
    s.expect("unix")?;
    s.expect("/home>")?;
    s.send("\x1b[A\x1b[A")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 10. **Reverse-search** (`\x12` is Ctrl-R), asserted on the command's output, not the redraw.
    s.send("\x12whoa")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 11. **Cancelling a search (Ctrl-G) restores what was typed**, so Ctrl-R is safe to press.
    s.send("date\x12who\x07")?;
    s.expect("unix")?;
    s.expect("/home>")?;
    steps += 1;

    // 12. **`break` at a real prompt**, and the multi-line path with it: the body's two statements
    // are newline-separated, so the loop spans two lines and the REPL's unclosed-brace
    // continuation carries it. **The assertion is `n=3`, not `3`**: `expect` scans forward, so a
    // bare digit finds the echo of the line just typed. **The increment comes before the test**:
    // with it last, a `break` that merely abandoned the iteration would leave `n` at 3 too.
    s.send("mut n = 0")?;
    s.expect("/home>")?;
    s.send("for x in 0..9 { n = n + 1\n if n == 3 { break } }")?;
    s.expect("/home>")?;
    s.send("format(\"n={}\", n)")?;
    s.expect("n=3")?;
    s.expect("/home>")?;
    steps += 1;

    // 13. **`parse` at a real prompt**, both directions: text becomes a number, and text that
    //     is not a number fails loud rather than becoming zero. `format` wraps the answer as
    //     at step 7.
    s.send("format(\"sum={}\", (\"40\" | parse Int) + 2)")?;
    s.expect("sum=42")?;
    s.expect("/home>")?;
    s.send("\"abc\" | parse Int")?;
    s.expect("cannot parse")?;
    s.expect("/home>")?;
    steps += 1;

    // 14. **Errors are constructible and recoverable, at a real prompt**: `fail` raises a
    // message, `catch` binds it as a record, `try` in value position supplies a fallback.
    // **The catch block formats its answer**: `expect("boom")` matched the echoed line and
    // passed with the `try`/`catch` removed; `caught=boom` appears only if the block ran.
    s.send("try { fail \"boom\" } catch (e) { format(\"caught={}\", e.message) }")?;
    s.expect("caught=boom")?;
    s.expect("/home>")?;
    s.send("let n = try { \"x\" | parse Int } catch { 8080 }")?;
    s.expect("/home>")?;
    s.send("format(\"n={}\", n)")?;
    s.expect("n=8080")?;
    s.expect("/home>")?;
    steps += 1;

    // 15. **Sequences**: a String is a sequence of its characters, and a Range of its values.
    s.send("format(\"len={}\", (\"hello\" | count))")?;
    s.expect("len=5")?;
    s.expect("/home>")?;
    s.send("format(\"sum={}\", (1..=10 | sum))")?;
    s.expect("sum=55")?;
    s.expect("/home>")?;
    s.send("\"a,b,c\" | split \",\" | join \"-\"")?;
    s.expect("a-b-c")?;
    s.expect("/home>")?;
    s.send("format(\"hit={}\", (2 in [1, 2]))")?;
    s.expect("hit=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 16. **`capture` at a real prompt**: match, take a group, convert it. The second line pins
    // the alternation fix — `~=` with a second branch that has to win was always false.
    s.send("let g = \"port 8080\" | capture /(\\d+)/")?;
    s.expect("/home>")?;
    s.send("format(\"port={}\", (g[1] | parse Int))")?;
    s.expect("port=8080")?;
    s.expect("/home>")?;
    s.send("format(\"alt={}\", (\"b\" ~= /a|b/))")?;
    s.expect("alt=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 17. **`Ctrl-C` interrupts a running evaluation.** `while true { }` has no exit and, with no
    // SIGINT, no way to be stopped. The loop and the interrupt go in **one write**: sent
    // separately, an empty loop reaches the ten-million-iteration backstop in under a second, so
    // the step would pass without the interrupt arriving. One write puts the byte in the guest
    // before evaluation starts, and the tty holds it until the shell's next checkpoint.
    s.send_raw("while true { }\n\x03")?;
    s.expect("interrupted")?;
    s.expect("/home>")?;
    s.send("format(\"alive={}\", 1 + 1)")?;
    s.expect("alive=2")?;
    s.expect("/home>")?;
    steps += 1;

    // 18. **`Ctrl-C` at a prompt discards the line** rather than leaving it half-typed.
    s.send_raw("garbage-that-should-vanish\x03")?;
    s.expect("^C")?;
    s.expect("/home>")?;
    s.send("format(\"clean={}\", 2 + 2)")?;
    s.expect("clean=4")?;
    s.expect("/home>")?;
    steps += 1;

    // 19. **`Ctrl-C` stops a running *stage*.** `sleep 60\n\x03` in one write does **not** test
    // this: the interrupt is already queued when the line is read, so the statement checkpoint
    // fires before the pipeline starts and nothing is spawned; sent separately it is a race the
    // test can only lose silently. So the guest supplies the anchor: two lines go in one write;
    // `started=1` is printed by the first, proving the second is *now running*, and only then is
    // the interrupt sent. Sixty seconds is far past the 45s expect timeout, so this can only
    // pass by `sleep` being cut short.
    s.send_raw("format(\"started={}\", 1)\nsleep 60\n")?;
    s.expect("started=1")?;
    s.send_raw("\x03")?;
    s.expect("interrupted")?;
    s.expect("/home>")?;
    steps += 1;

    // 19. **`list [x]` returns a prompt instead of locking the shell.** The word-mode scanner
    // produced an empty token at `[` without advancing, so the argument loop bumped it forever.
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
    // 19c. **The clipboard, from a pipeline**, whose whole point is that this resource is not
    // graphical-only. Four claims in order: a session's namespace carries `/dev/clipboard`; a
    // pipeline can push into the ring; a paste comes back out as text; and the ring is a *ring*,
    // so what was copied first is still reachable at index 1 after a second copy. Asserted from
    // the serial column deliberately: this proves the same server answers a process with no
    // window at all.
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
    // Index 1 is the one before it: a single-slot implementation would fail here.
    s.send("clip 1")?;
    s.expect("clip-one")?;
    s.expect("/home>")?;
    s.send("clip --list | count")?;
    s.expect("2")?;
    s.expect("/home>")?;
    steps += 1;

    // 19d. **Tab completion**, asserted through what the shell *ran* rather than what appeared:
    //      a completion is erase-and-rewrite bytes on a wire, so pressing Enter afterwards and
    //      checking the result asserts that the **buffer** was replaced. `send_raw` because Tab
    //      is a key, not a line: `send` would submit `who` as well.
    s.send_raw("who\t")?;
    s.send("")?;
    s.expect("alice")?;
    s.expect("/home>")?;

    //      A path, and the one place a completion has to know something a name lookup does
    //      not: `Pictures` is a directory, so it keeps its trailing slash and gets no space —
    //      which lets a second Tab descend into it.
    s.send_raw("cd /home/Pic\t")?;
    s.send("")?;
    s.expect("/home/Pictures")?;
    s.send("cd /home")?;
    s.expect("/home>")?;

    //      An ambiguous prefix shows the choice instead of guessing: `Do` matches two of the
    //      three staged folders; one more character then makes it unique.
    s.send_raw("cd /home/Do\t")?;
    s.expect("Documents/")?;
    s.expect("Downloads/")?;
    s.send_raw("c\t")?;
    s.send("")?;
    s.expect("/home/Documents")?;
    s.send("cd /home")?;
    s.expect("/home>")?;

    // **`..` completes**, which is path syntax rather than a directory entry — it can only come
    // from completion knowing what a path is.
    // **Two Tabs, and the first one is the test.** `cd ..` has to become `cd ../`, which is not
    // directly observable — so the second Tab lists through it, and `../bin/` can appear nowhere
    // else. As one Tab on `cd ../` this step passed with the dot entries removed: an
    // already-slashed word has an *empty* fragment and never asks about `..` at all.
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

    // And a word that matches nothing is **left alone**: the common prefix of no candidates is
    // the empty string, so Tab once replaced the word with it and deleted what you had typed. If
    // that came back, the line would be empty and a bare Enter would get a prompt, not this.
    s.send_raw("zzz\t")?;
    s.send("")?;
    s.expect("could not resolve `zzz`")?;
    s.expect("/home>")?;
    steps += 1;

    // 20. **`exit N` sets the status**, which `session-mgr` logs, so the argument form is
    // observable.
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
