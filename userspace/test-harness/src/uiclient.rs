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
use libkern::{SYS_MEMORY_CREATE, SYS_MEMORY_MAP, SYS_WAIT, exit, kprint, syscall4};
use librsproto::surface::Role;
use libui::{Window, ipc::ChannelTransport};

/// `alloc` backing — rendering the reference scene allocates.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// Frames to commit. **More than the buffer count on purpose**: with two buffers, frame 3
/// is only reachable if a `Release` arrived for frame 1's buffer.
const FRAMES: usize = 6;
/// Buffers the client allocates. Two is the minimum the protocol permits.
const BUFFERS: usize = 2;

fn kprint_u64(n: u64) {
    if n == 0 {
        kprint(b"0");
        return;
    }
    let mut d = [0u8; 20];
    let (mut i, mut n) = (0usize, n);
    while n > 0 {
        d[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    let mut out = [0u8; 20];
    for j in 0..i {
        out[j] = d[i - 1 - j];
    }
    kprint(&out[..i]);
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

    kprint(b"ui-testclient: info id=");
    kprint_u64(info.id as u64);
    kprint(b" ");
    kprint_u64(info.width as u64);
    kprint(b"x");
    kprint_u64(info.height as u64);
    kprint(b" role=");
    kprint_u64(info.role as u64);
    kprint(b"\n");

    info.id == id
        && info.width == want_w
        && info.height == want_h
        && info.role == librsproto::surface::ROLE_NORMAL
}

/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments.
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

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _boot2: u64) -> ! {
    kprint(b"ui-testclient: up\n");

    // 1. A session. The compositor mints a channel per resolve of `/dev/draw/new`.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let transport = match unsafe { ChannelTransport::connect(root_ns) } {
        Ok(t) => t,
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
    kprint(b"ui-testclient: window ");
    kprint_u64(win.id() as u64);
    kprint(b"\n");

    // 2b. Read the window's own metadata back through the *numbered* path. This is the
    //     other half of Part B: `/dev/draw/new` mints a session, `/dev/draw/<N>/info`
    //     answers with a mapped snapshot. Checking it here rather than in a host test is
    //     what proves the resolve suffix, the id parsing and the object hand-off agree.
    if !check_info(root_ns, win.id(), w, h) {
        fail(b"ui-testclient: /dev/draw/<N>/info FAILED\n");
    }

    // 3. Shared memory. Rendered once; both buffers get the same picture, so whichever is
    //    on screen at the end is the one `check-display` expects.
    let pitch = w as usize * 4;
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
                kprint(b"ui-testclient: STALLED at frame ");
                kprint_u64(frame as u64);
                fail(b" -- no buffer released\n");
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

    kprint(b"ui-testclient: committed ");
    kprint_u64(FRAMES as u64);
    kprint(b" frames over ");
    kprint_u64(BUFFERS as u64);
    kprint(b" buffers\n");
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
