//! `unsafe-inventory`: what each crate or kernel module holds of `unsafe` and raw pointers, by kind,
//! counted the same way every run so two surveys of one tree agree. Test code is left out.

use super::comment_budget::git;
use crate::*;

/// Each kind, and the text that marks one in a line of code with its comments removed and its
/// literals blanked. A raw deref counts only inside an `unsafe` block.
const KINDS: &[(&str, &[&str])] = &[
    ("unsafe block", &["unsafe {"]),
    ("unsafe fn", &["unsafe fn "]),
    ("unsafe impl", &["unsafe impl"]),
    ("static mut", &["static mut "]),
    ("raw syscall", &["syscall0(", "syscall1(", "syscall2(", "syscall3(", "syscall4(", "syscall5(", "syscall6("]),
    ("raw slice", &["from_raw_parts"]),
    ("raw deref", &["&*", "&mut *", "(*"]),
    ("transmute", &["transmute"]),
    ("asm", &["asm!", "global_asm!", "naked_asm!"]),
    ("volatile", &["read_volatile", "write_volatile"]),
];
const RAW_DEREF: usize = 6;

/// The counts for one path: lines of code, then one per [`KINDS`] entry.
#[derive(Default, Debug, PartialEq)]
struct Counts {
    code: usize,
    kinds: [usize; KINDS.len()],
}

impl Counts {
    fn add(&mut self, other: &Counts) {
        self.code += other.code;
        for (t, n) in self.kinds.iter_mut().zip(other.kinds) {
            *t += n;
        }
    }
}

/// Whether `path` holds only test code by its name: a `tests.rs`, a `*_tests.rs`, a
/// `test_support.rs`, or anything under a `tests/` directory.
fn is_test_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "tests.rs" || name.ends_with("_tests.rs") || name == "test_support.rs" || path.contains("/tests/")
}

/// `text` with its `//` comments removed and each string and character literal's contents
/// replaced by `x`, line for line, so a brace or a marker inside a literal is not code.
fn blanked(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let blank = |out: &mut String, cs: &[char]| out.extend(cs.iter().map(|&c| if c == '\n' { '\n' } else { 'x' }));
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let ident_before = i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_');
        let raw = (!ident_before && (c == 'r' || (c == 'b' && chars.get(i + 1) == Some(&'r'))))
            .then(|| {
                let start = if c == 'b' { i + 2 } else { i + 1 };
                let hashes = chars[start..].iter().take_while(|&&h| h == '#').count();
                (chars.get(start + hashes) == Some(&'"')).then_some((start + hashes + 1, hashes))
            })
            .flatten();
        if let Some((open, hashes)) = raw {
            let close: String = std::iter::once('"').chain(std::iter::repeat_n('#', hashes)).collect();
            let rest: String = chars[open..].iter().collect();
            let body_end = rest.find(&close).map_or(chars.len(), |e| open + rest[..e].chars().count());
            out.extend(&chars[i..open]);
            blank(&mut out, &chars[open..body_end]);
            out.extend(&chars[body_end..(body_end + close.len()).min(chars.len())]);
            i = body_end + close.len();
        } else if c == '"' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '"' {
                j += if chars[j] == '\\' { 2 } else { 1 };
            }
            let j = j.min(chars.len());
            out.push('"');
            blank(&mut out, &chars[i + 1..j]);
            out.push('"');
            i = j + 1;
        } else if c == '\'' && chars.get(i + 1) == Some(&'\\') {
            let end = chars[i + 2..].iter().position(|&q| q == '\'').map_or(chars.len(), |p| i + 2 + p);
            out.push_str("'x'");
            i = end + 1;
        } else if c == '\'' && chars.get(i + 2) == Some(&'\'') {
            out.push_str("'x'");
            i += 3;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else {
            out.push(c);
            i += 1;
        }
    }
    out.lines().map(|l| l.trim_end().to_string()).collect()
}

/// `lines` with every `#[cfg(test)]` item taken out: the attribute's next item, to the `}` that
/// closes it or, with no brace, to its `;` or `,`. An item still open at the end is an error.
fn non_test_lines(lines: Vec<String>) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "#[cfg(test)]" {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }
        let from = i + 1;
        i += 1;
        while i < lines.len() && lines[i].trim_start().starts_with("#[") {
            i += 1;
        }
        let (mut depth, mut opened, mut ended) = (0i64, false, false);
        while i < lines.len() && !ended {
            for c in lines[i].chars() {
                match c {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            let line = lines[i].trim_end();
            ended = if opened { depth <= 0 } else { line.ends_with(';') || line.ends_with(',') };
            i += 1;
        }
        if !ended {
            return Err(format!("the #[cfg(test)] item at line {from} never ends"));
        }
    }
    Ok(out.into_iter().filter(|l| !l.trim().is_empty()).collect())
}

/// How often `marker` occurs in `line` where no identifier runs into it from the left, so
/// `my_syscall1(` is not a raw syscall and `transmute_copy` still is a transmute. `(*` counts only
/// as a place, `(*p).field`.
fn occurrences(line: &str, marker: &str) -> usize {
    let mut n = 0;
    let mut from = 0;
    while let Some(at) = line[from..].find(marker) {
        let start = from + at;
        let before = line[..start].chars().next_back();
        let word_start = marker.starts_with(|c: char| c.is_alphanumeric() || c == '_');
        let place = marker != "(*"
            || line[start + 2..].split_once(')').is_some_and(|(p, rest)| !p.contains('(') && rest.starts_with('.'));
        if place && !(word_start && before.is_some_and(|c| c.is_alphanumeric() || c == '_')) {
            n += 1;
        }
        from = start + marker.len();
    }
    n
}

/// `line` with each `unsafe extern "<abi>" fn` read as `unsafe fn`.
fn unextern(line: &str) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(at) = rest.find("unsafe extern \"") {
        let after = &rest[at + "unsafe extern \"".len()..];
        match after.split_once('"').filter(|(_, tail)| tail.starts_with(" fn ")) {
            Some((_, tail)) => {
                out.push_str(&rest[..at]);
                out.push_str("unsafe");
                rest = tail;
            }
            None => {
                out.push_str(&rest[..at + 1]);
                rest = &rest[at + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn count(text: &str) -> Result<Counts, String> {
    let mut c = Counts::default();
    // Brace depth, and the depth each enclosing `unsafe` block opened at.
    let (mut depth, mut unsafe_at) = (0i64, Vec::<i64>::new());
    let derefs = |p: &str| KINDS[RAW_DEREF].1.iter().map(|m| occurrences(p, m)).sum::<usize>();
    for line in non_test_lines(blanked(text))? {
        let line = unextern(&line);
        c.code += 1;
        for (k, (_, markers)) in KINDS.iter().enumerate() {
            if k != RAW_DEREF {
                c.kinds[k] += markers.iter().map(|m| occurrences(&line, m)).sum::<usize>();
            }
        }
        // Cut at each brace, so a deref counts by whether an `unsafe` block is open where it is.
        let mut piece = String::new();
        for ch in line.chars() {
            if ch != '{' && ch != '}' {
                piece.push(ch);
                continue;
            }
            if !unsafe_at.is_empty() {
                c.kinds[RAW_DEREF] += derefs(&piece);
            }
            if ch == '{' {
                depth += 1;
                if piece.trim_end().ends_with("unsafe") {
                    unsafe_at.push(depth);
                }
            } else {
                if unsafe_at.last() == Some(&depth) {
                    unsafe_at.pop();
                }
                depth -= 1;
            }
            piece.clear();
        }
        if !unsafe_at.is_empty() {
            c.kinds[RAW_DEREF] += derefs(&piece);
        }
    }
    Ok(c)
}

/// The groups to count with no paths given, from `tracked`, the tracked files: each userspace
/// crate whose `Cargo.toml` is tracked, and each entry of `kernel/src` holding tracked Rust.
fn default_groups(tracked: &[&str]) -> Vec<String> {
    let mut groups = std::collections::BTreeSet::new();
    for path in tracked {
        match path.split('/').collect::<Vec<_>>().as_slice() {
            ["userspace", krate, "Cargo.toml"] => {
                groups.insert(format!("userspace/{krate}"));
            }
            ["kernel", "src", entry, ..] if path.ends_with(".rs") => {
                groups.insert(format!("kernel/src/{entry}"));
            }
            _ => {}
        }
    }
    groups.into_iter().collect()
}

/// The groups `args` name, a trailing `/` dropped so `kernel/src/mm/` and `kernel/src/mm` are one.
fn named_groups(args: &[String]) -> Vec<String> {
    args.iter().map(|p| p.trim_end_matches('/').to_string()).collect()
}

/// The counts over `files`, each a tracked path and its text, test files left out; refused if
/// that leaves no code, so a path that names nothing is not a row of zeros.
fn tally(group: &str, files: &[(String, String)]) -> R<Counts> {
    let mut total = Counts::default();
    for (path, text) in files.iter().filter(|(p, _)| p.ends_with(".rs") && !is_test_file(p)) {
        total.add(&count(text).map_err(|e| format!("unsafe-inventory: {path}: {e}"))?);
    }
    if total.code == 0 {
        return Err(format!("unsafe-inventory: no tracked Rust code under {group}").into());
    }
    Ok(total)
}

/// `unsafe-inventory [PATH...]`: a Markdown table, a row per path (every userspace crate and kernel
/// module when none is given), and their total.
pub(crate) fn cmd_unsafe_inventory(args: &[String]) -> R<()> {
    let root = repo_root();
    let groups: Vec<String> = if args.is_empty() {
        default_groups(&git(&["ls-files", "--", "userspace", "kernel/src"])?.lines().collect::<Vec<_>>())
    } else {
        named_groups(args)
    };
    let mut total = Counts::default();
    let mut rows = Vec::new();
    for g in groups {
        let mut files = Vec::new();
        for f in git(&["ls-files", "--", &g])?.lines().filter(|f| f.ends_with(".rs")) {
            let text = fs::read_to_string(root.join(f)).map_err(|e| format!("unsafe-inventory: {f}: {e}"))?;
            files.push((f.to_string(), text));
        }
        let c = tally(&g, &files)?;
        total.add(&c);
        rows.push((g, c));
    }
    rows.push(("total".to_string(), total));
    let head: Vec<&str> = KINDS.iter().map(|(name, _)| *name).collect();
    println!("| path | code lines | {} |", head.join(" | "));
    println!("|---|---:|{}", "---:|".repeat(KINDS.len()));
    for (p, c) in &rows {
        let cells: Vec<String> = c.kinds.iter().map(usize::to_string).collect();
        println!("| `{p}` | {} | {} |", c.code, cells.join(" | "));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
