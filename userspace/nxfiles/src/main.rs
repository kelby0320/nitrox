//! `nxfiles` — the file browser: the window, the buffers, and the event pump.
//!
//! Everything with behaviour is in the library half and host-tested there; this is the part that
//! needs an OS.
//!
//! The first window starts at `argv[1]` (the shell's Places menu passes one), else `HOME` from
//! the Tier-1 environment record — the user's own subtree — else `/`. Home is `HOME` either way:
//! the sidebar's places and every later window keep the session's home.
//!
//! The sidebar's Drives are the storage service's table, `/dev/storage/all.tsm`, read at start
//! and again whenever the watch at `/dev/storage/watch` pings. A browser refused a watch reads
//! the table when a window opens or takes focus, and follows nothing between. An eject opens a
//! media session for the one request, and closes it.
//!
//! The gates (`check-login`, `check-media`) match most `nxfiles:` lines this prints: grep
//! `tools/xtask/src` before changing one.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::geom::{Rect, Size};
use libdraw::text::{Font, load_ui};
use libkern::{exit, kprint};
use librsproto::surface::Role;

use libsurface::{Session, WindowEvent, ipc::ChannelTransport};

use libui::layout::layout;
use libui::paint::{FontMetrics, Theme};

use libui::menu::{Item, KeyOutcome};
use libui::window::Child;
use nxfiles::{App, Dialog, Drive, Entry, FileOp, Gesture, Msg, TITLE};

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Buffers shared with the compositor. Two is the minimum the protocol permits.
const BUFFERS: usize = 2;

const STORAGE_TABLE: &[u8] = b"/dev/storage/all.tsm";
const STORAGE_WATCH: &[u8] = b"/dev/storage/watch";
const STORAGE_MEDIA: &[u8] = b"/dev/storage/media";

fn read_drives(ns: u64) -> Option<Vec<Drive>> {
    let bytes = libfs::read_file(ns, STORAGE_TABLE).ok()?;
    let table = libstream::wire::Table::decode(&bytes).ok()?;
    Some(nxfiles::drives(&table))
}

fn say_drives(drives: &[Drive]) {
    let mut l = libkern::debug::Line::new();
    l.s(b"nxfiles: drives:");
    for d in drives {
        l.s(b" ").untrusted(d.name.as_bytes());
        if d.removable {
            l.s(b" (removable)");
        }
    }
    if drives.is_empty() {
        l.s(b" none");
    }
    l.end();
}

fn open_watch(ns: u64) -> Option<u64> {
    let (st, watch) = libfs::lookup_wait(ns, STORAGE_WATCH, librsproto::session::DIR_SESSION_RIGHTS);
    if st != 0 || watch == 0 {
        libkern::debug::Line::new()
            .s(b"nxfiles: no watch on the drives (status ")
            .i(st as i64)
            .s(b"), so they are read at a window's opening and focus")
            .end();
        return None;
    }
    kprint(b"nxfiles: watching the drives\n");
    Some(watch)
}

/// Take every ping queued on the watch, into `buf`: `Ok(true)` if there was one, `Err` once the
/// service's end has gone. Called only when a wait said the watch was ready.
fn drain_pings(watch: u64, buf: &mut [u8; libkern::abi::IPC_MSG_SIZE]) -> Result<bool, ()> {
    let mut pinged = false;
    loop {
        let _sent = match libkern::channel_recv(watch, buf) {
            Ok(sent) => sent,
            Err(got) if got == libkern::KError::WouldBlock.as_i32() as i64 => return Ok(pinged),
            Err(_) => return Err(()),
        };
        let off = libkern::abi::IPC_HEADER_SIZE;
        let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
        if let Ok(m) = librsproto::decode(&buf[off..off + len.min(buf.len() - off)]) {
            pinged |= librsproto::storage::is_changed(m.op, m.body);
        }
    }
}

/// Eject the drive mounted as `name`: a media session opened for the one `Eject`, and closed.
/// It waits for the service on the browser's one thread — `TODO(files-storage-wait)`. `Ok`
/// carries every mount unmounted on that stick, the drive going whole; `Err` the reason, put for
/// the person reading the strip.
fn eject(ns: u64, name: &str) -> Result<Vec<String>, String> {
    use librsproto::storage::OP_STORAGE_EJECT;
    let (st, session) = libfs::lookup_wait(ns, STORAGE_MEDIA, librsproto::session::DIR_SESSION_RIGHTS);
    if st == libkern::KError::WouldBlock.as_i32() {
        return Err(String::from("the storage service is busy; try again"));
    }
    if st != 0 || session == 0 {
        return Err(String::from("the storage service cannot be reached"));
    }
    let mut buf = alloc::vec![0u8; libkern::abi::IPC_MSG_SIZE];
    let reply = librsproto::session::round_trip(session, &mut buf, 1, OP_STORAGE_EJECT, name.as_bytes());
    // SAFETY: closing the media session this call opened.
    unsafe { libkern::syscall1(libkern::SYS_HANDLE_CLOSE, session) };
    let lost = || String::from("the storage service did not answer");
    let len = reply.map_err(|_| lost())?;
    let off = libkern::abi::IPC_HEADER_SIZE;
    let msg = librsproto::decode(&buf[off..off + len]).map_err(|_| lost())?;
    if msg.op != OP_STORAGE_EJECT {
        return Err(lost());
    }
    if msg.flags & librsproto::RS_FLAG_ERROR != 0 {
        return Err(librsproto::error::parse_error(msg.body)
            .map(|e| String::from_utf8_lossy(e.msg).into_owned())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| String::from("the storage service refused")));
    }
    Ok(librsproto::storage::ejected_names(msg.body).map(|n| String::from_utf8_lossy(n).into_owned()).collect())
}

/// Carry out a filesystem operation, and say in one line what happened.
///
/// The browser performs these itself: its `/home` binding is the authority they need. The
/// returned string is what the strip shows, so it is written for the person looking at it.
fn perform(ns: u64, op: &FileOp) -> (bool, &'static str) {
    // The destination is tested first, which is what makes "nothing overwrites" true:
    // `libfs::create_file` is idempotent, so *new file* onto an existing name would succeed and
    // report a new empty file, and `libfs::rename` does not distinguish an occupied destination,
    // so a refused rename would read like a fault. Testing here puts the promise in this program.
    let taken = |path: &String| libfs::file_size(ns, path.as_bytes()).is_some() || libfs::is_dir(ns, path.as_bytes());
    match op {
        FileOp::Create { path, dir } => {
            if taken(path) {
                return (false, "that name is taken");
            }
            let made = if *dir {
                libfs::mkdir(ns, path.as_bytes()).is_ok()
            } else {
                libfs::create_file(ns, path.as_bytes()).is_ok()
            };
            if made { (true, "created") } else { (false, "could not create it") }
        }
        // Never `replace`, for rename, copy and move alike: overwriting is a second question.
        FileOp::Rename { from, to } => {
            if taken(to) {
                return (false, "that name is taken");
            }
            match libfs::rename(ns, from.as_bytes(), to.as_bytes(), false) {
                Ok(()) => (true, "renamed"),
                Err(_) => (false, "could not rename it"),
            }
        }
        FileOp::MoveInto { from, to } => {
            if taken(to) {
                return (false, "there is one there already");
            }
            match libfs::rename(ns, from.as_bytes(), to.as_bytes(), false) {
                Ok(()) => (true, "moved"),
                Err(_) => (false, "could not move it"),
            }
        }
        // `copy_file` maps both sides and copies with no heap, bounded by `libfs::MAX_COPY`; a
        // *folder* takes `copy_tree`.
        FileOp::Copy { from, to, dir } => {
            if taken(to) {
                return (false, "that name is taken");
            }
            if *dir {
                return match libfs::copy_tree(ns, from.as_bytes(), to.as_bytes(), false, &mut |_, _, _| {}) {
                    Ok(()) => (true, "copied"),
                    Err(_) => (false, "could not copy it"),
                };
            }
            match libfs::copy_file(ns, from.as_bytes(), to.as_bytes(), false) {
                Ok(_) => (true, "copied"),
                Err(libfs::FileError::TooLarge) => (false, "too large to copy"),
                Err(_) => (false, "could not copy it"),
            }
        }
        FileOp::Delete { path, dir } => {
            let r = if *dir {
                libfs::remove_tree(ns, path.as_bytes(), &mut |_, _| {}).is_ok()
            } else {
                libfs::unlink_at(ns, path.as_bytes()).is_ok()
            };
            if r { (true, "deleted") } else { (false, "could not delete it") }
        }
    }
}

fn subject(op: &FileOp) -> &str {
    match op {
        FileOp::Create { path, .. } | FileOp::Delete { path, .. } => path,
        FileOp::Rename { to, .. } | FileOp::Copy { to, .. } | FileOp::MoveInto { to, .. } => to,
    }
}

fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: under the test-harness kernel this terminates QEMU; elsewhere it is
    // `Unsupported` and falls through.
    unsafe { libkern::syscall1(libkern::SYS_TEST_EXIT, libkern::TEST_EXIT_FAILURE as u64) };
    exit(1);
}

/// The wall clock in nanoseconds since the epoch, or `0` when the machine has none: a listing
/// dated 1970 would state something false. `column_modified` then shows dates, not times.
fn clock_now() -> u64 {
    let mut nanos = 0u64;
    // SAFETY: `&mut nanos` is a valid writable u64 out-param for the clock read.
    let r =
        unsafe { libkern::syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_REALTIME, (&raw mut nanos) as u64) };
    if r < 0 { 0 } else { nanos }
}

/// Read `path` and hand the listing to `app`.
///
/// A failed listing leaves the browser where it was: a directory that cannot be read (a path
/// that went away since the press) must not cost the listing the user came from.
fn navigate(app: &mut App, ns: u64, path: &str) {
    match libfs::list_dir(ns, path.as_bytes()) {
        Ok(entries) => {
            let rows: Vec<Entry> = entries.iter().filter_map(App::entry_of).collect();
            let read = rows.len();
            app.now_nanos = clock_now();
            app.show(path, rows);
            // Counted after `show`, which decides what the tab holds, and both numbers when they
            // differ, so a filtered listing is not mistaken for a small directory. Gates match
            // this line.
            let shown = app.entries().len();
            let mut l = libkern::debug::Line::new();
            l.s(b"nxfiles: listed ").untrusted(path.as_bytes()).s(b" - ").u(shown as u64);
            l.s(b" entries");
            if read != shown {
                l.s(b" (").u((read - shown) as u64).s(b" hidden)");
            }
            l.end();
        }
        Err(_) => {
            app.list_failed(path);
            libkern::debug::Line::new().s(b"nxfiles: cannot list ").untrusted(path.as_bytes()).end();
        }
    }
}

/// Ask `desktop-shell` to open `path`, and say whether it took the request.
///
/// A session per request: the shell bounds `/dev/desktop` sessions, and this is used once per
/// file at human speed. The answer is about the request; what the file turned out to be is the
/// opener's to report.
fn ask_shell_to_open(root_ns: u64, path: &str) -> bool {
    use librsproto::desktop::Desktop;
    let mut buf = alloc::vec![0u8; libkern::abi::IPC_MSG_SIZE];
    let Ok(mut desktop) = Desktop::connect(root_ns, &mut buf) else {
        kprint(b"nxfiles: no /dev/desktop; cannot open anything\n");
        return false;
    };
    let ok = desktop.open(path.as_bytes()).is_ok();
    desktop.close();
    if ok {
        libkern::debug::Line::new().s(b"nxfiles: asked to open ").s(path.as_bytes()).end();
    } else {
        libkern::debug::Line::new().s(b"nxfiles: could not open ").s(path.as_bytes()).end();
    }
    ok
}

fn home_of(env: &libstream::wire::Record) -> String {
    env.schema
        .fields
        .iter()
        .position(|f| f.name == "HOME")
        .and_then(|i| env.values.get(i))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| String::from("/"))
}

/// Put `bytes` on the clipboard as a list of paths, connecting per operation as `nxedit` and
/// `nxterm` do: a held session costs a wait-set slot.
fn clip_put(ns: u64, bytes: &[u8]) -> Result<(), &'static str> {
    let mut buf = [0u8; libkern::abi::IPC_MSG_SIZE];
    let mut clip = librsproto::clipboard::Clipboard::connect(ns, &mut buf).map_err(|_| "no /dev/clipboard")?;
    let r = clip.copy(librsproto::clipboard::CLIP_KIND_PATH, bytes).map(|_| ());
    clip.close();
    r.map_err(|_| "the clipboard refused it")
}

/// Read the newest clipboard entry: its kind, and how many bytes landed in `out`; `Ok(None)` is
/// an empty ring, which is what a paste before any copy finds.
fn clip_get(ns: u64, out: &mut [u8]) -> Result<Option<(u16, usize)>, &'static str> {
    let mut buf = [0u8; libkern::abi::IPC_MSG_SIZE];
    let mut clip = librsproto::clipboard::Clipboard::connect(ns, &mut buf).map_err(|_| "no /dev/clipboard")?;
    let r = clip.paste(0, librsproto::clipboard::CLIP_ANY_SERIAL, out);
    clip.close();
    match r {
        Ok((_, kind, len)) => Ok(Some((kind, len.min(out.len())))),
        Err(e) if e.is_empty() => Ok(None),
        Err(e) if e.is_stale() => Err("the clipboard changed"),
        Err(_) => Err("the clipboard refused"),
    }
}

/// Block until the compositor has something to say, or the watch does; with no watch it waits
/// on the session channel alone. Returns whether the watch was among what woke it: a record
/// names each handle ready, first word.
fn wait_for(h: u64, watch: Option<u64>) -> bool {
    let handles = [h, watch.unwrap_or(0)];
    let n = if watch.is_some() { 2 } else { 1 };
    let mut results = [0u8; 48];
    // SAFETY: a valid handle array of `n` and a result buffer sized for two records.
    let woke = unsafe {
        libkern::syscall4(libkern::SYS_WAIT, handles.as_ptr() as u64, n, results.as_mut_ptr() as u64, u64::MAX)
    };
    let Some(w) = watch else { return false };
    (0..woke.clamp(0, 2) as usize).any(|i| {
        let mut word = [0u8; 8];
        word.copy_from_slice(&results[i * 24..i * 24 + 8]);
        u64::from_le_bytes(word) == w
    })
}

fn dress<T: libsurface::Transport>(win: &mut Session<T>, id: u32) {
    if let Some(mut w) = win.window(id)
        && w.set_title(TITLE).is_err()
    {
        kprint(b"nxfiles: SetTitle refused\n");
    }
}

/// The theme the shell handed this application, or the built-in one: a client started outside a
/// graphical session gets no setup record, which is normal.
fn theme_of(env: &libstream::wire::Record) -> Theme {
    env.field_str("THEME").map(|s| Theme::from_config(s).0).unwrap_or_default()
}

/// Entry point.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, endpoint: u64, arg0: u64) -> ! {
    kprint(b"nxfiles: up\n");

    let (argv, env) = match libstream::setup::bootstrap(notif, root_ns, endpoint, arg0).setup() {
        Some(Ok(s)) => (s.argv, s.env),
        _ => (Vec::new(), libstream::wire::Record::default()),
    };
    let home = home_of(&env);
    // `argv[1]` is where the first window starts; home, which the sidebar's places and every later
    // window keep, is not the argument (it would offer `Documents/Documents`).
    let start = argv.get(1).cloned().unwrap_or_else(|| home.clone());
    libkern::debug::Line::new().s(b"nxfiles: starting at ").untrusted(start.as_bytes()).end();
    // Read here, beside the other thing the session tells this program, so the log shows it in
    // the order a reader expects.
    let theme = theme_of(&env);
    // Said out loud: a gate on a release image has no rendered grid to read, and this makes the
    // whole path — disk, shell, setup record, client — assertable from a console.
    libkern::debug::Line::new().s(b"nxfiles: theme font_px ").u(theme.font_px as u64).end();

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let (font, _) = match unsafe { load_ui(root_ns, &theme, b"nxfiles") } {
        Ok(loaded) => loaded,
        Err(e) => {
            libkern::debug::Line::new().s(b"nxfiles: the UI font ").s(e.why()).end();
            fail(b"nxfiles: font load FAILED\n");
        }
    };

    let mut app = App::new(&home);
    navigate(&mut app, root_ns, &start);

    let size = app.window_size();
    // SAFETY: `root_ns` is this process's live root namespace.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        // Boxed: ~9 KiB of message buffers has no business in a stack frame.
        Ok(t) => Box::new(t),
        Err(_) => fail(b"nxfiles: connect to /dev/draw FAILED\n"),
    };
    let mut win = Session::new(transport);
    // The window is a `libui::window::Child`, so a second window is a second value rather than a
    // second copy of this loop.
    let top = {
        let ui = app.view(&theme, None);
        match Child::open_sized(&mut win, Role::Normal, (0, 0), size, &ui, &font, &theme, BUFFERS) {
            Some(t) => t,
            None => fail(b"nxfiles: CreateWindow FAILED\n"),
        }
    };
    dress(&mut win, top.id());

    let bounds = Rect::new(0, 0, size.w, size.h);
    // The open menu's window and which menu it shows: choosing `Edit` while `File` is open
    // replaces the window rather than redrawing it at the other anchor.
    let menu: Option<Child> = None;
    let menu_shown: Option<usize> = None;
    let menu_hovered: Option<u64> = None;
    // The delete question's window, alive only while one is being asked.
    let dialog: Option<(Dialog, Child)> = None;
    let dialog_hovered: Option<u64> = None;
    let ev = win.wait_handle();
    let reported_prompt = app.prompt_len();
    let reported_pick: Option<String> = app.picked_name();
    let reported_loc: Option<usize> = app.location_text().map(|t| t.chars().count());

    /// Everything one window of this browser is: its own panes, surface, tree, menu, dialog and
    /// receipt. One connection, face and palette serve every window the process owns.
    struct Win {
        top: Child,
        app: App,
        size: Size,
        bounds: Rect,
        menu: Option<Child>,
        menu_shown: Option<usize>,
        menu_hovered: Option<u64>,
        /// The one dialog window this browser window may have open, and which dialog it is; the
        /// kind rides with it so a different dialog opening replaces it.
        dialog: Option<(Dialog, Child)>,
        dialog_hovered: Option<u64>,
        reported_prompt: Option<usize>,
        reported_pick: Option<String>,
        reported_loc: Option<usize>,
    }

    /// Open a window of this browser, dressed and ready: the path for the first window and every
    /// later one, so a New Window cannot differ.
    fn open_window<T: libsurface::Transport>(
        win: &mut Session<T>,
        mut app: App,
        font: &Font,
        theme: &Theme,
    ) -> Option<Win> {
        let size = app.window_size();
        let ui = app.view(theme, None);
        let top = Child::open_sized(win, Role::Normal, (0, 0), size, &ui, font, theme, BUFFERS)?;
        dress(win, top.id());
        let reported_prompt = app.prompt_len();
        let reported_pick = app.picked_name();
        let reported_loc = app.location_text().map(|t| t.chars().count());
        Some(Win {
            top,
            app,
            size,
            bounds: Rect::new(0, 0, size.w, size.h),
            menu: None,
            menu_shown: None,
            menu_hovered: None,
            dialog: None,
            dialog_hovered: None,
            reported_prompt,
            reported_pick,
            reported_loc,
        })
    }

    /// The drives as read now, given to every window when they changed, never to one alone, or
    /// the others go on showing what was.
    fn renew_drives(wins: &mut [Win], drives: &mut Vec<Drive>, now: Vec<Drive>) {
        if now != *drives {
            say_drives(&now);
            for w in wins.iter_mut() {
                w.app.set_drives(now.clone());
            }
            *drives = now;
        }
    }

    let mut quit_pending = false;
    // The drives are to be read again: a ping, an eject, or — with no watch — a focus.
    let mut refresh_drives = false;
    let mut wins = alloc::vec![Win {
        top,
        app,
        size,
        bounds,
        menu,
        menu_shown,
        menu_hovered,
        dialog,
        dialog_hovered,
        reported_prompt,
        reported_pick,
        reported_loc,
    }];

    // The drives, once the window is up: opening the watch and reading the table wait for the
    // storage service, which may be in an unmount chain for seconds. The first frame has no
    // Drives; the loop's first turn draws them.
    let mut watch = open_watch(root_ns);
    let mut drives = Vec::new();
    if let Some(now) = read_drives(root_ns) {
        renew_drives(&mut wins, &mut drives, now);
    }
    let mut ping_buf = alloc::boxed::Box::new([0u8; libkern::abi::IPC_MSG_SIZE]);

    loop {
        let mut redraw_now = false;
        if core::mem::take(&mut refresh_drives)
            && let Some(now) = read_drives(root_ns)
        {
            renew_drives(&mut wins, &mut drives, now);
        }
        for entry in wins.iter_mut() {
            let Win {
                top,
                app,
                size: _,
                bounds,
                menu,
                menu_shown,
                menu_hovered,
                dialog,
                dialog_hovered,
                reported_prompt,
                reported_pick,
                reported_loc,
            } = entry;
            let window_id = top.id();
            let ui = app.view(&theme, top.hovered_key());
            let l = layout(&ui, *bounds, &FontMetrics::new(&font, theme.font_px));
            // Where each menu drops from, read every frame: a bar item's position is a fact about
            // the layout.
            app.place_menus(&ui, &l);
            // Laid out here, not inside `present`, because the menu bar's anchors read it.
            if !top.present_laid_out(&mut win, &ui, &l, &font, &theme) {
                fail(b"nxfiles: the window could not be drawn\n");
            }

            if app.take_move_request()
                && let Some(mut w) = win.window(window_id)
                && w.start_move().is_err()
            {
                kprint(b"nxfiles: the compositor refused the move\n");
            }
            if let Some(edges) = app.take_resize_request()
                && let Some(mut w) = win.window(window_id)
                && w.start_resize(edges).is_err()
            {
                kprint(b"nxfiles: the compositor refused the resize\n");
            }
            if let Some(state) = app.take_state_request()
                && let Some(mut w) = win.window(window_id)
                && w.request_state(state).is_err()
            {
                kprint(b"nxfiles: the state request was refused\n");
            }
            if app.take_confirm_move()
                && let Some((_, c)) = dialog.as_ref()
                && let Some(mut w) = win.window(c.id())
                && w.start_move().is_err()
            {
                kprint(b"nxfiles: the compositor refused the dialog's move\n");
            }

            // Opened and destroyed with the menu: a `popup` is a transient role that holds the
            // keyboard while up, and a hidden one would still be a window in the stack.
            if *menu_shown != app.menus.open() {
                if let Some(m) = menu.take() {
                    m.close(&mut win);
                }
                *menu_hovered = None;
                *menu_shown = app.menus.open();
                if let Some(which) = menu_shown {
                    match app.menus.anchor() {
                        Some(a) => {
                            let view = app.menu_view(*which, &theme, None);
                            *menu = Child::open(
                                &mut win,
                                Role::Popup { parent: window_id },
                                a,
                                &view,
                                &font,
                                &theme,
                                BUFFERS,
                            );
                        }
                        None => *menu = None,
                    }
                    match menu.as_ref() {
                        // Where it is and how big, unconditionally (`check-login` matches this
                        // line, and a release image has no `test-harness` line): the gate divides
                        // the height by the row count rather than read the theme's text size.
                        Some(m) => {
                            let o = win.window(m.id()).and_then(|w| w.configured()).map_or((0, 0), |c| (c.x, c.y));
                            libkern::debug::Line::new()
                                .s(b"nxfiles: menu popup ")
                                .u(m.id() as u64)
                                .s(b" at ")
                                .i(o.0 as i64)
                                .s(b",")
                                .i(o.1 as i64)
                                .s(b" ")
                                .u(m.size().w as u64)
                                .s(b"x")
                                .u(m.size().h as u64)
                                .end();
                        }
                        // Not fatal: the browser is usable without its menu.
                        None => {
                            kprint(b"nxfiles: could not open the menu\n");
                            app.menus.close();
                            *menu_shown = None;
                        }
                    }
                }
            }
            if let Some(m) = menu.as_mut() {
                let now = m.hovered_key();
                if now != *menu_hovered {
                    *menu_hovered = now;
                    // Which row the pointer is over, unconditionally: `check-login` matches this
                    // line, walks down and stops at the row it means to click. A key is a number
                    // this program chose, not a label or a position.
                    let mut l = libkern::debug::Line::new();
                    l.s(b"nxfiles: menu hover ");
                    match now {
                        Some(k) => {
                            l.u(k);
                        }
                        None => {
                            l.s(b"none");
                        }
                    }
                    l.end();
                }
                let view = menu_shown.map(|w| app.menu_view(w, &theme, now));
                if let Some(view) = view
                    && !m.present(&mut win, &view, &font, &theme)
                {
                    kprint(b"nxfiles: the menu could not be drawn\n");
                }
            }

            // One line per character typed into the name prompt, a count and not the text: a
            // gate types at its own speed and otherwise finds a dropped keystroke as a wrong
            // filename later. The first (`0 chars`, which `check-login` waits on) is emitted after
            // the menu popup is told to close, as a popup holds the keyboard and a key injected
            // before the destroy was routed to the menu and dropped. The compositor serves every
            // queued request before it routes input, so a key after this line is routed after.
            let typed = app.prompt_len();
            // The selected row's name and the location bar's length, reported on change: a click
            // that selects has no other outward sign, and a gate typing a path needs a
            // per-character acknowledgement. Gates match `selected` and `location so far`.
            let loc = app.location_text().map(|t| t.chars().count());
            if loc != *reported_loc {
                *reported_loc = loc;
                match loc {
                    Some(n) => {
                        libkern::debug::Line::new().s(b"nxfiles: location so far ").u(n as u64).s(b" chars").end()
                    }
                    None => kprint(b"nxfiles: location bar closed\n"),
                }
            }
            let picked = app.picked_name();
            if picked != *reported_pick {
                *reported_pick = picked.clone();
                match picked.as_deref() {
                    Some(name) => libkern::debug::Line::new().s(b"nxfiles: selected ").untrusted(name.as_bytes()).end(),
                    None => kprint(b"nxfiles: nothing selected\n"),
                }
            }
            if typed != *reported_prompt {
                *reported_prompt = typed;
                if let Some(n) = typed {
                    libkern::debug::Line::new().s(b"nxfiles: name so far ").u(n as u64).s(b" chars").end();
                }
            }

            // Reconciled against the kind, not "is one open": a dialog replacing another is a
            // differently sized frame, so the window is closed and reopened.
            let want = app.dialog();
            if want != dialog.as_ref().map(|(k, _)| *k) {
                if let Some((kind, c)) = dialog.take() {
                    c.close(&mut win);
                    kprint(kind.closed());
                }
                *dialog_hovered = None;
                if let Some(kind) = want {
                    // Said before the window is asked for, so it cannot race the shell to the
                    // console: the dialog's first `Configure` is held for the manager.
                    kprint(kind.opening());
                    let view = app.dialog_view(&theme, None);
                    let opened = Child::open(
                        &mut win,
                        Role::Dialog { parent: window_id },
                        // (0, 0): the manager overrides a dialog's offset anyway.
                        (0, 0),
                        &view,
                        &font,
                        &theme,
                        BUFFERS,
                    );
                    match opened {
                        Some(c) => *dialog = Some((kind, c)),
                        None => {
                            kprint(kind.open_failed());
                            app.dialog_failed();
                        }
                    }
                }
            }
            if let Some((kind, c)) = dialog.as_mut() {
                let now = c.hovered_key();
                *dialog_hovered = now;
                let view = app.dialog_view(&theme, now);
                if !c.present(&mut win, &view, &font, &theme) {
                    kprint(kind.draw_failed());
                }
            }

            if let Some((verb, paths)) = app.take_clip_push() {
                let refs: Vec<&str> = paths.iter().map(String::as_str).collect();
                let mut payload = [0u8; librsproto::clipboard::MAX_CLIP_BYTES];
                match librsproto::clipboard::encode_paths(verb, &refs, &mut payload) {
                    Some(n) => match clip_put(root_ns, &payload[..n]) {
                        Ok(()) => libkern::debug::Line::new()
                            .s(b"nxfiles: clipboard now holds ")
                            .u(refs.len() as u64)
                            .s(if verb == librsproto::clipboard::PathVerb::Cut {
                                b" path(s) to move".as_slice()
                            } else {
                                b" path(s) to copy".as_slice()
                            })
                            .end(),
                        Err(e) => app.operated(e),
                    },
                    // Refused rather than truncated, and said: half a path names another file.
                    None => app.operated("too many paths for one clipboard entry"),
                }
            }
            if app.take_clip_read() {
                let mut payload = [0u8; librsproto::clipboard::MAX_CLIP_BYTES];
                match clip_get(root_ns, &mut payload) {
                    Ok(Some((kind, len))) if kind == librsproto::clipboard::CLIP_KIND_PATH => {
                        match librsproto::clipboard::Paths::read(&payload[..len]) {
                            Some(p) => {
                                // The filesystem is asked what each path is: the wire carries
                                // paths, and a folder copy differs from a file copy.
                                let list: Vec<(&str, bool)> =
                                    p.iter().map(|path| (path, libfs::is_dir(root_ns, path.as_bytes()))).collect();
                                app.pasted(p.verb, &list);
                            }
                            // The kind said paths and the bytes are not: not this browser's to fix.
                            None => app.operated("the clipboard entry is not readable"),
                        }
                    }
                    Ok(Some(_)) => app.paste_not_paths(),
                    Ok(None) => app.operated("the clipboard is empty"),
                    Err(e) => app.operated(e),
                }
            }

            if app.has_ops() {
                // The whole queue, then one listing, so the directory is not shown half-done. Every
                // outcome reaches the strip, not the last: three refusals then a success must not
                // say *copied*. The console keeps a line per operation.
                let mut said = "";
                let mut done = 0usize;
                let mut refused: Option<&'static str> = None;
                let mut refusals = 0usize;
                while let Some(op) = app.take_op() {
                    let (ok, why) = perform(root_ns, &op);
                    said = why;
                    if ok {
                        done += 1;
                    } else {
                        refusals += 1;
                        refused = Some(why);
                    }
                    libkern::debug::Line::new()
                        .s(b"nxfiles: ")
                        .s(said.as_bytes())
                        .s(b" ")
                        .untrusted(subject(&op).as_bytes())
                        .end();
                }
                match (done, refusals, refused) {
                    (_, 0, _) if done <= 1 => app.operated(said),
                    (_, 0, _) => app.operated_many(done, 0, said),
                    (0, _, Some(why)) if refusals == 1 => app.operated(why),
                    (_, _, Some(why)) => app.operated_many(done, refusals, why),
                    (_, _, None) => app.operated(said),
                }
                let here = String::from(app.path());
                navigate(app, root_ns, &here);
                app.operated(said);
                // Round again without waiting: inside the per-window loop `continue` means "next
                // window", and the wait below would block with a frame owed. The flag carries it.
                redraw_now = true;
                continue;
            }
            // Asking the shell to open a file is where a browser's authority stops: it has no
            // `/bin` and no namespace-building, only `/dev/desktop`. It names a path; the shell
            // decides what opens it.
            if let Some(path) = app.take_open() {
                let ok = ask_shell_to_open(root_ns, &path);
                app.opened(&path, ok);
            }
            // An eject for the one request, answered in the strip; the table is read again at once,
            // which a browser with no watch would otherwise never get.
            if let Some(name) = app.take_eject() {
                let answer = eject(root_ns, &name);
                let mut l = libkern::debug::Line::new();
                l.s(b"nxfiles: ");
                match &answer {
                    Ok(names) if !names.is_empty() => l.untrusted(names.join(", ").as_bytes()).s(b" ejected"),
                    Ok(_) => l.untrusted(name.as_bytes()).s(b" ejected"),
                    Err(why) => l.untrusted(name.as_bytes()).s(b" not ejected: ").untrusted(why.as_bytes()),
                };
                l.end();
                app.ejected(&name, answer.as_deref().map_err(|why| why.as_str()));
                refresh_drives = true;
                redraw_now = true;
                continue;
            }
            if let Some(to) = app.take_goto() {
                navigate(app, root_ns, &to);
                // Round again rather than waiting, or the listing is not drawn until the next
                // event arrives.
                redraw_now = true;
                continue;
            }
        }

        let mut opened = 0usize;
        for w in wins.iter_mut() {
            if w.app.take_new_window() {
                opened += 1;
            }
            if w.app.take_quit() {
                kprint(b"nxfiles: quitting\n");
                quit_pending = true;
            }
        }
        for _ in 0..opened {
            // A new window starts at home, not here, with the drives as last read (read again
            // first when no watch keeps them).
            if watch.is_none()
                && let Some(now) = read_drives(root_ns)
            {
                renew_drives(&mut wins, &mut drives, now);
            }
            let mut fresh = App::new(&home);
            fresh.set_drives(drives.clone());
            match open_window(&mut win, fresh, &font, &theme) {
                Some(w) => {
                    kprint(b"nxfiles: opened another window\n");
                    wins.push(w);
                }
                None => kprint(b"nxfiles: could not open another window\n"),
            }
        }
        // Quit closes every window: nothing here has unsaved work, so none can refuse.
        if quit_pending {
            for w in wins.iter_mut() {
                w.app.update(Msg::Close);
            }
        }
        let mut i = 0;
        while i < wins.len() {
            if wins[i].app.closing() {
                let w = wins.remove(i);
                if let Some(m) = w.menu {
                    m.close(&mut win);
                }
                if let Some((_, c)) = w.dialog {
                    c.close(&mut win);
                }
                w.top.close(&mut win);
                kprint(b"nxfiles: closed a window\n");
            } else {
                i += 1;
            }
        }
        if wins.is_empty() {
            kprint(b"nxfiles: closing\n");
            exit(0);
        }

        if redraw_now {
            continue;
        }

        let watch_ready = win.events_pending() == 0 && wait_for(ev, watch);
        // A ping, or several: the drives are read once, before the next frame. A watch whose
        // service end has gone is let go and the table is read at focus. A ping left queued while
        // events were pending keeps the watch ready, so the next wait returns at once.
        if watch_ready && let Some(w) = watch {
            match drain_pings(w, &mut ping_buf) {
                Ok(pinged) => refresh_drives |= pinged,
                Err(()) => {
                    kprint(b"nxfiles: the drives' watch closed, so they are read at focus\n");
                    // SAFETY: closing the watch this process opened.
                    unsafe { libkern::syscall1(libkern::SYS_HANDLE_CLOSE, w) };
                    watch = None;
                }
            }
        }
        let mut events = Vec::new();
        loop {
            match win.poll_event() {
                Ok(Some(e)) => events.push(e),
                Ok(None) => break,
                Err(_) => {
                    kprint(b"nxfiles: the compositor went away\n");
                    exit(0);
                }
            }
        }
        for (from, event) in events {
            let Some(wi) = wins.iter().position(|w| {
                w.top.id() == from
                    || w.menu.as_ref().is_some_and(|m| m.id() == from)
                    || w.dialog.as_ref().is_some_and(|(_, c)| c.id() == from)
            }) else {
                continue;
            };
            let Win { top, app, size, bounds, menu, menu_shown, menu_hovered, dialog, dialog_hovered, .. } =
                &mut wins[wi];
            let window_id = top.id();
            // Rebuilt for *this* window: routing is against this window's tree. `Child::route`
            // lays the content out itself.
            let ui = app.view(&theme, top.hovered_key());
            let mut resized = false;
            // The menu's window routes through the menu's tree, though the `Msg` updates the same
            // `App` a click in the list does.
            if menu.as_ref().is_some_and(|m| m.id() == from) {
                if matches!(event, WindowEvent::Dismissed) {
                    app.menus.close();
                    continue;
                }
                // Arrows, Esc and Enter drive the open menu: a popup holds the keyboard, so these
                // never reach the browser's router. The message comes from the table the popup
                // drew, so Enter and a click on a row cannot differ.
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
            if dialog.as_ref().is_some_and(|(_, c)| c.id() == from) {
                let view = app.dialog_view(&theme, *dialog_hovered);
                let mut msgs = dialog
                    .as_mut()
                    .map(|(kind, c)| {
                        // The receipt a key may be pressed after (`opening` precedes the window,
                        // and an earlier key can reach the browser): see `Child::took_keyboard`.
                        if c.took_keyboard(&event) {
                            kprint(kind.has_keyboard());
                        }
                        c.route(&view, &font, &theme, &event)
                    })
                    .unwrap_or_default();
                match event {
                    WindowEvent::Key(k) => msgs.extend(app.dialog_key(k)),
                    WindowEvent::Focus(f) => app.dialog_focused = f,
                    WindowEvent::Dismissed => {}
                    WindowEvent::CloseRequested => msgs.extend(app.dialog_dismissed()),
                    _ => {}
                }
                for msg in msgs {
                    // Unconditional: `check-login` matches these two lines, the only outside sign
                    // of which answer was given.
                    match msg {
                        Msg::ConfirmDelete => kprint(b"nxfiles: confirmed the delete\n"),
                        Msg::KeepIt => kprint(b"nxfiles: delete cancelled\n"),
                        _ => {}
                    }
                    app.update(msg);
                }
                continue;
            }
            if from != window_id {
                continue;
            }
            match event {
                WindowEvent::Key(k) => {
                    if let Some(msg) = top.route_key(&ui, k) {
                        app.update(msg);
                    } else {
                        // Arrow keys and Enter are the browser's own: nothing in this tree is
                        // focusable that would want them.
                        app.update(Msg::Key(k));
                    }
                }
                WindowEvent::Pointer(p) => {
                    // Counted before the event is routed, so the message the press produces can
                    // ask what number the click was; the record's own time is used.
                    if p.kind == librsproto::surface::POINTER_BUTTON
                        && p.flags & librsproto::surface::POINTER_PRESSED != 0
                        && p.button == libkern::abi::BTN_LEFT
                    {
                        app.note_press(libdraw::geom::Point::new(p.x, p.y), p.time_ms, p.modifiers);
                    }
                    let msgs = top.route(&ui, &font, &theme, &WindowEvent::Pointer(p));
                    for m in msgs {
                        app.update(m);
                    }
                    // What makes a press a drag is how far it has travelled, which `libui` does
                    // not measure. A drag is this window's until the pointer leaves it, and only
                    // then is it handed to the compositor.
                    match app.pointer_moved(p.x, p.y, p.buttons) {
                        Gesture::HandOff => {
                            if let Some((entry, path)) = app.take_drag() {
                                let kind = if entry.is_dir {
                                    librsproto::surface::DROP_KIND_DIR
                                } else {
                                    librsproto::surface::DROP_KIND_FILE
                                };
                                if let Some(mut w) = win.window(window_id) {
                                    match w.start_drag(kind, &path, &entry.name) {
                                        Ok(()) => libkern::debug::Line::new()
                                            .s(b"nxfiles: dragging ")
                                            .untrusted(entry.name.as_bytes())
                                            .end(),
                                        // Refused means the press ended between the motion and
                                        // this request: ordinary, not an error.
                                        Err(_) => kprint(b"nxfiles: the compositor refused the drag\n"),
                                    }
                                }
                            }
                        }
                        Gesture::Dropped | Gesture::Moved | Gesture::None => {}
                    }
                }
                WindowEvent::Focus(f) => {
                    top.route(&ui, &font, &theme, &WindowEvent::Focus(f));
                    app.focused = f;
                    refresh_drives |= f && watch.is_none();
                }
                WindowEvent::Configure { width, height, .. } => {
                    if app.resize(Size::new(width, height)) {
                        resized = true;
                        libkern::debug::Line::new()
                            .s(b"nxfiles: resized to ")
                            .u(u64::from(width))
                            .s(b"x")
                            .u(u64::from(height))
                            .end();
                    }
                }
                WindowEvent::Dismissed => {}
                WindowEvent::State(s) => app.update(Msg::WindowState(s)),
                // The shell asking, answered as the close button is: the kernel closes this
                // process's handles and the compositor tears its windows down.
                WindowEvent::CloseRequested => {
                    kprint(b"nxfiles: asked to close, exiting\n");
                    app.update(Msg::Close);
                }
                // This browser offers drags and takes none: a drop is ignored, not swallowed
                // under a highlight.
                WindowEvent::Drop { ref name, .. } => {
                    libkern::debug::Line::new().s(b"nxfiles: ignoring a drop of ").untrusted(name.as_bytes()).end();
                }
                WindowEvent::InputLost => kprint(b"nxfiles: input dropped\n"),
            }
            if resized {
                *size = app.window_size();
                // `Child::resize` throws the retained tree away, so the next diff reports the
                // whole window rather than damage in the old coordinates.
                if top.resize(*size) == Some(false) {
                    fail(b"nxfiles: impossible window geometry\n");
                }
                *bounds = Rect::new(0, 0, size.w, size.h);
            }
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxfiles: panic\n");
    exit(2);
}
