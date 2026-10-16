use crate::*;

/// `check-login`'s step 7: the file browser, and the two sessions used as one fact.
pub(crate) fn login_file_browser(qmp: &mut Qmp, session: &mut Session, st: &mut LoginState) -> R<()> {
    let shell_px = st.shell_px;
    // 7. **The file browser, and the two sessions used as one fact** (M10 Part B). The serial
    //    shell just logged in above, and it sees the *same* `/home` subtree the graphical
    //    session does — `libsession::build_namespace` and `desktop-shell::build_app_namespace`
    //    bind it identically, which is the assumption M10's decision 1 rests on. So the serial
    //    side can *make* the thing the graphical side is then asserted to see, which is a
    //    stronger gate than a fixture: nothing here is arranged by the harness.
    session.send("mkdir ./papers")?;
    session.expect("/home>")?;

    click_at(qmp, session, APPS_CLICK.0, APPS_CLICK.1)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(qmp, session, "nxfiles")?;
    press(qmp, "ret")?;
    // **The theme reached the application** (M11 Part C): a value that travelled from a file on
    // disk, through one reader in the shell, onto the setup record every launch already carries,
    // and into a window. It is asserted here because it is the first thing the client says — it
    // reads what the session told it before it reads a directory.
    // **The same number at both ends**, which is what says the theme crossed the wire rather than
    // each end reaching for its own default.
    //
    // **Unordered against the shell's own line**, for the reason the `nxedit` launch below
    // already carries and this one did not: the shell logs *after* `send_setup_env` returns, so
    // the application is already running and can reach the serial console first. Held as a
    // sequence, this passed for two milestones and then failed in CI on 2026-09-03 — timing out
    // on `nxfiles: theme font_px 14` while that exact line sat in the transcript it printed,
    // which is the symptom `expect_all`'s own doc predicts.
    //
    // **The race was always there; what changed is only how often it is lost.** M13 Part C grew
    // every window's damage by a shadow's radius, so the shell has more repainting to do around
    // the launch — a plausible nudge and not a proven one. Worth stating that way round, because
    // "the change caused it" would suggest reverting something, and what was actually wrong is
    // an assertion that asked the system to guarantee an order it never had.
    let launched = String::from("desktop-shell: launched nxfiles into its own namespace");
    let themed = format!("nxfiles: theme font_px {shell_px}");
    session.expect_all(&[&launched, &themed])?;
    // **And that it is the staged one**, which is what gates the *file*. `THEME_FONT_PX` is not
    // the built-in size, so a shell that stopped putting the theme on the setup record — or a
    // client that stopped reading it — reports 16 and fails here. **This is the one line the
    // "delete the theme file" control removes**, because that control is about the run where no
    // file exists.
    if shell_px != u32::from(THEME_FONT_PX) {
        return Err(format!(
            "the session resolved font_px {shell_px}, but the image staged {THEME_FONT_PX} — \
             the shell did not read the file the build wrote"
        )
        .into());
    }
    // **It starts at `HOME`**, which is the binding the shell gave it and not a path compiled
    // in. The count is read rather than asserted: what is in a user's home is the image's
    // business, and a gate that pinned it would fail the first time anything else wrote there.
    session.expect("nxfiles: listed /home - ")?;
    let listed = session.rest_of_line()?;
    println!("  ok: nxfiles started at HOME and listed it ({})", listed.trim());

    // **Where the shell put it**, read after the listing rather than before it: a client lists
    // its directory *then* creates its window, so the shell's geometry line comes second — and
    // asking for it first consumed the listing this step asserts. The gate presses on a row
    // later, and a row's position is the window's origin plus chrome; re-deriving the shell's
    // placement cascade here would be a second copy of a policy that is the shell's to change.
    let files_win = next_geometry(session)?;

    // **Enter descends into the selected row**, which is the keyboard reaching the same message a
    // row press produces.
    //
    // **Arrowed to rather than assumed.** This used to press Enter on row 0 and its comment said
    // that would fail loudly "if `/home` ever holds a directory sorting before `papers`" — M14
    // Part D staged three, so it does. Each arrow is acknowledged by the browser's `selected`
    // receipt, which is both a per-keystroke acknowledgement and the assertion that the arrow
    // moved where this thinks it did.
    select_papers(qmp, session)?;
    press(qmp, "ret")?;
    session.expect("nxfiles: listed /home/papers - 0 entries")?;
    println!("  ok: Enter descended, and an empty directory lists nothing");

    // And Backspace leaves it again. Two navigations, both from the keyboard, because a
    // listing a person cannot drive without aiming at it is a listing they have to aim at.
    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;
    println!("  ok: Backspace went back up");

    // **The location bar** (M14 Part D). `Ctrl+L` opens it on where the tab is, the path is
    // retyped, and `Enter` goes there — which is the whole chain: the chord reached the browser,
    // the keys went to the *field* rather than to the listing behind it, and what was typed became
    // a navigation.
    //
    // **A receipt per character**, the discipline every typed sequence in this gate follows.
    qmp.send_key("ctrl", true)?;
    press(qmp, "l")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: location so far 5 chars")?;
    println!("  ok: Ctrl+L opened the bar seeded with /home");

    // Clear the five it was seeded with. **Backspace here must not go up a directory**, which is
    // the property the emptying itself proves: a browser whose listing still had the keyboard
    // would navigate rather than edit, and the counts below would never arrive.
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
    // **The bar closes before the listing arrives, and the order is not a choice.** The receipt
    // is emitted from the render pass, which runs a full turn of the loop before the pending
    // navigation is taken — so `closed` is on the wire first. `Session::expect` consumes what it
    // scans past, so waiting for the listing first eats the close and then hangs on it; the first
    // version of this step did exactly that.
    session.expect("nxfiles: location bar closed")?;
    session.expect("nxfiles: listed /home/papers - ")?;
    println!("  ok: and what was typed into it became a navigation");

    // Back to `/home`, which is where the steps below expect to be.
    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    // **Properties** (M14 Part D). `Ctrl+I` on the selected row, then `Esc`.
    //
    // **What only a boot can say is that it *drew*.** A dialog whose tree is undiffable opens,
    // reports its size and never paints a frame — the failure M14 Part C spent a boot on — and
    // from outside that is indistinguishable from one that opened and closed. So both ends are
    // asserted, and the slice between them is checked for the browser's own complaint.
    let before_props = session.transcript().len();
    qmp.send_key("ctrl", true)?;
    press(qmp, "i")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: showing properties")?;
    // **Pressed once the dialog has the keyboard, not once it was asked for.** `showing` is
    // printed before the window exists, and while the shell is still placing it a key goes to
    // the browser — an `Esc` there does nothing, and the dialog never closes. A probe that held
    // the shell's placement for half a second failed this step exactly that way.
    session.expect("nxfiles: properties has the keyboard")?;
    press(qmp, "esc")?;
    session.expect("nxfiles: properties closed")?;
    let props_window = session.transcript()[before_props..].to_string();
    if props_window.contains("could not be drawn") || props_window.contains("could not open") {
        let _ = session.child.kill();
        return Err(format!("the properties dialog opened and complained: {props_window:?}").into());
    }
    println!("  ok: Ctrl+I showed the properties dialog, and Esc closed it");

    // **And the margin around it** (M15 Part E), which moved every sidebar row down by its height
    // without moving a listing row at all. The design's sidebar runs to the window's edge, so
    // since the refresh's Part I that margin is nought.
    const SIDEBAR_PAD: i32 = browser::SIDEBAR_PAD;
    // **No tab strip while there is one tab** (desktop refresh, Part I): the browser leaves it
    // out rather than drawing an empty one, so every row moved up by its height — and **a column
    // header sits above the rows now**, so they moved back down by its. `nxfiles::list_top` is
    // the browser's own version of this sum.
    // Where the window's content begins, below the chrome that spans its whole width.
    let content_top = files_win.2 + TITLE_BAR_H + MENU_BAR_H + PATH_H;
    let row_y = |row: i32| content_top + browser::HEADER_H + row * ROW_H + ROW_H / 2;
    // **A sidebar row is not a listing row's neighbour any more** (desktop refresh, Part I): the
    // column header sits above the *listing* and the sidebar starts at the content's top, so the
    // two are a header apart. This was `row_y(row) + SIDEBAR_PAD`, which after the header landed
    // a whole row low — the gate pressed `Downloads` while asking for `Documents`, and said so.
    // `SIDEBAR_PAD` is nought since the same part, the design's sidebar running to the edge.
    let side_y = |row: i32| content_top + SIDEBAR_PAD + row * ROW_H + ROW_H / 2;

    // **The sidebar** (M14 Part D). One press on *Documents* goes there — one, not two: a sidebar
    // row has nothing to select and no second verb, so the listing's double click would be a rule
    // copied past its reason.
    //
    // **This is also what keeps two spellings of the folder names in step.** `libfs` names them
    // in `HOME_FOLDERS` and the image build stages them from this crate's `HOME_FOLDERS`, and this
    // crate does not link a guest library to compare them — so a folder staged under another name
    // answers "no such directory" here rather than differing silently.
    //
    // Sidebar rows are Home, the three folders, then Root; they sit `SIDEBAR_PAD` below the
    // listing rows beside them, because the panel is inset from the content it shares an edge
    // with while the strips above span the full width.
    click_at(qmp, session, files_win.1 + 40, side_y(1))?;
    session.expect("nxfiles: listed /home/Documents - ")?;
    println!("  ok: one press on a sidebar row went there");
    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    // **Cut, copy and paste of files** (M14 Part D), which is the file-clipboard deferral built: a
    // path crosses `/dev/clipboard` between two directories and the **serial** side reads the
    // result back. Asserted through the filesystem rather than through a log line, for step 9d's
    // reason — a paste that acted on the wrong path makes a differently-named file rather than a
    // matching count.
    //
    // Row 4 is `theme.toml`, the first file after the three directories.
    click_at(qmp, session, files_win.1 + SIDEBAR_W + 120, row_y(4))?;
    session.expect("nxfiles: selected theme.toml")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "c")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: clipboard now holds 1 path(s) to copy")?;
    println!("  ok: Ctrl+C put a path on the clipboard");

    // Into an empty directory, by the sidebar, and paste there.
    click_at(qmp, session, files_win.1 + 40, side_y(1))?;
    session.expect("nxfiles: listed /home/Documents - 0 entries")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "v")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: copied /home/Documents/theme.toml")?;
    session.expect("nxfiles: listed /home/Documents - 1 entries")?;
    println!("  ok: and Ctrl+V copied it into another directory");

    // **The serial column is what says it happened**, because asking the browser would be asking
    // the program that claims to have done it.
    session.send("list ./Documents")?;
    session.expect("theme.toml")?;
    session.expect("/home>")?;
    println!("  ok: and the shell sees the file the paste made");

    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;

    // **A single click selects, a double click opens** (M14 Part D, decision 5).
    //
    // **This is the only place the interaction exists.** The gate navigates with `Enter` and drags
    // with a press-and-move, so neither touches what a *click* means; the host tests pin the
    // counting and the select-versus-open split, and what they cannot say is that a real pointer's
    // two presses arrive close enough together in the guest to be one run.
    //
    // **The single click aims at a *file*, and that is the assertion.** `/home` lists `papers`
    // first because directories lead, then `theme.toml` and `wallpaper.png` — so row 1 is a file,
    // and before this part clicking it asked the shell to open it. The receipt is change-driven,
    // which is why the row cannot be row 0: a fresh listing selects that one already, so a click
    // on it changes nothing and reports nothing.
    // Rows: `Documents`, `Downloads`, `Pictures`, `papers`, then `theme.toml` and
    // `wallpaper.png` — directories lead, so row 4 is the first file.
    let before = session.transcript().len();
    let file_row = (files_win.1 + SIDEBAR_W + 120, row_y(4));
    click_at(qmp, session, file_row.0, file_row.1)?;
    session.expect("nxfiles: selected theme.toml")?;
    println!("  ok: one click on a file selected it");

    // **And did not open it**, which `expect` cannot say on its own: it scans forward, so the very
    // line that would prove failure is one it would skip. The transcript since before the click is
    // checked for the request instead.
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

    // Two clicks on the directory row descend into it.
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

/// `check-login`'s steps 8 to 9: the editor opened by the browser, undo and find, and a file
/// dropped onto it.
pub(crate) fn login_editor(qmp: &mut Qmp, session: &mut Session, size: DisplaySize, st: &mut LoginState) -> R<()> {
    let files_win = st.files_win;
    let work = st.work;
    // 8. **The editor, opened by the browser, and read back by the shell** (M10 Part D). The
    //    same two-session fact as step 7, used the other way round: there the serial side made
    //    something the graphical side had to see; here the graphical side writes something the
    //    serial side has to read. Nothing in between is arranged by the harness.
    //
    //    **The file is made empty and inside `papers`**, which is what makes the listing
    //    deterministic: `/home` holds whatever the image put there, but `papers` was created by
    //    step 7 and holds exactly this.
    session.send("touch ./papers/notes.txt")?;
    session.expect("/home>")?;

    // **The selection is on `Documents` again**, because the `Backspace` above re-listed `/home`
    // and a fresh listing selects row 0.
    select_papers(qmp, session)?;
    press(qmp, "ret")?;
    session.expect("nxfiles: listed /home/papers - 1 entries")?;

    // **Enter on a file row asks the shell to open it**, which is Part B's box, moved here
    // because it needed something to launch. The browser holds no authority to spawn anything —
    // it names a path over `/dev/desktop` and the shell decides what opens it.
    press(qmp, "ret")?;
    session.expect("desktop-shell: launched nxedit into its own namespace")?;
    // **Two processes reacting to one launch, so no order between them**: the browser learns
    // its request was taken when the shell's reply arrives, and the editor reads the file when
    // the kernel gets round to running it.
    let asked = String::from("nxfiles: asked to open /home/papers/notes.txt");
    // **The language is on this line since M14 Part G**, and asserting it is what says the
    // shipped binary ran `syntax::for_name` at all. `.txt` is plain text, which is a supported
    // answer rather than a gap — the four languages this system contains are host-tested, and
    // what only a boot can add is that the dispatch happens in the binary that ships.
    let opened = String::from("nxedit: opened /home/papers/notes.txt - 0 bytes, highlighted as plain");
    session.expect_all(&[&asked, &opened])?;
    // After the open, for the reason the browser's is read after its listing: the editor reads
    // its file before it has a window to place.
    let edit_win = next_geometry(session)?;
    println!("  ok: a file row launched the editor on the file it names");

    for (i, c) in TYPED.chars().enumerate() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect(&format!("nxedit: buffer rev {}", i + 1))?;
    }
    println!("  ok: {} keystrokes reached the editor's buffer", TYPED.len());

    // Ctrl+S, and the editor says what it wrote. Seven bytes: six typed and the newline the
    // editor adds, because a last line without one reads as one line shorter than it looks.
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: saved /home/papers/notes.txt - 7 bytes")?;

    // **And the assertion is made from outside the editor.** Asking the editor what it saved
    // would be asking the accused: it would answer from the buffer it holds, which is the thing
    // in doubt. The serial session reads the file with `nxsh`'s `open` — there is no `cat`; a
    // path is a stream — and what the shell prints is what is on disk.
    session.send("open ./papers/notes.txt")?;
    session.expect(TYPED)?;
    session.expect("/home>")?;
    println!("  ok: the shell read back what the editor saved, from outside it");

    // 8b. **Undo, redo, and find** (M12 Part C). The grouping is the decision this part makes,
    //     and the gate asserts it the only way that cannot be argued with: **by byte count, from
    //     outside**. Two characters typed together are one group, so one undo takes both — seven
    //     bytes on disk rather than eight — and a redo brings both back to nine. An
    //     implementation that grouped per *keystroke* would leave **eight** here; nine is what an
    //     undo that took nothing leaves; and one that grouped per *save* would leave seven after
    //     the redo as well. (This paragraph said nine for the per-keystroke case, twice, two
    //     lines after getting it right — PR #269 review, worth fixing 4.)
    for c in "ab".chars() {
        let mut qcode = String::new();
        qcode.push(c);
        press(qmp, &qcode)?;
        session.expect("nxedit: buffer rev ")?;
    }

    // Ctrl+Z. The revision moves on an undo, which is what makes an undone buffer read as
    // modified — the editor has to ask before closing something that is no longer the file.
    qmp.send_key("ctrl", true)?;
    press(qmp, "z")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxedit: buffer rev ")?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "s")?;
    qmp.send_key("ctrl", false)?;
    // Seven: `nitrox` and the newline the editor adds. Eight would mean the undo took one
    // character rather than the group; nine would mean it took nothing.
    session.expect("nxedit: saved /home/papers/notes.txt - 7 bytes")?;
    session.send("open ./papers/notes.txt")?;
    session.expect(TYPED)?;
    session.expect("/home>")?;
    println!("  ok: one undo took the whole group, and the shell read back the shorter file");

    // Ctrl+Y brings it back, and the file grows by exactly what the undo removed.
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

    // **Find is the second thing that wants the keys** — the shape the save-as field
    // established, which is why it is the same field. What is typed goes to it and not to the
    // buffer, and the receipt is where the match landed rather than what was looked for.
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
    // Escape ends the mode. **Not asserted here, and this comment used to say it was** — nothing
    // types into this editor again: step 9 drags its title bar with the pointer, and the click
    // after that hands the keyboard to the browser, so a find field left open would change
    // nothing any later step can see (PR #269 review, worth fixing 3). What covers it is
    // `nxedit`'s own `ctrl_f_opens_a_find_field_and_enter_walks_the_matches`, which fails when
    // `Esc` stops closing a find field. The press is kept because leaving the editor in a mode
    // the rest of the run does not expect is worse than a keystroke nobody checks.
    press(qmp, "esc")?;
    println!("  ok: find took the keys, matched, and gave them back");

    // 9. **A file dragged from the browser onto the editor** (M10 Part E) — the part every
    //    application above exists to make honest: two real windows, one payload, and no test
    //    client on either end.
    //
    //    **The editor is snapped to the right half first**, because the shell cascades new
    //    windows and the editor is sitting on top of the browser it was launched from. Dropping
    //    a window at the screen's right edge is M9 Part F's own gesture, so the geometry the
    //    drag then uses is the work area's half rather than a number this gate invented.
    let (edit_id, ex, ey, ew, _eh) = edit_win;
    let title_grab = (ex + ew as i32 / 2, ey + chrome::TITLE_Y);
    move_pointer_to(qmp, title_grab.0, title_grab.1)?;
    qmp.pointer = Some(title_grab);
    qmp.send_button("left", true)?;
    for _ in 0..6 {
        qmp.send_motion(120, 0)?;
    }
    session.expect("compositor: interactive move of window ")?;
    // To the right edge, which is where the shell registered a snap zone — **a screen's width of
    // travel, into the clamp, and paced**. Six more motions of 120 reached it from here on 1280 and
    // 1360 and stopped 176 px short on 1920, where the drop snapped nowhere; twenty-three unpaced
    // ones overran the input ring on 2560, and the dropped batch ended the gesture before the
    // release — the pacing the left-edge drag above already explains (Phase 5 Part E.4).
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

    // A second file, so the drag carries something the editor is not already showing — dropping
    // the open file is deliberately a no-op, and a gate that did it would assert nothing.
    session.send("touch ./papers/other.txt")?;
    session.expect("/home>")?;
    // **A third file the browser must not show** (M14 Part D). A dot is a convention meaning
    // "not part of what this directory is for", and the assertion below is that `/home/papers`
    // holds three entries while the browser lists two — which is the only way to tell hiding
    // from a directory that happens to have two files in it.
    session.send("touch ./papers/.quiet.txt")?;
    session.expect("/home>")?;

    // **Click the browser to give it the keyboard**, then walk it out and back in so it lists
    // the file the serial side just made. A listing is read when something navigates; nothing
    // polls, deliberately.
    let (_, fx, fy, _fw, _fh) = files_win;
    // **On the path strip, not the title bar.** A press on a title bar is a `StartMove` — the
    // click would work and the window would not move, but the gate would be asserting against a
    // gesture it did not mean to make. The path text carries no handler and still raises the
    // window, because click-to-focus is the compositor's and not the toolkit's.
    // **On the path strip**, which is two strips down: a title bar and the menus. It was three
    // until the refresh's Part I, when the browser stopped drawing a tab strip for a single tab
    // — and an aim that still counted one landed on the *sidebar's* first place, so the "inert"
    // click navigated to `/home` and the `Backspace` after it went up to `/`. **This gate passed
    // locally anyway**, because that Backspace was lost on the way; CI's landed, and the step
    // walked into the wrong directory. `fy + 38` used to be the path and is the *menu bar* since
    // M12 Parts B and D — it still raised the window, because click-to-focus is the compositor's
    // and the bar's own background carries no handler, so this went on passing while meaning
    // something else. The same shape of mistake twice, which is why the sum lives in `browser`.
    // **And clear of the sidebar**, which is what turned a slip into a navigation: over the
    // places, a press one strip out goes somewhere; over the listing, the worst a slip can do is
    // select a row, because descending takes a second click. An inert click should be inert
    // under a mistake as well as under a correct sum.
    click_at(qmp, session, fx + SIDEBAR_W + 60, fy + 1 + TITLE_BAR_H + MENU_BAR_H + PATH_H / 2)?;
    press(qmp, "backspace")?;
    session.expect("nxfiles: listed /home - ")?;
    // **The third step to depend on which row a fresh listing selects**, and the one that never
    // said so: it walks out of `papers` and straight back in, which worked only while `papers`
    // was row 0. The two before it at least named the row.
    select_papers(qmp, session)?;
    press(qmp, "ret")?;
    // **The receipt says both numbers, and that is the assertion.** "2 entries" alone cannot tell
    // a filtered listing from a directory that happens to hold two; "(1 hidden)" is the browser
    // stating that it read three and is showing two.
    session.expect("nxfiles: listed /home/papers - 2 entries (1 hidden)")?;
    println!("  ok: three files in the directory, two of them listed");

    // **`Ctrl+H` shows them, and again hides them** (M14 Part D). The count is the assertion and
    // the *guest's* count is the only one that can fail here: the host tests already pin that a
    // dot is filtered, that the toggle asks for a listing and that the View menu's mark tracks
    // the flag — what none of them can say is that a real directory read through a real server
    // comes back with the entry in it.
    //
    // **Both directions, and the second is not tidying.** Hiding drops entries rather than
    // skipping them, so showing them again is a *listing* rather than a flag; a toggle that
    // worked once and not back would leave every later count in this gate one too high, which is
    // how it would be discovered — several hundred lines away, as a rename that seemed to add a
    // file.
    qmp.send_key("ctrl", true)?;
    press(qmp, "h")?;
    qmp.send_key("ctrl", false)?;
    // No suffix here: with nothing hidden, what was read and what is shown are the same number,
    // and the receipt says so by not mentioning a difference.
    session.expect("nxfiles: listed /home/papers - 3 entries")?;
    println!("  ok: Ctrl+H showed the hidden one");
    qmp.send_key("ctrl", true)?;
    press(qmp, "h")?;
    qmp.send_key("ctrl", false)?;
    session.expect("nxfiles: listed /home/papers - 2 entries (1 hidden)")?;
    println!("  ok: and hid it again, leaving the directory as the rest of this gate expects");

    // Row 1 is `other.txt`: the listing sorts directories first and then by name, and `notes`
    // sorts before `other`. The row's y is the window's origin plus its chrome — the title bar,
    // the menus, the path strip and the column header — plus a row and a half. **No tab strip**,
    // which the browser leaves out for a single tab (desktop refresh, Part I), and **a header**,
    // which it gained in the same part.
    let row1 = (fx + SIDEBAR_W + 120, fy + TITLE_BAR_H + MENU_BAR_H + PATH_H + browser::HEADER_H + ROW_H + ROW_H / 2);
    move_pointer_to(qmp, row1.0, row1.1)?;
    qmp.pointer = Some(row1);
    qmp.send_button("left", true)?;
    // **Past the slop, then across.** The browser turns a press into a drag once it has
    // travelled, which is what keeps a click a click.
    //
    // **The slop counts toward where the pointer is**, and it did not until M11 Part E batch 2b
    // found out. Injection is relative: these six motions move the guest's pointer 600 across
    // and 240 down, and the walk below started its arithmetic from `row1` as though they had
    // not happened — so every step was 600 too far right, and the pointer ended clamped against
    // the screen's right edge instead of at `onto`.
    //
    // It passed for a month because the editor's text area reached the window's last pixel
    // column, so a drop at the extreme edge landed on it anyway. Giving the window a frame moved
    // the content in by four pixels and the drop started landing on the frame — a real gate bug,
    // surfaced by a change that had nothing to do with it.
    let mut at = row1;
    for _ in 0..6 {
        qmp.send_motion(100, 40)?;
        at = (at.0 + 100, at.1 + 40);
    }
    // **Asserted directly, not retried.** Like the menu click below, this was flaky for exactly
    // one reason — a repaint between the press and the release re-identified the row under the
    // pointer and `libui` dropped the gesture — and the retry that hid it is gone with the bug.
    // **In either order** (Phase 6 Part B's `check-login --usb`, under TCG): `nxfiles` says it is
    // dragging after it asks the compositor to start the drag, and the compositor says so when the
    // request arrives — two processes answering one gesture, ordered by nothing. Expected in turn,
    // the compositor's line came first once and the first `expect` scanned past it.
    session.expect_all(&["nxfiles: dragging other.txt", "compositor: drag from window "])?;
    // Into the editor's document area — below its title bar and status strip, and well inside
    // the half of the screen it now occupies.
    let onto = (work.0 + work.2 as i32 * 3 / 4, work.1 + work.3 as i32 / 2);
    while at != onto {
        let step = ((onto.0 - at.0).clamp(-100, 100), (onto.1 - at.1).clamp(-100, 100));
        qmp.send_motion(step.0, step.1)?;
        at = (at.0 + step.0, at.1 + step.1);
    }
    // **Letting go is what delivers it.** The highlight the compositor draws while the pointer
    // is over a window that takes the payload is pixels rather than a line — its logic is
    // host-tested in `compositor::input`, including the two cases that must show *nothing*.
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
