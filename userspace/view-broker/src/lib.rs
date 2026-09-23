//! `view-broker` — the part of the view broker that can be wrong about who may do what.
//!
//! The broker runs a program in a **view**: its caller's namespace plus a profile's grants, when
//! `/system/views.toml` says the caller may (`docs/planning/administration.md` § Part A). This
//! library is everything it decides with, kept apart from the syscall plumbing so it can be
//! tested on the host:
//!
//! - [`policy`] — reading `views.toml`, deciding a request, listing what a principal may use,
//!   and the last-administrator guard;
//! - [`pacing`] — the delay after a wrong password, held per *session*, so a program that opens
//!   several requests at once still guesses at one per delay;
//! - [`sessions`] — which sessions are open, for whom, under ids that are never reused;
//! - [`suffix`] — what a forwarded resolve asked for, which is where a client's identity comes
//!   from.
//!
//! The spec for the file is `docs/spec/views-toml-schema.md`; the protocol is
//! `docs/spec/rsproto-views-ops.md` and `librsproto::views`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod policy {
    //! `/system/views.toml` — profiles, and the rules that say who may use them for what.
    //!
    //! **A focused reader, not a TOML parser**, in the house style (`init`'s `toml_lite`,
    //! `service-mgr`'s `service_toml`): it reads exactly the shape the schema allows — `#`
    //! comments, `[profile.<name>]` tables, `[[rule]]` entries, and values that are a string or a
    //! one-line array of strings — and refuses the rest with the line it stopped at. A policy
    //! that does not read denies everything, so a message that says where is the difference
    //! between a mistake and an outage.

    use alloc::format;
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    /// Something a profile grants. **Part A knows one**; each later part adds its own, and a
    /// policy naming a grant this broker does not know is refused rather than ignored — a grant
    /// silently dropped is an administrator who believes they can do something they cannot.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Grant {
        /// Every block device, raw: `/dev/blk/<n>` and its `info`, bound one by one.
        Disks,
    }

    impl Grant {
        /// The grant a policy spells `name`, if this broker knows it.
        pub fn from_name(name: &str) -> Option<Grant> {
            match name {
                "disks" => Some(Grant::Disks),
                _ => None,
            }
        }

        /// How a policy spells it.
        pub fn name(self) -> &'static str {
            match self {
                Grant::Disks => "disks",
            }
        }
    }

    /// Every grant this broker knows, for the message that refuses one it does not.
    pub const KNOWN_GRANTS: &[Grant] = &[Grant::Disks];

    /// A profile: a named set of grants. A request names one as its view.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Profile {
        pub name: String,
        pub grants: Vec<Grant>,
    }

    /// A rule's `who` or `run`: everyone, or these names.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Names {
        Any,
        Only(Vec<String>),
    }

    impl Names {
        fn allows(&self, name: &str) -> bool {
            match self {
                Names::Any => true,
                Names::Only(v) => v.iter().any(|n| n == name),
            }
        }

        fn is_empty(&self) -> bool {
            matches!(self, Names::Only(v) if v.is_empty())
        }

        /// The names as a policy would list them — `*`, or space-separated.
        pub fn describe(&self) -> String {
            match self {
                Names::Any => String::from("*"),
                Names::Only(v) => v.join(" "),
            }
        }
    }

    /// How a rule wants the person at the keyboard proved.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Auth {
        /// Their own password, checked by `auth-service`.
        Password,
        /// Nothing beyond being in the session — for what the person at the machine may always
        /// do.
        None,
    }

    /// One `[[rule]]`.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Rule {
        pub who: Names,
        pub views: Vec<String>,
        pub run: Names,
        pub auth: Auth,
        /// The line its `[[rule]]` header is on, so a decision can say which rule made it.
        pub line: usize,
    }

    /// A whole policy.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub struct Policy {
        pub profiles: Vec<Profile>,
        pub rules: Vec<Rule>,
    }

    /// Why a policy did not read: the line, and what was wrong there.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct PolicyError {
        pub line: usize,
        pub message: String,
    }

    impl core::fmt::Display for PolicyError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            if self.line == 0 {
                write!(f, "{}", self.message)
            } else {
                write!(f, "line {}: {}", self.line, self.message)
            }
        }
    }

    fn err(line: usize, message: String) -> PolicyError {
        PolicyError { line, message }
    }

    /// What a policy says about one request.
    #[derive(Debug, PartialEq, Eq)]
    pub enum Decision<'p> {
        /// Allowed: the profile to grant, and the proof the rule wants.
        Allow { profile: &'p Profile, auth: Auth, rule_line: usize },
        /// Refused, and why — worded for the person who asked.
        Deny(String),
    }

    /// A name a policy may use for a profile, and a program a rule may name: letters, digits,
    /// `-`, `_` and `.`, not empty, and not `.` or `..`. **A program is a bare name** because
    /// the broker resolves it under its own `/bin`; a path would let "the same name" mean
    /// something else.
    pub fn is_bare_name(name: &str) -> bool {
        !name.is_empty()
            && name != "."
            && name != ".."
            && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    }

    /// Strip a `#` comment that is not inside a string.
    fn strip_comment(line: &str) -> &str {
        let mut quoted = false;
        for (i, c) in line.char_indices() {
            match c {
                '"' => quoted = !quoted,
                '#' if !quoted => return &line[..i],
                _ => {}
            }
        }
        line
    }

    /// A value: `"a string"` or `["one", "or", "more"]`, on one line, with no escapes.
    enum Value {
        Str(String),
        List(Vec<String>),
    }

    fn parse_string(s: &str, line: usize) -> Result<String, PolicyError> {
        let inner = s
            .strip_prefix('"')
            .and_then(|r| r.strip_suffix('"'))
            .ok_or_else(|| err(line, format!("`{s}` is not a quoted string")))?;
        if inner.contains('"') || inner.contains('\\') {
            return Err(err(line, format!("`{s}`: quotes and backslashes are not supported")));
        }
        Ok(inner.to_string())
    }

    fn parse_value(s: &str, line: usize) -> Result<Value, PolicyError> {
        let s = s.trim();
        if let Some(inner) = s.strip_prefix('[') {
            let inner = inner
                .strip_suffix(']')
                .ok_or_else(|| err(line, String::from("an array must close on the same line")))?;
            let mut items = Vec::new();
            for part in inner.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue; // a trailing comma, or `[]`
                }
                items.push(parse_string(part, line)?);
            }
            Ok(Value::List(items))
        } else {
            Ok(Value::Str(parse_string(s, line)?))
        }
    }

    fn list(v: Value, key: &str, line: usize) -> Result<Vec<String>, PolicyError> {
        match v {
            Value::List(items) => Ok(items),
            Value::Str(_) => Err(err(line, format!("`{key}` is a list: write [\"…\"]"))),
        }
    }

    fn names(v: Value, key: &str, line: usize) -> Result<Names, PolicyError> {
        let items = list(v, key, line)?;
        if items.iter().any(|n| n == "*") {
            if items.len() > 1 {
                return Err(err(line, format!("`{key}`: `*` already means everyone")));
            }
            return Ok(Names::Any);
        }
        Ok(Names::Only(items))
    }

    /// A `[[rule]]` being read: each field as it arrives, checked for completeness at the end.
    #[derive(Default)]
    struct PartialRule {
        line: usize,
        who: Option<Names>,
        views: Option<Vec<String>>,
        run: Option<Names>,
        auth: Option<Auth>,
    }

    enum Section {
        None,
        Profile(usize),
        Rule(PartialRule),
    }

    fn finish(rule: PartialRule) -> Result<Rule, PolicyError> {
        let line = rule.line;
        let missing = |k: &str| err(line, format!("this rule has no `{k}`"));
        Ok(Rule {
            who: rule.who.ok_or_else(|| missing("who"))?,
            views: rule.views.ok_or_else(|| missing("use"))?,
            run: rule.run.ok_or_else(|| missing("run"))?,
            auth: rule.auth.ok_or_else(|| missing("auth"))?,
            line,
        })
    }

    /// Read a policy. The first thing wrong is the error, with its line.
    pub fn parse(text: &str) -> Result<Policy, PolicyError> {
        let mut policy = Policy::default();
        let mut section = Section::None;
        for (i, raw) in text.lines().enumerate() {
            let line = i + 1;
            let l = strip_comment(raw).trim();
            if l.is_empty() {
                continue;
            }
            if l == "[[rule]]" {
                if let Section::Rule(r) = core::mem::replace(&mut section, Section::None) {
                    policy.rules.push(finish(r)?);
                }
                section = Section::Rule(PartialRule { line, ..PartialRule::default() });
                continue;
            }
            if let Some(name) = l.strip_prefix("[profile.").and_then(|r| r.strip_suffix(']')) {
                if let Section::Rule(r) = core::mem::replace(&mut section, Section::None) {
                    policy.rules.push(finish(r)?);
                }
                if !is_bare_name(name) {
                    return Err(err(line, format!("`{name}` is not a usable profile name")));
                }
                if policy.profiles.iter().any(|p| p.name == name) {
                    return Err(err(line, format!("profile `{name}` is defined twice")));
                }
                policy.profiles.push(Profile { name: name.to_string(), grants: Vec::new() });
                section = Section::Profile(policy.profiles.len() - 1);
                continue;
            }
            if l.starts_with('[') {
                return Err(err(line, format!("`{l}` is not a section this file has")));
            }
            let (key, value) = l
                .split_once('=')
                .ok_or_else(|| err(line, format!("`{l}` is not `key = value`")))?;
            let key = key.trim();
            let value = parse_value(value, line)?;
            match &mut section {
                Section::None => {
                    return Err(err(line, format!("`{key}` is outside any profile or rule")));
                }
                Section::Profile(p) => {
                    if key != "grants" {
                        return Err(err(line, format!("a profile has `grants`, not `{key}`")));
                    }
                    let profile = &mut policy.profiles[*p];
                    if !profile.grants.is_empty() {
                        return Err(err(line, String::from("`grants` is given twice")));
                    }
                    for g in list(value, key, line)? {
                        let grant = Grant::from_name(&g).ok_or_else(|| {
                            let known: Vec<&str> = KNOWN_GRANTS.iter().map(|g| g.name()).collect();
                            let known = known.join(", ");
                            let msg =
                                format!("`{g}` is not a grant this system has (it has: {known})");
                            err(line, msg)
                        })?;
                        if !profile.grants.contains(&grant) {
                            profile.grants.push(grant);
                        }
                    }
                }
                Section::Rule(r) => {
                    let twice = || err(line, format!("`{key}` is given twice"));
                    match key {
                        "who" => {
                            if r.who.is_some() {
                                return Err(twice());
                            }
                            r.who = Some(names(value, key, line)?);
                        }
                        "use" => {
                            if r.views.is_some() {
                                return Err(twice());
                            }
                            r.views = Some(list(value, key, line)?);
                        }
                        "run" => {
                            if r.run.is_some() {
                                return Err(twice());
                            }
                            let run = names(value, key, line)?;
                            if let Names::Only(v) = &run
                                && let Some(bad) = v.iter().find(|n| !is_bare_name(n))
                            {
                                return Err(err(line, format!("`{bad}` is not a program name")));
                            }
                            r.run = Some(run);
                        }
                        "auth" => {
                            if r.auth.is_some() {
                                return Err(twice());
                            }
                            r.auth = Some(match value {
                                Value::Str(s) if s == "password" => Auth::Password,
                                Value::Str(s) if s == "none" => Auth::None,
                                _ => {
                                    return Err(err(
                                        line,
                                        String::from("`auth` is \"password\" or \"none\""),
                                    ));
                                }
                            });
                        }
                        _ => {
                            return Err(err(
                                line,
                                format!("a rule has `who`, `use`, `run` and `auth`, not `{key}`"),
                            ));
                        }
                    }
                }
            }
        }
        if let Section::Rule(r) = section {
            policy.rules.push(finish(r)?);
        }
        for rule in &policy.rules {
            for v in &rule.views {
                if !policy.profiles.iter().any(|p| &p.name == v) {
                    let msg = format!("this rule uses `{v}`, which no profile defines");
                    return Err(err(rule.line, msg));
                }
            }
        }
        Ok(policy)
    }

    impl Policy {
        /// Decide whether `principal` may run `program` in `view`. **The first matching rule
        /// decides**; none matching is a denial. The reason names what was missing, so the
        /// person reading it can tell a view they may not use from a program they may not run.
        pub fn decide(&self, principal: &str, view: &str, program: &str) -> Decision<'_> {
            let Some(profile) = self.profiles.iter().find(|p| p.name == view) else {
                return Decision::Deny(format!("there is no view called `{view}`"));
            };
            let mut may_use = false;
            for rule in &self.rules {
                if !rule.who.allows(principal) || !rule.views.iter().any(|v| v == view) {
                    continue;
                }
                may_use = true;
                if rule.run.allows(program) {
                    return Decision::Allow { profile, auth: rule.auth, rule_line: rule.line };
                }
            }
            Decision::Deny(if may_use {
                format!("`{view}` does not let {principal} run `{program}`")
            } else {
                format!("no rule lets {principal} use `{view}`")
            })
        }

        /// What `principal` may use: one row per view a rule lets them use — the view, the
        /// programs, and whether a password is asked for — in the policy's order.
        pub fn rows_for(&self, principal: &str) -> Vec<(String, String, bool)> {
            let mut rows = Vec::new();
            for rule in &self.rules {
                if !rule.who.allows(principal) {
                    continue;
                }
                for v in &rule.views {
                    rows.push((v.clone(), rule.run.describe(), rule.auth == Auth::Password));
                }
            }
            rows
        }

        /// **Whether anyone could administer the system under this policy** — narrowly, whether a
        /// rule lets some account use the `admin` view for *every* program.
        ///
        /// Narrow on purpose (`administration.md` § Policy): a rule granting one program does not
        /// make an administrator. A broad `who = ["*"]` rule letting everyone power off must not
        /// count, or removing the last real administrator would look safe.
        pub fn has_administrator(&self) -> bool {
            self.rules.iter().any(|r| {
                r.views.iter().any(|v| v == "admin") && r.run == Names::Any && !r.who.is_empty()
            })
        }
    }

    /// Judge a policy's text the way `with --check` asks: it must read, and it must leave
    /// someone able to administer the system — a policy with nobody who can use `admin` for
    /// everything cannot be changed again short of the live image.
    pub fn check(text: &str) -> Result<Policy, PolicyError> {
        let policy = parse(text)?;
        if !policy.has_administrator() {
            return Err(err(
                0,
                String::from(
                    "no rule lets anyone use `admin` for every program, so nothing could change \
                     this policy again — only the live image could",
                ),
            ));
        }
        Ok(policy)
    }
}

pub mod pacing {
    //! The delay after a wrong password.
    //!
    //! **Held per session, not per request.** Any program in a session can open a request and
    //! offer a password, so a delay per request would let one open several and guess on each in
    //! parallel. A failure therefore holds the session's *next* check, on whichever request it
    //! arrives. The cap — three failures — is per request, as a login's is: a cap per session
    //! would let one program's wrong guesses lock its person out of `with` until they logged out.
    //!
    //! **A deadline, never a sleep.** The broker is one thread serving every session and the
    //! supervisors' logins; it holds a check until [`Pacing::check_at`] and waits on its other
    //! channels meanwhile.

    /// How long after a wrong password the session's next check waits: two seconds, the login
    /// prompt's pause.
    pub const FAIL_DELAY_NS: u64 = 2_000_000_000;

    /// Wrong passwords a single request may offer before it is refused.
    pub const MAX_FAILURES: u8 = 3;

    /// One session's pacing.
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    pub struct Pacing {
        /// Monotonic nanoseconds before which no check is made for this session.
        not_before: u64,
    }

    impl Pacing {
        /// When a check asked for at `now` may run: `now`, unless a failure is still cooling.
        pub fn check_at(&self, now: u64) -> u64 {
            now.max(self.not_before)
        }

        /// A check failed at `now`: hold the session's next one.
        pub fn failed(&mut self, now: u64) {
            self.not_before = now.saturating_add(FAIL_DELAY_NS);
        }
    }
}

pub mod sessions {
    //! Which sessions are open, and for whom.
    //!
    //! **Ids increase and are never reused within a boot.** A program that ignores its session's
    //! end keeps a namespace with that session's `/dev/views` base in it; were the id handed out
    //! again, its requests would arrive with the next login's identity.

    use crate::pacing::Pacing;
    use alloc::string::String;
    use alloc::vec::Vec;

    /// One open session.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Session {
        pub id: u64,
        pub principal: String,
        pub pacing: Pacing,
    }

    /// Every open session.
    #[derive(Debug)]
    pub struct Sessions {
        next: u64,
        open: Vec<Session>,
    }

    impl Default for Sessions {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Sessions {
        /// None open; the first id is 1, so a zero id never names one.
        pub fn new() -> Sessions {
            Sessions { next: 1, open: Vec::new() }
        }

        /// Open a session for `principal`, and return its id.
        pub fn open(&mut self, principal: &str) -> u64 {
            let id = self.next;
            self.next += 1;
            let principal = String::from(principal);
            self.open.push(Session { id, principal, pacing: Pacing::default() });
            id
        }

        /// Close session `id`, returning it if it was open.
        pub fn close(&mut self, id: u64) -> Option<Session> {
            let i = self.open.iter().position(|s| s.id == id)?;
            Some(self.open.remove(i))
        }

        /// Session `id`, if it is open.
        pub fn get(&self, id: u64) -> Option<&Session> {
            self.open.iter().find(|s| s.id == id)
        }

        /// Session `id`, mutably, if it is open.
        pub fn get_mut(&mut self, id: u64) -> Option<&mut Session> {
            self.open.iter_mut().find(|s| s.id == id)
        }
    }
}

pub mod suffix {
    //! What a resolve that reached the broker asked for.
    //!
    //! **This is where identity comes from.** A session's `/dev/views` is the broker's forwarding
    //! endpoint bound with the base `/s/<session>`, so a resolve from inside it arrives with the
    //! suffix `s/<session>` — and nothing inside the session can change the base. A supervisor,
    //! holding the unscoped `/svc/views`, resolves `session`.

    /// A forwarded suffix, classified.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Suffix {
        /// `session`: a supervisor wants a channel to open and close sessions on.
        Supervisor,
        /// `s/<id>`: a client in session `id`.
        Client(u64),
        /// Anything else — answered `NotFound`.
        Unknown,
    }

    /// Classify `suffix`. A session id is decimal, non-zero, with no leading zero, and fits a
    /// `u64` — one spelling per id, so two paths never name the same session.
    pub fn parse(suffix: &[u8]) -> Suffix {
        if suffix == b"session" {
            return Suffix::Supervisor;
        }
        let Some(digits) = suffix.strip_prefix(b"s/") else {
            return Suffix::Unknown;
        };
        if digits.is_empty() || digits[0] == b'0' || !digits.iter().all(u8::is_ascii_digit) {
            return Suffix::Unknown;
        }
        let mut n: u64 = 0;
        for d in digits {
            match n.checked_mul(10).and_then(|n| n.checked_add((d - b'0') as u64)) {
                Some(v) => n = v,
                None => return Suffix::Unknown,
            }
        }
        Suffix::Client(n)
    }
}

#[cfg(test)]
mod tests {
    use super::pacing::{FAIL_DELAY_NS, Pacing};
    use super::policy::*;
    use super::sessions::Sessions;
    use super::suffix::{self, Suffix};

    const SEED: &str = r#"
# The build's policy: the demo account administers, and may install with a password.
[profile.admin]
grants = ["disks"]

[profile.install]
grants = ["disks"]   # the same grant, narrower rule

[[rule]]
who  = ["alice"]
use  = ["admin"]
run  = ["*"]
auth = "password"

[[rule]]
who  = ["alice", "bob"]
use  = ["install"]
run  = ["nxinstall"]
auth = "password"
"#;

    #[test]
    fn a_policy_reads_with_its_profiles_and_rules_in_order() {
        let p = parse(SEED).unwrap();
        assert_eq!(p.profiles.len(), 2);
        assert_eq!(p.profiles[0].grants, [Grant::Disks]);
        assert_eq!(p.rules.len(), 2);
        assert_eq!(p.rules[0].who, Names::Only(vec!["alice".into()]));
        assert_eq!(p.rules[0].run, Names::Any);
        assert_eq!(p.rules[1].line, 15, "a rule remembers its header's line");
    }

    #[test]
    fn a_request_is_decided_by_the_first_rule_that_matches_all_three() {
        let p = parse(SEED).unwrap();
        assert!(matches!(
            p.decide("alice", "admin", "nxsh"),
            Decision::Allow { auth: Auth::Password, rule_line: 9, .. }
        ));
        assert!(matches!(p.decide("bob", "install", "nxinstall"), Decision::Allow { .. }));
    }

    #[test]
    fn a_denial_says_whether_the_view_or_the_program_was_the_problem() {
        let p = parse(SEED).unwrap();
        assert_eq!(
            p.decide("bob", "install", "nxsh"),
            Decision::Deny("`install` does not let bob run `nxsh`".into()),
        );
        let deny = |s: &str| Decision::Deny(s.into());
        assert_eq!(p.decide("bob", "admin", "nxsh"), deny("no rule lets bob use `admin`"));
        assert_eq!(p.decide("alice", "nope", "x"), deny("there is no view called `nope`"));
    }

    #[test]
    fn a_mistake_is_refused_with_its_line() {
        let bad = |text: &str, line: usize, needle: &str| {
            let e = parse(text).unwrap_err();
            assert_eq!(e.line, line, "{e}");
            assert!(e.message.contains(needle), "`{e}` should mention `{needle}`");
        };
        bad("[profile.admin]\ngrants = [\"power\"]\n", 2, "not a grant this system has");
        let undefined = "[profile.admin]\ngrants = [\"disks\"]\n[[rule]]\nwho = [\"a\"]\n\
                         use = [\"x\"]\nrun = [\"*\"]\nauth = \"none\"\n";
        bad(undefined, 3, "no profile defines");
        bad("[[rule]]\nwho = [\"a\"]\n", 1, "no `use`");
        bad("[[rule]]\nwhom = [\"a\"]\n", 2, "not `whom`");
        bad("[[rule]]\nauth = \"maybe\"\n", 2, "\"password\" or \"none\"");
        bad("[[rule]]\nrun = [\"/bin/sh\"]\n", 2, "not a program name");
        bad("[[rule]]\nwho = [\"*\", \"a\"]\n", 2, "already means everyone");
        bad("grants = [\"disks\"]\n", 1, "outside any profile");
        bad("[profile.a]\n[profile.a]\n", 2, "defined twice");
        bad("[profiles.a]\n", 1, "not a section");
        bad("[profile.a]\ngrants = [\"disks\"\n", 2, "close on the same line");
    }

    #[test]
    fn a_comment_ends_a_line_unless_it_is_inside_a_string() {
        let p = parse("[profile.a] # the admin\ngrants = [\"disks\"] # all of them\n").unwrap();
        assert_eq!(p.profiles[0].name, "a");
        // `#` inside a string is data: the whole name reaches the grant check, which refuses it
        // *as written* — a stripper that cut at the `#` would complain about `di` instead.
        let e = parse("[profile.a]\ngrants = [\"di#sks\"]\n").unwrap_err();
        assert!(e.message.contains("`di#sks`"), "{e}");
    }

    #[test]
    fn rows_list_each_view_a_principal_may_use() {
        let p = parse(SEED).unwrap();
        assert_eq!(
            p.rows_for("alice"),
            [("admin".into(), "*".into(), true), ("install".into(), "nxinstall".into(), true)],
        );
        assert_eq!(p.rows_for("carol"), []);
    }

    /// **The guard at its neighbours.** Only `admin` with `run = ["*"]` for someone counts; one
    /// step away on each axis does not.
    #[test]
    fn only_admin_for_every_program_makes_an_administrator() {
        let with = |who: &str, view: &str, run: &str| {
            format!(
                "[profile.admin]\ngrants = [\"disks\"]\n[profile.other]\ngrants = []\n\
                 [[rule]]\nwho = {who}\nuse = [\"{view}\"]\nrun = {run}\nauth = \"password\"\n"
            )
        };
        assert!(check(&with("[\"alice\"]", "admin", "[\"*\"]")).is_ok());
        assert!(check(&with("[\"*\"]", "admin", "[\"*\"]")).is_ok(), "everyone is someone");
        assert!(check(&with("[\"alice\"]", "admin", "[\"disk\"]")).is_err(), "one program");
        assert!(check(&with("[\"alice\"]", "other", "[\"*\"]")).is_err(), "another view");
        assert!(check(&with("[]", "admin", "[\"*\"]")).is_err(), "nobody");
        assert!(check("").is_err(), "an empty policy leaves no administrator");
    }

    /// **Pacing is the session's.** Two requests in one session share the delay a failure on
    /// either starts; a request in another session does not wait.
    #[test]
    fn a_failure_delays_the_sessions_next_check_on_any_request_but_not_another_sessions() {
        let mut s = Sessions::new();
        let a = s.open("alice");
        let b = s.open("bob");
        let t0 = 1_000;
        // Request 1 in session `a` fails at t0.
        s.get_mut(a).unwrap().pacing.failed(t0);
        // Request 2 in session `a`, half a second later, waits for the whole delay.
        let half = t0 + FAIL_DELAY_NS / 2;
        assert_eq!(s.get(a).unwrap().pacing.check_at(half), t0 + FAIL_DELAY_NS);
        // Session `b` is not held.
        assert_eq!(s.get(b).unwrap().pacing.check_at(half), half);
        // And after the delay, `a` is free again.
        let after = t0 + FAIL_DELAY_NS + 1;
        assert_eq!(s.get(a).unwrap().pacing.check_at(after), after);
        assert_eq!(Pacing::default().check_at(0), 0);
    }

    #[test]
    fn session_ids_are_never_reused() {
        let mut s = Sessions::new();
        let a = s.open("alice");
        assert!(s.close(a).is_some());
        assert!(s.get(a).is_none(), "a closed session is gone");
        let b = s.open("alice");
        assert_ne!(a, b, "the next login got the old id");
        assert!(a != 0 && b != 0, "zero never names a session");
    }

    #[test]
    fn a_suffix_names_a_supervisor_a_session_or_nothing() {
        assert_eq!(suffix::parse(b"session"), Suffix::Supervisor);
        assert_eq!(suffix::parse(b"s/7"), Suffix::Client(7));
        assert_eq!(suffix::parse(b"s/18446744073709551615"), Suffix::Client(u64::MAX));
        let overflow = b"s/18446744073709551616";
        let bads =
            [&b""[..], b"s/", b"s/0", b"s/07", b"s/7x", b"s/-1", overflow, b"sessions", b"x/7"];
        for bad in bads {
            assert_eq!(suffix::parse(bad), Suffix::Unknown, "{:?}", core::str::from_utf8(bad));
        }
    }

    #[test]
    fn a_bare_name_is_one_path_component_of_plain_characters() {
        for ok in ["nxsh", "nx-install", "a_b", "v1.2"] {
            assert!(is_bare_name(ok), "{ok}");
        }
        for bad in ["", ".", "..", "a/b", "/bin/sh", "a b", "a*"] {
            assert!(!is_bare_name(bad), "{bad}");
        }
    }
}
