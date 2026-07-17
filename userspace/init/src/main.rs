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
/// Spawn args for the demo `parent`: no handles, inherit a LOOKUP-only handle to
/// init's root namespace (so parent can resolve the kernel servers but not bind
/// into init's root — it constructs its own namespaces for its children, which is
/// why init grants it `BIND_NAMESPACE`).
#[cfg(feature = "selftest")]
static mut SPAWN_PARENT: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/parent
    handle_count: 0,
    move_mask: 0,
    arg0: 0,
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0,
    syscaps: SYSCAP_BIND_NAMESPACE, // parent constructs namespaces for its children
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
/// Only in a non-`selftest` build: a selftest boot runs the demo chain instead of
/// handing off to service-mgr.
#[cfg(not(feature = "selftest"))]
static mut SPAWN_SERVICE_MGR: SpawnArgs = SpawnArgs {
    image: 0, // resolved at spawn from /initramfs/sbin/service-mgr
    handle_count: 0,
    move_mask: 0,
    arg0: 0,
    handles: [0; 4],
    rights: [0; 4],
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
    unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
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

    // 4. Bind the forwarding endpoint at `/bin`. The kernel adopts the IpcChannel as a
    //    Userspace Server; the binding takes its own reference, so init closes its
    //    endpoint handle after.
    // SAFETY: valid namespace handle + path pointer + endpoint handle.
    let br = unsafe {
        syscall4(SYS_NS_BIND, root_ns, b"/bin".as_ptr() as u64, 4, endpoint)
    };
    // SAFETY: closing init's endpoint handle (the binding holds its own reference).
    unsafe { syscall1(SYS_HANDLE_CLOSE, endpoint) };
    if br != 0 {
        kprint(b"init: profile-server bind FAIL at /bin\n");
        return false;
    }

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
        spawn_program(root_ns, b"/initramfs/sbin/logging-service", &raw mut SPAWN_LOGGING)
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
#[cfg(not(feature = "selftest"))]
fn spawn_service_mgr(root_ns: u64) -> i64 {
    kprint(b"init: handing off to service manager\n");
    // SAFETY: SPAWN_SERVICE_MGR is a valid writable arg block.
    let h =
        unsafe { spawn_program(root_ns, b"/initramfs/sbin/service-mgr", &raw mut SPAWN_SERVICE_MGR) };
    if h < 0 {
        kprint(b"init: service-mgr spawn FAIL\n");
    }
    h
}

/// The healthy supervise path. **Normally**, hand off to the service manager: spawn
/// it and supervise it via [`reap_loop`] (if service-mgr exits — a critical fault —
/// reap_loop drops to the emergency console as the interim recovery, until a reboot
/// path exists; see `docs/architecture/service-manager.md` § Recovery). **Under
/// `selftest`**, run the Phase-1/2 demo chain (`parent`) to completion FIRST, then
/// launch the interactive shell — they share the single-outstanding-command disk and
/// the serial console, so overlapping them corrupts the fs-server's reads; eshell is
/// launched once `parent` reaps (in [`reap_loop`]).
fn supervise(notif: u64, root_ns: u64) -> ! {
    #[cfg(feature = "selftest")]
    {
        kprint(b"init: spawning parent (slice-1/2/3 demo chain)\n");
        // SAFETY: SPAWN_PARENT is a valid writable arg block.
        let parent_h =
            unsafe { spawn_program(root_ns, b"/initramfs/sbin/parent", &raw mut SPAWN_PARENT) };
        if parent_h >= 0 {
            // reap_loop launches eshell once `parent` (the only exiting child) reaps.
            reap_loop(notif, root_ns, parent_h);
        }
        kprint(b"init: parent spawn FAIL\n");
        // Test-harness: couldn't even launch the demo chain — fail the run.
        #[cfg(feature = "test-harness")]
        test_exit(false);
        // Selftest parent-spawn-failure fallback: the console.
        spawn_eshell(root_ns);
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
/// `parent` under `selftest`, or `service-mgr` on a normal boot — or `0` if none is
/// pending. When it reaps, init hands off to the interactive console: the demo-done
/// handoff under selftest, or the emergency-recovery fallback if service-mgr died
/// (interim, until a reboot path exists). All other orphans are logged and released.
fn reap_loop(notif: u64, root_ns: u64, mut parent_h: i64) -> ! {
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
                // Release init's reference to the exited child. When the demo
                // `parent` reaps, launch the interactive console. Reparented orphans
                // have no handle here — the kernel tears them down; init observes.
                if parent_h != 0 {
                    // SAFETY: closing our own process handle.
                    unsafe { syscall1(SYS_HANDLE_CLOSE, parent_h as u64) };
                    parent_h = 0;
                    // Test-harness: `parent` reaping ends the self-test chain — report
                    // the verdict (PASS iff it exited cleanly), which terminates QEMU.
                    // If it doesn't (no exit device), fall through to the console.
                    #[cfg(feature = "test-harness")]
                    test_exit(code == 0);
                    spawn_eshell(root_ns);
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
