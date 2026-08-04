//! Generic value operators — §3's third command category, §10b's additions.
//!
//! Milestone 3 Part D. These are the operators that "do generic dispatch over `Value`'s
//! structural shape" (§5c): `filter` works on whatever fields showed up, `sort` orders by
//! a column it has never heard of, `save` serialises a schema it did not know at build
//! time.
//!
//! That genericity is exactly why they **cannot** be external programs. An external
//! program defines its own fixed schema; these have to walk one that arrives at runtime,
//! and a closure argument could not cross a process boundary at all (§5c). So they run
//! in-process on the `Value` tree — which is also why the dense middle of a pipeline costs
//! no spawns.
//!
//! ## Rows, generically
//!
//! Everything here goes through [`rows`] and [`rebuild`]. A `Table` yields one `Record`
//! per row and rebuilds as a `Table` with its schema preserved; a `List` yields its
//! elements and rebuilds as a `List`; a `String` yields its characters and rebuilds as a
//! `String`; a `Range` yields its values. Operators therefore never mention which they
//! were handed, which is the whole point — and a scalar is an error rather than a
//! one-element sequence, because silently treating `5` as `[5]` is the kind of coercion
//! §6 spends its fail-loud rule on.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use libstream::wire::{Record, Schema, StreamFlags, Table, TypeModifiers, TypeTag, Value};

use crate::value::{Val, render_i64};

/// An operator failure. Plain strings: the evaluator wraps them.
pub type OpResult<T> = Result<T, String>;

/// How many values a `Range` will materialise into before an operator refuses it.
///
/// The same backstop `for` applies to a range it walks: a range is a *language* value with
/// no storage behind it, so turning one into rows is where an absurd one has to be caught.
pub const MAX_RANGE_ROWS: i64 = 10_000_000;

/// Decompose a value into rows, generically.
///
/// A `Table` gives one `Record` per row — which is what makes `filter name == "x"` work on
/// a stream without the operator knowing the schema. A `List` gives its elements, a
/// `String` its characters, and a `Range` its values (§10b).
pub fn rows(v: &Val) -> OpResult<Vec<Val>> {
    match v {
        Val::Data(Value::Table(t)) => Ok(t
            .rows
            .iter()
            .map(|r| {
                Val::Data(Value::Record(Arc::new(Record {
                    schema: t.schema.clone(),
                    values: r.clone(),
                })))
            })
            .collect()),
        Val::Data(Value::List(items)) => {
            Ok(items.iter().map(|i| Val::Data(i.clone())).collect())
        }
        // **A String is a sequence of its characters** (§10b). This is where length,
        // substring and slicing come from — `count`, `take`, `skip`, `last` over text —
        // rather than from a second vocabulary of string-only verbs. Characters, not
        // bytes: `"abc"[0]` already indexes characters, and two answers to "what is an
        // element of a String" is one too many.
        Val::Data(Value::Str(text)) => {
            Ok(text.chars().map(|c| Val::str(alloc::string::String::from(c))).collect())
        }
        // A Range's elements are its values. It stays language-only (§8c) — materialising
        // one here is exactly what it means to put it in front of an operator.
        Val::Range { start, end, inclusive } => {
            let last = if *inclusive { *end } else { *end - 1 };
            let n = last.saturating_sub(*start).saturating_add(1);
            if n > MAX_RANGE_ROWS {
                return Err(String::from("range is too large to iterate"));
            }
            let mut out = Vec::new();
            let mut i = *start;
            while i <= last {
                out.push(Val::int(i));
                i += 1;
            }
            Ok(out)
        }
        other => Err(alloc::format!(
            "expected a sequence, got {} — an operator works over rows, and a scalar is \
             not one. A Table, a List, a String (its characters) or a Range is one",
            other.type_name()
        )),
    }
}

/// Reassemble rows into the shape they came from.
///
/// A `Table` keeps its schema, so a filtered stream is still the same stream minus rows.
/// A row whose record no longer matches the schema forces a `List`, since a table with
/// ragged rows is not a table.
pub fn rebuild(original: &Val, out: Vec<Val>) -> OpResult<Val> {
    // **The result comes back in the shape it went in.** Filtering a String yields a
    // String; mapping one to something that is not text falls back to a List, which is the
    // same rule a ragged Table already takes.
    if let Val::Data(Value::Str(_)) = original {
        let mut joined = String::new();
        for r in &out {
            match r.as_data() {
                Some(Value::Str(s)) => joined.push_str(s),
                _ => return as_list(out),
            }
        }
        return Ok(Val::str(joined));
    }
    if let Val::Data(Value::Table(t)) = original {
        let mut table_rows = Vec::with_capacity(out.len());
        for r in &out {
            match r {
                Val::Data(Value::Record(rec)) if rec.schema == t.schema => {
                    table_rows.push(rec.values.clone());
                }
                _ => return as_list(out),
            }
        }
        return Ok(Val::Data(Value::Table(Arc::new(Table {
            flags: t.flags,
            schema: t.schema.clone(),
            rows: table_rows,
        }))));
    }
    as_list(out)
}

fn as_list(out: Vec<Val>) -> OpResult<Val> {
    let mut items = Vec::with_capacity(out.len());
    for v in out {
        items.push(v.into_data().map_err(String::from)?);
    }
    Ok(Val::Data(Value::List(Arc::from(items))))
}

/// Build a table from records, inferring the schema from the first (§6's duck typing:
/// the shape is whatever showed up).
pub fn table_from_records(out: Vec<Val>) -> OpResult<Val> {
    let Some(Val::Data(Value::Record(first))) = out.first() else {
        return as_list(out);
    };
    let schema = first.schema.clone();
    let mut table_rows = Vec::with_capacity(out.len());
    for r in &out {
        match r {
            Val::Data(Value::Record(rec)) if rec.schema == schema => {
                table_rows.push(rec.values.clone());
            }
            _ => return as_list(out),
        }
    }
    Ok(Val::Data(Value::Table(Arc::new(Table {
        flags: StreamFlags::NONE,
        schema,
        rows: table_rows,
    }))))
}

/// `take N` / `skip N` / `last N` (§10b).
pub fn take(v: &Val, n: i64) -> OpResult<Val> {
    let r = rows(v)?;
    let n = clamp_count(n, r.len())?;
    rebuild(v, r.into_iter().take(n).collect())
}

pub fn skip(v: &Val, n: i64) -> OpResult<Val> {
    let r = rows(v)?;
    let n = clamp_count(n, r.len())?;
    rebuild(v, r.into_iter().skip(n).collect())
}

/// `last N` — the tail. §10a's replacement for `tail`, and the reason `head -n -N` needed
/// no separate program either.
pub fn last(v: &Val, n: i64) -> OpResult<Val> {
    let r = rows(v)?;
    let n = clamp_count(n, r.len())?;
    let skip = r.len().saturating_sub(n);
    rebuild(v, r.into_iter().skip(skip).collect())
}

fn clamp_count(n: i64, len: usize) -> OpResult<usize> {
    if n < 0 {
        return Err(String::from("a row count cannot be negative"));
    }
    Ok((n as usize).min(len))
}

/// `count` — the number of rows. A scalar count of a stream, which is why it is the one
/// operator that does not return the shape it was given.
pub fn count(v: &Val) -> OpResult<Val> {
    Ok(Val::int(rows(v)?.len() as i64))
}

/// `sum` / `min` / `max` / `avg` (§10b) — the reductions `count` was alone in.
///
/// Without them "the total size of these files" needs a `mut` and a `for` loop, which is
/// the shape a pipeline language exists to remove. A bareword names a *column*, the same
/// reading `sort size` has; with no column the rows are the values.
///
/// **Empty input is where these differ, and each answer is forced.** The sum of nothing is
/// zero; there is no minimum of an empty set and no average without a divisor, so those are
/// errors — the same refusal to fabricate a value §8a makes for division by zero.
pub fn sum(v: &Val, field: Option<&str>) -> OpResult<Val> {
    let mut int_total: i64 = 0;
    let mut float_total: f64 = 0.0;
    let mut is_float = false;
    for value in column(v, field, "sum")? {
        match value {
            Value::Int(i) => {
                if is_float {
                    float_total += i as f64;
                } else {
                    int_total = int_total.checked_add(i).ok_or_else(|| {
                        String::from("`sum` overflows an Int — Nitrox does not wrap, \
                                     because a wrapped total is a fabricated number")
                    })?;
                }
            }
            Value::Float(f) => {
                if !is_float {
                    is_float = true;
                    float_total = int_total as f64;
                }
                float_total += f;
            }
            other => return Err(not_a_number("sum", &other)),
        }
    }
    Ok(if is_float { Val::float(float_total) } else { Val::int(int_total) })
}

/// `avg` — always a `Float`, even over Ints. The alternative truncates silently.
pub fn avg(v: &Val, field: Option<&str>) -> OpResult<Val> {
    let values = column(v, field, "avg")?;
    if values.is_empty() {
        return Err(String::from(
            "`avg` of nothing has no value — there is no divisor, and inventing one would \
             be the fabricated number §8a refuses for division by zero",
        ));
    }
    let n = values.len() as f64;
    let mut total = 0.0;
    for value in values {
        match value {
            Value::Int(i) => total += i as f64,
            Value::Float(f) => total += f,
            other => return Err(not_a_number("avg", &other)),
        }
    }
    Ok(Val::float(total / n))
}

pub fn min(v: &Val, field: Option<&str>) -> OpResult<Val> {
    extreme(v, field, "min", -1)
}

pub fn max(v: &Val, field: Option<&str>) -> OpResult<Val> {
    extreme(v, field, "max", 1)
}

/// `min`/`max` share everything but which direction wins, and both order with the same
/// `compare` as `sort` — so mixed types are an error rather than an ordering invented at
/// the point of comparison.
fn extreme(v: &Val, field: Option<&str>, op: &str, want: i32) -> OpResult<Val> {
    let values = column(v, field, op)?;
    let mut best: Option<Value> = None;
    for value in values {
        best = Some(match best {
            None => value,
            Some(b) => {
                if compare(&value, &b)? == want {
                    value
                } else {
                    b
                }
            }
        });
    }
    best.map(Val::Data).ok_or_else(|| {
        alloc::format!("`{op}` of nothing has no value — an empty sequence has no extreme")
    })
}

fn not_a_number(op: &str, v: &Value) -> String {
    alloc::format!("`{op}` needs numbers, got {}", Val::Data(v.clone()).type_name())
}

/// The values a reduction folds over: a named column, or the rows themselves.
fn column(v: &Val, field: Option<&str>, op: &str) -> OpResult<Vec<Value>> {
    let r = rows(v)?;
    let mut out = Vec::with_capacity(r.len());
    for row in &r {
        out.push(key_of(row, field, op)?);
    }
    Ok(out)
}

/// `dedupe` (§10b) — row-equality deduplication, the same generic-row shape as
/// `sort`/`filter`. Order-preserving: the first occurrence wins, because a shell that
/// reordered while deduplicating would be doing two things under one name.
pub fn dedupe(v: &Val) -> OpResult<Val> {
    let r = rows(v)?;
    let mut seen: Vec<Val> = Vec::new();
    for row in r {
        if !seen.iter().any(|s| *s == row) {
            seen.push(row);
        }
    }
    rebuild(v, seen)
}

// --- strings (§10b) -------------------------------------------------------
//
// These are *scalar* operators: they take a String and give something back, rather than
// walking rows. v1.1 could **test** text with `~=` and never take it apart, and with `parse`
// also missing, text that arrived as text stayed that way forever.

/// `split SEP` — String → `List<String>`. The inverse of [`join`].
pub fn split(v: &Val, sep: &str) -> OpResult<Val> {
    let s = as_str(v, "split")?;
    // An empty separator would give one element per character with no way to put them back,
    // which is what `rows` already does properly (§10b) — so it is refused rather than
    // quietly meaning something else.
    if sep.is_empty() {
        return Err(String::from(
            "`split` needs a separator — to split into characters, the String is already a \
             sequence of them",
        ));
    }
    let parts: Vec<Value> =
        s.split(sep).map(|p| Value::Str(String::from(p))).collect();
    Ok(Val::Data(Value::List(Arc::from(parts))))
}

/// `join SEP` — a sequence → String.
///
/// Elements are rendered the way `++` renders them (§8a), so `[1, 2] | join ","` is
/// `"1,2"`: one definition of "this value as text" rather than a second that disagrees.
pub fn join(v: &Val, sep: &str) -> OpResult<Val> {
    let mut out = String::new();
    for (i, row) in rows(v)?.iter().enumerate() {
        if i > 0 {
            out.push_str(sep);
        }
        out.push_str(&row.render());
    }
    Ok(Val::str(out))
}

/// `trim` — leading and trailing whitespace.
///
/// This is what makes `parse`'s strictness workable (§6): the two exist as a pair, so text
/// with stray spaces is trimmed deliberately rather than silently accepted.
pub fn trim(v: &Val) -> OpResult<Val> {
    Ok(Val::str(as_str(v, "trim")?.trim()))
}

/// `replace FROM TO` — **literal, not a pattern**. Pattern replacement is a different verb
/// and waits on `capture`'s submatch work (§12).
pub fn replace(v: &Val, from: &str, to: &str) -> OpResult<Val> {
    if from.is_empty() {
        return Err(String::from("`replace` needs something to replace"));
    }
    Ok(Val::str(as_str(v, "replace")?.replace(from, to)))
}

/// `upper` / `lower` — **ASCII only**, said where a user meets it rather than only in the
/// design doc. Full case folding needs Unicode tables this system does not carry, and a
/// `lower` that quietly mangles non-ASCII is worse than one that documents its range.
pub fn upper(v: &Val) -> OpResult<Val> {
    Ok(Val::str(as_str(v, "upper")?.to_ascii_uppercase()))
}

pub fn lower(v: &Val) -> OpResult<Val> {
    Ok(Val::str(as_str(v, "lower")?.to_ascii_lowercase()))
}

fn as_str<'a>(v: &'a Val, op: &str) -> OpResult<&'a str> {
    match v.as_data() {
        Some(Value::Str(s)) => Ok(s.as_str()),
        _ => Err(alloc::format!("`{op}` works on a String, got {}", v.type_name())),
    }
}

// --- records (§10b) -------------------------------------------------------

/// `keys` / `values` — in schema order, because the schema *is* the order.
///
/// A Record was constructible and readable by known field name only, so no script could
/// walk one it did not itself write — which also blocked any generic handling of a schema
/// that arrived at runtime.
pub fn keys(v: &Val) -> OpResult<Val> {
    let r = as_record(v, "keys")?;
    let out: Vec<Value> =
        r.schema.fields.iter().map(|f| Value::Str(f.name.clone())).collect();
    Ok(Val::Data(Value::List(Arc::from(out))))
}

pub fn values(v: &Val) -> OpResult<Val> {
    let r = as_record(v, "values")?;
    Ok(Val::Data(Value::List(Arc::from(r.values.clone()))))
}

/// `merge OTHER` — the right operand wins a conflict, and the result schema is the union.
pub fn merge(a: &Val, b: &Val) -> OpResult<Val> {
    let left = as_record(a, "merge")?;
    let right = as_record(b, "merge")?;
    let mut schema = Schema::new();
    let mut values: Vec<Value> = Vec::new();
    for (i, f) in left.schema.fields.iter().enumerate() {
        // A field the right operand also carries takes its value from there, and keeps the
        // left's position — a merge should not reorder what it did not change.
        let from_right = right
            .schema
            .fields
            .iter()
            .position(|g| g.name == f.name)
            .and_then(|j| right.values.get(j));
        let (tag, value) = match from_right {
            Some(v) => (v.type_tag().unwrap_or(TypeTag::Null), v.clone()),
            None => (f.ty, left.values.get(i).cloned().unwrap_or(Value::Null)),
        };
        schema = schema.field(&f.name, tag, TypeModifiers::NONE);
        values.push(value);
    }
    for (j, g) in right.schema.fields.iter().enumerate() {
        if left.schema.fields.iter().any(|f| f.name == g.name) {
            continue;
        }
        let v = right.values.get(j).cloned().unwrap_or(Value::Null);
        schema = schema.field(&g.name, v.type_tag().unwrap_or(TypeTag::Null), TypeModifiers::NONE);
        values.push(v);
    }
    Ok(Val::Data(Value::Record(Arc::new(Record { schema, values }))))
}

fn as_record<'a>(v: &'a Val, op: &str) -> OpResult<&'a Record> {
    match v.as_data() {
        Some(Value::Record(r)) => Ok(r),
        _ => Err(alloc::format!("`{op}` works on a Record, got {}", v.type_name())),
    }
}

// --- numbers (§10b) -------------------------------------------------------

/// `round` / `floor` / `ceil` / `trunc` — `Float` → `Int`, **named by which way they lose
/// information** (§6). A cast that silently truncates is the fabricated value §8a refuses.
///
/// Hand-rolled because `f64::round` and friends live in `std`: this crate is `no_std`, so
/// the arithmetic is done here rather than pulling in a libm.
pub fn round(v: &Val) -> OpResult<Val> {
    let x = as_f64(v, "round")?;
    let t = trunc_f64(x, "round")?;
    // Half **away from zero** — stated out loud because half-to-even is the other
    // defensible answer, and silence would make it a coin flip settled by whoever wrote it.
    let frac = x - (t as f64);
    let away = if frac >= 0.5 {
        1
    } else if frac <= -0.5 {
        -1
    } else {
        0
    };
    t.checked_add(away)
        .map(Val::int)
        .ok_or_else(|| String::from("`round` overflows an Int"))
}

pub fn floor(v: &Val) -> OpResult<Val> {
    let x = as_f64(v, "floor")?;
    let t = trunc_f64(x, "floor")?;
    Ok(Val::int(if x < 0.0 && (t as f64) != x { t - 1 } else { t }))
}

pub fn ceil(v: &Val) -> OpResult<Val> {
    let x = as_f64(v, "ceil")?;
    let t = trunc_f64(x, "ceil")?;
    Ok(Val::int(if x > 0.0 && (t as f64) != x { t + 1 } else { t }))
}

pub fn trunc(v: &Val) -> OpResult<Val> {
    let x = as_f64(v, "trunc")?;
    Ok(Val::int(trunc_f64(x, "trunc")?))
}

/// `abs` — the one that keeps its type, since it does not lose information.
pub fn abs(v: &Val) -> OpResult<Val> {
    match v.as_data() {
        // `-i64::MIN` has no positive counterpart, so it is an error rather than itself
        // (§8a: no wrapping, because a wrapped result is a fabricated number).
        Some(Value::Int(i)) => i
            .checked_abs()
            .map(Val::int)
            .ok_or_else(|| String::from("`abs` overflows an Int")),
        Some(Value::Float(f)) => Ok(Val::float(if *f < 0.0 { -f } else { *f })),
        _ => Err(alloc::format!("`abs` needs a number, got {}", v.type_name())),
    }
}

fn as_f64(v: &Val, op: &str) -> OpResult<f64> {
    match v.as_data() {
        Some(Value::Float(f)) => Ok(*f),
        Some(Value::Int(i)) => Ok(*i as f64),
        _ => Err(alloc::format!("`{op}` needs a number, got {}", v.type_name())),
    }
}

/// Toward zero, refusing anything an `Int` cannot hold.
fn trunc_f64(x: f64, op: &str) -> OpResult<i64> {
    if !(x.is_finite() && x > -9.3e18 && x < 9.3e18) {
        return Err(alloc::format!(
            "`{op}` cannot turn {} into an Int",
            crate::value::render_f64(x)
        ));
    }
    Ok(x as i64)
}

/// `select FIELD...` — projection. Missing fields are an error rather than silently
/// absent columns: asking for a field that is not there is a mistake worth hearing about,
/// and §6's subset-match rule is about *accepting extra* fields, not inventing missing
/// ones.
pub fn select(v: &Val, fields: &[String]) -> OpResult<Val> {
    if fields.is_empty() {
        return Err(String::from("`select` needs at least one field name"));
    }
    let r = rows(v)?;
    let mut out = Vec::with_capacity(r.len());
    for row in &r {
        let Val::Data(Value::Record(rec)) = row else {
            return Err(alloc::format!(
                "`select` needs rows with fields, got {}",
                row.type_name()
            ));
        };
        let mut schema = Schema::new();
        let mut values = Vec::with_capacity(fields.len());
        for f in fields {
            let idx = rec.schema.fields.iter().position(|d| &d.name == f).ok_or_else(|| {
                alloc::format!(
                    "no field `{f}` — this row has [{}]",
                    field_names(&rec.schema)
                )
            })?;
            let def = &rec.schema.fields[idx];
            schema = schema.field(&def.name, def.ty, def.modifiers);
            values.push(rec.values.get(idx).cloned().unwrap_or(Value::Null));
        }
        out.push(Val::Data(Value::Record(Arc::new(Record { schema, values }))));
    }
    table_from_records(out)
}

/// `sort FIELD... [--reverse]`, or `sort` over bare values.
///
/// **Every key given is used**, in order: `sort dept name` sorts by department and then by
/// name within it. Taking the first and silently discarding the rest is neither of the two
/// acceptable behaviours (§10b).
pub fn sort(v: &Val, fields: &[String], reverse: bool) -> OpResult<Val> {
    let mut r = rows(v)?;
    // A stable insertion sort: `Vec::sort_by` needs a total order, and comparing two
    // arbitrary `Value`s does not have one (a String and an Int are simply not ordered).
    // Doing it by hand keeps the "these two cannot be compared" case an error rather than
    // an arbitrary answer.
    let mut err: Option<String> = None;
    let mut sorted: Vec<Val> = Vec::with_capacity(r.len());
    for item in r.drain(..) {
        let key = sort_keys(&item, fields)?;
        let mut at = sorted.len();
        for i in 0..sorted.len() {
            let other = sort_keys(&sorted[i], fields)?;
            match compare_keys(&key, &other) {
                Ok(ord) => {
                    let before = if reverse { ord > 0 } else { ord < 0 };
                    if before {
                        at = i;
                        break;
                    }
                }
                Err(e) => {
                    err = Some(e);
                    at = sorted.len();
                    break;
                }
            }
        }
        if let Some(e) = err {
            return Err(e);
        }
        sorted.insert(at, item);
    }
    rebuild(v, sorted)
}

/// The ordering keys for one row, one per named field — or the row itself when no field
/// was named.
fn sort_keys(row: &Val, fields: &[String]) -> OpResult<Vec<Value>> {
    if fields.is_empty() {
        return Ok(alloc::vec![sort_key(row, None)?]);
    }
    let mut keys = Vec::with_capacity(fields.len());
    for f in fields {
        keys.push(sort_key(row, Some(f.as_str()))?);
    }
    Ok(keys)
}

/// Compare rows key by key: the first that differs decides, which is what makes the
/// second key a tie-break rather than a second sort.
fn compare_keys(a: &[Value], b: &[Value]) -> OpResult<i32> {
    for (x, y) in a.iter().zip(b) {
        let ord = compare(x, y)?;
        if ord != 0 {
            return Ok(ord);
        }
    }
    Ok(0)
}

fn sort_key(row: &Val, field: Option<&str>) -> OpResult<Value> {
    key_of(row, field, "sort")
}

/// One value out of a row: a named field, or the row itself.
///
/// Shared by `sort` and the reductions so that "which column?" is answered once — and so
/// `sum size` and `sort size` fail the same way on a row that has no such field.
fn key_of(row: &Val, field: Option<&str>, op: &str) -> OpResult<Value> {
    match field {
        None => row
            .as_data()
            .cloned()
            .ok_or_else(|| alloc::format!("cannot `{op}` a value TSM1 cannot represent")),
        Some(f) => {
            let Val::Data(Value::Record(rec)) = row else {
                return Err(alloc::format!(
                    "`{op} {f}` needs rows with fields, got {}",
                    row.type_name()
                ));
            };
            let idx = rec.schema.fields.iter().position(|d| &d.name == f).ok_or_else(|| {
                alloc::format!("no field `{f}` — this row has [{}]", field_names(&rec.schema))
            })?;
            Ok(rec.values.get(idx).cloned().unwrap_or(Value::Null))
        }
    }
}

/// Order two values. `Null` sorts first; numbers compare numerically across Int/Float;
/// strings lexicographically. Anything else is an error, not an arbitrary order.
fn compare(a: &Value, b: &Value) -> OpResult<i32> {
    Ok(match (a, b) {
        (Value::Null, Value::Null) => 0,
        (Value::Null, _) => -1,
        (_, Value::Null) => 1,
        (Value::Int(x), Value::Int(y)) => cmp_ord(x, y),
        (Value::Float(x), Value::Float(y)) => cmp_f64(*x, *y),
        (Value::Int(x), Value::Float(y)) => cmp_f64(*x as f64, *y),
        (Value::Float(x), Value::Int(y)) => cmp_f64(*x, *y as f64),
        (Value::Str(x), Value::Str(y)) => cmp_ord(x, y),
        (Value::Bool(x), Value::Bool(y)) => cmp_ord(x, y),
        _ => {
            return Err(alloc::format!(
                "cannot order {} against {}",
                Val::Data(a.clone()).type_name(),
                Val::Data(b.clone()).type_name()
            ));
        }
    })
}

fn cmp_ord<T: PartialOrd>(a: &T, b: &T) -> i32 {
    if a < b {
        -1
    } else if a > b {
        1
    } else {
        0
    }
}

fn cmp_f64(a: f64, b: f64) -> i32 {
    if a < b {
        -1
    } else if a > b {
        1
    } else {
        0
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

/// `format(template, args…)` — §8d, positional and indexed only.
///
/// Deliberately not named capture. §8d's reasoning: Rust gets away with `format!("{name}")`
/// because it is a *macro* rewriting a literal at compile time, never looking up a string
/// against a live scope. Without a macro system the same syntax would mean resolving a
/// *string* against the caller's bindings at run time — categorically different from every
/// other name resolution in the language, and a refactoring trap where renaming a variable
/// breaks a string with no error pointing anywhere useful.
pub fn format(template: &str, args: &[Val]) -> OpResult<String> {
    let mut out = String::new();
    let bytes = template.as_bytes();
    let mut i = 0usize;
    let mut next_positional = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'{' if i + 1 < bytes.len() && bytes[i + 1] == b'{' => {
                out.push('{');
                i += 2;
            }
            b'}' if i + 1 < bytes.len() && bytes[i + 1] == b'}' => {
                out.push('}');
                i += 2;
            }
            b'{' => {
                let end = template[i..]
                    .find('}')
                    .map(|o| i + o)
                    .ok_or_else(|| String::from("unclosed `{` in a format template"))?;
                let spec = &template[i + 1..end];
                let idx = if spec.is_empty() {
                    let n = next_positional;
                    next_positional += 1;
                    n
                } else {
                    spec.parse::<usize>()
                        .map_err(|_| alloc::format!("`{{{spec}}}` is not a positional index"))?
                };
                let v = args.get(idx).ok_or_else(|| {
                    alloc::format!(
                        "format needs an argument {} but only {} were given",
                        render_i64(idx as i64),
                        render_i64(args.len() as i64)
                    )
                })?;
                out.push_str(&v.render());
                i = end + 1;
            }
            b'}' => return Err(String::from("unmatched `}` in a format template")),
            _ => {
                let ch = template[i..].chars().next().unwrap_or('\u{fffd}');
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    // §8d: arity mismatch is an error, same fail-loud default used throughout. An unused
    // argument is as much a mistake as a missing one — it means the template and the call
    // disagree.
    if next_positional < args.len() && !template.contains('{') {
        return Err(alloc::format!(
            "format was given {} argument(s) the template does not use",
            render_i64(args.len() as i64)
        ));
    }
    Ok(out)
}

/// Render a value for `display`: a table as aligned columns, anything else as itself.
///
/// This is where a `Table` finally gets a real rendering — `Val::render` deliberately
/// gives only a summary, because laying out columns is a *display* decision and belongs
/// with the operator that ends a chain rather than with the value.
pub fn display(v: &Val) -> String {
    let Val::Data(Value::Table(t)) = v else {
        let mut s = v.render();
        s.push('\n');
        return s;
    };
    let headers: Vec<String> = t.schema.fields.iter().map(|f| f.name.clone()).collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    let cells: Vec<Vec<String>> = t
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|c| Val::Data(c.clone()).render())
                .collect::<Vec<String>>()
        })
        .collect();
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            if i < widths.len() {
                widths[i] = widths[i].max(c.chars().count());
            }
        }
    }
    let mut out = String::new();
    for (i, h) in headers.iter().enumerate() {
        pad_into(&mut out, h, widths[i], i + 1 == headers.len());
    }
    out.push('\n');
    for row in &cells {
        for (i, c) in row.iter().enumerate() {
            if i < widths.len() {
                pad_into(&mut out, c, widths[i], i + 1 == row.len());
            }
        }
        out.push('\n');
    }
    out
}

fn pad_into(out: &mut String, text: &str, width: usize, last: bool) {
    out.push_str(text);
    if last {
        return;
    }
    for _ in text.chars().count()..width {
        out.push(' ');
    }
    out.push_str("  ");
}

/// Serialise for `save` (B5). `.tsm` is the native stream; `.txt` is one rendered row per
/// line. `.csv`/`.json` are deliberately not here — each is a real serialiser, and §4
/// scoped them as separate work rather than free.
pub fn encode_for(path: &str, v: &Val) -> OpResult<Vec<u8>> {
    if path.ends_with(".tsm") {
        let Val::Data(Value::Table(t)) = v else {
            return Err(alloc::format!(
                "`.tsm` saves a Table, got {} — the native format *is* the stream",
                v.type_name()
            ));
        };
        let mut buf = Vec::new();
        t.encode(&mut buf).map_err(|_| String::from("could not encode the stream"))?;
        return Ok(buf);
    }
    if path.ends_with(".txt") {
        let mut out = String::new();
        for row in rows(v).unwrap_or_else(|_| alloc::vec![v.clone()]) {
            out.push_str(&row.render());
            out.push('\n');
        }
        return Ok(out.into_bytes());
    }
    Err(alloc::format!(
        "`save` does not know the format of `{path}` — `.tsm` and `.txt` are supported; \
         `.csv` and `.json` are separate work (B5)"
    ))
}

/// Parse for `open` (B5).
///
/// §4: text is wrapped into a single-column `Table<String>`, because there is no such
/// thing as a raw untyped byte stream in this pipeline model — which is also what let
/// `open`'s multi-path form absorb `cat` entirely (§10a).
pub fn decode_from(path: &str, bytes: &[u8]) -> OpResult<Val> {
    if path.ends_with(".tsm") {
        let t = Table::decode(bytes).map_err(|_| alloc::format!("`{path}` is not a TSM1 stream"))?;
        return Ok(Val::Data(Value::Table(Arc::new(t))));
    }
    let text = core::str::from_utf8(bytes)
        .map_err(|_| alloc::format!("`{path}` is not valid UTF-8"))?;
    let schema = Schema::new().field("line", TypeTag::String, TypeModifiers::NONE);
    let rows: Vec<Vec<Value>> = text
        .lines()
        .map(|l| alloc::vec![Value::Str(l.to_string())])
        .collect();
    Ok(Val::Data(Value::Table(Arc::new(Table {
        flags: StreamFlags::NONE,
        schema,
        rows,
    }))))
}

/// Concatenate two streams for `open a b` — §4's multi-path form.
pub fn concat(a: Val, b: Val) -> OpResult<Val> {
    let (Val::Data(Value::Table(ta)), Val::Data(Value::Table(tb))) = (&a, &b) else {
        return Err(String::from("`open` concatenates streams"));
    };
    if ta.schema != tb.schema {
        return Err(String::from(
            "`open` cannot concatenate files with different shapes — their schemas differ",
        ));
    }
    let mut rows = ta.rows.clone();
    rows.extend(tb.rows.iter().cloned());
    Ok(Val::Data(Value::Table(Arc::new(Table {
        flags: ta.flags,
        schema: ta.schema.clone(),
        rows,
    }))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn table(names: &[&str], sizes: &[i64]) -> Val {
        let schema = Schema::new()
            .field("name", TypeTag::String, TypeModifiers::NONE)
            .field("size", TypeTag::Int, TypeModifiers::NONE);
        let rows = names
            .iter()
            .zip(sizes)
            .map(|(n, s)| vec![Value::Str(n.to_string()), Value::Int(*s)])
            .collect();
        Val::Data(Value::Table(Arc::new(Table { flags: StreamFlags::NONE, schema, rows })))
    }

    #[test]
    fn take_skip_and_last_slice_a_stream() {
        let t = table(&["a", "b", "c", "d"], &[1, 2, 3, 4]);
        assert_eq!(count(&take(&t, 2).unwrap()).unwrap().render(), "2");
        assert_eq!(count(&skip(&t, 3).unwrap()).unwrap().render(), "1");
        // Over-taking is a clamp, not an error: "give me five" from three rows is three.
        assert_eq!(count(&take(&t, 99).unwrap()).unwrap().render(), "4");
        // `last` is the tail, which is what §10a dissolved `tail` into.
        let l = last(&t, 1).unwrap();
        assert_eq!(select(&l, &[String::from("name")]).unwrap().render(), "<table 1 row [name]>");
    }

    #[test]
    fn sort_orders_by_a_field_it_has_never_heard_of() {
        let t = table(&["c", "a", "b"], &[3, 1, 2]);
        let s = sort(&t, &[String::from("size")], false).unwrap();
        let r = rows(&s).unwrap();
        assert_eq!(r[0].render(), "{ name: \"a\", size: 1 }");
        assert_eq!(r[2].render(), "{ name: \"c\", size: 3 }");
        let s = sort(&t, &[String::from("size")], true).unwrap();
        assert_eq!(rows(&s).unwrap()[0].render(), "{ name: \"c\", size: 3 }");
    }

    /// Two values with no order between them is an error, not an arbitrary answer.
    #[test]
    fn ordering_incomparable_values_is_an_error() {
        let schema = Schema::new().field("v", TypeTag::Null, TypeModifiers::NONE);
        let t = Val::Data(Value::Table(Arc::new(Table {
            flags: StreamFlags::NONE,
            schema,
            rows: vec![vec![Value::Str("a".into())], vec![Value::Int(1)]],
        })));
        assert!(sort(&t, &[String::from("v")], false).is_err());
    }

    #[test]
    fn select_projects_and_refuses_a_field_that_is_not_there() {
        let t = table(&["a", "b"], &[1, 2]);
        let s = select(&t, &[String::from("name")]).unwrap();
        assert_eq!(rows(&s).unwrap()[0].render(), "{ name: \"a\" }");
        let e = select(&t, &[String::from("nope")]).unwrap_err();
        assert!(e.contains("no field `nope`"), "{e}");
        assert!(e.contains("name, size"), "{e}");
    }

    #[test]
    fn dedupe_keeps_the_first_occurrence_and_the_order() {
        let t = table(&["a", "b", "a"], &[1, 2, 1]);
        let d = dedupe(&t).unwrap();
        let r = rows(&d).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].render(), "{ name: \"a\", size: 1 }");
    }

    /// A filtered table is still the same table minus rows — schema preserved, so a
    /// later `select` or ascription still works.
    #[test]
    fn rebuilding_preserves_the_schema() {
        let t = table(&["a", "b"], &[1, 2]);
        let one = take(&t, 1).unwrap();
        assert!(matches!(one, Val::Data(Value::Table(_))));
        assert_eq!(one.render(), "<table 1 row [name, size]>");
    }

    #[test]
    fn a_scalar_is_not_a_one_row_stream() {
        let e = rows(&Val::int(5)).unwrap_err();
        assert!(e.contains("scalar is not one"), "{e}");
    }

    // --- format (§8d) -------------------------------------------------------

    #[test]
    fn format_is_positional_and_indexed_only() {
        assert_eq!(format("hello {}", &[Val::str("world")]).unwrap(), "hello world");
        assert_eq!(
            format("{1}, you are {0}", &[Val::int(30), Val::str("bob")]).unwrap(),
            "bob, you are 30"
        );
        // An index may repeat — the reason indexed form exists at all.
        assert_eq!(
            format("{0} met {0}", &[Val::str("ann")]).unwrap(),
            "ann met ann"
        );
    }

    #[test]
    fn format_escapes_braces_and_fails_loud_on_arity() {
        assert_eq!(format("{{}}", &[]).unwrap(), "{}");
        assert!(format("{}", &[]).is_err());
        assert!(format("{5}", &[Val::int(1)]).is_err());
        assert!(format("{", &[]).is_err());
    }

    // --- save / open (B5) ---------------------------------------------------

    #[test]
    fn tsm_round_trips_through_save_and_open() {
        let t = table(&["a", "b"], &[1, 2]);
        let bytes = encode_for("x.tsm", &t).unwrap();
        let back = decode_from("x.tsm", &bytes).unwrap();
        assert_eq!(back.render(), t.render());
        assert_eq!(rows(&back).unwrap()[1].render(), "{ name: \"b\", size: 2 }");
    }

    /// §4: text becomes a single-column `Table<String>`, because there is no untyped byte
    /// stream in this model — which is what let `open a b` absorb `cat`.
    #[test]
    fn text_is_wrapped_into_a_single_column_table() {
        let v = decode_from("notes.txt", b"one\ntwo\n").unwrap();
        assert_eq!(v.render(), "<table 2 rows [line]>");
        assert_eq!(rows(&v).unwrap()[0].render(), "{ line: \"one\" }");
    }

    #[test]
    fn an_unknown_extension_says_which_formats_exist() {
        let e = encode_for("x.csv", &table(&["a"], &[1])).unwrap_err();
        assert!(e.contains(".tsm"), "{e}");
        assert!(e.contains("B5"), "{e}");
    }

    #[test]
    fn open_concatenates_matching_streams_and_refuses_mismatched_ones() {
        let a = table(&["a"], &[1]);
        let b = table(&["b"], &[2]);
        assert_eq!(count(&concat(a.clone(), b).unwrap()).unwrap().render(), "2");
        let other = decode_from("x.txt", b"line\n").unwrap();
        assert!(concat(a, other).is_err());
    }

    // --- display ------------------------------------------------------------

    #[test]
    fn display_lays_a_table_out_in_columns() {
        let t = table(&["short", "a-longer-name"], &[1, 200]);
        let out = display(&t);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "name           size");
        assert_eq!(lines[1], "short          1");
        assert_eq!(lines[2], "a-longer-name  200");
    }

    #[test]
    fn display_of_a_scalar_is_just_the_value() {
        assert_eq!(display(&Val::int(5)), "5\n");
    }
}
