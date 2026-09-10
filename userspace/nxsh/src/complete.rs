//! What a Tab press means for a line of input (§11c).
//!
//! **The baseline the design specified, and no more**: command names across §3's four
//! categories, and file paths. Schema-aware *field* completion — `filter siz<TAB>` → `size`
//! — is the half that needs a pipeline's shape known statically, and is still deferred; §11c
//! says so and this module does not reach for it.
//!
//! ## Why this is a module and not thirty lines in the REPL loop
//!
//! The console loop in `main.rs` is the one part of this shell no host test builds — the
//! deferral that named that (`nxsh-console-tests`) was resolved by booting a real image and
//! typing at it, which is the right answer for byte handling and a very expensive one for a
//! decision table. So the decision table lives here, where it costs a second, and what stays
//! in the loop is "press Tab, write what this says".
//!
//! ## The two questions
//!
//! **Where does the word begin?** Scan back from the cursor over
//! [`is_path_char`](crate::lex::is_path_char) — the lexer's own answer, shared rather than
//! copied, because a completion that disagreed with the lexer about where a word begins
//! would offer to finish something the lexer then reads as two tokens.
//!
//! **Is it a command or a path?** Look at what precedes it. A word at the start of the line,
//! or after `|`, `{`, a newline, or a *pipeline* `(`, is the head of a stage and takes a
//! command name; everything else takes a path. That is what every shell does, and it is
//! decidable from the raw line — which matters, because the line being completed is by
//! definition unfinished and will not parse.
//!
//! The one refinement over "the first word": a `(` that **touches** the name before it opens
//! an argument list (§5b's `f(a, b)`) rather than a pipeline, so what follows it is a value
//! and not a stage. Adjacency is the rule the grammar itself reads there.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The most candidates a single Tab press will list.
///
/// A cap rather than a pager: printing four hundred names into a scrollback is not a listing,
/// and asking "display all N possibilities?" needs a round trip the REPL loop has no shape
/// for. The count is reported, so a too-broad prefix says so rather than looking broken.
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
    /// Byte offset in the line where the word starts. Replacing from here is what a
    /// completion does, so this is the half the caller cannot recompute.
    pub start: usize,
    /// The word as typed. Empty is ordinary — `list <TAB>` asks for everything.
    pub text: &'a str,
    /// What kind of thing would go here.
    pub at: Where,
}

/// The word ending at the end of `line`, and what kind of thing belongs there.
///
/// The end of the line rather than a cursor offset, because this shell's line discipline has
/// no cursor movement: Left and Right are recognised as *ended* escape sequences and not
/// acted on, so the insertion point is always the end. A cursor parameter would be a lie
/// about a capability the terminal half does not have.
pub fn word_at_end(line: &str) -> Word<'_> {
    let b = line.as_bytes();
    let mut start = b.len();
    while start > 0 && crate::lex::is_path_char(b[start - 1]) {
        start -= 1;
    }
    Word { start, text: &line[start..], at: position(b, start) }
}

/// What precedes the word beginning at `start`.
fn position(b: &[u8], start: usize) -> Where {
    let mut i = start;
    while i > 0 && matches!(b[i - 1], b' ' | b'\t' | b'\r') {
        i -= 1;
    }
    if i == 0 {
        return Where::Command;
    }
    match b[i - 1] {
        // **No `;`**: statements here are separated by newlines and nothing else (D2). A
        // semicolon arm would have been a rule for a separator this language does not have,
        // which is the kind of thing that reads as support for it.
        b'|' | b'\n' | b'{' => Where::Command,
        // **A `(` is two different things and adjacency tells them apart.** `(list /bin |
        // count)` opens a pipeline, so a stage follows; `format("{}", x)` opens an argument
        // list, so a value does. The grammar reads adjacency here too — `f(` is a call and
        // `f (` is not written — which is what makes this a rule rather than a guess.
        b'(' if i < 2 || !crate::lex::is_path_char(b[i - 2]) => Where::Command,
        _ => Where::Argument,
    }
}

/// A path-shaped word split into the directory it names and the fragment to match.
///
/// `"/home/do"` → `("/home/", "do")`; `"doc"` → `("", "doc")`; `"/"` → `("/", "")`. The
/// directory keeps its trailing slash so that a candidate is `dir + name` and replacing the
/// whole word with it is correct without the caller rebuilding a separator.
pub fn split_path(word: &str) -> (&str, &str) {
    match word.rfind('/') {
        Some(i) => (&word[..i + 1], &word[i + 1..]),
        None => ("", word),
    }
}

/// The names in `names` that begin with `frag`, each prefixed by `dir` and — for a directory
/// — suffixed with `/`.
///
/// **The trailing slash is what makes a second Tab descend**, and it is also the only signal
/// in a flat list that a name is a place rather than a thing.
pub fn matching_paths(dir: &str, frag: &str, names: &[(String, bool)]) -> Vec<String> {
    let mut out: Vec<String> = names
        .iter()
        .filter(|(n, _)| n.starts_with(frag))
        .map(|(n, is_dir)| {
            let mut s = String::from(dir);
            s.push_str(n);
            if *is_dir {
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
    let mut out: Vec<String> =
        names.filter(|n| n.starts_with(frag)).map(|n| n.to_string()).collect();
    out.sort();
    out.dedup();
    out
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
    /// This is what Tab *inserts* when there is more than one answer: the part they agree on
    /// is unambiguous, so typing it for the person is free, and what is left is exactly the
    /// choice they still have to make. With one candidate it is that candidate.
    ///
    /// Byte-wise, and safe to be: every candidate shares the fragment that produced it, so
    /// the common prefix is at least that long, and a split inside a multi-byte character
    /// would need two candidates agreeing on a leading byte and differing inside the same
    /// character — impossible, since UTF-8 continuation bytes are only reached through a
    /// lead byte that already differed.
    pub fn common_prefix(&self) -> &str {
        let Some(first) = self.candidates.first() else { return "" };
        let mut n = first.len();
        for c in &self.candidates[1..] {
            let mut i = 0;
            let (a, b) = (first.as_bytes(), c.as_bytes());
            while i < n && i < b.len() && a[i] == b[i] {
                i += 1;
            }
            n = i;
        }
        &first[..n]
    }

    /// The line `line` with the completed word replaced by `text`.
    pub fn apply(&self, line: &str, text: &str) -> String {
        let mut out = String::from(&line[..self.start]);
        out.push_str(text);
        out
    }
}

/// The bytes that show `candidates` beneath the line being edited.
///
/// **A decision, not byte plumbing**, which is why it is here and not in the console loop: how
/// many fit on a row, what gets cut, and how the cut is reported are all things a host test can
/// check in a second, and the loop's share is one `write`.
///
/// **Wrapped at 80 columns because that is what a serial console is**, and because nothing
/// tells this shell the width of the terminal it is talking to: the tty protocol carries no
/// size, and the one terminal that knows its own grid (`nxterm`) has no way to say so. A
/// too-narrow guess wraps untidily on a wide terminal; a too-wide one would wrap *mid-name* on
/// a narrow one, which is the failure that matters. When a size op exists, this is the caller
/// that wants it.
///
/// Capped at [`MAX_LISTED`], with the remainder **counted** rather than dropped silently — a
/// prefix that matches four hundred things should say so rather than look like a shell that
/// lost some.
///
/// `\r\n` rather than `\n`: the shell holds the line discipline itself, so nothing downstream
/// adds the carriage return.
pub fn listing(candidates: &[String]) -> Vec<u8> {
    const WIDTH: usize = 80;
    const GAP: usize = 2;

    let shown = candidates.len().min(MAX_LISTED);
    let mut out = Vec::from(&b"\r\n"[..]);
    if shown == 0 {
        return out;
    }
    // One column width for all of them, so the names line up: a grid of ragged rows is harder
    // to read than a slightly wasteful even one.
    let widest = candidates[..shown].iter().map(|c| c.chars().count()).max().unwrap_or(0);
    // At least one per row, or a name wider than the terminal would divide by zero.
    let per_row = (WIDTH / (widest + GAP)).max(1);

    for (i, c) in candidates[..shown].iter().enumerate() {
        out.extend_from_slice(c.as_bytes());
        if (i + 1) % per_row == 0 || i + 1 == shown {
            out.extend_from_slice(b"\r\n");
        } else {
            for _ in 0..(widest + GAP - c.chars().count()) {
                out.push(b' ');
            }
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
            // A quote is not part of a word, so `save "/ho<TAB>` completes the path.
            ("save \"/ho", "/ho"),
            // Nor is a pipe, a comma or an equals sign.
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

    /// A head is a head wherever a stage can start, and nowhere else.
    #[test]
    fn a_stage_head_takes_a_command_and_an_argument_takes_a_path() {
        for line in ["li", "  li", "list /bin | co", "if true { li", "(list", "whoami\nli"] {
            assert_eq!(word_at_end(line).at, Where::Command, "for {line:?}");
        }
        for line in ["list /ho", "cd /sys", "list ", "let x = fo", "format(\"{}\", fo"] {
            assert_eq!(word_at_end(line).at, Where::Argument, "for {line:?}");
        }
    }

    /// **The `(` rule, both directions, because one fix would break the other.**
    ///
    /// A pipeline paren is followed by a stage; a call's argument list is followed by a
    /// value. Adjacency is what separates them, and it is the same signal §5b's grammar
    /// reads — so a test that only checked one shape would let the other silently offer the
    /// wrong vocabulary.
    #[test]
    fn a_paren_opens_a_pipeline_or_an_argument_list_by_adjacency() {
        assert_eq!(word_at_end("(list").at, Where::Command);
        assert_eq!(word_at_end("if (co").at, Where::Command);
        assert_eq!(word_at_end("format(fo").at, Where::Argument);
        assert_eq!(word_at_end("add(1, fo").at, Where::Argument);
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

    /// A prefix shared by *every* candidate, including when one candidate is that prefix.
    ///
    /// The loop takes its first candidate as the initial bound, so a shorter candidate later
    /// in the list has to shrink it — the case a fold that only ever compared pairs would
    /// get right by accident and a hand-written scan can get wrong.
    #[test]
    fn a_candidate_that_is_a_prefix_of_the_others_bounds_it() {
        let c = Completion {
            start: 0,
            candidates: vec![
                String::from("note"),
                String::from("notes"),
                String::from("notebook"),
            ],
        };
        assert_eq!(c.common_prefix(), "note");
    }

    fn listed(candidates: &[&str]) -> String {
        let v: Vec<String> = candidates.iter().map(|c| String::from(*c)).collect();
        String::from_utf8(listing(&v)).expect("the listing is text")
    }

    /// The listing starts on a line of its own and every candidate is in it.
    #[test]
    fn a_listing_puts_each_candidate_on_the_screen() {
        let out = listed(&["copy", "count", "continue"]);
        assert!(out.starts_with("\r\n"), "the listing must leave the line being edited alone");
        for want in ["copy", "count", "continue"] {
            assert!(out.contains(want), "{want} is missing from {out:?}");
        }
        assert!(out.ends_with("\r\n"), "the prompt is written after this and needs a fresh line");
    }

    /// Names line up in columns, and a row holds as many as 80 characters allow.
    #[test]
    fn a_listing_fills_a_row_before_starting_another() {
        // Four characters plus two of gap is six; 80 / 6 is 13.
        let names: Vec<&str> = alloc::vec!["abcd"; 13];
        assert_eq!(listed(&names).matches("\r\n").count(), 2, "13 six-wide names are one row");
        let names: Vec<&str> = alloc::vec!["abcd"; 14];
        assert_eq!(listed(&names).matches("\r\n").count(), 3, "the 14th starts a second row");
    }

    /// A name wider than the terminal gets a row to itself rather than a division by zero.
    #[test]
    fn a_name_wider_than_the_screen_does_not_divide_by_zero() {
        let long = "a".repeat(200);
        let out = listed(&[&long, &long]);
        assert_eq!(out.matches("\r\n").count(), 3, "one row each, plus the leading break");
    }

    /// Too many to show says so, rather than showing some and looking complete.
    #[test]
    fn a_listing_counts_what_it_could_not_show() {
        let names: Vec<String> = (0..MAX_LISTED + 7).map(|i| alloc::format!("n{i}")).collect();
        let out = String::from_utf8(listing(&names)).expect("text");
        assert!(out.contains("... and 7 more"), "the remainder was dropped silently: {out:?}");
        assert!(out.contains("n0"), "the first candidate should still be shown");
        assert!(
            !out.contains(&alloc::format!("n{}", MAX_LISTED)),
            "a candidate past the cap was shown"
        );
    }

    /// Nothing to list is not a crash — `listing` is called with what `complete` found.
    #[test]
    fn an_empty_listing_is_just_a_newline() {
        assert_eq!(listed(&[]), "\r\n");
    }
}
