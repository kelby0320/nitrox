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
#[derive(Clone, Default)]
pub struct MenuState {
    open: Option<usize>,
    /// Each bar word's rectangle, captured from the layout so a popup knows where to hang.
    anchors: Vec<Option<Rect>>,
    cursor: Option<usize>,
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
        Self { open: None, anchors: alloc::vec![None; count], cursor: None }
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

    /// Toggle menu `i`, which is what a press on the bar means.
    pub fn toggle(&mut self, i: usize) {
        self.open = if self.open == Some(i) { None } else { Some(i) };
        self.cursor = None;
    }

    /// Close, choosing nothing.
    pub fn close(&mut self) {
        self.open = None;
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

#[cfg(test)]
mod tests {

    #[test]
    fn a_marked_row_draws_the_mark() {
        fn labels<M>(e: &Element<M>, out: &mut Vec<String>) {
            if let crate::element::Node::Text(t) = &e.node {
                out.push(t.clone());
            }
            for c in e.children() {
                labels(c, out);
            }
        }
        let menu =
            |marked: bool| Menu { title: "View", items: alloc::vec![Item::plain("Newest First", 1u32).marked(marked)] };
        let theme = Theme::default();
        let st = MenuState::new(1);

        let mut lit = Vec::new();
        labels(&popup(&menu(true), &st, 0, None, &theme), &mut lit);
        assert!(lit.iter().any(|l| l == MARK), "a marked row draws the mark: {lit:?}");

        let mut rest = Vec::new();
        labels(&popup(&menu(false), &st, 0, None, &theme), &mut rest);
        assert!(!rest.iter().any(|l| l == MARK), "and an unmarked one does not: {rest:?}");
        assert!(rest.iter().any(|l| l == "Newest First"));
    }

    use super::*;
    use libdraw::format::Rgb;
    use libkern::abi::{KEY_DOWN, KEY_ENTER, KEY_ESC, KEY_LEFT, KEY_RIGHT, KEY_UP};

    fn ev(code: u16, mods: u16) -> KeyEvent {
        KeyEvent::new(0, code, 1, mods)
    }

    fn menu() -> Menu<u8> {
        Menu {
            title: "File",
            items: alloc::vec![
                Item::new("New Tab", Accel::ctrl_shift(20, "T"), 1),
                Item::Separator,
                Item::plain("Close Tab", 2).enabled(false),
                Item::plain("Quit", 3),
            ],
        }
    }

    #[allow(clippy::type_complexity, reason = "a one-off pair of lists in a test helper")]
    fn inks_and_washes(e: &Element<u8>) -> (Vec<(String, Option<Rgb>)>, Vec<(Rgb, u8)>) {
        fn walk(
            e: &Element<u8>,
            ink: Option<Rgb>,
            texts: &mut Vec<(String, Option<Rgb>)>,
            washes: &mut Vec<(Rgb, u8)>,
        ) {
            let ink = match &e.node {
                crate::element::Node::Ink { colour, .. } => Some(*colour),
                _ => ink,
            };
            match &e.node {
                crate::element::Node::Text(t) if !t.is_empty() => texts.push((t.clone(), ink)),
                crate::element::Node::Wash { colour, coverage } => washes.push((*colour, *coverage)),
                _ => {}
            }
            for c in e.children() {
                walk(c, ink, texts, washes);
            }
        }
        let (mut texts, mut washes) = (Vec::new(), Vec::new());
        walk(e, None, &mut texts, &mut washes);
        (texts, washes)
    }

    #[test]
    fn a_popup_dims_its_chords_and_what_it_cannot_do_and_reddens_what_destroys() {
        let theme = Theme::default();
        let m = Menu {
            title: "File",
            items: alloc::vec![
                Item::new("New Tab", Accel::ctrl_shift(20, "T"), 1u8),
                Item::plain("Close Tab", 2).enabled(false),
                Item::plain("Delete", 3).destructive(true),
                Item::plain("Wipe", 4).destructive(true).enabled(false),
            ],
        };
        let st = MenuState::new(1);
        let (texts, washes) = inks_and_washes(&popup(&m, &st, 0, None, &theme));
        let ink_of = |label: &str| texts.iter().find(|(t, _)| t == label).map(|(_, i)| *i);
        assert_eq!(ink_of("New Tab"), Some(None), "an ordinary label is in the foreground");
        assert_eq!(ink_of("Ctrl+Shift+T"), Some(Some(theme.foreground_dim)), "the chord is dim");
        assert_eq!(ink_of("Close Tab"), Some(Some(theme.foreground_dim)), "a disabled row is dim");
        assert_eq!(ink_of("Delete"), Some(Some(theme.deny)), "a destructive row is deny");
        assert_eq!(ink_of("Wipe"), Some(Some(theme.foreground_dim)), "disabled wins over destructive");
        assert!(washes.is_empty(), "nothing is lit: {washes:?}");

        let (_, washes) = inks_and_washes(&popup(&m, &st, 0, Some(0), &theme));
        assert_eq!(washes, [(theme.accent, theme.scheme.hover_coverage())]);
    }

    #[test]
    fn an_accelerator_labels_itself_and_matches_exactly() {
        let a = Accel::ctrl_shift(20, "T");
        assert_eq!(a.label(), "Ctrl+Shift+T");
        assert_eq!(Accel::ctrl(46, "C").label(), "Ctrl+C");

        assert!(a.matches(&ev(20, MOD_CTRL | MOD_SHIFT)));
        assert!(!Accel::ctrl(20, "T").matches(&ev(20, MOD_CTRL | MOD_SHIFT)));
        assert!(!a.matches(&ev(20, MOD_CTRL)));
        assert!(!a.matches(&ev(21, MOD_CTRL | MOD_SHIFT)));
        let mut up = ev(20, MOD_CTRL | MOD_SHIFT);
        up.pressed = 0;
        assert!(!a.matches(&up));
    }

    #[test]
    fn arrowing_skips_separators_and_disabled_rows() {
        let menus = alloc::vec![menu()];
        let mut s = MenuState::new(1);
        s.toggle(0);

        assert_eq!(s.key(&ev(KEY_DOWN, 0), &menus), KeyOutcome::Changed);
        assert_eq!(s.cursor(), Some(0));
        s.key(&ev(KEY_DOWN, 0), &menus);
        assert_eq!(s.cursor(), Some(3));
        s.key(&ev(KEY_DOWN, 0), &menus);
        assert_eq!(s.cursor(), Some(0));
        s.key(&ev(KEY_UP, 0), &menus);
        assert_eq!(s.cursor(), Some(3));
    }

    #[test]
    fn escape_dismisses_and_enter_without_a_cursor_does_not_choose() {
        let menus = alloc::vec![menu()];
        let mut s = MenuState::new(1);

        assert_eq!(s.key(&ev(KEY_ESC, 0), &menus), KeyOutcome::Ignored);

        s.toggle(0);
        assert_eq!(s.key(&ev(KEY_ESC, 0), &menus), KeyOutcome::Dismissed);
        assert_eq!(s.open(), None);

        s.toggle(0);
        assert_eq!(s.key(&ev(KEY_ENTER, 0), &menus), KeyOutcome::Dismissed);
        assert_eq!(s.open(), None);

        s.toggle(0);
        s.key(&ev(KEY_DOWN, 0), &menus);
        assert_eq!(s.key(&ev(KEY_ENTER, 0), &menus), KeyOutcome::Chose { menu: 0, item: 0 });
        assert_eq!(s.open(), None, "choosing closes");
    }

    #[test]
    fn a_menu_of_nothing_selectable_never_reports_a_cursor() {
        let menus = alloc::vec![Menu {
            title: "Edit",
            items: alloc::vec![Item::plain("Undo", 1).enabled(false), Item::Separator],
        }];
        let mut s = MenuState::new(1);
        s.toggle(0);
        s.key(&ev(KEY_DOWN, 0), &menus);
        assert_eq!(s.cursor(), None);
        assert_eq!(s.key(&ev(KEY_ENTER, 0), &menus), KeyOutcome::Dismissed);
    }

    #[test]
    fn a_chord_finds_its_item_and_a_disabled_one_stays_silent() {
        use crate::widget::Theme;
        let mut m = menu();
        assert_eq!(accel_match(core::slice::from_ref(&m), &ev(20, MOD_CTRL | MOD_SHIFT)), Some(1));
        let mut up = ev(20, MOD_CTRL | MOD_SHIFT);
        up.pressed = 0;
        assert_eq!(accel_match(core::slice::from_ref(&m), &up), None);
        // **The negative control**: disable the item and the same chord stops matching.
        m.items[0] = Item::new("New Tab", Accel::ctrl_shift(20, "T"), 1).enabled(false);
        assert_eq!(accel_match(core::slice::from_ref(&m), &ev(20, MOD_CTRL | MOD_SHIFT)), None);
        let st = MenuState::new(1);
        let _ = popup(&m, &st, 0, None, &Theme::default());
    }

    /// The popup survives being diffed frame to frame as the highlight moves: a menu is redrawn on
    /// every hover change.
    #[test]
    fn the_popup_diffs_from_one_highlight_to_the_next() {
        use crate::diff::Tree;
        use crate::layout::{FixedCell, layout};
        use crate::widget::Theme;
        let m = menu();
        let theme = Theme::default();
        let cell = FixedCell { w: 8, h: 16 };
        let bounds = Rect::new(0, 0, 200, 120);
        let mut tree = Tree::new();
        let mut st = MenuState::new(1);
        st.toggle(0);
        for hover in [None, Some(0), Some(3), None, Some(0)] {
            let e = popup(&m, &st, 0, hover, &theme);
            let l = layout(&e, bounds, &cell);
            assert!(
                tree.update(&e, &l).is_ok(),
                "the popup stopped being diffable when the highlight moved to {hover:?}"
            );
        }
    }

    /// `Chose` names the menu it came from: choosing closes, so a caller asking `open()` afterwards
    /// would get `None`.
    #[test]
    fn choosing_names_the_menu_it_chose_from_even_though_it_has_closed() {
        let menus = alloc::vec![Menu { title: "File", items: alloc::vec![Item::plain("Close", 9u8)] }, menu(),];
        let mut s = MenuState::new(2);
        s.toggle(1);
        s.key(&ev(KEY_DOWN, 0), &menus);
        let out = s.key(&ev(KEY_ENTER, 0), &menus);
        assert_eq!(s.open(), None, "choosing closes, which is what makes this necessary");
        let KeyOutcome::Chose { menu, item } = out else { panic!("Enter chose nothing: {out:?}") };
        assert_eq!(menu, 1, "the second menu was open, and the outcome still says so");
        assert!(matches!(menus[menu].items[item], Item::Action { msg: 1, .. }), "the pair does not address New Tab");
    }

    #[test]
    fn arrowing_sideways_moves_which_menu_is_open() {
        let menus = alloc::vec![Menu { title: "File", items: alloc::vec![Item::plain("Close", 9u8)] }, menu(),];
        let mut s = MenuState::new(2);
        s.toggle(0);
        assert_eq!(s.key(&ev(KEY_RIGHT, 0), &menus), KeyOutcome::Changed);
        assert_eq!(s.open(), Some(1), "Right moved to the next menu");
        assert_eq!(s.cursor(), None, "…and the cursor does not carry across");
        assert_eq!(s.key(&ev(KEY_LEFT, 0), &menus), KeyOutcome::Changed);
        assert_eq!(s.open(), Some(0), "Left wraps back");
        let one = alloc::vec![menu()];
        let mut s = MenuState::new(1);
        s.toggle(0);
        assert_eq!(s.key(&ev(KEY_RIGHT, 0), &one), KeyOutcome::Ignored);
        assert_eq!(s.open(), Some(0));
    }

    #[test]
    fn toggling_the_open_menu_closes_it() {
        let mut s = MenuState::new(2);
        s.toggle(0);
        assert_eq!(s.open(), Some(0));
        s.toggle(1);
        assert_eq!(s.open(), Some(1), "another word switches rather than closing");
        s.toggle(1);
        assert_eq!(s.open(), None);
    }

    fn text_rects<M>(e: &Element<M>, l: &crate::layout::Layout, out: &mut Vec<(String, Rect)>) {
        if let crate::element::Node::Text(t) = &e.node
            && !t.is_empty()
        {
            out.push((t.clone(), l.rect));
        }
        for (c, cl) in e.children().zip(l.children.iter()) {
            text_rects(c, cl, out);
        }
    }

    /// A hint sits in the chord's column, dim, and a chord wins where a row has both.
    #[test]
    fn a_hint_sits_in_the_chord_column_and_a_chord_wins() {
        let theme = Theme::default();
        let m = Menu {
            title: "Places",
            items: alloc::vec![
                Item::plain("Documents", 1u8).hint("~/Documents"),
                Item::new("New Tab", Accel::ctrl_shift(20, "T"), 2).hint("never shown"),
            ],
        };
        let (texts, _) = inks_and_washes(&popup(&m, &MenuState::new(1), 0, None, &theme));
        let ink_of = |label: &str| texts.iter().find(|(t, _)| t == label).map(|(_, i)| *i);
        assert_eq!(ink_of("~/Documents"), Some(Some(theme.foreground_dim)), "a hint is dim");
        assert_eq!(ink_of("Ctrl+Shift+T"), Some(Some(theme.foreground_dim)));
        assert_eq!(ink_of("never shown"), None, "a chord takes the column from a hint");

        let cell = crate::layout::FixedCell { w: 8, h: 16 };
        let e = popup(&m, &MenuState::new(1), 0, None, &theme);
        let l = crate::layout::layout(&e, Rect::new(0, 0, 300, 200), &cell);
        let mut rects = Vec::new();
        text_rects(&e, &l, &mut rects);
        let at = |t: &str| rects.iter().find(|(s, _)| s == t).map(|(_, r)| *r).unwrap();
        assert_eq!(at("~/Documents").right(), at("Ctrl+Shift+T").right(), "one column, ragged left");
    }

    /// A swatch widens the leading column for every row, so the labels stay in one line.
    #[test]
    fn a_swatch_widens_the_leading_column_for_the_whole_menu() {
        let theme = Theme::default();
        let red = Rgb::new(200, 0, 0);
        let m =
            Menu { title: "Places", items: alloc::vec![Item::plain("Home", 1u8).swatch(red), Item::plain("Other", 2)] };
        let cell = crate::layout::FixedCell { w: 8, h: 16 };
        let e = popup(&m, &MenuState::new(1), 0, None, &theme);
        let l = crate::layout::layout(&e, Rect::new(0, 0, 300, 200), &cell);
        let mut rects = Vec::new();
        text_rects(&e, &l, &mut rects);
        let at = |t: &str| rects.iter().find(|(s, _)| s == t).map(|(_, r)| *r).unwrap();
        assert_eq!(at("Home").origin.x, at("Other").origin.x, "the labels share a left edge");

        let plain = Menu { title: "Places", items: alloc::vec![Item::plain("Other", 2u8)] };
        let pe = popup(&plain, &MenuState::new(1), 0, None, &theme);
        let pl = crate::layout::layout(&pe, Rect::new(0, 0, 300, 200), &cell);
        let mut prects = Vec::new();
        text_rects(&pe, &pl, &mut prects);
        let plain_x = prects.iter().find(|(s, _)| s == "Other").unwrap().1.origin.x;
        assert_eq!(at("Other").origin.x - plain_x, (SWATCH_W + SWATCH_GAP - MARK_W) as i32);

        fn fills<M>(e: &Element<M>, out: &mut Vec<Rgb>) {
            if let crate::element::Node::RoundedFill { colour, .. } = &e.node {
                out.push(*colour);
            }
            for c in e.children() {
                fills(c, out);
            }
        }
        let mut found = Vec::new();
        fills(&e, &mut found);
        assert_eq!(found, [red]);
    }

    /// A headed popup keeps diffing as its rows come and go under the header (the header's key is
    /// what makes it diffable).
    #[test]
    fn a_headed_popup_diffs_as_its_rows_are_filtered() {
        use crate::diff::Tree;
        use crate::layout::{FixedCell, layout};
        let theme = Theme::default();
        let cell = FixedCell { w: 8, h: 16 };
        let bounds = Rect::new(0, 0, 200, 200);
        let names = ["Files", "Terminal", "Text Editor"];
        let mut tree = Tree::new();
        for keep in [3usize, 2, 1, 0, 3] {
            let m = Menu {
                title: "Applications",
                items: names[..keep].iter().enumerate().map(|(i, n)| Item::plain(*n, i as u8)).collect(),
            };
            let e = popup_headed(&m, &MenuState::new(1), 10, None, text("filter").key(1), &theme);
            let l = layout(&e, bounds, &cell);
            assert!(tree.update(&e, &l).is_ok(), "the popup stopped diffing at {keep} rows");
            let mut rects = Vec::new();
            text_rects(&e, &l, &mut rects);
            let header = rects.iter().find(|(s, _)| s == "filter").unwrap().1;
            for (s, r) in rects.iter().filter(|(s, _)| names.contains(&s.as_str())) {
                assert!(r.origin.y >= header.bottom() as i32, "{s} is above the header");
            }
        }
    }

    #[test]
    fn select_first_points_at_the_first_choosable_row() {
        let menus = alloc::vec![menu()];
        let mut s = MenuState::new(1);
        s.select_first(&menus);
        assert_eq!(s.cursor(), None, "nothing is open, so nothing is pointed at");
        s.toggle(0);
        s.select_first(&menus);
        assert_eq!(s.cursor(), Some(0));
        assert_eq!(s.key(&ev(KEY_ENTER, 0), &menus), KeyOutcome::Chose { menu: 0, item: 0 });

        let menus = alloc::vec![Menu {
            title: "Applications",
            items: alloc::vec![Item::Separator, Item::plain("Gone", 1u8).enabled(false), Item::plain("Here", 2)],
        }];
        let mut s = MenuState::new(1);
        s.toggle(0);
        s.select_first(&menus);
        assert_eq!(s.cursor(), Some(2));
        let empty: Vec<Menu<u8>> = alloc::vec![Menu { title: "Applications", items: Vec::new() }];
        s.select_first(&empty);
        assert_eq!(s.cursor(), None);
    }
}
