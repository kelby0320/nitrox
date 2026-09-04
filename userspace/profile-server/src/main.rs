//! `profile-server` — the userspace **profile server** (Phase 3).
//!
//! A forwarding resource server that projects the content-addressed store into
//! user-facing `/bin`: bound at `/bin` by a supervisor (init), it answers forwarded
//! `Namespace::Resolve` lookups (`/bin/foo` → suffix `foo`) by **probing** each package
//! in its manifest — `<pkg>/bin/foo` in the store, in manifest order — and **re-exporting
//! the resolved store `FileObject` handle**. It is pure name resolution: it holds no
//! file content and stays out of the data path (faults on the returned handle go
//! straight to the fs-server). See `docs/architecture/profiles-and-namespace-projection.md`.
//!
//! Structurally identical to `fs-server-ext4` at the IPC/wire/bootstrap layers; the
//! difference is the "produce the object" step (onward `sys_ns_lookup` vs. a block read).
//!
//! `#![no_std]` + `#![no_main]`; `libkern` + `libheap` (alloc for the manifest + path
//! building) + `librsproto` (the wire codec).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libkern::*;
use librsproto::error::error_body;
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, OBJECT_KIND_MEMOBJ, parse_resolve_request, resolve_reply,
};
use librsproto::{OP_FILE_READ_DIR, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};
use profile_server::manifest::{self, Package};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const PAGE: u64 = 4096;

/// One projected program: its name, the package that provides it, and the metadata the
/// store's own directory reported for it.
///
/// The metadata is carried through rather than zeroed so `list /bin` shows real sizes and
/// timestamps — this server is a *view* over the store, and a view that invented its own
/// numbers would be lying about files it does not own.
struct Entry {
    name: String,
    /// Index into the manifest's package list — the package whose `bin/` provided it.
    pkg: usize,
    inode: u32,
    kind: u8,
    mode: u16,
    size: u64,
    mtime: i64,
}

/// The merged `/bin` view, built once on first use.
///
/// **Caching is sound by construction, not by invalidation.** A store path is
/// content-addressed: `/store/<hash>-coreutils-0.1.0/bin/` cannot change contents, because
/// different contents would be a different hash and therefore a different path. What *can*
/// change is which packages a profile names — its membership — and this server reads its
/// manifest exactly once at startup, so it cannot observe that either. See
/// `TODO(profile-generation-refresh)`.
static mut INDEX: Option<Vec<(String, Vec<Entry>)>> = None;
/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
/// The most open `/bin` directory sessions served at once. One `sys_wait` slot is the
/// forwarding endpoint, so the ceiling is the kernel's fan-out limit less that one.
const MAX_SESSIONS: usize = libkern::abi::MAX_WAIT_HANDLES - 1;
/// Open directory sessions: the kept (server) endpoint per slot, `0` = free.
static mut SESSION_CH: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];
/// Which projection each session is listing — `bin` unless it was opened on one of
/// [`PROJECTED`]. A session is bound to a *view*, and there is now more than one.
static mut SESSION_DIR: [&str; MAX_SESSIONS] = ["bin"; MAX_SESSIONS];
/// The projection the resolve currently being answered asked for, handed to
/// [`open_dir_session`] a few lines later in the same loop iteration.
static mut PENDING_DIR: &str = "bin";
static mut WAIT_HANDLES: [u64; libkern::abi::MAX_WAIT_HANDLES] =
    [0; libkern::abi::MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * libkern::abi::MAX_WAIT_HANDLES] =
    [0; 24 * libkern::abi::MAX_WAIT_HANDLES];
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

/// Resolve `path` in namespace `ns` requesting `rights`; return the resolved handle, or
/// `0` on failure. Waits + closes the `PendingOperation`; the resolved handle is the
/// caller's to close/transfer.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return 0;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    // IoResult: status @8..12, resolved handle @16..24.
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO handle (the resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    if waited != 1 || status != 0 {
        0
    } else {
        handle
    }
}

/// Read + parse the system profile manifest from the initramfs. Returns the ordered
/// package list (empty on any failure — the server then resolves nothing).
fn read_manifest(root_ns: u64) -> Vec<Package> {
    let mem = ns_lookup(root_ns, b"/initramfs/etc/profiles/system.toml", RIGHT_MAP_READ);
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
    let packages = core::str::from_utf8(&bytes[..len])
        .map(manifest::parse)
        .unwrap_or_default();
    // SAFETY: closing our own handle (the mapping persists via its own reference).
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
    packages
}

/// Create a connected forwarding-channel pair (depth 4). Returns `(kernel_end,
/// serve_end)`: init binds `kernel_end` as the Userspace-Server endpoint; the server
/// serves on `serve_end`. `None` on failure.
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

/// Send `Meta::Ready` on the control channel, transferring `kernel_end` (the endpoint
/// init binds as a Userspace Server at `/bin`). `false` on any failure.
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
    // SAFETY: valid endpoint + message + 1-handle transfer. NoBlock: init's control
    // inbox starts empty.
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

/// Build the merged `/bin` view by reading each package's `bin/` **through the fs-server**
/// — a real `readdir` of what is on disk, not a recital of the manifest. Packages are read
/// in manifest order and the first provider of a name wins, which is the same precedence
/// resolve uses; a listing that disagreed with what a spawn would find would be worse than
/// no listing.
///
/// A package whose `bin/` cannot be opened is **skipped, not fatal**. A profile naming one
/// broken package should lose that package, not all of `/bin` — the alternative makes every
/// program on the system unreachable to protect the listing's completeness, which is the
/// wrong way round.
fn build_index(root_ns: u64, packages: &[Package], dir: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for (i, pkg) in packages.iter().enumerate() {
        let path = format!("{}/{dir}", pkg.path);
        let mut buf = [0u8; 4096]; // >= IPC_MSG_SIZE, what `Dir::open` requires
        let mut dir = match librsproto::session::Dir::open(root_ns, path.as_bytes(), &mut buf) {
            Ok(d) => d,
            Err(_) => {
                kprint(b"profile-server: package subdirectory unreadable (skipped)\n");
                continue;
            }
        };
        let _ = dir.read_dir(|e| {
            if e.name != b"." && e.name != b".." {
                let name = match core::str::from_utf8(e.name) {
                    Ok(n) => String::from(n),
                    Err(_) => return true, // a non-UTF-8 program name is not addressable
                };
                // First provider wins — manifest order is projection priority.
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
        dir.close();
    }
    out
}

/// The merged view, built on first use.
///
/// **Lazily**, not at startup: eager building costs every boot one directory read per
/// package over IPC, for a view that a boot with no `/bin` consumer never needs.
fn index(root_ns: u64, packages: &[Package], dir: &str) -> &'static [Entry] {
    // SAFETY: single-threaded server; each directory's view is built once and never mutated
    // after, and entries are only ever appended — so a `&'static [Entry]` handed out earlier
    // stays valid.
    unsafe {
        if (*(&raw const INDEX)).is_none() {
            INDEX = Some(Vec::new());
        }
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

/// The package subdirectories this server projects beyond `bin`, each under a namespace path of
/// the same name.
///
/// **A fixed list rather than "whatever a package contains"**, because the alternative makes the
/// *contents* of a store package decide what appears in a session's namespace — and a package is
/// data, not policy. A projection is a name this server offers; a package either fills it or does
/// not.
const PROJECTED: [&str; 1] = ["applications"];

/// Split a resolve suffix into the package subdirectory it names and the entry within it.
///
/// **`/bin` is bound with no subtree base and every other projection with one**, which is what
/// lets one endpoint serve several names. `/bin/list` arrives as `list`; `/applications` arrives
/// as `applications` and `/applications/nxterm.toml` as `applications/nxterm.toml`, because that
/// bind carries `applications` as its base.
///
/// **Matched against [`PROJECTED`] rather than split on the first slash**, which was the first
/// version and is wrong at the root: opening the directory itself yields a *bare* `applications`,
/// indistinguishable by shape from a program of that name in `/bin`. The list makes it
/// distinguishable by name instead. The one collision left — a program actually called
/// `applications` — is a name this server would then shadow, and is recorded here rather than
/// guarded against.
fn projection_of(suffix: &str) -> &'static str {
    let s = suffix.strip_prefix('/').unwrap_or(suffix);
    PROJECTED.into_iter().find(|d| *d == s).unwrap_or("bin")
}

fn split_suffix(suffix: &str) -> (&str, &str) {
    // A subtree base is an *absolute* path — the kernel's `SubtreeBase::from_path` rejects a bare
    // component — so a scoped bind forwards `/applications/x`, not `applications/x`. `/bin` is
    // unscoped and forwards a bare `list`, so this strip is what lets one rule read both.
    let suffix = suffix.strip_prefix('/').unwrap_or(suffix);
    for d in PROJECTED {
        if suffix == d {
            return (d, "");
        }
        if let Some(rest) = suffix.strip_prefix(d).and_then(|r| r.strip_prefix('/')) {
            return (d, rest);
        }
    }
    ("bin", suffix)
}

/// Resolve `suffix` (e.g. `heartbeat`) to its store `FileObject` handle (requested rights
/// + `TRANSFER`, so it can be re-exported). `0` if the profile provides no such program.
///
/// Served from the same index as the listing, deliberately. It was a probe — one
/// `sys_ns_lookup` per package until a hit — which is fine at two packages and is a
/// round trip per package per program spawn at fifty. More importantly, two code paths
/// that must agree about what `/bin` contains is the kind of split that drifts.
fn resolve_in_store(root_ns: u64, packages: &[Package], suffix: &[u8], rights: u64) -> u64 {
    let raw = match core::str::from_utf8(suffix) {
        Ok(s) => s,
        Err(_) => return 0,
    };
    let (dir, name) = split_suffix(raw);
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

/// Free directory-session slot `slot`: close the server endpoint and mark it empty.
///
/// **Called on `PeerClosed`, and that is not optional.** A channel whose peer has closed is
/// permanently `signaled`; a dead session left in the wait set makes `sys_wait` return
/// instantly, forever, and the server spins at 100% of a CPU. That exact bug in
/// `logging-service` stopped the whole system reclaiming exited processes — see the
/// 2026-07-31 decision-log entry.
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

/// Reply to a forwarded resolve with a **directory session**: transfer `client_end` as an
/// `OBJECT_KIND_CHANNEL`. The kernel installs the channel in the caller's table and
/// completes its lookup, so `Dir::open` gets an endpoint back. `true` on a successful send.
fn reply_dir_handle(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    // There is no distinct "directory" reply kind: a directory handle *is* a live channel
    // to the server. `content_len` is unused; the channel rides in handles[0].
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the reply goes at PAYLOAD_OFF and the
    // transferred handle in REPLY_HANDLES[0].
    unsafe {
        let rs_len = match encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            OP_NS_RESOLVE,
            request_id,
            RS_FLAG_REPLY,
            &body,
            1,
        ) {
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

/// Open a directory session over the projected `/bin`: mint a channel, keep the server end
/// in a free slot, and hand the client end back as the resolve's answer.
///
/// The session is bound to the *view*, not to a directory — this server owns a name, not a
/// place, so there is no inode to remember. Everything it will be asked comes from the
/// index.
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

/// Serve one batch of `/bin` entries from `cursor`, packing until the reply is full.
/// Returns the reply body length, or `None` if even the header did not fit.
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

/// Drain requests that arrived on an open directory session. Each `File::ReadDir` answers a
/// batch from the index; `PeerClosed` frees the slot; anything else is `Unsupported` — this
/// is a read-only projection, so `mkdir`/`unlink` on it are refused rather than forwarded
/// to the store.
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
            let payload_len =
                u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
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
        let mut body = [0u8; MSG_LEN - PAYLOAD_OFF - 64];
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

/// Send a success reply on `serve_end` transferring the resolved handle. The kernel
/// completes the original caller's lookup inline (installs the handle), so `NoBlock`.
fn reply_success(serve_end: u64, request_id: u64, handle: u64) {
    let mut body = [0u8; librsproto::namespace::RESOLVE_REPLY_LEN];
    // `content_len` is unused for a transferred handle (the kernel installs it directly).
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

/// Send an error reply on `serve_end` (no transferred handle), echoing `op`/`request_id`
/// so the kernel routes it to the right pending lookup.
fn reply_error(serve_end: u64, request_id: u64, op: u16, kerror: i32) {
    let mut ebody = [0u8; librsproto::error::ERROR_BODY_LEN];
    let elen = error_body(&mut ebody, kerror, 0, b"").unwrap_or(0);
    // SAFETY: REPLY_MSG is a valid buffer.
    let rs_len = unsafe {
        match encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            op,
            request_id,
            RS_FLAG_REPLY | RS_FLAG_ERROR,
            &ebody[..elen],
            0,
        ) {
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

/// The serve loop: block for a forwarded `Namespace::Resolve`, resolve it into the
/// store, and reply. Never returns.
fn serve_loop(root_ns: u64, serve_end: u64, packages: &[Package]) -> ! {
    kprint(b"profile-server: serving /bin over the store\n");
    loop {
        // Wait on the forwarding endpoint plus every open directory session.
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots and `count` is bounded by
        // `1 + MAX_SESSIONS`, which is that limit by construction.
        let (waited, count) = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1usize;
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
        // Each signaled handle is one 24-byte IoResult (the handle at offset 0).
        let mut served_endpoint = false;
        for j in 0..(waited as usize) {
            let off = j * 24;
            // SAFETY: `waited` records were written; `off + 8` stays inside WAIT_RESULTS.
            let h = unsafe {
                u64::from_le_bytes([
                    WAIT_RESULTS[off], WAIT_RESULTS[off + 1], WAIT_RESULTS[off + 2],
                    WAIT_RESULTS[off + 3], WAIT_RESULTS[off + 4], WAIT_RESULTS[off + 5],
                    WAIT_RESULTS[off + 6], WAIT_RESULTS[off + 7],
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

        // Decode the rsproto request from the IpcMsg payload (offset 24, `payload_len`).
        // SAFETY: read the header length + form a bounded read-only slice over RECV_MSG.
        let (op, request_id, handle, ok) = unsafe {
            let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]])
                as usize;
            let req = core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            match decode(req) {
                Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                    // A suffix naming a **projection's root** rather than an entry inside it
                    // is what `Dir::open` asks for: empty for `/bin`, and the projection's own
                    // name for the others (their binds carry a subtree base). `0` for the
                    // handle marks the session case, answered below rather than here, because
                    // minting a channel is a reply of a different shape.
                    Some(r)
                        if core::str::from_utf8(r.suffix)
                            .is_ok_and(|x| split_suffix(x).1.is_empty()) =>
                    {
                        // Remember which view this session will list.
                        let d = core::str::from_utf8(r.suffix).unwrap_or("");
                        // SAFETY: single-threaded; read back by `open_dir_session` below.
                        unsafe { PENDING_DIR = projection_of(d) };
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
            // The projected root: hand back a directory session so `/bin` is listable.
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

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the inherited
/// root namespace (used — resolves `/store` + `/initramfs`), `rdx` = the control-channel
/// endpoint init installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"profile-server: up\n");
    // Read the manifest now — before init releases the initramfs.
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
