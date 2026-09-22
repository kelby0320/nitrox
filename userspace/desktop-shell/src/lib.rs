//! `desktop-shell`'s host-testable internals.
//!
//! **A library beside the binary, holding only what is pure** — the shell itself is a bare-target
//! program built around syscalls and cannot be host-tested at all, which is why the desktop-entry
//! parser and the modal's filter went untested when they were written (PR #279 review, finding 7).
//! `nxterm`, `nxfiles`, `nxedit`, `service-mgr` and `init` all grew a library for this reason;
//! this is the same move, kept to the functions that need no world.
//!
//! `#![no_std]` for the bare build; `std` under `cargo test` so the host harness works
//! (`cargo xtask test` runs `cargo test -p desktop-shell --lib`).

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod panel;

/// One graphical application, from a desktop entry under `/applications`.
///
/// **The display name and the program are different strings, and that is the point** (M14 Part
/// H). The launcher showed `/bin` — every service, server and CLI tool on the system, under the
/// name of its binary. The Applications menu shows what a package *declares* is an application,
/// under the name that package gives it.
pub struct Application {
    /// What a person sees: "Files".
    pub name: alloc::string::String,
    /// What gets spawned: `nxfiles`, resolved through `/bin` like anything else.
    pub exec: alloc::string::String,
}

/// Parse a desktop entry: `name` and `exec`, both required.
///
/// The same shape `Theme`'s reader uses — `key = "value"` a line at a time, `#` a comment —
/// rather than a TOML library, because this is two keys and the system has no TOML crate.
/// **Both required**: an entry with no `exec` names nothing to launch, and one with no `name`
/// would fall back to the binary's, which is the thing this part exists to stop showing.
pub fn parse_entry(text: &str) -> Option<Application> {
    let (mut name, mut exec) = (None, None);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        let v = v.trim();
        // A quoted value, and only a quoted value — `trim_matches('"')` would accept `"x` and
        // `x"`, which is the trap `theme.rs` records having fallen into.
        let Some(v) = v.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else { continue };
        match k.trim() {
            "name" => name = Some(alloc::string::String::from(v)),
            "exec" => exec = Some(alloc::string::String::from(v)),
            _ => {}
        }
    }
    match (name, exec) {
        (Some(name), Some(exec)) if !name.is_empty() && !exec.is_empty() => {
            Some(Application { name, exec })
        }
        _ => None,
    }
}

/// Whether `app` is shown for query `q` — matched against **both** the display name and the
/// program.
///
/// **Both, because both are things a person types.** Somebody who knows the desktop types
/// "editor"; somebody who knows the system types `nxedit`. Matching only the name would make the
/// second fail, and this system's users are more likely than most to be the second kind.
pub fn matches_app(app: &Application, q: &str) -> bool {
    matches(&app.name, q) || matches(&app.exec, q)
}

/// Whether one string is shown for query `q`. Case-insensitive on ASCII, because a display name
/// is capitalised ("Files") and nobody types the capital.
pub fn matches(name: &str, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    let (n, q) = (name.to_ascii_lowercase(), q.to_ascii_lowercase());
    n.contains(&q)
}

/// The screen the shell lays itself out on, read once from `/dev/draw/screen` at startup.
///
/// **Asked, not written down** (Phase 5 Part E). Until then the shell had `SCREEN_W = 1280` and
/// `SCREEN_H = 800`, and on the laptop's 1366×768 its window list was placed at `y = 776` — below
/// the last row. Every size the shell derives from the screen is a method here, so the arithmetic
/// is host-tested at every size rather than checked by a `const` assert at one.
///
/// **Except how many windows the bottom bar holds**, since the desktop refresh's Part C: that
/// depends on the switcher beside them, which carries the desktop's name, so it is
/// [`panel::task_capacity`] of a measured width rather than a method of the screen alone.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Screen {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Each bar's height: the top bar, and the window list at the foot of the screen.
///
/// **30, the design's**, since the desktop refresh's Part C; it was 24. Six pixels a bar is what
/// the design's controls need: a 22-pixel task button and a 26-by-22 show-desktop button with
/// room above and below, where 24 would leave them one pixel from each edge. Every gate that
/// aims at a bar keeps its own copy, and moved with this.
pub const BAR_H: u32 = 30;

/// How wide an overview card is, as a fraction of the screen: the design's 330 on 1440.
///
/// **A fraction rather than a number** (desktop refresh, Part E). The page composes at 1440×900
/// and 330 pixels there is 22.9% of the width; written down as 330 it would be a quarter of a
/// 1360-wide screen and an eighth of a 2560-wide one, and `cargo xtask check-resolutions` boots
/// five. As a fraction the same three cards fit across at every size, which is what the design's
/// composition is actually saying.
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

    /// How tall a card's **top strip** is — the top bar, at the card's own scale.
    ///
    /// The design writes 8 where the scale gives 6.8, which is the one place its card is not
    /// simply the screen divided. Derived here instead, because a strip that is the bar scaled
    /// stays the bar scaled at five resolutions and a written 8 does not.
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

    /// `n` screen pixels at a card's scale — the one conversion, so a window's box and the card's
    /// own strip cannot be scaled by two slightly different ratios.
    pub const fn scaled_to_card(self, n: u32) -> u32 {
        let w = if self.width == 0 { 1 } else { self.width };
        n * self.card_interior_w() / w
    }

    /// How many cards fit across — at least one, so a screen too narrow for the design's
    /// margins lays cards out in a column rather than dividing by zero.
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

/// Where card `i` of `n` sits: its miniature box, in overview-local pixels.
///
/// **One function for drawing and for hit-testing**, which is the lesson the bottom bar's
/// indicator taught: a hit region computed separately from the layout is right at one count and
/// wrong everywhere else (PR #243 review, blocking 2).
///
/// **Centred both ways, and each row centred on its own** — the design's cards are a wrapping
/// flex with `justify-content:center`, so a final row of one sits under the middle of a full row
/// rather than under its left-hand end.
///
/// **Between the bars, not over them.** The overview's window is the whole screen, but the two
/// bars are sticky and drawn above it, so the block is centred in what is left between them.
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

/// Which card a point is in, if any — the whole card, its caption included.
///
/// The caption is part of the target because it is part of the thing: a click just under a card,
/// on the words naming it, is a click on that desktop and not on the background that dismisses.
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

/// Where a window sits inside a card: the screen's geometry at the card's scale.
///
/// `card` is that card's miniature box as [`card_rect`] gives it. The result is clamped into the
/// card's interior, so a window dragged half off the screen draws inside the box that stands for
/// the screen rather than over the card next to it.
///
/// **The strip is the top bar**, so a window at the very top of the screen starts just below the
/// strip rather than under it — the same relationship it has to the real bar.
///
/// **At least two pixels each way**, or the border and the face have nowhere to go and a window
/// vanishes rather than being small.
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

/// What show-desktop put away on one desktop, and what it could not reach (desktop refresh,
/// Part C).
///
/// **Both halves are kept**, and the second is what the review of PR #314 found missing. A press
/// puts away only the windows the bar has buttons for — `Super+H`'s bound, since a window past
/// the bar's end minimised and then abandoned would have no way back — so on a desktop with more
/// windows than buttons, some stay up. A check of "is anything on this desktop up again" then
/// found those, in the same pass as the press, and dropped the set before the button could light:
/// the second press brought nothing back.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ShownDesktop {
    /// The desktop it is on.
    pub desktop: u32,
    /// What the press put away, in the order it comes back: the window that had the keyboard last,
    /// so it is on top and focused again.
    pub put_away: alloc::vec::Vec<u32>,
    /// What the press left up because the bar has no button for it — or because putting it away
    /// was refused.
    pub left_up: alloc::vec::Vec<u32>,
}

impl ShownDesktop {
    /// What a press on `desktop` should put away, given every window in the bar's order and how
    /// many buttons the bar has. `None` when nothing it could reach is up — a press with nothing
    /// to bring back leaves the button unlit.
    ///
    /// **Only what is up is put away**, so a window already minimised before the press is not in
    /// the set and stays put away after the second — which is what makes the pair undo each other.
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
    /// could not reach.
    ///
    /// **Anything else up means something came back another way** — a window restored from its
    /// button, a new one, one moved here — and then "bring back what the first press put away" is
    /// no longer a coherent request. The button's light is this answer.
    pub fn holds(&self, windows: &[Window]) -> bool {
        windows
            .iter()
            .all(|w| w.desktop != self.desktop || w.minimized || self.left_up.contains(&w.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn win(id: u32, desktop: u32, minimized: bool, focused: bool) -> Window {
        Window { id, desktop, minimized, focused }
    }

    /// Six windows and five buttons — the review's scenario at 1360×768 — and the press still
    /// holds its set, because the sixth is the one it could not reach.
    #[test]
    fn show_desktop_holds_with_more_windows_than_buttons() {
        let before: alloc::vec::Vec<Window> = (1..=6).map(|i| win(i, 1, false, i == 3)).collect();
        let s = ShownDesktop::plan(&before, 1, 5).expect("five things to put away");
        assert_eq!(s.put_away, [1, 2, 4, 5, 3], "the focused window comes back last");
        assert_eq!(s.left_up, [6]);
        // After the press: the five are down, the sixth is not.
        let after: alloc::vec::Vec<Window> =
            before.iter().map(|w| Window { minimized: w.id != 6, focused: false, ..*w }).collect();
        assert!(s.holds(&after), "the window past the bar's end is not something coming back");
    }

    /// Anything on the desktop that is up and was not left up lets the set go — and nothing on
    /// another desktop does.
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

    /// A window already put away before the press is not in the set, so the second press does not
    /// bring it back — the pair undo each other. And a press with nothing up plans nothing.
    #[test]
    fn show_desktop_brings_back_exactly_what_it_put_away() {
        let before = [win(1, 1, true, false), win(2, 1, false, true), win(3, 2, false, false)];
        let s = ShownDesktop::plan(&before, 1, 5).unwrap();
        assert_eq!(s.put_away, [2], "not the one already minimised, and not another desktop's");
        assert!(ShownDesktop::plan(&[win(1, 1, true, false)], 1, 5).is_none());
        // A window past the bar's end is never put away, even when it is the only one up.
        assert!(ShownDesktop::plan(&[win(1, 1, true, false), win(2, 1, false, false)], 1, 1).is_none());
    }

    /// Sizes the shell has been or will be run at, and the edges of the arithmetic.
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
        // And the card still has an interior to draw into rather than a zero or a wrap.
        assert!(tiny.card_interior_w() >= 1 && tiny.card_interior_h() >= 1);
    }

    // ---- the overview's cards (desktop refresh, Part E) --------------------

    #[test]
    fn a_card_is_the_screen_at_one_scale() {
        // **The card is a model of the screen**, so its interior must be the screen's shape: the
        // strip is the top bar scaled and the rest is what is under the bar, at the same ratio.
        // A card that scaled x and y independently would show every window the wrong shape, and
        // the drag would still work — so nothing else here would notice.
        for (width, height) in SIZES {
            let s = Screen { width, height };
            let below = height.saturating_sub(BAR_H);
            assert_eq!(
                s.card_interior_h(),
                s.scaled_to_card(BAR_H) + s.scaled_to_card(below),
                "{width}x{height}: the interior is not the bar plus what is under it"
            );
            // The design's card is 22.9% of the screen's width; ours is within a pixel of that
            // at every size, which is the claim the fraction is making.
            assert_eq!(s.card_w(), (width * CARD_W_NUM / CARD_W_DEN).max(CARD_BORDER * 2 + 2));
        }
    }

    #[test]
    fn the_design_card_comes_out_where_the_page_draws_it() {
        // The page's own screen, so the numbers can be set beside it: a 330-wide card with a
        // 208-tall box. **Within two pixels, not equal** — the page writes an 8-pixel strip where
        // the scale gives 6, which is the one number it rounds by hand. A north star, not an
        // overlay.
        let page = Screen { width: 1440, height: 900 };
        assert_eq!(page.card_w(), 330);
        assert!(
            page.card_h().abs_diff(208) <= 2,
            "the page's card box is 208; ours is {}",
            page.card_h()
        );
        assert_eq!(page.card_cols(), 3, "the page fits three across");
    }

    #[test]
    fn cards_are_centred_and_the_last_row_is_centred_on_its_own() {
        let s = Screen { width: 1360, height: 768 };
        let cols = s.card_cols() as usize;
        assert!(cols >= 2, "this test needs a screen that fits a row");

        // One card sits in the middle of the screen, not at the left margin.
        // **Within a pixel**: the block is centred by integer division and a card an odd number
        // of pixels wide cannot sit exactly on the middle column.
        let (x, _, w, _) = card_rect(0, 1, s);
        assert!((x as u32 + w / 2).abs_diff(s.width / 2) <= 1, "a lone card is not centred");

        // A full row is centred as a block, and its cards are a gap apart.
        let first = card_rect(0, cols, s);
        let last = card_rect(cols - 1, cols, s);
        let block_mid = first.0 as u32 + (last.0 as u32 + last.2 - first.0 as u32) / 2;
        assert!(block_mid.abs_diff(s.width / 2) <= 1, "a full row is not centred: {block_mid}");
        assert_eq!(card_rect(1, cols, s).0 - first.0, (s.card_w() + CARD_GAP) as i32);

        // **And the row below it is centred on its own.** With `cols + 1` desktops the last row
        // holds one card, which belongs under the middle of the screen — a version that laid
        // every row out from the same left edge passes everything above.
        // **Both rows read out of the *same* layout.** A block of two rows is taller than one
        // of a single row and is centred as a whole, so it starts higher up — comparing a card
        // from a one-row call against one from a two-row call measures that difference and not
        // the row pitch, which is what the first version of this did.
        let two = cols + 1;
        let top = card_rect(0, two, s);
        let orphan = card_rect(cols, two, s);
        assert!(
            (orphan.0 as u32 + orphan.2 / 2).abs_diff(s.width / 2) <= 1,
            "the last row is not centred"
        );
        assert!(orphan.1 > top.1, "the second row is not below the first");
        assert_eq!(orphan.1 - top.1, (s.card_total_h() + CARD_GAP) as i32);
    }

    #[test]
    fn the_cards_stay_between_the_bars() {
        // The bars are sticky and drawn over the overview, so a card under one is a card with a
        // strip of it hidden. Checked for one row and for two, since the block is centred and a
        // second row grows it in both directions.
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
                    assert!(
                        foot <= height - BAR_H,
                        "{width}x{height}: a card runs into the window list at {foot}"
                    );
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
            // **The caption counts as the card.** It sits below the box, and a click on the
            // words naming a desktop is a click on that desktop — a version that hit-tested the
            // box alone would dismiss the overview instead.
            let caption = y + (s.card_h() + CARD_CAPTION_GAP + CARD_CAPTION_H / 2) as i32;
            assert_eq!(card_at(x + 4, caption, n, s), Some(i), "the caption of card {i}");
        }
        // The gap between two cards is background, which is what dismisses.
        let (x0, y0, w0, _) = card_rect(0, n, s);
        assert_eq!(card_at(x0 + w0 as i32 + CARD_GAP as i32 / 2, y0 + 4, n, s), None);
        assert_eq!(card_at(-1, y0, n, s), None, "a negative x is off the overview, not card 0");
        assert_eq!(card_at(x0, BAR_H as i32, n, s), None, "the top bar is not a card");
    }

    #[test]
    fn a_window_box_is_where_the_window_is_on_the_screen() {
        let s = Screen { width: 1360, height: 768 };
        let card = card_rect(0, 1, s);
        let interior = (card.0 + CARD_BORDER as i32, card.1 + CARD_BORDER as i32);

        // A window filling the screen below the bar fills the card below its strip.
        let full = window_box(card, (0, BAR_H as i32), (s.width, s.height - BAR_H), s);
        assert_eq!((full.0, full.1), (interior.0, interior.1 + s.card_strip_h() as i32));
        assert_eq!(full.2, s.card_interior_w());
        assert_eq!(full.1 as u32 + full.3, interior.1 as u32 + s.card_interior_h());

        // **A window at the very top starts below the strip, not under it** — the strip is the
        // bar, and a window is never under the bar.
        let top = window_box(card, (0, 0), (100, 100), s);
        assert_eq!(top.1, interior.1 + s.card_strip_h() as i32);

        // Half way across the screen is half way across the card.
        let mid = window_box(card, (s.width as i32 / 2, BAR_H as i32), (100, 100), s);
        assert!((mid.0 - interior.0).abs_diff(s.card_interior_w() as i32 / 2) <= 1);

        // **Clamped into the interior.** A window dragged off the right of the screen must not
        // draw over the card beside it — which is a thing you can do, and the only cue that it
        // went wrong would be pixels in the wrong card.
        let off = window_box(card, (s.width as i32 - 20, s.height as i32 - 20), (900, 600), s);
        assert!(off.0 as u32 + off.2 <= interior.0 as u32 + s.card_interior_w(), "{off:?}");
        assert!(off.1 as u32 + off.3 <= interior.1 as u32 + s.card_interior_h(), "{off:?}");

        // And a window too small to scale to anything is still drawn.
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

        // Comments, blank lines, surrounding space and a CRLF file all survive.
        let a = parse_entry("# an entry\r\n\r\n  name  =  \"Text Editor\"  \r\nexec=\"nxedit\"\r\n")
            .expect("valid despite CRLF and spacing");
        assert_eq!((a.name.as_str(), a.exec.as_str()), ("Text Editor", "nxedit"));

        // A file mapped from the store arrives page-padded with NULs; the tail is not lines.
        let padded = alloc::format!("name = \"Files\"\nexec = \"nxfiles\"\n{}", "\0".repeat(64));
        assert!(parse_entry(&padded).is_some(), "a page-padded entry must still parse");
    }

    #[test]
    fn a_malformed_entry_is_refused_rather_than_half_read() {
        // **Each of these would otherwise become a menu row that launches nothing.**
        for bad in [
            "name = \"Files\"\n",                    // no exec
            "exec = \"nxfiles\"\n",                  // no name
            "name = Files\nexec = nxfiles\n",       // unquoted
            "name = \"Files\nexec = \"nxfiles\"\n",  // one quote — the trap `theme.rs` records
            "name = \"\"\nexec = \"nxfiles\"\n",     // empty name
            "name = \"Files\"\nexec = \"\"\n",       // empty exec
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
