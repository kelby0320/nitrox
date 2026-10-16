//! `check-login`'s buffers, clipboard, `with` and rename.

use crate::*;

/// `check-login`'s steps 9a2 to 9d: two buffers in one window, copy and paste across programs,
/// `with`, and a rename.
pub(crate) fn login_buffers_and_clipboard(qmp: &mut Qmp, session: &mut Session, st: &mut LoginState) -> R<()> {
    let fx = st.fx;
    let fy = st.fy;
    let work = st.work;
    // 9c. **Two buffers in one window, switched, and the right one saved** (M12 Part D). The drop
    //     above opened a *tab* rather than replacing the buffer — which is what removed the old
    //     refusal, since there is nothing to lose by taking a file when it arrives beside the one
    //     already open. So the editor is now showing `other.txt` with `notes.txt` a tab away.
    session.expect("nxedit: tab ")?;
    let showing = session.rest_of_line()?;
    if !showing.contains("other.txt") {
        return Err(format!("the drop should have made other.txt current; the editor says {showing:?}").into());
    }

    // **The tabs are where the fixed width says they are**, which is what a fixed width buys: a
    // tab does not move when another opens. `libui::widget`'s constants are the source, pinned to
    // a real tree by `a_tab_selects_where_it_is_and_its_close_box_does_not_select_it`; this gate
    // aims from `chrome`'s copies rather than from the toolkit (M11 decision 2), and
    // `the_gates_chrome_table_is_the_toolkits` is what keeps those copies equal to it.
    //
    // **A tab strip in both applications** (M12 Part D): above the editor's text and below the
    // browser's menus, which moved every one of the browser's rows down again.
    // `nxfiles::list_top` is that application's own version of the same sum.
    //
    // The editor is the work area's right half after the snap above.
    let ed = (work.0 + (work.2 / 2) as i32, work.1);
    // Tab `i`'s label area: the strip's own left padding, `TAB_PITCH` per tab, and a third of a
    // tab into the label — clear of the close box, whose centre is at `TAB_CLOSE_CX` (110).
    //
    // **Down the tab rather than down the strip** since Part H: the strip is 30 and a tab is the
    // 24 below its 5 of padding, so half the strip is still inside a tab but half a tab is the
    // honest aim.
    //
    // **Below the editor's menu bar since M14 Part A**, which put one above the strip to match
    // the browser. The strip did not move relative to anything it contains; the whole of it moved
    // down by a bar, and a gate that aimed at the old y clicked the menus instead — which is how
    // this failed, silently, as a tab that never became current.
    let tab = |i: i32| {
        (
            ed.0 + chrome::CONTENT_X + chrome::TAB_SIDE + chrome::TAB_PITCH * i + chrome::TAB_W / 3,
            ed.1 + 1 + TITLE_BAR_H + MENU_BAR_H + chrome::TAB_TOP + chrome::TAB_H / 2,
        )
    };
    let tab0 = tab(0);
    click_at(qmp, session, tab0.0, tab0.1)?;
    session.expect("nxedit: tab ")?;
    let switched = session.rest_of_line()?;
    if !switched.contains("notes.txt") {
        return Err(format!("clicking the first tab should show notes.txt; got {switched:?}").into());
    }
    println!("  ok: the editor holds two buffers, and a tab click switched between them");

    // **One process, two top-level windows** (M14 Part B) — the thing the protocol has always
    // allowed and nothing had done. The shell's window list is where it shows, which is why the
    // assertion is made there rather than on a receipt the editor prints about itself: a client
    // can say anything, and what matters is that the *manager* sees two windows it can name.
    //
    // `Ctrl+Shift+N`, with Shift because `Ctrl+N` is "new" in the singular everywhere and an
    // editor's singular is a buffer.
    //
    // The shell logs the placement only after the compositor answers it, by which time the window
    // is configured and the editor may already have said so: the two lines come in either order.
    let from = session.transcript().len();
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(qmp, "n")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    let any = |_: &str| true;
    let wait = std::time::Duration::from_secs(45);
    let placed = session.line_since(from, "desktop-shell: placed window ", any, wait)?;
    session.line_since(from, "nxedit: opened another window", any, wait)?;
    println!("  ok: a second top-level window from one process: {}", placed.trim());

    // **And it is a window in its own right**, not a decoration: the taskbar lists it, which is
    // the shell's own account of what exists rather than the editor's.
    let listing = session.transcript();
    let last = last_window_list(&listing).unwrap_or_default().to_string();
    // **The entries, not the number.** The number after `of ` is `desktops.len()`, which this
    // gate switched to 2 several steps ago and which is satisfied whatever the taskbar holds — so
    // reading it made this assertion decoration that could not fail (PR #283 review, blocking 3).
    // The windows are the bracketed `[id:label]` items.
    let count = last.matches('[').count();
    if count < 3 {
        // **The transcript, kept**: the list says what the shell showed and not why, and this
        // failed once in seventeen runs under KVM with nothing kept to read (Phase 6 Part B.7's
        // gate round, 2026-10-13).
        let _ = fs::write(build_cache().join("guest-transcript-check-login.log"), session.transcript());
        let _ = session.child.kill();
        return Err(format!(
            "after New Window the shell lists {count} window(s) on this desktop — the browser, \
             the editor and the editor's second window make three. The list read {last:?}"
        )
        .into());
    }
    println!("  ok: the shell's window list counts it too ({count} windows)");

    // **Closing one window must not take the others with it**, which is the whole of why the
    // conversion had to be uniform — and was broken anyway until PR #283's review: an `exit(0)`
    // left inside the per-window loop ran against *each* window's `App`, so closing either one
    // ended the process and discarded whatever the other held. Nothing caught it because this
    // gate had never closed a second window.
    //
    // `Ctrl+W` on the new window: it has one tab, and closing the last tab closes the window.
    //
    // **Read from where the key was pressed, in either order.** The editor's receipt and the
    // shell's list are two processes answering one event, and nothing orders them: this waited
    // for the receipt and then for a list line, and when the shell spoke first the list was
    // consumed on the way to the receipt and the wait ran out (PR #353's CI). A bare
    // `expect_all` would not do either — the list line from opening the window is still unread,
    // and would answer for the close.
    let new_id = placed.split_whitespace().next().unwrap_or("").to_string();
    let from = session.transcript().len();
    qmp.send_key("ctrl", true)?;
    press(qmp, "w")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: closed a window")?;
    // …and the editor is still running, which is the half that would have been lost: **a list
    // made after the key, without the closed window and still with the first.**
    let gone = format!("[{new_id}:");
    session.line_since(
        from,
        "desktop-shell: window list on cli of ",
        |l| !l.contains(&gone) && l.contains("notes.txt"),
        std::time::Duration::from_secs(30),
    )?;
    println!("  ok: closing one window left the other open");

    // **Give the first window the keyboard back**, because the new one took it — which is right,
    // and is exactly what makes the rest of this gate's typing land in the wrong place otherwise.
    // The editor was snapped to the right half earlier and the new window was placed at the
    // top-left, so they do not overlap and a click in the text area is unambiguous.
    let back = (ed.0 + 300, ed.1 + 400);
    click_at(qmp, session, back.0, back.1)?;
    println!("  ok: and clicking the first window took the keyboard back");

    // **`End` first, and that is not tidying.** Step 8b's search left its match *selected* —
    // which is what a find is supposed to do — and typing replaces a selection, so a bare
    // keystroke here would have edited the middle of the file rather than appending to it. It
    // did, on the first run: `tro` became `c` and the save wrote seven bytes. The behaviour is
    // right and the gate's assumption was not.
    press(qmp, "end")?;
    // **And the save goes to the tab that is current, not the one opened last.** That is the
    // whole risk of tabs in an editor, and the assertion is made from outside: the serial shell
    // reads the file back.
    press(qmp, "c")?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 10 bytes")?;
    session.send("open ./papers/notes.txt")?;
    session.expect(&format!("{TYPED}abc"))?;
    session.expect("/home>")?;
    // The other tab's file is untouched: a save that went to the wrong buffer would have
    // written nine bytes here instead of leaving it empty.
    session.send("list ./papers")?;
    session.expect("/home>")?;
    println!("  ok: and the save reached the current tab's file, read back by the shell");

    // **The file chooser** (M14 Part C). `Ctrl+Shift+S` on a buffer that has a name, then a new
    // name typed and accepted — and the assertion is that the *save* went to the new path, not
    // that a dialog appeared.
    //
    // **After the ordinary save above, deliberately.** Save As changes what the buffer *is*, so
    // every later `Ctrl+S` follows it — which is the feature, and which broke the step above when
    // this ran before it. A gate that had to be reordered by the change is the change being
    // real. "Save As is not Save with a prompt" is the part's whole content:
    // a chooser that only picked a destination would write these bytes to `notes.txt` and leave
    // the tab, the marker and the next `Ctrl+S` all pointing at the old file.
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(qmp, "s")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    // **The receipt counts the rows**, which is the half a "a dialog opened" expectation cannot
    // see: an unreadable directory opens the chooser over an *empty* list deliberately, so a gate
    // that only waited for the window would pass for the case decision 3 is about — the
    // application listing and the widget rendering what it was handed. Two entries here:
    // `notes.txt` and the `other.txt` the serial side made at step 8.
    session.expect("nxedit: choosing a file in /home/papers - 2 entries")?;
    // **And then the keyboard.** The line above is said before the chooser's window is asked
    // for, so a backspace sent on it could reach the editor while the shell was still placing
    // the dialog — and delete a character of the *document*. That was this gate's flake:
    // `buffer rev 12` where `chooser name so far 8 chars` should have been.
    session.expect("nxedit: the chooser has the keyboard")?;
    // **A receipt per character**, which is the discipline every typed sequence here follows and
    // the reason `nxedit` grew the line: an unacknowledged burst is a dropped keystroke found as a
    // wrong filename several steps later, and under KVM the injected keys arrive bunched enough
    // for it to happen. The field is seeded with the buffer's own name — `notes.txt`, nine
    // characters — so this empties it rather than appending to it, and the count runs down.
    for left in (0..9).rev() {
        press(qmp, "backspace")?;
        session.expect(&format!("nxedit: chooser name so far {left} chars"))?;
    }
    for (i, c) in "asked".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxedit: chooser name so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    session.expect("nxedit: saved /home/papers/asked - ")?;
    println!("  ok: Save As wrote to the name it was given, not to the file it was opened as");

    // 9d. **Copy in the editor, paste in the terminal, and the shell reads what was copied**
    //     (M12 Part E) — one gesture crossing two applications and a server, which is the whole
    //     point of making the clipboard a resource rather than a slot in a widget.
    //
    //     **Asserted through the filesystem, not through a log line.** `nxterm` in a *release*
    //     image does not report its grid — deliberately, since a terminal narrating itself to
    //     the kernel log undoes the point of the tty server owning output — so "the shell
    //     printed it" cannot be read off the transcript. What can be is the file the pasted text
    //     names: the serial column lists a directory and the name is there or it is not. That is
    //     the same two-session trick steps 7, 8 and 9b use, and it is stronger than a log line,
    //     because a paste that delivered the wrong bytes produces a differently-named file
    //     rather than a matching count.
    //
    //     The copy is a **find**, which selects its match — so this needs no keystroke that
    //     changes the buffer, and steps 11 and 12 find the editor exactly as step 9c left it.
    qmp.send_key("ctrl", true)?;
    press(qmp, "f")?;
    qmp.send_key("ctrl", false)?;
    for (i, c) in TYPED.chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxedit: find so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    session.expect("nxedit: find hit at line 0")?;
    // **Escape first, and that is the rule Part D's review settled rather than tidying.** A
    // chord that acts on the *buffer* stays the field's while a field is open, and a copy is
    // one — so `Ctrl+C` here would have gone to the find field and done nothing. (It did, on
    // this step's first run.) Escape closes the field and leaves the match selected: the
    // selection is the buffer's, not the field's.
    press(qmp, "esc")?;
    // Ctrl+C. **A count, never the text** — an editor's buffer is a person's document, and the
    // serial console is a log file. The count is exactly `TYPED`'s length, which is what says
    // the *selection* was copied rather than the line or the buffer.
    qmp.send_key("ctrl", true)?;
    press(qmp, "c")?;
    qmp.send_key("ctrl", false)?;
    session.expect(&format!("nxedit: copied {} bytes", TYPED.len()))?;
    println!("  ok: the editor copied its selection onto the ring");

    // A terminal to paste into. **Launched here rather than reusing step 6's**, which was
    // closed long before the editor existed — and closed again at the end of this step, so the
    // windows steps 11 and 12 drive are the ones they expect to find.
    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(qmp, session, "nxterm")?;
    // The terminal's lines and the shell's placement of its window come in either order.
    let launched = session.transcript().len();
    press(qmp, "ret")?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    session.expect("desktop-shell: applications menu closed")?;

    // Click into the new terminal's grid to give it the keyboard. **Its origin comes off the
    // shell's own placement line** — the cascade moves, and a gate that assumed an origin is
    // the bug M11 Part E batch 4 fixed. Its *size* cannot: the shell logs at most
    // `MAX_LOGGED_GEOMETRY` geometry lines per session and this run passed that long ago, which
    // is why step 11 hardcodes the editor's size too. A point a little inside the top-left is
    // in the grid whatever the size, and needs no second number.
    let any = |_: &str| true;
    let wait = std::time::Duration::from_secs(45);
    let placed = session.line_since(launched, "desktop-shell: placed window ", any, wait)?;
    let (_, tx, ty) =
        parse_placement(&placed).ok_or_else(|| format!("could not read the terminal's placement from {placed:?}"))?;
    // Below the title bar and the terminal's own menu bar, and well inside the frame.
    // **Wait for the shell before clicking, not after**, and both halves of that are load-bearing.
    //
    // *Wait*, because this step used to type into a terminal that was still starting: the press
    // receipt is the *compositor's* and says nothing about the client, so the eight characters of
    // `touch ./` and the chord behind them went out while the guest was opening a tty, attaching a
    // backend and drawing a window. The batch carrying them was dropped —
    // `compositor: input batch DROPPED (SYN_DROPPED)` — and the paste simply never happened. It
    // was a race this step had been winning: M14 Part C added receipts to the editor step above,
    // the phase of everything after it moved, and it began failing every run. Same shape as the
    // drag hand-off in Part B, which the tab strip exposed and which had been called a flake twice.
    //
    // *Before the click*, because `Session::expect` consumes what it scans past and the two event
    // sources interleave freely: the first attempt put these two lines after `click_at`, and on
    // the very next run the shell came up *before* the press receipt, so the click's own wait ate
    // them and the gate hung on a line already in the transcript. The window has been placed by
    // here, which is all a click needs.
    session.line_since(launched, "nxterm: hosting a shell", any, wait)?;
    session.line_since(launched, "nxsh: up", any, wait)?;
    click_at(qmp, session, tx + 100, ty + TITLE_BAR_H + MENU_BAR_H + 40)?;

    // `touch ./` — typed — then the paste, then a suffix. **The suffix is what makes the
    // assertion discriminating**: `nitrox.clip` can only exist if the paste delivered exactly
    // `nitrox` *and* landed at the cursor with typing continuing after it. A paste that
    // delivered nothing leaves `.clip`; one that delivered the whole line leaves something
    // else again.
    // QMP's own key names, not the characters: `.` is `dot` and `/` is `slash`.
    //
    // **Paced, before every key including the first** — `type_at_terminal`'s rule, for its reason:
    // a release image renders no grid line to wait on per character, and a batch injected while
    // the compositor recomposes is dropped whole (`input batch DROPPED`). Waiting for `nxsh: up`
    // above closed the race at 1280×800 and 1360×768; `check-resolutions` lost the whole of
    // `touch ./` to it at 1024×768, where the window list's redraw lands in the same moment
    // (Phase 5 Part E.4).
    const PER_KEY: std::time::Duration = std::time::Duration::from_millis(40);
    for qcode in ["t", "o", "u", "c", "h", "spc", "dot", "slash"] {
        std::thread::sleep(PER_KEY);
        press(qmp, qcode)?;
    }
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(qmp, "v")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    session.expect(&format!("nxterm: pasted {} bytes", TYPED.len()))?;
    for qcode in ["dot", "c", "l", "i", "p"] {
        std::thread::sleep(PER_KEY);
        press(qmp, qcode)?;
    }
    press(qmp, "ret")?;

    // **The serial column is what says it happened.** Asking the terminal would be asking the
    // accused: its grid is where the pasted text was drawn, and the thing in doubt is whether
    // the bytes were real.
    //
    // **Retried, because the two sides are genuinely concurrent.** Nothing orders the terminal's
    // `touch` against the serial `list` — two shells in two sessions, and the only thing between
    // them is the filesystem. The first version sent one `list` and passed by luck; M12 Part F's
    // wallpaper added a PNG decode to the shell's startup, the timing moved, and it started
    // failing. This is `expect_within`'s stated case — absence that is retryable rather than a
    // verdict — and it is still a verdict at the end of the loop: a `touch` that never ran fails
    // the gate after thirty seconds rather than passing.
    let mut made = false;
    for _ in 0..10 {
        session.send("list .")?;
        if session.expect_within(&format!("{TYPED}.clip"), std::time::Duration::from_secs(3))? {
            made = true;
            break;
        }
        // Discard the failed listing, or the next attempt matches this one's output.
        session.skip_to_end()?;
    }
    if !made {
        return Err(format!(
            "the terminal pasted {} bytes but no {TYPED}.clip appeared in /home: the shell in \
             the terminal did not run the command the paste completed",
            TYPED.len()
        )
        .into());
    }
    session.expect("/home>")?;
    println!("  ok: the paste reached the shell in the terminal, and it made {TYPED}.clip");

    // 9a2. **`with`, from a terminal the Applications menu opened** (administration Part A). The
    //      whole chain the serial gate cannot see: `desktop-shell` bound `/dev/views` into this
    //      terminal's namespace at the graphical session's base; `nxsh` handed `with` a sibling of
    //      the window's terminal; the password is typed into *this window* and read there. A
    //      release image does not narrate the grid, so what is asserted is the broker's audit on
    //      the console — started, and `nxinstall` exiting 0 because it saw the granted disks.
    type_at_terminal(qmp, "with admin nxinstall")?;
    session.expect("view: alice admin nxinstall — allowed, asking for a password")?;
    // No leading Enter here, unlike `type_at_terminal`: at a password prompt one would be an empty
    // password.
    type_answer(qmp, DEMO_PASSWORD)?;
    session.expect("view: alice admin nxinstall — started")?;
    session.expect("view: alice admin nxinstall — exited, code 0")?;
    println!("  ok: `with admin nxinstall` in a desktop terminal saw the granted disks");
    //      **And not the one in use** (administration Part C.6). Code 0 above says the grant bound
    //      something, which on a release boot is the ESP alone. The disk holding `init`'s root is
    //      `/dev/blk/0`, and naming it makes `nxinstall` log its refusal on the console, where a
    //      release image's gate can read it: the disk is not in the session at all. Until C.6
    //      this step passed on that disk too, which was the weaker claim.
    //      **Not asked, in the same terminal, inside five minutes** (the laptop polish's Part A):
    //      the password above opened a window for this window's terminal and `admin`, so the
    //      broker starts this without a prompt, and nothing is typed. `boot-probe` holds the
    //      window to its terminal and view, and the forgery control, on a test image.
    type_at_terminal(qmp, "with admin nxinstall /dev/blk/0")?;
    // **Refused as the running system** (administration Part G.2), which the installer now knows:
    // the tables say `init` mounted its root partition. Unordered with the audit line: that one
    // reaches the console through the logging service, the refusal straight from `nxinstall`.
    session.expect_all(&[
        "view: alice admin nxinstall — allowed, within the grace period",
        "nxinstall: refused /dev/blk/0: it holds the running system",
    ])?;
    session.expect("view: alice admin nxinstall — exited, code 1")?;
    println!("  ok: and it did not see /dev/blk/0, the disk holding init's root");

    // Close the terminal from inside, so steps 11 and 12 drive the windows step 9 left.
    for qcode in ["e", "x", "i", "t"] {
        press(qmp, qcode)?;
    }
    press(qmp, "ret")?;
    session.expect("nxterm: the terminal ended")?;

    // 9b. **The browser renames a file, and the shell reads the new name back** (M12 Part B).
    //     The same two-session fact steps 7 and 8 use, a third time: the graphical side does
    //     something to the filesystem and the *serial* side is what says it happened. Asking the
    //     browser to re-list would be asking the accused — it would answer from the listing it
    //     holds, which is the thing in doubt.
    //
    //     The browser is showing `/home/papers` and a fresh listing selects row 0, which is
    //     `notes.txt` — directories sort first and then by name, and this directory has none.
    //     Nothing has moved the selection since: a *press* on a row starts a drag and only a
    //     click activates one, which is what step 9 relied on too.
    //
    //     **Clicking the bar item, not a keyboard route**, because a menu that cannot be opened
    //     by pointing at it is a menu nobody will find. The item sits at the content's left
    //     edge, one bar below the title.
    let file_menu = (fx + 20, fy + 1 + TITLE_BAR_H + MENU_BAR_H / 2);
    // **One click, asserted directly.** This was retried for a fortnight of CI runs while a
    // delivered click intermittently produced nothing; the cause was `libui` losing the gesture
    // when a repaint between the press and the release re-identified the widget under it, and it
    // is fixed. The retry is gone deliberately: it would hide the regression it was written for.
    click_at(qmp, session, file_menu.0, file_menu.1)?;
    session.expect("nxfiles: menu popup ")?;
    let popup = session.rest_of_line()?;
    let (_, px, py, _pw, ph) =
        parse_menu_popup(&popup).ok_or_else(|| format!("could not read the menu's placement from {popup:?}"))?;

    // **The row is found rather than computed** (M14 Part A). This used to divide the popup's
    // height by a row count, which was already careful about not deriving a row height from the
    // theme — `font_px` is a metric M11's decision 2 keeps out of a gate's assumptions. The File
    // menu now holds a **separator**, which occupies height without being a row, so no division
    // names the right one. The client says which row the pointer is over, so the gate walks down
    // the popup and stops when the guest agrees it is over `Rename`.
    // The popup's border and the space above its first row: one and five since the desktop
    // refresh's Part B (the design's `padding: 5px 0`), one and two before.
    const MENU_FRAME: i32 = 6;
    // `Rename` is `MENU_ROW_KEY + 8`: New Tab, Close Tab, a rule, New Window, Quit, a rule, New
    // File, New Folder, Rename. **It has moved twice** — once when the menu gained separators and
    // again when M14 Part B added the window rows — and each time the gate failed as a *prompt*
    // that never opened, several steps from the menu it was really about. The walk below at least
    // says which row it could not find.
    const RENAME_ROW: u64 = 109;
    let rx = px + 20;
    let mut ry = py + MENU_FRAME;
    let mut rename_at = None;
    // **Each step records where it went**, which is the exception the terminal gate's own walk
    // states: `move_pointer_to` walks a delta from the tracked position and does not update it,
    // so a loop that left it alone would re-walk the whole delta every time and run the pointer
    // off the popup. The receipt is what confirms the position; this is only what keeps the
    // steps four pixels apart.
    while ry < py + ph as i32 - MENU_FRAME {
        move_pointer_to(qmp, rx, ry)?;
        qmp.pointer = Some((rx, ry));
        // **The row the pointer settled on, not the first one it crossed** — see
        // `Session::settled_line`. The receipt lands on the frame after the motion; a step that
        // arrived nowhere new says nothing, and the next step is taken.
        let over = session.settled_line(
            "nxfiles: menu hover ",
            std::time::Duration::from_millis(150),
            std::time::Duration::from_millis(400),
        )?;
        if over.as_deref() == Some(RENAME_ROW.to_string().as_str()) {
            println!("  ok: the pointer settled over row {RENAME_ROW}");
            rename_at = Some((rx, ry));
            break;
        }
        ry += 4;
    }
    let Some(rename_at) = rename_at else {
        return Err(format!(
            "the pointer walked {}..{} down the File menu and the guest never reported row \
             {RENAME_ROW} under it",
            py + MENU_FRAME,
            py + ph as i32 - MENU_FRAME
        )
        .into());
    };
    // The guest has just said the pointer is over the row, so the position is known by evidence.
    // `move_pointer_to` records nothing itself — injection is relative, and an unacknowledged
    // move leaves the host believing something it cannot check.
    qmp.pointer = Some(rename_at);
    click_at(qmp, session, rename_at.0, rename_at.1)?;
    // **Wait for the menu popup to be gone before typing**, which the desktop-rename step at
    // step 6 already does in its own way and this one did not.
    //
    // The thing holding the keyboard is not the prompt — the prompt is a keyboard *mode* inside
    // the browser's own window. It is the **menu popup**: a `popup` takes focus and is topmost,
    // so until `nxfiles` closes it every key goes there and a letter yields no message. A
    // character injected before the close is dropped, and the gate then times out on "name so far
    // 1 chars" with "0 chars" sitting in the transcript. Seen under TCG and then under KVM on
    // 2026-09-03 (PR #278 review, blocking 1 — the first fix waited for a receipt that `nxfiles`
    // printed *before* closing the menu, which narrowed the window without closing it).
    //
    // `nxfiles` now emits this after the close rather than before, so it means the destroy has
    // been sent — **and since 2026-09-22 that is the whole guarantee**, because the compositor
    // serves every queued request before it routes input. Before that, a busy compositor could
    // still route the key first, and this step timed out now and then in CI with the destroy
    // served a millisecond after the key.
    session.expect("nxfiles: name so far 0 chars")?;

    // **A receipt per character**, the discipline every typed sequence in this gate follows —
    // and `nxfiles` grew the line to make it possible, for the reason the launcher's filter did
    // one part earlier: an unacknowledged burst is a dropped keystroke discovered as a wrong
    // filename several steps later.
    for (i, c) in "renamed".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxfiles: name so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    session.expect("nxfiles: renamed /home/papers/renamed")?;
    // **Three, because Save As made one.** The count is here to say a rename is not a copy, and
    // the number it compares against is the directory's — so the step above that wrote
    // `/home/papers/asked` moved it, and the earlier listing at step 8 still reads 2. A gate whose
    // constants are the filesystem's state has to change when something writes to it; that is the
    // coupling working, and it is what caught this on the first run.
    session.expect("nxfiles: listed /home/papers - 3 entries")?;
    println!("  ok: the browser's File menu renamed a file");

    // **And the serial session reads it back**, which is what makes this a rename rather than a
    // browser that redrew a label. The content is step 8's, so a rename that had copied or
    // truncated shows up here as well.
    session.send("open ./papers/renamed")?;
    session.expect(TYPED)?;
    session.expect("/home>")?;
    println!("  ok: and the shell found the new name, with the old contents");

    Ok(())
}
