//! `storage-service` — the event loop. Everything that decides is in the library half
//! ([`storage_service`]); this file is what cannot be tested on the host: taking `block` from the
//! device manager, reading each device, reading `init.toml`, and serving forwarded resolves.
//!
//! ## Shape (administration Part C.5a)
//!
//! 1. **Own `block`.** Resolve `/svc/devices/block`: the device manager has already queued an
//!    `Arrived` per disk, partition and RAM disk, each carrying this service's own duplicate of
//!    the device's node, then `Settled`. `init` spawns this service straight after the manager, so
//!    it is the class's owner from boot on and nothing else can take the disks.
//! 2. **Read what is there.** Each device is probed as its server would read it: ext4 through
//!    `fs-server-ext4`'s own checks, FAT from its boot sector, or nothing. Each disk's partition
//!    table is read too, for `init.toml`'s UUID sources.
//! 3. **Read `init.toml`**, and match each of `init`'s mounts to the device it is on. Those are
//!    reported and never mounted here. Whether the root is on a RAM disk decides whether this is
//!    a live boot.
//! 4. **Mount what it can serve** (C.5b): every ext4 `init` did not mount, read-only on a live
//!    boot. Each gets an `fs-server-ext4` spawned over it and a namespace of its own with that
//!    server bound at `/`, and is named by its label.
//! 5. **Serve.** Mint a forwarding endpoint and answer `Meta::Ready`; `init` binds it at
//!    `/svc/storage`. `info` is a directory session, `info/<name>.tsm` a table as a fresh
//!    read-only memory object, `fs` a directory of labels, and `fs/<label>/…` a `SUBNAMESPACE`
//!    reply: the kernel continues the resolve in the mount's namespace, so a file fills through
//!    the mounted server's own registration and nothing passes through this service.
//!    `session-endpoint` mints an endpoint on which only `info` and `fs` are answered, and
//!    `admin-endpoint` one on which any resolve opens an **admin session**: a channel for
//!    `Storage` requests (C.5c) — `Mount`, `Unmount` and `InUse`.
//!
//! **An unmount is a chain** (C.5c): the label leaves `fs`, every dirty file is written back
//! (`sys_ns_sync`), the unmount is refused if a file is still held (`sys_ns_held`), the server
//! records the filesystem clean and exits (`Meta::Unmount`), the drive's cache is flushed, and
//! the namespace is dropped.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use fs_server_ext4::{BlockReader, FsError};
use libinittoml::manifest::{self, Mode};
use libkern::abi::{IO_OPCODE_FLUSH, IO_OPCODE_READ, IoOp};
use libkern::debug::Line;
use libkern::device::{DeviceKind, DeviceRecord};
use libkern::*;
use librsproto::devices::{OP_DEVICES_ARRIVED, OP_DEVICES_SETTLED, parse_arrived};
use librsproto::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE, DirReplyWriter, parse_read_dir_request};
use librsproto::meta::{FS_SETUP_READ_ONLY, fs_setup};
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, OBJECT_KIND_MEMOBJ, RESOLVE_REPLY_LEN, SUBNAMESPACE_PREFIX_LEN, parse_resolve_request,
    resolve_reply, subnamespace_reply,
};
use librsproto::{OP_FILE_READ_DIR, OP_NS_RESOLVE, OP_UNMOUNT, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};
use librsproto::storage::{OP_STORAGE_IN_USE, OP_STORAGE_MOUNT, OP_STORAGE_UNMOUNT, build_in_use, parse_mount};
use storage_service::mounts::{self, Plan, automount, explicit, in_use};
use storage_service::probe::{Found, probe};
use storage_service::sources::{DiskTable, InitMount, TableEntry, init_known, init_mounts, live_boot};
use storage_service::suffix::{self, Asked, session_only};
use storage_service::table::{self, By, Device, Mounted};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
/// Where the block class is taken from.
const SUBSCRIPTION: &[u8] = b"/svc/devices/block";
/// Where `init`'s manifest is. The initramfs is `init`'s root namespace's, which this service
/// inherits.
const INIT_TOML: &[u8] = b"/initramfs/etc/init.toml";
/// How long one message of the replay may take to arrive. The manager queues the whole replay
/// before the subscription's resolve completes, so this bounds a manager that broke that rule,
/// not an ordinary wait.
const REPLAY_WAIT_NS: u64 = 2_000_000_000;
/// Session endpoints at once. The two login supervisors ask for one each at boot (C.6); the rest
/// is headroom, not a use.
const MAX_SESSION_ENDPOINTS: usize = 4;
/// Mounts at once. Each keeps its server's control channel in the wait set, to see it exit.
const MAX_MOUNTS: usize = 8;
/// Admin endpoints at once. The view broker asks for one at boot (C.6); the second is headroom.
const MAX_ADMIN_ENDPOINTS: usize = 2;
/// Admin sessions open at once: a `disk --mount` or `--unmount` is one, briefly.
const MAX_ADMIN_SESSIONS: usize = 4;
/// Directory sessions open at once: the wait set, less the endpoint, the subscription, and every
/// other kind's bound.
const MAX_DIRS: usize =
    MAX_WAIT_HANDLES - 2 - MAX_SESSION_ENDPOINTS - MAX_MOUNTS - MAX_ADMIN_ENDPOINTS - MAX_ADMIN_SESSIONS;
/// Where a filesystem server is spawned from: the store's copy, since the root is mounted by now.
const FS_SERVER: &[u8] = b"/bin/fs-server-ext4";
/// How long a filesystem server may take to answer `Meta::Ready`: `init`'s bound for its own.
const READY_TIMEOUT_NS: u64 = 30_000_000_000;
/// How many times an unmount asks whether a file is held before it believes the answer, and how
/// long it parks between: long enough for an IRP's completion on another CPU to finish, short
/// enough that a refused unmount is still prompt.
const HELD_ASKS: u32 = 3;
const HELD_PARK_NS: u64 = 5_000_000;
/// The scratch every device read passes through: 64 KiB, enough for the front of a disk's
/// partition table in one read.
const SCRATCH: usize = 64 * 1024;

static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut REPLY_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut REPLY_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_HANDLES: [u64; MAX_WAIT_HANDLES] = [0; MAX_WAIT_HANDLES];
static mut WAIT_RESULTS: [u8; 24 * MAX_WAIT_HANDLES] = [0; 24 * MAX_WAIT_HANDLES];

fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process holds.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

fn now_ns() -> u64 {
    let mut now = 0u64;
    // SAFETY: `now` is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    now
}

/// Wait on `h` until `deadline`. `true` if it became ready.
fn wait_until(h: u64, deadline: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid; one waiter.
    unsafe {
        WAIT_HANDLES[0] = h;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline) == 1
    }
}

/// Park this thread for `ns`: a one-shot timer, since `sys_wait` takes no empty handle list, so a
/// deadline alone is not a sleep. Returns at once if no timer can be made.
fn park(ns: u64) {
    // SAFETY: register-only syscall; returns a handle or a negative KError.
    let timer = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if timer < 0 {
        return;
    }
    // SAFETY: arming this process's own timer, one-shot at an absolute monotonic time.
    unsafe { syscall4(SYS_TIMER_SET, timer as u64, now_ns().saturating_add(ns), 0, 0) };
    wait_until(timer as u64, u64::MAX);
    close(timer as u64);
}

/// Wait for a pending operation and return its `(status, value)`.
fn po_wait(po: u64) -> (i64, u64) {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid; one waiter, no deadline.
    let (done, status, value) = unsafe {
        WAIT_HANDLES[0] = po;
        let w = syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX);
        let word = |off: usize| u64::from_le_bytes(WAIT_RESULTS[off..off + 8].try_into().unwrap_or([0; 8]));
        (w == 1, word(8) as i64, word(16))
    };
    close(po);
    if done { (status, value) } else { (-1, 0) }
}

/// Send `body` on `ch` as `op`, echoing `request_id`, with `flags`, moving `handles`.
fn send(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handles: &[u64]) -> bool {
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid buffers; single-threaded.
    unsafe {
        let Some(rs_len) =
            encode(&mut REPLY_MSG[PAYLOAD_OFF..], op, request_id, flags, body, handles.len() as u16)
        else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        REPLY_MSG[8] = handles.len() as u8;
        REPLY_HANDLES[..handles.len()].copy_from_slice(handles);
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            handles.len() as u64,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// An error reply: a whole twelve-byte `ErrorBody`, which is what the kernel reads a forwarded
/// resolve's refusal from.
fn reply_error(ch: u64, op: u16, request_id: u64, err: KError) {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, b"").unwrap_or(0);
    let _ = send(ch, op, request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &body[..n], &[]);
}

/// **Reply to a resolve carrying `handle`, or refuse it if the reply will not go.** The handle is
/// moved on success and closed otherwise, and a failed send is answered with an error. A resolve
/// the kernel forwarded waits for its answer with no deadline, so a reply that silently failed
/// would leave the caller blocked for good. That is how a `SUBNAMESPACE` reply first failed here:
/// its namespace lacked `TRANSFER`, which every moved handle needs. `false` if the reply did not go.
fn reply_with_handle(serve_end: u64, request_id: u64, body: &[u8], handle: u64) -> bool {
    if send(serve_end, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, body, &[handle]) {
        return true;
    }
    close(handle);
    kprint(b"storage-service: a reply could not be sent, so it was refused instead\n");
    reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::KernelError);
    false
}

/// Make a channel pair of `depth`. `(a, b)`.
fn make_channel(depth: u64) -> Option<(u64, u64)> {
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let r = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, depth, 0) };
    (r == 0).then_some((a, b))
}

/// One message received: its op, request id, body and the handles that came with it.
struct Received {
    op: u16,
    flags: u32,
    request_id: u64,
    body: Vec<u8>,
    handles: Vec<u64>,
}

/// Receive one message on `ch`. `Ok(None)` if nothing was queued, `Err(())` if the peer has gone.
/// The handles are the caller's: to keep, or to close.
fn recv(ch: u64) -> Result<Option<Received>, ()> {
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr == KError::PeerClosed.as_i32() as i64 {
        return Err(());
    }
    if rr != 0 {
        return Ok(None);
    }
    // SAFETY: the kernel wrote RECV_COUNT handles into RECV_HANDLES.
    let handles = unsafe { RECV_HANDLES[..RECV_COUNT.min(8)].to_vec() };
    // SAFETY: bounded read of the payload the kernel just wrote.
    let msg = unsafe {
        let len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        core::slice::from_raw_parts(((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF), len.min(MSG_LEN - PAYLOAD_OFF))
    };
    match decode(msg) {
        Ok(m) => Ok(Some(Received { op: m.op, flags: m.flags, request_id: m.request_id, body: m.body.to_vec(), handles })),
        Err(_) => {
            handles.iter().for_each(|&h| close(h));
            Ok(None)
        }
    }
}

/// Receive one message on `ch` and close whatever handles came with it: nothing a client sends
/// this service carries a handle it wants, and one kept would be kept for good.
fn recv_request(ch: u64) -> Result<Option<Received>, ()> {
    let m = recv(ch)?;
    if let Some(m) = &m {
        m.handles.iter().for_each(|&h| close(h));
    }
    Ok(m)
}

/// Resolve `path` in `ns` for `rights`. `(status, handle)`.
fn lookup(ns: u64, path: &[u8], rights: u64) -> (i64, u64) {
    // SAFETY: a valid path pointer and a namespace handle this process holds.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return (po, 0);
    }
    po_wait(po as u64)
}

/// Map a read-only memory object whole, run `f` over its bytes, and unmap it.
fn with_mapped<T>(obj: u64, f: impl FnOnce(&[u8]) -> T) -> Option<T> {
    let mut info = abi::HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `info` is a writable 24-byte `HandleInfo`, the layout the kernel writes.
    if unsafe { syscall2(SYS_HANDLE_STAT, obj, (&raw mut info) as u64) } != 0 || info.size == 0 {
        return None;
    }
    // SAFETY: register-only syscall; `obj` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, obj, 0, info.size, RIGHT_MAP_READ) };
    if addr < 0 {
        return None;
    }
    // SAFETY: `info.size` bytes are mapped read-only at `addr` until the unmap below.
    let out = f(unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, info.size as usize) });
    // SAFETY: unmapping what was mapped above; nothing refers to it after `f` returned.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, 0) };
    Some(out)
}

/// **One memory object every device read passes through**, mapped once for the life of the
/// service. Reads are sector-aligned and at most [`SCRATCH`] bytes at a time.
struct Scratch {
    mem: u64,
    addr: u64,
}

impl Scratch {
    fn new() -> Option<Scratch> {
        // SAFETY: a plain anonymous object of SCRATCH bytes.
        let mem = unsafe { syscall4(SYS_MEMORY_CREATE, SCRATCH as u64, 0, 0, 0) };
        if mem <= 0 {
            return None;
        }
        // SAFETY: mapping an object this process just created.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem as u64, 0, SCRATCH as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
        if addr <= 0 {
            close(mem as u64);
            return None;
        }
        Some(Scratch { mem: mem as u64, addr: addr as u64 })
    }
}

/// A device, read through [`Scratch`]: `len` bytes of it, in `sector`-byte units.
struct DeviceIo<'a> {
    node: u64,
    len: u64,
    sector: u64,
    scratch: &'a Scratch,
}

impl DeviceIo<'_> {
    fn new<'a>(node: u64, r: &DeviceRecord, scratch: &'a Scratch) -> DeviceIo<'a> {
        let sector = if r.logical_block_size == 0 { 512 } else { r.logical_block_size as u64 };
        DeviceIo { node, len: sector.saturating_mul(r.block_count), sector, scratch }
    }
}

impl BlockReader for DeviceIo<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let end = offset.checked_add(buf.len() as u64).ok_or(FsError::Io)?;
        if end > self.len {
            return Err(FsError::Io);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let at = offset + done as u64;
            let start = at / self.sector * self.sector;
            let intra = (at - start) as usize;
            let want = (buf.len() - done).min(SCRATCH - intra);
            let span = ((intra + want) as u64).div_ceil(self.sector) * self.sector;
            let op = IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: self.scratch.mem, buf_offset: 0, offset: start, length: span };
            // SAFETY: `node` is a block device handle this process holds with READ; `&op` names a
            // memory object this process owns.
            let po = unsafe { syscall2(SYS_IO_SUBMIT, self.node, (&op as *const IoOp) as u64) };
            if po < 0 {
                return Err(FsError::Io);
            }
            let (status, moved) = po_wait(po as u64);
            if status != 0 || moved != span {
                return Err(FsError::Io);
            }
            // SAFETY: `scratch.addr` maps SCRATCH read-write bytes for the life of the service, and
            // `intra + want <= span <= SCRATCH`; single-threaded, so nothing else holds a reference.
            let src = unsafe { core::slice::from_raw_parts(self.scratch.addr as *const u8, span as usize) };
            buf[done..done + want].copy_from_slice(&src[intra..intra + want]);
            done += want;
        }
        Ok(())
    }
}

/// Take `block`: every device the manager queued before the resolve completed, until `Settled`.
/// `Err` with the reason if the class could not be taken.
fn take_block(root_ns: u64) -> Result<(u64, Vec<(DeviceRecord, u64)>), &'static [u8]> {
    let (st, sub) = lookup(root_ns, SUBSCRIPTION, RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if st == KError::AlreadyExists.as_i32() as i64 {
        return Err(b"block is already owned, so the disks are someone else's");
    }
    if st != 0 || sub == 0 {
        return Err(b"/svc/devices/block did not resolve, so there are no disks to serve");
    }
    let mut devices = Vec::new();
    loop {
        let m = match recv(sub) {
            Ok(Some(m)) => m,
            Ok(None) if wait_until(sub, now_ns().saturating_add(REPLAY_WAIT_NS)) => continue,
            _ => {
                devices.iter().for_each(|&(_, node)| close(node));
                close(sub);
                return Err(b"the device manager's replay stopped before Settled");
            }
        };
        match (m.op, parse_arrived(&m.body).and_then(DeviceRecord::read), m.handles.as_slice()) {
            (OP_DEVICES_SETTLED, _, _) => return Ok((sub, devices)),
            (OP_DEVICES_ARRIVED, Some(r), &[node]) => devices.push((r, node)),
            _ => m.handles.iter().for_each(|&h| close(h)),
        }
    }
}

/// A disk's partition table, as `libgpt` reads it: its entries in use, in array order.
fn read_table(io: &DeviceIo, disk: u32) -> Option<DiskTable> {
    let mut front = alloc::vec![0u8; libgpt::table::FRONT_BYTES];
    io.read_at(0, &mut front).ok()?;
    let t = libgpt::table::read(&front).ok()?;
    let entries = t
        .partitions()
        .iter()
        .map(|p| TableEntry { guid: p.unique_guid, name: p.name().to_vec(), blocks: p.blocks() })
        .collect();
    Some(DiskTable { disk, entries })
}

/// `init`'s manifest, or why it could not be read — each way it can fail says so, since a service
/// that only said "no manifest" would leave the reason to a debugger.
fn read_manifest(root_ns: u64) -> Result<manifest::Manifest, &'static [u8]> {
    // `INSPECT` as well as `MAP_READ`: the object's size comes from `sys_handle_stat`.
    let (st, obj) = lookup(root_ns, INIT_TOML, RIGHT_MAP_READ | RIGHT_INSPECT);
    if st != 0 || obj == 0 {
        return Err(b"it did not resolve");
    }
    let parsed = with_mapped(obj, |bytes| {
        // The object is page-sized; the manifest ends at its first NUL.
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        match core::str::from_utf8(&bytes[..end]) {
            Ok(text) => manifest::parse(text).map_err(|_| b"it did not parse".as_slice()),
            Err(_) => Err(b"it is not UTF-8".as_slice()),
        }
    });
    close(obj);
    parsed.unwrap_or(Err(b"it would not map"))
}

/// Reply to a resolve with a fresh read-only memory object holding `bytes`.
fn reply_with_object(serve_end: u64, request_id: u64, bytes: &[u8]) {
    // SAFETY: a plain anonymous object of `bytes.len()`.
    let obj = unsafe { syscall4(SYS_MEMORY_CREATE, bytes.len() as u64, 0, 0, 0) };
    if obj <= 0 {
        return reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::OutOfMemory);
    }
    // SAFETY: mapping an object this process just created, to fill it.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, obj as u64, 0, bytes.len() as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr <= 0 {
        close(obj as u64);
        return reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::OutOfMemory);
    }
    // SAFETY: `addr` maps at least `bytes.len()` writable bytes.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr as *mut u8, bytes.len()) };
    // Unmapped before the reply, as `device-mgr` does: the mapping holds its own reference, so
    // left behind it would pin every table ever served.
    // SAFETY: unmapping a range this process mapped moments ago and never reads again.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, bytes.len() as u64) };
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_MEMOBJ, bytes.len() as u32);
    reply_with_handle(serve_end, request_id, &body, obj as u64);
}

/// Reply to a resolve with a channel: a directory session, or a session endpoint. `client_end` is
/// consumed either way.
fn reply_channel(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    reply_with_handle(serve_end, request_id, &body, client_end)
}

/// Reply to a resolve with `ns` and the bytes of the suffix it stands for: the kernel continues the
/// resolve there, at `/` joined with the rest of the suffix (`rsproto-namespace-ops.md` § *The
/// `SUBNAMESPACE` body*). The handle sent is a duplicate with `LOOKUP`, which is all a continuation
/// needs, and `TRANSFER`, which moving it needs; this service keeps its own.
fn reply_subnamespace(serve_end: u64, request_id: u64, ns: u64, consumed: usize) {
    let mut body = [0u8; SUBNAMESPACE_PREFIX_LEN + 1];
    let Some(n) = u16::try_from(consumed).ok().and_then(|c| subnamespace_reply(&mut body, c, b"/")) else {
        return reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::InvalidArgument);
    };
    // SAFETY: duplicating a namespace handle this process holds, narrowed.
    let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, ns, RIGHT_LOOKUP | RIGHT_TRANSFER) };
    if dup <= 0 {
        return reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::KernelError);
    }
    reply_with_handle(serve_end, request_id, &body[..n], dup as u64);
}

/// A filesystem this service mounted.
struct Mount {
    /// Its device's registry id.
    device: u32,
    label: String,
    mode: Mode,
    /// A namespace of its own, with the server bound at `/`: what a `SUBNAMESPACE` reply hands on.
    ns: u64,
    /// The server's control channel: kept, for the unmount it will carry (C.5c), and waited on,
    /// since its closing means the server has gone.
    control: u64,
    /// The server's process handle.
    process: u64,
}

/// Spawn `path` with `control_end` moved into it. The process handle, or `None`.
fn spawn(root_ns: u64, path: &[u8], control_end: u64) -> Option<u64> {
    let (st, image) = lookup(root_ns, path, RIGHT_MAP_READ);
    if st != 0 || image == 0 {
        return None;
    }
    let args = SpawnArgs {
        image,
        handle_count: 1,
        move_mask: 1, // move the control endpoint to the child
        arg0: 0,
        handles: [control_end, 0, 0, 0],
        rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_TRANSFER | RIGHT_WAIT, 0, 0, 0],
        namespace: 0,
        syscaps: 0, // a resource server holds no ambient capabilities
    };
    // SAFETY: `args` is a valid `SpawnArgs` naming handles this process holds.
    let h = unsafe { syscall1(SYS_PROCESS_SPAWN, (&args as *const SpawnArgs) as u64) };
    close(image);
    (h > 0).then_some(h as u64)
}

/// Send a filesystem server its **setup message**: the device in `handles[0]`, and one flags byte.
/// Not rsproto-framed — it precedes everything else on the channel (`rsproto-wire-format.md`).
fn send_setup(control: u64, device: u64, flags: u8) -> bool {
    // SAFETY: REPLY_MSG/REPLY_HANDLES are valid buffers; single-threaded.
    unsafe {
        let Some(n) = fs_setup(&mut REPLY_MSG[PAYLOAD_OFF..], flags) else {
            return false;
        };
        REPLY_MSG[4..8].copy_from_slice(&(n as u32).to_le_bytes());
        REPLY_MSG[8] = 1;
        REPLY_HANDLES[0] = device;
        syscall5(
            SYS_CHANNEL_SEND,
            control,
            (&raw const REPLY_MSG) as u64,
            (&raw const REPLY_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Wait, bounded, for a server's `Meta::Ready` and take the endpoint it carries. `Err` with what
/// happened instead: nothing in time, an exit, a refusal, or something else.
fn wait_ready(control: u64) -> Result<u64, &'static [u8]> {
    if !wait_until(control, now_ns().saturating_add(READY_TIMEOUT_NS)) {
        return Err(b"it sent no Ready in time");
    }
    let m = match recv(control) {
        Ok(Some(m)) => m,
        Ok(None) => return Err(b"what it sent was not a message"),
        Err(()) => return Err(b"it exited before Ready"),
    };
    match (m.op, m.flags & RS_FLAG_ERROR != 0, m.handles.as_slice()) {
        (librsproto::OP_READY, false, &[endpoint]) => Ok(endpoint),
        (librsproto::OP_READY, true, _) => {
            m.handles.iter().for_each(|&h| close(h));
            Err(b"it refused the device")
        }
        _ => {
            m.handles.iter().for_each(|&h| close(h));
            Err(b"it sent something other than Ready")
        }
    }
}

/// **Flush the drive's volatile write cache** (`IoOpcode::Flush`): `true` once it completes. A
/// partition passes it to its disk, a RAM disk completes it at once, and AHCI sends `FLUSH CACHE
/// EXT` ([`drivers-and-irps.md`](../../../docs/architecture/drivers-and-irps.md) § Flush). Needs
/// `WRITE` on the node, which the device manager's duplicate carries.
fn flush(node: u64) -> bool {
    let op = IoOp { opcode: IO_OPCODE_FLUSH, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 0 };
    // SAFETY: `node` is a block device handle this process holds; `&op` is a valid `IoOp`.
    let po = unsafe { syscall2(SYS_IO_SUBMIT, node, (&op as *const IoOp) as u64) };
    po >= 0 && po_wait(po as u64).0 == 0
}

/// Mount `plan`'s device, whose node is `node`: spawn a server over it, and build its namespace.
/// The server gets a duplicate of the node; this service keeps its own, for the flush that ends an
/// unmount and for a later mount of the same device.
fn mount(root_ns: u64, plan: &Plan, node: u64) -> Result<Mount, &'static [u8]> {
    // **The server's handle, narrowed to the mode**: a read-only mount's server cannot write the
    // device even if it tried, whatever `ReadOnly` does above it.
    let write = if plan.mode == Mode::Rw { RIGHT_WRITE } else { 0 };
    // SAFETY: duplicating a node this process holds with DUPLICATE, narrowed.
    let device = unsafe { syscall2(SYS_HANDLE_DUPLICATE, node, RIGHT_READ | write | RIGHT_TRANSFER | RIGHT_DUPLICATE) };
    if device <= 0 {
        return Err(b"its node would not duplicate");
    }
    let device = device as u64;
    let Some((control, server_end)) = make_channel(4) else {
        close(device);
        return Err(b"no control channel");
    };
    let Some(process) = spawn(root_ns, FS_SERVER, server_end) else {
        close(device);
        close(control);
        return Err(b"fs-server-ext4 would not spawn");
    };
    // **From here a failure ends the server too** (PR #336 review, finding 3): it is terminated
    // and its handle closed, so a refused or unanswered mount leaves no server running over the
    // device and no handle held for one. A server that refused `Ready` has exited already, and
    // terminating it is a no-op.
    let abandon = |control: u64| {
        close(control);
        // SAFETY: the Process handle the spawn returned, with SIGNAL.
        unsafe { syscall1(SYS_PROCESS_TERMINATE, process) };
        close(process);
    };
    let flags = if plan.mode == Mode::Ro { FS_SETUP_READ_ONLY } else { 0 };
    if !send_setup(control, device, flags) {
        close(device);
        abandon(control);
        return Err(b"the device could not be handed to its server");
    }
    let endpoint = match wait_ready(control) {
        Ok(e) => e,
        Err(why) => {
            abandon(control);
            return Err(why);
        }
    };
    // SAFETY: register-only syscall; returns a fresh namespace handle with every right.
    let ns = unsafe { syscall0(SYS_NS_CREATE) };
    if ns <= 0 {
        close(endpoint);
        abandon(control);
        return Err(b"no namespace for it");
    }
    let ns = ns as u64;
    // SAFETY: a namespace this process created, a valid path, and an endpoint it holds.
    let bound = unsafe { syscall4(SYS_NS_BIND, ns, b"/".as_ptr() as u64, 1, endpoint) };
    // The binding holds its own reference to the endpoint.
    close(endpoint);
    if bound != 0 {
        close(ns);
        abandon(control);
        return Err(b"its server would not bind into its namespace");
    }
    Ok(Mount { device: plan.device, label: plan.label.clone(), mode: plan.mode, ns, control, process })
}

/// What a directory session lists.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Listing {
    /// `info`: the tables.
    Tables,
    /// `fs`: a subdirectory per mount.
    Mounts,
}

struct Service {
    root_ns: u64,
    serve_end: u64,
    /// The `block` subscription. **Held for the life of the service**: closing it frees the
    /// class, and the disks would be anyone's.
    subscription: u64,
    devices: Vec<Device>,
    /// Every device's node, by registry id: this service's own duplicate, from the replay. Kept,
    /// since an administrator may mount any of them and an unmount flushes the drive.
    nodes: Vec<(u32, u64)>,
    /// `init`'s mounts, as the table reports them. This service's own are [`mounted`](Self::mounted).
    init: Vec<Mounted>,
    /// Whether every one of `init`'s mounts was placed on a device ([`init_known`]). Without it
    /// this service mounts nothing and does not answer `InUse`: a device it took for free could
    /// be the running root.
    init_known: bool,
    mounted: Vec<Mount>,
    /// The buffer every device read passes through, kept past the boot's probe for a device read
    /// again ([`refresh`](Self::refresh)).
    scratch: Scratch,
    /// This service's ends of the session endpoints it has minted: resolves arriving here are
    /// [`session_only`].
    session_ends: Vec<u64>,
    /// This service's ends of the admin endpoints it has minted: any resolve on one opens an admin
    /// session.
    admin_ends: Vec<u64>,
    /// Admin sessions: channels carrying `Storage` requests.
    admin_sessions: Vec<u64>,
    dirs: Vec<(u64, Listing)>,
}

impl Service {
    /// **Read device `id` again**: what it holds, and how an ext4 was left. The boot's probe goes
    /// stale as soon as anything writes the device, whether this service's own mounts or a raw
    /// writer through the `disks` grant. So what reads a device's `found` refreshes it first: a
    /// table, and an administrator's mount (PR #336 review, finding 1).
    ///
    /// **This is the one place `found` changes after the boot.** The unmount reads the device for
    /// its own line with [`read_found`](Self::read_found), which stores nothing, so a table that
    /// failed to refresh would show it rather than being kept right by some other path.
    fn refresh(&mut self, id: u32) {
        let Some(found) = self.read_found(id) else {
            return;
        };
        if let Some(d) = self.devices.iter_mut().find(|d| d.record.id == id) {
            d.found = found;
        }
    }

    /// What device `id` holds now, read through its node; `None` if this service holds no node
    /// for it.
    fn read_found(&self, id: u32) -> Option<Found> {
        let &(_, node) = self.nodes.iter().find(|(d, _)| *d == id)?;
        let d = self.devices.iter().find(|d| d.record.id == id)?;
        Some(probe(&DeviceIo::new(node, &d.record, &self.scratch)))
    }

    /// [`refresh`](Self::refresh) every device nothing has mounted. A mounted one is left to its
    /// server: a writable mount's state reads "in use" and the table says nothing of it, and a
    /// read-only one's cannot change under it.
    fn refresh_unmounted(&mut self) {
        let mounted: Vec<u32> = self.all_mounts().iter().map(|m| m.device).collect();
        let ids: Vec<u32> = self.devices.iter().map(|d| d.record.id).filter(|id| !mounted.contains(id)).collect();
        for id in ids {
            self.refresh(id);
        }
    }

    /// Every mount, `init`'s and this service's, as the table reports them.
    fn all_mounts(&self) -> Vec<Mounted> {
        let mut all = self.init.clone();
        all.extend(self.mounted.iter().map(|m| Mounted {
            device: m.device,
            at: mounts::at(&m.label),
            by: By::Storage,
            mode: m.mode,
        }));
        all
    }

    /// Answer one forwarded resolve arriving on `from` — the endpoint bound at `/svc/storage`, or,
    /// when `session`, a session endpoint — replying on the endpoint it came from. `false` if that
    /// endpoint has gone.
    fn serve_resolve(&mut self, from: u64, session: bool) -> bool {
        let m = match recv_request(from) {
            Ok(Some(m)) => m,
            Ok(None) => return true,
            Err(()) => return false,
        };
        let asked = match parse_resolve_request(&m.body) {
            Some(r) if m.op == OP_NS_RESOLVE => suffix::parse(r.suffix),
            _ => Asked::Unknown,
        };
        let asked = if session { session_only(asked) } else { asked };
        match asked {
            Asked::Directory => self.open_dir(from, m.request_id, Listing::Tables),
            Asked::Mounts => self.open_dir(from, m.request_id, Listing::Mounts),
            Asked::File(name) => {
                self.refresh_unmounted();
                let mounts = self.all_mounts();
                let bytes = if name == "all" {
                    Some(table::all(&self.devices, &mounts))
                } else {
                    table::one(&self.devices, &mounts, name)
                };
                match bytes {
                    Some(b) => reply_with_object(from, m.request_id, &b),
                    None => reply_error(from, OP_NS_RESOLVE, m.request_id, KError::NotFound),
                }
            }
            Asked::Mount { label, consumed } => match self.mounted.iter().find(|x| x.label == label) {
                Some(x) => reply_subnamespace(from, m.request_id, x.ns, consumed),
                None => reply_error(from, OP_NS_RESOLVE, m.request_id, KError::NotFound),
            },
            Asked::SessionEndpoint => self.mint_session_endpoint(from, m.request_id),
            Asked::AdminEndpoint => self.mint_admin_endpoint(from, m.request_id),
            Asked::Unknown => reply_error(from, OP_NS_RESOLVE, m.request_id, KError::NotFound),
        }
        true
    }

    /// Answer with a forwarding endpoint of this service's own, on which only the tables and the
    /// filesystems are answered. **Minted only on the root endpoint**: [`session_only`] refuses the
    /// suffix on a session endpoint, so its holder cannot make more.
    fn mint_session_endpoint(&mut self, reply_to: u64, request_id: u64) {
        if self.session_ends.len() >= MAX_SESSION_ENDPOINTS {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
        }
        let Some((client_end, ours)) = make_channel(4) else {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        if reply_channel(reply_to, request_id, client_end) {
            self.session_ends.push(ours);
            kprint(b"storage-service: a session endpoint minted\n");
        } else {
            close(ours);
        }
    }

    /// Answer with a forwarding endpoint of this service's own, on which any resolve opens an admin
    /// session. **Minted only on the root endpoint**: [`session_only`] refuses the suffix on a
    /// session endpoint, so a session cannot reach mounting at all. Who holds one is the view
    /// broker's `storage` grant to decide (C.6).
    fn mint_admin_endpoint(&mut self, reply_to: u64, request_id: u64) {
        if self.admin_ends.len() >= MAX_ADMIN_ENDPOINTS {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
        }
        let Some((client_end, ours)) = make_channel(4) else {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        if reply_channel(reply_to, request_id, client_end) {
            self.admin_ends.push(ours);
            kprint(b"storage-service: an admin endpoint minted\n");
        } else {
            close(ours);
        }
    }

    /// A resolve on an admin endpoint: whatever its suffix, answer with an admin session. `false`
    /// if the endpoint has gone.
    fn serve_admin_endpoint(&mut self, from: u64) -> bool {
        let m = match recv_request(from) {
            Ok(Some(m)) => m,
            Ok(None) => return true,
            Err(()) => return false,
        };
        if m.op != OP_NS_RESOLVE {
            reply_error(from, m.op, m.request_id, KError::Unsupported);
            return true;
        }
        if self.admin_sessions.len() >= MAX_ADMIN_SESSIONS {
            reply_error(from, OP_NS_RESOLVE, m.request_id, KError::WouldBlock);
            return true;
        }
        let Some((client_end, ours)) = make_channel(4) else {
            reply_error(from, OP_NS_RESOLVE, m.request_id, KError::KernelError);
            return true;
        };
        if reply_channel(from, m.request_id, client_end) {
            self.admin_sessions.push(ours);
        } else {
            close(ours);
        }
        true
    }

    /// One `Storage` request on admin session `i`.
    fn serve_admin(&mut self, i: usize) {
        let ch = self.admin_sessions[i];
        let m = match recv_request(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                close(ch);
                self.admin_sessions.remove(i);
                return;
            }
        };
        let refuse = |err: KError, why: &[u8]| {
            let mut body = [0u8; librsproto::error::ERROR_BODY_LEN + 96];
            let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, why).unwrap_or(0);
            let _ = send(ch, m.op, m.request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &body[..n], &[]);
        };
        match m.op {
            OP_STORAGE_MOUNT => {
                let Some((device, label)) = parse_mount(&m.body) else {
                    return refuse(KError::InvalidArgument, b"a malformed Mount");
                };
                let (Ok(device), Ok(label)) = (core::str::from_utf8(device), core::str::from_utf8(label)) else {
                    return refuse(KError::InvalidArgument, b"a name or label that is not UTF-8");
                };
                match self.mount_explicit(device, label) {
                    Ok(label) => {
                        let _ = send(ch, m.op, m.request_id, RS_FLAG_REPLY, label.as_bytes(), &[]);
                    }
                    Err((err, why)) => refuse(err, why),
                }
            }
            OP_STORAGE_UNMOUNT => {
                let Ok(label) = core::str::from_utf8(&m.body) else {
                    return refuse(KError::InvalidArgument, b"a label that is not UTF-8");
                };
                match self.unmount(label) {
                    Ok(()) => {
                        let _ = send(ch, m.op, m.request_id, RS_FLAG_REPLY, &[], &[]);
                    }
                    Err((err, why)) => refuse(err, why),
                }
            }
            OP_STORAGE_IN_USE => {
                // **Not answered unless `init`'s mounts are known**: an answer would leave out a
                // root it could not place, and the view broker would hand that disk over raw. The
                // broker refuses `disks` when this goes unanswered.
                if !self.init_known {
                    return refuse(KError::NoAccess, b"init's mounts are not all known, so what is in use cannot be said");
                }
                let ids = in_use(&self.devices, &self.all_mounts());
                let mut body = alloc::vec![0u8; 4 + 4 * ids.len()];
                match build_in_use(&mut body, &ids) {
                    Some(n) => {
                        let _ = send(ch, m.op, m.request_id, RS_FLAG_REPLY, &body[..n], &[]);
                    }
                    None => refuse(KError::KernelError, b"the list would not encode"),
                }
            }
            _ => refuse(KError::Unsupported, b"not a Storage request"),
        }
    }

    /// An administrator's `Mount`: the label it went under, or why not.
    fn mount_explicit(&mut self, device: &str, label: &str) -> Result<String, (KError, &'static [u8])> {
        self.refresh_unmounted();
        let taken: Vec<String> = self.mounted.iter().map(|x| x.label.clone()).collect();
        let room = self.mounted.len() < MAX_MOUNTS;
        let plan = explicit(&self.devices, &self.all_mounts(), &taken, room, self.init_known, device, label)
            .map_err(|r| (r.kerror(), r.why()))?;
        let Some(&(_, node)) = self.nodes.iter().find(|(id, _)| *id == plan.device) else {
            return Err((KError::NotFound, b"this service holds no node for it"));
        };
        let m = mount(self.root_ns, &plan, node).map_err(|why| (KError::IoError, why))?;
        Line::new().s(b"storage-service: mounted ").untrusted(m.label.as_bytes()).s(b" (rw), as asked").end();
        self.mounted.push(m);
        Ok(plan.label)
    }

    /// **Unmount `label`: the chain**, each link only once the one before it held. An `Err` names
    /// the link that refused. A refusal before the server is told leaves the mount as it was.
    fn unmount(&mut self, label: &str) -> Result<(), (KError, &'static [u8])> {
        let Some(i) = self.mounted.iter().position(|x| x.label == label) else {
            return Err((KError::NotFound, b"nothing is mounted with that label"));
        };
        // 1. **The label leaves `fs`**: taken out of the list, so a resolve under it is `NotFound`
        //    from here, and put back if a link before the server's refuses. This service is one
        //    thread, so nothing is answered in between anyway; taking it out says what the chain
        //    assumes.
        let x = self.mounted.remove(i);
        let root = b"/";
        // 2. **Every dirty file written back**, whether or not anything still holds it.
        // SAFETY: a namespace handle this process holds, and a valid path.
        let synced = unsafe { syscall3(SYS_NS_SYNC, x.ns, root.as_ptr() as u64, root.len() as u64) };
        if synced < 0 {
            self.mounted.insert(i, x);
            return Err((KError::IoError, b"its files could not all be written back"));
        }
        // 3. **Refused while a file is held**: a mapping or a handle could write after the
        //    filesystem is marked clean. Asked after the sync, which let go of every dirty pin a
        //    write-back could clean, so what is left is someone's — **once a finished IRP has let
        //    go of its file too**. The kernel frees finished IRPs before it counts, but one whose
        //    completion is still running on another CPU holds its file for that moment
        //    (`sys_ns_held`, `syscall-abi.md`), so a count is asked again after a short park
        //    before it is believed. A real holder is still holding a few milliseconds later.
        let mut held = 0;
        for attempt in 0..HELD_ASKS {
            if attempt > 0 {
                park(HELD_PARK_NS);
            }
            // SAFETY: as above.
            held = unsafe { syscall3(SYS_NS_HELD, x.ns, root.as_ptr() as u64, root.len() as u64) };
            if held == 0 {
                break;
            }
        }
        if held != 0 {
            Line::new()
                .s(b"storage-service: ")
                .untrusted(x.label.as_bytes())
                .s(b" is in use: ")
                .i(held as i64)
                .s(b" file(s) still open or mapped")
                .end();
            self.mounted.insert(i, x);
            return Err((KError::WouldBlock, b"a file on it is still open or mapped"));
        }
        // 4. **The server records the filesystem clean and exits.** From here the mount is gone
        //    whatever the answer: a server that could not record it exits too.
        let told = send(x.control, OP_UNMOUNT, 1, 0, &[], &[]);
        let answer = if told && wait_until(x.control, now_ns().saturating_add(READY_TIMEOUT_NS)) {
            recv(x.control).ok().flatten()
        } else {
            None
        };
        let recorded = answer.as_ref().is_some_and(|a| a.op == OP_UNMOUNT && a.flags & RS_FLAG_ERROR == 0);
        if let Some(a) = answer {
            a.handles.iter().for_each(|&h| close(h));
        }
        close(x.ns);
        close(x.control);
        close(x.process);
        if !recorded {
            return Err((KError::IoError, b"its server did not record it clean, and the filesystem was not left clean"));
        }
        // 5. **The drive's cache written to its medium**: a writable mount's writes are only
        //    durable once it is. A read-only mount wrote nothing.
        if x.mode == Mode::Rw
            && let Some(&(_, node)) = self.nodes.iter().find(|(id, _)| *id == x.device)
            && !flush(node)
        {
            return Err((KError::IoError, b"the drive's cache could not be flushed"));
        }
        // 6. The namespace went with the handle closed above. **What is said of the filesystem is
        //    what the device says**, read again now: a read-only mount wrote nothing, so one it
        //    found not clean is not clean still, whatever its server answered.
        let clean = matches!(self.read_found(x.device), Some(Found::Ext4 { clean: Some(true), .. }));
        let mut l = Line::new();
        l.s(b"storage-service: unmounted ").untrusted(x.label.as_bytes());
        l.s(if clean { b", left clean".as_slice() } else { b", not left clean" });
        if x.mode == Mode::Ro {
            l.s(b" (read-only, so as it was found)");
        }
        l.end();
        Ok(())
    }

    fn open_dir(&mut self, reply_to: u64, request_id: u64, listing: Listing) {
        if self.dirs.len() >= MAX_DIRS {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
        }
        let Some((client_end, ours)) = make_channel(4) else {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        if reply_channel(reply_to, request_id, client_end) {
            self.dirs.push((ours, listing));
        } else {
            close(ours);
        }
    }

    fn serve_dir(&mut self, i: usize) {
        let (ch, listing) = self.dirs[i];
        let m = match recv_request(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                close(ch);
                self.dirs.remove(i);
                return;
            }
        };
        let cursor = match parse_read_dir_request(&m.body) {
            Some(r) if m.op == OP_FILE_READ_DIR => r.cursor as usize,
            _ => return reply_error(ch, m.op, m.request_id, KError::Unsupported),
        };
        let mounts = self.all_mounts();
        // `(name, kind, size)` for every entry, in order.
        let entries: Vec<(String, u8, u64)> = match listing {
            Listing::Tables => table::entries(&self.devices)
                .into_iter()
                .map(|name| {
                    let stem = name.strip_suffix(".tsm").unwrap_or(&name);
                    let size = if stem == "all" {
                        table::all(&self.devices, &mounts).len()
                    } else {
                        table::one(&self.devices, &mounts, stem).map_or(0, |b| b.len())
                    };
                    (name, DIRENT_KIND_FILE, size as u64)
                })
                .collect(),
            Listing::Mounts => self.mounted.iter().map(|x| (x.label.clone(), DIRENT_KIND_DIR, 0)).collect(),
        };
        let mut out = [0u8; MSG_LEN - PAYLOAD_OFF - 64];
        let Some(mut w) = DirReplyWriter::new(&mut out) else {
            return reply_error(ch, m.op, m.request_id, KError::KernelError);
        };
        let mut i = cursor;
        while i < entries.len() {
            let (name, kind, size) = &entries[i];
            if !w.push(i as u32 + 1, *kind, 0, *size, 0, name.as_bytes()) {
                break;
            }
            i += 1;
        }
        let next = if i >= entries.len() { 0 } else { i as u64 };
        let n = w.finish(next);
        let _ = send(ch, OP_FILE_READ_DIR, m.request_id, RS_FLAG_REPLY, &out[..n], &[]);
    }

    /// A mount's control channel: its server sends nothing on it unasked, so a message is closed
    /// and dropped, and its closing means the server has gone. **The mount goes with it**: a label
    /// whose namespace forwards to a server that no longer answers would fail every resolve under
    /// it, and saying "gone" is better than saying "there" and failing.
    fn serve_control(&mut self, i: usize) {
        loop {
            match recv(self.mounted[i].control) {
                Ok(Some(m)) => m.handles.iter().for_each(|&h| close(h)),
                Ok(None) => return,
                Err(()) => {
                    let x = self.mounted.remove(i);
                    Line::new()
                        .s(b"storage-service: the server for ")
                        .untrusted(x.label.as_bytes())
                        .s(b" exited; it is no longer mounted")
                        .end();
                    close(x.ns);
                    close(x.control);
                    close(x.process);
                    return;
                }
            }
        }
    }

    /// The subscription: the manager has nothing to send after `Settled` until Phase 6 gives it
    /// an event source. Its handles are closed rather than leaked, and the manager going away is
    /// said once.
    fn serve_subscription(&mut self) {
        loop {
            match recv(self.subscription) {
                Ok(Some(m)) => {
                    Line::new().s(b"storage-service: the device manager sent op ").u(m.op as u64).s(b" after Settled; ignored").end();
                    m.handles.iter().for_each(|&h| close(h));
                }
                Ok(None) => return,
                Err(()) => {
                    kprint(b"storage-service: the device manager has gone; the disks held are kept\n");
                    close(self.subscription);
                    self.subscription = 0;
                    return;
                }
            }
        }
    }
}

/// Say what one device holds, and whose it is. How an ext4 was left is not said while it is
/// mounted writable, for the reason the table's `clean` is `Null` then: its state says "in use".
fn report(d: &Device, mounts: &[Mounted]) {
    let r = &d.record;
    let mount = mounts.iter().find(|m| m.device == r.id);
    let mut line = Line::new();
    line.s(b"storage-service: ").s(table::name(r).as_bytes()).s(b" (");
    line.s(match r.kind() {
        DeviceKind::Disk => b"disk".as_slice(),
        DeviceKind::Partition => b"partition",
        DeviceKind::RamDisk => b"ramdisk",
        _ => b"device",
    });
    if !r.name().is_empty() {
        line.s(b" ").untrusted(r.name());
    }
    line.s(b"): ");
    match &d.found {
        Found::Ext4 { label, clean } => {
            line.s(b"ext4");
            if !label.is_empty() {
                line.s(b" '").untrusted(label.as_bytes()).s(b"'");
            }
            if !mount.is_some_and(|m| m.mode == Mode::Rw) {
                line.s(match clean {
                    Some(true) => b", left clean".as_slice(),
                    Some(false) => b", not left clean",
                    None => b", state unreadable",
                });
            }
        }
        Found::Fat { label } => {
            line.s(b"fat");
            if !label.is_empty() {
                line.s(b" '").untrusted(label.as_bytes()).s(b"'");
            }
        }
        Found::Nothing => {
            line.s(b"no filesystem");
        }
    }
    if let Some(m) = mount {
        line.s(b"; ").s(if m.by == By::Init { b"init's".as_slice() } else { b"mounted" });
        line.s(b" at ").untrusted(m.at.as_bytes()).s(if m.mode == Mode::Ro { b" (ro)".as_slice() } else { b" (rw)" });
    }
    line.end();
}

/// Send `init` `Meta::Ready`, naming this server and carrying the forwarding endpoint's client end.
fn send_ready(control: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let Some(n) = librsproto::meta::ready(&mut body, b"storage-service") else {
        return false;
    };
    send(control, librsproto::OP_READY, 0, 0, &body[..n], &[client_end])
}

/// Say, in place of `Meta::Ready`, that there is nothing to serve — no handle, and `init` prints
/// `why` (`rsproto-wire-format.md` § Meta::Ready) — then exit.
fn refuse(control: u64, err: KError, why: &[u8]) -> ! {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN + 96];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, why).unwrap_or(0);
    let _ = send(control, librsproto::OP_READY, 0, RS_FLAG_ERROR, &body[..n], &[]);
    exit(1);
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = the inherited root namespace,
/// `rdx` = the control channel `init` installed, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"storage-service: up\n");
    let (subscription, held) = match take_block(root_ns) {
        Ok(taken) => taken,
        Err(why) => refuse(control, KError::NotFound, why),
    };
    let Some(scratch) = Scratch::new() else {
        refuse(control, KError::OutOfMemory, b"no scratch buffer to read the disks through");
    };

    let mut devices = Vec::new();
    let mut tables = Vec::new();
    let mut nodes = Vec::new();
    for (record, node) in held {
        let io = DeviceIo::new(node, &record, &scratch);
        if matches!(record.kind(), DeviceKind::Disk | DeviceKind::RamDisk)
            && let Some(t) = read_table(&io, record.id)
        {
            tables.push(t);
        }
        devices.push(Device { found: probe(&io), record });
        nodes.push((record.id, node));
    }
    let records: Vec<DeviceRecord> = devices.iter().map(|d| d.record).collect();

    let init_list: Option<Vec<InitMount>> = match read_manifest(root_ns) {
        Ok(m) => Some(init_mounts(&m, &records, &tables)),
        Err(why) => {
            Line::new()
                .s(b"storage-service: init.toml could not be read (")
                .s(why)
                .s(b"), so which devices are init's cannot be said")
                .end();
            None
        }
    };
    let mut init = Vec::new();
    for m in init_list.iter().flatten() {
        match m.device {
            Some(device) => init.push(Mounted { device, at: m.mount_point.clone(), by: By::Init, mode: m.mode }),
            None => Line::new()
                .s(b"storage-service: init's ")
                .s(m.mount_point.as_bytes())
                .s(b" names ")
                .untrusted(m.source.as_bytes())
                .s(b", which matched no device")
                .end(),
        }
    }
    let known = init_known(init_list.as_deref());
    if !known {
        kprint(b"storage-service: init's mounts are not all known, so this service mounts nothing and does not say what is in use\n");
    }
    let live = live_boot(init_list.as_deref().unwrap_or(&[]), &records);

    // Mount what can be served. A mount hands its server a duplicate of the node; the service
    // keeps every node, for an administrator's mount and for an unmount's flush.
    let mut plan = automount(&devices, &init, live, known);
    plan.truncate(MAX_MOUNTS);
    let mut mounted = Vec::new();
    for &(id, node) in &nodes {
        let Some(p) = plan.iter().find(|p| p.device == id) else {
            continue;
        };
        match mount(root_ns, p, node) {
            Ok(m) => mounted.push(m),
            Err(why) => Line::new()
                .s(b"storage-service: ")
                .untrusted(p.label.as_bytes())
                .s(b" did not mount: ")
                .s(why)
                .end(),
        }
    }

    let mut s = Service {
        root_ns,
        serve_end: 0,
        subscription,
        devices,
        nodes,
        init,
        init_known: known,
        mounted,
        scratch,
        session_ends: Vec::new(),
        admin_ends: Vec::new(),
        admin_sessions: Vec::new(),
        dirs: Vec::new(),
    };
    let mounts = s.all_mounts();
    Line::new().s(b"storage-service: ").u(s.devices.len() as u64).s(b" block device(s)").end();
    for d in &s.devices {
        report(d, &mounts);
    }
    if live {
        kprint(b"storage-service: a live boot: the root is on a RAM disk, so the machine's disks mount read-only\n");
    } else {
        kprint(b"storage-service: not a live boot\n");
    }

    let Some((client_end, serve_end)) = make_channel(4) else {
        kprint(b"storage-service: channel create FAIL\n");
        exit(1);
    };
    if !send_ready(control, client_end) {
        kprint(b"storage-service: Ready send FAIL\n");
        exit(1);
    }
    s.serve_end = serve_end;
    loop {
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots: the endpoint, the subscription, and at
        // most each kind's bound more — each refuses past it.
        let waited = unsafe {
            let mut n = 0usize;
            let mut push = |h: u64| {
                if h != 0 && n < MAX_WAIT_HANDLES {
                    WAIT_HANDLES[n] = h;
                    n += 1;
                }
            };
            push(s.serve_end);
            push(s.subscription);
            for &e in &s.session_ends {
                push(e);
            }
            for x in &s.mounted {
                push(x.control);
            }
            for &e in &s.admin_ends {
                push(e);
            }
            for &a in &s.admin_sessions {
                push(a);
            }
            for &(d, _) in &s.dirs {
                push(d);
            }
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, n as u64, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        for j in 0..waited.max(0) as usize {
            // SAFETY: `waited` records were written; the handle is the first word of each.
            let h = unsafe {
                let off = j * 24;
                u64::from_le_bytes(WAIT_RESULTS[off..off + 8].try_into().unwrap_or([0; 8]))
            };
            if h == s.serve_end {
                // A root endpoint with no peer is the end of this service, not a message to skip:
                // the kernel keeps a peer-closed channel signalled, so going round again would spin
                // a CPU (PR #333 review, finding 1, in `device-mgr`).
                if !s.serve_resolve(h, false) {
                    kprint(b"storage-service: forwarding endpoint closed\n");
                    exit(1);
                }
            } else if h == s.subscription {
                s.serve_subscription();
            } else if let Some(i) = s.session_ends.iter().position(|&e| e == h) {
                // Every holder of this endpoint has let it go, its bindings included.
                if !s.serve_resolve(h, true) {
                    close(h);
                    s.session_ends.remove(i);
                }
            } else if let Some(i) = s.mounted.iter().position(|x| x.control == h) {
                s.serve_control(i);
            } else if let Some(i) = s.admin_ends.iter().position(|&e| e == h) {
                // Every holder of this endpoint has let it go, its bindings included.
                if !s.serve_admin_endpoint(h) {
                    close(h);
                    s.admin_ends.remove(i);
                }
            } else if let Some(i) = s.admin_sessions.iter().position(|&a| a == h) {
                s.serve_admin(i);
            } else if let Some(i) = s.dirs.iter().position(|&(d, _)| d == h) {
                s.serve_dir(i);
            }
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"storage-service: PANIC\n");
    exit(1);
}
