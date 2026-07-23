//! `fs-server-ext4` — the server `[[bin]]` (slice 7 Part 4).
//!
//! The first **userspace resource server**: a process that reads a read-only ext4
//! filesystem over a block device and serves it through the resource-server
//! protocol, reached transparently via the namespace (the kernel forwards
//! `sys_ns_lookup` to it — slice 7 Part 3).
//!
//! ## Bootstrap (driven by init in Part 6)
//!
//! 1. init spawns this process, installing **one** handle — a **control channel**
//!    endpoint — which the kernel delivers in `rdx` (`_start`'s third argument).
//! 2. init sends a **setup message** on that channel transferring the **read-only
//!    block-device** handle; the server receives it ([`recv_device`]).
//! 3. The server creates a **forwarding channel** pair, keeps the serving end, and
//!    sends `Meta::Ready` on the control channel **transferring the other (kernel)
//!    end** ([`send_ready`]); init binds that endpoint as a Userspace Server.
//! 4. The server loops: recv a forwarded `Namespace::Resolve`, read the file, and
//!    reply transferring a read-only `MemoryObject` of its content ([`serve_loop`]).
//!
//! The request→reply logic lives in the host-tested [`fs_server_ext4::serve`]
//! module; this file is only the syscall plumbing + the `BlockReader` over
//! `sys_io_submit`. **Alloc-free** — fixed `.bss` buffers, no global allocator.
//!
//! It never holds `BIND_NAMESPACE` (init binds its endpoint) and receives only the
//! handles it needs at spawn — see `CLAUDE.md` and
//! `docs/rationale/why-supervisor-registration.md`.

#![no_std]
#![no_main]

use core::arch::asm;
use fs_server_ext4::serve::{MAX_SUFFIX, Served, encode_error, serve};
use fs_server_ext4::{BlockReader, BlockWriter, FsError, ext4};
use librsproto::file::{
    DIRENT_KIND_DIR, DIRENT_KIND_FILE, DIRENT_KIND_SYMLINK, DIRENT_KIND_UNKNOWN, DirReplyWriter,
    parse_name_request, parse_read_dir_request, parse_rename_request,
};
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, RESOLVE_CREATE, RESOLVE_GROW, RESOLVE_REPLY_LEN, parse_resolve_grow_size,
    parse_resolve_request, resolve_reply,
};
use librsproto::{
    OP_FILE_MKDIR, OP_FILE_READ_DIR, OP_FILE_RENAME, OP_FILE_RMDIR, OP_FILE_UNLINK, OP_NS_RESOLVE,
    RS_FLAG_REPLY,
};
use libkern::*;

/// One page; the scratch buffer + memory-object granularity.
const PAGE: u64 = 4096;
/// Block-device sector size (the `sys_io_submit` transfer unit).
const SECTOR: usize = 512;
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
/// Scratch for the file content (the 64 KiB read-model cap).
static mut CONTENT: [u8; ext4::MAX_FILE] = [0; ext4::MAX_FILE];
/// `sys_wait` scratch: the forwarding endpoint plus every open directory session. The
/// kernel's `MAX_WAIT_HANDLES` is 8; one slot is `serve_end`, so up to [`MAX_SESSIONS`]
/// directory sessions can be waited on at once. Each result is a 24-byte `IoResult`.
static mut WAIT_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_RESULTS: [u8; 8 * 24] = [0; 8 * 24];

/// The most open directory-handle sessions the server serves concurrently (one `sys_wait`
/// slot is reserved for `serve_end`). Sessions are short-lived — a client opens a
/// directory, reads it, and closes — so this bounds concurrent *in-flight* listings, not
/// total clients. Lifting it (an aggregate wait, or a multi-endpoint receive) is a later
/// refinement; a full slot table returns `WouldBlock` on `RESOLVE_DIR_OPEN`.
const MAX_SESSIONS: usize = 7;
/// Per-session state: the kept (server) endpoint (`0` = free slot) and the directory inode
/// the session is bound to. A session addresses entries by name, never path, so it can
/// only ever touch this inode's directory (structural confinement).
static mut SESSION_CH: [u64; MAX_SESSIONS] = [0; MAX_SESSIONS];
static mut SESSION_INO: [u32; MAX_SESSIONS] = [0; MAX_SESSIONS];
/// Body scratch for a `File::ReadDir` reply (packed entries), before the rsproto header is
/// prepended into `REPLY_MSG`. Bounded to one IPC payload minus the two headers.
const DIR_BODY_CAP: usize = MSG_LEN - PAYLOAD_OFF - librsproto::RS_HEADER_LEN;
static mut DIR_BODY: [u8; DIR_BODY_CAP] = [0; DIR_BODY_CAP];

/// Log `msg` and exit non-zero — the bootstrap failure path. (The server is not a
/// critical-path process like init/eshell, so exiting on a bootstrap fault is the
/// correct disposition; a supervisor observes the exit.)
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    exit(1)
}

/// Wait on a single handle then read a completed `PendingOperation`'s
/// `(status, result)` (`IoResult`: status @8, result @16) and close it.
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
    let status = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    let result = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ])
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 { (-1, 0) } else { (status, result) }
}

/// A [`BlockReader`] over the block device: each `read_at` reads the covering
/// 512-byte sectors via `sys_io_submit` into a one-page scratch `MemoryObject`
/// (mapped R/W) and copies the requested sub-range out. Sector-at-a-time — simple
/// and correct; the parser's reads are small (≤ one 4 KiB filesystem block).
struct DiskReader {
    /// The read-only block-device handle (from the setup message).
    device: u64,
    /// A one-page scratch `MemoryObject` the device DMAs sectors into.
    scratch: u64,
    /// `scratch`, mapped R/W into this process — where read sectors land.
    scratch_addr: u64,
}

impl DiskReader {
    /// DMA `sector` (512 bytes) into the scratch object; `Io` on any failure.
    fn read_sector(&self, sector: u64) -> Result<(), FsError> {
        let op = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: self.scratch,
            buf_offset: 0,
            offset: sector * SECTOR as u64,
            length: SECTOR as u64,
        };
        // SAFETY: `device` is a block DeviceNode with READ; `&op` is a valid IoOp.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, self.device, (&op as *const IoOp) as u64) };
        if po < 0 {
            return Err(FsError::Io);
        }
        let (status, result) = po_wait(po as u64);
        if status != 0 || result != SECTOR as u64 {
            return Err(FsError::Io);
        }
        Ok(())
    }
}

impl BlockReader for DiskReader {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let cur = offset + done as u64;
            let sector = cur / SECTOR as u64;
            let in_sector = (cur % SECTOR as u64) as usize;
            let n = core::cmp::min(SECTOR - in_sector, buf.len() - done);
            self.read_sector(sector)?;
            // SAFETY: `scratch_addr` maps a full page R/W; the read sector occupies
            // `[0, 512)`, so `[in_sector, in_sector + n)` is in bounds.
            let src = unsafe {
                core::slice::from_raw_parts(
                    (self.scratch_addr as usize + in_sector) as *const u8,
                    n,
                )
            };
            buf[done..done + n].copy_from_slice(src);
            done += n;
        }
        Ok(())
    }
}

impl DiskReader {
    /// Write the scratch object's sector-0 (512 bytes) to device `sector`; `Io` on failure.
    fn write_sector(&self, sector: u64) -> Result<(), FsError> {
        let op = IoOp {
            opcode: IO_OPCODE_WRITE,
            flags: 0,
            buffer: self.scratch,
            buf_offset: 0,
            offset: sector * SECTOR as u64,
            length: SECTOR as u64,
        };
        // SAFETY: `device` is a block DeviceNode with WRITE; `&op` is a valid IoOp.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, self.device, (&op as *const IoOp) as u64) };
        if po < 0 {
            return Err(FsError::Io);
        }
        let (status, result) = po_wait(po as u64);
        if status != 0 || result != SECTOR as u64 {
            return Err(FsError::Io);
        }
        Ok(())
    }
}

impl BlockWriter for DiskReader {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        let mut done = 0usize;
        while done < buf.len() {
            let cur = offset + done as u64;
            let sector = cur / SECTOR as u64;
            let in_sector = (cur % SECTOR as u64) as usize;
            let n = core::cmp::min(SECTOR - in_sector, buf.len() - done);
            // Read-modify-write for a partial sector: read it into scratch first so the
            // untouched bytes are preserved. A full-sector write skips the read.
            if n != SECTOR {
                self.read_sector(sector)?;
            }
            // SAFETY: `scratch_addr` maps a full page R/W; `[in_sector, in_sector + n)` is
            // within the sector-0 region `[0, 512)`.
            let dst = unsafe {
                core::slice::from_raw_parts_mut(
                    (self.scratch_addr as usize + in_sector) as *mut u8,
                    n,
                )
            };
            dst.copy_from_slice(&buf[done..done + n]);
            self.write_sector(sector)?;
            done += n;
        }
        Ok(())
    }
}

/// Receive the setup message on the control channel and return the transferred
/// block-device handle (its `handles[0]`). `None` on any failure.
fn recv_device(control: u64) -> Option<u64> {
    // SAFETY: one waiter on the control endpoint.
    let waited = unsafe {
        WAIT_HANDLES[0] = control;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
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
    Some(unsafe { (&raw const RECV_HANDLES[0]).read() })
}

/// Create a connected channel pair (depth 4), returning `(kernel_end, serve_end)`.
fn make_channel() -> Option<(u64, u64)> {
    static mut E0: u64 = 0;
    static mut E1: u64 = 0;
    // SAFETY: E0/E1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut E0) as u64, (&raw mut E1) as u64, 4, 0)
    };
    if cr != 0 {
        return None;
    }
    // SAFETY: the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const E0).read(), (&raw const E1).read()) })
}

/// Send `Meta::Ready` on the control channel, transferring `kernel_end` (the
/// endpoint init binds as a Userspace Server). `false` on any failure.
fn send_ready(control: u64, kernel_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let body_len = match librsproto::meta::ready(&mut body, b"fs-server-ext4") {
        Some(n) => n,
        None => return false,
    };
    // SAFETY: REPLY_MSG is a valid 4 KiB buffer; the rsproto message goes in the
    // IPC payload region (offset 24).
    let rs_len = unsafe {
        match librsproto::encode(
            &mut REPLY_MSG[24..],
            librsproto::OP_READY,
            0,
            0,
            &body[..body_len],
            1,
        ) {
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
    // SAFETY: valid endpoint + message + 1-handle transfer. NoBlock: init's control
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

/// The serve loop: block for a forwarded `Namespace::Resolve`, resolve it, and
/// reply. Never returns (the server runs until torn down).
/// If `req` is a `RESOLVE_GROW` resolve, grow the named file to the requested size (the
/// write path's ext4 metadata mutation); if it also carries `RESOLVE_CREATE`, create the
/// file first (allocate an inode + insert a directory entry in the parent). Best-effort —
/// any parse / create / grow error is ignored and the subsequent `serve` maps the file at
/// its current size (the reply reflects that, so a failed create surfaces as `NotFound`).
fn maybe_grow<RW: BlockReader + BlockWriter>(reader: &RW, req: &[u8]) {
    let Ok(m) = librsproto::decode(req) else {
        return;
    };
    if m.op != OP_NS_RESOLVE {
        return;
    }
    let Some(r) = parse_resolve_request(m.body) else {
        return;
    };
    if r.flags & RESOLVE_GROW == 0 || r.suffix.len() > MAX_SUFFIX {
        return;
    }
    let Some(new_size) = parse_resolve_grow_size(m.body) else {
        return;
    };
    let mut path = [0u8; MAX_SUFFIX + 1];
    path[0] = b'/';
    path[1..1 + r.suffix.len()].copy_from_slice(r.suffix);
    let path = &path[..1 + r.suffix.len()];

    // Create-on-resolve: split the absolute path into parent dir + leaf name at the last
    // `/`, then allocate the inode + link it into the parent. Idempotent (existing file →
    // its inode), so a re-resolve of an already-created file is harmless.
    if r.flags & RESOLVE_CREATE != 0 {
        if let Some(slash) = path.iter().rposition(|&b| b == b'/') {
            let parent = if slash == 0 { &b"/"[..] } else { &path[..slash] };
            let name = &path[slash + 1..];
            let _ = ext4::create_file(reader, parent, name);
        }
    }

    let _ = ext4::grow_file(reader, path, new_size as usize);
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

/// Map an ext4 `ext4_dir_entry_2.file_type` to the neutral wire kind.
fn map_kind(ext4_ft: u8) -> u8 {
    match ext4_ft {
        1 => DIRENT_KIND_FILE, // EXT4_FT_REG_FILE
        ext4::EXT4_FT_DIR => DIRENT_KIND_DIR,
        ext4::EXT4_FT_SYMLINK => DIRENT_KIND_SYMLINK,
        _ => DIRENT_KIND_UNKNOWN,
    }
}

/// If the forwarded request in `RECV_MSG` is a `Namespace::Resolve` whose suffix names a
/// **directory**, return its `(request_id, directory inode)`. A directory path resolves to
/// a directory session (below); a file path (or a miss) returns `None` and takes the file
/// path. Resolve flags are not plumbed from userspace, so the kind is inferred from what
/// the path actually names — the same way the logging service decides `OBJECT_KIND_CHANNEL`
/// by what it resolves. (The directory walk is repeated by the file path for a non-match;
/// folding it into a single `serve` pass — a `Served::Directory` — is a later refinement.)
fn try_resolve_directory<R: BlockReader + BlockWriter>(reader: &R) -> Option<(u64, u32)> {
    let mut suffix = [0u8; MAX_SUFFIX];
    // SAFETY: `RECV_MSG` holds a just-received message; the slice is bounded.
    let (request_id, suffix_len) = unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        let m = librsproto::decode(req).ok()?;
        if m.op != OP_NS_RESOLVE {
            return None;
        }
        let r = parse_resolve_request(m.body)?;
        let n = r.suffix.len().min(MAX_SUFFIX);
        suffix[..n].copy_from_slice(&r.suffix[..n]);
        (m.request_id, n)
    };
    let dir_ino = ext4::resolve_dir(reader, &suffix[..suffix_len]).ok()?;
    Some((request_id, dir_ino))
}

/// Free directory-session slot `slot`: close the server endpoint and mark it empty.
fn free_session_at(slot: usize) {
    // SAFETY: `slot < MAX_SESSIONS`; closing our own endpoint handle.
    unsafe {
        let ch = SESSION_CH[slot];
        SESSION_CH[slot] = 0;
        SESSION_INO[slot] = 0;
        if ch != 0 {
            syscall1(SYS_HANDLE_CLOSE, ch);
        }
    }
}

/// Send an error reply for a forwarded resolve (no transferred handle) on `serve_end`.
fn reply_resolve_error(serve_end: u64, request_id: u64, kerror: i32) {
    // SAFETY: disjoint reply region.
    let elen = unsafe {
        let reply = core::slice::from_raw_parts_mut(
            ((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF),
            MSG_LEN - PAYLOAD_OFF,
        );
        encode_error(reply, request_id, kerror, OP_NS_RESOLVE)
    };
    let count = stage_reply(elen, None);
    send_reply(serve_end, count);
}

/// Reply `OBJECT_KIND_DIRECTORY` to a forwarded `RESOLVE_DIR_OPEN`, transferring
/// `client_end` (the session channel's client side) in `handles[0]`. Mirrors the logging
/// service's `OBJECT_KIND_CHANNEL` reply. `true` on a successful send.
fn reply_dir_handle(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    // A directory handle is a live channel to the server (the kernel installs the
    // transferred `IpcChannel` from an `OBJECT_KIND_CHANNEL` reply — it has no distinct
    // "directory" reply kind). `content_len` is unused; the channel rides in handles[0].
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    // SAFETY: REPLY_MSG is a valid buffer; the rsproto reply goes at offset PAYLOAD_OFF,
    // and the transferred handle in REPLY_HANDLES[0].
    unsafe {
        let rs_len = match librsproto::encode(
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

/// Open a directory session for the already-resolved directory `dir_ino`: mint a session
/// channel bound to it and reply `OBJECT_KIND_CHANNEL` with the client endpoint. The kernel
/// installs the transferred channel in the client's table and completes its lookup. On any
/// failure an error reply is sent instead.
fn open_dir_session(serve_end: u64, request_id: u64, dir_ino: u32) {
    // SAFETY: single-threaded scan of the session table.
    let slot = unsafe { (0..MAX_SESSIONS).find(|&i| SESSION_CH[i] == 0) };
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
        SESSION_INO[slot] = dir_ino;
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
fn serve_session<R: BlockReader + BlockWriter>(reader: &R, session_ch: u64) {
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
        // the shared statics. SAFETY: RECV_MSG holds the request; the slice is bounded.
        let mut body_buf = [0u8; 600];
        let (request_id, op, body_len) = unsafe {
            let payload_len =
                u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
            let req = core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            match librsproto::decode(req) {
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
        let dir_ino = unsafe { SESSION_INO[slot] };

        match op {
            OP_FILE_READ_DIR => {
                let cursor = parse_read_dir_request(body).map(|r| r.cursor).unwrap_or(0);
                match build_readdir_reply(reader, dir_ino, cursor) {
                    Some(bl) => send_session_reply(session_ch, request_id, bl),
                    None => reply_session_error(session_ch, request_id, OP_FILE_READ_DIR, KError::KernelError.as_i32()),
                }
            }
            // The name-addressed mutations: each names an entry in the bound directory, so a
            // handle can never mutate outside it.
            OP_FILE_MKDIR | OP_FILE_UNLINK | OP_FILE_RMDIR => {
                let r = match parse_name_request(body) {
                    Some(name) => match op {
                        OP_FILE_MKDIR => ext4::mkdir_at(reader, dir_ino, name),
                        OP_FILE_UNLINK => ext4::unlink_at(reader, dir_ino, name),
                        _ => ext4::rmdir_at(reader, dir_ino, name),
                    },
                    None => Err(FsError::Unsupported),
                };
                reply_session_status(session_ch, request_id, op, r);
            }
            OP_FILE_RENAME => {
                let r = match parse_rename_request(body) {
                    Some((old, new)) => ext4::rename_at(reader, dir_ino, old, new),
                    None => Err(FsError::Unsupported),
                };
                reply_session_status(session_ch, request_id, op, r);
            }
            _ => reply_session_error(session_ch, request_id, op, KError::Unsupported.as_i32()),
        }
    }
}

/// Map an `FsError` to the `KError` discriminant carried in a reply.
fn fs_kerror(e: FsError) -> i32 {
    match e {
        FsError::NotFound => KError::NotFound.as_i32(),
        FsError::Unsupported => KError::Unsupported.as_i32(),
        FsError::TooLarge => KError::OutOfMemory.as_i32(),
        FsError::Exists | FsError::NotEmpty => KError::InvalidArgument.as_i32(),
        FsError::Corrupt | FsError::Io => KError::KernelError.as_i32(),
    }
}

/// Reply to a mutation op on `session_ch`: an empty-body success reply, or an error reply
/// carrying the mapped `KError`.
fn reply_session_status(session_ch: u64, request_id: u64, op: u16, r: Result<(), FsError>) {
    match r {
        Ok(()) => {
            // SAFETY: REPLY_MSG is a valid buffer; an empty-body reply, no handles.
            unsafe {
                let rs_len = match librsproto::encode(
                    &mut REPLY_MSG[PAYLOAD_OFF..],
                    op,
                    request_id,
                    RS_FLAG_REPLY,
                    &[],
                    0,
                ) {
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
        Err(e) => reply_session_error(session_ch, request_id, op, fs_kerror(e)),
    }
}

/// Enumerate directory `dir_ino` from `cursor` into `DIR_BODY`, packing as many entries as
/// fit; returns the body length, or `None` on a device/parse error.
fn build_readdir_reply<R: BlockReader>(reader: &R, dir_ino: u32, cursor: u64) -> Option<usize> {
    // SAFETY: DIR_BODY is a disjoint static; the writer holds it for this call only.
    let body = unsafe {
        core::slice::from_raw_parts_mut((&raw mut DIR_BODY) as *mut u8, DIR_BODY_CAP)
    };
    let mut w = DirReplyWriter::new(body)?;
    let next = ext4::read_dir(reader, dir_ino, cursor, |ino, ft, name| {
        w.push(ino, map_kind(ft), name)
    });
    let next_cursor = next.ok()?;
    Some(w.finish(next_cursor))
}

/// Send a `File::ReadDir` reply (the packed body in `DIR_BODY[..body_len]`) on
/// `session_ch`, wrapping it in the rsproto reply header.
fn send_session_reply(session_ch: u64, request_id: u64, body_len: usize) {
    // SAFETY: DIR_BODY (read) and REPLY_MSG (write) are disjoint statics.
    unsafe {
        let body = core::slice::from_raw_parts((&raw const DIR_BODY) as *const u8, body_len);
        let rs_len = match librsproto::encode(
            &mut REPLY_MSG[PAYLOAD_OFF..],
            OP_FILE_READ_DIR,
            request_id,
            RS_FLAG_REPLY,
            body,
            0,
        ) {
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
    // SAFETY: disjoint reply region.
    let elen = unsafe {
        let reply = core::slice::from_raw_parts_mut(
            ((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF),
            MSG_LEN - PAYLOAD_OFF,
        );
        encode_error(reply, request_id, kerror, op)
    };
    let count = stage_reply(elen, None);
    send_reply(session_ch, count);
}

fn serve_loop<R: BlockReader + BlockWriter>(reader: &R, serve_end: u64, device: u64) -> ! {
    loop {
        // Wait set: the forwarding endpoint plus every open directory session (mirrors the
        // logging service). `count ≤ 1 + MAX_SESSIONS = 8 = MAX_WAIT_HANDLES`.
        // SAFETY: single-threaded build of the wait array.
        let count = unsafe {
            WAIT_HANDLES[0] = serve_end;
            let mut n = 1;
            for i in 0..MAX_SESSIONS {
                if SESSION_CH[i] != 0 {
                    WAIT_HANDLES[n] = SESSION_CH[i];
                    n += 1;
                }
            }
            n
        };
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers sized for `count ≤ 8`.
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
        // Each signaled handle yields one 24-byte `IoResult` (the handle at offset 0).
        for j in 0..(waited as usize) {
            // SAFETY: `waited` records were written; `off + 8 ≤ 8*24`.
            let h = unsafe {
                let off = j * 24;
                u64::from_le_bytes([
                    WAIT_RESULTS[off], WAIT_RESULTS[off + 1], WAIT_RESULTS[off + 2],
                    WAIT_RESULTS[off + 3], WAIT_RESULTS[off + 4], WAIT_RESULTS[off + 5],
                    WAIT_RESULTS[off + 6], WAIT_RESULTS[off + 7],
                ])
            };
            if h == serve_end {
                // Drain every queued forwarded request on the kernel endpoint.
                while recv_on(serve_end) == 0 {
                    handle_forwarded_resolve(reader, serve_end, device);
                }
            } else {
                serve_session(reader, h);
            }
        }
    }
}

/// Handle one forwarded `Namespace::Resolve` already received into `RECV_MSG`. A
/// `RESOLVE_DIR_OPEN` resolve opens a directory session (above); every other resolve takes
/// the file path (Model-A lazy blocks / eager memobj) unchanged.
fn handle_forwarded_resolve<R: BlockReader + BlockWriter>(
    reader: &R,
    serve_end: u64,
    device: u64,
) {
    if let Some((request_id, dir_ino)) = try_resolve_directory(reader) {
        open_dir_session(serve_end, request_id, dir_ino);
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
            let payload_len = u32::from_le_bytes([
                RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7],
            ]) as usize;
            let req = core::slice::from_raw_parts(
                ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
                payload_len.min(MSG_LEN - PAYLOAD_OFF),
            );
            request_id = librsproto::decode(req).map(|m| m.request_id).unwrap_or(0);
            let content = core::slice::from_raw_parts_mut(
                (&raw mut CONTENT) as *mut u8,
                ext4::MAX_FILE,
            );
            let reply = core::slice::from_raw_parts_mut(
                ((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF),
                MSG_LEN - PAYLOAD_OFF,
            );
            let op = librsproto::decode(req).map(|m| m.op).unwrap_or(0);
            served_op = op;
            // Model A grow-on-resolve: a RESOLVE_GROW request grows the file first
            // (allocate blocks + extend the extent tree), so the map `serve` then builds
            // covers the new size. A grow failure falls through — `serve` maps the current
            // size and the reply reflects it.
            maybe_grow(reader, req);
            serve(reader, req, content, reply)
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
                        let reply = core::slice::from_raw_parts_mut(
                            ((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF),
                            MSG_LEN - PAYLOAD_OFF,
                        );
                        encode_error(reply, request_id, KError::OutOfMemory.as_i32(), served_op)
                    };
                    stage_reply(elen, None)
                }
            },
            // A Model A lazy resolve: transfer a READ|TRANSFER duplicate of the device
            // handle (the kernel does the file-data I/O); keep our own for metadata reads.
            Served::LazyBlocks { reply_len } => {
                // SAFETY: `device` is our block-device handle (READ | TRANSFER | DUPLICATE).
                let dup = unsafe {
                    syscall2(SYS_HANDLE_DUPLICATE, device, RIGHT_READ | RIGHT_TRANSFER)
                };
                if dup < 0 {
                    // Can't share the device — degrade to an error reply.
                    // SAFETY: disjoint static; reply region as above.
                    let elen = unsafe {
                        let reply = core::slice::from_raw_parts_mut(
                            ((&raw mut REPLY_MSG) as *mut u8).add(PAYLOAD_OFF),
                            MSG_LEN - PAYLOAD_OFF,
                        );
                        encode_error(reply, request_id, KError::KernelError.as_i32(), served_op)
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

/// `_start` bootstrap registers (`kernel/src/syscall/table.rs`): `rdi` = the
/// notification channel (unused), `rsi` = the inherited root namespace (unused —
/// the server resolves nothing), `rdx` = the **control channel** endpoint init
/// installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
    // 1. Receive the block-device handle via the setup message.
    let device = match recv_device(control) {
        Some(d) => d,
        None => fail(b"fs-server: setup recv failed\n"),
    };

    // 2. A one-page scratch MemoryObject (mapped R/W) for the BlockReader.
    let scratch = unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) };
    if scratch < 0 {
        fail(b"fs-server: scratch create failed\n");
    }
    let scratch = scratch as u64;
    let scratch_addr =
        unsafe { syscall4(SYS_MEMORY_MAP, scratch, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if scratch_addr < 0 {
        fail(b"fs-server: scratch map failed\n");
    }
    let reader = DiskReader { device, scratch, scratch_addr: scratch_addr as u64 };

    // 3. The forwarding channel: keep the serving end, hand the kernel end to init.
    let (kernel_end, serve_end) = match make_channel() {
        Some(p) => p,
        None => fail(b"fs-server: channel create failed\n"),
    };

    // 4. Announce readiness, transferring the kernel forwarding endpoint.
    if !send_ready(control, kernel_end) {
        fail(b"fs-server: ready send failed\n");
    }
    kprint(b"fs-server: ready (ext4, read-write)\n");

    // 5. Serve forwarded Resolve requests forever.
    serve_loop(&reader, serve_end, device);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
