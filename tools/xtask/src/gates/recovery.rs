//! `check-recovery`: a password reset on an installed disk from the live image, then booted.

use crate::*;

/// The password `check-recovery` resets `alice`'s to: a fixture, like `DEMO_PASSWORD`, on a disk
/// the gate makes and throws away.
const RECOVERED_PASSWORD: &str = "alice after recovery";

/// `cargo xtask check-recovery` — a forgotten password, reset from the live image, on demand like
/// `check-install`: two boots and a disk image.
///
/// The path a person takes on the laptop, with no view and no service writing the file. The first
/// boot is the live image as a USB stick beside a copy of the release disk, its root marked not
/// cleanly unmounted. On serial, as the live `alice` (administrator of the live system, not the
/// installed one): `with admin disk` mounts the disk's `nitrox-root` writable; `account --password
/// alice --users /storage/nitrox-root/system/users` sets a new password in that disk's file; `with
/// admin disk --unmount` leaves it clean; and the live `alice` still logs in with the old password.
/// Between the boots the host checks the filesystem is clean and that `/system/users` differs from
/// the shipped one in `alice`'s line alone. The second boot is that disk alone: the old password is
/// refused, the new one taken. Neither boot printed either.
pub(crate) fn cmd_check_recovery(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    require_tool("e2fsck")?;
    require_tool("debugfs")?;
    // A copy of the release disk, made fresh, so a run cannot pass on what an earlier one wrote.
    cmd_image(BuildMode::Normal)?;
    let work = build_cache().join("check-recovery");
    fs::create_dir_all(&work)?;
    let disk = work.join("disk.img");
    let _ = fs::remove_file(&disk);
    fs::copy(image_path(), &disk)?;
    mark_root_not_clean(&disk)?;
    let shipped = debugfs_cat(&storage_root_fs(&disk, &work)?, "/system/users")?
        .ok_or("the release disk has no /system/users")?;
    cmd_image_live()?;
    let ovmf = locate_ovmf()?;
    let secrets = [DEMO_PASSWORD, RECOVERED_PASSWORD];
    let leaked = |transcript: &str| secrets.iter().position(|p| transcript.contains(p));

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-device")
        .arg(XHCI_DEVICE)
        .arg("-drive")
        .arg(format!("if=none,id=stick,format=raw,file={}", live_image_path().display()))
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
    println!("xtask: recovery gate — booting the live image beside a copy of the release disk…\n");
    let mut session = Session::spawn(cmd, "check-recovery")?;
    let result = run_recovery_steps(&mut session);
    let transcript = session.finish();
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    if let Some(p) = leaked(&transcript) {
        return Err(format!("password {p} reached the console on the live boot: a prompt echoed it").into());
    }

    println!("\nxtask: the machine is stopped; the disk, on the host:");
    let fs_img = storage_root_fs(&disk, &work)?;
    check_left_clean(&fs_img)?;
    let users = debugfs_cat(&fs_img, "/system/users")?.ok_or("the disk has no /system/users")?;
    check_recovered_users(&shipped, &users)?;

    println!("\nxtask: booting the disk alone…\n");
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", disk.display()))
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
    let mut session = Session::spawn(cmd, "check-recovery-boot")?;
    let result = run_recovered_boot_steps(&mut session);
    let transcript = session.finish();
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    if let Some(p) = leaked(&transcript) {
        return Err(format!("password {p} reached the console on the disk's boot: a prompt echoed it").into());
    }
    println!("\nxtask: a password reset from the live image logs in on the installed disk ✓");
    Ok(())
}

/// Wait for the serial login prompt, searched for in the whole transcript as `check-live` does:
/// it and the greeter come up together, so an `expect` could scan past it.
fn await_serial_login(s: &mut Session) -> R<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    while !s.transcript().contains("nitrox login:") {
        if std::time::Instant::now() > deadline {
            return Err("no `nitrox login:` prompt on the serial column".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(())
}

fn run_recovery_steps(s: &mut Session) -> R<()> {
    let users = format!("/storage/{ROOT_PARTLABEL}/system/users");

    let reported = format!("(partition {ROOT_PARTLABEL}): ext4");
    s.expect(&reported)?;
    let line = s.matched_line()?;
    let name = line
        .split_whitespace()
        .find(|w| w.starts_with("blk-"))
        .ok_or_else(|| format!("no `blk-<n>` in {line:?}"))?
        .to_string();
    let index = name.trim_start_matches("blk-").to_string();
    if !line.contains(&format!("mounted at /storage/{ROOT_PARTLABEL} (ro)")) {
        return Err(format!("the disk's {ROOT_PARTLABEL} is not auto-mounted read-only: {line:?}").into());
    }
    println!("  ok: {name}, the disk's {ROOT_PARTLABEL}, auto-mounted read-only on a live boot");

    await_serial_login(s)?;
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;

    let admin = |s: &mut Session, command: &str| -> R<()> { with_admin_asked(s, command) };
    admin(s, &format!("disk --unmount {ROOT_PARTLABEL}"))?;
    s.expect(&format!("disk: unmounted {ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;
    admin(s, &format!("disk --mount /dev/blk/{index}"))?;
    s.expect(&format!("storage-service: mounted {ROOT_PARTLABEL} (rw), as asked"))?;
    s.expect(&format!("disk: mounted {name} at /storage/{ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;
    println!("  ok: `with admin disk` unmounted it and mounted it writable");

    s.send(&format!("account --password alice --users {users}"))?;
    s.expect("new password for alice: ")?;
    s.send(RECOVERED_PASSWORD)?;
    s.expect("again: ")?;
    s.send(RECOVERED_PASSWORD)?;
    s.expect(&format!("account: set a new password for alice in {users}"))?;
    s.expect("/home>")?;
    println!("  ok: account --password alice --users set it in the disk's file");

    admin(s, &format!("disk --unmount {ROOT_PARTLABEL}"))?;
    s.expect(&format!("storage-service: unmounted {ROOT_PARTLABEL}, left clean"))?;
    s.expect(&format!("disk: unmounted {ROOT_PARTLABEL}"))?;
    s.expect("/home>")?;

    // 6. **The running system's own file is untouched**: its `alice` logs in as before.
    s.send("exit")?;
    s.expect("nitrox login:")?;
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    println!("  ok: unmounted clean, and the live system's own alice logs in as before");
    Ok(())
}

/// `alice`'s line changed, **and nothing else**: every other line of `shipped` is in `now` byte for
/// byte, and `alice`'s new record takes the recovered password and refuses the old one.
fn check_recovered_users(shipped: &[u8], now: &[u8]) -> R<()> {
    let lines = |b: &[u8]| -> Vec<Vec<u8>> { b.split(|&c| c == b'\n').map(|l| l.to_vec()).collect() };
    let (before, after) = (lines(shipped), lines(now));
    let is_alice = |l: &[u8]| l.starts_with(format!("{DEMO_USER}:").as_bytes());
    if before.len() != after.len() {
        return Err(format!("/system/users went from {} lines to {}", before.len(), after.len()).into());
    }
    for (b, a) in before.iter().zip(&after) {
        if is_alice(b) != is_alice(a) || (!is_alice(b) && b != a) {
            return Err(format!(
                "a line other than alice's changed in /system/users: {:?} became {:?}",
                String::from_utf8_lossy(b),
                String::from_utf8_lossy(a)
            )
            .into());
        }
        if is_alice(b) && b == a {
            return Err("alice's line in the disk's /system/users is as the release disk shipped it".into());
        }
    }
    let alice = libusers::find(now, DEMO_USER.as_bytes()).ok_or("the disk's /system/users has no alice")?;
    if !alice.verifies(RECOVERED_PASSWORD.as_bytes()) || alice.verifies(DEMO_PASSWORD.as_bytes()) {
        return Err("alice's new record does not take the recovered password, or still takes the old one".into());
    }
    // Under a fresh salt, a whole one from the entropy source, not the build's.
    let salt_of = |file: &[u8]| {
        let mut out = [0u8; libusers::SALT_MAX];
        let n = libusers::find(file, DEMO_USER.as_bytes()).and_then(|r| r.salt(&mut out)).unwrap_or(0);
        out[..n].to_vec()
    };
    let (old, new) = (salt_of(shipped), salt_of(now));
    if new.len() != libusers::SALT_LEN || new.iter().all(|&b| b == 0) || new == old {
        return Err(format!(
            "alice's new salt is {} bytes {:02x?}: not a fresh {}-byte one from the entropy source",
            new.len(),
            new,
            libusers::SALT_LEN
        )
        .into());
    }
    println!("  ok: /system/users changed in alice's line alone, to the recovered password, under a fresh salt");
    Ok(())
}

fn run_recovered_boot_steps(s: &mut Session) -> R<()> {
    s.expect(&format!("init:   /: fs-server-ext4 on gpt-partlabel:{ROOT_PARTLABEL} (rw)"))?;
    await_serial_login(s)?;
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("login incorrect")?;
    s.expect("nitrox login:")?;
    s.send(DEMO_USER)?;
    s.expect("password:")?;
    s.send(RECOVERED_PASSWORD)?;
    s.expect("/home>")?;
    println!("  ok: on the disk alone, alice's old password is refused and the recovered one logs in");
    Ok(())
}
