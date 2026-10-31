//! `ui-testclient` — the display arm's first real client: a second process that connects over
//! `/dev/draw`, shares real memory, and requires the compositor to talk back.
//!
//! It proves a session exists, `CreateWindow` is answered, two `MemoryObject`s cross once, and
//! `Release` comes back: it commits more frames than it has buffers, so from the third it proceeds
//! only if the compositor released the buffer that left the screen. A one-way protocol stalls.
//!
//! It leaves the reference scene on screen, so `check-display` compares a picture that came through
//! the whole protocol. **On success it parks rather than exits**: an exiting client's windows are
//! destroyed, and the gate captured an empty screen that looked like a compositing failure.
//! Failures go to `sys_test_exit` directly. Beside the scene it presents [`libui::reference`],
//! drawn with a font read from disk, and the terminal's reference, which the gate compares with the
//! host's render. **The UI window is created first**, so the scene stacks above it; the other order
//! fails the comparison.
//!
//! Windows have one connection each, except in `verify_popup_placement`: it is also the manager,
//! and `Session::create` blocks for a `normal` window's first `Configure`, which only the manager
//! can release.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::framebuffer::Framebuffer;
use libdraw::scene;
use libkern::debug::Line;
use libkern::{SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_WAIT, exit, kprint, syscall2, syscall4};
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, ConfigureEvent, CreateWindowRequest, FocusEvent, MgrWindowCreated,
    MgrWindowRef, OP_ATTACH_BUFFER, OP_COMMIT, OP_CONFIGURE, OP_FOCUS_EVENT, OP_MGR_WINDOW_CREATED,
    OP_MGR_WINDOW_DESTROYED, OP_MGR_WINDOW_FOCUS, OP_MGR_WINDOW_GEOMETRY, ROLE_NORMAL, Role, SURFACE_FORMAT_XRGB8888,
    build_attach_buffer_request, build_commit_request,
};
use libsurface::{Session, Transport, ipc::ChannelTransport};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Frames to commit: more than the buffer count on purpose, so frame 3 needs a `Release`.
const FRAMES: usize = 6;
const BUFFERS: usize = 2;

/// Window-churn cycles, sized so the run **fails on a leak rather than merely leaking**: a break in
/// one of the two alternating disposal paths leaks on half the cycles, and 64 at 3 MiB is 192 MiB
/// against a 256 MiB guest. At 80 cycles such a break leaked 120 MiB, which the guest absorbed.
const CHURN_CYCLES: usize = 128;
/// Churn window width — the buffer is `CHURN_W * 4 * CHURN_H` = 3 MiB.
const CHURN_W: u32 = 1024;
const CHURN_H: u32 = 768;

/// A session holding exactly one window, the shape most of this client uses.
struct Win {
    session: Session<alloc::boxed::Box<ChannelTransport>>,
    id: u32,
}

impl Win {
    fn open(
        t: alloc::boxed::Box<ChannelTransport>,
        width: u32,
        height: u32,
        role: Role,
    ) -> Result<Self, libsurface::UiError> {
        let mut session = Session::new(t);
        let id = session.create(&CreateWindowRequest::new(width, height, role), BUFFERS)?;
        Ok(Self { session, id })
    }

    fn id(&self) -> u32 {
        self.id
    }

    fn w(&mut self) -> libsurface::WindowRef<'_, alloc::boxed::Box<ChannelTransport>> {
        self.session.window(self.id).expect("this session's only window")
    }

    fn attach(&mut self, b: u32, width: u32, height: u32, pitch: u32, handle: u64) -> Result<(), libsurface::UiError> {
        self.w().attach(b, width, height, pitch, handle)
    }

    fn commit(&mut self, b: u32, damage: (u32, u32, u32, u32)) -> Result<(), libsurface::UiError> {
        self.w().commit(b, damage)
    }

    fn acquire(&mut self) -> Result<u32, libsurface::UiError> {
        self.w().acquire()
    }

    fn destroy(&mut self) -> Result<(), libsurface::UiError> {
        self.w().destroy()
    }

    fn into_transport(self) -> alloc::boxed::Box<ChannelTransport> {
        self.session.into_transport()
    }
}

/// Create a `MemoryObject` of `len` bytes and map it read-write; returns `(handle, address)`.
fn shared_buffer(len: usize) -> Option<(u64, *mut u8)> {
    // SAFETY: a plain anonymous object of `len` bytes.
    let h = unsafe { syscall4(SYS_MEMORY_CREATE, len as u64, 0, 0, 0) };
    if h <= 0 {
        return None;
    }
    // SAFETY: mapping an object this process just created, read-write.
    let addr = unsafe {
        syscall4(SYS_MEMORY_MAP, h as u64, 0, len as u64, libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE)
    };
    if addr <= 0 {
        // SAFETY: the map failed, so nothing references the object; closing our only
        // handle to it. Only reachable when allocation is already failing — but this file
        // exists to prove leaks absent, so it should not contain one.
        unsafe { syscall4(libkern::SYS_HANDLE_CLOSE, h as u64, 0, 0, 0) };
        return None;
    }
    Some((h as u64, addr as *mut u8))
}

/// Resolve `/dev/draw/<id>/info`, map it, and return the snapshot.
fn read_window_info(root_ns: u64, id: u32) -> Option<librsproto::surface::WindowInfo> {
    use libkern::handle::{RawHandle, Rights};
    use libos::{Handle, MapRead, Memory, Namespace, NsReadOnly, block_on};

    let mut path = [0u8; 32];
    let prefix = b"/dev/draw/";
    path[..prefix.len()].copy_from_slice(prefix);
    let mut n = prefix.len();
    let mut digits = [0u8; 10];
    let (mut d, mut v) = (0usize, id);
    if v == 0 {
        digits[0] = b'0';
        d = 1;
    }
    while v > 0 {
        digits[d] = b'0' + (v % 10) as u8;
        v /= 10;
        d += 1;
    }
    for j in 0..d {
        path[n] = digits[d - 1 - j];
        n += 1;
    }
    path[n..n + 5].copy_from_slice(b"/info");
    n += 5;
    let path = core::str::from_utf8(&path[..n]).ok()?;

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let ns = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: the path resolves to a read-mappable object holding one `WindowInfo`.
    let obj = block_on(unsafe { ns.lookup::<Memory, MapRead>(path, Rights::MAP_READ) }).ok()?;
    // **The constant, not a literal**: a stale length would silently make this return `None`.
    const N: usize = librsproto::surface::WINDOW_INFO_LEN;
    let addr = obj.map(N).ok()?;
    // SAFETY: the compositor serves exactly `N` bytes of `WindowInfo` here.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, N) };
    let info = librsproto::surface::WindowInfo::read(bytes);
    // SAFETY: the mapping made above, `bytes` done with; each poll maps a fresh object to unmap.
    let _ = unsafe { obj.unmap(addr, N) };
    info
}

fn check_info(root_ns: u64, id: u32, want_w: u32, want_h: u32) -> bool {
    let Some(info) = read_window_info(root_ns, id) else { return false };

    Line::new()
        .s(b"ui-testclient: info id=")
        .u(info.id as u64)
        .s(b" ")
        .u(info.width as u64)
        .s(b"x")
        .u(info.height as u64)
        .s(b" role=")
        .u(info.role as u64)
        .end();

    info.id == id && info.width == want_w && info.height == want_h && info.role == librsproto::surface::ROLE_NORMAL
}

/// Open and close windows in a loop, proving the compositor gives the memory back.
#[inline(never)]
fn churn(root_ns: u64) -> bool {
    // **Boxed**: a `ChannelTransport` is ~9 KiB against a 32 KiB stack.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let mut t = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => return false,
    };
    let pitch = CHURN_W as usize * 4;
    let len = pitch * CHURN_H as usize;
    for cycle in 0..CHURN_CYCLES {
        let mut w = match Win::open(t, CHURN_W, CHURN_H, Role::Normal) {
            Ok(w) => w,
            Err(_) => {
                Line::new().s(b"churn: Win::open failed at cycle ").u(cycle as u64).end();
                return false;
            }
        };
        let Some((handle, addr)) = shared_buffer(len) else {
            Line::new().s(b"churn: real buffer alloc failed at cycle ").u(cycle as u64).end();
            return false;
        };
        if w.attach(0, CHURN_W, CHURN_H, pitch as u32, handle).is_err() {
            return false;
        }
        if w.destroy().is_err() {
            return false;
        }
        t = w.into_transport();
        // SAFETY: unmapping a range this process mapped in `shared_buffer`.
        unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, len as u64) };

        // **A rejected attach must also give the memory back**: sent raw, as `libsurface` cannot
        // express it.
        let Some((bogus_handle, bogus_addr)) = shared_buffer(len) else {
            Line::new().s(b"churn: bogus buffer alloc failed at cycle ").u(cycle as u64).end();
            return false;
        };
        // Alternates two disposal paths: a rejected attach exercises the outcome branch, a handle
        // on
        // `Commit` the op branch; only the latter fails if the close narrows to `op ==
        // OP_ATTACH_BUFFER`.
        let mut req = [0u8; 32];
        let built = if cycle % 2 == 0 {
            build_attach_buffer_request(
                &mut req,
                &AttachBufferRequest {
                    window: 0xDEAD_BEEF,
                    buffer: 0,
                    width: CHURN_W,
                    height: CHURN_H,
                    pitch: pitch as u32,
                    format: SURFACE_FORMAT_XRGB8888,
                },
            )
            .map(|n| (n, OP_ATTACH_BUFFER))
        } else {
            build_commit_request(
                &mut req,
                &CommitRequest {
                    window: 0xDEAD_BEEF,
                    buffer: 0,
                    damage_x: 0,
                    damage_y: 0,
                    damage_w: CHURN_W,
                    damage_h: CHURN_H,
                },
            )
            .map(|n| (n, OP_COMMIT))
        };
        let Some((rn, bogus_op)) = built else {
            return false;
        };
        // **Waits for the refusal**: it paces the loop; anything but `Err(Server)` means a bogus
        // attach was accepted.
        let mut reply = [0u8; 32];
        match t.request(bogus_op, &req[..rn], Some(bogus_handle), &mut reply) {
            Err(libsurface::UiError::Server) => {}
            _ => {
                Line::new().s(b"churn: bogus request was not refused at cycle ").u(cycle as u64).end();
                return false;
            }
        }
        // SAFETY: unmapping a range this process mapped in `shared_buffer`.
        unsafe { syscall2(SYS_MEMORY_UNMAP, bogus_addr as u64, len as u64) };
    }
    true
}

/// Load one of the two faces the image ships, or die saying which and why: a font that fails to
/// load must not leave a green boot with no text.
fn load_face(root_ns: u64, face: Face) -> libdraw::text::Font {
    use libdraw::theme::Theme;

    let theme = Theme::default();
    let (wanted, loaded) = match face {
        Face::Ui => (theme.font_ui, unsafe {
            // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
            libdraw::text::load_ui(root_ns, &theme, b"ui-testclient")
        }),
        Face::Mono => (theme.font_mono, unsafe {
            // SAFETY: as above.
            libdraw::text::load_mono(root_ns, &theme, b"ui-testclient")
        }),
    };
    match loaded {
        // **The path opened, not the one asked for**: `check-display` asserts against this line.
        Ok((f, opened)) => {
            Line::new().s(b"ui-testclient: font loaded ").s(opened.as_str().as_bytes()).end();
            f
        }
        Err(e) => {
            Line::new().s(b"ui-testclient: ").s(wanted.as_str().as_bytes()).s(b" ").s(e.why()).end();
            fail(b"ui-testclient: font load FAILED\n");
        }
    }
}

enum Face {
    Ui,
    Mono,
}

/// Present [`libui::reference`] in a window of its own, drawn with `font`; the caller keeps the
/// window.
fn present_reference_ui(root_ns: u64, font: &libdraw::text::Font) -> Win {
    let (w, h) = (libui::reference::WIDTH, libui::reference::HEIGHT);
    let pitch = libui::reference::PITCH;
    let len = pitch * h as usize;
    let picture = libui::reference::render(font).into_bytes();
    if picture.len() != len {
        fail(b"ui-testclient: reference UI is not the size it declares\n");
    }

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let t = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => fail(b"ui-testclient: second connect to /dev/draw FAILED\n"),
    };
    let mut win = match Win::open(t, w, h, Role::Normal) {
        Ok(win) => win,
        Err(_) => fail(b"ui-testclient: reference UI CreateWindow FAILED\n"),
    };
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            fail(b"ui-testclient: reference UI buffer alloc FAILED\n");
        };
        // SAFETY: `addr` maps `len` writable bytes and `picture` holds exactly `len`; the two
        // regions are distinct allocations, so they cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(picture.as_ptr(), addr, len) };
        if win.attach(i as u32, w, h, pitch as u32, handle).is_err() {
            fail(b"ui-testclient: reference UI AttachBuffer FAILED\n");
        }
    }
    // Commit both, then block for a release: one commit and one `acquire` proves nothing.
    for i in 0..BUFFERS {
        if win.commit(i as u32, (0, 0, w, h)).is_err() {
            fail(b"ui-testclient: reference UI Commit FAILED\n");
        }
    }
    if win.acquire().is_err() {
        fail(b"ui-testclient: reference UI never acknowledged\n");
    }
    Line::new()
        .s(b"ui-testclient: reference UI presented, window ")
        .u(win.id() as u64)
        .s(b" ")
        .u(w as u64)
        .s(b"x")
        .u(h as u64)
        .end();
    win
}

/// Present [`libterm::render::reference`] in a window of its own, the gate's third region.
///
/// **The reference, not `nxterm`'s window**: a live first frame is a banner; this stream makes each
/// line fail differently.
fn present_reference_term(root_ns: u64, font: &libdraw::text::Font) -> Win {
    use libterm::render::reference;

    let size = reference::size(font);
    let (w, h) = (size.w, size.h);
    let pitch = reference::pitch(font);
    let len = pitch * h as usize;
    let picture = reference::render_with(font).into_bytes();
    if picture.len() != len {
        fail(b"ui-testclient: reference terminal is not the size it declares\n");
    }

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let t = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => fail(b"ui-testclient: third connect to /dev/draw FAILED\n"),
    };
    let mut win = match Win::open(t, w, h, Role::Normal) {
        Ok(win) => win,
        Err(_) => fail(b"ui-testclient: reference terminal CreateWindow FAILED\n"),
    };
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            fail(b"ui-testclient: reference terminal buffer alloc FAILED\n");
        };
        // SAFETY: `addr` maps `len` writable bytes and `picture` holds exactly `len`; the two
        // regions are distinct allocations, so they cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(picture.as_ptr(), addr, len) };
        if win.attach(i as u32, w, h, pitch as u32, handle).is_err() {
            fail(b"ui-testclient: reference terminal AttachBuffer FAILED\n");
        }
    }
    for i in 0..BUFFERS {
        if win.commit(i as u32, (0, 0, w, h)).is_err() {
            fail(b"ui-testclient: reference terminal Commit FAILED\n");
        }
    }
    if win.acquire().is_err() {
        fail(b"ui-testclient: reference terminal never acknowledged\n");
    }
    Line::new()
        .s(b"ui-testclient: reference terminal presented, window ")
        .u(win.id() as u64)
        .s(b" ")
        .u(w as u64)
        .s(b"x")
        .u(h as u64)
        .end();
    win
}

/// Ask the manager channel to put `window` at `(x, y)`; `false` on failure.
///
/// The reply is not discarded: a refused `Place` moves nothing and looks identical, so the caller
/// reads the origin back.
fn place_window(mgr: &mut ChannelTransport, window: u32, x: i32, y: i32) -> bool {
    use librsproto::surface::{MgrPlace, OP_MGR_PLACE};
    let mut body = [0u8; 12];
    let req = MgrPlace { window, x, y };
    if req.write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 8];
    mgr.request(OP_MGR_PLACE, &body, None, &mut reply).is_ok()
}

/// Raise `window` to the top through the manager channel; `false` on failure.
fn raise_window(mgr: &mut ChannelTransport, window: u32) -> bool {
    use librsproto::surface::{MgrWindowRef, OP_MGR_RAISE};
    let mut body = [0u8; 8];
    if (MgrWindowRef { window, other: 0 }).write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 8];
    mgr.request(OP_MGR_RAISE, &body, None, &mut reply).is_ok()
}

/// Drive the three desktop requests over the real manager channel and read each back (the **wire**
/// is what no host test reaches); the window ends where it started.
fn verify_desktop_requests(mgr: &mut ChannelTransport, root_ns: u64, window: u32) {
    use librsproto::surface::{
        MgrDesktop, MgrWindowValue, OP_MGR_SET_CURRENT_DESKTOP, OP_MGR_SET_MINIMIZED, OP_MGR_SET_WINDOW_DESKTOP,
        STICKY_DESKTOP, WINDOW_FLAG_MINIMIZED,
    };

    fn send_value(mgr: &mut ChannelTransport, op: u16, window: u32, value: u32) -> bool {
        let mut body = [0u8; 8];
        if (MgrWindowValue { window, value }).write(&mut body).is_none() {
            return false;
        }
        let mut reply = [0u8; 8];
        mgr.request(op, &body, None, &mut reply).is_ok()
    }

    // Desktop, there and back, read back through `info` rather than trusting an `Ok`.
    if !send_value(mgr, OP_MGR_SET_WINDOW_DESKTOP, window, 2) {
        fail(b"ui-testclient: SetWindowDesktop was refused\n");
    }
    let Some(info) = read_window_info(root_ns, window) else {
        fail(b"ui-testclient: could not read back a window's desktop\n");
    };
    if info.desktop != 2 {
        Line::new()
            .s(b"ui-testclient: SetWindowDesktop had no effect -- info says desktop ")
            .u(info.desktop as u64)
            .end();
        fail(b"");
    }
    if !send_value(mgr, OP_MGR_SET_WINDOW_DESKTOP, window, 1) {
        fail(b"ui-testclient: SetWindowDesktop back to 1 was refused\n");
    }

    // Minimized, there and back: a separate bit in `info`, so folding it into desktop fails here.
    if !send_value(mgr, OP_MGR_SET_MINIMIZED, window, 1) {
        fail(b"ui-testclient: SetMinimized was refused\n");
    }
    let Some(info) = read_window_info(root_ns, window) else {
        fail(b"ui-testclient: could not read back a minimized window\n");
    };
    if info.flags & WINDOW_FLAG_MINIMIZED == 0 {
        fail(b"ui-testclient: SetMinimized had no effect -- info does not say minimized\n");
    }
    if info.desktop != 1 {
        fail(b"ui-testclient: minimizing moved the window off its desktop\n");
    }
    if !send_value(mgr, OP_MGR_SET_MINIMIZED, window, 0) {
        fail(b"ui-testclient: restoring from minimized was refused\n");
    }

    // The current desktop, and the one value it refuses: `STICKY_DESKTOP` would make new windows
    // sticky.
    let mut body = [0u8; 4];
    if (MgrDesktop { desktop: STICKY_DESKTOP }).write(&mut body).is_none() {
        fail(b"ui-testclient: could not encode a SetCurrentDesktop body\n");
    }
    let mut reply = [0u8; 8];
    if mgr.request(OP_MGR_SET_CURRENT_DESKTOP, &body, None, &mut reply).is_ok() {
        fail(b"ui-testclient: the compositor ACCEPTED a current desktop of 0\n");
    }
    // A legal switch is accepted, then straight back so the display gate's scene is unchanged.
    for d in [2u32, 1] {
        let mut body = [0u8; 4];
        if (MgrDesktop { desktop: d }).write(&mut body).is_none() {
            fail(b"ui-testclient: could not encode a SetCurrentDesktop body\n");
        }
        let mut reply = [0u8; 8];
        if mgr.request(OP_MGR_SET_CURRENT_DESKTOP, &body, None, &mut reply).is_err() {
            fail(b"ui-testclient: a legal SetCurrentDesktop was refused\n");
        }
    }
    kprint(b"ui-testclient: desktop and minimized requests round-tripped through info\n");
}

/// Place `window` at `(x, y)` and read the origin back through `/dev/draw/<id>/info`.
fn verify_placement(mgr: &mut ChannelTransport, root_ns: u64, window: u32, x: i32, y: i32) {
    if !place_window(mgr, window, x, y) {
        fail(b"ui-testclient: a manager Place was refused\n");
    }
    let Some(info) = read_window_info(root_ns, window) else {
        fail(b"ui-testclient: could not read back a placed window's info\n");
    };
    if info.x != x || info.y != y {
        Line::new()
            .s(b"ui-testclient: Place did not move window ")
            .u(window as u64)
            .s(b": asked (")
            .i(x as i64)
            .s(b",")
            .i(y as i64)
            .s(b") got (")
            .i(info.x as i64)
            .s(b",")
            .i(info.y as i64)
            .s(b")")
            .end();
        fail(b"ui-testclient: manager Place had no effect\n");
    }
}

/// One `wait_event_timeout` slice and the slice count: ~2s, so a missing event is reported *by
/// name* here.
const MGR_EVENT_SLICE_NS: u64 = 100_000_000;
const MGR_EVENT_TRIES: u32 = 20;

/// How long a popup's first `Configure` may take before it counts as held: an exempt popup is
/// answered at once, a held one waits 200 ms (`CONFIGURE_DEADLINE_NS`); only expired slices count.
const POPUP_CONFIGURE_SLICE_NS: u64 = 10_000_000;
const POPUP_CONFIGURE_SLICES: u32 = 5;

/// Wait for an event with op `want` that names window `id`, returning its body length; **filtered
/// by window**, as focus moving to a new window emits the *loss* for the old one first.
fn await_mgr(
    mgr: &mut ChannelTransport,
    want: u16,
    id: u32,
    window_of: fn(&[u8]) -> Option<u32>,
    out: &mut [u8],
    what: &[u8],
) -> usize {
    for _ in 0..MGR_EVENT_TRIES {
        match mgr.wait_event_timeout(out, MGR_EVENT_SLICE_NS) {
            Ok(Some((op, n))) => {
                if op == want && window_of(&out[..n]) == Some(id) {
                    return n;
                }
            }
            Ok(None) => {}
            Err(_) => fail(b"ui-testclient: manager channel error while awaiting an event\n"),
        }
    }
    kprint(what);
    fail(b"ui-testclient: a manager event never arrived\n");
}

fn created_window(b: &[u8]) -> Option<u32> {
    MgrWindowCreated::read(b).map(|c| c.window)
}
fn focus_window(b: &[u8]) -> Option<u32> {
    FocusEvent::read(b).map(|f| f.window)
}
fn geometry_window(b: &[u8]) -> Option<u32> {
    ConfigureEvent::read(b).map(|g| g.window)
}
fn destroyed_window(b: &[u8]) -> Option<u32> {
    MgrWindowRef::read(b).map(|d| d.window)
}

/// Throw away whatever the run queued before the probe below starts.
fn drain_mgr(mgr: &mut ChannelTransport) {
    let mut buf = [0u8; 64];
    while let Ok(Some(_)) = mgr.poll_event(&mut buf) {}
}

/// **A window is not shown until it has been configured, and the manager configures it.**
///
/// It cannot use `Window::new`, which blocks for a configure only this client, as the manager, can
/// release. Asserted: the reply arrives at once, no configure before the manager answers, and the
/// one that comes carries the origin **the manager placed**.
fn verify_initial_configure(mgr: &mut ChannelTransport, root_ns: u64) {
    drain_mgr(mgr);

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let mut t = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => fail(b"ui-testclient: B4 connect to /dev/draw FAILED\n"),
    };

    // 1. Create, without waiting for the configure.
    let (bw, bh) = (40u32, 20u32);
    let mut req = [0u8; librsproto::surface::CREATE_WINDOW_REQUEST_LEN];
    if librsproto::surface::build_create_window_request(&mut req, &CreateWindowRequest::new(bw, bh, Role::Normal))
        .is_none()
    {
        fail(b"ui-testclient: B4 could not build CreateWindow\n");
    }
    let mut reply = [0u8; 32];
    let n = match t.request(librsproto::surface::OP_CREATE_WINDOW, &req, None, &mut reply) {
        Ok(Some(n)) => n,
        _ => fail(b"ui-testclient: B4 CreateWindow got no reply\n"),
    };
    let Some(id) = librsproto::surface::parse_create_window_reply(&reply[..n]) else {
        fail(b"ui-testclient: B4 CreateWindow reply did not decode\n");
    };

    // 2. **Nothing yet**, polled since the assertion is an absence: **every queued event, not the
    // first** (a `FocusEvent` is queued ahead of any configure), and **a `FocusEvent` for this
    // window fails too**: a held window is not on screen.
    let mut buf = [0u8; 64];
    while let Ok(Some((op, n))) = t.poll_event(&mut buf) {
        if op == OP_CONFIGURE {
            fail(b"ui-testclient: a configure arrived before the manager had answered\n");
        }
        if op == OP_FOCUS_EVENT && FocusEvent::read(&buf[..n]).is_some_and(|f| f.window == id && f.focused != 0) {
            fail(b"ui-testclient: a held window was given the keyboard before it was on screen\n");
        }
    }

    // 3. Now answer, as the manager, with an origin nothing else would produce.
    await_mgr(
        mgr,
        OP_MGR_WINDOW_CREATED,
        id,
        created_window,
        &mut buf,
        b"ui-testclient: no WindowCreated for the B4 window\n",
    );
    let (px, py) = (137i32, 89i32);
    if !place_window(mgr, id, px, py) {
        fail(b"ui-testclient: B4 Place was refused\n");
    }

    // 4. The held configure arrives with the manager's placement **as the very next record**:
    // asserted, not searched for, or focus announced for an invisible window first would pass.
    let mut waited = 0;
    let cfg = loop {
        match t.wait_event_timeout(&mut buf, MGR_EVENT_SLICE_NS) {
            Ok(Some((OP_CONFIGURE, n))) => break ConfigureEvent::read(&buf[..n]),
            Ok(Some((op, _))) => {
                Line::new().s(b"ui-testclient: expected a configure, got op ").u(op as u64).end();
                fail(b"ui-testclient: a record preceded the window's first configure\n");
            }
            Ok(None) => {}
            Err(_) => fail(b"ui-testclient: B4 session channel error\n"),
        }
        waited += 1;
        if waited >= MGR_EVENT_TRIES {
            fail(b"ui-testclient: the held configure never arrived after the manager placed\n");
        }
    };
    let Some(cfg) = cfg else {
        fail(b"ui-testclient: B4 configure body did not decode\n");
    };
    if cfg.window != id || cfg.x != px || cfg.y != py {
        Line::new()
            .s(b"ui-testclient: configure for ")
            .u(cfg.window as u64)
            .s(b" at ")
            .i(cfg.x as i64)
            .s(b",")
            .i(cfg.y as i64)
            .s(b", placed ")
            .i(px as i64)
            .s(b",")
            .i(py as i64)
            .end();
        fail(b"ui-testclient: the first configure did not carry the manager's placement\n");
    }

    kprint(b"ui-testclient: the first configure carried the manager's placement\n");

    // **Closed, not leaked**: an unread session's outbox never empties, so the compositor would
    // retry for the rest of the boot.
    drop(t);
}

/// **A popup is placed by its creator, relative to its parent**: the offset resolves against the
/// parent's **current** origin, and a popup is **not** held for the manager; raw transport, as in
/// the module doc.
fn verify_popup_placement(mgr: &mut ChannelTransport, root_ns: u64) {
    drain_mgr(mgr);
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let mut t = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => fail(b"ui-testclient: C1 connect to /dev/draw FAILED\n"),
    };

    // 1. A parent moved off the default, so an offset resolved against `(0, 0)` differs.
    let parent = raw_create(&mut t, &CreateWindowRequest::new(120, 90, Role::Normal), b"C1 parent");
    let (px, py) = (211i32, 143i32);
    if !place_window(mgr, parent, px, py) {
        fail(b"ui-testclient: C1 parent Place was refused\n");
    }

    // 2. The popup, negative in x so it hangs off the parent's left edge.
    let (ox, oy) = (-14i32, 90i32);
    let popup = raw_create(&mut t, &CreateWindowRequest::at(40, 30, Role::Popup { parent }, ox, oy), b"C1 popup");

    // 3. **Not held**: nothing answered as the manager. **Only an empty slice counts** against
    // `POPUP_CONFIGURE_SLICES`: a queued record returns without waiting, so counting iterations
    // would spend the budget on other windows' events.
    let mut buf = [0u8; 64];
    let mut empty_slices = 0;
    loop {
        match t.wait_event_timeout(&mut buf, POPUP_CONFIGURE_SLICE_NS) {
            Ok(Some((OP_CONFIGURE, n))) => match ConfigureEvent::read(&buf[..n]) {
                Some(c) if c.window == popup => break,
                _ => {}
            },
            Ok(Some(_)) => {}
            Ok(None) => {
                empty_slices += 1;
                if empty_slices >= POPUP_CONFIGURE_SLICES {
                    fail(b"ui-testclient: a popup waited for the manager, which never places popups\n");
                }
            }
            Err(_) => fail(b"ui-testclient: C1 session channel error\n"),
        }
    }

    // 4. And it is where its creator asked, relative to where its parent *now* is.
    let Some(info) = read_window_info(root_ns, popup) else {
        fail(b"ui-testclient: C1 could not read the popup's info\n");
    };
    if (info.x, info.y) != (px + ox, py + oy) {
        Line::new()
            .s(b"ui-testclient: popup at ")
            .i(info.x as i64)
            .s(b",")
            .i(info.y as i64)
            .s(b", wanted ")
            .i((px + ox) as i64)
            .s(b",")
            .i((py + oy) as i64)
            .end();
        fail(b"ui-testclient: the popup was not placed at its offset from the parent\n");
    }

    kprint(b"ui-testclient: a popup was placed by its creator, without the manager\n");

    // 5. **A `dialog` is the other way round on both counts**: its parent carries desktop
    // membership and lifetime, not position, so a manager places it and it is held like a `normal`;
    // the offset is ignored. `placed_by_creator` is in the bin no host test builds, so the two
    // roles could merge unnoticed.
    let dialog = raw_create(&mut t, &CreateWindowRequest::at(24, 18, Role::Dialog { parent }, 500, 400), b"C1 dialog");

    let mut slices = 0;
    while slices < POPUP_CONFIGURE_SLICES {
        match t.wait_event_timeout(&mut buf, POPUP_CONFIGURE_SLICE_NS) {
            Ok(Some((OP_CONFIGURE, n))) => {
                if ConfigureEvent::read(&buf[..n]).is_some_and(|c| c.window == dialog) {
                    fail(b"ui-testclient: a dialog was configured with no manager answer\n");
                }
            }
            Ok(Some(_)) => {}
            Ok(None) => slices += 1,
            Err(_) => fail(b"ui-testclient: C1 dialog channel error\n"),
        }
    }

    // The manager places it, which releases the hold; the client gets the placement, not its
    // offset.
    let (dx, dy) = (301i32, 217i32);
    if !place_window(mgr, dialog, dx, dy) {
        fail(b"ui-testclient: C1 dialog Place was refused\n");
    }
    let mut waited = 0;
    let got = loop {
        match t.wait_event_timeout(&mut buf, POPUP_CONFIGURE_SLICE_NS) {
            Ok(Some((OP_CONFIGURE, n))) => match ConfigureEvent::read(&buf[..n]) {
                Some(c) if c.window == dialog => break c,
                _ => {}
            },
            Ok(Some(_)) => {}
            Ok(None) => {
                waited += 1;
                if waited >= MGR_EVENT_TRIES {
                    fail(b"ui-testclient: a placed dialog never got its held configure\n");
                }
            }
            Err(_) => fail(b"ui-testclient: C1 dialog channel error\n"),
        }
    };
    if (got.x, got.y) != (dx, dy) {
        Line::new()
            .s(b"ui-testclient: dialog at ")
            .i(got.x as i64)
            .s(b",")
            .i(got.y as i64)
            .s(b", manager placed ")
            .i(dx as i64)
            .s(b",")
            .i(dy as i64)
            .end();
        fail(b"ui-testclient: a dialog took its creator's offset instead of the placement\n");
    }

    kprint(b"ui-testclient: a dialog was held for the manager and placed by it\n");

    let mut body = [0u8; 8];
    body[..4].copy_from_slice(&parent.to_le_bytes());
    // **An empty reply buffer tells the transport the op is silent**; a real one would hang.
    let _ = t.request(librsproto::surface::OP_DESTROY_WINDOW, &body[..4], None, &mut []);
    drop(t);
}

/// `CreateWindow` through the raw transport, returning the new id. `fail`s if it did not.
fn raw_create(t: &mut ChannelTransport, req: &CreateWindowRequest, what: &[u8]) -> u32 {
    let mut body = [0u8; librsproto::surface::CREATE_WINDOW_REQUEST_LEN];
    if librsproto::surface::build_create_window_request(&mut body, req).is_none() {
        kprint(what);
        fail(b": could not build CreateWindow\n");
    }
    let mut reply = [0u8; 32];
    let n = match t.request(librsproto::surface::OP_CREATE_WINDOW, &body, None, &mut reply) {
        Ok(Some(n)) => n,
        _ => {
            kprint(what);
            fail(b": CreateWindow got no reply\n");
        }
    };
    match librsproto::surface::parse_create_window_reply(&reply[..n]) {
        Some(id) => id,
        None => {
            kprint(what);
            fail(b": CreateWindow reply did not decode\n");
        }
    }
}

/// **One window's whole life, watched from the manager channel**; each event is checked and named
/// separately.
fn verify_manager_events(mgr: &mut ChannelTransport, root_ns: u64) -> alloc::boxed::Box<ChannelTransport> {
    drain_mgr(mgr);

    // Created *after* the manager attached, so it is announced; never committed, so it paints
    // nothing.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let t = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => fail(b"ui-testclient: probe connect to /dev/draw FAILED\n"),
    };
    let (pw, ph) = (48u32, 24u32);
    let mut w = match Win::open(t, pw, ph, Role::Normal) {
        Ok(w) => w,
        Err(_) => fail(b"ui-testclient: probe CreateWindow FAILED\n"),
    };
    let id = w.id();

    let mut buf = [0u8; 64];

    // 1. Created — with the geometry and role the client asked for, not a placeholder.
    let n = await_mgr(
        mgr,
        OP_MGR_WINDOW_CREATED,
        id,
        created_window,
        &mut buf,
        b"ui-testclient: no WindowCreated for the probe window\n",
    );
    let Some(c) = MgrWindowCreated::read(&buf[..n]) else {
        fail(b"ui-testclient: WindowCreated body did not decode\n");
    };
    if c.width != pw || c.height != ph {
        Line::new().s(b"ui-testclient: WindowCreated size ").u(c.width as u64).s(b"x").u(c.height as u64).end();
        fail(b"ui-testclient: WindowCreated carried the wrong size\n");
    }
    if c.role != ROLE_NORMAL {
        fail(b"ui-testclient: WindowCreated carried the wrong role\n");
    }

    let n = await_mgr(
        mgr,
        OP_MGR_WINDOW_FOCUS,
        id,
        focus_window,
        &mut buf,
        b"ui-testclient: no WindowFocus for the probe window\n",
    );
    let Some(f) = FocusEvent::read(&buf[..n]) else {
        fail(b"ui-testclient: WindowFocus body did not decode\n");
    };
    if f.focused == 0 {
        fail(b"ui-testclient: WindowFocus did not say the new window gained focus\n");
    }

    // 3. Geometry, **after a move off the default, on a window committed at a size other than
    // requested**: otherwise an event echoing the request looks like one reporting the screen.
    let (cw, ch) = (32u32, 16u32);
    let cpitch = cw as usize * 4;
    let Some((chandle, caddr)) = shared_buffer(cpitch * ch as usize) else {
        fail(b"ui-testclient: probe buffer alloc FAILED\n");
    };
    if w.attach(0, cw, ch, cpitch as u32, chandle).is_err() {
        fail(b"ui-testclient: probe attach FAILED\n");
    }
    if w.commit(0, (0, 0, cw, ch)).is_err() {
        fail(b"ui-testclient: probe commit FAILED\n");
    }
    // A commit that resizes is itself announced; take that off the wire so step 3 reads the *move*.
    await_mgr(
        mgr,
        OP_MGR_WINDOW_GEOMETRY,
        id,
        geometry_window,
        &mut buf,
        b"ui-testclient: no WindowGeometry for a commit that resized the window\n",
    );

    let (px, py) = (29i32, 41i32);
    if !place_window(mgr, id, px, py) {
        fail(b"ui-testclient: probe Place was refused\n");
    }
    let n = await_mgr(
        mgr,
        OP_MGR_WINDOW_GEOMETRY,
        id,
        geometry_window,
        &mut buf,
        b"ui-testclient: no WindowGeometry for the probe window\n",
    );
    let Some(g) = ConfigureEvent::read(&buf[..n]) else {
        fail(b"ui-testclient: WindowGeometry body did not decode\n");
    };
    if g.x != px || g.y != py {
        Line::new()
            .s(b"ui-testclient: WindowGeometry ")
            .u(g.window as u64)
            .s(b" at ")
            .i(g.x as i64)
            .s(b",")
            .i(g.y as i64)
            .end();
        fail(b"ui-testclient: WindowGeometry did not report the position placed\n");
    }
    if (g.width, g.height) != (cw, ch) {
        Line::new()
            .s(b"ui-testclient: WindowGeometry size ")
            .u(g.width as u64)
            .s(b"x")
            .u(g.height as u64)
            .s(b", committed ")
            .u(cw as u64)
            .s(b"x")
            .u(ch as u64)
            .s(b", requested ")
            .u(pw as u64)
            .s(b"x")
            .u(ph as u64)
            .end();
        fail(b"ui-testclient: WindowGeometry reported a size that is not what is on screen\n");
    }

    if w.destroy().is_err() {
        fail(b"ui-testclient: probe destroy FAILED\n");
    }
    await_mgr(
        mgr,
        OP_MGR_WINDOW_DESTROYED,
        id,
        destroyed_window,
        &mut buf,
        b"ui-testclient: no WindowDestroyed for the probe window\n",
    );
    // SAFETY: unmapping a range this process mapped in `shared_buffer`.
    unsafe { syscall2(SYS_MEMORY_UNMAP, caddr as u64, (cpitch * ch as usize) as u64) };

    kprint(b"ui-testclient: manager saw created, focus, geometry and destroyed\n");

    // **Handed back rather than dropped**: closing the session repaints the screen, which tore the
    // gate's capture.
    w.into_transport()
}

/// Report failure and end the run; init cannot wait on this program, which on success never exits.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // The verdict path init uses; unconditional because this binary is embedded only in test
    // images, and without the exit device the syscall is `Unsupported` and falls through to `exit`.
    // SAFETY: SYS_TEST_EXIT takes the verdict in a0; under the test-harness kernel it
    // terminates QEMU, so this does not return in practice.
    unsafe { libkern::syscall1(libkern::SYS_TEST_EXIT, libkern::TEST_EXIT_FAILURE as u64) };
    exit(1);
}

/// Entry point: connect, create a window, share buffers, present, then serve chords or park.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"ui-testclient: up\n");

    // 0. The reference pictures first, in decreasing size (320x160, 180x96, then the scene's
    // 64x32), as windows stack in creation order and a larger later one would hide a region the
    // gate compares. **Which font each gets is the point**: the host renders each with the file the
    // guest read, so the wrong one fails against real pixels.
    let ui_font = load_face(root_ns, Face::Ui);
    let mono_font = load_face(root_ns, Face::Mono);
    let _ui_window = present_reference_ui(root_ns, &ui_font);
    let _term_window = present_reference_term(root_ns, &mono_font);

    // 1. A session. The compositor mints a channel per resolve of `/dev/draw/new`.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => {
            fail(b"ui-testclient: connect to /dev/draw FAILED\n");
        }
    };

    let (w, h) = (scene::SCREEN_WIDTH, scene::SCREEN_HEIGHT);
    let mut win = match Win::open(transport, w, h, Role::Normal) {
        Ok(win) => win,
        Err(_) => {
            fail(b"ui-testclient: CreateWindow FAILED (no reply?)\n");
        }
    };
    Line::new().s(b"ui-testclient: window ").u(win.id() as u64).end();

    // 2b. Read the window back through the *numbered* path: resolve suffix, id parsing, object
    // hand-off.
    if !check_info(root_ns, win.id(), w, h) {
        fail(b"ui-testclient: /dev/draw/<N>/info FAILED\n");
    }

    // 2c. Churn before the presented window exists in final form, so no churn window is captured.
    if !churn(root_ns) {
        // Names **no** suspect: the compositor is not the only one; the `churn:` line says which
        // step failed.
        fail(b"ui-testclient: window churn FAILED (see the churn: line above)\n");
    }
    Line::new().s(b"ui-testclient: churned ").u(CHURN_CYCLES as u64).s(b" windows").end();

    // 3. Shared memory, rendered once: both buffers get the same picture.
    // **Attached at `SCREEN_PITCH` (268), not `w * 4` (256)**: the row padding catches code that
    // computes a row offset from the width, which a 256-byte buffer hides.
    let pitch = scene::SCREEN_PITCH;
    let len = pitch * h as usize;
    let rendered = scene::render_reference();
    let mut maps: [*mut u8; BUFFERS] = [core::ptr::null_mut(); BUFFERS];
    #[allow(clippy::needless_range_loop, reason = "the index also feeds arithmetic or a second array")]
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            fail(b"ui-testclient: buffer alloc FAILED\n");
        };
        for y in 0..h {
            for x in 0..w {
                let c = Framebuffer::get_pixel(&rendered, x, y).unwrap_or_default();
                let word = libdraw::format::PixelFormat::XRGB8888.encode(c);
                // SAFETY: `addr` maps `len` writable bytes; `y*pitch + x*4 + 4 <= len`.
                unsafe { (addr.add(y as usize * pitch + x as usize * 4) as *mut u32).write(word) };
            }
        }
        maps[i] = addr;
        if win.attach(i as u32, w, h, pitch as u32, handle).is_err() {
            fail(b"ui-testclient: AttachBuffer FAILED\n");
        }
    }
    let _ = maps;

    // 4. More frames than buffers: from frame 3 this proceeds only if the compositor released one.
    for frame in 0..FRAMES {
        // `acquire` blocks if the compositor holds every buffer; polling once stalled at frame 2.
        let b = match win.acquire() {
            Ok(b) => b,
            Err(_) => {
                Line::new().s(b"ui-testclient: STALLED at frame ").u(frame as u64).s(b" -- no buffer released").end();
                fail(b"");
            }
        };
        if win.commit(b, (0, 0, w, h)).is_err() {
            fail(b"ui-testclient: Commit FAILED\n");
        }
    }

    // **Confirm the last commit was processed**: one more `acquire` returns only after a `Release`,
    // so the gate cannot screendump an uncomposited frame — a receipt, not a sleep.
    if win.acquire().is_err() {
        fail(b"ui-testclient: final frame never acknowledged\n");
    }

    Line::new()
        .s(b"ui-testclient: committed ")
        .u(FRAMES as u64)
        .s(b" frames over ")
        .u(BUFFERS as u64)
        .s(b" buffers")
        .end();
    // **Place the reference windows explicitly through the manager channel**: they land at the
    // origin anyway, but the gate's nesting assertion is now a placement this client performs.
    // Best-effort. Outlives the block below.
    let mut probe_session: Option<alloc::boxed::Box<ChannelTransport>> = None;
    // **Outlives the block below too**: the client serves chords for the rest of its life.
    let mut manager: Option<ChannelTransport> = None;
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    if let Ok(mut mgr) = unsafe { ChannelTransport::manage(root_ns) } {
        // **Before the placements**: this probe creates and destroys a window, so it goes first and
        // leaves the placements last. **Bound outside this block**: dropping it closes the probe's
        // session and repaints the screen.
        probe_session = Some(verify_manager_events(&mut mgr, root_ns));
        verify_initial_configure(&mut mgr, root_ns);
        verify_popup_placement(&mut mgr, root_ns);

        // **Move one window off the default and read it back** before placing at the origin: a
        // `Place` that did nothing looks the same at `(0, 0)`. Restored below.
        verify_placement(&mut mgr, root_ns, win.id(), 11, 7);

        for id in [_ui_window.id(), _term_window.id(), win.id()] {
            verify_placement(&mut mgr, root_ns, id, 0, 0);
        }
        verify_desktop_requests(&mut mgr, root_ns, win.id());

        // **Then stack them, bottom to top, in the order the gate composes them**, last so nothing
        // reorders it. Creation order was a race with `nxterm` (it landed above the toolkit
        // reference in 4 of 6 KVM boots). Still assumed: `nxterm`'s window *exists* by now, else it
        // covers the region the gate compares.
        for id in [_ui_window.id(), _term_window.id(), win.id()] {
            if !raise_window(&mut mgr, id) {
                fail(b"ui-testclient: a manager Raise was refused\n");
            }
        }
        kprint(b"ui-testclient: reference windows placed and stacked via /dev/draw/manage\n");

        // **Exercise the one-manager rule**: a second resolve is refused, not allowed to depose the
        // first (two managers placing windows is a race). A *success* here is the failure.
        // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
        match unsafe { ChannelTransport::manage(root_ns) } {
            Ok(_) => fail(b"ui-testclient: a second /dev/draw/manage was SERVED\n"),
            Err(_) => kprint(b"ui-testclient: a second /dev/draw/manage was refused\n"),
        }
        manager = Some(mgr);
    } else {
        // A distinguishable line, not a `fail`: the gate asserts the success line above, and this
        // says which way it failed.
        kprint(b"ui-testclient: no manager channel; windows keep the default placement\n");
    }

    kprint(b"ui-testclient: scene presented via /dev/draw\n");
    kprint(b"ui-testclient: PASSED\n");

    // Named so it is plainly deliberate: the probe's session stays open for the process's life.
    let _held_open = probe_session;

    // **The client serves chords rather than parking**: a chord is how the host asks the guest to
    // change the screen.
    match manager.as_mut() {
        Some(mgr) => serve_hotkeys(mgr, notif),
        // No manager channel, nothing to register a chord on: park so the scene stays up.
        None => park(notif),
    }
}

/// Wait forever on a notification nothing signals, for the path with no manager channel.
fn park(notif: u64) -> ! {
    loop {
        let handles = [notif];
        let mut results = [0u8; 24];
        // SAFETY: waiting on this process's own notification handle, which nothing
        // signals; the deadline is infinite, so this parks rather than spins
        unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
    }
}

/// Register a chord and act on it until killed; **toggling, not one-way**, so a gate sees the scene
/// come back.
fn serve_hotkeys(mgr: &mut ChannelTransport, notif: u64) -> ! {
    use libkern::abi::{KEY_F1, KEY_SPACE};
    use librsproto::surface::{MOD_META, MgrHotkey, OP_MGR_HOTKEY, OP_MGR_REGISTER_HOTKEY};
    let _ = notif;

    // `Super+F1`: a second key keeps it out of the way of a shell's `Super` launcher.
    const HOTKEY_ID: u32 = 0x8001;
    let hk = MgrHotkey { id: HOTKEY_ID, mods: MOD_META, code: KEY_F1 };
    let mut body = [0u8; 8];
    if hk.write(&mut body).is_none() {
        fail(b"ui-testclient: could not encode a RegisterHotkey body\n");
    }
    let mut reply = [0u8; 8];
    if mgr.request(OP_MGR_REGISTER_HOTKEY, &body, None, &mut reply).is_err() {
        fail(b"ui-testclient: RegisterHotkey was refused\n");
    }
    // **A second registration under the same id must be refused**, not silently replace the first.
    let mut reply = [0u8; 8];
    if mgr.request(OP_MGR_REGISTER_HOTKEY, &body, None, &mut reply).is_ok() {
        fail(b"ui-testclient: a duplicate hotkey id was ACCEPTED\n");
    }
    // **A second chord whose action moves nothing**, for key repeat: `Super+F1` empties the
    // desktop, so `fire_repeat` cancels itself and the gate could not see a wrongly armed repeat.
    // This one only prints, so one reaches the focused window and is logged.
    const QUIET_ID: u32 = 0x8002;
    let quiet = MgrHotkey { id: QUIET_ID, mods: MOD_META, code: KEY_SPACE };
    let mut body = [0u8; 8];
    if quiet.write(&mut body).is_none() {
        fail(b"ui-testclient: could not encode the quiet chord\n");
    }
    let mut reply = [0u8; 8];
    if mgr.request(OP_MGR_REGISTER_HOTKEY, &body, None, &mut reply).is_err() {
        fail(b"ui-testclient: registering the quiet chord was refused\n");
    }
    kprint(b"ui-testclient: hotkey registered, waiting\n");

    let mut current: u32 = 1;
    let mut buf = [0u8; 64];
    loop {
        match mgr.wait_event_timeout(&mut buf, MGR_EVENT_SLICE_NS) {
            Ok(Some((op, n))) if op == OP_MGR_HOTKEY => {
                let Some(got) = MgrHotkey::read(&buf[..n]) else {
                    fail(b"ui-testclient: a Hotkey event did not decode\n");
                };
                if got.id == QUIET_ID {
                    // Nothing is changed on purpose — see where it is registered.
                    kprint(b"ui-testclient: quiet chord fired\n");
                    continue;
                }
                if got.id != HOTKEY_ID {
                    fail(b"ui-testclient: a Hotkey event named a chord we never registered\n");
                }
                // The event echoes the chord, so a compositor that matched the wrong one is caught
                // here.
                if got.mods != MOD_META || got.code != KEY_F1 {
                    fail(b"ui-testclient: a Hotkey event carried the wrong chord\n");
                }
                current = if current == 1 { 2 } else { 1 };
                if !set_current_desktop(mgr, current) {
                    fail(b"ui-testclient: SetCurrentDesktop was refused\n");
                }
                Line::new().s(b"ui-testclient: hotkey fired -> desktop ").u(current as u64).end();
            }
            Ok(_) => {}
            Err(_) => fail(b"ui-testclient: manager channel error while waiting for a chord\n"),
        }
    }
}

/// Switch the compositor's current desktop. `false` if the request did not succeed.
fn set_current_desktop(mgr: &mut ChannelTransport, desktop: u32) -> bool {
    use librsproto::surface::{MgrDesktop, OP_MGR_SET_CURRENT_DESKTOP};
    let mut body = [0u8; 4];
    if (MgrDesktop { desktop }).write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 8];
    mgr.request(OP_MGR_SET_CURRENT_DESKTOP, &body, None, &mut reply).is_ok()
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"ui-testclient: panic\n");
    exit(2);
}
