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
//! **An unmount is a chain**: the label leaves `fs`, every dirty file is written back (the mount's
//! flush, by `FLUSH_TIMEOUT_NS`), the unmount is refused if a file is still held (the mount's
//! held), the server records the filesystem clean and exits (`Meta::Unmount`), the drive's cache
//! is flushed, and the namespace is dropped.
//!
//! **A shutdown unmounts everything.** On `CTRL_OP_SHUTDOWN` (sent only by a shutdown, since this
//! service is `essential`) it runs the chain on every mount, last mounted first, **without the
//! held check**, by the maintainer's call: a write made after the sync is lost, as at a power
//! cut, and every filesystem is left clean. Then it exits.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use fs_server_ext4::{BlockReader, FsError};
use libinittoml::manifest::{self, Mode};
use libkern::abi::{IO_OPCODE_EJECT, IO_OPCODE_FLUSH, IO_OPCODE_RESCAN, IoOp, IoResult};
use libkern::device::{DeviceKind, DeviceRecord};
use libkern::{CTRL_OP_SHUTDOWN, KError, MAX_WAIT_HANDLES, Rights};
use libos::boot::Start;
use libos::control::{self, Control};
use libos::log::{self, Line};
use libos::{
    Carried, Channel, Deadline, FLUSH_TIMEOUT_NS, File, Handle, MapRead, MapReadWrite, Mapping, Memory, Msg, Namespace,
    NsMutable, NsReadOnly, Only, Op, Process, ReadWrite, Resource, Spawn, WaitSet, block_on, time,
};
use librsproto::devices::{OP_DEVICES_ARRIVED, OP_DEVICES_DEPARTED, OP_DEVICES_SETTLED, parse_arrived, parse_departed};
use librsproto::file::{DIRENT_KIND_DIR, DIRENT_KIND_FILE, parse_read_dir_request};
use librsproto::meta::{FS_SETUP_READ_ONLY, fs_setup};
use librsproto::namespace::parse_resolve_request;
use librsproto::server;
use librsproto::storage::{
    OP_STORAGE_CHANGED, OP_STORAGE_EJECT, OP_STORAGE_IN_USE, OP_STORAGE_MOUNT, OP_STORAGE_REREAD, OP_STORAGE_UNMOUNT,
    build_in_use, build_mounted, parse_mount,
};
use librsproto::{OP_FILE_READ_DIR, OP_NS_RESOLVE, OP_READY, OP_UNMOUNT, RS_FLAG_ERROR, RS_FLAG_REPLY, decode};
use storage_service::fit::{SCRATCH, ScratchRead, scratch_read};
use storage_service::mounts::{self, Plan, Reread, RereadRefusal, automount, explicit, in_use};
use storage_service::probe::{Found, probe_record};
use storage_service::sources::{DiskTable, InitMount, TableEntry, init_known, init_mounts, live_boot};
use storage_service::suffix::{self, Asked, session_only};
use storage_service::table::{self, By, Device, Mounted};
use storage_service::watch::{Sent, Watches};

mod disks;
mod mounting;
use disks::*;
use mounting::*;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const SUBSCRIPTION: &str = "/svc/devices/block";
/// Where `init`'s manifest is, in the initramfs this service inherits.
const INIT_TOML: &str = "/initramfs/etc/init.toml";
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

type Chan = Handle<Channel, Only>;
type Node = Handle<Resource, ReadWrite>;
type Root = Handle<Namespace, NsReadOnly>;
type Ns = Handle<Namespace, NsMutable>;

/// `ns` from now.
fn within(ns: u64) -> Deadline {
    Deadline::after(ns).unwrap_or(Deadline::at(ns))
}

/// Whether `h` signalled before `deadline`.
fn wait_until(h: &Chan, deadline: Deadline) -> bool {
    let mut set = WaitSet::<1>::new();
    set.add(h).is_ok() && set.wait(deadline).is_ok_and(|s| !s.is_empty())
}

struct Message {
    op: u16,
    flags: u32,
    request_id: u64,
    body: Vec<u8>,
}

/// Receive one message on `ch`, and the handles it carried, closed with the [`Carried`] unless
/// taken. `Ok(None)` if nothing was queued, `Err(())` if the peer has gone.
fn recv(ch: &Chan) -> Result<Option<(Message, Carried)>, ()> {
    let mut msg = Msg::new();
    let sent = match ch.recv(&mut msg) {
        Ok(sent) => sent,
        Err(e) if e.code() == KError::PeerClosed => return Err(()),
        Err(_) => return Ok(None),
    };
    // A message that does not decode closes what it carried, as `sent` drops.
    Ok(decode(msg.payload())
        .ok()
        .map(|m| (Message { op: m.op, flags: m.flags, request_id: m.request_id, body: m.body.to_vec() }, sent)))
}

/// Receive one message on `ch` and close whatever handles came with it: nothing a client sends
/// this service carries a handle it wants, and one kept would be kept for good.
fn recv_request(ch: &Chan) -> Result<Option<Message>, ()> {
    Ok(recv(ch)?.map(|(m, _)| m))
}

/// Refuse request `op` with `err` and no reason.
fn fail(ch: &Chan, op: u16, request_id: u64, err: KError) {
    let _ = server::reply_error(ch, op, request_id, err, b"");
}

/// **Ping a watch**: a bare `Changed`, never blocking. A full queue is a ping already waiting, and
/// a refused send is a client gone.
fn ping(ch: Option<&Chan>) -> Sent {
    let Some(ch) = ch else { return Sent::Gone };
    match server::send(ch, OP_STORAGE_CHANGED, 0, 0, &[], &mut Carried::new()) {
        Ok(()) => Sent::Queued,
        Err(e) if e.code() == KError::WouldBlock => Sent::Full,
        Err(_) => Sent::Gone,
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Listing {
    Tables,
    Mounts,
}

struct Service {
    root: Root,
    /// The `block` subscription. **Held for the life of the service**: closing it frees the
    /// class, and the disks would be anyone's. `None` once the device manager has gone.
    subscription: Option<Chan>,
    devices: Vec<Device>,
    /// Every device's node, by registry id: this service's own duplicate, kept for mounts and for
    /// the flush an unmount ends with.
    nodes: Vec<(u32, Node)>,
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
    /// Ends of the session endpoints minted: resolves on them are [`session_only`]. The endpoints
    /// and sessions below are shared so a handler holds one while it changes the list.
    session_ends: Vec<Rc<Chan>>,
    /// Ends of the admin endpoints minted: any resolve on one opens an admin session.
    admin_ends: Vec<Rc<Chan>>,
    admin_sessions: Vec<Rc<Chan>>,
    /// Channels carrying `Eject`, in the wait set.
    media_sessions: Vec<Rc<Chan>>,
    /// Channels this service only sends a ping on, outside the wait set: `watches` names each by
    /// the value of its handle.
    watches: Watches,
    watch_ends: BTreeMap<u64, Chan>,
    dirs: Vec<(Chan, Listing)>,
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

    /// This service's node for device `id`.
    fn node_of(&self, id: u32) -> Option<&Node> {
        self.nodes.iter().find(|(d, _)| *d == id).map(|(_, n)| n)
    }

    /// What device `id` holds now; `None` if this service holds no node for it.
    fn read_found(&self, id: u32) -> Option<Found> {
        let node = self.node_of(id)?;
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
    fn serve_resolve(&mut self, from: &Chan, session: bool) -> bool {
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
                    Some(b) => {
                        server::reply_object(from, m.request_id, &b);
                    }
                    None => fail(from, OP_NS_RESOLVE, m.request_id, KError::NotFound),
                }
            }
            Asked::Mount { label, consumed } => match self.mounted.iter().find(|x| x.label == label) {
                Some(x) => {
                    server::reply_subnamespace(from, m.request_id, x.ns.as_ref(), consumed, b"/");
                }
                None => fail(from, OP_NS_RESOLVE, m.request_id, KError::NotFound),
            },
            Asked::SessionEndpoint => self.mint_session_endpoint(from, m.request_id),
            Asked::AdminEndpoint => self.mint_admin_endpoint(from, m.request_id),
            Asked::Media => self.open_media_session(from, m.request_id),
            Asked::Watch => self.open_watch(from, m.request_id),
            Asked::Unknown => fail(from, OP_NS_RESOLVE, m.request_id, KError::NotFound),
        }
        true
    }

    /// Answer with a session endpoint. **Minted only on the root endpoint**: [`session_only`]
    /// refuses the suffix on a session endpoint, so its holder cannot make more.
    fn mint_session_endpoint(&mut self, reply_to: &Chan, request_id: u64) {
        if self.session_ends.len() >= MAX_SESSION_ENDPOINTS {
            return fail(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
        }
        let Ok((client_end, ours)) = Chan::pair(4) else {
            return fail(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        if server::reply_channel(reply_to, request_id, client_end) {
            self.session_ends.push(Rc::new(ours));
            log::write(b"storage-service: a session endpoint minted\n");
        }
    }

    /// Answer with an admin endpoint. **Minted only on the root endpoint**: [`session_only`]
    /// refuses the suffix on a session endpoint, so a session cannot reach mounting at all. Who
    /// holds one is the view broker's `storage` grant to decide.
    fn mint_admin_endpoint(&mut self, reply_to: &Chan, request_id: u64) {
        if self.admin_ends.len() >= MAX_ADMIN_ENDPOINTS {
            return fail(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
        }
        let Ok((client_end, ours)) = Chan::pair(4) else {
            return fail(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        if server::reply_channel(reply_to, request_id, client_end) {
            self.admin_ends.push(Rc::new(ours));
            log::write(b"storage-service: an admin endpoint minted\n");
        }
    }

    /// Answer with a media session, which only ejects, so a session endpoint answers it.
    /// `WouldBlock` with every one taken: each is held for a request, so a retry finds one.
    fn open_media_session(&mut self, reply_to: &Chan, request_id: u64) {
        if self.media_sessions.len() >= MAX_MEDIA_SESSIONS {
            return fail(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
        }
        let Ok((client_end, ours)) = Chan::pair(4) else {
            return fail(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        if server::reply_channel(reply_to, request_id, client_end) {
            self.media_sessions.push(Rc::new(ours));
        }
    }

    /// Answer with a watch. `WouldBlock` once every watch is held by a client still there.
    fn open_watch(&mut self, reply_to: &Chan, request_id: u64) {
        let Ok((client_end, ours)) = Chan::pair(2) else {
            return fail(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        let key = ours.raw().0;
        match self.watches.add(key, |e| ping(self.watch_ends.get(&e))) {
            Ok(gone) => {
                for e in gone {
                    self.watch_ends.remove(&e);
                }
                self.watch_ends.insert(key, ours);
                // A reply that does not go closes the client's end, and the next ping finds the
                // watch gone.
                let _ = server::reply_channel(reply_to, request_id, client_end);
            }
            Err(()) => {
                drop((ours, client_end));
                fail(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
            }
        }
    }

    fn mounts_changed(&mut self) {
        for e in self.watches.changed(|e| ping(self.watch_ends.get(&e))) {
            self.watch_ends.remove(&e);
        }
    }

    /// What a watch is told has changed: the names mounted and the devices there are — a partition
    /// a rescan published holding nothing changes no mount, and `disk --partition` waits for it.
    fn watched(&self) -> (Vec<String>, Vec<u32>) {
        let names = self.mounted.iter().map(|m| m.label.clone()).collect();
        (names, self.devices.iter().map(|d| d.record.id).collect())
    }

    /// One `Eject` on media session `i`: the unmount chain, held check included.
    fn serve_media(&mut self, i: usize) {
        let shared = Rc::clone(&self.media_sessions[i]);
        let ch = &*shared;
        let m = match recv_request(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                self.media_sessions.remove(i);
                return;
            }
        };
        let refuse = |err: KError, why: &[u8]| {
            let _ = server::reply_error(ch, m.op, m.request_id, err, why);
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
            let Some(x) = self.mounted.iter().find(|x| &x.label == n) else { continue };
            if !sync_mount(&x.ns) {
                return refuse(KError::IoError, b"its files could not all be written back");
            }
            if held_files(n, &x.ns) != 0 {
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
        let reply = names.join("\n");
        let _ = server::send(ch, m.op, m.request_id, RS_FLAG_REPLY, reply.as_bytes(), &mut Carried::new());
        let said_stop = mounts::stop_said(self.node_of(disk).map_or(Err(KError::NotFound), eject_medium));
        Line::new().s(b"storage-service: ").untrusted(said.as_bytes()).s(b" ").s(said_stop.as_bytes()).end();
    }

    /// A resolve on an admin endpoint, whatever its suffix, gets an admin session. `false` if gone.
    fn serve_admin_endpoint(&mut self, from: &Chan) -> bool {
        let m = match recv_request(from) {
            Ok(Some(m)) => m,
            Ok(None) => return true,
            Err(()) => return false,
        };
        if m.op != OP_NS_RESOLVE {
            fail(from, m.op, m.request_id, KError::Unsupported);
            return true;
        }
        if self.admin_sessions.len() >= MAX_ADMIN_SESSIONS {
            fail(from, OP_NS_RESOLVE, m.request_id, KError::WouldBlock);
            return true;
        }
        let Ok((client_end, ours)) = Chan::pair(4) else {
            fail(from, OP_NS_RESOLVE, m.request_id, KError::KernelError);
            return true;
        };
        if server::reply_channel(from, m.request_id, client_end) {
            self.admin_sessions.push(Rc::new(ours));
        }
        true
    }

    fn serve_admin(&mut self, i: usize) {
        let shared = Rc::clone(&self.admin_sessions[i]);
        let ch = &*shared;
        let m = match recv_request(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                self.admin_sessions.remove(i);
                return;
            }
        };
        let refuse = |err: KError, why: &[u8]| {
            let _ = server::reply_error(ch, m.op, m.request_id, err, why);
        };
        let reply = |body: &[u8]| {
            let _ = server::send(ch, m.op, m.request_id, RS_FLAG_REPLY, body, &mut Carried::new());
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
                            Some(n) => reply(&body[..n]),
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
                    Ok(()) => reply(&[]),
                    Err((err, why)) => refuse(err, why),
                }
            }
            OP_STORAGE_REREAD => {
                let Ok(name) = core::str::from_utf8(&m.body) else {
                    return refuse(KError::InvalidArgument, b"a name that is not UTF-8");
                };
                match self.reread(name) {
                    Ok(names) => reply(names.join("\n").as_bytes()),
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
                    Some(n) => reply(&body[..n]),
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
        let Some(node) = self.node_of(plan.device) else {
            return Err((KError::NotFound, b"this service holds no node for it"));
        };
        let m = mount(&self.root, &plan, node).map_err(|why| (KError::IoError, why))?;
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
        let Some(node) = self.node_of(id) else {
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
        let names = self.mount_as_arrival(&d);
        report(&d, &self.all_mounts(), &self.devices);
        Ok(names)
    }

    /// Mount `d`, arrived or read again, by an arrival's rules: the names mounted; failures said.
    fn mount_as_arrival(&mut self, d: &Device) -> Vec<String> {
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
        let Some((_, node)) = self.nodes.iter().find(|(id, _)| *id == d.record.id) else {
            return names;
        };
        for p in plan {
            if self.mounted.len() >= MAX_MOUNTS {
                Line::new()
                    .s(b"storage-service: ")
                    .untrusted(p.label.as_bytes())
                    .s(b" did not mount: every mount slot is in use")
                    .end();
                break;
            }
            match mount(&self.root, &p, node) {
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
        if !sync_mount(&x.ns) {
            self.mounted.insert(i, x);
            return Err((KError::IoError, b"its files could not all be written back"));
        }
        // 3. **Refused while a file is held**: a mapping or a handle could write after the
        //    filesystem is marked clean ([`held_files`]).
        let held = if held_check { held_files(&x.label, &x.ns) } else { 0 };
        if held != 0 {
            self.mounted.insert(i, x);
            return Err((KError::WouldBlock, b"a file on it is still open or mapped"));
        }
        // 4. **The server records the filesystem clean and exits.** From here the mount is gone
        //    whatever the answer: a server that could not record it exits too.
        let told = server::send(&x.control, OP_UNMOUNT, 1, 0, &[], &mut Carried::new()).is_ok();
        let answer = if told && wait_until(&x.control, within(READY_TIMEOUT_NS)) {
            recv(&x.control).ok().flatten()
        } else {
            None
        };
        let recorded = answer.is_some_and(|(a, _)| a.op == OP_UNMOUNT && a.flags & RS_FLAG_ERROR == 0);
        drop((x.ns, x.control, x.process));
        if !recorded {
            return Err((
                KError::IoError,
                b"its server did not record it clean, and the filesystem was not left clean",
            ));
        }
        // 5. **The drive's cache flushed**: a writable mount's writes are durable only then.
        if x.mode == Mode::Rw
            && let Some(node) = self.node_of(x.device)
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
        log::write(b"storage-service: asked to stop: everything it mounted is unmounted, exiting\n");
        libos::exit(0);
    }

    fn open_dir(&mut self, reply_to: &Chan, request_id: u64, listing: Listing) {
        if self.dirs.len() >= MAX_DIRS {
            return fail(reply_to, OP_NS_RESOLVE, request_id, KError::WouldBlock);
        }
        let Ok((client_end, ours)) = Chan::pair(4) else {
            return fail(reply_to, OP_NS_RESOLVE, request_id, KError::KernelError);
        };
        if server::reply_channel(reply_to, request_id, client_end) {
            self.dirs.push((ours, listing));
        }
    }

    fn serve_dir(&mut self, i: usize) {
        let (ch, listing) = &self.dirs[i];
        let m = match recv_request(ch) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(()) => {
                self.dirs.remove(i);
                return;
            }
        };
        let cursor = match parse_read_dir_request(&m.body) {
            Some(r) if m.op == OP_FILE_READ_DIR => r.cursor as usize,
            _ => return fail(ch, m.op, m.request_id, KError::Unsupported),
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
        let sent = storage_service::fit::dir_page(&entries, cursor).is_some_and(|(body, _)| {
            server::send(ch, OP_FILE_READ_DIR, m.request_id, RS_FLAG_REPLY, &body, &mut Carried::new()).is_ok()
        });
        if !sent {
            fail(ch, m.op, m.request_id, KError::KernelError);
        }
    }

    /// A mount's control channel: its server sends nothing on it unasked, so a message is dropped,
    /// and its closing means the server has gone. **The mount goes with it**: a label forwarding
    /// to a server that no longer answers would fail every resolve under it.
    fn serve_control(&mut self, i: usize) {
        loop {
            match recv(&self.mounted[i].control) {
                Ok(Some(_)) => {}
                Ok(None) => return,
                Err(()) => {
                    let x = self.mounted.remove(i);
                    Line::new()
                        .s(b"storage-service: the server for ")
                        .untrusted(x.label.as_bytes())
                        .s(b" exited; it is no longer mounted")
                        .end();
                    drop((x.ns, x.control, x.process));
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
            let Some(sub) = &self.subscription else { return };
            match recv(sub) {
                Ok(Some((m, mut sent))) => match (m.op, sent.len()) {
                    (OP_DEVICES_ARRIVED, 1) if let Some(node) = sent.take(0) => {
                        match parse_arrived(&m.body).and_then(DeviceRecord::read) {
                            Some(r) => self.arrived(r, node),
                            None => drop(node),
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
                    log::write(b"storage-service: the device manager has gone; the disks held are kept\n");
                    self.subscription = None;
                    return;
                }
            }
        }
    }
}

impl Service {
    /// A device that arrived after the boot: read as at boot, kept with its node, mounted by the
    /// boot's rules, and said.
    fn arrived(&mut self, record: DeviceRecord, node: Node) {
        let found = probe_record(&DeviceIo::new(&node, &record, &self.scratch), &record);
        let d = Device { found, record };
        self.devices.push(d.clone());
        self.nodes.push((record.id, node));
        self.mount_as_arrival(&d);
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
            drop(self.nodes.remove(n));
        }
        Line::new().s(b"storage-service: ").s(table::name(&d.record).as_bytes()).s(b" departed").end();
    }

    /// **Tear down mount `i`, whose device has gone**: as an unmount, but nothing can be kept.
    /// The flush has every write-back refused and the kernel lets each file go; `Meta::Unmount`
    /// has the server fail to record the filesystem clean, answer and exit (**not a terminate**,
    /// which it does not read). The line it says is matched by the QEMU gates.
    fn tear_down(&mut self, i: usize) {
        let x = self.mounted.remove(i);
        let _ = sync_mount(&x.ns);
        if server::send(&x.control, OP_UNMOUNT, 1, 0, &[], &mut Carried::new()).is_ok()
            && wait_until(&x.control, within(READY_TIMEOUT_NS))
        {
            let _ = recv(&x.control);
        }
        drop((x.ns, x.control, x.process));
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

/// Say, in place of `Meta::Ready`, that there is nothing to serve — no handle, and `service-mgr`
/// says `why` (`rsproto-wire-format.md` § Meta::Ready) — then exit.
fn refuse(control: Option<&Chan>, err: KError, why: &[u8]) -> ! {
    if let Some(control) = control {
        let _ = server::reply_error(control, OP_READY, 0, err, why);
    }
    libos::exit(1);
}

/// Add `h` to `set`.
fn watch<'a>(set: &mut WaitSet<'a, MAX_WAIT_HANDLES>, h: &'a Chan) {
    let _ = set.add(h);
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = the inherited root namespace,
/// `rdx` = the control channel `init` installed, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"storage-service: up\n");
    // **The control channel**, for a shutdown's `CTRL_OP_SHUTDOWN`. `None` once `service-mgr` is gone.
    let mut control: Option<Chan> = start.take_handle0();
    let Some(root) = start.root.take() else {
        refuse(
            control.as_ref(),
            KError::NotFound,
            b"/svc/devices/block did not resolve, so there are no disks to serve",
        );
    };
    let (subscription, held) = match take_block(&root) {
        Ok(taken) => taken,
        Err(why) => refuse(control.as_ref(), KError::NotFound, why),
    };
    let Some(scratch) = Scratch::new() else {
        refuse(control.as_ref(), KError::OutOfMemory, b"no scratch buffer to read the disks through");
    };

    let mut devices = Vec::new();
    let mut tables = Vec::new();
    let mut nodes = Vec::new();
    for (record, node) in held {
        let io = DeviceIo::new(&node, &record, &scratch);
        if matches!(record.kind(), DeviceKind::Disk | DeviceKind::RamDisk)
            && let Some(t) = read_table(&io, record.id)
        {
            tables.push(t);
        }
        devices.push(Device { found: probe_record(&io, &record), record });
        nodes.push((record.id, node));
    }
    let records: Vec<DeviceRecord> = devices.iter().map(|d| d.record).collect();

    let init_list: Option<Vec<InitMount>> = match read_manifest(&root) {
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
        log::write(b"storage-service: init's mounts are not all known, so this service mounts nothing and does not say what is in use\n");
    }
    let live = live_boot(init_list.as_deref().unwrap_or(&[]), &records);

    // A mount hands its server a duplicate of the node; the service keeps every node, for an
    // administrator's mount and for an unmount's flush.
    let mut plan = automount(&devices, &init, live, known);
    plan.truncate(MAX_MOUNTS);
    let mut mounted = Vec::new();
    for (id, node) in &nodes {
        let Some(p) = plan.iter().find(|p| p.device == *id) else {
            continue;
        };
        match mount(&root, p, node) {
            Ok(m) => mounted.push(m),
            Err(why) => {
                Line::new().s(b"storage-service: ").untrusted(p.label.as_bytes()).s(b" did not mount: ").s(why).end()
            }
        }
    }

    let mut s = Service {
        root,
        subscription: Some(subscription),
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
        watch_ends: BTreeMap::new(),
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
        log::write(b"storage-service: a live boot: the root is on a RAM disk, so internal disks mount read-only\n");
    } else {
        log::write(b"storage-service: not a live boot\n");
    }

    let Ok((client_end, serve_end)) = Chan::pair(4) else {
        log::write(b"storage-service: channel create FAIL\n");
        libos::exit(1);
    };
    if !control.as_ref().is_some_and(|c| server::ready(c, b"storage-service", client_end)) {
        log::write(b"storage-service: Ready send FAIL\n");
        libos::exit(1);
    }
    loop {
        let signalled = {
            let mut set = WaitSet::<MAX_WAIT_HANDLES>::new();
            watch(&mut set, &serve_end);
            if let Some(sub) = &s.subscription {
                watch(&mut set, sub);
            }
            if let Some(c) = &control {
                watch(&mut set, c);
            }
            for e in &s.session_ends {
                watch(&mut set, e);
            }
            for x in &s.mounted {
                watch(&mut set, &x.control);
            }
            for e in &s.admin_ends {
                watch(&mut set, e);
            }
            for a in &s.admin_sessions {
                watch(&mut set, a);
            }
            for m in &s.media_sessions {
                watch(&mut set, m);
            }
            for (d, _) in &s.dirs {
                watch(&mut set, d);
            }
            set.wait(Deadline::NEVER)
        };
        let Ok(signalled) = signalled else { continue };
        // **What a watch is told**: the names mounted and the devices there are before this turn,
        // against after it. Compared here, once, rather than at each place that mounts, unmounts
        // or follows a device, so no path can miss a ping.
        let before = s.watched();
        for (_, ready) in signalled.iter() {
            // Matched by the handle's value, as an earlier handler this turn may have removed
            // the entry an index would name.
            let h = ready.handle;
            let is = |c: &Chan| c.raw().0 == h;
            if is(&serve_end) {
                // A root endpoint with no peer is the end of this service, not a message to skip:
                // the kernel keeps a peer-closed channel signalled, so going round again would spin
                // a CPU.
                if !s.serve_resolve(&serve_end, false) {
                    log::write(b"storage-service: forwarding endpoint closed\n");
                    libos::exit(1);
                }
            } else if s.subscription.as_ref().is_some_and(is) {
                s.serve_subscription();
            } else if control.as_ref().is_some_and(is) {
                if control::recv(&mut control) == Control::Op(CTRL_OP_SHUTDOWN) {
                    s.shut_down();
                }
            } else if let Some(i) = s.session_ends.iter().position(|e| is(e)) {
                // Every holder of this endpoint has let it go, its bindings included.
                let ch = Rc::clone(&s.session_ends[i]);
                if !s.serve_resolve(&ch, true) {
                    s.session_ends.remove(i);
                }
            } else if let Some(i) = s.mounted.iter().position(|x| is(&x.control)) {
                s.serve_control(i);
            } else if let Some(i) = s.admin_ends.iter().position(|e| is(e)) {
                // Every holder of this endpoint has let it go, its bindings included.
                let ch = Rc::clone(&s.admin_ends[i]);
                if !s.serve_admin_endpoint(&ch) {
                    s.admin_ends.remove(i);
                }
            } else if let Some(i) = s.admin_sessions.iter().position(|a| is(a)) {
                s.serve_admin(i);
            } else if let Some(i) = s.media_sessions.iter().position(|m| is(m)) {
                s.serve_media(i);
            } else if let Some(i) = s.dirs.iter().position(|(d, _)| is(d)) {
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
    log::write(b"storage-service: PANIC\n");
    libos::exit(1);
}
