//! The shell's two bars and the menus that hang from the top one, as element trees (desktop
//! refresh, Part C).
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

use libdraw::format::Rgb;
use libdraw::geom::Size;
use libui::element::{
    Element, Insets, TextSize, center, center_v, column, fill, ink, padding, rounded_fill, row,
    scaled, sized, stack, text, wash, with_spacing,
};
use libui::layout::{Constraints, Metrics, measure};
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
            with_spacing(
                row(alloc::vec![
                    center_v(dot),
                    center_v(scaled(TextSize::Large, text("Applications"))),
                ]),
                APPS_GAP,
            ),
        ),
    )
    .on_press(TopMsg::Menu(APPS))
    .key(APPS_KEY);
    let words = center_v(scaled(TextSize::Large, text("Places")));
    let places = face(lit(PLACES_KEY, PLACES), padding(PLACES_PAD, words))
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

// ---- the bottom bar -------------------------------------------------------------------------

/// What the bottom bar knows about one window: enough to draw its button.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Task<'a> {
    /// The compositor's id, which is what a press on the button names.
    pub id: u32,
    /// What it reads — the window's title, or the shell's stand-in for a window with none.
    pub title: &'a str,
    /// Whether it holds the keyboard.
    pub focused: bool,
    /// Whether the shell has put it away.
    pub minimized: bool,
}

/// What the switcher shows: every desktop, whether each has windows, and which one this is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Desktops<'a> {
    /// One flag per desktop, in order: whether any window is on it.
    pub occupied: &'a [bool],
    /// The current desktop, as an index into `occupied`.
    pub current: usize,
    /// The current desktop's name — its own, or `Desktop N`.
    pub label: &'a str,
}

/// What a press on the bottom bar asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BottomMsg {
    /// Minimise everything on this desktop, or bring back what the last press put away.
    ShowDesktop,
    /// The window with this id — raise it, or put it away if it holds the keyboard. A middle
    /// click closes it instead, which the shell reads off the event, since a message cannot say
    /// which button made it.
    Task(u32),
    /// Go to the desktop at this index.
    Desktop(usize),
    /// Go to the previous desktop.
    Previous,
    /// Go to the next desktop.
    Next,
    /// Open the overview: the desktop's name is the switcher's "all desktops".
    Overview,
}

/// The show-desktop button's key.
pub const SHOW_KEY: u64 = 1;
/// The rule after it.
const RULE_KEY: u64 = 2;
/// The empty stretch between the tasks and the switcher.
const SPREAD_KEY: u64 = 3;
/// The switcher, as one group.
const SWITCHER_KEY: u64 = 4;
/// The back arrow.
pub const PREV_KEY: u64 = 5;
/// The forward arrow.
pub const NEXT_KEY: u64 = 6;
/// The desktop's name.
pub const NAME_KEY: u64 = 7;
/// The cells, as a group.
const CELLS_KEY: u64 = 8;
/// Where the cells are keyed from, by desktop index.
pub const CELL_KEY_BASE: u64 = 100;
/// Where the task buttons are keyed from, by window id — clear of every other key here, since a
/// window id is any `u32`.
pub const TASK_KEY_BASE: u64 = 1 << 32;

/// The bar's side padding: `padding: 0 7px`.
pub const BAR_PAD_X: u32 = 7;
/// The gap between the bar's items: `gap: 4px`.
pub const BAR_GAP: u32 = 4;
/// The show-desktop button: `26px × 22px`.
pub const SHOW_W: u32 = 26;
/// Every control's height on the bar: `22px`.
const CONTROL_H: u32 = 22;
/// The design's `--r`, which buttons on the bar share with windows.
const CONTROL_RADIUS: u32 = 8;
/// The rule after the show-desktop button: one pixel with `margin: 0 3px`.
const RULE_W: u32 = 1 + 2 * 3;
/// A task button's width: `186px`.
pub const TASK_W: u32 = 186;
/// Its sides: `padding: 0 9px`.
const TASK_PAD_X: u32 = 9;
/// Its dot: `5px`.
const TASK_DOT: u32 = 5;
/// The gap between the dot and the title: `gap: 7px`.
const TASK_GAP: u32 = 7;
/// Where the first task button starts, from the bar's left edge.
pub const TASKS_X: u32 = BAR_PAD_X + SHOW_W + BAR_GAP + RULE_W + BAR_GAP;
/// From one task button's left edge to the next.
pub const TASK_PITCH: u32 = TASK_W + BAR_GAP;
/// Everything on the bar that is not a task button or the switcher: both paddings, the
/// show-desktop button, the rule, and the gaps around the empty stretch.
const FIXED_W: u32 = 2 * BAR_PAD_X + SHOW_W + RULE_W + 3 * BAR_GAP;

/// The switcher's padding: `padding: 0 8px`, inside its `border-left`.
const GROUP_PAD_X: u32 = 8;
/// The gap between the switcher's parts: `gap: 5px`.
const GROUP_GAP: u32 = 5;
/// An arrow: `17px × 18px`.
const ARROW: (u32, u32) = (17, 18);
/// A desktop cell: `24px × 16px`.
const CELL: (u32, u32) = (24, 16);
/// The gap between cells: `gap: 3px`.
const CELL_GAP: u32 = 3;
/// The mark in an occupied cell: `11px × 3px`, `3px` in and `4px` down from its border.
const MARK: (u32, u32) = (11, 3);
/// The name's sides: `padding: 0 5px`, and `margin-left: 2px` before it.
const NAME_PAD: Insets = Insets { top: 0, right: 5, bottom: 0, left: 5 + 2 };
/// The most cells the switcher shows at once.
pub const MAX_CELLS: usize = 3;

/// Which desktops the switcher shows cells for: up to [`MAX_CELLS`], around the current one.
///
/// **`min(3, total)`, never fewer** (the plan's rule, from the review that corrected a reading of
/// a screenshot with two desktops open): with three or more, three cells, the current one in the
/// middle where it can be — first, it is the first cell and the back arrow is disabled; last, it
/// is the last.
pub fn switcher_cells(current: usize, total: usize) -> core::ops::Range<usize> {
    if total <= MAX_CELLS {
        return 0..total;
    }
    let start = current.saturating_sub(1).min(total - MAX_CELLS);
    start..start + MAX_CELLS
}

/// How many task buttons fit beside a switcher `switcher_w` wide on a bar `width` wide.
///
/// **The invariant is the product**: the bar laid out with this many never puts a button under
/// the switcher. Entries past it are not shown; the window is still there, and on the overview.
/// The switcher's width is measured rather than written down because it carries the desktop's
/// name — see [`switcher_width`].
pub fn task_capacity(width: u32, switcher_w: u32) -> usize {
    (width.saturating_sub(FIXED_W + switcher_w) / TASK_PITCH) as usize
}

/// The switcher's width, measured — it changes with the desktop's name and how many cells show.
pub fn switcher_width(d: &Desktops<'_>, theme: &Theme, m: &dyn Metrics) -> u32 {
    measure(&switcher(d, None, theme), Constraints::loose(Size::new(u32::MAX / 4, u32::MAX / 4)), m).w
}

/// A rounded box with a one-pixel border, on the bar: the border colour, then the ground a pixel
/// in. `None` for either leaves it out, so a borderless box is its ground and a hollow one is a
/// ring on the bar's own.
fn boxed<Msg>(border: Option<Rgb>, ground: Option<Rgb>, radius: u32, panel: Rgb) -> Vec<Element<Msg>> {
    let mut layers = Vec::with_capacity(2);
    match (border, ground) {
        (Some(b), g) => {
            layers.push(rounded_fill(b, radius));
            layers.push(padding(Insets::all(1), rounded_fill(g.unwrap_or(panel), radius.saturating_sub(1))));
        }
        (None, Some(g)) => layers.push(rounded_fill(g, radius)),
        (None, None) => {}
    }
    layers
}

/// The bottom bar: show-desktop, a rule, one button per window, and the switcher at the right.
///
/// `tasks` is what fits — [`task_capacity`] is the caller's, because the chord that minimises the
/// focused window has to be bounded by the same count. `shown` is whether show-desktop is holding
/// windows to bring back, which lights it. The ground is the caller's, as the top bar's is.
pub fn bottom_bar(
    tasks: &[Task<'_>],
    shown: bool,
    desktops: &Desktops<'_>,
    hovered: Option<u64>,
    theme: &Theme,
) -> Element<BottomMsg> {
    let soft = theme.accent.blend(theme.panel, theme.scheme.hover_coverage());
    let line_soft = theme.border.blend(theme.panel, 128);

    // **Show desktop**: lit in the accent while it is holding windows, so the second press has a
    // visible reason to exist.
    let (bd, bg, fg) = if shown {
        (theme.accent, soft, theme.accent)
    } else {
        let bd = if hovered == Some(SHOW_KEY) { theme.accent } else { theme.border };
        (bd, theme.face_hover, theme.foreground_dim)
    };
    // The glyph: a 12×9 window, its top edge three pixels thick.
    let glyph = sized(
        Size::new(12, 9),
        stack(alloc::vec![
            fill(fg),
            padding(Insets { top: 3, right: 1, bottom: 1, left: 1 }, fill(bg)),
        ]),
    );
    let mut sd = boxed(Some(bd), Some(bg), CONTROL_RADIUS, theme.panel);
    sd.push(center(glyph));
    let show = center_v(sized(Size::new(SHOW_W, CONTROL_H), stack(sd)))
        .on_press(BottomMsg::ShowDesktop)
        .key(SHOW_KEY);
    let rule = center_v(padding(
        Insets { top: 0, right: 3, bottom: 0, left: 3 },
        sized(Size::new(1, 18), fill(line_soft)),
    ))
    .key(RULE_KEY);

    let mut items = alloc::vec![show, rule];
    for t in tasks {
        let key = TASK_KEY_BASE + t.id as u64;
        // **The dot says what the window is doing**: put away, holding the keyboard, or simply
        // there — the design's three states, where the old bar had a character in the label.
        let dot = if t.minimized {
            theme.foreground_dim
        } else if t.focused {
            theme.accent
        } else {
            theme.ok
        };
        // **The focused window is a raised face**, bordered, on the window's own ground; the
        // pointer draws the border in the accent on any of them.
        let border = if hovered == Some(key) {
            Some(theme.accent)
        } else if t.focused && !t.minimized {
            Some(theme.border)
        } else {
            None
        };
        let ground = t.focused.then_some(theme.background);
        let mut layers = boxed(border, ground, CONTROL_RADIUS, theme.panel);
        layers.push(padding(
            Insets { top: 0, right: TASK_PAD_X, bottom: 0, left: TASK_PAD_X },
            with_spacing(
                row(alloc::vec![
                    center_v(sized(Size::new(TASK_DOT, TASK_DOT), rounded_fill(dot, TASK_DOT / 2))),
                    center_v(text(t.title)),
                ]),
                TASK_GAP,
            ),
        ));
        items.push(
            center_v(sized(Size::new(TASK_W, CONTROL_H), stack(layers)))
                .on_press(BottomMsg::Task(t.id))
                .key(key),
        );
    }
    items.push(sized(Size::new(0, 0), text("")).flex(1).key(SPREAD_KEY));
    items.push(switcher(desktops, hovered, theme).key(SWITCHER_KEY));

    let rule_top = column(alloc::vec![
        sized(Size::new(0, 1), fill(theme.border)),
        sized(Size::new(0, 0), text("")).flex(1),
    ]);
    stack(alloc::vec![
        rule_top,
        padding(
            Insets { top: 1, right: BAR_PAD_X, bottom: 0, left: BAR_PAD_X },
            with_spacing(row(items), BAR_GAP),
        ),
    ])
}

/// The switcher: `‹`, up to three cells, `›`, and the desktop's name — which opens the overview.
fn switcher(d: &Desktops<'_>, hovered: Option<u64>, theme: &Theme) -> Element<BottomMsg> {
    let soft = theme.accent.blend(theme.panel, theme.scheme.hover_coverage());
    let line_soft = theme.border.blend(theme.panel, 128);
    let total = d.occupied.len();

    // An arrow: dim, and deaf, when there is nowhere to go.
    let arrow = |glyph: &'static str, key: u64, live: bool, msg: BottomMsg| {
        let mut layers = Vec::with_capacity(2);
        if live && hovered == Some(key) {
            layers.push(rounded_fill(soft, 2));
        }
        let ink_c = if live { theme.foreground } else { theme.border };
        layers.push(center(ink(ink_c, text(glyph))));
        let e = center_v(sized(Size::new(ARROW.0, ARROW.1), stack(layers)));
        if live { e.on_press(msg).key(key) } else { e.key(key) }
    };

    let mut cells = Vec::with_capacity(MAX_CELLS);
    for i in switcher_cells(d.current, total) {
        let (cur, occ) = (i == d.current, d.occupied.get(i).copied().unwrap_or(false));
        // **An empty desktop's border is the faint rule**, where the design dashes it: the
        // difference it draws is "nothing here", and the missing mark already says that — a
        // dashed rectangle is a primitive this toolkit does not have, for one border.
        let border = if cur {
            theme.accent
        } else if occ {
            theme.border
        } else {
            line_soft
        };
        let ground = if cur { soft } else { theme.background };
        let mut layers = boxed(Some(border), Some(ground), 2, theme.panel);
        if occ {
            let mark = if cur { theme.accent } else { theme.border };
            layers.push(padding(
                Insets { top: 1 + 4, right: 0, bottom: 0, left: 1 + 3 },
                sized(Size::new(MARK.0, MARK.1), rounded_fill(mark, 1)),
            ));
        }
        cells.push(
            center_v(sized(Size::new(CELL.0, CELL.1), stack(layers)))
                .on_press(BottomMsg::Desktop(i))
                .key(CELL_KEY_BASE + i as u64),
        );
    }
    let name_ink = if hovered == Some(NAME_KEY) { theme.accent } else { theme.foreground };
    let name = padding(NAME_PAD, center_v(ink(name_ink, text(d.label))))
        .on_press(BottomMsg::Overview)
        .key(NAME_KEY);

    let parts = with_spacing(
        row(alloc::vec![
            arrow("\u{2039}", PREV_KEY, d.current > 0, BottomMsg::Previous),
            with_spacing(row(cells), CELL_GAP).key(CELLS_KEY),
            arrow("\u{203A}", NEXT_KEY, d.current + 1 < total, BottomMsg::Next),
            name,
        ]),
        GROUP_GAP,
    );
    // The group's `border-left`, full control height, and its padding.
    row(alloc::vec![
        center_v(sized(Size::new(1, CONTROL_H), fill(line_soft))),
        padding(Insets { top: 0, right: GROUP_PAD_X, bottom: 0, left: GROUP_PAD_X }, parts),
    ])
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

    /// `check-login`'s aim at the forward arrow, from the screen's right edge, with `work`
    /// current and one scratch desktop after it.
    const NEXT_FROM_RIGHT: i32 = 67;
    /// Its aim at the first cell, from the right edge, with `Desktop 2` current of two.
    const FIRST_CELL_FROM_RIGHT: i32 = 148;

    fn font() -> Font {
        Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
    }

    /// The text sizes every aim must hold at: **12, what the image stages** — `xtask`'s
    /// `THEME_FONT_PX`, and so what every gate boots — and 13, the built-in theme's, for a session
    /// with no theme file (both since the desktop refresh's Part G; they were 14 and 16). The bars
    /// are laid out in the session's theme, so an aim pinned at one size could drift out of its
    /// target at the other with nothing failing; the first version of these tests pinned the
    /// built-in size alone while the guest drew the staged one (PR #314 review, finding 4).
    const FONT_SIZES: [f32; 2] = [12.0, 13.0];

    /// The light theme at each of [`FONT_SIZES`].
    fn themes() -> impl Iterator<Item = Theme> {
        FONT_SIZES.into_iter().map(|px| Theme { font_px: px, ..Theme::light() })
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
        let f = font();
        for theme in themes() {
            let m = FontMetrics::new(&f, theme.font_px);
            for width in [1024u32, 1280, 1360, 1920] {
                let bounds = Rect::new(0, 0, width, BAR_H);
                let bar = top_bar("12:34", None, None, &theme);
                assert_eq!(click(&bar, bounds, &m, 60, 12), [TopMsg::Menu(APPS)], "APPS_CLICK at {width}");
                let places = click(&bar, bounds, &m, 134, 12);
                assert_eq!(places, [TopMsg::Menu(PLACES)], "PLACES_CLICK at {width}");
                // And the clock is not a control: a press on it is a press on the bar.
                assert!(click(&bar, bounds, &m, width as i32 / 2, 12).is_empty(), "the clock at {width}");
            }
        }
    }

    /// The clock is centred on the **screen**, which is what the old bar's balancing slot was for.
    #[test]
    fn the_clock_is_centred_on_the_bar_whatever_is_left_of_it() {
        let f = font();
        for theme in themes() {
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
    }

    /// A menu hangs 8 pixels in from its word and 2 below the bar — the design's positions.
    #[test]
    fn a_menu_hangs_from_the_word_that_opened_it() {
        let f = font();
        for theme in themes() {
            let m = FontMetrics::new(&f, theme.font_px);
            let bar = top_bar("", None, None, &theme);
            let l = layout(&bar, Rect::new(0, 0, 1360, BAR_H), &m);
            let apps = locate(&bar, &l, APPS_KEY).expect("the Applications word");
            let places = locate(&bar, &l, PLACES_KEY).expect("the Places word");
            assert_eq!(menu_anchor(apps), (8, 32));
            assert_eq!(menu_anchor(places), (places.origin.x + 8, 32));
            assert_eq!(places.origin.x, apps.right() as i32, "Places sits right after Applications");
        }
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
        let f = font();
        for theme in themes() {
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
            let (got, want) = aim(APPS, "nxterm", ROW_KEY_BASE, (60, 82));
            assert_eq!(got, [want.unwrap()], "ROW1 is the one row left after typing nxterm");
            assert_eq!(got, [MenuMsg::Launch(1)]);
            let (got, _) = aim(PLACES, "", ROW_KEY_BASE + 1, (180, 75));
            assert_eq!(got, [MenuMsg::Place(1)], "PLACE_DOCUMENTS is the second row");
            assert_eq!(places[1].name, "Documents");
        }
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

    // --- the bottom bar -------------------------------------------------------------------------

    fn tasks(n: usize) -> Vec<Task<'static>> {
        (0..n)
            .map(|i| Task { id: 10 + i as u32, title: "nxterm", focused: i == 0, minimized: false })
            .collect()
    }

    /// The plan's rule: `min(3, total)` cells, the current one in the middle where it can be.
    #[test]
    fn the_switcher_shows_three_cells_around_the_current_desktop() {
        assert_eq!(switcher_cells(0, 1), 0..1);
        assert_eq!(switcher_cells(1, 2), 0..2);
        assert_eq!(switcher_cells(0, 3), 0..3);
        // On desktop 1 of three or more: three cells, not "two when there is no previous".
        assert_eq!(switcher_cells(0, 5), 0..3);
        assert_eq!(switcher_cells(1, 5), 0..3);
        assert_eq!(switcher_cells(2, 5), 1..4);
        assert_eq!(switcher_cells(4, 5), 2..5, "the last desktop is the last cell");
        for total in 1..8 {
            for cur in 0..total {
                let r = switcher_cells(cur, total);
                assert!(r.contains(&cur), "{cur} of {total} is not shown: {r:?}");
                assert_eq!(r.len(), total.min(MAX_CELLS));
                assert!(r.end <= total);
            }
        }
    }

    /// **No task button is ever laid out under the switcher**, at any screen and any name — the
    /// product the capacity exists for — and the capacity is tight: one more would not fit.
    #[test]
    fn no_task_is_ever_laid_out_under_the_switcher() {
        let f = font();
        for theme in themes() {
            let m = FontMetrics::new(&f, theme.font_px);
            let occupied = [true, false, true, true, false];
            for width in [640u32, 1024, 1280, 1360, 1366, 1920, 2560] {
                for label in ["Desktop 1", "cli", "work", "a desktop with a long name, 32b"] {
                    for total in [1usize, 2, 5] {
                        let d = Desktops { occupied: &occupied[..total], current: total - 1, label };
                        let n = task_capacity(width, switcher_width(&d, &theme, &m));
                        let ts = tasks(n);
                        let bar = bottom_bar(&ts, false, &d, None, &theme);
                        let l = layout(&bar, Rect::new(0, 0, width, BAR_H), &m);
                        // The whole group, from its rule — not the name, which starts further in.
                        let sw = locate(&bar, &l, SWITCHER_KEY).expect("the switcher is laid out");
                        assert_eq!(sw.size.w, switcher_width(&d, &theme, &m), "{width}/{label}: squeezed");
                        for t in &ts {
                            let r = locate(&bar, &l, TASK_KEY_BASE + t.id as u64).unwrap();
                            assert!(
                                r.right() <= sw.origin.x as i64 && r.size.w == TASK_W,
                                "{width}/{label}/{total}: task {} at {r:?} meets the switcher at {sw:?}",
                                t.id
                            );
                        }
                        let name = locate(&bar, &l, NAME_KEY).expect("the name is laid out");
                        assert!(name.right() <= width as i64, "{width}/{label}: the name runs off the bar");
                        // Tight: the next button would not have fitted.
                        let next_right = TASKS_X + n as u32 * TASK_PITCH + TASK_W;
                        let switcher_x = width - BAR_PAD_X - switcher_width(&d, &theme, &m);
                        assert!(next_right + BAR_GAP > switcher_x - BAR_GAP, "{width}/{label}: room for one more");
                    }
                }
            }
        }
    }

    /// **`check-login`'s bottom-bar aims, as literals.** Show-desktop at x 20; the first task's
    /// middle at 141 and the next at 331; the bar's empty stretch at 600; the desktop's name at
    /// 30 in from the right edge; and, with the desktops named `work` and one scratch, the
    /// forward arrow and the first cell measured from the right edge too. All at half the bar.
    #[test]
    fn the_gates_bottom_bar_aims_land_on_what_they_name() {
        let f = font();
        for theme in themes() {
            let m = FontMetrics::new(&f, theme.font_px);
            let y = BAR_H as i32 / 2;
            for width in [1024u32, 1280, 1360, 1920, 2560] {
                let bounds = Rect::new(0, 0, width, BAR_H);
                let w = width as i32;
                for label in ["Desktop 1", "work", "cli", "Desktop 2"] {
                    let occupied = [true, false];
                    let d = Desktops { occupied: &occupied, current: 0, label };
                    let ts = tasks(2);
                    let bar = bottom_bar(&ts, false, &d, None, &theme);
                    assert_eq!(click(&bar, bounds, &m, 20, y), [BottomMsg::ShowDesktop], "{width}");
                    assert_eq!(click(&bar, bounds, &m, 141, y), [BottomMsg::Task(10)], "{width}");
                    assert_eq!(click(&bar, bounds, &m, 331, y), [BottomMsg::Task(11)], "{width}");
                    assert!(click(&bar, bounds, &m, 600, y).is_empty(), "{width}: 600 is not empty space");
                    assert_eq!(click(&bar, bounds, &m, w - 30, y), [BottomMsg::Overview], "{width}/{label}");
                }
                // The two the switcher step aims at, with `work` current and the scratch desktop
                // after it — the state `check-login` is in when it presses them.
                let occupied = [true, false];
                let d = Desktops { occupied: &occupied, current: 0, label: "work" };
                let bar = bottom_bar(&tasks(1), false, &d, None, &theme);
                assert_eq!(click(&bar, bounds, &m, w - NEXT_FROM_RIGHT, y), [BottomMsg::Next], "{width}");
                // …and from the second desktop, named `Desktop 2`, the first cell and the back arrow.
                let d = Desktops { occupied: &occupied, current: 1, label: "Desktop 2" };
                let bar = bottom_bar(&[], false, &d, None, &theme);
                assert_eq!(
                    click(&bar, bounds, &m, w - FIRST_CELL_FROM_RIGHT, y),
                    [BottomMsg::Desktop(0)],
                    "{width}"
                );
            }
        }
    }

    /// An arrow with nowhere to go does nothing, and says so by its ink.
    #[test]
    fn an_arrow_with_nowhere_to_go_is_deaf() {
        let (f, theme) = (font(), Theme::light());
        let m = FontMetrics::new(&f, theme.font_px);
        let bounds = Rect::new(0, 0, 1360, BAR_H);
        let occupied = [true, true, false];
        for (current, prev_live, next_live) in [(0, false, true), (1, true, true), (2, true, false)] {
            let d = Desktops { occupied: &occupied, current, label: "x" };
            let bar = bottom_bar(&[], false, &d, None, &theme);
            let l = layout(&bar, bounds, &m);
            let at = |key| {
                let r = locate(&bar, &l, key).unwrap();
                (r.origin.x + r.size.w as i32 / 2, r.origin.y + r.size.h as i32 / 2)
            };
            let (px, py) = at(PREV_KEY);
            let (nx, ny) = at(NEXT_KEY);
            assert_eq!(click(&bar, bounds, &m, px, py) == [BottomMsg::Previous], prev_live, "prev at {current}");
            assert_eq!(click(&bar, bounds, &m, nx, ny) == [BottomMsg::Next], next_live, "next at {current}");
        }
    }

    /// Every colour the bar draws a state in, read off the tree: the dots, the focused face,
    /// show-desktop lit, and the current cell.
    #[test]
    fn the_bar_draws_each_state_in_its_colour() {
        let theme = Theme::light();
        fn fills<M>(e: &Element<M>, out: &mut Vec<Rgb>) {
            if let libui::element::Node::RoundedFill { colour, .. } = &e.node {
                out.push(*colour);
            }
            for c in e.children() {
                fills(c, out);
            }
        }
        // **The task's own element, found by its key** — not the whole bar, whose current desktop
        // cell draws in the accent too and hid a focused dot of any colour (PR #314 review,
        // blocking 3).
        fn keyed<M>(e: &Element<M>, key: u64) -> Option<&Element<M>> {
            if e.key == Some(key) {
                return Some(e);
            }
            e.children().find_map(|c| keyed(c, key))
        }
        let task = |t: Task<'static>| {
            let d = Desktops { occupied: &[true], current: 0, label: "x" };
            let mut out = Vec::new();
            let bar = bottom_bar(&[t], false, &d, None, &theme);
            fills(keyed(&bar, TASK_KEY_BASE + t.id as u64).expect("the task is keyed"), &mut out);
            out
        };
        let base = Task { id: 1, title: "t", focused: false, minimized: false };
        assert!(task(base).contains(&theme.ok), "a window that is simply there is `ok`");
        let focused = task(Task { focused: true, ..base });
        assert!(focused.contains(&theme.accent), "the focused window's dot is the accent");
        assert!(focused.contains(&theme.background), "and its face is the window's ground");
        let min = task(Task { minimized: true, ..base });
        assert!(min.contains(&theme.foreground_dim), "a minimised window's dot is dim");
        assert!(!min.contains(&theme.ok) && !min.contains(&theme.background));

        let d = Desktops { occupied: &[true], current: 0, label: "x" };
        let mut quiet = Vec::new();
        fills(&bottom_bar(&[], false, &d, None, &theme), &mut quiet);
        let mut lit = Vec::new();
        fills(&bottom_bar(&[], true, &d, None, &theme), &mut lit);
        let soft = theme.accent.blend(theme.panel, theme.scheme.hover_coverage());
        assert!(lit.iter().filter(|c| **c == theme.accent).count() > quiet.iter().filter(|c| **c == theme.accent).count(),
            "show-desktop is lit in the accent while it is holding windows");
        assert!(lit.contains(&soft) && quiet.contains(&theme.face_hover));
    }

    /// The bar keeps diffing as windows come and go, focus moves and the pointer lights things —
    /// a shape `diff` refuses is a bar that stops drawing.
    #[test]
    fn the_bottom_bar_diffs_as_its_windows_change() {
        let (f, theme) = (font(), Theme::light());
        let m = FontMetrics::new(&f, theme.font_px);
        let bounds = Rect::new(0, 0, 1360, BAR_H);
        let mut tree = Tree::new();
        let occupied = [true, false, false];
        for (n, shown, current, hovered) in [
            (0, false, 0, None),
            (1, false, 0, Some(TASK_KEY_BASE + 10)),
            (3, true, 1, Some(SHOW_KEY)),
            (2, false, 2, Some(NAME_KEY)),
            (0, false, 0, None),
        ] {
            let d = Desktops { occupied: &occupied, current, label: "Desktop 1" };
            let bar = bottom_bar(&tasks(n), shown, &d, hovered, &theme);
            let l = layout(&bar, bounds, &m);
            assert!(tree.update(&bar, &l).is_ok(), "the bar stopped diffing at {n} tasks");
        }
    }

    /// The arrows are characters, and a character the shipped face does not carry draws as
    /// nothing with nothing reported — the reason `libui` pins its menu mark the same way.
    #[test]
    fn the_arrows_exist_in_the_shipped_face() {
        let f = font();
        assert!(f.has_glyph('\u{2039}') && f.has_glyph('\u{203A}'));
    }
}
