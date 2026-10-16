use crate::*;

///
/// **The whole loop, in one assertion**: i8042 → `input-server` → compositor → `nxterm` →
/// `tty-server` → `nxsh` → back out → the terminal's grid. Every piece of it has its own test;
/// none of those can tell you the pieces are joined.
///
/// **Asserted on the grid's contents, not on pixels.** What a shell prints is not fixed by this
/// milestone, so comparing pixels would pin it — and the display gate already compares a
/// *fixed* terminal render, which is the part that must not drift. Under `test-harness`,
/// `nxterm` reports each completed grid line on the debug console; that is what this reads.
///
/// **It clicks before it types**, which is not ceremony. `nxterm` is created first so it sits at
/// the bottom of the stack (windows stack at the origin in creation order and it is the
/// largest — see `init`), and keys follow the *topmost focusable* window. Click-to-focus raises
/// it, which is both how a user would do it and the only mechanism available: there is no op to
/// raise a window, and there will not be until Milestone 6.
/// `cargo xtask check-terminal` — prove a keystroke reaches a shell and its answer comes back.
///
/// **Status 2026-08-13 (superseding an earlier status the same day): the flake was real, and it
/// was not in the harness.** This gate spent two milestones failing, and the standing diagnosis
/// here blamed "driving a GUI from QMP" and suspected an `input-server` bug where a consumer
/// disconnecting cost the others their stream. **Both were wrong**, and the second sent at least
/// one investigation after a bug that does not exist.
///
/// The cause was a kernel deadlock: the boot self-test's verdict syscall parked a CPU that the
/// scheduler still counted online, so the next TLB shootdown — any large `free` in any process —
/// waited forever for an acknowledgement it could never get. `nxterm` froze mid-repaint while
/// freeing a glyph outline, which is why it looked like a terminal that stops on a particular
/// keystroke. Fixed 2026-08-13; see the decision log.
///
/// One piece of the old diagnosis survives and is still load-bearing below: **a keystroke is not
/// cheap.** The terminal repaints, waits for a free buffer and copies a window of pixels before
/// it looks at input again, so keys injected as fast as QMP sends them outrun it. Pacing on the
/// echo is the fix, and it is what the typing loop does.
///
/// **Wired into CI** since 2026-08-18 (`.github/workflows/ci.yml`, the QEMU integration job,
/// unconditional), on 64 consecutive passes. The bar was never the count: the audit logged one
/// unreproduced failure at the click step, and what made promotion defensible is that this gate
/// now asserts *where* the press landed before asserting that `nxterm` received it, so a
/// recurrence reports coordinates rather than a bare timeout. That failure is still
/// unexplained. The deferral entry that used to hold this record has moved to the resolved
/// table; the live record is now `docs/decision-log.md`, 2026-08-18.
pub(crate) fn cmd_check_terminal(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::TestHarness)?;
    let ovmf = locate_ovmf()?;

    let work = repo_root().join("tools/build-cache");
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-terminal.sock");
    let _ = fs::remove_file(&qmp_sock);
    // QEMU's trace of the PS/2 keyboard's lights (Phase 6 Part B.5) — see [`ps2_lights`].
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

    // **The grid's cell, recomputed on the host from the same file** (M11 Part D). Since the
    // desktop's font became proportional, `nxterm` is the one program that loads two — and
    // handing the wrong one to `libterm` does not fail: `Metrics` takes a cell's width from a
    // single glyph's advance, so a proportional face yields a plausible number and then draws
    // every column at the wrong x. Everything else this gate checks would still pass, because
    // the grid the harness reports is cells and not pixels.
    //
    // So the guest says which face it measured with and what it got, and the host measures the
    // same file at the same size and compares. The size is taken from the line rather than
    // assumed: the image stages a theme with a deliberately non-default `font_px`, and pinning
    // the number here would be pinning the theme file to this gate.
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

    // The shell is up in the window: its banner reached the grid, which means the whole
    // output direction already works before a key is injected.
    session.expect("nxterm: grid> nxsh: interactive shell")?;

    // **Num Lock's light is on before anything is injected** (Phase 6 Part B.5): the compositor
    // sends the lights when it connects, and Num Lock is on from the start. Asserted here, ahead of
    // the first key and the first motion, because the compositor also sends after every input pass
    // whose locks differ from what it last sent — so a compositor that skipped the first send would
    // pass the same assertion made after any typing. The reset's own zero comes first.
    wait_ps2_lights(&led_log, &[0, 2], "the compositor's start")?;

    // **And the shell told the window where it is** (desktop refresh, Part K). `nxsh` writes
    // `OSC 7` beside every prompt, `libterm::parse` reads it and `nxterm` puts it in the title
    // bar as the design's subtitle. Three crates, each with host tests of its own that cannot
    // see the other two: the shell's test pins the bytes it writes, the parser's pins the bytes
    // it reads, and nothing on the host makes them the same bytes. This line is where they meet.
    session.expect("nxterm: shell is at /")?;
    println!("  ok: the shell announced its directory and the terminal read it");

    // Wait for `ui-testclient` to finish churning windows before touching focus — a click
    // landing mid-churn raises whatever exists at that instant. Same reason `check-input`
    // waits for it.
    session.expect("ui-testclient: PASSED")?;

    // **And wait for `input-testclient` to leave.** It creates a 2048×2048 window — larger
    // than the screen — as its second phase, and while that exists it is the topmost window
    // everywhere and takes every click and keystroke. Undriven, it gives up after an idle
    // deadline and says so. Waiting is not optional: a click sent before this *is* its first
    // phase's sentinel, so it wakes the client, which then creates that window and swallows
    // everything typed after the first character.
    // **The phase-1 message specifically.** The client has two idle exits and they used to
    // print the same line: this one before the window phase, where nothing has been created
    // and acting immediately is safe, and one *after* the 2048×2048 window exists, where the
    // line is an announcement and the window outlives it through `exit`, the kernel's
    // teardown and the compositor noticing the peer closed. Waiting on the ambiguous string
    // meant this gate could resume in either state; the click that follows lands on the
    // terminal in one and on a screen-sized window in the other.
    session.expect("input-testclient: idle before the window phase")?;

    // Click inside `nxterm`, past the right edge of the reference windows stacked above its
    // top-left corner — they are 320 wide, and the aim is clear of them in x whatever their
    // heights, which is what keeps this point stable as the reference picture grows.
    //
    // **Through `click_at`, which this gate hand-rolled the sequence for until 2026-08-31.**
    // The pin and the walk are the same arithmetic it does — injection is relative, so the
    // pointer is over-driven into a corner first and everything after that is subtraction; the
    // first version of this gate clicked at (0, 0), inside the 64×32 scene window, and the
    // version after that sent one motion of (4000, 4000), which a 9-bit PS/2 delta turns into a
    // different, meaningless movement. What the helper adds is the retry, and the retry is what
    // was missing: a misplaced press is not a lost pixel but a permanent offset, so asserting the
    // position once turns one misplaced motion into a failed build.
    //
    // **Which is what happened, and the split assertion is why it can be said.** CI failed here
    // with the press at (495, 351) — exactly one step of (-98, -56) short of its aim. The
    // comment this replaces called the position "bit-identical over 40 loaded runs" and left the
    // gate's one prior failure unexplained. It was read on 2026-08-31 as a packet lost, and on
    // 2026-09-23, from QEMU's `ps2.c`, as a packet *held*: QEMU keeps what its sixteen-byte queue
    // cannot take and adds it to the next injected event, so the pin's over-drive rode on the
    // walk's first step and cancelled it. `move_pointer_to` drains the queue after its pin and
    // then flushes what QEMU held into the corner (`expect_after_pointer` has the source). The
    // assertion is unchanged and still exact — a press that lands anywhere else is still a
    // failure, and a systematic misplacement still fails all three attempts.
    click_at(&mut qmp, &mut session, 397, 295)?;

    // **Wait for the click to land before typing.** Click-to-focus is what gives `nxterm` the
    // keyboard — it is created first and therefore bottom-most — and the raise is not
    // instantaneous from the host's point of view. Injected back to back, the first keystroke
    // races it and lands on whatever was on top; the failure is intermittent, which is worse
    // than consistent. A focus *change* is not usable as the signal here, because the window
    // may already have had focus and no event is sent for that.
    session.expect("nxterm: clicked")?;

    // **One character at a time, each waited for.** Two reasons, and the second is the one
    // that cost an afternoon.
    //
    // Keys go to the topmost focusable window, and the click above is what raises `nxterm` —
    // created first, so bottom-most until then. A word injected back to back races that raise.
    //
    // And a keystroke here is not cheap: the terminal repaints, waits for a free buffer and
    // copies a window's worth of pixels before it looks at input again. Six keys injected as
    // fast as QMP can send them outrun that, and the tail of the word lands on a client that
    // is not looking. A human types slower than this loop; a harness does not.
    //
    // Waiting on the echo is not a workaround for the pacing — it *is* the assertion. Each
    // character has gone `nxterm` → `tty-server` → `nxsh` → back before the next is sent.
    let mut typed = String::from("/> ");
    for k in ["w", "h", "o", "a", "m", "i"] {
        qmp.send_key(k, true)?;
        qmp.send_key(k, false)?;
        typed.push_str(k);
        session.expect(&format!("nxterm: grid> {typed}"))?;
    }

    // **The echo is the loop.** Every character above travelled to the shell and came back
    // as output before it reached the grid — `nxterm` does not echo locally, and a terminal
    // that did would pass this while talking to nobody.
    qmp.send_key("ret", true)?;
    qmp.send_key("ret", false)?;
    session.expect("nxterm: grid> /> whoami")?;

    // And the shell answered. **Its text is not asserted**, because a shell in a window has
    // no session: `nxterm` inherits `init`'s root namespace, so `/session/user` — which is
    // what `whoami` reads — is not bound, and it says so. That is the same gap as
    // `TODO(gui-dev-tty)` seen from the other side, and Milestone 7 closes both when
    // `desktop-shell` constructs a namespace per application.
    session.expect("nxterm: grid> ")?;

    // **A stage's *diagnostic* reaches the grid too** (Phase 5 Part H.1). Everything above
    // this is a program's `stdout` — a typed stream the shell captures and renders as the
    // pipeline's value. Diagnostics take the other path in design §1, and until now that
    // path ended nowhere a person could see: `Streams::stderr` was `None`, so a program fell
    // back to `kprint` — `SYS_DEBUG_KPRINT`, which reaches COM1 and nothing else. Under
    // QEMU a developer read them off the serial port and the hole was invisible; on the
    // laptop, which has no serial port, `nxinstall`'s entire first run printed nothing at
    // all (first install attempt, 2026-09-17).
    //
    // **This is the gate that can see it.** `check-install` drives a release image and
    // cannot read the grid; here `nxterm` narrates it, so the whole new path is asserted at
    // once: the stage sends on the shared `stderr`, the shell drains it while waiting, and
    // it lands in the grid as text. A failing `remove` is the cheapest diagnostic in the
    // tree, and a path that does not exist is a message no successful command would print.
    type_at_terminal(&mut qmp, "remove /nothing-here")?;
    session.expect("nxterm: grid> remove: ")?;
    println!("  ok: a stage's diagnostic rendered in the grid, not on the kernel log");

    // **And `nxsh --help` reaches the terminal** (the laptop polish's Part E). A shell running a
    // script is handed a terminal and set it aside, so its usage went to `kprint`: serial showed
    // it, and a person at a window saw nothing. The grid is narrated only from what reaches the
    // terminal, and the typed line does not hold the usage.
    type_at_terminal(&mut qmp, "nxsh --help")?;
    session.expect("nxterm: grid> usage: nxsh [SCRIPT.nx | -c SOURCE]")?;
    println!("  ok: `nxsh --help` rendered in the grid, not on the kernel log");

    // **And it follows the shell.** The line above proves the announcement arrives once; this
    // proves it is the shell's *current* answer rather than a value read at startup — which is
    // the whole point of putting it in a title bar, and which a terminal that announced only
    // its first prompt would pass the first assertion for. `cd` is a builtin, so nothing but
    // the shell's own state changes here.
    //
    // That the line is a *change* rather than one per prompt is
    // `a_repeated_announcement_is_not_a_change` in `nxterm`, on the host: `expect` scans
    // forward past whatever it does not match, so a gate cannot assert the absence of a line.
    type_at_terminal(&mut qmp, "cd /home")?;
    session.expect("nxterm: shell is at /home")?;
    println!("  ok: the terminal followed the shell into another directory");

    // **The locks, the keypad and the lights** (Phase 6 Part B.5). Nothing typed so far has
    // changed a lock, so nothing typed has set the lights again.
    wait_ps2_lights(&led_log, &[0, 2], "the typing so far")?;
    // One key at a time, each waited for as the loop above does, building the line it should show.
    let mut line = String::from("/home> ");
    let key = key_echoed;
    // **Caps Lock inverts Shift for letters, and for letters alone.** The quote is typed with it
    // on: a Caps Lock that inverted Shift for every key would type an apostrophe there. And a
    // shifted letter under it is lower case.
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
    // A string at the top level is shown bare, so this line is the shell's answer and cannot be
    // the typed line, which begins with the prompt.
    session.expect("nxterm: grid> ABac")?;
    println!("  ok: Caps Lock upper-cased the letters it was on for, and only the letters");
    // **The keypad, under Num Lock**: digits, an operator, and its own Enter — which the shell
    // takes as Enter, or there would be no answer.
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
    // **And with Num Lock off, the keypad is the keys printed under its digits**: 8 is Up, which
    // the line discipline answers by recalling the line before — under Num Lock it would be an 8.
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

    // **The menu is a window now (M6 C3).** It was a `Stack` layer over the terminal, which
    // worked only because it happened to fit inside it; as a `popup` it is parented to the
    // terminal, placed by `nxterm` at the anchor its own layout gives, and clipped by the
    // screen rather than by its parent.
    //
    // **F1, not a click on the bar.** `nxterm` is created before `ui-testclient`'s windows, so
    // the bar button that normally opens the menu is underneath them and cannot be clicked. A
    // key can be injected, and doing it *here* — after everything typed at the shell has been
    // asserted — matters: an open menu is a topmost popup and takes the keyboard, so opening it
    // earlier would swallow the typing this gate exists to check.
    qmp.send_key("f1", true)?;
    qmp.send_key("f1", false)?;
    session.expect("nxterm: menu popup ")?;
    // `<id> at <x>,<y> <w>x<h>`, in screen coordinates — the compositor resolved the offset
    // against the terminal's origin, so this is where a click has to be aimed.
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

    // **Click the menu.** This is the whole of C3 in one step: the record names the popup,
    // `libsurface` routes it to the popup's own tree rather than the terminal's, and the item
    // it lands on produces a message. Before part 1 the record could not say which window it
    // was for; before part 2 the client could not hold both windows at once.
    //
    // The press position is asserted before the effect, for the reason the click above states:
    // it separates "the pointer was not there" from "the pointer was there and nothing happened".
    //
    // **Hover before the click, and it has to be in that order** (M11 Part E batch 3). Hover is
    // the first thing in this system that reacts to the pointer without a button held, and it is
    // invisible to a gate: the highlight is pixels, and this boot has no reference render of a
    // menu to compare against. So the client says which row it is over, and the click activates
    // that one. Moving and clicking as one step does not show it: choosing closes the menu, and
    // the popup is destroyed before it would have painted itself hovered.
    //
    // **The row is found rather than computed** (M14 Part A). It used to be "the upper quarter",
    // which worked while the menu had two equal rows. It has five now — two of them with an
    // accelerator column, one of them a separator, and the first two greyed until something is
    // selected — so a fraction of the height names a different row depending on the theme's text
    // size. The hover receipt already says *which* row the pointer is over, so this walks down
    // the popup and stops when the guest says it is over the one it means to click.
    //
    // **`Clear`, which is `MENU_ROW_KEY + 6`.** Copy and Paste are above it — Copy is disabled
    // with nothing selected and would produce no message when clicked, which is correct behaviour
    // and useless as a proof that a click reached a row — and so are Select All, Find and Clear
    // Scrollback since M14 Part E, plus the separator.
    //
    // **It was `+ 3` and that row is now `Find`**, so the walk still found a row to hover and the
    // click chose the wrong one; the failure appeared as "never saw `menu chose Clear`" three
    // steps from the menu it was about. `nxterm::tests::the_gate_clicks_the_row_it_means` makes
    // this number wrong in a second instead of in a three-minute boot, which is the job the same
    // test has done twice for `nxfiles`.
    const CLEAR_ROW: u64 = 106;
    //
    // **Each step records where it went**, which is a deliberate exception to the rule that only
    // a *confirmed* press updates the tracked position. `move_pointer_to` walks a delta from that
    // position and does not update it, so a loop that left it alone would re-walk the whole delta
    // every iteration and run the pointer off the popup — which is what the first version of this
    // did. A dropped packet here costs one four-pixel step of drift, and the walk simply takes
    // one more; the receipt below is what actually confirms the position, so nothing downstream
    // trusts the estimate.
    let cx = px + pw as i32 / 2;
    let mut cy = py + 3;
    let mut found = None;
    while cy < py + ph as i32 - 3 {
        move_pointer_to(&mut qmp, cx, cy)?;
        qmp.pointer = Some((cx, cy));
        // **The row the pointer settled on, not the first one it crossed.** The first step here
        // comes from wherever the last click left the pointer, in 100-pixel packets, and the
        // guest reports every row it passes: after the desktop refresh's Part B moved this popup
        // four pixels, the approach's first packet landed on `Clear` itself, the walk took that
        // for arrival, and clicked the disabled `Copy` at the top. See `Session::settled_line`.
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
        // **Every line the popup said, because "never saw 103" alone does not distinguish the
        // ways this fails**: the pointer never reached the popup, it reached it and the row
        // numbering differs, or the popup could not draw at all. The first time this fired it
        // was the third — `nxterm: the menu popup could not be drawn`, twenty times over,
        // because the tree was not diffable — and a message that said only "never saw 103"
        // pointed at the walk rather than at the widget.
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
    // **The receipt is also the position proof**, which is why the tracked position is set here
    // rather than assumed. `move_pointer_to` deliberately does not record where it went — only a
    // *confirmed* press does, because injection is relative and an unacknowledged move leaves the
    // host believing something it cannot check. Here the guest has just said it is over the row,
    // so the position is known by evidence rather than by assertion.
    //
    // Skipping this cost an afternoon: the click below then walked its delta from the position
    // the *previous* click had confirmed, doubling the movement, landing at the corner, and
    // dismissing the menu — after which the retry pressed on the terminal underneath.
    qmp.pointer = Some((cx, cy));
    click_at(&mut qmp, &mut session, cx, cy)?;
    session.expect("nxterm: menu chose Clear")?;

    // **The keyboard half of an open menu**, which had no coverage until PR #280's review found
    // two blocking bugs living in it. Everything above this point is the pointer: a click opens,
    // a click chooses. Nothing had ever arrowed.
    //
    // The popup takes the keyboard while it is up, so these keys reach *it* and never the grid.
    //
    // **And the terminal has to have the keyboard back before F1 goes**, which is why this waits
    // for `focus=1`. Choosing closes the popup, and the compositor announces the terminal's focus
    // only once the popup has gone; F1 sent on `chose Clear` alone could arrive while the
    // closing popup still held the keyboard, and open nothing. It did, twice: once in the desktop
    // refresh's Part A under host load, and again in Part B without it, both transcripts ending
    // `chose Clear`, `focus=1`, and silence. The key's destination cannot be logged — console lines
    // never carry what was typed — so the evidence is the ordering: this is the transcript's own.
    session.expect("nxterm: focus=1")?;
    qmp.send_key("f1", true)?;
    qmp.send_key("f1", false)?;
    session.expect("nxterm: menu popup ")?;
    let reopened = session.rest_of_line()?;
    let Some((_, ex, _, _, eh)) = parse_popup_line(&reopened) else {
        let _ = session.child.kill();
        return Err(format!("could not read the reopened popup's geometry from {reopened:?}").into());
    };

    // **Left moves to the other menu, and the popup has to be rebuilt for it.** The window is
    // positioned under a bar word and sized for one menu's rows, both fixed at `Child::open` —
    // so a client that reconciled on "is a menu open" rather than "which one" would leave this
    // window where it is and draw the new menu's rows into the old menu's extent. The proof is a
    // *second* popup line naming a different word and a different height.
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

    // Back to the menu with rows worth choosing, then down one and Enter.
    qmp.send_key("right", true)?;
    qmp.send_key("right", false)?;
    session.expect("nxterm: menu popup ")?;
    let _ = session.rest_of_line()?;
    // **One Down lands on Paste, not on Copy.** Copy is the first row and is greyed with nothing
    // selected, and arrowing skips what Enter could not act on — so this asserts the skip as well
    // as the movement. A gate that expected `Copy` here would be asserting the bug.
    qmp.send_key("down", true)?;
    qmp.send_key("down", false)?;
    qmp.send_key("ret", true)?;
    qmp.send_key("ret", false)?;
    session.expect("nxterm: menu chose Paste")?;
    println!("  ok: Down skipped the greyed row and Enter chose the next one");

    // **A second window of the terminal, with a shell of its own** (M14 Part B). The window is
    // the shell's proof as much as the tab's was: a second top-level that opened with nothing to
    // talk to would look identical from the outside, and this is the only place that difference
    // is visible.
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(&mut qmp, "n")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    // **The shell's line comes first, and structurally so**: `open_window` gives the new
    // window's first tab a tty and a shell before it returns, so the terminal cannot report a
    // window it has not finished building.
    // **The shell's line comes first, and structurally so**: `open_window` gives the new window's
    // first tab a tty and a shell before it returns, so the terminal cannot report a window it has
    // not finished building. **Nothing between the last two**, because the third comes from a
    // freshly spawned `nxsh` and its order against this process's own `kprint` is a race — the
    // rule this gate's own closing comment states (PR #283 review, optional 9).
    session.expect("nxterm: hosting a shell")?;
    session.expect("nxterm: opened another window")?;
    session.expect("nxsh: interactive shell")?;
    println!("  ok: Ctrl+Shift+N opened a second window with a shell of its own");

    // **A second tab is a second terminal**, which is the only part of M14 Part B a host test
    // cannot reach: the split of grids and outboxes is pinned in `nxterm`'s own tests, but that
    // a new tab is given its *own* tty and its *own* shell happens in the binary and needs a
    // boot. `Ctrl+Shift+T` — Shift because `Ctrl+T` belongs to whatever is running in the
    // terminal.
    //
    // The receipt is the shell's, not the tab's: a strip with two words in it would be drawn by
    // a terminal that opened a tab and gave it nothing to talk to, which is exactly the failure
    // worth catching.
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

    // Everything above is ordered, so it is an `expect` chain. This last check is not:
    // `boot-probe` exits somewhere between `ui-testclient: PASSED` and
    // `input-testclient: idle`, and which side it lands on is timing. An `expect` placed
    // between two lines whose order is not guaranteed is a flake, so this reads the
    // finished transcript instead.
    check_service_attribution(&session.finish().into_bytes())?;

    println!(
        "\nxtask: terminal gate PASSED — a keystroke reached the shell and its answer \
         reached the grid ✓"
    );
    Ok(())
}

/// Assert that `service-mgr` told its two children's exits apart.
///
/// `KIND_CHILD_EXITED` names a child by pid and nothing maps a process handle to a pid, so
/// the discriminator is which control endpoint closed (`TODO(child-exit-attribution)`).
///
/// **This lives on `check-terminal` rather than `test-qemu`, and the reason is the verdict.**
/// `test-qemu` asserted it until retrofit Part B moved `SYS_TEST_EXIT` into `boot-probe`:
/// there, the probe's last act terminates the machine, so `service-mgr` never sees it exit
/// and there is nothing to attribute. `check-terminal` boots the *same image* without the
/// `isa-debug-exit` device, so the verdict write is ignored, the probe carries on to
/// `exit(0)`, and its exit is attributed like any other service's.
///
/// `code=0` and not a bare "exited": a supervisor that reads a *closed control channel* as
/// an exit reports the death before the child has run and prints `code=unknown`
/// (PR #226 review, findings 1 and 3).
fn check_service_attribution(transcript: &[u8]) -> R<()> {
    let text = String::from_utf8_lossy(transcript);
    if !text.contains("service-mgr: 'boot-probe' exited code=0") {
        // Three different faults reach this line, so the message names all three rather
        // than the one that prompted the assertion.
        return Err("service-mgr did not attribute boot-probe's exit with its status \
             (expected \"service-mgr: 'boot-probe' exited code=0\"). Check the transcript for \
             which it is: a \"boot-probe: … FAIL\" line means a gate failed and the probe \
             exited 1 — that is the real fault and this assertion is only how it surfaced; \
             \"code=unknown\" means the death was detected without a matching notification, \
             which is what an early control-handle close looks like; and no \"boot-probe\" \
             line at all means it never ran."
            .into());
    }
    // And nothing else was blamed for it. `heartbeat` is `policy = always`, so a
    // misattributed exit shows up as a restart of a service that never stopped. Nothing asks
    // it to stop any more: the 1.1 s demo stop sent `CTRL_OP_SHUTDOWN` to the *first*
    // declared service, which administration Part E.1a made `auth-service`, and it went.
    if text.contains("service-mgr: restarting 'heartbeat'") {
        return Err("service-mgr restarted 'heartbeat', which never exited — \
             boot-probe's exit was misattributed to it"
            .into());
    }
    println!("xtask: service-mgr attributed boot-probe's exit to boot-probe ✓");
    Ok(())
}
