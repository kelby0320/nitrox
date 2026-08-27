//! `Element` — what an application's `view` returns.
//!
//! An `Element` is a **description**, not a widget. It is built fresh every time `view` runs
//! and dropped once the diff has consumed it; the retained widget that survives between
//! frames is the diff's business, not this module's.
//!
//! That split decides where state may live, which is the part worth being firm about:
//! **anything the application owns belongs in its state and arrives through `view`.** What
//! a retained widget keeps is strictly *interaction* state — a scrollbar's drag origin, a
//! menu's open item — which the application has no opinion about and would be tedious to
//! thread through messages.
//!
//! ## Handlers are `fn` pointers, not boxed closures
//!
//! `Msg` arrived in Part B, with the routing that can fire it. A handler taking event data
//! is `Option<fn(KeyEvent) -> Msg>` rather than `Option<Box<dyn Fn(..) -> Msg>>`, and the
//! reason is that **a Rust tuple-variant constructor is already a `fn` pointer**: an
//! application writes `.on_key(Msg::Key)` and gets exactly what Iced spells with a closure.
//!
//! **The reason is `Clone` and `Debug`, not allocation.** `Box<dyn Fn>` is neither, so
//! `#[derive(Clone, Debug)]` on `Element` stops compiling and the diff's tests lose the
//! `assert_eq!` they are written with; `Rc<dyn Fn>` restores `Clone` but still needs a
//! hand-written `Debug`. A `fn` pointer is `Copy`, `Clone` and `Debug` for free, and reads
//! identically at every call site we have.
//!
//! An earlier version of this comment also claimed a box would cost an allocation per
//! handler on a hand-rolled heap. That argument does not survive contact with the rest of
//! the type: `view` already allocates a `Vec` per container, a `Box` per `padding`/`sized`
//! child and a `String` per `text`, so a handler box is a small fraction of a number that
//! is already fine — and `view` runs on *events*, not at frame rate. The claim is struck
//! rather than quietly deleted, because a justification nobody believes is worse than none.
//!
//! **The real limit is that a `fn` cannot capture**, and it is narrower than it sounds.
//! `on_press` takes a *value*, so `Msg::Select(i)` captures `i` perfectly well. It bites
//! only the handlers that take event data: a list row wanting both the event and its own
//! index — `Msg::RowKey(i, event)` — cannot be spelled. Expect that in Part C or Milestone
//! 5; the fix then is an `Rc<dyn Fn>` variant *alongside* this one, which is additive.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use libdraw::format::Rgb;
use libdraw::geom::Size;
use librsproto::surface::{KeyEvent, PointerEvent};

/// Which edge a docked child is pinned to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edge {
    /// Pinned to the top, full width.
    Top,
    /// Pinned to the bottom, full width.
    Bottom,
    /// Pinned to the left, full height of what remains.
    Left,
    /// Pinned to the right, full height of what remains.
    Right,
}

/// Space added around a child, in pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Insets {
    /// Space above.
    pub top: u32,
    /// Space to the right.
    pub right: u32,
    /// Space below.
    pub bottom: u32,
    /// Space to the left.
    pub left: u32,
}

impl Insets {
    /// The same inset on all four sides.
    pub const fn all(n: u32) -> Self {
        Self { top: n, right: n, bottom: n, left: n }
    }

    /// Total horizontal inset.
    pub const fn horizontal(&self) -> u32 {
        self.left + self.right
    }

    /// Total vertical inset.
    pub const fn vertical(&self) -> u32 {
        self.top + self.bottom
    }
}

/// A docked child and the edge it is pinned to.
#[derive(Clone, Debug)]
pub struct Docked<Msg> {
    /// Which edge it occupies.
    pub edge: Edge,
    /// The child itself.
    pub element: Element<Msg>,
}

/// What an element *is*.
///
/// Deliberately short. `docs/architecture/widget-toolkit.md` §1's rule is that anything Milestone
/// 5's terminal does not use is not in Milestone 4, and these are the structural nodes the
/// diff and layout need in order to be tested at all. The interactive widgets — button,
/// menu, scrollbar — arrive in Part C, pulled into existence by the terminal.
#[derive(Clone, Debug)]
pub enum Node<Msg> {
    /// A run of text, measured by the caller's [`Metrics`](crate::layout::Metrics).
    Text(String),
    /// Children stacked top to bottom.
    Column {
        /// Gap between adjacent children.
        spacing: u32,
        /// The children, in order.
        children: Vec<Element<Msg>>,
    },
    /// Children stacked left to right.
    Row {
        /// Gap between adjacent children.
        spacing: u32,
        /// The children, in order.
        children: Vec<Element<Msg>>,
    },
    /// Children pinned to edges, with one child filling what remains.
    ///
    /// The filler is a **named field rather than "the last child"**. A positional rule reads
    /// fine and then silently changes meaning the first time somebody appends a docked child
    /// to the end of the list.
    Dock {
        /// Edge-pinned children, applied in order — each takes its slice of what is left.
        edges: Vec<Docked<Msg>>,
        /// The child that takes everything remaining.
        fill: Box<Element<Msg>>,
    },
    /// Children overlaid in paint order, each filling the whole area.
    ///
    /// The overlay node menus are drawn into. Painted first to last, so the last child is on
    /// top — and hit-tested in the reverse of that, once Part B hit-tests anything.
    Stack(Vec<Element<Msg>>),
    /// A child with space around it.
    Padding {
        /// How much space, per side.
        insets: Insets,
        /// The child.
        child: Box<Element<Msg>>,
    },
    /// A child constrained on one or both axes.
    ///
    /// **A zero component means "whatever the parent gives"**, so `sized(12 × 0)` is a
    /// full-height 12-wide strip — a scrollbar — and `sized(0 × 16)` is a full-width bar.
    /// Without that convention every fixed-size child would have to name a cross-axis extent
    /// it does not care about, and would then be wrong the moment the window resized.
    ///
    /// The constraint binds the node's **own** rectangle, not merely its measured extent. An
    /// earlier version passed the parent's rectangle straight through, so a `sized` inside a
    /// `Stack` took the whole overlay and the doc claiming "an exact size" was false
    /// (PR #183 review, finding 5).
    Sized {
        /// The constraint; zero on an axis means unconstrained.
        size: Size,
        /// The child.
        child: Box<Element<Msg>>,
    },
    /// A rectangle of flat colour.
    ///
    /// The one painting primitive the composites need: a button's face, a scrollbar's track
    /// and thumb, a menu's backing. Without it every widget that is not text would have to be
    /// a `Custom`, and the application would end up painting the toolkit's own chrome.
    ///
    /// It measures to nothing and takes what it is given — a colour has no natural size, and
    /// a caller that wants one wraps it in [`sized`].
    Fill(Rgb),
    /// A child shifted within its parent, taking its own measured size.
    ///
    /// **The one place absolute positioning exists, and it exists for overlays.** §5 rules it
    /// out for ordinary widgets — "a terminal is resized constantly and every widget would need
    /// to recompute, which is a layout engine written in application code" — and that argument
    /// does not reach a menu popup, whose whole definition is "here, under the item that opened
    /// it". Without this a popup can only be placed by computing `Padding` insets in the
    /// application, which *is* the layout engine §5 refused.
    ///
    /// The offset is relative to the parent's origin, so a popup inside a window-level `Stack`
    /// is placed in window coordinates and moves with the window rather than with the screen.
    Offset {
        /// Rightward shift from the parent's origin.
        dx: i32,
        /// Downward shift from the parent's origin.
        dy: i32,
        /// The child, which takes the size it measures rather than the space it is offered.
        child: Box<Element<Msg>>,
    },
    /// An application-drawn node: the escape hatch.
    ///
    /// Opaque to the toolkit — it measures to `size` and the application paints it. This is
    /// what Milestone 5's terminal grid is, and it is deliberately a first-class node rather
    /// than an afterthought: the flagship application is an escape-hatch client, so the
    /// toolkit supplies chrome, layout and input plumbing and gets out of the way where the
    /// application knows better.
    Custom {
        /// Application-chosen discriminator, so a diff can tell two custom nodes apart.
        kind: u32,
        /// The size it wants.
        size: Size,
    },
}

/// A node, plus the properties every node carries.
#[derive(Clone, Debug)]
pub struct Element<Msg> {
    /// Identity within the parent, for the diff.
    ///
    /// `None` means "pair me by position", which is right for the fixed structural nesting
    /// that makes up most of a UI. Dynamic lists — anything reordered, inserted into, or
    /// removed from — must set it, or the diff pairs row 2's widget with row 3's element.
    pub key: Option<u64>,
    /// Share of a `Row`/`Column`'s leftover space; `0` means "just my measured size".
    pub flex: u16,
    /// What this element is.
    pub node: Node<Msg>,
    /// The message a click on this element produces.
    ///
    /// A *click* — a press and its release, both inside — rather than a bare press, because
    /// pressing a button and sliding off it is how a user cancels, and a toolkit that fired
    /// on the press removes that.
    pub on_press: Option<Msg>,
    /// A press going **down** on this element, before it is known whether it becomes a click.
    ///
    /// **For gestures that begin at the press**, which a click cannot express: dragging a
    /// window by its title bar is decided the moment the button goes down, and a handler that
    /// waited for the release would start the move after the user had finished making it.
    ///
    /// **A nearer [`on_press`](Self::on_press) shadows this**, which is the rule that lets a
    /// title bar carry a drag and still hold buttons: a widget that handles clicks handles the
    /// press that begins them, so pressing close does not also start a move. Dispatch is in
    /// [`Router::pointer`](crate::route::Router::pointer).
    pub on_press_down: Option<Msg>,
    /// Raw key events, while this element holds widget focus.
    ///
    /// The path the terminal grid uses: a `custom` widget that wants keycodes rather than a
    /// toolkit's interpretation of them.
    ///
    /// **Returns `Option`, so a handler can decline** and let the key keep bubbling. Without
    /// that, having a handler *means* handling: a focused text field would swallow Ctrl-O,
    /// and the menu accelerator this design uses as its motivating example could never fire.
    /// A non-capturing closure coerces to a `fn` pointer, so `.on_key(|k| Some(Msg::Key(k)))`
    /// costs nothing.
    pub on_key: Option<fn(KeyEvent) -> Option<Msg>>,
    /// Raw pointer events routed to this element.
    pub on_pointer: Option<fn(PointerEvent) -> Msg>,
    /// Whether this element accepts keyboard focus.
    ///
    /// Opt-in, not derived from having an `on_key`: a scrollbar has neither and must still
    /// be skipped by Tab, while a container may want focus without handling a key itself.
    pub focusable: bool,
}

impl<Msg> Element<Msg> {
    /// An element with no key, no flex, and no handlers.
    pub fn new(node: Node<Msg>) -> Self {
        Self {
            key: None,
            flex: 0,
            node,
            on_press: None,
            on_press_down: None,
            on_key: None,
            on_pointer: None,
            focusable: false,
        }
    }

    /// Give this element an identity within its parent.
    pub fn key(mut self, key: u64) -> Self {
        self.key = Some(key);
        self
    }

    /// Give this element a share of its parent's leftover space.
    pub fn flex(mut self, flex: u16) -> Self {
        self.flex = flex;
        self
    }

    /// Send `msg` when a press goes **down** on this element, before it is known whether it
    /// becomes a click — see [`Node::on_press_down`](crate::element::Element).
    pub fn on_press_down(mut self, msg: Msg) -> Self {
        self.on_press_down = Some(msg);
        self
    }

    /// The message a click produces.
    pub fn on_press(mut self, msg: Msg) -> Self {
        self.on_press = Some(msg);
        self
    }

    /// Take raw key events while focused. Implies [`focusable`](Self::focusable).
    ///
    /// Implied, because a handler for keys that can never be focused is dead code the
    /// application would have to notice it wrote. Something that wants focus *without*
    /// handling keys still says so explicitly.
    pub fn on_key(mut self, f: fn(KeyEvent) -> Option<Msg>) -> Self {
        self.on_key = Some(f);
        self.focusable = true;
        self
    }

    /// Take raw pointer events routed to this element.
    pub fn on_pointer(mut self, f: fn(PointerEvent) -> Msg) -> Self {
        self.on_pointer = Some(f);
        self
    }

    /// Accept keyboard focus.
    pub fn focusable(mut self) -> Self {
        self.focusable = true;
        self
    }

    /// This element's children, if it has any, in paint order.
    pub fn children(&self) -> impl Iterator<Item = &Element<Msg>> {
        // One iterator type for every shape, so callers do not each re-derive the match.
        // `Dock` yields its edge children before the filler, which is also the order layout
        // consumes them in and the order they are painted.
        let (a, b, c): (&[Element<Msg>], Option<&Element<Msg>>, Option<&Element<Msg>>) =
            match &self.node {
            Node::Text(_) | Node::Fill(_) | Node::Custom { .. } => (&[], None, None),
            Node::Column { children, .. } | Node::Row { children, .. } | Node::Stack(children) => {
                (children.as_slice(), None, None)
            }
            Node::Padding { child, .. }
            | Node::Sized { child, .. }
            | Node::Offset { child, .. } => (&[], Some(child), None),
            Node::Dock { fill, .. } => (&[], None, Some(fill)),
        };
        let docked: Option<&Vec<Docked<Msg>>> = match &self.node {
            Node::Dock { edges, .. } => Some(edges),
            _ => None,
        };
        a.iter()
            .chain(docked.into_iter().flatten().map(|d| &d.element))
            .chain(b)
            .chain(c)
    }
}

/// A run of text.
pub fn text<Msg>(s: impl Into<String>) -> Element<Msg> {
    Element::new(Node::Text(s.into()))
}

/// Children stacked top to bottom.
pub fn column<Msg>(children: impl Into<Vec<Element<Msg>>>) -> Element<Msg> {
    Element::new(Node::Column { spacing: 0, children: children.into() })
}

/// Children stacked left to right.
pub fn row<Msg>(children: impl Into<Vec<Element<Msg>>>) -> Element<Msg> {
    Element::new(Node::Row { spacing: 0, children: children.into() })
}

/// Children overlaid, last on top.
pub fn stack<Msg>(children: impl Into<Vec<Element<Msg>>>) -> Element<Msg> {
    Element::new(Node::Stack(children.into()))
}

/// Edge-pinned children around one that fills the rest.
pub fn dock<Msg>(edges: impl Into<Vec<Docked<Msg>>>, fill: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Dock { edges: edges.into(), fill: Box::new(fill) })
}

/// Pin `element` to `edge` inside a [`dock`].
pub fn docked<Msg>(edge: Edge, element: Element<Msg>) -> Docked<Msg> {
    Docked { edge, element }
}

/// A child with space around it.
pub fn padding<Msg>(insets: Insets, child: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Padding { insets, child: Box::new(child) })
}

/// A child constrained on one or both axes; a zero component means unconstrained.
pub fn sized<Msg>(size: Size, child: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Sized { size, child: Box::new(child) })
}

/// A rectangle of flat colour, filling whatever it is given.
pub fn fill<Msg>(colour: Rgb) -> Element<Msg> {
    Element::new(Node::Fill(colour))
}

/// A child shifted `(dx, dy)` from its parent's origin, at its own measured size.
pub fn offset<Msg>(dx: i32, dy: i32, child: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Offset { dx, dy, child: Box::new(child) })
}

/// An application-drawn node.
pub fn custom<Msg>(kind: u32, size: Size) -> Element<Msg> {
    Element::new(Node::Custom { kind, size })
}

/// Set the gap between a `Row`'s or `Column`'s children.
///
/// A no-op on anything else rather than an error: `spacing` is a layout hint, and refusing
/// it on a `Text` would make every builder chain conditional on the node kind.
pub fn with_spacing<Msg>(mut e: Element<Msg>, gap: u32) -> Element<Msg> {
    match &mut e.node {
        Node::Column { spacing, .. } | Node::Row { spacing, .. } => *spacing = gap,
        _ => {}
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Part A's tests carry no messages, and `()` is the simplest inhabited `Msg`. Part B's
    /// routing tests use a real enum; these are about shape, not about what a click means.
    type Msg = ();
    use alloc::vec;

    #[test]
    fn builders_carry_key_and_flex_without_changing_the_node() {
        let e: Element<Msg> = text("hi").key(7).flex(2);
        assert_eq!(e.key, Some(7));
        assert_eq!(e.flex, 2);
        assert!(matches!(e.node, Node::Text(ref s) if s == "hi"));
    }

    #[test]
    fn children_yields_every_shape_and_nothing_extra() {
        // Counted through a pinned binding rather than a turbofish per line: every one of
        // these is a `text("a")` whose `Msg` nothing else constrains.
        let n = |e: Element<Msg>| e.children().count();
        assert_eq!(n(text("a")), 0);
        assert_eq!(n(custom(1, Size::new(4, 4))), 0);
        assert_eq!(n(column(vec![text("a"), text("b")])), 2);
        assert_eq!(n(row(vec![text("a")])), 1);
        assert_eq!(n(stack(vec![text("a"), text("b"), text("c")])), 3);
        assert_eq!(n(padding(Insets::all(1), text("a"))), 1);
        assert_eq!(n(sized(Size::new(2, 2), text("a"))), 1);
    }

    #[test]
    fn a_docks_children_come_before_its_filler() {
        // The order layout consumes them in and the order they are painted. The diff walks
        // this too, so a different order here would silently re-pair every docked child
        // with the filler.
        let d: Element<Msg> =
            dock(vec![docked(Edge::Top, text("bar")), docked(Edge::Right, text("scroll"))],
                 text("fill"));
        let kinds: Vec<&str> = d
            .children()
            .map(|c| match &c.node {
                Node::Text(s) => s.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(kinds, ["bar", "scroll", "fill"]);
    }

    #[test]
    fn spacing_applies_to_the_two_nodes_that_have_it_and_is_ignored_elsewhere() {
        let c: Element<Msg> = with_spacing(column(vec![text("a")]), 4);
        assert!(matches!(c.node, Node::Column { spacing: 4, .. }));
        let r: Element<Msg> = with_spacing(row(vec![text("a")]), 3);
        assert!(matches!(r.node, Node::Row { spacing: 3, .. }));
        // Ignored rather than refused: a builder chain must not become conditional on kind.
        let t: Element<Msg> = with_spacing(text("a"), 9);
        assert!(matches!(t.node, Node::Text(_)));
    }

    #[test]
    fn insets_sum_per_axis() {
        let i = Insets { top: 1, right: 2, bottom: 3, left: 4 };
        assert_eq!(i.horizontal(), 6);
        assert_eq!(i.vertical(), 4);
        assert_eq!(Insets::all(5).horizontal(), 10);
    }
}
