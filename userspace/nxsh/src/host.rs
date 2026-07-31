//! The `Host` seam — everything the language does that touches the operating system.
//!
//! Milestone 3 Part C, and the structural bet the plan named: spawning a stage, wiring
//! pipes, reading a script, writing diagnostics all sit behind this trait, exactly as the
//! ext4 parser sits behind `BlockReader`.
//!
//! It is worth more here than there. An interpreter is mostly pure logic, and the parts
//! that are *not* — pipeline ordering, per-stage status, what happens when a stage
//! crashes — are precisely the parts hardest to provoke on real hardware and easiest to
//! get subtly wrong. [`MockHost`] makes all of them ordinary host tests.
//!
//! ## Where the boundary is drawn
//!
//! [`Host::run`] takes a **run of consecutive external stages**, not one stage. That is
//! deliberate: §1 says each stage is a concurrently-running process streaming to the next
//! through a bounded IPC channel, so the pipes between them are the kernel's business and
//! must not be serialised through the shell. The shell hands over a chain and gets back
//! per-stage status plus whatever the last stage produced.
//!
//! Only the *final* output is captured into memory. Backpressure between stages is real —
//! bounded channels, a slow consumer blocking its producer — and this interface preserves
//! that by not touching the middle.
//!
//! ## A finding about the standard coreutils
//!
//! No coreutil reads `stdin`. That is not an oversight: §10a dissolved every classic
//! *filter* — `grep`, `head`, `uniq`, `cat`, `find` — into generic operators, which run
//! in-process (§5c). What remains external is sources (`list`, `date`, `whoami`) and
//! mutations (`copy`, `move`, …).
//!
//! So with the standard set, a pipeline has **at most one external stage, always at the
//! head**. §5c's "most of a typical pipeline costs exactly one process boundary" is
//! stronger than it sounds: for the shipped programs it is *exactly* one. The N-stage
//! machinery below is still right — a user's own programs chain freely — but the standard
//! set cannot exercise it, which is why the in-guest demo borrows a pass-through role from
//! `test-stage` and the ordering semantics are tested here against a mock.

use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

/// One external stage: a program and its argument vector.
#[derive(Clone, PartialEq, Debug)]
pub struct StageSpec {
    /// The command as written, used for resolution and for the status report.
    pub program: String,
    /// `argv`, including `argv[0]`.
    pub argv: Vec<String>,
}

/// What one stage did — §1's `StageStatus`, verbatim.
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
        StageStatus {
            command: command.into(),
            exit_status: 0,
            crashed: false,
            cancelled: false,
        }
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
    /// `strict` selects §1's eager-abort behaviour: on the first stage failure the shell
    /// terminates the remaining stages rather than letting them drain. It needs no new
    /// mechanism — the shell holds a process handle for everything it spawned, so
    /// "abort the rest" is an ordinary capability-mediated call on handles it already
    /// owns, not signal delivery.
    ///
    /// An `Err` is a failure to *run* the pipeline at all (a program that does not
    /// resolve, a pipe that cannot be created). A stage that ran and failed is a
    /// successful `Ok` with a non-zero [`StageStatus`] — the distinction matters, because
    /// only the first is the shell's own fault.
    fn run(&mut self, stages: &[StageSpec], input: Option<&[u8]>, strict: bool)
    -> Result<PipelineRun, String>;

    /// Read a file — a script, or `open`'s operand.
    fn read_file(&mut self, path: &str) -> Result<Vec<u8>, String>;

    /// Write a file, for `save`.
    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String>;

    /// Diagnostics — §1's second error category. Bypasses the pipe entirely and goes to
    /// the shell's own `stderr`, which is why it is a separate method and not something
    /// threaded through a pipeline's value.
    fn diag(&mut self, text: &str);

    /// Ordinary output: what `display` and the REPL's auto-display write to.
    fn out(&mut self, text: &str);
}

/// A host that cannot do anything, for evaluating pure expressions.
///
/// Used by [`crate::Interp::new`] so Part B's arithmetic still evaluates without an OS
/// underneath it. Every method fails or discards rather than pretending.
#[derive(Default)]
pub struct NullHost;

impl Host for NullHost {
    fn run(
        &mut self,
        _stages: &[StageSpec],
        _input: Option<&[u8]>,
        _strict: bool,
    ) -> Result<PipelineRun, String> {
        Err(String::from(
            "this interpreter has no host attached, so it cannot run a program",
        ))
    }

    fn read_file(&mut self, _path: &str) -> Result<Vec<u8>, String> {
        Err(String::from("this interpreter has no host attached"))
    }

    fn write_file(&mut self, _path: &str, _bytes: &[u8]) -> Result<(), String> {
        Err(String::from("this interpreter has no host attached"))
    }

    fn diag(&mut self, _text: &str) {}

    fn out(&mut self, _text: &str) {}
}

/// A scripted host for tests: canned stage results, and a record of what was asked for.
///
/// This is what makes pipeline *semantics* testable without a kernel — stage ordering,
/// status collection, the strict-abort rule, a crashed stage, an empty output. Every one
/// of those is awkward to provoke on real hardware and trivial here.
pub struct MockHost {
    /// Programs the mock knows: name → (exit status, stdout bytes).
    pub programs: Vec<(String, i32, Option<Vec<u8>>)>,
    /// Programs that crash rather than exit.
    pub crashing: Vec<String>,
    /// Files the mock can read.
    pub files: Vec<(String, Vec<u8>)>,
    /// What the host was asked to do, shared so a test can still read it after the
    /// interpreter has taken ownership of the host.
    log: Rc<RefCell<MockLog>>,
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
            log: Rc::new(RefCell::new(MockLog::default())),
        }
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

    fn lookup(&self, name: &str) -> Option<(i32, Option<Vec<u8>>)> {
        self.programs
            .iter()
            .find(|(n, _, _)| n == name)
            .map(|(_, s, o)| (*s, o.clone()))
    }
}

impl Host for MockHost {
    fn run(
        &mut self,
        stages: &[StageSpec],
        input: Option<&[u8]>,
        strict: bool,
    ) -> Result<PipelineRun, String> {
        {
            let mut log = self.log.borrow_mut();
            log.runs.push(stages.to_vec());
            log.inputs.push(input.map(|b| b.to_vec()));
        }

        let mut statuses = Vec::new();
        let mut output = None;
        let mut aborted = false;
        for (i, s) in stages.iter().enumerate() {
            if aborted {
                // §1: `strict` terminates the *remaining* stages. They ran no code, so
                // they are `cancelled`, not failed — a distinction a report should keep.
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

    fn diag(&mut self, text: &str) {
        self.log.borrow_mut().diagnostics.push(text.to_string());
    }

    fn out(&mut self, text: &str) {
        self.log.borrow_mut().output.push(text.to_string());
    }
}
