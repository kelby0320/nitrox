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
//! plumbing, **alloc-free**: fixed `.bss` buffers, one set per process.

use crate::serve::{MAX_FILE, MAX_SUFFIX, Served, encode_error, encode_refusal, kerror, reason, serve, serve_grown};
use crate::{DirEntry, FsError, Refusal, Volume};
use libkern::*;
use librsproto::file::{
    DirReplyWriter, forget_request, parse_name_request, parse_read_dir_request, parse_rename_request,
    parse_touch_request,
};
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, OBJECT_KIND_NONE, RENAME_REPLACE, RESOLVE_CREATE, RESOLVE_GROW, RESOLVE_RENAME,
    RESOLVE_REPLY_LEN, RESOLVE_TRUNCATE, parse_resolve_grow_size, parse_resolve_rename, parse_resolve_request,
    resolve_reply,
};
use librsproto::{
    OP_FILE_FORGET, OP_FILE_MKDIR, OP_FILE_READ_DIR, OP_FILE_RENAME, OP_FILE_RMDIR, OP_FILE_TOUCH, OP_FILE_UNLINK,
    OP_NS_RESOLVE, OP_UNMOUNT, RS_FLAG_REPLY,
};

const PAGE: u64 = 4096;
const MSG_LEN: usize = 4096;
const PAYLOAD_OFF: usize = 24;

static mut RECV_MSG: [u8; 4096] = [0; 4096];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; 4096] = [0; 4096];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
/// Outbox for a `File::Forget`, its own since one is sent while a reply may be half-staged.
static mut FORGET_MSG: [u8; 4096] = [0; 4096];
static mut CONTENT: [u8; MAX_FILE] = [0; MAX_FILE];
static mut WAIT_HANDLES: [u64; MAX_WAIT_HANDLES] = [0; MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; MAX_WAIT_HANDLES * WAIT_RESULT_SIZE] = [0; MAX_WAIT_HANDLES * WAIT_RESULT_SIZE];

/// The most open directory sessions: the one `sys_wait` set holds `serve_end` plus every live
/// session, so it is the kernel's fan-out limit less one. A full table returns `WouldBlock`; a
/// client that opens a session and stalls pins a slot, which `TODO(server-fanout)` fixes.
const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 1;
/// Per-session state: the kept endpoint (`0` = free slot) and the directory the session is bound
/// to. A session addresses entries by name, never path, so it can only touch that directory.
static mut SESSION_CH: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];
static mut SESSION_DIR: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];
/// The control channel, kept after `Ready` for `Meta::Unmount`; `0` once its peer has closed it.
/// **While open it takes a wait slot**, so a session may use the last slot only once it has
/// closed ([`session_capacity`]).
static mut CONTROL: u64 = 0;

fn session_capacity() -> usize {
    // SAFETY: a single-threaded read.
    if unsafe { CONTROL } != 0 { MAX_SESSIONS - 1 } else { MAX_SESSIONS }
}

/// Body scratch for a `File::ReadDir` reply: [`librsproto::RS_MAX_BODY`], what one message
/// carries. A page less the two headers is too much: a reply over 3,980 bytes is refused by the
/// kernel and the client waits for good.
const DIR_BODY_CAP: usize = librsproto::RS_MAX_BODY;
static mut DIR_BODY: [u8; DIR_BODY_CAP] = [0; DIR_BODY_CAP];

fn fail(msg: &[u8]) -> ! {
    kprint(msg);
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

fn recv_device(control: u64) -> Option<(u64, bool)> {
    // SAFETY: one waiter on the control endpoint.
    let waited = unsafe {
        WAIT_HANDLES[0] = control;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    if waited != 1 {
        return None;
    }
    // SAFETY: valid recv out-params; on success the kernel installs the handle.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            control,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    // SAFETY: on success the kernel wrote the count + handle values.
    let count = unsafe { (&raw const RECV_COUNT).read() };
    if rr != 0 || count < 1 {
        return None;
    }
    // The payload is the setup flags; an empty one is writable.
    // SAFETY: `RECV_MSG` holds the setup message; the slice is bounded by its payload length.
    let flags = unsafe {
        let len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        librsproto::meta::parse_fs_setup(core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            len.min(MSG_LEN - PAYLOAD_OFF),
        ))
    };
    let read_only = flags & librsproto::meta::FS_SETUP_READ_ONLY != 0;
    // SAFETY: the kernel wrote the transferred handle.
    Some((unsafe { (&raw const RECV_HANDLES[0]).read() }, read_only))
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

fn send_ready(control: u64, name: &[u8], kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 32];
    let body_len = match librsproto::meta::ready(&mut body, name) {
        Some(n) => n,
        None => return false,
    };
    // SAFETY: REPLY_MSG is a valid 4 KiB buffer; the rsproto message goes in the
    // IPC payload region (offset 24).
    let rs_len = unsafe {
        match librsproto::encode(&mut REPLY_MSG[24..], librsproto::OP_READY, 0, 0, &body[..body_len], 1) {
            Some(n) => n,
            None => return false,
        }
    };
    // SAFETY: stamp the IpcMsg header (payload_len @4, handle_count @8) + the transferred-handle
    // slot.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = kernel_end;
    }
    // SAFETY: valid endpoint + message + 1-handle transfer. NoBlock: the supervisor's control
    // inbox starts empty, so the first Ready always has space.
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

fn send_refusal(control: u64, why: &impl Refusal) -> bool {
    // SAFETY: REPLY_MSG is a valid 4 KiB buffer; the rsproto message goes in the IPC payload
    // region (offset 24), and no handle rides with it.
    let rs_len = unsafe {
        match encode_refusal(&mut REPLY_MSG[24..], why) {
            Some(n) => n,
            None => return false,
        }
    };
    // SAFETY: stamp the IpcMsg header (payload_len @4, handle_count @8).
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
    }
    // SAFETY: valid endpoint + message, no handles. NoBlock: the supervisor's control inbox is
    // empty until this server's first message, as for the Ready this replaces.
    let sr = unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            control,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        )
    };
    sr == 0
}

/// `CONTENT[..len]` as a fresh `MemoryObject` attenuated to `MAP_READ | TRANSFER`.
fn make_content_memobj(len: usize) -> Option<u64> {
    let size = if len == 0 { PAGE } else { (len as u64).div_ceil(PAGE) * PAGE };
    // SAFETY: register-only syscall.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, size, 0, 0, 0) };
    if mem < 0 {
        return None;
    }
    let mem = mem as u64;
    // SAFETY: register-only syscall.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return None;
    }
    // SAFETY: `addr` maps `size ≥ len` bytes R/W; `CONTENT[..len]` is initialised.
    unsafe {
        let dst = core::slice::from_raw_parts_mut(addr as u64 as *mut u8, len);
        dst.copy_from_slice(&CONTENT[..len]);
    }
    // Unmap our own view: keeping the mapping would leak address space across requests.
    // SAFETY: register-only syscalls; `addr`/`size` are our just-made mapping.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
        syscall2(SYS_HANDLE_RESTRICT, mem, RIGHT_MAP_READ | RIGHT_TRANSFER);
    }
    Some(mem)
}

fn stage_reply(payload_len: usize, handle: Option<u64>) -> usize {
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid writable buffers.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(payload_len as u32).to_le_bytes());
        let count = if let Some(h) = handle {
            REPLY_HANDLES[0] = h;
            1
        } else {
            0
        };
        REPLY_MSG[8] = count as u8;
        count
    }
}

/// The kernel completes the waiting lookup inline (the peer is its forwarding endpoint), so
/// `NoBlock` is correct and the reply is consumed regardless.
fn send_reply(serve_end: u64, count: usize) {
    // SAFETY: valid endpoint + message + `count` transferred handles.
    unsafe {
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            count as u64,
            SENDMODE_NOBLOCK,
        );
    }
}

/// Apply a resolve's `RESOLVE_GROW` or `RESOLVE_TRUNCATE` (after `RESOLVE_CREATE`), best-effort:
/// an error leaves `serve` mapping the file at its current size. A truncate that ends a file's
/// id has it forgotten and released first. **The size a grow took the file from**, which the
/// reply marks past ([`crate::serve::serve_grown`]).
fn maybe_grow<V: Volume>(vol: &V, req: &[u8], serve_end: u64) -> Result<Option<u64>, FsError> {
    let Ok(m) = librsproto::decode(req) else {
        return Ok(None);
    };
    if m.op != OP_NS_RESOLVE {
        return Ok(None);
    }
    let Some(r) = parse_resolve_request(m.body) else {
        return Ok(None);
    };
    if r.flags & (RESOLVE_GROW | RESOLVE_TRUNCATE) == 0 || r.suffix.len() > MAX_SUFFIX {
        return Ok(None);
    }
    let Some(new_size) = parse_resolve_grow_size(m.body) else {
        return Ok(None);
    };
    // **A read-only mount's refusal is the one failure reported**: it would otherwise answer a
    // create or a grow with a file that never changed.
    let mut refused = false;
    let mut note = |r: Result<(), FsError>| refused |= r == Err(FsError::ReadOnly);
    let mut path = [0u8; MAX_SUFFIX + 1];
    path[0] = b'/';
    path[1..1 + r.suffix.len()].copy_from_slice(r.suffix);
    let path = &path[..1 + r.suffix.len()];

    // Create-on-resolve is idempotent, so a re-resolve of a created file is harmless.
    if r.flags & RESOLVE_CREATE != 0
        && let Some(slash) = path.iter().rposition(|&b| b == b'/')
    {
        let parent = if slash == 0 { &b"/"[..] } else { &path[..slash] };
        let name = &path[slash + 1..];
        note(vol.create_file(parent, name, now_secs()));
    }

    let mut grown = None;
    if r.flags & RESOLVE_TRUNCATE != 0 {
        // Never combined with GROW: the two move the allocator in opposite directions.
        match vol.truncate_file(path, new_size as usize, now_secs()) {
            Ok(Some(id)) => forget_then_release(vol, serve_end, id),
            Ok(None) => {}
            Err(e) => note(Err(e)),
        }
    } else {
        match vol.grow_file(path, new_size as usize, now_secs()) {
            Ok(was) if was < new_size as u64 => grown = Some(was),
            Ok(_) => {}
            Err(e) => note(Err(e)),
        }
    }
    if refused { Err(FsError::ReadOnly) } else { Ok(grown) }
}

/// Receive one message on `h` into the `RECV_*` statics: `0` = arrived; `-11` (`WouldBlock`) =
/// drained; `-13` (`PeerClosed`) = the peer closed.
fn recv_on(h: u64) -> i64 {
    // SAFETY: valid recv out-params; the server is single-threaded, one message at a time.
    unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            h,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    }
}

/// # Safety
/// Single-threaded: the slice borrows `RECV_MSG`, which the caller must not receive into while
/// it holds it.
unsafe fn received() -> &'static [u8] {
    // SAFETY: `RECV_MSG` holds a just-received message; the slice is bounded by the recorded
    // payload length, itself clamped to the buffer.
    unsafe {
        let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        )
    }
}

/// The `(request_id, directory id)` of a resolve whose suffix names a **directory**: resolve
/// flags are not plumbed from userspace, so the kind is inferred from the path.
fn try_resolve_directory<V: Volume>(vol: &V) -> Option<(u64, u64)> {
    let mut suffix = [0u8; MAX_SUFFIX];
    // SAFETY: single-threaded; nothing receives while `req` is held.
    let (request_id, suffix_len) = unsafe {
        let req = received();
        let m = librsproto::decode(req).ok()?;
        if m.op != OP_NS_RESOLVE {
            return None;
        }
        let r = parse_resolve_request(m.body)?;
        let n = r.suffix.len().min(MAX_SUFFIX);
        suffix[..n].copy_from_slice(&r.suffix[..n]);
        (m.request_id, n)
    };
    let dir = vol.resolve_dir(&suffix[..suffix_len]).ok()?;
    Some((request_id, dir))
}

/// If the message is a `File::Touch`, stamp the file it names and return `true`; **no reply**.
/// Under Model A an in-place overwrite goes straight to the device, so this is the only way the
/// server learns of it. The time is [`now_secs`], never the writer's. A failure is dropped.
fn try_touch<V: Volume>(vol: &V) -> bool {
    // SAFETY: single-threaded; nothing receives while `req` is held.
    let id = unsafe {
        let req = received();
        let Ok(m) = librsproto::decode(req) else {
            return false;
        };
        if m.op != OP_FILE_TOUCH {
            return false;
        }
        // Every exit past here returns `true`, even on a malformed body: the resolve path would
        // answer a message with no pending lookup.
        match parse_touch_request(m.body) {
            Some(id) => id,
            None => return true,
        }
    };
    let _ = vol.touch_file(id, now_secs());
    true
}

/// **Free file `id`, which no name reaches any more, once the kernel says it may**: its
/// `File::Forget` is answered once no IRP of the file is in flight, so no write can land in a
/// block since handed to another file. If the kernel cannot be asked the blocks leak until a
/// filesystem check, since freeing them could corrupt another file.
fn forget_then_release<V: Volume>(vol: &V, serve_end: u64, id: u64) {
    if !forget(serve_end, id) {
        say(&[V::NAME, b": the kernel did not answer a forget; the file is kept\n"]);
        return;
    }
    if vol.release(id, now_secs()).is_err() {
        say(&[V::NAME, b": releasing an unlinked file failed\n"]);
    }
}

/// Send `File::Forget` and wait for the answer (the send's `PendingOperation`, hence
/// `SENDMODE_BLOCK`). **Waits on buffers of its own**: `serve_loop` is still walking the batch
/// of results `WAIT_RESULTS` holds.
fn forget(serve_end: u64, id: u64) -> bool {
    let mut body = [0u8; 8];
    let Some(n) = forget_request(&mut body, id) else {
        return false;
    };
    // SAFETY: FORGET_MSG is a valid buffer, written only here; the rsproto message goes at offset
    // PAYLOAD_OFF.
    let sent = unsafe {
        let Some(rs_len) = librsproto::encode(&mut FORGET_MSG[PAYLOAD_OFF..], OP_FILE_FORGET, 0, 0, &body[..n], 0)
        else {
            return false;
        };
        FORGET_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        FORGET_MSG[8] = 0;
        syscall5(SYS_CHANNEL_SEND, serve_end, (&raw const FORGET_MSG) as u64, 0, 0, SENDMODE_BLOCK)
    };
    if sent < 0 {
        return false;
    }
    let handles = [sent as u64];
    let mut results = [0u8; WAIT_RESULT_SIZE];
    // SAFETY: `handles` and `results` are valid local buffers for one waiter.
    let waited = unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, 1, results.as_mut_ptr() as u64, u64::MAX) };
    let status = i32::from_le_bytes([results[8], results[9], results[10], results[11]]);
    // SAFETY: closing the PendingOperation the send returned.
    unsafe { syscall1(SYS_HANDLE_CLOSE, sent as u64) };
    waited == 1 && status == 0
}

/// If the forwarded request is a `RESOLVE_RENAME`, perform it and reply `OBJECT_KIND_NONE`;
/// `true` if handled. The kernel has established that both suffixes share this binding. It runs
/// **before** the directory-session path, which infers "directory open" from the suffix naming a
/// directory; renaming a directory names one too.
fn try_resolve_rename<V: Volume>(vol: &V, serve_end: u64) -> bool {
    let mut src = [0u8; MAX_SUFFIX + 1];
    let mut dst = [0u8; MAX_SUFFIX + 1];
    src[0] = b'/';
    dst[0] = b'/';
    // SAFETY: single-threaded; nothing receives while `req` is held. `src`/`dst` are local, and
    // the reply helpers touch only the disjoint `REPLY_*` statics.
    let parsed: Option<(u64, usize, usize, u16)> = unsafe {
        let req = received();
        let Ok(m) = librsproto::decode(req) else {
            return false;
        };
        if m.op != OP_NS_RESOLVE {
            return false;
        }
        let Some(r) = parse_resolve_request(m.body) else {
            return false;
        };
        if r.flags & RESOLVE_RENAME == 0 {
            return false;
        }
        // Past here the request *is* a rename, so every exit replies: a fall-through would
        // resolve the source path as an ordinary open.
        match parse_resolve_rename(m.body) {
            Some((dest, f))
                if !r.suffix.is_empty()
                    && !dest.is_empty()
                    && r.suffix.len() <= MAX_SUFFIX
                    && dest.len() <= MAX_SUFFIX =>
            {
                src[1..1 + r.suffix.len()].copy_from_slice(r.suffix);
                dst[1..1 + dest.len()].copy_from_slice(dest);
                Some((m.request_id, r.suffix.len(), dest.len(), f))
            }
            _ => {
                reply_resolve_error(serve_end, m.request_id, KError::InvalidArgument.as_i32());
                None
            }
        }
    };
    let Some((request_id, src_len, dst_len, flags)) = parsed else {
        return true; // the error reply went out above
    };

    let done = vol.rename_path(&src[..1 + src_len], &dst[..1 + dst_len], flags & RENAME_REPLACE != 0, now_secs());
    match done {
        Ok(replaced) => {
            if let Some(id) = replaced {
                forget_then_release(vol, serve_end, id);
            }
            reply_resolve_none(serve_end, request_id)
        }
        Err(e) => reply_resolve_error_why(serve_end, request_id, kerror(e).as_i32(), reason(e)),
    }
    true
}

fn reply_resolve_none(serve_end: u64, request_id: u64) {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_NONE, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at offset PAYLOAD_OFF.
    let count = unsafe {
        let rs_len =
            match librsproto::encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 0)
            {
                Some(n) => n,
                None => return,
            };
        stage_reply(rs_len, None)
    };
    send_reply(serve_end, count);
}

fn free_session_at(slot: usize) {
    // SAFETY: `slot < MAX_SESSIONS`; closing our own endpoint handle.
    unsafe {
        let ch = SESSION_CH[slot];
        SESSION_CH[slot] = 0;
        SESSION_DIR[slot] = 0;
        if ch != 0 {
            syscall1(SYS_HANDLE_CLOSE, ch);
        }
    }
}

fn reply_resolve_error(serve_end: u64, request_id: u64, kerror: i32) {
    reply_resolve_error_why(serve_end, request_id, kerror, b"");
}

fn reply_resolve_error_why(serve_end: u64, request_id: u64, kerror: i32, why: &[u8]) {
    // SAFETY: disjoint reply region.
    let elen = unsafe {
        let reply =
            core::slice::from_raw_parts_mut(((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF), MSG_LEN - PAYLOAD_OFF);
        encode_error(reply, request_id, kerror, OP_NS_RESOLVE, why)
    };
    let count = stage_reply(elen, None);
    send_reply(serve_end, count);
}

/// Reply `OBJECT_KIND_CHANNEL`, transferring `client_end`; `OBJECT_KIND_DIRECTORY` is never sent.
fn reply_dir_handle(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    // A directory handle is a live channel to the server; `content_len` is unused.
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at offset PAYLOAD_OFF,
    // and the transferred handle in REPLY_HANDLES[0].
    unsafe {
        let rs_len =
            match librsproto::encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1)
            {
                Some(n) => n,
                None => return false,
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

fn open_dir_session(serve_end: u64, request_id: u64, dir: u64) {
    // SAFETY: single-threaded scan of the session table.
    let slot = unsafe { (0..session_capacity()).find(|&i| SESSION_CH[i] == 0) };
    let Some(slot) = slot else {
        reply_resolve_error(serve_end, request_id, KError::WouldBlock.as_i32());
        return;
    };

    let (client_end, session_end) = match make_channel() {
        Some(p) => p,
        None => {
            reply_resolve_error(serve_end, request_id, KError::KernelError.as_i32());
            return;
        }
    };
    // SAFETY: `slot` is free; bind the session before replying so a fast client request
    // cannot arrive before the slot is live.
    unsafe {
        SESSION_CH[slot] = session_end;
        SESSION_DIR[slot] = dir;
    }
    if !reply_dir_handle(serve_end, request_id, client_end) {
        // The reply did not go: roll back the session and drop the client endpoint.
        free_session_at(slot);
        // SAFETY: closing our own not-yet-transferred handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, client_end) };
    }
}

fn serve_session<V: Volume>(vol: &V, session_ch: u64, serve_end: u64) {
    // SAFETY: single-threaded scan.
    let Some(slot) = (unsafe { (0..MAX_SESSIONS).find(|&i| SESSION_CH[i] == session_ch) }) else {
        return; // already freed (e.g. an earlier result in this batch closed it)
    };
    loop {
        let rr = recv_on(session_ch);
        if rr != 0 {
            if rr == KError::PeerClosed.as_i32() as i64 {
                free_session_at(slot);
            }
            return; // WouldBlock (drained) or PeerClosed (freed)
        }
        // Copied out (two 255-byte names and prefixes fit) so no borrow of the statics interleaves.
        let mut body_buf = [0u8; 600];
        // SAFETY: single-threaded; nothing receives while `req` is held.
        let (request_id, op, body_len) = unsafe {
            match librsproto::decode(received()) {
                Ok(m) => {
                    let n = m.body.len().min(body_buf.len());
                    body_buf[..n].copy_from_slice(&m.body[..n]);
                    (m.request_id, m.op, n)
                }
                Err(_) => continue, // malformed frame: skip
            }
        };
        let body = &body_buf[..body_len];
        // SAFETY: `slot` still valid here (only freed on PeerClosed, handled above).
        let dir = unsafe { SESSION_DIR[slot] };

        match op {
            OP_FILE_READ_DIR => {
                let cursor = parse_read_dir_request(body).map(|r| r.cursor).unwrap_or(0);
                match build_readdir_reply(vol, dir, cursor) {
                    Some(bl) => send_session_reply(session_ch, request_id, bl),
                    None => reply_session_error(session_ch, request_id, OP_FILE_READ_DIR, KError::KernelError.as_i32()),
                }
            }
            // Each names an entry in the bound directory, so a handle can never mutate outside it.
            OP_FILE_MKDIR | OP_FILE_UNLINK | OP_FILE_RMDIR | OP_FILE_TOUCH => {
                let r = match parse_name_request(body) {
                    Some(name) => match op {
                        OP_FILE_MKDIR => vol.mkdir_at(dir, name, now_secs()),
                        // The last name's removal frees the file only once the kernel has
                        // forgotten it, and before the client hears it is gone.
                        OP_FILE_UNLINK => vol.unlink_at(dir, name, now_secs()).map(|orphan| {
                            if let Some(id) = orphan {
                                forget_then_release(vol, serve_end, id);
                            }
                        }),
                        OP_FILE_RMDIR => vol.rmdir_at(dir, name, now_secs()),
                        _ => vol.touch_at(dir, name, now_secs()),
                    },
                    None => Err(FsError::Unsupported),
                };
                reply_session_status(session_ch, request_id, op, r);
            }
            OP_FILE_RENAME => {
                let r = match parse_rename_request(body) {
                    Some((old, new)) => vol.rename_at(dir, old, new, now_secs()),
                    None => Err(FsError::Unsupported),
                };
                reply_session_status(session_ch, request_id, op, r);
            }
            _ => reply_session_error(session_ch, request_id, op, KError::Unsupported.as_i32()),
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

fn reply_session_status(session_ch: u64, request_id: u64, op: u16, r: Result<(), FsError>) {
    match r {
        Ok(()) => {
            // SAFETY: REPLY_MSG is a valid buffer; an empty-body reply, no handles.
            unsafe {
                let rs_len =
                    match librsproto::encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, RS_FLAG_REPLY, &[], 0) {
                        Some(n) => n,
                        None => return,
                    };
                REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
                REPLY_MSG[8] = 0;
                syscall5(
                    SYS_CHANNEL_SEND,
                    session_ch,
                    (&raw const REPLY_MSG) as u64,
                    (&raw const REPLY_HANDLES) as u64,
                    0,
                    SENDMODE_NOBLOCK,
                );
            }
        }
        Err(e) => reply_session_error_why(session_ch, request_id, op, kerror(e).as_i32(), reason(e)),
    }
}

fn build_readdir_reply<V: Volume>(vol: &V, dir: u64, cursor: u64) -> Option<usize> {
    // SAFETY: DIR_BODY is a disjoint static; the writer holds it for this call only.
    let body = unsafe { core::slice::from_raw_parts_mut((&raw mut DIR_BODY) as *mut u8, DIR_BODY_CAP) };
    let mut w = DirReplyWriter::new(body)?;
    let next = vol.read_dir(dir, cursor, |e: &DirEntry| w.push(e.id, e.kind, e.mode, e.size, e.mtime, e.name));
    let next_cursor = next.ok()?;
    Some(w.finish(next_cursor))
}

fn send_session_reply(session_ch: u64, request_id: u64, body_len: usize) {
    // SAFETY: DIR_BODY (read) and REPLY_MSG (write) are disjoint statics.
    unsafe {
        let body = core::slice::from_raw_parts((&raw const DIR_BODY) as *const u8, body_len);
        // A reply too long to send is answered with an error, and a failed send is said: the
        // client is waiting for exactly this message.
        let Some(rs_len) =
            librsproto::encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_FILE_READ_DIR, request_id, RS_FLAG_REPLY, body, 0)
        else {
            return reply_session_error(session_ch, request_id, OP_FILE_READ_DIR, KError::KernelError.as_i32());
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        let sent = syscall5(
            SYS_CHANNEL_SEND,
            session_ch,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
        if sent != 0 {
            kprint(b"fs-server: a directory listing's reply would not send; its client waits\n");
        }
    }
}

fn reply_session_error(session_ch: u64, request_id: u64, op: u16, kerror: i32) {
    reply_session_error_why(session_ch, request_id, op, kerror, b"");
}

fn reply_session_error_why(session_ch: u64, request_id: u64, op: u16, kerror: i32, why: &[u8]) {
    // SAFETY: disjoint reply region.
    let elen = unsafe {
        let reply =
            core::slice::from_raw_parts_mut(((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF), MSG_LEN - PAYLOAD_OFF);
        encode_error(reply, request_id, kerror, op, why)
    };
    let count = stage_reply(elen, None);
    send_reply(session_ch, count);
}

/// Never returns: an unmount or an unreachable endpoint ends the process.
fn serve_loop<V: Volume>(vol: &V, serve_end: u64, device: u64) -> ! {
    loop {
        // `count ≤ MAX_WAIT_HANDLES`: while the control channel is open, a session never takes
        // the last slot ([`session_capacity`]).
        // SAFETY: single-threaded build of the wait array.
        let count = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1;
            if CONTROL != 0 {
                WAIT_HANDLES[n] = CONTROL;
                n += 1;
            }
            #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
            for i in 0..MAX_SESSIONS {
                if SESSION_CH[i] != 0 {
                    WAIT_HANDLES[n] = SESSION_CH[i];
                    n += 1;
                }
            }
            n
        };
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers sized for `count`.
        let waited = unsafe {
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, count as u64, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        if waited < 1 {
            continue;
        }
        // **Sessions before `serve_end`, in two passes.** One batch routinely contains both a
        // closed session and a new resolve (a shell pipeline's stage N exits as N+1 starts).
        // Draining `serve_end` first would answer the new `RESOLVE_DIR_OPEN` while the
        // just-closed slots still read as occupied, a spurious `WouldBlock`.
        for pass in 0..2 {
            for j in 0..(waited as usize) {
                // SAFETY: `waited` records were written; `off + 8` stays inside WAIT_RESULTS.
                let h = unsafe {
                    let off = j * WAIT_RESULT_SIZE;
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
                match (pass, h == serve_end) {
                    // SAFETY: a single-threaded read.
                    (0, false) if h == unsafe { CONTROL } => serve_control(vol),
                    (0, false) => serve_session(vol, h, serve_end),
                    (1, true) => loop {
                        match recv_on(serve_end) {
                            0 => handle_forwarded_resolve(vol, serve_end, device),
                            // **Nothing can reach this server any more**: its registration and
                            // every file it handed out have let go of the other end. That peer
                            // stays signalled, so going round again would spin a CPU for the
                            // rest of the boot. Exiting is what `device-mgr` does here.
                            rr if rr == KError::PeerClosed.as_i32() as i64 => {
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

/// Serve the control channel. A closed peer is ordinary: the channel leaves the wait set. **The
/// one request is `Meta::Unmount`**: record the filesystem cleanly unmounted, answer, and exit;
/// the supervisor has written back what the kernel held. A read-only mount writes nothing.
fn serve_control<V: Volume>(vol: &V) {
    // SAFETY: a single-threaded read.
    let control = unsafe { CONTROL };
    loop {
        let rr = recv_on(control);
        if rr == KError::PeerClosed.as_i32() as i64 {
            // SAFETY: closing our own end, and forgetting it.
            unsafe {
                syscall1(SYS_HANDLE_CLOSE, control);
                CONTROL = 0;
            }
            return;
        }
        if rr != 0 {
            return; // drained
        }
        // SAFETY: single-threaded; nothing receives while `req` is held.
        let (op, flags, request_id) = unsafe {
            match librsproto::decode(received()) {
                Ok(m) => (m.op, m.flags, m.request_id),
                Err(_) => continue,
            }
        };
        if op != OP_UNMOUNT || flags & RS_FLAG_REPLY != 0 {
            continue;
        }
        let marked = if vol.read_only() { Ok(()) } else { vol.mark_clean() };
        let count = match marked {
            Ok(()) => {
                // SAFETY: REPLY_MSG is a valid buffer; an empty-body reply.
                let rs_len = unsafe {
                    librsproto::encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_UNMOUNT, request_id, RS_FLAG_REPLY, &[], 0)
                };
                rs_len.map_or(0, |n| stage_reply(n, None))
            }
            Err(e) => {
                // SAFETY: disjoint reply region.
                let elen = unsafe {
                    let reply = core::slice::from_raw_parts_mut(
                        ((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF),
                        MSG_LEN - PAYLOAD_OFF,
                    );
                    encode_error(reply, request_id, kerror(e).as_i32(), OP_UNMOUNT, b"the state could not be written")
                };
                stage_reply(elen, None)
            }
        };
        send_reply(control, count);
        // A read-only mount says it recorded nothing: "recorded clean" would claim otherwise.
        if marked.is_ok() && vol.read_only() {
            kprint(b"fs-server: unmounted; a read-only mount wrote nothing, so the filesystem is as it was found\n");
            exit(0);
        }
        if marked.is_ok() {
            kprint(b"fs-server: unmounted, and the filesystem recorded clean\n");
            exit(0);
        }
        kprint(b"fs-server: unmounted, but the filesystem could not be recorded clean\n");
        exit(1);
    }
}

fn handle_forwarded_resolve<V: Volume>(vol: &V, serve_end: u64, device: u64) {
    if try_touch(vol) {
        return;
    }

    if try_resolve_rename(vol, serve_end) {
        return;
    }

    if let Some((request_id, dir)) = try_resolve_directory(vol) {
        open_dir_session(serve_end, request_id, dir);
        return;
    }

    let request_id;
    let served_op;
    // SAFETY: `received` is bounded to the payload region; the request, content and reply slices
    // address disjoint statics, so no aliasing `&`/`&mut` is formed
    let served = unsafe {
        let req = received();
        request_id = librsproto::decode(req).map(|m| m.request_id).unwrap_or(0);
        let content = core::slice::from_raw_parts_mut((&raw mut CONTENT) as *mut u8, MAX_FILE);
        let reply =
            core::slice::from_raw_parts_mut(((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF), MSG_LEN - PAYLOAD_OFF);
        let op = librsproto::decode(req).map(|m| m.op).unwrap_or(0);
        served_op = op;
        // A grow comes first so the map covers the new size; a failure falls through.
        match maybe_grow(vol, req, serve_end) {
            Err(e) => Served::Error {
                reply_len: encode_error(reply, request_id, kerror(e).as_i32(), OP_NS_RESOLVE, reason(e)),
            },
            Ok(Some(was)) => serve_grown(vol, req, content, reply, was),
            Ok(None) => serve(vol, req, content, reply),
        }
    };

    let count = match served {
        Served::File { reply_len, content_len } => match make_content_memobj(content_len) {
            Some(mem) => stage_reply(reply_len, Some(mem)),
            // Resolved, but the object could not be made: an error reply with the request's op.
            None => {
                // SAFETY: disjoint static; reply region as above.
                let elen = unsafe {
                    let reply = core::slice::from_raw_parts_mut(
                        ((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF),
                        MSG_LEN - PAYLOAD_OFF,
                    );
                    encode_error(reply, request_id, KError::OutOfMemory.as_i32(), served_op, b"")
                };
                stage_reply(elen, None)
            }
        },
        Served::LazyBlocks { reply_len } => {
            // SAFETY: our device handle, holding DUPLICATE. The copy has WRITE only if ours does.
            let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, device, RIGHT_READ | RIGHT_WRITE | RIGHT_TRANSFER) };
            if dup < 0 {
                // The device cannot be shared: degrade to an error reply.
                // SAFETY: disjoint static; reply region as above.
                let elen = unsafe {
                    let reply = core::slice::from_raw_parts_mut(
                        ((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF),
                        MSG_LEN - PAYLOAD_OFF,
                    );
                    encode_error(reply, request_id, KError::KernelError.as_i32(), served_op, b"")
                };
                stage_reply(elen, None)
            } else {
                stage_reply(reply_len, Some(dup as u64))
            }
        }
        Served::Error { reply_len } => stage_reply(reply_len, None),
    };
    send_reply(serve_end, count);
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
    // A Ready over a device holding no filesystem would mount it anyway; a refusal replaces it.
    if let Err(why) = vol.check() {
        if !send_refusal(control, &why) {
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
        if !send_refusal(control, &V::state_unwritable()) {
            fail(b"fs-server: the state could not be written, and the refusal could not be sent\n");
        }
        exit(1);
    }

    let (kernel_end, serve_end) = match make_channel() {
        Some(p) => p,
        None => fail(b"fs-server: channel create failed\n"),
    };

    if !send_ready(control, V::NAME, kernel_end) {
        fail(b"fs-server: ready send failed\n");
    }
    // SAFETY: single-threaded; set before the serve loop reads it.
    unsafe { CONTROL = control };

    let mode: &[u8] = if vol.read_only() { b", read-only)\n" } else { b", read-write)\n" };
    say(&[b"fs-server: ready (", V::KIND, mode]);
    serve_loop(vol, serve_end, device)
}
