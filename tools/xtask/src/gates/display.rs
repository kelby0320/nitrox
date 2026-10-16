//! `check-display`: a screendump compared with the host's render of the same screen.

use crate::*;

/// A **smoke gate, not a per-commit one**: it boots a full image and compares an image,
/// so the plan runs it once per display-arm change.
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
        // The machine protocol channel: `screendump` here, `input-send-event` in M3.
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

    // The guest paces this: capture only once it says the scene is on the screen.
    // Screendumping on a timer would race the boot and produce a blank or half-drawn
    // frame that looks like a real mismatch.
    //
    // Since M2 Part D the scene arrives through the **whole Surface protocol** — a real
    // client sharing memory with the compositor — rather than being written straight to
    // the aperture. The client emits this only after a `Release` acknowledges its final
    // commit, so the frame is composited by the time we capture.
    // **Which two faces the guest read, in the order it reads them** (M11 Part D).
    //
    // The pixel comparison below cannot make this claim on its own, and the reason is worth
    // stating because it is the trap this whole gate is shaped around: it checks that the host
    // and the guest *agree*, so a swap that happened on both sides at once would still be green.
    // Rendering the toolkit's window in the terminal's font is exactly that kind of change —
    // one constant, two call sites — and it is what the system did before this part.
    //
    // These lines pin the guest's half. The host's is pinned by `host_faces`, which takes both
    // paths from `Theme::light()`, and by `libdraw`'s own test that the two are different files.
    //
    // **First, because `expect` consumes forward.** Both are printed before `ui-testclient`
    // opens a window at all, so asserting them after any line further down the boot scans past
    // them and times out on output that was there — the failure mode PR #258's review named.
    {
        let t = libdraw::theme::Theme::light();
        session.expect(&format!("ui-testclient: font loaded {}", t.font_ui.as_str()))?;
        session.expect(&format!("ui-testclient: font loaded {}", t.font_mono.as_str()))?;
        println!("  ok: the guest loaded the desktop's face and the terminal's, in that order");
    }

    // **The deadline that keeps a held configure from becoming a lost window (M6 B4).**
    //
    // Asserted from the compositor's own log because the client cannot see *why* it was
    // released — a configure is a configure. The trigger is deterministic rather than lucky:
    // the B3 probe below calls `Window::new` while holding the manager channel, so it blocks
    // waiting for an answer that only it could give and provably will not. That is exactly the
    // wedged-shell case, produced on purpose.
    //
    // Without this line the deadline is still load-bearing — remove it and the probe hangs —
    // but the gate would report a 45-second timeout rather than naming what broke.
    session.expect("compositor: no manager answer for window")?;

    // **The other half of the seam (M6 B3): the manager is *told*, not just obeyed.** B1
    // proved a manager can act on the compositor; nothing proved the compositor reports back.
    // The client watches one window's whole life on the manager channel — created, focus,
    // geometry after a move, destroyed — and `fail()`s naming whichever record did not
    // arrive, so a broken event path reports itself here rather than as a screen that happens
    // to still match. The probe window is destroyed before the scene is captured, so it
    // changes no pixel this gate compares, and it runs *first* — creating and destroying a
    // window is the noisiest thing this client does, so it happens before the placements
    // rather than leaving a full-screen repaint racing the capture.
    session.expect("ui-testclient: manager saw created, focus, geometry and destroyed")?;

    // **Placement before first paint (M6 B4): the ordering rule, not just the event.** The
    // client creates a window through the raw transport, checks that no configure has arrived,
    // answers as the manager with an origin nothing else produces, and only then reads the
    // configure — asserting it carries that origin. A compositor that sent the configure at
    // once still passes every other gate here: the window would simply appear at the default
    // origin and jump when placed, which no screen comparison taken afterwards can see.
    session.expect("ui-testclient: the first configure carried the manager's placement")?;

    // **A popup is placed by its creator (M6 C1).** The client creates a parent, has the manager
    // move it somewhere the default never puts a window, then creates a popup at a negative-x
    // offset and reads the geometry back through `/dev/draw/<id>/info`. An offset resolved
    // against the origin instead of against the parent is a different answer, and a popup held
    // for a manager that by design never places popups would still be waiting.
    session.expect("ui-testclient: a popup was placed by its creator, without the manager")?;

    // **And the other parented role goes the other way.** A `dialog` names a parent but a
    // manager places it, so it is held like a `normal` and its creator's offset is ignored. The
    // hold lives in the compositor *binary*, which no host test builds — merge the two roles
    // back into one arm and every other gate here stays green.
    session.expect("ui-testclient: a dialog was held for the manager and placed by it")?;

    // **The manager seam, asserted rather than attempted (M6 B1).** `ui-testclient` places its
    // reference windows through `/dev/draw/manage` and falls back to the compositor's default
    // placement if the resolve fails — deliberately, since it is a test fixture and not a real
    // session's manager. That fallback is silent by design, which means without these two lines
    // the whole manager path could break and every gate would still pass: the windows land at
    // the origin either way, because the origin *is* the default.
    //
    // The second line is the contract B1 exists to state: one manager at a time. The compositor
    // refuses a second resolve rather than deposing the first, and until the client asked twice
    // that branch was unreachable, so the rule was pinned by nothing at all.
    // **M8 Part A: the desktop requests, driven the way a client drives them.** The filtering
    // itself is pinned by `compositor`'s own tests, over the same functions the binary calls.
    // What those cannot reach is the wire — that a body encoded by a client, sent down
    // `/dev/draw/manage`, lands in the right arm of `dispatch` and is answered — which is the
    // gap PR #233's title cap fell into. The client sets a desktop and a minimized flag, reads
    // each back through `/dev/draw/<id>/info`, checks a current desktop of 0 is refused, and
    // restores everything, so this changes no pixel the comparison below walks.
    session.expect("ui-testclient: desktop and minimized requests round-tripped through info")?;
    // **Placed and then raised, in the order the comparison below composes them.** The stacking
    // used to be creation order, which raced `nxterm`'s window against these three; the client
    // now raises them itself, so `nxterm` only has to *exist* first.
    session.expect("ui-testclient: reference windows placed and stacked via /dev/draw/manage")?;
    session.expect("ui-testclient: a second /dev/draw/manage was refused")?;

    session.expect("ui-testclient: scene presented via /dev/draw")?;
    // **Capture a screen that has stopped changing, rather than the first one offered.**
    //
    // The client's line says *it* is done, not that the compositor is: a repaint is work the
    // compositor does after the request that caused it was answered, and recompositing
    // 1280x800 is not instant. Capturing immediately caught a torn frame — background filled,
    // windows not yet composited, no cursor — about half the time once `ui-testclient` began
    // creating and destroying a window near the end of its run (M6 B3).
    //
    // Two identical consecutive dumps, not a sleep: a fixed delay is a guess that either
    // wastes time or is too short on a loaded machine, and silently becomes too short again
    // the next time the scene grows. This waits for the property the gate actually needs, and
    // it cannot pass a screen that is wrong but stable — the comparison below still runs.
    let captured = match settle_and_capture(&mut qmp, &shot) {
        Ok(b) => b,
        Err(e) => {
            let _ = session.child.kill();
            return Err(e);
        }
    };
    let (w, h, pixels) = parse_ppm(&captured)?;
    println!("  captured {w}x{h} from the guest's display");

    // The expected image, rendered here rather than stored.
    use libdraw::framebuffer::Framebuffer;
    let expected = libdraw::scene::render_reference();
    let (sw, sh) = (libdraw::scene::SCREEN_WIDTH, libdraw::scene::SCREEN_HEIGHT);
    if w < sw || h < sh {
        let _ = session.child.kill();
        return Err(format!("guest display is {w}x{h}, smaller than the {sw}x{sh} scene").into());
    }

    // **The cursor is on screen**, which nothing else here checks: the comparison below walks
    // only the scene's region at the top-left, and the pointer starts at the screen's centre.
    // Without this the sprite could fail to draw at all and every gate would stay green.
    let (cx, cy) = (w / 2, h / 2);
    let body = (0xFFu8, 0xFFu8, 0xFFu8);
    let mut cursor_px = 0usize;
    // The compositor's `CURSOR_H` / `CURSOR_W`, which this crate cannot import — it depends on
    // `libdraw`, `libui` and `libterm`, not on `compositor`.
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

    // The font both reference renders are drawn with here — the same file the image build
    // stages at `/system/fonts/`, against a guest render made from the bytes it read off the
    // disk. This is the only check anywhere that a font loads on the target at all.
    let faces = host_faces()?;

    // **The terminal's picture**, between the two. Same construction and the same argument as
    // the toolkit's: `libterm` on the host renders the fixed reference stream, the guest renders
    // it from the font it read off ext4, and the two are compared where the smaller windows
    // above do not cover. This is the only place a terminal render is checked against pixels
    // that actually reached a screen — every other check on it is the guest agreeing with
    // itself.
    // **Taken from `preview_frames`, which is what makes "a second entry point, not a second
    // renderer" true rather than claimed.** Both were built here independently until PR #261's
    // review demonstrated the gap: a preview replaced by a solid rectangle *at the gate's
    // dimensions* left every host test passing, because nothing tied the two together but a
    // comment. One source means the drift is not possible, and a wrong source fails here —
    // against a real guest, which is the only thing that can tell a render from a picture.
    let (uw, uh) = (libui::reference::WIDTH, libui::reference::HEIGHT);
    let term = reference_frame(&faces, "term")?;
    // The terminal's size comes from the frame rather than from `reference::size`, for the same
    // reason: two answers to one question are two answers that can differ. The host test still
    // compares them, so the agreement is asserted somewhere.
    let (tw, th) = {
        let g = libdraw::framebuffer::Framebuffer::geometry(&term);
        (g.width, g.height)
    };
    let ui = reference_frame(&faces, "ui")?;
    // The stacking the composition below assumes, stated rather than trusted: each window must sit
    // wholly inside the one beneath it, or a region the gate believes it is comparing is covered
    // by something it is not comparing against — a hole that would be silent.
    if !(sw <= tw && sh <= th && tw <= uw && th <= uh) {
        return Err(format!(
            "the reference windows are no longer nested: scene {sw}x{sh}, terminal {tw}x{th}, \
             toolkit {uw}x{uh}. `ui-testclient` creates them largest-first because windows stack \
             at the origin in creation order; fix the sizes or the order before the comparison \
             below can mean anything"
        )
        .into());
    }
    // **The expected screen is composed here, the way the guest composes it** (desktop refresh,
    // Part B). The three windows are rounded now, so each one's corners show what is beneath —
    // the scene's the terminal's, the terminal's the toolkit window's, and the toolkit window's
    // the desktop — and each casts its shadow over what is below and into its own corners. This
    // was built by hand until then: each render, the shadow of the window above painted onto it,
    // and the window above excluded. Rounding would have meant three more exclusions and a
    // hand-written blend per curve, so the pictures are stacked through `libdraw::compose`
    // instead, with the guest's own radius and shadow.
    //
    // **What is still computed independently is the part that matters**: the three pictures come
    // from `libdraw`'s scene, `libterm` and `libui`, and compose's rounding and shadowing are
    // held by their own unit tests. What this gate adds is that the guest applies them — a
    // compositor that forgot to round, or to shadow, fails here — and the binding: a wrong base,
    // stride or channel order.
    //
    // **The light scheme's shadow**, because nothing in a self-test boot says otherwise: there is
    // no session, so no shell sends `SetScheme`, and the compositor keeps the scheme it starts in.
    let shadow = libdraw::theme::window_shadow(libdraw::theme::Scheme::Light);
    let radius = libdraw::corner::WINDOW_RADIUS;
    let composed = {
        use libdraw::compose::SurfaceRef;
        let origin = libdraw::geom::Point::new(0, 0);
        // A function rather than a closure, so the borrow it returns can be named.
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
        // Bottom first, as `ui-testclient` created them — the nesting above is what makes this
        // the guest's order.
        let stack =
            [at(&ui, origin, shadow, radius), at(&term, origin, shadow, radius), at(&expected, origin, shadow, radius)];
        libdraw::compose::compose_full(&mut out, libdraw::scene::BACKGROUND, &stack);
        out
    };
    // **Except the bottom window's four corner squares**, and only those. The self-test image runs
    // `nxterm` as a service, and with no manager its window sits at the origin *beneath* these
    // three — invisible while they were square, because the toolkit window covered its rectangle
    // whole. Rounded, the toolkit window's corners show `nxterm`'s pixels and its shadow, which
    // this gate does not render. Everywhere else the toolkit window is opaque over whatever is
    // under it, so nothing else can reach the comparison; the scene's and the terminal's corners
    // show only windows this gate *does* render, and those are compared exactly — which is what
    // proves the guest rounds at all. The first run of the rounded gate failed on exactly these
    // pixels, first at the shared corner (0,0).
    let unknown_beneath = move |x: u32, y: u32| (x < radius || x >= uw - radius) && (y < radius || y >= uh - radius);
    // One pass, counted per window so a failure still says which picture it was in.
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

    // **M8 Part B: the screendump Part A could not take.** Part A's gate box asked for a
    // switched screen compared against a `libdraw` render, and could not have it: the guest had
    // no way to be *told* to switch, because this client parked on `sys_wait` rather than
    // reading anything. A registered chord is that channel — the host injects it over QMP, the
    // guest acts on it, and the screen captured is one the host asked for.
    //
    // **Only when the static comparison already passed.** A switched screen means nothing if
    // the unswitched one was wrong, and running it anyway would report the second failure while
    // hiding the first.
    if mismatches == 0
        && term_mismatches == 0
        && ui_mismatches == 0
        && let Err(e) = desktop_round_trip(&mut qmp, &mut session, &work, w, sw, sh, &composed, radius)
    {
        let _ = session.child.kill();
        let _ = fs::remove_file(&qmp_sock);
        return Err(e);
    }

    // **And `boot-probe` passed on this machine** (PR #358 review): read now, judged after the
    // screen, so a probe that failed cannot hide a mismatch.
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

/// Inject the test client's chord, capture the switched screen, and switch back.
///
/// **What this proves that a host test cannot.** `compositor`'s own tests pin the desktop filter
/// over `compose_into`, which is the function the binary composites with — so the *logic* is
/// covered there. What only a screendump can show is the guest being consistent with itself and
/// wrong: a filter that runs but composites into the wrong place, or a switch that leaves the
/// previous frame on screen because nothing repainted. That is the class `check-display` exists
/// for, and until a chord existed there was no way to ask the guest to enter the state.
///
/// The round trip is deliberately both ways. A one-way check passes for a compositor that
/// filtered a window out permanently, or dropped its buffer on the way — the scene coming *back*
/// with no further commit is what says a desktop switch is a filter changing its mind.
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

    // The client says when it is listening. Injecting before that is a race the guest would
    // lose silently — the chord would be delivered to the focused window as an ordinary key.
    session.expect("ui-testclient: hotkey registered, waiting")?;

    // `Super+F1`. The modifier goes down first and comes up last, which is what a person does
    // and what `libinput`'s modifier tracking expects.
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

    // **Every pixel of the scene region is the background.** The reference windows are all on
    // desktop 1, so desktop 2 shows nothing — and this region is where they were, so a filter
    // that did not run leaves their content exactly here.
    // The compositor clears to `libdraw::scene::BACKGROUND` — the same constant the reference
    // render fills with, so host and guest cannot disagree about what "empty" looks like.
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

    // And back. No client commits anything in between, so the scene returning is the filter
    // changing its mind rather than a redraw.
    chord(qmp)?;
    session.expect("ui-testclient: hotkey fired -> desktop 1")?;
    let restored = settle_and_capture(qmp, &shot)?;
    let (w3, _, px3) = parse_ppm(&restored)?;
    use libdraw::framebuffer::Framebuffer;
    // **The composed screen, not the scene's own render** (desktop refresh, Part B): the scene's
    // window is rounded, so its corners show the terminal beneath it, and that is what coming
    // back unchanged has to reproduce.
    let expected: &libdraw::framebuffer::MemFramebuffer = composed;
    let mut bad = 0usize;
    let mut first_bad = None;
    for y in 0..sh {
        for x in 0..sw {
            // The shared corner at the origin shows `nxterm` beneath, which the comparison above
            // also leaves out — see `unknown_beneath` there.
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

/// The last **complete** line of `fresh` containing `prefix` — what follows `prefix` on it,
/// trimmed — and how many bytes of `fresh` are whole lines. See [`Session::settled_line`].
///
/// **Whole lines only**: the reader appends as bytes arrive, and a half-written `hover 10` is not
/// row 10. The count lets the caller stop at the last complete line and read the rest next time.
pub(crate) fn last_receipt(fresh: &str, prefix: &str) -> (Option<String>, usize) {
    let whole = fresh.rfind('\n').map_or(0, |i| i + 1);
    let line = fresh[..whole].lines().rfind(|l| l.contains(prefix));
    let rest = line.map(|l| l[l.find(prefix).map_or(0, |i| i + prefix.len())..].trim().to_string());
    (rest, whole)
}
