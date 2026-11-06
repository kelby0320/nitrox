//! `check-unsafe`: the `unsafe` blocks and `unsafe fn`s of each kernel module only fall, against
//! `tools/xtask/unsafe-counts.toml`.
//!
//! The modules and the counts are `unsafe-inventory`'s: each module of `kernel/src` is `x.rs` and
//! `x/` together, test code left out. A rise fails, and so does an entry above its count.

use super::comment_budget::git;
use super::unsafe_inventory::kernel_unsafe;
use crate::*;
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

const BASELINE: &str = "tools/xtask/unsafe-counts.toml";
const HEADER: &str = "# The `unsafe` blocks and `unsafe fn`s of each kernel module, counted by `cargo xtask check-unsafe`\n\
                      # as `cargo xtask unsafe-inventory` counts them: a module is `x.rs` and `x/` together, test code\n\
                      # left out. A count only falls. `--update` lowers or removes entries; a raise is by hand, in the\n\
                      # commit that adds the `unsafe`, whose message says why (kernel/CLAUDE.md's `unsafe` policy).\n";
const SECTIONS: [&str; 2] = ["unsafe-blocks", "unsafe-fns"];

/// The checked-in baseline: a count per module, for each of the two kinds.
#[derive(Default, Debug, PartialEq, Clone)]
struct Baseline {
    blocks: BTreeMap<String, usize>,
    fns: BTreeMap<String, usize>,
}

fn parse_baseline(text: &str) -> R<Baseline> {
    let mut out = Baseline::default();
    let mut section = "";
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(name) = t.strip_prefix('[').and_then(|r| r.strip_suffix(']')).filter(|n| SECTIONS.contains(n)) {
            section = name;
            continue;
        }
        let bad = || format!("{BASELINE}: not `\"path\" = count`: {line}");
        let (path, value) = t.strip_prefix('"').and_then(|r| r.split_once("\" = ")).ok_or_else(bad)?;
        let count = value.parse().map_err(|_| bad())?;
        match section {
            "unsafe-blocks" => out.blocks.insert(path.to_string(), count),
            "unsafe-fns" => out.fns.insert(path.to_string(), count),
            _ => {
                return Err(format!("{BASELINE}: an entry outside `[unsafe-blocks]` and `[unsafe-fns]`: {line}").into());
            }
        };
    }
    Ok(out)
}

fn format_baseline(b: &Baseline) -> String {
    let mut out = HEADER.to_string();
    for (name, counts) in SECTIONS.iter().zip([&b.blocks, &b.fns]) {
        out.push_str(&format!("\n[{name}]\n"));
        for (path, n) in counts {
            out.push_str(&format!("\"{path}\" = {n}\n"));
        }
    }
    out
}

/// The measured counts as a baseline: a module counting zero in a kind has no entry in it.
fn measured(actual: &BTreeMap<String, (usize, usize)>) -> Baseline {
    let pick = |f: fn(&(usize, usize)) -> usize| {
        actual.iter().map(|(g, c)| (g.clone(), f(c))).filter(|(_, n)| *n > 0).collect::<BTreeMap<_, _>>()
    };
    Baseline { blocks: pick(|c| c.0), fns: pick(|c| c.1) }
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

fn failures(actual: &BTreeMap<String, (usize, usize)>, baseline: &Baseline) -> Vec<String> {
    let kind = |f: fn(&(usize, usize)) -> usize| -> BTreeMap<String, usize> {
        actual.iter().map(|(g, c)| (g.clone(), f(c))).collect()
    };
    let mut out = section_failures("unsafe block(s)", &kind(|c| c.0), &baseline.blocks);
    out.extend(section_failures("unsafe fn(s)", &kind(|c| c.1), &baseline.fns));
    out
}

/// `baseline` with each count lowered to the one measured and each entry that counts zero or is
/// gone removed; never raised.
fn updated(actual: &BTreeMap<String, (usize, usize)>, baseline: &Baseline) -> Baseline {
    let lower = |entries: &BTreeMap<String, usize>, f: fn(&(usize, usize)) -> usize| {
        entries
            .iter()
            .filter_map(|(g, &e)| actual.get(g).map(f).filter(|&n| n > 0).map(|n| (g.clone(), e.min(n))))
            .collect()
    };
    Baseline { blocks: lower(&baseline.blocks, |c| c.0), fns: lower(&baseline.fns, |c| c.1) }
}

/// Print each entry `new` changes from `old`.
fn print_changes(what: &str, old: &BTreeMap<String, usize>, new: &BTreeMap<String, usize>) {
    for (group, o) in old {
        match new.get(group) {
            Some(n) if n == o => {}
            Some(n) => println!("check-unsafe: {group}: {what} {o} -> {n}"),
            None => println!("check-unsafe: {group}: {what} removed"),
        }
    }
}

/// The counts of every kernel module, read from the tracked files.
fn measure() -> R<BTreeMap<String, (usize, usize)>> {
    let root = repo_root();
    let mut files = Vec::new();
    for path in git(&["ls-files", "--", "kernel/src"])?.lines().filter(|p| p.ends_with(".rs")) {
        let text = fs::read_to_string(root.join(path)).map_err(|e| format!("check-unsafe: {path}: {e}"))?;
        files.push((path.to_string(), text));
    }
    kernel_unsafe(&files)
}

/// `check-unsafe [--update | --list]`: `--list` prints every current count in the baseline's format
/// and changes nothing.
pub(crate) fn cmd_check_unsafe(args: &[String]) -> R<()> {
    let file = repo_root().join(BASELINE);
    let actual = measure()?;
    if args.iter().any(|a| a == "--list") {
        print!("{}", format_baseline(&measured(&actual)));
        return Ok(());
    }
    let mut baseline = parse_baseline(&crate::read_text(&file)?)?;
    if args.iter().any(|a| a == "--update") {
        let new = updated(&actual, &baseline);
        print_changes("unsafe blocks", &baseline.blocks, &new.blocks);
        print_changes("unsafe fns", &baseline.fns, &new.fns);
        fs::write(&file, format_baseline(&new))?;
        baseline = new;
    }
    let failed = failures(&actual, &baseline);
    if !failed.is_empty() {
        for f in &failed {
            eprintln!("check-unsafe: {f}");
        }
        return Err(format!("check-unsafe: {} failure(s)", failed.len()).into());
    }
    let (blocks, fns): (usize, usize) = actual.values().fold((0, 0), |(b, f), c| (b + c.0, f + c.1));
    println!(
        "check-unsafe: {blocks} unsafe blocks and {fns} unsafe fns in {} modules, none over its entry ✓",
        actual.len()
    );
    Ok(())
}
