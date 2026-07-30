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

/// Disk image size in MiB. 64 is enough for the kernel + Limine UEFI
/// loader several times over.
/// Total boot-disk size. Holds two GPT partitions: the EFI System Partition
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
    build_userspace_crate("coreutils", &["list", "copy", "mkdir", "remove", "rename", "move"], None)?;
    build_userspace_bin("profile-server", None)?;
    build_userspace_bin("logging-service", None)?;
    build_userspace_bin("auth-service", None)?;
    // session-mgr fires the self-test verdict, so it takes the build-mode feature
    // (`selftest`/`test-harness`) like init.
    build_userspace_bin("session-mgr", mode.features())?;
    // usersh (the throwaway user shell) exits with its home-write verdict under
    // test-harness, so it also takes the build-mode feature.
    build_userspace_bin("usersh", mode.features())?;

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

/// The store path `/store/<hash>-<name>-<version>` for a built program `bin`, keyed on
/// its ELF's content hash. A pure function of the ELF, so the ext4 store build and the
/// initramfs profile manifest derive the same path independently — no value threaded.
fn store_path_for(bin: &str, name: &str, version: &str) -> R<String> {
    let bytes = fs::read(userspace_bin_path(bin))
        .map_err(|e| format!("read {bin} ELF for store hash: {e}"))?;
    Ok(format!("/store/{}-{}-{}", store_hash(&bytes), name, version))
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
    let mut programs = vec![
        "init",
        "service-mgr",
        "heartbeat",
        "fs-server-ext4",
        "eshell",
        "profile-server",
        "logging-service",
        "auth-service",
        "session-mgr",
        "usersh",
        "list",
        "copy",
        "mkdir",
        "remove",
        "rename",
        "move",
    ];
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
    let hb_store = store_path_for("heartbeat", "heartbeat", "0.1.0")?;
    let system_profile = format!(
        "# System profile manifest (generation 1).\n\
         [profile]\n\
         name = \"system\"\n\
         generation = 1\n\
         \n\
         [[package]]\n\
         name = \"heartbeat\"\n\
         version = \"0.1.0\"\n\
         path = \"{hb_store}\"\n"
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
    let hb_store = store_path_for("heartbeat", "heartbeat", "0.1.0")?;
    let hb_bin = staging.join(hb_store.trim_start_matches('/')).join("bin");
    fs::create_dir_all(&hb_bin)?;
    fs::copy(userspace_bin_path("heartbeat"), hb_bin.join("heartbeat"))?;
    println!("xtask: store package {hb_store}/bin/heartbeat");
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

