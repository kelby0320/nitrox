//! `tty-server` — the userspace terminal server.
//!
//! Bound at `/dev/tty` by init. A client resolving it gets a **fresh per-caller channel**
//! (an `OBJECT_KIND_CHANNEL` resolve reply, the same shape the logging service uses), and
//! then speaks `Tty::ReadLine` / `Write` / `SetMode` / `Close` on it.
//!
//! ## Why it exists
//!
//! Console *input* was already a capability — `/dev/console` is a char `DeviceNode` you must
//! hold a handle to. Console *output* was not: `CharBackend` has no write path, so every
//! program printed through `SYS_DEBUG_KPRINT`, an ambient syscall taking no handle. A
//! process with an empty namespace could still write to the console, and nothing could
//! redirect or capture a shell's output because there was no object to redirect.
//!
//! This server closes that: it holds the raw device **exclusively**, and a session gets a
//! channel. `SYS_DEBUG_KPRINT` reverts to being what it is — a kernel debug facility, used
//! here as the write backend and by `eshell`, which runs when this server does not exist.
//!
//! See `docs/architecture/console-and-tty.md`.
//!
//! `#![no_std]` + `#![no_main]`; `libkern` + `libheap` + `librsproto`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use libkern::*;
use librsproto::error::error_body;
use librsproto::namespace::{OBJECT_KIND_CHANNEL, parse_resolve_request, resolve_reply};
use librsproto::{
    OP_NS_RESOLVE, OP_TTY_CLOSE, OP_TTY_READ_LINE, OP_TTY_SET_MODE, OP_TTY_WRITE, RS_FLAG_ERROR,
    RS_FLAG_REPLY, TTY_MODE_ECHO, decode, encode,
};
use tty_server::{Discipline, Step};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
/// One `sys_wait` slot goes to the serving endpoint and one to the outstanding console
/// read, so the rest bound how many terminals exist at once.
const MAX_TTYS: usize = libkern::abi::MAX_WAIT_HANDLES - 2;
/// Bytes per console read submission.
const READ_CHUNK: u64 = 64;

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_HANDLES: [u64; libkern::abi::MAX_WAIT_HANDLES] =
    [0; libkern::abi::MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * libkern::abi::MAX_WAIT_HANDLES] =
    [0; 24 * libkern::abi::MAX_WAIT_HANDLES];
static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;

/// **The write backend.** Serial today via the kernel's debug write; a compositor surface
/// later. The seam is deliberate — a terminal emulator is another backend for the same
/// discipline, and discovering that after the fact would cost a rewrite.
///
/// That this is `SYS_DEBUG_KPRINT` is now an implementation detail of *one* server rather
/// than the way every program prints.
fn backend_write(bytes: &[u8]) {
    if !bytes.is_empty() {
        kprint(bytes);
    }
}

fn exit(code: i64) -> ! {
    // SAFETY: SYS_PROCESS_EXIT terminates this process.
    unsafe { syscall1(SYS_PROCESS_EXIT, code as u64) };
    loop {
        core::hint::spin_loop();
    }
}

/// One terminal: its channel, its editing state, and the request it owes a reply to.
struct Tty {
    /// The server-side endpoint. `0` marks a freed slot.
    ch: u64,
    disc: Discipline,
    /// The `request_id` of an outstanding `ReadLine`, if the client is waiting for a line.
    waiting: Option<u64>,
}

/// Resolve `path` in `ns` requesting `rights`; `0` on failure.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights)
    };
    if po < 0 {
        return 0;
    }
    let (status, handle) = po_wait(po as u64);
    if status != 0 { 0 } else { handle }
}

/// Wait one handle and read `(status, value)` out of its `IoResult` (status @8, value @16).
fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    let (status, value) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 { (-1, 0) } else { (status, value) }
}

/// A connected channel pair, or `None`.
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

/// Send `Meta::Ready` on `control`, transferring the endpoint init binds at `/dev/tty`.
fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let Some(body_len) = librsproto::meta::ready(&mut body, b"tty-server") else {
        return false;
    };
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto message goes at offset 24.
    unsafe {
        let Some(rs_len) = encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            librsproto::OP_READY,
            0,
            0,
            &body[..body_len],
            1,
        ) else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = kernel_end;
        syscall5(
            SYS_CHANNEL_SEND,
            control,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Send a reply on `ch`: `op`/`request_id` echoed, `body` as the payload.
fn reply(ch: u64, op: u16, request_id: u64, body: &[u8]) {
    // SAFETY: REPLY_MSG is a valid buffer; no transferred handles.
    unsafe {
        let Some(rs_len) = encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, RS_FLAG_REPLY, body, 0)
        else {
            return;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

/// Send an error reply, echoing `op`/`request_id` so the caller can match it.
fn reply_error(ch: u64, op: u16, request_id: u64, kerror: i32) {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = error_body(&mut ebody, kerror, 0, b"").unwrap_or(0);
    // SAFETY: REPLY_MSG is a valid buffer.
    unsafe {
        let Some(rs_len) = encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            op,
            request_id,
            RS_FLAG_REPLY | RS_FLAG_ERROR,
            &ebody[..elen],
            0,
        ) else {
            return;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

/// Reply to a forwarded `/dev/tty` resolve with a fresh terminal channel.
fn open_tty(serve_end: u64, request_id: u64, ttys: &mut Vec<Tty>) {
    if ttys.len() >= MAX_TTYS {
        reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::WouldBlock.as_i32());
        return;
    }
    let Some((client_end, server_end)) = make_channel() else {
        reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::KernelError.as_i32());
        return;
    };
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the channel rides in handles[0].
    let sent = unsafe {
        let Some(rs_len) = encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            OP_NS_RESOLVE,
            request_id,
            RS_FLAG_REPLY,
            &body,
            1,
        ) else {
            return;
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
    };
    if sent {
        ttys.push(Tty { ch: server_end, disc: Discipline::new(), waiting: None });
        kprint(b"tty-server: terminal opened\n");
    } else {
        // The reply did not go, so the client end never moved — reclaim both.
        // SAFETY: closing our own handles.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, client_end);
            syscall1(SYS_HANDLE_CLOSE, server_end);
        }
    }
}

/// Free terminal `i`: close its endpoint and drop it.
///
/// **This is the revocation point.** Handles are refcounted and this kernel has none, so a
/// process that outlived its session while holding a `/dev/tty` handle cannot have it taken
/// away. The server declining to serve the channel is what makes teardown a guarantee
/// rather than a convention — see `docs/architecture/console-and-tty.md`.
fn free_tty(ttys: &mut Vec<Tty>, i: usize) {
    // SAFETY: closing our own endpoint.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ttys[i].ch) };
    ttys.remove(i);
    kprint(b"tty-server: terminal closed\n");
}

/// Feed queued input into whichever terminal is waiting for a line, completing its read
/// when a line is finished.
///
/// Input that arrives with nobody waiting stays queued rather than being dropped — typing
/// ahead of a prompt is ordinary, and losing it would be a bug the user cannot see.
fn drive_input(pending: &mut VecDeque<u8>, ttys: &mut Vec<Tty>) {
    while !pending.is_empty() {
        let Some(i) = ttys.iter().position(|t| t.waiting.is_some()) else {
            return; // nobody is reading; hold the bytes
        };
        let b = pending.pop_front().expect("non-empty");
        match ttys[i].disc.feed(b) {
            Step::None => {}
            Step::Echo(e) => backend_write(&e),
            Step::Line { bytes, echo } => {
                backend_write(&echo);
                let rid = ttys[i].waiting.take().expect("waiting");
                reply(ttys[i].ch, OP_TTY_READ_LINE, rid, &bytes);
            }
            Step::Eof => {
                // Ctrl-D at an empty prompt. Answered as an *error* rather than an empty
                // line, because those are different answers and a reader that conflated
                // them would either exit on a stray Enter or never exit at all.
                // `PeerClosed` is the honest code: the input side has ended.
                backend_write(b"\r\n");
                let rid = ttys[i].waiting.take().expect("waiting");
                reply_error(ttys[i].ch, OP_TTY_READ_LINE, rid, KError::PeerClosed.as_i32());
            }
        }
    }
}

/// Handle one request on terminal `i`'s channel. Returns `false` if the terminal is gone.
fn serve_tty(ttys: &mut Vec<Tty>, i: usize, pending: &mut VecDeque<u8>) -> bool {
    let ch = ttys[i].ch;
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
        // `PeerClosed` means the last holder is gone: the terminal is finished. Anything
        // else is an empty ring.
        return rr != KError::PeerClosed.as_i32() as i64;
    }
    // SAFETY: bounded read-only slice over the just-received message.
    let (op, request_id, body) = unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        match decode(req) {
            Ok(m) => (m.op, m.request_id, m.body),
            Err(_) => return true,
        }
    };
    match op {
        OP_TTY_WRITE => {
            backend_write(body);
            reply(ch, OP_TTY_WRITE, request_id, &[]);
        }
        OP_TTY_SET_MODE => {
            let flags = body.first().copied().unwrap_or(TTY_MODE_ECHO);
            ttys[i].disc.set_echo(flags & TTY_MODE_ECHO != 0);
            reply(ch, OP_TTY_SET_MODE, request_id, &[]);
        }
        OP_TTY_READ_LINE => {
            if ttys[i].waiting.is_some() {
                // One outstanding read per terminal: two would race for the same line and
                // there is no rule that says which should win.
                reply_error(ch, OP_TTY_READ_LINE, request_id, KError::WouldBlock.as_i32());
            } else {
                ttys[i].waiting = Some(request_id);
                drive_input(pending, ttys);
            }
        }
        OP_TTY_CLOSE => {
            reply(ch, OP_TTY_CLOSE, request_id, &[]);
            return false;
        }
        _ => reply_error(ch, op, request_id, KError::Unsupported.as_i32()),
    }
    true
}

/// The serve loop: the forwarding endpoint, every open terminal, and one outstanding
/// console read, all in one `sys_wait`.
fn serve_loop(serve_end: u64, console: u64, buf_h: u64, buf_addr: u64) -> ! {
    kprint(b"tty-server: serving /dev/tty over the console\n");
    let mut ttys: Vec<Tty> = Vec::new();
    let mut pending: VecDeque<u8> = VecDeque::new();
    let mut read_po: u64 = 0;

    loop {
        // Submit a console read **only when a terminal is actually waiting for a line.**
        //
        // Not an optimisation. The console driver is single-reader, and until every client
        // has moved onto this server, `session-mgr`'s login and `nxsh`'s REPL still read
        // the device directly. A permanently-outstanding read here steals their input —
        // which is precisely what happened: the interactive login test timed out waiting
        // for a password prompt whose keystrokes this server had swallowed.
        //
        // Reading only on demand means a terminal with no reader competes with nobody, so
        // the migration can happen one client at a time instead of all at once.
        let anyone_waiting = ttys.iter().any(|t| t.waiting.is_some());
        if read_po == 0 && anyone_waiting {
            let op = libkern::abi::IoOp {
                opcode: libkern::abi::IO_OPCODE_READ,
                flags: 0,
                buffer: buf_h,
                buf_offset: 0,
                offset: 0,
                length: READ_CHUNK,
            };
            // SAFETY: `console` is a char DeviceNode with READ; `&op` is a valid IoOp.
            let po = unsafe {
                syscall2(SYS_IO_SUBMIT, console, (&op as *const libkern::abi::IoOp) as u64)
            };
            if po >= 0 {
                read_po = po as u64;
            }
        }

        let count = {
            // SAFETY: WAIT_HANDLES has MAX_WAIT_HANDLES slots; `n` is bounded by
            // 1 + MAX_TTYS + 1, which is that limit by construction.
            unsafe {
                WAIT_HANDLES[0] = serve_end;
                let mut n = 1usize;
                for t in ttys.iter() {
                    WAIT_HANDLES[n] = t.ch;
                    n += 1;
                }
                if read_po != 0 {
                    WAIT_HANDLES[n] = read_po;
                    n += 1;
                }
                n
            }
        };
        // SAFETY: valid wait buffers sized for `count`.
        let waited = unsafe {
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                count as u64,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if waited < 1 {
            continue;
        }

        for j in 0..(waited as usize) {
            let off = j * 24;
            // SAFETY: `waited` records were written; `off + 8` is inside WAIT_RESULTS.
            let h = unsafe {
                u64::from_le_bytes([
                    WAIT_RESULTS[off], WAIT_RESULTS[off + 1], WAIT_RESULTS[off + 2],
                    WAIT_RESULTS[off + 3], WAIT_RESULTS[off + 4], WAIT_RESULTS[off + 5],
                    WAIT_RESULTS[off + 6], WAIT_RESULTS[off + 7],
                ])
            };
            if h == serve_end {
                drain_resolves(serve_end, &mut ttys);
            } else if read_po != 0 && h == read_po {
                let n = console_bytes(off);
                // SAFETY: the kernel wrote `n` bytes into the mapped read buffer.
                for k in 0..n {
                    let b = unsafe { ((buf_addr + k) as *const u8).read_volatile() };
                    pending.push_back(b);
                }
                read_po = 0; // consumed; the next iteration submits another
                drive_input(&mut pending, &mut ttys);
            } else if let Some(i) = ttys.iter().position(|t| t.ch == h) {
                if !serve_tty(&mut ttys, i, &mut pending) {
                    free_tty(&mut ttys, i);
                }
            }
        }
    }
}

/// The byte count from a completed console read's `IoResult` (status @8, value @16).
fn console_bytes(off: usize) -> u64 {
    // SAFETY: the record at `off` was written by the wait.
    unsafe {
        let status = i32::from_le_bytes([
            WAIT_RESULTS[off + 8], WAIT_RESULTS[off + 9],
            WAIT_RESULTS[off + 10], WAIT_RESULTS[off + 11],
        ]);
        if status != 0 {
            return 0;
        }
        u64::from_le_bytes([
            WAIT_RESULTS[off + 16], WAIT_RESULTS[off + 17], WAIT_RESULTS[off + 18],
            WAIT_RESULTS[off + 19], WAIT_RESULTS[off + 20], WAIT_RESULTS[off + 21],
            WAIT_RESULTS[off + 22], WAIT_RESULTS[off + 23],
        ])
        .min(READ_CHUNK)
    }
}

/// Drain every queued forwarded resolve on the serving endpoint.
fn drain_resolves(serve_end: u64, ttys: &mut Vec<Tty>) {
    loop {
        // SAFETY: valid recv out-params.
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
            return;
        }
        // SAFETY: bounded read-only slice over the received message.
        let (op, request_id, ok) = unsafe {
            let payload_len =
                u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let req = core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            match decode(req) {
                // Only the terminal itself is addressable: `/dev/tty/anything` is not a
                // name this server has.
                Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                    Some(r) => (m.op, m.request_id, r.suffix.is_empty()),
                    None => (m.op, m.request_id, false),
                },
                Ok(m) => (m.op, m.request_id, false),
                Err(_) => (0, 0, false),
            }
        };
        if ok {
            open_tty(serve_end, request_id, ttys);
        } else if op == OP_NS_RESOLVE {
            reply_error(serve_end, op, request_id, KError::NotFound.as_i32());
        } else {
            reply_error(serve_end, op, request_id, KError::Unsupported.as_i32());
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the inherited root
/// namespace (resolves `/dev/console`), `rdx` = init's control endpoint, `rcx` unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"tty-server: up\n");

    // The raw device, held **exclusively** from here on. A session gets `/dev/tty` and not
    // `/dev/console`, so it cannot reach the device at all — which also retires the
    // driver's single-reader assumption, previously kept only by session-mgr and nxsh
    // happening not to read at the same time.
    let console = ns_lookup(root_ns, b"/dev/console", RIGHT_READ);
    if console == 0 {
        kprint(b"tty-server: /dev/console not found\n");
        exit(1);
    }

    // One page, mapped once, for every console read.
    let buf_h = unsafe { syscall4(SYS_MEMORY_CREATE, 4096, 0, 0, 0) };
    if buf_h < 0 {
        kprint(b"tty-server: read buffer FAIL\n");
        exit(1);
    }
    let buf_h = buf_h as u64;
    let buf_addr =
        unsafe { syscall4(SYS_MEMORY_MAP, buf_h, 0, 4096, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if buf_addr < 0 {
        kprint(b"tty-server: read buffer map FAIL\n");
        exit(1);
    }

    let Some((kernel_end, serve_end)) = make_channel() else {
        kprint(b"tty-server: channel create FAIL\n");
        exit(1);
    };
    if !send_ready(control, kernel_end) {
        kprint(b"tty-server: Ready send FAIL\n");
        exit(1);
    }
    serve_loop(serve_end, console, buf_h, buf_addr as u64);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"tty-server: PANIC\n");
    exit(1);
}
