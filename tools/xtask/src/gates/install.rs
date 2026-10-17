//! `check-install`: installed to a blank disk from the live menu, then that disk booted.

use crate::*;

/// The target disk's model and serial: a name of its own choosing, so what `nxinstall` prints can
/// only have come from `ahci::identify`.
const TARGET_MODEL: &str = "NITROX-TEST-DISK";

const TARGET_SERIAL: &str = "INSTALL01";

/// The RAM disk `install-root.img` becomes: where the installer is pointed to prove it refuses one.
const RAMDISK_IDENTITY: &str = "module 3 (/boot/install-root.img)";

/// The new machine's first account, one the build does not make, so a login as it can only have
/// come from what the installer wrote.
const INSTALL_ACCOUNT: &str = "dana";

/// Its password: letters and spaces, as it is typed on PS/2 at a terminal and at a greeter.
const INSTALL_PASSWORD: &str = "a quiet morning by the lake";

/// The blank disk's size: quick to write under TCG, with room for a 33 MiB boot partition and a
/// root.
const TARGET_MIB: u64 = 512;

/// `cargo xtask check-install` — reinstall onto a disk holding an install, from an ordinary
/// session, then boot that disk on its own.
///
/// On demand, like `check-resolutions`: two boots and a 512 MiB image. The first boot drives the
/// laptop path (live menu's third entry, graphical greeter, a terminal, `with admin nxinstall`) and
/// asserts on the kernel log, since a release image does not narrate the terminal's grid. The
/// target is a copy of the release disk, so `disks` withholds its mounted `nitrox-root` until `with
/// admin disk --unmount` frees it. The second boot, the disk alone, is the assertion that matters.
pub(crate) fn cmd_check_install(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    let work = build_cache();
    fs::create_dir_all(&work).ok();

    // A copy of the release disk holding an install, as a reinstall meets, grown to `TARGET_MIB` (a
    // root on the 128 MiB disk is one block group, which `check_installed_root`'s write past group
    // 0 cannot cross). Made fresh, or an install that wrote nothing would pass on an earlier run's
    // bytes.
    cmd_image(BuildMode::Normal)?;
    let target = work.join("install-target.img");
    let _ = fs::remove_file(&target);
    fs::copy(image_path(), &target)?;
    fs::OpenOptions::new().write(true).open(&target)?.set_len(TARGET_MIB * 1024 * 1024)?;
    cmd_image_live()?;
    let ovmf = locate_ovmf()?;

    let identity = format!("{TARGET_MODEL} ({TARGET_SERIAL})");
    let qmp_sock = work.join("qmp-install.sock");
    let _ = fs::remove_file(&qmp_sock);
    let dump = work.join("install.ppm");

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-device")
        .arg(XHCI_DEVICE)
        .arg("-drive")
        .arg(format!("if=none,id=stick,format=raw,file={}", live_image_path().display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=stick")
        .arg("-drive")
        .arg(format!("if=none,id=target,format=raw,file={}", target.display()))
        .arg("-device")
        .arg(format!("ide-hd,drive=target,bus=ide.0,model={TARGET_MODEL},serial={TARGET_SERIAL}"))
        .arg("-display")
        .arg("none")
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
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
    println!("xtask: install gate — booting the live image's installer entry beside a disk holding an install…\n");
    let mut session = Session::spawn(cmd, "check-install")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;
    let result = run_install_steps(&mut session, &mut qmp, &dump, &identity);
    let transcript = session.finish();
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    // The refusal held: step 7 aimed the installer at the RAM disk, correctly named, so only its
    // kind could refuse it. The absence means something because the install that followed
    // succeeded.
    if let Some(line) =
        transcript.lines().find(|l| l.contains("nxinstall: installing to") && !l.contains("installing to /dev/blk/0 "))
    {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(format!(
            "the installer wrote to something other than the target: {line:?}. Step 7b aimed it at a \
             RAM disk — memory the bootloader loaded — and writing one survives nothing. Refusing \
             it is what `BlockKind` exists for."
        )
        .into());
    }
    println!("  ok: and it refused the RAM disk it was pointed at, named correctly");
    install_gate_no_passwords(&transcript, "the first boot")?;

    check_installed_root(&target, &work)?;
    check_installed_account(&target, &work)?;

    println!("\nxtask: booting the installed disk with no stick…\n");
    let boot_qmp = work.join("qmp-install-boot.sock");
    let _ = fs::remove_file(&boot_qmp);
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", target.display()))
        .arg("-display")
        .arg("none")
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", boot_qmp.display()))
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
    let mut session = Session::spawn(cmd, "check-install-boot")?;
    let mut qmp = Qmp::connect(&boot_qmp)?;
    let result = run_installed_boot_steps(&mut session, &mut qmp);
    let transcript = session.finish();
    match result.and_then(|()| install_gate_no_passwords(&transcript, "the installed disk's boot")) {
        Ok(()) => {
            println!(
                "\nxtask: reinstalled onto a disk that held an install, and that disk boots to a greeter \
                 that knows only {INSTALL_ACCOUNT} ✓"
            );
            Ok(())
        }
        Err(e) => {
            println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
            Err(e)
        }
    }
}

fn install_gate_no_passwords(transcript: &str, which: &str) -> R<()> {
    for (whose, pw) in [(DEMO_USER, DEMO_PASSWORD), (INSTALL_ACCOUNT, INSTALL_PASSWORD)] {
        if transcript.contains(pw) {
            return Err(format!("{whose}'s password is in {which}'s transcript").into());
        }
    }
    println!("  ok: neither password typed is in {which}'s transcript");
    Ok(())
}

/// The installed root holds the new machine's account and no other, read on the host with the code
/// the guest serves it with:
/// - `/system/users` has one record, `INSTALL_ACCOUNT`'s, at `/home/<name>`;
/// - `/system/views.toml` reads, and makes it the one administrator — `alice` is not one;
/// - `/home` holds its home alone, with the three folders a home has, and no `alice`.
fn check_installed_account(disk: &Path, work: &Path) -> R<()> {
    use fs_server_ext4::ext4;
    let fs_img = work.join("installed-account.ext4");
    carve_partition(disk, 2, &fs_img)?;
    let img = MemFs(std::cell::RefCell::new(fs::read(&fs_img)?));
    let read = |path: &str| -> R<Vec<u8>> {
        let mut out = vec![0u8; 64 * 1024];
        let n = ext4::read_file(&img, path.as_bytes(), &mut out).map_err(|e| format!("read {path}: {e:?}"))?;
        out.truncate(n);
        Ok(out)
    };
    let users = read("/system/users")?;
    let records: Vec<(String, String)> = libusers::records(&users)
        .map(|r| (String::from_utf8_lossy(r.name).into_owned(), String::from_utf8_lossy(r.home).into_owned()))
        .collect();
    let home = format!("/home/{INSTALL_ACCOUNT}");
    if records != [(INSTALL_ACCOUNT.to_string(), home.clone())] {
        return Err(format!("/system/users holds {records:?}, not {INSTALL_ACCOUNT} alone").into());
    }
    println!("  ok: /system/users holds {INSTALL_ACCOUNT} alone, at {home}");
    let policy_text = String::from_utf8(read("/system/views.toml")?)?;
    let policy =
        view_broker::policy::parse(&policy_text).map_err(|e| format!("/system/views.toml does not read: {e}"))?;
    let admins = policy.administrators(&[INSTALL_ACCOUNT, DEMO_USER]);
    if admins != [INSTALL_ACCOUNT] {
        return Err(format!("the installed policy makes {admins:?} administrators, not {INSTALL_ACCOUNT} alone").into());
    }
    println!("  ok: the installed policy makes {INSTALL_ACCOUNT} the one administrator");
    let mut homes = Vec::new();
    let home_ino = ext4::resolve_dir(&img, b"/home").map_err(|e| format!("/home: {e:?}"))?;
    ext4::read_dir(&img, home_ino, 0, |_, _, name| {
        if name != b"." && name != b".." {
            homes.push(String::from_utf8_lossy(name).into_owned());
        }
        true
    })
    .map_err(|e| format!("list /home: {e:?}"))?;
    if homes != [INSTALL_ACCOUNT] {
        return Err(format!("/home holds {homes:?}, not {INSTALL_ACCOUNT}'s home alone").into());
    }
    for folder in ["Documents", "Downloads", "Pictures"] {
        let path = format!("{home}/{folder}");
        ext4::resolve_dir(&img, path.as_bytes()).map_err(|e| format!("{path}: {e:?}"))?;
    }
    println!("  ok: /home holds {home} alone, with Documents, Downloads and Pictures");
    Ok(())
}

struct MemFs(std::cell::RefCell<Vec<u8>>);

impl fs_server_ext4::BlockReader for MemFs {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), fs_server_ext4::FsError> {
        let v = self.0.borrow();
        let start = offset as usize;
        let end = start.checked_add(buf.len()).ok_or(fs_server_ext4::FsError::Io)?;
        if end > v.len() {
            return Err(fs_server_ext4::FsError::Io);
        }
        buf.copy_from_slice(&v[start..end]);
        Ok(())
    }
}

impl fs_server_ext4::BlockWriter for MemFs {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), fs_server_ext4::FsError> {
        let mut v = self.0.borrow_mut();
        let start = offset as usize;
        let end = start.checked_add(buf.len()).ok_or(fs_server_ext4::FsError::Io)?;
        if end > v.len() {
            return Err(fs_server_ext4::FsError::Io);
        }
        v[start..end].copy_from_slice(buf);
        Ok(())
    }
}

/// Hold the root filesystem an install just made to three claims the boot cannot make.
///
/// A booting machine proves the filesystem works, not that it is the size of the disk (an install
/// once booted with 24 MiB on a 477 MiB partition):
///
/// 1. `e2fsck -fn` finds it clean;
/// 2. it spans more than one block group and its block count is the partition's;
/// 3. a write lands past group 0, done on a copy with the guest's allocator: a size assertion
///    passes with allocation confined to the first group.
fn check_installed_root(disk: &Path, work: &Path) -> R<()> {
    use fs_server_ext4::{BlockReader, ext4};

    let (lba, sectors) = partition_extent(disk, 2)?;
    let fs = work.join("installed-root.ext4");
    carve_partition(disk, 2, &fs)?;
    let bytes = fs::read(&fs)?;
    let partition_blocks = sectors * 512 / 4096;

    // (1) The oracle. `e2fsck -fn` exits 0 while reporting problems, so the output is what is read.
    let out = Command::new("e2fsck")
        .args(["-fn", &fs.display().to_string()])
        .output()
        .map_err(|e| format!("run e2fsck: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if text.contains("? no") || !text.contains(" files (") {
        return Err(
            format!("e2fsck is not happy with the root this install made (partition at LBA {lba}):\n{text}").into()
        );
    }
    println!("  ok: the installed root is e2fsck-clean");

    let img = MemFs(std::cell::RefCell::new(bytes));
    let mut sb = [0u8; 1024];
    img.read_at(1024, &mut sb).map_err(|e| format!("read the superblock: {e:?}"))?;
    let blocks = u32::from_le_bytes([sb[4], sb[5], sb[6], sb[7]]) as u64;
    let per_group = u32::from_le_bytes([sb[32], sb[33], sb[34], sb[35]]) as u64;
    let groups = blocks.div_ceil(per_group);
    // The tolerance is one block group: `mkfs` drops a final group too short for its own metadata.
    // The failure sought (24 MiB on a 477 MiB partition) is wrong by whole groups.
    if blocks + per_group < partition_blocks {
        return Err(format!(
            "the installed filesystem is {blocks} blocks on a {partition_blocks}-block \
             partition, short by more than one block group of {per_group} — it was copied onto \
             the disk rather than made for it, which is exactly what H.1 did and H.2 exists to \
             stop"
        )
        .into());
    }
    if groups < 2 {
        return Err(format!(
            "the installed filesystem has one block group ({blocks} blocks), so this gate \
             cannot say anything about crossing one"
        )
        .into());
    }
    println!("  ok: it is {blocks} blocks across {groups} groups — the partition's size");

    let big = per_group * 4096 + 8 * 1024 * 1024; // comfortably into group 1
    ext4::create_file(&img, b"/", b"past-group-0", 0).map_err(|e| format!("create: {e:?}"))?;
    ext4::grow_file(&img, b"/past-group-0", big as usize, 0).map_err(|e| match e {
        // `TooLarge` is the allocator refusing to leave group 0, which this exists for;
        // `Unsupported` is a different deferral (the inline extent header's four entries).
        fs_server_ext4::FsError::Unsupported => {
            "growing past group 0 needed a deeper extent tree than the four inline entries \
             hold — that is the extent-splitting deferral, not the allocator"
                .to_string()
        }
        other => format!(
            "grow past group 0 failed: {other:?} — the allocator did not leave the first \
             group, which is what H.2's first box is"
        ),
    })?;
    let mut runs = [fs_server_ext4::BlockRun::default(); 8];
    let n = ext4::map_range(&img, b"/past-group-0", 0, big / 4096 + 1, &mut runs).map_err(|e| format!("map: {e:?}"))?;
    let highest = runs[..n]
        .iter()
        .filter(|r| r.device_lba != 0)
        .map(|r| r.device_lba + r.length as u64 - 1)
        .max()
        .ok_or("the grown file has no blocks")?;
    if highest < per_group {
        return Err(format!(
            "every block of a {big}-byte file landed in group 0 (highest {highest} of \
             {per_group} per group)"
        )
        .into());
    }
    let grown = work.join("installed-root-grown.ext4");
    fs::write(&grown, img.0.into_inner())?;
    let out = Command::new("e2fsck")
        .args(["-fn", &grown.display().to_string()])
        .output()
        .map_err(|e| format!("run e2fsck: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if text.contains("? no") || !text.contains(" files (") {
        return Err(format!("e2fsck after a cross-group write:\n{text}").into());
    }
    println!("  ok: a {} file reached block {highest}, past group 0, and stays clean", human_bytes(big));
    Ok(())
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut v, mut u) = (n, 0);
    while v >= 1024 && u + 1 < UNITS.len() {
        v /= 1024;
        u += 1;
    }
    format!("{v} {}", UNITS[u])
}

fn run_install_steps(session: &mut Session, qmp: &mut Qmp, dump: &Path, identity: &str) -> R<()> {
    let look = |qmp: &mut Qmp| -> R<(u32, u32, Vec<u8>)> {
        qmp.screendump(dump)?;
        parse_ppm(&fs::read(dump)?)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let mut highlight = loop {
        let (w, h, rgb) = look(qmp)?;
        if let Some(y) = limine_menu(w, h, &rgb) {
            break y;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!("no Limine menu within 120s (last dump {})", dump.display()).into());
        }
    };
    for step in 1..=2 {
        press(qmp, "down")?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (w, h, rgb) = look(qmp)?;
            match limine_menu(w, h, &rgb) {
                Some(y) if y > highlight => {
                    highlight = y;
                    break;
                }
                _ if std::time::Instant::now() > deadline => {
                    return Err(format!(
                        "Down {step} did not move the menu's highlight below y {highlight} within \
                         10s — the countdown may have booted it (last dump {})",
                        dump.display()
                    )
                    .into());
                }
                _ => {}
            }
        }
    }
    press(qmp, "ret")?;
    println!("  ok: chose the live menu's third entry");

    // 2. The disk, named by IDENTIFY: the model and serial are the gate's own, so the string can
    // only have come off the wire. Before the module lines, because storage binds during PCI
    // enumeration and modules publish after, and `expect` consumes what it scans past.
    session.expect(&format!(
        "disk ready ({} sectors, {} MiB): {identity}",
        TARGET_MIB * 1024 * 1024 / 512,
        TARGET_MIB
    ))?;
    println!("  ok: the target disk identified itself as {identity}");

    // The modules the install entry alone carries: an ordinary live boot does not read or hold
    // them.
    session.expect("ramdisk: module 2 (/boot/install-esp.img)")?;
    println!("  ok: the installer entry loaded the installable ESP as a block device");
    session.expect("ramdisk: module 3 (/boot/install-root.img)")?;
    println!("  ok: the installer entry loaded the pristine root as a block device");
    session.expect("(partition nitrox-root): ext4")?;
    let root_report = session.rest_of_line()?;
    if !root_report.contains("mounted at /storage/nitrox-root (ro)") {
        return Err(format!(
            "the target's nitrox-root is reported {root_report:?}, not auto-mounted read-only — \
             the reinstall this gate exists for needs the live boot to have mounted it"
        )
        .into());
    }
    println!("  ok: the target's older install is auto-mounted read-only, as a live boot does");
    let pristine = session
        .transcript()
        .lines()
        .find(|l| l.contains(&format!("(ramdisk {RAMDISK_IDENTITY})")))
        .and_then(|l| l.split("blk-").nth(1)?.split(' ').next()?.parse::<u32>().ok())
        .ok_or("the storage service reported no pristine root module")?;
    session.expect(&format!("(partition {}): ext4", libgpt::INSTALL_SOURCE_LABEL))?;
    let report = session.rest_of_line()?;
    if !report.contains("the installer's source, left unmounted") {
        return Err(format!(
            "the storage service's report of {} is {report:?}, not the installer's source left \
             unmounted",
            libgpt::INSTALL_SOURCE_LABEL
        )
        .into());
    }
    println!("  ok: the storage service left the pristine root unmounted, as the installer's source");

    session.expect("desktop-session-mgr: greeter presented")?;
    type_at_greeter(qmp, session, DEMO_USER)?;
    press(qmp, "tab")?;
    type_at_greeter(qmp, session, DEMO_PASSWORD)?;
    press(qmp, "ret")?;
    session.expect("desktop-session-mgr: login ok -> home=")?;
    println!("  ok: logged in at the greeter");

    // 5. No session holds a disk: the installer reaches one only through the `disks` grant, which
    // `libsession` logs, so this absence and the grant's line later are the halves.
    if let Some(line) = session.transcript().lines().find(|l| l.contains("block device(s) handed to a namespace")) {
        return Err(format!("a namespace was handed disks before anything asked for a view: {line:?}").into());
    }
    println!("  ok: the session holds no disk");
    session.expect("desktop-shell: up (graphical session leader)")?;

    // 6. A terminal from the Applications menu, keyboard only, after the chord exists: a hotkey
    // pressed before the shell registers it (seconds into a session) is delivered to nobody.
    session.expect("desktop-shell: Super+1..4 switches")?;
    qmp.send_key("meta_l", true)?;
    qmp.send_key("a", true)?;
    qmp.send_key("a", false)?;
    qmp.send_key("meta_l", false)?;
    session.expect("desktop-shell: applications menu open")?;
    type_into_menu(qmp, session, "nxterm")?;
    press(qmp, "ret")?;

    session.expect("desktop-shell: placed window ")?;
    println!("  ok: a terminal opened");

    // 7. The disk, refused as in use. `/dev/blk/0` is the target (the AHCI disk is registered
    // before the modules, which this asserts).
    install_gate_with_admin(qmp, session, "nxinstall", " /dev/blk/0")?;
    session.expect("nxinstall: refused /dev/blk/0: it is in use")?;
    session.expect("view: alice admin nxinstall — exited, code 1")?;
    println!("  ok: the target was refused as in use, since its older install is mounted");
    // And the grant is what reached the installer: step 5's other half.
    if !session.transcript().contains("block device(s) handed to a namespace") {
        return Err("`with admin nxinstall` ran with no disks handed to its view".into());
    }
    println!("  ok: the view was handed its disks by the grant");

    // 7b. A RAM disk, refused for being one. Checked for an install to it only after the real
    // install succeeded, so an installer that never ran cannot satisfy the absence.
    install_gate_with_admin(qmp, session, "nxinstall", &format!(" /dev/blk/{pristine}"))?;
    // The positive half makes the absence below mean something: it says which check fired.
    session.expect(&format!("nxinstall: refused /dev/blk/{pristine}: it is a ram disk, not a disk"))?;
    session.expect("view: alice admin nxinstall — exited, code 1")?;
    println!("  ok: the pristine root, a RAM disk, was refused for being one, named correctly");

    let stick = session
        .transcript()
        .lines()
        .find(|l| l.contains("storage-service: blk-") && l.contains("on the disk the machine started from"))
        .and_then(|l| l.split_whitespace().find_map(|w| w.strip_prefix("blk-")).map(str::to_string))
        .ok_or("the storage service did not report the stick the machine started from")?;
    install_gate_with_admin(qmp, session, "nxinstall", &format!(" /dev/blk/{stick}"))?;
    session.expect(&format!("nxinstall: refused /dev/blk/{stick}: it holds the running system"))?;
    session.expect("view: alice admin nxinstall — exited, code 1")?;
    println!("  ok: the stick the machine started from was refused: it holds the running system");

    install_gate_with_admin(qmp, session, "disk", " --unmount nitrox-root")?;
    session.expect("storage-service: unmounted nitrox-root")?;
    session.expect("view: alice admin disk — exited, code 0")?;
    println!("  ok: `with admin disk --unmount nitrox-root` freed the target");

    // 8b. The installer again, and it asks first. Answered `no` once: nothing may be written. The
    // question is on a grid nothing reads, so the receipt is what the answer waits for.
    install_gate_with_admin(qmp, session, "nxinstall", " /dev/blk/0")?;
    session.expect("nxinstall: asking to go ahead")?;
    type_answer(qmp, "no")?;
    session.expect("nxinstall: not going ahead; nothing was written to /dev/blk/0")?;
    session.expect("view: alice admin nxinstall — exited, code 0")?;
    println!("  ok: the installer asked before writing, and `no` wrote nothing");
    install_gate_with_admin(qmp, session, "nxinstall", " /dev/blk/0")?;
    session.expect("nxinstall: asking to go ahead")?;
    type_answer(qmp, "yes")?;

    session.expect("nxinstall: asking for the first account")?;
    type_answer(qmp, INSTALL_ACCOUNT)?;
    session.expect("nxinstall: asking for its password")?;
    type_answer(qmp, INSTALL_PASSWORD)?;
    session.expect("nxinstall: asking for the password again")?;
    type_answer(qmp, INSTALL_PASSWORD)?;
    println!("  ok: the installer asked for the first account, and was answered");

    session.expect(&format!("nxinstall: installing to /dev/blk/0 ({identity})"))?;
    println!("  ok: the installer went ahead on `yes` and named the disk it is writing");
    session.expect("nxinstall: copying the root from ")?;
    let from = session.rest_of_line()?;
    if !from.trim_end().ends_with(&format!("'s {}", libgpt::INSTALL_SOURCE_LABEL)) {
        return Err(format!("the installer copied its root from {from:?}, not the pristine root").into());
    }
    println!("  ok: the root came from the pristine copy —{}", from.trim_end());
    session.expect(&format!("nxinstall: the first account is {INSTALL_ACCOUNT}, its administrator"))?;
    println!("  ok: it made {INSTALL_ACCOUNT} the new machine's first account and its administrator");
    session.expect("nxinstall: wrote the partition table, the boot partition, and a ")?;
    let line = session.rest_of_line()?;
    let files: u32 = line
        .rsplit_once("holding ")
        .and_then(|(_, tail)| tail.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("no file count in the installer's milestone: {line:?}"))?;
    if files < 20 {
        return Err(format!("the installer copied {files} files, which is not a root: {line}").into());
    }
    println!("  ok: the install finished —{}", line.trim_end());
    // A person powers the machine off next, and a drive may still hold the last sectors in its
    // cache.
    session.expect("nxinstall: flushed the disk's write cache")?;
    println!("  ok: the installer flushed the disk before saying done");
    session.expect("view: alice admin nxinstall — exited, code 0")?;
    Ok(())
}

/// `with admin <program><args>`, at the terminal: the command, then the live account's password
/// once the broker's audit says it is asking. It does not wait for the broker's `started`: that
/// audit record can arrive after the program's first line and scan past the caller's next wait.
fn install_gate_with_admin(qmp: &mut Qmp, session: &mut Session, program: &str, args: &str) -> R<()> {
    // Forgotten first, so each step is asked: the subject is the install, not the grace window.
    type_at_terminal(qmp, "with --forget")?;
    session.expect("view: alice — remembered passwords forgotten")?;
    type_at_terminal(qmp, &format!("with admin {program}{args}"))?;
    session.expect(&format!("view: alice admin {program} — allowed, asking for a password"))?;
    type_answer(qmp, DEMO_PASSWORD)?;
    Ok(())
}

fn run_installed_boot_steps(session: &mut Session, qmp: &mut Qmp) -> R<()> {
    session.expect("init:   /: fs-server-ext4 on gpt-partlabel:nitrox-root (rw)")?;
    session.expect("init: mounted fs-server-ext4 at /")?;
    println!("  ok: the installed disk mounted its own root");
    session.expect("desktop-session-mgr: greeter presented")?;
    println!("  ok: the greeter came up on the installed system");
    // The stick's account first, so a greeter that let anyone in cannot pass on the right one.
    type_at_greeter(qmp, session, DEMO_USER)?;
    press(qmp, "tab")?;
    type_at_greeter(qmp, session, DEMO_PASSWORD)?;
    press(qmp, "ret")?;
    session.expect("desktop-session-mgr: login denied")?;
    println!("  ok: the live stick's {DEMO_USER} is refused");
    type_at_greeter(qmp, session, INSTALL_ACCOUNT)?;
    press(qmp, "tab")?;
    type_at_greeter(qmp, session, INSTALL_PASSWORD)?;
    press(qmp, "ret")?;
    session.expect(&format!("desktop-session-mgr: login ok -> home=/home/{INSTALL_ACCOUNT}"))?;
    session.expect("desktop-shell: up (graphical session leader)")?;
    println!("  ok: {INSTALL_ACCOUNT}, the account the installer made, logs in to a desktop");
    Ok(())
}
