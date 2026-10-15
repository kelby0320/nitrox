//! **A HID keyboard's report as key presses and releases** (Phase 6 Parts B.2 and B.7), by a
//! [`Layout`]: where its report keeps its modifiers and its keys.
//!
//! A boot-protocol keyboard has one layout, [`Layout::BOOT`] (HID 1.11 Appendix B.1): a modifier
//! bitmap, a reserved byte, and up to six key usages from the Keyboard/Keypad page (0x07), in no
//! order. **A keyboard in report protocol has the layout its report descriptor gives** (B.7): the
//! same array, or **a bitmap** with a bit for each key, which is how a keyboard that reports every
//! key held at once says it — a Corsair K70's, whose boot interface sends its bitmap whatever
//! protocol it is asked for. Either way a report is the keyboard's **whole state**: it is read into
//! the set of usages held, and decoded against the set before — what left was released, what joined
//! was pressed.

use super::mouse::Field;
use crate::libkern::input::{InputEvent, KEY_PRESS, KEY_RELEASE};

/// **The most events one decode gives**: twenty changes and the `SYN`. A boot report's worst case —
/// eight modifiers and six keys each way — fits whole; a bitmap's larger change is given in batches
/// ([`decode`] says when more remain).
pub const EVENTS_MAX: usize = 8 + 6 + 6 + 1;

/// `ErrorRollOver`: more keys are down than the report can say. In every key slot.
const ERROR_ROLL_OVER: i64 = 0x01;

/// Usages 0x00–0x03 are not keys: none, `ErrorRollOver`, `POSTFail`, `ErrorUndefined`.
const FIRST_KEY: i64 = 0x04;

/// The modifiers' usages, 0xE0–0xE7: left Control, Shift, Alt and GUI, then the right four.
const MODIFIER_FIRST: u8 = 0xE0;

/// Runs of one-bit fields a layout holds: the modifiers' eight and a bitmap's, with room for a
/// keyboard that splits either.
pub const BITS_MAX: usize = 4;

/// **A run of one-bit fields**: bit `offset + i` is usage `first + i`, for `count` of them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Bits {
    pub offset: u16,
    pub first: u8,
    pub count: u16,
}

/// **An array of key slots**: `count` fields of `size` bits from `offset`, each holding a usage as
/// an index from `first`, counted from `logical_min`; a value below it is no key.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Array {
    pub offset: u16,
    pub count: u8,
    pub size: u8,
    pub first: u8,
    pub logical_min: i32,
}

/// **Where a keyboard takes its lights**: an output report whose first three bits are Num Lock,
/// Caps Lock and Scroll Lock, HID's order — the byte a node is written — after its report ID, when
/// the keyboard numbers its reports.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Lights {
    pub report_id: Option<u8>,
}

/// **Where a keyboard's report keeps what this driver reads.** Offsets count bits from the first
/// byte's least significant, after any report ID, as HID packs them.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The report ID that prefixes this report, when the device numbers its reports. A report with
    /// another is not this one.
    pub report_id: Option<u8>,
    /// The keys reported a bit each: the modifiers, and a bitmap's keys.
    pub bits: [Option<Bits>; BITS_MAX],
    /// The keys reported as usages in slots.
    pub array: Option<Array>,
    /// Where its lights go; `None` takes none.
    pub lights: Option<Lights>,
}

impl Layout {
    /// **A boot-protocol keyboard** (HID 1.11 Appendix B.1): the modifiers in the first byte, a
    /// reserved one, then six slots of a byte each; its lights one output byte, with no report ID.
    pub const BOOT: Layout = Layout {
        report_id: None,
        bits: [Some(Bits { offset: 0, first: MODIFIER_FIRST, count: 8 }), None, None, None],
        array: Some(Array { offset: 16, count: 6, size: 8, first: 0, logical_min: 0 }),
        lights: Some(Lights { report_id: None }),
    };

    /// The bytes a report takes: its last field's end, and the report ID before it.
    pub fn report_bytes(&self) -> u16 {
        let bits = self.bits.iter().flatten().map(|b| b.offset as u32 + b.count as u32);
        let array = self.array.map(|a| a.offset as u32 + a.count as u32 * a.size as u32);
        let end = bits.chain(array).max().unwrap_or(0);
        (end.div_ceil(8) + self.report_id.is_some() as u32) as u16
    }

    /// **The usages `report` says are held**, or `None` for a report not to keep: another report
    /// ID's, one too short for its fields, or one saying `ErrorRollOver` — more keys down than it can
    /// tell — which, decoded, would release every held key, so the previous state stands.
    pub fn read(&self, report: &[u8]) -> Option<Held> {
        let data = match self.report_id {
            Some(id) => report.split_first().filter(|(first, _)| **first == id)?.1,
            None => report,
        };
        let mut held = Held::NONE;
        let hold = |usage: i64, held: &mut Held| -> Option<()> {
            if usage == ERROR_ROLL_OVER {
                return None;
            }
            if (FIRST_KEY..=0xFF).contains(&usage) {
                held.set(usage as u8, true);
            }
            Some(())
        };
        for b in self.bits.iter().flatten() {
            for i in 0..b.count as usize {
                let bit = b.offset as usize + i;
                if data.get(bit / 8)? & (1 << (bit % 8)) != 0 {
                    hold(b.first as i64 + i as i64, &mut held)?;
                }
            }
        }
        if let Some(a) = self.array {
            for slot in 0..a.count as u16 {
                let field = Field { offset: a.offset + slot * a.size as u16, size: a.size, signed: a.logical_min < 0 };
                let value = field.read(data)?;
                if value >= a.logical_min {
                    hold(a.first as i64 + (value as i64 - a.logical_min as i64), &mut held)?;
                }
            }
        }
        Some(held)
    }
}

/// **The usages held**, a bit each for the Keyboard/Keypad page's 256 — keys and modifiers alike,
/// the modifiers being usages 0xE0–0xE7.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Held([u8; 32]);

impl Held {
    /// Nothing held.
    pub const NONE: Held = Held([0; 32]);

    fn has(&self, usage: u8) -> bool {
        self.0[usage as usize / 8] & (1 << (usage % 8)) != 0
    }

    fn set(&mut self, usage: u8, on: bool) {
        let bit = 1 << (usage % 8);
        if on {
            self.0[usage as usize / 8] |= bit;
        } else {
            self.0[usage as usize / 8] &= !bit;
        }
    }
}

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

/// The keycode for usage `usage`, a modifier's or a key's.
fn code_of(usage: u8) -> Option<u16> {
    match usage {
        MODIFIER_FIRST..=0xE7 => Some(MODIFIERS[(usage - MODIFIER_FIRST) as usize]),
        _ => keycode(usage),
    }
}

/// **Decode `report` by `layout` against `held`**, the usages held after the last, into `out`:
/// releases first, keys then modifiers, then presses, modifiers then keys — so a key pressed with
/// Shift in one report is shifted — then `SYN_REPORT`. `held` takes each change given. How many
/// events, and whether more remain: a change too large for `out` is given in batches, the same
/// report decoded again for each until none remain. `None` for a report not to keep
/// ([`Layout::read`]); a report that changes nothing gives `Some((0, false))`.
pub fn decode(
    layout: &Layout,
    held: &mut Held,
    report: &[u8],
    time_ns: u64,
    out: &mut [InputEvent; EVENTS_MAX],
) -> Option<(usize, bool)> {
    let now = layout.read(report)?;
    Some(change(held, &now, time_ns, out))
}

/// **What a keyboard held, let go** (Phase 6 Part C): its keys released, then its modifiers, then
/// `SYN_REPORT` — `held` decoded against a report holding nothing, in batches as [`decode`]'s are.
/// What its driver delivers when it departs, so that no key stays down and no repeat runs on.
/// Nothing when nothing was held.
pub fn release_all(held: &mut Held, time_ns: u64, out: &mut [InputEvent; EVENTS_MAX]) -> (usize, bool) {
    change(held, &Held::NONE, time_ns, out)
}

/// `held` moved toward `target` by as many changes as `out` holds, in [`decode`]'s order. A usage
/// with no keycode changes with no event.
fn change(held: &mut Held, target: &Held, time_ns: u64, out: &mut [InputEvent; EVENTS_MAX]) -> (usize, bool) {
    let room = EVENTS_MAX - 1;
    let (mut n, mut more) = (0, false);
    for (release, modifiers) in [(true, false), (true, true), (false, true), (false, false)] {
        for usage in 0..=0xFFu8 {
            let (was, will) = (held.has(usage), target.has(usage));
            if was == will || was != release || (usage >= MODIFIER_FIRST && usage <= 0xE7) != modifiers {
                continue;
            }
            let Some(code) = code_of(usage) else {
                held.set(usage, will);
                continue;
            };
            if n == room {
                more = true;
                continue;
            }
            out[n] = InputEvent::key(code, if release { KEY_RELEASE } else { KEY_PRESS }, time_ns);
            n += 1;
            held.set(usage, will);
        }
    }
    if n > 0 {
        out[n] = InputEvent::syn(time_ns);
        n += 1;
    }
    (n, more)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::input::*;

    /// `report` decoded in boot protocol against what `prev` held.
    fn run(prev: [u8; 8], report: [u8; 8]) -> Option<Vec<(u16, u16, i32)>> {
        let mut held = Layout::BOOT.read(&prev).expect("a report to keep");
        let mut out = [InputEvent::default(); EVENTS_MAX];
        let (n, more) = decode(&Layout::BOOT, &mut held, &report, 5, &mut out)?;
        assert!(!more, "a boot report's change fits one batch");
        Some(out[..n].iter().map(|e| (e.kind, e.code, e.value)).collect())
    }

    /// Every event `report` gives against `held`, batch by batch, each batch ending in a `SYN`.
    fn run_all(layout: &Layout, held: &mut Held, report: &[u8]) -> Option<Vec<(u16, u16, i32)>> {
        let mut got = Vec::new();
        loop {
            let mut out = [InputEvent::default(); EVENTS_MAX];
            let (n, more) = decode(layout, held, report, 5, &mut out)?;
            assert!(n == 0 || out[n - 1].kind == EV_SYN, "a batch ends in its SYN");
            got.extend(out[..n].iter().map(|e| (e.kind, e.code, e.value)));
            if !more {
                return Some(got);
            }
        }
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
        assert_eq!(decode(&Layout::BOOT, &mut Held::NONE.clone(), &[0, 0, 0x04], 0, &mut out), None);
    }

    /// **The worst case fits**: every modifier and six keys released, the rest pressed, in one
    /// report.
    #[test]
    fn the_worst_report_fits_its_buffer() {
        let before = [0xFF, 0, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09];
        let after = [0x00, 0, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F];
        assert_eq!(
            run(before, after).unwrap().len(),
            EVENTS_MAX,
            "six keys and eight modifiers released, six pressed, SYN"
        );
        let mods_on = [0xFF, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(run(after, mods_on).unwrap().len(), 6 + 8 + 1);
    }

    /// **A departing keyboard lets go of everything** (Phase 6 Part C): its keys, then its
    /// modifiers, then `SYN_REPORT`, so a key held when it is unplugged is released, Shift last, and
    /// nothing at all when nothing was held.
    #[test]
    fn a_departing_keyboard_releases_its_keys_then_its_modifiers() {
        let mut held = Layout::BOOT.read(&[0x02, 0, 0x04, 0x05, 0, 0, 0, 0]).unwrap(); // Left Shift, `a` and `b`
        let mut out = [InputEvent::default(); EVENTS_MAX];
        let (n, more) = release_all(&mut held, 5, &mut out);
        assert!(!more);
        assert_eq!(held, Held::NONE, "nothing held after");
        let got: Vec<(u16, u16, i32)> = out[..n].iter().map(|e| (e.kind, e.code, e.value)).collect();
        assert_eq!(
            got,
            // `a` is 30 and `b` 48, evdev's numbering.
            vec![(EV_KEY, 30, KEY_RELEASE), (EV_KEY, 48, KEY_RELEASE), (EV_KEY, KEY_LEFTSHIFT, KEY_RELEASE), SYN]
        );
        assert_eq!(release_all(&mut Held::NONE.clone(), 5, &mut out), (0, false), "nothing held, nothing sent");
    }

    /// A bitmap keyboard's layout: report ID 1, the modifiers, then a bit for each usage from 0 —
    /// the shape of the Corsair K70's (the descriptor's own tests read it from its bytes).
    const BITMAP: Layout = Layout {
        report_id: Some(1),
        bits: [
            Some(Bits { offset: 0, first: 0xE0, count: 8 }),
            Some(Bits { offset: 8, first: 0, count: 152 }),
            None,
            None,
        ],
        array: None,
        lights: Some(Lights { report_id: Some(1) }),
    };

    /// A bitmap report holding `usages` and the modifier byte `mods`.
    fn bitmap(mods: u8, usages: &[u8]) -> [u8; 21] {
        let mut r = [0u8; 21];
        r[0] = 1;
        r[1] = mods;
        for &u in usages {
            r[2 + u as usize / 8] |= 1 << (u % 8);
        }
        r
    }

    /// **A bitmap reads as the keys its bits name**, Shift before the key pressed with it and after
    /// the key released before it, as a boot report's array does.
    #[test]
    fn a_bitmap_report_reads_as_its_keys() {
        let mut held = Held::NONE;
        assert_eq!(run_all(&BITMAP, &mut held, &bitmap(0, &[0x04])), Some(vec![press(30), SYN]));
        let shift_ab = bitmap(0x02, &[0x04, 0x05]);
        assert_eq!(run_all(&BITMAP, &mut held, &shift_ab), Some(vec![press(KEY_LEFTSHIFT), press(48), SYN]));
        let none = bitmap(0, &[]);
        assert_eq!(
            run_all(&BITMAP, &mut held, &none),
            Some(vec![release(30), release(48), release(KEY_LEFTSHIFT), SYN])
        );
        assert_eq!(BITMAP.report_bytes(), 21, "its ID, the modifiers and 19 bytes of bits");
        assert_eq!(Layout::BOOT.report_bytes(), 8);
    }

    /// **Another report's ID is not this one**: the K70 sends its media keys and a mouse on the same
    /// endpoint, as reports 2 and 5. Not kept, and the keys held stay held.
    #[test]
    fn a_report_with_another_id_is_not_kept() {
        let mut held = Held::NONE;
        run_all(&BITMAP, &mut held, &bitmap(0, &[0x04])).unwrap();
        let mut consumer = bitmap(0, &[0x05]);
        consumer[0] = 2;
        assert_eq!(run_all(&BITMAP, &mut held, &consumer), None);
        assert_eq!(run_all(&BITMAP, &mut held, &bitmap(0, &[0x04])), Some(vec![]), "a still held");
        assert_eq!(run_all(&BITMAP, &mut held, &[1, 0, 0]), None, "too short for its bits");
        let mut rollover = bitmap(0, &[0x01]);
        rollover[2] |= 1 << 4;
        assert_eq!(run_all(&BITMAP, &mut held, &rollover), None, "ErrorRollOver's bit");
    }

    /// **A change larger than one batch is given in batches**, each within `EVENTS_MAX` and ending in
    /// its `SYN`, every key pressed once and in order, and the keys held after as the report says —
    /// and a departure lets them all go, in batches too.
    #[test]
    fn a_large_change_is_given_in_batches() {
        let usages: Vec<u8> = (0x04..0x04 + 50).collect();
        let mut held = Held::NONE;
        let mut out = [InputEvent::default(); EVENTS_MAX];
        let first = decode(&BITMAP, &mut held, &bitmap(0xFF, &usages), 5, &mut out).unwrap();
        assert_eq!(first, (EVENTS_MAX, true), "a full batch, and more to come");
        let mut held = Held::NONE;
        let got = run_all(&BITMAP, &mut held, &bitmap(0xFF, &usages)).unwrap();
        let presses: Vec<u16> = got.iter().filter(|e| e.0 == EV_KEY).map(|e| e.1).collect();
        let mut want: Vec<u16> = MODIFIERS.to_vec();
        want.extend(usages.iter().map(|&u| keycode(u).unwrap()));
        assert_eq!(presses, want, "the modifiers, then every key, once each");
        assert_eq!(got.iter().filter(|e| e.0 == EV_SYN).count(), 3, "58 presses in three batches");
        assert_eq!(held, BITMAP.read(&bitmap(0xFF, &usages)).unwrap());
        assert_eq!(run_all(&BITMAP, &mut held, &bitmap(0xFF, &usages)), Some(vec![]), "nothing left");

        let mut released = 0;
        loop {
            let (n, more) = release_all(&mut held, 5, &mut out);
            released += out[..n].iter().filter(|e| e.kind == EV_KEY && e.value == KEY_RELEASE).count();
            if !more {
                break;
            }
        }
        assert_eq!((released, held), (58, Held::NONE));
    }

    /// **An array's slots hold usages counted from its logical minimum**, and a value below it is no
    /// key: an array of 16-bit slots from usage 0x04, its values from 1.
    #[test]
    fn an_array_counts_its_usages_from_its_logical_minimum() {
        let layout = Layout {
            report_id: None,
            bits: [None; BITS_MAX],
            array: Some(Array { offset: 0, count: 2, size: 16, first: 0x04, logical_min: 1 }),
            lights: None,
        };
        let mut held = Held::NONE;
        // Value 1 is usage 0x04, `a`; value 0 is below the minimum.
        assert_eq!(run_all(&layout, &mut held, &[1, 0, 0, 0]), Some(vec![press(30), SYN]));
        assert_eq!(run_all(&layout, &mut held, &[0, 0, 2, 0]), Some(vec![release(30), press(48), SYN]));
    }
}
