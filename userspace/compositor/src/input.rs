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
    KeyEvent, POINTER_BUTTON, POINTER_ENTER, POINTER_LEAVE, POINTER_MOTION, POINTER_PRESSED,
    PointerEvent,
};

use crate::WindowStack;

/// One record, addressed to one window.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Outbound {
    /// A `Surface::KeyEvent` for `window`.
    Key {
        /// Which window it is addressed to.
        window: u32,
        /// The record to send.
        event: KeyEvent,
    },
    /// A `Surface::PointerEvent` for `window`.
    Pointer {
        /// Which window it is addressed to.
        window: u32,
        /// The record to send.
        event: PointerEvent,
    },
}

impl Outbound {
    /// The window this record is addressed to.
    pub fn window(&self) -> u32 {
        match self {
            Outbound::Key { window, .. } | Outbound::Pointer { window, .. } => *window,
        }
    }
}

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
        Self { pointer, screen, inside: None, grab: None }
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

    /// Route one logical event, appending what to send to `out`.
    ///
    /// Returns `true` if the stack was restacked — a press raises the window it lands on,
    /// and the caller has to recompose when that happens.
    pub fn route(
        &mut self,
        ev: &Logical,
        stack: &mut WindowStack,
        out: &mut Vec<Outbound>,
    ) -> bool {
        // A window can be destroyed between one event and the next, and the router is not
        // on the destroy path. Dropping stale ids here rather than wiring a callback keeps
        // the two apart and cannot go out of date: the stack is the authority, so ask it.
        if self.inside.is_some_and(|w| stack.window(w).is_none()) {
            self.inside = None;
        }
        if self.grab.is_some_and(|w| stack.window(w).is_none()) {
            self.grab = None;
        }

        match *ev {
            Logical::Key { keycode, pressed, modifiers } => {
                let Some(window) = stack.focus_candidate() else {
                    // Nothing focusable. Dropping beats delivering to the pointer's window,
                    // which would make typing depend on where the cursor happens to rest.
                    return false;
                };
                out.push(Outbound::Key {
                    window,
                    event: KeyEvent {
                        keycode,
                        pressed: u16::from(pressed),
                        modifiers,
                        _pad: 0,
                    },
                });
                false
            }

            Logical::Motion { dx, dy } => {
                self.move_by(dx, dy);
                self.update_crossing(stack, out);
                if let Some(window) = self.target(stack) {
                    self.emit(window, POINTER_MOTION, 0, 0, 0, stack, out);
                }
                false
            }

            Logical::Button { button, pressed, buttons, .. } => {
                let mut restacked = false;
                if pressed && self.grab.is_none() {
                    // The press decides the grab. Taking it before the raise reads as the
                    // careful order, but it is only the clearer one: hit-testing picks the
                    // topmost window *containing the point*, and raising that window leaves
                    // it topmost there too, so re-testing afterwards cannot differ.
                    self.grab = self.hit(stack);
                    if let Some(window) = self.grab
                        && stack.window(window).is_some_and(|w| w.role.takes_focus())
                    {
                        // Click to focus. `focus_candidate` is topmost-focusable, so the
                        // raise *is* the focus change — no second piece of state to
                        // disagree with the stack about who has focus.
                        restacked = stack.raise(window).is_ok();
                    }
                }

                if let Some(window) = self.target(stack) {
                    let flags = if pressed { POINTER_PRESSED } else { 0 };
                    self.emit(window, POINTER_BUTTON, button, buttons, flags, stack, out);
                }

                if !pressed && buttons == 0 {
                    // Last button up: the grab ends, and the cursor may have been dragged
                    // somewhere else entirely while it was held, so re-derive the crossing.
                    self.grab = None;
                    self.update_crossing(stack, out);
                }
                restacked
            }

            Logical::Dropped => {
                // Buttons are no longer trustworthy — a release may be the event that was
                // lost, and a grab that outlives its button never ends. `libinput` has
                // already reset what it accumulated; this is the same reset one layer up.
                if self.grab.take().is_some() {
                    self.update_crossing(stack, out);
                }
                false
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

    /// The topmost window containing the cursor, whatever its role.
    fn hit(&self, stack: &WindowStack) -> Option<u32> {
        stack
            .windows()
            .iter()
            .rev()
            .find(|w| w.bounds().contains(self.pointer.x, self.pointer.y))
            .map(|w| w.id)
    }

    /// Who a pointer event goes to: the grab holder if there is one, else what is under the
    /// cursor.
    fn target(&self, stack: &WindowStack) -> Option<u32> {
        self.grab.or_else(|| self.hit(stack))
    }

    /// Emit leave/enter if the cursor changed windows. A no-op while grabbed.
    fn update_crossing(&mut self, stack: &WindowStack, out: &mut Vec<Outbound>) {
        if self.grab.is_some() {
            // Crossings during a drag would tell a window the cursor left while it is still
            // receiving that cursor's events — two contradictory statements at once.
            return;
        }
        let now = self.hit(stack);
        if now == self.inside {
            return;
        }
        if let Some(old) = self.inside {
            self.emit(old, POINTER_LEAVE, 0, 0, 0, stack, out);
        }
        self.inside = now;
        if let Some(new) = now {
            self.emit(new, POINTER_ENTER, 0, 0, 0, stack, out);
        }
    }

    /// Build one pointer record with the cursor in `window`'s coordinates.
    fn emit(
        &self,
        window: u32,
        kind: u16,
        button: u16,
        buttons: u16,
        flags: u16,
        stack: &WindowStack,
        out: &mut Vec<Outbound>,
    ) {
        let Some(w) = stack.window(window) else {
            return;
        };
        let origin = w.bounds().origin;
        let event = PointerEvent {
            kind,
            button,
            buttons,
            flags,
            // Signed and unclamped: under a grab the cursor is routinely outside the
            // window, and reporting a clamped edge position would make a drag look like it
            // stopped at the border.
            x: self.pointer.x.saturating_sub(origin.x),
            y: self.pointer.y.saturating_sub(origin.y),
        };
        out.push(Outbound::Pointer { window, event });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::surface::{
        AttachBufferRequest, CommitRequest, CreateWindowRequest, Edge, Role,
        SURFACE_FORMAT_XRGB8888,
    };

    const SCREEN: Rect = Rect::new(0, 0, 640, 480);
    const BTN_LEFT: u16 = 0x110;

    /// A window at `(x, y)` of `w × h`, with a committed buffer so `bounds()` is its size.
    fn win(stack: &mut WindowStack, role: Role, x: i32, y: i32, w: u32, h: u32) -> u32 {
        let id = stack
            .create(&CreateWindowRequest { role, width: w, height: h })
            .expect("create");
        stack.set_origin(id, Point::new(x, y)).expect("origin");
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
        go(r, s, Logical::Motion { dx: x - p.x, dy: y - p.y });
    }

    fn key(keycode: u16, pressed: bool) -> Logical {
        Logical::Key { keycode, pressed, modifiers: 0 }
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
            out.iter().any(|o| matches!(o, Outbound::Pointer { window, .. } if *window == panel)),
            "the panel got the click"
        );
        go(&mut r, &mut s, button(false));

        let out = go(&mut r, &mut s, key(30, true));
        assert_eq!(out[0].window(), term, "but the keystroke still goes to the terminal");
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
        let restacked = r.route(&button(true), &mut s, &mut out);
        assert!(restacked, "the caller must recompose");
        assert_eq!(s.focus_candidate(), Some(lower), "the raise *is* the focus change");
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
        assert!(!r.route(&button(true), &mut s, &mut out), "no restack");
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
        let out = go(&mut r, &mut s, Logical::Motion { dx: 200, dy: 0 });
        let kinds: Vec<_> = out
            .iter()
            .map(|o| match o {
                Outbound::Pointer { window, event } => (*window, event.kind),
                Outbound::Key { .. } => unreachable!("no keys here"),
            })
            .collect();
        assert_eq!(
            kinds,
            [(a, POINTER_LEAVE), (b, POINTER_ENTER), (b, POINTER_MOTION)],
            "leave, then enter, then the motion that caused both"
        );
    }

    #[test]
    fn coordinates_are_window_local() {
        let mut s = WindowStack::new();
        let w = win(&mut s, Role::Normal, 200, 100, 100, 100);
        let mut r = InputRouter::new(SCREEN);
        warp(&mut r, &mut s, 250, 130);

        let out = go(&mut r, &mut s, Logical::Motion { dx: 5, dy: 5 });
        let Outbound::Pointer { window, event } =
            out.iter().find(|o| matches!(o, Outbound::Pointer { event, .. } if event.kind == POINTER_MOTION)).copied().expect("a motion")
        else {
            unreachable!()
        };
        assert_eq!(window, w);
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
        let moved = go(&mut r, &mut s, Logical::Motion { dx: 200, dy: 0 });
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
        let out = go(&mut r, &mut s, Logical::Motion { dx: -100, dy: 0 });
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
        go(&mut r, &mut s, Logical::Motion { dx: 200, dy: 0 });
        assert_eq!(r.inside(), Some(a), "suppressed while grabbed: still a, though over b");

        let out = go(&mut r, &mut s, button(false));
        assert_eq!(r.inside(), Some(b));
        let crossings: Vec<_> = out
            .iter()
            .filter_map(|o| match o {
                Outbound::Pointer { window, event }
                    if event.kind == POINTER_LEAVE || event.kind == POINTER_ENTER =>
                {
                    Some((*window, event.kind))
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
        go(&mut r, &mut s, Logical::Motion { dx: 200, dy: 0 });
        assert_eq!(r.grab(), Some(a));

        go(&mut r, &mut s, Logical::Dropped);
        assert_eq!(r.grab(), None);
        let out = go(&mut r, &mut s, Logical::Motion { dx: 1, dy: 0 });
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
        let out = go(&mut r, &mut s, Logical::Motion { dx: 200, dy: 0 });
        assert!(out.iter().all(|o| o.window() == b), "nothing addressed to the dead window");
        assert_eq!(r.grab(), None);
    }

    #[test]
    fn the_cursor_stops_at_the_screen_edge() {
        let mut s = WindowStack::new();
        let mut r = InputRouter::new(SCREEN);
        go(&mut r, &mut s, Logical::Motion { dx: -10_000, dy: -10_000 });
        assert_eq!(r.pointer(), Point::new(0, 0));
        go(&mut r, &mut s, Logical::Motion { dx: 10_000, dy: 10_000 });
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
            go(&mut r, &mut s, Logical::Motion { dx: i32::MAX, dy: i32::MAX });
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
