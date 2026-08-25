//! The shared session core: **authenticate → construct the namespace → spawn the leader →
//! reap → tear down**.
//!
//! The same sequence runs in both columns — `session-mgr` on a terminal and, from M7 Part D,
//! `desktop-session-mgr` in a window — against different arguments. Following Linux's PAM
//! precedent this is a **shared library, not a merged process**
//! (`docs/design/graphical-session.md` §4): the two supervisors stay separate principals with
//! separate lifetimes, and only the logic they genuinely share lives here.
//!
//! **The greeter is not here.** Prompting on a terminal and drawing a login window have
//! nothing in common but their result, so each supervisor keeps its own. What this crate
//! starts with is credentials in hand, and what it ends with is a leader that has exited.
//!
//! ## Why this crate exists before its second caller
//!
//! `session-mgr` moves onto it first and `cargo xtask test-interactive` stays green, which is
//! the point: the serial column proves the core before the graphical one depends on it. The
//! alternative is a crate whose only caller is also new, where a bug in either is
//! indistinguishable from a bug in the other.
//!
//! It is also why `docs/planning/test-path-retrofit.md` was a prerequisite for this milestone
//! rather than a follow-up: factoring a `login()` that had two compilations would have carried
//! the fork into the shared crate.
//!
//! ## Constraints
//!
//! `#![no_std]` with `alloc`, and **no `libos`** — `session-mgr` links this and that is its
//! rule (`userspace/session-mgr/CLAUDE.md`). `alloc` is needed for the same reason it was
//! allowed there on 2026-07-31: a session's environment is a TSM1 `Record` of `Vec`s.

#![no_std]
#![deny(missing_docs)]

extern crate alloc;

use libkern::*;
use librsproto::auth::{build_authenticate_request, parse_authenticate_reply};
use librsproto::{OP_AUTHENTICATE, decode, encode};

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
/// The largest IPC message this crate sends or receives.
const MSG_LEN: usize = 4096;

/// **This crate's own message buffers**, not shared with its caller's.
///
/// A supervisor keeps `.bss` buffers rather than allocating per message, and reaching across
/// the crate boundary for the caller's would make two unrelated conversations — a session's
/// authentication and a supervisor's terminal I/O — share one buffer. Two 4 KiB buffers is
/// the price of them not being able to tread on each other.
static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
/// See [`RECV_MSG`].
static mut SEND_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
/// See [`RECV_MSG`].
static mut RECV_HANDLES: [u64; 8] = [0; 8];
/// See [`RECV_MSG`].
static mut SEND_HANDLES: [u64; 8] = [0; 8];
/// See [`RECV_MSG`].
static mut RECV_COUNT: usize = 0;
/// See [`RECV_MSG`].
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

/// See [`RECV_MSG`].
static mut WAIT_HANDLES: [u64; 1] = [0; 1];

/// Block until `handle` is signalled. `false` if the wait did not report it.
///
/// A copy of `session-mgr`'s rather than a shared one, for the same reason the message
/// buffers are: it owns a `.bss` handle array, and two callers sharing one would be two
/// conversations sharing a slot.
fn wait_one(handle: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = handle;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    waited == 1
}

/// Write one line to the debug console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// What a session's namespace is built from.
///
/// A struct rather than seven positional arguments because the two columns differ in exactly
/// one of them and a bool in the seventh position is unreadable at the call site.
pub struct NamespaceSpec<'a> {
    /// The supervisor's own namespace, resolved from for direct-handle binds.
    pub root_ns: u64,
    /// The fs-server endpoint, bound at `/home` scoped to the user's subtree.
    pub fs_endpoint: u64,
    /// The profile-server endpoint, bound whole-tree at `/bin`.
    pub profile_endpoint: u64,
    /// The tty-server endpoint, bound at `/dev/tty`.
    pub tty_endpoint: u64,
    /// The user's home directory, as the subtree base for `/home`.
    pub home: &'a [u8],
    /// The user's name, snapshotted at `/session/user`.
    pub user: &'a [u8],
    /// Bind `/dev/console` into the session.
    ///
    /// **False for a graphical session**, and that is a decision rather than an omission:
    /// `graphical-session.md`'s governing decision 3 records what binding a shared console
    /// into a windowed session costs. The serial column binds it because the console *is* its
    /// terminal.
    pub bind_console: bool,
}

/// Authenticate `(user, pass)` against auth-service over `auth_ch`: build + send an
/// `Authenticate` request, receive the reply, and copy the returned home path into
/// `home_out` (returning its length). Returns `Some(home_len)` if AUTHENTICATED, `None`
/// if DENIED or on any protocol error.
pub fn authenticate(auth_ch: u64, user: &[u8], pass: &[u8], home_out: &mut [u8]) -> Option<usize> {
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
pub fn build_namespace(spec: &NamespaceSpec<'_>) -> u64 {
    let NamespaceSpec {
        root_ns,
        fs_endpoint,
        profile_endpoint,
        tty_endpoint,
        home,
        user,
        bind_console,
    } = *spec;
    // A fresh, owned namespace (full rights — this is *our* namespace to compose).
    let ns = unsafe { syscall0(SYS_NS_CREATE) };
    if ns < 0 {
        kprint(b"libsession: ns_create FAIL\n");
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
        kprint(b"libsession: /home subtree bind FAIL\n");
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
            kprint(b"libsession: /bin bind FAIL (session has no programs)\n");
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
            kprint(b"libsession: /dev/tty bind FAIL\n");
        }
        has_tty = tr == 0;
    }

    // `/dev/console` → a direct-handle bind of the console device (resolved from our own
    // namespace), so the shell can do console I/O within its sandbox. Non-fatal if absent.
    //
    // **Skipped entirely for a graphical session** — see [`NamespaceSpec::bind_console`]. Not
    // "bound and then unused": a binding a session holds is authority it has, and the console
    // is shared with every other session on the machine.
    let (cst, console) = if bind_console {
        ns_lookup(root_ns, b"/dev/console", RIGHT_READ | RIGHT_TRANSFER)
    } else {
        (0, 0)
    };
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
            kprint(b"libsession: /dev/console bind FAIL (shell has no console)\n");
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
pub fn session_has_bin() -> bool {
    // SAFETY: single-threaded session-mgr.
    unsafe { SESSION_HAS_BIN }
}

/// Set by [`build_session_namespace`]; see [`session_has_bin`].
static mut SESSION_HAS_BIN: bool = false;

/// Whether the last-built session namespace got a terminal. Read only for the log line —
/// which has to be able to say "no", the same reason `/bin` is reported separately.
pub fn session_has_tty() -> bool {
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
        kprint(b"libsession: /session/user create FAIL\n");
        return;
    }
    let mem = mem as u64;
    // SAFETY: maps the object we just created, read/write, at a kernel-chosen address.
    let base = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, 4096, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if base < 0 {
        kprint(b"libsession: /session/user map FAIL\n");
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
        kprint(b"libsession: /session/user bind FAIL (whoami will report no identity)\n");
    }
}

/// Resolve `path` in `ns` with `rights`, waiting the PO; returns `(status, handle)`.
pub fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
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
