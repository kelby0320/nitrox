//! A directory's comments against its budget, and whether a commit touched comments alone.

use std::path::Path;
use std::process::Command;

use super::comments::{Kind, Line, kind, reference, safety_lines};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const TARGET_PERCENT: usize = 15;

/// A directory's comment lines against what it may hold: 0.15 of its code, on top of the docs the
/// rules require — a line for each public item and field, each `TODO(tag)` and each module.
#[derive(Debug, Default, PartialEq, Eq)]
struct Budget {
    code: usize,
    comments: usize,
    floor: usize,
    files: usize,
    /// Each comment citing a PR, a review, a phase or a part, as `file:line: cited`.
    citations: Vec<String>,
}

impl Budget {
    fn allowed(&self) -> usize {
        self.code * TARGET_PERCENT / 100 + self.floor + self.files
    }
}

/// `pub fn x`, `pub(crate) struct Y` (not `pub(crate)`: a crate-private item needs no doc to be
/// read from outside), or a `pub` field.
fn is_public(line: &str) -> bool {
    let t = line.trim_start();
    let Some(mut rest) = t.strip_prefix("pub ") else { return false };
    loop {
        rest = match rest.split_once(' ') {
            Some(("unsafe" | "async", r)) => r,
            Some(("const", r)) if r.starts_with("fn ") || r.starts_with("unsafe ") => r,
            Some(("extern", r)) => r.split_once(' ').map_or("", |(_, r)| r),
            Some(("fn" | "struct" | "enum" | "const" | "static" | "type" | "trait" | "mod" | "union", _)) => {
                return true;
            }
            _ => break,
        }
    }
    let field = rest.split_once(':').map(|(name, _)| name.trim());
    line.starts_with(' ')
        && field.is_some_and(|n| !n.is_empty() && n.chars().all(|c| c == '_' || c.is_ascii_alphanumeric()))
}

fn is_todo(comment: &str) -> bool {
    comment.match_indices("TODO(").any(|(i, _)| {
        let tag = &comment[i + 5..];
        tag.find(')').is_some_and(|end| end > 0 && tag[..end].chars().all(|c| c == '-' || c.is_ascii_alphanumeric()))
    })
}

fn budget(files: &[(String, String)]) -> Budget {
    let mut b = Budget { files: files.len(), ..Budget::default() };
    for (file, text) in files {
        let lines: Vec<Line> = text
            .lines()
            .enumerate()
            .map(|(i, t)| Line { file: file.clone(), line: i + 1, text: t.to_string() })
            .collect();
        let safety = safety_lines(&lines);
        for (line, is_safety) in lines.iter().zip(safety) {
            if kind(&line.text) == Kind::Comment
                && let Some(cited) = reference(&line.text)
            {
                b.citations.push(format!("{file}:{}: {cited}", line.line));
            }
            match kind(&line.text) {
                Kind::Comment if !is_safety => {
                    b.comments += 1;
                    b.floor += usize::from(is_todo(&line.text));
                }
                Kind::Code => {
                    b.code += 1;
                    b.floor += usize::from(is_public(&line.text));
                }
                _ => {}
            }
        }
    }
    b
}

fn git(args: &[&str]) -> R<String> {
    let out = Command::new("git").args(args).current_dir(crate::repo_root()).output()?;
    if !out.status.success() {
        return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `check-comments --ratio DIR`: fails when the tracked Rust files under `DIR` hold more comment
/// lines than their budget allows.
pub(crate) fn cmd_ratio(args: &[String]) -> R<()> {
    let dir = args.first().ok_or("check-comments --ratio DIR")?;
    let root = crate::repo_root();
    let dir = Path::new(dir)
        .strip_prefix(&root)
        .unwrap_or(Path::new(dir))
        .to_string_lossy()
        .trim_end_matches('/')
        .to_string();
    let mut files = Vec::new();
    for f in git(&["ls-files", "--", &format!("{dir}/*.rs")])?.lines() {
        files.push((f.to_string(), std::fs::read_to_string(root.join(f))?));
    }
    if files.is_empty() {
        return Err(format!("check-comments --ratio: no tracked Rust file under {dir}").into());
    }
    let b = budget(&files);
    let ratio = |n: usize| n as f64 / b.code as f64;
    let line = format!(
        "{dir}: {} comment lines to {} of code, {:.3}; it may hold {} (0.15 of its code, and {} public items \
         and TODOs and {} modules)",
        b.comments,
        b.code,
        ratio(b.comments),
        b.allowed(),
        b.floor,
        b.files
    );
    for c in &b.citations {
        eprintln!("  {c}: cites history; say why the code is as it is instead");
    }
    match (b.comments <= b.allowed(), b.citations.is_empty()) {
        (true, true) => {
            println!("check-comments: {line} ✓");
            Ok(())
        }
        (within, _) => {
            let over = if within { String::new() } else { format!("; {} over", b.comments - b.allowed()) };
            Err(format!("check-comments: {line}{over}; {} citation(s)", b.citations.len()).into())
        }
    }
}

/// `text` with its `//` comments taken out, string and character literals left whole, and the lines
/// that held only comment or nothing dropped: what a comment-only change leaves alone.
fn code_lines(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
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
            let end = rest.find(&close).map_or(chars.len(), |e| open + rest[..e].chars().count() + close.len());
            out.extend(&chars[i..end]);
            i = end;
        } else if c == '"' {
            let mut j = i + 1;
            while j < chars.len() && chars[j] != '"' {
                j += if chars[j] == '\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(chars.len());
            out.extend(&chars[i..end]);
            i = end;
        } else if c == '\'' && chars.get(i + 1) == Some(&'\\') {
            let end = chars[i + 2..].iter().position(|&q| q == '\'').map_or(chars.len(), |p| i + 3 + p);
            out.extend(&chars[i..end]);
            i = end;
        } else if c == '\'' && chars.get(i + 2) == Some(&'\'') {
            out.extend(&chars[i..i + 3]);
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
    out.lines().map(str::trim_end).filter(|l| !l.trim().is_empty()).map(String::from).collect()
}

/// The first place `new`'s code differs from `old`'s, as the old line and the new.
fn code_change(old: &str, new: &str) -> Option<(String, String)> {
    let (a, b) = (code_lines(old), code_lines(new));
    let at = a.iter().zip(&b).position(|(x, y)| x != y).unwrap_or(a.len().min(b.len()));
    (a.len() != b.len() || at < a.len()).then(|| {
        let show = |v: &[String]| v.get(at).cloned().unwrap_or_else(|| "(the file's end)".to_string());
        (show(&a), show(&b))
    })
}

/// `check-comments --only-comments [REV | --worktree]`: fails when `REV` (`HEAD` by default), or
/// with `--worktree` the uncommitted changes to tracked files, change anything but comments: a
/// file added, removed or not Rust, or a code line of a Rust file.
pub(crate) fn cmd_only_comments(args: &[String]) -> R<()> {
    let worktree = args.first().is_some_and(|a| a == "--worktree");
    let rev = if worktree { "HEAD" } else { args.first().map_or("HEAD", String::as_str) };
    let (base, what) =
        if worktree { (rev.to_string(), "the worktree".to_string()) } else { (format!("{rev}^"), rev.to_string()) };
    let mut range = vec!["diff", "--no-renames", "--name-status", &base];
    if !worktree {
        range.push(rev);
    }
    let mut failures = Vec::new();
    let mut files = 0;
    for entry in git(&range)?.lines() {
        let Some((status, path)) = entry.split_once('\t') else { continue };
        if status != "M" || !path.ends_with(".rs") {
            failures.push(format!("{path}: {status}, which is not a Rust file's comments changing"));
            continue;
        }
        files += 1;
        let old = git(&["show", &format!("{base}:{path}")])?;
        let new = if worktree {
            std::fs::read_to_string(crate::repo_root().join(path))?
        } else {
            git(&["show", &format!("{rev}:{path}")])?
        };
        if let Some((was, is)) = code_change(&old, &new) {
            failures.push(format!("{path}: code changed — `{}` became `{}`", was.trim(), is.trim()));
        }
    }
    if failures.is_empty() {
        println!("check-comments: {what} changes comments alone, in {files} Rust file(s) ✓");
        return Ok(());
    }
    for f in &failures {
        eprintln!("  {f}");
    }
    Err(format!("check-comments: {what} changes more than comments ({} file(s))", failures.len()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crate_of(text: &str) -> Budget {
        budget(&[("a.rs".to_string(), text.to_string())])
    }

    #[test]
    fn a_crate_at_its_allowance_passes_and_one_line_over_fails() {
        let code: String = (0..20).map(|i| format!("let x{i} = {i};\n")).collect();
        let at = format!("{code}//! The module.\n// one\n// two\n// three\n");
        let b = crate_of(&at);
        assert_eq!((b.code, b.comments, b.allowed()), (20, 4, 4));
        assert!(b.comments <= b.allowed());
        let over = crate_of(&format!("{at}// four\n"));
        assert!(over.comments > over.allowed(), "{over:?}");
    }

    #[test]
    fn safety_comments_are_not_counted_and_public_items_raise_the_floor() {
        let b = crate_of(
            "// SAFETY: the pointer is live.\nunsafe { f() };\n/// A thing.\npub fn a() {}\npub struct S {\n    pub field: u32,\n    private: u32,\n}\npub const C: u32 = 1;\npub const fn d() {}\n// TODO(tag): later.\n",
        );
        assert_eq!((b.code, b.comments, b.floor), (8, 2, 6));
    }

    #[test]
    fn the_required_docs_are_on_top_of_the_share_of_code() {
        let code: String = (0..20).map(|i| format!("pub fn f{i}() {{}}\n")).collect();
        let b = budget(&[("a.rs".to_string(), code.clone()), ("b.rs".to_string(), code)]);
        assert_eq!((b.code, b.floor, b.files, b.allowed()), (40, 40, 2, 6 + 40 + 2));
    }

    #[test]
    fn a_comment_citing_history_is_found_and_one_saying_why_is_not() {
        let b = crate_of(
            "// Kept since PR #123's review.\nlet a = 1;\n// The cache is per file.\n/// Phase 6 Part H.4 made it.\n",
        );
        assert_eq!(b.citations.len(), 2, "{:?}", b.citations);
        assert!(b.citations[0].starts_with("a.rs:1: "), "{:?}", b.citations);
    }

    #[test]
    fn a_crate_private_item_does_not_raise_the_floor() {
        assert_eq!(crate_of("pub(crate) fn a() {}\npub(super) const B: u32 = 1;\nfn c() {}\n").floor, 0);
    }

    #[test]
    fn removing_and_shortening_comments_changes_no_code() {
        let old = "/// A long doc\n/// on two lines.\nfn a() {\n    // why\n    x(); // because\n}\n";
        let new = "/// A doc.\nfn a() {\n    x();\n}\n";
        assert_eq!(code_change(old, new), None);
    }

    #[test]
    fn a_code_edit_among_comment_edits_is_found() {
        let old = "fn a() {\n    // why\n    x();\n}\n";
        let new = "fn a() {\n    y();\n}\n";
        assert_eq!(code_change(old, new), Some(("    x();".to_string(), "    y();".to_string())));
    }

    #[test]
    fn a_slash_slash_in_a_string_is_code() {
        let old = "let u = \"http://a\"; // the host\nlet r = r#\"// not a comment\"#;\nlet c = '/';\n";
        assert_eq!(code_change(old, "let u = \"http://a\";\nlet r = r#\"// not a comment\"#;\nlet c = '/';\n"), None);
        assert!(code_change(old, "let u = \"http://b\";\nlet r = r#\"// not a comment\"#;\nlet c = '/';\n").is_some());
        assert!(code_change(old, "let u = \"http://a\";\nlet r = r#\"// changed\"#;\nlet c = '/';\n").is_some());
    }

    #[test]
    fn a_code_line_removed_or_added_is_found() {
        assert!(code_change("x();\ny();\n", "x();\n").is_some());
        assert!(code_change("x();\n", "x();\ny();\n").is_some());
    }
}
