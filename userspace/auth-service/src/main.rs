//! `auth-service` — the userspace **authentication service**.
//!
//! A request/reply resource server that answers the `Auth` rsproto category (`Authenticate {
//! username, password } → { AUTHENTICATED, principal, home } | DENIED`,
//! `docs/spec/rsproto-auth-ops.md`). Like the fs / profile servers it is a namespace forwarder:
//! `service-mgr` binds `/svc/auth` to reach it and it mints a session channel per caller that
//! resolves there, which is what lets `session-mgr` and `desktop-session-mgr` each hold one. The
//! credential logic is the host-tested `auth_service` library; this binary is the bare-target
//! `libos` plumbing: read `/system/users`, hand the supervisor a client endpoint via
//! `Meta::Ready`, then serve.
//!
//! **And the database's only writer.** A resolve of `/svc/auth/admin` opens an *admin* session,
//! which answers `List`, `Add`, `Remove` and `SetPassword`. The view
//! broker is the one client meant to hold one, since it fronts every account operation and applies
//! the guards; anything holding the root namespace can resolve it, the same boundary `/svc/auth`
//! has always had (`TODO(svc-auth-ungated)`). A write replaces the file atomically — written to
//! `/system/users.new`, synced, then renamed over — and the copy in memory changes only once the
//! rename has held.
//!
//! `#![no_std]` + `#![no_main]`, **no `alloc`** — stack buffers, no `#[global_allocator]`
//! (the DB + messages are bounded). See `userspace/auth-service/CLAUDE.md`.

#![no_std]
#![no_main]

use auth_service::{Admin, serve_admin, serve_authenticate};
use libkern::{CTRL_OP_SHUTDOWN, KError, MAX_WAIT_HANDLES, Rights, scrub};
use libos::boot::Start;
use libos::control::{self, Control};
use libos::log::{self, Line};
use libos::{
    Carried, Channel, Deadline, File, Handle, MapRead, MapReadWrite, Msg, Namespace, NsReadOnly, Only, WaitSet,
    block_on, entropy, fs, time,
};
use librsproto::auth::{
    OP_AUTH_ADD, OP_AUTH_LIST, OP_AUTH_REMOVE, OP_AUTH_SET_PASSWORD, build_denied_reply, parse_account_request,
};
use librsproto::namespace::{RENAME_REPLACE, parse_resolve_request};
use librsproto::{OP_AUTHENTICATE, OP_NS_RESOLVE, RS_FLAG_ERROR, RS_FLAG_REPLY, RS_MAX_BODY, decode, server};

/// One page; the map granularity for reading the user DB.
const PAGE: usize = 4096;
/// Largest user database we hold: one page, and `libusers`' bound, so no writer of the file can
/// make one this server will not load.
const DB_MAX: usize = libusers::MAX_FILE;
const _: () = assert!(DB_MAX <= PAGE);
/// The file, and the name a new copy is written under before it is renamed over it.
const USERS: &str = "/system/users";
const USERS_NEW: &str = "/system/users.new";

/// How many clients may hold an auth session at once.
///
/// Two slots go to the forwarding endpoint and the control channel; the rest are sessions. The
/// limit is the wait set's, so it costs nothing to be generous.
const MAX_SESSIONS: usize = MAX_WAIT_HANDLES - 2;
const _: () = assert!(2 + MAX_SESSIONS <= MAX_WAIT_HANDLES);

type Endpoint = Handle<Channel, Only>;

/// A message scrubbed where it lies when it goes out of scope, on every way out: a password came
/// through it, and a move of the message would leave a copy.
struct Scrubbed(Msg);

impl Drop for Scrubbed {
    fn drop(&mut self) {
        scrub(self.0.payload_mut());
    }
}

/// What a wait's index names.
#[derive(Copy, Clone)]
enum Waited {
    Resolve,
    Control,
    Session(usize),
}

/// One client's session channel, and whether it is an **admin** one, which answers only the
/// administrative ops, or an oracle one.
struct Session {
    ch: Endpoint,
    admin: bool,
}

/// The user database and what it is read and written through.
struct Store {
    /// The user database, copied from `/system/users` at startup, and its length.
    db: [u8; DB_MAX],
    db_len: usize,
    /// An administrator's request writes the new database here first.
    new_db: [u8; DB_MAX],
    /// An admin reply's body — the account list is the long one.
    admin_reply: [u8; RS_MAX_BODY],
    /// The namespace the file is read and written through: the inherited root.
    root: Handle<Namespace, NsReadOnly>,
}

/// **This server's state**, in `_start`'s frame rather than in `static mut`s.
struct Server {
    store: Store,
    /// The forwarding endpoint; `None` once its peer has gone, when it would stay signalled for good.
    serve_end: Option<Endpoint>,
    /// The control channel, for `service --stop`; `None` once its supervisor has gone, when it
    /// would stay signalled for good.
    control: Option<Endpoint>,
    /// Open auth sessions, one per client that resolved `/svc/auth`.
    sessions: [Option<Session>; MAX_SESSIONS],
}

/// Add `h` to `set` as `what`.
fn watch<'a>(set: &mut WaitSet<'a, MAX_WAIT_HANDLES>, names: &mut [Waited], h: &'a Endpoint, what: Waited) {
    if let Ok(i) = set.add(h) {
        names[i] = what;
    }
}

/// How long a receive that left its message queued pauses, as a wait on the channel would fire
/// again at once.
const HELD_PAUSE_NS: u64 = 10_000_000;

/// What a failed receive on `ch` means for it.
enum Recv {
    /// The peer has gone and the ring is empty, for good.
    Closed,
    /// Nothing queued.
    Empty,
    /// Anything else: `OutOfMemory` leaves the message queued for a live client, and has been
    /// paused for already.
    Held,
}

/// Receive one message from `ch` into `msg`; on a failure, what it says about the channel.
fn recv_on(ch: &Endpoint, msg: &mut Msg) -> Result<(), Recv> {
    match ch.recv(msg) {
        Ok(_carried) => Ok(()),
        Err(e) if e.code() == KError::PeerClosed => Err(Recv::Closed),
        Err(e) if e.code() == KError::WouldBlock => Err(Recv::Empty),
        Err(_) => {
            let _ = time::sleep(HELD_PAUSE_NS);
            Err(Recv::Held)
        }
    }
}

/// Read `/system/users` into the store's copy, recording its length. `false` on any
/// failure: a missing DB must never authenticate.
fn load_db(store: &mut Store) -> bool {
    let Ok(file) = block_on(store.root.lookup::<File, MapRead>(USERS, Rights::MAP_READ | Rights::INSPECT)) else {
        return false;
    };
    let size = match file.stat() {
        Ok(info) if info.size != 0 && info.size as usize <= DB_MAX => info.size as usize,
        _ => return false,
    };
    let Ok(mapping) = file.map(PAGE) else {
        return false;
    };
    // `size` <= DB_MAX <= PAGE, so the copy lies in the mapping.
    if mapping.read_into(0, &mut store.db[..size]).is_err() {
        return false;
    }
    store.db_len = size;
    true
}

fn send_reply(ch: &Endpoint, op: u16, request_id: u64, body: &[u8], error: bool) {
    let flags = if error { RS_FLAG_REPLY | RS_FLAG_ERROR } else { RS_FLAG_REPLY };
    let _ = server::send(ch, op, request_id, flags, body, &mut Carried::new());
}

impl Server {
    /// The serve loop. Never returns.
    ///
    /// **It exits on `CTRL_OP_SHUTDOWN`**, which a shutdown sends every service (`service --stop`
    /// never does: `auth-service` is `essential`). Nothing needs saving: every write to the users
    /// file is made whole — a new file, synced, then renamed over — before its answer is sent.
    fn serve_loop(&mut self) -> ! {
        log::write(b"auth-service: serving Auth::Authenticate over /svc/auth, and administration at /svc/auth/admin\n");
        loop {
            let mut names = [Waited::Resolve; MAX_WAIT_HANDLES];
            let signalled = {
                let mut set = WaitSet::new();
                if let Some(e) = &self.serve_end {
                    watch(&mut set, &mut names, e, Waited::Resolve);
                }
                if let Some(c) = &self.control {
                    watch(&mut set, &mut names, c, Waited::Control);
                }
                for (slot, s) in self.sessions.iter().enumerate() {
                    if let Some(s) = s {
                        watch(&mut set, &mut names, &s.ch, Waited::Session(slot));
                    }
                }
                set.wait(Deadline::NEVER)
            };
            let Ok(signalled) = signalled else { continue };
            for (i, _) in signalled.iter() {
                match names[i] {
                    Waited::Resolve => self.serve_resolve(),
                    Waited::Control => {
                        if control::recv(&mut self.control) == Control::Op(CTRL_OP_SHUTDOWN) {
                            log::write(b"auth-service: asked to stop, exiting\n");
                            libos::exit(0);
                        }
                    }
                    Waited::Session(slot) => self.serve_session(slot),
                }
            }
        }
    }

    /// Answer a resolve of `/svc/auth` by minting a session channel for the caller, so one oracle
    /// serves several supervisors (`docs/architecture/session-and-auth.md`). Once its peer has
    /// gone the endpoint is signalled for good and leaves the wait set.
    fn serve_resolve(&mut self) {
        let Some(serve_end) = &self.serve_end else { return };
        let mut msg = Msg::new();
        if let Err(failure) = recv_on(serve_end, &mut msg) {
            if matches!(failure, Recv::Closed) {
                self.serve_end = None;
            }
            return;
        }
        let (op, request_id, ok) = {
            match decode(msg.payload()) {
                // **Two suffixes, nothing else**: empty is the oracle, `admin` the administrative one.
                Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                    Some(r) if r.suffix.is_empty() => (m.op, m.request_id, Some(false)),
                    Some(r) if r.suffix == b"admin" => (m.op, m.request_id, Some(true)),
                    _ => (m.op, m.request_id, None),
                },
                Ok(m) => (m.op, m.request_id, None),
                Err(_) => (0, 0, None),
            }
        };
        let Some(admin) = ok else {
            server::reply_error(serve_end, op, request_id, KError::NotFound, b"");
            return;
        };
        open_auth_session(&mut self.sessions, serve_end, request_id, admin);
    }

    fn serve_session(&mut self, slot: usize) {
        let Some(Session { ch, admin }) = &self.sessions[slot] else { return };
        // **Scrubbed on every return**: an `Authenticate`, `Add` or `SetPassword` carries a password.
        let mut msg = Scrubbed(Msg::new());
        if let Err(failure) = recv_on(ch, &mut msg.0) {
            // **A closed peer frees the slot.** Without this a supervisor that exited would hold
            // one forever, and the set of clients would shrink by one per logout. **Nothing else
            // does**: `OutOfMemory` leaves the message queued for a live client.
            if matches!(failure, Recv::Closed) {
                self.sessions[slot] = None;
            }
            return;
        }
        if *admin {
            return serve_admin_request(&mut self.store, ch, msg.0.payload());
        }

        let mut reply_body = [0u8; 512];
        let store = &self.store;
        let db = &store.db[..store.db_len];
        let (op, request_id, reply_len, error) = match decode(msg.0.payload()) {
            Ok(m) if m.op == OP_AUTHENTICATE => match serve_authenticate(m.body, db, &mut reply_body) {
                Some(n) => (m.op, m.request_id, n, false),
                None => {
                    let n = build_denied_reply(&mut reply_body).unwrap_or(0);
                    (m.op, m.request_id, n, true)
                }
            },
            // A non-Auth op on this channel: deny + error (wrong protocol).
            Ok(m) => {
                let n = build_denied_reply(&mut reply_body).unwrap_or(0);
                (m.op, m.request_id, n, true)
            }
            Err(_) => (0, 0, 0, true),
        };
        if reply_len > 0 {
            send_reply(ch, op, request_id, &reply_body[..reply_len], error);
        }
    }
}

/// Mint an oracle or `admin` session channel and hand its client end back in the resolve reply.
fn open_auth_session(
    sessions: &mut [Option<Session>; MAX_SESSIONS],
    serve_end: &Endpoint,
    request_id: u64,
    admin: bool,
) {
    let Some(slot) = sessions.iter().position(Option::is_none) else {
        // Every slot in use — ask the caller to retry rather than denying a login outright.
        server::reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::WouldBlock, b"");
        return;
    };
    let Ok((client_end, session_end)) = Handle::pair(4) else {
        server::reply_error(serve_end, OP_NS_RESOLVE, request_id, KError::KernelError, b"");
        return;
    };
    // Bind the slot *before* replying, so a fast client's first request cannot arrive before
    // the slot is live.
    sessions[slot] = Some(Session { ch: session_end, admin });
    if !server::reply_channel(serve_end, request_id, client_end) {
        sessions[slot] = None;
    }
}

/// **Serve one administrator's request** on an admin session. A write is installed before it is
/// answered, and the copy in memory changes only once the install holds.
fn serve_admin_request(store: &mut Store, ch: &Endpoint, req: &[u8]) {
    let Ok(m) = decode(req) else {
        return;
    };
    // The name the request is about, for the log: the body of a `Remove`, the first field of an
    // `Add` or a `SetPassword`. Never the password.
    let name: &[u8] = match m.op {
        OP_AUTH_REMOVE => m.body,
        OP_AUTH_ADD | OP_AUTH_SET_PASSWORD => parse_account_request(m.body).map_or(&[], |r| r.username),
        _ => &[],
    };
    let needs_salt = matches!(m.op, OP_AUTH_ADD | OP_AUTH_SET_PASSWORD);
    let salt = if needs_salt { fresh_salt(&store.root) } else { Some([0u8; libusers::SALT_LEN]) };
    let Some(salt) = salt else {
        return refuse_admin(ch, m.op, m.request_id, name, KError::IoError, b"no entropy for a salt");
    };
    let outcome =
        serve_admin(m.op, m.body, &store.db[..store.db_len], &salt, &mut store.new_db, &mut store.admin_reply);
    match outcome {
        Admin::Reply(n) => send_reply(ch, m.op, m.request_id, &store.admin_reply[..n], false),
        Admin::Write(n) => {
            if let Err(why) = install(&store.root, &store.new_db[..n]) {
                return refuse_admin(ch, m.op, m.request_id, name, KError::IoError, why);
            }
            store.db[..n].copy_from_slice(&store.new_db[..n]);
            store.db_len = n;
            let did: &[u8] = match m.op {
                OP_AUTH_ADD => b"added ",
                OP_AUTH_REMOVE => b"removed ",
                _ => b"set a new password for ",
            };
            Line::new().s(b"auth-service: ").s(did).untrusted(name).end();
            send_reply(ch, m.op, m.request_id, &[], false);
        }
        Admin::Refused(r) => refuse_admin(ch, m.op, m.request_id, name, r.kerror(), r.why()),
    }
}

/// Refuse an administrator's request with `err` and `why`, and say so in the log.
fn refuse_admin(ch: &Endpoint, op: u16, request_id: u64, name: &[u8], err: KError, why: &[u8]) {
    let asked: &[u8] = match op {
        OP_AUTH_LIST => b"List",
        OP_AUTH_ADD => b"Add",
        OP_AUTH_REMOVE => b"Remove",
        OP_AUTH_SET_PASSWORD => b"SetPassword",
        _ => b"a request that is not administrative",
    };
    let mut l = Line::new();
    l.s(b"auth-service: refused ").s(asked);
    if !name.is_empty() {
        l.s(b" for ").untrusted(name);
    }
    l.s(b": ").s(why).end();
    server::reply_error(ch, op, request_id, err, why);
}

/// A new password's salt, from the kernel's generator `root` binds; `None` if it cannot be made
/// or fails.
fn fresh_salt(root: &Handle<Namespace, NsReadOnly>) -> Option<[u8; libusers::SALT_LEN]> {
    let mut salt = [0u8; libusers::SALT_LEN];
    entropy::fill(root, &mut salt).ok().map(|()| salt)
}

/// **Replace `/system/users` with `bytes`, atomically.** A leftover `users.new`, from a write that
/// died, is cut to nothing; the new copy is created at its size, written through a mapping,
/// unmapped, synced, and then renamed over the file. A crash before the rename leaves the old file
/// as it was, and the rename is the filesystem's one operation. `Err` names the step that failed.
fn install(ns: &Handle<Namespace, NsReadOnly>, bytes: &[u8]) -> Result<(), &'static [u8]> {
    let rights = Rights::MAP_READ | Rights::MAP_WRITE;
    // A leftover, cut to nothing. Absent is the usual case, and not an error.
    let _: libos::Result<Handle<File, MapReadWrite>> = block_on(fs::truncate(ns, USERS_NEW, 0, rights));
    let made: libos::Result<Handle<File, MapReadWrite>> =
        block_on(fs::create(ns, USERS_NEW, bytes.len() as u64, rights));
    let Ok(file) = made else {
        return Err(b"users.new could not be created");
    };
    if !bytes.is_empty() {
        let Ok(mut mapping) = file.map(PAGE) else {
            return Err(b"users.new could not be mapped");
        };
        // The file is at most a page (DB_MAX <= PAGE). Unmapped before the sync, as `libfs` does: a
        // write-back that begins with a writable mapping cannot call the file clean.
        if mapping.write_from(0, bytes).is_err() {
            return Err(b"users.new could not be mapped");
        }
    }
    let synced = block_on(fs::flush(&file));
    drop(file);
    if synced.is_err() {
        return Err(b"users.new could not be written back");
    }
    block_on(fs::rename(ns, USERS_NEW, USERS, RENAME_REPLACE))
        .map_err(|_| &b"users.new could not be renamed over users"[..])
}

/// Bootstrap registers: `rdi` = notification channel (unused), `rsi` = the inherited
/// root namespace (resolves `/system/users`), `rdx` = the control-channel endpoint the
/// supervisor installed, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"auth-service: up\n");
    let control: Option<Endpoint> = start.take_handle0();
    // A missing root loads nothing, as a root that resolves nothing does.
    let Some(root) = start.root.take() else {
        log::write(b"auth-service: /system/users load FAIL\n");
        libos::exit(1);
    };
    let mut srv = Server {
        store: Store { db: [0; DB_MAX], db_len: 0, new_db: [0; DB_MAX], admin_reply: [0; RS_MAX_BODY], root },
        serve_end: None,
        control,
        sessions: [const { None }; MAX_SESSIONS],
    };
    if !load_db(&mut srv.store) {
        // No DB → the server would deny everything; that is a misconfiguration, so
        // fail loudly rather than run a useless authenticator.
        log::write(b"auth-service: /system/users load FAIL\n");
        libos::exit(1);
    }
    log::write(b"auth-service: user database loaded\n");

    let Ok((client_end, serve_end)) = Handle::pair(4) else {
        log::write(b"auth-service: channel create FAIL\n");
        libos::exit(1);
    };
    // NoBlock: the control inbox starts empty.
    if !srv.control.as_ref().is_some_and(|c| server::ready(c, b"auth-service", client_end)) {
        log::write(b"auth-service: Ready send FAIL\n");
        libos::exit(1);
    }
    srv.serve_end = Some(serve_end);
    srv.serve_loop();
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    log::write(b"auth-service: PANIC\n");
    libos::exit(1);
}
