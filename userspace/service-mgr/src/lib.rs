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
    /// Bounded by its wait set, which holds the notification channel, every route and the control
    /// channel of each server still starting (see [`STARTING_ROOM`]).
    pub const MAX_ROUTES: usize = 16;

    /// The control channels of starting servers the wait set has room for, beside the
    /// notification channel and [`MAX_ROUTES`] routes, within the kernel's 32. One more starting
    /// at once is still seen, on the next pass: the wait is level-triggered and looks at everything.
    pub const STARTING_ROOM: usize = libkern::abi::MAX_WAIT_HANDLES - 1 - MAX_ROUTES;

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
        assert!(1 + MAX_ROUTES + STARTING_ROOM <= libkern::abi::MAX_WAIT_HANDLES);
        assert!(STARTING_ROOM >= 1);
        assert!(9 + DERIVED.len() + 1 <= MAX_ROUTES);
    }
}
