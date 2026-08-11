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
//! 3. spawns `parent` (the slice-1/2/3 demo chain: `parent` → `child`);
//! 4. enters the reaping loop, closing the process handle of each exited child.
//!
//! Per `userspace/init/CLAUDE.md`, init uses `libkern` + `alloc` only and never
//! `panic!`s in normal operation.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec::Vec;
use core::arch::asm;
use init::manifest::{self, Mode, MountSpec};
use libkern::*;
use libos::{Handle, MapRead, Memory, Namespace, NsReadOnly, block_on};

// The freeing userspace heap (slice 4). Replaces init's former fixed bump arena,
// which never freed — fine for init's one-shot bootstrap, but init is now the first
// consumer of the real allocator (`docs/architecture/libheap.md`).
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// One page; init.toml is assumed to fit (true for the bootstrapping manifest).
const PAGE: u64 = 4096;

/// The resource-server protocol magic (`"RSMG"`) and the `Meta::Ready` op, so init
/// can **hand-parse** the fs-server's Ready message without depending on
/// `librsproto` (forbidden in init — see `userspace/init/CLAUDE.md`). The rsproto
/// envelope sits in the `IpcMsg` payload (offset 24): magic @0, op @6.
const RS_MAGIC: u32 = 0x5253_4D47;
const RS_OP_READY: u16 = 0x0004;
/// Bounded wait for an fs-server's Ready (the CLAUDE.md mount timeout): init must
/// not wait forever for a server that never reports up.
const READY_TIMEOUT_NS: u64 = 30_000_000_000; // 30 s

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
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
/// The tty server's **forwarding** endpoint, retained after the `/dev/tty` bind so init can
/// hand it to service-mgr (→ session-mgr binds it into each session, sharing the one
/// registration exactly as `/home` and `/bin` do).
///
/// A session must bind *this* — the channel the kernel forwards resolves down — and not a
/// tty channel minted from it. Both are `IpcChannel`s and the kernel adopts any bound
/// channel as a server, so binding a client channel silently produces a namespace entry
/// that answers `Namespace::Resolve` with `Unsupported`.
static mut TTY_ENDPOINT: u64 = 0;
/// One IPC message + transferred-handle scratch for the setup send / Ready recv.
static mut IPC_MSG: [u8; 4096] = [0; 4096];
static mut IPC_HANDLES: [u64; 8] = [0; 8];
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
/// Spawn args for the system `logging-service` (slice: logging): one moved handle — the
/// control channel — in `handles[0]` (delivered in `rdx`). It resolves nothing (clients
/// bring their own log endpoint), so its inherited LOOKUP-only namespace is unused; it
/// answers forwarded `/log/...` resolves by minting per-principal log channels.
/// Spawn args for the `tty-server`: one moved handle — the control channel — in
/// `handles[0]`. It resolves `/dev/console` through its inherited LOOKUP-only root
/// namespace and holds it exclusively thereafter.
static mut SPAWN_TTY: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /bin/tty-server
    handle_count: 1,
    move_mask: 1,
    arg0: 0,
    handles: [0; 4],
    rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_TRANSFER | RIGHT_WAIT, 0, 0, 0],
    namespace: 0,
    syscaps: 0, // a resource server holds no ambient capabilities
};

static mut SPAWN_LOGGING: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/logging-service
    handle_count: 1,
    move_mask: 1, // move handle 0 (the control endpoint) to the child
    arg0: 0,
    handles: [0; 4],
    rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_TRANSFER | RIGHT_WAIT, 0, 0, 0],
    namespace: 0,
    syscaps: 0, // a resource server holds no ambient capabilities
};
/// Spawn args for the integration test harness (`/initramfs/sbin/test-harness`): no
/// handles, inherit a LOOKUP-only handle to init's root namespace (so it resolves the
/// kernel servers). It constructs fresh namespaces in its `ns`/`forward` checks, so init
/// grants it `BIND_NAMESPACE`. Selftest builds only.
#[cfg(feature = "selftest")]
static mut SPAWN_HARNESS: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/test-harness
    handle_count: 0,
    move_mask: 0,
    arg0: 0,
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0,
    syscaps: SYSCAP_BIND_NAMESPACE,
};
/// Spawn args for `input-testclient` (display arm M3 Part A): no handles, inheriting a
/// LOOKUP-only handle to init's root namespace so it resolves `/dev/input/raw/*`. **No
/// syscaps** — authority over an input device is the namespace binding, nothing more, and
/// that binding is the whole of the keylogging boundary.
#[cfg(feature = "selftest")]
static mut SPAWN_INPUTCLIENT: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/input-testclient
    handle_count: 0,
    move_mask: 0,
    arg0: 0,
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0,
    syscaps: 0,
};

/// Spawn `input-testclient`, which proves the i8042 actually delivers events.
///
/// Spawn-and-forget like the UI client: it exits on its own once the harness has injected,
/// and init cannot usefully wait for a program whose completion depends on a host action.
#[cfg(feature = "selftest")]
fn run_input_testclient(root_ns: u64) {
    // SAFETY: SPAWN_INPUTCLIENT is a valid writable arg block.
    let h = unsafe {
        spawn_program(root_ns, b"/initramfs/sbin/input-testclient", &raw mut SPAWN_INPUTCLIENT)
    };
    if h < 0 {
        kprint(b"init: input-testclient spawn FAIL\n");
        return;
    }
    // SAFETY: closing init's reference; `reap_loop` reaps it when it exits.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
}

/// Spawn args for `ui-testclient` (display arm M2 Part D): no handles, inheriting a
/// LOOKUP-only handle to init's root namespace so it resolves `/dev/draw/new`. **No
/// syscaps** — authority over the display is the namespace binding, nothing more.
#[cfg(feature = "selftest")]
static mut SPAWN_UICLIENT: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/ui-testclient
    handle_count: 0,
    move_mask: 0,
    arg0: 0,
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0,
    syscaps: 0,
};
/// Spawn args for `display-selftest` (display arm M1 Part C): no handles, and it
/// inherits a LOOKUP-only handle to init's root namespace so it resolves
/// `/dev/framebuffer` and `/dev/framebuffer/info`. **No syscaps** — it binds nothing;
/// authority over the display is the namespace binding itself. Selftest builds only.
#[cfg(feature = "selftest")]
static mut SPAWN_DISPLAY: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/display-selftest
    handle_count: 0,
    move_mask: 0,
    arg0: 0,
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0,
    syscaps: 0,
};
/// Spawn args for the `input-server` (display arm M3 Part B): one moved handle — the
/// control channel — and a LOOKUP-only namespace handle through which it resolves
/// `/dev/input/raw/*`. **No syscaps**: like every resource server, it does not hold
/// `BIND_NAMESPACE`; init binds its endpoint on its behalf.
///
/// **It is the only process that should ever resolve the raw nodes.** They are bound in the
/// root namespace and nowhere else, and no session namespace projects them — reading one
/// unfiltered is a keylogger, and the binding is the whole of that boundary
/// (`docs/design/input-subsystem.md` §5).
static mut SPAWN_INPUT_SERVER: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/input-server
    handle_count: 1,
    move_mask: 1, // move handle 0 (the control endpoint) to the child
    arg0: 0,
    handles: [0; 4],
    // `TRANSFER` is the one that is easy to omit and fails late: `Meta::Ready` carries the
    // forwarding endpoint as a handle transfer, so without it the server comes up, opens
    // both devices, and only then cannot announce itself.
    rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_TRANSFER | RIGHT_WAIT, 0, 0, 0],
    namespace: 0,
    syscaps: 0, // a resource server holds no ambient capabilities
};

/// Spawn args for the `compositor` (display arm M2 Part B): no handles, and it inherits a
/// LOOKUP-only handle to init's root namespace so it resolves `/dev/framebuffer`.
/// **No syscaps** — it binds nothing; init does the binding, as for every other resource
/// server (`docs/rationale/why-supervisor-registration.md`).
static mut SPAWN_COMPOSITOR: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/compositor
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
/// session-mgr). See `docs/architecture/service-manager.md` § Capability posture. In
/// slice A it supervises a leaf service and binds nothing yet; the bind-righted
/// namespace handle (the second gate) and the `LOAD_MODULE`/`SYSTEM_CLOCK`
/// pass-through caps arrive with the RS protocol + those services (slice B onward).
/// `handles[0]` is a **handoff channel** end, moved to service-mgr, over which init sends
/// the fs-server and profile-server forwarding endpoints (in that order) for service-mgr
/// to carry down to session-mgr. It carries `TRANSFER` so those endpoints can be handed
/// onward, and `SEND`/`RECV`/`WAIT` so the channel itself works. Spawned in **both**
/// boots now (the selftest boot brings the login chain up after the demo chain reaps so
/// it is exercised under `test-qemu`).
static mut SPAWN_SERVICE_MGR: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/service-mgr
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
        kprint(b"init: image not found: ");
        kprint(path);
        kprint(b"\n");
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
fn read_manifest(root_ns: u64) -> Option<Vec<MountSpec>> {
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
            Ok(mounts) => {
                kprint(b"init: init.toml OK, ");
                kprint_u64(mounts.len() as u64);
                kprint(b" mount(s) (shallowest first):\n");
                for m in &mounts {
                    kprint(b"init:   ");
                    kprint(m.mount_point.as_bytes());
                    kprint(b": ");
                    kprint(m.fs_server.as_bytes());
                    kprint(b" on ");
                    kprint(m.device.as_bytes());
                    kprint(b" (");
                    kprint(match m.mode {
                        Mode::Ro => b"ro" as &[u8],
                        Mode::Rw => b"rw",
                    });
                    kprint(b")\n");
                }
                Some(mounts)
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

/// Process the manifest's mounts in order (shallowest-first): for each, resolve
/// the device, spawn an `fs-server-ext4`, hand it the device, await Ready, and bind
/// its endpoint at the mount point. A failed mount is logged and skipped (the
/// eshell handoff is slice 9).
/// Mount every manifest entry; returns `true` iff all succeeded. A failure is
/// critical-path (the entries are all `required_for = boot`) and routes init to the
/// emergency shell.
fn mount_all(root_ns: u64, mounts: &[MountSpec]) -> bool {
    let mut ok = true;
    for m in mounts {
        if !mount_one(root_ns, m) {
            kprint(b"init: mount FAILED for ");
            kprint(m.mount_point.as_bytes());
            kprint(b"\n");
            ok = false;
        }
    }
    ok
}

/// Mount one `[[mount]]`: the Resource Server Startup Protocol from init's side.
/// Returns `true` on success (the fs-server is bound at `m.mount_point`).
fn mount_one(root_ns: u64, m: &MountSpec) -> bool {
    // Only `fs-server-ext4` exists in slice 7.
    if m.fs_server != "fs-server-ext4" {
        kprint(b"init: unknown fs_server '");
        kprint(m.fs_server.as_bytes());
        kprint(b"'\n");
        return false;
    }
    // 1. Resolve the block-device handle: READ (for the server's `sys_io_submit`)
    //    + TRANSFER (to hand it to the server).
    let dev_path = match manifest::device_ns_path(&m.device) {
        Some(p) => p,
        None => {
            kprint(b"init: unsupported device scheme '");
            kprint(m.device.as_bytes());
            kprint(b"'\n");
            return false;
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
        kprint(b"init: device ");
        kprint(dev_path.as_bytes());
        kprint(b" not found\n");
        return false;
    }

    // 2. Create the control channel (init keeps end 0, the server gets end 1).
    // SAFETY: CTRL0/CTRL1 are valid writable out-params.
    let cr = unsafe { syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0) };
    if cr != 0 {
        unsafe { syscall1(SYS_HANDLE_CLOSE, device) };
        return false;
    }
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
        return false;
    }

    // 4. Setup message: transfer the device handle to the server (an empty payload;
    //    the server just takes handles[0]). NoBlock — the control ring is empty.
    // SAFETY: IPC_MSG/IPC_HANDLES are valid buffers; transferring one handle.
    let sr = unsafe {
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
        return false;
    }
    // The device handle has moved to the server; init no longer owns it.

    // 5. Await Meta::Ready (bounded), then take the forwarding endpoint it carries.
    let endpoint = match wait_ready(ctrl_init) {
        Some(e) => e,
        None => {
            kprint(b"init: fs-server Ready timeout/invalid\n");
            unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
            return false;
        }
    };
    // The handshake is done; the control channel is no longer needed.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };

    // 6. Bind the forwarding endpoint at the mount point. The kernel sees an
    //    IpcChannel and adopts it as a Userspace Server (slice-7 forwarding). The
    //    binding takes its own reference, so init closes its endpoint handle after.
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
    // auth+session Part B smoke test (selftest): bind the *same* fs endpoint a second
    // time as a **subtree** scoped to `/system` at `/subtreetest`, so a later lookup of
    // `/subtreetest/current-generation` forwards `system/current-generation` to the
    // server. This shares the server's registration (bind-mount semantics) — the kernel
    // reuses it rather than minting a rival that would hijack replies. `sys_ns_bind`
    // holds its own reference, so `endpoint` stays valid for the close below. Root mount
    // only (it owns `/system`).
    #[cfg(feature = "selftest")]
    if m.mount_point.as_bytes() == b"/" {
        let sub = b"/subtreetest";
        let base = b"/system";
        // SAFETY: valid namespace handle, path/base pointers, and endpoint handle.
        let r = unsafe {
            syscall6(
                SYS_NS_BIND,
                root_ns,
                sub.as_ptr() as u64,
                sub.len() as u64,
                endpoint,
                base.as_ptr() as u64,
                base.len() as u64,
            )
        };
        if r != 0 {
            kprint(b"init: subtree test bind FAIL\n");
        }
        // A **second writable mount**, for the cross-mount half of `move`
        // (the `cross-mount-move` deferral, closed 2026-07-30). Same endpoint again, scoped
        // to base `/scratch`, so
        // the kernel's rename test — same server *and* same subtree base — calls
        // `/system/x → /scratch/y` cross-filesystem while both sides remain writable.
        //
        // Two bindings of one server rather than a second filesystem: the kernel already
        // shares one registration across many names (bind-mount semantics, as
        // `/subtreetest` above relies on), and what `move`'s fallback needs is a
        // destination the kernel *classifies* as another mount, which this is. A real
        // second ext4 would add an image partition and a second server process without
        // exercising one further line of the path under test.
        //
        // Selftest-only: it is a fixture, not a system mount. The backing `/scratch`
        // directory is staged into the image unconditionally (harmless when empty).
        let scratch = b"/scratch";
        // SAFETY: valid namespace handle, path/base pointers, and endpoint handle.
        let r = unsafe {
            syscall6(
                SYS_NS_BIND,
                root_ns,
                scratch.as_ptr() as u64,
                scratch.len() as u64,
                endpoint,
                scratch.as_ptr() as u64,
                scratch.len() as u64,
            )
        };
        if r != 0 {
            kprint(b"init: scratch mount bind FAIL\n");
        }
    }
    // The root fs-server's forwarding endpoint is handed down to service-mgr (→
    // session-mgr, which binds it as each login's `/home` subtree — bind-mount
    // sharing, Part B.2). `sys_ns_bind` cloned its own reference above, so keeping
    // this handle open is fine; stash it (transfer ownership to the global) instead
    // of closing. Non-root mounts have no consumer yet → close as before.
    if m.mount_point.as_bytes() == b"/" {
        // SAFETY: single-threaded init; the global takes ownership of `endpoint`.
        unsafe { FS_ENDPOINT = endpoint };
    } else {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
    }
    if br != 0 {
        kprint(b"init: bind FAIL at ");
        kprint(m.mount_point.as_bytes());
        kprint(b"\n");
        return false;
    }

    kprint(b"init: mounted fs-server-ext4 at ");
    kprint(m.mount_point.as_bytes());
    kprint(b"\n");
    // init keeps `fs_h` (the long-lived server's process handle).
    let _ = fs_h;
    true
}

/// Wait (bounded) for an fs-server's `Meta::Ready` on `ctrl`, validate it
/// (`"RSMG"` magic + `Ready` op, hand-parsed — init never speaks `librsproto`), and
/// return the forwarding endpoint it transfers (`handles[0]`). `None` on timeout, a
/// recv error, no transferred handle, or an unexpected message.
fn wait_ready(ctrl: u64) -> Option<u64> {
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
    if waited < 1 {
        return None; // timed out / error
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
    let count = unsafe { (&raw const IPC_COUNT).read() };
    if rr != 0 || count < 1 {
        return None;
    }
    // Hand-parse the rsproto envelope in the IpcMsg payload (offset 24): magic @0,
    // op @6. Confirm it is a Meta::Ready before trusting handles[0].
    let (magic, op, endpoint) = unsafe {
        let magic = u32::from_le_bytes([IPC_MSG[24], IPC_MSG[25], IPC_MSG[26], IPC_MSG[27]]);
        let op = u16::from_le_bytes([IPC_MSG[30], IPC_MSG[31]]);
        (magic, op, (&raw const IPC_HANDLES[0]).read())
    };
    if magic != RS_MAGIC || op != RS_OP_READY {
        // Not the message we expected — drop the transferred endpoint.
        unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
        return None;
    }
    Some(endpoint)
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
    let endpoint = match wait_ready(ctrl_init) {
        Some(e) => e,
        None => {
            kprint(b"init: profile-server Ready timeout/invalid\n");
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
    // SAFETY: valid namespace handle + path pointer + endpoint handle.
    let br = unsafe {
        syscall4(SYS_NS_BIND, root_ns, b"/bin".as_ptr() as u64, 4, endpoint)
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

/// Spawn the system logging service and bind its forwarding endpoint at `/log` (the RS
/// startup protocol, minus a device — it needs none). Clients then resolve
/// `/log/<tier>/<principal>` to obtain a per-principal log channel. Bound before the
/// service manager starts so services can log from launch. Returns `true` once bound.
fn bind_logging_service(root_ns: u64) -> bool {
    // 1. Create the control channel (init keeps end 0, the server gets end 1).
    // SAFETY: CTRL0/CTRL1 are valid writable out-params (reused; mounts + profile bind
    // already completed).
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0)
    };
    if cr != 0 {
        return false;
    }
    let (ctrl_init, ctrl_srv) = unsafe { ((&raw const CTRL0).read(), (&raw const CTRL1).read()) };

    // 2. Spawn the logging service, moving the control endpoint into it (in rdx).
    // SAFETY: SPAWN_LOGGING is a valid writable arg block; spawn_program resolves the ELF
    // image from the initramfs, stamps it, spawns, and closes the image handle.
    let ls_h = unsafe {
        SPAWN_LOGGING.handles[0] = ctrl_srv;
        spawn_program(root_ns, b"/bin/logging-service", &raw mut SPAWN_LOGGING)
    };
    if ls_h < 0 {
        kprint(b"init: logging-service spawn FAIL\n");
        // SAFETY: closing our own control endpoint (ctrl_srv moved to the child).
        unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
        return false;
    }

    // 3. Await Meta::Ready (bounded), then take the forwarding endpoint it carries.
    let endpoint = match wait_ready(ctrl_init) {
        Some(e) => e,
        None => {
            kprint(b"init: logging-service Ready timeout/invalid\n");
            // SAFETY: closing our own control endpoint.
            unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
            return false;
        }
    };
    // SAFETY: closing our own control endpoint (handshake done).
    unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };

    // 4. Bind the forwarding endpoint at `/log`.
    // SAFETY: valid namespace handle + path pointer + endpoint handle.
    let br = unsafe { syscall4(SYS_NS_BIND, root_ns, b"/log".as_ptr() as u64, 4, endpoint) };
    // SAFETY: closing init's endpoint handle (the binding holds its own reference).
    unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
    if br != 0 {
        kprint(b"init: logging-service bind FAIL at /log\n");
        return false;
    }

    kprint(b"init: logging service bound at /log\n");
    // init keeps `ls_h` (the long-lived server's process handle).
    let _ = ls_h;
    true
}

/// Spawn the terminal server and bind its forwarding endpoint at `/dev/tty`.
///
/// It holds `/dev/console` exclusively from here on; a session gets `/dev/tty` and cannot
/// reach the raw device at all. A client resolving `/dev/tty` receives a fresh per-caller
/// channel, the same shape the logging service uses. Non-critical: a boot without a
/// terminal server still reaches `eshell`, which owns the raw device precisely because it
/// runs when this does not. See `docs/architecture/console-and-tty.md`.
fn bind_tty_server(root_ns: u64) -> bool {
    // 1. Create the control channel (init keeps end 0, the server gets end 1).
    // SAFETY: CTRL0/CTRL1 are valid writable out-params (reused; mounts + profile bind
    // already completed).
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0)
    };
    if cr != 0 {
        return false;
    }
    let (ctrl_init, ctrl_srv) = unsafe { ((&raw const CTRL0).read(), (&raw const CTRL1).read()) };

    // 2. Spawn the tty server, moving the control endpoint into it (in rdx).
    // SAFETY: SPAWN_TTY is a valid writable arg block; spawn_program resolves the ELF
    // image from the initramfs, stamps it, spawns, and closes the image handle.
    let ls_h = unsafe {
        SPAWN_TTY.handles[0] = ctrl_srv;
        spawn_program(root_ns, b"/bin/tty-server", &raw mut SPAWN_TTY)
    };
    if ls_h < 0 {
        kprint(b"init: tty-server spawn FAIL\n");
        // SAFETY: closing our own control endpoint (ctrl_srv moved to the child).
        unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
        return false;
    }

    // 3. Await Meta::Ready (bounded), then take the forwarding endpoint it carries.
    let endpoint = match wait_ready(ctrl_init) {
        Some(e) => e,
        None => {
            kprint(b"init: tty-server Ready timeout/invalid\n");
            // SAFETY: closing our own control endpoint.
            unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
            return false;
        }
    };
    // SAFETY: closing our own control endpoint (handshake done).
    unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };

    // 4. Bind the forwarding endpoint at `/log`.
    // SAFETY: valid namespace handle + path pointer + endpoint handle.
    // Keep a second handle *before* binding, for session-mgr to bind into each session.
    // Duplicating first means a failure here is a failure to bind at all, rather than a
    // bound `/dev/tty` no session can be given.
    // SAFETY: duplicating our own endpoint handle with attenuated rights.
    let retained = unsafe {
        syscall2(SYS_HANDLE_DUPLICATE, endpoint, RIGHT_TRANSFER | RIGHT_DUPLICATE)
    };
    let br = unsafe { syscall4(SYS_NS_BIND, root_ns, b"/dev/tty".as_ptr() as u64, 8, endpoint) };
    if br == 0 && retained >= 0 {
        // SAFETY: single-threaded init.
        unsafe { TTY_ENDPOINT = retained as u64 };
    } else if retained >= 0 {
        // SAFETY: the bind failed; nothing will use the duplicate.
        unsafe { syscall1(SYS_HANDLE_CLOSE, retained as u64) };
    }
    // SAFETY: closing init's endpoint handle (the binding holds its own reference).
    unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
    if br != 0 {
        kprint(b"init: tty-server bind FAIL at /dev/tty\n");
        return false;
    }

    kprint(b"init: tty server bound at /dev/tty\n");
    // init keeps `ls_h` (the long-lived server's process handle).
    let _ = ls_h;
    true
}

/// Spawn the input server and bind its endpoint at `/dev/input/new`.
///
/// The Resource Server Startup Protocol, as everywhere: spawn with a control channel, wait
/// for `Meta::Ready`, bind the forwarding endpoint it carries. The server never binds
/// anything itself and holds no `BIND_NAMESPACE`.
///
/// Returns `false` on any failure, which is not fatal to the boot: a machine with no i8042
/// has no raw nodes, the server exits saying so, and everything else comes up normally.
fn bind_input_server(root_ns: u64) -> bool {
    // SAFETY: CTRL0/CTRL1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0)
    };
    if cr != 0 {
        return false;
    }
    let (ctrl_init, ctrl_srv) = unsafe { ((&raw const CTRL0).read(), (&raw const CTRL1).read()) };

    // SAFETY: SPAWN_INPUT_SERVER is a valid writable arg block.
    let h = unsafe {
        SPAWN_INPUT_SERVER.handles[0] = ctrl_srv;
        spawn_program(root_ns, b"/initramfs/sbin/input-server", &raw mut SPAWN_INPUT_SERVER)
    };
    if h < 0 {
        kprint(b"init: input-server spawn FAIL\n");
        // SAFETY: closing our own control endpoint (ctrl_srv moved to the child).
        unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
        return false;
    }

    let endpoint = match wait_ready(ctrl_init) {
        Some(e) => e,
        None => {
            kprint(b"init: input-server Ready timeout/invalid\n");
            // SAFETY: done with the control channel either way.
            unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
            return false;
        }
    };
    // SAFETY: closing init's own control endpoint — the `PeerClosed` the server expects.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };

    // SAFETY: binding the server's forwarding endpoint at /dev/input/new.
    let br =
        unsafe { syscall4(SYS_NS_BIND, root_ns, b"/dev/input/new".as_ptr() as u64, 14, endpoint) };
    // The binding takes its own reference, so init's handle goes either way.
    // SAFETY: closing init's reference to the endpoint.
    unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
    if br != 0 {
        kprint(b"init: input-server bind FAIL at /dev/input/new\n");
        return false;
    }
    kprint(b"init: input-server bound at /dev/input/new\n");
    true
}

/// Spawn the compositor and bind its endpoint at `/dev/draw`.
///
/// **Non-fatal.** A machine with no usable framebuffer still boots to a serial console,
/// and every existing test path is serial; the display arm is the only consumer. Returns
/// `false` if the display is unavailable, which the caller reports but does not treat as a
/// boot failure.
///
/// The binding is a **subtree** — the compositor answers resolves for everything beneath
/// `/dev/draw`, the same shape `/home` uses — so `/dev/draw/new` forwards `new` to it and
/// nobody calls `sys_ns_bind` when a window opens.
fn bind_compositor(root_ns: u64) -> bool {
    // 1. Control channel: init keeps end 0, the server gets end 1.
    // SAFETY: CTRL0/CTRL1 are valid writable out-params; earlier binds have completed.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0)
    };
    if cr != 0 {
        return false;
    }
    let (ctrl_init, ctrl_srv) = unsafe { ((&raw const CTRL0).read(), (&raw const CTRL1).read()) };

    // 2. Spawn, moving the control endpoint in.
    // SAFETY: SPAWN_COMPOSITOR is a valid writable arg block.
    let h = unsafe {
        SPAWN_COMPOSITOR.handles[0] = ctrl_srv;
        spawn_program(root_ns, b"/initramfs/sbin/compositor", &raw mut SPAWN_COMPOSITOR)
    };
    if h < 0 {
        kprint(b"init: compositor spawn FAIL\n");
        // SAFETY: closing our own control endpoint (ctrl_srv moved to the child).
        unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
        return false;
    }

    // 3. Await Meta::Ready and take the forwarding endpoint it carries.
    let endpoint = match wait_ready(ctrl_init) {
        Some(e) => e,
        None => {
            kprint(b"init: compositor Ready timeout/invalid\n");
            // SAFETY: done with the control channel either way.
            unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };
            return false;
        }
    };
    // The control channel has served its purpose. Closing it is not tidiness: it is the
    // `PeerClosed` every other server observes when init is finished with it.
    // SAFETY: closing init's own control endpoint.
    unsafe { syscall1(SYS_HANDLE_CLOSE, ctrl_init) };

    // SAFETY: binding the compositor's forwarding endpoint as a subtree at /dev/draw.
    let br = unsafe { syscall4(SYS_NS_BIND, root_ns, b"/dev/draw".as_ptr() as u64, 9, endpoint) };
    // The binding takes its own reference, so init's handle goes either way.
    // SAFETY: closing init's reference to the endpoint.
    unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
    if br != 0 {
        kprint(b"init: compositor bind FAIL at /dev/draw\n");
        return false;
    }
    kprint(b"init: compositor bound at /dev/draw\n");
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

/// Size of the Part-5 large-file fixture (`/system/large.bin`). MUST match the
/// xtask generator (`tools/xtask/src/main.rs`). 32 KiB = 8 pages — past the old
/// 64 KiB eager read cap, so reading it proves the page cache lifts the cap.
/// (Was 64 pages; trimmed to 8 because each page demand-faults through the
/// stateless fs-server fill at ~325 ms/page under QEMU — read-ahead is a Phase-3
/// item, see docs/rationale/deferred-decisions.md.)
#[cfg(feature = "selftest")]
const LARGE_FILE_BYTES: usize = 32 * 1024;

/// The expected byte at file offset `i` of `/system/large.bin` — position-sensitive
/// (the page index `i >> 12` in the high part) so a mis-faulted page is detected.
/// MUST match the xtask generator.
#[cfg(feature = "selftest")]
fn fill_byte(i: usize) -> u8 {
    (((i >> 12) ^ i) & 0xFF) as u8
}

/// fs-server-rw Part C milestone (selftest): **overwrite** an existing file in place through
/// a `MAP_WRITE` mapping, `sys_file_sync`, then re-resolve (a fresh `FileObject` that reads
/// the block from disk) and verify the change persisted — proving the Model A write data path
/// (dirty pages → write IRPs → device) with no fs-server metadata write.
#[cfg(feature = "selftest")]
fn overwrite_test(root_ns: u64) {
    let path = b"/system/rwtest";
    let marker = [0xDEu8, 0xAD, 0xBE, 0xEF];

    // 1. Map MAP_READ | MAP_WRITE; note an untouched byte, then overwrite bytes 0..4.
    let (st, fh) = ns_lookup_wait(root_ns, path, RIGHT_MAP_READ | RIGHT_MAP_WRITE);
    if st != 0 || fh == 0 {
        kprint(b"init: rwtest lookup FAIL\n");
        return;
    }
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, PAGE, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"init: rwtest map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return;
    }
    let base = addr as u64;
    // SAFETY: byte 8 is within the mapped page; read the original (== 8) to compare later.
    let orig8 = unsafe { ((base + 8) as *const u8).read_volatile() };
    // SAFETY: bytes 0..4 are within the writable mapping — the write dirties the page.
    for (i, m) in marker.iter().enumerate() {
        unsafe { ((base + i as u64) as *mut u8).write_volatile(*m) };
    }
    // 2. Flush the mapping's pages to disk (Model A write IRPs to the existing LBAs).
    // SAFETY: `fh` is our writable FileObject handle.
    if unsafe { syscall1(SYS_FILE_SYNC, fh) } != 0 {
        kprint(b"init: rwtest sync FAIL\n");
    }

    // 3. Re-resolve (a fresh FileObject reads from disk) and verify the overwrite persisted
    //    and the untouched byte is unchanged.
    let (st2, fh2) = ns_lookup_wait(root_ns, path, RIGHT_MAP_READ);
    if st2 != 0 || fh2 == 0 {
        kprint(b"init: rwtest re-read lookup FAIL\n");
        return;
    }
    let addr2 = unsafe { syscall4(SYS_MEMORY_MAP, fh2, 0, PAGE, RIGHT_MAP_READ) };
    if addr2 < 0 {
        kprint(b"init: rwtest re-read map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh2) };
        return;
    }
    let base2 = addr2 as u64;
    let mut ok = true;
    for (i, m) in marker.iter().enumerate() {
        // SAFETY: within the mapped page.
        if unsafe { ((base2 + i as u64) as *const u8).read_volatile() } != *m {
            ok = false;
        }
    }
    // SAFETY: byte 8 within the page — must be unchanged.
    let reread8 = unsafe { ((base2 + 8) as *const u8).read_volatile() };
    if ok && reread8 == orig8 {
        kprint(b"init: rwtest overwrite persisted + verified ok\n");
    } else {
        kprint(b"init: rwtest overwrite MISMATCH\n");
    }
}

/// fs-server-rw Part D milestone (selftest): **grow** a file past EOF via `sys_file_grow`
/// (the fs-server allocates a block + extends its extent tree + updates the inode), write
/// into the newly-allocated region, `sys_file_sync`, then re-resolve and confirm the
/// appended data persisted — proving the write path's metadata mutation end to end.
#[cfg(feature = "selftest")]
fn grow_test(root_ns: u64) {
    let path = b"/system/rwtest";
    let marker = [0xC0u8, 0xFF, 0xEEu8, 0x11];
    let new_size: u64 = 8000; // 4096 (1 block) → 8000 (2 blocks)

    // 1. Grow-resolve: the fs-server grows the file, then replies its (2-block) map. The
    //    lookup returns a PO; wait for the handle.
    let po = unsafe {
        syscall5(
            SYS_FILE_GROW,
            root_ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_MAP_READ | RIGHT_MAP_WRITE,
            new_size,
        )
    };
    if po < 0 {
        kprint(b"init: grow submit FAIL\n");
        return;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let (st, fh) = unsafe {
        WAIT_HANDLES[0] = po as u64;
        let w = syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        );
        let status =
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]);
        let handle = u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ]);
        syscall1(SYS_HANDLE_CLOSE, po as u64);
        if w != 1 { (-1, 0) } else { (status, handle) }
    };
    if st != 0 || fh == 0 {
        kprint(b"init: grow FAIL\n");
        return;
    }

    // 2. Map the grown file; write a marker in the **new** region (the appended 2nd block).
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, new_size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"init: grow map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return;
    }
    let base = addr as u64;
    for (i, m) in marker.iter().enumerate() {
        // SAFETY: offset `PAGE + i` is in the 2nd mapped page (the appended block).
        unsafe { ((base + PAGE + i as u64) as *mut u8).write_volatile(*m) };
    }
    // SAFETY: `fh` is our writable handle.
    if unsafe { syscall1(SYS_FILE_SYNC, fh) } != 0 {
        kprint(b"init: grow sync FAIL\n");
    }

    // 3. Re-resolve (a fresh FileObject reads from disk) and verify the appended data.
    let (st2, fh2) = ns_lookup_wait(root_ns, path, RIGHT_MAP_READ);
    if st2 != 0 || fh2 == 0 {
        kprint(b"init: grow re-read FAIL\n");
        return;
    }
    let addr2 = unsafe { syscall4(SYS_MEMORY_MAP, fh2, 0, new_size, RIGHT_MAP_READ) };
    if addr2 < 0 {
        kprint(b"init: grow re-read map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh2) };
        return;
    }
    let base2 = addr2 as u64;
    let mut ok = true;
    for (i, m) in marker.iter().enumerate() {
        // SAFETY: within the 2nd mapped page.
        if unsafe { ((base2 + PAGE + i as u64) as *const u8).read_volatile() } != *m {
            ok = false;
        }
    }
    if ok {
        kprint(b"init: grow appended a block + persisted + verified ok\n");
    } else {
        kprint(b"init: grow MISMATCH\n");
    }
}

/// fs-server-rw Part E milestone (selftest): **create** a brand-new file via
/// `sys_file_create` (the fs-server allocates an inode + inserts a directory entry in the
/// parent, then grows it to the target size), write into it, `sys_file_sync`, then
/// re-resolve with a plain lookup and confirm both that the new path now resolves and that
/// its data persisted — proving inode allocation + directory-entry insertion end to end.
#[cfg(feature = "selftest")]
fn create_test(root_ns: u64) {
    let path = b"/system/created";
    let marker = [0xABu8, 0xCD, 0xEFu8, 0x42];
    let new_size: u64 = 4096; // fresh file → 1 block.

    // 1. Create-resolve: the fs-server creates the file, grows it, then replies its map.
    let po = unsafe {
        syscall5(
            SYS_FILE_CREATE,
            root_ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_MAP_READ | RIGHT_MAP_WRITE,
            new_size,
        )
    };
    if po < 0 {
        kprint(b"init: create submit FAIL\n");
        return;
    }
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let (st, fh) = unsafe {
        WAIT_HANDLES[0] = po as u64;
        let w = syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        );
        let status =
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]);
        let handle = u64::from_le_bytes([
            WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
            WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
        ]);
        syscall1(SYS_HANDLE_CLOSE, po as u64);
        if w != 1 { (-1, 0) } else { (status, handle) }
    };
    if st != 0 || fh == 0 {
        kprint(b"init: create FAIL\n");
        return;
    }

    // 2. Map the new file; write a marker at the start.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, new_size, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if addr < 0 {
        kprint(b"init: create map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return;
    }
    let base = addr as u64;
    for (i, m) in marker.iter().enumerate() {
        // SAFETY: offset `i` is within the mapped first page.
        unsafe { ((base + i as u64) as *mut u8).write_volatile(*m) };
    }
    // SAFETY: `fh` is our writable handle.
    if unsafe { syscall1(SYS_FILE_SYNC, fh) } != 0 {
        kprint(b"init: create sync FAIL\n");
    }

    // 3. Re-resolve with a **plain** lookup (proves the directory entry is on disk: a path
    //    that did not exist before now resolves) and verify the data.
    let (st2, fh2) = ns_lookup_wait(root_ns, path, RIGHT_MAP_READ);
    if st2 != 0 || fh2 == 0 {
        kprint(b"init: create re-read FAIL\n");
        return;
    }
    let addr2 = unsafe { syscall4(SYS_MEMORY_MAP, fh2, 0, new_size, RIGHT_MAP_READ) };
    if addr2 < 0 {
        kprint(b"init: create re-read map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh2) };
        return;
    }
    let base2 = addr2 as u64;
    let mut ok = true;
    for (i, m) in marker.iter().enumerate() {
        // SAFETY: within the mapped first page.
        if unsafe { ((base2 + i as u64) as *const u8).read_volatile() } != *m {
            ok = false;
        }
    }
    if ok {
        kprint(b"init: create new file + persisted + verified ok\n");
    } else {
        kprint(b"init: create MISMATCH\n");
    }
}

/// auth+session Part B milestone (selftest): prove **subtree-scoped namespace
/// binding** end to end. `mount_one` bound the fs endpoint a second time at
/// `/subtreetest` scoped to base `/system` (sharing the server's registration), so a
/// lookup of `/subtreetest/current-generation` must forward `system/current-generation`
/// to the server and resolve to the *same* file as `/system/current-generation`. Read
/// the leading bytes of both and confirm they match — the kernel prepended the base to
/// the forwarded suffix, and the shared registration routed both replies correctly.
#[cfg(feature = "selftest")]
fn subtree_bind_test(root_ns: u64) {
    // Resolve + map the first page of `path` read-only; returns its address or 0.
    fn map_first_page(root_ns: u64, path: &[u8]) -> u64 {
        let (st, fh) = ns_lookup_wait(root_ns, path, RIGHT_MAP_READ);
        if st != 0 || fh == 0 {
            return 0;
        }
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, PAGE, RIGHT_MAP_READ) };
        // The mapping pins its own reference to the object; close the handle.
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        if addr < 0 { 0 } else { addr as u64 }
    }

    let direct = map_first_page(root_ns, b"/system/current-generation");
    let via_sub = map_first_page(root_ns, b"/subtreetest/current-generation");
    if direct == 0 || via_sub == 0 {
        kprint(b"init: subtree resolve FAIL\n");
        return;
    }
    // Compare the leading bytes (the file is a short text line; the page tail is
    // zero-padded, so the head suffices).
    let mut same = true;
    for i in 0..64u64 {
        // SAFETY: both addresses map a full page; `i < 64 < PAGE`.
        let a = unsafe { ((direct + i) as *const u8).read_volatile() };
        let b = unsafe { ((via_sub + i) as *const u8).read_volatile() };
        if a != b {
            same = false;
            break;
        }
    }
    // SAFETY: unmap our two mappings (init runs forever — don't leak).
    unsafe {
        syscall2(SYS_MEMORY_UNMAP, direct, PAGE);
        syscall2(SYS_MEMORY_UNMAP, via_sub, PAGE);
    }
    if same {
        kprint(b"init: subtree bind (/subtreetest -> /system) resolves + matches ok\n");
    } else {
        kprint(b"init: subtree bind MISMATCH\n");
    }
}

/// The slice-8 Part-5 milestone: map the **large** file `/system/large.bin`
/// (lazily, a `FileObject`) and read **every** byte — each first touch of a page is
/// a demand fault the kernel services by a `File::ReadRange` to the fs-server. Verify
/// the position-sensitive content (so a mis-filled / mis-ordered page is caught) and
/// log the result. Proves **multi-page demand faulting** past the old 64 KiB cap.
#[cfg(feature = "selftest")]
fn read_large_file(root_ns: u64) {
    let (st, fh) = ns_lookup_wait(root_ns, b"/system/large.bin", RIGHT_MAP_READ);
    if st != 0 || fh == 0 {
        kprint(b"init: /system/large.bin lookup FAIL\n");
        return;
    }
    // Map the whole file lazily (a FileBacked VMA — no frames until faulted).
    let addr =
        unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, LARGE_FILE_BYTES as u64, RIGHT_MAP_READ) };
    if addr < 0 {
        kprint(b"init: large.bin map FAIL\n");
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
        return;
    }
    let base = addr as u64;
    let mut mismatches = 0u64;
    let mut i = 0usize;
    while i < LARGE_FILE_BYTES {
        // First touch of each page faults; the kernel demand-fills it from the
        // fs-server. Subsequent bytes in the page are plain (already-resident) reads.
        // SAFETY: `base + i` is within the mapped [0, LARGE_FILE_BYTES) file range.
        let got = unsafe { ((base + i as u64) as *const u8).read_volatile() };
        if got != fill_byte(i) {
            mismatches += 1;
        }
        i += 1;
    }
    if mismatches == 0 {
        kprint(b"init: large.bin verified ");
        kprint_u64(LARGE_FILE_BYTES as u64);
        kprint(b" bytes across ");
        kprint_u64(LARGE_FILE_BYTES as u64 / PAGE);
        kprint(b" demand-faulted pages ok\n");
    } else {
        kprint(b"init: large.bin MISMATCH count=");
        kprint_u64(mismatches);
        kprint(b"\n");
    }
    // SAFETY: closing our own handle (the mapping keeps the object alive meanwhile).
    unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
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
#[cfg(feature = "test-harness")]
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

/// Spawn the service manager — the normal boot handoff. init keeps a handle to it (it
/// is init's child; service-mgr's death is a critical fault init must observe). Unlike
/// `eshell`, this is *not* closed after spawn, so init's reap loop can see a
/// `ChildExited` for it. Returns the process handle, or a negative error.
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
    // Nothing to hand over — a **restart**, since the endpoints moved to the first
    // service-mgr and cannot move twice. Spawn with no `handles[0]` at all, so the child
    // reads `rdx == 0` and takes its documented "no endpoints; skipping login chain" path.
    // Handing it a live but permanently empty channel would leave it blocked on a handoff
    // that is never coming, turning a degraded restart into a hung one.
    // SAFETY: single-threaded init.
    if unsafe { FS_ENDPOINT == 0 && PROFILE_ENDPOINT == 0 && TTY_ENDPOINT == 0 } {
        kprint(b"init: service-mgr restart -- no endpoints left to hand over\n");
        // SAFETY: SPAWN_SERVICE_MGR is our static; spawns are sequential.
        return unsafe {
            SPAWN_SERVICE_MGR.handles[0] = 0;
            SPAWN_SERVICE_MGR.handle_count = 0;
            SPAWN_SERVICE_MGR.move_mask = 0;
            spawn_program(root_ns, b"/bin/service-mgr", &raw mut SPAWN_SERVICE_MGR)
        };
    }

    // The handoff channel: depth 4, so both sends land in the ring without init ever
    // blocking on a child that has not run yet.
    // SAFETY: CTRL0/CTRL1 are valid writable out-params (mounts are long done).
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CTRL0) as u64, (&raw mut CTRL1) as u64, 4, 0)
    };
    if cr != 0 {
        kprint(b"init: service-mgr handoff channel FAIL\n");
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

    // The handoffs, in the order service-mgr receives them: the fs-server endpoint, then
    // the profile server's. Both are queued in the child's inbox; it has not run yet.
    // SAFETY: single-threaded init; each endpoint moves once, and the sends null the
    // statics so a later path cannot close a handle it no longer owns.
    unsafe {
        send_handle(init_end, FS_ENDPOINT);
        FS_ENDPOINT = 0;
        send_handle(init_end, PROFILE_ENDPOINT);
        PROFILE_ENDPOINT = 0;
        send_handle(init_end, TTY_ENDPOINT);
        TTY_ENDPOINT = 0;
        syscall1(SYS_HANDLE_CLOSE, init_end);
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
    // empty payload. NoBlock: the ring is depth 4 and holds at most two handoffs.
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
        if TTY_ENDPOINT != 0 {
            syscall1(SYS_HANDLE_CLOSE, TTY_ENDPOINT);
            TTY_ENDPOINT = 0;
        }
    }
}

/// Run the integration test harness synchronously (selftest builds): spawn it, block
/// until it exits, and report whether it exited `0`. A non-zero exit lets the caller
/// fail the run; a hang means this never returns, so the runner's wall-clock timeout
/// fails it. Its child processes (test-stages) are reaped by the harness, not init.
#[cfg(feature = "selftest")]
fn run_test_harness(notif: u64, root_ns: u64) -> bool {
    kprint(b"init: running integration test harness\n");
    // SAFETY: SPAWN_HARNESS is a valid writable arg block.
    let h =
        unsafe { spawn_program(root_ns, b"/initramfs/sbin/test-harness", &raw mut SPAWN_HARNESS) };
    if h < 0 {
        kprint(b"init: test-harness spawn FAIL\n");
        return false;
    }
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if waited < 1 {
            continue;
        }
        // SAFETY: NOTIF is a valid 64-byte writable out-param.
        let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
        if r != 0 {
            continue; // WouldBlock: drained
        }
        // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
        let (kind, body) =
            unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
        if kind == KIND_CHILD_EXITED {
            let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
            // SAFETY: closing our own process handle for the harness.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
            return code == 0;
        }
    }
}

/// Spawn `display-selftest` and adjudicate it.
///
/// The program reports three outcomes and this decides which of them matter: `0` passed,
/// **`2` found no `/dev/framebuffer` binding**, anything else failed.
///
/// The `2` case is the one that needs a policy rather than a value judgement in the
/// program. On a real machine with no display it is expected. Under `test-harness` the
/// emulator always reports a framebuffer, so it means the binding is broken — and folding
/// it into success is how the entire display arm could go missing with `test-qemu` still
/// green, which is exactly what an earlier version of this code did.
#[cfg(feature = "selftest")]
fn run_display_selftest(notif: u64, root_ns: u64) {
    // SAFETY: SPAWN_DISPLAY is a valid writable arg block.
    let h = unsafe {
        spawn_program(root_ns, b"/initramfs/sbin/display-selftest", &raw mut SPAWN_DISPLAY)
    };
    if h < 0 {
        kprint(b"init: display-selftest spawn FAIL\n");
        #[cfg(feature = "test-harness")]
        test_exit(false);
        return;
    }
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if waited < 1 {
            continue;
        }
        // SAFETY: NOTIF is a valid 64-byte writable out-param.
        let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
        if r != 0 {
            continue; // WouldBlock: drained
        }
        // SAFETY: the kernel wrote a 64-byte Notification into NOTIF.
        let (kind, body) =
            unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
        if kind == KIND_CHILD_EXITED {
            let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
            // SAFETY: closing init's reference to the finished child.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
            match code {
                0 => {}
                2 => {
                    kprint(b"init: display-selftest found no display\n");
                    #[cfg(feature = "test-harness")]
                    {
                        kprint(b"init: ...but this build always has one -- FAILED\n");
                        test_exit(false);
                    }
                }
                _ => {
                    kprint(b"init: display-selftest FAILED\n");
                    #[cfg(feature = "test-harness")]
                    test_exit(false);
                }
            }
            return;
        }
    }
}

/// Spawn `ui-testclient` — the display arm's first real client.
///
/// **Spawned, not awaited.** On success the client *parks*: exiting would close its
/// channel, and the compositor would correctly destroy its windows and repaint, leaving
/// the display gate to capture an empty screen. So it reports its own failures through
/// `SYS_TEST_EXIT` — the same verdict path init uses — and a successful run simply leaves
/// its window on screen for the rest of the boot.
///
/// This is the first test that exercises a client and the compositor together. Everything
/// before it tested one half: `libui` against a mock, the compositor against nothing. That
/// is how a one-way protocol shipped with green CI.
#[cfg(feature = "selftest")]
fn run_ui_testclient(root_ns: u64) {
    // SAFETY: SPAWN_UICLIENT is a valid writable arg block.
    let h = unsafe {
        spawn_program(root_ns, b"/initramfs/sbin/ui-testclient", &raw mut SPAWN_UICLIENT)
    };
    if h < 0 {
        kprint(b"init: ui-testclient spawn FAIL\n");
        #[cfg(feature = "test-harness")]
        test_exit(false);
        return;
    }
    // SAFETY: closing init's reference; the client runs on and is reaped by `reap_loop`
    // if it ever does exit.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
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
/// sustained multi-second load.) session-mgr fires the `test-harness` verdict once login is
/// proven; a crashed demo `parent` fails the run first (in `reap_loop`).
fn supervise(notif: u64, root_ns: u64) -> ! {
    #[cfg(feature = "selftest")]
    {
        // Serial adjudication (decision log 2026-07-24): run the integration test harness
        // **first**, synchronously — a non-zero exit fails the run, and a hang fails it via
        // the runner's wall-clock timeout (init never reaches the verdict) — **then** hand
        // off to the login chain, which fires the PASS verdict once login is proven. (The
        // earlier harness/login concurrency is retired with the parent/child demos.)
        if !run_test_harness(notif, root_ns) {
            kprint(b"init: integration test harness FAILED\n");
            #[cfg(feature = "test-harness")]
            test_exit(false);
            // Interactive selftest: nothing to hand off to; reap orphans.
            reap_loop(notif, root_ns, 0);
        }
        kprint(b"init: harness passed; handing off to login chain\n");
        let smgr_h = spawn_service_mgr(root_ns);
        if smgr_h >= 0 {
            // service-mgr runs independently and fires the verdict once login is proven
            // (or drops to the `login:` prompt in an interactive selftest). SAFETY:
            // closing init's reference; service-mgr runs on.
            unsafe { syscall1(SYS_HANDLE_CLOSE, smgr_h as u64) };
        }
        reap_loop(notif, root_ns, 0);
    }
    // Normal boot: hand off to the service manager and supervise it.
    #[cfg(not(feature = "selftest"))]
    {
        let service_mgr_h = spawn_service_mgr(root_ns);
        reap_loop(notif, root_ns, service_mgr_h);
    }
}

/// The **emergency** path: a critical-path boot failure (bad manifest, failed
/// mount). Drop straight to the interactive shell so the operator can inspect the
/// broken system (`cat /dev/log`, `mounts`, `lsblk`) — no demo chain, no milestones.
/// See `userspace/init/CLAUDE.md` § "Failure → eshell".
fn emergency(notif: u64, root_ns: u64) -> ! {
    kprint(b"init: critical-path failure -- dropping to emergency shell\n");
    // Test-harness: a critical-path boot failure is a failed test run.
    #[cfg(feature = "test-harness")]
    test_exit(false);
    spawn_eshell(root_ns);
    reap_loop(notif, root_ns, 0);
}

/// Reap exited children forever (init is the eventual parent of every orphan).
/// `parent_h` is the handle of the one child whose exit init reacts to — the demo
/// `parent` under `selftest` (a crash fails the test run; the login chain is already
/// up concurrently, so nothing is spawned on its exit), or `service-mgr` on a normal
/// boot (its death is a critical fault → interim recovery brings a fresh one up) — or
/// `0` if none is pending. All other orphans are logged and released.
fn reap_loop(notif: u64, root_ns: u64, mut parent_h: i64) -> ! {
    // `root_ns` is only needed on a normal boot (to respawn a dead service-mgr); under
    // `selftest` the login chain is already up, so mark it used to avoid a warning.
    #[cfg(feature = "selftest")]
    let _ = root_ns;
    kprint(b"init: entering reaping loop\n");
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                1,
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
                kprint(b"init: reaped pid=");
                kprint_u64(cpid as u64);
                kprint(b" code=");
                kprint_u64(code as u64);
                kprint(b"\n");
                // Release init's reference to the primary child on its exit. Reparented
                // orphans have no handle here — the kernel tears them down; init observes.
                if parent_h != 0 {
                    // SAFETY: closing our own process handle.
                    unsafe { syscall1(SYS_HANDLE_CLOSE, parent_h as u64) };
                    parent_h = 0;
                    #[cfg(feature = "selftest")]
                    {
                        // Primary = the demo `parent`. The login chain is already running
                        // concurrently (spawned in `supervise`) and owns the verdict; here
                        // a crashed demo fails the run. session-mgr fires the final PASS once
                        // it has authenticated the demo user under that concurrent load.
                        #[cfg(feature = "test-harness")]
                        if code != 0 {
                            test_exit(false);
                        }
                        // The interactive console is session-mgr's `login:` prompt (via the
                        // login chain), not eshell — eshell is the *emergency* shell only
                        // (the `emergency` path). Nothing to spawn here.
                    }
                    #[cfg(not(feature = "selftest"))]
                    {
                        // Primary = service-mgr; its death is a critical fault. Interim
                        // recovery until a reboot path exists: bring a fresh one up.
                        let smgr_h = spawn_service_mgr(root_ns);
                        if smgr_h >= 0 {
                            // SAFETY: closing init's reference; service-mgr runs independently.
                            unsafe { syscall1(SYS_HANDLE_CLOSE, smgr_h as u64) };
                        }
                    }
                }
            }
        }
    }
}

/// Bootstrap registers: `rdi` = notification channel, `rsi` = root namespace
/// (full-rights, kernel-bound servers), `rdx`/`rcx` unused (init takes no
/// installed handles or arg0 from the kernel).
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, _handle0: u64, _arg0: u64) -> ! {
    kprint(b"init: up (pid 1)\n");
    let count = (notif != 0) as u64 + (root_ns != 0) as u64;
    kprint(b"init: received ");
    kprint_u64(count);
    kprint(b" handles (notif=");
    kprint_u64(notif);
    kprint(b", ns=");
    kprint_u64(root_ns);
    kprint(b")\n");

    // Read the manifest and process its mounts (spawn fs-servers → Ready → bind). A
    // missing/invalid manifest or a failed required mount is a **critical-path
    // failure** → drop to the emergency shell (the operator inspects the broken
    // system). On success, prove the stack end to end (the slice-7/8 milestones) and
    // enter the normal supervise path.
    let booted = match read_manifest(root_ns) {
        Some(mounts) => mount_all(root_ns, &mounts),
        None => {
            kprint(b"init: no usable boot manifest\n");
            false
        }
    };
    if !booted {
        emergency(notif, root_ns);
    }

    read_current_generation(root_ns);
    // Slice-8 Part-5 milestone (selftest): a large file read entirely through the page
    // cache — many demand faults, each a `File::ReadRange` to the fs-server.
    #[cfg(feature = "selftest")]
    read_large_file(root_ns);
    // fs-server-rw Part C: overwrite an existing file in place and confirm it persists.
    #[cfg(feature = "selftest")]
    overwrite_test(root_ns);
    // fs-server-rw Part D: grow a file past EOF and confirm the appended data persists.
    #[cfg(feature = "selftest")]
    grow_test(root_ns);
    // fs-server-rw Part E: create a brand-new file and confirm inode + dir entry persist.
    #[cfg(feature = "selftest")]
    create_test(root_ns);
    // auth+session Part B: resolve through a subtree-scoped binding (the shared-reg
    // bind-mount from mount_one) and confirm it reaches the right file.
    #[cfg(feature = "selftest")]
    subtree_bind_test(root_ns);

    // The display arm's guest-side gate runs as its own program
    // (`display-selftest`), not inline here: compositing is not init's job, and
    // `userspace/init/CLAUDE.md` calls this critical-path code. It supersedes the
    // inline framebuffer demo that proved M1 Part B.
    // The input server first, and **the order is load-bearing**: the compositor resolves
    // `/dev/input/new` during its own startup, before it answers `Meta::Ready`. Spawned the
    // other way round it would find nothing bound and serve the display with no input, for
    // the life of the boot, with only a log line to say so. Not fatal either way — a machine
    // with no i8042 has no raw nodes, the server says so and exits, and everything else
    // comes up normally.
    if !bind_input_server(root_ns) {
        kprint(b"init: no input server; /dev/input/new unavailable\n");
    }

    if !bind_compositor(root_ns) {
        kprint(b"init: no compositor; /dev/draw unavailable\n");
    }

    #[cfg(feature = "selftest")]
    run_display_selftest(notif, root_ns);

    // After the compositor is serving: the first client. Its committed buffer is what
    // `check-display` compares, so it runs last and leaves the scene on screen.
    #[cfg(feature = "selftest")]
    run_ui_testclient(root_ns);

    // The input client reads `/dev/input/raw/*`, which the i8042 driver published at boot.
    // It parks its reads and announces `listening`; `cargo xtask check-input` injects from
    // the host once it sees that line.
    #[cfg(feature = "selftest")]
    run_input_testclient(root_ns);

    // Spawn the system profile server and bind it at `/bin` (per init CLAUDE.md step 4).
    // Critical-path: without `/bin`, no program resolves for the services init launches.
    if !bind_profile_server(root_ns) {
        emergency(notif, root_ns);
    }

    // Spawn the system logging service and bind it at `/log`, before the service manager,
    // so services can resolve `/log/<tier>/<principal>` and log from launch. Critical-path.
    if !bind_logging_service(root_ns) {
        emergency(notif, root_ns);
    }

    // The terminal server, after `/bin` (it is spawned from there) and deliberately **not**
    // critical-path: a boot without it still reaches `eshell`, which holds the raw console
    // for exactly the case where this server is absent.
    if !bind_tty_server(root_ns) {
        kprint(b"init: no terminal server; sessions will have no /dev/tty\n");
    }

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
