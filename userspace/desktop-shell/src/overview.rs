use super::*;

/// `theme`, as the overview wears it: light ink for a card's caption, on no ground of its own.
///
/// **Ink only.** The overview's ground is a fill at an alpha, and every card draws its own; what
/// this is for is the text beside them, which sits on the darkened desktop and must read against
/// a photograph. The design's caption ink is `#EAF6F6` with the count at 60% of it — taken here
/// as the theme's own paper and a blend towards the ground, so a new palette needs no extra
/// decision.
///
/// **Not `background` and `foreground` swapped**, which is what the sidebar did until Part E: a
/// card is a picture of a desktop rather than a panel, so there is no second surface to relate a
/// pair of colours to.
///
/// **`blend`'s coverage is the *source's* share**, which is the way round this had wrong at
/// first: the design's count is its ink at 60%, and `blend(ground, 102)` is 40% of it — `#666`
/// on the overview's ground, about 2.4:1, where 60% gives `#999` and about 3.4:1. Measured on a
/// screendump, because nothing renders this surface on the host (see `desktop-refresh.md`).
fn overview_ink(theme: &Theme) -> Theme {
    let paper = theme.background;
    Theme { foreground: paper, foreground_dim: paper.blend(OVERVIEW_GROUND, CAPTION_DIM), ..*theme }
}

/// How much of the caption's ink the window count keeps: the design's 60%.
const CAPTION_DIM: u8 = 153;

/// Copy `src` into `fb` at `(x, y)`, clipped to `clip`.
///
/// **One function for the two places that blit** — a card's wallpaper ground and a window's
/// capture. They were the same loop written twice, and the first of them honoured the clip while
/// the second did not, which only showed when a card ran under another.
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

/// Unmap a buffer this process mapped with [`shared_buffer`], and forget the pointer.
///
/// **The shell had no unmap call at all until M8 Part E's review.** `shared_buffer` maps
/// outside the heap, so nothing reclaims it on drop: every overview open leaked two 4 MB
/// mappings and up to six thumbnails, ~9 MB a cycle against a 256 MB guest — about
/// twenty-eight opens to exhaust the machine, and the landing spot for that is a `create` that
/// fails after the window exists (PR #244 review, finding 4). The modal had the same shape at
/// 614 KB, which is why it had not bitten; it was fixed there too rather than left as the next
/// instance of the same bug. (The menus that replaced it are `Child`s, whose pool unmaps on drop.)
fn release_buffer(addr: &mut *mut u8, len: usize) {
    if addr.is_null() {
        return;
    }
    // SAFETY: unmapping exactly what `shared_buffer` mapped, at the same length. The object
    // itself goes when its last reference does — the handle was moved away at `attach`.
    unsafe { syscall4(SYS_MEMORY_UNMAP, *addr as u64, len as u64, 0, 0) };
    *addr = core::ptr::null_mut();
}

/// Paint the overview: frozen thumbnails of the current desktop, and a sidebar of desktops.
///
/// **Frozen, and that is what makes this affordable.** `desktop-shell.md` §6 rejected
/// compositing live windows with a scale transform — which needs scale as a window attribute,
/// geometry save and restore, and windows physically relocating — in favour of asking the
/// compositor for a snapshot. Real windows never move; the compositor gained one operation
/// instead of a transform pipeline. A window drawn *after* the capture shows its state at the
/// moment the overview opened, which is accepted deliberately.
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
    // **A translucent ground, so the overview sits *on* the desktop rather than replacing it**
    // (M13 Part C). This window used to be opaque and full-screen, which meant that to look like
    // an overlay it had to redraw the desktop itself — the wallpaper, dimmed, scaled and composited
    // here. That was the nearest thing to translucency reachable without an alpha channel, and it
    // was reported as the picture disappearing whenever you opened the overview (2026-09-02).
    //
    // Now the compositor has the real desktop underneath — live windows and all — and this fills a
    // dark colour at a partial opacity over it. The wallpaper is no longer read, scaled or copied
    // for this; `libdraw::scale::dim` went with it. What remains is one fill.
    //
    // **Dark rather than clear**, which is what makes it read as *behind* something. GNOME blurs
    // as well; a blur is a separable convolution over a million pixels per open, and dimming alone
    // is what the request asked for.
    fb.fill_rect_alpha(geometry.bounds(), OVERVIEW_GROUND, OVERVIEW_GROUND_ALPHA);

    // **The wallpaper again, scaled once for every miniature that wants it.** Once rather than
    // per row: `box_downscale` averages the whole source per destination pixel, so doing it per
    // desktop would repeat a million reads for an identical answer.
    let mini = wallpaper.and_then(|p| mini_wallpaper(p, screen));

    // **A card per desktop, laid out by the one function the presses are hit-tested with.**
    // Each card is painted at its own rect rather than the whole block being one element tree:
    // `card_rect` is the layout, it is host-tested at five screen sizes, and a tree built here
    // would be a second description of the same arrangement for the drawing half alone.
    let ink = overview_ink(theme);
    let metrics = FontMetrics::new(font, theme.font_px);
    for (i, d) in desktops.iter().enumerate() {
        let (cx, cy, cw, ch) = desktop_shell::card_rect(i, desktops.len(), screen);
        let card = desktop_card(entries, d.id, d.id == current, theme, mini.is_some(), screen);
        let bounds = Rect::new(cx, cy, cw, ch);
        let l = layout(&card, bounds, &metrics);
        // **The shell's first custom node**, and what it draws is the card's ground. Bounded by
        // the clip as well as the node's rect: `paint` gives both, and a callback that honoured
        // only the rect would draw a whole card over a partly-damaged one.
        paint_over(&mut fb, font, &ink, &card, &l, bounds, &mut |kind, rect, clip, fb: &mut MemFramebuffer| {
            if kind != MINI_KIND {
                return;
            }
            let Some((px, g)) = mini.as_ref() else { return };
            blit_clipped(fb, px, *g, rect.origin.x, rect.origin.y, clip);
        });

        // The caption beneath: the desktop's name, and how many windows are on it.
        //
        // **Two runs rather than one string with a separator**, which is what the design draws:
        // the name in the ink and the count dimmed beside it, so the count reads as a note on
        // the name rather than as part of it.
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

    // **The captures, blitted over the current desktop's card**, straight in: they are already
    // pixels at the size of the box they go in, so there is nothing for the toolkit to lay out.
    // A window whose capture failed keeps the schematic box `desktop_card` drew underneath.
    if let Some(i) = desktops.iter().position(|d| d.id == current) {
        let card = desktop_shell::card_rect(i, desktops.len(), screen);
        for (id, w, h, px) in shots {
            let Some(e) = entries.iter().find(|e| e.id == *id) else { continue };
            let (bx, by, bw, bh) = desktop_shell::window_box(card, e.origin, e.size, screen);
            // Inside the box's own border, so a capture cannot paint over the edge that makes
            // two overlapping windows read as two.
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
/// **Undimmed, unlike the overview's own ground.** The ground is dimmed so the things drawn over
/// it read; a miniature *is* the thing being read, and a dimmed one would be a picture of a
/// desktop nobody has.
///
/// **`picture` is the screen-sized composition [`open_wallpaper`] kept, and `screen` must be the
/// screen it was composed for.** Read at any other width, `box_downscale` still accepts it and
/// every row comes out that many bytes out of step — sheared, with nothing logged.
fn mini_wallpaper(picture: &[u8], screen: Screen) -> Option<(alloc::vec::Vec<u8>, Geometry)> {
    let src = Geometry::with_pitch(screen.width, screen.height, screen.pitch(), PixelFormat::XRGB8888)?;
    let (w, h) = (screen.card_interior_w(), screen.card_interior_h());
    let dst = Geometry::with_pitch(w, h, w as usize * 4, PixelFormat::XRGB8888)?;
    let mut out = alloc::vec![0u8; dst.pitch * h as usize];
    if !libdraw::scale::box_downscale(picture, src, &mut out, dst) {
        // **Said rather than silently fallen back from** (PR #273 review, optional 5). A `None`
        // here puts every miniature back to flat blue — which is precisely the bug this change
        // exists to fix, reappearing with nothing printed and nothing failing. Every other
        // failure around the wallpaper names itself; this one did not.
        kprint(b"desktop-shell: the overview could not scale the wallpaper for a miniature\n");
        return None;
    }
    Some((out, dst))
}

/// Capture a thumbnail of every window on `current`, replacing whatever `shots` held.
///
/// **Minimized windows are not in the overview.** `set_minimized` flips a flag without touching
/// the committed buffer, so a minimized window captures perfectly and would be drawn in the grid
/// exactly like one that is on screen — an overview of "what is on this desktop" showing
/// something that deliberately is not (PR #244 review, optional 7). The bar is where a minimized
/// window is restored from, and it marks them.
///
/// **One function because opening and refreshing must agree.** They were the same loop written
/// once when only opening existed; a refresh that captured a different set would show a desktop
/// nobody could reach by opening it.
///
/// **At most `capacity`, the bar's own count** — the windows the bar shows, which is the bound
/// this has always had and what keeps the grid from running past the screen.
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
/// **A `popup`, like the menus, and for two reasons.** A popup is placed by its
/// creator and is *not* held for the manager — which is what a shell creating a window while
/// holding its own manager channel needs, as Part C learned the hard way. And a popup takes
/// keyboard focus, so Escape closes it; a `panel` never could.
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
    // **Every failure past `create` destroys the window**, which the modal's `open_modal` said
    // from PR #237 and this function did not. Returning `None` without it leaves the
    // compositor holding a popup whose id this process has forgotten — never closable, never
    // committable to — while `addrs` keeps a live mapping of an orphaned object that the next
    // present would write through. Repeat past `MAX_WINDOWS_PER_CONNECTION` and the menus stop
    // opening too, because they share the connection (PR #244 review, blocking 3).
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
        // **ARGB, which is what makes the overview an overlay** (M13 Part C). Everything else
        // this shell creates is opaque; this one window is composited per pixel so the live
        // desktop shows through its ground.
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
    // **One line whichever way it went**, the shape `read_theme` uses: it says the overview
    // *decided* about a ground without a gate having to assert which way, so "delete the
    // wallpaper and the overview still opens" is a control that runs against the committed gate
    // rather than one that needs a step edited out.
    if wallpaper.is_some() {
        kprint(b"desktop-shell: overview ground is the wallpaper\n");
    } else {
        kprint(b"desktop-shell: overview ground is the desktop colour\n");
    }
    present_overview(session, id, theme, font, shots, desktops, current, entries, addrs, wallpaper, screen);
    Some(id)
}

/// Render the overview into a free buffer and commit it.
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

/// Destroy the overview and forget what it was showing.
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
        // **Unmapped, not merely forgotten.** Destroying the window drops the compositor's
        // side; this process's two 4 MB mappings would otherwise stay for the session's life.
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
/// **A card is a model of the screen** (desktop refresh, Part E), which is what the design draws:
/// the same rectangle at 22.9% of its width, with the top bar as a strip across it and every
/// window where it actually is. Clicking a window in it raises that window; clicking the card
/// goes to that desktop; dragging a window onto another card moves it there.
///
/// **Rectangles rather than scaled window contents, where there is nothing to scale** (M11 Part E
/// batch 10). A thumbnail is a capture, and the compositor can only capture what it composites —
/// the windows on the desktop being *shown*. So a card for another desktop draws where its
/// windows are and names them, which is what the design draws for every card; the current
/// desktop's card has real captures blitted over these boxes by [`render_overview`], which is
/// more than the design asks for and is why the schematic is drawn underneath rather than
/// instead.
///
/// **The ground beneath them is pixels, since 2026-09-02**: a card's ground is the wallpaper,
/// box-downscaled once for every card. A preview of a desktop that has a picture, drawn as flat
/// blue, is a preview of a desktop nobody has (PR #273 review, optional 3).
///
/// **Bordered boxes rather than filled ones**, so two overlapping windows read as two.
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
    // **The current desktop is outlined in the accent**, the design's own cue and the same one a
    // focused window wears since Part H. The rest take a pale edge against the darkened desktop
    // rather than a theme colour: a card sits on the overview's ground, which is neither a
    // window's paper nor a panel, so a border from either palette reads as belonging elsewhere.
    layers.push(fill(if current { theme.accent } else { CARD_EDGE }));
    // **The card's ground is what a desktop's ground actually is**, which since M12 Part F is a
    // picture rather than a colour. A `custom` node because the picture is pixels: there is
    // nothing here for the toolkit to lay out, and `render_overview`'s paint callback blits the
    // wallpaper it scaled once.
    layers.push(padding(
        Insets::all(CARD_BORDER),
        if wallpaper { custom(MINI_KIND, libdraw::geom::Size::new(iw, ih)) } else { fill(theme.desktop) },
    ));
    // The top bar, at the card's own scale — so a card is the whole screen and not just the part
    // of it windows live in.
    layers.push(offset(
        CARD_BORDER as i32,
        CARD_BORDER as i32,
        sized(libdraw::geom::Size::new(iw, screen.card_strip_h()), fill(theme.panel)),
    ));
    for e in entries.iter().filter(|e| e.desktop == desktop && !e.minimized) {
        // **Positioned by the same function the press is hit-tested with**, which is the lesson
        // the bottom bar's indicator taught: a hit region computed separately from the layout is
        // right at one window count and wrong everywhere else (PR #243 review, blocking 2).
        // The card is passed at the origin, so what comes back is card-local.
        let (bx, by, bw, bh) = desktop_shell::window_box((0, 0, 0, 0), e.origin, e.size, screen);
        let face = if e.focused { theme.face_hover } else { theme.face };
        layers.push(offset(bx, by, sized(libdraw::geom::Size::new(bw, bh), window_mini(e, face, theme, screen))));
    }
    sized(libdraw::geom::Size::new(screen.card_w(), screen.card_h()), stack(layers))
}

/// One window inside a card: a bordered box, its title bar as a strip, and its name.
///
/// **The name is the design's**, and it is what makes a card of a desktop you are not on worth
/// looking at — two bordered rectangles say how many windows there are and nothing about what
/// they are. It is drawn only when the box can hold it: at the card's scale a small window is a
/// few pixels across, and text laid out in a box smaller than itself would spill over the window
/// beside it.
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
///
/// Sized from what a name needs rather than guessed: a box narrower than this holds two or three
/// characters, which is not a name, and one shorter has no room under the title strip.
const MINI_LABEL_W: u32 = 44;

/// See [`MINI_LABEL_W`].
const MINI_LABEL_H: u32 = 22;

/// The edge of a card that is not the current desktop's: white at a third, the design's own.
///
/// A literal rather than a theme colour because it sits on the overview's ground — see
/// [`desktop_card`]. `libdraw` has no alpha in a `Rgb`, so this is that blend against the ground
/// the overview fills, which is [`OVERVIEW_GROUND`] over the desktop.
const CARD_EDGE: libdraw::format::Rgb = libdraw::format::Rgb::new(0x8A, 0x92, 0x95);

/// The `custom` node a card's ground is drawn as. See [`desktop_card`].
const MINI_KIND: u32 = 1;

/// How far the wallpaper is darkened under the overview, as a coverage of black.
///
/// **Enough that light ink and pale thumbnails read over a photograph**, and not so much that
/// the picture stops being recognisable — the point is that the desktop is still *there*. A
/// number rather than a theme key: it is the overview's own composition, like the sidebar's
/// panel, and M11's decision 2 keeps chrome metrics out of what a theme file can set.
/// The overview's ground: this colour, at [`OVERVIEW_GROUND_ALPHA`], over the live desktop.
const OVERVIEW_GROUND: libdraw::format::Rgb = libdraw::format::Rgb::new(0, 0, 0);

/// How opaque the overview's ground is — the desktop shows through the remainder.
///
/// **Dark enough that a thumbnail reads against it**, which is the constraint: the overview's job
/// is to show windows, and a ground you can see the real desktop through too clearly puts a second
/// copy of every window behind its own thumbnail.
const OVERVIEW_GROUND_ALPHA: u8 = 210;

/// Which window's box in a card a point is in, if any — the window's id and the card it is on.
///
/// **Searched topmost-first within a card, which is the order they are drawn in reverse.**
/// `desktop_card` stacks windows in `entries` order, so the last one drawn is on top and is the
/// one a press means; walking forwards would hand a press to whichever overlapping window
/// happened to be first in the list.
pub(super) fn window_in_card_at(x: i32, y: i32, desktops: &[Desktop], entries: &[WinEntry], screen: Screen) -> Option<u32> {
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

/// How big a capture of `window` should be — [`desktop_shell::capture_box`], which says why it
/// is the window's own scale rather than the box it lands in.
///
/// **Captured at the size it is shown at** (desktop refresh, Part E), which is what makes this
/// affordable with no GPU — `desktop-shell.md` §6's "capture at thumbnail size, not full size",
/// now that a thumbnail is a card's window box rather than a fixed 240×150 cell. The compositor
/// scales once, on entry; nothing rescales per frame.
fn capture_size(e: &WinEntry, screen: Screen) -> (u32, u32) {
    desktop_shell::capture_box(e.size, screen)
}

/// Ask the compositor to scale `window` into a fresh buffer, and return the pixels.
///
/// **The manager allocates**, which is the mirror of a client attaching a buffer the compositor
/// reads. Clamped to the window's own size, because a capture may not scale *up*: a window
/// smaller than the box it will be drawn in is captured at its own size and drawn smaller.
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
    // **A reply is not an effect, and here the difference is invisible from outside.** The
    // compositor logs a successful capture whether or not the scale wrote anything, and the
    // overview would show a black rectangle that no serial gate could tell from a dark window.
    // `shared_buffer` hands back zeroed memory, so "some pixel is non-zero" is exactly the
    // question, and only the process holding the buffer can ask it (PR #216's rule, and the
    // control that caught this one writing nothing).
    // The copy is taken, so the mapping has done its job. Six of these per overview open at up
    // to 144 KB each, and nothing else would ever reclaim them.
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
