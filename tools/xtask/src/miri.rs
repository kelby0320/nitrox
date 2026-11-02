//! `miri`: the kernel and the system services under Miri, which CI runs on a schedule and by hand.
//!
//! Miri finds undefined behaviour in the `unsafe` a test reaches, by interpreting it: a test can
//! take ten thousand times as long, and no assembly runs. So this is a list, not the host suite: a
//! target with no `unsafe` of its own for a test to reach is left out, with its reason.

use crate::*;

/// What every target runs with: tests read host files, and the kernel's leak what lives for ever.
const FLAGS: &str = "-Zmiri-disable-isolation -Zmiri-ignore-leaks";
/// What `libheap` adds: its allocator hands out addresses, and its test pins their provenance.
const STRICT: &str = "-Zmiri-strict-provenance";

/// The userspace targets, each with the features its tests need.
const TARGETS: &[(&str, Option<&str>)] = &[
    ("libkern", None),
    ("libheap", None),
    ("libos", None),
    ("librsproto", Some("io")),
    ("libstream", None),
    ("libfs", None),
    ("libfsserver", None),
    ("libcrypto", None),
    ("libusers", None),
    ("libsession", None),
    ("libprompt", None),
    ("libviews", None),
    ("libinittoml", None),
    ("libinput", None),
    ("libline", None),
    ("libtime", None),
    ("libdraw", Some("io,soft-floats")),
    ("libsurface", None),
    ("init", None),
    ("service-mgr", None),
    ("device-mgr", None),
    ("storage-service", None),
    ("view-broker", None),
    ("logging-service", None),
    ("profile-server", None),
    ("tty-server", None),
    ("input-server", None),
    ("clipboard-server", None),
    ("compositor", None),
    ("desktop-shell", Some("libdraw/soft-floats")),
    ("desktop-session-mgr", Some("libdraw/soft-floats")),
    ("coreutils", None),
    ("nxsh", None),
];

/// The host-test targets this does not run, each with why.
#[cfg(test)]
const LEFT_OUT: &[(&str, &str)] = &[
    ("auth-service", "its library has no `unsafe`, and each test derives a key, for minutes"),
    ("fs-server-ext4", "its library has none outside tests, whose images come from `mke2fs`"),
    ("fs-server-fat", "its library has none outside tests, whose images come from `mkfs.fat`"),
    ("libgpt", "it has no `unsafe`, and its tests check themselves with `sfdisk`"),
    ("nxinstall", "its library has no `unsafe`, and its tests take fourteen minutes"),
    ("libui", "its `unsafe` is a scrub, which `desktop-session-mgr` reaches, for half an hour"),
    ("libterm", "it has no `unsafe`"),
    ("nxterm", "an application, not a service, and sixteen minutes of rasterising"),
    ("nxedit", "as `nxterm`"),
    ("nxfiles", "as `nxterm`"),
];

/// `cargo xtask miri [kernel|userspace|<crate>]`: everything when nothing is named. Every target
/// runs and reports, so one that fails hides none behind it.
pub(crate) fn cmd_miri(args: &[String]) -> R<()> {
    let which = args.first().map(String::as_str);
    let crate_named = which.filter(|w| TARGETS.iter().any(|t| t.0 == *w));
    if which.is_some_and(|w| w != "kernel" && w != "userspace") && crate_named.is_none() {
        return Err("usage: cargo xtask miri [kernel|userspace|<crate>]".into());
    }
    let (root, host) = (repo_root(), host_triple()?);
    let mut failed = Vec::new();
    if which.is_none() || which == Some("kernel") {
        let channel = read_text(&root.join("userspace/rust-toolchain.toml"))?;
        let channel = channel.lines().find_map(|l| l.strip_prefix("channel = ")).ok_or("no channel")?;
        // The kernel's toolchain is stable, which has no Miri: the one nightly it meets.
        let mut cmd = userspace_cargo();
        cmd.arg(format!("+{}", channel.trim_matches('"'))).args(["miri", "test", "--lib", "--target", &host]);
        if run(cmd.current_dir(root.join("kernel")).env("MIRIFLAGS", FLAGS)).is_err() {
            failed.push("kernel");
        }
    }
    if which != Some("kernel") {
        for &(name, feature) in TARGETS.iter().filter(|t| crate_named.is_none_or(|c| c == t.0)) {
            let mut cmd = userspace_cargo();
            cmd.args(["miri", "test", "-p", name, "--lib", "--target", &host]);
            if let Some(f) = feature {
                cmd.args(["--features", f]);
            }
            let flags = if name == "libheap" { format!("{FLAGS} {STRICT}") } else { FLAGS.to_string() };
            if run(cmd.current_dir(root.join("userspace")).env("MIRIFLAGS", flags)).is_err() {
                failed.push(name);
            }
        }
    }
    if failed.is_empty() { Ok(()) } else { Err(format!("Miri failed: {}", failed.join(", ")).into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A crate added to the host suite is added here, or says why not.**
    #[test]
    fn every_host_test_target_is_run_or_left_out_with_a_reason() {
        let suite = std::fs::read_to_string(repo_root().join("tools/xtask/src/host_test.rs")).unwrap();
        let names: Vec<&str> = suite
            .lines()
            .collect::<Vec<_>>()
            .windows(2)
            .filter(|w| w[0].trim() == ".arg(\"-p\")")
            .map(|w| w[1].trim().trim_start_matches(".arg(\"").trim_end_matches("\")"))
            .collect();
        assert!(names.contains(&"libkern"), "{names:?}");
        let run = TARGETS.iter().map(|t| t.0);
        let known: Vec<&str> = run.clone().chain(LEFT_OUT.iter().map(|l| l.0)).collect();
        for name in &names {
            assert!(known.contains(name), "{name} is in the host suite and neither run nor left out");
        }
        for name in &known {
            assert!(names.contains(name), "{name} is not in the host suite");
        }
        assert!(run.into_iter().all(|t| !LEFT_OUT.iter().any(|l| l.0 == t)), "run and left out both");
    }
}
