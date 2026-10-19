//! `check-login`'s dialogs, a close insisted on, and the Places menu.

use crate::*;

/// `check-login`'s steps 10 to 13 and the transcript's last checks: dialogs, insisting on a
/// close, and the Places menu.
pub(crate) fn login_dialogs_and_places(
    mut qmp: Qmp,
    mut session: Session,
    size: DisplaySize,
    st: &mut LoginState,
) -> R<()> {
    let edit_id = st.edit_id;
    let first_term_id = st.first_term_id;
    let qmp_sock = st.qmp_sock.clone();
    let term_id = st.term_id;
    let work = st.work;
    press(&mut qmp, "esc")?;
    session.skip_to_end()?;
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(&mut qmp, &mut session, "nxedit")?;
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: launched nxedit into its own namespace")?;
    session.expect("desktop-shell: placed window ")?;
    let untitled = session.rest_of_line()?;
    let (untitled_id, untitled_x, untitled_y) = parse_placement(&untitled)
        .ok_or_else(|| format!("could not read the untitled editor's placement from {untitled:?}"))?;
    println!("  ok: the editor launched from the menu and stayed up (window {untitled_id})");

    // **Wait for it to reach the screen before typing**: "placed" does not say the compositor has
    // moved the keyboard to it.
    let _ = settle_and_capture(&mut qmp, &build_cache().join("check-login.ppm"))?;

    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    for (i, c) in "scratch".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(&mut qmp, &qcode)?;
        session.expect(&format!("nxedit: name so far {} chars", i + 1))?;
    }
    press(&mut qmp, "ret")?;
    session.expect("nxedit: saved /home/scratch - 0 bytes")?;
    println!("  ok: an untitled buffer was named and written into the session's home");

    // 11. **A confirmation dialog, driven to both answers.** **The geometry is hardcoded here**: a
    //     gate that aimed from the toolkit could agree with a toolkit that had stopped drawing
    //     where it says. `libui::widget::tests::dialog_buttons_land_where_the_constants_say` pins
    //     the `DIALOG_*` constants to a built tree.
    const CONFIRM_W: i32 = chrome::DIALOG_W;
    const CONFIRM_H: i32 = chrome::DIALOG_H;
    const CONFIRM_DISCARD_CX: i32 = chrome::DIALOG_LEFT_CX;
    const CONFIRM_KEEP_CX: i32 = chrome::DIALOG_RIGHT_CX;
    const CONFIRM_BUTTON_CY: i32 = chrome::DIALOG_BUTTON_CY;
    // **And the editor's own size, `nxedit::START_SIZE`**: the geometry line cannot be read this
    // late (`MAX_LOGGED_GEOMETRY`), and nothing has resized this window.
    const EDITOR_W: i32 = 560;
    const EDITOR_H: i32 = 420;

    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;

    let untitled_right = untitled_x + EDITOR_W;
    let close_at = (untitled_right - chrome::CLOSE_X, untitled_y + chrome::TITLE_Y);
    click_at(&mut qmp, &mut session, close_at.0, close_at.1)?;
    // **The editor's line comes first, ordered by construction**: it is printed *before* the dialog
    // is asked for, since a dialog's first `Configure` is held for the manager; a line printed
    // after `Child::open` would race the shell, and TCG and KVM picked different winners.
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let placed_dialog = session.rest_of_line()?;
    let (_, parent_id, dx, dy, dw, dh) = parse_dialog_placement(&placed_dialog)
        .ok_or_else(|| format!("could not read a dialog placement from {placed_dialog:?}"))?;
    if parent_id != untitled_id {
        return Err(format!(
            "the editor asked about window {untitled_id}, and the shell placed a dialog on \
             window {parent_id}"
        )
        .into());
    }
    if (dw as i32, dh as i32) != (CONFIRM_W, CONFIRM_H) {
        return Err(format!(
            "the dialog is {dw}x{dh}, and this gate aims at a {CONFIRM_W}x{CONFIRM_H} one \
             — the four constants above came from `nxedit`'s published geometry and have \
             drifted from it"
        )
        .into());
    }
    let want_x = (untitled_x + (EDITOR_W - CONFIRM_W) / 2).clamp(work.0, work.0 + work.2 as i32 - CONFIRM_W);
    let want_y = (untitled_y + (EDITOR_H - CONFIRM_H) / 2).clamp(work.1, work.1 + work.3 as i32 - CONFIRM_H);
    if (dx, dy) != (want_x, want_y) {
        return Err(format!(
            "the dialog landed at {dx},{dy}; centred on window {untitled_id} at \
             {untitled_x},{untitled_y} {EDITOR_W}x{EDITOR_H} and clamped to the work area \
             it belongs at {want_x},{want_y}"
        )
        .into());
    }
    println!("  ok: an unsaved buffer asked instead of exiting, in a dialog centred on it");

    click_at(&mut qmp, &mut session, dx + CONFIRM_KEEP_CX, dy + CONFIRM_BUTTON_CY)?;
    session.expect("nxedit: close cancelled, still editing")?;
    println!("  ok: `keep editing` dismissed the question and kept the editor");

    click_at(&mut qmp, &mut session, close_at.0, close_at.1)?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let placed_again = session.rest_of_line()?;
    let (_, _, dx2, dy2, _, _) = parse_dialog_placement(&placed_again)
        .ok_or_else(|| format!("could not read the second dialog placement from {placed_again:?}"))?;
    let from = session.transcript().len();
    click_at(&mut qmp, &mut session, dx2 + CONFIRM_DISCARD_CX, dy2 + CONFIRM_BUTTON_CY)?;
    session.expect("nxedit: discarding the unsaved buffer")?;
    session.expect("nxedit: closing")?;
    // **The list that reflects the destroy, not merely the next one**: the dialog goes first, which
    // marks the list dirty with the editor still in it. **Read from the click, not the receipt**:
    // `nxedit` destroys its window *before* it says it is closing.
    let after_close = session.line_since(
        from,
        "desktop-shell: window list on ",
        |rest| taskbar_slot(rest, untitled_id).is_none(),
        std::time::Duration::from_secs(45),
    )?;
    let slot = taskbar_slot(&after_close, edit_id).ok_or_else(|| {
        format!("window {edit_id} is not in the taskbar list {after_close:?}, so it cannot be clicked")
    })?;
    println!("  ok: `discard` was the only thing that ended the run");

    // 12. **Insisting is a second click, not a clock**: a timer would destroy a buffer the person
    //     at the dialog could still save.
    let doc = (work.0 + work.2 as i32 * 3 / 4, work.1 + work.3 as i32 / 2);
    click_at(&mut qmp, &mut session, doc.0, doc.1)?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;

    let entry = size.task_click(slot);

    middle_click_at(&mut qmp, &mut session, entry.0, entry.1)?;
    session.expect(&format!("compositor: asked window {edit_id} to close"))?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let asked_dialog = session.rest_of_line()?;
    let (_, _, ax, ay, _, _) = parse_dialog_placement(&asked_dialog)
        .ok_or_else(|| format!("could not read the ask's dialog placement from {asked_dialog:?}"))?;
    println!("  ok: the taskbar's ask reached a client that declined to answer it");

    // **The arming must not outlive the answer**: if *keep editing* left it armed, a later
    // middle-click would destroy the window with no question.
    click_at(&mut qmp, &mut session, ax + CONFIRM_KEEP_CX, ay + CONFIRM_BUTTON_CY)?;
    session.expect("nxedit: close cancelled, still editing")?;

    // **A real wait**: expiry is the *absence* of a state, so the clock must pass
    // `INSIST_WINDOW_NS` (five seconds).
    std::thread::sleep(std::time::Duration::from_secs(6));
    middle_click_at(&mut qmp, &mut session, entry.0, entry.1)?;
    // **Asked, not destroyed** is the control: an entry left armed answers this click with "did not
    // answer; closed it".
    session.expect(&format!("compositor: asked window {edit_id} to close"))?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    println!("  ok: the arming expired, so a later click asked again rather than destroying");

    middle_click_at(&mut qmp, &mut session, entry.0, entry.1)?;
    session.expect(&format!("desktop-shell: window {edit_id} did not answer; closed it"))?;
    println!("  ok: a second middle-click destroyed the window the first one only asked about");

    // 13. **The Places menu**, **last**, as it leaves a browser on screen; the aims are pinned by
    //     `desktop_shell::panel`'s test at both text sizes.
    const PLACES_CLICK: (i32, i32) = (141, 12);
    const PLACE_DOCUMENTS: (i32, i32) = (180, 75);
    click_at(&mut qmp, &mut session, PLACES_CLICK.0, PLACES_CLICK.1)?;
    session.expect("desktop-shell: places menu open")?;
    click_at(&mut qmp, &mut session, PLACE_DOCUMENTS.0, PLACE_DOCUMENTS.1)?;
    session.expect("desktop-shell: opening place Documents")?;
    session.expect("desktop-shell: launched nxfiles into its own namespace")?;
    session.expect("desktop-shell: places menu closed")?;
    session.expect("desktop-shell: placed window ")?;
    println!("  ok: a place opened the file browser");

    let transcript = session.finish();
    let _ = fs::remove_file(&qmp_sock);
    // **Written on the way past, whichever way this ends**: a stale transcript from an earlier
    // failing run reads as evidence.
    let _ = fs::write(build_cache().join("guest-transcript-check-login.log"), &transcript);

    // **The grid grew, in cells**, read off the client's own line: a grid still at its start size
    // in a large window satisfies every pixel assertion. **The reflow**: a rewrap does not create
    // or destroy lines, so the count either side of every resize must agree; ignoring the soft-wrap
    // flag collapses them to one line. `libterm`'s own test has the row-level control.
    let reflows = transcript_reflows(&transcript);
    if reflows.is_empty() {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err("no \"nxterm: resized to …, lines N->M\" line in the transcript: the \
                    terminal never accepted a `Configure`"
            .into());
    }
    for &(before, after, evicted) in &reflows {
        // **The eviction is subtracted, not tolerated**: narrowing near `SCROLLBACK` loses the
        // oldest lines to the ring. It is 0 here.
        if after + evicted != before {
            let path = build_cache().join("guest-transcript-check-login.log");
            let _ = fs::write(&path, &transcript);
            return Err(format!(
                "a resize turned {before} logical lines into {after}, with {evicted} evicted by \
                 the bounded scrollback — so {} went missing. Re-wrapping moves where the \
                 breaks are and must not join lines that were never one: `Line::wrapped` is \
                 what separates a soft wrap from a line that ended, and a rewrap that ignores \
                 it merges paragraphs the first time a window widens",
                before as i64 - after as i64 - evicted as i64
            )
            .into());
        }
    }
    println!("  ok: {} reflow(s) kept every line a line", reflows.len());

    let Some(start) = transcript_start_grid(&transcript) else {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err("`nxterm: started, grid ` was not seen: without the grid the terminal started with, there \
                    is nothing to say the maximise grew"
            .into());
    };
    match transcript_grid(&transcript) {
        Some((c, r)) if c > start.0 && r > start.1 => {
            println!("  ok: the maximised terminal's grid grew from {}x{} to {c}x{r} cells", start.0, start.1)
        }
        found => {
            let path = build_cache().join("guest-transcript-check-login.log");
            let _ = fs::write(&path, &transcript);
            return Err(format!(
                "the maximised terminal's grid is {found:?}, not bigger than the {}x{} it starts \
                 at, in both columns and rows. `nxterm` must refit the grid to the window it \
                 accepted: a terminal that cannot use the room is the point of the reflow",
                start.0, start.1
            )
            .into());
        }
    }

    // **And that it asked before it insisted**, order-independent: destroying straight away would
    // produce the "did not answer" line and none of this.
    if !transcript.contains(&format!("desktop-shell: asked window {edit_id} to close")) {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the shell destroyed window {edit_id} without ever reporting that it asked first: \
             insisting must be the *second* middle-click, and the first must be a request the \
             client is free to answer with a dialog"
        )
        .into());
    }

    if !transcript.contains(&format!("desktop-shell: asked window {first_term_id} to close")) {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the shell never reported asking window {first_term_id} to close, so the taskbar's \
             middle-click did not reach `Manage::RequestClose`"
        )
        .into());
    }

    // **The menu was dismissed by the compositor, not by a focus change**: clicking another
    // *window* raises it and a popup hears that, which let an earlier version pass while the bug
    // survived. A press on a panel raises nothing.
    if !transcript.contains("compositor: dismissed win=") {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the Applications menu closed, but the compositor never sent a dismissal — so it \
             closed on a focus change, which is the half of this that already worked. A press on \
             a panel raises no window and changes no focus, so nothing but `Surface::Dismissed` \
             can have closed it.\n\nthe transcript is at {}",
            path.display()
        )
        .into());
    }
    if !transcript.contains("nxsh: terminal closed") {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err("the terminal closed and its `nxsh` did not: no \"nxsh: terminal closed\" in \
                    the transcript. A shell whose terminal is gone has nobody to read from and \
                    nobody to print to — it must exit rather than be orphaned per window closed"
            .into());
    }

    if transcript.contains(&format!("asked window {term_id} to close")) {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the close button asked the shell: the transcript contains \"asked window \
             {term_id} to close\". `nxterm`'s close button is its own — it exits, and the \
             compositor tears its windows down with its session. A client that routed its own \
             close through the manager would be asking somebody else for permission to stop"
        )
        .into());
    }
    // **The browser started in the place, and listed it**, the listing searched for only *after*
    // the start: the sidebar step listed `/home/Documents` long before.
    if let Err(why) = started_and_listed(&transcript, "/home/Documents") {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the Places menu launched the file browser, but {why}: `nxfiles` must start in the \
             place it is given as `argv[1]` and list it, not its home.\n\nthe transcript is at {}",
            path.display()
        )
        .into());
    }
    if let Err(e) = check_two_sessions(&transcript) {
        let path = build_cache().join("guest-transcript-check-login.log");
        let saved = fs::write(&path, &transcript).is_ok();
        if saved {
            println!("\nthe full transcript is at {}", path.display());
        }
        return Err(e);
    }
    println!(
        "\nxtask: graphical login gate PASSED — refused a wrong password, ran a session, and a \
         serial login ran beside it ✓"
    );
    Ok(())
}
