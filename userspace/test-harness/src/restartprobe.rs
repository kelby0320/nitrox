//! `restart-probe` — a server that can be made to exit, for proving that a restarted server is
//! reached again at its path (administration Part E.1).
//!
//! **A test image only.** Declared with `endpoint = "/svc/restart-probe"` and `policy = "always"`,
//! so `service-mgr` binds it through its registry and brings it back after it exits. A resolve of:
//! - **`id`** answers a channel carrying this instance's token — the monotonic time it started, so
//!   a restarted one answers differently;
//! - **`exit`** answers `NotFound`, and then the process exits, as a crash would.
//!
//! `boot-probe` asks for the token, asks it to exit, and resolves the same path until a different
//! token comes back. What that proves is `service-mgr`'s: the new instance's endpoint is bound in
//! the registry, and the root's `/svc/restart-probe`, bound once, reaches it.

#![no_std]
#![no_main]

use libkern::debug::Line;
use libkern::{
    CLOCK_MONOTONIC, KError, SENDMODE_NOBLOCK, SYS_CHANNEL_CREATE, SYS_CHANNEL_RECV,
    SYS_CHANNEL_SEND, SYS_CLOCK_READ, SYS_HANDLE_CLOSE, SYS_WAIT, exit, kprint, syscall1, syscall2,
    syscall4, syscall5,
};
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply,
};
use librsproto::{OP_NS_RESOLVE, OP_READY, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

static mut MSG: [u8; 4096] = [0; 4096];
static mut OUT: [u8; 4096] = [0; 4096];
static mut HANDLES: [u64; 8] = [0; 8];
static mut COUNT: usize = 0;

fn now_ns() -> u64 {
    let mut t = 0u64;
    // SAFETY: `t` is a valid writable out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut t) as u64) };
    t
}

fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

fn channel(depth: u64) -> Option<(u64, u64)> {
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let r = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, depth, 0) };
    (r == 0).then_some((a, b))
}

/// Send an rsproto message on `ch`, moving `handles`.
fn send(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handles: &[u64]) -> bool {
    // SAFETY: OUT is this process's; single-threaded.
    unsafe {
        let Some(n) = encode(&mut OUT[24..], op, request_id, flags, body, handles.len() as u16) else {
            return false;
        };
        OUT[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        OUT[8] = handles.len() as u8;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const OUT) as u64,
            handles.as_ptr() as u64,
            handles.len() as u64,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// An error reply with the whole twelve-byte `ErrorBody`.
fn refuse(ch: u64, request_id: u64, err: KError) {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, b"").unwrap_or(0);
    let _ = send(ch, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &body[..n], &[]);
}

/// Answer a resolve of `id`: a channel, with this instance's token queued on it.
fn answer_id(serve: u64, request_id: u64, token: u64) {
    let Some((theirs, ours)) = channel(2) else {
        return refuse(serve, request_id, KError::OutOfMemory);
    };
    let _ = send(ours, 0, 0, 0, &token.to_le_bytes(), &[]);
    close(ours);
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    if !send(serve, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, &[theirs]) {
        close(theirs);
        refuse(serve, request_id, KError::KernelError);
    }
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = namespace, `rdx` = the control
/// channel `service-mgr` moved in, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _ns: u64, control: u64, _arg0: u64) -> ! {
    let token = now_ns();
    let Some((client_end, serve)) = channel(4) else {
        kprint(b"restart-probe: endpoint create FAIL\n");
        exit(1);
    };
    let mut body = [0u8; 64];
    let n = librsproto::meta::ready(&mut body, b"restart-probe").unwrap_or(0);
    if !send(control, OP_READY, 0, 0, &body[..n], &[client_end]) {
        kprint(b"restart-probe: Ready send FAIL\n");
        exit(1);
    }
    Line::new().s(b"restart-probe: up, instance ").u(token).end();
    loop {
        let handles = [serve];
        let mut results = [0u8; 24];
        // SAFETY: valid one-entry wait arrays on this frame.
        unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
        loop {
            // SAFETY: valid recv out-params.
            let rr = unsafe {
                syscall4(
                    SYS_CHANNEL_RECV,
                    serve,
                    (&raw mut MSG) as u64,
                    (&raw mut HANDLES) as u64,
                    (&raw mut COUNT) as u64,
                )
            };
            if rr != 0 {
                break;
            }
            // SAFETY: the kernel wrote the count and the handles; none is expected.
            let count = unsafe { (&raw const COUNT).read() }.min(8);
            for k in 0..count {
                // SAFETY: an installed handle, ours to close.
                close(unsafe { (&raw const HANDLES[k]).read() });
            }
            // SAFETY: bounded read of the payload the kernel just wrote.
            let msg = unsafe {
                let len = u32::from_le_bytes([MSG[4], MSG[5], MSG[6], MSG[7]]) as usize;
                core::slice::from_raw_parts(((&raw const MSG) as *const u8).add(24), len.min(4096 - 24))
            };
            let Ok(m) = decode(msg) else { continue };
            if m.op != OP_NS_RESOLVE {
                continue;
            }
            match parse_resolve_request(m.body).map(|r| r.suffix) {
                Some(b"id") => answer_id(serve, m.request_id, token),
                Some(b"exit") => {
                    refuse(serve, m.request_id, KError::NotFound);
                    Line::new().s(b"restart-probe: instance ").u(token).s(b" exiting, as asked").end();
                    exit(1);
                }
                _ => refuse(serve, m.request_id, KError::NotFound),
            }
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"restart-probe: PANIC\n");
    exit(1);
}
