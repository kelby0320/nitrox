//! The input encoder: key events become bytes.
//!
//! The direction a terminal is usually described without. [`parse`](crate::parse) turns a
//! program's bytes into grid operations; this turns a person's key events back into the bytes
//! that program reads. They are two halves of one protocol and they live in one crate for that
//! reason — see the round-trip test at the bottom of this file.
//!
//! ## It delegates rather than reimplements
//!
//! `libinput::keymap::to_char` already maps a keycode and modifiers to a byte, **including the
//! control fold** — `Ctrl-C` is `0x03` there, and its doc says why. What it cannot express is a
//! key whose encoding is a *sequence* rather than a character, which is every cursor and
//! editing key. So this adds those, plus two overrides where a terminal disagrees with a plain
//! keymap, and reimplements nothing: two copies of a C0 fold would drift, and one of them is
//! already tested.
//!
//! ## What a terminal sends that a keymap would not
//!
//! - **Enter is `\r`, not `\n`.** The keymap says `\n` because that is the character the key
//!   *means*; a terminal sends carriage return and lets the line discipline decide what a line
//!   ending is. This is the input-side half of the same split `Grid::line_feed` documents.
//! - **Backspace is `0x7F` (DEL), not `0x08`.** Every terminal since the VT220 sends DEL, and
//!   `Discipline` erases on both — but a program that distinguishes them (readline binds them
//!   separately) sees what it expects.
//! - **Alt prefixes with `ESC`.** `metaSendsEscape` in xterm's terms. Without it a modifier a
//!   person pressed simply vanishes, which is worse than a convention they may not know.

use libinput::keymap;
use libkern::abi::{
    KEY_BACKSPACE, KEY_DELETE, KEY_DOWN, KEY_END, KEY_ENTER, KEY_ESC, KEY_HOME, KEY_INSERT,
    KEY_LEFT, KEY_PAGEDOWN, KEY_PAGEUP, KEY_RIGHT, KEY_UP,
};
use librsproto::surface::MOD_ALT;

/// The most bytes one key press can produce.
///
/// The longest today is `ESC [ 3 ~` at four, plus one for an `ESC` prefix from Alt. Eight
/// leaves room for the modified cursor forms (`ESC [ 1 ; 5 A`) if they ever land, which is the
/// only thing that would grow it.
pub const MAX_ENCODED: usize = 8;

/// The sequence a key sends when it is not a character, or `None` if it is one.
///
/// **The normal (non-application) cursor-key mode**, which is what `DECCKM` off means and what
/// a shell reading a raw terminal expects. Application mode swaps `ESC [` for `ESC O` on the
/// arrows, and nothing here sets it — a terminal that never enables it must never send it, or
/// a program will read a cursor key as a letter.
const fn sequence(keycode: u16) -> Option<&'static [u8]> {
    Some(match keycode {
        KEY_UP => b"\x1b[A",
        KEY_DOWN => b"\x1b[B",
        KEY_RIGHT => b"\x1b[C",
        KEY_LEFT => b"\x1b[D",
        KEY_HOME => b"\x1b[H",
        KEY_END => b"\x1b[F",
        KEY_INSERT => b"\x1b[2~",
        KEY_DELETE => b"\x1b[3~",
        KEY_PAGEUP => b"\x1b[5~",
        KEY_PAGEDOWN => b"\x1b[6~",
        // Not a sequence, but not what the keymap says either — see the module docs.
        KEY_ENTER => b"\r",
        KEY_BACKSPACE => b"\x7f",
        KEY_ESC => b"\x1b",
        _ => return None,
    })
}

/// Encode one key press into `out`, returning how many bytes it wrote.
///
/// `out` must be at least [`MAX_ENCODED`] long. A return of `0` means the key sends nothing —
/// a modifier held on its own, or a key with no encoding.
///
/// **Call this for presses and repeats, not releases.** A terminal sends on the way down; the
/// caller filters, because it is the caller that has the `pressed` field and the caller that
/// decides whether a repeat should type. Taking the whole `KeyEvent` here instead would couple
/// this crate to the Surface protocol for one field.
///
/// **Modifiers on the sequence keys are dropped**, so `Ctrl-Left` sends what `Left` sends.
/// xterm encodes them as `ESC [ 1 ; 5 D`, which is additive: nothing in this system reads
/// them, and inventing a form no consumer parses would be a guess. See the plan for the one
/// consequence that is not merely missing — `Shift-Enter`.
pub fn encode(keycode: u16, modifiers: u16, out: &mut [u8]) -> usize {
    let alt = modifiers & MOD_ALT != 0;
    let mut n = 0;

    if let Some(seq) = sequence(keycode) {
        // Alt does not prefix a sequence that already starts with `ESC`: `ESC ESC [ A` is not
        // a thing any program parses, and the second `ESC` would cancel the first sequence in
        // this crate's own parser.
        if alt && seq[0] != 0x1b {
            out[n] = 0x1b;
            n += 1;
        }
        out[n..n + seq.len()].copy_from_slice(seq);
        return n + seq.len();
    }

    let Some(b) = keymap::to_char(keycode, modifiers) else {
        return 0;
    };
    if alt {
        out[n] = 0x1b;
        n += 1;
    }
    out[n] = b;
    n + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use libkern::abi::{KEY_LEFTSHIFT, KEY_SPACE, KEY_TAB};
    use librsproto::surface::{MOD_CTRL, MOD_SHIFT};

    /// Encode and return the bytes.
    fn enc(keycode: u16, modifiers: u16) -> alloc::vec::Vec<u8> {
        let mut out = [0u8; MAX_ENCODED];
        let n = encode(keycode, modifiers, &mut out);
        out[..n].into()
    }

    #[test]
    fn text_and_control_come_from_the_keymap_unchanged() {
        // The delegation, stated as a test: this crate must not have its own copy of the C0
        // fold, and the way to show that is that its answers are the keymap's answers.
        for (code, mods) in [(30u16, 0u16), (30, MOD_SHIFT), (30, MOD_CTRL), (KEY_SPACE, 0)] {
            let expected = keymap::to_char(code, mods).map(|b| alloc::vec![b]).unwrap_or_default();
            assert_eq!(enc(code, mods), expected, "keycode {code} mods {mods:#x}");
        }
        // And the fold is really there, so the assertion above is not comparing two `None`s.
        assert_eq!(enc(30, MOD_CTRL), alloc::vec![0x01], "Ctrl-A is not the C0 code");
    }

    #[test]
    fn enter_sends_carriage_return_where_the_keymap_says_newline() {
        // The input-side half of `LF` being index. A terminal sends `\r` and lets the line
        // discipline decide what a line ending is; the keymap says `\n` because that is what
        // the key *means*.
        assert_eq!(keymap::to_char(KEY_ENTER, 0), Some(b'\n'), "the keymap changed under this");
        assert_eq!(enc(KEY_ENTER, 0), alloc::vec![b'\r']);
    }

    #[test]
    fn backspace_sends_del() {
        // Absent from the keymap entirely, and `0x7F` rather than `0x08`: every terminal since
        // the VT220 sends DEL, and a program that binds the two separately sees what it expects.
        assert_eq!(keymap::to_char(KEY_BACKSPACE, 0), None, "the keymap grew a backspace");
        assert_eq!(enc(KEY_BACKSPACE, 0), alloc::vec![0x7f]);
    }

    #[test]
    fn tab_still_comes_from_the_keymap() {
        // Tab *is* in the keymap and is right there, so it must not have acquired an override
        // beside Enter's and Backspace's.
        assert_eq!(enc(KEY_TAB, 0), alloc::vec![b'\t']);
    }

    #[test]
    fn the_cursor_and_editing_keys_send_their_sequences() {
        assert_eq!(enc(KEY_UP, 0), b"\x1b[A".to_vec());
        assert_eq!(enc(KEY_DOWN, 0), b"\x1b[B".to_vec());
        assert_eq!(enc(KEY_RIGHT, 0), b"\x1b[C".to_vec());
        assert_eq!(enc(KEY_LEFT, 0), b"\x1b[D".to_vec());
        assert_eq!(enc(KEY_HOME, 0), b"\x1b[H".to_vec());
        assert_eq!(enc(KEY_END, 0), b"\x1b[F".to_vec());
        assert_eq!(enc(KEY_INSERT, 0), b"\x1b[2~".to_vec());
        assert_eq!(enc(KEY_DELETE, 0), b"\x1b[3~".to_vec());
        assert_eq!(enc(KEY_PAGEUP, 0), b"\x1b[5~".to_vec());
        assert_eq!(enc(KEY_PAGEDOWN, 0), b"\x1b[6~".to_vec());
        assert_eq!(enc(KEY_ESC, 0), alloc::vec![0x1b]);
    }

    #[test]
    fn none_of_the_sequences_are_application_mode() {
        // `DECCKM` on swaps `ESC [` for `ESC O` on the arrows. Nothing here enables it, so
        // nothing here may send it — a program reading `ESC O A` when it expected `ESC [ A`
        // sees a stray `O`.
        for k in [KEY_UP, KEY_DOWN, KEY_LEFT, KEY_RIGHT] {
            let b = enc(k, 0);
            assert_eq!(&b[..2], b"\x1b[", "{k} sent an application-mode sequence: {b:x?}");
        }
    }

    #[test]
    fn a_modifier_key_by_itself_sends_nothing() {
        // Holding shift must not type. `to_char` returns `None` for the modifiers themselves,
        // and this has to pass that through rather than inventing a byte.
        assert_eq!(enc(KEY_LEFTSHIFT, 0), alloc::vec![]);
        assert_eq!(enc(KEY_LEFTSHIFT, MOD_SHIFT), alloc::vec![]);
        // An unmapped keycode likewise — a function key, say.
        assert_eq!(enc(59, 0), alloc::vec![], "F1 invented an encoding");
    }

    #[test]
    fn alt_prefixes_with_escape() {
        // `metaSendsEscape`. Without it the modifier simply vanishes, which is worse than a
        // convention someone may not know.
        assert_eq!(enc(30, MOD_ALT), alloc::vec![0x1b, b'a']);
        assert_eq!(enc(30, MOD_ALT | MOD_CTRL), alloc::vec![0x1b, 0x01]);
    }

    #[test]
    fn alt_does_not_double_an_escape_that_is_already_there() {
        // `ESC ESC [ A` is not a sequence any program parses — and in this crate's own parser
        // the second `ESC` cancels the first, so the whole thing would be swallowed.
        assert_eq!(enc(KEY_UP, MOD_ALT), b"\x1b[A".to_vec());
        assert_eq!(enc(KEY_ESC, MOD_ALT), alloc::vec![0x1b]);
        // The keys whose sequence is a bare character *do* take the prefix, because there is no
        // `ESC` to double.
        assert_eq!(enc(KEY_ENTER, MOD_ALT), alloc::vec![0x1b, b'\r']);
        assert_eq!(enc(KEY_BACKSPACE, MOD_ALT), alloc::vec![0x1b, 0x7f]);
    }

    #[test]
    fn nothing_overruns_the_buffer() {
        // `MAX_ENCODED` is a claim about the longest encoding, and a wrong one is an overrun
        // rather than a wrong answer. Swept over every keycode the tree names, with every
        // combination of the modifier that adds a byte.
        for code in 0u16..=0xFF {
            for mods in [0, MOD_ALT, MOD_SHIFT, MOD_CTRL, MOD_ALT | MOD_CTRL | MOD_SHIFT] {
                let mut out = [0u8; MAX_ENCODED];
                let n = encode(code, mods, &mut out);
                assert!(n <= MAX_ENCODED, "keycode {code} mods {mods:#x} wrote {n} bytes");
            }
        }
    }

    /// **The round trip.** `tty_server::Discipline` parses the input escape sequences a serial
    /// terminal sends, because it was written to read one; this crate produces them. So the two
    /// can be checked against each other rather than each against someone's belief about the
    /// other — two independently-written ends of one wire.
    ///
    /// **It reaches two keys of the eleven this module encodes.** `Discipline::Key` is exactly
    /// `Up` and `Down`, and its CSI state returns `Step::None` for everything else, so this is
    /// a real check of a narrow thing rather than coverage of the encoder. The rest are
    /// asserted against the sequences a terminal is documented to send, above.
    #[test]
    fn the_arrows_survive_a_round_trip_through_the_line_discipline() {
        use tty_server::{Discipline, Key, Step};

        for (code, expected) in [(KEY_UP, Key::Up), (KEY_DOWN, Key::Down)] {
            let mut d = Discipline::new();
            let bytes = enc(code, 0);
            let mut got = None;
            for b in &bytes {
                if let Step::Key(k) = d.feed(*b) {
                    got = Some(k);
                }
            }
            assert_eq!(got, Some(expected), "{bytes:x?} did not decode to {expected:?}");
        }
    }

    #[test]
    fn a_sequence_the_discipline_does_not_know_leaks_nothing_into_a_line() {
        // The other half of the round trip, and the more important one for a shell: `Home` is
        // not a key the discipline acts on, so it must be *consumed* rather than accumulated
        // into the line being edited. A terminal that leaked it would put `[H` in someone's
        // command.
        use tty_server::{Discipline, Step};

        let mut d = Discipline::new();
        for b in enc(KEY_HOME, 0) {
            d.feed(b);
        }
        for b in b"hi" {
            d.feed(*b);
        }
        match d.feed(b'\r') {
            Step::Line { bytes, .. } => assert_eq!(bytes, b"hi", "the sequence leaked into the line"),
            other => panic!("expected a line, got {other:?}"),
        }
    }
}
