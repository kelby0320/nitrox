//! **A HID report descriptor, read for a mouse's or a keyboard's layout**: where its report keeps
//! a mouse's buttons, X, Y and wheel, or a keyboard's modifiers and keys.
//!
//! HID 1.11 §6.2.2: a descriptor is a sequence of items, each a prefix byte — its size, type and
//! tag — and up to four bytes of data. *Global* items (usage page, logical extents, report size,
//! count and ID) hold until changed; *local* ones (usages) describe the next *main* item only; a
//! main item — Input, Output, Feature, Collection, End Collection — consumes them. Each Input item
//! is `count` fields of `size` bits, packed from the least significant bit, counted per report ID;
//! Output items count their own reports the same way.
//!
//! **One walk reads the items** ([`walk`]) and hands each collection and each Input and Output item
//! to a reader of the device's kind, so every length a device states is checked in one place: a
//! descriptor that runs past its bytes is read no further.
//!
//! **Only what this driver needs is read**, and anything else gives `None`, which runs the device in
//! boot protocol instead:
//! - **a mouse** ([`mouse_layout`]): the first application collection whose usage is *Mouse* or
//!   *Pointer*, one input report with relative X and Y, its buttons 1–3, and a wheel if it has one.
//!   A horizontal wheel and buttons past the third are walked past, since nothing above the kernel
//!   carries them;
//! - **a keyboard** ([`keyboard_layout`]): the first application collection whose usage is
//!   *Keyboard* or *Keypad*, the Keyboard page's fields of one input report — a bit each, as the
//!   modifiers and an NKRO bitmap are, or an array of slots — and its Num, Caps and Scroll Lock
//!   lights if they are an output report's first three bits. Consumer keys are walked past.

use super::keyboard::{self, Array, Bits, Lights};
use super::mouse::{Field, Layout};

const PAGE_GENERIC_DESKTOP: u16 = 0x01;
const PAGE_KEYBOARD: u16 = 0x07;
const PAGE_LEDS: u16 = 0x08;
const PAGE_BUTTON: u16 = 0x09;

const USAGE_POINTER: u16 = 0x01;
const USAGE_MOUSE: u16 = 0x02;
const USAGE_KEYBOARD: u16 = 0x06;
const USAGE_KEYPAD: u16 = 0x07;
const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;
const USAGE_WHEEL: u16 = 0x38;

/// LED usages: Num Lock, Caps Lock, Scroll Lock — the first three bits of a boot keyboard's lights.
const LED_NUM: u16 = 0x01;
const LED_SCROLL: u16 = 0x03;

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
/// An Input or Output item's flags: Constant, Variable rather than Array, and Relative.
const INPUT_CONSTANT: u32 = 1 << 0;
const INPUT_VARIABLE: u32 = 1 << 1;
const INPUT_RELATIVE: u32 = 1 << 2;

/// Usages a main item takes, one local list at most this long.
const USAGES_MAX: usize = 16;
/// Report IDs whose bit offsets are tracked, of each kind.
const IDS_MAX: usize = 16;
const STACK_MAX: usize = 4;
/// The longest report a layout may describe, in bits: what a TRB asks for, 64 bytes.
const REPORT_BITS_MAX: u32 = 64 * 8;
/// The longest report the walk accepts at all, in bits: a report descriptor names the length of
/// reports a device may send on any interface, and one longer than a page is no mouse's or
/// keyboard's.
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

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Main {
    Input,
    Output,
}

/// **One Input or Output item, as the walk meets it**: `count` fields of `size` bits from bit
/// `start` of report `id` of its kind, counted after any report ID; its flags; and the logical
/// minimum and usages in force.
struct Item<'a> {
    main: Main,
    id: u8,
    start: u32,
    size: u32,
    count: u32,
    flags: u32,
    logical_min: i32,
    usages: &'a [u32],
    usage_min: Option<u32>,
    usage_max: Option<u32>,
}

impl Item<'_> {
    /// **Field `f`'s usage**: from the usage minimum, or the list's (its last stands for the rest).
    fn usage(&self, f: u32) -> Option<u32> {
        let usage = match (self.usage_min, self.usages.last()) {
            (Some(min), _) => min.saturating_add(f),
            (None, Some(_)) => self.usages[(f as usize).min(self.usages.len() - 1)],
            (None, None) => return None,
        };
        if self.usage_max.is_some_and(|m| usage > m) { None } else { Some(usage) }
    }

    /// Whether its fields are data variables: a value each, not padding or an array of usages.
    fn variable(&self) -> bool {
        self.flags & INPUT_CONSTANT == 0 && self.flags & INPUT_VARIABLE != 0
    }
}

enum Event<'a> {
    Open { kind: u32, usage: Option<u32> },
    Close,
    Item(Item<'a>),
}

/// **Walk `desc`**, handing `visit` each collection as it opens and closes and each Input and Output
/// item whose fields have a bit or more. Whether any Input item has a report ID — whether input
/// reports are prefixed by one — or `None` for a descriptor that does not parse: one that runs past
/// its bytes, a Pop with nothing pushed, input reports under more than [`IDS_MAX`] IDs, or an input
/// report whose size overflows or runs past [`REPORT_BITS_LIMIT`].
///
/// **A device's sizes are taken as untrusted**: a count and size whose product overflows, or a
/// report past any a TRB could carry, ends the walk rather than overflow or loop for ever. **A
/// field of no bits reads nothing**, so an item of them is passed over whatever its count says: its
/// product is zero, so no count is too large for either guard, and walking its fields would loop
/// as long as the count. An output report that runs long, or a seventeenth output ID, is not an
/// error: its items are passed over, since only a keyboard's lights are read from one.
fn walk(desc: &[u8], mut visit: impl FnMut(Event<'_>)) -> Option<bool> {
    let mut g = Globals::default();
    let mut stack = [Globals::default(); STACK_MAX];
    let mut depth = 0usize;
    let mut usages = [0u32; USAGES_MAX];
    let mut n_usages = 0usize;
    let mut usage_min: Option<u32> = None;
    let mut usage_max: Option<u32> = None;
    // Bits so far in each report, by ID: input reports, then output ones.
    let mut inputs = [(0u8, 0u32); IDS_MAX];
    let mut n_inputs = 0usize;
    let mut outputs = [(0u8, 0u32); IDS_MAX];
    let mut n_outputs = 0usize;
    let mut uses_ids = false;

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
                        let usage = usages.first().copied().filter(|_| n_usages > 0);
                        visit(Event::Open { kind: unsigned, usage });
                    }
                    MAIN_END_COLLECTION => visit(Event::Close),
                    MAIN_INPUT | MAIN_OUTPUT => {
                        let (main, offsets, n) = if tag == MAIN_INPUT {
                            uses_ids |= g.id != 0;
                            (Main::Input, &mut inputs, &mut n_inputs)
                        } else {
                            (Main::Output, &mut outputs, &mut n_outputs)
                        };
                        let slot = id_slot(offsets, n, g.id);
                        let end = slot.and_then(|s| {
                            let end = g.size.checked_mul(g.count).and_then(|bits| offsets[s].1.checked_add(bits))?;
                            (end <= REPORT_BITS_LIMIT).then_some((s, end))
                        });
                        match (main, end) {
                            (_, Some((s, end))) => {
                                if g.size > 0 {
                                    visit(Event::Item(Item {
                                        main,
                                        id: g.id,
                                        start: offsets[s].1,
                                        size: g.size,
                                        count: g.count,
                                        flags: unsigned,
                                        logical_min: g.logical_min,
                                        usages: &usages[..n_usages],
                                        usage_min,
                                        usage_max,
                                    }));
                                }
                                offsets[s].1 = end;
                            }
                            (Main::Input, None) => return None,
                            // Its report's later items start past the limit, so are passed over too.
                            (Main::Output, None) => {
                                if let Some(s) = slot {
                                    offsets[s].1 = REPORT_BITS_LIMIT + 1;
                                }
                            }
                        }
                    }
                    MAIN_FEATURE => {}
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
    Some(uses_ids)
}

/// **The application collection a reader wants**, followed as the walk opens and closes them: the
/// first whose usage `wanted` accepts, and only it.
#[derive(Default)]
struct Within {
    depth: u32,
    at: Option<u32>,
    done: bool,
}

impl Within {
    /// Follow `event`; whether the walk is now inside the wanted collection.
    fn follow(&mut self, event: &Event<'_>, wanted: impl Fn(u32) -> bool) -> bool {
        match *event {
            Event::Open { kind, usage } => {
                self.depth += 1;
                if kind == COLLECTION_APPLICATION && self.at.is_none() && !self.done && usage.is_some_and(&wanted) {
                    self.at = Some(self.depth);
                }
            }
            Event::Close => {
                if self.at == Some(self.depth) {
                    self.at = None;
                    self.done = true;
                }
                self.depth = self.depth.saturating_sub(1);
            }
            Event::Item(_) => {}
        }
        self.at.is_some()
    }
}

/// **The mouse layout `desc` describes**, or `None` for one that does not describe a plain mouse,
/// or does not parse.
pub fn mouse_layout(desc: &[u8]) -> Option<Layout> {
    let mut within = Within::default();
    let mut found = Found::default();
    let is_mouse =
        |u: u32| u == usage_of(PAGE_GENERIC_DESKTOP, USAGE_MOUSE) || u == usage_of(PAGE_GENERIC_DESKTOP, USAGE_POINTER);
    let uses_ids = walk(desc, |event| {
        let inside = within.follow(&event, is_mouse);
        let Event::Item(item) = event else { return };
        if !inside || item.main != Main::Input || !item.variable() {
            return;
        }
        for f in 0..item.count {
            let Some(usage) = item.usage(f) else { continue };
            let field = Field {
                offset: (item.start + f * item.size) as u16,
                size: item.size.min(255) as u8,
                signed: item.logical_min < 0,
            };
            found.take(usage, field, item.flags & INPUT_RELATIVE != 0, item.id);
        }
    })?;
    found.layout(uses_ids)
}

/// **The keyboard layout `desc` describes**, or `None` for one this driver does
/// not read — no keyboard collection, keys in no field it knows, more runs of bits than a layout
/// holds, a report past any a TRB could carry — or one that does not parse.
pub fn keyboard_layout(desc: &[u8]) -> Option<keyboard::Layout> {
    let mut within = Within::default();
    let mut keys = Keys::default();
    let is_keyboard = |u: u32| {
        u == usage_of(PAGE_GENERIC_DESKTOP, USAGE_KEYBOARD) || u == usage_of(PAGE_GENERIC_DESKTOP, USAGE_KEYPAD)
    };
    let uses_ids = walk(desc, |event| {
        let inside = within.follow(&event, is_keyboard);
        if let Event::Item(item) = event
            && inside
        {
            keys.take(&item);
        }
    })?;
    keys.layout(uses_ids)
}

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

/// **What the keyboard walk found**: the key report's ID and fields, and its
/// lights.
#[derive(Default)]
struct Keys {
    /// The input report the keys are in: the first with a Keyboard-page field. Fields from another
    /// are not taken.
    id: Option<u8>,
    bits: [Option<Bits>; keyboard::BITS_MAX],
    array: Option<Array>,
    /// A key field this driver does not read: more runs of bits than a layout holds, a second array,
    /// or a variable field wider than a bit. The layout is refused rather than read in part.
    unread: bool,
    /// The output report the lights are in, and each light's bit there: Num, Caps, Scroll Lock.
    lights_id: Option<u8>,
    lights: [Option<u32>; 3],
}

impl Keys {
    fn take(&mut self, item: &Item<'_>) {
        match item.main {
            Main::Input => self.take_input(item),
            Main::Output => self.take_output(item),
        }
    }

    /// An input item: its Keyboard-page fields, a bit each or an array of slots.
    fn take_input(&mut self, item: &Item<'_>) {
        if item.flags & INPUT_CONSTANT != 0 {
            return;
        }
        let Some(first) = item.usage(0).or(item.usage_min) else { return };
        if (first >> 16) as u16 != PAGE_KEYBOARD {
            return;
        }
        match self.id {
            None => self.id = Some(item.id),
            Some(id) if id != item.id => return,
            Some(_) => {}
        }
        if item.flags & INPUT_VARIABLE == 0 {
            // **An array**: each slot a usage, counted from the usage minimum.
            let first = first & 0xFFFF;
            if self.array.is_some() || item.size > 16 || item.count > 255 || item.usage_min.is_none() || first > 0xFF {
                self.unread = true;
                return;
            }
            self.array = Some(Array {
                offset: item.start as u16,
                count: item.count as u8,
                size: item.size as u8,
                first: first as u8,
                logical_min: item.logical_min,
            });
            return;
        }
        if item.size != 1 {
            self.unread = true;
            return;
        }
        for f in 0..item.count {
            let Some(usage) = item.usage(f) else { continue };
            if (usage >> 16) as u16 != PAGE_KEYBOARD || usage & 0xFFFF > 0xFF {
                continue;
            }
            self.add_bit(item.start + f, usage as u8);
        }
    }

    /// Bit `offset` is key `usage`: the run it continues, or a new one.
    fn add_bit(&mut self, offset: u32, usage: u8) {
        let runs = self.bits.iter_mut().flatten();
        for run in runs {
            if run.offset as u32 + run.count as u32 == offset && run.first as u32 + run.count as u32 == usage as u32 {
                run.count += 1;
                return;
            }
        }
        match self.bits.iter_mut().find(|b| b.is_none()) {
            Some(free) => *free = Some(Bits { offset: offset as u16, first: usage, count: 1 }),
            None => self.unread = true,
        }
    }

    fn take_output(&mut self, item: &Item<'_>) {
        if !item.variable() || item.size != 1 {
            return;
        }
        for f in 0..item.count {
            let Some(usage) = item.usage(f) else { continue };
            let led = usage as u16;
            if (usage >> 16) as u16 != PAGE_LEDS || !(LED_NUM..=LED_SCROLL).contains(&led) {
                continue;
            }
            if self.lights_id.is_some_and(|id| id != item.id) {
                continue;
            }
            self.lights_id = Some(item.id);
            self.lights[(led - LED_NUM) as usize].get_or_insert(item.start + f);
        }
    }

    /// **The layout, if it is one this driver reads**: a field holding the letters — a run of bits
    /// over `a`, or an array — every field inside a report a TRB can carry, and no field left
    /// unread. Lights only where Num, Caps and Scroll Lock are their report's first three bits.
    fn layout(self, uses_ids: bool) -> Option<keyboard::Layout> {
        const A: u32 = 0x04;
        if self.unread {
            return None;
        }
        let letters = self.array.is_some()
            || self.bits.iter().flatten().any(|b| (b.first as u32..b.first as u32 + b.count as u32).contains(&A));
        if !letters {
            return None;
        }
        let room = REPORT_BITS_MAX - if uses_ids { 8 } else { 0 };
        let ends = self.bits.iter().flatten().map(|b| b.offset as u32 + b.count as u32);
        let array_end = self.array.map(|a| a.offset as u32 + a.count as u32 * a.size as u32);
        if ends.chain(array_end).any(|end| end > room) {
            return None;
        }
        let lights = (self.lights == [Some(0), Some(1), Some(2)])
            .then(|| Lights { report_id: self.lights_id.filter(|&id| id != 0) });
        Some(keyboard::Layout {
            report_id: if uses_ids { self.id } else { None },
            bits: self.bits,
            array: self.array,
            lights,
        })
    }
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
            [
                Some(Bits { offset: 0, first: 0xE0, count: 8 }),
                Some(Bits { offset: 8, first: 0, count: 152 }),
                None,
                None
            ]
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
            let keys: Vec<(u16, i32)> =
                out[..n].iter().filter(|e| e.kind == EV_KEY).map(|e| (e.code, e.value)).collect();
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
        scattered.extend_from_slice(&[
            0x09, 0x04, 0x09, 0x06, 0x09, 0x08, 0x09, 0x0A, 0x09, 0x0C, 0x95, 0x05, 0x81, 0x02, 0xC0,
        ]);
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
}
