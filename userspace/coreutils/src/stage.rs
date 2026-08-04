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
use libkern::abi::{KIND_TERMINATE_REQUESTED, Notification};
use libkern::syscall::{SYS_NOTIF_RECV, syscall4};
use libkern::{exit, kprint};
use libstream::setup::Streams;

/// Out-param for [`Stage::terminate_requested`]'s drain.
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
    /// The working directory from the environment's `PWD`, if it has one.
    ///
    /// `None` in Tier 0, and in Tier 1 when the spawner passed no `PWD`. A relative path
    /// then fails rather than resolving against `/` — see [`Stage::path`].
    pub cwd: Option<String>,
    /// Whether a setup message was received (Tier 1).
    pub from_shell: bool,
}

/// Pull `PWD` out of an environment record.
///
/// `PWD` is a *conventional* entry (Milestone 3.5), not a distinct field: there is no
/// second source of truth for it to disagree with, unlike Unix's `$PWD` versus `getcwd()`.
fn cwd_of(env: &libstream::wire::Record) -> Option<String> {
    let i = env.schema.fields.iter().position(|f| f.name == "PWD")?;
    env.values.get(i)?.as_str().map(String::from)
}

impl Stage {
    /// Receive the bootstrap: the four registers, plus the setup message when `arg0`
    /// marks one pending. A malformed setup message is fatal — a stage that cannot
    /// establish what it was asked to do must not guess.
    /// Has someone asked this stage to exit? (§11h)
    ///
    /// A **non-blocking** drain of this process's notification channel, looking for
    /// `TerminateRequested`. Nothing forces a stage to call it and nothing happens if it
    /// does not — there is no forcible kill in this system, so "well-behaved" is a
    /// property a program has by asking, and this is the asking.
    ///
    /// The place it belongs is a wait: a stage that blocks should put `notif` in its
    /// `sys_wait` set and call this when the wait returns, which is what makes the request
    /// take effect promptly rather than at the end of a job that was going to finish
    /// anyway.
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

    pub fn enter(notif: u64, namespace: u64, endpoint: u64, arg0: u64) -> Stage {
        let boot = libstream::setup::bootstrap(notif, namespace, endpoint, arg0);
        match boot.setup() {
            Some(Ok(s)) => Stage {
                notif,
                namespace,
                streams: s.streams,
                argv: s.argv,
                cwd: cwd_of(&s.env),
                from_shell: true,
            },
            Some(Err(_)) => {
                kprint(b"coreutil: malformed setup message\n");
                exit(EXIT_FAILURE);
            }
            // Tier 0: no setup message, so no `argv` and — for the same reason — no
            // environment and no working directory. A relative path then fails, which is
            // the honest outcome: this process was handed nothing to resolve against.
            None => Stage {
                notif,
                namespace,
                streams: Streams::default(),
                argv: Vec::new(),
                cwd: None,
                from_shell: false,
            },
        }
    }

    /// Resolve a user-supplied path against this stage's working directory.
    ///
    /// Called **once, where a path enters from `argv`** — the boundary — rather than
    /// threaded through every filesystem helper. That keeps one place per program where a
    /// relative path becomes absolute, and it is the same place a bad path should be
    /// reported.
    ///
    /// Diverges with a message on failure, since a path the caller typed that cannot be
    /// resolved is a usage error, not something to carry on past.
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
