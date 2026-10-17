//! The `Host` seam: everything the language does that touches the operating system, so pipeline
//! ordering, per-stage status and a crashed stage become ordinary tests against [`MockHost`].
//!
//! [`Host::run`] takes a run of consecutive external stages, not one: the stages are concurrent
//! processes streaming through bounded IPC channels, which are the kernel's business and not
//! serialised through the shell. Only the final output is captured. A standard pipeline has at most
//! one external stage (§10a made every filter a generic operator); the N-stage machinery is for a
//! user's own programs and is exercised against the mock.

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;
use libstream::wire::Record;

/// One external stage: a program and its argument vector.
#[derive(Clone, PartialEq, Debug)]
pub struct StageSpec {
    /// The command as written, used for resolution and for the status report.
    pub program: String,
    /// `argv`, including `argv[0]`.
    pub argv: Vec<String>,
}

/// What one stage did (§1's `StageStatus`).
#[derive(Clone, PartialEq, Debug)]
pub struct StageStatus {
    pub command: String,
    pub exit_status: i32,
    /// No clean terminator was ever sent: the process died unexpectedly.
    pub crashed: bool,
    /// The shell terminated this stage itself — a `strict` abort (§1).
    pub cancelled: bool,
}

impl StageStatus {
    pub fn ok(command: impl Into<String>) -> StageStatus {
        StageStatus { command: command.into(), exit_status: 0, crashed: false, cancelled: false }
    }

    /// Whether this stage counts as having succeeded.
    pub fn succeeded(&self) -> bool {
        self.exit_status == 0 && !self.crashed && !self.cancelled
    }
}

/// The result of running one chain of external stages.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct PipelineRun {
    /// One entry per stage, in pipeline order.
    pub stages: Vec<StageStatus>,
    /// Raw TSM1 bytes from the last stage's stdout, if it produced any.
    pub output: Option<Vec<u8>>,
}

/// Everything the evaluator needs from the operating system.
pub trait Host {
    /// Run `stages` as one connected pipeline, feeding `input` to the first stage's
    /// `stdin` and capturing the last stage's `stdout`.
    ///
    /// `strict` selects §1's eager-abort: on the first stage failure the shell terminates the
    /// remaining stages through the process handles it already holds.
    ///
    /// An `Err` is a failure to run the pipeline at all (a program that does not resolve, a
    /// pipe that cannot be created). A stage that ran and failed is an `Ok` with a non-zero
    /// [`StageStatus`]: only the first is the shell's own fault.
    ///
    /// `env` goes to every stage unchanged. The shell does not pre-resolve a stage's paths: it
    /// passes the arguments as written and the same `PWD`, so both sides resolve identically.
    fn run(
        &mut self,
        stages: &[StageSpec],
        input: Option<&[u8]>,
        strict: bool,
        env: &Record,
    ) -> Result<PipelineRun, String>;

    /// Read a file — a script, or `open`'s operand.
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String>;

    /// Write a file, for `save`.
    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String>;

    /// Whether `path` resolves. Used by `cd` before it commits to a new `PWD`.
    fn exists(&mut self, path: &str) -> bool;

    /// Diagnostics (§1's second error category): bypass the pipe and go to the shell's own
    /// `stderr`.
    fn diag(&mut self, text: &str);

    /// Ordinary output: what `display` and the REPL's auto-display write to.
    fn out(&mut self, text: &str);

    /// Whether what this host writes goes to a terminal that reads SGR. A script or a Tier-0 stage
    /// has none, and an escape in its output is corruption. Defaults to `false`.
    fn styled(&mut self) -> bool {
        false
    }

    /// The external programs this shell could run by name, for completion (§11c). Best-effort and
    /// never an error: a failed listing is a Tab that does less.
    fn commands(&mut self) -> Vec<String> {
        Vec::new()
    }

    /// What is directly under `dir` (absolute), each name with whether it is a directory.
    /// Best-effort like [`commands`](Self::commands); the real host calls `libfs::list_dir`, the
    /// filesystem-and-namespace union `list` shows.
    fn list_dir(&mut self, dir: &str) -> Vec<(String, bool)> {
        let _ = dir;
        Vec::new()
    }

    /// Has the terminal asked this evaluation to stop? (§11h)
    ///
    /// Called at statement boundaries and between loop iterations, so it must be cheap: the real
    /// implementation is a non-blocking receive on the tty channel.
    ///
    /// Edge-triggered: reported once and consumed. A level-triggered answer would interrupt a
    /// `catch` block's recovery as fast as it started, so `try { … } catch { … }` would not answer
    /// `Ctrl-C`.
    fn interrupted(&mut self) -> bool {
        false
    }

    /// Nanoseconds on the kernel's monotonic clock, for `time`. `None` from a host with no clock.
    fn now_ns(&mut self) -> Option<u64> {
        None
    }
}

/// A host that cannot do anything, for evaluating pure expressions: every method fails or
/// discards rather than pretending.
#[derive(Default)]
pub struct NullHost;

impl Host for NullHost {
    fn run(
        &mut self,
        _stages: &[StageSpec],
        _input: Option<&[u8]>,
        _strict: bool,
        _env: &Record,
    ) -> Result<PipelineRun, String> {
        Err(String::from("this interpreter has no host attached, so it cannot run a program"))
    }

    fn read_file(&mut self, _path: &str) -> Result<Vec<u8>, String> {
        Err(String::from("this interpreter has no host attached"))
    }

    fn write_file(&mut self, _path: &str, _bytes: &[u8]) -> Result<(), String> {
        Err(String::from("this interpreter has no host attached"))
    }

    fn exists(&mut self, _path: &str) -> bool {
        false
    }

    fn diag(&mut self, _text: &str) {}

    fn out(&mut self, _text: &str) {}
}

/// A scripted host for tests: canned stage results, and a record of what was asked for.
pub struct MockHost {
    /// Programs the mock knows: name → (exit status, stdout bytes).
    pub programs: Vec<(String, i32, Option<Vec<u8>>)>,
    /// Programs that crash rather than exit.
    pub crashing: Vec<String>,
    /// Files the mock can read.
    pub files: Vec<(String, Vec<u8>)>,
    /// Directories the mock reports as existing, for `cd`.
    pub dirs: Vec<String>,
    /// What each directory contains: absolute directory → (name, is a directory).
    ///
    /// Separate from [`dirs`](Self::dirs): a directory can exist and be empty.
    pub entries: Vec<(String, Vec<(String, bool)>)>,
    /// External program names, for completion.
    pub commands: Vec<String>,
    /// What the host was asked to do, shared so a test can read it after the interpreter owns the
    /// host.
    log: Rc<RefCell<MockLog>>,
    /// Report an interrupt once this many checkpoints have gone by (§11h).
    interrupt_after: Option<u32>,
    /// A clock for `time`: what it reads now, and how far each read moves it. `None`, no clock.
    clock_ns: Option<(u64, u64)>,
}

/// What a [`MockHost`] was asked to do.
#[derive(Default, Debug)]
pub struct MockLog {
    /// Every chain passed to [`Host::run`], in order.
    pub runs: Vec<Vec<StageSpec>>,
    /// Input handed to the first stage of each run.
    pub inputs: Vec<Option<Vec<u8>>>,
    /// Everything written to `stderr`.
    pub diagnostics: Vec<String>,
    /// Everything written to `stdout`.
    pub output: Vec<String>,
    /// The environment handed to each run.
    pub envs: Vec<Record>,
}

impl Default for MockHost {
    fn default() -> Self {
        MockHost::new()
    }
}

impl MockHost {
    pub fn new() -> MockHost {
        MockHost {
            programs: Vec::new(),
            crashing: Vec::new(),
            files: Vec::new(),
            dirs: Vec::new(),
            entries: Vec::new(),
            commands: Vec::new(),
            log: Rc::new(RefCell::new(MockLog::default())),
            interrupt_after: None,
            clock_ns: None,
        }
    }

    /// A clock that starts at nothing and moves on `step_ns` each time it is read, for `time`.
    pub fn with_clock(mut self, step_ns: u64) -> MockHost {
        self.clock_ns = Some((0, step_ns));
        self
    }

    /// Interrupt the evaluation once `n` checkpoints have passed (§11h).
    pub fn with_interrupt_after(mut self, n: u32) -> MockHost {
        self.interrupt_after = Some(n);
        self
    }

    /// A handle to the log, kept by the test while the interpreter owns the host.
    pub fn log(&self) -> Rc<RefCell<MockLog>> {
        Rc::clone(&self.log)
    }

    /// Declare a program that exits 0 and produces `out`.
    pub fn with_program(mut self, name: &str, out: Option<Vec<u8>>) -> MockHost {
        self.programs.push((String::from(name), 0, out));
        self
    }

    /// Declare a program that exits with `status`.
    pub fn with_failing(mut self, name: &str, status: i32) -> MockHost {
        self.programs.push((String::from(name), status, None));
        self
    }

    /// Declare a program that dies without a clean terminator.
    pub fn with_crashing(mut self, name: &str) -> MockHost {
        self.crashing.push(String::from(name));
        self
    }

    pub fn with_file(mut self, path: &str, body: &str) -> MockHost {
        self.files.push((String::from(path), body.as_bytes().to_vec()));
        self
    }

    pub fn with_dir(mut self, path: &str) -> MockHost {
        self.dirs.push(String::from(path));
        self
    }

    fn lookup(&self, name: &str) -> Option<(i32, Option<Vec<u8>>)> {
        self.programs.iter().find(|(n, _, _)| n == name).map(|(_, s, o)| (*s, o.clone()))
    }
}

impl Host for MockHost {
    fn commands(&mut self) -> Vec<String> {
        self.commands.clone()
    }

    fn list_dir(&mut self, dir: &str) -> Vec<(String, bool)> {
        self.entries.iter().find(|(d, _)| d == dir).map(|(_, e)| e.clone()).unwrap_or_default()
    }

    fn interrupted(&mut self) -> bool {
        match &mut self.interrupt_after {
            Some(0) => {
                self.interrupt_after = None;
                true
            }
            Some(n) => {
                *n -= 1;
                false
            }
            None => false,
        }
    }

    fn now_ns(&mut self) -> Option<u64> {
        let (now, step) = self.clock_ns?;
        self.clock_ns = Some((now + step, step));
        Some(now)
    }

    fn run(
        &mut self,
        stages: &[StageSpec],
        input: Option<&[u8]>,
        strict: bool,
        env: &Record,
    ) -> Result<PipelineRun, String> {
        {
            let mut log = self.log.borrow_mut();
            log.runs.push(stages.to_vec());
            log.inputs.push(input.map(|b| b.to_vec()));
            log.envs.push(env.clone());
        }

        let mut statuses = Vec::new();
        let mut output = None;
        let mut aborted = false;
        for (i, s) in stages.iter().enumerate() {
            if aborted {
                // §1: `strict` terminates the remaining stages; they ran no code, so they are
                // `cancelled`, not failed.
                statuses.push(StageStatus {
                    command: s.program.clone(),
                    exit_status: 0,
                    crashed: false,
                    cancelled: true,
                });
                continue;
            }
            if self.crashing.contains(&s.program) {
                statuses.push(StageStatus {
                    command: s.program.clone(),
                    exit_status: -1,
                    crashed: true,
                    cancelled: false,
                });
                if strict {
                    aborted = true;
                }
                continue;
            }
            let Some((status, out)) = self.lookup(&s.program) else {
                return Err(alloc::format!("`{}` is not a program", s.program));
            };
            if i + 1 == stages.len() {
                output = out;
            }
            statuses.push(StageStatus {
                command: s.program.clone(),
                exit_status: status,
                crashed: false,
                cancelled: false,
            });
            if strict && status != 0 {
                aborted = true;
            }
        }
        Ok(PipelineRun { stages: statuses, output })
    }

    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String> {
        self.files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| alloc::format!("no such file: {path}"))
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.files.push((String::from(path), bytes.to_vec()));
        Ok(())
    }

    fn exists(&mut self, path: &str) -> bool {
        self.files.iter().any(|(p, _)| p == path) || self.dirs.iter().any(|d| d == path)
    }

    fn diag(&mut self, text: &str) {
        self.log.borrow_mut().diagnostics.push(text.to_string());
    }

    fn out(&mut self, text: &str) {
        self.log.borrow_mut().output.push(text.to_string());
    }
}
