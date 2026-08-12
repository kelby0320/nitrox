//! The output parser: a program's bytes become grid operations.
//!
//! ## It emits operations rather than driving a grid
//!
//! [`Parser::feed`] returns [`Op`]s and touches no state but its own. The alternative — a
//! parser holding a `&mut Grid` and applying as it goes — fuses two state machines that fail
//! differently: an escape-sequence bug and a wrapping bug would present identically, and
//! neither could be tested without the other. This way A3 lands before A4 and each has tests
//! that name what they are about.
//!
//! **The parser does not own current attributes either.** `SGR` emits an effect and the grid
//! applies it, because attributes are terminal state — the thing `DECSC` saves and `RIS`
//! resets — and splitting them across two owners is how they end up disagreeing.
//!
//! ## What it handles, and why that is the list
//!
//! Bounded by what `nxsh` and the coreutils emit, plus what a person's `Ctrl-L` needs: cursor
//! addressing and relative movement, erase in display and line, and `SGR` for bold, underline,
//! reverse and the sixteen colours. Parameters are parsed **generically** — a bounded list of
//! numbers — so 256-colour becomes a match arm rather than a reshape.
//!
//! ## An unrecognised sequence is consumed, never printed
//!
//! The failure this exists to prevent is `[0m` appearing in the middle of someone's output. A
//! parser that abandons a sequence it does not know must still swallow every byte of it,
//! including the final one. `tty_server::Discipline` learned this on the input side; the rule
//! is the same here and the tests are the ones that check the *next* character survives.

use crate::cell::{Ansi, Colour};

/// The most operations one byte can complete.
///
/// An `SGR` with a full parameter list is the only sequence that produces more than one, and
/// [`MAX_PARAMS`] bounds it. Sized from that rather than guessed, the way `libinput`'s
/// `MAX_PER_GROUP` is.
pub const MAX_PER_BYTE: usize = MAX_PARAMS;

/// Parameters retained in one CSI sequence.
///
/// Sixteen, which is what `xterm` allows and more than anything in this system emits.
/// **Excess parameters are dropped, and the sequence still executes** — the alternative is
/// abandoning a sequence because it was too enthusiastic, which loses the effects that did
/// fit and is more surprising than ignoring the tail.
pub const MAX_PARAMS: usize = 16;

/// How much of a region an erase covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Erase {
    /// From the cursor to the end (parameter `0`, and the default).
    ToEnd,
    /// From the start to the cursor, inclusive (parameter `1`).
    ToStart,
    /// The whole line or screen (parameter `2`).
    All,
}

/// One `SGR` effect. A sequence with several parameters produces several of these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sgr {
    /// `0` — every attribute back to its default.
    Reset,
    /// `1`.
    Bold,
    /// `22` — the "normal intensity" that turns bold off. Not `21`, which is double-underline
    /// in ECMA-48 and only historically a bold-off.
    NoBold,
    /// `4`.
    Underline,
    /// `24`.
    NoUnderline,
    /// `7`.
    Reverse,
    /// `27`.
    NoReverse,
    /// `30`–`37`, `90`–`97`, and `39` for the default.
    Foreground(Colour),
    /// `40`–`47`, `100`–`107`, and `49` for the default.
    Background(Colour),
}

/// What the parser asks the grid to do.
///
/// Movement is expressed as the terminal expresses it — relative counts and a one-based
/// absolute position converted to zero-based here — and **clamping is the grid's**, because
/// only the grid knows how big it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// Put a character at the cursor and advance.
    Print(char),
    /// `CUP` — move to a zero-based position. Out-of-range values are the grid's to clamp.
    MoveTo {
        /// Zero-based row.
        row: u16,
        /// Zero-based column.
        col: u16,
    },
    /// `CUU`/`CUD`/`CUF`/`CUB` — move by a count, which is at least 1.
    MoveBy {
        /// Negative is up.
        rows: i32,
        /// Negative is left.
        cols: i32,
    },
    /// `\r`.
    CarriageReturn,
    /// `\n`.
    LineFeed,
    /// `\x08` — move left one, without erasing. Erasing is the caller's next write.
    Backspace,
    /// `\t`.
    Tab,
    /// `ED`.
    EraseInDisplay(Erase),
    /// `EL`.
    EraseInLine(Erase),
    /// One `SGR` effect.
    Attr(Sgr),
}

/// Where the parser is in a sequence.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Ordinary text.
    Ground,
    /// `ESC` seen.
    Escape,
    /// `ESC` and an intermediate byte seen — `ESC ( B` (charset), `ESC # 8` (DECALN). One
    /// more byte completes it, and swallowing only two would print the third.
    EscapeIntermediate,
    /// `ESC [` seen; accumulating parameters.
    Csi,
    /// Inside a CSI this build does not implement — a private sequence such as `ESC [ ? 25 l`.
    /// Every byte is swallowed up to and including the final one.
    CsiIgnore,
    /// Mid-character in a UTF-8 sequence.
    Utf8 {
        /// Bits decoded so far.
        acc: u32,
        /// Continuation bytes still expected.
        left: u8,
    },
}

/// The byte-stream state machine.
///
/// `Default` is [`Parser::new`] — a parser in the ground state with no attributes of its own.
#[derive(Clone, Debug)]
pub struct Parser {
    state: State,
    /// Parameters of the CSI in progress. `None` is "absent", which is not the same as zero:
    /// `ESC [ ; 5 H` has an absent first parameter and a present second.
    params: [Option<u16>; MAX_PARAMS],
    /// How many parameter slots the sequence has used, saturating at [`MAX_PARAMS`].
    count: usize,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

/// Replacement character, for a UTF-8 sequence that does not decode.
const REPLACEMENT: char = '\u{FFFD}';

impl Parser {
    /// A parser in the ground state.
    pub const fn new() -> Self {
        Parser { state: State::Ground, params: [None; MAX_PARAMS], count: 0 }
    }

    /// Feed one byte. Returns how many of `out`'s slots were filled.
    ///
    /// `out` must be at least [`MAX_PER_BYTE`] long; the return is `0` for every byte that
    /// does not complete something, which is most of them inside a sequence.
    pub fn feed(&mut self, b: u8, out: &mut [Op]) -> usize {
        match self.state {
            State::Ground => self.ground(b, out),
            State::Escape => self.escape(b),
            State::EscapeIntermediate => {
                match b {
                    // `ESC` cancels, here as everywhere else. This branch ate it until the
                    // test that checks every mid-sequence state, rather than one of them.
                    0x1B => self.state = State::Escape,
                    // Another intermediate: `ESC $ ( C` is a multibyte charset selection.
                    // ECMA-48 allows any number, so this loops rather than assuming one.
                    0x20..=0x2F => {}
                    // A final byte ends it.
                    _ => self.state = State::Ground,
                }
                0
            }
            State::Csi => self.csi(b, out),
            State::CsiIgnore => {
                // Swallow to the final byte. A sequence abandoned halfway would spill its
                // remaining bytes into the grid, which is the `[0m`-in-your-output failure.
                if b == 0x1B {
                    self.state = State::Escape;
                } else if (0x40..=0x7E).contains(&b) {
                    self.state = State::Ground;
                }
                0
            }
            State::Utf8 { acc, left } => self.utf8(b, acc, left, out),
        }
    }

    /// Whether the parser is mid-sequence.
    ///
    /// For a caller that needs to know a flush is not going to lose half a character — the
    /// render, deciding whether the grid is in a consistent state to draw.
    pub fn is_idle(&self) -> bool {
        self.state == State::Ground
    }

    fn ground(&mut self, b: u8, out: &mut [Op]) -> usize {
        let op = match b {
            0x1B => {
                self.state = State::Escape;
                return 0;
            }
            b'\r' => Op::CarriageReturn,
            b'\n' => Op::LineFeed,
            0x08 => Op::Backspace,
            b'\t' => Op::Tab,
            // BEL, and every other C0 this build does not act on. Dropped rather than
            // printed: a control character rendered as a glyph is worse than a silent one.
            0x00..=0x1F | 0x7F => return 0,
            0x20..=0x7E => Op::Print(b as char),
            // UTF-8. A lead byte starts a sequence; a stray continuation byte is a decoding
            // error on its own and becomes one replacement character rather than being
            // silently eaten, so a desynchronised stream shows up rather than shortening.
            0xC2..=0xDF => {
                self.state = State::Utf8 { acc: (b & 0x1F) as u32, left: 1 };
                return 0;
            }
            0xE0..=0xEF => {
                self.state = State::Utf8 { acc: (b & 0x0F) as u32, left: 2 };
                return 0;
            }
            0xF0..=0xF4 => {
                self.state = State::Utf8 { acc: (b & 0x07) as u32, left: 3 };
                return 0;
            }
            _ => Op::Print(REPLACEMENT),
        };
        out[0] = op;
        1
    }

    fn escape(&mut self, b: u8) -> usize {
        match b {
            b'[' => {
                self.params = [None; MAX_PARAMS];
                self.count = 0;
                self.state = State::Csi;
            }
            // **`ESC` cancels and restarts.** A program interrupted mid-sequence emits a fresh
            // one, and a parser that treated the new `ESC` as the old sequence's payload would
            // print the replacement sequence as text.
            0x1B => {}
            // An intermediate byte: `ESC ( B` selects a charset, `ESC # 8` is DECALN. Three
            // bytes, not two — swallowing two prints the third, which is how `\x1b(B` used to
            // leak a `B` into the grid.
            0x20..=0x2F => self.state = State::EscapeIntermediate,
            // `ESC` and a final byte: `RIS`, keypad modes, `ESC 7`/`ESC 8`. None implemented
            // here; consumed whole.
            _ => self.state = State::Ground,
        }
        0
    }

    fn utf8(&mut self, b: u8, acc: u32, left: u8, out: &mut [Op]) -> usize {
        if b & 0xC0 != 0x80 {
            // Not a continuation byte. The sequence is broken, and **the byte is re-fed**
            // rather than dropped: it is the start of something valid, and eating it would
            // turn one malformed character into two.
            self.state = State::Ground;
            out[0] = Op::Print(REPLACEMENT);
            return 1 + self.feed(b, &mut out[1..]);
        }
        let acc = (acc << 6) | (b & 0x3F) as u32;
        if left > 1 {
            self.state = State::Utf8 { acc, left: left - 1 };
            return 0;
        }
        self.state = State::Ground;
        // Surrogates and out-of-range values fail here; over-long encodings decode to a valid
        // scalar and are not rejected, which is a security concern for a *filter* and not for
        // a terminal that only draws the result.
        out[0] = Op::Print(char::from_u32(acc).unwrap_or(REPLACEMENT));
        1
    }

    /// Accumulate a parameter digit, or dispatch on a final byte.
    fn csi(&mut self, b: u8, out: &mut [Op]) -> usize {
        match b {
            // `ESC` cancels this sequence and starts a new one — see `escape`.
            0x1B => {
                self.state = State::Escape;
                0
            }
            b'0'..=b'9' => {
                if self.count == 0 {
                    self.count = 1;
                }
                if let Some(slot) = self.params.get_mut(self.count - 1) {
                    let v = slot.unwrap_or(0);
                    // Saturating: a parameter of a thousand digits is malformed input, and
                    // wrapping it would turn a clamp into a jump to somewhere plausible.
                    *slot = Some(v.saturating_mul(10).saturating_add((b - b'0') as u16));
                }
                0
            }
            b';' => {
                self.count = self.count.saturating_add(1).max(2);
                0
            }
            // A private-parameter marker (`?`, `<`, `=`, `>`) means a sequence outside the
            // standard set — `ESC [ ? 25 l` hides the cursor, and this build has no cursor
            // visibility to hide. Swallowed to the final byte.
            0x3C..=0x3F => {
                self.state = State::CsiIgnore;
                0
            }
            // Intermediate bytes. None of the sequences here use them, and a sequence that
            // does is one this build does not implement.
            0x20..=0x2F => {
                self.state = State::CsiIgnore;
                0
            }
            0x40..=0x7E => {
                self.state = State::Ground;
                self.dispatch(b, out)
            }
            // Anything else inside a CSI is malformed. Abandon without printing.
            _ => {
                self.state = State::Ground;
                0
            }
        }
    }

    /// Parameter `i`, or `None` if it was absent or past [`MAX_PARAMS`].
    ///
    /// **Bounded by the array, not only by `count`.** `count` records how many parameters the
    /// *sender* supplied, which is unbounded — seventeen semicolons make it seventeen — so
    /// indexing on `i < count` alone panics on the first read past the cap.
    fn param(&self, i: usize) -> Option<u16> {
        if i < self.count { self.params.get(i).copied().flatten() } else { None }
    }

    /// Parameter `i` as a movement count: absent or zero means one.
    ///
    /// The rule for `CUU`/`CUD`/`CUF`/`CUB` and for `CUP`'s two coordinates. `ESC [ 0 A` moves
    /// one line, not none — an off-by-one here makes a program that emits `0` stop moving.
    fn count_at(&self, i: usize) -> u16 {
        match self.param(i) {
            None | Some(0) => 1,
            Some(n) => n,
        }
    }

    fn dispatch(&mut self, final_byte: u8, out: &mut [Op]) -> usize {
        let one = |op: Op, out: &mut [Op]| {
            out[0] = op;
            1
        };
        match final_byte {
            b'A' => one(Op::MoveBy { rows: -(self.count_at(0) as i32), cols: 0 }, out),
            b'B' => one(Op::MoveBy { rows: self.count_at(0) as i32, cols: 0 }, out),
            b'C' => one(Op::MoveBy { rows: 0, cols: self.count_at(0) as i32 }, out),
            b'D' => one(Op::MoveBy { rows: 0, cols: -(self.count_at(0) as i32) }, out),
            // `CUP`, and `HVP` (`f`) which is the same operation under a different name.
            // **One-based on the wire, zero-based here**: the conversion belongs at the
            // boundary, so nothing below this line has to remember which convention it is in.
            b'H' | b'f' => one(
                Op::MoveTo { row: self.count_at(0) - 1, col: self.count_at(1) - 1 },
                out,
            ),
            b'J' => one(Op::EraseInDisplay(self.erase_at(0)), out),
            b'K' => one(Op::EraseInLine(self.erase_at(0)), out),
            b'm' => self.sgr(out),
            // A CSI this build does not implement — scroll regions, insert/delete line,
            // device status reports. Consumed and dropped, which is the whole point.
            _ => 0,
        }
    }

    /// Parameter `i` as an erase extent; absent means `0`, and an unknown value means `0` too.
    fn erase_at(&self, i: usize) -> Erase {
        match self.param(i).unwrap_or(0) {
            1 => Erase::ToStart,
            2 | 3 => Erase::All,
            _ => Erase::ToEnd,
        }
    }

    /// Decode every `SGR` parameter into an effect.
    ///
    /// A bare `ESC [ m` is `ESC [ 0 m` — a reset — which is what a shell emits at the end of a
    /// coloured prompt more often than it emits the explicit form.
    fn sgr(&self, out: &mut [Op]) -> usize {
        if self.count == 0 {
            out[0] = Op::Attr(Sgr::Reset);
            return 1;
        }
        let mut n = 0;
        for i in 0..self.count.min(MAX_PARAMS) {
            let Some(effect) = Self::sgr_effect(self.param(i).unwrap_or(0)) else { continue };
            out[n] = Op::Attr(effect);
            n += 1;
        }
        n
    }

    /// One `SGR` parameter, or `None` for a code this build does not implement.
    ///
    /// **Unknown codes are skipped, not abandoned.** `ESC [ 3 ; 31 m` is italic-then-red, and
    /// there is no italic here; dropping the whole sequence would lose the red as well.
    fn sgr_effect(p: u16) -> Option<Sgr> {
        Some(match p {
            0 => Sgr::Reset,
            1 => Sgr::Bold,
            4 => Sgr::Underline,
            7 => Sgr::Reverse,
            22 => Sgr::NoBold,
            24 => Sgr::NoUnderline,
            27 => Sgr::NoReverse,
            30..=37 => Sgr::Foreground(Colour::Ansi(Ansi::ALL[(p - 30) as usize])),
            39 => Sgr::Foreground(Colour::Default),
            40..=47 => Sgr::Background(Colour::Ansi(Ansi::ALL[(p - 40) as usize])),
            49 => Sgr::Background(Colour::Default),
            90..=97 => Sgr::Foreground(Colour::Ansi(Ansi::ALL[(p - 90) as usize + 8])),
            100..=107 => Sgr::Background(Colour::Ansi(Ansi::ALL[(p - 100) as usize + 8])),
            _ => return None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// Feed a whole string and collect everything it produced.
    fn run(s: &str) -> Vec<Op> {
        run_bytes(s.as_bytes())
    }

    fn run_bytes(bytes: &[u8]) -> Vec<Op> {
        let mut p = Parser::new();
        let mut all = Vec::new();
        let mut out = [Op::Print('\0'); MAX_PER_BYTE];
        for &b in bytes {
            let n = p.feed(b, &mut out);
            all.extend_from_slice(&out[..n]);
        }
        all
    }

    fn printed(ops: &[Op]) -> alloc::string::String {
        ops.iter()
            .filter_map(|o| match o {
                Op::Print(c) => Some(*c),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn plain_text_is_printed_one_character_at_a_time() {
        assert_eq!(run("hi"), alloc::vec![Op::Print('h'), Op::Print('i')]);
    }

    #[test]
    fn the_c0_controls_this_build_acts_on() {
        assert_eq!(
            run("\r\n\x08\t"),
            alloc::vec![Op::CarriageReturn, Op::LineFeed, Op::Backspace, Op::Tab]
        );
    }

    #[test]
    fn an_ignored_control_produces_nothing_rather_than_a_glyph() {
        // BEL and the rest. A control rendered as a character is worse than a silent one, and
        // `0x07 as char` is a printable-looking box in most fonts.
        assert_eq!(run("a\x07\x00\x7Fb"), alloc::vec![Op::Print('a'), Op::Print('b')]);
    }

    #[test]
    fn cursor_movement_defaults_to_one_and_zero_means_one() {
        // `ESC [ 0 A` moves one line. A program that emits the zero form stops moving if this
        // is wrong, which looks like the program's bug rather than the terminal's.
        assert_eq!(run("\x1b[A"), alloc::vec![Op::MoveBy { rows: -1, cols: 0 }]);
        assert_eq!(run("\x1b[0A"), alloc::vec![Op::MoveBy { rows: -1, cols: 0 }]);
        assert_eq!(run("\x1b[5B"), alloc::vec![Op::MoveBy { rows: 5, cols: 0 }]);
        assert_eq!(run("\x1b[3C"), alloc::vec![Op::MoveBy { rows: 0, cols: 3 }]);
        assert_eq!(run("\x1b[2D"), alloc::vec![Op::MoveBy { rows: 0, cols: -2 }]);
    }

    #[test]
    fn cup_is_one_based_on_the_wire_and_zero_based_here() {
        // The conversion happens once, at the boundary. Everything below this line is
        // zero-based, so nothing else has to remember which convention it is in.
        assert_eq!(run("\x1b[H"), alloc::vec![Op::MoveTo { row: 0, col: 0 }]);
        assert_eq!(run("\x1b[1;1H"), alloc::vec![Op::MoveTo { row: 0, col: 0 }]);
        assert_eq!(run("\x1b[10;40H"), alloc::vec![Op::MoveTo { row: 9, col: 39 }]);
        // `HVP` is the same operation spelled `f`.
        assert_eq!(run("\x1b[10;40f"), alloc::vec![Op::MoveTo { row: 9, col: 39 }]);
    }

    #[test]
    fn an_absent_parameter_is_not_a_zero() {
        // `ESC [ ; 5 H` has an absent row and a present column. Treating absent as zero would
        // move to row -1 before the default rule, and treating it as "no parameters at all"
        // would lose the 5.
        assert_eq!(run("\x1b[;5H"), alloc::vec![Op::MoveTo { row: 0, col: 4 }]);
    }

    #[test]
    fn erase_extents_and_their_default() {
        assert_eq!(run("\x1b[J"), alloc::vec![Op::EraseInDisplay(Erase::ToEnd)]);
        assert_eq!(run("\x1b[0J"), alloc::vec![Op::EraseInDisplay(Erase::ToEnd)]);
        assert_eq!(run("\x1b[1J"), alloc::vec![Op::EraseInDisplay(Erase::ToStart)]);
        assert_eq!(run("\x1b[2J"), alloc::vec![Op::EraseInDisplay(Erase::All)]);
        // `3J` is "erase scrollback" in xterm; treated as `All` rather than ignored, because
        // a `Ctrl-L` that clears less than asked is more surprising than one that clears more.
        assert_eq!(run("\x1b[3J"), alloc::vec![Op::EraseInDisplay(Erase::All)]);
        assert_eq!(run("\x1b[K"), alloc::vec![Op::EraseInLine(Erase::ToEnd)]);
        assert_eq!(run("\x1b[1K"), alloc::vec![Op::EraseInLine(Erase::ToStart)]);
        assert_eq!(run("\x1b[2K"), alloc::vec![Op::EraseInLine(Erase::All)]);
    }

    #[test]
    fn sgr_produces_one_effect_per_parameter() {
        assert_eq!(
            run("\x1b[1;4;31m"),
            alloc::vec![
                Op::Attr(Sgr::Bold),
                Op::Attr(Sgr::Underline),
                Op::Attr(Sgr::Foreground(Colour::Ansi(Ansi::Red))),
            ]
        );
    }

    #[test]
    fn a_bare_sgr_is_a_reset() {
        // What a shell emits at the end of a coloured prompt, more often than `ESC [ 0 m`.
        assert_eq!(run("\x1b[m"), alloc::vec![Op::Attr(Sgr::Reset)]);
        assert_eq!(run("\x1b[0m"), alloc::vec![Op::Attr(Sgr::Reset)]);
    }

    #[test]
    fn every_colour_code_maps_to_the_colour_it_names() {
        for (i, c) in Ansi::ALL.iter().enumerate().take(8) {
            let fg = alloc::format!("\x1b[{}m", 30 + i);
            assert_eq!(run(&fg), alloc::vec![Op::Attr(Sgr::Foreground(Colour::Ansi(*c)))]);
            let bg = alloc::format!("\x1b[{}m", 40 + i);
            assert_eq!(run(&bg), alloc::vec![Op::Attr(Sgr::Background(Colour::Ansi(*c)))]);
        }
        for (i, c) in Ansi::ALL.iter().enumerate().skip(8) {
            let fg = alloc::format!("\x1b[{}m", 90 + i - 8);
            assert_eq!(run(&fg), alloc::vec![Op::Attr(Sgr::Foreground(Colour::Ansi(*c)))]);
            let bg = alloc::format!("\x1b[{}m", 100 + i - 8);
            assert_eq!(run(&bg), alloc::vec![Op::Attr(Sgr::Background(Colour::Ansi(*c)))]);
        }
        assert_eq!(run("\x1b[39m"), alloc::vec![Op::Attr(Sgr::Foreground(Colour::Default))]);
        assert_eq!(run("\x1b[49m"), alloc::vec![Op::Attr(Sgr::Background(Colour::Default))]);
    }

    #[test]
    fn an_unknown_sgr_parameter_is_skipped_and_its_neighbours_survive() {
        // `3` is italic and there is none here. Abandoning the sequence would lose the red,
        // which is the visible half.
        assert_eq!(
            run("\x1b[3;31m"),
            alloc::vec![Op::Attr(Sgr::Foreground(Colour::Ansi(Ansi::Red)))]
        );
    }

    #[test]
    fn an_unknown_sequence_is_swallowed_whole() {
        // **The failure this parser exists to avoid**: `[0m` scattered through someone's
        // output. Every byte of an abandoned sequence goes, including the final one, and the
        // character *after* it must survive.
        for seq in ["\x1b[6n", "\x1b[?25l", "\x1b[1;2r", "\x1b[ q", "\x1b(B", "\x1b#8"] {
            let ops = run(&alloc::format!("a{seq}b"));
            assert_eq!(printed(&ops), "ab", "{seq:?} leaked");
        }
    }

    #[test]
    fn a_malformed_csi_does_not_swallow_the_rest_of_the_stream() {
        // A byte that can appear in no CSI ends it. The alternative is a parser that eats
        // everything after one corrupt byte, which is a terminal that goes silent.
        let ops = run("a\x1b[1\x01Zb");
        assert_eq!(printed(&ops), "aZb", "the stream did not resynchronise");
    }

    #[test]
    fn parameters_past_the_cap_are_dropped_and_the_sequence_still_runs() {
        // Seventeen parameters, the last of which is red. The cap loses it; what matters is
        // that the sixteen that fit still take effect rather than the whole sequence dying.
        let mut s = alloc::string::String::from("\x1b[");
        for _ in 0..MAX_PARAMS {
            s.push_str("1;");
        }
        s.push_str("31m");
        let ops = run(&s);
        assert_eq!(ops.len(), MAX_PARAMS, "expected exactly the parameters that fit");
        assert!(ops.iter().all(|o| *o == Op::Attr(Sgr::Bold)));
    }

    #[test]
    fn a_parameter_index_past_the_cap_is_none_rather_than_a_panic() {
        // **Tested against the private accessor on purpose.** `param`'s bound is unreachable
        // through `feed` today — the only loop over parameters stops at `MAX_PARAMS`, and the
        // cursor and erase dispatches read indices 0 and 1 — so breaking it changes nothing
        // any public test observes. That makes it exactly the kind of defensive bound that
        // rots: the panic it prevents is what the seventeen-parameter case originally hit, and
        // the next caller that loops to `count` would hit it again.
        let mut p = Parser::new();
        let mut out = [Op::Print('\0'); MAX_PER_BYTE];
        for b in b"\x1b[".iter().chain(b"1;".repeat(20).iter()) {
            p.feed(*b, &mut out);
        }
        assert!(p.count > MAX_PARAMS, "the state this is about was not reached");
        for i in MAX_PARAMS..p.count {
            assert_eq!(p.param(i), None, "index {i} is past the array");
        }
    }

    #[test]
    fn a_huge_parameter_saturates_rather_than_wrapping() {
        // Wrapping would turn a clamp into a jump to somewhere plausible-looking, which is
        // much harder to diagnose than a cursor pinned to the last row.
        assert_eq!(run("\x1b[99999999A"), alloc::vec![Op::MoveBy { rows: -(u16::MAX as i32), cols: 0 }]);
    }

    #[test]
    fn utf8_decodes_to_one_character() {
        assert_eq!(printed(&run("aé€𝄞b")), "aé€𝄞b");
    }

    #[test]
    fn a_broken_utf8_sequence_yields_one_replacement_and_keeps_the_next_byte() {
        // A truncated lead byte followed by ASCII: the `b` is the start of something valid and
        // eating it would turn one malformed character into two.
        assert_eq!(printed(&run_bytes(b"a\xC3b")), "a\u{FFFD}b");
        // A stray continuation byte on its own.
        assert_eq!(printed(&run_bytes(b"a\x80b")), "a\u{FFFD}b");
        // A surrogate, which encodes but is not a scalar value.
        assert_eq!(printed(&run_bytes(b"a\xED\xA0\x80b")), "a\u{FFFD}b");
    }

    #[test]
    fn a_sequence_split_across_feeds_still_parses() {
        // The real delivery shape: bytes arrive in whatever chunks the channel gives, and a
        // parser that only worked on whole sequences would be a parser that worked in tests.
        let mut p = Parser::new();
        let mut all = Vec::new();
        let mut out = [Op::Print('\0'); MAX_PER_BYTE];
        for chunk in [&b"\x1b"[..], &b"[1;3"[..], &b"1"[..], &b"m!"[..]] {
            for &b in chunk {
                let n = p.feed(b, &mut out);
                all.extend_from_slice(&out[..n]);
            }
        }
        assert_eq!(
            all,
            alloc::vec![
                Op::Attr(Sgr::Bold),
                Op::Attr(Sgr::Foreground(Colour::Ansi(Ansi::Red))),
                Op::Print('!'),
            ]
        );
    }

    #[test]
    fn is_idle_reports_mid_sequence() {
        let mut p = Parser::new();
        let mut out = [Op::Print('\0'); MAX_PER_BYTE];
        assert!(p.is_idle());
        p.feed(0x1b, &mut out);
        assert!(!p.is_idle(), "an ESC left the parser looking idle");
        p.feed(b'[', &mut out);
        assert!(!p.is_idle());
        p.feed(b'm', &mut out);
        assert!(p.is_idle(), "the sequence finished and the parser is still mid-sequence");
        // Mid-character counts too: a render that drew here would show half a glyph missing.
        p.feed(0xC3, &mut out);
        assert!(!p.is_idle());
    }

    #[test]
    fn escape_restarts_from_every_state_that_can_hold_one() {
        // `ESC` cancels whatever is in flight. Checked from each state that can be mid-sequence
        // rather than from one, because each has its own branch and only the CSI one was
        // written first — the private-sequence state swallowed a following `ESC` for a while.
        for prefix in [
            "\x1b[1",   // mid-CSI, parameters accumulated
            "\x1b[?25", // mid private sequence
            "\x1b",     // ESC ESC
            "\x1b(",    // mid three-byte sequence
        ] {
            let ops = run(&alloc::format!("{prefix}\x1b[31mX"));
            assert_eq!(
                ops,
                alloc::vec![Op::Attr(Sgr::Foreground(Colour::Ansi(Ansi::Red))), Op::Print('X')],
                "{prefix:?} ate the sequence that followed it"
            );
        }
    }

    #[test]
    fn a_three_byte_escape_sequence_consumes_exactly_three() {
        // `ESC ( B` and `ESC # 8`. Two-byte swallowing prints the third, which is how a
        // charset selection used to leak a `B`; four-byte swallowing eats the next character.
        assert_eq!(printed(&run("\x1b(BX")), "X");
        assert_eq!(printed(&run("\x1b#8X")), "X");
        // And the two-byte forms still take exactly two.
        assert_eq!(printed(&run("\x1b7X")), "X");
    }

    #[test]
    fn an_escape_inside_a_sequence_starts_a_new_one() {
        // A program that gets interrupted mid-sequence and starts another must not have its
        // second sequence eaten by the first. `ESC` is not in the CSI's accepted set, so the
        // malformed branch ends the old sequence — and the new `ESC` is the next byte fed.
        let ops = run("\x1b[1\x1b[31mX");
        assert_eq!(
            ops,
            alloc::vec![Op::Attr(Sgr::Foreground(Colour::Ansi(Ansi::Red))), Op::Print('X')]
        );
    }
}
