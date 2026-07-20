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

/// The demo credential the Part-D self-check authenticates. **Must match** the fixture
/// seeded into `/system/users` by `tools/xtask` (`DEMO_USER`/`DEMO_PASSWORD`). This
/// hardcoded login is a throwaway Part-D proof — Part E reads the credential from the
/// console instead.
const DEMO_USER: &[u8] = b"alice";
const DEMO_PASSWORD: &[u8] = b"correct horse battery staple";

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut RECV_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut RECV_HANDLES: [u64; 8] = [0; 8];
static mut RECV_COUNT: usize = 0;
static mut SEND_MSG: [u8; MSG_LEN] = [0; MSG_LEN];
static mut SEND_HANDLES: [u64; 8] = [0; 8];

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
/// e.g. `/home/alice`): create a fresh namespace and bind the fs-server `fs_endpoint`
/// at `/home` **scoped to the user's home subtree**. Proves `BIND_NAMESPACE` (the bind
/// syscap gate), subtree scoping, and shared-registration bind-mount all at once.
/// Returns the session-namespace handle, or `0` on failure.
fn build_session_namespace(fs_endpoint: u64, home: &[u8]) -> u64 {
    // A fresh, owned namespace (full rights — this is *our* namespace to compose).
    let ns = unsafe { syscall0(SYS_NS_CREATE) };
    if ns < 0 {
        kprint(b"session-mgr: ns_create FAIL\n");
        return 0;
    }
    let ns = ns as u64;
    // Bind the fs-server endpoint at `/home` scoped to the user's home subtree. The
    // kernel shares init's existing fs registration (bind-mount) and prepends `home`
    // to every forwarded suffix. Requires BIND_NAMESPACE (re-delegated) + BIND on `ns`.
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
    ns
}

/// Bootstrap registers: `rdi` = notification channel (unused in Part D), `rsi` = the
/// inherited (LOOKUP-only) root namespace, `rdx` = the control channel service-mgr
/// hands the endpoints over, `rcx` = `arg0` (unused).
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, _root_ns: u64, control: u64, _arg0: u64) -> ! {
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

    // Part-D self-check: authenticate the demo user (the first end-to-end exercise of
    // the credential stack under real spawning), construct their session namespace, and
    // confirm a wrong password is denied. All three must pass.
    let mut home = [0u8; 256];
    let mut ok = false;
    match authenticate(auth_ch, DEMO_USER, DEMO_PASSWORD, &mut home) {
        Some(home_len) => {
            kprint(b"session-mgr: authenticated 'alice' -> home=");
            kprint(&home[..home_len]);
            kprint(b"\n");
            let ns = build_session_namespace(fs_endpoint, &home[..home_len]);
            if ns != 0 {
                kprint(b"session-mgr: session namespace built (/home subtree bound)\n");
                ok = true;
            }
        }
        None => kprint(b"session-mgr: demo authentication FAILED\n"),
    }
    // Negative check: a wrong password must be denied.
    let mut scratch = [0u8; 256];
    if authenticate(auth_ch, DEMO_USER, b"not-the-password", &mut scratch).is_none() {
        kprint(b"session-mgr: wrong password correctly denied\n");
    } else {
        kprint(b"session-mgr: wrong password WRONGLY accepted\n");
        ok = false;
    }

    if ok {
        kprint(b"session-mgr: login-path plumbing verified (Part D)\n");
    }
    verdict(ok);
    idle();
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
