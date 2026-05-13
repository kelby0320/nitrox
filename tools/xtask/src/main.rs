//! Nitrox build orchestrator.
//!
//! Subcommands:
//!   build           build the kernel ELF
//!   image           build kernel + assemble a UEFI-bootable GPT/FAT32 image
//!   qemu            build + launch QEMU with OVMF
//!   qemu-debug      build + launch QEMU paused for GDB on :1234
//!   fetch-limine    download the pinned limine-binary tarball into the cache
//!   clean           remove all build outputs and caches
//!
//! Stays on std and avoids external crates so the host build can be a
//! single `cargo run -p xtask`. No "stable Rust only" rule applies here
//! the way it does to the kernel; this is host tooling.

use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// Limine version we build against. Bump this together with any changes
/// to `kernel/src/limine.rs`.
const LIMINE_VERSION: &str = "v12.2.0";

/// Disk image size in MiB. 64 is enough for the kernel + Limine UEFI
/// loader several times over.
const IMAGE_SIZE_MIB: u64 = 64;

type R<T> = Result<T, Box<dyn Error>>;

fn main() -> ExitCode {
    let mut args = env::args().skip(1);
    let cmd = args.next();
    let rest: Vec<String> = args.collect();

    let result = match cmd.as_deref() {
        Some("build") => cmd_build(),
        Some("image") => cmd_image(),
        Some("qemu") => cmd_qemu(false, &rest),
        Some("qemu-debug") => cmd_qemu(true, &rest),
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
           fetch-limine  download the pinned Limine binary tarball\n  \
           clean         remove build outputs and caches\n  \
           help          show this message\n\
         \n\
         Any args after `qemu` / `qemu-debug` are forwarded to QEMU.\n"
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

fn cmd_build() -> R<()> {
    let kernel_dir = repo_root().join("kernel");
    run(Command::new("cargo").arg("build").current_dir(&kernel_dir))?;
    let elf = kernel_elf();
    if !elf.exists() {
        return Err(format!("kernel ELF missing after build: {}", elf.display()).into());
    }
    println!("xtask: built kernel ELF at {}", elf.display());
    Ok(())
}

fn cmd_image() -> R<()> {
    cmd_build()?;
    let limine_root = cmd_fetch_limine()?;
    let bootx64 = find_bootx64(&limine_root)?;
    assemble_image(&bootx64, &kernel_elf(), &limine_conf(), &image_path())?;
    println!("xtask: image at {}", image_path().display());
    Ok(())
}

fn cmd_qemu(debug: bool, extra_args: &[String]) -> R<()> {
    cmd_image()?;
    let ovmf = locate_ovmf()?;
    let mut qemu = Command::new("qemu-system-x86_64");
    qemu.arg("-M")
        .arg("q35")
        .arg("-m")
        .arg("256M")
        .arg("-drive")
        .arg(format!("if=pflash,format=raw,readonly=on,file={}", ovmf.display()))
        .arg("-drive")
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
    let cache = build_cache();
    if cache.exists() {
        fs::remove_dir_all(&cache)?;
        println!("xtask: removed {}", cache.display());
    }
    Ok(())
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

fn assemble_image(
    bootx64: &Path,
    kernel: &Path,
    conf: &Path,
    out: &Path,
) -> R<()> {
    require_tool("sgdisk")?;
    require_tool("mformat")?;
    require_tool("mcopy")?;
    require_tool("mmd")?;

    if out.exists() {
        fs::remove_file(out)?;
    }

    // 1. Allocate the raw disk.
    {
        let f = fs::File::create(out)?;
        f.set_len(IMAGE_SIZE_MIB * 1024 * 1024)?;
    }

    // 2. GPT layout with a single EFI System Partition starting at 1 MiB.
    run(Command::new("sgdisk")
        .arg("--clear")
        .arg("-n").arg("1:2048")        // partition 1, start LBA 2048 (1 MiB)
        .arg("-t").arg("1:ef00")        // EFI System
        .arg("-c").arg("1:NITROX_ESP")
        .arg(out))?;

    // 3. FAT32 inside the partition (mformat's @@1M syntax = 1 MiB offset).
    let part = format!("{}@@1M", out.display());
    run(Command::new("mformat")
        .arg("-i").arg(&part)
        .arg("-F")
        .arg("-v").arg("NITROX_ESP"))?;

    // 4. Directory tree and file copies.
    run(Command::new("mmd")
        .arg("-i").arg(&part)
        .arg("::/EFI")
        .arg("::/EFI/BOOT")
        .arg("::/boot")
        .arg("::/boot/limine"))?;

    run(Command::new("mcopy")
        .arg("-i").arg(&part)
        .arg(bootx64)
        .arg("::/EFI/BOOT/BOOTX64.EFI"))?;

    run(Command::new("mcopy")
        .arg("-i").arg(&part)
        .arg(conf)
        .arg("::/boot/limine/limine.conf"))?;

    run(Command::new("mcopy")
        .arg("-i").arg(&part)
        .arg(kernel)
        .arg("::/boot/kernel"))?;

    Ok(())
}

fn locate_ovmf() -> R<PathBuf> {
    if let Ok(p) = env::var("NITROX_OVMF") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Ok(path);
        }
    }
    let candidates = [
        "/usr/share/ovmf/OVMF.fd",
        "/usr/share/OVMF/OVMF_CODE.fd",
        "/usr/share/qemu/OVMF.fd",
        "/usr/share/edk2-ovmf/x64/OVMF.fd",
    ];
    for c in candidates {
        let p = PathBuf::from(c);
        if p.exists() {
            return Ok(p);
        }
    }
    Err("could not locate an OVMF firmware image; set NITROX_OVMF=/path/to/OVMF.fd".into())
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

