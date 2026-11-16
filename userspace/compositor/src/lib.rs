//! The compositor's window model — the stack, roles and struts, commits, and compositing.
//!
//! No syscalls in here: what the screen should look like is a pure function of the window
//! stack, so compositing is asserted pixel-exactly on the host. The parts that touch the OS
//! (IPC, mapped buffers, `/dev/draw`) live in the server bin.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

extern crate alloc;

pub mod cursor;
pub mod input;
pub mod manager;
pub mod outbox;
pub mod placement;
pub mod server;

pub use cursor::*;

use alloc::vec::Vec;

use libdraw::compose::{Shadow, SurfaceRef, compose_exposed};
use libdraw::format::{PixelFormat, Rgb};
use libdraw::framebuffer::{Framebuffer, Geometry, MemFramebuffer};
use libdraw::geom::{Point, Rect};
use librsproto::surface::{
    AttachBufferRequest, CommitRequest, CreateWindowRequest, Edge, Placement, Role, STICKY_DESKTOP,
    SURFACE_FORMAT_ARGB8888, SURFACE_FORMAT_XRGB8888, WINDOW_FLAG_MINIMIZED,
};

/// Where a window's pixels are, for a given (window, buffer) pair.
pub trait BufferSource {
    /// The bytes of `buffer` on `window`, or `None` if it is not mapped here.
    fn pixels(&self, window: u32, buffer: u32) -> Option<libdraw::pixels::Pixels<'_>>;
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
/// A newtype so it cannot be dropped in silence: `#[must_use]` on `place` is satisfied by `?`.
/// A zero-sized damage means "nothing changed"; [`union`] treats it as the identity.
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

/// One thing a window says it can take a drop of — `Surface::DeclareAcceptor`.
///
/// A name and a set of kinds, no rectangle: where a drop is allowed is the client's to decide
/// from the point the event carries. The name is what a `Dropped` says it landed on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Acceptor {
    /// What the client calls it.
    pub name: alloc::string::String,
    /// The kinds it takes — a mask of `DROP_KIND_FILE` and friends.
    pub kinds: u32,
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
    /// Only a manager reads it; bounded at [`MAX_TITLE`](librsproto::surface::MAX_TITLE) on the way
    /// in.
    pub title: alloc::string::String,
    /// Whether this window's first `Configure` has been sent to its client.
    ///
    /// A window is not composited until it has been: this makes the client's obligation to wait for
    /// it an enforced ordering. Set by [`mark_configured`](WindowStack::mark_configured).
    pub configured: bool,
    /// Which desktop this window is on; [`STICKY_DESKTOP`] means all of them.
    ///
    /// Changed only by a manager. The compositor holds the attribute and nothing else about
    /// desktops.
    pub desktop: u32,
    /// Hidden without leaving its desktop.
    ///
    /// Not a reserved `desktop` value: a minimized window is still on its desktop and restores
    /// there.
    pub minimized: bool,
    /// What this window says it takes when something is dropped on it.
    ///
    /// Per window, so it is cleared with the window; bounded at
    /// [`MAX_ACCEPTORS`](librsproto::surface::MAX_ACCEPTORS) as it arrives off the wire.
    pub acceptors: Vec<Acceptor>,
    /// The last state its client asked the manager for; only to tell a repeat from a change.
    pub state_requested: Option<u32>,
    /// The shadow this window casts — what its role casts in the stack's current scheme.
    ///
    /// Stored so the scheme reaches every reader without being threaded through each; written only
    /// by [`create`](WindowStack::create) and [`set_scheme`](WindowStack::set_scheme).
    pub shadow: Option<Shadow>,
    /// A placed popup's anchor and placement, to place it again when a commit changes its size.
    pub placed: Option<(Point, Placement)>,
}

impl Window {
    /// Whether this window is on screen when `current` is the current desktop.
    ///
    /// The one predicate for "on screen", shared by compositing, focus and hit-testing so no site
    /// can forget a clause and leave a window invisible but clickable. It says nothing about
    /// whether there is anything to draw: a configured window with no buffer is on screen.
    pub fn visible_on(&self, current: u32) -> bool {
        self.configured && !self.minimized && (self.desktop == STICKY_DESKTOP || self.desktop == current)
    }
}

/// The shadow an ordinary window and a menu cast, in the scheme the manager last named.
use libdraw::theme::{Scheme, window_shadow};

/// The shadow `role` casts in `scheme`, if it casts one.
///
/// Panels do not: a bar is docked to a screen edge and a wallpaper is the ground. The three
/// that float do: an application window, a menu, and a dialog.
pub const fn shadow_for(role: Role, scheme: Scheme) -> Option<Shadow> {
    match role {
        Role::Normal | Role::Popup { .. } | Role::Dialog { .. } => Some(window_shadow(scheme)),
        Role::Panel { .. } => None,
    }
}

/// The radius a window of `role` at `bounds` is cut to, on a screen `screen`: [`corner_for`]'s,
/// unless the window covers the whole screen, when it is square.
/// A screen-sized window's corners are the screen's; rounding the overview notched the dimmed bars.
pub fn corner_of(role: Role, bounds: Rect, screen: Rect) -> u32 {
    let covers = bounds.origin.x <= screen.origin.x
        && bounds.origin.y <= screen.origin.y
        && bounds.right() >= screen.right()
        && bounds.bottom() >= screen.bottom();
    if covers { 0 } else { corner_for(role) }
}

/// The radius `role`'s corners are cut to.
///
/// The floating roles, as for the shadow. The radius is `libdraw`'s compiled
/// [`WINDOW_RADIUS`](libdraw::corner::WINDOW_RADIUS), as the toolkit draws its border on the
/// same curve. A maximised window is rounded: it fills the work area, not the screen, and the
/// compositor does not know it is maximised. A full-screen window is square, see [`corner_of`].
pub const fn corner_for(role: Role) -> u32 {
    match role {
        Role::Normal | Role::Popup { .. } | Role::Dialog { .. } => libdraw::corner::WINDOW_RADIUS,
        Role::Panel { .. } => 0,
    }
}

impl Window {
    /// Everything this window puts on screen, **its shadow included** — what damage is computed
    /// from.
    ///
    /// Not [`bounds`](Self::bounds), which is where the window *is*: what a click hits and what its
    /// client is told its geometry is. One rectangle for both would make the shadow clickable.
    pub fn painted_bounds(&self) -> Rect {
        match self.shadow {
            Some(sh) => sh.around(self.bounds()),
            None => self.bounds(),
        }
    }

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
    /// Re-attaching an id is how a client resizes, but replacing the committed buffer's pixels
    /// would change the screen without a commit: the tearing the buffer protocol exists to prevent.
    DuplicateBuffer,
    /// This connection already holds [`MAX_WINDOWS_PER_CONNECTION`] windows.
    TooManyWindows,
    /// [`STICKY_DESKTOP`] was given where a real desktop was required.
    StickyIsNotADesktop,
    /// This window already holds [`MAX_ACCEPTORS`](librsproto::surface::MAX_ACCEPTORS) acceptors.
    TooManyAcceptors,
    /// A popup asked to be placed by a stack given no screen ([`WindowStack::on_screen`]).
    NoScreen,
    /// That window is being dragged by the user, so a manager may not place it (it would be
    /// undone).
    Dragging,
}

/// How many windows one connection may hold at once.
///
/// Bounded like everything else here; 64 is far above honest use (a window, its menus and a
/// dialog) and far below what would exhaust memory. Sequential churn is unaffected.
pub const MAX_WINDOWS_PER_CONNECTION: usize = 64;

/// The `(lost, gained)` pair a focus change implies, or `None` if focus did not move.
///
/// `focus_candidate` is recomputed after anything that could move focus and most do not;
/// announcing unconditionally would flood a client's bounded queue with no-change events.
pub fn focus_transition(prev: Option<u32>, now: Option<u32>) -> Option<(Option<u32>, Option<u32>)> {
    (prev != now).then_some((prev, now))
}

/// The compositor's window stack: bottom-first order, plus the id allocator.
pub struct WindowStack {
    windows: Vec<Window>,
    next_id: u32,
    /// Every id that has left the stack since the last [`take_removed`](Self::take_removed).
    ///
    /// A log because one destroy can remove several windows; the bin drains it every dispatch.
    removed_log: Vec<u32>,
    /// Every id whose [`bounds`](Window::bounds) changed since the last
    /// [`take_geometry_changes`](Self::take_geometry_changes).
    ///
    /// Recorded where bounds change, whether by a manager `Place` or a client's commit.
    geometry_log: Vec<u32>,
    /// Which desktop is composited. Never [`STICKY_DESKTOP`] — see [`Self::set_current_desktop`].
    current_desktop: u32,
    /// Which scheme the compositor's own drawing follows — today, only how dark a shadow is.
    ///
    /// Told, never read: the compositor never sees a theme file, so the shell says which scheme it
    /// named via `SetScheme`. Light until then, as in a session with no shell.
    scheme: Scheme,
    /// The window the user is interactively dragging, and where it was when the drag began.
    ///
    /// Here because the manager path must refuse a `Place` for it and never sees the router.
    dragging: Option<(u32, Point)>,
    /// The screen a popup asking to be kept on it is placed against; with none, as asked.
    screen: Option<Rect>,
}

impl Default for WindowStack {
    /// Delegates to [`WindowStack::new`]; a derive would start `next_id` at 0, not 1, and accept
    /// `Popup { parent: 0 }`.
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
            // One, not zero: zero is `STICKY_DESKTOP`, which would make every new window sticky.
            current_desktop: 1,
            scheme: Scheme::Light,
            dragging: None,
            screen: None,
        }
    }

    /// The same stack, placing popups that ask against `screen` less its panels' reservations.
    pub fn on_screen(mut self, screen: Rect) -> Self {
        self.screen = Some(screen);
        self
    }

    /// The work area, or the screen when panels reserved all of it: that empty area is off the edge.
    fn placement_area(&self) -> Option<Rect> {
        let screen = self.screen?;
        let area = self.work_area(screen);
        Some(if area.size.w == 0 || area.size.h == 0 { screen } else { area })
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
    pub fn window_mut(&mut self, id: u32) -> Option<&mut Window> {
        self.windows.iter_mut().find(|w| w.id == id)
    }

    /// Create a window and return its id.
    pub fn create(&mut self, req: &CreateWindowRequest) -> Result<u32, StackError> {
        if let Role::Popup { parent } | Role::Dialog { parent } = req.role
            && self.window(parent).is_none()
        {
            return Err(StackError::NoSuchParent);
        }
        // A popup starts where its creator asked, relative to its parent, resolved to an absolute
        // origin once here; one that tracked its parent would be placement policy:
        // `TODO(popup-follows-parent)`.
        //
        // A dialog names a parent for desktop membership and lifetime, not position; it is placed
        // like a `normal` window.
        let (origin, placed) = match req.role {
            // Where its creator asks, or kept inside the work area if it asks that. Nothing makes an
            // application ask, so its popup can still cover the bars — `TODO(app-covers-panels)`.
            Role::Popup { parent } => {
                let base = self.window(parent).expect("checked above").origin;
                let anchor = Point::new(base.x.saturating_add(req.offset_x), base.y.saturating_add(req.offset_y));
                if req.place == Placement::AsAsked {
                    (anchor, None)
                } else {
                    // Refused: a compositor that forgot its screen would misplace menus silently.
                    let area = self.placement_area().ok_or(StackError::NoScreen)?;
                    (placement::place(anchor, (req.width, req.height), area, req.place), Some((anchor, req.place)))
                }
            }
            // A `normal` or `dialog` window starts where it asked and a manager overrides it: the
            // configure is held until it answers. Without a manager (the login greeter) it appears
            // where it asked.
            Role::Normal | Role::Dialog { .. } => (Point::new(req.offset_x, req.offset_y), None),
            // A panel is docked, not positioned: its `strut` says which edge it reserves.
            Role::Panel { .. } => (Point::new(0, 0), None),
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
            // Created unconfigured; whoever sends the first `Configure` marks it, and until then
            // the window is not on screen.
            title: alloc::string::String::new(),
            configured: false,
            // Onto the current desktop, so no window is ever on no desktop.
            desktop: self.current_desktop,
            minimized: false,
            acceptors: Vec::new(),
            state_requested: None,
            shadow: shadow_for(req.role, self.scheme),
            placed,
        });
        Ok(id)
    }

    /// Record that `window`'s first `Configure` has gone out; returns whether this was the
    /// transition, so the client is owed exactly one.
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
    /// Returns whether the window's visibility changed. Any id is accepted, zero included: which
    /// desktops exist is the shell's.
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

    /// Switch which desktop is composited. Returns whether it changed. `STICKY_DESKTOP` is refused:
    /// every window created afterwards would silently be sticky.
    pub fn set_current_desktop(&mut self, desktop: u32) -> Result<bool, StackError> {
        if desktop == STICKY_DESKTOP {
            return Err(StackError::StickyIsNotADesktop);
        }
        let changed = self.current_desktop != desktop;
        self.current_desktop = desktop;
        Ok(changed)
    }

    /// The scheme the compositor's own drawing follows.
    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Follow `scheme` from now on, and re-derive every window's shadow. Returns whether it
    /// changed; the whole screen is dirty when it does.
    pub fn set_scheme(&mut self, scheme: Scheme) -> bool {
        if self.scheme == scheme {
            return false;
        }
        self.scheme = scheme;
        for w in &mut self.windows {
            w.shadow = shadow_for(w.role, scheme);
        }
        true
    }

    /// Move a window's top-left corner, returning the region that must be repainted.
    ///
    /// The damage is the union of where the window was and where it is, returned from the mutation
    /// so it cannot be computed from state read after the change. A window never committed returns
    /// a zero-sized rectangle: it is not on screen, and managers place windows before first commit.
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
    /// `NoSuchWindow` if it is not in the stack. A second drag replaces the first.
    pub fn begin_drag(&mut self, id: u32) -> Result<(), StackError> {
        let origin = self.window(id).ok_or(StackError::NoSuchWindow)?.origin;
        self.dragging = Some((id, origin));
        Ok(())
    }

    /// End the drag, and record the one geometry change it produced.
    ///
    /// One record for the whole gesture, not per motion: the manager's queue does not coalesce and
    /// a long drag would push a `WindowCreated` off the front of it.
    pub fn end_drag(&mut self) {
        // Only if it moved: a click on a title bar is a zero-pixel drag and must not queue a no-op.
        // Only if it moved: a click on a title bar is a drag of zero pixels.
        if let Some((id, was)) = self.dragging.take()
            && self.window(id).is_some_and(|w| w.origin != was)
        {
            self.geometry_log.push(id);
        }
    }

    /// Record a state request; `false` for a repeat of the last one, or for a window that is gone.
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

    /// Record that `window` takes drops of `kinds` under `name`; re-declaring a name replaces it.
    /// `Err(TooManyAcceptors)` when full, a refusal rather than evicting the oldest.
    pub fn declare_acceptor(&mut self, id: u32, name: &str, kinds: u32) -> Result<(), StackError> {
        let w = self.windows.iter_mut().find(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        if let Some(a) = w.acceptors.iter_mut().find(|a| a.name == name) {
            a.kinds = kinds;
            return Ok(());
        }
        if w.acceptors.len() >= librsproto::surface::MAX_ACCEPTORS {
            return Err(StackError::TooManyAcceptors);
        }
        w.acceptors.push(Acceptor { name: alloc::string::String::from(name), kinds });
        Ok(())
    }

    /// The name of the first acceptor on `id` that takes `kind`, or `None`.
    pub fn acceptor_for(&self, id: u32, kind: u32) -> Option<&str> {
        let w = self.window(id)?;
        w.acceptors.iter().find(|a| a.kinds & kind != 0).map(|a| a.name.as_str())
    }

    /// Forget what `id` last asked to be, because a manager changed it by another route.
    pub fn clear_state_request(&mut self, id: u32) {
        if let Some(w) = self.windows.iter_mut().find(|w| w.id == id) {
            w.state_requested = None;
        }
    }

    /// Move a window the user is dragging, without logging a geometry change.
    pub fn drag_to(&mut self, id: u32, origin: Point) -> Result<Damage, StackError> {
        self.move_to(id, origin, false)
    }

    fn move_to(&mut self, id: u32, origin: Point, log: bool) -> Result<Damage, StackError> {
        let w = self.windows.iter_mut().find(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        let uncommitted = w.committed.is_none();
        let was = w.painted_bounds();
        w.origin = origin;
        let now = w.painted_bounds();
        if log && was != now {
            self.geometry_log.push(id);
        }
        if uncommitted {
            // Nothing on screen, so nothing to repaint, but it has still moved: the record above
            // fires.
            return Ok(Damage(Rect::new(origin.x, origin.y, 0, 0)));
        }
        Ok(Damage(union(was, now)))
    }

    /// Attach a buffer to a window.
    ///
    /// Unbounded in count — `TODO(window-buffer-cap)`: nothing limits how many distinct ids a
    /// window may attach.
    pub fn attach(&mut self, req: &AttachBufferRequest) -> Result<(), StackError> {
        // The tag decides whether this surface is opaque. The decoder has refused anything else,
        // and defaulting to opaque would composite a translucent surface against stale pixels.
        let format = match req.format {
            SURFACE_FORMAT_ARGB8888 => PixelFormat::ARGB8888,
            SURFACE_FORMAT_XRGB8888 => PixelFormat::XRGB8888,
            _ => return Err(StackError::BadGeometry),
        };
        let geometry =
            Geometry::with_pitch(req.width, req.height, req.pitch as usize, format).ok_or(StackError::BadGeometry)?;
        let w = self.windows.iter_mut().find(|w| w.id == req.window).ok_or(StackError::NoSuchWindow)?;
        // Re-attaching an id replaces it, which is how a client resizes (the protocol has no
        // detach). Except the committed one, whose pixels the compositor may be reading: replacing
        // it would change the screen without a commit.
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
    /// Returns the buffer that was on screen before, if any — that is the one to release; releasing
    /// the new one would hand the client memory the compositor is about to read.
    pub fn commit(&mut self, req: &CommitRequest) -> Result<Option<u32>, StackError> {
        let area = self.placement_area();
        let w = self.windows.iter_mut().find(|w| w.id == req.window).ok_or(StackError::NoSuchWindow)?;
        if !w.buffers.iter().any(|b| b.id == req.buffer) {
            return Err(StackError::NoSuchBuffer);
        }
        let previous = w.committed;
        let was = w.painted_bounds();
        w.committed = Some(req.buffer);
        // Placed again at its new size, or a menu that grows passes the edge it was kept inside.
        if let (Some((anchor, how)), Some(area)) = (w.placed, area)
            && w.bounds().size != was.size
        {
            let size = w.bounds().size;
            w.origin = placement::place(anchor, (size.w, size.h), area, how);
        }
        // A commit can resize the window: `bounds()` follows the committed buffer's geometry.
        if w.painted_bounds() != was {
            self.geometry_log.push(req.window);
        }
        // Re-committing the same buffer releases nothing, or the client could draw into what is
        // shown.
        Ok(previous.filter(|&p| p != req.buffer))
    }

    /// Take every id removed since the last call, parent before the popups it took with it.
    pub fn take_removed(&mut self) -> Vec<u32> {
        core::mem::take(&mut self.removed_log)
    }

    /// Take every id whose bounds changed since the last call, each reported once.
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
    /// One `DestroyWindow` can remove several windows (a menu chain goes with its parent), so the
    /// removed set is recorded for the manager's `WindowDestroyed`, parent first.
    pub fn destroy(&mut self, id: u32) -> Result<(), StackError> {
        let i = self.windows.iter().position(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        self.windows.remove(i);
        self.removed_log.push(id);
        // Descendants cannot outlive an ancestor, transitively: a single pass would leave a submenu
        // alive with a dead parent, still compositing and focusable.
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
        // A drag cannot outlive its window, or `Place` would refuse a dead id.
        if self.dragging.is_some_and(|(d, _)| self.window(d).is_none()) {
            self.dragging = None;
        }
        Ok(())
    }

    /// Raise a window to the top of the stack.
    ///
    /// The damage is the window's own rectangle, and that is exact: moving one window changes the
    /// relative order of no other pair, so only the pixels it covers can differ. Repainting the
    /// whole screen is ~100 ms under emulation with no input read.
    ///
    /// A window already topmost reports **empty** damage; click-to-focus raises on every press.
    ///
    /// A window's transients do not ride up with it and nothing keeps them above it:
    /// `TODO(dialog-stacking)`, see `deferred-decisions.md`.
    pub fn raise(&mut self, id: u32) -> Result<Damage, StackError> {
        let i = self.windows.iter().position(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        if i + 1 == self.windows.len() {
            return Ok(Damage(Rect::new(0, 0, 0, 0)));
        }
        let w = self.windows.remove(i);
        let rect = w.painted_bounds();
        self.windows.push(w);
        Ok(Damage(rect))
    }

    /// Send a window to the bottom of the stack.
    pub fn lower(&mut self, id: u32) -> Result<Damage, StackError> {
        let i = self.windows.iter().position(|w| w.id == id).ok_or(StackError::NoSuchWindow)?;
        if i == 0 {
            return Ok(Damage(Rect::new(0, 0, 0, 0)));
        }
        let w = self.windows.remove(i);
        let rect = w.painted_bounds();
        self.windows.insert(0, w);
        Ok(Damage(rect))
    }

    /// Put `id` directly above `other`, without reordering the windows between them. `id == other`
    /// is a no-op.
    pub fn raise_above(&mut self, id: u32, other: u32) -> Result<Damage, StackError> {
        if self.window(id).is_none() || self.window(other).is_none() {
            return Err(StackError::NoSuchWindow);
        }
        if id == other {
            return Ok(Damage(Rect::new(0, 0, 0, 0)));
        }
        let i = self.windows.iter().position(|w| w.id == id).expect("checked above");
        let w = self.windows.remove(i);
        let rect = w.painted_bounds();
        // Recomputed after the removal, which shifts everything above `id` down by one.
        let j = self.windows.iter().position(|w| w.id == other).expect("checked above");
        self.windows.insert(j + 1, w);
        // Empty when it landed where it was: the order is unchanged.
        let moved = j + 1 != i;
        Ok(Damage(if moved { rect } else { Rect::new(0, 0, 0, 0) }))
    }

    /// The area left for `normal` windows after every panel's reservation.
    ///
    /// Reservations are declared, not derived from a panel's geometry (`display-substrate.md` §4a),
    /// and clamped so over-reservation leaves an empty area. Every panel counts, including ones not
    /// on screen: a minimized or other-desktop bar is coming back, and a work area that grew
    /// meanwhile would relayout every maximised window twice per desktop switch.
    pub fn work_area(&self, screen: Rect) -> Rect {
        let (mut top, mut bottom, mut left, mut right) = (0u32, 0u32, 0u32, 0u32);
        for w in &self.windows {
            if let Some((edge, reserve)) = w.role.strut() {
                // Saturating: the sum over many panels is unbounded and a wrap would return the
                // full screen, defeating the clamp below. No catch-all arm, so a new `Edge` is a
                // compile error.
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

    /// The `info` a window reports at `/dev/draw/<id>/info`: the committed buffer's size if any.
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
        info.desktop = w.desktop;
        if w.minimized {
            info.flags |= WINDOW_FLAG_MINIMIZED;
        }
        Some(info)
    }

    /// The topmost window that may take keyboard focus, if any.
    ///
    /// Panels are skipped: clicking the clock must not steal input from the terminal. There is no
    /// input-exclusive window — `TODO(dialog-modality)`: a `dialog` stops being the candidate the
    /// moment anything else is raised. See `deferred-decisions.md`.
    pub fn focus_candidate(&self) -> Option<u32> {
        // Unconfigured windows are not candidates, as they are not composited: the keyboard must
        // not go to a window nobody can see.
        self.windows.iter().rev().find(|w| w.visible_on(self.current_desktop) && w.role.takes_focus()).map(|w| w.id)
    }

    /// Composite the stack into `fb`, repainting `damage` — **without the cursor**.
    ///
    /// `pub(crate)` so the binary must go through [`present_into`](Self::present_into): the cursor
    /// is drawn over the composed stack, so every recompose has to redraw it.
    pub(crate) fn compose_into<F, S>(&self, fb: &mut F, background: Rgb, source: &S, damage: &[Rect])
    where
        F: Framebuffer + ?Sized,
        S: BufferSource + ?Sized,
    {
        let mut surfaces: Vec<SurfaceRef<'_>> = Vec::new();
        let screen = fb.geometry().bounds();
        for w in &self.windows {
            // Not composited unless on screen (configured, not minimized, on the current desktop or
            // sticky): a client committing before its first `Configure` would paint at the default
            // origin and jump.
            if !w.visible_on(self.current_desktop) {
                continue;
            }
            let Some(buffer_id) = w.committed else { continue };
            let Some(b) = w.buffers.iter().find(|b| b.id == buffer_id) else { continue };
            let Some(px) = source.pixels(w.id, buffer_id).filter(|p| p.len() >= b.geometry.byte_len()) else {
                continue;
            };
            let bounds = Rect::new(w.origin.x, w.origin.y, b.geometry.width, b.geometry.height);
            let surface = SurfaceRef { geometry: b.geometry, origin: w.origin, pixels: px, shadow: None, corner: 0 }
                .with_corner(corner_of(w.role, bounds, screen));
            surfaces.push(match w.shadow {
                Some(sh) => surface.with_shadow(sh),
                None => surface,
            });
        }
        // `compose_exposed`, not `compose`: `attach` can add a translucent surface, whose blend
        // reads what is beneath it, so `covers` carries both conditions.
        compose_exposed(fb, background, &surfaces, damage);
    }

    /// Put `damage` on the screen: composite the stack, then draw the pointer over it.
    ///
    /// The one way a screen region is updated, so "the cursor survives a recompose" is
    /// host-testable. The cursor goes on last: it is not a window.
    pub(crate) fn present_into<F, S>(
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
        // Outline, then cursor, both over everything else: the cursor is what the user steers by.
        for r in damage {
            if let Some(o) = outline {
                draw_outline(fb, o, *r);
            }
            draw_cursor(fb, pointer, *r);
        }
    }
}

/// The display, and the buffer every frame is built in before it reaches it.
///
/// Composing straight into the aperture lets the scanout read a half-composed region: a flash of
/// background under a dragged window. So frames are built in a shadow and copied; the display is
/// private so no direct write can leave the two disagreeing.
pub struct Screen<F: Framebuffer> {
    /// Where frames are composed. `None` when the buffer would not fit — see [`Screen::new`].
    shadow: Option<MemFramebuffer>,
    display: F,
}

impl<F: Framebuffer> Screen<F> {
    /// Wrap `display`, allocating a shadow buffer the same shape.
    ///
    /// The caller must not run without it: the framebuffer is write-combining, so composing into it
    /// directly reads it back uncached (54 MiB/s measured). [`Screen::is_buffered`] reports which
    /// happened and `main.rs` refuses to serve when it is false.
    pub fn new(display: F) -> Self {
        let shadow = MemFramebuffer::try_new(display.geometry());
        Self { shadow, display }
    }

    /// Wrap `display` with a shadow buffer the caller already has.
    ///
    /// For a caller that must allocate before acquiring the display, which stops the kernel
    /// console.
    pub fn with_shadow(display: F, shadow: MemFramebuffer) -> Self {
        Self { shadow: Some(shadow), display }
    }

    /// A screen with no shadow buffer, the path [`Screen::new`] falls back to; test-only.
    #[cfg(test)]
    pub(crate) fn unbuffered(display: F) -> Self {
        Self { shadow: None, display }
    }

    /// The composed pixels, for a test that needs to look at the display it wrapped.
    #[cfg(test)]
    pub(crate) fn display(&self) -> &F {
        &self.display
    }

    /// Whether frames are being composed off-screen — false if the shadow buffer would not fit.
    pub fn is_buffered(&self) -> bool {
        self.shadow.is_some()
    }

    /// The display's shape.
    pub fn geometry(&self) -> Geometry {
        self.display.geometry()
    }

    /// Compose `stack` into the shadow, draw the pointer over it, and copy `damage` to the display.
    pub fn present<S>(
        &mut self,
        stack: &WindowStack,
        background: Rgb,
        source: &S,
        damage: &[Rect],
        pointer: Point,
        outline: Option<Rect>,
    ) where
        S: BufferSource + ?Sized,
    {
        let Some(shadow) = self.shadow.as_mut() else {
            stack.present_into(&mut self.display, background, source, damage, pointer, outline);
            return;
        };
        stack.present_into(shadow, background, source, damage, pointer, outline);
        let _ = copy_damage(&mut self.display, shadow, damage);
    }
}

/// Copy each damage rectangle from `shadow` to `display`, row by row. Returns the bytes copied.
///
/// The count is returned for a test: a copy that ignored damage would draw the same picture, so
/// the bound can only be checked by measuring the work.
#[must_use]
fn copy_damage<F: Framebuffer + ?Sized>(display: &mut F, shadow: &MemFramebuffer, damage: &[Rect]) -> usize {
    let g = display.geometry();
    let bounds = g.bounds();
    let mut copied = 0;
    for area in damage {
        let Some(area) = area.intersect(&bounds) else { continue };
        let width = area.size.w as usize * g.format.bytes_per_pixel();
        for row in 0..area.size.h {
            let y = area.origin.y as u32 + row;
            let Some(off) = g.offset_of(area.origin.x as u32, y) else { continue };
            let (src, dst) = (shadow.bytes(), display.bytes_mut());
            // Unreachable, and deliberately not what keeps this correct: `area` is clipped and both
            // buffers share a geometry. A skip is a row of stale pixels, so it must never become
            // the mechanism that bounds the copy (one invariant, two guards, and the quiet one
            // won).
            if off + width > src.len() || off + width > dst.len() {
                continue;
            }
            dst[off..off + width].copy_from_slice(&src[off..off + width]);
            copied += width;
        }
    }
    copied
}

#[cfg(test)]
mod tests;
