//! `init` — PID 1. The kernel hands it a notification channel (`rdi`), a full-rights root
//! namespace (`rsi`) with the boot kernel-server bindings, and the system-control object (`rdx`).
//! It processes `/initramfs/etc/init.toml`'s mounts in dependency order, binds the profile server
//! at `/bin`, spawns `service-mgr` (which starts every other server), then reaps orphans and
//! answers the terminal channel. It never `panic!`s in normal operation (see its `CLAUDE.md`).

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use core::arch::asm;
use init::manifest::{self, BindSpec, Manifest, Mode, MountSpec};
use libkern::debug::Line;
use libkern::*;
use libos::{Handle, MapRead, Memory, Namespace, NsReadOnly, block_on};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// One page; init.toml is assumed to fit (true for the bootstrapping manifest).
const PAGE: u64 = 4096;

/// Bounded wait for an fs-server's Ready: init must not wait forever for a server.
const READY_TIMEOUT_NS: u64 = 30_000_000_000; // 30 s

/// The notification and terminal channels, what [`reap_loop`] waits on; others use slot 0.
static mut WAIT_HANDLES: [u64; 2] = [0; 2];
static mut WAIT_RESULTS: [u8; 48] = [0; 48];
static mut NOTIF: Notification = Notification::zeroed();

/// Control-channel endpoints of an fs-server handshake: init keeps `[0]`, the server gets `[1]`.
static mut CTRL0: u64 = 0;
static mut CTRL1: u64 = 0;

/// The root fs-server's endpoint, kept after the `/` mount for service-mgr, which hands it on for
/// each login's `/home`. `0` until mounted.
static mut FS_ENDPOINT: u64 = 0;
/// The profile server's endpoint, kept after the `/bin` bind for service-mgr (→ session-mgr). `0`
/// until bound. A `UserspaceServer` binding resolves to a registration record, not the endpoint,
/// so a session with a LOOKUP-only root could never obtain what binds `/bin` elsewhere.
static mut PROFILE_ENDPOINT: u64 = 0;
/// **The terminal channel** to `service-mgr`: the handoff channel, kept open. `0` while none.
static mut TERMINAL: u64 = 0;

/// The system-control object, or 0 without one: what `sys_power` takes at a shutdown's last step.
static mut SYSTEM_CONTROL: u64 = 0;

/// **`init`'s own mounts**, in the order made, each with its server's control channel ([`finish`]).
static mut KEPT: Vec<Kept> = Vec::new();

/// How long a filesystem server may take to answer `Meta::Unmount`: its write-back came first, so
/// what is left is one superblock write.
const UNMOUNT_TIMEOUT_NS: u64 = 10_000_000_000;

struct Kept {
    mount_point: String,
    control: u64,
}
/// The size of an `IpcMsg`: a 24-byte header, then the payload.
const IPC_MSG_LEN: usize = 4096;
static mut IPC_MSG: [u8; IPC_MSG_LEN] = [0; IPC_MSG_LEN];
static mut IPC_HANDLES: [u64; init::ready::IPC_HANDLE_MAX] = [0; init::ready::IPC_HANDLE_MAX];
static mut IPC_COUNT: usize = 0;
/// Spawn args for an `fs-server-ext4`: the control channel in `handles[0]` (`rdx`), a LOOKUP root.
static mut SPAWN_FS: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/fs-server-ext4
    handle_count: 1,
    move_mask: 1, // move handle 0 (the control endpoint) to the child
    arg0: 0,
    handles: [0; 4],
    rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_TRANSFER | RIGHT_WAIT, 0, 0, 0],
    namespace: 0,
    syscaps: 0, // a resource server holds no ambient capabilities
};
/// Spawn args for the system `profile-server`: the control channel in `handles[0]`, a LOOKUP root.
static mut SPAWN_PROFILE: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/profile-server
    handle_count: 1,
    move_mask: 1, // move handle 0 (the control endpoint) to the child
    arg0: 0,
    handles: [0; 4],
    rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_TRANSFER | RIGHT_WAIT, 0, 0, 0],
    namespace: 0,
    syscaps: 0, // a resource server holds no ambient capabilities
};
/// Spawn args for `eshell`: no handles, a LOOKUP-only root (for `/dev/console`, `/dev/blk/*`).
static mut SPAWN_ESHELL: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/eshell
    handle_count: 0,
    move_mask: 0,
    arg0: 0,
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0,
    syscaps: 0, // the recovery shell needs no ambient capabilities
};
/// Spawn args for the service manager: a LOOKUP-only root, `BIND_NAMESPACE` (its supervisor
/// capability) and `SYSTEM_CLOCK` (only to pass to the view broker). `handles[0]` is the **handoff
/// channel** end: init sends the bind-righted root, then the fs-server and profile-server
/// endpoints, so it carries `TRANSFER`, to hand those on, and `SEND`/`RECV`/`WAIT`.
static mut SPAWN_SERVICE_MGR: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /bin/service-mgr
    handle_count: 1,
    move_mask: 1, // move handle 0 (the handoff channel) to service-mgr
    arg0: 0,
    handles: [0; 4], // handles[0] = the handoff channel end, set at spawn
    rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_TRANSFER | RIGHT_DUPLICATE, 0, 0, 0],
    namespace: 0,
    syscaps: SYSCAP_BIND_NAMESPACE | SYSCAP_SYSTEM_CLOCK,
};

/// Resolve `path` in `ns` requesting `rights` and wait the PO: `(status, resolved_handle)`, from
/// the `IoResult`'s bytes 8..12 and 16..24.
fn ns_lookup_wait(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return (po as i32, 0);
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let status = unsafe { i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]) };
    // SAFETY: bytes 8..24 are inside `WAIT_RESULTS`, a static of plain bytes only this thread
    // touches; they hold the wait's result only when it returned 1.
    let resolved = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16],
            WAIT_RESULTS[17],
            WAIT_RESULTS[18],
            WAIT_RESULTS[19],
            WAIT_RESULTS[20],
            WAIT_RESULTS[21],
            WAIT_RESULTS[22],
            WAIT_RESULTS[23],
        ])
    };
    // SAFETY: closing our own PO handle (the resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    if waited != 1 {
        return (-1, 0);
    }
    (status, resolved)
}

/// Resolve a program `path` to its ELF `MemoryObject`, stamp it into `args.image`, spawn, and close
/// the image, which the kernel has copied. Returns the child's handle, or a negative error.
///
/// # Safety
/// `args` must point to a valid, writable `SpawnArgs` (its `image` field is overwritten).
unsafe fn spawn_program(root_ns: u64, path: &[u8], args: *mut SpawnArgs) -> i64 {
    let (st, img) = ns_lookup_wait(root_ns, path, RIGHT_MAP_READ);
    if st != 0 || img == 0 {
        Line::new().s(b"init: image not found: ").s(path).end();
        return -1;
    }
    // SAFETY: caller guarantees `args` is a valid writable SpawnArgs.
    unsafe { (*args).image = img };
    // SAFETY: `args` is the SpawnArgs the caller guarantees valid, with its image stamped above
    let h = unsafe { syscall1(SYS_PROCESS_SPAWN, args as u64) };
    // SAFETY: closing our own handle to the image object (the child has its own copy).
    unsafe { syscall1(SYS_HANDLE_CLOSE, img) };
    h
}

/// Read and parse `/initramfs/etc/init.toml`, logging the mount plan; `None` on any failure.
fn read_manifest(root_ns: u64) -> Option<Manifest> {
    let (st, mem) = ns_lookup_wait(root_ns, b"/initramfs/etc/init.toml", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        kprint(b"init: /initramfs/etc/init.toml not found (would drop to eshell)\n");
        return None;
    }
    // init.toml fits in one page; the server zero-fills the tail, so trailing NULs are trimmed.
    // SAFETY: `mem` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, mem, 0, PAGE, RIGHT_MAP_READ) };
    if addr < 0 {
        kprint(b"init: init.toml map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
        return None;
    }
    // SAFETY: `addr` is a MAP_READ page holding the file bytes + zero padding.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, PAGE as usize) };
    let len = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    let result = match core::str::from_utf8(&bytes[..len]) {
        Ok(text) => match manifest::parse(text) {
            Ok(parsed) => {
                Line::new()
                    .s(b"init: init.toml OK, ")
                    .u(parsed.mounts.len() as u64)
                    .s(b" mount(s) (shallowest first), ")
                    .u(parsed.binds.len() as u64)
                    .s(b" bind(s):")
                    .end();
                for m in &parsed.mounts {
                    Line::new()
                        .s(b"init:   ")
                        .s(m.mount_point.as_bytes())
                        .s(b": ")
                        .s(m.fs_server.as_bytes())
                        .s(b" on ")
                        .s(m.device.as_bytes())
                        .s(b" (")
                        .s(match m.mode {
                            Mode::Ro => b"ro" as &[u8],
                            Mode::Rw => b"rw",
                        })
                        .s(b")")
                        .end();
                }
                for b in &parsed.binds {
                    Line::new()
                        .s(b"init:   ")
                        .s(b.path.as_bytes())
                        .s(b": bind of ")
                        .s(b.source.as_bytes())
                        .s(b" scoped to ")
                        .s(b.subtree.as_bytes())
                        .end();
                }
                Some(parsed)
            }
            Err(_) => {
                kprint(b"init: init.toml parse error (would drop to eshell)\n");
                None
            }
        },
        Err(_) => {
            kprint(b"init: init.toml not UTF-8 (would drop to eshell)\n");
            None
        }
    };
    // SAFETY: closing our own handle; the mapping kept the object alive, and the
    // parsed mounts own their strings, so the mapped bytes are no longer needed.
    unsafe { syscall1(SYS_HANDLE_CLOSE, mem) };
    result
}

/// Process the manifest: every mount shallowest first, then every bind; `true` iff all succeeded. A
/// failure is critical-path: a bind that silently did not happen is how a later lookup fails for a
/// reason nobody can see. **Every mount's endpoint is held until the binds are done**, as a bind's
/// source may be any mount; then the root's is kept for `service-mgr` and the rest closed.
fn mount_all(root_ns: u64, manifest: &Manifest) -> bool {
    let mut ok = true;
    let mut endpoints: Vec<(&str, u64)> = Vec::new();
    for m in &manifest.mounts {
        match mount_one(root_ns, m) {
            Some(endpoint) => endpoints.push((m.mount_point.as_str(), endpoint)),
            None => {
                Line::new().s(b"init: mount FAILED for ").s(m.mount_point.as_bytes()).end();
                ok = false;
            }
        }
    }
    for b in &manifest.binds {
        if !bind_one(root_ns, b, &endpoints) {
            Line::new().s(b"init: bind FAILED for ").s(b.path.as_bytes()).end();
            ok = false;
        }
    }
    for (mount_point, endpoint) in endpoints {
        if mount_point == "/" {
            // SAFETY: single-threaded init; the global takes ownership of `endpoint`.
            unsafe { FS_ENDPOINT = endpoint };
        } else {
            // SAFETY: closing our own handle; the binding holds its own reference.
            unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
        }
    }
    ok
}

/// Bind one `[[bind]]`: the source mount's endpoint again at `b.path`, scoped to `b.subtree`. The
/// kernel shares the source's server registration across both names rather than minting a rival.
fn bind_one(root_ns: u64, b: &BindSpec, endpoints: &[(&str, u64)]) -> bool {
    // The parser checked the source names a mount; it is absent only if that mount failed.
    let Some(&(_, endpoint)) = endpoints.iter().find(|(point, _)| *point == b.source) else {
        return false;
    };
    // `/` scopes to the whole tree, which the kernel spells as no base at all.
    let base: &[u8] = if b.subtree == "/" { b"" } else { b.subtree.as_bytes() };
    // SAFETY: valid namespace handle, path/base pointers and lengths, and an endpoint handle init
    // holds.
    let r = unsafe {
        syscall6(
            SYS_NS_BIND,
            root_ns,
            b.path.as_ptr() as u64,
            b.path.len() as u64,
            endpoint,
            base.as_ptr() as u64,
            base.len() as u64,
        )
    };
    if r != 0 {
        return false;
    }
    Line::new()
        .s(b"init: bound ")
        .s(b.path.as_bytes())
        .s(b" to ")
        .s(b.source.as_bytes())
        .s(b" scoped to ")
        .s(b.subtree.as_bytes())
        .end();
    true
}

/// Mount one `[[mount]]`: the Resource Server Startup Protocol from init's side. Returns the
/// server's endpoint, bound at `m.mount_point`; the caller owns it.
fn mount_one(root_ns: u64, m: &MountSpec) -> Option<u64> {
    if m.fs_server != "fs-server-ext4" {
        Line::new().s(b"init: unknown fs_server '").s(m.fs_server.as_bytes()).s(b"'").end();
        return None;
    }
    let dev_path = match manifest::device_ns_path(&m.device) {
        Some(p) => p,
        None => {
            Line::new().s(b"init: unsupported device scheme '").s(m.device.as_bytes()).s(b"'").end();
            return None;
        }
    };
    // READ+WRITE (the RW server writes metadata) + TRANSFER (to hand it over) + DUPLICATE (the
    // server hands the kernel a copy for the data path).
    let (st, device) =
        ns_lookup_wait(root_ns, dev_path.as_bytes(), RIGHT_READ | RIGHT_WRITE | RIGHT_TRANSFER | RIGHT_DUPLICATE);
    if st != 0 || device == 0 {
        Line::new().s(b"init: device ").s(dev_path.as_bytes()).s(b" not found").end();
        return None;
    }

    // SAFETY: CTRL0/CTRL1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0) };
    if cr != 0 {
        // SAFETY: closing our own handle; the channel just failed to create
        unsafe { syscall1(SYS_HANDLE_CLOSE, device) };
        return None;
    }
    // SAFETY: `sys_channel_create` just wrote both endpoints; init is single-threaded.
    let (ctrl_init, ctrl_srv) = unsafe { ((&raw const CTRL0).read(), (&raw const CTRL1).read()) };

    // SAFETY: SPAWN_FS is a valid writable arg block; spawn_program resolves the ELF
    // image from the initramfs, stamps it, spawns, and closes the image handle.
    let fs_h = unsafe {
        SPAWN_FS.handles[0] = ctrl_srv;
        spawn_program(root_ns, b"/initramfs/sbin/fs-server-ext4", &raw mut SPAWN_FS)
    };
    if fs_h < 0 {
        kprint(b"init: fs-server spawn FAIL\n");
        // SAFETY: closing our own handles; a failed spawn moves none, `ctrl_srv` included.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, device);
            syscall1(SYS_HANDLE_CLOSE, ctrl_init);
            syscall1(SYS_HANDLE_CLOSE, ctrl_srv);
        }
        return None;
    }

    // Setup message: the device and a flags byte (read-only for `"ro"`); NoBlock, ring empty.
    // SAFETY: IPC_MSG/IPC_HANDLES are valid buffers; transferring one handle.
    let sr = unsafe {
        IPC_MSG[4..8].copy_from_slice(&1u32.to_le_bytes());
        IPC_MSG[24] = m.mode.setup_flags();
        IPC_HANDLES[0] = device;
        syscall5(
            SYS_CHANNEL_SEND,
            ctrl_init,
            (&raw const IPC_MSG) as u64,
            (&raw const IPC_HANDLES) as u64,
            1,
            SENDMODE_NOBLOCK,
        )
    };
    if sr != 0 {
        kprint(b"init: device handoff FAIL\n");
        // SAFETY: closing our own handles; the failed send did not move the device
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, device);
            syscall1(SYS_HANDLE_CLOSE, ctrl_init);
        }
        return None;
    }

    let who: [&[u8]; 4] = [b"fs-server-ext4 for ", m.mount_point.as_bytes(), b" on ", m.device.as_bytes()];
    let endpoint = match wait_ready(ctrl_init, &who) {
        Some(e) => e,
        None => {
            // SAFETY: closing our own control endpoint; the handshake failed
            unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
            return None;
        }
    };
    // The control channel is kept for a shutdown's `Meta::Unmount` ([`finish`]).

    // Bind the endpoint at the mount point; `mount_all` may bind it again for a `[[bind]]`.
    // SAFETY: valid namespace handle + path pointer + endpoint handle.
    let br =
        unsafe { syscall4(SYS_NS_BIND, root_ns, m.mount_point.as_ptr() as u64, m.mount_point.len() as u64, endpoint) };
    if br != 0 {
        Line::new().s(b"init: bind FAIL at ").s(m.mount_point.as_bytes()).end();
        // SAFETY: closing our own handles; nothing was bound with them.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, endpoint);
            syscall1(SYS_HANDLE_CLOSE, ctrl_init);
        }
        return None;
    }
    let kept = Kept { mount_point: m.mount_point.clone(), control: ctrl_init };
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: single-threaded init; `KEPT` is only ever reached through this raw pointer.
    unsafe {
        (*(&raw mut KEPT)).push(kept)
    };

    Line::new().s(b"init: mounted fs-server-ext4 at ").s(m.mount_point.as_bytes()).end();
    // init keeps `fs_h` (the long-lived server's process handle).
    let _ = fs_h;
    Some(endpoint)
}

/// Wait (bounded) for a resource server's `Meta::Ready` on `ctrl` and return the endpoint it
/// transfers. `None` if there is none — and then **init has already said why**, naming the server
/// as the `who` pieces spell it: the four failures (too slow, exited first, sent something else,
/// **refused** with a reason) are four problems on a machine nobody can attach a debugger to.
fn wait_ready(ctrl: u64, who: &[&[u8]]) -> Option<u64> {
    let say = |what: &[u8]| {
        let mut line = Line::new();
        line.s(b"init: ");
        for piece in who {
            line.s(piece);
        }
        line.s(what);
        line
    };

    let mut now: u64 = 0;
    // SAFETY: `&now` is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    let deadline = now.saturating_add(READY_TIMEOUT_NS);

    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter, with deadline.
    let waited = unsafe {
        WAIT_HANDLES[0] = ctrl;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline)
    };
    if waited == KError::TimedOut as i64 {
        say(b" sent no Ready within ").u(READY_TIMEOUT_NS / 1_000_000_000).s(b" s").end();
        return None;
    }
    if waited < 1 {
        say(b": waiting for its Ready failed, error ").i(waited).end();
        return None;
    }
    // SAFETY: valid recv out-params; on success the kernel installs handles[0].
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ctrl,
            (&raw mut IPC_MSG) as u64,
            (&raw mut IPC_HANDLES) as u64,
            (&raw mut IPC_COUNT) as u64,
        )
    };
    // A queued message is received before the closed peer is: this is a server that exited
    // without sending anything, not one that refused and then exited.
    if rr == KError::PeerClosed as i64 {
        say(b" exited without sending Ready").end();
        return None;
    }
    if rr != 0 {
        say(b": its Ready could not be received, error ").i(rr).end();
        return None;
    }
    // SAFETY: the kernel wrote the count, the header, and `count` handles.
    let (count, payload_len) = unsafe {
        let count = (&raw const IPC_COUNT).read();
        let len = u32::from_le_bytes([IPC_MSG[4], IPC_MSG[5], IPC_MSG[6], IPC_MSG[7]]) as usize;
        (count, len.min(IPC_MSG_LEN - 24))
    };
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: IPC_MSG and IPC_HANDLES are init's own buffers, read after the kernel filled them;
    // single-threaded init writes them again only at its next receive.
    let (msg, handles) = unsafe { (&*(&raw const IPC_MSG), &*(&raw const IPC_HANDLES)) };
    let first = init::ready::parse(&msg[24..24 + payload_len], count);
    for &extra in &handles[init::ready::handles_to_close(&first, count)] {
        // SAFETY: a handle that came with the message and is not the endpoint init binds; nothing
        // else holds it.
        unsafe { syscall1(SYS_HANDLE_CLOSE, extra) };
    }
    match first {
        init::ready::First::Ready => return Some(handles[0]),
        init::ready::First::Refused(reason) if !reason.is_empty() => say(b" refused: ").untrusted(reason).end(),
        init::ready::First::Refused(_) => say(b" refused, giving no reason init could read").end(),
        init::ready::First::Unexpected => say(b" sent something other than a Ready").end(),
    }
    None
}

/// Spawn the system profile server and bind its endpoint at `/bin`: [`mount_one`] minus the device.
/// It answers forwarded `/bin/<prog>` resolves with the matching `/store/.../bin/<prog>` handle.
/// A failure is critical-path: without `/bin`, no program resolves for the services.
fn bind_profile_server(root_ns: u64) -> bool {
    // SAFETY: CTRL0/CTRL1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0) };
    if cr != 0 {
        return false;
    }
    // SAFETY: `sys_channel_create` just wrote both endpoints; init is single-threaded.
    let (ctrl_init, ctrl_srv) = unsafe { ((&raw const CTRL0).read(), (&raw const CTRL1).read()) };

    // SAFETY: SPAWN_PROFILE is a valid writable arg block; spawn_program resolves the
    // ELF image from the initramfs, stamps it, spawns, and closes the image handle.
    let ps_h = unsafe {
        SPAWN_PROFILE.handles[0] = ctrl_srv;
        spawn_program(root_ns, b"/initramfs/sbin/profile-server", &raw mut SPAWN_PROFILE)
    };
    if ps_h < 0 {
        kprint(b"init: profile-server spawn FAIL\n");
        // SAFETY: closing both control ends; a failed spawn moves neither.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, ctrl_init);
            syscall1(SYS_HANDLE_CLOSE, ctrl_srv);
        }
        return false;
    }

    let endpoint = match wait_ready(ctrl_init, &[b"profile-server".as_slice()]) {
        Some(e) => e,
        None => {
            // SAFETY: closing our own control endpoint.
            unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
            return false;
        }
    };
    // SAFETY: closing our own control endpoint.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };

    // Keep a second handle to the endpoint *before* binding, for service-mgr to carry to
    // session-mgr, so a failure is a failure to bind at all. `TRANSFER | DUPLICATE` is all the
    // hand-down needs.
    // SAFETY: duplicating our own endpoint handle with attenuated rights.
    let retained = unsafe { syscall2(SYS_HANDLE_DUPLICATE, endpoint, RIGHT_TRANSFER | RIGHT_DUPLICATE) };
    if retained < 0 {
        kprint(b"init: profile endpoint duplicate FAIL\n");
        // SAFETY: closing our own endpoint handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
        return false;
    }

    // Bind the endpoint at `/bin`; the retained duplicate keeps the *same* endpoint alive, so a
    // later bind shares this registration. **Scoped to `/bin`, not whole-tree**: the server tells a
    // package's projected directories apart by the suffix's first part, so unscoped,
    // `/bin/applications` would reach the *applications* projection.
    // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
    let br =
        unsafe { syscall6(SYS_NS_BIND, root_ns, b"/bin".as_ptr() as u64, 4, endpoint, b"/bin".as_ptr() as u64, 4) };
    // SAFETY: closing init's endpoint handle (the binding holds its own reference).
    unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
    if br != 0 {
        kprint(b"init: profile-server bind FAIL at /bin\n");
        // SAFETY: closing the retained duplicate; nothing will use it.
        unsafe { syscall1(SYS_HANDLE_CLOSE, retained as u64) };
        return false;
    }
    // SAFETY: single-threaded init.
    unsafe { PROFILE_ENDPOINT = retained as u64 };

    kprint(b"init: profile server bound at /bin\n");
    // init keeps `ps_h` (the long-lived server's process handle).
    let _ = ps_h;
    true
}

/// Read and log `/system/current-generation` through the root fs-server: the stack end to end.
fn read_current_generation(root_ns: u64) {
    // SAFETY: `root_ns` is init's live root namespace, owned for its whole run; a
    // borrowed Handle is a non-owning view and never closes it.
    let ns = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: the path resolves to a read-mappable file object (asserted by the `Memory, MapRead`
    // type arguments).
    let mem = match block_on(unsafe { ns.lookup::<Memory, MapRead>("/system/current-generation", Rights::MAP_READ) }) {
        Ok(m) => m,
        Err(_) => {
            kprint(b"init: /system/current-generation lookup FAIL\n");
            return;
        }
    };
    let addr = match mem.map(PAGE as usize) {
        Ok(a) => a,
        Err(_) => {
            kprint(b"init: current-generation map FAIL\n");
            return; // `mem` drops here → closes the resolved handle
        }
    };
    // SAFETY: `addr` maps a page of the file bytes + zero padding; trim the tail.
    let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, PAGE as usize) };
    let len = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    kprint(b"init: /system/current-generation = ");
    kprint(&bytes[..len]); // the file content ends in '\n'
}

/// Report the run's verdict to `xtask test-qemu` via `SYS_TEST_EXIT`, which ends QEMU under the
/// kernel's `test-harness` feature. Returns `()`, not `!`, so a missing exit device degrades to a
/// normal boot, not a hang. **Unconditional, in every build**: elsewhere it returns `Unsupported`,
/// on a path that is already a boot failure. `init` only ever fires **FAIL**; PASS is
/// `boot-probe`'s.
fn test_exit(ok: bool) {
    let code = if ok { TEST_EXIT_SUCCESS } else { TEST_EXIT_FAILURE };
    kprint(if ok { b"init: test-harness verdict PASS\n" } else { b"init: test-harness verdict FAIL\n" });
    // SAFETY: SYS_TEST_EXIT takes the verdict code in a0; under the kernel's
    // test-harness build it writes `isa-debug-exit` and QEMU terminates (so in
    // practice this syscall does not return).
    unsafe { syscall1(SYS_TEST_EXIT, code as u64) };
}

fn spawn_eshell(root_ns: u64) {
    kprint(b"init: starting interactive console (eshell)\n");
    // SAFETY: SPAWN_ESHELL is a valid writable arg block.
    let h = unsafe { spawn_program(root_ns, b"/initramfs/sbin/eshell", &raw mut SPAWN_ESHELL) };
    if h < 0 {
        kprint(b"init: eshell spawn FAIL\n");
    } else {
        // SAFETY: closing init's reference; eshell runs independently.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
    }
}

/// Spawn the service manager, the normal boot handoff: its process handle, or a negative error.
/// [`supervise`] closes it at once: the terminal channel closing names its death ([`reap_loop`]).
/// Only `handles[0]` reaches a child (`rdx`), so the endpoints travel on a channel.
fn spawn_service_mgr(root_ns: u64) -> i64 {
    kprint(b"init: handing off to service manager\n");
    // **The terminal channel.** Depth 4: three handoffs, queued before `service-mgr` runs — a ring
    // shorter than the sends drops the last one silently — and then the ops it sends back.
    // SAFETY: CTRL0/CTRL1 are valid writable out-params (mounts are long done).
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0) };
    if cr != 0 {
        kprint(b"init: service-mgr handoff channel FAIL\n");
        // SAFETY: nothing was handed off; the endpoints are ours to close.
        unsafe { close_retained_endpoints() };
        return -1;
    }
    // SAFETY: `sys_channel_create` just wrote both endpoints; init is single-threaded
    let (init_end, child_end) = unsafe { ((&raw const CTRL0).read(), (&raw const CTRL1).read()) };

    // SAFETY: single-threaded init; stamp the handoff end into the (moved) handle slot,
    // then spawn. `move_mask`/`handle_count`/`rights` are set in the static.
    let h = unsafe {
        SPAWN_SERVICE_MGR.handles[0] = child_end;
        spawn_program(root_ns, b"/bin/service-mgr", &raw mut SPAWN_SERVICE_MGR)
    };
    if h < 0 {
        kprint(b"init: service-mgr spawn FAIL\n");
        // A failed spawn moved nothing: close both ends and the endpoints they would have carried.
        // SAFETY: closing our own handles.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, init_end);
            syscall1(SYS_HANDLE_CLOSE, child_end);
            close_retained_endpoints();
        }
        return h;
    }

    // The handoffs, in the order service-mgr receives them: 1. **the root, with init's own rights**
    // (`BIND` and `UNBIND`: a spawned process gets only a lookup-only root, and `service-mgr` binds
    // the servers); 2. the root filesystem's endpoint; 3. the profile server's.
    // SAFETY: duplicating init's own root handle with every right it holds; single-threaded init,
    // and each endpoint moves once — the sends null the statics.
    unsafe {
        let root = syscall2(SYS_HANDLE_DUPLICATE, root_ns, u64::MAX);
        if root <= 0 {
            kprint(b"init: root handle duplicate FAIL -- service-mgr will bind nothing\n");
        }
        send_handle(init_end, if root > 0 { root as u64 } else { 0 });
        send_handle(init_end, FS_ENDPOINT);
        FS_ENDPOINT = 0;
        send_handle(init_end, PROFILE_ENDPOINT);
        PROFILE_ENDPOINT = 0;
        TERMINAL = init_end;
    }
    h
}

/// Transfer one `handle` over a handoff channel: an IPC message with a single moved handle and no
/// payload. On failure it did not move, so it is closed here. A zero `handle` sends **an empty
/// message**, not nothing: the receiver reads the handoffs positionally.
fn send_handle(ctrl: u64, handle: u64) {
    let count = if handle == 0 { 0 } else { 1 };
    // SAFETY: IPC_MSG/IPC_HANDLES are valid buffers; transferring `count` handles with an
    // empty payload. NoBlock: `spawn_service_mgr`'s ring is sized to hold every handoff.
    let sr = unsafe {
        IPC_MSG[4..8].copy_from_slice(&0u32.to_le_bytes());
        IPC_HANDLES[0] = handle;
        syscall5(
            SYS_CHANNEL_SEND,
            ctrl,
            (&raw const IPC_MSG) as u64,
            (&raw const IPC_HANDLES) as u64,
            count,
            SENDMODE_NOBLOCK,
        )
    };
    if sr != 0 {
        kprint(b"init: handoff send FAIL\n");
        if handle != 0 {
            // SAFETY: the transfer did not happen; reclaim the handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
        }
    }
}

/// Close whichever server endpoints init still holds for the handoff, when it cannot happen.
///
/// # Safety
/// Single-threaded init; the statics are init's own handles.
unsafe fn close_retained_endpoints() {
    // SAFETY: closing our own handles; the statics are nulled so no path closes twice.
    unsafe {
        if FS_ENDPOINT != 0 {
            syscall1(SYS_HANDLE_CLOSE, FS_ENDPOINT);
            FS_ENDPOINT = 0;
        }
        if PROFILE_ENDPOINT != 0 {
            syscall1(SYS_HANDLE_CLOSE, PROFILE_ENDPOINT);
            PROFILE_ENDPOINT = 0;
        }
    }
}

/// The healthy supervise path: spawn `service-mgr` and supervise it via [`reap_loop`].
fn supervise(notif: u64, root_ns: u64) -> ! {
    let service_mgr_h = spawn_service_mgr(root_ns);
    if service_mgr_h <= 0 {
        // No `service-mgr` is no server at all (`auth-service`, `logging-service`): console free.
        emergency(notif, root_ns);
    }
    // SAFETY: closing init's reference to the process; it runs independently. Its death is
    // learned from the terminal channel closing, not from a handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, service_mgr_h as u64) };
    reap_loop(notif, root_ns);
}

/// The **emergency** path: a critical-path boot failure. Drop straight to the interactive shell
/// so the operator can inspect the broken system (`userspace/init/CLAUDE.md` § Failure → eshell).
fn emergency(notif: u64, root_ns: u64) -> ! {
    kprint(b"init: critical-path failure -- dropping to emergency shell\n");
    // A critical-path boot failure is a failed test run. Outside `test-qemu` the verdict device is
    // absent, the syscall returns `Unsupported`, and the boot goes on to the shell, as on hardware.
    test_exit(false);
    spawn_eshell(root_ns);
    reap_loop(notif, root_ns);
}

/// Reap exited children forever (init is the eventual parent of every orphan), and answer the
/// terminal channel: `TERMINAL_OP_EMERGENCY` (a critical server did not come up) starts the
/// emergency shell, once, the console being still free; `TERMINAL_OP_FINISH` runs [`finish`].
///
/// **The channel's closing is `service-mgr`'s death**, attributed exactly (`KIND_CHILD_EXITED`
/// names a pid, and nothing maps a handle to one). It is **reported, not answered with a restart**:
/// its death takes every server path with it, a second `service-mgr` would start a second copy of
/// every server, and the emergency shell cannot take a console the terminal server still holds. The
/// machine needs a restart.
fn reap_loop(notif: u64, root_ns: u64) -> ! {
    kprint(b"init: entering reaping loop\n");
    let mut shell_started = false;
    // A `Finish` that returned: the machine could not stop itself. A second changes nothing.
    let mut finished = false;
    loop {
        // SAFETY: single-threaded init; the terminal channel is read only here.
        let terminal = unsafe { TERMINAL };
        let count = if terminal != 0 { 2 } else { 1 };
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers for two entries.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            WAIT_HANDLES[1] = terminal;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, count, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        if waited < 1 {
            continue;
        }
        loop {
            // SAFETY: NOTIF is a valid 64-byte writable out-param.
            let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
            if r != 0 {
                break; // WouldBlock: drained
            }
            // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
            let (kind, body) = unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
            if kind == KIND_CHILD_EXITED {
                let cpid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                Line::new()
                    .s(b"init: reaped pid=")
                    .u(cpid as u64)
                    .s(b" code=")
                    // `.i`, not `.u`: an exit code is signed; `-1` widened through `as u64` prints
                    // 18446744073709551615.
                    .i(code as i64)
                    .end();
            }
        }
        if terminal == 0 {
            continue;
        }
        loop {
            // SAFETY: IPC_MSG/IPC_HANDLES/IPC_COUNT are valid writable out-params.
            let rr = unsafe {
                syscall4(
                    SYS_CHANNEL_RECV,
                    terminal,
                    (&raw mut IPC_MSG) as u64,
                    (&raw mut IPC_HANDLES) as u64,
                    (&raw mut IPC_COUNT) as u64,
                )
            };
            if rr == KError::PeerClosed.as_i32() as i64 {
                Line::new()
                    .s(b"init: service-mgr has exited -- nothing supervises the services now, ")
                    .s(b"and the machine needs a restart")
                    .end();
                // SAFETY: closing our own handle; the static is nulled so it is not waited on
                // again.
                unsafe {
                    syscall1(SYS_HANDLE_CLOSE, terminal);
                    TERMINAL = 0;
                }
                break;
            }
            if rr != 0 {
                break; // WouldBlock: nothing more
            }
            // SAFETY: the kernel wrote the message; its payload at offset 24, an op and, for
            // `Finish`, whether to reboot.
            let (len, op, reboot, handles) = unsafe {
                (
                    u32::from_le_bytes([IPC_MSG[4], IPC_MSG[5], IPC_MSG[6], IPC_MSG[7]]),
                    IPC_MSG[24],
                    IPC_MSG[25] != 0,
                    (&raw const IPC_COUNT).read(),
                )
            };
            #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
            for k in 0..handles.min(init::ready::IPC_HANDLE_MAX) {
                // SAFETY: a handle the kernel installed, which no request carries.
                unsafe { syscall1(SYS_HANDLE_CLOSE, (&raw const IPC_HANDLES[k]).read()) };
            }
            if len >= 1 && op == TERMINAL_OP_EMERGENCY && !shell_started {
                Line::new()
                    .s(b"init: service-mgr asks for the emergency shell -- ")
                    .s(b"the boot cannot go on without what it could not start (it says what above)")
                    .end();
                // The same verdict a critical-path failure here gives: the boot failed.
                test_exit(false);
                spawn_eshell(root_ns);
                shell_started = true;
            }
            if len >= 2 && op == TERMINAL_OP_FINISH && !finished {
                finished = true;
                finish(root_ns, reboot);
            }
        }
    }
}

/// **Finish a shutdown.** `service-mgr` has asked every session and service to stop. What is left
/// is `init`'s own: its mounts, last first — each written back with `sys_ns_sync`, then told
/// `Meta::Unmount`, on which its server records the filesystem clean and exits — and then the
/// machine, through `sys_power`. Whatever one mount answers, the next is still unmounted, since one
/// stopped half way leaves every later filesystem not clean; whatever still runs writes after the
/// sync at its own risk, as at a power cut.
///
/// **The sync is the one wait `init` does not bound**: it waits on write IRPs, and an
/// interrupt-driven IRP has no force-complete timeout yet (`deferred-decisions.md` § *IRP
/// cancellation and the completion timeout*); each `Meta::Unmount` answer is bounded by
/// `UNMOUNT_TIMEOUT_NS`. Returns only if the machine cannot be stopped, after saying so.
fn finish(root_ns: u64, reboot: bool) {
    kprint(if reboot {
        b"init: finishing the shutdown, to reboot: its own filesystems, last mounted first\n"
    } else {
        b"init: finishing the shutdown: its own filesystems, last mounted first\n"
    });
    #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
    // SAFETY: single-threaded init; `KEPT` is only ever reached through this raw pointer, and is
    // taken whole so that nothing unmounted is unmounted twice.
    let kept = unsafe { core::mem::take(&mut *(&raw mut KEPT)) };
    for m in kept.iter().rev() {
        unmount_one(root_ns, m);
    }
    // SAFETY: single-threaded init; written once at startup.
    let system_control = unsafe { SYSTEM_CONTROL };
    if system_control == 0 {
        Line::new()
            .s(b"init: no system-control object, so the machine cannot stop itself; ")
            .s(b"its filesystems are unmounted, and it is safe to turn off")
            .end();
        return;
    }
    let op = if reboot { POWER_REBOOT } else { POWER_HALT };
    // SAFETY: a register-only syscall on a handle init holds; on success it does not return.
    let r = unsafe { syscall2(SYS_POWER, system_control, op) };
    Line::new()
        .s(b"init: sys_power refused (")
        .i(r)
        .s(b"), so the machine cannot stop itself; ")
        .s(b"its filesystems are unmounted, and it is safe to turn off")
        .end();
}

/// Unmount one of `init`'s mounts at shutdown; each step is tried whatever the last did.
fn unmount_one(root_ns: u64, m: &Kept) {
    let at = m.mount_point.as_bytes();
    // SAFETY: a namespace handle init holds, and a valid path.
    let synced = unsafe { syscall3(SYS_NS_SYNC, root_ns, at.as_ptr() as u64, at.len() as u64) };
    if synced < 0 {
        let mut l = Line::new();
        l.s(b"init: ").s(at).s(b": its files could not all be written back (").i(synced).s(b")");
        l.end();
    }
    // SAFETY: IPC_MSG is init's own buffer, and nothing else uses it during a shutdown; the
    // request is an envelope and no handles.
    let sent = unsafe {
        #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
        let msg: &mut [u8; IPC_MSG_LEN] = &mut *(&raw mut IPC_MSG);
        let payload = &mut msg[24..];
        match init::unmount::request(payload, 1) {
            Some(n) => {
                IPC_MSG[4..8].copy_from_slice(&(n as u32).to_le_bytes());
                IPC_MSG[8] = 0;
                syscall5(
                    SYS_CHANNEL_SEND,
                    m.control,
                    (&raw const IPC_MSG) as u64,
                    (&raw const IPC_HANDLES) as u64,
                    0,
                    SENDMODE_NOBLOCK,
                ) == 0
            }
            None => false,
        }
    };
    let answered = sent && wait_one_until(m.control, now_ns().saturating_add(UNMOUNT_TIMEOUT_NS));
    // SAFETY: IPC_MSG/IPC_HANDLES/IPC_COUNT are valid writable out-params.
    let received = answered
        && unsafe {
            syscall4(
                SYS_CHANNEL_RECV,
                m.control,
                (&raw mut IPC_MSG) as u64,
                (&raw mut IPC_HANDLES) as u64,
                (&raw mut IPC_COUNT) as u64,
            )
        } == 0;
    let mut line = Line::new();
    line.s(b"init: ").s(at);
    if received {
        // SAFETY: the kernel wrote the message; its payload is bounded by the header's length.
        let payload = unsafe {
            let len = u32::from_le_bytes([IPC_MSG[4], IPC_MSG[5], IPC_MSG[6], IPC_MSG[7]]) as usize;
            #[allow(clippy::deref_addrof, reason = "a `static mut` is reached only through a raw pointer")]
            let msg: &[u8; IPC_MSG_LEN] = &*(&raw const IPC_MSG);
            &msg[24..24 + len.min(IPC_MSG_LEN - 24)]
        };
        use init::unmount::Answer;
        match init::unmount::answer(payload) {
            Answer::Done => line.s(b" unmounted, and its server left the filesystem clean"),
            Answer::Failed(why) => line.s(b" unmounted, but not left clean: ").untrusted(why),
            Answer::Unexpected => line.s(b" answered its unmount with something else"),
        };
        // SAFETY: handles the kernel installed, which no answer carries.
        unsafe {
            #[allow(clippy::needless_range_loop, reason = "an iterator would borrow a `static mut`")]
            for k in 0..(&raw const IPC_COUNT).read().min(init::ready::IPC_HANDLE_MAX) {
                syscall1(SYS_HANDLE_CLOSE, (&raw const IPC_HANDLES[k]).read());
            }
        }
    } else if sent {
        line.s(b" did not answer its unmount, and may not be left clean");
    } else {
        line.s(b": its server could not be told to unmount");
    }
    line.end();
    // SAFETY: closing init's own control channel; its server has exited or been abandoned.
    unsafe { syscall1(SYS_HANDLE_CLOSE, m.control) };
}

fn now_ns() -> u64 {
    let mut now: u64 = 0;
    // SAFETY: `&now` is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    now
}

fn wait_one_until(h: u64, deadline: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter, with a deadline. The reaping
    // loop that also uses them is this call's caller, and is not waiting.
    unsafe {
        WAIT_HANDLES[0] = h;
        let (handles, results) = ((&raw const WAIT_HANDLES) as u64, (&raw mut WAIT_RESULTS) as u64);
        syscall4(SYS_WAIT, handles, 1, results, deadline) == 1
    }
}

/// **Keep the system-control object**, the capability to stop the machine, if `rdx` holds it: a
/// `SystemControl` handle with `WRITE`. A boot without it goes on, and says so: only a shutdown's
/// last step needs it.
fn keep_system_control(h: u64) {
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `info` is a writable 24-byte `HandleInfo`, the layout the kernel writes.
    let stat = unsafe { syscall2(SYS_HANDLE_STAT, h, (&raw mut info) as u64) };
    let kind = stat == 0 && info.object_type == KOBJ_SYSTEM_CONTROL;
    if h != 0 && kind && info.rights & RIGHT_WRITE != 0 {
        // SAFETY: single-threaded init; written once, here, before anything reads it.
        unsafe { SYSTEM_CONTROL = h };
        kprint(b"init: holds the system-control object\n");
    } else {
        kprint(b"init: no system-control object -- a shutdown cannot stop the machine\n");
    }
}

/// Registers: `rdi` = notification channel, `rsi` = root namespace, `rdx` = system-control object.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, system_control: u64, _arg0: u64) -> ! {
    kprint(b"init: up (pid 1)\n");
    keep_system_control(system_control);
    let count = (notif != 0) as u64 + (root_ns != 0) as u64 + (system_control != 0) as u64;
    Line::new()
        .s(b"init: received ")
        .u(count)
        .s(b" handles (notif=")
        .u(notif)
        .s(b", ns=")
        .u(root_ns)
        .s(b", system-control=")
        .u(system_control)
        .s(b")")
        .end();

    let booted = match read_manifest(root_ns) {
        Some(manifest) => mount_all(root_ns, &manifest),
        None => {
            kprint(b"init: no usable boot manifest\n");
            false
        }
    };
    if !booted {
        emergency(notif, root_ns);
    }

    read_current_generation(root_ns);

    if !bind_profile_server(root_ns) {
        emergency(notif, root_ns);
    }

    // **The servers are `service-mgr`'s to start**, as declarations; a `critical` one that does not
    // come up makes it ask over the terminal channel for the emergency shell, which `reap_loop`
    // starts. The test image's self-test, terminal and test clients are declarations too, which
    // `check-display` and `check-terminal` need without a login, so this file is identical in both
    // images; `nxterm` precedes `ui-testclient`, which raises its windows over the terminal's.
    supervise(notif, root_ns);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"init: PANIC\n");
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
