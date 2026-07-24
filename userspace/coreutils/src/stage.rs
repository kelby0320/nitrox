//! The **stage prologue** — what every coreutil does between `_start` and its own work.
//!
//! A coreutil is a *pipeline stage*: it is spawned by a shell, receives its streams and
//! `argv` in the setup message (`docs/spec/pipeline-stdio.md`), does its job, and exits
//! with a status. That shape is identical across programs, so it lives here once.
//!
//! ## Both tiers, deliberately
//!
//! Tier 1 (a setup message is pending) is the shell-spawned case and supplies `argv`.
//! Tier 0 — no descriptor, no message — is what a program gets when something spawns it
//! *without* a shell, which today is init and the test harness. A Tier-0 stage has no
//! `argv` and no streams, so it can only run its argument-free default. Handling both
//! means a coreutil is directly spawnable before the shell exists, which is exactly the
//! position Milestone 1 is in.
//!
//! ## Exit statuses
//!
//! [`EXIT_OK`] / [`EXIT_FAILURE`] / [`EXIT_USAGE`], the conventional split. The shell
//! surfaces these as a stage's `exit_status` in `PipelineStatus` (design §1).

use alloc::string::String;
use alloc::vec::Vec;
use libkern::{exit, kprint};
use libstream::setup::Streams;

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
    /// Whether a setup message was received (Tier 1).
    pub from_shell: bool,
}

impl Stage {
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
                from_shell: true,
            },
            Some(Err(_)) => {
                kprint(b"coreutil: malformed setup message\n");
                exit(EXIT_FAILURE);
            }
            None => Stage {
                notif,
                namespace,
                streams: Streams::default(),
                argv: Vec::new(),
                from_shell: false,
            },
        }
    }

    /// The program name (`argv[0]`), or `fallback` in Tier 0 where there is no `argv`.
    pub fn name<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.argv.first().map(|s| s.as_str()).unwrap_or(fallback)
    }

    /// Write `msg` to `stderr`.
    ///
    /// `stderr` is a **shared diagnostic sink**, separate from the pipe (design §1), so a
    /// diagnostic never corrupts the typed stream on `stdout`. Until a stage actually has
    /// one wired — Tier 0, or a shell that passed none — this falls back to the kernel
    /// log, which is where such output would otherwise vanish.
    pub fn diag(&self, msg: &[u8]) {
        match self.streams.stderr {
            Some(h) => {
                let mut port = libstream::channel::IpcPort::new(h);
                if send_diag(&mut port, msg).is_err() {
                    kprint(msg);
                }
            }
            None => kprint(msg),
        }
    }

    /// Report a fatal error on `stderr` and exit with `code`. `-> !`, so a caller can use
    /// it in any position without contorting its control flow.
    pub fn die(&self, msg: &[u8], code: i64) -> ! {
        self.diag(msg);
        exit(code)
    }
}

/// Send one diagnostic line as a framed message on the shared `stderr` sink.
fn send_diag(port: &mut libstream::channel::IpcPort, msg: &[u8]) -> libstream::wire::Result<()> {
    use libstream::channel::MsgPort;
    // Each diagnostic is one self-contained message: `stderr` is shared between every
    // stage of a pipeline, so a stage must never leave a partial line interleaved with
    // another's. `last` stays false — the sink outlives any one stage.
    port.send(msg, false)
}
