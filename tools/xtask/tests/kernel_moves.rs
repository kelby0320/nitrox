use std::path::{Path, PathBuf};

/// Each call in `text` from `name` to the parenthesis closing it.
fn calls<'a>(text: &'a str, name: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    for (at, _) in text.match_indices(name) {
        let mut depth = 0;
        for (i, c) in text[at + name.len() - 1..].char_indices() {
            match c {
                '(' => depth += 1,
                ')' => depth -= 1,
                _ => {}
            }
            if depth == 0 {
                out.push(&text[at..at + name.len() + i]);
                break;
            }
        }
    }
    out
}

/// The kernel's source files, its tests left out.
fn kernel_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            if !p.ends_with("tests") {
                kernel_sources(&p, out);
            }
        } else if p.extension().is_some_and(|e| e == "rs") && !p.ends_with("tests.rs") {
            out.push(p);
        }
    }
}

/// **A handle moves by a claim**: two moves of one handle at once would otherwise both pass their
/// lookups, a copy without `DUPLICATE`. So a lookup asking for `TRANSFER` is a copy, asking for
/// `DUPLICATE` too.
#[test]
fn a_lookup_for_transfer_is_a_copy_and_a_move_is_a_claim() {
    let mut files = Vec::new();
    kernel_sources(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../../kernel/src"), &mut files);
    let (mut copies, mut moves) = (0, 0);
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap();
        for call in calls(&text, ".lookup(").into_iter().filter(|c| c.contains("TRANSFER")) {
            copies += 1;
            assert!(call.contains("DUPLICATE"), "{}: a move by lookup: {call}", path.display());
        }
        moves += calls(&text, ".claim(").iter().filter(|c| c.contains("TRANSFER")).count();
    }
    assert!(copies >= 1 && moves >= 2, "{copies} copies, {moves} moves: the scan no longer sees spawn and send");
}

#[test]
fn a_call_is_read_to_its_closing_parenthesis() {
    let text = "a.lookup(h, f(x), Rights::TRANSFER).map(|v| v)";
    assert_eq!(calls(text, ".lookup("), [".lookup(h, f(x), Rights::TRANSFER)"]);
}
