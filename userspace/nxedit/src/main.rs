//! `nxedit` — the text editor: the window, the file, and the event pump.
//!
//! The library half holds every behaviour and is host-tested; this half needs an OS: reading
//! the file, writing it back safely, `/dev/draw`, shared buffers, input.
//!
//! **How a save fails.** [`save`] writes a temporary beside the target and renames it over. A
//! save that truncated and then failed would leave neither the old file nor the new one; a
//! rename either happened or did not, and the buffer on screen is kept whatever the answer.
//!
//! The path is `argv[1]`, which `desktop-shell` sets when a client asks it to open something.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::geom::{Rect, Size};
use libdraw::text::{Font, load_mono, load_ui};
use libkern::debug::Line;
use libkern::{exit, kprint};
use librsproto::clipboard::{CLIP_ANY_SERIAL, CLIP_KIND_TEXT, Clipboard, MAX_CLIP_BYTES};
use librsproto::surface::Role;

use libsurface::{Session, WindowEvent, ipc::ChannelTransport};

use libui::layout::{Metrics as _, layout};
use libui::paint::{FontMetrics, Theme};

use libui::menu::{Item, KeyOutcome};
use libui::window::Child;
use nxedit::{App, Msg, to_bytes};

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const BUFFERS: usize = 2;

fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: under the test-harness kernel this terminates QEMU; elsewhere it is
    // `Unsupported` and falls through.
    unsafe { libkern::syscall1(libkern::SYS_TEST_EXIT, libkern::TEST_EXIT_FAILURE as u64) };
    exit(1);
}

/// Read the file into `app`, or say why it could not be. A missing path is not a failure; a
/// directory or a failed read blocks the buffer, since saving the empty window would destroy it.
fn open_into(app: &mut App, ns: u64, path: &str) {
    if libfs::is_dir(ns, path.as_bytes()) {
        app.blocked("that is a directory");
        kprint(b"nxedit: refusing to edit a directory\n");
        return;
    }
    match libfs::read_file(ns, path.as_bytes()) {
        Ok(bytes) => match core::str::from_utf8(&bytes) {
            Ok(text) => {
                app.loaded(text, &bytes);
                // The language rides on this line so a gate that waits for the open learns it;
                // appended, so expectations matching the prefix still match.
                libkern::debug::Line::new()
                    .s(b"nxedit: opened ")
                    .untrusted(path.as_bytes())
                    .s(b" - ")
                    .u(bytes.len() as u64)
                    .s(b" bytes, highlighted as ")
                    .s(app.language_name().as_bytes())
                    .end();
            }
            // Refused rather than mangled: saving a lossy conversion would write it back.
            Err(_) => {
                app.blocked("not text");
                kprint(b"nxedit: refusing to edit a file that is not UTF-8\n");
            }
        },
        Err(libfs::FileError::NotFound) => {
            app.absent();
            libkern::debug::Line::new().s(b"nxedit: new file ").s(path.as_bytes()).end();
        }
        Err(_) => {
            app.blocked("could not be read");
            libkern::debug::Line::new().s(b"nxedit: cannot read ").s(path.as_bytes()).end();
        }
    }
}

/// What the taskbar should call this window: "untitled" rather than the compositor's
/// `window 20` fallback. Set once.
fn window_title(path: &str) -> &str {
    if path.is_empty() { "untitled" } else { libfs::basename_str(path) }
}

/// Write `bytes` to `path` with [`libfs::save_file`]. The error is the sentence the status strip
/// shows.
fn save(ns: u64, path: &str, bytes: &[u8]) -> Result<usize, &'static str> {
    match libfs::save_file(ns, path.as_bytes(), bytes) {
        Ok(()) => Ok(bytes.len()),
        Err(libfs::SaveError::Write(_)) => Err("the file could not be written"),
        Err(libfs::SaveError::Replace(_)) => Err("the file could not be replaced"),
    }
}

fn wait_one(h: u64) {
    let handles = [h];
    let mut results = [0u8; 24];
    // SAFETY: a valid one-handle array and a result buffer sized for one record.
    unsafe { libkern::syscall4(libkern::SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
}

fn dress<T: libsurface::Transport>(win: &mut Session<T>, id: u32, path: &str) {
    if let Some(mut w) = win.window(id)
        && w.set_title(window_title(path)).is_err()
    {
        kprint(b"nxedit: SetTitle refused\n");
    }
    // Files only: a directory has no contents to put in a buffer. The compositor matches this
    // while the pointer moves, so a drag carrying a folder is never highlighted here.
    if let Some(mut w) = win.window(id)
        && w.declare_acceptor(nxedit::ACCEPTOR, librsproto::surface::DROP_KIND_FILE).is_err()
    {
        kprint(b"nxedit: DeclareAcceptor refused\n");
    }
}

/// The theme the shell handed this application, or the built-in one (absent is normal).
fn theme_of(env: &libstream::wire::Record) -> Theme {
    env.field_str("THEME").map(|s| Theme::from_config(s).0).unwrap_or_default()
}

/// Push `bytes` onto the kill ring. Connects per operation, not holding a wait-set slot.
fn clip_copy(ns: u64, bytes: &[u8]) -> Result<(), &'static str> {
    let mut buf = [0u8; libkern::abi::IPC_MSG_SIZE];
    let mut clip = Clipboard::connect(ns, &mut buf).map_err(|_| "no /dev/clipboard")?;
    let r = clip.copy(CLIP_KIND_TEXT, bytes).map(|_| ());
    clip.close();
    r.map_err(|_| "the clipboard refused it")
}

/// Read ring entry `index`, continuing from `expect` — see `librsproto::clipboard::ClipPaste`.
/// `Ok(None)` is no entry there; the stale case has its own message, since the answer is to
/// start again.
fn clip_read(ns: u64, index: u32, expect: u64, out: &mut [u8]) -> Result<Option<(u64, usize)>, &'static str> {
    let mut buf = [0u8; libkern::abi::IPC_MSG_SIZE];
    let mut clip = Clipboard::connect(ns, &mut buf).map_err(|_| "no /dev/clipboard")?;
    let r = clip.paste(index, expect, out);
    clip.close();
    match r {
        Ok((serial, _, len)) => Ok(Some((serial, len.min(out.len())))),
        Err(e) if e.is_empty() => Ok(None),
        Err(e) if e.is_stale() => Err("the clipboard changed"),
        Err(_) => Err("the clipboard refused"),
    }
}

/// Entry point, called by the kernel's ELF entry with the standard bootstrap arguments.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, endpoint: u64, arg0: u64) -> ! {
    kprint(b"nxedit: up\n");

    let (argv, env) = match libstream::setup::bootstrap(notif, root_ns, endpoint, arg0).setup() {
        Some(Ok(s)) => (s.argv, s.env),
        _ => (Vec::new(), libstream::wire::Record::default()),
    };
    // `argv[1]`, or an untitled buffer whose name is asked for in the status strip.
    let path = argv.get(1).cloned().unwrap_or_default();
    // Where an untitled buffer is saved; `/home` outside a session.
    let home = alloc::string::String::from(env.field_str("HOME").filter(|h| !h.is_empty()).unwrap_or("/home"));

    // Read before the font: the theme names the file to load.
    let theme = theme_of(&env);
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let (font, _) = match unsafe { load_ui(root_ns, &theme, b"nxedit") } {
        Ok(loaded) => loaded,
        Err(e) => {
            libkern::debug::Line::new().s(b"nxedit: the UI font ").s(e.why()).end();
            fail(b"nxedit: font load FAILED\n");
        }
    };
    // The fixed-advance face for the buffer, a companion of the UI face. Fatal like it: a
    // silent fallback to the proportional face would be a window that looks subtly wrong.
    // SAFETY: as above.
    let font = match unsafe { load_mono(root_ns, &theme, b"nxedit") } {
        Ok((mono, _)) => font.with_mono(mono),
        Err(e) => {
            libkern::debug::Line::new().s(b"nxedit: the mono font ").s(e.why()).end();
            fail(b"nxedit: font load FAILED\n");
        }
    };

    let mut app = App::new(&path, &home);
    if !path.is_empty() {
        open_into(&mut app, root_ns, &path);
    }
    let size = app.window_size();
    // SAFETY: `root_ns` is this process's live root namespace.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => Box::new(t),
        Err(_) => fail(b"nxedit: connect to /dev/draw FAILED\n"),
    };
    let mut win = Session::new(transport);
    let top = {
        let ui = app.view(&theme, None);
        match Child::open_sized(&mut win, Role::Normal, (0, 0), size, &ui, &font, &theme, BUFFERS) {
            Some(t) => t,
            None => fail(b"nxedit: CreateWindow FAILED\n"),
        }
    };
    dress(&mut win, top.id(), &path);

    let bounds = Rect::new(0, 0, size.w, size.h);
    // The question's window is alive only while asked: a `dialog` is transient by role, and a
    // hidden one would still be a window in the stack.
    let confirm: Option<Child> = None;
    // The menu's window likewise; it holds the keyboard while it is up.
    let menu: Option<Child> = None;
    let menu_shown: Option<usize> = None;
    let menu_hovered: Option<u64> = None;
    let confirm_hovered: Option<u64> = None;
    let ev = win.wait_handle();
    let reported = app.revision();
    let reported_name = app.naming_len();
    let reported_title = String::from(window_title(&path));
    let reported_tab = app.current_tab();

    /// Everything one window of this editor is; the `Session`, font and theme are shared. Each
    /// receipt line carries an id so a gate can tell two windows' lines apart.
    struct Win {
        top: Child,
        app: App,
        size: Size,
        bounds: Rect,
        confirm: Option<Child>,
        confirm_hovered: Option<u64>,
        chooser: Option<Child>,
        chooser_hovered: Option<u64>,
        menu: Option<Child>,
        menu_shown: Option<usize>,
        menu_hovered: Option<u64>,
        reported: u64,
        reported_name: Option<usize>,
        reported_title: String,
        reported_tab: u64,
    }

    fn open_window<T: libsurface::Transport>(
        win: &mut Session<T>,
        mut app: App,
        font: &Font,
        theme: &Theme,
    ) -> Option<Win> {
        let size = app.window_size();
        let ui = app.view(theme, None);
        let top = Child::open_sized(win, Role::Normal, (0, 0), size, &ui, font, theme, BUFFERS)?;
        dress(win, top.id(), app.path());
        let reported = app.revision();
        let reported_name = app.naming_len();
        let reported_title = String::from(window_title(app.path()));
        let reported_tab = app.current_tab();
        Some(Win {
            top,
            app,
            size,
            bounds: Rect::new(0, 0, size.w, size.h),
            confirm: None,
            confirm_hovered: None,
            chooser: None,
            chooser_hovered: None,
            menu: None,
            menu_shown: None,
            menu_hovered: None,
            reported,
            reported_name,
            reported_title,
            reported_tab,
        })
    }

    let mut quit_pending = false;
    let mut wins = alloc::vec![Win {
        top,
        app,
        size,
        bounds,
        confirm,
        confirm_hovered,
        chooser: None,
        chooser_hovered: None,
        menu,
        menu_shown,
        menu_hovered,
        reported,
        reported_name,
        reported_title,
        reported_tab,
    }];

    loop {
        // Destructured so every name means what it meant with one window.
        let mut redraw_now = false;
        for entry in wins.iter_mut() {
            let Win {
                top,
                app,
                size: _,
                bounds,
                confirm,
                confirm_hovered,
                chooser,
                chooser_hovered,
                menu,
                menu_shown,
                menu_hovered,
                reported,
                reported_name,
                reported_title,
                reported_tab,
            } = entry;
            let window_id = top.id();
            // One line per edit, a count not the text: a gate on the release image cannot read
            // the window, and without a receipt a dropped keystroke surfaces as a wrong file
            // later. What somebody types is theirs.
            let rev = app.revision();
            if rev != *reported {
                *reported = rev;
                libkern::debug::Line::new().s(b"nxedit: buffer rev ").u(rev).end();
            }
            // The name field gets the same receipt (`revision` cannot see naming).
            let want_title = String::from(window_title(app.path()));
            if want_title != *reported_title
                && let Some(mut w) = win.window(window_id)
            {
                if w.set_title(&want_title).is_err() {
                    kprint(b"nxedit: SetTitle refused\n");
                }
                *reported_title = want_title;
            }
            // One line per tab landed on: all a gate can check of a tab switch.
            let showing = app.current_tab();
            if showing != *reported_tab {
                *reported_tab = showing;
                libkern::debug::Line::new()
                    .s(b"nxedit: tab ")
                    .u(showing)
                    .s(b" showing ")
                    .untrusted(app.path().as_bytes())
                    .end();
            }
            let named = app.naming_len();
            if named != *reported_name {
                *reported_name = named;
                if let Some(n) = named {
                    // Named by field: a gate waiting on "name so far" must not match a search.
                    let what = app.field_kind().map_or("field", nxedit::Field::label);
                    libkern::debug::Line::new()
                        .s(b"nxedit: ")
                        .s(what.as_bytes())
                        .s(b" so far ")
                        .u(n as u64)
                        .s(b" chars")
                        .end();
                }
            }
            let ui = app.view(&theme, top.hovered_key());
            let l = layout(&ui, *bounds, &FontMetrics::new(&font, theme.font_px));
            // Read every frame: before the first there is nowhere to put a popup.
            app.place_menus(&ui, &l);
            if !top.present_laid_out(&mut win, &ui, &l, &font, &theme) {
                fail(b"nxedit: the window could not be drawn\n");
            }

            // A search's receipt is where it landed, not the needle: `check-login` boots the
            // release image and has no rendered window to read.
            if let Some(hit) = app.take_find_report() {
                let mut l = libkern::debug::Line::new();
                match hit {
                    Some(line) => {
                        l.s(b"nxedit: find hit at line ").u(line as u64);
                    }
                    None => {
                        l.s(b"nxedit: find no match");
                    }
                }
                l.end();
            }

            if *menu_shown != app.menus.open() {
                if let Some(m) = menu.take() {
                    m.close(&mut win);
                }
                *menu_hovered = None;
                *menu_shown = app.menus.open();
                if let Some(which) = menu_shown {
                    match app.menus.anchor() {
                        Some(at) => {
                            let view = app.menu_view(*which, &theme, None);
                            *menu = Child::open(
                                &mut win,
                                Role::Popup { parent: window_id },
                                at,
                                &view,
                                &font,
                                &theme,
                                BUFFERS,
                            );
                        }
                        // No anchor means no layout yet; an unplaced menu beats one at 0,0.
                        None => *menu = None,
                    }
                    if menu.is_none() {
                        kprint(b"nxedit: could not open the menu\n");
                        app.menus.close();
                        *menu_shown = None;
                    }
                }
            }
            if let Some(m) = menu.as_mut() {
                let now = m.hovered_key();
                *menu_hovered = now;
                if let Some(which) = menu_shown {
                    let view = app.menu_view(*which, &theme, now);
                    if !m.present(&mut win, &view, &font, &theme) {
                        kprint(b"nxedit: the menu could not be drawn\n");
                    }
                }
            }

            // The application names the directory and this lists it: `libui::chooser` cannot
            // make the syscall. A chosen file opens in a tab, as a dropped one does.
            if let Some(path) = app.take_open()
                && app.accept_drop(&path)
            {
                open_into(app, root_ns, &path);
            }
            if let Some(dir) = app.take_chooser_list() {
                let rows = match libfs::list_dir(root_ns, dir.as_bytes()) {
                    Ok(mut entries) => {
                        libfs::sort(&mut entries, libfs::Order::NameAsc);
                        entries
                            .iter()
                            .filter_map(|e| {
                                let name = String::from_utf8_lossy(e.name()).into_owned();
                                // A leading dot is not shown, as in `nxfiles`: the two must
                                // list one directory the same way.
                                // TODO(chooser-hidden): no way to reach one from here. The browser
                                // has `Ctrl+H`; this has no menu to hang it on and needs its own
                                // chooser-level toggle.
                                let hidden = name.starts_with('.');
                                (!name.is_empty() && !hidden)
                                    .then_some((name, e.kind == librsproto::file::DIRENT_KIND_DIR))
                            })
                            .collect()
                    }
                    // An unreadable directory is shown empty, not refused: keep the person's place.
                    Err(_) => {
                        kprint(b"nxedit: could not list that directory\n");
                        alloc::vec::Vec::new()
                    }
                };
                app.show_chooser(&dir, rows);
                if chooser.is_some()
                    && let Some(c) = app.chooser()
                {
                    libkern::debug::Line::new()
                        .s(b"nxedit: the chooser moved to ")
                        .untrusted(c.dir.as_bytes())
                        .s(b" - ")
                        .u(c.entries.len() as u64)
                        .s(b" entries")
                        .end();
                }
            }
            match (app.chooser().is_some(), chooser.is_some()) {
                (true, false) => {
                    // Counts the entries: an empty list is deliberate for an unreadable directory.
                    if let Some(c) = app.chooser() {
                        libkern::debug::Line::new()
                            .s(b"nxedit: choosing a file in ")
                            .untrusted(c.dir.as_bytes())
                            .s(b" - ")
                            .u(c.entries.len() as u64)
                            .s(b" entries")
                            .end();
                    }
                    let view = app.chooser_view(&theme, None);
                    *chooser = Child::open(
                        &mut win,
                        Role::Dialog { parent: window_id },
                        (0, 0),
                        &view,
                        &font,
                        &theme,
                        BUFFERS,
                    );
                    if chooser.is_none() {
                        kprint(b"nxedit: could not open the chooser\n");
                        app.update(Msg::ChooserCancel);
                    }
                }
                (false, true) => {
                    if let Some(c) = chooser.take() {
                        c.close(&mut win);
                    }
                    *chooser_hovered = None;
                }
                _ => {}
            }
            if let Some(c) = chooser.as_mut() {
                let now = c.hovered_key();
                *chooser_hovered = now;
                let view = app.chooser_view(&theme, now);
                if !c.present(&mut win, &view, &font, &theme) {
                    kprint(b"nxedit: the chooser could not be drawn\n");
                }
            }

            // ---- the question's window, opened and destroyed with the question ----
            match (app.confirming(), confirm.is_some()) {
                (true, false) => {
                    // Said before the window is asked for: `Child::open` returns downstream of
                    // the shell, so a line after it raced the shell's (KVM failed, TCG passed).
                    kprint(b"nxedit: unsaved buffer - asking before closing\n");
                    let ask = app.confirm_view(&theme, None);
                    *confirm = Child::open(
                        &mut win,
                        Role::Dialog { parent: window_id },
                        // (0, 0): a manager places a dialog; the client does not know the screen.
                        (0, 0),
                        &ask,
                        &font,
                        &theme,
                        BUFFERS,
                    );
                    // Unconditional: `check-login` boots the release image, with no test line.
                    if confirm.is_none() {
                        kprint(b"nxedit: could not open the confirmation dialog\n");
                        app.confirm_failed();
                    }
                }
                (false, true) => {
                    if let Some(c) = confirm.take() {
                        c.close(&mut win);
                    }
                    *confirm_hovered = None;
                }
                _ => {}
            }
            if let Some(c) = confirm.as_mut() {
                let now = c.hovered_key();
                *confirm_hovered = now;
                let ask = app.confirm_view(&theme, now);
                if !c.present(&mut win, &ask, &font, &theme) {
                    kprint(b"nxedit: the confirmation dialog could not be drawn\n");
                }
            }

            if app.take_move_request()
                && let Some(mut w) = win.window(window_id)
                && w.start_move().is_err()
            {
                kprint(b"nxedit: the compositor refused the move\n");
            }
            if let Some(edges) = app.take_resize_request()
                && let Some(mut w) = win.window(window_id)
                && w.start_resize(edges).is_err()
            {
                kprint(b"nxedit: the compositor refused the resize\n");
            }
            if app.take_confirm_move()
                && let Some(c) = confirm.as_ref()
                && let Some(mut w) = win.window(c.id())
                && w.start_move().is_err()
            {
                kprint(b"nxedit: the compositor refused the dialog's move\n");
            }
            if let Some(state) = app.take_state_request()
                && let Some(mut w) = win.window(window_id)
                && w.request_state(state).is_err()
            {
                kprint(b"nxedit: the state request was refused\n");
            }
            if let Some(req) = app.take_clip_request() {
                match req {
                    nxedit::ClipRequest::Copy(text) => match clip_copy(root_ns, text.as_bytes()) {
                        // A count, never the text: the serial console is a log file.
                        Ok(()) => Line::new().s(b"nxedit: copied ").u(text.len() as u64).s(b" bytes").end(),
                        Err(why) => Line::new().s(b"nxedit: the copy failed: ").s(why.as_bytes()).end(),
                    },
                    nxedit::ClipRequest::Paste => {
                        let mut got = [0u8; MAX_CLIP_BYTES];
                        match clip_read(root_ns, 0, CLIP_ANY_SERIAL, &mut got) {
                            Ok(Some((serial, n))) => match core::str::from_utf8(&got[..n]) {
                                Ok(text) => {
                                    app.pasted(text, 0, serial);
                                    Line::new().s(b"nxedit: pasted ").u(n as u64).s(b" bytes").end();
                                }
                                Err(_) => app.cycle_ended("the clipboard is not text"),
                            },
                            Ok(None) => app.cycle_ended("the clipboard is empty"),
                            Err(why) => {
                                app.cycle_ended("the clipboard refused");
                                Line::new().s(b"nxedit: the paste failed: ").s(why.as_bytes()).end();
                            }
                        }
                    }
                    // A cycle carries the serial it last saw; a moved ring ends it visibly.
                    nxedit::ClipRequest::Cycle => match app.cycling() {
                        None => app.cycle_ended("nothing to cycle"),
                        Some(c) => {
                            let mut got = [0u8; MAX_CLIP_BYTES];
                            let next = c.index + 1;
                            match clip_read(root_ns, next, c.serial, &mut got) {
                                Ok(Some((serial, n))) => match core::str::from_utf8(&got[..n]) {
                                    Ok(text) => {
                                        app.cycled(text, next, serial);
                                        Line::new().s(b"nxedit: cycled to entry ").u(next as u64).end();
                                    }
                                    Err(_) => app.cycle_ended("that entry is not text"),
                                },
                                Ok(None) => app.cycle_ended("no older entry"),
                                Err(why) => {
                                    app.cycle_ended(why);
                                    kprint(b"nxedit: the cycle ended\n");
                                }
                            }
                        }
                    },
                }
                // Round again without waiting: `continue` here means "next window", and the wait
                // below would block with a frame owed.
                redraw_now = true;
                continue;
            }

            if let Some((key, editing, text)) = app.take_save() {
                // The path comes back with the bytes: `app.path()` is the current tab, and a
                // `Ctrl+S` plus a tab click in one drain wrote the wrong tab's bytes.
                let bytes = to_bytes(&text);
                let result = save(root_ns, &editing, &bytes);
                // Gates match the `saved` line; the failure line names the path and the reason.
                match result {
                    Ok(n) => libkern::debug::Line::new()
                        .s(b"nxedit: saved ")
                        .untrusted(editing.as_bytes())
                        .s(b" - ")
                        .u(n as u64)
                        .s(b" bytes")
                        .end(),
                    Err(why) => libkern::debug::Line::new()
                        .s(b"nxedit: save FAILED for ")
                        .untrusted(editing.as_bytes())
                        .s(b" - ")
                        .s(why.as_bytes())
                        .end(),
                }
                app.saved(key, result);
                redraw_now = true;
                continue;
            }
        }

        // After the loop: windows cannot be created or destroyed while iterating.
        let mut opened = 0usize;
        let mut quit_asked = false;
        for w in wins.iter_mut() {
            if w.app.take_new_window() {
                opened += 1;
            }
            if w.app.take_quit() {
                quit_asked = true;
            }
        }
        for _ in 0..opened {
            // A new window starts empty; mirroring this window's buffers would be a split.
            match open_window(&mut win, App::new("", &home), &font, &theme) {
                Some(w) => {
                    // Matched by a gate.
                    kprint(b"nxedit: opened another window\n");
                    wins.push(w);
                }
                None => kprint(b"nxedit: could not open another window\n"),
            }
        }
        if quit_asked {
            // Gates match "quitting", "closed a window" and "closing".
            kprint(b"nxedit: quitting\n");
            quit_pending = true;
        }

        // Quit asks one window at a time, as its own close button asks; cancelling aborts it.
        // Nothing is asked while a question is open, or every window would get a dialog.
        if quit_pending
            && !wins.iter().any(|w| w.app.confirming())
            && let Some(w) = wins.iter_mut().find(|w| !w.app.closing())
        {
            w.app.update(Msg::Close);
        }

        let mut i = 0;
        while i < wins.len() {
            if wins[i].app.closing() {
                let w = wins.remove(i);
                if let Some(m) = w.menu {
                    m.close(&mut win);
                }
                if let Some(c) = w.confirm {
                    c.close(&mut win);
                }
                w.top.close(&mut win);
                kprint(b"nxedit: closed a window\n");
            } else {
                i += 1;
            }
        }
        if wins.is_empty() {
            kprint(b"nxedit: closing\n");
            exit(0);
        }

        // A frame is owed: blocking now would delay the repaint to the next event.
        if redraw_now {
            continue;
        }

        if win.events_pending() == 0 {
            wait_one(ev);
        }
        let mut events = Vec::new();
        loop {
            match win.poll_event() {
                Ok(Some(e)) => events.push(e),
                Ok(None) => break,
                Err(_) => {
                    kprint(b"nxedit: the compositor went away\n");
                    exit(0);
                }
            }
        }
        for (from, event) in events {
            // A record naming no window is dropped, not routed into the first.
            let Some(wi) = wins.iter().position(|w| {
                w.top.id() == from
                    || w.menu.as_ref().is_some_and(|m| m.id() == from)
                    || w.confirm.as_ref().is_some_and(|c| c.id() == from)
                    || w.chooser.as_ref().is_some_and(|c| c.id() == from)
            }) else {
                continue;
            };
            let Win {
                top,
                app,
                size,
                bounds,
                confirm,
                confirm_hovered,
                chooser,
                chooser_hovered,
                menu,
                menu_shown,
                menu_hovered,
                ..
            } = &mut wins[wi];
            let window_id = top.id();
            let ui = app.view(&theme, top.hovered_key());
            let l = layout(&ui, *bounds, &FontMetrics::new(&font, theme.font_px));
            let mut resized = false;
            if menu.as_ref().is_some_and(|m| m.id() == from) {
                if matches!(event, WindowEvent::Dismissed) {
                    app.menus.close();
                    continue;
                }
                // The popup holds the keyboard, so its keys never reach the editor's router.
                if let WindowEvent::Key(k) = event {
                    let table = app.menu_table();
                    match app.menus.key(&k, &table) {
                        KeyOutcome::Chose { menu, item: i } => {
                            if let Some(msg) = table.get(menu).and_then(|m| m.items.get(i)).and_then(|it| match it {
                                Item::Action { msg, enabled: true, .. } => Some(msg.clone()),
                                _ => None,
                            }) {
                                app.update(msg);
                            }
                            continue;
                        }
                        KeyOutcome::Dismissed | KeyOutcome::Changed => continue,
                        KeyOutcome::Ignored => {}
                    }
                }
                let msgs = match (*menu_shown, menu.as_mut()) {
                    (Some(which), Some(m)) => {
                        let view = app.menu_view(which, &theme, *menu_hovered);
                        m.route(&view, &font, &theme, &event)
                    }
                    _ => Vec::new(),
                };
                for msg in msgs {
                    app.update(msg);
                }
                continue;
            }
            if chooser.as_ref().is_some_and(|c| c.id() == from) {
                // A manager asking a dialog to close means Cancel.
                let mut msgs = match chooser.as_mut() {
                    Some(c) => {
                        // Gates wait for this line before typing a name: a key that beats the
                        // manager's placement goes to the document instead.
                        if c.took_keyboard(&event) {
                            kprint(b"nxedit: the chooser has the keyboard\n");
                        }
                        let view = app.chooser_view(&theme, *chooser_hovered);
                        c.route(&view, &font, &theme, &event)
                    }
                    None => Vec::new(),
                };
                match event {
                    WindowEvent::Key(k) => {
                        // A receipt per character, so a dropped keystroke is not found as a wrong
                        // filename later; gated on the handler, which declines the release.
                        let handled = app.chooser_key(k);
                        if handled
                            && let Some(c) = app.chooser()
                            && c.mode == libui::chooser::Mode::Save
                        {
                            libkern::debug::Line::new()
                                .s(b"nxedit: chooser name so far ")
                                .u(c.state.name.text().chars().count() as u64)
                                .s(b" chars")
                                .end();
                        }
                    }
                    WindowEvent::CloseRequested => msgs.push(Msg::ChooserCancel),
                    WindowEvent::Focus(f) => app.chooser_focused = f,
                    _ => {}
                }
                for m in msgs {
                    app.update(m);
                }
                continue;
            }
            if confirm.as_ref().is_some_and(|c| c.id() == from) {
                let ask = app.confirm_view(&theme, *confirm_hovered);
                let mut msgs = confirm
                    .as_mut()
                    .map(|c| {
                        if c.took_keyboard(&event) {
                            kprint(b"nxedit: the question has the keyboard\n");
                        }
                        c.route(&ask, &font, &theme, &event)
                    })
                    .unwrap_or_default();
                match event {
                    // `Esc` is the dialog's and nothing else: no widget here takes a key.
                    WindowEvent::Key(k) => msgs.extend(app.confirm_key(k)),
                    WindowEvent::Focus(f) => app.confirm_focused = f,
                    WindowEvent::Dismissed => {}
                    WindowEvent::CloseRequested => msgs.push(Msg::KeepEditing),
                    _ => {}
                }
                for m in msgs {
                    // Unconditional (release image): the only outside sign of the answer given.
                    match m {
                        Msg::Discard => kprint(b"nxedit: discarding the unsaved buffer\n"),
                        // Cancelling aborts the quit; `quit_pending` must clear or it reopens.
                        Msg::KeepEditing => {
                            kprint(b"nxedit: close cancelled, still editing\n");
                            if quit_pending {
                                kprint(b"nxedit: quit cancelled\n");
                            }
                            quit_pending = false;
                        }
                        _ => {}
                    }
                    app.update(m);
                }
                continue;
            }
            if from != window_id {
                continue;
            }
            match event {
                WindowEvent::Key(k) => {
                    // A placeholder: no widget here takes a key, so the save button has no
                    // keyboard path.
                    if let Some(msg) = top.route_key(&ui, k) {
                        app.update(msg);
                    } else {
                        app.update(Msg::Key(k));
                    }
                }
                WindowEvent::Pointer(p) => {
                    let msgs = top.route(&ui, &font, &theme, &WindowEvent::Pointer(p));
                    for m in msgs {
                        app.update(m);
                    }
                    // Measuring is the binary's: it needs the font the area was drawn with, the
                    // fixed-advance one, or the caret lands off by more the further in.
                    let m = FontMetrics::new(&font, theme.font_px);
                    app.take_area_pointer(|s| m.text_size_as(s, nxedit::BUFFER_STYLE).w);
                }
                WindowEvent::Focus(f) => {
                    top.route(&ui, &font, &theme, &WindowEvent::Focus(f));
                    app.focused = f;
                }
                WindowEvent::Configure { width, height, .. } => {
                    if app.resize(Size::new(width, height)) {
                        resized = true;
                        // Matched by a gate.
                        libkern::debug::Line::new()
                            .s(b"nxedit: resized to ")
                            .u(u64::from(width))
                            .s(b"x")
                            .u(u64::from(height))
                            .end();
                    }
                }
                WindowEvent::Dismissed => {}
                WindowEvent::State(s) => app.update(Msg::WindowState(s)),
                // `Msg::Close` asks about unsaved work; a logout from the power menu waits on it.
                WindowEvent::CloseRequested => {
                    kprint(b"nxedit: asked to close\n");
                    app.update(Msg::Close);
                }
                WindowEvent::Drop { ref path, ref name, x, y, .. } => {
                    let taken = top.drop_at(&ui, &l, x, y);
                    // Matched by a gate; says whether the text area took the drop.
                    libkern::debug::Line::new()
                        .s(b"nxedit: drop of ")
                        .untrusted(name.as_bytes())
                        .s(if taken { b" on the document" as &[u8] } else { b" outside it" })
                        .end();
                    // A drop opens a tab, so nothing here decides whether the buffer can go.
                    if taken && app.accept_drop(path) {
                        open_into(app, root_ns, path);
                    }
                }
                WindowEvent::InputLost => kprint(b"nxedit: input dropped\n"),
            }
            if resized {
                *size = app.window_size();
                // `Child::resize` reallocates and drops the retained tree, whose damage would be
                // in the old coordinates. `Some(false)` is the memory refusing: the window keeps
                // its old size and still draws.
                if top.resize(*size) == Some(false) {
                    fail(b"nxedit: impossible window geometry\n");
                }
                *bounds = Rect::new(0, 0, size.w, size.h);
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxedit: panic\n");
    exit(2);
}
