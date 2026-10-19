//! The input encoder: key events become bytes.
//!
//! [`parse`](crate::parse) turns a program's bytes into grid operations; this turns key events
//! back into the bytes that program reads. Text and the control fold are delegated to
//! `libinput::keymap::text`; this adds the keys whose encoding is a sequence, and two
//! overrides where a terminal disagrees with the keymap:
//!
//! - **Enter is `\r`, not `\n`**: the terminal sends carriage return and the line discipline
//!   decides what a line ending is.
//! - **Backspace is `0x7F` (DEL)**, as every terminal since the VT220 sends, not `0x08`.
//! - **Alt prefixes with `ESC`** (xterm's `metaSendsEscape`), or the modifier would vanish.

use libinput::keymap;
use libkern::abi::{
    KEY_BACKSPACE, KEY_DELETE, KEY_DOWN, KEY_END, KEY_ENTER, KEY_ESC, KEY_HOME, KEY_INSERT, KEY_LEFT, KEY_PAGEDOWN,
    KEY_PAGEUP, KEY_RIGHT, KEY_UP,
};
use librsproto::surface::MOD_ALT;

/// The most bytes one key press can produce.
///
/// The longest reachable is four bytes (`ESC [ 3 ~`); Alt's `ESC` is never added to an encoding
/// that already starts with one. Eight leaves room for modified cursor forms (`ESC [ 1 ; 5 A`).
pub const MAX_ENCODED: usize = 8;

/// The sequence a key sends when it is not a character, or `None` if it is one.
/// The normal cursor-key mode (`DECCKM` off). Application mode swaps `ESC [` for `ESC O`;
/// nothing here sets it, so it must never be sent.
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
        // Not a sequence, but not what the keymap says either. `libinput` delivers the keypad's
        // Enter as `KEY_ENTER`.
        KEY_ENTER => b"\r",
        KEY_BACKSPACE => b"\x7f",
        KEY_ESC => b"\x1b",
        _ => return None,
    })
}

/// Encode one key press into `out`, returning how many bytes it wrote.
/// `out` must be at least [`MAX_ENCODED`] long. A return of `0` means the key sends nothing —
/// a modifier held on its own, or a key with no encoding.
/// Call this for presses and repeats, not releases: the caller has the `pressed` field.
///
/// **Modifiers on the sequence keys are dropped**, so `Ctrl-Left` sends what `Left` sends;
/// nothing here reads xterm's `ESC [ 1 ; 5 D` form.
pub fn encode(keycode: u16, modifiers: u16, locks: u16, out: &mut [u8]) -> usize {
    let alt = modifiers & MOD_ALT != 0;
    let mut n = 0;

    if let Some(seq) = sequence(keycode) {
        // Alt does not prefix a sequence that already starts with `ESC`: `libline::Discipline`
        // would type `[A` into the line from `ESC ESC [ A`.
        if alt && seq[0] != 0x1b {
            out[n] = 0x1b;
            n += 1;
        }
        out[n..n + seq.len()].copy_from_slice(seq);
        return n + seq.len();
    }

    // The locks as the key carried them: Caps Lock's case, and the keypad digits under Num Lock.
    let Some(b) = keymap::text(keycode, modifiers, locks) else {
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
    use librsproto::surface::{LOCK_CAPS, LOCK_NUM, MOD_CTRL, MOD_SHIFT};

    /// Encode and return the bytes.
    fn enc(keycode: u16, modifiers: u16) -> alloc::vec::Vec<u8> {
        let mut out = [0u8; MAX_ENCODED];
        let n = encode(keycode, modifiers, 0, &mut out);
        out[..n].into()
    }

    #[test]
    fn text_and_control_come_from_the_keymap_unchanged() {
        for (code, mods) in [(30u16, 0u16), (30, MOD_SHIFT), (30, MOD_CTRL), (KEY_SPACE, 0)] {
            let expected = keymap::to_char(code, mods).map(|b| alloc::vec![b]).unwrap_or_default();
            assert_eq!(enc(code, mods), expected, "keycode {code} mods {mods:#x}");
        }
        assert_eq!(enc(30, MOD_CTRL), alloc::vec![0x01], "Ctrl-A is not the C0 code");
    }

    #[test]
    fn enter_sends_carriage_return_where_the_keymap_says_newline() {
        // The keymap says `\n` for what the key means; a terminal sends `\r`.
        assert_eq!(keymap::to_char(KEY_ENTER, 0), Some(b'\n'), "the keymap changed under this");
        assert_eq!(enc(KEY_ENTER, 0), alloc::vec![b'\r']);
    }

    #[test]
    fn backspace_sends_del() {
        // Absent from the keymap; `0x7F`, as terminals since the VT220 send.
        assert_eq!(keymap::to_char(KEY_BACKSPACE, 0), None, "the keymap grew a backspace");
        assert_eq!(enc(KEY_BACKSPACE, 0), alloc::vec![0x7f]);
    }

    #[test]
    fn tab_still_comes_from_the_keymap() {
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
        // Nothing enables `DECCKM`, so nothing may send `ESC O A`.
        for k in [KEY_UP, KEY_DOWN, KEY_LEFT, KEY_RIGHT] {
            let b = enc(k, 0);
            assert_eq!(&b[..2], b"\x1b[", "{k} sent an application-mode sequence: {b:x?}");
        }
    }

    #[test]
    fn a_modifier_key_by_itself_sends_nothing() {
        assert_eq!(enc(KEY_LEFTSHIFT, 0), alloc::vec![]);
        assert_eq!(enc(KEY_LEFTSHIFT, MOD_SHIFT), alloc::vec![]);
        assert_eq!(enc(59, 0), alloc::vec![], "F1 invented an encoding");
    }

    #[test]
    fn alt_prefixes_with_escape() {
        assert_eq!(enc(30, MOD_ALT), alloc::vec![0x1b, b'a']);
        assert_eq!(enc(30, MOD_ALT | MOD_CTRL), alloc::vec![0x1b, 0x01]);
    }

    #[test]
    fn alt_does_not_double_an_escape_that_is_already_there() {
        // `ESC ESC [ A` would type `[A` into the line.
        assert_eq!(enc(KEY_UP, MOD_ALT), b"\x1b[A".to_vec());
        assert_eq!(enc(KEY_ESC, MOD_ALT), alloc::vec![0x1b]);
        assert_eq!(enc(KEY_ENTER, MOD_ALT), alloc::vec![0x1b, b'\r']);
        assert_eq!(enc(KEY_BACKSPACE, MOD_ALT), alloc::vec![0x1b, 0x7f]);
    }

    /// The locks a key carries: Caps Lock's case, Shift reversing it, and keypad digits under
    /// Num Lock.
    #[test]
    fn a_keys_locks_reach_its_bytes() {
        let mut out = [0u8; MAX_ENCODED];
        let mut e = |code: u16, mods: u16, locks: u16| {
            let n = encode(code, mods, locks, &mut out);
            out[..n].to_vec()
        };
        assert_eq!(e(30, 0, LOCK_CAPS), alloc::vec![b'A']);
        assert_eq!(e(30, MOD_SHIFT, LOCK_CAPS), alloc::vec![b'a']);
        assert_eq!(e(79, 0, LOCK_NUM), alloc::vec![b'1'], "keypad 1");
        assert_eq!(e(79, 0, 0), alloc::vec![], "keypad 1 with Num Lock off reaches here as End");
        assert_eq!(e(78, MOD_ALT, 0), alloc::vec![0x1b, b'+'], "a keypad operator, Alt-prefixed");
    }

    #[test]
    fn nothing_overruns_the_buffer() {
        for code in 0u16..=0xFF {
            for mods in [0, MOD_ALT, MOD_SHIFT, MOD_CTRL, MOD_ALT | MOD_CTRL | MOD_SHIFT] {
                let mut out = [0u8; MAX_ENCODED];
                let n = encode(code, mods, 0, &mut out);
                assert!(n <= MAX_ENCODED, "keycode {code} mods {mods:#x} wrote {n} bytes");
            }
        }
    }

    /// The round trip: `libline::Discipline` parses what a serial terminal sends, so the two
    /// ends of the wire are checked against each other, for every key `Discipline::Key` names.
    #[test]
    fn the_cursor_keys_survive_a_round_trip_through_the_line_discipline() {
        use libline::{Discipline, Key, Step};

        for (code, expected) in [
            (KEY_UP, Key::Up),
            (KEY_DOWN, Key::Down),
            (KEY_LEFT, Key::Left),
            (KEY_RIGHT, Key::Right),
            (KEY_HOME, Key::Home),
            (KEY_END, Key::End),
            (KEY_DELETE, Key::Delete),
        ] {
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
    fn no_sequence_this_encodes_leaks_into_a_line() {
        // A key the discipline does not act on must be consumed, not accumulated into the line:
        // the four `~` forms are four bytes, and a `~` once reached the shell as `~list /bin`.
        use libline::{Discipline, Step};

        for key in
            [KEY_UP, KEY_DOWN, KEY_LEFT, KEY_RIGHT, KEY_HOME, KEY_END, KEY_INSERT, KEY_DELETE, KEY_PAGEUP, KEY_PAGEDOWN]
        {
            let mut d = Discipline::new();
            for b in enc(key, 0) {
                d.feed(b);
            }
            for b in b"hi" {
                d.feed(*b);
            }
            match d.feed(b'\r') {
                Step::Line { bytes, echo } => {
                    assert_eq!(bytes, b"hi", "keycode {key} leaked into the line");
                    assert_eq!(echo, b"\r\n", "keycode {key} echoed part of its sequence");
                }
                other => panic!("keycode {key}: expected a line, got {other:?}"),
            }
        }
    }
}
