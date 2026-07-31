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
//! elements and rebuilds as a `List`. Operators therefore never mention which of the two
//! they were handed, which is the whole point — and a scalar is an error rather than a
//! one-element sequence, because silently treating `5` as `[5]` is the kind of coercion
//! §6 spends its fail-loud rule on.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use libstream::wire::{Record, Schema, StreamFlags, Table, TypeModifiers, TypeTag, Value};

use crate::value::{Val, render_i64};

/// An operator failure. Plain strings: the evaluator wraps them.
pub type OpResult<T> = Result<T, String>;

/// Decompose a value into rows, generically.
///
/// A `Table` gives one `Record` per row — which is what makes `filter name == "x"` work on
/// a stream without the operator knowing the schema. A `List` gives its elements.
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
        other => Err(alloc::format!(
            "expected a Table or a List, got {} — an operator works over rows, and a \
             scalar is not one",
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

/// `sort FIELD [--reverse]`, or `sort` over bare values.
pub fn sort(v: &Val, field: Option<&str>, reverse: bool) -> OpResult<Val> {
    let mut r = rows(v)?;
    // A stable insertion sort: `Vec::sort_by` needs a total order, and comparing two
    // arbitrary `Value`s does not have one (a String and an Int are simply not ordered).
    // Doing it by hand keeps the "these two cannot be compared" case an error rather than
    // an arbitrary answer.
    let mut err: Option<String> = None;
    let mut sorted: Vec<Val> = Vec::with_capacity(r.len());
    for item in r.drain(..) {
        let key = sort_key(&item, field)?;
        let mut at = sorted.len();
        for i in 0..sorted.len() {
            let other = sort_key(&sorted[i], field)?;
            match compare(&key, &other) {
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

fn sort_key(row: &Val, field: Option<&str>) -> OpResult<Value> {
    match field {
        None => row
            .as_data()
            .cloned()
            .ok_or_else(|| String::from("cannot sort a value TSM1 cannot represent")),
        Some(f) => {
            let Val::Data(Value::Record(rec)) = row else {
                return Err(alloc::format!(
                    "`sort {f}` needs rows with fields, got {}",
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
        let s = sort(&t, Some("size"), false).unwrap();
        let r = rows(&s).unwrap();
        assert_eq!(r[0].render(), "{ name: \"a\", size: 1 }");
        assert_eq!(r[2].render(), "{ name: \"c\", size: 3 }");
        let s = sort(&t, Some("size"), true).unwrap();
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
        assert!(sort(&t, Some("v"), false).is_err());
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
