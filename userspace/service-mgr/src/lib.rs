//! service-mgr's host-testable internals.
//!
//! `service-mgr` is a library + binary crate (mirroring init): this library holds the
//! logic that can be unit-tested on the host (the service-declaration parser), while
//! `src/main.rs` is the bare-target supervisor entry point that uses it.
//! `#![no_std]` for the bare build; `std` under `cargo test` so the host harness works
//! (`cargo xtask test` runs `cargo test -p service-mgr --lib`).
//!
//! The bare-target binary provides the `#[global_allocator]` (`libheap`); this library
//! only needs `alloc`. See `docs/architecture/service-manager.md`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod service_toml;

pub mod bringup {
    //! **The order the boot starts things in** (administration Part E.1).
    //!
    //! Declarations start in file order, a server's `Meta::Ready` awaited before the next starts
    //! — which keeps the orders `init`'s comments called load-bearing. The login chain is not a
    //! declaration, so where it goes is decided here: **after the last server and before
    //! anything else**. Its supervisors need the servers, and a test image's clients must start
    //! after the greeter, which is what keeps `check-display`'s reference windows on top.

    /// What a declaration contributes to the order.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Entry {
        /// It declares an `endpoint`: `service-mgr` waits for its `Ready` before the next.
        pub server: bool,
        /// It is `critical`: the boot cannot go on without it.
        pub critical: bool,
    }

    /// The index the login chain starts before: just after the last server, or first if there
    /// is none.
    pub fn chain_at(entries: &[Entry]) -> usize {
        entries.iter().rposition(|e| e.server).map_or(0, |i| i + 1)
    }

    /// What to do next.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Step {
        /// Start declaration `i`.
        Start(usize),
        /// Start the login chain.
        LoginChain,
        /// Everything has been started.
        Done,
    }

    /// The next step, with `started` declarations started and the login chain started or not.
    /// The caller asks only when nothing is being waited for.
    pub fn next(entries: &[Entry], started: usize, chain_started: bool) -> Step {
        if !chain_started && started >= chain_at(entries) {
            return Step::LoginChain;
        }
        if started < entries.len() { Step::Start(started) } else { Step::Done }
    }

    /// What a server that did not come up at boot means.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Failed {
        /// Start nothing more, and ask `init` for the emergency shell. The console is still free:
        /// the critical servers start before the terminal server.
        Emergency,
        /// Report it, and go on.
        Continue,
    }

    /// The consequence of `entry` failing to come up at boot.
    pub fn failed_at_boot(entry: Entry) -> Failed {
        if entry.critical { Failed::Emergency } else { Failed::Continue }
    }

    /// **Why a set of declarations is not one to boot with**, or `None` (PR #340 review, finding
    /// 2). `service-mgr` then starts nothing and asks `init` for the emergency shell, as for a
    /// critical server that did not come up — which each of these is, before it is started:
    /// - a declaration that said `critical = true` and could not be read (`skipped_critical`);
    /// - no declarations at all: a missing, unreadable or unparseable file;
    /// - none that is critical. The two critical servers are what a boot cannot go on without,
    ///   and a file declaring neither has lost them rather than meant it.
    ///
    /// Before administration Part E.1, `init` started those two unconditionally and took the
    /// emergency path if either failed, so no edit to a file could lose them.
    pub fn unfit(entries: &[Entry], skipped_critical: bool) -> Option<&'static str> {
        if skipped_critical {
            Some("a critical declaration could not be read")
        } else if entries.is_empty() {
            Some("no declaration could be read")
        } else if !entries.iter().any(|e| e.critical) {
            Some("no declaration is critical")
        } else {
            None
        }
    }
}

pub mod registry {
    //! **The registry**: where `service-mgr` binds each server's endpoint, under the server's
    //! name, in a namespace of its own (administration Part E.1). Every other binding of a
    //! server's path is one of `service-mgr`'s own endpoints — a **route** — which answers every
    //! resolve with `SUBNAMESPACE` into the registry at one place, and the resolve continues into
    //! whichever server is bound there now. A restart rebinds there and nowhere else.
    //!
    //! **A route reaches one server, never more** (Part E.1b). A single endpoint for every server,
    //! routing on the suffix, is what Part E.1a's root used, and it could not be handed to a
    //! session: `desktop-shell` holds what it is handed with `BIND_NAMESPACE`, and could have bound
    //! it with any base — `/auth-service/admin`, or `/device-mgr/input`. A route is attenuation by
    //! construction, as the device manager's info-only endpoint is.

    use alloc::string::String;

    /// Routes `service-mgr` serves at once: one per server, and one per [`DERIVED`] endpoint.
    /// Bounded by its wait set, which holds the notification channel, every route, the handles
    /// that serve `/svc/services` ([`crate::services::SLOTS`]) and the control channel of each
    /// server still starting (see [`STARTING_ROOM`]). Sixteen until administration Part E.2 made
    /// room for the services.
    pub const MAX_ROUTES: usize = 14;

    /// The control channels of starting servers the wait set has room for, beside the
    /// notification channel, [`MAX_ROUTES`] routes and the services' handles, within the kernel's
    /// 32. One more starting at once is still seen, on the next pass: the wait is level-triggered
    /// and looks at everything.
    pub const STARTING_ROOM: usize =
        libkern::abi::MAX_WAIT_HANDLES - 1 - MAX_ROUTES - crate::services::SLOTS;

    /// **Endpoints a server mints for sessions**, each reached through a route of its own
    /// (Part E.1b): `(server, suffix)` — resolving `suffix` on the server answers a forwarding
    /// endpoint of its own, narrower than its root one. `service-mgr` resolves it each time the
    /// server comes up, binds it at [`derived`], and hands the login supervisors the route to it,
    /// so a restart re-derives it and every session reaches the new one.
    /// - `device-mgr`'s `info-endpoint` answers its tables and never a class (Part B.4);
    /// - `storage-service`'s `session-endpoint` answers the filesystems and the tables and never an
    ///   admin endpoint (Part C.6).
    pub const DERIVED: &[(&str, &str)] =
        &[("device-mgr", "info-endpoint"), ("storage-service", "session-endpoint")];

    /// The suffixes `name` derives endpoints from, in [`DERIVED`]'s order.
    pub fn derives(name: &str) -> impl Iterator<Item = &'static str> + '_ {
        DERIVED.iter().filter(move |(n, _)| *n == name).map(|&(_, s)| s)
    }

    /// Where a derived endpoint is bound in the registry: `/<name>.<suffix>`. **The `.` is what
    /// keeps it apart from every server**: no name the registry binds has one ([`valid_name`]).
    pub fn derived(name: &str, suffix: &str) -> String {
        let mut b = base(name);
        b.push('.');
        b.push_str(suffix);
        b
    }

    /// A name the registry binds a server under: 1 to 32 bytes of lowercase letters, digits and
    /// `-`, as the declarations' names are.
    pub fn valid_name(name: &str) -> bool {
        (1..=32).contains(&name.len())
            && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    /// Where `name` is bound in the registry: its route continues every resolve there.
    pub fn base(name: &str) -> String {
        let mut b = String::from("/");
        b.push_str(name);
        b
    }
}

pub mod services {
    //! **`/svc/services`** (administration Part E.2): the list of services, for anyone, and
    //! starting, stopping and restarting them, on an admin session.
    //!
    //! - The list is a table, `all.tsm`, resolved from `/svc/services` or a session endpoint's
    //!   `/dev/services`, as the device manager's and the storage service's are: `name`, `state`
    //!   and `restarts`.
    //! - `admin-endpoint`, resolved from `/svc/services` only, mints a forwarding endpoint on which
    //!   any resolve opens an admin session. The view broker's `services` grant binds one at
    //!   `/dev/services/admin`.
    //! - On an admin session, `Start`, `Stop` and `Restart` (`librsproto::services`), each answered
    //!   once it has happened.

    use alloc::string::String;
    use alloc::vec::Vec;
    use libkern::error::KError;
    use libstream::wire::{Schema, StreamFlags, Table, TypeModifiers, TypeTag, Value};

    /// Admin endpoints at once. The view broker asks for one the first time a view needs it; the
    /// second is headroom.
    pub const MAX_ADMIN_ENDPOINTS: usize = 2;
    /// Admin sessions open at once: a `service --stop` or `--restart` is one, briefly.
    pub const MAX_ADMIN_SESSIONS: usize = 4;
    /// Wait-set slots the services take: `/svc/services`' own endpoint, the session endpoint, and
    /// the admin endpoints and sessions.
    pub const SLOTS: usize = 2 + MAX_ADMIN_ENDPOINTS + MAX_ADMIN_SESSIONS;

    /// Where a service is, as the table says it.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum State {
        /// Spawned, and a server whose `Meta::Ready` has not come.
        Starting,
        /// Running.
        Running,
        /// Not running, and not for a failure: asked to stop, finished cleanly, or never started.
        Stopped,
        /// Not running, because it failed: an exit other than `0`, or one whose code never came.
        Failed,
    }

    impl State {
        /// The word the table uses.
        pub fn word(self) -> &'static str {
            match self {
                State::Starting => "starting",
                State::Running => "running",
                State::Stopped => "stopped",
                State::Failed => "failed",
            }
        }
    }

    /// A service's state: whether it is `running` and still `starting`, or `usable` — a server
    /// bound, or any other service — whether its stop was `requested`, and how it last `exited`:
    /// `None` if it never has, `Some(None)` if its code never came.
    ///
    /// **Running and not usable is `failed`** (PR #341 review, finding 7): a server that refused,
    /// or sent no `Meta::Ready` in time, is still a process, and its path answers `NotFound`.
    pub fn state(
        running: bool,
        starting: bool,
        usable: bool,
        requested: bool,
        exited: Option<Option<i32>>,
    ) -> State {
        match (running, starting, usable) {
            (true, true, _) => State::Starting,
            (true, false, true) => State::Running,
            (true, false, false) => State::Failed,
            _ if requested => State::Stopped,
            _ => match exited {
                None | Some(Some(0)) => State::Stopped,
                Some(_) => State::Failed,
            },
        }
    }

    /// The table's schema: `name`, `state`, `restarts`.
    pub fn schema() -> Schema {
        Schema::new()
            .field("name", TypeTag::String, TypeModifiers::NONE)
            .field("state", TypeTag::String, TypeModifiers::NONE)
            .field("restarts", TypeTag::Int, TypeModifiers::NONE)
    }

    /// `all.tsm`: a row per service, in the declarations' order.
    pub fn table(rows: &[(&str, State, u32)]) -> Vec<u8> {
        let rows = rows
            .iter()
            .map(|&(name, state, restarts)| {
                alloc::vec![
                    Value::Str(String::from(name)),
                    Value::Str(String::from(state.word())),
                    Value::Int(restarts as i64),
                ]
            })
            .collect();
        let mut out = Vec::new();
        // A `Vec` sink cannot fail, and every row has the schema's shape by construction.
        let _ = Table { flags: StreamFlags::NONE, schema: schema(), rows }.encode(&mut out);
        out
    }

    /// What a suffix on `/svc/services` or a session endpoint asks for.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Asked {
        /// `all.tsm`: the table.
        Table,
        /// `admin-endpoint`: an admin endpoint. **Only on `/svc/services`**: a session endpoint
        /// answers it `NotFound`, so a session cannot reach starting and stopping at all.
        AdminEndpoint,
        /// Anything else.
        Unknown,
    }

    /// Classify `suffix`, as asked on a `session` endpoint or on `/svc/services` itself.
    pub fn asked(suffix: &[u8], session: bool) -> Asked {
        match suffix {
            b"all.tsm" => Asked::Table,
            b"admin-endpoint" if !session => Asked::AdminEndpoint,
            _ => Asked::Unknown,
        }
    }

    /// What `service-mgr` does for an admin request.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Action {
        /// Start it; answer once it is up.
        Start,
        /// Ask it to stop; answer once it has exited.
        Stop,
        /// Ask it to stop, then start it; answer once the new one is up.
        StopThenStart,
        /// **Cancel the restart its policy has scheduled**, and answer at once: a service in its
        /// backoff is not running, and a stop is what keeps it from running again (PR #341 review,
        /// finding 6). Without this a crash-looping service could not be stopped.
        CancelRestart,
    }

    /// What `decide` needs of the service a request names.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Found {
        pub essential: bool,
        pub running: bool,
        /// Whether its policy has a restart scheduled: it exited, and its backoff is not over.
        pub restart_due: bool,
    }

    /// The request `op` (`librsproto::services`) for the service `found`, or none: what to do, or
    /// why not.
    pub fn decide(op: u16, found: Option<Found>) -> Result<Action, (KError, &'static str)> {
        use librsproto::services::{OP_SERVICES_RESTART, OP_SERVICES_START, OP_SERVICES_STOP};
        let Some(Found { essential, running, restart_due }) = found else {
            return Err((KError::NotFound, "no service is declared by that name"));
        };
        match op {
            OP_SERVICES_START if running => Err((KError::AlreadyExists, "it is already running")),
            OP_SERVICES_START => Ok(Action::Start),
            OP_SERVICES_STOP | OP_SERVICES_RESTART if essential => Err((
                KError::NoAccess,
                "it is essential: without it the system cannot be administered, or loses what it holds",
            )),
            OP_SERVICES_STOP if !running && restart_due => Ok(Action::CancelRestart),
            OP_SERVICES_STOP if !running => Err((KError::InvalidArgument, "it is not running")),
            OP_SERVICES_STOP => Ok(Action::Stop),
            OP_SERVICES_RESTART if running => Ok(Action::StopThenStart),
            OP_SERVICES_RESTART => Ok(Action::Start),
            _ => Err((KError::Unsupported, "not a Services request")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::bringup::{Entry, Failed, Step, chain_at, failed_at_boot, next};
    use super::registry::{
        DERIVED, MAX_ROUTES, STARTING_ROOM, base, derived, derives, valid_name,
    };

    const S: Entry = Entry { server: true, critical: false };
    const C: Entry = Entry { server: true, critical: true };
    const N: Entry = Entry { server: false, critical: false };

    /// Walk the steps as the event loop takes them, starting each thing as it is named.
    fn order(entries: &[Entry]) -> std::vec::Vec<Step> {
        let (mut started, mut chain, mut out) = (0, false, std::vec::Vec::new());
        loop {
            let step = next(entries, started, chain);
            out.push(step);
            match step {
                Step::Start(_) => started += 1,
                Step::LoginChain => chain = true,
                Step::Done => return out,
            }
        }
    }

    /// **The login chain goes after the last server and before everything else**: its
    /// supervisors need the servers, and a test image's clients start after the greeter.
    #[test]
    fn the_login_chain_starts_after_the_last_server_and_before_the_rest() {
        use Step::*;
        assert_eq!(
            order(&[C, S, S, N, N]),
            [Start(0), Start(1), Start(2), LoginChain, Start(3), Start(4), Done]
        );
        assert_eq!(chain_at(&[C, S, S, N, N]), 3);
        // A server declared after a non-server still counts: the chain waits for it.
        assert_eq!(order(&[S, N, S, N]), [Start(0), Start(1), Start(2), LoginChain, Start(3), Done]);
        // No servers at all, as before Part E: the chain first.
        assert_eq!(order(&[N, N]), [LoginChain, Start(0), Start(1), Done]);
        assert_eq!(order(&[]), [LoginChain, Done]);
        assert_eq!(order(&[S]), [Start(0), LoginChain, Done]);
    }

    /// **What each admin request does, and what refuses it** (administration Part E.2).
    #[test]
    fn an_admin_request_is_decided_by_the_service_it_names() {
        use super::services::{Action, Found, decide};
        use librsproto::services::{
            OP_SERVICES_RESTART as RESTART, OP_SERVICES_START as START, OP_SERVICES_STOP as STOP,
        };
        use libkern::error::KError;
        let found = |essential, running, restart_due| Some(Found { essential, running, restart_due });
        let (plain_up, plain_down) = (found(false, true, false), found(false, false, false));
        let essential_up = found(true, true, false);
        assert_eq!(decide(START, plain_down), Ok(Action::Start));
        assert_eq!(decide(START, plain_up).map_err(|e| e.0), Err(KError::AlreadyExists));
        assert_eq!(decide(STOP, plain_up), Ok(Action::Stop));
        assert_eq!(decide(STOP, plain_down).map_err(|e| e.0), Err(KError::InvalidArgument));
        assert_eq!(decide(RESTART, plain_up), Ok(Action::StopThenStart));
        assert_eq!(decide(RESTART, plain_down), Ok(Action::Start), "a stopped one's restart starts it");
        // An essential service is refused a stop and a restart, running or not, and not a start.
        for op in [STOP, RESTART] {
            assert_eq!(decide(op, essential_up).map_err(|e| e.0), Err(KError::NoAccess));
            assert_eq!(decide(op, found(true, false, false)).map_err(|e| e.0), Err(KError::NoAccess));
        }
        assert_eq!(decide(START, found(true, false, false)), Ok(Action::Start));
        // **A stop during a backoff cancels the restart**, where "not running" would let it run.
        let backing_off = found(false, false, true);
        assert_eq!(decide(STOP, backing_off), Ok(Action::CancelRestart));
        assert_eq!(decide(START, backing_off), Ok(Action::Start));
        assert_eq!(decide(RESTART, backing_off), Ok(Action::Start));
        assert_eq!(decide(STOP, found(true, false, true)).map_err(|e| e.0), Err(KError::NoAccess));
        assert_eq!(decide(STOP, None).map_err(|e| e.0), Err(KError::NotFound));
        assert_eq!(decide(0x1103, plain_up).map_err(|e| e.0), Err(KError::Unsupported));
    }

    /// **A service's state**, as the table says it.
    #[test]
    fn a_state_is_told_from_how_a_service_last_ended() {
        use super::services::{State, state};
        assert_eq!(state(true, true, false, false, None), State::Starting);
        assert_eq!(state(true, false, true, false, Some(Some(1))), State::Running, "running is running");
        assert_eq!(state(true, false, false, false, None), State::Failed, "no Ready in time");
        assert_eq!(state(false, false, false, true, Some(Some(1))), State::Stopped, "asked to stop");
        assert_eq!(state(false, false, false, false, None), State::Stopped, "never started");
        assert_eq!(state(false, false, false, false, Some(Some(0))), State::Stopped, "finished cleanly");
        assert_eq!(state(false, false, false, false, Some(Some(-1))), State::Failed);
        let never_came = state(false, false, false, false, Some(None));
        assert_eq!(never_came, State::Failed, "a code that never came");
    }

    /// **The table is a TSM1 table** a reader decodes, a row per service, and a session endpoint
    /// cannot mint an admin endpoint.
    #[test]
    fn the_list_is_a_table_and_a_session_cannot_reach_the_admin_endpoint() {
        use super::services::{Asked, State, asked, table};
        let bytes = table(&[("tty-server", State::Running, 0), ("heartbeat", State::Failed, 3)]);
        let t = libstream::wire::Table::decode(&bytes).expect("a table");
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[1][0], libstream::wire::Value::Str("heartbeat".into()));
        assert_eq!(t.rows[1][1], libstream::wire::Value::Str("failed".into()));
        assert_eq!(t.rows[1][2], libstream::wire::Value::Int(3));
        assert_eq!(asked(b"all.tsm", true), Asked::Table);
        assert_eq!(asked(b"admin-endpoint", false), Asked::AdminEndpoint);
        assert_eq!(asked(b"admin-endpoint", true), Asked::Unknown);
        assert_eq!(asked(b"", false), Asked::Unknown);
    }

    /// **A file that has lost its critical servers is an emergency**, however it lost them.
    #[test]
    fn declarations_without_a_critical_server_are_unfit_to_boot_with() {
        use super::bringup::unfit;
        assert_eq!(unfit(&[C, S, N], false), None);
        assert_eq!(unfit(&[S, C], false), None, "critical anywhere counts");
        assert_eq!(unfit(&[], false), Some("no declaration could be read"));
        assert_eq!(unfit(&[S, S, N], false), Some("no declaration is critical"));
        assert_eq!(unfit(&[C, S], true), Some("a critical declaration could not be read"));
        assert_eq!(unfit(&[], true), Some("a critical declaration could not be read"));
    }

    #[test]
    fn only_a_critical_server_stops_the_boot() {
        assert_eq!(failed_at_boot(C), Failed::Emergency);
        assert_eq!(failed_at_boot(S), Failed::Continue);
    }

    /// **Each server has one place in the registry, and each derived endpoint another** that no
    /// server's name can take.
    #[test]
    fn a_derived_endpoint_is_bound_where_no_server_can_be() {
        assert_eq!(base("auth-service"), "/auth-service");
        assert!(valid_name(&"a".repeat(32)) && !valid_name(&"a".repeat(33)));
        for bad in ["", "Upper", "a b", "a/b", "a.b", ".."] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert_eq!(derived("device-mgr", "info-endpoint"), "/device-mgr.info-endpoint");
        for &(name, suffix) in DERIVED {
            assert!(valid_name(name), "{name}");
            let at = derived(name, suffix);
            assert!(!valid_name(&at[1..]), "{at} is a name a server could have");
            assert!(!suffix.contains('/') && !suffix.is_empty(), "{suffix}");
        }
        assert_eq!(derives("device-mgr").collect::<std::vec::Vec<_>>(), ["info-endpoint"]);
        assert_eq!(derives("storage-service").collect::<std::vec::Vec<_>>(), ["session-endpoint"]);
        assert_eq!(derives("tty-server").count(), 0);
    }

    /// **The wait set fits the kernel's**, with room for a server starting — and the nine servers,
    /// their derived endpoints and a test image's server fit the routes.
    #[test]
    fn the_routes_and_a_starting_server_fit_one_wait() {
        let used = 1 + MAX_ROUTES + crate::services::SLOTS + STARTING_ROOM;
        assert!(used <= libkern::abi::MAX_WAIT_HANDLES);
        assert!(STARTING_ROOM >= 1);
        assert!(9 + DERIVED.len() + 1 <= MAX_ROUTES);
    }
}
