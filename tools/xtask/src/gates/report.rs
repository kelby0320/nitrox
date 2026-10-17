//! `check-report`: the live menu's hardware report, read off the screen with no serial port.

use crate::*;

/// The facts `check-report`'s boot adds to [`EMULATED_MACHINE_FACTS`]: the laptop's shape (a USB
/// stick, an empty AHCI controller that is declined, no UART) and the stick enumerated.
const REPORT_FACTS: &[&[&str]] = &[
    &["boot: HHDM ", ", cmdline \"hwreport\""],
    &["drivers: 00:1f.2 declined by ahci: no SATA disk on any implemented port"],
    &["usb: port 1: 46f4:0001 class 08/06/50, SuperSpeed, \"QEMU USB HARDDRIVE (", "\": mass storage, bulk-only"],
    &["usb: port 1: LUN 0: disk \"QEMU QEMU HARDDISK (", " blocks of 512 bytes, record "],
    &["gpt: the disk the machine started from: its GUID is the one Limine loaded from"],
    &["usb: first round: 1 device(s) in "],
    &["ramdisk: module 1 (/boot/root.img)"],
    &["label \"nitrox-live\" -> block node"],
    &["console: no UART at COM1"],
];

/// `cargo xtask check-report` — the hardware report, read the way it will be read on the laptop.
///
/// Boots the live image as a USB stick with the AHCI controller empty and `-serial none`, then
/// works off the screen alone: finds Limine's menu with a detector of its own (the console's
/// decoder reads only the console's cells), presses Down and Enter, reads each report page with the
/// console's decoder, whole off one frame by its prompt row, and checks the pages carry
/// [`EMULATED_MACHINE_FACTS`] and [`REPORT_FACTS`].
pub(crate) fn cmd_check_report(accel: Accel, usb: bool, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image_live()?;

    let work = build_cache();
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp-report.sock");
    let dump = work.join("report.ppm");
    let _ = fs::remove_file(&qmp_sock);
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
        .arg("-qmp")
        .arg(format!("unix:{},server,nowait", qmp_sock.display()))
        .arg("-serial")
        .arg("none")
        .arg("-smp")
        .arg("4")
        .arg("-no-reboot")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    // `--usb`: the same machine with no i8042, so every page turns on a USB key press (the report
    // counts any keyboard's and holds its pages for one).
    if usb {
        usb_input_args(&mut cmd, false);
    }
    println!("xtask: hardware report gate — choosing the live image's report entry, with no serial port…\n");
    let _session = Session::spawn(cmd, "check-report")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;
    let look = |qmp: &mut Qmp| -> R<(u32, u32, Vec<u8>)> {
        qmp.screendump(&dump)?;
        parse_ppm(&fs::read(&dump)?)
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let first = loop {
        let (w, h, rgb) = look(&mut qmp)?;
        if let Some(highlight) = limine_menu(w, h, &rgb) {
            break highlight;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!("no Limine menu on the screen within 120s (last dump {})", dump.display()).into());
        }
    };
    press(&mut qmp, "down")?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (w, h, rgb) = look(&mut qmp)?;
        match limine_menu(w, h, &rgb) {
            Some(highlight) if highlight > first => break,
            _ if std::time::Instant::now() > deadline => {
                return Err(format!(
                    "Down did not move the menu's highlight below the first entry's (y {first}) \
                     within 10s — the countdown may have booted it (last dump {})",
                    dump.display()
                )
                .into());
            }
            _ => {}
        }
    }
    press(&mut qmp, "ret")?;
    println!("  ok: found Limine's menu, moved to its second entry, and chose it");

    let mut pages: Vec<Vec<String>> = Vec::new();
    let mut total: Option<usize> = None;
    let mut console_seen = false;
    let mut deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while total.is_none_or(|n| pages.len() < n) {
        let (w, h, rgb) = look(&mut qmp)?;
        let frame = Frame::read(w, h, &rgb);
        if frame.is_console() {
            console_seen = true;
            // The menu detector's control, on every console frame read here: the kernel's screen
            // is never taken for Limine's.
            if limine_menu(w, h, &rgb).is_some() {
                return Err(format!("the menu detector matched a kernel console frame ({})", dump.display()).into());
            }
            if let Some((index, of)) = frame.rows.last().and_then(|r| r.as_deref()).and_then(report_prompt) {
                let expected = *total.get_or_insert(of);
                if of != expected {
                    return Err(format!("page {index} says there are {of} pages; the first said {expected}").into());
                }
                if index == pages.len() + 1 {
                    let lines = frame.lines();
                    pages.push(lines[..lines.len() - 1].iter().map(|l| l.to_string()).collect());
                    press(&mut qmp, "spc")?;
                    deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                } else if index > pages.len() + 1 {
                    return Err(
                        format!("page {index}/{of} came up after page {} — one went by unread", pages.len()).into()
                    );
                }
            }
        } else if console_seen && frame.text_cells == 0 {
            return Err(format!(
                "the boot went on to the desktop after {} of {} report page(s) — the report did \
                 not hold (last dump {})",
                pages.len(),
                total.map_or("?".to_string(), |n| n.to_string()),
                dump.display()
            )
            .into());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "no report page {} on the screen in time — {} read so far{} (last dump {})",
                pages.len() + 1,
                pages.len(),
                if console_seen { "" } else { ", and never a whole console frame" },
                dump.display()
            )
            .into());
        }
    }
    let n = pages.len();
    println!("  ok: read {n} report page(s) off the screen, pressing a key for each");

    // 3. The facts, from the pages alone — the framebuffer's at the size this gate booted, with no
    //    padding, which is all QEMU can give (`DisplaySize`).
    let lines: Vec<&str> = pages.iter().flatten().map(String::as_str).collect();
    let framebuffer = format!("framebuffer: {}x{} pitch {} padding 0 bpp 32", size.w, size.h, size.w * 4);
    let framebuffer: &[&str] = &[&framebuffer];
    // Under `--usb` the machine has no i8042 and a keyboard and mouse beside the stick, so the
    // FADT's fact and the first round's count are the other ones.
    let replaced =
        |f: &&&[&str]| !usb || !f.iter().any(|frag| frag.contains("8042 present") || frag.contains("first round: 1 "));
    let usb_facts: &[&[&str]] = if usb { USB_INPUT_FACTS } else { &[] };
    let missing = missing_facts(
        &lines,
        EMULATED_MACHINE_FACTS
            .iter()
            .chain(REPORT_FACTS)
            .filter(replaced)
            .chain(usb_facts)
            .copied()
            .chain([framebuffer]),
    );
    if !missing.is_empty() {
        let read: Vec<String> =
            pages.iter().enumerate().map(|(i, p)| format!("--- page {} ---\n{}", i + 1, p.join("\n"))).collect();
        return Err(format!(
            "the report's {n} page(s) do not say: {missing:?}. The pages as read:\n{}",
            read.join("\n")
        )
        .into());
    }
    println!(
        "  ok: the pages say what this machine is — a declined AHCI controller, the module disk, the stick \
         enumerated, no COM1"
    );
    let built_from = built_from_line();
    if !pages.first().is_some_and(|p| p.iter().any(|l| l.contains(&built_from))) {
        return Err(format!("the report's first page does not say `{built_from}`").into());
    }
    println!("  ok: the first page names the build — {built_from}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let (w, h, rgb) = look(&mut qmp)?;
        let frame = Frame::read(w, h, &rgb);
        if !frame.is_console() && frame.text_cells == 0 {
            break;
        }
        if let Some((index, of)) = frame.rows.last().and_then(|r| r.as_deref()).and_then(report_prompt)
            && index > n
        {
            return Err(format!("page {index}/{of} came up after the last page's key").into());
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "the console did not hand the screen over within 120s of the last page (last dump {})",
                dump.display()
            )
            .into());
        }
    }
    println!("  ok: after the last page the boot went on, and the console handed the screen over");

    let _ = fs::remove_file(&qmp_sock);
    println!(
        "\nxtask: the hardware report is chosen from the menu, held a page at a time, and read with no serial port ✓"
    );
    Ok(())
}

/// `(page, of)` from a report page's prompt row, `— page 2/3 — any key —` (`kernel/src/report.rs`).
fn report_prompt(row: &str) -> Option<(usize, usize)> {
    let rest = row.strip_prefix("— page ")?.strip_suffix(" — any key —")?;
    let (index, of) = rest.split_once('/')?;
    let (index, of): (usize, usize) = (index.parse().ok()?, of.parse().ok()?);
    (index >= 1 && index <= of).then_some((index, of))
}

/// Limine's boot menu, as Limine 12.2.0 draws it on OVMF's framebuffer: `Some(y)` — the first pixel
/// row of the highlighted entry's bar — when `rgb` is that menu, `None` otherwise.
///
/// A detector of its own, calibrated on screendumps. All three must hold: every pixel is one of the
/// menu's five colours (OVMF's screens carry its logo's colours, the kernel console draws ink on
/// slate); a cyan title along the top only; a highlight bar, a row with a run of grey wider than
/// any glyph stroke, whose `y` shows Down moved the selection.
pub(crate) fn limine_menu(w: u32, h: u32, rgb: &[u8]) -> Option<usize> {
    const BLACK: [u8; 3] = [0, 0, 0];
    const GREY: [u8; 3] = [170, 170, 170];
    const GREEN: [u8; 3] = [0, 170, 0];
    const CYAN: [u8; 3] = [0, 170, 170];
    const BRIGHT_GREEN: [u8; 3] = [85, 255, 85];
    // Wider than any stroke of Limine's 8-pixel glyphs; the narrowest bar, behind `Nitrox`, is 72.
    const BAR_RUN: usize = 32;
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || rgb.len() < w * h * 3 {
        return None;
    }
    let (mut cyan, mut highlight) = (0usize, None);
    for y in 0..h {
        let mut run = 0usize;
        for x in 0..w {
            let i = (y * w + x) * 3;
            let px = [rgb[i], rgb[i + 1], rgb[i + 2]];
            if px == CYAN {
                if y > h / 10 {
                    return None;
                }
                cyan += 1;
            } else if px != BLACK && px != GREY && px != GREEN && px != BRIGHT_GREEN {
                return None;
            }
            run = if px == GREY { run + 1 } else { 0 };
            if run >= BAR_RUN && highlight.is_none() {
                highlight = Some(y);
            }
        }
    }
    if cyan >= 200 { highlight } else { None }
}
