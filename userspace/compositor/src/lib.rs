//! The compositor's window model — the stack, roles and struts, commits, and compositing.
//!
//! **No syscalls in here.** This is the same split `fs-server-ext4` uses for its ext4
//! parser and `nxsh` for its evaluator: the logic that decides *what the screen should
//! look like* is a pure function of the window stack, and the parts that touch the OS
//! (IPC, mapped buffers, `/dev/draw`) live in the server bin. Compositing is asserted
//! pixel-exactly on the host in milliseconds rather than through a 90-second boot.
//!
//! Pixels come in through [`BufferSource`] rather than being owned here. In the server
//! those bytes are a `MemoryObject` the client drew into and shared **once**; owning them
//! would mean a copy per frame and would defeat the shared-memory design the substrate
//! doc chose (`docs/design/display-substrate.md` §4).
//!
//! ## Scope
//!
//! Plan Milestone 2 Part A. Window creation, buffer attachment, commit, release, destroy,
//! and stacking — plus roles and struts, which land here rather than later because
//! "retrofitting a role into a shipped protocol touches every client".

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

pub mod input;
pub mod manager;
pub mod outbox;
pub mod server;

use alloc::vec::Vec;

use libdraw::compose::{SurfaceRef, compose};
use libdraw::format::{PixelFormat, Rgb};
use libdraw::framebuffer::{Framebuffer, Geometry};
use libdraw::geom::{Point, Rect};
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, CreateWindowRequest, Edge, Role, STICKY_DESKTOP,
    WINDOW_FLAG_MINIMIZED,
};

/// Where a window's pixels are, for a given (window, buffer) pair.
///
/// The seam that keeps this crate free of syscalls. Tests back it with owned vectors; the
/// server backs it with mapped shared memory.
pub trait BufferSource {
    /// The bytes of `buffer` on `window`, or `None` if it is not mapped here.
    fn pixels(&self, window: u32, buffer: u32) -> Option<&[u8]>;
}

/// A buffer a client has attached to a window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AttachedBuffer {
    /// Client-chosen id, unique within the window.
    pub id: u32,
    /// Shape of the buffer's pixels.
    pub geometry: Geometry,
}

/// The smallest rectangle containing both.
///
/// A local copy rather than `libui::damage::union`: `libui` is a sibling of this crate and
/// neither may depend on the other, and a rectangle union in `libdraw` would be the third
/// place to look for one. Six lines.
///
/// Lives here rather than in `server` because [`WindowStack::place`] returns a union too — the
/// move damage — and a second copy one module away is how the two would come to disagree.
pub fn union(a: Rect, b: Rect) -> Rect {
    if a.size.w == 0 || a.size.h == 0 {
        return b;
    }
    if b.size.w == 0 || b.size.h == 0 {
        return a;
    }
    let x0 = a.origin.x.min(b.origin.x);
    let y0 = a.origin.y.min(b.origin.y);
    let x1 = a.right().max(b.right());
    let y1 = a.bottom().max(b.bottom());
    Rect::new(x0, y0, (x1 - x0 as i64) as u32, (y1 - y0 as i64) as u32)
}

/// A region that must be repainted, returned by the mutations that create one.
///
/// **A newtype purely so it cannot be dropped in silence.** `#[must_use]` on `place` itself is
/// not enough and the difference matters: `stack.place(id, p)?` *uses* the `Result`, so the
/// attribute is satisfied and the `Rect` falls on the floor with no diagnostic — which is exactly
/// how the M5 bug would come back through the API built to prevent it (PR #196 review, finding
/// 3). The attribute has to be on the thing that survives the `?`.
///
/// A **zero-sized** damage is this crate's "nothing changed", the same thing
/// `Outcome::Applied { dirty: Some(empty) }` means and distinct from its `None`, which means "I
/// cannot name what changed, repaint everything". [`union`] treats it as the identity, so a
/// caller folding one into a wider region gets the right answer without a special case.
#[must_use = "a move's damage must be repainted, or the window's old pixels stay on screen"]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Damage(pub Rect);

impl Damage {
    /// The rectangle, for a caller that is about to repaint it.
    pub fn rect(self) -> Rect {
        self.0
    }

    /// Whether it covers no pixels.
    pub fn is_empty(self) -> bool {
        self.0.size.w == 0 || self.0.size.h == 0
    }
}

/// A window: an id, a fixed role, a position, and the buffer currently on screen.
#[derive(Clone, Debug)]
pub struct Window {
    /// Compositor-assigned id, unique for the life of the compositor.
    pub id: u32,
    /// Fixed at creation — see [`Role`].
    pub role: Role,
    /// Top-left corner in screen coordinates.
    pub origin: Point,
    /// Requested size. The committed buffer's geometry is what actually gets drawn.
    pub size: (u32, u32),
    /// Buffers the client has attached.
    pub buffers: Vec<AttachedBuffer>,
    /// The buffer last committed, if any — what compositing reads.
    pub committed: Option<u32>,
    /// What the client called this window, or empty if it never said.
    ///
    /// **Stored here rather than derived**, because only the client knows it and only a
    /// manager wants it — the compositor itself never draws a title. Bounded at
    /// [`MAX_TITLE`](librsproto::surface::MAX_TITLE) on the way in, so a window's cost stays
    /// finite however chatty its client is.
    pub title: alloc::string::String,
    /// Whether this window's first `Configure` has been sent to its client.
    ///
    /// **A window is not composited until it has been configured** — M6 B4. The client is
    /// obliged to wait for its first `Configure` before committing, so a well-behaved one
    /// cannot reach the screen early anyway; this flag is what makes that an enforced ordering
    /// rather than a convention, and it is the gate a manager's window of opportunity is built
    /// from. Set by [`mark_configured`](WindowStack::mark_configured).
    pub configured: bool,
    /// Which desktop this window is on; [`STICKY_DESKTOP`] means all of them.
    ///
    /// Set to the stack's current desktop at creation, and changed only by a manager. **The
    /// compositor holds the attribute and nothing else about desktops** — no list, no names,
    /// no lifecycle. Which desktops exist is the desktop shell's, which is what keeps the two
    /// from being able to disagree.
    pub desktop: u32,
    /// Hidden without leaving its desktop.
    ///
    /// **A separate attribute rather than a reserved `desktop` value**, because a minimized
    /// window is still *on* its desktop: it restores there and it belongs in that desktop's
    /// window list. Folding the two would make restoring a guess about where it came from.
    pub minimized: bool,
    /// The last state this window's client asked the manager for.
    ///
    /// **Only to tell a repeat from a change.** The compositor does not act on a state request
    /// and does not know whether a window is maximised — that is a rectangle the manager
    /// restores from, and a second copy here could disagree with it. What it does know is what
    /// was last *asked*, which is enough to keep a looping client off a bounded queue.
    pub state_requested: Option<u32>,
}

impl Window {
    /// Whether this window is on screen when `current` is the current desktop.
    ///
    /// **The one predicate for "on screen", and the reason it is a method.** Compositing,
    /// focus and hit-testing each need it, and until M8 Part A two of them carried their own
    /// copy of the `configured` half. A fourth site that forgets a clause is precisely how a
    /// window becomes invisible but still clickable, which is the bug this part was most
    /// likely to ship (`display-arm-plan.md`, M8 Part A).
    ///
    /// It deliberately says nothing about *whether there is anything to draw* — a window with
    /// no committed buffer is on screen and empty. Compositing skips it a step later, because
    /// "has pixels" is a different question from "is visible", and hit-testing wants the
    /// window that is there rather than the one that has drawn.
    pub fn visible_on(&self, current: u32) -> bool {
        self.configured
            && !self.minimized
            && (self.desktop == STICKY_DESKTOP || self.desktop == current)
    }
}

impl Window {
    /// The window's bounds in screen space, from its committed buffer if there is one.
    pub fn bounds(&self) -> Rect {
        let (w, h) = self
            .committed
            .and_then(|id| self.buffers.iter().find(|b| b.id == id))
            .map(|b| (b.geometry.width, b.geometry.height))
            .unwrap_or(self.size);
        Rect::new(self.origin.x, self.origin.y, w, h)
    }
}

/// What went wrong applying a client request.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StackError {
    /// No window with that id.
    NoSuchWindow,
    /// No buffer with that id on that window.
    NoSuchBuffer,
    /// A popup or dialog named a parent that does not exist.
    NoSuchParent,
    /// The buffer's geometry is not one this compositor can read.
    BadGeometry,
    /// A buffer id already attached to this window **and currently on screen**.
    ///
    /// Re-attaching an id is how a client resizes (M9 Part D) — it replaces the pixels behind
    /// that id — and the one buffer it may not do that to is the committed one, whose pixels
    /// the compositor is reading. Replacing that would change what is on screen without a
    /// commit, which is the tearing case the whole buffer protocol exists to make
    /// unrepresentable.
    DuplicateBuffer,
    /// This connection already holds [`MAX_WINDOWS_PER_CONNECTION`] windows.
    TooManyWindows,
    /// [`STICKY_DESKTOP`] was given where a real desktop was required.
    StickyIsNotADesktop,
    /// That window is being dragged by the user, so a manager may not place it.
    ///
    /// **Refused rather than silently overridden.** A `Place` that landed mid-drag would move the
    /// window out from under the pointer and be undone by the next motion event a moment later,
    /// so a manager that raced a drag would appear to work and fight the pointer. Refusing gives
    /// it an answer it can act on (M9 Part A).
    Dragging,
}

/// How many windows one connection may hold at once.
///
/// **Bounded because everything else here is** — the outbox at 32, the manager's at 512,
/// sessions at `MAX_WAIT_HANDLES - 3`. Until M6 C3 the *API* was the bound: `libsurface`'s
/// `Window` owned its transport, so a well-behaved client held one window per connection and
/// nothing needed a number. A session type removes that accident, so the limit becomes a real
/// one rather than an emergent one.
///
/// 64 is far above any honest use — a window, its menu, that menu's submenus and a dialog or
/// two — and far below what would let one client exhaust the compositor's memory. Sequential
/// churn is unaffected: `ui-testclient` opens 128 windows in a row and destroys each before the
/// next, so it never holds more than one.
pub const MAX_WINDOWS_PER_CONNECTION: usize = 64;

/// The pointer sprite: a plain arrow, `CURSOR_W × CURSOR_H`.
///
/// **One fixed shape, not a protocol.** Per-client cursors — an I-beam over a terminal grid,
/// a resize arrow on an edge — are a Surface addition and are deliberately not in this
/// milestone ([`widget-toolkit.md`](../architecture/widget-toolkit.md) §9.3); a single arrow is what
/// makes a menu usable by a person, which is Part C's bar.
///
/// Two colours so it is visible against both: `#` is the body, `.` the outline, ` ` is
/// transparent. Drawn from a string because a bitmap you can read is a bitmap you can fix.
const CURSOR: [&str; CURSOR_H as usize] = [
    ".",
    "..",
    ".#.",
    ".##.",
    ".###.",
    ".####.",
    ".#####.",
    ".######.",
    ".#######.",
    ".########.",
    ".#####....",
    ".##.##.",
    ".#. .##.",
    "..   .##.",
    "      .##.",
    "       ..",
];

/// Cursor sprite width.
pub const CURSOR_W: u32 = 12;
/// Cursor sprite height.
pub const CURSOR_H: u32 = 16;

/// The cursor's fill colour (`#` in the sprite).
pub const CURSOR_BODY: Rgb = Rgb::new(0xFF, 0xFF, 0xFF);
/// The cursor's outline colour (`.` in the sprite), so it stays visible against white.
pub const CURSOR_OUTLINE: Rgb = Rgb::new(0x00, 0x00, 0x00);

/// The rectangle a cursor at `at` occupies.
///
/// The hotspot is the **top-left**, which is where an arrow points; a cursor whose hotspot is
/// its centre clicks half a sprite away from where it looks like it is pointing.
pub fn cursor_rect(at: Point) -> Rect {
    Rect::new(at.x, at.y, CURSOR_W, CURSOR_H)
}

/// Draw the pointer sprite at `at`, clipped to `clip`.
///
/// Composited **after** the window stack, because a cursor under a window is not a cursor.
pub fn draw_cursor<F: Framebuffer + ?Sized>(fb: &mut F, at: Point, clip: Rect) {
    for (row, line) in CURSOR.iter().enumerate() {
        for (col, ch) in line.chars().enumerate() {
            let colour = match ch {
                '#' => CURSOR_BODY,
                '.' => CURSOR_OUTLINE,
                _ => continue,
            };
            let x = at.x.saturating_add(col as i32);
            let y = at.y.saturating_add(row as i32);
            // Clipped by the caller's rectangle *and* by the buffer: a cursor at the screen
            // edge is the ordinary case, not an error, and it must not wrap to the far side.
            if x >= 0 && y >= 0 && clip.contains(x, y) {
                fb.put_pixel(x as u32, y as u32, colour);
            }
        }
    }
}

/// The interactive-resize outline's thickness, in pixels.
pub const OUTLINE_W: u32 = 2;

/// The outline's colour — bright enough to read over any client's pixels.
pub const OUTLINE_COLOUR: Rgb = Rgb::new(0xE0, 0xE0, 0xE0);

/// The four edge strips of `rect`, which is what an outline occupies.
///
/// **Strips rather than the rectangle**, because this is what gets repainted per pointer motion.
/// The union of an outline's old and new rectangles is very nearly the window, and repainting
/// that under emulation is the ~100 ms full recompose that starves input — the failure this
/// milestone has already met twice. Four thin bands are a few thousand pixels whatever the
/// window's size.
///
/// Empty strips are returned as-is; the compositor's damage handling already skips them.
pub fn outline_edges(rect: Rect) -> [Rect; 4] {
    let t = OUTLINE_W.min(rect.size.h.max(1));
    let side = OUTLINE_W.min(rect.size.w.max(1));
    [
        Rect::new(rect.origin.x, rect.origin.y, rect.size.w, t),
        Rect::new(rect.origin.x, rect.bottom() as i32 - t as i32, rect.size.w, t),
        Rect::new(rect.origin.x, rect.origin.y, side, rect.size.h),
        Rect::new(rect.right() as i32 - side as i32, rect.origin.y, side, rect.size.h),
    ]
}

/// Draw the interactive-resize outline at `rect`, clipped to `clip`.
///
/// **Over the composed stack, like the cursor**, and for the same reason: it is not a window. It
/// has no client, no buffer, no place in the stacking order and nothing can cover it — a
/// preview of a rectangle the user has not committed to yet. Decision 1 of Milestone 9 refuses
/// chrome the compositor has to lay out and style; an outline has neither.
pub fn draw_outline<F: Framebuffer + ?Sized>(fb: &mut F, rect: Rect, clip: Rect) {
    for strip in outline_edges(rect) {
        let Some(r) = strip.intersect(&clip) else { continue };
        for y in r.origin.y..r.bottom() as i32 {
            for x in r.origin.x..r.right() as i32 {
                if x >= 0 && y >= 0 {
                    fb.put_pixel(x as u32, y as u32, OUTLINE_COLOUR);
                }
            }
        }
    }
}

/// How long a key must be held before it starts repeating, in nanoseconds.
///
/// Policy with no configuration surface yet. Both constants are what a settings service will
/// eventually own; until there is one, they are named here rather than spelled inline so the
/// eventual move is a change of provenance.
pub const REPEAT_DELAY_NS: u64 = 400_000_000;

/// How long between repeats once they start, in nanoseconds.
pub const REPEAT_INTERVAL_NS: u64 = 40_000_000;

/// A key held down, and when it next repeats.
///
/// **Compositor-side rather than per-client**, because the compositor knows which window has
/// focus and can therefore stop a repeat when focus moves, with no client involvement. The
/// alternative is Wayland's — publish a rate and let every client run its own timer — which
/// is better when clients disagree about what should repeat, a distinction nothing here makes
/// yet, and costs every client a timer and a state machine
/// ([`widget-toolkit.md`](../architecture/widget-toolkit.md) §9.2).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Repeat {
    /// The keycode being repeated.
    pub keycode: u16,
    /// Modifiers held when it went down.
    ///
    /// Frozen at the press rather than re-read: a repeat is that press continuing, so
    /// releasing shift mid-repeat must not turn `A` into `a` halfway through a run.
    pub modifiers: u16,
    /// The window it is going to.
    pub window: u32,
    /// When the next repeat is due.
    pub next_at: u64,
}

impl Repeat {
    /// Start repeating `keycode` for `window`, first repeat one delay from `now`.
    pub fn armed(keycode: u16, modifiers: u16, window: u32, now: u64) -> Self {
        Self { keycode, modifiers, window, next_at: now.saturating_add(REPEAT_DELAY_NS) }
    }

    /// Whether a repeat is due at `now`; advances to the next one if so.
    ///
    /// **Advances by adding an interval to the deadline, not to `now`.** Adding to `now`
    /// makes every late wake-up push the next repeat further out, so a busy compositor
    /// repeats slower and slower — and the tick that wakes this is 10 ms, so late is the
    /// normal case rather than the exception.
    pub fn due(&mut self, now: u64) -> bool {
        if now < self.next_at {
            return false;
        }
        self.next_at = self.next_at.saturating_add(REPEAT_INTERVAL_NS);
        // A wake-up long after the deadline — a stalled compositor, or a debugger — must not
        // then fire a burst catching up on repeats nobody asked for.
        if self.next_at <= now {
            self.next_at = now.saturating_add(REPEAT_INTERVAL_NS);
        }
        true
    }

    /// The repeat state after a key transition — arm, disarm, or leave alone.
    ///
    /// **Here rather than in the server loop**, for the reason [`focus_transition`] gives
    /// below and finding 2 of PR #185's review proved: the arming *decision* is a function of
    /// values, the server loop that held it needs syscalls to reach, and the two bugs it
    /// carried were both decisions rather than plumbing. `Repeat::due` was host-tested from
    /// the start and was correct; what was not tested was when a repeat starts.
    ///
    /// - **A modifier never arms and never disturbs.** Modifiers are ordinary
    ///   [`Logical::Key`](libinput::Logical::Key) transitions, so an unqualified arm made
    ///   holding Ctrl send 25 `KEY_REPEAT`s a second — and since there is one slot, pressing
    ///   Shift while `a` was held replaced `a`'s repeat, after which releasing Shift cleared
    ///   it and the still-held `a` never repeated again.
    /// - **Only the repeating key's own release disarms**, which is the same rule from the
    ///   other side: a repeat outlives any modifier moving under it.
    /// - **A press with no focus candidate disarms**, rather than leaving the previous key
    ///   repeating into a window that is no longer focused.
    pub fn after_key(
        current: Option<Repeat>,
        keycode: u16,
        pressed: bool,
        modifiers: u16,
        focus: Option<u32>,
        now: u64,
    ) -> Option<Repeat> {
        if libinput::is_modifier(keycode) {
            return current;
        }
        match (pressed, focus) {
            (true, Some(window)) => Some(Repeat::armed(keycode, modifiers, window, now)),
            (true, None) => None,
            (false, _) if current.is_some_and(|r| r.keycode == keycode) => None,
            (false, _) => current,
        }
    }
}

/// The `(lost, gained)` pair a focus change implies, or `None` if focus did not move.
///
/// **The comparison is the whole point.** `focus_candidate` is recomputed after anything that
/// *could* move focus — a create, a destroy, a raise, a session closing — and most of those
/// do not move it. Announcing unconditionally would send a `FocusEvent` on every one of them,
/// which for a client means a stream of messages saying nothing changed, on the same bounded
/// queue its input arrives on.
///
/// Pure, and here rather than in the server, because "did focus change" is a question about
/// two values and the boot gate can only observe that *a* focus event arrived — not that
/// exactly one did (PR #183's lesson, applied before the review).
pub fn focus_transition(
    prev: Option<u32>,
    now: Option<u32>,
) -> Option<(Option<u32>, Option<u32>)> {
    (prev != now).then_some((prev, now))
}

/// The compositor's window stack: bottom-first order, plus the id allocator.
pub struct WindowStack {
    windows: Vec<Window>,
    next_id: u32,
    /// Every id that has left the stack since the last [`take_removed`](Self::take_removed).
    ///
    /// **A log rather than a return value because one destroy can remove several windows and
    /// two different callers trigger it.** `DestroyWindow` and a client disconnecting both end
    /// up in [`destroy`](Self::destroy), and a manager owed a `WindowDestroyed` event needs
    /// both. Threading an out-param through `dispatch` would put it in the signature of every
    /// op that cannot remove anything; recording it where the removal happens does not.
    ///
    /// The bin drains this after every dispatch whether or not a manager is attached, so it
    /// holds at most one call's worth and cannot grow while nobody is listening.
    removed_log: Vec<u32>,
    /// Every id whose [`bounds`](Window::bounds) changed since the last
    /// [`take_geometry_changes`](Self::take_geometry_changes).
    ///
    /// **Recorded where bounds change, not where a request is dispatched.** A window's
    /// on-screen rectangle moves for more than one reason — a manager `Place`, and a client
    /// committing a buffer of a different size — and `WindowGeometry` promises to report it
    /// *for any reason*. Emitting from the one op the feature was written for is how the
    /// commit case silently goes unreported, leaving a manager to poll `/dev/draw/<id>/info`
    /// for the thing this event exists to save it from (PR #217 review, finding 2).
    ///
    /// **Including the changes a manager caused itself.** A manager that had to remember which
    /// moves were its own would be keeping a second copy of the stack, which is the duplicate
    /// state this event exists to make unnecessary.
    ///
    /// Drained like [`removed_log`](Self::removed_log), and for the same reason.
    geometry_log: Vec<u32>,
    /// Which desktop is composited. Never [`STICKY_DESKTOP`] — see [`Self::set_current_desktop`].
    current_desktop: u32,
    /// The window the user is interactively dragging, and where it was when the drag began.
    ///
    /// **Here rather than in the router, because two paths need it.** The router runs the drag;
    /// the *manager* path has to refuse a `Place` for a window being dragged, and it never sees
    /// the router. The stack is what both of them already hold.
    ///
    /// The origin is kept so that [`end_drag`](Self::end_drag) can tell a gesture that moved the
    /// window from one that did not.
    dragging: Option<(u32, Point)>,
}

impl Default for WindowStack {
    /// Delegates to [`WindowStack::new`].
    ///
    /// **Not `#[derive(Default)]`.** That would start `next_id` at 0 while `new()` starts
    /// at 1, so the two constructors would hand out different id spaces — and a client
    /// using 0 as a "no parent" sentinel would get `Popup { parent: 0 }` *accepted* on a
    /// derived stack where `new()` returns `NoSuchParent`.
    fn default() -> Self {
        Self::new()
    }
}

impl WindowStack {
    /// An empty stack.
    pub fn new() -> Self {
        Self {
            windows: Vec::new(),
            next_id: 1,
            removed_log: Vec::new(),
            geometry_log: Vec::new(),
            // **One, not zero.** Zero is `STICKY_DESKTOP`, and a compositor whose current
            // desktop was the sticky value would composite only sticky windows and make every
            // window it created afterwards sticky too, by the create-onto-current rule.
            current_desktop: 1,
            dragging: None,
        }
    }

    /// Windows, bottom-first.
    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    /// The window with `id`, if it exists.
    pub fn window(&self, id: u32) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
    }

    /// The window with `id`, mutably.
    ///
    /// The read-only [`window`](Self::window) covers every other caller; a title is the first
    /// thing a request changes on a window without touching geometry or buffers.
    pub fn window_mut(&mut self, id: u32) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    /// Create a window and return its id.
    ///
    /// A popup or dialog must name a parent that exists — otherwise the compositor would
    /// be holding a transient window with nothing to be transient to, and stacking it
    /// would be undefined.
    pub fn create(&mut self, req: &CreateWindowRequest) -> Result<u32, StackError> {
        if let Role::Popup { parent } | Role::Dialog { parent } = req.role
            && self.window(parent).is_none()
        {
            return Err(StackError::NoSuchParent);
        }
        // **A popup starts where its creator asked, relative to its parent** — the only party
        // that knows where the menu item it drops from was drawn (M6 C1). Resolved to an
        // absolute origin once, here: the stack stores absolute origins, and a popup that
        // tracked its parent would have to be re-placed whenever the parent moved, which is
        // placement policy — see `TODO(popup-follows-parent)`.
        //
        // **A `dialog` is not placed this way, though it also names a parent.** Its parent
        // carries desktop membership and lifetime — not its position (`display-substrate.md`
        // §4a, `ui-composition-model.md` §6). In placement terms it is an ordinary listed
        // window, so it lands at the origin and a manager places it, exactly like a `normal`.
        let origin = match req.role {
            Role::Popup { parent } => {
                // The parent is known to exist: checked directly above, and nothing between
                // here and there can remove it.
                let base = self.window(parent).expect("checked above").origin;
                Point::new(base.x.saturating_add(req.offset_x), base.y.saturating_add(req.offset_y))
            }
            Role::Normal | Role::Panel { .. } | Role::Dialog { .. } => Point::new(0, 0),
        };
        let id = self.next_id;
        self.next_id += 1;
        self.windows.push(Window {
            id,
            role: req.role,
            origin,
            size: (req.width, req.height),
            buffers: Vec::new(),
            committed: None,
            // Created unconfigured, whether or not anyone is going to answer. Whoever sends the
            // first `Configure` — the compositor at once, or a manager, or the deadline —
            // marks it, and until then the window exists without being on screen.
            title: alloc::string::String::new(),
            configured: false,
            // **Onto the current desktop**, which is what makes "a window is never on no
            // desktop" true by construction rather than by care: there is no window-exists-
            // but-is-unassigned moment for anything to have to render.
            desktop: self.current_desktop,
            minimized: false,
            state_requested: None,
        });
        Ok(id)
    }

    /// Record that `window`'s first `Configure` has gone out, making it compositable.
    ///
    /// Idempotent, and returns whether this was the transition. The caller uses that to send
    /// the record exactly once: a manager may `Place` a pending window and then `Configure` it,
    /// and the client is owed one initial configure, not two.
    pub fn mark_configured(&mut self, window: u32) -> bool {
        match self.windows.iter_mut().find(|w| w.id == window) {
            Some(w) if !w.configured => {
                w.configured = true;
                true
            }
            _ => false,
        }
    }

    /// Which desktop is composited.
    pub fn current_desktop(&self) -> u32 {
        self.current_desktop
    }

    /// Move `window` to `desktop`; [`STICKY_DESKTOP`] puts it on all of them.
    ///
    /// Returns whether the window's **visibility** changed, which is what the caller needs to
    /// decide whether to repaint — moving a window between two desktops that are both not the
    /// current one changes nothing on screen.
    ///
    /// **Any non-zero id is accepted, and zero is an id rather than an error.** This function
    /// cannot validate a desktop because the compositor does not know which desktops exist;
    /// that is the desktop shell's, and asking here would be keeping a second copy of it.
    pub fn set_window_desktop(&mut self, window: u32, desktop: u32) -> Result<bool, StackError> {
        let current = self.current_desktop;
        let w = self.windows.iter_mut().find(|w| w.id == window).ok_or(StackError::NoSuchWindow)?;
        let was = w.visible_on(current);
        w.desktop = desktop;
        Ok(was != w.visible_on(current))
    }

    /// Minimize or restore `window`. Returns whether its visibility changed.
    pub fn set_minimized(&mut self, window: u32, minimized: bool) -> Result<bool, StackError> {
        let current = self.current_desktop;
        let w = self.windows.iter_mut().find(|w| w.id == window).ok_or(StackError::NoSuchWindow)?;
        let was = w.visible_on(current);
        w.minimized = minimized;
        Ok(was != w.visible_on(current))
    }

    /// Switch which desktop is composited. Returns whether it changed.
    ///
    /// **`STICKY_DESKTOP` is refused, and it is the only value this validates.** `0` means "on
    /// every desktop", so a current desktop of `0` would composite only sticky windows *and* —
    /// by [`create`](Self::create) assigning the current desktop — make every window created
    /// afterwards silently sticky, which is a state nothing could undo without knowing what
    /// each window's desktop should have been.
    pub fn set_current_desktop(&mut self, desktop: u32) -> Result<bool, StackError> {
        if desktop == STICKY_DESKTOP {
            return Err(StackError::StickyIsNotADesktop);
        }
        let changed = self.current_desktop != desktop;
        self.current_desktop = desktop;
        Ok(changed)
    }


    /// Move a window's top-left corner, returning the region that must be repainted.
    ///
    /// **The damage comes back from the mutation, rather than being computed after it.** That is
    /// not a style preference: a move dirties the *union* of where the window was and where it
    /// now is, because a rectangle cannot express "old minus new" — and every other path in this
    /// compositor computes `dirty` from state read *before* the change, which is a discipline a
    /// caller can forget. M5 shipped exactly that bug for a resized buffer (PR #192, finding 3).
    /// Returning it makes the trap unreachable here instead of merely known.
    ///
    /// **A window that has never committed dirties nothing.** It is not on screen —
    /// [`present_into`](Self::present_into) skips windows with no committed buffer — so moving it
    /// paints over nothing and reveals nothing. This is the ordinary case rather than an edge
    /// one: placing a window *before* its first commit is what a manager does. That case returns
    /// a **zero-sized** rectangle, which is this crate's "nothing changed" — the same thing
    /// `Outcome::Applied { dirty: Some(empty) }` means, and distinct from its `None`, which means
    /// "I cannot name what changed, repaint everything". A caller unioning this into a `dirty`
    /// gets the right answer for free, because [`union`] treats a zero rect as the identity.
    ///
    /// Returns [`Damage`] rather than a bare `Rect` so that forgetting it is a warning: see that
    /// type for why `#[must_use]` on this function would not have been enough.
    pub fn place(&mut self, id: u32, origin: Point) -> Result<Damage, StackError> {
        if self.dragging.is_some_and(|(d, _)| d == id) {
            return Err(StackError::Dragging);
        }
        self.move_to(id, origin, true)
    }

    /// The window the user is interactively dragging, if any.
    pub fn dragging(&self) -> Option<u32> {
        self.dragging.map(|(id, _)| id)
    }

    /// Begin an interactive drag of `id`, which makes [`place`](Self::place) refuse it.
    ///
    /// `NoSuchWindow` if it is not in the stack. Beginning a second drag replaces the first: the
    /// pointer has one grab, so two drags cannot be in flight, and the arithmetic that would
    /// decide which to keep is arithmetic about a state that cannot happen.
    pub fn begin_drag(&mut self, id: u32) -> Result<(), StackError> {
        let origin = self.window(id).ok_or(StackError::NoSuchWindow)?.origin;
        self.dragging = Some((id, origin));
        Ok(())
    }

    /// End the drag, and record the one geometry change it produced.
    ///
    /// **One record for the whole gesture, not one per motion.** [`drag_to`](Self::drag_to) does
    /// not log, because the manager's queue does not coalesce and evicts its oldest when full —
    /// a five-second drag at 100 Hz would push a `WindowCreated` off the front and leave the
    /// manager with a window it will never place and never hear about again (PR #247 review).
    /// What a manager needs is where the window ended up, which is one record.
    pub fn end_drag(&mut self) {
        // **Only if it actually moved**, which `place` has always checked and this did not: an
        // ordinary click on a title bar is a drag of zero pixels, and it was putting a no-op
        // record into the queue this whole design exists to protect — the one that does not
        // coalesce and evicts its oldest — and printing a manager line for a move that did not
        // happen (PR #248 review, finding 4).
        if let Some((id, was)) = self.dragging.take()
            && self.window(id).is_some_and(|w| w.origin != was)
        {
            self.geometry_log.push(id);
        }
    }

    /// Record a state request, and report whether it differs from the last one.
    ///
    /// `false` for a window that does not exist — there is nobody to tell about it — and for a
    /// repeat of the state last asked for.
    pub fn note_state_request(&mut self, id: u32, state: u32) -> bool {
        let Some(w) = self.windows.iter_mut().find(|w| w.id == id) else {
            return false;
        };
        if w.state_requested == Some(state) {
            return false;
        }
        w.state_requested = Some(state);
        true
    }

    /// Forget what `id` last asked to be, because something else has changed it.
    ///
    /// **The shadow is only valid while nothing else has happened.** `state_requested` exists to
    /// drop a client repeating itself, and it compares against what that client last asked —
    /// which stops describing the window the moment a *manager* minimises, restores, places or
    /// configures it by any other route. Clearing it there costs one field write and makes the
    /// dedup unable to outlive its premise.
    pub fn clear_state_request(&mut self, id: u32) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
            w.state_requested = None;
        }
    }

    /// Move a window the user is dragging, without logging a geometry change.
    ///
    /// Returns `NoSuchWindow` if it has gone, which is the ordinary end of a drag whose client
    /// exited while the button was down.
    pub fn drag_to(&mut self, id: u32, origin: Point) -> Result<Damage, StackError> {
        self.move_to(id, origin, false)
    }

    /// The body of [`place`](Self::place) and [`drag_to`](Self::drag_to).
    fn move_to(&mut self, id: u32, origin: Point, log: bool) -> Result<Damage, StackError> {
        let w = self.windows.iter_mut().find(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        let uncommitted = w.committed.is_none();
        let was = w.bounds();
        w.origin = origin;
        let now = w.bounds();
        if log && was != now {
            self.geometry_log.push(id);
        }
        if uncommitted {
            // A window with nothing on screen has no pixels to repaint, but it has still
            // *moved*: `bounds()` is taken from the requested size at this point, so the
            // record above fires and a manager learns where it will appear.
            return Ok(Damage(Rect::new(origin.x, origin.y, 0, 0)));
        }
        Ok(Damage(union(was, now)))
    }

    /// Attach a buffer to a window.
    ///
    /// **Unbounded in count — `TODO(window-buffer-cap)`.** Nothing limits how many *distinct*
    /// ids a window may attach; a client may keep inventing them and the compositor maps each.
    /// Not reachable from `libsurface`, which attaches exactly what a client asked for at
    /// creation and — since M9 Part D — *replaces* those ids on a resize rather than adding to
    /// them, which is why the resize path is bounded while this is still filed. The mappings
    /// are reclaimed on destroy. Filed because the per-connection window cap made this the one
    /// thing here that is not bounded.
    pub fn attach(&mut self, req: &AttachBufferRequest) -> Result<(), StackError> {
        let geometry =
            Geometry::with_pitch(req.width, req.height, req.pitch as usize, PixelFormat::XRGB8888)
                .ok_or(StackError::BadGeometry)?;
        let w = self
            .windows
            .iter_mut()
            .find(|w| w.id == req.window)
            .ok_or(StackError::NoSuchWindow)?;
        // **Re-attaching an id replaces it, and that is how a client resizes** (M9 Part D). A
        // resize needs buffers of the new size and the protocol has no detach, so the
        // alternative was a fresh id per resize — which grows this list, and the compositor's
        // mappings with it, for the life of a window somebody maximises and restores.
        //
        // **Except the committed one**, whose pixels the compositor may be reading: replacing
        // that changes the screen without a commit. A double-buffered client always has a free
        // buffer to replace first, so this refuses nothing an honest resize needs.
        if let Some(existing) = w.buffers.iter_mut().find(|b| b.id == req.buffer) {
            if w.committed == Some(req.buffer) {
                return Err(StackError::DuplicateBuffer);
            }
            existing.geometry = geometry;
            return Ok(());
        }
        w.buffers.push(AttachedBuffer { id: req.buffer, geometry });
        Ok(())
    }

    /// Apply a commit: the named buffer becomes what compositing reads.
    ///
    /// Returns the buffer that was on screen before, if any — **that is the one to
    /// release**. Releasing the newly committed buffer instead would hand the client back
    /// the memory the compositor is about to read, which is the tearing this protocol
    /// exists to prevent.
    pub fn commit(&mut self, req: &CommitRequest) -> Result<Option<u32>, StackError> {
        let w = self
            .windows
            .iter_mut()
            .find(|w| w.id == req.window)
            .ok_or(StackError::NoSuchWindow)?;
        if !w.buffers.iter().any(|b| b.id == req.buffer) {
            return Err(StackError::NoSuchBuffer);
        }
        let previous = w.committed;
        let was = w.bounds();
        w.committed = Some(req.buffer);
        // **A commit can resize the window.** The committed buffer's geometry is what
        // `bounds()` reports, so a client that reflows and commits a taller buffer has
        // changed its on-screen rectangle without any manager involvement at all.
        if w.bounds() != was {
            self.geometry_log.push(req.window);
        }
        // Re-committing the same buffer releases nothing: the client already knows it owns
        // no other buffer, and reporting a release here would let it draw into the buffer
        // now on screen.
        Ok(previous.filter(|&p| p != req.buffer))
    }

    /// Take every id removed since the last call, parent before the popups it took with it.
    ///
    /// Draining is the point: the bin drains after every dispatch, so the log holds at most one
    /// call's worth and cannot grow while no manager is attached.
    pub fn take_removed(&mut self) -> Vec<u32> {
        core::mem::take(&mut self.removed_log)
    }

    /// Take every id whose bounds changed since the last call, each reported once.
    ///
    /// Deduplicated because one dispatch can change a window's rectangle twice — a place and
    /// a commit in the same batch — and a manager gains nothing from hearing the same
    /// rectangle described a second time. Draining is the point, for the same reason
    /// [`take_removed`](Self::take_removed) drains.
    pub fn take_geometry_changes(&mut self) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for id in core::mem::take(&mut self.geometry_log) {
            if !out.contains(&id) {
                out.push(id);
            }
        }
        out
    }

    /// Destroy a window and everything attached to it, recording every id removed.
    ///
    /// **The removed set is recorded because one `DestroyWindow` can remove several windows.**
    /// A menu chain goes with its parent, transitively, so a caller that has to tell someone
    /// else which windows disappeared — the manager's `WindowDestroyed` event — cannot work it
    /// out from the id it passed in. Diffing the stack around the call would be the alternative
    /// and is worse: it makes every caller responsible for a snapshot, and gets the order wrong
    /// (the parent should be reported before the popups it took with it).
    pub fn destroy(&mut self, id: u32) -> Result<(), StackError> {
        let i = self.windows.iter().position(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        self.windows.remove(i);
        self.removed_log.push(id);
        // Descendants cannot outlive an ancestor: a popup or dialog with no parent has no
        // defined desktop or stacking position, and `create` refuses to produce one.
        //
        // **Transitively.** A single pass over direct children leaves a submenu — a popup
        // parented to a popup, which is the canonical nested case — alive with a dead
        // parent, still compositing and still eligible for focus after the menu chain it
        // belonged to was closed. Repeat until nothing more is orphaned.
        loop {
            let live: Vec<u32> = self.windows.iter().map(|w| w.id).collect();
            let before = self.windows.len();
            let removed = &mut self.removed_log;
            self.windows.retain(|w| {
                let keep = match w.role {
                    Role::Popup { parent } | Role::Dialog { parent } => live.contains(&parent),
                    _ => true,
                };
                if !keep {
                    removed.push(w.id);
                }
                keep
            });
            if self.windows.len() == before {
                break;
            }
        }
        // **A drag cannot outlive its window**, and a client exiting with the button held is the
        // ordinary way that happens. Left set, the flag would refuse a `Place` for an id that no
        // longer exists — for the life of the compositor, since ids are never reused.
        if self.dragging.is_some_and(|(d, _)| self.window(d).is_none()) {
            self.dragging = None;
        }
        Ok(())
    }

    /// Raise a window to the top of the stack.
    ///
    /// **The damage is the window's own rectangle, and that is exact.** Moving one window within
    /// the order changes the relative order of no other pair, so every pixel outside this
    /// window's bounds is composed from the same windows in the same order as before — only the
    /// pixels it covers can differ. This used to answer "repaint everything", on the reasoning
    /// that which pixels change "depends on every overlap in the stack"; that is true of *which*
    /// of them change and irrelevant to *where* they are.
    ///
    /// It is not a micro-optimisation. A full recompose of a 1280×800 screen takes ~100 ms under
    /// emulation, during which the compositor reads no input, and the input server's ring holds
    /// a fraction of a second of a moving mouse — so a click that raised a window used to cost a
    /// visible chunk of the movement around it (2026-08-26).
    ///
    /// A window that is already topmost is left alone and reports **empty** damage: the stack
    /// did not change, so nothing needs repainting. Click-to-focus raises on every press,
    /// including the tenth press on the same window.
    pub fn raise(&mut self, id: u32) -> Result<Damage, StackError> {
        let i = self.windows.iter().position(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        if i + 1 == self.windows.len() {
            return Ok(Damage(Rect::new(0, 0, 0, 0)));
        }
        let w = self.windows.remove(i);
        let rect = w.bounds();
        self.windows.push(w);
        Ok(Damage(rect))
    }

    /// Send a window to the bottom of the stack.
    ///
    /// No caller until the shell (M7): click-to-focus only ever raises. It lands here with
    /// [`raise_above`](Self::raise_above) because the three are one ordering rule, and a stack
    /// that can only ever push in one direction is one nobody can write alt-tab against.
    pub fn lower(&mut self, id: u32) -> Result<Damage, StackError> {
        let i = self.windows.iter().position(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        if i == 0 {
            return Ok(Damage(Rect::new(0, 0, 0, 0)));
        }
        let w = self.windows.remove(i);
        let rect = w.bounds();
        self.windows.insert(0, w);
        Ok(Damage(rect))
    }

    /// Put `id` directly above `other` in the stack.
    ///
    /// The op alt-tab needs: "raise this one, but only to where that one was" — a full
    /// [`raise`](Self::raise) would reorder every window between them, which is visible as the
    /// rest of the stack shuffling behind the one the user asked for.
    ///
    /// `id == other` is a no-op rather than an error: it is the degenerate case of a request
    /// that is otherwise well-formed, and a shell iterating a window list should not have to
    /// special-case the window it is already above.
    pub fn raise_above(&mut self, id: u32, other: u32) -> Result<Damage, StackError> {
        if self.window(id).is_none() || self.window(other).is_none() {
            return Err(StackError::NoSuchWindow);
        }
        if id == other {
            return Ok(Damage(Rect::new(0, 0, 0, 0)));
        }
        let i = self.windows.iter().position(|w| w.id == id).expect("checked above");
        let w = self.windows.remove(i);
        let rect = w.bounds();
        // Recomputed *after* the removal: taking `id` out shifts everything above it down by
        // one, so an index captured before would place the window one slot too high whenever
        // `id` sat below `other`.
        let j = self.windows.iter().position(|w| w.id == other).expect("checked above");
        self.windows.insert(j + 1, w);
        // Empty when it landed where it already was, for the reason [`raise`](Self::raise)
        // gives: the order is unchanged, so nothing on screen is.
        let moved = j + 1 != i;
        Ok(Damage(if moved { rect } else { Rect::new(0, 0, 0, 0) }))
    }

    /// The area left for `normal` windows after every panel's reservation.
    ///
    /// This is what "a maximised window must not cover the bars" means concretely
    /// (`display-substrate.md` §4a). Reservations are **declared**, not derived from a
    /// panel's geometry: the two differ when a window is fullscreen (it covers the
    /// panel's pixels while the panel still reserves that space for maximised windows).
    ///
    /// Over-reservation is clamped rather than allowed to invert the rectangle: panels
    /// claiming more than the screen leave an empty work area, not a negative one.
    ///
    /// **It counts every panel, including ones that are not on screen — deliberately, and this
    /// is the one place that question is answered differently from
    /// [`visible_on`](Window::visible_on).** The declared-not-derived rule above was written
    /// against the fullscreen case, where the panel is still on its desktop and still wants its
    /// space. M8 Part A added two more ways to be off screen, and neither changes the answer:
    /// a bar minimized or sitting on another desktop is a bar that is coming back, and a work
    /// area that grew while it was away would relayout every maximised window twice per desktop
    /// switch. A panel that should stop reserving is destroyed, not hidden (PR #240 review,
    /// optional 5).
    pub fn work_area(&self, screen: Rect) -> Rect {
        let (mut top, mut bottom, mut left, mut right) = (0u32, 0u32, 0u32, 0u32);
        for w in &self.windows {
            if let Some((edge, reserve)) = w.role.strut() {
                // Saturating, not `+=`. `reserve` is bounded at the protocol edge, but the
                // *sum* over many panels is not, and a wrap here is worse than a panic: in
                // release it returns the full screen as the work area, silently defeating
                // the clamp below. No catch-all arm — `Edge` is an enum, so a new edge is a
                // compile error rather than a silently ignored reservation.
                let slot = match edge {
                    Edge::Top => &mut top,
                    Edge::Bottom => &mut bottom,
                    Edge::Left => &mut left,
                    Edge::Right => &mut right,
                };
                *slot = slot.saturating_add(reserve);
            }
        }
        let horizontal = left.saturating_add(right);
        let vertical = top.saturating_add(bottom);
        let w = screen.size.w.saturating_sub(horizontal);
        let h = screen.size.h.saturating_sub(vertical);
        Rect::new(
            screen.origin.x + left.min(screen.size.w) as i32,
            screen.origin.y + top.min(screen.size.h) as i32,
            w,
            h,
        )
    }

    /// The `info` a window reports at `/dev/draw/<id>/info`.
    ///
    /// Reports the **committed** buffer's size when there is one, because that is what is
    /// actually on screen; the requested size is only a hint until a client commits.
    pub fn info(&self, id: u32) -> Option<librsproto::surface::WindowInfo> {
        let w = self.window(id)?;
        let bounds = w.bounds();
        let mut info = librsproto::surface::WindowInfo::new(
            w.id,
            w.role,
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.w,
            bounds.size.h,
        );
        // Set after construction: `new` takes what `CreateWindow` fixes, and these two are
        // mutable state a manager changes over the window's life.
        info.desktop = w.desktop;
        if w.minimized {
            info.flags |= WINDOW_FLAG_MINIMIZED;
        }
        Some(info)
    }

    /// The topmost window that may take keyboard focus, if any.
    ///
    /// Panels are skipped: clicking the clock must not steal input from the terminal.
    pub fn focus_candidate(&self) -> Option<u32> {
        // **Unconfigured windows are not candidates**, for the same reason they are not
        // composited: the compositor has decided they are not on screen yet, and giving the
        // keyboard to a window nobody can see loses every keystroke typed into it. A new window
        // goes on top of the stack, so without this it becomes the candidate the instant it is
        // created — before anyone has placed it, and for as long as its configure is held
        // (PR #218 review, finding 3).
        self.windows
            .iter()
            .rev()
            .find(|w| w.visible_on(self.current_desktop) && w.role.takes_focus())
            .map(|w| w.id)
    }

    /// Composite the stack into `fb`, repainting `damage` — **without the cursor**.
    ///
    /// Windows with no committed buffer, or whose buffer the source cannot resolve, are
    /// skipped rather than drawn as garbage — a client that has created a window but not
    /// yet drawn into it should show background, not whatever the memory held.
    ///
    /// **`pub(crate)`, so the server binary cannot call it**, and must go through
    /// [`present_into`](Self::present_into) instead. That is not stylistic: the cursor is
    /// *drawn over* the composed stack rather than composited into it, so every path that
    /// recomposes has to redraw it, and three of the four paths in the binary had forgotten
    /// to (PR #185 review, finding 1) — a click erased the pointer until the mouse moved
    /// next. Making "compose" unreachable from outside means the mistake cannot be made a
    /// fourth time by adding a fifth path. If a caller ever genuinely wants the stack without
    /// a pointer over it — a screenshot — widening this is a deliberate act with a reason.
    pub(crate) fn compose_into<F, S>(&self, fb: &mut F, background: Rgb, source: &S, damage: &[Rect])
    where
        F: Framebuffer + ?Sized,
        S: BufferSource + ?Sized,
    {
        let mut surfaces: Vec<SurfaceRef<'_>> = Vec::new();
        for w in &self.windows {
            // **Not composited unless it is on screen** — configured (M6 B4), not minimized,
            // and on the current desktop or sticky. A client that commits before its first
            // `Configure` would otherwise paint at the default origin and then jump when the
            // manager places it, which is the symptom the configure ordering exists to remove;
            // the other two clauses are M8 Part A's. All three live in
            // [`Window::visible_on`](Window::visible_on) so that this site and hit-testing
            // cannot drift apart.
            if !w.visible_on(self.current_desktop) {
                continue;
            }
            let Some(buffer_id) = w.committed else { continue };
            let Some(b) = w.buffers.iter().find(|b| b.id == buffer_id) else { continue };
            let Some(px) = source.pixels(w.id, buffer_id) else { continue };
            if px.len() < b.geometry.byte_len() {
                continue;
            }
            surfaces.push(SurfaceRef::new(b.geometry, w.origin, px));
        }
        compose(fb, background, &surfaces, damage);
    }

    /// Put `damage` on the screen: composite the stack, then draw the pointer over it.
    ///
    /// **The one way a screen region is updated.** The pairing lives here rather than in the
    /// server binary because the binary cannot be host-tested — its screen-update path takes a
    /// `Server` and a `RawFramebuffer`, both of which need syscalls — so "the cursor survives
    /// a recompose" had no test it could fail, and did not survive one. Moving the boundary
    /// down one layer is what makes it testable, and `pub(crate)` on
    /// [`compose_into`](Self::compose_into) is what makes it unavoidable.
    ///
    /// **The cursor goes on last.** A cursor under a window is not a cursor, and compositing
    /// it into the stack would make it a window — with a position in the stacking order, a
    /// client that could cover it, and hit-testing that would have to skip it.
    pub fn present_into<F, S>(
        &self,
        fb: &mut F,
        background: Rgb,
        source: &S,
        damage: &[Rect],
        pointer: Point,
        outline: Option<Rect>,
    ) where
        F: Framebuffer + ?Sized,
        S: BufferSource + ?Sized,
    {
        self.compose_into(fb, background, source, damage);
        // **The outline under the cursor, both over everything else.** A cursor hidden behind
        // the outline it is dragging would be the one pixel the user is actually steering by.
        for r in damage {
            if let Some(o) = outline {
                draw_outline(fb, o, *r);
            }
            draw_cursor(fb, pointer, *r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use libkern::abi::{
        KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_RIGHTALT, KEY_RIGHTCTRL,
        KEY_RIGHTMETA, KEY_RIGHTSHIFT,
    };

    /// Two ordinary (non-modifier) keycodes — `a` and `b` in the Linux table `libkern::abi`
    /// mirrors. Named here rather than imported because `abi` exports the modifiers this
    /// module cares about and not every letter.
    const KEY_A: u16 = 30;
    /// See [`KEY_A`].
    const KEY_B: u16 = 48;

    #[test]
    fn the_cursors_hotspot_is_its_top_left() {
        // Where an arrow points. A centre hotspot clicks half a sprite from where it looks.
        let r = cursor_rect(Point::new(40, 30));
        assert_eq!(r.origin, Point::new(40, 30));
        assert_eq!(r.size, libdraw::geom::Size::new(CURSOR_W, CURSOR_H));
    }

    #[test]
    fn the_sprite_matches_the_size_it_claims() {
        // A row longer than `CURSOR_W` draws outside `cursor_rect`, so the damage the
        // compositor computes from that rectangle would leave a trail behind the pointer.
        assert_eq!(CURSOR.len(), CURSOR_H as usize);
        for (i, line) in CURSOR.iter().enumerate() {
            assert!(
                line.chars().count() <= CURSOR_W as usize,
                "row {i} is {} wide, past the declared {CURSOR_W}",
                line.chars().count()
            );
        }
    }

    #[test]
    fn a_cursor_at_the_screen_edge_is_clipped_rather_than_wrapped() {
        use libdraw::format::PixelFormat;
        use libdraw::framebuffer::{Geometry, MemFramebuffer};
        let mut fb = MemFramebuffer::new(Geometry::packed(40, 40, PixelFormat::XRGB8888));
        let clip = Rect::new(0, 0, 40, 40);
        // Off the right and bottom edges, and off the top-left with negative coordinates.
        draw_cursor(&mut fb, Point::new(36, 36), clip);
        draw_cursor(&mut fb, Point::new(-4, -4), clip);
        // Nothing wrapped to the opposite side: the far corner from each is untouched.
        assert_eq!(fb.get_pixel(0, 39), Some(Rgb::new(0, 0, 0)));
    }

    #[test]
    fn an_outlines_edges_are_four_strips_that_cover_its_border_and_nothing_inside() {
        // **The arithmetic the whole gesture's damage rests on.** These strips are what gets
        // repainted per pointer motion, and they are also what *erases* the outline — so a
        // strip short of an edge leaves a line of it behind, and a strip that covered the middle
        // would put the full recompose back that this exists to avoid.
        let r = Rect::new(10, 20, 100, 60);
        let e = outline_edges(r);
        for strip in e {
            assert!(strip.intersect(&r).is_some(), "a strip outside the rectangle: {strip:?}");
        }
        // Every border pixel is in some strip, and no interior pixel is in any.
        for y in r.origin.y..r.bottom() as i32 {
            for x in r.origin.x..r.right() as i32 {
                let on_border = x < r.origin.x + OUTLINE_W as i32
                    || x >= r.right() as i32 - OUTLINE_W as i32
                    || y < r.origin.y + OUTLINE_W as i32
                    || y >= r.bottom() as i32 - OUTLINE_W as i32;
                let covered = e.iter().any(|s| s.contains(x, y));
                assert_eq!(covered, on_border, "({x},{y}) border={on_border} covered={covered}");
            }
        }
    }

    #[test]
    fn a_degenerate_outline_stays_inside_itself() {
        // A rectangle thinner than the outline is what a resize clamped to its floor produces
        // in one axis; the strips must not reach outside it, or the damage names pixels the
        // repaint will not restore.
        for r in [Rect::new(5, 5, 1, 40), Rect::new(5, 5, 40, 1), Rect::new(5, 5, 0, 0)] {
            for strip in outline_edges(r) {
                assert!(
                    strip.is_empty() || strip.intersect(&r) == Some(strip),
                    "{strip:?} leaves {r:?}"
                );
            }
        }
    }

    #[test]
    fn the_outline_draws_only_inside_its_clip() {
        use libdraw::format::PixelFormat;
        use libdraw::framebuffer::{Geometry, MemFramebuffer};
        let mut fb = MemFramebuffer::new(Geometry::packed(60, 60, PixelFormat::XRGB8888));
        let bg = Rgb::new(0x11, 0x22, 0x33);
        for y in 0..60 {
            for x in 0..60 {
                fb.put_pixel(x, y, bg);
            }
        }
        // **The clip is on the outline's corner, not inside it.** A clip in the middle of a
        // large outline intersects no strip at all — the first version of this test used one,
        // and its "something was drawn" assertion was what caught that rather than the clip.
        let clip = Rect::new(8, 8, 4, 4);
        draw_outline(&mut fb, Rect::new(8, 8, 40, 40), clip);
        let mut ink = 0;
        for y in 0..60u32 {
            for x in 0..60u32 {
                if fb.get_pixel(x, y) != Some(bg) {
                    ink += 1;
                    assert!(clip.contains(x as i32, y as i32), "ink at ({x},{y}) outside the clip");
                }
            }
        }
        assert!(ink > 0, "the clip covers the outline's corner, so something must be drawn");
    }

    #[test]
    fn presenting_draws_the_outline_over_the_stack_and_into_every_damage_rectangle() {
        // The same rule the cursor has, and for the same reason: `serve_input` hands one damage
        // list holding where the outline *was* and where it is, and a `present_into` that drew
        // into only the first would leave it erased at its destination.
        let s = WindowStack::new();
        let src = MapSource::default();
        let mut fb = big_screen();
        let outline = Rect::new(20, 20, 60, 40);
        let corners = [Rect::new(20, 20, 4, 4), Rect::new(76, 56, 4, 4)];
        s.present_into(
            &mut fb,
            Rgb::BLACK,
            &src,
            &corners,
            Point::new(200, 200),
            Some(outline),
        );
        for c in corners {
            let painted = (c.origin.y..c.bottom() as i32)
                .flat_map(|y| (c.origin.x..c.right() as i32).map(move |x| (x, y)))
                .filter(|(x, y)| fb.get_pixel(*x as u32, *y as u32) == Some(OUTLINE_COLOUR))
                .count();
            assert!(painted > 0, "the outline was not drawn into {c:?}");
        }
    }

    #[test]
    fn the_cursor_draws_only_inside_its_clip() {
        use libdraw::format::PixelFormat;
        use libdraw::framebuffer::{Geometry, MemFramebuffer};
        let mut fb = MemFramebuffer::new(Geometry::packed(60, 60, PixelFormat::XRGB8888));
        let bg = Rgb::new(0x11, 0x22, 0x33);
        for y in 0..60 {
            for x in 0..60 {
                fb.put_pixel(x, y, bg);
            }
        }
        draw_cursor(&mut fb, Point::new(10, 10), Rect::new(10, 10, 4, 4));
        for y in 0..60u32 {
            for x in 0..60u32 {
                if !Rect::new(10, 10, 4, 4).contains(x as i32, y as i32) {
                    assert_eq!(fb.get_pixel(x, y), Some(bg), "ink at ({x},{y}) outside the clip");
                }
            }
        }
    }

    #[test]
    fn a_repeat_waits_the_delay_and_then_runs_at_the_interval() {
        let mut r = Repeat::armed(30, 0, 1, 1_000);
        assert!(!r.due(1_000), "not immediately");
        assert!(!r.due(1_000 + REPEAT_DELAY_NS - 1), "not a nanosecond early");
        assert!(r.due(1_000 + REPEAT_DELAY_NS), "and then it fires");
        assert!(!r.due(1_000 + REPEAT_DELAY_NS), "once per deadline");
        assert!(r.due(1_000 + REPEAT_DELAY_NS + REPEAT_INTERVAL_NS), "then at the interval");
    }

    #[test]
    fn a_late_wakeup_does_not_slow_the_repeat_rate() {
        // Advancing by `now + interval` rather than `deadline + interval` makes every late
        // wake-up push the next repeat further out, so a busy compositor repeats slower and
        // slower. The tick that wakes this is 10 ms, so late is the normal case.
        let start = 1_000;
        let mut r = Repeat::armed(30, 0, 1, start);
        let first = start + REPEAT_DELAY_NS;
        assert!(r.due(first + REPEAT_INTERVAL_NS / 2), "fired half an interval late");
        assert_eq!(
            r.next_at,
            first + REPEAT_INTERVAL_NS,
            "the next one is still on the original cadence, not shifted by the lateness"
        );
    }

    #[test]
    fn a_very_late_wakeup_does_not_fire_a_burst() {
        // A stalled compositor coming back must not deliver every repeat it missed. One
        // repeat, then back on cadence from now.
        let mut r = Repeat::armed(30, 0, 1, 0);
        let much_later = REPEAT_DELAY_NS + REPEAT_INTERVAL_NS * 1_000;
        assert!(r.due(much_later));
        assert!(!r.due(much_later), "and only one");
        assert_eq!(r.next_at, much_later + REPEAT_INTERVAL_NS);
    }

    #[test]
    fn modifiers_are_frozen_at_the_press() {
        // A repeat is that press continuing. Re-reading modifiers would turn `A` into `a`
        // halfway through a held run if the user let go of shift.
        let r = Repeat::armed(30, 0x0001, 7, 0);
        assert_eq!(r.modifiers, 0x0001);
        assert_eq!(r.keycode, 30);
        assert_eq!(r.window, 7);
    }

    #[test]
    fn focus_transition_is_silent_when_focus_did_not_move() {
        // The case that happens on every commit: recomputed, unchanged, nothing sent.
        assert_eq!(focus_transition(Some(3), Some(3)), None);
        assert_eq!(focus_transition(None, None), None);
    }

    #[test]
    fn focus_transition_names_both_halves() {
        // Both, because a client told only that it *gained* focus would keep a caret
        // blinking behind whatever took focus from it.
        assert_eq!(focus_transition(Some(1), Some(2)), Some((Some(1), Some(2))));
        assert_eq!(focus_transition(None, Some(2)), Some((None, Some(2))), "first window");
        assert_eq!(focus_transition(Some(1), None), Some((Some(1), None)), "last window went");
    }
    use alloc::collections::BTreeMap;
    use alloc::vec;
    use libdraw::framebuffer::MemFramebuffer;
    use librsproto::surface::SURFACE_FORMAT_XRGB8888;

    /// A `BufferSource` backed by owned vectors.
    #[derive(Default)]
    struct MapSource(BTreeMap<(u32, u32), Vec<u8>>);

    impl MapSource {
        fn put(&mut self, window: u32, buffer: u32, g: Geometry, colour: Rgb) {
            let mut px = vec![0u8; g.byte_len()];
            let word = g.format.encode(colour).to_le_bytes();
            for y in 0..g.height {
                for x in 0..g.width {
                    let off = g.offset_of(x, y).unwrap();
                    px[off..off + 4].copy_from_slice(&word);
                }
            }
            self.0.insert((window, buffer), px);
        }
    }

    /// A distinct colour per source row. A uniform fill cannot detect a row-stride error —
    /// a skew just moves one shade onto an identical one — which is exactly how
    /// `the_guest_configuration_composites_a_client_surface` passed against a compositor
    /// reading the source at `width * 4` (PR #175 review, finding 1).
    fn row_colour(y: u32) -> Rgb {
        Rgb::new(0x20 + y as u8, 0x40, 0x80)
    }

    impl MapSource {
        /// Fill with [`row_colour`], so which source row reached the screen is *readable
        /// from the screen*.
        fn put_striped(&mut self, window: u32, buffer: u32, g: Geometry) {
            let mut px = vec![0u8; g.byte_len()];
            for y in 0..g.height {
                let word = g.format.encode(row_colour(y)).to_le_bytes();
                for x in 0..g.width {
                    let off = g.offset_of(x, y).unwrap();
                    px[off..off + 4].copy_from_slice(&word);
                }
            }
            self.0.insert((window, buffer), px);
        }
    }

    impl BufferSource for MapSource {
        fn pixels(&self, window: u32, buffer: u32) -> Option<&[u8]> {
            self.0.get(&(window, buffer)).map(|v| v.as_slice())
        }
    }

    fn geom(w: u32, h: u32) -> Geometry {
        Geometry::packed(w, h, PixelFormat::XRGB8888)
    }

    fn attach(window: u32, buffer: u32, w: u32, h: u32) -> AttachBufferRequest {
        AttachBufferRequest {
            window,
            buffer,
            width: w,
            height: h,
            pitch: w * 4,
            format: SURFACE_FORMAT_XRGB8888,
        }
    }

    /// Create a window and mark it configured — which is what the bin does the instant a
    /// window is created with no manager attached.
    ///
    /// Compositing has been gated on the initial `Configure` since B4, so a test that creates,
    /// attaches and commits without one is modelling a client that jumped the handshake, and
    /// gets what that client gets: nothing on screen.
    fn shown(s: &mut WindowStack, req: &CreateWindowRequest) -> u32 {
        let id = s.create(req).expect("create");
        s.mark_configured(id);
        id
    }

    fn commit(window: u32, buffer: u32) -> CommitRequest {
        CommitRequest {
            window,
            buffer,
            damage_x: 0,
            damage_y: 0,
            damage_w: 0,
            damage_h: 0,
        }
    }

    fn screen() -> MemFramebuffer {
        MemFramebuffer::new(Geometry::with_pitch(32, 16, 140, PixelFormat::XRGB8888).unwrap())
    }

    /// A screen with room for the 12×16 cursor sprite away from its edges — [`screen`] is
    /// 32×16, so a sprite anywhere but the top-left corner falls off it.
    fn big_screen() -> MemFramebuffer {
        MemFramebuffer::new(Geometry::with_pitch(96, 96, 400, PixelFormat::XRGB8888).unwrap())
    }

    /// Two overlapping windows and a screen already showing them, for the restack tests.
    ///
    /// Returns the stack, the pixel source, and `(bottom, top)`.
    fn overlapping_pair() -> (WindowStack, MapSource, (u32, u32)) {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let bottom = shown(&mut s, &CreateWindowRequest::new(20, 10, Role::Normal));
        let top = shown(&mut s, &CreateWindowRequest::new(20, 10, Role::Normal));
        for (id, colour) in [(bottom, Rgb::new(0xFF, 0, 0)), (top, Rgb::new(0, 0xFF, 0))] {
            s.attach(&attach(id, 0, 20, 10)).unwrap();
            src.put(id, 0, geom(20, 10), colour);
            s.commit(&commit(id, 0)).unwrap();
        }
        // Overlapping by half, so a reorder changes pixels rather than merely the order.
        let _ = s.place(bottom, Point::new(0, 0)).unwrap();
        let _ = s.place(top, Point::new(10, 3)).unwrap();
        (s, src, (bottom, top))
    }

    #[test]
    fn the_region_a_raise_reports_is_the_whole_of_what_it_changed() {
        // **The guard on narrowing a restack's repaint from the whole screen to one rectangle.**
        // The claim is that reordering one window can only change pixels inside that window's
        // own bounds, because every other pair keeps its relative order. If that is wrong
        // anywhere, painting only the reported region leaves stale pixels — so this paints the
        // reported region into one screen, recomposes the whole of another, and compares every
        // byte.
        let (mut s, src, (bottom, _top)) = overlapping_pair();
        let mut painted = screen();
        let full = Rect::new(0, 0, 32, 16);
        s.present_into(&mut painted, Rgb::BLACK, &src, &[full], Point::new(100, 100), None);

        let d = s.raise(bottom).expect("in the stack");
        s.present_into(&mut painted, Rgb::BLACK, &src, &[d.rect()], Point::new(100, 100), None);

        let mut reference = screen();
        s.present_into(&mut reference, Rgb::BLACK, &src, &[full], Point::new(100, 100), None);
        assert_eq!(
            painted.bytes(),
            reference.bytes(),
            "repainting only the raised window's rectangle must leave the screen identical to a \
             full recompose"
        );
    }

    #[test]
    fn the_region_a_lower_reports_is_the_whole_of_what_it_changed() {
        // The same claim in the other direction: what a lowered window uncovers is inside its
        // own rectangle too.
        let (mut s, src, (_bottom, top)) = overlapping_pair();
        let mut painted = screen();
        let full = Rect::new(0, 0, 32, 16);
        s.present_into(&mut painted, Rgb::BLACK, &src, &[full], Point::new(100, 100), None);

        let d = s.lower(top).expect("in the stack");
        s.present_into(&mut painted, Rgb::BLACK, &src, &[d.rect()], Point::new(100, 100), None);

        let mut reference = screen();
        s.present_into(&mut reference, Rgb::BLACK, &src, &[full], Point::new(100, 100), None);
        assert_eq!(painted.bytes(), reference.bytes(), "a lower's region must cover its own");
    }

    #[test]
    fn a_restack_that_changes_no_order_reports_no_region() {
        // Click-to-focus raises on every press, including on the window already on top, and a
        // manager's window list does the same. Reporting a region for those made every one of
        // them a repaint.
        let (mut s, _src, (bottom, top)) = overlapping_pair();
        assert!(s.raise(top).expect("in the stack").is_empty(), "already topmost");
        assert!(s.lower(bottom).expect("in the stack").is_empty(), "already bottom-most");
        assert!(s.raise_above(top, bottom).expect("both exist").is_empty(), "already above it");
        assert_eq!(
            s.windows().iter().map(|w| w.id).collect::<Vec<_>>(),
            vec![bottom, top],
            "and none of them reordered anything"
        );
    }

    #[test]
    fn a_raise_that_does_reorder_reports_the_window_it_moved() {
        let (mut s, _src, (bottom, _top)) = overlapping_pair();
        let want = s.window(bottom).expect("in the stack").bounds();
        assert_eq!(s.raise(bottom).expect("in the stack").rect(), want);
    }

    #[test]
    fn a_repeated_state_request_is_not_a_second_event() {
        // **The bound this event needs.** It is the only manager event a client's own rate
        // drives, and the manager's queue does not coalesce and discards its oldest — so a
        // client looping on one state would push a `WindowCreated` off the front of a shell's
        // view of the world. `SetTitle` makes the same argument for an unchanged title.
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        assert!(s.note_state_request(w, 2), "the first ask is news");
        assert!(!s.note_state_request(w, 2), "the same ask again is not");
        assert!(s.note_state_request(w, 0), "and a different one is");
        assert!(!s.note_state_request(w, 0));
        assert!(
            !s.note_state_request(w + 100, 1),
            "a window that does not exist has nobody to tell about it"
        );
    }

    #[test]
    fn the_work_area_shrinks_by_every_panels_strut_not_only_the_first() {
        // What `QueryLayout` answers with, and the reason a manager cannot compute it: a shell
        // subtracting only its own bars is right until some other client declares a strut.
        let mut s = WindowStack::new();
        let screen = Rect::new(0, 0, 100, 100);
        assert_eq!(s.work_area(screen), screen, "no panels, no reservation");
        let _ = shown(&mut s, &CreateWindowRequest::new(100, 10, Role::Panel { dock: Edge::Top, reserve: 10 }));
        let _ = shown(&mut s, &CreateWindowRequest::new(100, 8, Role::Panel { dock: Edge::Bottom, reserve: 8 }));
        assert_eq!(
            s.work_area(screen),
            Rect::new(0, 10, 100, 82),
            "both struts, not just the first"
        );
    }

    #[test]
    fn a_manager_changing_a_windows_state_invalidates_what_it_last_asked_to_be() {
        // **The dedup compares against a value the manager also changes.** A client minimises
        // through its title bar; the user restores through the taskbar, which is `SetMinimized`
        // and not a client request; the next identical minimise was dropped as a repeat, and the
        // client was told it had succeeded. The button worked once and then never again
        // (PR #249 review, blocking 1).
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest::new(80, 60, Role::Normal)).unwrap();
        assert!(s.note_state_request(w, 1), "the first minimise is news");

        // What the shell does with it, and then what the taskbar does — neither is a client
        // request, and both are reasons the shadow no longer describes the window.
        s.set_minimized(w, true).unwrap();
        s.clear_state_request(w);
        s.set_minimized(w, false).unwrap();
        s.clear_state_request(w);

        assert!(s.note_state_request(w, 1), "and minimising again must reach the manager");
    }

    #[test]
    fn a_created_window_gets_a_fresh_id_and_keeps_its_role() {
        let mut s = WindowStack::new();
        let a = s
            .create(&CreateWindowRequest::new(8, 8, Role::Normal))
            .unwrap();
        let b = s
            .create(&CreateWindowRequest::new(32, 4, Role::Panel { dock: Edge::Top, reserve: 4 }))
            .unwrap();
        assert_ne!(a, b, "ids must be unique");
        assert_eq!(s.window(a).unwrap().role, Role::Normal);
        assert_eq!(s.window(b).unwrap().role, Role::Panel { dock: Edge::Top, reserve: 4 });
    }

    #[test]
    fn a_popup_must_name_a_parent_that_exists() {
        let mut s = WindowStack::new();
        assert_eq!(
            s.create(&CreateWindowRequest::new(4, 4, Role::Popup { parent: 99 })),
            Err(StackError::NoSuchParent)
        );
        let p = s
            .create(&CreateWindowRequest::new(8, 8, Role::Normal))
            .unwrap();
        assert!(
            s.create(&CreateWindowRequest::new(4, 4, Role::Popup { parent: p }))
            .is_ok()
        );
    }

    #[test]
    fn destroying_a_parent_takes_its_popups_with_it() {
        let mut s = WindowStack::new();
        let p = s
            .create(&CreateWindowRequest::new(8, 8, Role::Normal))
            .unwrap();
        let menu = s
            .create(&CreateWindowRequest::new(4, 4, Role::Popup { parent: p }))
            .unwrap();
        let other = s
            .create(&CreateWindowRequest::new(8, 8, Role::Normal))
            .unwrap();
        s.destroy(p).unwrap();
        assert!(s.window(menu).is_none(), "an orphaned popup has no defined stacking position");
        assert!(s.window(other).is_some(), "unrelated windows are untouched");
    }

    #[test]
    fn commit_returns_the_previous_buffer_to_release_not_the_new_one() {
        // Releasing the newly committed buffer would hand the client back memory the
        // compositor is about to read — the tearing this protocol exists to prevent.
        let mut s = WindowStack::new();
        let w = s
            .create(&CreateWindowRequest::new(4, 4, Role::Normal))
            .unwrap();
        s.attach(&attach(w, 0, 4, 4)).unwrap();
        s.attach(&attach(w, 1, 4, 4)).unwrap();

        assert_eq!(s.commit(&commit(w, 0)).unwrap(), None, "nothing was on screen before");
        assert_eq!(s.commit(&commit(w, 1)).unwrap(), Some(0), "buffer 0 is now free");
        assert_eq!(s.commit(&commit(w, 0)).unwrap(), Some(1));
        // Re-committing the same buffer frees nothing.
        assert_eq!(s.commit(&commit(w, 0)).unwrap(), None);
    }

    #[test]
    fn committing_an_unattached_buffer_is_refused() {
        let mut s = WindowStack::new();
        let w = s
            .create(&CreateWindowRequest::new(4, 4, Role::Normal))
            .unwrap();
        assert_eq!(s.commit(&commit(w, 7)), Err(StackError::NoSuchBuffer));
        assert_eq!(s.commit(&commit(99, 0)), Err(StackError::NoSuchWindow));
    }

    #[test]
    fn re_attaching_a_free_buffer_replaces_it_and_the_committed_one_is_refused() {
        // **How a client resizes** (M9 Part D). The protocol has no detach, so a resize that
        // needed new ids would grow this window's buffer list — and the compositor's mappings
        // — by two for every maximise and every restore. Replacing is bounded by construction.
        //
        // Refused for the *committed* buffer, whose pixels the compositor may be reading: that
        // would change the screen with no commit, which is the tearing the buffer protocol
        // exists to make unrepresentable. A double-buffered client always has a free buffer to
        // replace first, so nothing an honest resize does is refused.
        let mut s = WindowStack::new();
        let w = s
            .create(&CreateWindowRequest::new(4, 4, Role::Normal))
            .unwrap();
        s.attach(&attach(w, 0, 4, 4)).unwrap();
        s.attach(&attach(w, 1, 4, 4)).unwrap();
        assert_eq!(s.windows()[0].buffers.len(), 2);

        s.attach(&attach(w, 0, 8, 8)).expect("a free buffer takes new memory under its own id");
        assert_eq!(s.windows()[0].buffers.len(), 2, "replaced, not added");
        let g = s.windows()[0].buffers.iter().find(|b| b.id == 0).unwrap().geometry;
        assert_eq!(
            (g.width, g.height),
            (8, 8),
            "and the new geometry is what the compositor will read it at"
        );

        s.commit(&commit(w, 0)).unwrap();
        assert_eq!(
            s.attach(&attach(w, 0, 16, 16)),
            Err(StackError::DuplicateBuffer),
            "the buffer on screen is not replaceable"
        );
        s.attach(&attach(w, 1, 16, 16)).expect("the other one still is");
    }

    #[test]
    fn panels_reserve_space_and_normal_windows_do_not() {
        let mut s = WindowStack::new();
        let screen = Rect::new(0, 0, 100, 50);
        assert_eq!(s.work_area(screen), screen, "no panels, no reservation");

        s.create(&CreateWindowRequest::new(100, 8, Role::Panel { dock: Edge::Top, reserve: 8 }))
        .unwrap();
        s.create(&CreateWindowRequest::new(100, 6, Role::Panel { dock: Edge::Bottom, reserve: 6 }))
        .unwrap();
        s.create(&CreateWindowRequest::new(40, 40, Role::Normal)).unwrap();

        assert_eq!(s.work_area(screen), Rect::new(0, 8, 100, 36));
    }

    #[test]
    fn every_edge_reserves_on_its_own_axis() {
        let mut s = WindowStack::new();
        for (edge, reserve) in
            [(Edge::Left, 3u32), (Edge::Right, 5), (Edge::Top, 2), (Edge::Bottom, 4)]
        {
            s.create(&CreateWindowRequest::new(1, 1, Role::Panel { dock: edge, reserve }))
            .unwrap();
        }
        assert_eq!(s.work_area(Rect::new(0, 0, 100, 50)), Rect::new(3, 2, 92, 44));
    }

    #[test]
    fn over_reservation_empties_the_work_area_rather_than_inverting_it() {
        let mut s = WindowStack::new();
        s.create(&CreateWindowRequest::new(1, 1, Role::Panel { dock: Edge::Top, reserve: 90 }))
        .unwrap();
        let wa = s.work_area(Rect::new(0, 0, 100, 50));
        assert_eq!(wa.size.h, 0, "a negative height would be a clipping catastrophe");
        assert!(wa.is_empty());
    }

    #[test]
    fn focus_skips_panels_and_follows_the_top_of_the_stack() {
        let mut s = WindowStack::new();
        let a = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
        let b = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
        s.create(&CreateWindowRequest::new(32, 4, Role::Panel { dock: Edge::Top, reserve: 4 }))
        .unwrap();
        // The panel is topmost, but must not take focus.
        assert_eq!(s.focus_candidate(), Some(b));
        s.raise(a).unwrap();
        assert_eq!(s.focus_candidate(), Some(a));
    }

    #[test]
    fn a_stack_with_only_panels_has_nothing_to_focus() {
        let mut s = WindowStack::new();
        s.create(&CreateWindowRequest::new(32, 4, Role::Panel { dock: Edge::Top, reserve: 4 }))
        .unwrap();
        assert_eq!(s.focus_candidate(), None);
    }

    /// The stack's window ids, bottom-first.
    fn order(s: &WindowStack) -> Vec<u32> {
        s.windows().iter().map(|w| w.id).collect()
    }

    #[test]
    fn moving_a_window_dirties_where_it_was_and_where_it_is() {
        // **A rectangle cannot express "old minus new"**, so the union is the tightest correct
        // answer — and computing it after the move, the way every other path computes `dirty`,
        // would repaint the destination and leave the window's old pixels on screen. M5 shipped
        // that bug once for a resized buffer; `place` returns the damage so it cannot be
        // computed the wrong way here.
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        src.put(w, 0, geom(8, 8), Rgb::new(1, 2, 3));
        s.commit(&commit(w, 0)).unwrap();

        let dirty = s.place(w, Point::new(20, 10)).unwrap();
        assert_eq!(dirty.rect(), Rect::new(0, 0, 28, 18), "the union of (0,0,8,8) and (20,10,8,8)");
        assert_eq!(s.window(w).unwrap().bounds(), Rect::new(20, 10, 8, 8));

        // A move that changes nothing still names the window's own rectangle rather than
        // nothing — the union of a rect with itself — which is correct and costs one window.
        assert_eq!(s.place(w, Point::new(20, 10)).unwrap().rect(), Rect::new(20, 10, 8, 8));
    }

    #[test]
    fn placing_a_window_that_has_never_committed_dirties_nothing() {
        // Not an edge case: placing a window *before* its first commit is exactly what a
        // manager does, and it is the whole point of the initial-configure handshake. An
        // uncommitted window is skipped by compositing, so moving it paints over nothing and
        // reveals nothing — reporting its bounds would repaint a region for no reason on every
        // window launch.
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest::new(64, 64, Role::Normal)).unwrap();
        let dirty = s.place(w, Point::new(100, 100)).unwrap();
        assert!(dirty.is_empty(), "an unmapped window is not on screen");
        assert_eq!(s.window(w).unwrap().origin, Point::new(100, 100), "but it did move");
    }

    #[test]
    fn a_zero_sized_damage_is_the_identity_when_folded_into_a_wider_region() {
        // The convention this crate uses for "nothing changed", and the reason `place` can
        // return one rather than an `Option`: a caller unioning it into a wider `dirty` needs no
        // special case, and a caller repainting it directly paints nothing, because `Rect`'s
        // bounds are exclusive.
        let empty = Rect::new(100, 100, 0, 0);
        let real = Rect::new(4, 4, 10, 10);
        assert_eq!(union(empty, real), real);
        assert_eq!(union(real, empty), real);
        assert!(!empty.contains(100, 100), "a zero rect contains no pixel");
    }

    #[test]
    fn placing_a_window_that_does_not_exist_is_refused() {
        let mut s = WindowStack::new();
        assert_eq!(s.place(99, Point::new(1, 1)), Err(StackError::NoSuchWindow));
    }

    #[test]
    fn lower_sends_a_window_under_everything_and_raise_brings_it_back() {
        let mut s = WindowStack::new();
        let ids: Vec<u32> = (0..3)
            .map(|_| {
                s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap()
            })
            .collect();
        assert_eq!(order(&s), ids, "creation order is bottom-first");

        s.lower(ids[2]).unwrap();
        assert_eq!(order(&s), [ids[2], ids[0], ids[1]]);
        s.raise(ids[2]).unwrap();
        assert_eq!(order(&s), [ids[0], ids[1], ids[2]]);
        assert_eq!(s.lower(99), Err(StackError::NoSuchWindow));
    }

    #[test]
    fn raise_above_moves_one_window_and_leaves_the_rest_in_order() {
        // What alt-tab needs. A full `raise` would put the window on top and reorder everything
        // between, which the user sees as the rest of the stack shuffling behind the one they
        // asked for.
        let mut s = WindowStack::new();
        let ids: Vec<u32> = (0..4)
            .map(|_| {
                s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap()
            })
            .collect();

        // Move the bottom window to just above the second — the case where removing it shifts
        // the target's index, which an index captured before the removal gets wrong by one.
        s.raise_above(ids[0], ids[1]).unwrap();
        assert_eq!(order(&s), [ids[1], ids[0], ids[2], ids[3]]);

        // And downward, where no such shift happens: the two directions are different code
        // paths through the same two lines.
        s.raise_above(ids[3], ids[1]).unwrap();
        assert_eq!(order(&s), [ids[1], ids[3], ids[0], ids[2]]);

        // Already above it: a no-op, not an error — a shell walking a window list should not
        // have to special-case the window it is already above.
        let before = order(&s);
        s.raise_above(ids[3], ids[3]).unwrap();
        assert_eq!(order(&s), before);
        assert_eq!(s.raise_above(ids[0], 99), Err(StackError::NoSuchWindow));
        assert_eq!(s.raise_above(99, ids[0]), Err(StackError::NoSuchWindow));
    }

    #[test]
    fn compositing_draws_committed_windows_bottom_first() {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let red = Rgb::new(0xC0, 0x10, 0x10);
        let blue = Rgb::new(0x10, 0x10, 0xC0);

        let a = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
        s.attach(&attach(a, 0, 8, 8)).unwrap();
        src.put(a, 0, geom(8, 8), red);
        s.commit(&commit(a, 0)).unwrap();

        let b = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
        s.attach(&attach(b, 0, 8, 8)).unwrap();
        src.put(b, 0, geom(8, 8), blue);
        s.commit(&commit(b, 0)).unwrap();
        // Damage ignored: the test composites the whole screen below.
        let _ = s.place(b, Point::new(4, 4)).unwrap();

        let mut fb = screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, Rgb::BLACK, &src, &[full]);

        assert_eq!(fb.get_pixel(0, 0), Some(red), "the lower window");
        assert_eq!(fb.get_pixel(6, 6), Some(blue), "the upper window wins the overlap");
        assert_eq!(fb.get_pixel(10, 10), Some(blue));
        assert_eq!(fb.get_pixel(20, 2), Some(Rgb::BLACK), "background elsewhere");
    }

    #[test]
    fn a_window_with_no_committed_buffer_shows_background_not_garbage() {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        // **Configured**, or B4's gate skips the window one line above the `committed` guard
        // and this test passes without ever reaching what it is named for — the hazard the
        // comment below already warned about, for the guard after this one (PR #218 review,
        // finding 1).
        let w = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        // **The pixels must exist**, or the source-resolution guard catches the window and
        // this test says nothing about the `committed` guard it is named for. In the server
        // an attached buffer is real mapped memory holding whatever the client last put
        // there; showing it before a commit is the bug.
        src.put(w, 0, geom(8, 8), Rgb::new(0xC0, 0x10, 0x10));
        let mut fb = screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, Rgb::new(1, 2, 3), &src, &[full]);
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(1, 2, 3)), "uncommitted pixels leaked");
    }

    #[test]
    fn destroying_an_ancestor_reaches_grandchildren() {
        // A submenu is a popup parented to a popup. One pass over direct children leaves it
        // alive with a dead parent — still compositing, still eligible for focus.
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        let menu =
            s.create(&CreateWindowRequest::new(4, 4, Role::Popup { parent: w }))
                .unwrap();
        let sub = s
            .create(&CreateWindowRequest::new(2, 2, Role::Popup { parent: menu }))
            .unwrap();
        let dlg = s
            .create(&CreateWindowRequest::new(2, 2, Role::Dialog { parent: sub }))
            .unwrap();
        // Configured, because the last assertion is about focus and since B4 a window that is
        // not on screen is not a focus candidate.
        let other = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));

        s.destroy(w).unwrap();
        for gone in [menu, sub, dlg] {
            assert!(s.window(gone).is_none(), "window {gone} outlived its ancestry");
        }
        assert!(s.window(other).is_some(), "unrelated windows are untouched");
        assert_eq!(s.focus_candidate(), Some(other));
    }

    #[test]
    fn strut_accumulation_saturates_rather_than_wrapping() {
        // `reserve` is bounded at the protocol edge, but `Role`'s fields are **public**, so
        // the library must not rely on that: anything constructing a `Role` in Rust bypasses
        // the parser entirely. A wrap here is worse than a panic — in release it returns the
        // *full* screen as the work area, silently defeating the clamp.
        //
        // Sized to actually overflow: two reserves near `u32::MAX` do, where a hundred
        // legitimate ones do not. An earlier version of this test used 128 panels at the
        // protocol bound (8.4M total) and could not tell `wrapping_add` from
        // `saturating_add` at all.
        let mut s = WindowStack::new();
        for _ in 0..2 {
            // Exactly 2^31 each: the sum is 2^32, which wraps to **zero** — the case that
            // silently returns the full screen. `u32::MAX - 1` twice wraps to a still-huge
            // number the clamp handles anyway, so it proves nothing; that was this test's
            // second wrong version.
            s.create(&CreateWindowRequest::new(
                1,
                1,
                Role::Panel { dock: Edge::Top, reserve: 0x8000_0000 },
            ))
            .unwrap();
        }
        let wa = s.work_area(Rect::new(0, 0, 100, 50));
        assert_eq!(wa.size.h, 0, "the work area must clamp to empty, not wrap open");
        assert!(wa.is_empty());
    }

    #[test]
    fn a_realistic_pair_of_bars_still_leaves_a_work_area() {
        // The saturation guard must not swallow ordinary values.
        let mut s = WindowStack::new();
        s.create(&CreateWindowRequest::new(100, 32, Role::Panel { dock: Edge::Top, reserve: 32 }))
        .unwrap();
        s.create(&CreateWindowRequest::new(100, 28, Role::Panel { dock: Edge::Bottom, reserve: 28 }))
        .unwrap();
        assert_eq!(s.work_area(Rect::new(0, 0, 200, 100)), Rect::new(0, 32, 200, 40));
    }

    #[test]
    fn default_and_new_share_one_id_space() {
        let mut d = WindowStack::default();
        let mut n = WindowStack::new();
        let req = CreateWindowRequest::new(1, 1, Role::Normal);
        assert_eq!(d.create(&req).unwrap(), n.create(&req).unwrap());
    }

    #[test]
    fn a_buffer_the_source_cannot_resolve_is_skipped_not_drawn() {
        // The server maps buffers; a client can commit one whose mapping has gone. Drawing
        // from an unresolvable buffer would read whatever memory happened to be there.
        let mut s = WindowStack::new();
        let src = MapSource::default(); // deliberately empty
        // Configured, so B4's gate is not what skips this window — see finding 1.
        let w = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        s.commit(&commit(w, 0)).unwrap();

        let mut fb = screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, Rgb::new(9, 9, 9), &src, &[full]);
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(9, 9, 9)));
    }

    #[test]
    fn a_short_buffer_is_skipped_rather_than_read_past_its_end() {
        struct Short;
        impl BufferSource for Short {
            fn pixels(&self, _w: u32, _b: u32) -> Option<&[u8]> {
                Some(&[0u8; 4]) // far shorter than an 8x8 buffer
            }
        }
        let mut s = WindowStack::new();
        // Configured, so B4's gate is not what skips this window — see finding 1.
        let w = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        s.commit(&commit(w, 0)).unwrap();

        let mut fb = screen();
        let full = fb.geometry().bounds();
        s.compose_into(&mut fb, Rgb::new(4, 5, 6), &Short, &[full]);
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(4, 5, 6)));
    }

    #[test]
    fn raising_a_window_changes_what_wins_the_overlap() {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let red = Rgb::new(0xC0, 0x10, 0x10);
        let blue = Rgb::new(0x10, 0x10, 0xC0);
        for (id, colour) in [(1u32, red), (2u32, blue)] {
            let w = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
            assert_eq!(w, id);
            s.attach(&attach(w, 0, 8, 8)).unwrap();
            src.put(w, 0, geom(8, 8), colour);
            s.commit(&commit(w, 0)).unwrap();
        }
        let full = screen().geometry().bounds();

        let mut fb = screen();
        s.compose_into(&mut fb, Rgb::BLACK, &src, &[full]);
        assert_eq!(fb.get_pixel(0, 0), Some(blue), "window 2 is on top");

        s.raise(1).unwrap();
        let mut fb = screen();
        s.compose_into(&mut fb, Rgb::BLACK, &src, &[full]);
        assert_eq!(fb.get_pixel(0, 0), Some(red), "window 1 now is");
    }

    #[test]
    fn a_modifier_press_neither_repeats_nor_hijacks_a_repeat_in_flight() {
        // **The pair of bugs the arming decision carried while it lived in the server loop.**
        // Modifiers arrive as ordinary key transitions, so an unqualified arm made holding
        // Ctrl repeat it, and — one slot — made pressing Shift mid-run replace the run.
        let now = 1_000;

        // Holding a modifier with nothing repeating starts nothing.
        assert_eq!(
            Repeat::after_key(None, KEY_LEFTCTRL, true, 0, Some(7), now),
            None,
            "a modifier armed a repeat"
        );

        // Pressing a modifier while `a` repeats leaves `a`'s run exactly as it was...
        let run = Repeat::armed(KEY_A, 0, 7, now);
        let after_shift_down =
            Repeat::after_key(Some(run), KEY_LEFTSHIFT, true, 0, Some(7), now + 50);
        assert_eq!(after_shift_down, Some(run), "a modifier press hijacked the repeat");

        // ...and releasing it leaves the run alone too, which is the half that was already
        // guarded — it only held because the press half no longer overwrites `keycode`.
        let after_shift_up = Repeat::after_key(
            after_shift_down,
            KEY_LEFTSHIFT,
            false,
            0,
            Some(7),
            now + 90,
        );
        assert_eq!(after_shift_up, Some(run), "a modifier release stopped the repeat");

        // The held key's own release still stops it.
        assert_eq!(
            Repeat::after_key(after_shift_up, KEY_A, false, 0, Some(7), now + 120),
            None,
            "the repeating key's release must disarm"
        );
    }

    #[test]
    fn every_modifier_is_excluded_both_sides() {
        // Named individually rather than trusting one representative: the table has eight
        // entries and a keycode missing from it repeats.
        for code in [
            KEY_LEFTSHIFT,
            KEY_RIGHTSHIFT,
            KEY_LEFTCTRL,
            KEY_RIGHTCTRL,
            KEY_LEFTALT,
            KEY_RIGHTALT,
            KEY_LEFTMETA,
            KEY_RIGHTMETA,
        ] {
            assert_eq!(
                Repeat::after_key(None, code, true, 0, Some(1), 0),
                None,
                "keycode {code} armed a repeat"
            );
        }
        // And an ordinary key still does, or the exclusion is too wide.
        assert!(
            Repeat::after_key(None, KEY_A, true, 0, Some(1), 0).is_some(),
            "an ordinary key stopped repeating"
        );
    }

    #[test]
    fn a_press_with_no_focus_candidate_disarms_rather_than_leaving_a_stale_run() {
        // The window a repeat targets is captured at the press. A press while nothing can
        // take focus must not leave the *previous* key repeating into a window that is no
        // longer the focus candidate.
        let run = Repeat::armed(KEY_A, 0, 7, 0);
        assert_eq!(Repeat::after_key(Some(run), KEY_B, true, 0, None, 100), None);
    }

    #[test]
    fn presenting_leaves_the_pointer_on_screen_over_a_window() {
        // **The test that was missing.** The cursor is drawn over the composed stack rather
        // than composited into it, so every path that recomposes has to redraw it — and three
        // of the four paths in the server binary did not (PR #185 review, finding 1): a click
        // raised a window, recomposed the whole screen, and erased the pointer until the mouse
        // moved next. Nothing could fail: the binary's screen-update path takes a `Server` and
        // a `RawFramebuffer`, so it is unreachable from a host test, and the gate never sees it
        // because the click that would trigger it is followed by a reply that repaints.
        //
        // Pairing the two here is what makes it testable, and `pub(crate)` on `compose_into`
        // is what stops a fifth path from being added without it.
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = shown(&mut s, &CreateWindowRequest::new(64, 64, Role::Normal));
        s.attach(&attach(w, 0, 64, 64)).unwrap();
        // A window colour the cursor's body is not, so "the pointer is there" cannot be
        // satisfied by the window's own pixels.
        let window_colour = Rgb::new(0x10, 0x80, 0x20);
        src.put(w, 0, geom(64, 64), window_colour);
        s.commit(&commit(w, 0)).unwrap();

        let pointer = Point::new(20, 20);
        let mut fb = big_screen();
        let full = fb.geometry().bounds();
        s.present_into(&mut fb, Rgb::BLACK, &src, &[full], pointer, None);

        assert!(
            body_pixels(&fb, pointer) > 0,
            "the pointer is not on screen over the window it is above"
        );
        // And the window is still underneath it, rather than the cursor having replaced a
        // recompose that never happened.
        assert_eq!(fb.get_pixel(0, 0), Some(window_colour), "the stack did not compose");
    }

    /// Cursor body pixels inside the sprite's rectangle at `at`.
    fn body_pixels(fb: &MemFramebuffer, at: Point) -> usize {
        let r = cursor_rect(at);
        let mut n = 0;
        for y in r.origin.y..r.bottom() as i32 {
            for x in r.origin.x..r.right() as i32 {
                if fb.get_pixel(x as u32, y as u32) == Some(CURSOR_BODY) {
                    n += 1;
                }
            }
        }
        n
    }

    #[test]
    fn presenting_draws_the_pointer_into_every_damage_rectangle() {
        // `serve_input` passes the cursor's old and new rectangles in one damage list — with
        // whatever a restack disturbed alongside them — and a `present_into` that drew into only
        // the first would leave the pointer erased at its destination whenever the two
        // rectangles do not overlap.
        let s = WindowStack::new();
        let src = MapSource::default();
        let pointer = Point::new(40, 40);
        let mut fb = big_screen();
        let elsewhere = Rect::new(0, 0, 8, 8);
        s.present_into(
            &mut fb,
            Rgb::BLACK,
            &src,
            &[elsewhere, cursor_rect(pointer)],
            pointer,
            None,
        );
        assert!(
            body_pixels(&fb, pointer) > 0,
            "the pointer was not drawn into the second damage rectangle"
        );
    }

    #[test]
    fn the_guest_configuration_composites_a_client_surface() {
        // The exact shape `ui-testclient` produces in the guest: a 1280x800 screen at
        // pitch 5120, a 64x32 client surface at **pitch 268**, window origin (0,0). Written
        // after the guest showed pure background in the window region with every guard in
        // `compose_into` passing and the client's pixels verified correct.
        //
        // 268 is not 64*4. The padding is the point: a source stride computed from the
        // width instead of the pitch skews every row after the first, and the two numbers
        // agreeing would hide it. Keep this in step with `libdraw::scene::SCREEN_PITCH`.
        let mut fb = MemFramebuffer::new(
            Geometry::with_pitch(1280, 800, 5120, PixelFormat::XRGB8888).unwrap(),
        );
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = shown(&mut s, &CreateWindowRequest::new(64, 32, Role::Normal));
        s.attach(&AttachBufferRequest {
            window: w,
            buffer: 0,
            width: 64,
            height: 32,
            pitch: 268,
            format: SURFACE_FORMAT_XRGB8888,
        })
        .unwrap();
        let g = Geometry::with_pitch(64, 32, 268, PixelFormat::XRGB8888).unwrap();
        // Striped, not uniform. With a single colour every assertion below holds just as
        // well against a compositor that reads the source at `width * 4` — which is the
        // bug the 268 pitch exists to expose, so the test would have been asserting
        // nothing about the thing it was written for.
        src.put_striped(w, 0, g);
        s.commit(&commit(w, 0)).unwrap();

        let bounds = fb.geometry().bounds();
        s.compose_into(&mut fb, Rgb::new(0x0E, 0x14, 0x1B), &src, &[bounds]);

        // Each of these names the source row it must have come from. Read at pitch 256,
        // screen row 5 lands in source row 4 and row 31 in source row 29 — different
        // colours, so the skew is a failure rather than a coincidence.
        assert_eq!(
            fb.get_pixel(6, 5),
            Some(row_colour(5)),
            "the client's surface must reach the screen, row for row"
        );
        assert_eq!(fb.get_pixel(63, 31), Some(row_colour(31)), "including its last pixel");
        assert_eq!(fb.get_pixel(0, 0), Some(row_colour(0)), "and its first");
        assert_eq!(
            fb.get_pixel(64, 0),
            Some(Rgb::new(0x0E, 0x14, 0x1B)),
            "and stop at its edge"
        );
    }

    #[test]
    fn info_reports_the_committed_size_not_the_requested_one() {
        // A client may create a window at one size and commit a buffer of another; what is
        // on screen is the buffer, so that is what `info` must report.
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = s.create(&CreateWindowRequest::new(100, 50, Role::Normal)).unwrap();
        let info = s.info(w).unwrap();
        assert_eq!((info.width, info.height), (100, 50), "requested size before any commit");

        s.attach(&attach(w, 0, 8, 8)).unwrap();
        src.put(w, 0, geom(8, 8), Rgb::BLACK);
        s.commit(&commit(w, 0)).unwrap();
        // Damage ignored: this test is about the reported geometry, not repainting.
        let _ = s.place(w, Point::new(-3, 12)).unwrap();

        let info = s.info(w).unwrap();
        assert_eq!((info.width, info.height), (8, 8), "committed size once there is one");
        assert_eq!((info.x, info.y), (-3, 12));
        assert_eq!(info.id, w);
    }

    /// A geometry change reports the rectangle that is **on screen**, and reports one for a
    /// commit as well as for a place.
    ///
    /// Two defects in one test, because they share a cause — the event was built at the one
    /// op it was written for, from the field that op happened to have. It reported the
    /// *requested* size while `/dev/draw/<id>/info` reports the *committed* one, so a manager
    /// and a namespace read disagreed about the same window at the same instant; and a client
    /// that resized itself by committing a different buffer produced no event at all, leaving
    /// polling as the only way to notice — which is what this event exists to remove
    /// (PR #217 review, findings 1 and 2).
    #[test]
    fn a_geometry_change_reports_committed_bounds_and_fires_for_a_commit_too() {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = s.create(&CreateWindowRequest::new(100, 50, Role::Normal)).unwrap();
        // Creating alone changes no bounds: there is no previous rectangle to differ from.
        assert!(s.take_geometry_changes().is_empty(), "create is not a geometry change");

        // A commit that resizes the window — no manager involved anywhere.
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        src.put(w, 0, geom(8, 8), Rgb::BLACK);
        s.commit(&commit(w, 0)).unwrap();
        assert_eq!(s.take_geometry_changes(), vec![w], "a commit that resizes is reported");

        let _ = s.place(w, Point::new(-3, 12)).unwrap();
        assert_eq!(s.take_geometry_changes(), vec![w], "and so is a place");

        // The rectangle a manager would be handed must equal the one `info` answers with.
        let b = s.window(w).unwrap().bounds();
        let info = s.info(w).unwrap();
        assert_eq!(
            (b.origin.x, b.origin.y, b.size.w, b.size.h),
            (info.x, info.y, info.width, info.height),
            "the event and /dev/draw/<id>/info must describe one window the same way"
        );
        assert_eq!((b.size.w, b.size.h), (8, 8), "committed, not the 100x50 requested");

        // Re-committing the same buffer changes nothing, so it announces nothing.
        s.commit(&commit(w, 0)).unwrap();
        assert!(s.take_geometry_changes().is_empty(), "a commit that does not resize is silent");
    }

    /// One dispatch that both moves and resizes a window reports it once, not twice.
    #[test]
    fn geometry_changes_are_deduplicated() {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = s.create(&CreateWindowRequest::new(100, 50, Role::Normal)).unwrap();
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        src.put(w, 0, geom(8, 8), Rgb::BLACK);
        s.commit(&commit(w, 0)).unwrap();
        let _ = s.place(w, Point::new(5, 5)).unwrap();
        assert_eq!(s.take_geometry_changes(), vec![w], "one id, however many times it moved");
    }

    /// A window that has committed but has **not** been configured stays off the screen.
    ///
    /// This is M6 B4's ordering rule, and it is the whole of it. A client is obliged to wait
    /// for its first `Configure` before committing, so a well-behaved one cannot get here —
    /// which is exactly why the rule needs a test: without one, the gate is unreachable and
    /// nothing would fail if compositing stopped checking it. What it buys is that a client
    /// which jumps the handshake paints nothing rather than painting at the default origin
    /// and jumping when the manager places it.
    #[test]
    fn an_unconfigured_window_is_not_composited_however_much_it_commits() {
        let screen = Geometry::packed(16, 16, PixelFormat::XRGB8888);
        let mut fb = MemFramebuffer::new(screen);
        let mut s = WindowStack::new();
        let mut src = MapSource::default();

        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        src.put(w, 0, geom(8, 8), Rgb::new(0xFF, 0xFF, 0xFF));
        s.commit(&commit(w, 0)).unwrap();

        s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
        assert_eq!(
            fb.get_pixel(0, 0),
            Some(Rgb::new(0, 0, 0)),
            "committed, but never configured: the client jumped the handshake and is not on screen"
        );

        // The configure arrives — from the compositor, a manager, or the deadline; the stack
        // does not care which — and the same pixels appear with no further commit.
        assert!(s.mark_configured(w), "this is the transition");
        s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(0xFF, 0xFF, 0xFF)), "configured: now it composites");

        assert!(!s.mark_configured(w), "marking twice is not a second transition");
    }

    /// A configured, committed white 8×8 window at the origin, ready to composite.
    fn drawable(s: &mut WindowStack, src: &mut MapSource) -> u32 {
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        s.attach(&attach(w, 0, 8, 8)).unwrap();
        src.put(w, 0, geom(8, 8), Rgb::new(0xFF, 0xFF, 0xFF));
        s.commit(&commit(w, 0)).unwrap();
        s.mark_configured(w);
        w
    }

    #[test]
    fn a_window_on_another_desktop_is_not_composited_and_comes_back_when_you_switch_to_it() {
        let screen = Geometry::packed(16, 16, PixelFormat::XRGB8888);
        let mut fb = MemFramebuffer::new(screen);
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        drawable(&mut s, &mut src);
        let black = Rgb::new(0, 0, 0);
        let white = Rgb::new(0xFF, 0xFF, 0xFF);

        s.compose_into(&mut fb, black, &src, &[screen.bounds()]);
        assert_eq!(fb.get_pixel(0, 0), Some(white), "precondition: on the current desktop");

        assert!(s.set_current_desktop(2).unwrap(), "the switch changed something");
        s.compose_into(&mut fb, black, &src, &[screen.bounds()]);
        assert_eq!(fb.get_pixel(0, 0), Some(black), "desktop 2 does not show desktop 1's window");

        // **Nothing was destroyed, and no commit is needed to get it back** — the window kept
        // its buffer, so switching back is a filter changing its mind rather than a client
        // being asked to redraw. That is what makes a desktop switch cheap.
        s.set_current_desktop(1).unwrap();
        s.compose_into(&mut fb, black, &src, &[screen.bounds()]);
        assert_eq!(fb.get_pixel(0, 0), Some(white), "and back again, with no further commit");
    }

    #[test]
    fn a_sticky_window_composites_on_every_desktop() {
        let screen = Geometry::packed(16, 16, PixelFormat::XRGB8888);
        let mut fb = MemFramebuffer::new(screen);
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = drawable(&mut s, &mut src);
        s.set_window_desktop(w, STICKY_DESKTOP).unwrap();

        for d in [1u32, 2, 99] {
            s.set_current_desktop(d).unwrap();
            s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
            assert_eq!(
                fb.get_pixel(0, 0),
                Some(Rgb::new(0xFF, 0xFF, 0xFF)),
                "a sticky window is on desktop {d} too"
            );
        }
    }

    #[test]
    fn a_minimized_window_is_not_composited_though_it_stays_on_its_desktop() {
        let screen = Geometry::packed(16, 16, PixelFormat::XRGB8888);
        let mut fb = MemFramebuffer::new(screen);
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let w = drawable(&mut s, &mut src);

        assert!(s.set_minimized(w, true).unwrap(), "visibility changed");
        s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(0, 0, 0)), "minimized: off screen");

        // **Still on desktop 1**, which is the whole reason this is a separate attribute: a
        // window list is built per desktop, and a minimized window has to appear in the right
        // one to be restorable from it.
        assert_eq!(s.window(w).unwrap().desktop, 1, "minimizing did not move it");
        assert!(s.set_minimized(w, false).unwrap());
        s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
        assert_eq!(fb.get_pixel(0, 0), Some(Rgb::new(0xFF, 0xFF, 0xFF)), "restored");
    }

    #[test]
    fn focus_does_not_land_on_a_window_that_is_not_on_screen() {
        // Focus is derived from the stack — the topmost focusable window — so a window hidden
        // by either attribute must stop being a candidate. Otherwise switching desktops leaves
        // the keyboard pointed at something invisible and every keystroke is lost, which is the
        // same argument that made unconfigured windows non-candidates in M6 B4.
        let mut s = WindowStack::new();
        let lower = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        s.mark_configured(lower);
        let upper = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        s.mark_configured(upper);
        assert_eq!(s.focus_candidate(), Some(upper));

        s.set_minimized(upper, true).unwrap();
        assert_eq!(s.focus_candidate(), Some(lower), "minimized: not a candidate");

        s.set_minimized(upper, false).unwrap();
        s.set_window_desktop(upper, 2).unwrap();
        assert_eq!(s.focus_candidate(), Some(lower), "on another desktop: not a candidate");

        s.set_window_desktop(lower, 2).unwrap();
        assert_eq!(s.focus_candidate(), None, "nothing on this desktop takes focus");
    }

    #[test]
    fn a_window_is_created_onto_the_current_desktop() {
        // This is what makes "a window is never on no desktop" true by construction rather
        // than by care — there is no assigned-later moment for anything to have to render.
        let mut s = WindowStack::new();
        let first = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        assert_eq!(s.window(first).unwrap().desktop, 1);

        s.set_current_desktop(4).unwrap();
        let second = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        assert_eq!(s.window(second).unwrap().desktop, 4, "created onto the current desktop");
        assert_eq!(s.window(first).unwrap().desktop, 1, "and the earlier one did not move");
    }

    #[test]
    fn the_sticky_value_is_refused_as_a_current_desktop() {
        // `0` means "on every desktop". A current desktop of `0` would composite only sticky
        // windows *and*, by create-onto-current, make every window created afterwards sticky —
        // a state nothing could undo without knowing what each window's desktop should have
        // been. `desktop switch N` takes N off a command line, so this is one keystroke away
        // (PR #239 review, finding 7).
        let mut s = WindowStack::new();
        assert_eq!(s.set_current_desktop(STICKY_DESKTOP), Err(StackError::StickyIsNotADesktop));
        assert_eq!(s.current_desktop(), 1, "and the current desktop is unchanged");

        // A *window* may be sticky: the value is reserved, not forbidden.
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        assert!(s.set_window_desktop(w, STICKY_DESKTOP).is_ok());
    }

    #[test]
    fn moving_a_window_between_two_hidden_desktops_reports_no_visible_change() {
        // The caller repaints the screen when this says `true`. A shell tidying windows in the
        // background would otherwise repaint once per window for changes nobody can see.
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        s.mark_configured(w);

        assert!(s.set_window_desktop(w, 2).unwrap(), "leaving the current desktop is visible");
        assert!(!s.set_window_desktop(w, 3).unwrap(), "2 -> 3 changes nothing on screen");
        assert!(!s.set_minimized(w, true).unwrap(), "already hidden");
        assert!(!s.set_window_desktop(w, 1).unwrap(), "still minimized: no change");
        assert!(s.set_minimized(w, false).unwrap(), "back on desktop 1 and un-minimized");
    }

    /// A popup lands at its parent's origin plus the offset its creator asked for (C1).
    #[test]
    fn a_popup_is_created_at_its_offset_from_the_parent() {
        let mut s = WindowStack::new();
        let parent = shown(&mut s, &CreateWindowRequest::new(200, 100, Role::Normal));
        let _ = s.place(parent, Point::new(30, 40)).unwrap();

        let menu = s
            .create(&CreateWindowRequest::at(60, 80, Role::Popup { parent }, 5, 24))
            .unwrap();
        assert_eq!(
            s.window(menu).unwrap().origin,
            Point::new(35, 64),
            "the parent's origin plus the offset, not the offset alone and not the origin"
        );

        // **The offset is resolved once.** Moving the parent afterwards leaves the popup where
        // it was — the documented limit of C1, pinned so it is a decision rather than a
        // surprise. `TODO(popup-follows-parent)`.
        let _ = s.place(parent, Point::new(100, 100)).unwrap();
        assert_eq!(
            s.window(menu).unwrap().origin,
            Point::new(35, 64),
            "resolved at creation: the popup does not track the parent"
        );
    }

    /// Only a `popup` is offset from its parent. A `dialog` lands where a `normal` does.
    ///
    /// The two parented roles share a wire shape and were treated as one placement rule, which
    /// they are not: a `dialog`'s parent carries its desktop membership and its lifetime — not
    /// its position. In placement terms it is an ordinary listed window and a manager places it.
    #[test]
    fn only_a_popup_is_offset_from_its_parent() {
        let mut s = WindowStack::new();
        let parent = shown(&mut s, &CreateWindowRequest::new(50, 50, Role::Normal));
        let _ = s.place(parent, Point::new(60, 70)).unwrap();

        // `at` on a `normal` is a caller mistake; it must not move the window.
        let plain = s.create(&CreateWindowRequest::at(10, 10, Role::Normal, 77, 88)).unwrap();
        assert_eq!(s.window(plain).unwrap().origin, Point::new(0, 0), "a manager places this one");

        // A dialog names a parent and is still placed by a manager, from the origin.
        let dlg = s
            .create(&CreateWindowRequest::at(10, 10, Role::Dialog { parent }, 77, 88))
            .unwrap();
        assert_eq!(
            s.window(dlg).unwrap().origin,
            Point::new(0, 0),
            "a dialog is not offset from its parent, however parented it is"
        );

        // A popup is.
        let menu = s
            .create(&CreateWindowRequest::at(10, 10, Role::Popup { parent }, 5, 6))
            .unwrap();
        assert_eq!(s.window(menu).unwrap().origin, Point::new(65, 76), "the popup is");
    }

    /// **A popup is not clipped to its parent** — the whole reason popups are windows (C2).
    ///
    /// `libui`'s `offset` clips at the parent's edge, which is right one level down; a menu
    /// that could not leave its window would not be a menu. Nothing in the compositor clips a
    /// child to a parent, and this is what says so: the popup is drawn on all four sides of a
    /// parent far too small to contain it.
    #[test]
    fn a_popup_is_drawn_outside_its_parents_bounds() {
        let screen = Geometry::packed(64, 64, PixelFormat::XRGB8888);
        let mut fb = MemFramebuffer::new(screen);
        let mut s = WindowStack::new();
        let mut src = MapSource::default();

        // A tiny parent in the middle, and a popup that covers far more than it.
        let parent = shown(&mut s, &CreateWindowRequest::new(8, 8, Role::Normal));
        let _ = s.place(parent, Point::new(28, 28)).unwrap();
        s.attach(&attach(parent, 0, 8, 8)).unwrap();
        src.put(parent, 0, geom(8, 8), Rgb::new(0x20, 0x20, 0x20));
        s.commit(&commit(parent, 0)).unwrap();

        let menu = s
            .create(&CreateWindowRequest::at(32, 32, Role::Popup { parent }, -12, -12))
            .unwrap();
        s.mark_configured(menu);
        s.attach(&attach(menu, 0, 32, 32)).unwrap();
        src.put(menu, 0, geom(32, 32), Rgb::new(0xFF, 0xFF, 0xFF));
        s.commit(&commit(menu, 0)).unwrap();

        s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);
        let white = Some(Rgb::new(0xFF, 0xFF, 0xFF));
        // The popup spans (16,16)..(48,48); the parent only (28,28)..(36,36). Sample past the
        // parent's edge on all four sides.
        assert_eq!(fb.get_pixel(20, 32), white, "left of the parent");
        assert_eq!(fb.get_pixel(44, 32), white, "right of it");
        assert_eq!(fb.get_pixel(32, 20), white, "above it");
        assert_eq!(fb.get_pixel(32, 44), white, "below it");
    }

    /// A popup crossing a screen edge is clipped to the screen, including at a negative origin.
    ///
    /// The screen is the only clip left (C2). The negative-origin half is the one worth having:
    /// `blit_clipped` derives its source coordinates by subtracting the surface origin, so an
    /// origin left of or above the screen is where that arithmetic would go wrong — and it
    /// would go wrong by reading the wrong pixels rather than by crashing.
    #[test]
    fn a_popup_crossing_a_screen_edge_is_clipped_not_wrapped() {
        let screen = Geometry::packed(32, 32, PixelFormat::XRGB8888);
        let mut fb = MemFramebuffer::new(screen);
        let mut s = WindowStack::new();
        let mut src = MapSource::default();

        let parent = shown(&mut s, &CreateWindowRequest::new(4, 4, Role::Normal));
        // Offset up and left, so the popup's origin is off-screen at (-8, -8).
        let menu = s
            .create(&CreateWindowRequest::at(16, 16, Role::Popup { parent }, -8, -8))
            .unwrap();
        assert_eq!(s.window(menu).unwrap().origin, Point::new(-8, -8));
        s.mark_configured(menu);
        s.attach(&attach(menu, 0, 16, 16)).unwrap();
        // Striped, so a row read from the wrong offset is visible rather than plausible.
        src.put_striped(menu, 0, geom(16, 16));
        s.commit(&commit(menu, 0)).unwrap();

        s.compose_into(&mut fb, Rgb::new(0, 0, 0), &src, &[screen.bounds()]);

        // The visible quarter is the popup's bottom-right: screen (0,0) is popup pixel (8,8).
        assert_eq!(
            fb.get_pixel(0, 0),
            Some(row_colour(8)),
            "screen (0,0) shows the popup's row 8 — the clip moved the source, not just the size"
        );
        assert_eq!(fb.get_pixel(0, 7), Some(row_colour(15)), "and the last row it has");
        // Past the popup's extent (it ends at 8,8) the background shows through.
        assert_eq!(fb.get_pixel(9, 9), Some(Rgb::new(0, 0, 0)), "nothing beyond it");
    }

    #[test]
    fn info_publishes_the_desktop_and_the_minimized_bit() {
        // Deleting either line in `info()` passed every host test until this one existed —
        // only the guest gate caught it, and a host test is cheaper than a boot
        // (PR #240 review, optional 4).
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        assert_eq!(s.info(w).unwrap().desktop, 1, "created onto the current desktop");
        assert_eq!(s.info(w).unwrap().flags & WINDOW_FLAG_MINIMIZED, 0);

        s.set_window_desktop(w, 4).unwrap();
        s.set_minimized(w, true).unwrap();
        let info = s.info(w).unwrap();
        assert_eq!(info.desktop, 4, "the attribute a manager set is what `info` publishes");
        assert_ne!(info.flags & WINDOW_FLAG_MINIMIZED, 0, "and the minimized bit is set");

        s.set_minimized(w, false).unwrap();
        assert_eq!(s.info(w).unwrap().flags & WINDOW_FLAG_MINIMIZED, 0, "and cleared again");
    }

    #[test]
    fn info_carries_the_role_and_its_extra_fields() {
        let mut s = WindowStack::new();
        let p = s
            .create(&CreateWindowRequest::new(100, 24, Role::Panel { dock: Edge::Bottom, reserve: 24 }))
            .unwrap();
        let info = s.info(p).unwrap();
        assert_eq!(info.role, librsproto::surface::ROLE_PANEL);
        assert_eq!(info.reserve, 24);

        let child = s
            .create(&CreateWindowRequest::new(4, 4, Role::Popup { parent: p }))
            .unwrap();
        assert_eq!(s.info(child).unwrap().parent, p);
        assert!(s.info(9999).is_none(), "a window that does not exist has no info");
    }

    #[test]
    fn a_bad_pitch_is_refused_at_attach() {
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest::new(8, 8, Role::Normal)).unwrap();
        let mut req = attach(w, 0, 8, 8);
        req.pitch = 8 * 4 - 1; // cannot hold a row
        assert_eq!(s.attach(&req), Err(StackError::BadGeometry));
    }

    /// `take_removed` reports **every** window that went, parent first, and then drains.
    ///
    /// The set is what the manager's `WindowDestroyed` event is built from, and a menu chain
    /// means one `DestroyWindow` can remove several windows — so a stack that recorded only
    /// the id it was passed would leave a manager holding windows that no longer exist and
    /// will never be mentioned again. It drains because the bin calls it after every dispatch
    /// whether or not a manager is attached; a log that did not clear would re-announce the
    /// same destruction on every later call, and grow without bound when nobody is listening.
    #[test]
    fn take_removed_reports_the_whole_subtree_parent_first_then_drains() {
        let mut s = WindowStack::new();
        let root = s
            .create(&CreateWindowRequest::new(100, 80, Role::Normal))
            .unwrap();
        let menu = s
            .create(&CreateWindowRequest::new(40, 20, Role::Popup { parent: root }))
            .unwrap();
        let submenu = s
            .create(&CreateWindowRequest::new(30, 15, Role::Popup { parent: menu }))
            .unwrap();
        // An unrelated window must not appear in the removed set.
        let other = s
            .create(&CreateWindowRequest::new(10, 10, Role::Normal))
            .unwrap();

        assert!(s.take_removed().is_empty(), "nothing has been destroyed yet");

        s.destroy(root).unwrap();
        let gone = s.take_removed();

        assert_eq!(gone[0], root, "the window asked for is reported first");
        assert!(gone.contains(&menu), "a popup goes with its parent");
        assert!(gone.contains(&submenu), "and so does a popup of that popup");
        assert!(!gone.contains(&other), "an unrelated window is not reported");
        assert_eq!(gone.len(), 3, "exactly the subtree, no duplicates");
        assert!(s.window(other).is_some(), "the unrelated window survives");
        assert!(
            s.take_removed().is_empty(),
            "the log drained: a second read must not re-announce the same subtree"
        );
    }
}
