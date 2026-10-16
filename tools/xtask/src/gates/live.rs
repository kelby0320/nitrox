use crate::*;

/// `cargo xtask check-live` — **the live image boots a machine with no storage driver** (Phase 5
/// Part C).
///
/// Boots `nitrox-live.img` attached as a **USB stick** (an xHCI, [`XHCI_DEVICE`], + `usb-storage`) with nothing
/// on the AHCI controller: the laptop's situation, where the firmware's USB stack reads the stick
/// and the kernel, which has no USB storage driver, never reads it again — it claims and resets the
/// controller (Phase 6 Part A) but nothing binds the stick until mass storage. Asserts over serial,
/// in order:
///
/// 1. no SATA disk — no storage driver carried the boot;
/// 2. the second Limine module became a block device, and the GPT pass found a partition labelled
///    `nitrox-live` on it;
/// 3. `init` mounted `/` from that partition, and the greeter was presented **within
///    [`LIVE_MOUNT_TO_GREETER`]** — a RAM disk that completed on the timer tick instead of on its
///    own interrupt does not make it;
/// 4. a login on the serial column writes a file under `/home` and reads it back — the root is
///    writable, in RAM.
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

/// Mount to greeter, at most, on the live image.
///
/// **Set from measurement, 2026-09-14**, between the two ways a RAM-disk completion can arrive,
/// timed by when the serial lines reached the host:
///
/// | | completion interrupt (shipped) | deleted — completion on the tick |
/// |---|---|---|
/// | TCG | 180 ms | 4,363 ms |
/// | KVM | 39 ms | 3,082–3,311 ms |
///
/// 1.5 s is eight times the slower good path and under half the fastest bad one. A first guess of
/// 3 s would have passed the deleted-interrupt control by 311 ms under KVM. (The TCG bad-path
/// figure is from before the gate timed arrivals; that path's lines arrive seconds apart, so the
/// two timings agree there.)
const LIVE_MOUNT_TO_GREETER: std::time::Duration = std::time::Duration::from_millis(1500);

fn run_live_steps(s: &mut Session) -> R<()> {
    // 0. The live image has a menu (Phase 5 Part D.2), and nothing here touches it: the countdown
    //    runs out and boots the first entry, which must still be the ordinary boot — no command
    //    line, so no hardware report holding the screen.
    s.expect(", cmdline \"\"")?;

    // 1. No disk. QEMU's q35 AHCI controller is present and empty, as a laptop's would be of
    //    anything Nitrox could boot from.
    s.expect("ahci: no SATA disk on any implemented port")?;

    // 2. The module is a disk, and its partition is named.
    s.expect("ramdisk: module 1 (/boot/root.img)")?;
    s.expect(&format!("label \"{LIVE_ROOT_PARTLABEL}\" -> block node"))?;
    // 2b. **The stick is a disk too** (Phase 6 Part D), in the hub thread's first round — and the
    //     disk the machine started from, by the GUID Limine loaded the modules from.
    s.expect(": LUN 0: disk \"QEMU QEMU HARDDISK (")?;
    s.expect("gpt: the disk the machine started from: its GUID is the one Limine loaded from")?;
    s.expect("label \"NITROX_ESP\" -> block node")?;

    // 3. The root is that partition, and the desktop comes up off it without waiting on a tick.
    s.expect(&format!("init:   /: fs-server-ext4 on gpt-partlabel:{LIVE_ROOT_PARTLABEL} (rw)"))?;
    s.expect("init: mounted fs-server-ext4 at /")?;
    // Timed by **when the lines arrived**, not when `expect` returned: under KVM both land in one
    // burst, and timing the returns measured 0 ms — the harness, not the guest.
    let mounted = s.matched_at();
    // **And read through it.** Until 2026-09-15 `mounted` was not enough on its own:
    // `fs-server-ext4` sent Ready before it read the superblock, so a zeroed partition still
    // printed it. The server now checks the superblock and the root directory first and refuses
    // a device that fails, so a zeroed partition stops at the line above — but the check reads
    // three blocks, and `init`'s first lookup is still the first read of a file.
    s.expect("init: /system/current-generation = nitrox-rootfs generation 1")?;
    // **The storage service knows this is a live boot** (administration Part C.5a): `init`'s root
    // is on the RAM disk. The rule is host-tested; this is the one boot whose root is on a RAM
    // disk, so it is the only place the rule's input is real, and the fact is what makes a
    // machine's own disks auto-mount read-only (C.5b).
    s.expect("storage-service: a live boot")?;
    // **And the stick is passed over** (Phase 6 Part D): read from the whole transcript, since the
    // service reports every device before it says what kind of boot this is.
    if !s.transcript().lines().any(|l| {
        l.contains("storage-service: blk-")
            && l.contains("(disk QEMU QEMU HARDDISK (")
            && l.contains("; on the disk the machine started from, passed over")
    }) {
        return Err("the storage service did not pass over the stick the machine started from".into());
    }
    println!("  ok: the stick is a disk, flagged as the one the machine started from, and passed over");
    // **And no session on this boot holds a disk** — none does on any entry since administration
    // Part G.3 — and absence is the kind of property that rots silently: nothing fails when a
    // sandbox quietly widens. Asserted both ways at the end of the run, below.
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

    // 4. A login writes, and reads back. **Wait for the prompt by searching the whole
    //    transcript**, not with `expect`: the serial column's `nitrox login:` and the greeter start
    //    together, and in the runs measured the prompt came first — so an `expect` after the
    //    greeter would be waiting for a line it had already scanned past.
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

    // **And the device manager calls the module a RAM disk** (administration Part B). The live
    // boot's module is the one RAM disk any gate has, so this is the only place that kind is seen
    // on its way to a person. Asked of the session's own `/dev/devices`, and matched on the
    // module's path — the ramdisk row's description, which the typed command does not contain.
    s.send("open /dev/devices/all.tsm | filter kind == \"ramdisk\"")?;
    s.expect("module 1 (/boot/root.img)")?;
    s.expect("/home>")?;
    println!("  ok: and /dev/devices lists it as a ramdisk");

    // **A session reaches no disk** (Phase 5 Part H.1; administration Part G.3). Since G.3 no
    // session on any entry does — a program gets one only through the view broker's `disks` grant
    // — and a widened sandbox is exactly the kind of regression nothing fails on. So it is seen
    // both ways: the installer, asked in the session, finds none, and no namespace on this boot
    // was handed one, which `libsession` says whenever it hands any.
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
