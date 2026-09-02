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

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::{Font, load_ui};
use libkern::{exit, kprint};
use librsproto::surface::{CreateWindowRequest, Role};
use libsurface::buffers::BufferPool;
use libsurface::{Session, WindowEvent, ipc::ChannelTransport};
use libui::diff::Tree;
use libui::layout::{Layout, layout};
use libui::paint::{FontMetrics, Theme, paint};
use libui::route::Router;
use libui::window::Child;
use nxedit::{App, Msg, to_bytes};

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

/// A private framebuffer of `size` to compose a frame into.
fn compose_buffer(size: Size) -> Option<MemFramebuffer> {
    let pitch = (size.w as usize).checked_mul(4)?;
    Geometry::with_pitch(size.w, size.h, pitch, PixelFormat::XRGB8888).map(MemFramebuffer::new)
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

/// Paint `damage` of `app` into `fb`.
fn draw(
    fb: &mut MemFramebuffer,
    ui: &libui::element::Element<Msg>,
    l: &Layout,
    font: &Font,
    theme: &Theme,
    damage: Rect,
) {
    // No `custom` nodes: everything this application draws is a widget the toolkit owns.
    paint(fb, font, theme, ui, l, damage, &mut |_, _, _, _: &mut MemFramebuffer| {});
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
    let mut size = app.window_size();
    // SAFETY: `root_ns` is this process's live root namespace.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => Box::new(t),
        Err(_) => fail(b"nxedit: connect to /dev/draw FAILED\n"),
    };
    let mut win = Session::new(transport);
    let window_id =
        match win.create(&CreateWindowRequest::new(size.w, size.h, Role::Normal), BUFFERS) {
            Ok(w) => w,
            Err(_) => fail(b"nxedit: CreateWindow FAILED\n"),
        };
    // **Set once, and it is the name alone.** The modified marker lives in the window's own
    // title bar; retitling on every keystroke would be a message per keystroke to say something
    // the window already shows.
    if let Some(mut w) = win.window(window_id)
        && w.set_title(window_title(&path)).is_err()
    {
        kprint(b"nxedit: SetTitle refused\n");
    }
    // **What this window takes, said once** (M10 Part E). Files only: a directory has no
    // contents to put in a buffer, and an editor that accepted one would have to invent an
    // answer for it. The compositor matches against this while the pointer moves, so a drag
    // carrying a folder is never highlighted over this window at all.
    if let Some(mut w) = win.window(window_id)
        && w.declare_acceptor(nxedit::ACCEPTOR, librsproto::surface::DROP_KIND_FILE).is_err()
    {
        kprint(b"nxedit: DeclareAcceptor refused\n");
    }

    let mut scratch = match compose_buffer(size) {
        Some(fb) => fb,
        None => fail(b"nxedit: impossible window geometry\n"),
    };
    let mut pool = {
        let Some(mut w) = win.window(window_id) else {
            fail(b"nxedit: our own window is gone\n");
        };
        match BufferPool::new(&mut w, size, BUFFERS) {
            Some(p) => p,
            None => fail(b"nxedit: buffer alloc FAILED\n"),
        }
    };

    let mut bounds = Rect::new(0, 0, size.w, size.h);
    let mut tree = Tree::new();
    let mut router = Router::new();
    // **The second window, and the whole of M12 Part A**: alive only while a question is being
    // asked, because a `dialog` is transient by role — the compositor takes it with its parent,
    // and a hidden one would still be a window in the stack. `Role::Dialog` has existed since M2
    // Part A and no program a person runs had ever created one.
    let mut confirm: Option<Child> = None;
    // Which of the dialog's controls the pointer was over at its last paint — a receipt, the way
    // `nxterm`'s menu hover is. Nothing is built from it; the view reads `hovered_key` directly.
    let mut confirm_hovered: Option<u64> = None;
    let ev = win.wait_handle();
    let mut reported = app.revision();
    // The same shape as `reported`, for the field that names an untitled buffer.
    let mut reported_name = app.naming_len();
    // **The title follows the current tab** (M12 Part D). It used to be set once, because there
    // was one file for the life of the process; a window whose taskbar entry still names the tab
    // you switched away from is the window list lying about what is on screen.
    let mut reported_title = String::from(window_title(&path));
    // Which tab was last reported, so a switch is one line and a redraw is none.
    let mut reported_tab = app.current_tab();

    loop {
        // **One line per edit, and it carries a count rather than the text.** A gate driving a
        // release image cannot read this window — the pixels are the only echo an editor has —
        // so without a receipt it would type at whatever speed it liked and discover a dropped
        // keystroke as a wrong file three steps later. What somebody types into an editor is
        // theirs; the number says a keystroke landed and nothing else.
        let rev = app.revision();
        if rev != reported {
            reported = rev;
            libkern::debug::Line::new().s(b"nxedit: buffer rev ").u(rev).end();
        }
        // **And the same for the name field**, which `revision` cannot see: naming a buffer is
        // not editing it. Without this the seven keystrokes that name an untitled buffer are
        // unacknowledged, and a dropped one arrives as a file with the wrong name.
        // Retitled only when it changes, which is what keeps this one message per *switch*
        // rather than one per frame.
        let want_title = String::from(window_title(app.path()));
        if want_title != reported_title
            && let Some(mut w) = win.window(window_id)
        {
            if w.set_title(&want_title).is_err() {
                kprint(b"nxedit: SetTitle refused\n");
            }
            reported_title = want_title;
        }
        // **One line per tab the person lands on.** A gate driving a release image cannot see a
        // strip; what it can see is which file the editor says it is showing, which is the only
        // thing a tab switch changes that anybody outside can check.
        let showing = app.current_tab();
        if showing != reported_tab {
            reported_tab = showing;
            libkern::debug::Line::new()
                .s(b"nxedit: tab ")
                .u(showing)
                .s(b" showing ")
                .untrusted(app.path().as_bytes())
                .end();
        }
        let named = app.naming_len();
        if named != reported_name {
            reported_name = named;
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
        let ui = app.view(&theme, router.hovered_key(&tree));
        let l = layout(&ui, bounds, &FontMetrics::new(&font, theme.font_px));
        let damage = match tree.update(&ui, &l) {
            Ok(d) => d,
            // A malformed tree is a bug in `view`, not a runtime condition.
            Err(_) => fail(b"nxedit: the view is not diffable\n"),
        };
        if let Some(d) = damage {
            draw(&mut scratch, &ui, &l, &font, &theme, d);
            let Some(mut w) = win.window(window_id) else {
                fail(b"nxedit: our own window is gone\n");
            };
            let Ok(b) = pool.acquire(&mut w, app.window_size()) else {
                fail(b"nxedit: no buffer to draw into\n");
            };
            if !pool.write(b, scratch.bytes()) {
                fail(b"nxedit: the frame did not fit its buffer\n");
            }
            if w.commit(b, (d.origin.x as u32, d.origin.y as u32, d.size.w, d.size.h)).is_err() {
                fail(b"nxedit: Commit FAILED\n");
            }
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
                confirm = Child::open(
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
                confirm_hovered = None;
            }
            _ => {}
        }
        if let Some(c) = confirm.as_mut() {
            let now = c.hovered_key();
            confirm_hovered = now;
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
            continue;
        }

        if app.closing() {
            kprint(b"nxedit: closing\n");
            exit(0);
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
        let mut resized = false;
        for (from, event) in events {
            // **The dialog's window routes through the dialog's tree.** Same `App`, so an
            // answer's `Msg` updates the same state the main window's messages do; a different
            // tree and router, because they describe a different window. A record for a window
            // that is neither is not possible — `Session` filtered it — but a stale one for a
            // dialog just destroyed is, and it is dropped rather than routed into the buffer.
            if confirm.as_ref().is_some_and(|c| c.id() == from) {
                let ask = app.confirm_view(&theme, confirm_hovered);
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
                        Msg::KeepEditing => kprint(b"nxedit: close cancelled, still editing\n"),
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
                    if let Some(msg) = router.key(&tree, &ui, k) {
                        app.update(msg);
                    } else {
                        app.update(Msg::Key(k));
                    }
                }
                WindowEvent::Pointer(p) => {
                    let (msgs, _) = router.pointer(&tree, &ui, &l, p);
                    for m in msgs {
                        app.update(m);
                    }
                }
                WindowEvent::Focus(f) => {
                    router.set_window_focused(f);
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
                    let taken = router.drop_at(&tree, &ui, &l, x, y).is_some();
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
                        open_into(&mut app, root_ns, path);
                    }
                }
                WindowEvent::InputLost => kprint(b"nxedit: input dropped\n"),
            }
        }
        if resized {
            size = app.window_size();
            match compose_buffer(size) {
                Some(fb) => scratch = fb,
                None => fail(b"nxedit: impossible window geometry\n"),
            }
            bounds = Rect::new(0, 0, size.w, size.h);
            tree = Tree::new();
        }
    }
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"nxedit: panic\n");
    exit(2);
}
