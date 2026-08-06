//! `compositor` — the resource server behind `/dev/draw` (plan M2 Part B).
//!
//! **Everything that decides is in the library half** ([`compositor`]): the window stack,
//! roles and struts, request dispatch, and compositing, all host-tested without a kernel.
//! This file is the parts that cannot be: the IPC loop, mapping clients' transferred
//! `MemoryObject`s, and driving the framebuffer.
//!
//! ## Shape
//!
//! Modelled on `profile-server`, which serves `/bin` the same way:
//!
//! 1. Acquire `/dev/framebuffer` — **authority is the binding**, so a compositor is simply
//!    a process whose namespace contains it (`display-substrate.md` §3).
//! 2. Mint a forwarding channel pair; send `Meta::Ready` on the control channel
//!    transferring the kernel end, which the supervisor binds at `/dev/draw`.
//! 3. Serve. A forwarded resolve of `new` mints a **session channel**: the compositor
//!    keeps the server end and hands the client end back as the resolve's answer. There is
//!    no distinct "connection" object — a connection *is* a channel, which is what makes
//!    the per-connection ownership rule enforceable: a request's identity is the endpoint
//!    it arrived on.
//!
//! ## The one genuinely new mechanism
//!
//! `AttachBuffer` carries a `MemoryObject` handle in the message's transfer slot. The
//! compositor maps it **once** and keeps the mapping for the buffer's life — that is the
//! whole point of the design (`display-substrate.md` §4: the handle is transferred once,
//! not per frame). Everything before this milestone was logic testable on the host; this
//! part can only be proven by booting.

#![no_std]
#![no_main]

extern crate alloc;

use compositor::server::{Connection, Outcome, SurfaceError, disconnect, dispatch};
use compositor::{BufferSource, WindowStack};
use libdraw::format::Rgb;
use libdraw::framebuffer::{Framebuffer, RawFramebuffer};
use libkern::{
    SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_HANDLE_CLOSE,
    SYS_MEMORY_MAP, SYS_WAIT, exit, kprint, syscall4, syscall5,
};
use libkern::error::KError;
use librsproto::namespace::{OBJECT_KIND_CHANNEL, resolve_reply};
use librsproto::surface::{OP_ATTACH_BUFFER, OP_RELEASE};
use librsproto::{OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

/// `alloc` backing — the window stack and its buffer lists allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// IPC message buffer length.
const MSG_LEN: usize = 4096;
/// Offset of the rsproto payload inside an `IpcMsg`.
const PAYLOAD_OFF: usize = 24;
/// The background the screen is cleared to where no window covers.
const BACKGROUND: Rgb = Rgb::new(0x0E, 0x14, 0x1B);

/// Largest Surface request body (`AttachBuffer`/`Commit` are 24 bytes).
const MAX_BODY: usize = 64;

/// One less than the wait limit: the forwarding endpoint takes the first slot.
const MAX_SESSIONS: usize = libkern::abi::MAX_WAIT_HANDLES - 1;

static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;
static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 4] = [0; 4];
static mut RECV_COUNT: u64 = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 4] = [0; 4];
static mut SESSION_CH: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];
static mut WAIT_HANDLES: [u64; libkern::abi::MAX_WAIT_HANDLES] =
    [0; libkern::abi::MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * libkern::abi::MAX_WAIT_HANDLES] =
    [0; 24 * libkern::abi::MAX_WAIT_HANDLES];

/// A client buffer the compositor has mapped.
struct MappedBuffer {
    window: u32,
    buffer: u32,
    addr: *mut u8,
    len: usize,
}

/// Everything the serve loop owns.
struct Server {
    stack: WindowStack,
    /// Per-session connection state, parallel to `SESSION_CH`.
    conns: [Connection; MAX_SESSIONS],
    /// Mapped client buffers, in attach order.
    buffers: alloc::vec::Vec<MappedBuffer>,
}

impl BufferSource for Server {
    fn pixels(&self, window: u32, buffer: u32) -> Option<&[u8]> {
        let b = self.buffers.iter().find(|b| b.window == window && b.buffer == buffer)?;
        // SAFETY: `addr`/`len` come from a successful `sys_memory_map` of the client's
        // `MemoryObject`, which stays mapped for this process's life (nothing unmaps it),
        // and the slice is read-only and borrowed no longer than `&self`.
        Some(unsafe { core::slice::from_raw_parts(b.addr, b.len) })
    }
}

/// Create a connected forwarding-channel pair. Returns `(kernel_end, serve_end)`.
fn make_channel() -> Option<(u64, u64)> {
    // SAFETY: CTRL_OUT0/CTRL_OUT1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL_OUT0) as u64, (&raw mut CTRL_OUT1) as u64, 4, 0)
    };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const CTRL_OUT0).read(), (&raw const CTRL_OUT1).read()) })
}

/// Send `Meta::Ready` on the control channel, transferring `kernel_end`.
fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let Some(body_len) = librsproto::meta::ready(&mut body, b"compositor") else { return false };
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto message goes at PAYLOAD_OFF.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], librsproto::OP_READY, 0, 0, &body[..body_len], 1)
        {
            Some(n) => n,
            None => return false,
        }
    };
    // SAFETY: stamp the IpcMsg header (payload_len @4, handle_count @8) + handle slot.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = kernel_end;
    }
    // SAFETY: valid endpoint + message + 1-handle transfer.
    let sr = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            control,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        )
    };
    sr == 0
}

/// Reply to a forwarded resolve by handing back `client_end` as the answer.
///
/// A connection has no distinct object kind: it *is* a live channel, the same shape
/// `profile-server` uses for a directory session.
fn reply_session(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid; one handle rides the reply.
    unsafe {
        let Some(rs_len) =
            encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1)
        else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = client_end;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Reply to a forwarded resolve with an error.
fn reply_resolve_error(serve_end: u64, request_id: u64, err: KError) -> bool {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = librsproto::error::error_body(&mut ebody, err.as_i32(), 0, b"").unwrap_or(0);
    // SAFETY: REPLY_MSG is a valid buffer; no handles transferred.
    unsafe {
        let Some(rs_len) = encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            OP_NS_RESOLVE,
            request_id,
            RS_FLAG_REPLY | RS_FLAG_ERROR,
            &ebody[..elen],
            0,
        ) else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Send a reply body on a **session** channel.
///
/// The half of the protocol that was missing: `dispatch` produced replies and this never
/// sent them, so a client could not learn its window id and never saw a `Release`. The
/// spec has said "Reply, 4 bytes: the new `window` id" since Part A.
fn reply_on_session(session: u64, op: u16, request_id: u64, body: &[u8]) -> bool {
    // SAFETY: REPLY_MSG is a valid buffer; no handles transferred.
    unsafe {
        let Some(rs_len) =
            encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, RS_FLAG_REPLY, body, 0)
        else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            session,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Tell a client a buffer has left the screen and may be drawn into again.
///
/// Server-initiated, so it carries no request id. Without it a double-buffered client
/// stalls after its second commit, forever.
fn send_release(session: u64, window: u32, buffer: u32) -> bool {
    let mut body = [0u8; librsproto::surface::RELEASE_EVENT_LEN];
    let Some(n) = librsproto::surface::build_release_event(&mut body, window, buffer) else {
        return false;
    };
    reply_on_session(session, OP_RELEASE, 0, &body[..n])
}

/// Open a session: mint a channel, keep the server end, hand the client end back.
fn open_session(serve_end: u64, request_id: u64, srv: &mut Server) -> bool {
    // SAFETY: single-threaded server scanning its own slot table.
    let Some(slot) = (unsafe { (0..MAX_SESSIONS).find(|&i| SESSION_CH[i] == 0) }) else {
        return reply_resolve_error(serve_end, request_id, KError::OutOfHandles);
    };
    let Some((client_end, server_end)) = make_channel() else {
        return reply_resolve_error(serve_end, request_id, KError::OutOfMemory);
    };
    // SAFETY: `slot` is free; recording our end and a fresh connection for it.
    unsafe {
        SESSION_CH[slot] = server_end;
    }
    srv.conns[slot] = Connection::new();
    if !reply_session(serve_end, request_id, client_end) {
        // SAFETY: the reply failed, so the client never received its end; drop both.
        unsafe {
            syscall4(SYS_HANDLE_CLOSE, client_end, 0, 0, 0);
            syscall4(SYS_HANDLE_CLOSE, server_end, 0, 0, 0);
            SESSION_CH[slot] = 0;
        }
        return false;
    }
    true
}

/// Close session `slot` and destroy everything the client had on screen.
fn close_session(slot: usize, srv: &mut Server) {
    // SAFETY: closing our own endpoint and clearing the slot.
    unsafe {
        if SESSION_CH[slot] != 0 {
            syscall4(SYS_HANDLE_CLOSE, SESSION_CH[slot], 0, 0, 0);
            SESSION_CH[slot] = 0;
        }
    }
    // A client that exits without destroying its windows must not leave them on screen.
    disconnect(&mut srv.conns[slot], &mut srv.stack);
    srv.buffers.retain(|b| srv.stack.window(b.window).is_some());
}

/// Map a client's transferred `MemoryObject` and record it for `BufferSource`.
///
/// Mapped **once, at attach**, and kept for the buffer's life — the handle crosses the
/// channel a single time, not per frame.
fn map_attached_buffer(srv: &mut Server, body: &[u8], handle: u64) -> bool {
    let Some(req) = librsproto::surface::parse_attach_buffer_request(body) else { return false };
    let len = req.pitch as usize * req.height as usize;
    // SAFETY: `handle` is a `MemoryObject` the client transferred; mapping it read-only
    // with the size its own geometry declares.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, handle, 0, len as u64, libkern::RIGHT_MAP_READ) };
    if addr <= 0 {
        // SAFETY: the map failed, so nothing references the object; drop our handle rather
        // than leaking it for the compositor's life.
        unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        return false;
    }
    srv.buffers.push(MappedBuffer {
        window: req.window,
        buffer: req.buffer,
        addr: addr as *mut u8,
        len,
    });
    // The mapping holds its own reference to the object, so the handle can go.
    // SAFETY: closing our reference; the mapping outlives it.
    unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
    true
}

/// Recomposite the whole screen.
///
/// Damage-bounded repaint is an optimisation the protocol already carries and this does not
/// yet exploit — a full repaint is always correct, and this milestone has one client.
fn repaint(srv: &Server, fb: &mut RawFramebuffer) {
    let bounds = fb.geometry().bounds();
    srv.stack.compose_into(fb, BACKGROUND, srv, &[bounds]);
}

/// The wire error a rejected request reports.
fn surface_errno(e: SurfaceError) -> KError {
    match e {
        SurfaceError::Malformed => KError::InvalidArgument,
        SurfaceError::Unsupported => KError::Unsupported,
        // A window belonging to another connection reports exactly what a nonexistent one
        // does, so the reply cannot be used to probe which ids exist.
        SurfaceError::NotFound => KError::NotFound,
        SurfaceError::Rejected(_) => KError::InvalidArgument,
    }
}

/// Send an error reply on a session channel.
fn reply_error_on_session(session: u64, op: u16, request_id: u64, err: KError) -> bool {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = librsproto::error::error_body(&mut ebody, err.as_i32(), 0, b"").unwrap_or(0);
    // SAFETY: REPLY_MSG is a valid buffer; no handles transferred.
    unsafe {
        let Some(rs_len) = encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            op,
            request_id,
            RS_FLAG_REPLY | RS_FLAG_ERROR,
            &ebody[..elen],
            0,
        ) else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            session,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Serve one request on session `slot`. Returns `false` if the session should close.
fn serve_session(slot: usize, srv: &mut Server, fb: &mut RawFramebuffer) -> bool {
    let ch = unsafe { SESSION_CH[slot] };
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr != 0 {
        // `PeerClosed` is the client going away, which is the normal end of a session.
        return rr != KError::PeerClosed.as_i32() as i64;
    }

    // Copy the body out rather than holding a borrow of `RECV_MSG` across dispatch. Every
    // Surface body is at most 24 bytes, so this is a copy of nothing; the alternative is
    // two live `static mut` borrows at once.
    let mut body_buf = [0u8; MAX_BODY];
    let (op, request_id, body_len, handle) = unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            (&raw const RECV_MSG[PAYLOAD_OFF]) as *const u8,
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        let Ok(m) = decode(req) else { return true };
        let n = m.body.len().min(MAX_BODY);
        body_buf[..n].copy_from_slice(&m.body[..n]);
        let h = if (&raw const RECV_COUNT).read() > 0 { RECV_HANDLES[0] } else { 0 };
        (m.op, m.request_id, n, h)
    };
    let body = &body_buf[..body_len];

    // SAFETY: a bounded mutable slice over the reply buffer; `body` is a local copy.
    let outcome = unsafe {
        dispatch(
            &mut srv.conns[slot],
            &mut srv.stack,
            op,
            body,
            &mut REPLY_MSG[PAYLOAD_OFF + 16..],
        )
    };

    // **Map only after `dispatch` accepted the attach.** Mapping first would record the
    // client's memory under ids it does not own: `dispatch` would answer `NotFound`, the
    // entry would stay, and `Server::pixels` resolves by first match — so another client's
    // pixels would be painted into the rightful owner's window. Doing it in this order
    // makes the ownership boundary cover the pixels, not just the stack.
    if op == OP_ATTACH_BUFFER && handle != 0 {
        if matches!(outcome, Outcome::Applied { .. }) {
            if !map_attached_buffer(srv, body, handle) {
                return true;
            }
        } else {
            // Rejected: drop the handle rather than leaking it for the compositor's life.
            // SAFETY: closing a handle this process owns and will not use.
            unsafe { syscall4(SYS_HANDLE_CLOSE, handle, 0, 0, 0) };
        }
    }

    match outcome {
        Outcome::Reply(len) => {
            // SAFETY: `dispatch` wrote `len` bytes at this offset in REPLY_MSG. Copy them
            // out before `reply_on_session` re-encodes into the same buffer.
            let mut body = [0u8; MAX_BODY];
            let n = len.min(MAX_BODY);
            unsafe {
                body[..n].copy_from_slice(&REPLY_MSG[PAYLOAD_OFF + 16..PAYLOAD_OFF + 16 + n]);
            }
            reply_on_session(ch, op, request_id, &body[..n]);
            repaint(srv, fb);
        }
        Outcome::Applied { release } => {
            if let Some((window, buffer)) = release {
                send_release(ch, window, buffer);
            }
            // A destroy is transitive, so drop every mapping whose window is gone.
            // Otherwise a client looping create/attach/destroy grows the compositor's
            // address space without bound.
            srv.buffers.retain(|b| srv.stack.window(b.window).is_some());
            repaint(srv, fb);
        }
        Outcome::Failed(e) => {
            kprint(match e {
                SurfaceError::Malformed => b"compositor: malformed request\n" as &[u8],
                SurfaceError::Unsupported => b"compositor: unsupported op\n",
                SurfaceError::NotFound => b"compositor: no such window for this connection\n",
                SurfaceError::Rejected(_) => b"compositor: request rejected\n",
            });
            reply_error_on_session(ch, op, request_id, surface_errno(e));
        }
    }
    true
}

/// The serve loop: the forwarding endpoint plus every open session.
fn serve_loop(serve_end: u64, mut fb: RawFramebuffer, srv: &mut Server) -> ! {
    kprint(b"compositor: serving /dev/draw\n");
    loop {
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots; `n` is bounded by
        // `1 + MAX_SESSIONS`, which is that limit by construction.
        let waited = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1usize;
            for i in 0..MAX_SESSIONS {
                if SESSION_CH[i] != 0 {
                    WAIT_HANDLES[n] = SESSION_CH[i];
                    n += 1;
                }
            }
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                n as u64,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if waited < 1 {
            continue;
        }

        let mut serve_signalled = false;
        for j in 0..(waited as usize) {
            let off = j * 24;
            // SAFETY: `waited` records were written; `off + 8` stays inside WAIT_RESULTS.
            let h = unsafe {
                u64::from_le_bytes([
                    WAIT_RESULTS[off],
                    WAIT_RESULTS[off + 1],
                    WAIT_RESULTS[off + 2],
                    WAIT_RESULTS[off + 3],
                    WAIT_RESULTS[off + 4],
                    WAIT_RESULTS[off + 5],
                    WAIT_RESULTS[off + 6],
                    WAIT_RESULTS[off + 7],
                ])
            };
            if h == serve_end {
                serve_signalled = true;
                continue;
            }
            // SAFETY: scanning our own slot table for the signalled endpoint.
            if let Some(slot) = unsafe { (0..MAX_SESSIONS).find(|&i| SESSION_CH[i] == h) }
                && !serve_session(slot, srv, &mut fb)
            {
                close_session(slot, srv);
                let bounds = fb.geometry().bounds();
                srv.stack.compose_into(&mut fb, BACKGROUND, srv, &[bounds]);
            }
        }
        if !serve_signalled {
            continue;
        }

        // A forwarded resolve. Only `new` is answerable today: numbered window paths and
        // `info` are the rest of Part B.
        // SAFETY: valid recv out-params (a Resolve carries no transferred handles).
        let rr = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                serve_end,
                (&raw mut RECV_MSG) as u64,
                (&raw mut RECV_HANDLES) as u64,
                (&raw mut RECV_COUNT) as u64,
            )
        };
        if rr != 0 {
            continue;
        }
        // SAFETY: bounded read of the payload, then of the resolve's suffix.
        let (request_id, is_new, ok) = unsafe {
            let payload_len =
                u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let req = core::slice::from_raw_parts(
                (&raw const RECV_MSG[PAYLOAD_OFF]) as *const u8,
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            match decode(req) {
                Ok(m) if m.op == OP_NS_RESOLVE => {
                    let suffix = librsproto::namespace::parse_resolve_request(m.body)
                        .map(|r| r.suffix)
                        .unwrap_or(b"");
                    (m.request_id, suffix == b"new", true)
                }
                Ok(m) => (m.request_id, false, true),
                Err(_) => (0, false, false),
            }
        };
        if !ok {
            continue;
        }
        if is_new {
            open_session(serve_end, request_id, srv);
        } else {
            reply_resolve_error(serve_end, request_id, KError::NotFound);
        }
    }
}

/// # Safety
///
/// Called by the kernel's ELF entry with the standard bootstrap arguments; `ctrl` is the
/// control channel the supervisor spawned this server with.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, ctrl: u64) -> ! {
    kprint(b"compositor: up\n");

    // Authority is the binding: a compositor is a process whose namespace has this.
    // SAFETY: `root_ns` is this process's live root namespace, owned for its whole run.
    let (mut fb, info) = match unsafe { libdraw::acquire::acquire(root_ns) } {
        Ok(pair) => pair,
        Err(_) => {
            kprint(b"compositor: no /dev/framebuffer -- cannot serve\n");
            exit(1);
        }
    };
    let _ = info;

    let Some((kernel_end, serve_end)) = make_channel() else {
        kprint(b"compositor: channel create FAIL\n");
        exit(1);
    };
    let mut srv = Server {
        stack: WindowStack::new(),
        conns: core::array::from_fn(|_| Connection::new()),
        buffers: alloc::vec::Vec::new(),
    };

    // Clear **before** announcing readiness, so `Meta::Ready` means "I have taken the
    // screen" rather than "I am about to".
    //
    // Announcing first left the clear racing whatever init spawned next: `bind_compositor`
    // would return while the clear was still pending, and on `-smp 4` two processes then
    // held `/dev/framebuffer` mapped read-write with nothing arbitrating. Ordering it here
    // costs nothing and removes the window entirely, where pacing it against a later signal
    // would only narrow it.
    let bounds = fb.geometry().bounds();
    srv.stack.compose_into(&mut fb, BACKGROUND, &srv, &[bounds]);

    if !send_ready(ctrl, kernel_end) {
        kprint(b"compositor: Ready send FAIL\n");
        exit(1);
    }

    serve_loop(serve_end, fb, &mut srv);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"compositor: panic\n");
    exit(2);
}
