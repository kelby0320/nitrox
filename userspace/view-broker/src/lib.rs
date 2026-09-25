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
//! - [`slots`] — room in the one wait set, counted so a client let in can always start;
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

    /// Something a profile grants. **Part A knew one**; each later part adds its own, and a
    /// policy naming a grant this broker does not know is refused rather than ignored — a grant
    /// silently dropped is an administrator who believes they can do something they cannot.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Grant {
        /// Every block device **not in use**, raw: `/dev/blk/<n>` and its `info`, bound one by
        /// one. Since administration Part C.6 the storage service's `InUse` is asked first, and a
        /// mounted filesystem's device and the disk under it are left out.
        Disks,
        /// Mounting and unmounting: the storage service's admin endpoint, bound at
        /// `/dev/storage/admin` (administration Part C.6).
        Storage,
    }

    impl Grant {
        /// The grant a policy spells `name`, if this broker knows it.
        pub fn from_name(name: &str) -> Option<Grant> {
            match name {
                "disks" => Some(Grant::Disks),
                "storage" => Some(Grant::Storage),
                _ => None,
            }
        }

        /// How a policy spells it.
        pub fn name(self) -> &'static str {
            match self {
                Grant::Disks => "disks",
                Grant::Storage => "storage",
            }
        }
    }

    /// Every grant this broker knows, for the message that refuses one it does not.
    pub const KNOWN_GRANTS: &[Grant] = &[Grant::Disks, Grant::Storage];

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
        /// A profile, by index, and whether its `grants` has been given — which its grants
        /// cannot say, since `grants = []` gives none.
        Profile(usize, bool),
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
                section = Section::Profile(policy.profiles.len() - 1, false);
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
                Section::Profile(p, given) => {
                    if key != "grants" {
                        return Err(err(line, format!("a profile has `grants`, not `{key}`")));
                    }
                    if core::mem::replace(given, true) {
                        return Err(err(line, String::from("`grants` is given twice")));
                    }
                    let profile = &mut policy.profiles[*p];
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
    //! supervisors' logins; it keeps a check in [`Held`] until its session's
    //! [`Pacing::check_at`] and waits on its other channels meanwhile.

    use crate::sessions::Sessions;
    use alloc::vec::Vec;

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

    /// Passwords waiting to be checked, across every session, oldest first — each named by a
    /// key the broker chooses (its client's channel) and the session it arrived in.
    ///
    /// **A held check has no deadline of its own.** When one may run is asked of its session
    /// each time, so a failure on any request holds every check still waiting in that session —
    /// including those that arrived before it, whose delay had seemed to end sooner. A deadline
    /// fixed when a password arrived would let a program queue a password on each of several
    /// requests during one delay and have them all checked the moment it ended.
    #[derive(Debug, Default)]
    pub struct Held {
        waiting: Vec<(u64, u64)>,
    }

    impl Held {
        /// Hold a check for `key`, in `session`, behind every one already held.
        pub fn hold(&mut self, key: u64, session: u64) {
            self.waiting.push((key, session));
        }

        /// Drop `key`'s check, if one is held: its request ended some other way.
        pub fn forget(&mut self, key: u64) {
            self.waiting.retain(|&(k, _)| k != key);
        }

        /// The oldest held check its session lets run at `now`, taken out. A check whose session
        /// has closed is dropped on the way. **Take one, make the check, and ask again** — the
        /// check's failure is what holds the next.
        pub fn take_ready(&mut self, sessions: &Sessions, now: u64) -> Option<u64> {
            let mut i = 0;
            while i < self.waiting.len() {
                let (key, session) = self.waiting[i];
                match sessions.get(session) {
                    None => {
                        self.waiting.remove(i);
                    }
                    Some(s) if s.pacing.check_at(now) <= now => {
                        self.waiting.remove(i);
                        return Some(key);
                    }
                    Some(_) => i += 1,
                }
            }
            None
        }

        /// The soonest a held check may run, if any is held: when to wake.
        pub fn next_due(&self, sessions: &Sessions, now: u64) -> Option<u64> {
            self.waiting
                .iter()
                .filter_map(|&(_, session)| sessions.get(session))
                .map(|s| s.pacing.check_at(now))
                .min()
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

pub mod slots {
    //! Room in the broker's one wait set, which holds at most `MAX_WAIT_HANDLES` handles.
    //!
    //! **A client is two slots from the moment it is let in**: its channel, and the life channel
    //! of the program it may start. Counting only what is open would admit a channel into the
    //! last slot and leave its program's life channel nowhere to go — and a life channel the
    //! broker does not wait on is an exit it never sees, whose code is then paired with some other
    //! program's.

    /// The forwarding endpoint and the notification channel.
    pub const FIXED: usize = 2;

    /// What the wait set holds, by how many slots each may come to need.
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    pub struct Load {
        /// Supervisors' channels: one slot each.
        pub supervisors: usize,
        /// Clients that may yet start a program, or are running one: two slots each.
        pub clients: usize,
        /// Handles that stay one slot: a client whose request is over, or a running program
        /// whose client has gone.
        pub singles: usize,
    }

    impl Load {
        /// The most slots this load can come to need.
        pub fn worst(&self) -> usize {
            FIXED + self.supervisors + 2 * self.clients + self.singles
        }

        /// Whether another supervisor's channel fits in `max`.
        pub fn admits_supervisor(&self, max: usize) -> bool {
            self.worst() + 1 <= max
        }

        /// Whether another client fits in `max` — with room for the program it may start.
        pub fn admits_client(&self, max: usize) -> bool {
            self.worst() + 2 <= max
        }
    }
}

pub mod suffix {
    //! What a resolve that reached the broker asked for.
    //!
    //! **This is where identity comes from.** A session's `/dev/views` is the broker's forwarding
    //! endpoint bound with the base `/s/<session>`, so a resolve from inside it arrives with the
    //! suffix `s/<session>` — and no program in the session can change the base, except
    //! `desktop-shell`, which holds the raw endpoint to bind it into its applications. A
    //! supervisor, holding the unscoped `/svc/views`, resolves `session`.

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
    use super::pacing::{FAIL_DELAY_NS, Held, Pacing};
    use super::policy::*;
    use super::sessions::Sessions;
    use super::slots::{FIXED, Load};
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

    /// **Two rules that both match, with different `auth`, in both orders** — the only fixture
    /// that tells the first match from the last. Were the last to decide, the broad passwordless
    /// rule below would let alice skip her password for `nxinstall` (PR #329 review, finding 2).
    #[test]
    fn a_request_is_decided_by_the_first_rule_that_matches_all_three() {
        let p = parse(SEED).unwrap();
        assert!(matches!(
            p.decide("alice", "admin", "nxsh"),
            Decision::Allow { auth: Auth::Password, rule_line: 9, .. }
        ));
        assert!(matches!(p.decide("bob", "install", "nxinstall"), Decision::Allow { .. }));
        let strict = "[[rule]]\nwho = [\"alice\"]\nuse = [\"admin\"]\nrun = [\"*\"]\n\
                      auth = \"password\"\n";
        let lax = "[[rule]]\nwho = [\"*\"]\nuse = [\"admin\"]\nrun = [\"nxinstall\"]\n\
                   auth = \"none\"\n";
        let profile = "[profile.admin]\ngrants = [\"disks\"]\n";
        let decide = |first: &str, second: &str| {
            let p = parse(&format!("{profile}{first}{second}")).unwrap();
            match p.decide("alice", "admin", "nxinstall") {
                Decision::Allow { auth, rule_line, .. } => (auth, rule_line),
                d => panic!("{d:?}"),
            }
        };
        assert_eq!(decide(strict, lax), (Auth::Password, 3), "the strict rule is first");
        assert_eq!(decide(lax, strict), (Auth::None, 3), "the lax rule is first");
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

    /// **`storage` is a grant** (administration Part C.6), named beside `disks` and kept in the
    /// order the policy gives, and the refusal of an unknown grant names it among the ones there are.
    #[test]
    fn storage_is_a_grant_a_policy_can_name() {
        let p = parse("[profile.admin]\ngrants = [\"disks\", \"storage\"]\n").unwrap();
        assert_eq!(p.profiles[0].grants, [Grant::Disks, Grant::Storage]);
        assert_eq!(Grant::from_name("storage"), Some(Grant::Storage));
        assert_eq!(Grant::Storage.name(), "storage");
        let e = parse("[profile.admin]\ngrants = [\"power\"]\n").unwrap_err();
        assert!(e.message.contains("disks") && e.message.contains("storage"), "{e}");
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
        bad("[profile.a]\ngrants = [\"disks\"]\ngrants = [\"disks\"]\n", 3, "given twice");
        bad("[profile.a]\ngrants = []\ngrants = [\"disks\"]\n", 3, "given twice");
        bad("[[rule]]\nwho = [\"a\"]\nwho = [\"b\"]\n", 3, "given twice");
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

    /// **One guess per delay, however many requests.** Passwords queued on several requests
    /// during one delay are checked one per delay, not all at its end — a failure holds the checks that arrived before
    /// it as well as after.
    #[test]
    fn held_checks_in_a_session_run_one_per_delay_however_many_are_queued() {
        let mut s = Sessions::new();
        let a = s.open("alice");
        let b = s.open("bob");
        let mut held = Held::default();
        let t0 = 1_000;
        // One request in `a` fails at t0; during the delay, three more queue a password each, and
        // one in `b` does.
        s.get_mut(a).unwrap().pacing.failed(t0);
        for key in [1, 2, 3] {
            held.hold(key, a);
        }
        held.hold(9, b);
        let half = t0 + FAIL_DELAY_NS / 2;
        assert_eq!(held.take_ready(&s, half), Some(9), "another session is not held");
        assert_eq!(held.take_ready(&s, half), None);
        assert_eq!(held.next_due(&s, half), Some(t0 + FAIL_DELAY_NS));
        // The delay ends: the oldest runs, and fails.
        let t1 = t0 + FAIL_DELAY_NS;
        assert_eq!(held.take_ready(&s, t1), Some(1));
        s.get_mut(a).unwrap().pacing.failed(t1);
        // The other two arrived before that failure and wait for it all the same.
        assert_eq!(held.take_ready(&s, t1), None, "a second guess in the same delay");
        assert_eq!(held.next_due(&s, t1), Some(t1 + FAIL_DELAY_NS));
        let t2 = t1 + FAIL_DELAY_NS;
        assert_eq!(held.take_ready(&s, t2), Some(2));
        // A success holds nothing, so the next may run at once.
        assert_eq!(held.take_ready(&s, t2), Some(3));
        assert_eq!(held.next_due(&s, t2), None);
    }

    #[test]
    fn a_held_check_goes_with_its_request_or_its_session() {
        let mut s = Sessions::new();
        let a = s.open("alice");
        let b = s.open("bob");
        let mut held = Held::default();
        held.hold(1, a);
        held.hold(2, b);
        held.hold(3, b);
        held.forget(2);
        assert_eq!(held.take_ready(&s, 5), Some(1));
        assert_eq!(held.take_ready(&s, 5), Some(3), "a forgotten check is not taken");
        held.hold(4, a);
        s.close(a);
        assert_eq!(held.next_due(&s, 5), None, "a closed session's check is no reason to wake");
        assert_eq!(held.take_ready(&s, 5), None);
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

    /// **At the neighbour.** With two slots free a client is let in and with one it is not;
    /// either way, every client admitted can start its program without the wait set outgrowing
    /// `max`.
    #[test]
    fn a_client_is_admitted_only_with_room_for_its_program_too() {
        const MAX: usize = 32;
        let mut load = Load::default();
        while load.admits_client(MAX) {
            load.clients += 1;
        }
        assert_eq!(load.clients, 15, "(32 - 2) / 2");
        // Every one of them starts a program: channel and life channel each.
        let running = FIXED + 2 * load.clients;
        assert!(running <= MAX, "{running} handles in a {MAX}-slot wait set");
        // Exactly two slots free: one more client fits.
        let two = Load { clients: 14, ..Load::default() };
        assert_eq!(two.worst(), MAX - 2);
        assert!(two.admits_client(MAX));
        // One free: a supervisor fits, and half a client does not.
        let one = Load { singles: 1, ..two };
        assert_eq!(one.worst(), MAX - 1);
        assert!(!one.admits_client(MAX), "its program's life channel would have no slot");
        assert!(one.admits_supervisor(MAX));
        // None free: nothing fits.
        let none = Load { supervisors: 1, ..one };
        assert!(!none.admits_supervisor(MAX));
        assert!(!none.admits_client(MAX));
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
