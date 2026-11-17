//! `check-login`'s file browser and editor.

use crate::*;

/// `check-login`'s step 7: the file browser, and the two sessions used as one fact.
pub(crate) fn login_file_browser(qmp: &mut Qmp, session: &mut Session, st: &mut LoginState) -> R<()> {
    let shell_px = st.shell_px;
    session.send("mkdir ./papers")?;
    session.expect("/home>")?;

    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(qmp, session, "nxfiles")?;
    press(qmp, "ret")?;
    // **The theme reached the application**, the same number at both ends. **Unordered against the
    // shell's own line**: the shell logs *after* `send_setup_env` returns, so the application can
    // reach the console first, and a sequence timed out on a line already in the transcript.
    let launched = String::from("desktop-shell: launched nxfiles into its own namespace");
    let themed = format!("nxfiles: theme font_px {shell_px}");
    session.expect_all(&[&launched, &themed])?;
    // **And that it is the staged one**: `THEME_FONT_PX` is not the built-in size. This is the one
    // line the "delete the theme file" control removes.
    if shell_px != u32::from(THEME_FONT_PX) {
        return Err(format!(
            "the session resolved font_px {shell_px}, but the image staged {THEME_FONT_PX} — \
             the shell did not read the file the build wrote"
        )
        .into());
    }
    session.expect("nxfiles: listed /home - ")?;
    let listed = session.rest_of_line()?;
    println!("  ok: nxfiles started at HOME and listed it ({})", listed.trim());

    // **Where the shell put it**, read after the listing: a client lists *then* creates its window,
    // so asking first consumed the listing.
    let files_win = next_geometry(session)?;

    // **Arrowed to rather than assumed**: a fresh listing selects `Documents`, not `papers`; each
    // arrow is acknowledged by the browser's `selected` receipt.
    select_papers(qmp, session)?;
    press(qmp, "ret")?;
    session.expect("nxfiles: listed /home/papers - 0 entries")?;
    println!("  ok: Enter descended, and an empty directory lists nothing");

    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;
    println!("  ok: Backspace went back up");

    qmp.send_key("ctrl", true)?;
    press(qmp, "l")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: location so far 5 chars")?;
    println!("  ok: Ctrl+L opened the bar seeded with /home");

    // **Backspace here must not go up a directory**: a listing that still had the keyboard would
    // navigate and the counts would never arrive.
    for left in (0..5).rev() {
        press(qmp, "backspace")?;
        session.expect(&format!("nxfiles: location so far {left} chars"))?;
    }
    for (i, c) in "/home/papers".chars().enumerate() {
        let qcode = match c {
            '/' => String::from("slash"),
            other => other.to_string(),
        };
        press(qmp, &qcode)?;
        session.expect(&format!("nxfiles: location so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    // **The bar closes before the listing arrives**: the receipt is emitted from the render pass, a
    // loop turn before the navigation is taken, and `expect` consumes what it scans past.
    session.expect("nxfiles: location bar closed")?;
    session.expect("nxfiles: listed /home/papers - ")?;
    println!("  ok: and what was typed into it became a navigation");

    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    // **Properties.** **What only a boot can say is that it *drew***: an undiffable dialog opens
    // and never paints, so both ends are asserted and the slice between is checked.
    let before_props = session.transcript().len();
    qmp.send_key("ctrl", true)?;
    press(qmp, "i")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: showing properties")?;
    // **Pressed once the dialog has the keyboard**: `showing` is printed before the window exists,
    // and an `Esc` reaching the browser does nothing.
    session.expect("nxfiles: properties has the keyboard")?;
    press(qmp, "esc")?;
    session.expect("nxfiles: properties closed")?;
    let props_window = session.transcript()[before_props..].to_string();
    if props_window.contains("could not be drawn") || props_window.contains("could not open") {
        let _ = session.child.kill();
        return Err(format!("the properties dialog opened and complained: {props_window:?}").into());
    }
    println!("  ok: Ctrl+I showed the properties dialog, and Esc closed it");

    const SIDEBAR_PAD: i32 = browser::SIDEBAR_PAD;
    let content_top = files_win.2 + TITLE_BAR_H + MENU_BAR_H + PATH_H;
    let row_y = |row: i32| content_top + browser::HEADER_H + row * ROW_H + ROW_H / 2;
    let side_y = |row: i32| content_top + SIDEBAR_PAD + row * ROW_H + ROW_H / 2;

    files_context_menus(qmp, session, st, files_win, row_y(4))?;

    // **One press on *Documents* goes there.** **This keeps two spellings of the folder names in
    // step**: `libfs` and the image build each name them, and this crate cannot link a guest
    // library.
    click_at(qmp, session, files_win.1 + 40, side_y(1))?;
    session.expect("nxfiles: listed /home/Documents - ")?;
    println!("  ok: one press on a sidebar row went there");
    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    // **Cut, copy and paste of files**: the **serial** side reads the result back, since a paste on
    // the wrong path makes a differently-named file. Row 4 is `theme.toml`.
    click_at(qmp, session, files_win.1 + SIDEBAR_W + 120, row_y(4))?;
    session.expect("nxfiles: selected theme.toml")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "c")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: clipboard now holds 1 path(s) to copy")?;
    println!("  ok: Ctrl+C put a path on the clipboard");

    click_at(qmp, session, files_win.1 + 40, side_y(1))?;
    session.expect("nxfiles: listed /home/Documents - 0 entries")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "v")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: copied /home/Documents/theme.toml")?;
    session.expect("nxfiles: listed /home/Documents - 1 entries")?;
    println!("  ok: and Ctrl+V copied it into another directory");

    session.send("list ./Documents")?;
    session.expect("theme.toml")?;
    session.expect("/home>")?;
    println!("  ok: and the shell sees the file the paste made");

    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    // **A single click selects, a double click opens**: host tests do not show a real pointer's two
    // presses arrive close enough in the guest. The row cannot be row 0, already selected.
    let before = session.transcript().len();
    let file_row = (files_win.1 + SIDEBAR_W + 120, row_y(4));
    click_at(qmp, session, file_row.0, file_row.1)?;
    session.expect("nxfiles: selected theme.toml")?;
    println!("  ok: one click on a file selected it");

    // **And did not open it**, which `expect` cannot say, as it scans past the line that would
    // prove failure: the transcript since the click is checked.
    let window = session.transcript()[before..].to_string();
    if window.contains("nxfiles: asked to open") {
        let _ = session.child.kill();
        return Err(format!(
            "a single click on a file must select it, not open it — the transcript since the \
             click holds an open request. {window:?}"
        )
        .into());
    }
    println!("  ok: and did not ask the shell to open it");

    let dir_row = (files_win.1 + SIDEBAR_W + 120, row_y(3));
    click_at(qmp, session, dir_row.0, dir_row.1)?;
    session.expect("nxfiles: selected papers")?;
    click_at(qmp, session, dir_row.0, dir_row.1)?;
    session.expect("nxfiles: listed /home/papers - ")?;
    println!("  ok: and a second click on a directory descended");

    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    st.files_win = files_win;
    Ok(())
}

/// The browser's context menus: a right click on a row, the Menu key, and the folder's menu at the
/// bottom-right of a maximized window. Row 4, at `file_y`, is `theme.toml`; a fresh listing selected
/// row 0, and the navigation after this selects it again.
fn files_context_menus(
    qmp: &mut Qmp,
    session: &mut Session,
    st: &LoginState,
    files_win: (u32, i32, i32, u32, u32),
    file_y: i32,
) -> R<()> {
    // `Properties`, the row menu's last row (`nxfiles::menus::tests` pins the number).
    const PROPERTIES_ROW: u64 = 109;
    let (id, wx, wy, ww, _) = files_win;

    // **A right click on a row is its menu, not a click**: it selects the row and opens nothing, and
    // the menu's Properties shows the dialog Ctrl+I did.
    let before = session.transcript().len();
    right_click_at(qmp, session, wx + SIDEBAR_W + 120, file_y)?;
    session.expect("nxfiles: menu popup ")?;
    let popup = session.rest_of_line()?;
    let (_, px, py, _, ph) = parse_menu_popup(&popup).ok_or_else(|| format!("no menu placement in {popup:?}"))?;
    session.expect("nxfiles: context menu at ")?;
    let asked = session.rest_of_line()?;
    if parse_context_menu(&asked).map(|(_, _, what)| what) != Some("theme.toml") {
        return Err(format!("the right click opened a menu for something else: {asked:?}").into());
    }
    session.expect("nxfiles: selected theme.toml")?;
    let span = (py + MENU_FRAME, py + ph as i32 - MENU_FRAME);
    let props = walk_to_menu_row(qmp, session, "nxfiles: menu hover ", px + 20, span, PROPERTIES_ROW)?;
    click_at(qmp, session, props.0, props.1)?;
    session.expect("nxfiles: showing properties")?;
    session.expect("nxfiles: properties has the keyboard")?;
    press(qmp, "esc")?;
    session.expect("nxfiles: properties closed")?;
    let since = session.transcript()[before..].to_string();
    if since.contains("nxfiles: asked to open") {
        return Err(format!("a right click on a file opened it: {since:?}").into());
    }
    println!("  ok: a right click on a row opened its menu, and its Properties the dialog");

    // **The Menu key opens the selected row's menu**, from the row's bottom-left; Up from no row is
    // the last, Properties.
    press(qmp, "compose")?;
    session.expect("nxfiles: menu popup ")?;
    let popup = session.rest_of_line()?;
    let (_, _, py, _, ph) = parse_menu_popup(&popup).ok_or_else(|| format!("no menu placement in {popup:?}"))?;
    session.expect("nxfiles: context menu at ")?;
    let asked = session.rest_of_line()?;
    let Some((_, ay, "theme.toml")) = parse_context_menu(&asked) else {
        return Err(format!("the Menu key opened a menu for something else: {asked:?}").into());
    };
    // Below the row's bottom, or flipped above it: hung from it, either way.
    if py != wy + ay && py + ph as i32 != wy + ay {
        return Err(format!("the menu {popup:?} does not hang from {asked:?} in a window at y={wy}").into());
    }
    press(qmp, "up")?;
    press(qmp, "ret")?;
    session.expect("nxfiles: showing properties")?;
    session.expect("nxfiles: properties has the keyboard")?;
    press(qmp, "esc")?;
    session.expect("nxfiles: properties closed")?;
    println!("  ok: the Menu key opened the selected row's menu, and its keys chose Properties");

    // **Maximized, the folder's menu at the listing's bottom-right flips both ways**: the compositor
    // keeps it inside the work area, its bottom-right at the click.
    let work = st.work;
    click_at(qmp, session, wx + ww as i32 - chrome::MAXIMISE_X, wy + chrome::TITLE_Y)?;
    let maximised = format!("desktop-shell: maximize window {id} to {},{} {}x{}", work.0, work.1, work.2, work.3);
    let resized = format!("nxfiles: resized to {}x{}", work.2, work.3);
    session.expect_all(&[&maximised, &format!("desktop-shell: window {id} told state 2"), &resized])?;
    session.expect(&format!("desktop-shell: window {id} geometry {},{} {}x{}", work.0, work.1, work.2, work.3))?;
    let at = (work.0 + work.2 as i32 - 30, work.1 + work.3 as i32 - 60);
    right_click_at(qmp, session, at.0, at.1)?;
    session.expect("nxfiles: menu popup ")?;
    let popup = session.rest_of_line()?;
    let (_, px, py, pw, ph) = parse_menu_popup(&popup).ok_or_else(|| format!("no menu placement in {popup:?}"))?;
    session.expect("nxfiles: context menu at ")?;
    let asked = session.rest_of_line()?;
    if parse_context_menu(&asked) != Some((at.0 - work.0, at.1 - work.1, "the folder")) {
        return Err(format!("the click at {at:?} asked for {asked:?}, not the folder's menu there").into());
    }
    let (right, bottom) = (px + pw as i32, py + ph as i32);
    let inside = px >= work.0 && py >= work.1 && right <= work.0 + work.2 as i32 && bottom <= work.1 + work.3 as i32;
    if !inside || (right, bottom) != at {
        return Err(
            format!("the folder's menu {popup:?} is not inside {work:?} with its bottom-right at {at:?}").into()
        );
    }
    println!("  ok: the folder's menu at the bottom-right corner opened up and to the left, inside the work area");
    press(qmp, "esc")?;
    click_at(qmp, session, work.0 + work.2 as i32 - chrome::MAXIMISE_X, work.1 + chrome::TITLE_Y)?;
    session.expect_all(&[
        &format!("desktop-shell: restore window {id} to "),
        &format!("desktop-shell: window {id} told state 0"),
        &format!("nxfiles: resized to {}x{}", files_win.3, files_win.4),
    ])?;
    session.expect(&format!("desktop-shell: window {id} geometry {wx},{wy} {}x{}", files_win.3, files_win.4))?;
    Ok(())
}

/// `check-login`'s steps 8 to 9: the editor opened by the browser, undo and find, and a file
/// dropped onto it.
pub(crate) fn login_editor(qmp: &mut Qmp, session: &mut Session, size: DisplaySize, st: &mut LoginState) -> R<()> {
    let files_win = st.files_win;
    let work = st.work;
    session.send("touch ./papers/notes.txt")?;
    session.expect("/home>")?;

    select_papers(qmp, session)?;
    press(qmp, "ret")?;
    session.expect("nxfiles: listed /home/papers - 1 entries")?;

    press(qmp, "ret")?;
    session.expect("desktop-shell: launched nxedit into its own namespace")?;
    let asked = String::from("nxfiles: asked to open /home/papers/notes.txt");
    let opened = String::from("nxedit: opened /home/papers/notes.txt - 0 bytes, highlighted as plain");
    session.expect_all(&[&asked, &opened])?;
    let edit_win = next_geometry(session)?;
    println!("  ok: a file row launched the editor on the file it names");

    for (i, c) in TYPED.chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxedit: buffer rev {}", i + 1))?;
    }
    println!("  ok: {} keystrokes reached the editor's buffer", TYPED.len());

    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 7 bytes")?;

    // **The assertion is made from outside the editor**: asking it is asking the accused.
    session.send("open ./papers/notes.txt")?;
    session.expect(TYPED)?;
    session.expect("/home>")?;
    println!("  ok: the shell read back what the editor saved, from outside it");

    // 8b. **Undo, redo, and find**, asserted **by byte count, from outside**: two characters typed
    //     together are one group, so one undo leaves seven bytes and a redo nine. Per-*keystroke*
    //     grouping would leave eight.
    for c in "ab".chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect("nxedit: buffer rev ")?;
    }

    qmp.send_key("ctrl", true)?;
    press(qmp, "z")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 7 bytes")?;
    session.send("open ./papers/notes.txt")?;
    session.expect(TYPED)?;
    session.expect("/home>")?;
    println!("  ok: one undo took the whole group, and the shell read back the shorter file");

    qmp.send_key("ctrl", true)?;
    press(qmp, "y")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 9 bytes")?;
    session.send("open ./papers/notes.txt")?;
    session.expect(&format!("{TYPED}ab"))?;
    session.expect("/home>")?;
    println!("  ok: and redo put it back, byte for byte");

    qmp.send_key("ctrl", true)?;
    press(qmp, "f")?;
    qmp.send_key("ctrl", false)?;
    for (i, c) in "tro".chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxedit: find so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    session.expect("nxedit: find hit at line 0")?;
    press(qmp, "esc")?;
    println!("  ok: find took the keys, matched, and gave them back");

    // **A right click is the document's menu, and its Paste pastes where the click was**: find left
    // `tro` selected, and a right click past the end of the text, outside the selection, moves the
    // caret there first. The shell reads the file back.
    qmp.send_key("ctrl", true)?;
    press(qmp, "c")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: copied 3 bytes")?;
    let (_, ex, ey, ew, eh) = edit_win;
    right_click_at(qmp, session, ex + ew as i32 / 2, ey + eh as i32 / 2)?;
    session.expect("nxedit: menu popup ")?;
    let popup = session.rest_of_line()?;
    let (_, px, py, _, ph) = parse_menu_popup(&popup).ok_or_else(|| format!("no menu placement in {popup:?}"))?;
    // `Paste`, the menu's sixth row (`nxedit::menus::tests` pins the number).
    const PASTE_ROW: u64 = 105;
    let span = (py + MENU_FRAME, py + ph as i32 - MENU_FRAME);
    let paste = walk_to_menu_row(qmp, session, "nxedit: menu hover ", px + 20, span, PASTE_ROW)?;
    click_at(qmp, session, paste.0, paste.1)?;
    session.expect("nxedit: pasted 3 bytes")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 12 bytes")?;
    session.send("open ./papers/notes.txt")?;
    session.expect(&format!("{TYPED}abtro"))?;
    session.expect("/home>")?;
    println!("  ok: a right click past the text moved the caret, and its menu's Paste pasted there");
    // Undone and saved, so the file is as the steps after this one expect it.
    qmp.send_key("ctrl", true)?;
    press(qmp, "z")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 9 bytes")?;

    let (edit_id, ex, ey, ew, _eh) = edit_win;
    let title_grab = (ex + ew as i32 / 2, ey + chrome::TITLE_Y);
    move_pointer_to(qmp, title_grab.0, title_grab.1)?;
    qmp.pointer = Some(title_grab);
    qmp.send_button("left", true)?;
    for _ in 0..6 {
        qmp.send_motion(120, 0)?;
    }
    session.expect("compositor: interactive move of window ")?;
    // To the right edge, where the shell registered a snap zone: **a screen's width of travel, into
    // the clamp, and paced**. Six motions of 120 stopped 176 px short on 1920; twenty-three unpaced
    // ones overran the input ring on 2560 and dropped the release.
    for _ in 0..(size.w / 240 + 2) {
        qmp.send_motion(240, 0)?;
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    qmp.send_button("left", false)?;
    qmp.pointer = None;
    expect_after_pointer(
        qmp,
        session,
        &format!(
            "desktop-shell: drop window {edit_id} to {},{} {}x{}",
            work.0 + (work.2 / 2) as i32,
            work.1,
            work.2 / 2,
            work.3
        ),
    )?;
    session.expect(&format!("nxedit: resized to {}x{}", work.2 / 2, work.3))?;
    println!("  ok: the editor snapped to the right half, clear of the browser");

    session.send("touch ./papers/other.txt")?;
    session.expect("/home>")?;
    // **A third file the browser must not show**: three entries listed as two is the only way to
    // tell hiding from a directory with two files.
    session.send("touch ./papers/.quiet.txt")?;
    session.expect("/home>")?;

    let (_, fx, fy, _fw, _fh) = files_win;
    // **On the path strip, not the title bar** (a `StartMove`), **and clear of the sidebar**: over
    // the places a slip navigates, over the listing it only selects a row.
    click_at(qmp, session, fx + SIDEBAR_W + 60, fy + 1 + TITLE_BAR_H + MENU_BAR_H + PATH_H / 2)?;
    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;
    select_papers(qmp, session)?;
    press(qmp, "ret")?;
    // **The receipt says both numbers**: "(1 hidden)" says three were read and two shown.
    session.expect("nxfiles: listed /home/papers - 2 entries (1 hidden)")?;
    println!("  ok: three files in the directory, two of them listed");

    // **`Ctrl+H` shows them, and again hides them.** The guest's count is the one that can fail:
    // host tests do not read a real directory through a real server. **Both directions**: a toggle
    // that did not work back would leave every later count one too high.
    qmp.send_key("ctrl", true)?;
    press(qmp, "h")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: listed /home/papers - 3 entries")?;
    println!("  ok: Ctrl+H showed the hidden one");
    qmp.send_key("ctrl", true)?;
    press(qmp, "h")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: listed /home/papers - 2 entries (1 hidden)")?;
    println!("  ok: and hid it again, leaving the directory as the rest of this gate expects");

    let row1 = (fx + SIDEBAR_W + 120, fy + TITLE_BAR_H + MENU_BAR_H + PATH_H + browser::HEADER_H + ROW_H + ROW_H / 2);
    move_pointer_to(qmp, row1.0, row1.1)?;
    qmp.pointer = Some(row1);
    qmp.send_button("left", true)?;
    // **Past the slop, then across.** **The slop counts toward where the pointer is**: injection is
    // relative, and a walk starting from `row1` as though these motions had not happened ended
    // clamped against the screen's edge.
    let mut at = row1;
    for _ in 0..6 {
        qmp.send_motion(100, 40)?;
        at = (at.0 + 100, at.1 + 40);
    }
    // **Asserted directly, not retried**: a repaint between press and release re-identifies the row
    // and `libui` drops the gesture. **In either order**: two processes answer one gesture.
    session.expect_all(&["nxfiles: dragging other.txt", "compositor: drag from window "])?;
    let onto = (work.0 + work.2 as i32 * 3 / 4, work.1 + work.3 as i32 / 2);
    while at != onto {
        let step = ((onto.0 - at.0).clamp(-100, 100), (onto.1 - at.1).clamp(-100, 100));
        qmp.send_motion(step.0, step.1)?;
        at = (at.0 + step.0, at.1 + step.1);
    }
    qmp.send_button("left", false)?;
    qmp.pointer = Some(onto);
    expect_after_pointer(qmp, session, &format!("compositor: drop win={edit_id} on=document"))?;
    session.expect("nxedit: drop of other.txt on the document")?;
    session.expect("nxedit: opened /home/papers/other.txt - 0 bytes")?;
    println!("  ok: a file dragged from the browser opened in the editor");

    st.edit_id = edit_id;
    st.fx = fx;
    st.fy = fy;
    Ok(())
}
