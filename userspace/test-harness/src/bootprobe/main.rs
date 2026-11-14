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

use alloc::vec::Vec;
use libkern::Rights;
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{
    CanLookup, Carried, Channel, Deadline, File, Handle, MapRead, MapReadWrite, Mapping, Memory, Msg, Namespace,
    NsMutable, NsReadOnly, Only, ReadOnly, ReadWrite, Ref, Resource, WaitSet, block_on, exit, fs, system, time,
};

mod accounts;
mod authority;
mod deadlines;
mod exits;
mod file_io;
mod filelib;
mod files;
mod gates;
mod mapping;
mod policy;
mod received;
mod refused;
mod registry;
mod replies;
mod services;
mod setup;
mod storage;
mod storage_admin;
mod surface;
mod tty;
mod views;

use accounts::*;
use authority::*;
use deadlines::*;
use exits::*;
use file_io::*;
use filelib::*;
use files::*;
use gates::*;
use mapping::*;
use policy::*;
use received::*;
use registry::*;
use replies::*;
use services::*;
use setup::*;
use storage::*;
use storage_admin::*;
use surface::*;
use tty::*;
use views::*;

/// A namespace this process may look names up in.
type Root<'a> = Ref<'a, Namespace, NsReadOnly>;
/// A channel endpoint this process owns.
type Chan = Handle<Channel, Only>;
/// The two ends of a channel.
type Pipe = (Chan, Chan);

/// What a file is asked for to read and write through a mapping.
const RW: Rights = Rights::from_bits(libkern::RIGHT_MAP_READ | libkern::RIGHT_MAP_WRITE);

/// Page size, for the mapped-file checks below.
const PAGE: u64 = 4096;
/// A millisecond, in the monotonic clock's nanoseconds.
const MS: u64 = 1_000_000;

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
pub extern "C" fn _start(notif: u64, root_ns: u64, control: u64, arg0: u64) -> ! {
    // SAFETY: the four registers as the kernel set them at entry, wrapped here and nowhere else.
    let mut start = unsafe { Start::from_registers(notif, root_ns, control, arg0) };
    log::write(b"boot-probe: up\n");
    let Some(root) = start.root.take() else {
        log::write(b"boot-probe: no root namespace FAIL\n");
        verdict(false);
        exit(1);
    };
    let root = root.as_ref();
    // `&` and not `&&`: every check runs and reports, so one failure does not hide the
    // rest — a boot that fails three of these should say three, not one.
    let ok = sched_gate(root)
        & fp_gate()
        & copy_from_pages_not_faulted_in_test(root)
        & map_window_test(root)
        & anywhere_maps_test()
        & read_large_file(root)
        & overwrite_test(root)
        & grow_test(root)
        & create_test(root)
        & file_io_test(root)
        & nocache_test(root)
        & refused_create_test(root)
        & copy_into_itself_test(root)
        & file_cache_test(root)
        & unlinked_file_test(root)
        & libfs_refuses_a_directory_and_a_large_file_test(root)
        & save_leaves_the_file_when_it_fails_test(root)
        & a_refused_setup_message_closes_its_handles_test()
        & flush_test(root)
        & block_bounds_test(root)
        & dot_entries_test(root)
        & removed_dir_session_test(root)
        & power_refused_test(root)
        & clock_set_refused_test()
        & bind_needs_transfer_test()
        & transfer_list_test()
        & payload_sealed_test()
        & map_needs_read_test()
        & spawn_copy_needs_duplicate_test(root)
        & lookup_po_stays_test(root)
        & store_is_read_only_test(root)
        & exit_with_a_waiter_test(root)
        & exit_with_a_running_sibling_test(root)
        & exit_while_spawning_test(root)
        & thread_exit_under_load_test(root)
        & started_handles_are_kept_test(root)
        & many_timers_test()
        & refused::deadlines::wait_on_nothing_test()
        & received_closes_what_was_not_taken_test()
        & receive_closes_what_its_caller_does_not_take_test()
        & sessions_close_what_they_were_sent_test(root)
        & tty_backend_closes_what_it_was_sent_test(root)
        & tty_refuses_a_backend_it_cannot_wait_on_test(root)
        & tty_reattach_closes_the_old_backend_test(root)
        & setup_closes_what_its_fields_do_not_name_test(root)
        & ipc_port_closes_what_it_was_sent_test()
        & auth_closes_what_it_was_sent_test(root)
        & log_closes_what_it_was_sent_test(root)
        & continuation_test(root)
        & subtree_bind_test(root)
        & home_rules_are_the_kernels_test()
        & auth_multi_client_test(root)
        & a_reply_is_its_requests_test(root)
        & a_session_takes_memory_and_whole_bodies_test(root)
        & ns_derive_test(root)
        & view_broker_test(root)
        & view_grants_end_test(root)
        & registry_test(root)
        & past_two_tib_test(root)
        & devices_test(root)
        & storage_test(root)
        & storage_mount_test(root)
        & io_counters_test(root)
        & long_listing_test(root)
        & storage_admin_test(root)
        & storage_grant_test(root)
        & auth_admin_test(root)
        & policy_test(root)
        & grace_test(root)
        & accounts_test(root)
        & restart_test(root)
        & services_test(root)
        & power_endpoint_test(root)
        & logs_test(root);
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

/// The monotonic clock, in nanoseconds.
fn clock_ns() -> u64 {
    time::now().unwrap_or(0)
}

/// Resolve `path` in `ns` with `rights`, awaiting the `PendingOperation`.
fn lookup<'a, T, M2, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
    path: &str,
    rights: Rights,
) -> libos::Result<Handle<T, M2>> {
    block_on(ns.into().lookup(path, rights))
}

/// What a lookup's result says, as the `KError` it carries or `0`.
fn status<T>(r: &libos::Result<T>) -> i32 {
    r.as_ref().map_or_else(|e| e.code().as_i32(), |_| 0)
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
    log::write(if ok {
        b"boot-probe: test-harness verdict PASS\n"
    } else {
        b"boot-probe: test-harness verdict FAIL\n"
    });
    system::test_verdict(ok);
}

/// The bytes of the file at `path` in `ns`, read through a mapping and copied out.
fn read_path<'a, M: CanLookup + 'a>(ns: impl Into<Ref<'a, Namespace, M>>, path: &str) -> Option<Vec<u8>> {
    let h = lookup::<File, MapRead, _>(ns, path, Rights::MAP_READ | Rights::INSPECT).ok()?;
    read_all(&h)
}

/// One message on `ch` of rsproto `op`, moving `handles`, without waiting for an answer — to the
/// view broker, or down an endpoint as the kernel would forward a resolve. A send refused leaves
/// `handles` to close as they drop.
fn rs_send<'a>(
    ch: impl Into<Ref<'a, Channel, Only>>,
    op: u16,
    request_id: u64,
    body: &[u8],
    mut handles: Carried,
) -> bool {
    let mut msg = Msg::new();
    let Some(n) = librsproto::encode(msg.payload_mut(), op, request_id, 0, body, handles.len() as u16) else {
        return false;
    };
    msg.set_len(n).is_ok() && ch.into().send(&msg, &mut handles).is_ok()
}

/// `h`, alone, to move in a send.
fn carry<T, M>(h: Handle<T, M>) -> Carried {
    let mut handles = Carried::new();
    // A fresh `Carried` has room for one.
    let _ = handles.push(h);
    handles
}

/// One exchange with the view broker: send `op` on `ch` carrying `body` and moving `handles`, and
/// wait — at most ten seconds — for the reply to it. An `Exited` that arrives first is set aside in
/// `exited` rather than mistaken for the reply. `None` if nothing came.
fn views_call<'a>(
    ch: impl Into<Ref<'a, Channel, Only>>,
    op: u16,
    request_id: u64,
    body: &[u8],
    handles: Carried,
    exited: &mut Vec<Vec<u8>>,
) -> Option<(bool, Vec<u8>)> {
    let ch = ch.into();
    if !rs_send(ch, op, request_id, body, handles) {
        return None;
    }
    views_receive(ch, request_id, exited)
}

/// Wait for the message with `request_id` on `ch` (`0` for the next `Exited`), setting any other
/// `Exited` aside in `exited`. Ten seconds at most.
fn views_receive<'a>(
    ch: impl Into<Ref<'a, Channel, Only>>,
    request_id: u64,
    exited: &mut Vec<Vec<u8>>,
) -> Option<(bool, Vec<u8>)> {
    let ch = ch.into();
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
    body: Vec<u8>,
    sent: Carried,
}

impl Received {
    /// Take handle `i` of the message: the caller's now. Those not taken close as this drops.
    fn take<T, M>(&mut self, i: usize) -> Option<Handle<T, M>> {
        self.sent.take(i)
    }

    /// How many handles the message carried, taken or not.
    fn count(&self) -> usize {
        self.sent.len()
    }
}

/// Wait on `ch` until `deadline` on the monotonic clock, or until it signals.
fn wait_for(ch: Ref<'_, Channel, Only>, deadline: u64) {
    let mut set = WaitSet::<1>::new();
    if set.add(ch).is_ok() {
        let _ = set.wait(Deadline::at(deadline));
    }
}

/// The next message on `ch`, waiting until `deadline` on the monotonic clock — `0` only looks.
/// `None` if nothing came by then, or if what came is not rsproto.
fn receive<'a>(ch: impl Into<Ref<'a, Channel, Only>>, deadline: u64) -> Option<Received> {
    let ch = ch.into();
    let mut msg = Msg::new();
    loop {
        if let Ok(sent) = ch.recv(&mut msg) {
            let m = librsproto::decode(msg.payload()).ok()?;
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
        wait_for(ch, deadline);
    }
}

/// Map a read-only object and copy it out, so nothing borrows the mapping past this call.
fn read_all(h: &Handle<File, MapRead>) -> Option<Vec<u8>> {
    let size = h.stat().ok()?.size as usize;
    let mapping = h.map(size).ok()?;
    let mut bytes = alloc::vec![0u8; size];
    mapping.read_into(0, &mut bytes).ok()?;
    Some(bytes)
}

/// `/dev/registry`'s records, read through `ns`. `None` if it is not bound there or does not read.
fn registry_records<'a, M: CanLookup + 'a>(
    ns: impl Into<Ref<'a, Namespace, M>>,
) -> Option<Vec<libkern::device::DeviceRecord>> {
    let snap: Handle<File, MapRead> = lookup(ns, "/dev/registry", Rights::MAP_READ | Rights::INSPECT).ok()?;
    let bytes = read_all(&snap);
    drop(snap);
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
    root: Root<'_>,
    client_path: &str,
    program: &[u8],
    args: &[&[u8]],
    exited: &mut Vec<Vec<u8>>,
) -> Option<(i64, Vec<Vec<libstream::wire::Value>>, Vec<u8>)> {
    use librsproto::views::*;
    let cli: Handle<Channel, Only> = lookup(root, client_path, Rights::SEND | Rights::RECV | Rights::WAIT).ok()?;
    let copy = root.copy().ok()?;
    let (out_w, out_r) = Handle::<Channel, Only>::pair(16).ok()?;
    let (err_w, err_r) = Handle::<Channel, Only>::pair(16).ok()?;
    let mut req = [0u8; 256];
    let n = build_request(&mut req, REQ_STDOUT | REQ_STDERR, b"admin", program, args, b"", None)?;
    let mut granted = Carried::new();
    let _ = granted.push(copy);
    let _ = granted.push(out_w);
    let _ = granted.push(err_w);
    let asked = matches!(
        views_call(&cli, OP_VIEWS_REQUEST, 1, &req[..n], granted, exited),
        Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::NeedPassword, _)))
    ) && matches!(
        views_call(&cli, OP_VIEWS_PASSWORD, 2, DEMO_PASSWORD, Carried::new(), exited),
        Some((false, ref body)) if matches!(parse_outcome(body), Some((Outcome::Started, _)))
    );
    let listing = if asked {
        libstream::channel::ChannelReceiver::new(libstream::channel::IpcPort::owning(out_r)).receive(1 << 20).ok()
    } else {
        drop(out_r);
        None
    };
    // Every diagnostic is one message, and the program has closed its end by now: read to
    // the close.
    let mut said = Vec::new();
    if asked {
        use libstream::channel::MsgPort;
        let mut port = libstream::channel::IpcPort::owning(err_r);
        while port.recv(&mut said).is_ok() {}
    } else {
        drop(err_r);
    }
    let code = if asked { exited.pop().or_else(|| views_receive(&cli, 0, exited).map(|r| r.1)) } else { None };
    drop(cli);
    let code = code.as_deref().and_then(parse_exited)?.0 as i64;
    let mut rows = Vec::new();
    if let Some(mut tr) = listing.as_deref().and_then(|b| libstream::table::TableReader::new(b).ok()) {
        while let Some(Ok(libstream::table::Item::Row(vals))) = tr.next() {
            rows.push(vals);
        }
    }
    Some((code, rows, said))
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"boot-probe: PANIC\n");
    // A panicking probe must not let the run pass by defaulting to silence.
    verdict(false);
    exit(1);
}
