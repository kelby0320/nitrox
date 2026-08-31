//! Where an input event goes: focus for keys, hit-testing for the pointer.
//!
//! `libinput` turns device triples into [`Logical`] events but deliberately stops there —
//! it has no screen to clamp a delta against and no window stack to hit-test. **This is the
//! half that needs both**, and like the rest of this crate it is a pure function of state
//! the caller owns, so a click landing on the wrong window is a host test rather than a
//! boot.
//!
//! ## The rules
//!
//! - **Keys follow focus**, which is the topmost window whose role takes it
//!   ([`WindowStack::focus_candidate`]). A panel never takes a keystroke: clicking the clock
//!   must not stop the terminal receiving what you type next.
//! - **The pointer follows the cursor**, hit-testing every role — a panel that cannot take
//!   keyboard focus can still be clicked, which is the entire point of a panel.
//! - **A press grabs.** Every pointer event up to the matching release goes to the window
//!   the press landed on, even after the cursor leaves it. Without this a drag that ends
//!   outside the window delivers a press with no release, and the client believes a button
//!   is held forever. The grab is why [`PointerEvent`]'s coordinates are **signed**: during
//!   one, window-local x can legitimately be negative.

use alloc::vec::Vec;

use libdraw::geom::{Point, Rect};
use libinput::Logical;
use librsproto::surface::{
    KeyEvent, MAX_HOTKEYS, MAX_SNAP_ZONES, MgrHotkey, MgrSnapZone, POINTER_BUTTON, POINTER_ENTER,
    POINTER_LEAVE, POINTER_MOTION, POINTER_PRESSED, PointerEvent, RESIZE_BOTTOM, RESIZE_LEFT,
    RESIZE_RIGHT, RESIZE_TOP, StartResize,
};

use crate::{Damage, StackError, WindowStack};
use crate::outbox::Outbound;

/// What routing one event did, beyond what it put in `out`.
///
/// **`consumed` exists because the caller cannot otherwise tell.** `route` used to answer only
/// "did the stack restack", so a chord press and an ordinary keystroke were indistinguishable
/// from outside — and the binary arms key repeat from the *physical* transition, deliberately,
/// so a consumed chord went on to repeat its key into the focused window at 25/s. Holding
/// `Super+1` while already on desktop 1 filled the terminal with `1`s: exactly the outcome
/// consumption exists to prevent, by the one path consumption could not see (PR #241 review,
/// blocking 1).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Routed {
    /// The region a restack disturbed, or `None` if the stack was not reordered.
    ///
    /// **The rectangle rather than a flag**, because the flag made the caller repaint the
    /// screen. A raise changes only the pixels the raised window covers (the argument is on
    /// [`WindowStack::raise`](crate::WindowStack::raise)), and a full recompose is ~100 ms under
    /// emulation with no input read during it — so every click that raised a window threw away
    /// the mouse movement around it (2026-08-26).
    pub restacked: Option<Rect>,
    /// The region an interactive move disturbed, or `None` if no window moved.
    ///
    /// Separate from [`restacked`](Self::restacked) because the two mean different things to the
    /// caller: a restack also changes who is focused and has to be announced, and a move does
    /// not. Both are repainted the same way.
    pub moved: Option<Rect>,
    /// The event was taken by a registered chord and reached no window.
    pub consumed: bool,
    /// The outline moved: where it was, and where it is now. See [`Outline`].
    ///
    /// Produced by both gestures — a resize's own rectangle, and the target a move is previewing
    /// over a snap zone.
    pub outline: Option<Outline>,
    /// A gesture ended, and this is the rectangle it asks for.
    ///
    /// **The gesture's one report**, from either that produces one: a resize (where the user let
    /// go) or a move released in a snap zone (that zone's target). The compositor does not apply
    /// it — changing a window's geometry is the manager's, so this becomes a `DragEnded` event
    /// and the manager answers with the `Configure` it would have sent anyway (M9 Parts E, F).
    pub resized: Option<(u32, Rect)>,
}

/// Where the resize outline was and where it is now, both to repaint.
///
/// **Both, because an outline is drawn over the composed stack** — like the cursor, and unlike a
/// window — so the region it leaves has to be recomposed to erase it. `None` on either side is a
/// gesture beginning or ending.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Outline {
    /// The rectangle the outline occupied before this event.
    pub was: Option<Rect>,
    /// The rectangle it occupies now.
    pub now: Option<Rect>,
}

/// An interactive move the compositor is running on a client's behalf.
///
/// **The window's origin is remembered, not recomputed.** Adding the pointer's delta to the
/// *current* origin each time would accumulate rounding and drift against the clamp; taking it
/// from where the window and the pointer both were when the grab was taken makes every motion an
/// absolute answer that cannot drift.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Drag {
    /// The window being moved.
    window: u32,
    /// Where the pointer was when the grab was taken.
    from: Point,
    /// Where the window's origin was then.
    origin: Point,
}

/// A drag-and-drop gesture the compositor is running on a client's behalf.
///
/// **The payload is a path and never a handle** (M10 decision 1): a handle would have to belong
/// to somebody while the gesture is in flight, and a transfer the receiver refuses has no clean
/// owner. A path is a name the receiving program opens for itself, and reports its own errors
/// about in its own window.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Carrying {
    /// The window the drag came out of — the one that must hold the grab.
    window: u32,
    /// What the payload is: exactly one of `DROP_KIND_FILE` and friends.
    kind: u32,
    /// What is being offered.
    path: alloc::string::String,
    /// What to call it on screen.
    name: alloc::string::String,
}

/// An interactive resize the compositor is running on a client's behalf.
///
/// **The window does not move and does not change size while this runs.** Only an outline does,
/// and the client is told nothing until the button comes up — decision 3 of Milestone 9: a
/// `Configure` per motion makes the client allocate, map, re-lay-out and repaint per motion,
/// which is the expensive path and is a client cost rather than a protocol one.
///
/// The starting rectangle is remembered for the same reason [`Drag`] remembers the origin:
/// deriving each step from the last accumulates against the clamp, and taking every step from
/// where the window and the pointer both were at the press makes each one an absolute answer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Resize {
    /// The window being resized.
    window: u32,
    /// Where the pointer was when the grab was taken.
    from: Point,
    /// The window's rectangle then.
    rect: Rect,
    /// Which edges are being dragged — a mask of `RESIZE_LEFT` and friends.
    edges: u32,
}

/// The smallest rectangle a resize will offer.
///
/// **A floor rather than a client's own minimum**, which the protocol has no way to state: a
/// `Configure` is a request, so a client that cannot be this small commits whatever it likes and
/// the compositor composites what it is given. What this prevents is the *outline* collapsing to
/// nothing or inverting as the pointer crosses the far edge, which is a drawing problem rather
/// than a policy one.
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

/// Whether a window the router still names is usable, hidden, or gone.
///
/// Three states rather than a boolean because the two failure cases need different treatment:
/// a hidden window can still be *told* it is losing the pointer, and a destroyed one cannot.
#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowState {
    /// In the stack and on screen.
    OnScreen,
    /// In the stack, but minimized or on another desktop — reachable, not visible.
    OffScreen,
    /// No longer in the stack.
    Gone,
}
use WindowState::{Gone, OffScreen, OnScreen};

/// Cursor position, crossing state, and the implicit grab.
///
/// Not part of [`WindowStack`]: the stack is what the screen looks like, and none of this
/// changes a pixel. Keeping it separate is also what lets the whole router be exercised
/// against a stack built by hand.
pub struct InputRouter {
    /// Where the cursor is, in screen coordinates, always inside `screen`.
    pointer: Point,
    /// The screen the pointer is clamped to.
    screen: Rect,
    /// The window the cursor is currently inside, if any — what makes enter/leave edges.
    inside: Option<u32>,
    /// The window holding the implicit grab, if a button is down.
    grab: Option<u32>,
    /// Where the pointer was when [`grab`](Self::grab) was taken.
    ///
    /// **Recorded at the press, not read when somebody asks.** An interactive move is requested
    /// by the client, which learns about the press, routes it through its own toolkit, decides
    /// it landed on a title bar, and only then sends `StartMove` — a full round trip later, by
    /// which time the pointer has moved, and by which time coalescing may have handed that
    /// client a position older still. A drag whose offset is measured at the request jumps by
    /// however far the pointer travelled in between, which is the same defect
    /// `TODO(scroll-grab)` describes for a scrollbar thumb (PR #247 review, finding 4).
    grab_at: Point,
    /// The interactive move in progress, if any.
    drag: Option<Drag>,
    /// The interactive resize in progress, if any.
    ///
    /// **Never both**: a grab is opened by one press and carries one gesture, so `start_move`
    /// and `start_resize` each refuse while the other is running rather than replacing it.
    resize: Option<Resize>,
    /// The drag-and-drop gesture in progress, if any — the third thing a grab can carry.
    ///
    /// **The payload lives here for the gesture's life**, because that is exactly how long it
    /// exists: a drag is an offer, and an offer that outlived the button would be a payload
    /// belonging to nobody. Cleared wherever the grab ends, like the other two.
    carrying: Option<Carrying>,
    /// The window a drop would land on right now — what the highlight is drawn around.
    ///
    /// Kept rather than recomputed at the release for the reason the snap zone's id is: the
    /// window the user let go over must be the one they were *shown*, and two hit tests of the
    /// same pointer are two chances to disagree.
    over: Option<u32>,
    /// The rectangle the outline is drawn at, if anything is drawing one.
    ///
    /// **Two gestures write it**, which is what makes `resize.is_none()` say nothing about it: a
    /// resize derives it from its edges and the pointer, and a move over a snap zone sets it to
    /// that zone's target. Kept rather than recomputed because the *previous* value is what a
    /// repaint needs in order to erase what was there — so every path that stops drawing has to
    /// hand it back rather than merely forget it.
    outline: Option<Rect>,
    /// Which button opened [`grab`](Self::grab) — the one a synthetic release names when the
    /// grab is taken away by something other than that button coming up.
    ///
    /// One code rather than a mask: a grab is opened by exactly one press (`grab.is_none()`
    /// gates it), so this is the button whose sequence the compositor promised to finish.
    /// A second button pressed during a drag never opened a grab and is not owed a close.
    grab_button: u16,
    /// A grab ended because its window left the screen, and the buttons are still down.
    ///
    /// **Until they come up, pointer input goes nowhere.** Without this, `target` falls
    /// through to `hit` and the window *underneath* receives the release for a press it never
    /// saw — and in `libui` a bare release fires nothing, so the defect is silent until the
    /// day a client acts on one. The sequence belonged to a window that can no longer receive
    /// it; handing its tail to a different window is not a repair (PR #240 review, blocking 1b).
    grab_broken: bool,
    /// Buttons held, mirrored from the last event that carried them.
    ///
    /// Stamped onto **every** record, not just `POINTER_BUTTON`. A drag is motion with a
    /// button held, so a client told `buttons == 0` on motion cannot implement one without
    /// re-accumulating the state itself — the duplication the Surface layer exists to
    /// avoid (PR #180 review, finding 2).
    buttons: u16,
    /// Modifiers held, mirrored the same way — what makes shift-click and shift-drag
    /// expressible on the wire rather than only in `libinput`.
    modifiers: u16,
    /// Keycodes whose press this router **delivered**, awaiting their release.
    ///
    /// **The other half of `consumed`, and it is what keeps presses and releases balanced.**
    /// A key already down cannot begin a chord: pressing `2`, then holding `Super`, makes the
    /// *repeat* of `2` match `Super+2` — so the chord would be recorded as consumed and the
    /// physical release swallowed, leaving the window with a press it never saw released
    /// (PR #241 review, finding 2). A press whose keycode is already here is a repeat of a
    /// delivered press, and is delivered.
    held: Vec<u16>,
    /// Chords a manager asked to receive instead of the focused window.
    ///
    /// Held here rather than in [`WindowStack`] because this is input *routing* policy and the
    /// stack is what the screen looks like. Bounded at [`MAX_HOTKEYS`].
    hotkeys: Vec<MgrHotkey>,
    /// Regions a manager asked to be told about, and what a window dropped in each becomes.
    ///
    /// Held here for the reason `hotkeys` is: matching a pointer against a table is routing, and
    /// what the numbers *mean* — a half, a quarter, how close counts — is the manager's and
    /// never reaches this side. Bounded at [`MAX_SNAP_ZONES`].
    zones: Vec<MgrSnapZone>,
    /// The zone the pointer is in during a move, if any — what the outline is previewing.
    ///
    /// Kept rather than recomputed at the release so that the outline and the rectangle finally
    /// asked for cannot come from different tests of the same pointer.
    in_zone: Option<u32>,
    /// Chords matched since the last [`take_hotkeys`](Self::take_hotkeys).
    ///
    /// **Drained by the caller rather than pushed into `out`.** `Outbound` is addressed to a
    /// *window* — `Outbound::window()` is the one place delivery is decided — and a hotkey is
    /// addressed to the manager, which is not a window and has its own queue. Threading a
    /// second out-param through `route` would put it in the signature of every caller that
    /// cannot produce one.
    fired: Vec<MgrHotkey>,
    /// Keycodes whose press was consumed as a hotkey, awaiting their release.
    ///
    /// **By keycode, not by re-matching the chord.** A user who lets go of `Super` before `2`
    /// releases a chord that no longer matches, so re-testing the modifiers on release would
    /// deliver a release for a press the focused window never saw — the same defect as handing
    /// a broken pointer grab's release to the window underneath.
    consumed: Vec<u16>,
}

impl InputRouter {
    /// A router with the cursor at the centre of `screen`.
    ///
    /// The centre rather than the origin because `(0, 0)` is a corner a panel usually
    /// occupies, and starting inside a window would owe an enter event nobody sent.
    pub fn new(screen: Rect) -> Self {
        let pointer = Point::new(
            screen.origin.x + (screen.size.w / 2) as i32,
            screen.origin.y + (screen.size.h / 2) as i32,
        );
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
    ///
    /// **The compositor already keeps its own copy** — `Server::outline`, which is what
    /// `present_into` reads — and this is the router's, which is what that copy is *derived*
    /// from through the `Outline` changes `route` hands back. Exposed so a test can ask the
    /// state rather than accumulating the changes itself, which would be a test that
    /// re-implements the thing it checks.
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
    ///
    /// The [`Routed`] it returns names the region a restack disturbed — a press raises the window
    /// it lands on, and the caller has to recompose exactly that much when it does.
    pub fn route(
        &mut self,
        ev: &Logical,
        stack: &mut WindowStack,
        out: &mut Vec<Outbound>,
    ) -> Routed {
        // Mirror the interpreter's state before anything is emitted. Enter and leave are
        // generated *by* the router rather than arriving as events, so they have no state of
        // their own to read; taking it from the event that provoked them is what lets every
        // record carry the same answer.
        match *ev {
            Logical::Key { modifiers, .. } => self.modifiers = modifiers,
            Logical::Motion { buttons, modifiers, .. }
            | Logical::Button { buttons, modifiers, .. } => {
                self.buttons = buttons;
                self.modifiers = modifiers;
            }
            Logical::Dropped => {
                self.buttons = 0;
                self.modifiers = 0;
            }
        }

        // **Reconcile with the stack — after the mirror above, not before it.** A window can
        // be destroyed between one event and the next, and since M8 Part A it can also be
        // minimized or moved to another desktop, which leaves it in the stack. The router is
        // on none of those paths, so it asks the stack rather than being called back: the
        // stack is the authority, and a question asked here cannot go out of date.
        //
        // It runs *after* the mirror because it emits — the leave below is a record like any
        // other, and records in one batch have to agree about `buttons` and `modifiers`. When
        // this block ran first it stamped the *previous* event's state onto them, so a release
        // that ended a shift-drag produced a leave saying shift was still down beside a button
        // record saying it was not (PR #240 review, blocking 1c).
        self.reconcile_with(stack, out);
        // What a gesture ending on this event owes the caller. Filled in wherever a grab ends —
        // the invariant just below, the release, and `Dropped` — and carried out through every
        // arm's `Routed`, because any of them can be the event that notices.
        let mut ended: Option<(u32, Rect)> = None;
        let mut outline_gone: Option<Outline> = None;
        // **A drag implies a grab, enforced here rather than trusted.** `reconcile_with` breaks
        // the grab of a window that has left the screen — minimized, or moved to another desktop
        // mid-gesture — and it takes the stack immutably, so the drag is torn down on its way
        // out instead. Stating it as an invariant rather than as a branch also covers the next
        // path that clears a grab without knowing a drag exists (PR #248 review, blocking 2).
        if self.grab.is_none() {
            (ended, outline_gone) = self.stop_drag(stack, false);
            // A resize goes with its grab for the same reason — and an outline left on screen
            // with nothing driving it is the visible version of that phantom. **`finished` is
            // false**: a grab taken away is not a gesture the user completed, so the outline
            // comes down and the shell is asked for nothing. (Twice over, in fact: this path is
            // reached when `reconcile_with` breaks a grab, which it does only for a window that
            // has left the screen — the other condition `stop_resize` requires.)
            let (r, gone) = self.stop_resize(stack, false);
            ended = ended.or(r);
            outline_gone = outline_gone.or(gone);
            // **And a payload goes with the grab that was carrying it.** `finished` is false, so
            // nothing is delivered: a grab taken away is not somebody letting go, and a drop
            // handed to a window because the gesture was interrupted is a file opened by an
            // application nobody dropped it on.
            let (_, gone) = self.stop_carrying(stack, false);
            outline_gone = outline_gone.or(gone);
        }

        match *ev {
            Logical::Key { keycode, pressed, modifiers } => {
                // **Before focus routing, and consuming rather than copying.** A chord that
                // also reached the focused window would type into it — `Super+2` would switch
                // desktops *and* put a `2` in the terminal.
                if self.take_as_hotkey(keycode, pressed, modifiers) {
                    return Routed { consumed: true, resized: ended, outline: outline_gone, ..Routed::default() };
                }
                let Some(window) = stack.focus_candidate() else {
                    // Nothing focusable. Dropping beats delivering to the pointer's window,
                    // which would make typing depend on where the cursor happens to rest.
                    //
                    // **The gesture's ending still gets out.** `reconcile_with` can break a
                    // grab on any event — a window minimized mid-drag — so a keystroke is a
                    // path on which a resize ends, and a `Routed::default()` here would leave
                    // the outline on screen with the shell never told.
                    return Routed { resized: ended, outline: outline_gone, ..Routed::default() };
                };
                // TODO(focus-change-key-balance): a key held across a focus change is delivered
                // to one window and released to another, or to none. Harmless today because
                // `KeyEvent` carries `modifiers` on every record, so no client accumulates
                // them — but it is the same unbalanced-press shape the chord rules above exist
                // to prevent, reached by a different route.
                out.push(Outbound::Key {
                    event: KeyEvent::new(window, keycode, u16::from(pressed), modifiers),
                });
                Routed { resized: ended, outline: outline_gone, ..Routed::default() }
            }

            Logical::Motion { dx, dy, .. } => {
                self.move_by(dx, dy);
                // **The drag first, so what follows describes where the window now is.** The
                // crossing pass and the motion record both read the stack, and a window that has
                // moved under the pointer this instant is the state a client should be told
                // about — not the one it was in a frame ago.
                let moved = self.drag_to_pointer(stack);
                // The outline follows the pointer the same way, and reaches no window at all:
                // it is drawn over the composed stack, like the cursor.
                //
                // **Two gestures produce one**, and they cannot both be running: a resize moves
                // it to the rectangle the edges say, and a *move* over a registered snap zone
                // shows that zone's target — a preview of what letting go there would ask for
                // (M9 Part F).
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
                if pressed && self.grab.is_none() {
                    // **Before the grab**, or the first click after boot is delivered to a
                    // window that was never entered: nothing has moved the cursor, `inside`
                    // is still `None`, and once grabbed the crossing pass early-returns —
                    // so the enter would only be derived on release, after a whole click had
                    // been processed for a pointer the client believes is elsewhere
                    // (PR #180 review, finding 7).
                    self.update_crossing(stack, out);
                    // The press decides the grab. Taking it before the raise reads as the
                    // careful order, but it is only the clearer one: hit-testing picks the
                    // topmost window *containing the point*, and raising that window leaves
                    // it topmost there too, so re-testing afterwards cannot differ.
                    self.grab = self.hit(stack);
                    // **Where it was pressed, kept with the grab it opened.** This is what an
                    // interactive move offsets by; see the field's own doc for why reading the
                    // pointer when `StartMove` arrives is a different, wrong number.
                    self.grab_at = self.pointer;
                    // Remembered for the release the compositor owes this window if the grab
                    // ends any way other than this button coming up.
                    self.grab_button = button;
                    if let Some(window) = self.grab
                        && stack.window(window).is_some_and(|w| w.role.takes_focus())
                    {
                        // Click to focus. `focus_candidate` is topmost-focusable, so the
                        // raise *is* the focus change — no second piece of state to
                        // disagree with the stack about who has focus.
                        //
                        // Empty damage is the press that changed nothing — the window was
                        // already on top — and reports no restack at all, so the caller neither
                        // recomposes nor announces a focus change that did not happen.
                        restacked = stack.raise(window).ok().filter(|d| !d.is_empty()).map(Damage::rect);
                    }
                }

                if let Some(window) = self.target(stack) {
                    let flags = if pressed { POINTER_PRESSED } else { 0 };
                    self.emit(window, POINTER_BUTTON, button, flags, stack, out);
                }

                if !pressed && buttons == 0 {
                    // The drag ends with the grab that carries it, and the stack records the
                    // one geometry change the whole gesture produced — **plus the snap zone it
                    // was dropped in**, if the pointer was in one (M9 Part F).
                    let (r, gone) = self.stop_drag(stack, true);
                    ended = ended.or(r);
                    outline_gone = outline_gone.or(gone);
                    // **And this is where a resize becomes a request.** The whole gesture has
                    // moved an outline; the manager hears one event, now, carrying the
                    // rectangle the user let go at.
                    let (r, gone) = self.stop_resize(stack, true);
                    ended = ended.or(r);
                    outline_gone = outline_gone.or(gone);
                    // **And this is where a drag becomes a drop.** Over a window that takes the
                    // payload it is one message to that window; over anything else the gesture
                    // is simply over — a drop on nothing is how somebody changes their mind, not
                    // an error to report to either side.
                    let (drop, gone) = self.stop_carrying(stack, true);
                    outline_gone = outline_gone.or(gone);
                    if let Some(rec) = drop {
                        out.push(rec);
                    }
                    // Last button up: the grab ends, and the cursor may have been dragged
                    // somewhere else entirely while it was held, so re-derive the crossing.
                    //
                    // This is also where a *broken* grab is finally settled: the sequence it
                    // belonged to is over, so input resumes and the window under the cursor
                    // gets its enter — the half deliberately withheld until now.
                    self.grab = None;
                    self.grab_broken = false;
                    self.update_crossing(stack, out);
                }
                Routed { restacked, resized: ended, outline: outline_gone, ..Routed::default() }
            }

            Logical::Dropped => {
                // Buttons are no longer trustworthy — a release may be the event that was
                // lost, and a grab that outlives its button never ends. `libinput` has
                // already reset what it accumulated; this is the same reset one layer up.
                //
                // A broken grab is cleared here too: `Dropped` says the button state itself is
                // unknown, so waiting for a release that may never be reported would wedge
                // input for the life of the process.
                let had = self.grab.take().is_some() || self.grab_broken;
                self.grab_broken = false;
                // And the drag derived from it — a window still tracking a pointer with nothing
                // held is the same phantom this arm exists to clear, one layer up. An outline
                // left on screen with nothing driving it is the visible version of it.
                let (r, gone) = self.stop_drag(stack, false);
                ended = ended.or(r);
                outline_gone = outline_gone.or(gone);
                let (r, gone) = self.stop_resize(stack, false);
                ended = ended.or(r);
                outline_gone = outline_gone.or(gone);
                // The payload too: `Logical::Dropped` says the pointer's position is a guess,
                // and a guess is the last thing to hand another application a file on.
                let (_, gone) = self.stop_carrying(stack, false);
                outline_gone = outline_gone.or(gone);
                if had {
                    self.update_crossing(stack, out);
                }
                // **Key beliefs go too.** `Dropped` says the held-key state is unknown, and
                // `consumed` and `held` are both beliefs about held keys: a swallowed release
                // would otherwise leave a keycode recorded forever, so the *next* ordinary
                // press of it is delivered and its release swallowed — a window holding a key
                // down for the rest of its life, which is the phantom-held-key bug `Dropped`
                // exists one layer down to prevent (PR #241 review, finding 2).
                self.consumed.clear();
                self.held.clear();
                Routed { resized: ended, outline: outline_gone, ..Routed::default() }
            }
        }
    }

    /// Drop router state that names a window which is gone or no longer on screen.
    ///
    /// **Two cases, and they differ in whether the window can be told.** A destroyed window is
    /// unreachable, so its id is simply forgotten. One that is merely off screen — minimized,
    /// or on another desktop — is still in the stack, still has a client, and is mid-way
    /// through an interaction the compositor promised to finish. It is told.
    fn reconcile_with(&mut self, stack: &WindowStack, out: &mut Vec<Outbound>) {
        let cur = stack.current_desktop();
        // Borrow-free classification: `emit` takes `&self`, so decide first, then act.
        let state = |id: u32| match stack.window(id) {
            None => Gone,
            Some(w) if !w.visible_on(cur) => OffScreen,
            Some(_) => OnScreen,
        };

        if let Some(id) = self.grab
            && !matches!(state(id), OnScreen)
        {
            // **The grab is why filtering hit-testing alone is not enough.** Every pointer
            // event up to the matching release goes to the grab holder *without* consulting
            // `hit`, so minimizing or switching away mid-drag would keep delivering motion and
            // the release to a window that is not on screen — the "invisible but still
            // hit-testable" bug, by the one path a hit-test filter cannot see (PR #239 review,
            // finding 2).
            if matches!(state(id), OffScreen) {
                // **Close the sequence, do not abandon it.** The grab exists so that a press
                // and its release reach one window even when the cursor leaves it; the window
                // going off screen ends the grab, and the release is the last thing the
                // compositor owes it. Without this the client is left holding a pointer
                // capture that only a release clears — in `libui`, `capture` survives, and the
                // next press after the window comes back routes to the stale widget rather
                // than the one clicked (PR #240 review, blocking 1b).
                //
                // This is not "input to an off-screen window": it is the tail of a sequence
                // granted while it was on screen, which is the same reason a grab delivers
                // outside the window's own bounds.
                self.emit(id, POINTER_BUTTON, self.grab_button, 0, stack, out);
            }
            self.grab = None;
            // **Unconditionally, not `self.buttons != 0`.** The mirror above has already
            // absorbed the provoking event, so when that event *is* the release the mask reads
            // zero and a condition on it would leave the flag clear — letting `target` fall
            // through to `hit` and deliver this very release to the window underneath, which
            // is the case this flag exists for. A grab existing at all means a button was
            // down; the sequence is broken either way. The last-button-up branch below clears
            // it in the same event when the release is what provoked this.
            self.grab_broken = true;
        }

        if let Some(id) = self.inside {
            match state(id) {
                OnScreen => {}
                // **The leave is emitted before the id is forgotten**, which is the whole of
                // the ordering. The first version cleared `inside` and then called
                // `update_crossing`, which derives the leave *from* `inside` — so the router
                // had already forgotten who was owed one and emitted only the enter. A client
                // whose window is minimized under the cursor kept its hover state forever,
                // because nothing else would tell it (PR #240 review, blocking 1a).
                OffScreen => {
                    self.emit(id, POINTER_LEAVE, 0, 0, stack, out);
                    self.inside = None;
                }
                Gone => self.inside = None,
            }
        }
    }

    /// Move the cursor by a delta, clamped to the screen.
    fn move_by(&mut self, dx: i32, dy: i32) {
        // Saturating, because a mouse held against the edge accumulates deltas forever and
        // `i32::MIN` would wrap the clamp below into placing the cursor at the far edge.
        let x = self.pointer.x.saturating_add(dx);
        let y = self.pointer.y.saturating_add(dy);
        // `right()`/`bottom()` are exclusive, so the last addressable pixel is one less.
        self.pointer = Point::new(
            x.clamp(self.screen.left(), self.screen.right() as i32 - 1),
            y.clamp(self.screen.top(), self.screen.bottom() as i32 - 1),
        );
    }

    /// Begin an interactive move of `window` on the client's behalf — `Surface::StartMove`.
    ///
    /// **The grab is the authority.** The request is refused unless the caller's window is the
    /// one holding the implicit pointer grab, which is what makes "the user is dragging me"
    /// true: without the check a client could move its window at any time, from anywhere, with
    /// nobody touching it — a `Place` for itself by another name, and `Place` is deliberately a
    /// manager op.
    ///
    /// **Refused while a resize or a drag-and-drop is running** — one grab carries one gesture.
    /// The resize half of that rule was enforced in one direction only until the M9 Part E
    /// review found it; the drag half was missing in *both* directions until a Part E test
    /// asked, which would have let a window follow the pointer while a payload was in flight
    /// out of it and made the release mean two things at once. Both gestures would
    /// run: the window would follow the pointer *and* the outline would follow the pointer,
    /// and the release would hand the shell a rectangle built from the window's origin at the
    /// *resize* press — an origin the move has since changed. The window jumps back by
    /// however far it was dragged, and the compositor's own move record and the `ResizeEnded`
    /// disagree about the origin in the same release. That is precisely the "two paths to a
    /// window's geometry that can disagree" this part exists to prevent, arriving through the
    /// one door left open. A client is not required to have good manners; the grab is the
    /// compositor's trust boundary.
    ///
    /// A second `StartMove` while one is running **changes nothing** and reports the same
    /// success. It cannot be treated as a fresh request: `from` is the press, which has not
    /// moved, but the window's origin has — so rebuilding the drag from where the window is
    /// *now* applies the distance already travelled a second time, and the window jumps by it.
    /// One gesture holds one grab and is one drag; the second request names the drag that is
    /// already running (PR #248 review, blocking 1).
    ///
    /// Returns the region the catch-up disturbed, which the caller must repaint.
    pub fn start_move(
        &mut self,
        window: u32,
        stack: &mut WindowStack,
    ) -> Result<Option<Rect>, StackError> {
        if self.grab != Some(window) || self.resize.is_some() || self.carrying.is_some() {
            return Err(StackError::NoSuchWindow);
        }
        if self.drag.is_some_and(|d| d.window == window) {
            return Ok(None);
        }
        let origin = stack.window(window).ok_or(StackError::NoSuchWindow)?.origin;
        stack.begin_drag(window)?;
        self.drag = Some(Drag { window, from: self.grab_at, origin });
        // **Applied at once, not at the next motion.** The pointer has already moved by the time
        // this request arrives — that round trip is the reason the press position is recorded at
        // all — so a drag that waited for the next event would leave the window trailing until
        // one happened, and would never move it at all for a press-and-drag that ended in the
        // meantime. **And its damage is returned rather than dropped**: `Logical::Button` reports
        // no movement, so a press-flick-release with no motion event in between left the window
        // painted where it used to be until something unrelated repainted (PR #248 review,
        // finding 3).
        Ok(self.drag_to_pointer(stack))
    }

    /// Begin an interactive resize of `window` on the client's behalf — `Surface::StartResize`.
    ///
    /// **The grab is the authority**, exactly as it is for [`start_move`](Self::start_move): a
    /// client that could resize itself at any time from anywhere would be sending itself a
    /// `Configure`, and `Configure` is deliberately a manager op.
    ///
    /// **Nothing about the window changes.** The window keeps its rectangle for the whole
    /// gesture; what moves is an outline the compositor draws over the composed stack, and the
    /// manager hears one `DragEnded` when the button comes up. The stack is marked as being
    /// dragged all the same, so a `Place` landing mid-gesture is refused rather than fighting
    /// the pointer — the same reason a move refuses it.
    ///
    /// A second `StartResize` while one is running changes nothing and reports success, for the
    /// reason a second `StartMove` does: one gesture holds one grab and is one drag, and
    /// rebuilding from where things are *now* would apply the distance already travelled twice.
    /// A `StartResize` while a *move* is running is refused: they are two gestures and there is
    /// one grab.
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
        // Drawn at once rather than at the next motion, for the reason a move is applied at
        // once: the pointer has already travelled during the round trip that brought this
        // request, and a press-flick-release with no motion event in between would otherwise
        // show no outline at all and end at the rectangle it started from.
        self.outline = Some(self.outline_now());
        Ok(self.outline)
    }

    /// Begin a drag-and-drop gesture out of `window` — `Surface::StartDrag`.
    ///
    /// **The grab is the authority**, exactly as it is for [`start_move`](Self::start_move) and
    /// [`start_resize`](Self::start_resize), and for a reason that is sharper here: a drag is an
    /// *offer of a payload* to whatever window it ends over. A client that could start one with
    /// nobody touching it could push a path into another application's window at any moment.
    ///
    /// **Refused while a move or a resize is running**, and they refuse while this is — one grab
    /// carries one gesture. A second `StartDrag` while one is running replaces the payload
    /// rather than being refused: the gesture is the same gesture, and a client that decided
    /// what it was dragging a moment later than it decided *that* it was dragging is not doing
    /// anything wrong.
    ///
    /// Returns the highlight to draw, if the pointer already stands over a window that takes it
    /// — for the reason a move applies its first step at once: the pointer has travelled during
    /// the round trip that brought this request.
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

    /// Whether a drag is in flight — for a caller deciding what a release means.
    pub fn is_dragging_payload(&self) -> bool {
        self.carrying.is_some()
    }

    /// Move the highlight to whatever window would take the drag now.
    ///
    /// **The topmost window under the pointer, and only if it takes this kind** — not the
    /// topmost *acceptor*. A window that does not take the payload is still a window: a drag
    /// passing over it must not highlight something behind it, because letting go there drops on
    /// nothing, and a highlight that lies about where a payload will land is worse than none.
    fn highlight_target(&mut self, stack: &WindowStack) -> Option<Outline> {
        let Some(c) = &self.carrying else { return None };
        // **The source window is skipped.** A drag out of a window that also accepts drops would
        // otherwise highlight itself the instant it began, which is a gesture nobody is making —
        // and dropping a thing back where it came from is the definition of a no-op.
        let found = self
            .hit(stack)
            .filter(|&w| w != c.window)
            .filter(|&w| stack.acceptor_for(w, c.kind).is_some());
        if found == self.over {
            return None;
        }
        let was = self.outline;
        self.over = found;
        self.outline = found.and_then(|w| stack.window(w)).map(|w| w.bounds());
        Some(Outline { was, now: self.outline })
    }

    /// End a drag-and-drop gesture, if one is running.
    ///
    /// **The drop is reported only if `finished`**, which is the same rule a resize follows and
    /// for the same reason: a gesture whose grab was taken away, or whose input stream reported
    /// a loss, is not somebody letting go. Here the stakes are plainer than a rectangle — a drop
    /// hands a payload to another application, and one delivered because the pointer's position
    /// was a guess is a file opened by a window nobody dropped it on.
    ///
    /// Returns the event to deliver (if any) and the highlight to take down.
    fn stop_carrying(
        &mut self,
        stack: &WindowStack,
        finished: bool,
    ) -> (Option<Outbound>, Option<Outline>) {
        let Some(c) = self.carrying.take() else { return (None, None) };
        let target = self.over.take();
        let gone = self.outline.take().map(|was| Outline { was: Some(was), now: None });
        let event = finished
            .then_some(target)
            .flatten()
            // Re-read here rather than trusted: the window may have gone between the last
            // motion and this release, and `acceptor_for` is what says it still takes this.
            .and_then(|w| {
                let acceptor = stack.acceptor_for(w, c.kind)?;
                let rect = stack.window(w)?.bounds();
                Some(Outbound::Dropped {
                    window: w,
                    acceptor: alloc::string::String::from(acceptor),
                    kind: c.kind,
                    path: c.path.clone(),
                    name: c.name.clone(),
                    // **Window-local, like a `PointerEvent`'s**, which is what lets the client
                    // route it to a widget without the protocol knowing about regions.
                    x: self.pointer.x - rect.origin.x,
                    y: self.pointer.y - rect.origin.y,
                })
            });
        (event, gone)
    }

    /// The rectangle the outline should occupy for the pointer's current position.
    ///
    /// Every edge in the mask moves by the pointer's travel since the press; the opposite edge
    /// stays where it was. Each is clamped so the rectangle keeps at least [`MIN_RESIZE`] in
    /// each axis — a dragged edge that crossed its opposite would otherwise invert the
    /// rectangle, which is a shape no repaint can describe.
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
        // **Clamped per edge, not per rectangle.** The floor belongs to the axes the gesture is
        // actually dragging; running it over both makes a window narrower than `MIN_RESIZE`,
        // resized by its bottom edge alone, report a rectangle wider than it was — a
        // `Configure` widening an axis the user never touched (PR #253 review, optional 8).
        Rect::new(x, y, w.max(0) as u32, h.max(0) as u32)
    }

    /// Move the outline to follow the pointer, reporting where it was and where it is.
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

    /// Show, or stop showing, the snap target under the pointer during a move.
    ///
    /// **Only during a move**, because that is the gesture a zone answers: a resize already has
    /// an outline of its own, and a pointer wandering over a zone with no drag in flight is a
    /// pointer wandering. `None` when nothing changed, so a drag across the middle of the screen
    /// costs one table lookup per motion and no damage at all.
    fn preview_zone(&mut self) -> Option<Outline> {
        if self.drag.is_none() {
            // A gesture that is not a move shows no preview — and anything left over from one
            // goes, which is what makes a drag ending outside a zone take its outline down.
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

    /// Take down a snap preview, if one is showing.
    fn leave_zone(&mut self) -> Option<Outline> {
        self.in_zone.take()?;
        let was = self.outline.take();
        Some(Outline { was, now: None })
    }

    /// End an interactive resize, if one is running. Reports a rectangle only if `finished`.
    ///
    /// **Called wherever the grab ends**, like [`stop_drag`](Self::stop_drag) and for the same
    /// reason: a gesture is a belief derived from the grab, and one that outlives it leaves an
    /// outline on the screen with nothing driving it.
    ///
    /// **`finished` is true only for the button coming up**, and that distinction is where a
    /// resize stops being modelled on a move. A move has *applied* every step as it went, so
    /// ending it however it ends merely stops something already on screen. A resize has applied
    /// nothing — so reporting *initiates* a change, and initiating one from a gesture the user
    /// has not finished is a window jumping to a half-chosen size while they are still holding
    /// the button. `Logical::Dropped` is the clearest case: it means events were lost upstream
    /// and the pointer position is a guess, which is exactly the state not to derive a new
    /// window rectangle from (PR #253 review, finding 5).
    fn stop_resize(
        &mut self,
        stack: &mut WindowStack,
        finished: bool,
    ) -> (Option<(u32, Rect)>, Option<Outline>) {
        // **Read before the state is taken**, because that is what it is derived from.
        let Some(r) = self.resize else { return (None, None) };
        let final_rect = self.outline_now();
        self.resize = None;
        stack.end_drag();
        let gone = self.outline.take().map(|was| Outline { was: Some(was), now: None });
        // **Three things have to be true to ask the shell for anything.**
        //
        // The gesture was *finished* — the button came up — rather than ended by something
        // else; see this function's own doc for why that is the whole difference from a move.
        //
        // The window is still on screen. A client that exits mid-gesture is ordinary and there
        // is nobody to configure; a window *put away* mid-gesture is a gesture the user
        // interrupted, and resizing a window somebody just put away to a rectangle they were
        // half-way through choosing is not what they asked for.
        //
        // And the rectangle actually changed. A gesture that ended where it started is an
        // ordinary click on the grip: the manager's queue does not coalesce, and a `Configure`
        // to the size a window already has is a round trip through the client for no change.
        let on_screen = stack
            .window(r.window)
            .is_some_and(|w| w.visible_on(stack.current_desktop()));
        let rect =
            (finished && final_rect != r.rect && on_screen).then_some((r.window, final_rect));
        (rect, gone)
    }

    /// End an interactive move, if one is running.
    ///
    /// **Called wherever the grab ends, not only where a button comes up.** A drag is a belief
    /// derived from the grab — `Logical::Dropped` says the button state is unknown, and
    /// `reconcile_with` breaks a grab whose window left the screen — and a belief that outlives
    /// what it was derived from is the defect those two arms already exist to prevent. Left
    /// behind, the window went on following a pointer with nothing held, and every `Place`
    /// naming it was refused until the next click (PR #248 review, blocking 2).
    fn stop_drag(
        &mut self,
        stack: &mut WindowStack,
        finished: bool,
    ) -> (Option<(u32, Rect)>, Option<Outline>) {
        let Some(d) = self.drag.take() else { return (None, None) };
        stack.end_drag();
        // **The zone the drag ended in, if it ended in one and the user ended it** (M9 Part F).
        // Read from `in_zone` rather than re-tested here, so the rectangle asked for is the one
        // whose outline the user was looking at — two tests of the same pointer are two chances
        // to disagree. `finished` is the same gate a resize has, for the same reason: a drag
        // whose grab was taken away, or whose input stream reported a loss, is not a drop.
        let asked = self
            .in_zone
            .filter(|_| finished)
            .and_then(|id| self.zones.iter().find(|z| z.id == id))
            // A second line rather than the first: `reconcile_with` has already broken the grab
            // of a window that has gone, so no path through `route` reaches this with `finished`
            // true. Pinned directly by `a_gesture_whose_window_has_gone_asks_for_nothing…`.
            .filter(|_| stack.window(d.window).is_some())
            .map(|z| (d.window, Rect::new(z.target_x, z.target_y, z.target_w, z.target_h)));
        (asked, self.leave_zone())
    }

    /// Move the dragged window so it keeps the same point under the pointer.
    ///
    /// `None` when no drag is in flight, or when the window has gone — a client that exits with
    /// the button still down is ordinary, not an error.
    fn drag_to_pointer(&mut self, stack: &mut WindowStack) -> Option<Rect> {
        let d = self.drag?;
        let origin = Point::new(
            d.origin.x + (self.pointer.x - d.from.x),
            d.origin.y + (self.pointer.y - d.from.y),
        );
        match stack.drag_to(d.window, origin) {
            Ok(damage) => Some(damage.rect()),
            Err(_) => {
                // The window has gone; there is nobody to snap and nothing to report.
                let _ = self.stop_drag(stack, false);
                None
            }
        }
    }

    /// Register a chord, to be delivered to the manager instead of the focused window.
    ///
    /// `Err` if `id` is zero, already registered, or the table is full — never a silent
    /// replacement, because a manager that registered two chords under one id would be told
    /// nothing and then wonder why one of them never fires.
    pub fn register_hotkey(&mut self, hk: MgrHotkey) -> Result<(), HotkeyError> {
        if hk.id == 0 {
            return Err(HotkeyError::ZeroId);
        }
        if self.hotkeys.iter().any(|h| h.id == hk.id) {
            return Err(HotkeyError::DuplicateId);
        }
        // **And a duplicate *chord*, for the reason a duplicate id is refused.** `find` returns
        // the first match, so a second registration of the same `mods`+`code` would be
        // permanently silent — the manager told nothing, then wondering why one of them never
        // fires. That argument does not care which field collides (PR #241 review, finding 7).
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
    /// `Err` if `id` is zero — reserved so a zeroed body registers nothing — or if the table is
    /// full and this is a new id. `Ok(Some(_))` when this replaced the zone a drag is currently
    /// previewing, which the caller must repaint.
    ///
    /// **Replacing rather than refusing** is the difference between this table and the chord
    /// table beside it, and it follows from what each one is. A chord table is a set of distinct
    /// chords, so a duplicate id is a manager confusing itself. A zone table is a *layout*,
    /// recomputed wholesale whenever the work area changes — a shell re-registering its eight
    /// ids with new rectangles is doing the ordinary thing, and a refusal would leave it holding
    /// zones for a screen that has changed shape with no way to say so.
    pub fn register_zone(&mut self, z: MgrSnapZone) -> Result<Option<Outline>, HotkeyError> {
        if z.id == 0 {
            return Err(HotkeyError::ZeroId);
        }
        if let Some(existing) = self.zones.iter_mut().find(|e| e.id == z.id) {
            *existing = z;
            // **A preview of the zone just replaced follows it.** A shell re-registers on
            // `LayoutChanged`, which can arrive mid-drag — a panel appearing while a window is
            // being dragged — and leaving the old rectangle on screen would break the one
            // promise `stop_drag` makes: that what is asked for is what the user was looking at.
            // Re-previewed rather than re-tested, so the two cannot come apart (PR #254 review,
            // optional 5).
            if self.in_zone == Some(z.id) {
                let was = self.outline;
                self.outline =
                    Some(Rect::new(z.target_x, z.target_y, z.target_w, z.target_h));
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

    /// Forget every snap zone — the manager that registered them has gone.
    ///
    /// **Returns the preview it took down**, and the caller must repaint it. Clearing the table
    /// while a preview is showing is the one path that disturbs the outline without going
    /// through `stop_drag`: every other way of taking it down is gated on `in_zone`, so an
    /// outline left behind here is never reported to anybody and is redrawn by every later
    /// compose for the life of the process. That is reachable by the compositor's *designed*
    /// manager-death path — a shell exiting while the user is mid-drag over a zone — and it is
    /// the same phantom two other comments in this file already name (PR #254 review,
    /// blocking 1).
    pub fn clear_zones(&mut self) -> Option<Outline> {
        self.zones.clear();
        self.leave_zone()
    }

    /// The zone whose trigger region contains `at`, if any.
    ///
    /// **First match wins**, and overlapping triggers are the manager's business: it wrote the
    /// table, and a compositor arbitrating between two of its rectangles would be making the
    /// policy this design keeps out of here.
    fn zone_at(&self, at: Point) -> Option<MgrSnapZone> {
        self.zones.iter().copied().find(|z| {
            Rect::new(z.trigger_x, z.trigger_y, z.trigger_w, z.trigger_h).contains(at.x, at.y)
        })
    }

    /// Take the chords matched since this was last called.
    pub fn take_hotkeys(&mut self) -> Vec<MgrHotkey> {
        core::mem::take(&mut self.fired)
    }

    /// Consume a key transition if it belongs to a registered chord.
    ///
    /// Returns `true` when the key must **not** reach the focused window. A press matches on
    /// `mods` **exactly** — a prefix match would make `Super+Shift+2` fire `Super+2` as well —
    /// and its release is swallowed by keycode, whatever the modifiers say by then.
    fn take_as_hotkey(&mut self, keycode: u16, pressed: bool, modifiers: u16) -> bool {
        if !pressed {
            // The release half. Removing the record here is what stops one press from
            // swallowing every later release of the same key.
            if let Some(i) = self.consumed.iter().position(|&k| k == keycode) {
                self.consumed.swap_remove(i);
                return true;
            }
            // A key this router delivered is no longer held; its release is delivered too.
            if let Some(i) = self.held.iter().position(|&k| k == keycode) {
                self.held.swap_remove(i);
            }
            return false;
        }
        // **A key already down cannot begin a chord.** Its press was delivered, so its release
        // must be too — and a repeat arriving after a modifier went down would otherwise match
        // and swallow it. A chord fires on the transition *into* the chord.
        if self.held.contains(&keycode) {
            return false;
        }
        let Some(hk) = self.hotkeys.iter().find(|h| h.code == keycode && h.mods == modifiers)
        else {
            if !self.held.contains(&keycode) {
                self.held.push(keycode);
            }
            return false;
        };
        self.fired.push(*hk);
        // Guarded against a repeat: an auto-repeating press would otherwise push the same
        // keycode again and again, and only the first release would be swallowed.
        if !self.consumed.contains(&keycode) {
            self.consumed.push(keycode);
        }
        true
    }

    /// Forget every chord a manager registered.
    ///
    /// **Called when the manager channel closes.** Without it the table outlives its owner:
    /// every registered chord keeps being consumed and delivered to nobody, so the key silently
    /// reaches nothing for the life of the compositor — and a replacement manager inherits the
    /// dead one's ids, so re-registering its own returns `DuplicateId` (PR #241 review,
    /// finding 3). The same reasoning `close_manager` already applies to its queued events:
    /// they describe a world the departed manager asked about.
    pub fn clear_hotkeys(&mut self) {
        self.hotkeys.clear();
        // Anything mid-chord loses its owner with the table. Leaving `consumed` populated
        // would swallow releases for presses nothing will ever act on.
        self.consumed.clear();
    }

    /// The topmost window containing the cursor, whatever its role.
    fn hit(&self, stack: &WindowStack) -> Option<u32> {
        stack
            .windows()
            .iter()
            .rev()
            // **Only what is on screen can be clicked.** An unconfigured window still occupies
            // its rectangle in the stack — at the default origin, at its requested size — so
            // without this a click anywhere inside it lands on a window the compositor has
            // decided not to draw, in front of the visible one underneath (PR #218 review,
            // finding 3). Since M8 Part A the same is true of a minimized window and of one on
            // another desktop, and all three clauses are `visible_on` so that this and
            // `compose_into` cannot answer differently.
            .find(|w| {
                w.visible_on(stack.current_desktop())
                    && w.bounds().contains(self.pointer.x, self.pointer.y)
            })
            .map(|w| w.id)
    }

    /// Who a pointer event goes to: the grab holder if there is one, else what is under the
    /// cursor.
    fn target(&self, stack: &WindowStack) -> Option<u32> {
        if self.grab_broken {
            // The sequence still running belonged to a window that can no longer receive it.
            // `hit` would hand its tail to whoever is underneath — a release for a press that
            // window never saw.
            return None;
        }
        self.grab.or_else(|| self.hit(stack))
    }

    /// Emit leave/enter if the cursor changed windows. A no-op while grabbed.
    fn update_crossing(&mut self, stack: &WindowStack, out: &mut Vec<Outbound>) {
        if self.grab.is_some() || self.grab_broken {
            // Crossings during a drag would tell a window the cursor left while it is still
            // receiving that cursor's events — two contradictory statements at once.
            //
            // **A broken grab counts as a drag for this purpose**, because from the user's
            // side it is one: a button is still down. Resuming crossings the moment the grab
            // is taken away would walk enters and leaves across every window the cursor
            // crosses on the way to the release, none of which can receive the release.
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

    /// Build one pointer record with the cursor in `window`'s coordinates.
    fn emit(
        &self,
        window: u32,
        kind: u16,
        button: u16,
        flags: u16,
        stack: &WindowStack,
        out: &mut Vec<Outbound>,
    ) {
        let Some(w) = stack.window(window) else {
            return;
        };
        let origin = w.bounds().origin;
        let event = PointerEvent::new(
            window,
            kind,
            button,
            self.buttons,
            flags,
            self.modifiers,
            // Signed and unclamped: under a grab the cursor is routinely outside the
            // window, and reporting a clamped edge position would make a drag look like it
            // stopped at the border.
            self.pointer.x.saturating_sub(origin.x),
            self.pointer.y.saturating_sub(origin.y),
        );
        out.push(Outbound::Pointer { event });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::surface::{
        AttachBufferRequest, CommitRequest, CreateWindowRequest, DROP_KIND_DIR, DROP_KIND_FILE,
        Edge, MOD_SHIFT, Role, SURFACE_FORMAT_XRGB8888,
    };

    const SCREEN: Rect = Rect::new(0, 0, 640, 480);
    const BTN_LEFT: u16 = 0x110;

    /// A window at `(x, y)` of `w × h`, committed **and configured** — a window that is on
    /// screen, which is the only kind routing has anything to say about.
    ///
    /// Configured because since B4 neither hit-testing nor focus considers a window that is
    /// not: a held window occupies its rectangle in the stack but is not drawn, and routing a
    /// click or a keystroke to it would send input somewhere nobody can see.
    fn win(stack: &mut WindowStack, role: Role, x: i32, y: i32, w: u32, h: u32) -> u32 {
        let id = stack
            .create(&CreateWindowRequest::new(w, h, role))
            .expect("create");
        stack.mark_configured(id);
        // Damage ignored: this helper positions a window for a routing test, and nothing
        // in these tests paints.
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
            .commit(&CommitRequest {
                window: id,
                buffer: 1,
                damage_x: 0,
                damage_y: 0,
                damage_w: w,
                damage_h: h,
            })
            .expect("commit");
        id
    }

    /// Route one event and return what came out.
    fn go(r: &mut InputRouter, s: &mut WindowStack, ev: Logical) -> Vec<Outbound> {
        let mut out = Vec::new();
        r.route(&ev, s, &mut out);
        out
    }

    /// Put the cursor at an absolute screen position, discarding what that produced.
    fn warp(r: &mut InputRouter, s: &mut WindowStack, x: i32, y: i32) {
        let p = r.pointer();
        go(r, s, motion(x - p.x, y - p.y));
    }

    /// Motion with nothing held.
    fn motion(dx: i32, dy: i32) -> Logical {
        Logical::Motion { dx, dy, buttons: 0, modifiers: 0 }
    }

    /// Motion with the left button held — what `libinput` emits mid-drag.
    fn drag(dx: i32, dy: i32) -> Logical {
        Logical::Motion { dx, dy, buttons: 1, modifiers: 0 }
    }

    fn key(keycode: u16, pressed: bool) -> Logical {
        Logical::Key { keycode, pressed, modifiers: 0 }
    }

    /// A key transition with modifiers held — what a chord looks like on the wire.
    fn chord(keycode: u16, pressed: bool, modifiers: u16) -> Logical {
        Logical::Key { keycode, pressed, modifiers }
    }

    /// The "Super" key is `MOD_SUPER` on the wire — the name the modifier bitmask uses.
    const MOD_SUPER: u16 = librsproto::surface::MOD_META;
    /// `2` in the keycode table `libkern::abi` mirrors.
    const KEY_2: u16 = 3;

    // ---- drag and drop (M10 Part E) ----

    /// Two windows side by side: a source at the left and a target at the right.
    fn two_windows(s: &mut WindowStack) -> (u32, u32) {
        let src = win(s, Role::Normal, 0, 0, 200, 200);
        let dst = win(s, Role::Normal, 400, 0, 200, 200);
        (src, dst)
    }

    /// Press inside `src`, ask to carry `kind`, and drag to `(x, y)`.
    fn begin_drag(
        r: &mut InputRouter,
        s: &mut WindowStack,
        src: u32,
        kind: u32,
        to: (i32, i32),
    ) -> Option<Outline> {
        warp(r, s, 20, 20);
        go(r, s, button(true));
        let started = r.start_drag(src, kind, "/home/a.txt", "a.txt", s).expect("the grab is held");
        let p = r.pointer();
        go(r, s, drag(to.0 - p.x, to.1 - p.y));
        started
    }

    /// The drop record a release produced, if any.
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
        assert_eq!(
            r.outline(),
            Some(Rect::new(400, 0, 200, 200)),
            "the window that would take it is outlined"
        );

        let out = go(&mut r, &mut s, button(false));
        let Some(Outbound::Dropped { window, acceptor, kind, path, name, x, y }) = drop_of(&out)
        else {
            panic!("no drop was delivered: {out:?}");
        };
        assert_eq!(*window, dst);
        assert_eq!((acceptor.as_str(), kind, path.as_str(), name.as_str()),
            ("document", &DROP_KIND_FILE, "/home/a.txt", "a.txt"));
        // **Window-local, like a press's**, which is what lets the client route it to a widget.
        assert_eq!((*x, *y), (50, 60), "the pointer, relative to the window it landed on");
        assert_eq!(r.outline(), None, "and the highlight goes with the gesture");
    }

    #[test]
    fn a_window_that_does_not_take_this_kind_is_neither_highlighted_nor_dropped_on() {
        // **The first of Part E's two controls.** An editor that declares `file` must be inert
        // for a folder: a mechanism that highlighted everything would pass the positive test
        // above on its own, and the person dragging would be told a drop was possible where it
        // is not.
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
        // **The second control**, and the one that says the table is consulted at all: a window
        // that never declared anything must be exactly as inert as one that declared the wrong
        // kind — otherwise "declares nothing" would mean "takes everything".
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
        // A browser that also took drops would otherwise outline itself the instant a drag
        // began — a gesture nobody is making, and one whose completion means nothing.
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
        // **The same rule a resize follows, and the stakes are plainer here**: a drop hands a
        // payload to another application, and one delivered because the pointer's position was
        // a guess is a file opened by a window nobody dropped it on.
        // **The two ways a gesture ends without the button coming up**, and they are the two
        // the router models: the input stream reporting a loss, and the grab being taken away
        // because the window carrying it left the screen. (Motion arriving with no buttons held
        // is *not* one of them — a lost release is what `Logical::Dropped` exists to say, and
        // the router does not guess from a mirrored field.)
        for lost_input in [true, false] {
            let mut s = WindowStack::new();
            let (src, dst) = two_windows(&mut s);
            s.declare_acceptor(dst, "document", DROP_KIND_FILE).unwrap();
            let mut r = InputRouter::new(SCREEN);

            begin_drag(&mut r, &mut s, src, DROP_KIND_FILE, (450, 60));
            assert!(r.outline().is_some(), "precondition: the target is highlighted");

            let out = if lost_input {
                go(&mut r, &mut s, Logical::Dropped)
            } else {
                // The source window goes away mid-gesture: `reconcile_with` breaks the grab,
                // and the invariant at the top of `route` tears the drag down with it.
                s.set_minimized(src, true).expect("minimize");
                go(&mut r, &mut s, drag(1, 0))
            };
            assert!(drop_of(&out).is_none(), "an interrupted gesture is not somebody letting go");
            assert_eq!(r.outline(), None, "and the highlight comes down with the gesture");
        }
    }

    #[test]
    fn a_drag_is_refused_unless_the_pointer_is_holding_the_window() {
        let mut s = WindowStack::new();
        let (src, dst) = two_windows(&mut s);
        let mut r = InputRouter::new(SCREEN);

        // Nothing pressed at all.
        assert!(r.start_drag(src, DROP_KIND_FILE, "/a", "a", &s).is_err());

        // Pressed on the *other* window: the grab is what says the user is dragging *this*.
        warp(&mut r, &mut s, 450, 20);
        go(&mut r, &mut s, button(true));
        assert!(r.start_drag(src, DROP_KIND_FILE, "/a", "a", &s).is_err());
        assert!(r.start_drag(dst, DROP_KIND_FILE, "/a", "a", &s).is_ok(), "the held one may");
    }

    #[test]
    fn one_grab_carries_one_gesture() {
        // A move and a drag would both run: the window would follow the pointer while a payload
        // was in flight from it, and the release would be both a drop and a snap.
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
        // **Re-declaring is not a new entry**, so a client that changes its mind is not
        // eventually refused for saying the same thing again.
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

        // **The release too.** Delivering it would hand the window a release for a press it
        // never saw — the same defect as a broken pointer grab's tail going to whoever is
        // underneath.
        let out = go(&mut r, &mut s, chord(KEY_2, false, MOD_SUPER));
        assert!(out.is_empty(), "the release is consumed with the press");
    }

    #[test]
    fn a_chords_release_is_swallowed_by_keycode_even_after_the_modifiers_change() {
        // Letting go of `Super` before `2` is the ordinary way to release a chord, and by then
        // the modifiers no longer match. A compositor that re-tested them here would deliver
        // the release alone.
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

        go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        let out = go(&mut r, &mut s, chord(KEY_2, false, 0));
        assert!(out.is_empty(), "released after Super came up, and still swallowed");

        // And only that one release: the next press of the same key, without the chord, is an
        // ordinary keystroke again.
        let out = go(&mut r, &mut s, key(KEY_2, true));
        assert_eq!(out.len(), 1, "an unmodified press is delivered normally");
    }

    #[test]
    fn modifiers_must_match_exactly() {
        // A prefix match would make `Super+Shift+2` fire `Super+2` as well, so a shell binding
        // both would switch desktops every time you asked it to move a window to one.
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
        // A held chord repeats presses. Recording the keycode once per press would leave two
        // entries and swallow the *next* unrelated release of that key as well.
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
        // **The one thing `route` could not say.** The binary arms key repeat from the physical
        // transition — deliberately, so a key that reached no window still stops repeating when
        // it comes up — and a consumed chord looked exactly like an ordinary keystroke from
        // outside. So it armed, and 400 ms later delivered its key straight to the focused
        // window's session, bypassing the router: holding `Super+1` while already on desktop 1
        // filled the terminal with `1`s (PR #241 review, blocking 1).
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
        // Press `2` alone — delivered — then hold `Super`. The hardware repeat of `2` now
        // matches `Super+2`, so without this the chord would fire and the *physical* release
        // would be swallowed, leaving the window a press it never saw released
        // (PR #241 review, finding 2).
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

        assert_eq!(go(&mut r, &mut s, key(KEY_2, true)).len(), 1, "delivered while unmodified");
        // The repeat, now with Super held.
        let out = go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        assert_eq!(out.len(), 1, "a key already down is a repeat, not a new chord");
        assert!(r.take_hotkeys().is_empty(), "and it fires nothing");
        // The release balances the delivered press.
        assert_eq!(go(&mut r, &mut s, key(KEY_2, false)).len(), 1, "its release is delivered");
    }

    #[test]
    fn dropped_forgets_which_keys_were_consumed() {
        // `Dropped` says the held-key state is unknown. A swallowed release would otherwise
        // leave the keycode recorded forever: the next *ordinary* press of it is delivered and
        // its release swallowed, so the window holds that key down for the rest of its life —
        // the phantom-held-key bug `Dropped` exists one layer down to prevent.
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();

        go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        go(&mut r, &mut s, Logical::Dropped);
        // The release the chord was owed never arrives; the drop is what stands in for it.
        assert_eq!(go(&mut r, &mut s, key(KEY_2, true)).len(), 1, "an ordinary press after");
        assert_eq!(
            go(&mut r, &mut s, key(KEY_2, false)).len(),
            1,
            "and its release is delivered rather than swallowed by a stale record"
        );
    }

    #[test]
    fn clearing_the_table_stops_chords_being_consumed_by_nobody() {
        // The table used to outlive the manager that registered it: every chord kept being
        // consumed and delivered to a channel that no longer existed, so the key silently
        // reached nothing for the life of the compositor (PR #241 review, finding 3).
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        r.register_hotkey(MgrHotkey { id: 1, mods: MOD_SUPER, code: KEY_2 }).unwrap();
        assert!(go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER)).is_empty(), "consumed");
        r.take_hotkeys();

        r.clear_hotkeys();
        let out = go(&mut r, &mut s, chord(KEY_2, true, MOD_SUPER));
        assert_eq!(out.len(), 1, "with no table the chord is an ordinary keystroke again");
        // And the id is free for a replacement manager.
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
        assert_eq!(
            r.register_hotkey(MgrHotkey { id: 99, mods: 0, code: 99 }),
            Err(HotkeyError::TableFull)
        );
    }

    fn button(pressed: bool) -> Logical {
        Logical::Button {
            button: BTN_LEFT,
            pressed,
            buttons: if pressed { 1 } else { 0 },
            modifiers: 0,
        }
    }

    #[test]
    fn keys_go_to_the_topmost_focusable_window_not_the_one_under_the_cursor() {
        // The distinction the whole focus/hit-test split exists for: typing must not
        // depend on where the mouse happens to be resting.
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

    /// A window whose first `Configure` is still held takes neither the keyboard nor a click.
    ///
    /// It is on top of the stack and occupies its rectangle there, but the compositor has
    /// decided it is not on screen (M6 B4). Routing to it would send keystrokes into a window
    /// nobody can see and make a click land on it rather than on the visible window underneath
    /// — for as long as the hold lasts, which on the deadline path is a bounded 200 ms during
    /// every window launch (PR #218 review, finding 3).
    #[test]
    fn a_window_whose_configure_is_held_takes_neither_focus_nor_a_click() {
        let mut s = WindowStack::new();
        let visible = win(&mut s, Role::Normal, 0, 0, 200, 200);
        // Created *over* the visible one and never configured — a launch in progress. Built
        // with `create` rather than `win`, precisely because `win` configures.
        let held = s
            .create(&CreateWindowRequest::new(200, 200, Role::Normal))
            .expect("create");
        // No commit needed: `bounds()` falls back to the requested size, which is exactly the
        // rectangle a held window occupies in the stack and the reason it could be clicked.

        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        assert_eq!(r.inside(), Some(visible), "the click lands on what is on screen");

        let out = go(&mut r, &mut s, key(30, true));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].window(), visible, "and so does the keystroke");

        // Configured, and it takes both — it was topmost all along.
        s.mark_configured(held);
        warp(&mut r, &mut s, 51, 51);
        assert_eq!(r.inside(), Some(held));
        let out = go(&mut r, &mut s, key(30, true));
        assert_eq!(out[out.len() - 1].window(), held);
    }

    #[test]
    fn a_panel_never_takes_a_keystroke_but_does_take_a_click() {
        // Clicking the clock must not stop the terminal receiving what you type next —
        // and a panel you cannot click is not a panel.
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
        // The fresh-press half of "invisible but still hit-testable". `hit()` is where the
        // filter lives, so this is the half a filter in `hit()` does catch — and on its own it
        // is not enough to prove the rule, which is what the next test is for.
        let mut s = WindowStack::new();
        let here = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let gone = win(&mut s, Role::Normal, 0, 0, 200, 200);
        s.set_window_desktop(gone, 2).unwrap();
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);

        let out = go(&mut r, &mut s, button(true));
        // `gone` is above `here` in the stack and covers the same pixels, so without the
        // filter it would take the click.
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
        // **The half a filter in `hit()` cannot reach**, and the reason this rule needed two
        // controls. `target()` is `grab.or_else(hit)`, so once a press has grabbed, every
        // event until the release bypasses hit-testing entirely — and nothing clears `grab` on
        // a desktop switch, which is not a destroy, not a last-button-up and not a `Dropped`.
        // An implementation that filters only `hit()` passes the test above and fails here
        // (PR #239 review, finding 2).
        let mut s = WindowStack::new();
        let dragged = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        let out = go(&mut r, &mut s, button(true));
        assert!(
            out.iter().any(|o| matches!(o, Outbound::Pointer { event } if event.window == dragged)),
            "precondition: the press landed and grabbed"
        );

        // The manager switches desktops while the button is still held.
        s.set_current_desktop(2).unwrap();

        // The next event is what the router reconciles on. The window is owed exactly two
        // things and no more: the release that closes the sequence it was granted, and the
        // leave that says the cursor is no longer in it.
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

        // And from here it is owed nothing at all: motion during the rest of the drag, and the
        // real release, both go nowhere.
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
        // The same rule by the other route. Named separately because the two reach
        // `visible_on` through different attributes, and a fix that special-cased the desktop
        // comparison would pass the test above and fail this one.
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
        // **The leave has to be emitted before the id is forgotten.** The first version cleared
        // `inside` and *then* re-derived the crossing, which derives the leave from `inside` —
        // so it emitted the enter alone and the window under the cursor at minimize time never
        // learned the pointer had gone. In `libui` that leaves the hovered widget highlighted
        // for as long as the window stays hidden, since nothing else would clear it
        // (PR #240 review, blocking 1a).
        let mut s = WindowStack::new();
        let under = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let over = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        assert_eq!(r.inside, Some(over), "precondition: the cursor is in the top window");

        s.set_minimized(over, true).unwrap();
        let out = go(&mut r, &mut s, motion(1, 1));

        let leave = out.iter().any(|o| matches!(
            o, Outbound::Pointer { event } if event.window == over && event.kind == POINTER_LEAVE));
        let enter = out.iter().any(|o| matches!(
            o, Outbound::Pointer { event } if event.window == under && event.kind == POINTER_ENTER));
        assert!(leave, "the window that lost the cursor is owed a leave");
        assert!(enter, "and the one that gained it an enter");
    }

    #[test]
    fn breaking_a_grab_gives_the_release_to_nobody_else() {
        // The tail of a sequence belongs to the window that was granted it. Handing it to
        // whatever is underneath means a release for a press that window never saw — and in
        // `libui` a bare release fires nothing, so the defect stays silent until a client acts
        // on one (PR #240 review, blocking 1b).
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
        // **An enter is fine and correct**: the buttons are up, the sequence is over, and the
        // cursor really is inside `under`. What must not arrive is the button record.
        assert!(
            out.iter().any(|o| matches!(
                o, Outbound::Pointer { event } if event.window == under
                    && event.kind == POINTER_ENTER)),
            "and once the sequence is over it is entered normally"
        );

        // Once the buttons are up the world resumes: the next press reaches `under` normally.
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
        // Every record in one batch has to agree about `buttons` and `modifiers` — the router
        // generates enters and leaves itself, so it stamps them from the event that provoked
        // them. Reconciling *before* that mirror stamped the previous event's state instead, so
        // a release ending a shift-drag produced a leave saying shift was still held beside a
        // button record saying it was not (PR #240 review, blocking 1c).
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        go(
            &mut r,
            &mut s,
            Logical::Button { button: BTN_LEFT, pressed: true, buttons: 1, modifiers: MOD_SHIFT },
        );

        s.set_minimized(w, true).unwrap();
        // Shift released along with the button — the provoking event says neither is held.
        let out = go(
            &mut r,
            &mut s,
            Logical::Button { button: BTN_LEFT, pressed: false, buttons: 0, modifiers: 0 },
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
        // The reserved value has to work, not merely be reserved: `desktop == 0` is the one
        // value `visible_on` accepts regardless of the current desktop.
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
    fn clicking_a_window_raises_it_and_that_is_the_focus_change() {
        let mut s = WindowStack::new();
        let lower = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let upper = win(&mut s, Role::Normal, 300, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        assert_eq!(s.focus_candidate(), Some(upper));

        warp(&mut r, &mut s, 50, 50);
        let mut out = Vec::new();
        let routed = r.route(&button(true), &mut s, &mut out);
        assert_eq!(
            routed.restacked,
            Some(Rect::new(0, 0, 100, 100)),
            "the caller must recompose, and only where the raised window is"
        );
        assert_eq!(s.focus_candidate(), Some(lower), "the raise *is* the focus change");

        // **And the second click on the same window recomposes nothing.** Click-to-focus
        // raises on every press; a raise that changes no order changes no pixels, and
        // answering "recompose" to it is what made every click on the focused window cost a
        // full-screen repaint — ~100 ms under emulation, with no input read during it.
        let again = r.route(&button(false), &mut s, &mut out);
        assert_eq!(again.restacked, None);
        let again = r.route(&button(true), &mut s, &mut out);
        assert_eq!(again.restacked, None, "already topmost: nothing moved, nothing to paint");
    }

    #[test]
    fn an_interactive_move_offsets_by_where_the_press_landed_not_by_where_the_request_arrived() {
        // **The whole point of recording the press position.** `StartMove` arrives a round trip
        // after the press — the client has to receive it, route it through its own toolkit and
        // decide it landed on a title bar — and the pointer keeps moving meanwhile. A drag that
        // takes its origin from the pointer *at the request* jumps by that distance and then
        // tracks correctly, which is the defect `TODO(scroll-grab)` describes for a scrollbar.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);

        // Press 20 px into the window, then move 40 px before the request is dispatched.
        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        go(&mut r, &mut s, drag(40, 0));
        r.start_move(w, &mut s).expect("the grab is on that window");

        // One more motion. The window must sit where the *press* offset says, which is the
        // pointer minus the 20 px into the window — not the pointer minus 60.
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
        // **The offsets are not the same, which the spec used to claim.** `from` is the press
        // and does not move; the window's origin does — so rebuilding the drag from where the
        // window is *now* applies the distance already travelled a second time. Reachable by
        // pressing a second button mid-drag, which used to re-fire the toolkit's press handler
        // (PR #248 review, blocking 1).
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        r.start_move(w, &mut s).expect("grabbed");
        go(&mut r, &mut s, drag(40, 20));
        let after_first = s.window(w).expect("there").origin;

        r.start_move(w, &mut s).expect("still grabbed");
        assert_eq!(
            s.window(w).expect("there").origin,
            after_first,
            "the second request moved the window again"
        );
    }

    #[test]
    fn a_drag_ends_when_a_dropped_batch_takes_its_grab_away() {
        // `Dropped` says the button state is unknown, so the grab is cleared — and a drag is a
        // belief derived from that grab. Left behind, the window follows a pointer with nothing
        // held and `Place` for it stays refused until the next click.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 120, 110);
        go(&mut r, &mut s, button(true));
        r.start_move(w, &mut s).expect("grabbed");

        go(&mut r, &mut s, Logical::Dropped);
        let at = s.window(w).expect("there").origin;
        assert_eq!(s.dragging(), None, "the drag went with the grab");
        go(&mut r, &mut s, motion(30, 30));
        assert_eq!(s.window(w).expect("there").origin, at, "and the window stopped following");
        assert!(s.place(w, Point::new(0, 0)).is_ok(), "and a manager may place it again");
    }

    #[test]
    fn a_drag_ends_when_its_window_leaves_the_screen() {
        // Minimizing or moving a dragged window to another desktop breaks the grab —
        // `reconcile_with` emits the release the client is owed. A window that went on tracking
        // the pointer would contradict that release, invisibly.
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
        // An ordinary click on a title bar is a drag of zero pixels. Recording one puts a no-op
        // event into the manager's queue — the one that does not coalesce and evicts its oldest.
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
        // The pointer has already moved by the time `StartMove` arrives, so the window moves the
        // instant the drag begins — and `Logical::Button` reports no movement, so if this damage
        // were dropped a press-flick-release would leave the window drawn where it used to be.
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
        // Without this a client could move its own window at any time from anywhere, which is
        // `Place` for itself — and `Place` is a manager op deliberately.
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

    /// Route one event and return what it did to the stack and the outline.
    fn go_routed(r: &mut InputRouter, s: &mut WindowStack, ev: Logical) -> Routed {
        let mut out = Vec::new();
        r.route(&ev, s, &mut out)
    }

    /// Press at `(x, y)` on the window under it and begin resizing `edges`.
    fn grip(r: &mut InputRouter, s: &mut WindowStack, w: u32, x: i32, y: i32, edges: u32) {
        warp(r, s, x, y);
        go(r, s, button(true));
        r.start_resize(w, edges, s).expect("the grab is on that window");
    }

    /// A zone whose trigger is `t` and whose target is `g`.
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

    /// Press on `w` at `(x, y)` and begin an interactive move.
    fn hold(r: &mut InputRouter, s: &mut WindowStack, w: u32, x: i32, y: i32) {
        warp(r, s, x, y);
        go(r, s, button(true));
        r.start_move(w, s).expect("the grab is on that window");
    }

    #[test]
    fn a_move_into_a_zone_previews_its_target_and_a_drop_asks_for_it() {
        // **The whole of Part F.** The compositor matches the pointer against a table it was
        // given and knows nothing about halves or edges; what it shows is that zone's *target*,
        // and what it asks for at the release is the same rectangle — so the user cannot be
        // shown one thing and given another.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
        let mut r = InputRouter::new(SCREEN);
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
        hold(&mut r, &mut s, w, 240, 240);

        // Across the middle: no zone, no outline, and the window follows as it always did.
        let routed = go_routed(&mut r, &mut s, drag(-100, 0));
        assert!(routed.outline.is_none(), "nothing is previewed away from a zone");
        assert!(routed.moved.is_some(), "and the move is unaffected");

        // Into the band at the left edge.
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
        // The control the plan names. A preview shown and taken down again must leave nothing
        // behind: the gesture is decided by where the button comes up, not by where the pointer
        // has been.
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
        // **The difference from a chord table, and it is what the table is.** Zones are a
        // layout: a shell recomputes all of them when the work area changes and re-registers
        // the same ids. A refusal would leave it holding zones for a screen that has changed
        // shape with no way to say so.
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
        // They are a layout that manager computed from a work area it was watching; a
        // replacement inherits nothing it did not register, exactly as with chords.
        let mut r = InputRouter::new(SCREEN);
        r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
        assert!(r.zone_at(Point::new(5, 5)).is_some());
        assert_eq!(r.clear_zones(), None, "nothing was being previewed");
        assert!(r.zone_at(Point::new(5, 5)).is_none());

        // **And the pixels go with the table.** A shell exiting mid-drag is the compositor's
        // designed manager-death path, and every other way of taking a preview down is gated on
        // the zone this just removed — so an outline left here is never reported to anybody and
        // is redrawn by every later compose for the life of the process (PR #254 review,
        // blocking 1).
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
        // And nothing is left for a later event to report, because there is nothing left.
        assert!(go_routed(&mut r, &mut s, drag(5, 0)).outline.is_none());
        assert!(go_routed(&mut r, &mut s, button(false)).outline.is_none());
    }

    #[test]
    fn re_registering_the_zone_under_the_pointer_moves_the_preview_with_it() {
        // A shell re-registers its whole table on `LayoutChanged`, which can arrive mid-drag —
        // a panel appearing while a window is being dragged. Leaving the old rectangle on screen
        // would break the one promise the drop makes: that what is asked for is what the user
        // was looking at (PR #254 review, optional 5).
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
        // **The rule Part E's review required, on the other gesture.** A `Logical::Dropped` says
        // events were lost and the pointer position is a guess; a grab taken away says the
        // gesture was interrupted. Neither is a drop, and snapping a window somebody is still
        // holding is what asking for one here would mean.
        //
        // The gate met this before the test did: its first version injected motions fast enough
        // to overrun the consumer ring, and the drag correctly asked for nothing (PR #254
        // review, finding 2).
        for interrupted_by_loss in [true, false] {
            let mut s = WindowStack::new();
            let w = win(&mut s, Role::Normal, 200, 200, 100, 80);
            let mut r = InputRouter::new(SCREEN);
            r.register_zone(zone(1, (0, 0, 20, 480), (0, 0, 320, 480))).unwrap();
            hold(&mut r, &mut s, w, 240, 240);
            go(&mut r, &mut s, drag(-230, 0));

            let routed = if interrupted_by_loss {
                go_routed(&mut r, &mut s, Logical::Dropped)
            } else {
                // The window leaves the screen under the pointer; `reconcile_with` breaks the
                // grab on the next event and the invariant tears the gesture down.
                s.set_minimized(w, true).expect("put away mid-drag");
                go_routed(&mut r, &mut s, key(30, true))
            };
            assert!(
                routed.resized.is_none(),
                "loss={interrupted_by_loss}: an unfinished drag asked for a snap"
            );
            assert_eq!(
                routed.outline.expect("the preview is taken down").now,
                None,
                "loss={interrupted_by_loss}"
            );
        }
    }

    #[test]
    fn a_gesture_whose_window_has_gone_asks_for_nothing_even_when_finished() {
        // **Called directly, because no route through `route` reaches it.** `reconcile_with`
        // runs before every arm and breaks the grab of a window that is not on screen, so by
        // the time `finished` can be true the window is there — which makes these two checks a
        // second line rather than the first. Asserting that through `route` would be asserting
        // `reconcile_with`, which has its own tests; this states what the guards themselves do,
        // so that a later change to the ordering does not silently make them wrong.
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
        // A pointer wandering over a zone with nothing held is a pointer wandering. **The
        // approach has to happen inside the assertion**: `warp` is itself a motion, so warping
        // *into* the zone consumes the transition and the next event finds nothing changed —
        // which is how the first version of this test passed with the guard removed.
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
        // **A resize is not asserted here, and the reason is worth stating rather than
        // asserting badly.** `preview_zone` is reached through an `.or_else` after
        // `outline_to_pointer`, which returns `Some` for every motion while a resize runs — so
        // no implementation in reach consults the table during one, and a test saying "a resize
        // is not snapped" passes for every version of this code including the ones that would
        // be wrong. It was there, it was decoration, and a control found it (PR #254 review,
        // optional 6). What keeps a resize from being snapped is that `start_move` and
        // `start_resize` refuse each other, which has its own test.
    }

    #[test]
    fn a_resize_needs_the_grab_and_a_pair_of_edges_that_name_a_gesture() {
        // The grab is what makes "the user is dragging my edge" true, exactly as it is for a
        // move: without it a client could resize itself from anywhere with nobody touching it,
        // which is a `Configure` for itself — and `Configure` is a manager op deliberately.
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 200, 200);
        let b = win(&mut s, Role::Normal, 300, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        assert!(r.start_resize(a, RESIZE_RIGHT, &mut s).is_err(), "no button is down");

        warp(&mut r, &mut s, 350, 50);
        go(&mut r, &mut s, button(true));
        assert!(r.start_resize(a, RESIZE_RIGHT, &mut s).is_err(), "the grab is elsewhere");

        // Opposite edges together is no gesture — nobody drags a window's left and right at
        // once — and neither is naming none, which would hold a grab and change nothing.
        for edges in [0, RESIZE_LEFT | RESIZE_RIGHT, RESIZE_TOP | RESIZE_BOTTOM, 1 << 9] {
            assert_eq!(
                r.start_resize(b, edges, &mut s),
                Err(StackError::BadGeometry),
                "edges {edges:#x}"
            );
        }
        assert!(r.start_resize(b, RESIZE_RIGHT | RESIZE_BOTTOM, &mut s).is_ok());
    }

    #[test]
    fn the_outline_follows_the_pointer_and_the_window_does_not_move() {
        // **The whole of decision 3.** A resize that changed the window per motion would make
        // the client allocate, map, re-lay-out and repaint per motion. What moves is an outline
        // the compositor draws over the composed stack; the client hears nothing until the
        // button comes up.
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
        // The half that is easy to get wrong: a left edge dragged right makes the window
        // narrower *and* moves it, because the right edge stays where it is.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 100, 100, RESIZE_LEFT | RESIZE_TOP);

        let o = go_routed(&mut r, &mut s, drag(30, 20)).outline.expect("moved");
        assert_eq!(o.now, Some(Rect::new(130, 120, 170, 80)));
    }

    #[test]
    fn an_edge_dragged_past_its_opposite_stops_rather_than_inverting() {
        // A rectangle with a negative width is a shape no repaint can describe. The floor is
        // the compositor's own, not a client's minimum — the protocol has no way to state one,
        // and a `Configure` is a request the client may decline anyway.
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);

        let o = go_routed(&mut r, &mut s, drag(-400, -400)).outline.expect("moved");
        assert_eq!(o.now, Some(Rect::new(100, 100, MIN_RESIZE, MIN_RESIZE)));

        // And from the other side, where the *origin* is what the clamp has to hold still.
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
        // A window already narrower than the floor, dragged by its bottom edge alone: its width
        // is nobody's business here, and a rectangle-wide clamp would widen it to `MIN_RESIZE`
        // and hand the shell a `Configure` for an axis the user never touched.
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
        // A second release reports nothing: the gesture is over.
        assert_eq!(go_routed(&mut r, &mut s, button(false)).resized, None);
    }

    #[test]
    fn a_grip_click_that_moves_nothing_reports_nothing() {
        // An ordinary click on the corner is a resize of zero pixels. The manager's queue does
        // not coalesce, and a `Configure` to the size a window already has is a round trip
        // through the client for no change — the same argument `end_drag` makes for a move.
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

        // **And the other way round, which was enforced in one direction only.** With both
        // running the window follows the pointer *and* the outline does, and the release
        // reports a rectangle built from the origin the window had before the move — so it
        // jumps back by however far it was dragged (PR #253 review, blocking 1).
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

        // And a second `StartResize` during a resize is the same gesture, not a new one:
        // rebuilding from where things are now would apply the travel so far a second time.
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
        // A resize marks the window as being dragged for the reason a move does: a `Place` that
        // landed mid-gesture would fight the pointer. And a gesture is a belief derived from the
        // grab — `Dropped` says the button state is unknown, so an outline left on screen with
        // nothing driving it is the visible version of the phantom that arm exists to clear.
        //
        // **But it reports nothing**, which is where a resize stops being modelled on a move. A
        // move has applied every step already; a resize has applied none, so reporting here
        // would *initiate* a change from a pointer position this arm has just declared a guess,
        // while the button is still down — the window jumping to a half-chosen size mid-drag.
        // The failure `Dropped` stands for is a ring overflow under a heavy recompose, which is
        // the one this milestone keeps meeting (PR #253 review, finding 5).
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
        let mut r = InputRouter::new(SCREEN);
        grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
        assert_eq!(s.place(w, Point::new(0, 0)), Err(StackError::Dragging));
        go(&mut r, &mut s, drag(40, 30));

        let routed = go_routed(&mut r, &mut s, Logical::Dropped);
        assert_eq!(routed.resized, None, "a gesture the user did not finish asks for nothing");
        assert_eq!(routed.outline.expect("taken down").now, None);
        assert!(s.place(w, Point::new(0, 0)).is_ok(), "and the window is a manager's again");
    }

    #[test]
    fn a_window_put_away_mid_resize_ends_the_gesture_on_whichever_key_arm_notices() {
        // **`reconcile_with` breaks a grab on *any* event**, so the path that ends a gesture is
        // not always a pointer one: a chord that puts the window being resized away ends it on a
        // keystroke. A `Routed::default()` on such an arm leaves `srv.outline` set in the
        // compositor, and `repaint_region` then redraws it on every subsequent repaint — an
        // outline stranded on a desktop with nothing driving it.
        //
        // **All three key arms**, because the first version of this test reached exactly one:
        // it had a single window, so minimizing it left `focus_candidate` empty and every run
        // took the nothing-focusable early return. Breaking either of the other two failed
        // nothing in the crate (PR #253 review, finding 3).
        //
        // The reachable one is the *hotkey* arm: the user holds the grip and presses a chord.
        // It is consumed at the press, while the window is still on screen, so nothing is torn
        // down; the release comes back after the chord has put the window away, is swallowed by
        // keycode, and returns through that arm — which is the event on which the grab breaks.
        for arm in ["nothing focusable", "delivered", "hotkey"] {
            let mut s = WindowStack::new();
            let w = win(&mut s, Role::Normal, 100, 100, 200, 100);
            // A second window for the arms that need somebody to deliver to, and none for the
            // arm that needs nobody.
            if arm != "nothing focusable" {
                win(&mut s, Role::Normal, 400, 0, 100, 100);
            }
            let mut r = InputRouter::new(SCREEN);
            r.register_hotkey(MgrHotkey { id: 9, mods: MOD_SUPER, code: KEY_2 }).unwrap();
            grip(&mut r, &mut s, w, 299, 199, RESIZE_RIGHT | RESIZE_BOTTOM);
            go(&mut r, &mut s, drag(40, 30));
            if arm == "hotkey" {
                // Consumed while the window is still on screen: the gesture survives this.
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
        // The rule a manager and a drag have to agree on. Refused rather than silently
        // overridden: a `Place` that landed mid-drag would be undone by the next motion, so a
        // manager racing the pointer would appear to work.
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
        // **The bound that keeps this off the manager's queue.** That queue does not coalesce
        // and evicts its oldest when full, so a geometry record per motion would push a
        // `WindowCreated` off the front of a manager's view of the world (PR #247 review).
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
        // A client exiting with the button held is ordinary. Left set, the flag would refuse a
        // `Place` for that id for the compositor's life, since ids are never reused.
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
        // Panels do not take focus, so raising one on click would let a stray click on the
        // clock permanently cover a window with no way to get it back.
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
        // Order matters: a client that gets enter-then-leave concludes the cursor is
        // outside when it is in fact inside.
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
        // Not just `POINTER_BUTTON`. A client implementing the standard "on motion, if a
        // button is down, move the object" needs it on motion, and a crossing generated by
        // the router — which arrives as no event at all — must agree with its neighbours.
        let mut s = WindowStack::new();
        win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);

        let out = go(&mut r, &mut s, Logical::Button {
            button: BTN_LEFT,
            pressed: true,
            buttons: 1,
            modifiers: MOD_SHIFT,
        });
        for o in &out {
            let Outbound::Pointer { event, .. } = o else { unreachable!() };
            assert_eq!((event.buttons, event.modifiers), (1, MOD_SHIFT), "kind {}", event.kind);
        }

        let out = go(&mut r, &mut s, Logical::Motion {
            dx: 200,
            dy: 0,
            buttons: 1,
            modifiers: MOD_SHIFT,
        });
        let Outbound::Pointer { event, .. } = out
            .iter()
            .find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION))
            .cloned()
            .expect("a motion")
        else {
            unreachable!()
        };
        assert_eq!(
            (event.buttons, event.modifiers),
            (1, MOD_SHIFT),
            "a shift-drag is expressible from one record"
        );

        // And the release's crossing into `b`, which the router synthesises, agrees.
        let out = go(&mut r, &mut s, Logical::Button {
            button: BTN_LEFT,
            pressed: false,
            buttons: 0,
            modifiers: MOD_SHIFT,
        });
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
        // Nothing has moved the cursor, so `inside` is still `None` — and once the press
        // takes the grab the crossing pass early-returns. Deriving it only on release means
        // a client that arms hover state on `POINTER_ENTER` processes a whole click for a
        // pointer it believes is elsewhere (PR #180 review, finding 7).
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
        let Outbound::Pointer { event } =
            out.iter().find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION)).cloned().expect("a motion")
        else {
            unreachable!()
        };
        assert_eq!(event.window, w, "the record names the window it was routed to");
        assert_eq!((event.x, event.y), (55, 35), "screen (255,135) minus origin (200,100)");
    }

    #[test]
    fn a_drag_out_of_a_window_still_delivers_the_release_to_it() {
        // Without the implicit grab the release lands on whatever is under the cursor at
        // the time — or nothing — and the client believes the button is held forever.
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
        assert!(
            moved.iter().all(|o| o.window() == a),
            "everything mid-drag still goes to the grab holder"
        );

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
        // The reason `PointerEvent`'s coordinates are signed. Clamping them to zero would
        // make every leftward drag look like it stopped at the window's edge.
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
        // A drag that ends over a different window must leave the grab holder and enter
        // the new one — otherwise the client keeps believing the cursor is inside it.
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
                Outbound::Pointer { event }
                    if event.kind == POINTER_LEAVE || event.kind == POINTER_ENTER =>
                {
                    Some((event.window, event.kind))
                }
                _ => None,
            })
            .collect();
        assert_eq!(crossings, [(a, POINTER_LEAVE), (b, POINTER_ENTER)]);
    }

    #[test]
    fn a_dropped_batch_ends_the_grab() {
        // The lost event may have been the release. A grab that outlives its button never
        // ends, and every later click goes to the wrong window for the rest of the session.
        let mut s = WindowStack::new();
        let a = win(&mut s, Role::Normal, 0, 0, 100, 100);
        let b = win(&mut s, Role::Normal, 200, 0, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);
        go(&mut r, &mut s, button(true));
        go(&mut r, &mut s, drag(200, 0));
        assert_eq!(r.grab(), Some(a));

        go(&mut r, &mut s, Logical::Dropped);
        assert_eq!(r.grab(), None);
        let out = go(&mut r, &mut s, motion(1, 0));
        assert!(out.iter().all(|o| o.window() == b), "later events follow the cursor again");
    }

    #[test]
    fn destroying_the_grab_holder_does_not_route_to_a_dead_id() {
        // The router is not on the destroy path, so it has to notice by asking the stack.
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
        // `i32` deltas accumulate, and a stuck mouse really does send them without end.
        // The direction matters: leftward cannot overflow, because the cursor is already
        // clamped to `>= 0` and `0 + i32::MIN` is representable. **Rightward can** — the
        // cursor sits at 639, and `639 + i32::MAX` wraps to a large negative, which the
        // clamp then obediently pins to the *left* edge. A first version of this test
        // pushed left three times, overflowed nothing, and passed against `wrapping_add`.
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
        // Click-to-focus must not eat the click: the window that comes to the front is the
        // window that hears about the press that brought it there.
        //
        // Note the *ordering* here is not load-bearing, though it looks as if it should be.
        // Hit-testing takes the topmost window containing the point, and a raise moves that
        // same window further up — so re-testing after the raise gives the identical answer.
        // A break that took the grab after the restack passed every test here, correctly.
        let mut s = WindowStack::new();
        let lower = win(&mut s, Role::Normal, 0, 0, 640, 480);
        win(&mut s, Role::Normal, 300, 300, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 50, 50);

        let out = go(&mut r, &mut s, button(true));
        assert!(out.iter().all(|o| o.window() == lower));
        assert_eq!(r.grab(), Some(lower));
    }
}
