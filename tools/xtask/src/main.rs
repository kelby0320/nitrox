//! Nitrox build orchestrator.
//!
//! Subcommands:
//!   build           build the kernel ELF
//!   image           build kernel + assemble a UEFI-bootable GPT/FAT32 image
//!   qemu            build + launch QEMU with OVMF
//!   qemu-debug      build + launch QEMU paused for GDB on :1234
//!   test            host-side unit tests (kernel lib + tools workspace)
//!   test-qemu       boot a headless self-test image; adjudicate via isa-debug-exit
//!   fetch-limine    download the pinned limine-binary tarball into the cache
//!   clean           remove all build outputs and caches
//!
//! Stays on std and avoids external crates so the host build can be a
//! single `cargo run -p xtask`. No "stable Rust only" rule applies here
//! the way it does to the kernel; this is host tooling.

use std::env;
use std::error::Error;
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
    let qargs: Vec<String> = rest.iter().filter(|a| *a != "--selftest").cloned().collect();
    let mode = if selftest {
        BuildMode::Selftest
    } else {
        BuildMode::Normal
    };

    let result = match cmd.as_deref() {
        Some("build") => cmd_build(mode),
        Some("image") => cmd_image(mode),
        Some("qemu") => cmd_qemu(false, mode, &qargs),
        Some("qemu-debug") => cmd_qemu(true, mode, &qargs),
        Some("test") => cmd_test(),
        Some("test-qemu") => cmd_test_qemu(),
        Some("check-arch") => cmd_check_arch(),
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
           fetch-limine  download the pinned Limine binary tarball\n  \
           clean         remove build outputs and caches\n  \
           help          show this message\n\
         \n\
         `--selftest` (build/image/qemu) compiles + runs the boot self-tests / demos;\n         \
         without it the build boots straight to userspace.\n         \
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
    build_userspace_bin("parent", None)?;
    build_userspace_bin("child", None)?;
    build_userspace_bin("init", mode.features())?;
    build_userspace_bin("fs-server-ext4", None)?;
    build_userspace_bin("eshell", None)?;
    build_userspace_bin("service-mgr", None)?;
    build_userspace_bin("heartbeat", None)?;

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

/// Path to the built `hello` userspace ELF (release; the kernel embeds this).
fn hello_elf() -> PathBuf {
    repo_root()
        .join("userspace/target/x86_64-unknown-none/release/hello")
}

/// Build the `hello` userspace program as a static `ET_EXEC` for the bare
/// target. Run from `userspace/hello` so that crate's `.cargo/config.toml`
/// (target + non-PIE/static rustflags) applies without affecting the other
/// userspace members.
fn cmd_build_hello() -> R<()> {
    let hello_dir = repo_root().join("userspace").join("hello");
    run(Command::new("cargo")
        .arg("build")
        .arg("--release")
        .arg("--target")
        .arg("x86_64-unknown-none")
        .current_dir(&hello_dir))?;
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
    let dir = repo_root().join("userspace").join(name);
    let mut c = Command::new("cargo");
    c.arg("build")
        .arg("--release")
        .arg("--target")
        .arg("x86_64-unknown-none");
    if let Some(f) = features {
        c.arg("--features").arg(f);
    }
    run(c.current_dir(&dir))?;
    let elf = repo_root()
        .join("userspace/target/x86_64-unknown-none/release")
        .join(name);
    if !elf.exists() {
        return Err(format!("{name} ELF missing after build: {}", elf.display()).into());
    }
    println!("xtask: built {name} ELF at {}", elf.display());
    Ok(())
}

fn cmd_image(mode: BuildMode) -> R<()> {
    cmd_build(mode)?;
    let limine_root = cmd_fetch_limine()?;
    let bootx64 = find_bootx64(&limine_root)?;
    let initramfs = initramfs_path();
    build_initramfs(&initramfs)?;
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
fn qemu_base_args(qemu: &mut Command, ovmf: &Firmware) -> R<()> {
    qemu.arg("-M")
        .arg("q35")
        // CPU model = "the features the kernel actually requires,
        // nothing more". Base `qemu64` brings long mode, NX, and basic
        // SSE; the `+smap,+smep` opt-ins give us the user-access
        // protections `arch::init_protections` asserts on. The on-chip APIC
        // (CPUID.01H:EDX.9) is present in `qemu64` by default. The entropy slice
        // opts in `+rdrand,+rdseed` so the boot CSPRNG seeds from the hardware
        // source (TCG emulates both); without them the kernel falls back to
        // jitter-only seeding, which is correct but leaves `seeded=false` at boot.
        // The Phase-3 per-CPU substrate opts in `+rdtscp`: `current_cpu()` reads
        // the logical CPU id from `IA32_TSC_AUX` via `RDTSCP`. The Phase-3 SMP slice
        // brings the local APIC up in **x2APIC** mode and opts in `+x2apic`; TCG
        // only emulates x2APIC from **QEMU 9.0**, so the dev-loop QEMU floor is now
        // ≥ 9.0. Both RDTSCP and x2APIC are universal on the project's ≈2014
        // hardware baseline but are not in the bare `qemu64` model's defaults — the
        // kernel `#UD`s / `#GP`s on them without these flags. SMP runs `-smp N`.
        .arg("-cpu")
        .arg("qemu64,+smap,+smep,+rdrand,+rdseed,+rdtscp,+x2apic")
        .arg("-m")
        .arg("256M");
    // UEFI firmware pflash drive(s) — split CODE+VARS on modern QEMU, or a
    // single combined image on legacy setups (see `locate_ovmf`).
    for a in firmware_pflash_args(ovmf)? {
        qemu.arg(a);
    }
    Ok(())
}

fn cmd_qemu(debug: bool, mode: BuildMode, extra_args: &[String]) -> R<()> {
    cmd_image(mode)?;
    let ovmf = locate_ovmf()?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut qemu, &ovmf)?;
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
fn cmd_test_qemu() -> R<()> {
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
    qemu_base_args(&mut cmd, &ovmf)?;
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
    Ok(())
}

/// Enforce the architecture-abstraction boundary: kernel code outside
/// `kernel/src/arch/` must reach the arch layer only through the neutral
/// `crate::arch` interface, never `arch::x86_64::…` internals. The private
/// `mod x86_64` already makes such a path a compile error; this lint is the
/// regression net for comments, doc-links, and future re-export slips that
/// the compiler can't catch. See `docs/conventions/arch-boundary.md`.
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

/// Pack the initramfs CPIO `newc` archive at `out` (a placeholder `etc/init.toml`
/// + the mandatory `TRAILER!!!`).
fn build_initramfs(out: &Path) -> R<()> {
    let mut buf = Vec::new();
    cpio_entry(&mut buf, 1, "etc/init.toml", INIT_TOML.as_bytes());
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

