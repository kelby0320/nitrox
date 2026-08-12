//! Measure and arrange: constraints down, sizes up, then rectangles.
//!
//! Two passes, the model Flutter uses and the one that survives contact with a scrollbar:
//!
//! 1. **Measure.** The parent passes a min/max box down; each child returns the size it wants
//!    within it.
//! 2. **Arrange.** The parent assigns each child a rectangle.
//!
//! Rectangles come out **absolute** — in the window's coordinates, not the parent's. The
//! design document describes arrange as assigning "a rectangle in its own coordinates", and
//! this is the same thing with the origin already folded in: every consumer of a layout
//! (painting, damage, and later hit-testing) wants window coordinates, and having each of
//! them re-accumulate offsets down the tree is three chances to get it wrong instead of one.
//!
//! **There is no font here.** Text is measured through [`Metrics`], which the caller
//! supplies, because glyph rendering is Milestone 5 and layout must not wait for it.

use alloc::vec::Vec;

use libdraw::geom::{Point, Rect, Size};

use crate::element::{Edge, Element, Node};

/// How big a run of text is.
///
/// A trait rather than a font because `libdraw` has no glyphs until Milestone 5 Part A, and
/// because a terminal, a menu and a status line may not measure the same way forever.
pub trait Metrics {
    /// The size `s` occupies when drawn.
    fn text_size(&self, s: &str) -> Size;
}

/// A fixed-cell metric: every character the same box.
///
/// What a bitmap font gives, and enough for every text this milestone lays out.
#[derive(Clone, Copy, Debug)]
pub struct FixedCell {
    /// Width of one character cell.
    pub w: u32,
    /// Height of one character cell.
    pub h: u32,
}

impl Metrics for FixedCell {
    fn text_size(&self, s: &str) -> Size {
        // `chars()`, not `len()`: a byte count is a character count only for ASCII, and
        // getting that wrong makes every non-ASCII label mis-sized rather than mis-drawn,
        // which is much harder to spot.
        Size::new(self.w.saturating_mul(s.chars().count() as u32), self.h)
    }
}

/// The box a child must size itself within.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Constraints {
    /// Smallest acceptable size.
    pub min: Size,
    /// Largest acceptable size.
    pub max: Size,
}

impl Constraints {
    /// Anything up to `max`.
    pub const fn loose(max: Size) -> Self {
        Self { min: Size::new(0, 0), max }
    }

    /// Exactly `size`.
    pub const fn tight(size: Size) -> Self {
        Self { min: size, max: size }
    }

    /// `size`, clamped into this box.
    pub fn constrain(&self, size: Size) -> Size {
        Size::new(size.w.clamp(self.min.w, self.max.w), size.h.clamp(self.min.h, self.max.h))
    }

    /// The same box with `w`/`h` removed from `max`, saturating at zero.
    ///
    /// Saturating matters: a `Padding` wider than the space it was given would otherwise
    /// wrap to an enormous `max`, and the child would measure as if it had the whole screen.
    pub fn shrink(&self, w: u32, h: u32) -> Self {
        Self {
            min: Size::new(
                self.min.w.saturating_sub(w).min(self.max.w.saturating_sub(w)),
                self.min.h.saturating_sub(h).min(self.max.h.saturating_sub(h)),
            ),
            max: Size::new(self.max.w.saturating_sub(w), self.max.h.saturating_sub(h)),
        }
    }
}

/// Where an element ended up, and where its children did.
///
/// Parallel in shape to the [`Element`] it came from: `children` is in the same order as
/// [`Element::children`], so the two walk together.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Layout {
    /// The element's rectangle, in window coordinates.
    pub rect: Rect,
    /// Its children's layouts, in the same order as [`Element::children`].
    pub children: Vec<Layout>,
}

impl Layout {
    /// A leaf at `rect`.
    pub fn leaf(rect: Rect) -> Self {
        Self { rect, children: Vec::new() }
    }
}

/// The size `e` wants, within `c`.
pub fn measure<M: Metrics + ?Sized, Msg>(e: &Element<Msg>, c: Constraints, m: &M) -> Size {
    let size = match &e.node {
        Node::Text(s) => m.text_size(s),
        // A colour has no natural size, and a caller wanting a particular one wraps it in
        // `sized` or gives it `flex`.
        //
        // **Zero, not `c.max`.** It was `c.max` until 2026-08-11, on the stated grounds that
        // zero "would make a button's face collapse inside a `Column`" — which is not what
        // happens, because `arrange` gives every `Stack` layer the whole rect regardless of
        // what it measured. What `c.max` *did* do was make every `Stack`-based widget greedy:
        // `button` and `menu_bar` are stacks over a `Fill`, so the first one in a `Row` or
        // `Column` measured to the entire remaining extent and its siblings got nothing. A
        // two-item menu bar laid its second item out at zero width, off the right edge.
        //
        // Nothing caught it because every widget test lays *one* widget into a fixed
        // rectangle, where a greedy measure and a correct one give the same answer. Composing
        // the widget set into one UI (`reference::view`) is what surfaced it.
        Node::Fill(_) => Size::new(0, 0),
        Node::Custom { size, .. } => *size,
        // An offset child measures as itself: the shift moves it, it does not resize it. What
        // the *parent* sees is the child's size plus the shift, so a `Stack` that sized itself
        // to its layers would still contain the popup.
        Node::Offset { dx, dy, child } => {
            let inner = measure(child, c, m);
            Size::new(
                inner.w.saturating_add((*dx).max(0) as u32),
                inner.h.saturating_add((*dy).max(0) as u32),
            )
        }
        Node::Sized { size, child } => {
            // Zero means unconstrained, so the child's own measure stands on that axis.
            let inner = measure(child, c, m);
            Size::new(
                if size.w == 0 { inner.w } else { size.w },
                if size.h == 0 { inner.h } else { size.h },
            )
        }
        Node::Padding { insets, child } => {
            let inner = measure(child, c.shrink(insets.horizontal(), insets.vertical()), m);
            Size::new(
                inner.w.saturating_add(insets.horizontal()),
                inner.h.saturating_add(insets.vertical()),
            )
        }
        Node::Column { spacing, children } => {
            let gaps = spacing.saturating_mul(children.len().saturating_sub(1) as u32);
            let mut w = 0;
            let mut h = gaps;
            for ch in children {
                let s = measure(ch, c.shrink(0, h.min(c.max.h)), m);
                w = w.max(s.w);
                h = h.saturating_add(s.h);
            }
            Size::new(w, h)
        }
        Node::Row { spacing, children } => {
            let gaps = spacing.saturating_mul(children.len().saturating_sub(1) as u32);
            let mut w = gaps;
            let mut h = 0;
            for ch in children {
                let s = measure(ch, c.shrink(w.min(c.max.w), 0), m);
                w = w.saturating_add(s.w);
                h = h.max(s.h);
            }
            Size::new(w, h)
        }
        Node::Stack(children) => {
            // The union: an overlay is as big as its largest layer.
            let mut size = Size::new(0, 0);
            for ch in children {
                let s = measure(ch, c, m);
                size = Size::new(size.w.max(s.w), size.h.max(s.h));
            }
            size
        }
        // A dock takes everything it is offered — its whole purpose is to divide a given
        // area, so measuring it as the sum of its children would defeat the filler.
        Node::Dock { .. } => c.max,
    };
    c.constrain(size)
}

/// Place `e` at `rect` and everything inside it.
pub fn arrange<M: Metrics + ?Sized, Msg>(e: &Element<Msg>, rect: Rect, m: &M) -> Layout {
    // `Sized` and `Offset` are the two nodes that change their *own* rectangle rather than
    // only their children's. They have to: hit-testing and damage both read a node's rect, so
    // constraining only the child would leave a 12-pixel scrollbar claiming the whole overlay
    // it sits in — and an offset node whose rect stayed its parent's would break the
    // containment invariant `paint` relies on to skip a subtree, since its child would sit
    // outside it.
    let rect = match &e.node {
        Node::Sized { size, .. } => Rect::new(
            rect.origin.x,
            rect.origin.y,
            if size.w == 0 { rect.size.w } else { size.w.min(rect.size.w) },
            if size.h == 0 { rect.size.h } else { size.h.min(rect.size.h) },
        ),
        Node::Offset { dx, dy, child } => {
            // The child takes the size it *measures*, not the space it is offered — that is
            // what distinguishes this from `Padding`, which shrinks a child into the
            // remainder. A popup is as big as its contents wherever it lands.
            let want = measure(child, Constraints::loose(rect.size), m);
            Rect::new(
                rect.origin.x.saturating_add(*dx),
                rect.origin.y.saturating_add(*dy),
                want.w,
                want.h,
            )
        }
        _ => rect,
    };
    let children = match &e.node {
        Node::Text(_) | Node::Fill(_) | Node::Custom { .. } => Vec::new(),

        Node::Padding { insets, child } => {
            let inner = Rect::new(
                rect.origin.x.saturating_add(insets.left as i32),
                rect.origin.y.saturating_add(insets.top as i32),
                rect.size.w.saturating_sub(insets.horizontal()),
                rect.size.h.saturating_sub(insets.vertical()),
            );
            alloc::vec![arrange(child, inner, m)]
        }

        // `rect` is already the constrained one, computed above.
        Node::Sized { child, .. } => alloc::vec![arrange(child, rect, m)],

        // `rect` is already the offset one, computed above.
        Node::Offset { child, .. } => alloc::vec![arrange(child, rect, m)],

        Node::Stack(children) => {
            // Every layer gets the whole area. Overlays that want to be smaller wrap
            // themselves in a `Sized` or a `Padding`, which keeps this node one rule.
            children.iter().map(|ch| arrange(ch, rect, m)).collect()
        }

        Node::Column { spacing, children } => {
            arrange_linear(children, *spacing, rect, m, Axis::Vertical)
        }
        Node::Row { spacing, children } => {
            arrange_linear(children, *spacing, rect, m, Axis::Horizontal)
        }

        Node::Dock { edges, fill } => {
            let mut remaining = rect;
            let mut out = Vec::with_capacity(edges.len() + 1);
            for d in edges {
                let want = measure(&d.element, Constraints::loose(remaining.size), m);
                let (slot, rest) = split(remaining, d.edge, want);
                out.push(arrange(&d.element, slot, m));
                remaining = rest;
            }
            out.push(arrange(fill, remaining, m));
            out
        }
    };
    Layout { rect, children }
}

/// Measure and arrange `e` into `bounds` in one call.
pub fn layout<M: Metrics + ?Sized, Msg>(e: &Element<Msg>, bounds: Rect, m: &M) -> Layout {
    let size = measure(e, Constraints::tight(bounds.size), m);
    arrange(e, Rect::new(bounds.origin.x, bounds.origin.y, size.w, size.h), m)
}

/// Which way a linear container stacks.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Axis {
    Horizontal,
    Vertical,
}

/// Lay children along `axis`: fixed children take what they measure, flexed children split
/// what is left in proportion to their weight.
fn arrange_linear<M: Metrics + ?Sized, Msg>(
    children: &[Element<Msg>],
    spacing: u32,
    rect: Rect,
    m: &M,
    axis: Axis,
) -> Vec<Layout> {
    let extent = |s: Size| if axis == Axis::Horizontal { s.w } else { s.h };
    let gaps = spacing.saturating_mul(children.len().saturating_sub(1) as u32);
    let total = extent(rect.size);

    // Fixed children first: flex divides what they leave, not what they might have wanted.
    let mut sizes: Vec<u32> = Vec::with_capacity(children.len());
    let mut used = gaps;
    for ch in children {
        if ch.flex == 0 {
            let avail = total.saturating_sub(used);
            let c = match axis {
                Axis::Horizontal => Constraints::loose(Size::new(avail, rect.size.h)),
                Axis::Vertical => Constraints::loose(Size::new(rect.size.w, avail)),
            };
            let s = extent(measure(ch, c, m));
            used = used.saturating_add(s);
            sizes.push(s);
        } else {
            sizes.push(0);
        }
    }

    let weight: u32 = children.iter().map(|c| c.flex as u32).sum();
    if weight > 0 {
        // Integer division leaves a remainder; give it to the last flexed child rather than
        // dropping it, or a two-way split of an odd number leaves a one-pixel seam that
        // nothing paints.
        let free = total.saturating_sub(used);
        let mut handed_out = 0;
        let mut last_flex = None;
        for (i, ch) in children.iter().enumerate() {
            if ch.flex == 0 {
                continue;
            }
            let share = (free as u64 * ch.flex as u64 / weight as u64) as u32;
            sizes[i] = share;
            handed_out += share;
            last_flex = Some(i);
        }
        if let Some(i) = last_flex {
            sizes[i] = sizes[i].saturating_add(free.saturating_sub(handed_out));
        }
    }

    let mut out = Vec::with_capacity(children.len());
    let mut cursor = match axis {
        Axis::Horizontal => rect.origin.x,
        Axis::Vertical => rect.origin.y,
    };
    for (ch, &s) in children.iter().zip(sizes.iter()) {
        let slot = match axis {
            Axis::Horizontal => Rect::new(cursor, rect.origin.y, s, rect.size.h),
            Axis::Vertical => Rect::new(rect.origin.x, cursor, rect.size.w, s),
        };
        out.push(arrange(ch, slot, m));
        cursor = cursor.saturating_add(s as i32).saturating_add(spacing as i32);
    }
    out
}

/// Cut `want` off `rect` at `edge`, returning `(the slice, what is left)`.
fn split(rect: Rect, edge: Edge, want: Size) -> (Rect, Rect) {
    let (w, h) = (rect.size.w, rect.size.h);
    match edge {
        Edge::Top => {
            let cut = want.h.min(h);
            (
                Rect::new(rect.origin.x, rect.origin.y, w, cut),
                Rect::new(rect.origin.x, rect.origin.y + cut as i32, w, h - cut),
            )
        }
        Edge::Bottom => {
            let cut = want.h.min(h);
            (
                Rect::new(rect.origin.x, rect.origin.y + (h - cut) as i32, w, cut),
                Rect::new(rect.origin.x, rect.origin.y, w, h - cut),
            )
        }
        Edge::Left => {
            let cut = want.w.min(w);
            (
                Rect::new(rect.origin.x, rect.origin.y, cut, h),
                Rect::new(rect.origin.x + cut as i32, rect.origin.y, w - cut, h),
            )
        }
        Edge::Right => {
            let cut = want.w.min(w);
            (
                Rect::new(rect.origin.x + (w - cut) as i32, rect.origin.y, cut, h),
                Rect::new(rect.origin.x, rect.origin.y, w - cut, h),
            )
        }
    }
}

/// Where the element with `key` was laid out, if it is in this tree.
///
/// **The other half of [`offset`](crate::element::offset).** A popup is placed *under the item
/// that opened it*, so the application needs that item's rectangle — and the only thing that
/// knows it is the layout. The alternatives are both worse: re-deriving the item's width from
/// the font is the layout engine §5 refused, written in application code and wrong the moment a
/// label changes; and walking the tree by index is a path that silently means something else
/// the first time a container gains a child.
///
/// A key rather than a [`Widget::id`](crate::diff::Widget::id): ids are assigned by the diff
/// and an application does not choose them, whereas a key is what an application already uses
/// to say "this element, across frames".
///
/// Returns the **first** match in tree order. Duplicate keys within one parent are a
/// [`DiffError`](crate::diff::DiffError); duplicates across different parents are legal and
/// this does not disambiguate them.
pub fn locate<Msg>(e: &Element<Msg>, l: &Layout, key: u64) -> Option<Rect> {
    if e.key == Some(key) {
        return Some(l.rect);
    }
    e.children().zip(l.children.iter()).find_map(|(c, cl)| locate(c, cl, key))
}

/// The point `p` in `rect`'s coordinates.
pub fn to_local(rect: Rect, p: Point) -> Point {
    Point::new(p.x.saturating_sub(rect.origin.x), p.y.saturating_sub(rect.origin.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Part A's tests carry no messages, and `()` is the simplest inhabited `Msg`. Part B's
    /// routing tests use a real enum; these are about shape, not about what a click means.
    type Msg = ();

    /// `layout` with `Msg` and the metrics pinned, so a construction like
    /// `column(vec![text("a")])` infers from the parameter instead of needing a turbofish
    /// at every call.
    fn lay(e: &Element<Msg>, bounds: Rect) -> Layout {
        layout(e, bounds, &CELL)
    }

    /// `measure`, likewise.
    fn meas(e: &Element<Msg>, c: Constraints) -> Size {
        measure(e, c, &CELL)
    }
    use crate::element::{Insets, column, custom, dock, docked, fill, offset, padding, row,
                         sized, stack, text, with_spacing};
    use alloc::vec;
    use libdraw::format::Rgb;

    /// 8×16 cells, which is what a bitmap console font gives.
    const CELL: FixedCell = FixedCell { w: 8, h: 16 };
    const SCREEN: Rect = Rect::new(0, 0, 640, 480);

    #[test]
    fn text_measures_by_characters_not_bytes() {
        // A byte count is a character count only for ASCII. Getting this wrong mis-sizes
        // every non-ASCII label, which reads as a layout bug rather than a metrics one.
        assert_eq!(CELL.text_size("abc"), Size::new(24, 16));
        assert_eq!("é".len(), 2, "two bytes");
        assert_eq!(CELL.text_size("é"), Size::new(8, 16), "one cell");
    }

    #[test]
    fn a_column_stacks_and_a_row_runs() {
        let l = lay(&column(vec![text("ab"), text("cde")]), SCREEN);
        assert_eq!(l.children[0].rect, Rect::new(0, 0, 640, 16));
        assert_eq!(l.children[1].rect, Rect::new(0, 16, 640, 16));

        let l = lay(&row(vec![text("ab"), text("cde")]), SCREEN);
        assert_eq!(l.children[0].rect, Rect::new(0, 0, 16, 480));
        assert_eq!(l.children[1].rect, Rect::new(16, 0, 24, 480));
    }

    #[test]
    fn spacing_goes_between_children_and_not_at_the_ends() {
        // `n` children have `n-1` gaps. Two assertions, because they cover different code:
        // the cursor walk places each child, and the gap *total* is subtracted from what
        // flex divides. A first version tested only the placements and passed against `n`
        // gaps, because the cursor never reads the total.
        let c = with_spacing(column(vec![text("a"), text("b"), text("c")]), 4);
        let l = lay(&c, SCREEN);
        assert_eq!(l.children[0].rect.origin.y, 0);
        assert_eq!(l.children[1].rect.origin.y, 20);
        assert_eq!(l.children[2].rect.origin.y, 40);

        // 100 tall, two 16-tall fixed children, two gaps of 4 → the flexed child gets
        // 100 - 32 - 8 = 60. Counting three gaps would leave it 56 and the container short.
        let c = with_spacing(
            column(vec![text("a"), text("b"), custom(1, Size::new(0, 0)).flex(1)]),
            4,
        );
        let l = lay(&c, Rect::new(0, 0, 50, 100));
        assert_eq!(l.children[2].rect, Rect::new(0, 40, 50, 60));
    }

    #[test]
    fn flex_divides_only_what_the_fixed_children_leave() {
        // The terminal's shape: a fixed menu bar, a grid taking the rest.
        let c = column(vec![text("menu"), custom(1, Size::new(0, 0)).flex(1)]);
        let l = lay(&c, Rect::new(0, 0, 640, 480));
        assert_eq!(l.children[0].rect, Rect::new(0, 0, 640, 16), "fixed keeps its measure");
        assert_eq!(l.children[1].rect, Rect::new(0, 16, 640, 464), "flex takes the remainder");
    }

    #[test]
    fn flex_weights_split_in_proportion_and_lose_no_pixel() {
        // Integer division leaves a remainder. Dropping it leaves a one-pixel seam that
        // nothing paints — visible as a stripe of stale content, and maddening to trace.
        let c = row(vec![
            custom(1, Size::new(0, 0)).flex(1),
            custom(2, Size::new(0, 0)).flex(1),
            custom(3, Size::new(0, 0)).flex(1),
        ]);
        let l = lay(&c, Rect::new(0, 0, 100, 10));
        let widths: Vec<u32> = l.children.iter().map(|c| c.rect.size.w).collect();
        assert_eq!(widths.iter().sum::<u32>(), 100, "every pixel is assigned");
        assert_eq!(widths, [33, 33, 34], "and the remainder goes to the last");
    }

    #[test]
    fn a_dock_gives_each_edge_a_slice_and_the_filler_the_rest() {
        // The terminal: a menu bar on top, a scrollbar on the right, the grid filling.
        let d = dock(
            vec![
                docked(Edge::Top, sized(Size::new(0, 16), text(""))),
                docked(Edge::Right, sized(Size::new(12, 0), text(""))),
            ],
            custom(1, Size::new(0, 0)),
        );
        let l = lay(&d, Rect::new(0, 0, 640, 480));
        assert_eq!(l.children[0].rect, Rect::new(0, 0, 640, 16), "bar spans the full width");
        assert_eq!(
            l.children[1].rect,
            Rect::new(628, 16, 12, 464),
            "the scrollbar takes what the bar left, not the whole height"
        );
        assert_eq!(l.children[2].rect, Rect::new(0, 16, 628, 464), "the filler gets the rest");
    }

    #[test]
    fn dock_edges_apply_in_order() {
        // Right-then-top and top-then-right divide the same area differently, and both are
        // correct — which is why the order is the list's, not a fixed precedence.
        let top_first = dock(
            vec![docked(Edge::Top, sized(Size::new(0, 16), text(""))),
                 docked(Edge::Right, sized(Size::new(12, 0), text("")))],
            text(""),
        );
        let right_first = dock(
            vec![docked(Edge::Right, sized(Size::new(12, 0), text(""))),
                 docked(Edge::Top, sized(Size::new(0, 16), text("")))],
            text(""),
        );
        let a = lay(&top_first, Rect::new(0, 0, 100, 100));
        let b = lay(&right_first, Rect::new(0, 0, 100, 100));
        assert_eq!(a.children[0].rect, Rect::new(0, 0, 100, 16), "the bar spans everything");
        assert_eq!(b.children[1].rect, Rect::new(0, 0, 88, 16), "here the scrollbar took first");
    }

    #[test]
    fn a_dock_takes_what_it_is_offered_rather_than_what_its_children_measure() {
        // Measuring a dock as the sum of its children would leave the filler nothing to
        // fill, which defeats the only reason the node exists.
        let d = dock(vec![docked(Edge::Top, text("x"))], text("y"));
        assert_eq!(
            meas(&d, Constraints::loose(Size::new(200, 100))),
            Size::new(200, 100)
        );
    }

    #[test]
    fn padding_insets_the_child_and_grows_the_parent() {
        let p = padding(Insets::all(4), text("ab"));
        assert_eq!(meas(&p, Constraints::loose(Size::new(640, 480))), Size::new(24, 24));
        let l = lay(&p, Rect::new(10, 20, 100, 50));
        assert_eq!(l.children[0].rect, Rect::new(14, 24, 92, 42));
    }

    #[test]
    fn shrinking_past_zero_saturates_rather_than_wrapping() {
        // Tested on `shrink` directly, not through `measure`: the outer `constrain` clamps
        // the final answer either way, so an underflow that hands the *child* a `max` near
        // `u32::MAX` is invisible from outside — a first version of this test asserted the
        // outer size and passed against `wrapping_sub`.
        let c = Constraints::loose(Size::new(20, 20)).shrink(100, 100);
        assert_eq!(c.max, Size::new(0, 0), "saturated, not wrapped");
        assert!(c.min.w <= c.max.w && c.min.h <= c.max.h, "min must never exceed max");

        // And a tight box shrunk past its own minimum stays coherent.
        let c = Constraints::tight(Size::new(10, 10)).shrink(30, 4);
        assert!(c.min.w <= c.max.w && c.min.h <= c.max.h, "{c:?}");
        assert_eq!(c.max, Size::new(0, 6));
    }

    #[test]
    fn padding_wider_than_its_space_still_fits_what_it_was_offered() {
        let p = padding(Insets::all(50), text("hello"));
        let s = meas(&p, Constraints::loose(Size::new(20, 20)));
        assert!(s.w <= 20 && s.h <= 20, "clamped to what was offered, got {s:?}");
    }

    #[test]
    fn sized_constrains_the_axes_it_names_and_yields_the_ones_it_does_not() {
        // A zero component means "whatever the parent gives". Without it every fixed-size
        // child would have to name a cross-axis extent it does not care about.
        let l = lay(&sized(Size::new(12, 0), text("")), Rect::new(0, 0, 100, 80));
        assert_eq!(l.rect, Rect::new(0, 0, 12, 80), "a scrollbar strip");
        assert_eq!(l.children[0].rect, l.rect, "and the child gets exactly that");
        let l = lay(&sized(Size::new(0, 16), text("")), Rect::new(0, 0, 100, 80));
        assert_eq!(l.rect, Rect::new(0, 0, 100, 16), "a menu bar");
    }

    #[test]
    fn sized_binds_inside_a_stack_where_the_slot_is_the_whole_area() {
        // The case that exposed the doc overselling: `Stack` hands every layer the whole
        // area, so passing the parent rect straight through made `sized` a no-op there —
        // and Part C reaches for exactly this to place a scrollbar over a grid.
        let s = stack(vec![custom(1, Size::new(0, 0)), sized(Size::new(12, 20), text(""))]);
        let l = lay(&s, Rect::new(0, 0, 200, 100));
        assert_eq!(l.children[0].rect, Rect::new(0, 0, 200, 100), "the base layer fills");
        assert_eq!(l.children[1].rect, Rect::new(0, 0, 12, 20), "the overlay does not");
    }

    #[test]
    fn sized_never_exceeds_the_slot_it_was_given() {
        let l = lay(&sized(Size::new(500, 500), text("")), Rect::new(0, 0, 40, 30));
        assert_eq!(l.rect, Rect::new(0, 0, 40, 30));
    }

    #[test]
    fn a_stack_overlays_every_layer_on_the_whole_area() {
        let s = stack(vec![custom(1, Size::new(10, 10)), custom(2, Size::new(20, 20))]);
        let l = lay(&s, Rect::new(5, 5, 40, 30));
        assert_eq!(l.children[0].rect, Rect::new(5, 5, 40, 30));
        assert_eq!(l.children[1].rect, Rect::new(5, 5, 40, 30));
        // And it measures as the union, so a menu popup does not shrink its host.
        assert_eq!(
            meas(&s, Constraints::loose(Size::new(640, 480))),
            Size::new(20, 20)
        );
    }

    #[test]
    fn layouts_children_are_in_the_same_order_as_elements_children() {
        // The diff and the painter both walk the two together; a different order silently
        // pairs each node with the wrong rectangle.
        let e = dock(
            vec![docked(Edge::Top, text("bar")), docked(Edge::Left, text("side"))],
            column(vec![text("a"), text("b")]),
        );
        let l = lay(&e, SCREEN);
        assert_eq!(e.children().count(), l.children.len());
        for (child, cl) in e.children().zip(l.children.iter()) {
            assert_eq!(child.children().count(), cl.children.len());
        }
    }

    #[test]
    fn an_offset_child_is_placed_at_its_shift_and_keeps_its_measured_size() {
        let e: Element<Msg> = stack(vec![
            fill(Rgb::BLACK),
            offset(30, 12, sized(Size::new(40, 8), fill(Rgb::BLACK))),
        ]);
        let l = lay(&e, Rect::new(0, 0, 200, 100));
        let popup = &l.children[1];
        assert_eq!(popup.rect, Rect::new(30, 12, 40, 8));
        // And it takes its *measured* size rather than the space it was offered — the
        // distinction from `Padding`, which shrinks a child into the remainder.
        assert_ne!(popup.rect.size, Size::new(170, 88));
    }

    #[test]
    fn an_offset_node_owns_the_rectangle_its_child_sits_in() {
        // The containment invariant `paint` relies on to skip a subtree whole: a node whose
        // rect stayed its parent's would have a child outside it, and the skip could drop a
        // popup that overlapped the damage.
        let e: Element<Msg> = offset(20, 10, sized(Size::new(30, 6), fill(Rgb::BLACK)));
        let l = lay(&e, Rect::new(0, 0, 100, 50));
        assert_eq!(l.rect, Rect::new(20, 10, 30, 6));
        assert_eq!(l.children[0].rect, l.rect, "the child escaped its parent");
    }

    #[test]
    fn an_offset_is_relative_to_the_parent_not_the_screen() {
        // A popup in a window-level stack must move with the window. Nested inside a padding
        // so the parent's origin is not the screen's.
        let e: Element<Msg> =
            padding(Insets::all(5), offset(10, 4, sized(Size::new(8, 8), fill(Rgb::BLACK))));
        let l = lay(&e, Rect::new(100, 200, 80, 60));
        assert_eq!(l.children[0].rect.origin, Point::new(115, 209));
    }

    #[test]
    fn locate_finds_a_keyed_element_wherever_it_is() {
        let e: Element<Msg> = dock(
            vec![docked(
                Edge::Top,
                sized(Size::new(0, 20), row(vec![
                    sized(Size::new(30, 0), fill(Rgb::BLACK)).key(1),
                    sized(Size::new(40, 0), fill(Rgb::BLACK)).key(2),
                ])),
            )],
            fill(Rgb::BLACK),
        );
        let l = lay(&e, Rect::new(0, 0, 200, 100));
        assert_eq!(locate(&e, &l, 1), Some(Rect::new(0, 0, 30, 20)));
        assert_eq!(locate(&e, &l, 2), Some(Rect::new(30, 0, 40, 20)));
        assert_eq!(locate(&e, &l, 99), None, "an absent key found something");
    }

    #[test]
    fn locate_and_offset_place_a_popup_under_its_item() {
        // The two additions doing the job they exist for, end to end: find the item, put the
        // popup beneath it. This is the whole of what `nxterm`'s menu needs from the toolkit.
        let bar = || -> Element<Msg> {
            sized(
                Size::new(0, 20),
                row(vec![
                    sized(Size::new(30, 0), fill(Rgb::BLACK)).key(1),
                    sized(Size::new(40, 0), fill(Rgb::BLACK)).key(2),
                ]),
            )
        };
        let b = bar();
        let l0 = lay(&b, Rect::new(0, 0, 200, 100));
        let item = locate(&b, &l0, 2).expect("the item is in the tree");

        let ui: Element<Msg> = stack(vec![
            bar(),
            offset(item.origin.x, item.bottom() as i32, sized(Size::new(60, 50), fill(Rgb::BLACK))),
        ]);
        let l = lay(&ui, Rect::new(0, 0, 200, 100));
        let popup = l.children[1].rect;
        assert_eq!(popup.origin, Point::new(30, 20), "the popup is not under its item");
        assert_eq!(popup.size, Size::new(60, 50));
    }

    #[test]
    fn an_empty_container_lays_out_rather_than_panicking() {
        // `children.len() - 1` for the gap count underflows on an empty list, and an empty
        // container is what a `view` returns while a list is still loading.
        let l = lay(&with_spacing(column(vec![]), 4), SCREEN);
        assert!(l.children.is_empty());
        assert_eq!(meas(&column(vec![]), Constraints::loose(SCREEN.size)).h, 0);
    }

    #[test]
    fn a_child_larger_than_its_slot_is_clipped_by_the_slot_not_by_overflow() {
        // Arithmetic on a rect narrower than its content must not wrap. The child keeps its
        // requested size here — clipping is the painter's job — but nothing may go negative.
        let l = lay(&row(vec![text("a very long line indeed")]), Rect::new(0, 0, 16, 16));
        assert!(l.children[0].rect.size.w <= 16, "constrained, not wrapped");
    }
}
