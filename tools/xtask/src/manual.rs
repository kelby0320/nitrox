//! The manual staged into the coreutils package's `manual/`, as `<name>.<section>.md`: `docs/manual`'s
//! pages, and the specs that are pages too. The profile server projects it, read-only, at
//! `/system/manual`.

use crate::*;

/// The specs staged as pages, as they are: the file under `docs/spec`, its section, and its name
/// in the manual. **A spec is here or in the tests' `NOT_STAGED`**, which they hold, so a new one
/// is a decision rather than an omission.
pub(crate) const STAGED_SPECS: &[(&str, u8, &str)] = &[
    ("syscall-abi.md", 2, "syscalls"),
    ("handle-encoding.md", 2, "handles"),
    ("io-operation.md", 2, "io"),
    ("notification-format.md", 2, "notifications"),
    ("ipc-message-format.md", 2, "ipc"),
    ("process-spawn-args.md", 2, "spawn"),
    ("thread-args.md", 2, "threads"),
    ("init-toml-schema.md", 5, "init.toml"),
    ("service-toml-schema.md", 5, "services.toml"),
    ("theme-toml-schema.md", 5, "theme.toml"),
    ("views-toml-schema.md", 5, "views.toml"),
    ("desktop-entry.md", 5, "desktop-entry"),
    ("typed-stream-format.md", 5, "typed-stream"),
    ("manual-page.md", 5, "manual-page"),
    ("shell-language.md", 7, "shell-language"),
    ("pipeline-stdio.md", 7, "pipeline-stdio"),
    ("device-node.md", 7, "device-nodes"),
    ("rsproto-wire-format.md", 7, "rsproto"),
    ("rsproto-auth-ops.md", 7, "rsproto-auth"),
    ("rsproto-block-ops.md", 7, "rsproto-block"),
    ("rsproto-desktop-ops.md", 7, "rsproto-desktop"),
    ("rsproto-devices-ops.md", 7, "rsproto-devices"),
    ("rsproto-file-ops.md", 7, "rsproto-file"),
    ("rsproto-input-ops.md", 7, "rsproto-input"),
    ("rsproto-log-ops.md", 7, "rsproto-log"),
    ("rsproto-namespace-ops.md", 7, "rsproto-namespace"),
    ("rsproto-services-ops.md", 7, "rsproto-services"),
    ("rsproto-storage-ops.md", 7, "rsproto-storage"),
    ("rsproto-surface-ops.md", 7, "rsproto-surface"),
    ("rsproto-tty-ops.md", 7, "rsproto-tty"),
    ("rsproto-views-ops.md", 7, "rsproto-views"),
];

/// Every page the manual holds: its section, its name, and the file it is staged from.
pub(crate) fn pages() -> R<Vec<(u8, String, PathBuf)>> {
    let docs = repo_root().join("docs");
    let mut out = Vec::new();
    for entry in fs::read_dir(docs.join("manual"))? {
        let dir = entry?.path();
        let Some(section) = dir.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse::<u8>().ok()) else {
            continue;
        };
        for page in fs::read_dir(&dir)? {
            let path = page?.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_suffix(".md")) {
                out.push((section, name.to_string(), path.clone()));
            }
        }
    }
    for &(spec, section, name) in STAGED_SPECS {
        out.push((section, name.to_string(), docs.join("spec").join(spec)));
    }
    out.sort();
    for pair in out.windows(2) {
        if pair[0].0 == pair[1].0 && pair[0].1 == pair[1].1 {
            return Err(format!("two pages named {} in section {}", pair[0].1, pair[0].0).into());
        }
    }
    Ok(out)
}

/// Every page's staged name and text, for the hash of the package that carries them.
pub(crate) fn manual_bytes() -> R<Vec<u8>> {
    let mut out = Vec::new();
    for (section, name, from) in pages()? {
        out.extend(format!("{name}.{section}.md\0").bytes());
        out.extend(fs::read(&from).map_err(|e| format!("read {}: {e}", from.display()))?);
    }
    Ok(out)
}

/// Stage every page into `dir`, a package's `manual/`, flat: the profile server's projections hold
/// no directories.
pub(crate) fn stage_manual(dir: &Path) -> R<()> {
    let pages = pages()?;
    fs::create_dir_all(dir)?;
    for (section, name, from) in &pages {
        let to = dir.join(format!("{name}.{section}.md"));
        fs::copy(from, to).map_err(|e| format!("stage {}: {e}", from.display()))?;
    }
    println!("xtask: store package {} ({} pages)", dir.display(), pages.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The specs that are not pages, and why: the kernel's own, which no program sees.
    const NOT_STAGED: &[(&str, &str)] = &[
        ("abi-version-hash.md", "the kernel's module ABI"),
        ("irp-layout.md", "the kernel's I/O request layout"),
        ("user-memory-access.md", "how the kernel reaches user memory"),
    ];

    #[test]
    fn every_spec_is_a_page_or_said_not_to_be() {
        let mut undecided = Vec::new();
        for entry in fs::read_dir(repo_root().join("docs/spec")).unwrap() {
            let name = entry.unwrap().file_name().into_string().unwrap();
            let staged = STAGED_SPECS.iter().any(|s| s.0 == name);
            let not = NOT_STAGED.iter().any(|s| s.0 == name);
            assert!(!(staged && not), "{name} is both staged and not");
            if !staged && !not {
                undecided.push(name);
            }
        }
        assert!(undecided.is_empty(), "staged nowhere and not said why: {undecided:?}");
        for (spec, _, _) in STAGED_SPECS {
            assert!(repo_root().join("docs/spec").join(spec).exists(), "{spec} is gone");
        }
    }

    /// Every program in `/bin` has a page in section 1, where a person who ran it looks. No service
    /// is in `/bin`; one there would be described on the services' page instead.
    #[test]
    fn every_program_in_bin_has_a_page() {
        let programs = crate::profile_programs();
        let missing: Vec<&str> =
            programs.iter().copied().filter(|p| !repo_root().join(format!("docs/manual/1/{p}.md")).exists()).collect();
        assert!(programs.len() > 20 && missing.is_empty(), "no page in section 1: {missing:?}");
    }

    /// The text of the `HELP` a program's source declares, from `const HELP` to the `;` ending it.
    fn help_const(src: &str) -> Option<&str> {
        let at = src.find("const HELP")?;
        Some(&src[at..][..src[at..].find(";\n")?])
    }

    /// The flags `text` names: each `--word`, and each `-x` standing alone, that starts a word.
    fn flags_named(text: &str) -> std::collections::BTreeSet<String> {
        let b = text.as_bytes();
        let mut out = std::collections::BTreeSet::new();
        for i in (0..b.len())
            .filter(|&i| b[i] == b'-' && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'-')))
        {
            let word =
                |from: usize| from + b[from..].iter().take_while(|c| c.is_ascii_lowercase() || **c == b'-').count();
            if b.get(i + 1) == Some(&b'-') && b.get(i + 2).is_some_and(u8::is_ascii_lowercase) {
                out.insert(text[i..word(i + 2)].to_string());
            } else if b.get(i + 1).is_some_and(u8::is_ascii_lowercase)
                && !b.get(i + 2).is_some_and(u8::is_ascii_alphanumeric)
            {
                out.insert(text[i..i + 2].to_string());
            }
        }
        out
    }

    /// The spellings each bullet of a page's Options is about: before the `:` that starts its
    /// paragraph, each code span's first word.
    fn options_spellings(page: &str) -> Vec<String> {
        let options = page.split("\n## Options\n").nth(1).map_or("", |rest| rest.split("\n## ").next().unwrap_or(rest));
        let terms = options.lines().filter_map(|l| l.strip_prefix("- `")).filter_map(|l| l.split_once("`: "));
        terms.flat_map(|(t, _)| t.split("`, `").map(|s| s.split(' ').next().unwrap_or(s).to_string())).collect()
    }

    /// Every flag a program outside the coreutils names in its `--help` has its own bullet in its
    /// page's Options. The coreutils' flags are their tables', held by the tables' tests.
    #[test]
    fn every_flag_a_programs_help_names_is_on_its_page() {
        let mut named = 0;
        let mut missing = Vec::new();
        for p in crate::profile_programs().into_iter().filter(|p| !crate::build::COREUTILS.contains(p)) {
            let dir = repo_root().join(format!("userspace/{p}/src"));
            let sources: Vec<String> =
                ["main.rs", "lib.rs"].iter().filter_map(|f| fs::read_to_string(dir.join(f)).ok()).collect();
            let Some(help) = sources.iter().find_map(|s| help_const(s)) else {
                missing.push(format!("{p}: no HELP"));
                continue;
            };
            let page = fs::read_to_string(repo_root().join(format!("docs/manual/1/{p}.md"))).unwrap_or_default();
            let spellings = options_spellings(&page);
            for flag in flags_named(help) {
                named += 1;
                if !spellings.contains(&flag) {
                    missing.push(format!("{p} {flag}"));
                }
            }
        }
        assert!(missing.is_empty(), "{missing:?}");
        assert!(named >= 6, "{named} flags found: the scan has missed the programs' HELP");
    }

    #[test]
    fn a_flag_is_read_where_it_starts_a_word() {
        let named = flags_named("usage: nxsh [-c SOURCE]\n\x20 -h, --help  read-only, a --- and -- `x`, -cx");
        assert_eq!(named.into_iter().collect::<Vec<_>>(), ["--help", "-c", "-h"]);
    }

    /// One flat directory of `<name>.<section>.md`, as the profile server projects and `man` reads.
    #[test]
    fn the_pages_are_staged_flat_by_name_and_section() {
        let dir = std::env::temp_dir().join(format!("xtask-manual-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        stage_manual(&dir).unwrap();
        let mut names: Vec<String> =
            fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        names.sort();
        assert_eq!(names.len(), pages().unwrap().len(), "one file a page, and no directory");
        for want in ["man.1.md", "init.toml.5.md", "syscalls.2.md", "intro.7.md"] {
            assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
        }
        assert_eq!(
            fs::read(dir.join("man.1.md")).unwrap(),
            fs::read(repo_root().join("docs/manual/1/man.md")).unwrap()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    /// A changed page moves the package that carries it, and only that one.
    #[test]
    fn the_manual_is_hashed_into_the_package_that_ships_man() {
        let elf = |b: &str| -> R<Vec<u8>> { Ok(format!("ELF {b}").into_bytes()) };
        let page = fs::read(repo_root().join("docs/manual/1/man.md")).unwrap();
        let has = |bytes: &[u8], what: &[u8]| bytes.windows(what.len()).any(|w| w == what);
        let with_man = package_bytes(&["list", "man"], &elf).unwrap();
        assert!(has(&with_man, &page) && has(&with_man, b"man.1.md\0"), "a page's text and its name");
        let without = package_bytes(&["list", "nxsh"], &elf).unwrap();
        assert!(!has(&without, b"man.1.md\0"), "not a package without man");
    }

    #[test]
    fn every_page_has_a_section_man_reads_and_a_name_of_its_own() {
        let pages = pages().unwrap();
        assert!(pages.iter().any(|p| p.1 == "man" && p.0 == 1), "man(1) is there");
        for (section, name, _) in &pages {
            assert!([1, 2, 5, 7, 8].contains(section), "{name} is in section {section}, which man does not read");
        }
    }
}
