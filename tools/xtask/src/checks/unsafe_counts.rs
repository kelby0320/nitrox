//! `check-unsafe`: each kind of `unsafe` in each kernel module only falls, against
//! `tools/xtask/unsafe-counts.toml`.
//!
//! The modules and the counts are `unsafe-inventory`'s: each module of `kernel/src` is `x.rs` and
//! `x/` together, test code left out. A rise fails, and so does an entry above its count.

use super::comment_budget::git;
use super::unsafe_inventory::{
    KIND_COUNT, STATIC_MUT, UNSAFE_ATTR, UNSAFE_BLOCK, UNSAFE_EXTERN, UNSAFE_FN, UNSAFE_IMPL, kernel_counts,
};
use crate::*;
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

const BASELINE: &str = "tools/xtask/unsafe-counts.toml";
const HEADER: &str = "# The `unsafe` of each kernel module, by kind, counted by `cargo xtask check-unsafe` as\n\
                      # `cargo xtask unsafe-inventory` counts it: a module is `x.rs` and `x/` together, test code\n\
                      # left out. A count only falls. `--update` lowers or removes entries; a raise is by hand, in the\n\
                      # commit that adds the `unsafe`, whose message says why (kernel/CLAUDE.md's `unsafe` policy).\n";

/// Each section of the table: its name, the inventory kind it holds, and what a count of it is.
const SECTIONS: [(&str, usize, &str); 6] = [
    ("unsafe-blocks", UNSAFE_BLOCK, "unsafe block(s)"),
    ("unsafe-fns", UNSAFE_FN, "unsafe fn(s)"),
    ("unsafe-impls", UNSAFE_IMPL, "unsafe impl(s)"),
    ("unsafe-attrs", UNSAFE_ATTR, "#[unsafe(..)] attribute(s)"),
    ("unsafe-externs", UNSAFE_EXTERN, "unsafe extern block(s)"),
    ("static-muts", STATIC_MUT, "static mut(s)"),
];

/// Each kernel module's count of every inventory kind.
type Counts = BTreeMap<String, [usize; KIND_COUNT]>;

/// The checked-in baseline: for each of [`SECTIONS`], a count per module.
#[derive(Default, Debug, PartialEq, Clone)]
struct Baseline {
    sections: [BTreeMap<String, usize>; SECTIONS.len()],
}

fn parse_baseline(text: &str) -> R<Baseline> {
    let mut out = Baseline::default();
    let mut section = None;
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']'))
            && let Some(i) = SECTIONS.iter().position(|(s, _, _)| *s == name)
        {
            section = Some(i);
            continue;
        }
        let bad = || format!("{BASELINE}: not `\"path\" = count`: {line}");
        let (path, value) = t.strip_prefix('"').and_then(|r| r.split_once("\" = ")).ok_or_else(bad)?;
        let count = value.parse().map_err(|_| bad())?;
        let i = section.ok_or_else(|| format!("{BASELINE}: an entry before any section: {line}"))?;
        out.sections[i].insert(path.to_string(), count);
    }
    Ok(out)
}

fn format_baseline(b: &Baseline) -> String {
    let mut out = HEADER.to_string();
    for ((name, _, _), entries) in SECTIONS.iter().zip(&b.sections) {
        out.push_str(&format!("\n[{name}]\n"));
        for (path, n) in entries {
            out.push_str(&format!("\"{path}\" = {n}\n"));
        }
    }
    out
}

/// One kind's count in each module.
fn of_kind(actual: &Counts, kind: usize) -> BTreeMap<String, usize> {
    actual.iter().map(|(g, c)| (g.clone(), c[kind])).collect()
}

/// The measured counts as a baseline: a module counting zero in a kind has no entry in it.
fn measured(actual: &Counts) -> Baseline {
    let mut out = Baseline::default();
    for (i, (_, kind, _)) in SECTIONS.iter().enumerate() {
        out.sections[i] = of_kind(actual, *kind).into_iter().filter(|(_, n)| *n > 0).collect();
    }
    out
}

/// What is wrong with one section: `counts` against its `entries`, `what` naming the kind.
fn section_failures(what: &str, counts: &BTreeMap<String, usize>, entries: &BTreeMap<String, usize>) -> Vec<String> {
    let mut out = Vec::new();
    for (group, &n) in counts.iter().filter(|(_, n)| **n > 0) {
        match entries.get(group) {
            None => out.push(format!(
                "{group}: {n} {what}, and no entry: new `unsafe` in a module that had none; \
                 a raise is by hand, in the commit that adds it and says why"
            )),
            Some(&e) if n > e => out.push(format!(
                "{group}: {n} {what}, over its entry of {e}: it may not rise; \
                 a raise is by hand, in the commit that adds the `unsafe` and says why"
            )),
            Some(&e) if n < e => out.push(format!(
                "{group}: {n} {what}, under its entry of {e}: stale, run `cargo xtask check-unsafe --update`"
            )),
            _ => {}
        }
    }
    for (group, e) in entries {
        match counts.get(group) {
            None => out.push(format!(
                "{group}: an entry of {e} {what}, but no such module: stale, run `cargo xtask check-unsafe --update`"
            )),
            Some(0) => out.push(format!(
                "{group}: an entry of {e} {what}, but it counts zero: stale, run `cargo xtask check-unsafe --update`"
            )),
            Some(_) => {}
        }
    }
    out
}

fn failures(actual: &Counts, baseline: &Baseline) -> Vec<String> {
    let mut out = Vec::new();
    for ((_, kind, what), entries) in SECTIONS.iter().zip(&baseline.sections) {
        out.extend(section_failures(what, &of_kind(actual, *kind), entries));
    }
    out
}

/// `baseline` with each count lowered to the one measured and each entry that counts zero or is
/// gone removed; never raised.
fn updated(actual: &Counts, baseline: &Baseline) -> Baseline {
    let mut out = Baseline::default();
    for (i, (_, kind, _)) in SECTIONS.iter().enumerate() {
        out.sections[i] = baseline.sections[i]
            .iter()
            .filter_map(|(g, &e)| actual.get(g).map(|c| c[*kind]).filter(|&n| n > 0).map(|n| (g.clone(), e.min(n))))
            .collect();
    }
    out
}

/// What a run does: the table it writes, if any; the lines it prints; and its failures.
#[derive(Debug, Default)]
struct Run {
    write: Option<String>,
    printed: Vec<String>,
    failures: Vec<String>,
}

/// `check-unsafe [--update | --list]` over `actual`, given the table's text; `--list` needs none.
fn run(args: &[String], actual: &Counts, table: Option<&str>) -> R<Run> {
    let mut out = Run::default();
    if args.iter().any(|a| a == "--list") {
        out.printed.push(format_baseline(&measured(actual)));
        return Ok(out);
    }
    let mut baseline = parse_baseline(table.ok_or_else(|| format!("check-unsafe: no {BASELINE}"))?)?;
    if args.iter().any(|a| a == "--update") {
        let new = updated(actual, &baseline);
        for ((name, _, _), (old, now)) in SECTIONS.iter().zip(baseline.sections.iter().zip(&new.sections)) {
            for (group, o) in old {
                match now.get(group) {
                    Some(n) if n == o => {}
                    Some(n) => out.printed.push(format!("check-unsafe: {group}: {name} {o} -> {n}")),
                    None => out.printed.push(format!("check-unsafe: {group}: {name} removed")),
                }
            }
        }
        out.write = Some(format_baseline(&new));
        baseline = new;
    }
    out.failures = failures(actual, &baseline);
    if out.failures.is_empty() {
        let totals: Vec<String> = SECTIONS
            .iter()
            .map(|(name, kind, _)| format!("{} {name}", actual.values().map(|c| c[*kind]).sum::<usize>()))
            .collect();
        out.printed.push(format!(
            "check-unsafe: {} in {} modules, none over its entry ✓",
            totals.join(", "),
            actual.len()
        ));
    }
    Ok(out)
}

/// The counts of every kernel module, read from the tracked files.
fn measure() -> R<Counts> {
    let root = repo_root();
    let mut files = Vec::new();
    for path in git(&["ls-files", "--", "kernel/src"])?.lines().filter(|p| p.ends_with(".rs")) {
        let text = fs::read_to_string(root.join(path)).map_err(|e| format!("check-unsafe: {path}: {e}"))?;
        files.push((path.to_string(), text));
    }
    kernel_counts(&files)
}

/// `check-unsafe [--update | --list]`: `--list` prints every current count in the baseline's format
/// and changes nothing.
pub(crate) fn cmd_check_unsafe(args: &[String]) -> R<()> {
    let file = repo_root().join(BASELINE);
    let actual = measure()?;
    let table = if file.exists() { Some(crate::read_text(&file)?) } else { None };
    let done = run(args, &actual, table.as_deref())?;
    if let Some(text) = &done.write {
        fs::write(&file, text)?;
    }
    for line in &done.printed {
        println!("{}", line.trim_end());
    }
    if !done.failures.is_empty() {
        for f in &done.failures {
            eprintln!("check-unsafe: {f}");
        }
        return Err(format!("check-unsafe: {} failure(s)", done.failures.len()).into());
    }
    Ok(())
}
