//! A focused parser for the system profile manifest
//! (`docs/architecture/profiles-and-namespace-projection.md`).
//!
//! The manifest is TOML: a `[profile]` table (name/generation, used for display) plus
//! a `[[package]]` table array — one per package, each with `name`, `version`, and the
//! store `path`. The profile server projects each package's `bin/` into `/bin` by
//! probing the packages in **manifest order** (first match wins). This parser extracts
//! that ordered package list; `[profile]` keys and unknown keys/sections are ignored
//! (forward-compat).
//!
//! Slice 1 needs only the store `path` (to probe) + name/version (for logging), so this
//! stays deliberately small — it is *not* a general TOML parser.

use alloc::string::String;
use alloc::vec::Vec;

/// One package a profile projects: its store path (the identity) + display name/version.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// The store path, e.g. `/store/<hash>-<name>-<version>`.
    pub path: String,
}

/// Strip a trailing `#` comment and surrounding whitespace.
fn strip(line: &str) -> &str {
    match line.find('#') {
        Some(i) => &line[..i],
        None => line,
    }
    .trim()
}

/// Parse `key = "value"` into `(key, value)` for a basic-string value, or `None`.
fn key_string(line: &str) -> Option<(&str, &str)> {
    let (k, v) = line.split_once('=')?;
    let v = v.trim().strip_prefix('"')?.strip_suffix('"')?;
    Some((k.trim(), v))
}

/// Parse the manifest's ordered package list. Malformed lines are skipped; a package
/// with an empty `path` is dropped (it can't be projected). Returns the packages in
/// manifest order (projection priority).
pub fn parse(text: &str) -> Vec<Package> {
    let mut packages: Vec<Package> = Vec::new();
    let mut cur: Option<Package> = None;

    for raw in text.lines() {
        let line = strip(raw);
        if line.is_empty() {
            continue;
        }
        if line == "[[package]]" {
            if let Some(p) = cur.take() {
                packages.push(p);
            }
            cur = Some(Package::default());
        } else if line.starts_with('[') {
            // Any other section header (e.g. `[profile]`) ends a pending package.
            if let Some(p) = cur.take() {
                packages.push(p);
            }
        } else if let Some((key, value)) = key_string(line) {
            if let Some(p) = cur.as_mut() {
                match key {
                    "name" => p.name = String::from(value),
                    "version" => p.version = String::from(value),
                    "path" => p.path = String::from(value),
                    _ => {}
                }
            }
        }
    }
    if let Some(p) = cur.take() {
        packages.push(p);
    }
    // Drop packages with no store path — nothing to project.
    packages.retain(|p| !p.path.is_empty());
    packages
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;

    const MANIFEST: &str = "\
# System profile manifest (generation 1).\n\
[profile]\n\
name = \"system\"\n\
generation = 1\n\
\n\
[[package]]\n\
name = \"heartbeat\"\n\
version = \"0.1.0\"\n\
path = \"/store/9f3a-heartbeat-0.1.0\"\n";

    #[test]
    fn parses_one_package() {
        let pkgs = parse(MANIFEST);
        assert_eq!(pkgs.len(), 1);
        assert_eq!(pkgs[0].name, "heartbeat");
        assert_eq!(pkgs[0].version, "0.1.0");
        assert_eq!(pkgs[0].path, "/store/9f3a-heartbeat-0.1.0");
    }

    #[test]
    fn preserves_manifest_order() {
        let t = "[[package]]\npath=\"/store/a\"\n[[package]]\npath=\"/store/b\"\n";
        let pkgs = parse(t);
        assert_eq!(pkgs.len(), 2);
        assert_eq!(pkgs[0].path, "/store/a");
        assert_eq!(pkgs[1].path, "/store/b");
    }

    #[test]
    fn drops_pathless_package_and_ignores_profile_keys() {
        let t = "[profile]\nname=\"system\"\n[[package]]\nname=\"x\"\n";
        assert!(parse(t).is_empty()); // no path → dropped
    }

    #[test]
    fn empty_manifest_is_empty() {
        assert!(parse("").is_empty());
        assert!(parse("[profile]\nname=\"system\"\n").is_empty());
    }
}
