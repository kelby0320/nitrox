//! `shot`: the release image booted and its desktop photographed.

use crate::*;

/// `cargo xtask shot [all|greeter|desktop|apps|windows|terminal|hover|files|editor|overview]`:
/// photograph the running desktop. **A photograph, not a render**: it boots the **release** image
/// and writes what QEMU says is on the display, which shows what `preview` cannot (anything the
/// compositor draws, real windows). Several moments per boot. A tool, not a gate: it asserts only
/// that the desktop is working.
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

    session.expect("desktop-session-mgr: greeter presented")?;
    // The pointer somewhere a person would leave it, so the cursor is in the picture.
    move_pointer_to(&mut qmp, size.centre().0, size.centre().1)?;
    capture!("greeter");

    type_at_greeter(&mut qmp, &mut session, DEMO_USER)?;
    press(&mut qmp, "tab")?;
    type_at_greeter(&mut qmp, &mut session, DEMO_PASSWORD)?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: up (graphical session leader)")?;
    // Both bars, in the order the shell prints them (`expect` scans forward); the clock line tells
    // a clock absent because the RTC was unreadable from a bar that failed to draw one.
    session.expect("desktop-shell: clock ")?;
    session.expect("desktop-shell: top bar presented, window ")?;
    session.expect(&format!("desktop-shell: bottom bar placed at 0,{}", size.bottom_bar_y()))?;
    capture!("desktop");

    click_at(&mut qmp, &mut session, 60, 12)?;
    session.expect("desktop-shell: applications menu open")?;
    capture!("apps");

    let files = launch_from_menu(&mut qmp, &mut session, "nxfiles")?;
    // **Escape before aiming at the button again**: an abandoned `click_at` attempt may have opened
    // the menu.
    press(&mut qmp, "esc")?;
    session.skip_to_end()?;
    click_at(&mut qmp, &mut session, 60, 12)?;
    session.expect("desktop-shell: applications menu open")?;
    let _ = settle_and_capture(&mut qmp, &dump)?;
    let term = launch_from_menu(&mut qmp, &mut session, "nxterm")?;
    // No wait here: `launch_from_menu` already waited for the placement, and an `expect` for the
    // terminal's earlier startup lines would scan past it.
    move_pointer_to(&mut qmp, 900, 500)?;
    capture!("windows");

    type_at_terminal(&mut qmp, "list")?;
    capture!("terminal");

    // **The pointer resting on the terminal's close button**: only a live window shows the
    // application *repaints* on hover.
    move_pointer_to(&mut qmp, term.0 + term.2 as i32 - chrome::CLOSE_X, term.1 + chrome::TITLE_Y)?;
    capture!("hover");

    click_at(&mut qmp, &mut session, files.0 + 160, files.1 + 16)?;
    capture!("files");

    // **The editor, opened as a person opens a file**: the selection moved down to `theme.toml`,
    // then Enter; each press is acknowledged by a selection line.
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

    // **The overview**, pressed by hand rather than through `click_at`: the shell logs the open
    // before the compositor logs the press, so `click_at`'s position assertion would scan past it.
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

/// Type a program's name into the open Applications menu and launch it (**Enter launches the lit
/// row**).
fn launch_from_menu(qmp: &mut Qmp, session: &mut Session, program: &str) -> R<(i32, i32, u32, u32)> {
    for c in program.chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
    }
    press(qmp, "ret")?;
    session.expect(&format!("desktop-shell: launched {program} into its own namespace"))?;
    session.expect("desktop-shell: applications menu closed")?;
    // **And wait until its window is on screen**, or the next menu's typing reaches the first
    // program.
    placed_window(session)
}

/// Wait for the shell to place the next window and read where it went. **Its own geometry line, by
/// id**: the next `desktop-shell: window …` line can be the window list.
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

/// Boot the **release** image headless with a QMP socket and the serial on stdio, for
/// `check-login`, `check-logout` and `shot`. **`reboot`** lets a reset reboot the guest
/// (`check-logout` restarts it twice); otherwise `-no-reboot` ends the run on an accidental reset.
/// **`usb`** boots with only USB input ([`usb_input_args`]).
pub(crate) fn spawn_release_guest(
    accel: Accel,
    gate: &'static str,
    qmp_sock: &Path,
    size: DisplaySize,
    reboot: bool,
    usb: bool,
) -> R<(Session, Qmp)> {
    let ovmf = locate_ovmf()?;
    // Removed rather than reused: a socket left by a killed run never answers. The caller owns the
    // path.
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
