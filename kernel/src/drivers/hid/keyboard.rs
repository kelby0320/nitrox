//! **A HID keyboard in boot protocol** (Phase 6 Part B.2): its usages as keycodes, and its
//! eight-byte report as key presses and releases.
//!
//! HID 1.11 Appendix B.1: a modifier bitmap, a reserved byte, and up to six key usages from the
//! Keyboard/Keypad page (0x07), in no order. A report is the keyboard's **whole state**, so each is
//! decoded against the one before: what left the array was released, what joined it was pressed.

use crate::libkern::input::{InputEvent, KEY_PRESS, KEY_RELEASE};

/// A boot keyboard report's length.
pub const REPORT_LEN: usize = 8;

/// The most events one report can produce: eight modifiers and six keys each way, and the `SYN`.
pub const EVENTS_MAX: usize = 8 + 6 + 6 + 1;

/// `ErrorRollOver`: more keys are down than the report can say. In every key slot.
const ERROR_ROLL_OVER: u8 = 0x01;

/// **Keyboard/Keypad page usages 0x00–0x65 as keycodes**, `0` for none: the 104- and 105-key
/// layouts, the keypad included. evdev's numbering, as the PS/2 driver's scancode table gives it;
/// Linux's `hid_keyboard[]` (`drivers/hid/hid-input.c`) is the reference for which keycode each
/// usage is. Usages 0x00–0x03 are not keys: none, `ErrorRollOver`, `POSTFail`, `ErrorUndefined`.
const USAGES: [u8; 0x66] = [
    0, 0, 0, 0, 30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, // 0x00: a..l
    50, 49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44, 2, 3, // 0x10: m..z, 1, 2
    4, 5, 6, 7, 8, 9, 10, 11, 28, 1, 14, 15, 57, 12, 13, 26, // 0x20: 3..0, Enter, Esc, ⌫, Tab, Space, - = [
    27, 43, 43, 39, 40, 41, 51, 52, 53, 58, 59, 60, 61, 62, 63, 64, // 0x30: ] \ #, ; ' ` , . /, Caps, F1..F6
    65, 66, 67, 68, 87, 88, 99, 70, 119, 110, 102, 104, 111, 107, 109, 106, // 0x40: F7..F12, PrtSc..Right
    105, 108, 103, 69, 98, 55, 74, 78, 96, 79, 80, 81, 75, 76, 77, 71, // 0x50: Left..Up, NumLock, keypad
    72, 73, 82, 83, 86, 127, // 0x60: keypad 8, 9, 0, ., the 102nd key, Compose
];

/// The eight modifier bits' keycodes, bit 0 first: left Control, Shift, Alt and GUI, then the right
/// four (usages 0xE0–0xE7).
const MODIFIERS: [u16; 8] = [29, 42, 56, 125, 97, 54, 100, 126];

/// The keycode for Keyboard/Keypad usage `usage`, if it is one this table has.
pub fn keycode(usage: u8) -> Option<u16> {
    USAGES.get(usage as usize).copied().filter(|&k| k != 0).map(u16::from)
}

/// **Decode `report` against `prev`**, the last report kept, into `out`: releases first, keys then
/// modifiers, then presses, modifiers then keys — so a key pressed with Shift in one report is
/// shifted — then `SYN_REPORT`. How many events, or `None` for a report not to keep:
/// - **`ErrorRollOver`**: more keys are down than the device can say. Decoding it would release
///   every held key, so the previous state stands;
/// - a report shorter than eight bytes.
///
/// A report that changes nothing gives `Some(0)`.
pub fn decode(
    prev: &[u8; REPORT_LEN],
    report: &[u8],
    time_ns: u64,
    out: &mut [InputEvent; EVENTS_MAX],
) -> Option<usize> {
    let report: &[u8; REPORT_LEN] = report.get(..REPORT_LEN)?.try_into().ok()?;
    if report[2..].contains(&ERROR_ROLL_OVER) {
        return None;
    }
    let mut n = 0;
    let mut push = |e: InputEvent, n: &mut usize| {
        out[*n] = e;
        *n += 1;
    };
    let keys = |r: &[u8; REPORT_LEN]| -> [Option<u16>; 6] { core::array::from_fn(|i| keycode(r[2 + i])) };
    let (was, now) = (keys(prev), keys(report));
    for k in was.iter().flatten() {
        if !now.contains(&Some(*k)) {
            push(InputEvent::key(*k, KEY_RELEASE, time_ns), &mut n);
        }
    }
    for (bit, &code) in MODIFIERS.iter().enumerate() {
        if prev[0] & (1 << bit) != 0 && report[0] & (1 << bit) == 0 {
            push(InputEvent::key(code, KEY_RELEASE, time_ns), &mut n);
        }
    }
    for (bit, &code) in MODIFIERS.iter().enumerate() {
        if prev[0] & (1 << bit) == 0 && report[0] & (1 << bit) != 0 {
            push(InputEvent::key(code, KEY_PRESS, time_ns), &mut n);
        }
    }
    for k in now.iter().flatten() {
        if !was.contains(&Some(*k)) {
            push(InputEvent::key(*k, KEY_PRESS, time_ns), &mut n);
        }
    }
    if n > 0 {
        push(InputEvent::syn(time_ns), &mut n);
    }
    Some(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::input::*;

    fn run(prev: [u8; 8], report: [u8; 8]) -> Option<Vec<(u16, u16, i32)>> {
        let mut out = [InputEvent::default(); EVENTS_MAX];
        let n = decode(&prev, &report, 5, &mut out)?;
        Some(out[..n].iter().map(|e| (e.kind, e.code, e.value)).collect())
    }

    const NONE: [u8; 8] = [0; 8];
    const SYN: (u16, u16, i32) = (EV_SYN, SYN_REPORT, 0);

    fn press(code: u16) -> (u16, u16, i32) {
        (EV_KEY, code, KEY_PRESS)
    }

    fn release(code: u16) -> (u16, u16, i32) {
        (EV_KEY, code, KEY_RELEASE)
    }

    /// **The usages the system's own keys name map to its keycodes**: the letters, the digits, the
    /// keys around them, the arrows, the lock keys and the keypad.
    #[test]
    fn usages_are_evdev_keycodes() {
        assert_eq!(keycode(0x04), Some(30), "a is KEY_A");
        assert_eq!(keycode(0x1D), Some(44), "z is KEY_Z");
        assert_eq!(keycode(0x1E), Some(2), "1 is KEY_1");
        assert_eq!(keycode(0x27), Some(11), "0 is KEY_0");
        assert_eq!(keycode(0x28), Some(KEY_ENTER));
        assert_eq!(keycode(0x29), Some(KEY_ESC));
        assert_eq!(keycode(0x2A), Some(KEY_BACKSPACE));
        assert_eq!(keycode(0x2B), Some(KEY_TAB));
        assert_eq!(keycode(0x2C), Some(KEY_SPACE));
        assert_eq!(keycode(0x39), Some(KEY_CAPSLOCK));
        assert_eq!(keycode(0x3A), Some(KEY_F1));
        assert_eq!(keycode(0x45), Some(88), "F12");
        assert_eq!(keycode(0x47), Some(KEY_SCROLLLOCK));
        assert_eq!(
            [0x49, 0x4A, 0x4B, 0x4C, 0x4D, 0x4E].map(keycode),
            [KEY_INSERT, KEY_HOME, KEY_PAGEUP, KEY_DELETE, KEY_END, KEY_PAGEDOWN].map(Some)
        );
        assert_eq!([0x4F, 0x50, 0x51, 0x52].map(keycode), [KEY_RIGHT, KEY_LEFT, KEY_DOWN, KEY_UP].map(Some));
        assert_eq!(keycode(0x53), Some(KEY_NUMLOCK));
        assert_eq!(keycode(0x54), Some(KEY_KPSLASH));
        assert_eq!(keycode(0x58), Some(KEY_KPENTER));
        assert_eq!(keycode(0x59), Some(79), "keypad 1");
        assert_eq!(keycode(0x5F), Some(71), "keypad 7");
        assert_eq!(keycode(0x62), Some(82), "keypad 0");
        assert_eq!(keycode(0x63), Some(KEY_KPDOT));
        assert_eq!(keycode(0x65), Some(KEY_COMPOSE));
        let left = [KEY_LEFTCTRL, KEY_LEFTSHIFT, KEY_LEFTALT, KEY_LEFTMETA];
        let right = [KEY_RIGHTCTRL, KEY_RIGHTSHIFT, KEY_RIGHTALT, KEY_RIGHTMETA];
        assert_eq!(MODIFIERS[..4], left);
        assert_eq!(MODIFIERS[4..], right);
    }

    /// **No usage is a key it should not be**: 0x00–0x03 are none of them, past the table is
    /// nothing, and every keycode the table gives is inside evdev's key range, below the buttons.
    #[test]
    fn non_keys_and_unknown_usages_have_no_keycode() {
        for u in 0..=3 {
            assert_eq!(keycode(u), None, "usage {u:#x}");
        }
        assert_eq!(keycode(0x66), None, "past the table");
        assert_eq!(keycode(0xE0), None, "a modifier is a bit, not a usage in the array");
        for u in 0..=255u8 {
            if let Some(k) = keycode(u) {
                assert!(k > 0 && k < BTN_LEFT, "usage {u:#x} gives {k}");
            }
        }
    }

    #[test]
    fn a_press_and_a_release() {
        let a = [0, 0, 0x04, 0, 0, 0, 0, 0];
        assert_eq!(run(NONE, a), Some(vec![press(30), SYN]));
        assert_eq!(run(a, NONE), Some(vec![release(30), SYN]));
    }

    /// **Releases before presses**, so a key that left and one that came in the same report do not
    /// overlap; and a key that only moved slots is neither.
    #[test]
    fn releases_come_before_presses_and_a_moved_key_is_still_down() {
        let ab = [0, 0, 0x04, 0x05, 0, 0, 0, 0];
        let bc = [0, 0, 0x05, 0x06, 0, 0, 0, 0];
        assert_eq!(run(ab, bc), Some(vec![release(30), press(46), SYN]));
        let ba = [0, 0, 0x05, 0x04, 0, 0, 0, 0];
        assert_eq!(run(ab, ba), Some(vec![]), "the same keys in another order");
    }

    /// **A modifier is a bit**, pressed before a key in the same report so the key is shifted, and
    /// released after the keys that leave with it.
    #[test]
    fn modifiers_press_first_and_release_last() {
        let shift_a = [0x02, 0, 0x04, 0, 0, 0, 0, 0];
        assert_eq!(run(NONE, shift_a), Some(vec![press(KEY_LEFTSHIFT), press(30), SYN]));
        assert_eq!(run(shift_a, NONE), Some(vec![release(30), release(KEY_LEFTSHIFT), SYN]));
        let right_alt = [0x40, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(run(NONE, right_alt), Some(vec![press(KEY_RIGHTALT), SYN]));
    }

    #[test]
    fn the_same_report_twice_says_nothing() {
        let a = [0x01, 0, 0x04, 0, 0, 0, 0, 0];
        assert_eq!(run(a, a), Some(vec![]));
    }

    /// **`ErrorRollOver` keeps the previous state**: decoded, it would release every held key.
    #[test]
    fn roll_over_is_not_a_report_to_keep() {
        let rollover = [0x02, 0, 1, 1, 1, 1, 1, 1];
        assert_eq!(run([0x02, 0, 0x04, 0x05, 0, 0, 0, 0], rollover), None);
    }

    /// **Six keys at once**, and one more as rollover; a short report is not kept either.
    #[test]
    fn six_keys_and_a_short_report() {
        let six = [0, 0, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let got = run(NONE, six).unwrap();
        assert_eq!(got.len(), 7, "six presses and a SYN: {got:?}");
        let mut out = [InputEvent::default(); EVENTS_MAX];
        assert_eq!(decode(&NONE, &[0, 0, 0x04], 0, &mut out), None);
    }

    /// **The worst case fits**: every modifier and six keys released, the rest pressed, in one
    /// report.
    #[test]
    fn the_worst_report_fits_its_buffer() {
        let before = [0xFF, 0, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let after = [0x00, 0, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F];
        assert_eq!(run(before, after).unwrap().len(), EVENTS_MAX, "six keys and eight modifiers released, six pressed, SYN");
        let mods_on = [0xFF, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(run(after, mods_on).unwrap().len(), 6 + 8 + 1);
    }
}
