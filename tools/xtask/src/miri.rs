//! `miri`: the kernel and the system services under Miri, which CI runs on a schedule and by hand.
//!
//! Miri finds undefined behaviour in the `unsafe` a test reaches, by interpreting it: a test can
//! take ten thousand times as long, and no assembly runs. So this is a list, not the host suite: a
//! target with no `unsafe` of its own for a test to reach is left out, with its reason.

use crate::*;

/// What every target runs with: tests read host files.
const FLAGS: &str = "-Zmiri-disable-isolation";
/// What the kernel adds, whose tests leak what lives for ever, and `libos`, whose forget a read
/// that never ends.
const LEAKS: &str = "-Zmiri-ignore-leaks";
/// What `libheap` adds: its allocator hands out addresses, and its test pins their provenance.
const STRICT: &str = "-Zmiri-strict-provenance";

/// A target's name and the features its tests need.
type Target = (&'static str, Option<&'static str>);

/// The features of a target that draws text, whose `sqrtf` is otherwise assembly.
const SOFT: Option<&str> = Some("libdraw/soft-floats");

const TARGETS: &[Target] = &[
    ("libkern", None),
    ("libheap", None),
    ("libos", None),
    ("liblog", None),
    ("librsproto", Some("io")),
    ("libstream", Some("io")),
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
    ("desktop-shell", SOFT),
    ("desktop-session-mgr", SOFT),
    ("coreutils", None),
    ("nxsh", None),
];

/// The host-test targets the job does not run, each with why. A person may name one.
const LEFT_OUT: &[(Target, &str)] = &[
    (("auth-service", None), "its library has no `unsafe`, and each test derives a key, for minutes"),
    (("fs-server-ext4", None), "its library has none outside tests, whose images come from `mke2fs`"),
    (("fs-server-fat", None), "its library has none outside tests, whose images come from `mkfs.fat`"),
    (("libgpt", None), "it has no `unsafe`, and its tests check themselves with `sfdisk`"),
    (("nxinstall", None), "its library has no `unsafe`, and its tests take fourteen minutes"),
    (("libui", SOFT), "its `unsafe` is a scrub, which `desktop-session-mgr` reaches, for half an hour"),
    (("libterm", SOFT), "it has no `unsafe`"),
    (("nxterm", SOFT), "an application, not a service, and sixteen minutes of rasterising"),
    (("nxedit", SOFT), "as `nxterm`"),
    (("nxfiles", SOFT), "as `nxterm`"),
];

/// What `args` names: whether the kernel runs, and which userspace targets.
fn selection(args: &[String]) -> R<(bool, Vec<Target>)> {
    match args {
        [] => return Ok((true, TARGETS.to_vec())),
        [word] if word == "kernel" => return Ok((true, Vec::new())),
        [word] if word == "userspace" => return Ok((false, TARGETS.to_vec())),
        [word] => {
            if let Some(&target) = TARGETS.iter().find(|t| t.0 == word) {
                return Ok((false, vec![target]));
            }
            if let Some(&(target, why)) = LEFT_OUT.iter().find(|l| l.0.0 == word) {
                eprintln!("xtask: the job leaves {word} out: {why}");
                return Ok((false, vec![target]));
            }
        }
        _ => {}
    }
    Err("usage: cargo xtask miri [kernel|userspace|<crate>]".into())
}

fn flags(name: &str) -> String {
    match name {
        "kernel" | "libos" => format!("{FLAGS} {LEAKS}"),
        "libheap" => format!("{FLAGS} {STRICT}"),
        _ => FLAGS.to_string(),
    }
}

/// Miri is a component neither toolchain file asks for: one refusal naming the fix, not a failure
/// a target.
fn require_miri(channel: &str) -> R<()> {
    let mut check = userspace_cargo();
    check.arg(format!("+{channel}")).args(["miri", "--version"]);
    match check.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status() {
        Ok(s) if s.success() => Ok(()),
        _ => Err(format!("no Miri: rustup component add miri --toolchain {channel}").into()),
    }
}

/// `cargo xtask miri [kernel|userspace|<crate>]`: the job's targets when nothing is named. Every
/// target runs and reports, so one that fails hides none behind it.
pub(crate) fn cmd_miri(args: &[String]) -> R<()> {
    let (kernel, targets) = selection(args)?;
    let (root, host) = (repo_root(), host_triple()?);
    let channel = read_text(&root.join("userspace/rust-toolchain.toml"))?;
    let channel = channel.lines().find_map(|l| l.strip_prefix("channel = ")).ok_or("no channel")?;
    let channel = channel.trim_matches('"');
    require_miri(channel)?;
    let mut failed = Vec::new();
    let mut attempt = |name, cmd: &mut Command| {
        if let Err(e) = run(cmd.env("MIRIFLAGS", flags(name))) {
            eprintln!("xtask: {e}");
            failed.push(name);
        }
    };
    if kernel {
        // The kernel's toolchain is stable, which has no Miri: the one nightly it meets.
        let mut cmd = userspace_cargo();
        cmd.arg(format!("+{channel}")).args(["miri", "test", "--lib", "--target", &host]);
        attempt("kernel", cmd.current_dir(root.join("kernel")));
    }
    for (name, feature) in targets {
        let mut cmd = userspace_cargo();
        cmd.args(["miri", "test", "-p", name, "--lib", "--target", &host]);
        if let Some(f) = feature {
            cmd.args(["--features", f]);
        }
        attempt(name, cmd.current_dir(root.join("userspace")));
    }
    if failed.is_empty() { Ok(()) } else { Err(format!("Miri failed: {}", failed.join(", ")).into()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(words: &[&str]) -> Option<(bool, Vec<&'static str>)> {
        let args: Vec<String> = words.iter().map(|w| w.to_string()).collect();
        selection(&args).ok().map(|(kernel, targets)| (kernel, targets.iter().map(|t| t.0).collect()))
    }

    #[test]
    fn a_word_names_what_runs_and_only_one_word_does() {
        assert_eq!(named(&[]).map(|n| (n.0, n.1.len())), Some((true, TARGETS.len())));
        assert_eq!(named(&["kernel"]), Some((true, Vec::new())));
        assert_eq!(named(&["userspace"]).map(|n| (n.0, n.1.len())), Some((false, TARGETS.len())));
        assert_eq!(named(&["libtime"]), Some((false, vec!["libtime"])));
        assert_eq!(named(&["libui"]), Some((false, vec!["libui"])), "left out of the job, not refused");
        assert_eq!(named(&["nonsense"]), None);
        assert_eq!(named(&["kernel", "userspace"]), None, "an ignored word would hide a target");
        assert_eq!(named(&["libtime", "libheap"]), None);
    }

    #[test]
    fn leaks_are_ignored_where_a_test_leaks_and_nowhere_else() {
        for name in ["kernel", "libos"] {
            assert!(flags(name).contains(LEAKS), "{name}");
        }
        assert!(TARGETS.iter().filter(|t| t.0 != "libos").all(|t| !flags(t.0).contains(LEAKS)));
    }

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
        let known: Vec<&str> = run.clone().chain(LEFT_OUT.iter().map(|l| l.0.0)).collect();
        for name in &names {
            assert!(known.contains(name), "{name} is in the host suite and neither run nor left out");
        }
        for name in &known {
            assert!(names.contains(name), "{name} is not in the host suite");
        }
        assert!(run.into_iter().all(|t| !LEFT_OUT.iter().any(|l| l.0.0 == t)), "run and left out both");
    }
}
