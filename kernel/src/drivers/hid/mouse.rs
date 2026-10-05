//! **A HID mouse's report as events** (Phase 6 Parts B.2 and B.3), by a [`Layout`]: where in the
//! report its buttons, X, Y and wheel are.
//!
//! A boot-protocol mouse has one layout, [`Layout::BOOT`] (HID 1.11 Appendix B.2): buttons in the
//! first byte, X and Y as signed bytes after it, and nothing defined beyond. A mouse in report
//! protocol has the layout its report descriptor gives, which is how a wheel and axes wider than a
//! byte are read (B.3).
//!
//! The events are the PS/2 driver's shape: button changes, then `REL_X`, `REL_Y` and `REL_WHEEL`
//! when non-zero, then `SYN_REPORT`; a report that changes nothing gives none.

use crate::libkern::input::{
    BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, InputEvent, KEY_PRESS, KEY_RELEASE, REL_WHEEL, REL_X, REL_Y,
};

/// The most events one report can produce: three button changes, three axes, and the `SYN`.
pub const EVENTS_MAX: usize = 3 + 3 + 1;

/// **One field of a report**: `size` bits from bit `offset`, counted from the first byte's least
/// significant bit after any report ID, as HID packs them; `signed` when its logical minimum is
/// negative.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub offset: u16,
    pub size: u8,
    pub signed: bool,
}

impl Field {
    /// A byte-aligned 8-bit field, signed or not.
    pub const fn byte(index: u16, signed: bool) -> Field {
        Field { offset: index * 8, size: 8, signed }
    }

    /// A one-bit field: a button.
    pub const fn bit(offset: u16) -> Field {
        Field { offset, size: 1, signed: false }
    }

    /// This field's value in `data`, or `None` if `data` is too short to hold it. Fields wider than
    /// 32 bits are refused by whatever made the layout.
    pub fn read(&self, data: &[u8]) -> Option<i32> {
        let end = self.offset as usize + self.size as usize;
        if self.size == 0 || self.size > 32 || end > data.len() * 8 {
            return None;
        }
        let mut v: u64 = 0;
        for i in 0..self.size as usize {
            let bit = self.offset as usize + i;
            if data[bit / 8] & (1 << (bit % 8)) != 0 {
                v |= 1 << i;
            }
        }
        if self.signed && self.size < 64 && v & (1 << (self.size - 1)) != 0 {
            v |= !0u64 << self.size;
        }
        Some(v as i64 as i32)
    }
}

/// **Where a mouse's report keeps what this driver reads.**
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    /// The report ID that prefixes this report, when the device numbers its reports. A report with
    /// another is not this one.
    pub report_id: Option<u8>,
    /// Buttons 1–3: left, right, middle. One a mouse lacks is `None`.
    pub buttons: [Option<Field>; 3],
    pub x: Field,
    pub y: Field,
    pub wheel: Option<Field>,
    /// Negate the wheel: HID's is positive away from the user, and `REL_WHEEL` here is positive
    /// toward the user (`kernel/src/libkern/input.rs`).
    pub wheel_negated: bool,
}

impl Layout {
    /// **A boot-protocol mouse** (HID 1.11 Appendix B.2): buttons 1–3 in bits 0–2 of the first
    /// byte, then X and Y as signed bytes. What follows is the device's own, so no wheel.
    pub const BOOT: Layout = Layout {
        report_id: None,
        buttons: [Some(Field::bit(0)), Some(Field::bit(1)), Some(Field::bit(2))],
        x: Field::byte(1, true),
        y: Field::byte(2, true),
        wheel: None,
        wheel_negated: true,
    };
}

/// **Decode `report` by `layout`**, against `buttons`, the buttons held after the last report: the
/// events into `out`, and the buttons now held. `None` for a report that is not this layout's —
/// another report ID, or too short for its fields — which changes nothing.
pub fn decode(
    layout: &Layout,
    buttons: u8,
    report: &[u8],
    time_ns: u64,
    out: &mut [InputEvent; EVENTS_MAX],
) -> Option<(usize, u8)> {
    let data = match layout.report_id {
        Some(id) => {
            if report.first() != Some(&id) {
                return None;
            }
            &report[1..]
        }
        None => report,
    };
    let x = layout.x.read(data)?;
    let y = layout.y.read(data)?;
    let wheel = match layout.wheel {
        Some(f) => f.read(data)?,
        None => 0,
    };
    let mut held = 0u8;
    for (i, f) in layout.buttons.iter().enumerate() {
        if let Some(f) = f
            && f.read(data)? != 0
        {
            held |= 1 << i;
        }
    }
    let mut n = 0;
    let mut push = |e: InputEvent, n: &mut usize| {
        out[*n] = e;
        *n += 1;
    };
    for (i, code) in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE].into_iter().enumerate() {
        let (was, now) = (buttons & (1 << i) != 0, held & (1 << i) != 0);
        if was != now {
            push(InputEvent::key(code, if now { KEY_PRESS } else { KEY_RELEASE }, time_ns), &mut n);
        }
    }
    if x != 0 {
        push(InputEvent::rel(REL_X, x, time_ns), &mut n);
    }
    if y != 0 {
        push(InputEvent::rel(REL_Y, y, time_ns), &mut n);
    }
    let wheel = if layout.wheel_negated { wheel.saturating_neg() } else { wheel };
    if wheel != 0 {
        push(InputEvent::rel(REL_WHEEL, wheel, time_ns), &mut n);
    }
    if n > 0 {
        push(InputEvent::syn(time_ns), &mut n);
    }
    Some((n, held))
}

/// **What a mouse held, let go** (Phase 6 Part C): a release for each button `buttons` holds —
/// left, right, middle — then `SYN_REPORT`. What its driver delivers when it departs, ending a drag.
/// **From the decoder's held state, not by decoding an empty report**, which for a layout with a
/// report ID would carry no ID and decode to nothing. Nothing when no button was held.
pub fn release_all(buttons: u8, time_ns: u64, out: &mut [InputEvent; EVENTS_MAX]) -> usize {
    let mut n = 0;
    for (i, code) in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE].into_iter().enumerate() {
        if buttons & (1 << i) != 0 {
            out[n] = InputEvent::key(code, KEY_RELEASE, time_ns);
            n += 1;
        }
    }
    if n > 0 {
        out[n] = InputEvent::syn(time_ns);
        n += 1;
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::libkern::input::*;

    fn run(layout: &Layout, buttons: u8, report: &[u8]) -> Option<(Vec<(u16, u16, i32)>, u8)> {
        let mut out = [InputEvent::default(); EVENTS_MAX];
        let (n, held) = decode(layout, buttons, report, 9, &mut out)?;
        Some((out[..n].iter().map(|e| (e.kind, e.code, e.value)).collect(), held))
    }

    const SYN: (u16, u16, i32) = (EV_SYN, SYN_REPORT, 0);

    /// **Each button, on change**, left right middle as bits 0, 1, 2; held ones stay quiet.
    #[test]
    fn boot_buttons_press_and_release_on_change() {
        let b = &Layout::BOOT;
        assert_eq!(run(b, 0, &[0b001, 0, 0]), Some((vec![(EV_KEY, BTN_LEFT, KEY_PRESS), SYN], 0b001)));
        assert_eq!(run(b, 0b001, &[0b110, 0, 0]), Some((
            vec![(EV_KEY, BTN_LEFT, KEY_RELEASE), (EV_KEY, BTN_RIGHT, KEY_PRESS), (EV_KEY, BTN_MIDDLE, KEY_PRESS), SYN],
            0b110
        )));
        assert_eq!(run(b, 0b110, &[0b110, 0, 0]), Some((vec![], 0b110)), "nothing changed");
    }

    /// **Both axes signed**, and positive Y is down on HID as on `REL_Y`.
    #[test]
    fn boot_axes_are_signed_bytes() {
        let b = &Layout::BOOT;
        assert_eq!(run(b, 0, &[0, 5, 0xFB]), Some((vec![(EV_REL, REL_X, 5), (EV_REL, REL_Y, -5), SYN], 0)));
        assert_eq!(run(b, 0, &[0, 0x81, 0x7F]), Some((vec![(EV_REL, REL_X, -127), (EV_REL, REL_Y, 127), SYN], 0)));
    }

    /// **A boot mouse's fourth byte is not read**: it is the device's own, and QEMU's `usb-mouse`
    /// puts its wheel there.
    #[test]
    fn a_boot_report_reads_three_bytes() {
        assert_eq!(run(&Layout::BOOT, 0, &[0, 0, 0, 0xFF]), Some((vec![], 0)));
        assert_eq!(run(&Layout::BOOT, 0, &[0, 0]), None, "too short for Y");
    }

    /// **A wheel, negated**: HID's wheel is positive away from the user, `REL_WHEEL` here toward —
    /// so one notch toward the user, `-1` in the report, is `+1`, as on the PS/2 wire (PR #357
    /// review).
    #[test]
    fn the_wheel_is_negated_to_this_systems_sign() {
        let wheel = Layout { wheel: Some(Field::byte(3, true)), ..Layout::BOOT };
        assert_eq!(run(&wheel, 0, &[0, 0, 0, 0xFF]), Some((vec![(EV_REL, REL_WHEEL, 1), SYN], 0)));
        assert_eq!(run(&wheel, 0, &[0, 0, 0, 1]), Some((vec![(EV_REL, REL_WHEEL, -1), SYN], 0)));
    }

    /// **A report ID** must match, and the fields count from the byte after it.
    #[test]
    fn a_report_id_selects_and_prefixes() {
        let numbered = Layout { report_id: Some(2), ..Layout::BOOT };
        assert_eq!(run(&numbered, 0, &[2, 1, 3, 0]), Some((vec![(EV_KEY, BTN_LEFT, KEY_PRESS), (EV_REL, REL_X, 3), SYN], 1)));
        assert_eq!(run(&numbered, 0, &[1, 1, 3, 0]), None, "another report");
    }

    /// **Fields that are not bytes**: 12-bit axes packed into three bytes, and 16-bit ones, signed at
    /// their own width.
    #[test]
    fn wide_and_packed_fields_read_at_their_width() {
        let x12 = Field { offset: 8, size: 12, signed: true };
        let y12 = Field { offset: 20, size: 12, signed: true };
        // X = -2 (0xFFE), Y = 300 (0x12C): bytes 0xFE, 0xCF, 0x12 after the buttons.
        let data = [0u8, 0xFE, 0xCF, 0x12];
        assert_eq!(x12.read(&data), Some(-2));
        assert_eq!(y12.read(&data), Some(300));
        let x16 = Field { offset: 8, size: 16, signed: true };
        assert_eq!(x16.read(&[0, 0x00, 0x80]), Some(-32768));
        assert_eq!(Field { offset: 8, size: 16, signed: false }.read(&[0, 0x00, 0x80]), Some(32768));
        assert_eq!(x16.read(&[0, 0x00]), None, "past the report");
    }

    /// **A departing mouse lets go of its buttons** (Phase 6 Part C), from the held state, so it
    /// works for a layout whose reports carry an ID — where an empty report, having no ID, decodes
    /// to nothing at all. Nothing when no button was held.
    #[test]
    fn a_departing_mouse_releases_its_held_buttons_whatever_its_layout() {
        let numbered = Layout { report_id: Some(2), ..Layout::BOOT };
        assert_eq!(run(&numbered, 0b101, &[0, 0, 0, 0]), None, "an empty report is no report of this layout");
        let mut out = [InputEvent::default(); EVENTS_MAX];
        let n = release_all(0b101, 9, &mut out);
        let got: Vec<(u16, u16, i32)> = out[..n].iter().map(|e| (e.kind, e.code, e.value)).collect();
        assert_eq!(got, vec![(EV_KEY, BTN_LEFT, KEY_RELEASE), (EV_KEY, BTN_MIDDLE, KEY_RELEASE), SYN]);
        assert_eq!(release_all(0, 9, &mut out), 0);
    }
}
