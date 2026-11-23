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
mod tests;
