//! `check-moves`: a commit that moves code between files and changes nothing else.

use std::path::Path;
use std::process::Command;

type R<T> = Result<T, Box<dyn std::error::Error>>;

/// The colours `git diff --color-moved` is told to use, so a line's colour says whether it moved.
const COLOURS: [(&str, &str); 6] = [
    ("color.diff.old", "red"),
    ("color.diff.new", "green"),
    ("color.diff.oldMoved", "blue"),
    ("color.diff.newMoved", "cyan"),
    ("color.diff.oldMovedAlternative", "blue"),
    ("color.diff.newMovedAlternative", "cyan"),
];
const MOVED: [&str; 2] = ["34", "36"];

pub(crate) fn cmd_check_moves(args: &[String]) -> R<()> {
    let rev = args.first().map_or("HEAD", String::as_str);
    let diff = moved_diff(&crate::repo_root(), rev)?;
    let stray = unmoved(&diff);
    if stray.is_empty() {
        println!("check-moves: {rev} moves code and changes nothing else ✓");
        return Ok(());
    }
    for s in &stray {
        eprintln!("  {s}");
    }
    Err(format!("check-moves: {} line(s) of {rev} are not part of a move", stray.len()).into())
}

/// `rev`'s diff against its parent, each line coloured by whether git found it moved.
fn moved_diff(root: &Path, rev: &str) -> R<String> {
    let mut git = Command::new("git");
    for (key, colour) in COLOURS {
        git.args(["-c", &format!("{key}={colour}")]);
    }
    let parent = format!("{rev}^");
    let out = git
        .args(["diff", "--color=always", "--color-moved=blocks", "--color-moved-ws=allow-indentation-change"])
        .args(["--no-ext-diff", "-U0", &parent, rev])
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        return Err(format!("git diff {parent} {rev}: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

struct Changed {
    file: String,
    added: bool,
    moved: bool,
    text: String,
}

fn strip_ansi(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The SGR code a line starts with: `31` for `ESC[31m…`.
fn leading_colour(line: &str) -> &str {
    line.strip_prefix("\x1b[").and_then(|r| r.split_once('m')).map_or("", |(code, _)| code)
}

fn changed_lines(diff: &str) -> Vec<Changed> {
    let mut out = Vec::new();
    let mut file = String::new();
    for raw in diff.lines() {
        let plain = strip_ansi(raw);
        if let Some(path) = plain.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").unwrap_or(path).to_string();
        } else if let Some(path) = plain.strip_prefix("--- ") {
            file = path.strip_prefix("a/").unwrap_or(path).to_string();
        } else if let Some(text) =
            plain.strip_prefix('+').map(|t| (true, t)).or(plain.strip_prefix('-').map(|t| (false, t)))
        {
            out.push(Changed {
                file: file.clone(),
                added: text.0,
                moved: MOVED.contains(&leading_colour(raw)),
                text: text.1.to_string(),
            });
        }
    }
    out
}

fn is_mod_line(t: &str) -> bool {
    let rest = ["pub(crate) mod ", "pub(super) mod ", "pub mod ", "mod "].iter().find_map(|p| t.strip_prefix(p));
    rest.and_then(|r| r.strip_suffix(';'))
        .is_some_and(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
}

fn starts_use(t: &str) -> bool {
    ["use ", "pub use ", "pub(crate) use ", "pub(super) use "].iter().any(|p| t.starts_with(p))
}

/// Every changed line that is not part of a move, as `file: ±text`. Allowed besides moved blocks:
/// blank lines, `mod` lines with a `#[cfg(test)]` before them, `use` statements, and a line whose
/// only change is a visibility widened to `pub(crate)` or `pub(super)`.
fn unmoved(diff: &str) -> Vec<String> {
    let lines = changed_lines(diff);
    let mut ok = vec![false; lines.len()];
    let mut in_use = false;
    for (i, l) in lines.iter().enumerate() {
        if l.moved {
            in_use = false;
            ok[i] = true;
            continue;
        }
        let t = l.text.trim();
        if in_use {
            ok[i] = true;
            in_use = !t.ends_with(';');
        } else if t.is_empty() || is_mod_line(t) {
            ok[i] = true;
        } else if starts_use(t) {
            ok[i] = true;
            in_use = !t.ends_with(';');
        } else if t == "#[cfg(test)]" {
            let next = lines.get(i + 1);
            ok[i] =
                next.is_some_and(|n| !n.moved && n.added == l.added && n.file == l.file && is_mod_line(n.text.trim()));
        }
    }
    let mut removed: Vec<usize> = (0..lines.len()).filter(|&i| !ok[i] && !lines[i].added).collect();
    for i in 0..lines.len() {
        if ok[i] || !lines[i].added {
            continue;
        }
        let t = lines[i].text.trim();
        let narrowed = ["pub(crate) ", "pub(super) "].iter().find_map(|p| t.strip_prefix(p));
        if let Some(rest) = narrowed
            && let Some(k) = removed.iter().position(|&j| lines[j].text.trim() == rest)
        {
            ok[i] = true;
            ok[removed.swap_remove(k)] = true;
        }
    }
    lines
        .iter()
        .zip(&ok)
        .filter(|(_, ok)| !**ok)
        .map(|(l, _)| format!("{}: {}{}", l.file, if l.added { '+' } else { '-' }, l.text))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A repository whose second commit is `after`, applied over a first commit of `before`.
    fn diff_of(before: &[(&str, &str)], after: &[(&str, &str)]) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!("xtask-moves-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args(["-c", "user.email=t@t", "-c", "user.name=t"])
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?}");
        };
        git(&["init", "-q"]);
        for (path, text) in before {
            std::fs::write(root.join(path), text).unwrap();
        }
        git(&["add", "-A"]);
        git(&["commit", "-qm", "before"]);
        for (path, _) in before {
            let _ = std::fs::remove_file(root.join(path));
        }
        for (path, text) in after {
            std::fs::write(root.join(path), text).unwrap();
        }
        git(&["add", "-A"]);
        git(&["commit", "-qm", "after"]);
        let d = moved_diff(&root, "HEAD").unwrap();
        std::fs::remove_dir_all(&root).unwrap();
        d
    }

    const BODY: &str = "fn moved_away(x: u32) -> u32 {\n    let doubled = x * 2;\n    let tripled = doubled + x;\n    \
                        tripled + 1\n}\n";
    const KEEP: &str = "fn keep() {\n    let a = 1;\n}\n";

    #[test]
    fn a_function_moved_to_another_file_is_a_move() {
        let before = format!("{KEEP}\n{BODY}");
        let after = format!("#[cfg(test)]\nmod b;\nuse crate::b::{{\n    moved_away,\n}};\n\n{KEEP}");
        let moved = format!("pub(crate) {BODY}");
        let d = diff_of(&[("a.rs", &before)], &[("a.rs", &after), ("b.rs", &moved)]);
        assert_eq!(unmoved(&d), Vec::<String>::new());
    }

    #[test]
    fn an_edit_inside_a_move_is_not_a_move() {
        let before = format!("{KEEP}\n{BODY}");
        let moved = BODY.replace("x * 2", "x * 3");
        let d = diff_of(&[("a.rs", &before)], &[("a.rs", KEEP), ("b.rs", &moved)]);
        let stray = unmoved(&d);
        assert!(stray.iter().any(|s| s == "b.rs: +    let doubled = x * 3;"), "{stray:?}");
        assert!(stray.iter().any(|s| s == "a.rs: -    let doubled = x * 2;"), "{stray:?}");
    }

    #[test]
    fn a_line_deleted_in_one_place_and_added_in_another_is_not_a_move() {
        let before = "fn a(v: &[u32]) {\n    for x in v {\n        if *x == 0 {\n            continue;\n        }\n    }\n}\n\
                      fn b() {}\n";
        let after = "fn a(v: &[u32]) {\n    for x in v {\n        if *x == 0 {\n        }\n    }\n}\n\
                     fn b() {\n            continue;\n}\n";
        let stray = unmoved(&diff_of(&[("a.rs", before)], &[("a.rs", after)]));
        assert!(stray.iter().any(|s| s.contains("-            continue;")), "{stray:?}");
        assert!(stray.iter().any(|s| s.contains("+            continue;")), "{stray:?}");
    }

    #[test]
    fn a_cfg_test_that_is_not_on_a_mod_line_is_not_a_move() {
        let before = format!("{KEEP}\n{BODY}");
        let after = format!("#[cfg(test)]\n{KEEP}");
        let stray = unmoved(&diff_of(&[("a.rs", &before)], &[("a.rs", &after), ("b.rs", BODY)]));
        assert_eq!(stray, ["a.rs: +#[cfg(test)]"]);
    }
}
