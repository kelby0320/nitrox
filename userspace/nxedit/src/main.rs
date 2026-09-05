//! `nxedit` — the text editor: the window, the file, and the event pump.
//!
//! Everything with behaviour is in the library half and host-tested there. What is here is the
//! part that needs an OS: reading the file, writing it back safely, connecting to `/dev/draw`,
//! sharing buffers with the compositor, draining input.
//!
//! ## The one thing this file decides
//!
//! **How a save fails.** [`save`] writes a temporary beside the target and renames it over —
//! the sequence `coreutils` has used since Milestone 3.5, and the reason `libfs::rename` was
//! already there to be used. A save that truncated and then failed would leave neither the old
//! file nor the new one; a rename either happened or did not, and the file the person can still
//! see on screen is the buffer, which is kept whatever the answer.
//!
//! **Where the path comes from.** `argv[1]`, which is what `desktop-shell` puts there when a
//! client asks it to open something (`Desktop::Open`). With no argument there is nothing to
//! edit and the editor says so rather than opening an untitled buffer it could never save.

#![no_std]
#![no_main]

extern crate alloc;


use libdraw::geom::{Rect, Size};
use libdraw::text::{Font, load_ui};
use libkern::debug::Line;
use libkern::{exit, kprint};
use librsproto::clipboard::{CLIP_ANY_SERIAL, CLIP_KIND_TEXT, Clipboard, MAX_CLIP_BYTES};
use librsproto::surface::{Role};

use libsurface::{Session, WindowEvent, ipc::ChannelTransport};

use libui::layout::{layout, locate};
use libui::paint::{FontMetrics, Theme};

use libui::window::Child;
use libui::menu::{Item, KeyOutcome};
use nxedit::{App, MENU_BAR_KEY, MENU_COUNT, Msg, to_bytes};

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// `alloc` backing — the element tree and the buffer both allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Buffers shared with the compositor. Two is the minimum the protocol permits.
const BUFFERS: usize = 2;


/// The suffix a save's temporary carries.
///
/// **Beside the target, not in a temporary directory**, because a rename is only atomic within
/// one filesystem: a temporary elsewhere would be a cross-device rename, which the kernel
/// refuses as one operation and which would degrade the safe sequence into a copy.
const TEMP_SUFFIX: &str = ".nxedit-tmp";

/// Report and end the run.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: under the test-harness kernel this terminates QEMU; elsewhere it is
    // `Unsupported` and falls through.
    unsafe { libkern::syscall1(libkern::SYS_TEST_EXIT, libkern::TEST_EXIT_FAILURE as u64) };
    exit(1);
}


/// Read the file into `app`, or say why it could not be.
///
/// **A path that is not there is not a failure** — opening one is how a file gets made — but a
/// path that is a *directory*, or one that fails to read for any other reason, blocks the
/// buffer. See `App::blocked`: the danger is the empty window, which saved over a file is that
/// file destroyed by an editor that never showed it.
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
                libkern::debug::Line::new()
                    .s(b"nxedit: opened ")
                    .untrusted(path.as_bytes())
                    .s(b" - ")
                    .u(bytes.len() as u64)
                    .s(b" bytes")
                    .end();
            }
            // **Refused rather than mangled.** A lossy conversion would show a file that is not
            // the file, and saving it would write the mangling back.
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

/// What the taskbar should call this window.
///
/// **"untitled" rather than nothing** (M11 Part E batch 7). An empty title leaves the window list
/// showing `window 20`, which is the compositor's fallback and reads as a program that failed to
/// say what it is. The *title bar* has its own version of this, with the modified mark; this one
/// is what another process shows, and it is set once rather than per keystroke.
fn window_title(path: &str) -> &str {
    if path.is_empty() { "untitled" } else { libfs::basename_str(path) }
}

/// Write `bytes` to `path` the safe way: a temporary beside it, renamed over it.
///
/// The error is the message the status strip shows, so it is written for the person looking at
/// the window rather than for a log.
fn save(ns: u64, path: &str, bytes: &[u8]) -> Result<usize, &'static str> {
    let mut temp = String::from(path);
    temp.push_str(TEMP_SUFFIX);
    if libfs::write_file(ns, temp.as_bytes(), bytes).is_err() {
        // **The temporary is removed on the way out**, or a failed save leaves a file beside the
        // one being edited that nobody asked for and the browser then lists.
        let _ = libfs::unlink_at(ns, temp.as_bytes());
        return Err("the file could not be written");
    }
    if libfs::rename(ns, temp.as_bytes(), path.as_bytes(), true).is_err() {
        let _ = libfs::unlink_at(ns, temp.as_bytes());
        return Err("the file could not be replaced");
    }
    Ok(bytes.len())
}


/// Block until the compositor has something to say.
fn wait_one(h: u64) {
    let handles = [h];
    let mut results = [0u8; 24];
    // SAFETY: a valid one-handle array and a result buffer sized for one record.
    unsafe {
        libkern::syscall4(
            libkern::SYS_WAIT,
            handles.as_ptr() as u64,
            1,
            results.as_mut_ptr() as u64,
            u64::MAX,
        )
    };
}

/// Tell the compositor what a newly created window of this editor is.
///
/// **Extracted so a second window gets the same treatment** (M14 Part B). It was two blocks
/// inline before the loop, which was fine while a window was created once — and would have been
/// the first thing New Window quietly failed to do: an untitled bar, and a window that refuses
/// every drop because it never said it takes files.
fn dress<T: libsurface::Transport>(win: &mut Session<T>, id: u32, path: &str) {
    // **Set once, and it is the name alone.** The modified marker lives in the window's own
    // title bar; retitling on every keystroke would be a message per keystroke to say something
    // the window already shows.
    if let Some(mut w) = win.window(id)
        && w.set_title(window_title(path)).is_err()
    {
        kprint(b"nxedit: SetTitle refused\n");
    }
    // **What this window takes, said once** (M10 Part E). Files only: a directory has no contents
    // to put in a buffer, and an editor that accepted one would have to invent an answer for it.
    // The compositor matches against this while the pointer moves, so a drag carrying a folder is
    // never highlighted over this window at all.
    if let Some(mut w) = win.window(id)
        && w.declare_acceptor(nxedit::ACCEPTOR, librsproto::surface::DROP_KIND_FILE).is_err()
    {
        kprint(b"nxedit: DeclareAcceptor refused\n");
    }
}

/// The theme the shell handed this application, or the built-in one.
///
/// **Absent is normal rather than an error**: a client started outside a graphical session — by
/// `init`, or by a test harness — gets no setup record at all, and a desktop that refused to draw
/// without one would be a client that cannot be run on its own.
fn theme_of(env: &libstream::wire::Record) -> Theme {
    env.field_str("THEME").map(|s| Theme::from_config(s).0).unwrap_or_default()
}

/// Entry point.
///
/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments.
/// Push `bytes` onto the kill ring.
///
/// **Connects per operation rather than holding a session open**, like `nxterm`'s: a person
/// copies a handful of times a minute at most, and a held session costs the server a wait-set
/// slot in a set every application in the window system is in.
fn clip_copy(ns: u64, bytes: &[u8]) -> Result<(), &'static str> {
    let mut buf = [0u8; libkern::abi::IPC_MSG_SIZE];
    let mut clip = Clipboard::connect(ns, &mut buf).map_err(|_| "no /dev/clipboard")?;
    let r = clip.copy(CLIP_KIND_TEXT, bytes).map(|_| ());
    clip.close();
    r.map_err(|_| "the clipboard refused it")
}

/// Read ring entry `index`, continuing from `expect` — see `librsproto::clipboard::ClipPaste`.
///
/// `Ok(None)` is "there is no entry there", which is not a failure: an empty ring, or a cycle
/// that has walked off the end of it. `Err` carries a sentence the status strip can show, and
/// **the stale case gets its own**, because a client's answer to it is to start again rather
/// than to stop.
fn clip_read(
    ns: u64,
    index: u32,
    expect: u64,
    out: &mut [u8],
) -> Result<Option<(u64, usize)>, &'static str> {
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

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, endpoint: u64, arg0: u64) -> ! {
    kprint(b"nxedit: up\n");

    // **`argv` for the file and the environment for the theme.** Until M11 Part C this took
    // `argv` alone — `nxfiles` reads `HOME` because a browser with no argument still has
    // somewhere to start, while an editor with no file has nothing to edit. The theme is the
    // second thing the session tells every client, and it arrives on the same record.
    let (argv, env) = match libstream::setup::bootstrap(notif, root_ns, endpoint, arg0).setup() {
        Some(Ok(s)) => (s.argv, s.env),
        _ => (Vec::new(), libstream::wire::Record::default()),
    };
    // **`argv[1]`, or an untitled buffer** (M11 Part E batch 7). This used to print "no file to
    // edit" and exit, which is what "nxedit doesn't launch from the menu" turned out to be: the
    // applications modal passes no arguments, so the editor started and stopped. The refusal had
    // a reason — an untitled buffer is a promise an application cannot keep if it has no way to
    // ask for a name — and the answer is to ask, in a field in its own status strip.
    let path = argv.get(1).cloned().unwrap_or_default();
    // Where an untitled buffer is saved. The session hands every application its `HOME`; an
    // editor started outside one keeps `/home`, which is what a namespace without a user's
    // subtree still has.
    let home = alloc::string::String::from(
        env.field_str("HOME").filter(|h| !h.is_empty()).unwrap_or("/home"),
    );

    // **From the environment, not from a default** (M11 Part C), and read *before* the font,
    // because since M11 Part D the theme is what names the file to load. It used to sit further
    // down, next to the first thing drawn with it.
    let theme = theme_of(&env);
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let (font, _) = match unsafe { load_ui(root_ns, &theme, b"nxedit") } {
        Ok(loaded) => loaded,
        Err(e) => {
            libkern::debug::Line::new().s(b"nxedit: the UI font ").s(e.why()).end();
            fail(b"nxedit: font load FAILED\n");
        }
    };

    let mut app = App::new(&path, &home);
    // **Nothing to open when there is nothing named.** `open_into` would report a missing file,
    // which is true and is not what an empty buffer means.
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
    // **The first window is a `Child` too** (M14 Part B), which is what makes several of them
    // possible: everything a window needs to be drawn and routed lives in that value, so a second
    // one is a second value rather than a second copy of this loop.
    let top = {
        let ui = app.view(&theme, None);
        match Child::open_sized(
            &mut win,
            Role::Normal,
            (0, 0),
            size,
            &ui,
            &font,
            &theme,
            BUFFERS,
        ) {
            Some(t) => t,
            None => fail(b"nxedit: CreateWindow FAILED\n"),
        }
    };
    dress(&mut win, top.id(), &path);

    let bounds = Rect::new(0, 0, size.w, size.h);
    // **The second window, and the whole of M12 Part A**: alive only while a question is being
    // asked, because a `dialog` is transient by role — the compositor takes it with its parent,
    // and a hidden one would still be a window in the stack. `Role::Dialog` has existed since M2
    // Part A and no program a person runs had ever created one.
    let confirm: Option<Child> = None;
    // **The menu's window** — a third top-level window and this editor's first popup (M14 Part
    // A). Alive only while a menu is open, for the reason the dialog above is: a `popup` is
    // transient by role, the compositor takes it with its parent, and it holds the keyboard
    // while it is up.
    let menu: Option<Child> = None;
    // Which menu the live popup was opened for, so a *change* — not merely open-versus-shut —
    // rebuilds the window at the new word. Without it, clicking File then Edit would move the
    // rows and leave the window under the first word.
    let menu_shown: Option<usize> = None;
    let menu_hovered: Option<u64> = None;
    // Which of the dialog's controls the pointer was over at its last paint — a receipt, the way
    // `nxterm`'s menu hover is. Nothing is built from it; the view reads `hovered_key` directly.
    let confirm_hovered: Option<u64> = None;
    let ev = win.wait_handle();
    let reported = app.revision();
    // The same shape as `reported`, for the field that names an untitled buffer.
    let reported_name = app.naming_len();
    // **The title follows the current tab** (M12 Part D). It used to be set once, because there
    // was one file for the life of the process; a window whose taskbar entry still names the tab
    // you switched away from is the window list lying about what is on screen.
    let reported_title = String::from(window_title(&path));
    // Which tab was last reported, so a switch is one line and a redraw is none.
    let reported_tab = app.current_tab();

    /// Everything one window of this editor is.
    ///
    /// **A window, not the application** (M14 Part B). What is here is what a second window would
    /// need a second of: its own buffers and tabs (`app`), its own surface and retained tree
    /// (`top`), its own menu and its own question, and its own receipts — a gate reading one
    /// stream has to be able to tell two windows' lines apart, which is what `id` in each is for.
    ///
    /// What is *not* here is the `Session`, the font and the theme: one connection, one face and
    /// one palette serve every window this process owns.
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

    /// Open a window of this editor, dressed and ready to be serviced.
    ///
    /// **The same path for the first window and every later one** (M14 Part B). A New Window that
    /// built its window slightly differently from the original is how the second one ends up
    /// without a title, or without the drop acceptor — bugs nobody would think to look for.
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
        // **Every window this process owns, each serviced exactly as one used to be.**
        //
        // Destructured rather than dotted through, which is what keeps six hundred lines readable
        // across the conversion: every name below means what it meant when there was one window,
        // and the difference is that they are borrowed out of one of several.
        // Set by a window that owes a frame *now* — see the sites that set it.
        let mut redraw_now = false;
        for wi in 0..wins.len() {
        let Win {
            top,
            app,
            // The resize acts on the window whose `Configure` it was, in the dispatch below.
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
        } = &mut wins[wi];
        let window_id = top.id();
        // **One line per edit, and it carries a count rather than the text.** A gate driving a
        // release image cannot read this window — the pixels are the only echo an editor has —
        // so without a receipt it would type at whatever speed it liked and discover a dropped
        // keystroke as a wrong file three steps later. What somebody types into an editor is
        // theirs; the number says a keystroke landed and nothing else.
        let rev = app.revision();
        if rev != *reported {
            *reported = rev;
            libkern::debug::Line::new().s(b"nxedit: buffer rev ").u(rev).end();
        }
        // **And the same for the name field**, which `revision` cannot see: naming a buffer is
        // not editing it. Without this the seven keystrokes that name an untitled buffer are
        // unacknowledged, and a dropped one arrives as a file with the wrong name.
        // Retitled only when it changes, which is what keeps this one message per *switch*
        // rather than one per frame.
        let want_title = String::from(window_title(app.path()));
        if want_title != *reported_title
            && let Some(mut w) = win.window(window_id)
        {
            if w.set_title(&want_title).is_err() {
                kprint(b"nxedit: SetTitle refused\n");
            }
            *reported_title = want_title;
        }
        // **One line per tab the person lands on.** A gate driving a release image cannot see a
        // strip; what it can see is which file the editor says it is showing, which is the only
        // thing a tab switch changes that anybody outside can check.
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
                // **The field says which it is** (M12 Part C), because there are two of them
                // now and a gate waiting on "name so far" while somebody is typing a search
                // would wait for ever. `Field::label` is the one place that spelling lives.
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
        // ---- render ----
        // The widget under the pointer, from the router that has always known and that nothing
        // had ever asked (M11 Part E batch 3).
        let ui = app.view(&theme, top.hovered_key());
        let l = layout(&ui, *bounds, &FontMetrics::new(&font, theme.font_px));
        // Where each menu drops from, read every frame rather than when one opens: a bar word's
        // position is a fact about the layout, and before the first one there is nowhere to put a
        // popup at all — which is exactly what "could not open the menu" means without this.
        app.menus.set_anchors(
            (0..MENU_COUNT).map(|i| locate(&ui, &l, MENU_BAR_KEY + i as u64)).collect(),
        );
        // **The window is a `libui::window::Child` since M14 Part B**, top-level role and all —
        // the same value the menu and the dialog below have always been. What this loop keeps is
        // what a *main* window has and they do not: the `sys_wait`, a `Configure` to answer, and
        // the layout, which the menu bar's anchors are read from and which is therefore computed
        // here and handed on rather than computed twice.
        if !top.present_laid_out(&mut win, &ui, &l, &font, &theme) {
            fail(b"nxedit: the window could not be drawn\n");
        }

        // **What a search found, as a line number.** `check-login` boots the release image and
        // has no rendered window to read; the needle stays the person's business, so the receipt
        // is where it landed rather than what it was.
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

        // ---- the menu's window ----
        //
        // Opened and destroyed with the menu, and rebuilt when *which* menu is open changes.
        if *menu_shown != app.menus.open() {
            if let Some(m) = menu.take() {
                m.close(&mut win);
            }
            *menu_hovered = None;
            *menu_shown = app.menus.open();
            if let Some(which) = menu_shown {
                match app.menus.anchor() {
                    Some(at) => {
                        // Measured with no hover: the pointer is over the *bar word* that opened
                        // this rather than over the popup, and a highlight does not change what a
                        // menu measures.
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
                    // No anchor yet means no layout yet, which cannot happen after the first
                    // frame — and if it did, an unplaced menu is better than one at the origin.
                    None => *menu = None,
                }
                if menu.is_none() {
                    // Not fatal: the editor is usable without its menu, and saying so beats a
                    // window that silently never appears.
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

        // ---- the chooser's window ----
        //
        // **Listed here, drawn there** (M14 decision 3): `libui::chooser` renders over entries it
        // is given, and reading a directory is a syscall it cannot make. So the application says
        // which directory it wants and this does the listing — the same seam the tabs use for
        // their ttys and `nxfiles` uses for its own panes.
        // **A chosen file opens in a tab**, which is what `accept_drop` already decided a file
        // arriving from outside means: the buffers you have open are not given up for it.
        if let Some(path) = app.take_open()
            && app.accept_drop(&path)
        {
            open_into(app, root_ns, &path);
        }
        if let Some(dir) = app.take_chooser_list() {
            let rows = match libfs::list_dir(root_ns, dir.as_bytes()) {
                Ok(mut entries) => {
                    // **Ordered by `libfs`**, which is the point of putting the order there:
                    // this chooser and the browser cannot come to disagree about one.
                    libfs::sort(&mut entries, libfs::Order::NameAsc);
                    entries
                        .iter()
                        .filter_map(|e| {
                            let name = String::from_utf8_lossy(e.name()).into_owned();
                            (!name.is_empty()).then_some((
                                name,
                                e.kind == librsproto::file::DIRENT_KIND_DIR,
                            ))
                        })
                        .collect()
                }
                // **An unreadable directory is shown empty rather than refused.** The chooser is
                // already open and the path strip says where it is looking; closing it would
                // lose the person's place for a directory they can simply back out of.
                Err(_) => {
                    kprint(b"nxedit: could not list that directory\n");
                    alloc::vec::Vec::new()
                }
            };
            app.show_chooser(&dir, rows);
        }
        match (app.chooser().is_some(), chooser.is_some()) {
            (true, false) => {
                // **The receipt names the directory and counts what is in it**, because a chooser
                // that listed nothing draws exactly like one that listed a directory and opened
                // over it — the unreadable case above is *deliberately* an empty list. A gate
                // matching on "a chooser opened" would pass for the failure it exists to catch.
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

        // ---- the question's window ----
        //
        // **Opened and destroyed with the question**, not shown and hidden: the role is
        // transient, and there is nothing to keep alive between two closes of the same window.
        // Reconciled here rather than where `Msg::Close` is handled so that there is exactly one
        // place that turns `App::confirming` into a window — the alternative is a flag and a
        // window that can disagree about whether a question is being asked.
        match (app.confirming(), confirm.is_some()) {
            (true, false) => {
                // **Said before the window is asked for, not after it exists.** This used to
                // carry the dialog's id, which meant printing it once `Child::open` returned —
                // and that return is *downstream of the shell*, because a dialog's first
                // `Configure` is held until a manager places it. So the editor's line and the
                // shell's placement were two processes racing, and which won depended on the
                // accelerator: TCG gave the shell both, KVM gave the client the second one and
                // failed the gate that had watched TCG order twice (CI, PR #267).
                //
                // Printed here the order is a fact: the editor decides to ask, *then* asks for
                // a window. The id belongs in the shell's line anyway — it is the shell that
                // knows where the thing went.
                kprint(b"nxedit: unsaved buffer - asking before closing\n");
                // No hover yet: the pointer is wherever it was when the close was asked for, and
                // this call only measures — which a highlight does not change.
                let ask = app.confirm_view(&theme, None);
                *confirm = Child::open(
                    &mut win,
                    Role::Dialog { parent: window_id },
                    // **(0, 0), because this client does not know where it is.** A dialog's
                    // offset is a *preference* a manager overrides, and `rsproto-surface-ops.md`
                    // is explicit that a manager can centre a dialog on its parent from what it
                    // already tracks. A client that guessed would be guessing about a screen it
                    // has never been told the shape of.
                    (0, 0),
                    &ask,
                    &font,
                    &theme,
                    BUFFERS,
                );
                // **Unconditional, because `check-login` boots the release image.** A
                // `test-harness` line does not exist in the binary that gate runs, and the line
                // above is the only thing that says the editor asked rather than exited. It
                // names neither the window nor the file: what is on screen is the person's
                // business, and the path is already in the `opened` line above.
                if confirm.is_none() {
                    kprint(b"nxedit: could not open the confirmation dialog\n");
                    app.confirm_failed();
                }
            }
            (false, true) => {
                if let Some(c) = confirm.take() {
                    c.close(&mut win);
                }
                // The window is gone, so nothing in it is under the pointer.
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

        // ---- the requests this frame owes ----
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
        // **The dialog's own title bar, on the dialog's own window.** A `StartMove` names a
        // window id, so this cannot share the branch above: one flag would have moved whichever
        // window the argument happened to be.
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
        // ---- the clipboard ----
        //
        // **Here rather than in `update`**, for the save's reason below: `App::update` is a
        // function of values and `/dev/clipboard` is IPC.
        if let Some(req) = app.take_clip_request() {
            match req {
                nxedit::ClipRequest::Copy(text) => match clip_copy(root_ns, text.as_bytes()) {
                    // **A count, never the text.** An editor's buffer is a person's document
                    // and the serial console is a log file — the same rule the compositor
                    // follows for keystrokes.
                    Ok(()) => Line::new()
                        .s(b"nxedit: copied ")
                        .u(text.len() as u64)
                        .s(b" bytes")
                        .end(),
                    Err(why) => Line::new().s(b"nxedit: the copy failed: ").s(why.as_bytes()).end(),
                },
                // A plain paste always takes the newest, with no serial — decision 3's ordinary
                // case: copy in one application, paste in another.
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
                // **A cycle carries the serial it last saw** — decision 3. If the ring moved
                // under it (a pipeline pushed while somebody was mid-cycle) the server refuses
                // and the sequence ends here, visibly, rather than pasting a different entry.
                nxedit::ClipRequest::Cycle => match app.cycling() {
                    None => app.cycle_ended("nothing to cycle"),
                    Some(c) => {
                        let mut got = [0u8; MAX_CLIP_BYTES];
                        let next = c.index + 1;
                        match clip_read(root_ns, next, c.serial, &mut got) {
                            Ok(Some((serial, n))) => match core::str::from_utf8(&got[..n]) {
                                Ok(text) => {
                                    app.cycled(text, next, serial);
                                    Line::new()
                                        .s(b"nxedit: cycled to entry ")
                                        .u(next as u64)
                                        .end();
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
            // Round again rather than waiting: the buffer or the status strip has changed and
            // nothing else is going to arrive to prompt a redraw.
            // **Round again without waiting**, which is what this `continue` meant
            // when the body serviced one window. Inside the per-window loop it means
            // "next window", and the wait below would then block with a frame owed —
            // the stale-listing bug PR #257 removed, reintroduced by the conversion
            // (PR #283 review, worth fixing 4). The flag is what carries the old
            // meaning across the new shape.
            redraw_now = true;
            continue;
        }

        // **The write happens here, not in `update`.** `update` is a function of values; writing
        // a file is a syscall, so the application says it wants to save and the `main` that owns
        // the namespace performs it — the same outbox `nxfiles` uses for a directory read.
        if let Some((key, editing, text)) = app.take_save() {
            // **The path comes back with the bytes**, from the buffer that asked to be saved.
            // Reading `app.path()` here answers for whatever tab is current *now* — the top of
            // the iteration after the whole batch was applied — so a `Ctrl+S` and a tab click in
            // one drain wrote the other tab's bytes to the other tab's path (PR #270 review).
            let bytes = to_bytes(&text);
            let result = save(root_ns, &editing, &bytes);
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
            // Round again rather than waiting: the status strip has changed and nothing else is
            // going to arrive to prompt a redraw.
            // **Round again without waiting**, which is what this `continue` meant
            // when the body serviced one window. Inside the per-window loop it means
            // "next window", and the wait below would then block with a frame owed —
            // the stale-listing bug PR #257 removed, reintroduced by the conversion
            // (PR #283 review, worth fixing 4). The flag is what carries the old
            // meaning across the new shape.
            redraw_now = true;
            continue;
        }


        }

        // ---- what the windows asked for ----
        //
        // **After every window has been serviced**, not inside that loop: creating or destroying
        // a window while iterating over them is the one thing that shape cannot do.
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
            // **A new window starts empty**, which is what New Window means everywhere: the
            // buffers you have open are this window's, and a second window that mirrored them
            // would be two views of one thing — a split, which is a different feature.
            match open_window(&mut win, App::new("", &home), &font, &theme) {
                Some(w) => {
                    kprint(b"nxedit: opened another window\n");
                    wins.push(w);
                }
                // Not fatal: the editor is still usable, and saying so beats a chord that
                // silently does nothing.
                None => kprint(b"nxedit: could not open another window\n"),
            }
        }
        if quit_asked {
            kprint(b"nxedit: quitting\n");
            quit_pending = true;
        }

        // **Quit asks one window at a time.** Each is asked exactly as its own close button asks
        // — there is no second question for quitting — so a window with unsaved work raises its
        // dialog and the quit waits for the answer. **Cancelling aborts the quit** and leaves
        // every remaining window open (M14 decision 4); that is why this stops at the first
        // window that is asking rather than sending `Close` to all of them at once, which would
        // put a dialog on every window and take the first answer as the verdict for all.
        // **Nothing is asked while a question is open.** The first version skipped the window
        // that was confirming and asked the *next* one, which put a dialog on every window at
        // once — the alternative decision 4 explicitly rejected, arrived at by accident (PR #283
        // review, blocking 2).
        if quit_pending && !wins.iter().any(|w| w.app.confirming()) {
            if let Some(w) = wins.iter_mut().find(|w| !w.app.closing()) {
                w.app.update(Msg::Close);
            }
        }

        // **A window that has finished closing goes**, and the last one takes the process.
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

        // **A frame is owed, so do not block for an event.** Without this the window that
        // asked would not repaint until the next event happened to arrive — in practice the
        // key's own release, which is why this reads as a lag rather than a hang.
        if redraw_now {
            continue;
        }

        // ---- events ----
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
            // **Which window is this for?** With one window the answer was "this one, or a stale
            // record for a child just destroyed". With several it is a lookup, and a record
            // naming none of them is dropped rather than routed into whichever window happened
            // to be first — that being the bug this shape exists to prevent.
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
            // Rebuilt for *this* window: routing is against a layout, and a layout of another
            // window's tree would report a widget that is not the one under the pointer.
            let ui = app.view(&theme, top.hovered_key());
            let l = layout(&ui, *bounds, &FontMetrics::new(&font, theme.font_px));
            let mut resized = false;
            // **The dialog's window routes through the dialog's tree.** Same `App`, so an
            // answer's `Msg` updates the same state the main window's messages do; a different
            // tree and router, because they describe a different window. A record for a window
            // that is neither is not possible — `Session` filtered it — but a stale one for a
            // dialog just destroyed is, and it is dropped rather than routed into the buffer.
            // **The menu's window routes through the menu's tree.** Same `App`, so a row's
            // message updates the same state; a different tree, layout and router, because they
            // describe a different window.
            if menu.as_ref().is_some_and(|m| m.id() == from) {
                // A press landed outside the menu, so it goes away — the one thing a popup's
                // owner cannot work out for itself, because it never sees a press aimed elsewhere.
                if matches!(event, WindowEvent::Dismissed) {
                    app.menus.close();
                    continue;
                }
                // **Arrows, Esc and Enter drive the open menu**, which is possible only here: the
                // popup holds the keyboard while it is up, so these arrive naming *its* window and
                // never reach the editor's router below.
                if let WindowEvent::Key(k) = event {
                    let table = app.menu_table();
                    match app.menus.key(&k, &table) {
                        KeyOutcome::Chose { menu, item: i } => {
                            // **From the outcome rather than from `menu_shown`**, which happened
                            // to be right here and is a second copy of the same fact.
                            if let Some(msg) = table
                                .get(menu)
                                .and_then(|m| m.items.get(i))
                                .and_then(|it| match it {
                                    Item::Action { msg, enabled: true, .. } => Some(msg.clone()),
                                    _ => None,
                                })
                            {
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
            // **The chooser's window routes through the chooser's tree.** Same `App`, so a row's
            // message updates the same state; a different tree and router, because they describe
            // a different window.
            if chooser.as_ref().is_some_and(|c| c.id() == from) {
                // A dialog is not dismissed by a press elsewhere — a question stays until it is
                // answered — but a manager asking it to close means the same as *Cancel*.
                let mut msgs = match chooser.as_mut() {
                    Some(c) => {
                        let view = app.chooser_view(&theme, *chooser_hovered);
                        c.route(&view, &font, &theme, &event)
                    }
                    None => Vec::new(),
                };
                match event {
                    // **The keyboard is the application's here**, as it is for the naming field:
                    // `Child::route` sends a key to a *focused widget's* handler, and what a
                    // keystroke means to a chooser — an answer, a move, or a character — is not
                    // something the toolkit can decide.
                    WindowEvent::Key(k) => {
                        // **A receipt per character**, the discipline every typed sequence in this
                        // system's gates follows and the one `nxfiles`'s rename field grew a line
                        // for: an unacknowledged burst is a dropped keystroke discovered as a
                        // wrong filename several steps later. Under KVM the guest is fast enough
                        // that injected keys arrive bunched, which is exactly where that happens.
                        //
                        // **Gated on the handler's own answer**, which is what makes it *one* line
                        // per character: a key is a press and a release, `chooser_key` declines
                        // the release, and printing beside the call rather than after it emitted
                        // each count twice (PR #284 review, finding 7). Only while saving, because
                        // that is the mode with a field to type into.
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
                    .map(|c| c.route(&ask, &font, &theme, &event))
                    .unwrap_or_default();
                match event {
                    // **`Esc` is the dialog's, and nothing else is.** No widget in this tree
                    // takes a key — `Router::key` needs a focused widget and this one focuses
                    // none — so the router answers nothing and the application decides, exactly
                    // as it does for the naming field.
                    WindowEvent::Key(k) => msgs.extend(app.confirm_key(k)),
                    // Which window has the keyboard, for the dialog's own title bar. `route`
                    // has already told the router; this is the half the view reads.
                    WindowEvent::Focus(f) => app.confirm_focused = f,
                    // **A dialog is not dismissed by a press elsewhere.** The event is a
                    // popup's; a question stays until it is answered.
                    WindowEvent::Dismissed => {}
                    // The dialog's own frame draws a close button, which is `KeepEditing`. This
                    // arrives only if a *manager* asks the dialog to close, and it means the
                    // same thing: the question goes away and the buffer does not.
                    WindowEvent::CloseRequested => msgs.push(Msg::KeepEditing),
                    _ => {}
                }
                for m in msgs {
                    // **Unconditional, because `check-login` boots the release image**, and
                    // these two lines are the only outside sign of which answer was given. A
                    // dialog that only ever gets one answer is half a control.
                    match m {
                        Msg::Discard => kprint(b"nxedit: discarding the unsaved buffer\n"),
                        // **Cancelling aborts the quit**, which is decision 4's whole content and
                        // which nothing did until PR #283's review: `quitting` was set and never
                        // cleared, so the dialog reopened on the very next frame and could not be
                        // dismissed at all. Answering *this* question is answering the quit,
                        // because the quit is what asked it.
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
                    // **Every key reaches the buffer today, and the router branch is a
                    // placeholder rather than live routing.** `Router::key` returns early
                    // without a focused widget, this editor focuses none, and no toolkit widget
                    // sets `on_key` — so the `if` has never once been taken. It is kept because
                    // the first widget that wants a key (a find field) needs exactly this shape,
                    // and reading it as dispatch that already happens is the mistake: today the
                    // save button has **no keyboard path**, which is worth knowing before M12
                    // adds one (PR #259 review, optional 7).
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
                }
                WindowEvent::Focus(f) => {
                    top.route(&ui, &font, &theme, &WindowEvent::Focus(f));
                    app.focused = f;
                }
                WindowEvent::Configure { width, height, .. } => {
                    if app.resize(Size::new(width, height)) {
                        resized = true;
                        libkern::debug::Line::new()
                            .s(b"nxedit: resized to ")
                            .u(u64::from(width))
                            .s(b"x")
                            .u(u64::from(height))
                            .end();
                    }
                }
                // A dismissal is a popup's event; this window is not one.
                WindowEvent::Dismissed => {}
                // **Answered by exiting, and the unsaved buffer goes with it.** An editor with
                // somewhere to put a question would ask it — that is what a `CloseRequested`
                // rather than a destruction is *for* — and this one has no dialog to ask in.
                // Naming the gap is the honest half; the confirmation belongs with the rest of
                // the editor's second pass, which M12 owns.
                WindowEvent::CloseRequested => {
                    kprint(b"nxedit: asked to close, exiting\n");
                    app.update(Msg::Close);
                }
                // **Somebody dragged a file onto this window** (M10 Part E). Routed by position
                // like a press, so the text area takes it and the title bar does not — and the
                // buffer is only given up if there is nothing to lose.
                WindowEvent::Drop { ref path, ref name, x, y, .. } => {
                    let taken = top.drop_at(&ui, &l, x, y);
                    libkern::debug::Line::new()
                        .s(b"nxedit: drop of ")
                        .untrusted(name.as_bytes())
                        .s(if taken { b" on the document" as &[u8] } else { b" outside it" })
                        .end();
                    // **A drop opens a tab**, so nothing here has to decide whether the buffer
                    // on screen can be given up — `accept_drop` either switches to the tab that
                    // already has the file or makes a new one. The title follows the current tab
                    // at the top of the loop, so this no longer sets it by hand.
                    if taken && app.accept_drop(path) {
                        open_into(app, root_ns, path);
                    }
                }
                WindowEvent::InputLost => kprint(b"nxedit: input dropped\n"),
            }
            if resized {
            *size = app.window_size();
            // **`Child::resize` reallocates and throws the retained tree away**, which is what a
            // resize means: a tree diffed against a layout from the old bounds reports damage in
            // the old coordinates. `Some(false)` is the memory refusing, which leaves the window
            // at its old size and still drawing.
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
