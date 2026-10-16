use crate::*;

/// `check-login`'s steps 10 to 13 and the transcript's last checks: dialogs, insisting on a
/// close, and the Places menu.
pub(crate) fn login_dialogs_and_places(mut qmp: Qmp, mut session: Session, size: DisplaySize, st: &mut LoginState) -> R<()> {
    let edit_id = st.edit_id;
    let first_term_id = st.first_term_id;
    let qmp_sock = st.qmp_sock.clone();
    let term_id = st.term_id;
    let work = st.work;
    // 10. **An editor launched from the menu, which is a launch with no file** (M11 Part E
    //     batch 7). `nxedit` required `argv[1]` and the Applications menu passes none, so it
    //     printed "no file to edit" and exited — reported as "nxedit doesn't launch from the
    //     menu", and true in the most literal way. It opens untitled now and asks for a name when
    //     there is something to save.
    press(&mut qmp, "esc")?;
    session.skip_to_end()?;
    click_at(&mut qmp, &mut session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(&mut qmp, &mut session, "nxedit")?;
    // **Enter, not a click on the row.** Clicking a row is proved above, where the terminal every
    // later step depends on is launched that way; what this step is about is what happens *after*
    // a launch that carries no file, so it takes the shortest route to one.
    press(&mut qmp, "ret")?;
    session.expect("desktop-shell: launched nxedit into its own namespace")?;
    // **The placement is the proof**, not the launch: the shell said the same thing before this
    // change, and the editor then exited before it ever created a window. A window that gets
    // placed is a window that exists.
    session.expect("desktop-shell: placed window ")?;
    let untitled = session.rest_of_line()?;
    let (untitled_id, untitled_x, untitled_y) = parse_placement(&untitled)
        .ok_or_else(|| format!("could not read the untitled editor's placement from {untitled:?}"))?;
    println!("  ok: the editor launched from the menu and stayed up (window {untitled_id})");

    // **Wait for it to reach the screen before typing at it.** "Placed" says the shell put the
    // window somewhere, not that the compositor has moved the keyboard to it — the same race that
    // sent a launch's keystrokes into the wrong program in batch 4. Settling the screen is the
    // property that actually has to hold.
    let _ = settle_and_capture(&mut qmp, &build_cache().join("check-login.ppm"))?;

    // Ctrl+S with nothing named asks for a name rather than writing one nobody chose.
    qmp.send_key("ctrl", true)?;
    press(&mut qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    // Then the name, one key at a time — and **waiting for each**, which the comment here
    // claimed and the code did not do (M12 Part A). `nxedit` had no receipt for this field:
    // naming a buffer is not editing it, so `buffer rev` never moves, and seven unacknowledged
    // keystrokes were seven chances to end up asserting against a file called `scrath`.
    for (i, c) in "scratch".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(&mut qmp, &qcode)?;
        session.expect(&format!("nxedit: name so far {} chars", i + 1))?;
    }
    press(&mut qmp, "ret")?;
    // **`/home`, which *is* the user's subtree from inside the session.** A session namespace
    // binds the user's directory at `/home` — that is what scopes it — so an untitled buffer
    // named here lands in the place the session owns, and a path mentioning `alice` would be
    // this gate asserting the host's view of a name the guest cannot see. An empty buffer is
    // zero bytes: the newline the editor adds is per line, and there are none.
    session.expect("nxedit: saved /home/scratch - 0 bytes")?;
    println!("  ok: an untitled buffer was named and written into the session's home");

    // 11. **A confirmation dialog, driven to both answers** (M12 Part A). `Role::Dialog` has
    //     existed since M2 Part A and no program a person runs had ever created one: the editor
    //     answered every `CloseRequested` by exiting, and its own source said so — "an editor
    //     with somewhere to put a question would ask it, and this one has no dialog to ask in".
    //
    //     **The dialog's geometry is hardcoded here**, the way this gate already hardcodes a
    //     title bar's height and a list's row height — not because the crate cannot be linked
    //     (it is, for the display gate's reference frames) but because a gate that *aimed* from
    //     the toolkit could agree with a toolkit that had stopped drawing where it says (M11
    //     decision 2). A host test compares the two sets of numbers; a gate reads only its own. `libui::widget::DIALOG_LEFT_CX` and its siblings are the source — they
    //     were `nxedit`'s until M12 Part B moved them down beside `dialog_frame`, when a second
    //     confirmation would otherwise have given this gate two tables to keep in step — and
    //     `libui::widget::tests::dialog_buttons_land_where_the_constants_say` is the host test
    //     that pins these numbers to a tree that is actually built, so a change to `DIALOG_PAD`
    //     fails there, beside the change, rather than here after a three-minute boot.
    //
    //     (This paragraph named two symbols that same part deleted — PR #268 review, worth
    //     fixing 2 — which is the drift the move was made to prevent, in the one place a
    //     compiler cannot see.)
    const CONFIRM_W: i32 = chrome::DIALOG_W;
    const CONFIRM_H: i32 = chrome::DIALOG_H;
    // **Moved by the desktop refresh's Part B** from (91, 249, 103): the dialog's content runs
    // flush to its border now, three pixels wider each side and three lower. `libui`'s pin moved
    // with them, in the same change.
    const CONFIRM_DISCARD_CX: i32 = chrome::DIALOG_LEFT_CX;
    const CONFIRM_KEEP_CX: i32 = chrome::DIALOG_RIGHT_CX;
    const CONFIRM_BUTTON_CY: i32 = chrome::DIALOG_BUTTON_CY;
    // **And the editor's own size, `nxedit::START_SIZE`, because the geometry line cannot be
    // read this late.** The shell logs at most `MAX_LOGGED_GEOMETRY` of them per session and
    // this run passed that long ago — a bound that exists because the event is client-driven,
    // and a gate is not a reason to raise it. This window was launched from the menu and nothing
    // has resized it, so its size is the one the editor starts at.
    const EDITOR_W: i32 = 560;
    const EDITOR_H: i32 = 420;

    // One keystroke is all it takes to have something to lose. The editor was saved a moment
    // ago, so this is the difference between the buffer and the file and nothing else.
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;

    // **Its own close button, which used to be the end of the process.** The rightmost of the
    // three title-bar controls, measured from the window's right edge — the same arithmetic 6a2
    // uses, and the same one 6i used on `nxterm`, whose close button really does just exit.
    let untitled_right = untitled_x + EDITOR_W;
    let close_at = (untitled_right - chrome::CLOSE_X, untitled_y + chrome::TITLE_Y);
    click_at(&mut qmp, &mut session, close_at.0, close_at.1)?;
    // **The editor's line comes first, and it is ordered by construction rather than by luck.**
    // It is printed *before* the dialog is asked for, because a dialog's first `Configure` is
    // held for the manager — so a line printed after `Child::open` returned would be downstream
    // of the shell, and the two processes would be racing to the console. They were, and the
    // winner depended on the accelerator: TCG gave the shell both lines and KVM gave the client
    // the second one, which is how this passed twice locally and failed in CI (PR #267).
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
    // **The centring is re-derived here, and that is the assertion rather than a second copy of
    // a policy.** M10 Part E's rule — read the shell's geometry lines, do not recompute the
    // cascade — is about placements this gate has no opinion on. This one it does: "centred on
    // its parent, kept inside the work area" is the claim M12 Part A makes, and reading the
    // number back without checking it would assert only that some number was printed.
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

    // **The first answer**, and the one a person reaches for by accident: keep editing. The
    // editor is still there afterwards, which is the whole point of asking.
    click_at(&mut qmp, &mut session, dx + CONFIRM_KEEP_CX, dy + CONFIRM_BUTTON_CY)?;
    session.expect("nxedit: close cancelled, still editing")?;
    println!("  ok: `keep editing` dismissed the question and kept the editor");

    // **And the second answer.** Asked again — the same button, the same window, because a
    // question answered *no* must be askable again or the editor can never be closed at all.
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
    // **The list the destroy produces.** Step 12 needs a slot out of it, and asking for a list line
    // later would wait for a change that nothing is going to make.
    //
    // **The list that reflects the destroy, not merely the next one.** The bar is redrawn for
    // several reasons and two of them fire here in order: the dialog goes first, which hands the
    // keyboard back to the editor and marks the list dirty while the editor is still in it, and
    // only then does the window go. **And read from the click, not from the receipt**: `nxedit`
    // destroys its window *before* it says it is closing, so the shell's list can come first, and
    // waiting for it after the receipt waited for a line already passed (PR #353's CI, where the
    // same order closed the editor's second window).
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

    // 12. **Insisting is a second click, not a clock** (M12 Part A). M9 Part C left this
    //     ungated on the stated grounds that "the release image has no client that can be made
    //     to ignore a request", and named the trigger: *the first application that can be
    //     wedged on purpose*. An editor holding an unanswered question is exactly that — from
    //     the shell's side it is indistinguishable from a client that has stopped listening.
    //
    //     **Which is why the two-second grace period had to go.** Against this client a timer
    //     destroys the window, and the buffer with it, two seconds after one click and with no
    //     way to intervene. The shell cannot tell "wedged" from "asking"; the person looking at
    //     the dialog can, so the second middle-click is what says "I meant it".
    //
    //     Driven on the editor from step 9, which is snapped to the right half and holds
    //     `other.txt`. Clicking its document area both raises it and gives it the keyboard.
    let doc = (work.0 + work.2 as i32 * 3 / 4, work.1 + work.3 as i32 / 2);
    click_at(&mut qmp, &mut session, doc.0, doc.1)?;
    press(&mut qmp, "z")?;
    session.expect("nxedit: buffer rev 1")?;

    // Its taskbar slot came off the shell's own list above: a slot is a *position*, and windows
    // have been closed since the last time anything here counted, so aiming by id would land on
    // somebody else's entry.
    let entry = size.task_click(slot);

    // **`middle_click_at` verifies the pointer with a left click first**, which on a taskbar
    // entry is a gesture in its own right — it raises the window, or minimises it if it already
    // had the keyboard. Neither is asserted here, and neither matters: what the middle click
    // does is the same either way, and a gate that pinned it would be asserting against which
    // window happened to be focused three clicks earlier.
    middle_click_at(&mut qmp, &mut session, entry.0, entry.1)?;
    // **The compositor's line is the ordered one, not the shell's** — the same rule 6a3 states:
    // the compositor logs before it replies, so it leads, and the shell's own line comes after
    // its request returns, which is a race against the client it just woke. The shell's is
    // checked against the whole transcript below, where order does not matter.
    session.expect(&format!("compositor: asked window {edit_id} to close"))?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    session.expect("desktop-shell: placed dialog ")?;
    let asked_dialog = session.rest_of_line()?;
    let (_, _, ax, ay, _, _) = parse_dialog_placement(&asked_dialog)
        .ok_or_else(|| format!("could not read the ask's dialog placement from {asked_dialog:?}"))?;
    println!("  ok: the taskbar's ask reached a client that declined to answer it");

    // **The person answers it, and the arming must not outlive the answer** (PR #267 review,
    // blocking 1). The first version of this policy armed on the ask and disarmed only when the
    // window went away, so *keep editing* left the entry armed for the life of the window and
    // the next middle-click — at any distance in time — destroyed it with no question at all.
    // That is the unbounded version of the very outcome this change replaced a two-second timer
    // to avoid.
    click_at(&mut qmp, &mut session, ax + CONFIRM_KEEP_CX, ay + CONFIRM_BUTTON_CY)?;
    session.expect("nxedit: close cancelled, still editing")?;

    // **A real wait, in a gate that is otherwise expect-driven.** There is no output that says
    // an arming expired — expiry is the *absence* of a state — so the only way to observe it is
    // to let the clock pass it. `INSIST_WINDOW_NS` is five seconds; six is past it with room,
    // and it is the one sleep in this file that is waiting for a rule rather than for a message.
    std::thread::sleep(std::time::Duration::from_secs(6));
    middle_click_at(&mut qmp, &mut session, entry.0, entry.1)?;
    // **Asked, not destroyed** — and this assertion *is* the control. Under the version the
    // review caught, this click produced "did not answer; closed it" and the buffer went with
    // the window; here the question is put again, which is what a click made long after the
    // last one means.
    session.expect(&format!("compositor: asked window {edit_id} to close"))?;
    session.expect("nxedit: unsaved buffer - asking before closing")?;
    println!("  ok: the arming expired, so a later click asked again rather than destroying");

    // **And a click while the ask is still in hand insists.** The window goes, the dialog goes
    // with it — a dialog is destroyed with its parent — and the shell says which path it took.
    middle_click_at(&mut qmp, &mut session, entry.0, entry.1)?;
    session.expect(&format!("desktop-shell: window {edit_id} did not answer; closed it"))?;
    println!("  ok: a second middle-click destroyed the window the first one only asked about");

    // 13. **The Places menu** (desktop refresh, Part C): the top bar's second word, and a place
    //     opened from it is the file browser started *there* — the argument `nxfiles` learned
    //     for this — rather than at its home.
    //
    //     **Last, because it leaves a browser on screen** that nothing after it has to account
    //     for. Both aims are pinned against the bar's and the menu's layout by
    //     `desktop_shell::panel`'s own test at both text sizes, as `APPS_CLICK` and `ROW1` are.
    //     In the staged 12 px the Places word is x 111 to 167 and the menu hangs at (119, 32)
    //     with `Documents` its second row, y 62 to 86; at the built-in 13 they are 116 to 175,
    //     (124, 32) and 64 to 90 — the word moves because `Large` widens `Applications` beside
    //     it. **The margin is asserted rather than described**: the word aim sits at least
    //     `AIM_MARGIN` inside the word at either size, which that test checks, because the
    //     figures written here went a size-step stale the moment the top bar changed and the
    //     sentence claiming a comfortable aim outlived them (PR #316 review, finding 1).
    const PLACES_CLICK: (i32, i32) = (141, 12);
    const PLACE_DOCUMENTS: (i32, i32) = (180, 75);
    click_at(&mut qmp, &mut session, PLACES_CLICK.0, PLACES_CLICK.1)?;
    session.expect("desktop-shell: places menu open")?;
    click_at(&mut qmp, &mut session, PLACE_DOCUMENTS.0, PLACE_DOCUMENTS.1)?;
    // **The shell's own lines, in the order it prints them**: the launch, then the menu going.
    // What the browser did is another process's output and is checked against the whole
    // transcript below — the rule 5 states for `nxterm`.
    session.expect("desktop-shell: opening place Documents")?;
    session.expect("desktop-shell: launched nxfiles into its own namespace")?;
    session.expect("desktop-shell: places menu closed")?;
    // And its window arrived — the receipt that the browser ran far enough to draw, which is
    // what makes the transcript checks below about a browser that started rather than one that
    // might yet.
    session.expect("desktop-shell: placed window ")?;
    println!("  ok: a place opened the file browser");

    let transcript = session.finish();
    let _ = fs::remove_file(&qmp_sock);
    // **Written on the way past, whichever way this ends.** The checks below each wrote it on
    // *their* failure, so a passing run left whatever an earlier failing one had put there —
    // and a stale transcript is worse than none, because it reads as evidence. It cost an
    // afternoon in M12 Part F: a wallpaper line missing from a file three hours old was
    // diagnosed as a broken decoder for twenty minutes before anybody checked the timestamp.
    // The same argument the checks below already make about their own failures, applied to the
    // case nobody thought needed it.
    let _ = fs::write(build_cache().join("guest-transcript-check-login.log"), &transcript);

    // **The grid grew, in cells.** Read off the client's own line rather than computed here:
    // the cell size is the font's, and re-deriving it in the gate would be a second
    // implementation of `Metrics` that could agree with nothing. Every other assertion in 6g is
    // about *pixels*, and a grid still 80x24 in a 1280x752 window satisfies all of them — a
    // large window with a small terminal in the corner.
    // **The reflow, asserted on a release image without reading anybody's terminal.** A rewrap
    // re-breaks lines at a new width; it does not create or destroy them, so `nxterm` reports
    // the count either side of every resize and they must agree. The control the plan asks for
    // — "two deliberately short adjacent lines are still two rows" — is this same property
    // stated over the whole history: an implementation that ignored the soft-wrap flag and
    // joined every adjacent row collapses it to one line, in one number, whatever the content
    // happened to be. The row-level version of the control, with the long line and the short
    // pair spelled out, is `libterm`'s own test.
    let reflows = transcript_reflows(&transcript);
    if reflows.is_empty() {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err("no \"nxterm: resized to …, lines N->M\" line in the transcript: the \
                    terminal never accepted a `Configure`"
            .into());
    }
    for &(before, after, evicted) in &reflows {
        // **The eviction is subtracted, not tolerated.** Narrowing makes more rows out of the
        // same text, so a history near `SCROLLBACK` loses its oldest lines to the ring — and a
        // gate that asserted equality unconditionally would one day blame `Line::wrapped` for
        // the bound, sending the first person to hit it to the wrong file. Nothing in this
        // session comes near the cap, so `evicted` is 0 here; it is subtracted because the
        // claim is about the rewrap and this is what makes it exactly that (PR #252 review,
        // finding 2).
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

    match transcript_grid(&transcript) {
        Some((c, r)) if c > 80 && r > 24 => {
            println!("  ok: the maximised terminal's grid grew to {c}x{r} cells")
        }
        found => {
            let path = build_cache().join("guest-transcript-check-login.log");
            let _ = fs::write(&path, &transcript);
            return Err(format!(
                "the maximised terminal's grid is {found:?}, not bigger than the 80x24 it starts \
                 at. `nxterm` must refit the grid to the window it accepted — the window growing \
                 is Part D's easy half, and a terminal that cannot use the room is the point of \
                 the reflow"
            )
            .into());
        }
    }

    // **And that it asked before it insisted** (M12 Part A) — order-independent for the same
    // reason, and it is what makes step 12 a *policy* assertion rather than a coincidence: the
    // shell reached `Manage::Close` on the second click, and it reached `RequestClose` on the
    // first. A shell that destroyed the window straight away would produce the "did not answer"
    // line the ordered half already matched, and none of this.
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

    // The shell said it asked — order-independent, because that line races the client it woke.
    if !transcript.contains(&format!("desktop-shell: asked window {first_term_id} to close")) {
        let path = build_cache().join("guest-transcript-check-login.log");
        let _ = fs::write(&path, &transcript);
        return Err(format!(
            "the shell never reported asking window {first_term_id} to close, so the taskbar's \
             middle-click did not reach `Manage::RequestClose`"
        )
        .into());
    }

    // **The shell the terminal spawned went with it.** `nxterm` holds the pty master and hands
    // the far end to its `nxsh`; closing the window therefore has to end that shell too, or
    // every close leaks a process. Nothing observed the child until the Part C review asked —
    // the machinery was there (`libstream` documents `PeerClosed` as "stop producing, exit")
    // and the gate now says so out loud. Order-independent: the child notices its master go at
    // whatever moment the tty-server gets there.
    // **The menu was dismissed by the compositor, not by a focus change** (M11 Part E batch 5).
    // The distinction is the whole of the fix: clicking another *window* raises it, and a raise
    // is a focus change the popup hears about — which is why the first version of this step
    // passed while the reported bug survived. A press on a panel raises nothing.
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

    // **The close button sent nothing.** Present-and-absent together: the click above closed the
    // window, and no `RequestClose` was ever sent for it — the id is the second terminal's, so
    // the first one's genuine request cannot satisfy this.
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
    // **The browser started in the place, and listed it** (step 13) — the listing searched for
    // only *after* the start, because this gate has the browser list `/home/Documents` from its
    // sidebar long before, and a whole-transcript search passed a browser that logged its argument
    // and then navigated home (PR #314 review, blocking 2). Neither line can see the sidebar, which
    // is why `nxfiles` keeps `HOME` as its home whatever it starts at, and says so where it decides.
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
