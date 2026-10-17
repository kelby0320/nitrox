//! profile-server's host-testable internals.
//!
//! This library holds the manifest parser and the suffix routing; `src/main.rs` is the
//! bare-target server that uses them. `std` under `cargo test`; the binary provides the
//! allocator. See `docs/architecture/profiles-and-namespace-projection.md`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod manifest;

/// Which package directory a resolve suffix names, and what inside it.
pub mod projection {
    /// The package subdirectories this server projects, each under a namespace path of the
    /// same name.
    ///
    /// A fixed list, so a store package's contents never decide what appears in a namespace:
    /// a package is data, not policy.
    pub const PROJECTED: [&str; 2] = ["bin", "applications"];

    /// Split a resolve suffix into the projection it names and the entry within it, or `None` if it
    /// names no projection this server offers.
    ///
    /// Every bind of this endpoint is scoped: the kernel joins the subtree base without its
    /// leading slash, so `/bin` forwards `bin/list` and `/applications` forwards
    /// `applications/nxterm.toml`. The first component always names the projection, with no
    /// default; a bare suffix would let `/bin/applications` land on the applications root.
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
            assert_eq!(split_suffix("bin"), Some(("bin", "")));
            assert_eq!(split_suffix("bin/list"), Some(("bin", "list")));
            assert_eq!(split_suffix("applications"), Some(("applications", "")));
            assert_eq!(split_suffix("applications/nxterm.toml"), Some(("applications", "nxterm.toml")));

            // A program *called* `applications`.
            assert_eq!(split_suffix("bin/applications"), Some(("bin", "applications")));

            for s in ["", "list", "nxterm.toml", "/bin", "/applications", "store", "applicationsX"] {
                assert_eq!(split_suffix(s), None, "{s:?} named a projection");
            }
        }

        #[test]
        fn a_suffix_is_never_split_on_a_slash_it_merely_contains() {
            assert_eq!(split_suffix("etc/passwd"), None);
            assert_eq!(split_suffix("applications/../bin/list"), Some(("applications", "../bin/list")));
        }
    }
}
