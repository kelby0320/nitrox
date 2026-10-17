//! `image --live`: the live image, its root a RAM-disk module, and its boot menu.

use crate::*;

/// `cargo xtask image --live`: the release kernel and initramfs, with the release root riding along
/// as a second Limine module.
pub(crate) fn cmd_image_live() -> R<()> {
    cmd_image_live_for(BuildMode::Normal)
}

/// The live image built in `mode`: the release one, or with `--selftest` the **test live image**,
/// whose root carries the test packages. `check-images` holds each to its own ordinary image.
pub(crate) fn cmd_image_live_for(mode: BuildMode) -> R<()> {
    cmd_build(mode)?;
    let limine_root = cmd_fetch_limine()?;
    let bootx64 = find_bootx64(&limine_root)?;
    let (initramfs, out) = if mode.stages_test_data() {
        (live_test_initramfs_path(), live_test_image_path())
    } else {
        (live_initramfs_path(), live_image_path())
    };
    build_initramfs_for(&initramfs, mode, RootDevice::Live)?;
    assemble_live_image(&bootx64, &kernel_elf(), &initramfs, &out, mode)?;
    println!("xtask: live image at {}", out.display());
    Ok(())
}

/// Assemble the live image at `out`: a GPT disk with one ESP holding everything, `root.img` (a GPT
/// image with one `nitrox-live` partition, built from [`stage_rootfs`]) included.
fn assemble_live_image(bootx64: &Path, kernel: &Path, initramfs: &Path, out: &Path, mode: BuildMode) -> R<()> {
    const MIB: u64 = 1024 * 1024;
    require_tool("sgdisk")?;
    require_tool("mformat")?;
    require_tool("mcopy")?;
    require_tool("mmd")?;
    require_tool("mke2fs")?;

    let work = out.with_extension("partbuild");
    if work.exists() {
        fs::remove_dir_all(&work)?;
    }
    fs::create_dir_all(&work)?;

    let staging = work.join("rootfs");
    stage_rootfs(&staging, mode)?;
    let staged = tree_bytes(&staging)?;
    let part_mib = staged.div_ceil(MIB) + LIVE_ROOT_SLACK_MIB;
    let root_img_mib = part_mib + 2; // a MiB before the partition, room for the backup GPT after
    if root_img_mib > LIVE_ROOT_MAX_MIB {
        return Err(format!(
            "root.img would be {root_img_mib} MiB, over the {LIVE_ROOT_MAX_MIB} MiB ceiling — the \
             root stages {staged} bytes. The firmware reads all of it off a USB stick before the \
             kernel runs; see LIVE_ROOT_MAX_MIB before raising it."
        )
        .into());
    }
    let root_img = work.join("root.img");
    fs::File::create(&root_img)?.set_len(root_img_mib * MIB)?;
    run(Command::new("sgdisk")
        .arg("--clear")
        .arg("-n")
        .arg("1:2048:0")
        .arg("-t")
        .arg("1:8300")
        .arg("-c")
        .arg(format!("1:{LIVE_ROOT_PARTLABEL}"))
        .arg(&root_img))?;
    let (root_lba, root_sectors) = partition_extent(&root_img, 1)?;
    let rootfs = work.join("rootfs.ext4");
    run(Command::new("mke2fs")
        .arg("-q")
        .arg("-F")
        .arg("-t")
        .arg("ext4")
        .arg("-O")
        .arg("^has_journal,^64bit,^metadata_csum,^resize_inode")
        .arg("-b")
        .arg("4096")
        .arg("-d")
        .arg(&staging)
        .arg(&rootfs)
        .arg(((root_sectors * 512) / 4096).to_string()))?;
    splice_into(&root_img, root_lba * 512, &rootfs)?;

    // 1b. `install-root.img`: a pristine copy of the staged tree in a GPT whose one partition is
    // `nitrox-source`. `nxinstall` copies it rather than the live root, which the session writes to
    // and a raw read could tear; the storage service passes over this name, so nothing mounts it.
    let install_root = work.join("install-root.img");
    let source_mib = staged.div_ceil(MIB) + INSTALL_ROOT_MARGIN_MIB;
    fs::File::create(&install_root)?.set_len((source_mib + 2) * MIB)?;
    run(Command::new("sgdisk")
        .arg("--clear")
        .arg("-n")
        .arg("1:2048:0")
        .arg("-t")
        .arg("1:8300")
        .arg("-c")
        .arg(format!("1:{}", libgpt::INSTALL_SOURCE_LABEL))
        .arg(&install_root))?;
    let (source_lba, source_sectors) = partition_extent(&install_root, 1)?;
    let sourcefs = work.join("install-root.ext4");
    run(Command::new("mke2fs")
        .arg("-q")
        .arg("-F")
        .arg("-t")
        .arg("ext4")
        .arg("-O")
        .arg("^has_journal,^64bit,^metadata_csum,^resize_inode")
        .arg("-b")
        .arg("4096")
        .arg("-d")
        .arg(&staging)
        .arg(&sourcefs)
        .arg(((source_sectors * 512) / 4096).to_string()))?;
    splice_into(&install_root, source_lba * 512, &sourcefs)?;

    // 2. **The ESP the installer writes to a disk**: FAT32 from the same `build_esp`, with the
    // *release* menu, riding as a module so the installer copies it sector by sector. **Sized to
    // its contents**: the firmware reads every module off the stick before the kernel runs; FAT32's
    // floor is about 33 MiB. **Its initramfs is built here**: the caller's is the *live* one and
    // names `gpt-partlabel:nitrox-live`, which an installed machine would answer by mounting the
    // stick. `check_live_image` holds this ESP against the release one. In a test stick it is built
    // in the same mode as the rest, so the kernel beside it and its initramfs agree.
    let install_esp = work.join("install-esp.img");
    let install_initramfs = work.join("initramfs");
    build_initramfs_for(&install_initramfs, mode, RootDevice::Disk)?;
    let esp_payload = [bootx64, kernel, install_initramfs.as_path()]
        .iter()
        .map(|p| fs::metadata(p).map(|m| m.len()))
        .sum::<Result<u64, _>>()?;
    let install_esp_mib = FAT32_MIN_MIB.max(esp_payload.div_ceil(MIB) + 8);
    build_esp(&install_esp, install_esp_mib * MIB / 512, bootx64, &limine_conf(), kernel, &install_initramfs, &[])?;

    let conf = work.join("limine.conf");
    fs::write(&conf, live_limine_conf(&fs::read_to_string(limine_conf())?)?)?;

    let payload = [bootx64, kernel, initramfs, root_img.as_path(), install_esp.as_path(), install_root.as_path()]
        .iter()
        .map(|p| fs::metadata(p).map(|m| m.len()))
        .sum::<Result<u64, _>>()?;
    let esp_mib = ESP_SIZE_MIB.max(payload.div_ceil(MIB) + 8);
    if out.exists() {
        fs::remove_file(out)?;
    }
    fs::File::create(out)?.set_len((esp_mib + 2) * MIB)?;
    run(Command::new("sgdisk")
        .arg("--clear")
        .arg("-n")
        .arg(format!("1:2048:+{esp_mib}M"))
        .arg("-t")
        .arg("1:ef00")
        .arg("-c")
        .arg("1:NITROX_ESP")
        .arg(out))?;
    let (esp_lba, esp_sectors) = partition_extent(out, 1)?;
    let esp = work.join("esp.img");
    build_esp(
        &esp,
        esp_sectors,
        bootx64,
        &conf,
        kernel,
        initramfs,
        &[
            (root_img.as_path(), "/boot/root.img"),
            (install_esp.as_path(), "/boot/install-esp.img"),
            (install_root.as_path(), "/boot/install-root.img"),
        ],
    )?;
    splice_into(out, esp_lba * 512, &esp)?;
    println!(
        "xtask: live root.img {root_img_mib} MiB ({staged} bytes staged), install-root.img {} MiB, \
         stick {} MiB",
        source_mib + 2,
        esp_mib + 2
    );
    Ok(())
}

const LIVE_MENU_TIMEOUT_SECS: u32 = 5;

const LIVE_REPORT_ENTRY: &str = "Nitrox — hardware report";

/// The live menu's third entry, which loads what the installer copies.
const LIVE_INSTALL_ENTRY: &str = "Nitrox — install to this machine";

/// The live image's `limine.conf`, from the release one (`base`): `root.img` as a second module and
/// **a menu** (a countdown, the release entry first, and an entry passing `cmdline: hwreport`).
/// Only the live image has it: the others keep `timeout: 0`, and a nonzero timeout is what makes
/// Limine draw the menu.
pub(crate) fn live_limine_conf(base: &str) -> R<String> {
    let exactly_once = |line: &str| {
        if base.matches(line).count() == 1 {
            Ok(())
        } else {
            Err(format!("boot/limine.conf no longer has {line:?} exactly once"))
        }
    };
    let (timeout_line, entry_line, initramfs_line) =
        ("timeout: 0", "/Nitrox\n", "    module_path: boot():/boot/initramfs\n");
    exactly_once(timeout_line)?;
    exactly_once(entry_line)?;
    exactly_once(initramfs_line)?;
    let entry_start = base.find(entry_line).expect("checked above");
    if base[entry_start + entry_line.len()..].contains("\n/") {
        return Err("boot/limine.conf has a second entry; the live menu adds its own".into());
    }
    let with_root =
        base.replacen(initramfs_line, &format!("{initramfs_line}    module_path: boot():/boot/root.img\n"), 1);
    let with_timeout = with_root.replacen(timeout_line, &format!("timeout: {LIVE_MENU_TIMEOUT_SECS}"), 1);
    let entry_start = with_timeout.find(entry_line).expect("checked above");
    let default_entry = with_timeout[entry_start..].trim_end().to_string();
    let report_entry = default_entry.replacen("/Nitrox", &format!("/{LIVE_REPORT_ENTRY}"), 1).replacen(
        "\n    path:",
        "\n    cmdline: hwreport\n    path:",
        1,
    );
    if !report_entry.contains("cmdline: hwreport") {
        return Err("boot/limine.conf's entry has no `path:` line to put the command line beside".into());
    }
    // The installer's entry carries modules, not authority. **The installable ESP and the pristine
    // root ride on this entry alone**: Limine loads a module only if the chosen entry names it, so
    // other boots do not read them off the stick.
    let install_entry = default_entry.replacen("/Nitrox", &format!("/{LIVE_INSTALL_ENTRY}"), 1)
        + "\n    module_path: boot():/boot/install-esp.img"
        + "\n    module_path: boot():/boot/install-root.img";
    Ok(format!("{}\n\n{report_entry}\n\n{install_entry}\n", with_timeout.trim_end()))
}

/// Total bytes of the regular files under `dir`.
fn tree_bytes(dir: &Path) -> R<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        total += if meta.is_dir() { tree_bytes(&entry.path())? } else { meta.len() };
    }
    Ok(total)
}
