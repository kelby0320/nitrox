//! `desktop-shell`'s host-testable internals.
//!
//! **A library beside the binary, holding only what is pure** — the shell itself is a bare-target
//! program built around syscalls and cannot be host-tested at all, which is why the desktop-entry
//! parser and the modal's filter went untested when they were written (PR #279 review, finding 7).
//! `nxterm`, `nxfiles`, `nxedit`, `service-mgr` and `init` all grew a library for this reason;
//! this is the same move, kept to the functions that need no world.
//!
//! `#![no_std]` for the bare build; `std` under `cargo test` so the host harness works
//! (`cargo xtask test` runs `cargo test -p desktop-shell --lib`).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

/// One graphical application, from a desktop entry under `/applications`.
///
/// **The display name and the program are different strings, and that is the point** (M14 Part
/// H). The modal showed `/bin` — every service, server and CLI tool on the system, under the
/// name of its binary. It shows what a package *declares* is an application now, under the name
/// that package gives it.
pub struct Application {
    /// What a person sees: "Files".
    pub name: alloc::string::String,
    /// What gets spawned: `nxfiles`, resolved through `/bin` like anything else.
    pub exec: alloc::string::String,
}

/// Parse a desktop entry: `name` and `exec`, both required.
///
/// The same shape `Theme`'s reader uses — `key = "value"` a line at a time, `#` a comment —
/// rather than a TOML library, because this is two keys and the system has no TOML crate.
/// **Both required**: an entry with no `exec` names nothing to launch, and one with no `name`
/// would fall back to the binary's, which is the thing this part exists to stop showing.
pub fn parse_entry(text: &str) -> Option<Application> {
    let (mut name, mut exec) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        // A quoted value, and only a quoted value — `trim_matches('"')` would accept `"x` and
        // `x"`, which is the trap `theme.rs` records having fallen into.
        let Some(v) = v.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else { continue };
        match k.trim() {
            "name" => name = Some(alloc::string::String::from(v)),
            "exec" => exec = Some(alloc::string::String::from(v)),
            _ => {}
        }
    }
    match (name, exec) {
        (Some(name), Some(exec)) if !name.is_empty() && !exec.is_empty() => {
            Some(Application { name, exec })
        }
        _ => None,
    }
}

/// Whether `app` is shown for query `q` — matched against **both** the display name and the
/// program.
///
/// **Both, because both are things a person types.** Somebody who knows the desktop types
/// "editor"; somebody who knows the system types `nxedit`. Matching only the name would make the
/// second fail, and this system's users are more likely than most to be the second kind.
pub fn matches_app(app: &Application, q: &str) -> bool {
    matches(&app.name, q) || matches(&app.exec, q)
}

/// Whether one string is shown for query `q`. Case-insensitive on ASCII, because a display name
/// is capitalised ("Files") and nobody types the capital.
pub fn matches(name: &str, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    let (n, q) = (name.to_ascii_lowercase(), q.to_ascii_lowercase());
    n.contains(&q)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(name: &str, exec: &str) -> Application {
        Application { name: alloc::string::String::from(name), exec: alloc::string::String::from(exec) }
    }

    #[test]
    fn an_entry_needs_both_keys_and_both_quoted() {
        let a = parse_entry("name = \"Files\"\nexec = \"nxfiles\"\n").expect("valid");
        assert_eq!((a.name.as_str(), a.exec.as_str()), ("Files", "nxfiles"));

        // Comments, blank lines, surrounding space and a CRLF file all survive.
        let a = parse_entry("# an entry\r\n\r\n  name  =  \"Text Editor\"  \r\nexec=\"nxedit\"\r\n")
            .expect("valid despite CRLF and spacing");
        assert_eq!((a.name.as_str(), a.exec.as_str()), ("Text Editor", "nxedit"));

        // A file mapped from the store arrives page-padded with NULs; the tail is not lines.
        let padded = alloc::format!("name = \"Files\"\nexec = \"nxfiles\"\n{}", "\0".repeat(64));
        assert!(parse_entry(&padded).is_some(), "a page-padded entry must still parse");
    }

    #[test]
    fn a_malformed_entry_is_refused_rather_than_half_read() {
        // **Each of these would otherwise become a modal row that launches nothing.**
        for bad in [
            "name = \"Files\"\n",                    // no exec
            "exec = \"nxfiles\"\n",                  // no name
            "name = Files\nexec = nxfiles\n",       // unquoted
            "name = \"Files\nexec = \"nxfiles\"\n",  // one quote — the trap `theme.rs` records
            "name = \"\"\nexec = \"nxfiles\"\n",     // empty name
            "name = \"Files\"\nexec = \"\"\n",       // empty exec
            "",
        ] {
            assert!(parse_entry(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_filter_matches_the_program_as_well_as_the_name() {
        let a = app("Text Editor", "nxedit");
        assert!(matches_app(&a, ""), "an empty query shows everything");
        assert!(matches_app(&a, "editor"), "somebody who knows the desktop");
        assert!(matches_app(&a, "nxedit"), "somebody who knows the system");
        assert!(matches_app(&a, "EDIT"), "case-insensitive: display names are capitalised");
        assert!(matches_app(&a, "Text Ed"), "a substring spanning the space");
        assert!(!matches_app(&a, "nxterm"));
    }
}
