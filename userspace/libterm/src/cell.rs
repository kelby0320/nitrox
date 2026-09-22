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
    /// The design's terminal, as sixteen colours and a pair of defaults.
    ///
    /// **Retuned once, for the whole set** (desktop refresh, Part F). The design names five of
    /// these outright — the banner's cyan, the prompt's bright cyan, the dim its table headers
    /// and notes are set in, its success green and its error red — plus `--term` and `--termFg`.
    /// The other eleven are built in the same key rather than left at the previous set's, because
    /// a palette half in one key and half in another is what makes a terminal look like two
    /// programs. `ok`, `warn` and `deny` from the theme are the anchors for green, yellow and red.
    ///
    /// **One set, not one per palette.** The design keeps its terminal colours outside both of
    /// its themes, which is the same conclusion the paragraph below reaches from the other
    /// direction: these are tuned for a dark ground and a light desktop does not retune them.
    ///
    /// **All eighteen are the terminal's.** `ansi` is what a program addresses with `ESC[31m` — a
    /// vocabulary defined by what programs expect, which is why retheming a desktop must not
    /// retheme `ls` output. `foreground` and `background` are what `Colour::Default` means.
    ///
    /// **Those two were read from the shared theme from M11 Part B until Part E**, on the
    /// argument that a terminal whose ground differed from the chrome around it would flash
    /// against it. The desktop turning light is the event that showed what the tie really was:
    /// these two belong with *the sixteen*, which are tuned for a dark ground. Bright white is
    /// `#ECF0F4`; on a white ground it is invisible, and bright yellow is unreadable. Following
    /// the theme would therefore mean retuning the sixteen, which is the one thing the paragraph
    /// above says not to do — so the grid keeps its own ground and a dark terminal sits on a
    /// light desktop, which is what most people's screens look like anyway.
    ///
    /// The desktop's own values are still one source: they are `Theme`'s, and nothing here is a
    /// second opinion about them — these are a *different* pair, for a different surface.
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
            // **The design's `--term` and `--termFg`** (desktop refresh, Part F). They were the
            // dark theme's own two, carried here when the desktop turned light rather than
            // re-chosen; the design names a pair for its terminal specifically, and keeps them
            // *outside* both of its palettes for the same reason this whole set sits outside the
            // theme. **Not `ansi[0]`**, which is a different colour for a reason — a ground equal
            // to a cell colour is text nobody can read, and the first attempt at this line used
            // it (caught by `xtask`'s own cross-crate test).
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

    /// Every colour a program prints in is readable on the ground it is printed on.
    ///
    /// **The one property of a palette that is not a matter of taste** (desktop refresh, Part F).
    /// `xtask` already asserts no colour *equals* the ground — the sharp edge — but a colour a
    /// shade off the ground is text nobody can read either, and a whole set was retuned here by
    /// hand. 4.5:1 is WCAG's threshold for body text; the tightest of the fifteen is plain red at
    /// 4.88, so this has room without being slack.
    ///
    /// **ANSI black is exempt, and that is the convention rather than a hole.** Slot 0 is what a
    /// program means by "the darkest thing"; it is dim on a dark ground in every terminal ever
    /// shipped, and a palette that made it readable would have stopped being black.
    #[test]
    fn every_colour_but_black_is_readable_on_the_terminals_ground() {
        // sRGB relative luminance, as WCAG defines it.
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

        // **And the sixteen are sixteen.** A duplicate loses a colour with no other symptom:
        // `ESC[32m` and `ESC[36m` would print the same pixels and nothing would say so.
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
