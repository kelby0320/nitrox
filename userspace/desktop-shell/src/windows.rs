use super::*;

/// One entry in the bottom bar's window list.
///
/// **The shell's copy of what the compositor told it**, which is the point of the manager
/// events: `WindowCreated`, `WindowDestroyed`, `WindowTitle` and `WindowFocus` are exactly the
/// four facts a taskbar shows, and a shell that polled `/dev/draw/<id>/info` for them would be
/// keeping a second copy of the stack and racing it.
pub(super) struct WinEntry {
    /// Compositor window id.
    pub(super) id: u32,
    /// What the client called it, or empty until it says.
    pub(super) title: alloc::string::String,
    /// Whether it holds the keyboard.
    pub(super) focused: bool,
    /// Whether the shell has minimized it.
    pub(super) minimized: bool,
    /// Where the window is, kept current by `WindowGeometry`.
    ///
    /// **Needed because a maximised window has to come back.** The rectangle it returns to is
    /// the shell's to remember — the compositor keeps no `maximized` flag, deliberately — and
    /// "where it was" is a position as well as a size.
    pub(super) origin: (i32, i32),
    /// The window's committed size, from `WindowCreated` and kept current by `WindowGeometry`.
    ///
    /// **Needed because a capture may not scale up.** A thumbnail is a fixed size in the
    /// overview's grid, and a window smaller than that in either axis has to be captured at its
    /// own size and drawn smaller, rather than the request being refused.
    pub(super) size: (u32, u32),
    /// Which desktop it is on — the shell's copy of the attribute it set.
    ///
    /// **Tracked here rather than read back from `/dev/draw/<id>/info`.** The shell is the only
    /// thing that changes it, so a read-back would be asking the compositor to repeat what this
    /// process just said; and the list has to filter on it every redraw, which is not a place
    /// to be resolving a path per window.
    pub(super) desktop: u32,
}

/// One desktop. **The shell owns these; the compositor knows only a number.**
///
/// `ui-composition-model.md` §6: the compositor holds a `desktop` attribute per window and one
/// `current` value, and no notion of a desktop *object* — no list, no names, no lifecycle. That
/// is what keeps the two from being able to disagree, and it is why everything below is here.
pub(super) struct Desktop {
    /// The id the compositor is told. Stable for the desktop's life and never reused, so a
    /// window's attribute cannot come to mean a different desktop underneath it.
    pub(super) id: u32,
    /// What a user called it, or empty. **Naming is what makes a desktop persist** — see
    /// [`normalize_desktops`].
    pub(super) name: alloc::string::String,
}

/// Keep the desktop list to the lifecycle rule, and return whether it changed.
///
/// **Governing decision 3 — "naming pins a desktop" — implemented in one place.** An *unnamed*
/// empty desktop is removed; a *named* one is kept; and the list always ends with exactly one
/// empty unnamed desktop to create into. That makes `ui-composition-model.md` §6's "name it if
/// it turns out to matter" the lifecycle rule itself rather than a second mechanism: a scratch
/// desktop costs nothing and cleans itself up, a purposeful one survives its last window
/// leaving, and a name a user deliberately set is never discarded — which was GNOME 3's
/// surprise.
///
/// Called after **every** change to either list, because the rule is about the pair: a window
/// moving empties one desktop and fills another, and both halves have to be reconsidered.
pub(super) fn normalize_desktops(
    desktops: &mut alloc::vec::Vec<Desktop>,
    entries: &[WinEntry],
    current: &mut u32,
    next_id: &mut u32,
) -> bool {
    let before: alloc::vec::Vec<u32> = desktops.iter().map(|d| d.id).collect();
    let occupied = |id: u32| entries.iter().any(|e| e.desktop == id);
    // Where the current desktop sits *now*, so that if it is about to be removed the shell can
    // land on whatever takes its place rather than somewhere arbitrary.
    let was_at = desktops.iter().position(|d| d.id == *current);

    // **The trailing scratch slot is exempt from removal, not from the rule.** It is empty and
    // unnamed by definition, so without this the rule would delete the very desktop it requires
    // to exist and then re-create it, churning an id every time a window moved.
    let last = desktops.last().map(|d| d.id).unwrap_or(0);
    desktops.retain(|d| !d.name.is_empty() || occupied(d.id) || d.id == last);

    // Always one empty unnamed desktop at the end to create into. Checked after the retain, so
    // a scratch slot that has just been filled gets a successor in the same pass.
    let needs_slot = desktops.last().is_none_or(|d| !d.name.is_empty() || occupied(d.id));
    if needs_slot {
        desktops.push(Desktop { id: *next_id, name: alloc::string::String::new() });
        *next_id += 1;
    }

    // **A removed current desktop lands on whatever took its slot**, by position rather than
    // by id. Moving the last window off desktop 1 removes it, and the desktop that was second
    // becomes first — which is where the window went, so following it is what a person means.
    // Falling back to the end of the list instead would strand them on the empty scratch slot
    // holding nothing, immediately after an action whose whole point was that window.
    if !desktops.iter().any(|d| d.id == *current) {
        let i = was_at.unwrap_or(0).min(desktops.len().saturating_sub(1));
        *current = desktops.get(i).map(|d| d.id).unwrap_or(1);
    }

    before != desktops.iter().map(|d| d.id).collect::<alloc::vec::Vec<u32>>()
}

/// What the switcher shows beside its cells: the desktop's name, or its position when it has none.
///
/// **Its position, not its id.** Ids are stable and never reused, so after a few desktops have
/// come and gone they stop matching what a person sees — and `Super+N` addresses the Nth
/// desktop, which is how anyone thinks about them. `desktop-shell.md` §7 wants "something
/// human"; a name is that, and a position is the honest fallback.
pub(super) fn desktop_label(desktops: &[Desktop], current: u32) -> alloc::string::String {
    let mut s = alloc::string::String::new();
    let idx = desktops.iter().position(|d| d.id == current).unwrap_or(0);
    match desktops.get(idx) {
        Some(d) if !d.name.is_empty() => s.push_str(&d.name),
        _ => {
            // **Capitalised, and a *name* is not** (M14 Part F). A generated label is a title —
            // every desktop names its own the way "Applications" and "No Windows" do — but a name
            // a person chose is theirs, and title-casing it would be the shell editing their text.
            // So the capital belongs here, in the fallback, rather than at the call sites — of
            // which there are six: the bottom bar and the overview's sidebar draw it, and four
            // serial lines carry it.
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

/// The label the serial log gives an entry: its title, marked with what the shell knows about it.
///
/// **The log's, and no longer the bar's** (desktop refresh, Part C). The bar draws the design's
/// dot — dim for a window put away, the accent for the one with the keyboard, `ok` for the rest —
/// and these markers stay here because every gate reads the list from the serial log, where a
/// colour cannot go. Both are built from the same two flags, so they cannot disagree.
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

/// What an entry's button reads: its title, or the window's id when it has none.
pub(super) fn entry_title(e: &WinEntry) -> alloc::string::String {
    let mut s = alloc::string::String::new();
    if e.title.is_empty() {
        // A window that has not set a title still needs to be clickable, and an empty button
        // is not. Its id is the only other name it has.
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

/// The windows the bar shows: on the current desktop, in creation order.
///
/// **The filter Part C could not write.** It listed every `normal` window because nothing
/// switched desktops yet; now that something does, a bar showing another desktop's windows
/// would be showing you what you just navigated away from.
///
/// **At most `capacity`** — [`task_capacity`] — so no button is laid out under the switcher, and
/// the chord that minimises the focused window is bounded by the same count as the bar.
pub(super) fn visible_entries(entries: &[WinEntry], current: u32, capacity: usize) -> alloc::vec::Vec<&WinEntry> {
    entries.iter().filter(|e| e.desktop == current).take(capacity).collect()
}

/// What the switcher needs to know about the desktops: whether each has windows, where the
/// current one is, and its label.
pub(super) fn desktop_facts(
    desktops: &[Desktop],
    entries: &[WinEntry],
    current: u32,
) -> (alloc::vec::Vec<bool>, usize, alloc::string::String) {
    let occupied = desktops.iter().map(|d| entries.iter().any(|e| e.desktop == d.id)).collect();
    let at = desktops.iter().position(|d| d.id == current).unwrap_or(0);
    (occupied, at, desktop_label(desktops, current))
}

/// How many windows the bottom bar holds beside the switcher as it is now.
///
/// **Asked wherever the count matters, rather than kept**: it changes with the desktop's name and
/// how many desktops there are, and the chord bound and the bar must agree about it in the same
/// pass. A layout of one row is all it costs.
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

/// How many windows are on `desktop`, as the caption says it.
///
/// **"empty" rather than "0 windows"**, and the singular where there is one — the design's own
/// wording, and the difference between a card that reads as a sentence and one that reads as a
/// counter.
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

/// Log the window list, so a gate can read what the bar is showing.
///
/// **The bar's contents reach a gate this way and no other.** What it draws is pixels in a
/// window on a release image, and the only gate that boots one is `check-login`, which reads
/// the serial console. A line per change is what makes "the list has this window, focused"
/// assertable at all.
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
