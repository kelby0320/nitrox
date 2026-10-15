//! `check-comments`, and the edit hook built on it: how much of a change is comment, and whether a
//! comment cites a PR, a review, a phase or a part.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, ExitCode};

type R<T> = Result<T, Box<dyn std::error::Error>>;

const LIMIT_PERCENT: usize = 15;
const ALLOWANCE: usize = 3;
const NUDGE_PERCENT: usize = 10;
const NUDGE_MIN: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Blank,
    Comment,
    Code,
}

fn kind(line: &str) -> Kind {
    let t = line.trim();
    if t.is_empty() {
        Kind::Blank
    } else if t.starts_with("//") {
        Kind::Comment
    } else {
        Kind::Code
    }
}

struct Added {
    file: String,
    line: usize,
    text: String,
}

#[derive(Default)]
struct Change {
    added: Vec<Added>,
    removed: Vec<String>,
}

impl Change {
    fn from_diff(diff: &str) -> Change {
        let mut change = Change::default();
        let (mut file, mut at) = (String::new(), 0usize);
        for line in diff.lines() {
            if let Some(path) = line.strip_prefix("+++ ") {
                file = path.strip_prefix("b/").unwrap_or(path).to_string();
            } else if line.starts_with("--- ") || line.starts_with("diff ") || line.starts_with("index ") {
            } else if let Some(hunk) = line.strip_prefix("@@ ") {
                at = hunk
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix('+'))
                    .and_then(|w| w.split(',').next())
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            } else if let Some(text) = line.strip_prefix('+') {
                change.added.push(Added { file: file.clone(), line: at, text: text.to_string() });
                at += 1;
            } else if let Some(text) = line.strip_prefix('-') {
                change.removed.push(text.to_string());
            }
        }
        change
    }

    fn from_texts(file: &str, old: &str, new: &str) -> Change {
        Change {
            added: new
                .lines()
                .enumerate()
                .map(|(i, t)| Added { file: file.to_string(), line: i + 1, text: t.to_string() })
                .collect(),
            removed: old.lines().map(str::to_string).collect(),
        }
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

/// An added line is new unless the same trimmed text was removed elsewhere: a move or a re-indent.
fn tally(change: &Change) -> Tally {
    let mut removed: HashMap<&str, usize> = HashMap::new();
    for r in &change.removed {
        *removed.entry(r.trim()).or_default() += 1;
    }
    let mut t = Tally {
        removed_comments: change.removed.iter().filter(|r| kind(r) == Kind::Comment).count(),
        ..Tally::default()
    };
    let mut run: Option<(String, usize, usize)> = None;
    for a in &change.added {
        let k = kind(&a.text);
        if k == Kind::Comment {
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
        let new_comment = !moved && k == Kind::Comment;
        if new_comment {
            t.new_comments += 1;
            if let Some(why) = reference(trimmed) {
                t.references.push((a.file.clone(), a.line, why));
            }
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
    }
    t.runs.extend(run);
    t.runs.sort_by(|a, b| b.2.cmp(&a.2));
    t
}

fn over_limit(t: &Tally) -> bool {
    t.added_comments > t.removed_comments && t.new_comments * 100 > LIMIT_PERCENT * t.new_code + ALLOWANCE * 100
}

fn over_nudge(t: &Tally) -> bool {
    t.new_comments >= NUDGE_MIN && t.new_comments * 100 > NUDGE_PERCENT * t.new_code
}

/// The reference a comment makes to a PR, an issue, a review finding, a phase, a part, a milestone's
/// part or a slice, if it makes one.
fn reference(comment: &str) -> Option<String> {
    fn clean(w: &str) -> &str {
        w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '#' || c == '.')).trim_end_matches('.')
    }
    let number = |w: &str| w.trim_start_matches('#').starts_with(|c: char| c.is_ascii_digit());
    let part = |w: &str| {
        let mut c = w.chars();
        c.next().is_some_and(|c| ('A'..='H').contains(&c))
            && c.as_str().split('.').skip(1).all(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
            && (c.as_str().is_empty() || c.as_str().starts_with('.'))
    };
    let words: Vec<&str> = comment.split_whitespace().map(clean).collect();
    for pair in words.windows(2) {
        let (w, next) = (pair[0], pair[1]);
        let milestone = w.len() > 1 && w.starts_with('M') && w[1..].chars().all(|c| c.is_ascii_digit());
        let hit = match w {
            "PR" | "PRs" | "slice" | "finding" | "issue" | "ticket" => number(next),
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
    let base = args
        .iter()
        .position(|a| a == "--base")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "origin/main".to_string());
    let git = |args: &[&str]| -> R<String> {
        let out = Command::new("git").args(args).output()?;
        if !out.status.success() {
            return Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr)).into());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let merge_base = git(&["merge-base", &base, "HEAD"])?.trim().to_string();
    let diff = git(&["diff", "-U0", "--no-color", "--no-ext-diff", &merge_base, "--", "*.rs"])?;
    let mut change = Change::from_diff(&diff);
    for file in git(&["ls-files", "--others", "--exclude-standard", "--", "*.rs"])?.lines() {
        change.added.extend(Change::from_texts(file, "", &std::fs::read_to_string(file)?).added);
    }
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

/// The edit hook: never fails and never blocks; on a comment-heavy or history-citing edit of a Rust
/// file, it prints the nudge for the author.
pub(crate) fn hook_main() -> ExitCode {
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_ok() {
        if let Some(out) = hook(&input) {
            println!("{out}");
        }
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
    let change = match tool {
        "Edit" => Change::from_texts(file, args.get("old_string")?.str()?, args.get("new_string")?.str()?),
        "Write" => {
            let old = event.get("tool_response").and_then(|r| r.get("originalFile")).and_then(Json::str);
            Change::from_texts(file, old.unwrap_or(""), args.get("content")?.str()?)
        }
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
mod tests {
    use super::*;

    fn change(added: &[&str], removed: &[&str]) -> Change {
        Change {
            added: added
                .iter()
                .enumerate()
                .map(|(i, t)| Added { file: "a.rs".into(), line: i + 1, text: t.to_string() })
                .collect(),
            removed: removed.iter().map(|s| s.to_string()).collect(),
        }
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
    fn a_moved_or_reindented_line_is_not_new() {
        let t = tally(&change(&["        // kept, deeper", "    let a = 1;"], &["// kept, deeper", "let a = 1;"]));
        assert_eq!((t.new_comments, t.new_code), (0, 0));
        let t = tally(&change(&["// kept, deeper", "// kept, deeper"], &["// kept, deeper"]));
        assert_eq!(t.new_comments, 1, "one removed line answers one added line");
    }

    #[test]
    fn a_line_of_code_holding_comment_marks_is_code() {
        let t = tally(&change(&["let s = \"// not a comment\";", "let t = \"/* nor this */\";", "x(); // trailing"], &[]));
        assert_eq!((t.new_comments, t.new_code), (0, 3));
    }

    #[test]
    fn references_to_history_are_found() {
        for (text, cited) in [
            ("// as PR #371 found", "PR #371"),
            ("// PR 12's review", "PR 12's"),
            ("// fixed (#123) later", "(#123)"),
            ("/// Phase 6 added it", "Phase 6"),
            ("// since Part H.4", "Part H.4"),
            ("// in Part A, the plan", "Part A"),
            ("// M14 Part B built it", "M14 Part"),
            ("// slice 3 of the plan", "slice 3"),
            ("// the review's finding 2", "finding 2"),
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
        ] {
            assert_eq!(reference(text), None, "{text}");
        }
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
        assert_eq!(c.removed, ["// gone"]);
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
