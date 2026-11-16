//! Where an input event goes: focus for keys, hit-testing for the pointer.
//!
//! - **Keys follow focus**, the topmost window whose role takes it
//!   ([`WindowStack::focus_candidate`]). A panel never takes a keystroke.
//! - **The pointer follows the cursor**, hit-testing every role.
//! - **A press grabs.** Every pointer event up to the release goes to the window the press landed
//!   on, so a drag ending outside still delivers its release. That is why [`PointerEvent`]'s
//!   coordinates are **signed**: window-local x can be negative.

use alloc::vec::Vec;

use libdraw::geom::{Point, Rect};
use libinput::Logical;
use librsproto::surface::{
    KeyEvent, MAX_HOTKEYS, MAX_SNAP_ZONES, MgrHotkey, MgrSnapZone, POINTER_BUTTON, POINTER_ENTER, POINTER_LEAVE,
    POINTER_MOTION, POINTER_PRESSED, POINTER_WHEEL, PointerEvent, RESIZE_BOTTOM, RESIZE_LEFT, RESIZE_RIGHT, RESIZE_TOP,
    StartResize,
};

use crate::outbox::Outbound;
use crate::{Damage, StackError, WindowStack};

/// What routing one event did, beyond what it put in `out`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Routed {
    /// The region a restack disturbed, or `None`; a rectangle so the caller recomposes only that.
    pub restacked: Option<Rect>,
    /// The region an interactive move disturbed, or `None`; unlike a restack it changes no focus.
    pub moved: Option<Rect>,
    /// The event was taken by a registered chord and reached no window; its key must not repeat.
    ///
    /// Repeat is armed from the physical transition, so without this holding `Super+1` on desktop
    /// 1 filled the focused terminal with `1`s.
    pub consumed: bool,
    /// The outline moved: where it was, and where it is now. See [`Outline`].
    pub outline: Option<Outline>,
    /// A gesture ended; the manager is told, as a `DragEnded`, and answers it.
    pub ended: Option<Ended>,
    /// A popup a press landed outside of; the compositor reports, its owner decides to close.
    pub dismissed: Option<u32>,
}

/// A finished gesture, for the manager: what it asks for, and the window's rectangle when it began.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Ended {
    /// The window moved or resized.
    pub window: u32,
    /// A resize's final rectangle, a snap zone's target, or where a plain move left the window.
    pub asked: Rect,
    /// The window's rectangle when the gesture began.
    pub from: Rect,
    /// A plain move: the window is already at `asked`.
    pub moved: bool,
}

/// Where the resize outline was and where it is now, both to repaint: it is drawn over the stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Outline {
    /// The rectangle the outline occupied before this event.
    pub was: Option<Rect>,
    /// The rectangle it occupies now.
    pub now: Option<Rect>,
}

/// An interactive move run on a client's behalf, from an absolute origin so it cannot drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Drag {
    window: u32,
    from: Point,
    origin: Point,
    /// The pointer has gone [`DRAG_THRESHOLD`] from the press; until then nothing moves.
    going: bool,
}

/// How far the pointer travels from a press before a move begins, in pixels: a click with a
/// tremor in it stays a click, rather than moving a maximised window a pixel and so restoring it.
pub const DRAG_THRESHOLD: i32 = 4;

/// A drag-and-drop gesture run on a client's behalf; the payload is a path, never a handle.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Carrying {
    window: u32,
    kind: u32,
    path: alloc::string::String,
    name: alloc::string::String,
}

/// An interactive resize run on a client's behalf. The window does not change while it runs: a
/// `Configure` per motion would cost the client a repaint per motion.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Resize {
    window: u32,
    from: Point,
    rect: Rect,
    edges: u32,
}

/// The smallest rectangle a resize will offer: a floor against the outline inverting, as the
/// protocol cannot state a client's minimum.
pub const MIN_RESIZE: u32 = 64;

/// Why a chord could not be registered.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotkeyError {
    /// `id` was zero, which is reserved so a zeroed body registers nothing.
    ZeroId,
    /// Another chord is already registered under that `id`.
    DuplicateId,
    /// That `mods`+`code` combination is already registered, under a different id.
    DuplicateChord,
    /// [`MAX_HOTKEYS`] chords are already registered.
    TableFull,
}

/// Whether a window the router still names is usable, hidden (still told it loses the pointer), or
/// gone.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowState {
    OnScreen,
    OffScreen,
    Gone,
}
use WindowState::{Gone, OffScreen, OnScreen};

/// Cursor position, crossing state, and the implicit grab.
pub struct InputRouter {
    pointer: Point,
    screen: Rect,
    inside: Option<u32>,
    grab: Option<u32>,
    /// Where the pointer was when [`grab`](Self::grab) was taken: `StartMove` arrives a round trip
    /// later.
    grab_at: Point,
    drag: Option<Drag>,
    /// The interactive resize in progress, if any. Never both this and `drag`.
    resize: Option<Resize>,
    /// The drag-and-drop gesture in progress, if any. Cleared wherever the grab ends.
    carrying: Option<Carrying>,
    /// The window a drop would land on right now, kept so the drop lands where it was shown.
    over: Option<u32>,
    /// The rectangle the outline is drawn at; every path that stops drawing hands the old value
    /// back.
    outline: Option<Rect>,
    /// Which button opened [`grab`](Self::grab), named by a synthetic release if the grab is lost.
    grab_button: u16,
    /// A grab ended because its window left the screen with buttons still down: pointer input goes
    /// nowhere until they come up.
    grab_broken: bool,
    /// Buttons held, stamped onto every record so a client need not re-accumulate them for a drag.
    buttons: u16,
    /// Modifiers held, mirrored the same way.
    modifiers: u16,
    /// When the event being routed happened, mirrored so a batch's records agree. Nanoseconds;
    /// divided once, at [`emit`](Self::emit).
    time_ns: u64,
    /// Keycodes whose press this router **delivered**, awaiting release: a repeat must not begin a
    /// chord.
    held: Vec<u16>,
    /// Chords a manager asked to receive instead of the focused window. Bounded at [`MAX_HOTKEYS`].
    hotkeys: Vec<MgrHotkey>,
    /// Regions a manager asked to be told about, bounded at [`MAX_SNAP_ZONES`].
    zones: Vec<MgrSnapZone>,
    /// The zone the pointer is in during a move, kept so outline and result use one test.
    in_zone: Option<u32>,
    /// Chords matched since the last [`take_hotkeys`](Self::take_hotkeys); they go to the manager.
    fired: Vec<MgrHotkey>,
    /// Keycodes whose press was consumed as a hotkey, awaiting release; by keycode, as `Super` may
    /// come up first.
    consumed: Vec<u16>,
}

impl InputRouter {
    /// A router with the cursor at the centre of `screen`.
    pub fn new(screen: Rect) -> Self {
        let pointer =
            Point::new(screen.origin.x + (screen.size.w / 2) as i32, screen.origin.y + (screen.size.h / 2) as i32);
        Self {
            pointer,
            screen,
            inside: None,
            grab: None,
            grab_at: pointer,
            drag: None,
            resize: None,
            carrying: None,
            over: None,
            outline: None,
            grab_button: 0,
            grab_broken: false,
            buttons: 0,
            modifiers: 0,
            time_ns: 0,
            hotkeys: Vec::new(),
            zones: Vec::new(),
            in_zone: None,
            fired: Vec::new(),
            consumed: Vec::new(),
            held: Vec::new(),
        }
    }

    /// Where the cursor is, in screen coordinates.
    pub fn pointer(&self) -> Point {
        self.pointer
    }

    /// The rectangle an outline is being drawn at, if anything is drawing one.
    pub fn outline(&self) -> Option<Rect> {
        self.outline
    }

    /// The window the cursor is inside, if any.
    pub fn inside(&self) -> Option<u32> {
        self.inside
    }

    /// The window holding the implicit grab, if a button is down.
    pub fn grab(&self) -> Option<u32> {
        self.grab
    }

    /// Buttons held, as last reported.
    pub fn buttons(&self) -> u16 {
        self.buttons
    }

    /// Modifiers held, as last reported.
    pub fn modifiers(&self) -> u16 {
        self.modifiers
    }

    /// Route one logical event, appending what to send to `out`.
    pub fn route(&mut self, ev: &Logical, stack: &mut WindowStack, out: &mut Vec<Outbound>) -> Routed {
        // Mirror the interpreter's state first: generated enters and leaves take their stamps from
        // it.
        match *ev {
            Logical::Key { modifiers, time_ns, .. } => {
                self.modifiers = modifiers;
                self.time_ns = time_ns;
            }
            Logical::Motion { buttons, modifiers, time_ns, .. }
            | Logical::Button { buttons, modifiers, time_ns, .. }
            | Logical::Wheel { buttons, modifiers, time_ns, .. } => {
                self.buttons = buttons;
                self.modifiers = modifiers;
                self.time_ns = time_ns;
            }
            // The time survives a `Dropped`; synthesised crossings need a stamp.
            Logical::Dropped { time_ns } => {
                self.buttons = 0;
                self.modifiers = 0;
                self.time_ns = time_ns;
            }
        }

        // Reconcile after the mirror: it emits, and one batch's records must agree on `buttons`.
        self.reconcile_with(stack, out);
        let mut ended: Option<Ended> = None;
        let mut outline_gone: Option<Outline> = None;
        // A drag implies a grab, enforced here: `reconcile_with` breaks a grab but cannot reach the
        // drag.
        if self.grab.is_none() {
            (ended, outline_gone) = self.stop_drag(stack, false);
            // A resize goes with its grab; `finished` is false, so the shell is asked for nothing.
            let (r, gone) = self.stop_resize(stack, false);
            ended = ended.or(r);
            outline_gone = outline_gone.or(gone);
            // So does a payload; nothing is delivered.
            let (_, gone) = self.stop_carrying(stack, false);
            outline_gone = outline_gone.or(gone);
        }

        match *ev {
            Logical::Key { keycode, pressed, modifiers, locks, .. } => {
                // Before focus routing, and consuming: a copy would type the `2` of `Super+2` too.
                if self.take_as_hotkey(keycode, pressed, modifiers) {
                    return Routed { consumed: true, ended, outline: outline_gone, ..Routed::default() };
                }
                let Some(window) = stack.focus_candidate() else {
                    // Nothing focusable: dropped, not sent to the pointer's window, or typing would
                    // follow the cursor. A gesture ending on this event still gets out.
                    return Routed { ended, outline: outline_gone, ..Routed::default() };
                };
                // TODO(focus-change-key-balance): a key held across a focus change is delivered
                // to one window and released to another, or to none. Harmless today because
                // `KeyEvent` carries `modifiers` on every record, so no client accumulates
                // them — but it is the same unbalanced-press shape the chord rules above exist
                // to prevent, reached by a different route.
                out.push(Outbound::Key {
                    event: KeyEvent::new(window, keycode, u16::from(pressed), modifiers).with_locks(locks),
                });
                Routed { ended, outline: outline_gone, ..Routed::default() }
            }

            Logical::Motion { dx, dy, .. } => {
                self.move_by(dx, dy);
                // The drag first, so the crossing pass and the motion record see the window where
                // it now is.
                let moved = self.drag_to_pointer(stack);
                // The outline follows the pointer and reaches no window; a resize or a move over a
                // snap zone makes it.
                let outline = self
                    .outline_to_pointer()
                    .or_else(|| self.preview_zone())
                    .or_else(|| self.highlight_target(stack))
                    .or(outline_gone);
                self.update_crossing(stack, out);
                if let Some(window) = self.target(stack) {
                    self.emit(window, POINTER_MOTION, 0, 0, stack, out);
                }
                Routed { moved, outline, ended, ..Routed::default() }
            }

            Logical::Button { button, pressed, buttons, .. } => {
                let mut restacked = None;
                let mut dismissed = None;
                if pressed && self.grab.is_none() {
                    // Before the grab, or the first click after boot reaches a window never
                    // entered.
                    self.update_crossing(stack, out);
                    // Taken before the raise, which cannot change it: the hit is already topmost.
                    self.grab = self.hit(stack);
                    self.grab_at = self.pointer;
                    // The release owed this window if the grab ends any way but this button coming
                    // up.
                    self.grab_button = button;
                    // A press not on the popup dismisses it: pressing the desktop or a panel raises
                    // nothing, so focus alone left popups open. Its parent counts as outside, so
                    // clicking the opening button again closes the menu. The press is still
                    // delivered.
                    if let Some(top) = stack.focus_candidate()
                        && stack.window(top).is_some_and(|w| w.role.is_popup())
                        && self.grab != Some(top)
                    {
                        dismissed = Some(top);
                    }
                    if let Some(window) = self.grab
                        && stack.window(window).is_some_and(|w| w.role.takes_focus())
                    {
                        // Click to focus: the raise is the focus change. Empty damage (already on
                        // top) reports no restack.
                        restacked = stack.raise(window).ok().filter(|d| !d.is_empty()).map(Damage::rect);
                    }
                }

                if let Some(window) = self.target(stack) {
                    let flags = if pressed { POINTER_PRESSED } else { 0 };
                    self.emit(window, POINTER_BUTTON, button, flags, stack, out);
                }
                // **The dismissal follows the press that caused it**, so its client reads the press
                // first and knows the button is down when it is told.
                if let Some(window) = dismissed {
                    out.push(Outbound::Dismissed { window });
                }

                if !pressed && buttons == 0 {
                    // The drag ends with its grab; the stack records the gesture's one geometry
                    // change.
                    let (r, gone) = self.stop_drag(stack, true);
                    ended = ended.or(r);
                    outline_gone = outline_gone.or(gone);
                    let (r, gone) = self.stop_resize(stack, true);
                    ended = ended.or(r);
                    outline_gone = outline_gone.or(gone);
                    // A drag becomes a drop on a window that takes the payload; elsewhere the
                    // gesture just ends.
                    let (drop, gone) = self.stop_carrying(stack, true);
                    outline_gone = outline_gone.or(gone);
                    if let Some(rec) = drop {
                        out.push(rec);
                    }
                    // Last button up: re-derive the crossing, which also settles a broken grab.
                    self.grab = None;
                    self.grab_broken = false;
                    self.update_crossing(stack, out);
                }
                Routed { restacked, ended, outline: outline_gone, dismissed, ..Routed::default() }
            }

            // The wheel goes where the pointer is (the grab holder mid-drag). No crossing pass or
            // restack: scrolling is no reason to raise a window.
            Logical::Wheel { dz, .. } => {
                if let Some(window) = self.target(stack) {
                    self.emit_wheel(window, dz, stack, out);
                }
                Routed { ended, outline: outline_gone, ..Routed::default() }
            }

            Logical::Dropped { .. } => {
                // Buttons are no longer trustworthy, so the grab and its gestures go too, or input
                // would wedge.
                let had = self.grab.take().is_some() || self.grab_broken;
                self.grab_broken = false;
                let (r, gone) = self.stop_drag(stack, false);
                ended = ended.or(r);
                outline_gone = outline_gone.or(gone);
                let (r, gone) = self.stop_resize(stack, false);
                ended = ended.or(r);
                outline_gone = outline_gone.or(gone);
                let (_, gone) = self.stop_carrying(stack, false);
                outline_gone = outline_gone.or(gone);
                if had {
                    self.update_crossing(stack, out);
                }
                // Key beliefs go too: a stale `consumed` or `held` record would swallow a later
                // release.
                self.consumed.clear();
                self.held.clear();
                Routed { ended, outline: outline_gone, ..Routed::default() }
            }
        }
    }

    /// Drop router state that names a window which is gone or off screen; an off-screen one is
    /// told.
    fn reconcile_with(&mut self, stack: &WindowStack, out: &mut Vec<Outbound>) {
        let cur = stack.current_desktop();
        let state = |id: u32| match stack.window(id) {
            None => Gone,
            Some(w) if !w.visible_on(cur) => OffScreen,
            Some(_) => OnScreen,
        };

        if let Some(id) = self.grab
            && !matches!(state(id), OnScreen)
        {
            // The grab holder gets every event without consulting `hit`, so filtering hit-testing
            // alone leaves motion going to a hidden window.
            if matches!(state(id), OffScreen) {
                // Close the sequence: the release is owed, else the client keeps a pointer capture.
                // It is the tail of a sequence granted on screen.
                self.emit(id, POINTER_BUTTON, self.grab_button, 0, stack, out);
            }
            self.grab = None;
            // Unconditionally: the mirror has absorbed the provoking event, so a condition on
            // `buttons` would let a release reach the window underneath.
            self.grab_broken = true;
        }

        if let Some(id) = self.inside {
            match state(id) {
                OnScreen => {}
                // The leave goes out before the id is forgotten: `update_crossing` derives it from
                // `inside`.
                OffScreen => {
                    self.emit(id, POINTER_LEAVE, 0, 0, stack, out);
                    self.inside = None;
                }
                Gone => self.inside = None,
            }
        }
    }

    fn move_by(&mut self, dx: i32, dy: i32) {
        // Saturating: a mouse held against the edge accumulates forever, and `i32::MIN` would wrap.
        let x = self.pointer.x.saturating_add(dx);
        let y = self.pointer.y.saturating_add(dy);
        self.pointer = Point::new(
            x.clamp(self.screen.left(), self.screen.right() as i32 - 1),
            y.clamp(self.screen.top(), self.screen.bottom() as i32 - 1),
        );
    }

    /// Begin an interactive move of `window` on the client's behalf — `Surface::StartMove`.
    ///
    /// The grab is the authority: refused unless `window` holds it, else a client could move itself
    /// unprompted (`Place` is a manager op). Refused while a resize or drag-and-drop runs: one grab
    /// carries one gesture. A second call changes nothing, as rebuilding from the current origin
    /// would apply the travel twice.
    ///
    /// Returns the region the catch-up disturbed, which the caller must repaint.
    pub fn start_move(&mut self, window: u32, stack: &mut WindowStack) -> Result<Option<Rect>, StackError> {
        if self.grab != Some(window) || self.resize.is_some() || self.carrying.is_some() {
            return Err(StackError::NoSuchWindow);
        }
        if self.drag.is_some_and(|d| d.window == window) {
            return Ok(None);
        }
        let origin = stack.window(window).ok_or(StackError::NoSuchWindow)?.origin;
        stack.begin_drag(window)?;
        self.drag = Some(Drag { window, from: self.grab_at, origin, going: false });
        // Applied at once: the pointer moved during the round trip. Its damage is returned.
        Ok(self.drag_to_pointer(stack))
    }

    /// Begin an interactive resize of `window` on the client's behalf — `Surface::StartResize`.
    ///
    /// The grab is the authority, as for [`start_move`](Self::start_move). The window keeps its
    /// rectangle while an outline moves; the stack is marked dragged so a mid-gesture `Place` is
    /// refused. A second call changes nothing; one during a move is refused.
    ///
    /// Returns the outline's first rectangle, which the caller must draw.
    pub fn start_resize(
        &mut self,
        window: u32,
        edges: u32,
        stack: &mut WindowStack,
    ) -> Result<Option<Rect>, StackError> {
        if self.grab != Some(window) || self.drag.is_some() || self.carrying.is_some() {
            return Err(StackError::NoSuchWindow);
        }
        if !StartResize::edges_are_a_gesture(edges) {
            return Err(StackError::BadGeometry);
        }
        if self.resize.is_some_and(|r| r.window == window) {
            return Ok(self.outline);
        }
        let rect = stack.window(window).ok_or(StackError::NoSuchWindow)?.bounds();
        stack.begin_drag(window)?;
        self.resize = Some(Resize { window, from: self.grab_at, rect, edges });
        self.outline = Some(self.outline_now());
        Ok(self.outline)
    }

    /// Begin a drag-and-drop gesture out of `window` — `Surface::StartDrag`.
    ///
    /// The grab is the authority, sharper here: a drag offers a payload to whatever window it ends
    /// over. Refused while a move or resize runs; a second call replaces the payload.
    ///
    /// Returns the highlight to draw, if the pointer already stands over a window that takes it.
    pub fn start_drag(
        &mut self,
        window: u32,
        kind: u32,
        path: &str,
        name: &str,
        stack: &WindowStack,
    ) -> Result<Option<Outline>, StackError> {
        if self.grab != Some(window) || self.drag.is_some() || self.resize.is_some() {
            return Err(StackError::NoSuchWindow);
        }
        self.carrying = Some(Carrying {
            window,
            kind,
            path: alloc::string::String::from(path),
            name: alloc::string::String::from(name),
        });
        Ok(self.highlight_target(stack))
    }

    /// Whether a drag is in flight, for a caller deciding what a release means.
    pub fn is_dragging_payload(&self) -> bool {
        self.carrying.is_some()
    }

    /// Move the highlight to whatever window would take the drag now: the topmost under the
    /// pointer, and only if it takes this kind, so it never lies about where a payload lands.
    fn highlight_target(&mut self, stack: &WindowStack) -> Option<Outline> {
        let Some(c) = &self.carrying else { return None };
        // The source window is skipped, or a drag would highlight its own origin.
        let found = self.hit(stack).filter(|&w| w != c.window).filter(|&w| stack.acceptor_for(w, c.kind).is_some());
        if found == self.over {
            return None;
        }
        let was = self.outline;
        self.over = found;
        self.outline = found.and_then(|w| stack.window(w)).map(|w| w.bounds());
        Some(Outline { was, now: self.outline })
    }

    /// End a drag-and-drop gesture, if one is running. The drop is reported only if `finished`: a
    /// lost grab or guessed position is not somebody letting go. Returns the event to deliver and
    /// the highlight to take down.
    fn stop_carrying(&mut self, stack: &WindowStack, finished: bool) -> (Option<Outbound>, Option<Outline>) {
        let Some(c) = self.carrying.take() else { return (None, None) };
        let target = self.over.take();
        let gone = self.outline.take().map(|was| Outline { was: Some(was), now: None });
        let event = finished
            .then_some(target)
            .flatten()
            // Re-read: the window may have gone since the last motion.
            .and_then(|w| {
                let acceptor = stack.acceptor_for(w, c.kind)?;
                let rect = stack.window(w)?.bounds();
                Some(Outbound::Dropped {
                    window: w,
                    acceptor: alloc::string::String::from(acceptor),
                    kind: c.kind,
                    path: c.path.clone(),
                    name: c.name.clone(),
                    x: self.pointer.x - rect.origin.x,
                    y: self.pointer.y - rect.origin.y,
                })
            });
        (event, gone)
    }

    /// The rectangle the outline should occupy for the pointer's position, each axis at least
    /// [`MIN_RESIZE`] so a crossed edge cannot invert it.
    fn outline_now(&self) -> Rect {
        let Some(r) = self.resize else { return Rect::new(0, 0, 0, 0) };
        let (dx, dy) = (self.pointer.x - r.from.x, self.pointer.y - r.from.y);
        let (mut x, mut y) = (r.rect.origin.x, r.rect.origin.y);
        let (mut w, mut h) = (r.rect.size.w as i64, r.rect.size.h as i64);
        let min = MIN_RESIZE as i64;
        if r.edges & RESIZE_LEFT != 0 {
            let take = (dx as i64).min(w - min);
            x += take as i32;
            w -= take;
        }
        if r.edges & RESIZE_RIGHT != 0 {
            w = (w + dx as i64).max(min);
        }
        if r.edges & RESIZE_TOP != 0 {
            let take = (dy as i64).min(h - min);
            y += take as i32;
            h -= take;
        }
        if r.edges & RESIZE_BOTTOM != 0 {
            h = (h + dy as i64).max(min);
        }
        // Clamped per edge: the floor belongs to the axes being dragged, or a window narrower than
        // `MIN_RESIZE` resized by its bottom edge would widen.
        Rect::new(x, y, w.max(0) as u32, h.max(0) as u32)
    }

    fn outline_to_pointer(&mut self) -> Option<Outline> {
        self.resize?;
        let was = self.outline;
        let now = self.outline_now();
        if was == Some(now) {
            return None;
        }
        self.outline = Some(now);
        Some(Outline { was, now: Some(now) })
    }

    /// Show, or stop showing, the snap target under the pointer during a move; `None` if unchanged.
    fn preview_zone(&mut self) -> Option<Outline> {
        if !self.drag.is_some_and(|d| d.going) {
            // Not a move shows no preview, and anything left over from one goes.
            return self.leave_zone();
        }
        let found = self.zone_at(self.pointer);
        match (self.in_zone, found) {
            (Some(was), Some(z)) if was == z.id => None,
            (_, Some(z)) => {
                let was = self.outline;
                self.in_zone = Some(z.id);
                self.outline = Some(Rect::new(z.target_x, z.target_y, z.target_w, z.target_h));
                Some(Outline { was, now: self.outline })
            }
            (Some(_), None) => self.leave_zone(),
            (None, None) => None,
        }
    }

    fn leave_zone(&mut self) -> Option<Outline> {
        self.in_zone.take()?;
        let was = self.outline.take();
        Some(Outline { was, now: None })
    }

    /// End an interactive resize, if one is running. Reports a rectangle only if `finished`.
    ///
    /// Called wherever the grab ends, like [`stop_drag`](Self::stop_drag). `finished` is true only
    /// for the button coming up: a resize has applied nothing, so reporting from a guessed position
    /// (`Dropped`) would jump the window to a half-chosen size.
    fn stop_resize(&mut self, stack: &mut WindowStack, finished: bool) -> (Option<Ended>, Option<Outline>) {
        let Some(r) = self.resize else { return (None, None) };
        let final_rect = self.outline_now();
        self.resize = None;
        stack.end_drag();
        let gone = self.outline.take().map(|was| Outline { was: Some(was), now: None });
        // A request needs the gesture finished, the window on screen, and the rectangle changed.
        let on_screen = stack.window(r.window).is_some_and(|w| w.visible_on(stack.current_desktop()));
        let ended = (finished && final_rect != r.rect && on_screen).then_some(Ended {
            window: r.window,
            asked: final_rect,
            from: r.rect,
            moved: false,
        });
        (ended, gone)
    }

    /// End an interactive move, if one is running, wherever the grab ends: a drag outliving it kept
    /// following a pointer with nothing held.
    fn stop_drag(&mut self, stack: &mut WindowStack, finished: bool) -> (Option<Ended>, Option<Outline>) {
        let Some(d) = self.drag.take() else { return (None, None) };
        stack.end_drag();
        // A second line behind `reconcile_with`; pinned by
        // `a_gesture_whose_window_has_gone_asks_for_nothing…`.
        let now = stack.window(d.window).map(|w| w.bounds());
        // The zone it ended in, read from `in_zone` so the rectangle asked for is the one shown.
        let zone = self
            .in_zone
            .filter(|_| finished)
            .and_then(|id| self.zones.iter().find(|z| z.id == id))
            .map(|z| Rect::new(z.target_x, z.target_y, z.target_w, z.target_h));
        let ended = now.and_then(|now| {
            let from = Rect::new(d.origin.x, d.origin.y, now.size.w, now.size.h);
            // Outside a zone, a move reports where it went however it ended, as it is already on
            // screen: the manager learns a person moved the window, which its own `Place` hides.
            let (asked, moved) = match zone {
                Some(target) => (target, false),
                None => ((now != from).then_some(now)?, true),
            };
            Some(Ended { window: d.window, asked, from, moved })
        });
        (ended, self.leave_zone())
    }

    /// Move the dragged window so it keeps the same point under the pointer.
    fn drag_to_pointer(&mut self, stack: &mut WindowStack) -> Option<Rect> {
        let pointer = self.pointer;
        let d = self.drag.as_mut()?;
        let travel = (pointer.x - d.from.x).abs().max((pointer.y - d.from.y).abs());
        d.going |= travel >= DRAG_THRESHOLD;
        if !d.going {
            return None;
        }
        let d = *d;
        let origin = Point::new(d.origin.x + (self.pointer.x - d.from.x), d.origin.y + (self.pointer.y - d.from.y));
        match stack.drag_to(d.window, origin) {
            Ok(damage) => Some(damage.rect()),
            Err(_) => {
                // The window has gone; there is nobody to snap.
                let _ = self.stop_drag(stack, false);
                None
            }
        }
    }

    /// Register a chord, to be delivered to the manager instead of the focused window.
    ///
    /// `Err` for a zero or duplicate id, a duplicate chord, or a full table — never a silent
    /// replacement.
    pub fn register_hotkey(&mut self, hk: MgrHotkey) -> Result<(), HotkeyError> {
        if hk.id == 0 {
            return Err(HotkeyError::ZeroId);
        }
        if self.hotkeys.iter().any(|h| h.id == hk.id) {
            return Err(HotkeyError::DuplicateId);
        }
        // A duplicate chord is refused too: `find` returns the first match, so a second would be
        // silent.
        if self.hotkeys.iter().any(|h| h.mods == hk.mods && h.code == hk.code) {
            return Err(HotkeyError::DuplicateChord);
        }
        if self.hotkeys.len() >= MAX_HOTKEYS {
            return Err(HotkeyError::TableFull);
        }
        self.hotkeys.push(hk);
        Ok(())
    }

    /// Register a snap zone, **replacing** any zone already under that id.
    ///
    /// `Err` if `id` is zero or the table is full and this is a new id. `Ok(Some(_))` when this
    /// replaced the zone a drag is previewing, which the caller must repaint. Replacing, unlike
    /// chords: a shell recomputes the whole layout when the work area changes.
    pub fn register_zone(&mut self, z: MgrSnapZone) -> Result<Option<Outline>, HotkeyError> {
        if z.id == 0 {
            return Err(HotkeyError::ZeroId);
        }
        if let Some(existing) = self.zones.iter_mut().find(|e| e.id == z.id) {
            *existing = z;
            // A preview of the zone just replaced follows it: `LayoutChanged` can arrive mid-drag.
            if self.in_zone == Some(z.id) {
                let was = self.outline;
                self.outline = Some(Rect::new(z.target_x, z.target_y, z.target_w, z.target_h));
                return Ok(Some(Outline { was, now: self.outline }));
            }
            return Ok(None);
        }
        if self.zones.len() >= MAX_SNAP_ZONES {
            return Err(HotkeyError::TableFull);
        }
        self.zones.push(z);
        Ok(None)
    }

    /// Forget every snap zone — the manager has gone. Returns the preview it took down, which the
    /// caller must repaint.
    pub fn clear_zones(&mut self) -> Option<Outline> {
        self.zones.clear();
        self.leave_zone()
    }

    /// The zone whose trigger region contains `at`; first match wins.
    fn zone_at(&self, at: Point) -> Option<MgrSnapZone> {
        self.zones
            .iter()
            .copied()
            .find(|z| Rect::new(z.trigger_x, z.trigger_y, z.trigger_w, z.trigger_h).contains(at.x, at.y))
    }

    /// Take the chords matched since this was last called.
    pub fn take_hotkeys(&mut self) -> Vec<MgrHotkey> {
        core::mem::take(&mut self.fired)
    }

    /// Consume a key transition if it belongs to a registered chord; `true` when it must not reach
    /// the focused window. A press matches `mods` exactly, and its release is swallowed by keycode.
    fn take_as_hotkey(&mut self, keycode: u16, pressed: bool, modifiers: u16) -> bool {
        if !pressed {
            // Removing the record stops one press swallowing every later release of the key.
            if let Some(i) = self.consumed.iter().position(|&k| k == keycode) {
                self.consumed.swap_remove(i);
                return true;
            }
            if let Some(i) = self.held.iter().position(|&k| k == keycode) {
                self.held.swap_remove(i);
            }
            return false;
        }
        // A key already down cannot begin a chord: its release must be delivered too.
        if self.held.contains(&keycode) {
            return false;
        }
        let Some(hk) = self.hotkeys.iter().find(|h| h.code == keycode && h.mods == modifiers) else {
            if !self.held.contains(&keycode) {
                self.held.push(keycode);
            }
            return false;
        };
        self.fired.push(*hk);
        // Guarded against a repeat, or only the first release would be swallowed.
        if !self.consumed.contains(&keycode) {
            self.consumed.push(keycode);
        }
        true
    }

    /// Forget every chord a manager registered, when its channel closes: they would be consumed and
    /// delivered to nobody.
    pub fn clear_hotkeys(&mut self) {
        self.hotkeys.clear();
        self.consumed.clear();
    }

    /// The topmost window containing the cursor, whatever its role.
    fn hit(&self, stack: &WindowStack) -> Option<u32> {
        stack
            .windows()
            .iter()
            .rev()
            // Only what is on screen can be clicked; `visible_on` is shared with `compose_into`.
            .find(|w| w.visible_on(stack.current_desktop()) && w.bounds().contains(self.pointer.x, self.pointer.y))
            .map(|w| w.id)
    }

    /// Who a pointer event goes to: the grab holder, else what is under the cursor.
    fn target(&self, stack: &WindowStack) -> Option<u32> {
        if self.grab_broken {
            // The sequence belonged to a window that can no longer receive it; `hit` would hand its
            // tail on.
            return None;
        }
        self.grab.or_else(|| self.hit(stack))
    }

    /// Emit leave/enter if the cursor changed windows. A no-op while grabbed.
    fn update_crossing(&mut self, stack: &WindowStack, out: &mut Vec<Outbound>) {
        if self.grab.is_some() || self.grab_broken {
            // Crossings during a drag would contradict what the grab holder receives; a broken grab
            // counts.
            return;
        }
        let now = self.hit(stack);
        if now == self.inside {
            return;
        }
        if let Some(old) = self.inside {
            self.emit(old, POINTER_LEAVE, 0, 0, stack, out);
        }
        self.inside = now;
        if let Some(new) = now {
            self.emit(new, POINTER_ENTER, 0, 0, stack, out);
        }
    }

    fn emit(&self, window: u32, kind: u16, button: u16, flags: u16, stack: &WindowStack, out: &mut Vec<Outbound>) {
        if let Some(event) = self.record(window, kind, button, flags, stack) {
            out.push(Outbound::Pointer { event });
        }
    }

    /// Emit a `POINTER_WHEEL` record carrying `dz` detents, saturating into the wire's `i16`:
    /// wrapping would scroll the other way.
    fn emit_wheel(&self, window: u32, dz: i32, stack: &WindowStack, out: &mut Vec<Outbound>) {
        if let Some(event) = self.record(window, POINTER_WHEEL, 0, 0, stack) {
            let dz = dz.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            out.push(Outbound::Pointer { event: event.with_wheel(dz) });
        }
    }

    fn record(&self, window: u32, kind: u16, button: u16, flags: u16, stack: &WindowStack) -> Option<PointerEvent> {
        let w = stack.window(window)?;
        let origin = w.bounds().origin;
        Some(PointerEvent::new(
            window,
            kind,
            button,
            self.buttons,
            flags,
            self.modifiers,
            // Signed and unclamped: under a grab the cursor is outside the window.
            self.pointer.x.saturating_sub(origin.x),
            self.pointer.y.saturating_sub(origin.y),
            // The interrupt's time, divided once; a client's own clock measures when it got round
            // to it.
            self.time_ns / 1_000_000,
        ))
    }
}

#[cfg(test)]
mod tests;

/// What the router hands on puts a dismissal after the press that caused it, so its client reads
/// the press first and knows a button is down when it is told.
#[cfg(test)]
#[test]
fn a_dismissal_follows_the_press_that_caused_it() {
    use librsproto::surface::Role;
    use tests::{SCREEN, button, warp, win};
    let mut s = WindowStack::new();
    let window = win(&mut s, Role::Normal, 0, 0, 400, 400);
    let menu = win(&mut s, Role::Popup { parent: window }, 10, 10, 100, 100);
    let mut r = InputRouter::new(SCREEN);
    warp(&mut r, &mut s, 300, 300);
    let mut out = Vec::new();
    r.route(&button(true), &mut s, &mut out);
    let at = |want: &dyn Fn(&Outbound) -> bool| out.iter().position(want);
    let press =
        at(&|o| matches!(o, Outbound::Pointer { event } if event.kind == POINTER_BUTTON && event.window == window));
    let dismissed = at(&|o| matches!(o, Outbound::Dismissed { window: w } if *w == menu));
    assert!(press.is_some() && dismissed.is_some() && press < dismissed, "{out:?}");
}
