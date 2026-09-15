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

/// One graphical application, from a desktop entry under `/applications`.
///
/// **The display name and the program are different strings, and that is the point** (M14 Part
/// H). The modal showed `/bin` — every service, server and CLI tool on the system, under the
/// name of its binary. It shows what a package *declares* is an application now, under the name
/// that package gives it.
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
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Screen {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// Each bar's height: the top bar, and the window list at the foot of the screen.
pub const BAR_H: u32 = 24;

/// Width of one window-list entry, in pixels.
pub const ENTRY_W: u32 = 180;

/// Width of the desktop indicator at the window list's right-hand end.
pub const INDICATOR_W: u32 = 160;

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

    /// Where the indicator starts, in bar-local x. Clicks at or past this belong to it.
    ///
    /// **Anchored to the screen's right edge.** The first version laid the indicator out after the
    /// entries, so it was drawn at `n * ENTRY_W` and coincided with its hit region at exactly one
    /// window count (PR #243 review, blocking 2); a flexible spacer between the entries and the
    /// indicator is what puts it here, and [`max_entries`](Self::max_entries) reserves the width.
    pub const fn indicator_x(self) -> u32 {
        self.width.saturating_sub(INDICATOR_W)
    }

    /// How many entries the window list can show without one being painted under the indicator.
    ///
    /// **The invariant is the product**: `max_entries × ENTRY_W + INDICATOR_W ≤ width`. With the
    /// capacity computed from the full width, a full bar painted an entry across the indicator's
    /// hit region, and clicking the last window switched desktops (PR #243 review, blocking 2).
    /// Entries past the limit are not shown; the window is still there.
    pub const fn max_entries(self) -> usize {
        (self.indicator_x() / ENTRY_W) as usize
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn no_entry_is_ever_painted_under_the_indicator() {
        for (width, height) in SIZES {
            let s = Screen { width, height };
            let used = s.max_entries() as u32 * ENTRY_W;
            assert!(used + INDICATOR_W.min(width) <= width, "{width}x{height}: {used} + indicator");
            assert!(used <= s.indicator_x(), "{width}x{height}: an entry crosses the indicator");
        }
    }

    #[test]
    fn the_layout_at_the_old_size_and_at_the_gate_size() {
        let old = Screen { width: 1280, height: 800 };
        assert_eq!((old.indicator_x(), old.max_entries(), old.window_list_y()), (1120, 6, 776));
        assert_eq!((old.thumb_cols(), old.pitch()), (4, 5120));
        let gate = Screen { width: 1360, height: 768 };
        assert_eq!((gate.indicator_x(), gate.max_entries(), gate.window_list_y()), (1200, 6, 744));
        assert_eq!((gate.thumb_cols(), gate.pitch()), (4, 5440));
    }

    #[test]
    fn a_screen_too_small_for_the_chrome_degrades_rather_than_wrapping() {
        let tiny = Screen { width: 100, height: 10 };
        assert_eq!(tiny.indicator_x(), 0);
        assert_eq!(tiny.max_entries(), 0);
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
        // **Each of these would otherwise become a modal row that launches nothing.**
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
