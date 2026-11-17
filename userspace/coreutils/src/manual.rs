//! The manual's pages: a subset of Markdown, parsed into blocks and laid out at a width.
//!
//! The format is `docs/spec/manual-page.md`'s. `man` is its one reader; it is here, beside the
//! other coreutils' shared code, so its tests run on the host.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

mod inline;

use inline::{Run, Style, printable, runs};

/// How far a page's body sits in from its headings.
pub const BODY_INDENT: usize = 4;

/// The narrowest a table's column is squeezed to before the table prints one record per row.
const MIN_COLUMN: usize = 10;

/// The space between two columns of a table.
const GUTTER: usize = 2;

/// One block of a page.
#[derive(Debug, PartialEq)]
enum Block {
    /// `#` to `######`, and its inline text.
    Heading(usize, String),
    /// Lines joined into one run of inline text.
    Paragraph(String),
    /// A list's items, each with its marker and the items nested under it.
    List(Vec<Item>),
    /// A fenced block's lines, as they are.
    Code(Vec<String>),
    /// A table's header and its rows, each cell inline text.
    Table { head: Vec<String>, rows: Vec<Vec<String>> },
    /// `>` lines, as one paragraph.
    Quote(String),
    /// `---`.
    Rule,
}

/// One item of a list.
#[derive(Debug, PartialEq)]
struct Item {
    /// `-` for a bullet, `3.` for a number.
    marker: String,
    /// Its text, continuation lines joined.
    text: String,
    /// Items nested one level under it.
    children: Vec<Item>,
}

/// One rendered line, and whether it belongs to a code block, which is never wrapped.
#[derive(Debug, PartialEq)]
pub struct Line {
    /// The text, with its style's escapes if styled.
    pub text: String,
    /// Whether it is a code block's.
    pub code: bool,
}

/// Where the pages are, each `<name>.<section>.md`: the profile server's projection of each
/// package's `manual/`, which no session can write.
pub const ROOT: &str = "/system/manual";

/// A page file's name and section, from `<name>.<section>.md`; a name may hold dots of its own, as
/// `init.toml.5.md` does.
pub fn page_of(file: &str) -> Option<(String, u8)> {
    let (name, section) = file.strip_suffix(".md")?.rsplit_once('.')?;
    let section = section.parse::<u8>().ok()?;
    (!name.is_empty()).then(|| (String::from(name), section))
}

/// The sections a name is looked for in, in order, when none is named: commands, then services,
/// then the rest by number.
pub const SECTION_ORDER: [u8; 5] = [1, 8, 2, 5, 7];

/// Which of `pages`, each a section and a name, `name` means: the one in `section` if one is
/// named, or the first in [`SECTION_ORDER`].
pub fn choose(pages: &[(u8, String)], section: Option<u8>, name: &str) -> Option<usize> {
    let at = |s: u8| pages.iter().position(|(ps, pn)| *ps == s && pn == name);
    match section {
        Some(s) => at(s),
        None => SECTION_ORDER.iter().find_map(|&s| at(s)),
    }
}

/// Whether a page whose name and summary these are holds `word`, in either, case aside.
pub fn mentions(name: &str, summary: &str, word: &str) -> bool {
    let word = word.to_lowercase();
    name.to_lowercase().contains(&word) || summary.to_lowercase().contains(&word)
}

/// What page `name`'s first heading says it is: `# name — what it is` gives the part after the
/// dash; any other heading, as a spec's `# RS Protocol — Auth operations`, is its own summary.
pub fn summary(page: &str, name: &str) -> String {
    let first = page.lines().find_map(|l| l.strip_prefix("# ")).unwrap_or("");
    let plain = plain_text(first);
    match plain.split_once(" — ") {
        Some((named, what)) if named.trim() == name => what.trim().to_string(),
        _ => plain.trim().to_string(),
    }
}

/// What `man` was asked, from its operands and whether `--keyword` was given.
#[derive(Debug, PartialEq, Eq)]
pub enum Request<'a> {
    /// Every page.
    List,
    /// The pages mentioning a word.
    Keyword(&'a str),
    /// A page, in a section or the first of that name.
    Page(Option<u8>, &'a str),
}

/// `man`'s operands as a [`Request`], or the usage error to print.
pub fn request<'a>(operands: &[&'a str], keyword: bool) -> Result<Request<'a>, &'static str> {
    match (keyword, operands) {
        (true, [word]) => Ok(Request::Keyword(word)),
        (true, _) => Err("man: --keyword takes one WORD (try --help)\n"),
        (false, []) => Ok(Request::List),
        (false, [name]) => Ok(Request::Page(None, name)),
        (false, [section, name]) => match section.parse::<u8>() {
            Ok(s) => Ok(Request::Page(Some(s), name)),
            Err(_) => Err("man: SECTION is a number (try --help)\n"),
        },
        _ => Err("man: too many operands (try --help)\n"),
    }
}

/// One page found under [`ROOT`]: its text, or why it could not be read.
#[derive(Debug, PartialEq)]
pub struct Found {
    /// Its section.
    pub section: u8,
    /// Its name.
    pub name: String,
    /// Its text, or what reading it said.
    pub text: Result<String, String>,
}

impl Found {
    /// What `man`'s listing says of it: its summary, or that it could not be read.
    pub fn summary(&self) -> String {
        match &self.text {
            Ok(text) => summary(text, &self.name),
            Err(why) => alloc::format!("(could not be read: {why})"),
        }
    }
}

/// Page `name`, in `section` or the first of that name, rendered at `width`; or what to say
/// instead: that there is no such page, or that there is and it could not be read.
pub fn answer(pages: &[Found], section: Option<u8>, name: &str, width: usize) -> Result<String, String> {
    let names: Vec<(u8, String)> = pages.iter().map(|p| (p.section, p.name.clone())).collect();
    let Some(i) = choose(&names, section, name) else {
        return Err(alloc::format!("man: no manual page for {name}\n"));
    };
    match &pages[i].text {
        Ok(text) => Ok(render(text, width, false)),
        Err(why) => Err(alloc::format!("man: {name}({}) could not be read: {why}\n", pages[i].section)),
    }
}

/// `text`'s inline markup read and dropped.
fn plain_text(text: &str) -> String {
    let mut out = String::new();
    for r in runs(text) {
        out.push_str(&r.text);
    }
    out
}

/// Render `page` at `width` columns, with bold and the like as escapes when `styled`.
pub fn render(page: &str, width: usize, styled: bool) -> String {
    let mut out = String::new();
    for line in render_lines(page, width, styled) {
        out.push_str(&line.text);
        out.push('\n');
    }
    out
}

/// [`render`]'s lines, each saying whether it is code.
pub fn render_lines(page: &str, width: usize, styled: bool) -> Vec<Line> {
    let width = width.max(BODY_INDENT + MIN_COLUMN);
    let mut out: Vec<Line> = Vec::new();
    for (i, block) in parse(page).iter().enumerate() {
        if i > 0 {
            out.push(Line { text: String::new(), code: false });
        }
        lay_out(block, width, styled, &mut out);
    }
    out
}

fn lay_out(block: &Block, width: usize, styled: bool, out: &mut Vec<Line>) {
    let body = width - BODY_INDENT;
    match block {
        Block::Heading(_, text) => {
            for l in wrap(&with_style(runs(text), Style::BOLD), width, 0, 0, styled) {
                out.push(Line { text: l, code: false });
            }
        }
        Block::Paragraph(text) => push(out, wrap(&runs(text), body, BODY_INDENT, BODY_INDENT, styled)),
        Block::Quote(text) => {
            let lines = wrap(&runs(text), body - 2, 0, 0, styled);
            for l in lines {
                let mut s = " ".repeat(BODY_INDENT);
                s.push_str("| ");
                s.push_str(&l);
                out.push(Line { text: s, code: false });
            }
        }
        Block::List(items) => list(items, BODY_INDENT, width, styled, out),
        Block::Code(lines) => {
            for l in lines {
                let mut s = " ".repeat(BODY_INDENT * 2);
                s.push_str(&printable(&l.replace('\t', "    ")));
                out.push(Line { text: s, code: true });
            }
        }
        Block::Table { head, rows } => table(head, rows, body, styled, out),
        Block::Rule => {
            let mut s = " ".repeat(BODY_INDENT);
            s.push_str(&"-".repeat(body.min(40)));
            out.push(Line { text: s, code: false });
        }
    }
}

fn push(out: &mut Vec<Line>, lines: Vec<String>) {
    out.extend(lines.into_iter().map(|text| Line { text, code: false }));
}

fn list(items: &[Item], indent: usize, width: usize, styled: bool, out: &mut Vec<Line>) {
    for item in items {
        // A hanging indent: the text's later lines start under its first word, not the marker.
        let hang = indent + item.marker.chars().count() + 1;
        let mut lines = wrap(&runs(&item.text), width.saturating_sub(hang).max(MIN_COLUMN), hang, hang, styled);
        if let Some(first) = lines.first_mut() {
            let mut lead = " ".repeat(indent);
            lead.push_str(&item.marker);
            lead.push(' ');
            first.replace_range(..hang, &lead);
        }
        push(out, lines);
        list(&item.children, indent + 2, width, styled, out);
    }
}

fn table(head: &[String], rows: &[Vec<String>], width: usize, styled: bool, out: &mut Vec<Line>) {
    // As many columns as the widest row: a cell past the header's is drawn under an empty name.
    let n = rows.iter().map(Vec::len).chain([head.len()]).max().unwrap_or(0);
    let cells = |row: &[String], i: usize| row.get(i).map(|c| runs(c)).unwrap_or_default();
    let natural: Vec<usize> = (0..n)
        .map(|i| {
            core::iter::once(head)
                .chain(rows.iter().map(|r| r.as_slice()))
                .map(|r| visible(&cells(r, i)))
                .max()
                .unwrap_or(0)
                .max(1)
        })
        .collect();
    let room = width.saturating_sub(GUTTER * n.saturating_sub(1));
    let Some(widths) = fit(&natural, room) else {
        return records(head, rows, width, styled, out);
    };
    let row = |row: &[String], bold: bool, out: &mut Vec<Line>| draw(row, &widths, bold, styled, out);
    row(head, true, out);
    let mut rule = " ".repeat(BODY_INDENT);
    for (i, &w) in widths.iter().enumerate() {
        rule.push_str(&"-".repeat(w));
        if i + 1 < n {
            rule.push_str(&" ".repeat(GUTTER));
        }
    }
    out.push(Line { text: rule, code: false });
    for r in rows {
        row(r, false, out);
    }
}

/// One table row in its columns, each cell wrapped in its own, as many lines as the tallest.
fn draw(row: &[String], widths: &[usize], bold: bool, styled: bool, out: &mut Vec<Line>) {
    let n = widths.len();
    let mut columns: Vec<Vec<String>> = Vec::with_capacity(n);
    for (i, &w) in widths.iter().enumerate() {
        let r = row.get(i).map(|c| runs(c)).unwrap_or_default();
        let r = if bold { with_style(r, Style::BOLD) } else { r };
        columns.push(wrap(&r, w, 0, 0, styled));
    }
    let height = columns.iter().map(Vec::len).max().unwrap_or(0).max(1);
    for line in 0..height {
        let mut s = " ".repeat(BODY_INDENT);
        for (i, col) in columns.iter().enumerate() {
            let cell = col.get(line).map(String::as_str).unwrap_or("");
            s.push_str(cell);
            if i + 1 < n {
                s.push_str(&" ".repeat(widths[i] - visible_str(cell) + GUTTER));
            }
        }
        out.push(Line { text: String::from(s.trim_end()), code: false });
    }
}

/// The columns' widths in `room`: as they are if they fit, the widest narrowed one at a time
/// otherwise, `None` once every column is at [`MIN_COLUMN`] and they still do not.
fn fit(natural: &[usize], room: usize) -> Option<Vec<usize>> {
    let mut widths = natural.to_vec();
    while widths.iter().sum::<usize>() > room {
        let (i, &w) = widths.iter().enumerate().max_by_key(|&(_, w)| *w)?;
        if w <= MIN_COLUMN {
            return None;
        }
        widths[i] = w - 1;
    }
    Some(widths)
}

/// A table too wide for its columns: one record per row, each cell under its column's name.
fn records(head: &[String], rows: &[Vec<String>], width: usize, styled: bool, out: &mut Vec<Line>) {
    for (r, row) in rows.iter().enumerate() {
        if r > 0 {
            out.push(Line { text: String::new(), code: false });
        }
        for (i, name) in head.iter().enumerate() {
            let mut field = with_style(runs(name), Style::BOLD);
            if let Some(last) = field.last_mut() {
                last.text.push_str(": ");
            }
            field.extend(row.get(i).map(|c| runs(c)).unwrap_or_default());
            // Its later lines hang two in, under the value rather than the name.
            push(out, wrap(&field, width.saturating_sub(2).max(MIN_COLUMN), BODY_INDENT, BODY_INDENT + 2, styled));
        }
    }
}

fn with_style(mut r: Vec<Run>, style: Style) -> Vec<Run> {
    for run in &mut r {
        run.style = run.style.with(style);
    }
    r
}

/// The columns `r` takes, escapes aside.
fn visible(r: &[Run]) -> usize {
    r.iter().map(|x| x.text.chars().count()).sum()
}

/// The columns `s` takes, skipping the escapes [`wrap`] wrote into it.
fn visible_str(s: &str) -> usize {
    let mut n = 0;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for c in chars.by_ref() {
                if c == 'm' {
                    break;
                }
            }
        } else {
            n += 1;
        }
    }
    n
}

/// `r` filled into lines of at most `width` columns: the first indented `first`, the rest `rest`.
/// A word longer than a line is broken, so no line is wider than asked.
fn wrap(r: &[Run], width: usize, first: usize, rest: usize, styled: bool) -> Vec<String> {
    let mut w = Wrapper { width: width.max(1), rest, lines: Vec::new(), line: String::new(), used: 0, indent: first };
    for (mut word, space_before) in words(r) {
        loop {
            let gap = usize::from(w.used > 0 && space_before);
            if w.used + gap + word.len() <= w.width {
                if gap == 1 {
                    w.line.push(' ');
                }
                paint_word(&mut w.line, &word, styled);
                w.used += gap + word.len();
                break;
            }
            if w.used > 0 {
                w.flush();
                continue;
            }
            // A word wider than a whole line is broken where the line ends.
            let tail = word.split_off(w.width);
            paint_word(&mut w.line, &word, styled);
            w.used = w.width;
            w.flush();
            word = tail;
        }
    }
    if w.used > 0 || w.lines.is_empty() {
        w.flush();
    }
    w.lines
}

/// Paint `word`, each run of one style in it as that style.
fn paint_word(line: &mut String, word: &[(char, Style)], styled: bool) {
    let mut i = 0;
    while i < word.len() {
        let style = word[i].1;
        let end = word[i..].iter().position(|c| c.1 != style).map_or(word.len(), |n| i + n);
        paint(line, &word[i..end].iter().map(|c| c.0).collect::<String>(), style, styled);
        i = end;
    }
}

/// [`wrap`]'s state: the lines so far, and the one being filled.
struct Wrapper {
    width: usize,
    rest: usize,
    lines: Vec<String>,
    line: String,
    used: usize,
    indent: usize,
}

impl Wrapper {
    /// End the line being filled, indented, and start the next at the later lines' indent.
    fn flush(&mut self) {
        let mut s = " ".repeat(self.indent);
        s.push_str(self.line.trim_end());
        self.lines.push(s);
        self.line.clear();
        self.used = 0;
        self.indent = self.rest;
    }
}

fn paint(line: &mut String, text: &str, style: Style, styled: bool) {
    match (styled, style.escape()) {
        (true, Some(on)) => {
            line.push_str(on);
            line.push_str(text);
            line.push_str("\x1b[0m");
        }
        _ => line.push_str(text),
    }
}

/// `r`'s words, each a character at a time with its style, and whether a space came before it. A
/// word split across runs, as `**bold**ing` or `(`code`)` is, is one word: a line never breaks
/// between its parts.
fn words(r: &[Run]) -> Vec<(Vec<(char, Style)>, bool)> {
    let mut out = Vec::new();
    let mut word = Vec::new();
    let (mut space, mut space_before) = (false, false);
    for c in r.iter().flat_map(|run| run.text.chars().map(move |c| (c, run.style))) {
        if c.0.is_whitespace() {
            if !word.is_empty() {
                out.push((core::mem::take(&mut word), space_before));
            }
            space = true;
        } else {
            if word.is_empty() {
                space_before = core::mem::take(&mut space);
            }
            word.push(c);
        }
    }
    if !word.is_empty() {
        out.push((word, space_before));
    }
    out
}

/// `page` as blocks.
fn parse(page: &str) -> Vec<Block> {
    // A comment closed on its own line with text after it gives the line that text.
    let lines: Vec<&str> = page
        .lines()
        .map(|l| match l.trim_start().strip_prefix("<!--").and_then(|r| r.split_once("-->")) {
            Some((_, after)) if !after.trim().is_empty() => after,
            _ => l,
        })
        .collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();
        if t.is_empty() {
            i += 1;
        } else if t.starts_with("<!--") {
            // A comment, perhaps over several lines.
            while i < lines.len() && !lines[i].contains("-->") {
                i += 1;
            }
            i += 1;
        } else if let Some(fence) = fence(t) {
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !closes(lines[i], fence) {
                code.push(String::from(lines[i]));
                i += 1;
            }
            i += 1;
            blocks.push(Block::Code(code));
        } else if let Some((level, text)) = heading(t) {
            blocks.push(Block::Heading(level, String::from(text)));
            i += 1;
        } else if is_rule(t) {
            blocks.push(Block::Rule);
            i += 1;
        } else if t.starts_with('|') && lines.get(i + 1).is_some_and(|n| is_separator(n)) {
            let head = cells(t);
            let mut rows = Vec::new();
            i += 2;
            while i < lines.len() && lines[i].trim_start().starts_with('|') {
                rows.push(cells(lines[i].trim_start()));
                i += 1;
            }
            blocks.push(Block::Table { head, rows });
        } else if t.starts_with('>') {
            let mut text = String::new();
            while i < lines.len() && lines[i].trim_start().starts_with('>') {
                join(&mut text, lines[i].trim_start().trim_start_matches('>'));
                i += 1;
            }
            blocks.push(Block::Quote(text));
        } else if marker(line).is_some() {
            let (items, next) = list_items(&lines, i);
            blocks.push(Block::List(items));
            i = next;
        } else {
            // Its first line always, even one that looks like another block's start but is not,
            // as a `|` line with no table under it: or the loop would never move.
            let mut text = String::new();
            join(&mut text, line);
            i += 1;
            while i < lines.len() && !ends_paragraph(lines[i]) {
                join(&mut text, lines[i]);
                i += 1;
            }
            blocks.push(Block::Paragraph(text));
        }
    }
    blocks
}

/// The items of a list starting at `lines[start]`, and the line after it.
fn list_items(lines: &[&str], start: usize) -> (Vec<Item>, usize) {
    let mut items: Vec<Item> = Vec::new();
    let base = indent_of(lines[start]);
    let mut i = start;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            // A blank line ends the list unless an item, or a line indented under one, follows.
            match lines.get(i + 1) {
                Some(n) if marker(n).is_some() || (!n.trim().is_empty() && indent_of(n) > base) => {
                    i += 1;
                    continue;
                }
                _ => break,
            }
        }
        // A table or a fence indented under an item is a block of its own, which ends the list.
        let t = line.trim_start();
        if fence(t).is_some() || (t.starts_with('|') && lines.get(i + 1).is_some_and(|n| is_separator(n))) {
            break;
        }
        match marker(line) {
            Some((m, text)) if indent_of(line) <= base + 1 => {
                items.push(Item { marker: m, text: String::from(text), children: Vec::new() });
            }
            Some((m, text)) => match items.last_mut() {
                Some(parent) => {
                    parent.children.push(Item { marker: m, text: String::from(text), children: Vec::new() })
                }
                None => items.push(Item { marker: m, text: String::from(text), children: Vec::new() }),
            },
            None if indent_of(line) > base => {
                // A continuation, of the deepest item open.
                if let Some(parent) = items.last_mut() {
                    let target = parent.children.last_mut().map_or(&mut parent.text, |c| &mut c.text);
                    join(target, line);
                }
            }
            None if ends_paragraph(line) => break,
            None => {
                // A lazy continuation, flush with the marker: still the last item's.
                if let Some(parent) = items.last_mut() {
                    join(&mut parent.text, line);
                }
            }
        }
        i += 1;
    }
    (items, i)
}

fn join(text: &mut String, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    if !text.is_empty() {
        text.push(' ');
    }
    text.push_str(line);
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// A list item's marker and text: `- `, `* `, `+ ` or `12. `.
fn marker(line: &str) -> Option<(String, &str)> {
    let t = line.trim_start();
    for b in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(b) {
            return Some((String::from("-"), rest));
        }
    }
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && t[digits..].starts_with(". ") {
        return Some((String::from(&t[..digits + 1]), &t[digits + 2..]));
    }
    None
}

/// A fence opening a code block: its character and how many, three or more.
fn fence(t: &str) -> Option<(char, usize)> {
    let c = t.chars().next().filter(|&c| c == '`' || c == '~')?;
    let n = t.chars().take_while(|&x| x == c).count();
    (n >= 3).then_some((c, n))
}

/// Whether `line` closes the block `open` opened: as many of its character or more, and nothing
/// else.
fn closes(line: &str, open: (char, usize)) -> bool {
    let t = line.trim();
    t.chars().count() >= open.1 && t.chars().all(|c| c == open.0)
}

fn heading(t: &str) -> Option<(usize, &str)> {
    let level = t.chars().take_while(|&c| c == '#').count();
    let rest = t.get(level..)?;
    (1..=6).contains(&level).then_some(())?;
    let text = rest.strip_prefix(' ')?.trim();
    // A closing run of `#` only after a space, so `# Using C#` keeps its own.
    let bare = text.trim_end_matches('#');
    let text = if bare.is_empty() || bare.ends_with(' ') { bare.trim_end() } else { text };
    Some((level, text))
}

fn is_rule(t: &str) -> bool {
    let t = t.trim();
    let Some(c) = t.chars().next() else { return false };
    matches!(c, '-' | '*' | '_') && t.len() >= 3 && t.chars().all(|x| x == c)
}

fn is_separator(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Whether `line` starts a block other than the paragraph it would continue.
fn ends_paragraph(line: &str) -> bool {
    let t = line.trim_start();
    t.is_empty()
        || heading(t).is_some()
        || fence(t).is_some()
        || t.starts_with('|')
        || t.starts_with('>')
        || t.starts_with("<!--")
        || is_rule(t)
        || marker(line).is_some()
}

/// A table row's cells: split at each `|` outside a code span and not escaped.
fn cells(row: &str) -> Vec<String> {
    let row = row.trim();
    let row = row.strip_prefix('|').unwrap_or(row);
    let row = row.strip_suffix('|').filter(|r| !r.ends_with('\\')).unwrap_or(row);
    let mut out = Vec::new();
    let mut cell = String::new();
    // The length of the backtick run that opened the code span the cell is in, if it is in one.
    let mut code: Option<usize> = None;
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cell.push('|');
                chars.next();
            }
            '`' => {
                let mut run = 1;
                while chars.peek() == Some(&'`') {
                    chars.next();
                    run += 1;
                }
                code = match code {
                    None => Some(run),
                    Some(open) if open == run => None,
                    still => still,
                };
                cell.push_str(&"`".repeat(run));
            }
            '|' if code.is_none() => out.push(String::from(core::mem::take(&mut cell).trim())),
            _ => cell.push(c),
        }
    }
    out.push(String::from(cell.trim()));
    out
}

#[cfg(test)]
mod tests;
