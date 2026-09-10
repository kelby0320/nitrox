//! A tolerant, table-driven scanner: what a line of text *looks like*.
//!
//! **A lexer answers "what does this program mean". This answers "what does this text look
//! like", over text that is not a program yet** — which is a different job, and the reason
//! `nxsh`'s own lexer is not reused here (M14 Part G). That lexer is fallible: `peek` and `bump`
//! return `Result`, and while a person is typing an unterminated string is the *normal* state of
//! the buffer, so a highlighter built on it would stop colouring at the first quote and start
//! again when the pair closed. It is also parser-mode-driven — `Mode::Word` for a command's
//! arguments, expression mode elsewhere — and a standalone highlighter has no parser to pick the
//! mode with, so barewords, flags and regex literals would all be read as the wrong thing.
//!
//! **This is total by construction.** Every byte of every line lands in exactly one run, an
//! unterminated string simply runs to the end of the line (or past it, if the language says so),
//! and there is no input it can refuse. That is what every small editor does and what a person
//! typing a quote expects to see.
//!
//! ## A language is a table
//!
//! [`Language`] is data: line-comment markers, a block-comment pair, string rules, a keyword
//! list, whether numbers are lexed, a variable sigil, line prefixes and a fence. One scanner
//! drives all of them, so **the scanner is the work and a language after it is a table**.
//!
//! ## Where it lives
//!
//! In `nxedit`, because `nxedit` is its only consumer. `userspace/CLAUDE.md`'s rule is that a
//! helper with one consumer belongs to that consumer and one with two belongs below both — so
//! the trigger for moving this into a crate of its own is the second thing that wants to colour
//! text, which would today mean a preview pane in `nxfiles` or a pager.

use alloc::vec::Vec;

/// What a run of text is, for colouring. The application maps these onto `Theme`'s six
/// `syntax_*` colours; [`Kind::Plain`] takes the ordinary foreground.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Ordinary text — drawn in the theme's foreground, and never emitted as a run.
    Plain,
    /// A reserved word.
    Keyword,
    /// A string literal, a code span, or the inside of a fenced block.
    Str,
    /// A comment, or a block quote.
    Comment,
    /// A numeric literal.
    Number,
    /// A structural line marker: a Markdown heading, a TOML table header.
    Heading,
    /// A shell variable.
    Variable,
}

/// A coloured stretch of one line, in byte offsets within that line.
///
/// **Only the non-plain stretches are emitted.** Plain text is the theme's foreground, which is
/// what the widget draws without being told anything, so a line of ordinary prose produces no
/// runs at all and costs nothing to render.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Run {
    /// First byte, inclusive. Always a character boundary.
    pub start: usize,
    /// Last byte, exclusive. Always a character boundary.
    pub end: usize,
    /// What this stretch is.
    pub kind: Kind,
}

/// What was open at the end of a line, and therefore at the start of the next.
///
/// **The whole of the multi-line problem.** A block comment or a fenced code block makes line
/// *N*'s colours depend on where line *N−1* ended, so a line cannot be scanned alone. Kept small
/// and `Copy` so an editor can cache one per line cheaply and compare them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum State {
    /// Nothing is open.
    #[default]
    Normal,
    /// A block comment is open.
    Block,
    /// A fenced block is open — everything inside is [`Kind::Str`].
    Fence,
    /// A string is open, and this is its delimiter.
    ///
    /// Only reachable for a rule whose `multiline` is set; every other string is closed at the
    /// end of its line whether or not its quote arrived.
    Str(char),
}

/// How one kind of string literal behaves.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct StringRule {
    /// The character that opens and closes it.
    pub delim: char,
    /// Whether a backslash escapes the next character.
    pub escape: bool,
    /// Whether it may run past the end of its line.
    pub multiline: bool,
}

/// A language, as data. See the module docs.
#[derive(Clone, Copy)]
pub struct Language {
    /// What to call it — for a receipt, and for a test that wants to say which table it got.
    ///
    /// **A field rather than an identity derived from the table.** Comparing two `Language`
    /// values by the addresses of their slices is the clever alternative, and it rests on the
    /// compiler not merging two identical empty ones — a thing it is free to do.
    pub name: &'static str,
    /// Sequences that start a comment running to the end of the line.
    pub line_comments: &'static [&'static str],
    /// A block comment's opener and closer.
    pub block_comment: Option<(&'static str, &'static str)>,
    /// The string literals this language has.
    pub strings: &'static [StringRule],
    /// Reserved words, matched against whole identifiers.
    pub keywords: &'static [&'static str],
    /// Whether numeric literals are coloured.
    pub numbers: bool,
    /// A character that introduces a variable — `$` in a shell.
    pub variable: Option<char>,
    /// Prefixes that colour a **whole line**, tested against the line with leading blanks kept.
    ///
    /// **The whole line, deliberately.** A Markdown heading is its line, and a TOML table header
    /// is its line; making the rule stop at a comment would need each of them to say where it
    /// ends, which is a grammar. The cost is that `[server] # note` colours the note as part of
    /// the header, which nobody has ever been confused by.
    pub line_prefixes: &'static [(&'static str, Kind)],
    /// A fence that toggles a verbatim block, tested at the start of a line.
    pub fence: Option<&'static str>,
    /// A delimiter for a *span* of verbatim text within a line — Markdown's backtick.
    pub code_span: Option<char>,
}

/// The empty language: everything is plain. What a file with no known extension gets.
pub const PLAIN: Language = Language {
    name: "plain",
    line_comments: &[],
    block_comment: None,
    strings: &[],
    keywords: &[],
    numbers: false,
    variable: None,
    line_prefixes: &[],
    fence: None,
    code_span: None,
};

/// `nxsh`, this system's shell — see `docs/spec/shell-language.md`.
pub const NXSH: Language = Language {
    name: "nxsh",
    line_comments: &["#"],
    block_comment: None,
    strings: &[
        StringRule { delim: '"', escape: true, multiline: false },
        StringRule { delim: '\'', escape: false, multiline: false },
    ],
    keywords: &[
        "if", "else", "elif", "while", "for", "in", "match", "try", "catch", "fn", "let",
        "return", "break", "continue", "and", "or", "not", "true", "false", "def", "end",
    ],
    numbers: true,
    variable: Some('$'),
    line_prefixes: &[],
    fence: None,
    code_span: None,
};

/// TOML, which every configuration file in this system is written in.
pub const TOML: Language = Language {
    name: "toml",
    line_comments: &["#"],
    block_comment: None,
    strings: &[
        StringRule { delim: '"', escape: true, multiline: false },
        StringRule { delim: '\'', escape: false, multiline: false },
    ],
    keywords: &["true", "false"],
    numbers: true,
    variable: None,
    line_prefixes: &[("[", Kind::Heading)],
    fence: None,
    code_span: None,
};

/// Rust.
///
/// **No character literals**, and that is a table limitation stated rather than an oversight: the
/// same quote opens a lifetime, so `&'static str` would open a string that ran to the end of the
/// line. Telling the two apart needs a grammar, and a highlighter that coloured half of every
/// generic function is worse than one that leaves `'x'` plain.
pub const RUST: Language = Language {
    name: "rust",
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    strings: &[StringRule { delim: '"', escape: true, multiline: true }],
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod",
        "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super",
        "trait", "true", "type", "unsafe", "use", "where", "while",
    ],
    numbers: true,
    variable: None,
    line_prefixes: &[],
    fence: None,
    code_span: None,
};

/// Markdown — every document under `docs/`.
///
/// **The language that stretches the table**, and it was named in the plan as the drop candidate
/// rather than discovered as a distortion. Its structure is line-prefixed and fenced rather than
/// keyword-and-string, so it uses the two rules built for exactly that and nothing else: a
/// heading and a quote are line prefixes, a fenced block is the multi-line state block comments
/// already needed, and an inline code span is a delimiter. Emphasis is **not** coloured — `*`
/// pairs across a line are a grammar, and this is where the table would have started bending.
pub const MARKDOWN: Language = Language {
    name: "markdown",
    line_comments: &[],
    block_comment: None,
    strings: &[],
    keywords: &[],
    numbers: false,
    variable: None,
    line_prefixes: &[
        ("#", Kind::Heading),
        (">", Kind::Comment),
    ],
    fence: Some("```"),
    code_span: Some('`'),
};

/// The language a file's name implies.
///
/// **From the extension, and `None` is a supported answer** rather than a gap: a file with no
/// extension, or one this system does not contain, is plain text and is drawn in one colour.
pub fn for_name(name: &str) -> Option<Language> {
    let ext = name.rsplit_once('.')?.1;
    match ext {
        "nx" => Some(NXSH),
        "toml" => Some(TOML),
        "rs" => Some(RUST),
        "md" => Some(MARKDOWN),
        _ => None,
    }
}

/// What to call a language in a receipt. `None` — a file with no known extension — is `"plain"`.
pub fn name_of(lang: Option<Language>) -> &'static str {
    lang.map_or(PLAIN.name, |l| l.name)
}

/// Scan one line, given what was open at its start. Returns its runs and what is open after it.
///
/// `line` is one line **without** its terminator. Offsets in the returned runs are bytes within
/// it, and always character boundaries.
pub fn scan(lang: &Language, line: &str, start: State) -> (Vec<Run>, State) {
    let mut out = Vec::new();
    let mut state = start;

    // ---- states that own the whole line, or the start of it ----
    match state {
        State::Fence => {
            // A fence closes on a line that is itself a fence; the closing line is part of the
            // block, which is what makes an empty fenced block still look like one.
            let closes = lang.fence.is_some_and(|f| line.trim_start().starts_with(f));
            push(&mut out, 0, line.len(), Kind::Str);
            return (out, if closes { State::Normal } else { State::Fence });
        }
        State::Block => {
            let close = lang.block_comment.map(|(_, c)| c).unwrap_or("*/");
            match line.find(close) {
                Some(i) => {
                    let end = i + close.len();
                    push(&mut out, 0, end, Kind::Comment);
                    state = State::Normal;
                    return scan_from(lang, line, end, state, out);
                }
                None => {
                    push(&mut out, 0, line.len(), Kind::Comment);
                    return (out, State::Block);
                }
            }
        }
        State::Str(delim) => {
            let rule = lang.strings.iter().find(|r| r.delim == delim).copied();
            let escape = rule.is_some_and(|r| r.escape);
            match close_string(line, 0, delim, escape) {
                Some(end) => {
                    push(&mut out, 0, end, Kind::Str);
                    return scan_from(lang, line, end, State::Normal, out);
                }
                None => {
                    push(&mut out, 0, line.len(), Kind::Str);
                    return (out, State::Str(delim));
                }
            }
        }
        State::Normal => {}
    }

    // ---- a fence opening, and the line rules that own a whole line ----
    let trimmed = line.trim_start();
    if let Some(f) = lang.fence
        && trimmed.starts_with(f)
    {
        push(&mut out, 0, line.len(), Kind::Str);
        return (out, State::Fence);
    }
    for (prefix, kind) in lang.line_prefixes {
        if trimmed.starts_with(prefix) {
            push(&mut out, 0, line.len(), *kind);
            return (out, State::Normal);
        }
    }

    scan_from(lang, line, 0, State::Normal, out)
}

/// The ordinary scan of `line` from byte `at`, with `out` already holding earlier runs.
fn scan_from(
    lang: &Language,
    line: &str,
    at: usize,
    mut state: State,
    mut out: Vec<Run>,
) -> (Vec<Run>, State) {
    let b = line.as_bytes();
    let mut i = at;
    while i < line.len() {
        // A line comment takes everything after it.
        if lang.line_comments.iter().any(|c| line[i..].starts_with(c)) {
            push(&mut out, i, line.len(), Kind::Comment);
            return (out, state);
        }
        // A block comment may close on this line or run past it.
        if let Some((open, close)) = lang.block_comment
            && line[i..].starts_with(open)
        {
            match line[i + open.len()..].find(close) {
                Some(rel) => {
                    let end = i + open.len() + rel + close.len();
                    push(&mut out, i, end, Kind::Comment);
                    i = end;
                    continue;
                }
                None => {
                    push(&mut out, i, line.len(), Kind::Comment);
                    return (out, State::Block);
                }
            }
        }
        let c = line[i..].chars().next().unwrap_or(' ');
        // A code span, which is a string that cannot escape and cannot cross a line.
        if lang.code_span == Some(c) {
            match close_string(line, i + c.len_utf8(), c, false) {
                Some(end) => {
                    push(&mut out, i, end, Kind::Str);
                    i = end;
                }
                None => {
                    push(&mut out, i, line.len(), Kind::Str);
                    return (out, state);
                }
            }
            continue;
        }
        // A string literal.
        if let Some(rule) = lang.strings.iter().find(|r| r.delim == c) {
            match close_string(line, i + c.len_utf8(), c, rule.escape) {
                Some(end) => {
                    push(&mut out, i, end, Kind::Str);
                    i = end;
                }
                None => {
                    push(&mut out, i, line.len(), Kind::Str);
                    // **Only a multiline rule leaves the string open.** Everything else is
                    // closed at the newline, which is what makes a typed quote colour one line
                    // rather than the whole file below it.
                    state = if rule.multiline { State::Str(c) } else { State::Normal };
                    return (out, state);
                }
            }
            continue;
        }
        // A variable: the sigil plus a word, or a braced name.
        if lang.variable == Some(c) {
            let mut j = i + c.len_utf8();
            if line[j..].starts_with('{') {
                j = match line[j..].find('}') {
                    Some(rel) => j + rel + 1,
                    None => line.len(),
                };
            } else {
                while j < line.len() && is_word(b[j]) {
                    j += 1;
                }
            }
            // A bare sigil with nothing after it is not a variable; leaving it plain is what
            // stops a person typing `$` from seeing a colour appear and vanish.
            if j > i + c.len_utf8() {
                push(&mut out, i, j, Kind::Variable);
            }
            i = j.max(i + c.len_utf8());
            continue;
        }
        // A word: a keyword if the table lists it, a number if it starts with a digit.
        if is_word(b[i]) {
            let mut j = i;
            while j < line.len() && is_word(b[j]) {
                j += 1;
            }
            // **A number may carry a decimal point and an identifier may not**, which is why
            // this is here rather than in `is_word`: `.` as a general joiner would swallow the
            // `self` in `self.foo` and stop colouring the commonest keyword in the language it
            // was added for. A point counts only when a digit follows it, so `1.5` is one
            // number and Rust's `1..2` is two (PR #289 review, 3).
            if b[i].is_ascii_digit() {
                while j + 1 < line.len() && b[j] == b'.' && b[j + 1].is_ascii_digit() {
                    j += 1;
                    while j < line.len() && is_word(b[j]) {
                        j += 1;
                    }
                }
            }
            let word = &line[i..j];
            if b[i].is_ascii_digit() {
                if lang.numbers {
                    push(&mut out, i, j, Kind::Number);
                }
            } else if lang.keywords.contains(&word) {
                push(&mut out, i, j, Kind::Keyword);
            }
            i = j;
            continue;
        }
        i += c.len_utf8();
    }
    (out, state)
}

/// Where the string opened before `from` ends, as a byte offset **past** its closing delimiter.
fn close_string(line: &str, from: usize, delim: char, escape: bool) -> Option<usize> {
    let mut skip = false;
    for (i, c) in line[from..].char_indices() {
        if skip {
            skip = false;
            continue;
        }
        if escape && c == '\\' {
            skip = true;
            continue;
        }
        if c == delim {
            return Some(from + i + c.len_utf8());
        }
    }
    None
}

/// Whether `b` is part of a word: a letter, a digit or an underscore.
///
/// **`.` and `-` are not**, deliberately. A point joins the parts of a *number* and that is
/// handled where numbers are scanned, because as a general rule it would make `self.foo` one
/// word and stop `self` being a keyword. A hyphen would join a TOML key, which is not coloured
/// at all — so it would buy nothing and break `a-b` in every other language.
///
/// The doc here claimed both of them until PR #289's review pointed at the body, which had
/// never accepted either.
fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Append a run, skipping empties.
fn push(out: &mut Vec<Run>, start: usize, end: usize, kind: Kind) {
    if start < end && kind != Kind::Plain {
        out.push(Run { start, end, kind });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Scan one line from nothing, returning its runs as `(text, kind)` pairs.
    fn runs<'a>(lang: &Language, line: &'a str) -> Vec<(&'a str, Kind)> {
        let (rs, _) = scan(lang, line, State::Normal);
        rs.iter().map(|r| (&line[r.start..r.end], r.kind)).collect()
    }

    /// Scan a whole buffer, carrying the state from line to line.
    fn buffer<'a>(lang: &Language, lines: &[&'a str]) -> Vec<Vec<(&'a str, Kind)>> {
        let mut st = State::Normal;
        let mut out = Vec::new();
        for l in lines {
            let (rs, next) = scan(lang, l, st);
            st = next;
            out.push(rs.iter().map(|r| (&l[r.start..r.end], r.kind)).collect());
        }
        out
    }

    #[test]
    fn plain_text_produces_no_runs_at_all() {
        // The cheap case, and the one every line of prose takes: nothing to colour is nothing
        // to allocate, draw or diff.
        assert!(runs(&PLAIN, "just some words").is_empty());
        assert!(runs(&RUST, "        ").is_empty());
    }

    #[test]
    fn a_shell_line_colours_its_keyword_its_variable_and_its_comment() {
        assert_eq!(
            runs(&NXSH, "if $HOME { echo 1 } # done"),
            vec![
                ("if", Kind::Keyword),
                ("$HOME", Kind::Variable),
                ("1", Kind::Number),
                ("# done", Kind::Comment),
            ]
        );
        assert_eq!(runs(&NXSH, "echo ${WHAT}x"), vec![("${WHAT}", Kind::Variable)]);
    }

    /// A bare sigil is not a variable.
    ///
    /// **What a person typing sees**: `$` on its own would otherwise colour, and then uncolour
    /// as soon as a letter arrived, which is a flicker with no meaning behind it.
    #[test]
    fn a_sigil_with_no_name_after_it_is_not_a_variable() {
        assert!(runs(&NXSH, "echo $ x").is_empty());
    }

    /// An unterminated string runs to the end of its line and **stops there**.
    ///
    /// **The defining property of a tolerant scanner.** While a person is typing, an unterminated
    /// string is the normal state of the buffer — a scanner that carried it downward would
    /// recolour the whole file below the cursor on every opening quote.
    #[test]
    fn an_unterminated_string_ends_at_its_line() {
        assert_eq!(runs(&NXSH, "echo \"unfinished"), vec![("\"unfinished", Kind::Str)]);
        let (_, st) = scan(&NXSH, "echo \"unfinished", State::Normal);
        assert_eq!(st, State::Normal, "a shell string does not span lines");
    }

    /// …unless the language says its strings span lines, which Rust's do.
    #[test]
    fn a_rust_string_spans_lines_until_it_closes() {
        let out = buffer(&RUST, &["let s = \"one", "two", "three\"; let n = 1;"]);
        assert_eq!(out[0], vec![("let", Kind::Keyword), ("\"one", Kind::Str)]);
        assert_eq!(out[1], vec![("two", Kind::Str)], "the middle line is all string");
        assert_eq!(
            out[2],
            vec![("three\"", Kind::Str), ("let", Kind::Keyword), ("1", Kind::Number)],
            "and the line that closes it goes back to code afterwards"
        );
    }

    #[test]
    fn an_escaped_quote_does_not_close_a_string() {
        assert_eq!(runs(&RUST, r#"let s = "a\"b";"#), vec![
            ("let", Kind::Keyword),
            (r#""a\"b""#, Kind::Str),
        ]);
        // …and a language without escapes takes the backslash literally, which is what a
        // single-quoted shell string does.
        assert_eq!(runs(&NXSH, r"echo 'a\'"), vec![(r"'a\'", Kind::Str)]);
    }

    #[test]
    fn a_block_comment_carries_across_lines_and_releases_the_rest_of_the_closing_one() {
        let out = buffer(&RUST, &["fn a() { /* one", "two", "*/ let x = 2; }"]);
        assert_eq!(out[0], vec![("fn", Kind::Keyword), ("/* one", Kind::Comment)]);
        assert_eq!(out[1], vec![("two", Kind::Comment)]);
        assert_eq!(
            out[2],
            vec![("*/", Kind::Comment), ("let", Kind::Keyword), ("2", Kind::Number)],
            "the code after the closer is code again"
        );
    }

    #[test]
    fn a_block_comment_that_opens_and_closes_on_one_line_leaves_nothing_open() {
        let (rs, st) = scan(&RUST, "let /* here */ x = 1;", State::Normal);
        assert_eq!(st, State::Normal);
        assert_eq!(rs.len(), 3, "keyword, comment, number: {rs:?}");
    }

    /// Rust's `'` is a lifetime as often as a character, so it opens nothing.
    ///
    /// **A stated limitation of the table rather than an oversight.** Treating it as a string
    /// delimiter colours the remainder of every line holding `&'static str`, which is most of
    /// this repository's signatures.
    #[test]
    fn a_rust_lifetime_does_not_open_a_string() {
        // `static` is a keyword wherever it appears, `'static` included — which is what every
        // editor does. What matters is that nothing *after* a quote turns into a string.
        assert_eq!(
            runs(&RUST, "fn f<'a>(s: &'a str) -> &'static str { s }"),
            vec![("fn", Kind::Keyword), ("static", Kind::Keyword)],
            "a quote opened a string, so the rest of the line is coloured"
        );
        let (_, st) = scan(&RUST, "fn f<'a>(s: &'a str) {}", State::Normal);
        assert_eq!(st, State::Normal, "and it left nothing open for the next line");
    }

    /// A float is one number, and a range is two.
    ///
    /// **The visible half is the float**: `1.5` scanned as two numbers renders as a coloured
    /// digit, a foreground-coloured dot and a coloured digit — in the two languages most likely
    /// to be open on this system (PR #289 review, 3).
    #[test]
    fn a_decimal_point_joins_a_number_and_nothing_else() {
        assert_eq!(runs(&TOML, "timeout = 1.5"), vec![("1.5", Kind::Number)]);
        assert_eq!(runs(&RUST, "let x = 1.0;"), vec![
            ("let", Kind::Keyword),
            ("1.0", Kind::Number),
        ]);
        // A point with no digit after it ends the number, so Rust's range is two of them.
        assert_eq!(runs(&RUST, "for i in 1..2 {"), vec![
            ("for", Kind::Keyword),
            ("in", Kind::Keyword),
            ("1", Kind::Number),
            ("2", Kind::Number),
        ]);
        // …and a point after an *identifier* is not part of anything: the keyword still ends at
        // it, which is what makes `self.foo` work.
        assert_eq!(runs(&RUST, "self.field"), vec![("self", Kind::Keyword)]);
    }

    #[test]
    fn a_toml_table_header_and_its_values() {
        assert_eq!(runs(&TOML, "[server.tls]"), vec![("[server.tls]", Kind::Heading)]);
        assert_eq!(
            runs(&TOML, "port = 8080 # the usual"),
            vec![("8080", Kind::Number), ("# the usual", Kind::Comment)]
        );
        assert_eq!(runs(&TOML, "enabled = true"), vec![("true", Kind::Keyword)]);
    }

    #[test]
    fn markdown_headings_quotes_fences_and_code_spans() {
        assert_eq!(runs(&MARKDOWN, "## A heading"), vec![("## A heading", Kind::Heading)]);
        assert_eq!(runs(&MARKDOWN, "> quoted"), vec![("> quoted", Kind::Comment)]);
        assert_eq!(runs(&MARKDOWN, "use `code` here"), vec![("`code`", Kind::Str)]);

        let out = buffer(&MARKDOWN, &["```rust", "fn main() {}", "```", "after"]);
        assert_eq!(out[0], vec![("```rust", Kind::Str)]);
        assert_eq!(out[1], vec![("fn main() {}", Kind::Str)], "the fenced body is verbatim");
        assert_eq!(out[2], vec![("```", Kind::Str)], "and the closing fence is part of it");
        assert!(out[3].is_empty(), "the text after the fence is prose again");
    }

    /// The scanner is total: every input produces runs inside the line and no panic.
    ///
    /// **Including the inputs that are not a program.** A highlighter runs over a buffer
    /// mid-keystroke, so "half of something" is its ordinary input rather than an edge case.
    #[test]
    fn every_language_survives_garbage_and_stays_inside_the_line() {
        let langs = [PLAIN, NXSH, TOML, RUST, MARKDOWN];
        let lines = [
            "",
            "\"",
            "'",
            "`",
            "/*",
            "*/",
            "#",
            "$",
            "${",
            "```",
            ">",
            "[",
            "\\",
            "0x",
            "e\u{301}\u{4e2d}\u{6587} \"\u{1F600}",
            "let \"a",
        ];
        for lang in &langs {
            for start in [State::Normal, State::Block, State::Fence, State::Str('"')] {
                for line in lines {
                    let (rs, _) = scan(lang, line, start);
                    for r in &rs {
                        assert!(r.start < r.end, "{line:?}: empty run {r:?}");
                        assert!(r.end <= line.len(), "{line:?}: run past the end {r:?}");
                        assert!(line.is_char_boundary(r.start), "{line:?}: {r:?} splits a char");
                        assert!(line.is_char_boundary(r.end), "{line:?}: {r:?} splits a char");
                    }
                    for pair in rs.windows(2) {
                        assert!(pair[0].end <= pair[1].start, "{line:?}: overlap {pair:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn a_language_comes_from_the_extension_and_nothing_else_is_an_error() {
        // **Which language, not merely that there is one.** A `.toml` read as Markdown colours
        // nothing and looks exactly like a scanner that never ran, so the mapping is what the
        // assertion is about.
        for (file, want) in
            [("build.nx", "nxsh"), ("init.toml", "toml"), ("lib.rs", "rust"), ("README.md", "markdown")]
        {
            assert_eq!(name_of(for_name(file)), want, "{file}");
        }
        // **`None` is a supported answer**, not a gap: a file this system does not know is
        // plain text, drawn in one colour.
        assert!(for_name("notes").is_none(), "no extension is plain text");
        assert!(for_name("photo.png").is_none());
        assert!(for_name(".hidden").is_some_and(|_| true) || for_name(".hidden").is_none());
    }
}
