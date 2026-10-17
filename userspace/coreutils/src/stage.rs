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

use alloc::string::String;
use alloc::vec::Vec;
use libkern::abi::{KIND_TERMINATE_REQUESTED, Notification};
use libkern::syscall::{SYS_NOTIF_RECV, syscall4};
use libkern::{exit, kprint};
use libstream::setup::Streams;

static mut NOTIF_BUF: Notification = Notification { kind: 0, body: [0; 60] };

/// The program did its job. (`i64` — the width `sys_process_exit` takes.)
pub const EXIT_OK: i64 = 0;
/// The program failed at its job (I/O error, missing file, …).
pub const EXIT_FAILURE: i64 = 1;
/// The program was invoked wrongly (bad flag, missing operand).
pub const EXIT_USAGE: i64 = 2;

/// Everything a coreutil receives at startup.
pub struct Stage {
    /// This process's notification channel.
    pub notif: u64,
    /// This process's root namespace — how a coreutil reaches the filesystem.
    pub namespace: u64,
    /// The standard streams. All three are `None` in Tier 0.
    pub streams: Streams,
    /// `argv`, with `argv[0]` the program name. Empty in Tier 0.
    pub argv: Vec<String>,
    /// The working directory from the environment's `PWD`; `None` in Tier 0 or with no `PWD`, and a
    /// relative path then fails rather than resolving against `/` — see [`Stage::path`].
    pub cwd: Option<String>,
    /// The terminal the shell handed this stage, if any: what a stage that has to *ask* something
    /// prompts on. `None` in Tier 0, under a spawner that passes none, and when the tty server
    /// had none to spare, so a stage that needs one must say so rather than assume it.
    pub terminal: Option<u64>,
    /// The whole environment the spawner passed — empty in Tier 0. A stage that hands it on
    /// unchanged (`with`) needs the record, not `cwd`.
    pub env: libstream::wire::Record,
    /// Whether a setup message was received (Tier 1).
    pub from_shell: bool,
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
    /// A **non-blocking** drain of this process's notification channel, looking for
    /// `TerminateRequested`. Nothing forces a stage to call it: there is no forcible kill
    /// in this system.
    ///
    /// A stage that blocks should put `notif` in its `sys_wait` set and call this when the
    /// wait returns, so the request takes effect promptly.
    pub fn terminate_requested(&self) -> bool {
        let mut hit = false;
        // SAFETY: `NOTIF_BUF` is a valid 64-byte out-param; single-threaded stage.
        unsafe {
            loop {
                let r = syscall4(SYS_NOTIF_RECV, self.notif, (&raw mut NOTIF_BUF) as u64, 0, 0);
                if r != 0 {
                    break; // WouldBlock: nothing queued
                }
                if (&raw const NOTIF_BUF.kind).read() == KIND_TERMINATE_REQUESTED {
                    hit = true;
                }
            }
        }
        hit
    }

    /// Receive the bootstrap: the four registers, plus the setup message when `arg0`
    /// marks one pending. A malformed setup message is fatal — a stage that cannot
    /// establish what it was asked to do must not guess.
    pub fn enter(notif: u64, namespace: u64, endpoint: u64, arg0: u64) -> Stage {
        let boot = libstream::setup::bootstrap(notif, namespace, endpoint, arg0);
        match boot.setup() {
            Some(Ok(s)) => Stage {
                notif,
                namespace,
                streams: s.streams,
                argv: s.argv,
                cwd: cwd_of(&s.env),
                terminal: s.terminal,
                env: s.env,
                from_shell: true,
            },
            Some(Err(_)) => {
                kprint(b"coreutil: malformed setup message\n");
                exit(EXIT_FAILURE);
            }
            // Tier 0: no setup message, so no `argv`, environment or working directory.
            None => Stage {
                notif,
                namespace,
                streams: Streams::default(),
                argv: Vec::new(),
                cwd: None,
                terminal: None,
                env: libstream::wire::Record::default(),
                from_shell: false,
            },
        }
    }

    /// Resolve a user-supplied path against this stage's working directory.
    ///
    /// Called **once, where a path enters from `argv`**, rather than threaded through every
    /// filesystem helper. Diverges with a message and `EXIT_USAGE` on failure.
    pub fn path(&self, operand: &[u8]) -> Vec<u8> {
        let mut buf = [0u8; 1024];
        match librsproto::path::resolve(self.cwd.as_deref().map(str::as_bytes), operand, &mut buf) {
            Ok(p) => p.to_vec(),
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

    /// Write `msg` to `stderr` at `level`, framed by `libstream::diag`; through `kprint`, the text
    /// alone.
    pub fn diag_at(&self, level: libstream::diag::Level, msg: &[u8]) {
        match self.streams.stderr {
            Some(h) => {
                let mut port = libstream::channel::IpcPort::new(h);
                if send_diag(&mut port, &libstream::diag::frame(level, msg)).is_err() {
                    kprint(msg);
                }
            }
            None => kprint(msg),
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
        self.streams.stdout.is_some_and(|h| {
            use libstream::channel::{ChannelSink, IpcPort};
            let lines: Vec<&str> = text.strip_suffix('\n').unwrap_or(text).split('\n').collect();
            let mut sink = ChannelSink::new(IpcPort::new(h), libkern::abi::IPC_PAYLOAD_SIZE);
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
        exit(0)
    }

    /// Report a fatal error on `stderr` and exit with `code`.
    pub fn die(&self, msg: &[u8], code: i64) -> ! {
        self.diag(msg);
        exit(code)
    }
}

fn send_diag(port: &mut libstream::channel::IpcPort, msg: &[u8]) -> libstream::wire::Result<()> {
    use libstream::channel::MsgPort;
    // One self-contained message: `stderr` is shared by every stage of a pipeline, so a partial
    // line must never be interleaved with another's. `last` stays false: the sink outlives a stage.
    port.send(msg, false)
}
