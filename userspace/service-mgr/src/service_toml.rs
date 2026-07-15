//! A focused parser for service declarations (`docs/spec/service-toml-schema.md`).
//!
//! Slice A parses the subset the demo needs: the `[service.<name>]` header, the
//! `executable` key, and the nested `[service.<name>.restart]` table's `policy`.
//! It is line-oriented and section-tracking (unlike init's `toml_lite`, which does
//! not do two-level nesting) and reads a **single** service per file. The full schema
//! — arrays (`after`/`syscaps`), the `[handles]` table, multiple services, backoff
//! tuning — is parsed as those features are consumed by later parts/slices. Unknown
//! keys and sections are ignored (forward-compat, per the schema).

use alloc::string::String;

/// Restart policy from a declaration's `[restart].policy`. See the schema.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RestartPolicy {
    /// Never restart on any exit.
    Never,
    /// Restart only on abnormal exit (non-zero code / crash / killed).
    OnFailure,
    /// Restart on any exit.
    Always,
}

/// A parsed single-service declaration (the slice-A subset).
#[derive(Clone, Debug)]
pub struct ServiceDecl {
    /// The service name, from the `[service.<name>]` header.
    pub name: String,
    /// The declared executable path (mapped to an embedded image by the caller until
    /// a path-based ELF loader exists).
    pub executable: String,
    /// The restart policy (defaults to `Never` if no `[restart]` table is present —
    /// the conservative choice for an undeclared policy).
    pub restart: RestartPolicy,
}

/// Which section of the declaration the parser is currently inside.
enum Section {
    /// Before any recognized header, or inside an unrecognized section.
    None,
    /// Inside `[service.<name>]` (the service's own name matched).
    Root,
    /// Inside `[service.<name>.restart]`.
    Restart,
}

/// Strip a trailing `#` comment and surrounding whitespace. Quotes are not expected
/// to contain `#` in service declarations, so a naive split suffices for slice A.
fn strip(line: &str) -> &str {
    let no_comment = match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    };
    no_comment.trim()
}

/// Parse the bracketed header `[a.b.c]` into its dotted components, or `None` if it
/// is not a well-formed header line.
fn header_parts(line: &str) -> Option<impl Iterator<Item = &str>> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    Some(inner.split('.').map(str::trim))
}

/// Parse `key = "value"` into `(key, value)` for a basic-string value, or `None`.
fn key_string<'a>(line: &'a str) -> Option<(&'a str, &'a str)> {
    let (k, v) = line.split_once('=')?;
    let v = v.trim();
    let v = v.strip_prefix('"')?.strip_suffix('"')?;
    Some((k.trim(), v))
}

/// Parse the first service declaration in `text`. Returns `None` if no
/// `[service.<name>]` header with an `executable` is found.
pub fn parse(text: &str) -> Option<ServiceDecl> {
    let mut name: Option<String> = None;
    let mut executable: Option<String> = None;
    let mut restart = RestartPolicy::Never;
    let mut section = Section::None;

    for raw in text.lines() {
        let line = strip(raw);
        if line.is_empty() {
            continue;
        }

        if line.starts_with('[') {
            let mut parts = match header_parts(line) {
                Some(p) => p,
                None => {
                    section = Section::None;
                    continue;
                }
            };
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                // `[service.<name>]`
                (Some("service"), Some(svc), None, None) => {
                    match &name {
                        // First service: adopt its name.
                        None => {
                            name = Some(String::from(svc));
                            section = Section::Root;
                        }
                        // A second, different service — slice A parses only the first.
                        Some(existing) if existing != svc => break,
                        Some(_) => section = Section::Root,
                    }
                }
                // `[service.<name>.restart]` for the service we're parsing.
                (Some("service"), Some(svc), Some("restart"), None)
                    if name.as_deref() == Some(svc) =>
                {
                    section = Section::Restart;
                }
                // Any other section (unknown, or another service's subtable): ignore.
                _ => section = Section::None,
            }
            continue;
        }

        // Key = value line, routed by the current section.
        let (key, value) = match key_string(line) {
            Some(kv) => kv,
            None => continue,
        };
        match section {
            Section::Root if key == "executable" => executable = Some(String::from(value)),
            Section::Restart if key == "policy" => {
                restart = match value {
                    "never" => RestartPolicy::Never,
                    "on-failure" => RestartPolicy::OnFailure,
                    "always" => RestartPolicy::Always,
                    // Unknown policy: keep the conservative default.
                    _ => RestartPolicy::Never,
                };
            }
            _ => {}
        }
    }

    Some(ServiceDecl {
        name: name?,
        executable: executable?,
        restart,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    const DECL: &str = "\
# a comment\n\
[service.heartbeat]\n\
executable = \"/sbin/heartbeat\"\n\
description = \"demo\"\n\
\n\
[service.heartbeat.restart]\n\
policy = \"always\"\n";

    #[test]
    fn parses_the_slice_a_declaration() {
        let d = parse(DECL).expect("declaration parses");
        assert_eq!(d.name, "heartbeat");
        assert_eq!(d.executable, "/sbin/heartbeat");
        assert_eq!(d.restart, RestartPolicy::Always);
    }

    #[test]
    fn missing_executable_is_none() {
        assert!(parse("[service.x]\n").is_none());
    }

    #[test]
    fn policy_variants_and_default() {
        let mk = |p: &str| {
            let t = std::format!("[service.s]\nexecutable=\"/e\"\n[service.s.restart]\npolicy=\"{p}\"\n");
            parse(&t).unwrap().restart
        };
        assert_eq!(mk("never"), RestartPolicy::Never);
        assert_eq!(mk("on-failure"), RestartPolicy::OnFailure);
        assert_eq!(mk("always"), RestartPolicy::Always);
        // No restart table → conservative default.
        assert_eq!(
            parse("[service.s]\nexecutable=\"/e\"\n").unwrap().restart,
            RestartPolicy::Never
        );
    }

    #[test]
    fn only_first_service_is_parsed() {
        let t = "[service.a]\nexecutable=\"/a\"\n[service.b]\nexecutable=\"/b\"\n";
        assert_eq!(parse(t).unwrap().name, "a");
    }
}
