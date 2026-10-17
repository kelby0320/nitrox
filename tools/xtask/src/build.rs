//! `build`, `image`, `fetch-limine` and `clean`, and the userspace builds under them.

use crate::*;

/// The coreutils: one crate, a bin per program, packed from this single list.
pub(crate) const COREUTILS: &[&str] = &[
    "list", "copy", "mkdir", "remove", "rename", "move", "touch", "date", "sleep", "whoami",
    // The graphical session's desktops, and `/dev/desktop`'s first consumer, typed at a shell like
    // the rest of these.
    "desktop",
    // The clipboard, either side of a pipe, which is what makes the kill ring reachable by
    // something other than a window.
    "clip",
    // Running a program in a view: the view broker's client, typed at a shell like the rest of
    // these.
    "with",
    // The machine's disks: `--list` from any session, `--mount` and `--unmount` from a view with
    // the `storage` grant.
    "disk",
    // The accounts: `--list` and your own `--password` from any session, the rest from a view with
    // the `accounts` grant, or on a users file for recovery.
    "account",
    // The services: `--list` from any session, and `--start`, `--stop` and `--restart` from a view
    // with the `services` grant.
    "service",
    // Stopping or restarting the machine, through the `power` grant that the seeded policy gives
    // everyone.
    "shutdown", // Reading the service log back, through the `logs` grant.
    "log",
];

/// The system services, packaged into the store like any other program.
///
/// **`fs-server-ext4` is here *and* in the initramfs.** The store copy is versioned and is what a
/// non-root fs-server (a second mount) would be spawned from. The initramfs copy is the only one
/// that can mount, or re-mount, the root. See `TODO(fs-server-restart)`.
pub(crate) const SYSTEM_SERVICES: &[&str] = &[
    "service-mgr",
    "session-mgr",
    "desktop-session-mgr",
    "desktop-shell",
    "auth-service",
    "logging-service",
    "fs-server-ext4",
    // Store-only: no root is FAT; the storage service spawns it from `/bin` for a stick.
    "fs-server-fat",
    "tty-server",
    // The display servers are store packages: neither has a bootstrap role, and a compositor
    // cannot run before a root exists.
    "compositor",
    "input-server",
    "clipboard-server",
    "view-broker",
    "device-mgr",
    "storage-service",
];

/// The test programs, packaged apart from `system` in selftest/test-harness builds, so the system
/// package's content hash is identical in a release and a test image.
const TEST_PROGRAMS: &[&str] = &[
    "test-harness",
    "test-stage",
    "display-selftest",
    "ui-testclient",
    "input-testclient",
    "boot-probe",
    "compose-bench",
    // `check-storage`'s writer and reader.
    "test-pattern",
    "restart-probe",
];

/// The `test` package's programs built from a crate of their own rather than `test-harness`'s:
/// **`heartbeat`**, the `always` service that runs for the whole boot of a test image.
const TEST_CRATE_PROGRAMS: &[&str] = &["heartbeat"];

/// Everything in the `test` package: [`TEST_PROGRAMS`], then [`TEST_CRATE_PROGRAMS`].
pub(crate) fn test_package() -> Vec<&'static str> {
    TEST_PROGRAMS.iter().chain(TEST_CRATE_PROGRAMS).copied().collect()
}

pub(crate) fn cmd_build(mode: BuildMode) -> R<()> {
    // Userspace first: the kernel embeds the ELFs via `include_bytes!`.
    cmd_build_hello()?;
    if mode.stages_test_data() {
        build_userspace_crate("test-harness", TEST_PROGRAMS, None)?;
        for prog in TEST_CRATE_PROGRAMS {
            build_userspace_bin(prog, None)?;
        }
    }
    // `None` in every mode: a feature flag nothing reads would still make the two builds' bytes
    // differ.
    build_userspace_bin("init", None)?;
    build_userspace_bin("fs-server-ext4", None)?;
    build_userspace_bin("fs-server-fat", None)?;
    build_userspace_bin("eshell", None)?;
    build_userspace_bin("service-mgr", None)?;
    build_userspace_crate("coreutils", COREUTILS, None)?;
    build_userspace_bin("nxsh", None)?;
    build_userspace_bin("profile-server", None)?;
    build_userspace_bin("tty-server", None)?;
    build_userspace_bin("input-server", None)?;
    build_userspace_bin("logging-service", None)?;
    build_userspace_bin("auth-service", None)?;
    build_userspace_bin("clipboard-server", None)?;
    build_userspace_bin("view-broker", None)?;
    build_userspace_bin("device-mgr", None)?;
    build_userspace_bin("storage-service", None)?;
    // `None`: no reader remains for either feature, and passing one would let a test-only branch
    // compile unnoticed.
    build_userspace_bin("session-mgr", None)?;
    build_userspace_bin("desktop-session-mgr", None)?;
    build_userspace_bin("desktop-shell", None)?;
    build_userspace_bin("compositor", None)?;
    // `test-harness` only, not the kernel's `mode.features()`: it makes the terminal log each grid
    // line for `check-terminal`, which a real build must not do.
    build_userspace_bin("nxterm", mode.is_test_harness().then_some("test-harness"))?;
    // No `test-harness` feature: its gate asserts a line a release image prints too.
    build_userspace_bin("nxfiles", None)?;
    build_userspace_bin("nxedit", None)?;
    // In **every** image: it reaches no disk a session was not given, and `check-images` holds the
    // live root to the release root file for file.
    build_userspace_bin("nxinstall", None)?;
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

/// The userspace target's name — the stem of `userspace/x86_64-unknown-nitrox.json` and the
/// directory cargo puts its artifacts under. A custom spec because userspace needs a hard-float
/// ABI that no stable freestanding x86_64 target has.
const USERSPACE_TARGET: &str = "x86_64-unknown-nitrox";

/// A `cargo` command for the **userspace** workspace, resolved against
/// `userspace/rust-toolchain.toml`: `cargo run` exports `RUSTUP_TOOLCHAIN` (and `RUSTC`/`RUSTDOC`),
/// which would keep the child on the stable toolchain.
fn userspace_cargo() -> Command {
    let mut c = Command::new("cargo");
    c.env_remove("RUSTUP_TOOLCHAIN").env_remove("RUSTC").env_remove("RUSTDOC");
    c
}

/// Point a bare-target `cargo` invocation at the userspace target. A custom spec has no sysroot, so
/// `core`/`alloc` are rebuilt with `-Z build-std`; `compiler-builtins-mem` is **not** requested, as
/// `libkern` exports its own `memcpy` family.
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

/// Path to the built `hello` userspace ELF (the kernel embeds this).
fn hello_elf() -> PathBuf {
    userspace_release_dir().join("hello")
}

/// Build the `hello` userspace program as a static `ET_EXEC` for the bare target.
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

/// Build the userspace program `name` as a static `ET_EXEC` for the bare target.
fn build_userspace_bin(name: &str, features: Option<&str>) -> R<()> {
    build_userspace_crate(name, &[name], features)
}

/// Build a userspace **library** crate for the bare target, purely to prove it still compiles
/// there: a `no_std` regression in a library no bin consumes would go undetected. Delete the
/// entry once a bin depends on the crate.
fn check_userspace_lib(dir: &str) -> R<()> {
    let crate_dir = repo_root().join("userspace").join(dir);
    let mut c = userspace_cargo();
    c.arg("build").arg("--release").arg("-p").arg(dir);
    arg_userspace_target(&mut c);
    run(c.current_dir(&crate_dir))?;
    println!("xtask: {dir} compiles for {USERSPACE_TARGET} ✓");
    Ok(())
}

/// Build the userspace crate in `userspace/<dir>`, then verify each of `bins` produced an ELF. A
/// crate holding several programs names them, since its directory does not match the bin name.
/// Run from the crate's directory, whose `.cargo/config.toml` selects the target and link flags.
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
