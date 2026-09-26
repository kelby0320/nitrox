//! **One request, one reply** — the channel plumbing a coreutil that is a *client* of a server
//! needs: send a message, wait, and take the answer to it.
//!
//! Moved here from `with` when `account` became its second user (administration Part D.4). Both
//! speak to the view broker and prompt on a terminal, and a password passes through these buffers
//! on the way to either, so **every buffer here is zeroed once the kernel has the message** — a
//! copy left in a stack frame is a copy nothing else will overwrite.

use alloc::string::String;
use alloc::vec::Vec;
use libkern::abi::{IPC_MSG_SIZE, IPC_PAYLOAD_SIZE};
use libkern::scrub;
use libkern::syscall::{SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_HANDLE_CLOSE, SYS_WAIT, syscall1, syscall4, syscall5};
use libkern::{KError, SENDMODE_NOBLOCK};
use librsproto::views::{Outcome, parse_outcome};

/// A received message: `(op, request_id, is_error, body)`.
pub type Msg = (u16, u64, bool, Vec<u8>);

/// Close `h`, if it is a handle.
pub fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process owns.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

/// Wait on `handles`; the index of one that is ready. At most four.
pub fn wait(handles: &[u64]) -> Option<usize> {
    let mut results = [0u8; 24 * 4];
    if handles.len() > 4 {
        return None;
    }
    // SAFETY: valid buffers; at most four handles.
    let n = unsafe {
        syscall4(
            SYS_WAIT,
            handles.as_ptr() as u64,
            handles.len() as u64,
            results.as_mut_ptr() as u64,
            u64::MAX,
        )
    };
    if n < 1 {
        return None;
    }
    let h = u64::from_le_bytes(results[..8].try_into().ok()?);
    handles.iter().position(|&x| x == h)
}

/// Receive one message on `ch`. `Ok(None)` if nothing was queued — a wake with nothing behind
/// it — and `Err(())` if the peer has gone, which are different answers to a caller waiting on
/// a program's exit. Handles that came with it are closed: nothing here expects one.
pub fn recv(ch: u64) -> Result<Option<Msg>, ()> {
    let mut buf = [0u8; IPC_MSG_SIZE];
    let mut hs = [0u64; 8];
    let mut count = 0usize;
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            buf.as_mut_ptr() as u64,
            hs.as_mut_ptr() as u64,
            (&raw mut count) as u64,
        )
    };
    if rr == KError::PeerClosed.as_i32() as i64 {
        return Err(());
    }
    if rr != 0 {
        return Ok(None);
    }
    for &h in &hs[..count.min(8)] {
        close(h);
    }
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let msg = librsproto::decode(&buf[24..24 + len.min(IPC_PAYLOAD_SIZE)])
        .ok()
        .map(|m| (m.op, m.request_id, m.is_error(), m.body.to_vec()));
    // A line read at a password prompt came through here; the caller has its copy.
    scrub(&mut buf);
    Ok(msg)
}

/// Send `op` on `ch`, moving `handles`.
pub fn send(ch: u64, op: u16, request_id: u64, body: &[u8], handles: &[u64]) -> bool {
    let mut buf = [0u8; IPC_MSG_SIZE];
    let count = handles.len() as u16;
    let Some(n) = librsproto::encode(&mut buf[24..], op, request_id, 0, body, count) else {
        return false;
    };
    buf[4..8].copy_from_slice(&(n as u32).to_le_bytes());
    buf[8] = handles.len() as u8;
    // SAFETY: valid message buffer and handle array.
    let sent = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            buf.as_ptr() as u64,
            handles.as_ptr() as u64,
            handles.len() as u64,
            SENDMODE_NOBLOCK,
        ) == 0
    };
    // So did a password, on its way to the broker.
    scrub(&mut buf);
    sent
}

/// Send `op` and wait for the reply to it, as `(is_error, body)`. `None` if it could not be sent
/// or the peer went away first.
pub fn call(ch: u64, op: u16, request_id: u64, body: &[u8], handles: &[u64]) -> Option<(bool, Vec<u8>)> {
    if !send(ch, op, request_id, body, handles) {
        return None;
    }
    loop {
        wait(&[ch])?;
        match recv(ch) {
            Ok(Some((_, rid, err, body))) if rid == request_id => return Some((err, body)),
            Ok(_) => continue,
            Err(()) => return None,
        }
    }
}

/// Resolve `path` in `ns` with `rights`; `0` if it is not there.
pub fn lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    let (st, h) = libfs::lookup_wait(ns, path, rights);
    if st == 0 { h } else { 0 }
}

/// The view broker's answer, as the outcome and its reason. An answer that does not read is a
/// denial saying so.
pub fn outcome(body: &[u8]) -> (Outcome, String) {
    match parse_outcome(body) {
        Some((o, why)) => (o, String::from_utf8_lossy(why).into_owned()),
        None => (Outcome::Denied { retry: false }, String::from("the broker's answer did not read")),
    }
}
