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
    assert_eq!(summary("# man — read the **manual**\n\nbody"), "read the manual");
    assert_eq!(summary("# Syscall ABI\n"), "Syscall ABI");
    assert_eq!(summary("no heading"), "");
}

/// Every page this system stages, at the width a terminal is assumed to have: no line wider, but
/// a code block's, which is never wrapped.
#[test]
fn every_page_and_spec_renders_within_eighty_columns() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs");
    let mut pages = Vec::new();
    for dir in ["manual/1", "manual/2", "manual/5", "manual/7", "manual/8", "spec"] {
        let Ok(entries) = std::fs::read_dir(root.join(dir)) else { continue };
        for e in entries.flatten() {
            if e.path().extension().is_some_and(|x| x == "md") {
                pages.push(e.path());
            }
        }
    }
    assert!(pages.len() > 30, "the specs were found: {}", pages.len());
    let mut wide = Vec::new();
    for p in &pages {
        let text = std::fs::read_to_string(p).expect("readable");
        // On a thread of its own, so a page the renderer loops on fails this rather than hangs it.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(render_lines(&text, 80, false));
        });
        let lines = rx.recv_timeout(std::time::Duration::from_secs(10));
        let lines = lines.unwrap_or_else(|_| panic!("{} was not rendered within ten seconds", p.display()));
        for l in lines {
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
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(plain("Before.\n\n| not a table\nstill the paragraph\n", 40));
    });
    let lines = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the page was rendered, not looped on");
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
    assert_eq!(choose(&pages, None, "nothing"), None);
}

#[test]
fn a_keyword_is_looked_for_in_the_name_and_the_summary_case_aside() {
    assert!(mentions("list", "a directory as a table", "TABLE"));
    assert!(mentions("Init.toml", "", "init"));
    assert!(!mentions("list", "a directory", "file"));
}
