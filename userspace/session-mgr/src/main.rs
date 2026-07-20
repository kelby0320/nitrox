//! `session-mgr` — the userspace **session manager** (Phase 3, auth + session slice).
//!
//! The Tier-5 supervisor that logs a user in and hands them a sandboxed shell. It
//! holds re-delegated `BIND_NAMESPACE` (from service-mgr) and the building-block
//! endpoints — the fs-server forwarding endpoint + a channel to auth-service — from
//! which it composes each login's per-user namespace. See
//! `docs/architecture/session-and-auth.md`.
//!
//! **Part D (this file):** the plumbing is proven end to end — receive the handed-over
//! endpoints, authenticate the demo user against auth-service over the auth channel
//! (the first real exercise of the credential stack under spawning), and construct a
//! session namespace binding the user's `/home` as a subtree of the fs-server (proving
//! `BIND_NAMESPACE` + subtree scoping + shared registration). **Part E** replaces the
//! hardcoded round-trip with an interactive `login:` prompt and spawns the user shell
//! into the constructed namespace.
//!
//! `#![no_std]` + `#![no_main]`, **no `alloc`** — fixed `.bss` buffers, no
//! `#[global_allocator]`. `libkern` + `librsproto` (the Auth codec + envelope).
//! See `userspace/session-mgr/CLAUDE.md`.

#![no_std]
#![no_main]

use libkern::*;
use librsproto::auth::{build_authenticate_request, parse_authenticate_reply};
use librsproto::{OP_AUTHENTICATE, decode, encode};

/// IPC payload starts at offset 24 in the `IpcMsg` (after the 24-byte header).
const PAYLOAD_OFF: usize = 24;
const MSG_LEN: usize = 4096;

/// The demo credential the test-harness auto-login uses. **Must match** the fixture
/// seeded into `/system/users` by `tools/xtask` (`DEMO_USER`/`DEMO_PASSWORD`). Only the
/// deterministic test-harness path uses it; the interactive login reads the credential
/// from the console instead.
#[cfg(feature = "test-harness")]
const DEMO_USER: &[u8] = b"alice";
#[cfg(feature = "test-harness")]
const DEMO_PASSWORD: &[u8] = b"correct horse battery staple";

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
static mut SPAWN_USERSH: SpawnArgs = SpawnArgs {
    image: 0,
    handle_count: 0,
    move_mask: 0,
    arg0: 0,
    handles: [0; 4],
    rights: [0; 4],
    namespace: 0, // set at spawn = the session namespace
    syscaps: 0,   // empty — the shell is sandboxed
};

/// Emit `msg` to the serial console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Spin forever (session-mgr has nothing more to do in Part D once the checks run;
/// Part E adds the login loop).
fn idle() -> ! {
    loop {
        // SAFETY: `pause` is always valid in ring 3 with no effects.
        unsafe { core::arch::asm!("pause", options(nomem, nostack)) };
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
fn build_session_namespace(root_ns: u64, fs_endpoint: u64, home: &[u8]) -> u64 {
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
    ns
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

/// Spawn the user shell (`/initramfs/sbin/usersh`) into `session_ns` (empty syscaps),
/// then block on `notif` for its `ChildExited` and return its exit code. `-1` if the
/// shell could not be spawned. This is the login's payoff: an unprivileged process in a
/// per-user namespace, reaped by session-mgr.
fn spawn_user_shell(root_ns: u64, session_ns: u64, notif: u64) -> i32 {
    let image = ns_lookup(root_ns, b"/initramfs/sbin/usersh", RIGHT_MAP_READ).1;
    if image == 0 {
        kprint(b"session-mgr: usersh image not found\n");
        return -1;
    }
    // SAFETY: SPAWN_USERSH is a valid writable arg block; run in the session namespace.
    let h = unsafe {
        SPAWN_USERSH.image = image;
        SPAWN_USERSH.namespace = session_ns;
        syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_USERSH) as u64)
    };
    // The kernel copied the ELF during spawn; close our image handle.
    // SAFETY: closing our own handle.
    unsafe { syscall1(SYS_HANDLE_CLOSE, image) };
    if h < 0 {
        kprint(b"session-mgr: usersh spawn FAIL\n");
        return -1;
    }
    kprint(b"session-mgr: user shell spawned into the session namespace\n");
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
    // Receive the handed-over endpoints, in order: (1) fs-server endpoint, (2) auth channel.
    let fs_endpoint = recv_handoff(control);
    let auth_ch = recv_handoff(control);
    if fs_endpoint == 0 || auth_ch == 0 {
        kprint(b"session-mgr: endpoint handoff FAIL\n");
        verdict(false);
        idle();
    }
    kprint(b"session-mgr: received fs endpoint + auth channel\n");

    // The session loop: authenticate a user, construct their per-user namespace, spawn
    // the shell into it, and reap it. (Part D auto-logs-in the demo user for a
    // deterministic verdict; the interactive path reads the credential from console.)
    let mut home = [0u8; 256];
    match login(root_ns, auth_ch, &mut home) {
        Some(hl) => {
            kprint(b"session-mgr: login ok -> home=");
            kprint(&home[..hl]);
            kprint(b"\n");
            let session_ns = build_session_namespace(root_ns, fs_endpoint, &home[..hl]);
            if session_ns == 0 {
                verdict(false);
                idle();
            }
            kprint(b"session-mgr: session namespace built (/home subtree + /dev/console)\n");
            // The payoff: an unprivileged shell in the per-user namespace writes to home.
            let code = spawn_user_shell(root_ns, session_ns, notif);
            let ok = code == 0;
            if ok {
                kprint(b"session-mgr: user shell wrote to home + exited cleanly (login proven)\n");
            } else {
                kprint(b"session-mgr: user shell failed\n");
            }
            verdict(ok);
        }
        None => {
            kprint(b"session-mgr: login denied\n");
            verdict(false);
        }
    }
    idle();
}

/// Authenticate a user, returning their home path (copied into `home_out`) length, or
/// `None` if denied. **test-harness**: a wrong-password sanity check, then auto-login of
/// the demo user (deterministic verdict). **interactive**: prompt username + password on
/// the console (up to a few attempts).
#[cfg(feature = "test-harness")]
fn login(_root_ns: u64, auth_ch: u64, home_out: &mut [u8]) -> Option<usize> {
    // Sanity: a wrong password must be denied (no enumeration/timing oracle upstream).
    let mut scratch = [0u8; 256];
    if authenticate(auth_ch, DEMO_USER, b"not-the-password", &mut scratch).is_some() {
        kprint(b"session-mgr: wrong password WRONGLY accepted\n");
        return None;
    }
    kprint(b"session-mgr: wrong password correctly denied\n");
    authenticate(auth_ch, DEMO_USER, DEMO_PASSWORD, home_out)
}

#[cfg(not(feature = "test-harness"))]
fn login(root_ns: u64, auth_ch: u64, home_out: &mut [u8]) -> Option<usize> {
    let (cst, console) = ns_lookup(root_ns, b"/dev/console", RIGHT_READ);
    if cst != 0 || console == 0 {
        kprint(b"session-mgr: no console for login\n");
        return None;
    }
    // A one-page read buffer for console input.
    let buf_h = unsafe { syscall4(SYS_MEMORY_CREATE, 4096, 0, 0, 0) };
    if buf_h < 0 {
        return None;
    }
    let buf_h = buf_h as u64;
    let buf_addr = unsafe { syscall4(SYS_MEMORY_MAP, buf_h, 0, 4096, RIGHT_MAP_READ | RIGHT_MAP_WRITE) };
    if buf_addr < 0 {
        return None;
    }
    let buf_addr = buf_addr as u64;
    for _ in 0..3 {
        kprint(b"\r\nnitrox login: ");
        let mut user = [0u8; 64];
        let ulen = read_line(console, buf_h, buf_addr, &mut user, true);
        kprint(b"password: ");
        let mut pass = [0u8; 128];
        let plen = read_line(console, buf_h, buf_addr, &mut pass, false);
        kprint(b"\r\n");
        if let Some(hl) = authenticate(auth_ch, &user[..ulen], &pass[..plen], home_out) {
            return Some(hl);
        }
        kprint(b"login incorrect\r\n");
    }
    None
}

/// Read a line from `console` into `out` (until CR/LF), echoing each byte iff `echo`.
/// Returns the line length. `buf`/`buf_addr` are a shared one-page read buffer.
#[cfg(not(feature = "test-harness"))]
fn read_line(console: u64, buf: u64, buf_addr: u64, out: &mut [u8], echo: bool) -> usize {
    let mut len = 0usize;
    loop {
        let op = IoOp { opcode: IO_OPCODE_READ, flags: 0, buffer: buf, buf_offset: 0, offset: 0, length: 256 };
        // SAFETY: `console` is a char DeviceNode with READ; `&op` is a valid IoOp.
        let po = unsafe { syscall2(SYS_IO_SUBMIT, console, (&op as *const IoOp) as u64) };
        if po < 0 {
            continue;
        }
        if !wait_one(po as u64) {
            unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
            continue;
        }
        let (status, n) = unsafe {
            (
                i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
                u64::from_le_bytes([
                    WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                    WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
                ]),
            )
        };
        // SAFETY: closing our own PO.
        unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
        if status != 0 {
            continue;
        }
        for i in 0..(n as usize).min(256) {
            // SAFETY: `buf_addr + i` is within the mapped read buffer.
            let b = unsafe { ((buf_addr + i as u64) as *const u8).read_volatile() };
            match b {
                b'\r' | b'\n' => return len,
                0x08 | 0x7F => {
                    if len > 0 {
                        len -= 1;
                        if echo {
                            kprint(b"\x08 \x08");
                        }
                    }
                }
                0x20..=0x7E => {
                    if len < out.len() {
                        out[len] = b;
                        len += 1;
                        if echo {
                            kprint(&[b]);
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Fire the boot verdict under `test-harness` (terminates QEMU via `SYS_TEST_EXIT`);
/// a no-op otherwise. session-mgr is the final gate of the self-test boot in Part D.
#[cfg(feature = "test-harness")]
fn verdict(ok: bool) {
    let code = if ok { TEST_EXIT_SUCCESS } else { TEST_EXIT_FAILURE };
    kprint(if ok {
        b"session-mgr: test-harness verdict PASS\n"
    } else {
        b"session-mgr: test-harness verdict FAIL\n"
    });
    // SAFETY: SYS_TEST_EXIT takes the verdict in a0; under the kernel test-harness build
    // it writes isa-debug-exit and QEMU terminates (so this does not return in practice).
    unsafe { syscall1(SYS_TEST_EXIT, code as u64) };
}

/// No-op verdict outside the test harness (a normal / interactive boot).
#[cfg(not(feature = "test-harness"))]
fn verdict(_ok: bool) {}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"session-mgr: PANIC\n");
    idle();
}
