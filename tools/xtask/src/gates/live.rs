//! `check-live`: the live image booted as a USB stick with no disk.

use crate::*;

/// `cargo xtask check-live` — the live image boots a machine with no storage driver.
///
/// Boots `nitrox-live.img` as a USB stick ([`XHCI_DEVICE`] + `usb-storage`) with the AHCI
/// controller empty, the laptop's situation: no SATA disk, the second Limine module a block device
/// with a `nitrox-live` partition, `init`'s `/` mounted from it and the greeter up within
/// [`LIVE_MOUNT_TO_GREETER`], and a login that writes a file and reads it back.
pub(crate) fn cmd_check_live(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image_live()?;
    let ovmf = locate_ovmf()?;
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-device")
        .arg(XHCI_DEVICE)
        .arg("-drive")
        .arg(format!("if=none,id=stick,format=raw,file={}", live_image_path().display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=stick")
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
    println!("xtask: live image gate — booting a USB stick, with no disk…\n");
    let mut session = Session::spawn(cmd, "check-live")?;
    let result = run_live_steps(&mut session);
    let transcript = session.finish();
    match result {
        Ok(()) => {
            println!("\nxtask: the live image booted from RAM with no disk, and its root takes writes ✓");
            Ok(())
        }
        Err(e) => {
            println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
            Err(e)
        }
    }
}

/// Mount to greeter, at most, on the live image, measured by when the serial lines reached the
/// host:
///
/// | | completion interrupt (shipped) | deleted — completion on the tick |
/// |---|---|---|
/// | TCG | 180 ms | 4,363 ms |
/// | KVM | 39 ms | 3,082–3,311 ms |
///
/// 1.5 s is eight times the slower good path and under half the fastest bad one; 3 s would have
/// passed the deleted-interrupt control by 311 ms under KVM.
const LIVE_MOUNT_TO_GREETER: std::time::Duration = std::time::Duration::from_millis(1500);

fn run_live_steps(s: &mut Session) -> R<()> {
    // 0. The live image has a menu that nothing here touches: the countdown boots the first entry,
    // which must be the ordinary boot (no command line, no hardware report on screen).
    s.expect(", cmdline \"\"")?;

    s.expect("ahci: no SATA disk on any implemented port")?;

    s.expect("ramdisk: module 1 (/boot/root.img)")?;
    s.expect(&format!("label \"{LIVE_ROOT_PARTLABEL}\" -> block node"))?;
    s.expect(": LUN 0: disk \"QEMU QEMU HARDDISK (")?;
    s.expect("gpt: the disk the machine started from: its GUID is the one Limine loaded from")?;
    s.expect("label \"NITROX_ESP\" -> block node")?;

    s.expect(&format!("init:   /: fs-server-ext4 on gpt-partlabel:{LIVE_ROOT_PARTLABEL} (rw)"))?;
    s.expect("init: mounted fs-server-ext4 at /")?;
    // Timed by **when the lines arrived**, not when `expect` returned: under KVM both land in one
    // burst, and timing the returns measured 0 ms — the harness, not the guest.
    let mounted = s.matched_at();
    // And read through it: `fs-server-ext4` once sent Ready before reading the superblock, so a
    // zeroed partition printed `mounted`.
    s.expect("init: /system/current-generation = nitrox-rootfs generation 1")?;
    s.expect("storage-service: a live boot")?;
    // And the stick is passed over, read from the whole transcript (the service reports every
    // device before it says what kind of boot this is).
    if !s.transcript().lines().any(|l| {
        l.contains("storage-service: blk-")
            && l.contains("(disk QEMU QEMU HARDDISK (")
            && l.contains("; on the disk the machine started from, passed over")
    }) {
        return Err("the storage service did not pass over the stick the machine started from".into());
    }
    println!("  ok: the stick is a disk, flagged as the one the machine started from, and passed over");
    // No session on this boot holds a disk; absence rots silently, so it is asserted both ways at
    // the end.
    s.expect("desktop-session-mgr: greeter presented")?;
    let took = s.matched_at().saturating_duration_since(mounted);
    if took > LIVE_MOUNT_TO_GREETER {
        return Err(format!(
            "the greeter took {} ms from the mount, over the {} ms bound — block I/O on the RAM \
             disk is completing on the timer tick rather than on its own interrupt \
             (kernel/src/io/ramdisk.rs § Completing where a device completes)",
            took.as_millis(),
            LIVE_MOUNT_TO_GREETER.as_millis()
        )
        .into());
    }
    println!("  ok: mount to greeter in {} ms, within {} ms", took.as_millis(), LIVE_MOUNT_TO_GREETER.as_millis());

    // 4. A login writes, and reads back. The prompt is searched for in the whole transcript, not
    // with `expect`: the serial column's `nitrox login:` and the greeter start together and the
    // prompt came first in measured runs.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(45);
    while !s.transcript().contains("nitrox login:") {
        if std::time::Instant::now() > deadline {
            return Err("no `nitrox login:` prompt on the serial column".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    s.send("alice")?;
    s.expect("password:")?;
    s.send(DEMO_PASSWORD)?;
    s.expect("/home>")?;
    s.send("[7, 8, 9] | save ./live-proof.txt")?;
    s.expect("/home>")?;
    s.send("format(\"live-rows={}\", (open ./live-proof.txt | count))")?;
    s.expect("live-rows=3")?;
    println!("  ok: a file written under /home read back from the RAM disk");

    // The device manager calls the module a RAM disk: matched on the module's path, the ramdisk
    // row's description, which the typed command does not contain.
    s.send("open /dev/devices/all.tsm | filter kind == \"ramdisk\"")?;
    s.expect("module 1 (/boot/root.img)")?;
    s.expect("/home>")?;
    println!("  ok: and /dev/devices lists it as a ramdisk");

    // A session reaches no disk: a program gets one only through the view broker's `disks` grant.
    // Seen both ways: the installer finds none, and `libsession` logged no namespace handed one.
    s.send("nxinstall")?;
    s.expect("no block devices in this view")?;
    s.expect("/home>")?;
    let text = s.transcript();
    if let Some(line) = text.lines().find(|l| l.contains("block device(s) handed to a namespace")) {
        return Err(format!(
            "a namespace on an ordinary live boot was handed disks: {line:?}. Nothing here asked \
             for a view, so nothing should have"
        )
        .into());
    }
    println!("  ok: and no session on this boot could reach a disk");
    Ok(())
}
