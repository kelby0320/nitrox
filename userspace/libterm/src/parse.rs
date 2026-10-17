//! The output parser: a program's bytes become grid operations.
//!
//! The parser emits [`Op`]s and touches no state but its own, so an escape-sequence bug and a
//! wrapping bug cannot present identically. It does not own current attributes either: `SGR`
//! emits an effect and the grid applies it, because attributes are what `DECSC` saves and `RIS`
//! resets.
//!
//! It handles cursor addressing and relative movement, erase in display and line, and `SGR`
//! for bold, underline, reverse and the sixteen colours. **An unrecognised sequence is consumed
//! whole, never printed**, so `[0m` cannot appear in someone's output.

use crate::cell::{Ansi, Colour};

/// The most operations one byte can complete: an `SGR` with a full parameter list.
///
/// A broken UTF-8 sequence (a replacement plus the re-fed byte) and a C0 control inside a CSI
/// produce more than one too, both well below this.
pub const MAX_PER_BYTE: usize = MAX_PARAMS;

/// Parameters retained in one CSI sequence.
///
/// Sixteen, as `xterm` allows. **Excess parameters are dropped and the sequence still
/// executes**, which is less surprising than abandoning it.
pub const MAX_PARAMS: usize = 16;

/// How many bytes of an `OSC` payload are retained.
///
/// The payload comes from the program on the other end, so it is bounded. A payload that
/// overruns is *discarded whole* rather than truncated: acting on half a path would take the
/// terminal somewhere the shell never named. 256 exceeds any path the namespace can produce.
pub const MAX_OSC: usize = 256;

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
    /// `22` — normal intensity. Not `21`, which is double-underline in ECMA-48.
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
/// Movement is relative counts and a one-based absolute position converted to zero-based here;
/// **clamping is the grid's**, because only the grid knows its size.
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
    Ground,
    Escape,
    /// `ESC` and an intermediate byte — `ESC ( B` (charset), `ESC # 8` (DECALN). One more byte
    /// completes it.
    EscapeIntermediate,
    Csi,
    /// Inside a CSI this build does not implement, such as `ESC [ ? 25 l`; swallowed whole.
    CsiIgnore,
    /// Inside an `OSC`/`DCS`/`PM`/`APC` string, whose payload runs until `BEL` or `ST` (`ESC \\`).
    ///
    /// `osc` is set for `ESC ]` alone: only an `OSC` payload is kept.
    String {
        osc: bool,
    },
    StringEscape {
        osc: bool,
    },
    Utf8 {
        acc: u32,
        left: u8,
        /// Smallest scalar this many bytes may legally encode (rejects over-longs).
        min: u32,
    },
}

/// The byte-stream state machine.
///
/// `Default` is [`Parser::new`] — a parser in the ground state with no attributes of its own.
#[derive(Clone, Debug)]
pub struct Parser {
    state: State,
    /// Parameters of the CSI in progress. `None` is "absent", not zero: `ESC [ ; 5 H`.
    params: [Option<u16>; MAX_PARAMS],
    count: usize,
    osc: alloc::vec::Vec<u8>,
    osc_overran: bool,
    directory: Option<alloc::string::String>,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

const REPLACEMENT: char = '\u{FFFD}';

/// Whether `c` is a C0 or C1 control, which has no glyph and must never reach a cell.
const fn is_control(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1F}' | '\u{7F}'..='\u{9F}')
}

/// The C0 control this build acts on, if any.
///
/// Shared by the ground and CSI states, which must agree: a control mid-CSI is executed.
const fn c0_op(b: u8) -> Option<Op> {
    Some(match b {
        b'\r' => Op::CarriageReturn,
        b'\n' => Op::LineFeed,
        0x08 => Op::Backspace,
        b'\t' => Op::Tab,
        _ => return None,
    })
}

impl Parser {
    /// A parser in the ground state.
    pub const fn new() -> Self {
        Parser {
            state: State::Ground,
            params: [None; MAX_PARAMS],
            count: 0,
            osc: alloc::vec::Vec::new(),
            osc_overran: false,
            directory: None,
        }
    }

    /// Where the program on the other end last said it is, if it has ever said.
    ///
    /// Set by `OSC 7` (`ESC ] 7 ; <path> BEL` or `ST`), which a shell emits beside its prompt.
    ///
    /// **A path, not a `file://` URL**: Nitrox has no hosts, and half-reading a URL is worse
    /// than not claiming to. It lives on the parser rather than in an [`Op`] because it is a
    /// fact about the stream, not terminal state `DECSC` saves or `RIS` resets.
    pub fn directory(&self) -> Option<&str> {
        self.directory.as_deref()
    }

    /// Feed one byte. Returns how many of `out`'s slots were filled.
    ///
    /// `out` must be at least [`MAX_PER_BYTE`] long.
    pub fn feed(&mut self, b: u8, out: &mut [Op]) -> usize {
        match self.state {
            State::Ground => self.ground(b, out),
            State::Escape => self.escape(b),
            State::EscapeIntermediate => {
                match b {
                    0x1B => self.state = State::Escape,
                    // Another intermediate: `ESC $ ( C`; ECMA-48 allows any number.
                    0x20..=0x2F => {}
                    _ => self.state = State::Ground,
                }
                0
            }
            State::Csi => self.csi(b, out),
            State::String { osc } => {
                // Swallowed to `BEL`/`ST`, not a final byte: window-title `OSC`s are on every
                // prompt and would otherwise dump `0;title` into the grid.
                match b {
                    0x07 => {
                        self.state = State::Ground;
                        if osc {
                            self.finish_osc();
                        }
                    }
                    0x1B => self.state = State::StringEscape { osc },
                    _ => {
                        if osc {
                            self.push_osc(b);
                        }
                    }
                }
                0
            }
            State::StringEscape { osc } => {
                if b == b'\\' {
                    self.state = State::Ground;
                    if osc {
                        self.finish_osc();
                    }
                    0
                } else {
                    // A lone `ESC` inside a string cancels and restarts, so an unterminated
                    // string cannot swallow the session; the payload is dropped with it.
                    self.state = State::Escape;
                    self.discard_osc();
                    self.escape(b)
                }
            }
            State::CsiIgnore => {
                // Swallow to the final byte, or the rest would spill into the grid.
                if b == 0x1B {
                    self.state = State::Escape;
                } else if (0x40..=0x7E).contains(&b) {
                    self.state = State::Ground;
                }
                0
            }
            State::Utf8 { acc, left, min } => self.utf8(b, acc, left, min, out),
        }
    }

    /// Whether the parser is mid-sequence.
    ///
    /// For a caller that must not flush half a character.
    pub fn is_idle(&self) -> bool {
        self.state == State::Ground
    }

    fn ground(&mut self, b: u8, out: &mut [Op]) -> usize {
        let op = match b {
            0x1B => {
                self.state = State::Escape;
                return 0;
            }
            // BEL and every other C0 not acted on are dropped, not printed.
            0x00..=0x1F | 0x7F => match c0_op(b) {
                Some(op) => op,
                None => return 0,
            },
            0x20..=0x7E => Op::Print(b as char),
            // UTF-8 lead byte; a stray continuation byte becomes one replacement character.
            0xC2..=0xDF => {
                self.state = State::Utf8 { acc: (b & 0x1F) as u32, left: 1, min: 0x80 };
                return 0;
            }
            0xE0..=0xEF => {
                self.state = State::Utf8 { acc: (b & 0x0F) as u32, left: 2, min: 0x800 };
                return 0;
            }
            0xF0..=0xF4 => {
                self.state = State::Utf8 { acc: (b & 0x07) as u32, left: 3, min: 0x10000 };
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
            // `ESC` cancels and restarts, so the new sequence is not printed as text.
            0x1B => {}
            // An intermediate byte: three bytes, not two, or `\x1b(B` leaks a `B`.
            0x20..=0x2F => self.state = State::EscapeIntermediate,
            // String introducers: `OSC`, `DCS`, `PM`, `APC`; ended by `BEL` or `ST`.
            b']' | b'P' | b'^' | b'_' => {
                self.discard_osc();
                self.state = State::String { osc: b == b']' };
            }
            // `ESC` and a final byte (`RIS`, keypad modes, `ESC 7`/`ESC 8`): consumed whole.
            _ => self.state = State::Ground,
        }
        0
    }

    fn push_osc(&mut self, b: u8) {
        if self.osc.len() >= MAX_OSC {
            self.osc_overran = true;
            return;
        }
        self.osc.push(b);
    }

    fn discard_osc(&mut self) {
        self.osc.clear();
        self.osc_overran = false;
    }

    fn finish_osc(&mut self) {
        let payload = core::mem::take(&mut self.osc);
        let overran = core::mem::take(&mut self.osc_overran);
        if overran {
            return;
        }
        // `7` is the working directory. Titles (`0`, `1`, `2`) and other numbers are swallowed
        // unread.
        let Some(rest) = payload.strip_prefix(b"7;") else { return };
        let Ok(path) = core::str::from_utf8(rest) else { return };
        // The value goes into a title bar, so it must be absolute and free of controls: a
        // payload with an escape would let the program put anything in a window's chrome.
        if !path.starts_with('/') || path.chars().any(is_control) {
            return;
        }
        self.directory = Some(path.into());
    }

    fn utf8(&mut self, b: u8, acc: u32, left: u8, min: u32, out: &mut [Op]) -> usize {
        if b & 0xC0 != 0x80 {
            // Not a continuation byte: broken sequence; the byte is re-fed, being the start of
            // something valid.
            self.state = State::Ground;
            out[0] = Op::Print(REPLACEMENT);
            return 1 + self.feed(b, &mut out[1..]);
        }
        let acc = (acc << 6) | (b & 0x3F) as u32;
        if left > 1 {
            self.state = State::Utf8 { acc, left: left - 1, min };
            return 0;
        }
        self.state = State::Ground;
        // Over-longs are rejected explicitly, and so is any control scalar: `E0 80 9B` decodes
        // to `U+001B`, and `C2 9B` is the 8-bit `CSI`; printing either bypasses the `0x1B` rule.
        let c = match char::from_u32(acc) {
            Some(c) if acc >= min && !is_control(c) => c,
            _ => REPLACEMENT,
        };
        out[0] = Op::Print(c);
        1
    }

    fn csi(&mut self, b: u8, out: &mut [Op]) -> usize {
        match b {
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
                    // Saturating: wrapping would turn a clamp into a jump somewhere plausible.
                    *slot = Some(v.saturating_mul(10).saturating_add((b - b'0') as u16));
                }
                0
            }
            // `;` and `:` both advance to the next parameter. `:` is ECMA-48's sub-parameter
            // separator; grouping is lost, but `ESC [ 38:2:255:0:0 m` must not leak.
            b';' | b':' => {
                self.count = self.count.saturating_add(1).max(2);
                0
            }
            // A C0 control mid-sequence is executed and the sequence continues, as in xterm:
            // abandoning would print the rest, swallowing would drop the control.
            0x18 | 0x1A => {
                self.state = State::Ground;
                0
            }
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => match c0_op(b) {
                Some(op) => {
                    out[0] = op;
                    1
                }
                None => 0,
            },
            // A private-parameter marker (`?`, `<`, `=`, `>`): outside the standard set; swallowed.
            0x3C..=0x3F => {
                self.state = State::CsiIgnore;
                0
            }
            // Intermediate bytes: no sequence here uses them, so it is unimplemented.
            0x20..=0x2F => {
                self.state = State::CsiIgnore;
                0
            }
            0x40..=0x7E => {
                self.state = State::Ground;
                self.dispatch(b, out)
            }
            _ => {
                self.state = State::Ground;
                0
            }
        }
    }

    /// Parameter `i`, or `None` if it was absent or past [`MAX_PARAMS`].
    ///
    /// Bounded by the array as well as `count`, which the sender controls: seventeen
    /// semicolons would otherwise index past the cap.
    fn param(&self, i: usize) -> Option<u16> {
        if i < self.count { self.params.get(i).copied().flatten() } else { None }
    }

    /// Parameter `i` as a movement count: absent or zero means one.
    ///
    /// The rule for `CUU`/`CUD`/`CUF`/`CUB` and `CUP`: `ESC [ 0 A` moves one line, not none.
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
            // `CUP`, and `HVP` (`f`). One-based on the wire, zero-based from here on.
            b'H' | b'f' => one(Op::MoveTo { row: self.count_at(0) - 1, col: self.count_at(1) - 1 }, out),
            b'J' => one(Op::EraseInDisplay(self.erase_at(0)), out),
            b'K' => one(Op::EraseInLine(self.erase_at(0)), out),
            b'm' => self.sgr(out),
            // Unimplemented CSIs (scroll regions, insert/delete line, DSR): consumed and dropped.
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
    /// A bare `ESC [ m` is `ESC [ 0 m`, a reset.
    fn sgr(&self, out: &mut [Op]) -> usize {
        if self.count == 0 {
            out[0] = Op::Attr(Sgr::Reset);
            return 1;
        }
        let mut n = 0;
        for i in 0..self.count.min(MAX_PARAMS) {
            let p = self.param(i).unwrap_or(0);
            // `38` and `48` stop the scan: their sub-parameter counts differ by form, and
            // reading on takes their zeroes for spurious `Reset`s. What precedes still applies.
            if p == 38 || p == 48 {
                break;
            }
            let Some(effect) = Self::sgr_effect(p) else { continue };
            out[n] = Op::Attr(effect);
            n += 1;
        }
        n
    }

    /// One `SGR` parameter, or `None` for a code this build does not implement.
    ///
    /// Unknown codes are skipped, not abandoned: `ESC [ 3 ; 31 m` must keep its red.
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
        assert_eq!(run("\r\n\x08\t"), alloc::vec![Op::CarriageReturn, Op::LineFeed, Op::Backspace, Op::Tab]);
    }

    #[test]
    fn an_ignored_control_produces_nothing_rather_than_a_glyph() {
        assert_eq!(run("a\x07\x00\x7Fb"), alloc::vec![Op::Print('a'), Op::Print('b')]);
    }

    #[test]
    fn cursor_movement_defaults_to_one_and_zero_means_one() {
        assert_eq!(run("\x1b[A"), alloc::vec![Op::MoveBy { rows: -1, cols: 0 }]);
        assert_eq!(run("\x1b[0A"), alloc::vec![Op::MoveBy { rows: -1, cols: 0 }]);
        assert_eq!(run("\x1b[5B"), alloc::vec![Op::MoveBy { rows: 5, cols: 0 }]);
        assert_eq!(run("\x1b[3C"), alloc::vec![Op::MoveBy { rows: 0, cols: 3 }]);
        assert_eq!(run("\x1b[2D"), alloc::vec![Op::MoveBy { rows: 0, cols: -2 }]);
    }

    #[test]
    fn cup_is_one_based_on_the_wire_and_zero_based_here() {
        assert_eq!(run("\x1b[H"), alloc::vec![Op::MoveTo { row: 0, col: 0 }]);
        assert_eq!(run("\x1b[1;1H"), alloc::vec![Op::MoveTo { row: 0, col: 0 }]);
        assert_eq!(run("\x1b[10;40H"), alloc::vec![Op::MoveTo { row: 9, col: 39 }]);
        assert_eq!(run("\x1b[10;40f"), alloc::vec![Op::MoveTo { row: 9, col: 39 }]);
    }

    #[test]
    fn an_absent_parameter_is_not_a_zero() {
        // `ESC [ ; 5 H` has an absent row and a present column.
        assert_eq!(run("\x1b[;5H"), alloc::vec![Op::MoveTo { row: 0, col: 4 }]);
    }

    #[test]
    fn erase_extents_and_their_default() {
        assert_eq!(run("\x1b[J"), alloc::vec![Op::EraseInDisplay(Erase::ToEnd)]);
        assert_eq!(run("\x1b[0J"), alloc::vec![Op::EraseInDisplay(Erase::ToEnd)]);
        assert_eq!(run("\x1b[1J"), alloc::vec![Op::EraseInDisplay(Erase::ToStart)]);
        assert_eq!(run("\x1b[2J"), alloc::vec![Op::EraseInDisplay(Erase::All)]);
        // `3J` (erase scrollback in xterm) is treated as `All`: clearing less than asked surprises.
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
        assert_eq!(run("\x1b[3;31m"), alloc::vec![Op::Attr(Sgr::Foreground(Colour::Ansi(Ansi::Red)))]);
    }

    #[test]
    fn an_unknown_sequence_is_swallowed_whole() {
        for seq in ["\x1b[6n", "\x1b[?25l", "\x1b[1;2r", "\x1b[ q", "\x1b(B", "\x1b#8"] {
            let ops = run(&alloc::format!("a{seq}b"));
            assert_eq!(printed(&ops), "ab", "{seq:?} leaked");
        }
    }

    #[test]
    fn every_byte_of_the_parameter_range_keeps_the_sequence_intact() {
        // Swept, not sampled: `:` was the one byte of the `0x30..=0x3F` range with no arm.
        for b in 0x30u8..=0x3F {
            let s = alloc::format!("a\x1b[1{}mZ", b as char);
            assert_eq!(printed(&run(&s)), "aZ", "parameter byte {:#04x} leaked", b);
        }
        for seq in ["\x1b[38:2:255:0:0m", "\x1b[38:5:196m", "\x1b[4:3m", "\x1b[58:5:1m"] {
            let ops = run(&alloc::format!("a{seq}Z"));
            assert_eq!(printed(&ops), "aZ", "{seq:?} leaked");
        }
    }

    #[test]
    fn an_extended_colour_does_not_have_its_payload_read_as_codes() {
        // `38`'s sub-parameters must not be read as codes: an RGB triple's zeroes were `Reset`s.
        assert_eq!(run("\x1b[38;2;255;0;0m"), alloc::vec![]);
        assert_eq!(run("\x1b[48;5;0m"), alloc::vec![]);
        assert_eq!(run("\x1b[1;38;2;0;0;0m"), alloc::vec![Op::Attr(Sgr::Bold)]);
    }

    #[test]
    fn a_string_sequence_is_swallowed_to_its_terminator() {
        // `ESC ] 0 ; title BEL` is on every prompt; its payload must not reach the grid.
        for seq in [
            "\x1b]0;my title\x07",   // OSC, BEL-terminated
            "\x1b]0;my title\x1b\\", // OSC, ST-terminated
            "\x1bP1;2q#0\x1b\\",     // DCS
            "\x1b^private\x1b\\",    // PM
            "\x1b_app\x1b\\",        // APC
        ] {
            assert_eq!(printed(&run(&alloc::format!("a{seq}Z"))), "aZ", "{seq:?} leaked");
        }
        assert_eq!(printed(&run("a\x1b]0;m[1H\x07Z")), "aZ");
        assert_eq!(
            run("\x1b]0;oops\x1b[31mZ"),
            alloc::vec![Op::Attr(Sgr::Foreground(Colour::Ansi(Ansi::Red))), Op::Print('Z')]
        );
    }

    fn parse(s: &str) -> Parser {
        let mut p = Parser::new();
        let mut out = [Op::Print('\0'); MAX_PER_BYTE];
        for &b in s.as_bytes() {
            p.feed(b, &mut out);
        }
        p
    }

    #[test]
    fn osc_7_is_where_the_program_says_it_is() {
        assert_eq!(Parser::new().directory(), None, "nothing said, nothing claimed");
        for seq in ["\x1b]7;/home/alice\x07", "\x1b]7;/home/alice\x1b\\"] {
            assert_eq!(parse(seq).directory(), Some("/home/alice"), "{seq:?}");
        }
        // Read and swallowed are two things; this one must not print the path.
        assert_eq!(printed(&run("a\x1b]7;/home/alice\x07Z")), "aZ");
        assert_eq!(parse("\x1b]7;/a\x07\x1b]7;/b\x07").directory(), Some("/b"));
    }

    #[test]
    fn an_osc_this_build_does_not_read_leaves_the_directory_alone() {
        for seq in [
            "\x1b]0;a title\x07",       // a window title, not a directory
            "\x1b]77;/home/alice\x07",  // a number that merely starts with 7
            "\x1b]7;relative/path\x07", // not a path this could name
            "\x1b]7;\x07",              // empty
            "\x1b]7;/home/a\nb\x07",    // a control, on its way to a title bar
            "\x1bP7;/home/alice\x1b\\", // a DCS wearing OSC 7's clothes
        ] {
            let p = parse(&alloc::format!("\x1b]7;/start\x07{seq}"));
            assert_eq!(p.directory(), Some("/start"), "{seq:?} was taken for a place");
            assert_eq!(printed(&run(&alloc::format!("a{seq}Z"))), "aZ", "{seq:?} leaked");
        }
        // An `ESC` in the payload cancels: an abandoned `OSC` then a real `SGR`, so the `b` prints.
        let esc = "\x1b]7;/home/a\x1b[31mb\x07";
        assert_eq!(parse(&alloc::format!("\x1b]7;/start\x07{esc}")).directory(), Some("/start"));
        assert_eq!(printed(&run(&alloc::format!("a{esc}Z"))), "abZ");
        // An abandoned payload is not inherited: `7;/bad7;/good` would announce nothing.
        assert_eq!(parse("\x1b]7;/bad\x1b]7;/good\x07").directory(), Some("/good"));
    }

    #[test]
    fn an_osc_payload_is_bounded_and_an_overrun_announces_nothing() {
        // Past the bound the whole payload is dropped: a truncated path is a different directory.
        let long = alloc::format!("/{}", "x".repeat(MAX_OSC));
        let p = parse(&alloc::format!("\x1b]7;/start\x07\x1b]7;{long}\x07"));
        assert_eq!(p.directory(), Some("/start"), "an overrun payload was acted on");
        assert_eq!(printed(&run(&alloc::format!("a\x1b]7;{long}\x07Z"))), "aZ");

        // The bound from both sides: `7;` takes two bytes, so `MAX_OSC - 2` is one too long and
        // `MAX_OSC - 3` exactly fits.
        let over = alloc::format!("/{}", "x".repeat(MAX_OSC - 2));
        let p = parse(&alloc::format!("\x1b]7;/start\x07\x1b]7;{over}\x07"));
        assert_eq!(p.directory(), Some("/start"), "a payload one byte past the bound was taken");
        let fits = alloc::format!("/{}", "x".repeat(MAX_OSC - 3));
        assert_eq!(parse(&alloc::format!("\x1b]7;{fits}\x07")).directory(), Some(fits.as_str()));
    }

    #[test]
    fn a_control_character_never_reaches_a_cell_however_it_is_encoded() {
        // Over-longs and legal encodings of C1 controls (`C2 9B` is the 8-bit `CSI`) both.
        for bytes in [
            &b"\xE0\x80\x80"[..],     // over-long NUL
            &b"\xE0\x80\x9B"[..],     // over-long ESC
            &b"\xE0\x81\xBF"[..],     // over-long DEL
            &b"\xF0\x80\x80\xA0"[..], // over-long space
            &b"\xC2\x9B"[..],         // *legal* encoding of the 8-bit CSI
            &b"\xC2\x80"[..],         // legal C1 PAD
        ] {
            let ops = run_bytes(bytes);
            assert_eq!(ops.len(), 1, "{bytes:x?} produced {ops:?}");
            match ops[0] {
                Op::Print(c) => assert!(!is_control(c), "{bytes:x?} put control {c:?} in a cell"),
                ref o => panic!("{bytes:x?} produced {o:?}"),
            }
        }
        assert_eq!(printed(&run_bytes(b"\xC3\xA9")), "\u{e9}");
    }

    #[test]
    fn a_malformed_csi_does_not_swallow_the_rest_of_the_stream() {
        // Only `0x7F` and bytes above `0x7E` end a CSI unexecuted; the stream must resynchronise.
        for bad in [0x7Fu8, 0x80, 0xFF] {
            let mut s = alloc::vec::Vec::from(*b"a\x1b[1");
            s.push(bad);
            s.extend_from_slice(b"Zb");
            assert_eq!(printed(&run_bytes(&s)), "aZb", "{bad:#04x} did not resynchronise");
        }
    }

    #[test]
    fn a_c0_control_inside_a_csi_executes_and_the_sequence_continues() {
        // Abandoning would print the rest and swallowing would drop the control.
        assert_eq!(
            run("\x1b[1\rm"),
            alloc::vec![Op::CarriageReturn, Op::Attr(Sgr::Bold)],
            "the carriage return was lost, or `m` leaked"
        );
        // The parameter accumulates across the control: `1` and `2` make 12.
        assert_eq!(run("\x1b[1\n2A"), alloc::vec![Op::LineFeed, Op::MoveBy { rows: -12, cols: 0 }]);
        assert_eq!(printed(&run("a\x1b[1\x18Zb")), "aZb");
        assert_eq!(printed(&run("a\x1b[1\x1aZb")), "aZb");
    }

    #[test]
    fn parameters_past_the_cap_are_dropped_and_the_sequence_still_runs() {
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
        // The bound is unreachable through `feed`, so only the private accessor can pin it.
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
        assert_eq!(run("\x1b[99999999A"), alloc::vec![Op::MoveBy { rows: -(u16::MAX as i32), cols: 0 }]);
    }

    #[test]
    fn utf8_decodes_to_one_character() {
        assert_eq!(printed(&run("aé€𝄞b")), "aé€𝄞b");
    }

    #[test]
    fn a_broken_utf8_sequence_yields_one_replacement_and_keeps_the_next_byte() {
        assert_eq!(printed(&run_bytes(b"a\xC3b")), "a\u{FFFD}b");
        assert_eq!(printed(&run_bytes(b"a\x80b")), "a\u{FFFD}b");
        assert_eq!(printed(&run_bytes(b"a\xED\xA0\x80b")), "a\u{FFFD}b");
    }

    #[test]
    fn a_sequence_split_across_feeds_still_parses() {
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
            alloc::vec![Op::Attr(Sgr::Bold), Op::Attr(Sgr::Foreground(Colour::Ansi(Ansi::Red))), Op::Print('!'),]
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
        p.feed(0xC3, &mut out);
        assert!(!p.is_idle());
    }

    #[test]
    fn escape_restarts_from_every_state_that_can_hold_one() {
        // `ESC` cancels whatever is in flight, from each state that can be mid-sequence.
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
        assert_eq!(printed(&run("\x1b(BX")), "X");
        assert_eq!(printed(&run("\x1b#8X")), "X");
        assert_eq!(printed(&run("\x1b7X")), "X");
    }

    #[test]
    fn an_escape_inside_a_sequence_starts_a_new_one() {
        let ops = run("\x1b[1\x1b[31mX");
        assert_eq!(ops, alloc::vec![Op::Attr(Sgr::Foreground(Colour::Ansi(Ansi::Red))), Op::Print('X')]);
    }
}
