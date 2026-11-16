use super::*;
use libdraw::geom::Rect;
use libdraw::text::Font;
use librsproto::surface::{POINTER_BUTTON, POINTER_MOTION, POINTER_PRESSED, PointerEvent};
use libui::diff::Tree;
use libui::layout::{layout, locate};
use libui::paint::FontMetrics;
use libui::route::Router;

const DEJAVU: &[u8] = include_bytes!("../../../../assets/fonts/DejaVuSans.ttf");

/// `check-login`'s aim at the forward arrow, from the right edge, with `work` current and one
/// scratch desktop after it.
const NEXT_FROM_RIGHT: i32 = 67;
/// Its aim at the first cell, from the right edge, with `Desktop 2` current of two.
const FIRST_CELL_FROM_RIGHT: i32 = 148;

fn font() -> Font {
    Font::from_bytes(DEJAVU.to_vec()).expect("the vendored font parses")
}

/// The text sizes every aim must hold at: 12, what the image stages (`xtask`'s
/// `THEME_FONT_PX`), and 13, the built-in theme's.
const FONT_SIZES: [f32; 2] = [12.0, 13.0];

const APPS_CLICK: i32 = 60;
const PLACES_CLICK: i32 = 141;

/// How far inside its word each press must land, at either text size.
const AIM_MARGIN: i32 = 20;

fn themes() -> impl Iterator<Item = Theme> {
    FONT_SIZES.into_iter().map(|px| Theme { font_px: px, ..Theme::light() })
}

fn app(name: &str, exec: &str) -> Application {
    Application { name: String::from(name), exec: String::from(exec) }
}

fn staged() -> Vec<Application> {
    alloc::vec![app("Files", "nxfiles"), app("Terminal", "nxterm"), app("Text Editor", "nxedit")]
}

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

/// The gate's aims, as literals (`check-login`'s `APPS_CLICK`, `PLACES_CLICK`), so a bar
/// change that moves a word under them fails a host test, not a boot; each [`AIM_MARGIN`]
/// inside its word at both sizes.
#[test]
fn the_gates_aims_land_on_the_words_they_name() {
    let f = font();
    for theme in themes() {
        let m = FontMetrics::new(&f, theme.font_px);
        for width in [1024u32, 1280, 1360, 1920] {
            let bounds = Rect::new(0, 0, width, BAR_H);
            let bar = top_bar("12:34", None, None, &theme);
            let apps = click(&bar, bounds, &m, APPS_CLICK, 12);
            assert_eq!(apps, [TopMsg::Menu(APPS)], "APPS_CLICK at {width}");
            let places = click(&bar, bounds, &m, PLACES_CLICK, 12);
            assert_eq!(places, [TopMsg::Menu(PLACES)], "PLACES_CLICK at {width}");
            assert!(click(&bar, bounds, &m, width as i32 / 2, 12).is_empty(), "the clock at {width}");
        }
        let bar = top_bar("12:34", None, None, &theme);
        let l = layout(&bar, Rect::new(0, 0, 1360, BAR_H), &m);
        let aims = [(APPS_CLICK, APPS_KEY, "APPS_CLICK"), (PLACES_CLICK, PLACES_KEY, "PLACES_CLICK")];
        for (aim, key, what) in aims {
            let w = locate(&bar, &l, key).expect("the word");
            let (left, right) = (w.origin.x, w.origin.x + w.size.w as i32);
            let margin = (aim - left).min(right - aim);
            assert!(
                margin >= AIM_MARGIN,
                "{what} is {margin} px inside {left}..{right} at font_px {}, under \
                 {AIM_MARGIN}",
                theme.font_px
            );
        }
    }
}

#[test]
fn the_clock_is_centred_on_the_bar_whatever_is_left_of_it() {
    let f = font();
    for theme in themes() {
        let m = FontMetrics::new(&f, theme.font_px);
        for width in [1024u32, 1360, 2560] {
            let bar = top_bar("12:34", None, None, &theme);
            let l = layout(&bar, Rect::new(0, 0, width, BAR_H), &m);
            let t = l.children[1].children[0].children[0].rect;
            let mid = t.origin.x + t.size.w as i32 / 2;
            assert!((mid - width as i32 / 2).abs() <= 1, "clock centred at {mid}, not {}", width / 2);
            assert!(t.size.w > 0, "the clock drew nothing");
        }
    }
}

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

/// The power button is the bar's right-hand end at every width; `check-logout` aims `POWER_W
/// / 2` in from the right edge.
#[test]
fn the_power_button_is_the_bars_right_hand_end() {
    let f = font();
    for theme in themes() {
        let m = FontMetrics::new(&f, theme.font_px);
        for width in [1024u32, 1280, 1360, 1920, 2560] {
            let bar = top_bar("12:34", None, None, &theme);
            let l = layout(&bar, Rect::new(0, 0, width, BAR_H), &m);
            let power = locate(&bar, &l, POWER_KEY).expect("the power button");
            assert_eq!(power.right() as u32, width, "flush with the right edge at {width}");
            assert_eq!(power.size.w, POWER_W);
            // `xtask`'s `chrome::POWER_FROM_RIGHT`, as a literal on purpose (M11 decision 2).
            let at = width as i32 - 17;
            assert_eq!(POWER_W as i32 / 2, 17, "the gate aims at the button's middle");
            assert_eq!(click(&bar, Rect::new(0, 0, width, BAR_H), &m, at, 12), [TopMsg::Menu(POWER)], "at {width}");
        }
    }
}

/// The power menu is Log out, Restart and Shut down in that order (`check-logout` presses
/// Down that many times), the last two destructive.
#[test]
fn the_power_menu_logs_out_restarts_and_shuts_down() {
    let table = menus(&staged(), "", &libfs::places("/home"), "/home", &Theme::light());
    assert_eq!(table.len(), 3);
    assert_eq!(table[POWER].title, "Power");
    let rows: Vec<(&str, MenuMsg, bool)> = table[POWER]
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Action { label, msg, enabled: true, destructive, .. } => Some((label.as_ref(), *msg, *destructive)),
            _ => None,
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("Log out", MenuMsg::End(Ending::LogOut), false),
            ("Restart", MenuMsg::End(Ending::Restart), true),
            ("Shut down", MenuMsg::End(Ending::ShutDown), true),
        ]
    );
    assert_eq!(table[POWER].items.len(), rows.len());
}

#[test]
fn the_power_menu_hangs_right_aligned_on_the_screen() {
    let f = font();
    for theme in themes() {
        let m = FontMetrics::new(&f, theme.font_px);
        for width in [1024u32, 1360, 2560] {
            let bar = top_bar("", None, None, &theme);
            let l = layout(&bar, Rect::new(0, 0, width, BAR_H), &m);
            let word = locate(&bar, &l, POWER_KEY).expect("the power button");
            let table = menus(&staged(), "", &libfs::places("/home"), "/home", &theme);
            let view = menu_view(POWER, &table, &MenuState::new(3), None, &TextFieldState::new(), &theme);
            let size = libui::layout::measure(&view, libui::layout::Constraints::loose(Size::new(4000, 4000)), &m);
            let (x, y) = menu_anchor_right(word, size.w);
            assert!(x >= 0, "off the left at {width}");
            assert_eq!(x + size.w as i32, width as i32 - MENU_INSET_X, "right edge at {width}");
            assert_eq!(y, BAR_H as i32 + MENU_DROP);
            assert!(menu_anchor(word).0 + size.w as i32 > width as i32, "the left anchor fits after all");
        }
    }
}

/// The waiting dialog's buttons land where `libui` publishes (`check-logout` aims at them).
#[test]
fn the_waiting_dialogs_buttons_land_where_libui_says() {
    use libui::widget::{DIALOG_BUTTON_CY, DIALOG_H, DIALOG_LEFT_CX, DIALOG_RIGHT_CX, DIALOG_W};
    let f = font();
    let theme = Theme::light();
    let m = FontMetrics::new(&f, theme.font_px);
    let q = (String::from("Waiting for 1 window to close:"), String::from("notes.txt"));
    let view = ending_dialog(Ending::LogOut, &q, None, &theme);
    let size = libui::layout::measure(&view, libui::layout::Constraints::loose(Size::new(4000, 4000)), &m);
    assert_eq!((size.w, size.h), (DIALOG_W, DIALOG_H));
    let bounds = Rect::new(0, 0, size.w, size.h);
    assert_eq!(click(&view, bounds, &m, DIALOG_LEFT_CX, DIALOG_BUTTON_CY), [EndingMsg::EndAnyway]);
    assert_eq!(click(&view, bounds, &m, DIALOG_RIGHT_CX, DIALOG_BUTTON_CY), [EndingMsg::Cancel]);
    let l = layout(&view, bounds, &m);
    let text_rect = locate(&view, &l, ENDING_TEXT_KEY).expect("the question");
    assert!(text_rect.size.h > 0 && text_rect.size.w > 0);
    let close = locate(&view, &l, libui::widget::TITLE_CLOSE_KEY).expect("the close button");
    let (cx, cy) = (close.origin.x + close.size.w as i32 / 2, close.origin.y + close.size.h as i32 / 2);
    assert_eq!(click(&view, bounds, &m, cx, cy), [EndingMsg::Cancel], "closing the dialog is Cancel");
}

/// The refusal dialog's Close lands where the waiting dialog's Cancel does.
#[test]
fn the_refusal_dialogs_close_lands_where_libui_says() {
    use libui::widget::{DIALOG_BUTTON_CY, DIALOG_H, DIALOG_LEFT_CX, DIALOG_RIGHT_CX, DIALOG_W};
    let f = font();
    let theme = Theme::light();
    let m = FontMetrics::new(&f, theme.font_px);
    let view = refusal_dialog(Ending::ShutDown, &Refusal::Password, None, &theme);
    let size = libui::layout::measure(&view, libui::layout::Constraints::loose(Size::new(4000, 4000)), &m);
    assert_eq!((size.w, size.h), (DIALOG_W, DIALOG_H));
    let bounds = Rect::new(0, 0, size.w, size.h);
    assert_eq!(click(&view, bounds, &m, DIALOG_RIGHT_CX, DIALOG_BUTTON_CY), [RefusalMsg::Close]);
    assert!(click(&view, bounds, &m, DIALOG_LEFT_CX, DIALOG_BUTTON_CY).is_empty(), "the empty half");
    let l = layout(&view, bounds, &m);
    let close = locate(&view, &l, libui::widget::TITLE_CLOSE_KEY).expect("the close button");
    let (cx, cy) = (close.origin.x + close.size.w as i32 / 2, close.origin.y + close.size.h as i32 / 2);
    assert_eq!(click(&view, bounds, &m, cx, cy), [RefusalMsg::Close]);
}

/// Every refusal the shell can say fits the dialog's width, or it is cut off mid-reason.
#[test]
fn every_refusal_the_shell_says_fits_the_dialog() {
    use crate::ending::failed;
    let f = font();
    let refusals = [
        Refusal::Password,
        Refusal::Refused(String::from("no rule lets alice use `power`")),
        Refusal::Refused(String::from("`power` does not let alice run `shutdown`")),
        Refusal::Failed(String::from(failed::NO_BROKER)),
        Refusal::Failed(String::from(failed::NO_NAMESPACE)),
        Refusal::Failed(String::from(failed::BROKER_GONE)),
        Refusal::Failed(String::from(libviews::Failed::NoAnswer.why())),
        Refusal::Failed(String::from(libviews::Failed::Refused.why())),
        Refusal::Failed(String::from(libviews::Failed::Garbled.why())),
        Refusal::exited(-2147483648, false),
        Refusal::exited(1, true),
    ];
    for theme in themes() {
        let m = FontMetrics::new(&f, theme.font_px);
        for ending in [Ending::Restart, Ending::ShutDown] {
            for r in &refusals {
                let view = refusal_dialog(ending, r, None, &theme);
                let bounds = Rect::new(0, 0, libui::widget::DIALOG_W, libui::widget::DIALOG_H);
                let l = layout(&view, bounds, &m);
                let room = locate(&view, &l, REFUSAL_TEXT_KEY).expect("the lines").size.w - 2 * DIALOG_PAD;
                let (a, b) = r.lines(ending);
                for line in [a, b] {
                    let e: Element<RefusalMsg> = text(line.clone());
                    let w = libui::layout::measure(&e, Constraints::loose(Size::new(4000, 4000)), &m).w;
                    assert!(w <= room, "{line:?} is {w} wide in {room} at {} px", theme.font_px);
                }
            }
        }
    }
}

#[test]
fn the_bar_diffs_as_its_words_light() {
    let (f, theme) = (font(), Theme::light());
    let m = FontMetrics::new(&f, theme.font_px);
    let bounds = Rect::new(0, 0, 1360, BAR_H);
    let mut tree = Tree::new();
    for (open, hovered) in
        [(None, None), (None, Some(APPS_KEY)), (Some(APPS), None), (Some(PLACES), Some(PLACES_KEY)), (None, None)]
    {
        let bar = top_bar("12:34", open, hovered, &theme);
        let l = layout(&bar, bounds, &m);
        assert!(tree.update(&bar, &l).is_ok(), "the bar stopped diffing at {open:?}/{hovered:?}");
    }
}

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
    assert!(matches!(apps_menu(&apps, "nxedit").items[..], [Item::Action { msg: MenuMsg::Launch(2), .. }]));
}

#[test]
fn an_empty_menu_says_why() {
    let apps = staged();
    let none = apps_menu(&apps, "zzz");
    assert!(matches!(&none.items[..], [Item::Action { label, enabled: false, .. }] if label == "Nothing matches"));
    let empty = apps_menu(&[], "");
    assert!(matches!(&empty.items[..], [Item::Action { label, enabled: false, .. }] if label == "No applications"));
    let mut s = MenuState::new(3);
    s.toggle(APPS);
    s.select_first(&[none, apps_menu(&apps, "")]);
    assert_eq!(s.cursor(), None);
}

#[test]
fn the_places_menu_is_libfs_places_with_their_paths_beside_them() {
    let theme = Theme::light();
    let places = libfs::places("/home");
    let menu = places_menu(&places, "/home", &theme);
    let rows: Vec<(&str, Option<&str>, Option<libdraw::format::Rgb>)> = menu
        .items
        .iter()
        .filter_map(|it| match it {
            Item::Action { label, hint, swatch, .. } => Some((label.as_ref(), hint.as_deref(), *swatch)),
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
    assert_eq!(tilde("/homework", "/home"), "/homework");
    assert_eq!(tilde("/home", ""), "/home");
    assert_eq!(tilde("/Documents", "/"), "/Documents");
}

/// `check-login`'s row aims, as literals: the one row left after typing `nxterm` at `ROW1`,
/// and `Documents` at `PLACE_DOCUMENTS`, in screen coordinates.
#[test]
fn the_gates_row_aims_land_on_the_rows_they_name() {
    let f = font();
    for theme in themes() {
        let m = FontMetrics::new(&f, theme.font_px);
        let apps = staged();
        let places = libfs::places("/home");
        let bar = top_bar("", None, None, &theme);
        let bl = layout(&bar, Rect::new(0, 0, 1360, BAR_H), &m);

        let aim = |which: usize, query: &str, key: u64, (sx, sy): (i32, i32)| {
            let word = locate(&bar, &bl, if which == APPS { APPS_KEY } else { PLACES_KEY }).unwrap();
            let (ax, ay) = menu_anchor(word);
            let table = menus(&apps, query, &places, "/home", &theme);
            let mut q = TextFieldState::new();
            for c in query.chars() {
                q.insert(c);
            }
            let view = menu_view(which, &table, &MenuState::new(3), None, &q, &theme);
            let size = libui::layout::measure(&view, libui::layout::Constraints::loose(Size::new(4000, 4000)), &m);
            let msgs = click(&view, Rect::new(0, 0, size.w, size.h), &m, sx - ax, sy - ay);
            let want = table[which].items.iter().enumerate().find_map(|(i, it)| {
                (ROW_KEY_BASE + i as u64 == key).then_some(match it {
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

#[test]
fn the_name_prompt_has_a_size() {
    let (f, theme) = (font(), Theme::light());
    let m = FontMetrics::new(&f, theme.font_px);
    let e = name_prompt(&TextFieldState::new(), &theme);
    let s = libui::layout::measure(&e, libui::layout::Constraints::loose(Size::new(4000, 4000)), &m);
    assert_eq!(s.w, PROMPT_W);
    assert!(s.h > 20 && s.h < 200, "{s:?}");
}

fn tasks(n: usize) -> Vec<Task<'static>> {
    (0..n).map(|i| Task { id: 10 + i as u32, title: "nxterm", focused: i == 0, minimized: false }).collect()
}

#[test]
fn the_switcher_shows_three_cells_around_the_current_desktop() {
    assert_eq!(switcher_cells(0, 1), 0..1);
    assert_eq!(switcher_cells(1, 2), 0..2);
    assert_eq!(switcher_cells(0, 3), 0..3);
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

#[test]
#[cfg_attr(miri, ignore = "every width and label of every theme, in interpreted glyphs")]
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
                    let next_right = TASKS_X + n as u32 * TASK_PITCH + TASK_W;
                    let switcher_x = width - BAR_PAD_X - switcher_width(&d, &theme, &m);
                    assert!(next_right + BAR_GAP > switcher_x - BAR_GAP, "{width}/{label}: room for one more");
                }
            }
        }
    }
}

/// `check-login`'s bottom-bar aims at half the bar, as literals: show-desktop at x 20; the
/// first task's middle at 141 and the next at 331; the empty stretch at 600; the desktop's name
/// 30 in from the right; the forward arrow and first cell measured from the right edge.
#[test]
#[cfg_attr(miri, ignore = "every width and label of every theme, in interpreted glyphs")]
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
            let occupied = [true, false];
            let d = Desktops { occupied: &occupied, current: 0, label: "work" };
            let bar = bottom_bar(&tasks(1), false, &d, None, &theme);
            assert_eq!(click(&bar, bounds, &m, w - NEXT_FROM_RIGHT, y), [BottomMsg::Next], "{width}");
            let d = Desktops { occupied: &occupied, current: 1, label: "Desktop 2" };
            let bar = bottom_bar(&[], false, &d, None, &theme);
            assert_eq!(click(&bar, bounds, &m, w - FIRST_CELL_FROM_RIGHT, y), [BottomMsg::Desktop(0)], "{width}");
        }
    }
}

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
    // Found by key: the current-desktop cell draws in the accent too.
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
    assert!(
        lit.iter().filter(|c| **c == theme.accent).count() > quiet.iter().filter(|c| **c == theme.accent).count(),
        "show-desktop is lit in the accent while it is holding windows"
    );
    assert!(lit.contains(&soft) && quiet.contains(&theme.face_hover));
}

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

/// An arrow the shipped face lacks draws as nothing, with nothing reported.
#[test]
fn the_arrows_exist_in_the_shipped_face() {
    let f = font();
    assert!(f.has_glyph('\u{2039}') && f.has_glyph('\u{203A}'));
}

/// The middle button closes a task, and only a task: no message can say which button made it, so
/// the bar's task carries one for each (`check-login` middle-clicks the bar).
#[test]
fn a_middle_click_closes_a_task_and_does_nothing_elsewhere() {
    let f = font();
    let theme = Theme::default();
    let m = FontMetrics::new(&f, theme.font_px);
    let (bounds, y) = (Rect::new(0, 0, 1280, BAR_H), BAR_H as i32 / 2);
    let occupied = [true, false];
    let d = Desktops { occupied: &occupied, current: 0, label: "work" };
    let bar = bottom_bar(&tasks(2), false, &d, None, &theme);
    let middle = |x: i32| {
        let l = layout(&bar, bounds, &m);
        let mut tree = Tree::new();
        tree.update(&bar, &l).expect("the tree diffs");
        let mut r = Router::new();
        let at = |buttons, flags| PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x112,
            buttons,
            flags,
            x,
            y,
            ..Default::default()
        };
        let mut out = r.pointer(&tree, &bar, &l, at(4, POINTER_PRESSED)).0;
        out.extend(r.pointer(&tree, &bar, &l, at(0, 0)).0);
        out
    };
    assert_eq!(middle(141), [BottomMsg::Close(10)]);
    assert_eq!(middle(331), [BottomMsg::Close(11)]);
    assert_eq!(click(&bar, bounds, &m, 141, y), [BottomMsg::Task(10)], "the left button still raises");
    assert_eq!(middle(20), [], "show-desktop has no middle click");
    assert_eq!(middle(1280 - 30), [], "nor has the overview");
}
