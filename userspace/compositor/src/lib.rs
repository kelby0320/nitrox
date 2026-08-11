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
pub mod outbox;
pub mod server;

use alloc::vec::Vec;

use libdraw::compose::{SurfaceRef, compose};
use libdraw::format::{PixelFormat, Rgb};
use libdraw::framebuffer::{Framebuffer, Geometry};
use libdraw::geom::{Point, Rect};
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, CreateWindowRequest, Edge, Role,
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
    /// A buffer id already attached to this window.
    DuplicateBuffer,
}

/// The pointer sprite: a plain arrow, `CURSOR_W × CURSOR_H`.
///
/// **One fixed shape, not a protocol.** Per-client cursors — an I-beam over a terminal grid,
/// a resize arrow on an edge — are a Surface addition and are deliberately not in this
/// milestone ([`widget-toolkit.md`](../design/widget-toolkit.md) §9.3); a single arrow is what
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
/// ([`widget-toolkit.md`](../design/widget-toolkit.md) §9.2).
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
        Self { windows: Vec::new(), next_id: 1 }
    }

    /// Windows, bottom-first.
    pub fn windows(&self) -> &[Window] {
        &self.windows
    }

    /// The window with `id`, if it exists.
    pub fn window(&self, id: u32) -> Option<&Window> {
        self.windows.iter().find(|w| w.id == id)
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
        let id = self.next_id;
        self.next_id += 1;
        self.windows.push(Window {
            id,
            role: req.role,
            origin: Point::new(0, 0),
            size: (req.width, req.height),
            buffers: Vec::new(),
            committed: None,
        });
        Ok(id)
    }

    /// Move a window's top-left corner.
    pub fn set_origin(&mut self, id: u32, origin: Point) -> Result<(), StackError> {
        let w = self.windows.iter_mut().find(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        w.origin = origin;
        Ok(())
    }

    /// Attach a buffer to a window.
    pub fn attach(&mut self, req: &AttachBufferRequest) -> Result<(), StackError> {
        let geometry =
            Geometry::with_pitch(req.width, req.height, req.pitch as usize, PixelFormat::XRGB8888)
                .ok_or(StackError::BadGeometry)?;
        let w = self
            .windows
            .iter_mut()
            .find(|w| w.id == req.window)
            .ok_or(StackError::NoSuchWindow)?;
        if w.buffers.iter().any(|b| b.id == req.buffer) {
            return Err(StackError::DuplicateBuffer);
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
        w.committed = Some(req.buffer);
        // Re-committing the same buffer releases nothing: the client already knows it owns
        // no other buffer, and reporting a release here would let it draw into the buffer
        // now on screen.
        Ok(previous.filter(|&p| p != req.buffer))
    }

    /// Destroy a window and everything attached to it.
    pub fn destroy(&mut self, id: u32) -> Result<(), StackError> {
        let i = self.windows.iter().position(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        self.windows.remove(i);
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
            self.windows.retain(|w| match w.role {
                Role::Popup { parent } | Role::Dialog { parent } => live.contains(&parent),
                _ => true,
            });
            if self.windows.len() == before {
                break;
            }
        }
        Ok(())
    }

    /// Raise a window to the top of the stack.
    pub fn raise(&mut self, id: u32) -> Result<(), StackError> {
        let i = self.windows.iter().position(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        let w = self.windows.remove(i);
        self.windows.push(w);
        Ok(())
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
        Some(librsproto::surface::WindowInfo::new(
            w.id,
            w.role,
            bounds.origin.x,
            bounds.origin.y,
            bounds.size.w,
            bounds.size.h,
        ))
    }

    /// The topmost window that may take keyboard focus, if any.
    ///
    /// Panels are skipped: clicking the clock must not steal input from the terminal.
    pub fn focus_candidate(&self) -> Option<u32> {
        self.windows.iter().rev().find(|w| w.role.takes_focus()).map(|w| w.id)
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
    ) where
        F: Framebuffer + ?Sized,
        S: BufferSource + ?Sized,
    {
        self.compose_into(fb, background, source, damage);
        for r in damage {
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

    #[test]
    fn a_created_window_gets_a_fresh_id_and_keeps_its_role() {
        let mut s = WindowStack::new();
        let a = s
            .create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal })
            .unwrap();
        let b = s
            .create(&CreateWindowRequest {
                width: 32,
                height: 4,
                role: Role::Panel { dock: Edge::Top, reserve: 4 },
            })
            .unwrap();
        assert_ne!(a, b, "ids must be unique");
        assert_eq!(s.window(a).unwrap().role, Role::Normal);
        assert_eq!(s.window(b).unwrap().role, Role::Panel { dock: Edge::Top, reserve: 4 });
    }

    #[test]
    fn a_popup_must_name_a_parent_that_exists() {
        let mut s = WindowStack::new();
        assert_eq!(
            s.create(&CreateWindowRequest { width: 4, height: 4, role: Role::Popup { parent: 99 } }),
            Err(StackError::NoSuchParent)
        );
        let p = s
            .create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal })
            .unwrap();
        assert!(
            s.create(&CreateWindowRequest {
                width: 4,
                height: 4,
                role: Role::Popup { parent: p }
            })
            .is_ok()
        );
    }

    #[test]
    fn destroying_a_parent_takes_its_popups_with_it() {
        let mut s = WindowStack::new();
        let p = s
            .create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal })
            .unwrap();
        let menu = s
            .create(&CreateWindowRequest { width: 4, height: 4, role: Role::Popup { parent: p } })
            .unwrap();
        let other = s
            .create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal })
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
            .create(&CreateWindowRequest { width: 4, height: 4, role: Role::Normal })
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
            .create(&CreateWindowRequest { width: 4, height: 4, role: Role::Normal })
            .unwrap();
        assert_eq!(s.commit(&commit(w, 7)), Err(StackError::NoSuchBuffer));
        assert_eq!(s.commit(&commit(99, 0)), Err(StackError::NoSuchWindow));
    }

    #[test]
    fn attaching_the_same_buffer_id_twice_is_refused() {
        let mut s = WindowStack::new();
        let w = s
            .create(&CreateWindowRequest { width: 4, height: 4, role: Role::Normal })
            .unwrap();
        s.attach(&attach(w, 0, 4, 4)).unwrap();
        assert_eq!(s.attach(&attach(w, 0, 4, 4)), Err(StackError::DuplicateBuffer));
    }

    #[test]
    fn panels_reserve_space_and_normal_windows_do_not() {
        let mut s = WindowStack::new();
        let screen = Rect::new(0, 0, 100, 50);
        assert_eq!(s.work_area(screen), screen, "no panels, no reservation");

        s.create(&CreateWindowRequest {
            width: 100,
            height: 8,
            role: Role::Panel { dock: Edge::Top, reserve: 8 },
        })
        .unwrap();
        s.create(&CreateWindowRequest {
            width: 100,
            height: 6,
            role: Role::Panel { dock: Edge::Bottom, reserve: 6 },
        })
        .unwrap();
        s.create(&CreateWindowRequest { width: 40, height: 40, role: Role::Normal }).unwrap();

        assert_eq!(s.work_area(screen), Rect::new(0, 8, 100, 36));
    }

    #[test]
    fn every_edge_reserves_on_its_own_axis() {
        let mut s = WindowStack::new();
        for (edge, reserve) in
            [(Edge::Left, 3u32), (Edge::Right, 5), (Edge::Top, 2), (Edge::Bottom, 4)]
        {
            s.create(&CreateWindowRequest {
                width: 1,
                height: 1,
                role: Role::Panel { dock: edge, reserve },
            })
            .unwrap();
        }
        assert_eq!(s.work_area(Rect::new(0, 0, 100, 50)), Rect::new(3, 2, 92, 44));
    }

    #[test]
    fn over_reservation_empties_the_work_area_rather_than_inverting_it() {
        let mut s = WindowStack::new();
        s.create(&CreateWindowRequest {
            width: 1,
            height: 1,
            role: Role::Panel { dock: Edge::Top, reserve: 90 },
        })
        .unwrap();
        let wa = s.work_area(Rect::new(0, 0, 100, 50));
        assert_eq!(wa.size.h, 0, "a negative height would be a clipping catastrophe");
        assert!(wa.is_empty());
    }

    #[test]
    fn focus_skips_panels_and_follows_the_top_of_the_stack() {
        let mut s = WindowStack::new();
        let a = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        let b = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        s.create(&CreateWindowRequest {
            width: 32,
            height: 4,
            role: Role::Panel { dock: Edge::Top, reserve: 4 },
        })
        .unwrap();
        // The panel is topmost, but must not take focus.
        assert_eq!(s.focus_candidate(), Some(b));
        s.raise(a).unwrap();
        assert_eq!(s.focus_candidate(), Some(a));
    }

    #[test]
    fn a_stack_with_only_panels_has_nothing_to_focus() {
        let mut s = WindowStack::new();
        s.create(&CreateWindowRequest {
            width: 32,
            height: 4,
            role: Role::Panel { dock: Edge::Top, reserve: 4 },
        })
        .unwrap();
        assert_eq!(s.focus_candidate(), None);
    }

    #[test]
    fn compositing_draws_committed_windows_bottom_first() {
        let mut s = WindowStack::new();
        let mut src = MapSource::default();
        let red = Rgb::new(0xC0, 0x10, 0x10);
        let blue = Rgb::new(0x10, 0x10, 0xC0);

        let a = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        s.attach(&attach(a, 0, 8, 8)).unwrap();
        src.put(a, 0, geom(8, 8), red);
        s.commit(&commit(a, 0)).unwrap();

        let b = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        s.attach(&attach(b, 0, 8, 8)).unwrap();
        src.put(b, 0, geom(8, 8), blue);
        s.commit(&commit(b, 0)).unwrap();
        s.set_origin(b, Point::new(4, 4)).unwrap();

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
        let w = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
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
        let w = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        let menu =
            s.create(&CreateWindowRequest { width: 4, height: 4, role: Role::Popup { parent: w } })
                .unwrap();
        let sub = s
            .create(&CreateWindowRequest { width: 2, height: 2, role: Role::Popup { parent: menu } })
            .unwrap();
        let dlg = s
            .create(&CreateWindowRequest { width: 2, height: 2, role: Role::Dialog { parent: sub } })
            .unwrap();
        let other =
            s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();

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
            s.create(&CreateWindowRequest {
                width: 1,
                height: 1,
                // Exactly 2^31 each: the sum is 2^32, which wraps to **zero** — the case
                // that silently returns the full screen. `u32::MAX - 1` twice wraps to a
                // still-huge number the clamp handles anyway, so it proves nothing; that
                // was this test's second wrong version.
                role: Role::Panel { dock: Edge::Top, reserve: 0x8000_0000 },
            })
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
        s.create(&CreateWindowRequest {
            width: 100,
            height: 32,
            role: Role::Panel { dock: Edge::Top, reserve: 32 },
        })
        .unwrap();
        s.create(&CreateWindowRequest {
            width: 100,
            height: 28,
            role: Role::Panel { dock: Edge::Bottom, reserve: 28 },
        })
        .unwrap();
        assert_eq!(s.work_area(Rect::new(0, 0, 200, 100)), Rect::new(0, 32, 200, 40));
    }

    #[test]
    fn default_and_new_share_one_id_space() {
        let mut d = WindowStack::default();
        let mut n = WindowStack::new();
        let req = CreateWindowRequest { width: 1, height: 1, role: Role::Normal };
        assert_eq!(d.create(&req).unwrap(), n.create(&req).unwrap());
    }

    #[test]
    fn a_buffer_the_source_cannot_resolve_is_skipped_not_drawn() {
        // The server maps buffers; a client can commit one whose mapping has gone. Drawing
        // from an unresolvable buffer would read whatever memory happened to be there.
        let mut s = WindowStack::new();
        let src = MapSource::default(); // deliberately empty
        let w = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
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
        let w = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
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
            let w = s
                .create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal })
                .unwrap();
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
        let w = s
            .create(&CreateWindowRequest { width: 64, height: 64, role: Role::Normal })
            .unwrap();
        s.attach(&attach(w, 0, 64, 64)).unwrap();
        // A window colour the cursor's body is not, so "the pointer is there" cannot be
        // satisfied by the window's own pixels.
        let window_colour = Rgb::new(0x10, 0x80, 0x20);
        src.put(w, 0, geom(64, 64), window_colour);
        s.commit(&commit(w, 0)).unwrap();

        let pointer = Point::new(20, 20);
        let mut fb = big_screen();
        let full = fb.geometry().bounds();
        s.present_into(&mut fb, Rgb::BLACK, &src, &[full], pointer);

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
        // `repaint_cursor_move` passes the old and new cursor rectangles; a `present_into`
        // that drew into only the first would leave the pointer erased at its destination
        // whenever the two rectangles do not overlap.
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
        let w = s.create(&CreateWindowRequest { width: 64, height: 32, role: Role::Normal }).unwrap();
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
        let w = s.create(&CreateWindowRequest { width: 100, height: 50, role: Role::Normal }).unwrap();
        let info = s.info(w).unwrap();
        assert_eq!((info.width, info.height), (100, 50), "requested size before any commit");

        s.attach(&attach(w, 0, 8, 8)).unwrap();
        src.put(w, 0, geom(8, 8), Rgb::BLACK);
        s.commit(&commit(w, 0)).unwrap();
        s.set_origin(w, Point::new(-3, 12)).unwrap();

        let info = s.info(w).unwrap();
        assert_eq!((info.width, info.height), (8, 8), "committed size once there is one");
        assert_eq!((info.x, info.y), (-3, 12));
        assert_eq!(info.id, w);
    }

    #[test]
    fn info_carries_the_role_and_its_extra_fields() {
        let mut s = WindowStack::new();
        let p = s
            .create(&CreateWindowRequest {
                width: 100,
                height: 24,
                role: Role::Panel { dock: Edge::Bottom, reserve: 24 },
            })
            .unwrap();
        let info = s.info(p).unwrap();
        assert_eq!(info.role, librsproto::surface::ROLE_PANEL);
        assert_eq!(info.reserve, 24);

        let child = s
            .create(&CreateWindowRequest { width: 4, height: 4, role: Role::Popup { parent: p } })
            .unwrap();
        assert_eq!(s.info(child).unwrap().parent, p);
        assert!(s.info(9999).is_none(), "a window that does not exist has no info");
    }

    #[test]
    fn a_bad_pitch_is_refused_at_attach() {
        let mut s = WindowStack::new();
        let w = s.create(&CreateWindowRequest { width: 8, height: 8, role: Role::Normal }).unwrap();
        let mut req = attach(w, 0, 8, 8);
        req.pitch = 8 * 4 - 1; // cannot hold a row
        assert_eq!(s.attach(&req), Err(StackError::BadGeometry));
    }
}
