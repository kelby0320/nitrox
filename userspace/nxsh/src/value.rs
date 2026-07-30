//! Runtime values, and how they render.
//!
//! Milestone 3 Part B. The shape of [`Val`] is the design's §6 rule made structural
//! rather than remembered:
//!
//! > `Value` is exactly what TSM1 can represent — no more, no less.
//!
//! So [`Val::Data`] wraps `libstream::wire::Value` **unchanged** — the same type the wire
//! codec and every coreutil already use, not a parallel copy the shell would have to keep
//! in step. Everything the language has that TSM1 cannot carry sits *outside* that
//! variant: a range and a closure are real first-class values (§5c, §8c) that simply
//! cannot cross a process boundary, and putting them beside `Value` rather than inside it
//! is what stops the type system from acquiring a member that provably cannot survive
//! IPC.
//!
//! The practical payoff is that a pipeline stage's output needs no conversion: what comes
//! off the wire *is* a `Val::Data`.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use libstream::wire::{Record, Table, Value};

use crate::ast::{Param, Stmt};

/// A closure or a named function — a language-only value (§5c).
#[derive(Clone, PartialEq, Debug)]
pub struct Func {
    /// `None` for an anonymous closure.
    pub name: Option<String>,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    /// Captured bindings, **by value at creation** (§5a). Snapshotting here rather than
    /// holding a reference to the defining scope is what avoids the classic
    /// "every closure sees the loop variable's final value" bug without a borrow checker.
    /// A named `def` captures nothing: its parameter list is a complete account of its
    /// inputs.
    pub captured: Vec<(String, Val)>,
}

/// A runtime value.
#[derive(Clone, PartialEq, Debug)]
pub enum Val {
    /// Anything TSM1 can represent. The wire type itself, not a copy of it.
    Data(Value),
    /// `a..b` / `a..=b`. Always ascending, `i64`, no step — deliberately minimal (§8c).
    /// Language-only: a range would be materialised into a `List<Int>` before it could
    /// leave the process.
    Range { start: i64, end: i64, inclusive: bool },
    /// A function or closure (§5c).
    Func(Arc<Func>),
}

impl Val {
    pub const NULL: Val = Val::Data(Value::Null);

    pub fn int(v: i64) -> Val {
        Val::Data(Value::Int(v))
    }

    pub fn float(v: f64) -> Val {
        Val::Data(Value::Float(v))
    }

    pub fn bool(v: bool) -> Val {
        Val::Data(Value::Bool(v))
    }

    pub fn str(v: impl Into<String>) -> Val {
        Val::Data(Value::Str(v.into()))
    }

    pub fn list(items: Vec<Val>) -> Result<Val, &'static str> {
        let mut out = Vec::with_capacity(items.len());
        for v in items {
            out.push(v.into_data()?);
        }
        Ok(Val::Data(Value::List(Arc::from(out))))
    }

    /// The `Value` inside, or an error naming why this one has none.
    ///
    /// The error message is the §5c rule in the place a user meets it: a list or a record
    /// is a TSM1 container, so putting a closure in one would produce a value that cannot
    /// be encoded — better refused at construction than discovered at a pipe.
    pub fn into_data(self) -> Result<Value, &'static str> {
        match self {
            Val::Data(v) => Ok(v),
            Val::Range { .. } => Err("a range cannot be stored in a list or record — \
                                      it is not something TSM1 can represent"),
            Val::Func(_) => Err("a function cannot be stored in a list or record — \
                                 it is not something TSM1 can represent"),
        }
    }

    pub fn as_data(&self) -> Option<&Value> {
        match self {
            Val::Data(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Val::Data(Value::Bool(b)) => Some(*b),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Val::Data(Value::Null))
    }

    /// The name used in error messages — the §6 type vocabulary, which is also what
    /// patterns test against (§9f), so one word means one thing everywhere.
    pub fn type_name(&self) -> &'static str {
        match self {
            Val::Data(Value::Null) => "Null",
            Val::Data(Value::Bool(_)) => "Bool",
            Val::Data(Value::Int(_)) => "Int",
            Val::Data(Value::Float(_)) => "Float",
            Val::Data(Value::Str(_)) => "String",
            Val::Data(Value::Bytes(_)) => "Bytes",
            Val::Data(Value::Handle(_)) => "Handle",
            Val::Data(Value::List(_)) => "List",
            Val::Data(Value::Record(_)) => "Record",
            Val::Data(Value::Table(_)) => "Table",
            Val::Range { .. } => "Range",
            Val::Func(_) => "Function",
        }
    }

    /// The display form: what `display` would show, and what `++` concatenates.
    pub fn render(&self) -> String {
        match self {
            Val::Data(v) => render_value(v, false),
            Val::Range { start, end, inclusive } => {
                let mut s = render_i64(*start);
                s.push_str(if *inclusive { "..=" } else { ".." });
                s.push_str(&render_i64(*end));
                s
            }
            Val::Func(f) => match &f.name {
                Some(n) => {
                    let mut s = String::from("<def ");
                    s.push_str(n);
                    s.push('>');
                    s
                }
                None => String::from("<closure>"),
            },
        }
    }
}

/// Render a `Value`. `nested` quotes strings, because `["a", "b"]` and `[a, b]` are
/// different things and a list of strings should look like one.
fn render_value(v: &Value, nested: bool) -> String {
    match v {
        Value::Null => String::from("null"),
        Value::Bool(true) => String::from("true"),
        Value::Bool(false) => String::from("false"),
        Value::Int(i) => render_i64(*i),
        Value::Float(f) => render_f64(*f),
        Value::Str(s) => {
            if nested {
                let mut out = String::from("\"");
                out.push_str(s);
                out.push('"');
                out
            } else {
                s.clone()
            }
        }
        Value::Bytes(b) => {
            let mut s = String::from("<");
            s.push_str(&render_i64(b.len() as i64));
            s.push_str(" bytes>");
            s
        }
        Value::Handle(h) => {
            let mut s = String::from("<handle ");
            s.push_str(&render_i64(*h as i64));
            s.push('>');
            s
        }
        Value::List(items) => {
            let mut s = String::from("[");
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                s.push_str(&render_value(item, true));
            }
            s.push(']');
            s
        }
        Value::Record(r) => render_record(r),
        Value::Table(t) => render_table(t),
    }
}

fn render_record(r: &Record) -> String {
    let mut s = String::from("{ ");
    for (i, f) in r.schema.fields.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&f.name);
        s.push_str(": ");
        s.push_str(&render_value(r.values.get(i).unwrap_or(&Value::Null), true));
    }
    s.push_str(" }");
    s
}

/// A one-line summary. Rendering a table *properly* — aligned columns — is `display`'s
/// job in Part D, and belongs there rather than half-done here.
fn render_table(t: &Table) -> String {
    let mut s = String::from("<table ");
    s.push_str(&render_i64(t.rows.len() as i64));
    s.push_str(if t.rows.len() == 1 { " row [" } else { " rows [" });
    for (i, f) in t.schema.fields.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push_str(&f.name);
    }
    s.push_str("]>");
    s
}

pub fn render_i64(v: i64) -> String {
    let mut buf = [0u8; 20];
    let negative = v < 0;
    let mut n = 0usize;
    let mut u = v.unsigned_abs();
    loop {
        buf[n] = b'0' + (u % 10) as u8;
        u /= 10;
        n += 1;
        if u == 0 {
            break;
        }
    }
    let mut s = String::new();
    if negative {
        s.push('-');
    }
    for i in (0..n).rev() {
        s.push(buf[i] as char);
    }
    s
}

/// Render a float (C6).
///
/// Two rules beyond what `core`'s formatter does on its own, both of them about a typed
/// shell rather than about arithmetic:
///
/// - **A whole float keeps its point.** `core` renders `1.0f64` as `1`, which in this
///   language is indistinguishable from `Int(1)` — and the two are genuinely different
///   values that a schema, an ascription and a pattern all tell apart (§6, §9f). So a
///   rendering with no `.`, `e` or sign of non-finiteness gets `.0` appended.
/// - **Non-finite values are named, not printed as symbols.** `inf` and `NaN` are what
///   they are called in the type vocabulary and in every error message.
pub fn render_f64(v: f64) -> String {
    if v.is_nan() {
        return String::from("NaN");
    }
    if v.is_infinite() {
        return String::from(if v > 0.0 { "inf" } else { "-inf" });
    }
    // `core::fmt` carries the shortest-round-trip float formatter; no `std` needed.
    let s = v.to_string();
    if s.bytes().any(|c| c == b'.' || c == b'e' || c == b'E') {
        return s;
    }
    let mut s = s;
    s.push_str(".0");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_whole_float_is_distinguishable_from_an_int() {
        // The reason this rule exists: `1` and `1.0` are different values in a language
        // whose schemas, ascriptions and patterns all tell `Int` from `Float`.
        assert_eq!(Val::float(1.0).render(), "1.0");
        assert_eq!(Val::int(1).render(), "1");
        assert_eq!(Val::float(-2.0).render(), "-2.0");
    }

    #[test]
    fn floats_render_at_full_precision() {
        assert_eq!(Val::float(1.5).render(), "1.5");
        assert_eq!(Val::float(0.1).render(), "0.1");
        assert_eq!(Val::float(2.675).render(), "2.675");
        assert_eq!(Val::float(1e21).render(), "1000000000000000000000.0");
    }

    #[test]
    fn non_finite_floats_are_named() {
        assert_eq!(Val::float(f64::NAN).render(), "NaN");
        assert_eq!(Val::float(f64::INFINITY).render(), "inf");
        assert_eq!(Val::float(f64::NEG_INFINITY).render(), "-inf");
    }

    #[test]
    fn integers_render_including_the_extremes() {
        assert_eq!(Val::int(0).render(), "0");
        assert_eq!(Val::int(-1).render(), "-1");
        assert_eq!(Val::int(i64::MAX).render(), "9223372036854775807");
        assert_eq!(Val::int(i64::MIN).render(), "-9223372036854775808");
    }

    /// A bare string renders as itself; inside a collection it is quoted, because
    /// `[a, b]` and `["a", "b"]` are not the same value.
    #[test]
    fn strings_quote_only_when_nested() {
        assert_eq!(Val::str("hi").render(), "hi");
        let l = Val::list(vec![Val::str("a"), Val::int(1)]).unwrap();
        assert_eq!(l.render(), "[\"a\", 1]");
    }

    /// §5c, enforced where a user meets it: a closure cannot go into a TSM1 container.
    #[test]
    fn language_only_values_are_refused_by_tsm1_containers() {
        let r = Val::list(vec![Val::Range { start: 0, end: 3, inclusive: false }]);
        assert!(r.is_err());
        let f = Val::Func(Arc::new(Func {
            name: None,
            params: vec![],
            body: vec![],
            captured: vec![],
        }));
        assert!(Val::list(vec![f]).is_err());
    }

    #[test]
    fn type_names_match_the_ascription_vocabulary() {
        assert_eq!(Val::int(1).type_name(), "Int");
        assert_eq!(Val::str("x").type_name(), "String");
        assert_eq!(Val::NULL.type_name(), "Null");
        assert_eq!(Val::Range { start: 0, end: 1, inclusive: false }.type_name(), "Range");
    }
}
