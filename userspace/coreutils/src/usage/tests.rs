use super::*;
use alloc::string::ToString;

/// `--help` fits a serial line's 80 columns and names every flag the program parses.
#[test]
fn every_programs_help_fits_and_names_its_flags() {
    for u in PROGRAMS {
        let help = u.help();
        assert!(help.lines().all(|l| l.chars().count() <= 80), "{}:\n{help}", u.name);
        for long in u.flags.iter().map(|f| f.long).chain(["help", "version"]) {
            assert!(help.contains(&format!("--{long}")), "{} --{long}:\n{help}", u.name);
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

/// Every flag a program parses is in its page's Options, by name: a flag added without its
/// paragraph fails here, though the page mention it in passing.
#[test]
#[cfg_attr(miri, ignore = "reads the pages from the repository")]
fn every_flag_a_program_parses_is_on_its_page() {
    let manual = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/manual/1");
    let mut missing = alloc::vec::Vec::new();
    for u in PROGRAMS {
        let page = std::fs::read_to_string(manual.join(format!("{}.md", u.name))).unwrap_or_default();
        let options = page.split("\n## Options\n").nth(1).map_or("", |rest| rest.split("\n## ").next().unwrap_or(rest));
        for f in u.flags.iter().filter(|f| !options.contains(&format!("`--{}", f.long))) {
            missing.push(format!("{} --{}", u.name, f.long));
        }
        if page.is_empty() {
            missing.push(format!("{}: no page", u.name));
        }
    }
    assert!(missing.is_empty(), "{missing:?}");
}

/// A flag a program asks for is one its table declares: one it did not would never be set, as
/// `parse` refuses it.
#[test]
#[cfg_attr(miri, ignore = "reads the programs' sources")]
fn every_flag_a_program_asks_for_is_in_its_table() {
    let mut asked = 0;
    for u in PROGRAMS {
        let src = source(u.name);
        for (i, _) in src.match_indices(".has(\"") {
            let name = &src[i + 6..][..src[i + 6..].find('"').expect("closed")];
            assert!(u.flags.iter().any(|f| f.long == name), "{} asks for --{name}", u.name);
            asked += 1;
        }
    }
    assert!(asked > 20, "{asked} flags asked for: the scan has missed the programs' form");
}

/// `with` reads its options by hand, as the first word, so its table is held to the arms that read
/// them, both ways.
#[test]
#[cfg_attr(miri, ignore = "reads the program's source")]
fn withs_table_is_the_options_it_reads() {
    let src = source("with");
    let arms: alloc::collections::BTreeSet<&str> =
        src.match_indices("Some(\"--").map(|(i, _)| &src[i + 8..][..src[i + 8..].find('"').expect("closed")]).collect();
    let table: alloc::collections::BTreeSet<&str> =
        WITH.flags.iter().map(|f| f.long).chain(["help", "version"]).collect();
    assert_eq!(arms, table);
}
