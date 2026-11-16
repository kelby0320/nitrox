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
    fn walk(e: &Element<u8>, ink: Option<Rgb>, texts: &mut Vec<(String, Option<Rgb>)>, washes: &mut Vec<(Rgb, u8)>) {
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
    let menus =
        alloc::vec![Menu { title: "Edit", items: alloc::vec![Item::plain("Undo", 1).enabled(false), Item::Separator] }];
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
        assert!(tree.update(&e, &l).is_ok(), "the popup stopped being diffable when the highlight moved to {hover:?}");
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
    let m = Menu { title: "Places", items: alloc::vec![Item::plain("Home", 1u8).swatch(red), Item::plain("Other", 2)] };
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
