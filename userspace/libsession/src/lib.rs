//! The shared session core: **authenticate → construct the namespace → spawn the leader →
//! reap → tear down**.
//!
//! `session-mgr` and `desktop-session-mgr` run the same sequence against different arguments: a
//! **shared library, not a merged process** (`docs/architecture/graphical-session.md` §4).
//! **The greeter is not here**; it starts with credentials in hand and ends with a leader that
//! has exited. `no_std` with `alloc`, and **no `libos`** (`userspace/session-mgr/CLAUDE.md`).

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};
use libkern::channel_recv_raw;
use libkern::debug::Line;
use libkern::*;
use librsproto::auth::{build_authenticate_request, parse_authenticate_reply};
use librsproto::{OP_AUTHENTICATE, decode, encode};

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
/// The largest IPC message this crate sends or receives.
const MSG_LEN: usize = 4096;

/// **This crate's own message buffers**, not the caller's, so the two conversations cannot collide.
static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut SEND_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut SEND_HANDLES: [u64; 8] = [0; 8];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

static mut WAIT_HANDLES: [u64; 1] = [0; 1];

fn wait_one(handle: u64) -> bool {
    wait_until(handle, u64::MAX)
}

/// [`wait_one`], giving up at `deadline` (absolute monotonic ns). `false` if it passed first.
fn wait_until(handle: u64, deadline: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = handle;
        let (handles, results) = ((&raw const WAIT_HANDLES) as u64, (&raw mut WAIT_RESULTS) as u64);
        syscall4(SYS_WAIT, handles, 1, results, deadline)
    };
    waited == 1
}

fn now_ns() -> u64 {
    let mut now: u64 = 0;
    // SAFETY: `&now` is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    now
}

/// Whether a terminate request has reached this process; set by [`spawn_leader`] and the drain.
static STOP_ASKED: AtomicBool = AtomicBool::new(false);

/// How long a leader has, once asked to stop, before its supervisor goes on without it; under
/// `service-mgr`'s 10 s bound. **Public, because a leader's own wait must fit inside it.**
pub const LEADER_STOP_NS: u64 = 5_000_000_000;

/// **Whether this process has been asked to stop**: a supervisor exits rather than prompting again.
pub fn stop_asked() -> bool {
    STOP_ASKED.load(Ordering::Relaxed)
}

/// Drain `notif`, saying whether a terminate request was among it; for a supervisor between
/// sessions, with no child whose `ChildExited` could be dropped.
pub fn drain_for_stop(notif: u64) -> bool {
    loop {
        // SAFETY: NOTIF is a valid 64-byte writable out-param.
        let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
        if r != 0 {
            return stop_asked();
        }
        // SAFETY: the kernel wrote a Notification into NOTIF.
        if unsafe { (&raw const NOTIF.kind).read() } == KIND_TERMINATE_REQUESTED {
            STOP_ASKED.store(true, Ordering::Relaxed);
        }
    }
}

fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// What a session's namespace — **or an application's** — is built from: both login supervisors
/// and `desktop-shell` (per application) build with this, so they cannot drift.
pub struct NamespaceSpec<'a> {
    /// The namespace direct handles are resolved from: the supervisor's own — the root namespace —
    /// for a session, and **the session's** for an application.
    pub root_ns: u64,
    /// The fs-server endpoint, bound at `/home` scoped to the user's subtree.
    pub fs_endpoint: u64,
    /// The profile-server endpoint, bound whole-tree at `/bin`.
    pub profile_endpoint: u64,
    /// The tty server's, bound at `/dev/tty`. **This and every server endpoint below is
    /// `service-mgr`'s route to the server**, not its own, so a session survives a restart. Each
    /// route reaches one server and nothing else, which is what lets `desktop-shell` hold them.
    pub tty_endpoint: u64,
    /// The clipboard server's, bound at `/dev/clipboard` in **both columns**, as a pipeline uses it
    /// as a path. `0` binds nothing.
    pub clipboard_endpoint: u64,
    /// The user's home directory, as the subtree base for `/home`.
    pub home: &'a [u8],
    /// The user's name, snapshotted at `/session/user`.
    pub user: &'a [u8],
    /// Bind `/system/fonts` read-only; **a flag** because a member bound for nothing is still
    /// authority held (`session-mgr/CLAUDE.md`).
    pub bind_fonts: bool,
    /// Bind `/dev/console`. **False for a graphical session** (`graphical-session.md`, governing
    /// decision 3); the serial column binds it as its terminal.
    pub bind_console: bool,
    /// The view broker's, bound at `/dev/views` with [`views_base`](Self::views_base) as its base.
    /// `0` binds nothing. **The base is the session's identity**: the broker learns whose request
    /// it is from the suffix, and no program here can choose another — except `desktop-shell`,
    /// handed this endpoint to bind into applications.
    pub views_endpoint: u64,
    /// `/s/<session>`, from [`views_open_session`]. Empty binds nothing.
    pub views_base: &'a [u8],
    /// An **info-only** endpoint of the device manager's, bound at `/dev/devices` with the base
    /// `/info`. `0` binds nothing. **The endpoint keeps a session from taking a device**: the
    /// manager answers only its tables on it, whatever suffix arrives.
    pub devices_endpoint: u64,
    /// A **session endpoint** of the storage service's, bound at `/storage` (base `/fs`) and
    /// `/dev/storage` (base `/info`). `0` binds nothing. **The endpoint keeps a session from
    /// mounting**: `admin-endpoint` is `NotFound` there, so even `desktop-shell` cannot mint one.
    pub storage_endpoint: u64,
    /// `service-mgr`'s **session endpoint**, bound at `/dev/services`; it answers only the table.
    /// `0` binds nothing.
    pub services_endpoint: u64,
    /// Bind `/applications`: **true for a session**, **false for an application**.
    pub bind_applications: bool,
    /// The compositor's forwarding endpoint, bound at **`/dev/draw/new` alone**, with the base
    /// `/new`: an application may open windows, and `manage` is unreachable. `0` binds nothing
    /// (the graphical supervisor binds `/dev/draw` whole for `desktop-shell`). **Required when
    /// asked for**: the build fails if it does not bind.
    pub draw_endpoint: u64,
    /// `desktop-shell`'s own endpoint, bound at `/dev/desktop`. `0` binds nothing.
    pub desktop_endpoint: u64,
}

/// **What a namespace built from a [`NamespaceSpec`] holds**: it, and which optional members bound.
#[derive(Clone, Copy, Debug, Default)]
pub struct Built {
    /// The namespace handle.
    pub ns: u64,
    /// `/bin` bound.
    pub bin: bool,
    /// `/dev/tty` bound.
    pub tty: bool,
    /// `/dev/console` bound.
    pub console: bool,
    /// `/dev/clipboard` bound.
    pub clipboard: bool,
    /// `/dev/views` bound, at the session's base.
    pub views: bool,
    /// `/dev/devices` bound.
    pub devices: bool,
    /// `/storage` and `/dev/storage` both bound.
    pub storage: bool,
    /// `/dev/desktop` bound.
    pub desktop: bool,
}

/// Authenticate `(user, pass)` against auth-service over `auth_ch`, copying the home path into
/// `home_out`. `Some(home_len)` if AUTHENTICATED, `None` if DENIED or on any protocol error.
pub fn authenticate(auth_ch: u64, user: &[u8], pass: &[u8], home_out: &mut [u8]) -> Option<usize> {
    let mut body = [0u8; 512];
    let body_len = build_authenticate_request(&mut body, user, pass);
    // SAFETY: SEND_MSG is a valid 4 KiB buffer; the envelope goes at offset 24.
    let rs_len =
        body_len.and_then(|n| unsafe { encode(&mut SEND_MSG[PAYLOAD_OFF..], OP_AUTHENTICATE, 1, 0, &body[..n], 0) });
    // **Both copies of the password go as soon as it is sent**, sent or not: `SEND_MSG` outlives
    // this frame, and a supervisor lives as long as the machine.
    let Some(rs_len) = rs_len else {
        scrub(&mut body);
        return None;
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
    scrub(&mut body);
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: the crate's buffer; the kernel copied the message during the send.
    unsafe {
        scrub(&mut *(&raw mut SEND_MSG))
    };
    if sr != 0 {
        return None;
    }
    if !wait_one(auth_ch) {
        return None;
    }
    // SAFETY: single-threaded supervisor; `RECV_MSG` is lent for the call.
    let _sent = unsafe { channel_recv_raw(auth_ch, &raw mut RECV_MSG) }.ok()?;
    // SAFETY: read payload_len + form a bounded slice over the reply payload.
    let (result_ok, home_len) = unsafe {
        let payload_len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
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

/// One request to the view broker on `ch`; `None` if it failed or the broker answered an error.
fn views_call(ch: u64, op: u16, body: &[u8], out: &mut [u8]) -> Option<usize> {
    // SAFETY: SEND_MSG is a valid buffer; single-threaded supervisor.
    let rs_len = unsafe { encode(&mut SEND_MSG[PAYLOAD_OFF..], op, 1, 0, body, 0)? };
    // SAFETY: as above; a valid endpoint and a zero-handle send.
    let sr = unsafe {
        SEND_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        SEND_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const SEND_MSG) as u64,
            (&raw const SEND_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        )
    };
    if sr != 0 || !wait_one(ch) {
        return None;
    }
    // SAFETY: single-threaded supervisor; `RECV_MSG` is lent for the call.
    let _sent = unsafe { channel_recv_raw(ch, &raw mut RECV_MSG) }.ok()?;
    // SAFETY: bounded read of the reply the kernel just wrote.
    let reply = unsafe {
        let len = u32::from_le_bytes([RECV_MSG[4], RECV_MSG[5], RECV_MSG[6], RECV_MSG[7]]) as usize;
        core::slice::from_raw_parts(
            ((&raw const RECV_MSG) as *const u8).add(PAYLOAD_OFF),
            len.min(MSG_LEN - PAYLOAD_OFF),
        )
    };
    let m = decode(reply).ok()?;
    if m.op != op || m.is_error() {
        return None;
    }
    let n = m.body.len().min(out.len());
    out[..n].copy_from_slice(&m.body[..n]);
    Some(n)
}

/// Open a session with the view broker for `principal` over `sup`, writing its base `/s/<id>` into
/// `base_out`. Returns the id and the base's length, or `None` if the broker did not answer.
pub fn views_open_session(sup: u64, principal: &[u8], base_out: &mut [u8; 24]) -> Option<(u64, usize)> {
    let mut reply = [0u8; 8];
    let n = views_call(sup, librsproto::views::OP_VIEWS_OPEN_SESSION, principal, &mut reply)?;
    let id = librsproto::views::parse_session_id(&reply[..n])?;
    // `/s/` and at most twenty digits: 23 bytes, inside the 24 the caller gives.
    let mut buf = [0u8; 20];
    let d = libkern::debug::fmt_u64(id, &mut buf);
    base_out[..3].copy_from_slice(b"/s/");
    base_out[3..3 + d.len()].copy_from_slice(d);
    Some((id, 3 + d.len()))
}

/// Tell the view broker that session `id` has ended. **The supervisor's job**: a program in a view
/// still binds `/dev/views`, so the registration outlives the login.
pub fn views_close_session(sup: u64, id: u64) {
    let mut body = [0u8; 8];
    if let Some(n) = librsproto::views::build_session_id(&mut body, id) {
        let _ = views_call(sup, librsproto::views::OP_VIEWS_CLOSE_SESSION, &body[..n], &mut [0u8; 1]);
    }
}

/// [`build`] a session namespace, recording what bound for [`session_has_bin`] and its siblings.
/// Returns the namespace handle, or `0` on failure. **Absence is the sandbox**: `/dev/blk` is
/// omitted from every session; a disk is reached only through the view broker's `disks` grant.
pub fn build_namespace(spec: &NamespaceSpec<'_>) -> u64 {
    let Some(b) = build(spec) else { return 0 };
    // SAFETY: single-threaded supervisor; one namespace is built at a time.
    unsafe {
        SESSION_HAS_BIN = b.bin;
        SESSION_HAS_TTY = b.tty;
        SESSION_HAS_CONSOLE = b.console;
        SESSION_HAS_CLIPBOARD = b.clipboard;
        SESSION_HAS_VIEWS = b.views;
        SESSION_HAS_DEVICES = b.devices;
        SESSION_HAS_STORAGE = b.storage;
    }
    b.ns
}

/// **Build a namespace from `spec`** and say what bound. `None` if a member it cannot do without
/// did not: the namespace, `/home` when there is a home, or `/dev/draw/new` when asked for. Every
/// other member is reported and skipped on failure; a session without it still runs.
pub fn build(spec: &NamespaceSpec<'_>) -> Option<Built> {
    let NamespaceSpec {
        root_ns,
        fs_endpoint,
        profile_endpoint,
        tty_endpoint,
        clipboard_endpoint,
        home,
        user,
        bind_fonts,
        bind_console,
        views_endpoint,
        views_base,
        devices_endpoint,
        storage_endpoint,
        services_endpoint,
        bind_applications,
        draw_endpoint,
        desktop_endpoint,
    } = *spec;
    // SAFETY: register-only syscall; returns a fresh namespace handle or an error.
    let ns = unsafe { syscall0(SYS_NS_CREATE) };
    if ns < 0 {
        kprint(b"libsession: ns_create FAIL\n");
        return None;
    }
    let ns = ns as u64;
    let give_up = |why: &[u8]| {
        kprint(why);
        // SAFETY: closing the namespace we created.
        unsafe { syscall1(SYS_HANDLE_CLOSE, ns) };
        None
    };
    // `/dev/draw/new` → the compositor, **narrowly**: the base `/new` keeps `manage` unreachable;
    // `desktop-shell` checks that before it launches (`verify_app_namespace`).
    if draw_endpoint != 0 {
        let path = b"/dev/draw/new";
        let base = b"/new";
        // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
        let dr = unsafe {
            syscall6(
                SYS_NS_BIND,
                ns,
                path.as_ptr() as u64,
                path.len() as u64,
                draw_endpoint,
                base.as_ptr() as u64,
                base.len() as u64,
            )
        };
        if dr != 0 {
            return give_up(b"libsession: /dev/draw/new bind FAIL\n");
        }
    }
    // `/home` → the fs-server scoped to the user's subtree: the kernel prepends `home` to every
    // forwarded suffix. **Required when there is a home**: `PWD` starts at `/home`.
    if !home.is_empty() {
        let sub = b"/home";
        // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
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
            return give_up(b"libsession: /home subtree bind FAIL\n");
        }
    }
    // `/bin` → the profile server. **Not `/initramfs/sbin`**: that hands every session every boot
    // binary, the ambient authority namespaces refuse; a profile is a *choice* of what a user may
    // run. Non-fatal: a session with no `/bin` still has the in-process language.
    let mut has_bin = false;
    let mut has_tty = false;
    let mut has_console = false;
    let mut has_clipboard = false;
    if profile_endpoint != 0 {
        let bin = b"/bin";
        // **Scoped**: the server tells package directories apart by the suffix's first component.
        // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
        let pr = unsafe {
            syscall6(
                SYS_NS_BIND,
                ns,
                bin.as_ptr() as u64,
                bin.len() as u64,
                profile_endpoint,
                bin.as_ptr() as u64,
                bin.len() as u64,
            )
        };
        if pr != 0 {
            kprint(b"libsession: /bin bind FAIL (session has no programs)\n");
        }
        // `/applications` → the same server, projecting each package's `applications/`. Non-fatal.
        // **A session's alone**: an application launches nothing from a menu.
        if bind_applications {
            let apps = b"/applications";
            // **Absolute**: `SubtreeBase::from_path` rejects a bare component; `profile-server`'s
            // `split_suffix` strips the leading slash.
            let base = b"/applications";
            // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
            let ar = unsafe {
                syscall6(
                    SYS_NS_BIND,
                    ns,
                    apps.as_ptr() as u64,
                    apps.len() as u64,
                    profile_endpoint,
                    base.as_ptr() as u64,
                    base.len() as u64,
                )
            };
            if ar != 0 {
                kprint(b"libsession: /applications bind FAIL (the modal will be empty)\n");
            }
        }
        has_bin = pr == 0;
    }

    // `/session/user` → who this session belongs to; the kernel has no user identity. A memory
    // object is a snapshot, right while a session's user is immutable: the first mutable
    // `/session/*` member is the trigger for `TODO(session-metadata-server)`. **An application's
    // too**: `desktop-shell` passes the name on. An empty name binds nothing.
    if !user.is_empty() {
        bind_session_user(ns, user);
    }

    // `/dev/tty` → the terminal server's **forwarding** endpoint, so each resolve mints the
    // program its *own* terminal. A minted channel bound instead would answer `Namespace::Resolve`
    // with `Unsupported`: the kernel adopts *any* bound channel as a server.
    if tty_endpoint != 0 {
        let dev = b"/dev/tty";
        // SAFETY: valid namespace handle, path pointer, and endpoint handle; no base.
        let tr = unsafe { syscall6(SYS_NS_BIND, ns, dev.as_ptr() as u64, dev.len() as u64, tty_endpoint, 0, 0) };
        if tr != 0 {
            kprint(b"libsession: /dev/tty bind FAIL\n");
        }
        has_tty = tr == 0;
    }

    // `/dev/clipboard` → the clipboard server. **This binding is the authority**: there is no
    // ambient clipboard, and an endpoint attenuated to `RIGHT_SEND` would copy but not read.
    // The forwarding endpoint, for `/dev/tty`'s reason.
    if clipboard_endpoint != 0 {
        let dev = b"/dev/clipboard";
        // SAFETY: valid namespace handle, path pointer, and endpoint handle; no base.
        let cr = unsafe { syscall6(SYS_NS_BIND, ns, dev.as_ptr() as u64, dev.len() as u64, clipboard_endpoint, 0, 0) };
        if cr != 0 {
            kprint(b"libsession: /dev/clipboard bind FAIL (no copy and paste)\n");
        }
        has_clipboard = cr == 0;
    }

    // `/dev/views` → the view broker **at this session's base**, so a resolve reaches it as
    // `s/<session>`. Non-fatal.
    let mut has_views = false;
    if views_endpoint != 0 && !views_base.is_empty() {
        let dev = b"/dev/views";
        // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
        let vr = unsafe {
            syscall6(
                SYS_NS_BIND,
                ns,
                dev.as_ptr() as u64,
                dev.len() as u64,
                views_endpoint,
                views_base.as_ptr() as u64,
                views_base.len() as u64,
            )
        };
        if vr != 0 {
            kprint(b"libsession: /dev/views bind FAIL (no `with` in this session)\n");
        }
        has_views = vr == 0;
    }

    // `/dev/devices` → the device manager's info-only endpoint **at the base `/info`**, the
    // directory of tables. Non-fatal.
    let mut has_devices = false;
    if devices_endpoint != 0 {
        let dev = b"/dev/devices";
        let base = b"/info";
        // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
        let dvr = unsafe {
            syscall6(
                SYS_NS_BIND,
                ns,
                dev.as_ptr() as u64,
                dev.len() as u64,
                devices_endpoint,
                base.as_ptr() as u64,
                base.len() as u64,
            )
        };
        if dvr != 0 {
            kprint(b"libsession: /dev/devices bind FAIL (no device listing in this session)\n");
        }
        has_devices = dvr == 0;
    }

    // `/storage` (base `/fs`) and `/dev/storage` (base `/info`), both from the one endpoint.
    // Non-fatal.
    let has_storage = storage_endpoint != 0
        && [(&b"/storage"[..], &b"/fs"[..]), (b"/dev/storage", b"/info")].iter().all(|&(at, base)| {
            // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
            let r = unsafe {
                syscall6(
                    SYS_NS_BIND,
                    ns,
                    at.as_ptr() as u64,
                    at.len() as u64,
                    storage_endpoint,
                    base.as_ptr() as u64,
                    base.len() as u64,
                )
            };
            r == 0
        });
    if storage_endpoint != 0 && !has_storage {
        kprint(b"libsession: /storage bind FAIL (no filesystems in this session)\n");
    }

    // `/dev/services` → `service-mgr`'s session endpoint, with no base. Non-fatal.
    if services_endpoint != 0 {
        let at = b"/dev/services";
        // SAFETY: valid namespace handle, path pointer and endpoint handle.
        let r = unsafe { syscall6(SYS_NS_BIND, ns, at.as_ptr() as u64, at.len() as u64, services_endpoint, 0, 0) };
        if r != 0 {
            kprint(b"libsession: /dev/services bind FAIL (no service list in this session)\n");
        }
    }

    // `/system/fonts` → the fs-server scoped to that subtree. Read-only: no writable handle.
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
            kprint(b"libsession: /system/fonts bind FAIL (clients cannot render text)\n");
        }
    }

    // `/dev/console` → a direct-handle bind, resolved from our own namespace. **Skipped for a
    // graphical session** (see [`NamespaceSpec::bind_console`]): the console is shared.
    let (cst, console) =
        if bind_console { ns_lookup(root_ns, b"/dev/console", RIGHT_READ | RIGHT_TRANSFER) } else { (0, 0) };
    if cst == 0 && console != 0 {
        let dev = b"/dev/console";
        // SAFETY: valid namespace handle, path pointer, and console handle (a device
        // node → a direct-handle bind; no subtree base).
        let cr = unsafe { syscall6(SYS_NS_BIND, ns, dev.as_ptr() as u64, dev.len() as u64, console, 0, 0) };
        // SAFETY: the bind cloned its own reference; close ours.
        unsafe { syscall1(SYS_HANDLE_CLOSE, console) };
        if cr != 0 {
            kprint(b"libsession: /dev/console bind FAIL (shell has no console)\n");
        } else {
            has_console = true;
        }
    }
    // `/dev/desktop` → `desktop-shell`'s own endpoint, for an application. No base. Non-fatal.
    let mut has_desktop = false;
    if desktop_endpoint != 0 {
        let at = b"/dev/desktop";
        // SAFETY: valid namespace handle, path pointer and endpoint handle.
        let r = unsafe { syscall6(SYS_NS_BIND, ns, at.as_ptr() as u64, at.len() as u64, desktop_endpoint, 0, 0) };
        if r != 0 {
            kprint(b"libsession: /dev/desktop bind FAIL\n");
        }
        has_desktop = r == 0;
    }
    Some(Built {
        ns,
        bin: has_bin,
        tty: has_tty,
        console: has_console,
        clipboard: has_clipboard,
        views: has_views,
        devices: has_devices,
        storage: has_storage,
        desktop: has_desktop,
    })
}

/// **Hand every block device `from_ns`'s registry lists to `to_ns`, except those whose registry
/// ids are in `withheld`**, each with its `info` snapshot; returns how many were bound. The view
/// broker's `disks` grant, the only way a disk reaches a program; it withholds what is in use,
/// since a raw write would land underneath a live server. `sys_ns_bind` cannot re-bind the
/// kernel-server `/dev/blk`, so each device is bound singly. **Only a source with a registry hands
/// anything on**; the broker's is the root namespace.
pub fn rebind_block_devices_except(from_ns: u64, to_ns: u64, withheld: &[u32]) -> usize {
    let mut bound = 0;
    for n in block_indices(from_ns, withheld) {
        let path = blk_path(n, false);
        let (st, dev) = ns_lookup(
            from_ns,
            path.as_bytes(),
            RIGHT_READ | RIGHT_WRITE | RIGHT_DUPLICATE | RIGHT_INSPECT | RIGHT_TRANSFER,
        );
        if st != 0 || dev == 0 {
            // Not the end of the list: a source granted one disk has a gap before it.
            continue;
        }
        // SAFETY: valid namespace handle, path pointer and device handle — a direct-handle bind, so
        // no subtree base.
        let br = unsafe { syscall6(SYS_NS_BIND, to_ns, path.as_ptr() as u64, path.len() as u64, dev, 0, 0) };
        // SAFETY: the bind took its own reference; close ours.
        unsafe { syscall1(SYS_HANDLE_CLOSE, dev) };
        if br != 0 {
            continue;
        }
        bound += 1;
        // A snapshot, resolved once: device facts do not change while a machine runs.
        let info_path = blk_path(n, true);
        let (ist, info) = ns_lookup(from_ns, info_path.as_bytes(), RIGHT_MAP_READ | RIGHT_TRANSFER);
        if ist == 0 && info != 0 {
            // SAFETY: as above; the info snapshot binds as a direct handle too.
            let ir =
                unsafe { syscall6(SYS_NS_BIND, to_ns, info_path.as_ptr() as u64, info_path.len() as u64, info, 0, 0) };
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, info) };
            if ir != 0 {
                kprint(b"libsession: a device's info leaf would not bind\n");
            }
        }
    }
    if bound > 0 {
        Line::new().s(b"libsession: ").u(bound as u64).s(b" block device(s) handed to a namespace").end();
    }
    bound
}

/// Take back what [`rebind_block_devices_except`] bound into `ns`; returns how many devices were
/// unbound. A handle a program already holds is not taken back — the kernel has no revocation.
/// **What is bound is read from `ns` itself**, not a fixed range, which could miss a disk.
pub fn unbind_block_devices(ns: u64) -> usize {
    let mut unbound = 0;
    for n in bound_block_indices(ns) {
        // The leaf first, so a device is never left without its `info`; `NotFound` for a leaf
        // never bound is no error.
        let info_path = blk_path(n, true);
        // SAFETY: valid namespace handle and path.
        unsafe { syscall3(SYS_NS_UNBIND, ns, info_path.as_ptr() as u64, info_path.len() as u64) };
        let dev_path = blk_path(n, false);
        // SAFETY: as above.
        if unsafe { syscall3(SYS_NS_UNBIND, ns, dev_path.as_ptr() as u64, dev_path.len() as u64) } == 0 {
            unbound += 1;
        }
    }
    unbound
}

/// The `/dev/blk/<n>` indices `ns` can reach, less `withheld` registry ids. **From the registry**:
/// `/dev/blk` is one kernel-server binding whose children no enumeration sees.
fn block_indices(ns: u64, withheld: &[u32]) -> alloc::vec::Vec<u32> {
    let Some(blocks) = registry_blocks(ns) else { return alloc::vec::Vec::new() };
    let mut out: alloc::vec::Vec<u32> =
        blocks.iter().filter(|(id, _)| !withheld.contains(id)).map(|&(_, n)| n).collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn registry_blocks(ns: u64) -> Option<alloc::vec::Vec<(u32, u32)>> {
    let (st, snap) = ns_lookup(ns, b"/dev/registry", RIGHT_MAP_READ | RIGHT_INSPECT);
    if st != 0 || snap == 0 {
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
    // SAFETY: closing our own handle; a mapping outlives it.
    unsafe { syscall1(SYS_HANDLE_CLOSE, snap) };
    if addr < 0 {
        return None;
    }
    // SAFETY: `info.size` bytes are mapped read-only at `addr` until the unmap below.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, info.size as usize) };
    let out: alloc::vec::Vec<(u32, u32)> = match libkern::device::records(bytes) {
        Ok(records) => records.filter_map(|r| r.block_index().map(|n| (r.id, n))).collect(),
        Err(_) => alloc::vec::Vec::new(),
    };
    // SAFETY: unmapping what was mapped above; `out` holds copies.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, 0) };
    Some(out)
}

fn bound_block_indices(ns: u64) -> alloc::vec::Vec<u32> {
    let mut out = alloc::vec::Vec::new();
    let mut entry = abi::NsEntry::zeroed();
    for index in 0u64.. {
        // SAFETY: `entry` is a valid writable out-param of exactly `NsEntry`'s layout.
        let r = unsafe { syscall3(SYS_NS_ENUMERATE, ns, index, (&raw mut entry) as u64) };
        if r != 0 {
            break; // NotFound ends the walk
        }
        let len = (entry.path_len as usize).min(abi::NS_ENTRY_PATH_MAX);
        if let Some(n) = blk_binding_index(&entry.path[..len]) {
            out.push(n);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The `<n>` of a binding at exactly `/dev/blk/<n>`; decimal with no leading zero, so a device
/// has one spelling.
fn blk_binding_index(path: &[u8]) -> Option<u32> {
    let digits = path.strip_prefix(b"/dev/blk/")?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) || (digits.len() > 1 && digits[0] == b'0') {
        return None;
    }
    core::str::from_utf8(digits).ok()?.parse().ok()
}

fn blk_path(n: u32, info: bool) -> alloc::string::String {
    if info { alloc::format!("/dev/blk/{n}/info") } else { alloc::format!("/dev/blk/{n}") }
}

/// Whether the last-built session namespace got its `/bin`. The `session_has_*` readers exist so
/// a log line can say "no".
pub fn session_has_bin() -> bool {
    // SAFETY: single-threaded session-mgr.
    unsafe { SESSION_HAS_BIN }
}

static mut SESSION_HAS_BIN: bool = false;

/// Whether the last [`build_namespace`] bound `/dev/console`.
pub fn session_has_console() -> bool {
    // SAFETY: single-threaded supervisor; one namespace is built at a time.
    unsafe { SESSION_HAS_CONSOLE }
}

/// Whether the last [`build_namespace`] bound `/dev/clipboard`.
pub fn session_has_clipboard() -> bool {
    // SAFETY: single-threaded supervisor; one namespace is built at a time.
    unsafe { SESSION_HAS_CLIPBOARD }
}

static mut SESSION_HAS_CLIPBOARD: bool = false;

/// Whether the last [`build_namespace`] bound `/dev/views`.
pub fn session_has_views() -> bool {
    // SAFETY: single-threaded supervisor; one namespace is built at a time.
    unsafe { SESSION_HAS_VIEWS }
}

static mut SESSION_HAS_VIEWS: bool = false;

/// Whether the last [`build_namespace`] bound `/dev/devices`.
pub fn session_has_devices() -> bool {
    // SAFETY: single-threaded supervisor; one namespace is built at a time.
    unsafe { SESSION_HAS_DEVICES }
}

static mut SESSION_HAS_DEVICES: bool = false;

/// Whether the last [`build_namespace`] bound `/storage` and `/dev/storage`.
pub fn session_has_storage() -> bool {
    // SAFETY: single-threaded supervisor; one namespace is built at a time.
    unsafe { SESSION_HAS_STORAGE }
}

static mut SESSION_HAS_STORAGE: bool = false;

static mut SESSION_HAS_CONSOLE: bool = false;

/// Whether the last-built session namespace got a terminal.
pub fn session_has_tty() -> bool {
    // SAFETY: single-threaded session-mgr.
    unsafe { SESSION_HAS_TTY }
}

static mut SESSION_HAS_TTY: bool = false;

/// Publish `user` at `/session/user` in `ns`, as a read-only memory object. Non-fatal.
fn bind_session_user(ns: u64, user: &[u8]) {
    // SAFETY: a page-sized anonymous object; the syscall returns a handle or a negative error.
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
    let path = b"/session/user";
    // SAFETY: valid namespace handle, path pointer, and object handle (a direct-handle
    // bind — no subtree base).
    let br = unsafe { syscall6(SYS_NS_BIND, ns, path.as_ptr() as u64, path.len() as u64, mem, 0, 0) };
    // SAFETY: the bind cloned its own reference; close ours either way.
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
    // SAFETY: bytes 8..24 are inside WAIT_RESULTS, filled by the wait above; this thread owns it
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16],
                WAIT_RESULTS[17],
                WAIT_RESULTS[18],
                WAIT_RESULTS[19],
                WAIT_RESULTS[20],
                WAIT_RESULTS[21],
                WAIT_RESULTS[22],
                WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    (status, handle)
}

static mut NOTIF: Notification = Notification::zeroed();

static mut SPAWN_SHELL: SpawnArgs = SpawnArgs {
    image: 0,
    // One handle: the setup channel, through which a Tier-1 stage gets `argv` and environment.
    handle_count: 1,
    move_mask: 1,
    arg0: 0,
    handles: [0; 4],
    rights: [u64::MAX; 4],
    namespace: 0, // set at spawn = the session namespace
    syscaps: 0,   // set at spawn — see `spawn_leader`
};

/// The user's home *as seen from inside the session*; the outside path is not nameable.
const SESSION_HOME: &str = "/home";

/// Build the session's environment, a TSM1 `Record`. **`USER` is deliberately absent**: identity
/// lives at `/session/user`, which a process cannot forge; an env var it could tell a child.
pub fn session_env() -> libstream::wire::Record {
    use libstream::wire::{Record, Schema, TypeModifiers, TypeTag, Value};
    let schema = Schema::new()
        .field("HOME", TypeTag::String, TypeModifiers::NONE)
        .field("PWD", TypeTag::String, TypeModifiers::NONE)
        .field("PATH", TypeTag::List, TypeModifiers::NONE);
    let path: alloc::vec::Vec<Value> = alloc::vec![Value::Str(alloc::string::String::from("/bin"))];
    Record {
        schema,
        values: alloc::vec![
            Value::Str(alloc::string::String::from(SESSION_HOME)),
            Value::Str(alloc::string::String::from(SESSION_HOME)),
            Value::List(alloc::sync::Arc::from(path)),
        ],
    }
}

/// Spawn the leader `program` into `session_ns`, then block on `notif` for its `ChildExited` and
/// return its exit code. `-1` if it could not be spawned.
///
/// **`syscaps` is the caller's to choose.** The serial leader gets `0`, **and `0` is what every
/// command in the session gets**: `nxsh` passes each stage whatever it holds. The graphical
/// leader gets `SYSCAP_BIND_NAMESPACE`, because `desktop-shell` constructs a namespace per
/// application (`ui-composition-model.md` §5a, `graphical-session.md` §3).
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

    // Resolved from the *supervisor's* namespace: the session's exists to be handed to the leader.
    let mut path = alloc::string::String::from("/bin/");
    path.push_str(program);
    let image = ns_lookup(root_ns, path.as_bytes(), RIGHT_MAP_READ).1;
    if image == 0 {
        Line::new().s(b"libsession: ").s(program.as_bytes()).s(b" image not found").end();
        return -1;
    }
    // **Sized from what is sent**: sends are `NOBLOCK`, so a queue one short *drops the last
    // handle silently*.
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
    // SAFETY: the kernel copied the ELF during spawn; close our image handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, image) };
    if h < 0 {
        Line::new().s(b"libsession: ").s(program.as_bytes()).s(b" spawn FAIL").end();
        // A failed spawn never took `handles[0]`; a login is retried, so the leak would repeat.
        // SAFETY: closing a setup channel pair for a process that does not exist.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, setup_mgr);
            syscall1(SYS_HANDLE_CLOSE, setup_shell);
        }
        return -1;
    }

    // **`argv_rest` tells a leader what its namespace cannot show it**: `desktop-shell` needs the
    // user's real home to scope the `/home` it binds into applications, and a binding never
    // resolves back to its base. It states a fact; the fs endpoint carries the authority.
    let mut argv_buf: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(1 + argv_rest.len());
    argv_buf.push(program);
    argv_buf.extend_from_slice(argv_rest);
    let argv: &[&str] = &argv_buf;

    // No streams: a leader's I/O is what its namespace gives it, not a stream it was handed.
    let sent = send_setup_env(setup_mgr, &Streams::default(), argv, &session_env());
    // **One message per extra, in order, after the setup message.** Only `handles[0]` reaches a
    // child, so the rest arrive *over* the setup channel; a leader binding resources needs the
    // endpoints themselves, as a *binding* never resolves back to one.
    for &extra in extras {
        if sent.is_err() {
            continue;
        }
        // **A missing endpoint sends an empty message, not nothing.** The leader reads these
        // positionally, so skipping one shifts every later endpoint up a slot: `desktop-shell`
        // would bind the profile server at `/dev/tty` and no `/bin`. Reachable: `init`'s non-fatal
        // `bind_tty_server` failure propagates a zero. A failed duplicate or send does too.
        // **Duplicated, because a handle on an IPC message is always a *move***: the caller reuses
        // `extra` for every later session.
        let moved = if extra == 0 {
            0
        } else {
            // SAFETY: duplicating a handle the caller owns, with the rights a re-bind needs.
            unsafe { syscall2(SYS_HANDLE_DUPLICATE, extra, RIGHT_TRANSFER | RIGHT_DUPLICATE) }
        };
        if moved < 0 {
            kprint(b"libsession: could not duplicate the leader's extra handle\n");
        }
        let count: u64 = if moved > 0 { 1 } else { 0 };
        {
            // SAFETY: SEND_MSG/SEND_HANDLES are valid buffers; one moved handle, no payload.
            let r = unsafe {
                // **Scrub the payload first**: restamping only the header would send all `MSG_LEN`
                // bytes of whatever was sent last, such as an `Authenticate` request.
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
            if r != 0 {
                kprint(b"libsession: the leader's extra handle was not delivered\n");
                if count == 1 {
                    // SAFETY: the send failed, so the duplicate is still ours.
                    unsafe { syscall1(SYS_HANDLE_CLOSE, moved as u64) };
                }
            } else if count == 0 {
                // The leader cannot tell this from an endpoint that was never wanted.
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
    // **A terminate request is passed on**: a shutdown asks the supervisor, and the session is the
    // leader's to end. It has [`LEADER_STOP_NS`] to exit, and past that the supervisor goes on
    // without it — a stop is a request, and there is no forcible kill.
    let mut until = u64::MAX;
    loop {
        if !wait_until(notif, until) {
            if until != u64::MAX && now_ns() >= until {
                Line::new()
                    .s(b"libsession: ")
                    .s(program.as_bytes())
                    .s(b" is still running after it was asked to stop")
                    .end();
                // SAFETY: closing our reference to a process we no longer wait for.
                unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
                return -1;
            }
            continue;
        }
        loop {
            // SAFETY: NOTIF is a valid 64-byte writable out-param.
            let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
            if r != 0 {
                break;
            }
            // SAFETY: the recv above returned 0, so the kernel wrote a whole Notification into
            // NOTIF; this thread owns the static
            let (kind, body) = unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
            if kind == KIND_CHILD_EXITED {
                let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                // SAFETY: closing our reference to the exited shell (reaping).
                unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
                return code;
            }
            if kind == KIND_TERMINATE_REQUESTED && until == u64::MAX {
                STOP_ASKED.store(true, Ordering::Relaxed);
                let mut l = Line::new();
                l.s(b"libsession: asked to stop; passing it on to ").s(program.as_bytes()).end();
                // SAFETY: a register-only syscall on the leader's process handle, which we hold.
                unsafe { syscall1(SYS_PROCESS_TERMINATE, h as u64) };
                until = now_ns().saturating_add(LEADER_STOP_NS);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::blk_binding_index;

    /// A binding is a device only at exactly `/dev/blk/<n>`; lookalikes must not be (un)bound.
    #[test]
    fn a_device_binding_is_exactly_dev_blk_and_an_index() {
        assert_eq!(blk_binding_index(b"/dev/blk/0"), Some(0));
        assert_eq!(blk_binding_index(b"/dev/blk/12"), Some(12));
        assert_eq!(blk_binding_index(b"/dev/blk/4294967295"), Some(u32::MAX));
        for not in [
            &b"/dev/blk/0/info"[..],
            b"/dev/blk",
            b"/dev/blk/",
            b"/dev/blkx/0",
            b"/dev/blk/01",
            b"/dev/blk/1a",
            b"/dev/blk/-1",
            b"/dev/blk/4294967296",
            b"/home/dev/blk/0",
        ] {
            assert_eq!(blk_binding_index(not), None, "{:?}", core::str::from_utf8(not));
        }
    }
}
