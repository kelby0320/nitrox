//! `check-size`: no Rust file grows past 1,000 lines, and the files already past it only shrink.

use std::collections::BTreeMap;
use std::process::Command;

type R<T> = Result<T, Box<dyn std::error::Error>>;

const LIMIT: usize = 1000;
const BASELINE: &str = "tools/xtask/size-baseline.toml";
const HEADER: &str = "# Rust files allowed over 1,000 lines, each with its length. The tools only lower an entry:\n\
                      # `cargo xtask check-size --update` lowers or removes entries as files shrink. A raise is\n\
                      # by hand, in a reviewed commit of its own that says why.\n\
                      [files]\n";

type Baseline = BTreeMap<String, usize>;

fn parse_baseline(text: &str) -> R<Baseline> {
    let mut map = Baseline::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') || t == "[files]" {
            continue;
        }
        let entry = t
            .strip_prefix('"')
            .and_then(|r| r.split_once("\" = "))
            .and_then(|(path, n)| Some((path, n.parse::<usize>().ok()?)));
        match entry {
            Some((path, n)) => {
                map.insert(path.to_string(), n);
            }
            None => return Err(format!("{BASELINE}: not `\"path\" = lines`: {line}").into()),
        }
    }
    Ok(map)
}

fn format_baseline(baseline: &Baseline) -> String {
    let mut out = HEADER.to_string();
    for (path, n) in baseline {
        out.push_str(&format!("\"{path}\" = {n}\n"));
    }
    out
}

fn failures(files: &[(String, usize)], baseline: &Baseline) -> Vec<String> {
    let mut out = Vec::new();
    for (path, lines) in files {
        match baseline.get(path) {
            None if *lines > LIMIT => out.push(format!(
                "{path}: {lines} lines, over {LIMIT}: split it by responsibility, with its tests in a sibling `tests.rs`"
            )),
            Some(_) if *lines <= LIMIT => out.push(format!(
                "{path}: {lines} lines, no longer over {LIMIT}: run `cargo xtask check-size --update`"
            )),
            Some(&entry) if *lines > entry => out.push(format!(
                "{path}: {lines} lines, over its entry of {entry}: it may not grow; split it instead"
            )),
            Some(&entry) if *lines < entry => out.push(format!(
                "{path}: {lines} lines, under its entry of {entry}: run `cargo xtask check-size --update`"
            )),
            _ => {}
        }
    }
    for path in baseline.keys() {
        if !files.iter().any(|(p, _)| p == path) {
            out.push(format!(
                "{path}: listed but gone: if it moved, `cargo xtask check-size --rename {path} <new path>`; \
                 if it went, `cargo xtask check-size --update`"
            ));
        }
    }
    out
}

fn updated(files: &[(String, usize)], baseline: &Baseline) -> Baseline {
    let mut out = Baseline::new();
    for (path, &entry) in baseline {
        if let Some((_, lines)) = files.iter().find(|(p, _)| p == path)
            && *lines > LIMIT
        {
            out.insert(path.clone(), entry.min(*lines));
        }
    }
    out
}

/// Moves `old`'s entry to `new`, the file it became: at most its old allowance, and none when `new`
/// is 1,000 lines or fewer.
fn renamed(files: &[(String, usize)], baseline: &Baseline, old: &str, new: &str) -> R<Baseline> {
    let entry = *baseline.get(old).ok_or_else(|| format!("{old} has no entry to move"))?;
    if files.iter().any(|(p, _)| p == old) {
        return Err(format!("{old} still exists: a rename moves an entry from a file that has gone").into());
    }
    if baseline.contains_key(new) {
        return Err(format!("{new} already has an entry").into());
    }
    let (_, lines) = files.iter().find(|(p, _)| p == new).ok_or_else(|| format!("{new} is not a tracked Rust file"))?;
    let mut out = baseline.clone();
    out.remove(old);
    if *lines > LIMIT {
        out.insert(new.to_string(), entry.min(*lines));
    }
    Ok(out)
}

/// The lines at the top of a file that any module needs whatever it holds — its doc, `mod` and
/// `use` lines, attributes and blank lines — which a split does not count against the allowance.
fn leading_overhead(text: &str) -> usize {
    let starts_use = |t: &str| ["use ", "pub use ", "pub(crate) use "].iter().any(|p| t.starts_with(p));
    let mut n = 0;
    let mut in_use = false;
    for line in text.lines() {
        let t = line.trim();
        let needed = in_use
            || starts_use(t)
            || t.is_empty()
            || t.starts_with("//!")
            || t.starts_with("#[")
            || t.starts_with("#![")
            || ["mod ", "pub mod ", "pub(crate) mod "].iter().any(|p| t.starts_with(p) && t.ends_with(';'));
        if !needed {
            break;
        }
        if in_use || starts_use(t) {
            in_use = !t.ends_with(';');
        }
        n += 1;
    }
    n
}

/// Moves `old`'s allowance onto what it was split into: each of `news` still over 1,000 lines gets
/// an entry at its length, and `old` keeps one at most its own length, or none at 1,000 or fewer.
/// Refused if the pieces sum past `old`'s entry, not counting each new file's leading lines
/// ([`leading_overhead`]) or the `mod` line each needs in its parent.
fn split(
    files: &[(String, usize)],
    baseline: &Baseline,
    overhead: &dyn Fn(&str) -> R<usize>,
    old: &str,
    news: &[String],
) -> R<Baseline> {
    let entry = *baseline.get(old).ok_or_else(|| format!("{old} has no entry to split"))?;
    let len = |p: &str| files.iter().find(|(f, _)| f == p).map(|(_, n)| *n);
    let mut out = baseline.clone();
    out.remove(old);
    let mut total = len(old).unwrap_or(0);
    for new in news {
        if new == old || baseline.contains_key(new) {
            return Err(format!("{new} already has an entry").into());
        }
        let n = len(new).ok_or_else(|| format!("{new} is not a tracked Rust file"))?;
        total += n.saturating_sub(overhead(new)?);
        if n > LIMIT {
            out.insert(new.clone(), n);
        }
    }
    if total > entry + news.len() {
        return Err(format!(
            "{old} had {entry} lines and its pieces hold {total}, past it less the `mod` lines: a split moves an \
             allowance and never grows it"
        )
        .into());
    }
    if let Some(n) = len(old).filter(|&n| n > LIMIT) {
        out.insert(old.to_string(), entry.min(n));
    }
    Ok(out)
}

/// The paths a NUL-separated `git ls-files` listing names, once each: a conflicted merge lists a
/// path once per stage.
fn rs_paths(listing: &[u8]) -> Vec<String> {
    let mut paths: Vec<String> =
        String::from_utf8_lossy(listing).split('\0').filter(|p| !p.is_empty()).map(String::from).collect();
    paths.sort();
    paths.dedup();
    paths
}

fn measure() -> R<Vec<(String, usize)>> {
    let root = crate::repo_root();
    let out = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z", "--", "*.rs"])
        .current_dir(&root)
        .output()?;
    if !out.status.success() {
        return Err(format!("git ls-files: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    let mut files = Vec::new();
    for path in rs_paths(&out.stdout) {
        let full = root.join(&path);
        if full.is_file() {
            let bytes = std::fs::read(&full).map_err(|e| format!("{}: {e}", full.display()))?;
            files.push((path, bytes.iter().filter(|&&b| b == b'\n').count()));
        }
    }
    Ok(files)
}

pub(crate) fn cmd_check_size(args: &[String]) -> R<()> {
    let files = measure()?;
    let path = crate::repo_root().join(BASELINE);
    let mut baseline = parse_baseline(&crate::read_text(&path)?)?;
    if let Some(i) = args.iter().position(|a| a == "--rename") {
        let (Some(old), Some(new)) = (args.get(i + 1), args.get(i + 2)) else {
            return Err("check-size --rename takes OLD NEW".into());
        };
        baseline = renamed(&files, &baseline, old, new)?;
        std::fs::write(&path, format_baseline(&baseline))?;
        println!("check-size: {old} -> {new}");
    }
    if let Some(i) = args.iter().position(|a| a == "--split") {
        let (Some(old), news) = (args.get(i + 1), &args[(i + 2).min(args.len())..]) else {
            return Err("check-size --split takes OLD NEW...".into());
        };
        if news.is_empty() {
            return Err("check-size --split takes OLD NEW...".into());
        }
        let root = crate::repo_root();
        let overhead = |p: &str| -> R<usize> { Ok(leading_overhead(&crate::read_text(&root.join(p))?)) };
        baseline = split(&files, &baseline, &overhead, old, news)?;
        std::fs::write(&path, format_baseline(&baseline))?;
        println!("check-size: {old} split into {}", news.join(", "));
    }
    if args.iter().any(|a| a == "--update") {
        let new = updated(&files, &baseline);
        for (file, old) in &baseline {
            match new.get(file) {
                Some(n) if n == old => {}
                Some(n) => println!("check-size: {file}: {old} -> {n}"),
                None => println!("check-size: {file}: removed"),
            }
        }
        std::fs::write(&path, format_baseline(&new))?;
        baseline = new;
    }
    let failed = failures(&files, &baseline);
    if !failed.is_empty() {
        for f in &failed {
            eprintln!("check-size: {f}");
        }
        return Err(format!("check-size: {} failure(s)", failed.len()).into());
    }
    println!("check-size: {} files over 1,000 lines, each at or under its entry ✓", baseline.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[(&str, usize)]) -> Vec<(String, usize)> {
        list.iter().map(|(p, n)| (p.to_string(), *n)).collect()
    }

    fn base(list: &[(&str, usize)]) -> Baseline {
        list.iter().map(|(p, n)| (p.to_string(), *n)).collect()
    }

    #[test]
    fn unlisted_file_is_limited_to_1000() {
        assert!(failures(&files(&[("a.rs", 1000)]), &base(&[])).is_empty());
        assert_eq!(failures(&files(&[("a.rs", 1001)]), &base(&[])).len(), 1);
    }

    #[test]
    fn listed_file_may_not_grow() {
        let b = base(&[("a.rs", 1500)]);
        assert!(failures(&files(&[("a.rs", 1500)]), &b).is_empty());
        let f = failures(&files(&[("a.rs", 1501)]), &b);
        assert_eq!(f.len(), 1);
        assert!(f[0].contains("a.rs") && f[0].contains("may not grow"));
    }

    #[test]
    fn stale_entries_fail() {
        let b = base(&[("a.rs", 1500)]);
        assert_eq!(failures(&files(&[("a.rs", 1499)]), &b).len(), 1);
        assert_eq!(failures(&files(&[("a.rs", 1000)]), &b).len(), 1);
        assert_eq!(failures(&files(&[("a.rs", 10)]), &b).len(), 1);
        assert_eq!(failures(&files(&[]), &b).len(), 1);
        assert_eq!(failures(&files(&[("a.rs", 900)]), &base(&[("a.rs", 900)])).len(), 1);
    }

    #[test]
    fn update_lowers_removes_and_never_raises_or_adds() {
        let b = base(&[("lower.rs", 1500), ("small.rs", 1200), ("gone.rs", 1100), ("grew.rs", 1100)]);
        let f = files(&[("lower.rs", 1400), ("small.rs", 1000), ("grew.rs", 1300), ("new.rs", 2000)]);
        assert_eq!(updated(&f, &b), base(&[("lower.rs", 1400), ("grew.rs", 1100)]));
    }

    #[test]
    fn a_rename_moves_the_entry_with_its_file() {
        let b = base(&[("old.rs", 1500), ("other.rs", 1200)]);
        let moved = renamed(&files(&[("new.rs", 1400), ("other.rs", 1200)]), &b, "old.rs", "new.rs").unwrap();
        assert_eq!(moved, base(&[("new.rs", 1400), ("other.rs", 1200)]));
        let grew = renamed(&files(&[("new.rs", 1600)]), &base(&[("old.rs", 1500)]), "old.rs", "new.rs").unwrap();
        assert_eq!(grew, base(&[("new.rs", 1500)]), "a rename never raises the allowance");
        let small = renamed(&files(&[("new.rs", 1000)]), &base(&[("old.rs", 1500)]), "old.rs", "new.rs").unwrap();
        assert_eq!(small, base(&[]));
    }

    #[test]
    fn a_rename_is_refused_unless_it_is_one() {
        let b = base(&[("old.rs", 1500), ("listed.rs", 1200)]);
        for (f, old, new) in [
            (files(&[("old.rs", 1500), ("new.rs", 1400)]), "old.rs", "new.rs"),
            (files(&[("listed.rs", 1200)]), "old.rs", "listed.rs"),
            (files(&[]), "old.rs", "new.rs"),
            (files(&[("new.rs", 1400)]), "unlisted.rs", "new.rs"),
        ] {
            assert!(renamed(&f, &b, old, new).is_err(), "{old} -> {new}");
        }
    }

    fn no_overhead(_: &str) -> R<usize> {
        Ok(0)
    }

    fn strings(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_split_moves_the_allowance_onto_its_pieces() {
        let b = base(&[("big.rs", 3000), ("other.rs", 1200)]);
        let f = files(&[("big.rs", 1200), ("a.rs", 1600), ("b.rs", 200), ("other.rs", 1200)]);
        let after = split(&f, &b, &no_overhead, "big.rs", &strings(&["a.rs", "b.rs"])).unwrap();
        assert_eq!(after, base(&[("big.rs", 1200), ("a.rs", 1600), ("other.rs", 1200)]));
        let gone = files(&[("a.rs", 900), ("b.rs", 900)]);
        let none = split(&gone, &base(&[("big.rs", 1800)]), &no_overhead, "big.rs", &strings(&["a.rs", "b.rs"]));
        assert_eq!(none.unwrap(), base(&[]), "pieces at 1,000 or fewer need no entry");
    }

    #[test]
    fn a_split_never_grows_the_allowance() {
        let b = base(&[("big.rs", 2000)]);
        let at = files(&[("big.rs", 1001), ("a.rs", 1000)]);
        assert!(split(&at, &b, &no_overhead, "big.rs", &strings(&["a.rs"])).is_ok(), "the one `mod` line");
        let over = files(&[("big.rs", 1002), ("a.rs", 1000)]);
        let err = split(&over, &b, &no_overhead, "big.rs", &strings(&["a.rs"])).unwrap_err().to_string();
        assert!(err.contains("never grows"), "{err}");
        let head = |_: &str| -> R<usize> { Ok(5) };
        assert!(split(&files(&[("big.rs", 1006), ("a.rs", 1000)]), &b, &head, "big.rs", &strings(&["a.rs"])).is_ok());
    }

    #[test]
    fn a_split_is_refused_unless_it_is_one() {
        let b = base(&[("big.rs", 2000), ("listed.rs", 1200)]);
        let f = files(&[("big.rs", 900), ("a.rs", 900), ("listed.rs", 1200)]);
        assert!(split(&f, &b, &no_overhead, "unlisted.rs", &strings(&["a.rs"])).is_err());
        let within = files(&[("big.rs", 100), ("listed.rs", 1200)]);
        assert!(split(&within, &b, &no_overhead, "big.rs", &strings(&["listed.rs"])).is_err(), "within the sum");
        assert!(split(&f, &b, &no_overhead, "big.rs", &strings(&["missing.rs"])).is_err());
    }

    #[test]
    fn a_new_files_leading_lines_are_its_overhead() {
        let text = "//! A module.\n\nuse std::fmt;\nuse crate::{\n    a,\n    b,\n};\n#[allow(dead_code)]\nmod t;\n\nfn f() {}\nuse x;\n";
        assert_eq!(leading_overhead(text), 10);
        assert_eq!(leading_overhead("fn f() {}\n"), 0);
    }

    #[test]
    fn a_path_listed_once_a_merge_stage_is_measured_once() {
        assert_eq!(rs_paths(b"x.rs\0x.rs\0x.rs\0a.rs\0"), ["a.rs", "x.rs"]);
    }

    #[test]
    fn parse_refuses_a_malformed_line() {
        for bad in ["a.rs = 1001", "\"a.rs\" = x", "\"a.rs\" 1001", "\"a.rs\" = "] {
            let err = parse_baseline(&format!("[files]\n{bad}\n")).unwrap_err().to_string();
            assert!(err.contains(bad), "{err}");
        }
    }

    #[test]
    fn parse_then_format_round_trips() {
        let text = format!("{HEADER}\"a/b.rs\" = 1001\n\"c.rs\" = 21136\n");
        assert_eq!(format_baseline(&parse_baseline(&text).unwrap()), text);
    }
}
