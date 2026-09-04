//! Menu bars, and the drop-downs that were not widgets.
//!
//! **The bar was already a widget and the open menu was not**, which is the gap this module
//! closes (M14 Part A). [`widget::menu_bar`](crate::widget::menu_bar) and
//! [`widget::menu_item`](crate::widget::menu_item) have existed since M11, and `menu_bar`'s own
//! doc says the popup half "is not here, and is not a widget at all: an open menu is a `popup`
//! **window**". Two applications then wrote that half themselves — open/close state, the anchor
//! capture, row keying, dismissal — in two shapes that had drifted (`Option<Rect>` in one,
//! `[Option<Rect>; 2]` in the other) before a third wanted the same thing.
//!
//! What is here is the part that is a function of values: what a menu *contains*, which one is
//! open, where its rows are, and what a keystroke does to that. Opening the window is still the
//! application's, through [`window::Child`](crate::window::Child) — this module cannot make a
//! syscall and is not trying to.
//!
//! ## The accelerator is declared once
//!
//! A menu that *says* `Ctrl+C` and a handler that *implements* it are two statements of one fact,
//! and they drift. [`Accel`] is the fact: the item carries it, the row's label is rendered from
//! it, and the application's key handler asks [`Accel::matches`] rather than re-deriving the
//! chord. There is no second place to change.

use alloc::string::String;
use alloc::vec::Vec;

use librsproto::surface::{KeyEvent, MOD_ALT, MOD_CTRL, MOD_SHIFT};

use libdraw::geom::{Rect, Size};

use crate::element::{Element, Insets, column, fill, padding, row, sized, stack, text};
use crate::widget::{Theme, menu_bar, menu_item, popup_frame};
use crate::element::bevel;

/// A keyboard shortcut: the modifiers held, and the key pressed.
///
/// **One value, two readers** — the label a person sees and the match a handler makes. See the
/// module doc.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Accel {
    /// `MOD_CTRL` / `MOD_SHIFT` / `MOD_ALT`, or'd.
    pub mods: u16,
    /// The `EV_KEY` code, as `libinput`'s table gives it.
    pub key: u16,
    /// How the key prints: `"C"`, `"F4"`. **Given rather than derived**, because a keycode-to-name
    /// table is a second keymap, and this crate cannot make one that is right for every layout.
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

    /// Whether `ev` is this chord being pressed.
    ///
    /// **Exact on the modifiers this cares about, not a subset test.** `Ctrl+T` must not fire on
    /// `Ctrl+Shift+T`, or a terminal's "new tab" would also answer the chord meant for something
    /// else — the bug that shape produces is one keystroke doing two things.
    pub fn matches(&self, ev: &KeyEvent) -> bool {
        const KNOWN: u16 = MOD_CTRL | MOD_SHIFT | MOD_ALT;
        ev.pressed != 0 && ev.keycode == self.key && (ev.modifiers & KNOWN) == self.mods
    }
}

/// One row of an open menu.
pub enum Item<Msg> {
    /// A row that does something. Disabled rows are drawn dimmed and do not answer a press.
    Action {
        /// What the row reads.
        label: &'static str,
        /// The chord shown on its right, if it has one.
        accel: Option<Accel>,
        /// What choosing it means.
        msg: Msg,
        /// Whether it can be chosen now.
        enabled: bool,
    },
    /// A horizontal rule between groups.
    Separator,
}

impl<Msg> Item<Msg> {
    /// An enabled row with an accelerator.
    pub fn new(label: &'static str, accel: Accel, msg: Msg) -> Self {
        Item::Action { label, accel: Some(accel), msg, enabled: true }
    }

    /// An enabled row with no chord.
    pub fn plain(label: &'static str, msg: Msg) -> Self {
        Item::Action { label, accel: None, msg, enabled: true }
    }

    /// The same row, greyed and unpressable.
    pub fn enabled(self, on: bool) -> Self {
        match self {
            Item::Action { label, accel, msg, .. } => {
                Item::Action { label, accel, msg, enabled: on }
            }
            Item::Separator => Item::Separator,
        }
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
/// **The application owns this**, the way it owns a `ListState` — the widget is a function of it.
#[derive(Clone, Default)]
pub struct MenuState {
    /// Index into the bar's menus, or `None` when nothing is open.
    open: Option<usize>,
    /// Each bar word's rectangle, captured from the layout so a popup knows where to hang.
    anchors: Vec<Option<Rect>>,
    /// The row the keyboard has moved to, as an index into the open menu's items.
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
    /// The row at this index was chosen; the menu is closed.
    Chose(usize),
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

    /// Record where each bar word sits. Called every frame: a word's position is a fact about the
    /// layout rather than about the menu, and reading it only on open means reading it before the
    /// first layout exists.
    pub fn set_anchors(&mut self, anchors: Vec<Option<Rect>>) {
        self.anchors = anchors;
    }

    /// Drive the open menu from the keyboard.
    ///
    /// **Skips separators and disabled rows**, in both directions, so arrowing never lands
    /// somewhere Enter would do nothing. Returns [`KeyOutcome::Ignored`] when no menu is open, so
    /// a caller can pass every key through this before its own handling.
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
                    KeyOutcome::Chose(i)
                }
                // **Enter with nothing selected closes rather than choosing the first row.** A
                // menu opened by a chord has no cursor yet, and guessing would fire an action
                // somebody never pointed at.
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
    let start = from.map_or(if d > 0 { 0 } else { n - 1 }, |i| {
        ((i as isize + d).rem_euclid(n as isize)) as usize
    });
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
/// **This is what makes decision 2 true rather than aspirational.** A menu that *says* `Ctrl+C`
/// and a key handler that separately *implements* it are two statements of one fact, and the
/// pair drifts — the menu keeps saying a chord the handler stopped honouring, and nothing fails.
/// Routing the key through the same table the popup draws leaves one statement.
///
/// **Disabled items do not match**, for the reason arrowing skips them: an item that would do
/// nothing if clicked should do nothing if typed. A menu need not be open — accelerators are the
/// half of a menu that works when it is shut.
pub fn accel_match<Msg: Clone>(menus: &[Menu<Msg>], ev: &KeyEvent) -> Option<Msg> {
    if ev.pressed == 0 {
        return None;
    }
    menus.iter().flat_map(|m| m.items.iter()).find_map(|it| match it {
        Item::Action { accel: Some(a), msg, enabled: true, .. } if a.matches(ev) => {
            Some(msg.clone())
        }
        _ => None,
    })
}

/// The bar itself: one word per menu, keyed so a press and an anchor can find it.
///
/// `key_base` is where this bar's element keys start; it uses `key_base + i` per menu, so a caller
/// picks a range that does not collide with its own keys.
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

/// The open menu's popup: its rows, framed.
///
/// `key_base` is where the *rows'* element keys start — a different range from the bar's.
pub fn popup<Msg: Clone>(
    menu: &Menu<Msg>,
    state: &MenuState,
    key_base: u64,
    hovered: Option<u64>,
    theme: &Theme,
) -> Element<Msg> {
    let mut rows: Vec<Element<Msg>> = Vec::with_capacity(menu.items.len());
    for (i, it) in menu.items.iter().enumerate() {
        // **Every row is keyed, separators included**, and a rule has no more use for a key than
        // it has for a press handler. `diff` rejects a parent whose children are *partly* keyed
        // — `DiffError::MixedKeying` — so an unkeyed rule makes the whole popup undiffable, and
        // the way that shows is a menu that opens, reports its size, and then never draws a
        // frame. It cost a QEMU round trip to find and a host test to pin.
        let key = key_base + i as u64;
        match it {
            // **A rule that stretches without knowing the width.** A column arranges each child
            // at the column's full width and its own measured *height*, so an element measuring
            // zero wide and one tall paints across the whole popup. `sized` cannot ask for "as
            // wide as you have", and it does not need to.
            Item::Separator => rows.push(
                padding(SEPARATOR_PAD, sized(Size::new(0, 1), fill(theme.border))).key(key),
            ),
            Item::Action { label, accel, msg, enabled } => {
                let lit = *enabled && (hovered == Some(key) || state.cursor() == Some(i));
                // **The chord sits in the same row as its label, pushed right by a spacer.** A
                // menu that only names its actions teaches nothing; the point of the column on
                // the right is that somebody stops needing the menu.
                // **A flexed empty text is the spacer**, which is what right-aligns the chord
                // column: `text("")` measures nothing and paints nothing, and `flex(1)` hands it
                // whatever the widest row leaves over.
                let body: Element<Msg> = match accel {
                    Some(a) => row(alloc::vec![
                        text(*label),
                        text("").flex(1),
                        padding(ACCEL_PAD, text(a.label())),
                    ]),
                    None => row(alloc::vec![text(*label), text("").flex(1)]),
                };
                let mut e = menu_row(body, lit, *enabled, theme);
                if *enabled {
                    e = e.on_press(msg.clone());
                }
                rows.push(e.key(key));
            }
        }
    }
    popup_frame(padding(Insets::all(2), column(rows)), theme)
}

/// One popup row: its body, highlighted when the pointer or the keyboard is on it.
///
/// **Not `menu_item`**, which takes a `&str` and builds its own label — a row with an accelerator
/// column is two pieces of text with a gap between them, and giving `menu_item` a second shape
/// would make the bar's words and the popup's rows the same function pretending to be one thing.
///
/// **A disabled row is not dimmed, and that is a gap rather than a decision.** `paint` draws every
/// `Text` in `theme.foreground` — there is no per-element ink — so "unavailable" shows only as a
/// row that does not light under the pointer and that arrowing skips. The colour arrives with the
/// ink wrapper M14 Part G adds for syntax highlighting; until then this is honest about being half
/// of the affordance.
fn menu_row<Msg>(body: Element<Msg>, lit: bool, _enabled: bool, theme: &Theme) -> Element<Msg> {
    let mut layers = Vec::with_capacity(3);
    if lit {
        layers.push(fill(theme.focus_ring));
        layers.push(padding(Insets::all(1), bevel(theme.selection)));
    }
    layers.push(padding(ROW_PAD, body));
    stack(layers)
}

/// The space around a popup row's contents. Matches `menu_item`'s, so the bar and the rows below
/// it are spaced alike.
const ROW_PAD: Insets = Insets { top: 3, right: 10, bottom: 3, left: 10 };

/// The gap around a separator rule.
const SEPARATOR_PAD: Insets = Insets { top: 3, right: 2, bottom: 3, left: 2 };

/// The gap before an accelerator's text, so it never touches its label.
const ACCEL_PAD: Insets = Insets { top: 0, right: 0, bottom: 0, left: 24 };

#[cfg(test)]
mod tests {
    use super::*;
    use libkern::abi::{KEY_DOWN, KEY_ENTER, KEY_ESC, KEY_UP};

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

    #[test]
    fn an_accelerator_labels_itself_and_matches_exactly() {
        let a = Accel::ctrl_shift(20, "T");
        assert_eq!(a.label(), "Ctrl+Shift+T");
        assert_eq!(Accel::ctrl(46, "C").label(), "Ctrl+C");

        assert!(a.matches(&ev(20, MOD_CTRL | MOD_SHIFT)));
        // **Not a subset test.** `Ctrl+T` firing on `Ctrl+Shift+T` is one keystroke doing two
        // things, which is the bug this exactness exists to prevent.
        assert!(!Accel::ctrl(20, "T").matches(&ev(20, MOD_CTRL | MOD_SHIFT)));
        assert!(!a.matches(&ev(20, MOD_CTRL)));
        assert!(!a.matches(&ev(21, MOD_CTRL | MOD_SHIFT)));
        // A release is not a press.
        let mut up = ev(20, MOD_CTRL | MOD_SHIFT);
        up.pressed = 0;
        assert!(!a.matches(&up));
    }

    #[test]
    fn arrowing_skips_separators_and_disabled_rows() {
        let menus = alloc::vec![menu()];
        let mut s = MenuState::new(1);
        s.toggle(0);

        // Down from nothing lands on the first *selectable* row, not on index 0 blindly.
        assert_eq!(s.key(&ev(KEY_DOWN, 0), &menus), KeyOutcome::Changed);
        assert_eq!(s.cursor(), Some(0));
        // Next down skips the separator (1) and the disabled row (2).
        s.key(&ev(KEY_DOWN, 0), &menus);
        assert_eq!(s.cursor(), Some(3));
        // And wraps.
        s.key(&ev(KEY_DOWN, 0), &menus);
        assert_eq!(s.cursor(), Some(0));
        // Upward skips them too, which the first version got right in one direction only.
        s.key(&ev(KEY_UP, 0), &menus);
        assert_eq!(s.cursor(), Some(3));
    }

    #[test]
    fn escape_dismisses_and_enter_without_a_cursor_does_not_choose() {
        let menus = alloc::vec![menu()];
        let mut s = MenuState::new(1);

        // Closed: every key is the application's.
        assert_eq!(s.key(&ev(KEY_ESC, 0), &menus), KeyOutcome::Ignored);

        s.toggle(0);
        assert_eq!(s.key(&ev(KEY_ESC, 0), &menus), KeyOutcome::Dismissed);
        assert_eq!(s.open(), None);

        // **A menu opened by a chord has no cursor**, and Enter must not invent one — guessing
        // fires an action nobody pointed at.
        s.toggle(0);
        assert_eq!(s.key(&ev(KEY_ENTER, 0), &menus), KeyOutcome::Dismissed);
        assert_eq!(s.open(), None);

        s.toggle(0);
        s.key(&ev(KEY_DOWN, 0), &menus);
        assert_eq!(s.key(&ev(KEY_ENTER, 0), &menus), KeyOutcome::Chose(0));
        assert_eq!(s.open(), None, "choosing closes");
    }

    #[test]
    fn a_menu_of_nothing_selectable_never_reports_a_cursor() {
        // Every row disabled: arrowing has nowhere to go, and must not sit on an unpressable row.
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
        // The enabled item with a chord answers to it, with the menu shut.
        assert_eq!(accel_match(core::slice::from_ref(&m), &ev(20, MOD_CTRL | MOD_SHIFT)), Some(1));
        // A release is not a press, however well it matches.
        let mut up = ev(20, MOD_CTRL | MOD_SHIFT);
        up.pressed = 0;
        assert_eq!(accel_match(core::slice::from_ref(&m), &up), None);
        // **The negative control**: disable the item and the same chord stops matching. Without
        // this the test would pass for a version that ignored `enabled` entirely.
        m.items[0] = Item::new("New Tab", Accel::ctrl_shift(20, "T"), 1).enabled(false);
        assert_eq!(accel_match(core::slice::from_ref(&m), &ev(20, MOD_CTRL | MOD_SHIFT)), None);
        // …and the popup still *draws* it, so this is about the chord and not about the row.
        let st = MenuState::new(1);
        let _ = popup(&m, &st, 0, None, &Theme::default());
    }

    /// The popup survives being diffed frame to frame as the highlight moves.
    ///
    /// **A menu is redrawn on every hover change**, so `Tree::update` sees this tree many times
    /// with one row lit and then another. A shape it rejects makes `Child::present` return false
    /// every frame — a popup that opens, reports its size, and then never draws. The reference
    /// render catches nothing here: it is one frame, and the first frame always succeeds.
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
}
