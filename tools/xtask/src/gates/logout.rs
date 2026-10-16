//! `check-logout`: the power menu's log out past an editor's question, restart and shut down.

use crate::*;

/// `cargo xtask check-login` — the **graphical login gate**: a wrong password, then a right
/// one, then a session.
///
/// The sequence `test-interactive` runs on serial, driven the way `check-input` and
/// `check-terminal` drive input, and adjudicated on the host. Built in Part D rather than at
/// the end of the milestone so Parts E and F land against a gate that already exists.
///
/// **The release image, not the test one, and that is not a preference.** In the test image
/// the greeter is bottom-most — `service-mgr` brings the login chain up before declared
/// services, which is what keeps `check-display`'s reference windows undisturbed — so it does
/// not hold the keyboard and nothing typed would reach it. In a release image it is the only
/// window. That also makes this the gate that proves the graphical arm exists for a person:
/// every other display gate boots `--selftest`.
///
/// How many desktop entries the image stages — the number `check-login` asserts the shell read.
///
/// **Duplicated from `GRAPHICAL_APPLICATIONS`'s length on purpose.** Deriving it would make the
/// gate agree with the build by construction and assert nothing; a literal is a second statement
/// of the same fact, which is what an assertion is for.
pub(crate) const STAGED_APPLICATIONS: usize = 3;

/// `cargo xtask check-logout` — **ending a session from the desktop** (administration Part F.2),
/// in CI's QEMU job.
///
/// On the **release image**, as `check-login` is: only there does the greeter hold the keyboard,
/// and only after a real login does `desktop-shell` build a session a person can end. In one QEMU
/// run:
/// 1. log in at the greeter;
/// 2. open the editor from the Applications menu, and type a key, so it has something to lose;
/// 3. the power menu's **Log out**: the shell asks the editor's window to close, the editor asks
///    whether to discard, and half a second later **the shell's waiting dialog names it** —
///    **without taking the keyboard from the editor's question** (PR #346 review, finding 2):
///    Escape answers the question, *keep editing*, and the logout goes on waiting;
/// 4. **Cancel**, and the session goes on — the Applications menu still opens;
/// 5. Log out again, and **discard** in the editor's question, asked again: the editor
///    closes, the shell exits, and **the greeter comes back**;
/// 6. log in again, open a terminal, and Log out: **nothing to ask**, so no waiting dialog, and the
///    greeter comes back;
/// 7. log in again, open the editor with a key typed into it and a terminal, and type
///    `with power shutdown --reboot` there: **a shutdown started elsewhere asks the windows too** —
///    the editor asks, no dialog comes, and the shell goes after its own 3 s, **inside its
///    supervisor's 5 s**, so no leader is left "still running after it was asked to stop"; then the
///    machine resets, and **a second boot's greeter** comes up. No other gate reaches this path:
///    `check-shutdown` has no graphical session;
/// 8. log in, open the editor with a key typed into it, and choose **Restart** (administration Part
///    F.3): the editor asks, the waiting dialog names it, and **End anyway** destroys its window;
///    then the shell asks the view broker for `shutdown --reboot` in the `power` view, `service-mgr`'s
///    sequence asks the shell to stop, it goes at once with nothing left open, and **a third boot's
///    greeter** comes up;
/// 9. log in, and choose **Shut down**: nothing to ask, the broker starts `shutdown`, and the
///    machine halts — "It is now safe to turn off your computer." **read off the screen** with
///    `check-fbcon`'s decoder, as on the laptop, which has no serial port.
///
/// **Two resets and a halt in one QEMU run**, so it boots with a reset allowed to reboot — three
/// boots, and the order is forced: the halt must be last.
///
/// **Aimed, not measured**: the power button at `chrome::POWER_FROM_RIGHT` in from the right edge,
/// the menu's rows by the keyboard, and the dialogs' buttons at `chrome`'s published centres from
/// the origins the shell logs — the rule every screen gate here keeps (M11 decision 2).
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

    // 1. Log in.
    session.expect("desktop-session-mgr: greeter presented")?;
    logout_gate_login(&mut qmp, &mut session, size)?;
    println!("  ok: logged in at the greeter");

    // 2. The editor, with a key typed into it.
    let editor = logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxedit")?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;
    println!("  ok: the editor is open (window {editor}) with something to lose");

    // 3. Log out: the editor asks, and the shell names it.
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
    // **The keyboard is still the question's**: the dialog gave it back, so Escape reaches the
    // editor — *keep editing* — and not the dialog, where it would be Cancel. No receipt from the
    // editor to wait for: it announces only its first gain. None is needed: the shell logs after
    // the compositor has answered the raise, and a key is routed to whatever the stack says then.
    session.expect(&format!("desktop-shell: the keyboard back to window {placed_id}"))?;
    let from = session.transcript().len();
    press(&mut qmp, "esc")?;
    session.expect("nxedit: close cancelled, still editing")?;
    if session.transcript()[from..].contains("desktop-shell: ending the session cancelled") {
        return Err("Escape, meant for the editor's question, cancelled the logout".into());
    }
    println!("  ok: the waiting dialog left the keyboard with the editor's question");

    // 4. Cancel, and the session goes on.
    click_at(&mut qmp, &mut session, wx + chrome::DIALOG_RIGHT_CX, wy + chrome::DIALOG_BUTTON_CY)?;
    session.expect("desktop-shell: ending the session cancelled; it goes on")?;
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    press(&mut qmp, "esc")?;
    session.expect("desktop-shell: applications menu closed")?;
    println!("  ok: Cancel kept the session");

    // 5. Log out again, and discard in the question the editor asks again: the greeter comes back.
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

    // 6. Log in again; a terminal; Log out — nothing to ask.
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

    // 7. A restart typed at a terminal, with the editor holding something to lose.
    logout_gate_login(&mut qmp, &mut session, size)?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxedit")?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxterm")?;
    type_at_terminal(&mut qmp, "with power shutdown --reboot")?;
    session.expect("desktop-shell: asked to stop; asking every window to close first")?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    // **To the reset, then read back**: a shell that outlived its supervisor's bound never says it
    // is exiting before the machine goes, so waiting for that line would time out rather than say
    // why.
    let from = session.transcript().len();
    session.expect(RESET)?;
    let after = session.transcript()[from..].to_string();
    if after.contains("desktop-shell: waiting dialog open") {
        return Err("a shutdown started at a terminal brought up the waiting dialog".into());
    }
    logout_gate_stopped_in_time(&after)?;
    session.expect(BOOT_GREETER)?;
    println!("  ok: a restart from a terminal asked the windows, waited without a dialog, and a second boot came up");

    // 8. Restart from the desktop, with the editor holding something to lose: End anyway.
    logout_gate_login(&mut qmp, &mut session, size)?;
    logout_gate_launch(&mut qmp, &mut session, APPS_CLICK, "nxedit")?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;
    logout_gate_choose(&mut qmp, &mut session, power_click, RESTART)?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    let (wx, wy) = logout_gate_waiting(&mut session, "Waiting for 1 window to close:")?;
    // **Read back from the click**, not from the shell's line: `shutdown` runs as soon as the broker
    // has spawned it, and can say it is restarting before the shell has heard it was started.
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

    // 9. Shut down from the desktop: the message, on the screen.
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

/// The power menu's rows, as `check-logout` chooses them: each by pressing Down that many times, so
/// their order is `desktop-shell`'s `panel` test's, which pins it (M11 decision 2).
const LOG_OUT: (&str, usize) = ("Log out", 1);

/// See [`LOG_OUT`].
const RESTART: (&str, usize) = ("Restart", 2);

/// See [`LOG_OUT`].
const SHUT_DOWN: (&str, usize) = ("Shut down", 3);

/// What a reset says last on COM1, before the next boot's first line.
const RESET: &str = "power: resetting through the FADT's reset register";

/// **The greeter of a boot**, and not the one presented again after a logout, whose line begins the
/// same way.
const BOOT_GREETER: &str = "desktop-session-mgr: greeter presented,";

/// **The shell went when asked, inside its supervisor's bound**, in the stretch of `transcript` a
/// shutdown or a restart covered.
fn logout_gate_stopped_in_time(transcript: &str) -> R<()> {
    if transcript.contains("desktop-shell is still running after it was asked to stop")
        || !transcript.contains("desktop-shell: asked to stop, exiting")
    {
        return Err("the shell outlived its supervisor's bound: its own wait for the windows must end first".into());
    }
    Ok(())
}

/// Log in at the greeter as the demo account, and wait for the shell's bars.
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

/// Launch `program` from the Applications menu, and return its window's id once the shell has
/// placed it and the screen has settled — so what is typed next reaches it.
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

/// Open the power menu at `power` and choose `row` — one of [`LOG_OUT`], [`RESTART`] and
/// [`SHUT_DOWN`] — by the keyboard.
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

/// Wait for the shell's waiting dialog, check what it says, and return its origin.
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
