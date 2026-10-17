//! `check-logout`: the power menu's log out past an editor's question, restart and shut down.

use crate::*;

/// How many desktop entries the image stages, which `check-login` asserts the shell read: a
/// literal, as deriving it from `GRAPHICAL_APPLICATIONS` would agree with the build by
/// construction.
pub(crate) const STAGED_APPLICATIONS: usize = 3;

/// `cargo xtask check-logout` — ending a session from the desktop, in CI's QEMU job.
///
/// On the release image, as `check-login` is (only there does the greeter hold the keyboard). One
/// run, three boots with the halt last: Log out past an editor's question (the waiting dialog names
/// it without taking the keyboard from the question), Cancel, discard; a terminal with nothing to
/// ask; `with power shutdown --reboot` typed at a terminal, which asks the windows too and ends the
/// shell after its own 3 s, inside its supervisor's 5 s (no other gate reaches this:
/// `check-shutdown` has no graphical session); Restart with End anyway; Shut down, read off the
/// screen with `check-fbcon`'s decoder. Aimed, not measured: the power button at
/// `chrome::POWER_FROM_RIGHT`, the menu's rows by keyboard, the dialogs' buttons at `chrome`'s
/// published centres from the origins the shell logs.
pub(crate) fn cmd_check_logout(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;
    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-logout.sock");
    println!("xtask: logout gate — booting the release image…\n");
    let (mut session, mut qmp) = spawn_release_guest(accel, "check-logout", &qmp_sock, size, true, false)?;
    const APPS_CLICK: (i32, i32) = (60, 12);
    let power_click = (size.w as i32 - chrome::POWER_FROM_RIGHT, 12);

    session.expect("desktop-session-mgr: greeter presented")?;
    logout_gate_login(&mut qmp, &mut session, size)?;
    println!("  ok: logged in at the greeter");

    let editor = logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxedit")?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;
    println!("  ok: the editor is open (window {editor}) with something to lose");

    logout_gate_choose(&mut qmp, &mut session, power_click, LOG_OUT)?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let placed = session.rest_of_line()?;
    let (placed_id, parent, _, _, _, _) = parse_dialog_placement(&placed)
        .ok_or_else(|| format!("could not read the editor's question's placement from {placed:?}"))?;
    if parent != editor {
        return Err(format!("the question placed is on window {parent}, not the editor's {editor}").into());
    }
    let (wx, wy) = logout_gate_waiting(&mut session, "Waiting for 1 window to close:")?;
    println!("  ok: Log out asked the editor, which asked about its buffer, and the shell named it");
    // The keyboard is still the question's: the dialog gave it back, so Escape reaches the editor
    // (keep editing), not the dialog, where it would be Cancel. No receipt to wait for; the shell
    // logs after the compositor answered the raise.
    session.expect(&format!("desktop-shell: the keyboard back to window {placed_id}"))?;
    let from = session.transcript().len();
    press(&mut qmp, "esc")?;
    session.expect("nxedit: close cancelled, still editing")?;
    if session.transcript()[from..].contains("desktop-shell: ending the session cancelled") {
        return Err("Escape, meant for the editor's question, cancelled the logout".into());
    }
    println!("  ok: the waiting dialog left the keyboard with the editor's question");

    click_at(&mut qmp, &mut session, wx + chrome::DIALOG_RIGHT_CX, wy + chrome::DIALOG_BUTTON_CY)?;
    session.expect("desktop-shell: ending the session cancelled; it goes on")?;
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    press(&mut qmp, "esc")?;
    session.expect("desktop-shell: applications menu closed")?;
    println!("  ok: Cancel kept the session");

    logout_gate_choose(&mut qmp, &mut session, power_click, LOG_OUT)?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let placed = session.rest_of_line()?;
    let (_, _, qx, qy, _, _) = parse_dialog_placement(&placed)
        .ok_or_else(|| format!("could not read the editor's second question's placement from {placed:?}"))?;
    logout_gate_waiting(&mut session, "Waiting for 1 window to close:")?;
    click_at(&mut qmp, &mut session, qx + chrome::DIALOG_LEFT_CX, qy + chrome::DIALOG_BUTTON_CY)?;
    session.expect("nxedit: discarding the unsaved buffer")?;
    session.expect("desktop-shell: logging out, exiting")?;
    session.expect("desktop-session-mgr: session ended (leader exit 0)")?;
    session.expect("desktop-session-mgr: greeter presented again")?;
    println!("  ok: once the editor closed, the session ended and the greeter came back");

    logout_gate_login(&mut qmp, &mut session, size)?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxterm")?;
    logout_gate_choose(&mut qmp, &mut session, power_click, LOG_OUT)?;
    let from = session.transcript().len();
    session.expect("desktop-shell: logging out, exiting")?;
    if session.transcript()[from..].contains("desktop-shell: waiting dialog open") {
        return Err("a terminal, which closes when asked, still brought up the waiting dialog".into());
    }
    session.expect("desktop-session-mgr: greeter presented again")?;
    println!("  ok: with nothing to ask, the session ended at once and the greeter came back");

    logout_gate_login(&mut qmp, &mut session, size)?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxedit")?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxterm")?;
    type_at_terminal(&mut qmp, "with power shutdown --reboot")?;
    session.expect("desktop-shell: asked to stop; asking every window to close first")?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    // To the reset, then read back: a shell that outlived its supervisor's bound never says it is
    // exiting before the machine goes, so waiting for that line would time out without saying why.
    let from = session.transcript().len();
    session.expect(RESET)?;
    let after = session.transcript()[from..].to_string();
    if after.contains("desktop-shell: waiting dialog open") {
        return Err("a shutdown started at a terminal brought up the waiting dialog".into());
    }
    logout_gate_stopped_in_time(&after)?;
    session.expect(BOOT_GREETER)?;
    println!("  ok: a restart from a terminal asked the windows, waited without a dialog, and a second boot came up");

    logout_gate_login(&mut qmp, &mut session, size)?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxedit")?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;
    logout_gate_choose(&mut qmp, &mut session, power_click, RESTART)?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    let (wx, wy) = logout_gate_waiting(&mut session, "Waiting for 1 window to close:")?;
    // Read back from the click, not the shell's line: `shutdown` runs once the broker has spawned
    // it and can say it is restarting before the shell has heard it was started.
    let from = session.transcript().len();
    click_at(&mut qmp, &mut session, wx + chrome::DIALOG_LEFT_CX, wy + chrome::DIALOG_BUTTON_CY)?;
    session.expect("desktop-shell: ending anyway, closing 1 window(s)")?;
    session.expect("desktop-shell: Restart: the broker started shutdown in the power view")?;
    session.expect(RESET)?;
    let after = session.transcript()[from..].to_string();
    for line in ["shutdown: restarting", "service-mgr: shutting down, to reboot, as asked"] {
        if !after.contains(line) {
            return Err(format!("the desktop's Restart reset the machine without `{line}`").into());
        }
    }
    logout_gate_stopped_in_time(&after)?;
    session.expect(BOOT_GREETER)?;
    println!("  ok: Restart waited for the editor, ended it anyway, asked the broker, and a third boot came up");

    logout_gate_login(&mut qmp, &mut session, size)?;
    let from = session.transcript().len();
    logout_gate_choose(&mut qmp, &mut session, power_click, SHUT_DOWN)?;
    session.expect("desktop-shell: Shut down: the broker started shutdown in the power view")?;
    session.expect("It is now safe to turn off your computer.")?;
    let after = session.transcript()[from..].to_string();
    for line in ["shutdown: shutting down", "service-mgr: shutting down, as asked"] {
        if !after.contains(line) {
            return Err(format!("the desktop's Shut down halted the machine without `{line}`").into());
        }
    }
    logout_gate_stopped_in_time(&after)?;
    expect_safe_on_screen(&mut qmp, &build_cache().join("check-logout-halt.ppm"))?;
    println!("  ok: Shut down asked the broker, and the screen, read back as text, says it is safe to turn off");
    println!(
        "\nxtask: logout gate PASSED — a logout waited for the editor's question, was cancelled, then \
         ended; one with nothing to ask ended at once; a restart from a terminal asked the windows; \
         and the desktop restarted the machine and shut it down ✓"
    );
    Ok(())
}

/// The power menu's rows, each chosen by pressing Down that many times, in the order
/// `desktop-shell`'s `panel` test pins.
const LOG_OUT: (&str, usize) = ("Log out", 1);

const RESTART: (&str, usize) = ("Restart", 2);

const SHUT_DOWN: (&str, usize) = ("Shut down", 3);

const RESET: &str = "power: resetting through the FADT's reset register";

/// The greeter of a boot, not the one presented again after a logout, whose line begins the same
/// way.
const BOOT_GREETER: &str = "desktop-session-mgr: greeter presented,";

fn logout_gate_stopped_in_time(transcript: &str) -> R<()> {
    if transcript.contains("desktop-shell is still running after it was asked to stop")
        || !transcript.contains("desktop-shell: asked to stop, exiting")
    {
        return Err("the shell outlived its supervisor's bound: its own wait for the windows must end first".into());
    }
    Ok(())
}

pub(crate) fn logout_gate_login(qmp: &mut Qmp, session: &mut Session, size: DisplaySize) -> R<()> {
    type_at_greeter(qmp, session, DEMO_USER)?;
    press(qmp, "tab")?;
    type_at_greeter(qmp, session, DEMO_PASSWORD)?;
    press(qmp, "ret")?;
    session.expect("desktop-session-mgr: login ok -> home=/home/alice")?;
    session.expect("desktop-shell: up (graphical session leader)")?;
    session.expect(&format!("desktop-shell: bottom bar placed at 0,{}", size.bottom_bar_y()))?;
    Ok(())
}

fn logout_gate_launch(qmp: &mut Qmp, session: &mut Session, apps: (i32, i32), program: &str) -> R<u32> {
    click_at(qmp, session, apps.0, apps.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(qmp, session, program)?;
    press(qmp, "ret")?;
    session.expect(&format!("desktop-shell: launched {program} into its own namespace"))?;
    session.expect("desktop-shell: placed window ")?;
    let placed = session.rest_of_line()?;
    let (id, _, _) =
        parse_placement(&placed).ok_or_else(|| format!("could not read {program}'s placement from {placed:?}"))?;
    let _ = settle_and_capture(qmp, &build_cache().join("check-logout.ppm"))?;
    Ok(id)
}

fn logout_gate_choose(qmp: &mut Qmp, session: &mut Session, power: (i32, i32), row: (&str, usize)) -> R<()> {
    click_at(qmp, session, power.0, power.1)?;
    session.expect("desktop-shell: power menu open")?;
    for _ in 0..row.1 {
        press(qmp, "down")?;
    }
    press(qmp, "ret")?;
    session.expect(&format!("desktop-shell: ending the session: {}", row.0))?;
    Ok(())
}

fn logout_gate_waiting(session: &mut Session, says: &str) -> R<(i32, i32)> {
    session.expect("desktop-shell: waiting dialog open, window ")?;
    let line = session.rest_of_line()?;
    // `<id> at <x>,<y>: <what it says>`
    let (head, said) = line.split_once(": ").ok_or_else(|| format!("no text in {line:?}"))?;
    if said.trim() != says {
        return Err(format!("the waiting dialog says {:?}, not {says:?}", said.trim()).into());
    }
    let at = head.split_once(" at ").map(|(_, a)| a).ok_or_else(|| format!("no origin in {line:?}"))?;
    let (x, y) = at
        .split_once(',')
        .and_then(|(x, y)| Some((x.trim().parse::<i32>().ok()?, y.trim().parse::<i32>().ok()?)))
        .ok_or_else(|| format!("could not read the dialog's origin from {line:?}"))?;
    Ok((x, y))
}
