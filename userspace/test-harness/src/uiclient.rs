//! `ui-testclient` — the display arm's first real client (plan M2 Part D).
//!
//! Everything before this was one half of a conversation. `libui` had only met a mock and
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

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::framebuffer::Framebuffer;
use libdraw::scene;
use libkern::debug::Line;
use libkern::{
    SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_WAIT, exit, kprint, syscall2,
    syscall4,
};
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, OP_ATTACH_BUFFER, OP_COMMIT, Role,
    SURFACE_FORMAT_XRGB8888, build_attach_buffer_request, build_commit_request,
};
use libui::Transport;
use libui::{Window, ipc::ChannelTransport};

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

/// Resolve `/dev/draw/<id>/info`, map it, and check it describes the window we created.
fn check_info(root_ns: u64, id: u32, want_w: u32, want_h: u32) -> bool {
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
    let Ok(path) = core::str::from_utf8(&path[..n]) else { return false };

    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let ns =
        unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: the path resolves to a read-mappable object holding one `WindowInfo`.
    let Ok(obj) = block_on(unsafe { ns.lookup::<Memory, MapRead>(path, Rights::MAP_READ) }) else {
        return false;
    };
    let Ok(addr) = obj.map(32) else { return false };
    // SAFETY: the compositor serves exactly 32 bytes of `WindowInfo` here.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, 32) };
    let Some(info) = librsproto::surface::WindowInfo::read(bytes) else { return false };
    // Unmap once the snapshot is read. `info` mints a fresh object per resolve, so a client
    // that polls it and never unmaps leaks a page each time — the same defect the compositor
    // had on its side of this exchange (PR #175 review, finding 1).
    let _ = obj.unmap(addr as *mut u8, 32);

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
        let mut w = match Window::new(t, CHURN_W, CHURN_H, Role::Normal, BUFFERS) {
            Ok(w) => w,
            Err(_) => {
                Line::new().s(b"churn: Window::new failed at cycle ").u(cycle as u64).end();
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
        // `libui` deliberately cannot express it.
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
            Err(libui::UiError::Server) => {}
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
    let mut win = match Window::new(transport, w, h, Role::Normal, BUFFERS) {
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
    kprint(b"ui-testclient: scene presented via /dev/draw\n");
    kprint(b"ui-testclient: PASSED\n");

    // Park. Exiting would close the channel, and the compositor would destroy this window
    // and repaint — leaving the gate to capture an empty screen.
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

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"ui-testclient: panic\n");
    exit(2);
}
