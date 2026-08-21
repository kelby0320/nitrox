//! `session-mgr` — the userspace **session manager** (Phase 3, auth + session slice).
//!
//! The Tier-5 supervisor that logs a user in and hands them a sandboxed shell. It
//! holds re-delegated `BIND_NAMESPACE` (from service-mgr) and the building-block
//! endpoints — the fs-server forwarding endpoint + a channel to auth-service — from
//! which it composes each login's per-user namespace. See
//! `docs/architecture/session-and-auth.md`.
//!
//! **What it does:** receive the handed-over endpoints, prompt for a credential on a
//! terminal it opens, authenticate it against auth-service over the auth channel, construct
//! a session namespace binding the user's `/home` as a subtree of the fs-server
//! (`BIND_NAMESPACE` + subtree scoping + shared registration), spawn the user shell into it,
//! reap it, tear the session down, and prompt again.
//!
//! **One login path, in every build.** This file has no build-mode `cfg` and the crate
//! declares no features. It used to auto-log-in a hardcoded credential under `test-harness`
//! and run a fixed `-c` script, with the interactive `login()` and the whole `tty_*` layer
//! compiled out — so the gate that adjudicated the boot proved a string comparison worked.
//! Removed 2026-08-21; `cargo xtask test-interactive` types at the real prompt instead. See
//! `docs/planning/test-path-retrofit.md`, and `userspace/session-mgr/CLAUDE.md` § Forbidden.
//!
//! `#![no_std]` + `#![no_main]`, **with `alloc`**. The no-`alloc` rule was lifted on
//! 2026-07-31 because session-mgr hands each session its environment, and every step of
//! that needs a heap: a TSM1 `Record` holds `Vec`s, `send_setup` builds a `Vec<String>`,
//! and encoding returns a `Vec<u8>`. Without it the *parent* cannot give the child its
//! environment — which is what Milestone 3.5 is built on. `std` stays unported and there
//! is no runtime to hand a `main`, so those two rules are unchanged.
//! See `userspace/session-mgr/CLAUDE.md`.

#![no_std]
#![no_main]

extern crate alloc;

use libkern::debug::Line;
use libkern::*;

/// `alloc` backing: the environment record and the setup-message codec both allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;
use librsproto::auth::{build_authenticate_request, parse_authenticate_reply};
use librsproto::{OP_AUTHENTICATE, decode, encode};

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut SEND_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut SEND_HANDLES: [u64; 8] = [0; 8];
static mut NOTIF: Notification = Notification::zeroed();

/// Spawn args for the user shell: run in the **constructed session namespace** with
/// **empty syscaps** (a fully unprivileged sandbox). `image`/`namespace` are filled at
/// spawn.
static mut SPAWN_SHELL: SpawnArgs = SpawnArgs {
    image: 0,
    // One handle: the setup channel. The shell is a **Tier-1** stage — it receives its
    // `argv` and its **environment** the same way every pipeline stage does, rather than
    // through a special case (Milestone 3.5 Part D).
    handle_count: 1,
    move_mask: 1,
    arg0: 0,
    handles: [0; 4],
    rights: [u64::MAX; 4],
    namespace: 0, // set at spawn = the session namespace
    syscaps: 0,   // empty — the shell is sandboxed
};

/// The user's home *as seen from inside the session*.
///
/// session-mgr binds the user's home subtree at `/home`, so from within the session that
/// path **is** the home directory — the outside path (`/home/alice`) is not nameable, and
/// that is the point: absence is the sandbox.
const SESSION_HOME: &str = "/home";

/// Build the environment a session starts with.
///
/// A TSM1 `Record`, so it is typed: `PATH` is a `List<String>` rather than a colon-joined
/// string, which makes a path *containing* a colon representable and leaves no room for two
/// readers to split it differently.
///
/// **`USER` is deliberately absent.** Identity lives at `/session/user`, a namespace
/// binding a process cannot forge because it cannot bind at all. Copying it into the
/// environment would hand that property away for nothing: any process could then tell a
/// child it was somebody else.
fn session_env() -> libstream::wire::Record {
    use libstream::wire::{Record, Schema, TypeModifiers, TypeTag, Value};
    let schema = Schema::new()
        .field("HOME", TypeTag::String, TypeModifiers::NONE)
        .field("PWD", TypeTag::String, TypeModifiers::NONE)
        .field("PATH", TypeTag::List, TypeModifiers::NONE);
    let path: alloc::vec::Vec<Value> =
        alloc::vec![Value::Str(alloc::string::String::from("/bin"))];
    Record {
        schema,
        values: alloc::vec![
            Value::Str(alloc::string::String::from(SESSION_HOME)),
            // A session begins at home, which is also what makes a bare `list` mean
            // something useful the moment you log in.
            Value::Str(alloc::string::String::from(SESSION_HOME)),
            Value::List(alloc::sync::Arc::from(path)),
        ],
    }
}

/// Emit `msg` to the serial console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Park forever. Reached only when this supervisor cannot usefully continue — today, when
/// its endpoint handoff did not arrive, so no session can be built.
///
/// **Parks, never spins.** This was a `pause` loop, which burns a CPU for as long as the
/// machine is up. The cost is not the cycles: a run queue that is never empty means the
/// idle thread never runs, and deferred handle reclamation lives there — so a spinning
/// supervisor stops *every* exited process on the system from being reclaimed, and their
/// pipes from ever closing. That is exactly the `logging-service` bug of 2026-07-31,
/// which took an afternoon to find from a hung shell three subsystems away.
fn idle(notif: u64) -> ! {
    loop {
        wait_one(notif);
    }
}

/// Park forever without a handle to wait on — the panic path, which has no notification
/// channel in scope. Sleeps in long hops rather than spinning, for the reason [`idle`]
/// gives: a spinning process starves deferred reclamation system-wide.
fn park() -> ! {
    loop {
        sleep_ms(60_000);
    }
}

/// Sleep `ms` milliseconds on a one-shot timer. Best-effort: on any failure it returns
/// immediately (every caller is pacing, not depending on the delay).
fn sleep_ms(ms: u64) {
    // SAFETY: a valid syscall; returns a handle (>= 0) or a negative KError.
    let th = unsafe { syscall1(SYS_TIMER_CREATE, 0) };
    if th < 0 {
        return;
    }
    let th = th as u64;
    let mut now: u64 = 0;
    // SAFETY: `now` is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    let fire_at = now + ms * 1_000_000;
    // SAFETY: arming our own timer handle (absolute monotonic, one-shot).
    unsafe { syscall4(SYS_TIMER_SET, th, fire_at, 0, 0) };
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    unsafe {
        WAIT_HANDLES[0] = th;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            fire_at + 1_000_000_000,
        );
        syscall1(SYS_HANDLE_CLOSE, th);
    }
}

/// Block on `handle`; returns `true` if it signalled (vs. a spurious wake).
fn wait_one(handle: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = handle;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    waited == 1
}

/// Receive the next control message on `ctrl` and return its transferred `handles[0]`
/// (a handoff carries exactly one moved handle, no payload). `0` on failure.
fn recv_handoff(ctrl: u64) -> u64 {
    if !wait_one(ctrl) {
        return 0;
    }
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ctrl,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    let count = unsafe { (&raw const RECV_COUNT).read() };
    if rr != 0 || count < 1 {
        return 0;
    }
    // SAFETY: the kernel installed the transferred handle at handles[0].
    unsafe { (&raw const RECV_HANDLES[0]).read() }
}

/// Authenticate `(user, pass)` against auth-service over `auth_ch`: build + send an
/// `Authenticate` request, receive the reply, and copy the returned home path into
/// `home_out` (returning its length). Returns `Some(home_len)` if AUTHENTICATED, `None`
/// if DENIED or on any protocol error.
fn authenticate(auth_ch: u64, user: &[u8], pass: &[u8], home_out: &mut [u8]) -> Option<usize> {
    // Build the request body, then wrap it in the rsproto envelope at the payload offset.
    let mut body = [0u8; 512];
    let body_len = build_authenticate_request(&mut body, user, pass)?;
    // SAFETY: SEND_MSG is a valid 4 KiB buffer; the envelope goes at offset 24.
    let rs_len = unsafe {
        encode(&mut SEND_MSG[PAYLOAD_OFF..], OP_AUTHENTICATE, 1, 0, &body[..body_len], 0)?
    };
    // SAFETY: stamp the IpcMsg header (payload_len @4, handle_count @8 = 0) and send.
    let sr = unsafe {
        SEND_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        SEND_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            auth_ch,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        )
    };
    if sr != 0 {
        return None;
    }
    // Await + receive the reply on the same channel.
    if !wait_one(auth_ch) {
        return None;
    }
    // SAFETY: valid recv out-params (the reply carries no transferred handles).
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            auth_ch,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    if rr != 0 {
        return None;
    }
    // SAFETY: read payload_len + form a bounded slice over the reply payload.
    let (result_ok, home_len) = unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let reply = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        match decode(reply) {
            Ok(m) if m.op == OP_AUTHENTICATE => match parse_authenticate_reply(m.body) {
                Some(r) if r.is_authenticated() => {
                    let n = r.home.len().min(home_out.len());
                    home_out[..n].copy_from_slice(&r.home[..n]);
                    (true, n)
                }
                _ => (false, 0),
            },
            _ => (false, 0),
        }
    };
    if result_ok { Some(home_len) } else { None }
}

/// Construct a session namespace for a login whose home is `home` (an absolute path,
/// e.g. `/home/alice`): a fresh namespace binding the user's home subtree of the
/// fs-server at `/home` (RW) and the console at `/dev/console` (so the shell has I/O).
/// Deliberately **omits** everything else (`/dev/blk`, other homes, the raw fs root) —
/// absence is the sandbox. Proves `BIND_NAMESPACE` + subtree scoping + shared-
/// registration bind-mount. Returns the session-namespace handle, or `0` on failure.
/// `root_ns` is session-mgr's inherited namespace (to resolve the console).
fn build_session_namespace(
    root_ns: u64,
    fs_endpoint: u64,
    profile_endpoint: u64,
    tty_endpoint: u64,
    home: &[u8],
    user: &[u8],
) -> u64 {
    // A fresh, owned namespace (full rights — this is *our* namespace to compose).
    let ns = unsafe { syscall0(SYS_NS_CREATE) };
    if ns < 0 {
        kprint(b"session-mgr: ns_create FAIL\n");
        return 0;
    }
    let ns = ns as u64;
    // `/home` → the fs-server endpoint scoped to the user's home subtree. The kernel
    // shares init's fs registration (bind-mount) and prepends `home` to every forwarded
    // suffix. Requires BIND_NAMESPACE (re-delegated) + BIND on `ns`.
    let sub = b"/home";
    let br = unsafe {
        syscall6(
            SYS_NS_BIND,
            ns,
            sub.as_ptr() as u64,
            sub.len() as u64,
            fs_endpoint,
            home.as_ptr() as u64,
            home.len() as u64,
        )
    };
    if br != 0 {
        kprint(b"session-mgr: /home subtree bind FAIL\n");
        // SAFETY: closing the namespace we created.
        unsafe { syscall1(SYS_HANDLE_CLOSE, ns) };
        return 0;
    }
    // `/bin` → the profile server, whole-tree (no subtree base): a lookup of `/bin/list`
    // reaches it with suffix `list`, and it probes the system profile's packages in the
    // store. The same endpoint init bound in the root namespace, so the kernel *shares*
    // that registration rather than minting a rival — one server connection, two names,
    // exactly as `/home` shares the fs-server's.
    //
    // **Why a profile and not `/initramfs/sbin`.** Binding the boot image would be one
    // line and would work today. It also hands every session every binary the system
    // booted with, which is precisely the ambient authority per-process namespaces exist
    // to refuse — "absence is the sandbox" means nothing if absence is never arranged. A
    // profile is a *choice* about what a user may run; the initramfs is an accident of
    // what booting needed.
    //
    // Non-fatal: a session with no `/bin` still has the whole in-process language, and
    // failing the login would trade a working shell for a missing `list`.
    let mut has_bin = false;
    let mut has_tty = false;
    if profile_endpoint != 0 {
        let bin = b"/bin";
        // SAFETY: valid namespace handle, path pointer, and endpoint handle; no subtree
        // base (0/0) — the profile server is bound at its own root.
        let pr = unsafe {
            syscall6(SYS_NS_BIND, ns, bin.as_ptr() as u64, bin.len() as u64, profile_endpoint, 0, 0)
        };
        if pr != 0 {
            kprint(b"session-mgr: /bin bind FAIL (session has no programs)\n");
        }
        has_bin = pr == 0;
    }

    // `/session/user` → who this session belongs to.
    //
    // Nitrox has no kernel user identity — authority is capabilities, so there is nothing
    // for the kernel to report and identity is a session concept. We are the component
    // that authenticated the login, so we are the one that knows, and the way a process is
    // told about its world here is *namespace construction*: the shell does not ask us
    // where home is, it sees `/home`. "Who am I" is the same shape of question.
    //
    // A direct-handle bind of a memory object, i.e. a snapshot — correct because a
    // session's user is immutable for its lifetime (changing user means a new session).
    // The first genuinely *mutable* `/session/*` member is the trigger to put a resource
    // server behind this prefix instead; clients do not change when that happens, because
    // a server answers a resolve with a memory object too. See
    // `TODO(session-metadata-server)`.
    bind_session_user(ns, user);

    // `/dev/tty` → the terminal server, so a program in the session can obtain a *cooked*
    // terminal it can also write to — and, once every client has moved, has no way to
    // reach the raw device at all.
    if tty_endpoint != 0 {
        let dev = b"/dev/tty";
        // The tty server's **forwarding** endpoint — the channel the kernel sends resolves
        // down — so a program in the session resolving `/dev/tty` gets its *own* terminal,
        // freshly minted, exactly as session-mgr did for the login prompt.
        //
        // Binding a minted tty channel here instead would silently produce a namespace
        // entry that answers `Namespace::Resolve` with `Unsupported`: the kernel adopts
        // *any* bound channel as a server, and a client channel does not speak resolve.
        // Both are `IpcChannel`s; only the role differs.
        //
        // Sharing init's registration, exactly as `/home` shares the fs-server's.
        // SAFETY: valid namespace handle, path pointer, and endpoint handle; no base.
        let tr = unsafe {
            syscall6(SYS_NS_BIND, ns, dev.as_ptr() as u64, dev.len() as u64, tty_endpoint, 0, 0)
        };
        if tr != 0 {
            kprint(b"session-mgr: /dev/tty bind FAIL\n");
        }
        has_tty = tr == 0;
    }

    // `/dev/console` → a direct-handle bind of the console device (resolved from our own
    // namespace), so the shell can do console I/O within its sandbox. Non-fatal if
    // absent (the test-harness shell does not read the console).
    let (cst, console) = ns_lookup(root_ns, b"/dev/console", RIGHT_READ | RIGHT_TRANSFER);
    if cst == 0 && console != 0 {
        let dev = b"/dev/console";
        // SAFETY: valid namespace handle, path pointer, and console handle (a device
        // node → a direct-handle bind; no subtree base).
        let cr = unsafe {
            syscall6(SYS_NS_BIND, ns, dev.as_ptr() as u64, dev.len() as u64, console, 0, 0)
        };
        // The bind cloned its own reference; drop ours.
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, console) };
        if cr != 0 {
            kprint(b"session-mgr: /dev/console bind FAIL (shell has no console)\n");
        }
    }
    // SAFETY: single-threaded session-mgr; one namespace is built at a time.
    unsafe {
        SESSION_HAS_BIN = has_bin;
        SESSION_HAS_TTY = has_tty;
    }
    ns
}

/// Whether the last-built session namespace got its `/bin`. Read only for the log line —
/// nothing branches on it.
fn session_has_bin() -> bool {
    // SAFETY: single-threaded session-mgr.
    unsafe { SESSION_HAS_BIN }
}

/// Set by [`build_session_namespace`]; see [`session_has_bin`].
static mut SESSION_HAS_BIN: bool = false;

/// Whether the last-built session namespace got a terminal. Read only for the log line —
/// which has to be able to say "no", the same reason `/bin` is reported separately.
fn session_has_tty() -> bool {
    // SAFETY: single-threaded session-mgr.
    unsafe { SESSION_HAS_TTY }
}

/// Set by [`build_session_namespace`]; see [`session_has_tty`].
static mut SESSION_HAS_TTY: bool = false;

/// Publish `user` at `/session/user` in `ns`, as a read-only memory object.
///
/// Non-fatal: a session whose identity could not be published is still a usable session,
/// and failing the login over it would trade a working shell for a missing `whoami`.
fn bind_session_user(ns: u64, user: &[u8]) {
    // SAFETY: a page-sized anonymous object; the syscall returns a handle or a negative
    // error.
    let mem = unsafe { syscall4(SYS_MEMORY_CREATE, 4096, 0, 0, 0) };
    if mem < 0 {
        kprint(b"session-mgr: /session/user create FAIL\n");
        return;
    }
    let mem = mem as u64;
    // SAFETY: maps the object we just created, read/write, at a kernel-chosen address.
    let base = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, 4096, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if base < 0 {
        kprint(b"session-mgr: /session/user map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return;
    }
    let n = user.len().min(255);
    // SAFETY: `base` is a live writable mapping of one page; `n` is bounded well inside
    // it, and the trailing NUL is the terminator a reader stops at.
    unsafe {
        let dst = core::slice::from_raw_parts_mut(base as *mut u8, 4096);
        dst[..n].copy_from_slice(&user[..n]);
        dst[n] = 0;
        syscall4(SYS_MEMORY_UNMAP, base as u64, 4096, 0, 0);
    }
    // Bind read-only: a session's user is something to be told, not something to edit.
    let path = b"/session/user";
    // SAFETY: valid namespace handle, path pointer, and object handle (a direct-handle
    // bind — no subtree base).
    let br = unsafe {
        syscall6(SYS_NS_BIND, ns, path.as_ptr() as u64, path.len() as u64, mem, 0, 0)
    };
    // The bind cloned its own reference; drop ours either way.
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
    if br != 0 {
        kprint(b"session-mgr: /session/user bind FAIL (whoami will report no identity)\n");
    }
}

/// Resolve `path` in `ns` with `rights`, waiting the PO; returns `(status, handle)`.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return (po as i32, 0);
    }
    if !wait_one(po as u64) {
        // SAFETY: closing our own PO.
        unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
        return (-1, 0);
    }
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    (status, handle)
}

/// Spawn the user shell (`/initramfs/sbin/nxsh`) into `session_ns` (empty syscaps),
/// then block on `notif` for its `ChildExited` and return its exit code. `-1` if the
/// shell could not be spawned. This is the login's payoff: an unprivileged process in a
/// per-user namespace, reaped by session-mgr.
fn spawn_user_shell(root_ns: u64, session_ns: u64, notif: u64) -> i32 {
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup_env};

    let image = ns_lookup(root_ns, b"/bin/nxsh", RIGHT_MAP_READ).1;
    if image == 0 {
        kprint(b"session-mgr: nxsh image not found\n");
        return -1;
    }
    let (setup_mgr, setup_shell) = match pipe(4) {
        Ok(p) => p,
        Err(_) => {
            kprint(b"session-mgr: setup channel FAIL\n");
            return -1;
        }
    };
    // SAFETY: SPAWN_SHELL is a valid writable arg block; run in the session namespace.
    let h = unsafe {
        SPAWN_SHELL.image = image;
        SPAWN_SHELL.namespace = session_ns;
        SPAWN_SHELL.handles[0] = setup_shell;
        SPAWN_SHELL.arg0 = bootstrap_arg0(true);
        syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_SHELL) as u64)
    };
    // The kernel copied the ELF during spawn; close our image handle.
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, image) };
    if h < 0 {
        kprint(b"session-mgr: nxsh spawn FAIL\n");
        return -1;
    }

    // Hand the shell its `argv` and its environment. No streams: an interactive shell
    // reads `/dev/console` from its own namespace, which is a capability it was *given*
    // rather than a stream it was handed.
    //
    // **One `argv`, in every build.** Under `test-harness` this used to be a `-c` script —
    // `$env.PWD == $env.HOME`, a write to home read back, and `list .` finding something —
    // run after an auto-login, so the boot had a deterministic verdict without anyone
    // typing. It proved the right three things in a build where the *typed* login did not
    // exist: `login()` was a different function and the whole `tty_*` layer was compiled
    // out. Those three assertions are now steps 5a–5c of `cargo xtask test-interactive`,
    // which drives the release image (`docs/planning/test-path-retrofit.md` Part B).
    let argv: &[&str] = &["nxsh"];

    let sent = send_setup_env(setup_mgr, &Streams::default(), argv, &session_env());
    // SAFETY: closing our end of the setup channel; the shell holds its own.
    unsafe { syscall1(SYS_HANDLE_CLOSE, setup_mgr) };
    if sent.is_err() {
        kprint(b"session-mgr: setup message FAIL\n");
        return -1;
    }
    kprint(b"session-mgr: nxsh spawned into the session namespace with its environment\n");
    // Reap it: block on the notification channel for its ChildExited, then read the code.
    loop {
        if !wait_one(notif) {
            continue;
        }
        // Drain queued notifications.
        loop {
            // SAFETY: NOTIF is a valid 64-byte writable out-param.
            let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
            if r != 0 {
                break;
            }
            let (kind, body) =
                unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
            if kind == KIND_CHILD_EXITED {
                let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                // SAFETY: closing our reference to the exited shell (reaping).
                unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
                return code;
            }
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel (reaps the user shell), `rsi` =
/// the inherited (LOOKUP-only) root namespace, `rdx` = the control channel service-mgr
/// hands the endpoints over, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"session-mgr: up\n");
    // Receive the handed-over endpoints, in order: (1) fs-server endpoint, (2) profile
    // server endpoint, (3) tty server endpoint, (4) auth channel. Positional — service-mgr sends an empty message
    // for an endpoint it does not have, so a missing one shortens no one's count.
    let fs_endpoint = recv_handoff(control);
    let profile_endpoint = recv_handoff(control);
    let tty_endpoint = recv_handoff(control);
    let auth_ch = recv_handoff(control);
    if fs_endpoint == 0 || auth_ch == 0 {
        // No verdict here any more. A supervisor that cannot build sessions is a boot
        // failure, but adjudicating one is not a session supervisor's job — it says what
        // happened and stops. `test-qemu` requires the success line below in the
        // transcript, so this path fails the run without this process knowing a run exists
        // (`docs/planning/test-path-retrofit.md` Part B).
        kprint(b"session-mgr: endpoint handoff FAIL\n");
        idle(notif);
    }
    // A session without `/bin` is the pre-Part-F shell: the language works, nothing
    // spawns. Degraded, not broken — worth reporting and worth not failing the login over.
    if profile_endpoint == 0 {
        kprint(b"session-mgr: no profile endpoint -- sessions will have no /bin\n");
    }
    kprint(b"session-mgr: received fs + profile endpoints + auth channel\n");

    // The session loop: authenticate a user, construct their per-user namespace, spawn
    // the shell into it, and reap it — the same way in every build.
    let mut home = [0u8; 256];
    let mut user = [0u8; 64];
    // **The session loop.** It was a single `match` — log in once, run a shell, then park
    // forever — so typing `exit` left a machine with no prompt and no way back short of a
    // reboot. A login that cannot be repeated is not a login.
    loop {
    // One terminal per session: it serves the login prompt, is bound into the session's
    // namespace for the shell, and is closed when the session ends.
    let tty = tty_open(root_ns);

    match login(tty, auth_ch, &mut home, &mut user) {
        Some((hl, ul)) => {
            Line::new().s(b"session-mgr: login ok -> home=").s(&home[..hl]).end();
            let session_ns = build_session_namespace(
                root_ns,
                fs_endpoint,
                profile_endpoint,
                tty_endpoint,
                &home[..hl],
                &user[..ul],
            );
            if session_ns == 0 {
                kprint(b"session-mgr: session namespace FAIL\n");
                // One bad session: go back to the prompt rather than bricking the console,
                // since a permanent park is a worse answer than letting someone try again.
                continue;
            }
            kprint(b"session-mgr: session namespace built (/home subtree + /dev/console");
            // Named separately because it is the one member that can be absent, and a
            // run with the bind disabled proved the unconditional message would announce
            // a `/bin` that was not there — the log has to be able to say "no".
            if session_has_bin() {
                kprint(b" + /bin");
            }
            if session_has_tty() {
                kprint(b" + /dev/tty");
            }
            kprint(b")\n");
            // The payoff: an unprivileged shell in the per-user namespace writes to home.
            let code = spawn_user_shell(root_ns, session_ns, notif);

            // Tear the session down. The shell has been reaped, so this drops the last
            // reference to the namespace and with it every binding in it — the `/home`
            // and `/bin` registrations and the `/session/user` snapshot. Without it each
            // logout would leak a namespace, which is only harmless while there is
            // exactly one login per boot.
            // SAFETY: closing the namespace we created for this session.
            unsafe { syscall1(SYS_HANDLE_CLOSE, session_ns) };
            // And the terminal. `Close` is the revocation — a process that outlived the
            // session cannot have its handle taken back, so the server declining to serve
            // is what ends the terminal.
            if tty != 0 {
                tty_close(tty);
            }

            // The shell exited because the user asked it to. Say what happened and nothing
            // more — this used to sit beside a harness branch claiming "verified its
            // environment + wrote to home", a check an interactive session never ran.
            {
                // **One `kprint`, not four.** The console is shared, so a line assembled
                // from several calls can be torn by any other process that logs between
                // them — this one came back as `session ended (shell exit tty-server:
                // terminal closed\n3)`, because the tty server logs the close while the
                // status is being written. A log line that another writer can split down
                // the middle is not a log line; assemble it, then emit it once.
                let mut line = alloc::string::String::from(
                    "session-mgr: session ended (shell exit ",
                );
                let mut buf = [0u8; 20];
                if code < 0 {
                    line.push('-');
                }
                let digits = libkern::debug::fmt_u64(code.unsigned_abs() as u64, &mut buf);
                line.push_str(core::str::from_utf8(digits).unwrap_or("?"));
                line.push_str(")\n");
                kprint(line.as_bytes());
            }
        }
        None => {
            kprint(b"session-mgr: login denied\n");
            if tty != 0 {
                tty_close(tty);
            }
            // Re-prompt rather than locking out. A serial console has no second way in,
            // so a lockout bricks the machine; the pause is what keeps repeated failure
            // from being a free brute-force oracle.
            sleep_ms(2000);
        }
    }
    }
}

/// Authenticate a user, returning `(home_len, user_len)` with the home path copied into
/// `home_out` and the authenticated name into `user_out`, or `None` if denied.
///
/// The name comes back because the session namespace publishes it at `/session/user`:
/// we are the component that authenticated, so we are the one that knows who this is.
///
/// Prompts for a username and a password on the terminal, up to a few attempts.
///
/// **There used to be two of these**, and the other one is why
/// `docs/planning/test-path-retrofit.md` exists: under `test-harness` this function was
/// replaced by an auto-login of a hardcoded demo credential, so the gate that adjudicated
/// the whole boot ran a build in which the prompt below, the echo discipline, and the whole
/// `tty_*` layer were compiled out. What it proved about logging in was that a string
/// comparison worked. The real path is now driven from the host by
/// `cargo xtask test-interactive`, against the release image.
fn login(tty: u64, auth_ch: u64, home_out: &mut [u8], user_out: &mut [u8]) -> Option<(usize, usize)> {
    if tty == 0 {
        kprint(b"session-mgr: no terminal for login\n");
        return None;
    }
    for _ in 0..3 {
        tty_write(tty, b"\r\nnitrox login: ");
        let mut user = [0u8; 64];
        tty_set_echo(tty, true);
        let ulen = tty_read_line(tty, &mut user);

        tty_write(tty, b"password: ");
        let mut pass = [0u8; 128];
        // The server stops echoing; this cannot be forgotten the way a `bool` argument
        // could, and the discipline also stops *erasing* on screen, so a backspace in a
        // password reveals nothing about its length.
        tty_set_echo(tty, false);
        let plen = tty_read_line(tty, &mut pass);
        tty_set_echo(tty, true);
        // Nothing was echoed for the password, so the newline the user typed was not shown
        // either: supply it here so what follows starts on its own line.
        tty_write(tty, b"\r\n");

        if let Some(hl) = authenticate(auth_ch, &user[..ulen], &pass[..plen], home_out) {
            let ul = ulen.min(user_out.len());
            user_out[..ul].copy_from_slice(&user[..ul]);
            return Some((hl, ul));
        }
        tty_write(tty, b"login incorrect\r\n");
    }
    None
}

/// Open a terminal: resolve `/dev/tty`, which yields a fresh per-caller channel.
///
/// One tty per session. It serves the login prompt first, is then bound into the session's
/// namespace for the shell, and is closed when the session ends — the terminal belongs to
/// the session, not to session-mgr.
fn tty_open(root_ns: u64) -> u64 {
    let (st, ch) = ns_lookup(root_ns, b"/dev/tty", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_TRANSFER);
    if st != 0 { 0 } else { ch }
}

/// Send one tty request and wait for its reply. Returns the reply body length written into
/// `out` (`0` for the acknowledgement-only ops), or `None` on any failure.
fn tty_request(ch: u64, op: u16, body: &[u8], out: &mut [u8]) -> Option<usize> {
    // SAFETY: SEND_MSG is a valid buffer; the rsproto message goes at offset 24.
    let sent = unsafe {
        let rs_len = encode(&mut SEND_MSG[PAYLOAD_OFF..], op, 1, 0, body, 0)?;
        SEND_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        SEND_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    };
    if !sent || !wait_one(ch) {
        return None;
    }
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
    if rr != 0 {
        return None;
    }
    // SAFETY: bounded read-only slice over the reply.
    unsafe {
        let payload_len =
            u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        let rep = core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            payload_len.min(MSG_LEN - PAYLOAD_OFF),
        );
        let m = decode(rep).ok()?;
        if m.op != op || m.is_error() {
            return None;
        }
        let n = m.body.len().min(out.len());
        out[..n].copy_from_slice(&m.body[..n]);
        Some(n)
    }
}

/// Write `text` to the terminal. Output through a handle the process *holds* — not the
/// ambient debug syscall every program used to print with.
fn tty_write(ch: u64, text: &[u8]) {
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_WRITE, text, &mut scratch);
}

/// Turn echo on or off.
///
/// **This is why the tty server exists.** It was a `bool` every caller of `read_line` had
/// to remember to pass, so reading a password safely depended on each of them getting it
/// right. Now it is the server's state and a client cannot forget it.
fn tty_set_echo(ch: u64, on: bool) {
    let flags = [if on { librsproto::TTY_MODE_ECHO } else { 0 }];
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_SET_MODE, &flags, &mut scratch);
}

/// Read one edited line. The line discipline — backspace, kill, echo — is the server's,
/// so this is a request rather than a byte loop.
fn tty_read_line(ch: u64, out: &mut [u8]) -> usize {
    tty_request(ch, librsproto::OP_TTY_READ_LINE, &[], out).unwrap_or(0)
}

/// Tell the server this terminal is finished, then drop the handle.
///
/// **Revocation, not release.** Handles are refcounted and this kernel has none, so closing
/// cannot take a tty back from a process that outlived the session. The server declining to
/// serve the channel is what makes teardown a guarantee.
fn tty_close(ch: u64) {
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_CLOSE, &[], &mut scratch);
    // SAFETY: closing our own channel handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ch) };
}


#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"session-mgr: PANIC\n");
    park();
}
