use crate::*;

/// The file `check-shutdown` writes, under the serial session's `/home` — `alice`'s home on the
/// root filesystem.
const SHUTDOWN_PATTERN_FILE: &str = "check-shutdown.bin";

/// Where it lands on the root filesystem: the session's `/home` is `alice`'s home.
fn shutdown_pattern_path() -> String {
    format!("/home/{DEMO_USER}/{SHUTDOWN_PATTERN_FILE}")
}

/// The test image's services that do not honour a stop, passed over at a shutdown after their
/// bound: its graphical clients and `restart-probe`. Any other name "still running" is a failure.
const SHUTDOWN_UNHONOURED: &[&str] = &["nxterm", "ui-testclient", "input-testclient", "restart-probe"];

/// `cargo xtask check-shutdown` — **a shutdown, with the host holding the result** (administration
/// Part E.4d).
///
/// A copy of a `--selftest` disk image, the release root with the test packages, booted twice and
/// driven on serial as a person at the prompt would drive it.
///
/// **The halt.** Logged in as `alice`, `test-pattern` writes a pattern under `/home` through a
/// mapping and **exits without a sync**; the host reads the disk while the guest runs and finds the
/// file without the pattern, and the superblock saying mounted. Then `with power shutdown`, with no
/// password, as the seeded policy gives it to everyone. The gate asserts the sequence on COM1 — the
/// sessions ended when asked rather than after their bound, every service but the test image's
/// known clients stopped, `init` unmounted `/` clean — and **reads the message off the screen**
/// with `check-fbcon`'s decoder as well as off COM1, since the laptop has no serial port. With the
/// machine stopped, the host carves the root partition out: `e2fsck -fn` clean, the superblock
/// recording a clean unmount, and the file holding the pattern, read with `debugfs`.
///
/// **The reboot.** A fresh copy of the disk, since `boot-probe` is not one to run twice on the same
/// filesystem; `with admin date --set` to 2031, then `with power shutdown --reboot`, a reset
/// through the FADT's register, and QEMU is watched through a second boot — which must **anchor its
/// clock to the time set**, read back from the RTC the set wrote (administration Part E.5), and
/// whose `init` must mount the root **without** finding it not cleanly unmounted, which is the
/// reboot's own shutdown leaving it clean.
///
/// **Only after `boot-probe`'s verdict.** The power endpoint is one, and `boot-probe` takes and
/// lets go of it to test its refusals; a shutdown asked in the meantime would find the broker
/// unable to grant it.
pub(crate) fn cmd_check_shutdown(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    require_tool("e2fsck")?;
    require_tool("debugfs")?;
    cmd_image(BuildMode::Selftest)?;
    let work = build_cache().join("check-shutdown");
    fs::create_dir_all(&work)?;
    let disk = work.join("disk.img");
    let _ = fs::remove_file(&disk);
    fs::copy(image_path(), &disk)?;
    let ovmf = locate_ovmf()?;
    let qmp_sock = work.join("qmp.sock");
    let dump = work.join("screen.ppm");
    let boot = |reboot: bool| -> R<Command> {
        let _ = fs::remove_file(&qmp_sock);
        let mut cmd = Command::new("qemu-system-x86_64");
        qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
        cmd.arg("-drive")
            .arg(format!("if=none,id=disk,format=raw,file={}", disk.display()))
            .arg("-device")
            .arg("ide-hd,drive=disk,bus=ide.0")
            .arg("-display")
            .arg("none")
            .arg("-qmp")
            .arg(format!("unix:{},server=on,wait=off", qmp_sock.display()))
            .arg("-chardev")
            .arg("stdio,id=hostserial,signal=off")
            .arg("-serial")
            .arg("chardev:hostserial")
            .arg("-smp")
            .arg("4")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        // The halt must not be taken for a reset; the reboot must be allowed to happen.
        if !reboot {
            cmd.arg("-no-reboot");
        }
        Ok(cmd)
    };

    println!("xtask: shutdown gate — booting a copy of the test disk, to halt…\n");
    let mut session = Session::spawn(boot(false)?, "check-shutdown")?;
    let result = run_shutdown_halt(&mut session, &disk, &work, &qmp_sock, &dump);
    let transcript = session.finish();
    let _ = fs::write(build_cache().join("guest-transcript-check-shutdown.log"), &transcript);
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    check_shutdown_sequence(&transcript)?;
    println!("\nxtask: the machine is stopped; the disk, on the host:");
    let fs_img = work.join("nitrox-root.ext4");
    carve_partition(&disk, 2, &fs_img)?;
    check_left_clean(&fs_img)?;
    let path = shutdown_pattern_path();
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();
    match debugfs_cat(&fs_img, &path)? {
        Some(b) if b == pattern => {}
        Some(b) => {
            let first = (0..STORAGE_PATTERN_LEN).find(|&i| b.get(i) != Some(&pattern[i]));
            return Err(format!(
                "{path} on the disk is {} bytes and is not the pattern (first difference at byte \
                 {first:?}): the shutdown did not write the mapped pages back",
                b.len()
            )
            .into());
        }
        None => return Err(format!("{path} is not on the disk").into()),
    }
    println!("  ok: {path} holds the pattern, {STORAGE_PATTERN_LEN} bytes, read by debugfs");

    println!("\nxtask: a fresh copy of the disk, to reboot…\n");
    let _ = fs::remove_file(&disk);
    fs::copy(image_path(), &disk)?;
    let mut session = Session::spawn(boot(true)?, "check-shutdown")?;
    let result = run_shutdown_reboot(&mut session);
    let transcript = session.finish();
    let _ = fs::write(build_cache().join("guest-transcript-check-shutdown-reboot.log"), &transcript);
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    println!(
        "\nxtask: a shutdown wrote back a file nothing synced, left the root clean and said so on the \
         screen, and a reboot came back ✓"
    );
    Ok(())
}

/// Wait for `boot-probe`'s verdict, then log in on serial as `DEMO_USER`.
fn shutdown_login(s: &mut Session) -> R<()> {
    s.expect("boot-probe: test-harness verdict")?;
    // **The whole line, waited for** (PR #356's CI): the prefix can arrive before the word after
    // it, and on KVM this read `… verdict P` from the transcript and refused a passing image.
    if !s.matched_line()?.contains("boot-probe: test-harness verdict PASS") {
        return Err("boot-probe's verdict was not PASS, so the image is not one to shut down".into());
    }
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
    Ok(())
}

/// The halt: a write through a mapping with no sync, seen on the host without its pattern, then
/// `with power shutdown`, and the message on COM1 and on the screen.
fn run_shutdown_halt(s: &mut Session, disk: &Path, work: &Path, qmp_sock: &Path, dump: &Path) -> R<()> {
    shutdown_login(s)?;
    let at = format!("/home/{SHUTDOWN_PATTERN_FILE}");
    s.send(&format!("test-pattern --write {at}"))?;
    s.expect(&format!("test-pattern: wrote {STORAGE_PATTERN_LEN} bytes to {at} through a mapping, and did not sync"))?;
    s.expect("/home>")?;
    // **On the host, the file is on the disk and the pattern is not**, as in `check-storage`: so
    // what the host finds after the shutdown, the shutdown put there.
    let fs_img = work.join("nitrox-root-midrun.ext4");
    carve_partition(disk, 2, &fs_img)?;
    let path = shutdown_pattern_path();
    let pattern: Vec<u8> = (0..STORAGE_PATTERN_LEN).map(storage_pattern_byte).collect();
    match debugfs_cat(&fs_img, &path)? {
        None => {
            return Err(format!(
                "{path} is not on the disk at all after `test-pattern --write`, so the absence of \
                 the pattern would prove nothing"
            )
            .into());
        }
        Some(b) if b == pattern => {
            return Err(format!(
                "{path} already holds the pattern before any shutdown — something wrote it back, so \
                 this gate cannot show that the shutdown does"
            )
            .into());
        }
        Some(_) => {}
    }
    let state = ext4_s_state(&fs_img)?;
    if state & EXT4_VALID_FS != 0 {
        return Err(format!(
            "the superblock says clean (s_state {state:#06x}) while the root is mounted writable, \
             so a clean one at the end would not be the shutdown's doing"
        )
        .into());
    }
    println!("  ok: on the host meanwhile: the file is there without the pattern, and the root mounted");

    // **The shutdown, as anyone may ask it**: the seeded policy's `power` view, with no password.
    s.send("with power shutdown")?;
    s.expect("shutdown: shutting down")?;
    s.expect("It is now safe to turn off your computer.")?;
    println!("  ok: `with power shutdown` was taken, and COM1 says it is safe to turn off");

    // **And the screen says so**, read with the kernel console's own glyphs: the laptop has no
    // COM1. The machine is halted, so the frame is still.
    let mut qmp = Qmp::connect(qmp_sock)?;
    expect_safe_on_screen(&mut qmp, dump)?;
    println!("  ok: the screen, read back as text, ends in the message");
    Ok(())
}

/// **A halted machine's screen ends in "It is now safe to turn off your computer."**, read back
/// as text with the kernel console's own glyphs and layout (`check-fbcon`'s decoder) — what a
/// person sees on a machine with no serial port. `check-shutdown` and `check-logout` read it.
pub(crate) fn expect_safe_on_screen(qmp: &mut Qmp, dump: &Path) -> R<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        qmp.screendump(dump)?;
        let (w, h, rgb) = parse_ppm(&fs::read(dump)?)?;
        let frame = Frame::read(w, h, &rgb);
        let last = frame.lines().iter().rev().find(|l| !l.is_empty()).map(|l| l.to_string());
        if frame.is_console() && last.as_deref() == Some("It is now safe to turn off your computer.") {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "the screen does not end in the message: {} (last line {last:?}, dump at {})",
                if frame.is_console() { "it is console text" } else { "it is not the console" },
                dump.display()
            )
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// **What a shutdown's COM1 must show**: the sessions ended when asked, every service stopped but
/// the test image's known clients, and `init` left `/` clean before `sys_power` flushed the disk.
fn check_shutdown_sequence(transcript: &str) -> R<()> {
    for line in [
        "service-mgr: shutting down, as asked",
        "nxsh: asked to stop, exiting",
        "session-mgr: asked to stop, exiting",
        "desktop-session-mgr: asked to stop, exiting",
        "service-mgr: the sessions have ended",
        "storage-service: asked to stop: everything it mounted is unmounted, exiting",
        "logging-service: asked to stop, exiting",
        "auth-service: asked to stop, exiting",
        "service-mgr: every service has been asked to stop; telling init to finish",
        "init: / unmounted, and its server left the filesystem clean",
        "power: stopping every processor, to halt",
    ] {
        if !transcript.contains(line) {
            return Err(format!("`{line}` is not in the transcript").into());
        }
    }
    if transcript.contains("still running after the sessions' bound") {
        return Err("a login supervisor ran out its bound rather than ending its session when asked".into());
    }
    for l in transcript.lines().filter(|l| l.contains("is still running after it was asked to stop")) {
        if !SHUTDOWN_UNHONOURED.iter().any(|n| l.contains(&format!("'{n}'"))) {
            return Err(format!("a service did not stop when asked: {l:?}").into());
        }
    }
    println!("  ok: the sessions ended when asked, every service stopped, and init left / clean");
    Ok(())
}

/// The time the reboot half sets before it restarts: 2031-01-02T00:00:00Z, years from any host's
/// clock, so a second boot that anchored to the host's time rather than the one written is plain.
const SHUTDOWN_SET_CLOCK: (&str, i64) = ("2031-01-02T00:00:00Z", 1_925_078_400);

/// How far past [`SHUTDOWN_SET_CLOCK`] the second boot may anchor: the seconds the reboot took,
/// since QEMU's RTC goes on counting, with room for a slow TCG host. A clock that did not survive
/// is five years out.
const SHUTDOWN_CLOCK_SLACK_SECS: i64 = 600;

/// The reboot: the clock set with `with admin date --set`, then `with power shutdown --reboot`
/// resets through the FADT's register. The second boot anchors its clock to the time that was set
/// — the RTC's write-back, across a reset (administration Part E.5) — and its `init` mounts the
/// root without finding it not cleanly unmounted.
fn run_shutdown_reboot(s: &mut Session) -> R<()> {
    s.expect("init: up (pid 1)")?;
    shutdown_login(s)?;
    let (time, epoch) = SHUTDOWN_SET_CLOCK;
    s.send(&format!("with admin date --set {time} --unix"))?;
    s.expect("[with admin] password (1 of 3): ")?;
    s.send(DEMO_PASSWORD)?;
    s.expect(&format!("wall clock: set to {epoch} (Unix epoch seconds, UTC); the RTC holds it"))?;
    s.expect("/home>")?;
    s.send("with power shutdown --reboot")?;
    s.expect("shutdown: restarting")?;
    s.expect("power: resetting through the FADT's reset register")?;
    let reset_at = s.transcript().len();
    s.expect("wall clock: anchored at ")?;
    let anchored = s.rest_of_line()?;
    let secs: i64 = anchored
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("the second boot's clock line did not read: `{anchored}`"))?;
    if !(epoch..=epoch + SHUTDOWN_CLOCK_SLACK_SECS).contains(&secs) {
        let slack = SHUTDOWN_CLOCK_SLACK_SECS;
        return Err(format!(
            "the second boot anchored its clock at {secs}, not within {slack} s after the {epoch} \
             set before the reboot: the RTC did not keep the time written to it"
        )
        .into());
    }
    println!("  ok: the clock set before the reboot is the one the second boot anchored to: {secs}");
    s.expect("init: up (pid 1)")?;
    s.expect("init: mounted fs-server-ext4 at /")?;
    let after = s.transcript()[reset_at..].to_string();
    if after.contains("the filesystem was not cleanly unmounted last time") {
        return Err("the second boot found the root not cleanly unmounted: the reboot's shutdown did not \
             leave it clean"
            .into());
    }
    println!(
        "  ok: `with power shutdown --reboot` reset the machine, and the second boot mounted the \
         root clean"
    );
    Ok(())
}

/// **What an unmount through the storage service must leave**, read on the host from the carved
/// filesystem `fs_img`: `e2fsck -fn` clean, and the superblock's `s_state` recording a clean
/// unmount with no error — `check-storage`'s and `check-recovery`'s.
pub(crate) fn check_left_clean(fs_img: &Path) -> R<()> {
    check_ext4_clean(fs_img, &format!("the disk's {ROOT_PARTLABEL}"))
}

/// `e2fsck -fn` clean and `s_state` recorded clean, for the ext4 image `fs_img`, which is `what`.
pub(crate) fn check_ext4_clean(fs_img: &Path, what: &str) -> R<()> {
    // `e2fsck -fn` exits 0 while reporting problems, so its output is what is read — as
    // `check_installed_root` reads it.
    let out = Command::new("e2fsck")
        .args(["-fn", &fs_img.display().to_string()])
        .output()
        .map_err(|e| format!("run e2fsck: {e}"))?;
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    if text.contains("? no") || !text.contains(" files (") {
        return Err(format!("e2fsck is not happy with {what}:\n{text}").into());
    }
    println!("  ok: e2fsck -fn finds it clean");
    let state = ext4_s_state(fs_img)?;
    if state & EXT4_VALID_FS == 0 || state & EXT4_ERROR_FS != 0 {
        return Err(format!(
            "the superblock's s_state is {state:#06x}: not recorded clean, or with its error bit \
             set. The unmount's `Meta::Unmount` is what records it clean"
        )
        .into());
    }
    println!("  ok: the superblock records it cleanly unmounted (s_state {state:#06x})");
    Ok(())
}
