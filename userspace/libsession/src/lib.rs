//! The shared session core: **authenticate → construct the namespace → spawn the leader →
//! reap → tear down**.
//!
//! The same sequence runs in both columns — `session-mgr` on a terminal and, from M7 Part D,
//! `desktop-session-mgr` in a window — against different arguments. Following Linux's PAM
//! precedent this is a **shared library, not a merged process**
//! (`docs/architecture/graphical-session.md` §4): the two supervisors stay separate principals with
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

use libkern::debug::Line;
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
    /// Bind `/system/fonts` into the session, read-only.
    ///
    /// **A graphical session needs a font and a constructed namespace has none.** Every GUI
    /// client before M7 Part E ran with an inherited *root* namespace and read its font from
    /// there; `desktop-shell` is the first to run in a namespace someone built, and it could not
    /// find one — the symptom was a leader that logged "up" and exited.
    ///
    /// The directory rather than a file, which is what let M11 Part D put a second face in it
    /// without touching a session's authority: what a client loads is named by its theme, and
    /// both faces are under this one bind.
    ///
    /// **A flag rather than always**, symmetric with [`bind_console`](Self::bind_console) and
    /// for the same reason: a serial session has no use for a font, and a member bound for
    /// nothing is still authority held. `session-mgr/CLAUDE.md` states the rule — adding a
    /// member to a session namespace is a design decision each time.
    ///
    /// The alternative was vendoring the font into each binary, which is what `nxterm`'s
    /// *host tests* do. At ~347 KiB per application that scales badly, and it would put a
    /// resource in the binary that the system already serves as a resource.
    pub bind_fonts: bool,
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
        bind_fonts,
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
    let mut has_console = false;
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

    // `/system/fonts` → the fs-server endpoint scoped to that subtree, the same shape `/home`
    // uses. Read-only by construction: a subtree bind forwards to the same registration, and
    // nothing in the session has a writable handle to it.
    if bind_fonts && fs_endpoint != 0 {
        let sub = b"/system/fonts";
        let base = b"/system/fonts";
        // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
        let fr = unsafe {
            syscall6(
                SYS_NS_BIND,
                ns,
                sub.as_ptr() as u64,
                sub.len() as u64,
                fs_endpoint,
                base.as_ptr() as u64,
                base.len() as u64,
            )
        };
        if fr != 0 {
            // Non-fatal: the session exists, its clients just cannot render text. Worth
            // reporting rather than failing a login over.
            kprint(b"libsession: /system/fonts bind FAIL (clients cannot render text)\n");
        }
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
        } else {
            has_console = true;
        }
    }
    // SAFETY: single-threaded session-mgr; one namespace is built at a time.
    unsafe {
        SESSION_HAS_BIN = has_bin;
        SESSION_HAS_TTY = has_tty;
        SESSION_HAS_CONSOLE = has_console;
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

/// Whether the last [`build_namespace`] bound `/dev/console`.
///
/// **Reported rather than assumed**, and that distinction was not free: the graphical
/// supervisor printed a hardcoded "(no /dev/console)" until 2026-08-25, so its gate's
/// assertion passed just as happily with `bind_console: true`. A message that states a fact
/// it did not check is decoration, and the control is what found it.
pub fn session_has_console() -> bool {
    // SAFETY: single-threaded supervisor; one namespace is built at a time.
    unsafe { SESSION_HAS_CONSOLE }
}

/// See [`session_has_console`].
static mut SESSION_HAS_CONSOLE: bool = false;

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

/// The notification slot this crate reaps into.
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
    syscaps: 0,   // set at spawn — see `spawn_leader`
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
pub fn session_env() -> libstream::wire::Record {
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

/// Spawn the user shell (`/initramfs/sbin/nxsh`) into `session_ns` (empty syscaps),
/// then block on `notif` for its `ChildExited` and return its exit code. `-1` if the
/// shell could not be spawned. This is the login's payoff: an unprivileged process in a
/// per-user namespace, reaped by session-mgr.
/// **`syscaps` is the caller's to choose, and the two columns choose differently.** The serial
/// leader gets `0` — `nxsh` is a sandboxed user shell and holds nothing. The graphical leader
/// gets `SYSCAP_BIND_NAMESPACE`, because `desktop-shell` constructs a namespace per application
/// it launches: `ui-composition-model.md` §5a rests the guarantee that "an application cannot
/// compose other applications" on the shell being the process that built them.
///
/// That is what reconciles a process which both *serves* and *constructs* with
/// [`syscaps.md`](../../../docs/architecture/syscaps.md) — the shell holds the capability to
/// build application namespaces continuously, not to register itself once, and it does not
/// bind its own endpoint at all (`graphical-session.md` §3).
pub fn spawn_leader(
    root_ns: u64,
    session_ns: u64,
    notif: u64,
    program: &str,
    argv_rest: &[&str],
    syscaps: u64,
    extras: &[u64],
) -> i32 {
    use libstream::setup::{Streams, bootstrap_arg0, pipe, send_setup_env};

    // `/bin/<program>`, resolved from the *supervisor's* namespace. The session's own `/bin`
    // is the profile projection and is what the leader will resolve its children through;
    // the leader's own image has to come from here, because the session namespace exists to
    // be handed to it and not to be searched by its parent.
    let mut path = alloc::string::String::from("/bin/");
    path.push_str(program);
    let image = ns_lookup(root_ns, path.as_bytes(), RIGHT_MAP_READ).1;
    if image == 0 {
        Line::new().s(b"libsession: ").s(program.as_bytes()).s(b" image not found").end();
        return -1;
    }
    // **Sized from what is actually sent**, not a constant that used to be big enough. The
    // channel carries the Tier-1 setup message plus one message per extra endpoint, and the
    // sends are `NOBLOCK` — so a queue one short does not block, it *drops the last handle
    // silently*. That is what happened the first time this carried four endpoints instead of
    // one: `pipe(4)`, five messages, and the leader launched applications into namespaces with
    // no `/bin`. The failure was a terminal that opened with nothing in it, two processes away
    // (M7 Part F).
    let depth = (1 + extras.len()) as u32;
    let (setup_mgr, setup_shell) = match pipe(depth) {
        Ok(p) => p,
        Err(_) => {
            kprint(b"libsession: setup channel FAIL\n");
            return -1;
        }
    };
    // SAFETY: SPAWN_SHELL is a valid writable arg block; run in the session namespace.
    let h = unsafe {
        SPAWN_SHELL.image = image;
        SPAWN_SHELL.namespace = session_ns;
        SPAWN_SHELL.syscaps = syscaps;
        SPAWN_SHELL.handles[0] = setup_shell;
        SPAWN_SHELL.arg0 = bootstrap_arg0(true);
        syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_SHELL) as u64)
    };
    // The kernel copied the ELF during spawn; close our image handle.
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, image) };
    if h < 0 {
        Line::new().s(b"libsession: ").s(program.as_bytes()).s(b" spawn FAIL").end();
        // The same leak `launch` had, in the function `launch` was modelled on — a failed spawn
        // never took `handles[0]`, so both ends are still ours. `session-mgr` retries a login
        // after this returns, so the leak repeats per attempt (PR #238 review, finding 7).
        // SAFETY: closing a setup channel pair for a process that does not exist.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, setup_mgr);
            syscall1(SYS_HANDLE_CLOSE, setup_shell);
        }
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
    // **`argv_rest` exists so a leader can be told something its namespace cannot show it.**
    // `desktop-shell` needs the user's real home (`/home/alice`) to scope the `/home` it binds
    // into each application namespace, and it cannot read that anywhere: its own `HOME` is
    // `/home`, which is true *inside the session* precisely because `build_namespace` already
    // scoped it, and a binding never resolves back to the base it was made with. Passing it as
    // an argument is how a parent states a fact rather than delegating authority — the fs
    // endpoint is what carries the authority, and the shell already has that.
    let mut argv_buf: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(1 + argv_rest.len());
    argv_buf.push(program);
    argv_buf.extend_from_slice(argv_rest);
    let argv: &[&str] = &argv_buf;

    let sent = send_setup_env(setup_mgr, &Streams::default(), argv, &session_env());
    // **A second handle, after the setup message and only if there is one.**
    //
    // Only `handles[0]` of a `SpawnArgs` reaches a child, so anything past the setup channel
    // has to arrive *over* it — `service-mgr` makes the same observation where it couriers
    // endpoints. The graphical leader needs the compositor's forwarding endpoint: it builds a
    // namespace per application and binds `/dev/draw/new` into each, and a *binding* is not
    // re-bindable — it resolves to a kernel registration and never back to the endpoint.
    //
    // Ordered after the setup message, so a leader reads `argv` and its environment first and
    // this second message is simply the next one on the same channel.
    // **One message per extra, in the order given.** Only `handles[0]` of a `SpawnArgs` reaches
    // a child, so everything past the setup channel arrives *over* it — and a leader that binds
    // resources into namespaces it constructs needs the endpoints themselves, because a
    // *binding* resolves to a kernel registration and never back to one.
    for &extra in extras {
        if sent.is_err() {
            continue;
        }
        // **A missing endpoint sends an empty message, not nothing.** The leader reads these
        // positionally, so skipping one shifts every later endpoint up a slot — `desktop-shell`
        // would take the profile server's endpoint for the tty server's, bind `/dev/tty` to it
        // in every application namespace, and bind no `/bin` at all. `nxterm` would then resolve
        // `/dev/tty`, get a profile-server channel and fail at `AttachBackend`, while the shell's
        // own "no fonts or no terminal" warning stayed quiet because it saw a non-zero handle.
        //
        // Reachable: `init`'s `bind_tty_server` failure is non-fatal and merely prints "sessions
        // will have no /dev/tty", so a zero propagates all the way here. This is the rule
        // `init::send_handle` and `service-mgr::send_handle` already state and implement; this
        // loop was the level that broke it (PR #238 review, finding 2).
        //
        // A failed duplicate and a failed send take the same path, for the same reason: the
        // leader must see a gap where an endpoint was, not a shorter list.
        // **Duplicated, because a handle on an IPC message is always a *move*.** `sys_channel_send`
        // closes the sender's handle on success, so sending `extra_handle` directly would leave
        // the caller's copy dead — and the caller reuses it for every later session. The symptom
        // was every graphical login after the first getting no `/dev/draw` at all (PR #237
        // review, finding 2).
        // SAFETY: duplicating a handle the caller owns, with the rights a re-bind needs.
        let moved = if extra == 0 {
            0
        } else {
            // SAFETY: duplicating a handle the caller owns, with the rights a re-bind needs.
            unsafe { syscall2(SYS_HANDLE_DUPLICATE, extra, RIGHT_TRANSFER | RIGHT_DUPLICATE) }
        };
        if moved < 0 {
            kprint(b"libsession: could not duplicate the leader's extra handle\n");
        }
        // Zero handles when there is nothing to send — the message still goes, holding the slot.
        let count: u64 = if moved > 0 { 1 } else { 0 };
        {
            // SAFETY: SEND_MSG/SEND_HANDLES are valid buffers; one moved handle, no payload.
            let r = unsafe {
                // **Scrub the payload first.** This buffer is the crate's, and `authenticate`
                // filled it with an `Authenticate` request — the username and password in the
                // clear. Restamping only the header would send all `MSG_LEN` bytes anyway, so
                // the leader would receive the login password and hold it in `.bss` for the life
                // of the process. `desktop-session-mgr` volatile-zeroes its own stack copy two
                // lines from here; handing the same bytes to a child instead would make that
                // pointless (PR #237 review, finding 1).
                SEND_MSG[PAYLOAD_OFF..].fill(0);
                SEND_MSG[4..8].copy_from_slice(&0u32.to_le_bytes());
                SEND_MSG[8] = count as u8;
                SEND_HANDLES[0] = if count == 1 { moved as u64 } else { 0 };
                syscall5(
                    SYS_CHANNEL_SEND,
                    setup_mgr,
                    (&raw const SEND_MSG) as u64,
                    (&raw const SEND_HANDLES) as u64,
                    count,
                    SENDMODE_NOBLOCK,
                )
            };
            // Checked and reported, like `send_setup_env`'s result three lines up. Silence here
            // is what made finding 2 invisible: the supervisor announced a session starting
            // normally while the leader had been given nothing.
            if r != 0 {
                kprint(b"libsession: the leader's extra handle was not delivered\n");
                if count == 1 {
                    // SAFETY: the send failed, so the duplicate is still ours.
                    unsafe { syscall1(SYS_HANDLE_CLOSE, moved as u64) };
                }
            } else if count == 0 {
                // Said out loud, because the leader will read a live message holding nothing and
                // has no other way to tell that apart from an endpoint that was never wanted.
                kprint(b"libsession: an endpoint the leader expects is absent (placeholder sent)\n");
            }
        }
    }
    // SAFETY: closing our end of the setup channel; the shell holds its own.
    unsafe { syscall1(SYS_HANDLE_CLOSE, setup_mgr) };
    if sent.is_err() {
        kprint(b"libsession: setup message FAIL\n");
        return -1;
    }
    Line::new()
        .s(b"libsession: ")
        .s(program.as_bytes())
        .s(b" spawned into the session namespace with its environment")
        .end();
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
