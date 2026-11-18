//! A value as the shell shows it: a table as aligned columns under a header, anything else as
//! itself. A pager lays a table out with [`table`], so it matches the shell's.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::wire::{Record, StreamFlags, Table, TypeTag, Value};

/// The SGR a header row is drawn in on a terminal, bright black: the shell's and a pager's.
pub const HEADER: &str = "\x1b[90m";

/// A table laid out as text: its header row, if it has one, and its rows, each a line without
/// its newline.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Laid {
    /// The column names, aligned; `None` for a text-fallback stream, which is shown as text.
    pub header: Option<String>,
    /// The rows, aligned under the header.
    pub rows: Vec<String>,
}

/// `t` laid out as the shell shows it: aligned columns under a header, or a text-fallback
/// stream's lines with no header.
pub fn table(t: &Table) -> Laid {
    if let Some(rows) = text_fallback_lines(t) {
        return Laid { header: None, rows };
    }
    let headers: Vec<String> = t.schema.fields.iter().map(|f| f.name.clone()).collect();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    let cells: Vec<Vec<String>> = t
        .rows
        .iter()
        .map(|row| {
            row.iter()
                // A null cell is drawn blank; a null inside a list is still `[null]`.
                .map(|c| if *c == Value::Null { String::new() } else { render_value(c, false) })
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
    let mut rows = Vec::with_capacity(cells.len());
    for row in &cells {
        // A row ends at its last non-empty cell, so trailing blanks leave no padding to wrap a
        // narrow terminal; earlier blanks are padded to keep the columns aligned.
        let end = row.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1);
        let mut line = String::new();
        for (i, c) in row.iter().enumerate().take(end) {
            if i < widths.len() {
                pad_into(&mut line, c, widths[i], i + 1 == end);
            }
        }
        rows.push(line);
    }
    Laid { header: Some(head), rows }
}

/// A text-fallback stream's lines, one row per line, no column header, so plain text wrapped as
/// `Table<{line: String}>` still looks like text at the end of a pipeline.
///
/// `None` for anything else, including a stream that claims the flag but is not one `String`
/// column: rendering the first column of a wider table as "the text" would hide the rest.
fn text_fallback_lines(t: &Table) -> Option<Vec<String>> {
    if !t.flags.contains(StreamFlags::TEXT_FALLBACK) {
        return None;
    }
    if t.schema.fields.len() != 1 || t.schema.fields[0].ty != TypeTag::String {
        return None;
    }
    Some(
        t.rows
            .iter()
            .map(|row| match row.first() {
                Some(Value::Str(line)) => line.clone(),
                _ => String::new(),
            })
            .collect(),
    )
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

/// `v`'s display form: what the shell shows for it, and what its `++` concatenates.
pub fn value(v: &Value) -> String {
    render_value(v, false)
}

/// Render a `Value`; `nested` quotes strings, so a list of strings looks like one.
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

/// `v` in decimal.
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

/// Render a float. A whole float keeps its point (`1.0`, not `1`), so it stays distinguishable
/// from `Int(1)`; `inf` and `NaN` are named, as in the type vocabulary.
pub fn render_f64(v: f64) -> String {
    if v.is_nan() {
        return String::from("NaN");
    }
    if v.is_infinite() {
        return String::from(if v > 0.0 { "inf" } else { "-inf" });
    }
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
    use crate::wire::{Schema, TypeModifiers};
    use alloc::sync::Arc;
    use alloc::vec;

    /// `t` as the shell prints it, a newline after every line.
    fn text(t: &Table) -> String {
        let laid = table(t);
        let mut out = String::new();
        for line in laid.header.iter().chain(laid.rows.iter()) {
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    fn text_stream(flags: StreamFlags, lines: &[&str]) -> Table {
        let schema = Schema::new().field("line", TypeTag::String, TypeModifiers::NONE);
        Table { flags, schema, rows: lines.iter().map(|l| vec![Value::Str(String::from(*l))]).collect() }
    }

    fn named_sizes(names: &[&str], sizes: &[i64]) -> Table {
        let schema = Schema::new().field("name", TypeTag::String, TypeModifiers::NONE).field(
            "size",
            TypeTag::Int,
            TypeModifiers::NONE,
        );
        let rows = names.iter().zip(sizes).map(|(n, s)| vec![Value::Str(n.to_string()), Value::Int(*s)]).collect();
        Table { flags: StreamFlags::NONE, schema, rows }
    }

    #[test]
    fn a_whole_float_is_distinguishable_from_an_int() {
        assert_eq!(value(&Value::Float(1.0)), "1.0");
        assert_eq!(value(&Value::Int(1)), "1");
        assert_eq!(value(&Value::Float(-2.0)), "-2.0");
    }

    #[test]
    fn floats_render_at_full_precision() {
        assert_eq!(value(&Value::Float(1.5)), "1.5");
        assert_eq!(value(&Value::Float(0.1)), "0.1");
        assert_eq!(value(&Value::Float(2.675)), "2.675");
        assert_eq!(value(&Value::Float(1e21)), "1000000000000000000000.0");
    }

    #[test]
    fn non_finite_floats_are_named() {
        assert_eq!(value(&Value::Float(f64::NAN)), "NaN");
        assert_eq!(value(&Value::Float(f64::INFINITY)), "inf");
        assert_eq!(value(&Value::Float(f64::NEG_INFINITY)), "-inf");
    }

    #[test]
    fn integers_render_including_the_extremes() {
        assert_eq!(value(&Value::Int(0)), "0");
        assert_eq!(value(&Value::Int(-1)), "-1");
        assert_eq!(value(&Value::Int(i64::MAX)), "9223372036854775807");
        assert_eq!(value(&Value::Int(i64::MIN)), "-9223372036854775808");
    }

    #[test]
    fn strings_quote_only_when_nested() {
        assert_eq!(value(&Value::Str(String::from("hi"))), "hi");
        let l = Value::List(Arc::from(vec![Value::Str(String::from("a")), Value::Int(1)]));
        assert_eq!(value(&l), "[\"a\", 1]");
    }

    #[test]
    fn a_text_fallback_stream_displays_as_text_and_a_plain_table_keeps_its_header() {
        // `clip` is the first producer of the flag.
        assert_eq!(text(&text_stream(StreamFlags::TEXT_FALLBACK, &["one", "two"])), "one\ntwo\n");
        // The control: without the flag the same rows are a table, and have columns.
        assert_eq!(text(&text_stream(StreamFlags::NONE, &["one", "two"])), "line\none\ntwo\n");
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
        let t = Table {
            flags: StreamFlags::TEXT_FALLBACK,
            schema,
            rows: vec![vec![Value::Str(String::from("one")), Value::Int(1)]],
        };
        assert!(text(&t).starts_with("line"));
    }

    #[test]
    fn an_empty_text_fallback_stream_displays_as_nothing() {
        // An empty `clip` ring must not print a bare `line`, a column name.
        assert_eq!(text(&text_stream(StreamFlags::TEXT_FALLBACK, &[])), "");
        assert_eq!(table(&text_stream(StreamFlags::TEXT_FALLBACK, &[])), Laid { header: None, rows: vec![] });
    }

    #[test]
    fn a_table_with_no_rows_gives_its_header_and_no_rows() {
        let laid = table(&named_sizes(&[], &[]));
        assert_eq!(laid, Laid { header: Some(String::from("name  size")), rows: vec![] });
    }

    #[test]
    fn display_lays_a_table_out_in_columns() {
        let t = named_sizes(&["short", "a-longer-name"], &[1, 200]);
        let out = text(&t);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "name           size");
        assert_eq!(lines[1], "short          1");
        assert_eq!(lines[2], "a-longer-name  200");
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
        let t = Table { flags: StreamFlags::NONE, schema, rows };
        let out = text(&t);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "name   mounted  by");
        assert_eq!(lines[1], "blk-0", "two blank cells, and no trailing padding after the last");
        assert_eq!(lines[2], "blk-2  /        init");
        assert!(!out.contains("null"), "{out:?}");

        assert_eq!(value(&Value::Null), "null", "a bare null is the answer null");
        let schema = Schema::new().field("xs", TypeTag::List, TypeModifiers::NONE);
        let rows = vec![vec![Value::List(Arc::from(vec![Value::Null]))]];
        let nested = Table { flags: StreamFlags::NONE, schema, rows };
        assert!(text(&nested).contains("[null]"), "a null inside a cell's list stays");
    }
}
