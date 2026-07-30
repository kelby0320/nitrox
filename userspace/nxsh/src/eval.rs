//! The evaluator core — §9's statements and §8's operators, in process.
//!
//! Milestone 3 Part B. Everything here runs on the `Value` tree inside the shell: no
//! spawning, no IPC, no syscalls. That is not a simplification for testing's sake, it is
//! the design's actual claim (§5c) — most of a typical pipeline never crosses a process
//! boundary, so the in-process evaluator is the common path rather than a stub for one.
//!
//! Part C adds the boundary, Part D the generic operators, Part E functions and `match`.
//! Reaching any of those from here is a clean "not yet" error rather than a wrong answer.
//!
//! ## Two decisions worth stating
//!
//! **There is no truthiness.** `if 1 { … }` is an error, not a true branch. §6 makes
//! mismatch an error rather than a warning and §1 makes the default fail-loud; coercing
//! an `Int` to a condition is exactly the silent reinterpretation both rules exist to
//! prevent. The cost is a keystroke (`if n != 0`), the benefit is that a condition means
//! one thing.
//!
//! **Arithmetic overflow and division by zero are errors.** Not wrapping, not `inf`. A
//! shell that silently produced `-9223372036854775808` from an addition would be
//! reporting a fabricated number, the same category of thing as `date` printing 1970 when
//! the clock is unset.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use libstream::wire::{Record, Schema, TypeModifiers, Value};

use crate::ast::*;
use crate::value::{Func, Val, render_i64};

/// A runtime failure. The message is built for a person: it names the operation and the
/// types involved, which is the §6 "schema diff, not a vague mismatch" posture applied to
/// scalars.
#[derive(Clone, PartialEq, Debug)]
pub struct EvalError {
    pub message: String,
}

impl EvalError {
    fn new(message: impl Into<String>) -> EvalError {
        EvalError { message: message.into() }
    }
}

type Result<T> = core::result::Result<T, EvalError>;

/// How a statement finished. `return` unwinds to the enclosing function (§5b) without
/// being an error, so it needs its own channel out of the statement walker.
enum Flow {
    Normal(Val),
    Return(Val),
}

/// One binding.
struct Slot {
    val: Val,
    /// `let`/`const` are immutable; only `mut` may be assigned (§9d).
    mutable: bool,
    /// `const` may not be shadowed by an ordinary rebinding in the same scope.
    constant: bool,
}

/// The interpreter's state. Part C gives it a `Host`; for now it is just scopes.
pub struct Interp {
    /// A stack of scopes, innermost last. A `Vec` rather than a map because a scope holds
    /// a handful of names and linear scan beats hashing at that size — and it keeps
    /// shadowing order explicit.
    scopes: Vec<Vec<(String, Slot)>>,
}

impl Default for Interp {
    fn default() -> Self {
        Interp::new()
    }
}

impl Interp {
    pub fn new() -> Interp {
        Interp { scopes: alloc::vec![Vec::new()] }
    }

    /// Run a whole script, returning the value of its last expression statement.
    ///
    /// A script discards bare expression values (§11e) — this returns the last one anyway
    /// because it is what `nxsh -c` and the tests need to observe. The *auto-display*
    /// difference between REPL and script is Part C's, and lives at the driver, not here.
    pub fn run(&mut self, script: &Script) -> Result<Val> {
        match self.exec_block(&script.stmts)? {
            Flow::Normal(v) | Flow::Return(v) => Ok(v),
        }
    }

    /// Convenience for tests and `nxsh -c`: parse and run.
    pub fn eval_str(src: &str) -> Result<Val> {
        let script = crate::parse_script(src)
            .map_err(|e| EvalError::new(alloc::format!("line {}: {}", e.line, e.message)))?;
        Interp::new().run(&script)
    }

    // --- scopes -------------------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn lookup(&self, name: &str) -> Option<&Slot> {
        self.scopes.iter().rev().find_map(|s| {
            s.iter().rev().find(|(n, _)| n == name).map(|(_, slot)| slot)
        })
    }

    fn lookup_mut(&mut self, name: &str) -> Option<&mut Slot> {
        self.scopes.iter_mut().rev().find_map(|s| {
            s.iter_mut().rev().find(|(n, _)| n == name).map(|(_, slot)| slot)
        })
    }

    fn bind(&mut self, name: &str, val: Val, mutable: bool, constant: bool) -> Result<()> {
        let scope = self.scopes.last_mut().expect("a scope is always open");
        if let Some((_, slot)) = scope.iter().find(|(n, _)| n == name) {
            if slot.constant {
                return Err(EvalError::new(alloc::format!(
                    "`{name}` is a const and cannot be rebound in the same scope"
                )));
            }
        }
        scope.push((String::from(name), Slot { val, mutable, constant }));
        Ok(())
    }

    // --- statements ---------------------------------------------------------

    /// A block's value is its last statement's, if that statement is expression-shaped
    /// (§9a). Anything else — a `let`, a `for` — leaves the block evaluating to `Null`.
    fn exec_block(&mut self, stmts: &[Stmt]) -> Result<Flow> {
        let mut last = Val::NULL;
        for (i, s) in stmts.iter().enumerate() {
            match self.exec(s)? {
                Flow::Return(v) => return Ok(Flow::Return(v)),
                Flow::Normal(v) => {
                    last = if i + 1 == stmts.len() && s.is_expression_shaped() {
                        v
                    } else {
                        Val::NULL
                    };
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
                    // §9a: `if` without `else` is `Null` when the condition is false.
                    Ok(Flow::Normal(Val::NULL))
                }
            }
            Stmt::While { cond, body } => {
                let mut guard = 0u64;
                while self.condition(cond)? {
                    guard += 1;
                    if guard > MAX_ITERATIONS {
                        return Err(EvalError::new(
                            "loop ran past the iteration limit — this is almost certainly \
                             a runaway condition",
                        ));
                    }
                    if let Flow::Return(v) = self.scoped_block(body)? {
                        return Ok(Flow::Return(v));
                    }
                }
                Ok(Flow::Normal(Val::NULL))
            }
            Stmt::For { binding, iterable, body } => {
                let items = self.iterate(iterable)?;
                for item in items {
                    self.push_scope();
                    // The loop variable is immutable and fresh each turn, so a closure
                    // made in the body captures *this* iteration's value (§5a).
                    self.bind(binding, item, false, false)?;
                    let r = self.exec_block(body);
                    self.pop_scope();
                    if let Flow::Return(v) = r? {
                        return Ok(Flow::Return(v));
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
            Stmt::Expr(e) => Ok(Flow::Normal(self.eval(e)?)),
            Stmt::Def { .. } => Err(EvalError::new(
                "`def` is evaluated in Part E — the parser accepts it already",
            )),
            Stmt::Try { .. } => Err(EvalError::new("`try`/`catch` is evaluated in Part E")),
            Stmt::Strict(_) => Err(EvalError::new(
                "`strict` governs pipeline execution, which arrives in Part C",
            )),
            Stmt::Use { .. } => Err(EvalError::new("`use` is evaluated in Part E")),
        }
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

    /// What `for` walks. A range, a list, or a table's rows — the three things that are
    /// sequences; anything else is an error rather than a one-element loop.
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
                    if out.len() as u64 > MAX_ITERATIONS {
                        return Err(EvalError::new("range is too large to iterate"));
                    }
                }
                Ok(out)
            }
            Val::Data(Value::List(items)) => {
                Ok(items.iter().map(|v| Val::Data(v.clone())).collect())
            }
            Val::Data(Value::Table(t)) => Ok(t
                .rows
                .iter()
                .map(|row| {
                    Val::Data(Value::Record(Arc::new(Record {
                        schema: t.schema.clone(),
                        values: row.clone(),
                    })))
                })
                .collect()),
            _ => Err(EvalError::new(alloc::format!(
                "cannot iterate a {} — `for` walks a Range, a List or a Table",
                v.type_name()
            ))),
        }
    }

    /// `lvalue = value` (§9d). Legal only if the root binding is `mut`.
    fn assign(&mut self, target: &LValue, value: Val) -> Result<()> {
        // Evaluate any index expressions before touching the binding, so the borrow of
        // the slot does not overlap evaluation that could itself read it.
        let mut steps: Vec<Step> = Vec::new();
        for s in &target.path {
            steps.push(match s {
                LValueStep::Field(f) => Step::Field(f.clone()),
                LValueStep::Index(e) => Step::Index(self.index_of(e)?),
            });
        }
        let root = target.root.clone();
        let slot = self.lookup_mut(&root).ok_or_else(|| {
            EvalError::new(alloc::format!("`{root}` is not bound"))
        })?;
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
                return Err(EvalError::new(alloc::format!(
                    "cannot assign into a {}",
                    other.type_name()
                )));
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
            other => Err(EvalError::new(alloc::format!(
                "an index must be an Int or a String, got {}",
                other.type_name()
            ))),
        }
    }

    // --- expressions --------------------------------------------------------

    pub fn eval(&mut self, e: &Expr) -> Result<Val> {
        match e {
            Expr::Int(v) => Ok(Val::int(*v)),
            Expr::Float(v) => Ok(Val::float(*v)),
            Expr::Str(v) | Expr::Word(v) => Ok(Val::str(v.clone())),
            Expr::Bool(v) => Ok(Val::bool(*v)),
            Expr::Null => Ok(Val::NULL),
            Expr::Underscore => Err(EvalError::new(
                "`_` is a pipeline placeholder and has no value on its own",
            )),
            Expr::Regex(_) => Err(EvalError::new(
                "regex literals are evaluated in Part G — `~=` parses already",
            )),
            Expr::Ident(name) => match self.lookup(name) {
                Some(slot) => Ok(slot.val.clone()),
                // D4's resolution order continues past a local into commands, which
                // arrive in Part C. Naming what was searched beats "not found".
                None => Err(EvalError::new(alloc::format!(
                    "`{name}` is not a binding, and command resolution arrives in Part C"
                ))),
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
                    (Some(Value::Int(a)), Some(Value::Int(b))) => Ok(Val::Range {
                        start: *a,
                        end: *b,
                        inclusive: *inclusive,
                    }),
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
                        EvalError::new("a Table cannot be a record field — it is a stream, \
                                        not a cell")
                    })?;
                    schema = schema.field(name, tag, TypeModifiers::NONE);
                    values.push(v);
                }
                Ok(Val::Data(Value::Record(Arc::new(Record { schema, values }))))
            }
            Expr::Closure { params, body } => {
                // Capture by value at creation (§5a): snapshot every binding currently
                // visible. Simple, and it is what makes the loop-variable bug impossible
                // rather than merely unlikely.
                let mut captured = Vec::new();
                for scope in &self.scopes {
                    for (n, slot) in scope {
                        captured.push((n.clone(), slot.val.clone()));
                    }
                }
                Ok(Val::Func(Arc::new(Func {
                    name: None,
                    params: params.clone(),
                    body: body.clone(),
                    captured,
                })))
            }
            Expr::Block(stmts) => match self.scoped_block(stmts)? {
                Flow::Normal(v) | Flow::Return(v) => Ok(v),
            },
            Expr::If { cond, then, otherwise } => {
                if self.condition(cond)? {
                    match self.scoped_block(then)? {
                        Flow::Normal(v) | Flow::Return(v) => Ok(v),
                    }
                } else if let Some(b) = otherwise {
                    match self.scoped_block(b)? {
                        Flow::Normal(v) | Flow::Return(v) => Ok(v),
                    }
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
            Expr::Match { .. } => Err(EvalError::new("`match` is evaluated in Part E")),
            Expr::Try(_) => Err(EvalError::new("`?` propagation is evaluated in Part E")),
            Expr::Expect(_, _) => Err(EvalError::new("`expect` is evaluated in Part D")),
            Expr::Assert(_) => Err(EvalError::new("`assert` is evaluated in Part D")),
            Expr::Pipeline(_) => Err(EvalError::new(
                "pipelines are evaluated in Part C — the process boundary",
            )),
            // D4's resolution order, the part of it that exists yet: a **local binding
            // wins** over a command name for a bare, argument-less reference. That is
            // what stops the operator set behaving like a list of reserved words — `last`
            // and `count` are §10b operators and perfectly ordinary variable names, and
            // an argument-less mention of one outside a pipeline can only mean the value.
            //
            // Inside a pipeline the resolution is the other way round, because a stage
            // with a piped operand is unambiguously an invocation; that lives with the
            // pipeline evaluator in Part C.
            Expr::Call(c) if c.args.is_empty() && !c.forced_external => {
                match self.lookup(&c.name) {
                    Some(slot) => Ok(slot.val.clone()),
                    None => Err(unavailable(c)),
                }
            }
            Expr::Call(c) => Err(unavailable(c)),
        }
    }

    fn binary(&mut self, op: BinOp, a: &Expr, b: &Expr) -> Result<Val> {
        // Short-circuit before evaluating the right side, or `&&`/`||`/`??` would not be
        // short-circuiting at all.
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

/// "That command cannot run yet, and here is which part brings it." Names the category
/// and the part rather than failing vaguely, so a script written against the finished
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

/// Guard against a runaway `while` or an absurd range. Not a resource policy — a
/// backstop, like the parser's depth bound.
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
/// Rebuilds rather than mutating in place: `Value`'s collections are `Arc`-shared and
/// persistent by design, so another binding holding the same list must not see this
/// write. `Arc::make_mut` does the copy exactly when one is needed.
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
                .ok_or_else(|| {
                    EvalError::new(alloc::format!("record has no field `{name}`"))
                })?;
            set_path(&mut r.values[idx], rest, value)
        }
        (Step::Index(IndexKey::Name(name)), Value::Record(rec)) => {
            let r = Arc::make_mut(rec);
            let idx = r
                .schema
                .fields
                .iter()
                .position(|f| &f.name == name)
                .ok_or_else(|| {
                    EvalError::new(alloc::format!("record has no field `{name}`"))
                })?;
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
        (Step::Index(_), other) => Err(EvalError::new(alloc::format!(
            "cannot index into a {}",
            Val::Data(other.clone()).type_name()
        ))),
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

/// `x.field`. `lenient` is the `?.` form, which yields `Null` for a missing field rather
/// than failing — §9e's "handle the absent case" reading of `?`.
fn field_of(base: &Val, name: &str, lenient: bool) -> Result<Val> {
    match base {
        Val::Data(Value::Record(r)) => {
            match r.schema.fields.iter().position(|f| f.name == name) {
                Some(i) => Ok(Val::Data(r.values.get(i).cloned().unwrap_or(Value::Null))),
                None if lenient => Ok(Val::NULL),
                None => Err(EvalError::new(alloc::format!(
                    "record has no field `{name}` — it has [{}]",
                    field_names(&r.schema)
                ))),
            }
        }
        other if lenient => {
            let _ = other;
            Ok(Val::NULL)
        }
        other => Err(EvalError::new(alloc::format!(
            "cannot read field `{name}` from a {}",
            other.type_name()
        ))),
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
            Ok(Val::Data(Value::Record(Arc::new(Record {
                schema: t.schema.clone(),
                values: t.rows[idx].clone(),
            }))))
        }
        (Val::Data(Value::Record(_)), IndexKey::Name(n)) => field_of(base, n, false),
        (Val::Data(Value::Str(s)), IndexKey::Pos(i)) => {
            let chars: Vec<char> = s.chars().collect();
            let idx = bounded_index(*i, chars.len())?;
            Ok(Val::str(chars[idx].to_string()))
        }
        (b, IndexKey::Pos(_)) => Err(EvalError::new(alloc::format!(
            "cannot index a {} by position",
            b.type_name()
        ))),
        (b, IndexKey::Name(_)) => Err(EvalError::new(alloc::format!(
            "cannot index a {} by name",
            b.type_name()
        ))),
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
        (UnOp::Neg, Val::Data(Value::Int(i))) => i
            .checked_neg()
            .map(Val::int)
            .ok_or_else(|| EvalError::new("negation overflows an Int")),
        (UnOp::Neg, Val::Data(Value::Float(f))) => Ok(Val::float(-f)),
        (UnOp::Not, Val::Data(Value::Bool(b))) => Ok(Val::bool(!b)),
        (UnOp::Neg, other) => Err(EvalError::new(alloc::format!(
            "cannot negate a {}",
            other.type_name()
        ))),
        (UnOp::Not, other) => Err(EvalError::new(alloc::format!(
            "`!` needs a Bool, got {}",
            other.type_name()
        ))),
    }
}

fn binary_values(op: BinOp, l: Val, r: Val) -> Result<Val> {
    use BinOp::*;
    match op {
        Eq => return Ok(Val::bool(values_equal(&l, &r))),
        Ne => return Ok(Val::bool(!values_equal(&l, &r))),
        // §8a puts `++` at the additive tier: string concatenation, spelled apart from
        // `+` so that adding numbers and joining text are never the same operator.
        Concat => {
            let mut s = l.render();
            s.push_str(&r.render());
            return Ok(Val::str(s));
        }
        Match => {
            return Err(EvalError::new(
                "`~=` needs the regex engine, which arrives in Part G",
            ));
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

    // Strings compare, but do not do arithmetic — `+` on two strings is a common slip in
    // a language that also has `++`, so it earns a message that points at the fix.
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
            _ => Err(EvalError::new(alloc::format!(
                "cannot apply {} to two Strings",
                op_name(op)
            ))),
        };
    }

    match (a, b) {
        (Value::Int(x), Value::Int(y)) => int_op(op, *x, *y),
        (Value::Float(x), Value::Float(y)) => float_op(op, *x, *y),
        // Mixed arithmetic widens to Float, which is the only choice that does not lose
        // information — the alternative truncates silently.
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
        _ => Err(EvalError::new(alloc::format!(
            "cannot apply {} to two Ints",
            op_name(op)
        ))),
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
        _ => Err(EvalError::new(alloc::format!(
            "cannot apply {} to two Floats",
            op_name(op)
        ))),
    }
}

/// Structural equality. `Int(1)` and `Float(1.0)` compare equal *numerically*, because a
/// shell reading `1` from one program and `1.0` from another should not find them
/// different — but they still render differently and still ascribe differently.
fn values_equal(l: &Val, r: &Val) -> bool {
    match (l.as_data(), r.as_data()) {
        (Some(Value::Int(a)), Some(Value::Float(b)))
        | (Some(Value::Float(b)), Some(Value::Int(a))) => *a as f64 == *b,
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
        BinOp::And => "`&&`",
        BinOp::Or => "`||`",
        BinOp::Coalesce => "`??`",
    }
}

/// Check a value against an ascription (§6). Part B covers the scalar cases; `Table<{…}>`
/// against a stream header is Part D, where a stream exists to check.
fn check_type(v: &Val, t: &TypeExpr) -> Result<()> {
    let (name, nullable) = match t {
        TypeExpr::Named { name, nullable, .. } => (name.as_str(), *nullable),
        // A record shape needs the field-by-field comparison that produces §6's schema
        // diff; it arrives with the operators that consume schemas.
        TypeExpr::Record { nullable, .. } => {
            if v.is_null() && *nullable {
                return Ok(());
            }
            return match v {
                Val::Data(Value::Record(_)) => Ok(()),
                other => Err(EvalError::new(alloc::format!(
                    "expected a record shape, got {}",
                    other.type_name()
                ))),
            };
        }
    };
    if v.is_null() {
        // §9e: this is the whole point of the nullable suffix — a non-nullable annotation
        // rejects `Null` outright, so a false `if` without `else` is caught at the
        // binding rather than propagating.
        return if nullable {
            Ok(())
        } else {
            Err(EvalError::new(alloc::format!(
                "expected {name}, got Null — annotate `{name}?` if the value is genuinely \
                 optional"
            )))
        };
    }
    let actual = v.type_name();
    // Subset match by default (§6): a container annotation names the container, and its
    // element shape is checked where the elements are.
    if actual == name {
        return Ok(());
    }
    Err(EvalError::new(alloc::format!(
        "expected {name}, got {actual}"
    )))
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

    // --- arithmetic ---------------------------------------------------------

    #[test]
    fn arithmetic_follows_the_precedence_table() {
        assert_eq!(rendered("1 + 2 * 3"), "7");
        assert_eq!(rendered("(1 + 2) * 3"), "9");
        assert_eq!(rendered("7 / 2"), "3");
        assert_eq!(rendered("7 % 2"), "1");
        assert_eq!(rendered("-3 + 1"), "-2");
    }

    /// Mixed arithmetic widens rather than truncating: the alternative loses information
    /// silently, which is the thing this language keeps refusing to do.
    #[test]
    fn mixed_int_and_float_widens_to_float() {
        assert_eq!(rendered("1 + 1.5"), "2.5");
        assert_eq!(rendered("3 / 2.0"), "1.5");
        assert_eq!(rendered("2.0 * 2"), "4.0");
    }

    #[test]
    fn overflow_and_division_by_zero_are_errors_not_wrapped_values() {
        assert!(err("9223372036854775807 + 1").contains("overflows"));
        assert!(err("1 / 0").contains("division by zero"));
        assert!(err("1.0 / 0.0").contains("division by zero"));
        assert!(err("1 % 0").contains("remainder by zero"));
    }

    /// `+` and `++` are deliberately separate (§8a), so joining strings with `+` gets a
    /// message that points at the right operator instead of a bare type error.
    #[test]
    fn plus_does_not_join_strings() {
        assert_eq!(rendered("\"a\" ++ \"b\""), "ab");
        assert!(err("\"a\" + \"b\"").contains("`++`"));
        // `++` renders whatever it is given, so the trivial mixed case works.
        assert_eq!(rendered("\"n=\" ++ 5"), "n=5");
    }

    // --- comparison and equality --------------------------------------------

    #[test]
    fn comparison_works_on_numbers_and_strings() {
        assert_eq!(rendered("1 < 2"), "true");
        assert_eq!(rendered("2.5 >= 2"), "true");
        assert_eq!(rendered("\"a\" < \"b\""), "true");
        assert_eq!(rendered("1 == 1"), "true");
        assert_eq!(rendered("1 != 2"), "true");
    }

    /// `1` and `1.0` compare equal, because one program emitting an Int and another a
    /// Float should not make a pipeline disagree with itself.
    #[test]
    fn int_and_float_compare_numerically() {
        assert_eq!(rendered("1 == 1.0"), "true");
        assert_eq!(rendered("1 != 1.0"), "false");
        // …while still being different values.
        assert_eq!(rendered("1"), "1");
        assert_eq!(rendered("1.0"), "1.0");
    }

    // --- no truthiness ------------------------------------------------------

    /// The decision, tested: a non-Bool condition is an error rather than a coercion.
    #[test]
    fn there_is_no_truthiness() {
        let e = err("if 1 { 2 }");
        assert!(e.contains("Bool"), "{e}");
        assert!(e.contains("no truthiness"), "{e}");
        assert!(err("true && 1").contains("no truthiness"));
        // Spelling the comparison out is all it costs.
        assert_eq!(rendered("if 1 != 0 { 2 } else { 3 }"), "2");
    }

    #[test]
    fn logical_operators_short_circuit() {
        // The right operand is never evaluated, so its error never surfaces.
        assert_eq!(rendered("false && (1 / 0) == 0"), "false");
        assert_eq!(rendered("true || (1 / 0) == 0"), "true");
    }

    #[test]
    fn null_coalescing_short_circuits_too() {
        assert_eq!(rendered("null ?? 5"), "5");
        assert_eq!(rendered("3 ?? (1 / 0)"), "3");
    }

    // --- bindings and mutability (§9d) --------------------------------------

    #[test]
    fn let_is_immutable_and_mut_is_not() {
        assert_eq!(rendered("let x = 5\nx"), "5");
        assert_eq!(rendered("mut x = 5\nx = 6\nx"), "6");
        let e = err("let x = 5\nx = 6");
        assert!(e.contains("`let`"), "{e}");
        assert!(e.contains("mut"), "{e}");
    }

    /// An operator name is not a reserved word. `last` is a §10b operator *and* an
    /// ordinary name for a variable, and assigning to one must not parse as a call.
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

    #[test]
    fn an_unbound_name_says_what_was_searched() {
        let e = err("nope");
        assert!(e.contains("not a binding"), "{e}");
    }

    // --- blocks are expressions (§9a) ---------------------------------------

    #[test]
    fn a_block_evaluates_to_its_last_expression() {
        assert_eq!(rendered("if true { 1 + 1 }"), "2");
        // A block ending in a non-expression statement is Null.
        assert_eq!(rendered("if true { let x = 1 }"), "null");
        // …and `if` without `else`, when false, is Null (§9a).
        assert_eq!(rendered("if false { 1 }"), "null");
    }

    #[test]
    fn if_else_chains_pick_the_branch_that_ran() {
        assert_eq!(rendered("if false { 1 } else if true { 2 } else { 3 }"), "2");
        assert_eq!(rendered("if false { 1 } else if false { 2 } else { 3 }"), "3");
    }

    /// §9e: the nullable suffix is what defuses if-without-else, and it only helps if a
    /// non-nullable annotation genuinely rejects `Null`.
    #[test]
    fn ascription_rejects_null_unless_the_type_is_nullable() {
        assert_eq!(rendered("let x: Int? = if false { 5 }\nx"), "null");
        let e = err("let x: Int = if false { 5 }");
        assert!(e.contains("got Null"), "{e}");
        assert!(e.contains("Int?"), "{e}");
        assert!(err("let x: Int = \"s\"").contains("expected Int, got String"));
    }

    // --- scoping ------------------------------------------------------------

    #[test]
    fn inner_scopes_shadow_and_do_not_leak() {
        assert_eq!(rendered("let x = 1\nif true { let x = 2 }\nx"), "1");
        assert!(err("if true { let y = 2 }\ny").contains("not a binding"));
    }

    // --- control flow -------------------------------------------------------

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

    // --- collections --------------------------------------------------------

    #[test]
    fn lists_and_records_build_and_read_back() {
        assert_eq!(rendered("[1, 2, 3]"), "[1, 2, 3]");
        assert_eq!(rendered("[1, 2, 3][1]"), "2");
        assert_eq!(rendered("{ a: 1, b: \"x\" }"), "{ a: 1, b: \"x\" }");
        assert_eq!(rendered("{ a: 1 }.a"), "1");
        // §8e's record shorthand.
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

    /// §9e: `?.` is the "handle the absent case" spelling, so it yields Null rather than
    /// failing — including on a Null base, which is the point of chaining.
    #[test]
    fn safe_navigation_yields_null_instead_of_failing() {
        assert_eq!(rendered("{ a: 1 }?.missing"), "null");
        assert_eq!(rendered("null?.a"), "null");
        assert_eq!(rendered("{ a: 1 }?.missing ?? \"unknown\""), "unknown");
    }

    // --- field and index mutation (§9d) -------------------------------------

    #[test]
    fn field_and_index_assignment_reach_into_collections() {
        assert_eq!(rendered("mut r = { a: 1 }\nr.a = 5\nr.a"), "5");
        assert_eq!(rendered("mut l = [1, 2, 3]\nl[0] = 9\nl"), "[9, 2, 3]");
        assert_eq!(
            rendered("mut r = { inner: { v: 1 } }\nr.inner.v = 7\nr.inner.v"),
            "7"
        );
    }

    /// The collections are persistent and `Arc`-shared, so a write through one binding
    /// must not be visible through another that was copied from it.
    #[test]
    fn assignment_does_not_leak_through_a_shared_copy() {
        assert_eq!(rendered("mut a = [1, 2]\nlet b = a\na[0] = 9\nb"), "[1, 2]");
        assert_eq!(
            rendered("mut a = { v: 1 }\nlet b = a\na.v = 9\nb.v"),
            "1"
        );
    }

    #[test]
    fn assigning_through_an_immutable_root_is_refused() {
        assert!(err("let r = { a: 1 }\nr.a = 2").contains("only a `mut`"));
    }

    // --- language-only values (§5c) -----------------------------------------

    #[test]
    fn a_closure_is_a_value_but_not_a_tsm1_one() {
        assert_eq!(rendered("let f = { |x| x + 1 }\nf"), "<closure>");
        // …and cannot be put where TSM1 would have to encode it.
        assert!(err("let f = { |x| x }\n[f]").contains("TSM1"));
    }

    /// §5a: capture is by value at creation, which is what makes the classic
    /// loop-variable bug impossible rather than merely unlikely. The closure made on the
    /// last iteration must hold that iteration's value, not a live reference.
    #[test]
    fn closures_capture_by_value_at_creation() {
        let v = ok("mut made = null\nfor i in 0..3 { made = { |x| x } }\nmade");
        assert!(matches!(v, Val::Func(_)));
        // The captured environment is a snapshot: three iterations, three snapshots.
        let Val::Func(f) = v else { panic!() };
        assert!(f.captured.iter().any(|(n, _)| n == "i"));
        let (_, captured_i) = f.captured.iter().rev().find(|(n, _)| n == "i").unwrap();
        assert_eq!(captured_i.render(), "2");
    }

    // --- the parts that are not here yet ------------------------------------

    /// Reaching a later part is a clean "not yet", naming the part — not a wrong answer
    /// and not a panic.
    #[test]
    fn later_parts_report_themselves_rather_than_misbehaving() {
        assert!(err("ls | display").contains("Part C"));
        assert!(err("match 1 { _ => 2 }").contains("Part E"));
        assert!(err("\"a\" ~= /b/").contains("Part G"));
    }

    #[test]
    fn the_deliverable_computes() {
        // The plan's stated Part B deliverable.
        assert_eq!(rendered("let x = 2 + 3\nx"), "5");
    }
}
