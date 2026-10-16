use crate::*;

/// `cargo xtask check-display` — prove the pixels actually reach the screen.
///
/// **What a self-hash structurally cannot answer** (`docs/design/display-substrate.md`
/// §8c): a compositor can hash its own buffer correctly while writing to the wrong base
/// address, the wrong stride, or with the channels swapped. Nothing inside the guest can
/// detect that, because the guest stays perfectly consistent with itself. So the picture
/// has to be read from outside, which is what QEMU's `screendump` is for.
///
/// The division worth remembering: **`test-qemu` tests the compositor, this tests the
/// framebuffer binding.**
///
/// **No golden image.** The expected picture is rendered here by the same `libdraw` the
/// guest uses, so there is no binary artefact in the repo to regenerate, and no
/// brittle-image maintenance problem. That is sound precisely because what is under test
/// is the *binding* — base address, stride, channel order — not the compositing, which
/// §8b already covers.
///
/// `cargo xtask preview [ui|ui-dark|term|all]` — render the toolkit on the host and write it as a PNG.
///
/// **The whole point is that a judgement about how something looks should cost a glance rather
/// than a boot** (M11 Part A). Polish is a hundred small decisions, and a decision that costs
/// three minutes of QEMU is a decision not made — so this puts the same renders `check-display`
/// adjudicates against into a file anyone can open, plus `ui-dark`, the one no gate adjudicates
/// (see [`preview_frames`]).
///
/// **The same renderer, deliberately.** `xtask` already links `libui`, `libdraw` and `libterm`
/// because the display gate renders the expected picture here rather than checking in a golden
/// file. This is a second entry point onto that, not a second renderer: a preview that could
/// differ from what the gate demands of the guest would be a picture of nothing in particular.
///
/// **What it cannot show**, said plainly so nobody reads more into it: anything the *compositor*
/// draws — the cursor, the drag outline, the background between windows — and the arrangement of
/// real windows on a real screen. Those are composed in the guest by clients that have to run.
/// What this covers is the toolkit's own surfaces, which is where most of the polish lives.
pub(crate) fn cmd_preview(what: &str) -> R<()> {
    let faces = host_faces()?;
    let dir = build_cache();
    fs::create_dir_all(&dir).ok();
    let frames = preview_frames(&faces);
    let names: Vec<&str> = frames.iter().map(|(n, _)| *n).collect();
    if what != "all" && !names.contains(&what) {
        // Checked before anything is drawn: the first version reported this by rendering both
        // references a second time purely to list their names.
        return Err(format!("no preview called {what:?} — try `all` or one of: {}", names.join(", ")).into());
    }
    for (name, frame) in &frames {
        if what != "all" && what != *name {
            continue;
        }
        let (w, h, rgb) = rgb_of(frame);
        let path = dir.join(format!("preview-{name}.png"));
        write_png(&path, w, h, &rgb)?;
        println!("xtask: {} ({w}x{h})", path.display());
    }
    Ok(())
}

/// `cargo xtask tune [--ground N] [--side N] [--radius N] [--strength N] [--drop N]
/// [--contact-radius N] [--contact-strength N] [--contact-drop N] [--corner N] [--dark]` — try the
/// overview's opacity and a window's shadow **without booting**.
///
/// The shadow has two layers since the desktop refresh's Part A: `--radius`, `--strength` and
/// `--drop` are the wide one, as they always were, and the `--contact-` three are the tight one
/// that seats a window's edge. `--corner` rounds the mock windows, so a shadow can be judged
/// against the corners it will actually be cast from.
///
/// **The loop `preview` exists for, pointed at the two things `preview` could not show** (M13
/// Part C). `preview` renders the toolkit's own surfaces; the overview's translucency and a
/// window's shadow are *compositing*, which happened only in the guest — so judging either cost a
/// three-minute boot, and picking a number by eye needs half a dozen looks.
///
/// **The desktop underneath is the real one**, read back from `shot-windows.png` when a
/// `cargo xtask shot` has left one there, and the wallpaper otherwise. That is what makes the
/// answer trustworthy: the question is how much of the *actual* desktop should show through, and
/// a mock of it would be a picture of my guess rather than of the screen. The overview's own
/// content is stood in for — rectangles where thumbnails go — because the thing being judged is
/// the ground behind them, not the labels on them.
///
/// It writes `tune-shadow.png` and `tune-overview.png`, and prints the values it used so a good
/// one can be copied into `libdraw::theme::window_shadow` and `desktop-shell`'s constants.
///
/// `--dark` starts from the dark scheme's shadow and draws the mock windows in the dark scheme,
/// because a shadow's strength is judged against the windows it falls on.
pub(crate) fn cmd_tune(args: &[String]) -> R<()> {
    use libdraw::compose::{Shadow, ShadowLayer, SurfaceRef, compose_exposed};
    use libdraw::format::{PixelFormat, Rgb};
    use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
    use libdraw::geom::{Point, Rect};

    let num = |name: &str, default: u32| -> R<u32> {
        match args.iter().position(|a| a == name) {
            Some(i) => args
                .get(i + 1)
                .ok_or_else(|| format!("{name} wants a number"))?
                .parse::<u32>()
                .map_err(|e| format!("{name}: {e}").into()),
            None => Ok(default),
        }
    };
    let scheme =
        if args.iter().any(|a| a == "--dark") { libdraw::theme::Scheme::Dark } else { libdraw::theme::Scheme::Light };
    let [wide, contact] = libdraw::theme::window_shadow(scheme).layers;
    let shadow = Shadow {
        layers: [
            ShadowLayer {
                radius: num("--radius", wide.radius)?,
                offset: Point::new(0, num("--drop", wide.offset.y as u32)? as i32),
                colour: wide.colour,
                strength: num("--strength", wide.strength as u32)?.min(255) as u8,
            },
            ShadowLayer {
                radius: num("--contact-radius", contact.radius)?,
                offset: Point::new(0, num("--contact-drop", contact.offset.y as u32)? as i32),
                colour: wide.colour,
                strength: num("--contact-strength", contact.strength as u32)?.min(255) as u8,
            },
        ],
    };
    // What ships, like every other default here: floating windows are rounded since Part B.
    let corner = num("--corner", libdraw::corner::WINDOW_RADIUS)?;
    let ground_alpha = num("--ground", 210)?.min(255) as u8;
    let side_alpha = num("--side", 150)?.min(255) as u8;

    let dir = build_cache();
    fs::create_dir_all(&dir).ok();
    // **The size of the screen that was photographed**, when one was — it is the real desktop an
    // opacity is judged over — and the gates' size for the mock scene otherwise. It was a fixed
    // 1280×800 until Phase 5 Part E, which silently ignored a screendump of any other size.
    let shot = dir.join("shot-windows.png");
    let from_shot = read_rgb_png(&shot).ok();
    let (sw, sh) = from_shot.as_ref().map_or((DisplaySize::GATE.w, DisplaySize::GATE.h), |(w, h, _)| (*w, *h));
    let g = Geometry::packed(sw, sh, PixelFormat::XRGB8888);

    // --- the desktop underneath: the real screen if one has been photographed ---
    let mut desktop = MemFramebuffer::new(g);
    match &from_shot {
        Some((_, _, rgb)) => {
            for y in 0..sh {
                for x in 0..sw {
                    let i = (y as usize * sw as usize + x as usize) * 3;
                    desktop.put_pixel(x, y, Rgb::new(rgb[i], rgb[i + 1], rgb[i + 2]));
                }
            }
        }
        None => {
            let (px, wg) = wallpaper_for_screen(sw, sh)?;
            for y in 0..sh {
                for x in 0..sw {
                    let off = wg.offset_of(x, y).unwrap_or(0);
                    let word = u32::from_le_bytes([px[off], px[off + 1], px[off + 2], px[off + 3]]);
                    desktop.put_pixel(x, y, wg.format.decode(word));
                }
            }
        }
    }

    // --- 1. the shadow, over a wallpaper with two mock windows ---
    //
    // Drawn fresh rather than over the screendump, because the screendump already *has* the
    // shipped shadow baked into it: comparing a new one against it would be comparing two
    // shadows and calling the sum an answer.
    let (wp, wg) = wallpaper_for_screen(sw, sh)?;
    let mut shot_fb = MemFramebuffer::new(g);
    let faces = host_faces()?;
    let ui = reference_frame(&faces, if scheme == libdraw::theme::Scheme::Dark { "ui-dark" } else { "ui" })?;
    let (uw, uh) = {
        let ug = Framebuffer::geometry(&ui);
        (ug.width, ug.height)
    };
    // The top bar, as a strip of its ground: 30 pixels of `panel` since the desktop refresh's
    // Part C.
    let panel = MemFramebuffer::filled(
        Geometry::packed(sw, 30, PixelFormat::XRGB8888),
        libdraw::theme::Theme::for_scheme(scheme).panel,
    );
    let wall = SurfaceRef::new(wg, Point::new(0, 0), &wp);
    let bar = SurfaceRef::new(panel.geometry(), Point::new(0, 0), panel.bytes());
    let a = SurfaceRef::new(Framebuffer::geometry(&ui), Point::new(120, 140), ui.bytes())
        .with_shadow(shadow)
        .with_corner(corner);
    let b =
        SurfaceRef::new(Framebuffer::geometry(&ui), Point::new(120 + uw as i32 / 2, 140 + uh as i32 / 2), ui.bytes())
            .with_shadow(shadow)
            .with_corner(corner);
    compose_exposed(&mut shot_fb, Rgb::new(0x2A, 0x55, 0x70), &[wall, bar, a, b], &[g.bounds()]);
    let (w1, h1, rgb1) = rgb_of(&shot_fb);
    let p1 = dir.join("tune-shadow.png");
    write_png(&p1, w1, h1, &rgb1)?;

    // --- 2. the overview's ground and sidebar, over the real desktop ---
    let mut over = desktop;
    let side_w = 200u32;
    // **The bars stay uncovered**, which is what the guest does — the shell's two panels sit above
    // the overview popup, and a mock that dimmed them would show a picture the screen never has.
    // Compare `shot-overview.png`: its top bar and taskbar are at full brightness.
    let bar_h = 24u32;
    let body = Rect::new(0, bar_h as i32, sw, sh - bar_h * 2);
    let side = Rect::new((sw - side_w) as i32, bar_h as i32, side_w, sh - bar_h * 2);
    // The ground first, then the sidebar over it, then opaque stand-ins for the thumbnails —
    // the same order `render_overview` uses, so the layering is the shell's rather than a
    // rearrangement of it.
    blend_rect(&mut over, body, Rgb::new(0, 0, 0), ground_alpha);
    blend_rect(&mut over, side, Rgb::new(0x1B, 0x3A, 0x4E), side_alpha);
    for (i, r) in [Rect::new(60, 90, 420, 300), Rect::new(540, 210, 400, 280)].iter().enumerate() {
        let tint = if i == 0 { Rgb::new(0xF2, 0xF2, 0xF2) } else { Rgb::new(0x10, 0x14, 0x18) };
        over.fill_rect(*r, tint);
    }
    for row in 0..2u32 {
        over.fill_rect(
            Rect::new(side.origin.x + 12, bar_h as i32 + 16 + row as i32 * 90, side_w - 90, 70),
            Rgb::new(0x9F, 0xB8, 0xC8),
        );
    }
    let (w2, h2, rgb2) = rgb_of(&over);
    let p2 = dir.join("tune-overview.png");
    write_png(&p2, w2, h2, &rgb2)?;

    let [w, c] = shadow.layers;
    println!(
        "xtask: shadow radius {} drop {} strength {}, contact radius {} drop {} strength {}, \
         corner {corner}  ->  {}",
        w.radius,
        w.offset.y,
        w.strength,
        c.radius,
        c.offset.y,
        c.strength,
        p1.display()
    );
    println!(
        "xtask: overview ground {ground_alpha} sidebar {side_alpha}  ->  {}{}",
        p2.display(),
        if from_shot.is_some() {
            "  (over the real screendump)"
        } else {
            "  (over the wallpaper — run `xtask shot` for the real desktop)"
        }
    );
    Ok(())
}

/// The staged wallpaper, cropped and scaled to a `sw`x`sh` screen, as XRGB8888 pixels.
///
/// The same crop the image build stages and the same downscale the shell performs, so the ground
/// under a preview is the ground the guest would show.
fn wallpaper_for_screen(sw: u32, sh: u32) -> R<(Vec<u8>, libdraw::framebuffer::Geometry)> {
    use libdraw::format::{PixelFormat, Rgb};
    use libdraw::framebuffer::Geometry;
    let path = repo_root().join(WALLPAPER_ASSET);
    let (iw, ih, rgb) = read_rgb_png(&path)?;
    let top = ((ih.saturating_sub(WALLPAPER_H)) / 2) as usize;
    let src_g = Geometry::packed(iw, WALLPAPER_H.min(ih), PixelFormat::XRGB8888);
    let mut src = vec![0u8; src_g.byte_len()];
    for y in 0..src_g.height {
        for x in 0..iw {
            let i = ((top + y as usize) * iw as usize + x as usize) * 3;
            let c = Rgb::new(rgb[i], rgb[i + 1], rgb[i + 2]);
            let off = src_g.offset_of(x, y).unwrap_or(0);
            src[off..off + 4].copy_from_slice(&src_g.format.encode(c).to_le_bytes());
        }
    }
    // **Placed as the shell places it** — filled, the staged theme's mode — rather than stretched to
    // the screen, which is what this did until Phase 5 Part E and what no desktop ever showed.
    let dst_g = Geometry::packed(sw, sh, PixelFormat::XRGB8888);
    let mut dst = vec![0u8; dst_g.byte_len()];
    let plan = libdraw::theme::WallpaperMode::Fill
        .plan(libdraw::geom::Size::new(src_g.width, src_g.height), libdraw::geom::Size::new(sw, sh));
    let ground = libdraw::theme::Theme::light().desktop;
    if !libdraw::scale::place(&src, src_g, plan, ground, &mut dst, dst_g) {
        return Err("the wallpaper would not place on the preview screen".into());
    }
    Ok((dst, dst_g))
}

/// Fill `rect` by blending `colour` into what is already there — the host's stand-in for a
/// translucent surface the compositor would blend at scanout.
fn blend_rect(
    fb: &mut libdraw::framebuffer::MemFramebuffer,
    rect: libdraw::geom::Rect,
    colour: libdraw::format::Rgb,
    alpha: u8,
) {
    use libdraw::framebuffer::Framebuffer;
    let bounds = Framebuffer::geometry(fb).bounds();
    let Some(area) = rect.intersect(&bounds) else { return };
    for y in area.origin.y..area.bottom() as i32 {
        for x in area.origin.x..area.right() as i32 {
            fb.blend_pixel(x as u32, y as u32, colour, alpha);
        }
    }
}

/// Read an 8-bit RGB PNG back as `(width, height, rgb)`.
fn read_rgb_png(path: &std::path::Path) -> R<(u32, u32, Vec<u8>)> {
    let file = std::io::BufReader::new(fs::File::open(path)?);
    let mut reader = png::Decoder::new(file).read_info()?;
    let info = reader.info().clone();
    if info.color_type != png::ColorType::Rgb || info.bit_depth != png::BitDepth::Eight {
        return Err(format!("{}: expected 8-bit RGB", path.display()).into());
    }
    let size = reader.output_buffer_size().ok_or("image too large")?;
    let mut buf = vec![0u8; size];
    let frame = reader.next_frame(&mut buf)?;
    buf.truncate(frame.buffer_size());
    Ok((info.width, info.height, buf))
}
