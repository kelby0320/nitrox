use crate::*;

/// `cargo xtask shot [all|greeter|desktop|apps|windows|terminal|hover|files|editor|overview]` —
/// photograph the running desktop.
///
/// **The other half of `preview`, and the half it said it could not be.** Part A's command
/// renders the toolkit's own surfaces on the host in about a second, and its doc names what that
/// structurally cannot show: anything the *compositor* draws — the cursor, the drag outline, the
/// ground between windows — and the arrangement of real windows on a real screen. Those are
/// composed in the guest by clients that have to run, so the only honest way to look at them is
/// to boot and take the picture.
///
/// **A photograph, not a render**, which is what keeps this out of the "two sources for one
/// answer" trap that `preview_frames` exists to avoid: nothing here draws anything. It boots the
/// **release** image — the one a person would use, with no `--selftest` clients on the screen —
/// drives it to each moment worth looking at, and writes what QEMU says is on the display.
///
/// **Several moments per boot**, because the boot is the cost. One run gives nine: the greeter,
/// the bare desktop, the Applications menu, a screen with real windows on it, the terminal, the
/// pointer resting on the terminal's close button, the file browser, the editor, and the overview.
/// The overview is there because it is the one surface with no other way to be looked at: it
/// covers the screen and closes when anything else is clicked. **The hover is there for the same
/// kind of reason** (added after the desktop refresh's Part K): a host render can paint a lit
/// button and a routed test can prove a pointer names its key, but only a live window shows that
/// the application repaints when the pointer crosses onto it.
///
/// It is a tool and not a gate: it asserts only enough to know the picture is of a working
/// desktop rather than of a blank screen, which is the one failure that would otherwise be
/// mistaken for a design opinion.
pub(crate) fn cmd_shot(what: &str, accel: Accel, size: DisplaySize) -> R<()> {
    const MOMENTS: [&str; 9] =
        ["greeter", "desktop", "apps", "windows", "terminal", "hover", "files", "editor", "overview"];
    if what != "all" && !MOMENTS.contains(&what) {
        return Err(format!("no shot called {what:?} — try `all` or one of: {}", MOMENTS.join(", ")).into());
    }
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;

    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let dump = work.join("shot.ppm");
    let qmp_sock = work.join("qmp-shot.sock");
    let (mut session, mut qmp) = spawn_release_guest(accel, "shot", &qmp_sock, size, false, false)?;

    // A closure would borrow both halves for the rest of the function, so the capture is a
    // statement each time — four lines, and no plumbing to read past.
    macro_rules! capture {
        ($name:expr) => {
            if what == "all" || what == $name {
                let ppm = settle_and_capture(&mut qmp, &dump)?;
                let (w, h, rgb) = parse_ppm(&ppm)?;
                let path = work.join(concat!("shot-", $name, ".png"));
                write_png(&path, w, h, &rgb)?;
                println!("  shot: {} ({w}x{h})", path.display());
            }
        };
    }

    println!("xtask: booting the release image to photograph it…\n");

    // 1. **The greeter**, which in a release image is the only window there is.
    session.expect("desktop-session-mgr: greeter presented")?;
    // The pointer somewhere a person would leave it, so the cursor is in the picture. Without
    // this it sits whereever QEMU starts it, which is the top-left corner and under the window.
    move_pointer_to(&mut qmp, size.centre().0, size.centre().1)?;
    capture!("greeter");

    // 2. **A session.** No wrong password here — that is `check-login`'s claim to make, and a
    //    tool that took twice as long to produce the same pictures would be a tool used less.
    type_at_greeter(&mut qmp, &mut session, DEMO_USER)?;
    press(&mut qmp, "tab")?;
    type_at_greeter(&mut qmp, &mut session, DEMO_PASSWORD)?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: up (graphical session leader)")?;
    // Both bars, because a desktop missing one is exactly the picture that would be mistaken
    // for a design decision rather than a broken boot. In the order the shell prints them —
    // `expect` scans forward, so a pair asserted by topic rather than by position in the stream
    // times out on output that was there.
    // **The clock read the wall clock**, which is the one thing about it a gate can see: its
    // value changes every minute and the bar is pixels this boot has no reference render of. The
    // line distinguishes a clock that is absent because the RTC was unreadable from a bar that
    // failed to draw one — and the *formatting* is a host test in `libtime`, where it belongs.
    session.expect("desktop-shell: clock ")?;
    session.expect("desktop-shell: top bar presented, window ")?;
    session.expect(&format!("desktop-shell: bottom bar placed at 0,{}", size.bottom_bar_y()))?;
    capture!("desktop");

    // 3. **The Applications menu**, the one piece of chrome with no window of its own: a popup
    //    over the desktop, which is where the toolkit's list rows are seen at their real size.
    click_at(&mut qmp, &mut session, 60, 12)?;
    session.expect("desktop-shell: applications menu open")?;
    capture!("apps");

    // 4. **Real windows.** Two applications rather than one, because half of what a desktop
    //    looks like is how two windows sit next to each other — and one of each kind: a
    //    proportional-font application and the terminal, which is the only grid on the screen.
    let files = launch_from_menu(&mut qmp, &mut session, "nxfiles")?;
    // **Escape before aiming at the button again**, the precondition `check-login` states for
    // the same click: `click_at` retries a press that did not land, and an abandoned attempt
    // still pressed *somewhere* — if that somewhere was the Applications word the menu is
    // already open, the aimed click opens no second one, and the wait below never ends.
    press(&mut qmp, "esc")?;
    session.skip_to_end()?;
    click_at(&mut qmp, &mut session, 60, 12)?;
    session.expect("desktop-shell: applications menu open")?;
    // And drawn, before a keystroke is aimed at it.
    let _ = settle_and_capture(&mut qmp, &dump)?;
    let term = launch_from_menu(&mut qmp, &mut session, "nxterm")?;
    // The shell cascades what it places, so the two land offset rather than stacked.
    //
    // **No wait here**: `launch_from_menu` already waited for the shell to place the window,
    // which is the stronger receipt — and the terminal's own startup lines come *before* that, so
    // an `expect` for one of them scans past output that was already there.
    move_pointer_to(&mut qmp, 900, 500)?;
    capture!("windows");

    // 5. **Each application on its own, in the state the design shows it** (desktop refresh,
    //    Part C's comparison pass): the terminal after a `list`, the browser at `/home`, and the
    //    editor on `theme.toml`. These are what `docs/design/nitrox-shell/nitrox-shell.html` draws
    //    its three windows doing, so a picture of each can be set beside the design's.
    //
    //    The terminal holds the keyboard — it was launched last.
    type_at_terminal(&mut qmp, "list")?;
    capture!("terminal");

    //    **The pointer resting on the terminal's close button** (desktop refresh, after Part K):
    //    the one state a host render cannot vouch for. `libui`'s tests paint a hovered button and
    //    route a pointer to its key, but only a live window shows that the application *repaints*
    //    when the pointer crosses onto it — and a hover nobody can see in the product is the
    //    defect Part H's close box was. Aimed with the gates' own copy of where close sits.
    move_pointer_to(&mut qmp, term.0 + term.2 as i32 - chrome::CLOSE_X, term.1 + chrome::TITLE_Y)?;
    capture!("hover");

    //    The browser, raised by its title bar — which the terminal, placed a cascade step lower,
    //    leaves showing.
    click_at(&mut qmp, &mut session, files.0 + 160, files.1 + 16)?;
    capture!("files");

    //    **The editor, opened the way a person opens a file**: the browser's selection moved down
    //    its listing to `theme.toml`, and Enter. A fresh listing selects its first row, and the
    //    browser reports each change of selection, so each press is acknowledged before the next.
    let mut on_theme = false;
    for _ in 0..12 {
        press(&mut qmp, "down")?;
        if session.expect_within("nxfiles: selected theme.toml", std::time::Duration::from_secs(2))? {
            on_theme = true;
            break;
        }
    }
    if !on_theme {
        return Err("the browser's selection never reached theme.toml in /home".into());
    }
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: launched nxedit into its own namespace")?;
    placed_window(&mut session)?;
    capture!("editor");

    // 6. **The overview**, which is the one surface with no other way to be looked at: it is
    //    opened from the desktop's name, it covers the screen, and it is where the sidebar's
    //    desktop miniatures live (M11 Part E batch 10).
    //    **Pressed by hand rather than through `click_at`**, because the shell logs the open
    //    while it routes the press and the compositor logs the press when it *delivers* the
    //    routed record — so the open comes first, and `click_at`'s own position assertion scans
    //    past it. Nothing is lost: a press that misses simply does not open the overview, and the
    //    wait below fails.
    let overview_at = size.desktop_name_click();
    move_pointer_to(&mut qmp, overview_at.0, overview_at.1)?;
    qmp.send_button("left", true)?;
    qmp.send_button("left", false)?;
    expect_after_pointer(&mut qmp, &mut session, "desktop-shell: overview open, window ")?;
    qmp.pointer = Some(overview_at);
    capture!("overview");

    let _ = fs::remove_file(&qmp_sock);
    println!("\nxtask: shots written to {}", work.display());
    Ok(())
}

/// Type a program's name into the open Applications menu and launch it.
///
/// The menu is a `popup`, so it holds the keyboard — the same property `check-login` relies on.
///
/// **Enter launches the lit row**, and typing lights the top match: the menu points at what the
/// filter narrowed to, so the name typed here is the row Enter chooses.
fn launch_from_menu(qmp: &mut Qmp, session: &mut Session, program: &str) -> R<(i32, i32, u32, u32)> {
    for c in program.chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
    }
    press(qmp, "ret")?;
    session.expect(&format!("desktop-shell: launched {program} into its own namespace"))?;
    session.expect("desktop-shell: applications menu closed")?;
    // **And wait until its window is on screen**, which is not the same claim and is the one the
    // next step needs. A launch returns when the *shell* has spawned the program; the program
    // then starts, creates a window, and the compositor focuses it — after the menu for the
    // *next* launch has already opened. Typing into that menu put the second program's name
    // into the first program, and in a file browser Enter means "open the selected row", so the
    // shot ended up with an editor on `theme.toml` instead of a terminal.
    placed_window(session)
}

/// Wait for the shell to place the next window, and read where it went and how big it is.
///
/// **Its own geometry line, by id** — not the next `desktop-shell: window …` line, which can be the
/// window list. Returns `(x, y, w, h)`.
fn placed_window(session: &mut Session) -> R<(i32, i32, u32, u32)> {
    session.expect("desktop-shell: placed window ")?;
    let placed = session.rest_of_line()?;
    let (id, _, _) = parse_placement(&placed).ok_or_else(|| format!("could not read a placement from {placed:?}"))?;
    session.expect(&format!("desktop-shell: window {id} geometry "))?;
    let rest = session.rest_of_line()?;
    let line = format!("{id} geometry {rest}");
    let (_, x, y, w, h) =
        parse_geometry_line(&line).ok_or_else(|| format!("could not read window {id}'s geometry from {rest:?}"))?;
    Ok((x, y, w, h))
}

/// Boot the **release** image headless with a QMP socket and the serial on stdio.
///
/// Shared by `check-login`, `check-logout` and `shot`, the things that boot the image a person
/// would actually use — every other gate boots `--selftest`. The three that do build this
/// command themselves and differ from each other in the image mode and the flags; these are
/// identical, and identical copies of a boot are how the second one quietly stops matching.
///
/// **`reboot` lets a reset reboot the guest** rather than end QEMU: `check-logout` restarts the
/// machine twice and goes on in the boots that follow (administration Part F.3). Everything else
/// keeps `-no-reboot`, so a guest that resets by accident ends the run rather than booting again.
///
/// **`usb` boots a machine whose only keyboard and mouse are USB** ([`usb_input_args`]):
/// `check-login --usb` (Phase 6 Part B).
pub(crate) fn spawn_release_guest(
    accel: Accel,
    gate: &'static str,
    qmp_sock: &Path,
    size: DisplaySize,
    reboot: bool,
    usb: bool,
) -> R<(Session, Qmp)> {
    let ovmf = locate_ovmf()?;
    // Removed rather than reused: a socket left by a killed run is a file `Qmp::connect` will
    // open and never get an answer from. The *caller* owns the path, because it is also what
    // gets cleaned up at the end of a gate.
    let _ = fs::remove_file(qmp_sock);

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
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if !reboot {
        cmd.arg("-no-reboot");
    }
    if usb {
        usb_input_args(&mut cmd, true);
    }

    let session = Session::spawn(cmd, gate)?;
    let mut qmp = Qmp::connect(qmp_sock)?;
    qmp.screen = Some(size);
    Ok((session, qmp))
}
