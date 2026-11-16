//! Where an event goes inside a window.
//!
//! This decides which *widget* gets an event; the compositor decides which *window*. The two
//! focuses never share a field: widget focus outlives window focus, so a window that regains the
//! keyboard puts the caret back where it was.
//!
//! **A press captures.** Every pointer event up to the release of the last button goes to the
//! widget the press landed on, even after the cursor leaves it, or a drag that ends outside a
//! scrollbar never delivers its release.

use alloc::vec::Vec;

use libdraw::geom::{Point, Rect};
use libkern::abi::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT};
use librsproto::surface::{
    KeyEvent, POINTER_BUTTON, POINTER_ENTER, POINTER_LEAVE, POINTER_PRESSED, POINTER_WHEEL, PointerEvent,
};

use crate::diff::{Tree, Widget};
use crate::element::Element;
use crate::layout::Layout;

/// A path from the root to a widget: the child index at each level, so routing can bubble.
pub type Path = Vec<usize>;

/// Which widget has keyboard focus inside this window, what the pointer is doing, and the
/// last thing the compositor said about the window.
pub struct Router {
    focus: Option<u64>,
    capture: Option<u64>,
    /// Which button took that capture: a click is *that* button's release, not any release.
    capture_button: Option<u16>,
    /// The widget the cursor is inside, for synthesising crossings.
    inside: Option<u64>,
    window_focused: bool,
}

/// The two focus facts a widget needs in order to paint itself, kept apart on purpose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Focus {
    /// This widget holds keyboard focus within its window.
    pub widget_focused: bool,
    /// The compositor says this window has the keyboard.
    pub window_focused: bool,
}

impl Focus {
    /// Whether a caret should blink and a focus ring should be drawn.
    pub fn is_active(&self) -> bool {
        self.widget_focused && self.window_focused
    }
}

impl Default for Router {
    fn default() -> Self {
        Self::new()
    }
}

impl Router {
    /// A router with nothing focused and nothing captured; `window_focused` starts **true** so a
    /// client's first paint is not dim.
    pub fn new() -> Self {
        Self { focus: None, capture: None, capture_button: None, inside: None, window_focused: true }
    }

    /// The focused widget's id, if any.
    pub fn focused(&self) -> Option<u64> {
        self.focus
    }

    /// Whether a button is being held on a widget — a gesture is in progress.
    ///
    /// A capture is a *tree id*, which survives only while its node does: a caller that retains a
    /// tree must not rebuild it with a *different* hover while this is true, or a repaint can
    /// re-identify an **ancestor** of the captured handler and the click is silently lost.
    pub fn grabbed(&self) -> bool {
        self.capture.is_some()
    }

    /// The widget holding the pointer capture, if a button is down.
    pub fn capture(&self) -> Option<u64> {
        self.capture
    }

    /// The widget the cursor is inside, as a **diff-tree id**, not a key (see
    /// [`hovered_key`](Self::hovered_key)).
    pub fn inside(&self) -> Option<u64> {
        self.inside
    }

    /// The **key** of the widget under the cursor, if it has one.
    ///
    /// **Two id spaces meet here, and mixing them is a bug that looks like it works**: the tree's
    /// ids are the diff's counter, keys are the application's. The nearest *keyed* ancestor counts,
    /// as a widget built from layers carries its key on the outside.
    pub fn hovered_key(&self, tree: &Tree) -> Option<u64> {
        fn find(w: &Widget, id: u64, carried: Option<u64>) -> Option<Option<u64>> {
            let carried = w.key.or(carried);
            if w.id == id {
                return Some(carried);
            }
            w.children.iter().find_map(|c| find(c, id, carried))
        }
        let id = self.inside?;
        find(tree.root()?, id, None).flatten()
    }

    /// Whether the compositor says this window has the keyboard.
    pub fn window_focused(&self) -> bool {
        self.window_focused
    }

    /// Record a `Surface::FocusEvent`; **widget focus is untouched**, so a returning window keeps
    /// its caret.
    pub fn set_window_focused(&mut self, focused: bool) {
        self.window_focused = focused;
    }

    /// How `widget` should paint itself.
    pub fn focus_of(&self, widget: u64) -> Focus {
        Focus { widget_focused: self.focus == Some(widget), window_focused: self.window_focused }
    }

    /// Give focus to `widget` if the tree says it can take it.
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
    /// With nothing focusable this is `None` and focus is cleared, so a destroyed widget is not
    /// kept pointing at.
    pub fn focus_next(&mut self, tree: &Tree, element: &Element<impl Sized>) -> Option<u64> {
        self.step_focus(tree, element, 1)
    }

    /// Move focus to the previous focusable widget in tree order, wrapping.
    pub fn focus_prev(&mut self, tree: &Tree, element: &Element<impl Sized>) -> Option<u64> {
        self.step_focus(tree, element, -1)
    }

    fn step_focus(&mut self, tree: &Tree, element: &Element<impl Sized>, by: isize) -> Option<u64> {
        let order = focusable_ids(tree, element);
        if order.is_empty() {
            self.focus = None;
            return None;
        }
        let next = match self.focus.and_then(|f| order.iter().position(|&id| id == f)) {
            Some(i) => (i as isize + by).rem_euclid(order.len() as isize) as usize,
            None if by >= 0 => 0,
            None => order.len() - 1,
        };
        self.focus = Some(order[next]);
        self.focus
    }

    /// Drop focus, capture and the hovered widget if they name widgets the tree no longer has;
    /// [`pointer`](Self::pointer) calls it itself.
    fn forget_stale(&mut self, tree: &Tree) {
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
    /// The focused widget's `on_key` fires first; if it has none **or returns `None`**, the event
    /// bubbles to each ancestor in turn: a focused text field must not swallow accelerators.
    ///
    /// With nothing focused the event is dropped, not sent to whatever the pointer is over.
    pub fn key<Msg>(&self, tree: &Tree, element: &Element<Msg>, event: KeyEvent) -> Option<Msg> {
        let focused = self.focus?;
        let path = path_to_id(tree.root(), focused)?;
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
    /// Route a drop at `(x, y)` — window-local, as `Surface::Dropped` carries it. The handler may
    /// be on an ancestor of what was hit, as for a press; **no capture, and no crossing** — a drop
    /// is one event with no press.
    pub fn drop_at<Msg: Clone>(
        &self,
        tree: &Tree,
        element: &Element<Msg>,
        layout: &Layout,
        x: i32,
        y: i32,
    ) -> Option<Msg> {
        let id = hit_test(tree.root(), layout, Point::new(x, y))?;
        let path = path_to_id(tree.root(), id)?;
        (0..=path.len()).rev().find_map(|n| element_at(element, &path[..n])?.on_drop.clone())
    }

    /// Route a pointer event, returning the messages it produced.
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
        let wheel = event.kind == POINTER_WHEEL;

        // **The press decides the capture, from where the cursor is**, or a drag that leaves the
        // widget stops being its drag.
        self.forget_stale(tree);
        // The innermost node under the cursor: **click-to-focus is decided by it directly**, not by
        // `target`, the handler-bearing ancestor.
        let hit_now = hit_test(tree.root(), layout, at);
        let opens_capture = pressed && self.capture.is_none();
        if opens_capture {
            // **The widget that handles the gesture, not the deepest one under the cursor.** A
            // press establishes **hover**: the next frame draws the control lit with more layers,
            // positional matching gives each a fresh id, and a captured *part* (the label inside a
            // button) no longer exists, so the release dispatches *nothing* — intermittently, when
            // press and release land in different frames. The handler-bearing node is the same kind
            // lit or not, so `reconcile` keeps its id
            // (`a_gesture_survives_the_widget_under_it_being_re_identified`). **Searching from the
            // hit itself** keeps a self-handling widget (`nxterm`'s grid) capturing what it did.
            self.capture = hit_now.map(|hit| handling_ancestor(tree, element, hit).unwrap_or(hit));
            self.capture_button = Some(event.button);
        }
        let target = self.capture.or(hit_now);

        let mut out = Vec::new();

        // **The compositor's crossings are inputs, not events to forward**: a widget only sees
        // crossings this router synthesises about *widgets*.
        let window_crossing = matches!(event.kind, POINTER_ENTER | POINTER_LEAVE);
        let left_window = event.kind == POINTER_LEAVE;

        // Crossings first, so a widget hears "entered" before the motion that brought it in.
        // Suppressed while captured (the cursor is still that widget's) **and for a wheel**, which
        // moved no cursor.
        if self.capture.is_none() && !wheel {
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
            // **The handler may be on an ancestor of what was hit, so look up the path**:
            // `widget::button` is a `Stack` carrying `on_press` over a `fill` and a `text`, and
            // clicking the label hit a node that handles nothing.
            let nearest = |has: &dyn Fn(&Element<Msg>) -> bool| -> Option<(usize, &Element<Msg>)> {
                (0..=path.len()).rev().find_map(|n| {
                    let e = element_at(element, &path[..n])?;
                    has(e).then_some((n, e))
                })
            };

            // **The wheel goes to `on_wheel` and nowhere else**, walking up until something claims
            // it.
            if wheel {
                if let Some((n, e)) = nearest(&|e: &Element<Msg>| e.on_wheel.is_some())
                    && let Some(f) = e.on_wheel
                    && let Some(l) = layout_at(layout, &path[..n])
                {
                    out.push(f(localise(event, l.rect)));
                }
                return (out, target);
            }

            if !window_crossing
                && let Some((n, e)) = nearest(&|e: &Element<Msg>| e.on_pointer.is_some())
                && let Some(f) = e.on_pointer
                && let Some(l) = layout_at(layout, &path[..n])
            {
                out.push(f(localise(event, l.rect)));
            }

            // **A press going down, for gestures decided before the release** (dragging a window by
            // its title bar). **A nearer `on_press` shadows it**, so pressing close does not also
            // drag; `nearest` walks outwards, so a larger index is the nearer handler. **Only the
            // press that opened the capture**: while one is held `target` is the captured widget,
            // and a second button would re-fire this (a right-click mid-drag sent a second `Drag`).
            if opens_capture
                && event.button == BTN_LEFT
                && let Some((n_down, e_down)) = nearest(&|e: &Element<Msg>| e.on_press_down.is_some())
                && let Some(msg) = e_down.on_press_down.clone()
            {
                let shadowed =
                    nearest(&|e: &Element<Msg>| e.on_press.is_some()).is_some_and(|(n_click, _)| n_click > n_down);
                if !shadowed {
                    out.push(msg);
                }
            }
            // **A right press asks for a menu, at the press**, and only one that opens the gesture: a
            // right press while another button is held is part of that gesture, and the compositor,
            // which dismisses a menu only on a press that takes its grab, would never close it.
            if opens_capture
                && event.button == BTN_RIGHT
                && let Some((_, e)) = nearest(&|e: &Element<Msg>| e.on_context.is_some())
                && let Some(msg) = e.on_context.clone()
            {
                out.push(msg);
            }
            // **The captured element must still exist**: an unresolvable path means the tree
            // changed under the gesture.
            if element_at(element, &path).is_none() {
                return (out, target);
            }
            // A **click** is a release inside the widget that took the press, **the left button's**,
            // or the middle's for `on_middle` (a right click on a close button closed the window);
            // releasing outside is a cancel. **Compared in window coordinates, with nothing added to
            // either side**: adding `l.rect.origin` agrees with `hit_test` only at the window's
            // top-left, and clicks in a second column or inside a `padding` were lost.
            let click = |e: &Element<Msg>| match event.button {
                BTN_LEFT => e.on_press.clone(),
                BTN_MIDDLE => e.on_middle.clone(),
                _ => None,
            };
            if released
                && self.capture_button == Some(event.button)
                && let Some((n, pe)) = nearest(&|e: &Element<Msg>| click(e).is_some())
                && let Some(msg) = click(pe)
                && let Some(l) = layout_at(layout, &path[..n])
                && l.rect.contains(event.x, event.y)
            {
                out.push(msg);
            }
            // Click to focus, but **on the widget actually hit, not on an ancestor**: focus is a
            // claim on the keyboard, and `widget::button`'s outer `Stack` is focusable with no
            // `on_key`, so walking up took typing from the terminal grid. **`hit_now`, not `id`**:
            // `id` is the captured handler-bearing ancestor
            // (`clicking_a_button_does_not_take_the_keyboard_from_the_focused_widget`).
            if pressed
                && let Some(hid) = hit_now
                && let Some(hpath) = path_to_id(tree.root(), hid)
                && element_at(element, &hpath).is_some_and(|he| he.focusable)
            {
                self.focus = Some(hid);
            }
        }

        if released && event.buttons == 0 {
            // The cursor may have moved while held, so re-derive the crossing now that it is not
            // suppressed.
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
    #[allow(clippy::too_many_arguments, reason = "a crossing event needs the tree, element, layout and cause together")]
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
        // **The same walk-up the press and motion paths do**, or a `button` driving hover from
        // `on_pointer` would latch hovered.
        let Some((n, e)) = (0..=path.len())
            .rev()
            .find_map(|n| element_at(element, &path[..n]).filter(|e| e.on_pointer.is_some()).map(|e| (n, e)))
        else {
            return;
        };
        let Some(f) = e.on_pointer else { return };
        let Some(l) = layout_at(layout, &path[..n]) else { return };
        // Buttons and modifiers come from the event that moved the cursor. **The wheel is zeroed**,
        // or a crossing provoked by a wheel would claim detents of its own and the widget would
        // scroll twice.
        let ev = PointerEvent {
            kind,
            button: 0,
            buttons: cause.buttons,
            flags: 0,
            modifiers: cause.modifiers,
            wheel: 0,
            ..localise(cause, l.rect)
        };
        out.push(f(ev));
    }
}

/// An event in `rect`'s coordinates.
///
/// **The only place a coordinate space changes.** Everything internal stays in window coordinates;
/// handlers get widget-local ones, signed and routinely negative mid-capture.
fn localise(event: PointerEvent, rect: Rect) -> PointerEvent {
    PointerEvent { x: event.x.saturating_sub(rect.origin.x), y: event.y.saturating_sub(rect.origin.y), ..event }
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

/// The nearest node at or above `hit` that carries a pointer handler, as a widget id.
///
/// `None` when nothing on the path handles anything; the caller falls back to `hit`. **All three
/// handlers count**, or the gesture would move to different nodes depending on which handler was
/// nearer.
fn handling_ancestor<Msg>(tree: &Tree, element: &Element<Msg>, hit: u64) -> Option<u64> {
    let path = path_to_id(tree.root(), hit)?;
    (0..=path.len()).rev().find_map(|n| {
        let e = element_at(element, &path[..n])?;
        let handles = e.on_press.is_some()
            || e.on_press_down.is_some()
            || e.on_pointer.is_some()
            || e.on_context.is_some()
            || e.on_middle.is_some();
        if handles { widget_at(tree.root(), &path[..n]) } else { None }
    })
}

/// The id of the widget at `path`, for the ancestor a handler was actually found on.
fn widget_at(root: Option<&Widget>, path: &[usize]) -> Option<u64> {
    let mut cur = root?;
    for &i in path {
        cur = cur.children.get(i)?;
    }
    Some(cur.id)
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
/// Children are searched in reverse (the last paints on top). **A window's border is never hit**:
/// an [`Outline`](crate::element::Node::Outline) is painted last over the whole window, so it took
/// every press and answered none.
fn hit_test(root: Option<&Widget>, layout: &Layout, at: Point) -> Option<u64> {
    fn walk(w: &Widget, l: &Layout, at: Point) -> Option<u64> {
        if !l.rect.contains(at.x, at.y) || matches!(w.print, crate::diff::Fingerprint::Outline(..)) {
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
mod tests;
