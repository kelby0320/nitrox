//! `boot-probe` — the in-guest substrate checks, and the boot verdict.
//!
//! **Why this is a program and not a phase of a supervisor.** The SMP and floating-point
//! gates below lived in `session-mgr`, and the filesystem tests still live in `init`. They
//! were there because *the verdict* was, not because they belong to a supervisor:
//! `sched_gate` called itself "the Phase 3 clause 3 verdict gate, checked synchronously at
//! the single PASS point", which is a statement about where the verdict is, and nothing
//! about sessions. Move the verdict to a program whose job is adjudication and the probes
//! follow it out. See [`docs/planning/test-path-retrofit.md`](../../../docs/planning/test-path-retrofit.md).
//!
//! **What it now owns**, as of Part B: the clause-3 scheduler gate, the hard-float gate,
//! and `SYS_TEST_EXIT`. `init` still writes a FAIL verdict for a critical-path boot failure
//! or a crashed demo chain — that is a different question ("did the boot get this far") and
//! it is answered before this program starts. Part C moves `init`'s filesystem tests here.
//!
//! **The ordering that makes the gates meaningful is `init`'s, and it is serial.**
//! `init::supervise` runs the demo chain synchronously and fails the run on a non-zero
//! exit, and only then hands off to the login chain that reaches `service-mgr` and this
//! program. So everything the run adjudicates has already happened when the gates run, and
//! they are the last thing before the only `SYS_TEST_EXIT(PASS)` call. That placement is
//! the whole reason `fp_gate` was moved out of the demo `parent` in the first place — see
//! its own doc comment.
//!
//! **Started by `service-mgr`** from `/system/services.toml` on the root, which carries a
//! `[service.boot-probe]` table only in selftest / test-harness images. It is an ordinary
//! declared service: a control channel at `rdx`, a LOOKUP-only view of the root namespace
//! at `rsi`, no syscaps, and `policy = "never"` — start once, do not restart.

#![no_std]
#![no_main]

extern crate alloc;

/// `Line` builds its text on the heap, so this bin needs an allocator.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

use libkern::abi::HandleInfo;
use libkern::debug::Line;
use libkern::{
    IO_OPCODE_READ, IoOp, RIGHT_READ, SYS_FILE_TRUNCATE, SYS_IO_SUBMIT, SYS_MEMORY_CREATE, SYS_NS_SYNC, syscall6,
};
use libkern::{
    RIGHT_LOOKUP, RIGHT_TRANSFER, RIGHT_UNBIND, SYS_HANDLE_DUPLICATE, SYS_HANDLE_STAT, SYS_NS_DERIVE, SYS_NS_UNBIND,
};
use libkern::{
    RIGHT_MAP_READ, RIGHT_MAP_WRITE, SYS_FILE_CREATE, SYS_FILE_GROW, SYS_FILE_SYNC, SYS_HANDLE_CLOSE, SYS_MEMORY_MAP,
    SYS_MEMORY_UNMAP, SYS_NS_LOOKUP, SYS_TEST_EXIT, SYS_WAIT, TEST_EXIT_FAILURE, TEST_EXIT_SUCCESS, exit, kprint,
    syscall1, syscall2, syscall4, syscall5,
};

mod authority;
mod files;
mod gates;
mod policy;
mod registry;
mod storage;
mod storage_admin;
mod views;

use authority::*;
use files::*;
use gates::*;
use policy::*;
use registry::*;
use storage::*;
use storage_admin::*;
use views::*;

/// Page size, for the mapped-file checks below.
const PAGE: u64 = 4096;

static mut WAIT_HANDLES: [u64; 1] = [0];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];

/// Block until `handle` signals. `false` if the wait returned anything but one ready.
fn wait_one(handle: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = handle;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    waited == 1
}

/// Resolve `path` in `ns` with `rights`, awaiting the `PendingOperation`. Returns
/// `(status, handle)`; a non-zero status means the lookup failed and `handle` is `0`.
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
    // SAFETY: the wait completed, so the kernel wrote a 24-byte `IoResult`.
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

/// Fire the boot verdict — terminate QEMU via `SYS_TEST_EXIT` with pass or fail.
///
/// **The single PASS point.** `test-qemu` adjudicates the whole boot from this one call, so
/// every check that must gate the run has to happen before it and in this process. That is
/// why the SMP and floating-point gates above are here rather than wherever they are
/// conceptually at home: they were in `session-mgr` for the same reason, because the verdict
/// was, and they followed it here when it moved
/// (`docs/planning/test-path-retrofit.md` Part B).
///
/// Outside `test-qemu` the `isa-debug-exit` device is not attached, so the port write is
/// ignored and the syscall returns `Unsupported` — the caller carries on. It must not park
/// instead: doing so once stranded a thread and deadlocked every later TLB shootdown.
fn verdict(ok: bool) {
    let code = if ok { TEST_EXIT_SUCCESS } else { TEST_EXIT_FAILURE };
    kprint(if ok { b"boot-probe: test-harness verdict PASS\n" } else { b"boot-probe: test-harness verdict FAIL\n" });
    // SAFETY: SYS_TEST_EXIT takes the verdict in a0; under the kernel test-harness build
    // it writes isa-debug-exit and QEMU terminates (so this does not return in practice).
    unsafe { syscall1(SYS_TEST_EXIT, code as u64) };
}

/// Wait for a `PendingOperation`, close it, and return its `(status, result)`; `(-1, 0)` if the
/// wait itself failed.
fn po_wait(po: u64) -> (i32, u64) {
    let ok = wait_one(po);
    // SAFETY: when the wait completed the kernel wrote a 24-byte `IoResult`.
    let r = unsafe {
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
    close(po);
    if ok { r } else { (-1, 0) }
}

/// A size-changing resolve — `SYS_FILE_CREATE`, `_GROW` or `_TRUNCATE` of `path` to `size` —
/// returning the file's handle.
fn file_resize(nr: u64, ns: u64, path: &[u8], size: u64) -> Option<u64> {
    // SAFETY: valid path pointer + namespace handle.
    let po =
        unsafe { syscall5(nr, ns, path.as_ptr() as u64, path.len() as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE, size) };
    if po < 0 {
        return None;
    }
    match po_wait(po as u64) {
        (0, h) if h != 0 => Some(h),
        _ => None,
    }
}

/// Map `len` bytes of file handle `h` with `rights`; the address, or `None`.
fn map_file(h: u64, len: u64, rights: u64) -> Option<u64> {
    // SAFETY: register-only syscall on a handle this process holds.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, len, rights) };
    (addr >= 0).then_some(addr as u64)
}

/// Bootstrap registers, as `service-mgr`'s `SPAWN_SERVICE` fills them: `rdi` = this
/// process's notification channel, `rsi` = the inherited LOOKUP-only root namespace,
/// `rdx` = the control-channel endpoint (`RECV | WAIT`), `rcx` = `arg0` (unused).
///
/// **The control endpoint is held until exit, and closing it early is a bug.** An earlier
/// version of this file closed it as its second instruction, reasoning that a probe with no
/// lifecycle protocol to serve has no use for it. It does have one use, and it is not the
/// probe's: `service-mgr` reads *this handle's* closure as "the child is gone"
/// (`supervise`), because a pid on `KIND_CHILD_EXITED` cannot be matched to a process
/// handle. Closing it early therefore reports a death that has not happened — observed as
/// `'boot-probe' exited code=unknown` printed before this function's own next line, and
/// under `policy = "always"` as a *second copy of a live service*, which is the exact
/// failure this program was added to prove is gone (PR #226 review, finding 1).
///
/// So: hold it, and let process teardown close it. That is what makes "peer closed" mean
/// "child exited", and it is a contract on every declared service rather than a quirk of
/// this one — see `docs/spec/service-toml-schema.md`.
///
/// **The verdict is fired, then the process exits.** `verdict` does not return under
/// `test-qemu` (QEMU terminates), but it does everywhere else — every other gate boots this
/// image without the `isa-debug-exit` device — so there is a real path past it.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_notif: u64, root_ns: u64, control: u64, _arg0: u64) -> ! {
    kprint(b"boot-probe: up\n");
    let _ = control;
    // `&` and not `&&`: every check runs and reports, so one failure does not hide the
    // rest — a boot that fails three of these should say three, not one.
    let ok = sched_gate(root_ns)
        & fp_gate()
        & read_large_file(root_ns)
        & overwrite_test(root_ns)
        & grow_test(root_ns)
        & create_test(root_ns)
        & file_cache_test(root_ns)
        & unlinked_file_test(root_ns)
        & flush_test(root_ns)
        & power_refused_test(root_ns)
        & clock_set_refused_test()
        & bind_needs_transfer_test()
        & continuation_test(root_ns)
        & subtree_bind_test(root_ns)
        & auth_multi_client_test(root_ns)
        & ns_derive_test(root_ns)
        & view_broker_test(root_ns)
        & registry_test(root_ns)
        & past_two_tib_test(root_ns)
        & devices_test(root_ns)
        & storage_test(root_ns)
        & storage_mount_test(root_ns)
        & io_counters_test(root_ns)
        & long_listing_test(root_ns)
        & storage_admin_test(root_ns)
        & storage_grant_test(root_ns)
        & auth_admin_test(root_ns)
        & policy_test(root_ns)
        & grace_test(root_ns)
        & accounts_test(root_ns)
        & restart_test(root_ns)
        & services_test(root_ns)
        & power_endpoint_test(root_ns)
        & logs_test(root_ns);
    verdict(ok);
    // **Reached only where the verdict device is absent** — every gate except `test-qemu`
    // boots this image without `isa-debug-exit`, so `SYS_TEST_EXIT` returns `Unsupported` and
    // execution continues here. Naming `test-qemu` on this line would name the one gate that
    // is not running when it is reached.
    //
    // The exit code tracks the gates, and `service-mgr` reports it. `check-terminal`'s
    // `check_service_attribution` requires `code=0`, so a `sched_gate` or `fp_gate` regression
    // fails **that** gate — with a message about attribution, since that is what it asserts.
    // The `boot-probe: … FAIL` line naming the real cause is directly above it in the same
    // transcript.
    exit(if ok { 0 } else { 1 });
}

/// The demo account the build seeds, and the policy it seeds for it (`xtask`'s `DEMO_USER` and
/// `DEMO_PASSWORD`, and `seeded_views_toml`). A build input, not a secret — `init`'s login
/// selftest used the same literals.
const DEMO_USER: &[u8] = b"alice";
const DEMO_PASSWORD: &[u8] = b"correct horse battery staple";

fn clock_ns() -> u64 {
    let mut t = 0u64;
    // SAFETY: `t` is a valid writable u64 out-param.
    unsafe { syscall2(libkern::SYS_CLOCK_READ, libkern::abi::CLOCK_MONOTONIC, (&raw mut t) as u64) };
    t
}

/// One exchange with the view broker: send `op` on `ch` carrying `body` and moving `handles`, and
/// wait — at most ten seconds — for the reply to it. An `Exited` that arrives first is set aside in
/// `exited` rather than mistaken for the reply. `None` if nothing came.
fn views_call(
    ch: u64,
    op: u16,
    request_id: u64,
    body: &[u8],
    handles: &[u64],
    exited: &mut alloc::vec::Vec<alloc::vec::Vec<u8>>,
) -> Option<(bool, alloc::vec::Vec<u8>)> {
    if !rs_send(ch, op, request_id, body, handles) {
        return None;
    }
    views_receive(ch, request_id, exited)
}

/// Send one rsproto message on `ch`, moving `handles`, without waiting for an answer — to the view
/// broker, or down an endpoint as the kernel would forward a resolve.
fn rs_send(ch: u64, op: u16, request_id: u64, body: &[u8], handles: &[u64]) -> bool {
    let mut msg = [0u8; 4096];
    let Some(n) = librsproto::encode(&mut msg[24..], op, request_id, 0, body, handles.len() as u16) else {
        return false;
    };
    msg[4..8].copy_from_slice(&(n as u32).to_le_bytes());
    msg[8] = handles.len() as u8;
    // SAFETY: valid message buffer and handle array.
    let sr = unsafe {
        syscall5(
            libkern::SYS_CHANNEL_SEND,
            ch,
            msg.as_ptr() as u64,
            handles.as_ptr() as u64,
            handles.len() as u64,
            libkern::SENDMODE_NOBLOCK,
        )
    };
    sr == 0
}

/// Wait for the message with `request_id` on `ch` (`0` for the next `Exited`), setting any other
/// `Exited` aside in `exited`. Ten seconds at most.
fn views_receive(
    ch: u64,
    request_id: u64,
    exited: &mut alloc::vec::Vec<alloc::vec::Vec<u8>>,
) -> Option<(bool, alloc::vec::Vec<u8>)> {
    let deadline = clock_ns() + 10_000_000_000;
    loop {
        let m = receive(ch, deadline)?;
        if m.op == librsproto::views::OP_VIEWS_EXITED && m.request_id == 0 {
            if request_id == 0 {
                return Some((false, m.body));
            }
            exited.push(m.body);
            continue;
        }
        if m.request_id == request_id {
            return Some((m.error, m.body));
        }
    }
}

/// One rsproto message, received and copied out, with the handles that came with it.
struct Received {
    op: u16,
    request_id: u64,
    error: bool,
    body: alloc::vec::Vec<u8>,
    handles: alloc::vec::Vec<u64>,
}

/// The next message on `ch`, waiting until `deadline` on the monotonic clock — `0` only looks.
/// `None` if nothing came by then, or if what came is not rsproto.
fn receive(ch: u64, deadline: u64) -> Option<Received> {
    let mut buf = [0u8; 4096];
    let mut hs = [0u64; 8];
    loop {
        let mut count = 0usize;
        // SAFETY: valid recv out-params.
        let rr = unsafe {
            syscall4(
                libkern::SYS_CHANNEL_RECV,
                ch,
                buf.as_mut_ptr() as u64,
                hs.as_mut_ptr() as u64,
                (&raw mut count) as u64,
            )
        };
        if rr == 0 {
            let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
            let m = librsproto::decode(&buf[24..24 + len.min(4096 - 24)]).ok()?;
            return Some(Received {
                op: m.op,
                request_id: m.request_id,
                error: m.is_error(),
                body: m.body.to_vec(),
                handles: hs[..count.min(hs.len())].to_vec(),
            });
        }
        if clock_ns() >= deadline {
            return None;
        }
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers; one waiter, with a deadline.
        unsafe {
            WAIT_HANDLES[0] = ch;
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, deadline);
        }
    }
}

/// What the kernel says of handle `h`. `None` if it is not one this process holds.
fn stat(h: u64) -> Option<HandleInfo> {
    let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
    // SAFETY: `info` is a writable 24-byte `HandleInfo`, the layout the kernel writes.
    let sr = unsafe { syscall2(SYS_HANDLE_STAT, h, (&raw mut info) as u64) };
    (sr == 0).then_some(info)
}

/// Map a read-only object and copy it out, so nothing borrows the mapping past this call.
fn read_all(h: u64) -> Option<alloc::vec::Vec<u8>> {
    let size = stat(h)?.size;
    // SAFETY: register-only syscall; `h` is a MemoryObject handle with MAP_READ.
    let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, size, RIGHT_MAP_READ) };
    if addr < 0 {
        return None;
    }
    // SAFETY: `size` bytes are mapped read-only at `addr` until the unmap below.
    let bytes = unsafe { core::slice::from_raw_parts(addr as u64 as *const u8, size as usize) }.to_vec();
    // SAFETY: unmapping what was mapped above; `bytes` is a copy.
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, 0) };
    Some(bytes)
}

/// Close `h` if it is a handle at all.
fn close(h: u64) {
    if h != 0 {
        // SAFETY: closing a handle this process holds.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
    }
}

/// `/dev/registry`'s records, read through `ns`. `None` if it is not bound there or does not read.
fn registry_records(ns: u64) -> Option<alloc::vec::Vec<libkern::device::DeviceRecord>> {
    let (st, snap) = ns_lookup(ns, b"/dev/registry", RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
    if st != 0 || snap == 0 {
        return None;
    }
    let bytes = read_all(snap);
    close(snap);
    Some(libkern::device::records(&bytes?).ok()?.collect())
}

/// The test image's scratch filesystem: its label, and what its `README` says. The image builder
/// writes both (`SCRATCH_LABEL` and `SCRATCH_README` in `tools/xtask`); a copy that drifted from
/// its text fails the read below, which is the point of keeping two.
const SCRATCH_README: &[u8] = b"nitrox-scratch: a test image's second filesystem, mounted by the storage service\n";

/// **One program in the admin view**, as `with admin` runs it: a request on the client channel at
/// `client_path`, the demo password, and `stdout` and `stderr` piped back. Its exit code, every row
/// of the table it wrote, and what it wrote on `stderr`. `None` if the broker did not start it.
fn in_admin_view(
    root_ns: u64,
    client_path: &str,
    program: &[u8],
    args: &[&[u8]],
    exited: &mut alloc::vec::Vec<alloc::vec::Vec<u8>>,
) -> Option<(i64, alloc::vec::Vec<alloc::vec::Vec<libstream::wire::Value>>, alloc::vec::Vec<u8>)> {
    use libkern::SYS_NS_DERIVE;
    use librsproto::views::*;
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let (st, cli) = ns_lookup(root_ns, client_path.as_bytes(), chan);
    // SAFETY: a namespace handle this process holds.
    let copy = unsafe { syscall1(SYS_NS_DERIVE, root_ns) };
    let (mut out_w, mut out_r, mut err_w, mut err_r) = (0u64, 0u64, 0u64, 0u64);
    // SAFETY: valid writable out-params, for each of the two pipes.
    let piped = unsafe {
        syscall4(libkern::SYS_CHANNEL_CREATE, (&raw mut out_w) as u64, (&raw mut out_r) as u64, 16, 0) == 0
            && syscall4(libkern::SYS_CHANNEL_CREATE, (&raw mut err_w) as u64, (&raw mut err_r) as u64, 16, 0) == 0
    };
    if st != 0 || cli == 0 || copy <= 0 || !piped {
        return None;
    }
    let mut req = [0u8; 256];
    let n = build_request(&mut req, REQ_STDOUT | REQ_STDERR, b"admin", program, args, b"", None)?;
    let asked = matches!(
        views_call(cli, OP_VIEWS_REQUEST, 1, &req[..n], &[copy as u64, out_w, err_w], exited),
        Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::NeedPassword, _)))
    ) && matches!(
        views_call(cli, OP_VIEWS_PASSWORD, 2, DEMO_PASSWORD, &[], exited),
        Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::Started, _)))
    );
    let listing = if asked {
        libstream::channel::ChannelReceiver::new(libstream::channel::IpcPort::new(out_r)).receive().ok()
    } else {
        None
    };
    close(out_r);
    // Every diagnostic is one message, and the program has closed its end by now: read to
    // the close.
    let mut said = alloc::vec::Vec::new();
    if asked {
        use libstream::channel::MsgPort;
        let mut port = libstream::channel::IpcPort::new(err_r);
        while port.recv(&mut said).is_ok() {}
    }
    close(err_r);
    let code = if asked { exited.pop().or_else(|| views_receive(cli, 0, exited).map(|r| r.1)) } else { None };
    close(cli);
    let code = code.as_deref().and_then(parse_exited)?.0 as i64;
    let mut rows = alloc::vec::Vec::new();
    if let Some(mut tr) = listing.as_deref().and_then(|b| libstream::table::TableReader::new(b).ok()) {
        while let Some(Ok(libstream::table::Item::Row(vals))) = tr.next() {
            rows.push(vals);
        }
    }
    Some((code, rows, said))
}

/// **A restarted server is reached again at its path** (administration Part E.1).
///
/// `restart-probe` is a test image's server, declared with `endpoint = "/svc/restart-probe"` and
/// `policy = "always"`. It answers `id` with a token unique to the instance, and exits when asked
/// for `exit`. So: take its token, have it exit, and resolve the same path until a different token
/// comes back — from the instance `service-mgr` started in its place.
///
/// **What this proves is the registry.** The root's `/svc/restart-probe` is bound once, to
/// `service-mgr`'s route for the server, and never again: only the registry binding changes on a
/// restart. A root binding to the server's own endpoint would reach the dead one, which answers
/// `PeerClosed`, however long this waited.
///
/// **And that a binding made before the restart reaches the new instance** (Part E.1b), in a copy
/// of the root made before it. That is a session's case: its bindings are made at login, of the
/// same route the root binds, and nothing rebinds them. A restart that rebound the root's path
/// alone passes the first half and fails this one.
fn restart_test(root_ns: u64) -> bool {
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: restart: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let token = |ns: u64| -> Option<u64> {
        let (st, ch) = ns_lookup(ns, b"/svc/restart-probe/id", chan);
        if st != 0 || ch == 0 {
            return None;
        }
        let m = receive(ch, clock_ns() + 5_000_000_000);
        close(ch);
        let m = m?;
        m.handles.iter().for_each(|&h| close(h));
        Some(u64::from_le_bytes(m.body.get(..8)?.try_into().ok()?))
    };
    let Some(first) = token(root_ns) else {
        return fail(b"/svc/restart-probe answered no token");
    };
    // **A namespace bound before the restart, and never touched again** — a session's, as a
    // supervisor builds one (administration Part E.1b). A copy of the root holds the same binding
    // objects the root does, and later changes to the root do not reach it: a restart that
    // rebound the root's path alone would leave this one at the server that exited.
    // SAFETY: a namespace handle this process holds; the copy is its own to close.
    let copy = unsafe { syscall1(SYS_NS_DERIVE, root_ns) };
    if copy <= 0 {
        return fail(b"a copy of the root could not be made");
    }
    let copy = copy as u64;
    if token(copy) != Some(first) {
        close(copy);
        return fail(b"the copy of the root did not reach the instance the root does");
    }
    let (st, h) = ns_lookup(root_ns, b"/svc/restart-probe/exit", chan);
    close(h);
    if st == 0 {
        return fail(b"asking restart-probe to exit was not refused, as the probe answers it");
    }
    // Its backoff is 100 ms. Resolve until the next instance answers, or give up — pausing
    // twenty milliseconds between tries on a timer, since a wait on no handles is refused at once.
    // SAFETY: register-only syscall; a one-shot timer this process owns.
    let timer = unsafe { syscall1(libkern::SYS_TIMER_CREATE, 0) };
    let deadline = clock_ns() + 10_000_000_000;
    let second = loop {
        match token(root_ns) {
            Some(t) if t != first => break Some(t),
            _ if clock_ns() > deadline => break None,
            _ if timer > 0 => {
                let at = clock_ns() + 20_000_000;
                let handles = [timer as u64];
                let mut results = [0u8; 24];
                // SAFETY: arming our own timer, then a one-entry wait on it, bounded besides.
                unsafe {
                    syscall4(libkern::SYS_TIMER_SET, timer as u64, at, 0, 0);
                    syscall4(
                        libkern::SYS_WAIT,
                        handles.as_ptr() as u64,
                        1,
                        results.as_mut_ptr() as u64,
                        at + 1_000_000_000,
                    );
                }
            }
            _ => {}
        }
    };
    close(if timer > 0 { timer as u64 } else { 0 });
    let Some(second) = second else {
        close(copy);
        return fail(b"no new instance answered at /svc/restart-probe within 10 s of the old one exiting");
    };
    let in_copy = token(copy);
    close(copy);
    if in_copy != Some(second) {
        return fail(b"the copy of the root, made before the restart, did not reach the new instance");
    }
    kprint(b"boot-probe: restart: a server that exited was restarted, and its path reached the new instance ok\n");
    kprint(b"boot-probe: restart: a namespace bound before the restart reached it too ok\n");
    true
}

/// **Starting, stopping and restarting services** (administration Part E.2), on an admin session
/// opened as the view broker's `services` grant will open one: the endpoint
/// `/svc/services/admin-endpoint` mints, bound in a namespace the probe builds, and resolved there.
/// 1. `/svc/services/all.tsm` lists the declarations, `clipboard-server` running.
/// 2. **A stop is answered once it has happened**: the table then says `stopped`, and
///    `/dev/clipboard` answers `NotFound` — its registry entry went with the server.
/// 3. A start is answered once the server is up, and `/dev/clipboard` opens again.
/// 4. A restart is answered once the new instance is up, and the table counts it.
/// 5. The refusals: an essential service's stop and restart, a name nothing declares, a start of a
///    running service.
/// 6. **A restart whose session closes at once still starts the service again** (PR #341 review,
///    finding 1): only the answer is lost with the session.
fn services_test(root_ns: u64) -> bool {
    use libkern::{KError, SYS_NS_BIND, SYS_NS_CREATE};
    use librsproto::services::{OP_SERVICES_RESTART, OP_SERVICES_START, OP_SERVICES_STOP};
    use libstream::wire::{Table, Value};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: services: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT | libkern::RIGHT_DUPLICATE;
    // A service's row: its state and its restarts.
    let row = |name: &str| -> Option<(alloc::string::String, i64)> {
        let (st, h) = ns_lookup(root_ns, b"/svc/services/all.tsm", RIGHT_MAP_READ | libkern::RIGHT_INSPECT);
        let bytes = if st == 0 { read_all(h) } else { None };
        close(h);
        let t = Table::decode(&bytes?).ok()?;
        let col = |n: &str| t.schema.fields.iter().position(|f| f.name == n);
        let (cn, cs, cr) = (col("name")?, col("state")?, col("restarts")?);
        let r = t.rows.iter().find(|r| r[cn] == Value::Str(alloc::string::String::from(name)))?;
        match (&r[cs], &r[cr]) {
            (Value::Str(s), Value::Int(n)) => Some((s.clone(), *n)),
            _ => None,
        }
    };
    let state = |name: &str| row(name).map(|r| r.0);
    let clipboard_opens = || {
        let (st, h) = ns_lookup(root_ns, b"/dev/clipboard", chan);
        close(h);
        st
    };

    // 1. The list.
    if state("clipboard-server").as_deref() != Some("running") || state("auth-service").is_none() {
        return fail(b"all.tsm does not list clipboard-server running, and auth-service");
    }

    // The admin session, as the view broker will reach it.
    let (st, endpoint) = ns_lookup(root_ns, b"/svc/services/admin-endpoint", chan);
    if st != 0 || endpoint == 0 {
        return fail(b"no admin endpoint at /svc/services/admin-endpoint");
    }
    // SAFETY: register-only syscall (its argument is unused); returns a fresh namespace handle.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns <= 0 {
        close(endpoint);
        return fail(b"no namespace to bind the admin endpoint into");
    }
    let ns = ns as u64;
    let at = b"/dev/services/admin";
    // SAFETY: a namespace this process created, a valid path, and an endpoint it holds.
    let bound = unsafe { syscall6(SYS_NS_BIND, ns, at.as_ptr() as u64, at.len() as u64, endpoint, 0, 0) };
    close(endpoint);
    let (st, admin) = if bound == 0 { ns_lookup(ns, at, chan) } else { (-1, 0) };
    if st != 0 || admin == 0 {
        close(ns);
        return fail(b"/dev/services/admin opened no admin session");
    }
    let mut next = 0u64;
    let mut ask = |op: u16, body: &[u8]| -> Option<Received> {
        next += 1;
        if !rs_send(admin, op, next, body, &[]) {
            return None;
        }
        let deadline = clock_ns() + 40_000_000_000;
        loop {
            let m = receive(admin, deadline)?;
            m.handles.iter().for_each(|&h| close(h));
            if m.request_id == next {
                return Some(m);
            }
        }
    };
    let refused = |m: &Option<Received>, err: KError| {
        m.as_ref().is_some_and(|m| {
            m.error && librsproto::error::parse_error(&m.body).is_some_and(|e| e.kerror == err.as_i32())
        })
    };
    let answered = |m: &Option<Received>| m.as_ref().is_some_and(|m| !m.error);
    let finish = |ok: bool| {
        close(admin);
        close(ns);
        ok
    };

    // 2. Stopped.
    if !answered(&ask(OP_SERVICES_STOP, b"clipboard-server")) {
        return finish(fail(b"a stop of clipboard-server was not answered"));
    }
    if state("clipboard-server").as_deref() != Some("stopped") {
        return finish(fail(b"clipboard-server stopped, and the table does not say so"));
    }
    if clipboard_opens() != KError::NotFound.as_i32() {
        return finish(fail(b"/dev/clipboard did not answer NotFound with its server stopped"));
    }
    // 3. Started.
    if !answered(&ask(OP_SERVICES_START, b"clipboard-server")) {
        return finish(fail(b"a start of clipboard-server was not answered"));
    }
    if state("clipboard-server").as_deref() != Some("running") || clipboard_opens() != 0 {
        return finish(fail(b"clipboard-server started, and is not running at /dev/clipboard"));
    }
    // 4. Restarted.
    let before = row("clipboard-server").map_or(-1, |r| r.1);
    if !answered(&ask(OP_SERVICES_RESTART, b"clipboard-server")) {
        return finish(fail(b"a restart of clipboard-server was not answered"));
    }
    let after = row("clipboard-server");
    if after.as_ref().map(|r| (r.0.as_str(), r.1)) != Some(("running", before + 1)) || clipboard_opens() != 0 {
        return finish(fail(b"clipboard-server restarted, and is not running, counted once, at /dev/clipboard"));
    }
    // 5. Refused.
    if !refused(&ask(OP_SERVICES_STOP, b"auth-service"), KError::NoAccess)
        || !refused(&ask(OP_SERVICES_RESTART, b"auth-service"), KError::NoAccess)
    {
        return finish(fail(b"an essential service's stop or restart was not refused NoAccess"));
    }
    if !refused(&ask(OP_SERVICES_STOP, b"no-such-service"), KError::NotFound) {
        return finish(fail(b"a stop of a name nothing declares was not refused NotFound"));
    }
    if !refused(&ask(OP_SERVICES_START, b"clipboard-server"), KError::AlreadyExists) {
        return finish(fail(b"a start of a running service was not refused AlreadyExists"));
    }
    if state("auth-service").as_deref() != Some("running") {
        return finish(fail(b"auth-service is not running after its refused stop"));
    }
    // 6. A restart asked on a session that closes before its answer. The request and the close are
    //    handled in one pass: the kernel hands out what is queued before it reports the peer gone.
    let before = row("clipboard-server").map_or(-1, |r| r.1);
    let (st, second) = ns_lookup(ns, at, chan);
    if st != 0 || second == 0 || !rs_send(second, OP_SERVICES_RESTART, 1, b"clipboard-server", &[]) {
        close(second);
        return finish(fail(b"a second admin session would not take a restart"));
    }
    close(second);
    // Twenty milliseconds between reads, on a timer: a wait on no handles is refused at once.
    // SAFETY: register-only syscall; a one-shot timer this process owns.
    let timer = unsafe { syscall1(libkern::SYS_TIMER_CREATE, 0) };
    let deadline = clock_ns() + 15_000_000_000;
    let restarted = loop {
        match row("clipboard-server") {
            Some((s, n)) if s == "running" && n == before + 1 => break true,
            _ if clock_ns() > deadline => break false,
            _ if timer > 0 => {
                let at = clock_ns() + 20_000_000;
                let handles = [timer as u64];
                let mut results = [0u8; 24];
                // SAFETY: arming our own timer, then a one-entry wait on it, bounded besides.
                unsafe {
                    syscall4(libkern::SYS_TIMER_SET, timer as u64, at, 0, 0);
                    syscall4(
                        libkern::SYS_WAIT,
                        handles.as_ptr() as u64,
                        1,
                        results.as_mut_ptr() as u64,
                        at + 1_000_000_000,
                    );
                }
            }
            _ => {}
        }
    };
    close(if timer > 0 { timer as u64 } else { 0 });
    if !restarted {
        return finish(fail(b"a restart whose session closed at once did not start clipboard-server again"));
    }
    kprint(
        b"boot-probe: services: listed, clipboard-server stopped, started and restarted, each answered once done ok\n",
    );
    kprint(b"boot-probe: services: an essential stop, an unknown name and a second start refused ok\n");
    kprint(b"boot-probe: services: a restart whose session closed at once still started it again ok\n");
    finish(true)
}

/// **The power endpoint answers nothing but a well-formed shutdown** (administration Part E.4b),
/// on a session opened as the view broker's `power` grant will open one — and an admin session
/// answers no shutdown at all. A well-formed one on a power session would stop this machine, so
/// sending it is `check-shutdown`'s:
/// - `Start` on a power session, refused `NoAccess`;
/// - `Shutdown` with no body, two bytes, or a byte that names neither halt nor reboot, refused
///   `InvalidArgument`;
/// - `Shutdown` on an admin session, refused `NoAccess`;
/// - a third power endpoint while two are held, refused `WouldBlock`, and another taken at once
///   once one is let go — which is `serve_services` answering closes before resolves. **The view
///   broker holds the first** by then: `view_broker_test` has had it build `admin` views, whose
///   `power` grant takes one for the boot (administration Part E.4d).
fn power_endpoint_test(root_ns: u64) -> bool {
    use libkern::{KError, SYS_NS_BIND, SYS_NS_CREATE};
    use librsproto::services::{OP_SERVICES_SHUTDOWN, OP_SERVICES_START, SHUTDOWN_HALT, SHUTDOWN_REBOOT};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: power: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT | libkern::RIGHT_DUPLICATE;
    // A session on the endpoint `path` mints, as a grant's binding reaches it: bound in a namespace
    // of this probe's own, and resolved there. The namespace holds the endpoint until it is closed.
    let open = |path: &[u8]| -> Option<(u64, u64)> {
        let (st, endpoint) = ns_lookup(root_ns, path, chan);
        if st != 0 || endpoint == 0 {
            return None;
        }
        // SAFETY: register-only syscall (its argument is unused); returns a fresh namespace handle.
        let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
        if ns <= 0 {
            close(endpoint);
            return None;
        }
        let ns = ns as u64;
        let at = b"/grant";
        // SAFETY: a namespace this process created, a valid path, and an endpoint it holds.
        let bound = unsafe { syscall6(SYS_NS_BIND, ns, at.as_ptr() as u64, at.len() as u64, endpoint, 0, 0) };
        close(endpoint);
        let (st, session) = if bound == 0 { ns_lookup(ns, at, chan) } else { (-1, 0) };
        if st != 0 || session == 0 {
            close(ns);
            return None;
        }
        Some((ns, session))
    };
    // Ask `op` on `session` and say how it was answered: `0`, or the refusal's `KError`.
    let ask = |session: u64, id: u64, op: u16, body: &[u8]| -> Option<i32> {
        if !rs_send(session, op, id, body, &[]) {
            return None;
        }
        let deadline = clock_ns() + 10_000_000_000;
        loop {
            let m = receive(session, deadline)?;
            m.handles.iter().for_each(|&h| close(h));
            if m.request_id == id {
                return Some(if m.error {
                    librsproto::error::parse_error(&m.body).map_or(-1, |e| e.kerror)
                } else {
                    0
                });
            }
        }
    };
    let Some((power_ns, power)) = open(b"/svc/services/power-endpoint") else {
        return fail(b"no power session through /svc/services/power-endpoint");
    };
    let Some((admin_ns, admin)) = open(b"/svc/services/admin-endpoint") else {
        close(power);
        close(power_ns);
        return fail(b"no admin session through /svc/services/admin-endpoint");
    };
    let (no_access, invalid) = (Some(KError::NoAccess.as_i32()), Some(KError::InvalidArgument.as_i32()));
    let refusals: [(Option<i32>, Option<i32>, &[u8]); 5] = [
        (ask(power, 1, OP_SERVICES_START, b"clipboard-server"), no_access, b"a start on a power session"),
        (ask(power, 2, OP_SERVICES_SHUTDOWN, &[]), invalid, b"a shutdown with no body"),
        (ask(power, 3, OP_SERVICES_SHUTDOWN, &[SHUTDOWN_HALT, SHUTDOWN_REBOOT]), invalid, b"a shutdown with two bytes"),
        (ask(power, 4, OP_SERVICES_SHUTDOWN, &[2]), invalid, b"a shutdown naming neither halt nor reboot"),
        (ask(admin, 5, OP_SERVICES_SHUTDOWN, &[SHUTDOWN_HALT]), no_access, b"a shutdown on an admin session"),
    ];
    // Two power endpoints are held now: the broker's, and this probe's, by `power_ns`'s binding.
    let (second, extra) = ns_lookup(root_ns, b"/svc/services/power-endpoint", chan);
    close(extra);
    for h in [power, power_ns, admin, admin_ns] {
        close(h);
    }
    let (again, taken) = ns_lookup(root_ns, b"/svc/services/power-endpoint", chan);
    close(taken);
    for (answer, expected, what) in refusals {
        if answer != expected {
            Line::new().s(b"boot-probe: power: ").s(what).s(b" was answered ").i(answer.unwrap_or(1) as i64).end();
            return fail(b"a request was not refused as it should be");
        }
    }
    if second != KError::WouldBlock.as_i32() {
        return fail(b"a third power endpoint was not refused while the broker's and this probe's were held");
    }
    if again != 0 || taken == 0 {
        return fail(b"no power endpoint at once after the one was let go");
    }
    kprint(b"boot-probe: power endpoint refused a start, three malformed shutdowns and one on an admin session, and a third endpoint ok\n");
    true
}

/// **Reading the log back** (administration Part E.6), through a read endpoint bound as the
/// `logs` grant binds it:
/// - a record this probe writes on a log source of its own is read back, principal, source, level
///   and message as written, and stamped with the wall clock;
/// - a `Read` whose body is not twelve bytes is refused `InvalidArgument`, and an op that is not
///   `Read` `Unsupported`;
/// - **two sessions at once, and a third refused** `WouldBlock`; one let go, and another opened
///   at once — so a closed session is retired, not waited on;
/// - **two endpoints at once**, the broker's and this probe's: a third refused, and one minted at
///   once when this probe's is let go.
///
/// **After `power_endpoint_test`, and so after `storage_grant_test`**, whose admin views make the
/// broker resolve its read endpoint for the `logs` grant and keep it for the boot.
fn logs_test(root_ns: u64) -> bool {
    use libkern::{KError, SYS_NS_BIND, SYS_NS_CREATE};
    use librsproto::log::{LEVEL_INFO, OP_LOG_READ, TIER_SYSTEM, parse_read_reply, read_request};
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: logs: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT | libkern::RIGHT_DUPLICATE;
    const MESSAGE: &str = "boot-probe: a record to read back";
    liblog::open_source(root_ns, b"/log/system/boot-probe/logs-test").info(MESSAGE);

    // A read endpoint, bound in a namespace of this probe's own at `/logs`, as the grant binds one.
    let (st, endpoint) = ns_lookup(root_ns, b"/log/read-endpoint", chan);
    if st != 0 || endpoint == 0 {
        return fail(b"no read endpoint through /log/read-endpoint");
    }
    // SAFETY: register-only syscall (its argument is unused); returns a fresh namespace handle.
    let ns = unsafe { syscall1(SYS_NS_CREATE, 0) };
    if ns <= 0 {
        close(endpoint);
        return fail(b"no namespace to bind the read endpoint in");
    }
    let ns = ns as u64;
    let at = b"/logs";
    // SAFETY: a namespace this process created, a valid path, and an endpoint it holds.
    let bound = unsafe { syscall6(SYS_NS_BIND, ns, at.as_ptr() as u64, at.len() as u64, endpoint, 0, 0) };
    close(endpoint);
    if bound != 0 {
        close(ns);
        return fail(b"the read endpoint did not bind");
    }
    let (st, session) = ns_lookup(ns, at, chan);
    if st != 0 || session == 0 {
        close(ns);
        return fail(b"no read session through the endpoint");
    }
    let ask = |id: u64, op: u16, body: &[u8]| -> Option<Received> {
        if !rs_send(session, op, id, body, &[]) {
            return None;
        }
        let deadline = clock_ns() + 5_000_000_000;
        loop {
            let m = receive(session, deadline)?;
            m.handles.iter().for_each(|&h| close(h));
            if m.request_id == id {
                return Some(m);
            }
        }
    };
    // **The record, read back.** The append and the `Read` arrive on different channels, so the
    // first reads may come before the record does: read on from where the last reply ended until
    // it is there, for a few seconds.
    let (mut after, mut id, mut found) = (0u64, 10u64, false);
    let deadline = clock_ns() + 5_000_000_000;
    while !found && clock_ns() < deadline {
        id += 1;
        let Some(m) = ask(id, OP_LOG_READ, &read_request(after, 0)) else {
            close(session);
            close(ns);
            return fail(b"no answer to a Read");
        };
        let Some(reply) = (!m.error).then(|| parse_read_reply(&m.body)).flatten() else {
            close(session);
            close(ns);
            return fail(b"a Read's answer did not read");
        };
        for rec in reply.records() {
            after = rec.sequence;
            found |= rec.principal == b"boot-probe"
                && rec.source == Some(b"logs-test".as_slice())
                && rec.message == MESSAGE.as_bytes()
                && rec.level == LEVEL_INFO
                && rec.tier == TIER_SYSTEM
                && rec.time.is_some();
        }
    }
    let invalid = ask(90, OP_LOG_READ, &read_request(0, 0)[..11]);
    let unsupported = ask(91, OP_LOG_READ + 1, &read_request(0, 0));
    let refused_as = |m: Option<Received>, e: KError| {
        m.is_some_and(|m| m.error && librsproto::error::parse_error(&m.body).is_some_and(|b| b.kerror == e.as_i32()))
    };
    let (short_refused, op_refused) =
        (refused_as(invalid, KError::InvalidArgument), refused_as(unsupported, KError::Unsupported));
    // Two sessions, and a third refused; the second let go, and another taken at once.
    let (st2, second) = ns_lookup(ns, at, chan);
    let (st3, third) = ns_lookup(ns, at, chan);
    close(third);
    close(second);
    let (st4, fourth) = ns_lookup(ns, at, chan);
    close(fourth);
    close(session);
    // Two endpoints: the broker's and this probe's, by `ns`'s binding; a third refused. This
    // probe's let go, and another minted at once.
    let (e3, extra) = ns_lookup(root_ns, b"/log/read-endpoint", chan);
    close(extra);
    close(ns);
    let (e4, again) = ns_lookup(root_ns, b"/log/read-endpoint", chan);
    close(again);
    if !found {
        return fail(b"the record this probe wrote was not read back as written");
    }
    if !short_refused || !op_refused {
        return fail(b"a Read of eleven bytes, or an op that is not Read, was not refused as it should be");
    }
    if st2 != 0 || st3 != KError::WouldBlock.as_i32() || st4 != 0 {
        return fail(b"two read sessions, a third refused, and one at once after one was let go");
    }
    if e3 != KError::WouldBlock.as_i32() || e4 != 0 || again == 0 {
        // **Which half, by number** (2026-10-01): one failure in five runs since the laptop
        // polish's Part A, with two mints in the log rather than three, said only that one of the
        // two did not hold. The statuses say whether the third was granted or the one after the let
        // go was refused, which is the first thing the next occurrence has to tell us.
        Line::new()
            .s(b"boot-probe: logs: third endpoint status ")
            .i(e3 as i64)
            .s(b", after the let go ")
            .i(e4 as i64)
            .end();
        return fail(b"a third read endpoint refused, and one at once after this probe's was let go");
    }
    kprint(b"boot-probe: logs read a record back, refused two malformed requests, and held sessions and endpoints to two ok\n");
    true
}

/// **Accounts, fronted by the broker** (administration Part D.3).
///
/// **First the grant, as a view builds it**: `list /dev`, run in the admin view, names `accounts`
/// there, and `policy` beside it. Then the ops, asked at the protocol on `/svc/views/accounts/<id>`
/// — the suffix the grant's binding forwards to — for a session the probe opens as a supervisor
/// does, under a principal no account has, so no guard sees the probe itself as logged in:
/// - a client channel refuses `AddAccount`, and a name that is not one is refused without a home;
/// - `d3probe` added, its home made with the three folders, and it authenticates;
/// - `Accounts` names it with no session, then with the one the probe opens for it;
/// - its removal refused while that session is open;
/// - `ChangePassword` on its session: a wrong current password refused, and the right one answered
///   only once the session's delay has passed; the new one authenticates and the old does not;
/// - `SetPassword` from the accounts channel;
/// - removing `alice`, the only administrator, refused for that reason, with her logged out;
/// - `d3probe` removed keeping its home, added again adopting it, and removed with it;
/// - a closed session's accounts channel answers nothing.
fn accounts_test(root_ns: u64) -> bool {
    use libkern::KError;
    use librsproto::auth::{build_account_request, build_authenticate_request, parse_authenticate_reply};
    use librsproto::views::*;
    use libstream::wire::Value;
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: accounts: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    let (st, sup) = ns_lookup(root_ns, b"/svc/views/session", chan);
    if st != 0 || sup == 0 {
        return fail(b"no supervisor channel at /svc/views/session");
    }
    let open = |who: &[u8]| match views_call(sup, OP_VIEWS_OPEN_SESSION, 1, who, &[], &mut alloc::vec::Vec::new()) {
        Some((false, body)) => parse_session_id(&body),
        _ => None,
    };
    let close_session = |id: u64| {
        let mut b = [0u8; 8];
        let n = build_session_id(&mut b, id).unwrap_or(0);
        let _ = views_call(sup, OP_VIEWS_CLOSE_SESSION, 9, &b[..n], &[], &mut alloc::vec::Vec::new());
    };

    // The grant, as a view builds it.
    let Some(as_alice) = open(DEMO_USER) else {
        close(sup);
        return fail(b"OpenSession for alice");
    };
    let path = alloc::format!("/svc/views/s/{as_alice}");
    let listed = in_admin_view(root_ns, &path, b"list", &[b"/dev"], &mut alloc::vec::Vec::new());
    close_session(as_alice);
    let names = |rows: &[alloc::vec::Vec<Value>], what: &str| {
        rows.iter().any(|r| r.first() == Some(&Value::Str(alloc::string::String::from(what))))
    };
    if !listed.is_some_and(|(code, rows, _)| code == 0 && names(&rows, "accounts") && names(&rows, "policy")) {
        close(sup);
        return fail(b"list /dev in the admin view does not name accounts and policy");
    }

    // A session of the probe's own, and its channels.
    let Some(probe) = open(b"boot-probe") else {
        close(sup);
        return fail(b"OpenSession for the probe");
    };
    let (sa, acc) = ns_lookup(root_ns, alloc::format!("/svc/views/accounts/{probe}").as_bytes(), chan);
    let (sc, cli) = ns_lookup(root_ns, alloc::format!("/svc/views/s/{probe}").as_bytes(), chan);
    let (so, oracle) = ns_lookup(root_ns, b"/svc/auth", chan);
    let finish = |ok: bool| {
        close(acc);
        close(cli);
        close(oracle);
        close_session(probe);
        close(sup);
        ok
    };
    if sa != 0 || acc == 0 || sc != 0 || cli == 0 || so != 0 || oracle == 0 {
        return finish(fail(b"no accounts channel, client channel and oracle session"));
    }
    match ns_lookup(root_ns, b"/svc/views/accounts/999999", chan) {
        (st, _) if st == KError::NotFound.as_i32() => {}
        _ => return finish(fail(b"a session that is not open has an accounts endpoint")),
    }

    let next = core::cell::Cell::new(100u64);
    let ask = |ch: u64, op: u16, body: &[u8]| {
        next.set(next.get() + 1);
        views_call(ch, op, next.get(), body, &[], &mut alloc::vec::Vec::new())
    };
    // `Some((started, reason))` for an outcome, `None` for anything else.
    let outcome = |r: Option<(bool, alloc::vec::Vec<u8>)>| {
        r.filter(|(err, _)| !err)
            .and_then(|(_, b)| parse_outcome(&b).map(|(o, why)| (o == Outcome::Started, alloc::vec::Vec::from(why))))
    };
    let says = |text: &[u8], what: &[u8]| text.windows(what.len()).any(|w| w == what);
    let refused_for = |r: Option<(bool, alloc::vec::Vec<u8>)>, what: &[u8]| {
        outcome(r).is_some_and(|(started, why)| !started && says(&why, what))
    };
    let pair = |a: &[u8], b: &[u8]| {
        let mut x = [0u8; 256];
        let n = build_account_request(&mut x, a, b).unwrap_or(0);
        x[..n].to_vec()
    };
    let removal = |name: &[u8], home: bool| {
        let mut x = [0u8; 64];
        let n = build_remove_account(&mut x, name, home).unwrap_or(0);
        x[..n].to_vec()
    };
    let change = |current: &[u8], new: &[u8]| {
        let mut x = [0u8; 256];
        let n = build_password_change(&mut x, current, new).unwrap_or(0);
        x[..n].to_vec()
    };
    let authenticates = |name: &[u8], pw: &[u8]| {
        let mut b = [0u8; 256];
        let n = build_authenticate_request(&mut b, name, pw).unwrap_or(0);
        ask(oracle, librsproto::OP_AUTHENTICATE, &b[..n])
            .is_some_and(|(err, body)| !err && parse_authenticate_reply(&body).is_some_and(|r| r.is_authenticated()))
    };
    // Each account `Accounts` names: `(sessions, administers)`.
    let shown = |name: &[u8]| -> Option<(u16, bool)> {
        let (err, body) = ask(cli, OP_VIEWS_ACCOUNTS, &[])?;
        let mut found = None;
        parse_accounts(&body, |r| {
            if r.name == name {
                found = Some((r.sessions, r.administers));
            }
        })
        .filter(|_| !err)?;
        found
    };
    const NAME: &[u8] = b"d3probe";
    const HOME: &[u8] = b"/home/d3probe";
    let homed = || {
        libfs::is_dir(root_ns, HOME)
            && libfs::HOME_FOLDERS.iter().all(|f| libfs::is_dir(root_ns, libfs::join(HOME, f.as_bytes()).as_bytes()))
    };

    // Only an accounts channel adds, and a name has to be one.
    if !ask(cli, OP_VIEWS_ADD_ACCOUNT, &pair(NAME, b"first secret")).is_some_and(|(err, _)| err) {
        return finish(fail(b"a client channel answered AddAccount"));
    }
    if !refused_for(ask(acc, OP_VIEWS_ADD_ACCOUNT, &pair(b"../d3", b"first secret")), b"a name is 1 to 32 bytes")
        || libfs::is_dir(root_ns, b"/d3")
    {
        return finish(fail(b"a name that is not one was not refused, or made a directory"));
    }

    // Added, with a home and its folders.
    match outcome(ask(acc, OP_VIEWS_ADD_ACCOUNT, &pair(NAME, b"first secret"))) {
        Some((true, why)) if says(&why, b"with a new home at /home/d3probe") => {}
        _ => return finish(fail(b"AddAccount did not add d3probe with a new home")),
    }
    if !homed() || !authenticates(NAME, b"first secret") {
        return finish(fail(b"d3probe has no home with its folders, or does not authenticate"));
    }
    if !refused_for(ask(acc, OP_VIEWS_ADD_ACCOUNT, &pair(NAME, b"other")), b"already exists") {
        return finish(fail(b"a second add of d3probe was not refused"));
    }
    if shown(NAME) != Some((0, false)) || shown(DEMO_USER) != Some((0, true)) {
        return finish(fail(b"Accounts does not show d3probe with no session, and alice administering"));
    }

    // Logged in: shown with its session, and not removed.
    let Some(as_d3) = open(NAME) else {
        return finish(fail(b"OpenSession for d3probe"));
    };
    let (sd, d3) = ns_lookup(root_ns, alloc::format!("/svc/views/s/{as_d3}").as_bytes(), chan);
    let finish = |ok: bool| {
        close(d3);
        close_session(as_d3);
        finish(ok)
    };
    if sd != 0 || d3 == 0 {
        return finish(fail(b"no client channel for d3probe's session"));
    }
    if shown(NAME) != Some((1, false)) {
        return finish(fail(b"Accounts does not show d3probe's session"));
    }
    if !refused_for(ask(acc, OP_VIEWS_REMOVE_ACCOUNT, &removal(NAME, true)), b"d3probe is logged in")
        || !authenticates(NAME, b"first secret")
        || !homed()
    {
        return finish(fail(b"d3probe was removed while logged in"));
    }

    // Its own password: a wrong current one refused, and the next check held for the delay.
    if !refused_for(ask(d3, OP_VIEWS_CHANGE_PASSWORD, &change(b"wrong", b"second secret")), b"wrong password") {
        return finish(fail(b"ChangePassword took a wrong current password"));
    }
    let asked_at = clock_ns();
    let changed = outcome(ask(d3, OP_VIEWS_CHANGE_PASSWORD, &change(b"first secret", b"second secret")));
    let waited = clock_ns().saturating_sub(asked_at);
    if !changed.is_some_and(|(started, _)| started) {
        return finish(fail(b"ChangePassword with the right current password was not answered Started"));
    }
    // The broker's delay is two seconds (`view_broker::pacing::FAIL_DELAY_NS`); unpaced, the
    // answer takes milliseconds.
    if waited < 1_500_000_000 {
        return finish(fail(b"ChangePassword after a wrong one was answered without the session's delay"));
    }
    if !authenticates(NAME, b"second secret") || authenticates(NAME, b"first secret") {
        return finish(fail(b"after ChangePassword, the new password is refused or the old one taken"));
    }
    close(d3);
    close_session(as_d3);
    let finish = |ok: bool| {
        close(acc);
        close(cli);
        close(oracle);
        close_session(probe);
        close(sup);
        ok
    };
    if shown(NAME) != Some((0, false)) {
        return finish(fail(b"Accounts still shows d3probe's closed session"));
    }

    // An administrator's `SetPassword`, with no current password to prove.
    if !outcome(ask(acc, OP_VIEWS_SET_PASSWORD, &pair(NAME, b"third secret"))).is_some_and(|(s, _)| s)
        || !authenticates(NAME, b"third secret")
    {
        return finish(fail(b"SetPassword did not set d3probe's password"));
    }

    // The only administrator, logged out, is not removed.
    let why: &[u8] = b"removing alice would leave no account that could use `views` for every program";
    if !refused_for(ask(acc, OP_VIEWS_REMOVE_ACCOUNT, &removal(DEMO_USER, false)), why)
        || !authenticates(DEMO_USER, DEMO_PASSWORD)
    {
        return finish(fail(b"removing alice, the only administrator, was not refused for that reason"));
    }

    // Removed keeping the home, added again adopting it, and removed with it.
    match outcome(ask(acc, OP_VIEWS_REMOVE_ACCOUNT, &removal(NAME, false))) {
        Some((true, why)) if says(&why, b"keeping /home/d3probe") => {}
        _ => return finish(fail(b"RemoveAccount without the home did not remove d3probe")),
    }
    if authenticates(NAME, b"third secret") || !homed() {
        return finish(fail(b"a removed d3probe still authenticates, or its kept home went"));
    }
    match outcome(ask(acc, OP_VIEWS_ADD_ACCOUNT, &pair(NAME, b"fourth secret"))) {
        Some((true, why)) if says(&why, b"adopting /home/d3probe") => {}
        _ => return finish(fail(b"adding d3probe again did not adopt its home")),
    }
    match outcome(ask(acc, OP_VIEWS_REMOVE_ACCOUNT, &removal(NAME, true))) {
        Some((true, why)) if says(&why, b"and /home/d3probe with it") => {}
        _ => return finish(fail(b"RemoveAccount with the home did not remove both")),
    }
    if libfs::is_dir(root_ns, HOME) || authenticates(NAME, b"fourth secret") {
        return finish(fail(b"d3probe's home or record outlived its removal"));
    }
    if !refused_for(ask(acc, OP_VIEWS_REMOVE_ACCOUNT, &removal(NAME, true)), b"no account is named d3probe") {
        return finish(fail(b"a second removal of d3probe was not refused"));
    }

    // A closed session's accounts channel closes with it.
    close_session(probe);
    let answered = ask(acc, OP_VIEWS_SET_PASSWORD, &pair(NAME, b"fifth secret")).is_some();
    close(acc);
    close(cli);
    close(oracle);
    close(sup);
    if answered {
        return fail(b"a closed session's accounts channel still answered");
    }
    kprint(b"boot-probe: accounts: the grant binds /dev/accounts; an account added with its home, shown with its session, kept while logged in, its password changed after the delay and set; the only administrator kept; a home kept, adopted and removed; a closed session's channel closed ok\n");
    true
}

/// **`auth-service`'s admin session, and the file it writes** (administration Part D.1).
///
/// Asked directly, as the view broker will ask it, on a session resolved at `/svc/auth/admin`:
/// - an account added, then authenticated against on an ordinary oracle session;
/// - its password set, after which the old one is refused and the new one accepted;
/// - the account removed, after which it is refused altogether.
///
/// **After each write, the file is read from the device**, raw, through the ext4 library: a
/// re-resolve would read the page cache, and what is at stake is that the rename landed. `alice`'s
/// line is the same bytes throughout. Each refusal is its own, and a suffix the service does not
/// serve is `NotFound` — the whole error body, where a short one once arrived as `KernelError`.
fn auth_admin_test(root_ns: u64) -> bool {
    use libkern::KError;
    use librsproto::auth::{
        OP_AUTH_ADD, OP_AUTH_LIST, OP_AUTH_REMOVE, OP_AUTH_SET_PASSWORD, build_account_request,
        build_authenticate_request, parse_account_list, parse_authenticate_reply,
    };
    let fail = |what: &[u8]| {
        Line::new().s(b"boot-probe: auth admin: ").s(what).s(b" FAIL").end();
        false
    };
    let chan = libkern::RIGHT_SEND | libkern::RIGHT_RECV | libkern::RIGHT_WAIT;
    match ns_lookup(root_ns, b"/svc/auth/nope", chan) {
        (st, _) if st == KError::NotFound.as_i32() => {}
        _ => return fail(b"a suffix auth-service does not serve was not NotFound"),
    }
    let (st, admin) = ns_lookup(root_ns, b"/svc/auth/admin", chan);
    let (so, oracle) = ns_lookup(root_ns, b"/svc/auth", chan);
    if st != 0 || admin == 0 || so != 0 || oracle == 0 {
        return fail(b"no admin session and oracle session at /svc/auth");
    }
    let Some(dev) = RootDevice::open(root_ns) else {
        return fail(b"the root device would not open");
    };
    let finish = |ok: bool| {
        close(admin);
        close(oracle);
        ok
    };

    let next = core::cell::Cell::new(0u64);
    let ask = |ch: u64, op: u16, body: &[u8]| -> Option<Received> {
        next.set(next.get() + 1);
        let id = next.get();
        if !rs_send(ch, op, id, body, &[]) {
            return None;
        }
        let deadline = clock_ns() + 10_000_000_000;
        loop {
            let m = receive(ch, deadline)?;
            if m.request_id == id {
                return Some(m);
            }
        }
    };
    let account = |name: &[u8], pw: &[u8]| {
        let mut b = [0u8; 256];
        let n = build_account_request(&mut b, name, pw).unwrap_or(0);
        b[..n].to_vec()
    };
    let refused_with = |m: Option<Received>| {
        m.filter(|m| m.error).and_then(|m| librsproto::error::parse_error(&m.body).map(|e| e.kerror))
    };
    // `Some(home)` if the oracle authenticates `name` with `pw`, `None` if it refuses.
    let authenticates = |name: &[u8], pw: &[u8]| -> Option<alloc::vec::Vec<u8>> {
        let mut b = [0u8; 256];
        let n = build_authenticate_request(&mut b, name, pw)?;
        let m = ask(oracle, librsproto::OP_AUTHENTICATE, &b[..n])?;
        let r = parse_authenticate_reply(&m.body)?;
        r.is_authenticated().then(|| r.home.to_vec())
    };
    // The file as the device holds it, and the line in it naming `name`.
    let on_device = || -> Option<alloc::vec::Vec<u8>> {
        let size = fs_server_ext4::ext4::stat_file(&dev, b"/system/users").ok()?;
        dev.read_file(b"/system/users", 0, size)
    };
    let line_of = |file: &[u8], name: &[u8]| -> Option<alloc::vec::Vec<u8>> {
        file.split(|&b| b == b'\n')
            .find(|l| l.len() > name.len() && l.starts_with(name) && l[name.len()] == b':')
            .map(|l| l.to_vec())
    };
    let Some(before) = on_device() else {
        return finish(fail(b"/system/users would not read from the device"));
    };
    let Some(alice) = line_of(&before, b"alice") else {
        return finish(fail(b"alice is not in /system/users on the device"));
    };
    let alice_unchanged = |file: &[u8]| line_of(file, b"alice").as_deref() == Some(&alice[..]);

    // `List`: alice, and her home.
    let listed = |m: Option<Received>, name: &[u8]| {
        m.filter(|m| !m.error)
            .is_some_and(|m| parse_account_list(&m.body).is_some_and(|l| l.iter().any(|(n, _)| n == name)))
    };
    if !listed(ask(admin, OP_AUTH_LIST, &[]), b"alice") {
        return finish(fail(b"List does not name alice"));
    }

    // An account added: the oracle knows it, and so does the device.
    const NAME: &[u8] = b"d1probe";
    let added = ask(admin, OP_AUTH_ADD, &account(NAME, b"first secret"));
    if !added.as_ref().is_some_and(|m| !m.error) {
        return finish(fail(b"Add was not answered"));
    }
    if authenticates(NAME, b"first secret").as_deref() != Some(&b"/home/d1probe"[..]) {
        return finish(fail(b"the added account does not authenticate, with its home"));
    }
    let Some(file) = on_device() else {
        return finish(fail(b"/system/users would not read after the add"));
    };
    let Some(first) = line_of(&file, NAME).filter(|l| l.ends_with(b":/home/d1probe")) else {
        return finish(fail(b"the added account is not on the device"));
    };
    if !alice_unchanged(&file) {
        return finish(fail(b"alice's line changed when another account was added"));
    }

    // Each refusal, its own.
    if refused_with(ask(admin, OP_AUTH_ADD, &account(NAME, b"x"))) != Some(KError::AlreadyExists.as_i32()) {
        return finish(fail(b"a second Add of one name was not AlreadyExists"));
    }
    if refused_with(ask(admin, OP_AUTH_ADD, &account(b"Bad Name", b"x"))) != Some(KError::InvalidArgument.as_i32()) {
        return finish(fail(b"a bad name was not InvalidArgument"));
    }
    if refused_with(ask(admin, librsproto::OP_AUTHENTICATE, &[])) != Some(KError::Unsupported.as_i32()) {
        return finish(fail(b"an admin session authenticated"));
    }
    if !ask(oracle, OP_AUTH_LIST, &[]).is_some_and(|m| m.error) {
        return finish(fail(b"an oracle session answered List"));
    }

    // A new password: the old refused, the new accepted, and the device's line another.
    if !ask(admin, OP_AUTH_SET_PASSWORD, &account(NAME, b"second secret")).is_some_and(|m| !m.error) {
        return finish(fail(b"SetPassword was not answered"));
    }
    if authenticates(NAME, b"first secret").is_some() || authenticates(NAME, b"second secret").is_none() {
        return finish(fail(b"after SetPassword the old password still works, or the new one does not"));
    }
    let Some(file) = on_device() else {
        return finish(fail(b"/system/users would not read after SetPassword"));
    };
    if line_of(&file, NAME).is_none_or(|l| l == first) || !alice_unchanged(&file) {
        return finish(fail(b"the new password is not on the device, or alice's line moved"));
    }

    // Removed: refused, gone from the device and the list, and a second removal NotFound.
    if !ask(admin, OP_AUTH_REMOVE, NAME).is_some_and(|m| !m.error) {
        return finish(fail(b"Remove was not answered"));
    }
    if authenticates(NAME, b"second secret").is_some() {
        return finish(fail(b"a removed account still authenticates"));
    }
    let Some(file) = on_device() else {
        return finish(fail(b"/system/users would not read after Remove"));
    };
    if line_of(&file, NAME).is_some() || !alice_unchanged(&file) {
        return finish(fail(b"the removed account is still on the device, or alice's line moved"));
    }
    if listed(ask(admin, OP_AUTH_LIST, &[]), NAME) {
        return finish(fail(b"List still names the removed account"));
    }
    if refused_with(ask(admin, OP_AUTH_REMOVE, NAME)) != Some(KError::NotFound.as_i32()) {
        return finish(fail(b"a second Remove was not NotFound"));
    }
    if authenticates(b"alice", DEMO_PASSWORD).is_none() {
        return finish(fail(b"alice no longer authenticates"));
    }
    kprint(b"boot-probe: auth admin: an account added, authenticated, its password changed and removed, each on the device with alice's line untouched, each refusal its own ok\n");
    finish(true)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"boot-probe: PANIC\n");
    // A panicking probe must not let the run pass by defaulting to silence.
    verdict(false);
    exit(1);
}
