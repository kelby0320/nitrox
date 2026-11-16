//! The syscall numbers the docs state in prose, beside the table `check-docs` already holds: each
//! must be the number `table.rs` gives. Under the dense rule a removal moves every later call, and
//! a number repeated in a sentence is the copy a renumbering misses.

use std::collections::BTreeMap;

/// Each `(Syscall number `N`.)` sentence, by line: the call whose ```` ```rust ```` block it
/// follows, if any, and the number it states.
pub(super) fn per_call(text: &str) -> Vec<(usize, Option<String>, i64)> {
    let mut last_fn = None;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if let Some(rest) = line.trim_start().strip_prefix("fn sys_") {
            let name: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            last_fn = Some(format!("sys_{name}"));
        }
        for (_, tail) in line.match_indices("(Syscall number `").map(|(at, m)| line.split_at(at + m.len())) {
            if let Some(n) = tail.split_once('`').and_then(|(n, _)| number(n)) {
                out.push((i + 1, last_fn.clone(), n));
            }
        }
    }
    out
}

/// Each number a line states for a call by name, by line: `sys_name = N`, `` `sys_name` = N`` or
/// `` `sys_name` (N)``. A call's argument list, `sys_name(a, b)`, states none.
pub(super) fn stated(text: &str) -> Vec<(usize, String, i64)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for (at, _) in line.match_indices("sys_") {
            if line[..at].ends_with(|c: char| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            let name: String = line[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            let rest = line[at + name.len()..].trim_start_matches('`').trim_start();
            let value = if let Some(v) = rest.strip_prefix('=') {
                v.trim_start().trim_start_matches('`')
            } else if let Some(v) = rest.strip_prefix('(') {
                v.trim_start_matches('`')
            } else {
                continue;
            };
            let digits: String =
                value.chars().take_while(|c| c.is_ascii_hexdigit() || *c == 'x' || *c == '_').collect();
            if let Some(n) = number(&digits) {
                out.push((i + 1, name, n));
            }
        }
    }
    out
}

/// `12`, or `0xFFFF_0000`.
fn number(t: &str) -> Option<i64> {
    match t.strip_prefix("0x") {
        Some(hex) => i64::from_str_radix(&hex.replace('_', ""), 16).ok(),
        None => t.parse().ok(),
    }
}

/// The findings in one doc, given the table's numbers by name: a stated number that is not the
/// call's, and a per-call sentence that follows no numbered call.
pub(super) fn check(doc: &str, text: &str, numbers: &BTreeMap<String, i64>) -> Vec<String> {
    let mut found = Vec::new();
    for (line, name, n) in stated(text) {
        match numbers.get(&name) {
            Some(&want) if want != n => {
                found.push(format!("{doc}:{line}: states `{name}` is {n}, but `table.rs` numbers it {want}"))
            }
            _ => {}
        }
    }
    for (line, call, n) in per_call(text) {
        match call.as_ref().map(|c| (c, numbers.get(c))) {
            Some((_, Some(&want))) if want == n => {}
            Some((c, Some(&want))) => {
                found.push(format!("{doc}:{line}: `{c}`'s sentence says number {n}, but `table.rs` numbers it {want}"))
            }
            Some((c, None)) => {
                found.push(format!("{doc}:{line}: number {n} follows `{c}`, which `table.rs` does not number"))
            }
            None => found.push(format!("{doc}:{line}: number {n} follows no `fn sys_` block")),
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> BTreeMap<String, i64> {
        [("sys_wait", 10), ("sys_io_submit", 27), ("sys_debug_kprint", 0xFFFF_0000)].map(|(n, v)| (n.into(), v)).into()
    }

    #[test]
    fn a_sentence_is_the_number_of_the_call_above_it() {
        let doc = "```rust\nfn sys_wait(h: u64) -> isize\n```\nWaits. (Syscall number `10`.)\n\n\
                   ```rust\nfn sys_io_submit(\n    h: u64,\n) -> isize\n```\nSubmits. (Syscall number `28`.)\n";
        assert_eq!(per_call(doc), [(4, Some("sys_wait".into()), 10), (11, Some("sys_io_submit".into()), 28)]);
        let found = check("d.md", doc, &table());
        assert_eq!(found.len(), 1, "{found:?}");
        assert!(found[0].starts_with("d.md:11:") && found[0].contains("27"), "{found:?}");
    }

    #[test]
    fn a_sentence_after_an_unnumbered_call_or_none_is_a_finding() {
        let doc = "(Syscall number `3`.)\n```rust\nfn sys_ring_create() -> isize\n```\n(Syscall number `40`.)\n";
        assert_eq!(check("d.md", doc, &table()).len(), 2);
    }

    #[test]
    fn each_prose_form_is_read_and_an_argument_list_is_not() {
        let doc = "`sys_io_submit = 28` (since slice 5), `sys_wait` = 10, `sys_wait` (`10`),\n\
                   `sys_debug_kprint` (`0xFFFF_0000`), sys_wait(h, 3), my_sys_wait = 4, `sys_gone` = 40\n";
        let got = stated(doc);
        assert_eq!(
            got,
            [
                (1, "sys_io_submit".into(), 28),
                (1, "sys_wait".into(), 10),
                (1, "sys_wait".into(), 10),
                (2, "sys_debug_kprint".into(), 0xFFFF_0000),
                (2, "sys_gone".into(), 40),
            ]
        );
        let found = check("d.md", doc, &table());
        assert_eq!(found.len(), 1, "only the stale number, and no name the table lacks: {found:?}");
        assert!(found[0].contains("`sys_io_submit` is 28"), "{found:?}");
    }
}
