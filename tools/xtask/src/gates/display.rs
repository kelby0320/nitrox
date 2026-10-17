//! `check-display`: a screendump compared with the host's render of the same screen.

use crate::*;

/// A smoke gate, run once per display-arm change rather than per commit.
pub(crate) fn cmd_check_display(accel: Accel, size: DisplaySize) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::TestHarness)?;
    let ovmf = locate_ovmf()?;

    let work = repo_root().join("tools/build-cache");
    fs::create_dir_all(&work).ok();
    let qmp_sock = work.join("qmp.sock");
    let shot = work.join("screendump.ppm");
    let _ = fs::remove_file(&qmp_sock);
    let _ = fs::remove_file(&shot);

    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, Some(size))?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
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

    println!("xtask: display gate — booting and capturing the screen…\n");
    let mut session = Session::spawn(cmd, "check-display")?;
    let mut qmp = Qmp::connect(&qmp_sock)?;

    // The guest paces this: capture only once it says the scene is on screen, or a timer would
    // capture a blank frame that looks like a real mismatch.
    //
    // The two faces the guest read: the pixel comparison checks that host and guest agree, so a
    // swap on both sides at once would stay green; these lines pin the guest's half (`host_faces`
    // the host's). First, because `expect` consumes forward and both print before `ui-testclient`
    // opens a window.
    {
        let t = libdraw::theme::Theme::light();
        session.expect(&format!("ui-testclient: font loaded {}", t.font_ui.as_str()))?;
        session.expect(&format!("ui-testclient: font loaded {}", t.font_mono.as_str()))?;
        println!("  ok: the guest loaded the desktop's face and the terminal's, in that order");
    }

    // The deadline that keeps a held configure from becoming a lost window, asserted from the
    // compositor's log because the client cannot see why it was released; the probe below blocks on
    // the manager channel on purpose.
    session.expect("compositor: no manager answer for window")?;

    session.expect("ui-testclient: manager saw created, focus, geometry and destroyed")?;

    // Placement before first paint: the configure must carry the origin the client answered as the
    // manager; a compositor that configured at once passes every other gate and the window would
    // only jump.
    session.expect("ui-testclient: the first configure carried the manager's placement")?;

    session.expect("ui-testclient: a popup was placed by its creator, without the manager")?;

    // The dialog's hold lives in the compositor binary, which no host test builds.
    session.expect("ui-testclient: a dialog was held for the manager and placed by it")?;

    session.expect("ui-testclient: desktop and minimized requests round-tripped through info")?;
    // The second line is the one-manager-at-a-time rule; `ui-testclient` falls back silently to
    // default placement if its resolve fails, so a broken manager path would go unseen.
    session.expect("ui-testclient: reference windows placed and stacked via /dev/draw/manage")?;
    session.expect("ui-testclient: a second /dev/draw/manage was refused")?;

    session.expect("ui-testclient: scene presented via /dev/draw")?;
    // Capture a screen that has stopped changing: the client's line says it is done, not the
    // compositor, whose repaint follows (a capture at once caught a torn frame half the time). Two
    // identical consecutive dumps, not a sleep, which is too short on a loaded machine.
    let captured = match settle_and_capture(&mut qmp, &shot) {
        Ok(b) => b,
        Err(e) => {
            let _ = session.child.kill();
            return Err(e);
        }
    };
    let (w, h, pixels) = parse_ppm(&captured)?;
    println!("  captured {w}x{h} from the guest's display");

    use libdraw::framebuffer::Framebuffer;
    let expected = libdraw::scene::render_reference();
    let (sw, sh) = (libdraw::scene::SCREEN_WIDTH, libdraw::scene::SCREEN_HEIGHT);
    if w < sw || h < sh {
        let _ = session.child.kill();
        return Err(format!("guest display is {w}x{h}, smaller than the {sw}x{sh} scene").into());
    }

    // The cursor is on screen, which nothing else checks: the comparison walks only the scene's
    // top-left region, and the pointer starts at the screen's centre.
    let (cx, cy) = (w / 2, h / 2);
    let body = (0xFFu8, 0xFFu8, 0xFFu8);
    let mut cursor_px = 0usize;
    // The compositor's `CURSOR_H` / `CURSOR_W`, which this crate cannot import.
    for y in cy..(cy + 17).min(h) {
        for x in cx..(cx + 12).min(w) {
            let i = (y as usize * w as usize + x as usize) * 3;
            if (pixels[i], pixels[i + 1], pixels[i + 2]) == body {
                cursor_px += 1;
            }
        }
    }
    if cursor_px == 0 {
        let _ = session.child.kill();
        return Err(format!("no cursor at the screen centre ({cx},{cy}): the pointer sprite is not being drawn").into());
    }
    println!("  ok: cursor visible at ({cx},{cy}) — {cursor_px} body pixels");

    let faces = host_faces()?;

    // The terminal's picture: `libterm` on the host against the guest's render from the font it
    // read off ext4; the only terminal render checked against pixels that reached a screen.
    let (uw, uh) = (libui::reference::WIDTH, libui::reference::HEIGHT);
    let term = reference_frame(&faces, "term")?;
    let (tw, th) = {
        let g = libdraw::framebuffer::Framebuffer::geometry(&term);
        (g.width, g.height)
    };
    let ui = reference_frame(&faces, "ui")?;
    if !(sw <= tw && sh <= th && tw <= uw && th <= uh) {
        return Err(format!(
            "the reference windows are no longer nested: scene {sw}x{sh}, terminal {tw}x{th}, \
             toolkit {uw}x{uh}. `ui-testclient` creates them largest-first because windows stack \
             at the origin in creation order; fix the sizes or the order before the comparison \
             below can mean anything"
        )
        .into());
    }
    // The expected screen is composed as the guest composes it (rounded, shadowed, via
    // `libdraw::compose`, whose unit tests hold the rounding); the gate adds that the guest applies
    // them, and the binding. Light scheme: a self-test boot has no session to send `SetScheme`.
    let shadow = libdraw::theme::window_shadow(libdraw::theme::Scheme::Light);
    let radius = libdraw::corner::WINDOW_RADIUS;
    let composed = {
        use libdraw::compose::SurfaceRef;
        let origin = libdraw::geom::Point::new(0, 0);
        fn at<'a>(
            fb: &'a libdraw::framebuffer::MemFramebuffer,
            origin: libdraw::geom::Point,
            shadow: libdraw::compose::Shadow,
            radius: u32,
        ) -> SurfaceRef<'a> {
            SurfaceRef::new(Framebuffer::geometry(fb), origin, fb.bytes()).with_shadow(shadow).with_corner(radius)
        }
        let mut out = libdraw::framebuffer::MemFramebuffer::new(libdraw::framebuffer::Geometry::packed(
            uw,
            uh,
            libdraw::format::PixelFormat::XRGB8888,
        ));
        let stack =
            [at(&ui, origin, shadow, radius), at(&term, origin, shadow, radius), at(&expected, origin, shadow, radius)];
        libdraw::compose::compose_full(&mut out, libdraw::scene::BACKGROUND, &stack);
        out
    };
    // Except the bottom window's four corner squares: `nxterm` sits beneath these three in the
    // self-test image, so the toolkit window's rounded corners show pixels and a shadow this gate
    // does not render.
    let unknown_beneath = move |x: u32, y: u32| (x < radius || x >= uw - radius) && (y < radius || y >= uh - radius);
    let region = |x: u32, y: u32| {
        if x < sw && y < sh {
            0
        } else if x < tw && y < th {
            1
        } else {
            2
        }
    };
    let mut miss = [0usize; 3];
    let mut compared = [0usize; 3];
    type FirstMiss = (u32, u32, (u8, u8, u8), (u8, u8, u8));
    let mut firsts: [Option<FirstMiss>; 3] = [None; 3];
    if w >= uw && h >= uh {
        for y in 0..uh {
            for x in 0..uw {
                if unknown_beneath(x, y) {
                    continue;
                }
                let k = region(x, y);
                compared[k] += 1;
                let want = Framebuffer::get_pixel(&composed, x, y).unwrap_or_default();
                let i = (y as usize * w as usize + x as usize) * 3;
                let got = (pixels[i], pixels[i + 1], pixels[i + 2]);
                if got != (want.r, want.g, want.b) {
                    miss[k] += 1;
                    if firsts[k].is_none() {
                        firsts[k] = Some((x, y, got, (want.r, want.g, want.b)));
                    }
                }
            }
        }
    }
    let (mismatches, term_mismatches, ui_mismatches) = (miss[0], miss[1], miss[2]);
    let (first, term_first, ui_first) = (firsts[0], firsts[1], firsts[2]);
    let (term_compared, ui_compared) = (compared[1], compared[2]);

    // Only when the static comparison passed: a second failure would hide the first.
    if mismatches == 0
        && term_mismatches == 0
        && ui_mismatches == 0
        && let Err(e) = desktop_round_trip(&mut qmp, &mut session, &work, w, sw, sh, &composed, radius)
    {
        let _ = session.child.kill();
        let _ = fs::remove_file(&qmp_sock);
        return Err(e);
    }

    // `boot-probe` passed on this machine: judged after the screen, so a failed probe cannot hide a
    // mismatch.
    let probe = expect_probe_pass(&session);
    let _ = session.child.kill();
    let _ = fs::remove_file(&qmp_sock);

    if mismatches > 0 {
        let (x, y, got, want) = first.expect("a mismatch was counted");
        return Err(format!(
            "display gate FAILED: {mismatches} of {} scene pixels differ.\n  \
             first at ({x},{y}): screen {got:?}, expected {want:?}\n  \
             the capture is at {} — a whole-image shift suggests a base-address or stride \
             error, and swapped components suggest a channel-order one",
            compared[0],
            shot.display()
        )
        .into());
    }
    if w < tw || h < th {
        return Err(format!("guest display is {w}x{h}, smaller than the {tw}x{th} reference terminal").into());
    }
    if term_mismatches > 0 {
        let (x, y, got, want) = term_first.expect("a mismatch was counted");
        return Err(format!(
            "display gate FAILED: {term_mismatches} of {term_compared} terminal pixels differ.\n  \
             first at ({x},{y}): screen {got:?}, expected {want:?}\n  \
             the capture is at {} — if the whole region is one colour the guest never presented \
             the reference terminal; if only some cells differ, suspect the attribute or cursor \
             render rather than the binding",
            shot.display()
        )
        .into());
    }
    if w < uw || h < uh {
        return Err(format!("guest display is {w}x{h}, smaller than the {uw}x{uh} reference UI").into());
    }
    if ui_mismatches > 0 {
        let (x, y, got, want) = ui_first.expect("a mismatch was counted");
        return Err(format!(
            "display gate FAILED: {ui_mismatches} of {ui_compared} toolkit pixels differ.\n  \
             first at ({x},{y}): screen {got:?}, expected {want:?}\n  \
             the capture is at {} — if the whole region is the background colour the guest \
             never loaded the UI font or never presented its window; if \
             only the glyphs differ, the target rasterised differently from the host",
            shot.display()
        )
        .into());
    }
    probe?;
    println!("  ok: boot-probe's verdict is PASS");
    println!(
        "\nxtask: display gate PASSED — {} pixels of the {sw}x{sh} scene, {term_compared} of the \
         {tw}x{th} terminal and {ui_compared} of the {uw}x{uh} toolkit window match libdraw, \
         libterm and libui pixel for pixel, rounded corners and shadows composed ✓",
        compared[0]
    );
    Ok(())
}

/// Inject the test client's chord, capture the switched screen, and switch back. Both ways: the
/// scene returning with no further commit says a switch is a filter changing its mind.
#[allow(
    clippy::too_many_arguments,
    reason = "the screen and scene geometry are separate scalars the caller already holds"
)]
fn desktop_round_trip(
    qmp: &mut Qmp,
    session: &mut Session,
    work: &Path,
    screen_w: u32,
    sw: u32,
    sh: u32,
    composed: &libdraw::framebuffer::MemFramebuffer,
    radius: u32,
) -> R<()> {
    let shot = work.join("screendump-desktop.ppm");

    // Injected before the client listens, the chord would reach the focused window as a key.
    session.expect("ui-testclient: hotkey registered, waiting")?;

    let chord = |qmp: &mut Qmp| -> R<()> {
        qmp.send_key("meta_l", true)?;
        qmp.send_key("f1", true)?;
        qmp.send_key("f1", false)?;
        qmp.send_key("meta_l", false)?;
        Ok(())
    };

    chord(qmp)?;
    session.expect("ui-testclient: hotkey fired -> desktop 2")?;
    let switched = settle_and_capture(qmp, &shot)?;
    let (w2, _, px2) = parse_ppm(&switched)?;
    if w2 != screen_w {
        return Err(format!("the switched capture is {w2} wide, the first was {screen_w}").into());
    }

    // Desktop 2 shows only the background where the reference windows were: a filter that did not
    // run leaves them here. The compositor clears to the constant the reference render fills with.
    let bgc = libdraw::scene::BACKGROUND;
    let bg = (bgc.r, bgc.g, bgc.b);
    let mut lit = 0usize;
    let mut first_lit = None;
    for y in 0..sh {
        for x in 0..sw {
            let i = (y as usize * w2 as usize + x as usize) * 3;
            let got = (px2[i], px2[i + 1], px2[i + 2]);
            if got != bg {
                lit += 1;
                if first_lit.is_none() {
                    first_lit = Some((x, y, got));
                }
            }
        }
    }
    if lit > 0 {
        let (x, y, got) = first_lit.expect("a lit pixel was counted");
        return Err(format!(
            "display gate FAILED: {lit} of {} pixels are still drawn after switching to an \
             empty desktop.\n  first at ({x},{y}): screen {got:?}, expected the background \
             {bg:?}\n  the capture is at {} — the compositor is compositing windows that are \
             not on the current desktop, or switching did not repaint",
            sw as usize * sh as usize,
            shot.display()
        )
        .into());
    }
    println!("  ok: desktop 2 is empty — {} scene pixels are background", sw as usize * sh as usize);

    chord(qmp)?;
    session.expect("ui-testclient: hotkey fired -> desktop 1")?;
    let restored = settle_and_capture(qmp, &shot)?;
    let (w3, _, px3) = parse_ppm(&restored)?;
    use libdraw::framebuffer::Framebuffer;
    let expected: &libdraw::framebuffer::MemFramebuffer = composed;
    let mut bad = 0usize;
    let mut first_bad = None;
    for y in 0..sh {
        for x in 0..sw {
            if x < radius && y < radius {
                continue;
            }
            let want = Framebuffer::get_pixel(expected, x, y).unwrap_or_default();
            let i = (y as usize * w3 as usize + x as usize) * 3;
            let got = (px3[i], px3[i + 1], px3[i + 2]);
            if got != (want.r, want.g, want.b) {
                bad += 1;
                if first_bad.is_none() {
                    first_bad = Some((x, y, got, (want.r, want.g, want.b)));
                }
            }
        }
    }
    if bad > 0 {
        let (x, y, got, want) = first_bad.expect("a mismatch was counted");
        return Err(format!(
            "display gate FAILED: {bad} scene pixels differ after switching back.\n  \
             first at ({x},{y}): screen {got:?}, expected {want:?}\n  the capture is at {} — \
             the window came back changed, so a desktop switch is not the pure filter it is \
             specified to be",
            shot.display()
        )
        .into());
    }
    println!("  ok: switching back restored the scene pixel for pixel, with no client commit");
    Ok(())
}

/// The last complete line of `fresh` containing `prefix`, what follows `prefix` on it trimmed, and
/// how many bytes are whole lines (a half-written `hover 10` is not row 10).
pub(crate) fn last_receipt(fresh: &str, prefix: &str) -> (Option<String>, usize) {
    let whole = fresh.rfind('\n').map_or(0, |i| i + 1);
    let line = fresh[..whole].lines().rfind(|l| l.contains(prefix));
    let rest = line.map(|l| l[l.find(prefix).map_or(0, |i| i + prefix.len())..].trim().to_string());
    (rest, whole)
}
