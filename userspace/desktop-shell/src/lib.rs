//! `desktop-shell`'s host-testable internals: only what is pure, since the shell itself is a
//! bare-target program built around syscalls. `no_std` for the bare build, `std` under test.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod ending;
pub mod panel;
pub mod snap;

/// One graphical application, from a desktop entry under `/applications`.
pub struct Application {
    /// What a person sees: "Files".
    pub name: alloc::string::String,
    /// What gets spawned: `nxfiles`, resolved through `/bin` like anything else.
    pub exec: alloc::string::String,
}

/// Parse a desktop entry: `name` and `exec`, both required, as `key = "value"` lines like
/// `Theme`'s reader (the system has no TOML crate).
pub fn parse_entry(text: &str) -> Option<Application> {
    let (mut name, mut exec) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        // Quoted only: `trim_matches('"')` would accept `"x`.
        let Some(v) = v.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else { continue };
        match k.trim() {
            "name" => name = Some(alloc::string::String::from(v)),
            "exec" => exec = Some(alloc::string::String::from(v)),
            _ => {}
        }
    }
    match (name, exec) {
        (Some(name), Some(exec)) if !name.is_empty() && !exec.is_empty() => Some(Application { name, exec }),
        _ => None,
    }
}

/// Whether `app` is shown for query `q`, by display name or program: a person types either.
pub fn matches_app(app: &Application, q: &str) -> bool {
    matches(&app.name, q) || matches(&app.exec, q)
}

/// Whether one string is shown for query `q`, case-insensitive on ASCII.
pub fn matches(name: &str, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    let (n, q) = (name.to_ascii_lowercase(), q.to_ascii_lowercase());
    n.contains(&q)
}

/// The screen the shell lays itself out on, read once from `/dev/draw/screen` at startup.
///
/// Every size derived from it is a method here, host-tested at every size; how many windows the
/// bottom bar holds is [`panel::task_capacity`] of a measured width instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Screen {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Each bar's height: the top bar, and the window list at the foot of the screen. 30, the
/// design's; gates that aim at a bar keep their own copy.
pub const BAR_H: u32 = 30;

/// How far each new window steps from the last.
pub const CASCADE_STEP: i32 = 24;

/// The `y` of a new window `height` high, and the next origin: `next` if it ends at or above
/// `work_bottom`, else the first place, `work_top + CASCADE_STEP`.
pub fn cascade(next: i32, height: u32, work_top: i32, work_bottom: i32) -> (i32, i32) {
    let fits = i64::from(next) + i64::from(height) <= i64::from(work_bottom);
    let y = if fits { next } else { work_top + CASCADE_STEP };
    (y, y + CASCADE_STEP)
}

/// How wide an overview card is, as a fraction of the screen: the design's 330 on 1440, so the
/// same three cards fit across at every size `check-resolutions` boots.
pub const CARD_W_NUM: u32 = 330;
/// The screen width [`CARD_W_NUM`] was measured against. See it.
pub const CARD_W_DEN: u32 = 1440;

/// A card's border: the design's 2, in `accent` for the current desktop.
pub const CARD_BORDER: u32 = 2;

/// Between two cards, across and down — the design's 24.
pub const CARD_GAP: u32 = 24;

/// Between a card and its caption — the design's 9.
pub const CARD_CAPTION_GAP: u32 = 9;

/// How tall the caption row is: the design's 12.5 px name and 11 px count, with room to sit in.
pub const CARD_CAPTION_H: u32 = 18;

/// The clear space either side of the block of cards — the design's 40.
pub const CARD_SIDE_PAD: u32 = 40;

impl Screen {
    /// Bytes per row of a buffer as wide as the screen — both bars, the wallpaper, the overview.
    pub const fn pitch(self) -> usize {
        self.width as usize * 4
    }

    /// Where the window list is placed: its top edge, one bar above the bottom of the screen.
    pub const fn window_list_y(self) -> i32 {
        self.height.saturating_sub(BAR_H) as i32
    }

    /// How wide one overview card is, border included — at least enough to have an interior.
    pub const fn card_w(self) -> u32 {
        let w = self.width * CARD_W_NUM / CARD_W_DEN;
        if w < CARD_BORDER * 2 + 2 { CARD_BORDER * 2 + 2 } else { w }
    }

    /// The interior of a card: what a whole screen is scaled into.
    pub const fn card_interior_w(self) -> u32 {
        self.card_w() - CARD_BORDER * 2
    }

    /// How tall a card's top strip is: the top bar at the card's scale.
    pub const fn card_strip_h(self) -> u32 {
        self.scaled_to_card(BAR_H)
    }

    /// The interior of a card, down: the strip, then the screen below the bar at the same scale.
    pub const fn card_interior_h(self) -> u32 {
        let below = self.scaled_to_card(self.height.saturating_sub(BAR_H));
        let h = self.card_strip_h() + below;
        if h == 0 { 1 } else { h }
    }

    /// A card's miniature box, border included. The caption sits below this.
    pub const fn card_h(self) -> u32 {
        self.card_interior_h() + CARD_BORDER * 2
    }

    /// A whole card: its box, the gap, and the caption beneath.
    pub const fn card_total_h(self) -> u32 {
        self.card_h() + CARD_CAPTION_GAP + CARD_CAPTION_H
    }

    /// `n` screen pixels at a card's scale: the one conversion.
    pub const fn scaled_to_card(self, n: u32) -> u32 {
        let w = if self.width == 0 { 1 } else { self.width };
        n * self.card_interior_w() / w
    }

    /// How many cards fit across: at least one, so a narrow screen gets a column.
    pub const fn card_cols(self) -> u32 {
        let avail = self.width.saturating_sub(CARD_SIDE_PAD * 2);
        let cols = (avail + CARD_GAP) / (self.card_w() + CARD_GAP);
        if cols == 0 { 1 } else { cols }
    }
}

/// How many cards sit in row `r`, when `n` are laid out `cols` to a row.
const fn cards_in_row(r: u32, n: u32, cols: u32) -> u32 {
    let left = n.saturating_sub(r * cols);
    if left < cols { left } else { cols }
}

/// Where card `i` of `n` sits: its miniature box, in overview-local pixels. One function for
/// drawing and hit-testing, centred both ways with each row on its own, between the bars the
/// overview's window is drawn under.
pub fn card_rect(i: usize, n: usize, screen: Screen) -> (i32, i32, u32, u32) {
    let (cols, cw) = (screen.card_cols(), screen.card_w());
    let n = n.max(1) as u32;
    let i = i as u32;
    let (row, col) = (i / cols, i % cols);
    let rows = n.div_ceil(cols);

    let in_row = cards_in_row(row, n, cols);
    let across = in_row * cw + in_row.saturating_sub(1) * CARD_GAP;
    let x = (screen.width.saturating_sub(across) / 2) + col * (cw + CARD_GAP);

    let total = screen.card_total_h();
    let down = rows * total + rows.saturating_sub(1) * CARD_GAP;
    let avail = screen.height.saturating_sub(BAR_H * 2);
    let y = BAR_H + avail.saturating_sub(down) / 2 + row * (total + CARD_GAP);

    (x as i32, y as i32, cw, screen.card_h())
}

/// Which card a point is in, if any, caption included.
pub fn card_at(x: i32, y: i32, n: usize, screen: Screen) -> Option<usize> {
    if x < 0 || y < 0 {
        return None;
    }
    (0..n).find(|&i| {
        let (cx, cy, cw, _) = card_rect(i, n, screen);
        let ch = screen.card_total_h();
        x >= cx && x < cx + cw as i32 && y >= cy && y < cy + ch as i32
    })
}

/// Where a window sits inside a card: the screen's geometry at the card's scale, kept within the
/// interior and at least two pixels each way. `card` is the box from [`card_rect`].
pub fn window_box(
    card: (i32, i32, u32, u32),
    origin: (i32, i32),
    size: (u32, u32),
    screen: Screen,
) -> (i32, i32, u32, u32) {
    let (iw, ih) = (screen.card_interior_w(), screen.card_interior_h());
    let strip = screen.card_strip_h();
    let sx = screen.scaled_to_card(origin.0.max(0) as u32).min(iw.saturating_sub(1));
    let below = (origin.1.max(BAR_H as i32) as u32).saturating_sub(BAR_H);
    let sy = (strip + screen.scaled_to_card(below)).min(ih.saturating_sub(1));
    let sw = screen.scaled_to_card(size.0).max(2).min(iw - sx);
    let sh = screen.scaled_to_card(size.1).max(2).min(ih - sy);
    (card.0 + CARD_BORDER as i32 + sx as i32, card.1 + CARD_BORDER as i32 + sy as i32, sw, sh)
}

/// How big a capture of a window `size` pixels on screen should be: its box's interior at the
/// card's scale, unclamped on purpose, as [`window_box`] clamps a window dragged off the screen
/// into a sliver that would smear a whole-window capture.
pub fn capture_box(size: (u32, u32), screen: Screen) -> (u32, u32) {
    let inner = |n: u32| screen.scaled_to_card(n).max(2).saturating_sub(2).max(1);
    (inner(size.0), inner(size.1))
}

/// One window, as show-desktop sees it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Window {
    /// The compositor's id.
    pub id: u32,
    /// The desktop it is on.
    pub desktop: u32,
    /// Whether it is put away.
    pub minimized: bool,
    /// Whether it holds the keyboard.
    pub focused: bool,
}

/// What show-desktop put away on one desktop, and what it could not reach: a press puts away only
/// the windows the bar has buttons for (`Super+H`'s bound), so some stay up past the bar's end,
/// and checking "is anything up again" would find those and drop the set before the button could
/// light.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShownDesktop {
    /// The desktop it is on.
    pub desktop: u32,
    /// What the press put away, in the order it comes back: the window that had the keyboard last,
    /// so it is on top and focused again.
    pub put_away: alloc::vec::Vec<u32>,
    /// What the press left up: no bar button for it, or putting it away was refused.
    pub left_up: alloc::vec::Vec<u32>,
}

impl ShownDesktop {
    /// What a press on `desktop` should put away, given every window in the bar's order and how
    /// many buttons it has; `None` when nothing it could reach is up. Only what is up is put
    /// away, so the pair undo each other.
    pub fn plan(windows: &[Window], desktop: u32, capacity: usize) -> Option<Self> {
        let (mut put_away, mut left_up, mut focused) = (alloc::vec::Vec::new(), alloc::vec::Vec::new(), None);
        for (i, w) in windows.iter().filter(|w| w.desktop == desktop).enumerate() {
            if w.minimized {
                continue;
            }
            if i >= capacity {
                left_up.push(w.id);
            } else if w.focused {
                focused = Some(w.id);
            } else {
                put_away.push(w.id);
            }
        }
        put_away.extend(focused);
        (!put_away.is_empty()).then_some(ShownDesktop { desktop, put_away, left_up })
    }

    /// Whether the desktop is still as the press left it: nothing on it is up but what the press
    /// could not reach. Anything else came back another way, so a second press would no longer be
    /// coherent; the button's light is this answer.
    pub fn holds(&self, windows: &[Window]) -> bool {
        windows.iter().all(|w| w.desktop != self.desktop || w.minimized || self.left_up.contains(&w.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, desktop: u32, minimized: bool, focused: bool) -> Window {
        Window { id, desktop, minimized, focused }
    }

    /// The y of each window placed in turn, `count` of them, `height` high.
    fn cascade_ys(height: u32, count: usize, work_top: i32, work_bottom: i32) -> alloc::vec::Vec<i32> {
        let mut next = work_top + CASCADE_STEP;
        (0..count)
            .map(|_| {
                let (y, n) = cascade(next, height, work_top, work_bottom);
                next = n;
                y
            })
            .collect()
    }

    #[test]
    fn cascade_gives_a_505_high_window_eight_places() {
        let ys = cascade_ys(505, 10, 30, 738);
        assert_eq!(ys, [54, 78, 102, 126, 150, 174, 198, 222, 54, 78]);
    }

    #[test]
    fn cascade_gives_a_585_high_window_five_places() {
        let ys = cascade_ys(585, 7, 30, 738);
        assert_eq!(ys, [54, 78, 102, 126, 150, 54, 78]);
    }

    #[test]
    fn a_window_ending_at_the_bottom_fits_and_one_pixel_past_wraps() {
        assert_eq!(cascade(233, 505, 30, 738), (233, 257), "ends exactly at the bottom");
        assert_eq!(cascade(234, 505, 30, 738), (54, 78), "one pixel past the bottom");
    }

    #[test]
    fn cascade_puts_a_window_taller_than_the_work_area_at_the_first_place() {
        assert_eq!(cascade(54, 709, 30, 738), (54, 78));
        assert_eq!(cascade(78, 709, 30, 738), (54, 78));
        assert_eq!(cascade(54, u32::MAX, 30, 738), (54, 78));
    }

    #[test]
    fn show_desktop_holds_with_more_windows_than_buttons() {
        let before: alloc::vec::Vec<Window> = (1..=6).map(|i| win(i, 1, false, i == 3)).collect();
        let s = ShownDesktop::plan(&before, 1, 5).expect("five things to put away");
        assert_eq!(s.put_away, [1, 2, 4, 5, 3], "the focused window comes back last");
        assert_eq!(s.left_up, [6]);
        let after: alloc::vec::Vec<Window> =
            before.iter().map(|w| Window { minimized: w.id != 6, focused: false, ..*w }).collect();
        assert!(s.holds(&after), "the window past the bar's end is not something coming back");
    }

    #[test]
    fn show_desktop_lets_go_when_something_comes_back_another_way() {
        let s = ShownDesktop { desktop: 1, put_away: alloc::vec![1, 2], left_up: alloc::vec![3] };
        let quiet = [win(1, 1, true, false), win(2, 1, true, false), win(3, 1, false, false)];
        assert!(s.holds(&quiet));
        let restored = [win(1, 1, false, true), win(2, 1, true, false), win(3, 1, false, false)];
        assert!(!s.holds(&restored), "a window restored from its button");
        let mut arrived = quiet.to_vec();
        arrived.push(win(9, 1, false, true));
        assert!(!s.holds(&arrived), "a new window, or one moved here");
        let mut elsewhere = quiet.to_vec();
        elsewhere.push(win(9, 2, false, true));
        assert!(s.holds(&elsewhere), "another desktop's windows are not this press's business");
    }

    #[test]
    fn show_desktop_brings_back_exactly_what_it_put_away() {
        let before = [win(1, 1, true, false), win(2, 1, false, true), win(3, 2, false, false)];
        let s = ShownDesktop::plan(&before, 1, 5).unwrap();
        assert_eq!(s.put_away, [2], "not the one already minimised, and not another desktop's");
        assert!(ShownDesktop::plan(&[win(1, 1, true, false)], 1, 5).is_none());
        assert!(ShownDesktop::plan(&[win(1, 1, true, false), win(2, 1, false, false)], 1, 1).is_none());
    }

    const SIZES: [(u32, u32); 9] = [
        (0, 0),
        (160, 100),
        (339, 200),
        (1024, 768),
        (1280, 800),
        (1360, 768),
        (1366, 768),
        (1920, 1080),
        (2560, 1440),
    ];

    #[test]
    fn the_window_list_sits_one_bar_above_the_foot_at_every_size() {
        for (width, height) in SIZES {
            let s = Screen { width, height };
            assert_eq!(s.window_list_y() as u32, height.saturating_sub(BAR_H), "{width}x{height}");
            assert!(s.card_cols() >= 1, "{width}x{height}");
        }
    }

    #[test]
    fn the_layout_at_the_old_size_and_at_the_gate_size() {
        let old = Screen { width: 1280, height: 800 };
        assert_eq!((old.window_list_y(), old.card_cols(), old.pitch()), (770, 3, 5120));
        let gate = Screen { width: 1360, height: 768 };
        assert_eq!((gate.window_list_y(), gate.card_cols(), gate.pitch()), (738, 3, 5440));
    }

    #[test]
    fn a_screen_too_small_for_the_chrome_degrades_rather_than_wrapping() {
        let tiny = Screen { width: 100, height: 10 };
        assert_eq!(tiny.window_list_y(), 0, "saturating, not a negative origin from a wrap");
        assert_eq!(tiny.card_cols(), 1);
        assert!(tiny.card_interior_w() >= 1 && tiny.card_interior_h() >= 1);
    }

    #[test]
    fn a_card_is_the_screen_at_one_scale() {
        // The interior is the bar scaled plus what is under it, at one ratio.
        for (width, height) in SIZES {
            let s = Screen { width, height };
            let below = height.saturating_sub(BAR_H);
            assert_eq!(
                s.card_interior_h(),
                s.scaled_to_card(BAR_H) + s.scaled_to_card(below),
                "{width}x{height}: the interior is not the bar plus what is under it"
            );
            assert_eq!(s.card_w(), (width * CARD_W_NUM / CARD_W_DEN).max(CARD_BORDER * 2 + 2));
        }
    }

    #[test]
    fn the_design_card_comes_out_where_the_page_draws_it() {
        // The page's own screen: within two pixels, as it rounds an 8-pixel strip by hand.
        let page = Screen { width: 1440, height: 900 };
        assert_eq!(page.card_w(), 330);
        assert!(page.card_h().abs_diff(208) <= 2, "the page's card box is 208; ours is {}", page.card_h());
        assert_eq!(page.card_cols(), 3, "the page fits three across");
    }

    #[test]
    fn cards_are_centred_and_the_last_row_is_centred_on_its_own() {
        let s = Screen { width: 1360, height: 768 };
        let cols = s.card_cols() as usize;
        assert!(cols >= 2, "this test needs a screen that fits a row");

        let (x, _, w, _) = card_rect(0, 1, s);
        assert!((x as u32 + w / 2).abs_diff(s.width / 2) <= 1, "a lone card is not centred");

        let first = card_rect(0, cols, s);
        let last = card_rect(cols - 1, cols, s);
        let block_mid = first.0 as u32 + (last.0 as u32 + last.2 - first.0 as u32) / 2;
        assert!(block_mid.abs_diff(s.width / 2) <= 1, "a full row is not centred: {block_mid}");
        assert_eq!(card_rect(1, cols, s).0 - first.0, (s.card_w() + CARD_GAP) as i32);

        let two = cols + 1;
        let top = card_rect(0, two, s);
        let orphan = card_rect(cols, two, s);
        assert!((orphan.0 as u32 + orphan.2 / 2).abs_diff(s.width / 2) <= 1, "the last row is not centred");
        assert!(orphan.1 > top.1, "the second row is not below the first");
        assert_eq!(orphan.1 - top.1, (s.card_total_h() + CARD_GAP) as i32);
    }

    #[test]
    fn the_cards_stay_between_the_bars() {
        for (width, height) in SIZES {
            let s = Screen { width, height };
            if height < BAR_H * 2 + s.card_total_h() {
                continue; // no room for even one row; the degenerate case is its own test
            }
            for n in [1usize, s.card_cols() as usize + 1] {
                for i in 0..n {
                    let (_, y, _, _) = card_rect(i, n, s);
                    assert!(y >= BAR_H as i32, "{width}x{height}/{n}: card {i} is under the top bar");
                }
                let (_, y, _, _) = card_rect(n - 1, n, s);
                let foot = y as u32 + s.card_total_h();
                if n == 1 {
                    assert!(foot <= height - BAR_H, "{width}x{height}: a card runs into the window list at {foot}");
                }
            }
        }
    }

    #[test]
    fn a_click_finds_the_card_it_is_in_and_nothing_between_them() {
        let s = Screen { width: 1360, height: 768 };
        let n = 3;
        for i in 0..n {
            let (x, y, w, _) = card_rect(i, n, s);
            assert_eq!(card_at(x + w as i32 / 2, y + 4, n, s), Some(i), "the box of card {i}");
            let caption = y + (s.card_h() + CARD_CAPTION_GAP + CARD_CAPTION_H / 2) as i32;
            assert_eq!(card_at(x + 4, caption, n, s), Some(i), "the caption of card {i}");
        }
        let (x0, y0, w0, _) = card_rect(0, n, s);
        assert_eq!(card_at(x0 + w0 as i32 + CARD_GAP as i32 / 2, y0 + 4, n, s), None);
        assert_eq!(card_at(-1, y0, n, s), None, "a negative x is off the overview, not card 0");
        assert_eq!(card_at(x0, BAR_H as i32, n, s), None, "the top bar is not a card");
    }

    #[test]
    fn a_capture_is_the_windows_own_scale_however_far_off_screen_it_is() {
        let s = Screen { width: 1360, height: 768 };
        let size = (900, 600);
        let want = capture_box(size, s);
        assert!(want.0 > 100 && want.1 > 60, "a big window scales to a big capture: {want:?}");

        let card = card_rect(0, 1, s);
        let on = window_box(card, (10, BAR_H as i32), size, s);
        let off = window_box(card, (s.width as i32 - 20, BAR_H as i32), size, s);
        assert!(off.2 < on.2, "precondition: the off-screen window's box is clamped narrower");
        assert_eq!(capture_box(size, s), want);
        assert!(
            want.0 > off.2,
            "the capture {want:?} fits inside the clamped box {off:?}, so nothing is cropped \
             and the squash this guards against cannot be told apart"
        );
    }

    #[test]
    fn a_window_box_is_where_the_window_is_on_the_screen() {
        let s = Screen { width: 1360, height: 768 };
        let card = card_rect(0, 1, s);
        let interior = (card.0 + CARD_BORDER as i32, card.1 + CARD_BORDER as i32);

        let full = window_box(card, (0, BAR_H as i32), (s.width, s.height - BAR_H), s);
        assert_eq!((full.0, full.1), (interior.0, interior.1 + s.card_strip_h() as i32));
        assert_eq!(full.2, s.card_interior_w());
        assert_eq!(full.1 as u32 + full.3, interior.1 as u32 + s.card_interior_h());

        let top = window_box(card, (0, 0), (100, 100), s);
        assert_eq!(top.1, interior.1 + s.card_strip_h() as i32);

        let mid = window_box(card, (s.width as i32 / 2, BAR_H as i32), (100, 100), s);
        assert!((mid.0 - interior.0).abs_diff(s.card_interior_w() as i32 / 2) <= 1);

        let off = window_box(card, (s.width as i32 - 20, s.height as i32 - 20), (900, 600), s);
        assert!(off.0 as u32 + off.2 <= interior.0 as u32 + s.card_interior_w(), "{off:?}");
        assert!(off.1 as u32 + off.3 <= interior.1 as u32 + s.card_interior_h(), "{off:?}");

        let tiny = window_box(card, (0, BAR_H as i32), (1, 1), s);
        assert!(tiny.2 >= 2 && tiny.3 >= 2, "a small window vanished: {tiny:?}");
    }

    fn app(name: &str, exec: &str) -> Application {
        Application { name: alloc::string::String::from(name), exec: alloc::string::String::from(exec) }
    }

    #[test]
    fn an_entry_needs_both_keys_and_both_quoted() {
        let a = parse_entry("name = \"Files\"\nexec = \"nxfiles\"\n").expect("valid");
        assert_eq!((a.name.as_str(), a.exec.as_str()), ("Files", "nxfiles"));

        let a = parse_entry("# an entry\r\n\r\n  name  =  \"Text Editor\"  \r\nexec=\"nxedit\"\r\n")
            .expect("valid despite CRLF and spacing");
        assert_eq!((a.name.as_str(), a.exec.as_str()), ("Text Editor", "nxedit"));

        let padded = alloc::format!("name = \"Files\"\nexec = \"nxfiles\"\n{}", "\0".repeat(64));
        assert!(parse_entry(&padded).is_some(), "a page-padded entry must still parse");
    }

    #[test]
    fn a_malformed_entry_is_refused_rather_than_half_read() {
        // Each of these would otherwise become a menu row that launches nothing.
        for bad in [
            "name = \"Files\"\n",                   // no exec
            "exec = \"nxfiles\"\n",                 // no name
            "name = Files\nexec = nxfiles\n",       // unquoted
            "name = \"Files\nexec = \"nxfiles\"\n", // one quote — the trap `theme.rs` records
            "name = \"\"\nexec = \"nxfiles\"\n",    // empty name
            "name = \"Files\"\nexec = \"\"\n",      // empty exec
            "",
        ] {
            assert!(parse_entry(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[test]
    fn the_filter_matches_the_program_as_well_as_the_name() {
        let a = app("Text Editor", "nxedit");
        assert!(matches_app(&a, ""), "an empty query shows everything");
        assert!(matches_app(&a, "editor"), "somebody who knows the desktop");
        assert!(matches_app(&a, "nxedit"), "somebody who knows the system");
        assert!(matches_app(&a, "EDIT"), "case-insensitive: display names are capitalised");
        assert!(matches_app(&a, "Text Ed"), "a substring spanning the space");
        assert!(!matches_app(&a, "nxterm"));
    }
}
