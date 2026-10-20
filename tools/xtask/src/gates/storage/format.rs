//! `check-storage`'s formatting: blank sticks given a filesystem, and read on the host.

use crate::*;

/// **Partitioning and formatting, through `with admin`**, after the sticks:
///
/// 4. **`disk --list`'s `note`** names the internal FAT's reason, `not removable, so not mounted`.
/// 5. **`disk --format` of a partition of the boot stick** is refused before anything is written,
///    naming the disk the machine started from.
/// 1. **A blank 2 GiB stick plugged in, then `disk --format DISK ext4`**: a GPT — the default for
///    ext4 — with one partition, which the kernel's rescan publishes, an ext4 in it, mounted
///    writable, written and unmounted. Copied then, for the host to read the GPT and the ext4.
/// 2. **`disk --partition DISK`**: an MBR, the GPT's partition departed and a new one arrived,
///    holding nothing — the old ext4 at 1 MiB wiped with the table.
/// 3. **`disk --format PART fat`**: a FAT32, read again and mounted writable, written; the same
///    command refused while it is mounted; ejected and pulled. The host checks it after the boot.
///
/// The note and the boot stick come first, since they need no stick of their own.
pub(crate) fn run_format_steps(s: &mut Session, qmp: &mut Qmp, work: &Path) -> R<()> {
    let secs = std::time::Duration::from_secs;
    let any = |_: &str| true;
    let boot = s.transcript();
    let report =
        |part: &str| boot.lines().find(|l| l.contains("storage-service: blk-") && l.contains(part)).map(str::to_string);
    let internal = report(&format!("(partition {INTERNAL_FAT_PARTLABEL}):")).ok_or("no report of the internal FAT")?;
    let internal = blk_in(&internal)?;
    // **A partition of the boot stick**, by its parent in `/dev/devices`: the report names the disk
    // as passed over, and its partition by what is in it — the live stick's ESP, by its clusters.
    let boot_disk = boot
        .lines()
        .find(|l| {
            l.contains("storage-service: blk-")
                && l.contains("(disk ")
                && l.contains("; on the disk the machine started from, passed over")
        })
        .ok_or("no report of the boot stick, passed over")?;
    let boot_disk = blk_in(boot_disk)?;
    s.send(&format!(
        "open /dev/devices/all.tsm | filter parent == \"blk-{boot_disk}\" | map {{ |r| format(\"boot-part=<{{}}>\", r.name) }}"
    ))?;
    s.expect("boot-part=<blk-")?;
    // Read once the prompt is back, so the line is whole.
    s.expect("/home>")?;
    let on_stick = {
        let t = s.transcript();
        let at = t.rfind("boot-part=<blk-").ok_or("no boot-part line")?;
        t[at + "boot-part=<blk-".len()..].split('>').next().unwrap_or("").to_string()
    };

    s.send(&format!(
        "disk --list | filter name == \"blk-{internal}\" | map {{ |r| format(\"note-is={{}}\", r.note) }}"
    ))?;
    s.expect("note-is=not removable, so not mounted")?;
    s.expect("/home>")?;
    println!("  ok: `disk --list`'s note says why the internal FAT is not mounted");

    // 5. The boot stick, refused before anything is written.
    let from = s.transcript().len();
    with_admin_asked(s, &format!("disk --format blk-{on_stick} fat"))?;
    s.expect(&format!(
        "disk: blk-{on_stick} not formatted: it is on the disk the machine started from; nothing was written"
    ))?;
    s.expect("/home>")?;
    if s.transcript()[from..].contains("disk: wrote ") {
        return Err(format!("`disk --format blk-{on_stick}` wrote to the boot stick").into());
    }
    println!(
        "  ok: a partition of the boot stick is refused before anything is written, naming the disk the machine started from"
    );

    // 1. The blank stick, formatted ext4 whole.
    let blank = work.join(BLANK_STICK);
    qmp.execute(&format!(
        r#"{{"execute":"blockdev-add","arguments":{{"driver":"raw","node-name":"stickblank","file":{{"driver":"file","filename":"{}"}}}}}}"#,
        blank.display()
    ))?;
    let from = s.transcript().len();
    qmp.execute(r#"{"execute":"device_add","arguments":{"driver":"usb-storage","id":"stickblank","bus":"xhci.0","drive":"stickblank","removable":true}}"#)?;
    let disk_line = line_after(s, from, "\", 4194304 blocks of 512 bytes, record ")?;
    let port = disk_line
        .strip_prefix("usb: port ")
        .and_then(|r| r.split(':').next())
        .ok_or_else(|| format!("no port in {disk_line:?}"))?
        .to_string();
    let disk = blk_in(&line_after(s, from, "): no filesystem")?)?;
    println!("  ok: a blank 2 GiB stick plugged in: blk-{disk}, holding nothing");
    let from = s.transcript().len();
    with_admin_asked(s, &format!("disk --format blk-{disk} ext4"))?;
    s.line_since(from, &format!("disk: wrote blk-{disk} ext4 (gpt)"), any, secs(300))?;
    s.line_since(
        from,
        &format!("usb: port {port}: LUN 0: rescanned: 0 partition(s) departed, 1 published"),
        any,
        secs(60),
    )?;
    let mounted = line_after(s, from, "(partition nitrox): ext4 'nitrox'; mounted at /storage/nitrox (rw)")?;
    let gpt_part = blk_in(&mounted)?;
    s.line_since(
        from,
        &format!("disk: blk-{disk} formatted ext4 (gpt): blk-{gpt_part}, mounted at /storage/nitrox"),
        any,
        secs(60),
    )?;
    s.expect("/home>")?;
    println!(
        "  ok: `disk --format blk-{disk} ext4`: a GPT, its partition published by a rescan, an ext4 mounted writable"
    );
    let at = format!("/storage/nitrox/{STORAGE_PATTERN_FILE}");
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"))?;
    s.expect("/home>")?;
    // **Unmounted, not ejected**: the next step partitions it, and an ejected stick is refused.
    with_admin_asked(s, "disk --unmount nitrox")?;
    s.expect("fs-server: unmounted, and the filesystem recorded clean")?;
    s.expect("storage-service: unmounted nitrox, left clean")?;
    s.expect("disk: unmounted nitrox")?;
    s.expect("/home>")?;
    fs::copy(&blank, work.join(BLANK_STICK_EXT4))?;
    println!("  ok: written without a sync, and unmounted");

    // 2. Partitioned again, MBR: the GPT's partition goes, and the new one holds nothing.
    let from = s.transcript().len();
    with_admin_asked(s, &format!("disk --partition blk-{disk}"))?;
    s.line_since(from, &format!("disk: wrote blk-{disk} (mbr)"), any, secs(120))?;
    s.line_since(
        from,
        &format!("usb: port {port}: LUN 0: rescanned: 1 partition(s) departed, 1 published"),
        any,
        secs(60),
    )?;
    s.line_since(from, &format!("storage-service: blk-{gpt_part} departed"), any, secs(60))?;
    let arrived = line_after(s, from, "(partition partition 1 (unlabelled)): no filesystem")?;
    let part = blk_in(&arrived)?;
    s.line_since(from, &format!("disk: blk-{disk} partitioned (mbr): blk-{part}, holding nothing"), any, secs(60))?;
    s.expect("/home>")?;
    println!(
        "  ok: `disk --partition blk-{disk}`: an MBR, the GPT's blk-{gpt_part} departed, blk-{part} arrived holding nothing"
    );

    // 3. The partition formatted FAT, written, refused while mounted, ejected, pulled.
    let from = s.transcript().len();
    with_admin_asked(s, &format!("disk --format blk-{part} fat"))?;
    s.line_since(from, &format!("disk: wrote blk-{part} fat"), any, secs(300))?;
    line_after(
        s,
        from,
        &format!("blk-{part} (partition partition 1 (unlabelled)): fat 'NITROX'; mounted at /storage/NITROX (rw)"),
    )?;
    s.line_since(from, &format!("disk: blk-{part} formatted fat: mounted at /storage/NITROX"), any, secs(60))?;
    s.expect("/home>")?;
    println!("  ok: `disk --format blk-{part} fat`: read again and mounted writable by fs-server-fat");
    let at = format!("/storage/NITROX/{STORAGE_PATTERN_FILE}");
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"))?;
    s.expect("/home>")?;
    let from = s.transcript().len();
    with_admin_asked(s, &format!("disk --format blk-{part} fat"))?;
    s.expect(&format!(
        "disk: blk-{part} not formatted: it is mounted, as NITROX: unmount it first (with admin disk --unmount NAME); nothing was written"
    ))?;
    s.expect("/home>")?;
    if s.transcript()[from..].contains("disk: wrote ") {
        return Err(format!("`disk --format blk-{part}` wrote while it was mounted").into());
    }
    println!("  ok: the same command, while it is mounted, is refused before anything is written");
    let ejecting = crate::gates::media::before_eject(s, work);
    eject_stick(s, &["NITROX"])?;
    crate::gates::media::stopped_and_left_alone(s, qmp, work, "stickblank", ejecting)?;
    let from = s.transcript().len();
    qmp.execute(r#"{"execute":"device_del","arguments":{"id":"stickblank"}}"#)?;
    s.line_since(from, &format!("storage-service: blk-{part} departed"), any, secs(30))?;
    println!("  ok: written, ejected and pulled");
    Ok(())
}

/// **`len` bytes of `from` at `at`, copied into `to`** — sparsely, since most of a stick is never
/// written.
fn carve_sparse(from: &Path, at: u64, len: u64, to: &Path) -> R<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    const MIB: u64 = 1024 * 1024;
    let mut src = fs::File::open(from)?;
    src.seek(SeekFrom::Start(at))?;
    let mut dst = fs::File::create(to)?;
    let mut buf = vec![0u8; MIB as usize];
    let mut done = 0;
    while done < len {
        let n = (len - done).min(MIB) as usize;
        src.read_exact(&mut buf[..n])?;
        if buf[..n].iter().any(|&b| b != 0) {
            dst.write_all(&buf[..n])?;
        } else {
            dst.seek(SeekFrom::Current(n as i64))?;
        }
        done += n as u64;
    }
    dst.set_len(len)?;
    Ok(())
}

/// **The stick partitioned and formatted in the guest, on the host**:
/// - **as its ext4 was unmounted** (`ext4_copy`): a GPT whose one partition is a Linux filesystem
///   from 1 MiB to the last usable sector, holding an ext4 `e2fsck -fn` finds clean, with the
///   pattern `debugfs` reads;
/// - **as it was pulled** (`stick`): an MBR naming one FAT32 partition from 1 MiB to the end, **no
///   GPT left** — neither header, front nor back — and the FAT `fsck.fat -n` finds clean, its
///   pattern read by `mtype`.
pub(crate) fn check_formatted_stick(ext4_copy: &Path, stick: &Path, work: &Path) -> R<()> {
    use std::os::unix::fs::FileExt;
    let sectors = fs::metadata(stick)?.len() / 512;
    let sector = |path: &Path, lba: u64| -> R<[u8; 512]> {
        let mut b = [0u8; 512];
        fs::File::open(path)?.read_exact_at(&mut b, lba * 512)?;
        Ok(b)
    };
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();

    let header = sector(ext4_copy, 1)?;
    if &header[..8] != b"EFI PART" {
        return Err("the stick formatted ext4 has no GPT at LBA 1".into());
    }
    let entry = sector(ext4_copy, 2)?;
    let linux = [0xAFu8, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4];
    let first = u64::from_le_bytes(entry[32..40].try_into()?);
    let last = u64::from_le_bytes(entry[40..48].try_into()?);
    if entry[..16] != linux || first != 2048 || last != sectors - 34 {
        return Err(format!(
            "the GPT's first entry is not a Linux filesystem from 2048 to {}: {first}..{last}",
            sectors - 34
        )
        .into());
    }
    // **Every other entry unused**: entries are 128 bytes, four to a sector, so the
    // second is at LBA 2 + 128 — sixteen zero bytes at LBA 3 are the fifth's type.
    let mut array = vec![0u8; 128 * 128];
    fs::File::open(ext4_copy)?.read_exact_at(&mut array, 2 * 512)?;
    if let Some(n) = (1..128).find(|&n| array[n * 128..n * 128 + 16] != [0u8; 16]) {
        return Err(format!("the GPT has a partition in entry {n}, beside the one `disk --format` wrote").into());
    }
    println!(
        "  ok: the ext4 format's table is a GPT, one Linux filesystem partition from 1 MiB to the last usable sector"
    );
    let part = work.join("stick-blank-ext4-part.img");
    carve_sparse(ext4_copy, first * 512, (last - first + 1) * 512, &part)?;
    check_ext4_clean(&part, "the stick's ext4")?;
    match debugfs_cat(&part, &format!("/{STORAGE_PATTERN_FILE}"))? {
        Some(b) if b == pattern => println!("  ok: its ext4 holds the pattern, read with debugfs"),
        _ => return Err("the stick's ext4 does not hold the pattern: the eject did not write it back".into()),
    }

    let mbr = sector(stick, 0)?;
    let e = &mbr[0x1BE..0x1CE];
    let (kind, first, count) =
        (e[4], u32::from_le_bytes(e[8..12].try_into()?) as u64, u32::from_le_bytes(e[12..16].try_into()?) as u64);
    if mbr[510..512] != [0x55, 0xAA] || kind != 0x0C || first != 2048 || count != sectors - 2048 {
        return Err(format!(
            "the stick's MBR is not one FAT32 partition from 2048 to the end: type {kind:#04x}, {first}+{count}"
        )
        .into());
    }
    if mbr[0x1CE..0x1FE].iter().any(|&b| b != 0) {
        return Err("the stick's MBR names more than one partition".into());
    }
    if &sector(stick, 1)?[..8] == b"EFI PART" || &sector(stick, sectors - 1)?[..8] == b"EFI PART" {
        return Err("a GPT header outlived the MBR: the kernel could read the stale table".into());
    }
    println!(
        "  ok: the stick's table is an MBR, one FAT32 partition from 1 MiB, and no GPT header is left at either end"
    );
    let fat = work.join("stick-blank-fat-part.img");
    carve_fat_clean(stick, &fat, "the stick's FAT")?;
    let out = mtools_cmd("mtype").arg("-i").arg(&fat).arg(format!("::{STORAGE_PATTERN_FILE}")).output()?;
    if out.stdout != pattern {
        return Err("mtype does not read the pattern off the stick's FAT: the eject did not write it back".into());
    }
    let info = mtools_cmd("minfo").arg("-i").arg(&fat).arg("::").output()?;
    let info = String::from_utf8_lossy(&info.stdout);
    if !info.contains("FAT32") {
        return Err(format!("the stick's FAT is not a FAT32:\n{info}").into());
    }
    println!("  ok: its FAT32 holds the pattern, read with mtype");
    Ok(())
}
