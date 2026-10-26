//! `nxsh` — the Nitrox shell, host side: the [`Host`] implementation behind the library. It
//! resolves a program image, creates pipes, spawns a process per stage, reaps exits, and reads the
//! last stage's stream.
//!
//! ```text
//! nxsh script.nx      # run a script
//! nxsh -c 'SOURCE'    # run one line
//! ```
//!
//! The pipes between stages are bounded IPC channels and the stages run concurrently (§1). Stages
//! are spawned before any is reaped: reaping one at a time would deadlock once a stage produced
//! more than one channel's worth. Only the last stage's output is read into memory, to become a
//! `Value`.
//!
//! Exit codes are distinct on purpose: a harness must tell a malformed script from one that ran and
//! reported failure.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;
use libkern::{Received, channel_recv_raw};

use libkern::abi::{CLOCK_MONOTONIC, HandleInfo, SENDMODE_NOBLOCK, SPAWN_MAX_HANDLES, SpawnArgs};
use libkern::handle::{RIGHT_DUPLICATE, RIGHT_INSPECT, RIGHT_MAP_READ, RIGHT_SEND, RIGHT_TRANSFER};
use libkern::syscall::{
    SYS_CHANNEL_SEND, SYS_CLOCK_READ, SYS_FILE_CREATE, SYS_FILE_SYNC, SYS_HANDLE_CLOSE, SYS_HANDLE_DUPLICATE,
    SYS_HANDLE_STAT, SYS_MEMORY_MAP, SYS_MEMORY_UNMAP, SYS_NOTIF_RECV, SYS_NS_ENUMERATE, SYS_NS_LOOKUP,
    SYS_PROCESS_SPAWN, SYS_PROCESS_TERMINATE, SYS_WAIT, syscall1, syscall2, syscall3, syscall4, syscall5,
};
use libkern::{exit, kprint};
use libstream::channel::{ChannelSink, IpcPort, MsgPort};
use libstream::setup::{Streams, bootstrap, bootstrap_arg0, pipe, send_setup_full};
use libstream::wire::ByteSink;
use nxsh::host::{Host, PipelineRun, StageSpec, StageStatus};
use nxsh::{Interp, RunMode};

#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The commit this shell was built from (`xtask` sets it), named in the banner; else `unknown`.
const BUILD_COMMIT: &str = match option_env!("NITROX_COMMIT") {
    Some(c) => c,
    None => "unknown",
};

const EXIT_OK: i64 = 0;
const EXIT_SCRIPT_FAILED: i64 = 1;
const EXIT_PARSE_ERROR: i64 = 2;
const EXIT_USAGE: i64 = 64;

/// Where an unqualified program name is looked up: a short fixed list, not an ambient search path.
/// A session sees the programs its namespace was built with; `/initramfs/sbin` is absent because no
/// session namespace binds `/initramfs`.
const PROGRAM_DIRS: &[&[u8]] = &[b"/bin/"];

/// Cap on a captured stage output, so a spawned program cannot run the shell out of memory.
const MAX_CAPTURE: usize = 1 << 20;

const KIND_CHILD_EXITED: u32 = 0x0200;
/// `TerminateRequested`: someone holding this shell's process handle (its session's supervisor, at
/// a shutdown) asks it to exit.
const KIND_TERMINATE_REQUESTED: u32 = libkern::abi::KIND_TERMINATE_REQUESTED;
/// `ExitStatus.kind` for a normal exit; anything else means the process died.
const EXIT_KIND_NORMAL: u32 = 0;

// `libkern`'s own type, not a local copy: the body sits at offset 4, and a local pad put it at 8,
// reporting every non-zero exit as a crash.
static mut NOTIF: libkern::abi::Notification = libkern::abi::Notification { kind: 0, body: [0; 60] };
/// Waiters: the pipeline's output, the terminal (so `Ctrl-C` reaches a slow stage, §11h), its
/// shared `stderr` (diagnostics appear while a stage works) and the notification channel (a
/// shutdown's terminate request).
static mut WAIT_HANDLES: [u64; 4] = [0; 4];
static mut WAIT_RESULTS: [u8; 96] = [0; 96];

/// Was `handle` among the `n` results `sys_wait` just wrote?
///
/// `IoResult.handle` is at offset 0 of each 24-byte record. Without reading it a caller cannot tell
/// a diagnostic arriving from the pipeline's output, and would block reading output that is not
/// there.
fn signalled(n: i64, handle: u64) -> bool {
    if n < 1 {
        return false;
    }
    // SAFETY: the syscall wrote `n` records, and `n` is bounded by the handle count we
    // passed, which is at most the array's length.
    unsafe {
        let r = &raw const WAIT_RESULTS as *const u8;
        (0..n as usize).any(|i| {
            let mut bits = [0u8; 8];
            core::ptr::copy_nonoverlapping(r.add(i * 24), bits.as_mut_ptr(), 8);
            u64::from_le_bytes(bits) == handle
        })
    }
}

static mut ERR_MSG: [u8; 4096] = [0; 4096];

/// Write every diagnostic waiting on `err` to the terminal, and do not block.
///
/// Each is one whole message: `stderr` is shared between stages, and a partial line would
/// interleave.
fn drain_diagnostics(err: u64, tty: u64) {
    if err == 0 {
        return;
    }
    // SAFETY: single-threaded shell; valid recv out-params.
    unsafe {
        loop {
            // Every spawned program holds a `SEND` duplicate of this endpoint, so a stage can send
            // handles: they close as `_sent` drops, or the shell's spawns would fail in the end.
            let Ok(_sent) = channel_recv_raw(err, &raw mut ERR_MSG) else {
                break; // WouldBlock (nothing waiting) or the last stage has closed its end
            };
            let len = u32::from_le_bytes([ERR_MSG[4], ERR_MSG[5], ERR_MSG[6], ERR_MSG[7]]) as usize;
            let text = core::slice::from_raw_parts(((&raw const ERR_MSG) as *const u8).add(24), len.min(4096 - 24));
            let (level, text) = libstream::diag::parse(text);
            let Ok(text) = core::str::from_utf8(text) else {
                continue; // a diagnostic is text; anything else is not ours to render
            };
            // Strip the newline: `tty_write_crlf` ends every line.
            let text = text.strip_suffix('\n').unwrap_or(text);
            if tty != 0 {
                // Drawn like the shell's own errors, a warning in its own colour, a notice plain
                // (see `repl::diagnostic`).
                let painted = match nxsh::style::for_level(level) {
                    Some(sgr) => nxsh::style::paint(true, sgr, text),
                    None => alloc::string::String::from(text),
                };
                tty_write_crlf(tty, &painted);
            } else {
                kprint(text.as_bytes());
                kprint(b"\n");
            }
        }
    }
}
static mut SPAWN: SpawnArgs = SpawnArgs {
    image: 0,
    handle_count: 1,
    move_mask: 1,
    arg0: 0,
    handles: [0; SPAWN_MAX_HANDLES],
    rights: [u64::MAX; SPAWN_MAX_HANDLES],
    namespace: 0,
    // Whatever this shell holds, passed on; the kernel intersects it with its own (child = parent &
    // asked): nothing in a session, `SYSTEM_CLOCK` in a view with the `clock` grant, so `with admin
    // nxsh` has a working `date --set`.
    syscaps: u64::MAX,
};

mod stage;

struct NitroxHost {
    namespace: u64,
    notif: u64,
    /// The terminal, for §11h's interrupt checkpoint. `0` when there is none (a script or a Tier-0
    /// stage).
    tty: u64,
}

impl NitroxHost {
    /// Say `text` where the person will see it: the terminal if there is one, else the kernel log,
    /// which reaches COM1 only.
    fn say(&mut self, text: &str) {
        if self.tty != 0 {
            tty_write_crlf(self.tty, text.strip_suffix('\n').unwrap_or(text));
        } else {
            kprint(text.as_bytes());
        }
    }

    fn resolve_program(&mut self, name: &str) -> Result<u64, String> {
        if nxsh::exec::is_path_head(name) {
            return lookup(self.namespace, name.as_bytes()).ok_or_else(|| alloc::format!("`{name}` does not resolve"));
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
        // Resolve every image first: a chain that cannot run must not leave half its stages
        // spawned.
        let mut images = stage::Images(Vec::with_capacity(stages.len()));
        let mut argvs = Vec::with_capacity(stages.len());
        for s in stages {
            match self.resolve_stage(s, env) {
                Ok((img, argv)) => {
                    images.0.push(img);
                    argvs.push(argv);
                }
                Err(e) => {
                    // Said on the debug console too: under `nxterm` errors go to a grid that
                    // renders only under `test-harness`, so an unfound command would fail silently.
                    libkern::debug::Line::new().s(b"nxsh: could not resolve `").s(s.program.as_bytes()).s(b"`").end();
                    return Err(e);
                }
            }
        }

        let mut upstream: Option<u64> = None;
        let mut feed: Option<u64> = None;
        if input.is_some() {
            let (rx, tx) = pipe(4).map_err(|_| String::from("could not create an input pipe"))?;
            upstream = Some(rx);
            feed = Some(tx);
        }

        let mut spawned = 0usize;
        // Held until reaped, closed in `reap`: the authority `strict` and §11h's interrupt need.
        let mut children: Vec<u64> = Vec::new();
        let mut capture: Option<u64> = None;

        // One `stderr` for the whole pipeline (§1's shared diagnostic sink): a stage's diagnostics
        // bypass the pipe, and the shell drains them to its terminal.
        //
        // The slack is 36 messages, and that is a bound: a stage sends with `SENDMODE_BLOCK`, so
        // once the ring and the kernel's pending-sender queue (`IpcChannel::MAX_PENDING_SENDS`, 4)
        // are full it blocks, at its 37th unread diagnostic. The shell must be draining by then; it
        // drains at every wait, and the one window left is named at the feed below.
        let (err_rx, err_tx) = pipe(32).map_err(|_| String::from("could not create a diagnostic channel"))?;

        for (i, spec) in stages.iter().enumerate() {
            let last = i + 1 == stages.len();
            let (rx, tx) = pipe(4).map_err(|_| String::from("could not create a pipe"))?;
            let (setup_shell, setup_stage) = pipe(4).map_err(|_| String::from("could not create a setup channel"))?;

            // SAFETY: `SPAWN` is our static, filled in immediately before use; this
            // process is single-threaded so spawns are sequential.
            let proc = unsafe {
                SPAWN.image = images.0[i];
                SPAWN.handles[0] = setup_stage;
                SPAWN.arg0 = bootstrap_arg0(true);
                SPAWN.namespace = 0; // inherit a LOOKUP-only view of ours
                syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN) as u64)
            };
            images.release(i);
            if proc < 0 {
                return Err(alloc::format!("could not spawn `{}`", spec.program));
            }
            spawned += 1;

            // A duplicate per stage, because a setup message moves its handles.
            //
            // With `DUPLICATE`, because a stage may hand its `stderr` on: `with` gives its program
            // a duplicate and keeps one to report on. Without the right the program starts with no
            // `stderr` and its lines go to the kernel log, which on a laptop is nowhere.
            // SAFETY: register-only syscall on a channel endpoint this shell owns.
            let err_dup =
                unsafe { syscall2(SYS_HANDLE_DUPLICATE, err_tx, RIGHT_SEND | RIGHT_TRANSFER | RIGHT_DUPLICATE) };
            if err_dup <= 0 {
                // Say so rather than degrade quietly: a stage without `stderr` falls back to
                // `kprint`, which reaches nobody on a laptop. The stage still runs; losing its
                // diagnostics is not worth refusing.
                self.say(&alloc::format!(
                    "nxsh: `{}` gets no diagnostic channel; its errors go to the kernel log\n",
                    spec.program
                ));
            }
            let streams = Streams {
                stdin: upstream.take(),
                stdout: Some(tx),
                // §1: diagnostics bypass the pipe; they go to the shell's shared sink.
                stderr: (err_dup > 0).then_some(err_dup as u64),
            };
            let argv: Vec<&str> = argvs[i].iter().map(|s| s.as_str()).collect();
            // A terminal of its own, a sibling on the same window, so a stage that must ask
            // something (`with`, for a password) asks where the person is typing. The shell keeps
            // its own and `Ctrl-C`; `/dev/tty` would mint one on the console.
            let terminal = if self.tty != 0 { tty_open_sibling(self.tty) } else { None };
            // The same environment to every stage, arguments as written.
            if send_setup_full(setup_shell, &streams, terminal, &argv, env).is_err() {
                if let Some(t) = terminal {
                    // SAFETY: the send failed, so the terminal never moved; it is ours to close.
                    unsafe { syscall1(SYS_HANDLE_CLOSE, t) };
                }
                return Err(alloc::format!("could not hand `{}` its streams", spec.program));
            }
            // Keep the process handle for the pipeline's life: `strict` and §11h need the authority
            // to terminate the rest.
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

        // Close the shell's own write end before anything else can block: the read end reports
        // end-of-stream only once every writer, the shell included, is gone.
        // SAFETY: closing our own handle; the stages hold their duplicates.
        unsafe { syscall1(SYS_HANDLE_CLOSE, err_tx) };

        // An interrupt that arrived during setup: handing a stage its terminal is a tty exchange,
        // and `tty_await_reply` records a `Ctrl-C` there rather than leaving it queued, so the
        // waits below would never see it and `sleep 60` would run its minute (`test-interactive`
        // step 19). The flag stays set for the evaluator's checkpoint.
        // SAFETY: INTERRUPTED is a process-wide flag read and written only from this thread
        if unsafe { (&raw const INTERRUPTED).read() } {
            for &c in &children {
                // SAFETY: a Process handle this shell owns, with SIGNAL from spawn.
                unsafe { syscall1(SYS_PROCESS_TERMINATE, c) };
            }
        }

        // Feed the head stage only once everything downstream is running.
        //
        // The one window where the shell is not draining: `put` blocks on a depth-4 pipe, so a head
        // stage that stops reading `stdin` stops the shell, and a stage blocked on its 37th unread
        // diagnostic is such a stage. Closing it needs an interleaved send in `ChannelSink`.
        if let (Some(tx), Some(bytes)) = (feed, input) {
            drain_diagnostics(err_rx, self.tty);
            let mut sender = ChannelSink::new(IpcPort::new(tx), libkern::abi::IPC_PAYLOAD_SIZE);
            let _ = sender.put(bytes);
            let _ = sender.finish();
            // SAFETY: closing our write end so the stage sees the stream end.
            unsafe { syscall1(SYS_HANDLE_CLOSE, tx) };
            drain_diagnostics(err_rx, self.tty);
        }

        // Read the tail *before* reaping: a stage that has filled its channel is waiting
        // for this reader, so reaping first would deadlock any producer larger than one
        // pipe's worth.
        let mut output: Option<Vec<u8>> = None;
        // Exits read off the notification channel for `reap`, and whether a terminate was passed
        // on.
        let mut early: Vec<(i32, bool)> = Vec::new();
        let mut stop_sent = false;
        if let Some(rx) = capture {
            // The terminal is watched here, not only in `reap` (§11h): the tail's output does not
            // close until the last stage exits, so `sleep 60 | …` blocks here. Diagnostics are
            // drained here too, with or without a terminal, or a long job's progress would arrive
            // as a wall of text at the end.
            //
            // One loop that waits, drains and reads, taking the tail's messages one at a time
            // rather than through `ChannelReceiver::receive`, which loops on `rx` alone: a stage
            // blocked on its 37th unread diagnostic would never send its `last` message.
            let mut port = IpcPort::new(rx);
            let mut bytes: Vec<u8> = Vec::new();
            let mut complete = false;
            let mut overflowed = false;
            loop {
                // The notification channel too: a terminate request must reach the stages during
                // this wait, and a stage's exit read here is kept for `reap`.
                drain_notifications(self.notif, &children, &mut stop_sent, &mut early);
                // Poll before blocking: a waiter arriving after a message was enqueued never sees
                // that edge, and the tty queues the interrupt in the same console read as the line
                // that started the pipeline.
                drain_diagnostics(err_rx, self.tty);
                if self.tty != 0 && drain_tty_interrupt(self.tty) {
                    for &c in &children {
                        // SAFETY: a Process handle this shell owns, SIGNAL from spawn.
                        unsafe { syscall1(SYS_PROCESS_TERMINATE, c) };
                    }
                    continue;
                }
                // SAFETY: valid waiter buffers; three or four handles.
                let waited = unsafe {
                    WAIT_HANDLES[0] = rx;
                    WAIT_HANDLES[1] = err_rx;
                    WAIT_HANDLES[2] = self.notif;
                    let n = if self.tty != 0 {
                        WAIT_HANDLES[3] = self.tty;
                        4
                    } else {
                        3
                    };
                    syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, n, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
                };
                if waited < 1 {
                    break; // the wait failed; there is nothing more to read
                }
                // Only `rx` means output; reading on any other wake would block.
                if signalled(waited, rx) {
                    match port.recv(&mut bytes) {
                        Ok(true) => {
                            complete = true;
                            break;
                        }
                        Ok(false) => {}
                        // The peer closed without finishing: a fragment of a typed stream is not
                        // for the evaluator.
                        Err(_) => break,
                    }
                    if bytes.len() > MAX_CAPTURE {
                        overflowed = true;
                        break;
                    }
                }
            }
            drain_diagnostics(err_rx, self.tty);
            if overflowed {
                return Err(String::from("a stage produced more output than nxsh will hold"));
            }
            if complete && !bytes.is_empty() {
                output = Some(bytes);
            }
            // SAFETY: closing our read end.
            unsafe { syscall1(SYS_HANDLE_CLOSE, rx) };
        }

        let reaped = reap(self.notif, self.tty, err_rx, &children, stages, spawned, strict, early, stop_sent);
        // One last drain, after every stage is gone: a program's final word can be sent just before
        // `exit`, after the wait that noticed the exit returned.
        drain_diagnostics(err_rx, self.tty);
        // SAFETY: closing our read end; every writer is gone.
        unsafe { syscall1(SYS_HANDLE_CLOSE, err_rx) };

        Ok(PipelineRun { stages: reaped, output })
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
        let h = lookup(self.namespace, path.as_bytes()).ok_or_else(|| alloc::format!("cannot open `{path}`"))?;
        let mut info = HandleInfo { rights: 0, object_type: 0, generation: 0, size: 0 };
        // SAFETY: a correctly sized `HandleInfo` out-param for a handle we own.
        let r = unsafe { syscall2(SYS_HANDLE_STAT, h, (&raw mut info) as u64) };
        if r != 0 || info.size == 0 {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
            // An empty file has nothing to map, and is no error: it runs as an empty script. A
            // directory stats with no size too, and is not one.
            let file =
                matches!(info.object_type, libkern::handle::KOBJ_FILE_OBJECT | libkern::handle::KOBJ_MEMORY_OBJECT);
            return if r == 0 && file { Ok(Vec::new()) } else { Err(alloc::format!("cannot size `{path}`")) };
        }
        // SAFETY: mapping a readable file handle for exactly its own length.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, h, 0, info.size, RIGHT_MAP_READ) };
        if addr < 0 {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
            return Err(alloc::format!("cannot read `{path}`"));
        }
        // SAFETY: `addr` is a live read-only mapping of `info.size` bytes.
        let bytes = unsafe { core::slice::from_raw_parts(addr as *const u8, info.size as usize) }.to_vec();
        // SAFETY: unmapping and closing what we just created.
        unsafe {
            syscall2(SYS_MEMORY_UNMAP, addr as u64, info.size);
            syscall1(SYS_HANDLE_CLOSE, h);
        }
        Ok(bytes)
    }

    /// Create `path` at exactly `bytes.len()` and write it (`save`).
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
            return Err(alloc::format!("cannot create `{path}`"));
        }
        let (st, fh) = po_wait(po as u64);
        if st != 0 || fh == 0 {
            return Err(alloc::format!("cannot create `{path}`"));
        }
        // SAFETY: mapping our own writable file handle for exactly its length.
        let addr = unsafe { syscall4(SYS_MEMORY_MAP, fh, 0, size, RIGHT_MAP_READ | libkern::handle::RIGHT_MAP_WRITE) };
        if addr < 0 {
            // SAFETY: closing our own handle.
            unsafe { syscall1(SYS_HANDLE_CLOSE, fh) };
            return Err(alloc::format!("cannot map `{path}`"));
        }
        // SAFETY: `addr` is a live writable mapping of exactly `size` bytes.
        unsafe {
            core::slice::from_raw_parts_mut(addr as *mut u8, bytes.len()).copy_from_slice(bytes);
        }
        // SAFETY: unmap, flush and close what we created — unmapped before the flush, since
        // one that begins with a writable mapping in place leaves the file dirty.
        unsafe {
            syscall2(SYS_MEMORY_UNMAP, addr as u64, size);
            syscall2(SYS_FILE_SYNC, fh, 0);
            syscall1(SYS_HANDLE_CLOSE, fh);
        }
        Ok(())
    }

    fn exists(&mut self, path: &str) -> bool {
        // A path can be real in three ways, and `cd` must accept all three.
        // 1. It names a binding, or sits above one (`/`, `/bin`): nothing resolves at it, but
        // `list` shows it, so ask the namespace the same question `list` does.
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

    fn commands(&mut self) -> Vec<String> {
        // The list `resolve_program` searches, so Tab never offers a name the shell would fail to
        // run.
        let mut out = Vec::new();
        for dir in PROGRAM_DIRS {
            let path = dir.strip_suffix(b"/").unwrap_or(dir);
            if let Ok(entries) = libfs::list_dir(self.namespace, path) {
                for e in entries {
                    out.push(String::from_utf8_lossy(e.name()).into_owned());
                }
            }
        }
        out
    }

    fn list_dir(&mut self, dir: &str) -> Vec<(String, bool)> {
        // The filesystem-and-namespace union `list` shows; `cd` and Tab must ask the same question
        // (`nxsh/CLAUDE.md`).
        match libfs::list_dir(self.namespace, dir.as_bytes()) {
            Ok(entries) => entries
                .iter()
                .map(|e| (String::from_utf8_lossy(e.name()).into_owned(), e.kind == librsproto::file::DIRENT_KIND_DIR))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn diag(&mut self, text: &str) {
        // Not painted: every caller but one is script mode, whose host has `tty: 0` and plain-text
        // output. The exception is `time`'s line, plain on purpose since it is read off a laptop
        // screen and a serial console. It also arrives in fragments, so there is no whole run to
        // bracket; the REPL paints in `run_and_render`.
        self.say(text);
    }

    /// §11h. A non-blocking receive: this runs at every statement boundary and must never park.
    /// Anything waiting is the unsolicited interrupt.
    fn interrupted(&mut self) -> bool {
        // A terminate request, between statements: read off the notification channel here too, so a
        // loop that runs no stage still sees it. `reap` has counted every exit before a pipeline
        // returns.
        // SAFETY: single-threaded shell; set once at startup.
        let notif = unsafe { NOTIF_CH };
        if notif != 0 {
            drain_notifications(notif, &[], &mut false, &mut Vec::new());
        }
        // SAFETY: single-threaded shell.
        if unsafe { (&raw const STOP_ASKED).read() } {
            return true;
        }
        if self.tty == 0 {
            return false;
        }
        // SAFETY: single-threaded shell; valid recv out-params.
        unsafe {
            loop {
                let Ok(_sent) = channel_recv_raw(self.tty, &raw mut TTY_MSG) else {
                    break; // WouldBlock: nothing queued, which is the common case
                };
                let payload_len = u32::from_le_bytes([TTY_MSG[4], TTY_MSG[5], TTY_MSG[6], TTY_MSG[7]]) as usize;
                let rep = core::slice::from_raw_parts(
                    ((&raw const TTY_MSG) as *const u8).add(24),
                    payload_len.min(4096 - 24),
                );
                if let Ok(m) = librsproto::decode(rep)
                    && m.op == librsproto::OP_TTY_INTERRUPT
                {
                    INTERRUPTED = true;
                }
            }
            let hit = (&raw const INTERRUPTED).read();
            (&raw mut INTERRUPTED).write(false);
            hit || (&raw const STOP_ASKED).read()
        }
    }

    /// The kernel's monotonic clock, for `time`.
    fn now_ns(&mut self) -> Option<u64> {
        let mut ns = 0u64;
        // SAFETY: `ns` is a writable `u64`, which is what the kernel writes the clock's value into.
        let r = unsafe { syscall2(SYS_CLOCK_READ, CLOCK_MONOTONIC, (&raw mut ns) as u64) };
        (r == 0).then_some(ns)
    }

    /// `display`'s output goes to the terminal when there is one, else to `kprint`. A shell with no
    /// terminal still writes without a handle, so nothing can redirect it.
    /// TODO(tty-server): `docs/rationale/deferred-decisions.md`.
    fn out(&mut self, text: &str) {
        self.say(text);
    }

    fn styled(&mut self) -> bool {
        self.tty != 0
    }
}

/// Collect one `ChildExited` per spawned stage.
///
/// Per-stage attribution is exact only for a one-stage chain: `sys_process_spawn` returns a handle,
/// `ChildExited` carries a ProcessId, and `sys_wait` does not accept a process handle. A longer
/// chain reports the aggregate truthfully and marks each entry with the status of some stage, not
/// claiming which. Filed as `TODO(pipeline-stage-attribution)`; the fix is a pid on `HandleInfo`
/// for a process handle, or a handle in the notification, and it is an ABI change.
#[allow(clippy::too_many_arguments, reason = "a pipeline's channels, stages and what the capture wait already read")]
fn reap(
    notif: u64,
    tty: u64,
    err: u64,
    children: &[u64],
    stages: &[StageSpec],
    spawned: usize,
    strict: bool,
    // Exits the capture wait read before this ran, and whether it passed a terminate request on.
    early: Vec<(i32, bool)>,
    stop_sent: bool,
) -> Vec<StageStatus> {
    let mut seen: Vec<(i32, bool)> = early;
    let mut asked = stop_sent;
    while seen.len() < spawned {
        // Wait on the terminal as well as the children: this is the only place `Ctrl-C` is noticed
        // while a stage is slow (§11h). Diagnostics are drained here too: a pipeline with nothing
        // on its tail spends the whole job in this loop.
        drain_diagnostics(err, tty);
        // SAFETY: valid waiter buffers; two or three handles.
        let waited = unsafe {
            WAIT_HANDLES[0] = notif;
            WAIT_HANDLES[1] = err;
            let n = if tty != 0 {
                WAIT_HANDLES[2] = tty;
                3
            } else {
                2
            };
            syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, n, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
        };
        drain_diagnostics(err, tty);
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
        // Ask every stage to stop, once; a stage that ignores the request is still waited for.
        if tty != 0 && !asked && drain_tty_interrupt(tty) {
            asked = true;
            for &c in children {
                // SAFETY: a Process handle this shell owns, with SIGNAL from spawn.
                unsafe { syscall1(SYS_PROCESS_TERMINATE, c) };
            }
        }
        drain_notifications(notif, children, &mut asked, &mut seen);
    }

    let mut out = Vec::with_capacity(stages.len());
    let mut aborted = false;
    for (i, s) in stages.iter().enumerate() {
        if aborted {
            out.push(StageStatus { command: s.program.clone(), exit_status: 0, crashed: false, cancelled: true });
            continue;
        }
        let (code, crashed) = seen.get(i).copied().unwrap_or((0, false));
        if strict && (code != 0 || crashed) {
            aborted = true;
            // §1's eager abort: the later stages are asked to stop.
            for &c in children {
                // SAFETY: a Process handle this shell owns, with SIGNAL from spawn.
                unsafe { syscall1(SYS_PROCESS_TERMINATE, c) };
            }
        }
        out.push(StageStatus { command: s.program.clone(), exit_status: code, crashed, cancelled: false });
    }
    for &c in children {
        // SAFETY: the shell's own handles, no longer needed now every stage has exited
        unsafe { syscall1(SYS_HANDLE_CLOSE, c) };
    }
    out
}

/// Take a pending `Ctrl-C` from the terminal without blocking, setting the flag the evaluator's
/// checkpoint reads, so it also unwinds the evaluation.
fn drain_tty_interrupt(tty: u64) -> bool {
    let mut hit = false;
    // SAFETY: single-threaded shell; valid recv out-params.
    unsafe {
        loop {
            let Ok(_sent) = channel_recv_raw(tty, &raw mut TTY_MSG) else {
                break;
            };
            let payload_len = u32::from_le_bytes([TTY_MSG[4], TTY_MSG[5], TTY_MSG[6], TTY_MSG[7]]) as usize;
            let rep =
                core::slice::from_raw_parts(((&raw const TTY_MSG) as *const u8).add(24), payload_len.min(4096 - 24));
            if let Ok(m) = librsproto::decode(rep)
                && m.op == librsproto::OP_TTY_INTERRUPT
            {
                INTERRUPTED = true;
                hit = true;
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
/// A char device is not a file: `/dev/console` is read with `READ`, not `MAP_READ | INSPECT`. The
/// wrong rights give a handle that resolves and then fails every read.
fn lookup_rights(ns: u64, path: &[u8], rights: u64) -> Option<u64> {
    // SAFETY: valid namespace handle and path slice.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
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
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
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

fn run(notif: u64, namespace: u64, argv: &[String], env: libstream::wire::Record, terminal: Option<u64>) -> i64 {
    let mut host = NitroxHost { namespace, notif, tty: 0 };

    let mut script_path: Option<String> = None;
    let source = match argv.get(1).map(|s| s.as_str()) {
        Some("-c") => match argv.get(2) {
            Some(src) => src.clone(),
            None => {
                host.diag("nxsh: -c needs a script\n");
                return EXIT_USAGE;
            }
        },
        Some("--help") | Some("-h") => {
            // On the terminal it was handed: script mode keeps none for the host, and `kprint`
            // reaches nobody on a machine with no serial port.
            match terminal {
                Some(t) => tty_write_crlf(t, "usage: nxsh [SCRIPT [ARG...] | -c SOURCE [ARG...]]"),
                None => host.out("usage: nxsh [SCRIPT [ARG...] | -c SOURCE [ARG...]]\n"),
            }
            return EXIT_OK;
        }
        Some(path) => match nxsh::paths::resolve(nxsh::paths::pwd_of(&env), path).and_then(|abs| {
            let bytes = host.read_file(&abs)?;
            script_path = Some(abs);
            Ok(bytes)
        }) {
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
        None => return repl(notif, namespace, env, terminal),
    };

    let base = script_path.as_deref().map(nxsh::paths::dir_of);
    let script = match nxsh::parse_script_in(&source, base) {
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

    let mut interp = Interp::with_host(alloc::boxed::Box::new(host), RunMode::Script);
    interp.set_env(env);
    interp.set_args(argv.iter().skip(if argv.get(1).is_some_and(|a| a == "-c") { 3 } else { 2 }).cloned().collect());
    if let Some(abs) = &script_path {
        interp.mark_loading(abs);
    }
    match interp.run(&script) {
        Ok(_) => EXIT_OK,
        // §11f: `exit N` sets the script's status; it is not a failure.
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

/// The interactive loop (§11): read, edit, parse, evaluate, print. Continuation only where the
/// parser can prove a statement incomplete (§11b, `nxsh::repl`).
fn repl(notif: u64, namespace: u64, env: libstream::wire::Record, terminal: Option<u64>) -> i64 {
    // The session's terminal, a channel rather than the raw device. Handed down if the parent gave
    // one (a terminal emulator hands over the terminal whose backend it holds; `/dev/tty` cannot
    // name a particular window), resolved otherwise as a serial login does. See
    // `docs/architecture/graphical-session.md` §6.1.
    let tty = match terminal {
        Some(t) => t,
        None => {
            let Some(t) = lookup_rights(
                namespace,
                b"/dev/tty",
                libkern::handle::RIGHT_SEND | libkern::handle::RIGHT_RECV | libkern::handle::RIGHT_WAIT,
            ) else {
                kprint(b"nxsh: /dev/tty not found\r\n");
                return EXIT_USAGE;
            };
            t
        }
    };

    let mut interp = Interp::with_host(alloc::boxed::Box::new(NitroxHost { namespace, notif, tty }), RunMode::Repl);
    interp.set_env(env);

    // The shell edits its own input, as `bash` does with `ICANON` cleared: history and completion
    // are its knowledge and share one editing loop with backspace and kill. The discipline is
    // linked, not reimplemented. Server-side echo goes off because only the shell knows what the
    // screen should show after a recall.
    tty_set_echo(tty, false);
    let mut editor = nxsh::editor::Editor::new();

    tty_write(tty, b"\r\n");
    let banner = alloc::format!("nxsh: interactive shell, Nitrox {BUILD_COMMIT} (Ctrl-D or `exit` to leave)");
    tty_write(tty, nxsh::style::paint(tty != 0, nxsh::style::BANNER, &banner).as_bytes());
    tty_write(tty, b"\r\n");

    let mut history = nxsh::history::History::new();
    let prompt = |interp: &Interp| nxsh::repl::prompt(interp.cwd().unwrap_or("/"), tty != 0);
    tty_write(tty, prompt(&interp).as_bytes());

    let mut chunk = [0u8; 64];
    loop {
        // SAFETY: single-threaded shell.
        if unsafe { (&raw const STOP_ASKED).read() } {
            stop();
        }
        let n = match tty_read(tty, &mut chunk) {
            Some(n) => n,
            // The terminal is gone: leave, rather than spin on a dead channel.
            None => {
                kprint(b"nxsh: terminal closed\r\n");
                return EXIT_OK;
            }
        };
        // `Ctrl-C` at a prompt discards the line (§11h): the byte became an event, so the read came
        // back empty. The tty only reports it; what it means is the caller's decision.
        // SAFETY: single-threaded shell.
        if unsafe { (&raw const INTERRUPTED).read() } {
            // SAFETY: same.
            unsafe { (&raw mut INTERRUPTED).write(false) };
            tty_write(tty, &editor.interrupt());
            tty_write(tty, prompt(&interp).as_bytes());
            continue;
        }
        let read = &chunk[..n];
        for (i, &b) in read.iter().enumerate() {
            for out in editor.feed(b, read.get(i + 1).copied(), &mut history, &mut |whole| interp.complete(whole)) {
                match out {
                    nxsh::editor::Out::Write(bytes) => tty_write(tty, &bytes),
                    nxsh::editor::Out::Prompt => tty_write(tty, prompt(&interp).as_bytes()),
                    nxsh::editor::Out::Continuation => tty_write(tty, nxsh::repl::continuation_prompt().as_bytes()),
                    nxsh::editor::Out::Run(src) => {
                        if let Some(status) = run_and_render(&mut interp, tty, &src) {
                            return status;
                        }
                        tty_write(tty, prompt(&interp).as_bytes());
                    }
                    nxsh::editor::Out::Eof => return EXIT_OK,
                }
            }
        }
    }
}

/// Run one line, render what it produced, and report whether the shell was asked to leave.
///
/// The loop intercepts nothing: `exit` is a shell-state builtin (§3) whose status travels the error
/// channel, and a special case in the driver is a second implementation of the language (it once
/// made `exit 1` fail). Do not add a case there.
fn run_and_render(interp: &mut Interp, tty: u64, src: &str) -> Option<i64> {
    match interp.run_line(src) {
        Ok(Some(text)) => {
            tty_write_crlf(tty, &text);
            None
        }
        Ok(None) => None,
        Err(e) if e.is_exit() => Some(e.exit.unwrap_or(0) as i64),
        Err(e) => {
            // The REPL's error path: `Host::diag` is the script path, whose host has no terminal.
            // The newline goes after the painted message, never inside it; see
            // [`repl::diagnostic`](nxsh::repl::diagnostic).
            let mut msg = nxsh::repl::diagnostic(&e.message, tty != 0);
            msg.push('\n');
            tty_write_crlf(tty, &msg);
            None
        }
    }
}

/// Send one tty request and wait for its reply, copying the body into `out`, with the handles the
/// reply carried; `None` if the exchange failed.
fn tty_request(ch: u64, op: u16, body: &[u8], out: &mut [u8]) -> Option<(bool, usize, Received)> {
    tty_exchange(ch, op, body, out, false)
}

/// [`tty_request`], and when `watch`, the notification channel watched for a terminate request.
fn tty_exchange(ch: u64, op: u16, body: &[u8], out: &mut [u8], watch: bool) -> Option<(bool, usize, Received)> {
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
    tty_await_reply(ch, out, watch)
}

/// Wait for the reply to the request just sent, stepping over any interrupt event.
///
/// Re-entered because the interrupt is the one unsolicited message on this channel (§11h) and the
/// reply is still coming behind it. When `watch`, the notification channel is watched too, so a
/// terminate request finds the shell at the prompt's read; only the prompt watches, since elsewhere
/// a stage's `ChildExited` could be taken from [`reap`].
fn tty_await_reply(ch: u64, out: &mut [u8], watch: bool) -> Option<(bool, usize, Received)> {
    // SAFETY: single-threaded shell; set once at startup.
    let notif = unsafe { NOTIF_CH };
    let watch = watch && notif != 0;
    loop {
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid writable buffers; one or two waiters.
        let waited = unsafe {
            WAIT_HANDLES[0] = ch;
            WAIT_HANDLES[1] = notif;
            let n = if watch { 2 } else { 1 };
            let (handles, results) = ((&raw const WAIT_HANDLES) as u64, (&raw mut WAIT_RESULTS) as u64);
            syscall4(SYS_WAIT, handles, n, results, u64::MAX)
        };
        if waited < 1 {
            return None;
        }
        if !watch {
            break;
        }
        if signalled(waited, notif) {
            loop {
                // SAFETY: `NOTIF` is a valid 64-byte out-param.
                if unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) } != 0 {
                    break;
                }
                // SAFETY: the kernel wrote a Notification into `NOTIF`.
                if unsafe { (&raw const NOTIF.kind).read() } == KIND_TERMINATE_REQUESTED {
                    stop();
                }
            }
        }
        if signalled(waited, ch) {
            break;
        }
    }
    // SAFETY: single-threaded shell; `TTY_MSG` is lent for the call.
    let sent = unsafe { channel_recv_raw(ch, &raw mut TTY_MSG) }.ok()?;
    // SAFETY: bounded read-only slice over the reply.
    unsafe {
        let payload_len = u32::from_le_bytes([TTY_MSG[4], TTY_MSG[5], TTY_MSG[6], TTY_MSG[7]]) as usize;
        let rep = core::slice::from_raw_parts(((&raw const TTY_MSG) as *const u8).add(24), payload_len.min(4096 - 24));
        let m = librsproto::decode(rep).ok()?;
        // The interrupt is unsolicited, so it can land where a reply was expected: record it and
        // keep waiting, or the real reply stays queued for whoever asked next.
        if m.op == librsproto::OP_TTY_INTERRUPT {
            INTERRUPTED = true;
            return tty_await_reply(ch, out, watch);
        }
        let n = m.body.len().min(out.len());
        out[..n].copy_from_slice(&m.body[..n]);
        Some((m.is_error(), n, sent))
    }
}

/// Take whatever keystrokes are available. `None` if the terminal is unusable.
fn tty_read(ch: u64, out: &mut [u8]) -> Option<usize> {
    match tty_exchange(ch, librsproto::OP_TTY_READ, &[], out, true) {
        Some((false, n, _)) => Some(n),
        _ => None,
    }
}

/// Turn the terminal's own echo on or off; the shell holds the line buffer, so server-side echo
/// would fight it.
fn tty_set_echo(ch: u64, on: bool) {
    let flags = [if on { librsproto::TTY_MODE_ECHO } else { 0 }];
    let mut scratch = [0u8; 1];
    let _ = tty_request(ch, librsproto::OP_TTY_SET_MODE, &flags, &mut scratch);
}

fn tty_write(ch: u64, text: &[u8]) {
    let mut scratch = [0u8; 1];
    // In parts: a body over `RS_MAX_BODY` is refused whole, and the screen and the line part.
    for part in text.chunks(librsproto::RS_MAX_BODY) {
        let _ = tty_request(ch, librsproto::OP_TTY_WRITE, part, &mut scratch);
    }
}

/// Ask the tty server for another terminal on this one's backend (`Tty::OpenSibling`), for a stage
/// to prompt on. `None` if it refused or the exchange failed; the stage then runs without one.
fn tty_open_sibling(ch: u64) -> Option<u64> {
    let mut scratch = [0u8; 1];
    match tty_request(ch, librsproto::OP_TTY_OPEN_SIBLING, &[], &mut scratch) {
        Some((false, _, mut sent)) => sent.take(0),
        _ => None,
    }
}

fn tty_write_crlf(ch: u64, text: &str) {
    for chunk in text.split('\n') {
        if !chunk.is_empty() {
            tty_write(ch, chunk.as_bytes());
        }
        tty_write(ch, b"\r\n");
    }
}

/// Set when the terminal has reported `Ctrl-C` (§11h). The interrupt can arrive in any exchange, so
/// every path receiving from the tty channel records it here; the caller decides what it means.
static mut INTERRUPTED: bool = false;

/// This shell has been asked to exit: a `TerminateRequested` from its session's supervisor, at a
/// shutdown. Unlike [`INTERRUPTED`] it is sticky, so no `catch` recovers from it; the REPL exits
/// once what was running has unwound.
static mut STOP_ASKED: bool = false;

/// The shell's notification channel, where that request arrives: set once at startup.
static mut NOTIF_CH: u64 = 0;

/// Read every notification queued on `notif` while a pipeline runs: a `ChildExited` is kept in
/// `exits` as `(code, crashed)`, and a `TerminateRequested` asks every stage to stop once (`asked`)
/// and marks the shell to exit. Shared by the capture wait and [`reap`], so an exit read by the
/// first is counted by the second.
fn drain_notifications(notif: u64, children: &[u64], asked: &mut bool, exits: &mut Vec<(i32, bool)>) {
    loop {
        // SAFETY: `NOTIF` is a valid 64-byte out-param.
        if unsafe { syscall4(SYS_NOTIF_RECV, notif, (&raw mut NOTIF) as u64, 0, 0) } != 0 {
            return; // WouldBlock: drained
        }
        // SAFETY: the kernel wrote a Notification into `NOTIF`.
        let (kind, body) = unsafe { ((&raw const NOTIF.kind).read(), (&raw const NOTIF.body).read()) };
        if kind == KIND_TERMINATE_REQUESTED {
            // SAFETY: single-threaded shell.
            unsafe { (&raw mut STOP_ASKED).write(true) };
            if !*asked {
                *asked = true;
                for &c in children {
                    // SAFETY: a Process handle this shell owns, with SIGNAL from spawn.
                    unsafe { syscall1(SYS_PROCESS_TERMINATE, c) };
                }
            }
        } else if kind == KIND_CHILD_EXITED {
            let exit_kind = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            let code = i32::from_le_bytes([body[8], body[9], body[10], body[11]]);
            exits.push((code, exit_kind != EXIT_KIND_NORMAL));
        }
    }
}

/// Say this shell was asked to stop, and exit.
fn stop() -> ! {
    kprint(b"nxsh: asked to stop, exiting\n");
    exit(EXIT_OK)
}

static mut TTY_MSG: [u8; 4096] = [0; 4096];
static mut TTY_HANDLES: [u64; 8] = [0; 8];

/// Is `path` a binding in `ns`, or an ancestor of one?
/// Is `path` a binding in `ns`, or an ancestor of one? The namespace walk `list` uses
/// (`SYS_NS_ENUMERATE`), as a yes/no question. `/` is true by construction.
fn binding_at_or_under(ns: u64, path: &str) -> bool {
    if path == "/" {
        return true;
    }
    let exact = path.trim_end_matches('/');
    // One trailing slash, so "is an ancestor of" cannot match `/binary` for `/bin`.
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
        let bound = unsafe { core::slice::from_raw_parts((&raw const entry.path) as *const u8, len) };
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
    let (argv, env, terminal) = match boot.setup() {
        Some(Ok(s)) => (s.argv, s.env, s.terminal),
        _ => (Vec::new(), libstream::wire::Record::default(), None),
    };
    // Reported from the receiving end, the only end that proves anything: a parent that sent no
    // setup message and one that sent an empty environment look identical from outside. A login
    // gate waits on this line (`nxsh: up`). It goes to the debug console, not the tty: under
    // `nxterm` the grid renders only under `test-harness`.
    libkern::debug::Line::new().s(b"nxsh: up (env: ").u(env.values.len() as u64).s(b" fields)").end();
    // SAFETY: single-threaded shell; written once, here, before any wait reads it.
    unsafe { NOTIF_CH = notif };
    exit(run(notif, ns, &argv, env, terminal))
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    kprint(b"nxsh: panic\n");
    exit(EXIT_SCRIPT_FAILED)
}
