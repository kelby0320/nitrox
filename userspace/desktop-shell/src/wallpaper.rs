//! The wallpaper and the theme, read from disk.

use super::*;

/// The wallpaper, once it is on screen. The composed picture is kept (four megabytes) so the
/// overview, a full-screen window, can draw it as its ground instead of re-decoding on every
/// open.
pub(super) struct Wallpaper {
    /// The window it lives in: named among the shell's own, and made sticky.
    pub(super) window: u32,
    /// The screen-sized XRGB8888 composition, pitch [`Screen::pitch`].
    pub(super) picture: alloc::vec::Vec<u8>,
}

/// Put the theme's wallpaper on screen, if it names one.
///
/// `Role::Panel` with a zero reservation is the whole of "bottom-most and out of the way": it
/// cannot take focus, so a press raises nothing yet still dismisses an open popup, and a
/// maximised window is still the work area. Two buffers, the second dead: `libsurface` refuses
/// fewer, as a single-buffered client can never draw a second frame. Every failure returns `None`
/// after saying which; the desktop falls back to its ground colour.
pub(super) fn open_wallpaper(
    ns: u64,
    session: &mut Session<ChannelTransport>,
    theme: &Theme,
    screen: Screen,
) -> Option<Wallpaper> {
    let path = theme.wallpaper.as_ref()?;
    let bytes = match libfs::read_file(ns, path.as_str().as_bytes()) {
        Ok(b) => b,
        Err(e) => {
            let why: &[u8] = match e {
                libfs::FileError::NotFound => b" is not there",
                libfs::FileError::TooLarge => b" is too large to read",
                _ => b" could not be read",
            };
            Line::new().s(b"desktop-shell: wallpaper ").s(path.as_str().as_bytes()).s(why).end();
            return None;
        }
    };
    let image = match libdraw::png::decode(&bytes) {
        Ok(i) => i,
        Err(e) => {
            Line::new().s(b"desktop-shell: wallpaper ").s(path.as_str().as_bytes()).s(b" ").s(e.why().as_bytes()).end();
            return None;
        }
    };
    let size = Size::new(screen.width, screen.height);
    let plan = theme.wallpaper_mode.plan(Size::new(image.width(), image.height()), size);
    let pitch = screen.pitch();
    let len = pitch * screen.height as usize;
    let Some(geometry) = Geometry::with_pitch(screen.width, screen.height, pitch, PixelFormat::XRGB8888) else {
        kprint(b"desktop-shell: the wallpaper geometry is unusable\n");
        return None;
    };
    let mut picture = alloc::vec![0u8; len];
    if !libdraw::scale::place(&image.pixels, image.geometry, plan, theme.desktop, &mut picture, geometry) {
        kprint(b"desktop-shell: the wallpaper could not be placed\n");
        return None;
    }

    let role = Role::Panel { dock: Edge::Top, reserve: 0 };
    let id = match session.create(&CreateWindowRequest::new(screen.width, screen.height, role), BUFFERS) {
        Ok(id) => id,
        Err(_) => {
            kprint(b"desktop-shell: wallpaper CreateWindow FAILED\n");
            return None;
        }
    };
    // Everything past `create` unwinds through one exit, because the window now exists: returning
    // `None` would leave a full-screen, hit-testable `panel` with nothing committed, for the life
    // of the process.
    let mut ok = true;
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            kprint(b"desktop-shell: wallpaper buffer alloc FAILED\n");
            ok = false;
            break;
        };
        // SAFETY: `addr` maps `len` writable bytes and `picture` holds exactly `len`; the two
        // are distinct allocations, so they cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(picture.as_ptr(), addr, len) };
        let Some(mut w) = session.window(id) else {
            kprint(b"desktop-shell: wallpaper window vanished\n");
            ok = false;
            break;
        };
        if w.attach(i as u32, screen.width, screen.height, pitch as u32, handle).is_err() {
            kprint(b"desktop-shell: wallpaper AttachBuffer FAILED\n");
            ok = false;
            break;
        }
    }
    if ok {
        match session.window(id) {
            Some(mut w) => {
                if w.commit(0, (0, 0, screen.width, screen.height)).is_err() {
                    kprint(b"desktop-shell: wallpaper Commit FAILED\n");
                    ok = false;
                }
            }
            None => {
                kprint(b"desktop-shell: wallpaper window vanished\n");
                ok = false;
            }
        }
    }
    if !ok {
        if let Some(w) = session.window(id) {
            let _ = w.destroy();
        }
        return None;
    }
    // After the commit, not before: a line must be printed where the thing it claims has
    // happened. Both sizes: the decoded proves the `IHDR` was read, the drawn that the placement
    // ran.
    Line::new()
        .s(b"desktop-shell: wallpaper ")
        .u(image.width() as u64)
        .s(b"x")
        .u(image.height() as u64)
        .s(b" drawn ")
        .u(plan.size.w as u64)
        .s(b"x")
        .u(plan.size.h as u64)
        .s(b" at ")
        .i(plan.origin.x as i64)
        .s(b",")
        .i(plan.origin.y as i64)
        .s(b" window ")
        .u(id as u64)
        .end();
    Some(Wallpaper { window: id, picture })
}

/// Read the session's theme from `THEME_PATH`, falling back to the built-in one.
///
/// In the user's home, not `/etc`: a session namespace has none, and adding a member is a design
/// decision (`session-mgr/CLAUDE.md`). Every failure lands on the default: a desktop that will
/// not start because its colours did not parse is worse than any colour.
pub(super) fn read_theme(ns: u64) -> Theme {
    // One line whatever happens, beginning the same way, so a gate can assert the shell decided
    // about a theme without asserting which way.
    let bytes = match libfs::read_file(ns, THEME_PATH.as_bytes()) {
        Ok(b) => b,
        Err(e) => {
            // Absent and unreadable are different: "no such file" about one that is there costs
            // an afternoon.
            let why: &[u8] = match e {
                libfs::FileError::NotFound => b" absent; using the built-in theme",
                libfs::FileError::TooLarge => b" too large to read; using the built-in theme",
                _ => b" could not be read; using the built-in theme",
            };
            let t = Theme::default();
            Line::new()
                .s(b"desktop-shell: theme ")
                .s(THEME_PATH.as_bytes())
                .s(why)
                .s(b", font_px ")
                .u(t.font_px as u64)
                .end();
            return t;
        }
    };
    let Ok(text) = core::str::from_utf8(&bytes) else {
        let t = Theme::default();
        Line::new()
            .s(b"desktop-shell: theme ")
            .s(THEME_PATH.as_bytes())
            .s(b" is not UTF-8; using the built-in theme, font_px ")
            .u(t.font_px as u64)
            .end();
        return t;
    };
    let (theme, issues) = Theme::from_config(text);
    Line::new()
        .s(b"desktop-shell: theme ")
        .s(THEME_PATH.as_bytes())
        .s(b" read")
        .s(b" (")
        .u(bytes.len() as u64)
        .s(b" bytes, ")
        .u(issues.len() as u64)
        .s(b" ignored), font_px ")
        .u(theme.font_px as u64)
        .end();
    // Each one named, up to a bound: a thousand bad lines must not become a thousand console lines.
    for issue in issues.iter().take(MAX_LOGGED_THEME_ISSUES) {
        Line::new()
            .s(b"desktop-shell: theme line ")
            .u(issue.line as u64)
            .s(match issue.kind {
                libdraw::theme::IssueKind::Malformed => b" is not `key = value`" as &[u8],
                libdraw::theme::IssueKind::UnknownKey => b" names a key this version does not know",
                libdraw::theme::IssueKind::BadValue => b" has a value this version cannot read",
            })
            .end();
    }
    theme
}

const THEME_PATH: &str = "/home/theme.toml";

/// How many bad theme lines are named before the rest are counted only.
const MAX_LOGGED_THEME_ISSUES: usize = 8;
