//! `cargo xtask check-docs`: links, heading anchors, cited source paths and status lines in the docs.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{read_text, repo_root, R};

/// `cargo xtask check-docs` — the documentation cannot point at things that do not exist.
///
/// Three cheap, mechanical checks. None of them can tell whether a doc's *prose* is true;
/// they exist because the 2026-08-05 consistency pass found that a large share of the
/// drift was not subtle at all. `docs/architecture/overview.md` — the file the root
/// `CLAUDE.md` tells you to read first — linked to five documents that had never been
/// written, and a spec cited a `librsproto` source file that does not exist. A reader who
/// follows a dead reference goes to the source and builds a private model, which is one of
/// the ways the docs and the code came apart in the first place.
///
/// 1. **Link integrity.** Every relative `[text](path.md)` link resolves.
/// 2. **Cited source paths.** A backticked `kernel/…`, `userspace/…` or `tools/…` path in a
///    doc that describes *current behaviour* must exist.
/// 3. **Status lines.** Every `docs/architecture/*.md` carries one, because `CLAUDE.md`
///    promises it and tells readers to trust it over the body's tense.
///
/// **Check 2 is deliberately scoped, and the escape hatch is deliberate too.** It is an
/// *allowlist*, not a denylist: only `docs/spec/`, `docs/reference/`, `docs/architecture/`
/// and `docs/conventions/` are scanned for cited source paths (see `describes_current`).
/// Everything else — `design/`, `archive/`, `planning/`, `rationale/`, the top-level
/// `docs/*.md`, and the `CLAUDE.md` files — is exempt, because those either do not describe
/// current behaviour or record what *was* true: a ticked box reading "Retire
/// `kernel/src/embedded_images.rs` entirely" cites a path absent *because the work
/// succeeded*, and rewriting it would corrupt the record.
///
/// The remaining false positive is the honest forward reference — `user-memory-access.md`
/// says "*When* aarch64 is implemented its primitives live in …" — so a line carrying
/// `<!-- check-docs: allow-missing -->` is exempt anywhere. Both shapes were real findings,
/// not hypotheticals.
pub(crate) fn cmd_check_docs() -> R<()> {
    const ALLOW: &str = "<!-- check-docs: allow-missing -->";
    let root = repo_root();

    // Docs that describe how the system behaves today. `design/` (subsystems not yet
    // built), `archive/` (superseded artifacts) and `planning/` (intent, with checkboxes)
    // are excluded from the source-path check for the reason in the doc comment above.
    let describes_current = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        ["/docs/spec/", "/docs/reference/", "/docs/architecture/", "/docs/conventions/"]
            .iter()
            .any(|d| s.contains(d))
    };

    // Append-only records. Their backticked path mentions describe the world as it was —
    // an entry explaining that `docs/history/decision-log.md` moved has to name the old
    // path — so the cited-path checks are not applied to them, and the alternative was a
    // `check-docs: allow-missing` marker on every future entry that describes a rename.
    // Their markdown *links* are still checked: prose about an old path is a record, but a
    // link is navigation, and a reader will click it.
    let is_record = |p: &Path| {
        let s = p.to_string_lossy().replace('\\', "/");
        s.ends_with("/docs/decision-log.md") || s.contains("/docs/archive/")
    };

    let mut violations: Vec<String> = Vec::new();
    let (mut links, mut paths) = (0usize, 0usize);
    // `(citing file, line, target file, anchor)`, resolved after the walk — a link into
    // *another* document's heading needs that document read, which the per-file pass below
    // cannot do without re-reading it once per link.
    let mut anchor_refs: Vec<(PathBuf, usize, PathBuf, String)> = Vec::new();

    // Every Markdown file git knows or would: the `CLAUDE.md` files and the skills cite doc
    // paths as `docs/` does. Listed by git, not walked, so an ignored review worktree under
    // `.claude/worktrees/` is never read as this repo's docs.
    let md_files = git_md_files(&root)?;
    md_listing_has_root(&md_files, &root)?;

    // Every `*.md` basename in the repo, so a bare backticked filename can be resolved.
    let known_md: Vec<String> =
        md_files.iter().filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from)).collect();

    let mut check = |path: &Path| -> R<()> {
        let text = read_text(path)?;
        let dir = path.parent().unwrap_or(&root).to_path_buf();
        for (i, line) in text.lines().enumerate() {
            let at = |m: &str| format!("{}:{}: {m}", path.display(), i + 1);

            // 1. Relative markdown links to .md files.
            //
            // **Inline code is not markdown.** A document that *quotes* link syntax — this
            // gate's own entry in `decision-log.md` writes ``](#anchor)`` — is talking about
            // links, not making one. Blanking the code spans first is what lets prose describe
            // the syntax without the checker chasing it.
            let masked = mask_code_spans(line);
            let mut rest = masked.as_str();
            while let Some(open) = rest.find("](") {
                let after = &rest[open + 2..];
                let Some(close) = after.find(')') else { break };
                let target = &after[..close];
                rest = &after[close..];
                let file = target.split('#').next().unwrap_or("");
                if target.contains("://") {
                    continue;
                }
                // 1b. `](#anchor)` and `](other.md#anchor)`. A heading link that resolves to
                // nothing is silent in every renderer — it scrolls nowhere — and this gate
                // skipped it for years because it split on `#` and dropped what followed. A
                // Part C link to `#close-0x0925` (the heading is `RequestClose … and Close …`,
                // which slugs to something else entirely) is what found it; four more were
                // already broken, one of them since the section was renamed.
                if let Some((_, anchor)) = target.split_once('#')
                    && !anchor.is_empty()
                    // `#L36` is a GitHub line link, not a heading — it resolves on the web
                    // and nowhere else, and it is deliberate where it appears.
                    && !(anchor.starts_with('L')
                        && anchor[1..].chars().all(|c| c.is_ascii_digit() || c == '-' || c == 'L'))
                {
                    let target_file =
                        if file.is_empty() { path.to_path_buf() } else { dir.join(file) };
                    if target_file.extension().map_or(false, |e| e == "md") {
                        anchor_refs.push((
                            path.to_path_buf(),
                            i + 1,
                            target_file,
                            anchor.to_string(),
                        ));
                    }
                }
                if file.is_empty() || !file.ends_with(".md") {
                    continue;
                }
                links += 1;
                if !dir.join(file).exists() {
                    violations.push(at(&format!("link target does not exist: {file}")));
                }
            }

            // 2a. Backticked `docs/…​.md` references, in every doc. These are prose, not
            // markdown links, so check 1 never sees them — which is exactly how
            // `docs/history/design-doc-v5.1.md` survived in `overview.md` for months
            // while the file was really named `os-design-v5.1.md`.
            if !line.contains(ALLOW) && !is_record(path) {
                for piece in line.split('`').skip(1).step_by(2) {
                    // `docs/spec/rsproto-*.md` names a family, not a file.
                    if !piece.ends_with(".md") || piece.contains('*') || piece.contains(' ') {
                        continue;
                    }
                    if piece.starts_with("docs/") {
                        paths += 1;
                        if !root.join(piece).exists() {
                            violations.push(at(&format!("cited doc does not exist: {piece}")));
                        }
                    } else if !piece.contains('/') {
                        // A bare filename, e.g. ``see `desktop-shell.md` ``. Renaming a doc
                        // leaves these behind — the `docs/…`-prefixed form gets swept and
                        // the bare one does not. Dissolving `docs/history/` left 11 of them.
                        paths += 1;
                        if !known_md.iter().any(|k| k == piece) {
                            violations.push(at(&format!(
                                "cited doc does not exist anywhere in the repo: {piece}"
                            )));
                        }
                    }
                }
            }

            // 2. Backticked repo-relative source paths, in current-behaviour docs only.
            if describes_current(path) && !line.contains(ALLOW) {
                for piece in line.split('`').skip(1).step_by(2) {
                    let ok_prefix = ["kernel/", "userspace/", "tools/"]
                        .iter()
                        .any(|p| piece.starts_with(p));
                    let ok_suffix = piece.ends_with(".rs") || piece.ends_with(".toml");
                    // `kernel/src/arch/<arch>/registers.rs` is a template, not a path.
                    let placeholder = piece.contains('<') || piece.contains('>');
                    if !ok_prefix || !ok_suffix || piece.contains(' ') || placeholder {
                        continue;
                    }
                    paths += 1;
                    if !root.join(piece).exists() {
                        violations.push(at(&format!(
                            "cited source path does not exist: {piece} \
                             (if the reference is deliberate, mark the line `{ALLOW}`)"
                        )));
                    }
                }
            }
        }
        Ok(())
    };

    for p in &md_files {
        check(p)?;
    }

    // 1b (continued). Resolve the collected heading links, reading each target once.
    let mut anchor_cache: std::collections::BTreeMap<PathBuf, Vec<String>> =
        std::collections::BTreeMap::new();
    let anchors = anchor_refs.len();
    for (src, line, target, anchor) in anchor_refs {
        if !target.exists() {
            continue; // already reported by check 1 as a missing link target
        }
        let known = match anchor_cache.get(&target) {
            Some(a) => a,
            None => {
                let a = heading_anchors(&read_text(&target)?);
                anchor_cache.entry(target.clone()).or_insert(a)
            }
        };
        if !known.iter().any(|k| k == &anchor) {
            let near = known
                .iter()
                .find(|k| k.starts_with(anchor.split('-').next().unwrap_or(&anchor)))
                .map(|k| format!(" (did you mean #{k}?)"))
                .unwrap_or_default();
            violations.push(format!(
                "{}:{line}: link to a heading that does not exist: #{anchor}{near}",
                src.display()
            ));
        }
    }

    // 3. Every architecture and design doc states what is actually built. `design/` needs
    //    it most: those documents describe subsystems with no code behind them.
    let mut arch_docs = 0usize;
    for sub in ["architecture", "design"] {
        let dir = root.join("docs").join(sub);
        if !dir.exists() {
            continue;
        }
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().map_or(false, |e| e == "md") {
                arch_docs += 1;
                let text = read_text(&path)?;
                let has_status = text.lines().take(20).any(|l| {
                    let t = l
                        .trim_start_matches('#')
                        .trim_start_matches("> ")
                        .trim_start_matches('*')
                        .trim_start()
                        .to_lowercase();
                    t.starts_with("status")
                });
                if !has_status {
                    violations.push(format!(
                        "{}: no Status line in the first 20 lines — every architecture and \
                         design doc must state what is actually built (root CLAUDE.md \
                         promises this)",
                        path.display()
                    ));
                }
            }
        }
    }

    // 4. The two hand-maintained numeric tables must agree with the kernel.
    //
    // `abi-sync-check` ties the kernel to `libkern`; nothing tied either to the docs, so
    // `syscall-abi.md` and `error-codes.md` could drift from both and no gate would notice.
    // They happen to agree today (37 syscalls, 16 error codes, checked 2026-08-05) — this
    // keeps it that way, for the same reason `abi-sync-check` exists.
    let mut table_stats: Vec<String> = Vec::new();
    for (what, src_rel, doc_rel) in [
        ("syscall", "kernel/src/syscall/table.rs", "docs/spec/syscall-abi.md"),
        ("error code", "kernel/src/syscall/error.rs", "docs/reference/error-codes.md"),
    ] {
        let src_text = read_text(&root.join(src_rel))?;
        let doc_text = read_text(&root.join(doc_rel))?;

        // `12`, or `0xFFFF_0000` for the debug-only range.
        let parse_num = |t: &str| -> Option<i64> {
            let t = t.trim().trim_end_matches([';', ',']).trim();
            match t.strip_prefix("0x") {
                Some(hex) => i64::from_str_radix(&hex.replace('_', ""), 16).ok(),
                None => t.parse::<i64>().ok(),
            }
        };

        let mut from_src: Vec<(i64, String)> = Vec::new();
        let mut declared = 0usize;
        if what == "syscall" {
            // `pub const SYS_FOO_BAR: u64 = 12;` → (12, "sys_foo_bar")
            for line in src_text.lines() {
                let Some(rest) = line.trim().strip_prefix("pub const SYS_") else { continue };
                declared += 1;
                let Some((name, tail)) = rest.split_once(": u64 = ") else { continue };
                let Some(num) = parse_num(tail) else { continue };
                from_src.push((num, format!("sys_{}", name.to_ascii_lowercase())));
            }
            // Every declared constant must have parsed. Without this, a number the parser
            // cannot read is dropped *silently* — and if the doc omits it too, the two
            // omissions cancel and the check reports agreement it never tested. That is
            // how `0xFFFF_0000` slipped through: 39 constants, 37 compared, green.
            if declared != from_src.len() {
                violations.push(format!(
                    "{src_rel}: {declared} `pub const SYS_` declarations but only {} parsed — \
                     the rest are dropped silently, so the comparison is incomplete. Teach \
                     cmd_check_docs the number format it could not read.",
                    from_src.len()
                ));
            }
        } else {
            // The `KError` enum body only — a stray `= -1` elsewhere must not match.
            let body = match src_text.find("enum KError") {
                Some(a) => {
                    let tail = &src_text[a..];
                    &tail[..tail.find("\n}").unwrap_or(tail.len())]
                }
                None => "",
            };
            for line in body.lines() {
                let Some((name, tail)) = line.trim().split_once(" = ") else { continue };
                if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || name.is_empty() {
                    continue;
                }
                let Some(num) = tail.trim_end_matches(',').trim().parse::<i64>().ok() else {
                    continue;
                };
                from_src.push((num, name.to_string()));
            }
        }

        // Markdown rows: `| `<number>` | `<name>` | …`
        let mut from_doc: Vec<(i64, String)> = Vec::new();
        for line in doc_text.lines() {
            let line = line.trim();

            // The debug-only syscalls are documented as prose bullets, not table rows:
            // ``- `sys_debug_kprint(…) -> isize` (`0xFFFF_0000`) — …``. Read those too,
            // or they are invisible to a table-only parser.
            if what == "syscall" && line.starts_with("- ") {
                let cells: Vec<&str> = line.split('`').skip(1).step_by(2).collect();
                let name = cells.iter().find_map(|c| {
                    c.strip_prefix("sys_")
                        .and_then(|r| r.split_once('('))
                        .map(|(n, _)| format!("sys_{n}"))
                });
                let num = cells.iter().find(|c| c.starts_with("0x")).and_then(|c| parse_num(c));
                if let (Some(name), Some(num)) = (name, num) {
                    from_doc.push((num, name));
                    continue;
                }
            }

            if !line.starts_with('|') {
                continue;
            }
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            let unquote = |c: &str| c.trim_matches('`').trim().to_string();
            if cells.len() < 3 {
                continue;
            }
            let (a, b) = (unquote(cells[1]), unquote(cells[2]));
            let Ok(num) = a.parse::<i64>() else { continue };
            if b.is_empty() || !b.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                continue;
            }
            from_doc.push((num, b));
        }

        from_src.sort();
        from_doc.sort();

        // A checker that parses nothing passes silently. That is a vacuous check, and this
        // project has shipped four vacuous tests; refuse to be the fifth.
        if from_src.is_empty() || from_doc.is_empty() {
            violations.push(format!(
                "{what} table: parsed {} entries from {src_rel} and {} from {doc_rel} — \
                 one side parsed empty, so the comparison proves nothing. The format \
                 changed; update cmd_check_docs.",
                from_src.len(),
                from_doc.len()
            ));
            continue;
        }

        for (n, name) in &from_src {
            match from_doc.iter().find(|(dn, _)| dn == n) {
                None => violations.push(format!(
                    "{doc_rel}: {what} {n} (`{name}`) is defined in {src_rel} but missing \
                     from the table"
                )),
                Some((_, dname)) if dname != name => violations.push(format!(
                    "{doc_rel}: {what} {n} is `{name}` in {src_rel} but `{dname}` in the table"
                )),
                Some(_) => {}
            }
        }
        for (n, name) in &from_doc {
            if !from_src.iter().any(|(sn, _)| sn == n) {
                violations.push(format!(
                    "{doc_rel}: {what} {n} (`{name}`) is in the table but not defined in \
                     {src_rel}"
                ));
            }
        }
        table_stats.push(format!("{} {what}s", from_src.len()));
    }

    let claude_mds: Vec<String> = md_files
        .iter()
        .filter(|p| p.file_name().is_some_and(|n| n == "CLAUDE.md"))
        .filter_map(|p| p.strip_prefix(&root).ok()?.to_str().map(String::from))
        .collect();
    violations.extend(root_claude_md(&read_text(&root.join("CLAUDE.md"))?, &claude_mds));

    if violations.is_empty() {
        println!(
            "check-docs: {links} link(s), {anchors} heading link(s), \
             {paths} cited source path(s), \
             {arch_docs} architecture doc(s) with a Status line, {}, the root CLAUDE.md indexing {} \
             other(s) — all agree ✓",
            table_stats.join(" + "),
            claude_mds.len() - 1
        );
        Ok(())
    } else {
        let mut msg = String::from(
            "documentation must not point at things that do not exist \
             (see cmd_check_docs in tools/xtask/src/docs.rs for why each check exists):\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}

const ROOT_CLAUDE_MD_MAX: usize = 200;

/// Every session loads the root `CLAUDE.md`, so it stays short, and its index — a line
/// "- `dir/CLAUDE.md` — …" each — names every other `CLAUDE.md` git lists, and nothing else.
fn root_claude_md(text: &str, listed: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let lines = text.lines().count();
    if lines > ROOT_CLAUDE_MD_MAX {
        out.push(format!(
            "CLAUDE.md: {lines} lines, over {ROOT_CLAUDE_MD_MAX}: move the detail to the doc it belongs in \
             and leave a pointer"
        ));
    }
    let indexed: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("- `")?.split_once('`').map(|(p, _)| p))
        .filter(|p| p.ends_with("/CLAUDE.md"))
        .collect();
    for p in listed.iter().filter(|p| *p != "CLAUDE.md") {
        if !indexed.contains(&p.as_str()) {
            out.push(format!("CLAUDE.md: `{p}` is missing from its index: add a line saying what it governs"));
        }
    }
    for p in indexed.iter().filter(|p| !listed.iter().any(|l| l == *p)) {
        out.push(format!("CLAUDE.md: its index names `{p}`, which git does not list"));
    }
    out
}

/// `line` with each **closed** inline-code span replaced by a character that cannot appear in a
/// link, so a scan for `](` cannot find one inside backticks.
///
/// **Only closed pairs, and only within this line.** A code span may wrap across lines —
/// ``Priority::RealTime`` does, in `thread-args.md` — which leaves a line holding one unmatched
/// backtick and everything after it looking like code. Masking from an unmatched backtick to the
/// end of the line would swallow the real links that follow it (two of them, both found the
/// moment this was written the other way). An unmatched backtick therefore masks nothing, which
/// errs toward checking a link that was only being quoted rather than skipping one that was not.
fn mask_code_spans(line: &str) -> String {
    let mut out: Vec<char> = line.chars().collect();
    let mut open: Option<usize> = None;
    for i in 0..out.len() {
        if out[i] != '`' {
            continue;
        }
        match open {
            None => open = Some(i),
            Some(start) => {
                for c in out.iter_mut().take(i + 1).skip(start) {
                    *c = '\0';
                }
                open = None;
            }
        }
    }
    out.into_iter().collect()
}

/// Every anchor a markdown document offers: one per heading, plus explicit `<a id="…">`.
///
/// **GitHub's slug rules, because that is where these links are read**: lowercase, keep
/// alphanumerics, underscores and hyphens, turn spaces into hyphens, drop everything else — so
/// `## Transport — no bespoke protocol` is `transport--no-bespoke-protocol`, with *two* hyphens,
/// the em dash having left its two spaces behind. Repeats of one slug are suffixed `-1`, `-2`,
/// as `github-slugger` does. Headings inside fenced code are not headings.
fn heading_anchors(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut counts: Vec<(String, usize)> = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        // An explicit anchor is the escape hatch for a target that is not a heading at all —
        // `syscall-abi.md` uses one to name a bullet in its type list.
        let mut rest = line;
        while let Some(at) = rest.find("<a ") {
            rest = &rest[at + 3..];
            let Some(open) = rest.find(['"']) else { break };
            let key = rest[..open].trim();
            let after = &rest[open + 1..];
            let Some(close) = after.find('"') else { break };
            if key.ends_with("id=") || key.ends_with("name=") {
                out.push(after[..close].to_string());
            }
            rest = &after[close..];
        }

        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !t.starts_with('#') {
            continue;
        }
        let title = t.trim_start_matches('#').trim();
        if title.is_empty() {
            continue;
        }
        let mut slug = String::new();
        for c in title.chars() {
            if c.is_alphanumeric() {
                slug.extend(c.to_lowercase());
            } else if c == ' ' || c == '-' {
                slug.push('-');
            } else if c == '_' {
                slug.push('_');
            }
        }
        let n = match counts.iter_mut().find(|(s, _)| *s == slug) {
            Some((_, n)) => {
                *n += 1;
                *n
            }
            None => {
                counts.push((slug.clone(), 0));
                0
            }
        };
        out.push(if n == 0 { slug } else { format!("{slug}-{n}") });
    }
    out
}

fn git_md_files(root: &Path) -> R<Vec<PathBuf>> {
    let out = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z", "--", "*.md"])
        .current_dir(root)
        .output()?;
    if !out.status.success() {
        return Err(format!("git ls-files: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    Ok(md_paths(root, &out.stdout).into_iter().filter(|p| p.is_file()).collect())
}

/// The paths in `git ls-files -z` output, each once: a file unmerged mid-merge is listed once a stage.
fn md_paths(root: &Path, listing: &[u8]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = listing
        .split(|&b| b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| root.join(String::from_utf8_lossy(p).as_ref()))
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

/// A listing without the root `CLAUDE.md` is not this repository's: run from outside a checkout of
/// it, git lists nothing, and every check would pass on no files.
fn md_listing_has_root(files: &[PathBuf], root: &Path) -> R<()> {
    if files.iter().any(|p| *p == root.join("CLAUDE.md")) {
        return Ok(());
    }
    Err(format!(
        "check-docs: git listed {} Markdown file(s) under {} and not its CLAUDE.md; run it from a git checkout",
        files.len(),
        root.display()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn listed(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn the_root_claude_md_is_at_most_200_lines() {
        let at = "line\n".repeat(200);
        assert!(root_claude_md(&at, &listed(&["CLAUDE.md"])).is_empty());
        let over = format!("{at}one more\n");
        assert_eq!(root_claude_md(&over, &listed(&["CLAUDE.md"])).len(), 1);
    }

    #[test]
    fn the_root_claude_md_indexes_every_other_and_nothing_else() {
        let text = "# CLAUDE.md\n- `kernel/CLAUDE.md` — the kernel\n";
        assert!(root_claude_md(text, &listed(&["CLAUDE.md", "kernel/CLAUDE.md"])).is_empty());
        let missing = root_claude_md(text, &listed(&["CLAUDE.md", "kernel/CLAUDE.md", "tools/CLAUDE.md"]));
        assert!(missing.len() == 1 && missing[0].contains("`tools/CLAUDE.md` is missing"), "{missing:?}");
        let stale = root_claude_md(text, &listed(&["CLAUDE.md"]));
        assert!(stale.len() == 1 && stale[0].contains("names `kernel/CLAUDE.md`"), "{stale:?}");
    }

    #[test]
    fn a_file_listed_once_a_merge_stage_is_checked_once() {
        let root = std::path::Path::new("/r");
        let paths = super::md_paths(root, b"docs/a.md\0x.md\0x.md\0x.md\0");
        assert_eq!(paths, [root.join("docs/a.md"), root.join("x.md")]);
    }

    #[test]
    fn a_listing_without_the_root_claude_md_is_refused() {
        let root = std::path::Path::new("/r");
        assert!(super::md_listing_has_root(&[], root).is_err());
        assert!(super::md_listing_has_root(&[root.join("docs/CLAUDE.md")], root).is_err());
        assert!(super::md_listing_has_root(&[root.join("docs/a.md"), root.join("CLAUDE.md")], root).is_ok());
    }

    #[test]
    fn this_checkout_lists_its_docs_and_none_of_its_build_output() {
        let root = super::repo_root();
        let files = super::git_md_files(&root).unwrap();
        assert!(files.contains(&root.join("CLAUDE.md")));
        assert!(files.contains(&root.join("docs/decision-log.md")));
        assert!(!files.iter().any(|p| p.components().any(|c| c.as_os_str() == "target")));
    }
}
