//! `check-storage`: the test live stick beside a copy of the release disk, checked by the host.

use crate::*;

mod format;
mod sticks;

pub(crate) use format::*;
pub(crate) use sticks::*;

/// The file `check-storage` writes, at the root of the SATA disk's `nitrox-root`.
pub(crate) const STORAGE_PATTERN_FILE: &str = "check-storage.bin";

/// The pattern `test-pattern` writes, **written down a second time here** so the gate does not
/// take its aim from the program under test: `userspace/test-harness/src/pattern.rs`'s `LEN` and
/// `byte` must match these.
pub(crate) const STORAGE_PATTERN_LEN: usize = 3 * 4096 + 1234;

pub(crate) fn storage_pattern_byte(i: usize) -> u8 {
    ((i >> 12) as u8).wrapping_mul(0x5B) ^ (i as u8) ^ 0xA5
}

/// `cargo xtask check-storage` — **the storage chain, with the host holding the result**
///
/// The test live image boots as a USB stick beside **a copy of the release disk** on the AHCI
/// controller: the one topology with a second disk the host can read afterwards, since the only
/// other second disk a boot can have is a RAM disk, whose writes never reach a host file.
///
/// The copy's root is first marked **not cleanly unmounted**, as an installed machine's is
/// until its `shutdown`, and the copy gains **a decoy**: a zeroed partition labelled as the live
/// root is, which the kernel must not name `/dev/disk/by-partlabel/nitrox-live` and the storage
/// service must not take for `init`'s root ([`add_decoy_live_root`]). On the serial console:
/// - the storage service reports it so and auto-mounts it read-only, it being a live boot, and a
///   write there is refused;
/// - the table's `clean` column says no, and still says no after the read-only unmount, which
///   wrote nothing;
/// - `with admin disk` mounts it writable, and `test-pattern` writes a pattern through a mapping
///   and **exits without a sync**. The host sees the file on the disk without the pattern;
/// - `test-pattern --check` reads it back through `/storage`, and `with admin disk --unmount`
///   runs the chain, after which the table says clean.
///
/// Then the machine is stopped, and the host carves `nitrox-root` out of the disk: `e2fsck -fn`
/// clean, the superblock marked clean, and the file holding the pattern — read with `debugfs`,
/// not with the library that wrote it.
pub(crate) fn cmd_check_storage(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    require_tool("e2fsck")?;
    require_tool("debugfs")?;
    require_tool("fsck.fat")?;
    // **A copy of the release disk, made fresh**, so a run cannot pass on what an earlier one
    // wrote. The release image is built first: building the test stick rebuilds the programs
    // for its own mode.
    cmd_image(BuildMode::Normal)?;
    let work = build_cache().join("check-storage");
    fs::create_dir_all(&work)?;
    let disk = work.join("disk.img");
    let _ = fs::remove_file(&disk);
    fs::copy(image_path(), &disk)?;
    mark_root_not_clean(&disk)?;
    add_internal_fat(&disk)?;
    add_decoy_live_root(&disk)?;
    cmd_image_live_for(BuildMode::Selftest)?;
    // **Two sticks to plug in**: an MBR one holding an ext4 partition, which is ejected and read
    // on the host, and a whole-disk ext4, which is pulled while mounted.
    let stick_mbr = work.join("stick-mbr.img");
    mbr_ext4_stick(&stick_mbr, 48, [STICK_MBR_LABEL, STICK_MBR_LABEL_2])?;
    let stick_whole = work.join("stick-whole.img");
    whole_ext4_stick(&stick_whole, 16, STICK_WHOLE_LABEL)?;
    // **And a FAT stick**, for `fs-server-fat`.
    let stick_fat = work.join("stick-fat.img");
    fat32_stick(&stick_fat, &work)?;
    // **And a blank one**, 2 GiB and sparse, partitioned and formatted in the guest.
    let stick_blank = work.join(BLANK_STICK);
    let _ = fs::remove_file(&stick_blank);
    fs::File::create(&stick_blank).and_then(|f| f.set_len(2 << 30))?;
    let qmp_sock = work.join("qmp.sock");
    let _ = fs::remove_file(&qmp_sock);

    let ovmf = locate_ovmf()?;
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-qmp")
        .arg(format!("unix:{},server=on,wait=off", qmp_sock.display()))
        .arg("-device")
        .arg(XHCI_DEVICE)
        .arg("-drive")
        .arg(format!("if=none,id=stick,format=raw,file={}", live_test_image_path().display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=stick")
        .arg("-drive")
        .arg(format!("if=none,id=disk,format=raw,file={}", disk.display()))
        .arg("-device")
        .arg("ide-hd,drive=disk,bus=ide.0")
        .arg("-display")
        .arg("none")
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
    crate::scsi_trace::trace_args(&mut cmd, &work.join("scsi.log"));
    println!("xtask: storage gate — booting the test live image beside a copy of the release disk…\n");
    let mut session = Session::spawn(cmd, "check-storage")?;
    let result = run_storage_steps(&mut session, &disk, &work).and_then(|()| {
        let mut qmp = Qmp::connect(&qmp_sock)?;
        run_stick_steps(&mut session, &mut qmp, &work)?;
        run_format_steps(&mut session, &mut qmp, &work)
    });
    let transcript = session.finish();
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    println!("\nxtask: the machine is stopped; the disk, on the host:");
    check_storage_disk(&disk, &work)?;
    println!("\nxtask: and the sticks:");
    check_sticks(&stick_mbr, &work.join("stick-whole-pulled.img"), &work)?;
    check_fat_stick(&stick_fat, &work)?;
    println!("\nxtask: and the stick partitioned and formatted in the guest:");
    check_formatted_stick(&work.join(BLANK_STICK_EXT4), &stick_blank, &work)?;
    println!(
        "\nxtask: a file written through a mapping and never synced reached the disk through an \
         unmount, and the filesystem was left clean ✓"
    );
    Ok(())
}

pub(crate) const STICK_MBR_LABEL: &str = "nxstick";

/// The MBR stick's second partition's label: an eject takes the whole stick.
pub(crate) const STICK_MBR_LABEL_2: &str = "nxstick2";

/// The MBR stick's two partitions, as sectors: the first 31 MiB from 1 MiB, the second 15 MiB from
/// 32 MiB, on a 48 MiB stick.
pub(crate) const STICK_MBR_PARTS: [(u32, u32); 2] = [(2048, 63_488), (65_536, 30_720)];

pub(crate) const STICK_WHOLE_LABEL: &str = "nxwhole";

/// `mke2fs`'s features for a filesystem `fs-server-ext4` serves: the live image's root's.
const SERVED_EXT4_FEATURES: &str = "^has_journal,^64bit,^metadata_csum,^resize_inode";

fn served_ext4(path: &Path, blocks: u64, label: &str) -> R<()> {
    require_tool("mke2fs")?;
    run(Command::new("mke2fs")
        .arg("-q")
        .arg("-F")
        .arg("-t")
        .arg("ext4")
        .arg("-O")
        .arg(SERVED_EXT4_FEATURES)
        .arg("-b")
        .arg("4096")
        .arg("-L")
        .arg(label)
        .arg(path)
        .arg(blocks.to_string()))
}

/// **A stick with an MBR naming one Linux partition**, `mib` MiB, the partition from 1 MiB to the
/// end holding an ext4 labelled `label`.
///
/// **Two partitions**, an ext4 in each of [`STICK_MBR_PARTS`], so an eject named for one is seen
/// to take the other.
fn mbr_ext4_stick(path: &Path, mib: u64, labels: [&str; 2]) -> R<()> {
    let f = fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
    f.set_len(mib * 1024 * 1024)?;
    drop(f);
    let parts = STICK_MBR_PARTS.map(|(first, count)| (first, count, 0x83));
    write_mbr_parts(path, &parts)?;
    for ((first, count), label) in STICK_MBR_PARTS.into_iter().zip(labels) {
        let fs_img = path.with_extension(format!("{label}.ext4"));
        let _ = fs::remove_file(&fs_img);
        served_ext4(&fs_img, count as u64 * 512 / 4096, label)?;
        splice_into(path, first as u64 * 512, &fs_img)?;
    }
    Ok(())
}

fn whole_ext4_stick(path: &Path, mib: u64, label: &str) -> R<()> {
    let _ = fs::remove_file(path);
    served_ext4(path, mib * 256, label)
}

/// The FAT stick's label, and the names the host puts on it: a long name, a
/// Unicode one, a file two directories down, and one the guest removes. Beside them, the pattern
/// at [`STORAGE_PATTERN_FILE`], which the guest reads back.
pub(crate) const STICK_FAT_LABEL: &str = "NXFAT";

pub(crate) const FAT_LONG_NAME: &str = "a long file name from the host.txt";

pub(crate) const FAT_UNICODE_NAME: &str = "naïve résumé 名前.txt";

pub(crate) const FAT_NESTED_DIR: &str = "nested/deeper";

pub(crate) const FAT_NESTED_NAME: &str = "inner file.txt";

pub(crate) const FAT_DOOMED: &str = "REMOVE.ME";

/// What the guest makes on the FAT stick: a directory, a copy of the Unicode-named file into it
/// under a longer Unicode name built from its own, the long-named file renamed into it, and a file
/// written through a mapping. **The directory's name has no space**, so a path through it is one
/// bareword: a parenthesised path straight after a command's name reads as a call to it
/// (`userspace/nxsh/src/parse.rs`), so `copy` and `rename` lead with `--force` when theirs must be
/// computed.
pub(crate) const FAT_MADE_DIR: &str = "made-by-nitrox";

pub(crate) const FAT_RENAMED: &str = "renamed from the host.txt";

pub(crate) const FAT_WRITTEN: &str = "written-by-nitrox.bin";

pub(crate) const INTERNAL_FAT_PARTLABEL: &str = "internal-fat";

pub(crate) const INTERNAL_FAT_LABEL: &str = "NXINTERNAL";

pub(crate) fn fat_host_bytes(name: &str) -> Vec<u8> {
    match name {
        FAT_LONG_NAME => (0..9_000u32).map(|i| (i % 251) as u8).collect(),
        FAT_UNICODE_NAME => b"unicode, from the host\n".to_vec(),
        FAT_NESTED_NAME => b"two directories down\n".to_vec(),
        STORAGE_PATTERN_FILE => (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect(),
        _ => b"the guest removes this\n".to_vec(),
    }
}

/// An mtools command, with names in UTF-8 and no complaint about a geometry mtools did not choose.
pub(crate) fn mtools_cmd(tool: &str) -> Command {
    let mut c = Command::new(tool);
    c.env("MTOOLS_SKIP_CHECK", "1").env("LC_ALL", "C.UTF-8");
    c
}

/// **A FAT stick for `fs-server-fat`**: 300 MiB, sparse, an MBR naming one FAT32
/// partition from 1 MiB, formatted `mformat -F -c 8` — **FAT32 by its cluster count, with 4 KiB
/// clusters, and its data region off a 4 KiB boundary**, which is the case maps in sectors exist
/// for and is asserted here from the boot sector, so a formatter that aligned it would fail the
/// gate before the boot. Then the host's files: [`fat_host_bytes`]'s, by `mcopy`.
fn fat32_stick(path: &Path, work: &Path) -> R<()> {
    use std::io::{Read, Seek, SeekFrom};
    for tool in ["mformat", "mcopy", "mmd"] {
        require_tool(tool)?;
    }
    const MIB: u64 = 1024 * 1024;
    let _ = fs::remove_file(path);
    fs::File::create(path)?.set_len(300 * MIB)?;
    let count = (300 * 2048 - 2048) as u32;
    write_mbr(path, 2048, count, 0x0C)?;
    let at = format!("{}@@{MIB}", path.display());
    run(mtools_cmd("mformat")
        .arg("-i")
        .arg(&at)
        .arg("-F")
        .arg("-c")
        .arg("8")
        .arg("-T")
        .arg(count.to_string())
        .arg("-v")
        .arg(STICK_FAT_LABEL)
        .arg("::"))?;
    let mut boot = [0u8; 512];
    let mut f = fs::File::open(path)?;
    f.seek(SeekFrom::Start(MIB))?;
    f.read_exact(&mut boot)?;
    let reserved = u16::from_le_bytes([boot[14], boot[15]]) as u64;
    let fat_sectors = u32::from_le_bytes([boot[36], boot[37], boot[38], boot[39]]) as u64;
    let data = 2048 + reserved + boot[16] as u64 * fat_sectors;
    let clusters = (count as u64 + 2048 - data) / boot[13] as u64;
    if data.is_multiple_of(8) || boot[13] != 8 || clusters < 65_525 {
        return Err(format!(
            "the FAT stick's data region is at sector {data} with {} sectors a cluster and {clusters} \
             clusters: the gate needs FAT32 by count, 4 KiB clusters, and data off a 4 KiB boundary",
            boot[13]
        )
        .into());
    }
    let src = work.join("fat-src");
    let _ = fs::remove_dir_all(&src);
    fs::create_dir_all(&src)?;
    run(mtools_cmd("mmd").arg("-i").arg(&at).arg("::nested").arg(format!("::{FAT_NESTED_DIR}")))?;
    for (name, dest) in [
        (FAT_LONG_NAME, FAT_LONG_NAME.to_string()),
        (FAT_UNICODE_NAME, FAT_UNICODE_NAME.to_string()),
        (FAT_NESTED_NAME, format!("{FAT_NESTED_DIR}/{FAT_NESTED_NAME}")),
        (STORAGE_PATTERN_FILE, STORAGE_PATTERN_FILE.to_string()),
        (FAT_DOOMED, FAT_DOOMED.to_string()),
    ] {
        let file = src.join(name);
        fs::write(&file, fat_host_bytes(name))?;
        run(mtools_cmd("mcopy").arg("-i").arg(&at).arg(&file).arg(format!("::{dest}")))?;
    }
    println!(
        "xtask: the FAT stick: FAT32 of {clusters} 4 KiB clusters, its data from sector {data}, off \
         a 4 KiB boundary ✓"
    );
    Ok(())
}

/// **An internal disk's FAT**: a third partition on the copy of the release
/// disk, grown for it — 64 MiB of FAT32 with 4 KiB clusters, which `fs-server-fat` would serve,
/// on a disk that is not removable, so nothing mounts it by itself. On the same disk because the
/// AHCI driver binds the first SATA disk it finds.
fn add_internal_fat(disk: &Path) -> R<()> {
    require_tool("sgdisk")?;
    require_tool("mformat")?;
    const MIB: u64 = 1024 * 1024;
    let len = fs::metadata(disk)?.len();
    fs::OpenOptions::new().write(true).open(disk)?.set_len(len + 65 * MIB)?;
    run(Command::new("sgdisk").arg("-e").arg(disk))?;
    run(Command::new("sgdisk")
        .arg("-n")
        .arg("3:0:0")
        .arg("-t")
        .arg("3:0700")
        .arg("-c")
        .arg(format!("3:{INTERNAL_FAT_PARTLABEL}"))
        .arg(disk))?;
    let (lba, sectors) = partition_extent(disk, 3)?;
    run(mtools_cmd("mformat")
        .arg("-i")
        .arg(format!("{}@@{}", disk.display(), lba * 512))
        .arg("-F")
        .arg("-c")
        .arg("8")
        .arg("-T")
        .arg(sectors.to_string())
        .arg("-v")
        .arg(INTERNAL_FAT_LABEL)
        .arg("::"))
}

/// **A SATA partition labelled as the live root is**: 2 MiB of zeros, the fourth on the copy,
/// published before the RAM disk holding the real one.
fn add_decoy_live_root(disk: &Path) -> R<()> {
    const MIB: u64 = 1024 * 1024;
    let len = fs::metadata(disk)?.len();
    fs::OpenOptions::new().write(true).open(disk)?.set_len(len + 3 * MIB)?;
    run(Command::new("sgdisk").arg("-e").arg(disk))?;
    run(Command::new("sgdisk").arg("-n").arg("4:0:0").arg("-c").arg(format!("4:{LIVE_ROOT_PARTLABEL}")).arg(disk))
}

/// Mark the disk image's `nitrox-root` **not cleanly unmounted**, as an installed machine's is:
/// nothing unmounts `init`'s root until `shutdown`. It is also what makes the table's `clean`
/// column mean something: after the read-only unmount it must still say no.
pub(crate) fn mark_root_not_clean(disk: &Path) -> R<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    let (lba, _) = partition_extent(disk, 2)?;
    let at = lba * 512 + 1024 + 58;
    let mut f = fs::OpenOptions::new().read(true).write(true).open(disk)?;
    let mut b = [0u8; 2];
    f.seek(SeekFrom::Start(at))?;
    f.read_exact(&mut b)?;
    f.seek(SeekFrom::Start(at))?;
    f.write_all(&(u16::from_le_bytes(b) & !EXT4_VALID_FS).to_le_bytes())?;
    Ok(())
}

pub(crate) fn storage_root_fs(disk: &Path, work: &Path) -> R<PathBuf> {
    let fs_img = work.join("nitrox-root.ext4");
    carve_partition(disk, 2, &fs_img)?;
    Ok(fs_img)
}

pub(crate) fn debugfs_cat(fs_img: &Path, path: &str) -> R<Option<Vec<u8>>> {
    let out = Command::new("debugfs")
        .arg("-R")
        .arg(format!("cat {path}"))
        .arg(fs_img)
        .output()
        .map_err(|e| format!("run debugfs: {e}"))?;
    if String::from_utf8_lossy(&out.stderr).contains("not found") {
        return Ok(None);
    }
    if !out.status.success() {
        return Err(format!("debugfs could not read {path} in {}", fs_img.display()).into());
    }
    Ok(Some(out.stdout))
}

/// The superblock's `s_state` in the ext4 image `fs_img`: bytes 58–59 of the superblock at 1024,
/// read here rather than through `fs_server_ext4::ext4::was_left_clean`, the code under test.
pub(crate) fn ext4_s_state(fs_img: &Path) -> R<u16> {
    let bytes = fs::read(fs_img)?;
    let at = 1024 + 58;
    bytes
        .get(at..at + 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| format!("{} has no superblock", fs_img.display()).into())
}

/// `s_state`'s "cleanly unmounted" bit (`EXT4_VALID_FS`) and its error bit (`EXT4_ERROR_FS`).
pub(crate) const EXT4_VALID_FS: u16 = 0x0001;

pub(crate) const EXT4_ERROR_FS: u16 = 0x0002;

fn run_storage_steps(s: &mut Session, disk: &Path, work: &Path) -> R<()> {
    let at = format!("/storage/{ROOT_PARTLABEL}/{STORAGE_PATTERN_FILE}");
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();

    // 0. The menu's countdown boots its first entry, the ordinary one.
    s.expect(", cmdline \"\"")?;
    // 0b. **The decoy does not take the live root's name**; the storage service's live boot, below.
    s.expect(&format!(
        "gpt: /dev/disk/by-partlabel/{LIVE_ROOT_PARTLABEL} names an earlier partition, the boot medium's"
    ))?;
    s.expect("init: mounted fs-server-ext4 at /")?;
    println!("  ok: the SATA disk's decoy {LIVE_ROOT_PARTLABEL} passed over for the RAM disk's");

    // 1. **The disk's root is auto-mounted read-only**, the boot being a live one. Its name is
    //    what the storage service calls it, read off the line that reports it.
    let reported = format!("(partition {ROOT_PARTLABEL}): ext4");
    s.expect(&reported)?;
    let line = s.matched_line()?;
    let name = line
        .split_whitespace()
        .find(|w| w.starts_with("blk-"))
        .ok_or_else(|| format!("no `blk-<n>` in {line:?}"))?
        .to_string();
    let index = name.trim_start_matches("blk-").to_string();
    if !line.contains(&format!("ext4, not left clean; mounted at /storage/{ROOT_PARTLABEL} (ro)")) {
        return Err(format!(
            "the disk's {ROOT_PARTLABEL} is not reported not clean and auto-mounted read-only: \
             {line:?}. The gate cleared its clean bit, and a live boot mounts the machine's own \
             disks read-only"
        )
        .into());
    }
    s.expect("storage-service: a live boot")?;
    println!(
        "  ok: {name}, the disk's {ROOT_PARTLABEL}, not left clean and auto-mounted read-only on a \
         live boot"
    );

    // 1b. **`boot-probe` has finished, before anything is typed.** It starts once the test
    //     harness's chain has, and a `--kvm` run had it starting while a stick was pulled. Its
    //     later tests open view-broker clients until the broker refuses one, and install a
    //     policy, either of which a `with admin` here could meet halfway. **Its verdict is not
    //     this gate's**: it takes the SATA disk's `nitrox-root` for the root it wrote, finds no
    //     scratch disk, and fails. Waited for as `check-shutdown` waits, whatever its code.
    if !s.expect_within("service-mgr: 'boot-probe' exited code=", std::time::Duration::from_secs(300))? {
        return Err("`boot-probe` did not exit within five minutes of the storage service's report".into());
    }
    println!("  ok: boot-probe has finished, so nothing of it overlaps what is typed");

    // 2. A serial login. The prompt is searched for in the whole transcript, as `check-live`
    //    does: it and the greeter come up together.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !s.transcript().contains("nitrox login:") {
        if std::time::Instant::now() > deadline {
            return Err("no `nitrox login:` prompt on the serial column".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    // **The table's `clean` column, as a session reads it** — a token the typed command does not
    // contain, so the match is the answer and not the echo. The row is found by the device's name:
    // `label` is the filesystem's own, and this one has none.
    let clean_now = |s: &mut Session, expected: bool| -> R<()> {
        s.send(&format!(
            "disk --list | filter name == \"{name}\" | map {{ |r| format(\"clean-now={{}}\", r.clean) }}"
        ))?;
        s.expect(&format!("clean-now={expected}"))?;
        s.expect("/home>")?;
        Ok(())
    };
    clean_now(s, false)?;

    // 3. **A write there is refused**, by the read-only mount: named, rather than any failure.
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: {at} refused: NoAccess (-2)"))?;
    s.expect("/home>")?;
    println!("  ok: a write to it is refused NoAccess");
    //    **And a session's eject of it is refused**: an internal disk is the
    //    `storage` grant's to unmount, and the refusal says how — with no password asked.
    let from = s.transcript().len();
    s.send(&format!("disk --eject {ROOT_PARTLABEL}"))?;
    s.expect(&format!(
        "disk: {ROOT_PARTLABEL} not ejected: it is not removable: an internal disk is unmounted with the storage \
         grant (with admin disk --unmount)"
    ))?;
    s.expect("/home>")?;
    if s.transcript()[from..].contains("password:") {
        return Err("`disk --eject` asked for a password".into());
    }
    println!("  ok: `disk --eject {ROOT_PARTLABEL}` is refused, naming `with admin disk --unmount`");

    // 4. Unmounted, and mounted writable, through the `storage` grant.
    let admin = |s: &mut Session, command: &str| -> R<()> { with_admin_asked(s, command) };
    admin(s, &format!("disk --unmount {ROOT_PARTLABEL}"))?;
    s.expect("fs-server: unmounted; a read-only mount wrote nothing")?;
    s.expect(&format!("storage-service: unmounted {ROOT_PARTLABEL}, not left clean (read-only, so as it was found)"))?;
    s.expect(&format!("disk: unmounted {ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;
    // **Still not clean**, and the table says so: it reads the device, not the unmount's word.
    clean_now(s, false)?;
    admin(s, &format!("disk --mount /dev/blk/{index}"))?;
    s.expect(&format!("storage-service: mounted {ROOT_PARTLABEL} (rw), as asked"))?;
    s.expect(&format!("disk: mounted {name} at /storage/{ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;
    println!("  ok: `with admin disk` unmounted it and mounted it writable");

    // 5. **Written through a mapping, and not synced.**
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!(
        "test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not \
         sync"
    ))?;
    s.expect("/home>")?;

    // 6. **On the host, the file is on the disk and the pattern is not** — so what the last step
    //    finds, the unmount put there. Read while the guest runs: QEMU writes the image file as
    //    the guest writes the disk, and nothing on this filesystem is moving between steps. The
    //    file's being there, at its size, is what makes the pattern's absence mean something, and
    //    the superblock says it is mounted writable, so a clean one at the end is the unmount's.
    let fs_img = storage_root_fs(disk, work)?;
    let path = format!("/{STORAGE_PATTERN_FILE}");
    match debugfs_cat(&fs_img, &path)? {
        None => {
            return Err(format!(
                "{path} is not on the disk at all after `test-pattern --write`: the server's \
                 create did not reach it, so the absence of the pattern would prove nothing"
            )
            .into());
        }
        Some(b) if b.len() != STORAGE_PATTERN_LEN => {
            return Err(format!(
                "{path} is on the disk at {} bytes, not the {STORAGE_PATTERN_LEN} it was created \
                 with",
                b.len()
            )
            .into());
        }
        Some(b) if b == pattern => {
            return Err(format!(
                "{path} already holds the pattern before any sync or unmount — something wrote it \
                 back, so this gate cannot show that the unmount does"
            )
            .into());
        }
        Some(_) => {}
    }
    let state = ext4_s_state(&fs_img)?;
    if state & EXT4_VALID_FS != 0 {
        return Err(format!(
            "the superblock says clean (s_state {state:#06x}) while the filesystem is mounted \
             writable, so a clean one at the end would not be the unmount's doing"
        )
        .into());
    }
    println!(
        "  ok: on the host meanwhile: the file is there at {STORAGE_PATTERN_LEN} bytes without the \
         pattern, and the superblock says mounted"
    );

    // 7. Read back through `/storage` in the guest: from the kernel's cache, the one object.
    s.send(&format!("test-pattern --check {at}"))?;
    s.expect(&format!("test-pattern: {at} holds the pattern, {STORAGE_PATTERN_LEN} bytes ok"))?;
    s.expect("/home>")?;
    println!("  ok: test-pattern reads it back through /storage");

    // 8. **The unmount**: written back, marked clean, the drive flushed.
    admin(s, &format!("disk --unmount {ROOT_PARTLABEL}"))?;
    s.expect("fs-server: unmounted, and the filesystem recorded clean")?;
    s.expect(&format!("storage-service: unmounted {ROOT_PARTLABEL}, left clean"))?;
    s.expect(&format!("disk: unmounted {ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;
    // And the table, which said no after the read-only unmount, now says yes.
    clean_now(s, true)?;
    println!("  ok: `with admin disk --unmount` ran the chain, and the table says clean");
    Ok(())
}

/// **The blank stick `check-storage` partitions and formats**, and the copy of it
/// taken once its ext4 was unmounted, before an MBR replaced the GPT.
pub(crate) const BLANK_STICK: &str = "stick-blank.img";

pub(crate) const BLANK_STICK_EXT4: &str = "stick-blank-ext4.img";

/// **The whole line holding `pat`** after `from`, waited for.
pub(crate) fn line_after(s: &mut Session, from: usize, pat: &str) -> R<String> {
    s.line_since(from, pat, |_: &str| true, std::time::Duration::from_secs(120))?;
    let t = s.transcript();
    t[from.min(t.len())..]
        .lines()
        .find(|l| l.contains(pat))
        .map(|l| l.trim().to_string())
        .ok_or_else(|| format!("no line holding {pat:?}").into())
}

pub(crate) fn blk_in(line: &str) -> R<String> {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix("blk-"))
        .map(|n| n.trim_end_matches(|c: char| !c.is_ascii_digit()).to_string())
        .ok_or_else(|| format!("no `blk-<n>` in {line:?}").into())
}
