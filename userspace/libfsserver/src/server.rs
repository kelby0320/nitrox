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
//! The request→reply logic is the host-tested [`crate::serve`]; this module is the plumbing
//! between it and the kernel, **alloc-free**: one [`Server`], on [`serve_volume`]'s stack.

use crate::serve::{
    MAX_FILE, MAX_SUFFIX, Served, apply_size, directory_open, encode_error, encode_refusal, kerror, reason, serve,
    serve_grown, session,
};
use crate::{FsError, Refusal, Volume};
use libkern::{KError, MAX_WAIT_HANDLES, Rights};
use libos::{
    Carried, Channel, Deadline, Handle, MapRead, MapReadWrite, Memory, Msg, Only, ReadWrite, Ref, Resource, WaitSet,
    block_on, exit, log, time,
};
use librsproto::file::{forget_request, parse_touch_request};
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, OBJECT_KIND_NONE, RENAME_REPLACE, RESOLVE_RENAME, RESOLVE_REPLY_LEN, parse_resolve_rename,
    parse_resolve_request, resolve_reply,
};
use librsproto::{OP_FILE_FORGET, OP_FILE_TOUCH, OP_NS_RESOLVE, OP_UNMOUNT, RS_FLAG_REPLY};

#[cfg(test)]
mod tests;

const PAGE: u64 = 4096;

type Endpoint = Handle<Channel, Only>;
type EndpointRef<'a> = Ref<'a, Channel, Only>;
type Device<'a> = Ref<'a, Resource, ReadWrite>;

/// The most open directory sessions: the one `sys_wait` set holds `serve_end` plus every live
/// session, so it is the kernel's fan-out limit less one. A full table returns `WouldBlock`; a
/// client that opens a session and stalls pins a slot, which `TODO(server-fanout)` fixes.
const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 1;

/// Body scratch for a `File::ReadDir` reply: [`librsproto::RS_MAX_BODY`], what one message
/// carries. A page less the two headers is too much: a reply over 3,980 bytes is refused by the
/// kernel and the client waits for good.
const DIR_BODY_CAP: usize = librsproto::RS_MAX_BODY;

/// **One server**: made by [`serve_volume`] on its own stack, so its state is its own and nothing
/// else reaches it, and a request is borrowed from its inbox while the rest of it replies.
struct Server<'a> {
    inbox: Inbox,
    s: State<'a>,
}

/// The message last received. [`Inbox::recv`] lends its payload until the next receive, so no
/// receive can overwrite a request still being read.
struct Inbox(Msg);

impl Inbox {
    /// Receive one message on `ch`: its payload, and what came with it, which closes as it drops;
    /// no request here carries a handle. `WouldBlock` once drained, or `PeerClosed`.
    fn recv(&mut self, ch: EndpointRef<'_>) -> Result<(&[u8], Carried), KError> {
        let brought = ch.recv(&mut self.0).map_err(|e| e.code())?;
        Ok((self.0.payload(), brought))
    }
}

/// A message being sent, and the handles it moves.
struct Outbox {
    msg: Msg,
    handles: Carried,
}

impl Outbox {
    const fn new() -> Self {
        Outbox { msg: Msg::new(), handles: Carried::new() }
    }

    fn payload(&mut self) -> &mut [u8] {
        &mut self.msg.payload_mut()[..]
    }

    fn stage(&mut self, len: usize) {
        let staged = self.msg.set_len(len);
        assert!(staged.is_ok(), "an encoded payload fits the message it was written into");
    }

    fn attach<T, M>(&mut self, handle: Handle<T, M>) {
        assert!(self.handles.push(handle).is_ok(), "an outbox carries one handle at a time");
    }

    /// Send what is staged on `ch`: whether the kernel took it. **A handle it did not take is
    /// closed here**, not left for the next message.
    fn send(&mut self, ch: EndpointRef<'_>) -> bool {
        let sent = ch.send(&self.msg, &mut self.handles).is_ok();
        self.handles = Carried::new();
        sent
    }

    /// Stage and send an error reply on `ch`; whether it went.
    fn error(&mut self, ch: EndpointRef<'_>, request_id: u64, kerror: i32, op: u16, why: &[u8]) -> bool {
        let n = encode_error(self.payload(), request_id, kerror, op, why);
        self.stage(n);
        self.send(ch)
    }

    /// Send a forwarded request's reply. The kernel completes the waiting lookup inline (the peer
    /// is its forwarding endpoint), so `NoBlock` is correct. **One the kernel did not take is
    /// answered with an error**, and its handles, which stayed here, are closed: a forwarded
    /// request has no deadline, and its client would wait for ever.
    fn forwarded(&mut self, serve_end: EndpointRef<'_>, request_id: u64, op: u16) {
        if self.send(serve_end) {
            return;
        }
        let _ = self.error(serve_end, request_id, KError::KernelError.as_i32(), op, b"the reply could not be sent");
    }

    /// Send `Meta::Ready` on `control`, transferring `kernel_end`. NoBlock: the supervisor's
    /// control inbox starts empty, so the first message always has space.
    fn ready(&mut self, control: EndpointRef<'_>, name: &[u8], kernel_end: Endpoint) -> bool {
        let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 32];
        let Some(body_len) = librsproto::meta::ready(&mut body, name) else { return false };
        let Some(n) = librsproto::encode(self.payload(), librsproto::OP_READY, 0, 0, &body[..body_len], 1) else {
            return false;
        };
        self.stage(n);
        self.attach(kernel_end);
        self.send(control)
    }

    /// Send the refusal that replaces a Ready, with no handle; NoBlock as for the Ready.
    fn refusal(&mut self, control: EndpointRef<'_>, why: &impl Refusal) -> bool {
        let Some(n) = encode_refusal(self.payload(), why) else { return false };
        self.stage(n);
        self.send(control)
    }
}

/// One directory session: its endpoint and the directory it is bound to. A session addresses
/// entries by name, never path, so it can only touch that directory.
struct Session {
    ch: Endpoint,
    dir: u64,
}

struct Sessions([Option<Session>; MAX_SESSIONS]);

impl Sessions {
    fn new() -> Self {
        Sessions(core::array::from_fn(|_| None))
    }

    fn slot_of(&self, raw: u64) -> Option<usize> {
        self.0.iter().position(|s| s.as_ref().is_some_and(|s| s.ch.raw().0 == raw))
    }

    /// **End every session on directory `id`**, just removed: a session keeps a bare id, which the
    /// filesystem may give the next directory made, and the session would then reach into that one.
    fn end_on(&mut self, id: u64) {
        for slot in &mut self.0 {
            if slot.as_ref().is_some_and(|s| s.dir == id) {
                *slot = None;
            }
        }
    }
}

/// All of a server but its inbox, so a handler can hold a request and still reply.
struct State<'a> {
    serve_end: Endpoint,
    device: Device<'a>,
    /// The control channel, kept after `Ready` for `Meta::Unmount`; `None` once its peer has
    /// closed it. **While open it takes a wait slot**, so a session may use the last slot only
    /// once it has closed ([`State::session_capacity`]).
    control: Option<Endpoint>,
    out: Outbox,
    /// Outbox for a `File::Forget`, its own since one is sent while a reply may be half-staged.
    forget: Outbox,
    content: [u8; MAX_FILE],
    listing: [u8; DIR_BODY_CAP],
    sessions: Sessions,
}

fn fail(msg: &[u8]) -> ! {
    log::write(msg);
    exit(1)
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

/// One log write, so other output cannot land mid-line; what does not fit 192 bytes is cut.
fn say(parts: &[&[u8]]) {
    let mut line = [0u8; 192];
    let mut n = 0;
    for p in parts {
        let take = p.len().min(line.len() - n);
        line[n..n + take].copy_from_slice(&p[..take]);
        n += take;
    }
    log::write(&line[..n]);
}

/// The block-device handle and the read-only flag from the setup message on `control`: `None` if
/// it never came whole.
pub fn receive_setup<'a>(control: impl Into<EndpointRef<'a>>) -> Option<(Handle<Resource, ReadWrite>, bool)> {
    let control = control.into();
    let mut set = WaitSet::<1>::new();
    set.add(control).ok()?;
    if set.wait(Deadline::NEVER).ok()?.len() != 1 {
        return None;
    }
    let mut inbox = Inbox(Msg::new());
    // The device is the one handle the setup message brings; any more close as `brought` drops.
    let (payload, mut brought) = inbox.recv(control).ok()?;
    let device = brought.take(0)?;
    // The payload is the setup flags; an empty one is writable.
    let read_only = librsproto::meta::parse_fs_setup(payload) & librsproto::meta::FS_SETUP_READ_ONLY != 0;
    Some((device, read_only))
}

/// `content` in a fresh memory object attenuated to `MAP_READ | TRANSFER`.
fn make_content_memobj(content: &[u8]) -> Option<Handle<Memory, MapRead>> {
    let size = if content.is_empty() { PAGE } else { (content.len() as u64).div_ceil(PAGE) * PAGE } as usize;
    let mem = Handle::<Memory, MapReadWrite>::create(size).ok()?;
    // The view is a temporary: it unmaps before the object goes, or address space leaks.
    mem.map(size).ok()?.write_from(0, content).ok()?;
    mem.duplicate(Rights::MAP_READ | Rights::TRANSFER).ok()?.into_read_only().ok()
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
fn forget_then_release<V: Volume>(vol: &V, outbox: &mut Outbox, serve_end: EndpointRef<'_>, id: u64) {
    if !forget(outbox, serve_end, id) {
        say(&[V::NAME, b": the kernel did not answer a forget; the file is kept\n"]);
        return;
    }
    if vol.release(id, now_secs()).is_err() {
        say(&[V::NAME, b": releasing an unlinked file failed\n"]);
    }
}

/// Send `File::Forget` and wait for the answer: the send is held, and completes when the kernel
/// has answered.
fn forget(outbox: &mut Outbox, serve_end: EndpointRef<'_>, id: u64) -> bool {
    let mut body = [0u8; 8];
    let Some(n) = forget_request(&mut body, id) else { return false };
    let Some(rs_len) = librsproto::encode(outbox.payload(), OP_FILE_FORGET, 0, 0, &body[..n], 0) else {
        return false;
    };
    outbox.stage(rs_len);
    let Ok(sent) = serve_end.send_held(&outbox.msg, &mut outbox.handles) else { return false };
    matches!(block_on(sent), Ok(answer) if answer.status == 0)
}

impl State<'_> {
    fn session_capacity(&self) -> usize {
        if self.control.is_some() { MAX_SESSIONS - 1 } else { MAX_SESSIONS }
    }

    /// [`apply_size`], then the forget and the release of an id a truncate ended. **The size a
    /// grow took the file from**, which the reply marks past ([`crate::serve::serve_grown`]).
    fn maybe_grow<V: Volume>(&mut self, vol: &V, req: &[u8]) -> Result<Option<u64>, FsError> {
        let resized = apply_size(vol, req, now_secs())?;
        if let Some(id) = resized.ended {
            forget_then_release(vol, &mut self.forget, self.serve_end.as_ref(), id);
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
                    forget_then_release(vol, &mut self.forget, self.serve_end.as_ref(), id);
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
        self.out.stage(n);
        self.out.forwarded(self.serve_end.as_ref(), request_id, OP_NS_RESOLVE);
    }

    fn reply_resolve_error(&mut self, request_id: u64, kerror: i32, why: &[u8]) {
        let _ = self.out.error(self.serve_end.as_ref(), request_id, kerror, OP_NS_RESOLVE, why);
    }

    /// Reply `OBJECT_KIND_CHANNEL`, transferring `client_end`; `OBJECT_KIND_DIRECTORY` is never
    /// sent.
    fn reply_dir_handle(&mut self, request_id: u64, client_end: Endpoint) -> bool {
        let mut body = [0u8; RESOLVE_REPLY_LEN];
        // A directory handle is a live channel to the server; `content_len` is unused.
        let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
        let Some(n) = librsproto::encode(self.out.payload(), OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1) else {
            return false;
        };
        self.out.stage(n);
        self.out.attach(client_end);
        self.out.send(self.serve_end.as_ref())
    }

    fn open_dir_session(&mut self, request_id: u64, dir: u64) {
        let Some(slot) = (0..self.session_capacity()).find(|&i| self.sessions.0[i].is_none()) else {
            return self.reply_resolve_error(request_id, KError::WouldBlock.as_i32(), b"");
        };
        let Ok((client_end, session_end)) = Endpoint::pair(4) else {
            return self.reply_resolve_error(request_id, KError::KernelError.as_i32(), b"");
        };
        // Bound before the reply, so a fast client request cannot arrive before the slot is live.
        self.sessions.0[slot] = Some(Session { ch: session_end, dir });
        if !self.reply_dir_handle(request_id, client_end) {
            // The reply did not go, and the client endpoint with it: roll back the session and
            // answer, since the kernel took nothing and the lookup would wait for ever.
            self.sessions.0[slot] = None;
            self.reply_resolve_error(request_id, KError::KernelError.as_i32(), b"");
        }
    }

    /// Serve the requests waiting on the session whose endpoint is the raw handle `raw`.
    fn serve_session<V: Volume>(&mut self, inbox: &mut Inbox, vol: &V, raw: u64) {
        let Some(slot) = self.sessions.slot_of(raw) else {
            return; // already freed (e.g. an earlier result in this batch closed it)
        };
        // Out of its slot while it serves, so a removal it asks for can end the others without
        // freeing the endpoint it is replying on.
        let Some(held) = self.sessions.0[slot].take() else { return };
        if self.drain_session(inbox, vol, &held) {
            self.sessions.0[slot] = Some(held);
        }
    }

    /// Serve `held` until its ring is empty; `false` once its peer has closed.
    fn drain_session<V: Volume>(&mut self, inbox: &mut Inbox, vol: &V, held: &Session) -> bool {
        loop {
            let (req, _brought) = match inbox.recv(held.ch.as_ref()) {
                Ok(got) => got,
                Err(KError::PeerClosed) => return false,
                Err(_) => return true, // WouldBlock: drained
            };
            let (forget, sessions, serve_end) = (&mut self.forget, &mut self.sessions, self.serve_end.as_ref());
            let release = |id| forget_then_release(vol, forget, serve_end, id);
            let freeing = |id| sessions.end_on(id);
            let reply = self.out.payload();
            let Some(n) = session(vol, held.dir, req, reply, &mut self.listing, now_secs(), release, freeing) else {
                continue; // a malformed frame: skipped
            };
            self.out.stage(n);
            if !self.out.send(held.ch.as_ref()) {
                log::write(b"fs-server: a directory session's reply would not send; its client waits\n");
            }
        }
    }

    /// Serve the control channel. A closed peer is ordinary: the channel leaves the wait set.
    /// **The one request is `Meta::Unmount`**: record the filesystem cleanly unmounted, answer,
    /// and exit; the supervisor has written back what the kernel held. A read-only mount writes
    /// nothing.
    fn serve_control<V: Volume>(&mut self, inbox: &mut Inbox, vol: &V) {
        loop {
            let Some(control) = self.control.as_ref() else { return };
            let (req, _brought) = match inbox.recv(control.as_ref()) {
                Ok(got) => got,
                Err(KError::PeerClosed) => {
                    self.control = None;
                    return;
                }
                Err(_) => return, // drained
            };
            let Ok(m) = librsproto::decode(req) else { continue };
            if m.op != OP_UNMOUNT || m.flags & RS_FLAG_REPLY != 0 {
                continue;
            }
            let marked = if vol.read_only() { Ok(()) } else { vol.mark_clean() };
            let answer = match marked {
                Ok(()) => librsproto::encode(self.out.payload(), OP_UNMOUNT, m.request_id, RS_FLAG_REPLY, &[], 0),
                Err(e) => {
                    let why = b"the state could not be written";
                    Some(encode_error(self.out.payload(), m.request_id, kerror(e).as_i32(), OP_UNMOUNT, why))
                }
            };
            if let Some(n) = answer {
                self.out.stage(n);
                let _ = self.out.send(control.as_ref());
            }
            // A read-only mount says it recorded nothing: "recorded clean" would claim otherwise.
            if marked.is_ok() && vol.read_only() {
                log::write(
                    b"fs-server: unmounted; a read-only mount wrote nothing, so the filesystem is as it was found\n",
                );
                exit(0);
            }
            if marked.is_ok() {
                // A filesystem found not clean, or found corrupt since, is left marked for a check.
                match vol.was_left_clean() {
                    Ok(true) => log::write(b"fs-server: unmounted, and the filesystem recorded clean\n"),
                    _ => log::write(b"fs-server: unmounted, and the filesystem left marked for a check\n"),
                }
                exit(0);
            }
            log::write(b"fs-server: unmounted, but the filesystem could not be recorded clean\n");
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
        let reply = match served {
            Served::File { reply_len, content_len } => match make_content_memobj(&self.content[..content_len]) {
                Some(mem) => {
                    self.out.attach(mem);
                    Ok(reply_len)
                }
                // Resolved, but the object could not be made: an error reply with the request's op.
                None => Err(KError::OutOfMemory),
            },
            Served::LazyBlocks { reply_len } => {
                // The copy has WRITE only if ours does; a device that cannot be shared degrades to
                // an error reply.
                match self.device.duplicate(Rights::READ | Rights::WRITE | Rights::TRANSFER) {
                    Ok(dup) => {
                        self.out.attach(dup);
                        Ok(reply_len)
                    }
                    Err(_) => Err(KError::KernelError),
                }
            }
            Served::Error { reply_len } => Ok(reply_len),
        };
        let reply_len = reply.unwrap_or_else(|e| encode_error(self.out.payload(), request_id, e.as_i32(), op, b""));
        self.out.stage(reply_len);
        self.out.forwarded(self.serve_end.as_ref(), request_id, op);
    }

    fn watched(&self) -> (WaitSet<'_, MAX_WAIT_HANDLES>, [u64; MAX_WAIT_HANDLES]) {
        let mut set = WaitSet::new();
        let mut raw = [0; MAX_WAIT_HANDLES];
        let sessions = self.sessions.0.iter().flatten().map(|s| &s.ch);
        for ch in core::iter::once(&self.serve_end).chain(self.control.as_ref()).chain(sessions) {
            let Ok(at) = set.add(ch) else { panic!("the sessions and the two channels fit one wait") };
            raw[at] = ch.raw().0;
        }
        (set, raw)
    }

    fn is_control(&self, raw: u64) -> bool {
        self.control.as_ref().is_some_and(|c| c.raw().0 == raw)
    }
}

impl Server<'_> {
    /// Never returns: an unmount or an unreachable endpoint ends the process.
    fn serve_loop<V: Volume>(&mut self, vol: &V) -> ! {
        let mut told = false;
        loop {
            if !told && vol.refusing_changes() {
                told = true;
                log::write(
                    b"fs-server: a change found the filesystem corrupt, or could not be undone; refusing every change until it is mounted again\n",
                );
            }
            let (set, raw) = self.s.watched();
            let Ok(signalled) = set.wait(Deadline::NEVER) else { continue };
            let serve_end = self.s.serve_end.raw().0;
            // **Sessions before `serve_end`, in two passes.** One batch routinely contains both a
            // closed session and a new resolve (a shell pipeline's stage N exits as N+1 starts).
            // Draining `serve_end` first would answer the new `RESOLVE_DIR_OPEN` while the
            // just-closed slots still read as occupied, a spurious `WouldBlock`.
            for pass in 0..2 {
                for (at, _) in signalled.iter() {
                    let h = raw[at];
                    match (pass, h == serve_end) {
                        (0, false) if self.s.is_control(h) => self.s.serve_control(&mut self.inbox, vol),
                        (0, false) => self.s.serve_session(&mut self.inbox, vol, h),
                        (1, true) => loop {
                            match self.inbox.recv(self.s.serve_end.as_ref()) {
                                Ok((req, _brought)) => self.s.forwarded(vol, req),
                                // **Nothing can reach this server any more**: its registration
                                // and every file it handed out have let go of the other end. That
                                // peer stays signalled, so going round again would spin a CPU for
                                // the rest of the boot. Exiting is what `device-mgr` does here.
                                Err(KError::PeerClosed) => {
                                    log::write(b"fs-server: nothing can reach this server any more; exiting\n");
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
    time::wall().map_or(0, |ns| (ns / 1_000_000_000) as i64)
}

/// What makes a server's disk over the device its setup message brought, or says why it cannot.
pub type MakeDisk<D> = fn(Handle<Resource, ReadWrite>) -> Result<D, &'static [u8]>;

/// The block-device handle and read-only flag from the setup message on `control`, then the
/// server's disk over it, made by `disk`. A server that cannot do either says so and exits.
pub fn open_disk<'a, D>(control: impl Into<EndpointRef<'a>>, disk: MakeDisk<D>) -> (D, bool) {
    let Some((device, read_only)) = receive_setup(control) else { fail(b"fs-server: setup recv failed\n") };
    match disk(device) {
        Ok(disk) => (disk, read_only),
        Err(why) => fail(why),
    }
}

/// Check `vol`, record a writable mount mounted, say `Ready` on `control`, and serve `vol` until
/// an unmount. `device` is the block-device handle a Model A reply hands the kernel a duplicate of.
pub fn serve_volume<V: Volume>(vol: &V, control: Endpoint, device: Device<'_>) -> ! {
    let mut out = Outbox::new();
    // A Ready over a device holding no filesystem would mount it anyway; a refusal replaces it.
    if let Err(why) = vol.check() {
        if !out.refusal(control.as_ref(), &why) {
            fail(b"fs-server: the device failed its check, and the refusal could not be sent\n");
        }
        exit(1);
    }

    // How it was left is reported, never refused (`TODO(fs-repair)`). A writable mount records
    // itself mounted **before** anything can change it, so it never looks clean while it can.
    if vol.was_left_clean() == Ok(false) {
        log::write(b"fs-server: the filesystem was not cleanly unmounted last time; serving it anyway\n");
    }
    if !vol.read_only() && vol.mark_mounted().is_err() {
        if !out.refusal(control.as_ref(), &V::state_unwritable()) {
            fail(b"fs-server: the state could not be written, and the refusal could not be sent\n");
        }
        exit(1);
    }

    let Ok((kernel_end, serve_end)) = Endpoint::pair(4) else { fail(b"fs-server: channel create failed\n") };
    if !out.ready(control.as_ref(), V::NAME, kernel_end) {
        fail(b"fs-server: ready send failed\n");
    }
    let mode: &[u8] = if vol.read_only() { b", read-only)\n" } else { b", read-write)\n" };
    say(&[b"fs-server: ready (", V::KIND, mode]);
    let mut server = Server {
        inbox: Inbox(Msg::new()),
        s: State {
            serve_end,
            device,
            control: Some(control),
            out,
            forget: Outbox::new(),
            content: [0; MAX_FILE],
            listing: [0; DIR_BODY_CAP],
            sessions: Sessions::new(),
        },
    };
    server.serve_loop(vol)
}
