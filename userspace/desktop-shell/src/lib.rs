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

/// The overview's sidebar width, at the right-hand edge.
pub const SIDE_W: u32 = 200;

/// A thumbnail's width in the overview's grid.
pub const THUMB_W: u32 = 240;

/// Space around each thumbnail.
pub const THUMB_PAD: u32 = 16;

impl Screen {
    /// Bytes per row of a buffer as wide as the screen — both bars, the wallpaper, the overview.
    pub const fn pitch(self) -> usize {
        self.width as usize * 4
    }

    /// Where the window list is placed: its top edge, one bar above the bottom of the screen.
    pub const fn window_list_y(self) -> i32 {
        self.height.saturating_sub(BAR_H) as i32
    }

    /// How many thumbnails fit across the overview's grid, beside the sidebar — at least one, so
    /// a screen too narrow for a full column still lays thumbnails out rather than dividing by
    /// zero.
    pub const fn thumb_cols(self) -> u32 {
        let cols = self.width.saturating_sub(SIDE_W) / (THUMB_W + THUMB_PAD);
        if cols == 0 { 1 } else { cols }
    }
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
            assert!(s.thumb_cols() >= 1, "{width}x{height}");
        }
    }

    #[test]
    fn the_layout_at_the_old_size_and_at_the_gate_size() {
        let old = Screen { width: 1280, height: 800 };
        assert_eq!((old.window_list_y(), old.thumb_cols(), old.pitch()), (770, 4, 5120));
        let gate = Screen { width: 1360, height: 768 };
        assert_eq!((gate.window_list_y(), gate.thumb_cols(), gate.pitch()), (738, 4, 5440));
    }

    #[test]
    fn a_screen_too_small_for_the_chrome_degrades_rather_than_wrapping() {
        let tiny = Screen { width: 100, height: 10 };
        assert_eq!(tiny.window_list_y(), 0, "saturating, not a negative origin from a wrap");
        assert_eq!(tiny.thumb_cols(), 1);
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
