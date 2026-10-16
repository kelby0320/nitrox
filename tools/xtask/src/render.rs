use crate::*;

/// The repository file the image build stages at `guest_path`.
///
/// **The one place a guest font path becomes a host file**, used by the staging *and* by every
/// host-side render. That is what makes the gate's central claim true rather than asserted: the
/// picture drawn here is drawn with the same bytes the guest read off ext4, because both come
/// from this function and both paths come from `Theme::light()`. Point the built-in theme at a
/// different face and the image, the previews and the gate all follow it — or, if nothing stages
/// that face, all three fail together and say so.
pub(crate) fn font_asset(guest_path: &str) -> R<PathBuf> {
    let name = guest_path.strip_prefix(FONT_DIR).filter(|n| !n.contains('/')).ok_or_else(|| {
        // **Without saying where the path came from**, because two callers supply it: the
        // built-in theme, for the staging and the reference renders, and `check-terminal`, whose
        // path arrives off the guest's serial line and can therefore be a user's `theme.toml`.
        // Naming the wrong one would send a reader to `Theme::light()` for a value that is in a
        // file on the disk (PR #264 review, finding 1).
        format!(
            "{guest_path:?} is not a file directly under {FONT_DIR} — the image build stages \
             that directory and nothing else, so the host has no way to render with it"
        )
    })?;
    Ok(repo_root().join("assets/fonts").join(name))
}

/// Where the image build binds the fonts, and therefore where a themeable path starts.
const FONT_DIR: &str = "/system/fonts/";

/// One host-side face, loaded from the path a theme names.
pub(crate) fn host_font(guest_path: &str) -> R<libdraw::text::Font> {
    let path = font_asset(guest_path)?;
    libdraw::text::Font::from_bytes(fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?)
        .ok_or_else(|| format!("{} did not parse on the host", path.display()).into())
}

/// Both faces the built-in theme names, in the order [`preview_frames`] wants them — the
/// proportional one with its bold attached, as `libdraw::text::load_ui` attaches it in the guest.
///
/// **The built-in theme rather than the staged file**, and the difference matters: the file
/// carries a deliberately non-default `font_px` for `check-login` to read back, while the guest
/// client whose pictures this gate compares — `ui-testclient` — gets no setup record at all and
/// draws with `Theme::light()`. Rendering the host's reference from the file would compare two
/// different themes and call the difference a display bug.
pub(crate) fn host_faces() -> R<(libdraw::text::Font, libdraw::text::Font)> {
    let t = libdraw::theme::Theme::light();
    let ui = host_font(t.font_ui.as_str())?.with_bold(host_font(libdraw::text::UI_BOLD_FONT_PATH)?);
    Ok((ui, host_font(t.font_mono.as_str())?))
}

/// One reference render by name, for the gate that compares it against a guest.
///
/// **The gate reads this rather than calling the renderer itself**, which is what makes
/// `preview_frames` the single source it claims to be: adding a region to `check-display` and
/// adding a preview are one change, and a preview that stopped being the gate's picture fails
/// against the guest instead of quietly becoming a picture of nothing (PR #261 review, finding 2).
pub(crate) fn reference_frame(
    faces: &(libdraw::text::Font, libdraw::text::Font),
    name: &str,
) -> R<libdraw::framebuffer::MemFramebuffer> {
    preview_frames(faces)
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, f)| f)
        .ok_or_else(|| format!("no reference render called {name:?}").into())
}

/// Every arrangement the host can render, by name.
///
/// **The one place either the preview or the display gate builds a reference picture.** Both used
/// to construct their own; the comment claiming they were the same renders was true and unenforced
/// until it was tested, and a solid rectangle at the right size passed everything.
/// **Each arrangement takes the face its guest counterpart loads** (M11 Part D): the toolkit's
/// window is the desktop's proportional font and the terminal's is the fixed-advance one. They
/// took one font between them until Part D, because the system had only one.
///
/// **`ui-dark` is the one frame no gate compares against** (desktop refresh, Part A), because no
/// guest draws it: the test client paints the built-in theme, which is the light scheme. It is
/// here because the refresh decided how two schemes are covered without doubling the boots —
/// **the host renders both and a boot proves one**, since a scheme is data and a boot proves that
/// data reaches the screen. So the dark scheme's judgement costs a glance, like the light one's.
pub(crate) fn preview_frames(
    (ui, mono): &(libdraw::text::Font, libdraw::text::Font),
) -> Vec<(&'static str, libdraw::framebuffer::MemFramebuffer)> {
    vec![
        ("ui", libui::reference::render(ui)),
        ("ui-dark", libui::reference::render_with(ui, &libdraw::theme::Theme::dark())),
        ("term", libterm::render::reference::render_with(mono)),
    ]
}

/// A framebuffer's visible pixels as RGB triples, row-major.
///
/// **Read through `Framebuffer::get_pixel` rather than off the bytes**, which is not laziness:
/// the toolkit reference's pitch is 1292 for a 1280-byte row, and `XRGB8888` stores
/// little-endian — so the bytes run blue, green, red, pad. Walking the buffer directly is two
/// chances to be wrong (a stride and a channel order) in code whose only job is to be a faithful
/// copy, and `libdraw` already answers both questions correctly for its own compositing.
pub(crate) fn rgb_of(fb: &libdraw::framebuffer::MemFramebuffer) -> (u32, u32, Vec<u8>) {
    use libdraw::framebuffer::Framebuffer;
    let g = fb.geometry();
    let mut out = Vec::with_capacity((g.width as usize) * (g.height as usize) * 3);
    for y in 0..g.height {
        for x in 0..g.width {
            let p = Framebuffer::get_pixel(fb, x, y).unwrap_or_default();
            out.extend_from_slice(&[p.r, p.g, p.b]);
        }
    }
    (g.width, g.height, out)
}

/// Write `rgb` — `w * h` triples — to `path` as an 8-bit RGB PNG.
pub(crate) fn write_png(path: &std::path::Path, w: u32, h: u32, rgb: &[u8]) -> R<()> {
    fs::write(path, encode_png(w, h, rgb)?).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

/// The PNG's bytes, for a caller that wants them without a file — which is what lets the test
/// below decode what was encoded rather than trusting that it round-trips.
pub(crate) fn encode_png(w: u32, h: u32, rgb: &[u8]) -> R<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgb);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().map_err(|e| format!("png header: {e}"))?;
        writer.write_image_data(rgb).map_err(|e| format!("png data: {e}"))?;
    }
    Ok(out)
}

/// The demo user's `theme.toml`, and the theme it reads as.
///
/// **Shipped with every field shown** (M11 Part C). It could ship empty or not at
/// all — a missing file is the built-in theme, which is what the host tests pin — and a file
/// naming every value is what makes the thing *discoverable*: a person who wants to change a
/// colour opens it and sees which colours there are. It is written from `Theme::light()`
/// rather than typed out, so the file and the constants cannot drift.
///
/// **Shown, and only the deliberate ones live** (desktop refresh, Part A). Every value that is
/// the scheme's own is written commented out. Until there were two schemes this file wrote
/// every line live, which was harmless while it restated the only palette there was — and
/// with two it would pin every colour to the light one, so a person who changed `scheme` to
/// "dark" would see nothing happen but the shadow. The plan's words for what a theme file
/// should be: "an override on a palette rather than a list of thirty colours".
pub(crate) fn staged_theme() -> R<(libdraw::theme::Theme, String)> {
    let mut text = String::from("# The session's theme. Delete this file for the built-in one.\n#\n");
    text.push_str("# `scheme` picks the built-in palette the rest of this file adjusts: \"light\" or\n");
    text.push_str("# \"dark\". Everything else is shown as that scheme has it, commented out; remove\n");
    text.push_str("# the `#` from a line to change it. Colours are \"#RRGGBB\"; font_px is a size\n");
    text.push_str("# in pixels per em.\n\n");
    // Written from `Theme::light()` so the file and the constants cannot drift — except for the
    // three fields below, each deliberately not the default and each read back by a gate.
    let mut shipped = libdraw::theme::Theme::light();
    shipped.font_px = f32::from(THEME_FONT_PX);
    // **The theme names the wallpaper** (M12 Part F). It is the *file* that decides the
    // desktop has a picture behind it — the built-in theme names none, deliberately, because
    // a wallpaper is a file a person supplies and a default would make the desktop's ground
    // depend on whatever the build happened to stage.
    shipped.wallpaper = Some(
        libdraw::theme::ThemePath::parse(WALLPAPER_PATH).ok_or("the staged wallpaper path does not fit a ThemePath")?,
    );
    // **Filled, not fitted** (Phase 5 Part E): the picture is 16:10 and the laptop is 16:9, and
    // a fitted picture leaves bars of bare ground down both sides of the screen.
    shipped.wallpaper_mode = libdraw::theme::WallpaperMode::Fill;
    // Line for line against the scheme's own theme: `to_config` writes every field in one
    // order, so a line that matches is a value this file does not change. `scheme` stays live
    // whatever it says — it is the one line a person is expected to edit.
    let base = libdraw::theme::Theme::for_scheme(shipped.scheme).to_config();
    let lines = shipped.to_config();
    if lines.lines().count() != base.lines().count() {
        return Err("the staged theme and its scheme wrote different numbers of lines".into());
    }
    for (line, default) in lines.lines().zip(base.lines()) {
        if line == default && !line.starts_with("scheme ") {
            text.push_str("# ");
        }
        text.push_str(line);
        text.push('\n');
    }
    Ok((shipped, text))
}

/// The staged wallpaper's bytes: [`WALLPAPER_ASSET`], centre-cropped to 16:10.
///
/// **The repository keeps the photograph as supplied and the crop happens here.** It is 4:3 and
/// the screen is 16:10, so fitting it whole leaves 107 pixels of desktop colour down each side —
/// which is what it looked like, and not what a wallpaper should. Cropping in the build rather
/// than committing a pre-cropped file means nothing is lost and the decision is a dozen lines
/// somebody can read rather than something baked into a binary nobody can review.
///
/// **Centre, and only vertically.** The crop takes the full width and drops equal numbers of
/// rows from the top and bottom, which for this picture is empty water; a horizontal crop would
/// have to decide which diver to lose.
///
/// This replaced a generated gradient, which existed so that no unreviewable binary was
/// load-bearing for a gate. That argument is weaker than it sounds now the asset is *content* a
/// person chose rather than a fixture: what the gate needs from it is its size, and that is
/// asserted against [`WALLPAPER_W`] and [`WALLPAPER_H`] here, from the same crop that produced
/// it.
pub(crate) fn wallpaper_png() -> R<Vec<u8>> {
    let path = repo_root().join(WALLPAPER_ASSET);
    // `BufRead`, which the decoder wants: it reads chunk headers a few bytes at a time.
    let file = std::io::BufReader::new(fs::File::open(&path).map_err(|e| format!("open {}: {e}", path.display()))?);
    let mut reader = png::Decoder::new(file).read_info().map_err(|e| format!("read {}: {e}", path.display()))?;
    let info = reader.info().clone();
    if info.color_type != png::ColorType::Rgb || info.bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "{}: the staged wallpaper must be 8-bit RGB, not {:?}/{:?}",
            path.display(),
            info.color_type,
            info.bit_depth
        )
        .into());
    }
    // **An overflow guard, not an interlace check.** `png`'s own doc: "Returns `None` if the
    // output buffer does not fit into the memory space of the machine." An interlaced RGB8 file
    // returns `Some` and de-interlaces into the same layout, so this would pass it straight
    // through — which is fine here, since the guest's decoder is the one that refuses
    // interlacing and this is a host-side crop. The first version of this comment said the
    // opposite (PR #273 review, optional 4).
    let size =
        reader.output_buffer_size().ok_or_else(|| format!("{}: the decoder cannot size this image", path.display()))?;
    let mut src = vec![0u8; size];
    let frame = reader.next_frame(&mut src).map_err(|e| format!("decode: {e}"))?;
    let (sw, sh) = (frame.width, frame.height);
    if sw != WALLPAPER_W || sh < WALLPAPER_H {
        return Err(format!(
            "{}: expected {WALLPAPER_W} wide and at least {WALLPAPER_H} tall, got {sw}x{sh}",
            path.display()
        )
        .into());
    }
    let top = ((sh - WALLPAPER_H) / 2) as usize;
    let row = sw as usize * 3;
    let mut rgb = Vec::with_capacity(row * WALLPAPER_H as usize);
    for y in 0..WALLPAPER_H as usize {
        let o = (top + y) * row;
        rgb.extend_from_slice(&src[o..o + row]);
    }
    encode_png(WALLPAPER_W, WALLPAPER_H, &rgb)
}
