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
    /// A gesture ended, and this is the rectangle it asks for; the manager applies it, so it
    /// becomes a `DragEnded`.
    pub resized: Option<(u32, Rect)>,
    /// A popup a press landed outside of; the compositor reports, its owner decides to close.
    pub dismissed: Option<u32>,
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
}

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
        let mut ended: Option<(u32, Rect)> = None;
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
                    return Routed { consumed: true, resized: ended, outline: outline_gone, ..Routed::default() };
                }
                let Some(window) = stack.focus_candidate() else {
                    // Nothing focusable: dropped, not sent to the pointer's window, or typing would
                    // follow the cursor. A gesture ending on this event still gets out.
                    return Routed { resized: ended, outline: outline_gone, ..Routed::default() };
                };
                // TODO(focus-change-key-balance): a key held across a focus change is delivered
                // to one window and released to another, or to none. Harmless today because
                // `KeyEvent` carries `modifiers` on every record, so no client accumulates
                // them — but it is the same unbalanced-press shape the chord rules above exist
                // to prevent, reached by a different route.
                out.push(Outbound::Key {
                    event: KeyEvent::new(window, keycode, u16::from(pressed), modifiers).with_locks(locks),
                });
                Routed { resized: ended, outline: outline_gone, ..Routed::default() }
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
                Routed { moved, outline, resized: ended, ..Routed::default() }
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
                Routed { restacked, resized: ended, outline: outline_gone, dismissed, ..Routed::default() }
            }

            // The wheel goes where the pointer is (the grab holder mid-drag). No crossing pass or
            // restack: scrolling is no reason to raise a window.
            Logical::Wheel { dz, .. } => {
                if let Some(window) = self.target(stack) {
                    self.emit_wheel(window, dz, stack, out);
                }
                Routed { resized: ended, outline: outline_gone, ..Routed::default() }
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
                Routed { resized: ended, outline: outline_gone, ..Routed::default() }
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
        self.drag = Some(Drag { window, from: self.grab_at, origin });
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
        if self.drag.is_none() {
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
    fn stop_resize(&mut self, stack: &mut WindowStack, finished: bool) -> (Option<(u32, Rect)>, Option<Outline>) {
        let Some(r) = self.resize else { return (None, None) };
        let final_rect = self.outline_now();
        self.resize = None;
        stack.end_drag();
        let gone = self.outline.take().map(|was| Outline { was: Some(was), now: None });
        // A request needs the gesture finished, the window on screen, and the rectangle changed.
        let on_screen = stack.window(r.window).is_some_and(|w| w.visible_on(stack.current_desktop()));
        let rect = (finished && final_rect != r.rect && on_screen).then_some((r.window, final_rect));
        (rect, gone)
    }

    /// End an interactive move, if one is running, wherever the grab ends: a drag outliving it kept
    /// following a pointer with nothing held.
    fn stop_drag(&mut self, stack: &mut WindowStack, finished: bool) -> (Option<(u32, Rect)>, Option<Outline>) {
        let Some(d) = self.drag.take() else { return (None, None) };
        stack.end_drag();
        // The zone it ended in, read from `in_zone` so the rectangle asked for is the one shown.
        let asked = self
            .in_zone
            .filter(|_| finished)
            .and_then(|id| self.zones.iter().find(|z| z.id == id))
            // A second line behind `reconcile_with`; pinned by
            // `a_gesture_whose_window_has_gone_asks_for_nothing…`.
            .filter(|_| stack.window(d.window).is_some())
            .map(|z| (d.window, Rect::new(z.target_x, z.target_y, z.target_w, z.target_h)));
        (asked, self.leave_zone())
    }

    /// Move the dragged window so it keeps the same point under the pointer.
    fn drag_to_pointer(&mut self, stack: &mut WindowStack) -> Option<Rect> {
        let d = self.drag?;
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
mod tests {
    use super::*;
    use librsproto::surface::{
        AttachBufferRequest, CommitRequest, CreateWindowRequest, DROP_KIND_DIR, DROP_KIND_FILE, Edge, MOD_SHIFT, Role,
        SURFACE_FORMAT_XRGB8888,
    };

    const SCREEN: Rect = Rect::new(0, 0, 640, 480);
    const BTN_LEFT: u16 = 0x110;

    /// A window at `(x, y)` of `w × h`, committed and configured: routing ignores any other.
    fn win(stack: &mut WindowStack, role: Role, x: i32, y: i32, w: u32, h: u32) -> u32 {
        let id = stack.create(&CreateWindowRequest::new(w, h, role)).expect("create");
        stack.mark_configured(id);
        let _ = stack.place(id, Point::new(x, y)).expect("origin");
        stack
            .attach(&AttachBufferRequest {
                window: id,
                buffer: 1,
                width: w,
                height: h,
                pitch: w * 4,
                format: SURFACE_FORMAT_XRGB8888,
            })
            .expect("attach");
        stack
            .commit(&CommitRequest { window: id, buffer: 1, damage_x: 0, damage_y: 0, damage_w: w, damage_h: h })
            .expect("commit");
        id
    }

    fn go(r: &mut InputRouter, s: &mut WindowStack, ev: Logical) -> Vec<Outbound> {
        let mut out = Vec::new();
        r.route(&ev, s, &mut out);
        out
    }

    fn warp(r: &mut InputRouter, s: &mut WindowStack, x: i32, y: i32) {
        let p = r.pointer();
        go(r, s, motion(x - p.x, y - p.y));
    }

    /// The interrupt time every helper stamps; its milliseconds are neither zero nor round.
    const T: u64 = 7_123_456_789;
    const T_MS: u64 = T / 1_000_000;

    fn motion(dx: i32, dy: i32) -> Logical {
        Logical::Motion { dx, dy, buttons: 0, modifiers: 0, time_ns: T }
    }

    fn drag(dx: i32, dy: i32) -> Logical {
        Logical::Motion { dx, dy, buttons: 1, modifiers: 0, time_ns: T }
    }

    fn key(keycode: u16, pressed: bool) -> Logical {
        Logical::Key { keycode, pressed, modifiers: 0, locks: librsproto::surface::LOCK_NUM, time_ns: T }
    }

    /// A key transition with modifiers held, Num Lock on as from boot: a chord ignores locks.
    fn chord(keycode: u16, pressed: bool, modifiers: u16) -> Logical {
        Logical::Key { keycode, pressed, modifiers, locks: librsproto::surface::LOCK_NUM, time_ns: T }
    }

    const MOD_SUPER: u16 = librsproto::surface::MOD_META;
    const KEY_2: u16 = 3;

    fn two_windows(s: &mut WindowStack) -> (u32, u32) {
        let src = win(s, Role::Normal, 0, 0, 200, 200);
        let dst = win(s, Role::Normal, 400, 0, 200, 200);
        (src, dst)
    }

    fn begin_drag(r: &mut InputRouter, s: &mut WindowStack, src: u32, kind: u32, to: (i32, i32)) -> Option<Outline> {
        warp(r, s, 20, 20);
        go(r, s, button(true));
        let started = r.start_drag(src, kind, "/home/a.txt", "a.txt", s).expect("the grab is held");
        let p = r.pointer();
        go(r, s, drag(to.0 - p.x, to.1 - p.y));
        started
    }

    fn drop_of(out: &[Outbound]) -> Option<&Outbound> {
        out.iter().find(|o| matches!(o, Outbound::Dropped { .. }))
    }

    #[test]
    fn a_drag_highlights_a_window_that_takes_it_and_delivers_on_release() {
        let mut s = WindowStack::new();
        let (src, dst) = two_windows(&mut s);
        s.declare_acceptor(dst, "document", DROP_KIND_FILE).unwrap();
        let mut r = InputRouter::new(SCREEN);

        begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (450, 60));
        assert_eq!(r.outline(), Some(Rect::new(400, 0, 200, 200)), "the window that would take it is outlined");

        let out = go(&mut r, &mut s, button(false));
        let Some(Outbound::Dropped { window, acceptor, kind, path, name, x, y }) = drop_of(&out) else {
            panic!("no drop was delivered: {out:?}");
        };
        assert_eq!(*window, dst);
        assert_eq!(
            (acceptor.as_str(), kind, path.as_str(), name.as_str()),
            ("document", &DROP_KIND_FILE, "/home/a.txt", "a.txt")
        );
        assert_eq!((*x, *y), (50, 60), "the pointer, relative to the window it landed on");
        assert_eq!(r.outline(), None, "and the highlight goes with the gesture");
    }

    #[test]
    fn a_window_that_does_not_take_this_kind_is_neither_highlighted_nor_dropped_on() {
        let mut s = WindowStack::new();
        let (src, dst) = two_windows(&mut s);
        s.declare_acceptor(dst, "document", DROP_KIND_FILE).unwrap();
        let mut r = InputRouter::new(SCREEN);

        begin_drag(&mut r, &mut s, src, DROP_KIND_DIR, (450, 60));
        assert_eq!(r.outline(), None, "a folder over a files-only window highlights nothing");

        let out = go(&mut r, &mut s, button(false));
        assert!(drop_of(&out).is_none(), "and letting go delivers nothing");
    }

    #[test]
    fn a_window_with_no_acceptor_is_neither_highlighted_nor_dropped_on() {
        // A window that declared nothing is as inert as one that declared the wrong kind.
        let mut s = WindowStack::new();
        let (src, dst) = two_windows(&mut s);
        let _ = dst;
        let mut r = InputRouter::new(SCREEN);

        begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (450, 60));
        assert_eq!(r.outline(), None);

        let out = go(&mut r, &mut s, button(false));
        assert!(drop_of(&out).is_none());
    }

    #[test]
    fn a_drag_does_not_highlight_the_window_it_came_out_of() {
        let mut s = WindowStack::new();
        let src = win(&mut s, Role::Normal, 0, 0, 200, 200);
        s.declare_acceptor(src, "self", DROP_KIND_FILE).unwrap();
        let mut r = InputRouter::new(SCREEN);

        let started = begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (60, 60));
        assert!(started.is_none(), "nothing to highlight when the drag begins");
        assert_eq!(r.outline(), None);
        let out = go(&mut r, &mut s, button(false));
        assert!(drop_of(&out).is_none(), "and dropping a thing back where it came from is a no-op");
    }

    #[test]
    fn a_drag_that_ends_any_way_but_the_button_delivers_nothing() {
        // Two ways a gesture ends without the button: an input loss, and the grab's window leaving.
        for lost_input in [true, false] {
            let mut s = WindowStack::new();
            let (src, dst) = two_windows(&mut s);
            s.declare_acceptor(dst, "document", DROP_KIND_FILE).unwrap();
            let mut r = InputRouter::new(SCREEN);

            begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (450, 60));
            assert!(r.outline().is_some(), "precondition: the target is highlighted");

            let out = if lost_input {
                go(&mut r, &mut s, Logical::Dropped { time_ns: T })
            } else {
                s.set_minimized(src, true).expect("minimize");
                go(&mut r, &mut s, drag(1, 0))
            };
            assert!(drop_of(&out).is_none(), "an interrupted gesture is not somebody letting go");
            assert_eq!(r.outline(), None, "and the highlight comes down with the gesture");
        }
    }

    #[test]
    fn the_finished_guard_is_what_refuses_a_drop_and_not_only_its_callers() {
        // Called directly: only a direct call can see the `finished` guard.
        let mut s = WindowStack::new();
        let (src, dst) = two_windows(&mut s);
        s.declare_acceptor(dst, "document", DROP_KIND_FILE).unwrap();
        let mut r = InputRouter::new(SCREEN);
        begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (450, 60));
        assert_eq!(r.over, Some(dst), "precondition: a drop would land on the target");

        let (event, gone) = r.stop_carrying(&s, false);
        assert!(event.is_none(), "an unfinished gesture delivers nothing");
        assert!(gone.is_some(), "and its highlight still comes down");
        assert!(r.carrying.is_none(), "the payload goes either way");
    }

    #[test]
    fn a_drag_is_refused_unless_the_pointer_is_holding_the_window() {
        let mut s = WindowStack::new();
        let (src, dst) = two_windows(&mut s);
        let mut r = InputRouter::new(SCREEN);

        assert!(r.start_drag(src, DROP_KIND_FILE, "/a", "a", &s).is_err());

        warp(&mut r, &mut s, 450, 20);
        go(&mut r, &mut s, button(true));
        assert!(r.start_drag(src, DROP_KIND_FILE, "/a", "a", &s).is_err());
        assert!(r.start_drag(dst, DROP_KIND_FILE, "/a", "a", &s).is_ok(), "the held one may");
    }

    #[test]
    fn one_grab_carries_one_gesture() {
        let mut s = WindowStack::new();
        let src = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 20, 20);
        go(&mut r, &mut s, button(true));

        r.start_move(src, &mut s).expect("the grab is held");
        assert!(r.start_drag(src, DROP_KIND_FILE, "/a", "a", &s).is_err(), "not while moving");

        let mut s = WindowStack::new();
        let src = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 20, 20);
        go(&mut r, &mut s, button(true));
        r.start_drag(src, DROP_KIND_FILE, "/a", "a", &s).expect("the grab is held");
        assert!(r.start_move(src, &mut s).is_err(), "and not the other way round either");
        assert!(r.start_resize(src, RESIZE_RIGHT, &mut s).is_err());
    }

    #[test]
    fn a_window_may_declare_a_bounded_number_of_acceptors() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 0, 0, 200, 200);
        for i in 0..librsproto::surface::MAX_ACCEPTORS {
            let name = alloc::format!("sink{i}");
            s.declare_acceptor(w, &name, DROP_KIND_FILE).expect("within the bound");
        }
        assert_eq!(
            s.declare_acceptor(w, "one-more", DROP_KIND_FILE),
            Err(StackError::TooManyAcceptors),
            "refused rather than evicting one the client still believes in"
        );
        s.declare_acceptor(w, "sink0", DROP_KIND_DIR).expect("replaces");
        assert_eq!(s.acceptor_for(w, DROP_KIND_DIR), Some("sink0"));
    }

    #[test]
    fn a_registered_chord_goes_to_the_manager_and_not_to_the_focused_window() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 7, mods: MOD_SUPER, code: KEY_2 }).unwrap();
        assert_eq!(s.focus_candidate(), Some(w), "precondition: the window has the keyboard");

        let out = go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        assert!(out.is_empty(), "a consumed chord types into nothing");
        assert_eq!(
            r.take_hotkeys(),
            alloc::vec![MgrHotkey { id: 7, mods: MOD_SUPER, code: KEY_2 }],
            "the manager is told which chord fired"
        );
        assert!(r.take_hotkeys().is_empty(), "and told once");

        // The release too: delivering it hands the window a release for a press it never saw.
        let out = go(&mut r, &mut s, chord(KEY_2, false, MOD_SUPER));
        assert!(out.is_empty(), "the release is consumed with the press");
    }

    #[test]
    fn a_chords_release_is_swallowed_by_keycode_even_after_the_modifiers_change() {
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

        go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        let out = go(&mut r, &mut s, chord(KEY_2, false, 0));
        assert!(out.is_empty(), "released after Super came up, and still swallowed");

        let out = go(&mut r, &mut s, key(KEY_2, true));
        assert_eq!(out.len(), 1, "an unmodified press is delivered normally");
    }

    #[test]
    fn modifiers_must_match_exactly() {
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

        let out = go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER | MOD_SHIFT));
        assert_eq!(out.len(), 1, "Super+Shift+2 is not Super+2, so it reaches the window");
        assert!(r.take_hotkeys().is_empty(), "and fires nothing");
    }

    #[test]
    fn an_auto_repeating_chord_does_not_leave_a_release_owed_twice() {
        // One record per press would swallow the next unrelated release of the key too.
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

        go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        assert_eq!(r.take_hotkeys().len(), 3, "each press is a chord press");

        assert!(go(&mut r, &mut s, chord(KEY_2, false, MOD_SUPER)).is_empty(), "one release owed");
        let out = go(&mut r, &mut s, key(KEY_2, false));
        assert_eq!(out.len(), 1, "and only one -- a later release is an ordinary keystroke");
    }

    #[test]
    fn a_consumed_chord_tells_the_caller_so_it_does_not_arm_a_repeat() {
        // The binary arms key repeat from the physical transition, so a consumed chord must say so.
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

        let mut out = Vec::new();
        let routed = r.route(&chord(KEY_2, true, MOD_SUPER), &mut s, &mut out);
        assert!(routed.consumed, "a chord press must report itself consumed");
        out.clear();
        let routed = r.route(&key(KEY_2, true), &mut s, &mut out);
        assert!(!routed.consumed, "an ordinary press is not");
    }

    #[test]
    fn a_key_already_down_cannot_begin_a_chord() {
        // `2` delivered, then Super held: the repeat would match `Super+2` and swallow the
        // physical release.
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

        assert_eq!(go(&mut r, &mut s, key(KEY_2, true)).len(), 1, "delivered while unmodified");
        let out = go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        assert_eq!(out.len(), 1, "a key already down is a repeat, not a new chord");
        assert!(r.take_hotkeys().is_empty(), "and it fires nothing");
        assert_eq!(go(&mut r, &mut s, key(KEY_2, false)).len(), 1, "its release is delivered");
    }

    #[test]
    fn dropped_forgets_which_keys_were_consumed() {
        // `Dropped` says held-key state is unknown: a stale record would swallow a later release.
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

        go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        go(&mut r, &mut s, Logical::Dropped { time_ns: T });
        assert_eq!(go(&mut r, &mut s, key(KEY_2, true)).len(), 1, "an ordinary press after");
        assert_eq!(
            go(&mut r, &mut s, key(KEY_2, false)).len(),
            1,
            "and its release is delivered rather than swallowed by a stale record"
        );
    }

    #[test]
    fn clearing_the_table_stops_chords_being_consumed_by_nobody() {
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();
        assert!(go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER)).is_empty(), "consumed");
        r.take_hotkeys();

        r.clear_hotkeys();
        let out = go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        assert_eq!(out.len(), 1, "with no table the chord is an ordinary keystroke again");
        assert!(r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).is_ok());
    }

    #[test]
    fn registration_refuses_zero_a_duplicate_and_a_full_table() {
        let mut r = InputRouter::new(SCREEN);
        assert_eq!(
            r.register_hotkey(MgrHotkey { id: 0, mods: 0, code: 1 }),
            Err(HotkeyError::ZeroId),
            "zero is reserved so a zeroed body registers nothing"
        );
        r.register_hotkey(MgrHotkey { id: 1, mods: 0, code: 1 }).unwrap();
        assert_eq!(
            r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: 2 }),
            Err(HotkeyError::DuplicateId),
            "refused rather than silently replaced"
        );
        assert_eq!(
            r.register_hotkey(MgrHotkey { id: 2, mods: 0, code: 1 }),
            Err(HotkeyError::DuplicateChord),
            "a second id for the same chord would be permanently silent"
        );
        for i in 2..=MAX_HOTKEYS as u32 {
            r.register_hotkey(MgrHotkey { id: i, mods: 0, code: i as u16 }).unwrap();
        }
        assert_eq!(r.register_hotkey(MgrHotkey { id: 99, mods: 0, code: 99 }), Err(HotkeyError::TableFull));
    }

    fn button(pressed: bool) -> Logical {
        Logical::Button { button: BTN_LEFT, pressed, buttons: if pressed { 1 } else { 0 }, modifiers: 0, time_ns: T }
    }

    #[test]
    fn keys_go_to_the_topmost_focusable_window_not_the_one_under_the_cursor() {
        let mut s = WindowStack::new();
        let under = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let top = win(&mut s, Role::Normal, 300, 300, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        assert_eq!(r.inside(), Some(under), "cursor is over the bottom window");

        let out = go(&mut r, &mut s, key(30, true));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].window(), top, "the key went to the focused window");
    }

    #[test]
    fn a_key_carries_its_locks_to_the_window() {
        use librsproto::surface::{LOCK_CAPS, LOCK_NUM};
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        let caps = Logical::Key { keycode: 30, pressed: true, modifiers: 0, locks: LOCK_CAPS | LOCK_NUM, time_ns: T };
        let out = go(&mut r, &mut s, caps);
        match out.as_slice() {
            [Outbound::Key { event }] => assert_eq!((event.window, event.locks), (w, LOCK_CAPS | LOCK_NUM)),
            other => panic!("not one key: {other:?}"),
        }
    }

    #[test]
    fn a_window_whose_configure_is_held_takes_neither_focus_nor_a_click() {
        let mut s = WindowStack::new();
        let visible = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let held = s.create(&CreateWindowRequest::new(200, 200, Role::Normal)).expect("create");

        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        assert_eq!(r.inside(), Some(visible), "the click lands on what is on screen");

        let out = go(&mut r, &mut s, key(30, true));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].window(), visible, "and so does the keystroke");

        s.mark_configured(held);
        warp(&mut r, &mut s, 51, 51);
        assert_eq!(r.inside(), Some(held));
        let out = go(&mut r, &mut s, key(30, true));
        assert_eq!(out[out.len() - 1].window(), held);
    }

    #[test]
    fn a_panel_never_takes_a_keystroke_but_does_take_a_click() {
        let mut s = WindowStack::new();
        let term = win(&mut s, Role::Normal, 0, 100, 640, 380);
        let panel = win(&mut s, Role::Panel { dock: Edge::Top, reserve: 100 }, 0, 0, 640, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 320, 50);

        let out = go(&mut r, &mut s, button(true));
        assert!(
            out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == panel)),
            "the panel got the click"
        );
        go(&mut r, &mut s, button(false));

        let out = go(&mut r, &mut s, key(30, true));
        assert_eq!(out[0].window(), term, "but the keystroke still goes to the terminal");
    }

    #[test]
    fn a_window_on_another_desktop_cannot_be_clicked() {
        let mut s = WindowStack::new();
        let here = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let gone = win(&mut s, Role::Normal, 0, 0, 200, 200);
        s.set_window_desktop(gone, 2).unwrap();
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);

        let out = go(&mut r, &mut s, button(true));
        assert!(
            out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == here)),
            "the click went to the window that is actually on screen"
        );
        assert!(
            !out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == gone)),
            "and not to the one on desktop 2"
        );
    }

    #[test]
    fn switching_desktops_mid_drag_takes_the_grab_away_from_the_hidden_window() {
        // The half a filter in `hit()` cannot reach: after a press every event bypasses
        // hit-testing.
        let mut s = WindowStack::new();
        let dragged = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        let out = go(&mut r, &mut s, button(true));
        assert!(
            out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == dragged)),
            "precondition: the press landed and grabbed"
        );

        s.set_current_desktop(2).unwrap();

        let out = go(&mut r, &mut s, drag(10, 10));
        let to_dragged: Vec<_> = out
            .iter()
            .filter_map(|o| match o {
                Outbound::Pointer { event } if event.window == dragged => Some(event.kind),
                _ => None,
            })
            .collect();
        assert_eq!(
            to_dragged,
            alloc::vec![POINTER_BUTTON, POINTER_LEAVE],
            "the closing release then the leave, in that order and nothing else"
        );
        assert!(
            out.iter().any(|o| matches!(
                o,
                Outbound::Pointer { event }
                    if event.window == dragged && event.kind == POINTER_BUTTON
                        && event.flags & POINTER_PRESSED == 0
            )),
            "the synthetic button record is a release, not a press"
        );

        let out = go(&mut r, &mut s, drag(5, 5));
        assert!(out.is_empty(), "no records at all while a broken grab's buttons are down");
        let out = go(&mut r, &mut s, button(false));
        assert!(
            !out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == dragged)),
            "the real release does not reach it either -- it already got its close"
        );
    }

    #[test]
    fn minimizing_the_grab_holder_takes_the_grab_away_too() {
        let mut s = WindowStack::new();
        let dragged = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        go(&mut r, &mut s, button(true));

        s.set_minimized(dragged, true).unwrap();

        let out = go(&mut r, &mut s, drag(5, 5));
        let kinds: Vec<_> = out
            .iter()
            .filter_map(|o| match o {
                Outbound::Pointer { event } if event.window == dragged => Some(event.kind),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, alloc::vec![POINTER_BUTTON, POINTER_LEAVE], "closed, then left");
        assert!(
            !out.iter().any(|o| matches!(
                o,
                Outbound::Pointer { event }
                    if event.window == dragged && event.kind == POINTER_MOTION
            )),
            "a minimized window keeps receiving the drag it grabbed"
        );
    }

    #[test]
    fn minimizing_the_window_under_the_cursor_pairs_a_leave_with_the_enter() {
        let mut s = WindowStack::new();
        let under = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let over = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        assert_eq!(r.inside, Some(over), "precondition: the cursor is in the top window");

        s.set_minimized(over, true).unwrap();
        let out = go(&mut r, &mut s, motion(1, 1));

        let leave = out.iter().any(|o| {
            matches!(
            o, Outbound::Pointer { event } if event.window == over && event.kind == POINTER_LEAVE)
        });
        let enter = out.iter().any(|o| {
            matches!(
            o, Outbound::Pointer { event } if event.window == under && event.kind == POINTER_ENTER)
        });
        assert!(leave, "the window that lost the cursor is owed a leave");
        assert!(enter, "and the one that gained it an enter");
    }

    #[test]
    fn breaking_a_grab_gives_the_release_to_nobody_else() {
        let mut s = WindowStack::new();
        let under = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let over = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        go(&mut r, &mut s, button(true));

        s.set_minimized(over, true).unwrap();
        let out = go(&mut r, &mut s, button(false));
        assert!(
            !out.iter().any(|o| matches!(
                o, Outbound::Pointer { event } if event.window == under
                    && event.kind == POINTER_BUTTON)),
            "the window underneath must not receive a release for a press it never saw"
        );
        assert!(
            out.iter().any(|o| matches!(
                o, Outbound::Pointer { event } if event.window == under
                    && event.kind == POINTER_ENTER)),
            "and once the sequence is over it is entered normally"
        );

        let out = go(&mut r, &mut s, button(true));
        assert!(
            out.iter().any(|o| matches!(
                o, Outbound::Pointer { event } if event.window == under
                    && event.kind == POINTER_BUTTON)),
            "input resumes after the broken grab's buttons come up"
        );
    }

    #[test]
    fn the_records_a_broken_grab_emits_carry_the_provoking_events_modifiers() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        go(
            &mut r,
            &mut s,
            Logical::Button { button: BTN_LEFT, pressed: true, buttons: 1, modifiers: MOD_SHIFT, time_ns: T },
        );

        s.set_minimized(w, true).unwrap();
        let out = go(
            &mut r,
            &mut s,
            Logical::Button { button: BTN_LEFT, pressed: false, buttons: 0, modifiers: 0, time_ns: T },
        );
        for o in &out {
            let Outbound::Pointer { event } = o else { continue };
            assert_eq!(event.modifiers, 0, "kind {} carries a stale modifier", event.kind);
            assert_eq!(event.buttons, 0, "kind {} carries a stale button mask", event.kind);
        }
        assert!(!out.is_empty(), "the window is owed its close, so there is something to check");
    }

    #[test]
    fn a_sticky_window_is_clickable_on_every_desktop() {
        let mut s = WindowStack::new();
        let bar = win(&mut s, Role::Normal, 0, 0, 200, 200);
        s.set_window_desktop(bar, librsproto::surface::STICKY_DESKTOP).unwrap();
        s.set_current_desktop(7).unwrap();
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);

        let out = go(&mut r, &mut s, button(true));
        assert!(
            out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == bar)),
            "a sticky window takes the click on desktop 7"
        );
    }

    #[test]
    fn a_press_outside_a_popup_dismisses_it_and_one_inside_does_not() {
        let mut s = WindowStack::new();
        let bar = win(&mut s, Role::Panel { dock: Edge::Top, reserve: 24 }, 0, 0, 640, 24);
        let menu = win(&mut s, Role::Popup { parent: bar }, 0, 24, 120, 200);
        let mut r = InputRouter::new(SCREEN);
        assert_eq!(s.focus_candidate(), Some(menu), "the popup is topmost and takes focus");

        warp(&mut r, &mut s, 400, 10);
        let mut out = Vec::new();
        let routed = r.route(&button(true), &mut s, &mut out);
        assert_eq!(routed.dismissed, Some(menu), "a press on a panel did not dismiss the popup");
        assert_eq!(routed.restacked, None, "a panel raise would be the other mechanism");

        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 40, 100);
        let routed = r.route(&button(true), &mut s, &mut out);
        assert_eq!(routed.dismissed, None, "pressing the popup dismissed it");

        // Its parent counts as outside, so clicking the opening button again closes the menu.
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 300, 10);
        let routed = r.route(&button(true), &mut s, &mut out);
        assert_eq!(routed.dismissed, Some(menu), "pressing the popup's parent did not dismiss it");
    }

    #[test]
    fn clicking_a_window_raises_it_and_that_is_the_focus_change() {
        let mut s = WindowStack::new();
        let lower = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let upper = win(&mut s, Role::Normal, 300, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        assert_eq!(s.focus_candidate(), Some(upper));

        warp(&mut r, &mut s, 50, 50);
        let mut out = Vec::new();
        let routed = r.route(&button(true), &mut s, &mut out);
        // `painted_bounds`, not a literal: the raised window's shadow grows the region.
        let want = s.window(lower).expect("in the stack").painted_bounds();
        assert!(
            want.origin.x < 0 && want.size.w > 100,
            "the premise: a raised window's damage includes its shadow, got {want:?}"
        );
        assert_eq!(routed.restacked, Some(want), "the caller must recompose, and only where the raised window is");
        assert_eq!(s.focus_candidate(), Some(lower), "the raise *is* the focus change");

        let again = r.route(&button(false), &mut s, &mut out);
        assert_eq!(again.restacked, None);
        let again = r.route(&button(true), &mut s, &mut out);
        assert_eq!(again.restacked, None, "already topmost: nothing moved, nothing to paint");
    }

    #[test]
    fn an_interactive_move_offsets_by_where_the_press_landed_not_by_where_the_request_arrived() {
        // An origin taken at the request rather than the press would jump by the pointer's travel.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);

        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        go(&mut r, &mut s, drag(40, 0));
        r.start_move(w, &mut s).expect("the grab is on that window");

        go(&mut r, &mut s, drag(10, 5));
        let origin = s.window(w).expect("still there").origin;
        assert_eq!(
            origin,
            Point::new(150, 105),
            "the window jumped by the distance the pointer travelled before the request"
        );
    }

    #[test]
    fn a_second_start_move_in_one_gesture_changes_nothing() {
        // A second `StartMove` must not rebuild the drag: the origin moved, the press did not.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        r.start_move(w, &mut s).expect("grabbed");
        go(&mut r, &mut s, drag(40, 20));
        let after_first = s.window(w).expect("there").origin;

        r.start_move(w, &mut s).expect("still grabbed");
        assert_eq!(s.window(w).expect("there").origin, after_first, "the second request moved the window again");
    }

    #[test]
    fn a_drag_ends_when_a_dropped_batch_takes_its_grab_away() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        r.start_move(w, &mut s).expect("grabbed");

        go(&mut r, &mut s, Logical::Dropped { time_ns: T });
        let at = s.window(w).expect("there").origin;
        assert_eq!(s.dragging(), None, "the drag went with the grab");
        go(&mut r, &mut s, motion(30, 30));
        assert_eq!(s.window(w).expect("there").origin, at, "and the window stopped following");
        assert!(s.place(w, Point::new(0, 0)).is_ok(), "and a manager may place it again");
    }

    #[test]
    fn a_drag_ends_when_its_window_leaves_the_screen() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        r.start_move(w, &mut s).expect("grabbed");

        s.set_minimized(w, true).expect("minimize");
        go(&mut r, &mut s, drag(30, 30));
        assert_eq!(s.dragging(), None, "the drag went with the grab");
        let at = s.window(w).expect("there").origin;
        go(&mut r, &mut s, drag(30, 30));
        assert_eq!(s.window(w).expect("there").origin, at, "and it stopped following");
    }

    #[test]
    fn a_drag_that_moved_nothing_reports_no_geometry_change() {
        // A click on a title bar is a drag of zero pixels; the manager's queue does not coalesce.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        let _ = s.take_geometry_changes();

        go(&mut r, &mut s, button(true));
        r.start_move(w, &mut s).expect("grabbed");
        go(&mut r, &mut s, button(false));
        assert!(s.take_geometry_changes().is_empty(), "the window never moved");
    }

    #[test]
    fn the_catch_up_at_the_start_of_a_drag_reports_its_damage() {
        // A press-flick-release has no motion event to report the damage, so `StartMove` must.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        go(&mut r, &mut s, drag(40, 0));

        let damage = r.start_move(w, &mut s).expect("grabbed").expect("it moved, so it damaged");
        assert!(!damage.is_empty(), "a catch-up that moved the window damaged nothing: {damage:?}");
        assert_eq!(s.window(w).expect("there").origin, Point::new(140, 100));
    }

    #[test]
    fn a_move_needs_the_grab_on_that_window() {
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 300, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);

        assert!(r.start_move(a, &mut s).is_err(), "no button is down at all");

        warp(&mut r, &mut s, 350, 50);
        go(&mut r, &mut s, button(true));
        assert!(r.start_move(a, &mut s).is_err(), "the grab is on the other window");
        assert!(r.start_move(b, &mut s).is_ok(), "and this is the one being pressed");
    }

    fn go_routed(r: &mut InputRouter, s: &mut WindowStack, ev: Logical) -> Routed {
        let mut out = Vec::new();
        r.route(&ev, s, &mut out)
    }

    fn grip(r: &mut InputRouter, s: &mut WindowStack, w: u32, x: i32, y: i32, edges: u32) {
        warp(r, s, x, y);
        go(r, s, button(true));
        r.start_resize(w, edges, s).expect("the grab is on that window");
    }

    fn zone(id: u32, t: (i32, i32, u32, u32), g: (i32, i32, u32, u32)) -> MgrSnapZone {
        MgrSnapZone {
            id,
            trigger_x: t.0,
            trigger_y: t.1,
            trigger_w: t.2,
            trigger_h: t.3,
            target_x: g.0,
            target_y: g.1,
            target_w: g.2,
            target_h: g.3,
        }
    }

    fn hold(r: &mut InputRouter, s: &mut WindowStack, w: u32, x: i32, y: i32) {
        warp(r, s, x, y);
        go(r, s, button(true));
        r.start_move(w, s).expect("the grab is on that window");
    }

    #[test]
    fn a_move_into_a_zone_previews_its_target_and_a_drop_asks_for_it() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
        let mut r = InputRouter::new(SCREEN);
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
        hold(&mut r, &mut s, w, 240, 240);

        let routed = go_routed(&mut r, &mut s, drag(-100, 0));
        assert!(routed.outline.is_none(), "nothing is previewed away from a zone");
        assert!(routed.moved.is_some(), "and the move is unaffected");

        let routed = go_routed(&mut r, &mut s, drag(-130, 0));
        let o = routed.outline.expect("the zone is previewed");
        assert_eq!(o.now, Some(Rect::new(0, 0, 320, 480)), "the target, not the trigger");
        assert!(routed.resized.is_none(), "nothing is asked for until the button comes up");

        let routed = go_routed(&mut r, &mut s, button(false));
        assert_eq!(
            routed.resized,
            Some((w, Rect::new(0, 0, 320, 480))),
            "the drop asks for exactly what was previewed"
        );
        assert_eq!(routed.outline.expect("taken down").now, None);
    }

    #[test]
    fn a_drag_that_passes_through_a_zone_without_stopping_snaps_nothing() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
        let mut r = InputRouter::new(SCREEN);
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
        hold(&mut r, &mut s, w, 240, 240);

        go(&mut r, &mut s, drag(-230, 0)); // in
        let out = go_routed(&mut r, &mut s, drag(200, 0)); // and out again
        assert_eq!(out.outline.expect("the preview goes with the pointer").now, None);

        let routed = go_routed(&mut r, &mut s, button(false));
        assert!(routed.resized.is_none(), "a drag through a zone asks for nothing");
    }

    #[test]
    fn a_zone_id_registers_once_and_is_replaced_rather_than_refused() {
        let mut r = InputRouter::new(SCREEN);
        assert_eq!(r.register_zone(zone(0, (0, 0, 1, 1), (0, 0, 1, 1))), Err(HotkeyError::ZeroId));
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
        r.register_zone(zone(1, (0, 0, 20, 240), (0, 0, 320, 240))).expect("replaced");
        assert_eq!(
            r.zone_at(Point::new(5, 300)),
            None,
            "the replacement's trigger is what matches, not the original's"
        );
        assert_eq!(r.zone_at(Point::new(5, 100)).map(|z| z.target_h), Some(240));

        for id in 2..=MAX_SNAP_ZONES as u32 {
            r.register_zone(zone(id, (0, 0, 1, 1), (0, 0, 1, 1))).unwrap();
        }
        assert_eq!(
            r.register_zone(zone(99, (0, 0, 1, 1), (0, 0, 1, 1))),
            Err(HotkeyError::TableFull),
            "a new id past the bound is refused"
        );
        r.register_zone(zone(1, (0, 0, 2, 2), (0, 0, 2, 2))).expect("a replacement still fits");
    }

    #[test]
    fn zones_go_with_the_manager_that_registered_them_and_so_does_the_preview() {
        let mut r = InputRouter::new(SCREEN);
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
        assert!(r.zone_at(Point::new(5, 5)).is_some());
        assert_eq!(r.clear_zones(), None, "nothing was being previewed");
        assert!(r.zone_at(Point::new(5, 5)).is_none());

        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
        let mut r = InputRouter::new(SCREEN);
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
        hold(&mut r, &mut s, w, 240, 240);
        let shown = go_routed(&mut r, &mut s, drag(-230, 0)).outline.expect("previewed");
        assert_eq!(shown.now, Some(Rect::new(0, 0, 320, 480)));

        let taken = r.clear_zones().expect("the preview is handed back to be repainted");
        assert_eq!(taken.was, Some(Rect::new(0, 0, 320, 480)), "the rectangle to erase");
        assert_eq!(taken.now, None);
        assert!(go_routed(&mut r, &mut s, drag(5, 0)).outline.is_none());
        assert!(go_routed(&mut r, &mut s, button(false)).outline.is_none());
    }

    #[test]
    fn re_registering_the_zone_under_the_pointer_moves_the_preview_with_it() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
        let mut r = InputRouter::new(SCREEN);
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
        hold(&mut r, &mut s, w, 240, 240);
        go(&mut r, &mut s, drag(-230, 0));

        let moved = r
            .register_zone(zone(1, (0, 0, 20, 480), (0, 24, 320, 456)))
            .expect("replaced")
            .expect("the preview moved with it");
        assert_eq!(moved.was, Some(Rect::new(0, 0, 320, 480)));
        assert_eq!(moved.now, Some(Rect::new(0, 24, 320, 456)));
        assert_eq!(
            go_routed(&mut r, &mut s, button(false)).resized,
            Some((w, Rect::new(0, 24, 320, 456))),
            "and the drop asks for the rectangle now on screen"
        );
    }

    #[test]
    fn a_drag_the_user_did_not_finish_snaps_nothing() {
        for interrupted_by_loss in [true, false] {
            let mut s = WindowStack::new();
            let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
            let mut r = InputRouter::new(SCREEN);
            r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
            hold(&mut r, &mut s, w, 240, 240);
            go(&mut r, &mut s, drag(-230, 0));

            let routed = if interrupted_by_loss {
                go_routed(&mut r, &mut s, Logical::Dropped { time_ns: T })
            } else {
                s.set_minimized(w, true).expect("put away mid-drag");
                go_routed(&mut r, &mut s, key(30, true))
            };
            assert!(routed.resized.is_none(), "loss={interrupted_by_loss}: an unfinished drag asked for a snap");
            assert_eq!(routed.outline.expect("the preview is taken down").now, None, "loss={interrupted_by_loss}");
        }
    }

    #[test]
    fn a_gesture_whose_window_has_gone_asks_for_nothing_even_when_finished() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
        let mut r = InputRouter::new(SCREEN);
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
        hold(&mut r, &mut s, w, 240, 240);
        go(&mut r, &mut s, drag(-230, 0));
        s.destroy(w).expect("the client exited mid-drag");
        assert_eq!(r.stop_drag(&mut s, true).0, None, "a snap for a window that is not there");

        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
        go(&mut r, &mut s, drag(40, 30));
        s.set_minimized(w, true).expect("put away mid-gesture");
        assert_eq!(r.stop_resize(&mut s, true).0, None, "a resize for a window off screen");
    }

    #[test]
    fn a_zone_previews_only_during_a_move() {
        // The approach must happen inside the assertion: `warp` is itself a motion.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
        let mut r = InputRouter::new(SCREEN);
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();

        warp(&mut r, &mut s, 200, 100);
        assert!(
            go_routed(&mut r, &mut s, motion(-190, 0)).outline.is_none(),
            "the pointer crossed into a zone with nothing held, and something was previewed"
        );

        let _ = w;
        // A resize is not asserted: `preview_zone` sits behind `outline_to_pointer`, so such a test
        // passes for wrong code too. `start_move` and `start_resize` refusing each other stops it.
    }

    #[test]
    fn a_resize_needs_the_grab_and_a_pair_of_edges_that_name_a_gesture() {
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let b = win(&mut s, Role::Normal, 300, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        assert!(r.start_resize(a, RESIZE_RIGHT, &mut s).is_err(), "no button is down");

        warp(&mut r, &mut s, 350, 50);
        go(&mut r, &mut s, button(true));
        assert!(r.start_resize(a, RESIZE_RIGHT, &mut s).is_err(), "the grab is elsewhere");

        for edges in [0, RESIZE_LEFT | RESIZE_RIGHT, RESIZE_TOP | RESIZE_BOTTOM, 1 << 9] {
            assert_eq!(r.start_resize(b, edges, &mut s), Err(StackError::BadGeometry), "edges {edges:#x}");
        }
        assert!(r.start_resize(b, RESIZE_RIGHT | RESIZE_BOTTOM, &mut s).is_ok());
    }

    #[test]
    fn the_outline_follows_the_pointer_and_the_window_does_not_move() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);

        let routed = go_routed(&mut r, &mut s, drag(40, 30));
        let o = routed.outline.expect("the outline moved");
        assert_eq!(o.now, Some(Rect::new(100, 100, 240, 130)), "both edges followed");
        assert_eq!(o.was, Some(Rect::new(100, 100, 200, 100)), "and the old one is repainted");
        assert_eq!(
            s.window(w).unwrap().bounds(),
            Rect::new(100, 100, 200, 100),
            "the window itself has not changed at all"
        );
        assert!(routed.moved.is_none(), "and nothing was moved");
        assert!(routed.resized.is_none(), "nothing is reported until the button comes up");
    }

    #[test]
    fn dragging_a_top_or_left_edge_moves_the_origin_rather_than_only_the_size() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 100, 100, RESIZE_LEFT | RESIZE_TOP);

        let o = go_routed(&mut r, &mut s, drag(30, 20)).outline.expect("moved");
        assert_eq!(o.now, Some(Rect::new(130, 120, 170, 80)));
    }

    #[test]
    fn an_edge_dragged_past_its_opposite_stops_rather_than_inverting() {
        // The floor is the compositor's own: the protocol cannot state a client's minimum.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);

        let o = go_routed(&mut r, &mut s, drag(-400, -400)).outline.expect("moved");
        assert_eq!(o.now, Some(Rect::new(100, 100, MIN_RESIZE, MIN_RESIZE)));

        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 100, 100, RESIZE_LEFT | RESIZE_TOP);
        let o = go_routed(&mut r, &mut s, drag(400, 400)).outline.expect("moved");
        let now = o.now.unwrap();
        assert_eq!(now.size, libdraw::geom::Size::new(MIN_RESIZE, MIN_RESIZE));
        assert_eq!(now.right(), 300, "the edge that was not dragged did not move");
        assert_eq!(now.bottom(), 200);
    }

    #[test]
    fn the_floor_holds_only_the_axis_being_dragged() {
        // Dragging the bottom edge alone must not widen a window already narrower than the floor.
        let narrow = MIN_RESIZE / 2;
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 10, 10, narrow, 200);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 10 + narrow as i32 - 1, 209, RESIZE_BOTTOM);
        let o = go_routed(&mut r, &mut s, drag(0, -20)).outline.expect("moved");
        assert_eq!(o.now, Some(Rect::new(10, 10, narrow, 180)), "the width is untouched");
    }

    #[test]
    fn the_release_reports_the_rectangle_once_and_takes_the_outline_with_it() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
        go(&mut r, &mut s, drag(40, 30));

        let routed = go_routed(&mut r, &mut s, button(false));
        assert_eq!(
            routed.resized,
            Some((w, Rect::new(100, 100, 240, 130))),
            "one report, carrying where the user let go"
        );
        let o = routed.outline.expect("the outline is taken down");
        assert_eq!(o.now, None);
        assert_eq!(
            s.window(w).unwrap().bounds(),
            Rect::new(100, 100, 200, 100),
            "and the compositor still has not resized the client"
        );
        assert_eq!(go_routed(&mut r, &mut s, button(false)).resized, None);
    }

    #[test]
    fn a_grip_click_that_moves_nothing_reports_nothing() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
        assert_eq!(go_routed(&mut r, &mut s, button(false)).resized, None);
    }

    #[test]
    fn a_resize_and_a_move_are_one_grab_and_cannot_both_run() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        r.start_move(w, &mut s).expect("the move takes the grab");
        assert!(r.start_resize(w, RESIZE_RIGHT, &mut s).is_err(), "one gesture per grab");

        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
        assert!(r.start_move(w, &mut s).is_err(), "one gesture per grab, either order");
        go(&mut r, &mut s, drag(40, 30));
        assert_eq!(
            s.window(w).unwrap().origin,
            Point::new(100, 100),
            "and the window did not move while its outline did"
        );

        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT);
        go(&mut r, &mut s, drag(40, 0));
        assert_eq!(r.start_resize(w, RESIZE_RIGHT, &mut s), Ok(Some(Rect::new(100, 100, 240, 100))));
        assert_eq!(
            go_routed(&mut r, &mut s, button(false)).resized,
            Some((w, Rect::new(100, 100, 240, 100))),
            "the second request named the drag already running"
        );
    }

    #[test]
    fn a_resize_refuses_a_place_and_a_dropped_batch_ends_it_without_committing() {
        // A resize marks the window dragged so a `Place` is refused. `Dropped` ends it but reports
        // nothing: a guessed pointer position must not initiate a change.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
        assert_eq!(s.place(w, Point::new(0, 0)), Err(StackError::Dragging));
        go(&mut r, &mut s, drag(40, 30));

        let routed = go_routed(&mut r, &mut s, Logical::Dropped { time_ns: T });
        assert_eq!(routed.resized, None, "a gesture the user did not finish asks for nothing");
        assert_eq!(routed.outline.expect("taken down").now, None);
        assert!(s.place(w, Point::new(0, 0)).is_ok(), "and the window is a manager's again");
    }

    #[test]
    fn a_window_put_away_mid_resize_ends_the_gesture_on_whichever_key_arm_notices() {
        // `reconcile_with` breaks a grab on any event, so a gesture can end on a key arm; all three
        // are exercised, the hotkey one by releasing the grip after a chord put the window away.
        for arm in ["nothing focusable", "delivered", "hotkey"] {
            let mut s = WindowStack::new();
            let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
            if arm != "nothing focusable" {
                win(&mut s, Role::Normal, 400, 0, 100, 100);
            }
            let mut r = InputRouter::new(SCREEN);
            r.register_hotkey(MgrHotkey { id: 9, mods: MOD_SUPER, code: KEY_2 }).unwrap();
            grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
            go(&mut r, &mut s, drag(40, 30));
            if arm == "hotkey" {
                let held = go_routed(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
                assert!(held.consumed && held.outline.is_none(), "{arm}: nothing torn down yet");
            }

            s.set_minimized(w, true).expect("put away under the pointer");
            let ev = match arm {
                "hotkey" => chord(KEY_2, false, MOD_SUPER),
                _ => key(30, true),
            };
            let routed = go_routed(&mut r, &mut s, ev);
            assert_eq!(
                routed.outline.expect("the outline is taken down").now,
                None,
                "{arm}: the outline outlived the gesture"
            );
            assert!(
                routed.resized.is_none(),
                "{arm}: nothing is asked of the shell for a window that is not on screen"
            );
        }
    }

    #[test]
    fn a_drag_refuses_a_place_and_releases_it_on_the_button_coming_up() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        r.start_move(w, &mut s).expect("grabbed");

        assert_eq!(s.place(w, Point::new(0, 0)), Err(StackError::Dragging));
        go(&mut r, &mut s, button(false));
        assert!(s.place(w, Point::new(0, 0)).is_ok(), "the drag ended with the button");
    }

    #[test]
    fn a_drag_produces_one_geometry_record_however_far_it_moves() {
        // The manager's queue does not coalesce and evicts its oldest, so a record per motion
        // would push out `WindowCreated`.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        let _ = s.take_geometry_changes();

        go(&mut r, &mut s, button(true));
        r.start_move(w, &mut s).expect("grabbed");
        for _ in 0..25 {
            go(&mut r, &mut s, drag(3, 2));
        }
        assert!(s.take_geometry_changes().is_empty(), "a drag in flight reports nothing");

        go(&mut r, &mut s, button(false));
        assert_eq!(s.take_geometry_changes(), vec![w], "and exactly one record when it ends");
    }

    #[test]
    fn a_window_destroyed_mid_drag_ends_the_drag_rather_than_wedging_place() {
        // A client exiting with the button held is ordinary; ids are never reused, so a leftover
        // flag would last.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        r.start_move(w, &mut s).expect("grabbed");

        s.destroy(w).expect("in the stack");
        assert_eq!(s.dragging(), None, "the drag went with the window");
        go(&mut r, &mut s, drag(10, 10)); // must not panic, and nothing to move
    }

    #[test]
    fn clicking_a_panel_does_not_raise_it_over_the_window_it_docks_beside() {
        let mut s = WindowStack::new();
        let panel = win(&mut s, Role::Panel { dock: Edge::Top, reserve: 100 }, 0, 0, 640, 100);
        let normal = win(&mut s, Role::Normal, 0, 100, 640, 380);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 320, 50);

        let mut out = Vec::new();
        assert_eq!(r.route(&button(true), &mut s, &mut out).restacked, None, "no restack");
        assert_eq!(s.windows().last().map(|w| w.id), Some(normal));
        assert_eq!(s.windows().first().map(|w| w.id), Some(panel));
    }

    #[test]
    fn crossing_a_border_leaves_one_window_before_entering_the_next() {
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);

        warp(&mut r, &mut s, 50, 50);
        let out = go(&mut r, &mut s, motion(200, 0));
        let kinds: Vec<_> = out
            .iter()
            .map(|o| match o {
                Outbound::Pointer { event } => (event.window, event.kind),
                other => unreachable!("the router emits no {other:?} here"),
            })
            .collect();
        assert_eq!(
            kinds,
            [(a, POINTER_LEAVE), (b, POINTER_ENTER), (b, POINTER_MOTION)],
            "leave, then enter, then the motion that caused both"
        );
    }

    #[test]
    fn every_record_carries_the_buttons_and_modifiers_held() {
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);

        let out = go(
            &mut r,
            &mut s,
            Logical::Button { button: BTN_LEFT, pressed: true, buttons: 1, modifiers: MOD_SHIFT, time_ns: T },
        );
        for o in &out {
            let Outbound::Pointer { event, .. } = o else { unreachable!() };
            assert_eq!((event.buttons, event.modifiers), (1, MOD_SHIFT), "kind {}", event.kind);
        }

        let out = go(&mut r, &mut s, Logical::Motion { dx: 200, dy: 0, buttons: 1, modifiers: MOD_SHIFT, time_ns: T });
        let Outbound::Pointer { event, .. } = out
            .iter()
            .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION))
            .cloned()
            .expect("a motion")
        else {
            unreachable!()
        };
        assert_eq!((event.buttons, event.modifiers), (1, MOD_SHIFT), "a shift-drag is expressible from one record");

        let out = go(
            &mut r,
            &mut s,
            Logical::Button { button: BTN_LEFT, pressed: false, buttons: 0, modifiers: MOD_SHIFT, time_ns: T },
        );
        let entered = out
            .iter()
            .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_ENTER))
            .expect("an enter");
        assert_eq!(entered.window(), b);
        let Outbound::Pointer { event, .. } = entered else { unreachable!() };
        assert_eq!((event.buttons, event.modifiers), (0, MOD_SHIFT));
    }

    #[test]
    fn the_first_click_after_boot_enters_the_window_before_the_press() {
        // Once the press takes the grab the crossing pass early-returns, so the enter must come
        // first.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 0, 0, 640, 480);
        let mut r = InputRouter::new(SCREEN);
        assert_eq!(r.inside(), None, "no motion since boot");

        let out = go(&mut r, &mut s, button(true));
        let kinds: Vec<_> = out
            .iter()
            .map(|o| match o {
                Outbound::Pointer { event } => (event.window, event.kind),
                other => unreachable!("the router emits no {other:?} here"),
            })
            .collect();
        assert_eq!(kinds, [(w, POINTER_ENTER), (w, POINTER_BUTTON)], "entered, then clicked");
    }

    #[test]
    fn coordinates_are_window_local() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 200, 100, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 250, 130);

        let out = go(&mut r, &mut s, motion(5, 5));
        let Outbound::Pointer { event } = out
            .iter()
            .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION))
            .cloned()
            .expect("a motion")
        else {
            unreachable!()
        };
        assert_eq!(event.window, w, "the record names the window it was routed to");
        assert_eq!((event.x, event.y), (55, 35), "screen (255,135) minus origin (200,100)");
    }

    #[test]
    fn a_drag_out_of_a_window_still_delivers_the_release_to_it() {
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);

        go(&mut r, &mut s, button(true));
        assert_eq!(r.grab(), Some(a));
        let moved = go(&mut r, &mut s, drag(200, 0));
        assert!(
            s.window(b).expect("b").bounds().contains(r.pointer().x, r.pointer().y),
            "the cursor really did reach b — so the grab is what redirects, not geometry"
        );
        assert!(moved.iter().all(|o| o.window() == a), "everything mid-drag still goes to the grab holder");

        let out = go(&mut r, &mut s, button(false));
        let released = out
            .iter()
            .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_BUTTON))
            .expect("a button record");
        assert_eq!(released.window(), a, "the release went to the window that got the press");
        assert_eq!(r.grab(), None);
    }

    #[test]
    fn window_local_x_goes_negative_during_a_drag() {
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 200, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 250, 50);

        go(&mut r, &mut s, button(true));
        let out = go(&mut r, &mut s, drag(-100, 0));
        let Outbound::Pointer { event, .. } = out
            .iter()
            .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION))
            .cloned()
            .expect("a motion")
        else {
            unreachable!()
        };
        assert_eq!(event.x, -50, "50px left of the window's left edge");
    }

    #[test]
    fn the_release_re_derives_the_crossing_the_drag_suppressed() {
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        go(&mut r, &mut s, button(true));
        go(&mut r, &mut s, drag(200, 0));
        assert_eq!(r.inside(), Some(a), "suppressed while grabbed: still a, though over b");

        let out = go(&mut r, &mut s, button(false));
        assert_eq!(r.inside(), Some(b));
        let crossings: Vec<_> = out
            .iter()
            .filter_map(|o| match o {
                Outbound::Pointer { event } if event.kind == POINTER_LEAVE || event.kind == POINTER_ENTER => {
                    Some((event.window, event.kind))
                }
                _ => None,
            })
            .collect();
        assert_eq!(crossings, [(a, POINTER_LEAVE), (b, POINTER_ENTER)]);
    }

    #[test]
    fn a_dropped_batch_ends_the_grab() {
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        go(&mut r, &mut s, button(true));
        go(&mut r, &mut s, drag(200, 0));
        assert_eq!(r.grab(), Some(a));

        go(&mut r, &mut s, Logical::Dropped { time_ns: T });
        assert_eq!(r.grab(), None);
        let out = go(&mut r, &mut s, motion(1, 0));
        assert!(out.iter().all(|o| o.window() == b), "later events follow the cursor again");
    }

    #[test]
    fn destroying_the_grab_holder_does_not_route_to_a_dead_id() {
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        go(&mut r, &mut s, button(true));
        assert_eq!(r.grab(), Some(a));

        s.destroy(a).expect("destroy");
        let out = go(&mut r, &mut s, drag(200, 0));
        assert!(out.iter().all(|o| o.window() == b), "nothing addressed to the dead window");
        assert_eq!(r.grab(), None);
    }

    #[test]
    fn the_cursor_stops_at_the_screen_edge() {
        let mut s = WindowStack::new();
        let mut r = InputRouter::new(SCREEN);
        go(&mut r, &mut s, motion(-10_000, -10_000));
        assert_eq!(r.pointer(), Point::new(0, 0));
        go(&mut r, &mut s, motion(10_000, 10_000));
        assert_eq!(r.pointer(), Point::new(639, 479), "the last addressable pixel, not 640");
    }

    #[test]
    fn a_mouse_held_against_the_edge_does_not_wrap_to_the_other_side() {
        // Rightward can overflow: `639 + i32::MAX` wraps negative and pins to the left edge.
        let mut s = WindowStack::new();
        let mut r = InputRouter::new(SCREEN);
        for _ in 0..3 {
            go(&mut r, &mut s, motion(i32::MAX, i32::MAX));
        }
        assert_eq!(r.pointer(), Point::new(639, 479), "still pinned to the far edge");
    }

    #[test]
    fn a_key_with_nothing_focusable_is_dropped_rather_than_sent_to_the_cursor() {
        let mut s = WindowStack::new();
        win(&mut s, Role::Panel { dock: Edge::Top, reserve: 100 }, 0, 0, 640, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 320, 50);
        assert!(go(&mut r, &mut s, key(30, true)).is_empty());
    }

    #[test]
    fn the_press_that_focuses_a_window_is_also_delivered_to_it() {
        let mut s = WindowStack::new();
        let lower = win(&mut s, Role::Normal, 0, 0, 640, 480);
        win(&mut s, Role::Normal, 300, 300, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);

        let out = go(&mut r, &mut s, button(true));
        assert!(out.iter().all(|o| o.window() == lower));
        assert_eq!(r.grab(), Some(lower));
    }

    fn wheel(dz: i32) -> Logical {
        Logical::Wheel { dz, buttons: 0, modifiers: 0, time_ns: T }
    }

    #[test]
    fn a_wheel_reaches_the_window_under_the_pointer() {
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 300, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 350, 50);

        let out = go(&mut r, &mut s, wheel(2));
        assert_eq!(out.len(), 1, "a wheel moves no cursor, so it crosses nothing");
        let Outbound::Pointer { event } = &out[0] else { panic!("a pointer record") };
        assert_eq!(event.window, b, "the window under the cursor, not {a}");
        assert_eq!(event.kind, POINTER_WHEEL);
        assert_eq!(event.wheel, 2);
        assert_eq!((event.x, event.y), (50, 50), "and window-local, like every other record");
    }

    #[test]
    fn a_wheel_neither_raises_nor_focuses() {
        let mut s = WindowStack::new();
        let lower = win(&mut s, Role::Normal, 0, 0, 640, 480);
        let upper = win(&mut s, Role::Normal, 300, 300, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        assert_eq!(s.focus_candidate(), Some(upper), "precondition: the small one is on top");

        let routed = go_routed(&mut r, &mut s, wheel(1));
        assert_eq!(routed.restacked, None, "scrolling reordered the screen");
        assert_eq!(s.focus_candidate(), Some(upper), "…and moved the keyboard with it");
        assert_eq!(r.grab(), None, "a wheel is not a press and opens no grab");
        let _ = lower;
    }

    #[test]
    fn a_wheel_during_a_drag_goes_to_the_grab_holder() {
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
        win(&mut s, Role::Normal, 300, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        go(&mut r, &mut s, button(true));
        go(&mut r, &mut s, drag(300, 0));
        assert_eq!(r.grab(), Some(a), "precondition: the press grabbed the first window");

        let out = go(&mut r, &mut s, wheel(-1));
        assert!(out.iter().all(|o| o.window() == a), "the sequence belongs to the grab holder");
    }

    #[test]
    fn a_wheel_carries_the_state_held_while_it_turned() {
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);

        let out = go(&mut r, &mut s, Logical::Wheel { dz: 1, buttons: 0, modifiers: MOD_SHIFT, time_ns: T });
        let Outbound::Pointer { event } = &out[0] else { panic!("a pointer record") };
        assert_eq!(event.modifiers, MOD_SHIFT);
        assert_eq!(event.time_ms, T_MS);
    }

    /// A delta too large for the wire saturates rather than wrapping.
    #[test]
    fn a_wheel_delta_too_large_for_the_wire_saturates() {
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);

        let out = go(&mut r, &mut s, wheel(i32::from(i16::MAX) + 10));
        let Outbound::Pointer { event } = &out[0] else { panic!("a pointer record") };
        assert_eq!(event.wheel, i16::MAX, "saturated, still downward");

        let out = go(&mut r, &mut s, wheel(i32::MIN));
        let Outbound::Pointer { event } = &out[0] else { panic!("a pointer record") };
        assert_eq!(event.wheel, i16::MIN);
    }

    /// Every record of a batch carries the interrupt's time, synthesised crossings included.
    #[test]
    fn every_record_carries_the_time_of_the_event_that_caused_it() {
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 300, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);

        let at = r.pointer();
        let out = go(&mut r, &mut s, motion(50 - at.x, 50 - at.y));
        let times: Vec<(u16, u64)> = out
            .iter()
            .filter_map(|o| match o {
                Outbound::Pointer { event } => Some((event.kind, event.time_ms)),
                _ => None,
            })
            .collect();
        assert!(times.iter().any(|(k, _)| *k == POINTER_ENTER), "the cursor entered {a}");
        assert!(
            times.iter().all(|&(_, t)| t == T_MS),
            "a record was stamped from somewhere other than the event: {times:?}"
        );

        let out = go(&mut r, &mut s, motion(300, 0));
        let leave = out
            .iter()
            .find_map(|o| match o {
                Outbound::Pointer { event } if event.kind == POINTER_LEAVE => Some(*event),
                _ => None,
            })
            .expect("left the first window");
        assert_eq!(leave.window, a);
        assert_eq!(leave.time_ms, T_MS);
        assert!(out.iter().any(|o| o.window() == b), "and entered the second");
    }
}
