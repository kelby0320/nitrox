//! `check-terminal`: a click into `nxterm`, typing, and the shell's answer seen rendered.

use crate::*;

/// `cargo xtask check-terminal` — prove a keystroke reaches a shell and its answer comes back.
///
/// The whole loop in one assertion: i8042 -> `input-server` -> compositor -> `nxterm` ->
/// `tty-server` -> `nxsh` -> the grid. It clicks before it types, as `nxterm` is created first
/// and is bottom-most until click-to-focus raises it.
///
/// Asserted on the grid's contents, which `nxterm` reports line by line under `test-harness`. In
/// CI; it asserts where the press landed first, so a failure reports coordinates.
pub(crate) fn cmd_check_terminal(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::TestHarness)?;
    let ovmf = locate_ovmf()?;

    let work = repo_root().join("tools/build-cache");
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-terminal.sock");
    let _ = fs::remove_file(&qmp_sock);
    // QEMU's trace of the PS/2 keyboard's lights — see [`ps2_lights`].
    let led_log = work.join("terminal-leds.log");
    let _ = fs::remove_file(&led_log);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
        .arg("-chardev")
        .arg("stdio,id=hostserial,signal=off")
        .arg("-serial")
        .arg("chardev:hostserial")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot")
        .arg("-trace")
        .arg("ps2_set_ledstate")
        .arg("-trace")
        .arg("ps2_reset_keyboard")
        .arg("-D")
        .arg(&led_log)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    println!("xtask: terminal gate — booting and typing…\n");
    let mut session = Session::spawn(cmd, "check-terminal")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;
    qmp.screen = Some(size);

    // The grid's cell, recomputed on the host: `Metrics` takes a cell's width from one glyph's
    // advance, so the proportional face draws every column at the wrong x while the grid (cells)
    // passes. The size is read from the line because the theme's `font_px` is non-default.
    {
        session.expect("nxterm: grid font ")?;
        let line = session.rest_of_line()?;
        let (path, rest) = line
            .trim()
            .split_once(", cell ")
            .ok_or_else(|| format!("nxterm's grid line is not in the form this gate reads: {line:?}"))?;
        let (dims, px) = rest.split_once(" at ").ok_or_else(|| format!("nxterm's grid line has no size: {line:?}"))?;
        let (w, h) = dims.split_once('x').ok_or_else(|| format!("no cell size: {line:?}"))?;
        let (w, h): (u32, u32) = (w.parse()?, h.parse()?);
        let px: f32 = px.trim_end_matches("px").parse()?;
        let want = libterm::render::Metrics::new(&host_font(path)?, px);
        if (want.cell_w, want.cell_h) != (w, h) {
            return Err(format!(
                "the guest measured a {w}x{h} cell from {path} at {px}px; the host makes it \
                 {}x{} from the same file. Either the guest loaded a different font from the \
                 one it named — the proportional face is the live mistake here — or the two \
                 sides disagree about rasterisation, which no other gate would notice",
                want.cell_w, want.cell_h
            )
            .into());
        }
        println!("  ok: the grid measures a {w}x{h} cell from {path}, as the host does");
    }

    session.expect("nxterm: grid> nxsh: interactive shell")?;

    // Num Lock's light is on before anything is injected: the compositor resends after every input
    // pass whose locks differ, so asserting later would pass a compositor that skipped the first.
    wait_ps2_lights(&led_log, &[0, 2], "the compositor's start")?;

    // `nxsh` writes `OSC 7` beside every prompt and `nxterm` titles the window with it: the shell's
    // and the parser's host tests each pin their side of the bytes; this is where they meet.
    session.expect("nxterm: shell is at /")?;
    println!("  ok: the shell announced its directory and the terminal read it");

    // Wait for `ui-testclient`'s window churn to end: a click mid-churn raises whatever exists.
    session.expect("ui-testclient: PASSED")?;

    // Wait for `input-testclient`'s phase-1 idle line, not the later one: once its 2048x2048 window
    // exists it is topmost and takes every click and key, and a click sent earlier is its phase-1
    // sentinel that wakes it into that window.
    session.expect("input-testclient: idle before the window phase")?;

    // Aimed past the right edge of the 320-wide reference windows. `click_at` retries: QEMU's
    // 16-byte PS/2 queue holds what it cannot take and adds it to the next event, so a misplaced
    // press is a permanent offset (`expect_after_pointer`).
    click_at(&mut qmp, &mut session, 397, 295)?;

    // Wait for the click to land before typing: the raise is not instantaneous, and a focus change
    // is no signal, since the window may already have had focus.
    session.expect("nxterm: clicked")?;

    // One character at a time, each waited for: a word injected back to back races the raise and
    // the repaint. The echo wait is the assertion: each went `nxterm` -> `tty-server` -> `nxsh`.
    let mut typed = String::from("/> ");
    for k in ["w", "h", "o", "m", "i"] {
        qmp.send_key(k, true)?;
        qmp.send_key(k, false)?;
        typed.push_str(k);
        session.expect(&format!("nxterm: grid> {typed}"))?;
    }
    // **Left twice, then the missing letter**, through `nxterm`'s encoder. Each Left comes back as
    // the same row, waited for so the letter cannot overtake it.
    for _ in 0..2 {
        qmp.send_key("left", true)?;
        qmp.send_key("left", false)?;
        session.expect("nxterm: grid> /> whomi")?;
    }
    qmp.send_key("a", true)?;
    qmp.send_key("a", false)?;
    session.expect("nxterm: grid> /> whoami")?;
    println!("  ok: Left and a letter edited the line mid-way, through nxterm's encoder");

    // `nxterm` does not echo locally, so a matching grid line came back from the shell.
    qmp.send_key("ret", true)?;
    qmp.send_key("ret", false)?;
    session.expect("nxterm: grid> /> whoami")?;

    // The answer's text is not asserted: a shell in a window has no session, so `whoami`'s
    // `/session/user` is unbound (`TODO(gui-dev-tty)`).
    session.expect("nxterm: grid> ")?;

    type_at_terminal(&mut qmp, "remove /nothing-here")?;
    session.expect("nxterm: grid> remove: ")?;
    println!("  ok: a stage's diagnostic rendered in the grid, not on the kernel log");

    type_at_terminal(&mut qmp, "nxsh --help")?;
    session.expect("nxterm: grid> usage: nxsh [SCRIPT [ARG...] | -c SOURCE [ARG...]]")?;
    println!("  ok: `nxsh --help` rendered in the grid, not on the kernel log");

    // The title follows the shell's current directory; that it changes once, not per prompt, is
    // `a_repeated_announcement_is_not_a_change` (`expect` cannot assert an absence).
    type_at_terminal(&mut qmp, "cd /home")?;
    session.expect("nxterm: shell is at /home")?;
    println!("  ok: the terminal followed the shell into another directory");

    // Nothing typed so far changed a lock, so the lights are as the compositor first sent them.
    wait_ps2_lights(&led_log, &[0, 2], "the typing so far")?;
    let mut line = String::from("/home> ");
    let key = key_echoed;
    // Caps Lock inverts Shift for letters only: the quote is typed with it on, and a shifted letter
    // is lower case.
    press(&mut qmp, "caps_lock")?;
    wait_ps2_lights(&led_log, &[0, 2, 6], "Caps Lock on")?;
    key(&mut qmp, &mut session, &mut line, "apostrophe", true, "\"")?;
    key(&mut qmp, &mut session, &mut line, "a", false, "A")?;
    key(&mut qmp, &mut session, &mut line, "b", false, "B")?;
    key(&mut qmp, &mut session, &mut line, "a", true, "a")?;
    press(&mut qmp, "caps_lock")?;
    wait_ps2_lights(&led_log, &[0, 2, 6, 2], "Caps Lock off")?;
    key(&mut qmp, &mut session, &mut line, "c", false, "c")?;
    key(&mut qmp, &mut session, &mut line, "apostrophe", true, "\"")?;
    press(&mut qmp, "ret")?;
    // A top-level string is shown bare, so this is the answer, not the typed line and its prompt.
    session.expect("nxterm: grid> ABac")?;
    println!("  ok: Caps Lock upper-cased the letters it was on for, and only the letters");
    session.expect("nxterm: grid> /home>")?;
    line = String::from("/home> ");
    key(&mut qmp, &mut session, &mut line, "kp_1", false, "1")?;
    key(&mut qmp, &mut session, &mut line, "kp_2", false, "2")?;
    key(&mut qmp, &mut session, &mut line, "spc", false, " ")?;
    key(&mut qmp, &mut session, &mut line, "kp_add", false, "+")?;
    key(&mut qmp, &mut session, &mut line, "spc", false, " ")?;
    key(&mut qmp, &mut session, &mut line, "kp_3", false, "3")?;
    press(&mut qmp, "kp_enter")?;
    session.expect("nxterm: grid> 15")?;
    println!("  ok: the keypad typed digits and an operator, and its Enter ran the line");
    // With Num Lock off keypad 8 is Up, which recalls the previous line; under Num Lock it is an 8.
    session.expect("nxterm: grid> /home>")?;
    press(&mut qmp, "num_lock")?;
    wait_ps2_lights(&led_log, &[0, 2, 6, 2, 0], "Num Lock off")?;
    press(&mut qmp, "kp_8")?;
    session.expect("nxterm: grid> /home> 12 + 3")?;
    println!("  ok: with Num Lock off, keypad 8 was Up and recalled the line before");
    press(&mut qmp, "num_lock")?;
    wait_ps2_lights(&led_log, &[0, 2, 6, 2, 0, 2], "Num Lock on again")?;
    press(&mut qmp, "kp_enter")?;
    session.expect("nxterm: grid> 15")?;

    // F1, not a click on the bar (it is under `ui-testclient`'s windows), sent after the typing is
    // asserted: an open menu is topmost and takes the keyboard.
    qmp.send_key("f1", true)?;
    qmp.send_key("f1", false)?;
    session.expect("nxterm: menu popup ")?;
    let line = session.rest_of_line()?;
    let Some((_, px, py, pw, ph)) = parse_popup_line(&line) else {
        let _ = session.child.kill();
        return Err(format!("could not read the popup's geometry from {line:?}").into());
    };
    println!("  ok: the menu is a window: {line}");
    if pw == 0 || ph == 0 {
        let _ = session.child.kill();
        return Err(format!("the menu popup has no extent: {line:?}").into());
    }

    // Hover comes before the click (choosing closes the menu and destroys the popup before it could
    // paint hovered), and the row is found by walking because row size follows the theme. `Clear`
    // is `MENU_ROW_KEY + 6`; `nxterm::tests::the_gate_clicks_the_row_it_means` pins the number.
    const CLEAR_ROW: u64 = 106;
    let cx = px + pw as i32 / 2;
    let mut cy = py + 3;
    let mut found = None;
    while cy < py + ph as i32 - 3 {
        move_pointer_to(&mut qmp, cx, cy)?;
        // Recorded at each step, the one exception to recording only a confirmed press: the walk
        // starts from it, and left alone would re-walk the whole delta and run off the popup.
        qmp.pointer = Some((cx, cy));
        // The row the pointer settled on, not the first one it crossed: the approach's first packet
        // can land on `Clear` itself and be taken for arrival (see `Session::settled_line`).
        let over = session.settled_line(
            "nxterm: menu hover ",
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(400),
        )?;
        if over.as_deref() == Some(CLEAR_ROW.to_string().as_str()) {
            println!("  ok: the pointer settled over row {CLEAR_ROW}");
            found = Some(cy);
            break;
        }
        cy += 4;
    }
    let Some(cy) = found else {
        let seen: Vec<String> =
            session.transcript().lines().filter(|l| l.contains("nxterm: menu")).map(str::to_string).collect();
        return Err(format!(
            "the pointer walked {}..{} down the menu popup and the guest never reported row \
             {CLEAR_ROW} under it; it reported: {}",
            py + 3,
            py + ph as i32 - 3,
            if seen.is_empty() { String::from("nothing") } else { seen.join(", ") }
        )
        .into());
    };
    // The hover receipt is the position proof, so the position is known by evidence here.
    qmp.pointer = Some((cx, cy));
    click_at(&mut qmp, &mut session, cx, cy)?;
    session.expect("nxterm: menu chose Clear")?;

    // The popup takes the keyboard while up. Wait for `focus=1` first: the compositor announces the
    // terminal's focus only once the closing popup has gone, and an F1 sent on `chose Clear` alone
    // can arrive while it still holds the keyboard and open nothing.
    session.expect("nxterm: focus=1")?;
    qmp.send_key("f1", true)?;
    qmp.send_key("f1", false)?;
    session.expect("nxterm: menu popup ")?;
    let reopened = session.rest_of_line()?;
    let Some((_, ex, _, _, eh)) = parse_popup_line(&reopened) else {
        let _ = session.child.kill();
        return Err(format!("could not read the reopened popup's geometry from {reopened:?}").into());
    };

    // Left moves to the other menu, whose popup must be rebuilt: one reconciled on "a menu is open"
    // would draw the new rows into the old extent. The proof is a popup line of another height.
    qmp.send_key("left", true)?;
    qmp.send_key("left", false)?;
    session.expect("nxterm: menu popup ")?;
    let other = session.rest_of_line()?;
    let Some((_, fx2, _, _, fh)) = parse_popup_line(&other) else {
        let _ = session.child.kill();
        return Err(format!("could not read the File popup's geometry from {other:?}").into());
    };
    if fx2 == ex || fh == eh {
        let _ = session.child.kill();
        return Err(format!(
            "arrowing to the other menu did not rebuild the popup: Edit was {reopened:?}, \
             File is {other:?} — the window should move to File's word and shrink to its rows"
        )
        .into());
    }
    println!("  ok: arrowing rebuilt the popup under the other word: {other}");

    qmp.send_key("right", true)?;
    qmp.send_key("right", false)?;
    session.expect("nxterm: menu popup ")?;
    let _ = session.rest_of_line()?;
    // One Down lands on Paste, not Copy: Copy is greyed and arrowing skips it.
    qmp.send_key("down", true)?;
    qmp.send_key("down", false)?;
    qmp.send_key("ret", true)?;
    qmp.send_key("ret", false)?;
    session.expect("nxterm: menu chose Paste")?;
    println!("  ok: Down skipped the greyed row and Enter chose the next one");

    // A second window with a shell of its own, which one opened with nothing to talk to would look
    // the same as. The shell's line comes first: `open_window` starts it before it returns.
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(&mut qmp, "n")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    // Nothing between the last two: the third comes from a fresh `nxsh`, and its order against this
    // process's own `kprint` is a race.
    session.expect("nxterm: hosting a shell")?;
    session.expect("nxterm: opened another window")?;
    session.expect("nxsh: interactive shell")?;
    println!("  ok: Ctrl+Shift+N opened a second window with a shell of its own");

    // A second tab gets its own tty and shell in the binary; the receipt is the shell's. Shift, as
    // `Ctrl+T` belongs to what runs in the terminal.
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(&mut qmp, "t")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxterm: hosting a shell")?;
    session.expect("nxsh: interactive shell")?;
    println!("  ok: Ctrl+Shift+T opened a second tab with a shell of its own");

    let _ = session.child.kill();
    let _ = fs::remove_file(&qmp_sock);

    // Everything above is ordered. `boot-probe` exits somewhere between `ui-testclient: PASSED` and
    // `input-testclient: idle` in either order, so this reads the finished transcript instead.
    check_service_attribution(&session.finish().into_bytes())?;

    println!(
        "\nxtask: terminal gate PASSED — a keystroke reached the shell and its answer \
         reached the grid ✓"
    );
    Ok(())
}

/// Assert that `service-mgr` told its two children's exits apart (`TODO(child-exit-attribution)`):
/// `KIND_CHILD_EXITED` names a child by pid, so the discriminator is which control endpoint closed.
///
/// Here and not in `test-qemu`, where the probe's verdict ends the machine before an exit is
/// attributed. `code=0`, not a bare "exited": a supervisor that reads a closed control channel as
/// an exit prints `code=unknown`.
fn check_service_attribution(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("service-mgr: 'boot-probe' exited code=0") {
        return Err("service-mgr did not attribute boot-probe's exit with its status \
             (expected \"service-mgr: 'boot-probe' exited code=0\"). Check the transcript for \
             which it is: a \"boot-probe: … FAIL\" line means a gate failed and the probe \
             exited 1 — that is the real fault and this assertion is only how it surfaced; \
             \"code=unknown\" means the death was detected without a matching notification, \
             which is what an early control-handle close looks like; and no \"boot-probe\" \
             line at all means it never ran."
            .into());
    }
    // Nothing else was blamed: `heartbeat` is `policy = always`, so a misattributed exit shows as
    // its restart.
    if text.contains("service-mgr: restarting 'heartbeat'") {
        return Err("service-mgr restarted 'heartbeat', which never exited — \
             boot-probe's exit was misattributed to it"
            .into());
    }
    println!("xtask: service-mgr attributed boot-probe's exit to boot-probe ✓");
    Ok(())
}
