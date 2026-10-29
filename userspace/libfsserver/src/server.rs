//! **The server loop**: the bootstrap, then forwarded requests, directory sessions and the
//! control channel, for any [`Volume`]. Bootstrap:
//! 1. The supervisor spawns the server with a **control channel** endpoint (`rdx`) and sends a
//!    **setup message** transferring the **block-device** handle and a read-only flag.
//! 2. The server **checks the device holds a filesystem it can serve** ([`Volume::check`]); if
//!    not it sends a refusal in place of the Ready and exits.
//! 3. It creates a **forwarding channel** pair, keeps the serving end, and sends `Meta::Ready`
//!    **transferring the kernel end**, which the supervisor binds
//!    (`docs/rationale/why-supervisor-registration.md`).
//!
//! The request→reply logic is the host-tested [`crate::serve`]; this module is the syscall
//! plumbing, **alloc-free**: one [`Server`], on [`run`]'s stack.

use crate::serve::{
    MAX_FILE, MAX_SUFFIX, Served, apply_size, directory_open, encode_error, encode_refusal, kerror, reason, serve,
    serve_grown, session,
};
use crate::{FsError, Refusal, Volume};
use libkern::*;
use libkern::{Received, channel_recv};
use librsproto::file::{forget_request, parse_touch_request};
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, OBJECT_KIND_NONE, RENAME_REPLACE, RESOLVE_RENAME, RESOLVE_REPLY_LEN, parse_resolve_rename,
    parse_resolve_request, resolve_reply,
};
use librsproto::{OP_FILE_FORGET, OP_FILE_TOUCH, OP_NS_RESOLVE, OP_UNMOUNT, RS_FLAG_REPLY};

const PAGE: u64 = 4096;
const MSG_LEN: usize = 4096;
const PAYLOAD_OFF: usize = 24;

/// The most open directory sessions: the one `sys_wait` set holds `serve_end` plus every live
/// session, so it is the kernel's fan-out limit less one. A full table returns `WouldBlock`; a
/// client that opens a session and stalls pins a slot, which `TODO(server-fanout)` fixes.
const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 1;

/// Body scratch for a `File::ReadDir` reply: [`librsproto::RS_MAX_BODY`], what one message
/// carries. A page less the two headers is too much: a reply over 3,980 bytes is refused by the
/// kernel and the client waits for good.
const DIR_BODY_CAP: usize = librsproto::RS_MAX_BODY;

/// **One server**: made by [`run`] on its own stack, so its state is its own and nothing else
/// reaches it, and a request is borrowed from its inbox while the rest of it replies.
struct Server {
    inbox: Inbox,
    s: State,
}

/// The message last received. [`Inbox::recv`] lends its payload until the next receive, so no
/// receive can overwrite a request still being read.
struct Inbox([u8; MSG_LEN]);

impl Inbox {
    /// Receive one message on `h`: its payload, and what came with it, which closes as it drops;
    /// no request here carries a handle. `Err` with `WouldBlock` once drained, or `PeerClosed`.
    fn recv(&mut self, h: u64) -> Result<(&[u8], Received), i64> {
        let sent = channel_recv(h, &mut self.0)?;
        let len = u32::from_le_bytes([self.0[4], self.0[5], self.0[6], self.0[7]]) as usize;
        Ok((&self.0[PAYLOAD_OFF..PAYLOAD_OFF + len.min(MSG_LEN - PAYLOAD_OFF)], sent))
    }
}

/// A message being sent, and the handles it moves.
struct Outbox {
    msg: [u8; MSG_LEN],
    handles: [u64; 8],
}

impl Outbox {
    fn payload(&mut self) -> &mut [u8] {
        &mut self.msg[PAYLOAD_OFF..]
    }

    /// Stamp the header for `payload_len` bytes and `handle`, if any: how many handles go.
    fn stage(&mut self, payload_len: usize, handle: Option<u64>) -> usize {
        self.msg[4..8].copy_from_slice(&(payload_len as u32).to_le_bytes());
        let count = match handle {
            Some(h) => {
                self.handles[0] = h;
                1
            }
            None => 0,
        };
        self.msg[8] = count as u8;
        count
    }

    /// Send what is staged, with its first `count` handles, on `ch`: the kernel's answer.
    fn send(&self, ch: u64, count: usize, mode: u64) -> i64 {
        // SAFETY: a whole message and its handle array, this outbox's and live for the call.
        unsafe {
            syscall5(SYS_CHANNEL_SEND, ch, self.msg.as_ptr() as u64, self.handles.as_ptr() as u64, count as u64, mode)
        }
    }

    /// Stage and send an error reply on `ch`; whether it went.
    fn error(&mut self, ch: u64, request_id: u64, kerror: i32, op: u16, why: &[u8]) -> bool {
        let n = encode_error(self.payload(), request_id, kerror, op, why);
        let count = self.stage(n, None);
        self.send(ch, count, SENDMODE_NOBLOCK) == 0
    }

    /// Send a forwarded request's reply. The kernel completes the waiting lookup inline (the peer
    /// is its forwarding endpoint), so `NoBlock` is correct. **One the kernel did not take is
    /// answered with an error**, and its handles, which stayed here, are closed: a forwarded
    /// request has no deadline, and its client would wait for ever.
    fn forwarded(&mut self, serve_end: u64, count: usize, request_id: u64, op: u16) {
        if self.send(serve_end, count, SENDMODE_NOBLOCK) == 0 {
            return;
        }
        self.handles[..count].iter().for_each(|&h| close(h));
        let _ = self.error(serve_end, request_id, KError::KernelError.as_i32(), op, b"the reply could not be sent");
    }

    /// Send `Meta::Ready` on `control`, transferring `kernel_end`. NoBlock: the supervisor's
    /// control inbox starts empty, so the first message always has space.
    fn ready(&mut self, control: u64, name: &[u8], kernel_end: u64) -> bool {
        let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 32];
        let Some(body_len) = librsproto::meta::ready(&mut body, name) else { return false };
        let Some(n) = librsproto::encode(self.payload(), librsproto::OP_READY, 0, 0, &body[..body_len], 1) else {
            return false;
        };
        let count = self.stage(n, Some(kernel_end));
        self.send(control, count, SENDMODE_NOBLOCK) == 0
    }

    /// Send the refusal that replaces a Ready, with no handle; NoBlock as for the Ready.
    fn refusal(&mut self, control: u64, why: &impl Refusal) -> bool {
        let Some(n) = encode_refusal(self.payload(), why) else { return false };
        let count = self.stage(n, None);
        self.send(control, count, SENDMODE_NOBLOCK) == 0
    }
}

/// One directory session: its endpoint (`0` for a free slot) and the directory it is bound to. A
/// session addresses entries by name, never path, so it can only touch that directory.
#[derive(Clone, Copy, Default)]
struct Session {
    ch: u64,
    dir: u64,
}

struct Sessions([Session; MAX_SESSIONS]);

impl Sessions {
    fn free(&mut self, slot: usize) {
        let ch = self.0[slot].ch;
        self.0[slot] = Session::default();
        if ch != 0 {
            close(ch);
        }
    }

    /// **End every session on directory `id`**, just removed: a session keeps a bare id, which the
    /// filesystem may give the next directory made, and the session would then reach into that one.
    fn end_on(&mut self, id: u64) {
        for slot in 0..MAX_SESSIONS {
            if self.0[slot].ch != 0 && self.0[slot].dir == id {
                self.free(slot);
            }
        }
    }
}

/// All of a server but its inbox, so a handler can hold a request and still reply.
struct State {
    serve_end: u64,
    device: u64,
    /// The control channel, kept after `Ready` for `Meta::Unmount`; `0` once its peer has closed
    /// it. **While open it takes a wait slot**, so a session may use the last slot only once it
    /// has closed ([`State::session_capacity`]).
    control: u64,
    out: Outbox,
    /// Outbox for a `File::Forget`, its own since one is sent while a reply may be half-staged.
    forget: Outbox,
    content: [u8; MAX_FILE],
    listing: [u8; DIR_BODY_CAP],
    sessions: Sessions,
}

fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    exit(1)
}

fn close(h: u64) {
    // SAFETY: closing a handle this process holds.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
}

/// **What a server's panic handler does: say where, and exit.** A forwarded resolve has no
/// deadline, so a server spinning in its handler would hold every client waiting for ever; an
/// exit closes its endpoint and the kernel fails each of them `PeerClosed`.
pub fn panicked(info: &core::panic::PanicInfo) -> ! {
    match info.location() {
        Some(at) => {
            let mut digits = [0u8; 10];
            let mut n = at.line();
            let mut i = digits.len();
            loop {
                i -= 1;
                digits[i] = b'0' + (n % 10) as u8;
                n /= 10;
                if n == 0 {
                    break;
                }
            }
            say(&[b"fs-server: panicked at ", at.file().as_bytes(), b":", &digits[i..], b"; exiting\n"]);
        }
        None => say(&[b"fs-server: panicked; exiting\n"]),
    }
    exit(1)
}

/// One `kprint`, so other output cannot land mid-line; what does not fit 192 bytes is cut.
fn say(parts: &[&[u8]]) {
    let mut line = [0u8; 192];
    let mut n = 0;
    for p in parts {
        let take = p.len().min(line.len() - n);
        line[n..n + take].copy_from_slice(&p[..take]);
        n += take;
    }
    kprint(&line[..n]);
}

/// Wait for `h` alone, on buffers of its own: the loop may be walking a batch of results.
fn wait_one(h: u64) -> (i64, [u8; WAIT_RESULT_SIZE]) {
    let handles = [h];
    let mut results = [0u8; WAIT_RESULT_SIZE];
    // SAFETY: a one-entry handle array and room for one result, both live for the call.
    let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
    (waited, results)
}

fn recv_device(control: u64) -> Option<(u64, bool)> {
    if wait_one(control).0 != 1 {
        return None;
    }
    let mut inbox = Inbox([0; MSG_LEN]);
    // The device is the one handle the setup message brings; any more close as `sent` drops.
    let (payload, mut sent) = inbox.recv(control).ok()?;
    let device = sent.take(0)?;
    // The payload is the setup flags; an empty one is writable.
    let read_only = librsproto::meta::parse_fs_setup(payload) & librsproto::meta::FS_SETUP_READ_ONLY != 0;
    Some((device, read_only))
}

fn make_channel() -> Option<(u64, u64)> {
    let (mut e0, mut e1) = (0u64, 0u64);
    // SAFETY: `e0`/`e1` are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut e0) as u64, (&raw mut e1) as u64, 4, 0) };
    if cr != 0 {
        return None;
    }
    Some((e0, e1))
}

/// `content` in a fresh `MemoryObject` attenuated to `MAP_READ | TRANSFER`.
fn make_content_memobj(content: &[u8]) -> Option<u64> {
    let size = if content.is_empty() { PAGE } else { (content.len() as u64).div_ceil(PAGE) * PAGE };
    // SAFETY: register-only syscall.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, size, 0, 0, 0) };
    if mem < 0 {
        return None;
    }
    let mem = mem as u64;
    // SAFETY: register-only syscall.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        close(mem);
        return None;
    }
    // SAFETY: `addr` maps `size` bytes read-write, at least `content.len()`, and nothing else
    // refers to the new mapping.
    unsafe { core::slice::from_raw_parts_mut(addr as u64 as *mut u8, content.len()) }.copy_from_slice(content);
    // Unmap our own view: keeping the mapping would leak address space across requests.
    // SAFETY: register-only syscalls; `addr`/`size` are our just-made mapping.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
        syscall2(SYS_HANDLE_RESTRICT, mem, RIGHT_MAP_READ | RIGHT_TRANSFER);
    }
    Some(mem)
}

/// If the message is a `File::Touch`, stamp the file it names and return `true`; **no reply**.
/// Under Model A an in-place overwrite goes straight to the device, so this is the only way the
/// server learns of it. The time is [`now_secs`], never the writer's. A failure is dropped.
fn try_touch<V: Volume>(vol: &V, req: &[u8]) -> bool {
    let Ok(m) = librsproto::decode(req) else { return false };
    if m.op != OP_FILE_TOUCH {
        return false;
    }
    // A malformed body is still handled: the resolve path would answer a message with no pending
    // lookup.
    if let Some(id) = parse_touch_request(m.body) {
        let _ = vol.touch_file(id, now_secs());
    }
    true
}

/// **Free file `id`, which no name reaches any more, once the kernel says it may**: its
/// `File::Forget` is answered once no IRP of the file is in flight, so no write can land in a
/// block since handed to another file. If the kernel cannot be asked the blocks leak until a
/// filesystem check, since freeing them could corrupt another file.
fn forget_then_release<V: Volume>(vol: &V, outbox: &mut Outbox, serve_end: u64, id: u64) {
    if !forget(outbox, serve_end, id) {
        say(&[V::NAME, b": the kernel did not answer a forget; the file is kept\n"]);
        return;
    }
    if vol.release(id, now_secs()).is_err() {
        say(&[V::NAME, b": releasing an unlinked file failed\n"]);
    }
}

/// Send `File::Forget` and wait for the answer (the send's `PendingOperation`, hence
/// `SENDMODE_BLOCK`).
fn forget(outbox: &mut Outbox, serve_end: u64, id: u64) -> bool {
    let mut body = [0u8; 8];
    let Some(n) = forget_request(&mut body, id) else { return false };
    let Some(rs_len) = librsproto::encode(outbox.payload(), OP_FILE_FORGET, 0, 0, &body[..n], 0) else {
        return false;
    };
    let count = outbox.stage(rs_len, None);
    let sent = outbox.send(serve_end, count, SENDMODE_BLOCK);
    if sent < 0 {
        return false;
    }
    let (waited, results) = wait_one(sent as u64);
    let status = i32::from_le_bytes([results[8], results[9], results[10], results[11]]);
    close(sent as u64);
    waited == 1 && status == 0
}

impl State {
    fn session_capacity(&self) -> usize {
        if self.control != 0 { MAX_SESSIONS - 1 } else { MAX_SESSIONS }
    }

    /// [`apply_size`], then the forget and the release of an id a truncate ended. **The size a
    /// grow took the file from**, which the reply marks past ([`crate::serve::serve_grown`]).
    fn maybe_grow<V: Volume>(&mut self, vol: &V, req: &[u8]) -> Result<Option<u64>, FsError> {
        let resized = apply_size(vol, req, now_secs())?;
        if let Some(id) = resized.ended {
            forget_then_release(vol, &mut self.forget, self.serve_end, id);
        }
        Ok(resized.grown_from)
    }

    /// If the forwarded request is a `RESOLVE_RENAME`, perform it and reply `OBJECT_KIND_NONE`;
    /// `true` if handled. The kernel has established that both suffixes share this binding. It
    /// runs **before** the directory-session path, which infers "directory open" from the suffix
    /// naming a directory; renaming a directory names one too.
    fn try_resolve_rename<V: Volume>(&mut self, vol: &V, req: &[u8]) -> bool {
        let Ok(m) = librsproto::decode(req) else { return false };
        let Some(r) = parse_resolve_request(m.body).filter(|_| m.op == OP_NS_RESOLVE) else { return false };
        if r.flags & RESOLVE_RENAME == 0 {
            return false;
        }
        // Past here the request *is* a rename, so every exit replies: a fall-through would resolve
        // the source path as an ordinary open.
        let (dest, flags) = match parse_resolve_rename(m.body) {
            Some((dest, f))
                if !r.suffix.is_empty()
                    && !dest.is_empty()
                    && r.suffix.len() <= MAX_SUFFIX
                    && dest.len() <= MAX_SUFFIX =>
            {
                (dest, f)
            }
            _ => {
                self.reply_resolve_error(m.request_id, KError::InvalidArgument.as_i32(), b"");
                return true;
            }
        };
        let (mut src, mut dst) = ([0u8; MAX_SUFFIX + 1], [0u8; MAX_SUFFIX + 1]);
        src[0] = b'/';
        dst[0] = b'/';
        src[1..1 + r.suffix.len()].copy_from_slice(r.suffix);
        dst[1..1 + dest.len()].copy_from_slice(dest);
        let (src, dst) = (&src[..1 + r.suffix.len()], &dst[..1 + dest.len()]);
        match vol.rename_path(src, dst, flags & RENAME_REPLACE != 0, now_secs()) {
            Ok(replaced) => {
                if let Some(id) = replaced {
                    forget_then_release(vol, &mut self.forget, self.serve_end, id);
                }
                self.reply_resolve_none(m.request_id)
            }
            Err(e) => self.reply_resolve_error(m.request_id, kerror(e).as_i32(), reason(e)),
        }
        true
    }

    fn reply_resolve_none(&mut self, request_id: u64) {
        let mut body = [0u8; RESOLVE_REPLY_LEN];
        let _ = resolve_reply(&mut body, OBJECT_KIND_NONE, 0);
        let Some(n) = librsproto::encode(self.out.payload(), OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 0) else {
            return;
        };
        let count = self.out.stage(n, None);
        self.out.forwarded(self.serve_end, count, request_id, OP_NS_RESOLVE);
    }

    fn reply_resolve_error(&mut self, request_id: u64, kerror: i32, why: &[u8]) {
        let _ = self.out.error(self.serve_end, request_id, kerror, OP_NS_RESOLVE, why);
    }

    /// Reply `OBJECT_KIND_CHANNEL`, transferring `client_end`; `OBJECT_KIND_DIRECTORY` is never
    /// sent.
    fn reply_dir_handle(&mut self, request_id: u64, client_end: u64) -> bool {
        let mut body = [0u8; RESOLVE_REPLY_LEN];
        // A directory handle is a live channel to the server; `content_len` is unused.
        let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
        let Some(n) = librsproto::encode(self.out.payload(), OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1) else {
            return false;
        };
        let count = self.out.stage(n, Some(client_end));
        self.out.send(self.serve_end, count, SENDMODE_NOBLOCK) == 0
    }

    fn open_dir_session(&mut self, request_id: u64, dir: u64) {
        let Some(slot) = (0..self.session_capacity()).find(|&i| self.sessions.0[i].ch == 0) else {
            return self.reply_resolve_error(request_id, KError::WouldBlock.as_i32(), b"");
        };
        let Some((client_end, session_end)) = make_channel() else {
            return self.reply_resolve_error(request_id, KError::KernelError.as_i32(), b"");
        };
        // Bound before the reply, so a fast client request cannot arrive before the slot is live.
        self.sessions.0[slot] = Session { ch: session_end, dir };
        if !self.reply_dir_handle(request_id, client_end) {
            // The reply did not go: roll back the session, drop the client endpoint, and answer,
            // since the kernel took nothing and the lookup would wait for ever.
            self.sessions.free(slot);
            close(client_end);
            self.reply_resolve_error(request_id, KError::KernelError.as_i32(), b"");
        }
    }

    fn serve_session<V: Volume>(&mut self, inbox: &mut Inbox, vol: &V, ch: u64) {
        let Some(slot) = self.sessions.0.iter().position(|s| s.ch == ch) else {
            return; // already freed (e.g. an earlier result in this batch closed it)
        };
        loop {
            let (req, _sent) = match inbox.recv(ch) {
                Ok(got) => got,
                Err(rr) => {
                    if rr == KError::PeerClosed.as_i32() as i64 {
                        self.sessions.free(slot);
                    }
                    return; // WouldBlock (drained) or PeerClosed (freed)
                }
            };
            let dir = self.sessions.0[slot].dir;
            let (forget, sessions, serve_end) = (&mut self.forget, &mut self.sessions, self.serve_end);
            let release = |id| forget_then_release(vol, forget, serve_end, id);
            let freeing = |id| sessions.end_on(id);
            let reply = self.out.payload();
            let Some(n) = session(vol, dir, req, reply, &mut self.listing, now_secs(), release, freeing) else {
                continue; // a malformed frame: skipped
            };
            let count = self.out.stage(n, None);
            if self.out.send(ch, count, SENDMODE_NOBLOCK) != 0 {
                kprint(b"fs-server: a directory session's reply would not send; its client waits\n");
            }
        }
    }

    /// Serve the control channel. A closed peer is ordinary: the channel leaves the wait set.
    /// **The one request is `Meta::Unmount`**: record the filesystem cleanly unmounted, answer,
    /// and exit; the supervisor has written back what the kernel held. A read-only mount writes
    /// nothing.
    fn serve_control<V: Volume>(&mut self, inbox: &mut Inbox, vol: &V) {
        loop {
            let (req, _sent) = match inbox.recv(self.control) {
                Ok(got) => got,
                Err(rr) if rr == KError::PeerClosed.as_i32() as i64 => {
                    close(self.control);
                    self.control = 0;
                    return;
                }
                Err(_) => return, // drained
            };
            let Ok(m) = librsproto::decode(req) else { continue };
            if m.op != OP_UNMOUNT || m.flags & RS_FLAG_REPLY != 0 {
                continue;
            }
            let marked = if vol.read_only() { Ok(()) } else { vol.mark_clean() };
            let count = match marked {
                Ok(()) => librsproto::encode(self.out.payload(), OP_UNMOUNT, m.request_id, RS_FLAG_REPLY, &[], 0)
                    .map_or(0, |n| self.out.stage(n, None)),
                Err(e) => {
                    let why = b"the state could not be written";
                    let n = encode_error(self.out.payload(), m.request_id, kerror(e).as_i32(), OP_UNMOUNT, why);
                    self.out.stage(n, None)
                }
            };
            let _ = self.out.send(self.control, count, SENDMODE_NOBLOCK);
            // A read-only mount says it recorded nothing: "recorded clean" would claim otherwise.
            if marked.is_ok() && vol.read_only() {
                kprint(
                    b"fs-server: unmounted; a read-only mount wrote nothing, so the filesystem is as it was found\n",
                );
                exit(0);
            }
            if marked.is_ok() {
                // A filesystem found not clean, or found corrupt since, is left marked for a check.
                match vol.was_left_clean() {
                    Ok(true) => kprint(b"fs-server: unmounted, and the filesystem recorded clean\n"),
                    _ => kprint(b"fs-server: unmounted, and the filesystem left marked for a check\n"),
                }
                exit(0);
            }
            kprint(b"fs-server: unmounted, but the filesystem could not be recorded clean\n");
            exit(1);
        }
    }

    fn forwarded<V: Volume>(&mut self, vol: &V, req: &[u8]) {
        if try_touch(vol, req) || self.try_resolve_rename(vol, req) {
            return;
        }
        if let Some((request_id, dir)) = directory_open(vol, req) {
            return self.open_dir_session(request_id, dir);
        }
        let (request_id, op) = librsproto::decode(req).map_or((0, 0), |m| (m.request_id, m.op));
        // A grow comes first so the map covers the new size; its failure is the reply.
        let served = match self.maybe_grow(vol, req) {
            Err(e) => Served::Error {
                reply_len: encode_error(self.out.payload(), request_id, kerror(e).as_i32(), OP_NS_RESOLVE, reason(e)),
            },
            Ok(Some(was)) => serve_grown(vol, req, &mut self.content, self.out.payload(), was),
            Ok(None) => serve(vol, req, &mut self.content, self.out.payload()),
        };
        let handle = match served {
            Served::File { reply_len, content_len } => match make_content_memobj(&self.content[..content_len]) {
                Some(mem) => Ok((reply_len, mem)),
                // Resolved, but the object could not be made: an error reply with the request's op.
                None => Err(KError::OutOfMemory),
            },
            Served::LazyBlocks { reply_len } => {
                let rights = RIGHT_READ | RIGHT_WRITE | RIGHT_TRANSFER;
                // SAFETY: our device handle, holding DUPLICATE. The copy has WRITE only if ours does.
                match unsafe { syscall2(SYS_HANDLE_DUPLICATE, self.device, rights) } {
                    // The device cannot be shared: degrade to an error reply.
                    dup if dup < 0 => Err(KError::KernelError),
                    dup => Ok((reply_len, dup as u64)),
                }
            }
            Served::Error { reply_len } => {
                let count = self.out.stage(reply_len, None);
                return self.out.forwarded(self.serve_end, count, request_id, op);
            }
        };
        let count = match handle {
            Ok((reply_len, h)) => self.out.stage(reply_len, Some(h)),
            Err(e) => {
                let n = encode_error(self.out.payload(), request_id, e.as_i32(), op, b"");
                self.out.stage(n, None)
            }
        };
        self.out.forwarded(self.serve_end, count, request_id, op);
    }
}

impl Server {
    /// Never returns: an unmount or an unreachable endpoint ends the process.
    fn serve_loop<V: Volume>(&mut self, vol: &V) -> ! {
        let mut told = false;
        let mut handles = [0u64; MAX_WAIT_HANDLES];
        let mut results = [0u8; MAX_WAIT_HANDLES * WAIT_RESULT_SIZE];
        loop {
            if !told && vol.refusing_changes() {
                told = true;
                kprint(
                    b"fs-server: a change found the filesystem corrupt, or could not be undone; refusing every change until it is mounted again\n",
                );
            }
            // `count ≤ MAX_WAIT_HANDLES`: while the control channel is open, a session never takes
            // the last slot ([`State::session_capacity`]).
            let s = &self.s;
            let sessions = s.sessions.0.iter().map(|x| x.ch);
            let mut count = 0;
            for h in [s.serve_end, s.control].into_iter().chain(sessions).filter(|&h| h != 0) {
                handles[count] = h;
                count += 1;
            }
            // SAFETY: both arrays are this frame's, sized for `count` handles and their results.
            let waited = unsafe {
                syscall4(SYS_WAIT, handles.as_ptr() as u64, count as u64, results.as_mut_ptr() as u64, u64::MAX)
            };
            if waited < 1 {
                continue;
            }
            // **Sessions before `serve_end`, in two passes.** One batch routinely contains both a
            // closed session and a new resolve (a shell pipeline's stage N exits as N+1 starts).
            // Draining `serve_end` first would answer the new `RESOLVE_DIR_OPEN` while the
            // just-closed slots still read as occupied, a spurious `WouldBlock`.
            for pass in 0..2 {
                for j in 0..waited as usize {
                    let mut h = [0u8; 8];
                    h.copy_from_slice(&results[j * WAIT_RESULT_SIZE..j * WAIT_RESULT_SIZE + 8]);
                    let h = u64::from_le_bytes(h);
                    match (pass, h == self.s.serve_end) {
                        (0, false) if h == self.s.control => self.s.serve_control(&mut self.inbox, vol),
                        (0, false) => self.s.serve_session(&mut self.inbox, vol, h),
                        (1, true) => loop {
                            match self.inbox.recv(self.s.serve_end) {
                                Ok((req, _sent)) => self.s.forwarded(vol, req),
                                // **Nothing can reach this server any more**: its registration
                                // and every file it handed out have let go of the other end. That
                                // peer stays signalled, so going round again would spin a CPU for
                                // the rest of the boot. Exiting is what `device-mgr` does here.
                                Err(rr) if rr == KError::PeerClosed.as_i32() as i64 => {
                                    kprint(b"fs-server: nothing can reach this server any more; exiting\n");
                                    exit(0);
                                }
                                _ => break,
                            }
                        },
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Wall-clock seconds since the Unix epoch, or `0` ("unknown") with no anchored clock. **The
/// server reads this itself; a client never supplies it**, since a chosen timestamp would be
/// forgeable metadata.
fn now_secs() -> i64 {
    let mut ns: u64 = 0;
    // SAFETY: `ns` is a valid writable out-param; the syscall writes 8 bytes.
    let r = unsafe { syscall4(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut ns) as u64, 0, 0) };
    if r != 0 {
        return 0; // no anchored wall clock on this machine
    }
    (ns / 1_000_000_000) as i64
}

/// The block-device handle and read-only flag from the setup message on `control`, then the
/// server's disk over it, made by `disk`. A server that cannot do either says so and exits.
pub fn bootstrap<D>(control: u64, disk: fn(u64) -> Result<D, &'static [u8]>) -> (D, bool) {
    let (device, read_only) = match recv_device(control) {
        Some(d) => d,
        None => fail(b"fs-server: setup recv failed\n"),
    };
    match disk(device) {
        Ok(disk) => (disk, read_only),
        Err(why) => fail(why),
    }
}

/// Check `vol`, record a writable mount mounted, say `Ready` on `control`, and serve `vol` until
/// an unmount. `device` is the block-device handle a Model A reply hands the kernel a duplicate of.
pub fn run<V: Volume>(vol: &V, control: u64, device: u64) -> ! {
    let outbox = || Outbox { msg: [0; MSG_LEN], handles: [0; 8] };
    let mut server = Server {
        inbox: Inbox([0; MSG_LEN]),
        s: State {
            serve_end: 0,
            device,
            control: 0,
            out: outbox(),
            forget: outbox(),
            content: [0; MAX_FILE],
            listing: [0; DIR_BODY_CAP],
            sessions: Sessions([Session::default(); MAX_SESSIONS]),
        },
    };
    let out = &mut server.s.out;
    // A Ready over a device holding no filesystem would mount it anyway; a refusal replaces it.
    if let Err(why) = vol.check() {
        if !out.refusal(control, &why) {
            fail(b"fs-server: the device failed its check, and the refusal could not be sent\n");
        }
        exit(1);
    }

    // How it was left is reported, never refused (`TODO(fs-repair)`). A writable mount records
    // itself mounted **before** anything can change it, so it never looks clean while it can.
    if vol.was_left_clean() == Ok(false) {
        kprint(b"fs-server: the filesystem was not cleanly unmounted last time; serving it anyway\n");
    }
    if !vol.read_only() && vol.mark_mounted().is_err() {
        if !out.refusal(control, &V::state_unwritable()) {
            fail(b"fs-server: the state could not be written, and the refusal could not be sent\n");
        }
        exit(1);
    }

    let (kernel_end, serve_end) = match make_channel() {
        Some(p) => p,
        None => fail(b"fs-server: channel create failed\n"),
    };
    if !out.ready(control, V::NAME, kernel_end) {
        fail(b"fs-server: ready send failed\n");
    }
    server.s.serve_end = serve_end;
    server.s.control = control;

    let mode: &[u8] = if vol.read_only() { b", read-only)\n" } else { b", read-write)\n" };
    say(&[b"fs-server: ready (", V::KIND, mode]);
    server.serve_loop(vol)
}
