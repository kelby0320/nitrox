//! `desktop-shell` — the graphical session's leader, and the compositor's first real manager.
//!
//! **What M6 built for a manager has been exercised by a test client until now.** Placement,
//! restacking, focus, the initial-configure hold and the five manager events all have gates,
//! and every one of those gates is `ui-testclient` pretending. This is the process they were
//! for. (Of the five events, four are now consumed here as well; `WindowDestroyed` has no gate
//! that reaches it, because no gate closes a `normal` window in a session.)
//!
//! It draws a **top bar** across the screen and, since M8 Part C, a **bottom bar** carrying one
//! entry per `normal` window — click to raise, click the focused one to minimize, `Super+H` to
//! minimize without reaching for the bar. Both reserve space with a `panel` strut so ordinary
//! windows do not sit under them.
//!
//! **It does not bind its own endpoint**, which is what reconciles a process that both serves
//! and constructs with `syscaps.md` (`graphical-session.md` §3): it holds `BIND_NAMESPACE` to
//! construct *application* namespaces continuously, not to register itself once.
//!
//! Since M8 Part F it **serves `/dev/desktop`** and binds it into every application namespace
//! it constructs — not into the session namespace, which is its own and where nothing would
//! resolve it. A `desktop` command in `/bin` is the consumer that proves the binding is
//! reachable, which had to exist because a process cannot verify a binding of *itself* by
//! using it: a resolve is forwarded to whoever serves the path.
//!
//! `#![no_std]` + `#![no_main]`, with `alloc` — the toolkit builds an element tree per frame.

#![no_std]
#![no_main]

extern crate alloc;

use libdraw::format::PixelFormat;
use libdraw::framebuffer::{Geometry, MemFramebuffer};
use libdraw::geom::{Rect, Size};
use libdraw::text::Font;
use libkern::debug::Line;
use libkern::*;

/// `EV_KEY` code for Escape — the modal's dismissal.
const KEY_ESC: u16 = 1;
/// `EV_KEY` code for Enter — the modal's launch.
const KEY_ENTER: u16 = 28;
/// `EV_KEY` code for `h`, the minimize chord's key.
///
/// Named here rather than taken from `libkern::abi`, which publishes the keys the *ABI* needs
/// (modifiers, the function row, the keypad) and not every letter. A shell binding a chord is
/// the first thing that wants one.
const KEY_H: u16 = 35;
/// How many window-geometry lines have been printed. See [`MAX_LOGGED_GEOMETRY`].
static mut GEOMETRY_LOGGED: u32 = 0;

/// How many window-geometry lines to print before going quiet.
///
/// The event behind them is client-driven — a client committing buffers of changing size
/// produces one each — so this is bounded the way the compositor bounds its own input
/// diagnostics, and for the same reason.
const MAX_LOGGED_GEOMETRY: u32 = 16;

/// The shell's id for its minimize chord. Manager-chosen, and never zero.
const HOTKEY_MINIMIZE: u32 = 1;
/// `EV_KEY` code for `r`, the rename chord's key.
const KEY_R: u16 = 19;
/// The shell's id for its rename chord.
const HOTKEY_RENAME: u32 = 2;
/// How many desktops the number chords reach.
///
/// **Four, not nine.** `MAX_HOTKEYS` is sixteen and each desktop costs two chords — one to
/// switch, one to move — so nine would leave no room for the minimize and rename chords here,
/// let alone Part E's overview. Desktops past the fourth exist and hold windows; they are
/// reached by the indicator rather than by a chord.
const CHORD_DESKTOPS: u32 = 4;
/// Ids `HOTKEY_SWITCH_BASE + n` switch to desktop `n`; `HOTKEY_MOVE_BASE + n` move to it.
const HOTKEY_SWITCH_BASE: u32 = 10;
/// See [`HOTKEY_SWITCH_BASE`].
const HOTKEY_MOVE_BASE: u32 = 20;
/// `EV_KEY` code for `1`. `2`, `3` and `4` follow it.
const KEY_1: u16 = 2;
use librsproto::surface::{
    CreateWindowRequest, Edge, MgrLayout, OP_MGR_QUERY_LAYOUT, OP_MGR_SET_WINDOW_DESKTOP, Role,
    STICKY_DESKTOP,
};
use libsurface::{Session, Transport};
use libsurface::ipc::ChannelTransport;
use libui::element::{
    Element, Insets, bevel, column, custom, fill, offset, padding, row, sized, stack, text,
};
use libui::diff::Tree;
use libui::layout::layout;
use libui::route::Router;
use libui::paint::{FontMetrics, Theme, paint, paint_over};
use libui::widget::{ListRow, ListState, TextFieldState, WidgetState, list_view, popup_frame, text_field};

/// `alloc` backing: the toolkit builds an element tree per frame.
#[global_allocator]
static ALLOC: libheap::Heap = libheap::Heap;

/// The wall clock as the top bar shows it, or empty when there is none.
///
/// **`YYYY-MM-DD HH:MM`, and no month names** (M11 Part E batch 9). There is no timezone database
/// and no locale, so a localised form would be a fiction — the same reason `date` emits fields and
/// prints UTC. It also means the bar and the command agree about what time it is, which is worth
/// more here than looking like somebody else's desktop.
///
/// **Empty when the clock is unset**, rather than 1970. The kernel reports `Unsupported` when the
/// RTC could not be read at boot rather than inventing an epoch, and a bar that showed a
/// fabricated date would be undoing that decision one layer up.
fn clock_text() -> alloc::string::String {
    let mut nanos = 0u64;
    // SAFETY: `&mut nanos` is a valid writable u64 out-param for the clock read.
    let r = unsafe {
        libkern::syscall2(
            libkern::SYS_CLOCK_READ,
            libkern::abi::CLOCK_REALTIME,
            (&raw mut nanos) as u64,
        )
    };
    if r < 0 {
        return alloc::string::String::new();
    }
    let c = libtime::civil_from_unix(nanos);
    // `format_civil` is `YYYY-MM-DD HH:MM:SS`; a bar that counted seconds would repaint sixty
    // times a minute to show something nobody reads at that resolution.
    let full = libtime::format_civil(&c);
    alloc::string::String::from(full.get(..16).unwrap_or(full.as_str()))
}

/// When the displayed minute next changes, as a monotonic deadline.
///
/// **Aligned to the minute rather than "a minute from now"**, so the bar changes when the clock
/// does. The alignment is read from the *wall* clock and the deadline is *monotonic*, which is
/// not a mix-up: one says how far into the minute we are, the other is what `sys_wait` counts.
fn next_minute(now: u64) -> u64 {
    let mut nanos = 0u64;
    // SAFETY: as above.
    let r = unsafe {
        libkern::syscall2(
            libkern::SYS_CLOCK_READ,
            libkern::abi::CLOCK_REALTIME,
            (&raw mut nanos) as u64,
        )
    };
    const MINUTE: u64 = 60 * 1_000_000_000;
    if r < 0 {
        // No clock to follow. Wake in a minute anyway, so a clock that is set later appears
        // without a restart.
        return now.saturating_add(MINUTE);
    }
    now.saturating_add(MINUTE - nanos % MINUTE)
}

/// The screen's width, which the top bar spans.
///
/// Fixed rather than queried: the compositor has no "what size is the screen" op, and adding
/// one to draw a bar would be a protocol change made for a stub's convenience. `check-display`
/// already hardcodes the same 1280×800.
const SCREEN_W: u32 = 1280;
/// The top bar's height.
const BAR_H: u32 = 24;
/// The screen's height, which bounds the placement cascade. Fixed for the same reason
/// [`SCREEN_W`] is.
const SCREEN_H: i32 = 800;
/// Bytes per row.
const BAR_PITCH: usize = (SCREEN_W as usize) * 4;
/// How many buffers the bar attaches.
const BUFFERS: usize = 2;

/// Write one line to the debug console.
fn kprint(msg: &[u8]) {
    // SAFETY: SYS_DEBUG_KPRINT copies `len` bytes from `ptr`.
    unsafe { syscall4(SYS_DEBUG_KPRINT, msg.as_ptr() as u64, msg.len() as u64, 0, 0) };
}

/// Report and exit.
fn fail(msg: &[u8]) -> ! {
    kprint(msg);
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}

/// How wide the applications button is, in pixels.
///
/// The modal's **only** trigger for now. `desktop-shell.md` §4 gives it two — this button and
/// the Super key — but the Super key is a *global hotkey*, which §8 makes a capability rather
/// than an ambient grab, and the compositor has none. A `panel` does not take keyboard focus,
/// so a key would not reach this process at all; a click routes to the window under the
/// pointer whatever holds focus, which is why the button is the half that can exist yet.
const APPS_BUTTON_W: u32 = 120;

/// The top bar's element tree.
fn bar_view(clock: &str) -> Element<()> {
    // **One thing, and it does something** (M11 Part E batch 4). There was a "nitrox" label
    // beside the button — a word with no handler, which reads as a menu that does not open. A
    // control that looks live and is not is the defect M8's overview shipped three of; a label
    // that looks like a control is the same defect with less code behind it.
    row(alloc::vec![
        sized(
            libdraw::geom::Size::new(APPS_BUTTON_W, 0),
            padding(Insets { top: 4, right: 8, bottom: 4, left: 8 }, text("Applications")),
        ),
        // **Centred on the screen, not on what is left of it.** Two equal flexible gaps put the
        // clock in the middle of the space *between* them, so without the balancing slot on the
        // right it would sit half the button's width off centre. The slot is empty; it exists to
        // make the arithmetic symmetric (M11 Part E batch 9).
        sized(libdraw::geom::Size::new(0, 0), text("")).flex(1),
        padding(Insets { top: 4, right: 8, bottom: 4, left: 8 }, text(clock)),
        sized(libdraw::geom::Size::new(0, 0), text("")).flex(1),
        sized(libdraw::geom::Size::new(APPS_BUTTON_W, 0), text("")),
    ])
}

/// One entry in the bottom bar's window list.
///
/// **The shell's copy of what the compositor told it**, which is the point of the manager
/// events: `WindowCreated`, `WindowDestroyed`, `WindowTitle` and `WindowFocus` are exactly the
/// four facts a taskbar shows, and a shell that polled `/dev/draw/<id>/info` for them would be
/// keeping a second copy of the stack and racing it.
struct WinEntry {
    /// Compositor window id.
    id: u32,
    /// What the client called it, or empty until it says.
    title: alloc::string::String,
    /// Whether it holds the keyboard.
    focused: bool,
    /// Whether the shell has minimized it.
    minimized: bool,
    /// Where the window is, kept current by `WindowGeometry`.
    ///
    /// **Needed because a maximised window has to come back.** The rectangle it returns to is
    /// the shell's to remember — the compositor keeps no `maximized` flag, deliberately — and
    /// "where it was" is a position as well as a size.
    origin: (i32, i32),
    /// The window's committed size, from `WindowCreated` and kept current by `WindowGeometry`.
    ///
    /// **Needed because a capture may not scale up.** A thumbnail is a fixed size in the
    /// overview's grid, and a window smaller than that in either axis has to be captured at its
    /// own size and drawn smaller, rather than the request being refused.
    size: (u32, u32),
    /// Which desktop it is on — the shell's copy of the attribute it set.
    ///
    /// **Tracked here rather than read back from `/dev/draw/<id>/info`.** The shell is the only
    /// thing that changes it, so a read-back would be asking the compositor to repeat what this
    /// process just said; and the list has to filter on it every redraw, which is not a place
    /// to be resolving a path per window.
    desktop: u32,
}

/// One desktop. **The shell owns these; the compositor knows only a number.**
///
/// `ui-composition-model.md` §6: the compositor holds a `desktop` attribute per window and one
/// `current` value, and no notion of a desktop *object* — no list, no names, no lifecycle. That
/// is what keeps the two from being able to disagree, and it is why everything below is here.
struct Desktop {
    /// The id the compositor is told. Stable for the desktop's life and never reused, so a
    /// window's attribute cannot come to mean a different desktop underneath it.
    id: u32,
    /// What a user called it, or empty. **Naming is what makes a desktop persist** — see
    /// [`normalize_desktops`].
    name: alloc::string::String,
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
fn normalize_desktops(
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

/// What the indicator shows: the desktop's name, or its position when it has none.
///
/// **Its position, not its id.** Ids are stable and never reused, so after a few desktops have
/// come and gone they stop matching what a person sees — and `Super+N` addresses the Nth
/// desktop, which is how anyone thinks about them. `desktop-shell.md` §7 wants "something
/// human"; a name is that, and a position is the honest fallback.
fn desktop_label(desktops: &[Desktop], current: u32) -> alloc::string::String {
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

/// Width of one window-list entry, in pixels.
const ENTRY_W: u32 = 180;

/// Width of the desktop indicator at the bar's right-hand end.
const INDICATOR_W: u32 = 160;

/// Where the indicator starts, in bar-local x. Clicks at or past this belong to it.
///
/// **Anchored to the screen's right edge, and now actually drawn there.** The first version
/// asserted this while laying the indicator out *after* the entries, so it was drawn at
/// `n * ENTRY_W` and the two coincided at exactly one window count — everywhere else the
/// indicator a user could see did nothing, and at a full bar the region hit-tested as the
/// indicator was *painted* as the last window entry, so clicking that entry switched desktops
/// (PR #243 review, blocking 2). A flexible spacer between the entries and the indicator is
/// what makes the claim true, and `MAX_ENTRIES` reserves the width so the indicator is never
/// squeezed into what is left.
const INDICATOR_X: u32 = SCREEN_W - INDICATOR_W;

/// **No window entry may be painted under the indicator's hit region.**
///
/// This is the half of the misalignment that is a *correctness* bug rather than a usability
/// one: with `MAX_ENTRIES` computed from the full screen width, a full bar painted an entry
/// across x∈[1120,1260) while the hit-test read that range as the indicator, so clicking the
/// last window switched desktops instead of raising it (PR #243 review, blocking 2).
///
/// Tied here rather than left to the two constants agreeing by inspection, because they are
/// derived in different places and only their *product* is the invariant. It is checked by the
/// **image** build — `cargo xtask test` does not compile this binary.
const _: () = assert!(MAX_ENTRIES as u32 * ENTRY_W + INDICATOR_W <= SCREEN_W);

/// How many entries the bottom bar can show.
///
/// Bounded because the bar is: past this the row would overflow the screen and later entries
/// would be laid out off it, which is a window you cannot get back rather than a cosmetic
/// problem. Entries past the limit are simply not shown — the window is still there, still
/// raisable by clicking it.
const MAX_ENTRIES: usize = ((SCREEN_W - INDICATOR_W) / ENTRY_W) as usize;

/// The label an entry shows: its title, marked with what the shell knows about it.
///
/// **Marked rather than styled**, for now. The toolkit can colour a row, but the milestone that
/// makes the shell look like anything is M11 — and a marker is legible in the serial log, which
/// is where every gate reads this from. `desktop-shell.md` §2 describes the bar's appearance;
/// this is the behaviour under it.
fn entry_label(e: &WinEntry) -> alloc::string::String {
    let mut s = alloc::string::String::new();
    s.push_str(if e.minimized {
        "_ "
    } else if e.focused {
        "> "
    } else {
        "  "
    });
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
fn visible_entries(entries: &[WinEntry], current: u32) -> alloc::vec::Vec<&WinEntry> {
    entries.iter().filter(|e| e.desktop == current).take(MAX_ENTRIES).collect()
}

/// One taskbar entry: a bordered button, marked when its window holds the keyboard.
///
/// **A box rather than a run of text** (M11 Part E batch 8). The entries were labels on a flat
/// bar, so two windows read as one line with a gap in it — the reference desktop draws each as a
/// button, and the border is what says where one ends and the next begins.
///
/// **The focused one is filled, the rest are the bar's own face.** The list already marks focus
/// with a leading glyph; a filled face says it at a glance, and the two agree because they are
/// built from the same flag.
fn entry_cell(e: &WinEntry, theme: &Theme) -> Element<()> {
    let face = if e.focused { theme.face_hover } else { theme.face };
    stack(alloc::vec![
        fill(theme.border),
        padding(Insets::all(1), bevel(face)),
        padding(Insets { top: 3, right: 7, bottom: 3, left: 7 }, text(entry_label(e))),
    ])
}

/// The bottom bar's element tree: one button per window, then the desktop indicator.
fn window_bar_view<'a>(shown: &[&'a WinEntry], label: &str, theme: &Theme) -> Element<()> {
    let mut cells: alloc::vec::Vec<Element<()>> = alloc::vec::Vec::new();
    for e in shown {
        cells.push(sized(libdraw::geom::Size::new(ENTRY_W, 0), entry_cell(e, theme)));
    }
    if cells.is_empty() {
        // An empty row lays out to nothing and commits a blank bar, which reads as a broken
        // bar rather than an empty one.
        cells.push(sized(
            libdraw::geom::Size::new(ENTRY_W, 0),
            padding(Insets { top: 4, right: 8, bottom: 4, left: 8 }, text("No Windows")),
        ));
    }
    // **A flexible gap, so the indicator is drawn where the hit-test looks for it.** `row`
    // packs from the left; without this the indicator follows the last entry and moves every
    // time a window opens or closes.
    cells.push(sized(libdraw::geom::Size::new(0, 0), text("")).flex(1));
    // **The indicator, at the end of the bar** (`desktop-shell.md` §7) — a compact readout of
    // the current desktop rather than GNOME 2's switcher, which with *dynamic* desktops is a
    // list that changes length and would be the churniest widget here.
    cells.push(sized(
        libdraw::geom::Size::new(INDICATOR_W, 0),
        padding(Insets { top: 4, right: 8, bottom: 4, left: 8 }, text(label)),
    ));
    row(cells)
}

/// Render the bottom bar for the windows on `current`, with the desktop indicator.
fn render_window_bar(
    theme: &Theme,
    font: &Font,
    shown: &[&WinEntry],
    label: &str,
) -> MemFramebuffer {
    let geometry = Geometry::with_pitch(SCREEN_W, BAR_H, BAR_PITCH, PixelFormat::XRGB8888)
        .unwrap_or_else(|| fail(b"desktop-shell: bad bottom bar geometry\n"));
    let mut fb = MemFramebuffer::new(geometry);
    let ui = window_bar_view(shown, label, theme);
    let bounds = Rect::new(0, 0, SCREEN_W, BAR_H);
    let metrics = FontMetrics::new(font, theme.font_px);
    let l = layout(&ui, bounds, &metrics);
    // The session's theme, read once in `_start` — the shell's own chrome follows the file
    // it hands to every application, or it themes the windows and not the bars around them.
    //
    // **On the panel face, not on a window's ground** (M11 Part E, batch 1). `paint` clears a
    // damage rectangle to `background`, which since the theme turned light is the white an
    // application draws on — and a bar is not paper: it is a face, the surface a button and a
    // toolbar are made of. One substituted field rather than a new one; a panel wanting a colour
    // of its own needs more evidence than one screenshot.
    paint(&mut fb, font, &panel(theme), &ui, &l, bounds, &mut |_, _, _, _: &mut MemFramebuffer| {
    });
    fb
}

/// The applications modal's size.
const MODAL_W: u32 = 320;
/// See [`MODAL_W`].
const MODAL_H: u32 = 240;
/// Bytes per row in the modal.
const MODAL_PITCH: usize = (MODAL_W as usize) * 4;
/// How tall one entry is.
const ROW_H: u32 = 20;

/// What the applications modal can ask for.
///
/// **One variant, and it is still worth a type.** `Element<()>` was honest while nothing could be
/// clicked; a message with a payload is what carries *which* row, and the unit type cannot.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ModalMsg {
    /// Launch the program keyed by this index into the unfiltered program list.
    Launch(u64),
    /// The scrollbar is being dragged.
    ///
    /// **Which is why the modal's list state had to stop being a throwaway.** It was built fresh
    /// in `render_modal` on the argument that the launcher keeps no selection — true, and it also
    /// meant an offset that reset to zero every frame, so `/bin` was 26 entries of which ten were
    /// reachable and the rest only by typing a filter (M11 Part E batch 6).
    Scroll(librsproto::surface::PointerEvent),
}

/// The height the modal's list is laid out at — the space left after the filter field.
///
/// **One place, because two things have to agree about it** (PR #265 review, optional 10): the
/// view lays the list out at this height, and `ListState::drag_to` converts a pointer's y against
/// it. Open-coded at both, changing one would leave the thumb tracking a track that is not the
/// one drawn — which is the failure `drag_to`'s own doc names. `nxfiles` routes both through
/// `App::list_h`, and this is that shape.
fn modal_list_h() -> u32 {
    MODAL_H.saturating_sub(40)
}

/// The applications modal's element tree: a filter field over a list of `/bin` programs.
///
/// **The theme is passed in rather than built here**, and that is not tidiness. This function
/// builds widgets while its caller *paints* them, and two themes in one frame is a thing the old
/// `Theme`/`Palette` split made unwriteable and one type makes easy (PR #262 review, optional 5).
///
/// **The same mistake arrived a second time through the metrics**, which is worth stating here
/// because this is where it was first argued: M11 Part C took the theme from a file and left
/// every `layout()` measuring at a hardcoded 16, so text was laid out at one size and painted at
/// another — clipped and overlapping for every size but the default. There is no size constant
/// in this crate now; there is a theme, and both the measure and the paint come from it
/// (PR #263 review, blocking 1).
fn modal_view(
    query: &TextFieldState,
    rows: &[ListRow<'_>],
    state: &mut ListState,
    hovered: Option<u64>,
    theme: &Theme,
) -> Element<ModalMsg> {
    let field = text_field(query, false, WidgetState { active: true, ..Default::default() }, theme);
    let list_h = modal_list_h();
    // **Rows are clickable and they highlight** (M11 Part E batch 4). Until then this shell
    // looked at pointer events for exactly three things — the overview's thumbnails, the
    // applications button and the taskbar — and never at the modal's own window, so its rows
    // could not be clicked at all and nothing under the cursor reacted. Both are the same gap:
    // no router. The key is the row's index into the *unfiltered* list, which is what makes
    // `ModalMsg::Launch` resolvable after a filter has reordered what is shown.
    let list = list_view(
        rows,
        state,
        list_h,
        ROW_H,
        ModalMsg::Launch,
        None,
        Some(ModalMsg::Scroll),
        hovered,
        theme,
    );
    // **Framed, because a popup is the one surface with nothing behind it to define its edge**
    // (M11 Part E, batch 2). On a light theme this modal's face and the window it covers are
    // within a few units of each other, so without a line around it the two run together. Same
    // helper `nxterm`'s menu uses — they are the same kind of thing seen twice.
    popup_frame(
        padding(
            Insets::all(8),
            column(alloc::vec![field, sized(libdraw::geom::Size::new(0, list_h), list)]),
        ),
        theme,
    )
}

/// Render the modal.
fn render_modal(
    theme: &Theme,
    font: &Font,
    query: &TextFieldState,
    rows: &[ListRow<'_>],
    state: &mut ListState,
    tree: &mut Tree,
    hovered: Option<u64>,
) -> MemFramebuffer {
    let geometry = Geometry::with_pitch(MODAL_W, MODAL_H, MODAL_PITCH, PixelFormat::XRGB8888)
        .expect("the modal pitch is wide enough for a row");
    let mut fb = MemFramebuffer::new(geometry);
    // **Built once and used for both**, which is the whole of optional 5: this function lays the
    // tree out and paints it, and a tree built from one theme painted with another is two themes
    // in one frame.
    // The session's theme, read once in `_start` — the shell's own chrome follows the file
    // it hands to every application, or it themes the windows and not the bars around them.
    let ui = modal_view(query, rows, state, hovered, theme);
    let bounds = Rect::new(0, 0, MODAL_W, MODAL_H);
    let metrics = FontMetrics::new(font, theme.font_px);
    let l = layout(&ui, bounds, &metrics);
    // **The tree records what was painted**, which is what makes a click land on the row a
    // person can see: the router hit-tests the retained tree, so a tree from a different frame
    // is a hit test against a picture nobody is looking at. Updated here rather than at the
    // call sites, so it cannot be forgotten at one of them.
    let _ = tree.update(&ui, &l);
    paint(&mut fb, font, theme, &ui, &l, bounds, &mut |_, _, _, _: &mut MemFramebuffer| {});
    fb
}

/// The entries matching `q`, in order. An empty query matches everything.
///
/// Substring rather than prefix: a launcher that only matched from the start would make
/// "term" fail to find `nxterm`, which is the one thing anybody will type.
fn filter<'a>(apps: &'a [Application], q: &str) -> alloc::vec::Vec<&'a Application> {
    apps.iter().filter(|a| matches_app(a, q)).collect()
}

/// Whether `app` is shown for query `q` — matched against **both** the display name and the
/// program.
///
/// **Both, because both are things a person types.** Somebody who knows the desktop types
/// "editor"; somebody who knows the system types `nxedit`. Matching only the name would make the
/// second fail, and this system's users are more likely than most to be the second kind.
fn matches_app(app: &Application, q: &str) -> bool {
    matches(&app.name, q) || matches(&app.exec, q)
}

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

/// Whether one string is shown for query `q`. Case-insensitive on ASCII, because a display name
/// is capitalised ("Files") and nobody types the capital.
fn matches(name: &str, q: &str) -> bool {
    if q.is_empty() {
        return true;
    }
    let (n, q) = (name.to_ascii_lowercase(), q.to_ascii_lowercase());
    n.contains(&q)
}

/// The modal's rows: what `q` matches, **keyed by index into the unfiltered list**.
///
/// **One builder, because two of them disagreed.** `open_modal` keyed by the unfiltered index —
/// with a comment explaining that the filtered index would pair row 2's widget with row 3's
/// element the moment a character is typed — and the repaint site keyed by the filtered one. The
/// two produced different keys for the same row as soon as the query was non-empty, which
/// nothing noticed while a key was only ever used for diffing a modal that is repainted whole.
/// A click resolves a key back to a program, so it notices now (M11 Part E batch 4).
fn modal_rows<'a>(apps: &'a [Application], q: &str) -> alloc::vec::Vec<ListRow<'a>> {
    apps.iter()
        .enumerate()
        .filter(|(_, a)| matches_app(a, q))
        .map(|(i, a)| ListRow { key: i as u64, label: a.name.as_str() })
        .collect()
}

/// Read the desktop entries `/applications` projects, as the modal's entries.
///
/// **`/applications` is a forwarded directory, not a set of bindings**, so `SYS_NS_ENUMERATE`
/// does not see inside it — that walks the namespace's own bindings and this is one of them. The
/// names come from a directory session, the same way `list /bin` gets `/bin`'s; each is then read
/// and parsed.
///
/// **An entry that will not parse is skipped and said so**, rather than failing the modal: one
/// broken package should lose its own applications, not everybody's — the same rule the profile
/// server applies to a package whose `bin/` will not open.
fn read_applications(ns: u64) -> alloc::vec::Vec<Application> {
    use librsproto::session::Dir;
    let mut names = alloc::vec::Vec::new();
    let mut buf = [0u8; 4096];
    let Ok(mut dir) = Dir::open(ns, b"/applications", &mut buf) else {
        kprint(b"desktop-shell: /applications did not open; the modal will be empty\n");
        return alloc::vec::Vec::new();
    };
    let _ = dir.read_dir(|e| {
        if e.name != b"." && e.name != b".." {
            names.push(alloc::string::String::from_utf8_lossy(e.name).into_owned());
        }
        true
    });
    dir.close();
    names.sort();

    let mut apps = alloc::vec::Vec::new();
    for file in &names {
        let mut path = alloc::string::String::from("/applications/");
        path.push_str(file);
        let Ok(bytes) = libfs::read_file(ns, path.as_bytes()) else {
            Line::new().s(b"desktop-shell: application entry unreadable: ").untrusted(file.as_bytes()).end();
            continue;
        };
        match core::str::from_utf8(&bytes).ok().and_then(parse_entry) {
            Some(a) => apps.push(a),
            None => {
                Line::new().s(b"desktop-shell: application entry malformed: ").untrusted(file.as_bytes()).end();
            }
        }
    }
    apps.sort_by(|a, b| a.name.cmp(&b.name));
    apps
}

/// Parse a desktop entry: `name` and `exec`, both required.
///
/// The same shape `Theme`'s reader uses — `key = "value"` a line at a time, `#` a comment —
/// rather than a TOML library, because this is two keys and the system has no TOML crate.
/// **Both required**: an entry with no `exec` names nothing to launch, and one with no `name`
/// would fall back to the binary's, which is the thing this part exists to stop showing.
fn parse_entry(text: &str) -> Option<Application> {
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

/// `theme`, as the overview's sidebar wears it: the desktop's own ground, darkened, with the
/// window ground as ink.
///
/// **Not a colour of its own.** Everything here is derived from two the theme already has, so a
/// new palette needs no extra decision — and the sidebar stays related to the desktop it sits
/// over rather than being a third surface.
fn sidebar(theme: &Theme) -> Theme {
    Theme {
        background: theme.desktop.shade(-24),
        foreground: theme.background,
        ..*theme
    }
}

/// `theme`, with a bar's ground in place of a window's.
///
/// One place, so the two bars cannot disagree about what a panel is made of.
fn panel(theme: &Theme) -> Theme {
    Theme { background: theme.face, ..*theme }
}

/// Render the top bar.
fn render_bar(theme: &Theme, font: &Font, clock: &str) -> MemFramebuffer {
    let geometry = Geometry::with_pitch(SCREEN_W, BAR_H, BAR_PITCH, PixelFormat::XRGB8888)
        .expect("the bar pitch is wide enough for a row");
    let mut fb = MemFramebuffer::new(geometry);
    let ui = bar_view(clock);
    let bounds = Rect::new(0, 0, SCREEN_W, BAR_H);
    let metrics = FontMetrics::new(font, theme.font_px);
    let l = layout(&ui, bounds, &metrics);
    // The session's theme, read once in `_start` — the shell's own chrome follows the file
    // it hands to every application, or it themes the windows and not the bars around them.
    //
    // **On the panel face, not on a window's ground** (M11 Part E, batch 1). `paint` clears a
    // damage rectangle to `background`, which since the theme turned light is the white an
    // application draws on — and a bar is not paper: it is a face, the surface a button and a
    // toolbar are made of. One substituted field rather than a new one; a panel wanting a colour
    // of its own needs more evidence than one screenshot.
    paint(&mut fb, font, &panel(theme), &ui, &l, bounds, &mut |_, _, _, _: &mut MemFramebuffer| {
    });
    fb
}

/// Unmap a buffer this process mapped with [`shared_buffer`], and forget the pointer.
///
/// **The shell had no unmap call at all until M8 Part E's review.** `shared_buffer` maps
/// outside the heap, so nothing reclaims it on drop: every overview open leaked two 4 MB
/// mappings and up to six thumbnails, ~9 MB a cycle against a 256 MB guest — about
/// twenty-eight opens to exhaust the machine, and the landing spot for that is a `create` that
/// fails after the window exists (PR #244 review, finding 4). The modal has the same shape at
/// 614 KB, which is why it had not bitten; it is fixed here too rather than left as the next
/// instance of the same bug.
fn release_buffer(addr: &mut *mut u8, len: usize) {
    if addr.is_null() {
        return;
    }
    // SAFETY: unmapping exactly what `shared_buffer` mapped, at the same length. The object
    // itself goes when its last reference does — the handle was moved away at `attach`.
    unsafe { syscall4(SYS_MEMORY_UNMAP, *addr as u64, len as u64, 0, 0) };
    *addr = core::ptr::null_mut();
}

/// Allocate a shared memory object of `len` bytes and map it writable.
fn shared_buffer(len: usize) -> Option<(u64, *mut u8)> {
    // SAFETY: a plain anonymous object of `len` bytes.
    let h = unsafe { syscall4(SYS_MEMORY_CREATE, len as u64, 0, 0, 0) };
    if h <= 0 {
        return None;
    }
    // SAFETY: maps the object read/write at a kernel-chosen address.
    let base = unsafe {
        syscall4(SYS_MEMORY_MAP, h as u64, 0, len as u64, RIGHT_MAP_READ | RIGHT_MAP_WRITE)
    };
    if base < 0 {
        // SAFETY: closing our own handle.
        unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
        return None;
    }
    Some((h as u64, base as usize as *mut u8))
}

/// Construct the namespace one application runs in, and return its handle.
///
/// **The load-bearing part of the shell**, and the reason it holds `BIND_NAMESPACE` at all.
/// `ui-composition-model.md` §5a rests the guarantee that *an application cannot compose other
/// applications* on the shell being the process that built their namespaces — so authority is
/// what the shell binds, not what an application asks for.
///
/// **`/dev/draw/new` is bound as its own path, with subtree base `/new`** — not the
/// `/dev/draw` subtree. That single choice is what closed the `manage-ungated` deferral, and it needed
/// no protocol change and no second endpoint:
///
/// - Resolving `/dev/draw/new` is an **exact match** against the binding, so the forwarded
///   suffix is empty, the base supplies the whole of it, and the compositor classifies `new`
///   and mints a session.
/// - Resolving `/dev/draw/manage` is **not a component-boundary prefix match** against a
///   binding of `/dev/draw/new` (`kernel/src/object/namespace.rs`, `match_suffix_offset`), so
///   nothing in this namespace answers it.
///
/// A first draft of this milestone specified a second forwarding endpoint for management,
/// couriered `init` → `service-mgr` → `desktop-session-mgr`, reasoning that the compositor
/// classifies by suffix with no caller identity so binding could not distinguish. Both
/// premises are true and the conclusion does not follow: what a namespace can *reach* is
/// decided by what it **binds**, not by how the server on the far side dispatches
/// (PR #225 review, finding 1).
///
/// **The caveat, because it decides how long this lasts.** A narrow bind expresses "`new` and
/// not `manage`". It cannot express "the `/dev/draw` subtree *minus* `manage`" — so the first
/// application that needs `/dev/draw/<id>/info` for ids it does not know in advance forces a
/// subtree bind, and `manage` comes back with it. Today nothing in `libsurface`, `libui`,
/// `libdraw` or `nxterm` resolves anything but `new`. The second endpoint is the fallback and
/// that is its trigger.
#[allow(clippy::too_many_arguments)]
fn build_app_namespace(
    draw: u64,
    fs: u64,
    tty: u64,
    profile: u64,
    home: &str,
    desktop: u64,
    clipboard: u64,
) -> u64 {
    let ns = unsafe { syscall0(SYS_NS_CREATE) };
    if ns < 0 {
        kprint(b"desktop-shell: application ns_create FAIL\n");
        return 0;
    }
    let ns = ns as u64;

    // `/dev/draw/new`, narrow. See this function's doc for why the base is `/new`.
    let path = b"/dev/draw/new";
    let base = b"/new";
    // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
    let dr = unsafe {
        syscall6(
            SYS_NS_BIND,
            ns,
            path.as_ptr() as u64,
            path.len() as u64,
            draw,
            base.as_ptr() as u64,
            base.len() as u64,
        )
    };
    if dr != 0 {
        kprint(b"desktop-shell: application /dev/draw/new bind FAIL\n");
        // SAFETY: closing the namespace we just created.
        unsafe { syscall1(SYS_HANDLE_CLOSE, ns) };
        return 0;
    }

    // **No `/applications`, deliberately** — `TODO(admin-visibility)`. The *session* namespace has
    // it and this one does not, so `nxsh` on the serial console can list the installed
    // applications and the same `nxsh` inside `nxterm` cannot. Nothing in an application reads it,
    // and an application holds no authority to spawn in the first place — `Desktop::Open` exists
    // because of that — so the binding would be a hole in a sandbox with nothing on the other side
    // of it. The asymmetry is a symptom of a larger gap (there is no account that sees more than
    // its own corner of the system); see `deferred-decisions.md`.
    //
    // `/system/fonts`, read-only, so an application can render text. The same subtree bind the
    // session itself gets — an application that could not draw text would be a window of
    // rectangles.
    if fs != 0 {
        let fpath = b"/system/fonts";
        // SAFETY: valid namespace handle, path pointer, endpoint handle and subtree base.
        let fr = unsafe {
            syscall6(
                SYS_NS_BIND,
                ns,
                fpath.as_ptr() as u64,
                fpath.len() as u64,
                fs,
                fpath.as_ptr() as u64,
                fpath.len() as u64,
            )
        };
        if fr != 0 {
            kprint(b"desktop-shell: application /system/fonts bind FAIL\n");
        }
    }

    // **`/dev/tty`, which is `graphical-session.md` §6.1's first shape and Part F's answer.**
    //
    // The path names the tty *server*, not a device: each resolve mints a **fresh terminal**
    // (`tty-server`'s `open_tty`), exactly as `/dev/draw/new` mints a compositor session per
    // caller. So two terminal emulators in two namespaces each get their own, attach their own
    // backend, and share nothing — the binding does not need to be per-window because the
    // minting already is.
    //
    // §6.1's *second* shape — absent from application namespaces, the terminal handed down —
    // stays true one level in: `nxterm` resolves this to obtain a terminal, then hands **that
    // handle** to the `nxsh` it hosts, because a binding cannot name a particular window. The
    // emulator does not need to name one; it makes one.
    if tty != 0 {
        let tpath = b"/dev/tty";
        // SAFETY: valid namespace handle, path pointer and endpoint handle.
        let tr = unsafe {
            syscall4(SYS_NS_BIND, ns, tpath.as_ptr() as u64, tpath.len() as u64, tty)
        };
        if tr != 0 {
            kprint(b"desktop-shell: application /dev/tty bind FAIL\n");
        }
    }

    // **`/bin`, because a terminal has to be able to host a shell.** `nxterm` spawns `nxsh`,
    // and without this it launches, finds a font and a terminal, and then reports
    // `/bin/nxsh not found` — a window that opens and immediately has nothing in it.
    //
    // **What an application namespace holds is `/dev/draw/new`, `/system/fonts`, `/dev/tty`,
    // `/bin` and the user's `/home`** — the session's members less the manager channel and
    // less `/session/user`, and with `/dev/draw` narrowed to `/new` rather than the session's
    // whole subtree. That narrowing is what M7 is about. *Which* applications get which of
    // these is a per-application policy and a later question: there is no manifest to read it
    // from, and inventing one here would be guessing at what `ui-composition-model.md` wants
    // before anything asks.
    if profile != 0 {
        let bpath = b"/bin";
        // SAFETY: valid namespace handle, path pointer and endpoint handle.
        let br = unsafe {
            syscall4(SYS_NS_BIND, ns, bpath.as_ptr() as u64, bpath.len() as u64, profile)
        };
        if br != 0 {
            kprint(b"desktop-shell: application /bin bind FAIL\n");
        }
    }

    // **`/dev/desktop`, and binding it is a capability decision** — every application in this
    // session can then create, switch and name desktops, which is strictly more than one has
    // otherwise, since it cannot even raise its own window. Granted deliberately for v1: the
    // narrow-bind that withholds `/dev/draw/manage` while granting `new` is available here too,
    // but withholding mutation would leave `desktop switch` with no way to work, and a binding
    // whose only consumer is disarmed is the shape the `desktop-endpoint` deferral existed to refuse.
    //
    // **Bound here rather than into the session namespace.** The session namespace is the
    // shell's own and nothing else runs in it, so a binding there would have no consumer at
    // all — while a `/bin` command runs under the `nxsh` a terminal spawned, whose namespace is
    // this one (PR #239 review, finding 1).
    if desktop != 0 {
        let dpath = b"/dev/desktop";
        // SAFETY: valid namespace handle, path pointer and endpoint handle.
        let dr = unsafe {
            syscall4(SYS_NS_BIND, ns, dpath.as_ptr() as u64, dpath.len() as u64, desktop)
        };
        if dr != 0 {
            kprint(b"desktop-shell: application /dev/desktop bind FAIL\n");
        } else {
            kprint(b"desktop-shell: application /dev/desktop bound\n");
        }
    }

    // **`/dev/clipboard`, so applications can copy and paste** (M12 Part E). It is bound here
    // for `/dev/desktop`'s reason: the session namespace is the shell's own and nothing else
    // runs in it, so a binding there alone would have no consumer — while the editor, the
    // browser, the terminal and any `clip` a pipeline runs all live in namespaces this
    // function builds.
    //
    // **And granting it is a capability decision, exactly as `/dev/desktop` is.** Everything
    // in this session can then read what anything else copied. That is M12 decision 1's
    // accepted position — the binding is the authority, and the trigger for narrowing it is an
    // application inside a session that the person does not trust, which is the day profiles
    // stop being a build-time idea. The mechanism for narrowing needs no protocol change: an
    // endpoint attenuated to `RIGHT_SEND` before it reaches here is an application that can
    // copy and not read.
    if clipboard != 0 {
        let cpath = b"/dev/clipboard";
        // SAFETY: valid namespace handle, path pointer and endpoint handle.
        let cr = unsafe {
            syscall4(SYS_NS_BIND, ns, cpath.as_ptr() as u64, cpath.len() as u64, clipboard)
        };
        if cr != 0 {
            kprint(b"desktop-shell: application /dev/clipboard bind FAIL\n");
        }
    }

    // **`/home`, scoped to the user's subtree — because otherwise the environment lies.**
    // `session_env()` sets `HOME` and `PWD` to `/home`, and `launch` forwards that record
    // unchanged, so a terminal opened here started its `nxsh` with `PWD=/home` in a namespace
    // where `/home` resolved to nothing. `nxsh` resolves every relative path against `PWD`, so
    // `list .`, `cd`, and `open ./x` all failed in the graphical column while passing in the
    // serial one — and no gate saw it, because the grid renders only under `test-harness`
    // (PR #238 review, finding 3).
    //
    // The six-argument bind, with `home` as the subtree base: the same shape
    // `libsession::build_namespace` uses, so an application sees exactly the user's home and
    // not the `/home` above it. Binding the fs endpoint whole-tree here would hand every
    // application every user's files, which is the opposite of what this function is for.
    if fs != 0 && !home.is_empty() {
        let hpath = b"/home";
        // SAFETY: valid namespace handle, path and base pointers, and endpoint handle.
        let hr = unsafe {
            syscall6(
                SYS_NS_BIND,
                ns,
                hpath.as_ptr() as u64,
                hpath.len() as u64,
                fs,
                home.as_ptr() as u64,
                home.len() as u64,
            )
        };
        if hr != 0 {
            kprint(b"desktop-shell: application /home subtree bind FAIL\n");
        }
    }
    ns
}

/// Check the application namespace grants `new` and withholds `manage`, before anything runs
/// in it.
///
/// **Verified rather than assumed, and by the process that built it.** The narrow bind is the
/// whole of the `manage-ungated` deferral's answer, and it rests on a kernel matching rule
/// (`match_suffix_offset`) that this file does not own. A shell that constructed the namespace
/// wrongly and launched into it anyway would hand an application the manager channel — the
/// exact thing the deferral is about — and nothing downstream would notice, because an
/// application that *can* reach `manage` simply never says so.
///
/// Returns `false` if the namespace is not what it should be; the caller declines to launch.
fn verify_app_namespace(ns: u64, expect_home: bool, expect_desktop: bool) -> bool {
    let (new_st, new_h) = ns_lookup(ns, b"/dev/draw/new", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if new_h != 0 {
        // SAFETY: closing a session this check minted; the application will make its own.
        unsafe { syscall1(SYS_HANDLE_CLOSE, new_h) };
    }
    let (manage_st, manage_h) =
        ns_lookup(ns, b"/dev/draw/manage", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if manage_h != 0 {
        // SAFETY: closing a handle this check should never have obtained.
        unsafe { syscall1(SYS_HANDLE_CLOSE, manage_h) };
    }
    if new_st != 0 || new_h == 0 {
        kprint(b"desktop-shell: application namespace cannot reach /dev/draw/new\n");
        return false;
    }
    // **A refusal is not the same as an absence, and treating them alike made this check
    // pass for the exact mis-construction it exists to catch.**
    //
    // Once this shell holds the manager channel, the compositor answers a *second* `manage`
    // resolve with `WouldBlock` — the first-come rule, nothing to do with whether the path is
    // bound. So a namespace that wrongly bound the whole `/dev/draw` subtree looked identical
    // to one that bound `new` alone, and the launch-time check announced "withholds manage"
    // while handing an application the subtree. Demonstrated in review by widening the
    // namespace on the launch path only: the gate went green (PR #237 review, finding 3).
    //
    // `WouldBlock` means the resolve **reached the compositor**, which is precisely what must
    // not happen. Only `NotFound` — nothing in this namespace answers that path — is the
    // property being checked.
    if manage_st != KError::NotFound.as_i32() {
        Line::new()
            .s(b"desktop-shell: application namespace can REACH /dev/draw/manage (status ")
            .i(manage_st as i64)
            .s(b") -- refusing")
            .end();
        return false;
    }
    // **`/home` too, because the environment names it.** An application starts with
    // `PWD=/home`, so a namespace where that does not resolve gives a shell whose every
    // relative path fails — and nothing downstream reports it, since a terminal's output goes
    // to the grid and the grid renders only under `test-harness`. Checked by the process that
    // built the namespace, for the same reason `manage` is (PR #238 review, finding 3).
    if expect_home {
        let (home_st, home_h) = ns_lookup(ns, b"/home", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
        if home_h != 0 {
            // SAFETY: closing a session this check minted; the application will make its own.
            unsafe { syscall1(SYS_HANDLE_CLOSE, home_h) };
        }
        if home_st != 0 || home_h == 0 {
            Line::new()
                .s(b"desktop-shell: application namespace cannot reach /home (status ")
                .i(home_st as i64)
                .s(b") -- refusing")
                .end();
            return false;
        }
    }
    // **`/dev/desktop` is checked by *enumerating* the namespace, not by resolving it.**
    //
    // The shell serves that path, and a resolve is forwarded to whoever serves it — so asking
    // the kernel to resolve its own endpoint blocks this process inside `ns_lookup` waiting for
    // an answer only it could send. That is real, and it is the same self-deadlock the bottom
    // bar hit in Part C by another route.
    //
    // **But "cannot resolve it" is not "cannot check it", which is where the first version of
    // this stopped.** `SYS_NS_ENUMERATE` walks the caller's *own* namespace and copies out one
    // entry per call: local, no IPC, nothing forwarded — `nxsh`'s `binding_at_or_under` uses the
    // same idiom. So the binding gets a check on the same footing as `new` granted and `manage`
    // withheld, rather than the bind call's return value standing in for one, which was a proxy
    // where a real check was available (PR #245 review, finding 3).
    if expect_desktop && !bound_in(ns, "/dev/desktop") {
        kprint(b"desktop-shell: application namespace has no /dev/desktop -- refusing\n");
        return false;
    }
    kprint(b"desktop-shell: application namespace grants new + /home, withholds manage\n");
    true
}

/// Whether `path` is bound in `ns` — asked of the namespace itself, not of whoever serves it.
///
/// **Local and cheap**: the kernel walks the caller's own namespace and copies out one entry per
/// call, with no forwarding anywhere in it. That is what makes it usable on a path this process
/// *serves*, where a resolve would deadlock.
fn bound_in(ns: u64, path: &str) -> bool {
    let mut entry = libkern::abi::NsEntry::zeroed();
    for index in 0u64.. {
        // SAFETY: `entry` is a valid writable out-param of exactly `NsEntry`'s layout.
        let r =
            unsafe { syscall3(SYS_NS_ENUMERATE, ns, index, (&raw mut entry) as *mut _ as u64) };
        if r != 0 {
            return false; // NotFound ends the walk
        }
        let len = (entry.path_len as usize).min(libkern::abi::NS_ENTRY_PATH_MAX);
        // SAFETY: the kernel wrote the binding path; `len` is clamped to the buffer.
        let bound =
            unsafe { core::slice::from_raw_parts((&raw const entry.path) as *const u8, len) };
        if bound == path.as_bytes() {
            return true;
        }
    }
    false
}

/// Resolve `path` in `ns`, returning `(status, handle)`.
///
/// **Async, like every potentially-blocking syscall here**: `SYS_NS_LOOKUP` returns a
/// `PendingOperation` to wait on, and the status and handle are read out of the wait result —
/// not an out-param. A first version of this wrote it synchronously and every resolve
/// "failed", which is what a `PendingOperation` handle looks like when you read it as a
/// status.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe {
        syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights)
    };
    if po < 0 {
        return (po as i32, 0);
    }
    if !wait_one(po as u64) {
        // SAFETY: closing our own PO.
        unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
        return (-1, 0);
    }
    // SAFETY: the kernel wrote one 24-byte IoResult: status at 8, handle at 16.
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([
                WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11],
            ]),
            u64::from_le_bytes([
                WAIT_RESULTS[16], WAIT_RESULTS[17], WAIT_RESULTS[18], WAIT_RESULTS[19],
                WAIT_RESULTS[20], WAIT_RESULTS[21], WAIT_RESULTS[22], WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    (status, if status == 0 { handle } else { 0 })
}

/// Receive and discard one message, waiting for it. `false` if the wait or receive failed.
fn recv_message(ch: u64) -> bool {
    if !wait_one(ch) {
        return false;
    }
    // SAFETY: valid recv out-params.
    let r = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut RECV_MSG) as u64,
            (&raw mut RECV_HANDLES) as u64,
            (&raw mut RECV_COUNT) as u64,
        )
    };
    r == 0
}

/// Receive one message and return its first transferred handle, or `0`.
fn recv_handle(ch: u64) -> u64 {
    if !recv_message(ch) {
        return 0;
    }
    // SAFETY: the kernel wrote `RECV_COUNT` transferred handles into `RECV_HANDLES`.
    unsafe {
        if RECV_COUNT == 0 { 0 } else { RECV_HANDLES[0] }
    }
}

/// Block until `h` is signalled.
fn wait_one(h: u64) -> bool {
    // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers, and `WAIT_RESULTS` holds one
    // `IoResult` per handle in the set — see its declaration for why that is the requirement.
    let waited = unsafe {
        WAIT_HANDLES[0] = h;
        syscall4(SYS_WAIT, (&raw const WAIT_HANDLES) as u64, 1, (&raw mut WAIT_RESULTS) as u64, u64::MAX)
    };
    waited == 1
}

/// Recv buffers for the setup channel.
static mut RECV_MSG: [u8; 4096] = [0; 4096];
/// See [`RECV_MSG`].
static mut RECV_HANDLES: [u64; 8] = [0; 8];
/// See [`RECV_MSG`].
static mut RECV_COUNT: usize = 0;

/// Spawn args for a launched application: the namespace this shell constructed, and **no
/// syscaps at all**.
///
/// An application constructs nothing and registers nothing. Whatever it can reach was bound
/// into its namespace by this process — `ui-composition-model.md` §5a's guarantee that an
/// application cannot compose other applications is exactly this line plus
/// [`build_app_namespace`].
static mut SPAWN_APP: SpawnArgs = SpawnArgs {
    image: 0,
    // One handle: the setup channel. An application is a **Tier-1** stage — it receives its
    // `argv` and its environment the way every pipeline stage does, rather than through a
    // special case.
    handle_count: 1,
    move_mask: 1,
    arg0: 0,
    handles: [0; 4],
    rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT, 0, 0, 0],
    namespace: 0, // set per launch = the namespace built for it
    syscaps: 0,   // empty, and it stays empty
};

/// Everything a launch needs, gathered once.
///
/// **A struct because there is a second caller now.** The applications modal was the only one
/// until M10 Part D added `Desktop::Open` — a client asking the shell to open a path — and the
/// nine values a launch needs were about to be threaded through the desktop session handler as
/// nine more parameters. What they actually are is one thing: the authority this shell holds to
/// start an application.
struct Launcher<'a> {
    /// The session's namespace, which is where `/bin/<program>` is resolved from.
    session_ns: u64,
    /// The compositor endpoint every application is given.
    draw: u64,
    /// The filesystem, the terminal server and the profile server, bound the same way.
    fs: u64,
    /// See [`Launcher::fs`].
    tty: u64,
    /// See [`Launcher::fs`].
    profile: u64,
    /// This shell's own `/dev/desktop`, bound into what it builds.
    desktop: u64,
    /// The clipboard server, bound into what it builds — see [`build_app_namespace`].
    clipboard: u64,
    /// The user's home, bound as `/home` in an application's namespace.
    home: &'a str,
    /// The environment record an application reads its `HOME` from.
    env: &'a libstream::wire::Record,
    /// Whether a namespace this shell builds actually gates. False disables launching outright.
    enabled: bool,
}

impl Launcher<'_> {
    /// Launch `program` into a namespace built for it, with `args` after `argv[0]`.
    ///
    /// **The namespace is verified before anything runs in it**, and a shell that finds the gate
    /// open declines to launch. See [`verify_app_namespace`] for why that is behaviour rather
    /// than a test: an application that *can* reach `manage` never says so, and nothing
    /// downstream would notice.
    fn launch(&self, program: &str, args: &[&str]) -> bool {
        if !self.enabled {
            kprint(b"desktop-shell: launching is disabled; ignoring\n");
            return false;
        }
        launch(self, program, args)
    }
}

/// The body of [`Launcher::launch`], kept a free function so the long sequence of handle
/// bookkeeping reads as it did before the context was gathered.
fn launch(l: &Launcher<'_>, program: &str, args: &[&str]) -> bool {
    let (session_ns, draw, fs, tty, profile, desktop, clipboard, home, env) =
        (l.session_ns, l.draw, l.fs, l.tty, l.profile, l.desktop, l.clipboard, l.home, l.env);
    if draw == 0 {
        kprint(b"desktop-shell: no compositor endpoint; cannot launch\n");
        return false;
    }
    let app_ns = build_app_namespace(draw, fs, tty, profile, home, desktop, clipboard);
    if app_ns == 0 {
        return false;
    }
    if !verify_app_namespace(app_ns, !home.is_empty(), desktop != 0) {
        // SAFETY: closing the namespace; nothing was launched into it.
        unsafe { syscall1(SYS_HANDLE_CLOSE, app_ns) };
        kprint(b"desktop-shell: application namespace is not gated; refusing to launch\n");
        return false;
    }
    // The image comes from the **session's** `/bin`, not the application's: the shell resolves
    // what to run, and the namespace it built is what the program will run *in*.
    let mut path = alloc::string::String::from("/bin/");
    path.push_str(program);
    let (st, image) = ns_lookup(session_ns, path.as_bytes(), RIGHT_MAP_READ);
    if st != 0 || image == 0 {
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" not found in /bin").end();
        // SAFETY: closing the namespace we built for a launch that will not happen.
        unsafe { syscall1(SYS_HANDLE_CLOSE, app_ns) };
        return false;
    }
    // The setup channel this application will read its `argv` and environment from. Depth
    // one, because one message is what goes down it — sized from the payload, which is the
    // lesson `spawn_leader`'s `pipe(4)` taught when a fifth message was dropped silently.
    let Ok((setup_ours, setup_theirs)) = libstream::setup::pipe(1) else {
        kprint(b"desktop-shell: application setup channel FAIL\n");
        // SAFETY: closing handles for a launch that will not happen.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, image);
            syscall1(SYS_HANDLE_CLOSE, app_ns);
        }
        return false;
    };
    // SAFETY: SPAWN_APP is a valid writable arg block.
    let h = unsafe {
        SPAWN_APP.image = image;
        SPAWN_APP.namespace = app_ns;
        SPAWN_APP.handles[0] = setup_theirs;
        SPAWN_APP.arg0 = libstream::setup::bootstrap_arg0(true);
        syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_APP) as u64)
    };
    // The kernel copied the ELF during spawn, and the namespace is the child's now.
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, image);
        syscall1(SYS_HANDLE_CLOSE, app_ns);
    }
    if h < 0 {
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" spawn FAIL").end();
        // **Both ends, because nothing else will take them.** A spawn that fails leaves the
        // child's end still ours — `handles[0]` transfers on success only — so returning here
        // without closing leaks two handles per failed launch, in a process that lives for the
        // whole session and can be asked to launch again and again (PR #238 review, finding 7).
        // SAFETY: closing a setup channel pair for a process that does not exist.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, setup_ours);
            syscall1(SYS_HANDLE_CLOSE, setup_theirs);
        }
        return false;
    }
    // **Not reaped here.** This shell is not a supervisor of the applications it launches —
    // `desktop-session-mgr` reaps *it*, and an application's exit is the compositor noticing
    // its windows go away. Holding the process handle would make the shell responsible for a
    // lifecycle it has no opinion about.
    // SAFETY: closing the process handle; the child runs independently.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
    // `argv[0]` is the program name, as every Tier-1 stage's is, and `args` follows it — which
    // for an editor is the file it was asked to open. No streams: an application is not a
    // pipeline stage with a parent reading its output — a terminal makes its own.
    let mut argv: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(1 + args.len());
    argv.push(program);
    argv.extend_from_slice(args);
    let sent = libstream::setup::send_setup_env(
        setup_ours,
        &libstream::setup::Streams::default(),
        &argv,
        env,
    )
    .is_ok();
    // SAFETY: closing our end of the setup channel.
    unsafe { syscall1(SYS_HANDLE_CLOSE, setup_ours) };
    if !sent {
        // Reported rather than swallowed: the application is already running and will find no
        // setup message, which presents as a program with no environment rather than as a
        // failure to launch.
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" got no setup message").end();
    }
    Line::new().s(b"desktop-shell: launched ").s(program.as_bytes()).s(b" into its own namespace").end();
    true
}

/// The wallpaper, once it is on screen.
///
/// **The composed picture is kept**, which costs four megabytes for the machine's life and buys
/// the overview its ground: the overview is a full-screen opaque window, so without this it
/// covers the wallpaper with a flat colour and the desktop appears to lose its picture whenever
/// you look at the desktops (reported from a real session, 2026-09-02). The alternatives are
/// re-reading and re-decoding the file on every open — tens of milliseconds on a gesture that
/// should feel instant — or keeping the *decoded* image instead, which is twice the size.
struct Wallpaper {
    /// The window it lives in: named among the shell's own, and made sticky.
    window: u32,
    /// The screen-sized XRGB8888 composition, pitch `SCREEN_W * 4`. Ground, picture and
    /// letterbox, exactly as committed.
    picture: alloc::vec::Vec<u8>,
}

/// Put the theme's wallpaper on screen, if it names one.
///
/// **`Role::Panel` with a zero reservation, and that is the whole of "bottom-most and out of the
/// way".** It is the one role that cannot take focus, so a press on the picture raises nothing
/// and changes no focus — which is exactly what a press on bare desktop did before there was a
/// picture there. A press on it still *dismisses* an open popup, because the compositor's rule
/// is "anywhere that is not the popup" rather than "on a window" (M11 Part E batch 5), so the
/// applications menu goes on closing when you click the desktop. And `reserve: 0` means a
/// maximised window is still the work area: the wallpaper occupies the screen without claiming
/// any of it.
///
/// A new `Role::Background` was the alternative and is not taken: it would be a wire change, a
/// compositor stacking rule and a `check-display` reference, to express something three existing
/// properties already say.
///
/// **Two buffers, and the second is dead.** `libsurface` refuses fewer, for a reason that is
/// exactly right in general and does not apply here: a buffer is busy from commit until the
/// compositor releases it, and the compositor releases the one that *left* the screen — so a
/// single-buffered client can never draw a second frame. This window draws one frame ever and
/// never asks for another, but the library cannot tell that kind of client from a stalled one.
/// Four megabytes at 1280x800, on a 256 MB machine; a `create_static` that took the promise
/// instead of the count is where this would go if it ever mattered.
///
/// Every failure returns `None` after saying which one it was: the desktop falls back to its
/// ground colour, which is what a session with no wallpaper looks like anyway.
fn open_wallpaper(
    ns: u64,
    session: &mut Session<ChannelTransport>,
    theme: &Theme,
) -> Option<Wallpaper> {
    let path = theme.wallpaper.as_ref()?;
    let bytes = match libfs::read_file(ns, path.as_str().as_bytes()) {
        Ok(b) => b,
        Err(e) => {
            // **Absent and unreadable are different**, the same distinction `read_theme` makes:
            // a person who named a file that is sitting there needs to know it was the *reading*
            // that failed.
            let why: &[u8] = match e {
                libfs::FileError::NotFound => b" is not there",
                libfs::FileError::TooLarge => b" is too large to read",
                _ => b" could not be read",
            };
            Line::new().s(b"desktop-shell: wallpaper ").s(path.as_str().as_bytes()).s(why).end();
            return None;
        }
    };
    let image = match libdraw::png::decode(&bytes) {
        Ok(i) => i,
        Err(e) => {
            Line::new()
                .s(b"desktop-shell: wallpaper ")
                .s(path.as_str().as_bytes())
                .s(b" ")
                .s(e.why().as_bytes())
                .end();
            return None;
        }
    };
    let screen = Size::new(SCREEN_W, SCREEN_H as u32);
    let plan = libdraw::scale::fit(Size::new(image.width(), image.height()), screen);
    let pitch = SCREEN_W as usize * 4;
    let len = pitch * SCREEN_H as usize;
    let Some(geometry) =
        Geometry::with_pitch(SCREEN_W, SCREEN_H as u32, pitch, PixelFormat::XRGB8888)
    else {
        kprint(b"desktop-shell: the wallpaper geometry is unusable\n");
        return None;
    };
    let mut picture = alloc::vec![0u8; len];
    if !libdraw::scale::place(&image.pixels, image.geometry, plan, theme.desktop, &mut picture, geometry)
    {
        kprint(b"desktop-shell: the wallpaper could not be placed\n");
        return None;
    }

    let role = Role::Panel { dock: Edge::Top, reserve: 0 };
    let id = match session.create(&CreateWindowRequest::new(SCREEN_W, SCREEN_H as u32, role), BUFFERS)
    {
        Ok(id) => id,
        Err(_) => {
            kprint(b"desktop-shell: wallpaper CreateWindow FAILED\n");
            return None;
        }
    };
    // **Everything past `create` unwinds through one exit, because the window now exists.**
    // Returning `None` from the middle would leave the compositor holding a full-screen,
    // configured, hit-testable `panel` with nothing committed to it, for the life of the
    // process — no manager is attached this early, so its initial `Configure` goes out at once.
    // `open_overview` and `open_modal` in this file both say this, each after a review found it
    // (PR #244 blocking 3, PR #237 finding 7); this is the third (PR #272 review, worth
    // fixing 3).
    let mut ok = true;
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            kprint(b"desktop-shell: wallpaper buffer alloc FAILED\n");
            ok = false;
            break;
        };
        // SAFETY: `addr` maps `len` writable bytes and `picture` holds exactly `len`; the two
        // are distinct allocations, so they cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(picture.as_ptr(), addr, len) };
        let Some(mut w) = session.window(id) else {
            kprint(b"desktop-shell: wallpaper window vanished\n");
            ok = false;
            break;
        };
        if w.attach(i as u32, SCREEN_W, SCREEN_H as u32, pitch as u32, handle).is_err() {
            kprint(b"desktop-shell: wallpaper AttachBuffer FAILED\n");
            ok = false;
            break;
        }
    }
    if ok {
        // A pattern guard cannot borrow mutably, so the commit is a statement rather than a
        // `match` arm's condition.
        match session.window(id) {
            Some(mut w) => {
                if w.commit(0, (0, 0, SCREEN_W, SCREEN_H as u32)).is_err() {
                    kprint(b"desktop-shell: wallpaper Commit FAILED\n");
                    ok = false;
                }
            }
            None => {
                kprint(b"desktop-shell: wallpaper window vanished\n");
                ok = false;
            }
        }
    }
    if !ok {
        if let Some(w) = session.window(id) {
            let _ = w.destroy();
        }
        return None;
    }
    // **After the commit, not before it.** The first version of this line was printed as soon as
    // the picture had been decoded and placed — so it said the wallpaper was fitted while
    // `CreateWindow` went on to fail, and the gate asserting it passed against a desktop with no
    // picture on it. A line has to be printed where the thing it claims has happened.
    //
    // **Both sizes, because they answer different questions.** The decoded size can only have
    // come from an `IHDR` that was read; the drawn size can only have come from the fit
    // arithmetic having run on it. A gate asserting one proves less than a gate asserting both.
    Line::new()
        .s(b"desktop-shell: wallpaper ")
        .u(image.width() as u64)
        .s(b"x")
        .u(image.height() as u64)
        .s(b" drawn ")
        .u(plan.size.w as u64)
        .s(b"x")
        .u(plan.size.h as u64)
        .s(b" at ")
        .i(plan.origin.x as i64)
        .s(b",")
        .i(plan.origin.y as i64)
        .s(b" window ")
        .u(id as u64)
        .end();
    Some(Wallpaper { window: id, picture })
}

/// Read the session's theme from `THEME_PATH`, falling back to the built-in one.
///
/// **In the user's home rather than in `/etc`**, and that is a namespace decision rather than a
/// filing preference. A session namespace binds `/home`, `/bin`, `/dev/tty` and — for a
/// graphical one — `/system/fonts`; it has no `/etc`, and `session-mgr/CLAUDE.md` says adding a
/// member is a design decision each time. A theme is *a user's*, so the subtree a user already
/// owns is where it belongs, and no new authority is needed to read it. It is also what makes
/// the missing-file case testable from a prompt: the file is somewhere the person can delete.
///
/// **Every failure lands on the default**, silently as far as the screen is concerned: no file,
/// an unreadable one, bytes that are not UTF-8, or a file of nothing but typos all produce the
/// desktop that shipped. A theme is decoration, and a desktop that will not start because its
/// colours did not parse is a worse failure than any colour could be.
fn read_theme(ns: u64) -> Theme {
    // **One line whatever happens, beginning the same way**, so a gate can assert that the shell
    // *decided* about a theme without asserting which way it went — which is what makes "delete
    // the file and everything still renders" a control that can be re-run against the committed
    // gate rather than one that needs a step edited out (PR #263 review, finding 4).
    let bytes = match libfs::read_file(ns, THEME_PATH.as_bytes()) {
        Ok(b) => b,
        Err(e) => {
            // **Absent and unreadable are different**, and saying "no such file" about a file
            // that is sitting there is how somebody spends an afternoon looking for it.
            let why: &[u8] = match e {
                libfs::FileError::NotFound => b" absent; using the built-in theme",
                libfs::FileError::TooLarge => b" too large to read; using the built-in theme",
                _ => b" could not be read; using the built-in theme",
            };
            let t = Theme::default();
            Line::new()
                .s(b"desktop-shell: theme ")
                .s(THEME_PATH.as_bytes())
                .s(why)
                .s(b", font_px ")
                .u(t.font_px as u64)
                .end();
            return t;
        }
    };
    let Ok(text) = core::str::from_utf8(&bytes) else {
        let t = Theme::default();
        Line::new()
            .s(b"desktop-shell: theme ")
            .s(THEME_PATH.as_bytes())
            .s(b" is not UTF-8; using the built-in theme, font_px ")
            .u(t.font_px as u64)
            .end();
        return t;
    };
    let (theme, issues) = Theme::from_config(text);
    Line::new()
        .s(b"desktop-shell: theme ")
        .s(THEME_PATH.as_bytes())
        .s(b" read")
        .s(b" (")
        .u(bytes.len() as u64)
        .s(b" bytes, ")
        .u(issues.len() as u64)
        .s(b" ignored), font_px ")
        .u(theme.font_px as u64)
        .end();
    // **Each one named, up to a bound.** A person editing colours needs the line number, and a
    // file of a thousand bad lines must not become a thousand console lines.
    for issue in issues.iter().take(MAX_LOGGED_THEME_ISSUES) {
        Line::new()
            .s(b"desktop-shell: theme line ")
            .u(issue.line as u64)
            .s(match issue.kind {
                libdraw::theme::IssueKind::Malformed => b" is not `key = value`" as &[u8],
                libdraw::theme::IssueKind::UnknownKey => b" names a key this version does not know",
                libdraw::theme::IssueKind::BadValue => b" has a value this version cannot read",
            })
            .end();
    }
    theme
}

/// Where the session's theme lives, in the user's own subtree.
const THEME_PATH: &str = "/home/theme.toml";

/// How many bad theme lines are named before the rest are counted only.
const MAX_LOGGED_THEME_ISSUES: usize = 8;

/// Where a placed window's top-left goes: below the top bar, cascading so two launches do not
/// land on top of each other.
///
/// **A policy, and the shell's to have.** M6 built placement, restacking and the
/// initial-configure hold for a manager and nothing but a test client has ever supplied one —
/// this is the first process with an opinion about where a window goes. The opinion is
/// deliberately dull: below the bar, stepped. A real one is `desktop-shell.md`'s to specify.
const CASCADE_STEP: i32 = 24;

/// Wait set: the compositor's event channel, the manager channel, `/dev/desktop` and its
/// sessions.
static mut WAIT_HANDLES: [u64; 2 + 1 + MAX_DESKTOP_SESSIONS] = [0; 3 + MAX_DESKTOP_SESSIONS];
/// One 24-byte `IoResult` **per handle in the set**, because that is what the kernel writes.
///
/// `sys_wait` takes no length for this buffer: it writes one record for *every signalled*
/// handle (`kernel/src/syscall/table.rs`, `k * size_of::<IoResult>()`), and the caller is
/// required to have room for all of them. This was sized at one record while the handle set
/// grew to seven — so any wake where two handles signalled together wrote 24 bytes past the
/// end of a static. Nothing had noticed because the shell reads only the first record, and the
/// bytes past it land in whatever the linker put next.
///
/// Every other server in this tree already sizes it this way (`compositor`,
/// `logging-service`, `fs-server-ext4`); `session-mgr` waits on exactly one handle, where one
/// record is right. Found by the PR #257 reviewer while reading something else.
static mut WAIT_RESULTS: [u8; 24 * (3 + MAX_DESKTOP_SESSIONS)] =
    [0; 24 * (3 + MAX_DESKTOP_SESSIONS)];

/// Bootstrap registers, as `libsession::spawn_leader` fills them: `rdi` = notification
/// channel, `rsi` = the **session** namespace, `rdx` = the Tier-1 setup channel carrying
/// `argv` and the environment, `rcx` = `arg0`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(notif: u64, session_ns: u64, setup: u64, arg0: u64) -> ! {
    kprint(b"desktop-shell: up (graphical session leader)\n");

    // Two messages arrive on the setup channel, in order: the Tier-1 `argv` + environment,
    // then the compositor's forwarding endpoint. The second is what lets this process build
    // application namespaces — a `/dev/draw` *binding* resolves to a kernel registration and
    // never back to an endpoint, so the shell cannot re-bind what its own namespace holds.
    // **The shell's own environment, kept rather than discarded**, because every application
    // it launches gets one derived from it — `$env.HOME` and `PATH` should mean the same thing
    // in a window as at a serial prompt.
    let (argv, env) = match libstream::setup::bootstrap(notif, session_ns, setup, arg0).setup() {
        Some(Ok(s)) => (s.argv, s.env),
        _ => (alloc::vec::Vec::new(), libstream::wire::Record::default()),
    };
    // **The theme, read once and carried on the environment every application already gets**
    // (M11 Part C). One reader in the session rather than one per application: a client that
    // opened this file itself would need it in its namespace and would repeat the parse, and a
    // client launched before the file existed would disagree with one launched after.
    let theme = read_theme(session_ns);
    let env = env.with_str_field("THEME", &theme.to_config());
    // `argv[1]` is the user's real home, e.g. `/home/alice` — see `spawn_leader`'s `argv_rest`.
    // Empty means an application's `/home` cannot be scoped, so it is left unbound rather than
    // bound to something wider than the session's own.
    let home: &str = argv.get(1).map(|s| s.as_str()).unwrap_or("");
    if home.is_empty() {
        kprint(b"desktop-shell: no home in argv; applications get no /home\n");
    }
    let draw_endpoint = recv_handle(setup);
    let fs_endpoint = recv_handle(setup);
    let tty_endpoint = recv_handle(setup);
    let profile_endpoint = recv_handle(setup);
    // The clipboard server's forwarding endpoint (M12 Part E), for the same reason as the
    // other four: a `/dev/clipboard` *binding* resolves to a kernel registration and never
    // back to an endpoint, so the shell cannot re-bind what its own namespace holds.
    let clipboard_endpoint = recv_handle(setup);
    if draw_endpoint == 0 {
        kprint(b"desktop-shell: no compositor endpoint; cannot launch applications\n");
    }
    if fs_endpoint == 0 || tty_endpoint == 0 {
        // Degraded rather than fatal, and specific: an application without fonts draws
        // rectangles, and one without a tty cannot be a terminal. Both are worth naming,
        // because the symptom is a launched program that exits without saying why.
        kprint(b"desktop-shell: applications will have no fonts or no terminal\n");
    }

    // **Resolved from the session namespace, not from a root one.** This process has no root
    // handle: `spawn_leader` runs it in the namespace `desktop-session-mgr` constructed, which
    // is where `/dev/draw` was bound. That is the whole point — an application's namespace
    // will get a *narrower* bind, and the difference is what gates the manager channel.
    // SAFETY: `session_ns` is this process's namespace, live for its whole run.
    let (font, _) = match unsafe { libdraw::text::load_ui(session_ns, &theme, b"desktop-shell") } {
        Ok(loaded) => loaded,
        Err(e) => {
            libkern::debug::Line::new().s(b"desktop-shell: the UI font ").s(e.why()).end();
            fail(b"desktop-shell: font load FAILED (is /system readable in the session?)\n");
        }
    };

    // SAFETY: `session_ns` is live for this process's whole run.
    let transport = match unsafe { ChannelTransport::connect(session_ns) } {
        Ok(t) => t,
        Err(_) => fail(b"desktop-shell: connect to /dev/draw FAILED\n"),
    };
    let mut session = Session::new(transport);

    // **The wallpaper, before anything else this shell creates** (M12 Part F). Creation order
    // is bottom-first in the compositor's stack, so making it first is what makes it bottom-most
    // — no `Manage::Lower` needed, and nothing this shell raises later can get underneath it.
    let wallpaper = open_wallpaper(session_ns, &mut session, &theme);
    // Read often enough to be worth naming once. `0` is not a window id, so it is the "none"
    // every `ours`-style check already treats as absent.
    let wallpaper_window = wallpaper.as_ref().map_or(0, |w| w.window);

    // `panel`, not `normal`: the role is what reserves the strut, so ordinary windows are
    // placed below the bar rather than under it. M6 Part A built that and nothing but a test
    // client has asked for it.
    //
    // `reserve` is stated separately from the height on purpose — the role's own doc explains
    // that deriving it would make a bar that reserves less than it occupies inexpressible.
    // A bar wants them equal.
    let role = Role::Panel { dock: Edge::Top, reserve: BAR_H };
    let window = match session.create(&CreateWindowRequest::new(SCREEN_W, BAR_H, role), BUFFERS) {
        Ok(id) => id,
        Err(_) => fail(b"desktop-shell: top bar CreateWindow FAILED\n"),
    };

    // **Kept, because the top bar is repainted now** (M11 Part E batch 9). It was drawn once at
    // startup and never again — true while it held one static label, and the clock on it changes
    // every minute.
    let mut top_addrs = [core::ptr::null_mut::<u8>(); BUFFERS];
    let mut shown_clock = clock_text();
    // **Said once, because an absent clock is otherwise indistinguishable from a broken bar.**
    // The kernel reports the wall clock as unsupported when the RTC could not be read at boot,
    // and the bar's answer to that is to show nothing — which looks identical to a clock that
    // failed to draw. One line at startup tells the two apart.
    {
        let mut l = Line::new();
        l.s(b"desktop-shell: clock ");
        if shown_clock.is_empty() {
            l.s(b"unset");
        } else {
            l.s(shown_clock.as_bytes());
        }
        l.end();
    }
    let picture = render_bar(&theme, &font, &shown_clock).into_bytes();
    let len = BAR_PITCH * BAR_H as usize;
    if picture.len() != len {
        fail(b"desktop-shell: top bar render is not the size it declares\n");
    }
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            fail(b"desktop-shell: top bar buffer alloc FAILED\n");
        };
        top_addrs[i] = addr;
        // SAFETY: `addr` maps `len` writable bytes and `picture` holds exactly `len`; the two
        // regions are distinct allocations, so they cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(picture.as_ptr(), addr, len) };
        let Some(mut w) = session.window(window) else {
            fail(b"desktop-shell: top bar window vanished\n");
        };
        if w.attach(i as u32, SCREEN_W, BAR_H, BAR_PITCH as u32, handle).is_err() {
            fail(b"desktop-shell: top bar AttachBuffer FAILED\n");
        }
    }
    let Some(mut w) = session.window(window) else {
        fail(b"desktop-shell: top bar window vanished\n");
    };
    if w.commit(0, (0, 0, SCREEN_W, BAR_H)).is_err() {
        fail(b"desktop-shell: top bar Commit FAILED\n");
    }

    // **Build one application namespace and check it**, before anything is launched into it.
    // Part E's applications modal is what will call this per launch; doing it once here is
    // what makes the narrow bind observable — and the shell refusing to launch when the check
    // fails is the behaviour, not the test.
    // **The startup check gates, rather than only reporting.** It used to `kprint` and let
    // `launch` stay reachable, so a shell that knew its namespaces were wrong would launch into
    // them anyway (PR #237 review, finding 3).
    //
    // It runs here, *before* the manager channel is taken, which is the only moment a `manage`
    // resolve gets an honest answer from a namespace that does not bind it.
    // **The desktop endpoint, created before the first application namespace is built.**
    // `build_app_namespace` binds it, and the startup check below verifies it — so it has to
    // exist by then or the check would be verifying its absence.
    let desktop_endpoint = match make_channel() {
        Some((client_end, serve_end)) => {
            // SAFETY: storing our own serve end.
            unsafe { DESKTOP_SERVE = serve_end };
            kprint(b"desktop-shell: serving /dev/desktop\n");
            client_end
        }
        None => {
            kprint(b"desktop-shell: could not create the /dev/desktop endpoint\n");
            0
        }
    };

    let mut may_launch = false;
    if draw_endpoint != 0 {
        let app_ns =
            build_app_namespace(
                draw_endpoint,
                fs_endpoint,
                tty_endpoint,
                profile_endpoint,
                home,
                desktop_endpoint,
                clipboard_endpoint,
            );
        if app_ns != 0 {
            may_launch = verify_app_namespace(app_ns, !home.is_empty(), desktop_endpoint != 0);
            // SAFETY: closing the namespace; nothing has been launched into it yet.
            unsafe { syscall1(SYS_HANDLE_CLOSE, app_ns) };
        }
    }
    if !may_launch {
        kprint(b"desktop-shell: application namespaces are not gated; launching is disabled\n");
    }
    let launcher = Launcher {
        session_ns,
        draw: draw_endpoint,
        fs: fs_endpoint,
        tty: tty_endpoint,
        profile: profile_endpoint,
        desktop: desktop_endpoint,
        clipboard: clipboard_endpoint,
        home,
        env: &env,
        enabled: may_launch,
    };

    Line::new()
        .s(b"desktop-shell: top bar presented, window ")
        .u(window as u64)
        .s(b" ")
        .u(SCREEN_W as u64)
        .s(b"x")
        .u(BAR_H as u64)
        .end();

    // **The bottom bar is created here, beside the top one, and *placed* much later.**
    //
    // It sat after `manage()` in the first version of Part C, on the reasoning that it has to be
    // placed and only a manager can place. That deadlocks the shell against its own manager
    // hold: a `panel` is held for the manager exactly like a `normal` window — only a `popup` is
    // exempt — and `Session::create` blocks until the first `Configure` arrives. So the shell
    // parked inside `create`, unable to drain the manager channel and therefore unable to send
    // the `Place` that would have released it, and only the 200 ms configure deadline broke the
    // tie. Every session start paid it, and `no manager answer for window N` — a line that
    // exists to name a wedged or absent manager — fired on every healthy boot (PR #242 review,
    // blocking 1).
    //
    // `Place` works on an already-created window, so the create belongs here where no manager
    // is attached and nothing is held; only the placement has to wait.
    let mut entries: alloc::vec::Vec<WinEntry> = alloc::vec::Vec::new();
    let mut list_dirty = true;
    // **One desktop to start, which is also the scratch slot** — empty and unnamed, so the
    // first window created lands on it and `normalize_desktops` appends its successor.
    let mut desktops: alloc::vec::Vec<Desktop> =
        alloc::vec![Desktop { id: 1, name: alloc::string::String::new() }];
    let mut next_desktop_id: u32 = 2;
    let mut current_desktop: u32 = 1;
    let mut bottom_addrs = [core::ptr::null_mut::<u8>(); BUFFERS];
    let bottom = match session.create(
        &CreateWindowRequest::new(SCREEN_W, BAR_H, Role::Panel { dock: Edge::Bottom, reserve: BAR_H }),
        BUFFERS,
    ) {
        Ok(id) => {
            let mut ok = true;
            for i in 0..BUFFERS {
                let Some((handle, addr)) = shared_buffer(len) else {
                    ok = false;
                    break;
                };
                bottom_addrs[i] = addr;
                let Some(mut w) = session.window(id) else {
                    ok = false;
                    break;
                };
                if w.attach(i as u32, SCREEN_W, BAR_H, BAR_PITCH as u32, handle).is_err() {
                    ok = false;
                    break;
                }
            }
            if ok {
                Line::new()
                    .s(b"desktop-shell: bottom bar presented, window ")
                    .u(id as u64)
                    .end();
                Some(id)
            } else {
                kprint(b"desktop-shell: bottom bar buffers FAILED; no window list\n");
                None
            }
        }
        Err(_) => {
            kprint(b"desktop-shell: bottom bar CreateWindow FAILED; no window list\n");
            None
        }
    };

    // **The manager channel, which makes this the compositor's first real manager.**
    //
    // Resolved from the session namespace, which binds the `/dev/draw` subtree unscoped and
    // therefore reaches `manage`. An application's namespace binds `/dev/draw/new` alone and
    // does not — that asymmetry is the whole of what closed `manage-ungated`, and holding this
    // channel is the other half of it being a capability rather than a race.
    //
    // **Attaching a manager changes the compositor's behaviour**: a `normal` window's first
    // `Configure` is held until the manager acts (M6 Part B4), so from here on nothing reaches
    // the screen unless this process places it. That is the point — it is also why the top bar
    // is created *before* this, since a `panel` that waited on a manager that did not exist yet
    // would be waiting on itself.
    // SAFETY: `session_ns` is live for this process's whole run.
    let mut manager = match unsafe { ChannelTransport::manage(session_ns) } {
        Ok(m) => {
            kprint(b"desktop-shell: manager channel held\n");
            Some(m)
        }
        Err(_) => {
            // Not fatal: a shell that cannot manage is a shell that draws a bar and launches
            // things, which is worse but not nothing. Say so — the alternative is a session
            // where windows silently never appear.
            kprint(b"desktop-shell: /dev/draw/manage unavailable; windows will not be placed\n");
            None
        }
    };
    // **The work area, from the compositor rather than from arithmetic here.** The shell's own
    // two bars are not the only struts a session can have — any `panel`-role client declares
    // one — and a maximised window computed from `SCREEN_H - BAR_H * 2` would sit under the next
    // one with nothing able to notice. Kept current by `LayoutChanged` below (M9 Part B).
    let mut layout = manager.as_mut().and_then(query_layout).unwrap_or(default_layout());
    Line::new()
        .s(b"desktop-shell: work area ")
        .i(layout.work_x as i64)
        .s(b",")
        .i(layout.work_y as i64)
        .s(b" ")
        .u(layout.work_w as u64)
        .s(b"x")
        .u(layout.work_h as u64)
        .s(b" of ")
        .u(layout.screen_w as u64)
        .s(b"x")
        .u(layout.screen_h as u64)
        .end();
    // **And the snap zones the work area implies** (M9 Part F). Registered here rather than
    // computed by the compositor: which region means which rectangle is policy, and the
    // compositor's whole part is to match a pointer against a table it was given.
    if let Some(m) = manager.as_mut() {
        register_snap_zones(m, &layout);
    }

    // Where a maximised window came from, so restoring it has somewhere to go.
    //
    // **The shell's, not the compositor's.** A `maximized` flag there would be a second source
    // of truth about a rectangle, and the rectangle a window returns to is a decision — this is
    // the process that made it.
    let mut restore: alloc::vec::Vec<(u32, (i32, i32, u32, u32))> = alloc::vec::Vec::new();
    // Windows already asked to close, whose next middle-click destroys them.
    //
    // **Asked first, insisted on afterwards.** A window holds a process's work, and a taskbar
    // that destroyed it would take the decision away from the only participant that knows
    // whether that matters. A client that ignores the request gets `Manage::Close` instead,
    // which is the only answer available to a desktop whose applications draw their own chrome
    // (M9 Part C).
    //
    // **What insisting waits for is the person, not a clock** (M12 Part A). This was a
    // two-second grace period, and it was safe for exactly as long as no client could decline:
    // every application in the tree answered `CloseRequested` by exiting, so the timer never
    // fired outside a wedge. The editor's confirmation is the first client that deliberately
    // does not answer — it is asking the person the shell's own question — and against it a
    // timer *loses the buffer*, two seconds after a click, with no way to intervene. A shell
    // cannot tell "wedged" from "asking"; the person looking at the dialog can, so the second
    // click is what says "I meant it", which is what a Force Quit is everywhere else.
    //
    // Considered and rejected: cancelling the grace when a `dialog` parented to that window
    // appears. It reads well and only works for clients that ask in the way this one happens
    // to — a client asking in an overlay, or one genuinely busy, is punished for it — and it
    // makes a visible policy depend on a coincidence of roles.
    //
    // **And the arming expires, because the shell cannot see an answer** (PR #267 review,
    // blocking 1). The first version armed on the ask and disarmed only on the destroy, so a
    // person who middle-clicked, read the question and chose *keep editing* left the entry
    // armed for the life of the window — and a middle-click ten minutes later went straight to
    // `Manage::Close` with no question and nothing on screen that had ever said so. That is the
    // outcome this whole change exists to prevent, with the two-second bound replaced by an
    // unbounded one.
    //
    // There is no signal that says a client declined: `CloseRequested` has no refusal by
    // design, and inferring one from a dialog appearing or going away is the coincidence-of-
    // roles coupling rejected above. So the second click insists only while it is still *part of
    // the first gesture* — the rule M12's kill ring settles for cycling, for the same reason:
    // a continuation is valid immediately after the thing it continues, and a stale one must be
    // unreachable rather than merely unlikely.
    let mut asked_to_close: alloc::vec::Vec<(u32, u64)> = alloc::vec::Vec::new();
    let mut next_origin = BAR_H as i32 + CASCADE_STEP;

    // Registered on the first pass of the loop, once the manager channel is known to exist.
    let mut hotkey_done = manager.is_none();

    // **Placed now that a manager exists — created long before it.** A dock edge tells the
    // compositor how much space to reserve; it does not move the window, so a bottom bar that
    // is never placed sits at the origin under the top one.
    if let Some(m) = manager.as_mut() {
        // **Both bars are made sticky, and without this they vanish on the first switch.**
        // The compositor stamps every new window with its current desktop, so panels created
        // at startup live on desktop 1 — and `visible_on` is the single predicate behind
        // compositing, focus *and* hit-testing. From the moment `Super+2` succeeded there was
        // no window list, no applications button and no indicator on screen, and the only way
        // back to the chrome was a chord, because hotkeys are routed compositor-side and do not
        // need a visible window (PR #243 review, blocking 1).
        //
        // `STICKY_DESKTOP` is the reserved value for exactly this, specified in Part A and
        // unused until now: chrome belongs to the screen rather than to one desktop.
        //
        // **The wallpaper is in this list too, and was not** (PR #272 review, blocking 1). It
        // is created at startup like the bars, so it is stamped with desktop 1, and every other
        // desktop showed the bare ground colour until you switched back — which reads as
        // flicker rather than as a missing feature. A picture behind everything belongs to the
        // screen for exactly the reason the chrome does.
        //
        // **The gate could not have caught it**: `check-login` asserts the wallpaper line once,
        // at startup, and its `Super+2` comes hundreds of lines later. What catches it is the
        // step added below.
        for surface in [Some(window), bottom, Some(wallpaper_window).filter(|&w| w != 0)]
            .into_iter()
            .flatten()
        {
            if window_value(m, OP_MGR_SET_WINDOW_DESKTOP, surface, STICKY_DESKTOP) {
                Line::new().s(b"desktop-shell: surface ").u(surface as u64).s(b" is sticky").end();
            } else {
                Line::new()
                    .s(b"desktop-shell: surface ")
                    .u(surface as u64)
                    .s(b" could not be made sticky; it will vanish on a desktop switch")
                    .end();
            }
        }
    }
    if let Some(id) = bottom
        && let Some(m) = manager.as_mut()
    {
        place_window(m, id, 0, SCREEN_H - BAR_H as i32);
        Line::new()
            .s(b"desktop-shell: bottom bar placed at 0,")
            .i((SCREEN_H - BAR_H as i32) as i64)
            .end();
    }


    // The modal's entries, read once. `desktop-shell.md` §4: they are `/bin` programs, and
    // that falls out of decisions already made — they are ordinary files in the namespace, so
    // type-to-filter runs over them with no special mechanism.
    let programs = read_applications(session_ns);
    Line::new()
        .s(b"desktop-shell: /bin lists ")
        .u(programs.len() as u64)
        .s(b" programs")
        .end();
    let mut modal: Option<u32> = None;
    let mut modal_addrs = [core::ptr::null_mut::<u8>(); BUFFERS];
    // **The modal's own routing state** (M11 Part E batch 4). Until now this shell read pointer
    // events for three things it hit-tested by hand — the overview's thumbnails, the applications
    // button, and the taskbar's entries — and never for the modal, whose contents are a widget
    // tree rather than a fixed grid. Hand-testing a list that scrolls and filters would be the
    // toolkit's layout re-derived in the shell; a router is what the toolkit already has.
    let mut modal_tree = Tree::new();
    // The hover the modal's retained tree was last built with — see where it is resampled.
    let mut modal_hover: Option<u64> = None;
    let mut modal_router = Router::new();
    // **Persistent, since M11 Part E batch 6.** It was a throwaway in each render on the argument
    // that the launcher keeps no selection — which was true and also meant the scroll offset
    // reset every frame, so `/bin`'s 26 entries were ten reachable rows and a filter.
    let mut modal_list = ListState::default();
    let mut query = TextFieldState::new();
    // **The modal serves two purposes and has to know which.** It is the applications launcher
    // by default, and the desktop-name prompt after `Super+R` — same popup, same text field,
    // different thing to do with what was typed.
    let mut rename = false;
    // The overview: its window, the thumbnails it is showing, and which one is being dragged.
    let mut overview: Option<u32> = None;
    let mut over_addrs = [core::ptr::null_mut::<u8>(); BUFFERS];
    let mut shots: alloc::vec::Vec<(u32, u32, u32, alloc::vec::Vec<u8>)> = alloc::vec::Vec::new();
    let mut dragging: Option<u32> = None;
    // The overview is showing a desktop that has changed under it — by a click on its own
    // sidebar, or by a chord while it was open. It re-captures and re-presents rather than
    // closing: `desktop-shell.md` §6 is explicit that switching desktops inside the overview
    // "fetches a different set of images", and that is the whole of what it costs.
    let mut overview_dirty = false;

    // Blocks on the compositor's event channel, never spins — a spinning leader keeps a run
    // queue non-empty, so the idle thread never runs and deferred reclamation stops for the
    // whole machine (the 2026-07-31 `logging-service` bug).
    let ev = session.wait_handle();
    // Set whenever a manager request goes out; see the deadline below.
    let mut sent_request = false;
    // **The clock's next change, as a wait deadline** (M11 Part E batch 9). No timer handle is
    // needed: `sys_wait` already takes an absolute monotonic deadline, and the shell already
    // computes one for the close it may have to insist on. This is one more candidate for the
    // same minimum — a bar that ticks costs one wake a minute and no new kernel object.
    let mut next_tick = now_ns().map(next_minute).unwrap_or(u64::MAX);
    loop {
        // Both channels in one wait: the session's events and the manager's. Polling one
        // while blocked on the other would make a held window wait for a keystroke.
        // **A zero deadline when a request has just been sent.** `sys_wait` knows only about
        // the kernel queues; a manager event that arrived while a `Place`/`SetMinimized`/
        // `RegisterHotkey` reply was being waited for is parked *inside* the transport, with
        // nothing left in the kernel queue to wake this. Polling once after any request is the
        // belt: the drain below sees the parked events and the next iteration blocks normally
        // (PR #242 review, optional 10 — unverified there, and cheap enough to close).
        // **The clock's minute is the only deadline left.** A close used to put one here too;
        // it is a click rather than a timer now, so an outstanding close costs no wakeups at
        // all and the shell sleeps until something happens or the minute turns.
        //
        // **Which is what turned the belt above into a load-bearing one** (M12 Part A). The
        // `sent_request` flag covers *manager* requests, and the session's own — a `create`
        // waiting for its first `Configure`, an `acquire` waiting for a buffer release — park
        // arriving input inside the transport in exactly the same way, with nothing left in a
        // kernel queue to wake this. Those parked events were rescued by whatever wake happened
        // next, and until this part there was almost always one within two seconds: the close
        // grace period. Take the timer away and a press that landed while the modal was
        // committing a frame sat in the transport until the *minute* turned — which presented
        // as a launcher row that could be clicked and did nothing, once per run, entirely
        // reproducibly.
        //
        // So the queue is asked rather than assumed. `pump` is a non-blocking poll that moves
        // whatever the transport is holding into the per-window queues the drain below reads,
        // and a session with anything in them does not block. It cannot spin: the drain empties
        // what this counts.
        if session.pump().is_err() {
            fail(b"desktop-shell: compositor connection lost\n");
        }
        let deadline = if sent_request || session.events_pending() > 0 { 0 } else { next_tick };
        sent_request = false;
        let mgr_h = manager.as_ref().map(|m| m.wait_handle()).unwrap_or(0);
        // SAFETY: WAIT_HANDLES/WAIT_RESULTS are valid buffers sized for the whole set.
        unsafe {
            WAIT_HANDLES[0] = ev;
            let mut n = 1u64;
            if mgr_h != 0 {
                WAIT_HANDLES[n as usize] = mgr_h;
                n += 1;
            }
            // **The desktop endpoint and its sessions wait alongside the rest.** Polling them
            // between compositor events would make a `desktop list` wait on a keystroke, and
            // blocking on them separately would make the shell stop drawing while a command
            // thought about it.
            if DESKTOP_SERVE != 0 {
                WAIT_HANDLES[n as usize] = DESKTOP_SERVE;
                n += 1;
            }
            for i in 0..MAX_DESKTOP_SESSIONS {
                if DESKTOP_SESSIONS[i] != 0 {
                    WAIT_HANDLES[n as usize] = DESKTOP_SESSIONS[i];
                    n += 1;
                }
            }
            syscall4(
                SYS_WAIT,
                (&raw const WAIT_HANDLES) as u64,
                n,
                (&raw mut WAIT_RESULTS) as u64,
                deadline,
            )
        };
        // **Drained before the compositor's events**, so a `Switch` that changes what the bar
        // shows is reflected by the same iteration's redraw rather than the next one's.
        // SAFETY: reading our own endpoint and session table.
        unsafe {
            if DESKTOP_SERVE != 0 {
                serve_desktop_endpoint();
            }
            for i in 0..MAX_DESKTOP_SESSIONS {
                let ch = DESKTOP_SESSIONS[i];
                if ch != 0 {
                    list_dirty |= serve_desktop_session(
                        ch,
                        manager.as_mut(),
                        &mut desktops,
                        &entries,
                        &mut current_desktop,
                        &mut next_desktop_id,
                        &launcher,
                    );
                }
            }
        }
        if let Some(m) = manager.as_mut() {
            // The shell's own windows are never listed: the bars and the wallpaper are
            // `panel`s and the modal a `popup`, so the role filter already covers them, but
            // naming them is what keeps that true if a future shell window is `normal`.
            let ours =
                [window, bottom.unwrap_or(0), modal.unwrap_or(0), wallpaper_window];
            let mut fired = alloc::vec::Vec::new();
            let mut states: alloc::vec::Vec<librsproto::surface::WindowState> =
                alloc::vec::Vec::new();
            let mut dropped: alloc::vec::Vec<librsproto::surface::ConfigureEvent> =
                alloc::vec::Vec::new();
            list_dirty |= place_new_windows(
                m,
                &mut next_origin,
                &mut entries,
                &ours,
                &mut fired,
                &mut layout,
                &mut states,
                &mut dropped,
                &mut restore,
                current_desktop,
            );

            // **A gesture the user finished, answered with the `Configure` it asked for**
            // (M9 Parts E and F). The compositor drew the outline and applied nothing: changing
            // a window's geometry is this process's, so there is one path to it rather than two
            // that can disagree. What arrives is the rectangle the gesture asks for — where a
            // resize was let go, or the target of the zone a move was dropped in — and the
            // answer is the same request a maximise sends, which is why the event carries a
            // `ConfigureEvent` and this is a loop rather than a translation.
            for c in dropped {
                // **A window the user has resized or snapped is no longer the one that was
                // maximised.** Its restore rectangle described where it came from before a
                // maximise it has now left by hand; keeping it would make the next restore put
                // it somewhere it never was.
                restore.retain(|(id, _)| *id != c.window);
                sent_request = true;
                configure_window(m, c.window, c.x, c.y, c.width, c.height, b"drop");
            }
            // **What a client asked to be, decided here.** The compositor forwarded the
            // question and applied nothing: minimising is a manager request and maximising is a
            // `Configure` to a rectangle only this process can compute, because only this
            // process knows what a maximised window should return to.
            for s in states {
                use librsproto::surface::{
                    WINDOW_STATE_MAXIMIZED, WINDOW_STATE_MINIMIZED, WINDOW_STATE_NORMAL,
                };
                let Some(e) = entries.iter_mut().find(|e| e.id == s.window) else { continue };
                match s.state {
                    WINDOW_STATE_MINIMIZED => {
                        if minimize_window(m, e) {
                            sent_request = true;
                            list_dirty = true;
                            Line::new()
                                .s(b"desktop-shell: client asked to minimize window ")
                                .u(s.window as u64)
                                .end();
                        }
                    }
                    WINDOW_STATE_MAXIMIZED => {
                        // **Back on screen first.** A minimized window asked to maximise is
                        // asking to be *seen* maximised; configuring it where it is would resize
                        // something that is not on screen and leave it that way.
                        // `sent_request` is not set here, and it is not an omission: the
                        // `configure_window` below sets it unconditionally on every path
                        // through this arm. Setting it twice was a dead store the compiler
                        // pointed at once a rebuild stopped being cached.
                        if e.minimized && raise_window(m, e) {
                            list_dirty = true;
                        }
                        // Remembered before it moves, and only the first time: maximising an
                        // already-maximised window must not overwrite where it came from with
                        // the work area itself, which is a restore that does nothing.
                        if !restore.iter().any(|(id, _)| *id == s.window) {
                            restore.push((
                                s.window,
                                (e.origin.0, e.origin.1, e.size.0, e.size.1),
                            ));
                        }
                        sent_request = true;
                        // **What was asked for, not what happened.** A `Configure` is a request
                        // the client may decline — legally, and a fixed-size window is an
                        // ordinary thing — so the line `configure_window` prints is the shell's
                        // decision, and the window's own geometry event is what says whether it
                        // took it. `nxterm` declined every one until M9 Part D and now accepts.
                        configure_window(
                            m,
                            s.window,
                            layout.work_x,
                            layout.work_y,
                            layout.work_w,
                            layout.work_h,
                            b"maximize",
                        );
                    }
                    WINDOW_STATE_NORMAL => {
                        if e.minimized {
                            if raise_window(m, e) {
                                sent_request = true;
                                list_dirty = true;
                            }
                        }
                        if let Some(i) = restore.iter().position(|(id, _)| *id == s.window) {
                            let (_, (x, y, w, h)) = restore.remove(i);
                            sent_request = true;
                            configure_window(m, s.window, x, y, w, h, b"restore");
                        }
                    }
                    _ => {}
                }
            }

            // A window arriving or leaving can empty or fill a desktop, so the rule is
            // reconsidered after every drain rather than only where a desktop is switched.
            list_dirty |= normalize_desktops(
                &mut desktops,
                &entries,
                &mut current_desktop,
                &mut next_desktop_id,
            );
            for id in fired {
                // **Bounded exactly as the click is**, and the reason is `MAX_ENTRIES`' own:
                // an entry past it is neither drawn nor clickable, so minimizing one would take
                // the window off screen with no way to bring it back — "a window you cannot get
                // back rather than a cosmetic problem", which is what that bound exists to
                // prevent. Unbounded, the chord invalidated its own justification
                // (PR #242 review, finding 3).
                // **`Super+N`: switch.** The chord names the *Nth* desktop, not desktop id N —
                // ids are stable and never reused, so after a few have come and gone they stop
                // matching what a person sees on the indicator.
                if (HOTKEY_SWITCH_BASE + 1..=HOTKEY_SWITCH_BASE + CHORD_DESKTOPS).contains(&id) {
                    let n = (id - HOTKEY_SWITCH_BASE) as usize;
                    if let Some(d) = desktops.get(n - 1) {
                        let to = d.id;
                        if switch_desktop(m, &desktops, &mut current_desktop, to) {
                            sent_request = true;
                            list_dirty = true;
                            normalize_desktops(
                                &mut desktops,
                                &entries,
                                &mut current_desktop,
                                &mut next_desktop_id,
                            );
                            // **An open overview follows the switch.** A chord works while it
                            // is up — it is a popup, not a modal grab — and leaving it showing
                            // the desktop you just left is a lie on screen rather than a
                            // missing feature.
                            overview_dirty = overview.is_some();
                        }
                    }
                    continue;
                }
                // **`Super+Shift+N`: move the focused window there.** One attribute write, and
                // then the rule is reconsidered — the move can empty the desktop it left and
                // fill the one it joined, and both halves matter.
                if (HOTKEY_MOVE_BASE + 1..=HOTKEY_MOVE_BASE + CHORD_DESKTOPS).contains(&id) {
                    let n = (id - HOTKEY_MOVE_BASE) as usize;
                    let Some(to) = desktops.get(n - 1).map(|d| d.id) else { continue };
                    if let Some(e) = entries.iter_mut().find(|e| e.focused) {
                        let wid = e.id;
                        sent_request = true;
                        if window_value(m, OP_MGR_SET_WINDOW_DESKTOP, wid, to) {
                            e.desktop = to;
                            Line::new()
                                .s(b"desktop-shell: moved window ")
                                .u(wid as u64)
                                .s(b" to ")
                                .s(desktop_label(&desktops, to).as_bytes())
                                .end();
                            // **Here, which is where the doc always said it happens.** The
                            // first version relied on the next iteration's drain to re-apply
                            // the rule, so the bar was rendered from a `desktops` this move had
                            // already invalidated — a full-width panel blit showing a stale
                            // count, immediately followed by another with the right one
                            // (PR #243 review, finding 6).
                            normalize_desktops(
                                &mut desktops,
                                &entries,
                                &mut current_desktop,
                                &mut next_desktop_id,
                            );
                            list_dirty = true;
                        } else {
                            kprint(b"desktop-shell: SetWindowDesktop was refused\n");
                        }
                    }
                    continue;
                }
                if id == HOTKEY_RENAME {
                    // The rename prompt is a popup like the applications modal, and for the
                    // same reason: a `panel` takes no keyboard focus, so the bar could never
                    // read a typed name.
                    if modal.is_none() {
                        query.clear();
                        modal = open_modal(
                            &mut session, window, &theme, &font, &programs, &mut modal_addrs,
                            &query, &mut modal_tree, &mut modal_list,
                        );
                        if let Some(id) = modal {
                            stick(m, id, b"the rename prompt");
                        }
                        // **Set from whether the prompt actually opened.** `open_modal` returns
                        // `None` on three paths, and a `rename` left true with no modal sticks
                        // for the session — the next launcher Enter would rename the desktop to
                        // whatever was typed and never launch anything again
                        // (PR #243 review, finding 4).
                        rename = modal.is_some();
                        if rename {
                            kprint(b"desktop-shell: naming this desktop\n");
                        }
                    }
                    continue;
                }
                // **Bounded by what the bar is *showing*, which since Part D is not the first
                // `MAX_ENTRIES` of the global list but the first `MAX_ENTRIES` on the current
                // desktop.** With seven windows on another desktop and one here, the one here
                // is drawn, clickable and focused — and its index in `entries` is 7, so a bound
                // over the global list never reached it and the chord silently did nothing for
                // a window the bar was showing (PR #243 review, finding 5).
                let shown_now: alloc::vec::Vec<u32> =
                    visible_entries(&entries, current_desktop).iter().map(|e| e.id).collect();
                if id == HOTKEY_MINIMIZE
                    && let Some(e) = entries
                        .iter_mut()
                        .find(|e| shown_now.contains(&e.id) && e.focused && !e.minimized)
                {
                    let wid = e.id;
                    sent_request = true;
                    if minimize_window(m, e) {
                        Line::new()
                            .s(b"desktop-shell: Super+H minimized window ")
                            .u(wid as u64)
                            .end();
                        list_dirty = true;
                    }
                }
            }
        }
        if session.pump().is_err() {
            fail(b"desktop-shell: compositor connection lost\n");
        }
        // **A chord to minimize what has the keyboard**, registered once the manager exists.
        // The bar gives you a window back; this is how you put one away without reaching for
        // it, which is the half a taskbar alone does not cover.
        if !hotkey_done
            && let Some(m) = manager.as_mut()
        {
            hotkey_done = true;
            sent_request = true;
            use librsproto::surface::{MOD_META, MOD_SHIFT, MgrHotkey, OP_MGR_REGISTER_HOTKEY};
            let hk = MgrHotkey { id: HOTKEY_MINIMIZE, mods: MOD_META, code: KEY_H };
            // **The desktop chords, registered in the same pass.** `Super+N` switches and
            // `Super+Shift+N` moves the focused window — both ending in one attribute write,
            // which is what makes the move available without the overview open.
            for n in 1..=CHORD_DESKTOPS {
                let code = KEY_1 + (n as u16 - 1);
                for (base, mods) in
                    [(HOTKEY_SWITCH_BASE, MOD_META), (HOTKEY_MOVE_BASE, MOD_META | MOD_SHIFT)]
                {
                    let chord = MgrHotkey { id: base + n, mods, code };
                    let mut b = [0u8; core::mem::size_of::<MgrHotkey>()];
                    if chord.write(&mut b).is_none() {
                        kprint(b"desktop-shell: a desktop chord would not serialise\n");
                        continue;
                    }
                    let mut reply = [0u8; 64];
                    if m.request(OP_MGR_REGISTER_HOTKEY, &b, None, &mut reply).is_err() {
                        Line::new()
                            .s(b"desktop-shell: registering a desktop chord was refused (id ")
                            .u((base + n) as u64)
                            .s(b")")
                            .end();
                    }
                }
            }
            // Named `rename_hk` so it does not shadow the `rename` *bool* that decides what
            // the modal's Enter does — nothing in this block reads that bool today, which is
            // exactly the shape where a later edit silently reads the wrong one.
            let rename_hk = MgrHotkey { id: HOTKEY_RENAME, mods: MOD_META, code: KEY_R };
            let mut rb = [0u8; core::mem::size_of::<MgrHotkey>()];
            if rename_hk.write(&mut rb).is_some() {
                let mut reply = [0u8; 64];
                if m.request(OP_MGR_REGISTER_HOTKEY, &rb, None, &mut reply).is_err() {
                    kprint(b"desktop-shell: registering Super+R was refused\n");
                }
            }
            Line::new()
                .s(b"desktop-shell: Super+1..")
                .u(CHORD_DESKTOPS as u64)
                .s(b" switches, Super+Shift+N moves, Super+R names")
                .end();

            let mut body = [0u8; core::mem::size_of::<MgrHotkey>()];
            if hk.write(&mut body).is_none() {
                // **Said out loud.** The first version had no `else` at all, so a body that
                // would not serialise skipped the success line *and* the refusal line, and the
                // only evidence was a gate timing out on a line nobody printed.
                kprint(b"desktop-shell: a RegisterHotkey body would not serialise\n");
            } else {
                let mut reply = [0u8; 64];
                if m.request(OP_MGR_REGISTER_HOTKEY, &body, None, &mut reply).is_ok() {
                    kprint(b"desktop-shell: Super+H minimizes the focused window\n");
                } else {
                    kprint(b"desktop-shell: registering Super+H was refused\n");
                }
            }
        }

        let mut modal_dirty = false;
        // The hover the *next* frame will be drawn with. Applied after the drain, so that every
        // event in one batch is routed against the tree currently on screen.
        let mut next_hover = modal_hover;
        while let Some((w, event)) = session.next_event() {
            // A press on the applications button opens the modal. **A press, not a key**: a
            // `panel` takes no keyboard focus, so a key never reaches this process — see
            // `APPS_BUTTON_W`.
            // **Keys go to the modal**, which is a popup and therefore takes the keyboard —
            // the property `check-terminal` relies on when it says "an open menu is a topmost
            // popup and takes the keyboard". The top bar could never receive these.
            if Some(w) == modal {
                // **A click on a row launches it** (M11 Part E batch 4), routed through the
                // toolkit rather than hit-tested here: the modal's contents are a widget tree
                // that filters and scrolls, and re-deriving where its rows are would be the
                // layout engine written twice. The tree is the one `render_modal` recorded when
                // it painted, so a click lands on the row a person can see.
                if let libsurface::WindowEvent::Pointer(p) = event {
                    let rows = modal_rows(&programs, query.text());
                    let hovered = modal_hover;
                    let ui = modal_view(&query, &rows, &mut modal_list, hovered, &theme);
                    let bounds = Rect::new(0, 0, MODAL_W, MODAL_H);
                    let l = libui::layout::layout(&ui, bounds, &FontMetrics::new(&font, theme.font_px));
                    let (msgs, _) = modal_router.pointer(&modal_tree, &ui, &l, p);
                    // **Hover is a repaint even when nothing was clicked**, which is the whole
                    // of the highlight: a pointer that merely moved produces no message and
                    // still changes what the modal should look like.
                    // **A repaint, and no receipt.** `nxterm` reports its menu hover because a
                    // gate has no other way to see it; this shell must not, for a reason that
                    // outranks the convenience: it has **no build-mode `cfg` sites at all**, and
                    // `check-login` boots the *release* image, so a `test-harness` line here
                    // would be both a reintroduction of what the test-path retrofit removed and
                    // invisible to the gate that would want it. What proves this wiring is the
                    // click below it: hover and clicking ride the same router, so a router that
                    // hit-tests wrongly fails the launch.
                    // **What a gesture sees is what the tree was built with** (M12 Part D). A
                    // capture is a *tree id* of the deepest node under the cursor, and a hovered
                    // row draws more layers than a quiet one — so repainting with a *different*
                    // hover between a press and its release gives that node a new id,
                    // `path_to_id` finds nothing, and the click is silently lost. It presents as
                    // a launcher row that can be clicked and does nothing.
                    //
                    // **Not resampled while a button is held, and not mid-batch either.** M12
                    // Part B froze it from the press onwards, which is too late: the motion that
                    // brings the pointer onto a row is usually in the *same* drain as the press,
                    // so the hover advanced before any frame was drawn and the next one stranded
                    // the capture. Held until after the drain, every event in a batch routes
                    // against the tree that is actually on screen.
                    if !modal_router.grabbed() {
                        next_hover = modal_router.hovered_key(&modal_tree);
                    }
                    for msg in msgs {
                        // The drag converts through the widget's own arithmetic, which is what
                        // keeps a list's thumb and a terminal's agreeing about where a y points.
                        let ModalMsg::Launch(key) = msg else {
                            if let ModalMsg::Scroll(p) = msg
                                && p.buttons != 0
                            {
                                modal_list.drag_to(modal_list_h(), ROW_H, rows.len(), p.y);
                                modal_dirty = true;
                            }
                            continue;
                        };
                        // The key is an index into the *unfiltered* list, which is what
                        // `modal_rows` guarantees and what makes this resolvable at all.
                        if let Some(name) = programs.get(key as usize) {
                            if rename {
                                // A rename prompt has rows for the same reason the launcher
                                // does — it is the same widget — but choosing one is not
                                // naming a desktop, so a click is ignored rather than
                                // misinterpreted as one.
                                kprint(b"desktop-shell: the name prompt takes typing, not clicks\n");
                            } else {
                                // **The entry's `exec`, not its display name.** "Text Editor" is what a person
                                // reads; `nxedit` is what `/bin` resolves.
                                launcher.launch(name.exec.as_str(), &[]);
                                modal_hover = None;
                                close_modal(
                                    &mut session,
                                    &mut modal,
                                    &mut query,
                                    "applications modal",
                                    &mut modal_addrs,
                                );
                            }
                        }
                    }
                    continue;
                }
                // **A press outside it dismisses it, and losing the keyboard does too.**
                //
                // Two signals rather than one, because neither covers the other. `Focus(false)`
                // arrives when something *raises* — clicking another window, or a chord that
                // restacks — and it was all this had at first, which turned out to cover only
                // half the case: focus here is a consequence of stacking, so a press on the
                // desktop or on a panel raises nothing and changed no focus, and the modal
                // stayed open over the click (reported by the maintainer; M11 Part E batch 5).
                // `Dismissed` is the compositor saying the press landed elsewhere, which is the
                // half a client cannot see for itself.
                //
                // `InputLost` is neither of these — that is queue overflow, and reading it as
                // one would close the modal on a burst of pointer motion.
                if matches!(
                    event,
                    libsurface::WindowEvent::Dismissed | libsurface::WindowEvent::Focus(false)
                ) {
                    rename = false;
                    modal_hover = None;
                    close_modal(
                        &mut session,
                        &mut modal,
                        &mut query,
                        "applications modal",
                        &mut modal_addrs,
                    );
                    continue;
                }
                if let libsurface::WindowEvent::Key(k) = event {
                    if k.pressed != 0 {
                        if k.keycode == KEY_ESC {
                            // Dismissed without launching. The field declines Escape for
                            // exactly this — see `TextFieldState::apply`.
                            rename = false;
                            close_modal(&mut session, &mut modal, &mut query, "applications modal", &mut modal_addrs);
                        } else if k.keycode == KEY_ENTER && rename {
                            // **Naming is what makes a desktop persist**, so this is the one
                            // gesture that changes the lifecycle rather than the view.
                            // **Capped at what the wire can carry.** `write_list` refuses a
                            // whole `List` reply rather than truncating a name, which is right
                            // — but the text field has no cap of its own, so a 33-character
                            // label typed here made every later `List` fail for *all* desktops,
                            // permanently, since the name persists. The `desktop name` path
                            // already checked this bound; this one did not
                            // (PR #245 review, finding 4).
                            let full = query.text();
                            let name = &full[..full
                                .char_indices()
                                .map(|(i, c)| i + c.len_utf8())
                                .take_while(|&e| e <= librsproto::desktop::MAX_DESKTOP_NAME)
                                .last()
                                .unwrap_or(0)];
                            if let Some(d) =
                                desktops.iter_mut().find(|d| d.id == current_desktop)
                            {
                                d.name.clear();
                                d.name.push_str(name);
                            }
                            Line::new()
                                .s(b"desktop-shell: named this desktop ")
                                .untrusted(name.as_bytes())
                                .end();
                            rename = false;
                            list_dirty = true;
                            close_modal(&mut session, &mut modal, &mut query, "name prompt", &mut modal_addrs);
                            // Naming changes which desktops survive, so the rule applies here
                            // too — the one site that used to reach the next iteration by way
                            // of the popup's own destroy event.
                            normalize_desktops(
                                &mut desktops,
                                &entries,
                                &mut current_desktop,
                                &mut next_desktop_id,
                            );
                        } else if k.keycode == KEY_ENTER {
                            // The filtered list's first entry is what Enter launches. A
                            // selection the user moved would come from `ListState`; nothing
                            // moves it yet, and "the top hit" is what a launcher does with an
                            // untouched list anyway.
                            let filtered = filter(&programs, query.text());
                            if let Some(app) = filtered.first() {
                                // The entry's `exec`, not the name on the row — "Terminal" is
                                // what a person reads and `nxterm` is what `/bin` resolves.
                                launcher.launch(app.exec.as_str(), &[]);
                                // **Closed after launching, and this was the bug.** `modal`
                                // was set once and never cleared, so the popup stayed on top
                                // of whatever was launched and the top bar's click handler —
                                // gated on `modal.is_none()` — was inert for the rest of the
                                // session. There was no second launch and no way back, and
                                // the gate clicks once so it passed (PR #237 review,
                                // finding 6).
                                close_modal(&mut session, &mut modal, &mut query, "applications modal", &mut modal_addrs);
                            } else {
                                kprint(b"desktop-shell: nothing matches; not launching\n");
                            }
                        } else if query.apply(k.keycode, k.modifiers) {
                            modal_dirty = true;
                            // **A receipt per character.** Injection is relative and
                            // unacknowledged, so a dropped PS/2 batch silently eats a keystroke
                            // — a desktop named `wok` instead of `work`, which is a gate failure
                            // that looks like a logic bug. The greeter solved this by typing one
                            // character at a time and waiting for each redraw; this is the same
                            // receipt.
                            //
                            // **The launcher gets one too, and it is a count** (M12 Part A).
                            // This said the receipt was "limited to renaming so the launcher's
                            // typing stays quiet", and the quiet was the problem: `check-login`
                            // types six characters into the filter and immediately clicks a row,
                            // so a batch lost anywhere in that burst leaves the list showing
                            // something else and the click lands on nothing — a launcher row
                            // that can be clicked and does nothing, intermittently, with no line
                            // anywhere saying which key went missing. The gate waits for one of
                            // these per character now.
                            //
                            // A *count*, where naming logs the text: a desktop's name is a label
                            // the person is choosing and can see, and what somebody types into a
                            // launcher is a program they are about to run. The number is what
                            // says the keystroke arrived, which is the whole job.
                            if rename {
                                Line::new()
                                    .s(b"desktop-shell: name so far ")
                                    .untrusted(query.text().as_bytes())
                                    .end();
                            } else {
                                Line::new()
                                    .s(b"desktop-shell: applications modal listing ")
                                    .u(filter(&programs, query.text()).len() as u64)
                                    .end();
                            }
                        }
                    }
                }
            }
            // **The overview's own input**: Escape closes it, a press picks a thumbnail up, a
            // release over a sidebar row drops it there.
            if Some(w) == overview {
                if let libsurface::WindowEvent::Key(k) = event
                    && k.pressed != 0
                    && k.keycode == KEY_ESC
                {
                    close_overview(&mut session, &mut overview, &mut shots, &mut dragging, &mut over_addrs);
                    continue;
                }
                if let libsurface::WindowEvent::Pointer(p) = event
                    && p.kind == librsproto::surface::POINTER_BUTTON
                {
                    let pressed = p.flags & librsproto::surface::POINTER_PRESSED != 0;
                    if pressed {
                        // **Picked up by which thumbnail, not by where inside it.** The
                        // press-relative offset `TODO(scroll-grab)` is about matters when the
                        // thing being dragged is *drawn* following the cursor; here the
                        // thumbnail stays put and only the drop target is read, so the offset
                        // has nothing to be wrong about. That deferral named this as its second
                        // consumer; it is re-deferred rather than answered, and the reason is
                        // that this drag does not need what it is about.
                        dragging = thumb_at(p.x, p.y, shots.len()).map(|i| shots[i].0);
                        if let Some(id) = dragging {
                            Line::new()
                                .s(b"desktop-shell: dragging window ")
                                .u(id as u64)
                                .end();
                        }
                    } else {
                        // **Release: which of three gestures this was.** A press over a
                        // thumbnail always picks it up — it cannot know yet whether the pointer
                        // is about to move — so "was something picked up" does not separate a
                        // click from a drag. Where the release lands does:
                        //
                        // - over a sidebar row, holding a thumbnail → move that window there;
                        // - over the **same** thumbnail it started on → no movement, so it was
                        //   a click on a window: activate it;
                        // - over a sidebar row, holding nothing → a click on a desktop: go
                        //   there.
                        //
                        // The last two were dead until 2026-08-26: a press on a row set no drag
                        // and its release matched no arm, and a press-and-release on a thumbnail
                        // was discarded as "a drag abandoned". So the two most obvious gestures
                        // in an overview did nothing, while `desktop-shell.md` §6 said in as
                        // many words that "you can switch desktops from inside it". Only the
                        // drag was wired up, and only the drag was gated — which is how an
                        // unimplemented affordance passed for a tested one.
                        let picked = dragging.take();
                        let row = side_row_at(p.x, p.y, desktops.len());
                        let under = thumb_at(p.x, p.y, shots.len()).map(|i| shots[i].0);
                        match (picked, row) {
                            (Some(wid), Some(i)) => {
                                if let Some(m) = manager.as_mut() {
                                    let to = desktops[i].id;
                                    sent_request = true;
                                    if window_value(m, OP_MGR_SET_WINDOW_DESKTOP, wid, to) {
                                        if let Some(e) = entries.iter_mut().find(|e| e.id == wid) {
                                            e.desktop = to;
                                        }
                                        Line::new()
                                            .s(b"desktop-shell: dropped window ")
                                            .u(wid as u64)
                                            .s(b" on ")
                                            .s(desktop_label(&desktops, to).as_bytes())
                                            .end();
                                        normalize_desktops(
                                            &mut desktops,
                                            &entries,
                                            &mut current_desktop,
                                            &mut next_desktop_id,
                                        );
                                        list_dirty = true;
                                        // The overview is a snapshot of a desktop that has just
                                        // changed, so it is closed rather than left showing a
                                        // window that is no longer here.
                                        close_overview(
                                            &mut session,
                                            &mut overview,
                                            &mut shots,
                                            &mut dragging,
                                            &mut over_addrs,
                                        );
                                    }
                                }
                            }
                            (Some(wid), None) if under == Some(wid) => {
                                // **A window, so the overview has done its job.**
                                // `raise_window` is what a window-list entry does, and focus
                                // follows the raise — `focus_candidate` is topmost-focusable,
                                // so there is no second request and no second piece of state.
                                if let Some(m) = manager.as_mut()
                                    && let Some(e) = entries.iter_mut().find(|e| e.id == wid)
                                {
                                    sent_request = true;
                                    if raise_window(m, e) {
                                        list_dirty = true;
                                        Line::new()
                                            .s(b"desktop-shell: overview raised window ")
                                            .u(wid as u64)
                                            .end();
                                        close_overview(
                                            &mut session,
                                            &mut overview,
                                            &mut shots,
                                            &mut dragging,
                                            &mut over_addrs,
                                        );
                                    }
                                }
                            }
                            // **The row you are already on dismisses.** Clicking a desktop is
                            // "go there"; clicking it again, now that you are there, is the
                            // natural way to say "and I am done" — and without it an empty
                            // desktop is a dead end, because the way out of an overview was to
                            // click a window and there are none. Escape works and is not
                            // discoverable (reported from a real session, 2026-08-26).
                            (None, Some(i)) if desktops[i].id == current_desktop => {
                                close_overview(
                                    &mut session,
                                    &mut overview,
                                    &mut shots,
                                    &mut dragging,
                                    &mut over_addrs,
                                );
                            }
                            (None, Some(i)) => {
                                if let Some(m) = manager.as_mut() {
                                    let to = desktops[i].id;
                                    sent_request = true;
                                    if switch_desktop(m, &desktops, &mut current_desktop, to) {
                                        list_dirty = true;
                                        normalize_desktops(
                                            &mut desktops,
                                            &entries,
                                            &mut current_desktop,
                                            &mut next_desktop_id,
                                        );
                                        // Stays open, showing the desktop just switched to.
                                        overview_dirty = true;
                                    }
                                }
                            }
                            // **A click on the overview's own background dismisses it**, which
                            // is what clicking outside a menu does everywhere else. It also
                            // makes the indicator a toggle for free: the overview covers the
                            // bar, so a second click where the indicator is lands here.
                            (None, None) => {
                                close_overview(
                                    &mut session,
                                    &mut overview,
                                    &mut shots,
                                    &mut dragging,
                                    &mut over_addrs,
                                );
                            }
                            // A drag let go over nothing, which is not an error — abandoning a
                            // drag is not the same gesture as clicking the background, and
                            // dismissing on it would make a mis-aimed drop close the overview.
                            _ => {}
                        }
                    }
                }
                continue;
            }
            if w == window && modal.is_none() {
                if let libsurface::WindowEvent::Pointer(p) = event {

                    if p.kind == librsproto::surface::POINTER_BUTTON
                        && p.flags & librsproto::surface::POINTER_PRESSED != 0
                        && p.x >= 0
                        && (p.x as u32) < APPS_BUTTON_W
                    {
                        modal = open_modal(
                            &mut session, window, &theme, &font, &programs, &mut modal_addrs,
                            &query, &mut modal_tree, &mut modal_list,
                        );
                        if let Some((m, id)) = manager.as_mut().zip(modal) {
                            stick(m, id, b"the applications modal");
                        }
                    }
                }
            }
            // **A press on a window-list entry.** The entries are a fixed-width row, so the
            // index is the x coordinate divided by the width — the same arithmetic the layout
            // used to place them, rather than a second copy of the layout to hit-test against.
            if Some(w) == bottom
                && let libsurface::WindowEvent::Pointer(p) = event
                && p.kind == librsproto::surface::POINTER_BUTTON
                && p.flags & librsproto::surface::POINTER_PRESSED != 0
                && p.x >= 0
            {
                // **The indicator first**, because it owns the bar's right-hand end and the
                // entry arithmetic below would otherwise claim that x as a window slot.
                //
                // Clicking it advances to the next desktop. `desktop-shell.md` §7 says it opens
                // the overview, which is Part E — until then the indicator is the only pointer
                // way to change desktops, and a control that does nothing until a later
                // milestone is worse than one that does the obvious thing.
                if p.x as u32 >= INDICATOR_X {
                    // **Clicking the indicator opens the overview** (`desktop-shell.md` §7),
                    // which is what it was always specified to do — Part D made it advance to
                    // the next desktop only because there was no overview to open yet.
                    if overview.is_none()
                        && let Some(m) = manager.as_mut()
                    {
                        sent_request = true;
                        recapture(m, &entries, current_desktop, &mut shots);
                        overview = open_overview(
                            &mut session, window, &theme, &font, &shots, &desktops,
                            current_desktop, &entries, &mut over_addrs,
                            wallpaper.as_ref().map(|w| w.picture.as_slice()),
                        );
                        if let Some(id) = overview {
                            stick(m, id, b"the overview");
                            Line::new()
                                .s(b"desktop-shell: overview open, window ")
                                .u(id as u64)
                                .s(b" showing ")
                                .u(shots.len() as u64)
                                .s(b" of ")
                                .u(desktops.len() as u64)
                                .s(b" desktops")
                                .end();
                        }
                    }
                    continue;
                }
                let i = (p.x as u32 / ENTRY_W) as usize;
                let shown_ids: alloc::vec::Vec<u32> =
                    visible_entries(&entries, current_desktop).iter().map(|e| e.id).collect();
                if let Some(&wid) = shown_ids.get(i)
                    && let Some(m) = manager.as_mut()
                {
                    // **Clicking the focused window puts it away**, which is what every
                    // taskbar does and the only gesture that needs no second control. Clicking
                    // anything else brings it forward, restoring it first if it was minimized.
                    //
                    // **Indexed through the *visible* list**, which since Part D is not the
                    // whole one: entries on other desktops are not drawn, so `entries[i]` would
                    // name a different window than the one under the cursor as soon as a window
                    // moved away.
                    let Some(e) = entries.iter_mut().find(|e| e.id == wid) else { continue };
                    // **The middle button closes**, which is what every taskbar this borrows
                    // from does — and it needs no room in a layout that is already one fixed
                    // slot per window. It *asks*: a window holds a process's work, and the
                    // shell insists only when nothing happens (M9 Part C).
                    if p.button == libkern::abi::BTN_MIDDLE {
                        sent_request = true;
                        ask_to_close(m, wid, &mut asked_to_close);
                        continue;
                    }
                    if e.focused && !e.minimized {
                        sent_request = true;
                        if minimize_window(m, e) {
                            Line::new()
                                .s(b"desktop-shell: minimized window ")
                                .u(e.id as u64)
                                .end();
                        }
                    } else {
                        let id = e.id;
                        sent_request = true;
                        if raise_window(m, e) {
                            Line::new().s(b"desktop-shell: raised window ").u(id as u64).end();
                        } else {
                            Line::new()
                                .s(b"desktop-shell: raising window ")
                                .u(id as u64)
                                .s(b" was refused")
                                .end();
                        }
                    }
                    list_dirty = true;
                }
            }
        }
        // **A window that went on its own is no longer owed an insist.** Ids are never reused,
        // so a stale entry can never name a later window and nothing here is about correctness:
        // it is that this vector would otherwise hold an entry per window ever asked about, for
        // the life of the session (PR #267 review, optional 5 — the first version of this
        // comment claimed the stale entry could arm the *next* window, which its own first
        // clause rules out).
        asked_to_close.retain(|(id, _)| entries.iter().any(|e| e.id == *id));

        // **The clock, when the minute it shows has changed.** Compared as text rather than by
        // the deadline having passed: an unset clock formats to nothing every time, so this
        // repaints zero times instead of once a minute forever — and a wake that finds the same
        // string costs nothing but the comparison.
        if let Some(now) = now_ns()
            && now >= next_tick
        {
            next_tick = next_minute(now);
            let want = clock_text();
            if want != shown_clock {
                shown_clock = want;
                let picture = render_bar(&theme, &font, &shown_clock).into_bytes();
                let len = BAR_PITCH * BAR_H as usize;
                // `acquire`, for the reason the bottom bar's repaint gives: a buffer index this
                // code kept itself would invert its phase on any iteration where the commit did
                // not go out, and every repaint after that would write into what is on screen.
                if picture.len() == len
                    && let Some(mut w) = session.window(window)
                    && let Ok(b) = w.acquire()
                    && !top_addrs[b as usize].is_null()
                {
                    // SAFETY: the destination maps `len` writable bytes and `picture` holds
                    // exactly `len`; the two are distinct allocations.
                    unsafe {
                        core::ptr::copy_nonoverlapping(picture.as_ptr(), top_addrs[b as usize], len)
                    };
                    if w.commit(b, (0, 0, SCREEN_W, BAR_H)).is_err() {
                        kprint(b"desktop-shell: top bar Commit failed\n");
                    }
                }
            }
        }

        // **Redraw the bar when the list changed, and only then.** Every manager event would
        // otherwise repaint a bar that says the same thing, and a panel commit is a full-width
        // blit the compositor has to composite.
        if list_dirty
            && let Some(id) = bottom
        {
            let shown = visible_entries(&entries, current_desktop);
            let label = desktop_label(&desktops, current_desktop);
            log_window_list(&shown, &label, desktops.len());
            let picture = render_window_bar(&theme, &font, &shown, &label).into_bytes();
            let len = BAR_PITCH * BAR_H as usize;
            // **`acquire`, not an index this code keeps itself.** The first version alternated
            // a counter and advanced it unconditionally while the commit's result was
            // discarded — so any iteration where the commit did not go out inverted the phase,
            // and every repaint after that wrote into the buffer the compositor was displaying.
            // `acquire` blocks until one is genuinely free, which is the property being wanted,
            // and it is already what `present_modal` twelve lines below does
            // (PR #242 review, finding 4).
            if picture.len() == len
                && let Some(mut w) = session.window(id)
                && let Ok(b) = w.acquire()
                && !bottom_addrs[b as usize].is_null()
            {
                // SAFETY: the destination maps `len` writable bytes and `picture` holds exactly
                // `len`; the two are distinct allocations.
                unsafe {
                    core::ptr::copy_nonoverlapping(picture.as_ptr(), bottom_addrs[b as usize], len)
                };
                if w.commit(b, (0, 0, SCREEN_W, BAR_H)).is_err() {
                    kprint(b"desktop-shell: bottom bar Commit failed\n");
                }
            }
            list_dirty = false;
        }

        // **Re-render an open overview whose desktop changed.** Its thumbnails are of one
        // desktop, so a switch — from its own sidebar, or from a chord while it is up — makes
        // every one of them stale. `desktop-shell.md` §6 chose this over closing: "switching
        // desktops inside the overview is trivial, it fetches a different set of images".
        if overview_dirty {
            overview_dirty = false;
            if let Some(id) = overview
                && let Some(m) = manager.as_mut()
            {
                sent_request = true;
                recapture(m, &entries, current_desktop, &mut shots);
                present_overview(
                    &mut session, id, &theme, &font, &shots, &desktops, current_desktop,
                    &entries, &over_addrs,
                    wallpaper.as_ref().map(|w| w.picture.as_slice()),
                );
                Line::new()
                    .s(b"desktop-shell: overview now showing ")
                    .u(shots.len() as u64)
                    .s(b" on ")
                    .s(desktop_label(&desktops, current_desktop).as_bytes())
                    .end();
            }
        }

        // The hover the drain settled on — applied only if **no gesture is still running**.
        //
        // **Deferring the sample was half the rule** (PR #270 review, blocking 1). It fixes the
        // element-versus-tree mismatch *during* a batch, and then applies the new hover anyway:
        // a batch of `[ENTER, MOTION, PRESS]` leaves `next_hover` on the row, because the motion
        // sampled it before the press made `grabbed()` true. Repainting with it gives the row
        // three children where it had two, the captured node a new id, and the release nothing
        // to find — which is the whole bug, arriving one drain later than before.
        //
        // `Child` has no such hole because its `present` records `hovered_key()`, which under a
        // grab is already the shown value and therefore a no-op. This is the same rule spelled
        // out for a loop that owns its own tree.
        if !modal_router.grabbed() && next_hover != modal_hover {
            modal_hover = next_hover;
            modal_dirty = true;
        }
        // Redraw the modal when the query changed, so the filter is visible. A filter you
        // cannot see is not a filter.
        if modal_dirty {
            if let Some(id) = modal {
                let rows = modal_rows(&programs, query.text());
                // Read before the borrow: `present_modal` takes the tree mutably to record what
                // it painted, and the hover it should paint *with* comes from the tree as it is.
                let hovered = modal_hover;
                present_modal(
                    &mut session,
                    id,
                    &theme,
                    &font,
                    &query,
                    &rows,
                    &modal_addrs,
                    &mut modal_tree,
                    hovered,
                    &mut modal_list,
                );
            }
        }
    }
}

/// Scratch for [`now_ns`].
static mut CLOCK_BUF: u64 = 0;

/// The monotonic clock, in nanoseconds, or `None` if it will not answer.
///
/// **The clock on the top bar is what needs it**, since M11 Part E batch 9 — it ticks off this
/// rather than off a timer object. The close grace period used to be the only caller and is
/// gone: insisting on a close is a second click now rather than an elapsed two seconds, so
/// nothing about closing a window depends on the clock answering at all.
///
/// `None` is a clock that has stopped, and every caller treats it as *nothing to do this round*
/// rather than as zero: a deadline nothing can evaluate is a wait that returns immediately for
/// ever, which is the spin this file calls machine-wide harmful.
fn now_ns() -> Option<u64> {
    // SAFETY: CLOCK_BUF is a valid writable u64 out-param.
    let r = unsafe {
        libkern::syscall::syscall2(
            libkern::SYS_CLOCK_READ,
            libkern::abi::CLOCK_MONOTONIC,
            (&raw mut CLOCK_BUF) as u64,
        )
    };
    if r != 0 {
        return None;
    }
    // SAFETY: the call succeeded, so the kernel wrote the ns count.
    Some(unsafe { (&raw const CLOCK_BUF).read() })
}

/// How long a close request stays armed for the click that insists on it.
///
/// **Not the grace period coming back.** That timer *acted* when it expired — it destroyed a
/// window whose client had not answered. This one only forgets: when it runs out the next
/// middle-click asks again, which is the safe direction and the one a person can recover from
/// by clicking once more.
///
/// It is how long a second click is still part of the first, and the person is the clock it is
/// measured against: they click, watch for a moment, and click again when nothing happened.
/// Long enough for that; short enough that a click made after reading a question and answering
/// it is a fresh intention rather than a continuation. `check-login`'s two clicks are 63 ms
/// apart on the accelerator CI uses, so the gate has two orders of magnitude of room.
const INSIST_WINDOW_NS: u64 = 5_000_000_000;

/// Ask a window's client to close — or, if the ask is still in hand, insist.
///
/// **The second click is the insist**, which is the whole of M12 Part A's close-policy change:
/// the first middle-click asks, and if the window is still there when the person clicks again
/// they have said they meant it. Before this the answer was a two-second timer, which was safe
/// only while no client could decline — see `asked_to_close`'s declaration for why the editor's
/// confirmation dialog is exactly the client that makes a timer lose work, and for why the arm
/// expires rather than lasting the window's life.
///
/// **A clock that will not answer means nothing is ever armed**, so the taskbar can ask and
/// never insist. That is this file's stance on `now_ns` everywhere: the failure that leaves the
/// machine working is the one where a window stays, and a window that cannot be forced shut from
/// the bar can still be closed by its own button.
///
/// A window that goes away on its own leaves the entry that named it, so the ordinary case never
/// reaches `Manage::Close` at all.
fn ask_to_close(
    mgr: &mut ChannelTransport,
    window: u32,
    asked: &mut alloc::vec::Vec<(u32, u64)>,
) {
    use librsproto::surface::{MgrWindowRef, OP_MGR_REQUEST_CLOSE};
    let now = now_ns();
    let armed = asked
        .iter()
        .position(|&(id, until)| id == window && now.is_some_and(|n| n < until));
    if let Some(i) = armed {
        if insist_on_close(mgr, window) {
            asked.remove(i);
        }
        return;
    }
    let mut body = [0u8; core::mem::size_of::<MgrWindowRef>()];
    if (MgrWindowRef { window, other: 0 }).write(&mut body).is_none() {
        return;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_REQUEST_CLOSE, &body, None, &mut reply).is_err() {
        Line::new().s(b"desktop-shell: RequestClose refused for window ").u(window as u64).end();
        return;
    }
    Line::new().s(b"desktop-shell: asked window ").u(window as u64).s(b" to close").end();
    // An expired entry for this window is replaced rather than joined: one window is being
    // asked about once, however many times the question has been put.
    asked.retain(|&(id, _)| id != window);
    if let Some(n) = now {
        asked.push((window, n.saturating_add(INSIST_WINDOW_NS)));
    }
}

/// Destroy a window whose client did not answer — `Manage::Close`.
///
/// Reached only from [`ask_to_close`]'s second click. `true` if the compositor took it.
fn insist_on_close(mgr: &mut ChannelTransport, window: u32) -> bool {
    use librsproto::surface::{MgrWindowRef, OP_MGR_CLOSE};
    let mut body = [0u8; core::mem::size_of::<MgrWindowRef>()];
    if (MgrWindowRef { window, other: 0 }).write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_CLOSE, &body, None, &mut reply).is_err() {
        Line::new().s(b"desktop-shell: Close refused for window ").u(window as u64).end();
        return false;
    }
    Line::new()
        .s(b"desktop-shell: window ")
        .u(window as u64)
        .s(b" did not answer; closed it")
        .end();
    true
}

/// Ask a window's client to adopt a geometry — `Manage::Configure`.
///
/// **A request, and the reply says only that the compositor forwarded it.** Whether the client
/// adopts the size is the client's: declining is legal and stays legal, and a window that
/// declines simply goes on committing what it has.
/// How wide the band along each screen edge that triggers a snap is, in pixels.
///
/// **Policy, and it lives here rather than in the compositor** — which is the whole point of a
/// registered table: the compositor tests the pointer against rectangles and knows nothing about
/// edges, halves or how close counts. Wide enough to reach by throwing a window at the edge,
/// narrow enough not to fire while dragging a window that happens to end up near one.
const SNAP_BAND: u32 = 24;

/// Register the eight snap zones — four edges and four corners — for `work`.
///
/// **Recomputed and re-registered wholesale**, which is why the ids are fixed and registering an
/// existing id replaces it: the zones *are* the work area, so a bar appearing or going away
/// makes every one of them wrong at once. A shell that registered them once at startup would
/// snap windows over its own bars for the rest of the session.
///
/// The targets are the policy: half the work area for an edge, a quarter for a corner. The
/// compositor never learns that — it matches a pointer against a rectangle and reports the one
/// it matched.
fn register_snap_zones(mgr: &mut ChannelTransport, work: &MgrLayout) -> bool {
    use librsproto::surface::{MgrSnapZone, OP_MGR_REGISTER_SNAP_ZONE};
    let (x, y, w, h) = (work.work_x, work.work_y, work.work_w, work.work_h);
    let (hw, hh) = (w / 2, h / 2);
    let band = SNAP_BAND.min(w.max(1)).min(h.max(1));
    let b = band as i32;
    // `(id, trigger, target)`. Corners first: they overlap the edges, and the compositor takes
    // the **first** match — so the more specific zone has to come first, and that ordering is
    // the manager's to get right because the manager wrote the table.
    let zones = [
        (1u32, (x, y, band, band), (x, y, hw, hh)),
        (2, (x + w as i32 - b, y, band, band), (x + hw as i32, y, w - hw, hh)),
        (3, (x, y + h as i32 - b, band, band), (x, y + hh as i32, hw, h - hh)),
        (
            4,
            (x + w as i32 - b, y + h as i32 - b, band, band),
            (x + hw as i32, y + hh as i32, w - hw, h - hh),
        ),
        (5, (x, y, band, h), (x, y, hw, h)),
        (6, (x + w as i32 - b, y, band, h), (x + hw as i32, y, w - hw, h)),
        (7, (x, y, w, band), (x, y, w, hh)),
        (8, (x, y + h as i32 - b, w, band), (x, y + hh as i32, w, h - hh)),
    ];
    let mut all = true;
    for (id, t, g) in zones {
        let z = MgrSnapZone {
            id,
            trigger_x: t.0,
            trigger_y: t.1,
            trigger_w: t.2,
            trigger_h: t.3,
            target_x: g.0,
            target_y: g.1,
            target_w: g.2,
            target_h: g.3,
        };
        let mut body = [0u8; 36];
        let mut reply = [0u8; 8];
        let ok = z.write(&mut body).is_some()
            && mgr.request(OP_MGR_REGISTER_SNAP_ZONE, &body, None, &mut reply).is_ok();
        if !ok {
            Line::new().s(b"desktop-shell: snap zone ").u(id as u64).s(b" was refused").end();
            all = false;
        }
    }
    if all {
        Line::new()
            .s(b"desktop-shell: snap zones registered for work area ")
            .i(x as i64)
            .s(b",")
            .i(y as i64)
            .s(b" ")
            .u(w as u64)
            .s(b"x")
            .u(h as u64)
            .end();
    }
    all
}

fn configure_window(
    mgr: &mut ChannelTransport,
    window: u32,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    why: &[u8],
) {
    use librsproto::surface::{ConfigureEvent, OP_MGR_CONFIGURE};
    let mut body = [0u8; core::mem::size_of::<ConfigureEvent>()];
    let ev = ConfigureEvent { window, width: w, height: h, x, y };
    if ev.write(&mut body).is_none() {
        kprint(b"desktop-shell: a Configure body would not serialise\n");
        return;
    }
    // **Logged from the request's own arguments, inside the function that sends it.** The first
    // version logged the rectangle the *caller* had computed, beside the call — and a control
    // that changed what was sent while leaving the log alone passed the gate, which is the
    // defect PR #238's review found in a different assertion. One value, read once, by the code
    // that puts it on the wire.
    Line::new()
        .s(b"desktop-shell: ")
        .s(why)
        .s(b" window ")
        .u(window as u64)
        .s(b" to ")
        .i(x as i64)
        .s(b",")
        .i(y as i64)
        .s(b" ")
        .u(w as u64)
        .s(b"x")
        .u(h as u64)
        .end();
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_CONFIGURE, &body, None, &mut reply).is_err() {
        Line::new().s(b"desktop-shell: Configure refused for window ").u(window as u64).end();
    }
}

/// Ask the compositor for the screen and the work area.
fn query_layout(mgr: &mut ChannelTransport) -> Option<MgrLayout> {
    let mut reply = [0u8; 64];
    let n = mgr.request(OP_MGR_QUERY_LAYOUT, &[], None, &mut reply).ok()??;
    MgrLayout::read(&reply[..n])
}

/// What to assume when the compositor cannot be asked.
///
/// **A shell with no manager channel draws bars and launches things** — see where the channel is
/// taken — so it still needs numbers, and these are the ones it used everywhere before there was
/// an op to ask with. Named rather than inlined so the fallback is visible as a fallback.
fn default_layout() -> MgrLayout {
    MgrLayout {
        screen_w: SCREEN_W,
        screen_h: SCREEN_H as u32,
        work_x: 0,
        work_y: BAR_H as i32,
        work_w: SCREEN_W,
        work_h: (SCREEN_H as u32).saturating_sub(BAR_H * 2),
    }
}

/// Ask the manager channel to put `window` at `(x, y)`.
fn place_window(mgr: &mut ChannelTransport, window: u32, x: i32, y: i32) {
    use librsproto::surface::{MgrPlace, OP_MGR_PLACE};
    // **Sized from the type, not from the byte count the spec publishes.** `write` refuses a
    // short buffer by returning `None`, so a hand-written length that stops matching after a
    // field is added turns every request of that kind into a silent no-op — the defect
    // `send_mgr_event` documents at length (PR #217 finding 5, restated by #242 finding 5).
    let mut body = [0u8; core::mem::size_of::<MgrPlace>()];
    if (MgrPlace { window, x, y }).write(&mut body).is_none() {
        kprint(b"desktop-shell: a Place body would not serialise\n");
        return;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_PLACE, &body, None, &mut reply).is_err() {
        Line::new().s(b"desktop-shell: Place refused for window ").u(window as u64).end();
    }
}

/// Send one `window`+`value` manager request.
fn window_value(mgr: &mut ChannelTransport, op: u16, window: u32, value: u32) -> bool {
    use librsproto::surface::MgrWindowValue;
    let mut body = [0u8; core::mem::size_of::<MgrWindowValue>()];
    if (MgrWindowValue { window, value }).write(&mut body).is_none() {
        kprint(b"desktop-shell: a window-value body would not serialise\n");
        return false;
    }
    let mut reply = [0u8; 64];
    mgr.request(op, &body, None, &mut reply).is_ok()
}

/// Make one of the shell's own popups sticky, so a desktop switch does not strand it.
///
/// **The same defect the bars had, one layer along.** The compositor stamps every new window
/// with the desktop that is current when it is created, and `visible_on` is the single predicate
/// behind compositing, focus *and* hit-testing — so an overview opened on desktop 1 is invisible
/// and unclickable on desktop 2, while the shell still holds it and still believes it is open.
/// A person sees a menu that "stays there" when they come back and does nothing while they are
/// away; the gate saw a press land on `win=none`.
///
/// Chrome belongs to the screen rather than to one desktop, and that is as true of a launcher
/// and an overview as it is of a bar. It is *load-bearing* for the overview:
/// `desktop-shell.md` §6 says you switch desktops from inside it, which is only meaningful if it
/// survives the switch.
fn stick(mgr: &mut ChannelTransport, id: u32, what: &[u8]) {
    if !window_value(mgr, OP_MGR_SET_WINDOW_DESKTOP, id, STICKY_DESKTOP) {
        Line::new()
            .s(b"desktop-shell: ")
            .s(what)
            .s(b" could not be made sticky; it will strand on a desktop switch")
            .end();
    }
}

/// Raise `window` and give it the keyboard, restoring it first if it was minimized.
///
/// **`Raise` *is* the focus change** — the compositor's focus candidate is the topmost
/// focusable window, so there is no second request to make and no second piece of state to
/// disagree with the stack about who has it.
fn raise_window(mgr: &mut ChannelTransport, e: &mut WinEntry) -> bool {
    use librsproto::surface::{MgrWindowRef, OP_MGR_RAISE, OP_MGR_SET_MINIMIZED};
    if e.minimized {
        // Restore before raising: a minimized window is not a focus candidate, so raising it
        // first would reorder the stack and leave the keyboard where it was.
        if !window_value(mgr, OP_MGR_SET_MINIMIZED, e.id, 0) {
            return false;
        }
        e.minimized = false;
    }
    // **`MgrWindowRef`, which is what `dispatch` parses this op as.** `MgrWindowValue` is
    // byte-identical here only because the second field is zero, and a shape that is right by
    // coincidence stops being right the moment either side grows (PR #242 review, finding 8).
    let mut body = [0u8; core::mem::size_of::<MgrWindowRef>()];
    if (MgrWindowRef { window: e.id, other: 0 }).write(&mut body).is_none() {
        return false;
    }
    let mut reply = [0u8; 64];
    mgr.request(OP_MGR_RAISE, &body, None, &mut reply).is_ok()
}

/// Minimize `window`.
fn minimize_window(mgr: &mut ChannelTransport, e: &mut WinEntry) -> bool {
    use librsproto::surface::OP_MGR_SET_MINIMIZED;
    if window_value(mgr, OP_MGR_SET_MINIMIZED, e.id, 1) {
        e.minimized = true;
        e.focused = false;
        return true;
    }
    false
}

/// Paint the overview: frozen thumbnails of the current desktop, and a sidebar of desktops.
///
/// **Frozen, and that is what makes this affordable.** `desktop-shell.md` §6 rejected
/// compositing live windows with a scale transform — which needs scale as a window attribute,
/// geometry save and restore, and windows physically relocating — in favour of asking the
/// compositor for a snapshot. Real windows never move; the compositor gained one operation
/// instead of a transform pipeline. A window drawn *after* the capture shows its state at the
/// moment the overview opened, which is accepted deliberately.

fn render_overview(
    theme: &Theme,
    font: &Font,
    shots: &[(u32, u32, u32, alloc::vec::Vec<u8>)],
    desktops: &[Desktop],
    current: u32,
    entries: &[WinEntry],
    wallpaper: Option<&[u8]>,
) -> MemFramebuffer {
    let geometry =
        Geometry::with_pitch(SCREEN_W, SCREEN_H as u32, OVER_PITCH, PixelFormat::ARGB8888)
            .unwrap_or_else(|| fail(b"desktop-shell: bad overview geometry\n"));
    use libdraw::framebuffer::Framebuffer as _;
    let mut fb = MemFramebuffer::new(geometry);
    // **A translucent ground, so the overview sits *on* the desktop rather than replacing it**
    // (M13 Part C). This window used to be opaque and full-screen, which meant that to look like
    // an overlay it had to redraw the desktop itself — the wallpaper, dimmed, scaled and composited
    // here. That was the nearest thing to translucency reachable without an alpha channel, and it
    // was reported as the picture disappearing whenever you opened the overview (2026-09-02).
    //
    // Now the compositor has the real desktop underneath — live windows and all — and this fills a
    // dark colour at a partial opacity over it. The wallpaper is no longer read, scaled or copied
    // for this; `libdraw::scale::dim` went with it. What remains is one fill.
    //
    // **Dark rather than clear**, which is what makes it read as *behind* something. GNOME blurs
    // as well; a blur is a separable convolution over a million pixels per open, and dimming alone
    // is what the request asked for.
    fb.fill_rect_alpha(geometry.bounds(), OVERVIEW_GROUND, OVERVIEW_GROUND_ALPHA);

    // **The wallpaper again, scaled once for every miniature that wants it.** Once rather than
    // per row: `box_downscale` averages the whole source per destination pixel, so doing it per
    // desktop would repeat a million reads for an identical answer.
    let mini = wallpaper.and_then(|p| mini_wallpaper(p));

    // The sidebar's rows, drawn through the toolkit so they look like the rest of the shell.
    let mut rows: alloc::vec::Vec<Element<()>> = alloc::vec::Vec::new();
    for d in desktops {
        let mut label = alloc::string::String::new();
        label.push_str(if d.id == current { "> " } else { "  " });
        label.push_str(&desktop_label(desktops, d.id));
        rows.push(sized(
            libdraw::geom::Size::new(SIDE_W, SIDE_ROW_H),
            row(alloc::vec![
                padding(
                    Insets::all(MINI_PAD),
                    desktop_preview(entries, d.id, theme, mini.is_some())
                ),
                padding(Insets { top: 8, right: 8, bottom: 8, left: 0 }, text(label)),
            ]),
        ));
    }
    let side = column(rows);
    let bounds = Rect::new(
        (SCREEN_W - SIDE_W) as i32,
        BAR_H as i32,
        SIDE_W,
        SCREEN_H as u32 - BAR_H,
    );
    let metrics = FontMetrics::new(font, theme.font_px);
    let l = layout(&side, bounds, &metrics);
    // **A translucent panel with opaque ink on it** — Stretch 3, finally as asked (M13 Part C).
    // M11 batch 10 could only make it a *dark* panel: `paint` clears to `background`, which since
    // the theme turned light is the white an application draws on, so the sidebar was a white
    // column down the side of the desktop. A dark panel read as deliberate without translucency.
    //
    // The ground is filled here at its own opacity — lower than the overview's, so the sidebar
    // reads as a lighter sheet of glass over the darkened desktop — and the rows are then drawn
    // with `paint_over`, which is `paint` without the clear. That ordering is the whole trick and
    // it is why this needed a per-pixel alpha channel rather than a per-window opacity: the panel
    // is see-through and the text on it is not, and one opacity for the whole surface could not
    // say both.
    let side_theme = sidebar(theme);
    fb.fill_rect_alpha(bounds, side_theme.background, OVERVIEW_SIDE_ALPHA);
    // **The shell's first custom node**, and what it draws is the miniature's ground. Bounded by
    // the clip as well as the node's rect: `paint` gives both, and a callback that honoured only
    // the rect would draw a whole miniature over a partly-damaged sidebar.
    paint_over(&mut fb, font, &side_theme, &side, &l, bounds, &mut |kind,
                                                              rect,
                                                              clip,
                                                              fb: &mut MemFramebuffer| {
        if kind != MINI_KIND {
            return;
        }
        let Some((px, g)) = mini.as_ref() else { return };
        for y in 0..g.height {
            for x in 0..g.width {
                let (dx, dy) = (rect.origin.x + x as i32, rect.origin.y + y as i32);
                if dx < clip.origin.x
                    || dy < clip.origin.y
                    || dx >= clip.right() as i32
                    || dy >= clip.bottom() as i32
                {
                    continue;
                }
                let off = y as usize * g.pitch + x as usize * 4;
                let Some(b) = px.get(off..off + 4) else { continue };
                let word = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
                fb.put_pixel(dx as u32, dy as u32, PixelFormat::XRGB8888.decode(word));
            }
        }
    });

    // The thumbnails, blitted straight in: they are already pixels, so there is nothing for the
    // toolkit to lay out and a element per pixel would be absurd.
    for (i, (_, w, h, px)) in shots.iter().enumerate() {
        let (tx, ty, _, _) = thumb_rect(i);
        let pitch = (*w as usize) * 4;
        for y in 0..*h {
            for x in 0..*w {
                let off = y as usize * pitch + x as usize * 4;
                if off + 4 > px.len() {
                    continue;
                }
                let word = u32::from_le_bytes([px[off], px[off + 1], px[off + 2], px[off + 3]]);
                fb.put_pixel(tx + x, ty + y, PixelFormat::XRGB8888.decode(word));
            }
        }
    }
    fb
}

/// The wallpaper scaled to one sidebar miniature's interior, or `None` if it will not scale.
///
/// **Undimmed, unlike the overview's own ground.** The ground is dimmed so the things drawn over
/// it read; a miniature *is* the thing being read, and a dimmed one would be a picture of a
/// desktop nobody has.
fn mini_wallpaper(picture: &[u8]) -> Option<(alloc::vec::Vec<u8>, Geometry)> {
    let src = Geometry::with_pitch(
        SCREEN_W,
        SCREEN_H as u32,
        SCREEN_W as usize * 4,
        PixelFormat::XRGB8888,
    )?;
    let (w, h) = (MINI_W - 2, MINI_H - 2);
    let dst = Geometry::with_pitch(w, h, w as usize * 4, PixelFormat::XRGB8888)?;
    let mut out = alloc::vec![0u8; dst.pitch * h as usize];
    if !libdraw::scale::box_downscale(picture, src, &mut out, dst) {
        // **Said rather than silently fallen back from** (PR #273 review, optional 5). A `None`
        // here puts every miniature back to flat blue — which is precisely the bug this change
        // exists to fix, reappearing with nothing printed and nothing failing. Every other
        // failure around the wallpaper names itself; this one did not.
        kprint(b"desktop-shell: the overview could not scale the wallpaper for a miniature\n");
        return None;
    }
    Some((out, dst))
}

/// Capture a thumbnail of every window on `current`, replacing whatever `shots` held.
///
/// **Minimized windows are not in the overview.** `set_minimized` flips a flag without touching
/// the committed buffer, so a minimized window captures perfectly and would be drawn in the grid
/// exactly like one that is on screen — an overview of "what is on this desktop" showing
/// something that deliberately is not (PR #244 review, optional 7). The bar is where a minimized
/// window is restored from, and it marks them.
///
/// **One function because opening and refreshing must agree.** They were the same loop written
/// once when only opening existed; a refresh that captured a different set would show a desktop
/// nobody could reach by opening it.
fn recapture(
    mgr: &mut ChannelTransport,
    entries: &[WinEntry],
    current: u32,
    shots: &mut alloc::vec::Vec<(u32, u32, u32, alloc::vec::Vec<u8>)>,
) {
    shots.clear();
    for e in visible_entries(entries, current) {
        if e.minimized {
            continue;
        }
        if let Some((w, h, px)) = capture_window(mgr, e.id, e.size) {
            shots.push((e.id, w, h, px));
        }
    }
}

/// Create the overview window and present it. `None` if any step fails.
///
/// **A `popup`, like the applications modal, and for two reasons.** A popup is placed by its
/// creator and is *not* held for the manager — which is what a shell creating a window while
/// holding its own manager channel needs, as Part C learned the hard way. And a popup takes
/// keyboard focus, so Escape closes it; a `panel` never could.
#[allow(clippy::too_many_arguments)]
fn open_overview(
    session: &mut Session<ChannelTransport>,
    parent: u32,
    theme: &Theme,
    font: &Font,
    shots: &[(u32, u32, u32, alloc::vec::Vec<u8>)],
    desktops: &[Desktop],
    current: u32,
    entries: &[WinEntry],
    addrs: &mut [*mut u8; BUFFERS],
    wallpaper: Option<&[u8]>,
) -> Option<u32> {
    let len = OVER_PITCH * SCREEN_H as usize;
    let id = session
        .create(
            &CreateWindowRequest::new(SCREEN_W, SCREEN_H as u32, Role::Popup { parent }),
            BUFFERS,
        )
        .ok()?;
    // **Every failure past `create` destroys the window**, which `open_modal` below has said
    // since PR #237 and this function did not. Returning `None` without it leaves the
    // compositor holding a popup whose id this process has forgotten — never closable, never
    // committable to — while `addrs` keeps a live mapping of an orphaned object that the next
    // present would write through. Repeat past `MAX_WINDOWS_PER_CONNECTION` and the modal stops
    // opening too, because they share the connection (PR #244 review, blocking 3).
    let mut ok = true;
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            ok = false;
            break;
        };
        addrs[i] = addr;
        let Some(mut w) = session.window(id) else {
            ok = false;
            break;
        };
        // **ARGB, which is what makes the overview an overlay** (M13 Part C). Everything else
        // this shell creates is opaque; this one window is composited per pixel so the live
        // desktop shows through its ground.
        if w.attach_with_format(
            i as u32,
            SCREEN_W,
            SCREEN_H as u32,
            OVER_PITCH as u32,
            handle,
            PixelFormat::ARGB8888,
        )
        .is_err()
        {
            ok = false;
            break;
        }
    }
    if !ok {
        for a in addrs.iter_mut() {
            release_buffer(a, len);
        }
        if let Some(w) = session.window(id) {
            let _ = w.destroy();
        }
        kprint(b"desktop-shell: overview buffers FAILED\n");
        return None;
    }
    // **One line whichever way it went**, the shape `read_theme` uses: it says the overview
    // *decided* about a ground without a gate having to assert which way, so "delete the
    // wallpaper and the overview still opens" is a control that runs against the committed gate
    // rather than one that needs a step edited out.
    if wallpaper.is_some() {
        kprint(b"desktop-shell: overview ground is the wallpaper\n");
    } else {
        kprint(b"desktop-shell: overview ground is the desktop colour\n");
    }
    present_overview(session, id, theme, font, shots, desktops, current, entries, addrs, wallpaper);
    Some(id)
}

/// Render the overview into a free buffer and commit it.
#[allow(clippy::too_many_arguments)]
fn present_overview(
    session: &mut Session<ChannelTransport>,
    id: u32,
    theme: &Theme,
    font: &Font,
    shots: &[(u32, u32, u32, alloc::vec::Vec<u8>)],
    desktops: &[Desktop],
    current: u32,
    entries: &[WinEntry],
    addrs: &[*mut u8; BUFFERS],
    wallpaper: Option<&[u8]>,
) {
    let len = OVER_PITCH * SCREEN_H as usize;
    let bytes =
        render_overview(theme, font, shots, desktops, current, entries, wallpaper).into_bytes();
    if bytes.len() != len {
        return;
    }
    let Some(mut w) = session.window(id) else { return };
    let Ok(slot) = w.acquire() else { return };
    let addr = addrs[slot as usize % BUFFERS];
    if addr.is_null() {
        return;
    }
    // SAFETY: `addr` maps `len` writable bytes and `bytes` holds exactly `len`; distinct
    // allocations, so they cannot overlap.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr, len) };
    let _ = w.commit(slot, (0, 0, SCREEN_W, SCREEN_H as u32));
}

/// Destroy the overview and forget what it was showing.
fn close_overview(
    session: &mut Session<ChannelTransport>,
    overview: &mut Option<u32>,
    shots: &mut alloc::vec::Vec<(u32, u32, u32, alloc::vec::Vec<u8>)>,
    dragging: &mut Option<u32>,
    addrs: &mut [*mut u8; BUFFERS],
) {
    if let Some(id) = overview.take() {
        if let Some(w) = session.window(id) {
            let _ = w.destroy();
        }
        // **Unmapped, not merely forgotten.** Destroying the window drops the compositor's
        // side; this process's two 4 MB mappings would otherwise stay for the session's life.
        for a in addrs.iter_mut() {
            release_buffer(a, OVER_PITCH * SCREEN_H as usize);
        }
        shots.clear();
        *dragging = None;
        Line::new().s(b"desktop-shell: overview closed, window ").u(id as u64).end();
    }
}

/// A miniature of one desktop: its ground, with a box where each of its windows is.
///
/// **Rectangles rather than scaled window contents** (M11 Part E batch 10). The maintainer named
/// both and said "whatever is easy"; the difference is not effort but *availability*. A thumbnail
/// is a capture, and the compositor can only capture what it composites — the windows on the
/// desktop being shown. A sidebar is a row per desktop that is *not* being shown, so there is
/// nothing to capture, and asking the compositor to composite an off-screen desktop to photograph
/// it is a different feature from drawing where its windows are.
///
/// What the shell does have is every window's origin, size and desktop, which it keeps for the
/// taskbar. So the *windows* are arithmetic rather than pixels: they need no capture, no scaling
/// and no image decoding — which is what the same request looked like it needed.
///
/// **The ground beneath them is pixels, since 2026-09-02**, and that is the one clause above
/// that stopped being true: a miniature's ground is the wallpaper, box-downscaled once for every
/// row. A preview of a desktop that has a picture, drawn as flat blue, is a preview of a desktop
/// nobody has (PR #273 review, optional 3).
///
/// **Bordered boxes rather than filled ones**, so two overlapping windows read as two.
fn desktop_preview(
    entries: &[WinEntry],
    desktop: u32,
    theme: &Theme,
    wallpaper: bool,
) -> Element<()> {
    // The screen's proportions, so the miniature is the shape of the thing it stands for.
    let (iw, ih) = (MINI_W - 2, MINI_H - 2);
    let mut layers = alloc::vec::Vec::with_capacity(4);
    layers.push(fill(theme.border));
    // **The miniature's ground is what a desktop's ground actually is**, which since M12 Part F
    // is a picture rather than a colour. A preview showing flat blue while the desktop behind it
    // shows a photograph is a preview of something that does not exist — the same complaint the
    // overview's own ground drew, one level down. A `custom` node because the picture is pixels:
    // there is nothing here for the toolkit to lay out, and `render_overview`'s paint callback
    // blits the thumbnail it scaled once.
    layers.push(padding(
        Insets::all(1),
        if wallpaper {
            custom(MINI_KIND, libdraw::geom::Size::new(iw, ih))
        } else {
            fill(theme.desktop)
        },
    ));
    for e in entries.iter().filter(|e| e.desktop == desktop && !e.minimized) {
        // Scaled by the same ratio in both axes as the screen, and clamped into the interior: a
        // window dragged partly off-screen must not draw outside the miniature that stands for
        // the screen.
        let sx = (e.origin.0.max(0) as u32 * iw / SCREEN_W).min(iw.saturating_sub(1));
        let sy = (e.origin.1.max(0) as u32 * ih / SCREEN_H as u32).min(ih.saturating_sub(1));
        // At least two pixels, or the border and the face have nowhere to go and a window
        // vanishes rather than being small.
        let sw = (e.size.0 * iw / SCREEN_W).max(2).min(iw - sx);
        let sh = (e.size.1 * ih / SCREEN_H as u32).max(2).min(ih - sy);
        let face = if e.focused { theme.face_hover } else { theme.face };
        layers.push(offset(
            1 + sx as i32,
            1 + sy as i32,
            sized(
                libdraw::geom::Size::new(sw, sh),
                stack(alloc::vec![fill(theme.border), padding(Insets::all(1), fill(face))]),
            ),
        ));
    }
    sized(libdraw::geom::Size::new(MINI_W, MINI_H), stack(layers))
}

/// The `custom` node a sidebar miniature's ground is drawn as. See [`desktop_preview`].
const MINI_KIND: u32 = 1;

/// A sidebar miniature's size — the screen's 16:10, small enough for a row.
const MINI_W: u32 = 96;
/// See [`MINI_W`].
const MINI_H: u32 = 60;
/// Space around a miniature inside its row.
const MINI_PAD: u32 = 6;

// **The miniature is a size `box_downscale` will accept, and the compiler is what says so.**
// That function refuses a destination larger than the source in either axis, and a refusal here
// puts every preview back to a flat colour — which is precisely the bug this change exists to
// fix, reappearing with nothing failing (PR #273 review, optional 5). A gate line would report
// it; this makes it unbuildable, which is better: raising `MINI_W` past the screen's width is
// then a compile error beside the constant rather than flat blue somebody notices in a month.
const _: () = assert!(
    MINI_W > 2 && MINI_H > 2 && MINI_W - 2 <= SCREEN_W && MINI_H - 2 <= SCREEN_H as u32,
    "a sidebar miniature must be smaller than the screen it is a miniature of"
);

/// The overview's sidebar width, at the right-hand edge.
const SIDE_W: u32 = 200;
/// One desktop row in the sidebar.
///
/// **Tall enough for a miniature** since M11 Part E batch 10: `MINI_H` plus `MINI_PAD` on each
/// side. `check-login` clicks a row by index and computes the same arithmetic, so this number is
/// in two places and the gate names which (that is the cost of a click point a gate can aim at,
/// and M11's decision 2 chose it deliberately).
const SIDE_ROW_H: u32 = MINI_H + MINI_PAD * 2;
/// A thumbnail's size in the overview's grid.
const THUMB_W: u32 = 240;
/// See [`THUMB_W`].
const THUMB_H: u32 = 150;
/// Space around each thumbnail.
const THUMB_PAD: u32 = 16;
/// How many thumbnails fit across the grid.
const THUMB_COLS: u32 = (SCREEN_W - SIDE_W) / (THUMB_W + THUMB_PAD);
/// Bytes per row of the overview's own buffer.
const OVER_PITCH: usize = (SCREEN_W as usize) * 4;

/// How far the wallpaper is darkened under the overview, as a coverage of black.
///
/// **Enough that light ink and pale thumbnails read over a photograph**, and not so much that
/// the picture stops being recognisable — the point is that the desktop is still *there*. A
/// number rather than a theme key: it is the overview's own composition, like the sidebar's
/// panel, and M11's decision 2 keeps chrome metrics out of what a theme file can set.
/// The overview's ground: this colour, at [`OVERVIEW_GROUND_ALPHA`], over the live desktop.
const OVERVIEW_GROUND: libdraw::format::Rgb = libdraw::format::Rgb::new(0, 0, 0);

/// How opaque the overview's ground is — the desktop shows through the remainder.
///
/// **Dark enough that a thumbnail reads against it**, which is the constraint: the overview's job
/// is to show windows, and a ground you can see the real desktop through too clearly puts a second
/// copy of every window behind its own thumbnail.
const OVERVIEW_GROUND_ALPHA: u8 = 210;

/// How opaque the sidebar's ground is.
///
/// **Less than [`OVERVIEW_GROUND_ALPHA`], deliberately.** A panel that is *more* see-through than
/// the ground it sits on reads as a lighter sheet laid over it, which is what a sidebar is; the
/// other way round it would read as a hole.
const OVERVIEW_SIDE_ALPHA: u8 = 150;

/// Where thumbnail `i` sits in the overview, in overview-local pixels.
///
/// **One function for drawing and for hit-testing**, which is the lesson the bottom bar's
/// indicator taught: a hit region computed separately from the layout is right at one window
/// count and wrong everywhere else (PR #243 review, blocking 2).
fn thumb_rect(i: usize) -> (u32, u32, u32, u32) {
    let col = (i as u32) % THUMB_COLS;
    let row = (i as u32) / THUMB_COLS;
    let x = THUMB_PAD + col * (THUMB_W + THUMB_PAD);
    let y = BAR_H + THUMB_PAD + row * (THUMB_H + THUMB_PAD);
    (x, y, THUMB_W, THUMB_H)
}

/// Which sidebar row a point is in, if any.
fn side_row_at(x: i32, y: i32, rows: usize) -> Option<usize> {
    if x < (SCREEN_W - SIDE_W) as i32 || y < BAR_H as i32 {
        return None;
    }
    let i = ((y as u32 - BAR_H) / SIDE_ROW_H) as usize;
    (i < rows).then_some(i)
}

/// Which thumbnail a point is in, if any.
fn thumb_at(x: i32, y: i32, n: usize) -> Option<usize> {
    if x < 0 || y < 0 {
        return None;
    }
    (0..n).find(|&i| {
        let (tx, ty, tw, th) = thumb_rect(i);
        x >= tx as i32 && x < (tx + tw) as i32 && y >= ty as i32 && y < (ty + th) as i32
    })
}

/// Ask the compositor to scale `window` into a fresh buffer, and return the pixels.
///
/// **The manager allocates**, which is the mirror of a client attaching a buffer the compositor
/// reads. Clamped to the window's own size, because a capture may not scale *up*: a window
/// smaller than the grid cell is captured at its own size and drawn smaller.
fn capture_window(
    mgr: &mut ChannelTransport,
    window: u32,
    size: (u32, u32),
) -> Option<(u32, u32, alloc::vec::Vec<u8>)> {
    use librsproto::surface::{MgrCapture, OP_MGR_CAPTURE};
    let w = THUMB_W.min(size.0.max(1));
    let h = THUMB_H.min(size.1.max(1));
    let pitch = (w as usize) * 4;
    let len = pitch * h as usize;
    let (handle, addr) = shared_buffer(len)?;
    let req = MgrCapture { window, width: w, height: h, pitch: pitch as u32 };
    let mut body = [0u8; core::mem::size_of::<MgrCapture>()];
    if req.write(&mut body).is_none() {
        return None;
    }
    let mut reply = [0u8; 64];
    let ok = mgr.request(OP_MGR_CAPTURE, &body, Some(handle), &mut reply).is_ok();
    if !ok {
        Line::new().s(b"desktop-shell: Capture refused for window ").u(window as u64).end();
        return None;
    }
    // SAFETY: `addr` maps `len` readable bytes the compositor has just written.
    let px = unsafe { core::slice::from_raw_parts(addr, len) }.to_vec();
    // **A reply is not an effect, and here the difference is invisible from outside.** The
    // compositor logs a successful capture whether or not the scale wrote anything, and the
    // overview would show a black rectangle that no serial gate could tell from a dark window.
    // `shared_buffer` hands back zeroed memory, so "some pixel is non-zero" is exactly the
    // question, and only the process holding the buffer can ask it (PR #216's rule, and the
    // control that caught this one writing nothing).
    // The copy is taken, so the mapping has done its job. Six of these per overview open at up
    // to 144 KB each, and nothing else would ever reclaim them.
    let mut addr = addr;
    release_buffer(&mut addr, len);
    let lit = px.chunks_exact(4).filter(|c| c != &[0, 0, 0, 0]).count();
    if lit == 0 {
        Line::new()
            .s(b"desktop-shell: capture of window ")
            .u(window as u64)
            .s(b" came back blank")
            .end();
        return None;
    }
    Line::new()
        .s(b"desktop-shell: thumbnail of window ")
        .u(window as u64)
        .s(b" has ")
        .u(lit as u64)
        .s(b" painted pixels")
        .end();
    Some((w, h, px))
}

/// Serve one request on an open `/dev/desktop` session. Returns whether the shell must redraw.
///
/// **The one place another process changes the desktop model**, and it goes through exactly the
/// operations the shell's own chords do — `switch_desktop` and the name field — rather than
/// touching the lists directly. A second path into the same state is a second place for the
/// lifecycle rule to be forgotten.
fn serve_desktop_session(
    ch: u64,
    mgr: Option<&mut ChannelTransport>,
    desktops: &mut alloc::vec::Vec<Desktop>,
    entries: &[WinEntry],
    current: &mut u32,
    next_id: &mut u32,
    launcher: &Launcher<'_>,
) -> bool {
    use librsproto::decode;
    use librsproto::desktop::{
        DesktopEntry, DesktopIndex, MAX_DESKTOP_NAME, MAX_LISTED, MAX_OPEN_PATH, OP_DESKTOP_LIST,
        OP_DESKTOP_NAME, OP_DESKTOP_OPEN, OP_DESKTOP_SWITCH, write_list,
    };
    // SAFETY: valid recv out-params, sized from `IPC_HANDLE_MAX`.
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            ch,
            (&raw mut DS_MSG) as u64,
            (&raw mut DS_HANDLES) as u64,
            (&raw mut DS_COUNT) as u64,
        )
    };
    // **Close whatever came with it.** No desktop op takes a handle, but that is a property of
    // the *peer* rather than of this server — and a SAFETY comment that assumes it was the other
    // half of blocking 2. The kernel installs whatever the sender attached into this process's
    // table whether or not anything here looks at it, so left alone they pin slots in the global
    // handle table for the shell's life. `serve_manager` in the compositor states the same rule.
    // SAFETY: closing handles the kernel just installed for us.
    unsafe {
        let n = ((&raw const DS_COUNT).read()).min(libkern::abi::IPC_HANDLE_MAX);
        for i in 0..n {
            syscall1(SYS_HANDLE_CLOSE, DS_HANDLES[i]);
        }
    }
    if rr != 0 {
        if rr == KError::PeerClosed.as_i32() as i64 {
            // SAFETY: the peer is gone; free the slot and close our end.
            unsafe {
                for i in 0..MAX_DESKTOP_SESSIONS {
                    if DESKTOP_SESSIONS[i] == ch {
                        DESKTOP_SESSIONS[i] = 0;
                    }
                }
                syscall1(SYS_HANDLE_CLOSE, ch);
            }
        }
        return false;
    }
    // SAFETY: bounded read-only slice over the message just received.
    let (op, request_id, body) = unsafe {
        let payload_len = u32::from_le_bytes([DS_MSG[4], DS_MSG[5], DS_MSG[6], DS_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const DS_MSG) as *const u8).add(24),
            payload_len.min(4096 - 24),
        );
        match decode(req) {
            Ok(m) => (m.op, m.request_id, m.body.to_vec()),
            Err(_) => return false,
        }
    };
    let bad = |code: KError| {
        let mut b = [0u8; 4];
        b.copy_from_slice(&code.as_i32().to_le_bytes());
        ds_reply(ch, op, request_id, &b, 0, true);
        false
    };
    match op {
        OP_DESKTOP_LIST => {
            let cur = desktops.iter().position(|d| d.id == *current).map(|i| i + 1).unwrap_or(0);
            let listed: alloc::vec::Vec<DesktopEntry<'_>> = desktops
                .iter()
                .take(MAX_LISTED)
                .map(|d| DesktopEntry { id: d.id, name: d.name.as_str() })
                .collect();
            let mut out = [0u8; 1024];
            let Some(n) =
                write_list(&mut out, cur as u32, &listed, desktops.len() > MAX_LISTED)
            else {
                // Server-side data this server cannot encode — not a kernel fault, which is
                // what `KernelError` claims and what a caller would chase.
                kprint(b"desktop-shell: a desktop list would not serialise\n");
                return bad(KError::InvalidArgument);
            };
            ds_reply(ch, op, request_id, &out[..n], 0, false);
            Line::new().s(b"desktop-shell: served List of ").u(desktops.len() as u64).end();
            false
        }
        OP_DESKTOP_SWITCH => {
            let Some(req) = DesktopIndex::read(&body) else { return bad(KError::InvalidArgument) };
            let Some(d) = (req.index as usize).checked_sub(1).and_then(|i| desktops.get(i)) else {
                return bad(KError::NotFound);
            };
            let to = d.id;
            let Some(m) = mgr else { return bad(KError::Unsupported) };
            if !switch_desktop(m, desktops, current, to) {
                return bad(KError::InvalidArgument);
            }
            normalize_desktops(desktops, entries, current, next_id);
            ds_reply(ch, op, request_id, &[], 0, false);
            kprint(b"desktop-shell: served Switch\n");
            true
        }
        OP_DESKTOP_NAME => {
            let Some(req) = DesktopIndex::read(&body) else { return bad(KError::InvalidArgument) };
            let Ok(name) = core::str::from_utf8(&body[4..]) else {
                return bad(KError::InvalidArgument);
            };
            if name.len() > MAX_DESKTOP_NAME {
                return bad(KError::InvalidArgument);
            }
            let Some(d) = (req.index as usize).checked_sub(1).and_then(|i| desktops.get_mut(i))
            else {
                return bad(KError::NotFound);
            };
            d.name.clear();
            d.name.push_str(name);
            normalize_desktops(desktops, entries, current, next_id);
            ds_reply(ch, op, request_id, &[], 0, false);
            Line::new().s(b"desktop-shell: served Name ").untrusted(name.as_bytes()).end();
            true
        }
        OP_DESKTOP_OPEN => {
            // **The client names a path; the shell decides what runs.** An application holds no
            // authority to spawn — it has no `/bin` and no way to build a namespace — so
            // "open this" is a question rather than an instruction, and the answer is the
            // shell's policy. A request that named the *program* would be ambient authority
            // wearing a protocol.
            let Ok(path) = core::str::from_utf8(&body) else {
                return bad(KError::InvalidArgument);
            };
            if path.is_empty() || path.len() > MAX_OPEN_PATH || !path.starts_with('/') {
                // **Absolute only.** A relative path would be relative to *something*, and the
                // only candidate is a working directory this shell does not have and the
                // caller's namespace does not share.
                return bad(KError::InvalidArgument);
            }
            // **And nothing bounds how many times this may be asked.** `Open` is the first
            // spawn path a *program* can drive — the modal needs a person — and the handler
            // checks the path's shape and launches. `MAX_DESKTOP_SESSIONS` is not the bound
            // (`nxfiles` opens a session per file, so a per-session counter resets every call),
            // and this shell does not reap what it launches, so it cannot count what is alive.
            // TODO(open-amplification): bound this once the shell has that record.
            //
            // **Not stat'ed here, deliberately.** The shell could ask whether the path is a
            // file, and the answer would be about the *shell's* namespace rather than the
            // caller's or the editor's — three namespaces that agree today because one process
            // builds all three, which is exactly the kind of agreement not to lean on. What
            // opens the path reports what it found, in the window the person is looking at.
            if !launcher.launch(OPENER, &[path]) {
                return bad(KError::Unsupported);
            }
            ds_reply(ch, op, request_id, &[], 0, false);
            Line::new().s(b"desktop-shell: served Open ").untrusted(path.as_bytes()).end();
            false
        }
        _ => bad(KError::Unsupported),
    }
}

/// What opens a path.
///
/// **One program, and a constant rather than a table, because there is one.** Dispatching on an
/// extension is what this grows into the first time a second program can open something — an
/// image viewer, a `.tsm` table — and writing the table now would be writing a mechanism with
/// one entry and no second case to check it against. M12 names the applications that give it
/// one.
const OPENER: &str = "nxedit";

/// Answer one message on the desktop endpoint's serve end — a resolve, minting a session.
///
/// **The shell is a resource server here, which is the thing `graphical-session.md` §3 had to
/// reconcile.** It does not *register* itself: nothing binds this endpoint into a namespace a
/// supervisor owns. It binds it into the namespaces it **constructs** for the applications it
/// launches, which is the constructor role it already holds `BIND_NAMESPACE` for.
fn serve_desktop_endpoint() {
    use librsproto::namespace::{OBJECT_KIND_CHANNEL, parse_resolve_request, resolve_reply};
    use librsproto::{OP_NS_RESOLVE, decode};
    // SAFETY: reading our own serve end into valid out-params.
    let serve = unsafe { DESKTOP_SERVE };
    let rr = unsafe {
        syscall4(
            SYS_CHANNEL_RECV,
            serve,
            (&raw mut DS_MSG) as u64,
            (&raw mut DS_HANDLES) as u64,
            (&raw mut DS_COUNT) as u64,
        )
    };
    if rr != 0 {
        return;
    }
    // SAFETY: bounded read-only slice over the message just received.
    let (op, request_id, bare) = unsafe {
        let payload_len = u32::from_le_bytes([DS_MSG[4], DS_MSG[5], DS_MSG[6], DS_MSG[7]]) as usize;
        let req = core::slice::from_raw_parts(
            ((&raw const DS_MSG) as *const u8).add(24),
            payload_len.min(4096 - 24),
        );
        match decode(req) {
            // **An empty suffix only.** `/dev/desktop` is the resource itself; a suffix names
            // something beneath it, and the per-object paths composition §2a sketches are not
            // built — so answering one would invent a second level this server does not have.
            Ok(m) if m.op == OP_NS_RESOLVE => match parse_resolve_request(m.body) {
                Some(r) if r.suffix.is_empty() => (m.op, m.request_id, true),
                _ => (m.op, m.request_id, false),
            },
            Ok(m) => (m.op, m.request_id, false),
            Err(_) => return,
        }
    };
    if !bare {
        let mut b = [0u8; 8];
        b[..4].copy_from_slice(&KError::NotFound.as_i32().to_le_bytes());
        ds_reply(serve, op, request_id, &b[..4], 0, true);
        return;
    }
    // SAFETY: single-threaded scan of our own session table.
    let slot = unsafe { (0..MAX_DESKTOP_SESSIONS).find(|&i| DESKTOP_SESSIONS[i] == 0) };
    let Some(slot) = slot else {
        let mut b = [0u8; 4];
        b.copy_from_slice(&KError::WouldBlock.as_i32().to_le_bytes());
        ds_reply(serve, op, request_id, &b, 0, true);
        return;
    };
    let Some((client_end, session_end)) = make_channel() else {
        let mut b = [0u8; 4];
        b.copy_from_slice(&KError::KernelError.as_i32().to_le_bytes());
        ds_reply(serve, op, request_id, &b, 0, true);
        return;
    };
    // Bound before replying, so a fast client's first request cannot arrive before the slot is
    // live — the same ordering `auth-service` states.
    // SAFETY: `slot` is free.
    unsafe { DESKTOP_SESSIONS[slot] = session_end };
    let mut body = [0u8; 32];
    let n = resolve_reply(&mut body, OBJECT_KIND_CHANNEL, 0).unwrap_or(0);
    if !ds_reply(serve, op, request_id, &body[..n], client_end, false) {
        // SAFETY: the transfer failed, so both ends are still ours.
        unsafe {
            DESKTOP_SESSIONS[slot] = 0;
            syscall1(SYS_HANDLE_CLOSE, session_end);
            syscall1(SYS_HANDLE_CLOSE, client_end);
        }
        return;
    }
    kprint(b"desktop-shell: /dev/desktop session opened\n");
}

/// How many clients may hold a `/dev/desktop` session at once.
///
/// Small on purpose: the only consumer is a short-lived command, and every slot costs a waiter
/// in a set the compositor session and the manager channel are also in.
const MAX_DESKTOP_SESSIONS: usize = 4;

/// The desktop endpoint's serve end, and its open sessions.
static mut DESKTOP_SERVE: u64 = 0;
/// See [`DESKTOP_SERVE`].
static mut DESKTOP_SESSIONS: [u64; MAX_DESKTOP_SESSIONS] = [0; MAX_DESKTOP_SESSIONS];
static mut DS_MSG: [u8; 4096] = [0; 4096];
/// Sized from the **ABI**, not from what this server expects to receive.
///
/// `sys_channel_recv` passes no receiver-side capacity: the kernel copies out `n * 8` bytes
/// where `n` is the *sender's* stamped count, bounded only by `IPC_HANDLE_MAX`. At four this
/// was a 32-byte static the kernel would write 64 bytes into — and `/dev/desktop` is bound into
/// **every** application namespace this shell constructs, so any client sending a request with
/// eight handles attached smashes whatever `.bss` follows. No bug in the client is needed; a
/// careless one does it (PR #245 review, blocking 2). Every other server in the tree sizes this
/// `[u64; IPC_HANDLE_MAX]` for exactly this reason.
static mut DS_HANDLES: [u64; libkern::abi::IPC_HANDLE_MAX] = [0; libkern::abi::IPC_HANDLE_MAX];
static mut DS_COUNT: usize = 0;
static mut DS_REPLY: [u8; 4096] = [0; 4096];
static mut DS_REPLY_HANDLES: [u64; 4] = [0; 4];
static mut CH_OUT0: u64 = 0;
static mut CH_OUT1: u64 = 0;

/// Create a channel pair. `(client_end, server_end)`.
fn make_channel() -> Option<(u64, u64)> {
    // SAFETY: CH_OUT0/CH_OUT1 are valid writable out-params.
    let cr = unsafe {
        syscall4(SYS_CHANNEL_CREATE, (&raw mut CH_OUT0) as u64, (&raw mut CH_OUT1) as u64, 4, 0)
    };
    if cr != 0 {
        return None;
    }
    // SAFETY: on success the kernel wrote both endpoint handles.
    Some(unsafe { ((&raw const CH_OUT0).read(), (&raw const CH_OUT1).read()) })
}

/// Send a reply on `ch` for `request_id`, optionally transferring `handle`.
fn ds_reply(ch: u64, op: u16, request_id: u64, body: &[u8], handle: u64, err: bool) -> bool {
    use librsproto::{RS_FLAG_ERROR, RS_FLAG_REPLY};
    let flags = if err { RS_FLAG_REPLY | RS_FLAG_ERROR } else { RS_FLAG_REPLY };
    let hcount = u64::from(handle != 0);
    // SAFETY: DS_REPLY/DS_REPLY_HANDLES are valid buffers owned by this process.
    unsafe {
        let Some(rs_len) =
            librsproto::encode(&mut DS_REPLY[24..], op, request_id, flags, body, hcount as u16)
        else {
            return false;
        };
        DS_REPLY[4..8].copy_from_slice(&(rs_len as u32).to_le_bytes());
        DS_REPLY[8] = hcount as u8;
        DS_REPLY_HANDLES[0] = handle;
        syscall5(
            SYS_CHANNEL_SEND,
            ch,
            (&raw const DS_REPLY) as u64,
            (&raw const DS_REPLY_HANDLES) as u64,
            hcount,
            SENDMODE_NOBLOCK,
        ) == 0
    }
}

/// Tell the compositor which desktop to composite, and say so.
///
/// **The only thing the compositor is told about desktops.** Which ones exist, what they are
/// called and when they disappear never leaves this process — `ui-composition-model.md` §6's
/// split, and the reason the two cannot come to disagree.
fn switch_desktop(
    mgr: &mut ChannelTransport,
    desktops: &[Desktop],
    current: &mut u32,
    to: u32,
) -> bool {
    use librsproto::surface::{MgrDesktop, OP_MGR_SET_CURRENT_DESKTOP};
    if to == *current || !desktops.iter().any(|d| d.id == to) {
        return false;
    }
    let mut body = [0u8; core::mem::size_of::<MgrDesktop>()];
    if (MgrDesktop { desktop: to }).write(&mut body).is_none() {
        kprint(b"desktop-shell: a SetCurrentDesktop body would not serialise\n");
        return false;
    }
    let mut reply = [0u8; 64];
    if mgr.request(OP_MGR_SET_CURRENT_DESKTOP, &body, None, &mut reply).is_err() {
        kprint(b"desktop-shell: SetCurrentDesktop was refused\n");
        return false;
    }
    *current = to;
    Line::new()
        .s(b"desktop-shell: switched to ")
        .untrusted(desktop_label(desktops, to).as_bytes())
        .end();
    true
}

/// Log the window list, so a gate can read what the bar is showing.
///
/// **The bar's contents reach a gate this way and no other.** What it draws is pixels in a
/// window on a release image, and the only gate that boots one is `check-login`, which reads
/// the serial console. A line per change is what makes "the list has this window, focused"
/// assertable at all.
fn log_window_list(shown: &[&WinEntry], label: &str, desktops: usize) {
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

/// Where a dialog of `size` goes over `base`, kept inside the work area.
///
/// **Centred on its parent, then clamped**, in that order: a dialog wider than the window it
/// belongs to would otherwise hang off the screen's edge with half its buttons unreachable, and
/// a dialog is exactly the window a person must be able to answer. One that will not fit at all
/// is put at the work area's origin rather than negative, so what is lost is its bottom-right
/// rather than its title bar.
///
/// `base` is `(x, y, w, h)` — the parent's rectangle, or the work area when there is no parent
/// this shell knows about.
fn centre_dialog(base: (i32, i32, u32, u32), size: (u32, u32), work: &MgrLayout) -> (i32, i32) {
    let x = base.0.saturating_add((base.2 as i32 - size.0 as i32) / 2);
    let y = base.1.saturating_add((base.3 as i32 - size.1 as i32) / 2);
    // `.max(0)` keeps the clamp's bounds ordered when the dialog is larger than the work area;
    // `clamp` panics on an inverted range, and this file's whole stance is that a shell must
    // not be the thing that dies.
    let max_x = work.work_x.saturating_add((work.work_w as i32 - size.0 as i32).max(0));
    let max_y = work.work_y.saturating_add((work.work_h as i32 - size.1 as i32).max(0));
    (x.clamp(work.work_x, max_x), y.clamp(work.work_y, max_y))
}

/// Drain the manager channel and place every window it announces.
///
/// **Placing is what releases a held window.** With a manager attached the compositor holds a
/// `normal` window's first `Configure` until the manager acts, so a shell that received
/// `WindowCreated` and did nothing would leave every launched application invisible — a
/// failure that looks like the application never started.
fn place_new_windows(
    mgr: &mut ChannelTransport,
    next_origin: &mut i32,
    entries: &mut alloc::vec::Vec<WinEntry>,
    ours: &[u32],
    fired: &mut alloc::vec::Vec<u32>,
    layout: &mut MgrLayout,
    states: &mut alloc::vec::Vec<librsproto::surface::WindowState>,
    dropped: &mut alloc::vec::Vec<librsproto::surface::ConfigureEvent>,
    restore: &mut alloc::vec::Vec<(u32, (i32, i32, u32, u32))>,
    current: u32,
) -> bool {
    use librsproto::surface::{
        FocusEvent, MgrHotkey, MgrPlace, MgrWindowCreated, MgrWindowRef, OP_MGR_HOTKEY,
        OP_MGR_LAYOUT_CHANGED, OP_MGR_PLACE, OP_MGR_WINDOW_CREATED, OP_MGR_WINDOW_DESTROYED,
        OP_MGR_DRAG_ENDED, OP_MGR_WINDOW_FOCUS, OP_MGR_WINDOW_GEOMETRY,
        OP_MGR_WINDOW_STATE_REQUEST, OP_MGR_WINDOW_TITLE, ROLE_DIALOG, ROLE_NORMAL,
    };
    let mut dirty = false;
    // **Four bytes more than `MAX_TITLE`, which is what a title record actually is.** A
    // `WindowTitle` body is `4 + title.len()` and the compositor stores up to `MAX_TITLE`, so a
    // 256-byte buffer truncated the longest titles by four bytes — and a cut through a
    // multi-byte character makes `title::read` return `None`, so the arm did nothing and the
    // entry kept a stale label for the life of the window, since titles are re-sent only on
    // change (PR #242 review, finding 6).
    let mut buf = [0u8; 4 + librsproto::surface::MAX_TITLE];
    // Zero timeout: drain what is queued and return. The outer `sys_wait` is what blocks.
    while let Ok(Some((op, n))) = mgr.wait_event_timeout(&mut buf, 0) {
        // **The other three events, which this loop used to discard.** They are exactly the
        // facts a window list shows, and reading them here is what keeps the shell's copy from
        // being a second stack that can disagree with the compositor's.
        match op {
            OP_MGR_WINDOW_DESTROYED => {
                // **The restore rectangle goes with the window.** Ids are never reused, so a
                // stale entry can never be *mistaken* for another window — but it is never
                // collected either, and a client looping create → maximise → destroy would grow
                // this vector for the life of the session. The window cap bounds concurrent
                // windows, not the number that have ever existed (PR #249 review, finding 3).
                if let Some(r) = MgrWindowRef::read(&buf[..n]) {
                    restore.retain(|(id, _)| *id != r.window);
                    let before = entries.len();
                    entries.retain(|e| e.id != r.window);
                    dirty |= entries.len() != before;
                }
                continue;
            }
            OP_MGR_LAYOUT_CHANGED => {
                // A strut appeared, went away or moved. Nothing is re-laid out here: what this
                // changes is where the *next* maximise puts a window, and a shell that resized
                // every maximised window under the user would be making a decision the user did
                // not ask for.
                if let Some(l) = MgrLayout::read(&buf[..n]) {
                    *layout = l;
                    // **The zones are the work area**, so a strut appearing makes all eight
                    // wrong at once — and a shell that registered them once at startup would
                    // snap windows over its own bars for the rest of the session (M9 Part F).
                    register_snap_zones(mgr, &l);
                    Line::new()
                        .s(b"desktop-shell: work area now ")
                        .i(l.work_x as i64)
                        .s(b",")
                        .i(l.work_y as i64)
                        .s(b" ")
                        .u(l.work_w as u64)
                        .s(b"x")
                        .u(l.work_h as u64)
                        .end();
                }
                continue;
            }
            OP_MGR_WINDOW_STATE_REQUEST => {
                // **Collected, not acted on here**, the way a hotkey is: acting means sending
                // manager requests and remembering a rectangle to restore to, and both belong
                // where the shell's own state lives rather than inside a drain.
                if let Some(s) = librsproto::surface::WindowState::read(&buf[..n]) {
                    states.push(s);
                }
                continue;
            }
            OP_MGR_DRAG_ENDED => {
                // **The one event a whole gesture produces** (M9 Parts E and F). Two produce it
                // — a resize, and a move released in a snap zone — and they mean the same thing
                // here: a rectangle somebody asked for. The compositor ran the drag and drew
                // the outline; it changed no geometry, because that is the manager's — so this
                // is a request and the answer is the `Configure` this shell would have sent
                // anyway. Collected rather than answered here for the reason a state request
                // is: sending manager requests belongs where the shell's own state lives.
                if let Some(c) = librsproto::surface::ConfigureEvent::read(&buf[..n]) {
                    dropped.push(c);
                }
                continue;
            }
            OP_MGR_WINDOW_FOCUS => {
                if let Some(f) = FocusEvent::read(&buf[..n]) {
                    // Focus is exclusive, so a gain clears every other entry's flag rather
                    // than trusting a matching loss to arrive.
                    let has = f.focused != 0;
                    for e in entries.iter_mut() {
                        let was = e.focused;
                        e.focused = has && e.id == f.window;
                        dirty |= was != e.focused;
                    }
                }
                continue;
            }
            OP_MGR_WINDOW_GEOMETRY => {
                // A window's rectangle changes for more than one reason — a manager `Place`,
                // and a client committing a buffer of a different size — and this event
                // promises to report it for any of them. The overview needs the current size
                // to clamp a capture that may not scale up.
                if let Some(g) = librsproto::surface::ConfigureEvent::read(&buf[..n])
                    && let Some(e) = entries.iter_mut().find(|e| e.id == g.window)
                {
                    e.size = (g.width, g.height);
                    e.origin = (g.x, g.y);
                    // **Where it ended up, said once per move and no more than sixteen times.**
                    // A user-dragged window reports exactly one geometry event for the whole
                    // gesture — the compositor does not log one per motion, because that queue
                    // does not coalesce — so this is one line per move rather than one per
                    // frame, and it is the only place a gate can read where a drag put a window.
                    //
                    // Bounded because the *event* is client-driven as well: a client committing
                    // buffers of changing size produces one each, and the compositor caps its
                    // own diagnostics for exactly that reason (PR #248 review, finding 5).
                    // SAFETY: single-threaded process; this counter is touched only here, the
                    // way the compositor's own diagnostic counters are.
                    let seen = unsafe {
                        GEOMETRY_LOGGED += 1;
                        GEOMETRY_LOGGED
                    };
                    if seen > MAX_LOGGED_GEOMETRY {
                        continue;
                    }
                    Line::new()
                        .s(b"desktop-shell: window ")
                        .u(g.window as u64)
                        .s(b" geometry ")
                        .i(g.x as i64)
                        .s(b",")
                        .i(g.y as i64)
                        .s(b" ")
                        .u(g.width as u64)
                        .s(b"x")
                        .u(g.height as u64)
                        .end();
                }
                continue;
            }
            OP_MGR_WINDOW_TITLE => {
                if let Some((id, title)) = librsproto::surface::title::read(&buf[..n])
                    && let Some(e) = entries.iter_mut().find(|e| e.id == id)
                    && e.title != title
                {
                    e.title.clear();
                    e.title.push_str(title);
                    dirty = true;
                }
                continue;
            }
            OP_MGR_HOTKEY => {
                // **Collected here rather than in a second drain of the same channel**, which
                // is how the first version lost every chord: `take_hotkeys` ran after this
                // function and found nothing, because this loop had already read the event and
                // fallen through its `_ => continue`. One channel wants one reader — a second
                // does not see what the first consumed, and says nothing about it.
                if let Some(hk) = MgrHotkey::read(&buf[..n]) {
                    fired.push(hk.id);
                }
                continue;
            }
            OP_MGR_WINDOW_CREATED => {}
            _ => continue,
        }
        let Some(created) = MgrWindowCreated::read(&buf[..n]) else {
            continue;
        };
        // **A dialog is placed and not listed** (M12 Part A). It is *held* for a manager
        // exactly as a `normal` is — `rsproto-surface-ops.md` says so outright — so a shell
        // that ignored one would leave every dialog waiting out the compositor's 200 ms
        // deadline and then appearing wherever the client happened to ask, which for a client
        // that cannot know where it is means the top-left corner of the screen.
        //
        // **Centred on its parent**, which is the placement the spec says a manager can work
        // out for itself: `WindowCreated` carries the parent id and the requested size, and the
        // geometry stream has been telling this shell where every window is all along. A parent
        // it does not know — a dialog on one of the shell's own windows, or on a `popup` —
        // centres on the work area instead, which is the honest answer to "over what?".
        //
        // **And it does not join `entries`.** A taskbar slot for a dialog would offer to close
        // or minimise it on its own, and a question minimised behind its window is a window
        // that cannot be closed and will not say why. Its parent's slot is the one that stands
        // for both.
        if created.role == ROLE_DIALOG && !ours.contains(&created.window) {
            let base = entries
                .iter()
                .find(|e| e.id == created.aux32)
                .map(|e| (e.origin.0, e.origin.1, e.size.0, e.size.1))
                .unwrap_or((layout.work_x, layout.work_y, layout.work_w, layout.work_h));
            let (x, y) = centre_dialog(base, (created.width, created.height), layout);
            let place = MgrPlace { window: created.window, x, y };
            // Sized from the type, not from a literal: `compositor::send_mgr_event` states that
            // rule after a hand-written `[0u8; 16]` silently dropped every widened
            // `PointerEvent`, and copying the number from the placement below would be the same
            // shape waiting for the same widening (PR #267 review, optional 7).
            let mut body = [0u8; core::mem::size_of::<MgrPlace>()];
            if place.write(&mut body).is_none() {
                continue;
            }
            let mut reply = [0u8; 64];
            if mgr.request(OP_MGR_PLACE, &body, None, &mut reply).is_err() {
                Line::new()
                    .s(b"desktop-shell: Place refused for dialog ")
                    .u(created.window as u64)
                    .end();
                continue;
            }
            // **Where it went, because a gate has no other way to find it.** `check-login`
            // presses both of a confirmation's buttons, and it aims from this origin — the
            // alternative is re-deriving this centring in the harness, which would be a second
            // copy of a policy that is the shell's to change (M10 Part E's own rule).
            Line::new()
                .s(b"desktop-shell: placed dialog ")
                .u(created.window as u64)
                .s(b" of window ")
                .u(created.aux32 as u64)
                .s(b" at ")
                .i(x as i64)
                .s(b",")
                .i(y as i64)
                .s(b" ")
                .u(created.width as u64)
                .s(b"x")
                .u(created.height as u64)
                .end();
            continue;
        }
        // **Only `normal` windows, and none of the shell's own.** A bar is a `panel` and the
        // modal is a `popup`, so the role filter covers those — but an application's own popup
        // is a `popup` too, and listing one would put a menu in the taskbar. The id check is
        // belt and braces for anything the shell creates that is `normal` later.
        if created.role != ROLE_NORMAL || ours.contains(&created.window) {
            // **And it is not placed either, which the first version of this got wrong.** Every
            // window created while a manager is attached is announced to it, the shell's own
            // included — so the cascade moved the bottom bar it had just placed at the foot of
            // the screen up to `0,24`, under the top bar. Only `normal` windows are held for a
            // manager and only they want placing: a `panel` positions itself, and a `popup` is
            // placed by its creator (M6 Part C1).
            continue;
        }
        entries.push(WinEntry {
            id: created.window,
            title: alloc::string::String::new(),
            focused: false,
            minimized: false,
            // Placed below, and reported back by the geometry event that placement produces.
            origin: (0, 0),
            size: (created.width, created.height),
            // The compositor creates a window onto *its* current desktop, and the shell is what
            // set that — so this is not a guess, it is the same number read from the other side.
            desktop: current,
        });
        dirty = true;
        // **Inset from the left edge, not flush against it** (M11 Part E batch 4). The cascade
        // stepped down from the bar and started at x=0, so a first window sat with its frame on
        // the screen's border — which reads as a window that has been shoved rather than placed.
        // One step in, so the offset matches the one the cascade already uses downward.
        let (x, y) = (CASCADE_STEP, *next_origin);
        // Wrapped, or the 34th window is placed below an 800px screen and never seen.
        *next_origin += CASCADE_STEP;
        if *next_origin > SCREEN_H - CASCADE_STEP {
            // Back to where the cascade starts, which is one step below the bar rather than
            // against it — the same inset the first window gets.
            *next_origin = BAR_H as i32 + CASCADE_STEP;
        }
        let place = MgrPlace { window: created.window, x, y };
        let mut body = [0u8; 12];
        if place.write(&mut body).is_none() {
            continue;
        }
        let mut reply = [0u8; 64];
        if mgr.request(OP_MGR_PLACE, &body, None, &mut reply).is_err() {
            Line::new()
                .s(b"desktop-shell: Place refused for window ")
                .u(created.window as u64)
                .end();
            continue;
        }
        // **`x`, not a literal zero.** This said `at 0,` because the cascade always started at
        // the left edge — a value hardcoded into the line that reports it, which is the shape
        // that stays right until the thing it describes changes. Insetting the cascade made the
        // shell log one origin and place another, and `check-login` parses this line to find the
        // window it is about to click (M11 Part E batch 4).
        Line::new()
            .s(b"desktop-shell: placed window ")
            .u(created.window as u64)
            .s(b" at ")
            .i(x as i64)
            .s(b",")
            .i(y as i64)
            .end();
    }
    dirty
}

/// Render the modal into a free buffer and commit it.
fn present_modal(
    session: &mut Session<ChannelTransport>,
    id: u32,
    theme: &Theme,
    font: &Font,
    query: &TextFieldState,
    rows: &[ListRow<'_>],
    addrs: &[*mut u8; BUFFERS],
    tree: &mut Tree,
    hovered: Option<u64>,
    list: &mut ListState,
) {
    let len = MODAL_PITCH * MODAL_H as usize;
    let fb = render_modal(theme, font, query, rows, list, tree, hovered);
    let bytes = fb.into_bytes();
    if bytes.len() != len {
        return;
    }
    let Some(mut w) = session.window(id) else {
        return;
    };
    // A buffer the compositor is not displaying — writing into the committed one would tear
    // the picture on screen.
    let Ok(slot) = w.acquire() else {
        return;
    };
    let addr = addrs[slot as usize % BUFFERS];
    if addr.is_null() {
        return;
    }
    // SAFETY: `addr` maps `len` writable bytes and `bytes` holds exactly `len`; distinct
    // allocations, so they cannot overlap.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr, len) };
    let _ = w.commit(slot, (0, 0, MODAL_W, MODAL_H));
}

/// Destroy the modal and forget it, so the top bar accepts a click again.
///
/// The query is reset with it: a launcher that reopened still filtered by the last thing
/// launched would be showing a stale answer to a question nobody asked.
fn close_modal(
    session: &mut Session<ChannelTransport>,
    modal: &mut Option<u32>,
    query: &mut TextFieldState,
    what: &str,
    modal_addrs: &mut [*mut u8; BUFFERS],
) {
    if let Some(id) = modal.take() {
        if let Some(w) = session.window(id) {
            let _ = w.destroy();
        }
        query.clear();
        // The modal's buffers go the same way the overview's do — 614 KB a time rather than
        // 8 MB, which is why this had not bitten, but it is the same bug.
        for a in modal_addrs.iter_mut() {
            release_buffer(a, MODAL_PITCH * MODAL_H as usize);
        }
        // **Named, because the same popup serves two purposes.** These gates read the serial
        // log as the shell's only externally visible output, and a rename dismissal reading as
        // a launcher dismissal is the kind of line a later gate would assert the wrong thing
        // about (PR #243 review, optional 8).
        Line::new()
            .s(b"desktop-shell: ")
            .s(what.as_bytes())
            .s(b" closed, window ")
            .u(id as u64)
            .end();
    }
}

/// Open the applications modal as a popup parented to the top bar.
///
/// **A `popup`, which is what M6 Part C made it possible to be.** A menu was a `Stack` layer
/// over its window until then, and worked only because it happened to fit inside one; a modal
/// wider than the bar it hangs from could not have been drawn that way at all. It is
/// positioned by its creator and clipped by the *screen*, not by its parent.
fn open_modal(
    session: &mut Session<ChannelTransport>,
    parent: u32,
    theme: &Theme,
    font: &Font,
    apps: &[Application],
    addrs: &mut [*mut u8; BUFFERS],
    query: &TextFieldState,
    tree: &mut Tree,
    list: &mut ListState,
) -> Option<u32> {
    let rows = modal_rows(apps, query.text());
    // Nothing is hovered before the window exists.
    let picture = render_modal(theme, font, query, &rows, list, tree, None);
    let bytes = picture.into_bytes();
    let len = MODAL_PITCH * MODAL_H as usize;
    if bytes.len() != len {
        kprint(b"desktop-shell: modal render is not the size it declares\n");
        return None;
    }
    let role = Role::Popup { parent };
    // **Hanging from the applications button, not sitting on top of it** (M11 Part E batch 4).
    // A popup created with `new` takes its parent's origin, and the parent here is the top bar —
    // so the modal covered the bar it dropped from, including the button that opened it.
    // `nxterm`'s menu has always used `at`; this is the same call, with the button's left edge
    // and the bar's height. A popup is clipped by the *screen* rather than by its parent, which
    // is what lets it hang below a 24-pixel bar.
    let id = match session
        .create(&CreateWindowRequest::at(MODAL_W, MODAL_H, role, 0, BAR_H as i32), BUFFERS)
    {
        Ok(id) => id,
        Err(_) => {
            kprint(b"desktop-shell: modal CreateWindow FAILED\n");
            return None;
        }
    };
    // **Every failure past `create` destroys the window.** Returning `None` without it left
    // the compositor holding a mapped popup whose id this process had forgotten — never
    // closable, never committable to — while `addrs` kept a half-written new mapping that the
    // next `present_modal` would write through (PR #237 review, finding 7).
    let mut ok = true;
    for i in 0..BUFFERS {
        let Some((handle, addr)) = shared_buffer(len) else {
            kprint(b"desktop-shell: modal buffer alloc FAILED\n");
            ok = false;
            break;
        };
        addrs[i] = addr;
        // SAFETY: `addr` maps `len` writable bytes and `bytes` holds exactly `len`; distinct
        // allocations, so they cannot overlap.
        unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), addr, len) };
        let Some(mut w) = session.window(id) else {
            ok = false;
            break;
        };
        if w.attach(i as u32, MODAL_W, MODAL_H, MODAL_PITCH as u32, handle).is_err() {
            kprint(b"desktop-shell: modal AttachBuffer FAILED\n");
            ok = false;
            break;
        }
    }
    if ok {
        match session.window(id) {
            Some(mut w) => {
                if w.commit(0, (0, 0, MODAL_W, MODAL_H)).is_err() {
                    kprint(b"desktop-shell: modal Commit FAILED\n");
                    ok = false;
                }
            }
            None => ok = false,
        }
    }
    if !ok {
        if let Some(w) = session.window(id) {
            let _ = w.destroy();
        }
        return None;
    }
    Line::new()
        .s(b"desktop-shell: applications modal open, window ")
        .u(id as u64)
        .s(b" listing ")
        .u(apps.len() as u64)
        .end();
    Some(id)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    kprint(b"desktop-shell: PANIC\n");
    // SAFETY: terminating this process.
    unsafe { syscall4(SYS_PROCESS_EXIT, 1, 0, 0, 0) };
    loop {
        core::hint::spin_loop();
    }
}
