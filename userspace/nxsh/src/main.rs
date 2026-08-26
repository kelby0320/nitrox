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

use libkern::abi::{HandleInfo, SENDMODE_NOBLOCK, SPAWN_MAX_HANDLES, SpawnArgs};
use libkern::handle::{RIGHT_INSPECT, RIGHT_MAP_READ};
use libkern::syscall::{
    SYS_FILE_CREATE, SYS_FILE_SYNC, SYS_HANDLE_CLOSE, SYS_HANDLE_STAT, SYS_MEMORY_MAP,
    SYS_CHANNEL_RECV, SYS_CHANNEL_SEND, SYS_MEMORY_UNMAP, SYS_NOTIF_RECV, SYS_NS_ENUMERATE,
    SYS_NS_LOOKUP, SYS_PROCESS_SPAWN, SYS_PROCESS_TERMINATE, SYS_WAIT, syscall1, syscall2,
    syscall3, syscall4, syscall5,
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
///
/// **`/initramfs/sbin` was the second entry and is gone (2026-08-11).** It was already
/// unreachable — a session namespace holds no `/initramfs` binding (see
/// `userspace/session-mgr/src/main.rs`), which is deliberate: a profile is a *choice* about
/// what a user may run, and the boot image is an accident of bootstrapping. It has now also
/// stopped being plausible: the boot image carries four programs, none of which anyone would
/// type at a prompt.
const PROGRAM_DIRS: &[&[u8]] = &[b"/bin/"];

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
/// Two waiters: the notification channel, and the terminal — the pipeline wait watches
/// both, so `Ctrl-C` is noticed while a *stage* is what is taking the time (§11h).
static mut WAIT_HANDLES: [u64; 2] = [0; 2];
static mut WAIT_RESULTS: [u8; 48] = [0; 48];
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
    /// The terminal, for §11h's interrupt checkpoint. `0` when there is none — a script,
    /// or a Tier-0 stage — in which case the question is never asked of anyone.
    tty: u64,
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
        // Handles to the stages, held until they are reaped — the authority `strict` and
        // §11h's interrupt both need. Closed in `reap`, which is where they stop mattering.
        let mut children: Vec<u64> = Vec::new();
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
            // **Keep the process handle for the life of the pipeline.** It used to be
            // closed here, which is why `strict` could only *relabel* the stages after a
            // failure: §1 says the shell "already holds process handles for everything it
            // spawned, so abort the rest is an ordinary capability-mediated call on
            // handles it already owns" — and it did not hold them. The missing piece was
            // never only the syscall (§11h); it was the authority to use it.
            children.push(proc as u64);
            // SAFETY: closing our end of the setup channel; the stage holds its own.
            unsafe {
                syscall1(SYS_HANDLE_CLOSE, setup_shell);
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
            // **The terminal is watched here, not only in `reap`** (§11h). This is where
            // the shell actually spends a long pipeline: the tail's output does not close
            // until the last stage exits, so `sleep 60 | …` blocks here for a minute and
            // never reaches the reap loop. Waking on the tty, asking the stages to stop,
            // and then reading is what makes `Ctrl-C` reach a *stage* at all.
            if self.tty != 0 {
                loop {
                    // **Poll before blocking.** A channel signals its waiters at the moment
                    // a message is enqueued, so a waiter that arrives *afterwards* never
                    // sees that edge — and the interrupt is enqueued before this wait
                    // begins, because the tty sees `Ctrl-C` in the same console read as the
                    // line that started the pipeline. Waiting first meant sleeping on a
                    // message that was already sitting in the queue.
                    if drain_tty_interrupt(self.tty) {
                        for &c in &children {
                            // SAFETY: a Process handle this shell owns, SIGNAL from spawn.
                            unsafe { syscall1(SYS_PROCESS_TERMINATE, c) };
                        }
                        continue;
                    }
                    // SAFETY: valid waiter buffers; two handles.
                    let waited = unsafe {
                        WAIT_HANDLES[0] = rx;
                        WAIT_HANDLES[1] = self.tty;
                        syscall4(
                            SYS_WAIT,
                            (&raw const WAIT_HANDLES) as u64,
                            2,
                            (&raw mut WAIT_RESULTS) as u64,
                            u64::MAX,
                        )
                    };
                    if waited < 1 || !drain_tty_interrupt(self.tty) {
                        break; // the tail has something to say, or the wait failed
                    }
                    for &c in &children {
                        // SAFETY: a Process handle this shell owns, with SIGNAL from spawn.
                        unsafe { syscall1(SYS_PROCESS_TERMINATE, c) };
                    }
                }
            }
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

        Ok(PipelineRun {
            stages: reap(self.notif, self.tty, &children, stages, spawned, strict),
            output,
        })
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
        // A namespace path can be real in three different ways, and `cd` has to accept
        // all three or it refuses places you can plainly see.
        //
        // 1. It names a binding, or sits above one. `/` and `/bin` are both like this:
        //    nothing resolves *at* them and no single server owns them, so neither probe
        //    below can see them — yet `list` shows them, because `list` asks the namespace
        //    what is bound. Asking the same question is what keeps `cd` and `list`
        //    agreeing about which places exist.
        if binding_at_or_under(self.namespace, path) {
            return true;
        }
        // 2. It resolves to an object — an ordinary file.
        match lookup(self.namespace, path.as_bytes()) {
            Some(h) => {
                // SAFETY: closing a handle just installed into our table.
                unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
                true
            }
            // 3. A *directory* does not resolve to a mappable object, so a failed lookup
            //    is not proof of absence — a directory session is what answers for one.
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

    /// §11h. A **non-blocking** receive: this runs at every statement boundary, so it
    /// must never park. Nothing else can be in flight at a checkpoint — evaluation is
    /// sequential and each tty exchange completes before the next statement — so anything
    /// waiting here is the unsolicited interrupt.
    fn interrupted(&mut self) -> bool {
        if self.tty == 0 {
            return false;
        }
        // SAFETY: single-threaded shell; valid recv out-params.
        unsafe {
            loop {
                let rr = syscall4(
                    SYS_CHANNEL_RECV,
                    self.tty,
                    (&raw mut TTY_MSG) as u64,
                    (&raw mut TTY_HANDLES) as u64,
                    (&raw mut TTY_COUNT) as u64,
                );
                if rr != 0 {
                    break; // WouldBlock: nothing queued, which is the common case
                }
                let payload_len =
                    u32::from_le_bytes([TTY_MSG[4], TTY_MSG[5], TTY_MSG[6], TTY_MSG[7]]) as usize;
                let rep = core::slice::from_raw_parts(
                    ((&raw const TTY_MSG) as *const u8).add(24),
                    payload_len.min(4096 - 24),
                );
                if let Ok(m) = librsproto::decode(rep) {
                    if m.op == librsproto::OP_TTY_INTERRUPT {
                        INTERRUPTED = true;
                    }
                }
            }
            // **Consumed, not sticky** — see the trait: a `catch` handling the interrupt
            // reaches this checkpoint too, and a host that kept answering "yes" would
            // interrupt the recovery as fast as it started.
            let hit = (&raw const INTERRUPTED).read();
            (&raw mut INTERRUPTED).write(false);
            hit
        }
    }

    /// **Shell output is ambient.** `kprint` is `SYS_DEBUG_KPRINT`, which takes no handle,
    /// so nothing can redirect, pipe, capture or log this — there is no object to redirect.
    /// The *input* half of that hole closed when `tty-server` shipped (2026-08-13); this is
    /// the half still owed, and this line is the whole of it.
    /// TODO(tty-server): `docs/rationale/deferred-decisions.md`.
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
fn reap(
    notif: u64,
    tty: u64,
    children: &[u64],
    stages: &[StageSpec],
    spawned: usize,
    strict: bool,
) -> Vec<StageStatus> {
    let mut seen: Vec<(i32, bool)> = Vec::with_capacity(spawned);
    let mut asked = false;
    while seen.len() < spawned {
        // **Wait on the terminal as well as the children.** The shell is blocked here for
        // as long as the pipeline runs, so this is the only place `Ctrl-C` can be noticed
        // while a *stage* is what is taking the time (§11h) — the evaluator's checkpoint
        // does not run until this returns.
        // SAFETY: valid waiter buffers; two handles when there is a terminal.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            let n = if tty != 0 {
                WAIT_HANDLES[1] = tty;
                2
            } else {
                1
            };
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                n,
                (&raw mut WAIT_RESULTS) as u64,
                u64::MAX,
            )
        };
        if waited < 1 {
            continue;
        }
        // Poll before blocking, for the same reason as the capture wait above.
        if tty != 0 && !asked && drain_tty_interrupt(tty) {
            asked = true;
            for &c in children {
                // SAFETY: a Process handle this shell owns, with SIGNAL from spawn.
                unsafe { syscall1(SYS_PROCESS_TERMINATE, c) };
            }
        }
        // An interrupt from the terminal: ask every stage to stop, once. Each is a
        // *request* — a stage that listens exits, one that does not keeps running and is
        // still waited for, which is the contract `sys_process_terminate` states.
        if tty != 0 && !asked && drain_tty_interrupt(tty) {
            asked = true;
            for &c in children {
                // SAFETY: a Process handle this shell owns, with SIGNAL from spawn.
                unsafe { syscall1(SYS_PROCESS_TERMINATE, c) };
            }
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
            // §1's eager abort, now an actual request rather than a label. The stages
            // after this one are *asked* to stop; whether they do is theirs to decide.
            for &c in children {
                // SAFETY: a Process handle this shell owns, with SIGNAL from spawn.
                unsafe { syscall1(SYS_PROCESS_TERMINATE, c) };
            }
        }
        out.push(StageStatus { command: s.program.clone(), exit_status: code, crashed, cancelled: false });
    }
    // SAFETY: the shell's own handles, no longer needed now every stage has exited.
    for &c in children {
        unsafe { syscall1(SYS_HANDLE_CLOSE, c) };
    }
    out
}

/// Take a pending `Ctrl-C` from the terminal without blocking, for the pipeline wait.
///
/// Sets the same flag the evaluator's checkpoint reads, so an interrupt that arrives while
/// a stage is running is not lost when the pipeline finishes: it stops the stages *and*
/// unwinds the evaluation.
fn drain_tty_interrupt(tty: u64) -> bool {
    let mut hit = false;
    // SAFETY: single-threaded shell; valid recv out-params.
    unsafe {
        loop {
            let rr = syscall4(
                SYS_CHANNEL_RECV,
                tty,
                (&raw mut TTY_MSG) as u64,
                (&raw mut TTY_HANDLES) as u64,
                (&raw mut TTY_COUNT) as u64,
            );
            if rr != 0 {
                break;
            }
            let payload_len =
                u32::from_le_bytes([TTY_MSG[4], TTY_MSG[5], TTY_MSG[6], TTY_MSG[7]]) as usize;
            let rep = core::slice::from_raw_parts(
                ((&raw const TTY_MSG) as *const u8).add(24),
                payload_len.min(4096 - 24),
            );
            if let Ok(m) = librsproto::decode(rep) {
                if m.op == librsproto::OP_TTY_INTERRUPT {
                    INTERRUPTED = true;
                    hit = true;
                }
            }
        }
    }
    hit
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

fn run(
    notif: u64,
    namespace: u64,
    argv: &[String],
    env: libstream::wire::Record,
    terminal: Option<u64>,
) -> i64 {
    // Script mode has no terminal: §11h's checkpoint asks nobody.
    let mut host = NitroxHost { namespace, notif, tty: 0 };

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
        None => return repl(notif, namespace, env, terminal),
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
        // §11f: `exit N` sets the *script's* status. It arrives on the error channel
        // because that is the only path crossing every boundary — a `def`, a closure, a
        // loop — but it is not a failure and must not be reported as one.
        Err(e) if e.is_exit() => e.exit.unwrap_or(0) as i64,
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
fn repl(
    notif: u64,
    namespace: u64,
    env: libstream::wire::Record,
    terminal: Option<u64>,
) -> i64 {
    // The session's terminal. A **channel**, not the raw device: the line discipline —
    // backspace, kill, echo, end-of-input — is the tty server's, and so is the write path,
    // which is what makes this shell's output an object something could redirect rather
    // than bytes shoved at an ambient debug syscall.
    //
    // **Handed down if the parent gave one, resolved otherwise.** A terminal emulator hands
    // the shell it hosts the terminal whose backend it holds, because `/dev/tty` is a
    // namespace binding and cannot name a *particular* window — the same reason Unix
    // inherits fd 0/1/2 rather than looking them up. Resolving is still the ordinary path
    // and is what a serial login does. See `docs/architecture/graphical-session.md` §6.1 for what
    // makes the resolved path work inside a window too.
    let tty = match terminal {
        Some(t) => t,
        None => {
            let Some(t) = lookup_rights(
                namespace,
                b"/dev/tty",
                libkern::handle::RIGHT_SEND
                    | libkern::handle::RIGHT_RECV
                    | libkern::handle::RIGHT_WAIT,
            ) else {
                kprint(b"nxsh: /dev/tty not found\r\n");
                return EXIT_USAGE;
            };
            t
        }
    };

    let mut interp = Interp::with_host(
        alloc::boxed::Box::new(NitroxHost { namespace, notif, tty }),
        RunMode::Repl,
    );
    interp.set_env(env);

    // **The shell edits its own input.** It reads raw keystrokes and runs the line
    // discipline locally, which is what `bash` does when it clears `ICANON` — and for the
    // same reason: history, and later completion, are the shell's knowledge, and they share
    // one editing loop with backspace and kill. A terminal that returned finished lines
    // could not offer them.
    //
    // The discipline is *linked*, not reimplemented. Three copies of this logic were
    // deleted to build the tty server; this is the second deployment of the one that
    // survived, not a fourth copy.
    //
    // Server-side echo goes off because the shell now echoes: it holds the buffer, so only
    // it knows what the screen should show after a history recall.
    tty_set_echo(tty, false);
    let mut disc = tty_server::Discipline::new();

    tty_write(tty, b"\r\nnxsh: interactive shell (Ctrl-D or `exit` to leave)\r\n");

    // `pending` accumulates across continuation lines. Deciding whether what has been typed
    // is *complete input* stays here rather than in the tty: the discipline hands over
    // lines, and only the shell knows whether a line finishes a statement.
    let mut pending = String::new();
    let mut history = nxsh::history::History::new();
    // Reverse-search state, `None` when not searching.
    let mut search: Option<Search> = None;
    tty_write(tty, nxsh::repl::prompt(interp.cwd().unwrap_or("/")).as_bytes());

    let mut chunk = [0u8; 64];
    loop {
        let n = match tty_read(tty, &mut chunk) {
            Some(n) => n,
            // The terminal is gone (the session ended under us). Leaving is the only
            // sensible response; spinning on a dead channel would peg a CPU with nothing
            // to say why.
            None => {
                kprint(b"nxsh: terminal closed\r\n");
                return EXIT_OK;
            }
        };
        // **`Ctrl-C` at a prompt discards the line** (§11h). The read came back empty
        // because the byte became an event rather than input, so there is nothing to feed
        // the discipline — abandon what was typed, say so, and start again. The same event
        // mid-evaluation is the evaluator's checkpoint; what it *means* is the caller's
        // decision, which is why the tty only reports it.
        // SAFETY: single-threaded shell.
        if unsafe { (&raw const INTERRUPTED).read() } {
            // SAFETY: same.
            unsafe { (&raw mut INTERRUPTED).write(false) };
            search = None;
            pending.clear();
            disc.reset();
            tty_write(tty, b"^C\r\n");
            tty_write(tty, nxsh::repl::prompt(interp.cwd().unwrap_or("/")).as_bytes());
            continue;
        }
        for &b in &chunk[..n] {
            // While searching, the shell owns the keys: the discipline's line buffer is
            // empty and the display is the search prompt, not a line being edited.
            if let Some(st) = search.as_mut() {
                match search_key(st, b, &history) {
                    SearchStep::Redraw => {
                        let out = st.redraw();
                        tty_write(tty, &out);
                    }
                    SearchStep::Cancel => {
                        let mut out = st.erase();
                        out.extend_from_slice(&disc.replace_line(st.original.as_bytes()));
                        tty_write(tty, &out);
                        search = None;
                    }
                    SearchStep::Accept => {
                        let mut out = st.erase();
                        let chosen = st.matched.clone();
                        out.extend_from_slice(&disc.replace_line(chosen.as_bytes()));
                        out.extend_from_slice(b"\r\n");
                        tty_write(tty, &out);
                        search = None;
                        // Fall through to submission by feeding the terminator the user
                        // pressed, so an accepted search takes exactly the path a typed
                        // line takes — no second copy of "what happens on Enter".
                        if let tty_server::Step::Line { bytes, .. } = disc.feed(b'\n') {
                            let src = String::from_utf8_lossy(&bytes).into_owned();
                            pending.push_str(&src);
                            let line = core::mem::take(&mut pending);
                            history.push(&line);
                            if let Some(status) = run_and_render(&mut interp, tty, &line) {
                                return status;
                            }
                        }
                        tty_write(tty, nxsh::repl::prompt(interp.cwd().unwrap_or("/")).as_bytes());
                    }
                    SearchStep::None => {}
                }
                continue;
            }
            // Ctrl-R enters reverse-search. The line being typed is erased from the
            // display and stashed, so cancelling restores exactly what was there.
            if b == 0x12 {
                let original = String::from_utf8_lossy(disc.line()).into_owned();
                let cleared = disc.replace_line(b"");
                let mut st = Search::new(original);
                let mut out = cleared;
                out.extend_from_slice(&st.redraw());
                tty_write(tty, &out);
                search = Some(st);
                continue;
            }
            match disc.feed(b) {
                tty_server::Step::None => {}
                tty_server::Step::Echo(e) => tty_write(tty, &e),
                // Ctrl-D at an empty prompt, as the banner promises.
                tty_server::Step::Eof => return EXIT_OK,
                // History recall replaces the displayed line; the discipline says how to
                // redraw because it is the half that knows what is on screen.
                tty_server::Step::Key(k) => {
                    let recalled = match k {
                        tty_server::Key::Up => history.older(disc.line()),
                        tty_server::Key::Down => history.newer(),
                    };
                    if let Some(text) = recalled {
                        let redraw = disc.replace_line(text.as_bytes());
                        tty_write(tty, &redraw);
                    }
                }
                tty_server::Step::Line { bytes, echo } => {
                    tty_write(tty, &echo);
                    let typed = String::from_utf8_lossy(&bytes).into_owned();
                    pending.push_str(&typed);

                    if matches!(
                        nxsh::needs_continuation(&pending),
                        nxsh::Continue::Unclosed | nxsh::Continue::TrailingPipe
                    ) {
                        pending.push('\n');
                        tty_write(tty, nxsh::repl::continuation_prompt().as_bytes());
                        continue;
                    }

                    let src = core::mem::take(&mut pending);
                    if src.trim().is_empty() {
                        tty_write(tty, nxsh::repl::prompt(interp.cwd().unwrap_or("/")).as_bytes());
                        continue;
                    }
                    history.push(&src);
                    // **Every line goes to the interpreter unread.** See `run_and_render`.
                    if let Some(status) = run_and_render(&mut interp, tty, &src) {
                        return status;
                    }
                    tty_write(tty, nxsh::repl::prompt(interp.cwd().unwrap_or("/")).as_bytes());
                }
            }
        }
    }
}



/// Run one line, render what it produced, and report whether the shell was asked to leave.
///
/// **The interactive loop now intercepts nothing.** It used to compare the typed line
/// against the literal string `"exit"` — which is why `exit 1` missed the comparison,
/// reached the interpreter, and came back as "`exit` is handled by the shell's driver".
/// A special case in the driver is a second implementation of the language, and this one
/// had already produced a bug of exactly that shape (a `cd` guard that outlived the
/// builtin it was refusing). `exit` is a shell-state builtin (§3), so it is a control
/// outcome of evaluation, and this function is the only thing that has to know it.
fn run_and_render(interp: &mut Interp, tty: u64, src: &str) -> Option<i64> {
    match interp.run_line(src) {
        Ok(Some(text)) => {
            tty_write_crlf(tty, &text);
            None
        }
        Ok(None) => None,
        Err(e) if e.is_exit() => Some(e.exit.unwrap_or(0) as i64),
        Err(e) => {
            let mut msg = String::from("nxsh: ");
            msg.push_str(&e.message);
            msg.push('\n');
            tty_write_crlf(tty, &msg);
            None
        }
    }
}

/// Reverse-search state: the query, the current match, and what is on screen.
///
/// **The shell tracks its own display length here rather than reusing the discipline's.**
/// `replace_line` erases exactly as many characters as the *line buffer* holds, and during
/// a search what is displayed is `(reverse-i-search)'q': match` — longer than the line, and
/// not the line at all. Erasing the wrong count leaves debris on a dumb terminal.
struct Search {
    query: String,
    /// The entry currently matched, shown after the search prompt.
    matched: String,
    /// Index into the history of the current match, so `Ctrl-R` can continue past it.
    at: Option<usize>,
    /// What was being typed when the search started, restored on cancel.
    original: String,
    /// How many characters are currently displayed, so they can be erased exactly.
    shown: usize,
}

/// What the caller should do after a key during a search.
enum SearchStep {
    None,
    Redraw,
    /// Leave the search, restoring the original line.
    Cancel,
    /// Take the match and submit it.
    Accept,
}

impl Search {
    fn new(original: String) -> Search {
        Search { query: String::new(), matched: String::new(), at: None, original, shown: 0 }
    }

    /// Erase everything this search has drawn.
    fn erase(&mut self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.shown * 3);
        for _ in 0..self.shown {
            out.extend_from_slice(b"\x08 \x08");
        }
        self.shown = 0;
        out
    }

    /// Erase and redraw the search line, returning the bytes to write.
    ///
    /// **Not bash's `` (reverse-i-search)`q': `` .** A query is arbitrary user text, so it
    /// needs delimiting or `search: list /bin: whoami` is genuinely ambiguous about where
    /// the query ends — quotes do that, and the arrow separates what was asked from what
    /// came back. bash's mismatched `` `q' `` is an old GNU convention that reads like a
    /// typo now, and copying it character-for-character would be inheriting a quirk rather
    /// than a decision.
    ///
    /// A query with no match says so. Showing an empty match instead would look like a
    /// search still in progress.
    fn redraw(&mut self) -> Vec<u8> {
        let mut out = self.erase();
        let text = if self.query.is_empty() {
            // Nothing searched for yet — "no match" would be a lie about a search that has
            // not happened.
            alloc::format!("search '' -> ")
        } else if self.matched.is_empty() {
            alloc::format!("search '{}' -> (no match)", self.query)
        } else {
            alloc::format!("search '{}' -> {}", self.query, self.matched)
        };
        out.extend_from_slice(text.as_bytes());
        self.shown = text.chars().count();
        out
    }
}

/// Handle one key while searching.
fn search_key(st: &mut Search, b: u8, history: &nxsh::history::History) -> SearchStep {
    match b {
        // Ctrl-R again: the next *older* match, so repeated presses walk through all of
        // them rather than sticking on the newest.
        0x12 => {
            if let Some(i) = history.search_back(&st.query, st.at) {
                st.at = Some(i);
                st.matched = String::from(history.get(i).unwrap_or(""));
            }
            SearchStep::Redraw
        }
        b'\r' | b'\n' => SearchStep::Accept,
        // Ctrl-G and ESC both abandon the search — the two keys people reach for.
        0x07 | 0x1b => SearchStep::Cancel,
        0x08 | 0x7f => {
            st.query.pop();
            // Re-search from the newest: shortening the query can match something more
            // recent than the current position, and staying put would hide it.
            st.at = history.search_back(&st.query, None);
            st.matched = st.at.and_then(|i| history.get(i)).map(String::from).unwrap_or_default();
            SearchStep::Redraw
        }
        0x20..=0x7e => {
            st.query.push(b as char);
            st.at = history.search_back(&st.query, None);
            st.matched = st.at.and_then(|i| history.get(i)).map(String::from).unwrap_or_default();
            SearchStep::Redraw
        }
        _ => SearchStep::None,
    }
}

/// Send one tty request and wait for its reply; returns `(is_error, body_len)` with the
/// body copied into `out`, or `None` if the exchange failed outright.
fn tty_request(ch: u64, op: u16, body: &[u8], out: &mut [u8]) -> Option<(bool, usize)> {
    // SAFETY: TTY_MSG is a valid buffer; the rsproto message goes at the payload offset.
    let sent = unsafe {
        let rs_len = librsproto::encode(&mut TTY_MSG[24..], op, 1, 0, body, 0)?;
        TTY_MSG[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        TTY_MSG[8] = 0;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const TTY_MSG) as u64,
            (&raw const TTY_HANDLES) as u64,
            0,
            SENDMODE_NOBLOCK,
        ) == 0
    };
    if !sent {
        return None;
    }
    tty_await_reply(ch, out)
}

/// Wait for the reply to the request just sent, stepping over any interrupt event.
///
/// Split out so it can be re-entered: the interrupt is the one unsolicited message on this
/// channel (§11h), and the reply this exchange is owed is still coming behind it.
fn tty_await_reply(ch: u64, out: &mut [u8]) -> Option<(bool, usize)> {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one waiter.
    let waited = unsafe {
        WAIT_HANDLES[0] = ch;
        syscall4(
            SYS_WAIT,
            (&raw const WAIT_HANDLES) as u64,
            1,
            (&raw mut WAIT_RESULTS) as u64,
            u64::MAX,
        )
    };
    if waited < 1 {
        return None;
    }
    // SAFETY: valid recv out-params.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut TTY_MSG) as u64,
            (&raw mut TTY_HANDLES) as u64,
            (&raw mut TTY_COUNT) as u64,
        )
    };
    if rr != 0 {
        return None;
    }
    // SAFETY: bounded read-only slice over the reply.
    unsafe {
        let payload_len =
            u32::from_le_bytes([TTY_MSG[4], TTY_MSG[5], TTY_MSG[6], TTY_MSG[7]]) as usize;
        let rep = core::slice::from_raw_parts(
            ((&raw const TTY_MSG) as *const u8).add(24),
            payload_len.min(4096 - 24),
        );
        let m = librsproto::decode(rep).ok()?;
        // The interrupt is unsolicited, so it can land where a reply was expected. Record
        // it and keep waiting for the reply this request is owed — dropping the exchange
        // here would leave the real reply queued for whoever asked next.
        if m.op == librsproto::OP_TTY_INTERRUPT {
            INTERRUPTED = true;
            return tty_await_reply(ch, out);
        }
        let n = m.body.len().min(out.len());
        out[..n].copy_from_slice(&m.body[..n]);
        Some((m.is_error(), n))
    }
}

/// Take whatever keystrokes are available. `None` if the terminal is unusable.
fn tty_read(ch: u64, out: &mut [u8]) -> Option<usize> {
    match tty_request(ch, librsproto::OP_TTY_READ, &[], out) {
        Some((false, n)) => Some(n),
        _ => None,
    }
}

/// Turn the terminal's own echo on or off.
///
/// The shell holds the line buffer, so only the shell knows what the screen should show
/// after a history recall — server-side echo would fight it.
fn tty_set_echo(ch: u64, on: bool) {
    let flags = [if on { librsproto::TTY_MODE_ECHO } else { 0 }];
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_SET_MODE, &flags, &mut scratch);
}

/// Write to the terminal — through a handle this process holds.
fn tty_write(ch: u64, text: &[u8]) {
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_WRITE, text, &mut scratch);
}

/// Write with `\n` expanded to `\r\n`, which a raw terminal needs.
fn tty_write_crlf(ch: u64, text: &str) {
    for chunk in text.split('\n') {
        if !chunk.is_empty() {
            tty_write(ch, chunk.as_bytes());
        }
        tty_write(ch, b"\r\n");
    }
}

/// Set when the terminal has reported `Ctrl-C` (§11h).
///
/// The interrupt is the one message the tty server sends unsolicited, so it can arrive in
/// the middle of any request/reply exchange. Every path that receives from the tty channel
/// records it here rather than acting on it: what an interrupt *means* depends on what the
/// shell is doing — clear the line at a prompt, unwind an evaluation in progress — and only
/// the caller knows which.
static mut INTERRUPTED: bool = false;

static mut TTY_MSG: [u8; 4096] = [0; 4096];
static mut TTY_HANDLES: [u64; 8] = [0; 8];
static mut TTY_COUNT: usize = 0;

/// Is `path` a binding in `ns`, or an ancestor of one?
///
/// The namespace walk `list` uses (`SYS_NS_ENUMERATE`), asked as a yes/no question. Local
/// and cheap — the kernel walks the caller's own namespace, no IPC — over a handful of
/// bindings.
///
/// `/` is true by construction: it is what bindings hang off, and a shell can always be at
/// the root of its own namespace even if nothing is bound in it yet.
fn binding_at_or_under(ns: u64, path: &str) -> bool {
    if path == "/" {
        return true;
    }
    let exact = path.trim_end_matches('/');
    // One trailing slash, so "is an ancestor of" is a prefix test that cannot match
    // `/binary` when the question was about `/bin`.
    let mut prefix = String::from(exact);
    prefix.push('/');

    let mut entry = libkern::abi::NsEntry::zeroed();
    for index in 0u64.. {
        // SAFETY: `entry` is a valid writable out-param of exactly `NsEntry`'s layout.
        let r = unsafe { syscall3(SYS_NS_ENUMERATE, ns, index, (&raw mut entry) as *mut _ as u64) };
        if r != 0 {
            return false; // NotFound ends the walk
        }
        let len = (entry.path_len as usize).min(libkern::abi::NS_ENTRY_PATH_MAX);
        // SAFETY: the kernel wrote the binding path; `len` is clamped to the buffer.
        let bound =
            unsafe { core::slice::from_raw_parts((&raw const entry.path) as *const u8, len) };
        let Ok(bound) = core::str::from_utf8(bound) else {
            continue;
        };
        if bound == exact || bound.starts_with(&prefix) {
            return true;
        }
    }
    false
}


#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, ns: u64, endpoint: u64, arg0: u64) -> ! {
    let boot = bootstrap(notif, ns, endpoint, arg0);
    // The shell's own environment arrives the same way every stage's does — there is one
    // mechanism, not a special case for the shell.
    let (argv, env, terminal) = match boot.setup() {
        Some(Ok(s)) => (s.argv, s.env, s.terminal),
        _ => (Vec::new(), libstream::wire::Record::default(), None),
    };
    exit(run(notif, ns, &argv, env, terminal))
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"nxsh: panic\n");
    exit(EXIT_SCRIPT_FAILED)
}
