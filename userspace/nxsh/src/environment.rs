//! The environment: the record the shell reads and hands every stage, and `$env`, its binding.
//! **An assignment to `$env` writes the record**, so a program, `cd` and `~` see what `$env` says.

use crate::eval::{EvalError, IndexKey, Interp, Step, set_path};
use crate::value::Val;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use libstream::setup::{SETUP_PAYLOAD_MAX, SetupPayload};
use libstream::wire::{Record, TypeModifiers, TypeTag, Value, WireError};

impl Interp {
    /// Set a conventional environment entry, replacing it if present.
    pub(crate) fn set_env_entry(&mut self, name: &str, value: Value) {
        self.env = with_entry(core::mem::take(&mut self.env), name, value);
    }

    /// `$env… = value`, written to a copy of the environment, which then replaces it whole, or is
    /// refused whole.
    pub(crate) fn assign_env(&mut self, steps: &[Step], value: Val) -> Result<(), EvalError> {
        let value = value.into_data().map_err(EvalError::new)?;
        let mut env = Value::Record(Arc::new(self.env.clone()));
        match steps {
            // A new variable, which `set_path` refuses as a field the record lacks.
            [Step::Field(name) | Step::Index(IndexKey::Name(name))] if field(&self.env, name).is_none() => {
                env = Value::Record(Arc::new(with_entry(self.env.clone(), name, value)));
            }
            _ => set_path(&mut env, steps, value)?,
        }
        self.env = judged(&self.env, env).map_err(EvalError::new)?;
        Ok(())
    }
}

/// `record` with `name` set to `value`, added as a field of `value`'s type if it has none.
fn with_entry(mut record: Record, name: &str, value: Value) -> Record {
    match record.schema.fields.iter().position(|f| f.name == name) {
        Some(i) => record.values[i] = value,
        None => {
            let tag = value.type_tag().unwrap_or(TypeTag::Null);
            record.schema = record.schema.clone().field(name, tag, TypeModifiers::NONE);
            record.values.push(value);
        }
    }
    record
}

fn field<'a>(record: &'a Record, name: &str) -> Option<&'a Value> {
    let i = record.schema.fields.iter().position(|f| f.name == name)?;
    record.values.get(i)
}

/// The environment `after` makes, if the shell and its programs can go on reading it: a record of
/// named variables, each once and of the type it had; with `before`'s `PWD`; a `HOME`, if any, that
/// is a String, as `cd` and `~` read it; and the whole of it a setup message can carry.
pub(crate) fn judged(before: &Record, after: Value) -> Result<Record, String> {
    let Value::Record(record) = after else {
        return Err(format!("`$env` is a Record, and a {} cannot replace it", Val::Data(after).type_name()));
    };
    let fields = &record.schema.fields;
    for (i, f) in fields.iter().enumerate() {
        if f.name.is_empty() {
            return Err(String::from("a variable in `$env` has a name, and one would have none"));
        }
        if fields[..i].iter().any(|g| g.name == f.name) {
            return Err(format!("`$env.{}` would be there twice", f.name));
        }
    }
    if field(&record, "PWD") != field(before, "PWD") {
        return Err(String::from("`$env.PWD` is `cd`'s to change, which goes only to a directory that exists"));
    }
    for (f, v) in fields.iter().zip(&record.values) {
        // Whichever form assigned it: a merge types a field by its new value.
        let ty = before.schema.fields.iter().find(|b| b.name == f.name).map_or(f.ty, |b| b.ty);
        match v {
            Value::Table(_) => return Err(format!("`$env.{}` would hold a Table, a stream and not a value", f.name)),
            Value::Null if f.modifiers.contains(TypeModifiers::NULLABLE) => {}
            Value::Null => return Err(format!("`$env.{}` would hold nothing, and a variable holds a value", f.name)),
            _ if v.type_tag() != Some(ty) => {
                let got = Val::Data(v.clone()).type_name();
                return Err(format!("`$env.{}` is a {ty:?}, and a {got} does not fit it", f.name));
            }
            _ => {}
        }
    }
    if field(&record, "HOME").is_some_and(|h| h.as_str().is_none()) {
        return Err(String::from("`$env.HOME` is a String, the path `cd` and `~` go to"));
    }
    let setup = SetupPayload { streams: 0, argv: Vec::new(), env: (*record).clone(), terminal: false, shown: false };
    if let Err(e) = setup.encode() {
        return Err(format!("`$env` could not be handed to a program: {}", unsendable(e)));
    }
    Ok(Arc::unwrap_or_clone(record))
}

/// Why an environment does not encode, in words.
fn unsendable(e: WireError) -> String {
    match e {
        WireError::SetupTooLarge => format!("it is larger than the {SETUP_PAYLOAD_MAX} bytes a setup message holds"),
        WireError::NestedTable => String::from("a Table inside it is a stream, not a value"),
        WireError::SchemaMismatch => String::from("a value inside it does not fit its field's type"),
        WireError::TooLarge => String::from("it is nested too deep"),
        _ => String::from("it does not encode"),
    }
}
