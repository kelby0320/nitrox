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
}

pub mod registry {
    //! **The registry**: where `service-mgr` binds each server's endpoint, under the server's
    //! name, in a namespace of its own (administration Part E.1). Every other binding of a
    //! server's path is `service-mgr`'s endpoint with `/<name>` as its base, so a forwarded
    //! resolve arrives with the name first. `service-mgr` answers `SUBNAMESPACE` — the registry,
    //! at `/<name>` — and the resolve continues into whichever server is bound there now. A
    //! restart rebinds there and nowhere else.

    use alloc::string::String;

    /// A name the registry binds a server under: 1 to 32 bytes of lowercase letters, digits and
    /// `-`, as the declarations' names are.
    pub fn valid_name(name: &str) -> bool {
        (1..=32).contains(&name.len())
            && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    }

    /// The server a forwarded suffix names — its first component — and how many bytes of the
    /// suffix that is. `None` for an empty or invalid name.
    pub fn route(suffix: &[u8]) -> Option<(&str, usize)> {
        let end = suffix.iter().position(|&b| b == b'/').unwrap_or(suffix.len());
        let name = core::str::from_utf8(&suffix[..end]).ok()?;
        valid_name(name).then_some((name, end))
    }

    /// Where `name` is bound in the registry, which is also the base its bindings forward with.
    pub fn base(name: &str) -> String {
        let mut b = String::from("/");
        b.push_str(name);
        b
    }
}

#[cfg(test)]
mod tests {
    use super::bringup::{Entry, Failed, Step, chain_at, failed_at_boot, next};
    use super::registry::{base, route, valid_name};

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

    #[test]
    fn only_a_critical_server_stops_the_boot() {
        assert_eq!(failed_at_boot(C), Failed::Emergency);
        assert_eq!(failed_at_boot(S), Failed::Continue);
    }

    /// **A forwarded suffix names its server first**, and the rest goes on to it.
    #[test]
    fn a_suffix_names_the_server_it_goes_to() {
        assert_eq!(route(b"logging-service/system/heartbeat"), Some(("logging-service", 15)));
        assert_eq!(route(b"clipboard-server"), Some(("clipboard-server", 16)));
        assert_eq!(route(b"view-broker/s/7"), Some(("view-broker", 11)));
        for bad in [&b""[..], b"/x", b"Upper/x", b"../x", b"a b", b"\xff"] {
            assert_eq!(route(bad), None, "{bad:?}");
        }
        assert_eq!(base("auth-service"), "/auth-service");
        assert!(valid_name(&"a".repeat(32)) && !valid_name(&"a".repeat(33)));
    }
}
