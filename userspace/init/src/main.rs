//! `init` — PID 1 (bootstrapping form, Phase 2 slice 4 Part 5).
//!
//! The kernel loads init as the first userspace process (`run_first_userspace`),
//! handing it a notification channel (`rdi`) and a full-rights root namespace
//! (`rsi`) carrying the boot kernel-server bindings (`/initramfs`, `/dev/entropy`,
//! `/proc/self/*`). init:
//!
//! 1. reports the handle set it received;
//! 2. reads + parses `/initramfs/etc/init.toml` and **processes its mounts** in
//!    dependency order — for each, resolving the device, spawning an
//!    `fs-server-ext4`, handing it the device, awaiting `Meta::Ready`, and
//!    `sys_ns_bind`ing its forwarding endpoint at the mount point (the Resource
//!    Server Startup Protocol); then reads `/system/current-generation` through the
//!    freshly-mounted root (the slice-7 milestone — the whole stack end to end);
//! 3. binds the profile server at `/bin`;
//! 4. spawns `service-mgr`, handing it a root handle with init's own rights and the root
//!    filesystem's and profile server's endpoints — **`service-mgr` starts the system's servers**
//!    since administration Part E.1a, which init did before;
//! 5. enters the reaping loop, which also answers the terminal channel to `service-mgr`.
//!
//! Per `userspace/init/CLAUDE.md`, init uses `libkern` + `alloc` only and never
//! `panic!`s in normal operation.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::arch::asm;
use init::manifest::{self, BindSpec, Manifest, Mode, MountSpec};
use libkern::debug::Line;
use libkern::*;
use libos::{Handle, MapRead, Memory, Namespace, NsReadOnly, block_on};

// The freeing userspace heap (slice 4). Replaces init's former fixed bump arena,
// which never freed — fine for init's one-shot bootstrap, but init is now the first
// consumer of the real allocator (`docs/architecture/libheap.md`).
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// One page; init.toml is assumed to fit (true for the bootstrapping manifest).
const PAGE: u64 = 4096;

/// Bounded wait for an fs-server's Ready (the CLAUDE.md mount timeout): init must
/// not wait forever for a server that never reports up.
const READY_TIMEOUT_NS: u64 = 30_000_000_000; // 30 s

/// The notification channel and the terminal channel: what [`reap_loop`] waits on. Other
/// callers use the first slot with a count of one.
static mut WAIT_HANDLES: [u64; 2] = [0; 2];
static mut WAIT_RESULTS: [u8; 48] = [0; 48];
static mut NOTIF: Notification = Notification::zeroed();

/// Control-channel endpoints for an fs-server handshake (init keeps `[0]`, the
/// server gets `[1]`). Reused across mounts (processed one at a time).
static mut CTRL0: u64 = 0;
static mut CTRL1: u64 = 0;

/// The root fs-server's forwarding endpoint, retained after the `/` mount so init
/// can hand it to service-mgr (→ session-mgr binds it as each login's `/home`
/// subtree, sharing the one registration — Part B.2). `0` until the root is mounted.
static mut FS_ENDPOINT: u64 = 0;
/// The profile server's forwarding endpoint, retained after the `/bin` bind so init can
/// hand it to service-mgr (→ session-mgr binds it as each login's `/bin`, sharing the one
/// registration exactly as `/home` shares the fs-server's). `0` until `/bin` is bound.
///
/// A session cannot reach the store any other way. A `UserspaceServer` binding resolves to
/// a kernel registration record, not to the endpoint, so a process holding a LOOKUP-only
/// root namespace can *use* `/bin` but can never obtain the thing needed to bind it
/// elsewhere. Retaining it here is what makes the projection delegable at all.
static mut PROFILE_ENDPOINT: u64 = 0;
/// **The terminal channel** to `service-mgr` (administration Part E.1): the handoff channel,
/// kept open once its three handoffs are sent. `service-mgr` asks for the emergency shell on it
/// when a critical server does not come up at boot, and its closing is how `init` learns that
/// `service-mgr` has gone. `0` while there is none.
static mut TERMINAL: u64 = 0;

/// The system-control object (administration Part E.3), or 0 without one: the handle
/// `sys_power` takes, which E.4's shutdown uses as its last step.
static mut SYSTEM_CONTROL: u64 = 0;
/// The size of an `IpcMsg`: a 24-byte header, then the payload.
const IPC_MSG_LEN: usize = 4096;
/// One IPC message + transferred-handle scratch for the setup send / Ready recv.
static mut IPC_MSG: [u8; IPC_MSG_LEN] = [0; IPC_MSG_LEN];
static mut IPC_HANDLES: [u64; init::ready::IPC_HANDLE_MAX] = [0; init::ready::IPC_HANDLE_MAX];
static mut IPC_COUNT: usize = 0;
/// Spawn args for an `fs-server-ext4`: one moved handle — the control channel — in
/// `handles[0]` (delivered to the child in `rdx`); it inherits a LOOKUP-only handle
/// to init's root namespace (it resolves nothing — it gets the device by IPC).
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
/// Spawn args for the system `profile-server` (slice: store + profiles): one moved
/// handle — the control channel — in `handles[0]` (delivered in `rdx`); it inherits a
/// LOOKUP-only handle to init's root namespace. Unlike an fs-server it gets **no**
/// device by IPC: it uses its inherited namespace to read its manifest from
/// `/initramfs/...` and to resolve packages under `/store/...`, then re-exports the
/// resolved store handle as the reply to a forwarded `/bin/...` resolve.
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
/// Spawn args for the interactive emergency shell `eshell` (slice 9): no handles,
/// inherit a LOOKUP-only handle to init's root namespace (so it resolves
/// `/dev/console` for input and `/dev/blk/*` for `lsblk`). It runs as the
/// persistent interactive console.
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
/// Spawn args for the service manager (the normal handoff). It inherits a LOOKUP-only
/// handle to init's root namespace and holds `BIND_NAMESPACE` — its defining
/// supervisor capability (registering service endpoints, re-delegating to
/// session-mgr). See `docs/architecture/service-manager.md` § Capability posture. The
/// bind-righted root it binds the servers with — the second gate — is sent down the
/// handoff channel rather than inherited (administration Part E.1a).
/// `handles[0]` is that **handoff channel** end, moved to service-mgr, over which init sends
/// the root handle, then the fs-server and profile-server forwarding endpoints, in that
/// order. It carries `TRANSFER` so those endpoints can be handed onward, and
/// `SEND`/`RECV`/`WAIT` so the channel itself works; init keeps its own end as the
/// **terminal channel**. Spawned in **both** boots.
static mut SPAWN_SERVICE_MGR: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /bin/service-mgr
    handle_count: 1,
    move_mask: 1, // move handle 0 (the handoff channel) to service-mgr
    arg0: 0,
    handles: [0; 4], // handles[0] = the handoff channel end, set at spawn
    rights: [
        RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT | RIGHT_TRANSFER | RIGHT_DUPLICATE,
        0,
        0,
        0,
    ],
    namespace: 0,
    syscaps: SYSCAP_BIND_NAMESPACE,
};

/// Resolve `path` in namespace `ns` requesting `rights`, wait the PO, and return
/// `(status, resolved_handle)` (`IoResult`: status at bytes 8..12, handle 16..24).
fn ns_lookup_wait(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights)
    };
    if po < 0 {
        return (po as i32, 0);
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po as u64;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    let status = unsafe {
        i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]])
    };
    let resolved = unsafe {
        u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ])
    };
    // SAFETY: closing our own PO handle (the resolved handle is separate).
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    if waited != 1 {
        return (-1, 0);
    }
    (status, resolved)
}

/// Resolve a program `path` to its ELF `MemoryObject` (via the namespace, MAP_READ),
/// stamp the handle into `args.image`, spawn, and close init's handle to the image
/// (the kernel copies the ELF during spawn). Returns the child process handle, or a
/// negative error (`-1` if the image can't be resolved). This is the path-based spawn
/// that replaced the kernel-embedded `ImageId` selector.
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
    let h = unsafe { syscall1(SYS_PROCESS_SPAWN, args as u64) };
    // SAFETY: closing our own handle to the image object (the child has its own copy).
    unsafe { syscall1(SYS_HANDLE_CLOSE, img) };
    h
}

/// Read + parse `/initramfs/etc/init.toml`, log the topo-sorted mount plan, and
/// return the mounts (shallowest-first) for [`mount_all`] to process. `None` on any
/// failure (missing / unmappable / malformed manifest) — init would drop to the
/// emergency shell (slice 9); for now it logs and skips mounting.
fn read_manifest(root_ns: u64) -> Option<Manifest> {
    let (st, mem) = ns_lookup_wait(root_ns, b"/initramfs/etc/init.toml", RIGHT_MAP_READ);
    if st != 0 || mem == 0 {
        kprint(b"init: /initramfs/etc/init.toml not found (would drop to eshell)\n");
        return None;
    }
    // Map the read-only MemoryObject the initramfs server handed back. init.toml
    // is text and fits in one page; the server zero-fills the tail, so we trim
    // trailing NULs to recover the exact file content.
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

/// Process the manifest: every mount in order (shallowest first), then every bind over them.
/// Returns `true` iff all succeeded. A failure is critical-path — the mounts are all
/// `required_for = boot`, and a bind that silently did not happen is how a later lookup fails
/// for a reason nobody can see — and routes init to the emergency shell.
///
/// **Every mount's forwarding endpoint is held until the binds are done**, since a bind's
/// source may be any mount. Then the root's is kept (handed on to `service-mgr`, which gives
/// it to `session-mgr` for each login's `/home`) and the rest are closed — each binding took
/// its own reference.
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

/// Bind one `[[bind]]`: the source mount's forwarding endpoint again at `b.path`, scoped to
/// `b.subtree`. The kernel shares the source's server registration across both names rather
/// than minting a rival that would take its replies — the same thing `session-mgr` does for
/// each login's `/home`. Returns `true` on success.
fn bind_one(root_ns: u64, b: &BindSpec, endpoints: &[(&str, u64)]) -> bool {
    // The parser checked the source names a mount; it is absent here only if that mount failed,
    // which `mount_all` has already reported.
    let Some(&(_, endpoint)) = endpoints.iter().find(|(point, _)| *point == b.source) else {
        return false;
    };
    // `/` scopes to the whole tree, which the kernel spells as no base at all.
    let base: &[u8] = if b.subtree == "/" { b"" } else { b.subtree.as_bytes() };
    // SAFETY: valid namespace handle, path/base pointers and lengths, and an endpoint handle
    // init holds.
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

/// Mount one `[[mount]]`: the Resource Server Startup Protocol from init's side.
/// Returns the server's forwarding endpoint on success (the fs-server is bound at
/// `m.mount_point`); the caller owns it.
fn mount_one(root_ns: u64, m: &MountSpec) -> Option<u64> {
    // Only `fs-server-ext4` exists in slice 7.
    if m.fs_server != "fs-server-ext4" {
        Line::new().s(b"init: unknown fs_server '").s(m.fs_server.as_bytes()).s(b"'").end();
        return None;
    }
    // 1. Resolve the block-device handle: READ (for the server's `sys_io_submit`)
    //    + TRANSFER (to hand it to the server).
    let dev_path = match manifest::device_ns_path(&m.device) {
        Some(p) => p,
        None => {
            Line::new()
                .s(b"init: unsupported device scheme '")
                .s(m.device.as_bytes())
                .s(b"'")
                .end();
            return None;
        }
    };
    // READ+WRITE (the RW fs-server writes filesystem metadata) + TRANSFER (hand it to the
    // server) + DUPLICATE (the server hands a copy to the kernel for the Model A data path).
    let (st, device) = ns_lookup_wait(
        root_ns,
        dev_path.as_bytes(),
        RIGHT_READ | RIGHT_WRITE | RIGHT_TRANSFER | RIGHT_DUPLICATE,
    );
    if st != 0 || device == 0 {
        Line::new().s(b"init: device ").s(dev_path.as_bytes()).s(b" not found").end();
        return None;
    }

    // 2. Create the control channel (init keeps end 0, the server gets end 1).
    // SAFETY: CTRL0/CTRL1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0) };
    if cr != 0 {
        unsafe { syscall1(SYS_HANDLE_CLOSE, device) };
        return None;
    }
    // SAFETY: `sys_channel_create` just wrote both endpoints; init is single-threaded.
    let (ctrl_init, ctrl_srv) = unsafe { ((&raw const CTRL0).read(), (&raw const CTRL1).read()) };

    // 3. Spawn the fs-server, moving the control endpoint into it (delivered in rdx).
    // SAFETY: SPAWN_FS is a valid writable arg block; spawn_program resolves the ELF
    // image from the initramfs, stamps it, spawns, and closes the image handle.
    let fs_h = unsafe {
        SPAWN_FS.handles[0] = ctrl_srv;
        spawn_program(root_ns, b"/initramfs/sbin/fs-server-ext4", &raw mut SPAWN_FS)
    };
    if fs_h < 0 {
        kprint(b"init: fs-server spawn FAIL\n");
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, device);
            syscall1(SYS_HANDLE_CLOSE, ctrl_init);
        }
        return None;
    }

    // 4. Setup message: transfer the device handle to the server, with one flags byte — read-only
    //    for a `"ro"` mount (administration Part C.3). NoBlock — the control ring is empty.
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
        // The device handle was not moved (send failed) — close it + the rest.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, device);
            syscall1(SYS_HANDLE_CLOSE, ctrl_init);
        }
        return None;
    }
    // The device handle has moved to the server; init no longer owns it.

    // 5. Await Meta::Ready (bounded), then take the forwarding endpoint it carries.
    let who: [&[u8]; 4] = [b"fs-server-ext4 for ", m.mount_point.as_bytes(), b" on ", m.device.as_bytes()];
    let endpoint = match wait_ready(ctrl_init, &who) {
        Some(e) => e,
        None => {
            unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
            return None;
        }
    };
    // The handshake is done; the control channel is no longer needed.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };

    // 6. Bind the forwarding endpoint at the mount point. The kernel sees an
    //    IpcChannel and adopts it as a Userspace Server (slice-7 forwarding). The
    //    binding takes its own reference; the endpoint goes back to `mount_all`, which
    //    may bind it again for a `[[bind]]` before closing it or handing it on.
    // SAFETY: valid namespace handle + path pointer + endpoint handle.
    let br = unsafe {
        syscall4(
            SYS_NS_BIND,
            root_ns,
            m.mount_point.as_ptr() as u64,
            m.mount_point.len() as u64,
            endpoint,
        )
    };
    if br != 0 {
        Line::new().s(b"init: bind FAIL at ").s(m.mount_point.as_bytes()).end();
        // SAFETY: closing our own handle; nothing was bound with it.
        unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
        return None;
    }

    Line::new().s(b"init: mounted fs-server-ext4 at ").s(m.mount_point.as_bytes()).end();
    // init keeps `fs_h` (the long-lived server's process handle).
    let _ = fs_h;
    Some(endpoint)
}

/// Wait (bounded) for a resource server's `Meta::Ready` on `ctrl` and return the endpoint it
/// transfers (`handles[0]`). `None` if there is none — and then **init has already said why**,
/// naming the server as the `who` pieces spell it.
///
/// The message is parsed by [`init::ready`] (hand-parsed, host-tested). The four ways a
/// handshake fails are four different problems on a machine nobody can attach a debugger to, and
/// until Phase 5 every caller printed the same `Ready timeout/invalid` for all of them: the server
/// took too long, it exited first, it sent something else, or it **refused** — a server that
/// cannot serve what it was given says so in place of the Ready, and its reason is printed here.
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

    // Absolute deadline = now + READY_TIMEOUT_NS (monotonic clock).
    let mut now: u64 = 0;
    // SAFETY: `&now` is a valid writable u64 out-param.
    unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut now) as u64) };
    let deadline = now.saturating_add(READY_TIMEOUT_NS);

    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter, with deadline.
    let waited = unsafe {
        WAIT_HANDLES[0] = ctrl;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            deadline,
        )
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
        // A reason crossed the wire, so it is printed as one; `Line` marks it if it is cut.
        init::ready::First::Refused(reason) if !reason.is_empty() => {
            say(b" refused: ").untrusted(reason).end()
        }
        init::ready::First::Refused(_) => say(b" refused, giving no reason init could read").end(),
        init::ready::First::Unexpected => say(b" sent something other than a Ready").end(),
    }
    None
}

/// Spawn the system profile server and bind its forwarding endpoint at `/bin`. This is
/// the Resource Server Startup Protocol from init's side (mirrors [`mount_one`]) minus
/// the device handoff: the profile server needs no device — it resolves its manifest
/// and the store through the LOOKUP-only root namespace it inherits, and answers
/// forwarded `/bin/<prog>` resolves by re-exporting the matching `/store/.../bin/<prog>`
/// handle. Returns `true` once bound at `/bin`. A failure is critical-path: without
/// `/bin`, no program resolves for the services init is about to launch.
fn bind_profile_server(root_ns: u64) -> bool {
    // 1. Create the control channel (init keeps end 0, the server gets end 1).
    // SAFETY: CTRL0/CTRL1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0)
    };
    if cr != 0 {
        return false;
    }
    // SAFETY: `sys_channel_create` just wrote both endpoints; init is single-threaded.
    let (ctrl_init, ctrl_srv) = unsafe { ((&raw const CTRL0).read(), (&raw const CTRL1).read()) };

    // 2. Spawn the profile server, moving the control endpoint into it (in rdx). No
    //    setup message follows — it uses its inherited namespace, not a handed device.
    // SAFETY: SPAWN_PROFILE is a valid writable arg block; spawn_program resolves the
    // ELF image from the initramfs, stamps it, spawns, and closes the image handle.
    let ps_h = unsafe {
        SPAWN_PROFILE.handles[0] = ctrl_srv;
        spawn_program(root_ns, b"/initramfs/sbin/profile-server", &raw mut SPAWN_PROFILE)
    };
    if ps_h < 0 {
        kprint(b"init: profile-server spawn FAIL\n");
        // SAFETY: closing our own control endpoint (ctrl_srv moved to the child).
        unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
        return false;
    }

    // 3. Await Meta::Ready (bounded), then take the forwarding endpoint it carries.
    let endpoint = match wait_ready(ctrl_init, &[b"profile-server".as_slice()]) {
        Some(e) => e,
        None => {
            // SAFETY: closing our own control endpoint.
            unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
            return false;
        }
    };
    // The handshake is done; the control channel is no longer needed.
    // SAFETY: closing our own control endpoint.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };

    // 4. Keep a second handle to the endpoint *before* binding, for service-mgr to carry
    //    down to session-mgr. Duplicating first rather than after means a failure here is
    //    a failure to bind at all, instead of a bound `/bin` that no session can ever be
    //    given — the second being much harder to notice.
    //
    //    `TRANSFER | DUPLICATE` are the rights the hand-down needs and all it needs: this
    //    copy is carried and re-bound, never sent on.
    // SAFETY: duplicating our own endpoint handle with attenuated rights.
    let retained = unsafe {
        syscall2(SYS_HANDLE_DUPLICATE, endpoint, RIGHT_TRANSFER | RIGHT_DUPLICATE)
    };
    if retained < 0 {
        kprint(b"init: profile endpoint duplicate FAIL\n");
        // SAFETY: closing our own endpoint handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
        return false;
    }

    // 5. Bind the forwarding endpoint at `/bin`. The kernel adopts the IpcChannel as a
    //    Userspace Server; the binding takes its own reference, so init closes its
    //    endpoint handle after. The retained duplicate keeps the *same* endpoint alive,
    //    so a later bind of it shares this registration rather than minting a rival.
    //
    // **Scoped to `/bin`, not whole-tree** (M14 Part H). The profile server projects more than
    // one of a package's directories now, and it tells them apart by the first component of the
    // suffix — so every bind of it carries a base naming the directory it projects. Unscoped,
    // `/bin/applications` would forward as a bare `applications` and reach the *applications*
    // projection, which is an alias nobody asked for.
    // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
    let br = unsafe {
        syscall6(
            SYS_NS_BIND,
            root_ns,
            b"/bin".as_ptr() as u64,
            4,
            endpoint,
            b"/bin".as_ptr() as u64,
            4,
        )
    };
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

/// The slice-7 milestone: look up `/system/current-generation` through the just-
/// mounted root fs-server (the kernel forwards the lookup, the server reads the
/// file and replies a `MemoryObject`), map it, and log its content — proving the
/// whole stack end to end.
fn read_current_generation(root_ns: u64) {
    // libos path (the init dogfood for slice 5): borrow the process-owned root
    // namespace, then `lookup(...).block_on()` + `map()` — replacing the hand-rolled
    // `ns_lookup_wait` (submit → sys_wait → byte-offset decode → close). The resolved
    // handle is an owning libos `Handle` that closes itself on drop, so the two manual
    // `sys_handle_close`s go away.
    // SAFETY: `root_ns` is init's live root namespace, owned for its whole run; a
    // borrowed Handle is a non-owning view and never closes it.
    let ns = unsafe { Handle::<Namespace, NsReadOnly>::borrow(RawHandle(root_ns), Rights::LOOKUP) };
    // SAFETY: the path resolves to a read-mappable file object (asserted by the
    // `Memory, MapRead` type arguments).
    let mem = match block_on(unsafe {
        ns.lookup::<Memory, MapRead>("/system/current-generation", Rights::MAP_READ)
    }) {
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
    // `mem` drops at end of scope → closes the resolved handle.
}

/// Spawn the demo `parent`, then reap exited children forever. As PID 1, init is
/// the eventual parent of every orphan; here its only child is `parent`.
/// Spawn the interactive emergency shell as the persistent serial console (it runs
/// forever; init keeps no handle). Launched once the demo chain has exited, so the
/// shell has the disk and console to itself.
/// Integration-test build only: report the run's verdict to the `xtask test-qemu`
/// runner via `SYS_TEST_EXIT` — which, under the kernel's `test-harness` feature,
/// writes `isa-debug-exit` and terminates QEMU. `ok` selects PASS/FAIL. Modelled as
/// returning `()` rather than `!`: the syscall does not return in practice, but
/// letting callers fall through means a missing exit device degrades to a normal
/// boot instead of a hang. See `docs/conventions/qemu-integration-tests.md`.
///
/// **Unconditional, in every build** (retrofit Part C2). `SYS_TEST_EXIT` exists in `libkern`
/// always and is served only by a kernel built with its own `test-harness` feature; anywhere
/// else the syscall number is unknown and the call returns `Unsupported`. So a release init
/// makes one pointless syscall on a path that is already a boot failure, and gains a code
/// path identical to the tested one. `init` only ever fires **FAIL** — PASS is `boot-probe`'s.
fn test_exit(ok: bool) {
    let code = if ok { TEST_EXIT_SUCCESS } else { TEST_EXIT_FAILURE };
    kprint(if ok {
        b"init: test-harness verdict PASS\n"
    } else {
        b"init: test-harness verdict FAIL\n"
    });
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

/// Spawn the service manager — the normal boot handoff. Returns the process handle, or a negative
/// error. [`supervise`] closes the handle at once: `service-mgr`'s death, a critical fault init
/// must observe, is learned from the **terminal channel** closing (administration Part E.1a),
/// which names it exactly, where a `ChildExited` names only a pid.
///
/// **`handles[0]` is a handoff channel, not an endpoint.** It carried the fs-server
/// endpoint directly until a second endpoint (the profile server's) needed to go the same
/// way, and only `handles[0]` reaches a child — the kernel seeds `rdx` with it and there
/// is no register left for `handles[1]`, nor any documented way to learn its handle value.
/// Rather than invent one, this uses the mechanism the boot chain already uses one link
/// further down: service-mgr hands *its* children endpoints over a control channel. Adding
/// a third endpoint later is now one more `send_handle`, not another ABI question.
fn spawn_service_mgr(root_ns: u64) -> i64 {
    kprint(b"init: handing off to service manager\n");
    // **The terminal channel.** Depth 4: three handoffs, queued before `service-mgr` runs — a ring
    // shorter than the sends drops the last one silently — and then the ops it sends back.
    // SAFETY: CTRL0/CTRL1 are valid writable out-params (mounts are long done).
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0)
    };
    if cr != 0 {
        kprint(b"init: service-mgr handoff channel FAIL\n");
        // SAFETY: nothing was handed off; the endpoints are ours to close.
        unsafe { close_retained_endpoints() };
        return -1;
    }
    let (init_end, child_end) = unsafe { ((&raw const CTRL0).read(), (&raw const CTRL1).read()) };

    // SAFETY: single-threaded init; stamp the handoff end into the (moved) handle slot,
    // then spawn. `move_mask`/`handle_count`/`rights` are set in the static.
    let h = unsafe {
        SPAWN_SERVICE_MGR.handles[0] = child_end;
        spawn_program(root_ns, b"/bin/service-mgr", &raw mut SPAWN_SERVICE_MGR)
    };
    if h < 0 {
        kprint(b"init: service-mgr spawn FAIL\n");
        // Nothing moved (the spawn failed) — close both ends and the endpoints they
        // were about to carry, so a failed handoff leaks nothing.
        // SAFETY: closing our own handles.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, init_end);
            syscall1(SYS_HANDLE_CLOSE, child_end);
            close_retained_endpoints();
        }
        return h;
    }

    // The handoffs, in the order service-mgr receives them (administration Part E.1):
    //
    // 1. **The root, with init's own rights**, `BIND` and `UNBIND` among them. A spawned process
    //    only ever gets a lookup-only root, which is why every server was bound here until now;
    //    `service-mgr` sits in init's trust tier since E.1, the maintainer's call, and binds them.
    // 2. The root filesystem's endpoint, and 3. the profile server's, for the login chain.
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
        // Kept: the terminal channel, for the rest of the boot.
        TERMINAL = init_end;
    }
    h
}

/// Transfer one `handle` to a child over a handoff channel — an IPC message with a single
/// moved handle and no payload. On failure the handle did not move, so it is closed here:
/// a supervisor that drops a server endpoint on the floor keeps the server alive with
/// nothing able to reach it, which is worse than losing it outright.
///
/// A zero `handle` sends **an empty message**, not nothing. The receiver reads the
/// handoffs positionally, so skipping a send would shift every later one up a slot and
/// hand service-mgr the profile endpoint where it expects the fs-server's.
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
        // SAFETY: the transfer did not happen; reclaim the handle.
        if handle != 0 {
            unsafe { syscall1(SYS_HANDLE_CLOSE, handle) };
        }
    }
}

/// Close whichever server endpoints init is still holding for the handoff. Only reached
/// when the handoff cannot happen at all.
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


/// The healthy supervise path. **Normally**, hand off to the service manager: spawn
/// it and supervise it via [`reap_loop`] (if service-mgr exits — a critical fault —
/// reap_loop drops to the emergency console as the interim recovery, until a reboot
/// path exists; see `docs/architecture/service-manager.md` § Recovery). **Under
/// `selftest`**, bring up the login chain (service-mgr → auth-service + session-mgr) and
/// the Phase-1/2 demo chain (`parent`) **concurrently**, then supervise via [`reap_loop`].
/// Running them together is deliberate: `parent`'s direct `/dev/blk` reads overlap the
/// login chain's fs-mediated block I/O (session-mgr/nxsh's forwarded `/home` reads), so
/// the default test exercises concurrent direct + fs-mediated block I/O across all CPUs —
/// the scenario that originally surfaced the cross-CPU-wake hang (now fixed by the
/// reschedule IPI; see the 2026-07-20 decision log). The prior demo→login *sequencing* was
/// a workaround for that hang and is no longer needed. (This is a concurrency *smoke test*,
/// not a deterministic catch of that specific timing bug, which only reproduced under
/// sustained multi-second load.)
///
/// **This function's ordering is the argument that the boot verdict is airtight**, so it is
/// worth stating rather than leaving implicit. The demo chain runs **synchronously** and a
/// non-zero exit fails the run here; only then is the login chain handed off, and only then
/// does `service-mgr` start `boot-probe`, which runs the SMP and floating-point gates and
/// fires the single `SYS_TEST_EXIT(PASS)`. So everything the run adjudicates has already
/// happened when those gates run. That placement is why `fp_gate` was moved out of the demo
/// `parent` in the first place — it completed in 2 of 15 KVM runs there, because whoever owns
/// the verdict races the demo chain — and it survived the verdict moving out of `session-mgr`
/// (retrofit Part B) only because of the sequencing below.
fn supervise(notif: u64, root_ns: u64) -> ! {
    let service_mgr_h = spawn_service_mgr(root_ns);
    if service_mgr_h <= 0 {
        // No `service-mgr` is no server at all since E.1, `auth-service` and `logging-service`
        // among them — the pair this path caught when init started them. The console is free:
        // nothing that holds it has started.
        emergency(notif, root_ns);
    }
    // SAFETY: closing init's reference to the process; it runs independently. Its death is
    // learned from the terminal channel closing, not from a handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, service_mgr_h as u64) };
    reap_loop(notif, root_ns);
}

/// The **emergency** path: a critical-path boot failure (bad manifest, failed
/// mount). Drop straight to the interactive shell so the operator can inspect the
/// broken system (`cat /dev/log`, `mounts`, `lsblk`) — no demo chain, no milestones.
/// See `userspace/init/CLAUDE.md` § "Failure → eshell".
fn emergency(notif: u64, root_ns: u64) -> ! {
    kprint(b"init: critical-path failure -- dropping to emergency shell\n");
    // A critical-path boot failure is a failed test run. Outside `test-qemu` the verdict
    // device is absent, the syscall returns `Unsupported`, and the boot carries on to the
    // emergency shell below — which is what an operator wants on real hardware.
    test_exit(false);
    spawn_eshell(root_ns);
    reap_loop(notif, root_ns);
}

/// Reap exited children forever (init is the eventual parent of every orphan), and answer the
/// terminal channel.
///
/// **The terminal channel** (administration Part E.1) carries one request today:
/// `TERMINAL_OP_EMERGENCY`, sent when a critical server did not come up at boot, or the
/// declarations have lost their critical servers and nothing was started. The emergency shell is
/// started then, once; the console is still free, since the critical servers start before the
/// terminal server.
///
/// **Its closing is `service-mgr`'s death**, which is attributed exactly, as a control channel
/// closing is — `KIND_CHILD_EXITED` names a pid, and nothing maps a handle to one. That death is
/// **reported, and not answered with a restart**, which `init` did until E.1. Every server path, in
/// every session, now goes through `service-mgr`'s routes, so its death takes them all; a second
/// one would start a second copy of every server; and the emergency shell cannot take a console
/// the terminal server still holds. So the machine needs a restart.
fn reap_loop(notif: u64, root_ns: u64) -> ! {
    kprint(b"init: entering reaping loop\n");
    let mut shell_started = false;
    loop {
        // SAFETY: single-threaded init; the terminal channel is read only here.
        let terminal = unsafe { TERMINAL };
        let count = if terminal != 0 { 2 } else { 1 };
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers for two entries.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            WAIT_HANDLES[1] = terminal;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                count,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if waited < 1 {
            continue;
        }
        // Drain every queued notification this wake delivered.
        loop {
            // SAFETY: NOTIF is a valid 64-byte writable out-param.
            let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
            if r != 0 {
                break; // WouldBlock: drained
            }
            // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
            let (kind, body) =
                unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
            if kind == KIND_CHILD_EXITED {
                let cpid = u32::from_le_bytes([body[0], body[1], body[2], body[3]]);
                let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                Line::new()
                    .s(b"init: reaped pid=")
                    .u(cpid as u64)
                    .s(b" code=")
                    // `.i`, not `.u`: an exit code is signed, and `-1` widened through
                    // `as u64` prints 18446744073709551615 (PR #181 review, finding 8).
                    .i(code as i64)
                    .end();
            }
        }
        if terminal == 0 {
            continue;
        }
        // The terminal channel: requests, or its closing.
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
            // SAFETY: the kernel wrote the message; one payload byte at offset 24.
            let (len, op, handles) = unsafe {
                (
                    u32::from_le_bytes([IPC_MSG[4], IPC_MSG[5], IPC_MSG[6], IPC_MSG[7]]),
                    IPC_MSG[24],
                    (&raw const IPC_COUNT).read(),
                )
            };
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
        }
    }
}

/// **Keep the system-control object**, the capability to stop the machine (administration Part
/// E.3), if `rdx` holds it: a handle to a `SystemControl` with `WRITE`. The kernel makes one and
/// hands it to init alone, without the rights to give it away, so the process that stops the
/// machine is this one: a shutdown (administration Part E.4) ends with `sys_power` on it, once
/// everything else has stopped.
///
/// A boot without it goes on, and says so: nothing before the stop needs it, and a shutdown
/// would reach every step but the last.
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

/// Bootstrap registers: `rdi` = notification channel, `rsi` = root namespace
/// (full-rights, kernel-bound servers), `rdx` = the system-control object
/// (administration Part E.3), `rcx` unused (init takes no arg0 from the kernel).
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

    // Read the manifest and process its mounts (spawn fs-servers → Ready → bind). A
    // missing/invalid manifest or a failed required mount is a **critical-path
    // failure** → drop to the emergency shell (the operator inspects the broken
    // system). On success, prove the stack end to end (the slice-7/8 milestones) and
    // enter the normal supervise path.
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
    // The filesystem tests that used to run here — large-file read-through, overwrite,
    // grow, create — are `boot-probe`'s now (retrofit Part C). They exercise
    // `fs-server-ext4` through the namespace, which any program with the right bindings can
    // do, and a failing one there does not take PID 1 with it. They also *gate* the run now,
    // which they never did here: every failure path was a bare `return` after a `FAIL`
    // print, so a broken filesystem passed the boot.

    // Spawn the system profile server and bind it at `/bin` (per init CLAUDE.md step 4).
    // Critical-path: without `/bin`, no program resolves for the services init launches.
    if !bind_profile_server(root_ns) {
        emergency(notif, root_ns);
    }

    // **The servers are `service-mgr`'s to start** (administration Part E.1). `init` used to start
    // nine here — `auth-service`, `logging-service`, the terminal server, the clipboard, the view
    // broker, the device manager, the storage service, the input server and the compositor —
    // and bind each in the root namespace. They are declarations now, started by `service-mgr` in
    // that order, each `Meta::Ready` awaited before the next, and bound through its registry so a
    // restart reaches every binding. `init` starts only what it takes to reach `service-mgr`:
    // its mounts, the profile server at `/bin`, and `service-mgr` itself, which is what
    // `docs/architecture/service-manager.md` has said since Phase 3.
    //
    // Two of them were critical-path here: a boot without `auth-service` or `logging-service`
    // dropped to the emergency shell. They still are, as `critical` declarations: `service-mgr`
    // asks for the shell over the terminal channel, and `reap_loop` starts it.
    //
    // The display self-test, the GUI terminal and the two test clients used to be spawned
    // here under `selftest`. They are **service declarations** now (retrofit Part C2), started
    // by `service-mgr` from `/system/services.toml` on the root — which carries them only in a test
    // image, so this file is byte-identical in both. Their order is the file's order: `nxterm`
    // before `ui-testclient`, so that the terminal's window exists by the time `ui-testclient`
    // raises its reference windows over it. (Creation order was the stacking until
    // administration C.1 showed it to be a race; the raise is what stacks them now.)
    //
    // **And the real answer arrived (M7 Part F, 2026-08-25.)** The comment this file carried
    // from 2026-08-12 — *"Until Milestone 7 there is nothing to launch `nxterm` from"* — is
    // answered twice over: retrofit C2 gave the test image a declaration, and `desktop-shell`
    // now launches a terminal from the applications modal into a namespace it constructed, in
    // a **release** image, which is what `cargo xtask check-login` boots. The declarations
    // above stay, and are not a duplicate of that path: they put a terminal and the test
    // clients on screen *without a login*, which is what lets `check-display` and
    // `check-terminal` test the display arm without depending on authentication.
    supervise(notif, root_ns);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // init must not panic in normal operation (`userspace/init/CLAUDE.md`); this
    // is the last-ditch handler. Report and spin (no eshell handoff yet — slice 9+).
    kprint(b"init: PANIC\n");
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
