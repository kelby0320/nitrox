//! `check-deferrals`: every `TODO(tag)` has its entry in `deferred-decisions.md`.

use crate::*;

pub(crate) fn cmd_check_deferrals() -> R<()> {
    let doc_path = repo_root().join("docs").join("rationale").join("deferred-decisions.md");
    let doc = fs::read_to_string(&doc_path).map_err(|e| format!("read {}: {e}", doc_path.display()))?.to_lowercase();

    let mut violations: Vec<String> = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    for ws in ["kernel/src", "userspace", "tools/xtask/src"] {
        let src_root = repo_root().join(ws);
        visit_rs_files_skipping(&src_root, &["target"], &mut |path| {
            let text = fs::read_to_string(path)?;
            for (i, line) in text.lines().enumerate() {
                for tag in markers_in_line(line) {
                    if !tags.iter().any(|t| t == tag) {
                        tags.push(tag.to_string());
                    }
                    if !doc.contains(&format!("todo({})", tag.to_lowercase())) {
                        violations.push(format!(
                            "{}:{}: TODO({tag}) has no entry in deferred-decisions.md",
                            path.display(),
                            i + 1
                        ));
                    }
                }
            }
            Ok(())
        })?;
    }

    // **The other direction**, which the gate did not ask until 2026-08-18. Enforcing only
    // code-to-doc leaves the failure that actually happens unchecked: an entry whose marker was
    // deleted or never written is one nobody trips over while editing the code, which is the
    // whole reason the markers exist. Audit D.5(b) measured 9 of 28 open entries binding to
    // nothing, four of them appearing nowhere else in the repository.
    let raw = fs::read_to_string(&doc_path).map_err(|e| format!("read {}: {e}", doc_path.display()))?;
    let open_tags = open_section_tags(&raw);
    let exempt_count = open_tags.iter().filter(|(_, e)| *e).count();
    for (tag, exempt) in &open_tags {
        if *exempt || tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
            continue;
        }
        violations.push(format!(
            "deferred-decisions.md: TODO({tag}) is an open entry with no TODO({tag}) marker \
             anywhere in kernel/src, userspace or tools/xtask/src — put one where someone \
             changing that code will trip over it, or mark the entry `{NO_CODE_SITE}` if the \
             feature genuinely has no code site yet"
        ));
    }

    if violations.is_empty() {
        println!(
            "check-deferrals: {} tag(s) in code, all recorded; {} open entr(ies), all backed \
             by a marker ({} exempt) ✓",
            tags.len(),
            open_tags.len(),
            exempt_count
        );
        Ok(())
    } else {
        let mut msg = String::from(
            "`TODO(<tag>)` and deferred-decisions.md must agree in **both** directions — a \
             deferral only exists if it is in the canonical list, and one nobody can trip \
             over while editing the code is one nobody will act on (see that document's \
             closing section):\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}

/// Every `TODO(<name>)` marker on one line, in order, skipping anything that is not a plain
/// searchable word.
///
/// **One helper because the two directions disagreed.** The code scan used a single
/// `split_once`, so it saw only the *first* marker on a line and skipped the rest of the line
/// entirely when that one failed the plain-word check; the doc scan looped. While only the
/// code-to-doc direction existed that asymmetry cost silent under-enforcement. Once the
/// reverse direction depends on the code scan being complete it becomes a false failure: two
/// markers written on one line, and the gate reports the second as missing while pointing at
/// the line it is on.
pub(crate) fn markers_in_line(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = line;
    while let Some((_, after)) = rest.split_once("TODO(") {
        let Some((tag, tail)) = after.split_once(')') else {
            break;
        };
        rest = tail;
        // A tag has to be a plain word to be searchable; anything else is prose that happens
        // to contain the marker — including this module's own `TODO(<tag>)` placeholder.
        if !tag.is_empty() && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            out.push(tag);
        }
    }
    out
}

/// Marker exempting one deferral entry from the doc-to-code direction.
pub(crate) const NO_CODE_SITE: &str = "<!-- check-deferrals: no-code-site -->";

/// Every `TODO(<tag>)` in the document's **open** section, paired with whether its line
/// exempts it from needing a code marker.
///
/// **Open section only, and that boundary is the whole subtlety.** A resolved entry has no
/// code marker *because closing it deleted the marker* — that is the lifecycle working. A
/// first pass at this measurement scraped the whole file, counted `shell-cwd` (named
/// narratively inside a Resolved row, describing the deletion of its own markers) as rot, and
/// would have sent the follow-up work to reinstate the very marker `check-deferrals` had
/// caught. The same cut excludes the prose `TODO(<tag>)` in the closing "How to use this
/// document" section, which sits below `## Resolved`.
pub(crate) fn open_section_tags(doc: &str) -> Vec<(String, bool)> {
    let open = match doc.find("\n## Resolved") {
        Some(i) => &doc[..i],
        None => doc,
    };
    let mut out: Vec<(String, bool)> = Vec::new();
    for line in open.lines() {
        let exempt = line.contains(NO_CODE_SITE);
        for tag in markers_in_line(line) {
            // **Exemption ORs across occurrences, it does not take the first.** A tag is
            // routinely named on an earlier line than its own entry — `tty-server`'s entry
            // cross-references `TODO(session-metadata-server)` 170 lines above that entry —
            // and first-occurrence-wins would then discard a marker written on the entry
            // itself, failing the gate while telling you to do the thing you just did.
            match out.iter_mut().find(|(t, _)| t == tag) {
                Some(e) => e.1 |= exempt,
                None => out.push((tag.to_string(), exempt)),
            }
        }
    }
    out
}
