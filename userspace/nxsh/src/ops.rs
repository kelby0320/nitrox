//! Generic value operators — §3's third command category, §10b's additions.
//!
//! These "do generic dispatch over `Value`'s structural shape" (§5c): `filter` works on whatever
//! fields showed up, `sort` orders by a column it has never heard of. That is why they cannot be
//! external programs: they walk a schema that arrives at runtime, and a closure argument cannot
//! cross a process boundary. They run in-process on the `Value` tree.
//!
//! Everything goes through [`rows`] and [`rebuild`]: a `Table` yields one `Record` per row and
//! rebuilds as a `Table`, a `List` its elements, a `String` its characters, a `Range` its values.
//! A scalar is an error, not a one-element sequence: that is the coercion §6's fail-loud rule
//! refuses.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use libstream::wire::{Record, Schema, StreamFlags, Table, TypeModifiers, TypeTag, Value};

use crate::value::{Val, render_i64};

/// An operator failure. Plain strings: the evaluator wraps them.
pub type OpResult<T> = Result<T, String>;

/// How many values a `Range` will materialise into before an operator refuses it: the same
/// backstop `for` applies, since a range has no storage behind it.
pub const MAX_RANGE_ROWS: i64 = 10_000_000;

/// Decompose a value into rows, generically: a `Table` gives one `Record` per row, a `List` its
/// elements, a `String` its characters, and a `Range` its values (§10b).
pub fn rows(v: &Val) -> OpResult<Vec<Val>> {
    match v {
        Val::Data(Value::Table(t)) => Ok(t
            .rows
            .iter()
            .map(|r| Val::Data(Value::Record(Arc::new(Record { schema: t.schema.clone(), values: r.clone() }))))
            .collect()),
        Val::Data(Value::List(items)) => Ok(items.iter().map(|i| Val::Data(i.clone())).collect()),
        // A String is a sequence of its characters (§10b), which is where `count`, `take`, `skip`
        // and `last` over text come from. Characters, not bytes, as `"abc"[0]` indexes.
        Val::Data(Value::Str(text)) => Ok(text.chars().map(|c| Val::str(alloc::string::String::from(c))).collect()),
        // A Range stays language-only (§8c); materialising it is what putting it in front of an
        // operator means.
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

/// Reassemble rows into the shape they came from. A row whose record no longer matches the
/// table's schema forces a `List`, since a table with ragged rows is not a table.
pub fn rebuild(original: &Val, out: Vec<Val>) -> OpResult<Val> {
    // The result comes back in the shape it went in; a String mapped to non-text falls back to
    // a List, as a ragged Table does.
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
    Ok(Val::Data(Value::Table(Arc::new(Table { flags: StreamFlags::NONE, schema, rows: table_rows }))))
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

/// `last N` — the tail.
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

/// `count` — the number of rows; the one operator that does not return the shape it was given.
pub fn count(v: &Val) -> OpResult<Val> {
    Ok(Val::int(rows(v)?.len() as i64))
}

/// `sum` / `min` / `max` / `avg` (§10b). A bareword names a column, as `sort size` does; with no
/// column the rows are the values.
///
/// Empty input: the sum of nothing is zero; there is no minimum of an empty set and no average
/// without a divisor, so those are errors, as division by zero is (§8a).
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
                        String::from(
                            "`sum` overflows an Int — Nitrox does not wrap, \
                                     because a wrapped total is a fabricated number",
                        )
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

/// `min`/`max` order with the same `compare` as `sort`, so mixed types are an error.
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
    best.map(Val::Data)
        .ok_or_else(|| alloc::format!("`{op}` of nothing has no value — an empty sequence has no extreme"))
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

/// `dedupe` (§10b) — row-equality deduplication; the first occurrence wins, so order is kept.
pub fn dedupe(v: &Val) -> OpResult<Val> {
    let r = rows(v)?;
    let mut seen: Vec<Val> = Vec::new();
    for row in r {
        if !seen.contains(&row) {
            seen.push(row);
        }
    }
    rebuild(v, seen)
}

/// `split SEP` — String → `List<String>`. The inverse of [`join`].
pub fn split(v: &Val, sep: &str) -> OpResult<Val> {
    let s = as_str(v, "split")?;
    // An empty separator is refused rather than quietly meaning something else.
    if sep.is_empty() {
        return Err(String::from(
            "`split` needs a separator — to split into characters, the String is already a \
             sequence of them",
        ));
    }
    let parts: Vec<Value> = s.split(sep).map(|p| Value::Str(String::from(p))).collect();
    Ok(Val::Data(Value::List(Arc::from(parts))))
}

/// `join SEP` — a sequence → String, rendering elements as `++` does (§8a).
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

/// `trim` — leading and trailing whitespace; the pair to `parse`'s strictness (§6).
pub fn trim(v: &Val) -> OpResult<Val> {
    Ok(Val::str(as_str(v, "trim")?.trim()))
}

/// `replace FROM TO` — **literal, not a pattern**. Pattern replacement is a different verb
/// and waits on `capture`'s submatch work (§12).
/// TODO(regex-replace): `docs/rationale/deferred-decisions.md`.
pub fn replace(v: &Val, from: &str, to: &str) -> OpResult<Val> {
    if from.is_empty() {
        return Err(String::from("`replace` needs something to replace"));
    }
    Ok(Val::str(as_str(v, "replace")?.replace(from, to)))
}

/// `upper` / `lower` — ASCII only: full case folding needs Unicode tables this system does not
/// carry.
/// TODO(unicode-case): `docs/rationale/deferred-decisions.md`.
pub fn upper(v: &Val) -> OpResult<Val> {
    Ok(Val::str(as_str(v, "upper")?.to_ascii_uppercase()))
}

/// `lower` — ASCII-only, like [`upper`].
pub fn lower(v: &Val) -> OpResult<Val> {
    Ok(Val::str(as_str(v, "lower")?.to_ascii_lowercase()))
}

fn as_str<'a>(v: &'a Val, op: &str) -> OpResult<&'a str> {
    match v.as_data() {
        Some(Value::Str(s)) => Ok(s.as_str()),
        _ => Err(alloc::format!("`{op}` works on a String, got {}", v.type_name())),
    }
}

/// `keys` / `values` — in schema order, so a script can walk a record it did not write.
pub fn keys(v: &Val) -> OpResult<Val> {
    let r = as_record(v, "keys")?;
    let out: Vec<Value> = r.schema.fields.iter().map(|f| Value::Str(f.name.clone())).collect();
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
        // A field the right operand also carries takes its value from there and keeps the left's
        // position.
        let from_right = right.schema.fields.iter().position(|g| g.name == f.name).and_then(|j| right.values.get(j));
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

/// `round` / `floor` / `ceil` / `trunc` — `Float` → `Int`, named by which way they lose
/// information (§6). Hand-rolled: `f64::round` lives in `std`.
pub fn round(v: &Val) -> OpResult<Val> {
    let x = as_f64(v, "round")?;
    let t = trunc_f64(x, "round")?;
    // Half away from zero.
    let frac = x - (t as f64);
    let away = if frac >= 0.5 {
        1
    } else if frac <= -0.5 {
        -1
    } else {
        0
    };
    t.checked_add(away).map(Val::int).ok_or_else(|| String::from("`round` overflows an Int"))
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
        // `-i64::MIN` has no positive counterpart: an error, not a wrapped result (§8a).
        Some(Value::Int(i)) => i.checked_abs().map(Val::int).ok_or_else(|| String::from("`abs` overflows an Int")),
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
        return Err(alloc::format!("`{op}` cannot turn {} into an Int", crate::value::render_f64(x)));
    }
    Ok(x as i64)
}

/// `select FIELD...` — projection. A missing field is an error: §6's subset match accepts extra
/// fields, not missing ones.
pub fn select(v: &Val, fields: &[String]) -> OpResult<Val> {
    if fields.is_empty() {
        return Err(String::from("`select` needs at least one field name"));
    }
    let r = rows(v)?;
    let mut out = Vec::with_capacity(r.len());
    for row in &r {
        let Val::Data(Value::Record(rec)) = row else {
            return Err(alloc::format!("`select` needs rows with fields, got {}", row.type_name()));
        };
        let mut schema = Schema::new();
        let mut values = Vec::with_capacity(fields.len());
        for f in fields {
            let idx = rec
                .schema
                .fields
                .iter()
                .position(|d| &d.name == f)
                .ok_or_else(|| alloc::format!("no field `{f}` — this row has [{}]", field_names(&rec.schema)))?;
            let def = &rec.schema.fields[idx];
            schema = schema.field(&def.name, def.ty, def.modifiers);
            values.push(rec.values.get(idx).cloned().unwrap_or(Value::Null));
        }
        out.push(Val::Data(Value::Record(Arc::new(Record { schema, values }))));
    }
    table_from_records(out)
}

/// `sort FIELD... [--reverse]`, or `sort` over bare values. Every key given is used, in order.
pub fn sort(v: &Val, fields: &[String], reverse: bool) -> OpResult<Val> {
    let mut r = rows(v)?;
    // A stable insertion sort: two arbitrary `Value`s have no total order, and doing it by hand
    // keeps "cannot be compared" an error rather than an arbitrary answer.
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

/// The ordering keys for one row, one per named field, or the row itself with none.
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

/// Compare rows key by key: the first that differs decides.
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

/// One value out of a row: a named field, or the row itself. Shared with the reductions, so
/// `sum size` and `sort size` fail the same way on a row without the field.
fn key_of(row: &Val, field: Option<&str>, op: &str) -> OpResult<Value> {
    match field {
        None => row.as_data().cloned().ok_or_else(|| alloc::format!("cannot `{op}` a value TSM1 cannot represent")),
        Some(f) => {
            let Val::Data(Value::Record(rec)) = row else {
                return Err(alloc::format!("`{op} {f}` needs rows with fields, got {}", row.type_name()));
            };
            let idx = rec
                .schema
                .fields
                .iter()
                .position(|d| d.name == f)
                .ok_or_else(|| alloc::format!("no field `{f}` — this row has [{}]", field_names(&rec.schema)))?;
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

/// `format(template, args…)` — §8d, positional and indexed only. Not named capture: that would
/// resolve a string against the caller's scope at run time, unlike every other name in the
/// language.
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
                    spec.parse::<usize>().map_err(|_| alloc::format!("`{{{spec}}}` is not a positional index"))?
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
    // §8d: arity mismatch is an error; an unused argument is as much a mistake as a missing one.
    if next_positional < args.len() && !template.contains('{') {
        return Err(alloc::format!(
            "format was given {} argument(s) the template does not use",
            render_i64(args.len() as i64)
        ));
    }
    Ok(out)
}

/// Render a value for `display`: a table as aligned columns, anything else as itself. A
/// text-fallback stream prints as text, with no header; see [`text_fallback_lines`].
///
/// `styled` colours the header row and nothing else: the shell knows which row is a header, and a
/// value is data.
pub fn display(v: &Val, styled: bool) -> String {
    let Val::Data(Value::Table(t)) = v else {
        let mut s = v.render();
        s.push('\n');
        return s;
    };
    if let Some(text) = text_fallback_lines(t) {
        return text;
    }
    let headers: Vec<String> = t.schema.fields.iter().map(|f| f.name.clone()).collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    let cells: Vec<Vec<String>> = t
        .rows
        .iter()
        .map(|row| {
            row.iter()
                // A null cell is drawn blank; a null inside a list is still `[null]`.
                .map(|c| if *c == Value::Null { String::new() } else { Val::Data(c.clone()).render() })
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
    let mut head = String::new();
    for (i, h) in headers.iter().enumerate() {
        pad_into(&mut head, h, widths[i], i + 1 == headers.len());
    }
    // The row's newline comes after the reset: `tty_write_crlf` makes a chunk of anything
    // following a `\n`, so a reset after it would become a line of its own.
    let mut out = crate::style::paint(styled, crate::style::HEADER, &head);
    out.push('\n');
    for row in &cells {
        // A row ends at its last non-empty cell, so trailing blanks leave no padding to wrap a
        // narrow terminal; earlier blanks are padded to keep the columns aligned.
        let end = row.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1);
        for (i, c) in row.iter().enumerate().take(end) {
            if i < widths.len() {
                pad_into(&mut out, c, widths[i], i + 1 == end);
            }
        }
        out.push('\n');
    }
    out
}

/// A text-fallback stream rendered as the text it is: one row per line, no column header, so
/// plain text wrapped as `Table<{line: String}>` still looks like text at the end of a pipeline.
///
/// `None` for anything else, including a stream that claims the flag but is not one `String`
/// column: rendering the first column of a wider table as "the text" would hide the rest.
fn text_fallback_lines(t: &Table) -> Option<String> {
    if !t.flags.contains(libstream::StreamFlags::TEXT_FALLBACK) {
        return None;
    }
    if t.schema.fields.len() != 1 || t.schema.fields[0].ty != libstream::TypeTag::String {
        return None;
    }
    let mut out = String::new();
    for row in &t.rows {
        if let Some(Value::Str(line)) = row.first() {
            out.push_str(line);
        }
        out.push('\n');
    }
    Some(out)
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

/// Serialise for `save`. `.tsm` is the native stream; `.txt` is one rendered row per line.
/// `.csv`/`.json` are separate work (§4).
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

/// Parse for `open` (§4): text is wrapped into a single-column `Table<String>`.
///
/// Text ends at its last character that is not a NUL ([`libfs::unpadded`]): a snapshot such as
/// `/dev/log` is read whole, its last page padded with NULs, which would be a last line of them.
pub fn decode_from(path: &str, bytes: &[u8]) -> OpResult<Val> {
    if path.ends_with(".tsm") {
        let t = Table::decode(bytes).map_err(|_| alloc::format!("`{path}` is not a TSM1 stream"))?;
        return Ok(Val::Data(Value::Table(Arc::new(t))));
    }
    let text =
        core::str::from_utf8(libfs::unpadded(bytes)).map_err(|_| alloc::format!("`{path}` is not valid UTF-8"))?;
    let schema = Schema::new().field("line", TypeTag::String, TypeModifiers::NONE);
    let rows: Vec<Vec<Value>> = text.lines().map(|l| alloc::vec![Value::Str(l.to_string())]).collect();
    Ok(Val::Data(Value::Table(Arc::new(Table { flags: StreamFlags::NONE, schema, rows }))))
}

/// Concatenate two streams for `open a b` — §4's multi-path form.
pub fn concat(a: Val, b: Val) -> OpResult<Val> {
    let (Val::Data(Value::Table(ta)), Val::Data(Value::Table(tb))) = (&a, &b) else {
        return Err(String::from("`open` concatenates streams"));
    };
    if ta.schema != tb.schema {
        return Err(String::from("`open` cannot concatenate files with different shapes — their schemas differ"));
    }
    let mut rows = ta.rows.clone();
    rows.extend(tb.rows.iter().cloned());
    Ok(Val::Data(Value::Table(Arc::new(Table { flags: ta.flags, schema: ta.schema.clone(), rows }))))
}

#[cfg(test)]
mod tests {
    use super::*;

    use libstream::wire::Table;
    use libstream::{Schema, StreamFlags, TypeModifiers, TypeTag};

    fn text_stream(flags: StreamFlags, lines: &[&str]) -> Val {
        let schema = Schema::new().field("line", TypeTag::String, TypeModifiers::NONE);
        Val::Data(Value::Table(alloc::sync::Arc::new(Table {
            flags,
            schema,
            rows: lines.iter().map(|l| vec![Value::Str(String::from(*l))]).collect(),
        })))
    }

    #[test]
    fn a_text_fallback_stream_displays_as_text_and_a_plain_table_keeps_its_header() {
        // `clip` is the first producer of the flag.
        assert_eq!(display(&text_stream(StreamFlags::TEXT_FALLBACK, &["one", "two"]), false), "one\ntwo\n");
        // The control: without the flag the same rows are a table, and have columns.
        assert_eq!(display(&text_stream(StreamFlags::NONE, &["one", "two"]), false), "line\none\ntwo\n");
    }

    #[test]
    fn a_flag_on_the_wrong_shape_is_not_believed() {
        // A producer setting the bit on a table that is not one `String` column is wrong: the
        // header comes back.
        let schema = Schema::new().field("line", TypeTag::String, TypeModifiers::NONE).field(
            "n",
            TypeTag::Int,
            TypeModifiers::NONE,
        );
        let v = Val::Data(Value::Table(alloc::sync::Arc::new(Table {
            flags: StreamFlags::TEXT_FALLBACK,
            schema,
            rows: vec![vec![Value::Str(String::from("one")), Value::Int(1)]],
        })));
        assert!(display(&v, false).starts_with("line"));
    }

    #[test]
    fn an_empty_text_fallback_stream_displays_as_nothing() {
        // An empty `clip` ring must not print a bare `line`, a column name.
        assert_eq!(display(&text_stream(StreamFlags::TEXT_FALLBACK, &[]), false), "");
    }
    use alloc::vec;

    fn table(names: &[&str], sizes: &[i64]) -> Val {
        let schema = Schema::new().field("name", TypeTag::String, TypeModifiers::NONE).field(
            "size",
            TypeTag::Int,
            TypeModifiers::NONE,
        );
        let rows = names.iter().zip(sizes).map(|(n, s)| vec![Value::Str(n.to_string()), Value::Int(*s)]).collect();
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

    #[test]
    fn format_is_positional_and_indexed_only() {
        assert_eq!(format("hello {}", &[Val::str("world")]).unwrap(), "hello world");
        assert_eq!(format("{1}, you are {0}", &[Val::int(30), Val::str("bob")]).unwrap(), "bob, you are 30");
        // An index may repeat — the reason indexed form exists at all.
        assert_eq!(format("{0} met {0}", &[Val::str("ann")]).unwrap(), "ann met ann");
    }

    #[test]
    fn format_escapes_braces_and_fails_loud_on_arity() {
        assert_eq!(format("{{}}", &[]).unwrap(), "{}");
        assert!(format("{}", &[]).is_err());
        assert!(format("{5}", &[Val::int(1)]).is_err());
        assert!(format("{", &[]).is_err());
    }

    #[test]
    fn tsm_round_trips_through_save_and_open() {
        let t = table(&["a", "b"], &[1, 2]);
        let bytes = encode_for("x.tsm", &t).unwrap();
        let back = decode_from("x.tsm", &bytes).unwrap();
        assert_eq!(back.render(), t.render());
        assert_eq!(rows(&back).unwrap()[1].render(), "{ name: \"b\", size: 2 }");
    }

    /// §4: text becomes a single-column `Table<String>`.
    #[test]
    fn text_is_wrapped_into_a_single_column_table() {
        let v = decode_from("notes.txt", b"one\ntwo\n").unwrap();
        assert_eq!(v.render(), "<table 2 rows [line]>");
        assert_eq!(rows(&v).unwrap()[0].render(), "{ line: \"one\" }");
    }

    /// A snapshot's NUL padding is no line; one inside the text is still the text's.
    #[test]
    fn text_ends_before_the_nuls_that_pad_it() {
        let v = decode_from("/dev/log", b"one\ntwo\n\0\0\0").unwrap();
        assert_eq!(v.render(), "<table 2 rows [line]>");
        let v = decode_from("/dev/log", b"one\0two\n\0").unwrap();
        assert_eq!(rows(&v).unwrap()[0].render(), "{ line: \"one\0two\" }");
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

    #[test]
    fn display_lays_a_table_out_in_columns() {
        let t = table(&["short", "a-longer-name"], &[1, 200]);
        let out = display(&t, false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "name           size");
        assert_eq!(lines[1], "short          1");
        assert_eq!(lines[2], "a-longer-name  200");
    }

    #[test]
    fn display_of_a_scalar_is_just_the_value() {
        assert_eq!(display(&Val::int(5), false), "5\n");
    }

    /// A null cell is blank and keeps its column's width; a bare `null` still says so, and a null
    /// nested in a cell's list is still part of that list.
    #[test]
    fn a_null_cell_is_drawn_blank_and_a_bare_null_is_not() {
        let schema = Schema::new()
            .field("name", TypeTag::String, TypeModifiers::NONE)
            .field("mounted", TypeTag::String, TypeModifiers::NULLABLE)
            .field("by", TypeTag::String, TypeModifiers::NULLABLE);
        let rows = vec![
            vec![Value::Str("blk-0".into()), Value::Null, Value::Null],
            vec![Value::Str("blk-2".into()), Value::Str("/".into()), Value::Str("init".into())],
        ];
        let t = Val::Data(Value::Table(Arc::new(Table { flags: StreamFlags::NONE, schema, rows })));
        let out = display(&t, false);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "name   mounted  by");
        assert_eq!(lines[1], "blk-0", "two blank cells, and no trailing padding after the last");
        assert_eq!(lines[2], "blk-2  /        init");
        assert!(!out.contains("null"), "{out:?}");

        assert_eq!(display(&Val::Data(Value::Null), false), "null\n", "a bare null is the answer null");
        let schema = Schema::new().field("xs", TypeTag::List, TypeModifiers::NONE);
        let rows = vec![vec![Value::List(Arc::from(vec![Value::Null]))]];
        let nested = Val::Data(Value::Table(Arc::new(Table { flags: StreamFlags::NONE, schema, rows })));
        assert!(display(&nested, false).contains("[null]"), "a null inside a cell's list stays");
    }

    /// A table's header row is coloured and its values are not: a version wrapping the whole
    /// string would pass an assertion about the header alone.
    #[test]
    fn a_terminal_gets_a_coloured_header_and_plain_values() {
        let t = table(&["short"], &[1]);
        let plain = display(&t, false);
        let styled = display(&t, true);

        // Nothing at all when there is no terminal.
        assert!(!plain.contains('\x1b'), "{plain:?} carries an escape with no terminal");

        // The escape opens before the first header and the reset closes before the first value row.
        assert!(styled.starts_with(crate::style::HEADER), "{styled:?}");
        let (head, body) = styled.split_once(crate::style::RESET).expect("the header is reset");
        assert!(head.contains("name") && head.contains("size"), "{head:?}");
        assert!(body.contains("short") && body.contains('1'), "{body:?}");
        assert!(!body.contains('\x1b'), "{body:?}: a value was coloured");

        // The reset closes the row before its newline, asserted on the bytes: a strip-and-compare
        // cannot tell the two orders apart, and a reset after a `\n` that `drain_diagnostics`
        // strips would print a blank line led by `\x1b[0m`.
        assert!(
            styled.contains(&alloc::format!("{}\n", crate::style::RESET)),
            "{styled:?}: the header's reset does not close before its newline"
        );
        assert!(
            !styled.contains(&alloc::format!("\n{}", crate::style::RESET)),
            "{styled:?}: the reset is on the far side of a newline"
        );

        // The text is the same text; this half does not see the order above.
        let stripped: String = {
            let mut out = String::new();
            let mut rest = styled.as_str();
            while let Some(i) = rest.find('\x1b') {
                out.push_str(&rest[..i]);
                let j = rest[i..].find('m').expect("a complete SGR") + i + 1;
                rest = &rest[j..];
            }
            out.push_str(rest);
            out
        };
        assert_eq!(stripped, plain, "colouring changed the layout");
    }
}
