//! `check-login`'s buffers, clipboard, `with` and rename.

use crate::*;

/// `check-login`'s steps 9a2 to 9d: two buffers in one window, copy and paste across programs,
/// `with`, and a rename.
pub(crate) fn login_buffers_and_clipboard(qmp: &mut Qmp, session: &mut Session, st: &mut LoginState) -> R<()> {
    let fx = st.fx;
    let fy = st.fy;
    let work = st.work;
    session.expect("nxedit: tab ")?;
    let showing = session.rest_of_line()?;
    if !showing.contains("other.txt") {
        return Err(format!("the drop should have made other.txt current; the editor says {showing:?}").into());
    }

    // **The tabs are where the fixed width says**, aimed from `chrome`'s copies of the toolkit's
    // constants (`the_gates_chrome_table_is_the_toolkits` keeps them equal), **below the editor's
    // menu bar**: the old y clicked the menus and failed silently as a tab that never became
    // current.
    let ed = (work.0 + (work.2 / 2) as i32, work.1);
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

    // **One process, two top-level windows**, asserted on the shell's window list rather than a
    // receipt the editor prints about itself. `Ctrl+Shift+N`, as `Ctrl+N` is a new buffer. The
    // shell logs the placement after the compositor answers, so the two lines come in either order.
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

    let listing = session.transcript();
    let last = last_window_list(&listing).unwrap_or_default().to_string();
    // **The entries, not the number**: the count after `of ` is `desktops.len()`, satisfied
    // whatever the taskbar holds.
    let count = last.matches('[').count();
    if count < 3 {
        // **The transcript, kept**: the list says what the shell showed and not why.
        let _ = fs::write(build_cache().join("guest-transcript-check-login.log"), session.transcript());
        let _ = session.child.kill();
        return Err(format!(
            "after New Window the shell lists {count} window(s) on this desktop — the browser, \
             the editor and the editor's second window make three. The list read {last:?}"
        )
        .into());
    }
    println!("  ok: the shell's window list counts it too ({count} windows)");

    // **Closing one window must not take the others with it**: an `exit(0)` inside the per-window
    // loop would discard what the other held. **Read from where the key was pressed, in either
    // order**: the receipt and the shell's list are two processes answering one event, and a bare
    // `expect_all` would be answered by the list line from opening the window.
    let new_id = placed.split_whitespace().next().unwrap_or("").to_string();
    let from = session.transcript().len();
    qmp.send_key("ctrl", true)?;
    press(qmp, "w")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: closed a window")?;
    // …and the editor is still running: **a list made after the key, without the closed window.**
    let gone = format!("[{new_id}:");
    session.line_since(
        from,
        "desktop-shell: window list on cli of ",
        |l| !l.contains(&gone) && l.contains("notes.txt"),
        std::time::Duration::from_secs(30),
    )?;
    println!("  ok: closing one window left the other open");

    let back = (ed.0 + 300, ed.1 + 400);
    click_at(qmp, session, back.0, back.1)?;
    println!("  ok: and clicking the first window took the keyboard back");

    // **`End` first**: step 8b's find left its match *selected*, and typing replaces a selection.
    press(qmp, "end")?;
    press(qmp, "c")?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 10 bytes")?;
    session.send("open ./papers/notes.txt")?;
    session.expect(&format!("{TYPED}abc"))?;
    session.expect("/home>")?;
    session.send("list ./papers")?;
    session.expect("/home>")?;
    println!("  ok: and the save reached the current tab's file, read back by the shell");

    // **The file chooser**: the *save* must go to the new path, not merely a dialog appear.
    // **After the ordinary save, deliberately**: Save As changes what the buffer *is*.
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(qmp, "s")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    // **The receipt counts the rows**: an unreadable directory opens the chooser over an *empty*
    // list, so waiting for the window would pass. Two: `notes.txt` and `other.txt`.
    session.expect("nxedit: choosing a file in /home/papers - 2 entries")?;
    // **And then the keyboard**: the line above precedes the chooser's window, so a backspace sent
    // on it could delete a character of the *document*.
    session.expect("nxedit: the chooser has the keyboard")?;
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

    // 9d. **Copy in the editor, paste in the terminal, and the shell reads what was copied**,
    //     **asserted through the filesystem**: a release `nxterm` does not report its grid, and a
    //     wrong paste makes a differently-named file. The copy is a **find**, which selects without
    //     changing the buffer.
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
    // **Escape first**: a chord that acts on the *buffer* stays the field's while one is open, so
    // `Ctrl+C` would go to the find field.
    press(qmp, "esc")?;
    // **A count, never the text** (the serial console is a log file): `TYPED`'s length says the
    // *selection* was copied.
    qmp.send_key("ctrl", true)?;
    press(qmp, "c")?;
    qmp.send_key("ctrl", false)?;
    session.expect(&format!("nxedit: copied {} bytes", TYPED.len()))?;
    println!("  ok: the editor copied its selection onto the ring");

    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(qmp, session, "nxterm")?;
    let launched = session.transcript().len();
    press(qmp, "ret")?;
    session.expect("desktop-shell: launched nxterm into its own namespace")?;
    session.expect("desktop-shell: applications menu closed")?;

    // **Its origin comes off the shell's own placement line**, since the cascade moves; its *size*
    // cannot, as the shell logs at most `MAX_LOGGED_GEOMETRY` geometry lines per session.
    let any = |_: &str| true;
    let wait = std::time::Duration::from_secs(45);
    let placed = session.line_since(launched, "desktop-shell: placed window ", any, wait)?;
    let (_, tx, ty) =
        parse_placement(&placed).ok_or_else(|| format!("could not read the terminal's placement from {placed:?}"))?;
    // **Wait for the shell before clicking, not after.** The press receipt is the *compositor's*
    // and says nothing about the client: keys sent while the guest was still opening a tty were
    // dropped whole (`SYN_DROPPED`). *Before* the click, since `Session::expect` consumes what it
    // scans past and a click's own wait could eat these lines.
    session.line_since(launched, "nxterm: hosting a shell", any, wait)?;
    session.line_since(launched, "nxsh: up", any, wait)?;
    click_at(qmp, session, tx + 100, ty + TITLE_BAR_H + MENU_BAR_H + 40)?;

    // `nitrox.clip` exists only if the paste delivered exactly `nitrox` *and* landed at the cursor.
    // **Paced, before every key**: a release image renders no grid line to wait on, and a batch
    // injected while the compositor recomposes is dropped whole (seen at 1024×768).
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

    // **The serial column is what says it happened.** **Retried, because the two sides are
    // concurrent**: only the filesystem orders the terminal's `touch` against the serial `list`.
    // Still a verdict at the end of the loop.
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

    // 9a2. **`with`, from a terminal the Applications menu opened**: a release image does not
    //      narrate the grid, so the broker's audit on the console is asserted: started, and
    //      `nxinstall` exiting 0 because it saw the granted disks.
    type_at_terminal(qmp, "with admin nxinstall")?;
    session.expect("view: alice admin nxinstall — allowed, asking for a password")?;
    type_answer(qmp, DEMO_PASSWORD)?;
    session.expect("view: alice admin nxinstall — started")?;
    session.expect("view: alice admin nxinstall — exited, code 0")?;
    println!("  ok: `with admin nxinstall` in a desktop terminal saw the granted disks");
    // **And not the one in use**: naming `/dev/blk/0`, the disk holding `init`'s root, makes
    // `nxinstall` log its refusal where a release gate can read it. **Not asked inside five
    // minutes**: the password opened a window, so nothing is typed.
    type_at_terminal(qmp, "with admin nxinstall /dev/blk/0")?;
    session.expect_all(&[
        "view: alice admin nxinstall — allowed, within the grace period",
        "nxinstall: refused /dev/blk/0: it holds the running system",
    ])?;
    session.expect("view: alice admin nxinstall — exited, code 1")?;
    println!("  ok: and it did not see /dev/blk/0, the disk holding init's root");

    for qcode in ["e", "x", "i", "t"] {
        press(qmp, qcode)?;
    }
    press(qmp, "ret")?;
    session.expect("nxterm: the terminal ended")?;

    // 9b. **The browser renames a file, and the shell reads the new name back.** A fresh listing
    //     selects row 0, `notes.txt`; a *press* on a row starts a drag and only a click activates
    //     one. **Clicking the bar item, not a keyboard route**: a menu nobody can point at is one
    //     nobody will find.
    let file_menu = (fx + 20, fy + 1 + TITLE_BAR_H + MENU_BAR_H / 2);
    // **One click, asserted directly**: a retry would hide the `libui` regression of losing the
    // gesture to a repaint between press and release.
    click_at(qmp, session, file_menu.0, file_menu.1)?;
    session.expect("nxfiles: menu popup ")?;
    let popup = session.rest_of_line()?;
    let (_, px, py, _pw, ph) =
        parse_menu_popup(&popup).ok_or_else(|| format!("could not read the menu's placement from {popup:?}"))?;

    // **The row is found rather than computed**: the File menu's **separator** takes height without
    // being a row. The walk stops when the guest agrees it is over `Rename`. The border plus space
    // above the first row is six.
    const MENU_FRAME: i32 = 6;
    const RENAME_ROW: u64 = 109;
    let rx = px + 20;
    let mut ry = py + MENU_FRAME;
    let mut rename_at = None;
    // **Each step records where it went**: `move_pointer_to` does not update the tracked position,
    // so a loop leaving it alone would run off the popup.
    while ry < py + ph as i32 - MENU_FRAME {
        move_pointer_to(qmp, rx, ry)?;
        qmp.pointer = Some((rx, ry));
        // **The row the pointer settled on, not the first it crossed** (`Session::settled_line`).
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
    qmp.pointer = Some(rename_at);
    click_at(qmp, session, rename_at.0, rename_at.1)?;
    // **Wait for the menu popup to be gone before typing**: a `popup` takes focus, so until
    // `nxfiles` closes it every key goes there and is dropped. Emitted after the close, that is the
    // whole guarantee, as the compositor serves queued requests before routing input.
    session.expect("nxfiles: name so far 0 chars")?;

    for (i, c) in "renamed".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxfiles: name so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    session.expect("nxfiles: renamed /home/papers/renamed")?;
    session.expect("nxfiles: listed /home/papers - 3 entries")?;
    println!("  ok: the browser's File menu renamed a file");

    session.send("open ./papers/renamed")?;
    session.expect(TYPED)?;
    session.expect("/home>")?;
    println!("  ok: and the shell found the new name, with the old contents");

    Ok(())
}
