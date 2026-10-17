//! `check-comments`, and the edit hook built on it: how much of a change is comment, and whether a
//! comment cites a PR, a review, a phase or a part.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::process::{Command, ExitCode};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const LIMIT_PERCENT: usize = 15;
const ALLOWANCE: usize = 3;
const NUDGE_PERCENT: usize = 10;
const NUDGE_MIN: usize = 3;
/// The longest `SAFETY` comment the count passes over; lines past it count as comment.
const SAFETY_MAX: usize = 5;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    Blank,
    Comment,
    Code,
}

pub(super) fn kind(line: &str) -> Kind {
    let t = line.trim();
    if t.is_empty() {
        Kind::Blank
    } else if t.starts_with("//") {
        Kind::Comment
    } else {
        Kind::Code
    }
}

pub(super) struct Line {
    pub(super) file: String,
    pub(super) line: usize,
    pub(super) text: String,
}

#[derive(Default)]
struct Change {
    added: Vec<Line>,
    removed: Vec<Line>,
}

/// Whether `lines[i]` is the line after `lines[i - 1]` in the same file.
fn follows(lines: &[Line], i: usize) -> bool {
    let (a, b) = (&lines[i - 1], &lines[i]);
    a.file == b.file && a.line + 1 == b.line
}

impl Change {
    fn from_diff(diff: &str) -> Change {
        let mut change = Change::default();
        let (mut file, mut at, mut old_at) = (String::new(), 0usize, 0usize);
        for line in diff.lines() {
            if let Some(path) = line.strip_prefix("+++ ") {
                file = path.strip_prefix("b/").unwrap_or(path).to_string();
            } else if line.starts_with("--- ") || line.starts_with("diff ") || line.starts_with("index ") {
            } else if let Some(hunk) = line.strip_prefix("@@ ") {
                let start = |sign: char| {
                    hunk.split_whitespace()
                        .find_map(|w| w.strip_prefix(sign))
                        .and_then(|w| w.split(',').next())
                        .and_then(|n| n.parse().ok())
                        .unwrap_or(0)
                };
                (at, old_at) = (start('+'), start('-'));
            } else if let Some(text) = line.strip_prefix('+') {
                change.added.push(Line { file: file.clone(), line: at, text: text.to_string() });
                at += 1;
            } else if let Some(text) = line.strip_prefix('-') {
                change.removed.push(Line { file: file.clone(), line: old_at, text: text.to_string() });
                old_at += 1;
            }
        }
        change
    }

    fn from_texts(file: &str, first: usize, old: &str, new: &str) -> Change {
        let lines = |text: &str| {
            text.lines()
                .enumerate()
                .map(|(i, t)| Line { file: file.to_string(), line: first + i, text: t.to_string() })
                .collect()
        };
        Change { added: lines(new), removed: lines(old) }
    }
}

#[derive(Default, Debug)]
struct Tally {
    new_comments: usize,
    new_code: usize,
    added_comments: usize,
    removed_comments: usize,
    references: Vec<(String, usize, String)>,
    runs: Vec<(String, usize, usize)>,
}

/// Which of `lines` are a `SAFETY` comment or a doc comment's `# Safety` section, to its
/// [`SAFETY_MAX`]th line: the rules require both, so neither is counted.
pub(super) fn safety_lines(lines: &[Line]) -> Vec<bool> {
    let mut out = vec![false; lines.len()];
    let mut inside = 0usize;
    for (i, line) in lines.iter().enumerate() {
        if kind(&line.text) != Kind::Comment || (i > 0 && !follows(lines, i)) {
            inside = 0;
        }
        if kind(&line.text) == Kind::Comment {
            let t = line.text.trim();
            let doc = t.starts_with("///") || t.starts_with("//!");
            let b = body(t);
            if inside == 0 && (b.starts_with("SAFETY:") || (doc && b == "# Safety")) {
                inside = 1;
            } else if doc && b.starts_with("# ") {
                inside = 0;
            } else if inside > 0 {
                inside += 1;
            }
            out[i] = (1..=SAFETY_MAX).contains(&inside);
        }
    }
    out
}

/// An added line is new unless the same trimmed text was removed elsewhere: a move or a re-indent.
fn tally(change: &Change) -> Tally {
    let mut removed: HashMap<&str, usize> = HashMap::new();
    for r in &change.removed {
        *removed.entry(r.text.trim()).or_default() += 1;
    }
    let removed_safety = safety_lines(&change.removed);
    let added_safety = safety_lines(&change.added);
    let mut t = Tally {
        removed_comments: change
            .removed
            .iter()
            .zip(&removed_safety)
            .filter(|(r, s)| !**s && kind(&r.text) == Kind::Comment)
            .count(),
        ..Tally::default()
    };
    let mut run: Option<(String, usize, usize)> = None;
    let mut prev: Option<&Line> = None;
    for (a, &safety) in change.added.iter().zip(&added_safety) {
        let k = kind(&a.text);
        if k == Kind::Comment && !safety {
            t.added_comments += 1;
        }
        let trimmed = a.text.trim();
        let moved = match removed.get_mut(trimmed) {
            Some(n) if *n > 0 && k != Kind::Blank => {
                *n -= 1;
                true
            }
            _ => false,
        };
        if !moved && k == Kind::Comment {
            let wrapped = || {
                let p = prev.filter(|p| p.file == a.file && p.line + 1 == a.line && kind(&p.text) == Kind::Comment)?;
                let joined = reference(&format!("{} {}", body(&p.text), body(trimmed)));
                joined.filter(|_| reference(&p.text).is_none())
            };
            if let Some(why) = reference(trimmed).or_else(wrapped) {
                t.references.push((a.file.clone(), a.line, why));
            }
        }
        let new_comment = !moved && k == Kind::Comment && !safety;
        if new_comment {
            t.new_comments += 1;
            run = match run {
                Some((f, start, n)) if f == a.file && start + n == a.line => Some((f, start, n + 1)),
                other => {
                    t.runs.extend(other);
                    Some((a.file.clone(), a.line, 1))
                }
            };
        } else if !moved && k == Kind::Code {
            t.new_code += 1;
        }
        prev = Some(a);
    }
    t.runs.extend(run);
    t.runs.sort_by_key(|r| std::cmp::Reverse(r.2));
    t
}

fn over_limit(t: &Tally) -> bool {
    t.added_comments > t.removed_comments && t.new_comments * 100 > LIMIT_PERCENT * t.new_code + ALLOWANCE * 100
}

fn over_nudge(t: &Tally) -> bool {
    t.added_comments > t.removed_comments
        && t.new_comments >= NUDGE_MIN
        && t.new_comments * 100 > NUDGE_PERCENT * t.new_code
}

fn body(comment: &str) -> &str {
    comment.trim().trim_start_matches('/').trim_start_matches('!').trim()
}

/// The reference a comment makes to a PR, an issue, a review finding, a phase, a part, a milestone
/// or a slice, if it makes one.
pub(super) fn reference(comment: &str) -> Option<String> {
    fn clean(w: &str) -> &str {
        let w = w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '#'));
        w.strip_suffix("'s").or_else(|| w.strip_suffix("’s")).unwrap_or(w)
    }
    let number = |w: &str| w.trim_start_matches('#').starts_with(|c: char| c.is_ascii_digit());
    let digits = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit());
    let letter = |c: Option<char>| c.is_some_and(|c| c.is_ascii_uppercase());
    let part = |w: &str| {
        let mut chars = w.chars();
        if !letter(chars.next()) {
            return false;
        }
        let rest = chars.as_str();
        match rest.strip_prefix(['-', '–']) {
            Some(to) => to.chars().count() == 1 && letter(to.chars().next()),
            None => rest.is_empty() || rest.strip_prefix('.').is_some_and(|r| r.split('.').all(digits)),
        }
    };
    let words: Vec<&str> = comment.split_whitespace().map(clean).collect();
    for pair in words.windows(2) {
        let (w, next) = (pair[0], pair[1]);
        let milestone = w.len() > 1 && w.starts_with('M') && w[1..].chars().all(|c| c.is_ascii_digit());
        let hit = match w {
            "PR" | "PRs" | "slice" | "finding" | "issue" | "ticket" | "Milestone" => number(next),
            "Phase" => next.starts_with(|c: char| c.is_ascii_digit()),
            "Part" | "Parts" => part(next),
            _ => milestone && next == "Part",
        };
        if hit {
            return Some(format!("{w} {next}"));
        }
    }
    if let Some(w) = words.iter().find(|w| w.strip_prefix("PR#").is_some_and(number)) {
        return Some(w.to_string());
    }
    let b = comment.as_bytes();
    (0..b.len()).find_map(|i| {
        let digits = b[i..].iter().skip(2).take_while(|c| c.is_ascii_digit()).count();
        let closed = b.get(i + 2 + digits) == Some(&b')');
        (b[i] == b'(' && b.get(i + 1) == Some(&b'#') && (1..=5).contains(&digits) && closed)
            .then(|| comment[i..i + 3 + digits].to_string())
    })
}

pub(crate) fn cmd_check_comments(args: &[String]) -> R<()> {
    match args.first().map(String::as_str) {
        Some("--ratio") => return super::comment_budget::cmd_ratio(&args[1..]),
        Some("--only-comments") => return super::comment_budget::cmd_only_comments(&args[1..]),
        _ => {}
    }
    let base = args
        .iter()
        .position(|a| a == "--base")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "origin/main".to_string());
    let (merge_base, change) = change_since(&crate::repo_root(), &base)?;
    let t = tally(&change);
    let mut failures = Vec::new();
    for (file, line, cited) in &t.references {
        failures.push(format!(
            "{file}:{line}: a comment cites `{cited}`: say why the code is as it is, and keep its history \
             in the commit message and docs/decision-log.md"
        ));
    }
    if over_limit(&t) {
        let longest: Vec<String> = t.runs.iter().take(5).map(|(f, l, n)| format!("{f}:{l} ({n} lines)")).collect();
        failures.push(format!(
            "{} new comment lines to {} new lines of code, over 0.15 of the code plus 3: cut each comment to \
             what the code cannot say — a non-obvious why, an invariant, a SAFETY argument. The longest new \
             comments: {}",
            t.new_comments,
            t.new_code,
            longest.join(", ")
        ));
    }
    if failures.is_empty() {
        println!(
            "check-comments: {} new comment lines to {} new lines of code since {}, none citing a PR or a part ✓",
            t.new_comments,
            t.new_code,
            &merge_base[..merge_base.len().min(8)]
        );
        return Ok(());
    }
    for f in &failures {
        eprintln!("  {f}");
    }
    Err(format!("check-comments: {} failure(s)", failures.len()).into())
}

/// Everything a branch adds to the Rust files under `root` since its merge base with `base`,
/// untracked files whole.
fn change_since(root: &Path, base: &str) -> R<(String, Change)> {
    let git = |args: &[&str]| -> R<String> {
        let out = Command::new("git").args(args).current_dir(root).output()?;
        if !out.status.success() {
            return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr)).into());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let merge_base = git(&["merge-base", base, "HEAD"])?.trim().to_string();
    let diff = git(&["diff", "-U0", "--no-color", "--no-ext-diff", &merge_base, "--", "*.rs"])?;
    let mut change = Change::from_diff(&diff);
    for file in git(&["ls-files", "--others", "--exclude-standard", "--", "*.rs"])?.lines() {
        change.added.extend(Change::from_texts(file, 1, "", &crate::read_text(&root.join(file))?).added);
    }
    Ok((merge_base, change))
}

/// The edit hook: never fails and never blocks; on a comment-heavy or history-citing edit of a Rust
/// file, it prints the nudge for the author.
pub(crate) fn hook_main() -> ExitCode {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_ok()
        && let Some(out) = hook(&input)
    {
        println!("{out}");
    }
    ExitCode::SUCCESS
}

fn hook(input: &str) -> Option<String> {
    let event = Json::parse(input)?;
    let tool = event.get("tool_name")?.str()?;
    let args = event.get("tool_input")?;
    let file = args.get("file_path")?.str()?;
    if !file.ends_with(".rs") {
        return None;
    }
    let original = event.get("tool_response").and_then(|r| r.get("originalFile")).and_then(Json::str);
    let change = match tool {
        "Edit" => {
            let old = args.get("old_string")?.str()?;
            let first = original.and_then(|f| Some(f[..f.find(old)?].matches('\n').count() + 1)).unwrap_or(1);
            Change::from_texts(file, first, old, args.get("new_string")?.str()?)
        }
        "Write" => Change::from_texts(file, 1, original.unwrap_or(""), args.get("content")?.str()?),
        _ => return None,
    };
    let t = tally(&change);
    let mut notes = Vec::new();
    if over_nudge(&t) {
        notes.push(format!(
            "This edit adds {} comment lines to {} lines of code, over 0.1. Cut each comment to what the code \
             cannot say: a non-obvious why, an invariant, a SAFETY argument. `cargo xtask check-comments` fails a \
             branch over 0.15.",
            t.new_comments, t.new_code
        ));
    }
    if let Some((_, line, cited)) = t.references.first() {
        notes.push(format!(
            "Line {line} cites `{cited}`: a PR, review, phase or part belongs in the commit message and the \
             decision log, not the code."
        ));
    }
    if notes.is_empty() {
        return None;
    }
    Some(format!(
        "{{\"hookSpecificOutput\":{{\"hookEventName\":\"PostToolUse\",\"additionalContext\":{}}}}}",
        json_string(&notes.join(" "))
    ))
}

#[derive(Debug, PartialEq)]
enum Json {
    Null,
    Bool(bool),
    Num,
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn parse(s: &str) -> Option<Json> {
        let mut p = JsonParser { s: s.as_bytes(), i: 0 };
        let v = p.value()?;
        p.ws();
        (p.i == p.s.len()).then_some(v)
    }

    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
}

struct JsonParser<'a> {
    s: &'a [u8],
    i: usize,
}

impl JsonParser<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        let hit = self.s.get(self.i) == Some(&c);
        self.i += hit as usize;
        hit
    }

    fn value(&mut self) -> Option<Json> {
        self.ws();
        match *self.s.get(self.i)? {
            b'{' => {
                self.i += 1;
                let mut m = Vec::new();
                if self.eat(b'}') {
                    return Some(Json::Obj(m));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    if !self.eat(b':') {
                        return None;
                    }
                    m.push((k, self.value()?));
                    if self.eat(b'}') {
                        return Some(Json::Obj(m));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut a = Vec::new();
                if self.eat(b']') {
                    return Some(Json::Arr(a));
                }
                loop {
                    a.push(self.value()?);
                    if self.eat(b']') {
                        return Some(Json::Arr(a));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'"' => self.string().map(Json::Str),
            b't' => self.word("true", Json::Bool(true)),
            b'f' => self.word("false", Json::Bool(false)),
            b'n' => self.word("null", Json::Null),
            _ => {
                let start = self.i;
                while self.s.get(self.i).is_some_and(|c| matches!(c, b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                    self.i += 1;
                }
                (self.i > start).then_some(Json::Num)
            }
        }
    }

    fn word(&mut self, w: &str, v: Json) -> Option<Json> {
        self.s[self.i..].starts_with(w.as_bytes()).then(|| {
            self.i += w.len();
            v
        })
    }

    fn hex4(&mut self) -> Option<u32> {
        let h = std::str::from_utf8(self.s.get(self.i..self.i + 4)?).ok()?;
        self.i += 4;
        u32::from_str_radix(h, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        if self.s.get(self.i) != Some(&b'"') {
            return None;
        }
        self.i += 1;
        let mut out = String::new();
        loop {
            match *self.s.get(self.i)? {
                b'"' => {
                    self.i += 1;
                    return Some(out);
                }
                b'\\' => {
                    let e = *self.s.get(self.i + 1)?;
                    self.i += 2;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let c = if (0xD800..0xDC00).contains(&hi) {
                                if self.s.get(self.i..self.i + 2) != Some(b"\\u") {
                                    return None;
                                }
                                self.i += 2;
                                let lo = self.hex4()?;
                                char::from_u32(0x10000 + ((hi - 0xD800) << 10) + lo.checked_sub(0xDC00)?)?
                            } else {
                                char::from_u32(hi)?
                            };
                            out.push(c);
                        }
                        _ => return None,
                    }
                }
                _ => {
                    let start = self.i;
                    while self.s.get(self.i).is_some_and(|&c| c != b'"' && c != b'\\') {
                        self.i += 1;
                    }
                    out.push_str(std::str::from_utf8(&self.s[start..self.i]).ok()?);
                }
            }
        }
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
#[allow(clippy::unnecessary_safety_comment, reason = "the fixtures are SAFETY comments, in strings")]
mod tests {
    use super::*;

    fn change(added: &[&str], removed: &[&str]) -> Change {
        let lines = |texts: &[&str]| {
            texts
                .iter()
                .enumerate()
                .map(|(i, t)| Line { file: "a.rs".into(), line: i + 1, text: t.to_string() })
                .collect()
        };
        Change { added: lines(added), removed: lines(removed) }
    }

    fn at(file: &str, line: usize, text: &str) -> Line {
        Line { file: file.into(), line, text: text.into() }
    }

    fn lines(comments: usize, code: usize) -> Vec<String> {
        (0..comments).map(|i| format!("// note {i}")).chain((0..code).map(|i| format!("let v{i} = {i};"))).collect()
    }

    fn tally_of(comments: usize, code: usize) -> Tally {
        let l = lines(comments, code);
        tally(&change(&l.iter().map(String::as_str).collect::<Vec<_>>(), &[]))
    }

    #[test]
    fn the_limit_is_fifteen_hundredths_of_the_code_and_three() {
        assert!(!over_limit(&tally_of(18, 100)));
        assert!(over_limit(&tally_of(19, 100)));
        assert!(!over_limit(&tally_of(3, 0)));
        assert!(over_limit(&tally_of(4, 0)));
    }

    #[test]
    fn a_change_adding_no_more_comment_lines_than_it_removes_passes() {
        let added = lines(10, 0);
        let added: Vec<&str> = added.iter().map(String::as_str).collect();
        let gone: Vec<String> = (0..10).map(|i| format!("// old {i}")).collect();
        let gone: Vec<&str> = gone.iter().map(String::as_str).collect();
        assert!(!over_limit(&tally(&change(&added, &gone))));
        assert!(over_limit(&tally(&change(&added, &gone[1..]))));
    }

    #[test]
    fn safety_comments_are_neither_comment_nor_code() {
        let t =
            tally(&change(&["// SAFETY: the pointer is live.", "// It came from the caller.", "unsafe { f() };"], &[]));
        assert_eq!((t.new_comments, t.new_code, t.added_comments), (0, 1, 0));
        let t = tally(&change(
            &[
                "/// Frees it.",
                "///",
                "/// # Safety",
                "///",
                "/// It must be live.",
                "/// # Panics",
                "/// Never.",
                "pub unsafe fn f() {}",
            ],
            &[],
        ));
        assert_eq!((t.new_comments, t.new_code), (4, 1), "the doc and its other sections count");
        let after = tally(&change(&["// SAFETY: live.", "unsafe { f() };", "// an essay"], &[]));
        assert_eq!(after.new_comments, 1, "a comment after the code is not the SAFETY comment's");
    }

    #[test]
    fn a_safety_comment_is_passed_over_to_its_fifth_line() {
        let long = |n: usize| {
            let mut l = vec!["// SAFETY: live."];
            l.extend(vec!["// and more."; n - 1]);
            l.push("unsafe { f() };");
            tally(&change(&l, &[])).new_comments
        };
        assert_eq!((long(SAFETY_MAX), long(SAFETY_MAX + 1), long(40)), (0, 1, 40 - SAFETY_MAX));
        let mut stacked = vec!["// SAFETY: one"; 40];
        stacked.push("unsafe { f() };");
        assert_eq!(tally(&change(&stacked, &[])).new_comments, 40 - SAFETY_MAX, "a second mark starts no new comment");
    }

    #[test]
    fn a_safety_comment_ends_at_a_gap_or_another_file() {
        let gap = Change {
            added: vec![at("a.rs", 1, "// SAFETY: live."), at("a.rs", 30, "// an essay"), at("a.rs", 31, "let a = 1;")],
            removed: vec![],
        };
        assert_eq!(tally(&gap).new_comments, 1, "a comment in the next hunk");
        let other =
            Change { added: vec![at("a.rs", 1, "// SAFETY: live."), at("b.rs", 2, "// an essay")], removed: vec![] };
        assert_eq!(tally(&other).new_comments, 1, "a comment opening the next file");
    }

    #[test]
    fn a_removed_safety_comment_ends_at_its_hunk() {
        let mut removed = vec![at("a.rs", 10, "// SAFETY: as it was.")];
        let mut added = vec![at("a.rs", 10, "// SAFETY: as it is.")];
        for i in 0..4 {
            removed.push(at("a.rs", 40 + i, &format!("// old {i}")));
            added.push(at("a.rs", 40 + i, &format!("// new {i}")));
        }
        let t = tally(&Change { added, removed });
        assert_eq!(t.removed_comments, 4);
        assert!(!over_limit(&t), "four comment lines for four");
    }

    #[test]
    fn a_removed_safety_comment_does_not_pay_for_an_essay() {
        let t = tally(&change(
            &["// an essay", "// more of it", "// and more", "// still more", "f();"],
            &["// SAFETY: a", "// b", "// c", "// d"],
        ));
        assert_eq!(t.removed_comments, 0);
        assert!(over_limit(&t));
    }

    #[test]
    fn a_safety_comment_still_may_not_cite_history() {
        let t = tally(&change(&["// SAFETY: as Part H.4 found, live.", "unsafe { f() };"], &[]));
        assert_eq!(t.references.len(), 1);
        let t = tally(&change(&["// SAFETY: as the review of Part", "// H.4 found, live.", "unsafe { f() };"], &[]));
        assert_eq!(t.references, [("a.rs".to_string(), 2, "Part H.4".to_string())], "wrapped");
        let t = tally(&change(
            &["/// # Safety", "///", "/// As Part", "/// H.4 found, live.", "pub unsafe fn f() {}"],
            &[],
        ));
        assert_eq!(t.references, [("a.rs".to_string(), 4, "Part H.4".to_string())], "wrapped, in a doc");
    }

    #[test]
    fn a_moved_or_reindented_line_is_not_new() {
        let t = tally(&change(&["        // kept, deeper", "    let a = 1;"], &["// kept, deeper", "let a = 1;"]));
        assert_eq!((t.new_comments, t.new_code), (0, 0));
        let t = tally(&change(&["// kept, deeper", "// kept, deeper"], &["// kept, deeper"]));
        assert_eq!(t.new_comments, 1, "one removed line answers one added line");
    }

    #[test]
    fn a_line_of_code_holding_comment_marks_is_code() {
        let t =
            tally(&change(&["let s = \"// not a comment\";", "let t = \"/* nor this */\";", "x(); // trailing"], &[]));
        assert_eq!((t.new_comments, t.new_code), (0, 3));
    }

    #[test]
    fn references_to_history_are_found() {
        for (text, cited) in [
            ("// as PR #371 found", "PR #371"),
            ("// PR 12's review", "PR 12"),
            ("// fixed (#123) later", "(#123)"),
            ("/// Phase 6 added it", "Phase 6"),
            ("// since Part H.4", "Part H.4"),
            ("// in Part A, the plan", "Part A"),
            ("// M14 Part B built it", "M14 Part"),
            ("// slice 3 of the plan", "slice 3"),
            ("// the review's finding 2", "finding 2"),
            ("// as planned (Part A).", "Part A"),
            ("// set and read back (administration Part E.3).", "Part E.3"),
            ("// in Part B.2's design", "Part B.2"),
            ("// the refresh's Part K built it", "Part K"),
            ("// Parts A-G built it", "Parts A-G"),
            ("// Parts A–K, all eleven", "Parts A–K"),
            ("// Parts A and B", "Parts A"),
            ("// Milestone 7's greeter", "Milestone 7"),
            ("// PR#12 found it", "PR#12"),
            ("// fixed (#12345) later", "(#12345)"),
        ] {
            assert_eq!(reference(text).as_deref(), Some(cited), "{text}");
        }
    }

    #[test]
    fn near_misses_are_not_references() {
        for text in [
            "// part of the reason",
            "// Partition A holds it",
            "// the colour #121212 (#1e1e2e)",
            "// a slice of bytes",
            "// the phase of the moon",
            "// Part Ten",
            "// PR is the abbreviation",
            "// (#)",
            "// (#123456) is no PR",
            "// phase 3 refuses to drain",
            "// Part AB of the name",
        ] {
            assert_eq!(reference(text), None, "{text}");
        }
    }

    #[test]
    fn a_reference_wrapped_across_two_lines_is_found_on_the_second() {
        let t =
            tally(&change(&["// written when set (administration Part", "// E.5) and read back", "let x = 1;"], &[]));
        assert_eq!(t.references, [("a.rs".to_string(), 2, "Part E.5".to_string())]);
        let two_files = Change {
            added: vec![
                Line { file: "a.rs".into(), line: 1, text: "// see Part".into() },
                Line { file: "b.rs".into(), line: 2, text: "// E.5 in b".into() },
            ],
            removed: vec![],
        };
        assert!(tally(&two_files).references.is_empty());
    }

    #[test]
    fn a_run_of_new_comment_does_not_cross_files() {
        let two_files = Change {
            added: vec![
                Line { file: "a.rs".into(), line: 1, text: "// a".into() },
                Line { file: "b.rs".into(), line: 2, text: "// b".into() },
            ],
            removed: vec![],
        };
        assert_eq!(tally(&two_files).runs.len(), 2);
    }

    #[test]
    fn the_runs_of_new_comment_name_the_longest() {
        let t = tally(&change(&["// a", "// b", "let x = 1;", "// c", "// d", "// e"], &[]));
        assert_eq!(t.runs.first(), Some(&("a.rs".to_string(), 4, 3)));
    }

    #[test]
    fn a_unified_diff_is_read_with_its_new_line_numbers() {
        let diff = "diff --git a/k.rs b/k.rs\n--- a/k.rs\n+++ b/k.rs\n@@ -10,2 +12,3 @@ fn f() {\n-// gone\n+// new\n+let x = 1;\n";
        let c = Change::from_diff(diff);
        assert_eq!(c.removed.iter().map(|r| (r.line, r.text.as_str())).collect::<Vec<_>>(), [(10, "// gone")]);
        assert_eq!(c.added.iter().map(|a| (a.file.as_str(), a.line)).collect::<Vec<_>>(), [("k.rs", 12), ("k.rs", 13)]);
    }

    const EDIT: &str = r#"{"hook_event_name":"PostToolUse","tool_name":"Edit","tool_input":{"file_path":"/r/a.rs","old_string":"    let x = 1;","new_string":"    // One.\n    // Two \"quoted\" \\ é é 😀.\n    // Three.\n    let x = 1;","replace_all":false},"tool_response":{"filePath":"/r/a.rs","originalFile":"fn a() {}\n","structuredPatch":[{"oldStart":1,"lines":["-a"]}],"userModified":false}}"#;

    #[test]
    fn the_hook_reads_an_edit_and_nudges_on_three_comment_lines() {
        let out = hook(EDIT).expect("a nudge");
        assert!(out.starts_with("{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"additionalContext\":"));
        assert!(out.contains("adds 3 comment lines to 0 lines of code"), "{out}");
        assert_eq!(Json::parse(&out).and_then(|j| j.get("hookSpecificOutput").map(|_| ())), Some(()));
    }

    #[test]
    fn the_hook_reads_json_strings_whole() {
        let j = Json::parse(EDIT).unwrap();
        let s = j.get("tool_input").and_then(|t| t.get("new_string")).and_then(Json::str).unwrap();
        assert!(s.contains("\"quoted\" \\ é é 😀."), "{s}");
    }

    #[test]
    fn the_hook_is_silent_for_a_light_edit_or_another_file() {
        assert_eq!(hook(&EDIT.replace("    // Three.\\n", "")), None);
        assert_eq!(hook(&EDIT.replace("/r/a.rs", "/r/a.md")), None);
        assert_eq!(hook("not json"), None);
    }

    #[test]
    fn the_hook_is_silent_when_an_edit_shortens_a_comment() {
        let shrink = r#"{"tool_name":"Edit","tool_input":{"file_path":"/r/a.rs","old_string":"// one\n// two\n// three\n// four\n// five\n// six\nlet x = 1;","new_string":"// uno\n// dos\n// tres\nlet x = 1;"}}"#;
        assert_eq!(hook(shrink), None);
    }

    #[test]
    fn the_hook_names_the_line_in_the_file() {
        let at = r#"{"tool_name":"Edit","tool_input":{"file_path":"/r/a.rs","old_string":"    let y = 2;","new_string":"    // as Part H.4 found\n    let y = 2;"},"tool_response":{"originalFile":"fn a() {\n    let x = 1;\n    let y = 2;\n}\n"}}"#;
        assert!(hook(at).is_some_and(|o| o.contains("Line 3 cites `Part H.4`")), "{:?}", hook(at));
    }

    #[test]
    fn the_json_reader_takes_every_number_and_the_writer_escapes_controls() {
        let j = Json::parse(r#"{"a":-1.5e3,"b":2E-2,"c":+0,"d":[true,false,null],"e":"x"}"#).unwrap();
        assert_eq!(j.get("e").and_then(Json::str), Some("x"));
        assert_eq!(json_string("a\tb\rc\u{1}\"\\"), r#""a\tb\rc\u0001\"\\""#);
        assert_eq!(Json::parse(&json_string("a\tb\rc\u{1}")).as_ref().and_then(Json::str), Some("a\tb\rc\u{1}"));
    }

    #[test]
    fn the_change_is_measured_from_the_repository_root_wherever_it_runs() {
        let root = std::env::temp_dir().join(format!("xtask-comments-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
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
        std::fs::write(root.join("a.rs"), "fn a() {}\n").unwrap();
        git(&["add", "a.rs"]);
        git(&["commit", "-qm", "a"]);
        std::fs::write(root.join("a.rs"), "// tracked, changed\nfn a() {}\n").unwrap();
        std::fs::write(root.join("sub/b.rs"), "// as Part H.4 found\nfn b() {}\n").unwrap();
        let (_, change) = change_since(&root, "HEAD").unwrap();
        let t = tally(&change);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!((t.new_comments, t.new_code), (2, 1));
        assert_eq!(t.references, [("sub/b.rs".to_string(), 1, "Part H.4".to_string())]);
    }

    #[test]
    fn the_hook_compares_a_write_with_the_file_it_replaced() {
        let write = r#"{"tool_name":"Write","tool_input":{"file_path":"/r/b.rs","content":"// one\n// two\n// three\nfn b() {}\n"},"tool_response":{"originalFile":"// one\n// two\n// three\n"}}"#;
        assert_eq!(hook(write), None, "the comments were there before");
        let new = write.replace(r#""originalFile":"// one\n// two\n// three\n""#, r#""originalFile":null"#);
        assert!(hook(&new).is_some());
    }

    #[test]
    fn the_hook_names_a_reference() {
        let one = r#"{"tool_name":"Edit","tool_input":{"file_path":"/r/a.rs","old_string":"","new_string":"// as Part H.4 found\nlet x = 1;\nlet y = 2;"}}"#;
        assert!(hook(one).is_some_and(|o| o.contains("cites `Part H.4`")));
    }
}
