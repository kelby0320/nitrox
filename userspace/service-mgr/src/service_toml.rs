//! A focused parser for service declarations (`docs/spec/service-toml-schema.md`).
//!
//! Parses the subset the system uses, for **every** service in the file, in file order: the
//! `[service.<name>]` header with its keys, and the nested `[service.<name>.restart]` table. It
//! is line-oriented and section-tracking, unlike init's `toml_lite`, which does not do two-level
//! nesting. Unknown keys and sections are ignored (forward-compat, per the schema).
//!
//! **Malformed input resolves toward the safe answer**, since one bad table must not cost
//! the file: a declaration with no `executable` is skipped without swallowing the next, a
//! repeated key keeps the **first** value so a file cannot be steered by appending to it,
//! a name that returns after another service closed it is dropped rather than started
//! twice, and a `[restart]` table never leaks across a service boundary.

use alloc::string::String;
use alloc::vec::Vec;

/// Restart policy from `[restart].policy`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RestartPolicy {
    /// Never restart.
    Never,
    /// Restart on a non-zero exit, crash or kill.
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
    /// Restart policy (default `Never`, the conservative choice for an undeclared one).
    pub policy: RestartPolicy,
    /// Max restarts before giving up; `0` is unlimited.
    pub max_attempts: u32,
    /// Backoff strategy (default `Exponential`).
    pub backoff: Backoff,
    /// Initial backoff in nanoseconds (default `1s`).
    pub initial_ns: u64,
    /// Backoff cap for `Exponential` in nanoseconds (default `5min`).
    pub max_ns: u64,
}

impl Default for RestartConfig {
    fn default() -> Self {
        RestartConfig {
            policy: RestartPolicy::Never,
            max_attempts: 0,
            backoff: Backoff::Exponential,
            initial_ns: 1_000_000_000, // 1s
            max_ns: 300_000_000_000,   // 5min
        }
    }
}

/// A parsed single-service declaration.
#[derive(Clone, Debug)]
pub struct ServiceDecl {
    /// The service name, from the `[service.<name>]` header.
    pub name: String,
    /// The declared executable path.
    pub executable: String,
    /// System capabilities to grant at spawn, as a `SysCaps` bitmask.
    ///
    /// Names are the schema's (`"BIND_NAMESPACE"`, `"LOAD_MODULE"`, …); an unrecognised one
    /// contributes nothing and is **reported** by the caller rather than dropped silently, because
    /// a service that starts with less authority than it declared fails somewhere else entirely.
    /// The kernel refuses amplification, so this can only ever be a subset of what `service-mgr`
    /// itself holds.
    pub syscaps: u64,
    /// `syscaps` entries not recognised, kept so the caller can report them.
    pub unknown_syscaps: Vec<String>,
    /// Services that must have **finished** before this one starts.
    ///
    /// For a service that exits — a one-shot — finishing *is* readiness. There is no readiness
    /// protocol for a service that keeps running, so naming one here would wait forever;
    /// `service-mgr` bounds the wait and says so rather than hanging.
    ///
    /// **Ordinary start order does not need this**: declarations start in file order. `after`
    /// is for the stronger claim: *A has already exited*.
    pub after: Vec<String>,
    /// The restart configuration.
    pub restart: RestartConfig,
    /// **A server's path in the root namespace**: `service-mgr` waits
    /// for its `Meta::Ready`, binds the endpoint in its registry, and binds this path to the
    /// server's route, an endpoint of its own, so a restart reaches every binding. `None`
    /// for a service that is not a server. Absolute, and no `.` or `..` component; a declaration
    /// whose `endpoint` is not is skipped, rather than started as a server nothing can reach.
    pub endpoint: Option<String>,
    /// **A server the boot cannot go on without** — `auth-service` and `logging-service`, which
    /// `init` treated as critical-path. If one cannot be brought up at boot, `service-mgr` asks
    /// `init` for the emergency shell and starts nothing more. Only at bring-up: at runtime the
    /// terminal server holds the console, which the emergency shell would need.
    pub critical: bool,
    /// **A service `service --stop` and `--restart` refuse**: one whose absence would
    /// lock the administrator out, or lose state nothing rebuilds.
    pub essential: bool,
    /// **How long it may take to exit once asked to stop**, in nanoseconds — at a shutdown, or
    /// on `service --stop` — or `None` for `service-mgr`'s own bound. For a service whose stop
    /// is work that grows with what it holds: the storage service writes back and unmounts every
    /// filesystem it mounted before it exits.
    pub stop_timeout: Option<u64>,
}

/// Whether `path` may be a server's root path: absolute, not `/`, of plain visible-ASCII
/// components.
pub fn valid_endpoint(path: &str) -> bool {
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    !rest.is_empty()
        && rest.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
        && path.bytes().all(|b| b.is_ascii_graphic())
}

/// `SysCaps` bit for each name the schema recognises, taken from `libkern` rather than
/// written out: this is the one place that turns a *name* into a *grant*, and a hand-maintained
/// copy would grant the wrong capability on a renumbering, silently.
const SYSCAP_NAMES: &[(&str, u64)] = &[
    ("LOAD_MODULE", libkern::syscaps::SysCaps::LOAD_MODULE.bits()),
    ("BIND_NAMESPACE", libkern::syscaps::SysCaps::BIND_NAMESPACE.bits()),
    ("PHYSICAL_MEMORY", libkern::syscaps::SysCaps::PHYSICAL_MEMORY.bits()),
    ("REAL_TIME", libkern::syscaps::SysCaps::REAL_TIME.bits()),
    ("SYSTEM_CLOCK", libkern::syscaps::SysCaps::SYSTEM_CLOCK.bits()),
    ("AUDIT_CONTROL", libkern::syscaps::SysCaps::AUDIT_CONTROL.bits()),
];

/// Parse a `syscaps = [...]` array into `(bits, unknown_names)`; unknown names come back rather
/// than being dropped, since granting less than asked is a silent authority downgrade.
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

/// Which `[restart]` keys this declaration has already set. **First value wins, for every key**,
/// which is what makes the schema's "a declarations file cannot be steered by appending to it"
/// true. That includes appending a whole second `[service.<name>.restart]` table, since a repeated
/// header re-enters the section. Flags rather than "differs from the default", because a key
/// deliberately set *to* the default would otherwise stay overwritable.
#[derive(Default)]
struct RestartSeen {
    /// `after = []` followed by `after = ["x"]` must keep the empty list, which `is_empty()`
    /// cannot express.
    after: bool,
    syscaps: bool,
    endpoint: bool,
    critical: bool,
    essential: bool,
    stop_timeout: bool,
    policy: bool,
    max_attempts: bool,
    backoff: bool,
    initial: bool,
    max: bool,
}

enum Section {
    /// Before any recognized header, or inside an unrecognized section.
    None,
    Root,
    Restart,
}

/// Strip a trailing `#` comment and whitespace; quotes are not expected to contain `#`.
fn strip(line: &str) -> &str {
    let no_comment = match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    };
    no_comment.trim()
}

/// Parse the header `[a.b.c]` into its dotted components, or `None` if malformed.
fn header_parts(line: &str) -> Option<impl Iterator<Item = &str>> {
    let inner = line.strip_prefix('[')?.strip_suffix(']')?;
    Some(inner.split('.').map(str::trim))
}

/// Split `key = value` into `(key, raw_value)`; the value keeps its quotes.
fn key_raw(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once('=')?;
    Some((k.trim(), v.trim()))
}

fn unquote(v: &str) -> Option<&str> {
    v.strip_prefix('"')?.strip_suffix('"')
}

/// Parse an inline array of basic strings — `["a", "b"]` — into its elements.
///
/// Empty for anything that is not a well-formed array; unquoted elements are skipped rather
/// than taken raw, since a mangled name that matched nothing would look like no dependency.
/// Single-line only; a multi-line array parses as empty and the service starts unordered, which
/// the caller logs.
fn parse_string_array(v: &str) -> Vec<String> {
    let Some(inner) = v.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return Vec::new();
    };
    inner.split(',').filter_map(|e| unquote(e.trim()).map(String::from)).collect()
}

/// Parse a duration (`"200ms"`, `"1s"`, `"5min"`) to nanoseconds.
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
/// `executable`; one without is skipped rather than being a reason to drop the file.
///
/// **A repeated name is skipped, not re-emitted**: the failure it would otherwise cause is
/// starting the same service twice, which is worse than ignoring the duplicate. One file holds
/// every service because nothing can enumerate a directory of `.toml` files.
pub fn parse_all(text: &str) -> Vec<ServiceDecl> {
    parse_all_reporting(text).0
}

/// A declaration [`parse_all_reporting`] could not take. **The parser cannot log**, so it
/// carries each one out: a server skipped in silence is a boot that never says why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The name from its `[service.<name>]` header.
    pub name: String,
    /// Why it was not taken.
    pub why: SkipReason,
    /// Whether it said `critical = true`: then it is a critical server that did not come up.
    pub critical: bool,
}

/// Why a declaration was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// It names nothing to run.
    NoExecutable,
    /// A declaration of that name came earlier.
    Duplicate,
    /// Its `endpoint` is not an absolute path of plain components ([`valid_endpoint`]).
    BadEndpoint,
}

impl SkipReason {
    /// The reason, as a log line says it.
    pub fn describe(self) -> &'static str {
        match self {
            SkipReason::NoExecutable => "it has no executable",
            SkipReason::Duplicate => "a declaration of that name came earlier",
            SkipReason::BadEndpoint => "its endpoint is not an absolute path of plain components",
        }
    }
}

/// [`parse_all`], and every declaration it skipped, in file order.
pub fn parse_all_reporting(text: &str) -> (Vec<ServiceDecl>, Vec<Skipped>) {
    let mut out: Vec<ServiceDecl> = Vec::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    let mut name: Option<String> = None;
    let mut executable: Option<String> = None;
    let mut after: Vec<String> = Vec::new();
    let mut syscaps: Vec<String> = Vec::new();
    // `Some(None)` is an `endpoint` that did not read: the declaration is then skipped whole.
    let mut endpoint: Option<Option<String>> = None;
    let mut critical = false;
    let mut essential = false;
    let mut stop_timeout: Option<u64> = None;
    let mut restart = RestartConfig::default();
    let mut seen = RestartSeen::default();
    let mut section = Section::None;

    // Emit whatever has been accumulated, if complete and not a duplicate. The caller resets
    // `restart` afterwards where another declaration can follow.
    macro_rules! flush {
        () => {
            let (n, e) = (name.take(), executable.take());
            let why = match (&n, &e) {
                (None, _) => None,
                (Some(_), None) => Some(SkipReason::NoExecutable),
                (Some(n), Some(_)) if out.iter().any(|d: &ServiceDecl| &d.name == n) => Some(SkipReason::Duplicate),
                (Some(_), Some(_)) if matches!(endpoint, Some(None)) => Some(SkipReason::BadEndpoint),
                _ => None,
            };
            if let (Some(n), Some(why)) = (n.clone(), why) {
                skipped.push(Skipped { name: n, why, critical });
            } else if let (Some(n), Some(e)) = (n, e) {
                let (bits, unknown) = parse_syscaps(&syscaps);
                out.push(ServiceDecl {
                    name: n,
                    executable: e,
                    syscaps: bits,
                    unknown_syscaps: unknown,
                    after: after.clone(),
                    restart,
                    endpoint: endpoint.clone().flatten(),
                    critical,
                    essential,
                    stop_timeout,
                });
            }
            // **This is the only thing that resets them**, and it has to cover both paths:
            // a declaration that was emitted, and one skipped for having no `executable`.
            // Pinned by `an_after_on_a_skipped_declaration_does_not_leak`.
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
                (Some("service"), Some(svc), None, None) => {
                    if name.as_deref() != Some(svc) {
                        flush!();
                        restart = RestartConfig::default();
                        seen = RestartSeen::default();
                        endpoint = None;
                        critical = false;
                        essential = false;
                        stop_timeout = None;
                        name = Some(String::from(svc));
                    }
                    section = Section::Root;
                }
                (Some("service"), Some(svc), Some("restart"), None) if name.as_deref() == Some(svc) => {
                    section = Section::Restart;
                }
                _ => section = Section::None,
            }
            continue;
        }

        let (key, value) = match key_raw(line) {
            Some(kv) => kv,
            None => continue,
        };
        match section {
            // **First value wins**, so a file cannot be steered by appending to it; also what
            // makes a re-entered `[service.<name>]` header harmless.
            Section::Root if key == "executable" && executable.is_none() => {
                executable = unquote(value).map(String::from)
            }
            Section::Root if key == "after" && !seen.after => {
                seen.after = true;
                after = parse_string_array(value);
            }
            Section::Root if key == "syscaps" && !seen.syscaps => {
                seen.syscaps = true;
                syscaps = parse_string_array(value);
            }
            Section::Root if key == "endpoint" && !seen.endpoint => {
                seen.endpoint = true;
                endpoint = Some(unquote(value).filter(|p| valid_endpoint(p)).map(String::from));
            }
            Section::Root if key == "critical" && !seen.critical => {
                seen.critical = true;
                critical = value == "true";
            }
            Section::Root if key == "essential" && !seen.essential => {
                seen.essential = true;
                essential = value == "true";
            }
            // A duration that does not read is `service-mgr`'s own bound, as a backoff's is.
            Section::Root if key == "stop_timeout" && !seen.stop_timeout => {
                seen.stop_timeout = true;
                stop_timeout = unquote(value).and_then(parse_duration_ns);
            }
            // Every arm is guarded on **not yet seen** — see [`RestartSeen`].
            Section::Restart => match key {
                "policy" if !seen.policy => {
                    seen.policy = true;
                    restart.policy = match unquote(value) {
                        Some("never") => RestartPolicy::Never,
                        Some("on-failure") => RestartPolicy::OnFailure,
                        Some("always") => RestartPolicy::Always,
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
    (out, skipped)
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

    #[test]
    fn every_service_in_the_file_is_parsed_in_order() {
        let v = parse_all("[service.a]\nexecutable=\"/a\"\n[service.b]\nexecutable=\"/b\"\n");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].name, "a");
        assert_eq!(v[0].executable, "/a");
        assert_eq!(v[1].name, "b");
        assert_eq!(v[1].executable, "/b");
    }

    /// A flush that emits on header would land `b`'s executable on `a`.
    #[test]
    fn a_service_with_no_executable_does_not_swallow_the_next() {
        let v = parse_all("[service.a]\ndescription=\"no exe\"\n[service.b]\nexecutable=\"/b\"\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "b");
        assert_eq!(v[0].executable, "/b");
    }

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

    #[test]
    fn a_restart_table_naming_another_service_is_ignored() {
        let v = parse_all("[service.a]\nexecutable=\"/a\"\n[service.b.restart]\npolicy=\"always\"\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "a");
        assert_eq!(v[0].restart.policy, RestartPolicy::Never);
    }

    #[test]
    fn an_immediately_repeated_service_header_keeps_the_first_executable() {
        let v = parse_all("[service.a]\nexecutable=\"/first\"\n[service.a]\nexecutable=\"/second\"\n");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].executable, "/first");
    }

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

    #[test]
    fn a_repeated_executable_key_keeps_the_first() {
        let d = one("[service.a]\nexecutable=\"/first\"\nexecutable=\"/second\"\n");
        assert_eq!(d.executable, "/first");
    }

    #[test]
    fn a_repeated_restart_key_keeps_the_first() {
        let d = one("[service.a]\nexecutable=\"/a\"\n[service.a.restart]\n\
             policy=\"never\"\nmax_attempts=1\nbackoff=\"none\"\n\
             backoff_initial=\"1ms\"\nbackoff_max=\"2ms\"\n\
             policy=\"always\"\nmax_attempts=99\nbackoff=\"linear\"\n\
             backoff_initial=\"9s\"\nbackoff_max=\"9min\"\n");
        assert_eq!(d.restart.policy, RestartPolicy::Never);
        assert_eq!(d.restart.max_attempts, 1);
        assert_eq!(d.restart.backoff, Backoff::None);
        assert_eq!(d.restart.initial_ns, 1_000_000);
        assert_eq!(d.restart.max_ns, 2_000_000);
    }

    #[test]
    fn an_appended_restart_table_cannot_change_a_policy() {
        let d = one("[service.a]\nexecutable=\"/a\"\n[service.a.restart]\npolicy=\"never\"\n\
             [service.a.restart]\npolicy=\"always\"\n");
        assert_eq!(d.restart.policy, RestartPolicy::Never);
    }

    /// A key set **to its own default** is still consumed.
    #[test]
    fn a_key_set_to_its_default_still_consumes_the_slot() {
        let d = one("[service.a]\nexecutable=\"/a\"\n[service.a.restart]\n\
             policy=\"never\"\npolicy=\"always\"\n");
        assert_eq!(d.restart.policy, RestartPolicy::Never);
        let e = one("[service.b]\nexecutable=\"/b\"\n[service.b.restart]\n\
             backoff=\"exponential\"\nbackoff=\"none\"\n");
        assert_eq!(e.restart.backoff, Backoff::Exponential);
    }

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

    #[test]
    fn a_malformed_value_does_not_consume_the_slot() {
        let d = one("[service.a]\nexecutable=\"/a\"\n[service.a.restart]\n\
             max_attempts=\"oops\"\nmax_attempts=7\n");
        assert_eq!(d.restart.max_attempts, 7);
    }

    #[test]
    fn an_unknown_section_between_services_is_ignored() {
        let v = parse_all("[service.a]\nexecutable=\"/a\"\n[totally.unknown]\nkey=1\n[service.b]\nexecutable=\"/b\"\n");
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].name, "b");
    }

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

    #[test]
    fn a_malformed_after_element_is_dropped_not_taken_raw() {
        let d = one("[service.a]\nexecutable=\"/a\"\nafter=[\"good\", bare, \"also\"]\n");
        assert_eq!(d.after, ["good", "also"]);
        let e = one("[service.b]\nexecutable=\"/b\"\nafter=\"oops\"\n");
        assert!(e.after.is_empty());
        let f = one("[service.c]\nexecutable=\"/c\"\nafter=[]\n");
        assert!(f.after.is_empty());
    }

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

    #[test]
    fn a_repeated_after_keeps_the_first() {
        let d = one("[service.a]\nexecutable=\"/a\"\nafter=[\"first\"]\nafter=[\"second\"]\n");
        assert_eq!(d.after, ["first"]);
        let e = one("[service.b]\nexecutable=\"/b\"\nafter=[]\nafter=[\"sneaked-in\"]\n");
        assert!(e.after.is_empty(), "an appended line supplied a dependency");
        let f = one("[service.c]\nexecutable=\"/c\"\nsyscaps=[]\nsyscaps=[\"BIND_NAMESPACE\"]\n");
        assert_eq!(f.syscaps, 0, "an appended line supplied a capability");
    }

    #[test]
    fn syscaps_names_map_to_bits_and_unknown_names_are_reported() {
        let d = one("[service.a]\nexecutable=\"/a\"\nsyscaps=[\"BIND_NAMESPACE\", \"REAL_TIME\"]\n");
        assert_eq!(d.syscaps, (1 << 1) | (1 << 3));
        assert!(d.unknown_syscaps.is_empty());

        let e = one("[service.b]\nexecutable=\"/b\"\nsyscaps=[\"BIND_NAMESPACE\", \"NOPE\"]\n");
        assert_eq!(e.syscaps, 1 << 1, "the recognised half is still granted");
        assert_eq!(e.unknown_syscaps, ["NOPE"], "and the other half is reported");

        let f = one("[service.c]\nexecutable=\"/c\"\n");
        assert_eq!(f.syscaps, 0);
    }

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

    #[test]
    fn a_server_declares_its_path_and_whether_it_is_critical_or_essential() {
        let v = parse_all(
            "[service.auth-service]\nexecutable=\"/bin/auth-service\"\nendpoint=\"/svc/auth\"\n\
             critical=true\nessential=true\n\
             [service.heartbeat]\nexecutable=\"/bin/heartbeat\"\n",
        );
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].endpoint.as_deref(), Some("/svc/auth"));
        assert!(v[0].critical && v[0].essential);
        assert_eq!(v[1].endpoint, None, "a server's path does not leak to the next service");
        assert!(!v[1].critical && !v[1].essential, "nor do its flags");
        let first = parse_all(
            "[service.a]\nexecutable=\"/a\"\nendpoint=\"/x\"\nendpoint=\"/y\"\n\
             critical=true\ncritical=false\n",
        );
        assert_eq!(first[0].endpoint.as_deref(), Some("/x"), "first value wins");
        assert!(first[0].critical, "first value wins");
        let slow = parse_all(
            "[service.a]\nexecutable=\"/a\"\nstop_timeout=\"60s\"\nstop_timeout=\"1s\"\n\
             [service.b]\nexecutable=\"/b\"\n\
             [service.c]\nexecutable=\"/c\"\nstop_timeout=\"soon\"\n",
        );
        assert_eq!(slow[0].stop_timeout, Some(60_000_000_000), "first value wins");
        assert_eq!(slow[1].stop_timeout, None, "a bound does not leak to the next service");
        assert_eq!(slow[2].stop_timeout, None, "one that does not read is service-mgr's own");
        let yes = parse_all("[service.a]\nexecutable=\"/a\"\ncritical=yes\n");
        assert!(!yes[0].critical, "only `true` is true");
    }

    #[test]
    fn a_declaration_with_a_bad_endpoint_is_skipped_and_the_next_is_not() {
        let bads = ["\"svc/auth\"", "\"/\"", "\"/svc/../auth\"", "\"/svc//auth\"", "\"/svc/ auth\"", "/svc/auth"];
        for bad in bads {
            let v = parse_all(&alloc::format!(
                "[service.a]\nexecutable=\"/a\"\nendpoint={bad}\n[service.b]\nexecutable=\"/b\"\n"
            ));
            assert_eq!(v.len(), 1, "{bad}");
            assert_eq!(v[0].name, "b", "{bad}");
        }
    }

    #[test]
    fn a_skipped_declaration_is_reported_with_why_and_whether_it_was_critical() {
        let text = "[service.auth-service]\nexecutable=\"/a\"\nendpoint=\"svc/auth\"\ncritical=true\n\
                    [service.no-exe]\ndescription=\"x\"\n\
                    [service.ok]\nexecutable=\"/b\"\n\
                    [service.log]\nexecutable=\"/c\"\ncritical=true\n\
                    [service.ok]\nexecutable=\"/d\"\n";
        let (decls, skipped) = parse_all_reporting(text);
        let names: Vec<&str> = decls.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["ok", "log"]);
        let s = |name: &str, why, critical| Skipped { name: String::from(name), why, critical };
        assert_eq!(
            skipped,
            [
                s("auth-service", SkipReason::BadEndpoint, true),
                s("no-exe", SkipReason::NoExecutable, false),
                s("ok", SkipReason::Duplicate, false),
            ]
        );
        assert_eq!(parse_all(text).len(), 2);
        assert!(parse_all_reporting("[service.a]\nexecutable=\"/a\"\n").1.is_empty());
    }

    #[test]
    fn an_endpoint_is_an_absolute_path_of_plain_components() {
        for ok in ["/log", "/svc/auth", "/dev/input/new", "/dev/draw"] {
            assert!(valid_endpoint(ok), "{ok}");
        }
        for bad in ["", "/", "log", "/svc/", "/svc//auth", "/./x", "/x/..", "/a b"] {
            assert!(!valid_endpoint(bad), "{bad}");
        }
    }
}
