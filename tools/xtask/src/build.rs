//! `build`, `image`, `fetch-limine` and `clean`, and the userspace builds under them.

use crate::*;

/// The coreutils: one crate, a bin per program. Built, packed into the initramfs, and
/// packaged into the store from this single list — see [`profile_programs`].
pub(crate) const COREUTILS: &[&str] = &[
    "list", "copy", "mkdir", "remove", "rename", "move", "touch", "date", "sleep", "whoami",
    // The graphical session's desktops, and `/dev/desktop`'s first consumer — see M8 Part F.
    "desktop",
    // The clipboard, either side of a pipe — M12 decision 4, which is what makes the kill ring
    // reachable by something other than a window.
    "clip",
    // Running a program in a view (administration Part A.5): the view broker's client, typed at a
    // shell like the rest of these.
    "with",
    // The machine's disks (administration Part C.7): `--list` from any session, and `--mount` and
    // `--unmount` from a view with the `storage` grant.
    "disk",
    // The accounts (administration Part D.4): `--list` and your own `--password` from any session,
    // the rest from a view with the `accounts` grant, or on a users file for recovery.
    "account",
    // The services (administration Part E.2): `--list` from any session, and `--start`, `--stop`
    // and `--restart` from a view with the `services` grant.
    "service",
    // Stopping or restarting the machine (administration Part E.4d), through the `power` grant the
    // seeded policy gives everyone for it.
    "shutdown", // Reading the service log back (administration Part E.6), through the `logs` grant.
    "log",
];

/// The system services, packaged into the store like any other program.
///
/// **`fs-server-ext4` is here *and* in the initramfs**, and the two copies serve different
/// purposes. The store copy makes it versioned and content-addressed like everything else,
/// and is what a **non-root** fs-server (a second mount) would be spawned from, since root
/// is already up by then. The initramfs copy is the only one that can ever mount the root
/// — and the only one that could ever *re*-mount it, because `/store` is unreadable without
/// the very server being restarted. See `TODO(fs-server-restart)`.
pub(crate) const SYSTEM_SERVICES: &[&str] = &[
    "service-mgr",
    "session-mgr",
    "desktop-session-mgr",
    "desktop-shell",
    "auth-service",
    "logging-service",
    "fs-server-ext4",
    // FAT's server (Phase 6 Part E.4). Store-only: no root is FAT, so the storage service is what
    // spawns it, for a stick, from `/bin` like any second mount.
    "fs-server-fat",
    "tty-server",
    // The display arm's two servers. They were initramfs-resident until 2026-08-11 for one
    // reason — they predate `/bin` — and neither has a bootstrap role: a compositor cannot
    // run before there is a root to read a font from, let alone before there is one to spawn
    // it from. Moving them here cost a reordering in `init`, which now brings the display up
    // after the profile server rather than before it.
    "compositor",
    "input-server",
    // The kill ring (M12 Part E). A store package like the rest: nothing about a clipboard is
    // needed to reach a mounted root, and its only client runs long after one.
    "clipboard-server",
    // The view broker (administration Part A). `service-mgr` spawns it from here, as it does
    // `auth-service` — `init` did, until administration Part E.1a.
    "view-broker",
    // The device manager (administration Part B). `service-mgr` spawns it before `input-server`,
    // which takes its devices from it, and binds it at `/svc/devices`.
    "device-mgr",
    // The storage service (administration Part C.5). `service-mgr` spawns it straight after the
    // device manager, so it owns `block` from boot on, and binds it at `/svc/storage`.
    "storage-service",
];

/// The test programs, packaged into a store package of their own in selftest/test-harness
/// builds and absent from a release image entirely.
///
/// **A separate package from `system`**, so the system package keeps meaning "the services
/// this OS is made of" rather than "those, plus whatever the test build needed". It also
/// keeps the system package's content hash identical between a release and a test image,
/// which is the property a content-addressed path is supposed to have.
const TEST_PROGRAMS: &[&str] = &[
    "test-harness",
    "test-stage",
    "display-selftest",
    "ui-testclient",
    "input-testclient",
    "boot-probe",
    // The M13 Part A measurement. In the same package as the rest: it is a test program, and a
    // package per binary would claim an independence it does not have.
    "compose-bench",
    // `check-storage`'s writer and reader (administration Part C.8): no release program writes
    // through a mapping and lets go without a sync.
    "test-pattern",
    // A server that exits when asked (administration Part E.1), for `boot-probe`'s proof that a
    // restarted server is reached again at its path.
    "restart-probe",
];

/// The `test` package's programs built from a crate of their own rather than `test-harness`'s:
/// **`heartbeat`**, Phase 3's demo service, which left the release image with administration
/// Part E.1c. It was the only declaration a release image carried; a test image keeps it, as the
/// one `always` service that runs for the whole boot.
const TEST_CRATE_PROGRAMS: &[&str] = &["heartbeat"];

/// Everything in the `test` package: [`TEST_PROGRAMS`], then [`TEST_CRATE_PROGRAMS`].
pub(crate) fn test_package() -> Vec<&'static str> {
    TEST_PROGRAMS.iter().chain(TEST_CRATE_PROGRAMS).copied().collect()
}

pub(crate) fn cmd_build(mode: BuildMode) -> R<()> {
    // Build the userspace programs BEFORE the kernel: the kernel embeds their
    // ELFs via `include_bytes!`, so the artifacts must exist at kernel compile
    // time. Only the kernel carries the selftest / test-harness feature — `init` did until Phase 5
    // Part C.1, and `nxterm`'s `test-harness` is its own, passed below.
    cmd_build_hello()?;
    // The integration smoke-test harness (bins `test-harness`, `test-stage` and
    // `display-selftest`) is built
    // + embedded ONLY in selftest/test-harness builds — absent from release images.
    if mode.stages_test_data() {
        build_userspace_crate("test-harness", TEST_PROGRAMS, None)?;
        for prog in TEST_CRATE_PROGRAMS {
            build_userspace_bin(prog, None)?;
        }
    }
    // **`None` in every mode** (Phase 5 Part C.1), like `session-mgr` below. The last
    // test-only branch in `init` — the `/subtreetest` and `/scratch` binds — is a `[[bind]]` in
    // the test image's `init.toml` now, and a feature flag nothing reads would still make the
    // two builds' bytes differ.
    build_userspace_bin("init", None)?;
    build_userspace_bin("fs-server-ext4", None)?;
    build_userspace_bin("fs-server-fat", None)?;
    build_userspace_bin("eshell", None)?;
    build_userspace_bin("service-mgr", None)?;
    // The coreutils (`list`, …) — real programs, present in release images. One crate,
    // a bin per program, so the crate directory is named separately from the bins.
    build_userspace_crate("coreutils", COREUTILS, None)?;
    // `nxsh` — the shell. Milestone 3 Part A ships the *language* (lexer + parser, tested
    // on the host); this builds its bin for the bare target so the language cannot
    // quietly stop being part of the OS while it is being written.
    build_userspace_bin("nxsh", None)?;
    build_userspace_bin("profile-server", None)?;
    build_userspace_bin("tty-server", None)?;
    // `input-server` — holds the raw device nodes and serves the merged stream. A
    // lib + bin split like `tty-server`: the merge is host-tested, this builds the bin.
    build_userspace_bin("input-server", None)?;
    build_userspace_bin("logging-service", None)?;
    build_userspace_bin("auth-service", None)?;
    // The clipboard (M12 Part E). A lib + bin split like `auth-service`: the ring is
    // host-tested, this builds the bare-target server.
    build_userspace_bin("clipboard-server", None)?;
    // The view broker (administration Part A). A lib + bin split like `auth-service`: who may
    // do what is host-tested, this builds the bare-target server.
    build_userspace_bin("view-broker", None)?;
    // The device manager (administration Part B). A lib + bin split: names, classes, owners and
    // the tables are host-tested, this builds the bare-target server.
    build_userspace_bin("device-mgr", None)?;
    // The storage service (administration Part C.5). A lib + bin split: what a device holds,
    // which of them `init` mounted, the live-boot rule and the tables are host-tested, this builds
    // the bare-target server.
    build_userspace_bin("storage-service", None)?;
    // **`None`, and that is the point.** `session-mgr` took `mode.features()` because it
    // fired the self-test verdict; the retrofit moved the verdict to `boot-probe` and left
    // the crate with no reader for either feature. Passing one anyway would make the next
    // test-only branch compile on the first try, which is how the thing Part B removed comes
    // back — the zero should be a wall, not a count.
    build_userspace_bin("session-mgr", None)?;
    build_userspace_bin("desktop-session-mgr", None)?;
    build_userspace_bin("desktop-shell", None)?;
    build_userspace_bin("compositor", None)?;
    // The GUI terminal (M5 Part B). A lib/bin split like `tty-server`: the state, the view and
    // the update are host-tested, the bin is the window and the event pump.
    // **`test-harness` only**, not `mode.features()` — which is the kernel's, and would hand
    // this `selftest` as well. The feature makes the terminal report each
    // completed grid line on the debug console for `check-terminal` to assert on; a real build
    // must not have it, because a terminal narrating itself to the kernel log undoes the point
    // of the tty server owning output. A first version passed it unconditionally, so every
    // image shipped the instrumentation (PR #194 review, finding 3).
    build_userspace_bin("nxterm", mode.is_test_harness().then_some("test-harness"))?;
    // The file browser (M10 Part B). A lib/bin split like `nxterm`: the listing, the view and
    // the update are host-tested, the bin is the window and the event pump. No `test-harness`
    // feature — what its gate asserts is the line it prints when it lists a directory, which a
    // release image prints too, so there is nothing to compile in conditionally.
    build_userspace_bin("nxfiles", None)?;
    // The text editor (M10 Part D), split the same way and for the same reason. No
    // `test-harness` feature either: what its gate asserts is what it prints when it opens and
    // saves a file, which a release image prints too.
    build_userspace_bin("nxedit", None)?;
    // The installer (Phase 5 Part H.1). A lib/bin split like the two above, and built into
    // **every** image rather than the live one alone: it can reach a disk only where a session
    // was given one, so in a release image it is a program that resolves nothing — and
    // `check-images` holds the live root to being the release root file for file, which a
    // live-only program would break.
    build_userspace_bin("nxinstall", None)?;
    // A library with no consumer yet — see `check_userspace_lib`. `compositor` no longer
    // needs one: its own bin compiles it for the target.
    check_userspace_lib("libdraw")?;
    check_userspace_lib("libsurface")?;
    check_userspace_lib("libinput")?;
    check_userspace_lib("libui")?;
    check_userspace_lib("libterm")?;

    let kernel_dir = repo_root().join("kernel");
    let mut k = Command::new("cargo");
    k.arg("build");
    if let Some(f) = mode.kernel_features() {
        k.arg("--features").arg(f);
    }
    run(k.current_dir(&kernel_dir))?;
    let elf = kernel_elf();
    if !elf.exists() {
        return Err(format!("kernel ELF missing after build: {}", elf.display()).into());
    }
    println!("xtask: built kernel ELF at {}", elf.display());
    Ok(())
}

/// The userspace target's name — the stem of `userspace/x86_64-unknown-nitrox.json`,
/// which is also the directory cargo puts its artifacts under.
///
/// A **custom** spec rather than a built-in target because userspace needs a hard-float
/// ABI and stable rustc ships no freestanding x86_64 target that has one (see the
/// decision log, 2026-07-21). The kernel keeps the built-in `x86_64-unknown-none`.
const USERSPACE_TARGET: &str = "x86_64-unknown-nitrox";

/// A `cargo` command for the **userspace** workspace, resolved against
/// `userspace/rust-toolchain.toml`.
///
/// xtask is itself launched by `cargo run`, which exports `RUSTUP_TOOLCHAIN` into our
/// environment — and that variable *overrides* rustup's directory-based lookup, so a
/// child `cargo` would silently stay on the outer (stable) toolchain and reject `-Z`.
/// Clearing it lets `userspace/rust-toolchain.toml` govern, which keeps the nightly pin
/// in exactly one place instead of duplicating the version string here. `RUSTC`/`RUSTDOC`
/// go too: cargo points them at the outer toolchain's binaries, which would defeat the
/// re-resolution.
fn userspace_cargo() -> Command {
    let mut c = Command::new("cargo");
    c.env_remove("RUSTUP_TOOLCHAIN").env_remove("RUSTC").env_remove("RUSTDOC");
    c
}

/// Point a bare-target `cargo` invocation at the Nitrox userspace target.
///
/// A custom spec has no precompiled sysroot, so `core`/`alloc`/`compiler_builtins` are
/// rebuilt from source with `-Z build-std` — the reason `userspace/rust-toolchain.toml`
/// pins a nightly. `compiler-builtins-mem` is deliberately **not** requested: `libkern`
/// exports its own `memcpy`/`memmove`/`memset`/`memcmp`, and enabling the feature would
/// define them twice.
///
/// `--target` is passed as the JSON path relative to the crate directory (which is the
/// cwd for these builds), matching the crate's own `.cargo/config.toml`.
fn arg_userspace_target(c: &mut Command) {
    c.arg("--target")
        .arg(format!("../{USERSPACE_TARGET}.json"))
        .arg("-Z")
        .arg("build-std=core,alloc,compiler_builtins");
}

/// Directory holding built userspace release artifacts.
pub(crate) fn userspace_release_dir() -> PathBuf {
    repo_root().join("userspace/target").join(USERSPACE_TARGET).join("release")
}

/// Path to the built `hello` userspace ELF (release; the kernel embeds this).
fn hello_elf() -> PathBuf {
    userspace_release_dir().join("hello")
}

/// Build the `hello` userspace program as a static `ET_EXEC` for the bare
/// target. Run from `userspace/hello` so that crate's `.cargo/config.toml`
/// (target + non-PIE/static rustflags) applies without affecting the other
/// userspace members.
fn cmd_build_hello() -> R<()> {
    let hello_dir = repo_root().join("userspace").join("hello");
    let mut c = userspace_cargo();
    c.arg("build").arg("--release");
    arg_userspace_target(&mut c);
    run(c.current_dir(&hello_dir))?;
    let elf = hello_elf();
    if !elf.exists() {
        return Err(format!("hello ELF missing after build: {}", elf.display()).into());
    }
    println!("xtask: built hello ELF at {}", elf.display());
    Ok(())
}

/// Build the userspace program `name` as a static `ET_EXEC` for the bare
/// target (run from its own crate dir so its `.cargo/config.toml` applies). The
/// kernel embeds the result via `include_bytes!`. Generalises `cmd_build_hello`
/// for the spawn-demo binaries (`parent`, `child`).
fn build_userspace_bin(name: &str, features: Option<&str>) -> R<()> {
    build_userspace_crate(name, &[name], features)
}

/// Build a userspace **library** crate for the bare target, purely to prove it still
/// compiles there.
///
/// Libraries are normally built as a side effect of the bins that depend on them, so they
/// need no entry here. `libdraw` is the exception: it lands (plan M1 Part A) before the
/// compositor that will consume it, so nothing would compile it for
/// `x86_64-unknown-nitrox` at all, and a `no_std` regression — an accidental `std` path,
/// an `alloc` item behind the wrong cfg — would sit undetected until Part B went to build
/// on it. Its host tests pass on the host target and prove nothing about that.
///
/// Delete the entry once a bin depends on the crate; the bin's build covers it then.
fn check_userspace_lib(dir: &str) -> R<()> {
    let crate_dir = repo_root().join("userspace").join(dir);
    let mut c = userspace_cargo();
    c.arg("build").arg("--release").arg("-p").arg(dir);
    arg_userspace_target(&mut c);
    run(c.current_dir(&crate_dir))?;
    println!("xtask: {dir} compiles for {USERSPACE_TARGET} ✓");
    Ok(())
}

/// Build the userspace crate in `userspace/<dir>` for the bare target, then verify each of
/// `bins` produced an ELF. Most crates are one directory per program, so
/// [`build_userspace_bin`] covers them; a crate holding several programs (`coreutils`,
/// `test-harness`) names them here, since the directory no longer matches the bin name.
///
/// The build must run with `cwd` = the crate directory: its `.cargo/config.toml` is what
/// selects the custom target and the static/non-PIE link flags.
fn build_userspace_crate(dir: &str, bins: &[&str], features: Option<&str>) -> R<()> {
    let crate_dir = repo_root().join("userspace").join(dir);
    let mut c = userspace_cargo();
    c.arg("build").arg("--release");
    arg_userspace_target(&mut c);
    if let Some(f) = features {
        c.arg("--features").arg(f);
    }
    run(c.current_dir(&crate_dir))?;
    for name in bins {
        let elf = userspace_release_dir().join(name);
        if !elf.exists() {
            return Err(format!("{name} ELF missing after build: {}", elf.display()).into());
        }
        println!("xtask: built {name} ELF at {}", elf.display());
    }
    Ok(())
}

pub(crate) fn cmd_image(mode: BuildMode) -> R<()> {
    cmd_build(mode)?;
    let limine_root = cmd_fetch_limine()?;
    let bootx64 = find_bootx64(&limine_root)?;
    let initramfs = initramfs_path();
    build_initramfs(&initramfs, mode)?;
    assemble_image(&bootx64, &kernel_elf(), &limine_conf(), &initramfs, &image_path(), mode)?;
    println!("xtask: image at {}", image_path().display());
    Ok(())
}

pub(crate) fn cmd_fetch_limine() -> R<PathBuf> {
    let dir = limine_dir();
    let marker = dir.join(".version");
    if marker.exists()
        && let Ok(v) = fs::read_to_string(&marker)
        && v.trim() == LIMINE_VERSION
    {
        return Ok(dir);
    }

    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;

    let url =
        format!("https://github.com/limine-bootloader/limine/releases/download/{LIMINE_VERSION}/limine-binary.tar.gz");
    let tarball = build_cache().join("limine-binary.tar.gz");
    fs::create_dir_all(build_cache())?;
    println!("xtask: fetching {url}");
    run(Command::new("curl").arg("-fL").arg("--retry").arg("3").arg("-o").arg(&tarball).arg(&url))?;

    run(Command::new("tar").arg("-xzf").arg(&tarball).arg("-C").arg(&dir).arg("--strip-components=1"))?;

    fs::remove_file(&tarball).ok();
    fs::write(&marker, LIMINE_VERSION)?;
    Ok(dir)
}

pub(crate) fn cmd_clean() -> R<()> {
    let kernel_dir = repo_root().join("kernel");
    run(Command::new("cargo").arg("clean").current_dir(&kernel_dir))?;
    let userspace_dir = repo_root().join("userspace");
    run(Command::new("cargo").arg("clean").current_dir(&userspace_dir))?;
    let cache = build_cache();
    if cache.exists() {
        fs::remove_dir_all(&cache)?;
        println!("xtask: removed {}", cache.display());
    }
    Ok(())
}
