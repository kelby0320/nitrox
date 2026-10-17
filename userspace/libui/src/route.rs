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
                && let Some((n_down, e_down)) = nearest(&|e: &Element<Msg>| e.on_press_down.is_some())
                && let Some(msg) = e_down.on_press_down.clone()
            {
                let shadowed =
                    nearest(&|e: &Element<Msg>| e.on_press.is_some()).is_some_and(|(n_click, _)| n_click > n_down);
                if !shadowed {
                    out.push(msg);
                }
            }
            // **The captured element must still exist**: an unresolvable path means the tree
            // changed under the gesture.
            if element_at(element, &path).is_none() {
                return (out, target);
            }
            // A **click** is a release inside the widget that took the press; releasing outside is
            // a cancel. **Compared in window coordinates, with nothing added to either side**:
            // adding `l.rect.origin` agrees with `hit_test` only at the window's top-left, and
            // clicks in a second column or inside a `padding` were lost.
            if released
                && self.capture_button == Some(event.button)
                && let Some((n, pe)) = nearest(&|e: &Element<Msg>| e.on_press.is_some())
                && let Some(msg) = pe.on_press.clone()
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
        let handles = e.on_press.is_some() || e.on_press_down.is_some() || e.on_pointer.is_some();
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
        Wheel(PointerEvent),
    }

    fn build(e: &Element<Msg>) -> (Tree, Layout) {
        let l = layout(e, SCREEN, &CELL);
        let mut t = Tree::new();
        t.update(e, &l).expect("a clean frame");
        (t, l)
    }

    #[test]
    fn hovered_key_reports_the_applications_key_and_not_the_trees_id() {
        // **Two id spaces, and the bug is that mixing them compiles**: a key far outside the tree's
        // counter, on a widget deep enough that its id is not 1.
        const FAR: u64 = 9_000;
        let e: Element<Msg> = column(alloc::vec![
            sized(Size::new(640, 100), text("above")).key(FAR - 1),
            sized(Size::new(640, 100), stack(alloc::vec![text("target")])).key(FAR),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();
        r.pointer(&t, &e, &l, motion(10, 150));

        let id = r.inside().expect("the cursor is inside something");
        assert_ne!(id, FAR, "the tree's id happened to equal the key — pick a different key");
        assert_eq!(r.hovered_key(&t), Some(FAR), "the key, not the id");

        assert!(id != FAR, "the hit was a descendant, which is the case worth covering");
    }

    #[test]
    fn nothing_is_hovered_once_the_cursor_leaves() {
        let e: Element<Msg> = sized(Size::new(100, 100), text("x")).key(7);
        let (t, l) = build(&e);
        let mut r = Router::new();
        r.pointer(&t, &e, &l, motion(10, 10));
        assert_eq!(r.hovered_key(&t), Some(7));
        r.pointer(&t, &e, &l, motion(400, 400));
        assert_eq!(r.hovered_key(&t), None, "a highlight that never clears is a stuck highlight");
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
        PointerEvent { kind: POINTER_BUTTON, button: 0x110, buttons: 0, flags: 0, x, y, ..Default::default() }
    }

    fn motion(x: i32, y: i32) -> PointerEvent {
        PointerEvent { kind: POINTER_MOTION, buttons: 1, x, y, ..Default::default() }
    }

    #[test]
    fn one_element_may_carry_both_a_press_down_and_a_click() {
        let ui: Element<Msg> = sized(Size::new(640, 40), custom(1, Size::new(640, 40)))
            .on_press_down(Msg::Pressed(1))
            .on_press(Msg::Pressed(2));
        let (t, l) = build(&ui);
        let mut r = Router::new();

        let (down, _) = r.pointer(&t, &ui, &l, press(10, 10));
        assert_eq!(down, vec![Msg::Pressed(1)], "the press fires the gesture, not the click");

        let (up, _) = r.pointer(&t, &ui, &l, release(10, 10));
        assert_eq!(up, vec![Msg::Pressed(2)], "and the release inside fires the click");

        let (down, _) = r.pointer(&t, &ui, &l, press(10, 10));
        assert_eq!(down, vec![Msg::Pressed(1)]);
        let (up, _) = r.pointer(&t, &ui, &l, release(10, 400));
        assert!(up.is_empty(), "a release outside the widget is not a click");
    }

    #[test]
    fn a_border_painted_over_a_window_does_not_take_its_presses() {
        let ui: Element<Msg> = crate::element::stack(vec![
            sized(Size::new(640, 40), custom(1, Size::new(640, 40))).on_press(Msg::Pressed(1)),
            crate::element::outline(libdraw::format::Rgb::BLACK, 8),
        ]);
        let (t, l) = build(&ui);
        let mut r = Router::new();
        let _ = r.pointer(&t, &ui, &l, press(10, 10));
        let (up, _) = r.pointer(&t, &ui, &l, release(10, 10));
        assert_eq!(up, vec![Msg::Pressed(1)], "the press reached the content under the border");
    }

    #[test]
    fn a_drop_is_routed_by_position_like_a_press() {
        let ui: Element<Msg> = column(vec![
            sized(Size::new(640, 40), custom(1, Size::new(640, 40))),
            sized(Size::new(640, 200), custom(2, Size::new(640, 200))).on_drop(Msg::Pressed(9)),
        ]);
        let (t, l) = build(&ui);
        let r = Router::new();

        assert_eq!(r.drop_at(&t, &ui, &l, 20, 100), Some(Msg::Pressed(9)), "inside the region");
        assert_eq!(r.drop_at(&t, &ui, &l, 20, 10), None, "on the chrome above it");
        assert_eq!(r.drop_at(&t, &ui, &l, 20, 400), None, "past everything");
    }

    #[test]
    fn a_drops_handler_may_be_on_an_ancestor_of_what_it_landed_on() {
        let ui: Element<Msg> = stack(vec![row(vec![text("a line"), text("another")])]).on_drop(Msg::Pressed(4));
        let (t, l) = build(&ui);
        let r = Router::new();
        assert_eq!(r.drop_at(&t, &ui, &l, 4, 4), Some(Msg::Pressed(4)));
    }

    #[test]
    fn a_drop_needs_no_press_and_leaves_no_capture() {
        let ui: Element<Msg> = sized(Size::new(640, 200), custom(1, Size::new(640, 40))).on_drop(Msg::Pressed(1));
        let (t, l) = build(&ui);
        let r = Router::new();
        assert_eq!(r.drop_at(&t, &ui, &l, 5, 5), Some(Msg::Pressed(1)));
        assert_eq!(r.capture(), None, "a drop opens no capture");
        assert_eq!(r.inside(), None, "and synthesises no crossing");
    }

    fn key(code: u16) -> KeyEvent {
        KeyEvent::new(1, code, 1, 0)
    }

    #[test]
    fn clicking_a_button_does_not_take_the_keyboard_from_the_focused_widget() {
        use crate::widget::{WidgetState, button};

        let p = crate::paint::Theme::default();
        let e: Element<Msg> = column(vec![
            button("Menu", Msg::Pressed(1), WidgetState::default(), &p),
            custom(7, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k))).focusable(),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();

        let grid = t.root().unwrap().children[1].id;
        r.focus(&t, &e, grid);
        assert_eq!(r.key(&t, &e, key(30)), Some(Msg::Key(key(30))), "the grid has the keyboard");

        let btn = l.children[0].rect;
        let (at_x, at_y) = (btn.origin.x + btn.size.w as i32 / 2, btn.origin.y + btn.size.h as i32 / 2);
        let (_, _) = r.pointer(&t, &e, &l, press(at_x, at_y));
        let (msgs, _) = r.pointer(&t, &e, &l, release(at_x, at_y));
        assert_eq!(msgs, vec![Msg::Pressed(1)], "the button still fires");
        assert_eq!(r.key(&t, &e, key(31)), Some(Msg::Key(key(31))), "and the grid still has the keyboard");
    }

    #[test]
    fn a_press_on_a_button_in_a_padded_column_produces_its_message() {
        use crate::element::Insets;
        use crate::element::padding;
        use crate::widget::{WidgetState, button};

        let p = crate::paint::Theme::default();
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
        let size = crate::layout::measure(&e, crate::layout::Constraints::loose(SCREEN.size), &CELL);
        let bounds = Rect::new(0, 0, size.w, size.h);
        let l = layout(&e, bounds, &CELL);
        let mut t = Tree::new();
        t.update(&e, &l).expect("a clean frame");

        let mut r = Router::new();
        let at = (bounds.size.w as i32 / 2, bounds.size.h as i32 / 4);
        let (_, hit) = r.pointer(&t, &e, &l, press(at.0, at.1));
        assert!(hit.is_some(), "something was hit at {at:?} in {bounds:?}");
        let (msgs, _) = r.pointer(&t, &e, &l, release(at.0, at.1));
        assert_eq!(msgs, vec![Msg::Pressed(1)], "the first item's message, at {at:?}");
    }

    #[test]
    fn a_click_hits_the_topmost_widget_not_the_one_underneath() {
        let e: Element<Msg> = stack(vec![
            custom(1, Size::new(0, 0)).on_pointer(Msg::Ptr),
            sized(Size::new(40, 40), custom(2, Size::new(0, 0)).on_pointer(Msg::Ptr)),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();

        let (_, hit) = r.pointer(&t, &e, &l, press(10, 10));
        let overlay = t.root().unwrap().children[1].children[0].id;
        assert_eq!(hit, Some(overlay), "the overlay, not the base layer");

        let mut r = Router::new();
        let (_, hit) = r.pointer(&t, &e, &l, press(100, 100));
        let base = t.root().unwrap().children[0].id;
        assert_eq!(hit, Some(base));
    }

    #[test]
    fn a_press_captures_and_a_drag_that_leaves_still_reports_to_it() {
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

        let (_, hit) = r.pointer(&t, &e, &l, motion(400, 10));
        assert_eq!(hit, Some(left), "still the widget that took the press");

        let (_, hit) = r.pointer(&t, &e, &l, release(400, 10));
        assert_eq!(hit, Some(left));
        assert_eq!(r.capture(), None, "and the capture ends");
    }

    #[test]
    fn releasing_outside_the_widget_is_a_cancel_rather_than_a_click() {
        let e: Element<Msg> = row(vec![
            sized(Size::new(50, 480), custom(1, Size::new(0, 0)).on_press(Msg::Pressed(1))),
            custom(2, Size::new(0, 0)).flex(1),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();

        let (msgs, _) = r.pointer(&t, &e, &l, press(10, 10));
        assert!(msgs.is_empty(), "a press is not yet a click");
        let (msgs, _) = r.pointer(&t, &e, &l, release(400, 10));
        assert!(msgs.is_empty(), "released outside: cancelled");

        let mut r = Router::new();
        r.pointer(&t, &e, &l, press(10, 10));
        let (msgs, _) = r.pointer(&t, &e, &l, release(20, 20));
        assert_eq!(msgs, [Msg::Pressed(1)]);
    }

    #[test]
    fn a_click_lands_on_a_widget_away_from_the_window_origin() {
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

        let mut r = Router::new();
        r.pointer(&t, &e, &l, press(550, 10));
        let (msgs, _) = r.pointer(&t, &e, &l, release(50, 10));
        assert!(msgs.is_empty(), "released back over the spacer: cancelled");
    }

    #[test]
    fn only_the_capturing_buttons_release_is_a_click() {
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).on_press(Msg::Pressed(1)).flex(1)]);
        let (t, l) = build(&e);
        let mut r = Router::new();

        let (msgs, _) = r.pointer(&t, &e, &l, press(10, 10));
        assert!(msgs.is_empty());

        let right_down = PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x111,
            buttons: 3,
            flags: POINTER_PRESSED,
            x: 10,
            y: 10,
            ..Default::default()
        };
        let right_up = PointerEvent {
            kind: POINTER_BUTTON,
            button: 0x111,
            buttons: 1,
            flags: 0,
            x: 10,
            y: 10,
            ..Default::default()
        };
        let (msgs, _) = r.pointer(&t, &e, &l, right_down);
        assert!(msgs.is_empty(), "a second press is not a click");
        let (msgs, _) = r.pointer(&t, &e, &l, right_up);
        assert!(msgs.is_empty(), "and neither is its release: {msgs:?}");

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

        let (_, hit) = r.pointer(&t, &e, &l, motion(10, 10));
        assert_ne!(hit, Some(second));
        assert_eq!(r.key(&t, &e, key(30)), Some(Msg::Key(key(30))));
        assert_eq!(r.focused(), Some(second));
    }

    #[test]
    fn an_unhandled_key_bubbles_to_an_ancestor() {
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable()]).on_key(|k| Some(Msg::Key(k)));
        let (t, l) = build(&e);
        let _ = l;
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        assert!(r.focus(&t, &e, leaf));
        assert_eq!(r.key(&t, &e, key(1)), Some(Msg::Key(key(1))), "the parent took it");
    }

    #[test]
    fn a_focused_widget_can_decline_a_key_and_let_it_bubble() {
        let e: Element<Msg> =
            column(vec![custom(1, Size::new(0, 0)).on_key(|k| (k.keycode != 24).then_some(Msg::Key(k))).flex(1)])
                .on_key(|k| Some(Msg::Pressed(k.keycode as u8)));
        let (t, _) = build(&e);
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        assert!(r.focus(&t, &e, leaf));

        assert_eq!(r.key(&t, &e, key(30)), Some(Msg::Key(key(30))), "an ordinary key stays");
        assert_eq!(r.key(&t, &e, key(24)), Some(Msg::Pressed(24)), "the declined one reached the ancestor");
    }

    #[test]
    fn a_key_nothing_claims_returns_none() {
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).on_key(|_| None).flex(1)]);
        let (t, _) = build(&e);
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        assert!(r.focus(&t, &e, leaf));
        assert_eq!(r.key(&t, &e, key(30)), None);
    }

    #[test]
    fn a_key_with_nothing_focused_is_dropped() {
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
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0))]);
        let (t, _) = build(&e);
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        assert!(!r.focus(&t, &e, leaf));
        assert_eq!(r.focused(), None);
    }

    #[test]
    fn window_focus_and_widget_focus_are_two_fields_from_two_sources() {
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

        r.pointer(&t, &e, &l, press(400, 10));
        assert_eq!(r.focused(), Some(focusable));
    }

    /// A gesture survives the widget under it being **re-identified** between press and release.
    ///
    /// The press establishes hover, so the next frame draws the control with more layers and
    /// unkeyed children get fresh ids; a capture holding the deepest node would name one the tree
    /// no longer has. **The control itself never moved**: capturing the node that carries the
    /// handler survives.
    #[test]
    fn a_gesture_survives_the_widget_under_it_being_re_identified() {
        fn control(lit: bool) -> Element<Msg> {
            let mut layers = vec![];
            if lit {
                layers.push(text("ring"));
                layers.push(text("bevel"));
            }
            layers.push(sized(Size::new(200, 40), text("File")));
            stack(layers).on_press(Msg::Pressed(1)).key(7)
        }

        let rest: Element<Msg> = column(vec![control(false)]);
        let (mut t, l) = build(&rest);
        let mut r = Router::new();

        let before: Vec<u64> = ids(&t);
        assert!(r.pointer(&t, &rest, &l, press(10, 10)).0.is_empty(), "a press is not a click");

        let lit: Element<Msg> = column(vec![control(true)]);
        let l2 = layout(&lit, SCREEN, &CELL);
        t.update(&lit, &l2).expect("a clean frame");
        assert_ne!(ids(&t), before, "the highlight did not re-identify anything, so this test proves nothing");

        let (msgs, _) = r.pointer(&t, &lit, &l2, release(10, 10));
        assert_eq!(msgs, vec![Msg::Pressed(1)], "the click was dropped by the re-identification");
    }

    fn ids(t: &Tree) -> Vec<u64> {
        fn walk(w: &Widget, out: &mut Vec<u64>) {
            out.push(w.id);
            for c in &w.children {
                walk(c, out);
            }
        }
        let mut out = Vec::new();
        if let Some(root) = t.root() {
            walk(root, &mut out);
        }
        out
    }

    #[test]
    fn focus_and_capture_are_dropped_when_their_widgets_go() {
        let e: Element<Msg> = column(vec![custom(1, Size::new(0, 0)).focusable().on_pointer(Msg::Ptr).flex(1)]);
        let (mut t, l) = build(&e);
        let leaf = t.root().unwrap().children[0].id;
        let mut r = Router::new();
        r.focus(&t, &e, leaf);
        r.pointer(&t, &e, &l, press(10, 10));
        assert_eq!(r.capture(), Some(leaf));

        let e2: Element<Msg> = column(vec![text("nothing here")]);
        let l2 = layout(&e2, SCREEN, &CELL);
        t.update(&e2, &l2).expect("ok");
        r.pointer(&t, &e2, &l2, motion(10, 10));
        assert_eq!(r.focused(), None);
        assert_eq!(r.capture(), None);
    }

    #[test]
    fn tab_from_a_stale_focus_starts_from_the_beginning_rather_than_refusing() {
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

        let leave = PointerEvent { kind: POINTER_LEAVE, x: 5, y: 5, ..Default::default() };
        let (msgs, _) = r.pointer(&t, &e, &l, leave);
        assert_eq!(msgs.len(), 1);
        let Msg::Ptr(got) = &msgs[0] else { panic!("a pointer message") };
        assert_eq!(got.kind, POINTER_LEAVE);
        assert_eq!(r.inside(), None);
    }

    #[test]
    fn moving_between_widgets_leaves_one_before_entering_the_next() {
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

    fn turn(x: i32, y: i32, dz: i16) -> PointerEvent {
        PointerEvent { kind: librsproto::surface::POINTER_WHEEL, wheel: dz, x, y, ..Default::default() }
    }

    /// A wheel walks up to the nearest `on_wheel`, past widgets that only track the pointer.
    ///
    /// **The whole reason the wheel is not an `on_pointer` kind**: the thing that scrolls is the
    /// pane around the row, which must still see it.
    #[test]
    fn a_wheel_bubbles_past_a_pointer_handler_to_the_nearest_wheel_handler() {
        let e: Element<Msg> = sized(
            Size::new(400, 400),
            stack(vec![sized(Size::new(200, 200), custom(1, Size::new(0, 0))).on_pointer(Msg::Ptr)]),
        )
        .on_wheel(Msg::Wheel);
        let (t, l) = build(&e);
        let mut r = Router::new();

        let (msgs, _) = r.pointer(&t, &e, &l, turn(50, 60, 2));
        assert_eq!(msgs.len(), 1, "exactly one handler took it");
        let Msg::Wheel(p) = &msgs[0] else { panic!("the wheel handler, not the pointer one") };
        assert_eq!(p.wheel, 2);
        assert_eq!((p.x, p.y), (50, 60), "widget-local, like every other record");
    }

    #[test]
    fn a_pointer_handler_never_receives_a_wheel() {
        let e: Element<Msg> = sized(Size::new(400, 400), custom(1, Size::new(0, 0))).on_pointer(Msg::Ptr);
        let (t, l) = build(&e);
        let mut r = Router::new();
        let (msgs, _) = r.pointer(&t, &e, &l, turn(10, 10, 1));
        assert!(msgs.is_empty(), "a tree with no wheel handler scrolls nothing: {msgs:?}");
    }

    #[test]
    fn a_wheel_opens_no_gesture_and_moves_no_focus() {
        let e: Element<Msg> = row(vec![
            sized(Size::new(200, 480), custom(1, Size::new(0, 0)).on_key(|k| Some(Msg::Key(k)))),
            sized(Size::new(200, 480), custom(2, Size::new(0, 0))).on_press(Msg::Pressed(9)).on_wheel(Msg::Wheel),
        ]);
        let (t, l) = build(&e);
        let mut r = Router::new();
        r.pointer(&t, &e, &l, press(10, 10));
        r.pointer(&t, &e, &l, release(10, 10));
        let focused = r.focused();
        assert!(focused.is_some(), "precondition: the keyboard is somewhere");

        let (msgs, _) = r.pointer(&t, &e, &l, turn(300, 10, 1));
        assert!(matches!(msgs.as_slice(), [Msg::Wheel(_)]), "only the wheel: {msgs:?}");
        assert_eq!(r.focused(), focused, "scrolling over a widget must not take the keyboard");

        let (msgs, _) = r.pointer(&t, &e, &l, release(300, 10));
        assert!(!msgs.contains(&Msg::Pressed(9)), "a wheel became half a click: {msgs:?}");
    }
}
