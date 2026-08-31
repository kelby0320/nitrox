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
    fn a_window_and_the_space_between_windows_are_the_same_ground() {
        // A seam appears the moment these differ and a client's committed buffer is smaller than
        // the window it fills — which is every window during a resize.
        let t = Theme::dark();
        assert_eq!(t.background, crate::scene::BACKGROUND);
    }
}
