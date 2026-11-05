//! `restart-probe` — a server that can be made to exit, for proving that a restarted server is
//! reached again at its path.
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

use libkern::KError;
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{Carried, Channel, Deadline, Handle, Msg, Only, Ref, WaitSet, time};
use librsproto::namespace::{OBJECT_KIND_CHANNEL, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply};
use librsproto::{OP_NS_RESOLVE, OP_READY, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

fn now_ns() -> u64 {
    time::now().unwrap_or(0)
}

/// Send an rsproto message on `ch`, moving `handles`.
fn send(ch: Ref<'_, Channel, Only>, op: u16, request_id: u64, flags: u32, body: &[u8], handles: &mut Carried) -> bool {
    let mut msg = Msg::new();
    let Some(n) = encode(msg.payload_mut(), op, request_id, flags, body, handles.len() as u16) else {
        return false;
    };
    msg.set_len(n).is_ok() && ch.send(&msg, handles).is_ok()
}

/// An error reply with the whole twelve-byte `ErrorBody`.
fn refuse(ch: Ref<'_, Channel, Only>, request_id: u64, err: KError) {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, b"").unwrap_or(0);
    let _ = send(ch, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &body[..n], &mut Carried::new());
}

/// Answer a resolve of `id`: a channel, with this instance's token queued on it.
fn answer_id(serve: Ref<'_, Channel, Only>, request_id: u64, token: u64) {
    let Ok((theirs, ours)) = Handle::<Channel, Only>::pair(2) else {
        return refuse(serve, request_id, KError::OutOfMemory);
    };
    let _ = send(ours.as_ref(), 0, 0, 0, &token.to_le_bytes(), &mut Carried::new());
    drop(ours);
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    let mut carried = Carried::new();
    let _ = carried.push(theirs);
    if !send(serve, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, &mut carried) {
        drop(carried);
        refuse(serve, request_id, KError::KernelError);
    }
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = namespace, `rdx` = the control
/// channel `service-mgr` moved in, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(rdi: u64, rsi: u64, rdx: u64, rcx: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(rdi, rsi, rdx, rcx) };
    let control = start.take_handle0::<Channel, Only>();
    let token = now_ns();
    let Ok((client_end, serve)) = Handle::<Channel, Only>::pair(4) else {
        log::write(b"restart-probe: endpoint create FAIL\n");
        libos::exit(1);
    };
    let mut body = [0u8; 64];
    let n = librsproto::meta::ready(&mut body, b"restart-probe").unwrap_or(0);
    let mut carried = Carried::new();
    let _ = carried.push(client_end);
    if !control.as_ref().is_some_and(|c| send(c.as_ref(), OP_READY, 0, 0, &body[..n], &mut carried)) {
        log::write(b"restart-probe: Ready send FAIL\n");
        libos::exit(1);
    }
    Line::new().s(b"restart-probe: up, instance ").u(token).end();
    let mut msg = Msg::new();
    loop {
        let mut set = WaitSet::<1>::new();
        let _ = set.add(&serve);
        let _ = set.wait(Deadline::NEVER);
        loop {
            let _sent = match serve.recv(&mut msg) {
                Ok(sent) => sent,
                // **Nothing can reach the probe any more**: the peer stays signalled, so waiting
                // again would spin a CPU for the rest of the boot.
                Err(e) if e.code() == KError::PeerClosed => {
                    log::write(b"restart-probe: nothing can reach it any more; exiting\n");
                    libos::exit(0);
                }
                Err(_) => break,
            };
            let Ok(m) = decode(msg.payload()) else { continue };
            if m.op != OP_NS_RESOLVE {
                continue;
            }
            match parse_resolve_request(m.body).map(|r| r.suffix) {
                Some(b"id") => answer_id(serve.as_ref(), m.request_id, token),
                Some(b"exit") => {
                    refuse(serve.as_ref(), m.request_id, KError::NotFound);
                    Line::new().s(b"restart-probe: instance ").u(token).s(b" exiting, as asked").end();
                    libos::exit(1);
                }
                _ => refuse(serve.as_ref(), m.request_id, KError::NotFound),
            }
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"restart-probe: PANIC\n");
    libos::exit(1);
}
