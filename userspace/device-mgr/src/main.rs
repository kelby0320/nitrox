//! `device-mgr` — the event loop. Everything that decides is in the library half
//! ([`device_mgr`]); this file is what cannot be tested on the host: reading `/dev/registry`,
//! holding each device's node, serving forwarded resolves, and sending on channels.
//!
//! ## Shape
//!
//! 1. Read `/dev/registry` once: every node's record, then each class device's node by id. Every
//!    node registers before userspace starts, so one read is complete coldplug.
//! 2. Mint a forwarding endpoint and answer `Meta::Ready`; `init` binds it at `/svc/devices`.
//! 3. Serve. `<class>` makes the resolver that class's owner and replays its devices; `info` is a
//!    directory session; `info/<name>.tsm` is a table, as a fresh read-only memory object; and
//!    `info-endpoint` is a forwarding endpoint of the manager's own, on which only the last two
//!    are answered — what `init` couriers to the supervisors for a session's `/dev/devices`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::vec::Vec;
use device_mgr::classes::{Class, Owners, replay};
use device_mgr::suffix::{self, Asked};
use device_mgr::table;
use libkern::debug::Line;
use libkern::device::{DeviceRecord, records};
use libkern::*;
use librsproto::devices::{OP_DEVICES_ARRIVED, OP_DEVICES_SETTLED, build_arrived, build_settled};
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
/// Room a subscription channel keeps past its replay and `Settled`, for Phase 6's arrivals while
/// the owner is busy. The replay itself is always room made: sends do not block, so a channel
/// shallower than the replay would cut it short, and `Settled` would count what fit.
const SUBSCRIPTION_HEADROOM: usize = 32;
/// Info-only endpoints at once. `init` asks for one at boot and couriers it for every session;
/// the second is headroom, not a use.
const MAX_INFO_ENDPOINTS: usize = 2;
/// Directory sessions open at once: the wait set, less the endpoint, the info-only endpoints and
/// an owner per class.
const MAX_DIRS: usize = MAX_WAIT_HANDLES - 1 - MAX_INFO_ENDPOINTS - Class::ALL.len();
/// What the manager takes each node with, and hands its owner: `/dev/blk`'s authority, which is
/// the most any class needs — the storage service writes.
const NODE_RIGHTS: u64 = RIGHT_READ | RIGHT_WRITE | RIGHT_DUPLICATE | RIGHT_INSPECT | RIGHT_TRANSFER;

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

/// Receive one message on `ch`. `Ok(None)` if nothing was queued, `Err(())` if the peer has gone.
/// **Whatever handles came with it are closed here** — nothing a client sends this manager
/// carries a handle it wants, and one kept would be kept for good.
fn recv(ch: u64) -> Result<Option<(u16, u64, Vec<u8>)>, ()> {
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
    unsafe {
        for &h in &RECV_HANDLES[..RECV_COUNT.min(8)] {
            close(h);
        }
    }
    // SAFETY: bounded read of the payload the kernel just wrote.
    let msg = unsafe {
        let len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            len.min(MSG_LEN - PAYLOAD_OFF),
        )
    };
    Ok(decode(msg).ok().map(|m| (m.op, m.request_id, m.body.to_vec())))
}

fn lookup(ns: u64, path: &[u8], rights: u64) -> u64 {
    // SAFETY: a valid path pointer and a namespace handle this process holds.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return 0;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid; one waiter, no deadline.
    let (done, status, handle) = unsafe {
        WAIT_HANDLES[0] = po as u64;
        let w = syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX);
        let word = |off: usize| u64::from_le_bytes(WAIT_RESULTS[off..off + 8].try_into().unwrap_or([0; 8]));
        (w == 1, word(8) as i64, word(16))
    };
    close(po as u64);
    if done && status == 0 { handle } else { 0 }
}

/// Every node's record, from one read of `/dev/registry`.
fn read_registry(root_ns: u64) -> Option<Vec<DeviceRecord>> {
    let snap = lookup(root_ns, b"/dev/registry", RIGHT_MAP_READ | RIGHT_INSPECT);
    if snap == 0 {
        return None;
    }
    let mut info = abi::HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `info` is a writable 24-byte `HandleInfo`, the layout the kernel writes.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, snap, (&raw mut info) as u64) };
    // SAFETY: register-only syscall; `snap` is a MemoryObject handle with MAP_READ.
    let addr = if sr == 0 { unsafe { syscall4(SYS_MEMORY_MAP, snap, 0, info.size, RIGHT_MAP_READ) } } else { -1 };
    close(snap);
    if addr < 0 {
        return None;
    }
    // SAFETY: `info.size` bytes are mapped read-only at `addr` until the unmap below.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, info.size as usize) };
    let out = records(bytes).ok().map(|rs| rs.collect());
    // SAFETY: unmapping what was mapped above; `out` holds copies.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, 0) };
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
    // **Unmap before replying**, as the compositor learned (PR #175 review, finding 1): the
    // mapping holds its own reference, so left behind it would pin every table ever served.
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
    records: Vec<DeviceRecord>,
    /// Each class device's node, by registry id — what an owner is handed a duplicate of.
    nodes: Vec<(u32, u64)>,
    owners: Owners,
    dirs: Vec<u64>,
    /// The manager's ends of the info-only endpoints it has minted: resolves arriving here are
    /// [`suffix::info_only`].
    info_ends: Vec<u64>,
}

impl Manager {
    /// Answer one forwarded resolve from `from` — the endpoint bound at `/svc/devices`, or, when
    /// `info_only`, one of the info-only endpoints — replying on the endpoint it came from.
    /// `false` if that endpoint has gone.
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
                let bytes = if name == "all" { Some(table::all(&self.records)) } else { table::one(&self.records, name) };
                match bytes {
                    Some(b) => reply_with_object(from, request_id, &b),
                    None => reply_error(from, OP_NS_RESOLVE, request_id, KError::NotFound),
                }
            }
            Asked::Unknown => reply_error(from, OP_NS_RESOLVE, request_id, KError::NotFound),
        }
        true
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
        // **Said before the reply, not after**, so the line is ordered against what the asker
        // does next rather than racing it: a gate reads it as `init` having asked for the endpoint
        // it couriers, before any login exists.
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
            // One owner at a time — the kernel gives a raw device one reader.
            Line::new().s(b"device-mgr: ").s(class.name().as_bytes()).s(b" is owned; refused a second").end();
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::AlreadyExists);
        }
        let devices = replay(&self.records, class);
        let depth = (devices.len() + 1 + SUBSCRIPTION_HEADROOM).min(IPC_MAX_QUEUE_DEPTH as usize);
        let Some((client_end, ours)) = make_channel(depth as u64) else {
            return reply_error(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        // **The replay is queued before the owner has the channel**, so a completed resolve is a
        // whole subscription: `Settled` is already waiting, and what it counts cannot depend on
        // how soon the owner reads, or whether it closes first.
        let mut sent = 0u32;
        for r in devices {
            let Some(&(_, node)) = self.nodes.iter().find(|(id, _)| *id == r.id) else {
                continue;
            };
            // **The owner's own duplicate**, so an owner that exits cannot take the device from
            // the next one.
            // SAFETY: duplicating a handle this process holds, with DUPLICATE.
            let dup = unsafe { syscall2(SYS_HANDLE_DUPLICATE, node, NODE_RIGHTS) };
            if dup <= 0 {
                continue;
            }
            let mut body = [0u8; librsproto::devices::RECORD_LEN];
            let n = build_arrived(&mut body, r.as_bytes()).unwrap_or(0);
            if send(ours, OP_DEVICES_ARRIVED, 0, 0, &body[..n], &[dup as u64]) {
                sent += 1;
            } else {
                close(dup as u64);
            }
        }
        let mut body = [0u8; 4];
        let n = build_settled(&mut body, sent).unwrap_or(0);
        let _ = send(ours, OP_DEVICES_SETTLED, 0, 0, &body[..n], &[]);
        if !reply_channel(reply_to, request_id, client_end) {
            // The queued nodes go with the channel: the kernel releases an undelivered transfer
            // when its endpoint is destroyed.
            close(client_end);
            close(ours);
            return;
        }
        self.owners.claim(class, ours);
        Line::new().s(b"device-mgr: ").s(class.name().as_bytes()).s(b" owned, ").u(sent as u64).s(b" device(s) sent").end();
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

    /// An owner's channel: nothing is asked on it, so a message is answered `Unsupported`, and
    /// its closing frees the class.
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
        let mut out = [0u8; MSG_LEN - PAYLOAD_OFF - 64];
        let Some(mut w) = DirReplyWriter::new(&mut out) else {
            return reply_error(ch, op, request_id, KError::KernelError);
        };
        let mut i = cursor;
        while i < names.len() {
            let stem = names[i].strip_suffix(".tsm").unwrap_or(&names[i]);
            let size = if stem == "all" { table::all(&self.records).len() } else { table::one(&self.records, stem).map_or(0, |b| b.len()) };
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

/// Send `init` `Meta::Ready`, naming this server and carrying the forwarding endpoint's client end.
fn send_ready(control: u64, client_end: u64) -> bool {
    let mut body = [0u8; librsproto::meta::READY_PREFIX_LEN + 16];
    let Some(n) = librsproto::meta::ready(&mut body, b"device-mgr") else {
        return false;
    };
    send(control, librsproto::OP_READY, 0, 0, &body[..n], &[client_end])
}

/// Say, in place of `Meta::Ready`, that there is nothing to serve — no handle, and `init` prints
/// `why` (`rsproto-wire-format.md` § Meta::Ready) — then exit.
fn refuse(control: u64, err: KError, why: &[u8]) -> ! {
    let mut body = [0u8; librsproto::error::ERROR_BODY_LEN + 64];
    let n = librsproto::error::error_body(&mut body, err.as_i32(), 0, why).unwrap_or(0);
    let _ = send(control, librsproto::OP_READY, 0, RS_FLAG_ERROR, &body[..n], &[]);
    exit(1);
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = the inherited root namespace,
/// `rdx` = the control channel `init` installed, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"device-mgr: up\n");
    let Some(records) = read_registry(root_ns) else {
        refuse(control, KError::NotFound, b"no /dev/registry it could read, so no devices to hand out");
    };
    let mut nodes = Vec::new();
    for r in &records {
        if Class::of(r.kind()).is_none() {
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
        .u(records.len() as u64)
        .s(b" device(s) in the registry, ")
        .u(count(Class::Input))
        .s(b" input and ")
        .u(count(Class::Block))
        .s(b" block")
        .end();
    let mut m = Manager {
        serve_end,
        records,
        nodes,
        owners: Owners::new(),
        dirs: Vec::new(),
        info_ends: Vec::new(),
    };
    loop {
        // SAFETY: WAIT_HANDLES holds MAX_WAIT_HANDLES slots: the endpoint, the info-only
        // endpoints, an owner per class, and at most MAX_DIRS sessions — `mint_info_endpoint`
        // and `open_dir` refuse past their bounds.
        let waited = unsafe {
            let mut n = 0usize;
            let mut push = |h: u64| {
                if n < MAX_WAIT_HANDLES {
                    WAIT_HANDLES[n] = h;
                    n += 1;
                }
            };
            push(m.serve_end);
            for &e in &m.info_ends {
                push(e);
            }
            for c in m.owners.channels() {
                push(c);
            }
            for &d in &m.dirs {
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
            if h == m.serve_end {
                m.serve_resolve(h, false);
            } else if let Some(i) = m.info_ends.iter().position(|&e| e == h) {
                // Every holder of this endpoint has let it go — its bindings included.
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
