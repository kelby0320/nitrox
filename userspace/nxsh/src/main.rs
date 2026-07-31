//! `nxsh` — the Nitrox shell, host side.
//!
//! Milestone 3 Part C. The language lives in the library; this is the [`Host`]
//! implementation behind it: resolving a program image, creating pipes, spawning a
//! process per stage, reaping exits, and reading the last stage's stream.
//!
//! ```text
//! nxsh script.nx      # run a script
//! nxsh -c 'SOURCE'    # run one line
//! ```
//!
//! ## What is real, and what is captured
//!
//! The pipes *between* stages are bounded IPC channels and the stages run concurrently —
//! §1's model, with backpressure falling out of the channel depth rather than any flow
//! control the shell performs. Stages are spawned **before** any of them is reaped, which
//! is what makes that true: reaping one at a time would deadlock the moment a stage
//! produced more than one channel's worth. Only the last stage's output is read into
//! memory, because the shell must turn it into a `Value` for the operators that follow.
//!
//! ## Exit status (D7)
//!
//! Distinct codes on purpose: a harness adjudicating from an exit code has to tell a
//! malformed script from a script that ran and reported failure.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libkern::abi::{HandleInfo, SPAWN_MAX_HANDLES, SpawnArgs};
use libkern::handle::{RIGHT_INSPECT, RIGHT_MAP_READ};
use libkern::syscall::{
    SYS_FILE_CREATE, SYS_FILE_SYNC, SYS_HANDLE_CLOSE, SYS_HANDLE_STAT, SYS_MEMORY_MAP,
    SYS_MEMORY_UNMAP, SYS_NOTIF_RECV, SYS_NS_LOOKUP, SYS_PROCESS_SPAWN, SYS_WAIT, syscall1,
    syscall2, syscall4, syscall5,
};
use libkern::{exit, kprint};
use libstream::channel::{ChannelReceiver, ChannelSink, IpcPort};
use libstream::wire::ByteSink;
use libstream::setup::{Streams, bootstrap, bootstrap_arg0, pipe, send_setup};
use nxsh::host::{Host, PipelineRun, StageSpec, StageStatus};
use nxsh::{Interp, RunMode};

/// `alloc` backing: the parser, the evaluator and the TSM1 codec all allocate.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

const EXIT_OK: i64 = 0;
const EXIT_SCRIPT_FAILED: i64 = 1;
const EXIT_PARSE_ERROR: i64 = 2;
const EXIT_USAGE: i64 = 64;

/// Where an unqualified program name is looked up.
///
/// A short fixed list, not an ambient search path. §9h rejects `PATH`-style resolution
/// for module imports on the grounds that a name crossing a file boundary should be
/// explicit; a program name crosses the same boundary, and the real answer is namespace
/// construction — a session sees the programs its namespace was built with.
const PROGRAM_DIRS: &[&[u8]] = &[b"/bin/", b"/initramfs/sbin/"];

/// Cap on a captured stage output. Unbounded capture would let a spawned program run the
/// shell out of memory.
const MAX_CAPTURE: usize = 1 << 20;

/// `ChildExited` notification kind (`docs/spec/notification-format.md`).
const KIND_CHILD_EXITED: u32 = 0x0200;
/// `ExitStatus.kind` for a normal exit; anything else means the process died.
const EXIT_KIND_NORMAL: u32 = 0;

// `libkern`'s own type, not a local copy: the body sits at offset **4**, and a local
// struct with a `u32` pad put it at 8 — which silently read the exit *code* as the exit
// *kind* and reported every non-zero exit as a crash.
static mut NOTIF: libkern::abi::Notification =
    libkern::abi::Notification { kind: 0, body: [0; 60] };
static mut WAIT_HANDLES: [u64; 1] = [0; 1];
static mut WAIT_RESULTS: [u8; 24] = [0; 24];
static mut SPAWN: SpawnArgs = SpawnArgs {
    image: 0,
    handle_count: 1,
    move_mask: 1,
    arg0: 0,
    handles: [0; SPAWN_MAX_HANDLES],
    rights: [u64::MAX; SPAWN_MAX_HANDLES],
    namespace: 0,
    syscaps: 0,
};

/// The real host: syscalls, pipes, spawns.
struct NitroxHost {
    namespace: u64,
    notif: u64,
}

impl NitroxHost {
    fn resolve_program(&mut self, name: &str) -> Result<u64, String> {
        if name.starts_with('/') || name.starts_with("./") || name.starts_with("../") {
            return lookup(self.namespace, name.as_bytes())
                .ok_or_else(|| alloc::format!("`{name}` does not resolve"));
        }
        for dir in PROGRAM_DIRS {
            let mut path = Vec::from(*dir);
            path.extend_from_slice(name.as_bytes());
            if let Some(h) = lookup(self.namespace, &path) {
                return Ok(h);
            }
        }
        Err(alloc::format!("`{name}` is not a program"))
    }
}

impl Host for NitroxHost {
    fn run(
        &mut self,
        stages: &[StageSpec],
        input: Option<&[u8]>,
        strict: bool,
    ) -> Result<PipelineRun, String> {
        if stages.is_empty() {
            return Ok(PipelineRun::default());
        }
        // Resolve every image first: a chain that cannot run at all must not leave half
        // its stages spawned.
        let mut images = Vec::with_capacity(stages.len());
        for s in stages {
            images.push(self.resolve_program(&s.program)?);
        }

        let mut upstream: Option<u64> = None;
        let mut feed: Option<u64> = None;
        if input.is_some() {
            let (rx, tx) = pipe(4).map_err(|_| String::from("could not create an input pipe"))?;
            upstream = Some(rx);
            feed = Some(tx);
        }

        let mut spawned = 0usize;
        let mut capture: Option<u64> = None;

        for (i, spec) in stages.iter().enumerate() {
            let last = i + 1 == stages.len();
            // Depth 4: enough to keep a producer moving, small enough that backpressure
            // is real rather than hidden behind a large buffer.
            let (rx, tx) = pipe(4).map_err(|_| String::from("could not create a pipe"))?;
            let (setup_shell, setup_stage) =
                pipe(4).map_err(|_| String::from("could not create a setup channel"))?;

            // SAFETY: `SPAWN` is our static, filled in immediately before use; this
            // process is single-threaded so spawns are sequential.
            let proc = unsafe {
                SPAWN.image = images[i];
                SPAWN.handles[0] = setup_stage;
                SPAWN.arg0 = bootstrap_arg0(true);
                SPAWN.namespace = 0; // inherit a LOOKUP-only view of ours
                syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN) as u64)
            };
            if proc < 0 {
                return Err(alloc::format!("could not spawn `{}`", spec.program));
            }
            spawned += 1;

            let streams = Streams {
                stdin: upstream.take(),
                stdout: Some(tx),
                // §1: diagnostics bypass the pipe entirely. A stage's `stderr` is the
                // shell's own — the console — rather than anything threaded through the
                // pipeline's value.
                stderr: None,
            };
            let argv: Vec<&str> = spec.argv.iter().map(|s| s.as_str()).collect();
            send_setup(setup_shell, &streams, &argv)
                .map_err(|_| alloc::format!("could not hand `{}` its streams", spec.program))?;
            // SAFETY: closing our ends; the stage holds its own.
            unsafe {
                syscall1(SYS_HANDLE_CLOSE, setup_shell);
                syscall1(SYS_HANDLE_CLOSE, proc as u64);
            }

            if last {
                capture = Some(rx);
            } else {
                upstream = Some(rx);
            }
        }

        // Feed the head stage only once everything downstream is running.
        if let (Some(tx), Some(bytes)) = (feed, input) {
            let mut sender = ChannelSink::new(IpcPort::new(tx), libkern::abi::IPC_PAYLOAD_SIZE);
            let _ = sender.put(bytes);
            let _ = sender.finish();
            // SAFETY: closing our write end so the stage sees the stream end.
            unsafe { syscall1(SYS_HANDLE_CLOSE, tx) };
        }

        // Read the tail *before* reaping: a stage that has filled its channel is waiting
        // for this reader, so reaping first would deadlock any producer larger than one
        // pipe's worth.
        let mut output: Option<Vec<u8>> = None;
        if let Some(rx) = capture {
            if let Ok(bytes) = ChannelReceiver::new(IpcPort::new(rx)).receive() {
                if bytes.len() > MAX_CAPTURE {
                    return Err(String::from("a stage produced more output than nxsh will hold"));
                }
                if !bytes.is_empty() {
                    output = Some(bytes);
                }
            }
            // SAFETY: closing our read end.
            unsafe { syscall1(SYS_HANDLE_CLOSE, rx) };
        }

        Ok(PipelineRun { stages: reap(self.notif, stages, spawned, strict), output })
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
        let h = lookup(self.namespace, path.as_bytes())
            .ok_or_else(|| alloc::format!("cannot open {path}"))?;
        let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
        // SAFETY: a correctly sized `HandleInfo` out-param for a handle we own.
        let r = unsafe { syscall2(SYS_HANDLE_STAT, h, (&raw mut info) as u64) };
        if r != 0 || info.size == 0 {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
            return Err(alloc::format!("cannot size {path}"));
        }
        // SAFETY: mapping a readable file handle for exactly its own length.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, info.size, RIGHT_MAP_READ) };
        if addr < 0 {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
            return Err(alloc::format!("cannot read {path}"));
        }
        // SAFETY: `addr` is a live read-only mapping of `info.size` bytes.
        let bytes =
            unsafe { core::slice::from_raw_parts(addr as *const u8, info.size as usize) }.to_vec();
        // SAFETY: unmapping and closing what we just created.
        unsafe {
            syscall2(SYS_MEMORY_UNMAP, addr as u64, info.size);
            syscall1(SYS_HANDLE_CLOSE, h);
        }
        Ok(bytes)
    }

    /// Create `path` at exactly `bytes.len()` and write it (`save`, B5).
    ///
    /// `sys_file_create` sizes the file up front, so this is one create, one mapping and
    /// one copy — the Model A data path, with the kernel moving the bytes rather than the
    /// server.
    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let size = bytes.len() as u64;
        // SAFETY: valid namespace handle and path slice.
        let po = unsafe {
            syscall5(
                SYS_FILE_CREATE,
                self.namespace,
                path.as_ptr() as u64,
                path.len() as u64,
                RIGHT_MAP_READ | libkern::handle::RIGHT_MAP_WRITE,
                size,
            )
        };
        if po < 0 {
            return Err(alloc::format!("cannot create {path}"));
        }
        let (st, fh) = po_wait(po as u64);
        if st != 0 || fh == 0 {
            return Err(alloc::format!("cannot create {path}"));
        }
        // SAFETY: mapping our own writable file handle for exactly its length.
        let addr = unsafe {
            syscall4(
                SYS_MEMORY_MAP,
                fh,
                0,
                size,
                RIGHT_MAP_READ | libkern::handle::RIGHT_MAP_WRITE,
            )
        };
        if addr < 0 {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
            return Err(alloc::format!("cannot map {path}"));
        }
        // SAFETY: `addr` is a live writable mapping of exactly `size` bytes.
        unsafe {
            core::slice::from_raw_parts_mut(addr as *mut u8, bytes.len())
                .copy_from_slice(bytes);
        }
        // SAFETY: flush, unmap and close what we created.
        unsafe {
            syscall2(SYS_FILE_SYNC, fh, 0);
            syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
            syscall1(SYS_HANDLE_CLOSE, fh);
        }
        Ok(())
    }

    fn diag(&mut self, text: &str) {
        kprint(text.as_bytes());
    }

    fn out(&mut self, text: &str) {
        kprint(text.as_bytes());
    }
}

/// Collect one `ChildExited` per spawned stage.
///
/// **Per-stage attribution is exact only for a one-stage chain, and this is an ABI gap
/// rather than a shortcut.** `sys_process_spawn` returns a *handle*; `ChildExited`
/// carries a *ProcessId*; nothing maps one to the other, and `sys_wait` does not accept a
/// process handle, so the shell cannot ask "did *this* stage finish" — only "a child
/// finished, with this status".
///
/// So: a single stage is attributed exactly. A longer chain reports the aggregate
/// truthfully and marks each entry with the status of *some* stage rather than claiming
/// which — a report that is incomplete beats one that is confidently wrong. Filed as
/// `TODO(pipeline-stage-attribution)`; the fix is a pid on `HandleInfo` for a process
/// handle, or a handle in the notification, and it is an ABI change.
fn reap(notif: u64, stages: &[StageSpec], spawned: usize, strict: bool) -> Vec<StageStatus> {
    let mut seen: Vec<(i32, bool)> = Vec::with_capacity(spawned);
    while seen.len() < spawned {
        // SAFETY: valid single-waiter buffers.
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
        // SAFETY: `NOTIF` is a valid 64-byte out-param.
        let r = unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) };
        if r != 0 {
            continue; // WouldBlock — re-block
        }
        // SAFETY: the kernel wrote a Notification into `NOTIF`.
        let (kind, body) = unsafe {
            ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read())
        };
        if kind != KIND_CHILD_EXITED {
            continue;
        }
        let exit_kind = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
        let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
        seen.push((code, exit_kind != EXIT_KIND_NORMAL));
    }

    let mut out = Vec::with_capacity(stages.len());
    let mut aborted = false;
    for (i, s) in stages.iter().enumerate() {
        if aborted {
            out.push(StageStatus {
                command: s.program.clone(),
                exit_status: 0,
                crashed: false,
                cancelled: true,
            });
            continue;
        }
        let (code, crashed) = seen.get(i).copied().unwrap_or((0, false));
        if strict && (code != 0 || crashed) {
            aborted = true;
        }
        out.push(StageStatus { command: s.program.clone(), exit_status: code, crashed, cancelled: false });
    }
    out
}

/// Resolve `path` to a readable handle.
fn lookup(ns: u64, path: &[u8]) -> Option<u64> {
    // SAFETY: valid namespace handle and path slice.
    let po = unsafe {
        syscall4(
            SYS_NS_LOOKUP,
            ns,
            path.as_ptr() as u64,
            path.len() as u64,
            RIGHT_MAP_READ | RIGHT_INSPECT,
        )
    };
    if po < 0 {
        return None;
    }
    let (status, handle) = po_wait(po as u64);
    if status != 0 || handle == 0 { None } else { Some(handle) }
}

fn po_wait(po: u64) -> (i32, u64) {
    // SAFETY: valid single-waiter buffers.
    let waited = unsafe {
        WAIT_HANDLES[0] = po;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    // SAFETY: closing the PO we own.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po) };
    if waited != 1 {
        return (-1, 0);
    }
    // SAFETY: written by the syscall above.
    unsafe {
        let r = (&raw const WAIT_RESULTS).read();
        let status = i32::from_le_bytes([r[8], r[9], r[10], r[11]]);
        let value = u64::from_le_bytes([r[16], r[17], r[18], r[19], r[20], r[21], r[22], r[23]]);
        (status, value)
    }
}

fn run(notif: u64, namespace: u64, argv: &[String]) -> i64 {
    let mut host = NitroxHost { namespace, notif };

    let source = match argv.get(1).map(|s| s.as_str()) {
        Some("-c") => match argv.get(2) {
            Some(src) => src.clone(),
            None => {
                host.diag("nxsh: -c needs a script\n");
                return EXIT_USAGE;
            }
        },
        Some("--help") | Some("-h") => {
            host.out("usage: nxsh [SCRIPT.nx | -c SOURCE]\n");
            return EXIT_OK;
        }
        Some(path) => match host.read_file(path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => {
                    host.diag("nxsh: script is not valid UTF-8\n");
                    return EXIT_PARSE_ERROR;
                }
            },
            Err(e) => {
                host.diag("nxsh: ");
                host.diag(&e);
                host.diag("\n");
                return EXIT_PARSE_ERROR;
            }
        },
        // Part F turns this into the interactive loop.
        None => {
            host.out("nxsh: no script given; the interactive loop arrives in Part F\n");
            return EXIT_USAGE;
        }
    };

    let script = match nxsh::parse_script(&source) {
        Ok(s) => s,
        Err(e) => {
            let mut msg = String::from("nxsh: parse error on line ");
            msg.push_str(&nxsh::value::render_i64(e.line as i64));
            msg.push_str(": ");
            msg.push_str(e.message);
            msg.push('\n');
            host.diag(&msg);
            return EXIT_PARSE_ERROR;
        }
    };

    // §11e: a script discards a bare top-level value. The REPL's opposite default lands
    // with the REPL in Part F — the difference belongs to the driver, not the language.
    let mut interp = Interp::with_host(alloc::boxed::Box::new(host), RunMode::Script);
    match interp.run(&script) {
        Ok(_) => EXIT_OK,
        Err(e) => {
            let mut msg = String::from("nxsh: ");
            msg.push_str(&e.message);
            msg.push('\n');
            interp.host_mut().diag(&msg);
            EXIT_SCRIPT_FAILED
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let boot = bootstrap(notif, ns, endpoint, arg0);
    let argv = match boot.setup() {
        Some(Ok(s)) => s.argv,
        _ => Vec::new(),
    };
    exit(run(notif, ns, &argv))
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"nxsh: panic\n");
    exit(EXIT_SCRIPT_FAILED)
}
