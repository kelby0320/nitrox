//! The shell's two bars and the menus that hang from the top one, as element trees.
//!
//! Pure, so the layout is host-tested against the gates' own aims, which `xtask` writes down a
//! second time on purpose; the tests pin them as literals so the two copies cannot part. The
//! right-hand end of the design's top panel (quick settings, notifications) is not built
//! (`desktop-shell.md` §9); a power button is there instead.

use alloc::string::String;
use alloc::vec::Vec;

use libdraw::format::Rgb;
use libdraw::geom::Size;
use libui::element::{
    Element, IconKind, Insets, TextSize, center, center_v, column, fill, icon, ink, padding, rounded_fill, row, scaled,
    sized, stack, text, wash, with_spacing,
};
use libui::layout::{Constraints, Metrics, measure};
use libui::menu::{Item, Menu, MenuState, popup, popup_headed};
use libui::widget::{
    CONTROL_RADIUS, DIALOG_GAP, DIALOG_PAD, TextFieldState, Theme, TitleButtons, WidgetState, button, dialog_frame,
    popup_frame, text_field, title_bar,
};

use crate::ending::{Ending, Refusal};
use crate::{Application, BAR_H, matches_app};

/// The Applications menu's index in the bar's table — see [`menus`].
pub const APPS: usize = 0;
/// The Places menu's.
pub const PLACES: usize = 1;
/// The power menu's.
pub const POWER: usize = 2;

/// What a press on the top bar asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TopMsg {
    /// Open, or close, the menu at this index into [`menus`]'s table.
    Menu(usize),
}

/// What choosing a menu row asks for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MenuMsg {
    /// Launch the application at this index into the unfiltered list, so the message still names
    /// the right program after a filter reorders what is shown.
    Launch(usize),
    /// Open the place at this index into [`libfs::places`].
    Place(usize),
    /// End the session: the power menu's rows.
    End(Ending),
    /// What a row that cannot be chosen carries; a disabled row never produces it.
    Nothing,
}

/// The Applications button's element key.
pub const APPS_KEY: u64 = 1;
/// The Places button's.
pub const PLACES_KEY: u64 = 2;
const REST_KEY: u64 = 3;
/// The power button's, at the bar's right-hand end.
pub const POWER_KEY: u64 = 4;
/// The power button's width: a little wider than the bar is tall.
pub const POWER_W: u32 = BAR_H + 4;
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

/// The Applications menu's width: the design's 252 pixels, fixed because the filter field
/// measures as whatever has been typed.
pub const APPS_MENU_W: u32 = 252;
/// How far a menu hangs in from the left of the word that opened it: the design's 8 pixels.
pub const MENU_INSET_X: i32 = 8;
/// How far below the bar it hangs: the design's gap between the panel's edge and the menu's.
pub const MENU_DROP: i32 = 2;

/// The name prompt's width — the Applications menu's, since it is the same kind of popup.
pub const PROMPT_W: u32 = APPS_MENU_W;

/// The top bar: `Applications` and `Places` on the left, the clock centred on the screen, and a
/// rule along its foot. `open` lights its word as the pointer does; `hovered` is the key under
/// the pointer. The ground is the caller's.
pub fn top_bar(clock: &str, open: Option<usize>, hovered: Option<u64>, theme: &Theme) -> Element<TopMsg> {
    let hover = theme.scheme.hover_coverage();
    let face = |on: bool, body: Element<TopMsg>| {
        let mut layers = Vec::with_capacity(2);
        if on {
            layers.push(wash(theme.accent, hover));
        }
        layers.push(body);
        stack(layers)
    };
    let lit = |key: u64, menu: usize| hovered == Some(key) || open == Some(menu);

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
                row(alloc::vec![center_v(dot), center_v(scaled(TextSize::Large, text("Applications"))),]),
                APPS_GAP,
            ),
        ),
    )
    .on_press(TopMsg::Menu(APPS))
    .key(APPS_KEY);
    let words = center_v(scaled(TextSize::Large, text("Places")));
    let places =
        face(lit(PLACES_KEY, PLACES), padding(PLACES_PAD, words)).on_press(TopMsg::Menu(PLACES)).key(PLACES_KEY);

    // Above the rule, not over it: a lit word's wash would break the rule's pixel.
    let above_rule = Insets { top: 0, right: 0, bottom: 1, left: 0 };
    // The power button, at the right-hand end, lit as the words are.
    let power = face(lit(POWER_KEY, POWER), sized(Size::new(POWER_W, BAR_H - 1), icon(IconKind::Power)))
        .on_press(TopMsg::Menu(POWER))
        .key(POWER_KEY);
    let words = padding(
        above_rule,
        row(alloc::vec![apps, places, sized(Size::new(0, 0), text("")).flex(1).key(REST_KEY), power]),
    );
    let rule =
        column(alloc::vec![sized(Size::new(0, 0), text("")).flex(1), sized(Size::new(0, 1), fill(theme.border)),]);
    // Centred on the screen, not on what the words leave: a layer beneath the words.
    let clock = padding(above_rule, center(text(clock)));
    stack(alloc::vec![rule, clock, words])
}

/// The three menus the bar opens: the applications `query` matches, the places, and the power
/// menu, indexed by [`APPS`], [`PLACES`] and [`POWER`] for `MenuState`'s Left and Right.
pub fn menus(
    apps: &[Application],
    query: &str,
    places: &[libfs::Place],
    home: &str,
    theme: &Theme,
) -> Vec<Menu<MenuMsg>> {
    alloc::vec![apps_menu(apps, query), places_menu(places, home, theme), power_menu()]
}

/// The power menu: ending the session, or the machine. Restart and Shut down are destructive rows
/// (`deny`); neither asks here, as the windows are asked first and the policy decides.
pub fn power_menu() -> Menu<MenuMsg> {
    let row = |e: Ending| Item::plain(e.title(), MenuMsg::End(e));
    Menu {
        title: "Power",
        items: alloc::vec![
            row(Ending::LogOut),
            row(Ending::Restart).destructive(true),
            row(Ending::ShutDown).destructive(true),
        ],
    }
}

/// The applications `query` matches, keyed by index into the unfiltered list; a row says why
/// there is nothing.
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

/// The places, each with a swatch and its path; Root's swatch is `deny`.
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

/// The open menu's tree; `hovered` is the row key under the pointer in the popup window.
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
        let field = text_field(query, false, WidgetState { active: true, ..Default::default() }, theme).key(FILTER_KEY);
        sized(Size::new(APPS_MENU_W, 0), popup_headed(menu, state, ROW_KEY_BASE, hovered, field, theme))
    } else {
        popup(menu, state, ROW_KEY_BASE, hovered, theme)
    }
}

/// The desktop-name prompt: a line saying what it is for, and the field, in a popup of its own.
pub fn name_prompt(name: &TextFieldState, theme: &Theme) -> Element<()> {
    let field = text_field(name, false, WidgetState { active: true, ..Default::default() }, theme);
    let body = with_spacing(column(alloc::vec![ink(theme.foreground_dim, text("Name this desktop")), field]), 6);
    sized(Size::new(PROMPT_W, 0), popup_frame(padding(Insets { top: 8, right: 12, bottom: 10, left: 12 }, body), theme))
}

/// Where the menu at `which` hangs, in the top bar's (the screen's) coordinates; `word` is the
/// rectangle of the word that opens it.
pub fn menu_anchor(word: libdraw::geom::Rect) -> (i32, i32) {
    (word.origin.x + MENU_INSET_X, BAR_H as i32 + MENU_DROP)
}

/// Where a menu `width` wide hangs from a word at the right-hand end of the bar: its right edge
/// [`MENU_INSET_X`] in from the word's, so the power menu stays on the screen.
pub fn menu_anchor_right(word: libdraw::geom::Rect, width: u32) -> (i32, i32) {
    (word.origin.x + word.size.w as i32 - MENU_INSET_X - width as i32, BAR_H as i32 + MENU_DROP)
}

/// What the waiting dialog's controls ask for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EndingMsg {
    /// Destroy what is still open, and end the session.
    EndAnyway,
    /// Keep the session: nothing more is asked.
    Cancel,
    /// What the title bar's drag carries: the dialog stays where the shell put it.
    Nothing,
}

/// The waiting dialog's title bar's key.
pub const ENDING_TITLE_KEY: u64 = 200;
/// Its question's.
pub const ENDING_TEXT_KEY: u64 = 201;
/// Its End anyway button's.
pub const ENDING_END_KEY: u64 = 202;
/// Its Cancel button's.
pub const ENDING_CANCEL_KEY: u64 = 203;

/// The waiting dialog: what a session end is still waiting for
/// ([`Closing::question`](crate::ending::Closing::question)'s two lines), with End anyway and
/// Cancel. Its buttons land where `DIALOG_LEFT_CX` and `DIALOG_RIGHT_CX` say, which
/// `check-logout` aims at. Closing it is Cancel: the frame must not end a session.
pub fn ending_dialog(
    ending: Ending,
    question: &(String, String),
    hovered: Option<u64>,
    theme: &Theme,
) -> Element<EndingMsg> {
    let buttons = TitleButtons { minimise: None, maximise: None, close: Some(EndingMsg::Cancel) };
    let bar = title_bar(ending.title(), None, true, EndingMsg::Nothing, buttons, hovered, theme).key(ENDING_TITLE_KEY);
    let lines =
        padding(Insets::all(DIALOG_PAD), column(alloc::vec![text(question.0.clone()), text(question.1.clone())]))
            .key(ENDING_TEXT_KEY);
    let answer = |label: &'static str, msg: EndingMsg, key: u64| {
        button(label, msg, WidgetState { hovered: hovered == Some(key), ..Default::default() }, theme).key(key).flex(1)
    };
    let answers = with_spacing(
        row(alloc::vec![
            answer("End anyway", EndingMsg::EndAnyway, ENDING_END_KEY),
            answer("Cancel", EndingMsg::Cancel, ENDING_CANCEL_KEY),
        ]),
        DIALOG_GAP,
    );
    dialog_frame(bar, lines, answers, true, theme)
}

/// What the refusal dialog's controls ask for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefusalMsg {
    /// Put it away.
    Close,
    /// What the title bar's drag carries: the dialog stays where the shell put it.
    Nothing,
}

/// The refusal dialog's title bar's key.
pub const REFUSAL_TITLE_KEY: u64 = 210;
/// Its two lines'.
pub const REFUSAL_TEXT_KEY: u64 = 211;
/// The empty half where a question's first answer would be.
pub const REFUSAL_GAP_KEY: u64 = 212;
/// Its Close button's.
pub const REFUSAL_CLOSE_KEY: u64 = 213;

/// Why a Restart or a Shut down did not happen: the [`Refusal`]'s two lines, and Close, in the
/// right-hand half where the waiting dialog's Cancel is (`DIALOG_RIGHT_CX`).
pub fn refusal_dialog(ending: Ending, refusal: &Refusal, hovered: Option<u64>, theme: &Theme) -> Element<RefusalMsg> {
    let buttons = TitleButtons { minimise: None, maximise: None, close: Some(RefusalMsg::Close) };
    let bar =
        title_bar(ending.title(), None, true, RefusalMsg::Nothing, buttons, hovered, theme).key(REFUSAL_TITLE_KEY);
    let (first, second) = refusal.lines(ending);
    let lines = padding(Insets::all(DIALOG_PAD), column(alloc::vec![text(first), text(second)])).key(REFUSAL_TEXT_KEY);
    let close = button(
        "Close",
        RefusalMsg::Close,
        WidgetState { hovered: hovered == Some(REFUSAL_CLOSE_KEY), ..Default::default() },
        theme,
    )
    .key(REFUSAL_CLOSE_KEY)
    .flex(1);
    let answers = with_spacing(row(alloc::vec![text("").key(REFUSAL_GAP_KEY).flex(1), close]), DIALOG_GAP);
    dialog_frame(bar, lines, answers, true, theme)
}

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
    /// The window with this id: raise it, or put it away if it holds the keyboard.
    Task(u32),
    /// A middle click on that window's button: ask it to close.
    Close(u32),
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
const RULE_KEY: u64 = 2;
const SPREAD_KEY: u64 = 3;
const SWITCHER_KEY: u64 = 4;
/// The back arrow.
pub const PREV_KEY: u64 = 5;
/// The forward arrow.
pub const NEXT_KEY: u64 = 6;
/// The desktop's name.
pub const NAME_KEY: u64 = 7;
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
// The design's `--r`, shared with windows and fields, is `libui::widget::CONTROL_RADIUS`.
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
/// Everything on the bar that is not a task button or the switcher.
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

/// Which desktops the switcher shows cells for: up to [`MAX_CELLS`], around the current one, the
/// current one in the middle where it can be.
pub fn switcher_cells(current: usize, total: usize) -> core::ops::Range<usize> {
    if total <= MAX_CELLS {
        return 0..total;
    }
    let start = current.saturating_sub(1).min(total - MAX_CELLS);
    start..start + MAX_CELLS
}

/// How many task buttons fit beside a switcher `switcher_w` wide on a bar `width` wide, so the
/// bar never puts a button under the switcher. The switcher's width is measured, as it carries
/// the desktop's name: [`switcher_width`].
pub fn task_capacity(width: u32, switcher_w: u32) -> usize {
    (width.saturating_sub(FIXED_W + switcher_w) / TASK_PITCH) as usize
}

/// The switcher's width, measured — it changes with the desktop's name and how many cells show.
pub fn switcher_width(d: &Desktops<'_>, theme: &Theme, m: &dyn Metrics) -> u32 {
    measure(&switcher(d, None, theme), Constraints::loose(Size::new(u32::MAX / 4, u32::MAX / 4)), m).w
}

/// A rounded box with a one-pixel border: the border colour, then the ground a pixel in; `None`
/// leaves either out.
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
/// `tasks` is what fits ([`task_capacity`] is the caller's, as the minimise chord is bounded by
/// the same count); `shown` lights show-desktop while it holds windows.
pub fn bottom_bar(
    tasks: &[Task<'_>],
    shown: bool,
    desktops: &Desktops<'_>,
    hovered: Option<u64>,
    theme: &Theme,
) -> Element<BottomMsg> {
    let soft = theme.accent.blend(theme.panel, theme.scheme.hover_coverage());
    let line_soft = theme.border.blend(theme.panel, 128);

    let (bd, bg, fg) = if shown {
        (theme.accent, soft, theme.accent)
    } else {
        let bd = if hovered == Some(SHOW_KEY) { theme.accent } else { theme.border };
        (bd, theme.face_hover, theme.foreground_dim)
    };
    // The glyph: a 12×9 window, its top edge three pixels thick.
    let glyph = sized(
        Size::new(12, 9),
        stack(alloc::vec![fill(fg), padding(Insets { top: 3, right: 1, bottom: 1, left: 1 }, fill(bg)),]),
    );
    let mut sd = boxed(Some(bd), Some(bg), CONTROL_RADIUS, theme.panel);
    sd.push(center(glyph));
    let show = center_v(sized(Size::new(SHOW_W, CONTROL_H), stack(sd))).on_press(BottomMsg::ShowDesktop).key(SHOW_KEY);
    let rule =
        center_v(padding(Insets { top: 0, right: 3, bottom: 0, left: 3 }, sized(Size::new(1, 18), fill(line_soft))))
            .key(RULE_KEY);

    let mut items = alloc::vec![show, rule];
    for t in tasks {
        let key = TASK_KEY_BASE + t.id as u64;
        // The dot says what the window is doing: put away, holding the keyboard, or simply there.
        let dot = if t.minimized {
            theme.foreground_dim
        } else if t.focused {
            theme.accent
        } else {
            theme.ok
        };
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
        let task = center_v(sized(Size::new(TASK_W, CONTROL_H), stack(layers))).on_press(BottomMsg::Task(t.id));
        items.push(task.on_middle(BottomMsg::Close(t.id)).key(key));
    }
    items.push(sized(Size::new(0, 0), text("")).flex(1).key(SPREAD_KEY));
    items.push(switcher(desktops, hovered, theme).key(SWITCHER_KEY));

    let rule_top =
        column(alloc::vec![sized(Size::new(0, 1), fill(theme.border)), sized(Size::new(0, 0), text("")).flex(1),]);
    stack(alloc::vec![
        rule_top,
        padding(Insets { top: 1, right: BAR_PAD_X, bottom: 0, left: BAR_PAD_X }, with_spacing(row(items), BAR_GAP),),
    ])
}

/// The switcher: `‹`, up to three cells, `›`, and the desktop's name, which opens the overview.
fn switcher(d: &Desktops<'_>, hovered: Option<u64>, theme: &Theme) -> Element<BottomMsg> {
    let soft = theme.accent.blend(theme.panel, theme.scheme.hover_coverage());
    let line_soft = theme.border.blend(theme.panel, 128);
    let total = d.occupied.len();

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
        // An empty desktop's border is the faint rule; this toolkit has no dashed primitive.
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
    let name = padding(NAME_PAD, center_v(ink(name_ink, text(d.label)))).on_press(BottomMsg::Overview).key(NAME_KEY);

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
mod tests;
