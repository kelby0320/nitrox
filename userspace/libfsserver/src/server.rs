//! **The server loop** (slice 7 Part 4; a library since Phase 6 Part E.1): the bootstrap, then
//! forwarded requests, directory sessions and the control channel, for any [`Volume`].
//!
//! ## Bootstrap (driven by a supervisor: `init`, or the storage service)
//!
//! 1. The supervisor spawns the server, installing **one** handle — a **control channel**
//!    endpoint — which the kernel delivers in `rdx` (`_start`'s third argument).
//! 2. It sends a **setup message** on that channel transferring the **block-device** handle, and
//!    whether to serve it read-only; [`bootstrap`] receives it and makes the server's disk over
//!    it — ext4's 4 KiB [`Disk`](crate::disk::Disk), or FAT's
//!    [`SectorDisk`](crate::disk::SectorDisk).
//! 3. The server **checks the device holds a filesystem it can serve** ([`Volume::check`]). If
//!    not, it sends a refusal saying why in place of the Ready ([`send_refusal`]) and exits; the
//!    supervisor prints the reason.
//! 4. A writable mount records itself mounted, then the server creates a **forwarding channel**
//!    pair, keeps the serving end, and sends `Meta::Ready` on the control channel **transferring
//!    the other (kernel) end** ([`send_ready`]); the supervisor binds that endpoint.
//! 5. The server loops ([`serve_loop`]): forwarded resolves, directory sessions, and the control
//!    channel's `Meta::Unmount`.
//!
//! The request→reply logic lives in the host-tested [`crate::serve`]; this module is the
//! syscall plumbing. **Alloc-free** — fixed `.bss` buffers, one set per process, which is one
//! server.
//!
//! A server never holds `BIND_NAMESPACE` (its supervisor binds its endpoint) and receives only the
//! handles it needs at spawn — see `docs/rationale/why-supervisor-registration.md`.

use crate::serve::{MAX_FILE, MAX_SUFFIX, Served, encode_error, encode_refusal, kerror, reason, serve};
use crate::{DirEntry, FsError, Refusal, Volume};
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
use libkern::*;

/// One page; the memory-object granularity.
const PAGE: u64 = 4096;
/// IPC message size (the `RECV_MSG`/`REPLY_MSG` buffers); payload starts at 24.
const MSG_LEN: usize = 4096;
const PAYLOAD_OFF: usize = 24;

// --- fixed server buffers (.bss; the server is single-threaded) -------------
/// Inbox for a received message (setup, then each forwarded `Resolve`).
static mut RECV_MSG: [u8; 4096] = [0; 4096];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
/// Outbox for a reply (and the bootstrap Ready); the transferred handle in `[0]`.
static mut REPLY_MSG: [u8; 4096] = [0; 4096];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
/// Outbox for a `File::Forget` — its own, since one is sent while a reply may be half-staged.
static mut FORGET_MSG: [u8; 4096] = [0; 4096];
/// Scratch for the file content (the 64 KiB read-model cap).
static mut CONTENT: [u8; MAX_FILE] = [0; MAX_FILE];
/// `sys_wait` scratch: the forwarding endpoint plus every open directory session. One slot
/// is `serve_end`, so up to [`MAX_SESSIONS`] directory sessions can be waited on at once.
/// Each result is a 24-byte `IoResult`.
static mut WAIT_HANDLES: [u64; MAX_WAIT_HANDLES] = [0; MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; MAX_WAIT_HANDLES * WAIT_RESULT_SIZE] = [0; MAX_WAIT_HANDLES * WAIT_RESULT_SIZE];

/// The most open directory-handle sessions the server serves concurrently.
///
/// **Derived, not chosen**: the server waits on one `sys_wait` set holding `serve_end` plus
/// every live session, so the ceiling is the kernel's fan-out limit less that one slot.
/// Writing it this way rather than restating the number means raising
/// [`MAX_WAIT_HANDLES`] moves this with it — the two were separately-written `7`s until
/// Slice C3, which is how they would have drifted apart.
///
/// Sessions are short-lived — a client opens a directory, reads it, and closes — so this
/// bounds concurrent *in-flight* listings, not total clients. A full table returns
/// `WouldBlock` on `RESOLVE_DIR_OPEN`; a client that opens a session and then stalls still
/// pins a slot for as long as it lives, which no cap fixes and `TODO(server-fanout)` does.
const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 1;
/// Per-session state: the kept (server) endpoint (`0` = free slot) and the directory id
/// the session is bound to. A session addresses entries by name, never path, so it can
/// only ever touch this directory (structural confinement).
static mut SESSION_CH: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];
static mut SESSION_DIR: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];
/// The control channel, kept after `Ready` for `Meta::Unmount` (administration Part C.3); `0`
/// once its peer has closed it.
///
/// **While it is open it takes a wait slot**, so a session may use the last slot only once it
/// has closed ([`session_capacity`]).
static mut CONTROL: u64 = 0;

/// How many directory sessions may be open now: one fewer while the control channel holds a
/// wait slot of its own.
fn session_capacity() -> usize {
    // SAFETY: a single-threaded read.
    if unsafe { CONTROL } != 0 { MAX_SESSIONS - 1 } else { MAX_SESSIONS }
}

/// Body scratch for a `File::ReadDir` reply (packed entries), before the rsproto header is
/// prepended into `REPLY_MSG`. Bounded to one IPC payload minus the two headers.
const DIR_BODY_CAP: usize = MSG_LEN - PAYLOAD_OFF - librsproto::RS_HEADER_LEN;
static mut DIR_BODY: [u8; DIR_BODY_CAP] = [0; DIR_BODY_CAP];

/// Log `msg` and exit non-zero — the bootstrap failure path. (A server is not a
/// critical-path process like init/eshell, so exiting on a bootstrap fault is the
/// correct disposition; a supervisor observes the exit.)
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    exit(1)
}

/// **What a server's panic handler does: say where, and exit** (PR #365 review, finding 1). A
/// server that spun in its handler, as both did, left every resolve forwarded to it waiting for
/// ever, since a forwarded resolve has no deadline. One that exits closes its endpoint, so the
/// kernel fails each of them `PeerClosed`, and its supervisor sees it go.
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

/// **Print one line made of `parts`**, in one `kprint`, so another process's output cannot land
/// in the middle of it. What does not fit 192 bytes is cut.
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

/// Receive the setup message on the control channel and return the transferred
/// block-device handle (its `handles[0]`) and whether to serve it read-only. `None` on any
/// failure.
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
    // The payload is the setup flags (administration Part C.3); an empty one is writable.
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

/// Create a connected channel pair (depth 4), returning `(kernel_end, serve_end)`.
fn make_channel() -> Option<(u64, u64)> {
    let (mut e0, mut e1) = (0u64, 0u64);
    // SAFETY: `e0`/`e1` are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut e0) as u64, (&raw mut e1) as u64, 4, 0) };
    if cr != 0 {
        return None;
    }
    Some((e0, e1))
}

/// Send `Meta::Ready` on the control channel, naming the server and transferring `kernel_end`
/// (the endpoint the supervisor binds). `false` on any failure.
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
    // SAFETY: stamp the IpcMsg header (payload_len @4, handle_count @8) + the
    // transferred-handle slot.
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

/// Send a refusal in place of `Meta::Ready`: why the device cannot be served, and no handle
/// ([`encode_refusal`]). `false` on any failure.
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

/// Materialise the file content (already in `CONTENT[..len]`) as a fresh read-only
/// `MemoryObject` to transfer: create it, map R/W, copy the bytes in, unmap, then
/// attenuate to `MAP_READ | TRANSFER` (read-only content the client may map +
/// receive). Returns the handle, or `None` on any failure.
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
    // Unmap our own view (the object is transferred whole; keeping the mapping
    // would leak address space across requests) and attenuate to read-only content.
    // SAFETY: register-only syscalls; `addr`/`size` are our just-made mapping.
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
        syscall2(SYS_HANDLE_RESTRICT, mem, RIGHT_MAP_READ | RIGHT_TRANSFER);
    }
    Some(mem)
}

/// Stamp the reply IpcMsg header — `payload_len` (@4), `handle_count` (@8) — and
/// stage the transferred handle (`handles[0]`).
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

/// Send the staged reply (`count` transferred handles) on `serve_end`. The kernel
/// completes the waiting lookup inline (the peer is its forwarding endpoint), so
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

/// Apply a resolve's size change, if it carries one: `RESOLVE_GROW` grows the named file
/// to the requested size and `RESOLVE_TRUNCATE` shrinks it; if it also carries `RESOLVE_CREATE`,
/// create the file first. Best-effort — any parse / create / grow error is ignored and the
/// subsequent `serve` maps the file at its current size (the reply reflects that, so a failed
/// create surfaces as `NotFound`). A truncate that ends a file's id has it forgotten and released
/// before the reply.
fn maybe_grow<V: Volume>(vol: &V, req: &[u8], serve_end: u64) -> Result<(), FsError> {
    let Ok(m) = librsproto::decode(req) else {
        return Ok(());
    };
    if m.op != OP_NS_RESOLVE {
        return Ok(());
    }
    let Some(r) = parse_resolve_request(m.body) else {
        return Ok(());
    };
    if r.flags & (RESOLVE_GROW | RESOLVE_TRUNCATE) == 0 || r.suffix.len() > MAX_SUFFIX {
        return Ok(());
    }
    let Some(new_size) = parse_resolve_grow_size(m.body) else {
        return Ok(());
    };
    // **A read-only mount's refusal is the one failure reported** (administration Part C.3).
    // Every other one falls through, as it always has, and the reply shows the size the file
    // has; this one would otherwise answer a create or a grow with a file that never changed.
    let mut refused = false;
    let mut note = |r: Result<(), FsError>| refused |= r == Err(FsError::ReadOnly);
    let mut path = [0u8; MAX_SUFFIX + 1];
    path[0] = b'/';
    path[1..1 + r.suffix.len()].copy_from_slice(r.suffix);
    let path = &path[..1 + r.suffix.len()];

    // Create-on-resolve: split the absolute path into parent dir + leaf name at the last
    // `/`, then create the file in the parent. Idempotent (an existing file is left as it is),
    // so a re-resolve of an already-created file is harmless.
    if r.flags & RESOLVE_CREATE != 0
        && let Some(slash) = path.iter().rposition(|&b| b == b'/')
    {
        let parent = if slash == 0 { &b"/"[..] } else { &path[..slash] };
        let name = &path[slash + 1..];
        note(vol.create_file(parent, name, now_secs()));
    }

    if r.flags & RESOLVE_TRUNCATE != 0 {
        // Shrink: free what is past the new end. Never combined with GROW — the two
        // move the allocator in opposite directions, so the flags are exclusive.
        match vol.truncate_file(path, new_size as usize, now_secs()) {
            Ok(Some(id)) => forget_then_release(vol, serve_end, id),
            Ok(None) => {}
            Err(e) => note(Err(e)),
        }
    } else {
        note(vol.grow_file(path, new_size as usize, now_secs()));
    }
    if refused { Err(FsError::ReadOnly) } else { Ok(()) }
}

/// Receive one message on `h` into the `RECV_*` statics. Returns the syscall result:
/// `0` = a message arrived; `-11` (`WouldBlock`) = the ring is drained; `-13`
/// (`PeerClosed`) = the peer closed (a directory session's client is gone).
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

/// The request in `RECV_MSG`, bounded by its recorded payload length.
///
/// # Safety
/// Single-threaded: the slice borrows `RECV_MSG`, which the caller must not receive into while
/// it holds it.
unsafe fn received() -> &'static [u8] {
    // SAFETY: `RECV_MSG` holds a just-received message; the slice is bounded by the recorded
    // payload length, itself clamped to the buffer.
    unsafe {
        let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        core::slice::from_raw_parts(((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF), payload_len.min(MSG_LEN - PAYLOAD_OFF))
    }
}

/// If the forwarded request in `RECV_MSG` is a `Namespace::Resolve` whose suffix names a
/// **directory**, return its `(request_id, directory id)`. A directory path resolves to
/// a directory session (below); a file path (or a miss) returns `None` and takes the file
/// path. Resolve flags are not plumbed from userspace, so the kind is inferred from what
/// the path actually names — the same way the logging service decides `OBJECT_KIND_CHANNEL`
/// by what it resolves. (The directory walk is repeated by the file path for a non-match;
/// folding it into a single `serve` pass — a `Served::Directory` — is a later refinement.)
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

/// If the message in `RECV_MSG` is a `File::Touch`, stamp the file it names `mtime` and
/// return `true`. **No reply** — the kernel sends this fire-and-forget.
///
/// This is the server learning about a write it structurally cannot see. Under Model A the
/// kernel owns the file-data path, so an in-place, same-length overwrite goes from the page
/// cache straight to the device: no resolve, no IPC, nothing here to stamp. Without this
/// the file's `mtime` would keep reporting its last *size* change — a file edited ten times
/// in place would look untouched since it was created.
///
/// The timestamp is [`now_secs`], our own clock read. The wire carries only the file's id —
/// which this server's block-file reply gave the kernel — so a writer cannot pick the time its
/// write appears to have happened.
///
/// A failure is dropped silently: the data is already durable, and there is no caller
/// waiting on this. The cost of losing one is a stale timestamp — exactly the behaviour
/// this replaces.
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
        // From here it *is* a touch, so every exit returns `true` (consumed) even on a
        // malformed body — falling through would hand it to the resolve path, which would
        // try to answer a message that has no pending lookup behind it.
        match parse_touch_request(m.body) {
            Some(id) => id,
            None => return true,
        }
    };
    let _ = vol.touch_file(id, now_secs());
    true
}

/// **Free file `id`, which no name reaches any more, once the kernel says it may**
/// (administration Part C.1b). The kernel may hold the file's pages and be writing them to its
/// blocks. A `File::Forget` stops that, and its answer comes once no IRP of the file is in
/// flight; only then are the blocks freed, so no write the kernel issued can land in a block
/// this server has since handed to another file.
///
/// If the kernel cannot be asked, the file is left as it is, unreachable. That leaks its blocks
/// until a filesystem check reclaims them, where freeing them now could corrupt another file.
fn forget_then_release<V: Volume>(vol: &V, serve_end: u64, id: u64) {
    if !forget(serve_end, id) {
        say(&[V::NAME, b": the kernel did not answer a forget; the file is kept\n"]);
        return;
    }
    if vol.release(id, now_secs()).is_err() {
        say(&[V::NAME, b": releasing an unlinked file failed\n"]);
    }
}

/// Send `File::Forget` for `id` on the forwarding endpoint and wait for the kernel's answer
/// — the send's `PendingOperation`, which is why it goes `SENDMODE_BLOCK`. `true` once
/// answered.
///
/// **Waits on buffers of its own**, not `WAIT_HANDLES`/`WAIT_RESULTS`: this runs while
/// `serve_loop` is still walking the batch of results a wait wrote there.
fn forget(serve_end: u64, id: u64) -> bool {
    let mut body = [0u8; 8];
    let Some(n) = forget_request(&mut body, id) else {
        return false;
    };
    // SAFETY: FORGET_MSG is a valid buffer, written only here; the rsproto message goes at
    // offset PAYLOAD_OFF.
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

/// If the forwarded request in `RECV_MSG` is a `RESOLVE_RENAME`, perform the rename and
/// reply. `true` if it was handled — the caller must not fall through to the paths that
/// resolve to an object.
///
/// Rename is the one resolve that mutates the tree and hands back **nothing**: the reply is
/// `OBJECT_KIND_NONE`, status-only. Both suffixes are mount-relative and the kernel has
/// already established that they share this binding (and that the caller holds write
/// authority over both), so the server never sees a cross-filesystem move — it just joins
/// each suffix to the mount root and calls [`Volume::rename_path`].
///
/// This runs **before** the directory-session path deliberately: that path infers "this is
/// a directory open" from the suffix naming a directory, and renaming a directory names one
/// too. Without the ordering, `move` on a directory would silently open a session instead.
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
        // Past here the request *is* a rename, so every exit replies rather than falling
        // through — a fall-through would resolve the source path as an ordinary open and
        // hand back an object the caller never asked for.
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
        return true; // malformed rename — the error reply went out above
    };

    let done = vol.rename_path(&src[..1 + src_len], &dst[..1 + dst_len], flags & RENAME_REPLACE != 0, now_secs());
    match done {
        Ok(replaced) => {
            // A replaced file's blocks are freed before the rename is answered, once the
            // kernel has forgotten it.
            if let Some(id) = replaced {
                forget_then_release(vol, serve_end, id);
            }
            reply_resolve_none(serve_end, request_id)
        }
        Err(e) => reply_resolve_error_why(serve_end, request_id, kerror(e).as_i32(), reason(e)),
    }
    true
}

/// Reply to a completed mutating resolve: success, `OBJECT_KIND_NONE`, no transferred
/// handle. The kernel completes the caller's pending lookup with status `0` and installs
/// nothing.
fn reply_resolve_none(serve_end: u64, request_id: u64) {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_NONE, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at offset PAYLOAD_OFF.
    let count = unsafe {
        let rs_len =
            match librsproto::encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 0) {
                Some(n) => n,
                None => return,
            };
        stage_reply(rs_len, None)
    };
    send_reply(serve_end, count);
}

/// Free directory-session slot `slot`: close the server endpoint and mark it empty.
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

/// Send an error reply for a forwarded resolve (no transferred handle) on `serve_end`.
fn reply_resolve_error(serve_end: u64, request_id: u64, kerror: i32) {
    reply_resolve_error_why(serve_end, request_id, kerror, b"");
}

/// [`reply_resolve_error`] with a reason for a person reading it.
fn reply_resolve_error_why(serve_end: u64, request_id: u64, kerror: i32, why: &[u8]) {
    // SAFETY: disjoint reply region.
    let elen = unsafe {
        let reply = core::slice::from_raw_parts_mut(((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF), MSG_LEN - PAYLOAD_OFF);
        encode_error(reply, request_id, kerror, OP_NS_RESOLVE, why)
    };
    let count = stage_reply(elen, None);
    send_reply(serve_end, count);
}

/// Reply `OBJECT_KIND_CHANNEL` to a forwarded `RESOLVE_DIR_OPEN`, transferring `client_end`
/// (the session channel's client side) in `handles[0]` — the logging service's reply shape.
/// `OBJECT_KIND_DIRECTORY` is reserved and never sent. `true` on a successful send.
fn reply_dir_handle(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    // A directory handle is a live channel to the server (the kernel installs the
    // transferred `IpcChannel` from an `OBJECT_KIND_CHANNEL` reply — it has no distinct
    // "directory" reply kind). `content_len` is unused; the channel rides in handles[0].
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at offset PAYLOAD_OFF,
    // and the transferred handle in REPLY_HANDLES[0].
    unsafe {
        let rs_len =
            match librsproto::encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1) {
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

/// Open a directory session for the already-resolved directory `dir`: mint a session
/// channel bound to it and reply `OBJECT_KIND_CHANNEL` with the client endpoint. The kernel
/// installs the transferred channel in the client's table and completes its lookup. On any
/// failure an error reply is sent instead.
fn open_dir_session(serve_end: u64, request_id: u64, dir: u64) {
    // SAFETY: single-threaded scan of the session table.
    let slot = unsafe { (0..session_capacity()).find(|&i| SESSION_CH[i] == 0) };
    let Some(slot) = slot else {
        // Every session slot in use — ask the client to retry (WouldBlock).
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
        // The reply send failed — roll back the session and drop the client endpoint.
        free_session_at(slot);
        // SAFETY: closing our own not-yet-transferred handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, client_end) };
    }
}

/// Serve requests that arrived on an open directory session `session_ch`. Drains the
/// channel: each `File::ReadDir` enumerates the bound directory into a batch reply sent
/// back on the same channel; a `PeerClosed` frees the session.
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
        // Decode the request and copy its rsproto body into an owned buffer (two 255-byte
        // names + prefixes fit), so the op logic + reply staging don't interleave borrows of
        // the shared statics.
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
                    None => {
                        reply_session_error(session_ch, request_id, OP_FILE_READ_DIR, KError::KernelError.as_i32())
                    }
                }
            }
            // The name-addressed mutations: each names an entry in the bound directory, so a
            // handle can never mutate outside it.
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
                        // Session-scoped touch. The *id*-scoped form on the forwarding
                        // endpoint stays as it is: that one is the kernel reporting a
                        // Model A write it just flushed, is fire-and-forget, and has no
                        // client behind it to receive a status.
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

/// Wall-clock time in seconds since the Unix epoch, for stamping timestamps —
/// or `0` if this machine has no anchored clock.
///
/// **The server reads this itself; a client never supplies it.** A timestamp a caller
/// could choose would be forgeable metadata, so the filesystem's own authority for its
/// metadata is the only thing that may set it. (Inside the server, the value is passed
/// down into the filesystem library as a parameter — that boundary exists only because a
/// library is deliberately syscall-free so it can be host-tested, and it is not a trust
/// boundary.)
///
/// `0` propagates as "unknown" rather than as 1970-with-confidence: a library falls
/// back to its fixed sentinel where a nonzero value is structurally required.
fn now_secs() -> i64 {
    let mut ns: u64 = 0;
    // SAFETY: `ns` is a valid writable out-param; the syscall writes 8 bytes.
    let r = unsafe { syscall4(SYS_CLOCK_READ, CLOCK_REALTIME, (&raw mut ns) as u64, 0, 0) };
    if r != 0 {
        return 0; // no anchored wall clock on this machine
    }
    (ns / 1_000_000_000) as i64
}

/// Reply to a mutation op on `session_ch`: an empty-body success reply, or an error reply
/// carrying the mapped `KError`.
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

/// Enumerate directory `dir` from `cursor` into `DIR_BODY`, packing as many entries as
/// fit; returns the body length, or `None` on a device/parse error.
fn build_readdir_reply<V: Volume>(vol: &V, dir: u64, cursor: u64) -> Option<usize> {
    // SAFETY: DIR_BODY is a disjoint static; the writer holds it for this call only.
    let body = unsafe { core::slice::from_raw_parts_mut((&raw mut DIR_BODY) as *mut u8, DIR_BODY_CAP) };
    let mut w = DirReplyWriter::new(body)?;
    // The listing form: each entry carries its size/mtime/mode, so a client gets a
    // complete `Table<{name, size, kind, modified}>` from one round trip per reply.
    let next = vol.read_dir(dir, cursor, |e: &DirEntry| w.push(e.id, e.kind, e.mode, e.size, e.mtime, e.name));
    let next_cursor = next.ok()?;
    Some(w.finish(next_cursor))
}

/// Send a `File::ReadDir` reply (the packed body in `DIR_BODY[..body_len]`) on
/// `session_ch`, wrapping it in the rsproto reply header.
fn send_session_reply(session_ch: u64, request_id: u64, body_len: usize) {
    // SAFETY: DIR_BODY (read) and REPLY_MSG (write) are disjoint statics.
    unsafe {
        let body = core::slice::from_raw_parts((&raw const DIR_BODY) as *const u8, body_len);
        let rs_len =
            match librsproto::encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_FILE_READ_DIR, request_id, RS_FLAG_REPLY, body, 0) {
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

/// Send an error reply for a `File::ReadDir` on a session channel.
fn reply_session_error(session_ch: u64, request_id: u64, op: u16, kerror: i32) {
    reply_session_error_why(session_ch, request_id, op, kerror, b"");
}

/// [`reply_session_error`] with a reason for a person reading it.
fn reply_session_error_why(session_ch: u64, request_id: u64, op: u16, kerror: i32, why: &[u8]) {
    // SAFETY: disjoint reply region.
    let elen = unsafe {
        let reply = core::slice::from_raw_parts_mut(((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF), MSG_LEN - PAYLOAD_OFF);
        encode_error(reply, request_id, kerror, op, why)
    };
    let count = stage_reply(elen, None);
    send_reply(session_ch, count);
}

/// The serve loop: wait on the forwarding endpoint, the control channel and every session, and
/// serve what arrives. Never returns: an unmount or an unreachable endpoint ends the process.
fn serve_loop<V: Volume>(vol: &V, serve_end: u64, device: u64) -> ! {
    loop {
        // Wait set: the forwarding endpoint, the control channel while it is open, and every
        // open directory session (mirrors the logging service). `count ≤ MAX_WAIT_HANDLES` by
        // construction: while the control channel is open, a session never takes the last slot
        // ([`session_capacity`]).
        // SAFETY: single-threaded build of the wait array.
        let count = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1;
            if CONTROL != 0 {
                WAIT_HANDLES[n] = CONTROL;
                n += 1;
            }
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
        // Each signaled handle yields one 24-byte `IoResult` (the handle at offset 0).
        //
        // **Sessions before `serve_end`, in two passes.** One batch routinely contains both
        // a closed session and a new resolve — a shell pipeline does exactly that as stage
        // N exits while stage N+1 starts. Draining `serve_end` first would answer the new
        // `RESOLVE_DIR_OPEN` while the just-closed slots still read as occupied, so a
        // client would see a spurious `WouldBlock` with the table about to be freed a few
        // instructions later. Reclaiming first costs one extra walk of a ≤ 32-entry array
        // and makes a slot's release visible to the open that is waiting for it.
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
                    // Pass 0: the supervisor, and session traffic, including the `PeerClosed`
                    // that frees a slot.
                    // SAFETY: a single-threaded read.
                    (0, false) if h == unsafe { CONTROL } => serve_control(vol),
                    (0, false) => serve_session(vol, h, serve_end),
                    // Pass 1: drain every queued forwarded request on the kernel endpoint.
                    (1, true) => loop {
                        match recv_on(serve_end) {
                            0 => handle_forwarded_resolve(vol, serve_end, device),
                            // **Nothing can reach this server any more** (Phase 6 Part D.4): its
                            // registration and every file it handed out have let go of the other
                            // end. That peer stays signalled, so going round again would spin a
                            // CPU for the rest of the boot — measured: a million passes, after
                            // the storage service let a mount go without unmounting it. Exiting
                            // is what `device-mgr` does in the same place (PR #333 review).
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

/// Serve the control channel after `Ready` (administration Part C.3). Both supervisors keep their
/// end — `init` since Part E.4a, for a shutdown's unmount — but a closed peer is still ordinary:
/// the channel leaves the wait set and serving goes on. **The one request is `Meta::Unmount`**:
/// record the filesystem cleanly unmounted, answer, and exit. By then the supervisor has written
/// back everything the kernel held of it, and nothing more can reach it. A read-only mount writes
/// nothing, since it never marked the filesystem mounted.
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
                let rs_len =
                    unsafe { librsproto::encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_UNMOUNT, request_id, RS_FLAG_REPLY, &[], 0) };
                rs_len.map_or(0, |n| stage_reply(n, None))
            }
            Err(e) => {
                // SAFETY: disjoint reply region.
                let elen = unsafe {
                    let reply =
                        core::slice::from_raw_parts_mut(((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF), MSG_LEN - PAYLOAD_OFF);
                    encode_error(reply, request_id, kerror(e).as_i32(), OP_UNMOUNT, b"the state could not be written")
                };
                stage_reply(elen, None)
            }
        };
        send_reply(control, count);
        // **A read-only mount records nothing**, and says so: the filesystem is as it was found,
        // clean or not, and a line saying "recorded clean" would claim otherwise.
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

/// Handle one forwarded `Namespace::Resolve` already received into `RECV_MSG`. A
/// `RESOLVE_DIR_OPEN` resolve opens a directory session (above); every other resolve takes
/// the file path (Model-A lazy blocks / eager memobj) unchanged.
fn handle_forwarded_resolve<V: Volume>(vol: &V, serve_end: u64, device: u64) {
    // `File::Touch` is not a resolve at all — the kernel telling us a Model A write
    // happened, which we could not otherwise observe. No reply.
    if try_touch(vol) {
        return;
    }

    // A mutating resolve first: it replies status-only and must not be mistaken for a
    // directory open (see `try_resolve_rename`).
    if try_resolve_rename(vol, serve_end) {
        return;
    }

    if let Some((request_id, dir)) = try_resolve_directory(vol) {
        open_dir_session(serve_end, request_id, dir);
        return;
    }

    // The rsproto request occupies the IpcMsg payload (offset 24, `payload_len`
    // bytes). Form non-aliasing slices over the distinct request/content/reply
    // statics via raw pointers.
    // SAFETY: `payload_len` is bounded to the payload region; the three slices
    // address disjoint statics, so no aliasing `&`/`&mut` is formed.
    let request_id;
    let served_op;
    let served = unsafe {
        let req = received();
        request_id = librsproto::decode(req).map(|m| m.request_id).unwrap_or(0);
        let content = core::slice::from_raw_parts_mut((&raw mut CONTENT) as *mut u8, MAX_FILE);
        let reply = core::slice::from_raw_parts_mut(((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF), MSG_LEN - PAYLOAD_OFF);
        let op = librsproto::decode(req).map(|m| m.op).unwrap_or(0);
        served_op = op;
        // Model A grow-on-resolve: a RESOLVE_GROW request grows the file first, so the map
        // `serve` then builds covers the new size. A grow failure falls through — `serve`
        // maps the current size and the reply reflects it.
        match maybe_grow(vol, req, serve_end) {
            Err(e) => Served::Error {
                reply_len: encode_error(reply, request_id, kerror(e).as_i32(), OP_NS_RESOLVE, reason(e)),
            },
            Ok(()) => serve(vol, req, content, reply),
        }
    };

    let count = match served {
        Served::File { reply_len, content_len } => match make_content_memobj(content_len) {
            Some(mem) => stage_reply(reply_len, Some(mem)),
            // Resolved the file but couldn't materialise the object (OOM): turn
            // the reply into an error (carrying the request's op so the kernel
            // routes it to the right pending operation) so it completes cleanly.
            None => {
                // SAFETY: disjoint static; reply region as above.
                let elen = unsafe {
                    let reply =
                        core::slice::from_raw_parts_mut(((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF), MSG_LEN - PAYLOAD_OFF);
                    encode_error(reply, request_id, KError::OutOfMemory.as_i32(), served_op, b"")
                };
                stage_reply(elen, None)
            }
        },
        // A Model A lazy resolve: transfer a READ|TRANSFER duplicate of the device
        // handle (the kernel does the file-data I/O); keep our own for metadata reads.
        Served::LazyBlocks { reply_len } => {
            // SAFETY: `device` is our block-device handle (READ | TRANSFER | DUPLICATE).
            let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, device, RIGHT_READ | RIGHT_TRANSFER) };
            if dup < 0 {
                // Can't share the device — degrade to an error reply.
                // SAFETY: disjoint static; reply region as above.
                let elen = unsafe {
                    let reply =
                        core::slice::from_raw_parts_mut(((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF), MSG_LEN - PAYLOAD_OFF);
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

/// **Steps 1 and 2 of the bootstrap**: receive the block-device handle, and whether to serve it
/// read-only, via the setup message on `control`; then the server's disk over it, made by
/// `disk` — [`Disk::new`](crate::disk::Disk::new) for ext4. A server that cannot do either says
/// so and exits.
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

/// **Steps 3 to 6**: check `vol` before saying there is a filesystem, record a writable mount
/// mounted, say `Ready` on `control` with the forwarding endpoint, and serve `vol` until an
/// unmount. `device` is the block-device handle, which a Model A reply hands the kernel a
/// duplicate of.
pub fn run<V: Volume>(vol: &V, control: u64, device: u64) -> ! {
    // 3. **Check there is a filesystem to serve before saying there is** (Phase 5). A Ready
    //    over a device holding no filesystem mounted it anyway, and the first sign was the first
    //    program that would not load off it. A device that fails the check gets a refusal in
    //    place of the Ready, which the supervisor prints with the device's name, and this
    //    process is done.
    if let Err(why) = vol.check() {
        if !send_refusal(control, &why) {
            fail(b"fs-server: the device failed its check, and the refusal could not be sent\n");
        }
        exit(1);
    }

    // 3b. **How it was left** (administration Part C.3): reported, never refused — a repair tool
    //     is deferred (`TODO(fs-repair)`), and refusing would strand a disk that is most likely
    //     fine. Then a writable mount records itself mounted **before** anything can change the
    //     filesystem, so it never looks clean while it can change; one that cannot record it is
    //     refused. A read-only mount writes nothing.
    if vol.was_left_clean() == Ok(false) {
        kprint(b"fs-server: the filesystem was not cleanly unmounted last time; serving it anyway\n");
    }
    if !vol.read_only() && vol.mark_mounted().is_err() {
        if !send_refusal(control, &V::state_unwritable()) {
            fail(b"fs-server: the state could not be written, and the refusal could not be sent\n");
        }
        exit(1);
    }

    // 4. The forwarding channel: keep the serving end, hand the kernel end to the supervisor.
    let (kernel_end, serve_end) = match make_channel() {
        Some(p) => p,
        None => fail(b"fs-server: channel create failed\n"),
    };

    // 5. Announce readiness, transferring the kernel forwarding endpoint.
    if !send_ready(control, V::NAME, kernel_end) {
        fail(b"fs-server: ready send failed\n");
    }
    // Kept for `Meta::Unmount`, until its peer closes it.
    // SAFETY: single-threaded; set before the serve loop reads it.
    unsafe { CONTROL = control };

    // 6. Serve forwarded requests until an unmount — a read-only mount through the type that
    //    refuses every write, which also marks each file it resolves read-only.
    let mode: &[u8] = if vol.read_only() { b", read-only)\n" } else { b", read-write)\n" };
    say(&[b"fs-server: ready (", V::KIND, mode]);
    serve_loop(vol, serve_end, device)
}
