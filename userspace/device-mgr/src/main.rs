//! `device-mgr` — the event loop. Everything that decides is in the library half
//! ([`device_mgr`]); this file is what cannot be tested on the host: reading `/dev/registry`,
//! holding each device's node, serving forwarded resolves, and sending on channels.
//!
//! 1. Read `/dev/registry`: every node's record, then each class device's node by id. Every node
//!    present at boot registers before userspace starts, so the first read is complete coldplug.
//!    **Then follow the table**: a read waits on `/dev/registry/changes` past the generation of
//!    the last snapshot; each time it is answered the manager reads the table again, diffs it
//!    ([`device_mgr::follow`]), and sends each class's owner its `Arrived` and `Departed`.
//! 2. Mint a forwarding endpoint and answer `Meta::Ready`; `service-mgr` binds it at
//!    `/svc/devices`.
//! 3. Serve. `<class>` makes the resolver that class's owner and replays its devices; `info` is a
//!    directory session; `info/<name>.tsm` is a table, as a fresh read-only memory object; and
//!    `info-endpoint` is a forwarding endpoint of the manager's own, on which only the last two
//!    are answered — what `service-mgr` couriers to the supervisors for a session's `/dev/devices`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;
use device_mgr::classes::{Class, Owners, replay};
use device_mgr::follow::{Change, diff};
use device_mgr::io as io_table;
use device_mgr::suffix::{self, Asked};
use device_mgr::table;
use libkern::debug::Line;
use libkern::device::{CacheCounts, DeviceRecord, IoRecord, io, records};
use libkern::*;
use librsproto::devices::{
    OP_DEVICES_ARRIVED, OP_DEVICES_DEPARTED, OP_DEVICES_SETTLED, build_arrived, build_departed, build_settled,
};
use librsproto::file::{DIRENT_KIND_FILE, DirReplyWriter, parse_read_dir_request};
use librsproto::namespace::{
    OBJECT_KIND_CHANNEL, OBJECT_KIND_MEMOBJ, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply,
};
use librsproto::{OP_FILE_READ_DIR, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, decode, encode};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;
/// Room a subscription channel keeps past its replay and `Settled`, for arrivals while the owner
/// is busy. The replay itself is always room made: sends do not block, so a channel shallower than
/// the replay would cut it short, and `Settled` would count what fit.
const SUBSCRIPTION_HEADROOM: usize = 32;
/// Info-only endpoints at once; `service-mgr` asks for one, the second is headroom.
const MAX_INFO_ENDPOINTS: usize = 2;
/// Directory sessions open at once: the wait set, less the endpoint, the control channel, the read
/// waiting on `/dev/registry/changes`, the info-only endpoints and an owner per class.
const MAX_DIRS: usize = MAX_WAIT_HANDLES - 3 - MAX_INFO_ENDPOINTS - Class::ALL.len();
/// One page, for the eight bytes `/dev/registry/changes` answers with.
const PAGE: u64 = 4096;
/// What the manager takes each node with, and hands its owner: `/dev/blk`'s authority, which is
/// the most any class needs — the storage service writes.
const NODE_RIGHTS: u64 = RIGHT_READ | RIGHT_WRITE | RIGHT_DUPLICATE | RIGHT_INSPECT | RIGHT_TRANSFER;

fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process holds.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

/// Send `body` on `ch`, moving `handles`, from a whole message's buffer, as the kernel copies one.
fn send(ch: u64, op: u16, request_id: u64, flags: u32, body: &[u8], handles: &[u64]) -> bool {
    let (mut msg, mut carried) = ([0u8; MSG_LEN], [0u64; 8]);
    let Some(rs_len) = encode(&mut msg[PAYLOAD_OFF..], op, request_id, flags, body, handles.len() as u16) else {
        return false;
    };
    msg[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
    msg[8] = handles.len() as u8;
    carried[..handles.len()].copy_from_slice(handles);
    let (msg, carried) = (msg.as_ptr() as u64, carried.as_ptr() as u64);
    // SAFETY: a whole message, and the handles it carries, which this process holds.
    unsafe { syscall5(SYS_CHANNEL_SEND, ch, msg, carried, handles.len() as u64, SENDMODE_NOBLOCK) == 0 }
}

/// An error reply: a whole twelve-byte `ErrorBody`, which the kernel reads a refusal from.
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

/// Receive one message on `ch`. `Ok(None)` if nothing was queued, `Err(())` if the peer has gone.
/// **Whatever handles came with it are closed here** — nothing a client sends this manager
/// carries a handle it wants, and one kept would be kept for good.
fn recv(ch: u64) -> Result<Option<(u16, u64, Vec<u8>)>, ()> {
    let mut buf = [0u8; MSG_LEN];
    let _sent = match channel_recv(ch, &mut buf) {
        Ok(sent) => sent,
        Err(rr) if rr == KError::PeerClosed.as_i32() as i64 => return Err(()),
        Err(_) => return Ok(None),
    };
    let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
    let msg = &buf[PAYLOAD_OFF..PAYLOAD_OFF + len.min(MSG_LEN - PAYLOAD_OFF)];
    Ok(decode(msg).ok().map(|m| (m.op, m.request_id, m.body.to_vec())))
}

/// Resolve `path` in `ns`: the handle, the caller's to close, or `0` on failure.
fn lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    libfs::lookup_wait(ns, path, rights).1
}

/// Every node's record, from one read of `/dev/registry`, and the table's generation then.
fn read_registry(root_ns: u64) -> Option<(Vec<DeviceRecord>, u64)> {
    let snap = lookup(root_ns, b"/dev/registry", RIGHT_MAP_READ | RIGHT_INSPECT);
    if snap == 0 {
        return None;
    }
    let mut info = abi::HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `info` is a writable 24-byte `HandleInfo`, the layout the kernel writes.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, snap, (&raw mut info) as u64) };
    let addr = if sr == 0 {
        // SAFETY: register-only syscall; `snap` is a MemoryObject handle with MAP_READ.
        unsafe { syscall4(SYS_MEMORY_MAP, snap, 0, info.size, RIGHT_MAP_READ) }
    } else {
        -1
    };
    close(snap);
    if addr < 0 {
        return None;
    }
    // SAFETY: `info.size` bytes are mapped read-only at `addr` until the unmap below.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, info.size as usize) };
    let out = records(bytes).ok().map(|rs| {
        let generation = rs.generation();
        (rs.collect(), generation)
    });
    // SAFETY: unmapping what was mapped above; `out` holds copies.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, info.size) };
    out
}

/// **What the disks and the page cache have done**, from one read of `/dev/registry/io`: read
/// afresh for each `io.tsm`, since the counts move with every command.
fn read_io(root_ns: u64) -> Option<(CacheCounts, Vec<IoRecord>)> {
    let snap = lookup(root_ns, b"/dev/registry/io", RIGHT_MAP_READ | RIGHT_INSPECT);
    if snap == 0 {
        return None;
    }
    let mut info = abi::HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `info` is a writable 24-byte `HandleInfo`, the layout the kernel writes.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, snap, (&raw mut info) as u64) };
    let addr = if sr == 0 {
        // SAFETY: register-only syscall; `snap` is a MemoryObject handle with MAP_READ.
        unsafe { syscall4(SYS_MEMORY_MAP, snap, 0, info.size, RIGHT_MAP_READ) }
    } else {
        -1
    };
    close(snap);
    if addr < 0 {
        return None;
    }
    // SAFETY: `info.size` bytes are mapped read-only at `addr` until the unmap below.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, info.size as usize) };
    let out = io(bytes).ok().map(|snap| (snap.cache, snap.iter().collect()));
    // SAFETY: unmapping what was mapped above; `out` holds copies.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, info.size) };
    out
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
    // **Unmap before replying**: the mapping holds its own reference, so left behind it would
    // pin every table ever served.
    // SAFETY: unmapping a range this process mapped moments ago and never reads again.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, bytes.len() as u64) };
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_MEMOBJ, bytes.len() as u32);
    if !send(serve_end, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, &[obj as u64]) {
        close(obj as u64);
    }
}

/// Reply to a resolve with a channel: a subscription, or a directory session.
fn reply_channel(serve_end: u64, request_id: u64, client_end: u64) -> bool {
    let mut body = [0u8; RESOLVE_REPLY_LEN];
    let _ = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0);
    send(serve_end, OP_NS_RESOLVE, request_id, RS_FLAG_REPLY, &body, &[client_end])
}

struct Manager {
    serve_end: u64,
    root_ns: u64,
    /// The last snapshot's records.
    records: Vec<DeviceRecord>,
    /// **The generation the change read waits past**: the last snapshot's — or, when the table did
    /// not read after a change, the generation that change brought.
    past: u64,
    /// `/dev/registry/changes`, the page its answer lands in and where that is mapped, and the read
    /// waiting on it — `0` for none.
    changes: u64,
    change_buf: u64,
    change_at: u64,
    change_po: u64,
    /// Each class device's node, by registry id; an owner is handed a duplicate.
    nodes: Vec<(u32, u64)>,
    owners: Owners,
    dirs: Vec<u64>,
    /// The manager's ends of the info-only endpoints it has minted: resolves arriving here are
    /// [`suffix::info_only`].
    info_ends: Vec<u64>,
}

impl Manager {
    /// Answer one forwarded resolve from `from`: the endpoint bound at `/svc/devices`, or, when
    /// `info_only`, an info-only one. `false` if that endpoint has gone.
    fn serve_resolve(&mut self, from: u64, info_only: bool) -> bool {
        let (op, request_id, body) = match recv(from) {
            Ok(Some(m)) => m,
            Ok(None) => return true,
            Err(()) => return false,
        };
        let asked = match parse_resolve_request(&body) {
            Some(r) if op == OP_NS_RESOLVE => suffix::parse(r.suffix),
            _ => Asked::Unknown,
        };
        let asked = if info_only { suffix::info_only(asked) } else { asked };
        match asked {
            Asked::Subscribe(class) => self.subscribe(from, request_id, class),
            Asked::InfoEndpoint => self.mint_info_endpoint(from, request_id),
            Asked::Directory => self.open_dir(from, request_id),
            Asked::File(name) => {
                let bytes = match name {
                    "all" => Some(table::all(&self.records)),
                    "io" => self.io_table(),
                    _ => table::one(&self.records, name),
                };
                match bytes {
                    Some(b) => reply_with_object(from, request_id, &b),
                    None => reply_error(from, OP_NS_RESOLVE, request_id, KError::NotFound),
                }
            }
            Asked::Unknown => reply_error(from, OP_NS_RESOLVE, request_id, KError::NotFound),
        }
        true
    }

    /// `io.tsm`'s bytes, from a fresh read of `/dev/registry/io`; `None` if it would not read.
    fn io_table(&self) -> Option<Vec<u8>> {
        let (cache, disks) = read_io(self.root_ns)?;
        Some(io_table::table(cache, &disks, &self.records))
    }

    /// Answer with a forwarding endpoint of the manager's own, on which only the information is
    /// answered. **Minted only on the root endpoint**: `info_only` refuses this suffix on an
    /// info-only one, so its holder cannot make more.
    fn mint_info_endpoint(&mut self, reply_to: u64, request_id: u64) {
        if self.info_ends.len() >= MAX_INFO_ENDPOINTS {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
        }
        let Some((client_end, ours)) = make_channel(4) else {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        // Said before the reply, not after, so the line is ordered against what the asker does
        // next rather than racing it: a gate reads it as `service-mgr` having asked for the
        // endpoint it couriers, before any login exists.
        kprint(b"device-mgr: an info-only endpoint minted\n");
        if reply_channel(reply_to, request_id, client_end) {
            self.info_ends.push(ours);
        } else {
            kprint(b"device-mgr: ...and not delivered\n");
            close(client_end);
            close(ours);
        }
    }

    /// Make the resolver `class`'s owner, and replay the class's devices to it.
    fn subscribe(&mut self, reply_to: u64, request_id: u64, class: Class) {
        if self.owners.owner(class).is_some() {
            Line::new().s(b"device-mgr: ").s(class.name().as_bytes()).s(b" is owned; refused a second").end();
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::AlreadyExists);
        }
        let devices = replay(&self.records, class);
        let depth = (devices.len() + 1 + SUBSCRIPTION_HEADROOM).min(IPC_MAX_QUEUE_DEPTH as usize);
        let Some((client_end, ours)) = make_channel(depth as u64) else {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        // The replay is queued before the owner has the channel, so a completed resolve is a whole
        // subscription: what `Settled` counts cannot depend on how soon the owner reads.
        let mut sent = Vec::new();
        for r in devices {
            let Some(&(_, node)) = self.nodes.iter().find(|(id, _)| *id == r.id) else {
                continue;
            };
            if send_arrived(ours, r, node) {
                sent.push(r.id);
            }
        }
        let mut body = [0u8; 4];
        let n = build_settled(&mut body, sent.len() as u32).unwrap_or(0);
        let _ = send(ours, OP_DEVICES_SETTLED, 0, 0, &body[..n], &[]);
        if !reply_channel(reply_to, request_id, client_end) {
            // The kernel releases an undelivered transfer when its endpoint is destroyed.
            close(client_end);
            close(ours);
            return;
        }
        self.owners.claim(class, ours);
        for &id in &sent {
            self.owners.handed(class, id);
        }
        Line::new()
            .s(b"device-mgr: ")
            .s(class.name().as_bytes())
            .s(b" owned, ")
            .u(sent.len() as u64)
            .s(b" device(s) sent")
            .end();
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

    /// **Submit the read that waits on `/dev/registry/changes`** past [`Manager::past`], if none is
    /// waiting. A refusal is said, and the manager stops following: it keeps what it has, and
    /// serves it.
    fn arm_changes(&mut self) {
        if self.changes == 0 || self.change_po != 0 {
            return;
        }
        let op = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: self.change_buf,
            buf_offset: 0,
            offset: self.past,
            length: 8,
        };
        // SAFETY: a char `DeviceNode` with READ, and a valid `IoOp`.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, self.changes, (&op as *const IoOp) as u64) };
        if po > 0 {
            self.change_po = po as u64;
        } else {
            Line::new()
                .s(b"device-mgr: /dev/registry/changes refused a read (")
                .i(po)
                .s(b"); no longer following")
                .end();
            close(self.changes);
            self.changes = 0;
        }
    }

    /// **The table changed**: read it again, diff it against the records held, and tell each
    /// class's owner what arrived and what departed. Then wait again, past the new generation.
    /// **A table that does not read waits for the next change**, past the generation the node
    /// answered with: past the one held, the node answers at once, so a read that kept failing
    /// would spin a CPU, a line a pass. The next change's read diffs against the records held, so
    /// what this one missed is told then.
    fn follow(&mut self, status: i64) {
        close(self.change_po);
        self.change_po = 0;
        if status != 0 {
            Line::new()
                .s(b"device-mgr: a wait on /dev/registry/changes failed (")
                .i(status)
                .s(b"); no longer following")
                .end();
            close(self.changes);
            self.changes = 0;
            return;
        }
        let Some((now, generation)) = read_registry(self.root_ns) else {
            // SAFETY: `change_at` maps the change page read-only for the life of the manager, and the
            // kernel wrote its first eight bytes before completing the read just waited on.
            let answered = unsafe { core::ptr::read_volatile(self.change_at as *const u64) };
            self.past = self.past.max(answered);
            Line::new()
                .s(b"device-mgr: the registry did not read after a change, to generation ")
                .u(self.past)
                .s(b"; waiting for the next")
                .end();
            return self.arm_changes();
        };
        for change in diff(&self.records, &now) {
            match change {
                Change::Arrived(id) => self.arrived(&now[id as usize]),
                Change::Departed(id) => self.departed(&now[id as usize]),
            }
        }
        self.records = now;
        self.past = generation;
        self.arm_changes();
    }

    /// A class device that arrived: take its node, and send its owner an `Arrived` if it has one.
    fn arrived(&mut self, r: &DeviceRecord) {
        let Some(class) = Class::of(r.kind()) else { return };
        let path = format!("/dev/registry/{}", r.id);
        let node = lookup(self.root_ns, path.as_bytes(), NODE_RIGHTS);
        if node == 0 {
            // Departed meanwhile: handed to no one, so its departure is owed to no one.
            return;
        }
        self.nodes.push((r.id, node));
        let name = device_mgr::names::name(r);
        let said = |how: &[u8]| Line::new().s(b"device-mgr: ").s(name.as_bytes()).s(how).end();
        match self.owners.owner(class) {
            Some(ch) if send_arrived(ch, r, node) => {
                self.owners.handed(class, r.id);
                let mut line = Line::new();
                line.s(b"device-mgr: ").s(name.as_bytes()).s(b" arrived, sent to ").s(class.name().as_bytes()).end();
            }
            Some(_) => said(b" arrived, and its Arrived did not send"),
            None => said(b" arrived, for the replay"),
        }
    }

    /// A class device that departed: let its node go, and send its owner a `Departed` only if it
    /// was handed the device (`rsproto-devices-ops.md`).
    fn departed(&mut self, r: &DeviceRecord) {
        let Some(class) = Class::of(r.kind()) else { return };
        if let Some(i) = self.nodes.iter().position(|(id, _)| *id == r.id) {
            close(self.nodes[i].1);
            self.nodes.remove(i);
        }
        let name = device_mgr::names::name(r);
        let owed = self.owners.departed(class, r.id);
        let Some(ch) = self.owners.owner(class).filter(|_| owed) else {
            Line::new().s(b"device-mgr: ").s(name.as_bytes()).s(b" departed").end();
            return;
        };
        let mut body = [0u8; 4];
        let n = build_departed(&mut body, r.id).unwrap_or(0);
        if send(ch, OP_DEVICES_DEPARTED, 0, 0, &body[..n], &[]) {
            let mut line = Line::new();
            line.s(b"device-mgr: ").s(name.as_bytes()).s(b" departed, sent to ").s(class.name().as_bytes()).end();
        } else {
            Line::new().s(b"device-mgr: ").s(name.as_bytes()).s(b" departed, and its Departed did not send").end();
        }
    }

    /// An owner's channel: a message is answered `Unsupported`, and its closing frees the class.
    fn serve_owner(&mut self, ch: u64) {
        match recv(ch) {
            Err(()) => {
                if let Some(class) = self.owners.release(ch) {
                    Line::new().s(b"device-mgr: ").s(class.name().as_bytes()).s(b" released").end();
                }
                close(ch);
            }
            Ok(Some((op, request_id, _))) => reply_error(ch, op, request_id, KError::Unsupported),
            Ok(None) => {}
        }
    }

    fn serve_dir(&mut self, i: usize) {
        let ch = self.dirs[i];
        let (op, request_id, body) = match recv(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                close(ch);
                self.dirs.remove(i);
                return;
            }
        };
        let cursor = match parse_read_dir_request(&body) {
            Some(r) if op == OP_FILE_READ_DIR => r.cursor as usize,
            _ => return reply_error(ch, op, request_id, KError::Unsupported),
        };
        let names = table::entries(&self.records);
        let mut out = [0u8; librsproto::RS_MAX_BODY];
        let Some(mut w) = DirReplyWriter::new(&mut out) else {
            return reply_error(ch, op, request_id, KError::KernelError);
        };
        let mut i = cursor;
        while i < names.len() {
            let stem = names[i].strip_suffix(".tsm").unwrap_or(&names[i]);
            let size = match stem {
                "all" => table::all(&self.records).len(),
                "io" => self.io_table().map_or(0, |b| b.len()),
                _ => table::one(&self.records, stem).map_or(0, |b| b.len()),
            };
            if !w.push(i as u32 + 1, DIRENT_KIND_FILE, 0, size as u64, 0, names[i].as_bytes()) {
                break;
            }
            i += 1;
        }
        let next = if i >= names.len() { 0 } else { i as u64 };
        let n = w.finish(next);
        let _ = send(ch, OP_FILE_READ_DIR, request_id, RS_FLAG_REPLY, &out[..n], &[]);
    }
}

/// **Send `ch` an `Arrived` for `r`, carrying the owner's own duplicate of `node`**, so an owner
/// that exits cannot take the device from the next one. Whether it went.
fn send_arrived(ch: u64, r: &DeviceRecord, node: u64) -> bool {
    // SAFETY: duplicating a handle this process holds, with DUPLICATE.
    let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, node, NODE_RIGHTS) };
    if dup <= 0 {
        return false;
    }
    let mut body = [0u8; librsproto::devices::RECORD_LEN];
    let n = build_arrived(&mut body, r.as_bytes()).unwrap_or(0);
    let sent = send(ch, OP_DEVICES_ARRIVED, 0, 0, &body[..n], &[dup as u64]);
    if !sent {
        close(dup as u64);
    }
    sent
}

/// Send `Meta::Ready` to `service-mgr`, carrying the forwarding endpoint's client end.
fn send_ready(control: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let Some(n) = librsproto::meta::ready(&mut body, b"device-mgr") else {
        return false;
    };
    send(control, librsproto::OP_READY, 0, 0, &body[..n], &[client_end])
}

/// Say, in place of `Meta::Ready`, that there is nothing to serve: `service-mgr` prints `why`.
fn refuse(control: u64, err: KError, why: &[u8]) -> ! {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN + 64];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, why).unwrap_or(0);
    let _ = send(control, librsproto::OP_READY, 0, RS_FLAG_ERROR, &body[..n], &[]);
    exit(1);
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = the inherited root namespace,
/// `rdx` = the control channel `service-mgr` installed, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"device-mgr: up\n");
    let Some((records, generation)) = read_registry(root_ns) else {
        refuse(control, KError::NotFound, b"no /dev/registry it could read, so no devices to hand out");
    };
    let mut nodes = Vec::new();
    for r in &records {
        // A device that departed before this read has no node to take.
        if Class::of(r.kind()).is_none() || r.is_departed() {
            continue;
        }
        let path = format!("/dev/registry/{}", r.id);
        let node = lookup(root_ns, path.as_bytes(), NODE_RIGHTS);
        if node == 0 {
            Line::new().s(b"device-mgr: ").s(path.as_bytes()).s(b" would not resolve").end();
            continue;
        }
        nodes.push((r.id, node));
    }
    let Some((client_end, serve_end)) = make_channel(4) else {
        kprint(b"device-mgr: channel create FAIL\n");
        exit(1);
    };
    if !send_ready(control, client_end) {
        kprint(b"device-mgr: Ready send FAIL\n");
        exit(1);
    }
    let count = |c| replay(&records, c).len() as u64;
    Line::new()
        .s(b"device-mgr: ")
        .u(records.iter().filter(|r| !r.is_departed()).count() as u64)
        .s(b" device(s) in the registry, ")
        .u(count(Class::Input))
        .s(b" input and ")
        .u(count(Class::Block))
        .s(b" block")
        .end();
    // The control channel, for a shutdown's `CTRL_OP_SHUTDOWN`. `service --stop` never sends it:
    // the manager is `essential`. `0` once `service-mgr` is gone.
    let mut control = control;
    // The change node, and a page for its answers, mapped for the life of the manager so an
    // answer can be read when the table cannot.
    let changes = lookup(root_ns, b"/dev/registry/changes", RIGHT_READ | RIGHT_INSPECT);
    let change_buf = if changes == 0 {
        0
    } else {
        // SAFETY: register-only syscall.
        unsafe { syscall4(SYS_MEMORY_CREATE, PAGE, 0, 0, 0) }.max(0) as u64
    };
    let change_at = if change_buf == 0 {
        0
    } else {
        // SAFETY: register-only syscall; an object this process just created, mapped read-only.
        unsafe { syscall4(SYS_MEMORY_MAP, change_buf, 0, PAGE, RIGHT_MAP_READ) }.max(0) as u64
    };
    let following = change_at != 0;
    if !following {
        kprint(b"device-mgr: no /dev/registry/changes to follow; devices plugged in later will not be handed over\n");
        close(changes);
        close(change_buf);
    }
    let mut m = Manager {
        serve_end,
        root_ns,
        records,
        past: generation,
        changes: if following { changes } else { 0 },
        change_buf: if following { change_buf } else { 0 },
        change_at,
        change_po: 0,
        nodes,
        owners: Owners::new(),
        dirs: Vec::new(),
        info_ends: Vec::new(),
    };
    loop {
        m.arm_changes();
        let (mut handles, mut results) = ([0u64; MAX_WAIT_HANDLES], [0u8; 24 * MAX_WAIT_HANDLES]);
        let waited = {
            let mut n = 0usize;
            let mut push = |h: u64| {
                if n < MAX_WAIT_HANDLES {
                    handles[n] = h;
                    n += 1;
                }
            };
            push(m.serve_end);
            if control != 0 {
                push(control);
            }
            if m.change_po != 0 {
                push(m.change_po);
            }
            for &e in &m.info_ends {
                push(e);
            }
            for c in m.owners.channels() {
                push(c);
            }
            for &d in &m.dirs {
                push(d);
            }
            // SAFETY: `n` handles, and room for as many results: the endpoint, the control channel,
            // the read waiting on the change node, the info-only endpoints, an owner per class, and
            // at most MAX_DIRS sessions — `mint_info_endpoint` and `open_dir` refuse past their
            // bounds.
            unsafe { syscall4(SYS_WAIT, handles.as_ptr() as u64, n as u64, results.as_mut_ptr() as u64, u64::MAX) }
        };
        for j in 0..waited.max(0) as usize {
            // A record's first word is its handle; the low half of the second, an I/O's status.
            let word = |at: usize| u64::from_le_bytes(results[at..at + 8].try_into().unwrap_or([0; 8]));
            let (h, status) = (word(j * 24), word(j * 24 + 8) as u32 as i32 as i64);
            if m.change_po != 0 && h == m.change_po {
                m.follow(status);
            } else if h == m.serve_end {
                // A root endpoint with no peer is the end of this manager: the kernel keeps a
                // peer-closed channel signalled, so going round again would spin a CPU. It happens
                // when `service-mgr`'s bind fails, and then nothing can reach this manager anyway.
                if !m.serve_resolve(h, false) {
                    kprint(b"device-mgr: forwarding endpoint closed\n");
                    exit(1);
                }
            } else if h == control {
                match libkern::control::recv(control) {
                    libkern::control::Control::Op(CTRL_OP_SHUTDOWN) => {
                        kprint(b"device-mgr: asked to stop, exiting\n");
                        exit(0);
                    }
                    libkern::control::Control::Closed => control = 0,
                    _ => {}
                }
            } else if let Some(i) = m.info_ends.iter().position(|&e| e == h) {
                if !m.serve_resolve(h, true) {
                    close(h);
                    m.info_ends.remove(i);
                }
            } else if m.owners.channels().any(|c| c == h) {
                m.serve_owner(h);
            } else if let Some(i) = m.dirs.iter().position(|&d| d == h) {
                m.serve_dir(i);
            }
        }
    }
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"device-mgr: PANIC\n");
    exit(1);
}
