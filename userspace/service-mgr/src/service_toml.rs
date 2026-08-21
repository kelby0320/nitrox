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
use alloc::vec::Vec;

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

/// Parse **every** service declaration in `text`, in file order.
///
/// A declaration is emitted only if its `[service.<name>]` table carried an
/// `executable`; one without is skipped, because a service with nothing to run is a
/// misconfiguration the schema says to skip rather than a reason to drop the file.
///
/// **A repeated name is skipped, not re-emitted.** TOML forbids redefining a table, so a
/// second `[service.foo]` is already malformed — and the failure it would otherwise cause
/// here is starting the same service twice, which is worse than ignoring the duplicate.
///
/// One file holding many services is a **2026-08-21 change** to
/// `docs/spec/service-toml-schema.md`, which previously said each file declares one
/// service and the manager scans the directory. Nothing can enumerate a directory of
/// `.toml` files: the initramfs is a CPIO archive the kernel looks up by name with no
/// iteration, `sys_ns_enumerate` lists namespace bindings rather than directory entries
/// (it says so in its own doc), and `profile-server` projects only packages' `bin/`.
/// See the decision log.
pub fn parse_all(text: &str) -> Vec<ServiceDecl> {
    let mut out: Vec<ServiceDecl> = Vec::new();
    let mut name: Option<String> = None;
    let mut executable: Option<String> = None;
    let mut restart = RestartConfig::default();
    let mut section = Section::None;

    // Emit whatever has been accumulated, if it is complete and not a duplicate. The
    // caller resets `restart` afterwards where another declaration can follow; at EOF
    // there is nothing left to leak into.
    macro_rules! flush {
        () => {
            if let (Some(n), Some(e)) = (name.take(), executable.take())
                && !out.iter().any(|d: &ServiceDecl| d.name == n)
            {
                out.push(ServiceDecl { name: n, executable: e, restart });
            }
        };
    }

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
                // `[service.<name>]` — starts a declaration, and closes the previous one.
                (Some("service"), Some(svc), None, None) => {
                    if name.as_deref() != Some(svc) {
                        flush!();
                        // Nothing from the closed declaration may reach the next one.
                        restart = RestartConfig::default();
                        name = Some(String::from(svc));
                    }
                    section = Section::Root;
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
        let (key, value) = match key_raw(line) {
            Some(kv) => kv,
            None => continue,
        };
        match section {
            // **First value wins.** A key repeated inside one table is malformed TOML, and
            // of the two ways to be wrong, taking the first is the one that cannot be
            // steered by appending to a file. Also what makes a re-entered
            // `[service.<name>]` header harmless rather than a silent override.
            Section::Root if key == "executable" && executable.is_none() => {
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

    flush!();
    out
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

    /// Parse `text` and require exactly one declaration.
    fn one(text: &str) -> ServiceDecl {
        let mut v = parse_all(text);
        assert_eq!(v.len(), 1, "expected exactly one declaration, got {}", v.len());
        v.remove(0)
    }

    #[test]
    fn parses_the_slice_a_declaration() {
        let d = one(DECL);
        assert_eq!(d.name, "heartbeat");
        assert_eq!(d.executable, "/sbin/heartbeat");
        assert_eq!(d.restart.policy, RestartPolicy::Always);
        assert_eq!(d.restart.max_attempts, 3);
        assert_eq!(d.restart.backoff, Backoff::Exponential);
        assert_eq!(d.restart.initial_ns, 200_000_000);
        assert_eq!(d.restart.max_ns, 2_000_000_000);
    }

    #[test]
    fn missing_executable_is_skipped() {
        assert!(parse_all("[service.x]\n").is_empty());
    }

    #[test]
    fn restart_defaults_when_absent() {
        let d = one("[service.s]\nexecutable=\"/e\"\n");
        assert_eq!(d.restart, RestartConfig::default());
        assert_eq!(d.restart.policy, RestartPolicy::Never);
        assert_eq!(d.restart.max_attempts, 0);
    }

    #[test]
    fn policy_and_backoff_variants() {
        let mk = |extra: &str| {
            let t = std::format!("[service.s]\nexecutable=\"/e\"\n[service.s.restart]\n{extra}");
            one(&t).restart
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

    /// The whole point of the 2026-08-21 schema change: one file, every service in it.
    ///
    /// This replaces `only_first_service_is_parsed`, which asserted the old behaviour —
    /// so it is also the negative control for the change. Deleting the `flush!()` on a
    /// new `[service.<name>]` header makes this fail and nothing else.
    #[test]
    fn every_service_in_the_file_is_parsed_in_order() {
        let v = parse_all("[service.a]\nexecutable=\"/a\"\n[service.b]\nexecutable=\"/b\"\n");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "a");
        assert_eq!(v[0].executable, "/a");
        assert_eq!(v[1].name, "b");
        assert_eq!(v[1].executable, "/b");
    }

    /// **Text a correct writer would never produce.** A service with no `executable`
    /// must be skipped *without* taking the next one with it — the failure mode of a
    /// flush that emits on header rather than on completeness is that `b`'s executable
    /// lands on `a`, and both a length check and a name check would pass.
    #[test]
    fn a_service_with_no_executable_does_not_swallow_the_next() {
        let v = parse_all("[service.a]\ndescription=\"no exe\"\n[service.b]\nexecutable=\"/b\"\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "b");
        assert_eq!(v[0].executable, "/b");
    }

    /// A restart table must not leak across the service boundary. `a` declares one and
    /// `b` does not, so `b` gets the defaults.
    #[test]
    fn a_restart_table_does_not_leak_to_the_next_service() {
        let v = parse_all(
            "[service.a]\nexecutable=\"/a\"\n[service.a.restart]\npolicy=\"always\"\nmax_attempts=9\n\
             [service.b]\nexecutable=\"/b\"\n",
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].restart.policy, RestartPolicy::Always);
        assert_eq!(v[0].restart.max_attempts, 9);
        assert_eq!(v[1].restart, RestartConfig::default());
    }

    /// A `[service.<other>.restart]` table is not this service's, even mid-file.
    #[test]
    fn a_restart_table_naming_another_service_is_ignored() {
        let v = parse_all(
            "[service.a]\nexecutable=\"/a\"\n[service.b.restart]\npolicy=\"always\"\n",
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "a");
        assert_eq!(v[0].restart.policy, RestartPolicy::Never);
    }

    /// A repeated table is malformed TOML; emitting it twice would start one service
    /// twice, which is the worse of the two failures. **Immediately** repeated, the
    /// header re-enters the table in progress and the first `executable` stands.
    #[test]
    fn an_immediately_repeated_service_header_keeps_the_first_executable() {
        let v = parse_all("[service.a]\nexecutable=\"/first\"\n[service.a]\nexecutable=\"/second\"\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].executable, "/first");
    }

    /// The case the `out.iter().any(...)` guard in `flush!` is actually for: a name that
    /// returns **after** another service closed it, so the first copy has already been
    /// emitted. Without the guard this yields two services both called `a`, and
    /// `start_declared_services` would spawn the executable twice.
    #[test]
    fn a_service_name_returning_later_in_the_file_is_dropped() {
        let v = parse_all(
            "[service.a]\nexecutable=\"/first\"\n[service.b]\nexecutable=\"/b\"\n\
             [service.a]\nexecutable=\"/second\"\n",
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "a");
        assert_eq!(v[0].executable, "/first");
        assert_eq!(v[1].name, "b");
    }

    /// A repeated `executable` key inside one table takes the first, for the same reason.
    #[test]
    fn a_repeated_executable_key_keeps_the_first() {
        let d = one("[service.a]\nexecutable=\"/first\"\nexecutable=\"/second\"\n");
        assert_eq!(d.executable, "/first");
    }

    /// An unrecognized section between two services resets the parser's section without
    /// discarding the declaration in progress.
    #[test]
    fn an_unknown_section_between_services_is_ignored() {
        let v = parse_all(
            "[service.a]\nexecutable=\"/a\"\n[totally.unknown]\nkey=1\n[service.b]\nexecutable=\"/b\"\n",
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].name, "b");
    }

    #[test]
    fn an_empty_file_yields_nothing() {
        assert!(parse_all("").is_empty());
        assert!(parse_all("# just a comment\n").is_empty());
    }
}
