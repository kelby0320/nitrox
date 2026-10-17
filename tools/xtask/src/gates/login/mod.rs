//! `check-login`: the graphical greeter, then the session's desktop driven step by step.

use crate::*;

mod apps;
mod clipboard;
mod dialogs;
mod overview;

pub(crate) use apps::*;
pub(crate) use clipboard::*;
pub(crate) use dialogs::*;
pub(crate) use overview::*;

/// **The shell's last whole window list** in `transcript`: the last line holding `window list on
/// cli of ` **that has ended**, since a list read half-arrived counts too few.
pub(crate) fn last_window_list(transcript: &str) -> Option<&str> {
    let whole = &transcript[..transcript.rfind('\n').map_or(0, |i| i + 1)];
    let (i, m) = whole.rmatch_indices("window list on cli of ").next()?;
    whole[i + m.len()..].lines().next()
}

/// **Paced on the greeter's redraw counter**: a greeter has no echo, and each keystroke repaints
/// 420×200.
pub(crate) fn cmd_check_login(accel: Accel, usb: bool, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;

    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-login.sock");

    println!("xtask: graphical login gate — booting the release image…\n");
    let (mut session, mut qmp) = spawn_release_guest(accel, "check-login", &qmp_sock, size, false, usb)?;

    // 0. **The AHCI controller took the MSI path in a *release* image.** Ordered **first**:
    //    `expect` consumes what it scans past. It reads the path token, not the vector.
    session.expect("ahci: irq via MSI")?;

    // 0b. **The greeter is centred on this gate's screen**; [`GREETER`] says what keeps its size
    //     honest. **Before the first redraw**: the greeter reads the screen before opening its
    //     window.
    session.expect(&format!(
        "desktop-session-mgr: greeter centred at {},{} on a {} screen",
        (size.w - GREETER.0) / 2,
        (size.h - GREETER.1) / 2,
        size
    ))?;
    println!("  ok: the greeter centred itself on the {size} screen");

    session.expect("desktop-session-mgr: greeter redraw 1")?;
    session.expect("desktop-session-mgr: greeter presented")?;

    // 2. **A wrong password is refused**, before the right one so a broken denial cannot hide.
    type_at_greeter(&mut qmp, &mut session, DEMO_USER)?;
    press(&mut qmp, "tab")?;
    type_at_greeter(&mut qmp, &mut session, "wrong-password")?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-session-mgr: login denied")?;

    type_at_greeter(&mut qmp, &mut session, DEMO_USER)?;
    press(&mut qmp, "tab")?;
    type_at_greeter(&mut qmp, &mut session, DEMO_PASSWORD)?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-session-mgr: login ok -> home=/home/alice")?;
    session.expect("desktop-session-mgr: session namespace built (no /dev/console)")?;
    session.expect("desktop-session-mgr: session has /dev/views")?;
    session.expect("desktop-session-mgr: session has /dev/devices")?;
    session.expect("desktop-session-mgr: session has /storage")?;
    // **The leader's own line, and only it**: `libsession` logs "spawned" from the *parent* while
    // the child logs this itself, so their order is a race between two processes.
    session.expect("desktop-shell: up (graphical session leader)")?;
    // **The theme is read before anything is drawn**, so the expectation sits at its position in
    // the stream. **One line whichever way it went**, which lets the "delete the file" control run
    // against this gate.
    session.expect("desktop-shell: theme ")?;
    let theme_line = session.rest_of_line()?;
    let shell_px = theme_line
        .rsplit_once("font_px ")
        .and_then(|(_, n)| n.trim().parse::<u32>().ok())
        .ok_or_else(|| format!("no font_px in the shell's theme line: {theme_line:?}"))?;
    println!("  ok: the shell resolved a theme ({})", theme_line.trim());

    // **The wallpaper**, right after the theme, where it is in the stream. **The decoded size
    // discriminates**: the staged picture is not the screen's size. **The drawn size tells fit from
    // fill**: on 1360×768 a fill is `1360x850 at 0,-41`, a fit `1228x768 at 66,0`. Printed after
    // the window is committed, so `window N` makes this about a picture on screen.
    let drawn = libdraw::scale::fill(
        libdraw::geom::Size::new(WALLPAPER_W, WALLPAPER_H),
        libdraw::geom::Size::new(size.w, size.h),
    );
    session.expect(&format!(
        "desktop-shell: wallpaper {WALLPAPER_W}x{WALLPAPER_H} drawn {}x{} at {},{} window ",
        drawn.size.w, drawn.size.h, drawn.origin.x, drawn.origin.y
    ))?;
    let wallpaper_line = session.rest_of_line()?;
    let wallpaper_id: u32 = wallpaper_line
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("no window id in the wallpaper line: {wallpaper_line:?}"))?;
    println!("  ok: the guest decoded a {WALLPAPER_W}x{WALLPAPER_H} PNG and put it on screen");
    // The shell cannot resolve its own endpoint (it would block on its own answer): it reports the
    // bind.
    session.expect("desktop-shell: clock ")?;
    session.expect("desktop-shell: serving /dev/desktop")?;
    session.expect("desktop-shell: application /dev/desktop bound")?;
    session.expect("desktop-shell: application namespace grants new + /home + /session/user + /dev/devices + /storage, withholds manage")?;
    session.expect("desktop-shell: top bar presented")?;
    // **The bottom bar is created before the manager attaches**: a `panel` is held like a `normal`
    // window and `create` blocks until the first `Configure`, so creating it after `manage()`
    // parked the shell unable to send the `Place` that releases it.
    session.expect("desktop-shell: bottom bar presented")?;
    session.expect("desktop-shell: manager channel held")?;
    // **This proves the path, not the value**: the staged scheme and the compositor's start are
    // both light. The mapping is pinned by
    // `a_scheme_repaints_everything_once_and_an_unknown_one_is_refused`.
    let scheme = staged_theme()?.0.scheme;
    session.expect(&format!("compositor: scheme {}", scheme.as_str()))?;

    session.expect("desktop-shell: work area ")?;
    let work_line = session.rest_of_line()?;
    let work = parse_work_area(&work_line).ok_or_else(|| format!("could not read the work area from {work_line:?}"))?;
    if work.3 >= work.5 {
        return Err(format!(
            "the work area is as tall as the screen ({work_line}) — the shell's bars declare \
             struts, so a work area that ignores them would put every maximised window under \
             them"
        )
        .into());
    }
    println!("  ok: the work area is {}x{} of a {}x{} screen", work.2, work.3, work.4, work.5);

    session.expect(&format!("desktop-shell: bottom bar placed at 0,{}", size.bottom_bar_y()))?;
    session.expect(&format!("desktop-shell: /applications lists {STAGED_APPLICATIONS} application(s)"))?;
    session.expect("Super+R names, Super+A opens applications")?;
    session.expect("desktop-shell: Super+H minimizes the focused window")?;

    let mut st = LoginState { qmp_sock, shell_px, wallpaper_id, work, ..LoginState::default() };
    login_menus_and_buttons(&mut qmp, &mut session, size, &mut st)?;
    login_window_list_and_overview(&mut qmp, &mut session, size, &mut st)?;
    login_overview_drags(&mut qmp, &mut session, &mut st)?;
    login_file_browser(&mut qmp, &mut session, &mut st)?;
    login_editor(&mut qmp, &mut session, size, &mut st)?;
    login_buffers_and_clipboard(&mut qmp, &mut session, &mut st)?;
    login_dialogs_and_places(qmp, session, size, &mut st)
}

/// A receipt per character: a dropped PS/2 batch would otherwise show as a *wrong file*.
pub(crate) const TYPED: &str = "nitrox";

/// The bar's first word, `Applications`, is 105 px wide at the staged 12 px (111 at 13), so
/// (60, 12) lands inside it; `desktop_shell::panel`'s test pins this literal at both sizes.
pub(crate) const APPS_CLICK: (i32, i32) = (60, 12);

/// The title bar's height, shared with the compositor.
pub(crate) const TITLE_BAR_H: i32 = chrome::TITLE_BAR_H;

/// The menu bar above the path strip; `nxfiles::list_top` is the browser's own sum.
pub(crate) const MENU_BAR_H: i32 = 24;

/// The sidebar's width: listing rows start past it, the strips above span the full width.
pub(crate) const SIDEBAR_W: i32 = browser::SIDEBAR_W;

/// A listing row's height.
pub(crate) const ROW_H: i32 = browser::ROW_H;

/// The path strip's height.
pub(crate) const PATH_H: i32 = browser::PATH_H;

/// Press `code` with `Super`, and `Shift` too when `shift`, then let them all go.
pub(crate) fn chord(qmp: &mut Qmp, shift: bool, code: &str) -> R<()> {
    qmp.send_key("meta_l", true)?;
    if shift {
        qmp.send_key("shift", true)?;
    }
    qmp.send_key(code, true)?;
    qmp.send_key(code, false)?;
    if shift {
        qmp.send_key("shift", false)?;
    }
    qmp.send_key("meta_l", false)?;
    Ok(())
}

/// What `check-login`'s steps hand on to the steps after them.
#[derive(Default)]
pub(crate) struct LoginState {
    pub(crate) edit_id: u32,
    pub(crate) files_win: (u32, i32, i32, u32, u32),
    pub(crate) first_term_id: u32,
    pub(crate) fx: i32,
    pub(crate) fy: i32,
    pub(crate) list_click: (i32, i32),
    pub(crate) qmp_sock: PathBuf,
    pub(crate) shell_px: u32,
    pub(crate) term_h: u32,
    pub(crate) term_id: u32,
    pub(crate) term_w: u32,
    pub(crate) term_x: i32,
    pub(crate) term_y: i32,
    pub(crate) wallpaper_id: u32,
    pub(crate) work: (i32, i32, u32, u32, u32, u32),
}

/// `check-login`'s steps 4 to 6: the Applications menu, a launched terminal, and its and the
/// taskbar's buttons.
fn login_menus_and_buttons(qmp: &mut Qmp, session: &mut Session, size: DisplaySize, st: &mut LoginState) -> R<()> {
    let work = st.work;
    // 4. **The Applications menu, by both triggers**; `press at` is asserted first.
    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;

    // **The chord closes what the word opened, then reopens it**; a chord that only opened would
    // leave no way out but Escape.
    chord(qmp, false, "a")?;
    session.expect("desktop-shell: applications menu closed")?;
    chord(qmp, false, "a")?;
    session.expect("desktop-shell: applications menu open")?;

    type_into_menu(qmp, session, "nxterm")?;
    press(qmp, "ret")?;
    session.expect("desktop-shell: application namespace grants new + /home + /session/user + /dev/devices + /storage, withholds manage")?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    // **Only the shell's own lines are ordered here**: `nxterm` starts concurrently with the shell
    // closing the menu, so it is checked against the whole transcript below.
    session.expect("desktop-shell: applications menu closed")?;
    // **The shell placed it, after the close**: a `normal` window's first `Configure` is *held*
    // until the manager acts, and a `popup` never is.
    session.expect("desktop-shell: placed window ")?;
    let placed = session.rest_of_line()?;
    let (term_id, term_x, term_y) =
        parse_placement(&placed).ok_or_else(|| format!("could not read the terminal's placement from {placed:?}"))?;
    session.expect(&format!("desktop-shell: window {term_id} geometry "))?;
    let geom_line = session.rest_of_line()?;
    let (term_w, term_h) = parse_geometry(&geom_line)
        .ok_or_else(|| format!("could not read the terminal's geometry from {geom_line:?}"))?;

    session.expect("desktop-shell: window list on ")?;

    let list_click: (i32, i32) = size.task_click(0);

    // 6a2. **The title bar's buttons ask, and the shell disposes.** Measured from the **right
    //      edge** in layout order (minimise, maximise, close), which a changing layout exposes.
    let button_y = term_y + chrome::TITLE_Y;
    let right = term_x + term_w as i32;
    let maximise_at = (right - chrome::MAXIMISE_X, button_y);
    let minimise_at = (right - chrome::MINIMISE_X, button_y);

    click_at(qmp, session, maximise_at.0, maximise_at.1)?;
    // **In any order**: `nxterm` logs after the compositor answers, so the shell's line can lead.
    let maximised = format!("desktop-shell: maximize window {term_id} to {},{} {}x{}", work.0, work.1, work.2, work.3);
    session.expect_all(&["nxterm: asked the shell for window state 2", &maximised])?;
    println!("  ok: maximise asked for the work area, not the screen");

    session.expect(&format!("nxterm: resized to {}x{}, grid ", work.2, work.3))?;
    // **The committed geometry, not the client's report**: the window keeps its old size until the
    // next frame is committed, and a click on the first line lands outside it.
    session.expect(&format!("desktop-shell: window {term_id} geometry {},{} {}x{}", work.0, work.1, work.2, work.3))?;
    click_at(qmp, session, work.0 + work.2 as i32 - chrome::MAXIMISE_X, work.1 + chrome::TITLE_Y)?;
    session.expect_all(&[
        "nxterm: asked the shell for window state 0",
        &format!("desktop-shell: restore window {term_id} to "),
    ])?;
    session.expect(&format!("nxterm: resized to {term_w}x{term_h}, grid "))?;
    println!("  ok: and the same button restored it to where it came from");

    click_at(qmp, session, minimise_at.0, minimise_at.1)?;
    session.expect_all(&[
        "nxterm: asked the shell for window state 1",
        &format!("desktop-shell: client asked to minimize window {term_id}"),
        &format!("desktop-shell: window list on Desktop 1 of 2 [{term_id}:_ "),
    ])?;

    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;

    // **And minimise a second time**: the taskbar restore is a `SetMinimized` that bypassed the
    // compositor's drop-repeats shadow, so the second press was dropped as a repeat.
    click_at(qmp, session, minimise_at.0, minimise_at.1)?;
    session.expect_all(&[
        "nxterm: asked the shell for window state 1",
        &format!("desktop-shell: client asked to minimize window {term_id}"),
    ])?;
    println!("  ok: the minimise button still works after the taskbar restored the window");
    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;

    // 6a3. **The taskbar asks, and the client closes.** **The control is the live client**:
    //      `nxterm` says it was asked and is closing; a shell that destroyed the window would
    //      produce neither line.
    middle_click_at(qmp, session, list_click.0, list_click.1)?;
    // **Only the ordered half is an `expect`**: the *shell's* line races the client it woke.
    session.expect(&format!("compositor: asked window {term_id} to close"))?;
    session.expect("nxterm: asked to close, exiting")?;
    session.expect("nxterm: closing")?;
    session.expect("desktop-shell: the compositor follows to Desktop 1")?;
    session.expect("desktop-shell: window list on Desktop 1 of 1 (empty)")?;
    println!("  ok: the taskbar asked, and the client closed itself");
    let first_term_id = term_id;

    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(qmp, session, "nxterm")?;
    // **The menu hangs 8 px in from its word, 2 below the bar**; `desktop_shell::panel`'s test
    // pins y 69 to 93 (71 to 97 at 13).
    const ROW1: (i32, i32) = (60, 82);
    click_at(qmp, session, ROW1.0, ROW1.1)?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    session.expect("desktop-shell: placed window ")?;
    let replaced = session.rest_of_line()?;
    let (term_id, term_x, term_y) = parse_placement(&replaced)
        .ok_or_else(|| format!("could not read the replacement terminal's placement from {replaced:?}"))?;
    session.expect(&format!("desktop-shell: window {term_id} geometry "))?;
    let regeom = session.rest_of_line()?;
    let (term_w, _term_h) =
        parse_geometry(&regeom).ok_or_else(|| format!("could not read the replacement's geometry from {regeom:?}"))?;

    // 6. **The top bar still works** after a modal closed. **Escape first**: an abandoned
    //    `click_at` attempt on the Applications word leaves the menu open, and the aimed click
    //    would close and reopen it, matching either way.
    press(qmp, "esc")?;
    session.skip_to_end()?;
    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;

    st.first_term_id = first_term_id;
    st.list_click = list_click;
    st.term_h = term_h;
    st.term_id = term_id;
    st.term_w = term_w;
    st.term_x = term_x;
    st.term_y = term_y;
    Ok(())
}

/// Whether the file browser started at `path` and **then** listed it: an earlier listing of the
/// same directory says nothing about this browser.
pub(crate) fn started_and_listed(transcript: &str, path: &str) -> Result<(), String> {
    let start = format!("nxfiles: starting at {path}");
    let listed = format!("nxfiles: listed {path} - ");
    let mut lines = transcript.lines().map(|l| l.trim_end_matches('\r'));
    if !lines.any(|l| l == start) {
        return Err(format!("the transcript has no \"{start}\""));
    }
    if !lines.any(|l| l.starts_with(&listed)) {
        return Err(format!("nothing after \"{start}\" says \"{listed}…\""));
    }
    Ok(())
}

/// Assert the graphical session was still running when the serial one started.
///
/// **An absence, which `expect` cannot express**: only `desktop-session-mgr` never reporting a
/// session end proves the two overlapped.
pub(crate) fn check_two_sessions(transcript: &str) -> R<()> {
    // Checked against the transcript, since `nxterm` runs concurrently with the shell. **Not the
    // grid line**: `nxterm::report_row` is `test-harness`-only. **Asserted from `nxsh`'s own
    // line**: a parent cannot testify to what its child was given. Two shells must report.
    let shells = transcript.matches("nxsh: up (env: ").count();
    if transcript.contains("nxsh: up (env: 0 fields)") {
        return Err("a shell started with an empty environment, so its `$env.HOME` is unset. \
             Somewhere between `desktop-session-mgr` and `nxsh` a setup message was not sent \
             or not forwarded"
            .into());
    }
    if shells < 2 {
        return Err(format!(
            "expected two shells to report an environment (the launched terminal's and the \
             serial login's); saw {shells}. A shell that never started cannot have an empty \
             environment, which is why absence alone is not the check."
        )
        .into());
    }
    if !transcript.contains("nxterm: hosting a shell") {
        return Err("the launched terminal never hosted a shell. `nxterm: no shell` means it \
             could not spawn one — most likely `/bin` is missing from the application \
             namespace; no `nxterm:` line at all means it never started"
            .into());
    }
    // **An empty applications list would satisfy every expect above**, so it is asserted as an
    // absence. **`(table)`**: a release-image gate cannot see stdout, so the command names its
    // branch.
    for line in ["desktop: running", "desktop: listed 3 desktops (table)", "desktop: named 2 cli"] {
        if !transcript.contains(line) {
            return Err(format!(
                "the `desktop` command did not report \"{line}\". It resolves /dev/desktop from \
                 the namespace `desktop-shell` built for the terminal that ran it, so a missing \
                 line means either the bind or the resolve — check for \"nxsh: could not \
                 resolve\", which is what a command that never started looks like"
            )
            .into());
        }
    }

    // **The configure deadline must not fire**: it names a wedged manager, and a bottom bar created
    // late made it fire on every healthy boot.
    if transcript.contains("compositor: no manager answer for window") {
        return Err("the compositor's configure deadline fired during a healthy session start: \
             some window was held for the manager until the 200ms timeout. That timer exists for \
             a wedged or absent manager, so a session that needs it has a shell blocking on \
             something only the shell could answer — check what was created after \
             `/dev/draw/manage` was taken"
            .into());
    }
    if transcript.contains("desktop-shell: /applications lists 0 application(s)") {
        return Err("the Applications menu is empty: no desktop entries reached the session \
             namespace. Either the `/applications` bind failed (the session log says so), the \
             directory would not open, or every entry was malformed. The menu opening is not \
             evidence it has anything in it"
            .into());
    }
    if !transcript.contains("desktop-shell: up (graphical session leader)") {
        return Err("the graphical session never started, so nothing was concurrent".into());
    }
    if transcript.contains("desktop-session-mgr: session ended") {
        return Err("the graphical session ended before the serial login finished, so the two \
             were sequential rather than concurrent. Its leader blocks and should outlive this \
             gate — check the transcript for why it exited"
            .into());
    }
    println!("xtask: a graphical and a serial session ran at the same time ✓");
    Ok(())
}

/// Move the browser's selection from a fresh `/home` listing down to `papers`, since row 0 is
/// not `papers`: each arrow is acknowledged by `selected`, both pace and assertion.
pub(crate) fn select_papers(qmp: &mut Qmp, session: &mut Session) -> R<()> {
    for name in ["Downloads", "Pictures", "papers"] {
        press(qmp, "down")?;
        session.expect(&format!("nxfiles: selected {name}"))?;
    }
    Ok(())
}

/// Wait for `pat` after injecting pointer input, **flushing QEMU's mouse queue while waiting**.
///
/// **QEMU holds a packet it cannot fit rather than dropping it**: held buttons go out only at the
/// next injected event, so a gate whose last event is a release can wait for ever
/// (`hw/input/ps2.c`). Each second without `pat` sends [`Qmp::flush`]; forty-five seconds in all,
/// as [`Session::expect`]. A press held together with walk motion still lands short.
pub(crate) fn expect_after_pointer(qmp: &mut Qmp, session: &mut Session, pat: &str) -> R<()> {
    const SLICE: std::time::Duration = std::time::Duration::from_secs(1);
    const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
    let start = std::time::Instant::now();
    while start.elapsed() < TIMEOUT {
        if session.expect_within(pat, SLICE)? {
            return Ok(());
        }
        qmp.flush()?;
    }
    Err(session.timeout_report(&format!("{pat:?}"), TIMEOUT)?.into())
}
