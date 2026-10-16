//! The wallpaper and the theme, read from disk.

use super::*;

/// The wallpaper, once it is on screen.
///
/// **The composed picture is kept**, which costs four megabytes for the machine's life and buys
/// the overview its ground: the overview is a full-screen opaque window, so without this it
/// covers the wallpaper with a flat colour and the desktop appears to lose its picture whenever
/// you look at the desktops (reported from a real session, 2026-09-02). The alternatives are
/// re-reading and re-decoding the file on every open — tens of milliseconds on a gesture that
/// should feel instant — or keeping the *decoded* image instead, which is twice the size.
pub(super) struct Wallpaper {
    /// The window it lives in: named among the shell's own, and made sticky.
    pub(super) window: u32,
    /// The screen-sized XRGB8888 composition, pitch [`Screen::pitch`]. Ground and picture,
    /// exactly as committed.
    pub(super) picture: alloc::vec::Vec<u8>,
}

/// Put the theme's wallpaper on screen, if it names one.
///
/// **`Role::Panel` with a zero reservation, and that is the whole of "bottom-most and out of the
/// way".** It is the one role that cannot take focus, so a press on the picture raises nothing
/// and changes no focus — which is exactly what a press on bare desktop did before there was a
/// picture there. A press on it still *dismisses* an open popup, because the compositor's rule
/// is "anywhere that is not the popup" rather than "on a window" (M11 Part E batch 5), so the
/// applications menu goes on closing when you click the desktop. And `reserve: 0` means a
/// maximised window is still the work area: the wallpaper occupies the screen without claiming
/// any of it.
///
/// A new `Role::Background` was the alternative and is not taken: it would be a wire change, a
/// compositor stacking rule and a `check-display` reference, to express something three existing
/// properties already say.
///
/// **Two buffers, and the second is dead.** `libsurface` refuses fewer, for a reason that is
/// exactly right in general and does not apply here: a buffer is busy from commit until the
/// compositor releases it, and the compositor releases the one that *left* the screen — so a
/// single-buffered client can never draw a second frame. This window draws one frame ever and
/// never asks for another, but the library cannot tell that kind of client from a stalled one.
/// Four megabytes at 1280x800, on a 256 MB machine; a `create_static` that took the promise
/// instead of the count is where this would go if it ever mattered.
///
/// Every failure returns `None` after saying which one it was: the desktop falls back to its
/// ground colour, which is what a session with no wallpaper looks like anyway.
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
            // **Absent and unreadable are different**, the same distinction `read_theme` makes:
            // a person who named a file that is sitting there needs to know it was the *reading*
            // that failed.
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
    // **Fit or fill, as the theme says** (M12 decision 7's key; `fill` since Phase 5 Part E).
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
    // **Everything past `create` unwinds through one exit, because the window now exists.**
    // Returning `None` from the middle would leave the compositor holding a full-screen,
    // configured, hit-testable `panel` with nothing committed to it, for the life of the
    // process — no manager is attached this early, so its initial `Configure` goes out at once.
    // `open_overview` in `overview.rs` says this, and the modal it had before the Applications menu
    // said it too, each after a review found it (PR #244 blocking 3, PR #237 finding 7); this is
    // the third (PR #272 review, worth fixing 3). `Child::open`, which the menus use, says it in
    // `libui`.
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
        // A pattern guard cannot borrow mutably, so the commit is a statement rather than a
        // `match` arm's condition.
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
    // **After the commit, not before it.** The first version of this line was printed as soon as
    // the picture had been decoded and placed — so it said the wallpaper was fitted while
    // `CreateWindow` went on to fail, and the gate asserting it passed against a desktop with no
    // picture on it. A line has to be printed where the thing it claims has happened.
    //
    // **Both sizes, because they answer different questions.** The decoded size can only have
    // come from an `IHDR` that was read; the drawn size can only have come from the placement
    // arithmetic — fit or fill — having run on it. A gate asserting one proves less than a gate
    // asserting both.
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
/// **In the user's home rather than in `/etc`**, and that is a namespace decision rather than a
/// filing preference. A session namespace binds `/home`, `/bin`, `/dev/tty` and — for a
/// graphical one — `/system/fonts`; it has no `/etc`, and `session-mgr/CLAUDE.md` says adding a
/// member is a design decision each time. A theme is *a user's*, so the subtree a user already
/// owns is where it belongs, and no new authority is needed to read it. It is also what makes
/// the missing-file case testable from a prompt: the file is somewhere the person can delete.
///
/// **Every failure lands on the default**, silently as far as the screen is concerned: no file,
/// an unreadable one, bytes that are not UTF-8, or a file of nothing but typos all produce the
/// desktop that shipped. A theme is decoration, and a desktop that will not start because its
/// colours did not parse is a worse failure than any colour could be.
pub(super) fn read_theme(ns: u64) -> Theme {
    // **One line whatever happens, beginning the same way**, so a gate can assert that the shell
    // *decided* about a theme without asserting which way it went — which is what makes "delete
    // the file and everything still renders" a control that can be re-run against the committed
    // gate rather than one that needs a step edited out (PR #263 review, finding 4).
    let bytes = match libfs::read_file(ns, THEME_PATH.as_bytes()) {
        Ok(b) => b,
        Err(e) => {
            // **Absent and unreadable are different**, and saying "no such file" about a file
            // that is sitting there is how somebody spends an afternoon looking for it.
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
    // **Each one named, up to a bound.** A person editing colours needs the line number, and a
    // file of a thousand bad lines must not become a thousand console lines.
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

/// Where the session's theme lives, in the user's own subtree.
const THEME_PATH: &str = "/home/theme.toml";

/// How many bad theme lines are named before the rest are counted only.
const MAX_LOGGED_THEME_ISSUES: usize = 8;
