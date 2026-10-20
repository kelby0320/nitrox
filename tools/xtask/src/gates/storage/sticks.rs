//! `check-storage`'s sticks: mounted, written, renamed and ejected, then read on the host.

use crate::*;

/// **Sticks that come and go**, after the disk's steps, plugged and pulled over
/// QMP. Each wait reads from the moment of its action, since the kernel, the device manager and the
/// storage service each answer one and their lines land in any order.
///
/// 1. **The boot stick is passed over**: the kernel flags it, and the service says so.
/// 2. **An MBR stick plugged in**: its disk and partition read in the kernel, the partition's ext4
///    auto-mounted **writable**, on a live boot too, and written through a mapping without
///    a sync. **An eject while a file on it is held is refused**, and `disk --eject` then ejects it
///    with no password, and a read of it is refused by the kernel. Pulled then, its records depart
///    and the service drops it.
/// 3. **A whole-disk stick pulled while mounted** writable, with a file left dirty: the teardown's
///    write-back and the server's marking each come back at once, the dirty file let go, the server
///    gone, the label gone, and a command after it runs.
/// 4. **The same stick plugged in again** is a new disk at a new index, mounted again.
pub(crate) fn run_stick_steps(s: &mut Session, qmp: &mut Qmp, work: &Path) -> R<()> {
    let secs = std::time::Duration::from_secs;
    let any = |_: &str| true;
    let line_of = |s: &mut Session, from: usize, pat: &str| -> R<String> {
        s.line_since(from, pat, |_: &str| true, std::time::Duration::from_secs(60))?;
        let t = s.transcript();
        t[from.min(t.len())..]
            .lines()
            .find(|l| l.contains(pat))
            .map(|l| l.trim().to_string())
            .ok_or_else(|| format!("no line holding {pat:?}").into())
    };
    let blk_of = |line: &str| -> R<String> {
        line.split_whitespace()
            .find_map(|w| w.strip_prefix("blk-"))
            .map(|n| n.trim_end_matches(|c: char| !c.is_ascii_digit()).to_string())
            .ok_or_else(|| format!("no `blk-<n>` in {line:?}").into())
    };
    let plug = |qmp: &mut Qmp, id: &str, node: &str| -> R<()> {
        qmp.execute(&format!(
            r#"{{"execute":"device_add","arguments":{{"driver":"usb-storage","id":"{id}","bus":"xhci.0","drive":"{node}","removable":true}}}}"#
        ))?;
        Ok(())
    };
    let pull = |qmp: &mut Qmp, id: &str| -> R<()> {
        qmp.execute(&format!(r#"{{"execute":"device_del","arguments":{{"id":"{id}"}}}}"#))?;
        Ok(())
    };
    let add_node = |qmp: &mut Qmp, node: &str, file: &Path| -> R<()> {
        qmp.execute(&format!(
            r#"{{"execute":"blockdev-add","arguments":{{"driver":"raw","node-name":"{node}","file":{{"driver":"file","filename":"{}"}}}}}}"#,
            file.display()
        ))?;
        Ok(())
    };

    // 1. The boot stick: flagged by the kernel, passed over by the service.
    let boot = s.transcript();
    if !boot.contains("gpt: the disk the machine started from") {
        return Err("the kernel did not flag the live stick as the disk the machine started from".into());
    }
    if !boot.lines().any(|l| {
        l.contains("storage-service: blk-") && l.contains("; on the disk the machine started from, passed over")
    }) {
        return Err("the storage service did not say it passed the boot stick over".into());
    }
    println!("  ok: the boot stick is flagged, and passed over");

    // 1b. **The internal FAT is not mounted, and the release disk's ESP not served**: the first
    //     servable and on a disk that is not removable, the second refused for its 512-byte
    //     clusters. Read from the boot's report, as the boot stick's line is.
    let internal = format!(
        "(partition {INTERNAL_FAT_PARTLABEL}): fat '{INTERNAL_FAT_LABEL}', left clean; not removable, so not mounted"
    );
    if !boot.lines().any(|l| l.contains("storage-service: blk-") && l.contains(&internal)) {
        return Err(format!("no report of the internal FAT left unmounted: {internal:?}").into());
    }
    let esp =
        "(partition NITROX_ESP): fat 'NITROX_ESP', left clean; not served: 512-byte clusters, smaller than a page";
    if !boot.lines().any(|l| l.contains("storage-service: blk-") && l.contains(esp)) {
        return Err(format!("no report of the release disk's ESP refused for its clusters: {esp:?}").into());
    }
    println!(
        "  ok: the internal FAT is servable and not mounted, not being removable; the ESP is refused for its clusters"
    );

    // 2. The MBR stick.
    let mbr_img = build_cache().join("check-storage").join("stick-mbr.img");
    add_node(qmp, "stickmbr", &mbr_img)?;
    let from = s.transcript().len();
    plug(qmp, "stickmbr", "stickmbr")?;
    let disk_line = line_of(s, from, ": LUN 0: disk \"QEMU QEMU HARDDISK (")?;
    let port = disk_line
        .strip_prefix("usb: port ")
        .and_then(|r| r.split(':').next())
        .ok_or_else(|| format!("no port in {disk_line:?}"))?
        .to_string();
    s.line_since(from, "mbr:  partition 1 lba 2048..", any, secs(60))?;
    let mounted = line_of(
        s,
        from,
        &format!(
            "(partition partition 1 (unlabelled)): ext4 '{STICK_MBR_LABEL}'; mounted at /storage/{STICK_MBR_LABEL} (rw)"
        ),
    )?;
    let index = blk_of(&mounted)?;
    line_of(
        s,
        from,
        &format!(
            "(partition partition 2 (unlabelled)): ext4 '{STICK_MBR_LABEL_2}'; mounted at /storage/{STICK_MBR_LABEL_2} (rw)"
        ),
    )?;
    println!("  ok: an MBR stick plugged in: its two partitions read, each ext4 mounted writable on a live boot");
    // A pattern on each, written through a mapping and not synced, for the eject to write back.
    let ats = [STICK_MBR_LABEL, STICK_MBR_LABEL_2].map(|l| format!("/storage/{l}/{STORAGE_PATTERN_FILE}"));
    for at in &ats {
        s.send(&format!("test-pattern --write {at}"))?;
        s.expect(&format!(
            "test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"
        ))?;
        s.expect("/home>")?;
    }
    // **Refused while a file on the stick is held** — on its *other* partition: a session's eject
    // takes the whole stick, so a file held on either refuses it. `test-pattern` holds the file
    // and asks, since a shell with no background jobs cannot hold one while it runs `disk`.
    s.send(&format!("test-pattern --eject-held {} {STICK_MBR_LABEL}", ats[1]))?;
    s.expect(&format!("storage-service: {STICK_MBR_LABEL_2} is in use: 1 file(s) still open or mapped"))?;
    s.expect(&format!(
        "test-pattern: eject of {STICK_MBR_LABEL} with {} held: refused, a file on it is still open or mapped ok",
        ats[1]
    ))?;
    s.expect("/home>")?;
    // **And nothing was ejected**: the partition named is still mounted, not unmounted before the
    // other one refused. A token the typed line does not hold, so the match is the answer.
    s.send(&format!("format(\"still-mounted={{}}\", (list /storage | filter name == \"{STICK_MBR_LABEL}\" | count))"))?;
    s.expect("still-mounted=1")?;
    s.expect("/home>")?;
    println!("  ok: an eject of one partition while a file on the other is held is refused, and leaves both mounted");
    let ejecting = crate::gates::media::before_eject(s, work);
    eject_stick(s, &[STICK_MBR_LABEL, STICK_MBR_LABEL_2])?;
    println!("  ok: written without a sync, and both partitions ejected by one `disk --eject`, no password asked");
    // **Listed to be removed, and read no more**: the table's read probes every unmounted device.
    s.send(&format!("disk --list | filter name == \"blk-{index}\" | map {{ |r| format(\"note-is={{}}\", r.note) }}"))?;
    s.expect("note-is=ejected; remove it")?;
    s.expect("/home>")?;
    crate::gates::media::stopped_and_left_alone(s, qmp, work, "stickmbr", ejecting)?;
    // **And the kernel refuses what does ask**, as the service asks nothing.
    crate::gates::interactive::with_admin_asked(s, &format!("test-pattern --read-device /dev/blk/{index}"))?;
    s.expect_all(&[
        "ejected, and asked for more; refused until plugged in again",
        &format!("test-pattern: a read of /dev/blk/{index}: refused: PeerClosed"),
    ])?;
    s.expect("/home>")?;
    let log = fs::read_to_string(work.join("scsi.log")).unwrap_or_default();
    let eject = crate::scsi_trace::eject_traced(&log, ejecting.1).ok_or("the stick's eject is no longer traced")?;
    if crate::scsi_trace::not_ready_after(&log, eject) {
        return Err("the read reached the ejected stick: the kernel let it through".into());
    }
    println!("  ok: a read of the ejected stick is refused, and nothing reached it");
    let from = s.transcript().len();
    pull(qmp, "stickmbr")?;
    s.line_since(from, &format!("usb: port {port}: disconnected; slot "), any, secs(30))?;
    s.line_since(from, &format!("storage-service: blk-{index} departed"), any, secs(30))?;
    println!("  ok: pulled, its records departed, and the service let it go");

    // 3. The whole-disk stick, pulled while mounted.
    let whole_img = build_cache().join("check-storage").join("stick-whole.img");
    add_node(qmp, "stickwhole", &whole_img)?;
    let from = s.transcript().len();
    plug(qmp, "stickwhole", "stickwhole")?;
    let disk_line = line_of(s, from, ": LUN 0: disk \"QEMU QEMU HARDDISK (")?;
    let port = disk_line
        .strip_prefix("usb: port ")
        .and_then(|r| r.split(':').next())
        .ok_or_else(|| format!("no port in {disk_line:?}"))?
        .to_string();
    s.line_since(from, "partitions: no table: neither a GPT nor an MBR", any, secs(60))?;
    let mounted =
        line_of(s, from, &format!("): ext4 '{STICK_WHOLE_LABEL}'; mounted at /storage/{STICK_WHOLE_LABEL} (rw)"))?;
    let whole = blk_of(&mounted)?;
    println!("  ok: a whole-disk stick plugged in, read as no table, and its ext4 mounted writable as the disk");
    let at = format!("/storage/{STICK_WHOLE_LABEL}/{STORAGE_PATTERN_FILE}");
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"))?;
    s.expect("/home>")?;
    let from = s.transcript().len();
    pull(qmp, "stickwhole")?;
    s.line_since(from, &format!("usb: port {port}: disconnected; slot "), any, secs(30))?;
    // **The teardown's I/O to a disk that has gone comes back at once**: the write-back of the
    // file left dirty, whose refusal lets it go, and the server's attempt to record it clean.
    s.line_since(from, "file: its device has gone; a dirty file and its ", any, secs(30))?;
    s.line_since(from, "fs-server: unmounted, but the filesystem could not be recorded clean", any, secs(30))?;
    s.line_since(
        from,
        &format!("storage-service: {STICK_WHOLE_LABEL} left while mounted; what had not been written back is gone"),
        any,
        secs(30),
    )?;
    s.line_since(from, &format!("storage-service: blk-{whole} departed"), any, secs(30))?;
    // **The stick as the pull left it, copied now**, for the host to find still marked in use: the
    // same stick plugged in again below is mounted writable, which marks it in use whatever the
    // pull did.
    fs::copy(&whole_img, whole_img.with_file_name("stick-whole-pulled.img"))?;
    let from = s.transcript().len();
    s.send("list /storage")?;
    s.expect("/home>")?;
    if s.transcript()[from..].lines().skip(1).any(|l| l.contains(STICK_WHOLE_LABEL)) {
        return Err(format!("/storage still lists {STICK_WHOLE_LABEL} after its stick was pulled").into());
    }
    println!(
        "  ok: pulled while mounted: the dirty file let go, the server gone, the label gone, the session going on"
    );

    // 4. Plugged in again: a new disk, a new index, mounted again.
    let from = s.transcript().len();
    plug(qmp, "stickwhole2", "stickwhole")?;
    let mounted =
        line_of(s, from, &format!("): ext4 '{STICK_WHOLE_LABEL}'; mounted at /storage/{STICK_WHOLE_LABEL} (rw)"))?;
    let again = blk_of(&mounted)?;
    if again.parse::<u32>().ok() <= whole.parse::<u32>().ok() {
        return Err(format!("plugged in again, the stick took blk-{again}, not an index past blk-{whole}").into());
    }
    println!("  ok: plugged in again: blk-{again}, a new index, mounted again");

    // 5. **A FAT stick**, served by `fs-server-fat`: auto-mounted writable, on a live boot
    //    too, and what the host put there listed and read.
    let fat_img = build_cache().join("check-storage").join("stick-fat.img");
    add_node(qmp, "stickfat", &fat_img)?;
    let from = s.transcript().len();
    plug(qmp, "stickfat", "stickfat")?;
    let mounted = line_of(
        s,
        from,
        &format!(
            "(partition partition 1 (unlabelled)): fat '{STICK_FAT_LABEL}'; mounted at /storage/{STICK_FAT_LABEL} (rw)"
        ),
    )?;
    let fat = blk_of(&mounted)?;
    s.line_since(from, "fs-server: ready (fat, read-write)", any, secs(30))?;
    println!("  ok: a FAT stick plugged in, its FAT32 mounted writable by fs-server-fat at /storage/{STICK_FAT_LABEL}");
    let root = format!("/storage/{STICK_FAT_LABEL}");
    // **What a command printed**: everything after its echo, up to the next prompt.
    let output = |s: &mut Session, command: &str| -> R<String> {
        let from = s.transcript().len();
        s.send(command)?;
        s.expect("/home>")?;
        let t = s.transcript();
        Ok(t[from.min(t.len())..].lines().skip(1).filter(|l| !l.contains("/home>")).collect::<Vec<_>>().join("\n"))
    };
    let listing = |s: &mut Session, dir: &str| -> R<String> {
        output(s, &format!("list {dir} | map {{ |r| format(\"entry={{}}|{{}}|{{}}\", r.name, r.kind, r.size) }}"))
    };
    let names = listing(s, &root)?;
    for (name, kind, size) in [
        (FAT_LONG_NAME, "file", fat_host_bytes(FAT_LONG_NAME).len()),
        (FAT_UNICODE_NAME, "file", fat_host_bytes(FAT_UNICODE_NAME).len()),
        ("nested", "dir", 0),
        (STORAGE_PATTERN_FILE, "file", STORAGE_PATTERN_LEN),
        (FAT_DOOMED, "file", fat_host_bytes(FAT_DOOMED).len()),
    ] {
        if !names.contains(&format!("entry={name}|{kind}|{size}")) {
            return Err(format!("`list {root}` does not show {name:?}, a {kind} of {size} bytes:\n{names}").into());
        }
    }
    let nested = listing(s, &format!("{root}/{FAT_NESTED_DIR}"))?;
    let want = format!("entry={FAT_NESTED_NAME}|file|{}", fat_host_bytes(FAT_NESTED_NAME).len());
    if !nested.contains(&want) {
        return Err(format!("`list {root}/{FAT_NESTED_DIR}` does not show {want:?}:\n{nested}").into());
    }
    let at = format!("{root}/{STORAGE_PATTERN_FILE}");
    s.send(&format!("test-pattern --check {at}"))?;
    s.expect(&format!("test-pattern: {at} holds the pattern, {STORAGE_PATTERN_LEN} bytes ok"))?;
    s.expect("/home>")?;
    println!(
        "  ok: the host's names listed — long, Unicode, nested — with their sizes, and its pattern read through a mapping"
    );

    // 6. **Written**, with no remount: a directory made, the Unicode-named file copied into it
    //    under a longer name built from its own — the serial line carries ASCII alone, so the name
    //    comes from the listing — the long-named one renamed into it, one removed, and a file
    //    written through a mapping without a sync.
    let made = format!("{root}/{FAT_MADE_DIR}");
    for command in [
        format!("mkdir {made}"),
        format!(
            "for r in (list {root} | filter {{ |r| r.name ~= /^na/ }}) {{ copy --force (format(\"{root}/{{}}\", r.name)) (format(\"{made}/a copy of {{}}\", r.name)) }}"
        ),
        format!("rename --force (\"{root}/{FAT_LONG_NAME}\") (\"{made}/{FAT_RENAMED}\")"),
        format!("remove {root}/{FAT_DOOMED}"),
    ] {
        // A refusal is a line naming the program, or the shell's own; what each prints on success
        // (`mkdir`'s table of what it created, say) is the host's to judge, after the eject.
        let said = output(s, &command)?;
        let program =
            command.split_whitespace().find(|w| ["mkdir", "copy", "rename", "remove"].contains(w)).unwrap_or("nxsh");
        if said.lines().any(|l| l.contains(&format!("{program}: ")) || l.contains("nxsh: ")) {
            return Err(format!("`{command}` was refused:\n{said}").into());
        }
    }
    let at = format!("{root}/{FAT_WRITTEN}");
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"))?;
    s.expect("/home>")?;
    let made_names = listing(s, &made)?;
    let copied = format!("entry=a copy of {FAT_UNICODE_NAME}|file|{}", fat_host_bytes(FAT_UNICODE_NAME).len());
    if !made_names.contains(&copied) || !made_names.contains(&format!("entry={FAT_RENAMED}|file|")) {
        return Err(format!("`list {made}` is not the copy and the rename:\n{made_names}").into());
    }
    println!(
        "  ok: a directory, a copy to a long Unicode name, a rename and a removal; a file written through a mapping"
    );

    // 7. **Ejected and pulled**: the eject writes back and records the filesystem clean. First a
    //    program is run from the stick: the shell closes a stage's image once it is spawned, so
    //    nothing it ran holds a file there, and the eject is not refused as in use.
    s.send(&format!("copy /bin/whoami {root}/w"))?;
    s.expect("/home>")?;
    s.send(&format!("{root}/w"))?;
    s.expect("alice")?;
    s.expect("/home>")?;
    let ejecting = crate::gates::media::before_eject(s, work);
    eject_stick(s, &[STICK_FAT_LABEL])?;
    crate::gates::media::stopped_and_left_alone(s, qmp, work, "stickfat", ejecting)?;
    let from = s.transcript().len();
    pull(qmp, "stickfat")?;
    s.line_since(from, &format!("storage-service: blk-{fat} departed"), any, secs(30))?;
    println!("  ok: ejected, recorded clean, and pulled");
    Ok(())
}

/// **`disk --eject NAME`, as a session runs it**: no `with`, and no password asked. The chain
/// writes back and records the filesystem clean before the service says the stick can be pulled.
///
/// **The drive goes whole**: `names` are every filesystem on the stick, the first the one named,
/// and each is unmounted, in the order mounted, before the service and `disk` name them all.
pub(crate) fn eject_stick(s: &mut Session, names: &[&str]) -> R<()> {
    let from = s.transcript().len();
    s.send(&format!("disk --eject {}", names[0]))?;
    for name in names {
        s.expect("fs-server: unmounted, and the filesystem recorded clean")?;
        s.expect(&format!("storage-service: unmounted {name}, left clean"))?;
    }
    let all = names.join(", ");
    s.expect(&format!("storage-service: ejected {all}, safe to remove"))?;
    // **In either order**: the service replies before it stops the stick.
    s.expect_all(&[&format!("storage-service: {all} stopped"), &format!("disk: ejected {all}"), "/home>"])?;
    if s.transcript()[from..].contains("password:") {
        return Err(format!("`disk --eject {}` asked for a password", names[0]).into());
    }
    Ok(())
}

/// The sticks, once the machine has stopped: the ejected one's partitions carved out by their
/// MBR, `e2fsck -fn` clean, `s_state` clean and the pattern read with `debugfs`; the pulled one,
/// copied as the pull left it, still marked in use — what an unplug without an eject leaves.
pub(crate) fn check_sticks(mbr: &Path, whole: &Path, work: &Path) -> R<()> {
    use std::io::{Read, Seek, SeekFrom};
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();
    for ((first, count), label) in STICK_MBR_PARTS.into_iter().zip([STICK_MBR_LABEL, STICK_MBR_LABEL_2]) {
        let part = work.join(format!("stick-mbr-{label}.ext4"));
        let mut f = fs::File::open(mbr)?;
        f.seek(SeekFrom::Start(first as u64 * 512))?;
        let mut buf = vec![0u8; count as usize * 512];
        f.read_exact(&mut buf)?;
        fs::write(&part, buf)?;
        check_ext4_clean(&part, &format!("the ejected stick's {label}"))?;
        match debugfs_cat(&part, &format!("/{STORAGE_PATTERN_FILE}"))? {
            Some(b) if b == pattern => {
                println!("  ok: the ejected stick's {label} holds the pattern, read with debugfs")
            }
            Some(b) => {
                return Err(format!("the ejected stick's {label} file is {} bytes and not the pattern", b.len()).into());
            }
            None => {
                return Err(format!(
                    "the ejected stick's {label} has no pattern file: the eject did not write it back"
                )
                .into());
            }
        }
    }
    let state = ext4_s_state(whole)?;
    if state & EXT4_VALID_FS != 0 {
        return Err(format!(
            "the stick pulled while mounted writable records itself clean (s_state {state:#06x}): \
             nothing should have been able to mark it"
        )
        .into());
    }
    println!("  ok: the stick pulled while mounted is still marked in use (s_state {state:#06x})");
    Ok(())
}

/// **A stick's FAT partition carved out** — sparsely, at the MBR's 1 MiB — to `part`, and required
/// clean: `fsck.fat -n` finds nothing, and the dirty bit is clear.
pub(crate) fn carve_fat_clean(stick: &Path, part: &Path, what: &str) -> R<()> {
    use std::io::{Read, Seek, SeekFrom, Write};
    const MIB: u64 = 1024 * 1024;
    let mut from = fs::File::open(stick)?;
    let len = from.metadata()?.len() - MIB;
    from.seek(SeekFrom::Start(MIB))?;
    let mut to = fs::File::create(part)?;
    let mut buf = vec![0u8; MIB as usize];
    for _ in 0..len / MIB {
        from.read_exact(&mut buf)?;
        if buf.iter().any(|&b| b != 0) {
            to.write_all(&buf)?;
        } else {
            to.seek(SeekFrom::Current(MIB as i64))?;
        }
    }
    to.set_len(len)?;
    drop(to);
    let out = Command::new("fsck.fat").arg("-n").arg(part).output()?;
    if !out.status.success() {
        return Err(format!(
            "fsck.fat -n does not find {what} clean:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    let mut boot = [0u8; 512];
    fs::File::open(part)?.read_exact(&mut boot)?;
    if boot[0x41] & 1 != 0 {
        return Err(format!("{what}'s dirty bit is set after its unmount").into());
    }
    println!("  ok: fsck.fat -n finds {what} clean, its dirty bit clear");
    Ok(())
}

/// **The FAT stick, once the machine has stopped**: its partition carved out,
/// `fsck.fat -n` clean — the dirty bit among what it checks — and mtools reading what the guest
/// did: the names it made, long and Unicode, the host's file under its new name, the one removed
/// gone, the copy's bytes, and the pattern it wrote through a mapping.
pub(crate) fn check_fat_stick(stick: &Path, work: &Path) -> R<()> {
    let part = work.join("stick-fat-part.img");
    carve_fat_clean(stick, &part, "the FAT stick")?;
    let at = format!("{}", part.display());
    let listed = mtools_cmd("mdir").arg("-i").arg(&at).arg("-/").arg("-b").arg("::").output()?;
    let listed: std::collections::BTreeSet<String> = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix("::/"))
        .map(str::to_string)
        .collect();
    let want: std::collections::BTreeSet<String> = [
        FAT_UNICODE_NAME.to_string(),
        "nested/".to_string(),
        format!("{FAT_NESTED_DIR}/"),
        format!("{FAT_NESTED_DIR}/{FAT_NESTED_NAME}"),
        STORAGE_PATTERN_FILE.to_string(),
        format!("{FAT_MADE_DIR}/"),
        format!("{FAT_MADE_DIR}/a copy of {FAT_UNICODE_NAME}"),
        format!("{FAT_MADE_DIR}/{FAT_RENAMED}"),
        FAT_WRITTEN.to_string(),
        // The program run from the stick before its eject.
        "w".to_string(),
    ]
    .into_iter()
    .collect();
    if listed != want {
        return Err(format!("mdir lists the FAT stick as\n{listed:#?}\nwhere the guest left\n{want:#?}").into());
    }
    let read = |path: &str| -> R<Vec<u8>> {
        let out = mtools_cmd("mtype").arg("-i").arg(&at).arg(format!("::{path}")).output()?;
        if !out.status.success() {
            return Err(format!("mtype could not read {path:?} off the FAT stick").into());
        }
        Ok(out.stdout)
    };
    if read(&format!("{FAT_MADE_DIR}/a copy of {FAT_UNICODE_NAME}"))? != fat_host_bytes(FAT_UNICODE_NAME) {
        return Err("the guest's copy of the Unicode-named file does not hold its bytes".into());
    }
    if read(&format!("{FAT_MADE_DIR}/{FAT_RENAMED}"))? != fat_host_bytes(FAT_LONG_NAME) {
        return Err("the renamed file does not hold the long-named file's bytes".into());
    }
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();
    if read(FAT_WRITTEN)? != pattern {
        return Err(format!(
            "{FAT_WRITTEN} on the FAT stick does not hold the pattern: the eject did not write it back"
        )
        .into());
    }
    println!("  ok: mdir lists the guest's names, long and Unicode; mtype reads the copy, the rename and the pattern");
    Ok(())
}

/// The disk, once the machine has stopped: `e2fsck -fn` clean, the superblock marked clean, and
/// the file holding the pattern.
pub(crate) fn check_storage_disk(disk: &Path, work: &Path) -> R<()> {
    let fs_img = storage_root_fs(disk, work)?;
    check_left_clean(&fs_img)?;
    let path = format!("/{STORAGE_PATTERN_FILE}");
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();
    match debugfs_cat(&fs_img, &path)? {
        Some(b) if b == pattern => {}
        Some(b) => {
            let first = (0..STORAGE_PATTERN_LEN).find(|&i| b.get(i) != Some(&pattern[i]));
            return Err(format!(
                "{path} on the disk is {} bytes and is not the pattern (first difference at \
                 byte {first:?}): the unmount did not write the mapped pages back",
                b.len()
            )
            .into());
        }
        None => return Err(format!("{path} is not on the disk").into()),
    }
    println!("  ok: {path} holds the pattern, {STORAGE_PATTERN_LEN} bytes, read by debugfs");
    Ok(())
}
