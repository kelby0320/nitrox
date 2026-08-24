//! A focused parser for service declarations (`docs/spec/service-toml-schema.md`).
//!
//! Parses the subset the system uses: the `[service.<name>]` header, the `executable`
//! key, and the nested `[service.<name>.restart]` table (`policy`, `max_attempts`,
//! `backoff`, `backoff_initial`, `backoff_max`) — for **every** service in the file, in
//! file order. It is line-oriented and section-tracking, unlike init's `toml_lite`, which
//! does not do two-level nesting.
//!
//! Still unparsed, and parsed when something consumes them: the arrays (`after`,
//! `before`, `wants`, `syscaps`), the `[handles]` table, `[environment]` and `[argv]`.
//! Unknown keys and sections are ignored (forward-compat, per the schema).
//!
//! **Malformed input resolves toward the safe answer**, since one bad table must not cost
//! the file: a declaration with no `executable` is skipped without swallowing the next, a
//! repeated key keeps the **first** value so a file cannot be steered by appending to it,
//! a name that returns after another service closed it is dropped rather than started
//! twice, and a `[restart]` table never leaks across a service boundary.

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

/// A parsed single-service declaration (the subset the system uses).
#[derive(Clone, Debug)]
pub struct ServiceDecl {
    /// The service name, from the `[service.<name>]` header.
    pub name: String,
    /// The declared executable path (mapped to an embedded image by the caller until
    /// a path-based ELF loader exists).
    pub executable: String,
    /// System capabilities to grant at spawn, as a `SysCaps` bitmask.
    ///
    /// Names are the schema's (`"BIND_NAMESPACE"`, `"LOAD_MODULE"`, …); an unrecognised one
    /// contributes nothing and is **reported** by the caller rather than dropped silently,
    /// because a service that starts with less authority than it declared fails somewhere
    /// else entirely — which is how this key came to be parsed at all. Retrofit Part A said
    /// `syscaps` was not needed, on the strength of `boot-probe` needing none; Part C2 then
    /// moved the demo chain into a declaration and it stopped at
    /// `test-harness: session user bind FAIL`, because `init` had spawned it with
    /// `BIND_NAMESPACE` and a declaration could not say so.
    ///
    /// The kernel refuses amplification, so this can only ever be a subset of what
    /// `service-mgr` itself holds.
    pub syscaps: u64,
    /// `syscaps` entries this parser did not recognise, kept so the caller can report them.
    /// Dropping them silently would downgrade a service's authority with no trace.
    pub unknown_syscaps: Vec<String>,
    /// Services that must have **finished** before this one starts.
    ///
    /// The schema calls this "must reach ready state", and for a service that exits — a
    /// one-shot — finishing *is* readiness. There is no readiness protocol for a service
    /// that keeps running, so naming one here would wait forever; `service-mgr` bounds the
    /// wait and says so rather than hanging. See `docs/spec/service-toml-schema.md`.
    ///
    /// **Ordinary start order does not need this.** Declarations are started in file order,
    /// so "start B after A" is written by putting A first. `after` is for the stronger claim:
    /// *A has already exited*.
    pub after: Vec<String>,
    /// The restart configuration.
    pub restart: RestartConfig,
}

/// `SysCaps` bit for each name the schema recognises. The values mirror
/// `libkern::syscaps::SysCaps`, which this crate does not depend on.
const SYSCAP_NAMES: &[(&str, u64)] = &[
    ("LOAD_MODULE", 1 << 0),
    ("BIND_NAMESPACE", 1 << 1),
    ("PHYSICAL_MEMORY", 1 << 2),
    ("REAL_TIME", 1 << 3),
    ("SYSTEM_CLOCK", 1 << 4),
    ("AUDIT_CONTROL", 1 << 5),
];

/// Parse a `syscaps = [...]` array into `(bits, unknown_names)`.
///
/// Unknown names come back rather than being dropped: granting less than a declaration asked
/// for is a silent authority downgrade, and the failure it produces is somewhere else.
pub fn parse_syscaps(list: &[String]) -> (u64, Vec<String>) {
    let mut bits = 0u64;
    let mut unknown = Vec::new();
    for n in list {
        match SYSCAP_NAMES.iter().find(|(name, _)| *name == n.as_str()) {
            Some((_, b)) => bits |= b,
            None => unknown.push(n.clone()),
        }
    }
    (bits, unknown)
}

/// Which `[restart]` keys this declaration has already set.
///
/// **First value wins, for every key**, which is what makes the schema's "a declarations
/// file cannot be steered by appending to it" true rather than true-of-`executable`-only.
/// An earlier version tracked it for `executable` and left the restart keys last-wins, so
/// appending `policy = "always"` after `policy = "never"` changed the policy — and
/// appending a whole second `[service.<name>.restart]` table did too, since a repeated
/// header re-enters the section (PR #226 review, finding 2).
///
/// Flags rather than "differs from the default", because a key deliberately set *to* the
/// default would otherwise stay overwritable.
#[derive(Default)]
struct RestartSeen {
    policy: bool,
    max_attempts: bool,
    backoff: bool,
    initial: bool,
    max: bool,
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

/// Parse an inline array of basic strings — `["a", "b"]` — into its elements.
///
/// Empty for anything that is not a well-formed array, and elements that are not quoted are
/// skipped rather than taken raw: a service name is used to *wait* on a service, and a
/// mangled one that silently matched nothing would look exactly like no dependency at all.
/// Single-line only, which is what the declarations file writes; a multi-line array parses as
/// empty and the service starts unordered, which the caller logs.
fn parse_string_array(v: &str) -> Vec<String> {
    let Some(inner) = v.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return Vec::new();
    };
    inner
        .split(',')
        .filter_map(|e| unquote(e.trim()).map(String::from))
        .collect()
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
    let mut after: Vec<String> = Vec::new();
    let mut syscaps: Vec<String> = Vec::new();
    let mut restart = RestartConfig::default();
    let mut seen = RestartSeen::default();
    let mut section = Section::None;

    // Emit whatever has been accumulated, if it is complete and not a duplicate. The
    // caller resets `restart` afterwards where another declaration can follow; at EOF
    // there is nothing left to leak into.
    macro_rules! flush {
        () => {
            if let (Some(n), Some(e)) = (name.take(), executable.take())
                && !out.iter().any(|d: &ServiceDecl| d.name == n)
            {
                // The parser cannot log; carry unrecognised names out so the caller does.
                let (bits, unknown) = parse_syscaps(&syscaps);
                out.push(ServiceDecl {
                    name: n,
                    executable: e,
                    syscaps: bits,
                    unknown_syscaps: unknown,
                    after: core::mem::take(&mut after),
                    restart,
                });
            }
            // `mem::take` above empties it on the *emit* path; this covers the other one —
            // a declaration skipped for having no `executable` must not hand its `after` to
            // the next service. Pinned by `an_after_on_a_skipped_declaration_does_not_leak`.
            after.clear();
            syscaps.clear();
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
                        seen = RestartSeen::default();
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
            // First-wins like the rest: a repeated `after` keeps the first list.
            Section::Root if key == "after" && after.is_empty() => {
                after = parse_string_array(value);
            }
            Section::Root if key == "syscaps" && syscaps.is_empty() => {
                syscaps = parse_string_array(value);
            }
            // Every arm is guarded on **not yet seen** — see [`RestartSeen`].
            Section::Restart => match key {
                "policy" if !seen.policy => {
                    seen.policy = true;
                    restart.policy = match unquote(value) {
                        Some("never") => RestartPolicy::Never,
                        Some("on-failure") => RestartPolicy::OnFailure,
                        Some("always") => RestartPolicy::Always,
                        // Unknown/malformed: keep the conservative default.
                        _ => RestartPolicy::Never,
                    };
                }
                "max_attempts" if !seen.max_attempts => {
                    if let Ok(n) = value.parse::<u32>() {
                        seen.max_attempts = true;
                        restart.max_attempts = n;
                    }
                }
                "backoff" if !seen.backoff => {
                    seen.backoff = true;
                    restart.backoff = match unquote(value) {
                        Some("none") => Backoff::None,
                        Some("linear") => Backoff::Linear,
                        Some("exponential") => Backoff::Exponential,
                        _ => restart.backoff,
                    };
                }
                "backoff_initial" if !seen.initial => {
                    if let Some(ns) = unquote(value).and_then(parse_duration_ns) {
                        seen.initial = true;
                        restart.initial_ns = ns;
                    }
                }
                "backoff_max" if !seen.max => {
                    if let Some(ns) = unquote(value).and_then(parse_duration_ns) {
                        seen.max = true;
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

    /// **Every** `[restart]` key is first-wins, not just `executable`. The schema promises a
    /// declarations file cannot be steered by appending to it, and until 2026-08-21 that was
    /// only true of `executable` — appending `policy = "always"` after `policy = "never"`
    /// silently changed the policy (PR #226 review, finding 2).
    #[test]
    fn a_repeated_restart_key_keeps_the_first() {
        let d = one(
            "[service.a]\nexecutable=\"/a\"\n[service.a.restart]\n\
             policy=\"never\"\nmax_attempts=1\nbackoff=\"none\"\n\
             backoff_initial=\"1ms\"\nbackoff_max=\"2ms\"\n\
             policy=\"always\"\nmax_attempts=99\nbackoff=\"linear\"\n\
             backoff_initial=\"9s\"\nbackoff_max=\"9min\"\n",
        );
        assert_eq!(d.restart.policy, RestartPolicy::Never);
        assert_eq!(d.restart.max_attempts, 1);
        assert_eq!(d.restart.backoff, Backoff::None);
        assert_eq!(d.restart.initial_ns, 1_000_000);
        assert_eq!(d.restart.max_ns, 2_000_000);
    }

    /// The same, by **appending a whole second table** — the shape an attacker has, since a
    /// repeated `[service.<name>.restart]` header re-enters the section rather than starting
    /// anything new.
    #[test]
    fn an_appended_restart_table_cannot_change_a_policy() {
        let d = one(
            "[service.a]\nexecutable=\"/a\"\n[service.a.restart]\npolicy=\"never\"\n\
             [service.a.restart]\npolicy=\"always\"\n",
        );
        assert_eq!(d.restart.policy, RestartPolicy::Never);
    }

    /// A key set **to its own default** is still consumed: `policy = "never"` is the default,
    /// and a later `policy = "always"` must not win because "it was never really set". This
    /// is why [`RestartSeen`] is flags rather than a comparison against the default.
    #[test]
    fn a_key_set_to_its_default_still_consumes_the_slot() {
        let d = one(
            "[service.a]\nexecutable=\"/a\"\n[service.a.restart]\n\
             policy=\"never\"\npolicy=\"always\"\n",
        );
        assert_eq!(d.restart.policy, RestartPolicy::Never);
        // And `backoff`, whose default is `Exponential`, behaves the same way.
        let e = one(
            "[service.b]\nexecutable=\"/b\"\n[service.b.restart]\n\
             backoff=\"exponential\"\nbackoff=\"none\"\n",
        );
        assert_eq!(e.restart.backoff, Backoff::Exponential);
    }

    /// The first-wins flags are **per declaration**, not per file. Two services each with a
    /// `[restart]` table must each get their own values; without the `seen` reset beside the
    /// `restart` reset, the *second* service's keys are all silently ignored and it inherits
    /// the defaults. The neighbouring leak test cannot catch this — its second service
    /// declares no restart table, so the defaults are the right answer there either way.
    #[test]
    fn the_first_wins_flags_reset_between_services() {
        let v = parse_all(
            "[service.a]\nexecutable=\"/a\"\n[service.a.restart]\n\
             policy=\"never\"\nmax_attempts=1\nbackoff=\"none\"\n\
             [service.b]\nexecutable=\"/b\"\n[service.b.restart]\n\
             policy=\"always\"\nmax_attempts=5\nbackoff=\"linear\"\n",
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].restart.policy, RestartPolicy::Never);
        assert_eq!(v[0].restart.max_attempts, 1);
        assert_eq!(v[0].restart.backoff, Backoff::None);
        assert_eq!(v[1].restart.policy, RestartPolicy::Always);
        assert_eq!(v[1].restart.max_attempts, 5);
        assert_eq!(v[1].restart.backoff, Backoff::Linear);
    }

    /// A malformed value does **not** consume the slot: `max_attempts = "oops"` is not a
    /// value, so a later well-formed one is still taken. Otherwise a typo would pin a key to
    /// its default and nothing would say so.
    #[test]
    fn a_malformed_value_does_not_consume_the_slot() {
        let d = one(
            "[service.a]\nexecutable=\"/a\"\n[service.a.restart]\n\
             max_attempts=\"oops\"\nmax_attempts=7\n",
        );
        assert_eq!(d.restart.max_attempts, 7);
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

    /// `after` is parsed, and it is per declaration — the list does not leak to the next
    /// service, in either direction.
    #[test]
    fn after_is_parsed_and_does_not_leak_between_services() {
        let v = parse_all(
            "[service.a]\nexecutable=\"/a\"\nafter=[\"x\", \"y\"]\n\
             [service.b]\nexecutable=\"/b\"\n",
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].after, ["x", "y"]);
        assert!(v[1].after.is_empty(), "b declared no dependency");
    }

    /// **Text a correct writer would never produce.** An element that is not a quoted string
    /// is dropped rather than taken raw: `after` names a service to *wait* on, and a mangled
    /// name that matched nothing would be indistinguishable from declaring no dependency.
    #[test]
    fn a_malformed_after_element_is_dropped_not_taken_raw() {
        let d = one("[service.a]\nexecutable=\"/a\"\nafter=[\"good\", bare, \"also\"]\n");
        assert_eq!(d.after, ["good", "also"]);
        // Not an array at all: no dependency, rather than one called `\"oops\"`.
        let e = one("[service.b]\nexecutable=\"/b\"\nafter=\"oops\"\n");
        assert!(e.after.is_empty());
        // An empty array is an empty list.
        let f = one("[service.c]\nexecutable=\"/c\"\nafter=[]\n");
        assert!(f.after.is_empty());
    }

    /// The failure the surviving `after.clear()` prevents, which the neighbouring leak test
    /// cannot reach: `mem::take` empties the list when a declaration is *emitted*, so only a
    /// declaration **skipped** for having no `executable` can carry its `after` forward.
    #[test]
    fn an_after_on_a_skipped_declaration_does_not_leak() {
        let v = parse_all(
            "[service.a]\nafter=[\"x\"]\ndescription=\"no exe\"\n\
             [service.b]\nexecutable=\"/b\"\n",
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "b");
        assert!(v[0].after.is_empty(), "b inherited a's dependency");
    }

    /// First-wins, like every other key.
    #[test]
    fn a_repeated_after_keeps_the_first() {
        let d = one("[service.a]\nexecutable=\"/a\"\nafter=[\"first\"]\nafter=[\"second\"]\n");
        assert_eq!(d.after, ["first"]);
    }

    /// `syscaps` names map to bits, and an unrecognised one is **carried out**, not dropped.
    /// Retrofit Part A judged this key unnecessary; Part C2's demo chain then stopped at
    /// `session user bind FAIL` for want of `BIND_NAMESPACE`, which is what a silent
    /// authority downgrade looks like from three subsystems away.
    #[test]
    fn syscaps_names_map_to_bits_and_unknown_names_are_reported() {
        let d = one(
            "[service.a]\nexecutable=\"/a\"\nsyscaps=[\"BIND_NAMESPACE\", \"REAL_TIME\"]\n",
        );
        assert_eq!(d.syscaps, (1 << 1) | (1 << 3));
        assert!(d.unknown_syscaps.is_empty());

        let e = one("[service.b]\nexecutable=\"/b\"\nsyscaps=[\"BIND_NAMESPACE\", \"NOPE\"]\n");
        assert_eq!(e.syscaps, 1 << 1, "the recognised half is still granted");
        assert_eq!(e.unknown_syscaps, ["NOPE"], "and the other half is reported");

        // Declaring none is zero, which is what almost every service should hold.
        let f = one("[service.c]\nexecutable=\"/c\"\n");
        assert_eq!(f.syscaps, 0);
    }

    /// Per declaration, like `after` — a grant must not leak to the next service.
    #[test]
    fn syscaps_do_not_leak_between_services() {
        let v = parse_all(
            "[service.a]\nexecutable=\"/a\"\nsyscaps=[\"BIND_NAMESPACE\"]\n\
             [service.b]\nexecutable=\"/b\"\n",
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].syscaps, 1 << 1);
        assert_eq!(v[1].syscaps, 0, "b inherited a's authority");
    }

    #[test]
    fn an_empty_file_yields_nothing() {
        assert!(parse_all("").is_empty());
        assert!(parse_all("# just a comment\n").is_empty());
    }
}
