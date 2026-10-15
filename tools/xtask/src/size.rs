//! `check-size`: no Rust file grows past 1,000 lines, and the files already past it only shrink.

use std::collections::BTreeMap;
use std::process::Command;

type R<T> = Result<T, Box<dyn std::error::Error>>;

const LIMIT: usize = 1000;
const BASELINE: &str = "tools/xtask/size-baseline.toml";
const HEADER: &str = "# Rust files allowed over 1,000 lines, each with its length. An entry only goes down:\n\
                      # `cargo xtask check-size --update` lowers or removes entries as files shrink.\n\
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
            out.push(format!("{path}: listed but gone: run `cargo xtask check-size --update`"));
        }
    }
    out
}

fn updated(files: &[(String, usize)], baseline: &Baseline) -> Baseline {
    let mut out = Baseline::new();
    for (path, &entry) in baseline {
        if let Some((_, lines)) = files.iter().find(|(p, _)| p == path) {
            if *lines > LIMIT {
                out.insert(path.clone(), entry.min(*lines));
            }
        }
    }
    out
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
    for path in String::from_utf8_lossy(&out.stdout).split('\0').filter(|p| !p.is_empty()) {
        let full = root.join(path);
        if full.is_file() {
            let lines = std::fs::read(&full)?.iter().filter(|&&b| b == b'\n').count();
            files.push((path.to_string(), lines));
        }
    }
    Ok(files)
}

pub(crate) fn cmd_check_size(args: &[String]) -> R<()> {
    let files = measure()?;
    let path = crate::repo_root().join(BASELINE);
    let mut baseline = parse_baseline(&std::fs::read_to_string(&path)?)?;
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
