//! What a Tab press means for a line of input (§11c): command names and file paths. Schema-aware
//! field completion needs a pipeline's shape known statically and is still deferred.
//!
//! The decision table lives here because the console loop in `main.rs` is not host-tested; the
//! loop's share is "press Tab, write what this says".
//!
//! The word begins where [`is_word_char`](crate::lex::is_word_char) stops matching going back: the
//! lexer's word-mode rule, since an argument is lexed in word mode. Not `is_path_char`: `+` fails
//! it, so `cd my+not<TAB>` completed `not` to a path nobody typed.
//!
//! A word at the start of the line, or after `|`, `{`, a newline or a pipeline `(`, takes a command
//! name; everything else takes a path. A `(` that touches the name before it opens an argument list
//! (§5b's `f(a, b)`), so a value follows, not a stage.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The most candidates a single Tab press will list.
///
/// A cap rather than a pager: the REPL loop has no round trip for "display all N possibilities?".
/// The count is reported.
pub const MAX_LISTED: usize = 48;

/// What kind of thing belongs where the cursor is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Where {
    /// The head of a statement or of a pipeline stage: a command name.
    Command,
    /// Anywhere else: a path.
    Argument,
}

/// The word a Tab press acts on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Word<'a> {
    /// Byte offset in the line where the word starts: what a completion replaces from.
    pub start: usize,
    /// The word as typed. Empty is ordinary — `list <TAB>` asks for everything.
    pub text: &'a str,
    /// What kind of thing would go here.
    pub at: Where,
}

/// The word ending at the end of `line`, and what kind of thing belongs there.
///
/// The end of the line rather than a cursor offset: the line discipline has no cursor movement,
/// so the insertion point is always the end.
pub fn word_at_end(line: &str) -> Word<'_> {
    let b = line.as_bytes();
    let mut start = b.len();
    while start > 0 && crate::lex::is_word_char(b[start - 1]) {
        start -= 1;
    }
    Word { start, text: &line[start..], at: position(line, start) }
}

/// What precedes the word beginning at `start`.
fn position(line: &str, start: usize) -> Where {
    let b = line.as_bytes();
    let mut i = start;
    loop {
        while i > 0 && matches!(b[i - 1], b' ' | b'\t' | b'\r') {
            i -= 1;
        }
        // A newline ends a statement only when what precedes it is a finished one. Inside an
        // unclosed construct, or after a trailing `|`, it is a continuation, so look through
        // it; `needs_continuation` lexes properly, so a brace inside a string opens nothing.
        if i > 0 && b[i - 1] == b'\n' && crate::repl::needs_continuation(&line[..i - 1]) != crate::repl::Continue::No {
            i -= 1;
            continue;
        }
        break;
    }
    if i == 0 {
        return Where::Command;
    }
    match b[i - 1] {
        // No `;`: statements are separated by newlines and nothing else.
        b'|' | b'\n' | b'{' => Where::Command,
        // A `(` is a pipeline or an argument list, and adjacency tells them apart: `(list /bin |
        // count)` takes a stage, `format("{}", x)` a value.
        b'(' if i < 2 || !crate::lex::is_word_char(b[i - 2]) => Where::Command,
        _ => Where::Argument,
    }
}

/// A path-shaped word split into the directory it names and the fragment to match.
///
/// `"/home/do"` → `("/home/", "do")`. The directory keeps its trailing slash, so a candidate is
/// `dir + name`.
pub fn split_path(word: &str) -> (&str, &str) {
    match word.rfind('/') {
        Some(i) => (&word[..i + 1], &word[i + 1..]),
        None => ("", word),
    }
}

/// The names in `names` that begin with `frag`, each prefixed by `dir` and, for a directory,
/// suffixed with `/` so a second Tab descends.
///
/// `.` and `..` are considered too, but only for a dotted fragment: they are not directory
/// contents, so a bare `list <TAB>` must not offer them, but `cd ..<TAB>` must not erase the `..`.
pub fn matching_paths(dir: &str, frag: &str, names: &[(String, bool)]) -> Vec<String> {
    /// Both are directories, which is what gives them their trailing slash.
    const DOTS: [(&str, bool); 2] = [(".", true), ("..", true)];

    let dots = DOTS.iter().filter(|_| frag.starts_with('.')).map(|(n, d)| (*n, *d));
    let listed = names.iter().map(|(n, d)| (n.as_str(), *d));
    let mut out: Vec<String> = dots
        .chain(listed)
        .filter(|(n, _)| n.starts_with(frag))
        .map(|(n, is_dir)| {
            let mut s = String::from(dir);
            s.push_str(n);
            if is_dir {
                s.push('/');
            }
            s
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The names that begin with `frag`, sorted and deduplicated.
pub fn matching_names<'a>(frag: &str, names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = names.filter(|n| n.starts_with(frag)).map(|n| n.to_string()).collect();
    out.sort();
    out.dedup();
    out
}

/// What a Tab press should do to the line being edited.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Action {
    /// Leave the line exactly as it is.
    Nothing,
    /// Replace the whole line with this.
    Replace(String),
    /// Show the candidates. The line does not change.
    List,
}

/// What a Tab press found.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Completion {
    /// Where in the line the replaced word begins.
    pub start: usize,
    /// The candidates, sorted and unique. Empty means nothing matched.
    pub candidates: Vec<String>,
}

impl Completion {
    /// Nothing to offer, for a word at `start`.
    pub fn none(start: usize) -> Completion {
        Completion { start, candidates: Vec::new() }
    }

    /// The longest string every candidate begins with.
    ///
    /// This is what Tab inserts when there is more than one answer. Character-wise: a byte-wise
    /// scan can stop inside a character (`é` is `C3 A9`, `è` is `C3 A8`), and slicing there
    /// panics, which in the shipped binary ends the login session.
    pub fn common_prefix(&self) -> &str {
        let Some(first) = self.candidates.first() else { return "" };
        let mut n = first.len();
        for c in &self.candidates[1..] {
            let mut i = 0;
            for (a, b) in first.chars().zip(c.chars()) {
                if a != b || i >= n {
                    break;
                }
                i += a.len_utf8();
            }
            n = n.min(i);
        }
        &first[..n]
    }

    /// The line `line` with the completed word replaced by `text`.
    pub fn apply(&self, line: &str, text: &str) -> String {
        let mut out = String::from(&line[..self.start]);
        out.push_str(text);
        out
    }

    /// What a Tab press should do to `line`.
    ///
    /// Whether to type, show the choice or do nothing is decided here, so a lone candidate equal to
    /// the typed word cannot fall between two arms in the loop.
    pub fn action(&self, line: &str) -> Action {
        let Some(filled) = self.filled(line) else { return Action::Nothing };
        let mut out = filled;
        // A lone candidate ends the word, so it gets a space, even when it adds nothing else.
        if self.candidates.len() == 1 && !out.ends_with('/') {
            out.push(' ');
        }
        if out != line {
            return Action::Replace(out);
        }
        // Nothing they all agree on that is not already typed: with several candidates, the choice
        // is the only useful answer.
        match self.candidates.len() > 1 {
            true => Action::List,
            false => Action::Nothing,
        }
    }

    /// `line` with the word replaced by everything the candidates agree on, or `None` when
    /// there is nothing to replace it with.
    ///
    /// `None` rather than the line with the word deleted: [`common_prefix`] of no candidates is
    /// empty, and applying it blindly erases the word being typed.
    ///
    /// [`common_prefix`]: Self::common_prefix
    pub fn filled(&self, line: &str) -> Option<String> {
        match self.candidates.is_empty() {
            true => None,
            false => Some(self.apply(line, self.common_prefix())),
        }
    }
}

/// The bytes that show `candidates` beneath the line being edited.
///
/// A decision, not byte plumbing: how many fit on a row, what gets cut and how the cut is
/// reported are host-testable, and the loop's share is one `write`.
///
/// Wrapped at 80 columns, since the tty protocol carries no terminal size. Capped at
/// [`MAX_LISTED`] with the remainder counted, not dropped. `\r\n`, not `\n`: the shell holds the
/// line discipline itself, so nothing downstream adds the carriage return.
pub fn listing(candidates: &[String]) -> Vec<u8> {
    const WIDTH: usize = 80;
    const GAP: usize = 2;

    let shown = candidates.len().min(MAX_LISTED);
    let mut out = Vec::from(&b"\r\n"[..]);
    if shown == 0 {
        return out;
    }
    // One column width for all of them, so the names line up.
    let widest = candidates[..shown].iter().map(|c| c.chars().count()).max().unwrap_or(0);
    // At least one per row, or a name wider than the terminal would divide by zero.
    let per_row = (WIDTH / (widest + GAP)).max(1);

    for (i, c) in candidates[..shown].iter().enumerate() {
        out.extend_from_slice(c.as_bytes());
        if (i + 1) % per_row == 0 || i + 1 == shown {
            out.extend_from_slice(b"\r\n");
        } else {
            out.resize(out.len() + (widest + GAP - c.chars().count()), b' ');
        }
    }
    if candidates.len() > shown {
        out.extend_from_slice(b"... and ");
        push_u64(&mut out, (candidates.len() - shown) as u64);
        out.extend_from_slice(b" more\r\n");
    }
    out
}

/// `n` in decimal, appended to `out`.
fn push_u64(out: &mut Vec<u8>, n: u64) {
    if n >= 10 {
        push_u64(out, n / 10);
    }
    out.push(b'0' + (n % 10) as u8);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn names(v: &[(&str, bool)]) -> Vec<(String, bool)> {
        v.iter().map(|(n, d)| (String::from(*n), *d)).collect()
    }

    #[test]
    fn a_word_ends_where_the_lexer_would_end_it() {
        for (line, want) in [
            ("li", "li"),
            ("list /ho", "/ho"),
            ("list ", ""),
            ("", ""),
            ("list /home/a.txt", "/home/a.txt"),
            ("save \"/ho", "/ho"),
            ("list /bin | co", "co"),
            ("f(a, b", "b"),
            ("let x = fo", "fo"),
        ] {
            assert_eq!(word_at_end(line).text, want, "for {line:?}");
        }
    }

    #[test]
    fn the_start_offset_is_where_the_word_begins() {
        let w = word_at_end("list /ho");
        assert_eq!(w.start, 5);
        assert_eq!(&"list /ho"[w.start..], "/ho");
        assert_eq!(word_at_end("list ").start, 5, "an empty word starts at the cursor");
    }

    #[test]
    fn a_stage_head_takes_a_command_and_an_argument_takes_a_path() {
        for line in ["li", "  li", "list /bin | co", "if true { li", "(list", "whoami\nli"] {
            assert_eq!(word_at_end(line).at, Where::Command, "for {line:?}");
        }
        for line in ["list /ho", "cd /sys", "list ", "let x = fo", "format(\"{}\", fo"] {
            assert_eq!(word_at_end(line).at, Where::Argument, "for {line:?}");
        }
    }

    /// The `(` rule in both directions: a test of one shape would let the other offer the wrong
    /// vocabulary.
    #[test]
    fn a_paren_opens_a_pipeline_or_an_argument_list_by_adjacency() {
        assert_eq!(word_at_end("(list").at, Where::Command);
        assert_eq!(word_at_end("if (co").at, Where::Command);
        assert_eq!(word_at_end("format(fo").at, Where::Argument);
        assert_eq!(word_at_end("add(1, fo").at, Where::Argument);
    }

    /// A newline is a statement boundary or a continuation, and only the language knows.
    #[test]
    fn a_newline_inside_an_unclosed_construct_is_not_a_new_statement() {
        assert_eq!(word_at_end("whoami\nli").at, Where::Command);
        assert_eq!(word_at_end("list /bin\ncd /ho").at, Where::Argument);
        // Unclosed: the newline is a continuation and the argument list is still open.
        assert_eq!(word_at_end("format(\"{}\",\nDoc").at, Where::Argument);
        assert_eq!(word_at_end("f(\n  a,\n  b").at, Where::Argument);
        assert_eq!(word_at_end("list /bin |\nco").at, Where::Command);
        assert_eq!(word_at_end("if true {\nli").at, Where::Command);
        // A bracket inside a string opens nothing — the thing a character count gets wrong.
        assert_eq!(word_at_end("display \"(\"\nli").at, Where::Command);
    }

    #[test]
    fn a_path_splits_at_its_last_slash() {
        assert_eq!(split_path("/home/do"), ("/home/", "do"));
        assert_eq!(split_path("doc"), ("", "doc"));
        assert_eq!(split_path("/"), ("/", ""));
        assert_eq!(split_path(""), ("", ""));
        assert_eq!(split_path("./sc"), ("./", "sc"));
    }

    #[test]
    fn a_candidate_carries_the_directory_it_was_found_in() {
        let entries = names(&[("Documents", true), ("Downloads", true), ("notes.txt", false)]);
        assert_eq!(
            matching_paths("/home/", "D", &entries),
            vec![String::from("/home/Documents/"), String::from("/home/Downloads/")],
            "a candidate must be substitutable for the whole word, directory and all"
        );
        assert_eq!(
            matching_paths("", "n", &entries),
            vec![String::from("notes.txt")],
            "a file gets no trailing slash — only a place you can go into does"
        );
    }

    /// `.` and `..` complete: a fragment starting with a dot is somebody spelling one.
    #[test]
    fn the_parent_and_the_current_directory_complete() {
        let entries = names(&[("Documents", true), ("notes.txt", false)]);
        assert_eq!(matching_paths("", "..", &entries), vec![String::from("../")]);
        assert_eq!(
            matching_paths("", ".", &entries),
            vec![String::from("../"), String::from("./")],
            "a lone dot is the start of both"
        );
        assert_eq!(matching_paths("/home/", "..", &entries), vec![String::from("/home/../")]);
    }

    #[test]
    fn a_bare_tab_does_not_offer_dot_entries() {
        let entries = names(&[("Documents", true), ("notes.txt", false)]);
        assert_eq!(matching_paths("", "", &entries), vec![String::from("Documents/"), String::from("notes.txt")],);
        assert_eq!(matching_paths("", "notes.", &entries), vec![String::from("notes.txt")]);
    }

    #[test]
    fn a_dotted_fragment_matches_hidden_names_too() {
        let entries = names(&[(".config", true), ("notes.txt", false)]);
        assert_eq!(
            matching_paths("", ".", &entries),
            vec![String::from("../"), String::from("./"), String::from(".config/")],
        );
    }

    /// Every arm of a Tab press, including the lone candidate already typed, which once did
    /// nothing.
    #[test]
    fn every_outcome_of_a_tab_press() {
        let start = 5;
        let c = |names: &[&str]| Completion { start, candidates: names.iter().map(|n| String::from(*n)).collect() };

        assert_eq!(c(&[]).action("list zz"), Action::Nothing);

        assert_eq!(c(&["notes.txt"]).action("list not"), Action::Replace(String::from("list notes.txt ")));
        assert_eq!(c(&["Documents/"]).action("list Doc"), Action::Replace(String::from("list Documents/")));
        // **One match, already fully typed**: still the space, and this is what fell through.
        assert_eq!(c(&["notes.txt"]).action("list notes.txt"), Action::Replace(String::from("list notes.txt ")));

        assert_eq!(c(&["Documents/", "Downloads/"]).action("list D"), Action::Replace(String::from("list Do")));
        assert_eq!(c(&["Documents/", "Downloads/"]).action("list Do"), Action::List);
        assert_eq!(c(&["a/x", "a/y"]).action("list a/"), Action::List);
    }

    #[test]
    fn a_completion_that_found_nothing_changes_nothing() {
        assert_eq!(Completion::none(3).filled("cd zz"), None);
        let one = Completion { start: 3, candidates: vec![String::from("zzz")] };
        assert_eq!(one.filled("cd zz").as_deref(), Some("cd zzz"));
    }

    #[test]
    fn the_common_prefix_is_what_tab_can_type_for_you() {
        let c = Completion {
            start: 5,
            candidates: vec![String::from("/home/Documents/"), String::from("/home/Downloads/")],
        };
        assert_eq!(c.common_prefix(), "/home/Do");
        assert_eq!(c.apply("list /home/D", c.common_prefix()), "list /home/Do");

        let one = Completion { start: 5, candidates: vec![String::from("/home/notes.txt")] };
        assert_eq!(one.common_prefix(), "/home/notes.txt");
        assert_eq!(one.apply("list /home/n", one.common_prefix()), "list /home/notes.txt");

        assert_eq!(Completion::none(0).common_prefix(), "");
    }

    /// Two names that differ inside a character still share a whole-character prefix.
    #[test]
    fn a_prefix_never_splits_a_character() {
        for (a, b, want) in [
            ("café", "cafè", "caf"),
            ("日本", "文字", ""),
            ("données", "donné", "donné"),
            ("/home/café.txt", "/home/cafè.txt", "/home/caf"),
        ] {
            let c = Completion { start: 0, candidates: vec![String::from(a), String::from(b)] };
            assert_eq!(c.common_prefix(), want, "for {a:?} and {b:?}");
        }
    }

    #[test]
    fn a_candidate_that_is_a_prefix_of_the_others_bounds_it() {
        let c = Completion {
            start: 0,
            candidates: vec![String::from("note"), String::from("notes"), String::from("notebook")],
        };
        assert_eq!(c.common_prefix(), "note");
    }

    fn listed(candidates: &[&str]) -> String {
        let v: Vec<String> = candidates.iter().map(|c| String::from(*c)).collect();
        String::from_utf8(listing(&v)).expect("the listing is text")
    }

    #[test]
    fn a_listing_puts_each_candidate_on_the_screen() {
        let out = listed(&["copy", "count", "continue"]);
        assert!(out.starts_with("\r\n"), "the listing must leave the line being edited alone");
        for want in ["copy", "count", "continue"] {
            assert!(out.contains(want), "{want} is missing from {out:?}");
        }
        assert!(out.ends_with("\r\n"), "the prompt is written after this and needs a fresh line");
    }

    #[test]
    fn a_listing_fills_a_row_before_starting_another() {
        let names: Vec<&str> = alloc::vec!["abcd"; 13];
        assert_eq!(listed(&names).matches("\r\n").count(), 2, "13 six-wide names are one row");
        let names: Vec<&str> = alloc::vec!["abcd"; 14];
        assert_eq!(listed(&names).matches("\r\n").count(), 3, "the 14th starts a second row");
    }

    #[test]
    fn a_name_wider_than_the_screen_does_not_divide_by_zero() {
        let long = "a".repeat(200);
        let out = listed(&[&long, &long]);
        assert_eq!(out.matches("\r\n").count(), 3, "one row each, plus the leading break");
    }

    #[test]
    fn a_listing_counts_what_it_could_not_show() {
        let names: Vec<String> = (0..MAX_LISTED + 7).map(|i| alloc::format!("n{i}")).collect();
        let out = String::from_utf8(listing(&names)).expect("text");
        assert!(out.contains("... and 7 more"), "the remainder was dropped silently: {out:?}");
        assert!(out.contains("n0"), "the first candidate should still be shown");
        assert!(!out.contains(&alloc::format!("n{}", MAX_LISTED)), "a candidate past the cap was shown");
    }

    #[test]
    fn an_empty_listing_is_just_a_newline() {
        assert_eq!(listed(&[]), "\r\n");
    }
}
