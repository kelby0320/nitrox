//! What a grid position holds: a character, and how it is drawn.
//!
//! This is the vocabulary the parser writes and the render reads, so it lands before either.

use libdraw::format::Rgb;

/// One of the sixteen ANSI colours.
///
/// **Named exhaustively rather than held as an index.** A `u8` would admit 240 values the
/// system does not have, and every consumer would then need a rule for them; sixteen variants
/// make the supported set the type. 256-colour and truecolour become *variants* of [`Colour`]
/// when something emits them — which is what the plan means by "a match arm rather than a
/// reshape" — and neither is in Milestone 5, because nothing does.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Ansi {
    /// SGR 30 / 40.
    Black,
    /// SGR 31 / 41.
    Red,
    /// SGR 32 / 42.
    Green,
    /// SGR 33 / 43.
    Yellow,
    /// SGR 34 / 44.
    Blue,
    /// SGR 35 / 45.
    Magenta,
    /// SGR 36 / 46.
    Cyan,
    /// SGR 37 / 47.
    White,
    /// SGR 90 / 100.
    BrightBlack,
    /// SGR 91 / 101.
    BrightRed,
    /// SGR 92 / 102.
    BrightGreen,
    /// SGR 93 / 103.
    BrightYellow,
    /// SGR 94 / 104.
    BrightBlue,
    /// SGR 95 / 105.
    BrightMagenta,
    /// SGR 96 / 106.
    BrightCyan,
    /// SGR 97 / 107.
    BrightWhite,
}

impl Ansi {
    /// Every colour, in palette order — index `n` is SGR `30 + n` for `n < 8` and
    /// `90 + n - 8` above it.
    pub const ALL: [Ansi; 16] = [
        Ansi::Black,
        Ansi::Red,
        Ansi::Green,
        Ansi::Yellow,
        Ansi::Blue,
        Ansi::Magenta,
        Ansi::Cyan,
        Ansi::White,
        Ansi::BrightBlack,
        Ansi::BrightRed,
        Ansi::BrightGreen,
        Ansi::BrightYellow,
        Ansi::BrightBlue,
        Ansi::BrightMagenta,
        Ansi::BrightCyan,
        Ansi::BrightWhite,
    ];

    /// This colour's position in [`Palette`].
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The bright counterpart, or `self` if it is already bright.
    ///
    /// What `SGR 1` uses. See [`Attributes::resolve`] for why bold brightens here.
    pub const fn brightened(self) -> Ansi {
        let i = self.index();
        if i >= 8 { self } else { Ansi::ALL[i + 8] }
    }
}

/// A cell's foreground or background colour.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Colour {
    /// The theme's default — SGR 39 for a foreground, SGR 49 for a background.
    ///
    /// **Not a synonym for white-on-black.** A default is whatever the theme says, so it must
    /// survive into the cell rather than being resolved at parse time: a stored colour would
    /// freeze the theme into the scrollback, and re-theming would recolour new text only.
    #[default]
    Default,
    /// One of the sixteen.
    Ansi(Ansi),
}

/// The sixteen colours, as pixels.
///
/// Separate from the cells that name them, so the same scrollback renders under a different
/// theme without being rewritten.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Palette {
    /// Indexed by [`Ansi::index`].
    pub ansi: [Rgb; 16],
    /// What [`Colour::Default`] means in the foreground.
    pub foreground: Rgb,
    /// What [`Colour::Default`] means in the background.
    pub background: Rgb,
}

impl Default for Palette {
    /// A conventional dark palette.
    ///
    /// **The sixteen are the terminal's; the two defaults are the desktop's.** `ansi` is what a
    /// program addresses with `ESC[31m` — a vocabulary defined by what programs expect, which is
    /// why retheming a desktop must not retheme `ls` output. `foreground` and `background` are
    /// what `Colour::Default` means, and a terminal whose ground differed from the chrome around
    /// it would flash against it — so those two are **read from the shared theme** rather than
    /// written out again (M11 Part B).
    ///
    /// **That replaces an assertion with a fact.** From A5 until Part B this equality was a pair
    /// of literals in two crates, enforced by a host test in `xtask` — the one place that links
    /// `libterm` and `libui`, chosen because they are siblings and "putting a theme colour in
    /// `libdraw` would make the pixel layer own a theme". Part B did precisely that, on purpose:
    /// the compositor paints chrome too and links no toolkit, so `libdraw` is where a value all
    /// three need belongs. One source, so the test that compared two of them has nothing left
    /// to compare (PR #189 review, finding 6; PR #190 review, finding 2).
    fn default() -> Self {
        Self {
            ansi: [
                Rgb::new(0x1C, 0x22, 0x2A), // black
                Rgb::new(0xC4, 0x3B, 0x3B), // red
                Rgb::new(0x4E, 0xA8, 0x4E), // green
                Rgb::new(0xC0, 0x94, 0x2E), // yellow
                Rgb::new(0x41, 0x7C, 0xC4), // blue
                Rgb::new(0xA0, 0x55, 0xB8), // magenta
                Rgb::new(0x3E, 0xA6, 0xA6), // cyan
                Rgb::new(0xC8, 0xCE, 0xD4), // white
                Rgb::new(0x44, 0x4E, 0x5A), // bright black
                Rgb::new(0xE6, 0x5C, 0x5C), // bright red
                Rgb::new(0x74, 0xCE, 0x74), // bright green
                Rgb::new(0xE6, 0xB8, 0x50), // bright yellow
                Rgb::new(0x6A, 0xA2, 0xE6), // bright blue
                Rgb::new(0xC2, 0x7C, 0xDA), // bright magenta
                Rgb::new(0x60, 0xC8, 0xC8), // bright cyan
                Rgb::new(0xEC, 0xF0, 0xF4), // bright white
            ],
            foreground: libdraw::theme::Theme::dark().foreground,
            background: libdraw::theme::Theme::dark().background,
        }
    }
}

impl Palette {
    /// The pixel colour of `c` used as a foreground.
    pub fn foreground_of(&self, c: Colour) -> Rgb {
        match c {
            Colour::Default => self.foreground,
            Colour::Ansi(a) => self.ansi[a.index()],
        }
    }

    /// The pixel colour of `c` used as a background.
    pub fn background_of(&self, c: Colour) -> Rgb {
        match c {
            Colour::Default => self.background,
            Colour::Ansi(a) => self.ansi[a.index()],
        }
    }
}

/// The on/off attributes a cell carries, as a bit set.
///
/// A newtype over `u8` rather than four `bool`s: a cell is stored per grid position and there
/// are 80 × 24 of them before scrollback, so the packing is the difference between an
/// attribute set that costs one byte and one that costs four.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Flags(u8);

impl Flags {
    /// No attributes — what `SGR 0` resets to.
    pub const NONE: Flags = Flags(0);
    /// `SGR 1`. With one font weight this brightens the foreground; see [`Attributes::resolve`].
    pub const BOLD: Flags = Flags(1 << 0);
    /// `SGR 4`.
    pub const UNDERLINE: Flags = Flags(1 << 1);
    /// `SGR 7` — foreground and background exchanged.
    pub const REVERSE: Flags = Flags(1 << 2);

    /// Whether every bit of `f` is set.
    pub const fn contains(self, f: Flags) -> bool {
        self.0 & f.0 == f.0
    }

    /// `self` with `f`'s bits set.
    pub const fn with(self, f: Flags) -> Flags {
        Flags(self.0 | f.0)
    }

    /// `self` with `f`'s bits cleared.
    pub const fn without(self, f: Flags) -> Flags {
        Flags(self.0 & !f.0)
    }
}

/// How a cell is drawn: two colours and a set of flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Attributes {
    /// Ink.
    pub fg: Colour,
    /// Behind it.
    pub bg: Colour,
    /// Bold, underline, reverse.
    pub flags: Flags,
}

impl Attributes {
    /// The pixel colours this cell is drawn in: `(foreground, background)`.
    ///
    /// Two rules, in this order, and the order is the whole of the function:
    ///
    /// 1. **Bold brightens the foreground**, and only an [`Ansi`] one — `Colour::Default`
    ///    stays the theme's default. This is what xterm and VTE do *when no bold face is
    ///    available*, which is our situation: `libdraw` has one weight, so a bold that changed
    ///    nothing would make `SGR 1` invisible, and shells emit it constantly. A real bold face
    ///    supersedes this rather than joining it.
    /// 2. **Reverse swaps what results**, not what was named. Reversing
    ///    default-on-default must give the theme's background drawn on its foreground — a swap
    ///    applied before resolution would produce `Default` on `Default` and change nothing at
    ///    all, which is the bug this ordering exists to prevent.
    pub fn resolve(&self, palette: &Palette) -> (Rgb, Rgb) {
        let fg = match (self.flags.contains(Flags::BOLD), self.fg) {
            (true, Colour::Ansi(a)) => Colour::Ansi(a.brightened()),
            (_, c) => c,
        };
        let (fg, bg) = (palette.foreground_of(fg), palette.background_of(self.bg));
        if self.flags.contains(Flags::REVERSE) { (bg, fg) } else { (fg, bg) }
    }
}

/// One grid position.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    /// What is printed. A blank cell holds a space rather than `\0`, so a cell is always
    /// drawable and nothing needs a "is this empty" rule.
    pub ch: char,
    /// How it is drawn.
    pub attrs: Attributes,
}

impl Cell {
    /// A space in the default attributes — what erasing produces and what a new line is.
    pub const BLANK: Cell = Cell {
        ch: ' ',
        attrs: Attributes { fg: Colour::Default, bg: Colour::Default, flags: Flags::NONE },
    };

    /// A cell holding `ch` with `attrs`.
    pub const fn new(ch: char, attrs: Attributes) -> Cell {
        Cell { ch, attrs }
    }
}

impl Default for Cell {
    /// [`Cell::BLANK`].
    fn default() -> Self {
        Cell::BLANK
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_colour_has_a_distinct_palette_entry() {
        // `Ansi::index` is `self as usize`, which silently depends on declaration order
        // matching `ALL`. If the two ever disagree, two colours share an entry and text
        // renders in the wrong one — visible, but only to someone who knows what it should be.
        for (i, c) in Ansi::ALL.iter().enumerate() {
            assert_eq!(c.index(), i, "{c:?} indexes {} but sits at {i}", c.index());
        }
    }

    #[test]
    fn brightening_maps_the_first_eight_onto_the_second_and_stops() {
        for (i, c) in Ansi::ALL.iter().enumerate() {
            let b = c.brightened();
            if i < 8 {
                assert_eq!(b, Ansi::ALL[i + 8], "{c:?} brightened to {b:?}");
            } else {
                assert_eq!(b, *c, "{c:?} is already bright and moved to {b:?}");
            }
            assert_eq!(b.brightened(), b, "brightening is idempotent");
        }
    }

    #[test]
    fn a_default_colour_is_not_a_named_one() {
        // The reason `Colour::Default` exists at all: SGR 39 must not resolve to whatever
        // white happens to be, or a re-theme leaves old text stranded in the old default.
        let mut p = Palette::default();
        p.foreground = Rgb::new(1, 2, 3);
        assert_eq!(p.foreground_of(Colour::Default), Rgb::new(1, 2, 3));
        assert_ne!(
            p.foreground_of(Colour::Default),
            p.foreground_of(Colour::Ansi(Ansi::White)),
            "the default resolved to a palette entry"
        );
    }

    #[test]
    fn the_same_colour_differs_by_where_it_is_used() {
        // Only for `Default`: a named colour is the same pixel either side, and the default is
        // two different pixels. A single `resolve(c)` would have to pick one and be wrong half
        // the time.
        let p = Palette::default();
        assert_eq!(p.foreground_of(Colour::Ansi(Ansi::Red)), p.background_of(Colour::Ansi(Ansi::Red)));
        assert_ne!(p.foreground_of(Colour::Default), p.background_of(Colour::Default));
    }

    #[test]
    fn bold_brightens_a_named_foreground_and_leaves_the_default_alone() {
        let p = Palette::default();
        let plain = Attributes { fg: Colour::Ansi(Ansi::Red), ..Default::default() };
        let bold = Attributes { flags: Flags::BOLD, ..plain };
        assert_eq!(bold.resolve(&p).0, p.ansi[Ansi::BrightRed.index()]);
        assert_ne!(bold.resolve(&p).0, plain.resolve(&p).0, "bold changed nothing visible");

        // The default has no bright counterpart to move to, and inventing one would make
        // `SGR 1` on default text a theme change.
        let bold_default = Attributes { flags: Flags::BOLD, ..Default::default() };
        assert_eq!(bold_default.resolve(&p).0, p.foreground);
    }

    #[test]
    fn bold_does_not_touch_the_background() {
        let p = Palette::default();
        let a = Attributes {
            fg: Colour::Ansi(Ansi::Red),
            bg: Colour::Ansi(Ansi::Blue),
            flags: Flags::BOLD,
        };
        assert_eq!(a.resolve(&p).1, p.ansi[Ansi::Blue.index()]);
    }

    #[test]
    fn reverse_swaps_the_resolved_colours_not_the_named_ones() {
        // **The ordering bug this exists to prevent.** Swapping `fg`/`bg` before resolution
        // turns `Default`/`Default` into `Default`/`Default` — reverse video on default text
        // would do nothing, which is exactly where a shell uses it (a selected menu line).
        let p = Palette::default();
        let rev = Attributes { flags: Flags::REVERSE, ..Default::default() };
        assert_eq!(rev.resolve(&p), (p.background, p.foreground));

        let plain = Attributes::default();
        assert_ne!(rev.resolve(&p), plain.resolve(&p), "reverse on defaults did nothing");
    }

    #[test]
    fn reverse_and_bold_compose_in_the_stated_order() {
        // Bold brightens the foreground, *then* reverse moves it to the background. A reverse
        // applied first would brighten what is now the background instead.
        let p = Palette::default();
        let a = Attributes {
            fg: Colour::Ansi(Ansi::Green),
            bg: Colour::Ansi(Ansi::Black),
            flags: Flags::BOLD.with(Flags::REVERSE),
        };
        let (fg, bg) = a.resolve(&p);
        assert_eq!(fg, p.ansi[Ansi::Black.index()], "the background did not become the ink");
        assert_eq!(bg, p.ansi[Ansi::BrightGreen.index()], "the brightened ink did not move");
    }

    #[test]
    fn flags_are_a_set() {
        let f = Flags::NONE.with(Flags::BOLD).with(Flags::UNDERLINE);
        assert!(f.contains(Flags::BOLD) && f.contains(Flags::UNDERLINE));
        assert!(!f.contains(Flags::REVERSE));
        assert!(f.contains(Flags::BOLD.with(Flags::UNDERLINE)), "contains takes a set, not a bit");
        assert!(!f.contains(Flags::BOLD.with(Flags::REVERSE)), "a partial match is not a match");
        assert!(!f.without(Flags::BOLD).contains(Flags::BOLD));
        assert_eq!(f.with(Flags::BOLD), f, "setting a set bit changed something");
    }

    #[test]
    fn a_blank_cell_is_a_space_in_default_attributes() {
        // Erasing writes this, so it has to be what a never-written cell already is — or a
        // cleared region would differ from an untouched one and the diff would repaint it.
        assert_eq!(Cell::default(), Cell::BLANK);
        assert_eq!(Cell::BLANK.ch, ' ');
        assert_eq!(Cell::BLANK.attrs, Attributes::default());
    }

    #[test]
    fn a_cell_is_no_bigger_than_it_needs_to_be() {
        // 80x24 is 1,920 cells before any scrollback, and the scrollback ring multiplies that
        // by the number of retained lines. Not a hard requirement — an assertion so that a
        // field added without thinking shows up as a decision rather than as memory.
        assert!(
            core::mem::size_of::<Cell>() <= 8,
            "a cell is {} bytes",
            core::mem::size_of::<Cell>()
        );
    }
}
