//! The manual staged onto the root, at `/system/manual/<section>/<name>.md`: `docs/manual`'s
//! pages, and the specs that are pages too.

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

/// Stage every page under `staging`'s `/system/manual`.
pub(crate) fn stage_manual(staging: &Path) -> R<()> {
    let root = staging.join("system").join("manual");
    let pages = pages()?;
    for (section, name, from) in &pages {
        let dir = root.join(section.to_string());
        fs::create_dir_all(&dir)?;
        fs::copy(from, dir.join(format!("{name}.md"))).map_err(|e| format!("stage {}: {e}", from.display()))?;
    }
    println!("xtask: seeded /system/manual ({} pages)", pages.len());
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

    #[test]
    fn every_page_has_a_section_man_reads_and_a_name_of_its_own() {
        let pages = pages().unwrap();
        assert!(pages.iter().any(|p| p.1 == "man" && p.0 == 1), "man(1) is there");
        for (section, name, _) in &pages {
            assert!([1, 2, 5, 7, 8].contains(section), "{name} is in section {section}, which man does not read");
        }
    }
}
