use super::*;

/// **QEMU's `usb-mouse`**: five buttons, padding, then X, Y and the wheel as signed bytes.
#[rustfmt::skip]
const QEMU_MOUSE: [u8; 52] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x05,
    0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03,
    0x81, 0x06, 0xC0, 0xC0,
];

#[rustfmt::skip]
const QEMU_KEYBOARD: [u8; 63] = [
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x75, 0x01, 0x95, 0x08, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7,
    0x15, 0x00, 0x25, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0xFF, 0x05, 0x07, 0x19, 0x00, 0x29, 0xFF, 0x81, 0x00, 0xC0,
];

/// **A Corsair K70 RGB MK.2's boot interface** (`1b1c:1b49`): report 1 a keyboard — the
/// modifiers, a bit for each usage 0x00–0x97, and the three lights as an output — then consumer
/// keys, two vendor reports, and a mouse as report 5. It sends report 1 in any protocol.
#[rustfmt::skip]
const K70: [u8; 192] = [
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00,
    0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x19, 0x00, 0x29, 0x97, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x98, 0x81, 0x02, 0x05, 0x08, 0x19, 0x01, 0x29, 0x03, 0x25, 0x01, 0x75, 0x01,
    0x95, 0x03, 0x91, 0x02, 0x75, 0x05, 0x95, 0x01, 0x91, 0x01, 0xC0, 0x05, 0x0C, 0x09, 0x01, 0xA1,
    0x01, 0x85, 0x02, 0x05, 0x0C, 0x19, 0x00, 0x2A, 0xFF, 0x0F, 0x15, 0x00, 0x26, 0xFF, 0x0F, 0x75,
    0x10, 0x95, 0x02, 0x81, 0x00, 0xC0, 0x06, 0xC0, 0xFF, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x03, 0x15,
    0x00, 0x26, 0xFF, 0x00, 0x95, 0x3F, 0x75, 0x08, 0x09, 0x02, 0x81, 0x02, 0xC0, 0x06, 0xC2, 0xFF,
    0x09, 0x03, 0xA1, 0x01, 0x85, 0x0E, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x95, 0x3F, 0x75, 0x08, 0x09,
    0x03, 0x81, 0x02, 0xC0, 0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x05, 0x09, 0x01, 0xA1, 0x00,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x20, 0x15, 0x00, 0x25, 0x01, 0x95, 0x20, 0x75, 0x01, 0x81, 0x02,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x01, 0x00, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02,
    0x81, 0x06, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xC0, 0xC0,
];

fn signed(offset: u16, size: u8) -> Field {
    Field { offset, size, signed: true }
}

#[test]
fn qemus_mouse_reads_as_buttons_then_signed_bytes() {
    let l = mouse_layout(&QEMU_MOUSE).expect("a plain mouse");
    assert_eq!(l.report_id, None);
    assert_eq!(l.buttons, [Some(Field::bit(0)), Some(Field::bit(1)), Some(Field::bit(2))]);
    assert_eq!((l.x, l.y, l.wheel), (signed(8, 8), signed(16, 8), Some(signed(24, 8))));
    assert!(l.wheel_negated, "HID's wheel is positive away from the user");
}

#[test]
fn a_keyboard_is_not_a_mouse() {
    assert_eq!(mouse_layout(&QEMU_KEYBOARD), None);
}

/// **The shape many real mice have in report protocol**: ID 2, sixteen buttons, 12-bit X and Y
/// packed into three bytes, a wheel, and a horizontal wheel on the Consumer page (walked past).
#[test]
fn a_numbered_report_with_twelve_bit_axes() {
    #[rustfmt::skip]
    let d = [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xA1, 0x00, // ID 2, Pointer
        0x05, 0x09, 0x19, 0x01, 0x29, 0x10, 0x15, 0x00, 0x25, 0x01, 0x95, 0x10, 0x75, 0x01, 0x81, 0x02, // 16 buttons
        0x05, 0x01, 0x16, 0x01, 0xF8, 0x26, 0xFF, 0x07, 0x75, 0x0C, 0x95, 0x02, 0x09, 0x30, 0x09, 0x31,
        0x81, 0x06, // X, Y: 12 bits, -2047..2047
        0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x09, 0x38, 0x81, 0x06, // wheel
        0x05, 0x0C, 0x0A, 0x38, 0x02, 0x95, 0x01, 0x81, 0x06, // AC Pan
        0xC0, 0xC0,
    ];
    let l = mouse_layout(&d).expect("a plain mouse");
    assert_eq!(l.report_id, Some(2));
    assert_eq!(
        l.buttons,
        [Some(Field::bit(0)), Some(Field::bit(1)), Some(Field::bit(2))],
        "the first three of sixteen"
    );
    assert_eq!((l.x, l.y), (signed(16, 12), signed(28, 12)));
    assert_eq!(l.wheel, Some(signed(40, 8)));
}

#[test]
fn sixteen_bit_axes_and_no_wheel() {
    #[rustfmt::skip]
    let d = [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00,
        0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x05, 0x81, 0x03, // padding, constant
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x00, 0x80, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02,
        0x81, 0x06, 0xC0, 0xC0,
    ];
    let l = mouse_layout(&d).expect("a plain mouse");
    assert_eq!((l.x, l.y, l.wheel), (signed(8, 16), signed(24, 16), None));
}

/// **Not a plain mouse**: no X, absolute axes, or no button each run in boot protocol.
#[test]
fn what_is_not_a_plain_mouse_gives_no_layout() {
    // QEMU's mouse with X's usage changed to Z (0x32): no X.
    let mut no_x = QEMU_MOUSE;
    no_x[35] = 0x32;
    assert_eq!(mouse_layout(&no_x), None);
    let mut absolute = QEMU_MOUSE;
    absolute[49] = 0x02;
    assert_eq!(mouse_layout(&absolute), None);
    let mut no_buttons = QEMU_MOUSE;
    no_buttons[25] = 0x03;
    assert_eq!(mouse_layout(&no_buttons), None);
}

/// **A device's sizes are untrusted**: a count and size whose product overflows ends the walk
/// with no layout rather than panicking a debug kernel; a report longer than a page ends it at
/// once; and so does an item whose fields have no bits, however many it says it has.
#[test]
fn sizes_that_overflow_or_run_long_give_no_layout() {
    // The axes' Report Size and Count as four-byte items: their product overflows.
    let mut d = QEMU_MOUSE[..44].to_vec();
    d.extend_from_slice(&[0x77, 0xFF, 0xFF, 0xFF, 0xFF, 0x97, 0xFF, 0xFF, 0xFF, 0xFF, 0x81, 0x06, 0xC0, 0xC0]);
    assert_eq!(mouse_layout(&d), None, "a size and count whose product overflows");
    // One-bit fields, two billion of them, in the mouse's collection.
    let mut d = QEMU_MOUSE[..44].to_vec();
    d.extend_from_slice(&[0x75, 0x01, 0x97, 0xFF, 0xFF, 0xFF, 0x7F, 0x81, 0x06, 0xC0, 0xC0]);
    let start = std::time::Instant::now();
    assert_eq!(mouse_layout(&d), None, "a report longer than a page");
    assert!(start.elapsed() < std::time::Duration::from_secs(1), "the walk ended at once");
    // Fields of no bits, as many as a count can say: their product is zero, so neither guard
    // sees them. Walked, it took 19 s.
    let mut d = QEMU_MOUSE[..44].to_vec();
    d.extend_from_slice(&[0x75, 0x00, 0x97, 0xFF, 0xFF, 0xFF, 0xFF, 0x81, 0x06, 0xC0, 0xC0]);
    let start = std::time::Instant::now();
    assert_eq!(mouse_layout(&d), None, "axes of no bits are no axes");
    assert!(start.elapsed() < std::time::Duration::from_secs(1), "the item was passed over at once");
}

/// **Only the mouse's collection is read**. A joystick's fields before it, with relative X and
/// Y of their own, push the mouse's fields along (one report is every input item with its ID)
/// but are not the mouse's axes.
#[test]
fn axes_outside_the_mouse_collection_are_not_its() {
    let mut d = vec![
        0x05, 0x01, 0x09, 0x04, 0xA1, 0x01, // Generic Desktop, Joystick, Application
        0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, // X, Y
        0xC0,
    ];
    d.extend_from_slice(&QEMU_MOUSE);
    let l = mouse_layout(&d).expect("the mouse after the joystick");
    assert_eq!(l.buttons, [Some(Field::bit(16)), Some(Field::bit(17)), Some(Field::bit(18))]);
    assert_eq!((l.x, l.y, l.wheel), (signed(24, 8), signed(32, 8), Some(signed(40, 8))));
}

/// **A descriptor that runs past its bytes is read no further**; so is a Pop with no push.
#[test]
fn a_descriptor_that_runs_past_its_bytes_gives_none() {
    assert_eq!(mouse_layout(&QEMU_MOUSE[..49]), None, "cut inside the axes' Input item");
    assert_eq!(mouse_layout(&[0x26, 0xFF]), None, "a two-byte item with one byte");
    assert_eq!(mouse_layout(&[0xB4]), None, "Pop with an empty stack");
    assert_eq!(mouse_layout(&[]), None);
}

/// **QEMU's keyboard reads as a boot keyboard**: its descriptor is the boot report's, so report
/// protocol reads it exactly as boot protocol did.
#[test]
fn qemus_keyboard_reads_as_the_boot_layout() {
    assert_eq!(keyboard_layout(&QEMU_KEYBOARD), Some(keyboard::Layout::BOOT));
    assert_eq!(keyboard_layout(&QEMU_MOUSE), None, "a mouse is not a keyboard");
}

/// **The K70's layout**: report 1, the modifiers and a 152-bit bitmap, its lights in output
/// report 1, and nothing from the reports after the keyboard's collection.
#[test]
fn the_k70_reads_as_its_modifiers_and_a_bitmap() {
    let l = keyboard_layout(&K70).expect("a keyboard this driver reads");
    assert_eq!(l.report_id, Some(1));
    assert_eq!(
        l.bits,
        [Some(Bits { offset: 0, first: 0xE0, count: 8 }), Some(Bits { offset: 8, first: 0, count: 152 }), None, None]
    );
    assert_eq!(l.array, None);
    assert_eq!(l.lights, Some(Lights { report_id: Some(1) }));
    assert_eq!(l.report_bytes(), 21, "the 21 bytes a K70 sends");
}

/// **The K70's own reports, as it sent them**: the boot layout read these as `m`, `3` and a
/// Control held throughout; read by its own, what was typed.
#[test]
fn the_k70s_reports_type_what_was_pressed() {
    use crate::libkern::input::*;
    let layout = keyboard_layout(&K70).unwrap();
    let report = |mods: u8, keys: u8| {
        let mut r = [0u8; 21];
        (r[0], r[1], r[2]) = (1, mods, keys);
        r
    };
    let sent = [
        report(0, 0x10),
        report(0, 0),
        report(0, 0x20),
        report(0, 0),
        report(2, 0),
        report(2, 0x10),
        report(2, 0),
        report(0, 0),
    ];
    let (a, b) = (30, 48);
    let want: [&[(u16, i32)]; 8] = [
        &[(a, KEY_PRESS)],
        &[(a, KEY_RELEASE)],
        &[(b, KEY_PRESS)],
        &[(b, KEY_RELEASE)],
        &[(KEY_LEFTSHIFT, KEY_PRESS)],
        &[(a, KEY_PRESS)],
        &[(a, KEY_RELEASE)],
        &[(KEY_LEFTSHIFT, KEY_RELEASE)],
    ];
    let mut held = keyboard::Held::NONE;
    for (r, want) in sent.iter().zip(want) {
        let mut out = [InputEvent::default(); keyboard::EVENTS_MAX];
        let (n, more) = keyboard::decode(&layout, &mut held, r, 7, &mut out).expect("its keyboard report");
        assert!(!more);
        let keys: Vec<(u16, i32)> = out[..n].iter().filter(|e| e.kind == EV_KEY).map(|e| (e.code, e.value)).collect();
        assert_eq!(keys, want, "report {r:02x?}");
    }
    assert_eq!(held, keyboard::Held::NONE, "nothing held at the end");
    let mut out = [InputEvent::default(); keyboard::EVENTS_MAX];
    let mut nothing = keyboard::Held::NONE;
    let (n, _) = keyboard::decode(&keyboard::Layout::BOOT, &mut nothing, &sent[0], 7, &mut out).unwrap();
    let keys: Vec<u16> = out[..n].iter().filter(|e| e.kind == EV_KEY).map(|e| e.code).collect();
    assert_eq!(keys, [KEY_LEFTCTRL, 50], "the fault as the laptop showed it");
}

/// **What is not a keyboard this driver reads gives no layout**; lights anywhere but the first
/// three bits are not taken, and the keys still are.
#[test]
fn what_is_not_a_readable_keyboard_gives_no_layout() {
    let mods_only = [
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01,
        0x95, 0x08, 0x81, 0x02, 0xC0,
    ];
    assert_eq!(keyboard_layout(&mods_only), None, "no letters");
    let mut wide = K70;
    let at = K70.windows(4).position(|w| w == [0x75, 0x01, 0x95, 0x98]).expect("the bitmap's size and count");
    wide[at + 1] = 0x02;
    assert_eq!(keyboard_layout(&wide), None, "a variable key field wider than a bit");
    // Five keys a bit each, no two next to each other: five runs, with the modifiers six.
    let mut scattered = mods_only[..22].to_vec();
    scattered
        .extend_from_slice(&[0x09, 0x04, 0x09, 0x06, 0x09, 0x08, 0x09, 0x0A, 0x09, 0x0C, 0x95, 0x05, 0x81, 0x02, 0xC0]);
    assert_eq!(keyboard_layout(&scattered), None, "more runs than a layout holds");
    let mut four = mods_only[..22].to_vec();
    four.extend_from_slice(&[0x09, 0x04, 0x09, 0x06, 0x09, 0x08, 0x95, 0x03, 0x81, 0x02, 0xC0]);
    assert!(keyboard_layout(&four).is_some(), "four runs fit: the neighbour of the limit");
    // The bitmap pushed past a TRB's 64 bytes by padding before it: with the ID, the modifiers'
    // byte and its 152 bits, 344 bits of padding end it at the 64th byte, and 345 one bit past.
    let padded = |bits: u16| {
        let mut d = K70.to_vec();
        let usage_min = d.windows(2).position(|w| w == [0x19, 0x00]).expect("the bitmap's usage minimum");
        let [lo, hi] = bits.to_le_bytes();
        d.splice(usage_min..usage_min, [0x75, 0x01, 0x96, lo, hi, 0x81, 0x01]);
        d
    };
    assert_eq!(keyboard_layout(&padded(344)).map(|l| l.report_bytes()), Some(64), "ending at the 64th byte");
    assert_eq!(keyboard_layout(&padded(345)), None, "a field past 64 bytes");
    // QEMU's lights with Caps Lock first: the keys read, no lights.
    let mut swapped = QEMU_KEYBOARD;
    let at = QEMU_KEYBOARD.windows(4).position(|w| w == [0x19, 0x01, 0x29, 0x05]).expect("the lights' usages");
    swapped[at + 1] = 0x02;
    let l = keyboard_layout(&swapped).expect("its keys");
    assert_eq!((l.array.is_some(), l.lights), (true, None));
    // All three lights, a bit along: a padding bit before them.
    let mut shifted = QEMU_KEYBOARD.to_vec();
    let leds = shifted.windows(2).position(|w| w == [0x95, 0x05]).expect("the lights' count");
    shifted.splice(leds..leds, [0x95, 0x01, 0x75, 0x01, 0x91, 0x01]);
    let l = keyboard_layout(&shifted).expect("its keys");
    assert_eq!((l.array.is_some(), l.lights), (true, None), "lights not in the first three bits");
}

/// **The keys are one report's**: a second report's keys in the same collection are not taken.
#[test]
fn the_keys_are_the_first_reports() {
    #[rustfmt::skip]
    let d = [
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x85, 0x01, // keyboard, report 1
        0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, // modifiers
        0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x19, 0x00, 0x2A, 0xFF, 0x00, 0x81, 0x00, // six slots
        0x85, 0x02, 0x19, 0x04, 0x29, 0x1D, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x1A, 0x81, 0x02,
        0xC0,
    ];
    let l = keyboard_layout(&d).expect("report 1's keyboard");
    assert_eq!(l.report_id, Some(1));
    assert_eq!(l.bits, [Some(Bits { offset: 0, first: 0xE0, count: 8 }), None, None, None], "none of report 2's");
    assert_eq!(l.array, Some(Array { offset: 8, count: 6, size: 8, first: 0, logical_min: 0 }));
}

/// **Only the keyboard's collection is read**: a Consumer Control's fields before it are not.
#[test]
fn keys_outside_the_keyboard_collection_are_not_its() {
    let mut d = vec![
        0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x85, 0x03, // Consumer Control, report 3
        0x05, 0x07, 0x19, 0x04, 0x29, 0x1D, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x1A, 0x81, 0x02, 0xC0,
    ];
    d.extend_from_slice(&K70);
    let l = keyboard_layout(&d).expect("the keyboard after it");
    assert_eq!(l.report_id, Some(1));
    assert_eq!(l.bits[1], Some(Bits { offset: 8, first: 0, count: 152 }));
}

/// QEMU's mouse with `tail` after it.
fn mouse_then(tail: &[u8]) -> Vec<u8> {
    let mut d = QEMU_MOUSE.to_vec();
    d.extend_from_slice(tail);
    d
}

/// **A corrupt item after a valid mouse gives no layout**: passed over, it would leave the mouse
/// read. Alone, a refusal and a pass both look like "no mouse".
#[test]
fn a_corrupt_item_after_a_mouse_gives_no_layout() {
    assert!(mouse_layout(&mouse_then(&[])).is_some(), "the mouse alone");
    assert_eq!(mouse_layout(&mouse_then(&[0x26, 0xFF])), None, "a two-byte item with one byte");
    assert_eq!(mouse_layout(&mouse_then(&[0xFE])), None, "a long item with no length");
    assert_eq!(mouse_layout(&mouse_then(&[0xB4])), None, "a pop with nothing pushed");
    assert_eq!(mouse_layout(&mouse_then(&[0xA4; 5])), None, "five pushes, one past the stack");
    let four = [[0xA4; 4], [0xB4; 4]].concat();
    assert!(mouse_layout(&mouse_then(&four)).is_some(), "four pushes and four pops");
}

/// **The first mouse collection is the layout's**: a second's is not read, even a field it lacks.
#[test]
fn a_second_mouse_collection_is_not_read() {
    let mut no_wheel = QEMU_MOUSE;
    let at = QEMU_MOUSE.windows(2).position(|w| w == [0x09, 0x38]).expect("the wheel's usage");
    no_wheel[at + 1] = 0x33; // Rx, which a mouse layout does not read
    let first = mouse_layout(&no_wheel).expect("a mouse without a wheel");
    assert_eq!(first.wheel, None);
    let mut two = no_wheel.to_vec();
    two.extend_from_slice(&QEMU_MOUSE);
    assert_eq!(mouse_layout(&two), Some(first), "not the second's wheel");
}

/// **A second key array refuses the keyboard**, rather than replacing the first.
#[test]
fn a_second_key_array_refuses_the_keyboard() {
    let array = &QEMU_KEYBOARD[46..62];
    assert_eq!(array, [0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0xFF, 0x05, 0x07, 0x19, 0x00, 0x29, 0xFF, 0x81, 0x00]);
    let mut two = QEMU_KEYBOARD[..62].to_vec();
    two.extend_from_slice(array);
    two.push(0xC0);
    assert!(keyboard_layout(&QEMU_KEYBOARD).is_some());
    assert_eq!(keyboard_layout(&two), None);
}

/// **Sixteen report IDs are read, and an input in a seventeenth ends the walk.**
#[test]
fn sixteen_report_ids_and_a_seventeenth() {
    #[rustfmt::skip]
    let numbered = [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xA1, 0x00,
        0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06,
        0xC0, 0xC0,
    ];
    let with = |ids: core::ops::RangeInclusive<u8>| {
        let mut d = numbered.to_vec();
        ids.for_each(|id| d.extend_from_slice(&[0x85, id, 0x75, 0x08, 0x95, 0x01, 0x81, 0x01]));
        mouse_layout(&d)
    };
    assert_eq!(with(3..=17).map(|l| l.report_id), Some(Some(2)), "the mouse's and fifteen more");
    assert_eq!(with(3..=18), None, "a seventeenth");
}
