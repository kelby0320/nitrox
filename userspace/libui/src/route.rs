//! Where an event goes inside a window.
//!
//! The compositor decides which *window* has focus and which window a click lands on. This
//! decides which *widget* — and the two must never share a field. Conflating them is the
//! classic source of typing arriving in the wrong place: a window that regains focus has to
//! put the caret back where it was, so widget focus outlives window focus rather than being
//! derived from it.
//!
//! ## The rules, and where each came from
//!
//! **Hit-testing is topmost-first**, the reverse of paint order, so a menu overlaid on a grid
//! takes the click that visually landed on it.
//!
//! **A press captures.** Every pointer event up to the release of the last button goes to the
//! widget the press landed on, even after the cursor leaves it. This is the compositor's rule
//! one layer down, and it exists for the same reason: a drag that ends outside a scrollbar
//! must still deliver the release, or the scrollbar believes it is still being dragged
//! forever.
//!
//! **A key goes to the focused widget, then bubbles.** Unhandled keys walk up the ancestors,
//! then reach the application — which is how a menu accelerator works without every widget
//! knowing about menus.
//!
//! ## What this module does not do
//!
//! It does not own the pointer's position — the compositor does, and `PointerEvent` carries
//! window-local coordinates already. It does not decide *which window* anything goes to.
//! And it holds no widget interaction state; that is the retained tree's, and Part C's
//! widgets are what will put anything in it.

use alloc::vec::Vec;

use libdraw::geom::{Point, Rect};
use librsproto::surface::{
    KeyEvent, POINTER_BUTTON, POINTER_ENTER, POINTER_LEAVE, POINTER_PRESSED, PointerEvent,
};

use crate::diff::{Tree, Widget};
use crate::element::Element;
use crate::layout::Layout;

/// A path from the root to a widget: the child index at each level.
///
/// A path rather than an id, because routing walks the tree and needs the ancestors on the
/// way — bubbling is "the path minus its last element", repeatedly. The id it resolves to is
/// what survives a reordering; the path is how this frame gets there.
pub type Path = Vec<usize>;

/// Which widget has keyboard focus inside this window, what the pointer is doing, and the
/// last thing the compositor said about the window.
///
/// It holds all three, but **`focus` and `window_focused` are never merged into one field**:
/// they come from two places and answer two questions, and [`Focus`] is what hands both to a
/// caller at once.
pub struct Router {
    focus: Option<u64>,
    capture: Option<u64>,
    /// Which button took that capture.
    ///
    /// A click is *that* button's release, not any release. Without this, left-pressing a
    /// widget and then clicking the right button over it fires `on_press` — and the left
    /// release fires it again, so one interaction produces two clicks (PR #184 re-review,
    /// finding 2).
    capture_button: Option<u16>,
    /// The widget the cursor is inside, for synthesising crossings.
    inside: Option<u64>,
    window_focused: bool,
}

/// The two focus facts a widget needs in order to paint itself, kept apart on purpose.
///
/// Widget focus is the toolkit's and window focus is the compositor's. Conflating them is the
/// classic source of typing arriving in the wrong place, so they are two fields here and
/// [`is_active`](Self::is_active) is the only thing that combines them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Focus {
    /// This widget holds keyboard focus within its window.
    pub widget_focused: bool,
    /// The compositor says this window has the keyboard.
    pub window_focused: bool,
}

impl Focus {
    /// Whether a caret should blink and a focus ring should be drawn.
    ///
    /// **Both, and that is the whole point of the type.** Two fields from two sources: a
    /// window that lost the keyboard must stop blinking, and must not forget where the caret
    /// was.
    pub fn is_active(&self) -> bool {
        self.widget_focused && self.window_focused
    }
}

impl Default for Router {
    /// Delegates to [`Router::new`].
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    /// A router with nothing focused and nothing captured.
    ///
    /// `window_focused` starts **true**: a window is mapped because something wanted it, and
    /// the compositor's first `FocusEvent` corrects this within a frame. Starting `false`
    /// would make a client's first paint dim.
    pub fn new() -> Self {
        Self {
            focus: None,
            capture: None,
            capture_button: None,
            inside: None,
            window_focused: true,
        }
    }

    /// The focused widget's id, if any.
    pub fn focused(&self) -> Option<u64> {
        self.focus
    }

    /// The widget holding the pointer capture, if a button is down.
    pub fn capture(&self) -> Option<u64> {
        self.capture
    }

    /// The widget the cursor is inside, if any.
    pub fn inside(&self) -> Option<u64> {
        self.inside
    }

    /// Whether the compositor says this window has the keyboard.
    pub fn window_focused(&self) -> bool {
        self.window_focused
    }

    /// Record a `Surface::FocusEvent`.
    ///
    /// **Widget focus is untouched.** Returning to a window must put the caret back where it
    /// was; discarding it would make every window switch lose the user's place.
    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    /// How `widget` should paint itself.
    pub fn focus_of(&self, widget: u64) -> Focus {
        Focus {
            widget_focused: self.focus == Some(widget),
            window_focused: self.window_focused,
        }
    }

    /// Give focus to `widget` if the tree says it can take it.
    ///
    /// Refused rather than silently accepted for a widget that is not focusable, or focus
    /// could land somewhere Tab can never reach and the user would have no way out.
    pub fn focus(&mut self, tree: &Tree, element: &Element<impl Sized>, widget: u64) -> bool {
        if focusable_ids(tree, element).contains(&widget) {
            self.focus = Some(widget);
            true
        } else {
            false
        }
    }

    /// Move focus to the next focusable widget in tree order, wrapping.
    ///
    /// Returns the newly focused widget. With nothing focusable this is `None` and focus is
    /// cleared — a window whose only focusable widget was just destroyed must not keep
    /// pointing at it.
    pub fn focus_next(&mut self, tree: &Tree, element: &Element<impl Sized>) -> Option<u64> {
        self.step_focus(tree, element, 1)
    }

    /// Move focus to the previous focusable widget in tree order, wrapping.
    pub fn focus_prev(&mut self, tree: &Tree, element: &Element<impl Sized>) -> Option<u64> {
        self.step_focus(tree, element, -1)
    }

    fn step_focus(
        &mut self,
        tree: &Tree,
        element: &Element<impl Sized>,
        by: isize,
    ) -> Option<u64> {
        let order = focusable_ids(tree, element);
        if order.is_empty() {
            self.focus = None;
            return None;
        }
        let next = match self.focus.and_then(|f| order.iter().position(|&id| id == f)) {
            // Wrapping in both directions, computed on `isize` so that stepping back from
            // the first lands on the last rather than underflowing to nothing.
            Some(i) => (i as isize + by).rem_euclid(order.len() as isize) as usize,
            // Nothing focused, or focus pointing at a widget that no longer exists: start
            // from the appropriate end rather than refusing to move.
            None if by >= 0 => 0,
            None => order.len() - 1,
        };
        self.focus = Some(order[next]);
        self.focus
    }

    /// Drop focus, capture and the hovered widget if they name widgets the tree no longer
    /// has.
    ///
    /// Called after every diff. The router is not on the destroy path, and asking the tree
    /// cannot go out of date the way a callback can — the same reasoning the compositor's
    /// `InputRouter` uses for window ids.
    pub fn prune(&mut self, tree: &Tree) {
        if self.focus.is_some_and(|id| find_by_id(tree.root(), id).is_none()) {
            self.focus = None;
        }
        if self.capture.is_some_and(|id| find_by_id(tree.root(), id).is_none()) {
            self.capture = None;
            self.capture_button = None;
        }
        if self.inside.is_some_and(|id| find_by_id(tree.root(), id).is_none()) {
            self.inside = None;
        }
    }

    /// Route a key event, returning the message it produced.
    ///
    /// The focused widget's `on_key` fires first; if it has none **or returns `None`**, the
    /// event bubbles to each ancestor in turn. Declining matters: a focused text field with a
    /// handler would otherwise swallow every accelerator, and "unhandled keys reach the menu"
    /// is the reason bubbling exists at all.
    ///
    /// With nothing focused the event is dropped rather than sent to whatever the pointer
    /// happens to be over, which would make typing depend on where the mouse rests. And a key
    /// nothing claims returns `None` — the caller *is* the application, and it still holds
    /// the event it passed in, so there is no separate "reaches the application" path.
    pub fn key<Msg>(
        &self,
        tree: &Tree,
        element: &Element<Msg>,
        event: KeyEvent,
    ) -> Option<Msg> {
        let focused = self.focus?;
        let path = path_to_id(tree.root(), focused)?;
        // From the focused widget outward: `path`, then its parent, and so on to the root.
        for depth in (0..=path.len()).rev() {
            let e = element_at(element, &path[..depth])?;
            if let Some(f) = e.on_key
                && let Some(msg) = f(event)
            {
                return Some(msg);
            }
        }
        None
    }

    /// Route a pointer event, returning the messages it produced.
    ///
    /// Returns `(messages, hit)` — the widget it went to, which a caller wants for
    /// diagnostics and which the tests assert on.
    pub fn pointer<Msg: Clone>(
        &mut self,
        tree: &Tree,
        element: &Element<Msg>,
        layout: &Layout,
        event: PointerEvent,
    ) -> (Vec<Msg>, Option<u64>) {
        let at = Point::new(event.x, event.y);
        let pressed = event.kind == POINTER_BUTTON && event.flags & POINTER_PRESSED != 0;
        let released = event.kind == POINTER_BUTTON && event.flags & POINTER_PRESSED == 0;

        // **The press decides the capture, from where the cursor is.** Anything else and a
        // drag that leaves the widget stops being that widget's drag.
        //
        // Whether *this* press is the one that opened it is read before the assignment, and is
        // what `on_press_down` fires on — see the dispatch below.
        let opens_capture = pressed && self.capture.is_none();
        if opens_capture {
            self.capture = hit_test(tree.root(), layout, at);
            self.capture_button = Some(event.button);
        }
        let target = self.capture.or_else(|| hit_test(tree.root(), layout, at));

        let mut out = Vec::new();

        // **The compositor's crossings are inputs, not events to forward.** Its `ENTER` and
        // `LEAVE` are about the *window*; a widget only ever sees crossings this router
        // synthesises about *widgets*. Forwarding the window one as well handed a widget two
        // enters for one cursor movement, which is the layering leaking through.
        let window_crossing = matches!(event.kind, POINTER_ENTER | POINTER_LEAVE);
        let left_window = event.kind == POINTER_LEAVE;

        // Crossings first, so a widget hears "the cursor entered me" before the motion that
        // brought it in. Suppressed while captured, for the compositor's own reason: telling
        // a widget the cursor left while it is still receiving that cursor's events is two
        // contradictory statements at once.
        if self.capture.is_none() {
            // The cursor leaving the window leaves every widget in it, wherever the last
            // coordinates happened to point.
            let now = if left_window { None } else { hit_test(tree.root(), layout, at) };
            if now != self.inside {
                if let Some(old) = self.inside {
                    self.emit_crossing(tree, element, layout, old, POINTER_LEAVE, event, &mut out);
                }
                self.inside = now;
                if let Some(new) = now {
                    self.emit_crossing(tree, element, layout, new, POINTER_ENTER, event, &mut out);
                }
            }
        }

        if let Some(id) = target
            && let Some(path) = path_to_id(tree.root(), id)
        {
            // **The handler may be on an ancestor of what was hit, so look up the path.**
            // `hit_test` returns the *deepest* widget under the cursor, and a widget that draws
            // itself out of smaller pieces puts its handler on the outside: `widget::button` is
            // a `Stack` carrying `on_press`, with a `fill` and a `text` inside it. Clicking the
            // label therefore hit the label, which handles nothing — so **no button in this
            // toolkit was clickable**, anywhere, until this walked up to find the handler.
            //
            // Nothing caught it because the routing tests attach handlers to leaves and the
            // gates only ever clicked `custom` nodes, which are leaves. Found by `nxterm`'s
            // menu becoming a real popup and the gate clicking an item (M6 C3).
            let nearest = |has: &dyn Fn(&Element<Msg>) -> bool| -> Option<(usize, &Element<Msg>)> {
                (0..=path.len()).rev().find_map(|n| {
                    let e = element_at(element, &path[..n])?;
                    has(e).then_some((n, e))
                })
            };

            if !window_crossing
                && let Some((n, e)) = nearest(&|e: &Element<Msg>| e.on_pointer.is_some())
                && let Some(f) = e.on_pointer
                && let Some(l) = layout_at(layout, &path[..n])
            {
                out.push(f(localise(event, l.rect)));
            }

            // **A press going down, for gestures decided before the release.** A drag is the
            // motivating one: a window follows the pointer from the moment the button lands on
            // its title bar, and `on_press` — a *click*, which is a release inside the widget
            // that took the press — would start the move after the user had finished making it.
            //
            // **A nearer `on_press` shadows it**, and that rule is the whole discrimination a
            // title bar needs: its buttons handle clicks, so pressing close does not also drag
            // the window. `nearest` walks from the deepest element outwards, so a larger index
            // is the nearer handler.
            // **Only the press that opened the capture**, not every press while it is held.
            // While a capture is in force `target` is the *captured* widget rather than the one
            // under the cursor, so a second button pressed mid-gesture re-fired this for a
            // widget the pointer may not even be over — a right-click during a window drag sent
            // a second `Drag`, and the shadowing rule cannot help, because it walks the captured
            // node's path rather than the pointer's (PR #248 review, blocking 1).
            if opens_capture
                && let Some((n_down, e_down)) = nearest(&|e: &Element<Msg>| e.on_press_down.is_some())
                && let Some(msg) = e_down.on_press_down.clone()
            {
                let shadowed = nearest(&|e: &Element<Msg>| e.on_press.is_some())
                    .is_some_and(|(n_click, _)| n_click > n_down);
                if !shadowed {
                    out.push(msg);
                }
            }
            let e = match element_at(element, &path) {
                Some(e) => e,
                None => return (out, target),
            };
            // A **click** is a release inside the widget that took the press. Releasing
            // outside is a cancel, which is why this is not simply "on release".
            //
            // **Compared in window coordinates, with nothing added to either side.** A
            // layout rect is already absolute within the window — `layout` is handed the
            // window's bounds and every child is placed inside its parent's rect, not
            // relative to it — and `event.x`/`event.y` arrive in that same space, which is
            // exactly what `hit_test` relies on one screen up. An earlier version added
            // `l.rect.origin` here, which agrees with `hit_test` only for a widget at the
            // window's top-left corner: everything in a second column, below a menu bar, or
            // inside a `padding` silently lost its clicks (PR #184 review, finding 1).
            if released
                && self.capture_button == Some(event.button)
                && let Some((n, pe)) = nearest(&|e: &Element<Msg>| e.on_press.is_some())
                && let Some(msg) = pe.on_press.clone()
                && let Some(l) = layout_at(layout, &path[..n])
                && l.rect.contains(event.x, event.y)
            {
                out.push(msg);
            }
            // Click to focus, but only where focus can land — **on the widget actually hit,
            // not on an ancestor**.
            //
            // The walk-up above is right for `on_press` and `on_pointer`, where a composite
            // widget's handler lives on its outside and the click landed on one of its parts.
            // It is wrong here, and the difference is that focus is a *claim on the keyboard*
            // rather than a response to this event: `widget::button` marks its outer `Stack`
            // focusable but gives it no `on_key`, so walking up moved focus from the terminal
            // grid to a widget that claims keys and handles none — and nothing puts it back.
            // Clicking the menu bar killed typing until you clicked the grid again
            // (PR #223 review, finding 1).
            //
            // Leaving it on the hit node means clicking a `button` moves focus nowhere at all,
            // because the label under the cursor is not focusable. That is the behaviour that
            // shipped before this, and making a button take focus needs it to *do* something
            // with the keyboard first.
            if pressed && e.focusable {
                self.focus = Some(id);
            }
        }

        if released && event.buttons == 0 {
            // The cursor may have been dragged somewhere else entirely while the button was
            // held, so re-derive the crossing now that it is not suppressed.
            self.capture = None;
            self.capture_button = None;
            let now = hit_test(tree.root(), layout, at);
            if now != self.inside {
                if let Some(old) = self.inside {
                    self.emit_crossing(tree, element, layout, old, POINTER_LEAVE, event, &mut out);
                }
                self.inside = now;
                if let Some(new) = now {
                    self.emit_crossing(tree, element, layout, new, POINTER_ENTER, event, &mut out);
                }
            }
        }
        (out, target)
    }
}

impl Router {
    /// Hand a synthesised crossing to `id`'s handler, if it has one.
    fn emit_crossing<Msg>(
        &self,
        tree: &Tree,
        element: &Element<Msg>,
        layout: &Layout,
        id: u64,
        kind: u16,
        cause: PointerEvent,
        out: &mut Vec<Msg>,
    ) {
        let Some(path) = path_to_id(tree.root(), id) else { return };
        // **The same walk-up the press and motion paths do.** A composite widget's handler is
        // on its outside, so requiring it on the exact node hit would give it motion — which
        // walks up — and never a crossing, which did not. A `button` driving its hover state
        // from `on_pointer` would latch hovered on entry and never hear that the cursor left
        // (PR #223 review, finding 5).
        let Some((n, e)) = (0..=path.len())
            .rev()
            .find_map(|n| element_at(element, &path[..n]).filter(|e| e.on_pointer.is_some()).map(|e| (n, e)))
        else {
            return;
        };
        let Some(f) = e.on_pointer else { return };
        let Some(l) = layout_at(layout, &path[..n]) else { return };
        // The state that was true when the cursor crossed, from the event that moved it —
        // a crossing has no buttons or modifiers of its own, and inventing zeroes would tell
        // a widget the button it is about to be dragged with is not held.
        let ev = PointerEvent {
            kind,
            button: 0,
            buttons: cause.buttons,
            flags: 0,
            modifiers: cause.modifiers,
            _pad: 0,
            ..localise(cause, l.rect)
        };
        out.push(f(ev));
    }
}

/// An event in `rect`'s coordinates.
///
/// **The only place a coordinate space changes.** Everything internal — hit-testing, the
/// click's containment check — stays in window coordinates, because that is what a
/// `PointerEvent` arrives in and what a `Layout` rectangle is expressed in. Translating in
/// two places is how a release ended up tested against a point offset by the widget's own
/// origin, so every click away from the window's corner was silently cancelled (PR #184
/// review, finding 1). Handlers get widget-local because a widget cannot otherwise know
/// where in *itself* it was clicked; the coordinates are signed, and mid-capture they are
/// routinely negative.
fn localise(event: PointerEvent, rect: Rect) -> PointerEvent {
    PointerEvent {
        x: event.x.saturating_sub(rect.origin.x),
        y: event.y.saturating_sub(rect.origin.y),
        ..event
    }
}

/// Every focusable widget's id, in tree order.
fn focusable_ids(tree: &Tree, element: &Element<impl Sized>) -> Vec<u64> {
    let mut out = Vec::new();
    if let Some(root) = tree.root() {
        collect_focusable(root, element, &mut out);
    }
    out
}

fn collect_focusable(w: &Widget, e: &Element<impl Sized>, out: &mut Vec<u64>) {
    if e.focusable {
        out.push(w.id);
    }
    for (cw, ce) in w.children.iter().zip(e.children()) {
        collect_focusable(cw, ce, out);
    }
}

/// The widget with `id`, if the tree has one.
fn find_by_id(w: Option<&Widget>, id: u64) -> Option<&Widget> {
    let w = w?;
    if w.id == id {
        return Some(w);
    }
    w.children.iter().find_map(|c| find_by_id(Some(c), id))
}

/// The index path from the root to `id`.
fn path_to_id(root: Option<&Widget>, id: u64) -> Option<Path> {
    fn walk(w: &Widget, id: u64, path: &mut Path) -> bool {
        if w.id == id {
            return true;
        }
        for (i, c) in w.children.iter().enumerate() {
            path.push(i);
            if walk(c, id, path) {
                return true;
            }
            path.pop();
        }
        false
    }
    let root = root?;
    let mut path = Path::new();
    walk(root, id, &mut path).then_some(path)
}

/// The id of the widget at `path`, for the ancestor a handler was actually found on.
/// The element at `path`, walking [`Element::children`] order.
fn element_at<'a, Msg>(e: &'a Element<Msg>, path: &[usize]) -> Option<&'a Element<Msg>> {
    let mut cur = e;
    for &i in path {
        cur = cur.children().nth(i)?;
    }
    Some(cur)
}

/// The layout at `path`.
fn layout_at<'a>(l: &'a Layout, path: &[usize]) -> Option<&'a Layout> {
    let mut cur = l;
    for &i in path {
        cur = cur.children.get(i)?;
    }
    Some(cur)
}

/// The topmost widget containing `at`, in window coordinates.
///
/// Children are searched in reverse — the last child paints on top, so it is hit first — and
/// a parent is only a hit if none of its children was. Zero-extent rectangles can contain
/// nothing and fall out of `contains` naturally.
fn hit_test(root: Option<&Widget>, layout: &Layout, at: Point) -> Option<u64> {
    fn walk(w: &Widget, l: &Layout, at: Point) -> Option<u64> {
        if !l.rect.contains(at.x, at.y) {
            return None;
        }
        for (cw, cl) in w.children.iter().zip(l.children.iter()).rev() {
            if let Some(hit) = walk(cw, cl, at) {
                return Some(hit);
            }
        }
        Some(w.id)
    }
    walk(root?, layout, at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::element::{column, custom, row, sized, stack, text};
    use crate::layout::{FixedCell, layout};
    use alloc::vec;
    use libdraw::geom::Size;
    use librsproto::surface::{POINTER_ENTER, POINTER_LEAVE, POINTER_MOTION};

    const CELL: FixedCell = FixedCell { w: 8, h: 16 };
    const SCREEN: Rect = Rect::new(0, 0, 640, 480);

    #[derive(Clone, PartialEq, Eq, Debug)]
    enum Msg {
        Pressed(u8),
        Key(KeyEvent),
        Ptr(PointerEvent),
    }

    /// Lay out and diff, returning the tree and layout the router works against.
    fn build(e: &Element<Msg>) -> (Tree, Layout) {
        let l = layout(e, SCREEN, &CELL);
        let mut t = Tree::new();
        t.update(e, &l).expect("a clean frame");
        (t, l)
    }

    fn press(x: i32, y: i32) -> PointerEvent {
        PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x110,
            buttons: 1,
            flags: POINTER_PRESSED,
            x,
            y,
            ..Default::default()
        }
    }

    fn release(x: i32, y: i32) -> PointerEvent {
        PointerEvent { kind: POINTER_BUTTON, button: 0x110, buttons: 0, flags: 0, x, y,
                       ..Default::default() }
    }

    fn motion(x: i32, y: i32) -> PointerEvent {
        PointerEvent { kind: POINTER_MOTION, buttons: 1, x, y, ..Default::default() }
    }

    /// A key record for these tests. The window id is arbitrary: routing to a *widget* happens
    /// inside one window, so the toolkit never reads it — `libsurface` has already decided the
    /// record belongs to this window before it gets here.
    fn key(code: u16) -> KeyEvent {
        KeyEvent::new(1, code, 1, 0)
    }

    /// Clicking a composite widget does **not** steal the keyboard from whatever had it.
    ///
    /// The press and pointer handlers of a composite widget live on its outside, so dispatch
    /// walks up to find them. Focus must not: `widget::button` marks its outer `Stack`
    /// focusable and gives it no `on_key`, so walking up moved focus onto a widget that claims
    /// keys and handles none — clicking a menu bar killed typing until the user clicked the
    /// text back (PR #223 review, finding 1).
    ///
    /// There was no click-to-focus test here at all, which is why the walk-up looked harmless.
    #[test]
    fn clicking_a_button_does_not_take_the_keyboard_from_the_focused_widget() {
        use crate::widget::{Palette, WidgetState, button};

        let p = Palette::default();
        // A focusable widget that handles keys — the terminal grid's shape — beside a button.
        let e: Element<Msg> = column(vec![
            button("Menu", Msg::Pressed(1), WidgetState::default(), &p),
            custom(7, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k))).focusable(),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();

        let grid = t.root().unwrap().children[1].id;
        r.focus(&t, &e, grid);
        assert_eq!(r.key(&t, &e, key(30)), Some(Msg::Key(key(30))), "the grid has the keyboard");

        // Click the button. Its message is produced — and the keyboard stays where it was.
        let btn = l.children[0].rect;
        let (at_x, at_y) = (btn.origin.x + btn.size.w as i32 / 2, btn.origin.y + btn.size.h as i32 / 2);
        let (_, _) = r.pointer(&t, &e, &l, press(at_x, at_y));
        let (msgs, _) = r.pointer(&t, &e, &l, release(at_x, at_y));
        assert_eq!(msgs, vec![Msg::Pressed(1)], "the button still fires");
        assert_eq!(
            r.key(&t, &e, key(31)),
            Some(Msg::Key(key(31))),
            "and the grid still has the keyboard"
        );
    }

    /// A press inside a `button` in a padded column inside a stack produces its message.
    ///
    /// The exact shape `nxterm`'s menu takes — a backing `fill` under a padded column of
    /// buttons — checked because a popup made of it was hit-testing to the column rather than
    /// to the button under the cursor.
    #[test]
    fn a_press_on_a_button_in_a_padded_column_produces_its_message() {
        use crate::element::padding;
        use crate::widget::{Palette, WidgetState, button};
        use crate::element::Insets;

        let p = Palette::default();
        let e: Element<Msg> = stack(vec![
            crate::element::fill(p.face),
            padding(
                Insets::all(2),
                column(vec![
                    button("Clear", Msg::Pressed(1), WidgetState::default(), &p),
                    button("Reset", Msg::Pressed(2), WidgetState::default(), &p),
                ]),
            ),
        ]);
        // A window the size the menu measures to, as the popup is created at.
        let size = crate::layout::measure(&e, crate::layout::Constraints::loose(SCREEN.size), &CELL);
        let bounds = Rect::new(0, 0, size.w, size.h);
        let l = layout(&e, bounds, &CELL);
        let mut t = Tree::new();
        t.update(&e, &l).expect("a clean frame");

        let mut r = Router::new();
        // The upper quarter: inside the first button, clear of the boundary.
        let at = (bounds.size.w as i32 / 2, bounds.size.h as i32 / 4);
        // A click is a press *and* a release inside the widget the press captured.
        let (_, hit) = r.pointer(&t, &e, &l, press(at.0, at.1));
        assert!(hit.is_some(), "something was hit at {at:?} in {bounds:?}");
        let (msgs, _) = r.pointer(&t, &e, &l, release(at.0, at.1));
        assert_eq!(msgs, vec![Msg::Pressed(1)], "the first item's message, at {at:?}");
    }

    #[test]
    fn a_click_hits_the_topmost_widget_not_the_one_underneath() {
        // The reverse of paint order: a menu overlaid on a grid takes the click that
        // visually landed on it.
        let e: Element<Msg> = stack(vec![
            custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr),
            sized(Size::new(40, 40), custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr)),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();

        let (_, hit) = r.pointer(&t, &e, &l, press(10, 10));
        let overlay = t.root().unwrap().children[1].children[0].id;
        assert_eq!(hit, Some(overlay), "the overlay, not the base layer");

        // ...and outside the overlay, the base layer takes it.
        let mut r = Router::new();
        let (_, hit) = r.pointer(&t, &e, &l, press(100, 100));
        let base = t.root().unwrap().children[0].id;
        assert_eq!(hit, Some(base));
    }

    #[test]
    fn a_press_captures_and_a_drag_that_leaves_still_reports_to_it() {
        // Without this the release lands on whatever is under the cursor, and a scrollbar
        // believes it is still being dragged forever.
        let e: Element<Msg> = row(vec![
            sized(Size::new(50, 480), custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr)),
            custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr).flex(1),
        ]);
        let (t, l) = build(&e);
        let left = t.root().unwrap().children[0].children[0].id;
        let mut r = Router::new();

        let (_, hit) = r.pointer(&t, &e, &l, press(10, 10));
        assert_eq!(hit, Some(left));
        assert_eq!(r.capture(), Some(left));

        // Dragged far to the right, over the other widget.
        let (_, hit) = r.pointer(&t, &e, &l, motion(400, 10));
        assert_eq!(hit, Some(left), "still the widget that took the press");

        let (_, hit) = r.pointer(&t, &e, &l, release(400, 10));
        assert_eq!(hit, Some(left));
        assert_eq!(r.capture(), None, "and the capture ends");
    }

    #[test]
    fn releasing_outside_the_widget_is_a_cancel_rather_than_a_click() {
        // Pressing a button and sliding off it is how a user changes their mind. Firing
        // `on_press` on the press instead of the click removes that.
        let e: Element<Msg> = row(vec![
            sized(Size::new(50, 480), custom(1, Size::new(0, 0)).on_press(Msg::Pressed(1))),
            custom(2, Size::new(0, 0)).flex(1),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();

        // **The press itself fires nothing.** Asserted rather than discarded: a first
        // version threw this away, and an implementation firing on the press *as well as*
        // the click — so every button emits twice — passed the whole file (PR #184 review).
        let (msgs, _) = r.pointer(&t, &e, &l, press(10, 10));
        assert!(msgs.is_empty(), "a press is not yet a click");
        let (msgs, _) = r.pointer(&t, &e, &l, release(400, 10));
        assert!(msgs.is_empty(), "released outside: cancelled");

        // ...and released inside, it fires.
        let mut r = Router::new();
        r.pointer(&t, &e, &l, press(10, 10));
        let (msgs, _) = r.pointer(&t, &e, &l, release(20, 20));
        assert_eq!(msgs, [Msg::Pressed(1)]);
    }

    #[test]
    fn a_click_lands_on_a_widget_away_from_the_window_origin() {
        // Every other click test puts its widget at (0, 0), where a containment check that
        // adds the rect's own origin to the point is indistinguishable from one that does
        // not. It is not indistinguishable here, and this is the shape a real window has:
        // a button in a second column, or anything below a menu bar (PR #184 review).
        let e: Element<Msg> = row(vec![
            sized(Size::new(500, 480), custom(1, Size::new(0, 0))),
            sized(Size::new(100, 480), custom(2, Size::new(0, 0)).on_press(Msg::Pressed(2))),
        ]);
        let (t, l) = build(&e);
        let target = t.root().unwrap().children[1].children[0].id;
        assert_eq!(
            layout_at(&l, &path_to_id(t.root(), target).unwrap()).unwrap().rect,
            Rect::new(500, 0, 100, 480),
            "the premise: this widget is nowhere near the origin",
        );

        let mut r = Router::new();
        let (_, hit) = r.pointer(&t, &e, &l, press(550, 10));
        assert_eq!(hit, Some(target), "the press landed on it");
        let (msgs, _) = r.pointer(&t, &e, &l, release(550, 10));
        assert_eq!(msgs, [Msg::Pressed(2)], "released inside: a click");

        // ...and the cancel still works out here, so this cannot be satisfied by dropping
        // the containment check altogether.
        let mut r = Router::new();
        r.pointer(&t, &e, &l, press(550, 10));
        let (msgs, _) = r.pointer(&t, &e, &l, release(50, 10));
        assert!(msgs.is_empty(), "released back over the spacer: cancelled");
    }

    #[test]
    fn only_the_capturing_buttons_release_is_a_click() {
        // Left-press the widget, then press *and release* the right button over it, then
        // release left. Without checking which button, the right release fires `on_press`
        // and the left release fires it again — two clicks for one press-and-release of the
        // button the user actually clicked (PR #184 re-review, finding 2).
        let e: Element<Msg> = column(vec![
            custom(1, Size::new(0, 0)).on_press(Msg::Pressed(1)).flex(1),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();

        let (msgs, _) = r.pointer(&t, &e, &l, press(10, 10));
        assert!(msgs.is_empty());

        // Right button down (both held) and up again, without moving.
        let right_down = PointerEvent {
            kind: POINTER_BUTTON, button: 0x111, buttons: 3, flags: POINTER_PRESSED,
            x: 10, y: 10, ..Default::default()
        };
        let right_up = PointerEvent {
            kind: POINTER_BUTTON, button: 0x111, buttons: 1, flags: 0,
            x: 10, y: 10, ..Default::default()
        };
        let (msgs, _) = r.pointer(&t, &e, &l, right_down);
        assert!(msgs.is_empty(), "a second press is not a click");
        let (msgs, _) = r.pointer(&t, &e, &l, right_up);
        assert!(msgs.is_empty(), "and neither is its release: {msgs:?}");

        // The left release — the button that took the capture — is the click, once.
        let (msgs, _) = r.pointer(&t, &e, &l, release(10, 10));
        assert_eq!(msgs, [Msg::Pressed(1)], "exactly one click");
    }

    #[test]
    fn a_key_goes_to_the_focused_widget_not_the_one_under_the_cursor() {
        let e: Element<Msg> = column(vec![
            custom(1, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k))).flex(1),
            custom(2, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k))).flex(1),
        ]);
        let (t, l) = build(&e);
        let second = t.root().unwrap().children[1].id;
        let mut r = Router::new();
        assert!(r.focus(&t, &e, second));

        // The pointer is over the first widget; the key still goes to the focused one.
        let (_, hit) = r.pointer(&t, &e, &l, motion(10, 10));
        assert_ne!(hit, Some(second));
        assert_eq!(r.key(&t, &e, key(30)), Some(Msg::Key(key(30))));
        assert_eq!(r.focused(), Some(second));
    }

    #[test]
    fn an_unhandled_key_bubbles_to_an_ancestor() {
        // How a menu accelerator works without every widget knowing about menus.
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable()])
            .on_key(|k| Some(Msg::Key(k)));
        let (t, l) = build(&e);
        let _ = l;
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        assert!(r.focus(&t, &e, leaf));
        assert_eq!(r.key(&t, &e, key(1)), Some(Msg::Key(key(1))), "the parent took it");
    }

    #[test]
    fn a_focused_widget_can_decline_a_key_and_let_it_bubble() {
        // **The design's own motivating example, which the first API could not express.**
        // With `on_key` returning `Msg` rather than `Option<Msg>`, having a handler *meant*
        // handling — so a focused text field swallowed every accelerator and "unhandled keys
        // reach the menu" was unreachable. Here the leaf takes ordinary keys and declines
        // keycode 24, which the ancestor claims.
        let e: Element<Msg> = column(vec![
            custom(1, Size::new(0, 0))
                .on_key(|k| (k.keycode != 24).then_some(Msg::Key(k)))
                .flex(1),
        ])
        .on_key(|k| Some(Msg::Pressed(k.keycode as u8)));
        let (t, _) = build(&e);
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        assert!(r.focus(&t, &e, leaf));

        assert_eq!(r.key(&t, &e, key(30)), Some(Msg::Key(key(30))), "an ordinary key stays");
        assert_eq!(
            r.key(&t, &e, key(24)),
            Some(Msg::Pressed(24)),
            "the declined one reached the ancestor"
        );
    }

    #[test]
    fn a_key_nothing_claims_returns_none() {
        // There is no separate "reaches the application" path: the caller *is* the
        // application and still holds the event it passed in.
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).on_key(|_| None).flex(1)]);
        let (t, _) = build(&e);
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        assert!(r.focus(&t, &e, leaf));
        assert_eq!(r.key(&t, &e, key(30)), None);
    }

    #[test]
    fn a_key_with_nothing_focused_is_dropped() {
        // Not sent to whatever the pointer is over, which would make typing depend on where
        // the mouse happens to rest — the same rule the compositor applies between windows.
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k)))]);
        let (t, _) = build(&e);
        let r = Router::new();
        assert_eq!(r.focused(), None);
        assert_eq!(r.key(&t, &e, key(30)), None);
    }

    #[test]
    fn tab_traverses_focusable_widgets_in_tree_order_and_wraps() {
        let e: Element<Msg> = column(vec![
            custom(1, Size::new(0, 0)).focusable(),
            custom(2, Size::new(0, 0)), // skipped: not focusable
            custom(3, Size::new(0, 0)).focusable(),
        ]);
        let (t, _) = build(&e);
        let kids = &t.root().unwrap().children;
        let (a, c) = (kids[0].id, kids[2].id);
        let mut r = Router::new();

        assert_eq!(r.focus_next(&t, &e), Some(a));
        assert_eq!(r.focus_next(&t, &e), Some(c), "the middle one is skipped");
        assert_eq!(r.focus_next(&t, &e), Some(a), "and it wraps");
        assert_eq!(r.focus_prev(&t, &e), Some(c), "backwards wraps too");
    }

    #[test]
    fn focus_cannot_be_given_to_a_widget_tab_could_never_reach() {
        // Otherwise focus lands somewhere traversal cannot leave, and the user is stuck.
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0))]);
        let (t, _) = build(&e);
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        assert!(!r.focus(&t, &e, leaf));
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn window_focus_and_widget_focus_are_two_fields_from_two_sources() {
        // The rule the design pass insisted on. A window that lost the keyboard stops
        // blinking; when it returns, the caret is where it was.
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable()]);
        let (t, _) = build(&e);
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        r.focus(&t, &e, leaf);
        assert!(r.focus_of(leaf).is_active());

        r.set_window_focused(false);
        assert!(!r.focus_of(leaf).is_active(), "stops blinking");
        assert_eq!(r.focused(), Some(leaf), "but does not forget where it was");

        r.set_window_focused(true);
        assert!(r.focus_of(leaf).is_active(), "and comes back to the same widget");
    }

    #[test]
    fn a_click_focuses_a_focusable_widget_and_leaves_focus_alone_otherwise() {
        let e: Element<Msg> = row(vec![
            sized(Size::new(50, 480), custom(1, Size::new(0, 0)).focusable()),
            custom(2, Size::new(0, 0)).flex(1), // not focusable
        ]);
        let (t, l) = build(&e);
        let focusable = t.root().unwrap().children[0].children[0].id;
        let mut r = Router::new();

        r.pointer(&t, &e, &l, press(10, 10));
        r.pointer(&t, &e, &l, release(10, 10));
        assert_eq!(r.focused(), Some(focusable));

        // Clicking the non-focusable one must not clear focus — a scrollbar drag would
        // otherwise steal the caret from the text beside it.
        r.pointer(&t, &e, &l, press(400, 10));
        assert_eq!(r.focused(), Some(focusable));
    }

    #[test]
    fn focus_and_capture_are_dropped_when_their_widgets_go() {
        // The router is not on the destroy path, so it asks the tree — which cannot go out
        // of date the way a callback can.
        let e: Element<Msg> = column(vec![
            custom(1, Size::new(0, 0)).focusable().on_pointer(Msg::Ptr).flex(1),
        ]);
        let (mut t, l) = build(&e);
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        r.focus(&t, &e, leaf);
        r.pointer(&t, &e, &l, press(10, 10));
        assert_eq!(r.capture(), Some(leaf));

        let e2: Element<Msg> = column(vec![text("nothing here")]);
        let l2 = layout(&e2, SCREEN, &CELL);
        t.update(&e2, &l2).expect("ok");
        r.prune(&t);
        assert_eq!(r.focused(), None);
        assert_eq!(r.capture(), None);
    }

    #[test]
    fn tab_from_a_stale_focus_starts_from_the_beginning_rather_than_refusing() {
        // After a `prune` there is nothing focused; Tab must still move.
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable()]);
        let (t, _) = build(&e);
        let mut r = Router::new();
        assert_eq!(r.focus_next(&t, &e), Some(t.root().unwrap().children[0].id));
    }

    #[test]
    fn a_window_with_nothing_focusable_clears_focus_rather_than_pointing_at_a_ghost() {
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable()]);
        let (t, _) = build(&e);
        let mut r = Router::new();
        r.focus_next(&t, &e);
        assert!(r.focused().is_some());

        let e2: Element<Msg> = column(vec![custom(1, Size::new(0, 0))]);
        let (t2, _) = build(&e2);
        assert_eq!(r.focus_next(&t2, &e2), None);
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn the_compositors_window_crossing_becomes_one_widget_crossing_not_two() {
        // Its `ENTER` is about the *window*; the widget's is about the *widget*. Forwarding
        // both handed a widget two enters for one cursor movement.
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr).flex(1)]);
        let (t, l) = build(&e);
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();

        let enter = PointerEvent { kind: POINTER_ENTER, x: 5, y: 5, ..Default::default() };
        let (msgs, _) = r.pointer(&t, &e, &l, enter);
        assert_eq!(msgs.len(), 1, "one crossing, not two: {msgs:?}");
        let Msg::Ptr(got) = &msgs[0] else { panic!("a pointer message") };
        assert_eq!(got.kind, POINTER_ENTER);
        assert_eq!(r.inside(), Some(leaf));
        assert_eq!(r.capture(), None, "a crossing is not a press");

        // And leaving the window leaves the widget, wherever the coordinates point.
        let leave = PointerEvent { kind: POINTER_LEAVE, x: 5, y: 5, ..Default::default() };
        let (msgs, _) = r.pointer(&t, &e, &l, leave);
        assert_eq!(msgs.len(), 1);
        let Msg::Ptr(got) = &msgs[0] else { panic!("a pointer message") };
        assert_eq!(got.kind, POINTER_LEAVE);
        assert_eq!(r.inside(), None);
    }

    #[test]
    fn moving_between_widgets_leaves_one_before_entering_the_next() {
        // Order matters: a widget told it was entered before the previous one was left
        // concludes two widgets are hovered at once.
        let e: Element<Msg> = row(vec![
            sized(Size::new(50, 480), custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr)),
            custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr).flex(1),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();

        r.pointer(&t, &e, &l, motion(10, 10));
        let (msgs, _) = r.pointer(&t, &e, &l, motion(400, 10));
        let kinds: alloc::vec::Vec<u16> = msgs
            .iter()
            .map(|m| match m {
                Msg::Ptr(p) => p.kind,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            kinds,
            [POINTER_LEAVE, POINTER_ENTER, POINTER_MOTION],
            "leave, then enter, then the motion that caused both"
        );
    }

    #[test]
    fn crossings_are_suppressed_during_a_capture_and_re_derived_on_release() {
        // Telling a widget the cursor left while it is still receiving that cursor's events
        // is two contradictory statements at once — the compositor's own rule, one layer
        // down.
        let e: Element<Msg> = row(vec![
            sized(Size::new(50, 480), custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr)),
            custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr).flex(1),
        ]);
        let (t, l) = build(&e);
        let left = t.root().unwrap().children[0].children[0].id;
        let right = t.root().unwrap().children[1].id;
        let mut r = Router::new();

        r.pointer(&t, &e, &l, motion(10, 10));
        r.pointer(&t, &e, &l, press(10, 10));
        let (msgs, _) = r.pointer(&t, &e, &l, motion(400, 10));
        assert_eq!(r.inside(), Some(left), "still inside, though over the other widget");
        assert!(
            msgs.iter().all(|m| !matches!(m, Msg::Ptr(p) if p.kind == POINTER_LEAVE)),
            "no crossing while captured: {msgs:?}"
        );

        let (msgs, _) = r.pointer(&t, &e, &l, release(400, 10));
        assert_eq!(r.inside(), Some(right), "re-derived once the capture ends");
        let kinds: alloc::vec::Vec<u16> = msgs
            .iter()
            .filter_map(|m| match m {
                Msg::Ptr(p) if p.kind == POINTER_LEAVE || p.kind == POINTER_ENTER => Some(p.kind),
                _ => None,
            })
            .collect();
        assert_eq!(kinds, [POINTER_LEAVE, POINTER_ENTER]);
    }

    #[test]
    fn a_handler_gets_widget_local_coordinates() {
        // A widget cannot otherwise know where in *itself* it was clicked. Every internal
        // comparison stays in window coordinates; this is the only place the space changes.
        let e: Element<Msg> = row(vec![
            sized(Size::new(500, 480), custom(1, Size::new(0, 0))),
            sized(Size::new(100, 480), custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr)),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();
        let (msgs, _) = r.pointer(&t, &e, &l, motion(550, 30));
        let motion_msg = msgs
            .iter()
            .find_map(|m| match m {
                Msg::Ptr(p) if p.kind == POINTER_MOTION => Some(*p),
                _ => None,
            })
            .expect("a motion");
        assert_eq!((motion_msg.x, motion_msg.y), (50, 30), "window 550 minus the widget's 500");
    }

    #[test]
    fn widget_local_coordinates_go_negative_during_a_capture() {
        // The reason they are signed. Clamping would make every leftward drag look like it
        // stopped at the widget's edge — the same rule as the compositor's window-local
        // coordinates, one layer down.
        let e: Element<Msg> = row(vec![
            sized(Size::new(500, 480), custom(1, Size::new(0, 0))),
            sized(Size::new(100, 480), custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr)),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();
        r.pointer(&t, &e, &l, press(550, 10));
        let (msgs, _) = r.pointer(&t, &e, &l, motion(400, 10));
        let m = msgs
            .iter()
            .find_map(|m| match m {
                Msg::Ptr(p) if p.kind == POINTER_MOTION => Some(*p),
                _ => None,
            })
            .expect("a motion");
        assert_eq!(m.x, -100, "100px left of the captured widget's left edge");
    }
}
