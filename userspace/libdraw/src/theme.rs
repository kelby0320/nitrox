//! Every colour the desktop draws itself in, and the size it draws text at.
//!
//! The toolkit and the compositor both link this crate and the compositor does not depend on
//! `libui`, so a value both paint with belongs here. One type: these values arrive together.
//!
//! Not here: the terminal's ANSI palette (defined by what programs expect) and chrome metrics,
//! which stay constants so gates click title bars at fixed offsets without reading a theme.

use crate::format::Rgb;

/// The shadow a floating window casts in `scheme`.
///
/// Here, not in the compositor, because `cargo xtask check-display` renders the expected screen
/// from it pixel for pixel. Not a `Theme` field: a shadow is black at a coverage, and only its
/// strength depends on the palette, hence the scheme argument.
///
/// The design's `--sh`: `0 10px 28px` at 22% and `0 2px 6px` at 14% (light), `0 12px 34px` at
/// 55% and `0 2px 8px` at 40% (`deep`). Blur radius and drop become radius and offset, alpha a
/// strength out of 255.
pub const fn window_shadow(scheme: Scheme) -> crate::compose::Shadow {
    use crate::compose::{Shadow, ShadowLayer};
    use crate::geom::Point;
    let (wide, contact) = match scheme {
        // Dropped, not centred: a shadow directly under a window reads as a glow.
        Scheme::Light => ((28, 10, 56), (6, 2, 36)),
        Scheme::Dark => ((34, 12, 140), (8, 2, 102)),
    };
    // A nested `const fn`, since a `const fn` cannot call a closure.
    const fn layer((radius, drop, strength): (u32, i32, u8)) -> ShadowLayer {
        ShadowLayer { radius, offset: Point::new(0, drop), colour: Rgb::BLACK, strength }
    }
    Shadow { layers: [layer(wide), layer(contact)] }
}

/// The colours and text size everything on screen is drawn from.
///
/// A `const fn` constructor per theme lets the compositor keep colours as `const` items.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Theme {
    /// Which built-in palette this theme starts from.
    ///
    /// Told to the compositor, which draws shadows from compiled values (the dark one is much
    /// darker), and carried on the wire so a reader knows what `accent`'s washes derive from.
    pub scheme: Scheme,

    /// What a damaged region is cleared to: a window's ground, not the [`desktop`](Self::desktop).
    pub background: Rgb,
    /// Text and other ink.
    pub foreground: Rgb,
    /// Ink that should be read second, such as a file's size beside its name (the design's
    /// `--fgdim`). A shade of the foreground would dim differently on light and dark grounds.
    pub foreground_dim: Rgb,

    /// A button's face at rest.
    pub face: Rgb,
    /// Its face under the pointer.
    pub face_hover: Rgb,
    /// Its face while held.
    pub face_pressed: Rgb,
    /// The one colour of emphasis: focus ring, caret, and the base of the selection (20%) and
    /// hover (10%, 18% dark) washes, which the design computes from it. See
    /// [`selection`](Theme::selection).
    pub accent: Rgb,
    /// A list's ground, the paper its rows sit on.
    pub track: Rgb,
    /// A scrollbar's groove. Darker than [`track`](Self::track) on purpose, so a person can see
    /// where the scrollbar is when the thumb is elsewhere.
    pub groove: Rgb,
    /// A panel beside content, plainly not paper: the file browser's sidebar.
    pub sidebar: Rgb,
    /// The desktop's top and bottom bars. Not a shade of [`sidebar`](Self::sidebar) or
    /// [`face`](Self::face): light is lighter than a face, dark is the darkest surface.
    pub panel: Rgb,
    /// A scrollbar's thumb.
    pub thumb: Rgb,

    // Syntax colours encode meaning, so they are not derived from the ground and foreground, like
    // `accent`, `ok` and `deny`. Chosen for contrast against a white ground.
    /// A language's reserved words — `if`, `fn`, `let`, and TOML's `true`/`false`.
    pub syntax_keyword: Rgb,
    /// String literals, Markdown code spans and the inside of a fenced block.
    pub syntax_string: Rgb,
    /// Comments, and Markdown block quotes — the two things a reader skips.
    pub syntax_comment: Rgb,
    /// Numeric literals.
    pub syntax_number: Rgb,
    /// Structural markers at line level: a Markdown heading, a TOML `[table]` header.
    pub syntax_heading: Rgb,
    /// A shell variable (`$name`, `${name}`): `$PATH` standing out is the most useful colour in an
    /// `nxsh` file.
    pub syntax_variable: Rgb,

    /// Something working as it should — a running window's dot in the window list.
    pub ok: Rgb,
    /// Something that destroys or reaches past the person's own files, such as a destructive menu
    /// item. Legible as text on every menu ground, so the dark scheme's differs from the design's
    /// `--deny` (2.9:1 on its dark ground).
    pub deny: Rgb,

    /// A title bar's face while its window holds the keyboard: the accent washed over the face,
    /// a deliberate divergence from the design.
    pub title_active: Rgb,
    /// A title bar's face while it does not; the only chrome that says which window is focused.
    pub title_inactive: Rgb,

    /// The pointer's fill.
    pub cursor_body: Rgb,
    /// The pointer's outline, so it stays visible against white.
    pub cursor_outline: Rgb,
    /// A resize, snap preview or drop target outline; one colour for all three.
    pub outline: Rgb,

    /// The line around a window, a menu and anything with an edge: one grey where the reference
    /// desktop uses three within a few units.
    pub border: Rgb,

    /// The ground between windows. Read but not live: the compositor never sees a theme file, so
    /// `scene::BACKGROUND` takes its value from here to keep them from drifting.
    pub desktop: Rgb,

    /// Text size, in pixels per em.
    pub font_px: f32,

    /// How far a gradient's top is lightened and its bottom darkened, per channel. Zero is flat;
    /// the upper bound is [`MAX_BEVEL`].
    pub bevel: u8,
    /// The proportional font every label, button and list row is drawn with.
    pub font_ui: ThemePath,
    /// The font a character grid is drawn with: a grid needs a fixed advance, since `libterm`
    /// takes its cell width from one glyph.
    pub font_mono: ThemePath,
    /// A picture drawn behind everything, or `None`. The file names it and `desktop-shell` reads
    /// it; the compositor holds no filesystem. `Option` so a theme on the wire, always complete
    /// (see [`to_config`](Self::to_config)), can say "none".
    pub wallpaper: Option<ThemePath>,
    /// How a wallpaper is placed when it is not the screen's size. An unknown value is refused
    /// by name; neither mode scales up (`TODO(wallpaper-fill)`).
    pub wallpaper_mode: WallpaperMode,
}

/// How a wallpaper is placed. See [`Theme::wallpaper_mode`].
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum WallpaperMode {
    /// Scale down to fit inside the screen, centre if smaller ([`scale::fit`](crate::scale::fit)).
    #[default]
    Fit,
    /// Scale down to cover, cropping the overhang ([`scale::fill`](crate::scale::fill)).
    Fill,
}

impl WallpaperMode {
    /// The name this writes and reads.
    pub fn as_str(self) -> &'static str {
        match self {
            WallpaperMode::Fit => "fit",
            WallpaperMode::Fill => "fill",
        }
    }

    /// Parse a mode name — `None` for one that does not exist.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "fit" => Some(WallpaperMode::Fit),
            "fill" => Some(WallpaperMode::Fill),
            _ => None,
        }
    }

    /// Plan where a picture of `image` goes on a `screen` of this mode.
    pub fn plan(self, image: crate::geom::Size, screen: crate::geom::Size) -> crate::scale::Fit {
        match self {
            WallpaperMode::Fit => crate::scale::fit(image, screen),
            WallpaperMode::Fill => crate::scale::fill(image, screen),
        }
    }
}

/// Which built-in palette a theme starts from. Named for CSS's `color-scheme`, since
/// `libterm::Palette` is the terminal's ANSI colours.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Scheme {
    /// Dark ink on light grounds. The built-in theme's.
    #[default]
    Light,
    /// Light ink on dark grounds.
    Dark,
}

impl Scheme {
    /// The name this writes and reads.
    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Light => "light",
            Scheme::Dark => "dark",
        }
    }

    /// Parse a scheme name — `None` for one that does not exist.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "light" => Some(Scheme::Light),
            "dark" => Some(Scheme::Dark),
            _ => None,
        }
    }
}

/// How much of the accent a selection is: the design's `accent + '33'`, 20%, in both schemes.
pub const SELECTION_COVERAGE: u8 = 0x33;

impl Scheme {
    /// How much of the accent a hovered row or menu item is: the design's `--soft`, 10% (`1A`) in
    /// the light palette and 18% (`2E`) in the dark one, where the accent needs more of itself.
    pub const fn hover_coverage(self) -> u8 {
        match self {
            Scheme::Light => 0x1A,
            Scheme::Dark => 0x2E,
        }
    }
}

impl Theme {
    /// The light scheme, the built-in theme: the design's palette, read from its stylesheet
    /// (`docs/design/nitrox-shell/`); `docs/spec/theme-toml-schema.md` has the table.
    pub const fn light() -> Self {
        Self {
            scheme: Scheme::Light,
            background: Rgb::new(0xFF, 0xFF, 0xFF),
            foreground: Rgb::new(0x16, 0x20, 0x1F),
            foreground_dim: Rgb::new(0x5B, 0x67, 0x66),

            // `--face` at rest, `--faceHi` under the pointer, `--faceLo` held.
            face: Rgb::new(0xED, 0xEC, 0xE9),
            face_hover: Rgb::new(0xF8, 0xF7, 0xF4),
            face_pressed: Rgb::new(0xDF, 0xDD, 0xD8),
            accent: Rgb::new(0x2C, 0x7F, 0x92),
            // A list sits on the window's own ground; its scrollbar track is `--faceLo`, its thumb
            // `--line`.
            track: Rgb::new(0xFF, 0xFF, 0xFF),
            groove: Rgb::new(0xDF, 0xDD, 0xD8),
            sidebar: Rgb::new(0xE5, 0xE3, 0xDE),
            panel: Rgb::new(0xF2, 0xF1, 0xED),
            thumb: Rgb::new(0xC7, 0xC4, 0xBD),

            // Chosen for contrast on white; the ground is still white.
            syntax_keyword: Rgb::new(0x7A, 0x3E, 0x9D),
            syntax_string: Rgb::new(0xA0, 0x30, 0x00),
            syntax_comment: Rgb::new(0x5E, 0x7A, 0x5E),
            syntax_number: Rgb::new(0x1E, 0x6F, 0xA8),
            syntax_heading: Rgb::new(0x1A, 0x4C, 0x8B),
            syntax_variable: Rgb::new(0x8A, 0x5A, 0x00),

            ok: Rgb::new(0x3C, 0x7A, 0x5A),
            deny: Rgb::new(0xA4, 0x45, 0x3C),

            title_active: Rgb::new(0xCA, 0xD8, 0xD9),
            title_inactive: Rgb::new(0xED, 0xEC, 0xE9),

            cursor_body: Rgb::new(0xFF, 0xFF, 0xFF),
            cursor_outline: Rgb::new(0x00, 0x00, 0x00),
            // Saturated, because it is drawn over both the desktop and its windows: not the
            // accent, which would vanish over the teal desktop.
            outline: Rgb::new(0x2C, 0x65, 0xAE),

            border: Rgb::new(0xC7, 0xC4, 0xBD),
            // The deep teal of the design's `reef` wallpaper, the same in both schemes.
            desktop: Rgb::new(0x13, 0x52, 0x5D),

            // 13: DejaVu's tall lowercase makes 12 read like the design's Plex at 11.5. The image
            // stages 12 on purpose (`xtask`'s `THEME_FONT_PX`).
            font_px: 13.0,
            // Flat, as the design is.
            bevel: 0,
            font_ui: ThemePath::new(crate::text::UI_FONT_PATH),
            font_mono: ThemePath::new(crate::text::MONO_FONT_PATH),
            // The shipped theme names no picture: a wallpaper is a file a person supplies.
            wallpaper: None,
            wallpaper_mode: WallpaperMode::Fit,
        }
    }

    /// The dark scheme: the design's `deep` palette. Chosen here where it specifies none: the
    /// syntax colours (each at least 6:1 on this ground), `deny` and the title faces.
    pub const fn dark() -> Self {
        Self {
            scheme: Scheme::Dark,
            background: Rgb::new(0x13, 0x1A, 0x1B),
            foreground: Rgb::new(0xE6, 0xED, 0xEC),
            foreground_dim: Rgb::new(0x8B, 0x9A, 0x99),

            face: Rgb::new(0x1E, 0x27, 0x28),
            face_hover: Rgb::new(0x23, 0x2D, 0x2E),
            face_pressed: Rgb::new(0x17, 0x1F, 0x20),
            accent: Rgb::new(0x2C, 0x7F, 0x92),
            track: Rgb::new(0x13, 0x1A, 0x1B),
            groove: Rgb::new(0x17, 0x1F, 0x20),
            sidebar: Rgb::new(0x18, 0x20, 0x20),
            panel: Rgb::new(0x10, 0x16, 0x17),
            thumb: Rgb::new(0x36, 0x43, 0x42),

            syntax_keyword: Rgb::new(0xC3, 0x9B, 0xE0),
            syntax_string: Rgb::new(0xE8, 0x89, 0x6A),
            syntax_comment: Rgb::new(0x86, 0xA3, 0x86),
            syntax_number: Rgb::new(0x6F, 0xB4, 0xE8),
            syntax_heading: Rgb::new(0x8F, 0xB4, 0xF0),
            syntax_variable: Rgb::new(0xD9, 0xA8, 0x45),

            ok: Rgb::new(0x3C, 0x7A, 0x5A),
            deny: Rgb::new(0xD4, 0x6F, 0x63),

            title_active: Rgb::new(0x21, 0x37, 0x3B),
            title_inactive: Rgb::new(0x1E, 0x27, 0x28),

            cursor_body: Rgb::new(0xFF, 0xFF, 0xFF),
            cursor_outline: Rgb::new(0x00, 0x00, 0x00),
            outline: Rgb::new(0x2C, 0x65, 0xAE),

            border: Rgb::new(0x36, 0x43, 0x42),
            desktop: Rgb::new(0x13, 0x52, 0x5D),

            font_px: 13.0,
            bevel: 0,
            font_ui: ThemePath::new(crate::text::UI_FONT_PATH),
            font_mono: ThemePath::new(crate::text::MONO_FONT_PATH),
            wallpaper: None,
            wallpaper_mode: WallpaperMode::Fit,
        }
    }

    /// The built-in theme `scheme` names.
    pub const fn for_scheme(scheme: Scheme) -> Self {
        match scheme {
            Scheme::Light => Self::light(),
            Scheme::Dark => Self::dark(),
        }
    }

    /// The colour behind selected text: the [`accent`](Self::accent) at [`SELECTION_COVERAGE`]
    /// over the [`background`](Self::background). Derived, not stored (see `accent`).
    pub const fn selection(&self) -> Rgb {
        self.accent.blend(self.background, SELECTION_COVERAGE)
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::light()
    }
}

/// What was wrong with one line of a theme file. Reported rather than fatal: a typo must not lock
/// a person out of the machine they are editing colours on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Issue {
    /// One-based line number, as an editor counts.
    pub line: usize,
    /// What was wrong with it.
    pub kind: IssueKind,
}

/// The three ways a line can fail.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IssueKind {
    /// No `=`, so there is no key and no value.
    Malformed,
    /// A key this version does not know; kept as an issue because a misspelling looks like a
    /// future key.
    UnknownKey,
    /// A key this version knows, with a value it cannot read.
    BadValue,
}

impl Theme {
    /// Read a theme from `key = "value"` lines, starting from the scheme the file names.
    ///
    /// A focused reader, not a TOML parser: flat `key = value` only, and what it accepts is valid
    /// TOML. Missing keys keep their defaults and unknown keys are skipped, so a newer file starts
    /// an older system; an empty file is exactly [`light`](Self::light). `scheme` is read first
    /// wherever it is, since it decides what every other line overrides.
    ///
    /// Colours are `"#RRGGBB"`; `font_px` is a decimal number. A `#` comment runs to the end of a
    /// line except inside the quotes of a value.
    pub fn from_config(text: &str) -> (Self, alloc::vec::Vec<Issue>) {
        // The last well-formed `scheme` line decides the starting palette; a bad one is reported
        // below and starts from the default.
        let scheme = text
            .lines()
            .filter_map(|raw| strip_comment(raw).trim().split_once('='))
            .filter(|(k, _)| k.trim() == "scheme")
            .filter_map(|(_, v)| v.trim().strip_prefix('"')?.strip_suffix('"').and_then(Scheme::parse))
            .next_back()
            .unwrap_or_default();
        let mut t = Self::for_scheme(scheme);
        let mut issues = alloc::vec::Vec::new();
        for (n, raw) in text.lines().enumerate() {
            let line = strip_comment(raw).trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                issues.push(Issue { line: n + 1, kind: IssueKind::Malformed });
                continue;
            };
            let key = key.trim();
            // Both forms are kept: a colour is a TOML string and `font_px` a TOML number, so the
            // quotes are part of the type.
            let raw = value.trim();
            let Some(value) = unquote(raw) else {
                issues.push(Issue { line: n + 1, kind: IssueKind::BadValue });
                continue;
            };
            let ok = match key {
                "scheme" => raw.starts_with('"') && Scheme::parse(value).is_some(),
                "background" => set(&mut t.background, value),
                "foreground" => set(&mut t.foreground, value),
                "foreground_dim" => set(&mut t.foreground_dim, value),
                "face" => set(&mut t.face, value),
                "face_hover" => set(&mut t.face_hover, value),
                "face_pressed" => set(&mut t.face_pressed, value),
                "accent" => set(&mut t.accent, value),
                "track" => set(&mut t.track, value),
                "groove" => set(&mut t.groove, value),
                "sidebar" => set(&mut t.sidebar, value),
                "panel" => set(&mut t.panel, value),
                "thumb" => set(&mut t.thumb, value),
                "syntax_keyword" => set(&mut t.syntax_keyword, value),
                "syntax_string" => set(&mut t.syntax_string, value),
                "syntax_comment" => set(&mut t.syntax_comment, value),
                "syntax_number" => set(&mut t.syntax_number, value),
                "syntax_heading" => set(&mut t.syntax_heading, value),
                "syntax_variable" => set(&mut t.syntax_variable, value),
                "ok" => set(&mut t.ok, value),
                "deny" => set(&mut t.deny, value),
                "title_active" => set(&mut t.title_active, value),
                "title_inactive" => set(&mut t.title_inactive, value),
                "cursor_body" => set(&mut t.cursor_body, value),
                "cursor_outline" => set(&mut t.cursor_outline, value),
                "outline" => set(&mut t.outline, value),
                "border" => set(&mut t.border, value),
                "desktop" => set(&mut t.desktop, value),
                // Unquoted: TOML types a quoted number as a string.
                "bevel" if raw.starts_with('"') => false,
                "bevel" => match value.parse::<u8>() {
                    Ok(v) if v <= MAX_BEVEL => {
                        t.bevel = v;
                        true
                    }
                    _ => false,
                },
                "font_px" if raw.starts_with('"') => false,
                "font_px" => match value.parse::<f32>() {
                    // A size bounded at both ends: zero divides in the layout, and above
                    // [`MAX_FONT_PX`] (16: a list row is 20 with 4 of padding) `paint` clips.
                    Ok(v) if (MIN_FONT_PX..=MAX_FONT_PX).contains(&v) => {
                        // To the nearest hundredth: a value a console line cannot print exactly
                        // would let `check-terminal`'s host and the guest disagree.
                        t.font_px = round_px(v);
                        true
                    }
                    _ => false,
                },
                // Quoted: a path is a TOML string, and `unquote` accepts a bare value.
                "font_ui" | "font_mono" | "wallpaper" | "wallpaper_mode" if !raw.starts_with('"') => false,
                "font_ui" => set_path(&mut t.font_ui, value),
                "font_mono" => set_path(&mut t.font_mono, value),
                // An empty string is "no wallpaper": `to_config` writes one for `None`.
                "wallpaper" => {
                    if value.is_empty() {
                        t.wallpaper = None;
                        true
                    } else {
                        match ThemePath::parse(value) {
                            Some(p) => {
                                t.wallpaper = Some(p);
                                true
                            }
                            None => false,
                        }
                    }
                }
                "wallpaper_mode" => match WallpaperMode::parse(value) {
                    Some(m) => {
                        t.wallpaper_mode = m;
                        true
                    }
                    None => false,
                },
                _ => {
                    issues.push(Issue { line: n + 1, kind: IssueKind::UnknownKey });
                    continue;
                }
            };
            if !ok {
                issues.push(Issue { line: n + 1, kind: IssueKind::BadValue });
            }
        }
        (t, issues)
    }

    /// Write the theme back in the form [`from_config`](Self::from_config) reads: every field,
    /// always, so a reader never falls back to a default (the wire is complete and validated).
    pub fn to_config(&self) -> alloc::string::String {
        use core::fmt::Write as _;
        // Destructured so a new `Theme` field fails to compile until it is written out.
        let Theme {
            scheme,
            background,
            foreground,
            foreground_dim,
            face,
            face_hover,
            face_pressed,
            accent,
            track,
            groove,
            sidebar,
            panel,
            thumb,
            syntax_keyword,
            syntax_string,
            syntax_comment,
            syntax_number,
            syntax_heading,
            syntax_variable,
            ok,
            deny,
            title_active,
            title_inactive,
            cursor_body,
            cursor_outline,
            outline,
            border,
            desktop,
            font_px,
            bevel,
            font_ui,
            font_mono,
            wallpaper,
            wallpaper_mode,
        } = *self;
        let mut s = alloc::string::String::new();
        // First, so a reader sees what the colours below are overrides on.
        let _ = writeln!(s, "scheme = \"{}\"", scheme.as_str());
        for (k, c) in [
            ("background", background),
            ("foreground", foreground),
            ("foreground_dim", foreground_dim),
            ("face", face),
            ("face_hover", face_hover),
            ("face_pressed", face_pressed),
            ("accent", accent),
            ("track", track),
            ("groove", groove),
            ("sidebar", sidebar),
            ("panel", panel),
            ("thumb", thumb),
            ("syntax_keyword", syntax_keyword),
            ("syntax_string", syntax_string),
            ("syntax_comment", syntax_comment),
            ("syntax_number", syntax_number),
            ("syntax_heading", syntax_heading),
            ("syntax_variable", syntax_variable),
            ("ok", ok),
            ("deny", deny),
            ("title_active", title_active),
            ("title_inactive", title_inactive),
            ("cursor_body", cursor_body),
            ("cursor_outline", cursor_outline),
            ("outline", outline),
            ("border", border),
            ("desktop", desktop),
        ] {
            let _ = writeln!(s, "{k} = \"#{:02X}{:02X}{:02X}\"", c.r, c.g, c.b);
        }
        let _ = writeln!(s, "font_px = {font_px}");
        let _ = writeln!(s, "bevel = {bevel}");
        // Quoted, the form `from_config` accepts.
        let _ = writeln!(s, "font_ui = \"{}\"", font_ui.as_str());
        let _ = writeln!(s, "font_mono = \"{}\"", font_mono.as_str());
        // Written even when there is none, as an empty string.
        let _ = writeln!(s, "wallpaper = \"{}\"", wallpaper.as_ref().map_or("", |p| p.as_str()));
        let _ = writeln!(s, "wallpaper_mode = \"{}\"", wallpaper_mode.as_str());
        s
    }
}

/// A path a theme names, bounded so a [`Theme`] stays `Copy` and `const`-constructible.
///
/// Fixed-capacity because heap allocations cannot appear in a `const` and the path travels on the
/// 4 KiB setup record. Absolute, non-empty, at most [`MAX_THEME_PATH`] bytes, and free of control
/// characters (it is logged), `"` and `\` (it is written back as a TOML basic string).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ThemePath {
    /// Zero-filled past `len`, so derived equality compares paths, not leftover bytes.
    bytes: [u8; MAX_THEME_PATH],
    len: u8,
}

impl ThemePath {
    /// Panics on a path this type cannot hold, so built-in paths are checked at compile time;
    /// readers of a file call [`parse`](Self::parse).
    pub const fn new(s: &str) -> Self {
        let b = s.as_bytes();
        assert!(usable(b), "a theme path must be absolute, printable, and fit MAX_THEME_PATH");
        let mut bytes = [0u8; MAX_THEME_PATH];
        let mut i = 0;
        while i < b.len() {
            bytes[i] = b[i];
            i += 1;
        }
        Self { bytes, len: b.len() as u8 }
    }

    /// A path from a theme file — `None` for one this type cannot hold.
    pub fn parse(s: &str) -> Option<Self> {
        usable(s.as_bytes()).then(|| Self::new(s))
    }

    /// The path, for a namespace lookup or a log line.
    pub fn as_str(&self) -> &str {
        // Valid UTF-8 by construction; the fallback keeps a graphics path panic-free.
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }
}

impl core::fmt::Debug for ThemePath {
    /// The path, not 64 bytes of mostly zeroes.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self.as_str(), f)
    }
}

/// Whether `b` is a path [`ThemePath`] can hold.
const fn usable(b: &[u8]) -> bool {
    if b.is_empty() || b.len() > MAX_THEME_PATH || b[0] != b'/' {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        // Control bytes and the two characters a TOML basic string cannot carry unescaped;
        // UTF-8 high bytes are fine.
        if b[i] < 0x20 || b[i] == 0x7F || b[i] == b'"' || b[i] == b'\\' {
            return false;
        }
        i += 1;
    }
    true
}

// `len` is a `u8`: raising the bound past 255 would truncate silently.
const _: () = assert!(MAX_THEME_PATH <= u8::MAX as usize);

/// Enough for `/system/fonts/` plus a long family name, or a wallpaper under a home directory;
/// three of them are noise beside the setup record's 4 KiB.
pub const MAX_THEME_PATH: usize = 128;

/// A size rounded to the precision [`px_parts`] can print without loss.
fn round_px(v: f32) -> f32 {
    libm::roundf(v * 100.0) / 100.0
}

/// Every size is exact to a hundredth ([`Theme::from_config`] rounds there), so these two
/// integers are the whole value `check-terminal` re-measures the font at.
pub fn px_parts(px: f32) -> (u64, u64) {
    let px = round_px(px).max(0.0);
    let whole = px as u64;
    (whole, libm::roundf((px - whole as f32) * 100.0) as u64)
}

/// A limit on taste: `shade` clamps, but past about a quarter of the range a gradient is two
/// colours with a seam and a title bar's text crosses both.
pub const MAX_BEVEL: u8 = 64;

/// Below this the rasteriser produces unreadable shapes. It also bounds the toolkit's steps:
/// `libui::element::TextSize::px` clamps here.
pub const MIN_FONT_PX: f32 = 6.0;

/// The largest, and what the chrome holds: the tightest fixed box is a list row, `ROW_H` 20 less
/// `ROW_PAD` 2 above and below, leaving 16. This knob shrinks and does not grow; raising it needs
/// metrics that follow type, which would make gates read a theme.
pub const MAX_FONT_PX: f32 = 16.0;

/// A basic string's contents, or a bare value unchanged; `None` for a half-quoted one, which
/// `trim_matches('"')` accepted and TOML does not.
fn unquote(value: &str) -> Option<&str> {
    match (value.starts_with('"'), value.ends_with('"'), value.len()) {
        (true, true, n) if n >= 2 => Some(&value[1..n - 1]),
        (false, false, _) => Some(value),
        _ => None,
    }
}

/// Everything before an unquoted `#`.
fn strip_comment(line: &str) -> &str {
    let mut quoted = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted => return &line[..i],
            _ => {}
        }
    }
    line
}

/// Parse `#RRGGBB` into `slot`, answering whether it was one.
fn set(slot: &mut Rgb, value: &str) -> bool {
    let Some(hex) = value.strip_prefix('#') else { return false };
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).unwrap_or(0);
    *slot = Rgb::new(byte(0), byte(2), byte(4));
    true
}

/// Parse a font path into `slot`, answering whether it was one.
fn set_path(slot: &mut ThemePath, value: &str) -> bool {
    match ThemePath::parse(value) {
        Some(p) => {
            *slot = p;
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_light_theme_and_is_available_in_a_const() {
        const CURSOR: Rgb = Theme::light().cursor_body;
        assert_eq!(CURSOR, Rgb::new(0xFF, 0xFF, 0xFF));
        assert_eq!(Theme::default(), Theme::light());
    }

    #[test]
    fn a_file_overrides_what_it_names_and_nothing_else() {
        let (t, issues) = Theme::from_config("# my theme\nbackground = \"#102030\"\n\nfont_px = 13.5   # smaller\n");
        assert_eq!(t.background, Rgb::new(0x10, 0x20, 0x30));
        assert_eq!(t.font_px, 13.5);
        assert_eq!(t.face, Theme::light().face, "a key the file did not name keeps its default");
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn an_absent_or_empty_file_is_exactly_the_default() {
        for text in ["", "\n\n", "# nothing but a comment\n"] {
            let (t, issues) = Theme::from_config(text);
            assert_eq!(t, Theme::light(), "{text:?}");
            assert!(issues.is_empty());
        }
    }

    #[test]
    fn a_bad_line_costs_its_own_field_and_no_others() {
        // A theme is decoration: every failure leaves that one field at its default and the rest
        // is still read.
        let (t, issues) = Theme::from_config(
            "background = \"#zzzzzz\"\n             face\n             frobnicate = \"#112233\"\n             font_px = 0\n             foreground = \"#010203\"\n",
        );
        assert_eq!(t.background, Theme::light().background, "a bad colour keeps the default");
        assert_eq!(t.font_px, Theme::light().font_px, "and a size outside the readable range");
        assert_eq!(t.foreground, Rgb::new(1, 2, 3), "the line after the bad ones was still read");
        assert_eq!(
            issues,
            [
                Issue { line: 1, kind: IssueKind::BadValue },
                Issue { line: 2, kind: IssueKind::Malformed },
                Issue { line: 3, kind: IssueKind::UnknownKey },
                Issue { line: 4, kind: IssueKind::BadValue },
            ],
            "each named by the line an editor would show"
        );
    }

    #[test]
    fn a_value_that_toml_would_read_differently_is_refused() {
        // `trim_matches('"')` made "every accepted file is valid TOML" false in two ways.
        for bad in [
            "background = \"#102030", // one quote
            "background = #102030\"", // the other
            "font_px = \"14\"",       // TOML types this as a string, not a number
        ] {
            let (t, issues) = Theme::from_config(bad);
            assert_eq!(t, Theme::light(), "{bad:?} changed something");
            assert_eq!(issues.len(), 1, "{bad:?}");
            assert_eq!(issues[0].kind, IssueKind::BadValue, "{bad:?}");
        }
        let (t, issues) = Theme::from_config("background = \"#102030\"\nfont_px = 14\n");
        assert_eq!(t.background, Rgb::new(0x10, 0x20, 0x30));
        assert_eq!(t.font_px, 14.0);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn a_font_size_outside_the_readable_range_is_refused() {
        for bad in ["0", "-4", "0.5", "17", "1000", "nan"] {
            let (t, issues) = Theme::from_config(&alloc::format!("font_px = {bad}"));
            assert_eq!(t.font_px, Theme::light().font_px, "font_px = {bad}");
            assert_eq!(issues.len(), 1, "font_px = {bad}");
        }
        for good in ["6", "10", "13.5", "16"] {
            let (_, issues) = Theme::from_config(&alloc::format!("font_px = {good}"));
            assert!(issues.is_empty(), "font_px = {good} should be accepted");
        }
    }

    #[test]
    fn a_hash_inside_a_value_is_not_a_comment() {
        let (t, issues) = Theme::from_config("accent = \"#ABCDEF\" # the ring\n");
        assert_eq!(t.accent, Rgb::new(0xAB, 0xCD, 0xEF));
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn what_is_written_is_complete_and_reads_back_as_itself() {
        let mut t = Theme::light();
        t.background = Rgb::new(0x01, 0x02, 0x03);
        t.accent = Rgb::new(0xFE, 0xDC, 0xBA);
        t.font_px = 13.0;
        t.font_ui = ThemePath::new("/home/Fancy.ttf");
        t.wallpaper = Some(ThemePath::new("/home/alice/hills.png"));

        let text = t.to_config();
        assert_eq!(
            text.lines().count(),
            34,
            "a scheme, twenty-one colours, six syntax colours, a size, a bevel, two fonts, a \
             wallpaper and its mode"
        );
        let (back, issues) = Theme::from_config(&text);
        assert_eq!(back, t);
        assert!(issues.is_empty(), "{issues:?}");
        // A named key as well as the count: a field written under the wrong name keeps the
        // default on reading while `back == t` still holds.
        assert!(
            text.contains("syntax_comment = \"#5E7A5E\""),
            "the syntax colours are not written under the names the reader accepts:\n{text}"
        );

        let (over, _) = Theme::from_config(&text);
        assert_eq!(over, t, "every field is present, so nothing is inherited");

        // `None` has to survive the wire, or a shell would fall back to its own default.
        let bare = Theme::light();
        assert_eq!(bare.wallpaper, None, "the shipped theme names none");
        let (read_back, issues) = Theme::from_config(&bare.to_config());
        assert_eq!(read_back, bare);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn a_wallpaper_mode_that_does_not_exist_is_refused_by_name() {
        // Silently fitting would leave a person concluding the mode was broken; `stretch` makes
        // every face the wrong shape and is not a mode.
        let (t, issues) = Theme::from_config("wallpaper_mode = \"stretch\"\n");
        assert_eq!(t.wallpaper_mode, WallpaperMode::Fit, "the default is unchanged");
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].kind, IssueKind::BadValue);
        for (name, mode) in [("fit", WallpaperMode::Fit), ("fill", WallpaperMode::Fill)] {
            let (t, issues) = Theme::from_config(&alloc::format!("wallpaper_mode = \"{name}\"\n"));
            assert_eq!(t.wallpaper_mode, mode);
            assert!(issues.is_empty(), "{issues:?}");
            assert!(t.to_config().contains(&alloc::format!("wallpaper_mode = \"{name}\"")));
        }
    }

    #[test]
    fn each_wallpaper_mode_plans_with_its_own_arithmetic() {
        use crate::geom::Size;
        let (image, screen) = (Size::new(1920, 1200), Size::new(1360, 768));
        assert_eq!(WallpaperMode::Fit.plan(image, screen), crate::scale::fit(image, screen));
        assert_eq!(WallpaperMode::Fill.plan(image, screen), crate::scale::fill(image, screen));
        assert_ne!(WallpaperMode::Fit.plan(image, screen), WallpaperMode::Fill.plan(image, screen));
    }

    #[test]
    fn a_wallpaper_path_is_bounded_and_quoted_like_a_font_path_is() {
        let (_, issues) = Theme::from_config("wallpaper = /home/x.png\n");
        assert_eq!(issues.len(), 1, "a bare path is refused");
        let (_, issues) = Theme::from_config("wallpaper = \"relative.png\"\n");
        assert_eq!(issues.len(), 1, "so is a relative one");
        let long = "/".repeat(MAX_THEME_PATH + 1);
        let (_, issues) = Theme::from_config(&alloc::format!("wallpaper = \"{long}\"\n"));
        assert_eq!(issues.len(), 1, "and one past the bound");
        let (t, issues) = Theme::from_config("wallpaper = \"\"\n");
        assert_eq!(t.wallpaper, None);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn a_theme_names_two_fonts_and_the_defaults_are_where_the_image_stages_them() {
        // `xtask` stages exactly these two names into `/system/fonts`.
        let t = Theme::light();
        assert_eq!(t.font_ui.as_str(), crate::text::UI_FONT_PATH);
        assert_eq!(t.font_mono.as_str(), crate::text::MONO_FONT_PATH);
        // Two different files: the desktop stopped drawing itself in a terminal's font.
        assert_ne!(t.font_ui, t.font_mono);
    }

    #[test]
    fn a_font_path_is_absolute_bounded_and_free_of_control_bytes() {
        assert_eq!(ThemePath::parse("/system/fonts/DejaVuSans.ttf").map(|p| p.as_str().len()), Some(28));
        assert!(ThemePath::parse("/home/\u{c9}criture.ttf").is_some());

        assert_eq!(ThemePath::parse(""), None, "empty");
        assert_eq!(ThemePath::parse("DejaVuSans.ttf"), None, "relative");
        assert_eq!(ThemePath::parse("/home/a\nb.ttf"), None, "a path with a newline is not a path");
        // A quote or backslash would round-trip here and mean something else to another TOML
        // parser.
        assert_eq!(ThemePath::parse("/home/a\"b.ttf"), None, "a quote would escape the string");
        assert_eq!(ThemePath::parse("/home/a\\b.ttf"), None, "a backslash would be an escape");
        let long = alloc::format!("/home/{}.ttf", "x".repeat(MAX_THEME_PATH));
        assert_eq!(ThemePath::parse(&long), None, "longer than the record can carry");
        let exact = alloc::format!("/{}", "x".repeat(MAX_THEME_PATH - 1));
        assert_eq!(ThemePath::parse(&exact).map(|p| p.as_str().len()), Some(MAX_THEME_PATH));
    }

    #[test]
    fn a_font_path_compares_as_a_path_and_not_as_its_buffer() {
        // What this pins is `len`: filling with `0xFF` instead leaves every test green, but
        // `as_str` ignoring `len` fails five.
        let a = ThemePath::new("/system/fonts/DejaVuSans.ttf");
        let b = ThemePath::parse("/system/fonts/DejaVuSans.ttf").expect("a usable path");
        assert_eq!(a, b);
        assert_ne!(a, ThemePath::new("/system/fonts/DejaVuSansMono.ttf"));
        assert_eq!(alloc::format!("{a:?}"), "\"/system/fonts/DejaVuSans.ttf\"");
    }

    #[test]
    fn a_font_key_takes_a_quoted_path_and_refuses_what_toml_reads_differently() {
        let (t, issues) = Theme::from_config("font_ui = \"/home/Fancy.ttf\"\n");
        assert_eq!(t.font_ui.as_str(), "/home/Fancy.ttf");
        assert_eq!(t.font_mono, Theme::light().font_mono, "the other role kept its default");
        assert!(issues.is_empty(), "{issues:?}");

        for bad in ["font_ui = /home/Fancy.ttf\n", "font_ui = \"Fancy.ttf\"\n", "font_mono = \"\"\n"] {
            let (t, issues) = Theme::from_config(bad);
            assert_eq!(t, Theme::light(), "{bad:?} left the theme alone");
            assert_eq!(issues, [Issue { line: 1, kind: IssueKind::BadValue }], "{bad:?} was named as a bad value");
        }
    }

    #[test]
    fn a_bevel_is_a_number_and_is_bounded() {
        let (t, issues) = Theme::from_config("bevel = 0\n");
        assert_eq!(t.bevel, 0, "flat is a theme somebody may want");
        assert!(issues.is_empty(), "{issues:?}");

        for bad in ["bevel = \"12\"\n", "bevel = 65\n", "bevel = -4\n", "bevel = 12.5\n"] {
            let (t, issues) = Theme::from_config(bad);
            assert_eq!(t.bevel, Theme::light().bevel, "{bad:?} left the default");
            assert_eq!(issues, [Issue { line: 1, kind: IssueKind::BadValue }], "{bad:?}");
        }
    }

    #[test]
    fn a_size_is_read_to_a_hundredth_and_prints_back_as_itself() {
        // Every size must survive the trip through a console line: `13.5` printed as `13` is a
        // cell a pixel short.
        for (text, want, parts) in [
            ("font_px = 13.5\n", 13.5, (13, 50)),
            ("font_px = 13.05\n", 13.05, (13, 5)),
            ("font_px = 16\n", 16.0, (16, 0)),
            // Beyond a hundredth the file is rounded, so the two integers are the whole value.
            ("font_px = 13.333\n", 13.33, (13, 33)),
        ] {
            let (t, issues) = Theme::from_config(text);
            assert!(issues.is_empty(), "{text:?} {issues:?}");
            assert_eq!(t.font_px, want, "{text:?}");
            assert_eq!(px_parts(t.font_px), parts, "{text:?}");
            let (whole, cents) = parts;
            assert_eq!(whole as f32 + cents as f32 / 100.0, t.font_px, "{text:?}");
        }
    }

    #[test]
    fn the_scenes_ground_is_still_the_themes_ground() {
        // A provenance guard: `BACKGROUND` derives from this field, so `scene::REFERENCE_HASH`
        // catches a moved pixel; this fails if `BACKGROUND` becomes a literal again.
        let t = Theme::light();
        assert_eq!(t.desktop, crate::scene::BACKGROUND);
        assert_ne!(t.background, crate::scene::BACKGROUND);
    }

    #[test]
    fn a_file_names_its_scheme_and_the_rest_of_it_is_overrides_on_that() {
        let (t, issues) = Theme::from_config("scheme = \"dark\"\n");
        assert_eq!(t, Theme::dark());
        assert!(issues.is_empty(), "{issues:?}");

        for text in ["accent = \"#102030\"\nscheme = \"dark\"\n", "scheme = \"dark\"\naccent = \"#102030\"\n"] {
            let (t, issues) = Theme::from_config(text);
            assert!(issues.is_empty(), "{text:?} {issues:?}");
            assert_eq!(t.accent, Rgb::new(0x10, 0x20, 0x30), "{text:?}");
            assert_eq!(t.background, Theme::dark().background, "{text:?} is not on the dark scheme");
            assert_eq!(t.scheme, Scheme::Dark);
        }
    }

    #[test]
    fn a_scheme_that_does_not_exist_is_named_and_the_file_starts_from_light() {
        for bad in ["scheme = \"sepia\"\n", "scheme = dark\n", "scheme = \"Dark\"\n"] {
            let (t, issues) = Theme::from_config(bad);
            assert_eq!(t, Theme::light(), "{bad:?}");
            assert_eq!(issues, [Issue { line: 1, kind: IssueKind::BadValue }], "{bad:?}");
        }
    }

    #[test]
    fn a_dark_theme_round_trips_and_says_which_scheme_it_is() {
        // The wire carries the scheme: a reader must know what `accent` washes over and the shell
        // must tell the compositor which shadow to draw.
        let mut t = Theme::dark();
        t.accent = Rgb::new(0x3B, 0x5F, 0x8A);
        let text = t.to_config();
        assert!(text.starts_with("scheme = \"dark\"\n"), "{text}");
        let (back, issues) = Theme::from_config(&text);
        assert_eq!(back, t);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn the_selection_is_the_accent_and_follows_it() {
        for base in [Theme::light(), Theme::dark()] {
            assert_eq!(base.selection(), base.accent.blend(base.background, SELECTION_COVERAGE));
            let mut t = base;
            t.accent = Rgb::new(0x9C, 0x5B, 0x3C);
            assert_ne!(t.selection(), base.selection(), "{:?}", base.scheme);
        }
    }

    #[test]
    fn the_focused_title_bar_is_the_accent_washed_over_the_face_in_both_schemes() {
        // The field's doc claims this construction; 0x2E is the dark-scheme hover coverage, 18%.
        for t in [Theme::light(), Theme::dark()] {
            assert_eq!(t.title_active, t.accent.blend(t.face, 0x2E), "{:?}", t.scheme);
            assert_eq!(t.title_inactive, t.face, "{:?}", t.scheme);
        }
    }

    /// WCAG 2's contrast ratio between two colours. Host-only: shipped code stays out of floating
    /// point (see `corner`).
    fn contrast(a: Rgb, b: Rgb) -> f64 {
        fn lum(c: Rgb) -> f64 {
            let ch = |v: u8| {
                let v = v as f64 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            };
            0.2126 * ch(c.r) + 0.7152 * ch(c.g) + 0.0722 * ch(c.b)
        }
        let (x, y) = (lum(a), lum(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn every_colour_drawn_as_text_can_be_read_in_both_schemes() {
        // WCAG's 4.5:1 for text, 7:1 for body text. It found the design's `--deny` at 2.9:1 on its
        // dark ground.
        for t in [Theme::light(), Theme::dark()] {
            let s = t.scheme;
            assert!(contrast(t.foreground, t.background) >= 7.0, "{s:?} foreground");
            assert!(contrast(t.foreground, t.selection()) >= 7.0, "{s:?} selected text");
            assert!(contrast(t.foreground, t.title_active) >= 7.0, "{s:?} a focused title");
            for (name, c) in [
                ("foreground_dim", t.foreground_dim),
                ("syntax_keyword", t.syntax_keyword),
                ("syntax_string", t.syntax_string),
                ("syntax_comment", t.syntax_comment),
                ("syntax_number", t.syntax_number),
                ("syntax_heading", t.syntax_heading),
                ("syntax_variable", t.syntax_variable),
            ] {
                let r = contrast(c, t.background);
                assert!(r >= 4.5, "{s:?} {name} is {r:.2}:1 on the background");
            }
            for (ground, g) in [("background", t.background), ("face", t.face), ("panel", t.panel)] {
                let r = contrast(t.deny, g);
                assert!(r >= 4.5, "{s:?} deny is {r:.2}:1 on {ground}");
            }
        }
    }
}
