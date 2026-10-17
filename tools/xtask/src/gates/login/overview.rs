//! `check-login`'s window list, overview, and windows dragged, resized and snapped.

use crate::*;

/// `check-login`'s steps 6b to 6e: the window list, show desktop, the lifecycle rule, the
/// switcher, the overview and `/dev/desktop`.
pub(crate) fn login_window_list_and_overview(
    qmp: &mut Qmp,
    session: &mut Session,
    size: DisplaySize,
    st: &mut LoginState,
) -> R<()> {
    let list_click = st.list_click;
    let term_h = st.term_h;
    let term_w = st.term_w;
    let term_x = st.term_x;
    let term_y = st.term_y;
    let wallpaper_id = st.wallpaper_id;

    // Close the menu first **by clicking outside it**, onto the bottom bar's dead space: clicking
    // another *window* raises it and a popup hears the focus change, which let an earlier version
    // pass while the bug survived. A press on a panel raises nothing, so the compositor sends
    // `Surface::Dismissed`.
    click_at(qmp, session, 600, size.bottom_bar_click_y())?;
    session.expect("desktop-shell: applications menu closed")?;
    // That the compositor *said* so is checked against the whole transcript below: the dismissal is
    // logged before the `press at` line, which `click_at` has already scanned past.

    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: minimized window ")?;
    session.expect("desktop-shell: window list on ")?;
    session.expect(":_ nxterm")?;

    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;
    session.expect(":> nxterm")?;

    // 6b2. **Show desktop, and back.** One window here, so the counts say the press put something
    //      away and brought it back; that it restores *exactly* that is
    //      `desktop_shell::ShownDesktop`'s host tests.
    let show = size.show_desktop_click();
    click_at(qmp, session, show.0, show.1)?;
    session.expect("desktop-shell: showing the desktop, minimised 1 window(s)")?;
    session.expect("desktop-shell: window list on ")?;
    session.expect(":_ nxterm")?;
    click_at(qmp, session, show.0, show.1)?;
    session.expect("desktop-shell: restored 1 window(s)")?;
    session.expect(":> nxterm")?;
    println!("  ok: show-desktop put the window away and brought it back");

    chord(qmp, false, "h")?;
    session.expect("desktop-shell: Super+H minimized window ")?;

    // 6c. **The lifecycle rule**: an **unnamed** empty desktop is removed, a **named** one kept.
    //     No gate can close the terminal inside a session, so the last window is **moved** away.
    //     **Restore the terminal first**: a minimized window is not focused, so `Super+Shift+N`
    //     would find nothing to move.
    click_at(qmp, session, list_click.0, list_click.1)?;
    session.expect("desktop-shell: raised window ")?;
    session.expect(":> nxterm")?;

    chord(qmp, false, "r")?;
    session.expect("desktop-shell: naming this desktop")?;
    // **`Super+A` closes this too, and says which popup it closed**: one window serves both, so a
    // dismissal read as the launcher's would hide a line under the other name.
    chord(qmp, false, "a")?;
    session.expect("desktop-shell: name prompt closed")?;
    chord(qmp, false, "r")?;
    session.expect("desktop-shell: naming this desktop")?;
    let mut typed = String::new();
    for c in "work".chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        typed.push(c);
        session.expect(&format!("desktop-shell: name so far {typed}"))?;
    }
    press(qmp, "ret")?;
    session.expect("desktop-shell: named this desktop work")?;
    session.expect("desktop-shell: window list on work of 2")?;

    // 6c2. **The switcher**: the name beside the arrows moves them, so
    //      `desktop_shell::panel`'s test pins the aims with `work` and `Desktop 2`.
    const NEXT_FROM_RIGHT: i32 = 67;
    const FIRST_CELL_FROM_RIGHT: i32 = 148;
    let bar_y = size.bottom_bar_click_y();
    click_at(qmp, session, size.w as i32 - NEXT_FROM_RIGHT, bar_y)?;
    session.expect("desktop-shell: switched to Desktop 2")?;
    session.expect("desktop-shell: window list on Desktop 2 of 2 (empty)")?;
    click_at(qmp, session, size.w as i32 - FIRST_CELL_FROM_RIGHT, bar_y)?;
    session.expect("desktop-shell: switched to work")?;
    // **The terminal has the keyboard again, because it is on the desktop the compositor is
    // showing**: a shell re-pointed off a removed desktop without telling the compositor.
    session.expect("desktop-shell: window list on work of 2 [")?;
    session.expect(":> nxterm")?;
    println!("  ok: the switcher's arrow and its cell each switched desktops, and back to the window");

    chord(qmp, true, "2")?;
    session.expect("desktop-shell: moved window ")?;
    session.expect("desktop-shell: window list on work of 3 (empty)")?;

    chord(qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;
    session.expect(":> nxterm")?;

    // The desktop vacated is **unnamed**, so it goes. **The bar does not follow the window**, so
    // the reading is "desktop 2 of 2, empty".
    chord(qmp, true, "1")?;
    session.expect("desktop-shell: moved window ")?;
    session.expect("desktop-shell: window list on Desktop 2 of 2 (empty)")?;

    chord(qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    // **The compositor's line first, and first in the guest too**: the shell captures every window
    // *before* creating the overview, and an overview with no thumbnails would satisfy every later
    // assertion, since the drag is by coordinate.
    session.expect("compositor: captured window ")?;
    session.expect("desktop-shell: thumbnail of window ")?;
    session.expect("desktop-shell: overview open, window ")?;
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;

    // Press in the terminal's box in the **current** desktop's card, release over the second.
    // **Aimed from the window's real geometry**, which moves with the cascade.
    let thumb = size.window_in_card(0, cards, (term_x, term_y, term_w, term_h));
    // **The drag starts from a position already verified** by the click that opened this: a drag
    // cannot check its own start, and no second `click_at` has an inert place to land.
    move_pointer_to(qmp, thumb.0, thumb.1)?;
    qmp.pointer = Some(thumb);
    qmp.send_button("left", true)?;
    expect_after_pointer(qmp, session, "desktop-shell: dragging window ")?;
    let (dx, dy) = size.card_click(1, cards);
    move_pointer_to(qmp, dx, dy)?;
    qmp.pointer = Some((dx, dy));
    qmp.send_button("left", false)?;
    expect_after_pointer(qmp, session, "desktop-shell: dropped window ")?;
    session.expect("desktop-shell: overview closed")?;

    session.expect("desktop-shell: window list on work of 3 (empty)")?;

    // **A window clicked in a card that is not the current desktop's** once raised a window on a
    // desktop nobody was looking at and left the screen unchanged. The switch is asserted *before*
    // the raise, the shell's order.
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;
    let away = size.window_in_card(1, cards, (term_x, term_y, term_w, term_h));
    click_at(qmp, session, away.0, away.1)?;
    session.expect("desktop-shell: switched to Desktop 2")?;
    session.expect("desktop-shell: overview raised window ")?;
    session.expect("desktop-shell: overview closed")?;
    println!("  ok: a window in another desktop's card went to that desktop and raised it");

    chord(qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    chord(qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;
    session.expect(":> nxterm")?;

    // 6e. **`/dev/desktop` and its first consumer**: a `/bin` command resolving a path bound into a
    //     namespace `desktop-shell` constructed. The command also says what it did on the debug
    //     console, since the terminal's grid renders under `test-harness` only.
    // **Click inside the terminal first**: raising it gives the *window* the keyboard, but the grid
    // *widget* is focused by `libui`'s router on a press.
    click_at(qmp, session, 200, 200)?;
    type_at_terminal(qmp, "desktop")?;
    // **Only the shell's own lines are ordered here**: the command is its own process, so its
    // output races the shell's next line and is checked against the whole transcript below.
    session.expect("desktop-shell: served List of ")?;

    // A mutation, which a read-only op cannot prove, on **the second desktop**, the one showing.
    type_at_terminal(qmp, "desktop name 2 cli")?;
    session.expect("desktop-shell: served Name cli")?;
    session.expect("desktop-shell: window list on cli of ")?;

    // **And the chrome is still there**: both bars were stamped with the desktop current at
    // startup, and `visible_on` is the one predicate behind compositing, focus and hit-testing.
    // Asserted by the compositor naming the window a press landed on (`win=none` if not sticky).
    // Escape closes the overview so the serial login is not typing at a popup.
    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    // **Before the "overview open" line, because that is where it is.** **The line, not the
    // pixels**: a release boot has no reference render for the wallpaper kept in the overview.
    session.expect("desktop-shell: overview ground is the wallpaper")?;
    println!("  ok: the overview kept the desktop's picture rather than covering it");
    session.expect("desktop-shell: overview open, window ")?;
    press(qmp, "esc")?;
    session.expect("desktop-shell: overview closed")?;

    // **And so is the wallpaper**, **asserted on an empty desktop**, so every point that is not a
    // bar is the wallpaper or nothing: a press names its window when sticky, `win=none` when not.
    chord(qmp, false, "3")?;
    session.expect("desktop-shell: switched to ")?;
    session.expect("(empty)")?;
    let hit = click_at(qmp, session, size.centre().0, size.centre().1)?;
    if !hit.contains(&format!("win={wallpaper_id}")) {
        return Err(format!(
            "a press on an empty desktop reported '{}' — the wallpaper (window {wallpaper_id}) \
             is not sticky, so every desktop but the first shows the bare ground colour",
            hit.trim()
        )
        .into());
    }
    println!("  ok: the wallpaper is sticky — still there on an empty desktop");
    chord(qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;

    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;

    chord(qmp, false, "1")?;
    session.expect("desktop-shell: switched to work")?;
    session.expect("desktop-shell: overview now showing 0 on work")?;

    // Card 1 is `cli`. **Aimed at the foot of the card**: a click on a window means "raise it", not
    // "go to that desktop".
    let (sx, sy) = size.card_click(1, cards);
    click_at(qmp, session, sx, sy)?;
    session.expect("desktop-shell: switched to cli")?;
    session.expect("desktop-shell: overview now showing 1 on cli")?;

    click_at(qmp, session, sx, sy)?;
    session.expect("desktop-shell: overview closed")?;

    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    click_at(qmp, session, 600, 700)?;
    session.expect("desktop-shell: overview closed")?;

    click_at(qmp, session, size.desktop_name_click().0, size.desktop_name_click().1)?;
    session.expect("desktop-shell: overview open, window ")?;
    let open_line = session.rest_of_line()?;
    let cards = parse_overview_line(&open_line)
        .ok_or_else(|| format!("could not read the overview's card count from {open_line:?}"))?;
    let on_term = size.window_in_card(1, cards, (term_x, term_y, term_w, term_h));
    click_at(qmp, session, on_term.0, on_term.1)?;
    session.expect("desktop-shell: overview raised window ")?;
    session.expect("desktop-shell: overview closed")?;

    Ok(())
}

/// `check-login`'s steps 6f to 6k and the second session: the overview's clicks, dragging,
/// resizing and snapping windows.
pub(crate) fn login_overview_drags(qmp: &mut Qmp, session: &mut Session, st: &mut LoginState) -> R<()> {
    let term_id = st.term_id;
    let term_w = st.term_w;
    let term_x = st.term_x;
    let term_y = st.term_y;
    let work = st.work;
    // 6g. **A window is dragged by its own title bar.** **The motion is injected before the request
    //     can arrive, and that is the assertion**: `StartMove` is a round trip after the press, and
    //     a compositor taking its offset from the pointer *at the request* would lose what the
    //     pointer did meanwhile. Every other step waits between injections; this one deliberately
    //     does not. The race is won by a wide margin, not by construction;
    //     `an_interactive_move_offsets_by_where_the_press_landed…` is the guard that cannot drift.
    const DRAG_STEPS: i32 = 4;
    const DRAG_DX: i32 = 10;
    const DRAG_DY: i32 = 5;
    let press_at = (100, term_y + chrome::TITLE_Y);
    move_pointer_to(qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    for _ in 0..DRAG_STEPS / 2 {
        qmp.send_motion(DRAG_DX, DRAG_DY)?;
    }
    // **The button stays down until the drag is accepted**: a release landing first takes the grab
    // away.
    session.expect("compositor: interactive move of window ")?;
    for _ in 0..DRAG_STEPS / 2 {
        qmp.send_motion(DRAG_DX, DRAG_DY)?;
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some((press_at.0 + DRAG_STEPS * DRAG_DX, press_at.1 + DRAG_STEPS * DRAG_DY));
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "desktop-shell: window {term_id} geometry {},{} ",
            term_x + DRAG_STEPS * DRAG_DX,
            term_y + DRAG_STEPS * DRAG_DY
        ),
    )?;
    println!("  ok: the terminal moved with its own title bar, offset by where it was grabbed");

    // **Maximising now *moves* it**: a maximise carrying only a size would hang the window off the
    // screen. **The size is asserted too**; a client that still declined would produce every line
    // but the last.
    let moved_x = term_x + DRAG_STEPS * DRAG_DX;
    let moved_y = term_y + DRAG_STEPS * DRAG_DY;
    click_at(qmp, session, moved_x + term_w as i32 - chrome::MAXIMISE_X, moved_y + chrome::TITLE_Y)?;
    // **Two producers, one cause, so no order**: the shell's origin and the client's size are both
    // downstream of the same apply, so either reaches the console first. **The request and the
    // shell's answer in the same wait**: `nxterm` logs the request after the compositor answers, so
    // the shell's maximise *and its move* can precede it. The client's line says what it came to in
    // cells: a grid still 80x24 would satisfy the rest.
    let maximised = format!("desktop-shell: maximize window {term_id} to {},{} {}x{}", work.0, work.1, work.2, work.3);
    let moved = format!("desktop-shell: window {term_id} geometry {},{} ", work.0, work.1);
    let took = format!("nxterm: resized to {}x{}, grid ", work.2, work.3);
    session.expect_all(&["nxterm: asked the shell for window state 2", &maximised, &moved, &took])?;
    session.expect(&format!("desktop-shell: window {term_id} geometry {},{} {}x{}", work.0, work.1, work.2, work.3))?;
    println!("  ok: maximise moved the window and the client committed the work area exactly");

    // 6j. **The window is dragged smaller by its own corner.** The compositor moves an outline and
    //     hands the shell one rectangle at the release. **Maximised here**, so dragging inward
    //     keeps every motion inside the screen. **Four assertions, in the order the mechanism
    //     goes**; the last, the client's commit, is what an outline that tracks and a release that
    //     commits nothing fails.
    const RESIZE_STEPS: i32 = 4;
    const RESIZE_DX: i32 = -50;
    const RESIZE_DY: i32 = -25;
    let resized_w = (work.2 as i32 + RESIZE_STEPS * RESIZE_DX) as u32;
    let resized_h = (work.3 as i32 + RESIZE_STEPS * RESIZE_DY) as u32;
    let grip_at = (work.0 + work.2 as i32 - 8, work.1 + work.3 as i32 - 8);
    move_pointer_to(qmp, grip_at.0, grip_at.1)?;
    qmp.pointer = Some(grip_at);
    qmp.send_button("left", true)?;
    expect_after_pointer(qmp, session, &format!("compositor: interactive resize of window {term_id} edges 10"))?;
    for _ in 0..RESIZE_STEPS {
        qmp.send_motion(RESIZE_DX, RESIZE_DY)?;
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some((grip_at.0 + RESIZE_STEPS * RESIZE_DX, grip_at.1 + RESIZE_STEPS * RESIZE_DY));
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "compositor: interactive resize of window {term_id} ended at {},{} {resized_w}x{resized_h}",
            work.0, work.1
        ),
    )?;
    session
        .expect(&format!("desktop-shell: drop window {term_id} to {},{} {resized_w}x{resized_h}", work.0, work.1))?;
    session.expect(&format!("nxterm: resized to {resized_w}x{resized_h}, grid "))?;
    session
        .expect(&format!("desktop-shell: window {term_id} geometry {},{} {resized_w}x{resized_h}", work.0, work.1))?;
    println!("  ok: the terminal was dragged smaller by its corner, and committed it");

    // 6k. **A window thrown at the left edge snaps to half the work area.** **Asserted against the
    //     work area, not the screen**: a table from `screen_h` would put the window under the bars.
    // **First the control**: a drag through a zone and released outside must snap nothing, and the
    // ordinary move's geometry catches a compositor that snapped on entry. Asserting the absence
    // of a snap would not: the next step produces that line, and an `expect` scans forward.
    let press_at = (100, work.1 + chrome::TITLE_Y);
    move_pointer_to(qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    expect_after_pointer(qmp, session, "compositor: interactive move of window ")?;
    let mut walked = (0, 0);
    for (dx, dy) in [(-30, 60), (-30, 60), (-30, 60), (66, 0), (66, 0), (66, 0)] {
        qmp.send_motion(dx, dy)?;
        walked = (walked.0 + dx, walked.1 + dy);
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    qmp.send_button("left", false)?;
    let passed_to = (press_at.0 + walked.0, press_at.1 + walked.1);
    qmp.pointer = Some(passed_to);
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "desktop-shell: window {term_id} geometry {},{} {}x{}",
            work.0 + walked.0,
            work.1 + walked.1,
            resized_w,
            resized_h
        ),
    )?;
    println!("  ok: a drag through the zone and out again moved the window and snapped nothing");

    // **The press is asserted to land on *that* window**, which makes the step above a control: a
    // compositor that snapped on entry has moved the terminal, and the press lands on nothing.
    let press_at = (passed_to.0 + 90, passed_to.1);
    move_pointer_to(qmp, press_at.0, press_at.1)?;
    qmp.pointer = Some(press_at);
    qmp.send_button("left", true)?;
    expect_after_pointer(
        qmp,
        session,
        &format!("compositor: press at x={} y={} win={term_id}", press_at.0, press_at.1),
    )?;
    // **On its title bar specifically**: at the work area's origin the press would land on the
    // *grid* and no drag begins. In either order.
    session.expect_all(&["nxterm: dragging its own title bar", "compositor: interactive move of window "])?;
    // **Paced**: thirty-six motions at QMP speed overran the consumer ring and `libinput` turned
    // the gap into a `Logical::Dropped`, ending the gesture. **Down as well as left**: the
    // *pointer* picks the zone, and a straight drag left snaps a quarter, saying nothing about
    // edges.
    let step = -(press_at.0 - 10) / 6;
    for _ in 0..6 {
        qmp.send_motion(step, 30)?;
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some((press_at.0 + step * 6, press_at.1 + 180));
    expect_after_pointer(
        qmp,
        session,
        &format!("desktop-shell: drop window {term_id} to {},{} {}x{}", work.0, work.1, work.2 / 2, work.3),
    )?;
    session.expect(&format!("nxterm: resized to {}x{}, grid ", work.2 / 2, work.3))?;
    session.expect(&format!(
        "desktop-shell: window {term_id} geometry {},{} {}x{}",
        work.0,
        work.1,
        work.2 / 2,
        work.3
    ))?;
    println!("  ok: dropped at the left edge, the terminal took half the work area");

    // 6h. **Movement is not lost while the compositor is busy**: a relative pointer's deltas *are*
    //     its position, so a dropped batch offsets the guest's cursor for good. Injected **against
    //     a desktop switch**, the repaint that costs the whole screen. **Under TCG only, so not the
    //     guard**: under KVM the ring never fills and the step passes either way; `check-input`
    //     gates the property.
    burst_holds_its_position(qmp, session, &chord)?;

    // 6i. **The close button is the client's own, and sends nothing.** **After 6h**: its stall is a
    //     whole-screen recompose. **Asserted by what is absent as well as present**: the transcript
    //     check below says no `RequestClose` was sent for *this* window (the two terminals have
    //     different ids so it can name one). Measured from the snapped width: the work area's
    //     midpoint.
    chord(qmp, false, "2")?;
    session.expect("desktop-shell: switched to ")?;
    click_at(qmp, session, work.0 + (work.2 / 2) as i32 - chrome::CLOSE_X, work.1 + chrome::TITLE_Y)?;
    session.expect("nxterm: closing")?;
    session.expect("desktop-shell: window list on ")?;
    println!("  ok: the close button closed the terminal with no request to the shell");

    // 4. **Two independent sessions**: the graphical leader blocks, so it is running now, and
    //    logging in at the serial prompt proves the two supervisors do not contend.
    session.send(DEMO_USER)?;
    session.expect("password:")?;
    session.send(DEMO_PASSWORD)?;
    session.expect("/home>")?;

    Ok(())
}
