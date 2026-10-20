//! `check-media`: a stick plugged into the live desktop, shown in Files, saved to and ejected.

use crate::*;

const MEDIA_LABEL: &str = "NXMEDIA";

const MEDIA_TEXT: &str = "saved onto a stick from nxedit";

const MEDIA_FILE: &str = "from-nitrox.txt";

/// `cargo xtask check-media` — removable media in a session, on the desktop a person uses: the
/// release live image as the boot stick, beside a copy of the release disk, logged in at the
/// graphical greeter.
///
/// Files at Home lists the internal disk's `nitrox-root` (read-only, no eject button). A FAT stick
/// is plugged in over QMP with nothing typed or moved after it, and its row with an eject button
/// must appear, read off screendumps (any input would wake Files and draw the row whether the watch
/// had woken it or not). A file is saved to it from the editor through Save As, the chooser's Up
/// aimed at where `libui` lays it out for the staged theme; ejecting by a click on the button
/// records the filesystem clean with no password asked, and the host finds it `fsck.fat -n` clean
/// with `mtype` reading back the text. It asserts effects, not Files' account of itself: an eject
/// happens only if the button was drawn where the gate aims.
pub(crate) fn cmd_check_media(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    require_tool("fsck.fat")?;
    for tool in ["mformat", "mtype"] {
        require_tool(tool)?;
    }
    cmd_image(BuildMode::Normal)?;
    cmd_image_live()?;
    let work = build_cache().join("check-media");
    fs::create_dir_all(&work)?;
    let disk = work.join("disk.img");
    let _ = fs::remove_file(&disk);
    fs::copy(image_path(), &disk)?;
    let stick = work.join("stick.img");
    media_stick(&stick)?;
    let qmp_sock = work.join("qmp.sock");
    let _ = fs::remove_file(&qmp_sock);
    let ovmf = locate_ovmf()?;
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-qmp")
        .arg(format!("unix:{},server=on,wait=off", qmp_sock.display()))
        .arg("-device")
        .arg(XHCI_DEVICE)
        .arg("-drive")
        .arg(format!("if=none,id=boot,format=raw,file={}", live_image_path().display()))
        .arg("-device")
        .arg("usb-storage,bus=xhci.0,drive=boot")
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
    crate::scsi_trace::trace_args(&mut cmd, &work.join("scsi.log"));
    println!("xtask: media gate — booting the live image beside a copy of the release disk…\n");
    let mut session = Session::spawn(cmd, "check-media")?;
    let result = Qmp::connect(&qmp_sock).and_then(|mut qmp| {
        qmp.screen = Some(size);
        run_media_steps(&mut session, &mut qmp, &stick, &work, size)
    });
    let transcript = session.finish();
    let _ = fs::remove_file(&qmp_sock);
    if let Err(e) = result {
        println!("\n--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    if transcript.contains(DEMO_PASSWORD) {
        return Err("the password reached the console".into());
    }
    println!("\nxtask: the machine is stopped; the stick, on the host:");
    let part = work.join("stick-part.img");
    carve_fat_clean(&stick, &part, "the ejected stick")?;
    let out = mtools_cmd("mtype").arg("-i").arg(&part).arg(format!("::{MEDIA_FILE}")).output()?;
    let want = format!("{MEDIA_TEXT}\n");
    if !out.status.success() || out.stdout != want.as_bytes() {
        return Err(format!(
            "mtype reads {MEDIA_FILE} off the stick as {:?}, not the {want:?} typed into the editor",
            String::from_utf8_lossy(&out.stdout)
        )
        .into());
    }
    println!("  ok: mtype reads back what was typed into the editor");
    println!(
        "\nxtask: a stick plugged into the live desktop appeared in Files, took a file from the editor, and ejected clean ✓"
    );
    Ok(())
}

/// The stick `check-media` plugs in: a FAT32 partition behind an MBR, labelled [`MEDIA_LABEL`] and
/// empty. 300 MiB is what FAT32 by count needs at 4 KiB clusters, the smallest `fs-server-fat`
/// serves; the file is sparse.
fn media_stick(path: &Path) -> R<()> {
    const MIB: u64 = 1024 * 1024;
    let _ = fs::remove_file(path);
    fs::File::create(path)?.set_len(300 * MIB)?;
    let count = (300 * 2048 - 2048) as u32;
    write_mbr(path, 2048, count, 0x0C)?;
    let at = format!("{}@@{MIB}", path.display());
    run(mtools_cmd("mformat")
        .arg("-i")
        .arg(&at)
        .arg("-F")
        .arg("-c")
        .arg("8")
        .arg("-T")
        .arg(count.to_string())
        .arg("-v")
        .arg(MEDIA_LABEL)
        .arg("::"))?;
    Ok(())
}

/// How much is drawn in a rectangle of a screendump: the pixels differing from its top-left one by
/// more than the faintest edge of anti-aliased text. Glyph ink against a flat ground.
fn ink_in(dump: &(u32, u32, Vec<u8>), x: i32, y: i32, w: i32, h: i32) -> usize {
    let (dw, dh, rgb) = dump;
    let px = |x: i32, y: i32| {
        let i = (y as usize * *dw as usize + x as usize) * 3;
        (rgb[i], rgb[i + 1], rgb[i + 2])
    };
    let (x0, y0) = (x.clamp(0, *dw as i32 - 1), y.clamp(0, *dh as i32 - 1));
    let ground = px(x0, y0);
    let mut n = 0;
    for yy in y0..(y + h).min(*dh as i32) {
        for xx in x0..(x + w).min(*dw as i32) {
            let c = px(xx, yy);
            let d = c.0.abs_diff(ground.0) as u32 + c.1.abs_diff(ground.1) as u32 + c.2.abs_diff(ground.2) as u32;
            if d > 96 {
                n += 1;
            }
        }
    }
    n
}

fn run_media_steps(s: &mut Session, qmp: &mut Qmp, stick: &Path, work: &Path, size: DisplaySize) -> R<()> {
    let secs = std::time::Duration::from_secs;
    let any = |_: &str| true;
    const APPS_CLICK: (i32, i32) = (60, 12);
    let dump_at = work.join("screen.ppm");
    let look = |qmp: &mut Qmp| -> R<(u32, u32, Vec<u8>)> {
        qmp.screendump(&dump_at)?;
        parse_ppm(&fs::read(&dump_at)?)
    };
    let launch = |qmp: &mut Qmp, s: &mut Session, program: &str| -> R<(u32, i32, i32)> {
        click_at(qmp, s, APPS_CLICK.0, APPS_CLICK.1)?;
        s.expect("desktop-shell: applications menu open")?;
        type_into_menu(qmp, s, program)?;
        press(qmp, "ret")?;
        s.expect(&format!("desktop-shell: launched {program} into its own namespace"))?;
        s.expect("desktop-shell: placed window ")?;
        let placed = s.rest_of_line()?;
        parse_placement(&placed).ok_or_else(|| format!("could not read {program}'s placement from {placed:?}").into())
    };

    s.expect("desktop-session-mgr: greeter presented")?;
    logout_gate_login(qmp, s, size)?;
    let (files_id, fx, fy) = launch(qmp, s, "nxfiles")?;
    let side_x = fx + chrome::CONTENT_X;
    let content_top = fy + chrome::TITLE_BAR_H + browser::MENU_BAR_H + browser::PATH_H;
    let places = HOME_FOLDERS.len() as i32 + 2;
    let drive_top = |i: i32| content_top + (places + i) * browser::ROW_H + browser::DRIVES_HEADING_H;
    let button_w = libui::widget::ROW_BUTTON_W as i32;
    let label_ink = |d: &(u32, u32, Vec<u8>), i: i32| {
        ink_in(d, side_x, drive_top(i), browser::SIDEBAR_W - button_w, browser::ROW_H)
    };
    let button_ink = |d: &(u32, u32, Vec<u8>), i: i32| {
        ink_in(d, side_x + browser::SIDEBAR_W - button_w, drive_top(i), button_w, browser::ROW_H)
    };
    let settled = parse_ppm(&settle_and_capture(qmp, &dump_at)?)?;
    let (root_label, root_button) = (label_ink(&settled, 0), button_ink(&settled, 0));
    if root_label < 30 || root_button != 0 || label_ink(&settled, 1) != 0 {
        return Err(format!(
            "Files' first drive row has {root_label} px of label and {root_button} of button, and its second {} px: \
             the internal disk alone, with no eject button, was expected (dump {})",
            label_ink(&settled, 1),
            dump_at.display()
        )
        .into());
    }
    println!("  ok: Files lists the internal disk in Drives, with no eject button");

    qmp.execute(&format!(
        r#"{{"execute":"blockdev-add","arguments":{{"driver":"raw","node-name":"media","file":{{"driver":"file","filename":"{}"}}}}}}"#,
        stick.display()
    ))?;
    let from = s.transcript().len();
    qmp.execute(
        r#"{"execute":"device_add","arguments":{"driver":"usb-storage","id":"media","bus":"xhci.0","drive":"media","removable":true}}"#,
    )?;
    // No "left clean" on a writable mount's line: the mount has marked it in use.
    s.line_since(from, &format!("fat '{MEDIA_LABEL}'; mounted at /storage/{MEDIA_LABEL} (rw)"), any, secs(60))?;
    let deadline = std::time::Instant::now() + secs(30);
    loop {
        let d = look(qmp)?;
        if label_ink(&d, 1) >= 30 && button_ink(&d, 1) >= 10 {
            break;
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "the stick mounted, and 30 s later Files' second drive row has {} px of label and {} of eject \
                 button: the watch did not wake it (dump {})",
                label_ink(&d, 1),
                button_ink(&d, 1),
                dump_at.display()
            )
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    println!("  ok: the stick mounted writable, and its row and eject button appeared with no input");

    launch(qmp, s, "nxedit")?;
    let _ = settle_and_capture(qmp, &dump_at)?;
    for (i, c) in MEDIA_TEXT.chars().enumerate() {
        let (qcode, shift) = qcode_for(c)?;
        if shift {
            qmp.send_key("shift", true)?;
        }
        press(qmp, &qcode)?;
        if shift {
            qmp.send_key("shift", false)?;
        }
        s.expect(&format!("nxedit: buffer rev {}", i + 1))?;
    }
    let from = s.transcript().len();
    qmp.send_key("ctrl", true)?;
    qmp.send_key("shift", true)?;
    press(qmp, "s")?;
    qmp.send_key("shift", false)?;
    qmp.send_key("ctrl", false)?;
    s.expect("nxedit: choosing a file in /home - ")?;
    s.expect("nxedit: the chooser has the keyboard")?;
    s.line_since(from, "desktop-shell: placed dialog ", any, secs(30))?;
    let placed = s.transcript()[from..]
        .lines()
        .find_map(|l| l.split_once("desktop-shell: placed dialog ").map(|(_, r)| r.to_string()))
        .ok_or("no dialog placement")?;
    let (_, _, dx, dy, dw, dh) = parse_dialog_placement(&placed)
        .ok_or_else(|| format!("could not read the chooser's placement from {placed:?}"))?;
    let up = chooser_up_at(dw, dh)?;
    click_at(qmp, s, dx + up.0, dy + up.1)?;
    s.expect("nxedit: the chooser moved to / - ")?;
    press(qmp, "backspace")?;
    s.expect("nxedit: chooser name so far ")?;
    let left: usize = s
        .rest_of_line()?
        .split_whitespace()
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or("could not read how much of the name was left")?;
    for n in (0..left).rev() {
        press(qmp, "backspace")?;
        s.expect(&format!("nxedit: chooser name so far {n} chars"))?;
    }
    let name = format!("storage/{MEDIA_LABEL}/{MEDIA_FILE}");
    for (i, c) in name.chars().enumerate() {
        let (qcode, shift) = qcode_for(c)?;
        if shift {
            qmp.send_key("shift", true)?;
        }
        press(qmp, &qcode)?;
        if shift {
            qmp.send_key("shift", false)?;
        }
        s.expect(&format!("nxedit: chooser name so far {} chars", i + 1))?;
    }
    press(qmp, "ret")?;
    s.expect(&format!("nxedit: saved /storage/{MEDIA_LABEL}/{MEDIA_FILE} - "))?;
    qmp.send_key("ctrl", true)?;
    press(qmp, "q")?;
    qmp.send_key("ctrl", false)?;
    s.expect("nxedit: quitting")?;
    s.expect("nxedit: closing")?;
    let _ = settle_and_capture(qmp, &dump_at)?;
    println!("  ok: the editor saved onto the stick through Save As: Up to /, then storage/{MEDIA_LABEL}");

    let from = before_eject(s, work);
    let pressed = click_at(qmp, s, side_x + browser::SIDEBAR_W - button_w / 2, drive_top(1) + browser::ROW_H / 2)?;
    if !pressed.contains(&format!("win={files_id}")) {
        return Err(format!("the click on the eject button reached {pressed:?}, not Files' window {files_id}").into());
    }
    s.expect("fs-server: unmounted, and the filesystem recorded clean")?;
    s.expect(&format!("storage-service: unmounted {MEDIA_LABEL}, left clean"))?;
    s.expect(&format!("storage-service: ejected {MEDIA_LABEL}, safe to remove"))?;
    s.expect_all(&[&format!("storage-service: {MEDIA_LABEL} stopped"), &format!("nxfiles: {MEDIA_LABEL} ejected")])?;
    if s.transcript()[from.0..].contains("password") {
        return Err("ejecting from Files asked for a password".into());
    }
    // **The row's label, not its button**: the pointer rests where the button was, and the cursor
    // is drawn into the screen this reads.
    let deadline = std::time::Instant::now() + secs(30);
    while label_ink(&look(qmp)?, 1) != 0 {
        if std::time::Instant::now() > deadline {
            return Err(format!("the ejected stick's row is still drawn (dump {})", dump_at.display()).into());
        }
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // **Stopped, and asked nothing after**, now that Files has read the table again without it.
    stopped_and_left_alone(s, qmp, work, "media", from)?;
    let from = s.transcript().len();
    qmp.execute(r#"{"execute":"device_del","arguments":{"id":"media"}}"#)?;
    s.line_since(from, "storage-service: blk-", |rest: &str| rest.ends_with(" departed"), secs(30))?;
    println!("  ok: ejected from Files with no password, recorded clean, its row gone, and pulled");
    Ok(())
}

/// The transcript's length and the SCSI trace's, before an eject.
pub(crate) fn before_eject(s: &Session, work: &Path) -> (usize, usize) {
    (s.transcript().len(), crate::scsi_trace::log_len(&work.join("scsi.log")))
}

/// **An ejected stick was stopped and asked nothing after**: since `from`, its eject traced, its
/// tray open, no request refused, and no command answered as a missing medium after the eject.
pub(crate) fn stopped_and_left_alone(s: &Session, qmp: &mut Qmp, work: &Path, id: &str, from: (usize, usize)) -> R<()> {
    let (from, log_from) = from;
    let log = fs::read_to_string(work.join("scsi.log")).unwrap_or_default();
    let at = crate::scsi_trace::eject_traced(&log, log_from)
        .ok_or("no START STOP UNIT with LoEj reached the stick: the kernel did not stop it")?;
    let blocks = qmp.execute(r#"{"execute":"query-block"}"#)?;
    if crate::scsi_trace::tray_open(&blocks, id) != Some(true) {
        return Err(format!("the stick's tray is not open after the eject: {blocks}").into());
    }
    if s.transcript()[from..].contains("ejected, and asked for more") {
        return Err("something asked the ejected stick for more: the service read it after the eject".into());
    }
    if crate::scsi_trace::not_ready_after(&log, at) {
        return Err("a command reached the ejected stick: the kernel let a request through after the eject".into());
    }
    println!("  ok: the stick was sent START STOP UNIT and its tray opened, and nothing asked it for more");
    Ok(())
}

/// Where the chooser's Up button is, inside a chooser window `w`×`h`: laid out by `libui` as the
/// editor lays it out, with the theme the image stages, whose `font_px` sizes the title above it.
pub(crate) fn chooser_up_at(w: u32, h: u32) -> R<(i32, i32)> {
    let (theme, _) = staged_theme()?;
    if (w, h) != (libui::chooser::CHOOSER_W, libui::chooser::CHOOSER_H) {
        let (tw, th) = (libui::chooser::CHOOSER_W, libui::chooser::CHOOSER_H);
        return Err(format!("the chooser was placed {w}x{h}, not the toolkit's {tw}x{th}").into());
    }
    let font = host_font(theme.font_ui.as_str())?;
    let mut state = libui::chooser::ChooserState::saving("untitled");
    let view: libui::element::Element<u64> = libui::chooser::view(
        libui::chooser::Mode::Save,
        "/home",
        &[],
        &mut state,
        editor::CHOOSER_KEY,
        None,
        |k| k,
        0,
        0,
        0,
        true,
        &theme,
    );
    let l = libui::layout::layout(
        &view,
        libdraw::geom::Rect::new(0, 0, w, h),
        &libui::paint::FontMetrics::new(&font, theme.font_px),
    );
    let r = libui::layout::locate(&view, &l, editor::CHOOSER_UP_KEY).ok_or("the chooser has no Up button")?;
    Ok((r.origin.x + r.size.w as i32 / 2, r.origin.y + r.size.h as i32 / 2))
}
