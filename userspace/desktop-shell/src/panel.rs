//! The top bar and the menus that hang from it, as element trees (desktop refresh, Part C).
//!
//! **Pure, so the layout is host-tested** — and tested against the gates' own aims, which
//! `xtask` writes down a second time on purpose (M11 decision 2: a gate that read the shell's
//! layout to know where to click could agree with a shell that had stopped drawing where it
//! says). The tests here pin those aims as literals, so the two copies cannot part without one of
//! them failing — the arrangement `libui`'s `dialog_buttons_land_where_the_constants_say` made for
//! the dialogs.
//!
//! **What the design asks for and what is here.** The top panel is the design's: 30 pixels on the
//! panel ground with a rule beneath, an `Applications` button with its accent dot, a `Places`
//! button, and the clock centred on the screen. Its right-hand end — quick settings and a
//! notifications tray — is not built, because neither exists (`desktop-shell.md` §9). The
//! Applications menu is the design's menu with no categories and a filter field above the rows,
//! because typing to narrow it is the fastest way to a program and the only one without a pointer
//! (`docs/planning/desktop-refresh.md`, Part C).

use alloc::string::String;
use alloc::vec::Vec;

use libdraw::geom::Size;
use libui::element::{
    Element, Insets, center, center_v, column, fill, ink, padding, rounded_fill, row, sized, stack,
    text, wash, with_spacing,
};
use libui::menu::{Item, Menu, MenuState, popup, popup_headed};
use libui::widget::{TextFieldState, Theme, WidgetState, popup_frame, text_field};

use crate::{Application, BAR_H, matches_app};

/// The Applications menu's index in the bar's table — see [`menus`].
pub const APPS: usize = 0;
/// The Places menu's.
pub const PLACES: usize = 1;

/// What a press on the top bar asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TopMsg {
    /// Open, or close, the menu at this index into [`menus`]'s table.
    Menu(usize),
}

/// What choosing a menu row asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuMsg {
    /// Launch the application at this index into the **unfiltered** list.
    ///
    /// Unfiltered, so the message still names the right program after a filter has reordered
    /// what is shown — the rule `modal_rows` kept before this was a menu (M11 Part E batch 4).
    Launch(usize),
    /// Open the place at this index into [`libfs::places`].
    Place(usize),
    /// What a row that cannot be chosen carries. A disabled row never produces its message; it
    /// has to have one all the same.
    Nothing,
}

/// The Applications button's element key.
pub const APPS_KEY: u64 = 1;
/// The Places button's.
pub const PLACES_KEY: u64 = 2;
/// The empty stretch after them, keyed because its siblings are.
const REST_KEY: u64 = 3;
/// The filter field's key, above the rows.
pub const FILTER_KEY: u64 = 10;
/// Where a menu's rows are keyed from.
pub const ROW_KEY_BASE: u64 = 100;

/// The accent dot beside `Applications`: the design's 9 pixels.
const DOT: u32 = 9;
/// The `--soft` ring around it: `box-shadow: 0 0 0 2px`.
const DOT_RING: u32 = 2;
/// The Applications button's sides: `padding: 0 11px`.
const APPS_PAD: Insets = Insets { top: 0, right: 11, bottom: 0, left: 11 };
/// The gap between the dot and the word: `gap: 7px`.
const APPS_GAP: u32 = 7;
/// The Places button's sides: `padding: 0 10px`.
const PLACES_PAD: Insets = Insets { top: 0, right: 10, bottom: 0, left: 10 };

/// The Applications menu's width: the design's 252 pixels.
///
/// **Fixed rather than measured**, because the filter field in it measures as whatever has been
/// typed: a menu sized to its contents would widen and narrow a keystroke at a time.
pub const APPS_MENU_W: u32 = 252;
/// How far a menu hangs in from the left of the word that opened it: the design's 8 pixels.
pub const MENU_INSET_X: i32 = 8;
/// How far below the bar it hangs: the design's gap between the panel's edge and the menu's.
pub const MENU_DROP: i32 = 2;

/// The name prompt's width — the Applications menu's, since it is the same kind of popup.
pub const PROMPT_W: u32 = APPS_MENU_W;

/// The top bar: `Applications` and `Places` on the left, the clock centred on the screen, and a
/// rule along its foot.
///
/// `open` is the menu that is open, which lights its word as the pointer does; `hovered` is the
/// key under the pointer. The ground is the caller's — `paint` clears to the theme's
/// `background`, and the shell paints a bar with `panel` there.
pub fn top_bar(clock: &str, open: Option<usize>, hovered: Option<u64>, theme: &Theme) -> Element<TopMsg> {
    let hover = theme.scheme.hover_coverage();
    // **Lit under the pointer and while its menu is open**, the design's `--soft`: the word a
    // menu hangs from says which one it is.
    let face = |on: bool, body: Element<TopMsg>| {
        let mut layers = Vec::with_capacity(2);
        if on {
            layers.push(wash(theme.accent, hover));
        }
        layers.push(body);
        stack(layers)
    };
    let lit = |key: u64, menu: usize| hovered == Some(key) || open == Some(menu);

    // The dot and its ring. The ring is the accent at the hover's coverage over the panel —
    // `--soft`, made opaque, because a wash is square and this is round.
    let halo = theme.accent.blend(theme.panel, hover);
    let side = DOT + 2 * DOT_RING;
    let dot = sized(
        Size::new(side, side),
        stack(alloc::vec![
            rounded_fill(halo, side / 2),
            padding(Insets::all(DOT_RING), rounded_fill(theme.accent, DOT / 2)),
        ]),
    );
    let apps = face(
        lit(APPS_KEY, APPS),
        padding(
            APPS_PAD,
            with_spacing(row(alloc::vec![center_v(dot), center_v(text("Applications"))]), APPS_GAP),
        ),
    )
    .on_press(TopMsg::Menu(APPS))
    .key(APPS_KEY);
    let places = face(lit(PLACES_KEY, PLACES), padding(PLACES_PAD, center_v(text("Places"))))
        .on_press(TopMsg::Menu(PLACES))
        .key(PLACES_KEY);

    // **Above the rule, not over it**: a lit word's wash would otherwise run across the rule's
    // pixel and break the line under it.
    let above_rule = Insets { top: 0, right: 0, bottom: 1, left: 0 };
    let words = padding(
        above_rule,
        row(alloc::vec![apps, places, sized(Size::new(0, 0), text("")).flex(1).key(REST_KEY)]),
    );
    let rule = column(alloc::vec![
        sized(Size::new(0, 0), text("")).flex(1),
        sized(Size::new(0, 1), fill(theme.border)),
    ]);
    // **Centred on the screen, not on what the words leave** — a layer of its own spanning the
    // bar, which is what the balancing slot the old bar carried was approximating. Beneath the
    // words in the stack, so a press over the clock lands on the empty stretch rather than on a
    // layer with nothing to answer it.
    let clock = padding(above_rule, center(text(clock)));
    stack(alloc::vec![rule, clock, words])
}

/// The two menus the bar opens: the applications `query` matches, and the places.
///
/// **One table, indexed by [`APPS`] and [`PLACES`]**, because `MenuState` moves between them
/// with Left and Right the way a window's menu bar does — the only way to reach `Places` on a
/// machine with no pointer.
pub fn menus(
    apps: &[Application],
    query: &str,
    places: &[libfs::Place],
    home: &str,
    theme: &Theme,
) -> Vec<Menu<MenuMsg>> {
    alloc::vec![apps_menu(apps, query), places_menu(places, home, theme)]
}

/// The applications `query` matches, **keyed by index into the unfiltered list**.
///
/// **A row that says why there is nothing**, rather than an empty menu: a filter that matches
/// nothing and a session with no desktop entries would otherwise both look like a menu that
/// failed to draw.
pub fn apps_menu(apps: &[Application], query: &str) -> Menu<MenuMsg> {
    let mut items: Vec<Item<MenuMsg>> = apps
        .iter()
        .enumerate()
        .filter(|(_, a)| matches_app(a, query))
        .map(|(i, a)| Item::plain(a.name.clone(), MenuMsg::Launch(i)))
        .collect();
    if items.is_empty() {
        let why = if apps.is_empty() { "No applications" } else { "Nothing matches" };
        items.push(Item::plain(why, MenuMsg::Nothing).enabled(false));
    }
    Menu { title: "Applications", items }
}

/// The places, each with a swatch and its path beside it.
///
/// **Root's swatch is `deny`**, the design's, and the reason is the one `theme.deny` gives: it is
/// the place that reaches past the person's own files. Matched on the path rather than the name,
/// because the path is what makes it so.
pub fn places_menu(places: &[libfs::Place], home: &str, theme: &Theme) -> Menu<MenuMsg> {
    let items = places
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let swatch = if p.path == "/" { theme.deny } else { theme.accent };
            Item::plain(p.name, MenuMsg::Place(i)).hint(tilde(&p.path, home)).swatch(swatch)
        })
        .collect();
    Menu { title: "Places", items }
}

/// `path` as a person reads it: the home written `~`.
///
/// The design's Places menu shows `~/Documents`, and an application's home is `/home` — a path
/// that is true inside the application's namespace and says nothing about whose home it is.
pub fn tilde(path: &str, home: &str) -> String {
    if home.is_empty() || home == "/" {
        return String::from(path);
    }
    let home = home.trim_end_matches('/');
    if path == home {
        return String::from("~");
    }
    match path.strip_prefix(home).filter(|rest| rest.starts_with('/')) {
        Some(rest) => alloc::format!("~{rest}"),
        None => String::from(path),
    }
}

/// The open menu's tree: the Applications menu with its filter field, or the Places menu.
///
/// `which` is the menu's index, `table` is [`menus`]' answer, and `hovered` is the row key under
/// the pointer — the popup window's own, not the bar's.
pub fn menu_view(
    which: usize,
    table: &[Menu<MenuMsg>],
    state: &MenuState,
    hovered: Option<u64>,
    query: &TextFieldState,
    theme: &Theme,
) -> Element<MenuMsg> {
    let Some(menu) = table.get(which) else { return text("") };
    if which == APPS {
        // **Active whenever the menu is up**: the popup holds the keyboard, and every key the
        // menu does not claim is the field's.
        let field = text_field(query, false, WidgetState { active: true, ..Default::default() }, theme)
            .key(FILTER_KEY);
        sized(Size::new(APPS_MENU_W, 0), popup_headed(menu, state, ROW_KEY_BASE, hovered, field, theme))
    } else {
        popup(menu, state, ROW_KEY_BASE, hovered, theme)
    }
}

/// The desktop-name prompt: a line saying what it is for, and the field.
///
/// **A popup of its own since Part C.** It borrowed the applications modal until the modal became
/// a menu — a field over a list whose rows ignored clicks, because choosing a program is not
/// naming a desktop.
pub fn name_prompt(name: &TextFieldState, theme: &Theme) -> Element<()> {
    let field = text_field(name, false, WidgetState { active: true, ..Default::default() }, theme);
    let body = with_spacing(
        column(alloc::vec![ink(theme.foreground_dim, text("Name this desktop")), field]),
        6,
    );
    sized(
        Size::new(PROMPT_W, 0),
        popup_frame(padding(Insets { top: 8, right: 12, bottom: 10, left: 12 }, body), theme),
    )
}

/// Where the menu at `which` hangs, in the top bar's coordinates — which, the bar being at the
/// screen's origin, are the screen's.
///
/// `word` is the rectangle of the word that opens it, from a layout of [`top_bar`].
pub fn menu_anchor(word: libdraw::geom::Rect) -> (i32, i32) {
    (word.origin.x + MENU_INSET_X, BAR_H as i32 + MENU_DROP)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libdraw::geom::Rect;
    use libdraw::text::Font;
    use libui::diff::Tree;
    use libui::layout::{layout, locate};
    use libui::paint::FontMetrics;
    use libui::route::Router;
    use librsproto::surface::{POINTER_BUTTON, POINTER_MOTION, POINTER_PRESSED, PointerEvent};

    const DEJAVU: &[u8] = include_bytes!("../../../assets/fonts/DejaVuSans.ttf");

    fn font() -> Font {
        Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }

    fn app(name: &str, exec: &str) -> Application {
        Application { name: String::from(name), exec: String::from(exec) }
    }

    /// The three staged applications, in the order `read_applications` sorts them.
    fn staged() -> Vec<Application> {
        alloc::vec![app("Files", "nxfiles"), app("Terminal", "nxterm"), app("Text Editor", "nxedit")]
    }

    /// Press and release at `(x, y)` through a router, as a click arrives.
    fn click<M: Clone>(e: &Element<M>, bounds: Rect, m: &FontMetrics<'_>, x: i32, y: i32) -> Vec<M> {
        let l = layout(e, bounds, m);
        let mut tree = Tree::new();
        tree.update(e, &l).expect("the tree diffs");
        let mut r = Router::new();
        let mut out = Vec::new();
        let at = |kind, flags| PointerEvent { kind, button: 0x110, buttons: 1, flags, x, y, ..Default::default() };
        for ev in [at(POINTER_MOTION, 0), at(POINTER_BUTTON, POINTER_PRESSED), at(POINTER_BUTTON, 0)] {
            out.extend(r.pointer(&tree, e, &l, ev).0);
        }
        out
    }

    /// **The gate's aims, as literals** — `check-login`'s `APPS_CLICK` and `PLACES_CLICK`.
    ///
    /// Written down in `xtask` a second time on purpose; pinned here so a change to the bar that
    /// moves a word under them fails a host test rather than a boot.
    #[test]
    fn the_gates_aims_land_on_the_words_they_name() {
        let (f, theme) = (font(), Theme::light());
        let m = FontMetrics::new(&f, theme.font_px);
        for width in [1024u32, 1280, 1360, 1920] {
            let bounds = Rect::new(0, 0, width, BAR_H);
            let bar = top_bar("12:34", None, None, &theme);
            assert_eq!(click(&bar, bounds, &m, 60, 12), [TopMsg::Menu(APPS)], "APPS_CLICK at {width}");
            assert_eq!(click(&bar, bounds, &m, 158, 12), [TopMsg::Menu(PLACES)], "PLACES_CLICK at {width}");
            // And the clock is not a control: a press on it is a press on the bar.
            assert!(click(&bar, bounds, &m, width as i32 / 2, 12).is_empty(), "the clock at {width}");
        }
    }

    /// The clock is centred on the **screen**, which is what the old bar's balancing slot was for.
    #[test]
    fn the_clock_is_centred_on_the_bar_whatever_is_left_of_it() {
        let (f, theme) = (font(), Theme::light());
        let m = FontMetrics::new(&f, theme.font_px);
        for width in [1024u32, 1360, 2560] {
            let bar = top_bar("12:34", None, None, &theme);
            let l = layout(&bar, Rect::new(0, 0, width, BAR_H), &m);
            // The clock layer is the stack's second: padding → center → text.
            let t = l.children[1].children[0].children[0].rect;
            let mid = t.origin.x + t.size.w as i32 / 2;
            assert!((mid - width as i32 / 2).abs() <= 1, "clock centred at {mid}, not {}", width / 2);
            assert!(t.size.w > 0, "the clock drew nothing");
        }
    }

    /// A menu hangs 8 pixels in from its word and 2 below the bar — the design's positions.
    #[test]
    fn a_menu_hangs_from_the_word_that_opened_it() {
        let (f, theme) = (font(), Theme::light());
        let m = FontMetrics::new(&f, theme.font_px);
        let bar = top_bar("", None, None, &theme);
        let l = layout(&bar, Rect::new(0, 0, 1360, BAR_H), &m);
        let apps = locate(&bar, &l, APPS_KEY).expect("the Applications word");
        let places = locate(&bar, &l, PLACES_KEY).expect("the Places word");
        assert_eq!(menu_anchor(apps), (8, 32));
        assert_eq!(menu_anchor(places), (places.origin.x + 8, 32));
        assert_eq!(places.origin.x, apps.right() as i32, "Places sits right after Applications");
    }

    /// The bar keeps diffing as a word lights and its menu opens — a shape `diff` refuses is a bar
    /// that stops drawing, the failure `menu::popup`'s note describes.
    #[test]
    fn the_bar_diffs_as_its_words_light() {
        let (f, theme) = (font(), Theme::light());
        let m = FontMetrics::new(&f, theme.font_px);
        let bounds = Rect::new(0, 0, 1360, BAR_H);
        let mut tree = Tree::new();
        for (open, hovered) in [(None, None), (None, Some(APPS_KEY)), (Some(APPS), None), (Some(PLACES), Some(PLACES_KEY)), (None, None)] {
            let bar = top_bar("12:34", open, hovered, &theme);
            let l = layout(&bar, bounds, &m);
            assert!(tree.update(&bar, &l).is_ok(), "the bar stopped diffing at {open:?}/{hovered:?}");
        }
    }

    /// Filtering narrows the rows and keeps each one keyed by its place in the unfiltered list.
    #[test]
    fn the_applications_menu_narrows_and_remembers_what_each_row_launches() {
        let apps = staged();
        let all = apps_menu(&apps, "");
        assert_eq!(all.items.len(), 3);
        let term = apps_menu(&apps, "term");
        let msgs: Vec<MenuMsg> = term
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Action { msg, enabled: true, .. } => Some(*msg),
                _ => None,
            })
            .collect();
        assert_eq!(msgs, [MenuMsg::Launch(1)], "`term` is the terminal, which is the second entry");
        // The program as well as the name — somebody who knows the system types `nxedit`.
        assert!(matches!(apps_menu(&apps, "nxedit").items[..], [Item::Action { msg: MenuMsg::Launch(2), .. }]));
    }

    /// A filter that matches nothing says so, in a row that cannot be chosen — and says which
    /// kind of nothing it is.
    #[test]
    fn an_empty_menu_says_why() {
        let apps = staged();
        let none = apps_menu(&apps, "zzz");
        assert!(matches!(&none.items[..], [Item::Action { label, enabled: false, .. }] if label == "Nothing matches"));
        let empty = apps_menu(&[], "");
        assert!(matches!(&empty.items[..], [Item::Action { label, enabled: false, .. }] if label == "No applications"));
        // Enter has nothing to choose: the cursor lands nowhere.
        let mut s = MenuState::new(2);
        s.toggle(APPS);
        s.select_first(&[none, apps_menu(&apps, "")]);
        assert_eq!(s.cursor(), None);
    }

    /// The places are `libfs`'s, in its order, with `~` for home and Root in `deny`.
    #[test]
    fn the_places_menu_is_libfs_places_with_their_paths_beside_them() {
        let theme = Theme::light();
        let places = libfs::places("/home");
        let menu = places_menu(&places, "/home", &theme);
        let rows: Vec<(&str, Option<&str>, Option<libdraw::format::Rgb>)> = menu
            .items
            .iter()
            .filter_map(|it| match it {
                Item::Action { label, hint, swatch, .. } => {
                    Some((label.as_ref(), hint.as_deref(), *swatch))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            rows,
            [
                ("Home", Some("~"), Some(theme.accent)),
                ("Documents", Some("~/Documents"), Some(theme.accent)),
                ("Downloads", Some("~/Downloads"), Some(theme.accent)),
                ("Pictures", Some("~/Pictures"), Some(theme.accent)),
                ("Root", Some("/"), Some(theme.deny)),
            ]
        );
    }

    #[test]
    fn a_path_under_home_is_written_from_the_tilde() {
        assert_eq!(tilde("/home", "/home"), "~");
        assert_eq!(tilde("/home/Documents", "/home"), "~/Documents");
        assert_eq!(tilde("/home/Documents", "/home/"), "~/Documents", "a trailing separator on home");
        assert_eq!(tilde("/", "/home"), "/");
        // A sibling that merely *starts* with home's spelling is not under it.
        assert_eq!(tilde("/homework", "/home"), "/homework");
        // No home, or a home that is the root, abbreviates nothing.
        assert_eq!(tilde("/home", ""), "/home");
        assert_eq!(tilde("/Documents", "/"), "/Documents");
    }

    /// **`check-login`'s row aims, as literals.** The Applications menu hangs at `(8, 32)`, and
    /// the gate types `nxterm` and clicks the one row left at `ROW1`; it opens `Places` and
    /// clicks `Documents` at `PLACE_DOCUMENTS`. Both are screen coordinates.
    #[test]
    fn the_gates_row_aims_land_on_the_rows_they_name() {
        let (f, theme) = (font(), Theme::light());
        let m = FontMetrics::new(&f, theme.font_px);
        let apps = staged();
        let places = libfs::places("/home");
        let bar = top_bar("", None, None, &theme);
        let bl = layout(&bar, Rect::new(0, 0, 1360, BAR_H), &m);

        // The popup's own coordinates are the screen's less its anchor.
        let aim = |which: usize, query: &str, key: u64, (sx, sy): (i32, i32)| {
            let word = locate(&bar, &bl, if which == APPS { APPS_KEY } else { PLACES_KEY }).unwrap();
            let (ax, ay) = menu_anchor(word);
            let table = menus(&apps, query, &places, "/home", &theme);
            let mut q = TextFieldState::new();
            for c in query.chars() {
                q.insert(c);
            }
            let view = menu_view(which, &table, &MenuState::new(2), None, &q, &theme);
            let size = libui::layout::measure(&view, libui::layout::Constraints::loose(Size::new(4000, 4000)), &m);
            let msgs = click(&view, Rect::new(0, 0, size.w, size.h), &m, sx - ax, sy - ay);
            let want = table[which].items.iter().enumerate().find_map(|(i, it)| {
                (ROW_KEY_BASE + i as u64 == key).then(|| match it {
                    Item::Action { msg, .. } => *msg,
                    Item::Separator => MenuMsg::Nothing,
                })
            });
            (msgs, want)
        };
        let (got, want) = aim(APPS, "nxterm", ROW_KEY_BASE, (60, 87));
        assert_eq!(got, [want.unwrap()], "ROW1 is the one row left after typing nxterm");
        assert_eq!(got, [MenuMsg::Launch(1)]);
        let (got, _) = aim(PLACES, "", ROW_KEY_BASE + 1, (180, 80));
        assert_eq!(got, [MenuMsg::Place(1)], "PLACE_DOCUMENTS is the second row");
        assert_eq!(places[1].name, "Documents");
    }

    /// The name prompt measures to a size a popup can be created at — nothing, and everything,
    /// are both refusals in `Child::open`.
    #[test]
    fn the_name_prompt_has_a_size() {
        let (f, theme) = (font(), Theme::light());
        let m = FontMetrics::new(&f, theme.font_px);
        let e = name_prompt(&TextFieldState::new(), &theme);
        let s = libui::layout::measure(&e, libui::layout::Constraints::loose(Size::new(4000, 4000)), &m);
        assert_eq!(s.w, PROMPT_W);
        assert!(s.h > 20 && s.h < 200, "{s:?}");
    }
}
