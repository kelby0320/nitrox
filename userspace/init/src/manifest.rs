//! `init.toml` → a [`Manifest`]: an ordered list of [`MountSpec`]s and the [`BindSpec`]s
//! that re-bind them.
//!
//! Parses the bootstrap mount manifest (`docs/spec/init-toml-schema.md`) via
//! [`crate::toml_lite`], validates the required fields and their types, and
//! topologically sorts the mounts by mount-point depth (shallowest first, so a
//! parent path is bound before its children). Binds are validated against the mounts —
//! each names one as its source — and kept in file order. The mount *processing* loop (spawn
//! fs-server → Ready handshake → `sys_ns_bind`) is slice-4 Part 5 / slice 7; this
//! module is the pure, host-testable front half.

use alloc::string::String;
use alloc::vec::Vec;

use crate::toml_lite::{self, Table};

/// Mount access mode (`mode = "ro" | "rw"`), which determines the rights init
/// grants when binding the fs-server endpoint (see the schema's rights table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Ro,
    Rw,
}

impl Mode {
    /// The flags byte of the fs-server's **setup message** for this mode: `1` — read-only — for
    /// `"ro"` (administration Part C.3). Since then `"ro"` is the server's too: it refuses every
    /// mutation and marks every file read-only, where before only the binding's rights changed,
    /// and forwarded resolves ignore those. Stated here rather than taken from `librsproto`,
    /// which init does not link; a host test holds the two equal.
    pub fn setup_flags(self) -> u8 {
        match self {
            Mode::Ro => 1,
            Mode::Rw => 0,
        }
    }
}

/// One validated `[[mount]]` entry. `options` (the optional `[mount.options]`
/// subtable) is kept verbatim to forward to the fs-server at Ready (slice 7);
/// `required_for` is validated to be `"boot"` but not stored (the only value).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    pub fs_server: alloc::string::String,
    pub device: alloc::string::String,
    pub mount_point: alloc::string::String,
    pub mode: Mode,
    pub options: Option<Table>,
}

/// One validated `[[bind]]` entry: the forwarding endpoint of the mount at `source`, bound a
/// second time at `path` and scoped to `subtree` — `mount --bind` for a resource server.
///
/// It shares the source's server registration rather than spawning a rival server for the
/// same device, which is what a second `[[mount]]` of that device would do. `session-mgr`
/// binds each login's `/home` the same way, from the same endpoint (Phase 5 Part C.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BindSpec {
    /// Where the binding appears. Absolute.
    pub path: alloc::string::String,
    /// The `mount_point` of a `[[mount]]` in the same manifest.
    pub source: alloc::string::String,
    /// The path on the source's server that lookups through `path` are scoped to. Absolute;
    /// `/` means the whole tree.
    pub subtree: alloc::string::String,
}

/// A parsed `init.toml`: the mounts (shallowest first) and the binds over them (file order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub mounts: Vec<MountSpec>,
    pub binds: Vec<BindSpec>,
}

/// Why an `init.toml` manifest was rejected. Init logs this and drops to the
/// emergency shell (Part 5); the `line`/field detail aids the operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// The file was not valid TOML (carries the parser's error).
    Toml(toml_lite::ParseError),
    /// No `[[mount]]` entries at all.
    NoMounts,
    /// A required field was missing on the `index`-th mount.
    MissingField { index: usize, field: &'static str },
    /// A field was the wrong type / not a string where one was required.
    NotAString { index: usize, field: &'static str },
    /// `mode` was neither `"ro"` nor `"rw"`.
    BadMode { index: usize },
    /// `required_for` was not the only supported value, `"boot"`.
    UnsupportedRequiredFor { index: usize },
    /// `mount_point` was not an absolute path.
    NonAbsoluteMountPoint { index: usize },
    /// A required field was missing on the `index`-th bind.
    BindMissingField { index: usize, field: &'static str },
    /// A bind field was the wrong type.
    BindNotAString { index: usize, field: &'static str },
    /// A bind's `path` or `subtree` was not an absolute path.
    BindNotAbsolute { index: usize, field: &'static str },
    /// A bind's `source` is not the `mount_point` of any mount in the manifest.
    BindUnknownSource { index: usize },
}

/// Parse + validate `init.toml`: the mounts in shallowest-first order, and the binds in file
/// order, each checked to name a mount as its source.
pub fn parse(input: &str) -> Result<Manifest, ManifestError> {
    let doc = toml_lite::parse(input).map_err(ManifestError::Toml)?;
    let entries = doc.array("mount");
    if entries.is_empty() {
        return Err(ManifestError::NoMounts);
    }

    let mut mounts: Vec<MountSpec> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        let t = &entry.table;
        let fs_server = required_str(t, "fs_server", index)?;
        let device = required_str(t, "device", index)?;
        let mount_point = required_str(t, "mount_point", index)?;
        let mode_str = required_str(t, "mode", index)?;
        let required_for = required_str(t, "required_for", index)?;

        if !mount_point.starts_with('/') {
            return Err(ManifestError::NonAbsoluteMountPoint { index });
        }
        let mode = match mode_str {
            "ro" => Mode::Ro,
            "rw" => Mode::Rw,
            _ => return Err(ManifestError::BadMode { index }),
        };
        if required_for != "boot" {
            return Err(ManifestError::UnsupportedRequiredFor { index });
        }

        mounts.push(MountSpec {
            fs_server: fs_server.into(),
            device: device.into(),
            mount_point: mount_point.into(),
            mode,
            options: entry.subtable("options").cloned(),
        });
    }

    // Topologically sort by mount-point depth (shallowest first). Stable, so
    // equal-depth mounts keep file order.
    mounts.sort_by_key(|m| depth(&m.mount_point));

    let mut binds: Vec<BindSpec> = Vec::new();
    for (index, entry) in doc.array("bind").iter().enumerate() {
        let t = &entry.table;
        let field = |name: &'static str| match t.get(name) {
            None => Err(ManifestError::BindMissingField { index, field: name }),
            Some(v) => v.as_str().ok_or(ManifestError::BindNotAString { index, field: name }),
        };
        let path = field("path")?;
        let source = field("source")?;
        let subtree = field("subtree")?;
        for (name, value) in [("path", path), ("subtree", subtree)] {
            if !value.starts_with('/') {
                return Err(ManifestError::BindNotAbsolute { index, field: name });
            }
        }
        if !mounts.iter().any(|m| m.mount_point == source) {
            return Err(ManifestError::BindUnknownSource { index });
        }
        binds.push(BindSpec { path: path.into(), source: source.into(), subtree: subtree.into() });
    }
    Ok(Manifest { mounts, binds })
}

/// Fetch a required string field, distinguishing "absent" from "present but not
/// a string".
fn required_str<'a>(
    t: &'a Table,
    field: &'static str,
    index: usize,
) -> Result<&'a str, ManifestError> {
    match t.get(field) {
        None => Err(ManifestError::MissingField { index, field }),
        Some(v) => v.as_str().ok_or(ManifestError::NotAString { index, field }),
    }
}

/// Mount-point depth = number of non-empty `/`-separated components. `/` is 0,
/// `/store` is 1, `/store/data` is 2.
fn depth(path: &str) -> usize {
    path.split('/').filter(|c| !c.is_empty()).count()
}

/// Map a `device = "<scheme>:<value>"` spec to the namespace path that resolves to
/// its block device (`docs/spec/init-toml-schema.md`): `gpt-partlabel:<l>` →
/// `/dev/disk/by-partlabel/<l>`, `gpt-partuuid:<u>` → `/dev/disk/by-partuuid/<u>`.
/// `None` for an unknown scheme (init logs it and drops the mount). The kernel
/// (slice 6 GPT driver) binds these paths to the partition's device node at boot.
pub fn device_ns_path(device: &str) -> Option<String> {
    let (prefix, value) = if let Some(v) = device.strip_prefix("gpt-partlabel:") {
        ("/dev/disk/by-partlabel/", v)
    } else if let Some(v) = device.strip_prefix("gpt-partuuid:") {
        ("/dev/disk/by-partuuid/", v)
    } else {
        return None;
    };
    if value.is_empty() {
        return None;
    }
    let mut p = String::from(prefix);
    p.push_str(value);
    Some(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **init's copy of the read-only flag is the server's**: the byte `"ro"` sends parses, on the
    /// server's side, as read-only, and `"rw"`'s as writable.
    #[test]
    fn a_read_only_mount_tells_its_server_so() {
        use librsproto::meta::{FS_SETUP_READ_ONLY, parse_fs_setup};
        assert_eq!(Mode::Ro.setup_flags(), FS_SETUP_READ_ONLY);
        assert_ne!(parse_fs_setup(&[Mode::Ro.setup_flags()]) & FS_SETUP_READ_ONLY, 0);
        assert_eq!(parse_fs_setup(&[Mode::Rw.setup_flags()]) & FS_SETUP_READ_ONLY, 0);
    }

    const SINGLE_ROOT: &str = "\
[[mount]]
fs_server    = \"fs-server-ext4\"
device       = \"gpt-partuuid:01234567-89ab-cdef-0123-456789abcdef\"
mount_point  = \"/\"
mode         = \"rw\"
required_for = \"boot\"
";

    #[test]
    fn parses_single_root() {
        let m = parse(SINGLE_ROOT).unwrap().mounts;
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].fs_server, "fs-server-ext4");
        assert_eq!(m[0].mount_point, "/");
        assert_eq!(m[0].mode, Mode::Rw);
        assert!(m[0].options.is_none());
    }

    #[test]
    fn topo_sorts_shallowest_first() {
        // Deliberately out of order in the file; expect /, /store, /store/data.
        let src = "\
[[mount]]
fs_server=\"a\"
device=\"d\"
mount_point=\"/store/data\"
mode=\"ro\"
required_for=\"boot\"
[[mount]]
fs_server=\"b\"
device=\"d\"
mount_point=\"/store\"
mode=\"ro\"
required_for=\"boot\"
[[mount]]
fs_server=\"c\"
device=\"d\"
mount_point=\"/\"
mode=\"rw\"
required_for=\"boot\"
";
        let m = parse(src).unwrap().mounts;
        let points: Vec<&str> = m.iter().map(|s| s.mount_point.as_str()).collect();
        assert_eq!(points, ["/", "/store", "/store/data"]);
    }

    #[test]
    fn equal_depth_keeps_file_order() {
        let src = "\
[[mount]]
fs_server=\"a\"
device=\"d\"
mount_point=\"/home\"
mode=\"rw\"
required_for=\"boot\"
[[mount]]
fs_server=\"b\"
device=\"d\"
mount_point=\"/store\"
mode=\"ro\"
required_for=\"boot\"
";
        let m = parse(src).unwrap().mounts;
        let points: Vec<&str> = m.iter().map(|s| s.mount_point.as_str()).collect();
        assert_eq!(points, ["/home", "/store"]); // both depth 1, file order kept
    }

    #[test]
    fn options_subtable_is_captured() {
        let src = "\
[[mount]]
fs_server=\"fs-server-ext4\"
device=\"d\"
mount_point=\"/\"
mode=\"rw\"
required_for=\"boot\"
[mount.options]
data_journal = true
";
        let m = parse(src).unwrap().mounts;
        let opts = m[0].options.as_ref().unwrap();
        assert_eq!(opts.get_str("fs_server"), None);
        assert_eq!(
            opts.get("data_journal"),
            Some(&toml_lite::Value::Boolean(true))
        );
    }

    #[test]
    fn missing_required_field_is_rejected() {
        // No `mode`.
        let src = "\
[[mount]]
fs_server=\"a\"
device=\"d\"
mount_point=\"/\"
required_for=\"boot\"
";
        assert_eq!(
            parse(src),
            Err(ManifestError::MissingField { index: 0, field: "mode" })
        );
    }

    #[test]
    fn bad_mode_and_required_for_rejected() {
        let bad_mode = SINGLE_ROOT.replace("\"rw\"", "\"append\"");
        assert_eq!(parse(&bad_mode), Err(ManifestError::BadMode { index: 0 }));

        let bad_req = SINGLE_ROOT.replace("\"boot\"", "\"lazy\"");
        assert_eq!(
            parse(&bad_req),
            Err(ManifestError::UnsupportedRequiredFor { index: 0 })
        );
    }

    #[test]
    fn non_absolute_mount_point_rejected() {
        let bad = SINGLE_ROOT.replace("\"/\"", "\"store\"");
        assert_eq!(
            parse(&bad),
            Err(ManifestError::NonAbsoluteMountPoint { index: 0 })
        );
    }

    /// The test image's shape: the root mount, and two binds over it (Part C.1).
    const ROOT_AND_BINDS: &str = "\
[[mount]]
fs_server    = \"fs-server-ext4\"
device       = \"gpt-partlabel:nitrox-root\"
mount_point  = \"/\"
mode         = \"rw\"
required_for = \"boot\"

[[bind]]
path    = \"/subtreetest\"
source  = \"/\"
subtree = \"/system\"

[[bind]]
path    = \"/scratch\"
source  = \"/\"
subtree = \"/scratch\"
";

    #[test]
    fn binds_parse_in_file_order_after_the_mounts() {
        let m = parse(ROOT_AND_BINDS).unwrap();
        assert_eq!(m.mounts.len(), 1);
        assert_eq!(
            m.binds,
            [
                BindSpec { path: "/subtreetest".into(), source: "/".into(), subtree: "/system".into() },
                BindSpec { path: "/scratch".into(), source: "/".into(), subtree: "/scratch".into() },
            ]
        );
        assert!(parse(SINGLE_ROOT).unwrap().binds.is_empty(), "binds are optional");
    }

    #[test]
    fn a_bind_must_name_a_mount_and_use_absolute_paths() {
        let unknown = ROOT_AND_BINDS.replacen("source  = \"/\"", "source  = \"/store\"", 1);
        assert_eq!(parse(&unknown), Err(ManifestError::BindUnknownSource { index: 0 }));

        let relative = ROOT_AND_BINDS.replace("subtree = \"/system\"", "subtree = \"system\"");
        assert_eq!(parse(&relative), Err(ManifestError::BindNotAbsolute { index: 0, field: "subtree" }));

        let relative_path = ROOT_AND_BINDS.replace("path    = \"/scratch\"", "path    = \"scratch\"");
        assert_eq!(parse(&relative_path), Err(ManifestError::BindNotAbsolute { index: 1, field: "path" }));

        let missing = ROOT_AND_BINDS.replace("subtree = \"/scratch\"\n", "");
        assert_eq!(parse(&missing), Err(ManifestError::BindMissingField { index: 1, field: "subtree" }));
    }

    #[test]
    fn empty_manifest_rejected() {
        assert_eq!(parse("# nothing here\n"), Err(ManifestError::NoMounts));
    }

    #[test]
    fn device_ns_path_maps_schemes() {
        assert_eq!(
            device_ns_path("gpt-partlabel:nitrox-root").as_deref(),
            Some("/dev/disk/by-partlabel/nitrox-root")
        );
        assert_eq!(
            device_ns_path("gpt-partuuid:01234567-89ab").as_deref(),
            Some("/dev/disk/by-partuuid/01234567-89ab")
        );
        // Unknown scheme / empty value → None.
        assert_eq!(device_ns_path("sd:0"), None);
        assert_eq!(device_ns_path("gpt-partlabel:"), None);
        assert_eq!(device_ns_path("nitrox-root"), None);
    }

    #[test]
    fn malformed_toml_propagates() {
        match parse("[[mount]]\ngarbage\n") {
            Err(ManifestError::Toml(_)) => {}
            other => panic!("expected Toml error, got {other:?}"),
        }
    }
}
