//! The **stage prologue** — what every coreutil does between `_start` and its own work.
//!
//! A coreutil is a *pipeline stage*: it is spawned by a shell, receives its streams and
//! `argv` in the setup message (`docs/spec/pipeline-stdio.md`), does its job, and exits
//! with a status.
//!
//! ## Both tiers, deliberately
//!
//! Tier 1 (a setup message is pending) is the shell-spawned case and supplies `argv`.
//! Tier 0 — no descriptor, no message — is what a program gets when init or the test harness
//! spawns it *without* a shell. A Tier-0 stage has no `argv` and no streams, so it can only
//! run its argument-free default.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use libkern::KError;
use libkern::abi::KIND_TERMINATE_REQUESTED;
use libos::boot::Start;
use libos::log;
use libos::{Carried, Channel, Deadline, Handle, Msg, Namespace, Notify, NsReadOnly, Only, Ref, WaitSet, block_on};
use libstream::channel::MsgPort;
use libstream::setup::{StageStreams, receive_setup, setup_is_pending};
use libstream::wire::WireError;

/// The program did its job. (`i32` — the width `libos::exit` takes.)
pub const EXIT_OK: i32 = 0;
/// The program failed at its job (I/O error, missing file, …).
pub const EXIT_FAILURE: i32 = 1;
/// The program was invoked wrongly (bad flag, missing operand).
pub const EXIT_USAGE: i32 = 2;

/// The transport flag in the IPC header that marks the final message of a stream.
const MSG_FLAG_LAST: u16 = 1 << 0;

/// A [`MsgPort`] over a stream endpoint the [`Stage`] keeps: it closes nothing, and the stage's
/// streams stay open to its exit, as a port that owned the endpoint would not leave them.
pub struct Port<'a> {
    end: Ref<'a, Channel, Only>,
    msg: Box<Msg>,
}

impl<'a> Port<'a> {
    /// A port on `end`, borrowed.
    pub fn new(end: Ref<'a, Channel, Only>) -> Self {
        Port { end, msg: Box::new(Msg::new()) }
    }
}

fn transport(e: libos::Error) -> WireError {
    match e.code() {
        KError::PeerClosed => WireError::PeerClosed,
        _ => WireError::SinkFull,
    }
}

impl MsgPort for Port<'_> {
    fn send(&mut self, payload: &[u8], last: bool) -> libstream::wire::Result<()> {
        self.msg.set(payload).map_err(|_| WireError::SinkFull)?;
        self.msg.set_flags(if last { MSG_FLAG_LAST } else { 0 });
        let op = self.end.send_held(&self.msg, &mut Carried::new()).map_err(transport)?;
        block_on(op).map(|_| ()).map_err(transport)
    }

    fn recv(&mut self, out: &mut Vec<u8>) -> libstream::wire::Result<bool> {
        loop {
            match self.end.recv(&mut self.msg) {
                // Whatever handles the message brought close as they drop: a frame carries none.
                Ok(_) => break,
                Err(e) if e.code() == KError::WouldBlock => {
                    let mut set = WaitSet::<1>::new();
                    set.add(self.end).map_err(|_| WireError::PeerClosed)?;
                    set.wait(Deadline::NEVER).map_err(|_| WireError::PeerClosed)?;
                }
                Err(e) => return Err(transport(e)),
            }
        }
        out.extend_from_slice(self.msg.payload());
        Ok(self.msg.flags() & MSG_FLAG_LAST != 0)
    }
}

/// Everything a coreutil receives at startup.
pub struct Stage {
    /// This process's notification queue; `None` if it was handed none.
    pub notif: Option<Handle<Notify, Only>>,
    /// This process's root namespace — how a coreutil reaches the filesystem.
    pub namespace: Handle<Namespace, NsReadOnly>,
    /// The standard streams. All three are `None` in Tier 0.
    pub streams: StageStreams,
    /// `argv`, with `argv[0]` the program name. Empty in Tier 0.
    pub argv: Vec<String>,
    /// The working directory from the environment's `PWD`; `None` in Tier 0 or with no `PWD`, and a
    /// relative path then fails rather than resolving against `/` — see [`Stage::path`].
    pub cwd: Option<String>,
    /// The terminal the shell handed this stage, if any: what a stage that has to *ask* something
    /// prompts on. `None` in Tier 0, under a spawner that passes none, and when the tty server
    /// had none to spare, so a stage that needs one must say so rather than assume it.
    pub terminal: Option<Handle<Channel, Only>>,
    /// The whole environment the spawner passed — empty in Tier 0. A stage that hands it on
    /// unchanged (`with`) needs the record, not `cwd`.
    pub env: libstream::wire::Record,
    /// Whether a setup message was received (Tier 1).
    pub from_shell: bool,
    /// The bootstrap endpoint the setup message came on, kept open for the process's life.
    _endpoint: Option<Handle<Channel, Only>>,
}

/// Pull `PWD` out of an environment record.
///
/// `PWD` is a *conventional* entry, not a distinct field: there is no second source of truth
/// for it to disagree with, unlike Unix's `$PWD` versus `getcwd()`.
fn cwd_of(env: &libstream::wire::Record) -> Option<String> {
    let i = env.schema.fields.iter().position(|f| f.name == "PWD")?;
    env.values.get(i)?.as_str().map(String::from)
}

impl Stage {
    /// Has someone asked this stage to exit?
    ///
    /// A **non-blocking** drain of this process's notification queue, looking for
    /// `TerminateRequested`. Nothing forces a stage to call it: there is no forcible kill
    /// in this system.
    ///
    /// A stage that blocks should put `notif` in its wait set and call this when the
    /// wait returns, so the request takes effect promptly.
    pub fn terminate_requested(&self) -> bool {
        let Some(notif) = &self.notif else { return false };
        let mut hit = false;
        // Nothing queued is `Ok(None)`; a refusal ends the drain as that does.
        while let Ok(Some(n)) = notif.recv() {
            hit |= n.kind == KIND_TERMINATE_REQUESTED;
        }
        hit
    }

    /// Receive the bootstrap: the four registers, plus the setup message when `arg0`
    /// marks one pending. A malformed setup message is fatal — a stage that cannot
    /// establish what it was asked to do must not guess.
    pub fn enter(mut start: Start) -> Stage {
        let endpoint = start.take_handle0::<Channel, Only>();
        let notif = start.notify.take();
        let Some(namespace) = start.root.take() else {
            log::write(b"coreutil: no namespace\n");
            libos::exit(EXIT_FAILURE);
        };
        let tier0 = |namespace, notif, endpoint| Stage {
            notif,
            namespace,
            // Tier 0: no setup message, so no `argv`, environment or working directory.
            streams: StageStreams::default(),
            argv: Vec::new(),
            cwd: None,
            terminal: None,
            env: libstream::wire::Record::default(),
            from_shell: false,
            _endpoint: endpoint,
        };
        if !setup_is_pending(start.arg0) {
            return tier0(namespace, notif, endpoint);
        }
        let setup = endpoint.as_ref().and_then(|ep| receive_setup(ep, Deadline::NEVER).ok().flatten());
        match setup {
            Some(s) => Stage {
                notif,
                namespace,
                streams: s.streams,
                argv: s.argv,
                cwd: cwd_of(&s.env),
                terminal: s.terminal,
                env: s.env,
                from_shell: true,
                _endpoint: endpoint,
            },
            None => {
                log::write(b"coreutil: malformed setup message\n");
                libos::exit(EXIT_FAILURE);
            }
        }
    }

    /// A port on `stdin`, if it has one.
    pub fn stdin(&self) -> Option<Port<'_>> {
        self.streams.stdin.as_ref().map(|h| Port::new(h.as_ref()))
    }

    /// A port on `stdout`, if it has one.
    pub fn stdout(&self) -> Option<Port<'_>> {
        self.streams.stdout.as_ref().map(|h| Port::new(h.as_ref()))
    }

    /// A port on `stderr`, if it has one.
    pub fn stderr(&self) -> Option<Port<'_>> {
        self.streams.stderr.as_ref().map(|h| Port::new(h.as_ref()))
    }

    /// Resolve a user-supplied path against this stage's working directory.
    ///
    /// Called **once, where a path enters from `argv`**, rather than threaded through every
    /// filesystem helper. Diverges with a message and `EXIT_USAGE` on failure.
    pub fn path(&self, operand: &[u8]) -> String {
        let mut buf = [0u8; 1024];
        match librsproto::path::resolve(self.cwd.as_deref().map(str::as_bytes), operand, &mut buf) {
            Ok(p) => String::from_utf8_lossy(p).into_owned(),
            Err(e) => {
                let mut msg = Vec::from(&b"path: "[..]);
                msg.extend_from_slice(e.message().as_bytes());
                msg.push(b'\n');
                self.die(&msg, EXIT_USAGE);
            }
        }
    }

    /// The program name (`argv[0]`), or `fallback` in Tier 0 where there is no `argv`.
    pub fn name<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.argv.first().map(|s| s.as_str()).unwrap_or(fallback)
    }

    /// Write `msg` to `stderr` as an **error**; [`Stage::warn`] and [`Stage::note`] say otherwise.
    ///
    /// `stderr` is a **shared diagnostic sink**, separate from the pipe, so a diagnostic never
    /// corrupts the typed stream on `stdout`. With none wired — Tier 0, or a shell that passed
    /// none — this falls back to the kernel log.
    pub fn diag(&self, msg: &[u8]) {
        self.diag_at(libstream::diag::Level::Error, msg);
    }

    /// Write `msg` to `stderr` as a **warning**: something the person may have wanted, and is not
    /// getting. The shell draws it in its own colour.
    pub fn warn(&self, msg: &[u8]) {
        self.diag_at(libstream::diag::Level::Warning, msg);
    }

    /// Write `msg` to `stderr` as a **notice**: progress, or a result said in words, drawn plain.
    pub fn note(&self, msg: &[u8]) {
        self.diag_at(libstream::diag::Level::Notice, msg);
    }

    /// Write `msg` to `stderr` at `level`, framed by `libstream::diag`; to the kernel log, the text
    /// alone.
    pub fn diag_at(&self, level: libstream::diag::Level, msg: &[u8]) {
        match self.stderr() {
            Some(mut port) => {
                if send_diag(&mut port, &libstream::diag::frame(level, msg)).is_err() {
                    log::write(msg);
                }
            }
            None => log::write(msg),
        }
    }

    /// Write `text` to `stdout` as text — a `TEXT_FALLBACK` stream, which the shell prints as lines,
    /// and `ChannelSink` splits however long it is. `false` if there is no `stdout`, or it failed.
    ///
    /// **Bytes that are not UTF-8 are shown replaced, not dropped**: an empty row in place of a
    /// body that did not decode would print a blank line and call it the answer.
    pub fn text_out(&self, text: &[u8]) -> bool {
        let text = String::from_utf8_lossy(text);
        let text: &str = &text;
        self.stdout().is_some_and(|port| {
            use libstream::channel::ChannelSink;
            let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(text).split('\n').collect();
            let mut sink = ChannelSink::new(port, libkern::abi::IPC_PAYLOAD_SIZE);
            libstream::table::write_text_fallback(&mut sink, &lines, 0).and_then(|()| sink.finish()).is_ok()
        })
    }

    /// **`--help` and `--version`: the text, as what was asked for**, and exit 0. It goes to
    /// `stdout` ([`Stage::text_out`]), so it can be piped and is not drawn as an error. With no
    /// `stdout`, it is a notice.
    pub fn answer(&self, text: &[u8]) -> ! {
        if !self.text_out(text) {
            self.note(text);
        }
        libos::exit(0)
    }

    /// Report a fatal error on `stderr` and exit with `code`.
    pub fn die(&self, msg: &[u8], code: i32) -> ! {
        self.diag(msg);
        libos::exit(code)
    }
}

fn send_diag(port: &mut Port<'_>, msg: &[u8]) -> libstream::wire::Result<()> {
    // One self-contained message: `stderr` is shared by every stage of a pipeline, so a partial
    // line must never be interleaved with another's. `last` stays false: the sink outlives a stage.
    port.send(msg, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use libkern::{Notification, Rights};
    use libos::mock;
    use libstream::Item;
    use libstream::channel::{ChannelReceiver, ChannelSink};
    use libstream::diag::{Level, frame};
    use libstream::setup::{bootstrap_arg0, send_stage_setup};
    use libstream::table::{TableReader, write_text_fallback};
    use libstream::wire::{Record, Schema, TypeModifiers, TypeTag, Value};

    type Pipe = (Handle<Channel, Only>, Handle<Channel, Only>);

    fn pipe() -> Pipe {
        Handle::<Channel, Only>::pair(4).unwrap()
    }

    fn start(queued: &[Notification], endpoint: u64, arg0: u64) -> Start {
        let root = mock::with_rights(mock::served("/x"), Rights::LOOKUP);
        // SAFETY: handles of the model's, each wrapped here and nowhere else.
        unsafe { Start::from_registers(mock::notify(queued), root, endpoint, arg0) }
    }

    fn note(kind: u32) -> Notification {
        let mut n = Notification::zeroed();
        n.kind = kind;
        n
    }

    fn pwd(dir: &str) -> Record {
        let schema = Schema::new().field("PWD", TypeTag::String, TypeModifiers::NONE);
        Record { schema, values: vec![Value::Str(String::from(dir))] }
    }

    /// A Tier-1 stage whose `stdout` and `stderr` the test holds the far ends of, with the
    /// bootstrap endpoint's raw handle. The far ends of the pairs are returned beside the shell's.
    fn shell_stage(queued: &[Notification]) -> (Stage, Pipe, Pipe, u64) {
        mock::reset();
        let (shell, endpoint) = pipe();
        let (out_far, out_near) = pipe();
        let (err_far, err_near) = pipe();
        let streams = StageStreams { stdout: Some(out_near), stderr: Some(err_near), ..StageStreams::default() };
        let (term, term_far) = pipe();
        send_stage_setup(&shell, streams, Some(term), &["list", "-r"], &pwd("/home/ann"), Deadline::NEVER).unwrap();
        let raw = endpoint.raw().0;
        let stage = Stage::enter(start(queued, endpoint.into_raw().0, bootstrap_arg0(true)));
        (stage, (shell, out_far), (err_far, term_far), raw)
    }

    #[test]
    fn a_shell_stage_takes_its_streams_arguments_and_directory_from_the_setup_message() {
        let (stage, ..) = shell_stage(&[]);
        assert!(stage.from_shell);
        assert_eq!(stage.argv, ["list", "-r"]);
        assert_eq!(stage.cwd.as_deref(), Some("/home/ann"));
        assert!(stage.streams.stdout.is_some() && stage.streams.stderr.is_some() && stage.streams.stdin.is_none());
        assert!(stage.terminal.is_some());
        assert_eq!(stage.path(b"a/b"), "/home/ann/a/b");
        assert_eq!(stage.path(b"/etc"), "/etc");
    }

    #[test]
    fn the_bootstrap_endpoint_stays_open_for_the_stage() {
        let (stage, _, _, endpoint) = shell_stage(&[]);
        assert!(mock::is_open(endpoint), "the setup channel was closed once its message was read");
        drop(stage);
        assert!(!mock::is_open(endpoint));
    }

    #[test]
    fn a_stage_without_a_setup_message_has_nothing_and_does_not_wait_for_one() {
        mock::reset();
        let (_shell, endpoint) = pipe();
        let raw = endpoint.raw().0;
        let stage = Stage::enter(start(&[], endpoint.into_raw().0, 0));
        assert!(!stage.from_shell);
        assert!(stage.argv.is_empty() && stage.cwd.is_none() && stage.terminal.is_none());
        assert!(stage.streams.stdin.is_none() && stage.streams.stdout.is_none() && stage.streams.stderr.is_none());
        assert!(mock::is_open(raw));
    }

    #[test]
    fn a_terminate_request_is_found_among_other_notifications_and_taken() {
        let (stage, ..) = shell_stage(&[note(7), note(KIND_TERMINATE_REQUESTED), note(7)]);
        assert!(stage.terminate_requested());
        assert!(!stage.terminate_requested(), "the queue was drained, so the request is not seen twice");
        let (stage, ..) = shell_stage(&[note(7)]);
        assert!(!stage.terminate_requested());
    }

    #[test]
    fn a_diagnostic_goes_to_stderr_framed_at_its_level() {
        let (stage, _, (err_far, _term_far), _) = shell_stage(&[]);
        stage.diag(b"bad\n");
        stage.warn(b"hmm\n");
        let mut got = Port::new(err_far.as_ref());
        let (mut first, mut second) = (Vec::new(), Vec::new());
        got.recv(&mut first).unwrap();
        got.recv(&mut second).unwrap();
        assert_eq!(first, frame(Level::Error, b"bad\n"));
        assert_eq!(second, frame(Level::Warning, b"hmm\n"));
        assert!(mock::logged().is_empty());
    }

    #[test]
    fn a_diagnostic_no_one_can_take_reaches_the_kernel_log_as_bare_text() {
        let (stage, _, (err_far, term_far), _) = shell_stage(&[]);
        drop((err_far, term_far));
        stage.diag(b"bad\n");
        assert_eq!(mock::logged(), b"bad\n");
        mock::reset();
        let stage = Stage::enter(start(&[], 0, 0));
        stage.note(b"said\n");
        assert_eq!(mock::logged(), b"said\n");
    }

    fn rows(far: &Handle<Channel, Only>) -> Vec<String> {
        let bytes = ChannelReceiver::new(Port::new(far.as_ref())).receive(1 << 16).unwrap();
        let mut table = TableReader::new(&bytes).unwrap();
        let mut out = Vec::new();
        while let Some(Ok(Item::Row(row))) = table.next() {
            out.push(String::from(row[0].as_str().unwrap()));
        }
        out
    }

    #[test]
    fn text_out_writes_each_line_as_a_row_and_replaces_what_is_not_utf8() {
        let (stage, (_, out_far), ..) = shell_stage(&[]);
        assert!(stage.text_out(b"a\nb\xff\n"));
        assert_eq!(rows(&out_far), ["a", "b\u{fffd}"]);
    }

    #[test]
    fn text_out_without_a_stdout_is_false() {
        mock::reset();
        let stage = Stage::enter(start(&[], 0, 0));
        assert!(!stage.text_out(b"a\n"));
    }

    #[test]
    fn a_port_marks_only_the_final_message_of_a_stream() {
        let (stage, (_, out_far), ..) = shell_stage(&[]);
        let mut sink = ChannelSink::new(stage.stdout().unwrap(), 4);
        write_text_fallback(&mut sink, &["abcdefgh"], 0).unwrap();
        sink.finish().unwrap();
        let (mut port, mut flags) = (Port::new(out_far.as_ref()), Vec::new());
        loop {
            let mut chunk = Vec::new();
            let last = port.recv(&mut chunk).unwrap();
            flags.push(last);
            if last {
                break;
            }
        }
        assert!(flags.len() > 1 && flags[..flags.len() - 1].iter().all(|l| !l) && flags[flags.len() - 1]);
    }
}
