//! `desktop-session-mgr` — the graphical login supervisor.
//!
//! `session-mgr`'s twin. It runs the same sequence through the same `libsession` —
//! authenticate, construct the namespace, spawn the leader, reap, tear down — and differs in
//! exactly one place: its **greeter is a window**, not a terminal prompt. That is the split
//! `graphical-session.md` §4 draws, and the reason `libsession` exists.
//!
//! **The greeter is a compositor client that exists before anyone has logged in**, which makes
//! this the first process to hold a `/dev/draw` connection in a *release* image. Everything
//! graphical before it was `selftest`-gated. It is closer to `gdm`'s `class=greeter` than to
//! anything `session-mgr` does: it draws first and outlives every session it starts.
//!
//! **No `/dev/console` in a graphical session.** `libsession::NamespaceSpec::bind_console` is
//! false here, and this is that flag's first caller — governing decision 3.
//!
//! `#![no_std]` + `#![no_main]`, with `alloc`: the same reasoning as `session-mgr`'s, plus a
//! toolkit that builds an `Element` tree per frame.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;
use libkern::debug::Line;
use libkern::*;
use librsproto::surface::{CreateWindowRequest, Role};
use libsurface::Session;
use libsurface::ipc::ChannelTransport;
use libui::element::{Element, Insets, column, padding, row, sized, text};
use libui::layout::layout;
use libui::paint::{FontMetrics, Theme, paint};
use libui::widget::{TextFieldState, WidgetState, text_field};
use libsession::{NamespaceSpec, authenticate, build_namespace, ns_lookup, spawn_leader};

/// `alloc` backing: the toolkit builds an element tree per frame and `libsession` builds the
/// session's environment record.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Where the font comes from — the same path every other graphical client reads.
const SYSTEM_FONT_PATH: &str = "/system/fonts/DejaVuSansMono.ttf";

/// The greeter window's size. Fixed rather than screen-relative: the compositor places it at
/// the origin today, and a greeter that resized itself would be the first client to have a
/// placement opinion — which is `desktop-shell`'s job from Part E.
const GREETER_W: u32 = 420;
/// See [`GREETER_W`].
const GREETER_H: u32 = 200;
/// Bytes per row. `WIDTH * 4` exactly: nothing here needs the padded pitch the reference UI
/// uses to catch stride bugs, and an unpadded one keeps the buffer copy a memcpy.
const GREETER_PITCH: usize = (GREETER_W as usize) * 4;
/// How many buffers the greeter attaches.
const BUFFERS: usize = 2;

/// How many redraws to report before going quiet.
///
/// Generous: two login attempts at this demo's credential lengths is about seventy.
const MAX_REDRAWS_LOGGED: u32 = 512;

/// Redraws so far — see [`present`].
static mut REDRAWS: u32 = 0;

/// Write one line to the debug console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Report and exit. A greeter that cannot draw is not a degraded greeter.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

/// Which field the keyboard is going to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// The username field.
    User,
    /// The password field.
    Password,
}

/// Everything the greeter draws from.
struct Greeter {
    /// The username being typed.
    user: TextFieldState,
    /// The password being typed. Rendered masked.
    password: TextFieldState,
    /// Which field has the caret.
    focus: Focus,
    /// Whether the last attempt was refused.
    denied: bool,
}

/// `EV_KEY` codes the greeter acts on itself. The fields claim everything else through
/// [`TextFieldState::apply`], which declines exactly these three so they can reach here — the
/// reason `Element::on_key` returns an `Option` at all.
const KEY_TAB: u16 = 15;
/// See [`KEY_TAB`].
const KEY_ENTER: u16 = 28;

impl Greeter {
    /// An empty greeter, caret in the username field.
    fn new() -> Self {
        Self {
            user: TextFieldState::new(),
            password: TextFieldState::new(),
            focus: Focus::User,
            denied: false,
        }
    }

    /// The element tree for the current state.
    ///
    /// Rebuilt per frame, which is the toolkit's model: `view(&state) -> Element`.
    ///
    /// **The theme comes from the caller**, because the caller is what paints this tree — and it
    /// paints with `Theme { font_px: FONT_PX, .. }`, equal to the default today only because
    /// `FONT_PX` happens to be `16.0`. One frame built from one theme and painted with another is
    /// the mistake one type makes easy and the old two-type split made unwriteable
    /// (PR #262 review, optional 5).
    fn view(&self, theme: &Theme) -> Element<()> {
        let active = |f: Focus| WidgetState { active: self.focus == f, ..Default::default() };
        let mut rows = alloc::vec::Vec::with_capacity(6);
        rows.push(text("nitrox"));
        if self.denied {
            // Said once, above the fields, and cleared on the next keystroke. The serial
            // column prints `login incorrect` for the same reason: a refusal a user cannot
            // see is a login that appears to have done nothing.
            rows.push(text("login incorrect"));
        }
        rows.push(row(alloc::vec![
            sized(Size::new(90, 0), text("username")),
            text_field(&self.user, false, active(Focus::User), theme).flex(1),
        ]));
        rows.push(row(alloc::vec![
            sized(Size::new(90, 0), text("password")),
            text_field(&self.password, true, active(Focus::Password), theme).flex(1),
        ]));
        padding(Insets::all(16), column(rows))
    }

    /// The field the caret is in.
    fn active_field(&mut self) -> &mut TextFieldState {
        match self.focus {
            Focus::User => &mut self.user,
            Focus::Password => &mut self.password,
        }
    }

    /// Apply a key. `true` if anything changed and the greeter must be redrawn.
    ///
    /// **Tab and Enter are handled here, not by the field**, which is the split
    /// `Element::on_key`'s `Option` return exists for: a field that swallowed Tab could never
    /// be left, and one that swallowed Enter could never submit.
    fn key(&mut self, keycode: u16, modifiers: u16) -> bool {
        match keycode {
            KEY_TAB => {
                self.focus = match self.focus {
                    Focus::User => Focus::Password,
                    Focus::Password => Focus::User,
                };
                true
            }
            _ => {
                // Any edit clears a previous refusal: a "login incorrect" that outlives the
                // typing that answers it reads as a second failure.
                let changed = self.active_field().apply(keycode, modifiers);
                if changed && self.denied {
                    self.denied = false;
                }
                changed
            }
        }
    }

    /// Clear both fields and put the caret back — after a session ends, and after a refusal.
    ///
    /// **The password leaves the screen the moment it has been read**, whichever way the
    /// attempt went. A greeter outlives every session it starts, so one left in the field
    /// would sit behind whatever the session drew.
    ///
    /// **Not a scrub.** `String::clear` sets the length to zero and leaves the bytes in the
    /// allocation, so this is a claim about what is displayed and what a later attempt can
    /// read back — not about this process's memory. The caller's stack copy is
    /// volatile-zeroed after the session ends, which is the same distinction (PR #236 review,
    /// finding 8).
    fn reset(&mut self) {
        self.user.clear();
        self.password.clear();
        self.focus = Focus::User;
    }

    /// Render the current state into a fresh framebuffer.
    fn render(&self, font: &Font) -> MemFramebuffer {
        let geometry = Geometry::with_pitch(GREETER_W, GREETER_H, GREETER_PITCH, PixelFormat::XRGB8888)
            .expect("the greeter pitch is wide enough for a row");
        let mut fb = MemFramebuffer::new(geometry);
        // **The built-in theme, and the greeter is the one surface that cannot have another.**
        // A theme lives in a user's home (M11 Part C) and this runs *before* there is a user —
        // asking who they are is what this window is for. Built once and used for both the tree
        // and the paint, so one frame is never two themes.
        let theme = Theme::default();
        let ui = self.view(&theme);
        let bounds = Rect::new(0, 0, GREETER_W, GREETER_H);
        let metrics = FontMetrics::new(font, theme.font_px);
        let l = layout(&ui, bounds, &metrics);
        paint(&mut fb, font, &theme, &ui, &l, bounds, &mut |_, _, _, _: &mut MemFramebuffer| {});
        fb
    }
}

/// Allocate a shared memory object of `len` bytes and map it writable.
fn shared_buffer(len: usize) -> Option<(u64, *mut u8)> {
    // SAFETY: a plain anonymous object of `len` bytes.
    let h = unsafe { syscall4(SYS_MEMORY_CREATE, len as u64, 0, 0, 0) };
    if h <= 0 {
        return None;
    }
    // SAFETY: maps the object read/write at a kernel-chosen address.
    let base = unsafe {
        syscall4(SYS_MEMORY_MAP, h as u64, 0, len as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if base < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
        return None;
    }
    Some((h as u64, base as usize as *mut u8))
}

/// Receive the next control message on `ctrl` and return its transferred `handles[0]`.
///
/// A handoff carries exactly one moved handle and no payload; `0` on failure. The same shape
/// `session-mgr` uses, and positional for the same reason — `service-mgr` sends an empty
/// message for an endpoint it does not have, so a missing one shortens no one's count.
fn recv_handoff(ctrl: u64) -> u64 {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = ctrl;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    if waited != 1 {
        return 0;
    }
    // SAFETY: valid recv out-params.
    let r = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ctrl,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if r != 0 {
        return 0;
    }
    // SAFETY: the kernel wrote `RECV_COUNT` transferred handles into `RECV_HANDLES`.
    unsafe {
        if RECV_COUNT == 0 { 0 } else { RECV_HANDLES[0] }
    }
}

/// Buffers for the endpoint handoff.
static mut RECV_MSG: [u8; 256] = [0; 256];
/// See [`RECV_MSG`].
static mut RECV_HANDLES: [u64; 8] = [0; 8];
/// See [`RECV_MSG`].
static mut RECV_COUNT: usize = 0;

/// One attempt at a login: authenticate, build the session, run the leader, tear down.
///
/// Returns once the session has ended, so the greeter can draw again. `false` if the
/// credentials were refused, which is the only outcome the greeter shows differently.
#[allow(clippy::too_many_arguments)]
fn run_session(
    root_ns: u64,
    notif: u64,
    auth_ch: u64,
    fs: u64,
    profile: u64,
    tty: u64,
    draw: u64,
    user: &[u8],
    password: &[u8],
) -> bool {
    let mut home = [0u8; 256];
    let Some(hl) = authenticate(auth_ch, user, password, &mut home) else {
        kprint(b"desktop-session-mgr: login denied\n");
        return false;
    };
    Line::new().s(b"desktop-session-mgr: login ok -> home=").s(&home[..hl]).end();

    // **No `/dev/console` in a graphical session** — governing decision 3, and this is that
    // flag's first caller since `libsession` gained it in Part B. Not "bound and unused": a
    // binding a session holds is authority it has, and the console is shared with the serial
    // column, which is the recovery path.
    let session_ns = build_namespace(&NamespaceSpec {
        root_ns,
        fs_endpoint: fs,
        profile_endpoint: profile,
        tty_endpoint: tty,
        home: &home[..hl],
        user,
        // Its clients render text, and a constructed namespace has no font without this.
        bind_fonts: true,
        bind_console: false,
    });
    if session_ns == 0 {
        kprint(b"desktop-session-mgr: session namespace FAIL\n");
        return true;
    }
    // **`/dev/draw`, bound as a subtree and unscoped**, so the shell resolves both `new` (a
    // window of its own) and `manage` (the manager channel). That breadth is deliberate and is
    // exactly what an *application's* namespace will not get: Part E binds `/dev/draw/new`
    // alone into those, which is what closed the `manage-ungated` deferral — a narrow bind
    // expresses
    // "new and not manage" without any protocol change.
    if draw != 0 {
        let path = b"/dev/draw";
        // SAFETY: valid namespace handle, path pointer and endpoint handle.
        let dr = unsafe {
            syscall4(SYS_NS_BIND, session_ns, path.as_ptr() as u64, path.len() as u64, draw)
        };
        if dr != 0 {
            // Non-fatal: the session exists, the shell just cannot draw in it. Say so rather
            // than failing a login the user completed.
            kprint(b"desktop-session-mgr: /dev/draw bind FAIL (the shell cannot draw)\n");
        } else {
            kprint(b"desktop-session-mgr: /dev/draw bound into the session\n");
        }
    }
    // **Reported from what was bound, not from what was asked for.** This was a hardcoded
    // "(no /dev/console)" and the gate's assertion on it passed with `bind_console: true` —
    // decoration, found by running the control rather than by reading it.
    if libsession::session_has_console() {
        kprint(b"desktop-session-mgr: session namespace built (WITH /dev/console)\n");
    } else {
        kprint(b"desktop-session-mgr: session namespace built (no /dev/console)\n");
    }

    // `desktop-shell` is the leader here where `nxsh` is the serial column's. Part E makes it
    // a real shell; what it has to be now is a process that proves the session runs.
    // **`BIND_NAMESPACE`, and only the graphical leader gets it.** The shell constructs a
    // namespace per application it launches; `nxsh` is handed one and constructs nothing.
    // The compositor endpoint travels with it: the shell binds `/dev/draw/new` into every
    // application namespace it builds, and a binding cannot be re-bound.
    // **Four endpoints, in the order the shell reads them.** It binds all four into the
    // namespaces it constructs per application — the compositor's so a window can be made, the
    // fs-server's so text can be rendered, the tty server's so a terminal emulator can obtain a
    // terminal, and the profile server's so an application can spawn programs. Each is an
    // *endpoint* rather than the session's binding of it, because a binding resolves to a
    // kernel registration and never back to one.
    // **The real home, as an argument.** The shell binds `/home` into every application
    // namespace it constructs, and that bind needs the subtree base — which its own namespace
    // cannot tell it, because there `/home` *is* the home and a binding does not resolve back
    // to its base. Not authority: the fs endpoint above is the authority, and this only says
    // which subtree of it an application should see (PR #238 review, finding 3).
    let home_str = core::str::from_utf8(&home[..hl]).unwrap_or("");
    let code = spawn_leader(
        root_ns,
        session_ns,
        notif,
        "desktop-shell",
        &[home_str],
        SYSCAP_BIND_NAMESPACE,
        &[draw, fs, tty, profile],
    );
    // The leader has been reaped, so this drops the last reference to the namespace and with
    // it every binding in it.
    // SAFETY: closing the namespace we created for this session.
    unsafe { syscall1(SYS_HANDLE_CLOSE, session_ns) };
    Line::new().s(b"desktop-session-mgr: session ended (leader exit ").i(code as i64).s(b")").end();
    true
}

/// Bootstrap registers, as `service-mgr`'s spawn fills them: `rdi` = the notification channel
/// this supervisor reaps its session leader on, `rsi` = the inherited LOOKUP-only root
/// namespace, `rdx` = the control channel the endpoints arrive over, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"desktop-session-mgr: up\n");

    // The endpoints, in `service-mgr`'s send order. Positional, like the serial column's.
    let fs_endpoint = recv_handoff(control);
    let profile_endpoint = recv_handoff(control);
    let tty_endpoint = recv_handoff(control);
    // The compositor's forwarding endpoint — the one handoff the serial column does not get.
    // `desktop-shell` needs `/dev/draw` **bound in the namespace it runs in**, not a
    // connection handed to it: it resolves `manage` as well as `new`, and Part E gates
    // applications by binding those two paths differently.
    let draw_endpoint = recv_handoff(control);
    // The oracle, resolved rather than couriered — Part C. Once at startup: its lifetime is
    // the machine's, and re-resolving per attempt would mint a session per keystroke.
    let (auth_status, auth_ch) = ns_lookup(root_ns, b"/svc/auth", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if fs_endpoint == 0 || auth_status != 0 || auth_ch == 0 {
        // A greeter that cannot authenticate is a window that wastes a screen. Say which half
        // is missing — the two have completely different causes.
        if fs_endpoint == 0 {
            kprint(b"desktop-session-mgr: no fs endpoint; cannot build a session\n");
        } else {
            kprint(b"desktop-session-mgr: /svc/auth resolve FAIL\n");
        }
        fail(b"desktop-session-mgr: no graphical login\n");
    }

    // The font, before the window: a greeter that cannot draw text has nothing to show, and
    // failing here reports the real cause rather than an empty window.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let font = match unsafe { libdraw::text::load(root_ns, SYSTEM_FONT_PATH) } {
        Ok(f) => f,
        Err(_) => fail(b"desktop-session-mgr: font load FAILED\n"),
    };

    // SAFETY: `root_ns` is live for this process's whole run.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => t,
        Err(_) => fail(b"desktop-session-mgr: connect to /dev/draw FAILED\n"),
    };
    let mut session = Session::new(transport);
    // **This window lands at the origin, and it is created before every other client's.**
    // `service-mgr` brings the login chain up before it starts declared services, so the
    // greeter is bottom-most and the reference windows `check-display` and `check-terminal`
    // depend on stack above it. That is load-bearing rather than incidental: a greeter
    // created *after* them would cover the regions those gates compare, and the failure would
    // read as a compositing regression. The plan's Milestone 6 Part D says the same thing
    // about `init`'s spawn order — "a stale comment asserting a retired invariant is what the
    // last two milestones each shipped once".
    let mut greeter = Greeter::new();
    let len = GREETER_PITCH * GREETER_H as usize;
    // **The mapped addresses are kept**, not dropped after attach: every keystroke redraws,
    // so the greeter writes new pixels into whichever buffer the compositor has released.
    let mut addrs = [core::ptr::null_mut::<u8>(); BUFFERS];
    let mut window = match open_greeter(&mut session, &font, &greeter, &mut addrs, len) {
        Some(id) => id,
        None => fail(b"desktop-session-mgr: greeter could not be drawn\n"),
    };
    Line::new()
        .s(b"desktop-session-mgr: greeter presented, window ")
        .u(window as u64)
        .s(b" ")
        .u(GREETER_W as u64)
        .s(b"x")
        .u(GREETER_H as u64)
        .end();

    // **The greeter loop.** Blocks on the event channel, never spins: a supervisor that spins
    // keeps a run queue non-empty, the idle thread never runs, and deferred handle
    // reclamation lives there — so every exited process on the system stops being reaped.
    // That is the 2026-07-31 `logging-service` bug, and `session-mgr`'s park comment records
    // the same thing.
    let ev = session.wait_handle();
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
        unsafe {
            WAIT_HANDLES[0] = ev;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if session.pump().is_err() {
            fail(b"desktop-session-mgr: compositor connection lost\n");
        }
        let mut dirty = false;
        let mut submit = false;
        while let Some((w, event)) = session.next_event() {
            if w != window {
                continue;
            }
            if let libsurface::WindowEvent::Key(k) = event {
                // Presses only. A greeter that acted on releases would type every character
                // twice, and the repeat the compositor sends is a press too — which is what
                // makes holding backspace work without any code here.
                if k.pressed == 0 {
                    continue;
                }
                if k.keycode == KEY_ENTER {
                    submit = true;
                } else if greeter.key(k.keycode, k.modifiers) {
                    dirty = true;
                }
            }
        }
        if submit {
            // Copied out before the fields are cleared, and cleared before the session runs:
            // the password must not still be in the greeter while a session is on screen.
            let mut user = [0u8; 64];
            let mut pass = [0u8; 128];
            let ul = greeter.user.text().len().min(user.len());
            let pl = greeter.password.text().len().min(pass.len());
            user[..ul].copy_from_slice(&greeter.user.text().as_bytes()[..ul]);
            pass[..pl].copy_from_slice(&greeter.password.text().as_bytes()[..pl]);
            greeter.reset();
            // **The greeter's window goes away for the duration of the session.**
            //
            // `graphical-session.md` says the greeter *outlives* each session, and that is
            // about the **process**, not the window: a login window left mapped sits at the
            // origin underneath whatever the session draws, and the moment anything raises it
            // it covers the shell's top bar — which is exactly what happened when a dropped
            // PS/2 packet sent a click to (160, 112) instead of the bar, and every later click
            // then landed on the greeter instead. Found by a negative control, not by
            // reasoning (M7 Part E).
            //
            // Destroyed rather than hidden because the protocol has no "hide": a window is
            // mapped or it does not exist, and recreating one is what M6 Part A's
            // initial-configure handshake is already built for.
            if let Some(w) = session.window(window) {
                let _ = w.destroy();
            }
            let ok = run_session(
                root_ns, notif, auth_ch, fs_endpoint, profile_endpoint, tty_endpoint,
                draw_endpoint, &user[..ul], &pass[..pl],
            );
            // SAFETY: a local buffer this function owns; zeroed so a refused password does
            // not sit in this process's stack for the machine's lifetime.
            unsafe { core::ptr::write_volatile(&mut pass, [0u8; 128]) };
            // **Set before the window is drawn, not after.** `open_greeter` presents, and
            // `view()` reads `denied` — so setting it afterwards drew a blank greeter and the
            // refusal never appeared. The next keystroke clears `denied` before its own
            // redraw, so the message was reachable only by pressing Tab, and the gate asserts
            // the *kprint* rather than the rendering, so nothing caught it (PR #237 review,
            // finding 4). `Greeter::view`'s own comment describes what that state is: a
            // refusal a user cannot see is a login that appears to have done nothing.
            greeter.denied = !ok;
            // Back to a login window. A fresh one rather than a retained one, for the reason
            // above — and its buffers with it, since the old window's are gone.
            match open_greeter(&mut session, &font, &greeter, &mut addrs, len) {
                Some(id) => window = id,
                None => fail(b"desktop-session-mgr: could not draw the greeter again\n"),
            }
            dirty = false;
        }
        if dirty && !present(&mut session, window, &greeter, &font, &addrs, len) {
            fail(b"desktop-session-mgr: greeter redraw FAILED\n");
        }
    }
}

/// Create the greeter window, attach fresh buffers, and present it. `None` if any step failed.
///
/// Called for the first login and again after every session, because the window is destroyed
/// for the duration of one — see the call site for why.
fn open_greeter(
    session: &mut Session<ChannelTransport>,
    font: &Font,
    greeter: &Greeter,
    addrs: &mut [*mut u8; BUFFERS],
    len: usize,
) -> Option<u32> {
    // **This window lands at the origin, and is created before every other client's.**
    // `service-mgr` brings the login chain up before it starts declared services, so the
    // greeter is bottom-most and the reference windows `check-display` and `check-terminal`
    // depend on stack above it. That is load-bearing rather than incidental: a greeter created
    // *after* them would cover the regions those gates compare, and the failure would read as
    // a compositing regression.
    let window = session
        .create(&CreateWindowRequest::new(GREETER_W, GREETER_H, Role::Normal), BUFFERS)
        .ok()?;
    for i in 0..BUFFERS {
        let (handle, addr) = shared_buffer(len)?;
        addrs[i] = addr;
        let mut w = session.window(window)?;
        if w.attach(i as u32, GREETER_W, GREETER_H, GREETER_PITCH as u32, handle).is_err() {
            return None;
        }
    }
    if !present(session, window, greeter, font, addrs, len) {
        return None;
    }
    Some(window)
}

/// Render the greeter into a free buffer and commit it. `false` if the compositor refused.
fn present(
    session: &mut Session<ChannelTransport>,
    window: u32,
    greeter: &Greeter,
    font: &Font,
    addrs: &[*mut u8; BUFFERS],
    len: usize,
) -> bool {
    let picture = greeter.render(font).into_bytes();
    if picture.len() != len {
        return false;
    }
    let Some(mut w) = session.window(window) else {
        return false;
    };
    // `acquire` blocks for a buffer the compositor is not displaying, which is what makes a
    // redraw safe: writing into the committed one would tear the picture on screen.
    let Ok(slot) = w.acquire() else {
        return false;
    };
    let addr = addrs[slot as usize % BUFFERS];
    // SAFETY: `addr` maps `len` writable bytes and `picture` holds exactly `len`; the two are
    // distinct allocations, so they cannot overlap.
    unsafe { core::ptr::copy_nonoverlapping(picture.as_ptr(), addr, len) };
    let ok = w.commit(slot, (0, 0, GREETER_W, GREETER_H)).is_ok();
    // **A redraw counter, and it is not decoration.** A greeter has no echo: what a person
    // typed is on screen and nowhere else, so a harness driving it has nothing to pace
    // against — and this window repaints 420×200 per keystroke, which is exactly the cost
    // `check-terminal` types one character at a time to stay behind. This line is what the
    // graphical login gate waits on between keystrokes.
    //
    // **A count, deliberately, and never the fields.** Reporting what was typed would put a
    // password on the console; reporting its *length* would leak it more slowly. A monotonic
    // count paces a harness, says "the greeter is still redrawing" to anyone debugging one
    // that is not, and tells an observer nothing about the credential.
    //
    // Unconditional, like the compositor's press diagnostics, and capped for the same reason.
    // SAFETY: single-threaded supervisor; one redraw at a time.
    unsafe {
        if REDRAWS < MAX_REDRAWS_LOGGED {
            REDRAWS += 1;
            Line::new().s(b"desktop-session-mgr: greeter redraw ").u(REDRAWS as u64).end();
        }
    }
    ok
}

/// Wait set for the greeter's event channel.
static mut WAIT_HANDLES: [u64; 1] = [0; 1];
/// One 24-byte `IoResult`.
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"desktop-session-mgr: PANIC\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}
