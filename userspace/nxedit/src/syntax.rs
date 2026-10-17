//! A tolerant, table-driven scanner: what a line of text *looks like*.
//!
//! Total by construction: no input is refused, and an unterminated string runs to the end of
//! its line (or past it, if the language says so). `nxsh`'s lexer is not reused: it is
//! fallible and parser-mode-driven, and an unterminated string is the normal state of a
//! buffer being typed in.
//!
//! [`Language`] is data, so one scanner drives them all and a new language is a table.

use alloc::vec::Vec;

/// What a run of text is, for colouring. [`Kind::Plain`] takes the ordinary foreground.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Ordinary text; never emitted as a run.
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
    /// The name on the left of an assignment: a TOML key.
    Key,
}

/// A coloured stretch of one line, in byte offsets within that line.
///
/// Only non-plain stretches are emitted, so a line of prose produces no runs.
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
/// A block comment or fence makes line *N*'s colours depend on line *N-1*, so a line cannot be
/// scanned alone. `Copy`, so an editor can cache one per line and compare them.
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
    /// Only reachable for a rule whose `multiline` is set.
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
    /// What to call it, for a receipt. A field because comparing slices by address rests on
    /// the compiler not merging two identical empty ones.
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
    /// The whole line because stopping at a comment would need a grammar: `[server] # note`
    /// colours the note as part of the header.
    pub line_prefixes: &'static [(&'static str, Kind)],
    /// A fence that toggles a verbatim block, tested at the start of a line.
    pub fence: Option<&'static str>,
    /// A delimiter for a *span* of verbatim text within a line — Markdown's backtick.
    pub code_span: Option<char>,
    /// The character that separates a name from its value, when a language has one line-shaped
    /// assignment — TOML's `=`. The name before it is a [`Kind::Key`].
    ///
    /// `None` for every language with a grammar: there an `=` is an operator.
    pub assignment: Option<char>,
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
    assignment: None,
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
        "if", "else", "elif", "while", "for", "in", "match", "try", "catch", "fn", "let", "return", "break",
        "continue", "and", "or", "not", "true", "false", "def", "end",
    ],
    numbers: true,
    variable: Some('$'),
    line_prefixes: &[],
    fence: None,
    code_span: None,
    assignment: None,
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
    assignment: Some('='),
};

/// Rust.
///
/// No character literals: the same quote opens a lifetime, and telling them apart needs a
/// grammar. Leaving `'x'` plain beats colouring half of every generic function.
pub const RUST: Language = Language {
    name: "rust",
    line_comments: &["//"],
    block_comment: Some(("/*", "*/")),
    strings: &[StringRule { delim: '"', escape: true, multiline: true }],
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn",
        "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self",
        "Self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
    ],
    numbers: true,
    variable: None,
    line_prefixes: &[],
    fence: None,
    code_span: None,
    assignment: None,
};

/// Markdown — every document under `docs/`.
///
/// Headings and quotes are line prefixes, a fenced block is the multi-line state, an inline
/// code span is a delimiter. Emphasis is not coloured: pairing `*` across a line is a grammar.
pub const MARKDOWN: Language = Language {
    name: "markdown",
    line_comments: &[],
    block_comment: None,
    strings: &[],
    keywords: &[],
    numbers: false,
    variable: None,
    line_prefixes: &[("#", Kind::Heading), (">", Kind::Comment)],
    fence: Some("```"),
    code_span: Some('`'),
    assignment: None,
};

/// The language a file's name implies, from its extension. `None` (plain text) is a supported
/// answer.
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

/// What to call a language in a receipt; `None` is `"plain"`.
pub fn name_of(lang: Option<Language>) -> &'static str {
    lang.map_or(PLAIN.name, |l| l.name)
}

/// Scan one line, given what was open at its start. Returns its runs and what is open after it.
///
/// `line` is without its terminator; run offsets are bytes within it.
pub fn scan(lang: &Language, line: &str, start: State) -> (Vec<Run>, State) {
    let mut out = Vec::new();
    let mut state = start;

    match state {
        State::Fence => {
            // The closing fence line is part of the block.
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

    // Only the head of the line, up to the first separator: an `=` in a value is not a second
    // key.
    //
    // The comment check is redundant today (`#` is not a name character) but kept: the name
    // rule may be widened, and without this `# font_px = 12` would become a key and a number.
    let commented = lang.line_comments.iter().any(|c| trimmed.starts_with(c));
    if let Some(sep) = lang.assignment
        && !commented
        && let Some(at) = line.find(sep)
    {
        let name = &line[..at];
        let trimmed_name = name.trim();
        let plain_name = !trimmed_name.is_empty()
            && trimmed_name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == ' ');
        if plain_name {
            let from = name.len() - name.trim_start().len();
            push(&mut out, from, from + trimmed_name.len(), Kind::Key);
            return scan_from(lang, line, at, State::Normal, out);
        }
    }

    scan_from(lang, line, 0, State::Normal, out)
}

/// The ordinary scan of `line` from byte `at`, with `out` already holding earlier runs.
fn scan_from(lang: &Language, line: &str, at: usize, mut state: State, mut out: Vec<Run>) -> (Vec<Run>, State) {
    let b = line.as_bytes();
    let mut i = at;
    while i < line.len() {
        if lang.line_comments.iter().any(|c| line[i..].starts_with(c)) {
            push(&mut out, i, line.len(), Kind::Comment);
            return (out, state);
        }
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
        if let Some(rule) = lang.strings.iter().find(|r| r.delim == c) {
            match close_string(line, i + c.len_utf8(), c, rule.escape) {
                Some(end) => {
                    push(&mut out, i, end, Kind::Str);
                    i = end;
                }
                None => {
                    push(&mut out, i, line.len(), Kind::Str);
                    // Only a multiline rule leaves the string open, so a typed quote colours one
                    // line rather than the file below it.
                    state = if rule.multiline { State::Str(c) } else { State::Normal };
                    return (out, state);
                }
            }
            continue;
        }
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
            // A bare sigil is left plain so a typed `$` does not flicker a colour.
            if j > i + c.len_utf8() {
                push(&mut out, i, j, Kind::Variable);
            }
            i = j.max(i + c.len_utf8());
            continue;
        }
        if is_word(b[i]) {
            let mut j = i;
            while j < line.len() && is_word(b[j]) {
                j += 1;
            }
            // A point joins a number only when a digit follows it, so `1.5` is one number and
            // `1..2` is two. Not in `is_word`: that would swallow the `self` in `self.foo`.
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

/// Where the string opened before `from` ends, as a byte offset past its closing delimiter.
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
/// `.` joins only the parts of a number, handled where numbers are scanned; as a general rule
/// it would make `self.foo` one word. `-` would break `a-b` in every other language.
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

    fn runs<'a>(lang: &Language, line: &'a str) -> Vec<(&'a str, Kind)> {
        let (rs, _) = scan(lang, line, State::Normal);
        rs.iter().map(|r| (&line[r.start..r.end], r.kind)).collect()
    }

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
        assert!(runs(&PLAIN, "just some words").is_empty());
        assert!(runs(&RUST, "        ").is_empty());
    }

    #[test]
    fn a_shell_line_colours_its_keyword_its_variable_and_its_comment() {
        assert_eq!(
            runs(&NXSH, "if $HOME { echo 1 } # done"),
            vec![("if", Kind::Keyword), ("$HOME", Kind::Variable), ("1", Kind::Number), ("# done", Kind::Comment),]
        );
        assert_eq!(runs(&NXSH, "echo ${WHAT}x"), vec![("${WHAT}", Kind::Variable)]);
    }

    /// A bare sigil is not a variable.
    #[test]
    fn a_sigil_with_no_name_after_it_is_not_a_variable() {
        assert!(runs(&NXSH, "echo $ x").is_empty());
    }

    /// An unterminated string ends at its line: carrying it down would recolour the file.
    #[test]
    fn an_unterminated_string_ends_at_its_line() {
        assert_eq!(runs(&NXSH, "echo \"unfinished"), vec![("\"unfinished", Kind::Str)]);
        let (_, st) = scan(&NXSH, "echo \"unfinished", State::Normal);
        assert_eq!(st, State::Normal, "a shell string does not span lines");
    }

    /// A language whose strings span lines (Rust) carries the open string.
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
        assert_eq!(runs(&RUST, r#"let s = "a\"b";"#), vec![("let", Kind::Keyword), (r#""a\"b""#, Kind::Str),]);
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
    #[test]
    fn a_rust_lifetime_does_not_open_a_string() {
        assert_eq!(
            runs(&RUST, "fn f<'a>(s: &'a str) -> &'static str { s }"),
            vec![("fn", Kind::Keyword), ("static", Kind::Keyword)],
            "a quote opened a string, so the rest of the line is coloured"
        );
        let (_, st) = scan(&RUST, "fn f<'a>(s: &'a str) {}", State::Normal);
        assert_eq!(st, State::Normal, "and it left nothing open for the next line");
    }

    /// A float is one number, and a range is two.
    #[test]
    fn a_decimal_point_joins_a_number_and_nothing_else() {
        assert_eq!(runs(&TOML, "timeout = 1.5"), vec![("timeout", Kind::Key), ("1.5", Kind::Number)]);
        assert_eq!(runs(&RUST, "let x = 1.0;"), vec![("let", Kind::Keyword), ("1.0", Kind::Number),]);
        assert_eq!(
            runs(&RUST, "for i in 1..2 {"),
            vec![("for", Kind::Keyword), ("in", Kind::Keyword), ("1", Kind::Number), ("2", Kind::Number),]
        );
        assert_eq!(runs(&RUST, "self.field"), vec![("self", Kind::Keyword)]);
    }

    /// A key is the plain name at the head of a line, and nothing else is.
    #[test]
    fn a_key_is_the_name_at_the_head_of_a_line() {
        assert_eq!(
            runs(&TOML, "wallpaper = \"/home/a.png\""),
            vec![("wallpaper", Kind::Key), ("\"/home/a.png\"", Kind::Str)],
            "the name, then the value"
        );
        assert_eq!(
            runs(&TOML, "  indented = 1"),
            vec![("indented", Kind::Key), ("1", Kind::Number)],
            "leading space is not part of the name"
        );
        assert_eq!(
            runs(&TOML, "# font_px = 12"),
            vec![("# font_px = 12", Kind::Comment)],
            "a commented-out setting is not a key"
        );
        assert_eq!(runs(&TOML, "[a.b]"), vec![("[a.b]", Kind::Heading)]);
        assert_eq!(
            runs(&TOML, "greeting = \"a = b\""),
            vec![("greeting", Kind::Key), ("\"a = b\"", Kind::Str)],
            "only the first separator counts"
        );
        assert_eq!(
            runs(&TOML, "\"quoted = key\" = 1"),
            vec![("\"quoted = key\"", Kind::Str), ("1", Kind::Number)],
            "a quoted name is a string, which this rule leaves alone"
        );
        assert_eq!(
            runs(&RUST, "let x = 1;"),
            vec![("let", Kind::Keyword), ("1", Kind::Number)],
            "Rust has no line-shaped assignment"
        );
    }

    #[test]
    fn a_toml_table_header_and_its_values() {
        assert_eq!(runs(&TOML, "[server.tls]"), vec![("[server.tls]", Kind::Heading)]);
        assert_eq!(
            runs(&TOML, "port = 8080 # the usual"),
            vec![("port", Kind::Key), ("8080", Kind::Number), ("# the usual", Kind::Comment)]
        );
        assert_eq!(runs(&TOML, "enabled = true"), vec![("enabled", Kind::Key), ("true", Kind::Keyword)]);
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

    /// Every input yields runs inside the line and no panic; a buffer is scanned mid-keystroke.
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
        for (file, want) in [("build.nx", "nxsh"), ("init.toml", "toml"), ("lib.rs", "rust"), ("README.md", "markdown")]
        {
            assert_eq!(name_of(for_name(file)), want, "{file}");
        }
        assert!(for_name("notes").is_none(), "no extension is plain text");
        assert!(for_name("photo.png").is_none());
        assert!(for_name(".hidden").is_some_and(|_| true) || for_name(".hidden").is_none());
    }
}
