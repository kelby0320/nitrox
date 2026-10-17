//! The overview: cards, miniatures and window captures.

use super::*;

/// `theme`, as the overview wears it: light ink for a card's caption, on no ground of its own, so
/// it reads against the darkened desktop. `blend`'s coverage is the source's share: the count is
/// the ink at 60%, and `blend(ground, 102)` would be 40% of it.
fn overview_ink(theme: &Theme) -> Theme {
    let paper = theme.background;
    Theme { foreground: paper, foreground_dim: paper.blend(OVERVIEW_GROUND, CAPTION_DIM), ..*theme }
}

/// How much of the caption's ink the window count keeps: the design's 60%.
const CAPTION_DIM: u8 = 153;

fn blit_clipped(fb: &mut MemFramebuffer, src: &[u8], g: Geometry, x: i32, y: i32, clip: Rect) {
    use libdraw::framebuffer::Framebuffer as _;
    for sy in 0..g.height {
        for sx in 0..g.width {
            let (dx, dy) = (x + sx as i32, y + sy as i32);
            if dx < clip.origin.x || dy < clip.origin.y || dx >= clip.right() as i32 || dy >= clip.bottom() as i32 {
                continue;
            }
            let off = sy as usize * g.pitch + sx as usize * 4;
            let Some(b) = src.get(off..off + 4) else { continue };
            let word = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
            fb.put_pixel(dx as u32, dy as u32, PixelFormat::XRGB8888.decode(word));
        }
    }
}

/// Unmap a buffer this process mapped with [`shared_buffer`], and forget the pointer. Nothing
/// reclaims it on drop: an overview open once leaked about 9 MB.
fn release_buffer(addr: &mut *mut u8, len: usize) {
    if addr.is_null() {
        return;
    }
    // SAFETY: unmapping exactly what `shared_buffer` mapped, at the same length; the handle moved
    // away at `attach`, so the object goes with its last reference.
    unsafe { syscall4(SYS_MEMORY_UNMAP, *addr as u64, len as u64, 0, 0) };
    *addr = core::ptr::null_mut();
}

/// Paint the overview: frozen thumbnails of the current desktop, and a sidebar of desktops.
///
/// Frozen by design: `desktop-shell.md` §6 chose a compositor snapshot over compositing live
/// windows through a scale transform, so a window drawn after the capture shows its state at
/// open.
#[allow(clippy::too_many_arguments, reason = "the scene's parts are separate borrows the caller already holds")]
fn render_overview(
    theme: &Theme,
    font: &Font,
    shots: &[(u32, u32, u32, alloc::vec::Vec<u8>)],
    desktops: &[Desktop],
    current: u32,
    entries: &[WinEntry],
    wallpaper: Option<&[u8]>,
    screen: Screen,
) -> MemFramebuffer {
    let geometry = Geometry::with_pitch(screen.width, screen.height, screen.pitch(), PixelFormat::ARGB8888)
        .unwrap_or_else(|| fail(b"desktop-shell: bad overview geometry\n"));
    use libdraw::framebuffer::Framebuffer as _;
    let mut fb = MemFramebuffer::new(geometry);
    // A translucent ground, so the overview sits on the desktop: a dark fill at a partial opacity
    // over the real desktop, dark rather than clear so it reads as behind something, and no blur
    // (a convolution over a million pixels per open).
    fb.fill_rect_alpha(geometry.bounds(), OVERVIEW_GROUND, OVERVIEW_GROUND_ALPHA);

    let mini = wallpaper.and_then(|p| mini_wallpaper(p, screen));

    // A card per desktop, painted at the rect `card_rect` gives, the function the presses are
    // hit-tested with.
    let ink = overview_ink(theme);
    let metrics = FontMetrics::new(font, theme.font_px);
    for (i, d) in desktops.iter().enumerate() {
        let (cx, cy, cw, ch) = desktop_shell::card_rect(i, desktops.len(), screen);
        let card = desktop_card(entries, d.id, d.id == current, theme, mini.is_some(), screen);
        let bounds = Rect::new(cx, cy, cw, ch);
        let l = layout(&card, bounds, &metrics);
        // A custom node draws the card's ground, bounded by the clip as well as its rect.
        paint_over(&mut fb, font, &ink, &card, &l, bounds, &mut |kind, rect, clip, fb: &mut MemFramebuffer| {
            if kind != MINI_KIND {
                return;
            }
            let Some((px, g)) = mini.as_ref() else { return };
            blit_clipped(fb, px, *g, rect.origin.x, rect.origin.y, clip);
        });

        let count = window_count(entries, d.id);
        let caption: Element<()> = row(alloc::vec![
            text(desktop_label(desktops, d.id)),
            sized(libdraw::geom::Size::new(CARD_CAPTION_GAP, 0), text("")),
            libui::element::ink(ink.foreground_dim, text(count)),
        ]);
        let cap_rect = Rect::new(cx, cy + ch as i32 + CARD_CAPTION_GAP as i32, cw, CARD_CAPTION_H);
        let cl = layout(&caption, cap_rect, &metrics);
        paint_over(&mut fb, font, &ink, &caption, &cl, cap_rect, &mut |_, _, _, _: &mut MemFramebuffer| {});
    }

    // The captures, blitted over the current desktop's card: already pixels at the size of their
    // box. A window whose capture failed keeps the schematic box `desktop_card` drew.
    if let Some(i) = desktops.iter().position(|d| d.id == current) {
        let card = desktop_shell::card_rect(i, desktops.len(), screen);
        for (id, w, h, px) in shots {
            let Some(e) = entries.iter().find(|e| e.id == *id) else { continue };
            let (bx, by, bw, bh) = desktop_shell::window_box(card, e.origin, e.size, screen);
            let inner = Rect::new(bx + 1, by + 1, bw.saturating_sub(2), bh.saturating_sub(2));
            let g = match Geometry::with_pitch(*w, *h, *w as usize * 4, PixelFormat::XRGB8888) {
                Some(g) => g,
                None => continue,
            };
            blit_clipped(&mut fb, px, g, inner.origin.x, inner.origin.y, inner);
        }
    }
    fb
}

/// The wallpaper scaled to one card's interior, or `None` if it will not scale.
///
/// Undimmed: a miniature is the thing being read. `picture` is the screen-sized composition
/// [`open_wallpaper`] kept; `screen` must be the screen it was composed for, or `box_downscale`
/// shears every row, logging nothing.
fn mini_wallpaper(picture: &[u8], screen: Screen) -> Option<(alloc::vec::Vec<u8>, Geometry)> {
    let src = Geometry::with_pitch(screen.width, screen.height, screen.pitch(), PixelFormat::XRGB8888)?;
    let (w, h) = (screen.card_interior_w(), screen.card_interior_h());
    let dst = Geometry::with_pitch(w, h, w as usize * 4, PixelFormat::XRGB8888)?;
    let mut out = alloc::vec![0u8; dst.pitch * h as usize];
    if !libdraw::scale::box_downscale(picture, src, &mut out, dst) {
        // Said, not silently fallen back from: `None` puts every miniature back to flat blue.
        kprint(b"desktop-shell: the overview could not scale the wallpaper for a miniature\n");
        return None;
    }
    Some((out, dst))
}

/// Capture a thumbnail of every window on `current`, replacing whatever `shots` held.
///
/// Minimized windows are not in the overview (the bar restores them). At most `capacity`, the
/// bar's own count, which keeps the grid on the screen. One function, as opening and refreshing
/// must agree.
pub(super) fn recapture(
    mgr: &mut ChannelTransport,
    entries: &[WinEntry],
    current: u32,
    shots: &mut alloc::vec::Vec<(u32, u32, u32, alloc::vec::Vec<u8>)>,
    capacity: usize,
    screen: Screen,
) {
    shots.clear();
    for e in visible_entries(entries, current, capacity) {
        if e.minimized {
            continue;
        }
        if let Some((w, h, px)) = capture_window(mgr, e.id, e.size, capture_size(e, screen)) {
            shots.push((e.id, w, h, px));
        }
    }
}

/// Create the overview window and present it. `None` if any step fails.
///
/// A `popup`, like the menus: placed by its creator, not held for the manager, and it takes
/// keyboard focus so Escape closes it.
#[allow(clippy::too_many_arguments, reason = "the overview's captures, desktops and windows, and how to draw them")]
pub(super) fn open_overview(
    session: &mut Session<ChannelTransport>,
    parent: u32,
    theme: &Theme,
    font: &Font,
    shots: &[(u32, u32, u32, alloc::vec::Vec<u8>)],
    desktops: &[Desktop],
    current: u32,
    entries: &[WinEntry],
    addrs: &mut [*mut u8; BUFFERS],
    wallpaper: Option<&[u8]>,
    screen: Screen,
) -> Option<u32> {
    let len = screen.pitch() * screen.height as usize;
    let id =
        session.create(&CreateWindowRequest::new(screen.width, screen.height, Role::Popup { parent }), BUFFERS).ok()?;
    // Every failure past `create` destroys the window: otherwise the compositor holds a popup
    // this process has forgotten, with a live mapping, and repeats exhaust
    // `MAX_WINDOWS_PER_CONNECTION`.
    let mut ok = true;
    #[allow(clippy::needless_range_loop, reason = "the index also feeds arithmetic or a second array")]
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            ok = false;
            break;
        };
        addrs[i] = addr;
        let Some(mut w) = session.window(id) else {
            ok = false;
            break;
        };
        if w.attach_with_format(
            i as u32,
            screen.width,
            screen.height,
            screen.pitch() as u32,
            handle,
            PixelFormat::ARGB8888,
        )
        .is_err()
        {
            ok = false;
            break;
        }
    }
    if !ok {
        for a in addrs.iter_mut() {
            release_buffer(a, len);
        }
        if let Some(w) = session.window(id) {
            let _ = w.destroy();
        }
        kprint(b"desktop-shell: overview buffers FAILED\n");
        return None;
    }
    // One line whichever way it went (`check-login` matches it), so a gate can assert the
    // overview decided about a ground without asserting which way.
    if wallpaper.is_some() {
        kprint(b"desktop-shell: overview ground is the wallpaper\n");
    } else {
        kprint(b"desktop-shell: overview ground is the desktop colour\n");
    }
    present_overview(session, id, theme, font, shots, desktops, current, entries, addrs, wallpaper, screen);
    Some(id)
}

#[allow(clippy::too_many_arguments, reason = "the overview's captures, desktops and windows, and how to draw them")]
pub(super) fn present_overview(
    session: &mut Session<ChannelTransport>,
    id: u32,
    theme: &Theme,
    font: &Font,
    shots: &[(u32, u32, u32, alloc::vec::Vec<u8>)],
    desktops: &[Desktop],
    current: u32,
    entries: &[WinEntry],
    addrs: &[*mut u8; BUFFERS],
    wallpaper: Option<&[u8]>,
    screen: Screen,
) {
    let len = screen.pitch() * screen.height as usize;
    let bytes = render_overview(theme, font, shots, desktops, current, entries, wallpaper, screen).into_bytes();
    if bytes.len() != len {
        return;
    }
    let Some(mut w) = session.window(id) else { return };
    let Ok(slot) = w.acquire() else { return };
    let addr = addrs[slot as usize % BUFFERS];
    if addr.is_null() {
        return;
    }
    // SAFETY: `addr` maps `len` writable bytes and `bytes` holds exactly `len`; distinct
    // allocations, so they cannot overlap.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr, len) };
    let _ = w.commit(slot, (0, 0, screen.width, screen.height));
}

pub(super) fn close_overview(
    session: &mut Session<ChannelTransport>,
    overview: &mut Option<u32>,
    shots: &mut alloc::vec::Vec<(u32, u32, u32, alloc::vec::Vec<u8>)>,
    dragging: &mut Option<u32>,
    addrs: &mut [*mut u8; BUFFERS],
    screen: Screen,
) {
    if let Some(id) = overview.take() {
        if let Some(w) = session.window(id) {
            let _ = w.destroy();
        }
        // Unmapped, not merely forgotten: destroying the window drops only the compositor's side.
        for a in addrs.iter_mut() {
            release_buffer(a, screen.pitch() * screen.height as usize);
        }
        shots.clear();
        *dragging = None;
        Line::new().s(b"desktop-shell: overview closed, window ").u(id as u64).end();
    }
}

/// One desktop's card: its ground, the top bar's strip, and a box where each of its windows is.
///
/// A card is a model of the screen at 22.9% of its width. Clicking a window raises it, clicking
/// the card goes to that desktop, dragging a window onto another card moves it there. Other
/// desktops' cards name their windows in boxes, since the compositor can only capture the desktop
/// being shown; the current card has real captures blitted over them by [`render_overview`].
/// Bordered boxes, so two overlapping windows read as two.
fn desktop_card(
    entries: &[WinEntry],
    desktop: u32,
    current: bool,
    theme: &Theme,
    wallpaper: bool,
    screen: Screen,
) -> Element<()> {
    let (iw, ih) = (screen.card_interior_w(), screen.card_interior_h());
    let mut layers = alloc::vec::Vec::with_capacity(6);
    // The current desktop is outlined in the accent; the rest take a pale edge, since a card sits
    // on the overview's ground, neither a window's paper nor a panel.
    layers.push(fill(if current { theme.accent } else { CARD_EDGE }));
    layers.push(padding(
        Insets::all(CARD_BORDER),
        if wallpaper { custom(MINI_KIND, libdraw::geom::Size::new(iw, ih)) } else { fill(theme.desktop) },
    ));
    layers.push(offset(
        CARD_BORDER as i32,
        CARD_BORDER as i32,
        sized(libdraw::geom::Size::new(iw, screen.card_strip_h()), fill(theme.panel)),
    ));
    for e in entries.iter().filter(|e| e.desktop == desktop && !e.minimized) {
        // Positioned by the same function the press is hit-tested with; card-local, as the card
        // is passed at the origin.
        let (bx, by, bw, bh) = desktop_shell::window_box((0, 0, 0, 0), e.origin, e.size, screen);
        let face = if e.focused { theme.face_hover } else { theme.face };
        layers.push(offset(bx, by, sized(libdraw::geom::Size::new(bw, bh), window_mini(e, face, theme, screen))));
    }
    sized(libdraw::geom::Size::new(screen.card_w(), screen.card_h()), stack(layers))
}

/// One window inside a card: a bordered box, its title bar as a strip, and its name, drawn only
/// when the box can hold it.
fn window_mini(e: &WinEntry, face: libdraw::format::Rgb, theme: &Theme, screen: Screen) -> Element<()> {
    let strip = screen.scaled_to_card(libui::widget::TITLE_BAR_H).max(1);
    let (bw, bh) = (screen.scaled_to_card(e.size.0).max(2), screen.scaled_to_card(e.size.1).max(2));
    let mut layers = alloc::vec::Vec::with_capacity(3);
    layers.push(fill(theme.border));
    layers.push(padding(Insets::all(1), fill(face)));
    if bw >= MINI_LABEL_W && bh >= MINI_LABEL_H && !e.title.is_empty() {
        layers.push(offset(
            1,
            (1 + strip) as i32,
            padding(
                Insets { top: 1, right: 2, bottom: 0, left: 2 },
                libui::element::scaled(libui::element::TextSize::Small, text(e.title.as_str())),
            ),
        ));
    }
    stack(layers)
}

/// How wide and tall a window's box must be before its name is drawn in it.
const MINI_LABEL_W: u32 = 44;

const MINI_LABEL_H: u32 = 22;

/// The edge of a card that is not the current desktop's: white at a third, the design's own, a
/// literal because `Rgb` has no alpha.
const CARD_EDGE: libdraw::format::Rgb = libdraw::format::Rgb::new(0x8A, 0x92, 0x95);

const MINI_KIND: u32 = 1;

/// The overview's ground: black at [`OVERVIEW_GROUND_ALPHA`] over the live desktop. Not a theme
/// key: chrome metrics stay out of what a theme file can set.
const OVERVIEW_GROUND: libdraw::format::Rgb = libdraw::format::Rgb::new(0, 0, 0);

/// How opaque the overview's ground is: dark enough that a thumbnail reads against it.
const OVERVIEW_GROUND_ALPHA: u8 = 210;

/// Which window's box in a card a point is in, if any: its id. Topmost-first, the reverse of the
/// drawing order, so a press goes to the window on top.
pub(super) fn window_in_card_at(
    x: i32,
    y: i32,
    desktops: &[Desktop],
    entries: &[WinEntry],
    screen: Screen,
) -> Option<u32> {
    let i = desktop_shell::card_at(x, y, desktops.len(), screen)?;
    let card = desktop_shell::card_rect(i, desktops.len(), screen);
    let d = desktops[i].id;
    entries
        .iter()
        .filter(|e| e.desktop == d && !e.minimized)
        .rev()
        .find(|e| {
            let (bx, by, bw, bh) = desktop_shell::window_box(card, e.origin, e.size, screen);
            x >= bx && x < bx + bw as i32 && y >= by && y < by + bh as i32
        })
        .map(|e| e.id)
}

/// How big a capture of `window` should be: [`desktop_shell::capture_box`].
fn capture_size(e: &WinEntry, screen: Screen) -> (u32, u32) {
    desktop_shell::capture_box(e.size, screen)
}

/// Ask the compositor to scale `window` into a fresh buffer, and return the pixels. The manager
/// allocates; clamped to the window's own size, as a capture may not scale up.
fn capture_window(
    mgr: &mut ChannelTransport,
    window: u32,
    size: (u32, u32),
    want: (u32, u32),
) -> Option<(u32, u32, alloc::vec::Vec<u8>)> {
    use librsproto::surface::{MgrCapture, OP_MGR_CAPTURE};
    let w = want.0.min(size.0.max(1));
    let h = want.1.min(size.1.max(1));
    let pitch = (w as usize) * 4;
    let len = pitch * h as usize;
    let (handle, addr) = shared_buffer(len)?;
    let req = MgrCapture { window, width: w, height: h, pitch: pitch as u32 };
    let mut body = [0u8; core::mem::size_of::<MgrCapture>()];
    req.write(&mut body)?;
    let mut reply = [0u8; 64];
    let ok = mgr.request(OP_MGR_CAPTURE, &body, Some(handle), &mut reply).is_ok();
    if !ok {
        Line::new().s(b"desktop-shell: Capture refused for window ").u(window as u64).end();
        // TODO(capture-refused-leak): this leaves the mapping, and a failed send the handle too.
        return None;
    }
    // SAFETY: `addr` maps `len` readable bytes the compositor has just written.
    let px = unsafe { core::slice::from_raw_parts(addr, len) }.to_vec();
    // A reply is not an effect: the compositor logs a capture whether or not the scale wrote
    // anything, so a black rectangle looks like a dark window to every gate. `shared_buffer` is
    // zeroed, so "some pixel is non-zero" is the question. The copy is taken, so the mapping is
    // released.
    let mut addr = addr;
    release_buffer(&mut addr, len);
    let lit = px.as_chunks::<4>().0.iter().filter(|c| **c != [0, 0, 0, 0]).count();
    if lit == 0 {
        Line::new().s(b"desktop-shell: capture of window ").u(window as u64).s(b" came back blank").end();
        return None;
    }
    Line::new()
        .s(b"desktop-shell: thumbnail of window ")
        .u(window as u64)
        .s(b" has ")
        .u(lit as u64)
        .s(b" painted pixels")
        .end();
    Some((w, h, px))
}
