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

use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use libkern::Rights;
use libkern::abi::{IPC_PAYLOAD_SIZE, NS_ENTRY_PATH_MAX};
use libos::boot::Start;
use libos::log::{self, Line};
use libos::{Channel, Deadline, File, Handle, MapRead, Msg, Namespace, NsReadOnly, Only, Ref, WaitSet, block_on};
use libstream::channel::{ChannelSink, IpcPort, MsgPort};
use libstream::setup::{StageStreams, receive_setup, setup_is_pending};
use libstream::wire::ByteSink;
use notices::Notices;
use nxsh::host::{Host, PipelineRun, StageSpec, StageStatus};
use nxsh::pipeline::{Pipeline, Sys};
use nxsh::{Interp, RunMode};
use tty::Tty;

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
const PROGRAM_DIRS: &[&str] = &["/bin/"];

/// Cap on a captured stage output, so a spawned program cannot run the shell out of memory.
const MAX_CAPTURE: usize = 1 << 20;

/// Write every diagnostic waiting on `err` to the terminal, and do not block.
///
/// Each is one whole message: `stderr` is shared between stages, and a partial line would
/// interleave.
fn drain_diagnostics(err: Option<Ref<'_, Channel, Only>>, tty: Option<&Tty>) {
    let Some(err) = err else { return };
    let mut msg = Msg::new();
    loop {
        // Every spawned program holds a `SEND` duplicate of this endpoint, so a stage can send
        // handles: they close as `_sent` drops, or the shell's spawns would fail in the end.
        let Ok(_sent) = err.recv(&mut msg) else {
            break; // WouldBlock (nothing waiting) or the last stage has closed its end
        };
        let (level, text) = libstream::diag::parse(msg.payload());
        let Ok(text) = core::str::from_utf8(text) else {
            continue; // a diagnostic is text; anything else is not ours to render
        };
        // Strip the newline: `write_crlf` ends every line.
        let text = text.strip_suffix('\n').unwrap_or(text);
        if let Some(tty) = tty {
            // Drawn like the shell's own errors, a warning in its own colour, a notice plain
            // (see `repl::diagnostic`).
            let painted = match nxsh::style::for_level(level) {
                Some(sgr) => nxsh::style::paint(true, sgr, text),
                None => alloc::string::String::from(text),
            };
            tty.write_crlf(&painted);
        } else {
            log::write(text.as_bytes());
            log::write(b"\n");
        }
    }
}

mod notices;
mod stage;
mod stages;
mod tty;

struct NitroxHost {
    namespace: Handle<Namespace, NsReadOnly>,
    notices: Rc<Notices>,
    /// The terminal, for §11h's interrupt checkpoint. `None` when there is none (a script or a
    /// Tier-0 stage).
    tty: Option<Rc<Tty>>,
}

impl NitroxHost {
    /// Say `text` where the person will see it: the terminal if there is one, else the kernel log,
    /// which reaches COM1 only.
    fn say(&mut self, text: &str) {
        match &self.tty {
            Some(tty) => tty.write_crlf(text.strip_suffix('\n').unwrap_or(text)),
            None => log::write(text.as_bytes()),
        }
    }

    fn resolve_program(&mut self, name: &str) -> Result<Handle<File, MapRead>, String> {
        if nxsh::exec::is_path_head(name) {
            return lookup(&self.namespace, name).ok_or_else(|| alloc::format!("`{name}` does not resolve"));
        }
        for dir in PROGRAM_DIRS {
            if let Some(h) = lookup(&self.namespace, &alloc::format!("{dir}{name}")) {
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
                    images.0.push(Some(img));
                    argvs.push(argv);
                }
                Err(e) => {
                    // Said on the debug console too: under `nxterm` errors go to a grid that
                    // renders only under `test-harness`, so an unfound command would fail silently.
                    Line::new().s(b"nxsh: could not resolve `").s(s.program.as_bytes()).s(b"`").end();
                    return Err(e);
                }
            }
        }

        let (notices, tty) = (self.notices.clone(), self.tty.clone());
        let tty = tty.as_deref();
        let names: Vec<&str> = stages.iter().map(|s| s.program.as_str()).collect();
        let mut p = Pipeline::new(stages::Stages::new(self, images, &argvs, env));
        let ends = p.start(&names, input.is_some())?;

        // An interrupt that arrived during setup: handing a stage its terminal is a tty exchange,
        // and `Tty::await_reply` records a `Ctrl-C` there rather than leaving it queued, so the
        // waits below would never see it and `sleep 60` would run its minute (`test-interactive`
        // step 19). The flag stays set for the evaluator's checkpoint.
        if notices.interrupted() {
            terminate_all(&mut p);
        }

        // Feed the head stage only once everything downstream is running.
        //
        // The one window where the shell is not draining: `put` blocks on a depth-4 pipe, so a head
        // stage that stops reading `stdin` stops the shell, and a stage blocked on its 37th unread
        // diagnostic is such a stage. Closing it needs an interleaved send in `ChannelSink`.
        if let (Some(tx), Some(bytes)) = (ends.feed, input) {
            drain_diagnostics(p.sys.channel(ends.diagnostics), tty);
            if let Some(feed) = p.sys.take_channel(tx) {
                let mut sender = ChannelSink::new(IpcPort::owning(feed), IPC_PAYLOAD_SIZE);
                let _ = sender.put(bytes);
                let _ = sender.finish();
                // Closed, so the stage sees the stream end.
                drop(sender);
            }
            p.close(tx);
            drain_diagnostics(p.sys.channel(ends.diagnostics), tty);
        }

        // Read the tail *before* reaping: a stage that has filled its channel is waiting
        // for this reader, so reaping first would deadlock any producer larger than one
        // pipe's worth.
        let mut output: Option<Vec<u8>> = None;
        // Whether a terminate was passed on; the exits read here are kept for `reap`.
        let mut stop_sent = false;
        if let Some((rx, tail)) = ends.capture.and_then(|rx| Some((rx, p.sys.take_channel(rx)?))) {
            // The terminal is watched here, not only in `reap` (§11h): the tail's output does not
            // close until the last stage exits, so `sleep 60 | …` blocks here. Diagnostics are
            // drained here too, with or without a terminal, or a long job's progress would arrive
            // as a wall of text at the end.
            //
            // One loop that waits, drains and reads, taking the tail's messages one at a time
            // rather than through `ChannelReceiver::receive`, which loops on `rx` alone: a stage
            // blocked on its 37th unread diagnostic would never send its `last` message.
            let mut port = IpcPort::owning(tail);
            let mut bytes: Vec<u8> = Vec::new();
            let mut complete = false;
            let mut overflowed = false;
            loop {
                // The notification channel too: a terminate request must reach the stages during
                // this wait, and a stage's exit read here is kept for `reap`.
                drain_notifications(&mut p, &notices, &mut stop_sent);
                // Poll before blocking: a waiter arriving after a message was enqueued never sees
                // that edge, and the tty queues the interrupt in the same console read as the line
                // that started the pipeline.
                drain_diagnostics(p.sys.channel(ends.diagnostics), tty);
                if let Some(tty) = tty
                    && tty.poll_interrupt()
                {
                    terminate_all(&mut p);
                    continue;
                }
                let waited = {
                    let mut set = WaitSet::<4>::new();
                    let _ = set.add(port.channel());
                    if let Some(diagnostics) = p.sys.channel(ends.diagnostics) {
                        let _ = set.add(diagnostics);
                    }
                    if let Some(notify) = notices.notify() {
                        let _ = set.add(notify);
                    }
                    if let Some(tty) = tty {
                        let _ = set.add(tty.channel());
                    }
                    set.wait(Deadline::NEVER)
                };
                let output_ready = match waited {
                    Ok(found) if !found.is_empty() => found.contains(0),
                    _ => break, // the wait failed; there is nothing more to read
                };
                // Only `rx` means output; reading on any other wake would block.
                if output_ready {
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
            drain_diagnostics(p.sys.channel(ends.diagnostics), tty);
            if overflowed {
                return Err(String::from("a stage produced more output than nxsh will hold"));
            }
            if complete && !bytes.is_empty() {
                output = Some(bytes);
            }
            drop(port);
            p.close(rx);
        }

        let reaped = reap(&mut p, &notices, tty, ends.diagnostics, stages, strict, stop_sent);
        p.reaped();
        // One last drain, after every stage is gone: a program's final word can be sent just before
        // `exit`, after the wait that noticed the exit returned.
        drain_diagnostics(p.sys.channel(ends.diagnostics), tty);
        p.close(ends.diagnostics);

        Ok(PipelineRun { stages: reaped, output })
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
        libfs::read(&self.namespace, path).map_err(|e| match e {
            libfs::FileError::NotFound => alloc::format!("cannot open `{path}`"),
            libfs::FileError::TooLarge => alloc::format!("`{path}` is too large to open"),
            _ => alloc::format!("cannot read `{path}`"),
        })
    }

    /// Replace `path` with `bytes` (`save`).
    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
        libfs::save(&self.namespace, path, bytes).map_err(|e| match e {
            libfs::SaveError::Write(libfs::FileError::TooLarge) => alloc::format!("`{path}` is too large to save"),
            _ => alloc::format!("cannot save `{path}`"),
        })
    }

    fn exists(&mut self, path: &str) -> bool {
        // A path can be real in three ways, and `cd` must accept all three.
        // 1. It names a binding, or sits above one (`/`, `/bin`): nothing resolves at it, but
        // `list` shows it, so ask the namespace the same question `list` does.
        if binding_at_or_under(&self.namespace, path) {
            return true;
        }
        // 2. It resolves to an object — an ordinary file.
        if lookup::<File, MapRead>(&self.namespace, path).is_some() {
            return true;
        }
        // 3. A *directory* does not resolve to a mappable object, so a failed lookup
        //    is not proof of absence — a directory session is what answers for one.
        let mut msg = Msg::new();
        match librsproto::session::Dir::resolve(&self.namespace, path, &mut msg) {
            Ok(d) => {
                d.close();
                true
            }
            Err(_) => false,
        }
    }

    fn commands(&mut self) -> Vec<String> {
        // The list `resolve_program` searches, so Tab never offers a name the shell would fail to
        // run.
        let mut out = Vec::new();
        for dir in PROGRAM_DIRS {
            let path = dir.strip_suffix('/').unwrap_or(dir);
            if let Ok(entries) = libfs::list_directory(&self.namespace, path) {
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
        match libfs::list_directory(&self.namespace, dir) {
            Ok(entries) => entries
                .iter()
                .map(|e| (String::from_utf8_lossy(e.name()).into_owned(), e.kind == librsproto::file::DIRENT_KIND_DIR))
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    fn diag(&mut self, text: &str) {
        // Not painted: every caller but one is script mode, whose host has no terminal and
        // plain-text output. The exception is `time`'s line, plain on purpose since it is read off a
        // laptop screen and a serial console. It also arrives in fragments, so there is no whole run
        // to bracket; the REPL paints in `run_and_render`.
        self.say(text);
    }

    /// §11h. A non-blocking receive: this runs at every statement boundary and must never park.
    /// Anything waiting is the unsolicited interrupt.
    fn interrupted(&mut self) -> bool {
        // A terminate request, between statements: read off the notification channel here too, so a
        // loop that runs no stage still sees it. `reap` has counted every exit before a pipeline
        // returns.
        self.notices.drain(&mut Vec::new());
        if self.notices.stop_asked() {
            return true;
        }
        let Some(tty) = &self.tty else { return false };
        tty.poll_interrupt();
        self.notices.take_interrupt() || self.notices.stop_asked()
    }

    /// The kernel's monotonic clock, for `time`.
    fn now_ns(&mut self) -> Option<u64> {
        libos::time::now().ok()
    }

    /// `display`'s output goes to the terminal when there is one, else to the kernel log. A shell
    /// with no terminal still writes without a handle, so nothing can redirect it.
    /// TODO(tty-server): `docs/rationale/deferred-decisions.md`.
    fn out(&mut self, text: &str) {
        self.say(text);
    }

    fn styled(&mut self) -> bool {
        self.tty.is_some()
    }
}

/// Collect one `ChildExited` per spawned stage.
///
/// Per-stage attribution is exact only for a one-stage chain: `sys_process_spawn` returns a handle,
/// `ChildExited` carries a ProcessId, and `sys_wait` does not accept a process handle. A longer
/// chain reports the aggregate truthfully and marks each entry with the status of some stage, not
/// claiming which. Filed as `TODO(pipeline-stage-attribution)`; the fix is a pid on `HandleInfo`
/// for a process handle, or a handle in the notification, and it is an ABI change.
fn reap(
    p: &mut Pipeline<stages::Stages<'_>>,
    notices: &Notices,
    tty: Option<&Tty>,
    err: u64,
    stages: &[StageSpec],
    strict: bool,
    // Whether the capture wait passed a terminate request on.
    stop_sent: bool,
) -> Vec<StageStatus> {
    let children = p.parts().0.len();
    let mut asked = stop_sent;
    while p.parts().1.len() < children {
        // Wait on the terminal as well as the children: this is the only place `Ctrl-C` is noticed
        // while a stage is slow (§11h). Diagnostics are drained here too: a pipeline with nothing
        // on its tail spends the whole job in this loop.
        drain_diagnostics(p.sys.channel(err), tty);
        let waited = {
            let mut set = WaitSet::<3>::new();
            if let Some(notify) = notices.notify() {
                let _ = set.add(notify);
            }
            if let Some(diagnostics) = p.sys.channel(err) {
                let _ = set.add(diagnostics);
            }
            if let Some(tty) = tty {
                let _ = set.add(tty.channel());
            }
            set.wait(Deadline::NEVER)
        };
        drain_diagnostics(p.sys.channel(err), tty);
        if !matches!(waited, Ok(found) if !found.is_empty()) {
            continue;
        }
        // Poll before blocking, for the same reason as the capture wait above. Every stage is asked
        // to stop, once; a stage that ignores the request is still waited for.
        if let Some(tty) = tty
            && !asked
            && tty.poll_interrupt()
        {
            asked = true;
            terminate_all(p);
        }
        drain_notifications(p, notices, &mut asked);
    }

    let seen = p.parts().1.clone();
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
            terminate_all(p);
        }
        out.push(StageStatus { command: s.program.clone(), exit_status: code, crashed, cancelled: false });
    }
    out
}

/// Ask every stage to stop: a request, which a stage may take its time over.
fn terminate_all(p: &mut Pipeline<stages::Stages<'_>>) {
    let children = p.parts().0.to_vec();
    for c in children {
        p.sys.terminate(c);
    }
}

/// Read what the notification queue holds while a pipeline runs: exits are kept for `reap`, and a
/// terminate request asks every stage to stop once (`asked`).
fn drain_notifications(p: &mut Pipeline<stages::Stages<'_>>, notices: &Notices, asked: &mut bool) {
    if notices.drain(p.parts().1) && !*asked {
        *asked = true;
        terminate_all(p);
    }
}

/// Resolve `path` to a handle of the kind the caller expects, with **file** rights — mapping and
/// stat.
fn lookup<T, M>(ns: &Handle<Namespace, NsReadOnly>, path: &str) -> Option<Handle<T, M>> {
    lookup_rights(ns, path, Rights::MAP_READ | Rights::INSPECT)
}

/// Resolve `path` asking for `rights`.
///
/// A char device is not a file: `/dev/console` is read with `READ`, not `MAP_READ | INSPECT`. The
/// wrong rights give a handle that resolves and then fails every read.
fn lookup_rights<T, M>(ns: &Handle<Namespace, NsReadOnly>, path: &str, rights: Rights) -> Option<Handle<T, M>> {
    block_on(ns.lookup(path, rights)).ok()
}

fn run(
    root: Handle<Namespace, NsReadOnly>,
    notices: Rc<Notices>,
    argv: &[String],
    env: libstream::wire::Record,
    terminal: Option<Handle<Channel, Only>>,
) -> i64 {
    let mut host = NitroxHost { namespace: root, notices, tty: None };

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
            // On the terminal it was handed: script mode keeps none for the host, and the kernel
            // log reaches nobody on a machine with no serial port.
            match terminal {
                Some(t) => {
                    Tty::new(t, host.notices.clone()).write_crlf("usage: nxsh [SCRIPT [ARG...] | -c SOURCE [ARG...]]")
                }
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
        None => return repl(host, env, terminal),
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
fn repl(mut host: NitroxHost, env: libstream::wire::Record, terminal: Option<Handle<Channel, Only>>) -> i64 {
    // The session's terminal, a channel rather than the raw device. Handed down if the parent gave
    // one (a terminal emulator hands over the terminal whose backend it holds; `/dev/tty` cannot
    // name a particular window), resolved otherwise as a serial login does. See
    // `docs/architecture/graphical-session.md` §6.1.
    let chan = match terminal {
        Some(t) => t,
        None => {
            let Some(t) = lookup_rights(&host.namespace, "/dev/tty", Rights::SEND | Rights::RECV | Rights::WAIT) else {
                log::write(b"nxsh: /dev/tty not found\r\n");
                return EXIT_USAGE;
            };
            t
        }
    };
    let notices = host.notices.clone();
    let tty = Rc::new(Tty::new(chan, notices.clone()));
    host.tty = Some(tty.clone());

    let mut interp = Interp::with_host(alloc::boxed::Box::new(host), RunMode::Repl);
    interp.set_env(env);

    // The shell edits its own input, as `bash` does with `ICANON` cleared: history and completion
    // are its knowledge and share one editing loop with backspace and kill. The discipline is
    // linked, not reimplemented. Server-side echo goes off because only the shell knows what the
    // screen should show after a recall.
    tty.set_echo(false);
    let mut editor = nxsh::editor::Editor::new();

    tty.write(b"\r\n");
    let banner = alloc::format!("nxsh: interactive shell, Nitrox {BUILD_COMMIT} (Ctrl-D or `exit` to leave)");
    tty.write(nxsh::style::paint(true, nxsh::style::BANNER, &banner).as_bytes());
    tty.write(b"\r\n");

    let mut history = nxsh::history::History::new();
    let prompt = |interp: &Interp| nxsh::repl::prompt(interp.cwd().unwrap_or("/"), true);
    tty.write(prompt(&interp).as_bytes());

    let mut chunk = [0u8; 64];
    loop {
        if notices.stop_asked() {
            notices::stop();
        }
        let n = match tty.read(&mut chunk) {
            Some(n) => n,
            // The terminal is gone: leave, rather than spin on a dead channel.
            None => {
                log::write(b"nxsh: terminal closed\r\n");
                return EXIT_OK;
            }
        };
        // `Ctrl-C` at a prompt discards the line (§11h): the byte became an event, so the read came
        // back empty. The tty only reports it; what it means is the caller's decision.
        if notices.take_interrupt() {
            tty.write(&editor.interrupt());
            tty.write(prompt(&interp).as_bytes());
            continue;
        }
        let read = &chunk[..n];
        for (i, &b) in read.iter().enumerate() {
            for out in editor.feed(b, read.get(i + 1).copied(), &mut history, &mut |whole| interp.complete(whole)) {
                match out {
                    nxsh::editor::Out::Write(bytes) => tty.write(&bytes),
                    nxsh::editor::Out::Prompt => tty.write(prompt(&interp).as_bytes()),
                    nxsh::editor::Out::Continuation => tty.write(nxsh::repl::continuation_prompt().as_bytes()),
                    nxsh::editor::Out::Run(src) => {
                        if let Some(status) = run_and_render(&mut interp, &tty, &src) {
                            return status;
                        }
                        tty.write(prompt(&interp).as_bytes());
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
fn run_and_render(interp: &mut Interp, tty: &Tty, src: &str) -> Option<i64> {
    match interp.run_line(src) {
        Ok(Some(text)) => {
            tty.write_crlf(&text);
            None
        }
        Ok(None) => None,
        Err(e) if e.is_exit() => Some(e.exit.unwrap_or(0) as i64),
        Err(e) => {
            // The REPL's error path: `Host::diag` is the script path, whose host has no terminal.
            // The newline goes after the painted message, never inside it; see
            // [`repl::diagnostic`](nxsh::repl::diagnostic).
            let mut msg = nxsh::repl::diagnostic(&e.message, true);
            msg.push('\n');
            tty.write_crlf(&msg);
            None
        }
    }
}

/// Is `path` a binding in `ns`, or an ancestor of one? The namespace walk `list` uses, as a yes/no
/// question. `/` is true by construction.
fn binding_at_or_under(ns: &Handle<Namespace, NsReadOnly>, path: &str) -> bool {
    if path == "/" {
        return true;
    }
    let exact = path.trim_end_matches('/');
    // One trailing slash, so "is an ancestor of" cannot match `/binary` for `/bin`.
    let mut prefix = String::from(exact);
    prefix.push('/');

    for entry in ns.bindings() {
        let Ok(entry) = entry else {
            return false; // the walk ends at the first refusal
        };
        let len = (entry.path_len as usize).min(NS_ENTRY_PATH_MAX);
        let Ok(bound) = core::str::from_utf8(&entry.path[..len]) else {
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
    // SAFETY: the kernel's bootstrap registers, read once, here.
    let mut start = unsafe { Start::from_registers(notif, ns, endpoint, arg0) };
    let endpoint: Option<Handle<Channel, Only>> = start.take_handle0();
    let setup = if setup_is_pending(start.arg0) {
        endpoint.as_ref().and_then(|e| receive_setup(e, Deadline::NEVER).ok().flatten())
    } else {
        None
    };
    // The streams are the shell's for its life, though it reads none of them.
    let (argv, env, terminal, _streams) = match setup {
        Some(s) => (s.argv, s.env, s.terminal, s.streams),
        _ => (Vec::new(), libstream::wire::Record::default(), None, StageStreams::default()),
    };
    // Reported from the receiving end, the only end that proves anything: a parent that sent no
    // setup message and one that sent an empty environment look identical from outside. A login
    // gate waits on this line (`nxsh: up`). It goes to the debug console, not the tty: under
    // `nxterm` the grid renders only under `test-harness`.
    Line::new().s(b"nxsh: up (env: ").u(env.values.len() as u64).s(b" fields)").end();
    let notices = Rc::new(Notices::new(start.notify.take()));
    let Some(root) = start.root.take() else {
        log::write(b"nxsh: no root namespace\n");
        libos::exit(EXIT_USAGE as i32)
    };
    libos::exit(run(root, notices, &argv, env, terminal) as i32)
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    log::write(b"nxsh: panic\n");
    libos::exit(EXIT_SCRIPT_FAILED as i32)
}
