//! The window list: entries, desktops and their labels.

use super::*;

/// One entry in the bottom bar's window list: the shell's copy of what the manager events told it
/// (`WindowCreated`, `WindowDestroyed`, `WindowTitle`, `WindowFocus`), so it never polls
/// `/dev/draw/<id>/info` and races a second copy of the stack.
pub(super) struct WinEntry {
    pub(super) id: u32,
    pub(super) title: alloc::string::String,
    pub(super) focused: bool,
    pub(super) minimized: bool,
    /// Where the window is, kept current by `WindowGeometry`. A maximised window has to come back
    /// to it, and the compositor keeps no `maximized` flag, deliberately.
    pub(super) origin: (i32, i32),
    /// The window's committed size, kept current by `WindowGeometry`. A capture may not scale up,
    /// so a window smaller than a thumbnail is captured at its own size.
    pub(super) size: (u32, u32),
    /// Which desktop it is on. Tracked here, not read back: the shell is the only thing that
    /// changes it, and the list filters on it every redraw.
    pub(super) desktop: u32,
}

/// One desktop. The shell owns these; the compositor knows only a number
/// (`ui-composition-model.md` §6), so the two cannot disagree.
pub(super) struct Desktop {
    /// The id the compositor is told: never reused, so a window's attribute cannot come to mean a
    /// different desktop.
    pub(super) id: u32,
    /// What a user called it, or empty. Naming is what makes a desktop persist
    /// ([`normalize_desktops`]).
    pub(super) name: alloc::string::String,
}

/// Keep the desktop list to the lifecycle rule, and return whether it changed.
///
/// Naming pins a desktop: an unnamed empty one is removed, a named one kept, and the list always
/// ends with one empty unnamed desktop to create into. Called after every change to either list.
pub(super) fn normalize_desktops(
    desktops: &mut alloc::vec::Vec<Desktop>,
    entries: &[WinEntry],
    current: &mut u32,
    next_id: &mut u32,
) -> bool {
    let before: alloc::vec::Vec<u32> = desktops.iter().map(|d| d.id).collect();
    let occupied = |id: u32| entries.iter().any(|e| e.desktop == id);
    // Where the current desktop sits now, so a removal lands on whatever takes its place.
    let was_at = desktops.iter().position(|d| d.id == *current);

    // The trailing scratch slot is exempt from removal: deleting and re-creating it would churn
    // an id every time a window moved.
    let last = desktops.last().map(|d| d.id).unwrap_or(0);
    desktops.retain(|d| !d.name.is_empty() || occupied(d.id) || d.id == last);

    // Checked after the retain, so a slot just filled gets a successor in the same pass.
    let needs_slot = desktops.last().is_none_or(|d| !d.name.is_empty() || occupied(d.id));
    if needs_slot {
        desktops.push(Desktop { id: *next_id, name: alloc::string::String::new() });
        *next_id += 1;
    }

    // A removed current desktop lands on whatever took its slot, by position: moving the last
    // window off desktop 1 should follow it, not strand the person on the empty scratch slot.
    if !desktops.iter().any(|d| d.id == *current) {
        let i = was_at.unwrap_or(0).min(desktops.len().saturating_sub(1));
        *current = desktops.get(i).map(|d| d.id).unwrap_or(1);
    }

    before != desktops.iter().map(|d| d.id).collect::<alloc::vec::Vec<u32>>()
}

/// What the switcher shows beside its cells: the desktop's name, or its position when it has none
/// (ids are never reused and stop matching what a person sees; `Super+N` addresses the Nth).
pub(super) fn desktop_label(desktops: &[Desktop], current: u32) -> alloc::string::String {
    let mut s = alloc::string::String::new();
    let idx = desktops.iter().position(|d| d.id == current).unwrap_or(0);
    match desktops.get(idx) {
        Some(d) if !d.name.is_empty() => s.push_str(&d.name),
        _ => {
            // Capitalised, and a *name* is not: a name a person chose is theirs.
            s.push_str("Desktop ");
            let n = idx + 1;
            if n >= 10 {
                s.push((b'0' + (n / 10) as u8) as char);
            }
            s.push((b'0' + (n % 10) as u8) as char);
        }
    }
    s
}

/// The label the serial log gives an entry: its title, marked with its state. The log's, not the
/// bar's: every gate reads the list from the serial log, where the bar's dot cannot go.
fn entry_label(e: &WinEntry) -> alloc::string::String {
    let mut s = alloc::string::String::new();
    s.push_str(if e.minimized {
        "_ "
    } else if e.focused {
        "> "
    } else {
        "  "
    });
    s.push_str(&entry_title(e));
    s
}

pub(super) fn entry_title(e: &WinEntry) -> alloc::string::String {
    let mut s = alloc::string::String::new();
    if e.title.is_empty() {
        s.push_str("window ");
        let mut n = e.id;
        let mut digits = [0u8; 10];
        let mut i = 0;
        if n == 0 {
            digits[0] = b'0';
            i = 1;
        }
        while n > 0 {
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
            i += 1;
        }
        while i > 0 {
            i -= 1;
            s.push(digits[i] as char);
        }
    } else {
        s.push_str(&e.title);
    }
    s
}

/// The windows the bar shows: on the current desktop, in creation order, at most `capacity`
/// ([`task_capacity`]), so no button sits under the switcher.
pub(super) fn visible_entries(entries: &[WinEntry], current: u32, capacity: usize) -> alloc::vec::Vec<&WinEntry> {
    entries.iter().filter(|e| e.desktop == current).take(capacity).collect()
}

pub(super) fn desktop_facts(
    desktops: &[Desktop],
    entries: &[WinEntry],
    current: u32,
) -> (alloc::vec::Vec<bool>, usize, alloc::string::String) {
    let occupied = desktops.iter().map(|d| entries.iter().any(|e| e.desktop == d.id)).collect();
    let at = desktops.iter().position(|d| d.id == current).unwrap_or(0);
    (occupied, at, desktop_label(desktops, current))
}

/// How many windows the bottom bar holds beside the switcher as it is now: asked, not kept, as it
/// changes with the desktop's name and count.
pub(super) fn task_capacity(
    desktops: &[Desktop],
    entries: &[WinEntry],
    current: u32,
    theme: &Theme,
    font: &Font,
    screen: Screen,
) -> usize {
    let (occupied, at, label) = desktop_facts(desktops, entries, current);
    let d = panel::Desktops { occupied: &occupied, current: at, label: &label };
    let w = panel::switcher_width(&d, theme, &FontMetrics::new(font, theme.font_px));
    panel::task_capacity(screen.width, w)
}

/// How many windows are on `desktop`, as the caption says it ("empty", "1 window").
pub(super) fn window_count(entries: &[WinEntry], desktop: u32) -> alloc::string::String {
    let n = entries.iter().filter(|e| e.desktop == desktop && !e.minimized).count();
    match n {
        0 => alloc::string::String::from("empty"),
        1 => alloc::string::String::from("1 window"),
        _ => {
            let mut s = alloc::string::String::new();
            let mut digits = alloc::vec::Vec::new();
            let mut v = n;
            while v > 0 {
                digits.push(b'0' + (v % 10) as u8);
                v /= 10;
            }
            for d in digits.iter().rev() {
                s.push(*d as char);
            }
            s.push_str(" windows");
            s
        }
    }
}

/// Log the window list, so a gate can read what the bar is showing: `check-login` reads the
/// serial console, not pixels.
pub(super) fn log_window_list(shown: &[&WinEntry], label: &str, desktops: usize) {
    let mut l = Line::new();
    l.s(b"desktop-shell: window list on ").s(label.as_bytes());
    l.s(b" of ").u(desktops as u64);
    if shown.is_empty() {
        l.s(b" (empty)");
    }
    for e in shown {
        l.s(b" [").u(e.id as u64).s(b":");
        l.untrusted(entry_label(e).as_bytes());
        l.s(b"]");
    }
    l.end();
}

pub(super) type RestoreList = alloc::vec::Vec<(u32, (i32, i32, u32, u32))>;

/// The windows as show-desktop sees them — see [`ShownDesktop`].
pub(super) fn windows_of(entries: &[WinEntry]) -> alloc::vec::Vec<desktop_shell::Window> {
    entries
        .iter()
        .map(|e| desktop_shell::Window { id: e.id, desktop: e.desktop, minimized: e.minimized, focused: e.focused })
        .collect()
}
