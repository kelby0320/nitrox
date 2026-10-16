//! `check-fbcon`: a boot and a panic read off the screen, with no serial port.

use crate::*;

/// `cargo xtask check-fbcon` — **the boot is legible with no serial port at all** (Phase 5
/// Part B).
///
/// Boots the release userspace with `-serial none` and reads the screen back into text with the
/// kernel console's own glyphs, palette and geometry (`glyphs` and `text`, compiled here by path).
/// Nothing is learned from COM1, because on the machine this is for there is no COM1. Three claims:
///
/// 1. **The boot is on the screen**: the kernel's first and last lines, and userspace's lines up to
///    the compositor — the last thing printed before a client takes the screen.
/// 2. **The console lets go**: once the compositor has drawn, no console text reappears over it,
///    although every service goes on printing.
/// 3. **A stop takes the screen back**: a panic after the desktop is up repaints the console with
///    the diagnosis at the bottom, and nothing draws over it afterwards.
///
/// All three rest on the `fbcon-gate` kernel feature, which is why this gate builds
/// [`BuildMode::FbconGate`] rather than booting `check-login`'s image.
///
/// **Held frames, not lucky samples.** A dump costs 4–18 ms depending on the host, and a
/// scrolling console does not wait for it: the first version of this gate required
/// `compositor: up` to be *seen*, and that line is on screen only from its own paint to the
/// compositor's first frame — one dump in 1640 on a fast host, and missed 3 runs in 5 with a
/// dump slowed to CI's rate (PR #296 review). So the kernel holds the screen still for a second
/// after the timer is calibrated and again at the handout, and each claim below is read off one
/// of those frames — **all of its lines together, in a single frame**, which a hold guarantees
/// and no run of transient sightings can imitate.
pub(crate) fn cmd_check_fbcon(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::FbconGate)?;

    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-fbcon.sock");
    let dump = work.join("fbcon.ppm");
    let _ = fs::remove_file(&qmp_sock);
    let ovmf = locate_ovmf()?;
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .arg("-display")
        .arg("none")
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
        // **The point of the gate.** No UART is created, so nothing the guest writes to COM1
        // goes anywhere, and the port reads back as a floating bus.
        .arg("-serial")
        .arg("none")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    println!("xtask: console gate — booting with no serial port…\n");
    let _session = Session::spawn(cmd, "check-fbcon")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    let look = |qmp: &mut Qmp, seen: &mut std::collections::BTreeSet<String>| -> R<Frame> {
        qmp.screendump(&dump)?;
        let (w, h, rgb) = parse_ppm(&fs::read(&dump)?)?;
        let frame = Frame::read(w, h, &rgb);
        for row in frame.lines() {
            if !row.is_empty() {
                seen.insert(row.to_string());
            }
        }
        Ok(frame)
    };

    // 1. The boot, read off the two held frames. Each group must appear whole in one frame.
    //
    // **The handout group's first line is the input server's bind, not the kernel's last line**
    // (administration Part B.4, moved again by Part E.1). It was the kernel's last line until the
    // boot grew past what one frame shows: Part B's device manager put 41 lines between it and
    // `compositor: up`, against the 36 a held frame guarantees. Then it was `init`'s auth-service
    // bind, until `service-mgr` took the servers over and logs two lines for each between. The
    // input server is bound just before the compositor starts. The kernel's own lines are the
    // early group's claim; this group's is that `sys_kprint` lines reach the screen up to the
    // handout, which any userspace line proves.
    const EARLY: &[&str] = &["Nitrox kernel — diagnostics online", "allocators up"];
    const HANDOUT: &[&str] = &["service-mgr: input-server bound at /dev/input/new", "compositor: up"];
    let (mut early, mut handout, mut console_seen) = (false, false, false);
    let mut rows = 0;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let frame = look(&mut qmp, &mut seen)?;
        if frame.is_console() {
            console_seen = true;
            rows = frame.rows.len();
        }
        early |= frame.has_all(EARLY);
        handout |= frame.has_all(HANDOUT);
        // The handover: a frame that is not the console and has no console glyph anywhere on it,
        // after the console has been seen. Not "after a given line", which is what made the
        // first version of this loop wait on a sighting.
        if console_seen && frame.text_cells == 0 && !frame.is_console() {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "no console handover within 120s — {} distinct row(s) read off the screen, {}",
                seen.len(),
                if console_seen { "and the console never left it" } else { "and never a whole console frame" }
            )
            .into());
        }
    }
    if !early {
        return Err(format!(
            "no single frame showed {EARLY:?} — the kernel's first line (with its em dash, from the \
             face's table) and a line from before PCI. The `fbcon-gate` hold after the timer keeps \
             them up for a second. Read {} distinct row(s); the last dump is {}",
            seen.len(),
            dump.display()
        )
        .into());
    }
    println!("  ok: held after the timer, with no serial port — the kernel's first line, em dash and all, before PCI");
    if !handout {
        return Err(format!(
            "no single frame showed {HANDOUT:?} together — a line `init` printed as it brought its \
             services up, and the last before the handout. The hold at the handout keeps that frame \
             up for a second, and it shows at least the last {} lines ({rows} rows less a \
             quarter-screen jump), so a boot that now prints more than that after the first needs a \
             nearer one here. Read {} distinct row(s)",
            rows - rows / 4,
            seen.len()
        )
        .into());
    }
    println!(
        "  ok: held at the handout — a line init printed bringing its services up, and \"compositor: up\", on one screen"
    );

    // 2. The console let go. Every service keeps printing through all of this — the greeter's
    //    redraws, the heartbeat — so a console still drawing would put text over the desktop.
    const SETTLE: std::time::Duration = std::time::Duration::from_secs(6);
    let until = std::time::Instant::now() + SETTLE;
    let mut dumps = 0;
    while std::time::Instant::now() < until {
        let frame = look(&mut qmp, &mut seen)?;
        dumps += 1;
        if frame.text_cells > 0 {
            return Err(format!(
                "console text reappeared over the desktop after the handover: {} cell(s) read as \
                 ink-on-paper glyphs, e.g. {:?} (dump at {})",
                frame.text_cells,
                frame.cells_sample(),
                dump.display()
            )
            .into());
        }
    }
    println!(
        "  ok: the compositor has the screen, and {dumps} dumps over {}s found no console text on it",
        SETTLE.as_secs()
    );

    // 3. A stop takes it back.
    qmp.send_key("f10", true)?;
    qmp.send_key("f10", false)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let panic_frame = loop {
        let frame = look(&mut qmp, &mut seen)?;
        if frame.is_console() && frame.lines().iter().any(|l| l.contains("*** KERNEL PANIC ***")) {
            break frame;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "no panic on the screen within 30s of F10 — the last dump ({}) {}",
                dump.display(),
                if frame.is_console() { "is console text without a panic in it" } else { "is not the console" }
            )
            .into());
        }
    };
    let message = "fbcon-gate: F10 pressed, and this kernel was built to stop on it";
    if !panic_frame.lines().iter().any(|l| l.contains(message)) {
        return Err(format!("the panic reached the screen without its message {message:?}").into());
    }
    println!("  ok: the panic took the screen back from the desktop, message and all");
    // Held, not merely drawn: the other CPUs were told to halt before the repaint, so nothing —
    // least of all the compositor — draws over it. **The pointer is moved to give something a
    // reason to.** An idle greeter draws nothing for seconds on end, so without the motion this
    // check passed with the stop's NMIs deleted and the compositor still running (measured
    // 2026-09-14). A running machine answers motion with a cursor, drawn over the text; a stopped
    // one does not answer — the PS/2 bytes sit in the controller, since every CPU that would
    // drain them, by interrupt or by its tick, is halted.
    for _ in 0..6 {
        qmp.send_motion(60, 40)?;
    }
    std::thread::sleep(std::time::Duration::from_secs(2));
    let later = look(&mut qmp, &mut seen)?;
    if later.lines() != panic_frame.lines() || !later.is_console() {
        return Err(format!(
            "something drew over the panic within 2s of it reaching the screen (dump at {})",
            dump.display()
        )
        .into());
    }
    println!("  ok: and it is still there 2s later, untouched");

    let _ = fs::remove_file(&qmp_sock);
    println!("\nxtask: the boot, the handover and a stop are all legible with no serial port ✓");
    Ok(())
}
