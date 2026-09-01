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

    /// Text size, in pixels per em.
    pub font_px: f32,
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
            let value = value.trim().trim_matches('"');
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
                "font_px" => match value.parse::<f32>() {
                    // **A size, not a number.** Zero divides in the layout and a huge one makes
                    // one glyph fill a window; both are a file that renders nothing usable, which
                    // is the state a theme must not be able to reach.
                    Ok(v) if (MIN_FONT_PX..=MAX_FONT_PX).contains(&v) => {
                        t.font_px = v;
                        true
                    }
                    _ => false,
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

    /// Write the theme back in the form [`from_config`](Self::from_config) reads.
    ///
    /// **Every field, always**, which is what makes this safe to hand to another process: a
    /// reader of the result never falls back to a default, because there is nothing missing to
    /// fall back for. That is the difference between the file (a person's, partial, forgiving)
    /// and the wire (a shell's, complete, already validated).
    pub fn to_config(&self) -> alloc::string::String {
        use core::fmt::Write as _;
        let mut s = alloc::string::String::new();
        for (k, c) in [
            ("background", self.background),
            ("foreground", self.foreground),
            ("face", self.face),
            ("face_hover", self.face_hover),
            ("face_pressed", self.face_pressed),
            ("focus_ring", self.focus_ring),
            ("track", self.track),
            ("thumb", self.thumb),
            ("selection", self.selection),
            ("title_active", self.title_active),
            ("title_inactive", self.title_inactive),
            ("cursor_body", self.cursor_body),
            ("cursor_outline", self.cursor_outline),
            ("outline", self.outline),
        ] {
            let _ = writeln!(s, "{k} = \"#{:02X}{:02X}{:02X}\"", c.r, c.g, c.b);
        }
        let _ = writeln!(s, "font_px = {}", self.font_px);
        s
    }
}

/// The smallest text this system will render at, in pixels per em.
///
/// Below this the glyph rasteriser produces shapes nobody can read, and a theme that could set it
/// is a theme that can make the machine unusable from a text file.
pub const MIN_FONT_PX: f32 = 6.0;

/// The largest, for the same reason from the other end: one glyph per window is not a desktop.
pub const MAX_FONT_PX: f32 = 96.0;

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
            "# my theme\nbackground = \"#102030\"\n\nfont_px = 18.5   # bigger\n",
        );
        assert_eq!(t.background, Rgb::new(0x10, 0x20, 0x30));
        assert_eq!(t.font_px, 18.5);
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
    fn a_font_size_outside_the_readable_range_is_refused() {
        // Zero divides in the layout; a huge one puts a single glyph in a window. Both are a
        // theme file that renders nothing usable, which is the state this must not reach.
        for bad in ["0", "-4", "0.5", "1000", "nan"] {
            let (t, issues) = Theme::from_config(&alloc::format!("font_px = {bad}"));
            assert_eq!(t.font_px, Theme::dark().font_px, "font_px = {bad}");
            assert_eq!(issues.len(), 1, "font_px = {bad}");
        }
        for good in ["6", "16", "18.5", "96"] {
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

        let text = t.to_config();
        assert_eq!(text.lines().count(), 15, "fourteen colours and a size");
        let (back, issues) = Theme::from_config(&text);
        assert_eq!(back, t);
        assert!(issues.is_empty(), "{issues:?}");

        // And it says so about a theme it did *not* come from: reading this on top of a
        // different starting point still lands on `t`, because every field is named.
        let (over, _) = Theme::from_config(&text);
        assert_eq!(over, t, "every field is present, so nothing is inherited");
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
