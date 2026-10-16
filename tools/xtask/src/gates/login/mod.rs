use crate::*;

mod overview;

pub(crate) use overview::*;

/// **The shell's last whole window list** in `transcript`: what follows `window list on cli of ` on
/// the last line holding it **that has ended**. The transcript is read as it arrives, so the shell's
/// next list may be half there — a line cut after its second window, counted, said two, and once
/// after its first said one: `check-login --usb`'s two failures in Phase 6 Part B.7's rounds, where
/// the whole line before it held all three. A match is not a line (PR #352 review).
pub(crate) fn last_window_list(transcript: &str) -> Option<&str> {
    let whole = &transcript[..transcript.rfind('\n').map_or(0, |i| i + 1)];
    let (i, m) = whole.rmatch_indices("window list on cli of ").next()?;
    whole[i + m.len()..].lines().next()
}

/// **Paced on the greeter's redraw counter**, because a greeter has no echo. What was typed is
/// on screen and nowhere else, and this window repaints 420×200 per keystroke — the cost
/// `check-terminal` types one character at a time to stay behind. Waiting for the redraw is
/// the same discipline against a different receipt.
pub(crate) fn cmd_check_login(accel: Accel, usb: bool, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;

    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-login.sock");

    println!("xtask: graphical login gate — booting the release image…\n");
    // **`--usb`: a machine with no i8042** (Phase 6 Part B), so every key at the greeter is typed on
    // a USB keyboard: the Definition of Done's "types at the greeter".
    let (mut session, mut qmp) = spawn_release_guest(accel, "check-login", &qmp_sock, size, false, usb)?;

    // 0. **The AHCI controller took the MSI path in a *release* image.** `test-qemu` already
    //    adjudicates this (`check_ahci_msi_path`), but only for the selftest build — and the
    //    image that will be carried to the laptop is this one. It costs one expectation
    //    because the driver binds long before anything draws.
    //
    //    Ordered **first**, before the greeter lines, because `expect` consumes what it scans
    //    past: asserting it after them would find nothing and hang. It is also why this reads
    //    the path token rather than the vector — the vector differs between this image and the
    //    selftest one (0x30 here, 0x31 there), since the selftest build's `IrqRouter::self_test`
    //    takes a device vector for the PIT and never releases it.
    session.expect("ahci: irq via MSI")?;

    // 0b. **The greeter is centred on the screen this gate booted** (Phase 5 Part E). It centred
    //     on a written-down 1280×800 until then, which on the laptop put it 43 px left and 16 px
    //     low of centre. Its size is `desktop-session-mgr`'s `GREETER_W`×`GREETER_H`, written down
    //     here a second time for the reason every chrome metric in this file is (M11 decision 2).
    //     [`GREETER`] says what keeps this copy honest.
    //     **Before the first redraw**, because the greeter reads the screen before it opens its
    //     window, and `expect` consumes what it scans past.
    session.expect(&format!(
        "desktop-session-mgr: greeter centred at {},{} on a {} screen",
        (size.w - GREETER.0) / 2,
        (size.h - GREETER.1) / 2,
        size
    ))?;
    println!("  ok: the greeter centred itself on the {size} screen");

    // 1. The greeter is up before anyone has authenticated. That is the claim Part D's second
    //    box makes, and in a release image nothing else has drawn anything.
    // The redraw is logged inside `present`, so it precedes the line that announces the
    // window — asserting them the other way round consumes the transcript past it.
    session.expect("desktop-session-mgr: greeter redraw 1")?;
    session.expect("desktop-session-mgr: greeter presented")?;

    // 2. **A wrong password is refused**, tested before the right one so a broken denial
    //    cannot hide behind a successful login — the same ordering `test-interactive` uses.
    type_at_greeter(&mut qmp, &mut session, DEMO_USER)?;
    press(&mut qmp, "tab")?;
    type_at_greeter(&mut qmp, &mut session, "wrong-password")?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-session-mgr: login denied")?;

    // 3. The right one reaches a session. Each line is a distinct claim: the oracle answered,
    //    the namespace was built **without a console**, and an unprivileged leader started in
    //    it.
    type_at_greeter(&mut qmp, &mut session, DEMO_USER)?;
    press(&mut qmp, "tab")?;
    type_at_greeter(&mut qmp, &mut session, DEMO_PASSWORD)?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-session-mgr: login ok -> home=/home/alice")?;
    session.expect("desktop-session-mgr: session namespace built (no /dev/console)")?;
    // **And the view broker's `/dev/views`, at the session's base** (administration Part A.4).
    // Printed before the leader is spawned, so it is ordered against the next line, not racing it.
    session.expect("desktop-session-mgr: session has /dev/views")?;
    // **And the device manager's tables, at the base `/info`** (administration Part B.4) — printed
    // right after, from what was bound. The application namespaces the shell builds carry them too,
    // which the shell checks by resolving the binding and says on its `grants` line below.
    session.expect("desktop-session-mgr: session has /dev/devices")?;
    // **And `/storage`** (administration Part C.6): the storage service's session endpoint, which
    // the supervisor resolved itself and bound at `/storage` and `/dev/storage`.
    session.expect("desktop-session-mgr: session has /storage")?;
    // **The leader's own line, and only it.** `libsession` logs "spawned … with its
    // environment" from the *parent* after the setup message goes out, while the child logs
    // this from its first instruction — so their order is a race between two processes, and
    // asserting one would be a flake that passes until it does not (PR #227 review). This is
    // the stronger claim anyway: the parent saying it spawned something is not evidence the
    // something ran. `test-interactive` already pins the `libsession` line for the serial
    // column, where it is ordered against a prompt rather than against another process.
    session.expect("desktop-shell: up (graphical session leader)")?;
    // **The theme, read before anything is drawn** — which is also where this assertion has to
    // sit: the shell reads it in `_start`, before its first bar exists, so an expectation placed
    // beside the *topic* rather than beside its position in the stream scans past it (M11
    // Part C). It comes from the user's own subtree rather than `/etc`, because a session
    // namespace binds `/home` and has no `/etc` — no new authority, and the file is somewhere a
    // person can actually delete.
    // **One line whichever way it went**, which is what lets the "delete the file" control run
    // against this gate rather than needing this step edited out (PR #263 review, finding 4).
    // What it takes from the line is the size the shell *resolved*; the client's own line is
    // compared against it below.
    session.expect("desktop-shell: theme ")?;
    let theme_line = session.rest_of_line()?;
    let shell_px = theme_line
        .rsplit_once("font_px ")
        .and_then(|(_, n)| n.trim().parse::<u32>().ok())
        .ok_or_else(|| format!("no font_px in the shell's theme line: {theme_line:?}"))?;
    println!("  ok: the shell resolved a theme ({})", theme_line.trim());

    // **The wallpaper, decoded in the guest** (M12 Part F). Immediately after the theme, because
    // that is where it is in the stream: the shell reads the theme, opens the picture it names,
    // and only then creates its first bar. An expectation placed beside the *topic* rather than
    // beside its position scans past output that was there — the same rule the theme's own line
    // above states, and the one M11 Part C learned the expensive way.
    //
    // **The decoded size is the half that discriminates.** `1920x1200` can only have come from
    // an `IHDR` the guest actually read — the staged picture is deliberately not the screen's
    // size, so a shell that decoded nothing cannot report it, exactly as `THEME_FONT_PX` is
    // deliberately not the built-in 13.
    //
    // **The drawn size tells fit from fill again, at the gates' screen** (Phase 5 Part E). On the
    // 1280×800 screen every gate booted until then, 16:10 into 16:10 is `1280x800 at 0,0` under
    // either rule; on 1360×768 a fill is `1360x850 at 0,-41` and a fit `1228x768 at 66,0`. The
    // expected line is computed here with `libdraw::scale::fill` — the staged theme's mode — on
    // the size this gate booted, so the arithmetic has one source and its own tests pin it.
    //
    // A picture is pixels a release-image boot has no reference for, which is what the plan says
    // about this gate; the dimensions are what can be asserted, and they pin the whole chain —
    // the theme naming a file and a mode, `libfs` reading it, `libdraw::png` decoding it, and
    // `libdraw::scale::fill` placing it.
    // **And the line is printed after the window is committed**, which the first version of
    // this step was not: it asserted a line emitted as soon as the picture had been placed, so
    // it passed while `CreateWindow` failed and the desktop showed its bare ground colour. The
    // `window N` on the end is what makes this an assertion about a picture on screen rather
    // than about arithmetic.
    let drawn = libdraw::scale::fill(
        libdraw::geom::Size::new(WALLPAPER_W, WALLPAPER_H),
        libdraw::geom::Size::new(size.w, size.h),
    );
    session.expect(&format!(
        "desktop-shell: wallpaper {WALLPAPER_W}x{WALLPAPER_H} drawn {}x{} at {},{} window ",
        drawn.size.w, drawn.size.h, drawn.origin.x, drawn.origin.y
    ))?;
    // **Kept, because stickiness is asserted against this id** further down: a press on an empty
    // desktop has to name *this* window, and `win=none` is what a wallpaper stamped with
    // desktop 1 gives.
    let wallpaper_line = session.rest_of_line()?;
    let wallpaper_id: u32 = wallpaper_line
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("no window id in the wallpaper line: {wallpaper_line:?}"))?;
    println!("  ok: the guest decoded a {WALLPAPER_W}x{WALLPAPER_H} PNG and put it on screen");
    // **The narrow bind, which is what closed the `manage-ungated` deferral.** The shell builds
    // an application namespace and checks it *before* launching into it: `/dev/draw/new`
    // resolves, `/dev/draw/manage` does not. Asserting the shell's own verdict rather than
    // re-deriving it here keeps the check where the refusal is — a shell that found the gate
    // open declines to launch, which is behaviour rather than a test.
    // **The endpoint is bound into application namespaces, not into the session's.** The shell
    // cannot check this by resolving it — a resolve is forwarded to whoever serves the path, so
    // asking the kernel for its own endpoint would block it waiting for its own answer. The
    // bind's success is what it can report; the `desktop` command below is what proves the
    // binding is reachable (M8 Part F).
    // **The clock read the wall clock** (M11 Part E batch 9), which is the one thing about it a
    // gate can see: its value changes every minute, and this boot has no reference render of a
    // bar to compare pixels against. What the line distinguishes is a clock absent because the
    // RTC was unreadable from a bar that failed to draw one — the *formatting* is a host test in
    // `libtime`, where it belongs.
    //
    // Immediately after the theme, because that is where it is: the shell reads both before it
    // draws anything, and an expectation placed beside its topic rather than its position in the
    // stream scans past output that was there.
    session.expect("desktop-shell: clock ")?;
    // **And `/session/user`, since administration Part F.1**: `desktop-shell` builds an
    // application's namespace with `libsession`, as the supervisors build a session's, and the
    // one piece its own builder had never bound was who the session is for. `whoami` in a desktop
    // terminal said "no session identity" until then. The shell resolves it in the namespace it
    // built, and names it here only if it answered.
    session.expect("desktop-shell: serving /dev/desktop")?;
    session.expect("desktop-shell: application /dev/desktop bound")?;
    session.expect("desktop-shell: application namespace grants new + /home + /session/user + /dev/devices + /storage, withholds manage")?;
    // **And it draws.** M7 Part E makes the shell a real compositor client: it resolves
    // `/dev/draw` from the namespace `desktop-session-mgr` built — not from a root one, which
    // it does not have — and presents a `panel` top bar. Asserting the window rather than only
    // the process is what distinguishes a session that runs from one that is merely spawned.
    session.expect("desktop-shell: top bar presented")?;
    // **The compositor's first real manager.** Everything M6 built for one -- placement,
    // restacking, the initial-configure hold -- has been exercised by a test client until now.
    // Holding the channel is also the other half of what closed `manage-ungated`: the shell's
    // session namespace binds the `/dev/draw` subtree and reaches `manage`, an application's
    // binds `/dev/draw/new` alone and does not.
    // **The bottom bar is created here, before the manager attaches** (M8 Part C). It has to be
    // *placed*, and only a manager can place — but a `panel` is held for the manager exactly
    // like a `normal` window, and `create` blocks until the first `Configure`. Creating it after
    // `manage()` parked the shell inside `create`, unable to drain the channel and so unable to
    // send the `Place` that would release it; only the 200 ms configure deadline broke the tie.
    session.expect("desktop-shell: bottom bar presented")?;
    session.expect("desktop-shell: manager channel held")?;
    // **The shell's `SetScheme` reached the compositor and was accepted** (desktop refresh, Part
    // A) — the one process that never reads the theme file, and the one that draws every window's
    // shadow. The shell sends it whatever the scheme, so the line appears on a light boot too.
    //
    // **What this proves on this image is the path, not the value.** The staged scheme is light
    // and the compositor starts light, so a compositor that accepted the message and applied
    // nothing would log the same line and pass. The value's mapping is pinned by the manager's
    // unit test, `a_scheme_repaints_everything_once_and_an_unknown_one_is_refused`. The expected
    // name is computed from the staged theme rather than written as `light` so that the day the
    // staged scheme changes, this starts checking the value without anyone remembering to
    // (PR #312 review, optional 7, which caught the comment claiming more).
    let scheme = staged_theme()?.0.scheme;
    session.expect(&format!("compositor: scheme {}", scheme.as_str()))?;

    // **The work area, and that it is not the screen** (M9 Part B). The shell asks the
    // compositor rather than subtracting its own bars, because any `panel`-role client declares
    // a strut and only the compositor sees them all. If this ever equals the screen, the struts
    // are not being counted and every maximised window will sit under the bars.
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

    // **And the position, asserted rather than inferred.** A dock edge reserves space; it does
    // not move the window. Without this the bar's placement was only covered by proxy — the
    // window-list click landing on nothing — which says the bar is not *there* rather
    // than where it is (PR #242 review, optional 9).
    session.expect(&format!("desktop-shell: bottom bar placed at 0,{}", size.bottom_bar_y()))?;
    // **The count, not just the prefix** (PR #279 review, finding 7). This is the one line that
    // distinguishes desktop entries from the `/bin` listing they replaced: a regression to
    // listing every program would still open the menu, still match `nxterm`, and still launch it,
    // so every other expectation in this gate holds under both behaviours.
    session.expect(&format!("desktop-shell: /applications lists {STAGED_APPLICATIONS} application(s)"))?;
    // After it, because the chord is registered on the loop's first pass and the entries
    // are read before the loop. `expect` scans forward, so asserting these out of order times
    // out on a line already behind the cursor.
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

// **A receipt per character, and it is a count rather than an echo.** An editor's echo is
// its own window, and this gate boots a release image — there are no rendered glyphs to
// read on serial. Injection is relative and unacknowledged, so without a per-key wait a
// dropped PS/2 batch would show up as a *wrong file* two steps later.
pub(crate) const TYPED: &str = "nitrox";

//    The bar spans the screen at y=0 and `Applications` is its first word, 105 pixels wide
//    at the staged 12 px (111 at the built-in 13), so a press at (60, 12) lands inside it —
//    which `desktop_shell::panel`'s own test pins against this literal at both sizes
//    (desktop refresh, Part C).
pub(crate) const APPS_CLICK: (i32, i32) = (60, 12);

pub(crate) const TITLE_BAR_H: i32 = chrome::TITLE_BAR_H;

// **And the menu bar above the path strip** (M12 Part B), which moved every row down by its
// height. `nxfiles::list_top` is the browser's own version of this sum; a gate that had
// missed the change would press one row high and drag the wrong file.
pub(crate) const MENU_BAR_H: i32 = 24;

// **Where a listing row is.** Hoisted here from the drag step below, which is the other thing
// that aims at one — two copies of this sum is how a gate comes to press one row high after
// a strip changes height. `nxfiles::list_top` is the browser's own version of it.
// **The sidebar takes the left of the content** (M14 Part D), so every aim at a *listing* row
// starts past it while the strips above still span the full width. `nxfiles::SIDEBAR_W` is
// the browser's own version of this number.
pub(crate) const SIDEBAR_W: i32 = browser::SIDEBAR_W;

pub(crate) const ROW_H: i32 = browser::ROW_H;

pub(crate) const PATH_H: i32 = browser::PATH_H;

// Every `Super`-chord this gate presses goes through here, so the four `send_key` calls a
// chord is have one spelling rather than one per step.
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

/// `check-login`'s steps 4 to 6: the Applications menu, a terminal launched from it, its title
/// bar's buttons and the taskbar's.
fn login_menus_and_buttons(qmp: &mut Qmp, session: &mut Session, size: DisplaySize, st: &mut LoginState) -> R<()> {
    let work = st.work;
    // 4. **The Applications menu, by both of its triggers** — `desktop-shell.md` §4's word on
    //    the top bar and its chord. The chord is `Super+A` since Phase 5, because the word is on
    //    a `panel` and a panel can only be clicked: on the laptop Phase 5 targets there is no
    //    pointing device until USB (Phase 6), so without a chord a session could be logged into
    //    and never used.
    //
    //    Asserted through the compositor's own `press at` line first, for the reason
    //    `check-terminal` gives: it separates "the pointer was not there" from "the pointer was
    //    there and nothing happened".
    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;

    //    **And the chord, closing what the word opened, then opening it again.** Closing first
    //    is deliberate: it is the half the word cannot do — a press on it dismisses the open menu
    //    and the click then reopens it, as on every window's menu bar — and it leaves the launch
    //    below driven entirely by the keyboard, which is the path the laptop has. A chord that
    //    only ever opened would pass this gate while leaving a person no way out of the menu but
    //    Escape.
    chord(qmp, false, "a")?;
    session.expect("desktop-shell: applications menu closed")?;
    chord(qmp, false, "a")?;
    session.expect("desktop-shell: applications menu open")?;

    // 5. **Type to filter, then launch.** The menu is a `popup`, so it holds the keyboard —
    //    the property `check-terminal` relies on when it says an open menu "is a topmost popup
    //    and takes the keyboard". The top bar could not receive these keys at all.
    //
    //    **`nxterm`, which is what makes the milestone visible**: a person types into the
    //    Applications menu and a terminal opens. Part E launched a coreutil because the
    //    mechanism was the deliverable; Part F is the thing launched being worth looking at.
    type_into_menu(qmp, session, "nxterm")?;
    press(qmp, "ret")?;
    // Each line is a distinct claim: the namespace was built and **checked** before anything
    // ran in it, and only then was the program spawned into it.
    session.expect("desktop-shell: application namespace grants new + /home + /session/user + /dev/devices + /storage, withholds manage")?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    // **Only the shell's own lines are ordered here.** `nxterm` starts concurrently with the
    // shell closing the menu, so an `expect` between the two is a race between processes —
    // the flake PR #227's review caught and PR #236's avoided. What `nxterm` did is checked
    // against the whole transcript below, where order does not matter.
    session.expect("desktop-shell: applications menu closed")?;
    // **And the shell placed it**, which is the manager half actually doing something rather
    // than merely holding a channel. Every window created while a manager is attached is
    // announced to it, and a `normal` one's first `Configure` is *held* until the manager acts
    // — so a shell that received `WindowCreated` and did nothing would leave launched
    // applications invisible, a failure that looks like they never started.
    //
    // **Asserted against the launched terminal, and after the close.** It sat after the menu
    // *opened* until M8 Part C, where the shell stopped placing its own windows — a menu is a
    // `popup`, placed by its creator and never held for anyone, so the line it matched was a
    // placement that could not have been load-bearing. The terminal's placement arrives one
    // loop iteration after the close, when its `WindowCreated` is drained.
    session.expect("desktop-shell: placed window ")?;
    // **Remembered, because the drag below needs to know where it started.** The line reads
    // `<id> at 0,<y>`; nothing moves this window between here and there — a desktop change and a
    // minimise leave the origin alone — so this is its origin at that point.
    let placed = session.rest_of_line()?;
    let (term_id, term_x, term_y) =
        parse_placement(&placed).ok_or_else(|| format!("could not read the terminal's placement from {placed:?}"))?;
    // Its width, which the title bar's buttons are measured from — **before the window-list
    // line**, which follows it: an `expect` scans forward, so asserting the list first would
    // consume past the geometry this needs.
    session.expect(&format!("desktop-shell: window {term_id} geometry "))?;
    let geom_line = session.rest_of_line()?;
    let (term_w, term_h) = parse_geometry(&geom_line)
        .ok_or_else(|| format!("could not read the terminal's geometry from {geom_line:?}"))?;

    // And it is listed, focused, because it has the keyboard.
    session.expect("desktop-shell: window list on ")?;

    // Where a window-list entry sits: the first task button on the bottom bar, whose height comes
    // from the screen this gate booted. `DisplaySize::task_click` is hardcoded like every other
    // chrome metric this gate aims at, and for the same reason: a gate that read the shell's layout
    // to know where to click could agree with a shell that had stopped drawing where it says (M11
    // decision 2).
    let list_click: (i32, i32) = size.task_click(0);

    // 6a2. **The title bar's buttons ask, and the shell disposes** (M9 Part B). A client cannot
    //      minimise or maximise itself — both are manager operations — so the button sends
    //      `Surface::RequestState`, the compositor forwards it, and the shell decides.
    //
    // **The buttons are measured from the right edge in layout order** — minimise, maximise,
    // close; see `chrome`. Part C added close, which moved the other two a slot left; the first
    // run after that clicked *close* where it meant maximise, which is what a coordinate constant
    // hides and a changing layout exposes.
    let button_y = term_y + chrome::TITLE_Y;
    // Measured from the window's **right edge**, which is its origin plus its width — not from
    // its width, which is the same number only while windows are placed at x=0.
    let right = term_x + term_w as i32;
    let maximise_at = (right - chrome::MAXIMISE_X, button_y);
    let minimise_at = (right - chrome::MINIMISE_X, button_y);

    // **Maximise here is asserted as far as the shell's answer** — that the ask reached it and
    // that it answered with the *work area* rather than the screen. What the client does with
    // the size is 6g's, which is where the whole round trip is read back.
    click_at(qmp, session, maximise_at.0, maximise_at.1)?;
    // **In any order**: `nxterm` logs its request after the compositor answers, and the compositor
    // answers once it has forwarded it, so the shell's line can come first (PR #353's CI).
    let maximised = format!("desktop-shell: maximize window {term_id} to {},{} {}x{}", work.0, work.1, work.2, work.3);
    session.expect_all(&["nxterm: asked the shell for window state 2", &maximised])?;
    println!("  ok: maximise asked for the work area, not the screen");

    // **And put it back, which is a gesture in its own right and the only one that sends
    // `WINDOW_STATE_NORMAL`** (M9 Part D). The shell has had a restore path since Part B and
    // nothing could reach it: the button was one-way, which was invisible while the client
    // declined every `Configure` and is a window you cannot get back the moment it does not.
    //
    // **The button has moved**, because the window has: it is the work area now, so its
    // top-right corner is the work area's. A gate that clicked the old coordinates would land
    // on the title bar and start a drag — which is exactly what the first run of this did.
    session.expect(&format!("nxterm: resized to {}x{}, grid ", work.2, work.3))?;
    // **The committed geometry, not the client's report of what it was asked.** The resize line
    // is printed when the `Configure` arrives; the window is still its old size until the frame
    // after that is drawn and committed. Clicking on the strength of the first line lands
    // outside the window — `win=none` — which is what the first run of this step did.
    session.expect(&format!("desktop-shell: window {term_id} geometry {},{} {}x{}", work.0, work.1, work.2, work.3))?;
    click_at(qmp, session, work.0 + work.2 as i32 - chrome::MAXIMISE_X, work.1 + chrome::TITLE_Y)?;
    // **In any order**: `nxterm` logs its request after the compositor answers, and the compositor
    // answers once it has forwarded it, so the shell's line can come first (PR #353's CI).
    session.expect_all(&[
        "nxterm: asked the shell for window state 0",
        &format!("desktop-shell: restore window {term_id} to "),
    ])?;
    // The size is pinned by the client's own line rather than by the shell's: the shell prints
    // what it asked for, and what this step is about is the window going back to the shape it
    // had — which only the client can say it did.
    session.expect(&format!("nxterm: resized to {term_w}x{term_h}, grid "))?;
    println!("  ok: and the same button restored it to where it came from");

    // **And minimise, end to end**, because nothing in it depends on a client honouring
    // anything: the window leaves the screen and the bar marks it with `_`.
    click_at(qmp, session, minimise_at.0, minimise_at.1)?;
    // **In any order** (PR #353's CI, the same class one step down): `nxterm` logs its request
    // after the compositor answers it, and the compositor answers once it has forwarded it, so the
    // shell can be first. The list's `_` exists only after the minimise, so no earlier line
    // answers for it. `_` is the bar's mark for a minimized window. The desktop count is not
    // asserted: the lifecycle rule appends an empty desktop as soon as one has a window, so it is
    // 2 here and says nothing about minimising.
    session.expect_all(&[
        "nxterm: asked the shell for window state 1",
        &format!("desktop-shell: client asked to minimize window {term_id}"),
        &format!("desktop-shell: window list on Desktop 1 of 2 [{term_id}:_ "),
    ])?;

    // Restore it from the list, which is where a minimized window comes back from, so the steps
    // below have a window to work with.
    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;

    // **And minimise it a second time**, which is the step that was missing. The compositor drops
    // a request for the state a window last *asked* to be in, to keep a looping client off a
    // bounded queue — and the taskbar restore above is a `SetMinimized` from the *manager*, which
    // does not go through that path. With the shadow left stale, this second press was dropped as
    // a repeat, the client was told it had succeeded, and the button stayed dead until some other
    // state was asked for first (PR #249 review, blocking 1).
    click_at(qmp, session, minimise_at.0, minimise_at.1)?;
    // **In any order**: `nxterm` logs its request after the compositor answers, and the compositor
    // answers once it has forwarded it, so the shell's line can come first (PR #353's CI).
    session.expect_all(&[
        "nxterm: asked the shell for window state 1",
        &format!("desktop-shell: client asked to minimize window {term_id}"),
    ])?;
    println!("  ok: the minimise button still works after the taskbar restored the window");
    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;

    // 6a3. **The taskbar asks, and the client is what closes** (M9 Part C). Middle-click is the
    //      gesture — the same one every taskbar this borrows from uses, and it needs no room in
    //      a layout that is one fixed slot per window. A window holds a process's work, so the
    //      shell *asks*: `Manage::Close` exists for a client that will not answer, and reaching
    //      for it first would destroy windows out from under processes that were fine.
    //
    //      **The control is the live client, and it is these two assertions rather than a
    //      separate run**: `nxterm` says it was asked and says it is closing, so the window went
    //      away by its own hand. A shell that destroyed it instead would produce neither line —
    //      and the window would be gone all the same, which is why the client's side is the only
    //      place the difference is visible.
    middle_click_at(qmp, session, list_click.0, list_click.1)?;
    // **Only the ordered half is an `expect`.** The compositor logs before it replies, so it
    // leads; the client then wakes and answers. The *shell's* own line comes after its request
    // returns, which is a race against the client it just woke — observed on both sides of the
    // client's two lines — so it is checked against the whole transcript below, where order does
    // not matter. Same rule as `nxterm`'s output in PR #227.
    session.expect(&format!("compositor: asked window {term_id} to close"))?;
    session.expect("nxterm: asked to close, exiting")?;
    session.expect("nxterm: closing")?;
    // **Its desktop went with it, and the compositor was told.** Desktop 1 was emptied and
    // unnamed, so the lifecycle rule removed it and the shell landed on the one after — which is
    // `Desktop 1` now, by position. Until the desktop refresh's Part C this move reached only the
    // shell; step 6c2 is where the difference showed.
    session.expect("desktop-shell: the compositor follows to Desktop 1")?;
    // The compositor tore the windows down with the session, and the list lost the entry.
    session.expect("desktop-shell: window list on Desktop 1 of 1 (empty)")?;
    println!("  ok: the taskbar asked, and the client closed itself");
    let first_term_id = term_id;

    // Launch another, because everything below needs a terminal — and **by clicking the row this
    // time**, which is the second launch path and did not exist until M11 Part E batch 4. The
    // shell read pointer events for the overview, the applications button and the taskbar, and
    // never for the modal's own window: its rows could not be clicked at all, and nothing under
    // the cursor reacted. Everything after this step depends on the terminal, so a click that
    // silently does nothing fails the rest of the gate rather than passing quietly.
    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    // Filtered to one row first, so the row being clicked is known without the gate having to
    // work out where `nxterm` sorts in the contents of `/bin`.
    type_into_menu(qmp, session, "nxterm")?;
    // **The menu hangs 8 pixels in from its word and 2 below the bar**, at (8, 32), with the
    // filter field above its rows (desktop refresh, Part C) — so the one row left sits a field's
    // height below that, at y 69 to 93 in the staged 12 px (71 to 97 at the built-in 13).
    // `desktop_shell::panel`'s own test pins this literal against a layout of the menu at both.
    // `click_at` asserts the press position before anything downstream is checked, which is what
    // separates "the pointer was not over the row" from "it was, and the click did nothing".
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

    // 6. **And the top bar still works.** The modal used to be opened once and never closed,
    //    so it stayed on top of whatever was launched and the bar's click handler — gated on
    //    there being no modal — was inert for the rest of the session: no second launch, no
    //    way back. Clicking again is the direct test; asserting the close alone would be a
    //    proxy for it (PR #237 review, finding 6).
    // **Escape first, so this step's precondition is stated rather than assumed.** `click_at`
    // retries a press that did not land where it was aimed, and an abandoned attempt still
    // *pressed* somewhere — if that somewhere is the Applications word (a mis-walked pointer
    // parks at x=0) the menu is already open, and the aimed click then dismisses it and opens it
    // again: the line below matches either way, and would be asserting less than it says. Escape
    // closes a menu left open, and with none open it reaches the focused terminal and does
    // nothing.
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

/// Whether the file browser started at `path` and **then** listed it.
///
/// **Line by line, and the listing only after the start.** One browser prints both, in that
/// order, so the order is the claim; a listing of the same directory from before the start — which
/// `check-login` has, from the sidebar step — says nothing about this browser. Exact on the start
/// line, so `/home/Documents/sub` is not `/home/Documents`. Another browser listing the same
/// directory in between would satisfy it; none is driven while step 13 runs.
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
/// **An absence, which `expect` cannot express.** The two logins succeeding in sequence does
/// not by itself prove they overlapped: a graphical session that ended the moment the serial
/// one began would produce the same ordered lines. What distinguishes them is that
/// `desktop-session-mgr` never reported a session ending — its leader blocks, so the only way
/// that line appears is if the session came down.
pub(crate) fn check_two_sessions(transcript: &str) -> R<()> {
    // **The terminal opened and hosted a shell**, which is what makes M7 visible: a person
    // typed in the Applications menu and got a terminal. It found a font, a terminal and
    // `/bin` in the namespace the shell built for it — none of which an application namespace
    // had before Part F.
    //
    // Checked against the transcript rather than with an `expect`, because `nxterm` runs
    // concurrently with the shell and ordering the two would be a race. **Not the grid line**:
    // `nxterm::report_row` is `test-harness`-only and this gate boots a release image, so
    // asserting `"nxterm: grid> …"` here could never have passed — which is how the first
    // version of this check failed.
    // **And its shell got an environment — asserted from the shell, not from its parent.**
    // M7 Part F's second claim: `nxterm` took no setup message and handed `nxsh` a
    // `Record::default()`, so a terminal launched into a constructed namespace would give its
    // shell no `$env.HOME` while every serial login's has one.
    //
    // **This reads `nxsh`'s own line, and the first version did not.** It read `nxterm:
    // hosting a shell (env: N)`, which `nxterm` logs from the environment it *received* — so
    // breaking the forward to `nxsh` and leaving the receipt intact kept the gate green. A
    // parent cannot testify to what its child was given; only the child can. Demonstrated in
    // review by doing exactly that (PR #238 review, finding 1).
    //
    // Two shells must report, because this gate runs two sessions: the terminal
    // `desktop-shell` launched, and the serial login beside it. Counting them is what stops an
    // absence-only check from passing when no shell started at all.
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
    // **An empty applications list would satisfy every expect above.** "`/bin` lists " matches
    // "lists 0 programs", and "menu open" says nothing about its contents — so a session
    // whose `/bin` failed to open would pass the whole gate. Asserted as an absence for the
    // same reason the concurrency check is: `expect` cannot say "a number greater than zero".
    // **What the `desktop` command itself reported** — order-independent, because it is a
    // different process from the shell whose lines are asserted above. Both halves matter:
    // without the shell's lines a command could have invented these, and without these a shell
    // could have served a reply nobody received.
    // **`(table)` rather than a bare "listed"**: the listing is the one data product here, and
    // it goes to stdout as TSM1 like every other coreutil's. It went to *stderr* until PR #245's
    // review — where `desktop | sort name` produced nothing and the rows interleaved with every
    // stage's diagnostics on the shared sink — and a gate reading a release image can see the
    // bytes nowhere, so the command names the branch it took. Match the name, not just the count.
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

    // **The configure deadline must not fire, and this is what would have caught the deadlock.**
    // `no manager answer for window N; showing it` exists to name a wedged or absent manager —
    // "a wedged or slow shell must delay a window, never lose it". Part C briefly created the
    // bottom bar *after* taking the manager channel, so the shell parked inside `create` waiting
    // for a `Configure` only it could release, and this line fired on every healthy boot. A
    // signal that fires when nothing is wrong is not a signal (PR #242 review, blocking 1).
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

/// Move the browser's selection from a fresh `/home` listing down to `papers`.
///
/// **Because row 0 stopped being `papers`.** M14 Part D stages `Documents`, `Downloads` and
/// `Pictures` into the demo home for the sidebar to point at, and directories lead the listing —
/// so a fresh listing selects `Documents` and the two steps that pressed Enter on it descended
/// into the wrong directory. The old comment predicted precisely this and said it would fail
/// loudly, which it did.
///
/// Each arrow is acknowledged by the browser's `selected` receipt, which is both a per-keystroke
/// pace and the assertion that the arrow moved where this thinks it did.
pub(crate) fn select_papers(qmp: &mut Qmp, session: &mut Session) -> R<()> {
    for name in ["Downloads", "Pictures", "papers"] {
        press(qmp, "down")?;
        session.expect(&format!("nxfiles: selected {name}"))?;
    }
    Ok(())
}

/// Wait for `pat` after injecting pointer input, **flushing QEMU's mouse queue while waiting**.
///
/// **QEMU holds a packet it cannot fit rather than dropping it.** Its PS/2 queue is sixteen bytes,
/// and `ps2_mouse_send_packet` returns without queuing when a packet will not fit, leaving the
/// buttons and motion in the device's state (`hw/input/ps2.c`, the same in 8.2 and 11.0). They go
/// out at the device's next sync — and only an injected event causes one. So a gate that injects a
/// release last and then waits can wait for ever: the guest drains the queue, but nothing asks QEMU
/// to send what it held. That was the whole cause of the `lost-release` deferral, reproduced by holding the
/// guest's drain for a walk and a click (2026-09-23): ten releases held out of ten, each delivered
/// by one event that moved nothing.
///
/// So each second without `pat` sends [`Qmp::flush`], which delivers only what was already
/// injected — and a later flush delivers what an earlier one had no room for. Forty-five seconds
/// in all, as [`Session::expect`].
///
/// **A press held together with walk motion still lands short**: QEMU packs both into one packet
/// and the guest's decoder emits the button before the motion. What this recovers is a button
/// held on its own, which is the case a gate's last event is (PR #331 review, optional 2).
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
