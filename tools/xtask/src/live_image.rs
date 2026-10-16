//! `image --live`: the live image, its root a RAM-disk module, and its boot menu.

use crate::*;

/// `cargo xtask image --live` — the live image (Phase 5 Part C): the release kernel and initramfs,
/// with the release root filesystem riding along as a second Limine module, so a machine boots to
/// a desktop without a storage driver.
pub(crate) fn cmd_image_live() -> R<()> {
    cmd_image_live_for(BuildMode::Normal)
}

/// The live image built in `mode`: the release one, or with `--selftest` the **test live image**
/// (administration Part C.8) — the same stick with the test data a `--selftest` image carries,
/// its test packages on the root in RAM. `check-images` holds each to its own ordinary image.
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

/// Assemble the live image at `out`: a GPT disk with one ESP holding everything, `root.img`
/// included. `root.img` is itself a GPT image with one `nitrox-live` partition, whose ext4
/// filesystem is built from [`stage_rootfs`] for `mode` — the release root, in RAM, or a test one.
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

    // 1. `root.img`: a partition sized to the staged tree plus room to write, under its ceiling.
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

    // 1b. **The installer's source** (administration Part G.1): `install-root.img`, a pristine copy
    //     of the same staged tree in a GPT whose one partition is `nitrox-source`. `nxinstall`
    //     copies it, where it used to copy the live root raw — a root the session writes to and
    //     `init` has mounted, so `disks` withholds it and a raw read could tear. The partition's
    //     name is distinct because the live root is found by its label, and it is the name the
    //     storage service passes over, so nothing mounts the copy. Sized to its files and
    //     `mke2fs`'s tables, since nothing writes it.
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

    // 2. **The ESP the installer writes to a disk** (Phase 5 Part H.1): a FAT32 filesystem built
    //    by the same `build_esp` every image uses, carrying the *release* menu — no module lines,
    //    and an initramfs whose `init.toml` names `gpt-partlabel:nitrox-root`. It rides as a
    //    module, so the kernel publishes it as a block device and the installer copies it sector
    //    by sector; writing FAT32 is thereby not this project's problem.
    //
    //    **Sized to its contents, not to `ESP_SIZE_MIB`.** The firmware reads every module off
    //    the stick before the kernel runs, which is what `LIVE_ROOT_MAX_MIB` exists to bound, and
    //    a 48 MiB image mostly of zeroes would triple that read for nothing. FAT32's own floor is
    //    about 33 MiB of clusters, so that is the floor here too.
    //
    //    **Its initramfs is built here rather than being the one this function was handed**: the
    //    caller's is the *live* one, and it names `gpt-partlabel:nitrox-live`. Copied into an
    //    installed machine it would produce a system that mounts the stick it was installed from
    //    — which boots exactly once, on the desk, and never again (PR #307 review, finding 6).
    //    `check_live_image` reads this filesystem back out of the built stick and holds every
    //    file in it against the release ESP, because the mistake is invisible until a real
    //    machine reboots without the stick.
    //
    //    **In a test live image it is built in the same mode as the rest**, so the kernel beside it
    //    and its initramfs agree. Nobody installs from a test stick; `check-images` compares only
    //    the release stick's installable ESP with a release image's.
    let install_esp = work.join("install-esp.img");
    let install_initramfs = work.join("initramfs");
    build_initramfs_for(&install_initramfs, mode, RootDevice::Disk)?;
    let esp_payload = [bootx64, kernel, install_initramfs.as_path()]
        .iter()
        .map(|p| fs::metadata(p).map(|m| m.len()))
        .sum::<Result<u64, _>>()?;
    let install_esp_mib = FAT32_MIN_MIB.max(esp_payload.div_ceil(MIB) + 8);
    build_esp(&install_esp, install_esp_mib * MIB / 512, bootx64, &limine_conf(), kernel, &install_initramfs, &[])?;

    // 3. Limine's configuration: the release one, with `root.img` as a module and a menu with a
    //    hardware-report entry and an installer entry, which alone loads the installable ESP and
    //    the pristine root.
    let conf = work.join("limine.conf");
    fs::write(&conf, live_limine_conf(&fs::read_to_string(limine_conf())?)?)?;

    // 4. The stick: one ESP big enough for all of it.
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

/// How long the live image's boot menu counts down before booting its default entry.
const LIVE_MENU_TIMEOUT_SECS: u32 = 5;

/// The live image's boot-menu entry that boots into the hardware report (Phase 5 Part D.2).
const LIVE_REPORT_ENTRY: &str = "Nitrox — hardware report";

/// The live menu's third entry, which loads what the installer copies: the installable ESP and the
/// pristine root. Its sessions are ordinary (administration Part G.3).
const LIVE_INSTALL_ENTRY: &str = "Nitrox — install to this machine";

/// The live image's `limine.conf`, from the release one (`base`): `root.img` as a second module,
/// and **a menu** — a countdown, the release entry first as the default, and a second entry that
/// passes `cmdline: hwreport`.
///
/// Only the live image carries the menu: it is the stick a person boots a machine with, and the
/// release and test images keep `timeout: 0` so no other gate pays for a countdown. A nonzero
/// timeout is also what makes Limine draw the menu at all.
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
    // **The installer's own entry** (Phase 5 Part H.1). **It carries modules, not authority**
    // (administration Part G.3): until then it passed `cmdline: install`, which made its sessions
    // hold every disk. Now its sessions are ordinary, a program reaches a disk only through the
    // view broker's `disks` grant — `with admin nxinstall` — and what the entry adds is the two
    // things that installer copies.
    //
    // **The installable ESP rides on this entry alone**, and since administration Part G.1 **the
    // pristine root beside it**. Limine loads a module because the entry the person chose names
    // it, so a module line here costs the other two boots nothing: an ordinary live boot does not
    // read them off the stick, hold them in RAM for the rest of the session, or publish block
    // devices no session can use. The plan's worry about what firmware reads before the kernel
    // runs is therefore a worry about *installing*, which is the one boot where paying it buys
    // something.
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
