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
    KeyEvent, MAX_HOTKEYS, MgrHotkey, POINTER_BUTTON, POINTER_ENTER, POINTER_LEAVE,
    POINTER_MOTION, POINTER_PRESSED, PointerEvent,
};

use crate::{Damage, WindowStack};
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
    /// The event was taken by a registered chord and reached no window.
    pub consumed: bool,
}

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
            grab_button: 0,
            grab_broken: false,
            buttons: 0,
            modifiers: 0,
            hotkeys: Vec::new(),
            fired: Vec::new(),
            consumed: Vec::new(),
            held: Vec::new(),
        }
    }

    /// Where the cursor is, in screen coordinates.
    pub fn pointer(&self) -> Point {
        self.pointer
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

        match *ev {
            Logical::Key { keycode, pressed, modifiers } => {
                // **Before focus routing, and consuming rather than copying.** A chord that
                // also reached the focused window would type into it — `Super+2` would switch
                // desktops *and* put a `2` in the terminal.
                if self.take_as_hotkey(keycode, pressed, modifiers) {
                    return Routed { restacked: None, consumed: true };
                }
                let Some(window) = stack.focus_candidate() else {
                    // Nothing focusable. Dropping beats delivering to the pointer's window,
                    // which would make typing depend on where the cursor happens to rest.
                    return Routed::default();
                };
                // TODO(focus-change-key-balance): a key held across a focus change is delivered
                // to one window and released to another, or to none. Harmless today because
                // `KeyEvent` carries `modifiers` on every record, so no client accumulates
                // them — but it is the same unbalanced-press shape the chord rules above exist
                // to prevent, reached by a different route.
                out.push(Outbound::Key {
                    event: KeyEvent::new(window, keycode, u16::from(pressed), modifiers),
                });
                Routed::default()
            }

            Logical::Motion { dx, dy, .. } => {
                self.move_by(dx, dy);
                self.update_crossing(stack, out);
                if let Some(window) = self.target(stack) {
                    self.emit(window, POINTER_MOTION, 0, 0, stack, out);
                }
                Routed::default()
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
                Routed { restacked, consumed: false }
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
                Routed::default()
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
        AttachBufferRequest, CommitRequest, CreateWindowRequest, Edge, MOD_SHIFT, Role,
        SURFACE_FORMAT_XRGB8888,
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
            .copied()
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
            out.iter().find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION)).copied().expect("a motion")
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
            .copied()
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
