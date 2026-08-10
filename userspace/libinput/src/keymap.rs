//! Keycode + modifiers → text. The **US layout**, and the reason a layout is not in the
//! kernel.
//!
//! `display-substrate.md` §5: "A keymap is *policy and data* — layout, locale, dead keys,
//! compose sequences. It has no business in the kernel, and putting it there would make a
//! keyboard layout a kernel rebuild." Putting one copy per application instead would be the
//! same mistake one layer up, which is why this sits in a library both ends can use.
//!
//! ## What this is not
//!
//! Not an input method. No dead keys, no compose sequences, no locale — those are the reasons
//! the boundary is *here* rather than in the kernel, and they get built when something needs
//! them. What exists is enough to type into a terminal, which is Milestone 5's requirement.

use librsproto::surface::{MOD_CTRL, MOD_SHIFT};

/// The printable keys of the US layout, as `(keycode, unshifted, shifted)`.
///
/// Only the keycodes that produce characters. Anything absent — function keys, arrows,
/// modifiers themselves — yields `None`, which is not the same as producing nothing: a caller
/// that wants "the user pressed Home" reads the keycode, and a caller that wants text asks
/// here and gets told there is none.
const US: &[(u16, u8, u8)] = &[
    (2, b'1', b'!'),
    (3, b'2', b'@'),
    (4, b'3', b'#'),
    (5, b'4', b'$'),
    (6, b'5', b'%'),
    (7, b'6', b'^'),
    (8, b'7', b'&'),
    (9, b'8', b'*'),
    (10, b'9', b'('),
    (11, b'0', b')'),
    (12, b'-', b'_'),
    (13, b'=', b'+'),
    (15, b'\t', b'\t'),
    (16, b'q', b'Q'),
    (17, b'w', b'W'),
    (18, b'e', b'E'),
    (19, b'r', b'R'),
    (20, b't', b'T'),
    (21, b'y', b'Y'),
    (22, b'u', b'U'),
    (23, b'i', b'I'),
    (24, b'o', b'O'),
    (25, b'p', b'P'),
    (26, b'[', b'{'),
    (27, b']', b'}'),
    (28, b'\n', b'\n'),
    (30, b'a', b'A'),
    (31, b's', b'S'),
    (32, b'd', b'D'),
    (33, b'f', b'F'),
    (34, b'g', b'G'),
    (35, b'h', b'H'),
    (36, b'j', b'J'),
    (37, b'k', b'K'),
    (38, b'l', b'L'),
    (39, b';', b':'),
    (40, b'\'', b'"'),
    (41, b'`', b'~'),
    (43, b'\\', b'|'),
    (44, b'z', b'Z'),
    (45, b'x', b'X'),
    (46, b'c', b'C'),
    (47, b'v', b'V'),
    (48, b'b', b'B'),
    (49, b'n', b'N'),
    (50, b'm', b'M'),
    (51, b',', b'<'),
    (52, b'.', b'>'),
    (53, b'/', b'?'),
    (57, b' ', b' '),
];

/// The character a keycode produces under `modifiers`, if any.
///
/// **Control folds to the C0 range**, so Ctrl-C is `0x03` — the convention every terminal
/// expects, and the reason the shell can tell Ctrl-C from the letter `c` at all. Only letters
/// fold; `Ctrl-1` has no C0 meaning and yields the unmodified character rather than
/// something invented.
pub fn to_char(keycode: u16, modifiers: u16) -> Option<u8> {
    let (lower, upper) = US.iter().find(|(k, _, _)| *k == keycode).map(|&(_, l, u)| (l, u))?;
    let shifted = modifiers & MOD_SHIFT != 0;
    let base = if shifted { upper } else { lower };
    if modifiers & MOD_CTRL != 0 {
        // Ctrl-A..Ctrl-Z → 0x01..0x1A, from either case.
        let letter = base.to_ascii_lowercase();
        if letter.is_ascii_lowercase() {
            return Some(letter - b'a' + 1);
        }
    }
    Some(base)
}

/// Whether a keycode produces text at all.
///
/// Cheaper than [`to_char`] when the answer is all a caller wants, and clearer at a call
/// site than comparing against `None`.
pub fn is_text(keycode: u16) -> bool {
    US.iter().any(|(k, _, _)| *k == keycode)
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::surface::{MOD_ALT, MOD_META};

    #[test]
    fn letters_map_in_both_cases() {
        assert_eq!(to_char(30, 0), Some(b'a'));
        assert_eq!(to_char(30, MOD_SHIFT), Some(b'A'));
        assert_eq!(to_char(44, 0), Some(b'z'));
        assert_eq!(to_char(44, MOD_SHIFT), Some(b'Z'));
    }

    #[test]
    fn shifted_digits_are_symbols_not_uppercase_digits() {
        // The case a naive `to_ascii_uppercase` gets wrong: shift-1 is `!`, and `1` has no
        // uppercase. A table is why this is right rather than a lucky transformation.
        assert_eq!(to_char(2, 0), Some(b'1'));
        assert_eq!(to_char(2, MOD_SHIFT), Some(b'!'));
        assert_eq!(to_char(11, MOD_SHIFT), Some(b')'));
        assert_eq!(to_char(12, MOD_SHIFT), Some(b'_'));
    }

    #[test]
    fn control_folds_letters_to_the_c0_range() {
        // Ctrl-C is 0x03, which is how a shell tells it from the letter `c` — the whole
        // reason a byte stream needed modifiers in the first place.
        assert_eq!(to_char(46, MOD_CTRL), Some(0x03), "ctrl-c");
        assert_eq!(to_char(30, MOD_CTRL), Some(0x01), "ctrl-a");
        assert_eq!(to_char(44, MOD_CTRL), Some(0x1A), "ctrl-z");
    }

    #[test]
    fn control_folds_from_either_case() {
        // Ctrl-Shift-C is still 0x03: the fold is about the letter, not its case.
        assert_eq!(to_char(46, MOD_CTRL | MOD_SHIFT), Some(0x03));
    }

    #[test]
    fn control_on_a_non_letter_is_not_invented() {
        // `Ctrl-1` has no C0 meaning; returning the character beats returning a made-up
        // control code that some consumer would then have to special-case back out.
        assert_eq!(to_char(2, MOD_CTRL), Some(b'1'));
        assert_eq!(to_char(53, MOD_CTRL), Some(b'/'));
    }

    #[test]
    fn keys_with_no_text_report_none_rather_than_a_placeholder() {
        // Escape, F1, the modifiers themselves. A caller wanting "the user pressed Home"
        // reads the keycode; a caller wanting text is told there is none.
        for k in [1u16, 59, 42, 29, 103] {
            assert_eq!(to_char(k, 0), None, "keycode {k} produces no text");
            assert!(!is_text(k));
        }
    }

    #[test]
    fn alt_and_meta_do_not_change_the_character() {
        // They are routing information — a hotkey, a menu mnemonic — not text. Folding them
        // into bytes is what makes Alt-F indistinguishable from a stray `f`.
        assert_eq!(to_char(33, MOD_ALT), Some(b'f'));
        assert_eq!(to_char(33, MOD_META), Some(b'f'));
        assert_eq!(to_char(33, MOD_ALT | MOD_SHIFT), Some(b'F'));
    }

    #[test]
    fn enter_tab_and_space_are_text() {
        assert_eq!(to_char(28, 0), Some(b'\n'));
        assert_eq!(to_char(15, 0), Some(b'\t'));
        assert_eq!(to_char(57, 0), Some(b' '));
        assert_eq!(to_char(57, MOD_SHIFT), Some(b' '), "shift-space is still a space");
    }

    #[test]
    fn the_table_has_no_duplicate_keycodes() {
        // A duplicate makes one entry unreachable and is invisible at a glance in a
        // fifty-row table.
        for (i, &(k, _, _)) in US.iter().enumerate() {
            for &(k2, _, _) in &US[i + 1..] {
                assert_ne!(k, k2, "keycode {k} appears twice");
            }
        }
    }
}
