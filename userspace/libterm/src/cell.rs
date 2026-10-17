//! What a grid position holds: a character, and how it is drawn.

use libdraw::format::Rgb;

/// One of the sixteen ANSI colours.
///
/// Named exhaustively rather than held as an index, so the supported set is the type.
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
    /// Not white-on-black: it stays unresolved in the cell so a re-theme recolours scrollback too.
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
    /// The terminal's own sixteen colours and default pair.
    ///
    /// Tuned for a dark ground and independent of the desktop theme: bright white vanishes and
    /// bright yellow is unreadable on a light ground, and retheming the desktop must not
    /// retheme `ls` output.
    fn default() -> Self {
        Self {
            ansi: [
                Rgb::new(0x1A, 0x22, 0x24), // black
                Rgb::new(0xBE, 0x6A, 0x62), // red
                Rgb::new(0x5E, 0x9E, 0x78), // green
                Rgb::new(0xB0, 0x8A, 0x4A), // yellow
                Rgb::new(0x4E, 0x8C, 0xA6), // blue
                Rgb::new(0x9A, 0x7B, 0xA8), // magenta
                Rgb::new(0x6F, 0xB7, 0xAE), // cyan — the design's banner
                Rgb::new(0xB8, 0xC6, 0xC4), // white
                Rgb::new(0x8F, 0xA5, 0xA3), // bright black — the design's dim
                Rgb::new(0xD6, 0x8A, 0x83), // bright red — the design's error
                Rgb::new(0x8F, 0xD6, 0xA8), // bright green — the design's success
                Rgb::new(0xD6, 0xB8, 0x7F), // bright yellow
                Rgb::new(0x79, 0xA8, 0xD6), // bright blue
                Rgb::new(0xC0, 0xA0, 0xCC), // bright magenta
                Rgb::new(0x79, 0xC6, 0xD6), // bright cyan — the design's prompt
                Rgb::new(0xEA, 0xF6, 0xF6), // bright white
            ],
            // The terminal's own ground, not `ansi[0]`: a ground equal to a cell colour is
            // unreadable.
            foreground: Rgb::new(0xCF, 0xDE, 0xDC),
            background: Rgb::new(0x0C, 0x12, 0x13),
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
/// A `u8` bit set rather than four `bool`s, so a cell's attributes cost one byte.
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
    /// **Bold brightens the foreground** first, and only an [`Ansi`] one: with one font weight
    /// (as in xterm and VTE without a bold face) `SGR 1` would otherwise be invisible.
    /// **Reverse then swaps the resolved colours**, not the named ones: swapping first would turn
    /// default-on-default into itself and change nothing.
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
    /// What is printed; a blank cell holds a space, never `\0`, so every cell is drawable.
    pub ch: char,
    /// How it is drawn.
    pub attrs: Attributes,
}

impl Cell {
    /// A space in the default attributes — what erasing produces and what a new line is.
    pub const BLANK: Cell =
        Cell { ch: ' ', attrs: Attributes { fg: Colour::Default, bg: Colour::Default, flags: Flags::NONE } };

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
        // `Ansi::index` is `self as usize`, which depends on declaration order matching `ALL`.
        for (i, c) in Ansi::ALL.iter().enumerate() {
            assert_eq!(c.index(), i, "{c:?} indexes {} but sits at {i}", c.index());
        }
    }

    /// Every colour a program prints in is readable on the ground it is printed on.
    ///
    /// 4.5:1 is WCAG's threshold for body text. ANSI black is exempt: slot 0 is dim on a dark
    /// ground in every terminal, and a readable one would stop being black.
    #[test]
    fn every_colour_but_black_is_readable_on_the_terminals_ground() {
        fn lum(c: Rgb) -> f32 {
            fn chan(v: u8) -> f32 {
                let v = v as f32 / 255.0;
                if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
            }
            0.2126 * chan(c.r) + 0.7152 * chan(c.g) + 0.0722 * chan(c.b)
        }
        fn ratio(a: Rgb, b: Rgb) -> f32 {
            let (hi, lo) = if lum(a) >= lum(b) { (lum(a), lum(b)) } else { (lum(b), lum(a)) };
            (hi + 0.05) / (lo + 0.05)
        }
        let p = Palette::default();
        for (i, c) in p.ansi.iter().enumerate() {
            if i == Ansi::Black.index() {
                continue;
            }
            let r = ratio(*c, p.background);
            assert!(r >= 4.5, "ANSI colour {i} ({c:?}) is {r:.2}:1 on the terminal's ground");
        }
        assert!(ratio(p.foreground, p.background) >= 4.5, "the default foreground is unreadable");

        for (i, a) in p.ansi.iter().enumerate() {
            for (j, b) in p.ansi.iter().enumerate().skip(i + 1) {
                assert_ne!(a, b, "ANSI colours {i} and {j} are the same colour");
            }
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
        // SGR 39 must not resolve to a palette entry, or a re-theme strands old text.
        let p = Palette { foreground: Rgb::new(1, 2, 3), ..Palette::default() };
        assert_eq!(p.foreground_of(Colour::Default), Rgb::new(1, 2, 3));
        assert_ne!(
            p.foreground_of(Colour::Default),
            p.foreground_of(Colour::Ansi(Ansi::White)),
            "the default resolved to a palette entry"
        );
    }

    #[test]
    fn the_same_colour_differs_by_where_it_is_used() {
        // Only `Default` differs by use; a named colour is one pixel either side.
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

        // The default has no bright counterpart; inventing one would make `SGR 1` a theme change.
        let bold_default = Attributes { flags: Flags::BOLD, ..Default::default() };
        assert_eq!(bold_default.resolve(&p).0, p.foreground);
    }

    #[test]
    fn bold_does_not_touch_the_background() {
        let p = Palette::default();
        let a = Attributes { fg: Colour::Ansi(Ansi::Red), bg: Colour::Ansi(Ansi::Blue), flags: Flags::BOLD };
        assert_eq!(a.resolve(&p).1, p.ansi[Ansi::Blue.index()]);
    }

    #[test]
    fn reverse_swaps_the_resolved_colours_not_the_named_ones() {
        // Swapping before resolution would make reverse a no-op on default text.
        let p = Palette::default();
        let rev = Attributes { flags: Flags::REVERSE, ..Default::default() };
        assert_eq!(rev.resolve(&p), (p.background, p.foreground));

        let plain = Attributes::default();
        assert_ne!(rev.resolve(&p), plain.resolve(&p), "reverse on defaults did nothing");
    }

    #[test]
    fn reverse_and_bold_compose_in_the_stated_order() {
        // Bold brightens first, then reverse moves the result to the background.
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
        // Erasing writes this, so it must equal a never-written cell or the diff would repaint.
        assert_eq!(Cell::default(), Cell::BLANK);
        assert_eq!(Cell::BLANK.ch, ' ');
        assert_eq!(Cell::BLANK.attrs, Attributes::default());
    }

    #[test]
    fn a_cell_is_no_bigger_than_it_needs_to_be() {
        assert!(core::mem::size_of::<Cell>() <= 8, "a cell is {} bytes", core::mem::size_of::<Cell>());
    }
}
