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
    SYS_FILE_CREATE, SYS_FILE_SYNC, SYS_HANDLE_CLOSE, SYS_HANDLE_STAT, SYS_IO_SUBMIT,
    SYS_MEMORY_CREATE, SYS_MEMORY_MAP,
    SYS_MEMORY_UNMAP, SYS_NOTIF_RECV, SYS_NS_LOOKUP, SYS_PROCESS_SPAWN, SYS_WAIT, syscall1,
    syscall2, syscall4, syscall5,
};
use libkern::{exit, kprint};
use libstream::channel::{ChannelReceiver, ChannelSink, IpcPort};
use libstream::wire::ByteSink;
use libstream::setup::{Streams, bootstrap, bootstrap_arg0, pipe, send_setup_env};
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
        env: &libstream::wire::Record,
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
            // The same environment to every stage, arguments passed through as written:
            // both sides then resolve a relative path identically, which is the property
            // this slice exists for.
            send_setup_env(setup_shell, &streams, &argv, env)
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

    fn exists(&mut self, path: &str) -> bool {
        // The root of a namespace exists by construction — it is what bindings hang off,
        // and `list /` enumerates it. Neither probe below can see that: nothing is bound
        // *at* `/`, so the lookup misses, and no single server owns it, so `Dir::open`
        // misses too. Without this, `cd /` and `cd ..` out of `/home` both reported that
        // the root does not exist.
        if path == "/" {
            return true;
        }
        match lookup(self.namespace, path.as_bytes()) {
            Some(h) => {
                // SAFETY: closing a handle just installed into our table.
                unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
                true
            }
            // A *directory* does not resolve to a mappable object, so a failed lookup is
            // not proof of absence — a directory session is what answers for one.
            None => {
                let mut buf = [0u8; 4096];
                match librsproto::session::Dir::open(self.namespace, path.as_bytes(), &mut buf) {
                    Ok(d) => {
                        d.close();
                        true
                    }
                    Err(_) => false,
                }
            }
        }
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

/// Resolve `path` to a handle with **file** rights — mapping and stat.
fn lookup(ns: u64, path: &[u8]) -> Option<u64> {
    lookup_rights(ns, path, RIGHT_MAP_READ | RIGHT_INSPECT)
}

/// Resolve `path` asking for `rights`.
///
/// A **char device is not a file.** `/dev/console` is read with `sys_io_submit`, which
/// needs `READ` — not the `MAP_READ | INSPECT` a mappable object wants. Asking for the
/// wrong ones yields a handle that resolves perfectly well and then fails on every read,
/// which is exactly the bug this call site had: the REPL got a console it could not read
/// from, and the read loop swallowed the failure into a busy wait.
fn lookup_rights(ns: u64, path: &[u8], rights: u64) -> Option<u64> {
    // SAFETY: valid namespace handle and path slice.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights)
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

fn run(notif: u64, namespace: u64, argv: &[String], env: libstream::wire::Record) -> i64 {
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
        // A script path is the shell's own lookup, so it resolves against `PWD`.
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
        // No script: the interactive loop (§11).
        None => return repl(notif, namespace, env),
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
    interp.set_env(env);
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


/// The interactive loop (§11) — the *minimal* one.
///
/// Read, parse, evaluate, print. Continuation only where the parser can prove a statement
/// incomplete (§11b, decided in `nxsh::repl`). No reverse-search, no Shift-Enter, no
/// completion, no job control: those are §11's rich REPL, gated on the console/tty server
/// and the compositor terminal, and building them against a raw character device would be
/// a dependency inversion.
fn repl(notif: u64, namespace: u64, env: libstream::wire::Record) -> i64 {
    // `RIGHT_READ`, not the file rights `lookup` asks for: the console is a char device
    // read through `sys_io_submit`.
    let Some(console) = lookup_rights(namespace, b"/dev/console", libkern::handle::RIGHT_READ)
    else {
        kprint(b"nxsh: /dev/console not found\r\n");
        return EXIT_USAGE;
    };
    // A one-page read buffer the kernel writes input into.
    // SAFETY: register-only syscall.
    let buf_h = unsafe { syscall4(SYS_MEMORY_CREATE, 4096, 0, 0, 0) };
    if buf_h < 0 {
        kprint(b"nxsh: read buffer alloc failed\r\n");
        return EXIT_USAGE;
    }
    // SAFETY: a fresh MemoryObject with full MAP rights.
    let buf_addr = unsafe {
        syscall4(
            SYS_MEMORY_MAP,
            buf_h as u64,
            0,
            4096,
            RIGHT_MAP_READ | libkern::handle::RIGHT_MAP_WRITE,
        )
    };
    if buf_addr < 0 {
        kprint(b"nxsh: read buffer map failed\r\n");
        return EXIT_USAGE;
    }

    let mut interp = Interp::with_host(
        alloc::boxed::Box::new(NitroxHost { namespace, notif }),
        RunMode::Repl,
    );
    interp.set_env(env);
    kprint(b"\r\nnxsh: interactive shell (Ctrl-D or `exit` to leave)\r\n");

    // `pending` accumulates across continuation lines; `line` is the one being typed.
    let mut pending = String::new();
    let mut line: Vec<u8> = Vec::new();
    kprint(nxsh::repl::prompt(interp.cwd().unwrap_or("/")).as_bytes());

    // A read that keeps failing must **report**, not spin. The original loop did
    // `if po < 0 { continue }`, which turned a wrong-rights handle into a silent busy
    // loop: a prompt, no input, and a pegged CPU with nothing on the console to say why.
    // A failure that hangs is worse than one that exits.
    let mut consecutive_failures = 0u32;
    const MAX_READ_FAILURES: u32 = 16;

    loop {
        let op = libkern::abi::IoOp {
            opcode: libkern::abi::IO_OPCODE_READ,
            flags: 0,
            buffer: buf_h as u64,
            buf_offset: 0,
            offset: 0,
            length: 64,
        };
        // SAFETY: `console` is a char DeviceNode with READ; `&op` is a valid IoOp.
        let po = unsafe {
            syscall2(SYS_IO_SUBMIT, console, (&op as *const libkern::abi::IoOp) as u64)
        };
        if po < 0 {
            consecutive_failures += 1;
            if consecutive_failures >= MAX_READ_FAILURES {
                kprint(b"\r\nnxsh: cannot read the console (io_submit refused)\r\n");
                return EXIT_USAGE;
            }
            continue;
        }
        let (status, n) = po_wait(po as u64);
        if status != 0 {
            consecutive_failures += 1;
            if consecutive_failures >= MAX_READ_FAILURES {
                kprint(b"\r\nnxsh: console read failed repeatedly\r\n");
                return EXIT_USAGE;
            }
            continue;
        }
        consecutive_failures = 0;
        for i in 0..(n as usize).min(64) {
            // SAFETY: within the mapped one-page read buffer.
            let b = unsafe { ((buf_addr as u64 + i as u64) as *const u8).read_volatile() };
            match b {
                // Ctrl-D at an empty prompt is `exit` (§11f, universal convention).
                0x04 if line.is_empty() && pending.is_empty() => {
                    kprint(b"\r\n");
                    return EXIT_OK;
                }
                b'\r' | b'\n' => {
                    kprint(b"\r\n");
                    let typed = String::from_utf8_lossy(&line).into_owned();
                    line.clear();
                    pending.push_str(&typed);

                    if matches!(
                        nxsh::needs_continuation(&pending),
                        nxsh::Continue::Unclosed | nxsh::Continue::TrailingPipe
                    ) {
                        pending.push('\n');
                        kprint(nxsh::repl::continuation_prompt().as_bytes());
                        continue;
                    }

                    let src = core::mem::take(&mut pending);
                    if src.trim().is_empty() {
                        kprint(nxsh::repl::prompt(interp.cwd().unwrap_or("/")).as_bytes());
                        continue;
                    }
                    // `exit` is a shell-state builtin (§3): it must change *this* process,
                    // which an external program structurally cannot do — and it must end
                    // *this loop*, which is the one thing `run_line` cannot do for it.
                    //
                    // **It is the only line this loop may intercept.** A `cd` guard sat
                    // here too, left from before `cd` existed, and went on refusing a
                    // builtin the interpreter had implemented — the script path called
                    // `run_line` and worked, the interactive path never reached it. Every
                    // other line goes to `run_line` unread.
                    if src.trim() == "exit" {
                        return EXIT_OK;
                    }
                    match interp.run_line(&src) {
                        Ok(Some(text)) => kprint_crlf(&text),
                        Ok(None) => {}
                        Err(e) => {
                            let mut msg = String::from("nxsh: ");
                            msg.push_str(&e.message);
                            msg.push('\n');
                            kprint_crlf(&msg);
                        }
                    }
                    kprint(nxsh::repl::prompt(interp.cwd().unwrap_or("/")).as_bytes());
                }
                0x08 | 0x7f => {
                    if line.pop().is_some() {
                        kprint(b"\x08 \x08");
                    }
                }
                0x20..=0x7e => {
                    line.push(b);
                    // SAFETY: a single printable byte.
                    kprint(unsafe { core::slice::from_raw_parts(&b, 1) });
                }
                _ => {}
            }
        }
    }
}

/// Write `text` with `\n` expanded to `\r\n` — a raw console needs both.
fn kprint_crlf(text: &str) {
    for chunk in text.split('\n') {
        if !chunk.is_empty() {
            kprint(chunk.as_bytes());
        }
        kprint(b"\r\n");
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let boot = bootstrap(notif, ns, endpoint, arg0);
    // The shell's own environment arrives the same way every stage's does — there is one
    // mechanism, not a special case for the shell.
    let (argv, env) = match boot.setup() {
        Some(Ok(s)) => (s.argv, s.env),
        _ => (Vec::new(), libstream::wire::Record::default()),
    };
    exit(run(notif, ns, &argv, env))
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"nxsh: panic\n");
    exit(EXIT_SCRIPT_FAILED)
}
