//! Nitrox build orchestrator.
//!
//! Subcommands:
//!   build           build the kernel ELF
//!   image           build kernel + assemble a UEFI-bootable GPT/FAT32 image
//!   qemu            build + launch QEMU with OVMF
//!   qemu-debug      build + launch QEMU paused for GDB on :1234
//!   test            host-side unit tests (kernel lib + tools workspace)
//!   test-qemu       boot a headless self-test image; adjudicate via isa-debug-exit
//!   check-deferrals fail if a `TODO(tag)` has no deferred-decisions.md entry
//!   check-irq-scope fail if an interrupt entry stub skips the lock-ordering scope
//!   abi-sync-check  fail if userspace/libkern has drifted from the kernel ABI
//!   fetch-limine    download the pinned limine-binary tarball into the cache
//!   clean           remove all build outputs and caches
//!
//! Stays on std and avoids external crates so the host build can be a
//! single `cargo run -p xtask`. No "stable Rust only" rule applies here
//! the way it does to the kernel; this is host tooling.

use std::env;
use std::error::Error;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Limine version we build against. Bump this together with any changes
/// to `kernel/src/limine.rs`.
const LIMINE_VERSION: &str = "v12.2.0";

/// Total boot-disk size, in MiB. Holds two GPT partitions: the EFI System Partition
/// (FAT32, [`ESP_SIZE_MIB`]) and the ext4 `nitrox-root` filesystem (the rest).
const IMAGE_SIZE_MIB: u64 = 128;
/// The EFI System Partition size. Comfortably above the FAT32 minimum so
/// `mformat -F` (forced FAT32) is always valid; the rest of the disk is the ext4
/// `nitrox-root` partition.
const ESP_SIZE_MIB: u64 = 48;

// Test-only fixture credential for the auth + session-mgr login-path demo. **Not a
// secret**: the shipped image stores only the one-way PBKDF2 verifier of
// `DEMO_PASSWORD`; the password is a build input for the emulator demo user. init's
// login selftest (auth Part E) must use these same literals. A fixed salt keeps the
// image build reproducible (a single demo user makes salt-uniqueness moot). See
// `docs/architecture/session-and-auth.md`.
const DEMO_USER: &str = "alice";
const DEMO_PASSWORD: &str = "correct horse battery staple";
const DEMO_HOME: &str = "/home/alice";
const DEMO_SALT: [u8; 8] = [0x9e, 0x3f, 0xa2, 0x5c, 0x71, 0x0b, 0xd4, 0x86];

type R<T> = Result<T, Box<dyn Error>>;

/// What to compile into the kernel + `init`. Selects the cargo feature the two
/// crates are built with; the other userspace binaries are always feature-less.
#[derive(Clone, Copy, PartialEq)]
enum BuildMode {
    /// Production boot: straight to userspace, no demos.
    Normal,
    /// `--selftest`: compile + run the boot self-tests / demos, then drop to eshell.
    Selftest,
    /// `test-qemu`: everything `Selftest` runs, plus the `isa-debug-exit` verdict path
    /// so the run self-adjudicates headlessly (`test-harness` feature).
    TestHarness,
}

impl BuildMode {
    /// The cargo `--features` value for the kernel + `init` builds (`None` = no flag).
    fn features(self) -> Option<&'static str> {
        match self {
            BuildMode::Normal => None,
            BuildMode::Selftest => Some("selftest"),
            BuildMode::TestHarness => Some("test-harness"),
        }
    }
}

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let cmd = args.next();
    let rest: Vec<String> = args.collect();

    // `--selftest` (anywhere in the args) compiles + runs the boot self-tests / demos
    // (kernel `boot_selftest` + init's demo chain); without it the build boots straight
    // to userspace. Strip it out before forwarding the rest to QEMU.
    let selftest = rest.iter().any(|a| a == "--selftest");
    // `--kvm` runs the guest under hardware virtualisation instead of TCG. Two reasons to
    // reach for it: speed (a boot loop runs in a fraction of the time), and hosts whose
    // QEMU predates 9.0 and therefore cannot emulate the x2APIC this kernel requires —
    // KVM has no such limit. Stripped before the rest is forwarded to QEMU.
    let accel = if rest.iter().any(|a| a == "--kvm") {
        Accel::Kvm
    } else {
        Accel::Tcg
    };
    let qargs: Vec<String> = rest
        .iter()
        .filter(|a| *a != "--selftest" && *a != "--kvm")
        .cloned()
        .collect();
    let mode = if selftest {
        BuildMode::Selftest
    } else {
        BuildMode::Normal
    };

    let result = match cmd.as_deref() {
        Some("build") => cmd_build(mode),
        Some("image") => cmd_image(mode),
        Some("qemu") => cmd_qemu(false, mode, accel, &qargs),
        Some("qemu-debug") => cmd_qemu(true, mode, accel, &qargs),
        Some("test") => cmd_test(),
        Some("test-qemu") => cmd_test_qemu(accel),
        Some("test-interactive") => cmd_test_interactive(accel),
        Some("check-arch") => cmd_check_arch(),
        Some("check-nightly") => cmd_check_nightly(),
        Some("check-deferrals") => cmd_check_deferrals(),
        Some("check-irq-scope") => cmd_check_irq_scope(),
        Some("abi-sync-check") => cmd_abi_sync_check(),
        Some("fetch-limine") => cmd_fetch_limine().map(|_| ()),
        Some("clean") => cmd_clean(),
        Some("help") | Some("--help") | Some("-h") | None => {
            print_help();
            Ok(())
        }
        Some(other) => Err(format!("unknown subcommand: {other}").into()),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_help() {
    println!(
        "Nitrox build orchestrator.\n\
         \n\
         Usage: cargo xtask <command> [args]\n\
         \n\
         Commands:\n  \
           build         build the kernel ELF\n  \
           image         build + assemble a UEFI-bootable disk image\n  \
           qemu          build + launch QEMU with OVMF\n  \
           qemu-debug    build + launch QEMU paused for GDB on :1234\n  \
           test          host-side unit tests (kernel lib + tools)\n  \
           test-qemu     boot a headless self-test image; pass/fail via isa-debug-exit\n  \
           check-arch    fail if kernel code outside arch/ uses arch internals\n  \
           check-nightly fail if any crate uses a nightly `#![feature(...)]`\n  \
           check-deferrals fail if a `TODO(tag)` has no deferred-decisions.md entry\n  \
           check-irq-scope fail if an interrupt entry stub skips the lock-ordering scope\n  \
           abi-sync-check  fail if userspace/libkern has drifted from the kernel ABI\n  \
           fetch-limine  download the pinned Limine binary tarball\n  \
           clean         remove build outputs and caches\n  \
           help          show this message\n\
         \n\
         `--selftest` (build/image/qemu) compiles + runs the boot self-tests / demos;\n         \
         without it the build boots straight to userspace.\n         \
         `--kvm` (qemu/qemu-debug/test-qemu) runs under hardware virtualisation instead\n         \
         of TCG — faster, and required on a host whose QEMU predates 9.0 (TCG emulates\n         \
         x2APIC only from 9.0, and this kernel is x2APIC-only).\n         \
         Other args after `qemu` / `qemu-debug` are forwarded to QEMU.\n"
    );
}

// --- Paths --------------------------------------------------------------

fn repo_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `tools/xtask`; the repo root is two up.
    let manifest = env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let path = PathBuf::from(&manifest);
    match path.parent().and_then(Path::parent) {
        Some(p) => p.to_path_buf(),
        None => panic!("cannot derive repo root from {manifest}"),
    }
}

fn build_cache() -> PathBuf {
    repo_root().join("tools").join("build-cache")
}

fn limine_dir() -> PathBuf {
    build_cache().join("limine")
}

fn kernel_elf() -> PathBuf {
    repo_root()
        .join("kernel")
        .join("target")
        .join("x86_64-unknown-none")
        .join("debug")
        .join("nitrox-kernel")
}

fn image_path() -> PathBuf {
    build_cache().join("nitrox.hdd")
}

fn limine_conf() -> PathBuf {
    repo_root().join("boot").join("limine.conf")
}

// --- Subcommands --------------------------------------------------------

/// The coreutils: one crate, a bin per program. Built, packed into the initramfs, and
/// packaged into the store from this single list — see [`profile_programs`].
const COREUTILS: &[&str] = &[
    "list", "copy", "mkdir", "remove", "rename", "move", "touch", "date", "sleep", "whoami",
];

/// The system services, packaged into the store like any other program.
///
/// **`fs-server-ext4` is here *and* in the initramfs**, and the two copies serve different
/// purposes. The store copy makes it versioned and content-addressed like everything else,
/// and is what a **non-root** fs-server (a second mount) would be spawned from, since root
/// is already up by then. The initramfs copy is the only one that can ever mount the root
/// — and the only one that could ever *re*-mount it, because `/store` is unreadable without
/// the very server being restarted. See `TODO(fs-server-restart)`.
const SYSTEM_SERVICES: &[&str] = &[
    "service-mgr",
    "session-mgr",
    "auth-service",
    "logging-service",
    "heartbeat",
    "fs-server-ext4",
    "tty-server",
];

fn cmd_build(mode: BuildMode) -> R<()> {
    // Build the userspace programs BEFORE the kernel: the kernel embeds their
    // ELFs via `include_bytes!`, so the artifacts must exist at kernel compile
    // time. Only `init` (and the kernel) carry the selftest / test-harness feature.
    cmd_build_hello()?;
    // The integration smoke-test harness (bins `test-harness` + `test-stage`) is built
    // + embedded ONLY in selftest/test-harness builds — absent from release images.
    if mode.features().is_some() {
        build_userspace_bin("test-harness", None)?;
    }
    build_userspace_bin("init", mode.features())?;
    build_userspace_bin("fs-server-ext4", None)?;
    build_userspace_bin("eshell", None)?;
    build_userspace_bin("service-mgr", None)?;
    build_userspace_bin("heartbeat", None)?;
    // The coreutils (`list`, …) — real programs, present in release images. One crate,
    // a bin per program, so the crate directory is named separately from the bins.
    build_userspace_crate("coreutils", COREUTILS, None)?;
    // `nxsh` — the shell. Milestone 3 Part A ships the *language* (lexer + parser, tested
    // on the host); this builds its bin for the bare target so the language cannot
    // quietly stop being part of the OS while it is being written.
    build_userspace_bin("nxsh", None)?;
    build_userspace_bin("profile-server", None)?;
    build_userspace_bin("tty-server", None)?;
    build_userspace_bin("logging-service", None)?;
    build_userspace_bin("auth-service", None)?;
    // session-mgr fires the self-test verdict, so it takes the build-mode feature
    // (`selftest`/`test-harness`) like init.
    build_userspace_bin("session-mgr", mode.features())?;

    let kernel_dir = repo_root().join("kernel");
    let mut k = Command::new("cargo");
    k.arg("build");
    if let Some(f) = mode.features() {
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
    c.env_remove("RUSTUP_TOOLCHAIN")
        .env_remove("RUSTC")
        .env_remove("RUSTDOC");
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
fn userspace_release_dir() -> PathBuf {
    repo_root()
        .join("userspace/target")
        .join(USERSPACE_TARGET)
        .join("release")
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

fn cmd_image(mode: BuildMode) -> R<()> {
    cmd_build(mode)?;
    let limine_root = cmd_fetch_limine()?;
    let bootx64 = find_bootx64(&limine_root)?;
    let initramfs = initramfs_path();
    build_initramfs(&initramfs, mode)?;
    assemble_image(
        &bootx64,
        &kernel_elf(),
        &limine_conf(),
        &initramfs,
        &image_path(),
    )?;
    println!("xtask: image at {}", image_path().display());
    Ok(())
}

/// Append the machine / CPU / memory / UEFI-firmware flags shared by every QEMU
/// launch (`qemu`, `qemu-debug`, `test-qemu`) to `qemu`.
/// How the guest CPU is executed.
///
/// The kernel is **x2APIC-only** (decision log, 2026-06-26), which sets a hard floor on
/// the host side: QEMU's TCG only emulates x2APIC from **9.0**, so an older QEMU boots to
/// a kernel panic ("CPU lacks x2APIC"). KVM has no such limit — the in-kernel APIC has
/// supported x2APIC for years — so `--kvm` is both the fast path and the way to run on a
/// host whose QEMU is too old to emulate it.
#[derive(Clone, Copy, PartialEq)]
enum Accel {
    /// Pure emulation. Requires QEMU ≥ 9.0 for x2APIC.
    Tcg,
    /// Hardware virtualisation (`-enable-kvm -cpu host`). Needs `/dev/kvm`.
    Kvm,
}

/// The host QEMU's `(major, minor)` version, or `None` if it could not be parsed.
fn qemu_version() -> Option<(u32, u32)> {
    let out = Command::new("qemu-system-x86_64").arg("--version").output().ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    // "QEMU emulator version 9.2.0 (…)" — take the first dotted number.
    let ver = text.split_whitespace().find(|w| w.contains('.') && w.starts_with(|c: char| c.is_ascii_digit()))?;
    let mut parts = ver.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().unwrap_or(0);
    Some((major, minor))
}

/// Fail *before* launching if the host cannot give the guest what the kernel requires, so
/// the operator sees an actionable message instead of a kernel panic from inside QEMU.
///
/// This check exists because the failure it replaces was genuinely confusing: a CI runner
/// with QEMU 8.2 booted to `*** KERNEL PANIC *** CPU lacks x2APIC`, which reads like a
/// kernel bug and is actually a host-tooling floor.
fn preflight_accel(accel: Accel) -> R<()> {
    match accel {
        Accel::Kvm => {
            if !Path::new("/dev/kvm").exists() {
                return Err("`--kvm` requested but /dev/kvm is absent — no hardware \
                     virtualisation available (nested virt off, or the kvm module is not \
                     loaded). Drop `--kvm` to use TCG, which needs QEMU >= 9.0."
                    .into());
            }
        }
        Accel::Tcg => {
            if let Some((major, minor)) = qemu_version() {
                if major < 9 {
                    return Err(format!(
                        "QEMU {major}.{minor} is too old: TCG only emulates x2APIC from 9.0, \
                         and this kernel is x2APIC-only (decision log 2026-06-26), so the \
                         guest would panic with \"CPU lacks x2APIC\". Use `--kvm` (needs \
                         /dev/kvm) or install QEMU >= 9.0."
                    )
                    .into());
                }
            }
        }
    }
    Ok(())
}

fn qemu_base_args(qemu: &mut Command, ovmf: &Firmware, accel: Accel) -> R<()> {
    qemu.arg("-M")
        .arg("q35")
        // CPU model = QEMU's `max`: every feature the emulator can provide,
        // which is a strict superset of what the kernel requires. The kernel
        // enables only what it opts into (SMAP/SMEP in `init_protections`;
        // x87+SSE+AVX in `fpu_init_cpu`'s `XCR0`, never AVX-512 or LA57), so a
        // richer model changes nothing it doesn't ask for. `max` supplies, in
        // one word, everything the previous hand-rolled `qemu64,+…` string spelt
        // out — SMEP/SMAP (user-access protections), the on-chip + x2APIC local
        // controller, RDRAND/RDSEED (hardware CSPRNG seed), RDTSCP (`current_cpu`
        // reads `IA32_TSC_AUX`) — **plus** a properly-emulated XSAVE/AVX extended
        // state. That last point is why we moved off `qemu64`: splicing
        // `+xsave,+avx` onto the ancient `qemu64` model *hangs* TCG at the
        // `CR4.OSXSAVE` enable (a QEMU emulation fragility), whereas `max`
        // emulates the whole XSAVE path and boots clean. The real hardware path
        // is additionally proven under KVM (`-cpu host`); see the decision log
        // (2026-07-21 floating-point). x2APIC needs QEMU ≥ 9.0. SMP runs `-smp N`.
        .arg("-m")
        .arg("256M");
    // `max` (TCG) is every feature the emulator can provide; `host` (KVM) is every
    // feature the physical CPU has. Both are strict supersets of what the kernel asks
    // for, and both carry x2APIC — under TCG only from QEMU 9.0, which `preflight_accel`
    // checks before we get here.
    match accel {
        Accel::Tcg => {
            qemu.arg("-cpu").arg("max");
        }
        Accel::Kvm => {
            qemu.arg("-enable-kvm").arg("-cpu").arg("host");
        }
    }
    // UEFI firmware pflash drive(s) — split CODE+VARS on modern QEMU, or a
    // single combined image on legacy setups (see `locate_ovmf`).
    for a in firmware_pflash_args(ovmf)? {
        qemu.arg(a);
    }
    Ok(())
}

fn cmd_qemu(debug: bool, mode: BuildMode, accel: Accel, extra_args: &[String]) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(mode)?;
    let ovmf = locate_ovmf()?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut qemu, &ovmf, accel)?;
    qemu.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-serial")
        .arg("stdio")
        .arg("-no-reboot")
        .arg("-no-shutdown");
    if debug {
        qemu.arg("-S").arg("-s");
        println!("xtask: QEMU paused on entry; attach gdb to localhost:1234");
    }
    for a in extra_args {
        qemu.arg(a);
    }
    run(&mut qemu)
}

/// **Interactive-session tests: drive the real login and shell over the serial console.**
///
/// This boots `BuildMode::Normal` — the **release image**, which nothing else ever boots.
/// `test-qemu` runs the `test-harness` build, where session-mgr auto-logs-in and runs a
/// fixed script; the `login:` prompt, a typed password, a real shell prompt and `exit` are
/// all `#[cfg(not(feature = "test-harness"))]` code that CI compiled and never executed.
/// Every interactive bug this project has had lived exactly there — the console read using
/// the wrong rights, a `cd` guard refusing a builtin that existed, a login that could not
/// be repeated, a password prompt landing on the username's line.
///
/// **Expect-driven, not sleep-driven.** Each step waits for the text that says the guest is
/// ready for it, so the run is paced by the guest rather than by guessed delays. That is
/// the difference between a test and a flake.
///
/// One boot serves every scenario: the shell returns to `login:`, so the session sequence
/// continues rather than paying ~15 s of boot per case.
fn cmd_test_interactive(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::Normal)?;
    let ovmf = locate_ovmf()?;

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel)?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        // **`signal=off` is what lets the guest see `Ctrl-C`.** QEMU's `stdio` chardev
        // defaults to `signal=on`, where `0x03` on stdin is QEMU's own interrupt and never
        // reaches the guest — so the harness could type every key except the one §11h is
        // about. Spelled as an explicit chardev because `-serial stdio` has no way to say
        // it.
        .arg("-chardev")
        .arg("stdio,id=hostserial,signal=off")
        .arg("-serial")
        .arg("chardev:hostserial")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());

    println!("xtask: interactive session tests (release image, expect-driven)…\n");
    let mut session = Session::spawn(cmd)?;
    let result = run_interactive_scenarios(&mut session);
    let transcript = session.finish();

    match result {
        Ok(n) => {
            println!("\nxtask: interactive tests PASSED ({n} steps)");
            Ok(())
        }
        Err(e) => {
            // The transcript is the diagnosis — without it a failure says only "expected X".
            println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
            Err(e)
        }
    }
}

/// The scenarios, in one boot. Returns the number of steps that passed.
fn run_interactive_scenarios(s: &mut Session) -> R<usize> {
    let mut steps = 0usize;

    // 1. The machine reaches a login prompt at all — the release image's first claim.
    s.expect("nitrox login:")?;
    steps += 1;

    // 2. A wrong password is refused and the prompt comes back. Tested before the good
    //    one so a broken denial cannot hide behind a successful login.
    s.send("alice")?;
    s.expect("password:")?;
    s.send("wrong-password")?;
    s.expect("login incorrect")?;
    s.expect("nitrox login:")?;
    steps += 1;

    // 3. The password prompt begins its own line. `read_line` used to return on CR/LF
    //    without echoing it, so the cursor never left the username's line and the prompt
    //    rendered as `alicepassword:`.
    s.send("alice")?;
    s.expect("\npassword:")?;
    steps += 1;

    // 4. A correct password reaches a shell prompt.
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    steps += 1;

    // 5. A program from the profile runs — `/bin` is bound in the session namespace and
    //    the shell can spawn through it.
    s.send("whoami")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 6. A *failing* stage reports and returns to the prompt rather than hanging. This is
    //    the shape that hung the shell for an afternoon: a stage that exits without
    //    writing, whose pipe only closes once its process is reclaimed.
    s.send("list /nope")?;
    s.expect("filesystem unreachable")?;
    s.expect("/home>")?;
    steps += 1;

    // 7. The interpreter keeps state across lines — a `def` typed at the prompt is
    //    callable afterwards.
    s.send("def add(a, b) { a + b }")?;
    s.expect("/home>")?;
    s.send("add(2, 3)")?;
    s.expect("5")?;
    steps += 1;

    // 8. **History.** `\x1b[A` is Up; `send` appends the Enter. Recalling `whoami` and
    //    running it proves the *buffer* was replaced, which is the part that matters —
    //    the erase-and-redraw is bytes on a wire that only a real terminal renders, so
    //    asserting on appearance would assert on the capture rather than the shell.
    s.send("whoami")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    s.send("\x1b[A")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 9. Two Ups walk further back: with `date` most recent, the second Up reaches
    //    `whoami` again. This is what distinguishes a working cursor from a history that
    //    only ever returns its newest entry.
    s.send("date")?;
    s.expect("unix")?;
    s.expect("/home>")?;
    s.send("\x1b[A\x1b[A")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 10. **Reverse-search.** `\x12` is Ctrl-R; the query narrows to `whoami`, and Enter
    //     runs it. Asserting on the command's *output* again, not on the search prompt —
    //     the redraw is erase-and-rewrite, which only a real terminal renders.
    s.send("\x12whoa")?;
    s.expect("alice")?;
    s.expect("/home>")?;
    steps += 1;

    // 11. **Cancelling a search restores what was being typed.** Type `date`, search for
    //     something else, abandon it with Ctrl-G, press Enter — `date` must run. This is
    //     the property that makes Ctrl-R safe to press by accident.
    s.send("date\x12who\x07")?;
    s.expect("unix")?;
    s.expect("/home>")?;
    steps += 1;

    // 12. **`break` at a real prompt** — and the multi-line path with it. The body holds
    //     two statements, which are newline-separated (§9a), so the loop spans two lines
    //     and the REPL's unclosed-brace continuation carries it (§11b). The `mut` survives
    //     from the line before, which is what makes the count observable at all. Host
    //     tests cover the semantics; this covers the path they cannot — every expensive
    //     bug this shell has had was interactive-only.
    //
    //
    //     Two things here were got wrong first and are worth keeping right. **The
    //     assertion is `n=3`, not `3`**: `expect` scans forward until it matches, so a
    //     bare digit finds the echo of the line that was just typed. And **the increment
    //     comes before the test**, so that "the loop stopped" and "the rest of the body
    //     was skipped" give different answers — with the increment last, a `break` that
    //     merely abandoned the iteration would leave `n` at 3 as well, and the step would
    //     pass against a broken implementation. Both were found by breaking `break` and
    //     watching this step keep passing.
    s.send("mut n = 0")?;
    s.expect("/home>")?;
    s.send("for x in 0..9 { n = n + 1\n if n == 3 { break } }")?;
    s.expect("/home>")?;
    s.send("format(\"n={}\", n)")?;
    s.expect("n=3")?;
    s.expect("/home>")?;
    steps += 1;

    // 13. **`parse` at a real prompt**, both directions of §6's contract: text becomes a
    //     number and takes part in arithmetic, and text that is not a number fails loud
    //     rather than becoming zero. `format` wraps the answer for the same reason as the
    //     step above — `42` on its own would match the echo of what was typed, `sum=42`
    //     cannot.
    s.send("format(\"sum={}\", (\"40\" | parse Int) + 2)")?;
    s.expect("sum=42")?;
    s.expect("/home>")?;
    s.send("\"abc\" | parse Int")?;
    s.expect("cannot parse")?;
    s.expect("/home>")?;
    steps += 1;

    // 14. **Errors are constructible and recoverable, at a real prompt.** `fail` raises
    //     with a message a script chose, `catch` binds it as an ordinary record, and `try`
    //     in value position supplies a fallback — the three halves of §2 that did not
    //     exist before Part C. `n=8080` rather than `8080` for the reason step 12 gives.
    s.send("try { fail \"boom\" } catch (e) { e.message }")?;
    s.expect("boom")?;
    s.expect("/home>")?;
    s.send("let n = try { \"x\" | parse Int } catch { 8080 }")?;
    s.expect("/home>")?;
    s.send("format(\"n={}\", n)")?;
    s.expect("n=8080")?;
    s.expect("/home>")?;
    steps += 1;

    // 15. **Sequences and reduction** at a real prompt: a String is a sequence of its
    //     characters (so `count` is its length), and a Range is a sequence of its values
    //     (so it can be summed without first being written out as a list). Both were
    //     "expected a Table or a List" before Part D.
    s.send("format(\"len={}\", (\"hello\" | count))")?;
    s.expect("len=5")?;
    s.expect("/home>")?;
    s.send("format(\"sum={}\", (1..=10 | sum))")?;
    s.expect("sum=55")?;
    s.expect("/home>")?;
    // …and Part E's breadth: text taken apart and put back, and membership.
    s.send("\"a,b,c\" | split \",\" | join \"-\"")?;
    s.expect("a-b-c")?;
    s.expect("/home>")?;
    s.send("format(\"hit={}\", (2 in [1, 2]))")?;
    s.expect("hit=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 16. **`capture` at a real prompt**, the shape §10b writes: match, take a group,
    //     convert it. Text that matched could only be tested before, never taken apart.
    //     The second line pins the alternation fix — `~=` with a second branch that has to
    //     win was false for the entire life of the engine.
    s.send("let g = \"port 8080\" | capture /(\\d+)/")?;
    s.expect("/home>")?;
    s.send("format(\"port={}\", (g[1] | parse Int))")?;
    s.expect("port=8080")?;
    s.expect("/home>")?;
    s.send("format(\"alt={}\", (\"b\" ~= /a|b/))")?;
    s.expect("alt=true")?;
    s.expect("/home>")?;
    steps += 1;

    // 17. **`Ctrl-C` interrupts a running evaluation** (§11h) — the hazard that section
    //     exists for. `while true { }` has no exit and, on a system with no SIGINT, no way
    //     to be stopped: it meant a reboot.
    //
    //     The loop and the interrupt go in **one write** deliberately. Sent separately it
    //     is a race the test loses: an empty loop reaches the ten-million-iteration
    //     backstop in well under a second, so the run would end on its own and the step
    //     would pass without the interrupt ever arriving. One write puts the byte in the
    //     guest before evaluation starts, and the tty holds it until the shell's next
    //     checkpoint asks.
    s.send_raw("while true { }\n\x03")?;
    s.expect("interrupted")?;
    s.expect("/home>")?;
    // …and the shell is still usable afterwards, which is the actual claim.
    s.send("format(\"alive={}\", 1 + 1)")?;
    s.expect("alive=2")?;
    s.expect("/home>")?;
    steps += 1;

    // 18. **`Ctrl-C` at a prompt discards the line** rather than leaving it half-typed —
    //     the same event, and the shell decides what it means from what it was doing.
    s.send_raw("garbage-that-should-vanish\x03")?;
    s.expect("^C")?;
    s.expect("/home>")?;
    s.send("format(\"clean={}\", 2 + 2)")?;
    s.expect("clean=4")?;
    s.expect("/home>")?;
    steps += 1;

    // 19. **`Ctrl-C` stops a running *stage*** (§11h, G2).
    //
    //     The ordering is made deterministic rather than assumed, and that took two goes.
    //     Sending `sleep 60\n\x03` in one write does **not** test this: the interrupt is
    //     already queued when the line is read, so the shell's statement checkpoint fires
    //     before the pipeline ever starts and nothing is ever spawned — it passes without
    //     a stage being involved. Sending the interrupt separately is a race the test can
    //     only lose silently.
    //
    //     So the guest supplies the anchor. Two lines go in one write; `started=1` is
    //     printed by the first, which proves the second is *now running*, and only then is
    //     the interrupt sent. Sixty seconds is far past the 45s expect timeout, so this can
    //     only pass by `sleep` actually being cut short.
    s.send_raw("format(\"started={}\", 1)\nsleep 60\n")?;
    s.expect("started=1")?;
    s.send_raw("\x03")?;
    s.expect("interrupted")?;
    s.expect("/home>")?;
    steps += 1;

    // 19. **`list [x]` returns a prompt instead of locking the shell.** The word-mode
    //     scanner produced an empty token at `[` without advancing, so the argument loop
    //     bumped it forever — a user-reachable hang on a path that predates every part of
    //     Milestone 4, and one with no escape until Ctrl-C exists (§11h). It is asserted
    //     in guest because "the shell is still there" is the whole claim.
    s.send("list [x]")?;
    s.expect("cannot begin a bareword")?;
    s.expect("/home>")?;
    steps += 1;

    // 20. **`exit N` sets the status**, which `session-mgr` logs — so the argument form is
    //     observable rather than merely "the shell left". Before Part C the driver matched
    //     the literal line `exit`, so `exit 3` missed it entirely and came back as
    //     "`exit` is handled by the shell's driver".
    s.send("exit 3")?;
    s.expect("shell exit 3")?;
    s.expect("nitrox login:")?;
    s.send("alice")?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    steps += 1;

    // 21. A bare `exit` still returns to the login prompt, and logging in again works. A
    //     login that cannot be repeated is not a login.
    s.send("exit")?;
    s.expect("nitrox login:")?;
    s.send("alice")?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    steps += 1;

    Ok(steps)
}

/// A driven QEMU serial session: write lines, wait for text.
struct Session {
    child: std::process::Child,
    /// Everything the guest has printed, accumulated by a reader thread.
    out: std::sync::Arc<std::sync::Mutex<String>>,
    /// How far `expect` has already matched, so each step scans only new output and a
    /// pattern cannot be satisfied by an earlier occurrence of itself.
    cursor: usize,
}

impl Session {
    fn spawn(mut cmd: Command) -> R<Session> {
        let mut child = cmd.spawn().map_err(|e| format!("spawn qemu: {e}"))?;
        let stdout = child.stdout.take().ok_or("qemu stdout")?;
        let out = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = out.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut r = stdout;
            let mut buf = [0u8; 1024];
            while let Ok(n) = r.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Ok(mut g) = sink.lock() {
                    g.push_str(&String::from_utf8_lossy(&buf[..n]));
                }
            }
        });
        Ok(Session { child, out, cursor: 0 })
    }

    /// Wait for `pat` in output not yet consumed. The guest paces the test.
    fn expect(&mut self, pat: &str) -> R<()> {
        const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            {
                let g = self.out.lock().map_err(|_| "transcript lock")?;
                if let Some(i) = g[self.cursor..].find(pat) {
                    self.cursor += i + pat.len();
                    println!("  ok: saw {pat:?}");
                    return Ok(());
                }
            }
            if std::time::Instant::now() > deadline {
                return Err(format!("timed out after {TIMEOUT:?} waiting for {pat:?}").into());
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// Type a line, Enter included.
    /// Send bytes with **no trailing newline** — for keys that are not a line.
    ///
    /// `Ctrl-C` is the case that needs it: appending `\n` would submit the line as well as
    /// interrupt, and the two behaviours are exactly what the step is trying to tell apart.
    fn send_raw(&mut self, bytes: &str) -> R<()> {
        use std::io::Write as _;
        let stdin = self.child.stdin.as_mut().ok_or("qemu stdin")?;
        stdin.write_all(bytes.as_bytes()).map_err(|e| format!("write to guest: {e}"))?;
        stdin.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    fn send(&mut self, line: &str) -> R<()> {
        use std::io::Write as _;
        let stdin = self.child.stdin.as_mut().ok_or("qemu stdin")?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .map_err(|e| format!("write to guest: {e}"))?;
        stdin.flush().map_err(|e| format!("flush: {e}"))?;
        Ok(())
    }

    /// Kill the guest and hand back the transcript.
    fn finish(mut self) -> String {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.out.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Integration-test runner: build the `test-harness` image, boot it headless, and
/// adjudicate the run from QEMU's exit code. The guest ends the run by writing a
/// verdict to the `isa-debug-exit` device (init on success/failure, or the kernel
/// panic handler); QEMU then exits `(verdict << 1) | 1`. A hung boot is caught by a
/// wall-clock timeout. See `docs/conventions/qemu-integration-tests.md`.
fn cmd_test_qemu(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::TestHarness)?;
    let ovmf = locate_ovmf()?;

    // Wall-clock ceiling: a hung boot must fail the run, not block CI forever. The
    // healthy self-test boot completes in a few seconds under TCG; 90 s is generous
    // (the demand-paging demo does many emulated-AHCI faults).
    const TIMEOUT_SECS: u32 = 90;
    // isa-debug-exit maps a guest port write `v` to host exit `(v << 1) | 1`: init's
    // PASS verdict (0x10) → 33; FAIL (0x11) → 35; the `timeout` tool uses 124.
    const PASS_EXIT: i32 = (0x10 << 1) | 1; // 33

    let mut cmd = Command::new("timeout");
    // `--foreground` so QEMU still receives the terminate signal when the timeout
    // fires from outside its process group.
    cmd.arg("--foreground").arg(TIMEOUT_SECS.to_string());
    cmd.arg("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel)?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        // The guest ends the run by writing its verdict to this port.
        .arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04")
        // Headless: serial to our captured stdout, no display; `-smp 4` so the SMP
        // distribution/affinity self-tests are meaningful; never reboot on triple-fault.
        .arg("-display")
        .arg("none")
        .arg("-serial")
        .arg("stdio")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot");

    println!("xtask: running integration tests under QEMU (timeout {TIMEOUT_SECS}s)…\n");
    let output = cmd.output()?;
    // Echo the captured serial log so the operator sees the boot + self-test output.
    std::io::stdout().write_all(&output.stdout)?;
    std::io::stderr().write_all(&output.stderr)?;

    match output.status.code() {
        Some(code) if code == PASS_EXIT => {
            println!("\nxtask: integration tests PASSED (qemu exit {code})");
            Ok(())
        }
        Some(124) => Err(format!(
            "integration tests TIMED OUT after {TIMEOUT_SECS}s — no verdict (likely a hang)"
        )
        .into()),
        Some(code) => {
            Err(format!("integration tests FAILED (qemu exit {code}; expected {PASS_EXIT})").into())
        }
        None => Err("qemu terminated by a signal with no exit code".into()),
    }
}

fn cmd_fetch_limine() -> R<PathBuf> {
    let dir = limine_dir();
    let marker = dir.join(".version");
    if marker.exists() {
        if let Ok(v) = fs::read_to_string(&marker) {
            if v.trim() == LIMINE_VERSION {
                return Ok(dir);
            }
        }
    }

    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    fs::create_dir_all(&dir)?;

    let url = format!(
        "https://github.com/limine-bootloader/limine/releases/download/{LIMINE_VERSION}/limine-binary.tar.gz"
    );
    let tarball = build_cache().join("limine-binary.tar.gz");
    fs::create_dir_all(build_cache())?;
    println!("xtask: fetching {url}");
    run(Command::new("curl")
        .arg("-fL")
        .arg("--retry")
        .arg("3")
        .arg("-o")
        .arg(&tarball)
        .arg(&url))?;

    run(Command::new("tar")
        .arg("-xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&dir)
        .arg("--strip-components=1"))?;

    fs::remove_file(&tarball).ok();
    fs::write(&marker, LIMINE_VERSION)?;
    Ok(dir)
}

fn cmd_clean() -> R<()> {
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

fn cmd_test() -> R<()> {
    // Tools workspace tests (xtask itself, image-builder helpers, etc.).
    let tools_manifest = repo_root().join("tools").join("Cargo.toml");
    run(Command::new("cargo")
        .arg("test")
        .arg("--manifest-path")
        .arg(&tools_manifest))?;

    // Kernel host tests. The kernel's `.cargo/config.toml` pins the
    // build target to `x86_64-unknown-none`, which can't link the
    // standard test harness, so we force the host triple here. `--lib`
    // skips the `[[bin]]` (it's `#![no_main]`, unbuildable on host).
    let host = host_triple()?;
    let kernel_dir = repo_root().join("kernel");
    run(Command::new("cargo")
        .arg("test")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&kernel_dir))?;

    // Userspace `libkern` host tests. From the userspace workspace dir libkern
    // builds for the host (it has no per-crate `.cargo/config.toml` pinning the
    // bare target, unlike the demo bins); `-p libkern` skips those bins and the
    // explicit host `--target` mirrors the kernel approach.
    let userspace_dir = repo_root().join("userspace");
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libkern")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libheap` host tests (the freeing allocator engine, exercised through a
    // `std`-backed arena source). A plain lib (no bare-target bin), host-tested like
    // `libkern`; the target `SyscallSource` is `cfg`'d out under `test`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libheap")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libos` host tests (the async core — the `Op` future + `block_on` + error
    // mapping, against a mock syscall seam). A plain lib; the target syscall path is
    // `cfg`'d out under `test`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libos")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libstream` host tests (the TSM1 wire codec — type tags, header/schema/value
    // round-trips, truncation/EOF handling). Pure `core + alloc`, no deps, so it
    // host-tests unchanged like `libcrypto`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libstream")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // init's library tests (the `manifest` + `toml_lite` parsers). `--lib` skips the
    // `#![no_main]` bin, which can't build for the host.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("init")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `librsproto` host tests (the resource-server protocol wire codec). A plain
    // lib (no bare-target bin), host-tested like `libkern`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("librsproto")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `libcrypto` host tests (SHA-256 / HMAC / PBKDF2 against published vectors). A
    // plain `core`-only lib (no bare-target bin), host-tested like `librsproto`.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("libcrypto")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // service-mgr's library tests (the service-declaration parser). `--lib` skips the
    // `#![no_main]` supervisor bin, which can't build for the host.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("service-mgr")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // profile-server's library tests (the profile-manifest parser). `--lib` skips the
    // `#![no_main]` server bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("profile-server")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // auth-service's library tests (the credential logic: user-DB parse + PBKDF2
    // verify + the Auth serve path). `--lib` skips the `#![no_main]` server bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("auth-service")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // logging-service's library tests (the log-path classifier). `--lib` skips the
    // `#![no_main]` server bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("logging-service")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `fs-server-ext4` reader-library tests (the ext4 parser, against an `mke2fs`
    // fixture). `--lib` skips the bare-target server `[[bin]]` (added in Part 4).
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("fs-server-ext4")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `coreutils` shared-library tests (argument parsing). `--lib` skips the
    // bare-target program `[[bin]]`s.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("coreutils")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    // `nxsh` language tests — lexer and parser (Milestone 3 Part A). The whole language
    // is a library with no syscalls in it precisely so it can be tested here, in a
    // second, rather than through a 90-second boot. `--lib` skips the bare-target bin.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("nxsh")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;

    // `tty-server`'s line discipline — the part with all the behaviour and none of the
    // syscalls. Line editing existed three times before this server and the copies
    // disagreed (the `alicepassword:` prompt bug), so the one implementation is tested
    // where testing is cheap.
    run(Command::new("cargo")
        .arg("test")
        .arg("-p")
        .arg("tty-server")
        .arg("--lib")
        .arg("--target")
        .arg(&host)
        .current_dir(&userspace_dir))?;
    Ok(())
}

/// Enforce the architecture-abstraction boundary: kernel code outside
/// `kernel/src/arch/` must reach the arch layer only through the neutral
/// `crate::arch` interface, never `arch::x86_64::…` internals. The private
/// `mod x86_64` already makes such a path a compile error; this lint is the
/// regression net for comments, doc-links, and future re-export slips that
/// the compiler can't catch. See `docs/conventions/arch-boundary.md`.
/// Enforce the **narrowed** stable-Rust rule: userspace pins a nightly toolchain (it
/// must, to `-Z build-std` a custom hard-float target), but no crate anywhere may use a
/// nightly *language or library* feature.
///
/// Without this check the pin would quietly turn into a licence to reach for
/// `#![feature(...)]`, and the project would drift onto nightly for real. See
/// `userspace/rust-toolchain.toml` and the decision log (2026-07-21 floating-point).
fn cmd_check_nightly() -> R<()> {
    let mut violations: Vec<String> = Vec::new();
    // The two workspaces that ship *in* Nitrox. `tools/` is deliberately excluded:
    // xtask is host tooling, explicitly outside the stable-Rust rule (see this file's
    // module docs). `target/` is pruned because build-std rebuilds `core` from a
    // vendored checkout that legitimately uses nightly features.
    for ws in ["kernel/src", "userspace"] {
        let src_root = repo_root().join(ws);
        visit_rs_files_skipping(&src_root, &["target"], &mut |path| {
            let text = fs::read_to_string(path)?;
            for (i, line) in text.lines().enumerate() {
                // Only real code counts; prose may legitimately discuss the attribute.
                let code = line.split("//").next().unwrap_or("");
                if code.contains("#![feature(") {
                    violations.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
                }
            }
            Ok(())
        })?;
    }

    if violations.is_empty() {
        println!("check-nightly: no `#![feature(...)]` — nightly is used only for build-std ✓");
        Ok(())
    } else {
        let mut msg = String::from(
            "nightly language/library features are not permitted — the nightly toolchain \
             exists solely to `-Z build-std` the custom userspace target:\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}

/// Every `TODO(tag)` in the shipping source must have a matching entry in
/// `docs/rationale/deferred-decisions.md`.
///
/// This exists because of a specific, repeated failure: the three deferrals that cost the
/// most to rediscover (exit-time handle reclamation, the wall clock, file truncate) were
/// each recorded *somewhere other than* the canonical list — one in an architecture doc,
/// one as a `TODO` tag in the kernel's syscall table, one in a crate `CLAUDE.md` — so none was
/// ever reviewed, and each surfaced only when a consumer tripped over it (audit,
/// 2026-07-24). A `TODO` is a deferral; this makes the code half of that mechanical.
///
/// The document must name the tag **literally** — `TODO(msi)`, not just the word "msi" —
/// because a bare short tag (`mm`) matches half the prose in any technical document, which
/// would make the check pass without recording anything. Naming it also makes the entry
/// searchable from the code and vice versa.
/// One family of ABI constants mirrored between the kernel and `userspace/libkern`.
///
/// `pattern` is matched per line on both sides; capture group semantics are handled by
/// [`extract_consts`] (name, then value). The two sides use the *same* shape within a
/// family — `pub const SYS_X: u64 = N;` on both, `Name = -N,` enum variants on both — which
/// is what makes a line-based comparison sound here rather than needing a Rust parser.
struct AbiFamily {
    /// Human name for the report.
    what: &'static str,
    kernel_file: &'static str,
    user_file: &'static str,
    /// Which line shape to extract. See [`extract_consts`].
    shape: AbiShape,
    /// Names legitimately present on **one** side only, each with the reason. An
    /// unexplained one-sided name is a finding; a listed one is a documented asymmetry.
    /// Keeping this explicit is what stops the check from being noisy enough to disable.
    one_sided: &'static [(&'static str, &'static str)],
}

#[derive(Copy, Clone, PartialEq)]
enum AbiShape {
    /// `pub const NAME: u64 = <int>;`
    U64Const,
    /// `    Name = <int>,` — an enum variant with an explicit discriminant.
    EnumVariant,
    /// `pub const NAME: Rights = Rights(1 << k);`
    RightsBit,
}

/// The ABI surfaces `userspace/libkern` mirrors by hand, and therefore the ones that can
/// silently drift. Layout of `#[repr(C)]` types is deliberately **not** here: both sides
/// already carry `offset_of!`/`size_of` compile-time asserts, which is a stronger check than
/// text comparison and fails at build time.
const ABI_FAMILIES: &[AbiFamily] = &[
    AbiFamily {
        what: "syscall numbers",
        kernel_file: "kernel/src/syscall/table.rs",
        user_file: "userspace/libkern/src/syscall.rs",
        shape: AbiShape::U64Const,
        one_sided: &[
            ("SYS_TEST_EXIT", "kernel test-harness builds only; userspace mirrors it for the harness"),
        ],
    },
    AbiFamily {
        what: "KError discriminants",
        kernel_file: "kernel/src/syscall/error.rs",
        user_file: "userspace/libkern/src/error.rs",
        shape: AbiShape::EnumVariant,
        one_sided: &[],
    },
    AbiFamily {
        what: "Rights bits",
        kernel_file: "kernel/src/libkern/handle.rs",
        user_file: "userspace/libkern/src/handle.rs",
        shape: AbiShape::RightsBit,
        one_sided: &[],
    },
    AbiFamily {
        what: "KObjectType discriminants",
        kernel_file: "kernel/src/libkern/handle.rs",
        user_file: "userspace/libkern/src/handle.rs",
        shape: AbiShape::EnumVariant,
        one_sided: &[],
    },
];

/// Individually-named constants that mirror across the boundary under *different* names or
/// in unrelated files, so a family sweep cannot pair them. Each is `(kernel file, kernel
/// name, userspace file, userspace name)`.
///
/// These are the ones that bit in practice: `MAX_WAIT_HANDLES` and the IPC limits are
/// hand-copied values with no compile-time tie between the two sides at all.
const ABI_PAIRS: &[(&str, &str, &str, &str)] = &[
    (
        "kernel/src/object/thread.rs",
        "MAX_WAIT_HANDLES",
        "userspace/libkern/src/abi.rs",
        "MAX_WAIT_HANDLES",
    ),
    (
        "kernel/src/libkern/ipc.rs",
        "IPC_HANDLE_MAX",
        "userspace/libkern/src/abi.rs",
        "IPC_HANDLE_MAX",
    ),
];

/// Pull `name -> value` pairs of one `shape` out of a source file.
fn extract_consts(text: &str, shape: AbiShape) -> BTreeMap<String, i128> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        match shape {
            AbiShape::U64Const => {
                // pub const NAME: u64 = <int>;
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                let Some((ty, val)) = tail.split_once('=') else { continue };
                if ty.trim() != "u64" {
                    continue;
                }
                if let Some(v) = parse_int(val) {
                    out.insert(name.trim().to_string(), v);
                }
            }
            AbiShape::EnumVariant => {
                // Name = <int>,
                let Some(body) = t.strip_suffix(',') else { continue };
                let Some((name, val)) = body.split_once('=') else { continue };
                let name = name.trim();
                if name.is_empty()
                    || !name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                    || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    continue;
                }
                if let Some(v) = parse_int(val) {
                    out.insert(name.to_string(), v);
                }
            }
            AbiShape::RightsBit => {
                // pub const NAME: Rights = Rights(1 << k);
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                if !tail.contains("Rights(") {
                    continue;
                }
                let Some((_, shifted)) = tail.split_once("1 <<") else { continue };
                let digits: String =
                    shifted.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(k) = digits.parse::<u32>() {
                    out.insert(name.trim().to_string(), 1i128 << k);
                }
            }
        }
    }
    out
}

/// Parse a trailing integer literal (decimal or `0x`), tolerating `_` and a `;`/`,` tail.
fn parse_int(s: &str) -> Option<i128> {
    let t = s.trim().trim_end_matches([';', ',']).trim().replace('_', "");
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i128::from_str_radix(hex, 16).ok();
    }
    t.parse::<i128>().ok()
}

/// Find `pub const NAME: <ty> = <int>;` for one specific name, whatever the type.
fn extract_named(text: &str, want: &str) -> Option<i128> {
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("pub const ") else { continue };
        let Some((name, tail)) = rest.split_once(':') else { continue };
        if name.trim() != want {
            continue;
        }
        let Some((_, val)) = tail.split_once('=') else { continue };
        return parse_int(val);
    }
    None
}

/// `cargo xtask abi-sync-check` — verify `userspace/libkern` still mirrors the kernel ABI.
///
/// `libkern` is a **hand-maintained** copy of the kernel's syscall numbers, error and object
/// discriminants, rights bits, and shared limits. Nothing in the build ties the two together,
/// so an edit to one side is invisible until something misbehaves at runtime. This compares
/// them and fails on a mismatch, a name the kernel has and userspace lacks, or a one-sided
/// name that is not documented as such.
///
/// Deliberately **not** checked here: `#[repr(C)]` layouts. Both sides already assert their
/// own field offsets and sizes at compile time, which is stronger than text comparison and
/// fails earlier.
fn cmd_abi_sync_check() -> R<()> {
    let root = repo_root();
    let mut problems: Vec<String> = Vec::new();
    let mut compared = 0usize;

    for fam in ABI_FAMILIES {
        let kt = fs::read_to_string(root.join(fam.kernel_file))
            .map_err(|e| format!("read {}: {e}", fam.kernel_file))?;
        let ut = fs::read_to_string(root.join(fam.user_file))
            .map_err(|e| format!("read {}: {e}", fam.user_file))?;
        let k = extract_consts(&kt, fam.shape);
        let u = extract_consts(&ut, fam.shape);
        if k.is_empty() {
            problems.push(format!(
                "{}: extracted nothing from {} — the checker's pattern has gone stale, \
                 which silently disables this family",
                fam.what, fam.kernel_file
            ));
            continue;
        }
        for (name, kv) in &k {
            match u.get(name) {
                Some(uv) if uv == kv => compared += 1,
                Some(uv) => problems.push(format!(
                    "{}: {} is {} in {} but {} in {}",
                    fam.what, name, kv, fam.kernel_file, uv, fam.user_file
                )),
                None => {
                    if !fam.one_sided.iter().any(|(n, _)| n == name) {
                        problems.push(format!(
                            "{}: {} exists in {} but not in {} — mirror it, or record it in \
                             the checker's `one_sided` list with a reason",
                            fam.what, name, fam.kernel_file, fam.user_file
                        ));
                    }
                }
            }
        }
    }

    match check_kerror_decode_table(&root) {
        Ok(n) => compared += n,
        Err(mut found) => problems.append(&mut found),
    }

    for (kf, kn, uf, un) in ABI_PAIRS {
        let kt = fs::read_to_string(root.join(kf)).map_err(|e| format!("read {kf}: {e}"))?;
        let ut = fs::read_to_string(root.join(uf)).map_err(|e| format!("read {uf}: {e}"))?;
        match (extract_named(&kt, kn), extract_named(&ut, un)) {
            (Some(a), Some(b)) if a == b => compared += 1,
            (Some(a), Some(b)) => problems.push(format!(
                "shared limit: {kn} is {a} in {kf} but {un} is {b} in {uf}"
            )),
            (a, _) => problems.push(format!(
                "shared limit: could not read {} — the checker's pattern has gone stale",
                if a.is_none() { format!("{kn} in {kf}") } else { format!("{un} in {uf}") }
            )),
        }
    }

    if !problems.is_empty() {
        let mut msg = String::from(
            "userspace/libkern must mirror the kernel ABI exactly (docs/spec/syscall-abi.md); \
             these disagree:\n",
        );
        for p in &problems {
            msg.push_str("  ");
            msg.push_str(p);
            msg.push('\n');
        }
        return Err(msg.into());
    }
    println!("abi-sync-check: {compared} ABI value(s) agree between the kernel and libkern ✓");
    Ok(())
}

/// Verify `libkern::KError::from_i32` decodes **every** kernel `KError` variant.
///
/// Mirroring the enum is not enough. `from_i32` is a second, hand-written copy of the
/// same table, and its `_ => KernelError` catch-all — deliberate forward-compat, so a
/// newer kernel's error does not panic an older `libkern` — means a *missing* arm is
/// indistinguishable from an unknown code at runtime. `IoError` sat in both enums with
/// matching discriminants and no arm here from 2026-06 to 2026-07-30: every device error
/// silently decoded as `KernelError`, `abi-sync-check` passed, and the round-trip test in
/// `error.rs` missed it because that test enumerates variants by hand too.
///
/// So this derives the expected set from the **kernel's** enum. A variant added to the
/// kernel and mirrored into the userspace enum but forgotten here is now a guard failure,
/// which is the only place in the chain that does not depend on someone remembering.
fn check_kerror_decode_table(root: &Path) -> Result<usize, Vec<String>> {
    const KERNEL: &str = "kernel/src/syscall/error.rs";
    const USER: &str = "userspace/libkern/src/error.rs";

    let read = |p: &str| {
        fs::read_to_string(root.join(p)).map_err(|e| vec![format!("KError decode: read {p}: {e}")])
    };
    let kt = read(KERNEL)?;
    let ut = read(USER)?;

    let kernel = extract_consts(&kt, AbiShape::EnumVariant);
    if kernel.is_empty() {
        return Err(vec![format!(
            "KError decode: extracted no variants from {KERNEL} — the checker's pattern has \
             gone stale, which silently disables this check"
        )]);
    }

    // `    -40 => KError::IoError,` → (-40, "IoError"). The wildcard arm and every other
    // line shape fall out on the integer parse.
    let mut arms: BTreeMap<i128, String> = BTreeMap::new();
    // The variant the `_` arm yields. It needs no explicit arm of its own: an unlisted
    // code already lands on it, including its own. Every *other* variant does.
    let mut catch_all: Option<String> = None;
    for line in ut.lines() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        let Some((lhs, rhs)) = t.split_once("=>") else { continue };
        let Some(name) = rhs.trim().trim_end_matches(',').strip_prefix("KError::") else {
            continue;
        };
        match lhs.trim() {
            "_" => catch_all = Some(name.to_string()),
            n => {
                let Ok(code) = n.parse::<i128>() else { continue };
                arms.insert(code, name.to_string());
            }
        }
    }
    if arms.is_empty() {
        return Err(vec![format!(
            "KError decode: found no `<int> => KError::Name` arms in {USER} — either \
             `from_i32` was rewritten in another shape, or the checker's pattern has gone \
             stale. Either way this check is no longer checking anything"
        )]);
    }

    let mut problems = Vec::new();
    for (name, value) in &kernel {
        if catch_all.as_deref() == Some(name.as_str()) {
            continue;
        }
        match arms.get(value) {
            Some(mapped) if mapped == name => {}
            Some(mapped) => problems.push(format!(
                "KError decode: {USER} decodes {value} as {mapped}, but {KERNEL} defines \
                 {value} as {name}"
            )),
            None => problems.push(format!(
                "KError decode: {KERNEL} defines {name} = {value}, but {USER}'s `from_i32` \
                 has no arm for it — it would decode as KernelError, silently"
            )),
        }
    }
    if problems.is_empty() {
        Ok(kernel.len())
    } else {
        Err(problems)
    }
}

fn cmd_check_deferrals() -> R<()> {
    let doc_path = repo_root()
        .join("docs")
        .join("rationale")
        .join("deferred-decisions.md");
    let doc = fs::read_to_string(&doc_path)
        .map_err(|e| format!("read {}: {e}", doc_path.display()))?
        .to_lowercase();

    let mut violations: Vec<String> = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    for ws in ["kernel/src", "userspace", "tools/xtask/src"] {
        let src_root = repo_root().join(ws);
        visit_rs_files_skipping(&src_root, &["target"], &mut |path| {
            let text = fs::read_to_string(path)?;
            for (i, line) in text.lines().enumerate() {
                let Some(rest) = line.split_once("TODO(") else {
                    continue;
                };
                let Some((tag, _)) = rest.1.split_once(')') else {
                    continue;
                };
                // A tag has to be a plain word to be searchable; anything else is prose
                // that happens to contain the marker.
                if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
                    continue;
                }
                if !tags.iter().any(|t| t == tag) {
                    tags.push(tag.to_string());
                }
                if !doc.contains(&format!("todo({})", tag.to_lowercase())) {
                    violations.push(format!(
                        "{}:{}: TODO({tag}) has no entry in deferred-decisions.md",
                        path.display(),
                        i + 1
                    ));
                }
            }
            Ok(())
        })?;
    }

    if violations.is_empty() {
        println!(
            "check-deferrals: {} TODO tag(s) — every one is recorded in deferred-decisions.md ✓",
            tags.len()
        );
        Ok(())
    } else {
        let mut msg = String::from(
            "every `TODO(tag)` must have a matching entry in \
             docs/rationale/deferred-decisions.md — a deferral only exists if it is in the \
             canonical list (see that document's closing section):\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}

/// `cargo xtask check-irq-scope` — every interrupt entry opens a lock-ordering scope.
///
/// The kernel's lock-rank tracker (`kernel/src/libkern/lockrank.rs`) models interrupt
/// context by having each handler start from an empty view of the held-rank stack, because
/// the acquisition order genuinely restarts at an interrupt boundary — a tick that lands on
/// a thread holding an allocator lock and takes `SCHED` is correct, and reads as an
/// inversion otherwise. That is not a refinement: it is what makes the tracker sound, and
/// getting it wrong is what got the first attempt withdrawn (decision log 2026-07-29).
///
/// A dispatcher that forgets the scope does not fail loudly. It just starts reporting
/// phantom inversions from its own vector, days later, in whoever's boot log. So the scope
/// is not left to the author of the next dispatcher:
///
/// 1. every `dispatch = sym NAME` in a naked entry stub must name a function defined by the
///    `irq_dispatcher!` macro, which opens the scope itself; and
/// 2. that macro must in fact still call `enter_interrupt` — otherwise rule 1 checks that
///    everyone went through a door that no longer leads anywhere.
///
/// Arch-generic: it walks `kernel/src/arch`, so an aarch64 entry path is covered the day it
/// exists rather than the day someone remembers this check.
fn cmd_check_irq_scope() -> R<()> {
    let arch_dir = repo_root().join("kernel").join("src").join("arch");
    // Dispatchers named by a naked stub, as (file:line, name).
    let mut called: Vec<(String, String)> = Vec::new();
    // Dispatchers the macro generated.
    let mut generated: Vec<String> = Vec::new();
    // Dispatchers that are not interrupts and take the ring-3 entry discipline instead
    // (see `assert_user_entry_safe`): the order begins there rather than restarting.
    let mut user_entry: Vec<String> = Vec::new();
    // Files that define the macro, and whether the definition still opens a scope.
    let mut macro_defs: Vec<(String, bool)> = Vec::new();

    visit_rs_files(&arch_dir, &mut |path| {
        let text = fs::read_to_string(path)?;
        let lines: Vec<&str> = text.lines().collect();
        // `irq_dispatcher! {` … `fn NAME(` — the docs sit between the two.
        let mut pending_macro_body = false;
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();

            if let Some(rest) = line.split("dispatch = sym").nth(1) {
                let name: String = rest
                    .trim()
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if !name.is_empty() {
                    called.push((format!("{}:{}", path.display(), i + 1), name));
                }
            }

            if trimmed.starts_with("irq_dispatcher!") {
                pending_macro_body = true;
            } else if pending_macro_body && trimmed.starts_with("fn ") {
                let name: String = trimmed[3..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                generated.push(name);
                pending_macro_body = false;
            }

            // A plain `fn NAME(` whose body asserts the ring-3 entry invariant. Scanned to
            // the next column-0 `}`, which ends a top-level function.
            if let Some(after) = trimmed.strip_prefix("fn ").or_else(|| {
                trimmed
                    .strip_prefix("unsafe extern \"C\" fn ")
                    .or_else(|| trimmed.strip_prefix("extern \"C\" fn "))
            }) {
                let name: String = after
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                let body_end = lines[i..]
                    .iter()
                    .position(|l| *l == "}")
                    .map(|o| i + o)
                    .unwrap_or(lines.len());
                if lines[i..body_end]
                    .iter()
                    .any(|l| l.contains("assert_user_entry_safe"))
                {
                    user_entry.push(name);
                }
            }

            if trimmed.starts_with("macro_rules! irq_dispatcher") {
                // The expansion is short; the scope call is within a few lines.
                let end = (i + 12).min(lines.len());
                let opens = lines[i..end].iter().any(|l| l.contains("enter_interrupt"));
                macro_defs.push((format!("{}:{}", path.display(), i + 1), opens));
            }
        }
        Ok(())
    })?;

    let mut violations: Vec<String> = Vec::new();

    // A check that silently finds nothing to check is worse than no check — it reads as a
    // pass. (The `abi-sync-check` guard, for the same reason.)
    if called.is_empty() {
        violations.push(
            "found no `dispatch = sym …` entry stubs under kernel/src/arch — either the \
             entry path was restructured (update this check) or the scan is broken"
                .to_string(),
        );
    }
    if macro_defs.is_empty() {
        violations.push(
            "found no `macro_rules! irq_dispatcher` definition — the macro every dispatcher \
             is required to go through does not exist"
                .to_string(),
        );
    }
    for (site, opens) in &macro_defs {
        if !opens {
            violations.push(format!(
                "{site}: `irq_dispatcher!` no longer calls `enter_interrupt` — dispatchers \
                 go through it precisely so they get a lock-ordering scope"
            ));
        }
    }
    for (site, name) in &called {
        if !generated.iter().any(|g| g == name) && !user_entry.iter().any(|u| u == name) {
            violations.push(format!(
                "{site}: entry stub dispatches to `{name}`, which is neither defined by \
                 `irq_dispatcher!` nor asserts `assert_user_entry_safe()` — it would run \
                 without a lock-ordering scope"
            ));
        }
    }

    if violations.is_empty() {
        println!(
            "check-irq-scope: {} entry stub(s) → {} scoped dispatcher(s) + {} ring-3 \
             entry point(s) ✓",
            called.len(),
            generated.len(),
            user_entry.len()
        );
        Ok(())
    } else {
        let mut msg = String::from(
            "every interrupt entry must open a lock-ordering scope — define the dispatcher \
             with the `irq_dispatcher!` macro (see kernel/src/libkern/lockrank.rs \
             § Interrupt scopes):\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}

fn cmd_check_arch() -> R<()> {
    let kernel_src = repo_root().join("kernel").join("src");
    let arch_dir = kernel_src.join("arch");
    let mut violations: Vec<String> = Vec::new();

    visit_rs_files(&kernel_src, &mut |path| {
        // The arch implementation legitimately names its own internals.
        if path.starts_with(&arch_dir) {
            return Ok(());
        }
        let text = fs::read_to_string(path)?;
        for (i, line) in text.lines().enumerate() {
            // Ignore comment/doc text — only real code is a boundary break.
            let code = line.split("//").next().unwrap_or("");
            if code.contains("arch::x86_64") || code.contains("arch::aarch64") {
                violations.push(format!("{}:{}: {}", path.display(), i + 1, line.trim()));
            }
        }
        Ok(())
    })?;

    if violations.is_empty() {
        println!("check-arch: no arch-internal references outside kernel/src/arch ✓");
        Ok(())
    } else {
        let mut msg = String::from(
            "arch boundary violated — use the neutral `crate::arch` interface, \
             not arch-internal modules:\n",
        );
        for v in &violations {
            msg.push_str("  ");
            msg.push_str(v);
            msg.push('\n');
        }
        Err(msg.into())
    }
}

/// Recursively visit every `.rs` file under `dir`, calling `f` on each.
/// [`visit_rs_files`], but pruning any directory whose name is in `skip` (e.g. build
/// output trees, which can be enormous and are not project source).
fn visit_rs_files_skipping(
    dir: &Path,
    skip: &[&str],
    f: &mut dyn FnMut(&Path) -> R<()>,
) -> R<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if skip.contains(&name) {
                continue;
            }
            visit_rs_files_skipping(&path, skip, f)?;
        } else if path.extension().map_or(false, |e| e == "rs") {
            f(&path)?;
        }
    }
    Ok(())
}

fn visit_rs_files(dir: &Path, f: &mut dyn FnMut(&Path) -> R<()>) -> R<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            visit_rs_files(&path, f)?;
        } else if path.extension().map_or(false, |e| e == "rs") {
            f(&path)?;
        }
    }
    Ok(())
}

/// Return the host's target triple by parsing `rustc -vV` output.
fn host_triple() -> R<String> {
    let out = Command::new("rustc").arg("-vV").output()?;
    if !out.status.success() {
        return Err(format!("rustc -vV exited {}", out.status).into());
    }
    let text = String::from_utf8(out.stdout)?;
    parse_host_from_rustc_vv(&text)
        .ok_or_else(|| "rustc -vV did not contain a `host:` line".into())
}

/// Find the `host:` line in `rustc -vV` output and return the triple.
fn parse_host_from_rustc_vv(s: &str) -> Option<String> {
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("host:") {
            let triple = rest.trim();
            if !triple.is_empty() {
                return Some(triple.to_string());
            }
        }
    }
    None
}

// --- Image assembly -----------------------------------------------------

fn find_bootx64(limine_root: &Path) -> R<PathBuf> {
    // The tarball layout has varied between versions; search a small set of
    // known locations rather than hard-coding one.
    let candidates = [
        limine_root.join("BOOTX64.EFI"),
        limine_root.join("limine-binary").join("BOOTX64.EFI"),
        limine_root.join("efi").join("x86_64").join("BOOTX64.EFI"),
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    // Fall back to a recursive scan.
    if let Some(found) = walk_for(limine_root, "BOOTX64.EFI")? {
        return Ok(found);
    }
    Err(format!(
        "BOOTX64.EFI not found under {}; tarball layout may have changed",
        limine_root.display()
    )
    .into())
}

fn walk_for(root: &Path, name: &str) -> R<Option<PathBuf>> {
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let p = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            if let Some(found) = walk_for(&p, name)? {
                return Ok(Some(found));
            }
        } else if ft.is_file() && p.file_name().is_some_and(|n| n == name) {
            return Ok(Some(p));
        }
    }
    Ok(None)
}

/// The initramfs payload. Slice 4 ships a placeholder `etc/init.toml` (a single
/// critical-path mount, processed once an fs-server exists in slice 5+);
/// spawnable images move into the initramfs with the spawn-ABI work (slice 7).
const INIT_TOML: &str = "\
# Nitrox init manifest (Phase 2 slice 4 placeholder).\n\
[[mount]]\n\
fs_server = \"fs-server-ext4\"\n\
device = \"gpt-partlabel:nitrox-root\"\n\
mount_point = \"/\"\n\
mode = \"rw\"\n\
required_for = \"boot\"\n";

/// A service declaration for the `heartbeat` demo service, read by `service-mgr` from
/// the initramfs (`/initramfs/etc/services/heartbeat.toml`) in slice A. `executable`
/// is a path per `docs/spec/service-toml-schema.md`, resolved through service-mgr's
/// namespace: `/bin/heartbeat` is projected from the content-addressed store by the
/// profile server (the real userspace path), not the initramfs `/sbin` staging.
const HEARTBEAT_TOML: &str = "\
# Nitrox service declaration (service-mgr slice A demo).\n\
[service.heartbeat]\n\
executable = \"/bin/heartbeat\"\n\
description = \"Demo supervised service (slice A)\"\n\
\n\
[service.heartbeat.restart]\n\
policy = \"always\"\n\
max_attempts = 3\n\
backoff = \"exponential\"\n\
backoff_initial = \"200ms\"\n\
backoff_max = \"2s\"\n";

/// Build path for the packed initramfs CPIO archive.
fn initramfs_path() -> PathBuf {
    build_cache().join("initramfs.cpio")
}

/// Append one CPIO `newc` entry (header + NUL-terminated name + data, each region
/// NUL-padded to a 4-byte boundary) to `out`. Matches `kernel/src/initramfs.rs`.
fn cpio_entry(out: &mut Vec<u8>, ino: u32, name: &str, data: &[u8]) {
    let namesize = name.len() + 1; // includes the trailing NUL
    out.extend_from_slice(b"070701");
    // 13 eight-hex fields: ino, mode, uid, gid, nlink, mtime, filesize,
    // devmajor, devminor, rdevmajor, rdevminor, namesize, check.
    for f in [
        ino, 0o100644, 0, 0, 1, 0, data.len() as u32, 0, 0, 0, 0, namesize as u32, 0,
    ] {
        out.extend_from_slice(format!("{f:08x}").as_bytes());
    }
    out.extend_from_slice(name.as_bytes());
    out.push(0);
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out.extend_from_slice(data);
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

/// The release path of a built userspace binary (bare target).
fn userspace_bin_path(name: &str) -> PathBuf {
    userspace_release_dir().join(name)
}

/// FNV-1a content hash of `bytes` as the store path's opaque `<hash>` (12 hex chars).
/// A non-cryptographic content hash — sufficient as a unique, deterministic directory
/// name for now; the Nix-style build-input hash arrives with the build system. See
/// `docs/architecture/content-addressed-store.md`.
fn store_hash(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{h:016x}")[..12].to_string()
}

/// A store path for a package containing several programs.
///
/// The hash covers **every** ELF in the package, concatenated in the given order, so
/// changing any one of them moves the whole package — which is the property that makes a
/// content-addressed path mean anything. Hashing only the first would let nine of the ten
/// coreutils change under a path that claims they did not.
fn store_path_for_all(bins: &[&str], name: &str, version: &str) -> R<String> {
    let mut bytes = Vec::new();
    for b in bins {
        bytes.extend_from_slice(
            &fs::read(userspace_bin_path(b))
                .map_err(|e| format!("read {b} ELF for store hash: {e}"))?,
        );
    }
    Ok(format!("/store/{}-{}-{}", store_hash(&bytes), name, version))
}

/// The programs a session gets through its profile: the coreutils, plus `nxsh`.
///
/// `nxsh` is here as well as being the login leaf — a user should be able to run a nested
/// shell, and one that cannot invoke itself is a strange thing to hand someone.
///
/// One list, three consumers (the build, the initramfs, the store package), so a new
/// coreutil cannot end up built-but-unreachable or packaged-but-unbuilt.
fn profile_programs() -> Vec<&'static str> {
    let mut v = COREUTILS.to_vec();
    v.push("nxsh");
    v
}

/// Pack the initramfs CPIO `newc` archive at `out`: the config manifests, the `init`
/// ELF (the kernel boot-loads `/sbin/init` from here — retiring the embedded copy),
/// and the mandatory `TRAILER!!!`. Built by `cmd_build` before this runs.
fn build_initramfs(out: &Path, mode: BuildMode) -> R<()> {
    let mut buf = Vec::new();
    cpio_entry(&mut buf, 1, "etc/init.toml", INIT_TOML.as_bytes());
    cpio_entry(&mut buf, 2, "etc/services/heartbeat.toml", HEARTBEAT_TOML.as_bytes());
    // Pack every program ELF at `sbin/<name>`: the kernel boot-loads `/sbin/init`, and
    // the spawners resolve their children by path (`/initramfs/sbin/<name>`), retiring
    // the kernel-embedded `ImageId` images. Built by `cmd_build` before this runs.
    // **Only what is required to get from the bootloader to a mounted root**, plus the two
    // programs that cannot live on the filesystem they depend on:
    //
    // - `init` — the kernel boot-loads it from here.
    // - `fs-server-ext4` — it *is* the mount; nothing else can provide it.
    // - `eshell` — the recovery path *for a failed mount*, so it must not live on the
    //   filesystem it exists to recover from.
    // - `profile-server` — `/bin` does not exist until it runs, so it cannot come from
    //   `/bin`. The alternative (teach `init` to read the manifest and spawn it by store
    //   path) puts TOML parsing in the one process that must not fail; not worth it for
    //   one small binary.
    //
    // Everything else is read from the real filesystem through the store and a profile,
    // like any other program.
    let mut programs = vec!["init", "fs-server-ext4", "eshell", "profile-server"];
    // The integration smoke-test harness is embedded only in selftest/test-harness
    // builds (it is also only built then) — never in a release image.
    if mode.features().is_some() {
        programs.push("test-harness");
        programs.push("test-stage");
    }
    let mut ino = 3u32;
    for name in programs {
        let elf = userspace_bin_path(name);
        let bytes =
            fs::read(&elf).map_err(|e| format!("read {name} ELF {}: {e}", elf.display()))?;
        cpio_entry(&mut buf, ino, &format!("sbin/{name}"), &bytes);
        ino += 1;
    }
    // The system profile manifest — the profile server reads it and projects the listed
    // packages' `bin/` into `/bin`. Generated (not a static const) because it references
    // the store path, whose hash is content-derived at build time (must match the ext4
    // store dir). See `docs/architecture/profiles-and-namespace-projection.md`.
    let sys_store = store_path_for_all(SYSTEM_SERVICES, "system", "0.1.0")?;
    let cu_store = store_path_for_all(&profile_programs(), "coreutils", "0.1.0")?;
    let system_profile = format!(
        "# System profile manifest (generation 1).\n\
         [profile]\n\
         name = \"system\"\n\
         generation = 1\n\
         \n\
         [[package]]\n\
         name = \"system\"\n\
         version = \"0.1.0\"\n\
         path = \"{sys_store}\"\n\
         \n\
         [[package]]\n\
         name = \"coreutils\"\n\
         version = \"0.1.0\"\n\
         path = \"{cu_store}\"\n"
    );
    cpio_entry(&mut buf, ino, "etc/profiles/system.toml", system_profile.as_bytes());
    cpio_entry(&mut buf, 0, "TRAILER!!!", b"");
    fs::write(out, &buf)?;
    println!(
        "xtask: built initramfs ({} bytes) at {}",
        buf.len(),
        out.display()
    );
    Ok(())
}

fn assemble_image(
    bootx64: &Path,
    kernel: &Path,
    conf: &Path,
    initramfs: &Path,
    out: &Path,
) -> R<()> {
    require_tool("sgdisk")?;
    require_tool("mformat")?;
    require_tool("mcopy")?;
    require_tool("mmd")?;
    require_tool("mke2fs")?;

    if out.exists() {
        fs::remove_file(out)?;
    }

    // 1. Allocate the raw disk.
    {
        let f = fs::File::create(out)?;
        f.set_len(IMAGE_SIZE_MIB * 1024 * 1024)?;
    }

    // 2. GPT: an EFI System Partition (FAT32, ESP_SIZE_MIB starting at 1 MiB) and
    //    the ext4 `nitrox-root` filesystem filling the rest. The slice-6 GPT driver
    //    enumerates every non-empty entry (no type-GUID filter) and binds
    //    `/dev/disk/by-partlabel/nitrox-root` at boot — so the second partition
    //    rides the existing boot disk; no separate QEMU drive is needed.
    run(Command::new("sgdisk")
        .arg("--clear")
        .arg("-n").arg(format!("1:2048:+{ESP_SIZE_MIB}M")) // ESP: LBA 2048 (1 MiB), bounded
        .arg("-t").arg("1:ef00")                            // EFI System
        .arg("-c").arg("1:NITROX_ESP")
        .arg("-n").arg("2:0:0")                             // nitrox-root: next aligned → end
        .arg("-t").arg("2:8300")                            // Linux filesystem
        .arg("-c").arg("2:nitrox-root")
        .arg(out))?;

    // Query each partition's on-disk extent (robust to GPT's end-of-disk reserve).
    let (esp_lba, esp_sectors) = partition_extent(out, 1)?;
    let (root_lba, root_sectors) = partition_extent(out, 2)?;

    // A scratch dir for the per-partition images + the ext4 staging tree.
    let work = out.with_extension("partbuild");
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    fs::create_dir_all(&work)?;

    // 3. Build the ESP as a separate, exactly-partition-sized FAT32 image (so the
    //    FAT is bounded to the partition), then splice it in. mformat on a plain
    //    file formats the whole file; no `@@offset` games.
    let esp = work.join("esp.img");
    {
        let f = fs::File::create(&esp)?;
        f.set_len(esp_sectors * 512)?;
    }
    let espf = esp.display().to_string();
    run(Command::new("mformat").arg("-i").arg(&espf).arg("-F").arg("-v").arg("NITROX_ESP"))?;
    run(Command::new("mmd")
        .arg("-i").arg(&espf)
        .arg("::/EFI").arg("::/EFI/BOOT").arg("::/boot").arg("::/boot/limine"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(bootx64).arg("::/EFI/BOOT/BOOTX64.EFI"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(conf).arg("::/boot/limine/limine.conf"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(kernel).arg("::/boot/kernel"))?;
    run(Command::new("mcopy").arg("-i").arg(&espf).arg(initramfs).arg("::/boot/initramfs"))?;
    splice_into(out, esp_lba * 512, &esp)?;

    // 4. Build the ext4 `nitrox-root` filesystem as a separate, partition-sized
    //    image populated at creation (`mke2fs -d`, no root/mount), then splice it
    //    in. The feature set matches the fs-server-ext4 reader's support (the
    //    Part-2 fixture uses the same flags). The staging tree holds the milestone
    //    file the Part-6 init loop reads.
    let staging = work.join("rootfs");
    fs::create_dir_all(staging.join("system"))?;
    // `/scratch` — the backing directory for the **second writable mount**. The kernel
    // calls a rename cross-filesystem when the two paths resolve to a different (server,
    // subtree base) pair, so binding this one server a second time with base `/scratch`
    // yields a destination that is genuinely cross-mount to `/system` while staying
    // writable. That combination did not exist before (2026-07-30): `/initramfs` is
    // cross-mount but read-only, so only the *detection* half of `move`'s fallback could
    // ever run. Empty here; init binds it under `selftest` and the harness populates it.
    fs::create_dir_all(staging.join("scratch"))?;
    fs::write(
        staging.join("system").join("current-generation"),
        b"nitrox-rootfs generation 1\n",
    )?;
    // `system/large.bin` — the slice-8 Part-5 large-file milestone fixture: a file
    // past the old 64 KiB eager cap, spanning several pages, with **position-
    // sensitive** content so init's verifier catches a mis-faulted page. Each byte
    // `i` is `((i >> 12) ^ i) as u8` (the page index in the high part XOR the low
    // offset byte). This generator MUST match init's `fill_byte` /
    // `LARGE_FILE_BYTES` (`userspace/init/src/main.rs`).
    //
    // Sized at 8 pages (was 64): each demand-fault round-trips through the
    // *stateless* fs-server fill (full path/extent re-resolve per page), which
    // costs ~325 ms/page under QEMU's emulated AHCI — 64 pages made boot a ~20 s
    // silent wait. 8 pages still proves multi-page demand-faulting; the per-page
    // cost (kernel read-ahead + an fs-server open-file cookie) is a Phase-3 item.
    // See docs/rationale/deferred-decisions.md.
    const LARGE_FILE_BYTES: usize = 32 * 1024; // 8 pages
    let mut large = vec![0u8; LARGE_FILE_BYTES];
    for (i, b) in large.iter_mut().enumerate() {
        *b = (((i >> 12) ^ i) & 0xFF) as u8;
    }
    fs::write(staging.join("system").join("large.bin"), &large)?;
    // `system/rwtest` — a one-block (4 KiB) writable fixture for the Model A overwrite
    // test (fs-server-rw Part C). Initial content is `byte[i] = i & 0xFF`; init's selftest
    // maps it `MAP_WRITE`, overwrites a marker, `sys_file_sync`s, then re-resolves + reads
    // to confirm the write reached disk.
    let mut rwtest = vec![0u8; 4096];
    for (i, b) in rwtest.iter_mut().enumerate() {
        *b = (i & 0xFF) as u8;
    }
    fs::write(staging.join("system").join("rwtest"), &rwtest)?;
    // `/system/users` — the auth-service credential DB (passwd-style:
    // `name:salt_hex:iterations:verifier_hex:home`). Seeded here so NO plaintext or
    // verifier is committed to the source tree: the stored value is the one-way
    // PBKDF2 of the fixture password, computed with the *same* libcrypto the
    // on-target auth-service verifies with (no drift). The fixture credential is a
    // build input for the emulator demo user, not a secret; init's login selftest
    // (auth Part E) uses the same literals. See docs/architecture/session-and-auth.md.
    {
        use std::fmt::Write as _;
        let iters = libcrypto::password::DEFAULT_ITERATIONS;
        let verifier = libcrypto::password::derive(DEMO_PASSWORD.as_bytes(), &DEMO_SALT, iters);
        let mut users = String::new();
        users.push_str("# Nitrox user database (auth-service).\n");
        users.push_str("# name:salt_hex:iterations:verifier_hex:home\n");
        write!(users, "{DEMO_USER}:").unwrap();
        for b in &DEMO_SALT {
            write!(users, "{b:02x}").unwrap();
        }
        write!(users, ":{iters}:").unwrap();
        for b in &verifier {
            write!(users, "{b:02x}").unwrap();
        }
        writeln!(users, ":{DEMO_HOME}").unwrap();
        fs::write(staging.join("system").join("users"), users.as_bytes())?;
    }
    // The demo user's home directory — the writable session root a login constructs
    // (auth Part E). Empty for now; the user shell writes a file into it.
    fs::create_dir_all(staging.join(DEMO_HOME.trim_start_matches('/')))?;
    println!("xtask: seeded /system/users + {DEMO_HOME}");
    // The content-addressed store, pre-built read-only into the ext4 root. Each package
    // lives at /store/<hash>-<name>-<version>/bin/<prog> — a demand-paged file the profile
    // server projects into /bin. heartbeat is the first package. The store path (hash) is
    // derived from the ELF, matching the initramfs profile manifest. See
    // `docs/architecture/content-addressed-store.md`.
    // The `coreutils` package: the programs a shell can actually run. One package rather
    // than one per binary — they version and ship together, and ten manifest entries would
    // claim an independence they do not have.
    //
    // Until this existed, the coreutils lived *only* in the initramfs, so a session could
    // not reach them at all without being handed the whole boot image. Since 2026-08-03
    // the boot image no longer carries them at all.
    let programs = profile_programs();
    let cu_store = store_path_for_all(&programs, "coreutils", "0.1.0")?;
    let cu_bin = staging.join(cu_store.trim_start_matches('/')).join("bin");
    fs::create_dir_all(&cu_bin)?;
    for prog in &programs {
        fs::copy(userspace_bin_path(prog), cu_bin.join(prog))
            .map_err(|e| format!("stage {prog} into the store: {e}"))?;
    }
    println!(
        "xtask: store package {cu_store}/bin/ ({} programs)",
        programs.len()
    );

    // The `system` package: the services. Everything init and service-mgr spawn after the
    // root is mounted now resolves through `/bin` like any other program.
    let sys_store = store_path_for_all(SYSTEM_SERVICES, "system", "0.1.0")?;
    let sys_bin = staging.join(sys_store.trim_start_matches('/')).join("bin");
    fs::create_dir_all(&sys_bin)?;
    for prog in SYSTEM_SERVICES {
        fs::copy(userspace_bin_path(prog), sys_bin.join(prog))
            .map_err(|e| format!("stage {prog} into the store: {e}"))?;
    }
    println!(
        "xtask: store package {sys_store}/bin/ ({} services)",
        SYSTEM_SERVICES.len()
    );
    let rootfs = work.join("rootfs.ext4");
    let blocks = (root_sectors * 512) / 4096; // 4 KiB block count
    run(Command::new("mke2fs")
        .arg("-q").arg("-F").arg("-t").arg("ext4")
        .arg("-O").arg("^has_journal,^64bit,^metadata_csum,^resize_inode")
        .arg("-b").arg("4096")
        .arg("-d").arg(&staging)
        .arg(&rootfs)
        .arg(blocks.to_string()))?;
    splice_into(out, root_lba * 512, &rootfs)?;

    // Leave `work/` in place for inspection; `cmd_image` rebuilds it each run.
    Ok(())
}

/// Parse `sgdisk -i <n> <disk>` for partition `n`'s first LBA and sector count.
fn partition_extent(disk: &Path, n: u32) -> R<(u64, u64)> {
    let out = Command::new("sgdisk")
        .arg("-i").arg(n.to_string()).arg(disk)
        .output()
        .map_err(|e| format!("failed to run sgdisk -i {n}: {e}"))?;
    if !out.status.success() {
        return Err(format!("sgdisk -i {n} {} failed", disk.display()).into());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut first = None;
    let mut last = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("First sector:") {
            first = rest.split_whitespace().next().and_then(|s| s.parse::<u64>().ok());
        } else if let Some(rest) = line.strip_prefix("Last sector:") {
            last = rest.split_whitespace().next().and_then(|s| s.parse::<u64>().ok());
        }
    }
    let first = first.ok_or("sgdisk: missing 'First sector'")?;
    let last = last.ok_or("sgdisk: missing 'Last sector'")?;
    Ok((first, last - first + 1))
}

/// Overwrite `dst` (in place, no truncation) with `src`'s bytes starting at byte
/// `offset` — splice a partition image into the GPT disk.
fn splice_into(dst: &Path, offset: u64, src: &Path) -> R<()> {
    use std::io::{Seek, SeekFrom, Write};
    let data = fs::read(src)?;
    let mut f = fs::OpenOptions::new().write(true).open(dst)?;
    f.seek(SeekFrom::Start(offset))?;
    f.write_all(&data)?;
    Ok(())
}

/// UEFI firmware for the QEMU pflash. Modern QEMU ships **split** firmware — a
/// read-only CODE image plus a writable VARS (NVRAM) store — and a CODE-only
/// pflash will not boot (the firmware needs its variable region). Older setups
/// shipped a single combined image used as one read-only pflash.
enum Firmware {
    /// Legacy single combined image (e.g. `OVMF.fd`): one read-only pflash.
    Single(PathBuf),
    /// Split firmware: a read-only CODE image plus a VARS *template* that is
    /// copied to a writable per-run store before boot.
    Split { code: PathBuf, vars_template: PathBuf },
}

/// Locate UEFI firmware, preferring the modern split (CODE+VARS) layout that
/// QEMU bundles under its data dir. `NITROX_OVMF` overrides the CODE/combined
/// image; pair it with `NITROX_OVMF_VARS` to force the split layout.
fn locate_ovmf() -> R<Firmware> {
    if let Ok(code) = env::var("NITROX_OVMF") {
        let code = PathBuf::from(code);
        if code.exists() {
            if let Ok(vars) = env::var("NITROX_OVMF_VARS") {
                let vars = PathBuf::from(vars);
                if vars.exists() {
                    return Ok(Firmware::Split { code, vars_template: vars });
                }
            }
            return Ok(Firmware::Single(code));
        }
    }
    // Split CODE+VARS pairs. QEMU's x86_64 CODE pairs with the (historically
    // i386-named) VARS template; the `/usr/local` paths are a from-source/tarball
    // QEMU install's bundled edk2 firmware.
    let split = [
        (
            "/usr/local/share/qemu/edk2-x86_64-code.fd",
            "/usr/local/share/qemu/edk2-i386-vars.fd",
        ),
        (
            "/usr/share/qemu/edk2-x86_64-code.fd",
            "/usr/share/qemu/edk2-i386-vars.fd",
        ),
        // Debian/Ubuntu's `ovmf` package has shipped the 4 MB build under these names
        // since 22.04, and on current releases they are the *only* ones present — the
        // unsuffixed pair below is older layouts (and the `/usr/share/ovmf/OVMF.fd`
        // single image at the end is older still). Ordered newest-first so a machine
        // with both prefers the split pair, which gives a writable VARS store.
        (
            "/usr/share/OVMF/OVMF_CODE_4M.fd",
            "/usr/share/OVMF/OVMF_VARS_4M.fd",
        ),
        (
            "/usr/share/OVMF/OVMF_CODE.fd",
            "/usr/share/OVMF/OVMF_VARS.fd",
        ),
        (
            "/usr/share/edk2-ovmf/x64/OVMF_CODE.fd",
            "/usr/share/edk2-ovmf/x64/OVMF_VARS.fd",
        ),
    ];
    for (code, vars) in split {
        if Path::new(code).exists() && Path::new(vars).exists() {
            return Ok(Firmware::Split {
                code: PathBuf::from(code),
                vars_template: PathBuf::from(vars),
            });
        }
    }
    // Legacy single combined image.
    let single = [
        "/usr/share/ovmf/OVMF.fd",
        "/usr/share/qemu/OVMF.fd",
        "/usr/share/edk2-ovmf/x64/OVMF.fd",
    ];
    for c in single {
        if Path::new(c).exists() {
            return Ok(Firmware::Single(PathBuf::from(c)));
        }
    }
    Err("could not locate UEFI firmware; set NITROX_OVMF=/path/to/CODE.fd \
         (and NITROX_OVMF_VARS=/path/to/VARS.fd for split firmware)"
        .into())
}

/// The `-drive if=pflash,…` argument(s) for `firmware`. For split firmware the
/// read-only VARS template is copied to a fresh writable per-run store under
/// build-cache (UEFI needs a writable NVRAM region; the shared template is
/// read-only), emitted as `unit=0` CODE (ro) + `unit=1` VARS (rw).
fn firmware_pflash_args(firmware: &Firmware) -> R<Vec<String>> {
    match firmware {
        Firmware::Single(code) => Ok(vec![
            "-drive".into(),
            format!("if=pflash,format=raw,readonly=on,file={}", code.display()),
        ]),
        Firmware::Split { code, vars_template } => {
            let vars = build_cache().join("ovmf-vars.fd");
            fs::copy(vars_template, &vars).map_err(|e| {
                format!(
                    "copy OVMF vars {} -> {}: {e}",
                    vars_template.display(),
                    vars.display()
                )
            })?;
            Ok(vec![
                "-drive".into(),
                format!("if=pflash,unit=0,format=raw,readonly=on,file={}", code.display()),
                "-drive".into(),
                format!("if=pflash,unit=1,format=raw,file={}", vars.display()),
            ])
        }
    }
}

// --- Helpers ------------------------------------------------------------

fn require_tool(name: &str) -> R<()> {
    let status = Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        _ => Err(format!(
            "required host tool `{name}` is missing — install it and retry"
        )
        .into()),
    }
}

fn run(cmd: &mut Command) -> R<()> {
    let pretty = format_cmd(cmd);
    let status = cmd.status().map_err(|e| format!("failed to spawn {pretty}: {e}"))?;
    if !status.success() {
        return Err(format!("command failed ({status}): {pretty}").into());
    }
    Ok(())
}

fn format_cmd(cmd: &Command) -> String {
    let mut s = cmd.get_program().to_string_lossy().into_owned();
    for arg in cmd.get_args() {
        s.push(' ');
        s.push_str(&arg.to_string_lossy());
    }
    s
}

// --- Tests --------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Per-test unique tmp dir. We avoid `tempfile` to keep xtask
    /// dependency-free, so we have to clean up manually.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new(tag: &str) -> Self {
            let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = env::temp_dir().join(format!(
                "nitrox-xtask-{}-{}-{}",
                tag,
                std::process::id(),
                n
            ));
            if path.exists() {
                fs::remove_dir_all(&path).expect("clear stale tmp");
            }
            fs::create_dir_all(&path).expect("create tmp");
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn touch(p: &Path) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).expect("mkdir -p");
        }
        fs::write(p, b"").expect("touch");
    }

    #[test]
    fn walk_for_finds_nested_file() {
        let tmp = TmpDir::new("walk-nested");
        touch(&tmp.path().join("a/b/c/target.bin"));
        let found = walk_for(tmp.path(), "target.bin").unwrap();
        let found = found.expect("walk_for should locate target.bin");
        assert_eq!(found.file_name().unwrap(), "target.bin");
    }

    #[test]
    fn walk_for_returns_none_when_missing() {
        let tmp = TmpDir::new("walk-missing");
        fs::create_dir_all(tmp.path().join("a")).unwrap();
        assert!(walk_for(tmp.path(), "nope.efi").unwrap().is_none());
    }

    #[test]
    fn find_bootx64_uses_known_location() {
        let tmp = TmpDir::new("bootx64-known");
        // Limine v12 layout: efi/x86_64/BOOTX64.EFI
        let expected = tmp.path().join("efi/x86_64/BOOTX64.EFI");
        touch(&expected);
        // Decoy that should be ignored because the known location wins.
        touch(&tmp.path().join("somewhere/else/BOOTX64.EFI"));
        let found = find_bootx64(tmp.path()).unwrap();
        assert_eq!(found, expected);
    }

    #[test]
    fn find_bootx64_falls_back_to_recursive_scan() {
        let tmp = TmpDir::new("bootx64-fallback");
        let weird = tmp.path().join("unexpected/depth/BOOTX64.EFI");
        touch(&weird);
        let found = find_bootx64(tmp.path()).unwrap();
        assert!(found.ends_with("BOOTX64.EFI"));
    }

    #[test]
    fn find_bootx64_errors_when_absent() {
        let tmp = TmpDir::new("bootx64-absent");
        fs::create_dir_all(tmp.path().join("efi")).unwrap();
        assert!(find_bootx64(tmp.path()).is_err());
    }

    #[test]
    fn format_cmd_includes_program_and_args() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello").arg("world");
        assert_eq!(format_cmd(&cmd), "echo hello world");
    }

    #[test]
    fn format_cmd_handles_no_args() {
        let cmd = Command::new("true");
        assert_eq!(format_cmd(&cmd), "true");
    }

    #[test]
    fn parse_host_extracts_linux_triple() {
        let sample = "\
rustc 1.95.0 (59807616e 2026-04-14)
binary: rustc
commit-hash: 59807616e1fa2540724bfbac14d7976d7e4a3860
commit-date: 2026-04-14
host: x86_64-unknown-linux-gnu
release: 1.95.0
LLVM version: 22.1.2
";
        assert_eq!(
            parse_host_from_rustc_vv(sample).as_deref(),
            Some("x86_64-unknown-linux-gnu")
        );
    }

    #[test]
    fn parse_host_extracts_macos_triple() {
        let sample = "rustc 1.95.0\nhost: aarch64-apple-darwin\n";
        assert_eq!(
            parse_host_from_rustc_vv(sample).as_deref(),
            Some("aarch64-apple-darwin")
        );
    }

    #[test]
    fn parse_host_returns_none_when_absent() {
        let sample = "rustc 1.95.0\nrelease: 1.95.0\n";
        assert!(parse_host_from_rustc_vv(sample).is_none());
    }
}

