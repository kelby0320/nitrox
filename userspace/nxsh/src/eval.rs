//! The evaluator core — §9's statements and §8's operators, in process.
//!
//! Everything here runs on the `Value` tree inside the shell: no spawning, no IPC, no syscalls.
//! That is the design's claim (§5c): most of a typical pipeline never crosses a process boundary,
//! so the in-process evaluator is the common path.
//!
//! ## Two decisions worth stating
//!
//! **There is no truthiness.** `if 1 { … }` is an error. §6 makes mismatch an error and §1 makes
//! the default fail-loud; coercing an `Int` to a condition is the silent reinterpretation both
//! rules exist to prevent. The cost is a keystroke (`if n != 0`).
//!
//! **Arithmetic overflow and division by zero are errors.** Not wrapping, not `inf`: a shell that
//! printed `-9223372036854775808` from an addition would be reporting a fabricated number, like
//! `date` printing 1970 when the clock is unset.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use libstream::wire::{Record, Schema, TypeModifiers, Value};

use crate::ast::*;
use crate::host::{Host, PipelineRun, StageSpec, StageStatus};
use crate::ops;
use crate::value::{Func, Val, render_i64};

/// A runtime failure. The message is built for a person: it names the operation and the types
/// involved (§6's "schema diff, not a vague mismatch").
///
/// `kind` is §2's vocabulary — small and closed at any moment, and extensible because §6's subset
/// match lets a `catch` that reads only `message` survive a new kind.
#[derive(Clone, PartialEq, Debug)]
pub struct EvalError {
    pub message: String,
    pub kind: String,
    /// The §1 per-stage report, carried only by a [`PIPELINE_FAILED`].
    ///
    /// A pipeline's value never reaches the binding on failure, so the report rides on the error,
    /// the one thing that reaches the `catch` (§2).
    pub stages: Option<Value>,
    /// `exit N` (§11f) travelling out — **not a failure**, despite the channel.
    ///
    /// The error path is the only one already crossing every boundary: a loop, a `def`, a closure,
    /// a `match` arm. `catch` re-raises it rather than handling it, so a script cannot prevent
    /// leaving the shell by accident.
    pub exit: Option<i32>,
}

/// §2's `kind` vocabulary.
pub const ERROR: &str = "Error";
pub const TYPE_ERROR: &str = "TypeError";
pub const PARSE_ERROR: &str = "ParseError";
pub const ASSERTION_FAILED: &str = "AssertionFailed";
pub const PIPELINE_FAILED: &str = "PipelineFailed";
/// §11h. Catchable on purpose, so a script can clean up after `Ctrl-C`. `exit` is not: leaving is a
/// decision, being interrupted is an event.
pub const INTERRUPTED: &str = "Interrupted";

impl EvalError {
    pub(crate) fn new(message: impl Into<String>) -> EvalError {
        EvalError::of(ERROR, message)
    }

    fn of(kind: &str, message: impl Into<String>) -> EvalError {
        EvalError { message: message.into(), kind: String::from(kind), stages: None, exit: None }
    }

    /// `exit N` — see the field.
    fn exiting(status: i32) -> EvalError {
        EvalError { message: String::from("exit"), kind: String::from(ERROR), stages: None, exit: Some(status) }
    }

    pub fn is_exit(&self) -> bool {
        self.exit.is_some()
    }

    /// The error as a `Record`, which is what `catch (e)` binds (§2).
    ///
    /// An ordinary record so §9f's patterns take it apart with no new grammar, built here once so
    /// the vocabulary cannot drift between the sites that raise it.
    fn to_record(&self) -> Val {
        let mut schema = Schema::new().field("kind", libstream::wire::TypeTag::String, TypeModifiers::NONE).field(
            "message",
            libstream::wire::TypeTag::String,
            TypeModifiers::NONE,
        );
        let mut values = alloc::vec![Value::Str(self.kind.clone()), Value::Str(self.message.clone())];
        if let Some(stages) = &self.stages {
            schema = schema.field("stages", libstream::wire::TypeTag::List, TypeModifiers::NONE);
            values.push(stages.clone());
        }
        Val::Data(Value::Record(Arc::new(Record { schema, values })))
    }
}

type Result<T> = core::result::Result<T, EvalError>;

/// How a statement finished. `return` unwinds to the enclosing function (§5b) without
/// being an error, so it needs its own channel out of the statement walker.
enum Flow {
    Normal(Val),
    Return(Val),
    /// `break` / `continue` (§9c), travelling out to the nearest enclosing loop.
    /// `break` / `continue` (§9c), travelling out to the nearest enclosing loop.
    ///
    /// **Every `match` on `Flow` can swallow one of these**, so none uses a wildcard arm:
    /// exhaustiveness makes the compiler ask at each site.
    Break,
    Continue,
}

/// One binding.
struct Slot {
    val: Val,
    /// `let`/`const` are immutable; only `mut` may be assigned (§9d).
    mutable: bool,
    /// `const` may not be shadowed by an ordinary rebinding in the same scope.
    constant: bool,
}

/// Whether a bare top-level expression is displayed (§11e).
///
/// The two modes differ deliberately: a REPL appends `| display` to an unassigned pipeline, while a
/// script discards it, or a script of `remove`/`move` calls would flood its output. The difference
/// lives at the driver, not in the statement.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Bare expression values are discarded.
    Script,
    /// Bare expression values are displayed.
    Repl,
}

/// The interpreter's state.
pub struct Interp {
    /// A stack of scopes, innermost last. A `Vec` because a scope holds a handful of names, where
    /// linear scan beats hashing, and shadowing order stays explicit.
    scopes: Vec<Vec<(String, Slot)>>,
    /// The environment: a TSM1 `Record`, handed to every stage this shell spawns.
    ///
    /// Also bound as `$env`, so it is an ordinary value: `$env.PWD` and `env.EDITOR = "vi"` need
    /// nothing beyond field access (§6).
    env: Record,
    /// Hoisted `def` bindings, visible from inside any call.
    ///
    /// §5a: a named function captures nothing, but it must still be able to **call** other
    /// functions, or mutual recursion is impossible. So a `def` goes here and is always reachable,
    /// while a `let` stays in `scopes` and is not.
    functions: Vec<(String, Val)>,
    host: alloc::boxed::Box<dyn Host>,
    mode: Mode,
    /// Whether the statement currently executing is inside a `strict { }` block (§1).
    strict: bool,
    /// The most recent pipeline's `PipelineStatus`, for `$last.status` (§11d).
    ///
    /// A private field, not a `__status` binding: that was a name a user could type and was dropped
    /// on the failure path. The report rides on the error (§2); this is only the REPL's convenience
    /// copy.
    last_status: Option<Val>,
    /// The runaway backstop, [`MAX_ITERATIONS`] in every build.
    ///
    /// A field so the guard is **testable in microseconds**; proving it at ten million iterations
    /// would cost more than the whole suite. Not a resource policy.
    iteration_limit: u64,
    /// Absolute paths of the files being loaded, outermost first, so a `use` cannot recurse.
    pub(crate) loading: Vec<String>,
}

impl Default for Interp {
    fn default() -> Self {
        Interp::new()
    }
}

impl Interp {
    pub fn new() -> Interp {
        Interp::with_host(alloc::boxed::Box::new(crate::host::NullHost), Mode::Script)
    }

    pub fn with_host(host: alloc::boxed::Box<dyn Host>, mode: Mode) -> Interp {
        Interp {
            scopes: alloc::vec![Vec::new()],
            env: Record::default(),
            functions: Vec::new(),
            host,
            mode,
            strict: false,
            last_status: None,
            iteration_limit: MAX_ITERATIONS,
            loading: Vec::new(),
        }
    }

    /// Install the environment this shell was started with, and bind `$env`.
    pub fn set_env(&mut self, env: Record) {
        self.env = env;
        self.rebind_env();
    }

    /// The working directory, from the environment's conventional `PWD` entry.
    ///
    /// A *conventional* entry, not a distinct field: Unix has two sources of truth, `$PWD` and
    /// `getcwd()`, and the bugs live in the gap. There is no kernel cwd here to disagree.
    pub fn cwd(&self) -> Option<&str> {
        crate::paths::pwd_of(&self.env)
    }

    /// Resolve a path the *shell itself* will use, against `PWD`.
    ///
    /// Only for the shell's own lookups (`open`, `save`, a script path). A spawned stage's
    /// arguments are passed through as written; see [`Host::run`].
    fn resolve_path(&self, path: &str) -> Result<alloc::string::String> {
        crate::paths::resolve(self.cwd(), path).map_err(EvalError::new)
    }

    /// Re-publish `$env` after a change, so the binding and the record never disagree.
    fn rebind_env(&mut self) {
        let v = Val::Data(Value::Record(Arc::new(self.env.clone())));
        if let Some(slot) = self.lookup_mut("$env") {
            slot.val = v;
            return;
        }
        let _ = self.bind("$env", v, true, false);
    }

    /// Set a conventional environment entry, replacing it if present.
    fn set_env_entry(&mut self, name: &str, value: Value) {
        match self.env.schema.fields.iter().position(|f| f.name == name) {
            Some(i) => self.env.values[i] = value,
            None => {
                let tag = value.type_tag().unwrap_or(libstream::wire::TypeTag::Null);
                self.env.schema = self.env.schema.clone().field(name, tag, TypeModifiers::NONE);
                self.env.values.push(value);
            }
        }
        self.rebind_env();
    }

    /// What a Tab press at the end of `line` should offer (§11c).
    ///
    /// The gathering half of completion; the deciding half is [`crate::complete`]. It lives here
    /// because the answer needs the `def`s typed so far and a `Host` to list a directory.
    ///
    /// **A word containing a `/` is a path wherever it appears**, including at the head of a stage:
    /// `./script.nx` and `/bin/list` are commands written as paths, as in §9h's `use`.
    pub fn complete(&mut self, line: &str) -> crate::complete::Completion {
        use crate::complete::{Completion, Where, matching_names, matching_paths, split_path, word_at_end};
        let w = word_at_end(line);
        let as_path = w.at == Where::Argument || w.text.contains('/');
        if !as_path {
            // §3's four categories plus the person's `def`s. Keywords that are not words, like `_`,
            // are skipped: offering punctuation at a prompt is noise.
            let keywords = crate::lex::KEYWORDS
                .iter()
                .map(|(k, _)| *k)
                .filter(|k| k.as_bytes().first().is_some_and(u8::is_ascii_alphabetic));
            let commands = self.host.commands();
            let names = keywords
                .chain(crate::parse::BUILTINS.iter().copied())
                .chain(crate::parse::OPERATORS.iter().copied())
                .chain(self.functions.iter().map(|(n, _)| n.as_str()))
                .chain(commands.iter().map(alloc::string::String::as_str));
            return Completion { start: w.start, candidates: matching_names(w.text, names) };
        }

        let (dir, frag) = split_path(w.text);
        let target = match self.resolve_path(if dir.is_empty() { "." } else { dir }) {
            Ok(p) => p,
            // A path that will not resolve has nothing to offer; a Tab that finds nothing does
            // nothing.
            Err(_) => return Completion::none(w.start),
        };
        let entries = self.host.list_dir(&target);
        Completion { start: w.start, candidates: matching_paths(dir, frag, &entries) }
    }

    /// The host, for a driver that needs to flush or inspect it after a run.
    pub fn host_mut(&mut self) -> &mut dyn Host {
        &mut *self.host
    }

    /// Run a whole script, returning the value of its last expression statement.
    ///
    /// A script discards bare expression values (§11e); this returns the last one anyway for `nxsh
    /// -c` and the tests. The REPL/script auto-display difference lives at the driver.
    pub fn run(&mut self, script: &Script) -> Result<Val> {
        boundary_value(self.exec_block(&script.stmts)?)
    }

    /// Run one interactive line: execute it, bind `$last`, and return what the REPL should print
    /// (§11d, §11e).
    ///
    /// `$last` is a small `Record` of **both** halves, since §11d keeps a pipeline's value and its
    /// `PipelineStatus` distinct: `$last.value` is what you computed, `$last.status` whether it
    /// worked. REPL bookkeeping only, not visible inside a function or script.
    pub fn run_line(&mut self, src: &str) -> Result<Option<alloc::string::String>> {
        let script =
            crate::parse_script(src).map_err(|e| EvalError::new(alloc::format!("line {}: {}", e.line, e.message)))?;
        // **Hoist, exactly as a block does.** `exec` treats `Stmt::Def` as a no-op because
        // definitions are registered here (§5a); without it a `def` typed at a prompt vanished.
        self.hoist_defs(&script.stmts)?;
        let mut out = None;
        for stmt in &script.stmts {
            // **Anything `exec_block` does to a list of statements, `run_line` owes them too**
            // (`nxsh/CLAUDE.md`): the interrupt checkpoint between statements lived only there.
            self.check_interrupt()?;
            let value = boundary_value(self.exec(stmt)?)?;
            self.bind_last(&value)?;
            if crate::repl::should_display(stmt) && !value.is_null() {
                let styled = self.host.styled();
                out = Some(crate::ops::display(&value, styled));
            }
        }
        Ok(out)
    }

    /// Bind `$last` — REPL-only, per §2 and §11d.
    fn bind_last(&mut self, value: &Val) -> Result<()> {
        if self.mode != Mode::Repl {
            return Ok(());
        }
        let status = self.last_status.clone().unwrap_or(Val::NULL);
        let schema = Schema::new()
            .field(
                "value",
                value.as_data().and_then(|v| v.type_tag()).unwrap_or(libstream::wire::TypeTag::Null),
                TypeModifiers::NONE,
            )
            .field("status", libstream::wire::TypeTag::Record, TypeModifiers::NONE);
        let values = alloc::vec![
            value.as_data().cloned().unwrap_or(Value::Null),
            status.as_data().cloned().unwrap_or(Value::Null),
        ];
        let rec = Val::Data(Value::Record(Arc::new(Record { schema, values })));
        if let Some(slot) = self.lookup_mut("$last") {
            slot.val = rec;
            return Ok(());
        }
        self.bind("$last", rec, true, false)
    }

    /// Convenience for tests and `nxsh -c`: parse and run.
    pub fn eval_str(src: &str) -> Result<Val> {
        let script =
            crate::parse_script(src).map_err(|e| EvalError::new(alloc::format!("line {}: {}", e.line, e.message)))?;
        Interp::new().run(&script)
    }

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn lookup(&self, name: &str) -> Option<&Slot> {
        self.scopes.iter().rev().find_map(|s| s.iter().rev().find(|(n, _)| n == name).map(|(_, slot)| slot))
    }

    /// A binding, or failing that a hoisted function.
    fn lookup_any(&self, name: &str) -> Option<Val> {
        if let Some(s) = self.lookup(name) {
            return Some(s.val.clone());
        }
        self.functions.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.clone())
    }

    fn lookup_mut(&mut self, name: &str) -> Option<&mut Slot> {
        self.scopes.iter_mut().rev().find_map(|s| s.iter_mut().rev().find(|(n, _)| n == name).map(|(_, slot)| slot))
    }

    fn bind(&mut self, name: &str, val: Val, mutable: bool, constant: bool) -> Result<()> {
        let scope = self.scopes.last_mut().expect("a scope is always open");
        if let Some((_, slot)) = scope.iter().find(|(n, _)| n == name)
            && slot.constant
        {
            return Err(EvalError::new(alloc::format!("`{name}` is a const and cannot be rebound in the same scope")));
        }
        scope.push((String::from(name), Slot { val, mutable, constant }));
        Ok(())
    }

    /// A block's value is its last statement's, if that statement is expression-shaped
    /// (§9a). Anything else — a `let`, a `for` — leaves the block evaluating to `Null`.
    fn exec_block(&mut self, stmts: &[Stmt]) -> Result<Flow> {
        self.hoist_defs(stmts)?;
        let mut last = Val::NULL;
        for (i, s) in stmts.iter().enumerate() {
            // §11h's checkpoint, **between statements**: cheap, and it cannot tear an assignment.
            self.check_interrupt()?;
            match self.exec(s)? {
                Flow::Return(v) => return Ok(Flow::Return(v)),
                // The enclosing loop decides what these mean; the rest of the block is skipped.
                Flow::Break => return Ok(Flow::Break),
                Flow::Continue => return Ok(Flow::Continue),
                Flow::Normal(v) => {
                    last = if i + 1 == stmts.len() && s.is_expression_shaped() { v } else { Val::NULL };
                }
            }
        }
        Ok(Flow::Normal(last))
    }

    fn scoped_block(&mut self, stmts: &[Stmt]) -> Result<Flow> {
        self.push_scope();
        let r = self.exec_block(stmts);
        self.pop_scope();
        r
    }

    fn exec(&mut self, stmt: &Stmt) -> Result<Flow> {
        match stmt {
            Stmt::Bind { kind, name, ty, value, .. } => {
                let v = self.eval(value)?;
                if let Some(t) = ty {
                    check_type(&v, t)?;
                }
                let (mutable, constant) = match kind {
                    BindKind::Let => (false, false),
                    BindKind::Mut => (true, false),
                    BindKind::Const => (false, true),
                };
                self.bind(name, v, mutable, constant)?;
                Ok(Flow::Normal(Val::NULL))
            }
            Stmt::Assign { target, value } => {
                let v = self.eval(value)?;
                self.assign(target, v)?;
                Ok(Flow::Normal(Val::NULL))
            }
            Stmt::If { cond, then, otherwise } => {
                let c = self.condition(cond)?;
                if c {
                    self.scoped_block(then)
                } else if let Some(b) = otherwise {
                    self.scoped_block(b)
                } else {
                    Ok(Flow::Normal(Val::NULL))
                }
            }
            Stmt::While { cond, body } => {
                let mut guard = 0u64;
                while self.condition(cond)? {
                    // A loop body can be empty (`while true { }`, the case §11h exists for), so the
                    // per-statement checkpoint is not enough.
                    self.check_interrupt()?;
                    guard += 1;
                    if guard > self.iteration_limit {
                        return Err(EvalError::new(
                            "loop ran past the iteration limit — this is almost certainly \
                             a runaway condition",
                        ));
                    }
                    match self.scoped_block(body)? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal(_) => {}
                    }
                }
                Ok(Flow::Normal(Val::NULL))
            }
            Stmt::For { binding, iterable, body } => {
                let items = self.iterate(iterable)?;
                for item in items {
                    self.check_interrupt()?;
                    self.push_scope();
                    // The loop variable is fresh each turn, so a closure captures *this*
                    // iteration's value (§5a).
                    self.bind(binding, item, false, false)?;
                    let r = self.exec_block(body);
                    self.pop_scope();
                    match r? {
                        Flow::Return(v) => return Ok(Flow::Return(v)),
                        Flow::Break => break,
                        Flow::Continue | Flow::Normal(_) => {}
                    }
                }
                Ok(Flow::Normal(Val::NULL))
            }
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.eval(e)?,
                    None => Val::NULL,
                };
                Ok(Flow::Return(v))
            }
            // §2's raising half: `try`/`catch` could catch an error but nothing could produce one.
            Stmt::Fail(e) => {
                let v = self.eval(e)?;
                Err(match v.as_data() {
                    Some(Value::Str(m)) => EvalError::of(ERROR, m.clone()),
                    // A record is raised as the error value itself and must carry a `message`:
                    // without one it is the vague failure §6 spends its diff on.
                    Some(Value::Record(r)) => {
                        let field =
                            |n: &str| r.schema.fields.iter().position(|f| f.name == n).and_then(|i| r.values.get(i));
                        let Some(Value::Str(message)) = field("message") else {
                            return Err(EvalError::new(
                                "`fail` takes a message — a Record raised as an error must \
                                 carry `message: String`",
                            ));
                        };
                        let kind = match field("kind") {
                            Some(Value::Str(k)) => k.clone(),
                            _ => String::from(ERROR),
                        };
                        EvalError { message: message.clone(), kind, stages: None, exit: None }
                    }
                    _ => EvalError::new(alloc::format!(
                        "`fail` takes a String message or a Record, got {}",
                        v.type_name()
                    )),
                })
            }
            Stmt::Break => Ok(Flow::Break),
            Stmt::Continue => Ok(Flow::Continue),
            // `try` in statement position is unwrapped here, not via `eval`, which can only return
            // a value: its control flow must still reach the enclosing loop or function.
            Stmt::Expr(Expr::Try { body, catch_binding, catch_body }) => self.exec_try(body, catch_binding, catch_body),
            Stmt::Expr(e) => Ok(Flow::Normal(self.eval(e)?)),
            Stmt::Def { .. } => Ok(Flow::Normal(Val::NULL)),
            // §1: a local, visible block rather than a `pipefail`-style ambient mode, in line with
            // rejecting a global `$?` and implicit env inheritance.
            Stmt::Strict(body) => {
                let saved = self.strict;
                self.strict = true;
                let r = self.scoped_block(body);
                self.strict = saved;
                r
            }
            Stmt::Use { path, names, alias, base } => {
                self.exec_use(path, names, alias, base.as_deref())?;
                Ok(Flow::Normal(Val::NULL))
            }
        }
    }

    /// `try { … } catch (e) { … }` — one implementation, two entry points.
    ///
    /// §2: sugar over branching on a failure, **not** stack unwinding.
    ///
    /// It returns `Flow` rather than a `Val` **so statement position keeps working**: `for x in xs
    /// { try { … } catch { continue } }` must reach the loop, and a `return` inside a `try` must
    /// leave the function. `eval` reduces it with `expression_value`, which refuses a `break` in
    /// *value* position (`TODO(control-flow-in-expression-position)`).
    fn exec_try(&mut self, body: &[Stmt], catch_binding: &Option<String>, catch_body: &[Stmt]) -> Result<Flow> {
        let e = match self.scoped_block(body) {
            Ok(flow) => return Ok(flow),
            Err(e) => e,
        };
        // **`exit` is not a failure** (§11f): a `catch` that swallowed it would let a script
        // prevent leaving the shell by accident.
        if e.is_exit() {
            return Err(e);
        }
        self.push_scope();
        if let Some(n) = catch_binding {
            // An ordinary `Record` built in one place (`EvalError::to_record`), so `match err { …
            // }` needs no new grammar.
            let _ = self.bind(n, e.to_record(), false, false);
        }
        let r = self.exec_block(catch_body);
        self.pop_scope();
        r
    }

    /// Ask the host whether the terminal wants this evaluation to stop (§11h).
    ///
    /// An interrupt unwinds as an ordinary error, so `try`/`catch` still runs. It is *not* an
    /// `exit`: leaving is a decision, being interrupted is something that happens.
    fn check_interrupt(&mut self) -> Result<()> {
        if self.host.interrupted() {
            return Err(EvalError::of(INTERRUPTED, "interrupted"));
        }
        Ok(())
    }

    /// A condition must be a `Bool`. See the module docs: no truthiness, deliberately.
    fn condition(&mut self, e: &Expr) -> Result<bool> {
        let v = self.eval(e)?;
        v.as_bool().ok_or_else(|| {
            EvalError::new(alloc::format!(
                "a condition must be Bool, got {} — there is no truthiness here, so write \
                 the comparison out",
                v.type_name()
            ))
        })
    }

    /// What `for` walks: a range, a list, or a table's rows. Anything else is an error, not a
    /// one-element loop.
    fn iterate(&mut self, e: &Expr) -> Result<Vec<Val>> {
        let v = self.eval(e)?;
        match &v {
            Val::Range { start, end, inclusive } => {
                let last = if *inclusive { *end } else { *end - 1 };
                let mut out = Vec::new();
                let mut i = *start;
                while i <= last {
                    out.push(Val::int(i));
                    i += 1;
                    if out.len() as u64 > self.iteration_limit {
                        return Err(EvalError::new("range is too large to iterate"));
                    }
                }
                Ok(out)
            }
            Val::Data(Value::List(items)) => Ok(items.iter().map(|v| Val::Data(v.clone())).collect()),
            Val::Data(Value::Table(t)) => Ok(t
                .rows
                .iter()
                .map(|row| Val::Data(Value::Record(Arc::new(Record { schema: t.schema.clone(), values: row.clone() }))))
                .collect()),
            _ => Err(EvalError::new(alloc::format!(
                "cannot iterate a {} — `for` walks a Range, a List or a Table",
                v.type_name()
            ))),
        }
    }

    /// `lvalue = value` (§9d). Legal only if the root binding is `mut`.
    fn assign(&mut self, target: &LValue, value: Val) -> Result<()> {
        // Evaluate index expressions before touching the binding, so the slot borrow does not
        // overlap evaluation that could read it.
        let mut steps: Vec<Step> = Vec::new();
        for s in &target.path {
            steps.push(match s {
                LValueStep::Field(f) => Step::Field(f.clone()),
                LValueStep::Index(e) => Step::Index(self.index_of(e)?),
            });
        }
        let root = target.root.clone();
        let slot = self.lookup_mut(&root).ok_or_else(|| EvalError::new(alloc::format!("`{root}` is not bound")))?;
        if !slot.mutable {
            return Err(EvalError::new(alloc::format!(
                "`{root}` was bound with `let` — only a `mut` binding can be assigned to"
            )));
        }
        if steps.is_empty() {
            slot.val = value;
            return Ok(());
        }
        let mut data = match &slot.val {
            Val::Data(v) => v.clone(),
            other => {
                return Err(EvalError::new(alloc::format!("cannot assign into a {}", other.type_name())));
            }
        };
        set_path(&mut data, &steps, value.into_data().map_err(EvalError::new)?)?;
        slot.val = Val::Data(data);
        Ok(())
    }

    fn index_of(&mut self, e: &Expr) -> Result<IndexKey> {
        let v = self.eval(e)?;
        match v {
            Val::Data(Value::Int(i)) => Ok(IndexKey::Pos(i)),
            Val::Data(Value::Str(s)) => Ok(IndexKey::Name(s)),
            other => {
                Err(EvalError::new(alloc::format!("an index must be an Int or a String, got {}", other.type_name())))
            }
        }
    }

    pub fn eval(&mut self, e: &Expr) -> Result<Val> {
        match e {
            Expr::Int(v) => Ok(Val::int(*v)),
            Expr::Float(v) => Ok(Val::float(*v)),
            Expr::Str(v) | Expr::Word(v) => Ok(Val::str(v.clone())),
            Expr::Bool(v) => Ok(Val::bool(*v)),
            Expr::Null => Ok(Val::NULL),
            // Inside a keyword stage `_` is the value flowing past (§8c).
            Expr::Underscore => match self.lookup_any(UNDERSCORE) {
                Some(v) => Ok(v),
                None => Err(EvalError::new("`_` is a pipeline placeholder and has no value on its own")),
            },
            // A `/pattern/` literal is its source text, so a pattern can also be built at run time.
            Expr::Regex(p) => Ok(Val::str(p.clone())),
            // D4: a local binding first, then a command. A name that is neither gets a message
            // naming *both* searches.
            Expr::Ident(name) => match self.lookup_any(name) {
                Some(v) => Ok(v),
                None => {
                    let call = Expr::Call(Box::new(Call {
                        name: name.clone(),
                        kind: CallKind::External,
                        args: Vec::new(),
                        forced_external: false,
                    }));
                    self.pipeline(core::slice::from_ref(&call)).map_err(|e| {
                        // **Only rewrap when the name was the problem.** If the program was found
                        // and *ran*, the failure is the program's; rewrapping it buried its `kind`
                        // and the per-stage report (§2).
                        if e.kind == PIPELINE_FAILED || e.is_exit() {
                            return e;
                        }
                        EvalError::new(alloc::format!(
                            "`{name}` is not a binding, and running it as a program \
                             failed: {}",
                            e.message
                        ))
                    })
                }
            },
            Expr::Unary(op, a) => {
                let v = self.eval(a)?;
                unary(*op, v)
            }
            Expr::Binary(op, a, b) => self.binary(*op, a, b),
            Expr::Range { start, end, inclusive } => {
                let s = self.eval(start)?;
                let e2 = self.eval(end)?;
                match (s.as_data(), e2.as_data()) {
                    (Some(Value::Int(a)), Some(Value::Int(b))) => {
                        Ok(Val::Range { start: *a, end: *b, inclusive: *inclusive })
                    }
                    _ => Err(EvalError::new(alloc::format!(
                        "a range needs Int bounds, got {}..{}",
                        s.type_name(),
                        e2.type_name()
                    ))),
                }
            }
            Expr::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for i in items {
                    out.push(self.eval(i)?);
                }
                Val::list(out).map_err(EvalError::new)
            }
            Expr::Record(fields) => {
                let mut schema = Schema::new();
                let mut values = Vec::with_capacity(fields.len());
                for (name, e) in fields {
                    let v = self.eval(e)?.into_data().map_err(EvalError::new)?;
                    let tag = v.type_tag().ok_or_else(|| {
                        EvalError::new(
                            "a Table cannot be a record field — it is a stream, \
                                        not a cell",
                        )
                    })?;
                    schema = schema.field(name, tag, TypeModifiers::NONE);
                    values.push(v);
                }
                Ok(Val::Data(Value::Record(Arc::new(Record { schema, values }))))
            }
            Expr::Closure { params, body } => {
                // Capture by value at creation (§5a): snapshot every visible binding, which makes
                // the loop-variable bug impossible rather than unlikely.
                let mut captured = Vec::new();
                for scope in &self.scopes {
                    for (n, slot) in scope {
                        captured.push((n.clone(), slot.val.clone()));
                    }
                }
                Ok(Val::Func(Arc::new(Func { name: None, params: params.clone(), body: body.clone(), captured })))
            }
            Expr::Block(stmts) => expression_value(self.scoped_block(stmts)?),
            Expr::If { cond, then, otherwise } => {
                if self.condition(cond)? {
                    expression_value(self.scoped_block(then)?)
                } else if let Some(b) = otherwise {
                    expression_value(self.scoped_block(b)?)
                } else {
                    Ok(Val::NULL)
                }
            }
            Expr::Field(base, name) => {
                let b = self.eval(base)?;
                field_of(&b, name, false)
            }
            Expr::TryField(base, name) => {
                let b = self.eval(base)?;
                if b.is_null() {
                    return Ok(Val::NULL); // §9e: `?.` short-circuits on Null
                }
                field_of(&b, name, true)
            }
            Expr::Index(base, idx) => {
                let b = self.eval(base)?;
                let k = self.index_of(idx)?;
                index_of(&b, &k)
            }
            Expr::Match { scrutinee, arms } => self.eval_match(scrutinee, arms),
            // §2's `?`: an operation that fails already raises and exits the function, so `?` is
            // the default behaviour and this a pass-through. It becomes load-bearing only once an
            // operation returns a Result-shaped **value** instead of raising; none does yet.
            Expr::Try { body, catch_binding, catch_body } => {
                expression_value(self.exec_try(body, catch_binding, catch_body)?)
            }
            // §6: `expect` is ascription, `assert` its sibling for a *content* predicate. Separate
            // keywords so the message says "shape mismatch" or "assertion failed".
            Expr::Expect(inner, t) => {
                let v = self.eval(inner)?;
                check_type(&v, t)?;
                Ok(v)
            }
            Expr::Parse(inner, t) => {
                let v = self.eval(inner)?;
                parse_value(v, t)
            }
            Expr::Assert(pred) => {
                let v = self.eval(pred)?;
                match v.as_bool() {
                    Some(true) => Ok(Val::NULL),
                    Some(false) => Err(EvalError::of(ASSERTION_FAILED, "assertion failed")),
                    None => {
                        Err(EvalError::new(alloc::format!("`assert` needs a Bool predicate, got {}", v.type_name())))
                    }
                }
            }
            Expr::Pipeline(stages) => self.pipeline(stages),
            Expr::Time(timed) => self.time(timed),
            Expr::Call(c) => {
                // D4: a **local binding wins** over a command name for a bare, argument-less
                // reference, so the operator set does not behave like reserved words (`last` and
                // `count` are §10b operators and ordinary variable names). Inside a pipeline the
                // precedence is the other way round; see `pipeline`.
                //
                // `f()` is a call with an empty list, not a mention, and a `Def`-kind call only
                // comes from the parens form (§5b); conflating them made `f()` evaluate to the
                // function itself.
                if c.args.is_empty()
                    && !c.forced_external
                    && c.kind != CallKind::Def
                    && let Some(v) = self.lookup_any(&c.name)
                {
                    return Ok(v);
                }
                if c.kind == CallKind::External {
                    return self.pipeline(core::slice::from_ref(e));
                }
                if c.kind == CallKind::Operator {
                    return self.apply_operator(c, None);
                }
                if c.kind == CallKind::Def {
                    return self.call_def(c, None);
                }
                if c.kind == CallKind::Builtin {
                    return self.apply_builtin(c);
                }
                Err(unavailable(c))
            }
        }
    }

    /// Bind every `def` in a block before executing any of it (§5a).
    ///
    /// Mutual recursion needs it, and `let` deliberately does not hoist: it is tied to evaluation
    /// order while a `def` is a declaration.
    fn hoist_defs(&mut self, stmts: &[Stmt]) -> Result<()> {
        for s in stmts {
            if let Stmt::Def { name, params, ret: _, body, .. } = s {
                // A named function captures **nothing** (§5a); only a closure captures.
                let f = Val::Func(Arc::new(Func {
                    name: Some(name.clone()),
                    params: params.clone(),
                    body: body.clone(),
                    captured: Vec::new(),
                }));
                self.functions.retain(|(n, _)| n != name);
                self.functions.push((name.clone(), f));
            }
        }
        Ok(())
    }

    /// Call a `def` or a closure with the §5b convention: positional, then named, then
    /// defaults, with an optional variadic tail.
    fn call_function(
        &mut self,
        f: &Func,
        positional: &[Val],
        named: &[(String, Val)],
        operand: Option<Val>,
        fill: bool,
    ) -> Result<Val> {
        let mut positional = positional.to_vec();
        // §5b's pipeline-fill rule: `_` marks where the piped value goes when an argument list is
        // present; a bare, argument-free call fills its sole slot. Implicit first-parameter fill
        // was rejected: nothing guarantees the first parameter is the data slot.
        if let Some(v) = operand {
            if fill {
                positional.insert(0, v);
            } else if positional.is_empty() && named.is_empty() {
                positional.push(v);
            } else {
                return Err(EvalError::new(alloc::format!(
                    "`{}` was given an argument list, so mark the piped value with `_` \
                     (§5b) — the language will not guess which parameter it belongs to",
                    f.name.as_deref().unwrap_or("this function")
                )));
            }
        }

        let variadic = f.params.last().is_some_and(|p| p.variadic);
        let fixed = if variadic { f.params.len() - 1 } else { f.params.len() };
        if positional.len() > fixed && !variadic {
            return Err(EvalError::new(alloc::format!(
                "`{}` takes {} argument(s), given {}",
                f.name.as_deref().unwrap_or("this function"),
                render_i64(fixed as i64),
                render_i64(positional.len() as i64)
            )));
        }

        let saved = core::mem::replace(&mut self.scopes, alloc::vec![Vec::new()]);
        for (n, v) in &f.captured {
            let _ = self.bind(n, v.clone(), false, false);
        }
        self.push_scope();

        let mut result = Ok(());
        for (i, p) in f.params.iter().enumerate() {
            if p.variadic {
                let rest: Vec<Val> = positional.iter().skip(i).cloned().collect();
                match Val::list(rest) {
                    Ok(v) => {
                        let _ = self.bind(&p.name, v, false, false);
                    }
                    Err(e) => result = Err(EvalError::new(e)),
                }
                continue;
            }
            let supplied =
                positional.get(i).cloned().or_else(|| named.iter().find(|(n, _)| n == &p.name).map(|(_, v)| v.clone()));
            let value = match supplied {
                Some(v) => v,
                None => match &p.default {
                    // §5b: defaults are evaluated **fresh per call, in the function's own parameter
                    // scope**: no shared-mutable-default trap, and a later default can use an
                    // earlier parameter.
                    Some(d) => match self.eval(d) {
                        Ok(v) => v,
                        Err(e) => {
                            result = Err(e);
                            Val::NULL
                        }
                    },
                    None => {
                        result = Err(EvalError::new(alloc::format!(
                            "`{}` needs an argument for `{}`",
                            f.name.as_deref().unwrap_or("this function"),
                            p.name
                        )));
                        Val::NULL
                    }
                },
            };
            if let (Some(t), Ok(())) = (&p.ty, &result)
                && let Err(e) = check_type(&value, t)
            {
                result = Err(EvalError::new(alloc::format!(
                    "`{}` parameter `{}`: {}",
                    f.name.as_deref().unwrap_or("this function"),
                    p.name,
                    e.message
                )));
            }
            let _ = self.bind(&p.name, value, false, false);
        }

        // An unknown named argument is an error, not silently ignored (§5b).
        if result.is_ok()
            && let Some((n, _)) = named.iter().find(|(n, _)| !f.params.iter().any(|p| &p.name == n))
        {
            result = Err(EvalError::new(alloc::format!(
                "`{}` has no parameter `{n}`",
                f.name.as_deref().unwrap_or("this function")
            )));
        }

        let out = match result {
            Ok(()) => self.exec_block(&f.body).and_then(boundary_value),
            Err(e) => Err(e),
        };
        self.pop_scope();
        self.scopes = saved;
        out
    }

    /// Evaluate a `def` call site.
    fn call_def(&mut self, c: &Call, operand: Option<Val>) -> Result<Val> {
        let Some(v) = self.lookup_any(&c.name) else {
            return Err(EvalError::new(alloc::format!("`{}` is not a function", c.name)));
        };
        let Val::Func(f) = v else {
            return Err(EvalError::new(alloc::format!("`{}` is not something callable", c.name)));
        };
        let mut positional = Vec::new();
        let mut named = Vec::new();
        let mut fill = false;
        for a in &c.args {
            match a {
                Arg::Positional(e) => positional.push(self.eval(e)?),
                Arg::Named(n, e) => {
                    let v = self.eval(e)?;
                    named.push((n.clone(), v));
                }
                Arg::PipeFill => fill = true,
                Arg::Flag(f, _) | Arg::ShortFlags(f) => {
                    return Err(EvalError::new(alloc::format!(
                        "`{}` is a script function, so `--{f}` does not apply — flags are \
                         the external-program convention (§5b)",
                        c.name
                    )));
                }
            }
        }
        self.call_function(&f, &positional, &named, operand, fill)
    }

    fn eval_match(&mut self, scrutinee: &Expr, arms: &[MatchArm]) -> Result<Val> {
        let v = self.eval(scrutinee)?;
        for arm in arms {
            let mut bindings: Vec<(String, Val)> = Vec::new();
            if !self.pattern_matches(&arm.pattern, &v, &mut bindings)? {
                continue;
            }
            self.push_scope();
            for (n, bv) in &bindings {
                let _ = self.bind(n, bv.clone(), false, false);
            }
            // A guard runs *with the arm's bindings in scope*: `{ name, size } if size > 1000`.
            let passed = match &arm.guard {
                Some(g) => match self.eval(g)?.as_bool() {
                    Some(b) => b,
                    None => {
                        self.pop_scope();
                        return Err(EvalError::new("a match guard must be Bool"));
                    }
                },
                None => true,
            };
            if !passed {
                self.pop_scope();
                continue;
            }
            let r = self.exec_block(&arm.body);
            self.pop_scope();
            return expression_value(r?);
        }
        // §9f: no static exhaustiveness check (§6 made that call for the type system). A value
        // hitting no arm is a runtime `MatchError`, styled like a `TypeError`.
        Err(EvalError::new(alloc::format!("MatchError: no arm matched a {} value", v.type_name())))
    }

    fn pattern_matches(&mut self, p: &Pattern, v: &Val, out: &mut Vec<(String, Val)>) -> Result<bool> {
        Ok(match p {
            Pattern::Wildcard => true,
            Pattern::Bind(n) => {
                out.push((n.clone(), v.clone()));
                true
            }
            Pattern::Capture(n, inner) => {
                if self.pattern_matches(inner, v, out)? {
                    out.push((n.clone(), v.clone()));
                    true
                } else {
                    false
                }
            }
            Pattern::Or(alts) => {
                for a in alts {
                    // Each alternative gets a fresh binding set, so a failed one leaves nothing
                    // behind.
                    let mut trial = Vec::new();
                    if self.pattern_matches(a, v, &mut trial)? {
                        out.extend(trial);
                        return Ok(true);
                    }
                }
                false
            }
            // §9f: variant patterns reuse the **ascription vocabulary** (§6), not a second set of
            // names.
            Pattern::Variant(name, inner) => {
                if v.type_name() != name {
                    return Ok(false);
                }
                if inner.is_empty() {
                    return Ok(true);
                }
                match v {
                    Val::Data(Value::List(items)) => {
                        if inner.len() != items.len() {
                            return Ok(false);
                        }
                        for (sub, item) in inner.iter().zip(items.iter()) {
                            if !self.pattern_matches(sub, &Val::Data(item.clone()), out)? {
                                return Ok(false);
                            }
                        }
                        true
                    }
                    _ => {
                        if inner.len() != 1 {
                            return Ok(false);
                        }
                        self.pattern_matches(&inner[0], v, out)?
                    }
                }
            }
            // §6's subset match for free: "a Record with at least these fields".
            Pattern::Record(fields) => {
                let Val::Data(Value::Record(rec)) = v else { return Ok(false) };
                for (name, sub) in fields {
                    let Some(idx) = rec.schema.fields.iter().position(|d| &d.name == name) else {
                        return Ok(false);
                    };
                    let fv = Val::Data(rec.values.get(idx).cloned().unwrap_or(Value::Null));
                    match sub {
                        Some(sp) => {
                            if !self.pattern_matches(sp, &fv, out)? {
                                return Ok(false);
                            }
                        }
                        None => out.push((name.clone(), fv)),
                    }
                }
                true
            }
            Pattern::Literal(e) => {
                let lit = self.eval(e)?;
                values_equal(&lit, v)
            }
            Pattern::Range { start, end, inclusive } => {
                let s = self.eval(start)?;
                let e2 = self.eval(end)?;
                let (Some(Value::Int(a)), Some(Value::Int(b)), Some(Value::Int(x))) =
                    (s.as_data(), e2.as_data(), v.as_data())
                else {
                    return Ok(false);
                };
                if *inclusive { *x >= *a && *x <= *b } else { *x >= *a && *x < *b }
            }
        })
    }

    /// Read, run and import the module at `abs`, which `use` named as `path`; see
    /// [`exec_use`](Self::exec_use) for where `abs` comes from.
    ///
    /// Explicit relative path only, no search: §9h rejects `PATH`-like resolution for the reason
    /// the system rejects ambient authority, a name crossing a boundary should be visible.
    pub(crate) fn load_module(
        &mut self,
        path: &str,
        abs: &str,
        names: &Option<Vec<String>>,
        alias: &Option<String>,
    ) -> Result<()> {
        let bytes = self.host.read_file(abs).map_err(EvalError::new)?;
        let src = alloc::string::String::from_utf8(bytes)
            .map_err(|_| EvalError::new(alloc::format!("`{path}` is not valid UTF-8")))?;
        let module = crate::parse_script_in(&src, Some(crate::paths::dir_of(abs)))
            .map_err(|e| EvalError::new(alloc::format!("`{path}` line {}: {}", e.line, e.message)))?;

        // Run the module in a scope of its own and take only what it exported. Its `def`s land in
        // `functions`, so both are swept; `functions` is snapshotted first so a module cannot
        // export something the *caller* had already defined.
        let before = self.functions.len();
        self.push_scope();
        let r = self.exec_block(&module.stmts);
        let mut exported: Vec<(String, Val)> = {
            let scope = self.scopes.last().expect("the module scope is open");
            scope.iter().map(|(n, s)| (n.clone(), s.val.clone())).collect()
        };
        exported.extend(self.functions.iter().skip(before).cloned());
        self.functions.truncate(before);
        self.pop_scope();
        r?;

        // `pub` is what crosses the file boundary (§9h).
        let public: Vec<&String> = module
            .stmts
            .iter()
            .filter_map(|s| match s {
                Stmt::Def { name, public: true, .. } => Some(name),
                Stmt::Bind { name, public: true, .. } => Some(name),
                _ => None,
            })
            .collect();

        match (names, alias) {
            (Some(wanted), _) => {
                for n in wanted {
                    if !public.contains(&n) {
                        return Err(EvalError::new(alloc::format!(
                            "`{path}` does not export `{n}` — mark it `pub` to let it cross \
                             the file boundary"
                        )));
                    }
                    let Some((_, v)) = exported.iter().find(|(en, _)| en == n) else {
                        return Err(EvalError::new(alloc::format!("`{path}` has no `{n}`")));
                    };
                    self.bind(n, v.clone(), false, false)?;
                }
            }
            (None, Some(a)) => {
                // `use "…" as utils` binds `utils.helper`, not a record called `utils`: exports are
                // usually functions, which are not TSM1-representable (§5c), so a `Record` could
                // not hold them. The alias is a *naming device*.
                for n in &public {
                    if let Some((_, v)) = exported.iter().find(|(en, _)| &en == n) {
                        self.bind(&alloc::format!("{a}.{n}"), v.clone(), false, false)?;
                    }
                }
            }
            (None, None) => {
                return Err(EvalError::new("`use` needs `{ names }` or `as alias` — there is no wildcard import"));
            }
        }
        Ok(())
    }

    /// Invoke a closure with one argument.
    ///
    /// §8b's sugar *is* a closure (`filter size > 1000` is `filter { |it| it.size > 1000 }`). It
    /// runs in a scope built from its **captured snapshot** (§5a), not the caller's.
    pub fn call_closure(&mut self, f: &Func, args: &[Val]) -> Result<Val> {
        if args.len() != f.params.len() {
            return Err(EvalError::new(alloc::format!(
                "this closure takes {} argument(s), given {}",
                render_i64(f.params.len() as i64),
                render_i64(args.len() as i64)
            )));
        }
        // A fresh scope stack: a closure sees only its capture, or `filter` could read the
        // pipeline's internals by accident.
        let saved = core::mem::replace(&mut self.scopes, alloc::vec![Vec::new()]);
        for (n, v) in &f.captured {
            let _ = self.bind(n, v.clone(), false, false);
        }
        self.push_scope();
        for (p, v) in f.params.iter().zip(args) {
            let _ = self.bind(&p.name, v.clone(), false, false);
        }
        let r = self.exec_block(&f.body);
        self.pop_scope();
        self.scopes = saved;
        boundary_value(r?)
    }

    /// **`time PIPELINE`** (§3): the pipeline evaluated as ever, its value or failure passed
    /// through, and one line on `stderr` when it ends: the elapsed time by the host's monotonic
    /// clock, and what the disks and page cache did, from `/dev/devices/io.tsm` read before and
    /// after ([`crate::timing`]). That read is answered from kernel counters, so touches no disk.
    fn time(&mut self, timed: &Expr) -> Result<Val> {
        let before = self.io_rows();
        let started = self.host.now_ns();
        let result = self.eval(timed);
        let ended = self.host.now_ns();
        let after = self.io_rows();
        let wall = started.zip(ended).map(|(s, e)| e.saturating_sub(s));
        let mut line = crate::timing::line(wall, before.as_deref(), after.as_deref());
        line.push('\n');
        self.host.diag(&line);
        result
    }

    /// `io.tsm`'s rows, or `None` where it is not bound or does not read.
    fn io_rows(&mut self) -> Option<Vec<crate::timing::IoRow>> {
        let bytes = self.host.read_file(crate::timing::IO_TABLE).ok()?;
        crate::timing::rows(&bytes)
    }

    /// §3's shell-state builtins: they mutate *this* process, which an external cannot.
    fn apply_builtin(&mut self, c: &Call) -> Result<Val> {
        match c.name.as_str() {
            "cd" => {
                let target = match c.args.first() {
                    Some(Arg::Positional(e)) => match e {
                        Expr::Word(w) | Expr::Str(w) | Expr::Ident(w) => w.clone(),
                        other => self.eval(other)?.render(),
                    },
                    // `cd` with no argument goes home, and says so if there is no home.
                    None => match self.env_str("HOME") {
                        Some(h) => h,
                        None => {
                            return Err(EvalError::new("`cd` with no argument goes to HOME, and HOME is not set"));
                        }
                    },
                    Some(_) => return Err(EvalError::new("`cd` takes one path")),
                };
                let resolved = self.resolve_path(&target)?;
                // **Resolve first, set second.** The invariant is "PWD named something real when
                // you went there", not "PWD is whatever you typed".
                if !self.path_exists(&resolved) {
                    return Err(EvalError::new(alloc::format!("`{resolved}` does not exist, so PWD was left alone")));
                }
                self.set_env_entry("PWD", Value::Str(resolved));
                Ok(Val::NULL)
            }
            // §11f. `exit` is a *control outcome of evaluation*, not a string the REPL loop watches
            // for (`exit 1` once missed that comparison). It travels the error channel because that
            // already crosses a `def`, a closure and a loop; `catch` re-raises it (see `exec_try`).
            "exit" => {
                let status = match c.args.first() {
                    None => 0,
                    Some(Arg::Positional(e)) => {
                        // A builtin's argument is a **bareword**, so `exit 3` arrives as the text
                        // `"3"`; the `parse` scanner keeps one definition of what a number looks
                        // like.
                        let v = self.eval(e)?;
                        match v.as_data() {
                            Some(Value::Int(n)) => *n as i32,
                            Some(Value::Str(t)) => match scan_number(t) {
                                Some(Val::Data(Value::Int(n))) => n as i32,
                                _ => {
                                    return Err(EvalError::new(alloc::format!("`exit` takes a status Int, got {t:?}")));
                                }
                            },
                            _ => {
                                return Err(EvalError::new(alloc::format!(
                                    "`exit` takes a status Int, got {}",
                                    v.type_name()
                                )));
                            }
                        }
                    }
                    Some(_) => {
                        return Err(EvalError::new("`exit` takes a single status argument"));
                    }
                };
                Err(EvalError::exiting(status))
            }
            _ => Err(unavailable(c)),
        }
    }

    fn env_str(&self, name: &str) -> Option<alloc::string::String> {
        let i = self.env.schema.fields.iter().position(|f| f.name == name)?;
        self.env.values.get(i)?.as_str().map(alloc::string::String::from)
    }

    /// Whether `path` resolves — used by `cd` before it commits.
    fn path_exists(&mut self, path: &str) -> bool {
        self.host.exists(path)
    }

    /// Apply a generic operator to the value arriving on its left.
    ///
    /// `operand` is `None` outside a pipeline. Most operators need one: `filter` with nothing to
    /// filter is a mistake, not an empty result.
    fn apply_operator(&mut self, c: &Call, operand: Option<Val>) -> Result<Val> {
        // §6's `ls | assert (count > 0)`: inside a keyword stage the value flowing past is bound to
        // `_`, so an operator with nothing piped *into* it means that value. `_` is bound nowhere
        // else, so this cannot reach past its stage.
        let operand = operand.or_else(|| self.lookup_any(UNDERSCORE));
        let name = c.name.as_str();
        let mut flags: Vec<&str> = Vec::new();
        let mut closures: Vec<Val> = Vec::new();
        // Positional arguments are kept **unevaluated** and interpreted per operator: `sort size`
        // names a *column* (as in §8b's predicates), while `display files` names a *binding*. A
        // global rule breaks one of them, so the operator decides, because only it knows.
        let mut raw: Vec<&Expr> = Vec::new();
        for a in &c.args {
            match a {
                Arg::Positional(Expr::Closure { params, body }) => {
                    closures.push(self.eval(&Expr::Closure { params: params.clone(), body: body.clone() })?);
                }
                Arg::Positional(e) => raw.push(e),
                Arg::Flag(f, None) => flags.push(f.as_str()),
                Arg::Flag(f, Some(e)) => {
                    flags.push(f.as_str());
                    raw.push(e);
                }
                Arg::ShortFlags(f) => {
                    for ch in f.chars() {
                        // Short flags map to their long form (the GNU convention, §10f).
                        if ch == 'r' {
                            flags.push("reverse");
                        }
                    }
                }
                Arg::Named(n, _) => {
                    return Err(EvalError::new(alloc::format!(
                        "`{name}` is a generic operator and takes bareword arguments, so \
                         `{n}:` does not apply — named arguments are the `def` convention \
                         (§5b)"
                    )));
                }
                Arg::PipeFill => {}
            }
        }
        let reverse = flags.contains(&"reverse");

        let need = |o: Option<Val>| -> Result<Val> {
            o.ok_or_else(|| {
                EvalError::new(alloc::format!(
                    "`{name}` needs an operand — it works on the value arriving from the \
                     left of a pipe"
                ))
            })
        };
        let wrap = |r: core::result::Result<Val, alloc::string::String>| -> Result<Val> { r.map_err(EvalError::new) };
        let field_names: Vec<alloc::string::String> = raw
            .iter()
            .filter_map(|e| match e {
                Expr::Ident(n) | Expr::Word(n) | Expr::Str(n) => Some(n.clone()),
                _ => None,
            })
            .collect();

        match name {
            "count" => wrap(ops::count(&need(operand)?)),
            // §10b: argument-free scalar operators; the operand is the whole input.
            "trim" | "upper" | "lower" | "keys" | "values" | "round" | "floor" | "ceil" | "trunc" | "abs" => {
                let v = need(operand)?;
                wrap(match name {
                    "trim" => ops::trim(&v),
                    "upper" => ops::upper(&v),
                    "lower" => ops::lower(&v),
                    "keys" => ops::keys(&v),
                    "values" => ops::values(&v),
                    "round" => ops::round(&v),
                    "floor" => ops::floor(&v),
                    "ceil" => ops::ceil(&v),
                    "trunc" => ops::trunc(&v),
                    _ => ops::abs(&v),
                })
            }
            "split" | "join" => {
                let v = need(operand)?;
                let Some(sep) = field_names.first() else {
                    return Err(EvalError::new(alloc::format!("`{name}` needs a separator")));
                };
                wrap(if name == "split" { ops::split(&v, sep) } else { ops::join(&v, sep) })
            }
            "replace" => {
                let v = need(operand)?;
                let (Some(from), Some(to)) = (field_names.first(), field_names.get(1)) else {
                    return Err(EvalError::new("`replace` needs what to replace and what to replace it with"));
                };
                wrap(ops::replace(&v, from, to))
            }
            // §10b: `capture` is the only operator with real engine work behind it (submatch slots
            // in the Pike VM) and the only one returning `null` for a failure rather than raising:
            // "did not match" is an ordinary answer that `??` and `== null` handle (§9e). Groups
            // are positional; naming them needs a name table in the compiler.
            // TODO(regex-named-captures): `docs/rationale/deferred-decisions.md`.
            "capture" => {
                let v = need(operand)?;
                let Some(Value::Str(text)) = v.as_data() else {
                    return Err(EvalError::new(alloc::format!("`capture` works on a String, got {}", v.type_name())));
                };
                let Some(pattern) = raw.first() else {
                    return Err(EvalError::new("`capture` needs a pattern"));
                };
                let pattern = self.eval(pattern)?;
                let Some(Value::Str(pattern)) = pattern.as_data() else {
                    return Err(EvalError::new("`capture` needs a pattern"));
                };
                let re = crate::regex::Regex::new(pattern).map_err(EvalError::new)?;
                let chars: Vec<char> = text.chars().collect();
                Ok(match re.captures(text) {
                    None => Val::NULL,
                    Some(groups) => Val::Data(Value::List(Arc::from(
                        groups
                            .iter()
                            .map(|g| match g {
                                // A group that did not participate is Null, not "".
                                None => Value::Null,
                                Some((a, b)) => Value::Str(chars[*a..*b].iter().collect()),
                            })
                            .collect::<Vec<_>>(),
                    ))),
                })
            }
            "merge" => {
                let v = need(operand)?;
                let Some(e) = raw.first() else {
                    return Err(EvalError::new("`merge` needs a record to merge in"));
                };
                let other = self.eval(e)?;
                wrap(ops::merge(&v, &other))
            }
            // §10b's reductions. A bareword is a *column*; with none, the rows are the values.
            "sum" | "min" | "max" | "avg" => {
                let v = need(operand)?;
                let field = field_names.first().map(|s| s.as_str());
                wrap(match name {
                    "sum" => ops::sum(&v, field),
                    "min" => ops::min(&v, field),
                    "max" => ops::max(&v, field),
                    _ => ops::avg(&v, field),
                })
            }
            // `reduce` lives here, not in `ops`: it calls back into the interpreter (§5c).
            "reduce" => {
                let v = need(operand)?;
                let f = self.one_closure(&closures, name)?;
                let rows = ops::rows(&v).map_err(EvalError::new)?;
                // **The two forms differ precisely on the empty case**: seeded returns the seed,
                // unseeded has nothing to return.
                let mut acc = match raw.first() {
                    Some(e) => Some(self.eval(e)?),
                    None => None,
                };
                let mut rest = rows.into_iter();
                if acc.is_none() {
                    acc = Some(rest.next().ok_or_else(|| {
                        EvalError::new(
                            "`reduce` over nothing has no value — give it a starting value \
                             with `--from`, which is what makes the empty case answerable",
                        )
                    })?);
                }
                let mut acc = acc.expect("seeded above");
                for row in rest {
                    acc = self.call_closure(&f, &[acc, row])?;
                }
                Ok(acc)
            }
            "dedupe" => wrap(ops::dedupe(&need(operand)?)),
            "take" | "skip" | "last" => {
                let v = need(operand)?;
                let first = match raw.first() {
                    Some(e) => Some(self.eval(e)?),
                    None => None,
                };
                let n = match first.as_ref().and_then(|a| a.as_data()) {
                    Some(Value::Int(n)) => *n,
                    _ => {
                        return Err(EvalError::new(alloc::format!("`{name}` needs a row count")));
                    }
                };
                wrap(match name {
                    "take" => ops::take(&v, n),
                    "skip" => ops::skip(&v, n),
                    _ => ops::last(&v, n),
                })
            }
            "select" => {
                let v = need(operand)?;
                wrap(ops::select(&v, &field_names))
            }
            "sort" => {
                let v = need(operand)?;
                wrap(ops::sort(&v, &field_names, reverse))
            }
            "filter" => {
                let v = need(operand)?;
                let f = self.one_closure(&closures, name)?;
                let mut kept = Vec::new();
                for row in ops::rows(&v).map_err(EvalError::new)? {
                    let keep = self.call_closure(&f, core::slice::from_ref(&row))?;
                    match keep.as_bool() {
                        Some(true) => kept.push(row),
                        Some(false) => {}
                        None => {
                            return Err(EvalError::new(alloc::format!(
                                "a `filter` predicate must return Bool, got {}",
                                keep.type_name()
                            )));
                        }
                    }
                }
                wrap(ops::rebuild(&v, kept))
            }
            "map" => {
                let v = need(operand)?;
                let f = self.one_closure(&closures, name)?;
                let mut out = Vec::new();
                for row in ops::rows(&v).map_err(EvalError::new)? {
                    out.push(self.call_closure(&f, &[row])?);
                }
                wrap(ops::table_from_records(out))
            }
            "each" => {
                // `each` is for effects, so it returns its input unchanged.
                let v = need(operand)?;
                let f = self.one_closure(&closures, name)?;
                for row in ops::rows(&v).map_err(EvalError::new)? {
                    self.call_closure(&f, &[row])?;
                }
                Ok(v)
            }
            "format" => {
                let mut vals = Vec::with_capacity(raw.len() + 1);
                for e in &raw {
                    vals.push(self.eval(e)?);
                }
                // **In stage position the value flowing past is the subject**, argument 0: `… |
                // format("{}")` renders what arrived.
                if let Some(v) = operand {
                    vals.insert(1.min(vals.len()), v);
                }
                let Some(t) = vals.first() else {
                    return Err(EvalError::new("`format` needs a template"));
                };
                let template = t.render();
                wrap(ops::format(&template, &vals[1..]).map(Val::str))
            }
            "display" => {
                // §7 writes `display files` as well as `… | display`: an explicit operand stands
                // in.
                let v = match operand {
                    Some(v) => v,
                    None => match raw.first() {
                        Some(e) => self.eval(e)?,
                        None => need(None)?,
                    },
                };
                let styled = self.host.styled();
                let text = ops::display(&v, styled);
                self.host.out(&text);
                // A terminal operator ends the chain, so it yields Null rather than passing the
                // value on to be displayed twice.
                Ok(Val::NULL)
            }
            "save" => {
                let v = need(operand)?;
                let Some(path) = field_names.first().cloned() else {
                    return Err(EvalError::new("`save` needs a path"));
                };
                let path = self.resolve_path(&path)?;
                let bytes = ops::encode_for(&path, &v).map_err(EvalError::new)?;
                self.host.write_file(&path, &bytes).map_err(EvalError::new)?;
                Ok(Val::NULL)
            }
            "open" => {
                let paths = field_names.clone();
                if paths.is_empty() {
                    return Err(EvalError::new("`open` needs at least one path"));
                }
                // §4: several paths concatenate into one stream, which absorbs `cat`.
                let mut acc: Option<Val> = None;
                for path in &paths {
                    let path = self.resolve_path(path)?;
                    let bytes = self.host.read_file(&path).map_err(EvalError::new)?;
                    let v = ops::decode_from(&path, &bytes).map_err(EvalError::new)?;
                    acc = Some(match acc {
                        None => v,
                        Some(prev) => ops::concat(prev, v).map_err(EvalError::new)?,
                    });
                }
                Ok(acc.unwrap_or(Val::NULL))
            }
            _ => Err(unavailable(c)),
        }
    }

    fn one_closure(&mut self, closures: &[Val], name: &str) -> Result<Arc<Func>> {
        match closures.first() {
            Some(Val::Func(f)) => Ok(Arc::clone(f)),
            _ => Err(EvalError::new(alloc::format!(
                "`{name}` needs a predicate — either `{name} field > value` or \
                 `{name} {{ |row| … }}`"
            ))),
        }
    }

    /// Evaluate `a | b | c`.
    ///
    /// Stages are grouped into **runs of consecutive external commands**, each handed to the host
    /// as one connected chain: §1 makes the pipes between adjacent processes the kernel's business,
    /// with real backpressure, so the shell must not run one stage at a time. Between runs an
    /// in-process operator materialises the value (§5c).
    fn pipeline(&mut self, stages: &[Expr]) -> Result<Val> {
        let mut carried: Option<Val> = None;
        let mut statuses: Vec<StageStatus> = Vec::new();
        let mut i = 0usize;

        while i < stages.len() {
            if let Some(spec) = self.as_external(&stages[i], i == 0)? {
                let mut run = alloc::vec![spec];
                i += 1;
                while i < stages.len() {
                    match self.as_external(&stages[i], false)? {
                        Some(next) => {
                            run.push(next);
                            i += 1;
                        }
                        None => break,
                    }
                }
                let input = match carried.take() {
                    Some(v) => Some(encode_stream(&v)?),
                    None => None,
                };
                let strict = self.strict;
                let outcome = self.host.run(&run, input.as_deref(), strict, &self.env).map_err(EvalError::new)?;
                carried = decode_output(&outcome)?;
                statuses.extend(outcome.stages.iter().cloned());
                continue;
            }

            // An in-process stage: a generic operator on the `Value` tree, no spawn (§5c).
            if let Expr::Call(c) = &stages[i] {
                if matches!(c.kind, CallKind::Operator) {
                    carried = Some(self.apply_operator(c, carried.take())?);
                    i += 1;
                    continue;
                }
                if matches!(c.kind, CallKind::Def) {
                    carried = Some(self.call_def(c, carried.take())?);
                    i += 1;
                    continue;
                }
            }
            if let Expr::Ident(n) = &stages[i]
                && let Some(Val::Func(f)) = self.lookup_any(n)
            {
                let op = carried.take();
                carried = Some(self.call_function(&f, &[], &[], op, false)?);
                i += 1;
                continue;
            }
            // **A keyword stage** — `expect T`, `parse T`, `assert (P)` (§8c). They read the value
            // flowing past, so it is bound to `_` for the length of the stage and
            // `Expr::Underscore` picks it up; `assert (count > 0)` works because a nullary operator
            // finds `_` too.
            if matches!(&stages[i], Expr::Expect(..) | Expr::Parse(..) | Expr::Assert(_)) {
                let operand = carried.take().ok_or_else(|| {
                    EvalError::new(
                        "this stage checks the value arriving from the left of a pipe,                          and nothing is arriving",
                    )
                })?;
                self.push_scope();
                self.bind(UNDERSCORE, operand.clone(), false, false)?;
                let r = self.eval(&stages[i]);
                self.pop_scope();
                let v = r?;
                // `assert` is a check, not a transform: the value continues down the chain.
                carried = Some(if matches!(&stages[i], Expr::Assert(_)) { operand } else { v });
                i += 1;
                continue;
            }
            if i == 0 && !matches!(&stages[0], Expr::Call(_)) {
                carried = Some(self.eval(&stages[0])?);
                i += 1;
                continue;
            }
            return Err(match &stages[i] {
                Expr::Call(c) => unavailable(c),
                other => EvalError::new(alloc::format!("a {} cannot be a pipeline stage", describe_expr(other))),
            });
        }

        // §1/§2: the pipeline *is* an expression, so its data value is what a `let` binds.
        // `PipelineStatus` is orthogonal metadata (there is no global `$?`, §11d).
        self.last_status = Some(pipeline_status(&statuses));

        // **An interrupted pipeline reports as interrupted** (§11h): the host noticed the terminal
        // while waiting and asked the stages to stop; without this the shell reports `^C` only at
        // its next prompt, for something that cut a command short.
        self.check_interrupt()?;

        // Fail loud (§1): a non-zero or crashed stage anywhere fails the pipeline. Downstream
        // stages are *not* torn down, so this is checked after the run rather than during it.
        if let Some(bad) = statuses.iter().find(|s| !s.succeeded()) {
            let mut msg = alloc::string::String::from("pipeline failed: `");
            msg.push_str(&bad.command);
            msg.push('`');
            if bad.crashed {
                msg.push_str(" crashed");
            } else if bad.cancelled {
                msg.push_str(" was cancelled by `strict`");
            } else {
                msg.push_str(" exited ");
                msg.push_str(&render_i64(bad.exit_status as i64));
            }
            // The per-stage report travels on the error, the one thing that reaches the `catch`
            // (§2).
            let mut e = EvalError::of(PIPELINE_FAILED, msg);
            e.stages = Some(stage_rows(&statuses));
            return Err(e);
        }
        Ok(carried.unwrap_or(Val::NULL))
    }

    /// If this stage is an external command, build its [`StageSpec`].
    ///
    /// D4 where it matters: a stage with a piped operand is unambiguously an *invocation*, so a
    /// command name is not shadowed by a local binding here, the opposite of [`Interp::eval`]'s
    /// bare-reference rule and for the opposite reason.
    fn as_external(&mut self, stage: &Expr, at_head: bool) -> Result<Option<StageSpec>> {
        // A bare name in stage position is a command, except at the *head*, where it may be the
        // value being piped onward (D4).
        if let Expr::Ident(name) = stage {
            // A bound *function* named bare in a stage is a call: `ls | summarize` (§5b).
            if matches!(self.lookup_any(name), Some(Val::Func(_))) {
                return Ok(None);
            }
            if at_head && self.lookup(name).is_some() {
                return Ok(None);
            }
            return Ok(Some(StageSpec { program: name.clone(), argv: alloc::vec![name.clone()] }));
        }
        let Expr::Call(c) = stage else { return Ok(None) };
        if c.kind != CallKind::External {
            return Ok(None);
        }
        let mut argv = alloc::vec![c.name.clone()];
        for a in &c.args {
            match a {
                Arg::Positional(e) => argv.push(self.eval(e)?.render()),
                Arg::Flag(f, None) => {
                    let mut s = alloc::string::String::from("--");
                    s.push_str(f);
                    argv.push(s);
                }
                Arg::Flag(f, Some(e)) => {
                    let mut s = alloc::string::String::from("--");
                    s.push_str(f);
                    argv.push(s);
                    argv.push(self.eval(e)?.render());
                }
                Arg::ShortFlags(f) => {
                    let mut s = alloc::string::String::from("-");
                    s.push_str(f);
                    argv.push(s);
                }
                Arg::Named(n, _) => {
                    return Err(EvalError::new(alloc::format!(
                        "`{}` is an external program, so `{n}:` named arguments do not \
                         apply — those are the `def` calling convention (§5b)",
                        c.name
                    )));
                }
                Arg::PipeFill => {
                    return Err(EvalError::new(alloc::format!(
                        "`{}` is an external program and takes its input from the pipe, \
                         so `_` is neither needed nor allowed (§5b)",
                        c.name
                    )));
                }
            }
        }
        Ok(Some(StageSpec { program: c.name.clone(), argv }))
    }

    fn binary(&mut self, op: BinOp, a: &Expr, b: &Expr) -> Result<Val> {
        match op {
            BinOp::And => {
                let l = self.eval(a)?;
                let lb = bool_operand(&l, "&&")?;
                if !lb {
                    return Ok(Val::bool(false));
                }
                let r = self.eval(b)?;
                return Ok(Val::bool(bool_operand(&r, "&&")?));
            }
            BinOp::Or => {
                let l = self.eval(a)?;
                if bool_operand(&l, "||")? {
                    return Ok(Val::bool(true));
                }
                let r = self.eval(b)?;
                return Ok(Val::bool(bool_operand(&r, "||")?));
            }
            BinOp::Coalesce => {
                let l = self.eval(a)?;
                if !l.is_null() {
                    return Ok(l);
                }
                return self.eval(b);
            }
            _ => {}
        }
        let l = self.eval(a)?;
        let r = self.eval(b)?;
        binary_values(op, l, r)
    }
}

/// "That command cannot run yet": names the category so a script written against the finished
/// language fails informatively against a half-built one.
fn unavailable(c: &Call) -> EvalError {
    EvalError::new(alloc::format!(
        "`{}` cannot run yet: {} arrive in {}",
        c.name,
        match c.kind {
            CallKind::Operator => "generic operators",
            CallKind::Builtin => "shell builtins",
            CallKind::Def => "function calls",
            CallKind::External => "external programs",
        },
        match c.kind {
            CallKind::Operator => "Part D",
            CallKind::Def => "Part E",
            _ => "Part C",
        }
    ))
}

/// Encode a value as a TSM1 stream for a stage's `stdin`.
///
/// **Text is wrapped rather than refused** (`StreamFlags::TEXT_FALLBACK`): a `String` becomes a
/// one-column stream with a row per line, and so does a `List` of them: the spec's "Unix floor".
/// **Only those two shapes**: inventing a rendering for a `List` of Ints would put a display
/// decision in the transport.
fn encode_stream(v: &Val) -> Result<Vec<u8>> {
    if let Some(bytes) = encode_text_stream(v) {
        return bytes.map_err(|_| EvalError::new("could not encode the piped text"));
    }
    let Val::Data(Value::Table(t)) = v else {
        return Err(EvalError::new(alloc::format!(
            "only a Table or text can be piped into a program, got {} — a program reads a \
             stream, and a stream is a table",
            v.type_name()
        )));
    };
    let mut buf: Vec<u8> = Vec::new();
    t.encode(&mut buf).map_err(|_| EvalError::new("could not encode the piped table"))?;
    Ok(buf)
}

/// `Some` if `v` is text, a `String` or a `List` of them, encoded as a text-fallback stream.
///
/// `None` is "not text", distinct from "text that would not encode": the caller falls through to
/// the table path for the first and reports the second.
fn encode_text_stream(v: &Val) -> Option<core::result::Result<Vec<u8>, libstream::wire::WireError>> {
    let lines: Vec<&str> = match v {
        Val::Data(Value::Str(s)) => s.split('\n').collect(),
        Val::Data(Value::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for i in items.iter() {
                match i {
                    Value::Str(s) => out.push(s.as_str()),
                    // One non-string element means not a list of text; falling through beats a
                    // partial encode that silently loses data.
                    _ => return None,
                }
            }
            out
        }
        _ => return None,
    };
    let mut buf: Vec<u8> = Vec::new();
    Some(libstream::table::write_text_fallback(&mut buf, &lines, 0).map(|()| buf))
}

/// Decode a stage run's captured output into a value.
///
/// Absent output is `Null`, not an empty table: a mutation like `remove` has no value, and an empty
/// table would let `| count` answer 0 for something that never counted.
fn decode_output(run: &PipelineRun) -> Result<Option<Val>> {
    let Some(bytes) = &run.output else { return Ok(None) };
    if bytes.is_empty() {
        return Ok(None);
    }
    let t = libstream::wire::Table::decode(bytes)
        .map_err(|_| EvalError::new("a stage produced output that is not a TSM1 stream"))?;
    Ok(Some(Val::Data(Value::Table(Arc::new(t)))))
}

/// The §1 per-stage rows: `List<{command, exit_status, crashed, cancelled}>`.
///
/// One definition, used by `$last.status` and the error a failed pipeline raises, so the two cannot
/// describe the same run differently.
fn stage_rows(stages: &[StageStatus]) -> Value {
    let mut rows: Vec<Value> = Vec::with_capacity(stages.len());
    for s in stages {
        let schema = Schema::new()
            .field("command", libstream::wire::TypeTag::String, TypeModifiers::NONE)
            .field("exit_status", libstream::wire::TypeTag::Int, TypeModifiers::NONE)
            .field("crashed", libstream::wire::TypeTag::Bool, TypeModifiers::NONE)
            .field("cancelled", libstream::wire::TypeTag::Bool, TypeModifiers::NONE);
        rows.push(Value::Record(Arc::new(Record {
            schema,
            values: alloc::vec![
                Value::Str(s.command.clone()),
                Value::Int(s.exit_status as i64),
                Value::Bool(s.crashed),
                Value::Bool(s.cancelled),
            ],
        })));
    }
    Value::List(Arc::from(rows))
}

fn pipeline_status(stages: &[StageStatus]) -> Val {
    let all_ok = stages.iter().all(|s| s.succeeded());
    let schema = Schema::new().field("stages", libstream::wire::TypeTag::List, TypeModifiers::NONE).field(
        "all_ok",
        libstream::wire::TypeTag::Bool,
        TypeModifiers::NONE,
    );
    Val::Data(Value::Record(Arc::new(Record { schema, values: alloc::vec![stage_rows(stages), Value::Bool(all_ok)] })))
}

fn describe_expr(e: &Expr) -> &'static str {
    match e {
        Expr::Closure { .. } => "closure",
        Expr::Int(_) | Expr::Float(_) | Expr::Str(_) | Expr::Word(_) => "literal",
        Expr::Ident(_) => "name",
        _ => "value",
    }
}

/// The name `_` is bound to inside a keyword stage (§8c): a real binding, so `expect`/`parse` reach
/// it through `Expr::Underscore` and a nullary operator with no piped operand finds it.
const UNDERSCORE: &str = "_";

/// Guard against a runaway `while` or an absurd range; a backstop, not a resource policy.
const MAX_ITERATIONS: u64 = 10_000_000;

enum Step {
    Field(String),
    Index(IndexKey),
}

enum IndexKey {
    Pos(i64),
    Name(String),
}

/// Walk `steps` into `data` and replace the leaf.
///
/// Rebuilds rather than mutating in place: `Value`'s collections are `Arc`-shared, so another
/// binding holding the same list must not see the write. `Arc::make_mut` copies when needed.
fn set_path(data: &mut Value, steps: &[Step], value: Value) -> Result<()> {
    let Some((first, rest)) = steps.split_first() else {
        *data = value;
        return Ok(());
    };
    match (first, data) {
        (Step::Field(name), Value::Record(rec)) => {
            let r = Arc::make_mut(rec);
            let idx = r
                .schema
                .fields
                .iter()
                .position(|f| &f.name == name)
                .ok_or_else(|| EvalError::new(alloc::format!("record has no field `{name}`")))?;
            set_path(&mut r.values[idx], rest, value)
        }
        (Step::Index(IndexKey::Name(name)), Value::Record(rec)) => {
            let r = Arc::make_mut(rec);
            let idx = r
                .schema
                .fields
                .iter()
                .position(|f| &f.name == name)
                .ok_or_else(|| EvalError::new(alloc::format!("record has no field `{name}`")))?;
            set_path(&mut r.values[idx], rest, value)
        }
        (Step::Index(IndexKey::Pos(i)), Value::List(items)) => {
            let mut v: Vec<Value> = items.to_vec();
            let idx = bounded_index(*i, v.len())?;
            set_path(&mut v[idx], rest, value)?;
            *items = Arc::from(v);
            Ok(())
        }
        (Step::Field(name), other) => Err(EvalError::new(alloc::format!(
            "cannot set field `{name}` on a {}",
            Val::Data(other.clone()).type_name()
        ))),
        (Step::Index(_), other) => {
            Err(EvalError::new(alloc::format!("cannot index into a {}", Val::Data(other.clone()).type_name())))
        }
    }
}

fn bounded_index(i: i64, len: usize) -> Result<usize> {
    if i < 0 || i as usize >= len {
        return Err(EvalError::new(alloc::format!(
            "index {} is out of range for a length of {}",
            render_i64(i),
            render_i64(len as i64)
        )));
    }
    Ok(i as usize)
}

/// `x.field`. `lenient` is the `?.` form: `Null` for a missing field, not a failure (§9e).
fn field_of(base: &Val, name: &str, lenient: bool) -> Result<Val> {
    match base {
        Val::Data(Value::Record(r)) => match r.schema.fields.iter().position(|f| f.name == name) {
            Some(i) => Ok(Val::Data(r.values.get(i).cloned().unwrap_or(Value::Null))),
            None if lenient => Ok(Val::NULL),
            None => Err(EvalError::new(alloc::format!(
                "record has no field `{name}` — it has [{}]",
                field_names(&r.schema)
            ))),
        },
        other if lenient => {
            let _ = other;
            Ok(Val::NULL)
        }
        other => Err(EvalError::new(alloc::format!("cannot read field `{name}` from a {}", other.type_name()))),
    }
}

fn field_names(s: &Schema) -> String {
    let mut out = String::new();
    for (i, f) in s.fields.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&f.name);
    }
    out
}

fn index_of(base: &Val, key: &IndexKey) -> Result<Val> {
    match (base, key) {
        (Val::Data(Value::List(items)), IndexKey::Pos(i)) => {
            let idx = bounded_index(*i, items.len())?;
            Ok(Val::Data(items[idx].clone()))
        }
        (Val::Data(Value::Table(t)), IndexKey::Pos(i)) => {
            let idx = bounded_index(*i, t.rows.len())?;
            Ok(Val::Data(Value::Record(Arc::new(Record { schema: t.schema.clone(), values: t.rows[idx].clone() }))))
        }
        (Val::Data(Value::Record(_)), IndexKey::Name(n)) => field_of(base, n, false),
        (Val::Data(Value::Str(s)), IndexKey::Pos(i)) => {
            let chars: Vec<char> = s.chars().collect();
            let idx = bounded_index(*i, chars.len())?;
            Ok(Val::str(chars[idx].to_string()))
        }
        (b, IndexKey::Pos(_)) => Err(EvalError::new(alloc::format!("cannot index a {} by position", b.type_name()))),
        (b, IndexKey::Name(_)) => Err(EvalError::new(alloc::format!("cannot index a {} by name", b.type_name()))),
    }
}

/// The value of a block that has run to the end of a **function, closure or script**.
///
/// `break`/`continue` cannot arrive here: the parser refuses them outside a loop body and resets
/// its loop count at every `def` and closure (§9c). The arm states that invariant, so a tree built
/// without the parser gets a sentence rather than a silent `Null`.
fn boundary_value(flow: Flow) -> Result<Val> {
    match flow {
        Flow::Normal(v) | Flow::Return(v) => Ok(v),
        Flow::Break | Flow::Continue => Err(EvalError::new(
            "`break` and `continue` are only meaningful inside a `for` or `while` body, \
             and they do not cross out of a `def` or a closure",
        )),
    }
}

/// The value of a block used **in expression position** — `let x = { … }`, an `if` whose value is
/// taken, a `match` arm.
///
/// A `break` here is legal per §9c yet cannot work: `eval` returns a *value*, with no channel for
/// control flow. Refused with the fix in the message. `return` hits the same wall and *silently*
/// yields its operand instead of leaving the function (a divergence from §5b); both are filed as
/// `TODO(control-flow-in-expression-position)`.
fn expression_value(flow: Flow) -> Result<Val> {
    match flow {
        Flow::Normal(v) | Flow::Return(v) => Ok(v),
        Flow::Break | Flow::Continue => Err(EvalError::new(
            "`break` and `continue` cannot be used where a value is expected — this \
             block's value is being taken, and they produce none. Put it in statement \
             position instead: `if done { break }` on a line of its own",
        )),
    }
}

/// `parse T` (§6) — conversion. `expect T` asserts a value already *is* a `T`; this reads one *as*
/// a `T`. Fail-loud on anything it cannot read, since a silent coercion is unrecoverable.
fn parse_value(v: Val, t: &TypeExpr) -> Result<Val> {
    let bad = |m: String| EvalError::of(PARSE_ERROR, m);
    let (name, nullable) = match t {
        TypeExpr::Named { name, nullable, .. } => (name.as_str(), *nullable),
        TypeExpr::Record { .. } => {
            return Err(EvalError::of(
                PARSE_ERROR,
                "`parse` converts to a scalar type — checking a record *shape* is what \
                 `expect` is for (§6)",
            ));
        }
    };
    if v.is_null() {
        return if nullable {
            Ok(Val::NULL)
        } else {
            Err(bad(alloc::format!(
                "cannot parse Null as {name} — annotate `{name}?` if the value is \
                 genuinely optional"
            )))
        };
    }
    // Already a `T`: the identity case.
    if v.type_name() == name {
        return Ok(v);
    }
    let got = v.type_name();
    match (v.as_data(), name) {
        (Some(Value::Str(s)), "Int" | "Float") => {
            // §6 is strict about surrounding whitespace *because* `trim` exists.
            if s.trim() != s.as_str() {
                return Err(bad(alloc::format!(
                    "cannot parse {s:?} as {name} — it has surrounding whitespace; \
                     `trim` it first"
                )));
            }
            match (scan_number(s), name) {
                (Some(Val::Data(Value::Int(i))), "Int") => Ok(Val::int(i)),
                (Some(Val::Data(Value::Int(i))), "Float") => Ok(Val::float(i as f64)),
                (Some(Val::Data(Value::Float(f))), "Float") => Ok(Val::float(f)),
                // `Int` and `Float` are different types (§6); truncating `"3.5" | parse Int` would
                // be a silent coercion.
                (Some(Val::Data(Value::Float(_))), "Int") => Err(bad(alloc::format!(
                    "cannot parse {s:?} as Int — it reads as a Float; `parse Float` it, or \
                     round it"
                ))),
                _ => Err(bad(alloc::format!("cannot parse {s:?} as {name}"))),
            }
        }
        // No `1`/`yes`/`on`: that list is where configuration languages went wrong; `==` covers the
        // rest.
        (Some(Value::Str(s)), "Bool") => match s.as_str() {
            "true" => Ok(Val::bool(true)),
            "false" => Ok(Val::bool(false)),
            _ => Err(bad(alloc::format!("cannot parse {s:?} as Bool — only \"true\" and \"false\""))),
        },
        (Some(Value::Bytes(b)), "String") => match core::str::from_utf8(b) {
            Ok(s) => Ok(Val::str(s)),
            Err(e) => Err(bad(alloc::format!(
                "cannot parse these bytes as String — not UTF-8 at byte {}",
                render_i64(e.valid_up_to() as i64)
            ))),
        },
        (Some(Value::Int(i)), "Float") => {
            let f = *i as f64;
            // Widening is lossy past 2^53, a fabricated number (§8a).
            if f as i64 != *i {
                return Err(bad(alloc::format!("cannot parse {} as Float without losing precision", render_i64(*i))));
            }
            Ok(Val::float(f))
        }
        (_, "String") => Err(bad(alloc::format!(
            "cannot parse {got} as String — rendering a value as text is `format(\"{{}}\", …)` \
             (§8d)"
        ))),
        _ => Err(bad(alloc::format!("cannot parse {got} as {name}"))),
    }
}

/// Read a numeric literal — **by running the lexer over it**.
///
/// §6: what a number looks like to `parse` is what it looks like to the lexer, so radix prefixes,
/// `_` separators, exponents and the no-octal rule stay in step (§8e). The sign is handled here
/// because a literal is unsigned in the grammar, which also means `parse Int` cannot read
/// `i64::MIN`, as no literal can write it.
fn scan_number(text: &str) -> Option<Val> {
    use crate::lex::{Lexer, Tok};
    match Lexer::tokenize_expr(text).ok()?.as_slice() {
        [Tok::Int(i), Tok::Eof] => Some(Val::int(*i)),
        [Tok::Float(f), Tok::Eof] => Some(Val::float(*f)),
        [Tok::Minus, Tok::Int(i), Tok::Eof] => Some(Val::int(-*i)),
        [Tok::Minus, Tok::Float(f), Tok::Eof] => Some(Val::float(-*f)),
        _ => None,
    }
}

fn bool_operand(v: &Val, op: &str) -> Result<bool> {
    v.as_bool().ok_or_else(|| {
        EvalError::new(alloc::format!(
            "`{op}` needs Bool operands, got {} — there is no truthiness here",
            v.type_name()
        ))
    })
}

fn unary(op: UnOp, v: Val) -> Result<Val> {
    match (op, &v) {
        (UnOp::Neg, Val::Data(Value::Int(i))) => {
            i.checked_neg().map(Val::int).ok_or_else(|| EvalError::new("negation overflows an Int"))
        }
        (UnOp::Neg, Val::Data(Value::Float(f))) => Ok(Val::float(-f)),
        (UnOp::Not, Val::Data(Value::Bool(b))) => Ok(Val::bool(!b)),
        (UnOp::Neg, other) => Err(EvalError::new(alloc::format!("cannot negate a {}", other.type_name()))),
        (UnOp::Not, other) => Err(EvalError::new(alloc::format!("`!` needs a Bool, got {}", other.type_name()))),
    }
}

fn binary_values(op: BinOp, l: Val, r: Val) -> Result<Val> {
    use BinOp::*;
    match op {
        Eq => return Ok(Val::bool(values_equal(&l, &r))),
        Ne => return Ok(Val::bool(!values_equal(&l, &r))),
        // §8a: `++` is string concatenation, spelled apart from `+`.
        Concat => {
            let mut s = l.render();
            s.push_str(&r.render());
            return Ok(Val::str(s));
        }
        // §8a/§10b. Membership is an infix comparison because that is where the question is asked,
        // inside an `if`; a pipeline `contains` would be a second spelling.
        In => {
            return match r.as_data() {
                Some(Value::Str(text)) => {
                    let Some(Value::Str(needle)) = l.as_data() else {
                        return Err(EvalError::new(alloc::format!(
                            "`in` over a String needs a String on the left, got {}",
                            l.type_name()
                        )));
                    };
                    Ok(Val::bool(text.contains(needle.as_str())))
                }
                // A Record's membership is over its *field names*: `"size" in row` (§10b).
                Some(Value::Record(rec)) => {
                    let Some(Value::Str(name)) = l.as_data() else {
                        return Err(EvalError::new(alloc::format!(
                            "`in` over a Record tests a field name, so it needs a String \
                             on the left, got {}",
                            l.type_name()
                        )));
                    };
                    Ok(Val::bool(rec.schema.fields.iter().any(|f| &f.name == name)))
                }
                _ => match &r {
                    // A Range answers without materialising millions of values.
                    Val::Range { start, end, inclusive } => {
                        let Some(Value::Int(n)) = l.as_data() else {
                            return Err(EvalError::new(alloc::format!(
                                "`in` over a Range needs an Int on the left, got {}",
                                l.type_name()
                            )));
                        };
                        Ok(Val::bool(*n >= *start && (*n < *end || (*inclusive && *n == *end))))
                    }
                    other => {
                        let items = ops::rows(other).map_err(EvalError::new)?;
                        Ok(Val::bool(items.iter().any(|i| values_equal(i, &l))))
                    }
                },
            };
        }
        Match => {
            let (Some(Value::Str(text)), Some(Value::Str(pattern))) = (l.as_data(), r.as_data()) else {
                return Err(EvalError::new(alloc::format!(
                    "`~=` matches a String against a pattern, got {} and {}",
                    l.type_name(),
                    r.type_name()
                )));
            };
            let re = crate::regex::Regex::new(pattern).map_err(EvalError::new)?;
            return Ok(Val::bool(re.is_match(text)));
        }
        And | Or | Coalesce => unreachable!("short-circuited before evaluation"),
        _ => {}
    }

    let (a, b) = match (l.as_data(), r.as_data()) {
        (Some(a), Some(b)) => (a, b),
        _ => {
            return Err(EvalError::new(alloc::format!(
                "cannot apply {} to {} and {}",
                op_name(op),
                l.type_name(),
                r.type_name()
            )));
        }
    };

    // Strings compare but do no arithmetic; `+` on two strings is a common slip, so its message
    // points at `++`.
    if let (Value::Str(x), Value::Str(y)) = (a, b) {
        return match op {
            Lt => Ok(Val::bool(x < y)),
            Le => Ok(Val::bool(x <= y)),
            Gt => Ok(Val::bool(x > y)),
            Ge => Ok(Val::bool(x >= y)),
            Add => Err(EvalError::new(
                "`+` does not join strings — use `++` (§8a), which is a separate operator \
                 so that arithmetic and concatenation never look alike",
            )),
            _ => Err(EvalError::new(alloc::format!("cannot apply {} to two Strings", op_name(op)))),
        };
    }

    match (a, b) {
        (Value::Int(x), Value::Int(y)) => int_op(op, *x, *y),
        (Value::Float(x), Value::Float(y)) => float_op(op, *x, *y),
        // Mixed arithmetic widens to Float, the only choice that loses no information.
        (Value::Int(x), Value::Float(y)) => float_op(op, *x as f64, *y),
        (Value::Float(x), Value::Int(y)) => float_op(op, *x, *y as f64),
        _ => Err(EvalError::new(alloc::format!(
            "cannot apply {} to {} and {}",
            op_name(op),
            l.type_name(),
            r.type_name()
        ))),
    }
}

fn int_op(op: BinOp, x: i64, y: i64) -> Result<Val> {
    use BinOp::*;
    let arith = |o: Option<i64>| {
        o.map(Val::int).ok_or_else(|| {
            EvalError::new(alloc::format!(
                "{} overflows an Int — Nitrox does not wrap, because a wrapped result is \
                 a fabricated number",
                op_name(op)
            ))
        })
    };
    match op {
        Add => arith(x.checked_add(y)),
        Sub => arith(x.checked_sub(y)),
        Mul => arith(x.checked_mul(y)),
        Div => {
            if y == 0 {
                return Err(EvalError::new("division by zero"));
            }
            arith(x.checked_div(y))
        }
        Rem => {
            if y == 0 {
                return Err(EvalError::new("remainder by zero"));
            }
            arith(x.checked_rem(y))
        }
        Lt => Ok(Val::bool(x < y)),
        Le => Ok(Val::bool(x <= y)),
        Gt => Ok(Val::bool(x > y)),
        Ge => Ok(Val::bool(x >= y)),
        _ => Err(EvalError::new(alloc::format!("cannot apply {} to two Ints", op_name(op)))),
    }
}

fn float_op(op: BinOp, x: f64, y: f64) -> Result<Val> {
    use BinOp::*;
    match op {
        Add => Ok(Val::float(x + y)),
        Sub => Ok(Val::float(x - y)),
        Mul => Ok(Val::float(x * y)),
        Div => {
            if y == 0.0 {
                return Err(EvalError::new(
                    "division by zero — this yields no value rather than `inf`, which \
                     would be a fabricated one",
                ));
            }
            Ok(Val::float(x / y))
        }
        Rem => {
            if y == 0.0 {
                return Err(EvalError::new("remainder by zero"));
            }
            Ok(Val::float(x % y))
        }
        Lt => Ok(Val::bool(x < y)),
        Le => Ok(Val::bool(x <= y)),
        Gt => Ok(Val::bool(x > y)),
        Ge => Ok(Val::bool(x >= y)),
        _ => Err(EvalError::new(alloc::format!("cannot apply {} to two Floats", op_name(op)))),
    }
}

/// Structural equality. `Int(1)` and `Float(1.0)` compare equal *numerically* but still render and
/// ascribe differently.
fn values_equal(l: &Val, r: &Val) -> bool {
    match (l.as_data(), r.as_data()) {
        (Some(Value::Int(a)), Some(Value::Float(b))) | (Some(Value::Float(b)), Some(Value::Int(a))) => *a as f64 == *b,
        _ => l == r,
    }
}

fn op_name(op: BinOp) -> &'static str {
    match op {
        BinOp::Add => "`+`",
        BinOp::Sub => "`-`",
        BinOp::Mul => "`*`",
        BinOp::Div => "`/`",
        BinOp::Rem => "`%`",
        BinOp::Concat => "`++`",
        BinOp::Lt => "`<`",
        BinOp::Le => "`<=`",
        BinOp::Gt => "`>`",
        BinOp::Ge => "`>=`",
        BinOp::Eq => "`==`",
        BinOp::Ne => "`!=`",
        BinOp::Match => "`~=`",
        BinOp::In => "`in`",
        BinOp::And => "`&&`",
        BinOp::Or => "`||`",
        BinOp::Coalesce => "`??`",
    }
}

/// Check a value against an ascription (§6). Scalars only; `Table<{…}>` against a stream header is
/// not checked here.
fn check_type(v: &Val, t: &TypeExpr) -> Result<()> {
    let (name, nullable) = match t {
        TypeExpr::Named { name, nullable, .. } => (name.as_str(), *nullable),
        // A record shape would need the field-by-field comparison behind §6's schema diff.
        TypeExpr::Record { nullable, .. } => {
            if v.is_null() && *nullable {
                return Ok(());
            }
            return match v {
                Val::Data(Value::Record(_)) => Ok(()),
                other => {
                    Err(EvalError::of(TYPE_ERROR, alloc::format!("expected a record shape, got {}", other.type_name())))
                }
            };
        }
    };
    if v.is_null() {
        // §9e: a non-nullable annotation rejects `Null` outright, so a false `if` without `else` is
        // caught at the binding.
        return if nullable {
            Ok(())
        } else {
            Err(EvalError::of(
                TYPE_ERROR,
                alloc::format!(
                    "expected {name}, got Null — annotate `{name}?` if the value is genuinely \
                 optional"
                ),
            ))
        };
    }
    let actual = v.type_name();
    // Subset match by default (§6): a container annotation names the container only.
    if actual == name {
        return Ok(());
    }
    Err(EvalError::of(TYPE_ERROR, alloc::format!("expected {name}, got {actual}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Val {
        match Interp::eval_str(src) {
            Ok(v) => v,
            Err(e) => panic!("evaluation failed: {}", e.message),
        }
    }

    fn err(src: &str) -> String {
        match Interp::eval_str(src) {
            Ok(v) => panic!("expected an error, got {}", v.render()),
            Err(e) => e.message,
        }
    }

    fn rendered(src: &str) -> String {
        ok(src).render()
    }

    #[test]
    fn arithmetic_follows_the_precedence_table() {
        assert_eq!(rendered("1 + 2 * 3"), "7");
        assert_eq!(rendered("(1 + 2) * 3"), "9");
        assert_eq!(rendered("7 / 2"), "3");
        assert_eq!(rendered("7 % 2"), "1");
        assert_eq!(rendered("-3 + 1"), "-2");
    }

    #[test]
    fn mixed_int_and_float_widens_to_float() {
        assert_eq!(rendered("1 + 1.5"), "2.5");
        assert_eq!(rendered("3 / 2.0"), "1.5");
        assert_eq!(rendered("2.0 * 2"), "4.0");
    }

    /// Arithmetic overflow and division by zero are errors, never wrapped or fabricated values.
    #[test]
    fn overflow_and_division_by_zero_are_errors_not_wrapped_values() {
        assert!(err("9223372036854775807 + 1").contains("overflows"));
        assert!(err("1 / 0").contains("division by zero"));
        assert!(err("1.0 / 0.0").contains("division by zero"));
        assert!(err("1 % 0").contains("remainder by zero"));
    }

    /// `+` and `++` are separate (§8a): `+` on strings earns a message pointing at `++`.
    #[test]
    fn plus_does_not_join_strings() {
        assert_eq!(rendered("\"a\" ++ \"b\""), "ab");
        assert!(err("\"a\" + \"b\"").contains("`++`"));
        assert_eq!(rendered("\"n=\" ++ 5"), "n=5");
    }

    #[test]
    fn comparison_works_on_numbers_and_strings() {
        assert_eq!(rendered("1 < 2"), "true");
        assert_eq!(rendered("2.5 >= 2"), "true");
        assert_eq!(rendered("\"a\" < \"b\""), "true");
        assert_eq!(rendered("1 == 1"), "true");
        assert_eq!(rendered("1 != 2"), "true");
    }

    #[test]
    fn int_and_float_compare_numerically() {
        assert_eq!(rendered("1 == 1.0"), "true");
        assert_eq!(rendered("1 != 1.0"), "false");
        assert_eq!(rendered("1"), "1");
        assert_eq!(rendered("1.0"), "1.0");
    }

    #[test]
    fn there_is_no_truthiness() {
        let e = err("if 1 { 2 }");
        assert!(e.contains("Bool"), "{e}");
        assert!(e.contains("no truthiness"), "{e}");
        assert!(err("true && 1").contains("no truthiness"));
        assert_eq!(rendered("if 1 != 0 { 2 } else { 3 }"), "2");
    }

    #[test]
    fn logical_operators_short_circuit() {
        assert_eq!(rendered("false && (1 / 0) == 0"), "false");
        assert_eq!(rendered("true || (1 / 0) == 0"), "true");
    }

    #[test]
    fn null_coalescing_short_circuits_too() {
        assert_eq!(rendered("null ?? 5"), "5");
        assert_eq!(rendered("3 ?? (1 / 0)"), "3");
    }

    #[test]
    fn let_is_immutable_and_mut_is_not() {
        assert_eq!(rendered("let x = 5\nx"), "5");
        assert_eq!(rendered("mut x = 5\nx = 6\nx"), "6");
        let e = err("let x = 5\nx = 6");
        assert!(e.contains("`let`"), "{e}");
        assert!(e.contains("mut"), "{e}");
    }

    /// An operator name is not a reserved word: `last` is both, and assigning to it is not a call.
    #[test]
    fn an_operator_name_can_still_be_a_variable() {
        assert_eq!(rendered("mut last = 1\nlast = 2\nlast"), "2");
        assert_eq!(rendered("let count = 7\ncount"), "7");
    }

    #[test]
    fn a_const_cannot_be_rebound_in_its_scope() {
        assert!(err("const X = 1\nconst X = 2").contains("const"));
        assert!(err("const X = 1\nX = 2").contains("only a `mut`"));
    }

    /// D4: a bare name is looked for as a binding and then as a program; a failure names both.
    #[test]
    fn an_unbound_name_says_what_was_searched() {
        let e = err("nope");
        assert!(e.contains("not a binding"), "{e}");
        assert!(e.contains("as a program"), "{e}");
    }

    #[test]
    fn a_block_evaluates_to_its_last_expression() {
        assert_eq!(rendered("if true { 1 + 1 }"), "2");
        assert_eq!(rendered("if true { let x = 1 }"), "null");
        assert_eq!(rendered("if false { 1 }"), "null");
    }

    #[test]
    fn if_else_chains_pick_the_branch_that_ran() {
        assert_eq!(rendered("if false { 1 } else if true { 2 } else { 3 }"), "2");
        assert_eq!(rendered("if false { 1 } else if false { 2 } else { 3 }"), "3");
    }

    /// §9e: the nullable suffix defuses if-without-else, if a non-nullable annotation rejects `Null`.
    #[test]
    fn ascription_rejects_null_unless_the_type_is_nullable() {
        assert_eq!(rendered("let x: Int? = if false { 5 }\nx"), "null");
        let e = err("let x: Int = if false { 5 }");
        assert!(e.contains("got Null"), "{e}");
        assert!(e.contains("Int?"), "{e}");
        assert!(err("let x: Int = \"s\"").contains("expected Int, got String"));
    }

    #[test]
    fn inner_scopes_shadow_and_do_not_leak() {
        assert_eq!(rendered("let x = 1\nif true { let x = 2 }\nx"), "1");
        assert!(err("if true { let y = 2 }\ny").contains("not a binding"));
    }

    #[test]
    fn for_walks_ranges_lists_and_accumulates() {
        assert_eq!(rendered("mut n = 0\nfor i in 0..5 { n = n + i }\nn"), "10");
        assert_eq!(rendered("mut n = 0\nfor i in 0..=5 { n = n + i }\nn"), "15");
        assert_eq!(rendered("mut n = 0\nfor v in [1, 2, 3] { n = n + v }\nn"), "6");
    }

    #[test]
    fn while_loops_until_the_condition_fails() {
        assert_eq!(rendered("mut n = 0\nwhile n < 5 { n = n + 1 }\nn"), "5");
    }

    #[test]
    fn iterating_a_non_sequence_is_an_error() {
        let e = err("for x in 5 { x }");
        assert!(e.contains("cannot iterate"), "{e}");
    }

    #[test]
    fn lists_and_records_build_and_read_back() {
        assert_eq!(rendered("[1, 2, 3]"), "[1, 2, 3]");
        assert_eq!(rendered("[1, 2, 3][1]"), "2");
        assert_eq!(rendered("{ a: 1, b: \"x\" }"), "{ a: 1, b: \"x\" }");
        assert_eq!(rendered("{ a: 1 }.a"), "1");
        assert_eq!(rendered("let name = \"n\"\n{ name }"), "{ name: \"n\" }");
    }

    #[test]
    fn an_out_of_range_index_names_the_range() {
        let e = err("[1, 2][5]");
        assert!(e.contains("out of range"), "{e}");
        assert!(e.contains("length of 2"), "{e}");
    }

    #[test]
    fn a_missing_field_lists_the_ones_that_exist() {
        let e = err("{ a: 1, b: 2 }.c");
        assert!(e.contains("no field `c`"), "{e}");
        assert!(e.contains("a, b"), "{e}");
    }

    /// §9e: `?.` yields Null rather than failing, including on a Null base, so it chains.
    #[test]
    fn safe_navigation_yields_null_instead_of_failing() {
        assert_eq!(rendered("{ a: 1 }?.missing"), "null");
        assert_eq!(rendered("null?.a"), "null");
        assert_eq!(rendered("{ a: 1 }?.missing ?? \"unknown\""), "unknown");
    }

    #[test]
    fn field_and_index_assignment_reach_into_collections() {
        assert_eq!(rendered("mut r = { a: 1 }\nr.a = 5\nr.a"), "5");
        assert_eq!(rendered("mut l = [1, 2, 3]\nl[0] = 9\nl"), "[9, 2, 3]");
        assert_eq!(rendered("mut r = { inner: { v: 1 } }\nr.inner.v = 7\nr.inner.v"), "7");
    }

    /// A write through one binding must not be visible through a copy (collections are
    /// `Arc`-shared).
    #[test]
    fn assignment_does_not_leak_through_a_shared_copy() {
        assert_eq!(rendered("mut a = [1, 2]\nlet b = a\na[0] = 9\nb"), "[1, 2]");
        assert_eq!(rendered("mut a = { v: 1 }\nlet b = a\na.v = 9\nb.v"), "1");
    }

    #[test]
    fn assigning_through_an_immutable_root_is_refused() {
        assert!(err("let r = { a: 1 }\nr.a = 2").contains("only a `mut`"));
    }

    #[test]
    fn a_closure_is_a_value_but_not_a_tsm1_one() {
        assert_eq!(rendered("let f = { |x| x + 1 }\nf"), "<closure>");
        assert!(err("let f = { |x| x }\n[f]").contains("TSM1"));
    }

    /// §5a: capture is by value, so the closure made on the last iteration holds that iteration's
    /// value.
    #[test]
    fn closures_capture_by_value_at_creation() {
        let v = ok("mut made = null\nfor i in 0..3 { made = { |x| x } }\nmade");
        assert!(matches!(v, Val::Func(_)));
        let Val::Func(f) = v else { panic!() };
        assert!(f.captured.iter().any(|(n, _)| n == "i"));
        let (_, captured_i) = f.captured.iter().rev().find(|(n, _)| n == "i").unwrap();
        assert_eq!(captured_i.render(), "2");
    }

    #[test]
    fn later_parts_report_themselves_rather_than_misbehaving() {
        assert!(err("ls").contains("no host attached"));
    }

    use crate::host::MockHost;
    use alloc::boxed::Box;

    fn stream(name: &str, values: &[i64]) -> alloc::vec::Vec<u8> {
        use libstream::wire::{StreamFlags, Table, TypeTag};
        let schema = Schema::new().field(name, TypeTag::Int, TypeModifiers::NONE);
        let t = Table {
            flags: StreamFlags::NONE,
            schema,
            rows: values.iter().map(|v| alloc::vec![Value::Int(*v)]).collect(),
        };
        let mut buf = alloc::vec::Vec::new();
        t.encode(&mut buf).expect("encodes");
        buf
    }

    /// Run `src` against `host`, returning the result and the shared log of what it was asked.
    fn run_with(
        host: MockHost,
        src: &str,
    ) -> (core::result::Result<Val, EvalError>, alloc::rc::Rc<core::cell::RefCell<crate::host::MockLog>>) {
        let log = host.log();
        let script = crate::parse_script(src).expect("parses");
        let mut interp = Interp::with_host(Box::new(host), Mode::Script);
        let r = interp.run(&script);
        (r, log)
    }

    /// **`time PIPELINE`** times the whole pipeline after it, passes its value (or failure)
    /// through, and writes its line to `stderr`; alone, or assigned to, `time` is an ordinary name.
    #[test]
    fn time_passes_a_pipelines_value_through_and_says_how_long_it_took() {
        let host = MockHost::new().with_program("list", Some(stream("n", &[1, 2, 3]))).with_clock(250_000_000);
        let (r, log) = run_with(host, "time list /home | count");
        assert_eq!(r.expect("runs").render(), "3");
        assert_eq!(log.borrow().runs[0][0].argv, alloc::vec!["list", "/home"]);
        assert_eq!(log.borrow().diagnostics, ["time: 250 ms\n"], "no io.tsm: the time alone");

        let mut host = MockHost::new().with_program("list", Some(stream("n", &[1]))).with_clock(2_000_000_000);
        let io = {
            use libstream::wire::{StreamFlags, Table, TypeTag};
            let schema = Schema::new().field("name", TypeTag::String, TypeModifiers::NONE).field(
                "reads",
                TypeTag::Int,
                TypeModifiers::NONE,
            );
            let rows = alloc::vec![alloc::vec![Value::Str("blk-0".into()), Value::Int(5)]];
            let mut buf = alloc::vec::Vec::new();
            Table { flags: StreamFlags::NONE, schema, rows }.encode(&mut buf).expect("encodes");
            buf
        };
        host.files.push((alloc::string::String::from(crate::timing::IO_TABLE), io));
        let (r, log) = run_with(host, "time list");
        assert_eq!(r.expect("runs").render(), "<table 1 row [n]>");
        assert_eq!(log.borrow().diagnostics, ["time: 2.00 s; no disk I/O\n"]);

        let host = MockHost::new().with_failing("list", 2).with_clock(1_000);
        let (r, log) = run_with(host, "time list");
        assert!(r.is_err(), "the pipeline's failure is time's");
        assert_eq!(log.borrow().diagnostics[0], "time: 1 us\n");

        let (r, _) = run_with(MockHost::new(), "let time = 4\ntime");
        assert_eq!(r.expect("a name").render(), "4");
    }

    /// **The throughput stick's script, as the maintainer runs it**: a script that does not parse,
    /// or a set that runs the wrong copy, otherwise costs a boot to find out.
    #[test]
    fn the_throughput_sticks_script_runs_each_set_timed() {
        const H3: &str = include_str!("../../../tools/xtask/src/throughput/h3.nx");
        let (s, h) = ("/storage/THROUGHPUT", "/home");
        let copies = |from: &str, to: &str, prefix: &str| -> Vec<Vec<String>> {
            ["big", "small", "tree"]
                .iter()
                .map(|t| {
                    let src = if from == s { alloc::format!("{s}/{t}") } else { alloc::format!("{from}/t-{t}") };
                    alloc::vec!["copy".into(), src, alloc::format!("{to}/{prefix}-{t}")]
                })
                .collect()
        };
        let removes = |dir: &str, prefix: &str| -> Vec<Vec<String>> {
            ["big", "small", "tree"]
                .iter()
                .map(|t| alloc::vec!["remove".into(), "-r".into(), alloc::format!("{dir}/{prefix}-{t}")])
                .collect()
        };
        let round_end: Vec<Vec<String>> =
            alloc::vec![["remove", "-r", "/home/t-big", "/home/t-small", "/home/t-tree"].map(String::from).to_vec()];
        // (set, its timed commands, its untimed ones after them, its answer)
        let sets = [
            (1, copies(s, h, "t"), alloc::vec![], "set 1 done: photograph the lines, then restart for set 2."),
            (2, copies(h, h, "u"), alloc::vec![], "set 2 done: photograph the lines, then restart for set 3."),
            (
                3,
                copies(h, s, "c"),
                alloc::vec![],
                "set 3 done: photograph the lines. Its copies can be checked on the host now.",
            ),
            (
                4,
                [removes(h, "u"), removes(s, "c")].concat(),
                round_end,
                "set 4 done: photograph the lines, then disk --eject THROUGHPUT. The round is over.",
            ),
        ];
        let host = || {
            MockHost::new()
                .with_file("/storage/THROUGHPUT/h3.nx", H3)
                .with_program("copy", None)
                .with_program("remove", None)
                .with_clock(1_000_000_000)
        };
        // Two lines at a prompt, as the maintainer types them: the import has to outlive its line.
        let prompt = |host: MockHost, line: &str| {
            let mut interp = Interp::with_host(Box::new(host), Mode::Repl);
            let import = interp.run_line("use \"/storage/THROUGHPUT/h3.nx\" { run }").expect("imports");
            assert_eq!(import, None, "an import shows nothing");
            interp.run_line(line).map(|shown| shown.map(|s| String::from(s.trim_end())))
        };
        for (set, timed, untimed, answer) in sets {
            let host = host();
            let log = host.log();
            let shown = prompt(host, &alloc::format!("run({set})")).expect("the set runs");
            assert_eq!(shown.as_deref(), Some(answer), "set {set}");
            let ran: Vec<Vec<String>> = log.borrow().runs.iter().map(|chain| chain[0].argv.clone()).collect();
            assert_eq!(ran, [timed.clone(), untimed].concat(), "set {set}'s commands");
            let lines = alloc::vec!["time: 1.00 s\n"; timed.len()];
            assert_eq!(log.borrow().diagnostics, lines, "set {set}: what is timed");
        }

        let host = host();
        let log = host.log();
        assert!(prompt(host, "run(5)").expect_err("no set 5").message.contains("the sets are 1 to 4"));
        assert!(log.borrow().runs.is_empty());
    }

    #[test]
    fn a_single_external_stage_runs_and_its_output_becomes_a_table() {
        let host = MockHost::new().with_program("list", Some(stream("n", &[1, 2, 3])));
        let (r, log) = run_with(host, "list /system");
        let v = r.expect("runs");
        assert_eq!(v.render(), "<table 3 rows [n]>");
        assert_eq!(log.borrow().runs.len(), 1);
        assert_eq!(log.borrow().runs[0][0].argv, alloc::vec!["list", "/system"]);
    }

    /// Consecutive external stages go to the host as **one chain** (§1): running them one at a time
    /// would serialise what should stream.
    #[test]
    fn consecutive_external_stages_are_handed_over_as_one_chain() {
        let host = MockHost::new().with_program("list", None).with_program("xform", Some(stream("n", &[7])));
        let (r, log) = run_with(host, "list | xform | xform");
        r.expect("runs");
        assert_eq!(log.borrow().runs.len(), 1, "one chain, not three");
        assert_eq!(log.borrow().runs[0].len(), 3);
    }

    #[test]
    fn flags_and_arguments_reach_argv_in_order() {
        let host = MockHost::new().with_program("list", None);
        let (r, log) = run_with(host, "list --long -rf /system");
        r.expect("runs");
        assert_eq!(log.borrow().runs[0][0].argv, alloc::vec!["list", "--long", "-rf", "/system"]);
    }

    #[test]
    fn a_failing_stage_fails_the_pipeline() {
        let host = MockHost::new().with_failing("nope", 3);
        let (r, _log) = run_with(host, "nope");
        let e = r.expect_err("should fail");
        assert!(e.message.contains("exited 3"), "{}", e.message);
    }

    /// The per-stage report rides on the error, the one thing that reaches the `catch` (§2).
    #[test]
    fn a_failed_pipeline_carries_its_per_stage_report() {
        let host = MockHost::new().with_failing("nope", 3);
        let (r, _log) = run_with(host, "nope");
        let e = r.expect_err("should fail");
        std::println!("ERR = {:?}", e);
        assert_eq!(e.kind, PIPELINE_FAILED);
        let stages = e.stages.as_ref().expect("the per-stage report");
        let Value::List(rows) = stages else { panic!("expected a list of stages") };
        assert_eq!(rows.len(), 1);
        let Value::Record(r0) = &rows[0] else { panic!("expected a stage record") };
        let names: Vec<&str> = r0.schema.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["command", "exit_status", "crashed", "cancelled"]);
        assert_eq!(r0.values[0], Value::Str(alloc::string::String::from("nope")));
        assert_eq!(r0.values[1], Value::Int(3));
    }

    #[test]
    fn a_script_can_read_which_stage_failed() {
        let host = MockHost::new().with_failing("nope", 3);
        let (r, _log) = run_with(host, "try { nope } catch (e) { e.stages | filter exit_status != 0 | count }");
        assert_eq!(r.expect("caught").render(), "1");
    }

    #[test]
    fn a_crashed_stage_is_reported_as_crashed_not_as_an_exit_code() {
        let host = MockHost::new().with_crashing("boom");
        let (r, _log) = run_with(host, "boom");
        let e = r.expect_err("should fail");
        assert!(e.message.contains("crashed"), "{}", e.message);
    }

    /// §1's eager-abort opt-in: with `strict`, stages after a failure are terminated and reported
    /// *cancelled*, which is neither succeeded nor failed.
    #[test]
    fn strict_cancels_the_remaining_stages() {
        let host = MockHost::new().with_failing("bad", 1).with_program("after", None);
        let (r, _log) = run_with(host, "bad | after");
        assert!(r.is_err());

        let host = MockHost::new().with_failing("bad", 1).with_program("after", None);
        let (r, _log) = run_with(host, "strict {\n bad | after\n}");
        let e = r.expect_err("should fail");
        assert!(e.message.contains("exited 1") || e.message.contains("cancelled"), "{}", e.message);
    }

    /// The value flowing between two external runs is re-encoded as a stream.
    #[test]
    fn a_value_piped_into_a_program_is_encoded_as_a_stream() {
        let host = MockHost::new().with_program("src", Some(stream("n", &[1, 2]))).with_program("sink", None);
        let (r, log) = run_with(host, "src | sink");
        r.expect("runs");
        assert_eq!(log.borrow().inputs[0], None, "a head stage has no piped input");
    }

    #[test]
    fn a_stage_with_no_output_yields_null_not_an_empty_table() {
        let host = MockHost::new().with_program("remove", None);
        let (r, _log) = run_with(host, "remove /x");
        assert!(r.expect("runs").is_null());
    }

    /// §5b: an external program takes its input from the pipe, so the `def` conventions are refused.
    #[test]
    fn def_calling_conventions_are_refused_for_an_external() {
        let host = MockHost::new().with_program("prog", None);
        let (r, _log) = run_with(host, "prog (_)");
        assert!(r.is_err());
    }

    /// D4 in a pipeline: a stage with a piped operand is unambiguously an invocation, so a local
    /// binding does **not** shadow a command name here, the opposite of the bare-reference rule.
    #[test]
    fn a_local_binding_does_not_shadow_a_command_inside_a_pipeline() {
        // The operand has to be a *bare name in a non-head stage*, the only place the rule applies;
        // a call with arguments never reaches the branch.
        let host = MockHost::new().with_program("src", Some(stream("n", &[1]))).with_program("sink", None);
        let (r, log) = run_with(host, "let sink = 5\nsrc | sink");
        r.expect("runs");
        assert_eq!(log.borrow().runs[0].len(), 2, "the second stage ran as a command, not as the binding");
    }

    /// At the *head* the precedence is the other way round: a bound bare name is the value
    /// being piped onward, not a command.
    #[test]
    fn a_head_stage_prefers_a_local_binding() {
        let host = MockHost::new().with_program("t", None).with_program("sink", None);
        let (r, _log) = run_with(host, "let t = 5\nt | sink");
        // `t` resolved to the Int binding, which cannot be piped into a program; that error is the
        // proof, since the mock knows a command `t`.
        let e = r.expect_err("an Int cannot be piped into a program");
        assert!(e.message.contains("only a Table or text can be piped"), "{}", e.message);
    }

    /// Text can be piped into a program, wrapped as a `TEXT_FALLBACK` stream (the "Unix floor").
    #[test]
    fn a_string_and_a_list_of_strings_reach_a_program_as_a_text_stream() {
        // `\\n` in the *shell* source: the literal has to carry a newline for the split to be
        // visible, and a raw one would end the line.
        for source in ["\"one\\ntwo\" | sink", "[\"one\", \"two\"] | sink"] {
            let host = MockHost::new().with_program("sink", None);
            let (r, log) = run_with(host, source);
            r.unwrap_or_else(|e| panic!("{source}: {}", e.message));
            let inputs = log.borrow().inputs.clone();
            let bytes =
                inputs.first().cloned().flatten().unwrap_or_else(|| panic!("{source}: the stage got no stream"));
            let t = libstream::wire::Table::decode(&bytes).expect("a TSM1 stream");
            assert!(
                t.flags.contains(libstream::StreamFlags::TEXT_FALLBACK),
                "{source}: the flag is what makes it text rather than a one-column table"
            );
            assert_eq!(t.rows.len(), 2, "{source}: one row per line");
            assert_eq!(t.rows[0][0], Value::Str(alloc::string::String::from("one")));
            assert_eq!(t.rows[1][0], Value::Str(alloc::string::String::from("two")));
        }
    }

    #[test]
    fn a_list_with_one_non_string_is_not_text() {
        let host = MockHost::new().with_program("sink", None);
        let (r, _log) = run_with(host, "[\"one\", 2] | sink");
        let e = r.expect_err("a mixed list is not text");
        assert!(e.message.contains("only a Table or text can be piped"), "{}", e.message);
    }

    #[test]
    fn the_deliverable_pipeline_runs() {
        let host = MockHost::new().with_program("list", Some(stream("size", &[5, 200, 3000])));
        let (r, log) = run_with(host, "list | filter size > 100 | sort size | take 5 | display");
        r.expect("runs");
        let out = log.borrow().output.concat();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "size");
        assert_eq!(lines[1], "200");
        assert_eq!(lines[2], "3000");
    }

    #[test]
    fn a_bareword_predicate_filters() {
        let host = MockHost::new().with_program("src", Some(stream("n", &[1, 2, 3])));
        let (r, _log) = run_with(host, "src | filter n > 1 | count");
        assert_eq!(r.expect("runs").render(), "2");
    }

    #[test]
    fn an_explicit_closure_filters_identically() {
        let host = MockHost::new().with_program("src", Some(stream("n", &[1, 2, 3])));
        let (r, _log) = run_with(host, "src | filter { |row| row.n > 1 } | count");
        assert_eq!(r.expect("runs").render(), "2");
    }

    /// An explicit closure can reach a local; the sugared form cannot (a bare name there is a
    /// field, §8b).
    #[test]
    fn an_explicit_closure_can_close_over_a_local() {
        let host = MockHost::new().with_program("src", Some(stream("n", &[1, 5, 9])));
        let (r, _log) = run_with(
            host,
            "let threshold = 4
src | filter { |row| row.n > threshold } | count",
        );
        assert_eq!(r.expect("runs").render(), "2");
    }

    /// §5c: only the first stage is a process; the rest runs on the `Value` tree.
    #[test]
    fn only_the_external_stage_costs_a_spawn() {
        let host = MockHost::new().with_program("list", Some(stream("n", &[1, 2, 3])));
        let (r, log) = run_with(host, "list | filter n > 0 | sort n | take 2 | count");
        assert_eq!(r.expect("runs").render(), "2");
        assert_eq!(log.borrow().runs.len(), 1, "one spawn for the whole pipeline");
        assert_eq!(log.borrow().runs[0].len(), 1);
    }

    #[test]
    fn an_operator_without_an_operand_says_so() {
        let e = err("count");
        assert!(e.contains("needs an operand"), "{e}");
    }

    /// `each` is for effects, so the chain does not narrow because something looked at every row.
    #[test]
    fn each_passes_its_input_through() {
        let host = MockHost::new().with_program("src", Some(stream("n", &[1, 2])));
        let (r, _log) = run_with(host, "src | each { |row| row } | count");
        assert_eq!(r.expect("runs").render(), "2");
    }

    /// §6: `expect` checks shape, `assert` checks content; separate keywords so the message
    /// says which kind of thing went wrong.
    #[test]
    fn expect_and_assert_are_different_checks() {
        assert_eq!(
            rendered(
                "let x: Int = 5
x"
            ),
            "5"
        );
        assert!(err("assert (1 == 2)").contains("assertion failed"));
        assert!(err("assert (5)").contains("needs a Bool"));
    }

    #[test]
    fn save_and_open_round_trip() {
        let host = MockHost::new().with_program("src", Some(stream("n", &[7, 8])));
        let (r, _log) = run_env(host, env_with(&[("PWD", "/tmp")]), "src | save ./out.tsm");
        r.expect("saves");

        let host = MockHost::new().with_file("/tmp/notes.txt", "alpha\nbeta\n");
        let (r, _log) = run_env(host, env_with(&[("PWD", "/tmp")]), "open ./notes.txt | count");
        assert_eq!(r.expect("opens").render(), "2");
    }

    #[test]
    fn open_concatenates_several_paths() {
        let host = MockHost::new().with_file("/tmp/a.txt", "one\n").with_file("/tmp/b.txt", "two\nthree\n");
        let (r, _log) = run_env(host, env_with(&[("PWD", "/tmp")]), "open ./a.txt ./b.txt | count");
        assert_eq!(r.expect("opens").render(), "3");
    }

    #[test]
    fn a_def_is_callable_with_positional_and_named_arguments() {
        assert_eq!(
            rendered(
                "def greet(name: String, loud: Bool = false) -> String {\n                       if loud { name ++ \"!\" } else { name ++ \".\" }\n}\ngreet(\"a\")"
            ),
            "a."
        );
        assert_eq!(
            rendered(
                "def greet(name: String, loud: Bool = false) -> String {\n                       if loud { name ++ \"!\" } else { name ++ \".\" }\n}\n                      greet(\"a\", loud: true)"
            ),
            "a!"
        );
    }

    /// §5b: defaults are evaluated fresh per call in the function's own parameter scope, so a
    /// later default can reference an earlier parameter and no mutable default is shared.
    #[test]
    fn a_default_can_reference_an_earlier_parameter() {
        assert_eq!(rendered("def f(a: Int, b: Int = a + 1) -> Int { b }\nf(5)"), "6");
        assert_eq!(rendered("def f(a: Int, b: Int = a + 1) -> Int { b }\nf(5, b: 0)"), "0");
    }

    #[test]
    fn arity_and_unknown_named_arguments_are_errors() {
        let src = "def f(a: Int) -> Int { a }\n";
        assert!(err(&alloc::format!("{src}f(1, 2)")).contains("takes 1"));
        assert!(err(&alloc::format!("{src}f(1, nope: 2)")).contains("no parameter `nope`"));
        assert!(err(&alloc::format!("{src}f()")).contains("needs an argument"));
    }

    #[test]
    fn variadic_parameters_collect_the_rest() {
        assert_eq!(rendered("def n(...rest: List<Int>) -> Int { rest | count }\nn(1, 2, 3)"), "3");
    }

    /// §5a: `def` bindings hoist, so two functions can call each other in any textual order;
    /// `let` does not.
    #[test]
    fn defs_hoist_so_mutual_recursion_works() {
        let src = "def even(n: Int) -> Bool { if n == 0 { true } else { odd(n - 1) } }\n                   def odd(n: Int) -> Bool { if n == 0 { false } else { even(n - 1) } }\n                   even(4)";
        assert_eq!(rendered(src), "true");
    }

    /// §5a: a named function captures **nothing**; its parameter list is a complete account of
    /// its inputs.
    #[test]
    fn a_def_does_not_capture_its_enclosing_scope() {
        let e = err("let outside = 1\ndef f() -> Int { outside }\nf()");
        assert!(e.contains("not a binding"), "{e}");
    }

    /// §5b: `_` is required whenever an argument list is present; nothing guarantees the first
    /// parameter is the data slot.
    #[test]
    fn the_pipeline_fill_placeholder_is_explicit_when_a_list_is_present() {
        let host = MockHost::new().with_program("src", Some(stream("n", &[1, 2])));
        let (r, _l) = run_with(
            host,
            "def rows(t: Table, label: String = \"x\") -> Int { t | count }\n             src | rows(_, label: \"n\")",
        );
        assert_eq!(r.expect("runs").render(), "2");

        let host = MockHost::new().with_program("src", Some(stream("n", &[1, 2])));
        let (r, _l) = run_with(host, "def rows(t: Table) -> Int { t | count }\nsrc | rows");
        assert_eq!(r.expect("runs").render(), "2");

        let host = MockHost::new().with_program("src", Some(stream("n", &[1])));
        let (r, _l) = run_with(
            host,
            "def rows(t: Table, label: String) -> Int { t | count }\n             src | rows(label: \"n\")",
        );
        assert!(r.expect_err("should refuse").message.contains("`_`"));
    }

    #[test]
    fn match_covers_the_9f_pattern_forms() {
        assert_eq!(rendered("match 5 { 0..3 => \"low\"\n _ => \"high\" }"), "high");
        assert_eq!(rendered("match 1 { 1 | 2 => \"low\"\n _ => \"high\" }"), "low");
        assert_eq!(rendered("match 7 { Int => \"an int\"\n _ => \"other\" }"), "an int");
        assert_eq!(rendered("match 7 { n => n + 1 }"), "8");
        assert_eq!(rendered("match { name: \"a\", size: 2 } { { name } => name\n _ => \"?\" }"), "a");
    }

    #[test]
    fn a_guard_sees_the_arms_bindings() {
        let src = "match { name: \"big\", size: 2000 } {\n                   { name, size } if size > 1000 => name\n                   { name } => \"small\"\n}";
        assert_eq!(rendered(src), "big");
    }

    /// §9f: no static exhaustiveness check, so an unmatched value is a runtime `MatchError`,
    /// not a silent fallthrough.
    #[test]
    fn an_unmatched_value_is_a_match_error() {
        let e = err("match 5 { 0 => 1 }");
        assert!(e.contains("MatchError"), "{e}");
    }

    /// §2: the error arrives as an ordinary `Record`, so `match err { … }` needs no new grammar.
    #[test]
    fn try_catch_binds_the_error_as_a_record() {
        assert_eq!(rendered("try { 1 / 0 } catch (e) { e.message }"), "division by zero");
        assert_eq!(rendered("try { 1 } catch (e) { 2 }"), "1");
        let src = "try { 1 / 0 } catch (e) {\n                   match e { { kind: \"Error\" } => \"handled\"\n _ => \"?\" }\n}";
        assert_eq!(rendered(src), "handled");
    }

    #[test]
    fn the_match_operator_works_end_to_end() {
        assert_eq!(rendered("\"parse.rs\" ~= /\\.rs$/"), "true");
        assert_eq!(rendered("\"parse.py\" ~= /\\.rs$/"), "false");
        assert_eq!(rendered("let p = \"^a\"\n\"abc\" ~= p"), "true");
    }

    /// §10a: `filter` plus `~=` covers `grep`'s job; the gap was an operator, not a program.
    #[test]
    fn filter_with_a_regex_replaces_grep() {
        use libstream::wire::{StreamFlags, Table, TypeTag};
        let schema = Schema::new().field("name", TypeTag::String, TypeModifiers::NONE);
        let t = Table {
            flags: StreamFlags::NONE,
            schema,
            rows: alloc::vec![
                alloc::vec![Value::Str(alloc::string::String::from("parse.rs"))],
                alloc::vec![Value::Str(alloc::string::String::from("notes.txt"))],
                alloc::vec![Value::Str(alloc::string::String::from("lex.rs"))],
            ],
        };
        let mut buf = alloc::vec::Vec::new();
        t.encode(&mut buf).expect("encodes");
        let host = MockHost::new().with_program("list", Some(buf));
        let (r, _l) = run_with(host, "list | filter name ~= /\\.rs$/ | count");
        assert_eq!(r.expect("runs").render(), "2");
    }

    #[test]
    fn a_malformed_pattern_is_a_loud_error() {
        assert!(err("\"x\" ~= /(a/").contains("unclosed"));
        assert!(err("\"x\" ~= /a{2}/").contains("counted repetition"));
    }

    fn env_with(pairs: &[(&str, &str)]) -> libstream::wire::Record {
        let mut schema = Schema::new();
        let mut values = Vec::new();
        for (k, v) in pairs {
            schema = schema.field(k, libstream::wire::TypeTag::String, TypeModifiers::NONE);
            values.push(Value::Str(alloc::string::String::from(*v)));
        }
        Record { schema, values }
    }

    /// Drive a **sequence of REPL lines** through one interpreter, each a separate parse and
    /// execute against the state the last left. Whole-script tests via `run` cannot see
    /// interactive-only bugs.
    fn repl(lines: &[&str]) -> core::result::Result<Option<alloc::string::String>, EvalError> {
        let mut interp = Interp::with_host(Box::new(MockHost::new()), Mode::Repl);
        let mut last = None;
        for l in lines {
            last = interp.run_line(l)?;
        }
        Ok(last.map(|s| alloc::string::String::from(s.trim_end())))
    }

    /// A host with a `/bin`, a `PWD` and something under it, for completion.
    fn completing_host() -> MockHost {
        let mut h = MockHost::new();
        h.commands = alloc::vec![String::from("list"), String::from("copy"), String::from("whoami"),];
        h.entries = alloc::vec![
            (
                String::from("/home"),
                alloc::vec![
                    (String::from("Documents"), true),
                    (String::from("Downloads"), true),
                    (String::from("notes.txt"), false),
                ],
            ),
            (String::from("/bin"), alloc::vec![(String::from("list"), false)]),
            (String::from("/"), alloc::vec![(String::from("bin"), true), (String::from("home"), true)],),
        ];
        h
    }

    /// An interpreter completing from `/home`.
    fn completing() -> Interp {
        let mut i = Interp::with_host(Box::new(completing_host()), Mode::Repl);
        i.set_env(env_with(&[("PWD", "/home")]));
        i
    }

    /// A stage head offers all four categories, checked separately: they come from four sources,
    /// and one assertion on one prefix passes with the rest unwired.
    #[test]
    fn a_stage_head_completes_every_category_of_name() {
        let mut i = completing();
        i.run_line("def deploy() { 1 }").expect("a def at the prompt");

        assert_eq!(i.complete("le").candidates, alloc::vec![String::from("let")]);
        // `exi`, not `ex`: a prefix shared by two sources proves neither.
        assert_eq!(i.complete("exi").candidates, alloc::vec![String::from("exit")]);
        assert_eq!(i.complete("ded").candidates, alloc::vec![String::from("dedupe")]);
        assert_eq!(i.complete("who").candidates, alloc::vec![String::from("whoami")]);
        assert_eq!(i.complete("dep").candidates, alloc::vec![String::from("deploy")]);
    }

    #[test]
    fn punctuation_in_the_keyword_table_is_not_a_candidate() {
        let mut i = completing();
        assert!(
            !i.complete("").candidates.iter().any(|c| c == "_"),
            "the pipeline-fill placeholder is punctuation and was offered as a name"
        );
    }

    #[test]
    fn an_argument_completes_a_path() {
        let mut i = completing();
        assert_eq!(
            i.complete("list Do").candidates,
            alloc::vec![String::from("Documents/"), String::from("Downloads/")],
        );
        assert_eq!(i.complete("list /home/n").candidates, alloc::vec![String::from("/home/notes.txt")],);
        assert!(i.complete("list /nowhere/x").candidates.is_empty());
    }

    /// `..` completes to the parent, and a second Tab lists what is in it. `cd ..<TAB>` once erased
    /// the `..`: it was no candidate, and a completion finding nothing replaced the word with the
    /// empty string.
    #[test]
    fn the_parent_directory_completes_and_then_lists() {
        let mut i = completing();
        assert_eq!(i.complete("cd ..").candidates, alloc::vec![String::from("../")]);
        assert_eq!(i.complete("cd ../").candidates, alloc::vec![String::from("../bin/"), String::from("../home/")],);
        assert_eq!(i.complete("cd .").candidates, alloc::vec![String::from("../"), String::from("./")],);
        assert_eq!(i.complete("cd ../ho").candidates, alloc::vec![String::from("../home/")]);
    }

    /// Two files whose names differ in an accent complete without panicking: `list_dir` keeps valid
    /// non-ASCII via `from_utf8_lossy`, so the bytes reach the prefix scan.
    #[test]
    fn accented_names_complete_rather_than_panicking() {
        let mut h = completing_host();
        h.entries.push((
            String::from("/home/notes"),
            alloc::vec![(String::from("caf\u{e9}.txt"), false), (String::from("caf\u{e8}.txt"), false),],
        ));
        let mut i = Interp::with_host(Box::new(h), Mode::Repl);
        i.set_env(env_with(&[("PWD", "/home")]));
        let c = i.complete("list /home/notes/ca");
        assert_eq!(c.candidates.len(), 2);
        assert_eq!(c.common_prefix(), "/home/notes/caf");
        assert_eq!(c.filled("list /home/notes/ca").as_deref(), Some("list /home/notes/caf"));
    }

    /// Tab must never splice a candidate into the **middle** of what was typed. An argument is read
    /// in **word** mode, where `+` is ordinary text, so `cd my+not<TAB>` has the word `my+not`,
    /// matches nothing, and leaves the line alone.
    #[test]
    fn a_candidate_is_never_spliced_into_the_middle_of_a_word() {
        let mut i = completing();
        for line in ["cd my+not", "cd a~not", "cd x@not", "list 50%not"] {
            let c = i.complete(line);
            assert!(
                c.candidates.is_empty(),
                "{line} completed to {:?} — a fragment of the word was matched on its own",
                c.candidates
            );
            assert_eq!(c.filled(line), None, "{line} would have been rewritten");
        }
        assert_eq!(i.complete("cd not").candidates, alloc::vec![String::from("notes.txt")]);
        // `let x=not` once became `let x=notes.txt`, a filename where an expression starts.
        assert!(i.complete("let x=not").candidates.is_empty());
        assert_eq!(i.complete("let x = not").candidates, alloc::vec![String::from("notes.txt")]);
    }

    /// The second line of a statement completes as the statement: the console loop hands `complete`
    /// the accumulated statement, or the newline reads as a boundary.
    #[test]
    fn a_continuation_line_completes_in_the_statement_it_continues() {
        let mut i = completing();
        assert_eq!(
            i.complete("format(\"{}\",\nDo").candidates,
            alloc::vec![String::from("Documents/"), String::from("Downloads/")],
            "a continuation line was completed as if it were a new statement"
        );
        assert_eq!(i.complete("whoami\nwho").candidates, alloc::vec![String::from("whoami")]);
    }

    /// A word that matches nothing leaves the line as typed; `filled` returns `None` so the console
    /// loop cannot replace it with the empty common prefix.
    #[test]
    fn a_completion_that_matches_nothing_leaves_the_word_alone() {
        let mut i = completing();
        for line in ["cd zzz", "zzz", "list /home/zzz"] {
            let c = i.complete(line);
            assert!(c.candidates.is_empty(), "{line} should match nothing");
            assert_eq!(c.filled(line), None, "{line} would have had its word erased");
        }
    }

    /// A word with a slash is a path even at the head of a stage: `./script.nx` and `/bin/list` are
    /// commands written as paths, and there is no search path (§9h).
    #[test]
    fn a_command_written_as_a_path_completes_as_a_path() {
        let mut i = completing();
        assert_eq!(i.complete("/bi").candidates, alloc::vec![String::from("/bin/")]);
        assert_eq!(i.complete("/bin/li").candidates, alloc::vec![String::from("/bin/list")]);
    }

    /// Completing runs nothing: `list_dir` on the real host opens a directory session, so this
    /// asserts it stayed a *read*.
    #[test]
    fn completing_runs_nothing() {
        let host = completing_host();
        let log = host.log();
        let mut i = Interp::with_host(Box::new(host), Mode::Repl);
        i.set_env(env_with(&[("PWD", "/home")]));
        i.complete("list Do");
        i.complete("who");
        assert!(log.borrow().runs.is_empty(), "completing spawned a pipeline");
        assert!(log.borrow().output.is_empty(), "completing wrote to stdout");
        assert!(log.borrow().diagnostics.is_empty(), "completing wrote a diagnostic");
    }

    /// A call in an argument list is **evaluated**: the inner call runs and its value becomes the
    /// outer call's argument.
    #[test]
    fn a_call_in_an_argument_list_is_evaluated() {
        let out =
            repl(&["def my_age() { 41 }", "def age_plus_n(a, n) { a + n }", "let x = age_plus_n(my_age(), 3)", "x"])
                .expect("the report's own script should run");
        assert_eq!(out.as_deref(), Some("44"));

        let out = repl(&["def double(n) { n * 2 }", "format(\"n={}\", double(double(3)))"])
            .expect("a nested call inside an operator should run");
        assert_eq!(out.as_deref(), Some("n=12"));
    }

    /// **A parenthesised expression computes an external program's argument**, and a bareword
    /// beside it stays literal: `copy /home/a /home/b` names two paths and `sort size` a column, so
    /// argument position lexes words. Nothing had tested the parens on an *external* call.
    #[test]
    fn a_parenthesised_argument_to_an_external_program_is_evaluated() {
        let mut h = MockHost::new();
        h.programs.push((String::from("copy"), 0, None));
        let log = h.log();
        let mut interp = Interp::with_host(Box::new(h), Mode::Script);
        interp
            .run_line(r#"for i in 1..4 { copy /src (format("/dst-{}.png", i)) }"#)
            .expect("a loop with a computed destination should run");
        let runs = log.borrow().runs.clone();
        let argv: alloc::vec::Vec<alloc::vec::Vec<String>> = runs.iter().map(|r| r[0].argv.clone()).collect();
        assert_eq!(argv.len(), 3, "one spawn per iteration");
        assert_eq!(argv[0], ["copy", "/src", "/dst-1.png"]);
        assert_eq!(argv[2], ["copy", "/src", "/dst-3.png"]);

        let mut h = MockHost::new();
        h.programs.push((String::from("copy"), 0, None));
        let log = h.log();
        let mut interp = Interp::with_host(Box::new(h), Mode::Script);
        interp.run_line("let i = 7").unwrap();
        interp.run_line("copy /src /dst-$i.png").unwrap();
        assert_eq!(
            log.borrow().runs[0][0].argv,
            ["copy", "/src", "/dst-$i.png"],
            "a bareword is literal — there is no interpolation (§8d)"
        );
    }

    /// Argument-position arithmetic means what it means everywhere else (a second copy of the
    /// binary tiers once folded flat: 12, not 8).
    #[test]
    fn arithmetic_in_an_argument_list_keeps_its_precedence() {
        let out = repl(&["let a = 2", "let b = 3", "let c = 2", "format(\"{}\", a + b * c)"])
            .expect("arithmetic in an argument list should run");
        assert_eq!(out.as_deref(), Some("8"), "an argument list changed what `+` and `*` mean");
    }

    /// **A `def` typed at the prompt has to be callable on the next line.** `exec` treats
    /// `Stmt::Def` as a no-op because `hoist_defs` registers definitions, and it once ran only from
    /// `exec_block`.
    #[test]
    fn a_def_typed_at_the_prompt_is_callable_afterwards() {
        let out = repl(&["def add(a, b) { a + b }", "add(1, 2)"]).expect("both lines run");
        assert_eq!(out.as_deref(), Some("3"));
    }

    #[test]
    fn defs_on_one_repl_line_hoist_like_a_script() {
        let out = repl(&["def a(n) { b(n) + 1 }\ndef b(n) { n * 2 }", "a(3)"]).expect("runs");
        assert_eq!(out.as_deref(), Some("7"));
    }

    #[test]
    fn redefining_a_function_at_the_prompt_takes_effect() {
        let out = repl(&["def f() { 1 }", "def f() { 2 }", "f()"]).expect("runs");
        assert_eq!(out.as_deref(), Some("2"));
    }

    /// **An `if` typed at the prompt shows its value** (§9a): `should_display` once asked only for
    /// `Stmt::Expr`.
    #[test]
    fn an_if_expression_shows_its_value_at_the_prompt() {
        let out = repl(&["let n = 5", "if n > 3 { \"big\" } else { \"small\" }"]).expect("runs");
        assert_eq!(out.as_deref(), Some("big"));
    }

    /// A chain ending in a terminal operator must not echo what `display` already printed.
    #[test]
    fn a_terminal_operator_still_suppresses_the_echo() {
        let out = repl(&["[1, 2] | display"]).expect("runs");
        assert_eq!(out, None, "`display` already printed; the REPL must not print again");
    }

    /// `let` still does **not** hoist: it is tied to evaluation order.
    #[test]
    fn let_still_does_not_hoist_across_repl_lines() {
        let mut interp = Interp::with_host(Box::new(MockHost::new()), Mode::Repl);
        assert!(interp.run_line("x").is_err(), "`x` is not bound yet");
        interp.run_line("let x = 5").expect("binds");
        let got = interp.run_line("x").expect("reads");
        assert_eq!(got.as_deref().map(str::trim_end), Some("5"));
    }

    fn run_env(
        host: MockHost,
        env: libstream::wire::Record,
        src: &str,
    ) -> (core::result::Result<Val, EvalError>, alloc::rc::Rc<core::cell::RefCell<crate::host::MockLog>>) {
        let log = host.log();
        let script = crate::parse_script(src).expect("parses");
        let mut interp = Interp::with_host(Box::new(host), Mode::Script);
        interp.set_env(env);
        let r = interp.run(&script);
        (r, log)
    }

    #[test]
    fn the_environment_is_an_ordinary_value() {
        let (r, _l) = run_env(MockHost::new(), env_with(&[("PWD", "/home/alice")]), "$env.PWD");
        assert_eq!(r.expect("reads").render(), "/home/alice");
    }

    /// A stage's arguments pass through *as written* with the same `PWD`, so both sides resolve a
    /// relative path identically.
    #[test]
    fn a_spawned_stage_receives_the_environment_and_unrewritten_arguments() {
        let host = MockHost::new().with_program("list", None);
        let (r, log) = run_env(host, env_with(&[("PWD", "/system")]), "list .");
        r.expect("runs");
        let log = log.borrow();
        assert_eq!(log.runs[0][0].argv, alloc::vec!["list", "."], "argv passed through");
        let env = &log.envs[0];
        let i = env.schema.fields.iter().position(|f| f.name == "PWD").expect("PWD");
        assert_eq!(env.values[i].as_str(), Some("/system"), "same PWD handed over");
    }

    #[test]
    fn cd_commits_only_to_a_path_that_exists() {
        let host = MockHost::new().with_dir("/system");
        let (r, _l) = run_env(host, env_with(&[("PWD", "/")]), "cd /system\n$env.PWD");
        assert_eq!(r.expect("cd works").render(), "/system");

        let host = MockHost::new().with_dir("/system");
        let (r, _l) = run_env(host, env_with(&[("PWD", "/")]), "cd /nope");
        let e = r.expect_err("a missing directory is refused");
        assert!(e.message.contains("does not exist"), "{}", e.message);
        assert!(e.message.contains("PWD was left alone"), "{}", e.message);
    }

    #[test]
    fn cd_accepts_a_relative_path() {
        let host = MockHost::new().with_dir("/home");
        let (r, _l) = run_env(host, env_with(&[("PWD", "/home/alice")]), "cd ..\n$env.PWD");
        assert_eq!(r.expect("cd ..").render(), "/home");
    }

    #[test]
    fn bare_cd_goes_home_or_explains() {
        let host = MockHost::new().with_dir("/home/alice");
        let (r, _l) = run_env(host, env_with(&[("PWD", "/"), ("HOME", "/home/alice")]), "cd\n$env.PWD");
        assert_eq!(r.expect("cd home").render(), "/home/alice");

        let (r, _l) = run_env(MockHost::new(), env_with(&[("PWD", "/")]), "cd");
        assert!(r.expect_err("no HOME").message.contains("HOME is not set"));
    }

    #[test]
    fn the_shell_resolves_its_own_relative_paths() {
        let host = MockHost::new().with_file("/home/alice/notes.txt", "one\ntwo\n");
        let (r, _l) = run_env(host, env_with(&[("PWD", "/home/alice")]), "open ./notes.txt | count");
        assert_eq!(r.expect("opens").render(), "2");
    }

    #[test]
    fn without_a_pwd_a_relative_path_is_refused() {
        let host = MockHost::new().with_file("/notes.txt", "x\n");
        let (r, _l) = run_env(host, Record::default(), "open ./notes.txt");
        assert!(r.expect_err("no PWD").message.contains("no working directory"));
    }

    #[test]
    fn the_deliverable_computes() {
        assert_eq!(rendered("let x = 2 + 3\nx"), "5");
    }

    /// The motivating case: **stop at the first match**. `return` was the sole early exit and it
    /// leaves the whole function.
    #[test]
    fn break_stops_at_the_first_match() {
        assert_eq!(
            rendered("mut found = -1\nfor x in [4, 7, 9] {\n if x > 5 {\n found = x\n break\n }\n}\nfound"),
            "7"
        );
    }

    #[test]
    fn break_and_continue_do_what_they_say() {
        assert_eq!(rendered("mut t = 0\nfor x in 0..10 {\n if x == 3 { break }\n t = t + x\n}\nt"), "3");
        assert_eq!(rendered("mut t = 0\nfor x in 0..5 {\n if x % 2 == 0 { continue }\n t = t + x\n}\nt"), "4");
        assert_eq!(rendered("mut i = 0\nwhile true {\n i = i + 1\n if i == 3 { break }\n}\ni"), "3");
    }

    /// `break` leaves **one** loop; there are no labels (§9c).
    #[test]
    fn break_leaves_only_the_innermost_loop() {
        assert_eq!(rendered("mut n = 0\nfor x in 0..3 {\n for y in 0..3 { break }\n n = n + 1\n}\nn"), "3");
    }

    /// `return` still travels *past* a loop to leave the function.
    #[test]
    fn return_still_escapes_a_loop_and_its_function() {
        assert_eq!(
            rendered(
                "def first_odd(xs) {\n for x in xs {\n if x % 2 == 1 { return x }\n }\n -1\n}\nfirst_odd([2, 4, 5, 6])"
            ),
            "5"
        );
        assert_eq!(
            rendered("def first_odd(xs) {\n for x in xs {\n if x % 2 == 1 { return x }\n }\n -1\n}\nfirst_odd([2, 4])"),
            "-1"
        );
    }

    /// A block whose *value* is taken cannot also be a jump; see
    /// `TODO(control-flow-in-expression-position)`.
    #[test]
    fn break_where_a_value_is_expected_is_refused() {
        let e = err("for x in 0..3 {\n let y = if true { break } else { 1 }\n}");
        assert!(e.contains("where a value is expected"), "{e}");
        assert!(e.contains("statement position"), "{e}");
    }

    /// A `continue` that skips the increment hits the runaway backstop instead of hanging.
    /// **`continue` must still count**, or a guard incremented only on a normally-completing body
    /// runs forever. The limit is lowered because ten million iterations outcost the suite.
    #[test]
    fn a_continue_that_skips_the_increment_is_caught_not_hung() {
        let mut i = Interp::new();
        i.iteration_limit = 1_000;
        let e = i
            .run(&crate::parse::parse_script("mut i = 0\nwhile i < 3 {\n continue\n i = i + 1\n}").unwrap())
            .expect_err("a loop with no way to advance");
        assert!(e.message.contains("iteration limit"), "{}", e.message);

        let e = i.run(&crate::parse::parse_script("for x in 0..100000 { }").unwrap()).expect_err("an absurd range");
        assert!(e.message.contains("too large to iterate"), "{}", e.message);
    }

    /// §6's own examples are written mid-pipeline; the evaluator once refused a keyword stage.
    #[test]
    fn expect_and_assert_work_as_pipeline_stages() {
        assert_eq!(rendered("[1, 2] | expect List | count"), "2");
        assert_eq!(rendered("[1, 2] | assert (count > 0) | count"), "2");
        assert!(err("[1, 2] | expect Int").contains("expected Int, got List"));
        assert!(err("[1, 2] | assert (count > 5)").contains("assertion failed"));
    }

    /// `assert` is a check, not a transform: the value keeps going down the chain.
    #[test]
    fn assert_passes_its_value_through() {
        assert_eq!(rendered("[1, 2, 3] | assert (count == 3) | take 2 | count"), "2");
    }

    /// The mechanism under all of it: `_` is the value flowing past, for the length of
    /// the stage and nowhere else.
    #[test]
    fn underscore_is_the_value_in_a_stage_and_nothing_outside_one() {
        assert_eq!(rendered("[1, 2] | expect List | count"), "2");
        assert!(err("_").contains("placeholder"));
        assert!(err("[1, 2] | expect List\n_").contains("placeholder"));
    }

    /// A keyword stage needs something arriving; the two ways to get that wrong get different
    /// messages.
    #[test]
    fn a_keyword_stage_needs_something_arriving() {
        assert!(err("expect Int | count").contains("nothing is arriving"));
        assert!(err("expect Int").contains("placeholder"));
    }

    /// D4 still holds inside a predicate: a bare, argument-free name is a *binding* first.
    #[test]
    fn a_binding_still_wins_inside_an_assert_predicate() {
        assert_eq!(rendered("let n = 5\n[1, 2] | assert (n > 0) | count"), "2");
        assert!(err("let n = 0\n[1, 2] | assert (n > 0)").contains("assertion failed"));
    }

    #[test]
    fn parse_reads_text_as_a_number() {
        assert_eq!(rendered("\"42\" | parse Int"), "42");
        assert_eq!(rendered("\"-5\" | parse Int"), "-5");
        assert_eq!(rendered("\"3.5\" | parse Float"), "3.5");
        assert_eq!(rendered("\"42\" | parse Float"), "42.0");
        assert_eq!(rendered("42 | parse Float"), "42.0");
    }

    /// What a number looks like to `parse` is what it looks like to the lexer (§6, §8e).
    #[test]
    fn parse_accepts_exactly_the_literals_the_lexer_does() {
        assert_eq!(rendered("\"0xff\" | parse Int"), "255");
        assert_eq!(rendered("\"0b1010\" | parse Int"), "10");
        assert_eq!(rendered("\"1_000_000\" | parse Int"), "1000000");
        assert_eq!(rendered("\"1e3\" | parse Float"), "1000.0");
        assert_eq!(rendered("\"010\" | parse Int"), "10");
    }

    /// Every refusal here is a coercion another language would perform silently (§6).
    #[test]
    fn parse_fails_loud_on_everything_it_cannot_read() {
        assert!(err("\" 42 \" | parse Int").contains("whitespace"));
        let e = err("\"3.5\" | parse Int");
        assert!(e.contains("reads as a Float"), "{e}");
        assert!(err("\"abc\" | parse Int").contains("cannot parse"));
        assert_eq!(rendered("\"true\" | parse Bool"), "true");
        assert!(err("\"yes\" | parse Bool").contains("only \"true\" and \"false\""));
        assert!(err("9007199254740993 | parse Float").contains("losing precision"));
        assert!(err("42 | parse String").contains("format"));
    }

    #[test]
    fn parse_of_the_type_it_already_is_passes_through() {
        assert_eq!(rendered("42 | parse Int"), "42");
        assert_eq!(rendered("\"x\" | parse String"), "x");
        assert_eq!(rendered("null | parse Int?"), "null");
        assert!(err("null | parse Int").contains("annotate `Int?`"));
    }

    #[test]
    fn format_and_parse_round_trip() {
        assert_eq!(rendered("format(\"{}\", 42) | parse Int"), "42");
        assert_eq!(rendered("format(\"{}\", 2.5) | parse Float"), "2.5");
    }

    #[test]
    fn fail_raises_a_catchable_error_with_a_message() {
        assert_eq!(rendered("try { fail \"bad path\" } catch (e) { e.message }"), "bad path");
        assert_eq!(
            rendered(
                "def check(n) {\n if n < 0 { fail \"negative\" }\n n\n}\ntry { check(-1) } catch (e) { e.message }"
            ),
            "negative"
        );
    }

    /// A Record is raised as the error *value*, so a caller can match on more than a message;
    /// it must carry one, since an error without a message is the vague failure §6 avoids.
    #[test]
    fn fail_can_raise_a_record_and_demands_a_message() {
        assert_eq!(
            rendered("try { fail { kind: \"NotFound\", message: \"no such user\" } } catch (e) { e.kind }"),
            "NotFound"
        );
        assert!(
            err("fail { code: 2 }").contains("must \\\n                                 carry")
                || err("fail { code: 2 }").contains("message")
        );
        assert!(err("fail 42").contains("String message or a Record"));
    }

    #[test]
    fn each_kind_of_failure_names_itself() {
        let kind = |src: &str| rendered(&alloc::format!("try {{ {src} }} catch (e) {{ e.kind }}"));
        assert_eq!(kind("fail \"x\""), "Error");
        assert_eq!(kind("assert (1 == 2)"), "AssertionFailed");
        assert_eq!(kind("\"abc\" | parse Int"), "ParseError");
        assert_eq!(kind("let x: Int = \"s\""), "TypeError");
    }

    /// §6's subset match makes the vocabulary extensible: a `catch` reading only `message`
    /// keeps working when a kind is added.
    #[test]
    fn a_catch_that_reads_only_the_message_is_unaffected_by_kinds() {
        assert_eq!(rendered("try { assert (false) } catch (e) { e.message }"), "assertion failed");
    }

    /// **`exit` is a control outcome, not an error** (§11f): `catch` must not swallow it.
    #[test]
    fn exit_is_not_catchable_and_carries_its_status() {
        let mut i = Interp::new();
        let e = i
            .run(&crate::parse::parse_script("try { exit 3 } catch (e) { \"caught\" }").unwrap())
            .expect_err("exit propagates");
        assert!(e.is_exit(), "a catch swallowed `exit`");
        assert_eq!(e.exit, Some(3));

        let mut i = Interp::new();
        let e = i.run(&crate::parse::parse_script("exit").unwrap()).expect_err("exit");
        assert_eq!(e.exit, Some(0));
        let mut i = Interp::new();
        let e = i.run(&crate::parse::parse_script("def quit() { exit 7 }\nquit()").unwrap()).expect_err("exit");
        assert_eq!(e.exit, Some(7));
    }

    #[test]
    fn exit_wants_a_status_int() {
        assert!(err("exit \"now\"").contains("status Int"));
    }

    /// A failure can produce a fallback **value** chosen by the catch branch, which sees the error.
    #[test]
    fn try_catch_produces_a_value() {
        assert_eq!(rendered("let n = try { \"x\" | parse Int } catch { 8080 }\nn"), "8080");
        assert_eq!(rendered("let n = try { \"42\" | parse Int } catch { 8080 }\nn"), "42");
        assert_eq!(
            rendered("let n = try { fail \"nope\" } catch (e) { if e.kind == \"Error\" { 1 } else { 2 } }\nn"),
            "1"
        );
    }

    /// Statement position must keep working: control flow cannot leave an expression, so `exec`
    /// evaluates `try` through a `Flow`-returning path.
    #[test]
    fn control_flow_still_escapes_a_try_in_statement_position() {
        assert_eq!(
            rendered("mut n = 0\nfor x in 0..4 {\n try { fail \"x\" } catch { continue }\n n = n + 1\n}\nn"),
            "0"
        );
        assert_eq!(rendered("def f() {\n try { return 5 } catch { 0 }\n 9\n}\nf()"), "5");
        assert_eq!(rendered("mut n = 0\nfor x in 0..4 {\n n = n + 1\n try { fail \"x\" } catch { break }\n}\nn"), "1");
    }

    /// In *value* position it is refused rather than silently dropped
    /// (`TODO(control-flow-in-expression-position)`).
    #[test]
    fn control_flow_inside_a_try_used_as_a_value_is_refused() {
        let e = err("for x in 0..3 {\n let y = try { fail \"x\" } catch { break }\n}");
        assert!(e.contains("where a value is expected"), "{e}");
    }

    /// **A String is a sequence of its characters**, which gives length, substring and slicing
    /// without a second vocabulary of string-only verbs.
    #[test]
    fn a_string_is_a_sequence_of_its_characters() {
        assert_eq!(rendered("\"hello\" | count"), "5");
        assert_eq!(rendered("\"hello\" | take 3"), "hel");
        assert_eq!(rendered("\"hello\" | skip 2 | take 2"), "ll");
        assert_eq!(rendered("\"hello\" | last 2"), "lo");
        assert_eq!(rendered("\"banana\" | filter { |c| c != \"a\" }"), "bnn");
        assert_eq!(rendered("\"ab\" | map { |c| 1 }"), "[1, 1]");
    }

    /// A Range's elements are its values, so it can be summed or filtered without a list.
    #[test]
    fn a_range_is_a_sequence_of_its_values() {
        assert_eq!(rendered("0..5 | count"), "5");
        assert_eq!(rendered("0..=5 | count"), "6");
        assert_eq!(rendered("1..=10 | sum"), "55");
        assert_eq!(rendered("0..10 | filter { |n| n % 3 == 0 } | count"), "4");
    }

    /// A scalar is still not a sequence: treating `5` as `[5]` is the coercion §6 refuses.
    #[test]
    fn a_scalar_is_still_not_a_sequence() {
        let e = err("5 | count");
        assert!(e.contains("scalar is not one"), "{e}");
        assert!(err("{ a: 1 } | count").contains("scalar is not one"));
    }

    #[test]
    fn reductions_fold_a_sequence_to_a_value() {
        assert_eq!(rendered("[1, 2, 3] | sum"), "6");
        assert_eq!(rendered("[3, 1, 2] | min"), "1");
        assert_eq!(rendered("[3, 1, 2] | max"), "3");
        assert_eq!(rendered("[1, 2] | avg"), "1.5");
        let src = "[{ n: \"a\", size: 10 }, { n: \"b\", size: 32 }]";
        assert_eq!(rendered(&alloc::format!("{src} | sum size")), "42");
        assert_eq!(rendered(&alloc::format!("{src} | max size")), "32");
        assert_eq!(rendered(&alloc::format!("{src} | avg size")), "21.0");
    }

    /// `avg` is always a Float, and a sum that overflows is an error rather than a wrapped total (§8a).
    #[test]
    fn reductions_do_not_fabricate_numbers() {
        assert_eq!(rendered("[1, 2] | avg"), "1.5");
        assert_eq!(rendered("[2, 4] | avg"), "3.0");
        assert!(err("[9223372036854775807, 1] | sum").contains("overflows"));
        assert!(err("[1, \"x\"] | sum").contains("needs numbers"));
        assert!(err("[1, \"x\"] | max").contains("cannot order"));
    }

    /// **Empty input is where the reductions differ, and each answer is forced.**
    #[test]
    fn an_empty_sequence_reduces_only_where_that_means_something() {
        assert_eq!(rendered("[] | sum"), "0");
        assert!(err("[] | min").contains("no extreme"));
        assert!(err("[] | max").contains("no extreme"));
        assert!(err("[] | avg").contains("no divisor"));
    }

    /// `reduce` has two forms because they differ precisely on the empty case.
    #[test]
    fn reduce_folds_with_and_without_a_seed() {
        assert_eq!(rendered("[1, 2, 3] | reduce { |a, b| a + b }"), "6");
        assert_eq!(rendered("[1, 2, 3] | reduce --from 10 { |a, b| a + b }"), "16");
        assert_eq!(rendered("[] | reduce --from 10 { |a, b| a + b }"), "10");
        assert!(err("[] | reduce { |a, b| a + b }").contains("has no value"));
        assert_eq!(rendered("[\"a\", \"b\", \"c\"] | reduce { |a, b| a ++ \"-\" ++ b }"), "a-b-c");
    }

    #[test]
    fn sort_uses_every_key_it_is_given() {
        let src = "[{ d: 2, n: \"b\" }, { d: 1, n: \"z\" }, { d: 2, n: \"a\" }]";
        assert_eq!(rendered(&alloc::format!("{src} | sort d n | map {{ |r| r.n }}")), "[\"z\", \"a\", \"b\"]");
        // The pair makes the test mean something: with only the first key a stable sort keeps input
        // order for the two `d: 2` rows.
        assert_eq!(rendered(&alloc::format!("{src} | sort d | map {{ |r| r.n }}")), "[\"z\", \"b\", \"a\"]");
    }

    #[test]
    fn format_in_stage_position_formats_its_operand() {
        assert_eq!(rendered("42 | format(\"n={}\")"), "n=42");
        assert_eq!(rendered("[1, 2] | count | format(\"{} rows\")"), "2 rows");
        assert_eq!(rendered("format(\"{} {}\", 1, 2)"), "1 2");
    }

    #[test]
    fn strings_can_be_taken_apart_and_put_back() {
        assert_eq!(rendered("\"a,b,c\" | split \",\" | count"), "3");
        assert_eq!(rendered("\"a,b,c\" | split \",\" | join \"-\""), "a-b-c");
        assert_eq!(rendered("\"  x  \" | trim"), "x");
        assert_eq!(rendered("\"a-b\" | replace \"-\" \"+\""), "a+b");
        assert_eq!(rendered("\"Ab\" | upper"), "AB");
        assert_eq!(rendered("\"Ab\" | lower"), "ab");
        assert_eq!(rendered("\" 42 \" | trim | parse Int"), "42");
    }

    #[test]
    fn join_renders_the_way_concatenation_does() {
        assert_eq!(rendered("[1, 2] | join \",\""), "1,2");
        assert_eq!(rendered("[1, 2] | join \",\""), rendered("1 ++ \",\" ++ 2"));
    }

    /// An empty separator is refused rather than quietly meaning "characters" (§10b).
    #[test]
    fn split_refuses_an_empty_separator() {
        assert!(err("\"ab\" | split \"\"").contains("already a sequence"));
        assert!(err("42 | trim").contains("works on a String"));
    }

    #[test]
    fn records_can_be_walked_and_merged() {
        assert_eq!(rendered("{ a: 1, b: 2 } | keys"), "[\"a\", \"b\"]");
        assert_eq!(rendered("{ a: 1, b: 2 } | values"), "[1, 2]");
        assert_eq!(rendered("{ a: 1, b: 2 } | merge { b: 9, c: 3 }"), "{ a: 1, b: 9, c: 3 }");
        assert_eq!(rendered("{ a: 1, b: 2 } | keys | join \"+\""), "a+b");
    }

    /// Float → Int, named by **which way** it loses information (§6).
    #[test]
    fn rounding_says_which_direction_it_loses() {
        assert_eq!(rendered("2.4 | round"), "2");
        assert_eq!(rendered("2.5 | round"), "3");
        assert_eq!(rendered("-2.5 | round"), "-3");
        assert_eq!(rendered("2.9 | floor"), "2");
        assert_eq!(rendered("-2.1 | floor"), "-3");
        assert_eq!(rendered("2.1 | ceil"), "3");
        assert_eq!(rendered("-2.9 | ceil"), "-2");
        assert_eq!(rendered("-2.9 | trunc"), "-2");
        assert_eq!(rendered("2.9 | trunc"), "2");
    }

    /// `abs` keeps its type and refuses the one Int with no positive counterpart (§8a).
    #[test]
    fn abs_keeps_its_type_and_refuses_to_wrap() {
        assert_eq!(rendered("-3 | abs"), "3");
        assert_eq!(rendered("-2.5 | abs"), "2.5");
        assert!(err("-9223372036854775808 | abs").contains("overflows"));
        assert!(err("1e30 | round").contains("cannot turn"));
    }

    /// **Membership is an infix comparison** (§8a); the trap is the other `in`, in `for`.
    #[test]
    fn in_tests_membership_and_does_not_collide_with_for() {
        assert_eq!(rendered("1 in [1, 2]"), "true");
        assert_eq!(rendered("3 in [1, 2]"), "false");
        assert_eq!(rendered("\"ell\" in \"hello\""), "true");
        assert_eq!(rendered("\"a\" in { a: 1 }"), "true");
        assert_eq!(rendered("\"z\" in { a: 1 }"), "false");
        assert_eq!(rendered("5 in 0..10"), "true");
        assert_eq!(rendered("10 in 0..10"), "false");
        assert_eq!(rendered("10 in 0..=10"), "true");

        // **The pair that matters**: both `in`s in one script; `for` consumes its own first.
        assert_eq!(rendered("mut hits = 0\nfor x in [1, 2, 3] {\n if x in [2, 3] { hits = hits + 1 }\n}\nhits"), "2");
    }

    #[test]
    fn membership_composes_with_the_rest() {
        assert_eq!(rendered("[{ n: \"a\" }, { n: \"b\" }] | filter { |r| r.n in [\"b\"] } | count"), "1");
    }

    #[test]
    fn capture_returns_the_match_and_its_groups() {
        assert_eq!(rendered("\"a12b\" | capture /(\\d+)/"), "[\"12\", \"12\"]");
        assert_eq!(rendered("\"12-345\" | capture /(\\d+)-(\\d+)/"), "[\"12-345\", \"12\", \"345\"]");
        assert_eq!(rendered("\"abc\" | capture /b./"), "[\"bc\"]");
    }

    /// **No match is `null`, not an error**: an ordinary answer that `??` and `== null` handle (§9e).
    #[test]
    fn no_match_is_null_and_composes() {
        assert_eq!(rendered("\"abc\" | capture /(\\d+)/"), "null");
        assert_eq!(rendered("(\"abc\" | capture /(\\d+)/) ?? [\"none\"]"), "[\"none\"]");
        assert_eq!(rendered("let g = \"port 8080\" | capture /(\\d+)/\ng[1] | parse Int"), "8080");
    }

    /// A group that did not participate is `null`: "did not match" is not "matched nothing".
    #[test]
    fn a_group_that_did_not_participate_is_null() {
        assert_eq!(rendered("\"b\" | capture /(a)|(b)/"), "[\"b\", null, \"b\"]");
    }

    /// A pattern only lexes as one where a pattern can go (D3): after `~=` and `capture`.
    #[test]
    fn a_regex_literal_still_only_lexes_where_a_pattern_belongs() {
        assert_eq!(rendered("6 / 2"), "3");
        assert_eq!(rendered("\"x.rs\" ~= /\\.rs$/"), "true");
        assert!(err("42 | capture /x/").contains("works on a String"));
    }

    fn interrupted_after(n: u32, src: &str) -> EvalError {
        let mut i = Interp::with_host(Box::new(MockHost::new().with_interrupt_after(n)), Mode::Script);
        i.run(&crate::parse::parse_script(src).unwrap()).expect_err("interrupted")
    }

    /// `while true { }` has no exit and, with no `SIGINT`, would need a reboot (§11h).
    #[test]
    fn an_endless_loop_can_be_interrupted() {
        let e = interrupted_after(5, "while true { }");
        assert_eq!(e.kind, INTERRUPTED);
        assert_eq!(e.message, "interrupted");
    }

    /// The checkpoint covers a loop with an **empty body**, which a per-statement check misses.
    #[test]
    fn the_checkpoint_covers_both_loop_forms_and_empty_bodies() {
        assert_eq!(interrupted_after(3, "while true { }").kind, INTERRUPTED);
        assert_eq!(interrupted_after(3, "for x in 0..1000000 { }").kind, INTERRUPTED);
        assert_eq!(interrupted_after(3, "mut n = 0\nwhile true {\n n = n + 1\n}").kind, INTERRUPTED);
    }

    /// It unwinds as an ordinary error, so a script can clean up; `exit` is not catchable.
    #[test]
    fn an_interrupt_is_catchable_unlike_exit() {
        let mut i = Interp::with_host(Box::new(MockHost::new().with_interrupt_after(4)), Mode::Script);
        let v = i
            .run(&crate::parse::parse_script("try { while true { } } catch (e) { e.kind }").unwrap())
            .expect("the catch runs");
        assert_eq!(v.render(), "Interrupted");
    }

    #[test]
    fn an_evaluation_that_finishes_first_is_untouched() {
        let mut i = Interp::with_host(Box::new(MockHost::new().with_interrupt_after(1000)), Mode::Script);
        let v = i.run(&crate::parse::parse_script("1 + 1").unwrap()).expect("no interrupt");
        assert_eq!(v.render(), "2");
    }
}
