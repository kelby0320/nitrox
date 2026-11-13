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
use libkern::{
    KError, KIND_CHILD_EXITED, KOBJ_SYSTEM_CONTROL, Rights, SysCaps, TERMINAL_OP_EMERGENCY, TERMINAL_OP_FINISH,
};
use libos::FLUSH_TIMEOUT_NS;
use libos::boot::Start;
use libos::log::{self, Line};
use libos::system::{self, Stop};
use libos::{
    Carried, Channel, Deadline, File, Handle, MapRead, Msg, Namespace, Notify, NsMutable, Only, Process, ReadOnly,
    Resource, Spawn, WaitSet, WriteOnly, block_on,
};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// One page, which `/system/current-generation` fits in.
const PAGE: usize = 4096;

/// Bounded wait for an fs-server's Ready: init must not wait forever for a server.
const READY_TIMEOUT_NS: u64 = 30_000_000_000; // 30 s

/// Init's root namespace, with every right: it binds the mounts and `/bin` in it.
type Root = Handle<Namespace, NsMutable>;
type Endpoint = Handle<Channel, Only>;

/// What a shutdown's [`finish`] needs: init's mounts, as made, and the system-control object.
struct Finish {
    mounts: Vec<Kept>,
    system_control: Option<Handle<Resource, WriteOnly>>,
}

/// How long a filesystem server may take to answer `Meta::Unmount`: its write-back came first, so
/// what is left is one superblock write.
const UNMOUNT_TIMEOUT_NS: u64 = 10_000_000_000;

struct Kept {
    mount_point: String,
    control: Endpoint,
}

/// What an `fs-server-ext4` and the system `profile-server` are granted: the control endpoint,
/// their `rdx`. They share init's root, `LOOKUP` only, and hold no ambient capabilities.
const SERVER_CONTROL: Rights =
    Rights::from_bits(Rights::SEND.bits() | Rights::RECV.bits() | Rights::TRANSFER.bits() | Rights::WAIT.bits());
/// The service manager's **handoff channel** end: init sends the bind-righted root, then the
/// fs-server and profile-server endpoints, so it carries `TRANSFER`, to hand those on, and
/// `SEND`/`RECV`/`WAIT`.
const SERVICE_MGR_HANDOFF: Rights = Rights::from_bits(SERVER_CONTROL.bits() | Rights::DUPLICATE.bits());
/// The service manager's capabilities: `BIND_NAMESPACE`, its supervisor capability, and
/// `SYSTEM_CLOCK`, only to pass to the view broker.
const SERVICE_MGR_CAPS: SysCaps =
    SysCaps::from_bits_truncate(SysCaps::BIND_NAMESPACE.bits() | SysCaps::SYSTEM_CLOCK.bits());

/// Resolve `path` to its ELF image and spawn it, granting `grant` as its `rdx`: the child, or
/// `None`. The image closes as it drops; the child has its own copy. A refused spawn hands its
/// grant back, which closes here.
fn spawn_program(
    root: &Root,
    path: &str,
    grant: Option<(Endpoint, Rights)>,
    caps: SysCaps,
) -> Option<Handle<Process, Only>> {
    let Ok(image) = block_on(root.lookup::<File, MapRead>(path, Rights::MAP_READ | Rights::INSPECT)) else {
        Line::new().s(b"init: image not found: ").s(path.as_bytes()).end();
        return None;
    };
    let mut spawn = Spawn::new(&image);
    if let Some((h, rights)) = grant {
        // Refused only past the kernel's handle count, which one grant is not.
        let _ = spawn.grant(h, rights);
    }
    spawn.syscaps(caps);
    spawn.spawn().ok()
}

/// Read and parse `/initramfs/etc/init.toml`, logging the mount plan; `None` on any failure.
fn read_manifest(root: &Root) -> Option<Manifest> {
    // `INSPECT` as well as `MAP_READ`: the object's size comes from its stat.
    let lookup = root.lookup::<File, MapRead>("/initramfs/etc/init.toml", Rights::MAP_READ | Rights::INSPECT);
    let Ok(mem) = block_on(lookup) else {
        log::write(b"init: /initramfs/etc/init.toml not found (would drop to eshell)\n");
        return None;
    };
    // **The whole object, as the storage service maps it**: a page would cut a longer manifest.
    // Copied out, as nothing borrows a mapping's bytes; the mapping and the handle go as they drop.
    // Reserved fallibly: an absurd size is a failure to read, not a heap panic in PID 1.
    let read = mem.stat().ok().and_then(|info| {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(info.size as usize).ok()?;
        bytes.resize(info.size as usize, 0);
        mem.map(bytes.len()).and_then(|m| m.read_into(0, &mut bytes)).ok().map(|()| bytes)
    });
    let Some(bytes) = read else {
        log::write(b"init: init.toml map FAIL\n");
        return None;
    };
    match manifest::text(&bytes) {
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
                log::write(b"init: init.toml parse error (would drop to eshell)\n");
                None
            }
        },
        Err(_) => {
            log::write(b"init: init.toml not UTF-8 (would drop to eshell)\n");
            None
        }
    }
}

/// Process the manifest: every mount shallowest first, then every bind; `true` iff all succeeded. A
/// failure is critical-path: a bind that silently did not happen is how a later lookup fails for a
/// reason nobody can see. **Every mount's endpoint is held until the binds are done**, as a bind's
/// source may be any mount; then the root's is kept for `service-mgr` and the rest closed.
fn mount_all(root: &Root, manifest: &Manifest, kept: &mut Vec<Kept>) -> (bool, Option<Endpoint>) {
    let mut ok = true;
    let mut endpoints: Vec<(&str, Endpoint)> = Vec::new();
    for m in &manifest.mounts {
        match mount_one(root, m, kept) {
            Some(endpoint) => endpoints.push((m.mount_point.as_str(), endpoint)),
            None => {
                Line::new().s(b"init: mount FAILED for ").s(m.mount_point.as_bytes()).end();
                ok = false;
            }
        }
    }
    for b in &manifest.binds {
        if !bind_one(root, b, &endpoints) {
            Line::new().s(b"init: bind FAILED for ").s(b.path.as_bytes()).end();
            ok = false;
        }
    }
    // The rest close as they drop; each binding holds its own reference.
    let fs_endpoint = endpoints.into_iter().find(|(mount_point, _)| *mount_point == "/").map(|(_, e)| e);
    (ok, fs_endpoint)
}

/// Bind one `[[bind]]`: the source mount's endpoint again at `b.path`, scoped to `b.subtree`. The
/// kernel shares the source's server registration across both names rather than minting a rival.
fn bind_one(root: &Root, b: &BindSpec, endpoints: &[(&str, Endpoint)]) -> bool {
    // The parser checked the source names a mount; it is absent only if that mount failed.
    let Some((_, endpoint)) = endpoints.iter().find(|(point, _)| *point == b.source) else {
        return false;
    };
    // `/` scopes to the whole tree, which the kernel spells as no base at all.
    let base = if b.subtree == "/" { "" } else { b.subtree.as_str() };
    if root.bind_server(&b.path, endpoint.as_ref(), base).is_err() {
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
/// server's endpoint, bound at `m.mount_point`; the caller owns it. Whatever fails, the handles
/// made for it close as they drop.
fn mount_one(root: &Root, m: &MountSpec, kept: &mut Vec<Kept>) -> Option<Endpoint> {
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
    let rights = Rights::from_bits(init::device_rights(m.mode));
    let Ok(device) = block_on(root.lookup::<Resource, ReadOnly>(&dev_path, rights)) else {
        Line::new().s(b"init: device ").s(dev_path.as_bytes()).s(b" not found").end();
        return None;
    };

    let (ctrl_init, ctrl_srv) = Handle::pair(4).ok()?;
    // Its process handle closes as it drops, so an exited server's address space is freed.
    let grant = Some((ctrl_srv, SERVER_CONTROL));
    if spawn_program(root, "/initramfs/sbin/fs-server-ext4", grant, SysCaps::empty()).is_none() {
        log::write(b"init: fs-server spawn FAIL\n");
        return None;
    }

    // Setup message: the device and a flags byte (read-only for `"ro"`); NoBlock, ring empty.
    let mut carried = Carried::new();
    if carried.push(device).is_err() || send_msg(&ctrl_init, &[m.mode.setup_flags()], &mut carried).is_err() {
        log::write(b"init: device handoff FAIL\n");
        return None;
    }

    let who: [&[u8]; 4] = [b"fs-server-ext4 for ", m.mount_point.as_bytes(), b" on ", m.device.as_bytes()];
    let endpoint = wait_ready(&ctrl_init, &who)?;
    // The control channel is kept for a shutdown's `Meta::Unmount` ([`finish`]).

    // Bind the endpoint at the mount point; `mount_all` may bind it again for a `[[bind]]`.
    if root.bind_server(&m.mount_point, endpoint.as_ref(), "").is_err() {
        Line::new().s(b"init: bind FAIL at ").s(m.mount_point.as_bytes()).end();
        return None;
    }
    kept.push(Kept { mount_point: m.mount_point.clone(), control: ctrl_init });
    Line::new().s(b"init: mounted fs-server-ext4 at ").s(m.mount_point.as_bytes()).end();
    Some(endpoint)
}

/// Wait (bounded) for a resource server's `Meta::Ready` on `ctrl` and return the endpoint it
/// transfers. `None` if there is none — and then **init has already said why**, naming the server
/// as the `who` pieces spell it: the four failures (too slow, exited first, sent something else,
/// **refused** with a reason) are four problems on a machine nobody can attach a debugger to.
fn wait_ready(ctrl: &Endpoint, who: &[&[u8]]) -> Option<Endpoint> {
    let say = |what: &[u8]| {
        let mut line = Line::new();
        line.s(b"init: ");
        for piece in who {
            line.s(piece);
        }
        line.s(what);
        line
    };

    let deadline = Deadline::after(READY_TIMEOUT_NS).unwrap_or(Deadline::at(READY_TIMEOUT_NS));
    let mut set = WaitSet::<1>::new();
    let waited = set.add(ctrl).and_then(|_| set.wait(deadline));
    match waited {
        Ok(s) if s.is_empty() => {
            say(b" sent no Ready within ").u(READY_TIMEOUT_NS / 1_000_000_000).s(b" s").end();
            return None;
        }
        Ok(_) => {}
        Err(e) => {
            say(b": waiting for its Ready failed, error ").i(e.code() as i64).end();
            return None;
        }
    }
    // A Ready's endpoint is the one handle kept; any other a server sends closes as `sent` drops,
    // where it would stay in PID 1's table for good.
    let mut msg = Msg::new();
    let mut sent = match ctrl.recv(&mut msg) {
        Ok(sent) => sent,
        // A queued message is received before the closed peer is: this is a server that exited
        // without sending anything, not one that refused and then exited.
        Err(e) if e.code() == KError::PeerClosed => {
            say(b" exited without sending Ready").end();
            return None;
        }
        Err(e) => {
            say(b": its Ready could not be received, error ").i(e.code() as i64).end();
            return None;
        }
    };
    let first = init::ready::parse(msg.payload(), sent.len());
    match first {
        init::ready::First::Ready => {}
        init::ready::First::Refused(reason) if !reason.is_empty() => say(b" refused: ").untrusted(reason).end(),
        init::ready::First::Refused(_) => say(b" refused, giving no reason init could read").end(),
        init::ready::First::Unexpected => say(b" sent something other than a Ready").end(),
    }
    init::ready::kept(&first).and_then(|i| sent.take(i))
}

/// Spawn the system profile server and bind its endpoint at `/bin`: [`mount_one`] minus the device.
/// It answers forwarded `/bin/<prog>` resolves with the matching `/store/.../bin/<prog>` handle.
/// A failure is critical-path: without `/bin`, no program resolves for the services.
/// Returns the endpoint, kept for `service-mgr` to carry to session-mgr.
fn bind_profile_server(root: &Root) -> Option<Endpoint> {
    let (ctrl_init, ctrl_srv) = Handle::pair(4).ok()?;
    let grant = Some((ctrl_srv, SERVER_CONTROL));
    if spawn_program(root, "/initramfs/sbin/profile-server", grant, SysCaps::empty()).is_none() {
        log::write(b"init: profile-server spawn FAIL\n");
        return None;
    }
    // Its control endpoint closes once the Ready is in, as nothing more comes down it.
    let endpoint = wait_ready(&ctrl_init, &[b"profile-server".as_slice()])?;
    drop(ctrl_init);

    // Keep a second handle to the endpoint *before* binding, for service-mgr to carry to
    // session-mgr, so a failure is a failure to bind at all. `TRANSFER | DUPLICATE` is all the
    // hand-down needs.
    let Ok(retained) = endpoint.duplicate(Rights::TRANSFER | Rights::DUPLICATE) else {
        log::write(b"init: profile endpoint duplicate FAIL\n");
        return None;
    };

    // Bind the endpoint at `/bin`; the retained duplicate keeps the *same* endpoint alive, so a
    // later bind shares this registration. **Scoped to `/bin`, not whole-tree**: the server tells a
    // package's projected directories apart by the suffix's first part, so unscoped,
    // `/bin/applications` would reach the *applications* projection. Init's endpoint handle closes
    // as it drops: the binding holds its own reference.
    if root.bind_server("/bin", endpoint.as_ref(), "/bin").is_err() {
        log::write(b"init: profile-server bind FAIL at /bin\n");
        return None;
    }
    log::write(b"init: profile server bound at /bin\n");
    Some(retained)
}

/// Read and log `/system/current-generation` through the root fs-server: the stack end to end.
fn read_current_generation(root: &Root) {
    let Ok(mem) = block_on(root.lookup::<File, MapRead>("/system/current-generation", Rights::MAP_READ)) else {
        log::write(b"init: /system/current-generation lookup FAIL\n");
        return;
    };
    // A page of the file's bytes and zero padding; the tail trimmed.
    let mut bytes = [0u8; PAGE];
    if mem.map(PAGE).and_then(|m| m.read_into(0, &mut bytes)).is_err() {
        log::write(b"init: current-generation map FAIL\n");
        return;
    }
    let len = bytes.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    log::write(b"init: /system/current-generation = ");
    log::write(&bytes[..len]); // the file content ends in '\n'
}

/// Report the run's verdict to `xtask test-qemu`, which ends QEMU under the kernel's
/// `test-harness` feature. Returns, so a missing exit device degrades to a normal boot, not a hang.
/// **Unconditional, in every build**: elsewhere the kernel refuses it, on a path that is already a
/// boot failure. `init` only ever fires **FAIL**; PASS is `boot-probe`'s.
fn test_exit(ok: bool) {
    log::write(if ok { b"init: test-harness verdict PASS\n" } else { b"init: test-harness verdict FAIL\n" });
    system::test_verdict(ok);
}

fn spawn_eshell(root: &Root) {
    log::write(b"init: starting interactive console (eshell)\n");
    // init's handle closes as it drops; eshell runs independently.
    if spawn_program(root, "/initramfs/sbin/eshell", None, SysCaps::empty()).is_none() {
        log::write(b"init: eshell spawn FAIL\n");
    }
}

/// Spawn the service manager, the normal boot handoff, and hand it the endpoints: the terminal
/// channel, or `None`. Its process handle closes at once: the channel closing names its death.
/// Only one grant reaches a child (`rdx`), so the endpoints travel on a channel.
fn spawn_service_mgr(root: &Root, fs_endpoint: Option<Endpoint>, profile_endpoint: Endpoint) -> Option<Endpoint> {
    log::write(b"init: handing off to service manager\n");
    // **The terminal channel.** Depth 4: three handoffs, queued before `service-mgr` runs — a ring
    // shorter than the sends drops the last one silently — and then the ops it sends back.
    let Ok((init_end, child_end)) = Handle::pair(4) else {
        log::write(b"init: service-mgr handoff channel FAIL\n");
        return None;
    };
    // A failed spawn moved nothing: both ends close as they drop, with the endpoints they would
    // have carried.
    let grant = Some((child_end, SERVICE_MGR_HANDOFF));
    if spawn_program(root, "/bin/service-mgr", grant, SERVICE_MGR_CAPS).is_none() {
        log::write(b"init: service-mgr spawn FAIL\n");
        return None;
    }
    // The handoffs, in the order service-mgr receives them: 1. **the root, with init's own rights**
    // (`BIND` and `UNBIND`: a spawned process gets only a lookup-only root, and `service-mgr` binds
    // the servers); 2. the root filesystem's endpoint; 3. the profile server's.
    let root_copy = root.duplicate(Rights::from_bits(u64::MAX)).ok();
    if root_copy.is_none() {
        log::write(b"init: root handle duplicate FAIL -- service-mgr will bind nothing\n");
    }
    send_handle(&init_end, root_copy);
    send_handle(&init_end, fs_endpoint);
    send_handle(&init_end, Some(profile_endpoint));
    Some(init_end)
}

/// Send `payload` and `carried`, unframed; on a refusal nothing moved, and `carried` holds it.
fn send_msg(ch: &Endpoint, payload: &[u8], carried: &mut Carried) -> libos::Result<()> {
    let mut msg = Msg::new();
    msg.set(payload)?;
    // NoBlock: each ring it sends on is sized to hold what goes down it.
    ch.send(&msg, carried)
}

/// Transfer one `handle` over a handoff channel: an IPC message with a single moved handle and no
/// payload. One refused is closed here, and in its place, as for `None`, goes **an empty
/// message**, not nothing: the receiver reads the handoffs positionally.
fn send_handle<T, M>(ctrl: &Endpoint, handle: Option<Handle<T, M>>) {
    let mut carried = Carried::new();
    let Some(handle) = handle else {
        let _ = send_msg(ctrl, &[], &mut carried);
        return;
    };
    if carried.push(handle).is_ok() && send_msg(ctrl, &[], &mut carried).is_ok() {
        return;
    }
    log::write(b"init: handoff send FAIL\n");
    drop(carried);
    let _ = send_msg(ctrl, &[], &mut Carried::new());
}

/// The healthy supervise path: spawn `service-mgr` and supervise it via [`reap_loop`].
fn supervise(
    notify: Option<Handle<Notify, Only>>,
    root: Root,
    fs_endpoint: Option<Endpoint>,
    profile_endpoint: Endpoint,
    held: Finish,
) -> ! {
    let Some(terminal) = spawn_service_mgr(&root, fs_endpoint, profile_endpoint) else {
        // No `service-mgr` is no server at all (`auth-service`, `logging-service`): console free.
        emergency(notify, root, held);
    };
    reap_loop(notify, root, Some(terminal), held);
}

/// The **emergency** path: a critical-path boot failure. Drop straight to the interactive shell
/// so the operator can inspect the broken system (`userspace/init/CLAUDE.md` § Failure → eshell).
fn emergency(notify: Option<Handle<Notify, Only>>, root: Root, held: Finish) -> ! {
    log::write(b"init: critical-path failure -- dropping to emergency shell\n");
    // A critical-path boot failure is a failed test run. Outside `test-qemu` the verdict device is
    // absent, the kernel refuses the verdict, and the boot goes on to the shell, as on hardware.
    test_exit(false);
    spawn_eshell(&root);
    reap_loop(notify, root, None, held);
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
fn reap_loop(notify: Option<Handle<Notify, Only>>, root: Root, mut terminal: Option<Endpoint>, mut held: Finish) -> ! {
    log::write(b"init: entering reaping loop\n");
    let mut shell_started = false;
    // A `Finish` that returned: the machine could not stop itself. A second changes nothing.
    let mut finished = false;
    let mut msg = Msg::new();
    loop {
        let waited = {
            let mut set = WaitSet::<2>::new();
            if let Some(q) = &notify {
                let _ = set.add(q);
            }
            if let Some(ch) = &terminal {
                let _ = set.add(ch);
            }
            set.wait(Deadline::NEVER)
        };
        if waited.as_ref().is_ok_and(|s| s.is_empty()) || waited.is_err() {
            continue;
        }
        // Drained, until it answers nothing.
        while let Some(Ok(Some(n))) = notify.as_ref().map(|q| q.recv()) {
            let (kind, body) = (n.kind, n.body);
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
        while let Some(ch) = &terminal {
            let rr = ch.recv(&mut msg);
            if rr.as_ref().is_err_and(|e| e.code() == KError::PeerClosed) {
                Line::new()
                    .s(b"init: service-mgr has exited -- nothing supervises the services now, ")
                    .s(b"and the machine needs a restart")
                    .end();
                // Closed, so it is not waited on again.
                terminal = None;
                break;
            }
            let Ok(_sent) = rr else {
                break; // WouldBlock: nothing more
            };
            // An op and, for `Finish`, whether to reboot.
            let (op, reboot) = (msg.payload().first().copied(), msg.payload().get(1).map(|&b| b != 0));
            if op == Some(TERMINAL_OP_EMERGENCY) && !shell_started {
                Line::new()
                    .s(b"init: service-mgr asks for the emergency shell -- ")
                    .s(b"the boot cannot go on without what it could not start (it says what above)")
                    .end();
                // The same verdict a critical-path failure here gives: the boot failed.
                test_exit(false);
                spawn_eshell(&root);
                shell_started = true;
            }
            if let (Some(TERMINAL_OP_FINISH), Some(reboot)) = (op, reboot)
                && !finished
            {
                finished = true;
                finish(&root, reboot, &mut held);
            }
        }
    }
}

/// **Finish a shutdown.** `service-mgr` has asked every session and service to stop. What is left
/// is `init`'s own: its mounts, last first — each flushed, then told `Meta::Unmount`, on which its
/// server records the filesystem clean and exits — and then the machine, through `sys_power`.
/// Whatever one mount answers, the next is still unmounted, since one stopped half way leaves every
/// later filesystem not clean; whatever still runs writes after the flush at its own risk, as at a
/// power cut. **Each wait is bounded**: a flush by `FLUSH_TIMEOUT_NS`, an unmount's answer by
/// `UNMOUNT_TIMEOUT_NS`. Returns only if the machine cannot be stopped, after saying so.
fn finish(root: &Root, reboot: bool, held: &mut Finish) {
    log::write(if reboot {
        b"init: finishing the shutdown, to reboot: its own filesystems, last mounted first\n"
    } else {
        b"init: finishing the shutdown: its own filesystems, last mounted first\n"
    });
    // Taken whole, so that nothing unmounted is unmounted twice.
    for m in core::mem::take(&mut held.mounts).into_iter().rev() {
        unmount_one(root, m);
    }
    let Some(system_control) = &held.system_control else {
        Line::new()
            .s(b"init: no system-control object, so the machine cannot stop itself; ")
            .s(b"its filesystems are unmounted, and it is safe to turn off")
            .end();
        return;
    };
    let refused = system::stop(system_control, if reboot { Stop::Reboot } else { Stop::Halt });
    Line::new()
        .s(b"init: sys_power refused (")
        .i(refused.code() as i64)
        .s(b"), so the machine cannot stop itself; ")
        .s(b"its filesystems are unmounted, and it is safe to turn off")
        .end();
}

/// Unmount one of `init`'s mounts at shutdown, unless its flush did not finish. Its
/// control channel closes at the end: its server has exited or been abandoned.
fn unmount_one(root: &Root, m: Kept) {
    let at = m.mount_point.as_bytes();
    let deadline = Deadline::after(FLUSH_TIMEOUT_NS).unwrap_or(Deadline::at(FLUSH_TIMEOUT_NS));
    let sent = match init::unmount::flush_then_ask(root, &m.mount_point, (&m.control).into(), deadline) {
        Ok(sent) => sent,
        Err(e) => {
            let mut l = Line::new();
            l.s(b"init: ").s(at).s(b": its files could not all be written back (").i(e as i64);
            l.s(b"), so it is not marked clean").end();
            return;
        }
    };
    let mut msg = Msg::new();
    let answered = sent && {
        let deadline = Deadline::after(UNMOUNT_TIMEOUT_NS).unwrap_or(Deadline::at(UNMOUNT_TIMEOUT_NS));
        let mut set = WaitSet::<1>::new();
        set.add(&m.control).and_then(|_| set.wait(deadline)).is_ok_and(|s| !s.is_empty())
    };
    let received = answered && m.control.recv(&mut msg).is_ok();
    let mut line = Line::new();
    line.s(b"init: ").s(at);
    if received {
        use init::unmount::Answer;
        match init::unmount::answer(msg.payload()) {
            Answer::Done => line.s(b" unmounted, and its server left the filesystem clean"),
            Answer::Failed(why) => line.s(b" unmounted, but not left clean: ").untrusted(why),
            Answer::Unexpected => line.s(b" answered its unmount with something else"),
        };
    } else if sent {
        line.s(b" did not answer its unmount, and may not be left clean");
    } else {
        line.s(b": its server could not be told to unmount");
    }
    line.end();
}

/// **Keep the system-control object**, the capability to stop the machine, if `rdx` holds it: a
/// `SystemControl` handle with `WRITE`. A boot without it goes on, and says so: only a shutdown's
/// last step needs it. A handle in `rdx` that is not it closes here.
fn keep_system_control(h: Option<Handle<Resource, WriteOnly>>) -> Option<Handle<Resource, WriteOnly>> {
    let is_it = |h: &Handle<Resource, WriteOnly>| {
        h.stat().is_ok_and(|info| info.object_type == KOBJ_SYSTEM_CONTROL && info.rights & Rights::WRITE.bits() != 0)
    };
    match h {
        Some(h) if is_it(&h) => {
            log::write(b"init: holds the system-control object\n");
            Some(h)
        }
        _ => {
            log::write(b"init: no system-control object -- a shutdown cannot stop the machine\n");
            None
        }
    }
}

/// Registers: `rdi` = notification channel, `rsi` = root namespace, `rdx` = system-control object.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, root_ns: u64, system_control: u64, arg0: u64) -> ! {
    log::write(b"init: up (pid 1)\n");
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, root_ns, system_control, arg0) };
    let kept_control = keep_system_control(start.take_handle0());
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
    let notify = start.notify.take();
    // SAFETY: the root `Start` took, now owned here alone: PID 1's is the kernel's root namespace
    // with every right, `BIND` among them, where any other process's is `LOOKUP` only.
    let root: Option<Root> = start.root.take().map(|r| unsafe { Handle::from_raw(r.into_raw(), Rights::empty()) });
    let Some(root) = root else {
        log::write(b"init: no root namespace -- nothing can be mounted or started\n");
        test_exit(false);
        loop {
            libos::time::sleep_until(Deadline::NEVER);
        }
    };

    let mut mounts = Vec::new();
    let (booted, fs_endpoint) = match read_manifest(&root) {
        Some(manifest) => mount_all(&root, &manifest, &mut mounts),
        None => {
            log::write(b"init: no usable boot manifest\n");
            (false, None)
        }
    };
    let held = Finish { mounts, system_control: kept_control };
    if !booted {
        drop(fs_endpoint);
        emergency(notify, root, held);
    }

    read_current_generation(&root);

    let Some(profile_endpoint) = bind_profile_server(&root) else {
        drop(fs_endpoint);
        emergency(notify, root, held);
    };
    // **The servers are `service-mgr`'s to start**, as declarations; a `critical` one that does not
    // come up makes it ask over the terminal channel for the emergency shell, which `reap_loop`
    // starts. The test image's self-test, terminal and test clients are declarations too, which
    // `check-display` and `check-terminal` need without a login, so this file is identical in both
    // images; `nxterm` precedes `ui-testclient`, which raises its windows over the terminal's.
    supervise(notify, root, fs_endpoint, profile_endpoint, held);
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"init: PANIC\n");
    loop {
        // SAFETY: `pause` is always valid in ring 3 and has no effects.
        unsafe { asm!("pause", options(nomem, nostack)) };
    }
}
