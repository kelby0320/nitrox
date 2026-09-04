//! profile-server's host-testable internals.
//!
//! `profile-server` is a library + binary crate (mirroring init/service-mgr): this
//! library holds the profile-manifest parser (host-tested), while `src/main.rs` is the
//! bare-target resource server that uses it. `#![no_std]` for the bare build; `std`
//! under `cargo test` so the host harness works (`cargo xtask test` runs
//! `cargo test -p profile-server --lib`).
//!
//! The bare-target binary provides the `#[global_allocator]` (`libheap`); this library
//! only needs `alloc`. See `docs/architecture/profiles-and-namespace-projection.md`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod manifest;

/// Which package directory a resolve suffix names, and what inside it.
///
/// Its own module because it is the whole of the routing rule and it is pure — the bug it now
/// guards against (`/bin/applications` reaching the applications projection) was invisible in a
/// binary that cannot be host-tested.
pub mod projection {
    /// The package subdirectories this server projects beyond `bin`, each under a namespace path of
    /// the same name.
    ///
    /// **A fixed list rather than "whatever a package contains"**, because the alternative makes the
    /// *contents* of a store package decide what appears in a session's namespace — and a package is
    /// data, not policy. A projection is a name this server offers; a package either fills it or does
    /// not.
    pub const PROJECTED: [&str; 2] = ["bin", "applications"];

    /// Split a resolve suffix into the projection it names and the entry within it, or `None` if it
    /// names no projection this server offers.
    ///
    /// **Every bind of this endpoint is scoped, and that is the whole mechanism.** A subtree base is
    /// an absolute path and the kernel joins it *without* its leading slash (`join_subtree` takes
    /// `base[1..]`), so `/bin` bound with base `/bin` forwards `bin` and `bin/list`, and
    /// `/applications` bound with base `/applications` forwards `applications` and
    /// `applications/nxterm.toml`. The first component names the projection, always, with no default.
    ///
    /// **`/bin` used to be unscoped, and that was an alias rather than a shortcut** (PR #279 review,
    /// blocking 1). A bare suffix meant `bin`, so `/bin/applications` forwarded as `applications` and
    /// landed on the *applications* projection's root — putting the whole of it inside every namespace
    /// that binds `/bin`, including the application namespaces documented as not having it. Scoping
    /// both removes the bare form that made two different things look alike.
    pub fn split_suffix(suffix: &str) -> Option<(&'static str, &str)> {
        for d in PROJECTED {
            if suffix == d {
                return Some((d, ""));
            }
            if let Some(rest) = suffix.strip_prefix(d).and_then(|r| r.strip_prefix('/')) {
                return Some((d, rest));
            }
        }
        None
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn every_projection_is_named_and_there_is_no_default() {
            // **The alias this replaced.** `/bin` used to be bound unscoped, so `/bin/applications`
            // forwarded as a bare `applications` and landed here — on the applications
            // projection's *root*, making the whole of it reachable from every namespace holding
            // `/bin`. With both binds scoped there is no bare form to be ambiguous (PR #279
            // review, blocking 1).
            assert_eq!(split_suffix("bin"), Some(("bin", "")));
            assert_eq!(split_suffix("bin/list"), Some(("bin", "list")));
            assert_eq!(split_suffix("applications"), Some(("applications", "")));
            assert_eq!(split_suffix("applications/nxterm.toml"), Some(("applications", "nxterm.toml")));

            // A program *called* `applications`, reached the only way it now can be.
            assert_eq!(split_suffix("bin/applications"), Some(("bin", "applications")));

            // Nothing else routes anywhere. An unscoped bind would deliver these, and used to.
            for s in ["", "list", "nxterm.toml", "/bin", "/applications", "store", "applicationsX"] {
                assert_eq!(split_suffix(s), None, "{s:?} named a projection");
            }
        }

        #[test]
        fn a_suffix_is_never_split_on_a_slash_it_merely_contains() {
            // The first version split on the first slash, which routed anything shaped like a
            // path. `bin` and `applications` are the only two names, matched whole.
            assert_eq!(split_suffix("etc/passwd"), None);
            assert_eq!(split_suffix("applications/../bin/list"), Some(("applications", "../bin/list")));
        }
    }
}
