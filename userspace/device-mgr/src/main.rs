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

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use device_mgr::classes::{Class, Owners, replay};
use device_mgr::follow::{Change, diff};
use device_mgr::io as io_table;
use device_mgr::suffix::{self, Asked};
use device_mgr::table;
use libkern::device::{CacheCounts, DeviceRecord, IoRecord, io, records};
use libkern::{
    CTRL_OP_SHUTDOWN, IO_OPCODE_READ, IPC_MAX_QUEUE_DEPTH, IoOp, KError, MAX_WAIT_HANDLES, RIGHT_DUPLICATE,
    RIGHT_INSPECT, RIGHT_READ, RIGHT_TRANSFER, RIGHT_WRITE, Rights,
};
use libos::boot::Start;
use libos::control::{self, Control};
use libos::log::{self, Line};
use libos::{
    Carried, Channel, Deadline, Handle, MapRead, MapReadWrite, Mapping, Memory, Msg, Namespace, NsReadOnly, Only, Op,
    ReadOnly, ReadWrite, Resource, WaitSet, block_on,
};
use librsproto::devices::{
    OP_DEVICES_ARRIVED, OP_DEVICES_DEPARTED, OP_DEVICES_SETTLED, build_arrived, build_departed, build_settled,
};
use librsproto::error::{ERROR_BODY_LEN, error_body};
use librsproto::file::{DIRENT_KIND_FILE, parse_read_dir_request};
use librsproto::namespace::parse_resolve_request;
use librsproto::server::{self, ListedEntry};
use librsproto::{OP_FILE_READ_DIR, OP_NS_RESOLVE, OP_READY, RS_FLAG_ERROR, decode};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

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
const PAGE: usize = 4096;
/// What the manager takes each node with, and hands its owner: `/dev/blk`'s authority, which is
/// the most any class needs — the storage service writes.
const NODE_RIGHTS: Rights =
    Rights::from_bits(RIGHT_READ | RIGHT_WRITE | RIGHT_DUPLICATE | RIGHT_INSPECT | RIGHT_TRANSFER);

type Endpoint = Handle<Channel, Only>;
type Node = Handle<Resource, ReadWrite>;
type Root = Handle<Namespace, NsReadOnly>;

/// Answer a resolve with an error.
fn refuse_resolve(ch: &Endpoint, request_id: u64, err: KError) {
    let _ = server::reply_error(ch, OP_NS_RESOLVE, request_id, err, b"");
}

/// Receive one message on `ch`. `Ok(None)` if nothing was queued, `Err(())` if the peer has gone.
/// **Whatever handles came with it are closed here** — nothing a client sends this manager
/// carries a handle it wants, and one kept would be kept for good.
fn recv(ch: &Endpoint) -> Result<Option<(u16, u64, Vec<u8>)>, ()> {
    let mut msg = Msg::new();
    let _carried = match ch.recv(&mut msg) {
        Ok(carried) => carried,
        Err(e) if e.code() == KError::PeerClosed => return Err(()),
        Err(_) => return Ok(None),
    };
    Ok(decode(msg.payload()).ok().map(|m| (m.op, m.request_id, m.body.to_vec())))
}

/// Resolve `path` in `ns`.
fn lookup<T, M>(ns: &Root, path: &str, rights: Rights) -> Option<Handle<T, M>> {
    block_on(ns.lookup(path, rights)).ok()
}

/// A registry node's bytes, from one read of the memory object `path` resolves to.
fn snapshot(root: &Root, path: &str) -> Option<Vec<u8>> {
    let snap: Handle<Memory, MapRead> = lookup(root, path, Rights::MAP_READ | Rights::INSPECT)?;
    let size = usize::try_from(snap.stat().ok()?.size).ok()?;
    let mut bytes = vec![0u8; size];
    snap.map(size).ok()?.read_into(0, &mut bytes).ok()?;
    Some(bytes)
}

/// Every node's record, from one read of `/dev/registry`, and the table's generation then.
fn read_registry(root: &Root) -> Option<(Vec<DeviceRecord>, u64)> {
    let bytes = snapshot(root, "/dev/registry")?;
    records(&bytes).ok().map(|rs| {
        let generation = rs.generation();
        (rs.collect(), generation)
    })
}

/// **What the disks and the page cache have done**, from one read of `/dev/registry/io`: read
/// afresh for each `io.tsm`, since the counts move with every command.
fn read_io(root: &Root) -> Option<(CacheCounts, Vec<IoRecord>)> {
    let bytes = snapshot(root, "/dev/registry/io")?;
    io(&bytes).ok().map(|snap| (snap.cache, snap.iter().collect()))
}

/// `/dev/registry/changes`, the page its answers land in, and the read waiting on it.
struct Changes {
    /// First, so a read still running is waited out before the page it fills goes.
    op: Option<Op>,
    node: Handle<Resource, ReadOnly>,
    buf: Handle<Memory, MapReadWrite>,
    /// The page mapped for the life of the manager, so an answer can be read when the table cannot;
    /// read-only, as only the kernel writes it.
    view: Mapping<MapRead>,
}

impl Changes {
    fn open(root: &Root) -> Option<Changes> {
        let node = lookup(root, "/dev/registry/changes", Rights::READ | Rights::INSPECT)?;
        let buf = Handle::<Memory, MapReadWrite>::create(PAGE).ok()?;
        let view = buf.duplicate(Rights::MAP_READ).and_then(Handle::into_read_only).ok()?.map(PAGE).ok()?;
        Some(Changes { op: None, node, buf, view })
    }
}

/// What a wait's index names. A session is named by its handle, as an earlier answer in the same
/// round may have closed another.
#[derive(Copy, Clone)]
enum Waited {
    Resolve,
    Control,
    Change,
    Info(u64),
    Owner(u64),
    Dir(u64),
}

struct Manager {
    root: Root,
    /// The last snapshot's records.
    records: Vec<DeviceRecord>,
    /// **The generation the change read waits past**: the last snapshot's — or, when the table did
    /// not read after a change, the generation that change brought.
    past: u64,
    /// `None` when the manager is not following the table.
    changes: Option<Changes>,
    /// Each class device's node, by registry id; an owner is handed a duplicate.
    nodes: Vec<(u32, Node)>,
    owners: Owners,
    /// The owners' channels, by the value [`Owners`] names them with.
    owner_ends: BTreeMap<u64, Endpoint>,
    dirs: Vec<Endpoint>,
    /// The manager's ends of the info-only endpoints it has minted: resolves arriving here are
    /// [`suffix::info_only`].
    info_ends: Vec<Endpoint>,
}

impl Manager {
    /// Answer one forwarded resolve from `from`: the endpoint bound at `/svc/devices`, or, when
    /// `info_only`, an info-only one. `false` if that endpoint has gone.
    fn serve_resolve(&mut self, from: &Endpoint, info_only: bool) -> bool {
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
                    Some(b) => {
                        server::reply_object(from, request_id, &b);
                    }
                    None => refuse_resolve(from, request_id, KError::NotFound),
                }
            }
            Asked::Unknown => refuse_resolve(from, request_id, KError::NotFound),
        }
        true
    }

    /// `io.tsm`'s bytes, from a fresh read of `/dev/registry/io`; `None` if it would not read.
    fn io_table(&self) -> Option<Vec<u8>> {
        let (cache, disks) = read_io(&self.root)?;
        Some(io_table::table(cache, &disks, &self.records))
    }

    /// Answer with a forwarding endpoint of the manager's own, on which only the information is
    /// answered. **Minted only on the root endpoint**: `info_only` refuses this suffix on an
    /// info-only one, so its holder cannot make more.
    fn mint_info_endpoint(&mut self, reply_to: &Endpoint, request_id: u64) {
        if self.info_ends.len() >= MAX_INFO_ENDPOINTS {
            return refuse_resolve(reply_to, request_id, KError::WouldBlock);
        }
        let Ok((client_end, ours)) = Handle::pair(4) else {
            return refuse_resolve(reply_to, request_id, KError::KernelError);
        };
        // Said before the reply, not after, so the line is ordered against what the asker does
        // next rather than racing it: a gate reads it as `service-mgr` having asked for the
        // endpoint it couriers, before any login exists.
        log::write(b"device-mgr: an info-only endpoint minted\n");
        if server::reply_channel(reply_to, request_id, client_end) {
            self.info_ends.push(ours);
        } else {
            log::write(b"device-mgr: ...and not delivered\n");
        }
    }

    /// Make the resolver `class`'s owner, and replay the class's devices to it.
    fn subscribe(&mut self, reply_to: &Endpoint, request_id: u64, class: Class) {
        if self.owners.owner(class).is_some() {
            Line::new().s(b"device-mgr: ").s(class.name().as_bytes()).s(b" is owned; refused a second").end();
            return refuse_resolve(reply_to, request_id, KError::AlreadyExists);
        }
        let devices = replay(&self.records, class);
        let depth = (devices.len() + 1 + SUBSCRIPTION_HEADROOM).min(IPC_MAX_QUEUE_DEPTH as usize);
        let Ok((client_end, ours)) = Handle::pair(depth as u32) else {
            return refuse_resolve(reply_to, request_id, KError::KernelError);
        };
        // The replay is queued before the owner has the channel, so a completed resolve is a whole
        // subscription: what `Settled` counts cannot depend on how soon the owner reads.
        let mut sent = Vec::new();
        for r in devices {
            let Some((_, node)) = self.nodes.iter().find(|(id, _)| *id == r.id) else {
                continue;
            };
            if send_arrived(&ours, r, node) {
                sent.push(r.id);
            }
        }
        let mut body = [0u8; 4];
        let n = build_settled(&mut body, sent.len() as u32).unwrap_or(0);
        let _ = server::send(&ours, OP_DEVICES_SETTLED, 0, 0, &body[..n], &mut Carried::new());
        if !server::reply_channel(reply_to, request_id, client_end) {
            return;
        }
        let key = ours.raw().0;
        self.owner_ends.insert(key, ours);
        self.owners.claim(class, key);
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

    fn open_dir(&mut self, reply_to: &Endpoint, request_id: u64) {
        if self.dirs.len() >= MAX_DIRS {
            return refuse_resolve(reply_to, request_id, KError::WouldBlock);
        }
        let Ok((client_end, ours)) = Handle::pair(4) else {
            return refuse_resolve(reply_to, request_id, KError::KernelError);
        };
        if server::reply_channel(reply_to, request_id, client_end) {
            self.dirs.push(ours);
        }
    }

    /// **Submit the read that waits on `/dev/registry/changes`** past [`Manager::past`], if none is
    /// waiting. A refusal is said, and the manager stops following: it keeps what it has, and
    /// serves it.
    fn arm_changes(&mut self) {
        let Some(c) = &mut self.changes else { return };
        if c.op.is_some() {
            return;
        }
        let op = IoOp {
            opcode: IO_OPCODE_READ,
            flags: 0,
            buffer: c.buf.raw().0,
            buf_offset: 0,
            offset: self.past,
            length: 8,
        };
        match Op::submit(c.node.raw(), &op) {
            Ok(po) => c.op = Some(po),
            Err(e) => {
                Line::new()
                    .s(b"device-mgr: /dev/registry/changes refused a read (")
                    .i(i64::from(e.code().as_i32()))
                    .s(b"); no longer following")
                    .end();
                self.changes = None;
            }
        }
    }

    /// **The table changed**: read it again, diff it against the records held, and tell each
    /// class's owner what arrived and what departed. Then wait again, past the new generation.
    /// **A table that does not read waits for the next change**, past the generation the node
    /// answered with: past the one held, the node answers at once, so a read that kept failing
    /// would spin a CPU, a line a pass. The next change's read diffs against the records held, so
    /// what this one missed is told then.
    fn follow(&mut self, done: libos::Result<libkern::IoResult>) {
        if let Some(c) = &mut self.changes {
            c.op = None;
        }
        let status = match done {
            Ok(res) => i64::from(res.status),
            Err(e) => i64::from(e.code().as_i32()),
        };
        if status != 0 {
            Line::new()
                .s(b"device-mgr: a wait on /dev/registry/changes failed (")
                .i(status)
                .s(b"); no longer following")
                .end();
            self.changes = None;
            return;
        }
        let Some((now, generation)) = read_registry(&self.root) else {
            let answered = self.changes.as_ref().map_or(0, |c| {
                let mut word = [0u8; 8];
                // The kernel wrote the first eight bytes before completing the read just waited on.
                let _ = c.view.read_into(0, &mut word);
                u64::from_le_bytes(word)
            });
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
        let Some(node) = lookup::<Resource, ReadWrite>(&self.root, &path, NODE_RIGHTS) else {
            // Departed meanwhile: handed to no one, so its departure is owed to no one.
            return;
        };
        let name = device_mgr::names::name(r);
        let said = |how: &[u8]| Line::new().s(b"device-mgr: ").s(name.as_bytes()).s(how).end();
        let owner = self.owners.owner(class);
        let sent = owner.map(|ch| self.owner_ends.get(&ch).is_some_and(|end| send_arrived(end, r, &node)));
        self.nodes.push((r.id, node));
        match sent {
            Some(true) => {
                self.owners.handed(class, r.id);
                let mut line = Line::new();
                line.s(b"device-mgr: ").s(name.as_bytes()).s(b" arrived, sent to ").s(class.name().as_bytes()).end();
            }
            Some(false) => said(b" arrived, and its Arrived did not send"),
            None => said(b" arrived, for the replay"),
        }
    }

    /// A class device that departed: let its node go, and send its owner a `Departed` only if it
    /// was handed the device (`rsproto-devices-ops.md`).
    fn departed(&mut self, r: &DeviceRecord) {
        let Some(class) = Class::of(r.kind()) else { return };
        if let Some(i) = self.nodes.iter().position(|(id, _)| *id == r.id) {
            self.nodes.remove(i);
        }
        let name = device_mgr::names::name(r);
        let owed = self.owners.departed(class, r.id);
        let Some(ch) = self.owners.owner(class).filter(|_| owed).and_then(|ch| self.owner_ends.get(&ch)) else {
            Line::new().s(b"device-mgr: ").s(name.as_bytes()).s(b" departed").end();
            return;
        };
        let mut body = [0u8; 4];
        let n = build_departed(&mut body, r.id).unwrap_or(0);
        if server::send(ch, OP_DEVICES_DEPARTED, 0, 0, &body[..n], &mut Carried::new()).is_ok() {
            let mut line = Line::new();
            line.s(b"device-mgr: ").s(name.as_bytes()).s(b" departed, sent to ").s(class.name().as_bytes()).end();
        } else {
            Line::new().s(b"device-mgr: ").s(name.as_bytes()).s(b" departed, and its Departed did not send").end();
        }
    }

    /// An owner's channel: a message is answered `Unsupported`, and its closing frees the class.
    fn serve_owner(&mut self, ch: u64) {
        let Some(end) = self.owner_ends.get(&ch) else { return };
        match recv(end) {
            Err(()) => {
                if let Some(class) = self.owners.release(ch) {
                    Line::new().s(b"device-mgr: ").s(class.name().as_bytes()).s(b" released").end();
                }
                self.owner_ends.remove(&ch);
            }
            Ok(Some((op, request_id, _))) => {
                let _ = server::reply_error(end, op, request_id, KError::Unsupported, b"");
            }
            Ok(None) => {}
        }
    }

    fn serve_dir(&mut self, i: usize) {
        let ch = &self.dirs[i];
        let (op, request_id, body) = match recv(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                self.dirs.remove(i);
                return;
            }
        };
        let cursor = match parse_read_dir_request(&body) {
            Some(r) if op == OP_FILE_READ_DIR => r.cursor as usize,
            _ => {
                let _ = server::reply_error(ch, op, request_id, KError::Unsupported, b"");
                return;
            }
        };
        let names = table::entries(&self.records);
        let _ = server::serve_read_dir(ch, request_id, cursor, |at| {
            let name = names.get(at)?;
            let stem = name.strip_suffix(".tsm").unwrap_or(name);
            let size = match stem {
                "all" => table::all(&self.records).len(),
                "io" => self.io_table().map_or(0, |b| b.len()),
                _ => table::one(&self.records, stem).map_or(0, |b| b.len()),
            };
            Some(ListedEntry { name: name.as_bytes(), kind: DIRENT_KIND_FILE, size: size as u64 })
        });
    }
}

/// **Send `ch` an `Arrived` for `r`, carrying the owner's own duplicate of `node`**, so an owner
/// that exits cannot take the device from the next one. Whether it went.
fn send_arrived(ch: &Endpoint, r: &DeviceRecord, node: &Node) -> bool {
    let Ok(dup) = node.duplicate(NODE_RIGHTS) else {
        return false;
    };
    let mut carried = Carried::new();
    if carried.push(dup).is_err() {
        return false;
    }
    let mut body = [0u8; librsproto::devices::RECORD_LEN];
    let n = build_arrived(&mut body, r.as_bytes()).unwrap_or(0);
    // A refused send leaves the duplicate in `carried`, which closes it.
    server::send(ch, OP_DEVICES_ARRIVED, 0, 0, &body[..n], &mut carried).is_ok()
}

/// Say, in place of `Meta::Ready`, that there is nothing to serve: `service-mgr` prints `why`.
fn refuse(control: Option<&Endpoint>, err: KError, why: &[u8]) -> ! {
    let mut body = [0u8; ERROR_BODY_LEN + 64];
    let n = error_body(&mut body, err.as_i32(), 0, why).unwrap_or(0);
    if let Some(c) = control {
        let _ = server::send(c, OP_READY, 0, RS_FLAG_ERROR, &body[..n], &mut Carried::new());
    }
    libos::exit(1);
}

/// `control` is for a shutdown's `CTRL_OP_SHUTDOWN`. `service --stop` never sends it: the manager is
/// `essential`. It is `None` once `service-mgr` is gone.
fn serve_loop(m: &mut Manager, serve_end: &Endpoint, mut control: Option<Endpoint>) -> ! {
    loop {
        m.arm_changes();
        let mut names = [Waited::Resolve; MAX_WAIT_HANDLES];
        let signalled = {
            let mut set = WaitSet::<MAX_WAIT_HANDLES>::new();
            let mut watch = |added: libos::Result<usize>, what: Waited| {
                if let Ok(i) = added {
                    names[i] = what;
                }
            };
            watch(set.add(serve_end), Waited::Resolve);
            if let Some(c) = &control {
                watch(set.add(c), Waited::Control);
            }
            if let Some(op) = m.changes.as_ref().and_then(|c| c.op.as_ref()) {
                watch(set.add_op(op), Waited::Change);
            }
            for e in &m.info_ends {
                watch(set.add(e), Waited::Info(e.raw().0));
            }
            for c in m.owners.channels() {
                if let Some(e) = m.owner_ends.get(&c) {
                    watch(set.add(e), Waited::Owner(c));
                }
            }
            for d in &m.dirs {
                watch(set.add(d), Waited::Dir(d.raw().0));
            }
            // At most the endpoint, the control channel, the read waiting on the change node, the
            // info-only endpoints, an owner per class, and MAX_DIRS sessions: `mint_info_endpoint`
            // and `open_dir` refuse past their bounds.
            set.wait(Deadline::NEVER)
        };
        let Ok(signalled) = signalled else { continue };
        for (i, _) in signalled.iter() {
            match names[i] {
                Waited::Change => {
                    let done =
                        m.changes.as_mut().and_then(|c| c.op.as_mut()).and_then(|op| op.completion_in(&signalled));
                    if let Some(done) = done {
                        m.follow(done);
                    }
                }
                Waited::Resolve => {
                    // A root endpoint with no peer is the end of this manager: the kernel keeps a
                    // peer-closed channel signalled, so going round again would spin a CPU. It
                    // happens when `service-mgr`'s bind fails, and then nothing can reach this
                    // manager anyway.
                    if !m.serve_resolve(serve_end, false) {
                        log::write(b"device-mgr: forwarding endpoint closed\n");
                        libos::exit(1);
                    }
                }
                Waited::Control => {
                    if control::recv(&mut control) == Control::Op(CTRL_OP_SHUTDOWN) {
                        log::write(b"device-mgr: asked to stop, exiting\n");
                        libos::exit(0);
                    }
                }
                Waited::Info(h) => {
                    if let Some(i) = m.info_ends.iter().position(|e| e.raw().0 == h) {
                        // Out of the list while it is served, so the answer can add to it; an
                        // info-only endpoint never does.
                        let end = m.info_ends.remove(i);
                        if m.serve_resolve(&end, true) {
                            m.info_ends.insert(i, end);
                        }
                    }
                }
                Waited::Owner(h) => m.serve_owner(h),
                Waited::Dir(h) => {
                    if let Some(i) = m.dirs.iter().position(|d| d.raw().0 == h) {
                        m.serve_dir(i);
                    }
                }
            }
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = the inherited root namespace,
/// `rdx` = the control channel `service-mgr` installed, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"device-mgr: up\n");
    let control: Option<Endpoint> = start.take_handle0();
    let root = start.root.take();
    let read = root.as_ref().and_then(read_registry);
    let (Some(root), Some((records, generation))) = (root, read) else {
        refuse(control.as_ref(), KError::NotFound, b"no /dev/registry it could read, so no devices to hand out");
    };
    let mut nodes = Vec::new();
    for r in &records {
        // A device that departed before this read has no node to take.
        if Class::of(r.kind()).is_none() || r.is_departed() {
            continue;
        }
        let path = format!("/dev/registry/{}", r.id);
        let Some(node) = lookup::<Resource, ReadWrite>(&root, &path, NODE_RIGHTS) else {
            Line::new().s(b"device-mgr: ").s(path.as_bytes()).s(b" would not resolve").end();
            continue;
        };
        nodes.push((r.id, node));
    }
    let Ok((client_end, serve_end)) = Handle::pair(4) else {
        log::write(b"device-mgr: channel create FAIL\n");
        libos::exit(1);
    };
    if !control.as_ref().is_some_and(|c| server::ready(c, b"device-mgr", client_end)) {
        log::write(b"device-mgr: Ready send FAIL\n");
        libos::exit(1);
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
    let changes = Changes::open(&root);
    if changes.is_none() {
        log::write(
            b"device-mgr: no /dev/registry/changes to follow; devices plugged in later will not be handed over\n",
        );
    }
    let mut m = Manager {
        root,
        records,
        past: generation,
        changes,
        nodes,
        owners: Owners::new(),
        owner_ends: BTreeMap::new(),
        dirs: Vec::new(),
        info_ends: Vec::new(),
    };
    serve_loop(&mut m, &serve_end, control);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"device-mgr: PANIC\n");
    libos::exit(1);
}
