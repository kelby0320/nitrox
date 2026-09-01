//! Every colour the desktop draws itself in, and the size it draws text at.
//!
//! **Here because this is the crate both sides link.** The toolkit and the compositor both paint
//! chrome, and they share nothing above `libdraw`: `libui` is the toolkit's and the compositor
//! deliberately does not depend on it — a compositor that linked a widget library would be a
//! compositor with opinions about widgets. `Rgb` already lives here, and so does the background
//! the screen is cleared to, so this is where a value both of them need belongs (M11 Part B).
//!
//! ## One type, not two
//!
//! `libui` carried a `Theme` (background, foreground, text size) and a `Palette` (the widget
//! colours) as separate structs, split by which function needed which. That is a distinction
//! between *call sites*, not between kinds of value — and it is the wrong seam for M11, where the
//! whole point is that these arrive together from one place. They are one struct now.
//!
//! ## What is not here
//!
//! **The terminal's ANSI palette.** `libterm::Palette` is the sixteen colours a program addresses
//! with `ESC[31m`, which is a *terminal's* vocabulary rather than a desktop's: it is defined by
//! what programs expect, not by how this system chooses to look. Folding it in would mean
//! retheming `ls` output.
//!
//! **Chrome metrics** — padding, title-bar height, the resize grip. Colour and text size move
//! (M11's decision 2); the rest stay constants, because gates click title bars at `+13` and
//! close buttons at `-39`, and a gate that had to read a theme to know where to click is a gate
//! that can disagree with the thing it is checking.

use crate::format::Rgb;

/// The colours and text size everything on screen is drawn from.
///
/// **A `const fn` constructor per theme**, which is what lets the compositor keep its cursor and
/// outline colours as `const` items while still taking them from here: `const C: Rgb =
/// Theme::dark().cursor_body;` is a constant expression. Without it the shared type would force
/// every consumer to a runtime lookup for a value that has not changed since boot.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Theme {
    // ---- surfaces ----
    /// What a damaged region is cleared to, and what the compositor clears the screen to.
    ///
    /// One value rather than two: a window's ground and the space between windows differing is
    /// visible as a seam the moment a client's buffer is smaller than the window it fills.
    pub background: Rgb,
    /// Text and other ink.
    pub foreground: Rgb,

    // ---- widgets ----
    /// A button's face at rest.
    pub face: Rgb,
    /// Its face under the pointer.
    pub face_hover: Rgb,
    /// Its face while held.
    pub face_pressed: Rgb,
    /// The ring drawn around the focused widget.
    pub focus_ring: Rgb,
    /// A scrollbar's groove, and a list's ground.
    pub track: Rgb,
    /// A scrollbar's thumb.
    pub thumb: Rgb,
    /// The background behind selected text.
    ///
    /// **A background rather than an inverted foreground**, which is what a terminal does: a
    /// terminal owns every cell's colours and can swap them, while a toolkit draws text over
    /// whatever a widget's own layers put down. Darker than [`focus_ring`](Self::focus_ring) so
    /// black text stays legible on it — the one constraint a selection colour actually has.
    pub selection: Rgb,

    // ---- window chrome ----
    /// A title bar's face while its window holds the keyboard.
    pub title_active: Rgb,
    /// A title bar's face while it does not.
    ///
    /// **Two faces rather than one**, because a title bar is the only chrome that says which
    /// window is focused. The compositor announces focus and the window list marks it, but a
    /// person looking at two overlapping windows reads it here.
    pub title_inactive: Rgb,

    // ---- what the compositor draws itself ----
    /// The pointer's fill.
    pub cursor_body: Rgb,
    /// The pointer's outline, so it stays visible against white.
    pub cursor_outline: Rgb,
    /// The rectangle a resize, a snap preview or a drop target is outlined in.
    ///
    /// **One colour for all three**, which is a decision deferred rather than taken: what a drop
    /// target should look like as distinct from a resize is a question for the polish passes, and
    /// a second colour chosen here would be a guess made before anything had been looked at.
    pub outline: Rgb,

    // ---- type ----
    /// Text size, in pixels per em.
    pub font_px: f32,
    /// The font every label, button and list row is drawn with.
    ///
    /// **Proportional, and a different file from [`font_mono`](Self::font_mono)** — M11's
    /// decision 3. A desktop whose menus are monospaced is a desktop that looks like a terminal,
    /// and until Part D every window in this system was: `SYSTEM_FONT_PATH` was one constant and
    /// every client loaded it.
    pub font_ui: FontPath,
    /// The font a character grid is drawn with.
    ///
    /// **Separate because a grid needs a fixed advance**, which is a property of the file rather
    /// than a setting: `libterm` takes its cell width from one glyph's advance, so a proportional
    /// font here does not make a terminal with narrow columns, it makes a terminal whose columns
    /// are wrong. A theme may name a different mono face; naming a proportional one is a theme
    /// breaking its own terminal, which is why the two are separate keys and not one.
    pub font_mono: FontPath,
}

impl Theme {
    /// The dark theme — the only one that ships.
    ///
    /// **Named rather than anonymous**, which is decision 4 of M11's details pass made visible in
    /// the code: every value is chosen once and `check-display` keeps one reference, while the
    /// *mechanism* can hold a second theme the day somebody wants one. A `Theme::light()` beside
    /// this would be a constructor, not a redesign.
    pub const fn dark() -> Self {
        Self {
            background: Rgb::new(0x0E, 0x14, 0x1B),
            foreground: Rgb::new(0xE0, 0xE6, 0xEC),

            face: Rgb::new(0x24, 0x2C, 0x36),
            face_hover: Rgb::new(0x30, 0x3A, 0x46),
            face_pressed: Rgb::new(0x18, 0x1E, 0x26),
            focus_ring: Rgb::new(0x5A, 0x9F, 0xD4),
            track: Rgb::new(0x18, 0x1E, 0x26),
            thumb: Rgb::new(0x3A, 0x46, 0x54),
            selection: Rgb::new(0x2A, 0x4A, 0x6A),

            title_active: Rgb::new(0x2E, 0x3A, 0x4A),
            title_inactive: Rgb::new(0x1C, 0x22, 0x2A),

            cursor_body: Rgb::new(0xFF, 0xFF, 0xFF),
            cursor_outline: Rgb::new(0x00, 0x00, 0x00),
            outline: Rgb::new(0xE0, 0xE0, 0xE0),

            font_px: 16.0,
            font_ui: FontPath::new(crate::text::UI_FONT_PATH),
            font_mono: FontPath::new(crate::text::MONO_FONT_PATH),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// What was wrong with one line of a theme file.
///
/// **Reported rather than fatal.** A theme is decoration: a file with a typo in it must still
/// produce a usable desktop, or a person editing colours can lock themselves out of the machine
/// they were editing them on. Every issue here leaves the field at its default and carries the
/// line number so the shell can say which line to look at.
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
    /// A key this version does not know.
    ///
    /// **Not an error, and kept as an issue anyway**: forward compatibility means an older
    /// system must read a newer file, so an unknown key is skipped — but a *misspelled* key is
    /// indistinguishable from a future one, and silence is what makes a typo take an afternoon.
    UnknownKey,
    /// A key this version knows, with a value it cannot read.
    BadValue,
}

impl Theme {
    /// Read a theme from `key = "value"` lines, starting from [`dark`](Self::dark).
    ///
    /// **A focused reader, not a TOML parser**, in the house style: `init`'s `toml_lite` handles
    /// table arrays and one-level subtables, `service-mgr`'s `service_toml` tracks two-level
    /// sections, and this one takes flat `key = value` and nothing else. Each is written for the
    /// schema it reads and says how it differs from the others. What it accepts *is* valid TOML —
    /// basic strings and floats at the top level — so the file is a TOML file, and a reader that
    /// grew tables would be reading a different schema than this one has.
    ///
    /// **Missing keys keep their defaults and unknown keys are skipped**, which is the same
    /// forward-compatibility rule `service-toml-schema.md` states: a file written by a newer
    /// system must still start an older one. A file that is empty, absent, or entirely comments
    /// is therefore exactly [`dark`](Self::dark).
    ///
    /// Colours are `"#RRGGBB"`; `font_px` is a decimal number. Comments run from `#` to the end
    /// of a line — except inside the quotes of a value, which is the whole reason this is a
    /// parser rather than a `split('#')`.
    pub fn from_config(text: &str) -> (Self, alloc::vec::Vec<Issue>) {
        let mut t = Self::dark();
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
            // **Both forms are kept**, because one key cares which it was: a colour is a TOML
            // string and `font_px` is a TOML number, so the quotes are part of the *type* rather
            // than punctuation to strip on the way past.
            let raw = value.trim();
            let Some(value) = unquote(raw) else {
                issues.push(Issue { line: n + 1, kind: IssueKind::BadValue });
                continue;
            };
            let ok = match key {
                "background" => set(&mut t.background, value),
                "foreground" => set(&mut t.foreground, value),
                "face" => set(&mut t.face, value),
                "face_hover" => set(&mut t.face_hover, value),
                "face_pressed" => set(&mut t.face_pressed, value),
                "focus_ring" => set(&mut t.focus_ring, value),
                "track" => set(&mut t.track, value),
                "thumb" => set(&mut t.thumb, value),
                "selection" => set(&mut t.selection, value),
                "title_active" => set(&mut t.title_active, value),
                "title_inactive" => set(&mut t.title_inactive, value),
                "cursor_body" => set(&mut t.cursor_body, value),
                "cursor_outline" => set(&mut t.cursor_outline, value),
                "outline" => set(&mut t.outline, value),
                // **Unquoted, because TOML types a quoted number as a string.** Accepting
                // `font_px = "14"` would be accepting a file a real TOML reader disagrees with
                // this one about.
                "font_px" if raw.starts_with('"') => false,
                "font_px" => match value.parse::<f32>() {
                    // **A size, not a number**, and bounded at both ends by what can be read:
                    // zero divides in the layout, and anything above what the fixed chrome holds
                    // is clipped by `paint` and overlapped by its neighbours. See
                    // [`MAX_FONT_PX`], which is 16 because a list row is 20 with 4 of padding.
                    Ok(v) if (MIN_FONT_PX..=MAX_FONT_PX).contains(&v) => {
                        // **To the nearest hundredth of a pixel**, which is a precision decision
                        // rather than a rounding accident. A size is reported to a console by
                        // whatever draws with it, and `check-terminal` recomputes a cell from
                        // that number — so a value the line cannot print exactly is a value the
                        // host and the guest can disagree about, and the gate would report a
                        // wrong font (PR #264 review, finding 2). A hundredth of a pixel is far
                        // below anything a rasteriser resolves; what it buys is that "the size
                        // printed is the size used" is true by construction.
                        t.font_px = round_px(v);
                        true
                    }
                    _ => false,
                },
                // **Quoted, for the mirror of the reason `font_px` must not be.** A path is a
                // TOML string, so a bare one is a file this reader and a real TOML reader
                // disagree about — and `unquote` accepts a bare value, which is what makes the
                // check explicit here rather than implied.
                "font_ui" | "font_mono" if !raw.starts_with('"') => false,
                "font_ui" => set_path(&mut t.font_ui, value),
                "font_mono" => set_path(&mut t.font_mono, value),
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

    /// Write the theme back in the form [`from_config`](Self::from_config) reads.
    ///
    /// **Every field, always**, which is what makes this safe to hand to another process: a
    /// reader of the result never falls back to a default, because there is nothing missing to
    /// fall back for. That is the difference between the file (a person's, partial, forgiving)
    /// and the wire (a shell's, complete, already validated).
    pub fn to_config(&self) -> alloc::string::String {
        use core::fmt::Write as _;
        // **Destructured so the compiler enforces completeness.** A field-count assertion catches
        // a line going missing and not a *field* being added — add one, forget it here, and every
        // client silently falls back to a default for it, which is the "never receives a partial
        // theme" property quietly gone (PR #263 review, optional 1). Adding a field to `Theme`
        // now fails to compile until it is written out.
        let Theme {
            background,
            foreground,
            face,
            face_hover,
            face_pressed,
            focus_ring,
            track,
            thumb,
            selection,
            title_active,
            title_inactive,
            cursor_body,
            cursor_outline,
            outline,
            font_px,
            font_ui,
            font_mono,
        } = *self;
        let mut s = alloc::string::String::new();
        for (k, c) in [
            ("background", background),
            ("foreground", foreground),
            ("face", face),
            ("face_hover", face_hover),
            ("face_pressed", face_pressed),
            ("focus_ring", focus_ring),
            ("track", track),
            ("thumb", thumb),
            ("selection", selection),
            ("title_active", title_active),
            ("title_inactive", title_inactive),
            ("cursor_body", cursor_body),
            ("cursor_outline", cursor_outline),
            ("outline", outline),
        ] {
            let _ = writeln!(s, "{k} = \"#{:02X}{:02X}{:02X}\"", c.r, c.g, c.b);
        }
        let _ = writeln!(s, "font_px = {font_px}");
        // Quoted, because that is the form `from_config` accepts — a writer whose output its own
        // reader refuses is a round trip that only works by accident.
        let _ = writeln!(s, "font_ui = \"{}\"", font_ui.as_str());
        let _ = writeln!(s, "font_mono = \"{}\"", font_mono.as_str());
        s
    }
}

/// The path to a font file, bounded so a [`Theme`] stays `Copy` and `const`-constructible.
///
/// **A fixed-capacity path rather than a `String`**, and the reason is the same one that made
/// `Theme::dark()` a `const fn`: the compositor keeps theme colours as `const` items, and a heap
/// allocation cannot appear in a constant. The bound is not a limitation reluctantly accepted
/// either — this value travels on the setup record, which is one 4 KiB IPC message for *all* of
/// argv and the environment, so a path a person could make arbitrarily long is a theme file that
/// could stop applications from launching.
///
/// A path is absolute, non-empty, at most [`MAX_FONT_PATH`] bytes, and free of control
/// characters, `"` and `\` — the control bytes because it is logged when it fails to load and a
/// font path is one of the few pieces of a theme file that reaches a console, the other two
/// because the value is written back out as a TOML basic string. A path holding a quote would
/// round-trip through *this* reader and mean something else to a real one, which is the same
/// argument the unquoted-`font_px` rule rests on, applied to the other end of the string
/// (PR #264 review, optional 4).
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct FontPath {
    /// Zero-filled past `len`, so the derived equality compares paths rather than the debris of
    /// whatever longer path a slot held before.
    bytes: [u8; MAX_FONT_PATH],
    len: u8,
}

impl FontPath {
    /// A path known at compile time.
    ///
    /// **Panics on a path this type cannot hold**, which is what makes it usable in a `const`:
    /// the built-in theme's two paths are checked when the crate compiles, and a bad one is a
    /// build error rather than a desktop with no text. Call it on literals. Everything reading a
    /// file calls [`parse`](Self::parse), which answers instead of panicking.
    pub const fn new(s: &str) -> Self {
        let b = s.as_bytes();
        assert!(usable(b), "a font path must be absolute, printable, and fit MAX_FONT_PATH");
        let mut bytes = [0u8; MAX_FONT_PATH];
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
        // Valid UTF-8 by construction: every constructor takes a `&str` and copies its bytes
        // whole. The fallback exists so a graphics path has no panic in it at all.
        core::str::from_utf8(&self.bytes[..self.len as usize]).unwrap_or("")
    }
}

impl core::fmt::Debug for FontPath {
    /// The path, not 64 bytes of mostly zeroes — a `Theme` is printed by a failing test.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Debug::fmt(self.as_str(), f)
    }
}

/// Whether `b` is a path [`FontPath`] can hold.
const fn usable(b: &[u8]) -> bool {
    if b.is_empty() || b.len() > MAX_FONT_PATH || b[0] != b'/' {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        // Control bytes, and the two characters a TOML basic string cannot carry unescaped.
        // Everything else is allowed, including the high halves of UTF-8: the input is a `&str`,
        // and a font whose name is not ASCII is a font all the same.
        if b[i] < 0x20 || b[i] == 0x7F || b[i] == b'"' || b[i] == b'\\' {
            return false;
        }
        i += 1;
    }
    true
}

// `len` is a `u8`, and it is only wide enough to hold what `usable` admits while the bound below
// fits in one. Raising it past 255 would truncate silently and `as_str` would hand back a short
// path with nothing anywhere reporting it (PR #264 review, optional 3).
const _: () = assert!(MAX_FONT_PATH <= u8::MAX as usize);

/// The longest font path a theme can name.
///
/// Enough for `/system/fonts/` plus a long family name, and small enough that two of them on the
/// setup record are noise beside the 4 KiB it holds.
pub const MAX_FONT_PATH: usize = 64;

/// A size rounded to the precision [`px_parts`] can print without loss.
fn round_px(v: f32) -> f32 {
    libm::roundf(v * 100.0) / 100.0
}

/// `px` as whole pixels and hundredths, for a console line that has to be exact.
///
/// **Because a truncated size is a wrong answer somewhere else.** `nxterm` prints the size it
/// measured its grid at and `check-terminal` re-measures the same font at that number on the
/// host; printing `13` for `13.5` makes the two disagree by a pixel and the gate blames the font.
/// Every size the system can hold is exact to a hundredth — [`Theme::from_config`] rounds there —
/// so these two integers are the whole value.
pub fn px_parts(px: f32) -> (u64, u64) {
    let px = round_px(px).max(0.0);
    let whole = px as u64;
    (whole, libm::roundf((px - whole as f32) * 100.0) as u64)
}

/// The smallest text this system will render at, in pixels per em.
///
/// Below this the glyph rasteriser produces shapes nobody can read, and a theme that could set it
/// is a theme that can make the machine unusable from a text file.
pub const MIN_FONT_PX: f32 = 6.0;

/// The largest, and it is **not a taste judgement — it is what the chrome holds**.
///
/// `text_size().h` is exactly the em size, and the tightest fixed box in the system is a list
/// row: `ROW_H` is 20 pixels with `ROW_PAD` taking 2 above and 2 below, leaving 16. The window
/// bars are 24 with 4+4 of button padding, which lands on the same number. That is why the
/// system's text has always been 16 and not a coincidence anybody chose.
///
/// **So this knob shrinks and does not grow**, which is the honest consequence of M11's decision
/// 2: colour and type are themeable, chrome metrics are not. Text larger than its box is clipped
/// by `paint`, and rows keep their spacing, so glyphs overlap — a theme file that could ask for
/// that is a theme file that can make the desktop unreadable, which is the same argument the
/// lower bound rests on (PR #263 review, blocking 1).
///
/// **Trigger for raising it: metrics that follow type.** `ROW_H`, `BAR_H` and `TITLE_BAR_H`
/// derived from `font_px` would let it grow — and would mean the gates computing their click
/// points from a theme, which is exactly what decision 2 declined. It is a decision, not an
/// oversight, and it belongs to whoever revisits that one.
pub const MAX_FONT_PX: f32 = 16.0;

/// A basic string's contents, or a bare value unchanged — `None` for a half-quoted one.
///
/// **Because `trim_matches('"')` accepts what TOML does not.** `"#102030` (one quote) and
/// `#102030"` both parsed before, which made the doc's claim that every accepted file is valid
/// TOML false in a way nobody would notice until a real parser read the same file back
/// (PR #263 review, optional 3).
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
fn set_path(slot: &mut FontPath, value: &str) -> bool {
    match FontPath::parse(value) {
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
    fn the_default_is_the_dark_theme_and_is_available_in_a_const() {
        // The `const fn` is the whole reason the compositor can take its cursor colour from here
        // and still declare it as a `const` — a runtime default would have forced every consumer
        // of a value that never changes into a lookup.
        const CURSOR: Rgb = Theme::dark().cursor_body;
        assert_eq!(CURSOR, Rgb::new(0xFF, 0xFF, 0xFF));
        assert_eq!(Theme::default(), Theme::dark());
    }

    #[test]
    fn a_file_overrides_what_it_names_and_nothing_else() {
        let (t, issues) = Theme::from_config(
            "# my theme\nbackground = \"#102030\"\n\nfont_px = 13.5   # smaller\n",
        );
        assert_eq!(t.background, Rgb::new(0x10, 0x20, 0x30));
        assert_eq!(t.font_px, 13.5);
        assert_eq!(t.face, Theme::dark().face, "a key the file did not name keeps its default");
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn an_absent_or_empty_file_is_exactly_the_default() {
        // **The part's own control, at the value level.** A desktop with no theme file must be
        // the desktop that shipped — not a black screen, not a partial theme.
        for text in ["", "\n\n", "# nothing but a comment\n"] {
            let (t, issues) = Theme::from_config(text);
            assert_eq!(t, Theme::dark(), "{text:?}");
            assert!(issues.is_empty());
        }
    }

    #[test]
    fn a_bad_line_costs_its_own_field_and_no_others() {
        // **A theme is decoration, and a typo in it must not cost the desktop.** Somebody editing
        // colours on the machine they are editing them on cannot be locked out by a stray
        // character — so every failure below leaves that one field at its default and the rest of
        // the file is still read.
        let (t, issues) = Theme::from_config(
            "background = \"#zzzzzz\"\n             face\n             frobnicate = \"#112233\"\n             font_px = 0\n             foreground = \"#010203\"\n",
        );
        assert_eq!(t.background, Theme::dark().background, "a bad colour keeps the default");
        assert_eq!(t.font_px, Theme::dark().font_px, "and a size outside the readable range");
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
        // **The doc claims every file this accepts is also valid TOML and means the same thing**,
        // and `trim_matches('"')` made that false in two ways nobody would notice until a real
        // parser read the file back (PR #263 review, optional 3).
        for bad in [
            "background = \"#102030",   // one quote
            "background = #102030\"",   // the other
            "font_px = \"14\"",         // TOML types this as a string, not a number
        ] {
            let (t, issues) = Theme::from_config(bad);
            assert_eq!(t, Theme::dark(), "{bad:?} changed something");
            assert_eq!(issues.len(), 1, "{bad:?}");
            assert_eq!(issues[0].kind, IssueKind::BadValue, "{bad:?}");
        }
        // Both forms TOML *does* accept still work: a quoted string and a bare number.
        let (t, issues) = Theme::from_config("background = \"#102030\"\nfont_px = 14\n");
        assert_eq!(t.background, Rgb::new(0x10, 0x20, 0x30));
        assert_eq!(t.font_px, 14.0);
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn a_font_size_outside_the_readable_range_is_refused() {
        // Zero divides in the layout; a huge one puts a single glyph in a window. Both are a
        // theme file that renders nothing usable, which is the state this must not reach.
        for bad in ["0", "-4", "0.5", "17", "1000", "nan"] {
            let (t, issues) = Theme::from_config(&alloc::format!("font_px = {bad}"));
            assert_eq!(t.font_px, Theme::dark().font_px, "font_px = {bad}");
            assert_eq!(issues.len(), 1, "font_px = {bad}");
        }
        for good in ["6", "10", "13.5", "16"] {
            let (_, issues) = Theme::from_config(&alloc::format!("font_px = {good}"));
            assert!(issues.is_empty(), "font_px = {good} should be accepted");
        }
    }

    #[test]
    fn a_hash_inside_a_value_is_not_a_comment() {
        // The whole reason this is a parser rather than `split('#')`: every colour begins with
        // the comment character.
        let (t, issues) = Theme::from_config("focus_ring = \"#ABCDEF\" # the ring\n");
        assert_eq!(t.focus_ring, Rgb::new(0xAB, 0xCD, 0xEF));
        assert!(issues.is_empty(), "{issues:?}");
    }

    #[test]
    fn what_is_written_is_complete_and_reads_back_as_itself() {
        // **Complete, which is what makes it safe to hand to another process**: a reader of this
        // never falls back to a default, because nothing is missing to fall back for. The round
        // trip is the cheap half; the field count is the half that matters.
        let mut t = Theme::dark();
        t.background = Rgb::new(0x01, 0x02, 0x03);
        t.selection = Rgb::new(0xFE, 0xDC, 0xBA);
        t.font_px = 13.0;
        t.font_ui = FontPath::new("/home/Fancy.ttf");

        let text = t.to_config();
        assert_eq!(text.lines().count(), 17, "fourteen colours, a size and two fonts");
        let (back, issues) = Theme::from_config(&text);
        assert_eq!(back, t);
        assert!(issues.is_empty(), "{issues:?}");

        // And it says so about a theme it did *not* come from: reading this on top of a
        // different starting point still lands on `t`, because every field is named.
        let (over, _) = Theme::from_config(&text);
        assert_eq!(over, t, "every field is present, so nothing is inherited");
    }

    #[test]
    fn a_theme_names_two_fonts_and_the_defaults_are_where_the_image_stages_them() {
        // The built-in paths are the image's, not a second opinion about it: `xtask` stages
        // exactly these two names into `/system/fonts`, and its own test asserts that.
        let t = Theme::dark();
        assert_eq!(t.font_ui.as_str(), crate::text::UI_FONT_PATH);
        assert_eq!(t.font_mono.as_str(), crate::text::MONO_FONT_PATH);
        // **And they are two different files.** The whole of Part D is that the desktop stopped
        // drawing itself in a terminal's font; a theme whose two roles named one path would be
        // the state before it, expressed in two fields.
        assert_ne!(t.font_ui, t.font_mono);
    }

    #[test]
    fn a_font_path_is_absolute_bounded_and_free_of_control_bytes() {
        assert_eq!(FontPath::parse("/system/fonts/DejaVuSans.ttf").map(|p| p.as_str().len()), Some(28));
        // A name that is not ASCII is a name all the same — the input is a `&str`.
        assert!(FontPath::parse("/home/\u{c9}criture.ttf").is_some());

        assert_eq!(FontPath::parse(""), None, "empty");
        assert_eq!(FontPath::parse("DejaVuSans.ttf"), None, "relative");
        assert_eq!(FontPath::parse("/home/a\nb.ttf"), None, "a path with a newline is not a path");
        // **A quote and a backslash, because the value is written back as a TOML basic string.**
        // `/home/a"b.ttf` would round-trip through this reader and read as something else in any
        // other TOML parser, which is the claim the schema makes about every file it accepts.
        assert_eq!(FontPath::parse("/home/a\"b.ttf"), None, "a quote would escape the string");
        assert_eq!(FontPath::parse("/home/a\\b.ttf"), None, "a backslash would be an escape");
        let long = alloc::format!("/home/{}.ttf", "x".repeat(MAX_FONT_PATH));
        assert_eq!(FontPath::parse(&long), None, "longer than the record can carry");
        // The bound is inclusive, and the test says which side of it: exactly MAX is fine.
        let exact = alloc::format!("/{}", "x".repeat(MAX_FONT_PATH - 1));
        assert_eq!(FontPath::parse(&exact).map(|p| p.as_str().len()), Some(MAX_FONT_PATH));
    }

    #[test]
    fn a_font_path_compares_as_a_path_and_not_as_its_buffer() {
        // **What this actually pins is `len`**, and the comment used to claim more. `FontPath`
        // derives `PartialEq` over a 64-byte array, so the zero fill is what makes that equality
        // mean "the same path" — but no constructor here can leave a previous path's tail
        // behind, because both start from a fresh array, so nothing in this crate can produce
        // the case the fill defends against. Filling with `0xFF` instead leaves every test in
        // this file green (PR #264 review, optional 2). The half that does bite is below: stop
        // slicing at `len` in `as_str` and five tests fail, this one included.
        let a = FontPath::new("/system/fonts/DejaVuSans.ttf");
        let b = FontPath::parse("/system/fonts/DejaVuSans.ttf").expect("a usable path");
        assert_eq!(a, b);
        assert_ne!(a, FontPath::new("/system/fonts/DejaVuSansMono.ttf"));
        assert_eq!(alloc::format!("{a:?}"), "\"/system/fonts/DejaVuSans.ttf\"");
    }

    #[test]
    fn a_font_key_takes_a_quoted_path_and_refuses_what_toml_reads_differently() {
        let (t, issues) = Theme::from_config("font_ui = \"/home/Fancy.ttf\"\n");
        assert_eq!(t.font_ui.as_str(), "/home/Fancy.ttf");
        assert_eq!(t.font_mono, Theme::dark().font_mono, "the other role kept its default");
        assert!(issues.is_empty(), "{issues:?}");

        // **Bare is refused**, the mirror of `font_px` refusing quotes: a path is a TOML string,
        // and a file this reader accepts must be a file a TOML reader agrees with.
        for bad in [
            "font_ui = /home/Fancy.ttf\n",
            "font_ui = \"Fancy.ttf\"\n",
            "font_mono = \"\"\n",
        ] {
            let (t, issues) = Theme::from_config(bad);
            assert_eq!(t, Theme::dark(), "{bad:?} left the theme alone");
            assert_eq!(
                issues,
                [Issue { line: 1, kind: IssueKind::BadValue }],
                "{bad:?} was named as a bad value"
            );
        }
    }

    #[test]
    fn a_size_is_read_to_a_hundredth_and_prints_back_as_itself() {
        // **The property `check-terminal` rests on**: whatever draws with a size reports it to a
        // console as two integers, and the host re-measures the font at that number. So every
        // size the system can hold must survive the trip — `13.5` printed as `13` is a cell a
        // pixel short and a gate blaming the font (PR #264 review, finding 2).
        for (text, want, parts) in [
            ("font_px = 13.5\n", 13.5, (13, 50)),
            ("font_px = 13.05\n", 13.05, (13, 5)),
            ("font_px = 16\n", 16.0, (16, 0)),
            // Beyond a hundredth the file is rounded rather than kept, which is what makes the
            // two integers the whole value instead of most of it.
            ("font_px = 13.333\n", 13.33, (13, 33)),
        ] {
            let (t, issues) = Theme::from_config(text);
            assert!(issues.is_empty(), "{text:?} {issues:?}");
            assert_eq!(t.font_px, want, "{text:?}");
            assert_eq!(px_parts(t.font_px), parts, "{text:?}");
            // And the two integers reassemble into the size that was used, exactly — the step
            // the gate performs on the other side of the serial line.
            let (whole, cents) = parts;
            assert_eq!(whole as f32 + cents as f32 / 100.0, t.font_px, "{text:?}");
        }
    }

    #[test]
    fn the_scenes_ground_is_still_the_themes_ground() {
        // **A provenance guard, not a pixel guard**, and the difference is worth stating because
        // the obvious reading is wrong: `BACKGROUND` is *derived* from this field, so retuning
        // the field moves both and leaves this green. What catches a moved pixel is
        // `scene::REFERENCE_HASH`, which fails on exactly that change.
        //
        // What this fails on is the re-divergence: somebody writing `BACKGROUND` out as a
        // literal again, which is how the two came to need an equality test in the first place.
        let t = Theme::dark();
        assert_eq!(t.background, crate::scene::BACKGROUND);
    }
}
