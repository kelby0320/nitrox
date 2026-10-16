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
    hunk: usize,
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
    let mut hunk = 0;
    for raw in diff.lines() {
        let plain = strip_ansi(raw);
        if plain.starts_with("@@") {
            hunk += 1;
        } else if let Some(path) = plain.strip_prefix("+++ ") {
            file = path.strip_prefix("b/").unwrap_or(path).to_string();
        } else if let Some(path) = plain.strip_prefix("--- ") {
            file = path.strip_prefix("a/").unwrap_or(path).to_string();
        } else if let Some(text) =
            plain.strip_prefix('+').map(|t| (true, t)).or(plain.strip_prefix('-').map(|t| (false, t)))
        {
            out.push(Changed {
                file: file.clone(),
                hunk,
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

/// `mod NAME {` (with its visibility) as `mod NAME;`, the line that replaces it when the inline
/// module becomes a file of its own.
fn made_a_file(t: &str) -> Option<String> {
    let line = format!("{};", t.strip_suffix(" {")?);
    is_mod_line(&line).then_some(line)
}

/// `text` with its `include_bytes!` or `include_str!` path taken out, and that path resolved from
/// `file`'s directory: a line moved to another directory names the same file by another path.
fn rerooted_include(file: &str, text: &str) -> Option<(String, String)> {
    let at = ["include_bytes!(\"", "include_str!(\""].iter().find_map(|m| text.find(m).map(|i| i + m.len()))?;
    let len = text[at..].find('"')?;
    let mut parts: Vec<&str> = file.split('/').collect();
    parts.pop();
    for part in text[at..at + len].split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some((format!("{}{}", &text[..at], &text[at + len..]), parts.join("/")))
}

/// Every changed line that is not part of a move, as `file: ±text`. Allowed besides moved blocks:
/// blank lines, `mod` lines with a `#[cfg(test)]` before them, `use` statements, a line whose
/// only change is a visibility widened to `pub(crate)` or `pub(super)`, or an include's path
/// re-rooted to name the same file from the line's new directory, with the lines around it that
/// are the same on both sides, and an inline module made a file: its `mod NAME {` replaced by
/// `mod NAME;`, and one closing `}` in that file for each.
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
    let mut pairs = Vec::new();
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
            pairs.push((removed.swap_remove(k), i));
        }
    }
    for i in 0..lines.len() {
        if ok[i] || !lines[i].added {
            continue;
        }
        let added = rerooted_include(&lines[i].file, lines[i].text.trim());
        if added.is_some()
            && let Some(k) =
                removed.iter().position(|&j| rerooted_include(&lines[j].file, lines[j].text.trim()) == added)
        {
            ok[i] = true;
            pairs.push((removed.swap_remove(k), i));
        }
    }
    // A widened line, or a re-rooted include, splits git's moved block, and a piece of it too short for git to call moved is
    // left uncoloured: the same lines on both sides, running on from the pair, are that piece. A run
    // is a hunk's removed lines, or its added ones.
    let mut runs: Vec<Vec<usize>> = Vec::new();
    let mut place = vec![(0, 0); lines.len()];
    for (i, l) in lines.iter().enumerate() {
        if i == 0 || lines[i - 1].hunk != l.hunk || lines[i - 1].added != l.added {
            runs.push(Vec::new());
        }
        let run = runs.len() - 1;
        place[i] = (run, runs[run].len());
        runs[run].push(i);
    }
    for &(r, a) in &pairs {
        ok[r] = true;
        let ((rr, rp), (ar, ap)) = (place[r], place[a]);
        for step in [1, -1] {
            let mut k = step;
            while let (Some(&x), Some(&y)) =
                (runs[rr].get(rp.wrapping_add_signed(k)), runs[ar].get(ap.wrapping_add_signed(k)))
            {
                if lines[x].text.trim() != lines[y].text.trim() {
                    break;
                }
                ok[x] = true;
                ok[y] = true;
                k += step;
            }
        }
    }
    let mut closes: Vec<&str> = Vec::new();
    for i in 0..lines.len() {
        let l = &lines[i];
        if ok[i] || l.added {
            continue;
        }
        if let Some(as_file) = made_a_file(l.text.trim())
            && lines.iter().any(|n| n.added && n.file == l.file && n.text.trim() == as_file)
        {
            ok[i] = true;
            closes.push(&l.file);
        }
    }
    for i in 0..lines.len() {
        if ok[i] || lines[i].added || lines[i].text != "}" {
            continue;
        }
        if let Some(k) = closes.iter().position(|f| *f == lines[i].file) {
            ok[i] = true;
            closes.swap_remove(k);
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
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
            std::fs::write(root.join(path), text).unwrap();
        }
        git(&["add", "-A"]);
        git(&["commit", "-qm", "before"]);
        for (path, _) in before {
            let _ = std::fs::remove_file(root.join(path));
        }
        for (path, text) in after {
            std::fs::create_dir_all(root.join(path).parent().unwrap()).unwrap();
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
    fn an_inline_module_made_a_file_of_its_own_is_a_move() {
        let inline = BODY.lines().map(|l| format!("    {l}\n")).collect::<String>();
        let before = format!("{KEEP}\n#[cfg(test)]\nmod t {{\n    use super::*;\n\n{inline}}}\n");
        let after = format!("{KEEP}\n#[cfg(test)]\nmod t;\n");
        let file = format!("use super::*;\n\n{BODY}");
        let d = diff_of(&[("a.rs", &before)], &[("a.rs", &after), ("t.rs", &file)]);
        assert_eq!(unmoved(&d), Vec::<String>::new());
    }

    #[test]
    fn an_inline_module_is_only_made_a_file_by_a_mod_line_of_its_own_name() {
        let inline = BODY.lines().map(|l| format!("    {l}\n")).collect::<String>();
        let before = format!("{KEEP}\nmod t {{\n{inline}}}\n");
        let after = format!("mod u;\n{KEEP}");
        let stray = unmoved(&diff_of(&[("a.rs", &before)], &[("a.rs", &after), ("t.rs", BODY)]));
        assert_eq!(stray, ["a.rs: -mod t {", "a.rs: -}"]);
    }

    const FRAME: &str = "struct Frame {\n    sample: String,\n}\n\nimpl Frame {\n    fn cells_sample_of_the_frame(&self) \
                         -> &str {\n        &self.sample\n    }\n}\n";

    #[test]
    fn a_tail_too_short_for_git_after_a_widened_line_is_part_of_the_move() {
        let before = format!("{KEEP}\n{FRAME}");
        let moved =
            format!("use crate::*;\n\n{}", FRAME.replace("struct", "pub(crate) struct").replace("fn", "pub(crate) fn"));
        let d = diff_of(&[("a.rs", &before)], &[("a.rs", KEEP), ("b.rs", &moved)]);
        assert_eq!(unmoved(&d), Vec::<String>::new());
    }

    #[test]
    fn a_tail_after_a_widened_line_is_only_a_move_where_it_is_the_same() {
        let before = format!("{KEEP}\n{FRAME}");
        let moved = FRAME.replace("fn", "pub(crate) fn").replace("&self.sample", "&self.other");
        let stray = unmoved(&diff_of(&[("a.rs", &before)], &[("a.rs", KEEP), ("b.rs", &moved)]));
        assert!(stray.iter().any(|s| s == "a.rs: -        &self.sample"), "{stray:?}");
        assert!(stray.iter().any(|s| s == "b.rs: +        &self.other"), "{stray:?}");
    }

    const LONG: &str = "fn widened_function_with_a_long_name() -> u32 {\n    7\n}\n";
    const MARKER: &str = "    continue_marker();\n";

    #[test]
    fn a_widened_lines_neighbours_are_in_its_own_hunk() {
        let other = "fn other() {\n";
        let before = format!("{LONG}{KEEP}{other}{MARKER}}}\n");
        let after = format!("{KEEP}{other}}}\n");
        let moved = format!("pub(crate) {LONG}{MARKER}");
        let stray = unmoved(&diff_of(&[("a.rs", &before)], &[("a.rs", &after), ("b.rs", &moved)]));
        assert_eq!(stray, ["a.rs: -    continue_marker();", "b.rs: +    continue_marker();"]);
    }

    #[test]
    fn a_widened_lines_neighbours_are_on_its_own_side() {
        let before = format!("{LONG}{KEEP}");
        let after = format!("{MARKER}{KEEP}");
        let moved = format!("pub(crate) {LONG}{MARKER}");
        let stray = unmoved(&diff_of(&[("a.rs", &before)], &[("a.rs", &after), ("b.rs", &moved)]));
        assert_eq!(stray, ["a.rs: +    continue_marker();", "b.rs: +    continue_marker();"]);
    }

    fn font(path: &str) -> String {
        format!("fn the_font_the_test_draws_with() -> &'static [u8] {{\n    include_bytes!(\"{path}\")\n}}\n")
    }

    #[test]
    fn an_include_rerooted_for_its_new_directory_is_a_move() {
        let before = format!("{KEEP}\n{}", font("../assets/f.ttf"));
        let d = diff_of(&[("src/a.rs", &before)], &[("src/a.rs", KEEP), ("src/sub/b.rs", &font("../../assets/f.ttf"))]);
        assert_eq!(unmoved(&d), Vec::<String>::new());
    }

    #[test]
    fn an_include_rerooted_to_another_file_is_not_a_move() {
        let before = format!("{KEEP}\n{}", font("../assets/f.ttf"));
        let d = diff_of(&[("src/a.rs", &before)], &[("src/a.rs", KEEP), ("src/sub/b.rs", &font("../../assets/g.ttf"))]);
        let stray = unmoved(&d);
        assert!(stray.iter().any(|s| s.ends_with("include_bytes!(\"../../assets/g.ttf\")")), "{stray:?}");
    }

    #[test]
    fn a_cfg_test_that_is_not_on_a_mod_line_is_not_a_move() {
        let before = format!("{KEEP}\n{BODY}");
        let after = format!("#[cfg(test)]\n{KEEP}");
        let stray = unmoved(&diff_of(&[("a.rs", &before)], &[("a.rs", &after), ("b.rs", BODY)]));
        assert_eq!(stray, ["a.rs: +#[cfg(test)]"]);
    }
}
