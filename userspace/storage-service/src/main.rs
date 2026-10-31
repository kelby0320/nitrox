//! `storage-service` — the event loop. Everything that decides is in the library half
//! ([`storage_service`]); this file is what cannot be tested on the host.
//!
//! 1. **Own `block`.** Resolve `/svc/devices/block`: the device manager has already queued an
//!    `Arrived` per disk, partition and RAM disk, each carrying this service's own duplicate of the
//!    device's node, then `Settled`. `service-mgr` spawns this service straight after the manager,
//!    so it owns the class from boot on and nothing else can take the disks.
//! 2. **Read what is there**: each device as its server would read it, and each disk's partition
//!    table, for `init.toml`'s UUID sources.
//! 3. **Read `init.toml`**, and match each of `init`'s mounts to its device. Those are reported
//!    and never mounted here. A root on a RAM disk makes this a live boot.
//! 4. **Mount what it can serve**: each gets the server for its kind spawned over it and a
//!    namespace of its own with that server bound at `/`, and is named by its label.
//! 5. **Serve.** Mint a forwarding endpoint and answer `Meta::Ready`; `service-mgr` binds it at
//!    `/svc/storage`. `info` is a directory session, `info/<name>.tsm` a table as a fresh
//!    read-only memory object, `fs` a directory of labels, and `fs/<label>/…` a `SUBNAMESPACE`
//!    reply: the kernel continues the resolve in the mount's namespace, so nothing passes through
//!    this service. `session-endpoint` mints an endpoint on which only `info`, `fs`, `info/media`
//!    and `info/watch` are answered; `admin-endpoint` one on which any resolve opens an **admin
//!    session** for `Mount`, `Unmount`, `Reread` and `InUse`. A **media session** carries `Eject`
//!    alone; a **watch** is only pinged when the mounts change ([`storage_service::watch`]).
//!
//! **An unmount is a chain**: the label leaves `fs`, every dirty file is written back
//! (`sys_ns_sync`), the unmount is refused if a file is still held (`sys_ns_held`), the server
//! records the filesystem clean and exits (`Meta::Unmount`), the drive's cache is flushed, and
//! the namespace is dropped.
//!
//! **A shutdown unmounts everything.** On `CTRL_OP_SHUTDOWN` (sent only by a shutdown, since this
//! service is `essential`) it runs the chain on every mount, last mounted first, **without the
//! held check**, by the maintainer's call: a write made after the sync is lost, as at a power
//! cut, and every filesystem is left clean. Then it exits.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use fs_server_ext4::{BlockReader, FsError};
use libinittoml::manifest::{self, Mode};
use libkern::abi::{IO_OPCODE_EJECT, IO_OPCODE_FLUSH, IO_OPCODE_READ, IoOp};
use libkern::debug::Line;
use libkern::device::{DeviceKind, DeviceRecord};
use libkern::*;
use librsproto::devices::{OP_DEVICES_ARRIVED, OP_DEVICES_DEPARTED, OP_DEVICES_SETTLED, parse_arrived, parse_departed};
use librsproto::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE, parse_read_dir_request};
use librsproto::meta::{FS_SETUP_READ_ONLY, fs_setup};
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, OBJECT_KIND_MEMOBJ, RESOLVE_REPLY_LEN, SUBNAMESPACE_PREFIX_LEN, parse_resolve_request,
    resolve_reply, subnamespace_reply,
};
use librsproto::storage::{
    OP_STORAGE_CHANGED, OP_STORAGE_EJECT, OP_STORAGE_IN_USE, OP_STORAGE_MOUNT, OP_STORAGE_REREAD, OP_STORAGE_UNMOUNT,
    build_in_use, build_mounted, parse_mount,
};
use librsproto::{OP_FILE_READ_DIR, OP_NS_RESOLVE, OP_UNMOUNT, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};
use storage_service::fit::{SCRATCH, ScratchRead, scratch_read};
use storage_service::mounts::{self, Plan, Reread, RereadRefusal, automount, explicit, in_use};
use storage_service::probe::{Found, probe_record};
use storage_service::sources::{DiskTable, InitMount, TableEntry, init_known, init_mounts, live_boot};
use storage_service::suffix::{self, Asked, session_only};
use storage_service::table::{self, By, Device, Mounted};
use storage_service::watch::{Sent, Watches};

mod mounting;
use mounting::*;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
const SUBSCRIPTION: &[u8] = b"/svc/devices/block";
/// Where `init`'s manifest is, in the initramfs this service inherits.
const INIT_TOML: &[u8] = b"/initramfs/etc/init.toml";
/// How long one message of the replay may take. The manager queues the whole replay before the
/// resolve completes, so this bounds a manager that broke that rule.
const REPLAY_WAIT_NS: u64 = 2_000_000_000;
/// Session endpoints at once: one per login supervisor, the rest headroom.
const MAX_SESSION_ENDPOINTS: usize = 4;
/// Mounts at once. Each keeps its server's control channel in the wait set, to see it exit.
const MAX_MOUNTS: usize = 8;
/// Admin endpoints at once. The view broker asks for one at boot; the second is headroom.
const MAX_ADMIN_ENDPOINTS: usize = 2;
/// Admin sessions open at once: a `disk --mount` or `--unmount` is one, briefly.
const MAX_ADMIN_SESSIONS: usize = 4;
/// Media sessions at once: each is held for one `Eject`. A watch takes no slot.
const MAX_MEDIA_SESSIONS: usize = 2;
/// Directory sessions at once: the wait set less every other kind's bound, the endpoint, the
/// subscription and the control channel.
const MAX_DIRS: usize = MAX_WAIT_HANDLES
    - 3
    - MAX_SESSION_ENDPOINTS
    - MAX_MOUNTS
    - MAX_ADMIN_ENDPOINTS
    - MAX_ADMIN_SESSIONS
    - MAX_MEDIA_SESSIONS;
/// How long a filesystem server may take to answer `Meta::Ready`: `init`'s bound for its own.
const READY_TIMEOUT_NS: u64 = 30_000_000_000;
/// How many times an unmount asks whether a file is held before it believes the answer, and how
/// long it parks between: long enough for an IRP's completion on another CPU to finish.
const HELD_ASKS: u32 = 3;
const HELD_PARK_NS: u64 = 5_000_000;

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

fn wait_until(h: u64, deadline: u64) -> bool {
    let mut result = [0u8; 24];
    // SAFETY: one handle, and room for its one result.
    unsafe { syscall4(SYS_WAIT, (&raw const h) as u64, 1, result.as_mut_ptr() as u64, deadline) == 1 }
}

/// Park this thread for `ns`, in a wait on no handles, which the kernel never refuses.
fn park(ns: u64) {
    // SAFETY: no handles and no results, so the kernel reads and writes neither.
    unsafe { syscall4(SYS_WAIT, 0, 0, 0, now_ns().saturating_add(ns)) };
}

/// Wait for a pending operation and return its `(status, value)`.
/// Only once done: a full waiter queue refuses a wait for now, while the I/O may run on.
fn po_wait(po: u64) -> (i64, u64) {
    let mut r = [0u8; 24];
    let done = loop {
        // SAFETY: one handle, and room for its one result.
        let waited = unsafe { syscall4(SYS_WAIT, (&raw const po) as u64, 1, r.as_mut_ptr() as u64, u64::MAX) };
        if waited != KError::OutOfMemory.as_i32() as i64 {
            break waited == 1;
        }
        park(HELD_PARK_NS);
    };
    let word = |off: usize| u64::from_le_bytes(r[off..off + 8].try_into().unwrap_or([0; 8]));
    // The status is an `i32` beside a zero word: read whole, `AlreadyExists` would never match.
    let (status, value) = (word(8) as u32 as i32 as i64, word(16));
    close(po);
    if done { (status, value) } else { (-1, 0) }
}

fn send(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handles: &[u64]) -> bool {
    send_raw(ch, op, request_id, flags, body, handles) == 0
}

/// [`send`], answering the syscall's own result. **Through a whole message's buffer**: the kernel
/// copies a full message from the pointer it is given, so a smaller buffer would be read past.
fn send_raw(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handles: &[u64]) -> i64 {
    let (mut msg, mut carried) = ([0u8; MSG_LEN], [0u64; 8]);
    let Some(rs_len) = encode(&mut msg[PAYLOAD_OFF..], op, request_id, flags, body, handles.len() as u16) else {
        return KError::InvalidArgument.as_i32() as i64;
    };
    msg[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
    msg[8] = handles.len() as u8;
    carried[..handles.len()].copy_from_slice(handles);
    let (msg, carried) = (msg.as_ptr() as u64, carried.as_ptr() as u64);
    // SAFETY: a whole message, and the handles it carries, which this process holds.
    unsafe { syscall5(SYS_CHANNEL_SEND, ch, msg, carried, handles.len() as u64, SENDMODE_NOBLOCK) }
}

/// **Ping a watch**: a bare `Changed`, never blocking. A full queue is a ping already waiting, and
/// a refused send is a client gone.
fn ping(ch: u64) -> Sent {
    match send_raw(ch, OP_STORAGE_CHANGED, 0, 0, &[], &[]) {
        0 => Sent::Queued,
        r if r == KError::WouldBlock.as_i32() as i64 => Sent::Full,
        _ => Sent::Gone,
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
/// moved on success and closed otherwise. A forwarded resolve waits for its answer with no
/// deadline, so a silently failed reply would block the caller for good (a namespace lacking
/// `TRANSFER` fails every move). `false` if the reply did not go.
fn reply_with_handle(serve_end: u64, request_id: u64, body: &[u8], handle: u64) -> bool {
    if send(serve_end, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, body, &[handle]) {
        return true;
    }
    close(handle);
    kprint(b"storage-service: a reply could not be sent, so it was refused instead\n");
    reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::KernelError);
    false
}

/// Write back every dirty file of the mount in `ns` (`sys_ns_sync`), held or not. `false` if one
/// failed.
fn sync_mount(ns: u64) -> bool {
    let root = b"/";
    // SAFETY: a namespace handle this process holds, and a valid path.
    unsafe { syscall3(SYS_NS_SYNC, ns, root.as_ptr() as u64, root.len() as u64) >= 0 }
}

/// **How many of the mount's files something still holds** (`sys_ns_held`), asked after a sync.
/// An IRP whose completion is still running on another CPU holds its file for that moment
/// (`syscall-abi.md`), so a nonzero count is asked again after a park before it is believed; a
/// real holder still holds. A nonzero count is said, naming `label`.
fn held_files(label: &str, ns: u64) -> i64 {
    let root = b"/";
    let mut held = 0;
    for attempt in 0..HELD_ASKS {
        if attempt > 0 {
            park(HELD_PARK_NS);
        }
        // SAFETY: a namespace handle this process holds, and a valid path.
        held = unsafe { syscall3(SYS_NS_HELD, ns, root.as_ptr() as u64, root.len() as u64) };
        if held == 0 {
            break;
        }
    }
    if held != 0 {
        Line::new()
            .s(b"storage-service: ")
            .untrusted(label.as_bytes())
            .s(b" is in use: ")
            .i(held)
            .s(b" file(s) still open or mapped")
            .end();
    }
    held
}

fn make_channel(depth: u64) -> Option<(u64, u64)> {
    let (mut a, mut b) = (0u64, 0u64);
    // SAFETY: valid writable out-params.
    let r = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut a) as u64, (&raw mut b) as u64, depth, 0) };
    (r == 0).then_some((a, b))
}

struct Message {
    op: u16,
    flags: u32,
    request_id: u64,
    body: Vec<u8>,
}

/// Receive one message on `ch`, and the handles it carried, closed with the [`libkern::Received`]
/// unless taken. `Ok(None)` if nothing was queued, `Err(())` if the peer has gone.
fn recv(ch: u64) -> Result<Option<(Message, libkern::Received)>, ()> {
    let mut buf = [0u8; MSG_LEN];
    let sent = match channel_recv(ch, &mut buf) {
        Ok(sent) => sent,
        Err(rr) if rr == KError::PeerClosed.as_i32() as i64 => return Err(()),
        Err(_) => return Ok(None),
    };
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let msg = &buf[PAYLOAD_OFF..PAYLOAD_OFF + len.min(MSG_LEN - PAYLOAD_OFF)];
    // A message that does not decode closes what it carried, as `sent` drops.
    Ok(decode(msg)
        .ok()
        .map(|m| (Message { op: m.op, flags: m.flags, request_id: m.request_id, body: m.body.to_vec() }, sent)))
}

/// Receive one message on `ch` and close whatever handles came with it: nothing a client sends
/// this service carries a handle it wants, and one kept would be kept for good.
fn recv_request(ch: u64) -> Result<Option<Message>, ()> {
    Ok(recv(ch)?.map(|(m, _)| m))
}

fn lookup(ns: u64, path: &[u8], rights: u64) -> (i64, u64) {
    let (status, handle) = libfs::lookup_wait(ns, path, rights);
    (status.into(), handle)
}

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
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, info.size) };
    Some(out)
}

/// The one memory object every device read passes through, mapped for the life of the service.
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
            let ScratchRead { start, intra, want, span } =
                scratch_read(offset + done as u64, buf.len() - done, self.sector).ok_or(FsError::Io)?;
            let op = IoOp {
                opcode: IO_OPCODE_READ,
                flags: 0,
                buffer: self.scratch.mem,
                buf_offset: 0,
                offset: start,
                length: span,
            };
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
            // SAFETY: `scratch.addr` maps SCRATCH read-write bytes for the life of the service,
            // `scratch_read` keeps `intra + want <= span <= SCRATCH`, and the device has moved all
            // `span`; single-threaded, so nothing else holds a reference.
            let src = unsafe { core::slice::from_raw_parts(self.scratch.addr as *const u8, span as usize) };
            buf[done..done + want].copy_from_slice(&src[intra..intra + want]);
            done += want;
        }
        Ok(())
    }
}

/// Take `block`: every device queued before `Settled`. `Err` with why if the class was not taken.
#[allow(clippy::type_complexity, reason = "the pair is returned once and a name for it would only be read here")]
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
        let (m, mut sent) = match recv(sub) {
            Ok(Some(m)) => m,
            Ok(None) if wait_until(sub, now_ns().saturating_add(REPLAY_WAIT_NS)) => continue,
            _ => {
                devices.iter().for_each(|&(_, node)| close(node));
                close(sub);
                return Err(b"the device manager's replay stopped before Settled");
            }
        };
        match (m.op, parse_arrived(&m.body).and_then(DeviceRecord::read), sent.count()) {
            (OP_DEVICES_SETTLED, _, _) => return Ok((sub, devices)),
            (OP_DEVICES_ARRIVED, Some(r), 1) if let Some(node) = sent.take(0) => devices.push((r, node)),
            _ => {}
        }
    }
}

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

/// `init`'s manifest, or why it could not be read: each way it can fail says so.
fn read_manifest(root_ns: u64) -> Result<manifest::Manifest, &'static [u8]> {
    // `INSPECT` as well as `MAP_READ`: the object's size comes from `sys_handle_stat`.
    let (st, obj) = lookup(root_ns, INIT_TOML, RIGHT_MAP_READ | RIGHT_INSPECT);
    if st != 0 || obj == 0 {
        return Err(b"it did not resolve");
    }
    let parsed = with_mapped(obj, |bytes| match manifest::text(bytes) {
        Ok(text) => manifest::parse(text).map_err(|_| b"it did not parse".as_slice()),
        Err(_) => Err(b"it is not UTF-8".as_slice()),
    });
    close(obj);
    parsed.unwrap_or(Err(b"it would not map"))
}

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

/// Reply to a resolve with `ns` and the bytes of the suffix it stands for: the kernel continues
/// the resolve there (`rsproto-namespace-ops.md` § *The `SUBNAMESPACE` body*). The handle sent is
/// a duplicate with `LOOKUP`, all a continuation needs, and `TRANSFER`, which moving it needs.
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

#[derive(Copy, Clone, PartialEq, Eq)]
enum Listing {
    Tables,
    Mounts,
}

struct Service {
    root_ns: u64,
    serve_end: u64,
    /// The `block` subscription. **Held for the life of the service**: closing it frees the
    /// class, and the disks would be anyone's.
    subscription: u64,
    devices: Vec<Device>,
    /// Every device's node, by registry id: this service's own duplicate, kept for mounts and for
    /// the flush an unmount ends with.
    nodes: Vec<(u32, u64)>,
    /// `init`'s mounts, as the table reports them.
    init: Vec<Mounted>,
    /// Whether every one of `init`'s mounts was placed on a device ([`init_known`]). Without it
    /// this service mounts nothing and does not answer `InUse`: a device it took for free could
    /// be the running root.
    init_known: bool,
    /// **A live boot**: the root on a RAM disk, so the machine's internal disks mount read-only.
    live: bool,
    mounted: Vec<Mount>,
    /// The buffer every device read passes through, kept for [`refresh`](Self::refresh).
    scratch: Scratch,
    /// Ends of the session endpoints minted: resolves on them are [`session_only`].
    session_ends: Vec<u64>,
    /// Ends of the admin endpoints minted: any resolve on one opens an admin session.
    admin_ends: Vec<u64>,
    admin_sessions: Vec<u64>,
    /// Channels carrying `Eject`, in the wait set.
    media_sessions: Vec<u64>,
    /// Channels this service only sends a ping on, outside the wait set.
    watches: Watches,
    dirs: Vec<(u64, Listing)>,
}

/// A request refused: the error, and the reason its reply carries.
type Refused = (KError, &'static [u8]);

impl Service {
    /// **Read device `id` again.** The boot's probe goes stale as soon as anything writes the
    /// device, a mount of this service's or a raw writer through the `disks` grant, so what reads
    /// a device's `found` (a table, an administrator's mount) refreshes it first. **This is the
    /// one place `found` changes after the boot**: the unmount reads the device for its own line
    /// with [`read_found`](Self::read_found), which stores nothing.
    fn refresh(&mut self, id: u32) {
        if mounts::ejected(&self.devices, id) {
            return;
        }
        let Some(found) = self.read_found(id) else {
            return;
        };
        if let Some(d) = self.devices.iter_mut().find(|d| d.record.id == id) {
            d.found = found;
        }
    }

    /// What device `id` holds now; `None` if this service holds no node for it.
    fn read_found(&self, id: u32) -> Option<Found> {
        let &(_, node) = self.nodes.iter().find(|(d, _)| *d == id)?;
        let d = self.devices.iter().find(|d| d.record.id == id)?;
        Some(probe_record(&DeviceIo::new(node, &d.record, &self.scratch), &d.record))
    }

    /// [`refresh`](Self::refresh) every device nothing has mounted. A mounted one is left to its
    /// server: a writable mount's state reads "in use", a read-only one's cannot change.
    fn refresh_unmounted(&mut self) {
        let mounted: Vec<u32> = self.all_mounts().iter().map(|m| m.device).collect();
        let ids: Vec<u32> = self.devices.iter().map(|d| d.record.id).filter(|id| !mounted.contains(id)).collect();
        for id in ids {
            self.refresh(id);
        }
    }

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

    /// Answer one forwarded resolve on `from`, the endpoint at `/svc/storage` or, when `session`, a
    /// session endpoint. `false` if that endpoint has gone.
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
            Asked::Media => self.open_media_session(from, m.request_id),
            Asked::Watch => self.open_watch(from, m.request_id),
            Asked::Unknown => reply_error(from, OP_NS_RESOLVE, m.request_id, KError::NotFound),
        }
        true
    }

    /// Answer with a session endpoint. **Minted only on the root endpoint**: [`session_only`]
    /// refuses the suffix on a session endpoint, so its holder cannot make more.
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

    /// Answer with an admin endpoint. **Minted only on the root endpoint**: [`session_only`]
    /// refuses the suffix on a session endpoint, so a session cannot reach mounting at all. Who
    /// holds one is the view broker's `storage` grant to decide.
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

    /// Answer with a media session, which only ejects, so a session endpoint answers it.
    /// `WouldBlock` with every one taken: each is held for a request, so a retry finds one.
    fn open_media_session(&mut self, reply_to: u64, request_id: u64) {
        if self.media_sessions.len() >= MAX_MEDIA_SESSIONS {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
        }
        let Some((client_end, ours)) = make_channel(4) else {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        if reply_channel(reply_to, request_id, client_end) {
            self.media_sessions.push(ours);
        } else {
            close(ours);
        }
    }

    /// Answer with a watch. `WouldBlock` once every watch is held by a client still there.
    fn open_watch(&mut self, reply_to: u64, request_id: u64) {
        let Some((client_end, ours)) = make_channel(2) else {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        match self.watches.add(ours, ping) {
            Ok(gone) => {
                gone.into_iter().for_each(close);
                // A reply that does not go closes the client's end, and the next ping finds the
                // watch gone.
                let _ = reply_channel(reply_to, request_id, client_end);
            }
            Err(()) => {
                close(ours);
                close(client_end);
                reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
            }
        }
    }

    fn mounts_changed(&mut self) {
        self.watches.changed(ping).into_iter().for_each(close);
    }

    /// What a watch is told has changed: the names mounted and the devices there are — a partition
    /// a rescan published holding nothing changes no mount, and `disk --partition` waits for it.
    fn watched(&self) -> (Vec<String>, Vec<u32>) {
        let names = self.mounted.iter().map(|m| m.label.clone()).collect();
        (names, self.devices.iter().map(|d| d.record.id).collect())
    }

    /// One `Eject` on media session `i`: the unmount chain, held check included.
    fn serve_media(&mut self, i: usize) {
        let ch = self.media_sessions[i];
        let m = match recv_request(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                close(ch);
                self.media_sessions.remove(i);
                return;
            }
        };
        let refuse = |err: KError, why: &[u8]| {
            let _ = send(ch, m.op, m.request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &error_reply(err, why), &[]);
        };
        if m.op != OP_STORAGE_EJECT {
            return refuse(KError::Unsupported, b"a media session carries Eject alone");
        }
        let Ok(name) = core::str::from_utf8(&m.body) else {
            return refuse(KError::InvalidArgument, b"a name that is not UTF-8");
        };
        let (disk, names) = match storage_service::mounts::eject(&self.devices, &self.all_mounts(), name) {
            Ok(e) => (e.disk, e.names),
            Err(r) => return refuse(r.kerror(), r.why()),
        };
        // **The whole stick or none of it**: every mount is synced and asked whether a file is
        // held before any is unmounted, so a held file leaves no half-ejected stick.
        for n in &names {
            let Some(ns) = self.mounted.iter().find(|x| &x.label == n).map(|x| x.ns) else { continue };
            if !sync_mount(ns) {
                return refuse(KError::IoError, b"its files could not all be written back");
            }
            if held_files(n, ns) != 0 {
                return refuse(KError::WouldBlock, b"a file on it is still open or mapped");
            }
        }
        for n in &names {
            // A file opened since the check refuses here, and what was unmounted before it stays
            // unmounted: the refusal says the stick cannot be pulled, which is what matters.
            if let Err((err, why)) = self.unmount(n, true) {
                return refuse(err, why);
            }
        }
        let said = names.join(", ");
        Line::new().s(b"storage-service: ejected ").untrusted(said.as_bytes()).s(b", safe to remove").end();
        mounts::mark_ejected(&mut self.devices, disk);
        // The reply names every mount of the drive, a name per line (`rsproto-storage-ops.md`).
        // **Before the stop**, which a stick that never answers holds for a command's deadline:
        // the eject is done either way, as everything on it is unmounted.
        let _ = send(ch, m.op, m.request_id, RS_FLAG_REPLY, names.join("\n").as_bytes(), &[]);
        let node = self.nodes.iter().find(|(id, _)| *id == disk).map(|&(_, n)| n);
        let said_stop = mounts::stop_said(node.map_or(Err(KError::NotFound), eject_medium));
        Line::new().s(b"storage-service: ").untrusted(said.as_bytes()).s(b" ").s(said_stop.as_bytes()).end();
    }

    /// A resolve on an admin endpoint, whatever its suffix, gets an admin session. `false` if gone.
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
            let _ = send(ch, m.op, m.request_id, RS_FLAG_REPLY | RS_FLAG_ERROR, &error_reply(err, why), &[]);
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
                    Ok((label, read_only)) => {
                        let mut body = [0u8; 512];
                        let why = read_only.as_deref().map(str::as_bytes);
                        match build_mounted(&mut body, label.as_bytes(), why) {
                            Some(n) => {
                                let _ = send(ch, m.op, m.request_id, RS_FLAG_REPLY, &body[..n], &[]);
                            }
                            None => refuse(KError::KernelError, b"mounted, but the reply would not encode"),
                        }
                    }
                    Err((err, why)) => refuse(err, why),
                }
            }
            OP_STORAGE_UNMOUNT => {
                let Ok(label) = core::str::from_utf8(&m.body) else {
                    return refuse(KError::InvalidArgument, b"a label that is not UTF-8");
                };
                match self.unmount(label, true) {
                    Ok(()) => {
                        let _ = send(ch, m.op, m.request_id, RS_FLAG_REPLY, &[], &[]);
                    }
                    Err((err, why)) => refuse(err, why),
                }
            }
            OP_STORAGE_REREAD => {
                let Ok(name) = core::str::from_utf8(&m.body) else {
                    return refuse(KError::InvalidArgument, b"a name that is not UTF-8");
                };
                match self.reread(name) {
                    Ok(names) => {
                        let _ = send(ch, m.op, m.request_id, RS_FLAG_REPLY, names.join("\n").as_bytes(), &[]);
                    }
                    Err((err, why)) => refuse(err, why),
                }
            }
            OP_STORAGE_IN_USE => {
                // **Not answered unless `init`'s mounts are known**: an answer would leave out a
                // root it could not place, and the broker would hand that disk over raw.
                // Unanswered, the broker refuses `disks`.
                if !self.init_known {
                    return refuse(
                        KError::NoAccess,
                        b"init's mounts are not all known, so what is in use cannot be said",
                    );
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

    /// An administrator's mount: the label, and why it is read-only if its server would only read it.
    fn mount_explicit(&mut self, device: &str, label: &str) -> Result<(String, Option<String>), Refused> {
        self.refresh_unmounted();
        let taken: Vec<String> = self.mounted.iter().map(|x| x.label.clone()).collect();
        let room = self.mounted.len() < MAX_MOUNTS;
        let plan = explicit(&self.devices, &self.all_mounts(), &taken, room, self.init_known, device, label)
            .map_err(|r| (r.kerror(), r.why()))?;
        let Some(&(_, node)) = self.nodes.iter().find(|(id, _)| *id == plan.device) else {
            return Err((KError::NotFound, b"this service holds no node for it"));
        };
        let m = mount(self.root_ns, &plan, node).map_err(|why| (KError::IoError, why))?;
        let found = self.devices.iter().find(|d| d.record.id == plan.device);
        let read_only = found.and_then(|d| d.found.read_only()).filter(|_| m.mode == Mode::Ro).map(String::from);
        let (mode, why): (&[u8], &[u8]) = match &read_only {
            Some(why) => (b" (ro): ", why.as_bytes()),
            None => (b" (rw), as asked", b""),
        };
        Line::new().s(b"storage-service: mounted ").untrusted(m.label.as_bytes()).s(mode).s(why).end();
        self.mounted.push(m);
        Ok((plan.label, read_only))
    }

    /// **An administrator's `Reread`**: device `name` read again after `disk` wrote it and mounted
    /// by an arrival's rules. **A disk is rescanned first**, and its new partitions are mounted
    /// when they arrive, after the reply: the kernel publishes them, the manager passes them on.
    fn reread(&mut self, name: &str) -> Result<Vec<String>, (KError, &'static [u8])> {
        let what = mounts::reread(&self.devices, &self.all_mounts(), self.init_known, name)
            .map_err(|r| (r.kerror(), r.why()))?;
        let (Reread::Partition(id) | Reread::Disk(id)) = what;
        let Some(&(_, node)) = self.nodes.iter().find(|(d, _)| *d == id) else {
            return Err((KError::NotFound, b"this service holds no node for it"));
        };
        if let Reread::Disk(_) = what {
            match rescan(node) {
                Ok(n) => Line::new()
                    .s(b"storage-service: ")
                    .s(name.as_bytes())
                    .s(b" rescanned, as asked: ")
                    .u(n)
                    .s(b" partition(s)")
                    .end(),
                Err(KError::Unsupported) => {
                    let r = RereadRefusal::CannotRescan;
                    return Err((r.kerror(), r.why()));
                }
                Err(err) => return Err((err, b"the kernel could not read its partition table again")),
            }
        }
        self.refresh(id);
        let Some(d) = self.devices.iter().find(|d| d.record.id == id).cloned() else {
            return Err((KError::NotFound, b"it departed as it was read"));
        };
        let names = self.mount_as_arrival(&d, node);
        report(&d, &self.all_mounts(), &self.devices);
        Ok(names)
    }

    /// Mount `d`, arrived or read again, by an arrival's rules: the names mounted; failures said.
    fn mount_as_arrival(&mut self, d: &Device, node: u64) -> Vec<String> {
        let taken: Vec<String> = self.mounted.iter().map(|m| m.label.clone()).collect();
        let plan = mounts::arrival(
            core::slice::from_ref(d),
            &self.devices,
            &self.all_mounts(),
            &taken,
            self.live,
            self.init_known,
        );
        let mut names = Vec::new();
        for p in plan {
            if self.mounted.len() >= MAX_MOUNTS {
                Line::new()
                    .s(b"storage-service: ")
                    .untrusted(p.label.as_bytes())
                    .s(b" did not mount: every mount slot is in use")
                    .end();
                break;
            }
            match mount(self.root_ns, &p, node) {
                Ok(m) => {
                    names.push(m.label.clone());
                    self.mounted.push(m);
                }
                Err(why) => Line::new()
                    .s(b"storage-service: ")
                    .untrusted(p.label.as_bytes())
                    .s(b" did not mount: ")
                    .s(why)
                    .end(),
            }
        }
        names
    }

    /// **Unmount `label`: the chain**, each link only once the one before it held; an `Err` names
    /// the link that refused, and a refusal before the server is told leaves the mount as it was.
    /// `held_check` is `false` only for a shutdown's ([`shut_down`](Self::shut_down)).
    fn unmount(&mut self, label: &str, held_check: bool) -> Result<(), (KError, &'static [u8])> {
        let Some(i) = self.mounted.iter().position(|x| x.label == label) else {
            return Err((KError::NotFound, b"nothing is mounted with that label"));
        };
        // 1. **The label leaves `fs`**: taken out of the list, so a resolve under it is `NotFound`
        //    from here, and put back if a link before the server's refuses.
        let x = self.mounted.remove(i);
        // 2. **Every dirty file written back**, held or not.
        if !sync_mount(x.ns) {
            self.mounted.insert(i, x);
            return Err((KError::IoError, b"its files could not all be written back"));
        }
        // 3. **Refused while a file is held**: a mapping or a handle could write after the
        //    filesystem is marked clean ([`held_files`]).
        let held = if held_check { held_files(&x.label, x.ns) } else { 0 };
        if held != 0 {
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
        let recorded = answer.is_some_and(|(a, _)| a.op == OP_UNMOUNT && a.flags & RS_FLAG_ERROR == 0);
        close(x.ns);
        close(x.control);
        close(x.process);
        if !recorded {
            return Err((
                KError::IoError,
                b"its server did not record it clean, and the filesystem was not left clean",
            ));
        }
        // 5. **The drive's cache flushed**: a writable mount's writes are durable only then.
        if x.mode == Mode::Rw
            && let Some(&(_, node)) = self.nodes.iter().find(|(id, _)| *id == x.device)
            && !flush(node)
        {
            return Err((KError::IoError, b"the drive's cache could not be flushed"));
        }
        // 6. **What is said of the filesystem is what the device says**, read again now: a
        //    read-only mount wrote nothing, so one found not clean is not clean still. The QEMU
        //    gates match this line.
        let clean = self.read_found(x.device).and_then(|f| f.clean()) == Some(true);
        let mut l = Line::new();
        l.s(b"storage-service: unmounted ").untrusted(x.label.as_bytes());
        l.s(if clean { b", left clean".as_slice() } else { b", not left clean" });
        if x.mode == Mode::Ro {
            l.s(b" (read-only, so as it was found)");
        }
        l.end();
        Ok(())
    }

    /// **A shutdown's unmount**: every mount this service made, last mounted first, by the chain
    /// without its held check, each outcome said (the QEMU gates match it) — then exit. `init`'s
    /// own mounts are `init`'s to unmount.
    fn shut_down(&mut self) -> ! {
        while let Some(x) = self.mounted.last() {
            let label = x.label.clone();
            let result = self.unmount(&label, false);
            self.mounts_changed();
            if let Err((_, why)) = result {
                // Out of the list whatever happened: the chain drops a mount once its server has
                // been told, and one refused earlier is not asked again.
                self.mounted.retain(|m| m.label != label);
                Line::new()
                    .s(b"storage-service: ")
                    .untrusted(label.as_bytes())
                    .s(b" was not unmounted cleanly at shutdown: ")
                    .s(why)
                    .end();
            }
        }
        kprint(b"storage-service: asked to stop: everything it mounted is unmounted, exiting\n");
        exit(0);
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
        let sent = storage_service::fit::dir_page(&entries, cursor)
            .is_some_and(|(body, _)| send(ch, OP_FILE_READ_DIR, m.request_id, RS_FLAG_REPLY, &body, &[]));
        if !sent {
            reply_error(ch, m.op, m.request_id, KError::KernelError);
        }
    }

    /// A mount's control channel: its server sends nothing on it unasked, so a message is dropped,
    /// and its closing means the server has gone. **The mount goes with it**: a label forwarding
    /// to a server that no longer answers would fail every resolve under it.
    fn serve_control(&mut self, i: usize) {
        loop {
            match recv(self.mounted[i].control) {
                Ok(Some(_)) => {}
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

    /// The subscription: after `Settled` the manager sends `Arrived` or `Departed`, and **this
    /// follows them**: an arrival read and mounted by the boot's rules, a departure's mount torn
    /// down. Anything else is said and its handles closed; the manager going away is said once.
    fn serve_subscription(&mut self) {
        loop {
            match recv(self.subscription) {
                Ok(Some((m, mut sent))) => match (m.op, sent.count()) {
                    (OP_DEVICES_ARRIVED, 1) if let Some(node) = sent.take(0) => {
                        match parse_arrived(&m.body).and_then(DeviceRecord::read) {
                            Some(r) => self.arrived(r, node),
                            None => close(node),
                        }
                    }
                    (OP_DEVICES_DEPARTED, 0) if let Some(id) = parse_departed(&m.body) => self.departed(id),
                    _ => {
                        Line::new()
                            .s(b"storage-service: the device manager sent op ")
                            .u(m.op as u64)
                            .s(b" it does not read; ignored")
                            .end();
                    }
                },
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

impl Service {
    /// A device that arrived after the boot: read as at boot, kept with its node, mounted by the
    /// boot's rules, and said.
    fn arrived(&mut self, record: DeviceRecord, node: u64) {
        let found = probe_record(&DeviceIo::new(node, &record, &self.scratch), &record);
        let d = Device { found, record };
        self.devices.push(d.clone());
        self.nodes.push((record.id, node));
        self.mount_as_arrival(&d, node);
        report(&d, &self.all_mounts(), &self.devices);
    }

    /// **A device that departed**: a mount on it torn down, then the device out of the table and
    /// its node closed. The `departed` line is matched by the QEMU gates.
    fn departed(&mut self, id: u32) {
        let Some(at) = self.devices.iter().position(|d| d.record.id == id) else {
            return;
        };
        if let Some(i) = self.mounted.iter().position(|m| m.device == id) {
            self.tear_down(i);
        }
        let d = self.devices.remove(at);
        if let Some(n) = self.nodes.iter().position(|&(nid, _)| nid == id) {
            close(self.nodes.remove(n).1);
        }
        Line::new().s(b"storage-service: ").s(table::name(&d.record).as_bytes()).s(b" departed").end();
    }

    /// **Tear down mount `i`, whose device has gone**: as an unmount, but nothing can be kept.
    /// `sys_ns_sync` has every write-back refused and the kernel lets each file go; `Meta::Unmount`
    /// has the server fail to record the filesystem clean, answer and exit (**not a terminate**,
    /// which it does not read). The line it says is matched by the QEMU gates.
    fn tear_down(&mut self, i: usize) {
        let x = self.mounted.remove(i);
        let root = b"/";
        // SAFETY: a namespace handle this process holds, and a valid path.
        let _ = unsafe { syscall3(SYS_NS_SYNC, x.ns, root.as_ptr() as u64, root.len() as u64) };
        if send(x.control, OP_UNMOUNT, 1, 0, &[], &[])
            && wait_until(x.control, now_ns().saturating_add(READY_TIMEOUT_NS))
        {
            let _ = recv(x.control);
        }
        close(x.ns);
        close(x.control);
        close(x.process);
        Line::new()
            .s(b"storage-service: ")
            .untrusted(x.label.as_bytes())
            .s(b" left while mounted; what had not been written back is gone")
            .end();
    }
}

/// Say what one device holds, and whose it is. How a filesystem was left is not said while it is
/// mounted writable, as the table's `clean` is `Null` then. **A device not mounted says why**
/// ([`mounts::unmounted_why`]). The QEMU gates match these lines.
fn report(d: &Device, mounts: &[Mounted], all: &[Device]) {
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
        Found::Ext4 { label, .. } | Found::Fat { label, .. } => {
            line.s(d.found.filesystem().unwrap_or("").as_bytes());
            if !label.is_empty() {
                line.s(b" '").untrusted(label.as_bytes()).s(b"'");
            }
            if !mount.is_some_and(|m| m.mode == Mode::Rw) {
                line.s(match d.found.clean() {
                    Some(true) => b", left clean".as_slice(),
                    Some(false) => b", not left clean",
                    None => b", state unreadable",
                });
            }
        }
        Found::Nothing => {
            line.s(b"no filesystem");
        }
        Found::Ejected => {
            line.s(b"ejected");
        }
    }
    if let Some(m) = mount {
        line.s(b"; ").s(if m.by == By::Init { b"init's".as_slice() } else { b"mounted" });
        line.s(b" at ").untrusted(m.at.as_bytes()).s(if m.mode == Mode::Ro { b" (ro)".as_slice() } else { b" (rw)" });
        if let Some(why) = d.found.read_only() {
            line.s(b": ").s(why.as_bytes());
        }
    } else if let Some(why) = mounts::unmounted_why(d, all) {
        // **Said, not left to an absence**: a gate can then match the reason a device was passed
        // over. The table's `note` gives the same words.
        line.s(b"; ").s(why.as_bytes());
    }
    line.end();
}

/// Send `service-mgr` `Meta::Ready` naming this server, with the forwarding endpoint's client end.
fn send_ready(control: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let Some(n) = librsproto::meta::ready(&mut body, b"storage-service") else {
        return false;
    };
    send(control, librsproto::OP_READY, 0, 0, &body[..n], &[client_end])
}

/// Say, in place of `Meta::Ready`, that there is nothing to serve — no handle, and `service-mgr`
/// says `why` (`rsproto-wire-format.md` § Meta::Ready) — then exit.
fn refuse(control: u64, err: KError, why: &[u8]) -> ! {
    let _ = send(control, librsproto::OP_READY, 0, RS_FLAG_ERROR, &error_reply(err, why), &[]);
    exit(1);
}

/// **A refusal's body, sized to its reason**: in a fixed buffer, a reason longer than it makes
/// `error_body` decline, and the reply goes out flagged an error with no body, its code and its
/// reason both lost (`Reread`'s 108-byte `CannotRescan` did).
fn error_reply(err: KError, why: &[u8]) -> Vec<u8> {
    let mut body = alloc::vec![0u8; librsproto::error::ERROR_BODY_LEN + why.len()];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, why).unwrap_or(0);
    body.truncate(n);
    body
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
        devices.push(Device { found: probe_record(&io, &record), record });
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

    // A mount hands its server a duplicate of the node; the service keeps every node, for an
    // administrator's mount and for an unmount's flush.
    let mut plan = automount(&devices, &init, live, known);
    plan.truncate(MAX_MOUNTS);
    let mut mounted = Vec::new();
    for &(id, node) in &nodes {
        let Some(p) = plan.iter().find(|p| p.device == id) else {
            continue;
        };
        match mount(root_ns, p, node) {
            Ok(m) => mounted.push(m),
            Err(why) => {
                Line::new().s(b"storage-service: ").untrusted(p.label.as_bytes()).s(b" did not mount: ").s(why).end()
            }
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
        live,
        mounted,
        scratch,
        session_ends: Vec::new(),
        media_sessions: Vec::new(),
        watches: Watches::new(),
        admin_ends: Vec::new(),
        admin_sessions: Vec::new(),
        dirs: Vec::new(),
    };
    let mounts = s.all_mounts();
    Line::new().s(b"storage-service: ").u(s.devices.len() as u64).s(b" block device(s)").end();
    for d in &s.devices {
        report(d, &mounts, &s.devices);
    }
    if live {
        kprint(b"storage-service: a live boot: the root is on a RAM disk, so internal disks mount read-only\n");
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
    // **The control channel**, for a shutdown's `CTRL_OP_SHUTDOWN`. `0` once `service-mgr` is gone.
    let mut control = control;
    loop {
        let (mut handles, mut results) = ([0u64; MAX_WAIT_HANDLES], [0u8; 24 * MAX_WAIT_HANDLES]);
        let waited = {
            let mut n = 0usize;
            let mut push = |h: u64| {
                if h != 0 && n < MAX_WAIT_HANDLES {
                    handles[n] = h;
                    n += 1;
                }
            };
            push(s.serve_end);
            push(s.subscription);
            if control != 0 {
                push(control);
            }
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
            for &m in &s.media_sessions {
                push(m);
            }
            for &(d, _) in &s.dirs {
                push(d);
            }
            // SAFETY: `n` handles, and room for as many results; each kind refuses past its bound.
            unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, n as u64, results.as_mut_ptr() as u64, u64::MAX) }
        };
        // **What a watch is told**: the names mounted and the devices there are before this turn,
        // against after it. Compared here, once, rather than at each place that mounts, unmounts
        // or follows a device, so no path can miss a ping.
        let before = s.watched();
        for j in 0..waited.max(0) as usize {
            // The handle is the first word of each record.
            let h = u64::from_le_bytes(results[j * 24..j * 24 + 8].try_into().unwrap_or([0; 8]));
            if h == s.serve_end {
                // A root endpoint with no peer is the end of this service, not a message to skip:
                // the kernel keeps a peer-closed channel signalled, so going round again would spin
                // a CPU.
                if !s.serve_resolve(h, false) {
                    kprint(b"storage-service: forwarding endpoint closed\n");
                    exit(1);
                }
            } else if h == s.subscription {
                s.serve_subscription();
            } else if h == control {
                match libkern::control::recv(control) {
                    libkern::control::Control::Op(CTRL_OP_SHUTDOWN) => s.shut_down(),
                    libkern::control::Control::Closed => control = 0,
                    _ => {}
                }
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
            } else if let Some(i) = s.media_sessions.iter().position(|&m| m == h) {
                s.serve_media(i);
            } else if let Some(i) = s.dirs.iter().position(|&(d, _)| d == h) {
                s.serve_dir(i);
            }
        }
        if s.watched() != before {
            s.mounts_changed();
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"storage-service: PANIC\n");
    exit(1);
}
