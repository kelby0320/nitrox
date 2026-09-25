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
//! 4. **Serve.** Mint a forwarding endpoint and answer `Meta::Ready`; `init` binds it at
//!    `/svc/storage`. `info` is a directory session, and `info/<name>.tsm` a table as a fresh
//!    read-only memory object.
//!
//! Mounting is C.5b's, and the `Storage` protocol C.5c's.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use fs_server_ext4::{BlockReader, FsError};
use libinittoml::manifest::{self, Mode};
use libkern::abi::{IO_OPCODE_READ, IoOp};
use libkern::debug::Line;
use libkern::device::{DeviceKind, DeviceRecord};
use libkern::*;
use librsproto::devices::{OP_DEVICES_ARRIVED, OP_DEVICES_SETTLED, parse_arrived};
use librsproto::file::{DIRENT_KIND_FILE, DirReplyWriter, parse_read_dir_request};
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, OBJECT_KIND_MEMOBJ, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply,
};
use librsproto::{OP_FILE_READ_DIR, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};
use storage_service::probe::{Found, probe};
use storage_service::sources::{DiskTable, InitMount, TableEntry, init_mounts, live_boot};
use storage_service::suffix::{self, Asked};
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
/// Directory sessions open at once: the wait set, less the endpoint and the subscription.
const MAX_DIRS: usize = MAX_WAIT_HANDLES - 2;
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
        Ok(m) => Ok(Some(Received { op: m.op, request_id: m.request_id, body: m.body.to_vec(), handles })),
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
    if !send(serve_end, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, &[obj as u64]) {
        close(obj as u64);
    }
}

/// Reply to a resolve with a channel: a directory session.
fn reply_channel(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    send(serve_end, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, &[client_end])
}

struct Service {
    serve_end: u64,
    /// The `block` subscription. **Held for the life of the service**: closing it frees the
    /// class, and the disks would be anyone's.
    subscription: u64,
    devices: Vec<Device>,
    mounts: Vec<Mounted>,
    dirs: Vec<u64>,
}

impl Service {
    /// Answer one forwarded resolve on the endpoint bound at `/svc/storage`. `false` if that
    /// endpoint has gone.
    fn serve_resolve(&mut self) -> bool {
        let from = self.serve_end;
        let m = match recv_request(from) {
            Ok(Some(m)) => m,
            Ok(None) => return true,
            Err(()) => return false,
        };
        let asked = match parse_resolve_request(&m.body) {
            Some(r) if m.op == OP_NS_RESOLVE => suffix::parse(r.suffix),
            _ => Asked::Unknown,
        };
        match asked {
            Asked::Directory => self.open_dir(from, m.request_id),
            Asked::File(name) => {
                let bytes = if name == "all" {
                    Some(table::all(&self.devices, &self.mounts))
                } else {
                    table::one(&self.devices, &self.mounts, name)
                };
                match bytes {
                    Some(b) => reply_with_object(from, m.request_id, &b),
                    None => reply_error(from, OP_NS_RESOLVE, m.request_id, KError::NotFound),
                }
            }
            Asked::Unknown => reply_error(from, OP_NS_RESOLVE, m.request_id, KError::NotFound),
        }
        true
    }

    fn open_dir(&mut self, reply_to: u64, request_id: u64) {
        if self.dirs.len() >= MAX_DIRS {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
        }
        let Some((client_end, ours)) = make_channel(4) else {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        if reply_channel(reply_to, request_id, client_end) {
            self.dirs.push(ours);
        } else {
            close(client_end);
            close(ours);
        }
    }

    fn serve_dir(&mut self, i: usize) {
        let ch = self.dirs[i];
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
        let names = table::entries(&self.devices);
        let mut out = [0u8; MSG_LEN - PAYLOAD_OFF - 64];
        let Some(mut w) = DirReplyWriter::new(&mut out) else {
            return reply_error(ch, m.op, m.request_id, KError::KernelError);
        };
        let mut i = cursor;
        while i < names.len() {
            let stem = names[i].strip_suffix(".tsm").unwrap_or(&names[i]);
            let size = if stem == "all" {
                table::all(&self.devices, &self.mounts).len()
            } else {
                table::one(&self.devices, &self.mounts, stem).map_or(0, |b| b.len())
            };
            if !w.push(i as u32 + 1, DIRENT_KIND_FILE, 0, size as u64, 0, names[i].as_bytes()) {
                break;
            }
            i += 1;
        }
        let next = if i >= names.len() { 0 } else { i as u64 };
        let n = w.finish(next);
        let _ = send(ch, OP_FILE_READ_DIR, m.request_id, RS_FLAG_REPLY, &out[..n], &[]);
    }

    /// The subscription: the manager has nothing to send after `Settled` until Phase 6 gives it
    /// an event source, and an arrival then is C.5b's to mount. Its handles are closed rather than
    /// leaked, and the manager going away is said once.
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
        line.s(b" at ").s(m.at.as_bytes()).s(if m.mode == Mode::Ro { b" (ro)".as_slice() } else { b" (rw)" });
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

    // Each node is read and then closed: nothing here writes to a device, and the class stays
    // this service's while the subscription is open.
    let mut devices = Vec::new();
    let mut tables = Vec::new();
    for (record, node) in held {
        let io = DeviceIo::new(node, &record, &scratch);
        if matches!(record.kind(), DeviceKind::Disk | DeviceKind::RamDisk)
            && let Some(t) = read_table(&io, record.id)
        {
            tables.push(t);
        }
        devices.push(Device { found: probe(&io), record });
        close(node);
    }
    let records: Vec<DeviceRecord> = devices.iter().map(|d| d.record).collect();

    let init: Vec<InitMount> = match read_manifest(root_ns) {
        Ok(m) => init_mounts(&m, &records, &tables),
        Err(why) => {
            Line::new().s(b"storage-service: init.toml could not be read (").s(why).s(b"), so no device is init's").end();
            Vec::new()
        }
    };
    let mut mounts = Vec::new();
    for m in &init {
        match m.device {
            Some(device) => mounts.push(Mounted { device, at: m.mount_point.clone(), by: By::Init, mode: m.mode }),
            None => Line::new()
                .s(b"storage-service: init's ")
                .s(m.mount_point.as_bytes())
                .s(b" names ")
                .untrusted(m.source.as_bytes())
                .s(b", which matched no device")
                .end(),
        }
    }

    Line::new().s(b"storage-service: ").u(devices.len() as u64).s(b" block device(s)").end();
    for d in &devices {
        report(d, &mounts);
    }
    if live_boot(&init, &records) {
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
    let mut s = Service { serve_end, subscription, devices, mounts, dirs: Vec::new() };
    loop {
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots: the endpoint, the subscription, and at
        // most MAX_DIRS sessions — `open_dir` refuses past that bound.
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
            for &d in &s.dirs {
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
                if !s.serve_resolve() {
                    kprint(b"storage-service: forwarding endpoint closed\n");
                    exit(1);
                }
            } else if h == s.subscription {
                s.serve_subscription();
            } else if let Some(i) = s.dirs.iter().position(|&d| d == h) {
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
