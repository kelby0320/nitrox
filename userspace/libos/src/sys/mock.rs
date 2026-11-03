//! A model of the kernel for host tests, answering each call as the kernel does: the same
//! refusals in the same order, handles that exist or do not, channels with rings and peers,
//! namespaces with direct bindings, and objects whose bytes are real host memory a mapping reads.
//! It panics on a wait for ever that nothing will end, a loop of waits that never does, and an
//! exit, which ends the test that reaches it.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};

use libkern::{
    HandleInfo, IPC_HANDLE_MAX, IPC_PAYLOAD_SIZE, IoOp, IoResult, IpcMsg, KError, KObjectType, Notification, NsEntry,
    Rights, SPAWN_MAX_HANDLES, SpawnArgs, ThreadArgs,
};

use super::Resize;

const PAGE: usize = 4096;
/// The kernel's cap on one memory object.
const MEMORY_MAX: u64 = 16 << 20;
/// Room each file's bytes have, so a mapping's address outlives the file's growth.
const FILE_ROOM: usize = 64 << 10;
const PATH_MAX: usize = 1024;

fn e(k: KError) -> i64 {
    k as i64
}

/// One kernel object.
enum Object {
    Memory(Pages),
    File {
        bytes: Pages,
        size: u64,
    },
    Namespace(Vec<(Vec<u8>, usize, u64)>),
    Channel {
        peer: usize,
        ring: VecDeque<Queued>,
        held: VecDeque<(Queued, usize, bool)>,
        depth: usize,
        /// A server's forwarding endpoint, whose sends the kernel takes within the call.
        forward: bool,
    },
    Po(Po),
    Notify(VecDeque<Notification>),
    Device,
    /// A userspace server: a lookup through it waits for its answer, a fresh memory object.
    Server,
    /// A userspace server that answers every lookup at once with the object it names, as the
    /// logging service answers with an endpoint.
    Answers(usize),
    Process,
    Thread,
    Entropy,
}

impl Object {
    fn kind(&self) -> KObjectType {
        match self {
            Object::Memory(_) => KObjectType::MemoryObject,
            Object::File { .. } => KObjectType::FileObject,
            Object::Namespace(_) => KObjectType::Namespace,
            Object::Channel { .. } => KObjectType::IpcChannel,
            Object::Po(_) => KObjectType::PendingOperation,
            Object::Notify(_) => KObjectType::NotificationChannel,
            Object::Device => KObjectType::DeviceNode,
            Object::Server | Object::Answers(_) => KObjectType::UserspaceServerReg,
            Object::Process => KObjectType::Process,
            Object::Thread => KObjectType::Thread,
            Object::Entropy => KObjectType::EntropyObject,
        }
    }
}

/// An object's bytes, in whole pages as the kernel maps them, so a mapping starts on a page.
struct Pages(Box<[Page]>);

#[repr(C, align(4096))]
#[derive(Clone, Copy)]
struct Page([u8; PAGE]);

impl Pages {
    fn new(bytes: usize) -> Pages {
        Pages(vec![Page([0; PAGE]); bytes.div_ceil(PAGE)].into_boxed_slice())
    }

    fn len(&self) -> usize {
        self.0.len() * PAGE
    }

    fn base(&mut self) -> u64 {
        self.0.as_mut_ptr() as u64
    }
}

/// A message in a ring: its bytes, and the objects and rights its handles carry.
struct Queued {
    msg: Box<IpcMsg>,
    handles: Vec<(usize, u64)>,
}

/// One `PendingOperation`: signalled, or not yet, or never; a lookup's object installed at the
/// first read that reports it.
struct Po {
    ready: bool,
    never: bool,
    status: i32,
    result: u64,
    lookup: Option<(usize, u64)>,
}

#[derive(Default)]
struct Mock {
    next_handle: u64,
    objects: Vec<Object>,
    table: BTreeMap<u64, (usize, u64)>,
    maps: BTreeMap<u64, (usize, usize)>,
    /// Completions armed for upcoming `io_submit`s: `(status, result, ready_now, never)`.
    script: VecDeque<(i32, u64, bool, bool)>,
    fail_wait: Option<i64>,
    fail_submit: Option<i32>,
    fail_bind: Option<i32>,
    fail_sync: Option<i32>,
    fail_thread: Option<i32>,
    fail_recv: Option<i32>,
    /// Whether the next rename's answer carries a handle, as a server's could.
    rename_handle: bool,
    submitted: Vec<IoOp>,
    /// What `ns_sync` and `ns_held` answer for a mount path: `(written, held)`.
    mounts: BTreeMap<Vec<u8>, (i64, i64)>,
    unseeded: u32,
    realtime: Option<u64>,
    clock: u64,
    sleeps: u32,
    submits: u32,
    waits: u32,
    closes: u32,
    last_wait_count: usize,
    closed_in_flight: u32,
    /// Closes of a handle not in the table: one already closed, or moved away.
    stray_closes: u32,
    terminated: u32,
    log: Vec<u8>,
    spawned: Vec<SpawnArgs>,
    threads: Vec<ThreadArgs>,
}

thread_local! {
    static MOCK: RefCell<Mock> = RefCell::new(fresh());
}

/// An empty model, but for handles `1` and `2`, devices `io_submit` takes.
fn fresh() -> Mock {
    let mut m = Mock { next_handle: 1, ..Default::default() };
    for _ in 0..2 {
        m.mint(Object::Device, Rights::READ | Rights::WRITE);
    }
    m.next_handle = 1000;
    m
}

fn with<R>(f: impl FnOnce(&mut Mock) -> R) -> R {
    MOCK.with(|m| f(&mut m.borrow_mut()))
}

impl Mock {
    fn object(&mut self, o: Object) -> usize {
        self.objects.push(o);
        self.objects.len() - 1
    }

    fn install(&mut self, obj: usize, rights: u64) -> u64 {
        let h = self.next_handle;
        self.next_handle += 1;
        self.table.insert(h, (obj, rights));
        h
    }

    fn mint(&mut self, o: Object, rights: Rights) -> i64 {
        let obj = self.object(o);
        self.install(obj, rights.bits()) as i64
    }

    /// The object and rights `h` names, needing `need`: `InvalidHandle` or `NoAccess` otherwise.
    fn get(&self, h: u64, need: Rights) -> Result<(usize, u64), i64> {
        let &(obj, rights) = self.table.get(&h).ok_or(e(KError::InvalidHandle))?;
        if !Rights::from_bits(rights).contains(need) {
            return Err(e(KError::NoAccess));
        }
        Ok((obj, rights))
    }

    /// As [`get`](Self::get), and of `kind` or `InvalidArgument`, as the kernel's `lookup_typed`.
    fn typed(&self, h: u64, need: Rights, kind: KObjectType) -> Result<(usize, u64), i64> {
        let (obj, rights) = self.get(h, need)?;
        if self.objects[obj].kind() != kind {
            return Err(e(KError::InvalidArgument));
        }
        Ok((obj, rights))
    }

    /// Whether anything still holds `obj`: a handle, a binding or a message in a ring.
    fn held(&self, obj: usize) -> bool {
        self.table.values().any(|&(o, _)| o == obj)
            || self.objects.iter().any(|o| match o {
                Object::Namespace(b) => b.iter().any(|&(_, o, _)| o == obj),
                Object::Answers(target) => *target == obj,
                Object::Channel { ring, held, .. } => {
                    ring.iter().chain(held.iter().map(|(q, ..)| q)).any(|q| q.handles.iter().any(|&(o, _)| o == obj))
                }
                _ => false,
            })
    }

    fn signalled(&self, obj: usize) -> Option<IoResult> {
        let ready = IoResult { handle: 0, status: 0, reserved: 0, result: 0 };
        match &self.objects[obj] {
            Object::Po(po) if po.ready => Some(IoResult { status: po.status, result: po.result, ..ready }),
            Object::Channel { peer, ring, .. } if !ring.is_empty() || !self.held(*peer) => Some(ready),
            Object::Notify(q) if !q.is_empty() => Some(ready),
            _ => None,
        }
    }

    fn po(&mut self, po: Po) -> i64 {
        self.mint(Object::Po(po), Rights::WAIT | Rights::INSPECT)
    }

    /// A lookup's PO, complete at once: `found` the object and rights it resolves to.
    fn lookup_po(&mut self, found: Result<(usize, u64), KError>) -> i64 {
        let (status, lookup) = match found {
            Ok(target) => (0, Some(target)),
            Err(k) => (k as i32, None),
        };
        self.po(Po { ready: true, never: false, status, result: 0, lookup })
    }

    fn namespace(&mut self, ns: u64, need: Rights) -> Result<usize, i64> {
        self.typed(ns, need, KObjectType::Namespace).map(|(o, _)| o)
    }

    fn bindings(&mut self, ns: usize) -> &mut Vec<(Vec<u8>, usize, u64)> {
        match &mut self.objects[ns] {
            Object::Namespace(b) => b,
            _ => unreachable!("checked a namespace"),
        }
    }

    /// A file's object and rights at `path` in `ns`, bound as a direct binding.
    fn file_at(&mut self, ns: usize, path: &[u8]) -> Option<(usize, u64)> {
        self.bindings(ns).iter().find(|(p, ..)| p == path).map(|&(_, o, r)| (o, r))
    }

    /// Whether `obj` is a server: a lookup through it is forwarded, and waits for an answer.
    fn serves(&self, obj: usize) -> bool {
        matches!(self.objects[obj], Object::Server | Object::Answers(_) | Object::Channel { .. })
    }

    /// The server whose binding is the longest prefix of `path`, as the kernel resolves one.
    fn server_for(&mut self, ns: usize, path: &[u8]) -> Option<usize> {
        let under = |bound: &[u8]| bound == b"/" || path == bound || path.starts_with(&[bound, b"/"].concat());
        let found =
            self.bindings(ns).iter().filter(|(p, ..)| under(p)).max_by_key(|(p, ..)| p.len()).map(|(_, o, _)| *o);
        found.filter(|&o| self.serves(o))
    }
}

/// A path the kernel takes: absolute, `/`-separated, short enough, no empty, `.` or `..` part.
fn valid_path(path: &[u8]) -> bool {
    path.len() <= PATH_MAX
        && path.first() == Some(&b'/')
        && (path == b"/" || path[1..].split(|&b| b == b'/').all(|c| !c.is_empty() && c != b"." && c != b".."))
}

// --- the calls --------------------------------------------------------------

pub fn io_submit(resource: u64, op: &IoOp) -> i64 {
    with(|m| {
        m.submits += 1;
        m.submitted.push(*op);
        if let Some(status) = m.fail_submit.take() {
            return status as i64;
        }
        if let Err(r) = m.get(resource, Rights::empty()) {
            return r;
        }
        let (status, result, ready, never) = m.script.pop_front().unwrap_or((0, 0, true, false));
        m.po(Po { ready, never, status, result, lookup: None })
    })
}

/// As the kernel answers (`InvalidArgument`, `InvalidHandle`, an empty poll's `WouldBlock`, a
/// sleep on no handles); a deadline lets time pass, completing what will complete; a short
/// `results` or an endless wait panics.
pub(super) fn wait_exact(handles: &[u64], results: &mut [IoResult], deadline: u64) -> i64 {
    with(|m| {
        m.waits += 1;
        assert!(m.waits < 10_000, "a loop of waits that never ends");
        m.last_wait_count = handles.len();
        if handles.is_empty() && (deadline == 0 || deadline == u64::MAX) {
            return e(KError::InvalidArgument);
        }
        if handles.len() > libkern::MAX_WAIT_HANDLES {
            return e(KError::InvalidArgument);
        }
        let mut objs = Vec::new();
        for h in handles {
            match m.get(*h, Rights::WAIT) {
                Ok((o, _)) => objs.push(o),
                Err(r) => return r,
            }
        }
        if deadline != 0 {
            if let Some(r) = m.fail_wait.take() {
                return r;
            }
            pass_time(m);
        }
        let mut n = 0usize;
        for (&h, &obj) in handles.iter().zip(&objs) {
            let Some(mut res) = m.signalled(obj) else { continue };
            res.handle = h;
            if let Object::Po(Po { lookup: Some(target), status: 0, .. }) = &m.objects[obj] {
                let target = *target;
                let installed = m.install(target.0, target.1);
                if let Object::Po(po) = &mut m.objects[obj] {
                    po.lookup = None;
                    po.result = installed;
                }
                res.result = installed;
            }
            results[n] = res;
            n += 1;
        }
        match (n, deadline) {
            (0, 0) => e(KError::WouldBlock),
            (0, u64::MAX) => panic!("a wait for ever on handles none of which will signal"),
            (0, _) => {
                m.sleeps += 1;
                e(KError::TimedOut)
            }
            _ => n as i64,
        }
    })
}

/// What a wait that lets time pass completes: every I/O that will, and every bounded send held
/// past its deadline, `TimedOut` with its handles closed.
fn pass_time(m: &mut Mock) {
    m.clock += 1_000_000;
    let mut timed_out = Vec::new();
    for o in m.objects.iter_mut() {
        match o {
            Object::Po(po) if !po.never => po.ready = true,
            Object::Channel { held, .. } => {
                while let Some(i) = held.iter().position(|(_, _, bounded)| *bounded) {
                    let (_, po, _) = held.remove(i).expect("found");
                    timed_out.push(po);
                }
            }
            _ => {}
        }
    }
    for po in timed_out {
        if let Object::Po(p) = &mut m.objects[po] {
            p.ready = true;
            p.status = KError::TimedOut as i32;
        }
    }
}

pub fn clock_read(clock: u64, out: &mut u64) -> i64 {
    with(|m| match clock {
        libkern::CLOCK_MONOTONIC => {
            m.clock += 1_000;
            *out = m.clock;
            0
        }
        libkern::CLOCK_REALTIME => match m.realtime {
            Some(t) => {
                *out = t + m.clock;
                0
            }
            None => e(KError::Unsupported),
        },
        _ => e(KError::InvalidArgument),
    })
}

pub fn handle_close(h: u64) {
    with(|m| {
        m.closes += 1;
        match m.table.remove(&h) {
            Some((obj, _)) if matches!(&m.objects[obj], Object::Po(po) if !po.ready) => m.closed_in_flight += 1,
            Some(_) => {}
            None => m.stray_closes += 1,
        }
    });
}

pub fn handle_restrict(h: u64, mask: u64) -> i64 {
    with(|m| match m.table.get_mut(&h) {
        Some((_, rights)) => {
            *rights &= mask;
            0
        }
        None => e(KError::InvalidHandle),
    })
}

pub fn handle_duplicate(h: u64, rights: u64) -> i64 {
    with(|m| match m.get(h, Rights::DUPLICATE) {
        Ok((obj, have)) => m.install(obj, have & rights) as i64,
        Err(r) => r,
    })
}

pub fn handle_stat(h: u64, out: &mut HandleInfo) -> i64 {
    with(|m| {
        let (obj, rights) = match m.get(h, Rights::INSPECT) {
            Ok(got) => got,
            Err(r) => return r,
        };
        let size = match &m.objects[obj] {
            Object::Memory(b) => b.len() as u64,
            Object::File { size, .. } => *size,
            _ => 0,
        };
        *out = HandleInfo { rights, object_type: m.objects[obj].kind() as u32, generation: 0, size };
        0
    })
}

pub fn memory_create(size: u64) -> i64 {
    with(|m| match size {
        0 => e(KError::InvalidArgument),
        s if s > MEMORY_MAX => e(KError::TooLarge),
        s => {
            let bytes = Pages::new(s as usize);
            let all = Rights::MAP_READ | Rights::MAP_WRITE | Rights::MAP_EXEC;
            m.mint(Object::Memory(bytes), all | Rights::DUPLICATE | Rights::INSPECT | Rights::TRANSFER)
        }
    })
}

pub fn memory_map(h: u64, _hint: u64, size: u64, rights: u64) -> i64 {
    with(|m| {
        let (obj, have) = match m.get(h, Rights::empty()) {
            Ok(got) => got,
            Err(r) => return r,
        };
        let need = Rights::from_bits(rights) | Rights::MAP_READ;
        if !Rights::from_bits(have).contains(need) {
            return e(KError::NoAccess);
        }
        let room = match &mut m.objects[obj] {
            Object::Memory(b) => b.len(),
            Object::File { size, bytes } => {
                let room = (*size as usize).next_multiple_of(PAGE);
                assert!(
                    room <= bytes.len(),
                    "the mock backs a file with {FILE_ROOM} bytes; a map past them is the test's"
                );
                room
            }
            _ => return e(KError::InvalidArgument),
        };
        if size == 0 || size as usize > room {
            return e(KError::InvalidArgument);
        }
        let base = match &mut m.objects[obj] {
            Object::Memory(b) | Object::File { bytes: b, .. } => b.base(),
            _ => unreachable!("checked mappable"),
        };
        m.maps.entry(base).and_modify(|(_, n)| *n += 1).or_insert((obj, 1));
        base as i64
    })
}

/// # Safety
/// Nothing: the model frees no memory at an unmap.
pub unsafe fn memory_unmap(addr: u64, _size: u64) -> i64 {
    with(|m| match m.maps.get_mut(&addr) {
        Some((_, n)) if *n > 1 => {
            *n -= 1;
            0
        }
        Some(_) => {
            m.maps.remove(&addr);
            0
        }
        None => e(KError::InvalidArgument),
    })
}

pub fn notif_recv(queue: u64, out: &mut Notification) -> i64 {
    with(|m| {
        let obj = match m.typed(queue, Rights::empty(), KObjectType::NotificationChannel) {
            Ok((o, _)) => o,
            Err(r) => return r,
        };
        let Object::Notify(q) = &mut m.objects[obj] else { unreachable!("checked") };
        match q.pop_front() {
            Some(n) => {
                *out = n;
                0
            }
            None => e(KError::WouldBlock),
        }
    })
}

pub fn channel_create(depth: u64, ends: &mut [u64; 2]) -> i64 {
    with(|m| {
        if depth > libkern::IPC_MAX_QUEUE_DEPTH as u64 {
            return e(KError::InvalidArgument);
        }
        let depth = if depth == 0 { 16 } else { depth as usize };
        let a = m.objects.len();
        let ring = || VecDeque::new();
        m.object(Object::Channel { peer: a + 1, ring: ring(), held: VecDeque::new(), depth, forward: false });
        m.object(Object::Channel { peer: a, ring: ring(), held: VecDeque::new(), depth, forward: false });
        let rights =
            Rights::SEND | Rights::RECV | Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT | Rights::WAIT;
        *ends = [m.install(a, rights.bits()), m.install(a + 1, rights.bits())];
        0
    })
}

pub fn channel_send(ch: u64, msg: &IpcMsg, handles: &[u64], mode: u64, _deadline: u64) -> i64 {
    with(|m| {
        if mode > libkern::SENDMODE_BLOCKBOUNDED {
            return e(KError::InvalidArgument);
        }
        if handles.len() > IPC_HANDLE_MAX {
            return e(KError::TooLarge);
        }
        if (1..handles.len()).any(|i| handles[..i].contains(&handles[i])) {
            return e(KError::InvalidArgument);
        }
        let ours = match m.typed(ch, Rights::SEND, KObjectType::IpcChannel) {
            Ok((o, _)) => o,
            Err(r) => return r,
        };
        let Object::Channel { peer, forward, .. } = m.objects[ours] else { unreachable!("checked") };
        let mut moved = Vec::new();
        for &h in handles {
            match m.get(h, Rights::TRANSFER) {
                Ok((o, _)) if o == peer => return e(KError::InvalidArgument),
                Ok(got) => moved.push(got),
                Err(r) => return r,
            }
        }
        if msg.header.payload_len as usize > IPC_PAYLOAD_SIZE {
            return e(KError::InvalidArgument);
        }
        if forward {
            // A reply the kernel consumes within the call, whatever the mode.
            for h in handles {
                m.table.remove(h);
            }
            return 0;
        }
        if !m.held(peer) {
            return e(KError::PeerClosed);
        }
        let mut copy = Box::new(*msg);
        copy.header.sender_pid = 1;
        copy.header.handle_count = moved.len() as u8;
        let q = Queued { msg: copy, handles: moved };
        let Object::Channel { ring, depth, .. } = &m.objects[peer] else { unreachable!("a channel's peer") };
        let full = ring.len() >= *depth;
        if full && mode == libkern::SENDMODE_NOBLOCK {
            return e(KError::WouldBlock);
        }
        for h in handles {
            m.table.remove(h);
        }
        let po = (mode != libkern::SENDMODE_NOBLOCK)
            .then(|| m.po(Po { ready: !full, never: false, status: 0, result: 0, lookup: None }));
        let po_obj = po.map(|po| m.table[&(po as u64)].0);
        let Object::Channel { ring, held, .. } = &mut m.objects[peer] else { unreachable!("a channel's peer") };
        match po_obj {
            Some(po) if full => held.push_back((q, po, mode == libkern::SENDMODE_BLOCKBOUNDED)),
            _ => ring.push_back(q),
        }
        po.unwrap_or(0)
    })
}

pub fn channel_recv(ch: u64, msg: &mut IpcMsg, handles: &mut [u64; IPC_HANDLE_MAX], count: &mut usize) -> i64 {
    with(|m| {
        let ours = match m.typed(ch, Rights::RECV, KObjectType::IpcChannel) {
            Ok((o, _)) => o,
            Err(r) => return r,
        };
        if let Some(status) = m.fail_recv.take() {
            return status as i64;
        }
        let Object::Channel { peer, ring, held, .. } = &mut m.objects[ours] else { unreachable!("checked") };
        let peer = *peer;
        let Some(q) = ring.pop_front() else {
            return if m.held(peer) { e(KError::WouldBlock) } else { e(KError::PeerClosed) };
        };
        let next = held.pop_front();
        if let Some((q2, po, _)) = next {
            ring.push_back(q2);
            if let Object::Po(p) = &mut m.objects[po] {
                p.ready = true;
            }
        }
        *msg = *q.msg;
        *handles = [0; IPC_HANDLE_MAX];
        for (i, &(obj, rights)) in q.handles.iter().enumerate() {
            handles[i] = m.install(obj, rights);
        }
        msg.handles = *handles;
        *count = q.handles.len();
        0
    })
}

pub fn ns_create() -> i64 {
    with(|m| {
        let rights = Rights::LOOKUP | Rights::BIND | Rights::UNBIND | Rights::DUPLICATE | Rights::TRANSFER;
        m.mint(Object::Namespace(Vec::new()), rights | Rights::INSPECT)
    })
}

pub fn ns_derive(ns: u64) -> i64 {
    with(|m| match m.namespace(ns, Rights::LOOKUP) {
        Ok(src) => {
            let copy = m.bindings(src).clone();
            let rights = Rights::LOOKUP | Rights::BIND | Rights::UNBIND | Rights::DUPLICATE | Rights::TRANSFER;
            m.mint(Object::Namespace(copy), rights | Rights::INSPECT)
        }
        Err(r) => r,
    })
}

pub fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> i64 {
    with(|m| {
        let ns = match m.namespace(ns, Rights::LOOKUP) {
            Ok(o) => o,
            Err(r) => return r,
        };
        if !valid_path(path) {
            return e(KError::InvalidArgument);
        }
        if let Some(server) = m.server_for(ns, path) {
            if let Object::Answers(target) = m.objects[server] {
                return m.lookup_po(Ok((target, rights)));
            }
            let answer = m.object(Object::Memory(Pages::new(PAGE)));
            let lookup = Some((answer, rights));
            return m.po(Po { ready: false, never: false, status: 0, result: 0, lookup });
        }
        let found = m.file_at(ns, path).map(|(o, bound)| (o, bound & rights)).ok_or(KError::NotFound);
        m.lookup_po(found)
    })
}

/// As the kernel's: the syscap, the namespace, the paths, then the resource. An endpoint binds as
/// a server, needing no right and taking a base; anything else directly, needing `TRANSFER`.
pub fn ns_bind(ns: u64, path: &[u8], resource: u64, base: &[u8]) -> i64 {
    with(|m| {
        if let Some(status) = m.fail_bind.take() {
            return status as i64;
        }
        let ns = match m.namespace(ns, Rights::BIND) {
            Ok(o) => o,
            Err(r) => return r,
        };
        if !valid_path(path) || !(base.is_empty() || (valid_path(base) && base != b"/" && base.len() <= 128)) {
            return e(KError::InvalidArgument);
        }
        let (obj, rights) = match m.get(resource, Rights::empty()) {
            Ok(got) => got,
            Err(r) => return r,
        };
        if !matches!(m.objects[obj], Object::Channel { .. }) {
            if !base.is_empty() {
                return e(KError::InvalidArgument);
            }
            if !Rights::from_bits(rights).contains(Rights::TRANSFER) {
                return e(KError::NoAccess);
            }
        }
        if m.file_at(ns, path).is_some() {
            return e(KError::AlreadyExists);
        }
        m.bindings(ns).push((path.to_vec(), obj, rights));
        0
    })
}

pub fn ns_unbind(ns: u64, path: &[u8]) -> i64 {
    with(|m| {
        let ns = match m.namespace(ns, Rights::UNBIND) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let b = m.bindings(ns);
        match b.iter().position(|(p, ..)| p == path) {
            Some(i) => {
                b.remove(i);
                0
            }
            None => e(KError::NotFound),
        }
    })
}

pub fn ns_enumerate(ns: u64, index: u64, out: &mut NsEntry) -> i64 {
    with(|m| {
        let ns = match m.namespace(ns, Rights::LOOKUP) {
            Ok(o) => o,
            Err(r) => return r,
        };
        let Some((path, _, rights)) = m.bindings(ns).get(index as usize) else { return e(KError::NotFound) };
        *out = NsEntry::zeroed();
        let n = path.len().min(out.path.len());
        out.path[..n].copy_from_slice(&path[..n]);
        out.path_len = path.len() as u32;
        out.rights = *rights;
        0
    })
}

fn mount_answer(ns: u64, path: &[u8], held: bool) -> i64 {
    with(|m| {
        if let Err(r) = m.namespace(ns, Rights::LOOKUP) {
            return r;
        }
        match m.mounts.get(path) {
            Some(&(written, still)) => {
                if held {
                    return still;
                }
                m.fail_sync.take().map_or(written, |s| s as i64)
            }
            None => e(KError::NotFound),
        }
    })
}

pub fn ns_sync(ns: u64, path: &[u8]) -> i64 {
    mount_answer(ns, path, false)
}

pub fn ns_held(ns: u64, path: &[u8]) -> i64 {
    mount_answer(ns, path, true)
}

/// The file verbs, as a filesystem server mounted over the whole namespace would answer them: a
/// create is not exclusive, a grow never shrinks, a truncate never grows.
pub fn file_resize(verb: Resize, ns: u64, path: &[u8], rights: u64, size: u64) -> i64 {
    with(|m| {
        if size > u32::MAX as u64 {
            return e(KError::TooLarge);
        }
        let ns = match m.namespace(ns, Rights::LOOKUP) {
            Ok(o) => o,
            Err(r) => return r,
        };
        if !valid_path(path) {
            return e(KError::InvalidArgument);
        }
        let found = match (m.file_at(ns, path), verb) {
            (None, Resize::Create) => {
                let obj = m.object(Object::File { bytes: Pages::new(FILE_ROOM), size: 0 });
                let all = Rights::MAP_READ | Rights::MAP_WRITE | Rights::INSPECT | Rights::TRANSFER | Rights::DUPLICATE;
                m.bindings(ns).push((path.to_vec(), obj, all.bits()));
                Some((obj, all.bits()))
            }
            (None, _) => None,
            (Some(got), _) => Some(got),
        };
        let Some((obj, bound)) = found else { return m.lookup_po(Err(KError::NotFound)) };
        if let Object::File { size: now, .. } = &mut m.objects[obj] {
            *now = match verb {
                Resize::Create | Resize::Grow => (*now).max(size),
                Resize::Truncate => (*now).min(size),
            };
        }
        m.lookup_po(Ok((obj, bound & rights)))
    })
}

pub fn file_rename(ns: u64, from: &[u8], to: &[u8], _flags: u64) -> i64 {
    with(|m| {
        let ns = match m.namespace(ns, Rights::LOOKUP) {
            Ok(o) => o,
            Err(r) => return r,
        };
        if !valid_path(from) || !valid_path(to) {
            return e(KError::InvalidArgument);
        }
        let b = m.bindings(ns);
        let status = match b.iter().position(|(p, ..)| p == from) {
            Some(i) => {
                b.retain(|(p, ..)| p != to);
                let i = b.iter().position(|(p, ..)| p == from).unwrap_or(i);
                b[i].0 = to.to_vec();
                0
            }
            None => KError::NotFound as i32,
        };
        let lookup = (status == 0 && core::mem::take(&mut m.rename_handle))
            .then(|| (m.object(Object::Memory(Pages::new(PAGE))), Rights::MAP_READ.bits()));
        m.po(Po { ready: true, never: false, status, result: 0, lookup })
    })
}

pub fn file_sync(file: u64) -> i64 {
    with(|m| match m.typed(file, Rights::MAP_WRITE, KObjectType::FileObject) {
        Ok(_) => m.fail_sync.take().map_or(0, |s| s as i64),
        Err(r) => r,
    })
}

pub fn process_spawn(args: &SpawnArgs) -> i64 {
    with(|m| {
        match m.get(args.image, Rights::MAP_READ) {
            Ok((o, _)) if matches!(m.objects[o], Object::Memory(_) | Object::File { .. }) => {}
            Ok(_) => return e(KError::InvalidArgument),
            Err(r) => return r,
        }
        let count = args.handle_count as usize;
        if count > SPAWN_MAX_HANDLES {
            return e(KError::TooLarge);
        }
        let hs = &args.handles[..count];
        if (1..count).any(|i| hs[..i].contains(&hs[i])) {
            return e(KError::InvalidArgument);
        }
        for (i, &h) in hs.iter().enumerate() {
            let copy = args.move_mask & (1 << i) == 0;
            let need = if copy { Rights::TRANSFER | Rights::DUPLICATE } else { Rights::TRANSFER };
            if let Err(r) = m.get(h, need) {
                return r;
            }
        }
        if args.namespace != 0
            && let Err(r) = m.namespace(args.namespace, Rights::LOOKUP)
        {
            return r;
        }
        for (i, h) in hs.iter().enumerate() {
            if args.move_mask & (1 << i) != 0 {
                m.table.remove(h);
            }
        }
        m.spawned.push(*args);
        m.mint(Object::Process, Rights::SIGNAL | Rights::TERMINATE)
    })
}

/// A request the model records and the child, which never runs, ignores.
pub fn process_terminate(process: u64) -> i64 {
    with(|m| match m.typed(process, Rights::SIGNAL, KObjectType::Process) {
        Ok(_) => {
            m.terminated += 1;
            0
        }
        Err(r) => r,
    })
}

pub fn process_exit(status: i32) -> ! {
    panic!("exit({status})")
}

/// # Safety
/// Nothing: the model starts no thread.
pub unsafe fn thread_create(args: &ThreadArgs) -> i64 {
    with(|m| {
        if args.entry == 0 || args.user_sp == 0 || args._reserved.iter().any(|&b| b != 0) {
            return e(KError::InvalidArgument);
        }
        if let Some(status) = m.fail_thread.take() {
            return status as i64;
        }
        m.threads.push(*args);
        m.mint(Object::Thread, Rights::SIGNAL | Rights::TERMINATE | Rights::INSPECT | Rights::DUPLICATE)
    })
}

pub fn entropy_create() -> i64 {
    with(|m| m.mint(Object::Entropy, Rights::READ | Rights::DUPLICATE | Rights::TRANSFER | Rights::INSPECT))
}

pub fn entropy_read(h: u64, buf: &mut [u8]) -> i64 {
    with(|m| {
        if buf.is_empty() {
            return 0;
        }
        if buf.len() > 256 {
            return e(KError::TooLarge);
        }
        if let Err(r) = m.typed(h, Rights::READ, KObjectType::EntropyObject) {
            return r;
        }
        if m.unseeded > 0 {
            m.unseeded -= 1;
            return m.po(Po { ready: false, never: false, status: 0, result: 0, lookup: None });
        }
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i as u8).wrapping_mul(37).wrapping_add(11);
        }
        0
    })
}

pub fn kprint(bytes: &[u8]) {
    with(|m| m.log.extend_from_slice(bytes));
}

mod controls;
pub use controls::*;
