//! `boot-probe` — the in-guest substrate checks, and the boot verdict.
//!
//! **Why a program and not a phase of a supervisor.** The verdict lives here, so the probes
//! that gate it live here too: the scheduler and hard-float gates, the filesystem tests and
//! the rest. `init` still writes a FAIL verdict for a critical-path boot failure or a crashed
//! demo chain; that is answered before this program starts.
//!
//! **The ordering that makes the gates meaningful is `init`'s, and it is serial.**
//! `init::supervise` runs the demo chain synchronously and fails the run on a non-zero
//! exit, and only then hands off to the login chain that reaches `service-mgr` and this
//! program. So everything the run adjudicates has already happened when the gates run, and
//! they are the last thing before the only `SYS_TEST_EXIT(PASS)` call.
//!
//! **Started by `service-mgr`** from `/system/services.toml`, which carries a
//! `[service.boot-probe]` table only in selftest / test-harness images: a LOOKUP-only root
//! namespace, no syscaps, and `policy = "never"`.

#![no_std]
#![no_main]

extern crate alloc;

/// `Line` builds its text on the heap, so this bin needs an allocator.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

use libkern::abi::{HandleInfo, IPC_MSG_SIZE};
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

mod accounts;
mod authority;
mod deadlines;
mod exits;
mod files;
mod gates;
mod policy;
mod received;
mod registry;
mod services;
mod storage;
mod storage_admin;
mod views;

use accounts::*;
use authority::*;
use deadlines::*;
use exits::*;
use files::*;
use gates::*;
use policy::*;
use received::*;
use registry::*;
use services::*;
use storage::*;
use storage_admin::*;
use views::*;

/// Page size, for the mapped-file checks below.
const PAGE: u64 = 4096;
/// A millisecond, in the monotonic clock's nanoseconds.
const MS: u64 = 1_000_000;

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
/// every check that must gate the run has to happen before it and in this process.
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
/// **The control endpoint is held until exit, and closing it early is a bug.** `service-mgr`
/// reads *this handle's* closure as "the child is gone" (`supervise`), because a pid on
/// `KIND_CHILD_EXITED` cannot be matched to a process handle. Closing it early reports a death
/// that has not happened, and under `policy = "always"` starts a *second copy of a live
/// service*. So hold it and let process teardown close it; this is a contract on every declared
/// service (`docs/spec/service-toml-schema.md`).
///
/// **The verdict is fired, then the process exits.** `verdict` returns everywhere but
/// `test-qemu`, the only gate with the `isa-debug-exit` device.
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
        & refused_create_test(root_ns)
        & copy_into_itself_test(root_ns)
        & file_cache_test(root_ns)
        & unlinked_file_test(root_ns)
        & flush_test(root_ns)
        & block_bounds_test(root_ns)
        & dot_entries_test(root_ns)
        & removed_dir_session_test(root_ns)
        & power_refused_test(root_ns)
        & clock_set_refused_test()
        & bind_needs_transfer_test()
        & transfer_list_test()
        & payload_sealed_test()
        & map_needs_read_test()
        & spawn_copy_needs_duplicate_test(root_ns)
        & lookup_po_stays_test(root_ns)
        & exit_with_a_waiter_test(root_ns)
        & exit_with_a_running_sibling_test(root_ns)
        & exit_while_spawning_test(root_ns)
        & thread_exit_under_load_test(root_ns)
        & many_timers_test()
        & received_closes_what_was_not_taken_test()
        & receive_closes_what_its_caller_does_not_take_test()
        & sessions_close_what_they_were_sent_test(root_ns)
        & tty_backend_closes_what_it_was_sent_test(root_ns)
        & setup_closes_what_its_fields_do_not_name_test(root_ns)
        & ipc_port_closes_what_it_was_sent_test()
        & auth_closes_what_it_was_sent_test(root_ns)
        & log_closes_what_it_was_sent_test(root_ns)
        & continuation_test(root_ns)
        & subtree_bind_test(root_ns)
        & auth_multi_client_test(root_ns)
        & ns_derive_test(root_ns)
        & view_broker_test(root_ns)
        & view_grants_end_test(root_ns)
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
    // boots this image without `isa-debug-exit`, so `SYS_TEST_EXIT` returns `Unsupported`.
    //
    // The exit code tracks the gates, and `service-mgr` reports it. `check-terminal`'s
    // `check_service_attribution` requires `code=0`, so a `sched_gate` or `fp_gate` regression
    // fails **that** gate with a message about attribution; the `boot-probe: … FAIL` line naming
    // the real cause is directly above it in the same transcript.
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
    sent: libkern::Received,
}

impl Received {
    /// Take handle `i` of the message: the caller's now. Those not taken close as this drops.
    fn take(&mut self, i: usize) -> Option<u64> {
        self.sent.take(i)
    }

    /// How many handles the message carried, taken or not.
    fn count(&self) -> usize {
        self.sent.count()
    }
}

/// The next message on `ch`, waiting until `deadline` on the monotonic clock — `0` only looks.
/// `None` if nothing came by then, or if what came is not rsproto.
fn receive(ch: u64, deadline: u64) -> Option<Received> {
    let mut buf = [0u8; IPC_MSG_SIZE];
    loop {
        if let Ok(sent) = libkern::channel_recv(ch, &mut buf) {
            let len = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]) as usize;
            let m = librsproto::decode(&buf[24..24 + len.min(4096 - 24)]).ok()?;
            return Some(Received {
                op: m.op,
                request_id: m.request_id,
                error: m.is_error(),
                body: m.body.to_vec(),
                sent,
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
    unsafe { syscall2(SYS_MEMORY_UNMAP, addr as u64, size) };
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

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"boot-probe: PANIC\n");
    // A panicking probe must not let the run pass by defaulting to silence.
    verdict(false);
    exit(1);
}
