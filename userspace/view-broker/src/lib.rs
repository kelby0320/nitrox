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
//! - [`accounts`] — the guards on removing an account, and what `Accounts` shows of each;
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
        /// **Changing the policy**: the broker's own policy endpoint, bound at `/dev/policy` with
        /// the base `/policy/<session>`, which answers `Show` and `Install` (administration Part
        /// D.2). Nothing else changes `/system/views.toml` on a running system, so **an
        /// administrator is an account that may use this for every program**
        /// ([`Policy::administrators`]).
        Views,
        /// **Administering accounts**: the broker's own accounts endpoint, bound at `/dev/accounts`
        /// with the base `/accounts/<session>`, which answers `AddAccount`, `RemoveAccount` and
        /// `SetPassword` (administration Part D.3). The broker checks the guards and asks
        /// `auth-service`, the accounts' only writer.
        Accounts,
    }

    impl Grant {
        /// The grant a policy spells `name`, if this broker knows it.
        pub fn from_name(name: &str) -> Option<Grant> {
            match name {
                "disks" => Some(Grant::Disks),
                "storage" => Some(Grant::Storage),
                "views" => Some(Grant::Views),
                "accounts" => Some(Grant::Accounts),
                _ => None,
            }
        }

        /// How a policy spells it.
        pub fn name(self) -> &'static str {
            match self {
                Grant::Disks => "disks",
                Grant::Storage => "storage",
                Grant::Views => "views",
                Grant::Accounts => "accounts",
            }
        }
    }

    /// Every grant this broker knows, for the message that refuses one it does not.
    pub const KNOWN_GRANTS: &[Grant] = &[Grant::Disks, Grant::Storage, Grant::Views, Grant::Accounts];

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

        /// **Who, of `accounts`, could administer the system under this policy**: the accounts a
        /// rule lets use, **for every program**, a view whose profile grants [`Grant::Views`].
        ///
        /// **To administer is to be able to change the policy again** (administration Part D.2,
        /// PR #337 review). After Part D nothing but the `views` grant changes it, so a policy
        /// under which no account could use that grant for everything could be replaced only from
        /// the live image. Until D.2 this asked only for a view *named* `admin`, and a policy whose
        /// `admin` profile had lost `views` passed.
        ///
        /// Narrow on purpose (`administration.md` § Policy): a rule granting one program does not
        /// make an administrator, and a broad `who = ["*"]` rule letting everyone power off must
        /// not count, or removing the last real administrator would look safe. **Only accounts
        /// that exist count**: `who = ["kelby"]` names nobody if `kelby` has no account, and
        /// `who = ["*"]` names every account there is. In the order `accounts` gives them.
        pub fn administrators<'a>(&self, accounts: &[&'a str]) -> Vec<&'a str> {
            let grants_views = |view: &String| {
                self.profiles.iter().any(|p| &p.name == view && p.grants.contains(&Grant::Views))
            };
            accounts
                .iter()
                .copied()
                .filter(|a| {
                    self.rules.iter().any(|r| {
                        r.run == Names::Any && r.who.allows(a) && r.views.iter().any(grants_views)
                    })
                })
                .collect()
        }
    }

    /// Judge a policy's text the way `with --check` and `Install` ask: it must read, and **an
    /// account of `accounts` — the ones that exist — must be able to administer under it**
    /// ([`Policy::administrators`]). A policy nobody could administer cannot be changed again
    /// short of the live image.
    pub fn check(text: &str, accounts: &[&str]) -> Result<Policy, PolicyError> {
        let policy = parse(text)?;
        if policy.administrators(accounts).is_empty() {
            return Err(err(
                0,
                String::from(
                    "no account that exists could use `views` for every program, so nothing could \
                     change this policy again — only the live image could",
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

        /// How many sessions are open for `principal`.
        pub fn count_for(&self, principal: &str) -> usize {
            self.open.iter().filter(|s| s.principal == principal).count()
        }
    }
}

pub mod accounts {
    //! **Accounts, as the broker fronts them** (administration Part D.3). `auth-service` holds
    //! them and is their only writer; the broker checks what only it knows — the policy, and who
    //! is logged in — before it asks.

    use crate::policy::Policy;
    use crate::sessions::Sessions;
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    /// One account as `Accounts` shows it.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Shown<'a> {
        pub name: &'a str,
        pub home: &'a str,
        /// Sessions open for it.
        pub sessions: usize,
        /// Whether it could administer under the policy as it stands.
        pub administers: bool,
    }

    /// Every account of `accounts` — `(name, home)`, in the service's order — with its open
    /// sessions and whether it could administer under `policy`. **Nobody administers when the
    /// policy does not read** (`None`): nothing can be said of it, and saying "no" is the answer
    /// that does not mislead.
    pub fn shown<'a>(accounts: &'a [(String, String)], sessions: &Sessions, policy: Option<&Policy>) -> Vec<Shown<'a>> {
        let names: Vec<&str> = accounts.iter().map(|(n, _)| n.as_str()).collect();
        let administrators = policy.map(|p| p.administrators(&names)).unwrap_or_default();
        accounts
            .iter()
            .map(|(name, home)| Shown {
                name,
                home,
                sessions: sessions.count_for(name),
                administers: administrators.contains(&name.as_str()),
            })
            .collect()
    }

    /// **Why removing `name` is refused**, if it is. The guards, in the order a person should
    /// hear them:
    /// - no account of `accounts` has that name;
    /// - **it is logged in**: removal waits until every session of the account has ended (the
    ///   maintainer's call in Part D's detail pass, over ending the sessions);
    /// - the policy does not read (`Err`, with its reason), so whether an administrator would
    ///   remain cannot be said — refused rather than guessed;
    /// - **no account left could administer** ([`Policy::administrators`]).
    pub fn refuse_removal(
        name: &str,
        accounts: &[&str],
        sessions: &Sessions,
        policy: Result<&Policy, &str>,
    ) -> Option<String> {
        if !accounts.contains(&name) {
            return Some(format!("no account is named {name}"));
        }
        let open = sessions.count_for(name);
        if open > 0 {
            let plural = if open == 1 { "" } else { "s" };
            return Some(format!(
                "{name} is logged in, in {open} session{plural}; an account can be removed once it has logged out"
            ));
        }
        let policy = match policy {
            Ok(p) => p,
            Err(why) => return Some(format!("{why}, so whether anyone could still administer cannot be said")),
        };
        let remaining: Vec<&str> = accounts.iter().copied().filter(|a| *a != name).collect();
        if policy.administrators(&remaining).is_empty() {
            return Some(format!(
                "removing {name} would leave no account that could use `views` for every program, so nothing \
                 could change the policy again — only the live image could"
            ));
        }
        None
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

pub mod exits {
    //! Which program exited, and with what code: a life channel closing, paired with a
    //! `ChildExited`.
    //!
    //! **Either can come first.** `sys_process_exit` closes the program's handle table, its life
    //! channel's end included, before it queues `ChildExited`, so that a peer's `PeerClosed` is
    //! prompt. On another CPU the broker can see the close while the code is still to come. A
    //! closed channel stays ready, so the broker must also stop waiting on it
    //! ([`Exits::is_closed`]); otherwise every wake until the code arrives reports it again.
    //!
    //! **A first version pushed the life on every report** (administration C.7, found by
    //! `boot-probe` under KVM). A life reported twice left a copy behind. That copy then took the
    //! next program's code and matched no program, and the code was lost. That program's life
    //! then never paired: it was queued again on every wake until the broker ran out of memory.
    //!
    //! Codes are still paired first with first. **Two exits in one wake can swap codes**, which is
    //! the residual of `TODO(child-exit-attribution)`.

    use alloc::collections::VecDeque;

    /// Closed life channels waiting for a code, and codes waiting for a closed life channel.
    #[derive(Debug, Default)]
    pub struct Exits {
        closed: VecDeque<u64>,
        codes: VecDeque<(i32, bool)>,
    }

    impl Exits {
        /// Life channel `life` closed. **Queued once**, however many times it is reported.
        pub fn closed(&mut self, life: u64) {
            if !self.closed.contains(&life) {
                self.closed.push_back(life);
            }
        }

        /// Whether `life` has closed and waits for its code. The broker does not wait on it again.
        pub fn is_closed(&self, life: u64) -> bool {
            self.closed.contains(&life)
        }

        /// A `ChildExited`: its code, and whether the program crashed.
        pub fn code(&mut self, code: i32, crashed: bool) {
            self.codes.push_back((code, crashed));
        }

        /// The next closed life that is still `running`, with its code, taken out. **A closed life
        /// that is no longer running is dropped without taking a code**, which then goes to the
        /// next life. `None` if no pair is ready yet.
        pub fn next(&mut self, running: impl Fn(u64) -> bool) -> Option<(u64, i32, bool)> {
            while let Some(&life) = self.closed.front() {
                if !running(life) {
                    self.closed.pop_front();
                    continue;
                }
                let (code, crashed) = self.codes.pop_front()?;
                self.closed.pop_front();
                return Some((life, code, crashed));
            }
            None
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
        /// `policy/<id>`: the policy endpoint, as the `views` grant binds it into a view of session
        /// `id` (administration Part D.2).
        Policy(u64),
        /// `accounts/<id>`: the accounts endpoint, as the `accounts` grant binds it into a view of
        /// session `id` (administration Part D.3).
        Accounts(u64),
        /// Anything else — answered `NotFound`.
        Unknown,
    }

    /// Classify `suffix`. A session id is decimal, non-zero, with no leading zero, and fits a
    /// `u64` — one spelling per id, so two paths never name the same session.
    pub fn parse(suffix: &[u8]) -> Suffix {
        if suffix == b"session" {
            return Suffix::Supervisor;
        }
        if let Some(digits) = suffix.strip_prefix(b"s/") {
            return session_id(digits).map_or(Suffix::Unknown, Suffix::Client);
        }
        if let Some(digits) = suffix.strip_prefix(b"policy/") {
            return session_id(digits).map_or(Suffix::Unknown, Suffix::Policy);
        }
        if let Some(digits) = suffix.strip_prefix(b"accounts/") {
            return session_id(digits).map_or(Suffix::Unknown, Suffix::Accounts);
        }
        Suffix::Unknown
    }

    /// A session id: decimal, non-zero, with no leading zero, fitting a `u64` — one spelling per
    /// id, so two paths never name the same session.
    fn session_id(digits: &[u8]) -> Option<u64> {
        if digits.is_empty() || digits[0] == b'0' || !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        let mut n: u64 = 0;
        for d in digits {
            n = n.checked_mul(10)?.checked_add((d - b'0') as u64)?;
        }
        Some(n)
    }
}

#[cfg(test)]
mod tests {
    use super::accounts;
    use super::exits::Exits;
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
        // And `views` (administration Part D.2).
        assert_eq!(Grant::from_name("views"), Some(Grant::Views));
        assert_eq!(Grant::Views.name(), "views");
        assert!(e.message.contains("views"), "{e}");
        // And `accounts` (administration Part D.3).
        assert_eq!(Grant::from_name("accounts"), Some(Grant::Accounts));
        assert_eq!(Grant::Accounts.name(), "accounts");
        assert!(e.message.contains("accounts"), "{e}");
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

    /// **The guard at its neighbours.** Only a profile granting `views`, with `run = ["*"]`, for
    /// someone makes an administrator; one step away on each axis does not. Every case parses, so
    /// an empty answer is the guard's and not the reader's.
    #[test]
    fn only_views_for_every_program_makes_an_administrator() {
        let with = |who: &str, view: &str, run: &str| {
            parse(&format!(
                "[profile.admin]\ngrants = [\"disks\", \"views\"]\n[profile.other]\ngrants = [\"disks\"]\n\
                 [[rule]]\nwho = {who}\nuse = [\"{view}\"]\nrun = {run}\nauth = \"password\"\n"
            ))
            .unwrap()
        };
        let accounts = ["alice", "bob"];
        assert_eq!(with("[\"alice\"]", "admin", "[\"*\"]").administrators(&accounts), ["alice"]);
        let everyone = with("[\"*\"]", "admin", "[\"*\"]").administrators(&accounts);
        assert_eq!(everyone, ["alice", "bob"], "everyone is someone");
        assert!(with("[\"alice\"]", "admin", "[\"disk\"]").administrators(&accounts).is_empty(), "one program");
        assert!(with("[\"alice\"]", "other", "[\"*\"]").administrators(&accounts).is_empty(), "no `views` there");
        assert!(with("[]", "admin", "[\"*\"]").administrators(&accounts).is_empty(), "nobody");
        assert!(parse("").unwrap().administrators(&accounts).is_empty(), "an empty policy");
    }

    /// **An administrator is whoever can use `views` for everything, whatever the view is
    /// called** — the review's case, an `admin` profile that lost `views`, is not one, and a
    /// profile under another name that has it is.
    #[test]
    fn what_makes_an_administrator_is_the_views_grant_not_the_name() {
        let policy = |grants: &str, view: &str| {
            parse(&format!(
                "[profile.{view}]\ngrants = [{grants}]\n\
                 [[rule]]\nwho = [\"alice\"]\nuse = [\"{view}\"]\nrun = [\"*\"]\nauth = \"password\"\n"
            ))
            .unwrap()
        };
        let accounts = ["alice"];
        assert!(policy("\"disks\", \"storage\"", "admin").administrators(&accounts).is_empty(), "admin without views");
        assert_eq!(policy("\"views\"", "keeper").administrators(&accounts), ["alice"], "another name, with views");
    }

    /// **Only accounts that exist count**, for each shape of `who`.
    #[test]
    fn an_administrator_is_an_account_that_exists() {
        let policy = |who: &str| {
            parse(&format!(
                "[profile.admin]\ngrants = [\"views\"]\n\
                 [[rule]]\nwho = {who}\nuse = [\"admin\"]\nrun = [\"*\"]\nauth = \"password\"\n"
            ))
            .unwrap()
        };
        assert_eq!(policy("[\"alice\"]").administrators(&["alice", "bob"]), ["alice"]);
        assert!(policy("[\"kelby\"]").administrators(&["alice", "bob"]).is_empty(), "a name with no account");
        assert_eq!(policy("[\"*\"]").administrators(&["alice", "bob"]), ["alice", "bob"], "every account");
        assert!(policy("[\"*\"]").administrators(&[]).is_empty(), "`*` of nobody is nobody");
    }

    /// **`check` is the reader, then the guard**: a policy that reads and leaves an administrator
    /// passes, and one that reads and leaves none is refused for that reason, not another.
    #[test]
    fn check_refuses_a_policy_that_reads_but_leaves_no_administrator() {
        let text = |who: &str| {
            format!(
                "[profile.admin]\ngrants = [\"views\"]\n\
                 [[rule]]\nwho = {who}\nuse = [\"admin\"]\nrun = [\"*\"]\nauth = \"password\"\n"
            )
        };
        assert!(check(&text("[\"alice\"]"), &["alice"]).is_ok());
        let orphaned = text("[\"kelby\"]");
        assert!(parse(&orphaned).is_ok());
        let refused = check(&orphaned, &["alice"]).unwrap_err();
        assert!(refused.message.starts_with("no account that exists could use `views`"), "{refused}");
        assert!(check("[rule]\n", &["alice"]).unwrap_err().line == 1, "one that does not read says where");
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

    /// **A life reported closed twice before its code is queued once**: the case `boot-probe` hit
    /// under KVM. The copy the first version left took the next program's code, matched nothing,
    /// and lost it.
    #[test]
    fn a_life_reported_closed_twice_takes_one_code_and_leaves_the_next_one_alone() {
        let mut e = Exits::default();
        let mut running = alloc::vec![7u64, 9];
        e.closed(7);
        assert!(e.is_closed(7));
        assert_eq!(e.next(|l| running.contains(&l)), None, "no code yet");
        e.closed(7);
        e.code(0, false);
        assert_eq!(e.next(|l| running.contains(&l)), Some((7, 0, false)));
        assert!(!e.is_closed(7));
        running.retain(|&l| l != 7);
        e.closed(9);
        e.code(3, true);
        assert_eq!(e.next(|l| running.contains(&l)), Some((9, 3, true)), "the next program's code is its own");
        assert_eq!(e.next(|_| true), None);
    }

    /// **A closed life whose program is gone takes no code**: the code goes to the next one.
    #[test]
    fn a_life_no_longer_running_takes_no_code() {
        let mut e = Exits::default();
        e.closed(5);
        e.closed(6);
        e.code(1, false);
        assert_eq!(e.next(|l| l == 6), Some((6, 1, false)));
        assert!(!e.is_closed(5), "dropped on the way");
    }

    /// **A code can come first**, and waits for its close.
    #[test]
    fn a_code_before_its_close_waits_for_it() {
        let mut e = Exits::default();
        e.code(2, false);
        assert_eq!(e.next(|_| true), None);
        e.closed(4);
        assert_eq!(e.next(|l| l == 4), Some((4, 2, false)));
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
        // The policy endpoint's base, with the same rule for the id (administration Part D.2).
        assert_eq!(suffix::parse(b"policy/3"), Suffix::Policy(3));
        for bad in [&b"policy/"[..], b"policy/0", b"policy/03", b"policy/x", b"policy", b"policies/3"] {
            assert_eq!(suffix::parse(bad), Suffix::Unknown, "{:?}", core::str::from_utf8(bad));
        }
        // And the accounts endpoint's (administration Part D.3).
        assert_eq!(suffix::parse(b"accounts/12"), Suffix::Accounts(12));
        for bad in [&b"accounts/"[..], b"accounts/0", b"accounts/012", b"accounts/1/x", b"accounts", b"account/1"] {
            assert_eq!(suffix::parse(bad), Suffix::Unknown, "{:?}", core::str::from_utf8(bad));
        }
    }

    /// **The guards on a removal, each at its neighbour** (administration Part D.3): an account
    /// that exists, logged out, whose going leaves someone able to administer, is removed; one step
    /// away on each axis is refused, for its own reason.
    #[test]
    fn a_removal_waits_for_logout_and_leaves_an_administrator() {
        let policy = |who: &str| {
            parse(&alloc::format!(
                "[profile.admin]\ngrants = [\"views\"]\n\
                 [[rule]]\nwho = {who}\nuse = [\"admin\"]\nrun = [\"*\"]\nauth = \"password\"\n"
            ))
            .unwrap()
        };
        let alice_only = policy("[\"alice\"]");
        let accounts = ["alice", "bob"];
        let mut sessions = Sessions::new();
        let refuse =
            |name: &str, s: &Sessions, p: Result<&Policy, &str>| accounts::refuse_removal(name, &accounts, s, p);

        assert_eq!(refuse("bob", &sessions, Ok(&alice_only)), None, "logged out, and alice remains");
        let id = sessions.open("bob");
        let why = refuse("bob", &sessions, Ok(&alice_only)).unwrap();
        assert!(why.contains("bob is logged in, in 1 session;"), "{why}");
        sessions.open("bob");
        assert!(refuse("bob", &sessions, Ok(&alice_only)).unwrap().contains("in 2 sessions"));
        let why = refuse("bob", &sessions, Err("the policy does not read")).unwrap();
        assert!(why.contains("logged in"), "logged in is said first: {why}");
        sessions.close(id);
        sessions.close(id + 1);
        assert_eq!(refuse("bob", &sessions, Ok(&alice_only)), None, "and once logged out, removed");

        let why = refuse("alice", &sessions, Ok(&alice_only)).unwrap();
        assert!(why.starts_with("removing alice would leave no account that could use `views`"), "{why}");
        assert_eq!(refuse("alice", &sessions, Ok(&policy("[\"*\"]"))), None, "bob could administer");
        let why = refuse("carol", &sessions, Ok(&alice_only)).unwrap();
        assert_eq!(why, "no account is named carol");
        let why = refuse("bob", &sessions, Err("the policy does not read")).unwrap();
        assert!(why.ends_with("so whether anyone could still administer cannot be said"), "{why}");

        let last = accounts::refuse_removal("alice", &["alice"], &sessions, Ok(&policy("[\"*\"]")));
        assert!(last.is_some(), "`*` of nobody is nobody: the last account is never removed");
    }

    /// **`Accounts` shows each account's sessions and whether it administers** — and, when the
    /// policy does not read, that nobody does.
    #[test]
    fn accounts_are_shown_with_their_sessions_and_who_administers() {
        let policy = parse(
            "[profile.admin]\ngrants = [\"views\"]\n\
             [[rule]]\nwho = [\"alice\"]\nuse = [\"admin\"]\nrun = [\"*\"]\nauth = \"password\"\n",
        )
        .unwrap();
        let accounts = alloc::vec![
            (String::from("alice"), String::from("/home/alice")),
            (String::from("bob"), String::from("/home/bob")),
        ];
        let mut sessions = Sessions::new();
        sessions.open("bob");
        sessions.open("bob");
        sessions.open("carol");
        let shown = accounts::shown(&accounts, &sessions, Some(&policy));
        let alice = accounts::Shown { name: "alice", home: "/home/alice", sessions: 0, administers: true };
        let bob = accounts::Shown { name: "bob", home: "/home/bob", sessions: 2, administers: false };
        assert_eq!(shown, [alice.clone(), bob.clone()], "carol has a session and no account");
        let unread = accounts::shown(&accounts, &sessions, None);
        assert!(unread.iter().all(|a| !a.administers), "nobody, when the policy does not read");
        assert_eq!(unread[1].sessions, 2);
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
