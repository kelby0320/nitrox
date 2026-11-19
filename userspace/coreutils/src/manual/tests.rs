use super::*;

fn plain(page: &str, width: usize) -> Vec<String> {
    render_lines(page, width, false).into_iter().map(|l| l.text).collect()
}

fn widest(lines: &[String]) -> usize {
    lines.iter().map(|l| l.chars().count()).max().unwrap_or(0)
}

#[test]
fn a_heading_sits_at_the_margin_and_its_body_in_from_it() {
    let lines = plain("# man — read the manual\n\n## Options\n\nOne line of **body** text.\n", 40);
    assert_eq!(lines, ["man — read the manual", "", "Options", "", "    One line of body text."]);
}

#[test]
fn a_paragraph_wraps_at_the_width_and_never_past_it() {
    let text = "the quick brown fox jumps over the lazy dog ".repeat(6);
    let lines = plain(&text, 30);
    assert!(lines.len() > 5, "{lines:?}");
    assert_eq!(widest(&lines), 30, "filled to the width: {lines:?}");
    assert!(lines.iter().all(|l| l.starts_with("    ")), "indented under its heading: {lines:?}");
}

#[test]
fn a_word_longer_than_a_line_is_broken_where_the_line_ends() {
    let word = "x".repeat(70);
    let lines = plain(&word, 30);
    assert_eq!(widest(&lines), 30);
    assert_eq!(lines.iter().map(|l| l.trim().len()).sum::<usize>(), 70, "nothing lost: {lines:?}");
}

#[test]
fn a_list_hangs_its_lines_under_its_text_and_nests_one_level() {
    let page = "- first item that is long enough to wrap onto a second line\n  - nested\n- second\n";
    let lines = plain(page, 30);
    assert_eq!(lines[0], "    - first item that is long");
    assert!(lines[1].starts_with("      enough"), "hung under the text: {lines:?}");
    assert!(lines.iter().any(|l| l == "      - nested"), "{lines:?}");
    assert!(lines.iter().any(|l| l == "    - second"), "{lines:?}");
    let numbered = plain("1. one\n2. two\n", 30);
    assert_eq!(numbered, ["    1. one", "    2. two"]);
}

#[test]
fn a_code_block_is_left_whole_and_said_to_be_code() {
    let long = "let x = ".to_string() + &"y".repeat(60);
    let page = alloc::format!("Before.\n\n```nxsh\n{long}\n```\n\nAfter.\n");
    let lines = render_lines(&page, 30, false);
    let code: Vec<&Line> = lines.iter().filter(|l| l.code).collect();
    assert_eq!(code.len(), 1);
    assert!(code[0].text.ends_with(&long), "not wrapped: {:?}", code[0]);
    assert!(lines.iter().filter(|l| !l.code).all(|l| l.text.chars().count() <= 30));
}

#[test]
fn a_table_that_fits_is_drawn_in_columns_under_a_rule() {
    let page = "| Section | Holds |\n|---|---|\n| 1 | Commands |\n| 2 | System calls |\n";
    let lines = plain(page, 60);
    assert_eq!(
        lines,
        ["    Section  Holds", "    -------  ------------", "    1        Commands", "    2        System calls"]
    );
}

#[test]
fn a_table_too_wide_wraps_each_cell_in_its_column() {
    let long = "a cell with many words in it that cannot all fit on one line";
    let page = alloc::format!("| Name | What |\n|---|---|\n| list | {long} |\n");
    let lines = plain(&page, 40);
    assert!(widest(&lines) <= 40, "{lines:?}");
    assert!(lines.len() > 4, "the cell wrapped: {lines:?}");
    assert!(lines[2].starts_with("    list  "), "the row's first line has both cells: {lines:?}");
    assert!(lines[3].starts_with("          "), "and its next only the wrapped cell's: {lines:?}");
}

#[test]
fn a_table_whose_columns_cannot_fit_prints_one_record_per_row() {
    let head: Vec<String> = (0..8).map(|i| alloc::format!("Column{i}")).collect();
    let row: Vec<String> = (0..8).map(|i| alloc::format!("value-{i}-long")).collect();
    let page = alloc::format!("| {} |\n|{}\n| {} |\n", head.join(" | "), "---|".repeat(8), row.join(" | "));
    let lines = plain(&page, 40);
    assert!(widest(&lines) <= 40, "{lines:?}");
    assert_eq!(lines[0], "    Column0: value-0-long");
    assert_eq!(lines.len(), 8, "a line a cell: {lines:?}");
}

#[test]
fn a_link_is_its_text_and_html_draws_nothing_but_a_placeholder_stays() {
    let page = "See [the spec](../spec/x.md#y) and <a id=\"here\"></a>more.<!-- check-docs: allow-missing -->\n\n\
                Spawn <name> with `with <view> <program>` &amp; done.\n";
    let lines = plain(page, 80);
    assert_eq!(lines[0], "    See the spec and more.");
    assert_eq!(lines[2], "    Spawn <name> with with <view> <program> & done.");
}

#[test]
fn styled_text_carries_its_escapes_and_plain_text_none() {
    let page = "Use **bold**, `code` and *stress*.\n";
    let styled = render(page, 60, true);
    assert!(styled.contains("\x1b[1mbold\x1b[0m"), "{styled:?}");
    assert!(styled.contains("\x1b[36mcode\x1b[0m"), "{styled:?}");
    assert!(styled.contains("\x1b[4mstress\x1b[0m"), "{styled:?}");
    assert_eq!(render(page, 60, false), "    Use bold, code and stress.\n");
}

#[test]
fn an_asterisk_between_spaces_is_an_asterisk() {
    assert_eq!(plain("Compute a * b * c.\n", 60), ["    Compute a * b * c."]);
    assert_eq!(plain("And 2*3*4, and x*y.\n", 60), ["    And 2*3*4, and x*y."], "inside a word");
}

#[test]
fn a_pipe_in_code_or_escaped_stays_in_its_cell() {
    let page = "| Op | Use |\n|---|---|\n| `a \\| b` | pipe |\n| x \\| y | or |\n";
    let lines = plain(page, 60);
    assert!(lines[2].starts_with("    a | b  pipe"), "{lines:?}");
    assert!(lines[3].starts_with("    x | y  or"), "{lines:?}");
}

#[test]
fn a_summary_is_what_the_first_heading_says_after_its_dash() {
    assert_eq!(summary("# man — read the **manual**\n\nbody", "man"), "read the manual");
    assert_eq!(summary("# Syscall ABI\n", "syscalls"), "Syscall ABI");
    assert_eq!(summary("no heading", "x"), "");
}

/// A spec's heading names its subject before the dash, not the page: all of it is the summary, so
/// a search for its subject finds it.
#[test]
fn a_heading_that_does_not_name_its_page_is_its_whole_summary() {
    let page = "# RS Protocol — Auth operations\n\nbody";
    assert_eq!(summary(page, "rsproto-auth"), "RS Protocol — Auth operations");
    assert!(mentions("rsproto-auth", &summary(page, "rsproto-auth"), "protocol"));
}

/// Runs `f` on a thread of its own and waits `secs` seconds for it, so a renderer that loops on
/// `what` fails the test rather than hangs it, and one that panics on it says so.
fn within<T: Send + 'static>(what: &str, secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
    use std::sync::mpsc::RecvTimeoutError;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(std::time::Duration::from_secs(secs)) {
        Ok(done) => done,
        Err(RecvTimeoutError::Timeout) => panic!("{what} was not rendered within {secs} seconds"),
        Err(RecvTimeoutError::Disconnected) => panic!("the renderer panicked on {what}"),
    }
}

/// Every page in the manual's sections, found as they are staged, and every spec, staged or not,
/// at the width a terminal is assumed to have: no line wider, but a code block's, which is never
/// wrapped. A directory that cannot be read, or holds no page, fails rather than going unchecked.
#[test]
#[cfg_attr(miri, ignore = "renders every page and spec: tens of minutes interpreted, past its ten-second guard")]
fn every_page_and_spec_renders_within_eighty_columns() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    let manual = root.join("manual");
    let mut dirs: Vec<std::path::PathBuf> = std::fs::read_dir(&manual)
        .unwrap_or_else(|e| panic!("{}: {e}", manual.display()))
        .map(|e| e.expect("an entry of docs/manual").path())
        .filter(|d| d.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.parse::<u8>().is_ok()))
        .collect();
    assert!(!dirs.is_empty(), "{} holds no section", manual.display());
    dirs.push(root.join("spec"));
    let mut pages = Vec::new();
    for dir in &dirs {
        let before = pages.len();
        for e in std::fs::read_dir(dir).unwrap_or_else(|e| panic!("{}: {e}", dir.display())) {
            let path = e.unwrap_or_else(|e| panic!("{}: {e}", dir.display())).path();
            if path.extension().is_some_and(|x| x == "md") {
                pages.push(path);
            }
        }
        assert!(pages.len() > before, "{} holds no page", dir.display());
    }
    let mut wide = Vec::new();
    for p in &pages {
        let text = std::fs::read_to_string(p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        for l in within(&p.display().to_string(), 10, move || render_lines(&text, 80, false)) {
            if !l.code && l.text.chars().count() > 80 {
                wide.push(alloc::format!("{}: {}", p.display(), l.text));
            }
        }
    }
    assert!(wide.is_empty(), "{} lines wider than 80:\n{}", wide.len(), wide.join("\n"));
}

/// A line that starts like a block but is not one, as a `|` line with no table under it, is a
/// paragraph's: the shell spec's took the parser round a loop that never moved.
#[test]
fn a_pipe_line_with_no_table_under_it_is_a_paragraph() {
    let lines =
        within("a `|` line with no table under it", 5, || plain("Before.\n\n| not a table\nstill the paragraph\n", 40));
    assert_eq!(lines, ["    Before.", "", "    | not a table still the paragraph"]);
}

#[test]
fn a_table_or_a_fence_under_a_list_item_is_drawn_as_one() {
    let page = "- **Categories**:\n\n  | Kind | Example |\n  |---|---|\n  | keyword | `if` |\n\n- after\n\n  ```\n  code\n  ```\n";
    let lines = plain(page, 60);
    assert!(lines.contains(&String::from("    Kind     Example")), "{lines:?}");
    assert!(lines.contains(&String::from("    keyword  if")), "{lines:?}");
    assert!(!lines.iter().any(|l| l.contains('|')), "no row read as text: {lines:?}");
    assert!(render_lines(page, 60, false).iter().any(|l| l.code && l.text.trim() == "code"), "{lines:?}");
}

#[test]
fn a_name_means_the_page_in_its_section_or_the_first_in_order() {
    let pages: Vec<(u8, String)> =
        [(7, "shell"), (1, "list"), (5, "init.toml"), (8, "services"), (1, "services"), (7, "storage"), (8, "storage")]
            .iter()
            .map(|(s, n)| (*s, String::from(*n)))
            .collect();
    assert_eq!(choose(&pages, None, "list"), Some(1));
    assert_eq!(choose(&pages, None, "services"), Some(4), "section 1 before 8");
    assert_eq!(choose(&pages, Some(8), "services"), Some(3), "or the one named");
    assert_eq!(choose(&pages, Some(1), "shell"), None, "not in that section");
    assert_eq!(choose(&pages, None, "storage"), Some(6), "a service before an overview");
    // The rest of the order, a pair at a time: 8 before 2, 2 before 5, 5 before 7.
    let pair = |a: u8, b: u8| [(a, String::from("x")), (b, String::from("x"))];
    assert_eq!(choose(&pair(2, 8), None, "x"), Some(1), "8 before 2");
    assert_eq!(choose(&pair(5, 2), None, "x"), Some(1), "2 before 5");
    assert_eq!(choose(&pair(7, 5), None, "x"), Some(1), "5 before 7");
    assert_eq!(choose(&pages, None, "nothing"), None);
}

#[test]
fn a_keyword_is_looked_for_in_the_name_and_the_summary_case_aside() {
    assert!(mentions("list", "a directory as a table", "TABLE"));
    assert!(mentions("list", "A Directory as a TABLE", "table"), "the summary's case aside too");
    assert!(mentions("Init.toml", "", "init"));
    assert!(!mentions("list", "a directory", "file"));
}

#[test]
fn a_page_file_names_its_page_and_its_section() {
    assert_eq!(page_of("man.1.md"), Some((String::from("man"), 1)));
    assert_eq!(page_of("init.toml.5.md"), Some((String::from("init.toml"), 5)), "a name with a dot");
    for not in ["man.md", "man.x.md", ".1.md", "man.1.txt", "man.1"] {
        assert_eq!(page_of(not), None, "{not}");
    }
}

#[test]
fn a_request_is_read_from_the_operands_or_refused() {
    assert_eq!(request(&[], false), Ok(Request::List));
    assert_eq!(request(&["list"], false), Ok(Request::Page(None, "list")));
    assert_eq!(request(&["5", "init.toml"], false), Ok(Request::Page(Some(5), "init.toml")));
    assert_eq!(request(&["table"], true), Ok(Request::Keyword("table")));
    assert!(request(&["x", "list"], false).is_err_and(|e| e.contains("SECTION is a number")));
    assert!(request(&["1", "2", "3"], false).is_err_and(|e| e.contains("too many")));
    for ops in [&[][..], &["a", "b"][..]] {
        assert!(request(ops, true).is_err_and(|e| e.contains("one WORD")), "{ops:?}");
    }
}

#[test]
fn a_page_that_cannot_be_read_says_so_rather_than_that_it_is_not_there() {
    let found = |section, name: &str, text: Result<&str, &str>| Found {
        section,
        name: String::from(name),
        text: text.map(String::from).map_err(String::from),
    };
    let pages = [found(1, "list", Ok("# list — a directory\n\nBody.\n")), found(2, "syscalls", Err("TooLarge"))];
    assert!(answer(&pages, None, "list", 80).is_ok_and(|p| p.contains("Body.")));
    assert_eq!(
        answer(&pages, None, "syscalls", 80),
        Err(String::from("man: syscalls(2) could not be read: TooLarge\n"))
    );
    assert_eq!(answer(&pages, None, "nothing", 80), Err(String::from("man: no manual page for nothing\n")));
    assert_eq!(pages[1].summary(), "(could not be read: TooLarge)", "and listed as such");
    assert_eq!(pages[0].summary(), "a directory");
}

/// Markdown the renderer does not model is drawn as the text it is, not dropped.
#[test]
fn what_is_not_markup_is_kept_as_text() {
    let one = |page: &str| plain(page, 80).join("\n");
    assert_eq!(one("Glob *.md and *.rs files.\n"), "    Glob *.md and *.rs files.");
    assert_eq!(one("Raise 2 ** 3 here.\n"), "    Raise 2 ** 3 here.");
    assert_eq!(one("# Using C#\n"), "Using C#");
    assert_eq!(one("# Closed ##\n"), "Closed");
    assert_eq!(one("Index [1, 2][0] here.\n"), "    Index [1, 2][0] here.");
    assert_eq!(one("Hello![x and more.\n"), "    Hello![x and more.");
    assert_eq!(one("<!-- note --> Text after.\n"), "    Text after.");
    let table = plain("| A | B |\n|---|---|\n| 1 | 2 | 3 |\n| ``a | b`` | x |\n", 80);
    assert!(table.iter().any(|l| l.trim_end().ends_with('3')), "an extra cell: {table:?}");
    assert!(table.iter().any(|l| l.contains("a | b")), "a pipe in a double-backtick span: {table:?}");
    let fenced = render_lines("````\n```\ninside\n````\n", 80, false);
    assert_eq!(fenced.iter().filter(|l| l.code).count(), 2, "a shorter fence inside a longer one: {fenced:?}");
}

/// A page is text: what a terminal would act on rather than draw never reaches it.
#[test]
fn a_page_carries_no_control_character_to_the_terminal() {
    let page = "Hello \x1b[2J\x1b]0;title\x07 there `\x1b[31mred` x.\n\n```\ncode\x1b[H\there\n```\n";
    for styled in [false, true] {
        let out = render(page, 80, styled);
        let stray: String = out.replace("\x1b[0m", "").replace("\x1b[36m", "");
        assert!(!stray.chars().any(|c| c.is_control() && c != '\n'), "styled {styled}: {out:?}");
    }
    assert!(render(page, 80, false).contains("code[H    here"), "a tab is spaces and the rest is drawn");
}

/// A word made of several runs, as `(`a/b.rs`)` is, moves to the next line whole.
#[test]
fn a_line_never_breaks_inside_a_word_made_of_runs() {
    let lines = plain("Some words then (`x/y/z.rs`) and more.\n", 4 + 20);
    assert_eq!(lines, ["    Some words then", "    (x/y/z.rs) and more."], "{lines:?}");
}
