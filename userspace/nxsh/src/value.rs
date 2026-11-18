//! Runtime values, and how they render.
//!
//! [`Val::Data`] wraps `libstream::wire::Value` unchanged, so a stage's output needs no
//! conversion. What TSM1 cannot carry (a range, a closure) sits beside `Value`, not inside it, so
//! the type never gains a member that cannot survive IPC.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use libstream::wire::Value;

pub use libstream::display::{render_f64, render_i64};

use crate::ast::{Param, Stmt};

/// A closure or a named function — a language-only value (§5c).
#[derive(Clone, PartialEq, Debug)]
pub struct Func {
    /// `None` for an anonymous closure.
    pub name: Option<String>,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    /// Captured bindings, by value at creation: snapshotting avoids every closure seeing the loop
    /// variable's final value. A named `def` captures nothing.
    pub captured: Vec<(String, Val)>,
}

/// A runtime value.
#[derive(Clone, PartialEq, Debug)]
pub enum Val {
    /// Anything TSM1 can represent. The wire type itself, not a copy of it.
    Data(Value),
    /// `a..b` / `a..=b`: ascending, `i64`, no step. Language-only; it is materialised into a
    /// `List<Int>` before it can leave the process.
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
    /// A closure in a list or record would not encode, so it is refused at construction.
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

    /// The §6 type vocabulary, which is also what patterns test against.
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
            Val::Data(v) => libstream::display::value(v),
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

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn language_only_values_are_refused_by_tsm1_containers() {
        let r = Val::list(vec![Val::Range { start: 0, end: 3, inclusive: false }]);
        assert!(r.is_err());
        let f = Val::Func(Arc::new(Func { name: None, params: vec![], body: vec![], captured: vec![] }));
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
