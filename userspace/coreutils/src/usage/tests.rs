use super::*;
use alloc::string::ToString;

/// Whether `text` starts with `spelling` as a whole word: `--mount` starts `--mount DEVICE`, and
/// not `--mountpoint`.
fn starts_with_word(text: &str, spelling: &str) -> bool {
    text.strip_prefix(spelling).is_some_and(|rest| !rest.starts_with(|c: char| c.is_alphanumeric() || c == '-'))
}

/// `--help` fits a serial line's 80 columns and has a row for every flag the program parses: a
/// line that starts with the flag. The usage line names most of them too, so a mention is not a row.
#[test]
fn every_programs_help_fits_and_names_its_flags() {
    for u in PROGRAMS {
        let help = u.help();
        assert!(help.lines().all(|l| l.chars().count() <= 80), "{}:\n{help}", u.name);
        // A flag's row is indented two, and four more where it has no short spelling.
        let rows: alloc::vec::Vec<&str> =
            help.lines().filter(|l| l.starts_with("  -") || l.starts_with("      --")).map(str::trim_start).collect();
        for f in u.flags.iter().chain(crate::args::UNIVERSAL_FLAGS.iter()) {
            let long = format!("--{}", f.long);
            let short = format!("-{}, {long}", f.short);
            let row =
                rows.iter().any(|r| starts_with_word(r, &long) || (f.short != '\0' && starts_with_word(r, &short)));
            assert!(row, "{} {long} has no row:\n{help}", u.name);
        }
        assert!(u.about.starts_with(&format!("usage: {}", u.name)), "{}", u.name);
    }
}

/// The flags line up after the widest spelling, a flag's arguments beside it, and a long line of
/// help wraps under its own column.
#[test]
fn help_lines_the_flags_up_and_wraps_their_help() {
    assert_eq!(
        LIST.help(),
        "usage: list [--recursive] [PATH...]\n\nList each PATH, or the working directory, as a typed \
         stream (TSM1) on stdout:\nTable<{name: String, size: Int, kind: String, modified: Int}>\n\n  \
         -r, --recursive  list subdirectories recursively\n      --help       show this help and \
         exit\n      --version    show version information and exit\n"
    );
    let disk = DISK.help();
    let lines: alloc::vec::Vec<&str> = disk.lines().collect();
    let mount = lines.iter().position(|l| l.starts_with("      --mount DEVICE [LABEL]  mount ")).expect("mount");
    let column = lines[mount].find("mount DEVICE (").expect("its help");
    let next = lines[mount + 1];
    assert_eq!(next.len() - next.trim_start().len(), column, "wrapped under its column:\n{disk}");
    assert_eq!(LIST.version(), "list (nitrox coreutils) 0.1.0\n".to_string());
}

/// The source of the program `name`, as the build compiles it.
fn source(name: &str) -> alloc::string::String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("src/bin/{name}.rs"));
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Every flag a program parses has its own bullet in its page's Options: a flag added without its
/// paragraph fails here, though another bullet mention it in passing.
#[test]
fn every_flag_a_program_parses_is_on_its_page() {
    let manual = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/manual/1");
    let mut missing = alloc::vec::Vec::new();
    for u in PROGRAMS {
        let page = std::fs::read_to_string(manual.join(format!("{}.md", u.name))).unwrap_or_default();
        let options = page.split("\n## Options\n").nth(1).map_or("", |rest| rest.split("\n## ").next().unwrap_or(rest));
        // What each bullet is about: its spellings, before the `:` that starts its paragraph.
        let terms: alloc::vec::Vec<&str> = options
            .lines()
            .filter_map(|l| l.strip_prefix("- `"))
            .filter_map(|l| l.split_once("`: "))
            .map(|(t, _)| t)
            .collect();
        for f in u.flags {
            let long = format!("--{}", f.long);
            let named = terms.iter().any(|t| t.split("`, `").any(|s| starts_with_word(s, &long)));
            if !named {
                missing.push(format!("{} {long}", u.name));
            }
        }
        if page.is_empty() {
            missing.push(format!("{}: no page", u.name));
        }
    }
    assert!(missing.is_empty(), "{missing:?}");
}

/// A program's flags are the ones its table declares, both ways: one it asked for and did not
/// declare would never be set, as `parse` refuses it, and one it declared and never asked for is
/// shown and documented and does nothing. `with` reads its own, and is held below.
#[test]
fn every_flag_a_program_asks_for_is_in_its_table() {
    let mut asked = 0;
    for u in PROGRAMS {
        let src = source(u.name);
        let mut read = alloc::collections::BTreeSet::new();
        for (i, _) in src.match_indices(".has(\"") {
            let name = &src[i + 6..][..src[i + 6..].find('"').expect("closed")];
            assert!(u.flags.iter().any(|f| f.long == name), "{} asks for --{name}", u.name);
            read.insert(name);
            asked += 1;
        }
        if u.name != "with" {
            let unread = u.flags.iter().find(|f| !read.contains(f.long));
            assert!(unread.is_none(), "{} declares --{} and never asks for it", u.name, unread.unwrap().long);
        }
    }
    assert!(asked > 20, "{asked} flags asked for: the scan has missed the programs' form");
}

/// Each program parses and answers `--help` and `--version` from its own table, and names no
/// other's: a table borrowed from another program would pass every test above.
#[test]
fn every_program_uses_its_own_table() {
    let consts: alloc::vec::Vec<alloc::string::String> = PROGRAMS.iter().map(|u| u.name.to_uppercase()).collect();
    for (u, own) in PROGRAMS.iter().zip(&consts) {
        let src = source(u.name);
        let calls: &[&str] =
            if u.name == "with" { &["help()", "version()"] } else { &["parse(", "help()", "version()"] };
        for call in calls {
            assert!(src.contains(&format!("{own}.{call}")), "{} does not call {own}.{call}", u.name);
        }
        // The whole name before each `.`, read once a source: another program's table is a use.
        for (i, _) in src.match_indices('.') {
            let name = &src[src[..i].trim_end_matches(|c: char| c.is_alphanumeric() || c == '_').len()..i];
            assert!(name == own || !consts.iter().any(|c| c == name), "{} uses {name}", u.name);
        }
    }
}

/// `with` reads its options by hand, as the first word, so its table is held to the arms that read
/// them, both ways.
#[test]
fn withs_table_is_the_options_it_reads() {
    let src = source("with");
    let arms: alloc::collections::BTreeSet<&str> =
        src.match_indices("Some(\"--").map(|(i, _)| &src[i + 8..][..src[i + 8..].find('"').expect("closed")]).collect();
    let table: alloc::collections::BTreeSet<&str> =
        WITH.flags.iter().map(|f| f.long).chain(["help", "version"]).collect();
    assert_eq!(arms, table);
}
