//! `profile-server` — projects the content-addressed store into `/bin` and `/applications`.
//!
//! Bound by a supervisor (init), it answers forwarded `Namespace::Resolve` lookups by
//! finding the first package in manifest order that provides the name, and re-exporting
//! that store `FileObject` handle. It is pure name resolution: it holds no file content
//! and stays out of the data path. See `docs/architecture/profiles-and-namespace-projection.md`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libkern::{KError, MAX_WAIT_HANDLES, RIGHT_MAP_WRITE, RIGHT_TRANSFER, RIGHT_WRITE, Rights};
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{Carried, Channel, Deadline, File, Handle, MapRead, Msg, Namespace, NsReadOnly, Only, WaitSet, block_on};
use librsproto::namespace::{OBJECT_KIND_MEMOBJ, RESOLVE_REPLY_LEN, parse_resolve_request, resolve_reply};
use librsproto::server;
use librsproto::session::Dir;
use librsproto::{OP_FILE_READ_DIR, OP_NS_RESOLVE, RS_FLAG_REPLY, RS_MAX_BODY, decode};
use profile_server::manifest::{self, Package};
use profile_server::projection::split_suffix;

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

type Endpoint = Handle<Channel, Only>;

/// One projected program, with the metadata the store's own directory reported for it: the
/// view carries real sizes and timestamps rather than inventing numbers for files it does
/// not own.
struct Entry {
    name: String,
    /// Index into the manifest's package list.
    pkg: usize,
    inode: u32,
    kind: u8,
    mode: u16,
    size: u64,
    mtime: i64,
}

/// The most open directory sessions at once; one `sys_wait` slot is the forwarding endpoint.
const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 1;

/// An open directory session: the kept (server) endpoint, and the projection it is listing.
struct Session {
    ch: Endpoint,
    dir: &'static str,
}

/// What a wait's index names.
#[derive(Copy, Clone)]
enum Waited {
    Resolve,
    Session(usize),
}

/// What a forwarded resolve asks for.
enum Resolved {
    /// A projection's root: a directory session over it.
    Dir(&'static str),
    /// A program in the store.
    File(Handle<File, MapRead>),
    Refused,
}

struct Server {
    /// The namespace that resolves `/store` and the manifest.
    root: Option<Handle<Namespace, NsReadOnly>>,
    packages: Vec<Package>,
    /// The merged view per projection, built once on first use.
    ///
    /// Caching needs no invalidation: a store path is content-addressed, so its contents
    /// cannot change, and the manifest (the profile's membership) is read once at startup.
    /// See `TODO(profile-generation-refresh)`.
    index: Vec<(String, Vec<Entry>)>,
    serve_end: Endpoint,
    /// Open directory sessions; a free slot is `None`.
    sessions: [Option<Session>; MAX_SESSIONS],
}

/// Read and parse the manifest at `/system/profiles/system.toml` on the root filesystem,
/// which `init` has mounted before spawning this process. Empty on any failure: the server
/// then resolves nothing.
fn read_manifest(root: Option<&Handle<Namespace, NsReadOnly>>) -> Vec<Package> {
    let found = root.and_then(|root| {
        block_on(root.lookup::<File, MapRead>("/system/profiles/system.toml", Rights::MAP_READ | Rights::INSPECT)).ok()
    });
    let Some(mem) = found else {
        log::write(b"profile-server: no system profile manifest\n");
        return Vec::new();
    };
    let size = mem.stat().map_or(0, |info| info.size as usize);
    let mut bytes = Vec::new();
    // The mapping keeps the file after `mem` is closed; it is copied out and unmapped at once.
    let read = size != 0
        && bytes.try_reserve_exact(size).is_ok()
        && mem
            .map(size)
            .and_then(|view| {
                bytes.resize(size, 0);
                view.read_into(0, &mut bytes)
            })
            .is_ok();
    drop(mem);
    if !read {
        log::write(b"profile-server: the system profile manifest does not read\n");
        return Vec::new();
    }
    core::str::from_utf8(&bytes).map(manifest::parse).unwrap_or_default()
}

/// Build the merged view of one projection from a real `readdir` of each package's `<dir>/`.
/// Packages are read in manifest order and the first provider of a name wins, the same
/// precedence resolve uses: a listing that disagreed with what a spawn finds would be worse
/// than none.
///
/// A package whose directory cannot be opened is skipped, not fatal: one broken package
/// must not make every program unreachable.
fn build_index(root: &Handle<Namespace, NsReadOnly>, packages: &[Package], dir: &str) -> Vec<Entry> {
    let mut out: Vec<Entry> = Vec::new();
    for (i, pkg) in packages.iter().enumerate() {
        let path = format!("{}/{dir}", pkg.path);
        let mut msg = Msg::new();
        let mut listing = match Dir::resolve(root, &path, &mut msg) {
            Ok(d) => d,
            Err(_) => {
                log::write(b"profile-server: package subdirectory unreadable (skipped)\n");
                continue;
            }
        };
        let _ = listing.read_dir(|e| {
            if e.name != b"." && e.name != b".." {
                let name = match core::str::from_utf8(e.name) {
                    Ok(n) => String::from(n),
                    Err(_) => return true, // a non-UTF-8 program name is not addressable
                };
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
        listing.close();
    }
    out
}

/// What a program in the store is never looked up with: the kernel grants a resolve what this
/// server's reply carries, and the store is on a writable mount. The rest pass on, as what is
/// asked for says what is wanted: a directory session is refused on a file.
const WRITE_RIGHTS: u64 = RIGHT_WRITE | RIGHT_MAP_WRITE | Rights::APPEND.bits() | Rights::TRUNCATE.bits();

/// Pack entries from `cursor` until the reply is full; the body length, or `None` if even
/// the header did not fit.
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

impl Server {
    /// Where projection `dir`'s view is in `index`, built if it is not there yet. Lazy: eager
    /// building costs every boot a directory read per package over IPC for a view that may
    /// never be asked for.
    fn index_of(&mut self, dir: &str) -> usize {
        if let Some(at) = self.index.iter().position(|(d, _)| d == dir) {
            return at;
        }
        let built = match &self.root {
            Some(root) => build_index(root, &self.packages, dir),
            None => Vec::new(),
        };
        Line::new()
            .s(b"profile-server: /")
            .s(dir.as_bytes())
            .s(b" index built, ")
            .u(built.len() as u64)
            .s(b" entr(ies)")
            .end();
        self.index.push((String::from(dir), built));
        self.index.len() - 1
    }

    /// Resolve `suffix` (e.g. `bin/heartbeat`) to its store `FileObject` handle, with the requested
    /// rights less [`WRITE_RIGHTS`], plus `TRANSFER` so it can be re-exported. `None` if the profile
    /// provides no such program, or the suffix names a projection's root.
    ///
    /// Served from the same index as the listing so the two cannot disagree.
    fn resolve_in_store(&mut self, suffix: &[u8], rights: u64) -> Option<Handle<File, MapRead>> {
        let raw = core::str::from_utf8(suffix).ok()?;
        let (dir, name) = split_suffix(raw)?;
        if name.is_empty() {
            return None;
        }
        let at = self.index_of(dir);
        let e = self.index[at].1.iter().find(|e| e.name == name)?;
        let path = format!("{}/{dir}/{name}", self.packages[e.pkg].path);
        let rights = Rights::from_bits((rights & !WRITE_RIGHTS) | RIGHT_TRANSFER);
        block_on(self.root.as_ref()?.lookup(&path, rights)).ok()
    }

    /// Open a directory session over projection `dir`: mint a channel, keep the server end in a
    /// free slot, and reply with the client end. The session is bound to the view, not an inode.
    fn open_dir_session(&mut self, request_id: u64, dir: &'static str) {
        let serve_end = &self.serve_end;
        let Some(slot) = self.sessions.iter().position(Option::is_none) else {
            // Every slot in use — ask the client to retry rather than failing the listing.
            server::reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::WouldBlock, b"");
            return;
        };
        let Ok((client_end, session_end)) = Handle::pair(4) else {
            server::reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::KernelError, b"");
            return;
        };
        // Bind the slot *before* replying, so a fast client's first request cannot arrive
        // before the slot is live.
        self.sessions[slot] = Some(Session { ch: session_end, dir });
        if !server::reply_channel(serve_end, request_id, client_end) {
            self.sessions[slot] = None;
        }
    }

    /// Drain requests on an open directory session. `File::ReadDir` is answered from the index;
    /// anything else is `Unsupported`: the projection is read-only, so `mkdir`/`unlink` are
    /// refused rather than forwarded to the store.
    fn serve_session(&mut self, slot: usize) {
        let mut msg = Msg::new();
        loop {
            let Some(session) = &self.sessions[slot] else {
                return; // already freed earlier in this batch
            };
            if let Err(e) = session.ch.recv(&mut msg) {
                // A channel whose peer has closed stays signaled, so a dead session left in the
                // wait set makes `sys_wait` return at once, forever.
                if e.code() == KError::PeerClosed {
                    self.sessions[slot] = None;
                }
                return; // WouldBlock (drained) or PeerClosed (freed)
            }
            let dir = session.dir;
            let (op, request_id, cursor) = match decode(msg.payload()) {
                Ok(m) if m.op == OP_FILE_READ_DIR => {
                    (m.op, m.request_id, librsproto::file::parse_read_dir_request(m.body).map(|r| r.cursor))
                }
                Ok(m) => (m.op, m.request_id, None),
                Err(_) => (0, 0, None),
            };
            let Some(cursor) = cursor else {
                server::reply_error(&session.ch, op, request_id, KError::Unsupported, b"");
                continue;
            };
            let at = self.index_of(dir);
            let Some(session) = &self.sessions[slot] else { return };
            let mut body = [0u8; RS_MAX_BODY];
            let Some(blen) = pack_read_dir(&mut body, &self.index[at].1, cursor) else {
                server::reply_error(&session.ch, op, request_id, KError::KernelError, b"");
                continue;
            };
            let _ = server::send(
                &session.ch,
                OP_FILE_READ_DIR,
                request_id,
                RS_FLAG_REPLY,
                &body[..blen],
                &mut Carried::new(),
            );
        }
    }

    /// Answer one forwarded `Namespace::Resolve` on the serve endpoint.
    fn serve_resolve(&mut self) {
        let mut msg = Msg::new();
        let _sent = match self.serve_end.recv(&mut msg) {
            Ok(sent) => sent,
            // A forwarding endpoint with no peer is the end of this server, not a message to skip: the
            // kernel keeps a peer-closed channel signalled, so going round again would spin a CPU.
            Err(e) if e.code() == KError::PeerClosed => {
                log::write(b"profile-server: forwarding endpoint closed\n");
                libos::exit(1);
            }
            Err(_) => return,
        };
        let (op, request_id, resolved) = match decode(msg.payload()) {
            Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                // A suffix naming a projection's root is what `Dir::open` asks for: answered with
                // a session, a reply of a different shape from a file's.
                Some(r) => match core::str::from_utf8(r.suffix).ok().and_then(split_suffix) {
                    Some((dir, "")) => (m.op, m.request_id, Resolved::Dir(dir)),
                    _ => match self.resolve_in_store(r.suffix, r.requested_rights) {
                        Some(h) => (m.op, m.request_id, Resolved::File(h)),
                        None => (m.op, m.request_id, Resolved::Refused),
                    },
                },
                None => (m.op, m.request_id, Resolved::Refused),
            },
            Ok(m) => (m.op, m.request_id, Resolved::Refused),
            Err(_) => (0, 0, Resolved::Refused),
        };
        match resolved {
            Resolved::File(handle) => {
                let mut body = [0u8; RESOLVE_REPLY_LEN];
                let _ = resolve_reply(&mut body, OBJECT_KIND_MEMOBJ, 0);
                server::reply_handle(&self.serve_end, request_id, &body, handle);
            }
            Resolved::Dir(dir) => self.open_dir_session(request_id, dir),
            Resolved::Refused if op == OP_NS_RESOLVE => {
                server::reply_error(&self.serve_end, op, request_id, KError::NotFound, b"");
            }
            Resolved::Refused => {
                server::reply_error(&self.serve_end, op, request_id, KError::Unsupported, b"");
            }
        }
    }

    /// The serve loop: wait for a forwarded `Namespace::Resolve`, resolve it, and reply.
    fn serve_loop(&mut self) -> ! {
        log::write(b"profile-server: serving /bin over the store\n");
        loop {
            let mut names = [Waited::Resolve; MAX_WAIT_HANDLES];
            let signalled = {
                let mut set = WaitSet::<MAX_WAIT_HANDLES>::new();
                if set.add(&self.serve_end).is_err() {
                    continue;
                }
                for (slot, s) in self.sessions.iter().enumerate() {
                    if let Some(s) = s
                        && let Ok(i) = set.add(&s.ch)
                    {
                        names[i] = Waited::Session(slot);
                    }
                }
                set.wait(Deadline::NEVER)
            };
            let Ok(signalled) = signalled else { continue };
            let mut served_endpoint = false;
            for (i, _) in signalled.iter() {
                match names[i] {
                    Waited::Resolve => served_endpoint = true,
                    Waited::Session(slot) => self.serve_session(slot),
                }
            }
            if served_endpoint {
                self.serve_resolve();
            }
        }
    }
}

/// Bootstrap registers: `rsi` = the inherited root namespace (resolves `/store` and the
/// manifest), `rdx` = the control-channel endpoint init installed; `rdi` and `rcx` unused.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"profile-server: up\n");
    let root = start.root.take();
    let packages = read_manifest(root.as_ref());
    log::write(b"profile-server: manifest loaded\n");

    let control: Option<Endpoint> = start.take_handle0();
    let Ok((kernel_end, serve_end)) = Handle::pair(4) else {
        log::write(b"profile-server: channel create FAIL\n");
        libos::exit(1);
    };
    if !control.as_ref().is_some_and(|c| server::ready(c, b"profile-server", kernel_end)) {
        log::write(b"profile-server: Ready send FAIL\n");
        libos::exit(1);
    }
    // Init's control channel stays open for the process's life, as it always has.
    control.map(Handle::into_raw);
    let mut srv = Server { root, packages, index: Vec::new(), serve_end, sessions: [const { None }; MAX_SESSIONS] };
    srv.serve_loop();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"profile-server: PANIC\n");
    libos::exit(1);
}
