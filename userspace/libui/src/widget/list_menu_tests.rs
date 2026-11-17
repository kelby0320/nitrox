use super::*;
use crate::diff::Tree;
use crate::layout::{FixedCell, layout};
use crate::route::Router;
use libdraw::geom::Rect;
use libkern::abi::{BTN_LEFT, BTN_RIGHT};
use librsproto::surface::{POINTER_BUTTON, POINTER_PRESSED};

const CELL: FixedCell = FixedCell { w: 8, h: 16 };
const ROW_H: u32 = 20;
const WIDE: u32 = 300;

#[derive(Clone, PartialEq, Eq, Debug)]
enum M {
    Open(u64),
    RowMenu(u64),
    Folder,
    Scroll,
}

fn rows<'a>(labels: &'a [(u64, &'a str)], button: Option<RowButton<'a>>) -> alloc::vec::Vec<ListRow<'a>> {
    labels.iter().map(|&(key, label)| ListRow { key, label, button, ..Default::default() }).collect()
}

/// What one press of `button` at `(x, y)` and its release send, on a fresh router.
fn click(e: &Element<M>, height: u32, button: u16, x: i32, y: i32) -> alloc::vec::Vec<M> {
    let l = layout(e, Rect::new(0, 0, WIDE, height), &CELL);
    let mut t = Tree::new();
    t.update(e, &l).expect("a clean frame");
    let held = if button == BTN_LEFT { 1 } else { 2 };
    let at = |pressed: bool| PointerEvent {
        kind: POINTER_BUTTON,
        button,
        buttons: if pressed { held } else { 0 },
        flags: if pressed { POINTER_PRESSED } else { 0 },
        x,
        y,
        ..Default::default()
    };
    let mut r = Router::new();
    let mut out = r.pointer(&t, e, &l, at(true)).0;
    out.extend(r.pointer(&t, e, &l, at(false)).0);
    out
}

fn menus() -> ListMenus<M> {
    ListMenus { row: M::RowMenu, empty: Some(M::Folder) }
}

#[test]
fn a_right_press_asks_for_the_rows_menu_or_the_spaces_below_them() {
    let labels = [(7, "a"), (8, "b")];
    let height = 5 * ROW_H;
    let mut st = ListState::default();
    let t = Theme::default();
    let e = list_view_with_menus(
        &rows(&labels, None),
        &[],
        &mut st,
        height,
        ROW_H,
        M::Open,
        None,
        None,
        menus(),
        None,
        None,
        &t,
    );
    assert_eq!(click(&e, height, BTN_RIGHT, 40, 30), [M::RowMenu(8)], "the second row's");
    assert_eq!(click(&e, height, BTN_RIGHT, 40, 90), [M::Folder], "the space below the rows");
    assert_eq!(click(&e, height, BTN_LEFT, 40, 30), [M::Open(8)], "the left button still opens");
    assert_eq!(click(&e, height, BTN_LEFT, 40, 90), [], "and asks for no menu below the rows");

    let none = ListMenus { row: M::RowMenu, empty: None };
    let e = list_view_with_menus(
        &rows(&labels, None),
        &[],
        &mut st,
        height,
        ROW_H,
        M::Open,
        None,
        None,
        none,
        None,
        None,
        &t,
    );
    assert_eq!(click(&e, height, BTN_RIGHT, 40, 90), [], "a list with no menu of its own below the rows");
    let e = list_view(&rows(&labels, None), &[], &mut st, height, ROW_H, M::Open, None, None, None, None, &t);
    assert_eq!(click(&e, height, BTN_RIGHT, 40, 10), [], "and `list_view` asks for none");
}

#[test]
fn a_right_press_on_a_rows_button_asks_for_the_rows_menu() {
    let labels = [(7, "NXFAT")];
    let button = Some(RowButton { key: 70, glyph: "x" });
    let mut st = ListState::default();
    let t = Theme::default();
    let e = list_view_with_menus(
        &rows(&labels, button),
        &[],
        &mut st,
        ROW_H,
        ROW_H,
        M::Open,
        None,
        None,
        menus(),
        None,
        None,
        &t,
    );
    let x = (WIDE - ROW_BUTTON_W / 2) as i32;
    assert_eq!(click(&e, ROW_H, BTN_LEFT, x, 10), [M::Open(70)], "precondition: the button is under the pointer");
    assert_eq!(click(&e, ROW_H, BTN_RIGHT, x, 10), [M::RowMenu(7)], "the row's, keyed as the row");
}

#[test]
fn a_right_press_on_the_scrollbar_asks_for_no_menu() {
    let labels: alloc::vec::Vec<(u64, &str)> = (0..10).map(|k| (k, "row")).collect();
    let height = 3 * ROW_H;
    let mut st = ListState::default();
    let t = Theme::default();
    let e = list_view_with_menus(
        &rows(&labels, None),
        &[],
        &mut st,
        height,
        ROW_H,
        M::Open,
        None,
        Some(|_| M::Scroll),
        menus(),
        None,
        None,
        &t,
    );
    let bar = (WIDE - SCROLLBAR_W / 2) as i32;
    assert!(click(&e, height, BTN_LEFT, bar, 10).contains(&M::Scroll), "precondition: the bar is under the pointer");
    // The bar is handed every pointer event, whichever button; what matters is that no menu is asked for.
    let right = click(&e, height, BTN_RIGHT, bar, 10);
    assert!(right.iter().all(|m| *m == M::Scroll), "the bar asked for a menu: {right:?}");
    assert_eq!(click(&e, height, BTN_RIGHT, 40, 10), [M::RowMenu(0)], "beside it, a row");
}

#[test]
fn follow_brings_a_selection_scrolled_away_back_at_the_next_build() {
    let mut st = ListState::at(Some(1), 0);
    st.ensure_visible(3);
    st.offset = 6;
    st.ensure_visible(3);
    assert_eq!(st.offset, 6, "precondition: scrolled away, the selection unchanged, it stays away");
    st.follow();
    st.ensure_visible(3);
    assert_eq!(st.offset, 1, "followed again");
}
