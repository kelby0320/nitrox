//! `ui-testclient` — the display arm's first real client (plan M2 Part D).
//!
//! Everything before this was one half of a conversation. `libsurface` had only met a mock and
//! the compositor had never answered anybody, which is how the Surface protocol shipped
//! **one-way** through a PR with three green CI jobs and 1213 passing tests: the code that
//! produced replies existed, and nothing consumed them.
//!
//! So this is the test that could not have passed before: a second process that connects
//! over `/dev/draw`, shares real memory, and requires the compositor to talk back.
//!
//! ## What it proves, and why each step is load-bearing
//!
//! 1. **A session exists.** Resolving `/dev/draw/new` returns a channel — the forwarded
//!    resolve, the compositor's session slot, and handle transfer of the endpoint.
//! 2. **The compositor replies.** `CreateWindow` must come back with a window id, or the
//!    client cannot proceed at all. This is the step that was missing.
//! 3. **Shared memory crosses once.** Two `MemoryObject`s are created, mapped, drawn into,
//!    and their handles transferred on `AttachBuffer` — not per frame.
//! 4. **`Release` comes back.** The client commits more frames than it has buffers, so
//!    from the third frame it can only proceed if the compositor released the buffer that
//!    left the screen. A one-way protocol stalls here, and that is the point.
//!
//! It leaves the reference scene on screen in its final buffer, so `check-display` compares
//! a picture that arrived through the whole protocol rather than one written straight to
//! the aperture.
//!
//! ## Why it parks instead of exiting
//!
//! It does not exit on success. A client that exits closes its channel, the compositor sees
//! `PeerClosed` and destroys its windows — correctly — and the screen goes back to
//! background. The first version exited straight after presenting and the display gate
//! captured an empty screen, which looked exactly like a compositing failure and was not
//! one.
//!
//! So failures are reported with `sys_test_exit` directly, and success **parks**: the
//! window stays on screen for as long as the machine runs, which is what every real client
//! does.
//!
//! ## The second window: the toolkit, and a font read from the disk
//!
//! Since M4 Part C it also presents [`libui::reference`] in a window of its own, which is the
//! only thing on the target that has ever loaded a font. Everything about the toolkit was
//! host-tested against a font compiled into the test binary; this reads
//! `/system/fonts/DejaVuSansMono.ttf` through `fs-server-ext4` and rasterises with it, and
//! `check-display` compares the result against the same render performed on the host.
//!
//! **A connection per window, though neither thing that forced it is true any more.** Input
//! records carry a window id (C3 part 1) and `Session` holds several windows on one connection
//! (C3 part 2), so this client *could* be one session now. It is not, because nothing here
//! needs it and rewriting a working gate to prove an API is how gates acquire bugs.
//!
//! `verify_popup_placement` is the exception, and drives its two windows through the raw
//! transport for a reason that **is** still live: it is the manager as well as the client, and
//! `Session::create` blocks for a window's first `Configure` — which, for a `normal` window
//! with a manager attached, only the manager can release. See `verify_initial_configure`.
//!
//! **The UI window is created first**, so the reference scene — created second — stacks above
//! it. That ordering is load-bearing for the gate: it compares the scene's 64×32 at the
//! top-left and the toolkit's picture everywhere *else* the UI window covers, so a compositor
//! that stacked them the other way fails the scene comparison rather than passing quietly.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::framebuffer::Framebuffer;
use libdraw::scene;
use libkern::debug::Line;
use libkern::{
    SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_WAIT, exit, kprint,
    syscall2, syscall4,
};
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, ConfigureEvent, CreateWindowRequest, FocusEvent,
    MgrWindowCreated, OP_CONFIGURE, OP_FOCUS_EVENT,
    MgrWindowRef, OP_ATTACH_BUFFER, OP_COMMIT, OP_MGR_WINDOW_CREATED, OP_MGR_WINDOW_DESTROYED,
    OP_MGR_WINDOW_FOCUS, OP_MGR_WINDOW_GEOMETRY, ROLE_NORMAL, Role,
    SURFACE_FORMAT_XRGB8888, build_attach_buffer_request, build_commit_request,
};
use libsurface::{Session, Transport, ipc::ChannelTransport};

/// `alloc` backing — rendering the reference scene allocates.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Frames to commit. **More than the buffer count on purpose**: with two buffers, frame 3
/// is only reachable if a `Release` arrived for frame 1's buffer.
const FRAMES: usize = 6;
/// Buffers the client allocates. Two is the minimum the protocol permits.
const BUFFERS: usize = 2;

/// Window-churn cycles, and the buffer size each one shares with the compositor.
///
/// Sized so the run **fails on a leak rather than merely leaking**, and sized against the
/// *sparsest* leak it has to catch. The cycle alternates two disposal paths, so a break in
/// either one leaks on only half the cycles: 128 cycles is 64 leaking ones at 3 MiB, which
/// is 192 MiB against a 256 MiB guest. At 80 cycles a break in the alternating half leaked
/// 120 MiB, the guest absorbed it, and the probe passed — vacuous coverage of exactly the
/// kind this milestone keeps producing. A smaller buffer or fewer cycles would leak just as
/// truly and pass just as green.
const CHURN_CYCLES: usize = 128;
/// Churn window width — the buffer is `CHURN_W * 4 * CHURN_H` = 3 MiB.
const CHURN_W: u32 = 1024;
/// Churn window height.
const CHURN_H: u32 = 768;

/// A session holding exactly one window — the shape most of this client uses.
///
/// `libsurface` became session-oriented in M6 C3, because a client may hold several windows on
/// one connection and a menu needs exactly that. Most of this file predates it and wants one
/// window per connection, so this keeps those call sites saying what they always said. The
/// multi-window API is used directly where it matters — see `verify_popup_placement`.
struct Win {
    session: Session<alloc::boxed::Box<ChannelTransport>>,
    id: u32,
}

impl Win {
    /// Connect, create one window, and wait out its initial configure.
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

    fn attach(
        &mut self,
        b: u32,
        width: u32,
        height: u32,
        pitch: u32,
        handle: u64,
    ) -> Result<(), libsurface::UiError> {
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


/// Create a `MemoryObject` of `len` bytes and map it read-write.
///
/// Returns `(handle, address)`. The handle is transferred to the compositor on attach; the
/// mapping stays, because the client keeps drawing into it.
fn shared_buffer(len: usize) -> Option<(u64, *mut u8)> {
    // SAFETY: a plain anonymous object of `len` bytes.
    let h = unsafe { syscall4(SYS_MEMORY_CREATE, len as u64, 0, 0, 0) };
    if h <= 0 {
        return None;
    }
    // SAFETY: mapping an object this process just created, read-write.
    let addr = unsafe {
        syscall4(
            SYS_MEMORY_MAP,
            h as u64,
            0,
            len as u64,
            libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE,
        )
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

/// Resolve `/dev/draw/<id>/info`, map it, and hand back the snapshot.
///
/// Split out of [`check_info`] so a placement can be *read back* rather than assumed: the
/// manager's `Place` reply says only that the compositor answered, and B1's gate line was
/// satisfied by that alone until 2026-08-19 (PR #216 review, blocking 1).
fn read_window_info(root_ns: u64, id: u32) -> Option<librsproto::surface::WindowInfo> {
    use libkern::handle::{RawHandle, Rights};
    use libos::{Handle, MapRead, Memory, Namespace, NsReadOnly, block_on};

    let mut path = [0u8; 32];
    let prefix = b"/dev/draw/";
    path[..prefix.len()].copy_from_slice(prefix);
    let mut n = prefix.len();
    // The id, in decimal.
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
    let ns =
        unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: the path resolves to a read-mappable object holding one `WindowInfo`.
    let obj = block_on(unsafe { ns.lookup::<Memory, MapRead>(path, Rights::MAP_READ) }).ok()?;
    // **The constant, not the literal it used to equal.** `WindowInfo` grew from 32 to 40 bytes
    // in M8 Part A; mapping 32 here would leave `read` refusing the short slice and this
    // function returning `None` for every window — a *silent* failure, because the image still
    // builds and the gate would report "no info" rather than a mismatch.
    const N: usize = librsproto::surface::WINDOW_INFO_LEN;
    let addr = obj.map(N).ok()?;
    // SAFETY: the compositor serves exactly `N` bytes of `WindowInfo` here.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, N) };
    let info = librsproto::surface::WindowInfo::read(bytes);
    // Unmap once the snapshot is read. `info` mints a fresh object per resolve, so a client
    // that polls it and never unmaps leaks a page each time — the same defect the compositor
    // had on its side of this exchange (PR #175 review, finding 1).
    let _ = obj.unmap(addr as *mut u8, N);
    info
}

/// Resolve `/dev/draw/<id>/info` and check it describes the window we created.
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

    info.id == id
        && info.width == want_w
        && info.height == want_h
        && info.role == librsproto::surface::ROLE_NORMAL
}

/// Open and close windows in a loop, proving the compositor gives the memory back.
///
/// This is the ordinary application lifecycle — a window closes, a client exits — and until
/// the review of PR #175 nothing exercised it: `ui-testclient` parked with its window open,
/// so no buffer mapping was ever torn down in the guest. The compositor dropped its
/// bookkeeping record on destroy and left the mapping behind, which pinned the *client's*
/// frames, because `map_attached_buffer` closes its handle and relies on the mapping to
/// hold the object alive.
///
/// Runs on its own connection so the presented window is untouched, and commits nothing —
/// a churn window never reaches the screen.
#[inline(never)]
fn churn(root_ns: u64) -> bool {
    // **Boxed.** A `ChannelTransport` is ~9 KiB against a 32 KiB stack, and moving one in
    // and out of a `Window` each cycle overflows it — which presents as a process that dies
    // in its prologue and never prints its own first line.
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
        // The allocation that fails first when the compositor is hoarding.
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
        // The client's own half. `attach` transferred the handle away, so this mapping is
        // all that is left holding the object on this side.
        // SAFETY: unmapping a range this process mapped in `shared_buffer`.
        unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, len as u64) };

        // **A rejected attach must also give the memory back.** The handle rides the
        // message whether or not the compositor accepts it, so an attach naming a window
        // that does not exist — one malformed message, which any client can send — used to
        // hand over an object nobody ever closed. Sent through the raw transport because
        // `libsurface` deliberately cannot express it.
        let Some((bogus_handle, bogus_addr)) = shared_buffer(len) else {
            Line::new().s(b"churn: bogus buffer alloc failed at cycle ").u(cycle as u64).end();
            return false;
        };
        //
        // Alternates between two *different* disposal paths, because they are distinguished
        // by different code. An attach that is rejected exercises the outcome branch; a
        // handle riding `Commit` exercises the op branch — and only the second one fails if
        // the close is ever narrowed back to `if op == OP_ATTACH_BUFFER` with an else.
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
        // **Waits for the refusal rather than firing and forgetting.** Two reasons: it
        // paces the loop against the compositor, and an unconsumed error reply is a message
        // that parks — eight of those overflow the queue and the *next* request fails for
        // an unrelated-looking reason. `Err(Server)` is the expected answer here; anything
        // else means the compositor accepted an attach to a window that does not exist.
        let mut reply = [0u8; 32];
        match t.request(bogus_op, &req[..rn], Some(bogus_handle), &mut reply) {
            Err(libsurface::UiError::Server) => {}
            _ => {
                Line::new()
                    .s(b"churn: bogus request was not refused at cycle ")
                    .u(cycle as u64)
                    .end();
                return false;
            }
        }
        // SAFETY: unmapping a range this process mapped in `shared_buffer`.
        unsafe { syscall2(SYS_MEMORY_UNMAP, bogus_addr as u64, len as u64) };
    }
    true
}

/// Present [`libui::reference`] in a window of its own, drawn with the font from the disk.
///
/// Returns the window **and the font**: the window must be kept alive — dropping it closes the
/// channel, the compositor destroys the window, and the picture leaves the screen — and the
/// font is handed on rather than loaded twice, so the two reference windows are provably drawn
/// with the same bytes off the same disk.
///
/// Every failure here is fatal to the run. A font that does not load is exactly the
/// regression this exists to catch — the file is staged into the ext4 root by the image
/// build, so "it did not resolve" means the staging broke, and carrying on would leave a
/// green boot with no text on screen.
fn present_reference_ui(
    root_ns: u64,
) -> (Win, libdraw::text::Font) {
    use libdraw::text::{LoadError, SYSTEM_FONT_PATH, load};

    // The font. Resolved through the namespace and demand-paged out of ext4 — the first time
    // anything on the target has read one.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let font = match unsafe { load(root_ns, SYSTEM_FONT_PATH) } {
        Ok(f) => f,
        Err(e) => {
            let why: &[u8] = match e {
                LoadError::NoBinding => b"did not resolve (is it staged into the rootfs?)",
                LoadError::Unstattable => b"stat failed",
                LoadError::ImpossibleSize(_) => b"empty, or larger than the cap",
                LoadError::Unmappable => b"could not be mapped",
                LoadError::NotAFont => b"is not a parseable font",
            };
            Line::new().s(b"ui-testclient: ").s(SYSTEM_FONT_PATH.as_bytes()).s(b" ").s(why).end();
            fail(b"ui-testclient: font load FAILED\n");
        }
    };
    kprint(b"ui-testclient: font loaded from /system/fonts\n");

    let (w, h) = (libui::reference::WIDTH, libui::reference::HEIGHT);
    let pitch = libui::reference::PITCH;
    let len = pitch * h as usize;
    // Rendered once. `into_bytes` hands back the buffer at exactly this pitch, so the copy
    // below is a straight memcpy rather than the row-by-row translation the scene needs.
    let picture = libui::reference::render(&font).into_bytes();
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
    // Commit both, then block for a release. One commit and one `acquire` would return the
    // *other* free buffer immediately and prove nothing: the gate screendumps later, on a
    // different channel, so without a receipt this window could still be uncomposited then.
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
    (win, font)
}

/// Present [`libterm::render::reference`] in a window of its own, drawn with `font`.
///
/// The gate's third region, and the only place a terminal render is compared against pixels
/// that actually reached a screen. Same shape as [`present_reference_ui`] and same contract:
/// the caller keeps the window, and every failure here is fatal.
///
/// **The reference rather than `nxterm`'s window**, which is also on screen. A live terminal's
/// first frame is deterministic but shows a boot banner — one plain line — whereas this stream
/// is built so each of its lines fails differently. A gate should compare the picture that
/// discriminates.
fn present_reference_term(
    root_ns: u64,
    font: &libdraw::text::Font,
) -> Win {
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
    // Both buffers, then block for a release — see `present_reference_ui` for why one commit
    // proves nothing.
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

/// Ask the manager channel to put `window` at `(x, y)`. `false` if the request did not succeed.
///
/// **The reply is no longer discarded, and the old reason it could be was wrong.** This said the
/// display gate checked the result, because "a placement that silently failed shows up there as
/// a window in the wrong place" — which it cannot, since every placement here is to `(0, 0)` and
/// that is already the compositor's default. A refused `Place` moved nothing and looked
/// identical. Demonstrated in review: with every `OP_MGR_PLACE` made to fail, `check-display`
/// stayed green (PR #216 review, blocking 1).
///
/// A successful reply still only says the compositor answered `Ok`, so the caller reads the
/// origin back through `/dev/draw/<id>/info` — see `verify_placement`.
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

/// Drive the three desktop requests over the real manager channel and read each back.
///
/// **What this covers that a host unit test cannot.** `compose_into`, `hit` and
/// `focus_candidate` are exercised directly by `compositor`'s own tests, and those are the
/// functions the binary calls — so the filtering itself is pinned there. What no host test can
/// reach is the **wire**: that a real client's request, encoded into a body and sent down
/// `/dev/draw/manage`, is routed to the right arm of `dispatch` and answered. That is the gap
/// PR #233 fell into, where a title cap was specified, tested in isolation, and unreachable on
/// the path a client actually uses.
///
/// **The window ends exactly where it started**, so this changes no pixel the display gate
/// compares — the round trip is the assertion, not a state left behind.
fn verify_desktop_requests(mgr: &mut ChannelTransport, root_ns: u64, window: u32) {
    use librsproto::surface::{
        MgrDesktop, MgrWindowValue, OP_MGR_SET_CURRENT_DESKTOP, OP_MGR_SET_MINIMIZED,
        OP_MGR_SET_WINDOW_DESKTOP, STICKY_DESKTOP, WINDOW_FLAG_MINIMIZED,
    };

    /// Send one `window`+`value` request; `false` if the compositor did not answer `Ok`.
    fn send_value(mgr: &mut ChannelTransport, op: u16, window: u32, value: u32) -> bool {
        let mut body = [0u8; 8];
        if (MgrWindowValue { window, value }).write(&mut body).is_none() {
            return false;
        }
        let mut reply = [0u8; 8];
        mgr.request(op, &body, None, &mut reply).is_ok()
    }

    // Desktop, there and back. Read back through `info` rather than trusting the reply: an
    // `Ok` says the compositor answered, which is the distinction PR #216 turned into a rule.
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

    // Minimized, there and back — the second attribute, and a separate bit in `info` so that a
    // compositor folding the two into one would fail here rather than look equivalent.
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

    // The current desktop, and the one value it refuses. `STICKY_DESKTOP` as a *current*
    // desktop would composite only sticky windows and make everything created afterwards
    // sticky, so the compositor rejects it — and this is the only place that rejection is
    // reached the way a real caller reaches it.
    let mut body = [0u8; 4];
    if (MgrDesktop { desktop: STICKY_DESKTOP }).write(&mut body).is_none() {
        fail(b"ui-testclient: could not encode a SetCurrentDesktop body\n");
    }
    let mut reply = [0u8; 8];
    if mgr.request(OP_MGR_SET_CURRENT_DESKTOP, &body, None, &mut reply).is_ok() {
        fail(b"ui-testclient: the compositor ACCEPTED a current desktop of 0\n");
    }
    // And a legal switch is accepted — then straight back, so the scene the display gate
    // compares is the one this client drew.
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
///
/// This is what makes the gate's placement line mean something. The reply to `Place` says the
/// compositor answered; only the read-back says the window moved.
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

/// How long one `wait_event_timeout` slice waits for a manager event, and how many slices.
///
/// **Up to ~2s, not a 2s budget.** An event of the wrong kind consumes a slice without
/// waiting, so the wall-clock floor is only reached when nothing arrives at all. That is the
/// case this bound exists for; with the handful of strays this probe can see, the count is
/// ample either way. Generous next to a compositor that flushes its manager outbox every loop
/// iteration, and short enough that a missing event is reported *by name* here rather than by
/// the gate's wall-clock timeout, which cannot say which event never came.
const MGR_EVENT_SLICE_NS: u64 = 100_000_000;
const MGR_EVENT_TRIES: u32 = 20;

/// How long a popup's first `Configure` may take before it is treated as having been held.
///
/// Bimodal, not a tolerance: a popup that is exempt from the initial-configure hold is answered
/// in the same handler that created it, and one that is held waits out `CONFIGURE_DEADLINE_NS`
/// — 200 ms. Anything in between does not happen, so 50 ms sits far from both.
///
/// **Slices that expired**, not iterations: a record that was already queued returns at once and
/// must not spend the budget, or the bound is "five other events" rather than 50 ms.
const POPUP_CONFIGURE_SLICE_NS: u64 = 10_000_000;
const POPUP_CONFIGURE_SLICES: u32 = 5;

/// Wait for an event with op `want` that names window `id`. Returns its body length.
///
/// **Filtered by window, not just by op**, because several windows are alive here and one
/// change produces records about more than one of them: focus moving to a new window emits
/// the *loss* for the old one first, so a wait that took the first `WindowFocus` off the
/// wire would read the wrong window's event and call the feature broken. `window_of` is
/// per-op because each body puts the id in a different place.
///
/// Events of other kinds are discarded: the four are queued by different paths in the
/// compositor and this checks that each *arrives*, not how they interleave.
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
///
/// `poll_event`, not a zero timeout: this wants what is already here and nothing more.
fn drain_mgr(mgr: &mut ChannelTransport) {
    let mut buf = [0u8; 64];
    while let Ok(Some(_)) = mgr.poll_event(&mut buf) {}
}

/// **A window is not shown until it has been configured, and the manager configures it** — M6 B4.
///
/// The interleaving is the whole test, and it is why this cannot use `Window::new`: that call
/// creates the window *and* blocks for the configure, so a single-threaded client that is also
/// the manager would be waiting for an answer only it can give. (B3's probe does exactly that
/// and is released by the deadline instead — which is a fair test of the deadline and no test
/// at all of the manager path.) Splitting the two with the raw transport is also the pattern a
/// real manager-and-client process has to follow, so it is worth showing once.
///
/// Three things are asserted, in order:
///   1. the `CreateWindow` **reply** arrives at once — only the configure is held;
///   2. no configure arrives while the manager has not answered;
///   3. the configure that does arrive carries the origin **the manager placed**, not the
///      default — which is the launch-without-a-jump this milestone item exists for.
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
    if librsproto::surface::build_create_window_request(
        &mut req,
        &CreateWindowRequest::new(bw, bh, Role::Normal),
    )
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

    // 2. **Nothing yet.** The reply came back, so the compositor has handled the request in
    //    full; a configure it did not hold would already be queued. Polled rather than waited,
    //    because the assertion is an absence and waiting for one proves nothing.
    //
    //    **Every queued event, not the first.** A single poll only fails if a configure happens
    //    to be at the head of the ring, and it will not be: creating a window puts it on top of
    //    the stack, so a `FocusEvent` is queued for it first. A release that moved slightly
    //    earlier — onto the created-event, say, rather than onto the manager acting — would
    //    then sit second and the check would report success (PR #218 review, finding 4).
    //
    //    **A `FocusEvent` for this window is a failure here too**, and that assertion is not
    //    decoration: a held window is not on screen, so it must not be the focus candidate, and
    //    nothing about it can be announced before the configure that makes it one. Draining is
    //    what makes the configure check thorough and is also what would hide this one — the
    //    stray record would simply be discarded — so the drain has to judge what it discards.
    let mut buf = [0u8; 64];
    while let Ok(Some((op, n))) = t.poll_event(&mut buf) {
        if op == OP_CONFIGURE {
            fail(b"ui-testclient: a configure arrived before the manager had answered\n");
        }
        if op == OP_FOCUS_EVENT
            && FocusEvent::read(&buf[..n]).is_some_and(|f| f.window == id && f.focused != 0)
        {
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

    // 4. And the held configure arrives, carrying where the manager put it — **as the very
    //    next record on this channel**, which is what the spec promises about `CreateWindow`
    //    producing the reply and then a `Configure`. Asserted strictly rather than searched
    //    for, because the ordering is the interesting part: a held window is not a focus
    //    candidate (it is not on screen), so nothing about it can be announced before the
    //    configure that makes it one. Searching past other records would pass on a compositor
    //    that announced focus for an invisible window first.
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
            .s(b"ui-testclient: configure for ").u(cfg.window as u64)
            .s(b" at ").i(cfg.x as i64).s(b",").i(cfg.y as i64)
            .s(b", placed ").i(px as i64).s(b",").i(py as i64).end();
        fail(b"ui-testclient: the first configure did not carry the manager's placement\n");
    }

    kprint(b"ui-testclient: the first configure carried the manager's placement\n");

    // **Closed, not leaked.** These probes used to `core::mem::forget` their transport, because
    // closing a session makes the compositor tear it down and repaint the whole screen, which
    // raced `check-display`'s capture. That race is gone — the gate now captures only once two
    // consecutive screendumps match (B4) — and leaking was never free: an unread session's
    // outbox never empties, so the compositor stays `parked` and wakes every
    // `RETRY_INTERVAL_NS` to retry sends that can never land, for the rest of the boot. That is
    // steady pressure on exactly the path `input-testclient`'s stall phase measures.
    drop(t);
}

/// **A popup is placed by its creator, relative to its parent** — M6 C1, on the wire.
///
/// Two things only a booted guest can show. That the offset is resolved against the parent's
/// **current** origin, so a popup created after the manager moved its parent lands beside the
/// parent rather than beside where it started. And that a popup is **not** held for the
/// manager: its position is its creator's business, so there is nobody to wait for, and holding
/// it would spend the initial-configure deadline on every menu open.
///
/// **Raw transport, and not because `libsurface` cannot do this any more.** `Session` holds
/// both windows on one connection since C3 part 2, which is what a popup needs — a popup may
/// only name a parent its own connection owns. The live reason is different: this client is
/// *also* the manager, and `Session::create` blocks for a window's first `Configure`, which for
/// a `normal` window with a manager attached only the manager can release. Splitting create
/// from wait is the pattern such a process has to follow either way.
fn verify_popup_placement(mgr: &mut ChannelTransport, root_ns: u64) {
    drain_mgr(mgr);
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let mut t = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => fail(b"ui-testclient: C1 connect to /dev/draw FAILED\n"),
    };

    // 1. A parent, moved somewhere the default placement would never put it — so an offset
    //    resolved against `(0, 0)` instead of against the parent is a different answer.
    let parent = raw_create(&mut t, &CreateWindowRequest::new(120, 90, Role::Normal), b"C1 parent");
    let (px, py) = (211i32, 143i32);
    if !place_window(mgr, parent, px, py) {
        fail(b"ui-testclient: C1 parent Place was refused\n");
    }

    // 2. The popup. Negative in x, so it hangs off the parent's left edge: a menu that could not
    //    do that would not be a menu. That it is *drawn* there is C2's host test, which reads
    //    pixels; this checks the geometry the compositor recorded.
    let (ox, oy) = (-14i32, 90i32);
    let popup = raw_create(
        &mut t,
        &CreateWindowRequest::at(40, 30, Role::Popup { parent }, ox, oy),
        b"C1 popup",
    );

    // 3. **Not held.** Nothing here answered as the manager, so a popup that was held would
    //    still be waiting. Bounded by *elapsed time*, not by iterations — see
    //    `POPUP_CONFIGURE_SLICES`.
    //
    //    **Only an empty slice counts.** `wait_event_timeout` returns a queued record without
    //    waiting, so counting every iteration would spend the budget on other people's events
    //    rather than on time: two of the five were already gone before this ever ran, on the
    //    parent's own configure and the focus change that followed it. One more record ahead of
    //    the popup — a pointer crossing the parent, a third window in this probe — and the gate
    //    would report "a popup waited for the manager" about a compositor that is perfectly
    //    fine (PR #219 review, finding 1).
    let mut buf = [0u8; 64];
    let mut empty_slices = 0;
    loop {
        match t.wait_event_timeout(&mut buf, POPUP_CONFIGURE_SLICE_NS) {
            Ok(Some((OP_CONFIGURE, n))) => {
                match ConfigureEvent::read(&buf[..n]) {
                    Some(c) if c.window == popup => break,
                    _ => {}
                }
            }
            // Somebody else's record. It cost no time, so it costs no budget.
            Ok(Some(_)) => {}
            Ok(None) => {
                empty_slices += 1;
                if empty_slices >= POPUP_CONFIGURE_SLICES {
                    fail(
                        b"ui-testclient: a popup waited for the manager, which never places popups\n",
                    );
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
            .s(b"ui-testclient: popup at ").i(info.x as i64).s(b",").i(info.y as i64)
            .s(b", wanted ").i((px + ox) as i64).s(b",").i((py + oy) as i64).end();
        fail(b"ui-testclient: the popup was not placed at its offset from the parent\n");
    }

    kprint(b"ui-testclient: a popup was placed by its creator, without the manager\n");

    // 5. **And a `dialog` is the other way round on both counts.** It names a parent, but the
    //    parent carries its desktop membership and its lifetime — not its position — so a
    //    manager places it and it is held like a `normal`. The offset below is deliberately
    //    large and deliberately ignored.
    //
    //    Neither half of that is reachable from a host test: `placed_by_creator` lives in the
    //    `#![no_main]` bin, which `cargo test -p compositor --lib` does not build. Without this
    //    the two roles could be merged back into one arm and every gate would stay green
    //    (PR #220 review, finding 2).
    let dialog = raw_create(
        &mut t,
        &CreateWindowRequest::at(24, 18, Role::Dialog { parent }, 500, 400),
        b"C1 dialog",
    );

    // Held: nothing answers as the manager yet, so no configure may arrive for it.
    let mut slices = 0;
    while slices < POPUP_CONFIGURE_SLICES {
        match t.wait_event_timeout(&mut buf, POPUP_CONFIGURE_SLICE_NS) {
            Ok(Some((OP_CONFIGURE, n))) => {
                if ConfigureEvent::read(&buf[..n]).is_some_and(|c| c.window == dialog) {
                    fail(b"ui-testclient: a dialog was configured with no manager answer\n");
                }
            }
            // Somebody else's record; it cost no time, so it costs no budget.
            Ok(Some(_)) => {}
            Ok(None) => slices += 1,
            Err(_) => fail(b"ui-testclient: C1 dialog channel error\n"),
        }
    }

    // The manager places it, which releases the hold — and what the client is told is the
    // manager's placement, not the offset it asked for.
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
            .s(b"ui-testclient: dialog at ").i(got.x as i64).s(b",").i(got.y as i64)
            .s(b", manager placed ").i(dx as i64).s(b",").i(dy as i64).end();
        fail(b"ui-testclient: a dialog took its creator's offset instead of the placement\n");
    }

    kprint(b"ui-testclient: a dialog was held for the manager and placed by it\n");

    // Destroying the parent takes the popup with it — transitively, which is the stack's rule.
    let mut body = [0u8; 8];
    body[..4].copy_from_slice(&parent.to_le_bytes());
    // **An empty reply buffer is how the transport is told an op is silent** — `DestroyWindow`
    // answers only on failure. Passing a real buffer parks this thread waiting for a reply that
    // is never coming, which is a hang rather than an error.
    let _ = t.request(librsproto::surface::OP_DESTROY_WINDOW, &body[..4], None, &mut []);
    // Closed rather than leaked, for the reasons given in `verify_initial_configure`.
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

/// **One window's whole life, watched from the manager channel** — M6 B3.
///
/// The manager is told about windows it did not create, which is the half of the seam B1
/// left unproven: B1 showed a manager can *act* (`Place`), not that it is *told* anything.
/// Each of the four events is queued by a different path in the compositor, so each is
/// checked separately and named separately when it does not come.
///
/// The *transitive* removed set — a popup going with its parent — is covered by the host
/// test `take_removed_reports_the_whole_subtree_parent_first_then_drains`. What only a
/// booted guest can show is that these records are framed, queued and delivered at all.
fn verify_manager_events(
    mgr: &mut ChannelTransport,
    root_ns: u64,
) -> alloc::boxed::Box<ChannelTransport> {
    // Everything the reference placements above queued is not this probe's business.
    drain_mgr(mgr);

    // A window created *after* the manager attached, so it is announced. Small and never
    // committed, so it paints nothing the display gate could capture.
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

    // 2. Focus — a new window takes the keyboard, and the manager is told.
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

    // 3. Geometry — **after a move to somewhere that is not the default, on a window whose
    //    committed size is not the size it asked for.** `(0, 0)` is where the compositor
    //    already puts windows, so a geometry event reporting the origin cannot be told from one
    //    that echoed the request without the window having moved. And the two sizes must
    //    differ, or an event reporting the *requested* size looks identical to one reporting
    //    what is actually on screen — which is exactly how that defect survived this gate the
    //    first time (PR #217 review, finding 1).
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
    // A commit that resizes is itself a geometry change, and it is announced. Take that one
    // off the wire before placing, so step 3 below reads the event for the *move*.
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
            .s(b"ui-testclient: WindowGeometry ").u(g.window as u64)
            .s(b" at ").i(g.x as i64).s(b",").i(g.y as i64).end();
        fail(b"ui-testclient: WindowGeometry did not report the position placed\n");
    }
    if (g.width, g.height) != (cw, ch) {
        Line::new()
            .s(b"ui-testclient: WindowGeometry size ").u(g.width as u64).s(b"x").u(g.height as u64)
            .s(b", committed ").u(cw as u64).s(b"x").u(ch as u64)
            .s(b", requested ").u(pw as u64).s(b"x").u(ph as u64).end();
        fail(b"ui-testclient: WindowGeometry reported a size that is not what is on screen\n");
    }

    // 4. Destroyed.
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
    // The client's own half of the buffer; `attach` transferred the handle away.
    // SAFETY: unmapping a range this process mapped in `shared_buffer`.
    unsafe { syscall2(SYS_MEMORY_UNMAP, caddr as u64, (cpitch * ch as usize) as u64) };

    kprint(b"ui-testclient: manager saw created, focus, geometry and destroyed\n");

    // **Handed back rather than dropped.** Closing this session would make the compositor
    // tear it down and *full-screen repaint* — the path that redraws the cursor after a
    // client dies under it (PR #185 review, finding 1). Recompositing 1280x800 takes long
    // enough that the display gate, which captures as soon as this client says the scene is
    // up, caught a torn frame about half the time: reference windows half-drawn, no cursor.
    // The window is destroyed either way, which is what this probe is checking; keeping the
    // channel open just means the probe's *cleanup* is not the last thing to touch the screen.
    w.into_transport()
}


/// Report failure and end the run.
///
/// Called instead of exiting with a code, because init cannot wait for this program: on
/// success it never exits.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // The same verdict path init uses. Unconditional rather than `cfg`-gated: this binary
    // is embedded only in selftest/test-harness images, and on a kernel without the exit
    // device the syscall is `Unsupported` and falls through to `exit` below.
    //
    // SAFETY: SYS_TEST_EXIT takes the verdict in a0; under the test-harness kernel it
    // terminates QEMU, so this does not return in practice.
    unsafe { libkern::syscall1(libkern::SYS_TEST_EXIT, libkern::TEST_EXIT_FAILURE as u64) };
    exit(1);
}

/// Entry point: connect, create a window, share buffers, present, then park.
///
/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments — `notif` is this
/// process's notification handle and `root_ns` its root namespace.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"ui-testclient: up\n");

    // 0. The reference pictures, each in its own window, **before** the scene's and in
    //    decreasing size — windows stack in creation order, so each must be smaller than the one
    //    below or it would hide it entirely and the gate would compare a region that is not on
    //    screen. Held for the process's life; dropping either closes its channel and takes the
    //    window off the screen.
    //
    //    The toolkit's is 320x160, the terminal's 180x96 and the scene's 64x32.
    let (_ui_window, font) = present_reference_ui(root_ns);
    let _term_window = present_reference_term(root_ns, &font);

    // 1. A session. The compositor mints a channel per resolve of `/dev/draw/new`.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        // Boxed for the same reason `churn` boxes its own: ~9 KiB of message buffers has no
        // business sitting in a 32 KiB stack frame alongside everything else here.
        Ok(t) => alloc::boxed::Box::new(t),
        Err(_) => {
            fail(b"ui-testclient: connect to /dev/draw FAILED\n");
        }
    };

    // 2. A window. This blocks on the compositor's reply — the step that did not exist.
    let (w, h) = (scene::SCREEN_WIDTH, scene::SCREEN_HEIGHT);
    let mut win = match Win::open(transport, w, h, Role::Normal) {
        Ok(win) => win,
        Err(_) => {
            fail(b"ui-testclient: CreateWindow FAILED (no reply?)\n");
        }
    };
    Line::new().s(b"ui-testclient: window ").u(win.id() as u64).end();

    // 2b. Read the window's own metadata back through the *numbered* path. This is the
    //     other half of Part B: `/dev/draw/new` mints a session, `/dev/draw/<N>/info`
    //     answers with a mapped snapshot. Checking it here rather than in a host test is
    //     what proves the resolve suffix, the id parsing and the object hand-off agree.
    if !check_info(root_ns, win.id(), w, h) {
        fail(b"ui-testclient: /dev/draw/<N>/info FAILED\n");
    }

    // 2c. Open and close windows until the guest would run out of memory if the compositor
    //     kept a single one of their buffers mapped. Before the presented window exists in
    //     its final form, so a churn window can never be what `check-display` captures.
    if !churn(root_ns) {
        // Deliberately names **no** suspect. This line used to blame the compositor for
        // leaking, and during review it misattributed two client-side failures in a row —
        // the parked-queue overflow and a missing `RS_FLAG_ERROR` check. This PR's own
        // lesson applies to its own diagnostics: when the picture is wrong, the compositor
        // is not the only suspect. The `churn:` line above says which step failed.
        fail(b"ui-testclient: window churn FAILED (see the churn: line above)\n");
    }
    Line::new().s(b"ui-testclient: churned ").u(CHURN_CYCLES as u64).s(b" windows").end();

    // 3. Shared memory. Rendered once; both buffers get the same picture, so whichever is
    //    on screen at the end is the one `check-display` expects.
    // **Attached at `SCREEN_PITCH` (268), not `w * 4` (256).** The extra 12 bytes are three
    // pixels of row padding that exist for exactly one reason: any code computing a row
    // offset from the width rather than the pitch skews every row after the first, and a
    // 256-byte buffer hides that bug because the two numbers agree. `display-selftest` used
    // to carry this coverage in the guest; when the client took over producing the picture
    // it briefly did not (PR #175 review, finding 7).
    let pitch = scene::SCREEN_PITCH;
    let len = pitch * h as usize;
    let rendered = scene::render_reference();
    let mut maps: [*mut u8; BUFFERS] = [core::ptr::null_mut(); BUFFERS];
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            fail(b"ui-testclient: buffer alloc FAILED\n");
        };
        // Copy the scene in, row by row: the client's pitch and the render's may differ.
        for y in 0..h {
            for x in 0..w {
                let c = Framebuffer::get_pixel(&rendered, x, y).unwrap_or_default();
                let word = libdraw::format::PixelFormat::XRGB8888.encode(c);
                // SAFETY: `addr` maps `len` writable bytes; `y*pitch + x*4 + 4 <= len`.
                unsafe {
                    (addr.add(y as usize * pitch + x as usize * 4) as *mut u32).write(word)
                };
            }
        }
        maps[i] = addr;
        if win.attach(i as u32, w, h, pitch as u32, handle).is_err() {
            fail(b"ui-testclient: AttachBuffer FAILED\n");
        }
    }
    let _ = maps;

    // 4. More frames than buffers. From frame 3 this only proceeds if the compositor
    //    released the buffer that left the screen — a one-way protocol stalls here.
    for frame in 0..FRAMES {
        // `acquire` blocks if the compositor holds every buffer. Polling once and giving up
        // is how this stalled at frame 2 the first time: a release that has not arrived
        // *yet* is not one that will never arrive.
        let b = match win.acquire() {
            Ok(b) => b,
            Err(_) => {
                // Built here rather than split across `fail`, which prints its argument as
                // given: the frame number and the reason are one line.
                Line::new()
                    .s(b"ui-testclient: STALLED at frame ")
                    .u(frame as u64)
                    .s(b" -- no buffer released")
                    .end();
                fail(b"");
            }
        };
        if win.commit(b, (0, 0, w, h)).is_err() {
            fail(b"ui-testclient: Commit FAILED\n");
        }
    }

    // **Confirm the last commit was actually processed.** Printing straight after the
    // final `commit` would race the compositor: the send returns as soon as the message is
    // queued, so the gate could screendump a frame that has not been composited yet. One
    // more `acquire` blocks until a `Release` arrives, and the compositor only sends that
    // after handling the commit that displaced the buffer — so it is a receipt, not a
    // sleep.
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
    // **Place the reference windows explicitly, through the manager channel.**
    //
    // They land at the origin anyway — that is the compositor's default and M6 kept it — so this
    // changes no pixel. What it changes is what the display gate *means*: its nesting assertion
    // was an accident it relied on (windows cannot move, so they must overlap at the origin) and
    // is now a placement this client performs. A gate that asserts a behaviour beats one that
    // asserts the absence of a feature, and the seam gets its first consumer a milestone before
    // the shell exists.
    //
    // Best-effort: a compositor with a manager already attached refuses, and this client is a
    // test fixture rather than the manager of a real session.
    //
    // Outlives the block below deliberately; see where it is assigned.
    let mut probe_session: Option<alloc::boxed::Box<ChannelTransport>> = None;
    // **The manager channel outlives the block below too**, since M8 Part B: the client keeps
    // serving registered chords for the rest of its life, which is what lets a gate ask the
    // guest to change what is on screen. Dropping it at the end of the block would close the
    // channel and take the chord table with it.
    let mut manager: Option<ChannelTransport> = None;
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    if let Ok(mut mgr) = unsafe { ChannelTransport::manage(root_ns) } {
        // **Before the placements below, not after.** This probe creates and destroys a
        // window, so it is the noisiest thing this client does to the screen; running it
        // first leaves the reference placements as the last screen-affecting work, which is
        // the sequence the display gate was stable under before this probe existed.
        //
        // **Bound outside this block**, so it lives as long as the process rather than as
        // long as the manager channel. Dropping it here would close the probe's session, and
        // the compositor answers a closed session with a full-screen repaint — the thing this
        // whole arrangement exists to keep away from the moment a gate starts looking at, or
        // clicking on, the screen.
        probe_session = Some(verify_manager_events(&mut mgr, root_ns));
        verify_initial_configure(&mut mgr, root_ns);
        verify_popup_placement(&mut mgr, root_ns);

        // **Move one window somewhere that is not the default, and read it back**, before
        // putting everything at the origin. Placing only at `(0, 0)` proves nothing: that is
        // where the compositor already puts windows, so a `Place` that did nothing is
        // indistinguishable from one that worked. The offset is restored below, so the scene
        // the display gate compares is unchanged.
        verify_placement(&mut mgr, root_ns, win.id(), 11, 7);

        for id in [_ui_window.id(), _term_window.id(), win.id()] {
            verify_placement(&mut mgr, root_ns, id, 0, 0);
        }
        // **After the placements, before the scene is declared presented.** It restores every
        // attribute it touches, so it leaves the screen exactly as the placements left it.
        verify_desktop_requests(&mut mgr, root_ns, win.id());
        kprint(b"ui-testclient: reference windows placed via /dev/draw/manage\n");

        // **Exercise the one-manager rule, which is the whole of B1's contract.** The
        // compositor refuses a second resolve rather than deposing the first, because two
        // managers placing windows is a race with no arbiter and the failure would look like
        // windows moving on their own. That refusal was implemented and never executed: with
        // one client asking once, the branch was unreachable, so the rule was pinned by
        // nothing. Asking twice from the client that already holds the channel is the
        // cheapest honest exercise of it.
        //
        // A *success* here is the failure: it means the channel was handed out twice, and the
        // second holder would silently depose the first.
        // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
        match unsafe { ChannelTransport::manage(root_ns) } {
            Ok(_) => fail(b"ui-testclient: a second /dev/draw/manage was SERVED\n"),
            Err(_) => kprint(b"ui-testclient: a second /dev/draw/manage was refused\n"),
        }
        manager = Some(mgr);
    } else {
        // Kept as a distinguishable line rather than a `fail`: the gate asserts the success
        // line above, so this path already fails the run — and it says *which* way it failed,
        // which a panic here would not.
        kprint(b"ui-testclient: no manager channel; windows keep the default placement\n");
    }

    kprint(b"ui-testclient: scene presented via /dev/draw\n");
    kprint(b"ui-testclient: PASSED\n");

    // Named so it is obvious this is deliberate rather than a forgotten binding: the probe's
    // session stays open for the life of the process.
    let _held_open = probe_session;

    // **The client stops parking here, and that is M8 Part B's point.** Until now it waited on
    // a notification nothing signals, so the screen it left behind was the only thing a gate
    // could look at — which is why Part A could not take a screendump of a *switched* desktop:
    // nothing host-side could tell the guest to switch. A registered chord is exactly that
    // channel. The host injects it over QMP, this loop acts on it, and the screen the gate
    // captures is one the host asked for.
    match manager.as_mut() {
        Some(mgr) => serve_hotkeys(mgr, notif),
        // No manager channel: there is nothing to register a chord on, and the line above
        // already says so. Park as this client did before Part B, so the scene stays up.
        None => park(notif),
    }
}

/// Wait forever on a notification nothing signals — what this client did before it served
/// chords, kept for the path where there is no manager channel to serve them on.
fn park(notif: u64) -> ! {
    loop {
        // SAFETY: waiting on this process's own notification handle, which nothing
        // signals; the deadline is infinite, so this parks rather than spins.
        let handles = [notif];
        let mut results = [0u8; 24];
        unsafe {
            syscall4(
                SYS_WAIT,
                handles.as_ptr() as u64,
                1,
                results.as_mut_ptr() as u64,
                u64::MAX,
            )
        };
    }
}

/// Register a chord and act on it until the process is killed.
///
/// **Toggling, not one-way**, so a gate can capture the switched screen *and* prove the scene
/// comes back — a compositor that filtered a window out permanently, or lost its buffer on the
/// way, would pass a one-way check and fail this one.
fn serve_hotkeys(mgr: &mut ChannelTransport, notif: u64) -> ! {
    use libkern::abi::{KEY_F1, KEY_SPACE};
    use librsproto::surface::{MOD_META, MgrHotkey, OP_MGR_HOTKEY, OP_MGR_REGISTER_HOTKEY};
    let _ = notif;

    // `Super+F1`: `Super` alone is what a real shell binds for its launcher, so a chord with a
    // second key stays out of the way of anything Part D will want. `F1` because it is the
    // highest function key `libkern::abi` names, and this client must not need a keycode the
    // ABI does not publish.
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
    // **A second registration under the same id must be refused**, not silently replace the
    // first — a manager holding two chords under one id would be told nothing and then wonder
    // why one of them never fires. Checked here because this is the only caller there is.
    let mut reply = [0u8; 8];
    if mgr.request(OP_MGR_REGISTER_HOTKEY, &body, None, &mut reply).is_ok() {
        fail(b"ui-testclient: a duplicate hotkey id was ACCEPTED\n");
    }
    // **A second chord whose action moves nothing**, and it exists for one reason: key repeat.
    // A consumed chord must not arm a repeat, and the only gate that can see that is one where
    // the chord is *held* — but `Super+F1` empties the current desktop, so `focus_candidate`
    // becomes `None` and `fire_repeat` cancels itself. The gate was immune by coincidence of
    // what this client did with the chord, which is why the control passed against a compositor
    // that did arm one (PR #241 review, blocking 1). This chord only prints, so focus is
    // untouched and a wrongly-armed repeat reaches the focused window and is logged there.
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
                // The event echoes the chord, so a compositor that matched the wrong one is
                // caught here rather than looking like a missing event.
                if got.mods != MOD_META || got.code != KEY_F1 {
                    fail(b"ui-testclient: a Hotkey event carried the wrong chord\n");
                }
                current = if current == 1 { 2 } else { 1 };
                if !set_current_desktop(mgr, current) {
                    fail(b"ui-testclient: SetCurrentDesktop was refused\n");
                }
                Line::new()
                    .s(b"ui-testclient: hotkey fired -> desktop ")
                    .u(current as u64)
                    .end();
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
