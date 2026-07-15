//! A focused parser for service declarations (`docs/spec/service-toml-schema.md`).
//!
//! Slice A parses the subset the demo needs: the `[service.<name>]` header, the
//! `executable` key, and the nested `[service.<name>.restart]` table (`policy`,
//! `max_attempts`, `backoff`, `backoff_initial`, `backoff_max`). It is line-oriented
//! and section-tracking (unlike init's `toml_lite`, which does not do two-level
//! nesting) and reads a **single** service per file. The rest of the schema — arrays
//! (`after`/`syscaps`), the `[handles]` table, multiple services — is parsed as those
//! features are consumed by later parts/slices. Unknown keys and sections are ignored
//! (forward-compat, per the schema).

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

/// Time-between-restarts strategy from `[restart].backoff`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Backoff {
    /// Restart immediately.
    None,
    /// Wait `backoff_initial` between every attempt.
    Linear,
    /// Double the wait each attempt, capped at `backoff_max`.
    Exponential,
}

/// The parsed `[service.<name>.restart]` table, with schema defaults applied.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct RestartConfig {
    /// Restart policy (default `Never` — the conservative choice for an undeclared
    /// policy, since the schema marks it required).
    pub policy: RestartPolicy,
    /// Max restarts before giving up; `0` = unlimited (the schema default).
    pub max_attempts: u32,
    /// Backoff strategy (schema default `Exponential`).
    pub backoff: Backoff,
    /// Initial backoff, in nanoseconds (schema default `1s`).
    pub initial_ns: u64,
    /// Backoff cap for `Exponential`, in nanoseconds (schema default `5min`).
    pub max_ns: u64,
}

impl Default for RestartConfig {
    fn default() -> Self {
        RestartConfig {
            policy: RestartPolicy::Never,
            max_attempts: 0,
            backoff: Backoff::Exponential,
            initial_ns: 1_000_000_000,   // 1s
            max_ns: 300_000_000_000,     // 5min
        }
    }
}

/// A parsed single-service declaration (the slice-A subset).
#[derive(Clone, Debug)]
pub struct ServiceDecl {
    /// The service name, from the `[service.<name>]` header.
    pub name: String,
    /// The declared executable path (mapped to an embedded image by the caller until
    /// a path-based ELF loader exists).
    pub executable: String,
    /// The restart configuration.
    pub restart: RestartConfig,
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

/// Split `key = value` into `(key, raw_value)` (both trimmed; the value keeps its
/// quotes, if any), or `None` if there is no `=`.
fn key_raw(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once('=')?;
    Some((k.trim(), v.trim()))
}

/// Strip surrounding double quotes from a basic-string value, or `None` if unquoted.
fn unquote(v: &str) -> Option<&str> {
    v.strip_prefix('"')?.strip_suffix('"')
}

/// Parse a duration string (`"200ms"`, `"1s"`, `"5min"`) to nanoseconds. `None` on a
/// malformed value or unrecognized unit.
fn parse_duration_ns(v: &str) -> Option<u64> {
    let split = v.find(|c: char| !c.is_ascii_digit())?;
    let (num, unit) = v.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mult: u64 = match unit {
        "ms" => 1_000_000,
        "s" => 1_000_000_000,
        "min" => 60_000_000_000,
        _ => return None,
    };
    Some(n.saturating_mul(mult))
}

/// Parse the first service declaration in `text`. Returns `None` if no
/// `[service.<name>]` header with an `executable` is found.
pub fn parse(text: &str) -> Option<ServiceDecl> {
    let mut name: Option<String> = None;
    let mut executable: Option<String> = None;
    let mut restart = RestartConfig::default();
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
                (Some("service"), Some(svc), None, None) => match &name {
                    None => {
                        name = Some(String::from(svc));
                        section = Section::Root;
                    }
                    // A second, different service — slice A parses only the first.
                    Some(existing) if existing != svc => break,
                    Some(_) => section = Section::Root,
                },
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
        let (key, value) = match key_raw(line) {
            Some(kv) => kv,
            None => continue,
        };
        match section {
            Section::Root if key == "executable" => {
                executable = unquote(value).map(String::from)
            }
            Section::Restart => match key {
                "policy" => {
                    restart.policy = match unquote(value) {
                        Some("never") => RestartPolicy::Never,
                        Some("on-failure") => RestartPolicy::OnFailure,
                        Some("always") => RestartPolicy::Always,
                        // Unknown/malformed: keep the conservative default.
                        _ => RestartPolicy::Never,
                    };
                }
                "max_attempts" => {
                    if let Ok(n) = value.parse::<u32>() {
                        restart.max_attempts = n;
                    }
                }
                "backoff" => {
                    restart.backoff = match unquote(value) {
                        Some("none") => Backoff::None,
                        Some("linear") => Backoff::Linear,
                        Some("exponential") => Backoff::Exponential,
                        _ => restart.backoff,
                    };
                }
                "backoff_initial" => {
                    if let Some(ns) = unquote(value).and_then(parse_duration_ns) {
                        restart.initial_ns = ns;
                    }
                }
                "backoff_max" => {
                    if let Some(ns) = unquote(value).and_then(parse_duration_ns) {
                        restart.max_ns = ns;
                    }
                }
                _ => {}
            },
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
policy = \"always\"\n\
max_attempts = 3\n\
backoff = \"exponential\"\n\
backoff_initial = \"200ms\"\n\
backoff_max = \"2s\"\n";

    #[test]
    fn parses_the_slice_a_declaration() {
        let d = parse(DECL).expect("declaration parses");
        assert_eq!(d.name, "heartbeat");
        assert_eq!(d.executable, "/sbin/heartbeat");
        assert_eq!(d.restart.policy, RestartPolicy::Always);
        assert_eq!(d.restart.max_attempts, 3);
        assert_eq!(d.restart.backoff, Backoff::Exponential);
        assert_eq!(d.restart.initial_ns, 200_000_000);
        assert_eq!(d.restart.max_ns, 2_000_000_000);
    }

    #[test]
    fn missing_executable_is_none() {
        assert!(parse("[service.x]\n").is_none());
    }

    #[test]
    fn restart_defaults_when_absent() {
        let d = parse("[service.s]\nexecutable=\"/e\"\n").unwrap();
        assert_eq!(d.restart, RestartConfig::default());
        assert_eq!(d.restart.policy, RestartPolicy::Never);
        assert_eq!(d.restart.max_attempts, 0);
    }

    #[test]
    fn policy_and_backoff_variants() {
        let mk = |extra: &str| {
            let t = std::format!("[service.s]\nexecutable=\"/e\"\n[service.s.restart]\n{extra}");
            parse(&t).unwrap().restart
        };
        assert_eq!(mk("policy=\"on-failure\"\n").policy, RestartPolicy::OnFailure);
        assert_eq!(mk("backoff=\"linear\"\n").backoff, Backoff::Linear);
        assert_eq!(mk("backoff=\"none\"\n").backoff, Backoff::None);
    }

    #[test]
    fn duration_parsing() {
        assert_eq!(parse_duration_ns("500ms"), Some(500_000_000));
        assert_eq!(parse_duration_ns("2s"), Some(2_000_000_000));
        assert_eq!(parse_duration_ns("5min"), Some(300_000_000_000));
        assert_eq!(parse_duration_ns("bad"), None);
        assert_eq!(parse_duration_ns("10h"), None);
    }

    #[test]
    fn only_first_service_is_parsed() {
        let t = "[service.a]\nexecutable=\"/a\"\n[service.b]\nexecutable=\"/b\"\n";
        assert_eq!(parse(t).unwrap().name, "a");
    }
}
