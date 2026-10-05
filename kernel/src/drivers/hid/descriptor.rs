//! **A HID report descriptor, read for a mouse's layout** (Phase 6 Part B.3): where its report
//! keeps its buttons, X, Y and wheel, so a mouse in report protocol is read by what it says.
//!
//! HID 1.11 §6.2.2: a descriptor is a sequence of items, each a prefix byte — its size, type and
//! tag — and up to four bytes of data. *Global* items (usage page, logical extents, report size,
//! count and ID) hold until changed; *local* ones (usages) describe the next *main* item only; a
//! main item — Input, Output, Feature, Collection, End Collection — consumes them. Each Input item
//! is `count` fields of `size` bits, packed from the least significant bit, counted per report ID.
//!
//! **Only what a plain mouse needs is read**, and anything else gives `None`, which runs the mouse in
//! boot protocol instead: the first application collection whose usage is *Mouse* or *Pointer*,
//! one input report with relative X and Y, its buttons 1–3, and a wheel if it has one. A horizontal
//! wheel and buttons past the third are walked past, since nothing above the kernel carries them.
//! Every length is checked against the bytes in hand, as every other descriptor reader in this
//! driver checks: a descriptor that runs past them is read no further.

use super::mouse::{Field, Layout};

/// Usage pages.
const PAGE_GENERIC_DESKTOP: u16 = 0x01;
const PAGE_BUTTON: u16 = 0x09;

/// Generic Desktop usages.
const USAGE_POINTER: u16 = 0x01;
const USAGE_MOUSE: u16 = 0x02;
const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;
const USAGE_WHEEL: u16 = 0x38;

/// Main item tags (type 0).
const MAIN_INPUT: u8 = 0x8;
const MAIN_OUTPUT: u8 = 0x9;
const MAIN_FEATURE: u8 = 0xB;
const MAIN_COLLECTION: u8 = 0xA;
const MAIN_END_COLLECTION: u8 = 0xC;
/// Global item tags (type 1).
const GLOBAL_USAGE_PAGE: u8 = 0x0;
const GLOBAL_LOGICAL_MIN: u8 = 0x1;
const GLOBAL_REPORT_SIZE: u8 = 0x7;
const GLOBAL_REPORT_ID: u8 = 0x8;
const GLOBAL_REPORT_COUNT: u8 = 0x9;
const GLOBAL_PUSH: u8 = 0xA;
const GLOBAL_POP: u8 = 0xB;
/// Local item tags (type 2).
const LOCAL_USAGE: u8 = 0x0;
const LOCAL_USAGE_MIN: u8 = 0x1;
const LOCAL_USAGE_MAX: u8 = 0x2;
/// A collection's type: Application.
const COLLECTION_APPLICATION: u32 = 0x01;
/// An Input item's flags: Constant, and Variable rather than Array.
const INPUT_CONSTANT: u32 = 1 << 0;
const INPUT_VARIABLE: u32 = 1 << 1;
const INPUT_RELATIVE: u32 = 1 << 2;

/// Usages a main item takes, one local list at most this long.
const USAGES_MAX: usize = 16;
/// Report IDs whose bit offsets are tracked.
const IDS_MAX: usize = 16;
/// Push depth.
const STACK_MAX: usize = 4;
/// The longest report a layout may describe, in bits: what a TRB asks for, 64 bytes.
const REPORT_BITS_MAX: u32 = 64 * 8;
/// The longest input report the walk accepts at all, in bits: a report descriptor names the length
/// of reports a device may send on any interface, and one longer than a page is no mouse's.
const REPORT_BITS_LIMIT: u32 = 4096 * 8;

/// The global items that hold until changed.
#[derive(Copy, Clone, Default)]
struct Globals {
    page: u16,
    logical_min: i32,
    size: u32,
    count: u32,
    id: u8,
}

/// **The mouse layout `desc` describes**, or `None` for one that does not describe a plain mouse,
/// or does not parse.
pub fn mouse_layout(desc: &[u8]) -> Option<Layout> {
    let mut g = Globals::default();
    let mut stack = [Globals::default(); STACK_MAX];
    let mut depth = 0usize;
    // Local items.
    let mut usages = [0u32; USAGES_MAX];
    let mut n_usages = 0usize;
    let mut usage_min: Option<u32> = None;
    let mut usage_max: Option<u32> = None;
    // Bits so far in each report, by ID.
    let mut offsets = [(0u8, 0u32); IDS_MAX];
    let mut n_ids = 0usize;
    let mut uses_ids = false;
    // Collections: how deep, and how deep the mouse's application collection began.
    let mut collection_depth = 0u32;
    let mut mouse_at: Option<u32> = None;
    let mut mouse_done = false;
    // What is found, in the first input report that has X.
    let mut found = Found::default();

    let mut i = 0usize;
    while i < desc.len() {
        let prefix = desc[i];
        if prefix == 0xFE {
            // A long item: its data size is the next byte. Walked past.
            let len = *desc.get(i + 1)? as usize;
            i += 3 + len;
            continue;
        }
        let size = match prefix & 0x3 {
            3 => 4,
            s => s as usize,
        };
        let data_bytes = desc.get(i + 1..i + 1 + size)?;
        let (tag, kind) = (prefix >> 4, (prefix >> 2) & 0x3);
        let unsigned = data_bytes.iter().rev().fold(0u32, |v, &b| v << 8 | b as u32);
        let signed = match size {
            1 => unsigned as u8 as i8 as i32,
            2 => unsigned as u16 as i16 as i32,
            _ => unsigned as i32,
        };
        i += 1 + size;
        match kind {
            0 => {
                match tag {
                    MAIN_COLLECTION => {
                        collection_depth += 1;
                        let usage = usages.first().copied().filter(|_| n_usages > 0);
                        let is_mouse = matches!(usage, Some(u) if u == usage_of(PAGE_GENERIC_DESKTOP, USAGE_MOUSE)
                            || u == usage_of(PAGE_GENERIC_DESKTOP, USAGE_POINTER));
                        if unsigned == COLLECTION_APPLICATION && mouse_at.is_none() && !mouse_done && is_mouse {
                            mouse_at = Some(collection_depth);
                        }
                    }
                    MAIN_END_COLLECTION => {
                        if mouse_at == Some(collection_depth) {
                            mouse_at = None;
                            mouse_done = true;
                        }
                        collection_depth = collection_depth.saturating_sub(1);
                    }
                    MAIN_INPUT => {
                        uses_ids |= g.id != 0;
                        let slot = id_slot(&mut offsets, &mut n_ids, g.id)?;
                        let start = offsets[slot].1;
                        // **A device's sizes are taken as untrusted**: a count and size whose
                        // product overflows, or a report past any a TRB could carry, is not a plain
                        // mouse's, and the walk ends rather than overflow or loop for ever.
                        let end = g.size.checked_mul(g.count).and_then(|bits| start.checked_add(bits))?;
                        if end > REPORT_BITS_LIMIT {
                            return None;
                        }
                        let in_mouse = mouse_at.is_some();
                        // **A field of no bits reads nothing**, so an item of them is passed over
                        // whatever its count says. Its product above is zero, so no count is too
                        // large for either guard, and walking its fields was a loop as long as the
                        // count: 19 s for one item (PR #358 review). With a bit or more per field,
                        // the limit above bounds every report's fields, and so the walk.
                        if in_mouse
                            && g.size > 0
                            && unsigned & INPUT_CONSTANT == 0
                            && unsigned & INPUT_VARIABLE != 0
                        {
                            for f in 0..g.count {
                                let usage = if let Some(min) = usage_min {
                                    min.saturating_add(f)
                                } else if n_usages > 0 {
                                    usages[(f as usize).min(n_usages - 1)]
                                } else {
                                    continue;
                                };
                                if usage_max.is_some_and(|m| usage > m) {
                                    continue;
                                }
                                let field = Field {
                                    offset: (start + f * g.size) as u16,
                                    size: g.size.min(255) as u8,
                                    signed: g.logical_min < 0,
                                };
                                found.take(usage, field, unsigned & INPUT_RELATIVE != 0, g.id);
                            }
                        }
                        offsets[slot].1 = end;
                    }
                    MAIN_OUTPUT | MAIN_FEATURE => {}
                    _ => {}
                }
                // A main item consumes the local items.
                n_usages = 0;
                usage_min = None;
                usage_max = None;
            }
            1 => match tag {
                GLOBAL_USAGE_PAGE => g.page = unsigned as u16,
                GLOBAL_LOGICAL_MIN => g.logical_min = signed,
                GLOBAL_REPORT_SIZE => g.size = unsigned,
                GLOBAL_REPORT_ID => g.id = unsigned as u8,
                GLOBAL_REPORT_COUNT => g.count = unsigned,
                GLOBAL_PUSH => {
                    *stack.get_mut(depth)? = g;
                    depth += 1;
                }
                GLOBAL_POP => {
                    depth = depth.checked_sub(1)?;
                    g = stack[depth];
                }
                _ => {}
            },
            2 => {
                // A usage of one or two bytes is on the current page; of four, it names its own.
                let full = if size == 4 { unsigned } else { usage_of(g.page, unsigned as u16) };
                match tag {
                    LOCAL_USAGE => {
                        if n_usages < USAGES_MAX {
                            usages[n_usages] = full;
                            n_usages += 1;
                        }
                    }
                    LOCAL_USAGE_MIN => usage_min = Some(full),
                    LOCAL_USAGE_MAX => usage_max = Some(full),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    found.layout(uses_ids)
}

/// A usage as page and ID together, as a four-byte usage item gives it.
const fn usage_of(page: u16, id: u16) -> u32 {
    (page as u32) << 16 | id as u32
}

/// The slot tracking report `id`'s offset, made if new. `None` past [`IDS_MAX`] IDs.
fn id_slot(offsets: &mut [(u8, u32); IDS_MAX], n: &mut usize, id: u8) -> Option<usize> {
    if let Some(s) = offsets[..*n].iter().position(|&(i, _)| i == id) {
        return Some(s);
    }
    if *n == IDS_MAX {
        return None;
    }
    offsets[*n] = (id, 0);
    *n += 1;
    Some(*n - 1)
}

/// What the walk found: the first X's report, and its fields.
#[derive(Default)]
struct Found {
    /// The report the layout is in: the one X was found in. Fields from another are not taken.
    id: Option<u8>,
    buttons: [Option<Field>; 3],
    x: Option<Field>,
    y: Option<Field>,
    wheel: Option<Field>,
    /// An axis was absolute, which this layout cannot decode as motion.
    absolute: bool,
}

impl Found {
    /// Take `field`, whose usage is `usage`, from report `id`.
    fn take(&mut self, usage: u32, field: Field, relative: bool, id: u8) {
        let page = (usage >> 16) as u16;
        let what = usage as u16;
        let axis = page == PAGE_GENERIC_DESKTOP && matches!(what, USAGE_X | USAGE_Y | USAGE_WHEEL);
        if axis && self.id.is_none() && what == USAGE_X {
            self.id = Some(id);
        }
        if self.id.is_some_and(|i| i != id) {
            return;
        }
        if axis && !relative {
            self.absolute = true;
        }
        match (page, what) {
            (PAGE_BUTTON, b @ 1..=3) => {
                let slot = &mut self.buttons[b as usize - 1];
                slot.get_or_insert(field);
            }
            (PAGE_GENERIC_DESKTOP, USAGE_X) => {
                self.x.get_or_insert(field);
            }
            (PAGE_GENERIC_DESKTOP, USAGE_Y) => {
                self.y.get_or_insert(field);
            }
            (PAGE_GENERIC_DESKTOP, USAGE_WHEEL) => {
                self.wheel.get_or_insert(field);
            }
            _ => {}
        }
    }

    /// **The layout, if it is a plain mouse's**: relative X and Y, a button, each field within 32
    /// bits and inside a report a TRB can carry.
    fn layout(self, uses_ids: bool) -> Option<Layout> {
        let (x, y) = (self.x?, self.y?);
        if self.absolute || self.buttons.iter().all(Option::is_none) {
            return None;
        }
        let fields = [Some(x), Some(y), self.wheel].into_iter().chain(self.buttons).flatten();
        for f in fields {
            let room = REPORT_BITS_MAX - if uses_ids { 8 } else { 0 };
            if f.size == 0 || f.size > 32 || f.offset as u32 + f.size as u32 > room {
                return None;
            }
        }
        Some(Layout {
            report_id: if uses_ids { self.id } else { None },
            buttons: self.buttons,
            x,
            y,
            wheel: self.wheel,
            wheel_negated: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **QEMU's `usb-mouse`** (`qemu_mouse_hid_report_descriptor`, `hw/usb/dev-hid.c`): five
    /// buttons, three bits of padding, then X, Y and the wheel as signed bytes, with no report ID.
    const QEMU_MOUSE: [u8; 52] = [
        0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x05,
        0x15, 0x00, 0x25, 0x01, 0x95, 0x05, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x03, 0x81, 0x01,
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x03,
        0x81, 0x06, 0xC0, 0xC0,
    ];

    /// QEMU's `usb-kbd`: a keyboard, not a mouse.
    const QEMU_KEYBOARD: [u8; 63] = [
        0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x75, 0x01, 0x95, 0x08, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7,
        0x15, 0x00, 0x25, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
        0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
        0x75, 0x08, 0x15, 0x00, 0x25, 0xFF, 0x05, 0x07, 0x19, 0x00, 0x29, 0xFF, 0x81, 0x00, 0xC0,
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

    /// **The shape many real mice have in report protocol**: report ID 2, sixteen buttons, X and Y
    /// as 12-bit fields packed into three bytes, a wheel, and a horizontal wheel on the Consumer page
    /// — which is walked past.
    #[test]
    fn a_numbered_report_with_twelve_bit_axes() {
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
        assert_eq!(l.buttons, [Some(Field::bit(0)), Some(Field::bit(1)), Some(Field::bit(2))], "the first three of sixteen");
        assert_eq!((l.x, l.y), (signed(16, 12), signed(28, 12)));
        assert_eq!(l.wheel, Some(signed(40, 8)));
    }

    /// **16-bit axes and no wheel**: still a mouse, without one.
    #[test]
    fn sixteen_bit_axes_and_no_wheel() {
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

    /// **Not a plain mouse**: no X, absolute axes (a tablet), or no button — each runs in boot
    /// protocol instead.
    #[test]
    fn what_is_not_a_plain_mouse_gives_no_layout() {
        // QEMU's mouse with X's usage changed to Z (0x32): no X.
        let mut no_x = QEMU_MOUSE;
        no_x[35] = 0x32;
        assert_eq!(mouse_layout(&no_x), None);
        // Its axes absolute: Input (Data, Variable, Absolute).
        let mut absolute = QEMU_MOUSE;
        absolute[49] = 0x02;
        assert_eq!(mouse_layout(&absolute), None);
        // Its buttons constant: no button.
        let mut no_buttons = QEMU_MOUSE;
        no_buttons[25] = 0x03;
        assert_eq!(mouse_layout(&no_buttons), None);
    }

    /// **A device's sizes are untrusted** (Phase 6 Part B.3): a count and size whose product
    /// overflows ends the walk with no layout rather than overflowing, which panics in a debug
    /// kernel; a report longer than a page ends it at once, where the field loop would otherwise
    /// run two billion times at boot, held here to a second; and so does an item whose fields have
    /// no bits, however many it says it has.
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
        // sees them (PR #358 review). Walked, it took 19 s.
        let mut d = QEMU_MOUSE[..44].to_vec();
        d.extend_from_slice(&[0x75, 0x00, 0x97, 0xFF, 0xFF, 0xFF, 0xFF, 0x81, 0x06, 0xC0, 0xC0]);
        let start = std::time::Instant::now();
        assert_eq!(mouse_layout(&d), None, "axes of no bits are no axes");
        assert!(start.elapsed() < std::time::Duration::from_secs(1), "the item was passed over at once");
    }

    /// **Only the mouse's collection is read** (PR #358 review). A report may hold another
    /// collection's fields first — here a joystick's, with relative X and Y of its own — and they push
    /// the mouse's fields along, since one report is every input item with its ID; but they are not
    /// the mouse's axes.
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

    /// **A descriptor that runs past its bytes is read no further**, and a Pop with nothing pushed
    /// ends it.
    #[test]
    fn a_descriptor_that_runs_past_its_bytes_gives_none() {
        assert_eq!(mouse_layout(&QEMU_MOUSE[..49]), None, "cut inside the axes' Input item");
        assert_eq!(mouse_layout(&[0x26, 0xFF]), None, "a two-byte item with one byte");
        assert_eq!(mouse_layout(&[0xB4]), None, "Pop with an empty stack");
        assert_eq!(mouse_layout(&[]), None);
    }
}
