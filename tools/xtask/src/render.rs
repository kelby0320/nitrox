//! The host renders a gate compares the guest's screen with, and the theme and wallpaper staged.

use crate::*;

/// The repository file the image build stages at `guest_path`: **the one place a guest font path
/// becomes a host file**, used by the staging and every render, so the gate's picture uses the
/// bytes the guest read off ext4.
pub(crate) fn font_asset(guest_path: &str) -> R<PathBuf> {
    let name = guest_path.strip_prefix(FONT_DIR).filter(|n| !n.contains('/')).ok_or_else(|| {
        // Without saying where the path came from: it is either the built-in theme or, in
        // `check-terminal`, a user's `theme.toml` off the guest's serial line.
        format!(
            "{guest_path:?} is not a file directly under {FONT_DIR} — the image build stages \
             that directory and nothing else, so the host has no way to render with it"
        )
    })?;
    Ok(repo_root().join("assets/fonts").join(name))
}

const FONT_DIR: &str = "/system/fonts/";

pub(crate) fn host_font(guest_path: &str) -> R<libdraw::text::Font> {
    let path = font_asset(guest_path)?;
    libdraw::text::Font::from_bytes(fs::read(&path).map_err(|e| format!("read {}: {e}", path.display()))?)
        .ok_or_else(|| format!("{} did not parse on the host", path.display()).into())
}

/// Both faces the built-in theme names, for [`preview_frames`]. **The built-in theme rather than
/// the staged file**: the file carries a non-default `font_px`, while `ui-testclient`, whose
/// pictures the gate compares, draws with `Theme::light()`.
pub(crate) fn host_faces() -> R<(libdraw::text::Font, libdraw::text::Font)> {
    let t = libdraw::theme::Theme::light();
    let ui = host_font(t.font_ui.as_str())?.with_bold(host_font(libdraw::text::UI_BOLD_FONT_PATH)?);
    Ok((ui, host_font(t.font_mono.as_str())?))
}

/// One reference render by name, for the gate: [`preview_frames`] is the single source.
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

/// Every arrangement the host can render, by name: **the one place a preview or the display gate
/// builds a reference picture**. **`ui-dark` is the one frame no gate compares against**, as no
/// guest draws it: the host renders both schemes and a boot proves one.
pub(crate) fn preview_frames(
    (ui, mono): &(libdraw::text::Font, libdraw::text::Font),
) -> Vec<(&'static str, libdraw::framebuffer::MemFramebuffer)> {
    vec![
        ("ui", libui::reference::render(ui)),
        ("ui-dark", libui::reference::render_with(ui, &libdraw::theme::Theme::dark())),
        ("term", libterm::render::reference::render_with(mono)),
    ]
}

/// A framebuffer's visible pixels as RGB triples. **Read through `get_pixel` rather than off the
/// bytes**: the pitch is not the row length, and `XRGB8888` stores blue, green, red, pad.
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

pub(crate) fn write_png(path: &std::path::Path, w: u32, h: u32, rgb: &[u8]) -> R<()> {
    fs::write(path, encode_png(w, h, rgb)?).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(())
}

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

/// The demo user's `theme.toml`, and the theme it reads as. **Every field shown**, written from
/// `Theme::light()` so the file and the constants cannot drift, but **only the deliberate values
/// live**: pinning every colour would make changing `scheme` do nothing.
pub(crate) fn staged_theme() -> R<(libdraw::theme::Theme, String)> {
    let mut text = String::from("# The session's theme. Delete this file for the built-in one.\n#\n");
    text.push_str("# `scheme` picks the built-in palette the rest of this file adjusts: \"light\" or\n");
    text.push_str("# \"dark\". Everything else is shown as that scheme has it, commented out; remove\n");
    text.push_str("# the `#` from a line to change it. Colours are \"#RRGGBB\"; font_px is a size\n");
    text.push_str("# in pixels per em.\n\n");
    // From `Theme::light()`, but for three fields, each deliberately not the default and read back
    // by a gate. The built-in theme names no wallpaper: it is a file a person supplies.
    let mut shipped = libdraw::theme::Theme::light();
    shipped.font_px = f32::from(THEME_FONT_PX);
    shipped.wallpaper = Some(
        libdraw::theme::ThemePath::parse(WALLPAPER_PATH).ok_or("the staged wallpaper path does not fit a ThemePath")?,
    );
    // **Filled, not fitted**: the picture is 16:10 and the laptop 16:9.
    shipped.wallpaper_mode = libdraw::theme::WallpaperMode::Fill;
    // Line for line against the scheme's own theme: `to_config` writes every field in one order, so
    // a line that matches is a value this file does not change. `scheme` stays live.
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

/// The staged wallpaper's bytes: [`WALLPAPER_ASSET`], centre-cropped to 16:10, as the repository
/// keeps the 4:3 photograph as supplied. The crop drops equal rows from the top and bottom only;
/// the size is asserted against [`WALLPAPER_W`] and [`WALLPAPER_H`].
pub(crate) fn wallpaper_png() -> R<Vec<u8>> {
    let path = repo_root().join(WALLPAPER_ASSET);
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
    // An overflow guard, not an interlace check: the guest's decoder refuses interlacing.
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
