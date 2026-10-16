use crate::*;

/// The model and serial the gate's target disk reports, and the identity they compose into.
///
/// **Set explicitly rather than taken from QEMU's defaults**, for two reasons. The gate has to
/// type the identity back, so it must know it exactly; and a name of its own choosing is proof
/// the string travelled from the *device* — `ahci::identify`'s IDENTIFY words, the `info` leaf,
/// `nxinstall`'s report — rather than being a constant that happens to match.
const TARGET_MODEL: &str = "NITROX-TEST-DISK";

/// The target disk's serial.
const TARGET_SERIAL: &str = "INSTALL01";

/// What the kernel calls the RAM disk `install-root.img` becomes — the pristine root, which the
/// view holds, and the device this gate points the installer at to prove it refuses a RAM disk as
/// a target (administration Part G.3). Until then it was `root.img`'s, which the view no longer
/// reaches: it holds the running system, and `disks` withholds it.
const RAMDISK_IDENTITY: &str = "module 3 (/boot/install-root.img)";

/// **The account `check-install` makes the new machine's first** (administration Part G.2): a
/// fixture, as `DEMO_USER` is, and one the build does not make, so a login as it can only have
/// come from what the installer wrote.
const INSTALL_ACCOUNT: &str = "dana";

/// Its password: letters and spaces, since it is typed on PS/2 at a terminal and at a greeter.
const INSTALL_PASSWORD: &str = "a quiet morning by the lake";

/// How big the gate's blank disk is. Small enough to write quickly under TCG, large enough for a
/// 33 MiB boot partition and a root partition with room left over.
const TARGET_MIB: u64 = 512;

/// `cargo xtask check-install` — **reinstall** onto a disk holding an install, from an ordinary
/// session, then boot that disk on its own.
///
/// **On demand, like `check-resolutions`**: two boots and a 512 MiB image is not a per-PR cost.
///
/// The first boot is the whole path a person takes on the laptop, driven the way they drive it:
/// the live image's third menu entry, the **graphical** greeter (that machine has no serial port,
/// so its greeter is the only way in), a terminal launched from the Applications menu, and `with
/// admin nxinstall` typed at the shell in it. Nothing here reads the terminal's grid — a release
/// image deliberately does not narrate it — so what the gate asserts on is the kernel log: the
/// modules the install entry loads, what the storage service mounted and left alone, the broker's
/// audit, and the milestones `nxinstall` records for a destructive operation — including the
/// *refusal* of one, which is an event in its own right and the only part of a refusal a gate can
/// see.
///
/// **A reinstall since administration Part G.3**: the target is a copy of the release disk, so the
/// live boot auto-mounts its `nitrox-root` read-only and `disks` withholds it. The installer
/// refuses it as in use, `with admin disk --unmount nitrox-root` frees it, and then it installs.
/// No session on the boot holds a disk; everything the installer reaches, it reaches through the
/// grant.
///
/// The second boot is the only assertion that really matters: the disk alone, no stick, and a
/// greeter on it that knows the account the installer made.
pub(crate) fn cmd_check_install(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    let work = build_cache();
    fs::create_dir_all(&work).ok();

    // **A copy of the release disk, grown, and made fresh** (administration Part G.3). A disk that
    // already holds an install, since that is what a reinstall meets: its `nitrox-root`
    // auto-mounted read-only on the live boot, and so withheld from `disks` until it is unmounted.
    // **Grown to `TARGET_MIB`**, because the release disk is 128 MiB and a root installed on it is
    // one block group, which `check_installed_root`'s write past group 0 cannot cross. The old
    // install survives the growing: its primary GPT still describes 128 MiB, and `nitrox-root`
    // sits inside it. Reusing a disk left by an earlier run would let an install that wrote nothing
    // pass on the last run's bytes — the gate's own version of a stale artifact.
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
        // The target: a SATA disk on the AHCI controller, with an identity of the gate's
        // choosing so the string `nxinstall` prints can only have come from IDENTIFY.
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
    // **And the refusal held.** Step 7 aimed the installer at the RAM disk the running system's
    // root is inside, naming it correctly, so only its *kind* could refuse it. An install
    // records a milestone; this asserts none names that device. The assertion is an absence, and
    // what makes an absence mean something here is that the install which followed it succeeded
    // — the same pairing `check-live` and this gate's step 5 make across two gates.
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

    // Before booting it: what the boot cannot tell us about the filesystem.
    check_installed_root(&target, &work)?;
    check_installed_account(&target, &work)?;

    // The second boot: what was written, on its own.
    println!("\nxtask: booting the installed disk with no stick…\n");
    let boot_qmp = work.join("qmp-install-boot.sock");
    let _ = fs::remove_file(&boot_qmp);
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", target.display()))
        .arg("-display")
        .arg("none")
        // **A login this time** (administration Part G.2), so the greeter needs keys.
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

/// **Neither password this gate typed is in what the guest printed** (administration Part G.2):
/// the live account's, at `with`'s prompts and a greeter, and the new account's, at the installer's
/// prompts and a greeter. Each was typed with echo off or into a masked field.
fn install_gate_no_passwords(transcript: &str, which: &str) -> R<()> {
    for (whose, pw) in [(DEMO_USER, DEMO_PASSWORD), (INSTALL_ACCOUNT, INSTALL_PASSWORD)] {
        if transcript.contains(pw) {
            return Err(format!("{whose}'s password is in {which}'s transcript").into());
        }
    }
    println!("  ok: neither password typed is in {which}'s transcript");
    Ok(())
}

/// **The installed root holds the new machine's account and no other** (administration Part G.2),
/// read on the host out of the partition the installer wrote, with the code the guest serves it
/// with:
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

/// An in-memory filesystem image, for holding an installed root to the code that will serve it.
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
/// **A booting machine proves the filesystem works; it does not prove the filesystem is the
/// size of the disk.** H.1's install booted perfectly with a 24 MiB filesystem on a 477 MiB
/// partition — everything the second boot asserts passed, and the machine had 24 MiB. So:
///
/// 1. `e2fsck -fn` finds it clean — the oracle, on a filesystem laid out by `mkfs` and filled
///    by `create_file`, both of them ours;
/// 2. it spans more than one block group and its superblock's block count is the partition's,
///    which is the difference between "a filesystem" and "a filesystem for this disk";
/// 3. **a write lands past group 0** — done here with the allocator the guest runs, because a
///    size assertion passes just as well with allocation confined to the first group, and
///    doing it in the guest means moving 128 MiB at TCG speed for a property this settles in a
///    second. The file is written to a copy, so the disk that boots is the one the installer
///    made.
fn check_installed_root(disk: &Path, work: &Path) -> R<()> {
    use fs_server_ext4::{BlockReader, ext4};

    let (lba, sectors) = partition_extent(disk, 2)?;
    let fs = work.join("installed-root.ext4");
    carve_partition(disk, 2, &fs)?;
    let bytes = fs::read(&fs)?;
    let partition_blocks = sectors * 512 / 4096;

    // (1) The oracle. `e2fsck -fn` **exits 0 while reporting problems**, so the output is what
    // is read — the same lesson the crate's own test helper learned (Phase 5 Part H.2).
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

    // (2) It is the partition's size, not the source image's.
    let img = MemFs(std::cell::RefCell::new(bytes));
    let mut sb = [0u8; 1024];
    img.read_at(1024, &mut sb).map_err(|e| format!("read the superblock: {e:?}"))?;
    let blocks = u32::from_le_bytes([sb[4], sb[5], sb[6], sb[7]]) as u64;
    let per_group = u32::from_le_bytes([sb[32], sb[33], sb[34], sb[35]]) as u64;
    let groups = blocks.div_ceil(per_group);
    // **The tolerance is one block group, not a handful of blocks.** `mkfs` drops a final
    // group too short to hold its own metadata, as `mke2fs` does, so a filesystem may
    // legitimately stop up to one short group before the end of its partition. What this is
    // looking for is the H.1 failure — a 24 MiB filesystem on a 477 MiB partition — which is
    // wrong by whole groups, not by a tail. A tighter bound would fail on the 0.25% of
    // partition sizes whose last group is unusable (PR #310 review, finding 1).
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

    // (3) A write past group 0, with the allocator the guest runs.
    let big = per_group * 4096 + 8 * 1024 * 1024; // comfortably into group 1
    ext4::create_file(&img, b"/", b"past-group-0", 0).map_err(|e| format!("create: {e:?}"))?;
    ext4::grow_file(&img, b"/past-group-0", big as usize, 0).map_err(|e| match e {
        // **Two failures that look alike and are not.** `TooLarge` is the allocator refusing
        // to leave group 0, which is what this assertion exists for; `Unsupported` is the
        // inline extent header running out of its four entries, which is a different deferral
        // and would be the wrong diagnosis to print here (PR #310 review).
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

/// Bytes as a person reads them.
fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let (mut v, mut u) = (n, 0);
    while v >= 1024 && u + 1 < UNITS.len() {
        v /= 1024;
        u += 1;
    }
    format!("{v} {}", UNITS[u])
}

/// The first boot: choose the installer entry, log in graphically, and run the installer.
fn run_install_steps(session: &mut Session, qmp: &mut Qmp, dump: &Path, identity: &str) -> R<()> {
    // 1. Limine's menu, and its **third** entry.
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

    // 2. The disk this gate attached, named by IDENTIFY rather than by the gate. The model and
    //    serial are the gate's own, so this string can only have come off the wire.
    //
    //    **Before the module lines, because that is where it is in the stream**: the storage
    //    driver binds during PCI enumeration and the bootloader's modules are published after.
    //    `expect` consumes what it scans past, so asserting these in the order they are
    //    *interesting* rather than the order they happen hangs on a line that was there.
    session.expect(&format!(
        "disk ready ({} sectors, {} MiB): {identity}",
        TARGET_MIB * 1024 * 1024 / 512,
        TARGET_MIB
    ))?;
    println!("  ok: the target disk identified itself as {identity}");

    // 3. **The module that entry alone carries.** This is the assertion that the installable ESP
    //    is loaded by choosing the installer, and it is why the entry has a module line the other
    //    two do not: an ordinary live boot does not read it, hold it, or publish it.
    session.expect("ramdisk: module 2 (/boot/install-esp.img)")?;
    println!("  ok: the installer entry loaded the installable ESP as a block device");
    // **And the pristine root** (administration Part G.1), which the installer copies from.
    session.expect("ramdisk: module 3 (/boot/install-root.img)")?;
    println!("  ok: the installer entry loaded the pristine root as a block device");
    // **The disk holds an install, auto-mounted read-only** (administration Part G.3): the
    // reinstall's premise, and why `disks` will withhold it. The storage service's report of the
    // partition, in registry order, before the modules' partitions below.
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
    // **And the pristine root's device**, from the same report: where it lands among the devices
    // depends on the target's partitions, which the storage service numbers, so it is read here
    // rather than written down.
    let pristine = session
        .transcript()
        .lines()
        .find(|l| l.contains(&format!("(ramdisk {RAMDISK_IDENTITY})")))
        .and_then(|l| l.split("blk-").nth(1)?.split(' ').next()?.parse::<u32>().ok())
        .ok_or("the storage service reported no pristine root module")?;
    // **Which the storage service left unmounted, and said why**: mounted, it would be in use, and
    // `disks` would withhold it from `nxinstall`. The line is its report of the partition.
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

    // 4. The graphical login, which on the laptop is the only way in.
    session.expect("desktop-session-mgr: greeter presented")?;
    type_at_greeter(qmp, session, DEMO_USER)?;
    press(qmp, "tab")?;
    type_at_greeter(qmp, session, DEMO_PASSWORD)?;
    press(qmp, "ret")?;
    session.expect("desktop-session-mgr: login ok -> home=")?;
    println!("  ok: logged in at the greeter");

    // 5. **No session holds a disk** (administration Part G.3). The install entry's sessions are
    //    ordinary: what the installer reaches, it reaches through the `disks` grant, and until the
    //    first `with` nothing on this boot has been handed one. `libsession` says whenever it hands
    //    any, so the absence here, and the grant's line later, are the two halves.
    if let Some(line) = session.transcript().lines().find(|l| l.contains("block device(s) handed to a namespace")) {
        return Err(format!("a namespace was handed disks before anything asked for a view: {line:?}").into());
    }
    println!("  ok: the session holds no disk");
    session.expect("desktop-shell: up (graphical session leader)")?;

    // 6. A terminal, from the Applications menu — the path a person takes, keyboard only,
    //    because that is the path the laptop has.
    //
    //    **Wait until the chord exists.** A hotkey is registered with the compositor by the
    //    shell, several seconds into a session, and a chord pressed before that is delivered to
    //    nobody: the first version of this step pressed it as soon as the shell said it was up,
    //    and the transcript shows `compositor: Super down` / `Super up` with nothing acting on
    //    it. This line is printed after `Super+A` is registered, so it is the receipt.
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

    // 7. **The disk, refused as in use** (administration Part G.3). `/dev/blk/0` is the target —
    //    the AHCI disk is registered before the modules, which this gate asserts rather than
    //    assumes — and `disks` withheld it, since its `nitrox-root` is mounted. The installer says
    //    why and names the command that frees it; the log line is the part of that a gate can see.
    install_gate_with_admin(qmp, session, "nxinstall", " /dev/blk/0")?;
    session.expect("nxinstall: refused /dev/blk/0: it is in use")?;
    session.expect("view: alice admin nxinstall — exited, code 1")?;
    println!("  ok: the target was refused as in use, since its older install is mounted");
    // **And the grant is what reached the installer**: the other half of step 5's absence. Read
    // from the transcript, since the broker logs it before its `started`, which is already past.
    if !session.transcript().contains("block device(s) handed to a namespace") {
        return Err("`with admin nxinstall` ran with no disks handed to its view".into());
    }
    println!("  ok: the view was handed its disks by the grant");

    // 7b. **A RAM disk, refused for being one, and named correctly.** The pristine root is a RAM
    //    disk the view holds, so the *only* reason to refuse it is what the device **is**. Until
    //    G.3 this was `root.img`, which the view no longer reaches. Ordered before the real install so its proof is available: nothing may say it
    //    installed to a RAM disk, and the transcript is checked for that after the install that
    //    follows has succeeded, which is what stops the absence from being satisfied by an
    //    installer that never ran at all.
    install_gate_with_admin(qmp, session, "nxinstall", &format!(" /dev/blk/{pristine}"))?;
    // **The positive half, and it is what makes the absence below mean anything.** An
    // absence alone is satisfied by a command that never ran, by a character going astray so
    // the operand named nothing, and by a refusal for the wrong *reason*. This line says which
    // check fired (PR #309 review, 7).
    session.expect(&format!("nxinstall: refused /dev/blk/{pristine}: it is a ram disk, not a disk"))?;
    session.expect("view: alice admin nxinstall — exited, code 1")?;
    println!("  ok: the pristine root, a RAM disk, was refused for being one, named correctly");

    // 7c. **The stick the machine started from, refused** (Phase 6 Part D): mass storage makes it a
    //     disk, the storage service names it in use, so `disks` withholds it, and the installer says
    //     it holds the running system. Its index is the service's report of it.
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

    // 8. **Freed, as the installer said** (administration Part G.3): `with admin disk --unmount`,
    //    naming the label, and the storage service's own line that it went.
    install_gate_with_admin(qmp, session, "disk", " --unmount nitrox-root")?;
    session.expect("storage-service: unmounted nitrox-root")?;
    session.expect("view: alice admin disk — exited, code 0")?;
    println!("  ok: `with admin disk --unmount nitrox-root` freed the target");

    // 8b. **The installer, again, and now the disk is in the view — and it asks first** (2026-10-01).
    //     Answered `no` once: nothing may be written, and saying so is not a failure. The plan and
    //     the question are on a grid nothing here reads, so the receipt is what the answer waits
    //     for, and the line after it is what says the `no` was heard.
    install_gate_with_admin(qmp, session, "nxinstall", " /dev/blk/0")?;
    session.expect("nxinstall: asking to go ahead")?;
    type_answer(qmp, "no")?;
    session.expect("nxinstall: not going ahead; nothing was written to /dev/blk/0")?;
    session.expect("view: alice admin nxinstall — exited, code 0")?;
    println!("  ok: the installer asked before writing, and `no` wrote nothing");
    install_gate_with_admin(qmp, session, "nxinstall", " /dev/blk/0")?;
    session.expect("nxinstall: asking to go ahead")?;
    type_answer(qmp, "yes")?;

    // **The first account, asked after the confirmation** (administration Part G.2). Each answer
    // is typed after the installer's receipt for it: the name's before it is asked, each
    // password's once echo is off, so nothing typed is echoed or early.
    session.expect("nxinstall: asking for the first account")?;
    type_answer(qmp, INSTALL_ACCOUNT)?;
    session.expect("nxinstall: asking for its password")?;
    type_answer(qmp, INSTALL_PASSWORD)?;
    session.expect("nxinstall: asking for the password again")?;
    type_answer(qmp, INSTALL_PASSWORD)?;
    println!("  ok: the installer asked for the first account, and was answered");

    // What a destructive operation leaves in the system log, which is also the only thing this
    // gate can read: a release terminal does not narrate its grid.
    session.expect(&format!("nxinstall: installing to /dev/blk/0 ({identity})"))?;
    println!("  ok: the installer went ahead on `yes` and named the disk it is writing");
    // **From the pristine root**, not the live one the session runs from (administration G.1).
    session.expect("nxinstall: copying the root from ")?;
    let from = session.rest_of_line()?;
    if !from.trim_end().ends_with(&format!("'s {}", libgpt::INSTALL_SOURCE_LABEL)) {
        return Err(format!("the installer copied its root from {from:?}, not the pristine root").into());
    }
    println!("  ok: the root came from the pristine copy —{}", from.trim_end());
    session.expect(&format!("nxinstall: the first account is {INSTALL_ACCOUNT}, its administrator"))?;
    println!("  ok: it made {INSTALL_ACCOUNT} the new machine's first account and its administrator");
    // **A filesystem it made, not one it copied** (Phase 5 Part H.2). The size in this line is
    // the *partition's*, which is the whole point: H.1 put a 24 MiB filesystem on it.
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
    // **Flushed before it said "done"** (administration Part C.2): a person powers the machine
    // off next, and a drive may still hold the last sectors in its cache.
    session.expect("nxinstall: flushed the disk's write cache")?;
    println!("  ok: the installer flushed the disk before saying done");
    session.expect("view: alice admin nxinstall — exited, code 0")?;
    Ok(())
}

/// **`with admin <program><args>`, at the terminal** (administration Part G.2): the command, then
/// the live stick account's password once the broker's audit says it is asking — the receipt
/// `check-login` types after too.
///
/// **It does not wait for the broker's `started`.** That line is an audit record, which reaches the
/// console through the logging service, after the program is already running — so it can arrive
/// after the program's own first line, and waiting for it would scan past the line the caller
/// waits for next (`expect` consumes what it scans past). The caller's next `expect` is a line of
/// the program's, which says it started.
fn install_gate_with_admin(qmp: &mut Qmp, session: &mut Session, program: &str, args: &str) -> R<()> {
    // **Forgotten first**, so each step is asked whatever the one before it left remembered (the
    // laptop polish's Part A): this gate's subject is the install, not the grace window.
    type_at_terminal(qmp, "with --forget")?;
    session.expect("view: alice — remembered passwords forgotten")?;
    type_at_terminal(qmp, &format!("with admin {program}{args}"))?;
    session.expect(&format!("view: alice admin {program} — allowed, asking for a password"))?;
    type_answer(qmp, DEMO_PASSWORD)?;
    Ok(())
}

/// The second boot: the installed disk, alone.
fn run_installed_boot_steps(session: &mut Session, qmp: &mut Qmp) -> R<()> {
    // **It found its root by the label this installer wrote**, not by the one the live image
    // used: the bytes copied in are the live root's, and what makes them `nitrox-root` is the
    // partition table `nxinstall` built.
    session.expect("init:   /: fs-server-ext4 on gpt-partlabel:nitrox-root (rw)")?;
    session.expect("init: mounted fs-server-ext4 at /")?;
    println!("  ok: the installed disk mounted its own root");
    // And no module disk: nothing is riding along this time.
    session.expect("desktop-session-mgr: greeter presented")?;
    println!("  ok: the greeter came up on the installed system");
    // **The live stick's account is not this machine's** (administration Part G.2): refused, with
    // the password that worked on the stick — tested before the new account, so a greeter that
    // let anyone in could not pass on the right one.
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
