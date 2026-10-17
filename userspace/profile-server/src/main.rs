//! `profile-server` — projects the content-addressed store into `/bin` and `/applications`.
//!
//! Bound by a supervisor (init), it answers forwarded `Namespace::Resolve` lookups by
//! finding the first package in manifest order that provides the name, and re-exporting
//! that store `FileObject` handle. It is pure name resolution: it holds no file content
//! and stays out of the data path. See `docs/architecture/profiles-and-namespace-projection.md`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libkern::*;
use librsproto::error::error_body;
use librsproto::namespace::{OBJECT_KIND_CHANNEL, OBJECT_KIND_MEMOBJ, parse_resolve_request, resolve_reply};
use librsproto::{OP_FILE_READ_DIR, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};
use profile_server::manifest::{self, Package};
use profile_server::projection::split_suffix;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const PAGE: u64 = 4096;

/// One projected program, with the metadata the store's own directory reported for it: the
/// view carries real sizes and timestamps rather than inventing numbers for files it does
/// not own.
struct Entry {
    name: String,
    /// Index into the manifest's package list.
    pkg: usize,
    inode: u32,
    kind: u8,
    mode: u16,
    size: u64,
    mtime: i64,
}

/// The merged view per projection, built once on first use.
///
/// Caching needs no invalidation: a store path is content-addressed, so its contents
/// cannot change, and the manifest (the profile's membership) is read once at startup.
/// See `TODO(profile-generation-refresh)`.
static mut INDEX: Option<Vec<(String, Vec<Entry>)>> = None;
/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
/// The most open directory sessions at once; one `sys_wait` slot is the forwarding endpoint.
const MAX_SESSIONS: usize = libkern::abi::MAX_WAIT_HANDLES - 1;
/// Open directory sessions: the kept (server) endpoint per slot, `0` = free.
static mut SESSION_CH: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];
/// Which projection each session is listing.
static mut SESSION_DIR: [&str; MAX_SESSIONS] = ["bin"; MAX_SESSIONS];
/// The projection the resolve being answered asked for, handed to [`open_dir_session`].
static mut PENDING_DIR: &str = "bin";
static mut WAIT_HANDLES: [u64; libkern::abi::MAX_WAIT_HANDLES] = [0; libkern::abi::MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * libkern::abi::MAX_WAIT_HANDLES] = [0; 24 * libkern::abi::MAX_WAIT_HANDLES];
static mut CTRL_OUT0: u64 = 0;
static mut CTRL_OUT1: u64 = 0;

/// Emit `msg` to the serial console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Exit the process (does not return).
fn exit(code: i64) -> ! {
    // SAFETY: SYS_PROCESS_EXIT terminates this process.
    unsafe { syscall1(SYS_PROCESS_EXIT, code as u64) };
    loop {
        core::hint::spin_loop();
    }
}

/// Resolve `path` in `ns` requesting `rights`; the resolved handle, or `0` on failure. It is
/// the caller's to close or transfer.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return 0;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // IoResult: status @8..12, resolved handle @16..24.
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16],
                WAIT_RESULTS[17],
                WAIT_RESULTS[18],
                WAIT_RESULTS[19],
                WAIT_RESULTS[20],
                WAIT_RESULTS[21],
                WAIT_RESULTS[22],
                WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO handle (the resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    if waited != 1 || status != 0 { 0 } else { handle }
}

/// Read and parse the manifest at `/system/profiles/system.toml` on the root filesystem,
/// which `init` has mounted before spawning this process. Empty on any failure: the server
/// then resolves nothing.
fn read_manifest(root_ns: u64) -> Vec<Package> {
    let mem = ns_lookup(root_ns, b"/system/profiles/system.toml", RIGHT_MAP_READ);
    if mem == 0 {
        kprint(b"profile-server: no system profile manifest\n");
        return Vec::new();
    }
    // SAFETY: `mem` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return Vec::new();
    }
    // SAFETY: `addr` is a MAP_READ page holding the manifest bytes + zero padding.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, PAGE as usize) };
    let len = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let packages = core::str::from_utf8(&bytes[..len]).map(manifest::parse).unwrap_or_default();
    // SAFETY: closing our own handle (the mapping persists via its own reference).
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
    packages
}

/// Create a channel pair (depth 4): `(kernel_end, serve_end)`. Init binds `kernel_end`; the
/// server serves on `serve_end`.
fn make_channel() -> Option<(u64, u64)> {
    // SAFETY: CTRL_OUT0/CTRL_OUT1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL_OUT0) as u64, (&raw mut CTRL_OUT1) as u64, 4, 0) };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const CTRL_OUT0).read(), (&raw const CTRL_OUT1).read()) })
}

/// Send `Meta::Ready` on the control channel, transferring `kernel_end` to init to bind.
fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let body_len = match librsproto::meta::ready(&mut body, b"profile-server") {
        Some(n) => n,
        None => return false,
    };
    // SAFETY: REPLY_MSG is a valid 4 KiB buffer; the rsproto message goes at offset 24.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], librsproto::OP_READY, 0, 0, &body[..body_len], 1) {
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
    // SAFETY: valid endpoint + message + 1-handle transfer. NoBlock: init's control inbox starts
    // empty.
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

/// Build the merged view of one projection from a real `readdir` of each package's `<dir>/`.
/// Packages are read in manifest order and the first provider of a name wins, the same
/// precedence resolve uses: a listing that disagreed with what a spawn finds would be worse
/// than none.
///
/// A package whose directory cannot be opened is skipped, not fatal: one broken package
/// must not make every program unreachable.
fn build_index(root_ns: u64, packages: &[Package], dir: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for (i, pkg) in packages.iter().enumerate() {
        let path = format!("{}/{dir}", pkg.path);
        let mut buf = [0u8; 4096]; // >= IPC_MSG_SIZE, what `Dir::open` requires
        let mut listing = match librsproto::session::Dir::open(root_ns, path.as_bytes(), &mut buf) {
            Ok(d) => d,
            Err(_) => {
                kprint(b"profile-server: package subdirectory unreadable (skipped)\n");
                continue;
            }
        };
        let _ = listing.read_dir(|e| {
            if e.name != b"." && e.name != b".." {
                let name = match core::str::from_utf8(e.name) {
                    Ok(n) => String::from(n),
                    Err(_) => return true, // a non-UTF-8 program name is not addressable
                };
                if !out.iter().any(|x| x.name == name) {
                    out.push(Entry {
                        name,
                        pkg: i,
                        inode: e.inode,
                        kind: e.kind,
                        mode: e.mode,
                        size: e.size,
                        mtime: e.mtime,
                    });
                }
            }
            true
        });
        listing.close();
    }
    out
}

/// The merged view, built lazily: eager building costs every boot a directory read per
/// package over IPC for a view that may never be asked for.
fn index(root_ns: u64, packages: &[Package], dir: &str) -> &'static [Entry] {
    // SAFETY: single-threaded server; each directory's view is built once and never mutated
    // after, and entries are only ever appended — so a `&'static [Entry]` handed out earlier
    // stays valid.
    unsafe {
        #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
        if (*(&raw const INDEX)).is_none() {
            INDEX = Some(Vec::new());
        }
        #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
        let cache = match &mut *(&raw mut INDEX) {
            Some(c) => c,
            None => return &[],
        };
        if !cache.iter().any(|(d, _)| d == dir) {
            let built = build_index(root_ns, packages, dir);
            libkern::debug::Line::new()
                .s(b"profile-server: /")
                .s(dir.as_bytes())
                .s(b" index built, ")
                .u(built.len() as u64)
                .s(b" entr(ies)")
                .end();
            cache.push((String::from(dir), built));
        }
        match cache.iter().find(|(d, _)| d == dir) {
            Some((_, v)) => {
                // Reborrow as `'static`: the Vec is never dropped or reallocated in place —
                // pushing to `cache` moves the *tuple*, not the entries' heap buffer.
                let p: *const [Entry] = v.as_slice();
                &*p
            }
            None => &[],
        }
    }
}

/// Resolve `suffix` (e.g. `bin/heartbeat`) to its store `FileObject` handle, with the requested
/// rights plus `TRANSFER` so it can be re-exported. `0` if the profile provides no such
/// program, or the suffix names a projection's root.
///
/// Served from the same index as the listing so the two cannot disagree.
fn resolve_in_store(root_ns: u64, packages: &[Package], suffix: &[u8], rights: u64) -> u64 {
    let raw = match core::str::from_utf8(suffix) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let Some((dir, name)) = split_suffix(raw) else { return 0 };
    if name.is_empty() {
        return 0;
    }
    let idx = index(root_ns, packages, dir);
    let Some(e) = idx.iter().find(|e| e.name == name) else {
        return 0;
    };
    let path = format!("{}/{dir}/{name}", packages[e.pkg].path);
    ns_lookup(root_ns, path.as_bytes(), rights | RIGHT_TRANSFER)
}

/// Free directory-session slot `slot`: close the server endpoint and mark it empty. Must be
/// called on `PeerClosed`: a channel whose peer has closed stays signaled, so a dead session
/// left in the wait set makes `sys_wait` return at once, forever.
fn free_session_at(slot: usize) {
    // SAFETY: single-threaded server; closing our own endpoint and clearing the slot.
    unsafe {
        if SESSION_CH[slot] != 0 {
            syscall1(SYS_HANDLE_CLOSE, SESSION_CH[slot]);
            SESSION_CH[slot] = 0;
            SESSION_DIR[slot] = "bin";
        }
    }
}

/// Reply to a forwarded resolve with a directory session: `client_end` moves to the caller
/// as an `OBJECT_KIND_CHANNEL`. `true` on a successful send.
fn reply_dir_handle(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    // A directory handle is a live channel to the server; `content_len` is unused.
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the transferred handle goes in REPLY_HANDLES[0].
    unsafe {
        let rs_len = match encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1) {
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

/// Open a directory session over projection `dir`: mint a channel, keep the server end in a
/// free slot, and reply with the client end. The session is bound to the view, not an inode.
fn open_dir_session(serve_end: u64, request_id: u64, dir: &'static str) {
    // SAFETY: single-threaded scan of the session table.
    let slot = unsafe { (0..MAX_SESSIONS).find(|&i| SESSION_CH[i] == 0) };
    let Some(slot) = slot else {
        // Every slot in use — ask the client to retry rather than failing the listing.
        reply_error(serve_end, request_id, OP_NS_RESOLVE, KError::WouldBlock.as_i32());
        return;
    };
    let Some((client_end, session_end)) = make_channel() else {
        reply_error(serve_end, request_id, OP_NS_RESOLVE, KError::KernelError.as_i32());
        return;
    };
    // Bind the slot *before* replying, so a fast client's first request cannot arrive
    // before the slot is live.
    // SAFETY: `slot` is free.
    unsafe {
        SESSION_CH[slot] = session_end;
        SESSION_DIR[slot] = dir;
    };
    if !reply_dir_handle(serve_end, request_id, client_end) {
        free_session_at(slot);
        // SAFETY: closing our own not-yet-transferred handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, client_end) };
    }
}

/// Pack entries from `cursor` until the reply is full; the body length, or `None` if even
/// the header did not fit.
fn pack_read_dir(body: &mut [u8], entries: &[Entry], cursor: u64) -> Option<usize> {
    let mut w = librsproto::file::DirReplyWriter::new(body)?;
    let start = cursor as usize;
    let mut i = start;
    while i < entries.len() {
        let e = &entries[i];
        if !w.push(e.inode, e.kind, e.mode, e.size, e.mtime, e.name.as_bytes()) {
            break; // full — the client resumes here via the cursor
        }
        i += 1;
    }
    // A single entry too large for an empty reply would otherwise stall the walk forever.
    if i == start && start < entries.len() {
        i = start + 1;
    }
    let next = if i >= entries.len() { 0 } else { i as u64 };
    Some(w.finish(next))
}

/// Drain requests on an open directory session. `File::ReadDir` is answered from the index;
/// anything else is `Unsupported`: the projection is read-only, so `mkdir`/`unlink` are
/// refused rather than forwarded to the store.
fn serve_session(root_ns: u64, packages: &[Package], session_ch: u64) {
    // SAFETY: single-threaded scan.
    let Some(slot) = (unsafe { (0..MAX_SESSIONS).find(|&i| SESSION_CH[i] == session_ch) }) else {
        return; // already freed earlier in this batch
    };
    loop {
        // SAFETY: valid recv out-params.
        let rr = unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                session_ch,
                (&raw mut RECV_MSG) as u64,
                (&raw mut RECV_HANDLES) as u64,
                (&raw mut RECV_COUNT) as u64,
            )
        };
        if rr != 0 {
            if rr == KError::PeerClosed.as_i32() as i64 {
                free_session_at(slot);
            }
            return; // WouldBlock (drained) or PeerClosed (freed)
        }
        // SAFETY: bounded read-only slice over the just-received message.
        let (op, request_id, cursor, ok) = unsafe {
            let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let req = core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            match decode(req) {
                Ok(m) if m.op == OP_FILE_READ_DIR => match librsproto::file::parse_read_dir_request(m.body) {
                    Some(r) => (m.op, m.request_id, r.cursor, true),
                    None => (m.op, m.request_id, 0, false),
                },
                Ok(m) => (m.op, m.request_id, 0, false),
                Err(_) => (0, 0, 0, false),
            }
        };
        if !ok {
            reply_error(session_ch, request_id, op, KError::Unsupported.as_i32());
            continue;
        }
        // SAFETY: single-threaded; `slot` is this session's, bound at mint time.
        let dir = unsafe { SESSION_DIR[slot] };
        let entries = index(root_ns, packages, dir);
        let mut body = [0u8; librsproto::RS_MAX_BODY];
        let Some(blen) = pack_read_dir(&mut body, entries, cursor) else {
            reply_error(session_ch, request_id, op, KError::KernelError.as_i32());
            continue;
        };
        // SAFETY: REPLY_MSG is a valid buffer; no transferred handles on a ReadDir reply.
        unsafe {
            let rs_len = match encode(
                &mut REPLY_MSG[PAYLOAD_OFF..],
                OP_FILE_READ_DIR,
                request_id,
                RS_FLAG_REPLY,
                &body[..blen],
                0,
            ) {
                Some(n) => n,
                None => continue,
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
}

/// Send a success reply on `serve_end` transferring the resolved handle; the kernel installs
/// it in the caller's table inline, so the send is `NoBlock`.
fn reply_success(serve_end: u64, request_id: u64, handle: u64) {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_MEMOBJ, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at offset 24.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, 1) {
            Some(n) => n,
            None => return,
        }
    };
    // SAFETY: stamp the header (payload_len @4, handle_count @8) + the handle slot.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = handle;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        );
    }
}

/// Send an error reply (no handle), echoing `op`/`request_id` so the kernel routes it to the
/// pending lookup.
fn reply_error(serve_end: u64, request_id: u64, op: u16, kerror: i32) {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = error_body(&mut ebody, kerror, 0, b"").unwrap_or(0);
    // SAFETY: REPLY_MSG is a valid buffer.
    let rs_len = unsafe {
        match encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &ebody[..elen], 0) {
            Some(n) => n,
            None => return,
        }
    };
    // SAFETY: stamp the header; no transferred handles.
    unsafe {
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            serve_end,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        );
    }
}

/// The serve loop: wait for a forwarded `Namespace::Resolve`, resolve it, and reply.
fn serve_loop(root_ns: u64, serve_end: u64, packages: &[Package]) -> ! {
    kprint(b"profile-server: serving /bin over the store\n");
    loop {
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots and `count` is bounded by
        // `1 + MAX_SESSIONS`, which is that limit by construction.
        let (waited, count) = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1usize;
            #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
            for i in 0..MAX_SESSIONS {
                if SESSION_CH[i] != 0 {
                    WAIT_HANDLES[n] = SESSION_CH[i];
                    n += 1;
                }
            }
            let w = syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                n as u64,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            );
            (w, n)
        };
        let _ = count;
        if waited < 1 {
            continue;
        }
        let mut served_endpoint = false;
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
                served_endpoint = true;
            } else {
                serve_session(root_ns, packages, h);
            }
        }
        if !served_endpoint {
            continue;
        }
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

        // SAFETY: read the header length + form a bounded read-only slice over RECV_MSG.
        let (op, request_id, handle, ok) = unsafe {
            let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let req = core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            match decode(req) {
                Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                    // A suffix naming a projection's root is what `Dir::open` asks for. Handle
                    // `0` marks the session case, answered below: minting a channel is a
                    // reply of a different shape.
                    Some(r)
                        if core::str::from_utf8(r.suffix)
                            .ok()
                            .and_then(split_suffix)
                            .is_some_and(|(_, name)| name.is_empty()) =>
                    {
                        let d = core::str::from_utf8(r.suffix).ok().and_then(split_suffix).map_or("bin", |(d, _)| d);
                        // This block's `unsafe`: single-threaded, and read back a few lines below.
                        PENDING_DIR = d;
                        (m.op, m.request_id, 0, true)
                    }
                    Some(r) => {
                        let h = resolve_in_store(root_ns, packages, r.suffix, r.requested_rights);
                        (m.op, m.request_id, h, h != 0)
                    }
                    None => (m.op, m.request_id, 0, false),
                },
                Ok(m) => (m.op, m.request_id, 0, false),
                Err(_) => (0, 0, 0, false),
            }
        };

        if ok && handle != 0 {
            reply_success(serve_end, request_id, handle);
        } else if ok && op == OP_NS_RESOLVE {
            // SAFETY: single-threaded; set by the resolve arm immediately above.
            let dir = unsafe { PENDING_DIR };
            open_dir_session(serve_end, request_id, dir);
        } else if op == OP_NS_RESOLVE {
            reply_error(serve_end, request_id, op, KError::NotFound.as_i32());
        } else {
            reply_error(serve_end, request_id, op, KError::Unsupported.as_i32());
        }
    }
}

/// Bootstrap registers: `rsi` = the inherited root namespace (resolves `/store` and the
/// manifest), `rdx` = the control-channel endpoint init installed; `rdi` and `rcx` unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"profile-server: up\n");
    let packages = read_manifest(root_ns);
    kprint(b"profile-server: manifest loaded\n");

    let (kernel_end, serve_end) = match make_channel() {
        Some(pair) => pair,
        None => {
            kprint(b"profile-server: channel create FAIL\n");
            exit(1);
        }
    };
    if !send_ready(control, kernel_end) {
        kprint(b"profile-server: Ready send FAIL\n");
        exit(1);
    }
    serve_loop(root_ns, serve_end, &packages);
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"profile-server: PANIC\n");
    exit(1);
}
