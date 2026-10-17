//! Keycode + modifiers → text, for the US layout. A layout is policy and data
//! (`display-substrate.md` §5), so it lives here and not in the kernel. No dead keys, compose
//! sequences or locale.

use librsproto::surface::{LOCK_CAPS, LOCK_NUM, MOD_CTRL, MOD_SHIFT};

/// The printable keys of the US layout, as `(keycode, unshifted, shifted)`; absent keycodes
/// yield `None`.
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

/// The keypad's digits and point, text when Num Lock is on. With it off the interpreter delivers
/// them as navigation keys, so they never reach here.
const KEYPAD_NUM: &[(u16, u8)] = &[
    (71, b'7'),
    (72, b'8'),
    (73, b'9'),
    (75, b'4'),
    (76, b'5'),
    (77, b'6'),
    (79, b'1'),
    (80, b'2'),
    (81, b'3'),
    (82, b'0'),
    (83, b'.'),
];

/// The keypad's operators: text whatever Num Lock says, and folded by nothing.
const KEYPAD_OPS: &[(u16, u8)] = &[(98, b'/'), (55, b'*'), (74, b'-'), (78, b'+')];

/// The character a keycode produces under `modifiers` and `locks` (`KeyEvent::locks`), if any.
///
/// - Caps Lock gives a letter its other case, which Shift reverses; it touches nothing else.
/// - Num Lock makes the keypad's digits and point text; its operators are text either way.
/// - Control folds a letter to the C0 range, so Ctrl-C is `0x03`. A non-letter yields its
///   unmodified character: `Ctrl-1` has no C0 meaning.
pub fn text(keycode: u16, modifiers: u16, locks: u16) -> Option<u8> {
    if let Some(&(_, c)) = KEYPAD_OPS.iter().find(|(k, _)| *k == keycode) {
        return Some(c);
    }
    if let Some(&(_, c)) = KEYPAD_NUM.iter().find(|(k, _)| *k == keycode) {
        return (locks & LOCK_NUM != 0).then_some(c);
    }
    let (lower, upper) = US.iter().find(|(k, _, _)| *k == keycode).map(|&(_, l, u)| (l, u))?;
    let letter = lower.is_ascii_lowercase();
    let shifted = (modifiers & MOD_SHIFT != 0) != (letter && locks & LOCK_CAPS != 0);
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

/// [`text`] with no lock on, for callers with no `KeyEvent` to read locks from.
pub fn to_char(keycode: u16, modifiers: u16) -> Option<u8> {
    text(keycode, modifiers, 0)
}

/// Whether a keycode produces text at all.
pub fn is_text(keycode: u16) -> bool {
    US.iter().any(|(k, _, _)| *k == keycode) || KEYPAD_NUM.iter().chain(KEYPAD_OPS).any(|(k, _)| *k == keycode)
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
        assert_eq!(to_char(2, 0), Some(b'1'));
        assert_eq!(to_char(2, MOD_SHIFT), Some(b'!'));
        assert_eq!(to_char(11, MOD_SHIFT), Some(b')'));
        assert_eq!(to_char(12, MOD_SHIFT), Some(b'_'));
    }

    #[test]
    fn control_folds_letters_to_the_c0_range() {
        assert_eq!(to_char(46, MOD_CTRL), Some(0x03), "ctrl-c");
        assert_eq!(to_char(30, MOD_CTRL), Some(0x01), "ctrl-a");
        assert_eq!(to_char(44, MOD_CTRL), Some(0x1A), "ctrl-z");
    }

    #[test]
    fn control_folds_from_either_case() {
        assert_eq!(to_char(46, MOD_CTRL | MOD_SHIFT), Some(0x03));
    }

    #[test]
    fn control_on_a_non_letter_is_not_invented() {
        assert_eq!(to_char(2, MOD_CTRL), Some(b'1'));
        assert_eq!(to_char(53, MOD_CTRL), Some(b'/'));
    }

    #[test]
    fn keys_with_no_text_report_none_rather_than_a_placeholder() {
        // Escape, F1, the modifiers themselves.
        for k in [1u16, 59, 42, 29, 103] {
            assert_eq!(to_char(k, 0), None, "keycode {k} produces no text");
            assert!(!is_text(k));
        }
    }

    #[test]
    fn alt_and_meta_do_not_change_the_character() {
        // Routing information (a hotkey), not text: folded into bytes, Alt-F would read as `f`.
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
        for (i, &(k, _, _)) in US.iter().enumerate() {
            for &(k2, _, _) in &US[i + 1..] {
                assert_ne!(k, k2, "keycode {k} appears twice");
            }
        }
    }

    #[test]
    fn caps_lock_inverts_a_letters_case_alone() {
        assert_eq!(text(30, 0, LOCK_CAPS), Some(b'A'));
        assert_eq!(text(30, MOD_SHIFT, LOCK_CAPS), Some(b'a'), "Shift reverses it");
        assert_eq!(text(2, 0, LOCK_CAPS), Some(b'1'), "a digit is not a letter");
        assert_eq!(text(2, MOD_SHIFT, LOCK_CAPS), Some(b'!'));
        assert_eq!(text(39, 0, LOCK_CAPS), Some(b';'));
        assert_eq!(text(46, MOD_CTRL, LOCK_CAPS), Some(0x03), "Ctrl-C under Caps Lock");
        assert_eq!(text(30, 0, 0), to_char(30, 0), "no lock is to_char");
    }

    #[test]
    fn the_keypad_is_text_under_num_lock() {
        let num = LOCK_NUM;
        assert_eq!([71, 72, 73, 75, 76, 77, 79, 80, 81, 82, 83].map(|k| text(k, 0, num)), (*b"7894561230.").map(Some));
        assert_eq!(text(79, 0, 0), None, "keypad 1 with Num Lock off");
        assert_eq!(text(79, MOD_SHIFT, num | LOCK_CAPS), Some(b'1'));
        assert_eq!([98, 55, 74, 78].map(|k| text(k, 0, 0)), (*b"/*-+").map(Some));
        assert_eq!(text(55, MOD_CTRL, num), Some(b'*'), "an operator is not folded");
        assert!(is_text(79) && is_text(78));
    }
}
