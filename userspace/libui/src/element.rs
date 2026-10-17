//! `Element` — what an application's `view` returns.
//!
//! An `Element` is a **description**, not a widget: built fresh every time `view` runs and dropped
//! once the diff has consumed it. **Anything the application owns belongs in its state and arrives
//! through `view`**; a retained widget keeps only *interaction* state (a scrollbar's drag origin, a
//! menu's open item).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use libdraw::format::Rgb;
use libdraw::geom::Size;
use libdraw::theme::MIN_FONT_PX;
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

/// Which glyph an [`Icon`](Node::Icon) draws: a window control, or the power symbol.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IconKind {
    /// A bar along the bottom of the glyph box.
    Minimise,
    /// An empty square.
    Maximise,
    /// Two crossed strokes.
    Close,
    /// **The power symbol**: a ring open at the top, with a bar down through the gap to the centre.
    Power,
}

/// A step on the text-size scale, relative to the theme's one `font_px`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TextSize {
    /// Metadata read second: a status bar, a column header, a chord or a path beside a menu row.
    Small,
    /// Everything else — `font_px` itself.
    #[default]
    Body,
    /// The one line that heads a surface: the top bar's words.
    Large,
}

impl TextSize {
    /// How much smaller [`Small`](Self::Small) is than the body: 10.5 at the staged 12.
    pub const SMALL: f32 = 0.875;
    /// How much larger [`Large`](Self::Large) is: 13 at the staged 12.
    pub const LARGE: f32 = 13.0 / 12.0;

    /// This step's size in pixels, for a body size of `body`.
    ///
    /// **Never below [`MIN_FONT_PX`]**, the floor a theme's `font_px` is held to.
    pub fn px(self, body: f32) -> f32 {
        let px = match self {
            TextSize::Small => body * Self::SMALL,
            TextSize::Body => body,
            TextSize::Large => body * Self::LARGE,
        };
        px.max(MIN_FONT_PX)
    }
}

/// How a run of text is set: its size step, and whether it is bold or mono.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TextStyle {
    /// The step on the size scale.
    pub size: TextSize,
    /// Drawn in the face's bold companion, if it has one — `libdraw::text::Font::bold`.
    pub bold: bool,
    /// Whether it is set in the face's fixed-advance companion.
    pub mono: bool,
}

/// What an element *is*.
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
    Dock {
        /// Edge-pinned children, applied in order — each takes its slice of what is left.
        edges: Vec<Docked<Msg>>,
        /// The child that takes everything remaining.
        fill: Box<Element<Msg>>,
    },
    /// Children overlaid in paint order, each filling the whole area; hit-tested in reverse.
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
    /// **A zero component means "whatever the parent gives"**: `sized(12 × 0)` is a full-height
    /// strip, `sized(0 × 16)` a full-width bar.
    ///
    /// The constraint binds the node's **own** rectangle, not merely its measured extent, so a
    /// `sized` inside a `Stack` does not take the whole overlay.
    Sized {
        /// The constraint; zero on an axis means unconstrained.
        size: Size,
        /// The child.
        child: Box<Element<Msg>>,
    },
    /// A child placed in the middle of what its parent gives it.
    ///
    /// **Down is always; across is a choice**: a label *beside* a control (the chooser's path)
    /// wants the vertical half only, or a short path floats in the middle of the dialog and its
    /// left edge moves with the string.
    Center {
        /// Whether the child is centred across its rectangle as well as down it.
        across: bool,
        /// The child.
        child: Box<Element<Msg>>,
    },
    /// A child drawn in a different ink.
    ///
    /// **A wrapper rather than a colour on [`Text`](Self::Text)**, because ink is inherited:
    /// `paint` threads it down the subtree; nesting is innermost-wins, and it changes no geometry.
    Ink {
        /// The colour text and icons inside are drawn in.
        colour: Rgb,
        /// The child.
        child: Box<Element<Msg>>,
    },
    /// A child whose text is drawn at another step of the size scale — see [`TextSize`]; unlike
    /// `Ink` it *does* change geometry.
    Scale {
        /// The step everything inside is set at.
        size: TextSize,
        /// The child.
        child: Box<Element<Msg>>,
    },
    /// A child whose text is set in the fixed-advance face.
    Mono {
        /// What is set in it.
        child: Box<Element<Msg>>,
    },
    /// A child whose text is bold — a window's title.
    Bold {
        /// The child.
        child: Box<Element<Msg>>,
    },
    /// A rectangle of flat colour; it measures to nothing and takes what it is given (wrap it in
    /// [`sized`] for a size).
    Fill(Rgb),
    /// A face with the theme's bevel: lightened at the top, darkened at the bottom of the node's
    /// own box; **a gradient is only correct from the node's rect, not the clip**.
    Bevel(Rgb),
    /// The one-pixel border of a window or a popup, with its corners rounded to `radius`.
    ///
    /// **Drawn last, over the surface's content**, because its corner curves inward over whatever
    /// is in the window's corners; **only for a surface the compositor cuts to the same curve**.
    Outline {
        /// The border's colour.
        colour: Rgb,
        /// The corner radius — `libdraw::corner::WINDOW_RADIUS`, which the compositor cuts with.
        radius: u32,
    },
    /// A rectangle of flat colour with rounded corners, antialiased against what is beneath it;
    /// **its ground must be painted before it**, like a [`Wash`](Self::Wash)'s.
    RoundedFill {
        /// The colour.
        colour: Rgb,
        /// The corner radius, clamped to half the shorter side.
        radius: u32,
    },
    /// A translucent wash of `colour` at `coverage` over whatever is beneath it: selection is the
    /// accent at 20%, hover at 10% (18% dark).
    ///
    /// **Its ground must be painted before it in the same repaint**: a wash repainted over itself
    /// darkens a step per frame.
    Wash {
        /// The colour washed over.
        colour: Rgb,
        /// How much of it: 0 is nothing, 255 is a fill.
        coverage: u8,
    },
    /// One of the three window-control glyphs, drawn rather than typed.
    Icon(IconKind),
    /// A child shifted within its parent, taking its own measured size.
    ///
    /// **The one place absolute positioning exists, and it exists for overlays** such as a menu
    /// popup placed under the item that opened it.
    Offset {
        /// Rightward shift from the parent's origin.
        dx: i32,
        /// Downward shift from the parent's origin.
        dy: i32,
        /// The child, which takes the size it measures rather than the space it is offered.
        child: Box<Element<Msg>>,
    },
    /// An application-drawn node, the escape hatch: it measures to `size` and the application
    /// paints it.
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
    /// `None` means "pair me by position". Dynamic lists must set it, or the diff pairs row 2's
    /// widget with row 3's element.
    pub key: Option<u64>,
    /// Share of a `Row`/`Column`'s leftover space; `0` means "just my measured size".
    pub flex: u16,
    /// What this element is.
    pub node: Node<Msg>,
    /// The message a click on this element produces.
    pub on_press: Option<Msg>,
    /// A press going **down** on this element, before it is known whether it becomes a click.
    ///
    /// **For gestures that begin at the press** (dragging a window by its title bar); **a nearer
    /// [`on_press`](Self::on_press) shadows it**, so pressing the close button does not also start
    /// a move.
    pub on_press_down: Option<Msg>,
    /// A drag-and-drop payload released over this element — `Surface::Dropped`.
    ///
    /// **Routed by position, exactly like a press**, so where on a window a drop is allowed is
    /// decided here; **the message carries no payload**.
    pub on_drop: Option<Msg>,
    /// Raw key events, while this element holds widget focus.
    ///
    /// **Returns `Option`, so a handler can decline** and let the key bubble.
    pub on_key: Option<fn(KeyEvent) -> Option<Msg>>,
    /// Raw pointer events routed to this element, **never a wheel**.
    pub on_pointer: Option<fn(PointerEvent) -> Msg>,
    /// The wheel turned over this element — a `POINTER_WHEEL` record.
    ///
    /// **Its own handler rather than an `on_pointer` kind**: the thing that scrolls is rarely the
    /// thing under the cursor, so a wheel must bubble past widgets that merely track the cursor.
    pub on_wheel: Option<fn(PointerEvent) -> Msg>,
    /// Whether this element accepts keyboard focus; opt-in, not derived from having an `on_key`.
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
            on_drop: None,
            on_key: None,
            on_pointer: None,
            on_wheel: None,
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

    /// Send `msg` when a drag is released over this element.
    pub fn on_drop(mut self, msg: Msg) -> Self {
        self.on_drop = Some(msg);
        self
    }

    /// Send `msg` when a press goes **down** on this element.
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
    pub fn on_key(mut self, f: fn(KeyEvent) -> Option<Msg>) -> Self {
        self.on_key = Some(f);
        self.focusable = true;
        self
    }

    /// Take raw pointer events routed to this element — everything but the wheel.
    pub fn on_pointer(mut self, f: fn(PointerEvent) -> Msg) -> Self {
        self.on_pointer = Some(f);
        self
    }

    /// Take the wheel; **claiming it stops it**: the nearest ancestor with a handler gets it.
    pub fn on_wheel(mut self, f: fn(PointerEvent) -> Msg) -> Self {
        self.on_wheel = Some(f);
        self
    }

    /// Accept keyboard focus.
    pub fn focusable(mut self) -> Self {
        self.focusable = true;
        self
    }

    /// This element's children, if it has any, in paint order.
    pub fn children(&self) -> impl Iterator<Item = &Element<Msg>> {
        // `Dock` yields its edge children before the filler: the order layout consumes them in and
        // paints.
        #[allow(clippy::type_complexity, reason = "a local annotation for one match, used nowhere else")]
        let (a, b, c): (&[Element<Msg>], Option<&Element<Msg>>, Option<&Element<Msg>>) = match &self.node {
            Node::Text(_)
            | Node::Fill(_)
            | Node::Bevel(_)
            | Node::Outline { .. }
            | Node::RoundedFill { .. }
            | Node::Wash { .. }
            | Node::Icon(_)
            | Node::Custom { .. } => (&[], None, None),
            Node::Column { children, .. } | Node::Row { children, .. } | Node::Stack(children) => {
                (children.as_slice(), None, None)
            }
            Node::Padding { child, .. }
            | Node::Sized { child, .. }
            | Node::Offset { child, .. }
            | Node::Ink { child, .. }
            | Node::Scale { child, .. }
            | Node::Bold { child }
            | Node::Mono { child }
            | Node::Center { child, .. } => (&[], Some(child), None),
            Node::Dock { fill, .. } => (&[], None, Some(fill)),
        };
        let docked: Option<&Vec<Docked<Msg>>> = match &self.node {
            Node::Dock { edges, .. } => Some(edges),
            _ => None,
        };
        a.iter().chain(docked.into_iter().flatten().map(|d| &d.element)).chain(b).chain(c)
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

/// A rounded window border — see [`Node::Outline`].
pub fn outline<Msg>(colour: Rgb, radius: u32) -> Element<Msg> {
    Element::new(Node::Outline { colour, radius })
}

/// A rounded rectangle of flat colour, inside a surface — see [`Node::RoundedFill`].
pub fn rounded_fill<Msg>(colour: Rgb, radius: u32) -> Element<Msg> {
    Element::new(Node::RoundedFill { colour, radius })
}

/// A translucent wash — see [`Node::Wash`].
pub fn wash<Msg>(colour: Rgb, coverage: u8) -> Element<Msg> {
    Element::new(Node::Wash { colour, coverage })
}

/// A face drawn with the theme's bevel — see [`Node::Bevel`].
pub fn bevel<Msg>(colour: Rgb) -> Element<Msg> {
    Element::new(Node::Bevel(colour))
}

/// A glyph — a window control, or the power symbol — see [`Node::Icon`].
pub fn icon<Msg>(kind: IconKind) -> Element<Msg> {
    Element::new(Node::Icon(kind))
}

/// Text at another step of the size scale — see [`Node::Scale`].
pub fn scaled<Msg>(size: TextSize, child: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Scale { size, child: Box::new(child) })
}

/// Text in the fixed-advance face — see [`Node::Mono`].
pub fn mono<Msg>(child: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Mono { child: Box::new(child) })
}

/// Text inside `child`, in the face's bold. See [`Node::Bold`].
pub fn bold<Msg>(child: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Bold { child: Box::new(child) })
}

/// A child in the middle of its parent's rectangle — see [`Node::Center`].
pub fn center<Msg>(child: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Center { across: true, child: Box::new(child) })
}

/// A child centred *down* its parent's rectangle only — see [`Node::Center`].
pub fn center_v<Msg>(child: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Center { across: false, child: Box::new(child) })
}

/// A child drawn in `colour` — see [`Node::Ink`].
pub fn ink<Msg>(colour: Rgb, child: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Ink { colour, child: Box::new(child) })
}

/// A child shifted `(dx, dy)` from its parent's origin, at its own measured size.
pub fn offset<Msg>(dx: i32, dy: i32, child: Element<Msg>) -> Element<Msg> {
    Element::new(Node::Offset { dx, dy, child: Box::new(child) })
}

/// An application-drawn node.
pub fn custom<Msg>(kind: u32, size: Size) -> Element<Msg> {
    Element::new(Node::Custom { kind, size })
}

/// Set the gap between a `Row`'s or `Column`'s children; a no-op on anything else.
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

    type Msg = ();
    use alloc::vec;

    /// The steps hold the theme's own floor at any size a theme file can ask for.
    #[test]
    fn no_step_goes_under_the_font_floor() {
        use libdraw::theme::{MAX_FONT_PX, MIN_FONT_PX};
        assert_eq!(TextSize::Small.px(MIN_FONT_PX), MIN_FONT_PX);
        assert_eq!(TextSize::Body.px(MIN_FONT_PX), MIN_FONT_PX);
        assert!(TextSize::Large.px(MIN_FONT_PX) > MIN_FONT_PX);
        let mut px = MIN_FONT_PX;
        while px <= MAX_FONT_PX {
            for step in [TextSize::Small, TextSize::Body, TextSize::Large] {
                let at = step.px(px);
                assert!(at >= MIN_FONT_PX, "{step:?} of {px} is {at}, under the floor");
            }
            px += 0.01;
        }
        let small = TextSize::Small.px(12.0);
        let (body, large) = (TextSize::Body.px(12.0), TextSize::Large.px(12.0));
        assert!(small < body && body < large, "{small} {body} {large}");
        assert_eq!(body, 12.0);
    }

    #[test]
    fn builders_carry_key_and_flex_without_changing_the_node() {
        let e: Element<Msg> = text("hi").key(7).flex(2);
        assert_eq!(e.key, Some(7));
        assert_eq!(e.flex, 2);
        assert!(matches!(e.node, Node::Text(ref s) if s == "hi"));
    }

    #[test]
    fn children_yields_every_shape_and_nothing_extra() {
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
        // The order layout consumes them in and the order they are painted; the diff walks it too.
        let d: Element<Msg> =
            dock(vec![docked(Edge::Top, text("bar")), docked(Edge::Right, text("scroll"))], text("fill"));
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
