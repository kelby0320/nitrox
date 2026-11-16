//! Menu bars, and the drop-downs that were not widgets.
//!
//! The part of a menu that is a function of values: what it *contains*, which one is open, where
//! its rows are, and what a keystroke does to that. Opening the window is the application's,
//! through [`window::Child`](crate::window::Child).

use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

use librsproto::surface::{KeyEvent, MOD_ALT, MOD_CTRL, MOD_SHIFT};

use libdraw::format::Rgb;
use libdraw::geom::{Rect, Size};

use crate::element::{
    Element, Insets, TextSize, center_v, column, fill, ink, padding, rounded_fill, row, scaled, sized, stack, text,
    wash,
};
use crate::widget::{Theme, menu_bar, menu_item, popup_frame};

/// A keyboard shortcut — **one value, two readers**: the label a person sees and the match a
/// handler makes, so the two cannot drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Accel {
    /// `MOD_CTRL` / `MOD_SHIFT` / `MOD_ALT`, or'd.
    pub mods: u16,
    /// The `EV_KEY` code, as `libinput`'s table gives it.
    pub key: u16,
    /// How the key prints: `"C"`, `"F4"`; **given rather than derived**, as a keycode-to-name table
    /// is a second keymap.
    pub key_label: &'static str,
}

impl Accel {
    /// `Ctrl+<key>`.
    pub const fn ctrl(key: u16, key_label: &'static str) -> Self {
        Self { mods: MOD_CTRL, key, key_label }
    }

    /// `Ctrl+Shift+<key>`.
    pub const fn ctrl_shift(key: u16, key_label: &'static str) -> Self {
        Self { mods: MOD_CTRL | MOD_SHIFT, key, key_label }
    }

    /// The keycode this chord is on, for a caller building the event it describes.
    pub fn key(&self) -> u16 {
        self.key
    }

    /// The modifiers this chord holds — `MOD_CTRL` and friends, or'd.
    pub fn mods(&self) -> u16 {
        self.mods
    }

    /// How it reads in a menu: `Ctrl+Shift+T`.
    pub fn label(&self) -> String {
        let mut s = String::new();
        if self.mods & MOD_CTRL != 0 {
            s.push_str("Ctrl+");
        }
        if self.mods & MOD_ALT != 0 {
            s.push_str("Alt+");
        }
        if self.mods & MOD_SHIFT != 0 {
            s.push_str("Shift+");
        }
        s.push_str(self.key_label);
        s
    }

    /// Whether `ev` is this chord being pressed; **exact on the modifiers, not a subset test**, so
    /// `Ctrl+T` does not fire on `Ctrl+Shift+T`.
    pub fn matches(&self, ev: &KeyEvent) -> bool {
        const KNOWN: u16 = MOD_CTRL | MOD_SHIFT | MOD_ALT;
        ev.pressed != 0 && ev.keycode == self.key && (ev.modifiers & KNOWN) == self.mods
    }
}

/// One row of an open menu.
pub enum Item<Msg> {
    /// A row that does something. Disabled rows are drawn dimmed and do not answer a press.
    Action {
        /// What the row reads; **borrowed or owned**, as the shell's labels come from desktop
        /// entries and a `&'static str` would force leaking.
        label: Cow<'static, str>,
        /// The chord shown on its right, if it has one.
        accel: Option<Accel>,
        /// Dim text in the chord's column, for a row whose annotation is not a chord; a chord wins
        /// where a row has both.
        hint: Option<Cow<'static, str>>,
        /// A square of colour in the leading column, before the label; the column widens for the
        /// whole menu when any row has one.
        swatch: Option<Rgb>,
        /// What choosing it means.
        msg: Msg,
        /// Whether it can be chosen now.
        enabled: bool,
        /// Whether the row shows a mark: **a menu that sets something must say what it is set to**.
        marked: bool,
        /// Whether choosing it destroys something — drawn in `deny`.
        destructive: bool,
    },
    /// A horizontal rule between groups.
    Separator,
}

impl<Msg> Item<Msg> {
    /// An enabled row with an accelerator.
    pub fn new(label: impl Into<Cow<'static, str>>, accel: Accel, msg: Msg) -> Self {
        Self::row(label.into(), Some(accel), msg)
    }

    /// An enabled row with no chord.
    pub fn plain(label: impl Into<Cow<'static, str>>, msg: Msg) -> Self {
        Self::row(label.into(), None, msg)
    }

    /// The one place every field of a row is named, so a field added later is added here.
    fn row(label: Cow<'static, str>, accel: Option<Accel>, msg: Msg) -> Self {
        Item::Action { label, accel, hint: None, swatch: None, msg, enabled: true, marked: false, destructive: false }
    }

    /// The same row, greyed and unpressable.
    pub fn enabled(mut self, on: bool) -> Self {
        if let Item::Action { enabled, .. } = &mut self {
            *enabled = on;
        }
        self
    }

    /// The same row, marked or not — the state a *setting* row is in.
    pub fn marked(mut self, on: bool) -> Self {
        if let Item::Action { marked, .. } = &mut self {
            *marked = on;
        }
        self
    }

    /// The same row, drawn as destroying something — in `deny`.
    pub fn destructive(mut self, on: bool) -> Self {
        if let Item::Action { destructive, .. } = &mut self {
            *destructive = on;
        }
        self
    }

    /// The same row, with dim text on its right — see the `hint` field.
    pub fn hint(mut self, text: impl Into<Cow<'static, str>>) -> Self {
        if let Item::Action { hint, .. } = &mut self {
            *hint = Some(text.into());
        }
        self
    }

    /// The same row, with a swatch of `colour` before its label — see the `swatch` field.
    pub fn swatch(mut self, colour: Rgb) -> Self {
        if let Item::Action { swatch, .. } = &mut self {
            *swatch = Some(colour);
        }
        self
    }
}

/// One menu: the word on the bar, and what drops from it.
pub struct Menu<Msg> {
    /// The bar's label — "File".
    pub title: &'static str,
    /// Its rows, top to bottom.
    pub items: Vec<Item<Msg>>,
}

/// Which menu is open, where the bar's words are, and which row the keyboard is on.
///
/// **A bar menu and a context menu are one state**, so a window shows one at a time: opening
/// either closes the other.
#[derive(Clone, Default)]
pub struct MenuState {
    open: Option<usize>,
    /// Each bar word's rectangle, captured from the layout so a popup knows where to hang.
    anchors: Vec<Option<Rect>>,
    cursor: Option<usize>,
    /// Where a context menu was asked, window-local.
    context: Option<(i32, i32)>,
    /// Bumped by each context menu's opening, so a `Dismissed` closes only the one it names.
    serial: u32,
    /// A bar menu a `Dismissed` closed while the press that caused it was still down, until every
    /// button is up: the press may be on its own word, whose release would otherwise open it again.
    dismissed: Option<usize>,
    /// A button is down on the window, as its last pointer event said.
    held: bool,
    /// The bar menu shown was reached by Left or Right, not by a click on its word.
    by_key: bool,
}

/// Which menu a window shows: a bar menu by index, or a context menu by its opening's serial.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Shown {
    /// The bar's menu at this index.
    Bar(usize),
    /// A context menu, the serial of its opening.
    Context(u32),
}

/// What a keystroke did to an open menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KeyOutcome {
    /// Nothing here wanted it; the application should handle it.
    Ignored,
    /// The menu changed — reopen or redraw it.
    Changed,
    /// The menu closed without choosing anything.
    Dismissed,
    /// A row was chosen, and the menu is **already closed**, so it carries the menu as well as the
    /// row.
    Chose {
        /// Which menu was open, as an index into the slice passed to [`MenuState::key`].
        menu: usize,
        /// The row, as an index into that menu's items.
        item: usize,
    },
}

impl MenuState {
    /// A closed bar over `count` menus.
    pub fn new(count: usize) -> Self {
        Self { anchors: alloc::vec![None; count], ..Self::default() }
    }

    /// Which menu is shown, if any.
    pub fn shown(&self) -> Option<Shown> {
        self.open.map(Shown::Bar).or(self.context.map(|_| Shown::Context(self.serial)))
    }

    /// The menu shown, and where it hangs: a bar menu under its word, a context menu at its point.
    pub fn shown_at(&self) -> Option<(Shown, (i32, i32))> {
        match self.shown()? {
            Shown::Bar(i) => Some((Shown::Bar(i), self.anchor()?)),
            shown => Some((shown, self.context?)),
        }
    }

    /// Where the context menu shown was asked, window-local.
    pub fn context(&self) -> Option<(i32, i32)> {
        self.context
    }

    /// Open a context menu at `(x, y)`, window-local, closing any bar menu.
    pub fn open_context(&mut self, x: i32, y: i32) {
        self.open = None;
        self.context = Some((x, y));
        self.serial = self.serial.wrapping_add(1);
        self.cursor = None;
        self.dismissed = None;
    }

    /// A press landed outside the window showing `shown`: close it, if it is still what is shown,
    /// or if the bar menu shown since was reached by the keyboard rather than chosen by this click.
    ///
    /// **The compositor sends it after the press**, so the owner has already been told the button is
    /// down: a dismissal read after the release has nothing to hold back.
    pub fn dismissed(&mut self, shown: Shown) {
        let now = self.shown();
        if now == Some(shown) {
            if let Shown::Bar(i) = shown
                && self.held
            {
                self.dismissed = Some(i);
            }
            self.close();
        } else if matches!((now, shown), (Some(Shown::Bar(_)), Shown::Bar(_))) && self.by_key {
            self.close();
        }
    }

    /// The buttons the window's latest pointer event held; the owner calls it after routing each.
    /// With none held, a gesture that dismissed a menu has ended.
    pub fn buttons(&mut self, held: u16) {
        self.held = held != 0;
        if !self.held {
            self.dismissed = None;
        }
    }

    /// Which menu is open.
    pub fn open(&self) -> Option<usize> {
        self.open
    }

    /// Where the open menu should hang from — the bottom-left of its bar word.
    pub fn anchor(&self) -> Option<(i32, i32)> {
        let i = self.open?;
        let r = (*self.anchors.get(i)?)?;
        Some((r.origin.x, r.bottom() as i32))
    }

    /// The row the keyboard is on.
    pub fn cursor(&self) -> Option<usize> {
        self.cursor
    }

    /// Toggle menu `i`, which is what a press on the bar means; it closes a context menu.
    pub fn toggle(&mut self, i: usize) {
        // The click that dismissed this menu was on its own word, and this is its release.
        if self.dismissed.take() == Some(i) {
            return;
        }
        self.open = if self.open == Some(i) { None } else { Some(i) };
        self.context = None;
        self.cursor = None;
        self.by_key = false;
    }

    /// Close, choosing nothing.
    pub fn close(&mut self) {
        self.open = None;
        self.context = None;
        self.cursor = None;
    }

    /// Put the cursor on the open menu's first selectable row, or none; **for a menu whose rows
    /// change under the keyboard** (the Applications menu narrows as you type), where a stale
    /// cursor could name a row no longer there.
    pub fn select_first<Msg>(&mut self, menus: &[Menu<Msg>]) {
        let items = self.open.and_then(|i| menus.get(i)).map_or(&[][..], |m| m.items.as_slice());
        self.cursor = step(items, None, 1);
    }

    /// Record where each bar word sits, every frame (a fact about the layout, unavailable before
    /// the first one).
    pub fn set_anchors(&mut self, anchors: Vec<Option<Rect>>) {
        self.anchors = anchors;
    }

    /// Drive the open menu from the keyboard.
    ///
    /// **Skips separators and disabled rows**, in both directions. Returns [`KeyOutcome::Ignored`]
    /// when no menu is open, so a caller can pass every key through this first.
    pub fn key<Msg>(&mut self, ev: &KeyEvent, menus: &[Menu<Msg>]) -> KeyOutcome {
        use libkern::abi::{KEY_DOWN, KEY_ENTER, KEY_ESC, KEY_LEFT, KEY_RIGHT, KEY_UP};
        let Some(open) = self.open else { return KeyOutcome::Ignored };
        if ev.pressed == 0 {
            return KeyOutcome::Ignored;
        }
        let items: &[Item<Msg>] = menus.get(open).map_or(&[], |m| m.items.as_slice());
        match ev.keycode {
            KEY_ESC => {
                self.close();
                KeyOutcome::Dismissed
            }
            KEY_DOWN => {
                self.cursor = step(items, self.cursor, 1);
                KeyOutcome::Changed
            }
            KEY_UP => {
                self.cursor = step(items, self.cursor, -1);
                KeyOutcome::Changed
            }
            KEY_LEFT | KEY_RIGHT if menus.len() > 1 => {
                let d = if ev.keycode == KEY_RIGHT { 1 } else { menus.len() - 1 };
                self.open = Some((open + d) % menus.len());
                self.cursor = None;
                self.by_key = true;
                KeyOutcome::Changed
            }
            KEY_ENTER => match self.cursor {
                Some(i) => {
                    self.close();
                    KeyOutcome::Chose { menu: open, item: i }
                }
                // **Enter with nothing selected closes rather than choosing the first row**:
                // guessing fires an action nobody pointed at.
                None => {
                    self.close();
                    KeyOutcome::Dismissed
                }
            },
            _ => KeyOutcome::Ignored,
        }
    }
}

impl MenuState {
    /// Drive the menu shown from the keyboard, and return the message a chosen row sends.
    ///
    /// `bar` is the bar's table and `context` the context menu's, each as its popup drew them. A
    /// context menu has no Left or Right: there is only one.
    pub fn choose<Msg: Clone>(&mut self, ev: &KeyEvent, bar: &[Menu<Msg>], context: Option<&Menu<Msg>>) -> Option<Msg> {
        use libkern::abi::{KEY_DOWN, KEY_ENTER, KEY_ESC, KEY_UP};
        let (items, outcome) = match self.shown()? {
            Shown::Bar(_) => {
                let outcome = self.key(ev, bar);
                let menu = match outcome {
                    KeyOutcome::Chose { menu, .. } => bar.get(menu),
                    _ => None,
                };
                (menu.map_or(&[][..], |m| m.items.as_slice()), outcome)
            }
            Shown::Context(_) => {
                let items = context.map_or(&[][..], |m| m.items.as_slice());
                let outcome = match (ev.pressed != 0).then_some(ev.keycode) {
                    Some(KEY_ESC) => KeyOutcome::Dismissed,
                    Some(KEY_DOWN | KEY_UP) => {
                        self.cursor = step(items, self.cursor, if ev.keycode == KEY_DOWN { 1 } else { -1 });
                        KeyOutcome::Changed
                    }
                    // Enter with nothing selected closes rather than guessing, as on the bar.
                    Some(KEY_ENTER) => {
                        self.cursor.map_or(KeyOutcome::Dismissed, |item| KeyOutcome::Chose { menu: 0, item })
                    }
                    _ => KeyOutcome::Ignored,
                };
                if matches!(outcome, KeyOutcome::Dismissed | KeyOutcome::Chose { .. }) {
                    self.close();
                }
                (items, outcome)
            }
        };
        match (outcome, items) {
            (KeyOutcome::Chose { item, .. }, items) => match items.get(item) {
                Some(Item::Action { msg, enabled: true, .. }) => Some(msg.clone()),
                _ => None,
            },
            _ => None,
        }
    }
}

/// The next selectable row from `from`, wrapping. `None` only if nothing is selectable.
fn step<Msg>(items: &[Item<Msg>], from: Option<usize>, d: isize) -> Option<usize> {
    let n = items.len();
    if n == 0 {
        return None;
    }
    let start = from.map_or(if d > 0 { 0 } else { n - 1 }, |i| ((i as isize + d).rem_euclid(n as isize)) as usize);
    let mut i = start;
    for _ in 0..n {
        if matches!(items[i], Item::Action { enabled: true, .. }) {
            return Some(i);
        }
        i = ((i as isize + d).rem_euclid(n as isize)) as usize;
    }
    None
}

/// The message whose menu item claims this key, if any.
///
/// **Disabled items do not match**; a menu need not be open, as accelerators work when it is shut.
pub fn accel_match<Msg: Clone>(menus: &[Menu<Msg>], ev: &KeyEvent) -> Option<Msg> {
    if ev.pressed == 0 {
        return None;
    }
    menus.iter().flat_map(|m| m.items.iter()).find_map(|it| match it {
        Item::Action { accel: Some(a), msg, enabled: true, .. } if a.matches(ev) => Some(msg.clone()),
        _ => None,
    })
}

/// The bar itself: one word per menu, keyed so a press and an anchor can find it.
///
/// `key_base` is where this bar's element keys start (`key_base + i` per menu).
pub fn bar<Msg: Clone>(
    menus: &[Menu<Msg>],
    state: &MenuState,
    key_base: u64,
    hovered: Option<u64>,
    on_press: impl Fn(usize) -> Msg,
    theme: &Theme,
    height: u32,
) -> Element<Msg> {
    let items = menus
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let key = key_base + i as u64;
            let lit = hovered == Some(key) || state.open() == Some(i);
            menu_item(m.title, on_press(i), lit, theme).key(key)
        })
        .collect::<Vec<_>>();
    menu_bar(items, height, theme)
}

/// The open menu's popup: its rows, framed; `key_base` is where the *rows'* element keys start.
pub fn popup<Msg: Clone>(
    menu: &Menu<Msg>,
    state: &MenuState,
    key_base: u64,
    hovered: Option<u64>,
    theme: &Theme,
) -> Element<Msg> {
    frame(rows(menu, state, key_base, hovered, theme), theme)
}

/// [`popup`], with `header` above the rows (the Applications menu's filter field).
///
/// **The header is the caller's element and the caller keys it**, outside `key_base..key_base +
/// items.len()`: `diff` rejects a column that is partly keyed.
pub fn popup_headed<Msg: Clone>(
    menu: &Menu<Msg>,
    state: &MenuState,
    key_base: u64,
    hovered: Option<u64>,
    header: Element<Msg>,
    theme: &Theme,
) -> Element<Msg> {
    debug_assert!(header.key.is_some(), "a popup's header must be keyed; see popup_headed");
    let key = header.key;
    let mut header = padding(HEADER_PAD, header);
    header.key = key;
    let mut all = Vec::with_capacity(menu.items.len() + 1);
    all.push(header);
    all.extend(rows(menu, state, key_base, hovered, theme));
    frame(all, theme)
}

/// The design's `padding: 5px 0`; rows run edge to edge so a hover runs the popup's width.
fn frame<Msg>(rows: Vec<Element<Msg>>, theme: &Theme) -> Element<Msg> {
    popup_frame(padding(POPUP_PAD, column(rows)), theme)
}

/// One element per item, keyed from `key_base`.
fn rows<Msg: Clone>(
    menu: &Menu<Msg>,
    state: &MenuState,
    key_base: u64,
    hovered: Option<u64>,
    theme: &Theme,
) -> Vec<Element<Msg>> {
    // **The leading column is as wide as the widest thing any row puts in it**, or labels would
    // land on different lines.
    let swatches = menu.items.iter().any(|it| matches!(it, Item::Action { swatch: Some(_), .. }));
    let lead_w = if swatches { SWATCH_W + SWATCH_GAP } else { MARK_W };
    let mut rows: Vec<Element<Msg>> = Vec::with_capacity(menu.items.len());
    for (i, it) in menu.items.iter().enumerate() {
        // **Every row is keyed, separators included**: `diff` rejects a parent whose children are
        // *partly* keyed (`DiffError::MixedKeying`), so an unkeyed rule makes the popup undiffable
        // — a menu that opens and never draws.
        let key = key_base + i as u64;
        match it {
            // **A rule that stretches without knowing the width**: a column gives each child its
            // full width and own measured height, so zero wide and one tall paints across the
            // popup.
            Item::Separator => rows.push(
                padding(SEPARATOR_PAD, sized(Size::new(0, 1), fill(theme.border.blend(theme.background, 128))))
                    .key(key),
            ),
            Item::Action { label, accel, hint, swatch, msg, enabled, marked, destructive } => {
                let lit = *enabled && (hovered == Some(key) || state.cursor() == Some(i));
                // **A flexed empty text is the spacer** that right-aligns the chord. **The mark is
                // a fixed-width column, not a prefix on the label**, or labels would shift as rows
                // changed state; an unmarked row draws *nothing* rather than a space, which would
                // leak into every label a caller reads back.
                let lead: Element<Msg> = match swatch {
                    Some(c) => sized(
                        Size::new(lead_w, 0),
                        center_v(sized(Size::new(SWATCH_W, SWATCH_H), rounded_fill(*c, SWATCH_RADIUS))),
                    ),
                    None => sized(Size::new(lead_w, 0), text(if *marked { MARK } else { "" })),
                };
                let right: Option<String> =
                    accel.map(|a| a.label()).or_else(|| hint.as_ref().map(|h| String::from(h.as_ref())));
                let body: Element<Msg> = match right {
                    Some(r) => row(alloc::vec![
                        lead,
                        text(label.as_ref()),
                        text("").flex(1),
                        padding(ACCEL_PAD, center_v(scaled(TextSize::Small, ink(theme.foreground_dim, text(r)))),),
                    ]),
                    None => row(alloc::vec![lead, text(label.as_ref()), text("").flex(1)]),
                };
                // **A disabled row is dim, and a destructive one is `deny`**; disabled wins.
                let body = if !*enabled {
                    ink(theme.foreground_dim, body)
                } else if *destructive {
                    ink(theme.deny, body)
                } else {
                    body
                };
                let mut e = menu_row(body, lit, *enabled, theme);
                if *enabled {
                    e = e.on_press(msg.clone());
                }
                rows.push(e.key(key));
            }
        }
    }
    rows
}

/// One popup row: its body, highlighted when the pointer or the keyboard is on it; lit is a wash of
/// the accent over the popup's ground, painted first.
fn menu_row<Msg>(body: Element<Msg>, lit: bool, _enabled: bool, theme: &Theme) -> Element<Msg> {
    let mut layers = Vec::with_capacity(2);
    if lit {
        layers.push(wash(theme.accent, theme.scheme.hover_coverage()));
    }
    layers.push(padding(ROW_PAD, body));
    stack(layers)
}

/// The space around a popup row's contents: the design's `padding: 6px 12px`.
const ROW_PAD: Insets = Insets { top: 6, right: 12, bottom: 6, left: 12 };

/// The space above and below a popup's rows: the design's `padding: 5px 0`.
const POPUP_PAD: Insets = Insets { top: 5, right: 0, bottom: 5, left: 0 };

/// The space around a popup's header: the design's section header, `padding: 6px 12px 5px`.
const HEADER_PAD: Insets = Insets { top: 6, right: 12, bottom: 5, left: 12 };

/// A swatch's size: the design's Places menu, `14px × 11px`.
const SWATCH_W: u32 = 14;
const SWATCH_H: u32 = 11;
/// Its corners: the design's `border-radius: 2px`.
const SWATCH_RADIUS: u32 = 2;
/// The space between a swatch and its label: the design's `gap: 10px`.
const SWATCH_GAP: u32 = 10;

/// The gap around a separator rule: the design's `margin: 5px 0`, the rule running edge to edge.
const SEPARATOR_PAD: Insets = Insets { top: 5, right: 0, bottom: 5, left: 0 };

/// The gap before an accelerator's text, so it never touches its label.
const ACCEL_PAD: Insets = Insets { top: 0, right: 0, bottom: 0, left: 24 };

/// What a marked row shows in its first column; **published so that a test can name it** (a glyph
/// the shipped face lacks draws blank).
pub const MARK: &str = "\u{2022}";

/// How wide that column is, whether or not a row is marked.
const MARK_W: u32 = 14;

mod host;
pub use host::*;

#[cfg(test)]
mod tests;
