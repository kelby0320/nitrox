//! The retained tree, and pairing it against a fresh `Element` tree.
//!
//! This is where the declarative model pays for itself: the diff pairs each node of the new
//! description with the widget it already has, and **what differs is the damage**.
//!
//! Two nodes pair if they are at the same position under the same parent and have the same *kind*;
//! a kind change destroys and rebuilds.
//!
//! Positional identity is wrong for a reordered, inserted-into or removed-from list: deleting the
//! first of five rows makes row 2's widget state belong to row 3. Hence
//! [`Element::key`](crate::element::Element::key): within a parent, keyed children pair by key.

use alloc::vec::Vec;

use libdraw::geom::Rect;

use crate::damage::union_opt;
use crate::element::{Element, Insets, Node};
use crate::layout::Layout;

/// What a node is, and everything about it that can change its own pixels.
///
/// Children are deliberately **not** part of this: folding them in would make every ancestor of a
/// changed leaf compare unequal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fingerprint {
    /// [`Node::Text`], with its contents.
    Text(alloc::string::String),
    /// [`Node::Column`], with its spacing.
    Column(u32),
    /// [`Node::Row`], with its spacing.
    Row(u32),
    /// [`Node::Dock`].
    Dock,
    /// [`Node::Stack`].
    Stack,
    /// [`Node::Padding`], with its insets.
    Padding(Insets),
    /// [`Node::Sized`], with its size.
    Sized(libdraw::geom::Size),
    /// [`Node::Bevel`], with its colour — like a fill, a recolour repaints.
    Bevel(libdraw::format::Rgb),
    /// [`Node::Icon`], with which glyph it is.
    Icon(crate::element::IconKind),
    /// [`Node::Fill`], with its colour — a recolour repaints, it does not rebuild.
    Fill(libdraw::format::Rgb),
    /// [`Node::Outline`], with its colour and radius — either one changing repaints.
    Outline(libdraw::format::Rgb, u32),
    /// [`Node::RoundedFill`], with its colour and radius — either one changing repaints.
    RoundedFill(libdraw::format::Rgb, u32),
    /// [`Node::Wash`], with its colour and coverage — either one changing repaints.
    Wash(libdraw::format::Rgb, u8),
    /// [`Node::Offset`], **without its shift**: an offset is a rect change, which `reconcile`
    /// already damages on.
    Offset,
    /// [`Node::Ink`], with its colour — a recolour repaints, like a fill's.
    Ink(libdraw::format::Rgb),
    /// [`Node::Scale`], with its step.
    Scale(crate::element::TextSize),
    /// [`Node::Bold`] — a different face repaints, like a different size.
    Bold,
    /// [`Node::Mono`] — as [`Bold`](Self::Bold), a different face.
    Mono,
    /// [`Node::Center`]; like `Offset` it carries nothing, as what it changes is a rect.
    Center,
    /// [`Node::Custom`], with its discriminator and size.
    Custom(u32, libdraw::geom::Size),
}

impl Fingerprint {
    /// The fingerprint of `e`'s own node, ignoring its children.
    pub fn of<Msg>(e: &Element<Msg>) -> Self {
        match &e.node {
            Node::Text(s) => Fingerprint::Text(s.clone()),
            Node::Column { spacing, .. } => Fingerprint::Column(*spacing),
            Node::Row { spacing, .. } => Fingerprint::Row(*spacing),
            Node::Dock { .. } => Fingerprint::Dock,
            Node::Stack(_) => Fingerprint::Stack,
            Node::Padding { insets, .. } => Fingerprint::Padding(*insets),
            Node::Sized { size, .. } => Fingerprint::Sized(*size),
            Node::Fill(c) => Fingerprint::Fill(*c),
            Node::Bevel(c) => Fingerprint::Bevel(*c),
            Node::Outline { colour, radius } => Fingerprint::Outline(*colour, *radius),
            Node::RoundedFill { colour, radius } => Fingerprint::RoundedFill(*colour, *radius),
            Node::Wash { colour, coverage } => Fingerprint::Wash(*colour, *coverage),
            Node::Icon(k) => Fingerprint::Icon(*k),
            Node::Offset { .. } => Fingerprint::Offset,
            // **With its colour**, unlike `Offset`: an ink change moves nothing, so a fingerprint
            // without it would leave the old colour on screen.
            Node::Ink { colour, .. } => Fingerprint::Ink(*colour),
            Node::Scale { size, .. } => Fingerprint::Scale(*size),
            Node::Bold { .. } => Fingerprint::Bold,
            Node::Mono { .. } => Fingerprint::Mono,
            Node::Center { .. } => Fingerprint::Center,
            Node::Custom { kind, size } => Fingerprint::Custom(*kind, *size),
        }
    }

    /// Whether two nodes are the same *kind*, ignoring their contents.
    ///
    /// **`Custom` compares its discriminator too**: the payload *is* the identity, or swapping
    /// custom widgets would keep the `Widget::id` and focus.
    pub fn same_kind(&self, other: &Self) -> bool {
        match (self, other) {
            (Fingerprint::Custom(a, _), Fingerprint::Custom(b, _)) => a == b,
            _ => core::mem::discriminant(self) == core::mem::discriminant(other),
        }
    }
}

/// A retained node: what survives between frames.
#[derive(Clone, Debug)]
pub struct Widget {
    /// Stable identity, assigned when this widget is created and carried through every frame it
    /// survives; **without it a keyed reorder and a positional one are indistinguishable**.
    pub id: u64,
    /// Identity within its parent, if the application gave one.
    pub key: Option<u64>,
    /// What it was last time.
    pub print: Fingerprint,
    /// Where it was last laid out.
    pub rect: Rect,
    /// Its children, in [`Element::children`] order.
    pub children: Vec<Widget>,
}

/// A mispairing the runtime refuses to guess at.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DiffError {
    /// Some of a parent's children are keyed and some are not.
    MixedKeying {
        /// Index path from the root to the offending parent.
        path: Vec<usize>,
    },
    /// A parent's children were keyed last frame and are not this frame, or the reverse.
    KeyingChanged {
        /// Index path from the root to the offending parent.
        path: Vec<usize>,
    },
    /// Two of a parent's children claim the same key.
    DuplicateKey {
        /// Index path from the root to the offending parent.
        path: Vec<usize>,
        /// The key claimed twice.
        key: u64,
    },
}

/// How a parent's children are identified.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Keying {
    /// No children at all — compatible with either mode, so an empty list is never a change.
    Empty,
    /// Paired by position.
    Positional,
    /// Paired by key.
    Keyed,
}

impl Keying {
    fn of_elements<Msg>(children: &[&Element<Msg>]) -> Option<Self> {
        Self::classify(children.iter().map(|c| c.key.is_some()))
    }

    fn of_widgets(children: &[Widget]) -> Option<Self> {
        Self::classify(children.iter().map(|c| c.key.is_some()))
    }

    /// `None` if the children disagree with each other.
    fn classify(mut keyed: impl Iterator<Item = bool>) -> Option<Self> {
        let Some(first) = keyed.next() else {
            return Some(Keying::Empty);
        };
        if keyed.any(|k| k != first) {
            return None;
        }
        Some(if first { Keying::Keyed } else { Keying::Positional })
    }

    /// Whether a parent may go from `self` to `other` between frames; `Empty` is compatible with
    /// everything, as a list that emptied and refilled has no widgets to mispair.
    fn compatible_with(self, other: Self) -> bool {
        self == other || self == Keying::Empty || other == Keying::Empty
    }
}

/// The retained tree, and the damage its last update produced.
pub struct Tree {
    root: Option<Widget>,
    next_id: u64,
}

impl Default for Tree {
    /// Delegates to [`Tree::new`]: **not `#[derive(Default)]`**, which starts `next_id` at 0 where
    /// `new()` starts at 1.
    fn default() -> Self {
        Self::new()
    }
}

impl Tree {
    /// An empty tree — the state before a window has ever painted.
    pub fn new() -> Self {
        Self { root: None, next_id: 1 }
    }

    /// The retained root, if anything has been laid out.
    pub fn root(&self) -> Option<&Widget> {
        self.root.as_ref()
    }

    /// Pair `element` (laid out as `layout`) against the retained tree.
    ///
    /// Returns the region that must be repainted, or `None`. On a [`DiffError`] the tree is left
    /// **unchanged** and no damage is reported, or the next frame diffs against a half-paired
    /// state.
    pub fn update<Msg>(&mut self, element: &Element<Msg>, layout: &Layout) -> Result<Option<Rect>, DiffError> {
        let mut damage = Damage::new();
        let mut path = Vec::new();
        let mut ids = self.next_id;
        let next = reconcile(self.root.as_ref(), element, layout, &mut damage, &mut path, &mut ids)?;
        self.root = Some(next);
        self.next_id = ids;
        Ok(damage.take())
    }

    /// Forget everything, so the next update damages the whole window.
    pub fn clear(&mut self) {
        self.root = None;
    }

    /// The id the next created widget will take. Test and diagnostic use.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// The widget id of the element carrying `key`, if the tree has one.
    ///
    /// [`locate`](crate::layout::locate)'s companion: this answers "which widget is it".
    pub fn find_by_key(&self, key: u64) -> Option<u64> {
        fn walk(w: &Widget, key: u64) -> Option<u64> {
            if w.key == Some(key) {
                return Some(w.id);
            }
            w.children.iter().find_map(|c| walk(c, key))
        }
        walk(self.root.as_ref()?, key)
    }
}

/// A union of rectangles, accumulated as one, because `Surface::Commit` carries exactly one damage
/// rectangle; the union is coarse.
struct Damage(Option<Rect>);

impl Damage {
    fn new() -> Self {
        Self(None)
    }

    fn add(&mut self, r: Rect) {
        self.0 = union_opt(self.0, Some(r));
    }

    fn take(self) -> Option<Rect> {
        self.0
    }
}

/// Pair one node, recursing into its children.
fn reconcile<Msg>(
    old: Option<&Widget>,
    e: &Element<Msg>,
    l: &Layout,
    damage: &mut Damage,
    path: &mut Vec<usize>,
    ids: &mut u64,
) -> Result<Widget, DiffError> {
    let print = Fingerprint::of(e);

    match old {
        Some(w) if w.print.same_kind(&print) => {
            if w.print != print {
                damage.add(l.rect);
            }
            if w.rect != l.rect {
                damage.add(w.rect);
                damage.add(l.rect);
            }
            let children = reconcile_children(&w.children, e, l, damage, path, ids)?;
            Ok(Widget { id: w.id, key: e.key, print, rect: l.rect, children })
        }
        other => {
            if let Some(w) = other {
                damage.add(w.rect);
            }
            damage.add(l.rect);
            let children = reconcile_children(&[], e, l, damage, path, ids)?;
            let id = *ids;
            *ids += 1;
            Ok(Widget { id, key: e.key, print, rect: l.rect, children })
        }
    }
}

/// Pair a parent's children, by key or by position.
fn reconcile_children<Msg>(
    old: &[Widget],
    e: &Element<Msg>,
    l: &Layout,
    damage: &mut Damage,
    path: &mut Vec<usize>,
    ids: &mut u64,
) -> Result<Vec<Widget>, DiffError> {
    let elems: Vec<&Element<Msg>> = e.children().collect();
    debug_assert_eq!(elems.len(), l.children.len(), "element and layout trees disagree");

    let Some(new_keying) = Keying::of_elements(&elems) else {
        return Err(DiffError::MixedKeying { path: path.clone() });
    };
    let Some(old_keying) = Keying::of_widgets(old) else {
        // Unreachable (a mixed retained tree needs a mixed previous frame, rejected above), but
        // reported rather than asserted: a toolkit panic kills the application.
        return Err(DiffError::MixedKeying { path: path.clone() });
    };
    if !old_keying.compatible_with(new_keying) {
        return Err(DiffError::KeyingChanged { path: path.clone() });
    }

    if new_keying == Keying::Keyed {
        for (i, a) in elems.iter().enumerate() {
            for b in &elems[i + 1..] {
                if a.key == b.key {
                    return Err(DiffError::DuplicateKey { path: path.clone(), key: a.key.expect("keyed") });
                }
            }
        }
    }

    let mut out = Vec::with_capacity(elems.len());
    let mut matched = alloc::vec![false; old.len()];

    for (i, (ce, cl)) in elems.iter().zip(l.children.iter()).enumerate() {
        let paired = match new_keying {
            Keying::Keyed => old.iter().position(|w| w.key == ce.key),
            _ => (i < old.len()).then_some(i),
        };
        if let Some(j) = paired {
            matched[j] = true;
        }
        path.push(i);
        let w = reconcile(paired.map(|j| &old[j]), ce, cl, damage, path, ids);
        path.pop();
        out.push(w?);
    }

    for (w, &m) in old.iter().zip(matched.iter()) {
        if !m {
            // The subtree needs no walk: every container lays its children inside its own
            // rectangle. `every_child_is_contained_by_its_parent` is what holds this up.
            damage.add(w.rect);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    type Msg = ();
    use crate::element::{
        Edge, Insets, column, custom, dock, docked, fill, offset, padding, row, sized, stack, text, with_spacing,
    };
    use crate::layout::{FixedCell, Layout, layout};
    use alloc::vec;
    use libdraw::format::Rgb;
    use libdraw::geom::Size;

    const CELL: FixedCell = FixedCell { w: 8, h: 16 };
    const SCREEN: Rect = Rect::new(0, 0, 640, 480);

    fn go(t: &mut Tree, e: &Element<Msg>) -> Result<Option<Rect>, DiffError> {
        let l = layout(e, SCREEN, &CELL);
        t.update(e, &l)
    }

    #[test]
    fn a_keyed_element_can_be_found_by_its_key_and_an_absent_one_is_none() {
        let e: Element<()> = column(vec![text("a").key(7), row(vec![text("b").key(9)]).key(8)]);
        let l = layout(&e, SCREEN, &CELL);
        let mut t = Tree::new();
        t.update(&e, &l).expect("a clean frame");

        let deep = t.root().unwrap().children[1].children[0].id;
        assert_eq!(t.find_by_key(9), Some(deep), "a key nested below the root");
        assert_eq!(t.find_by_key(7), Some(t.root().unwrap().children[0].id));
        assert_eq!(t.find_by_key(1234), None, "a key nothing carries");
        assert_eq!(Tree::new().find_by_key(7), None, "and a tree with no root at all");
    }

    #[test]
    fn the_first_frame_damages_everything_it_draws() {
        let mut t = Tree::new();
        let d = go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        assert_eq!(d, Some(Rect::new(0, 0, 640, 480)));
    }

    #[test]
    fn an_identical_frame_damages_nothing() {
        let mut t = Tree::new();
        let e = column(vec![text("a"), text("b")]);
        go(&mut t, &e).expect("ok");
        assert_eq!(go(&mut t, &e).expect("ok"), None);
    }

    #[test]
    fn changed_text_damages_only_that_node() {
        let mut t = Tree::new();
        go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        let d = go(&mut t, &column(vec![text("a"), text("Z")])).expect("ok");
        assert_eq!(d, Some(Rect::new(0, 16, 640, 16)), "the second row only");
    }

    #[test]
    fn a_kind_change_rebuilds_the_widget_and_damages_both_rectangles() {
        // **Asserted through the id**, not only the damage, which is the same whether the widget
        // was rebuilt or repainted.
        let mut t = Tree::new();
        go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        let before = ids(&t);

        let d = go(&mut t, &column(vec![text("a"), custom(1, Size::new(4, 4))])).expect("ok");
        assert_eq!(d, Some(Rect::new(0, 16, 640, 16)));
        let after = ids(&t);
        assert_eq!(after[0], before[0], "the untouched sibling kept its widget");
        assert_ne!(after[1], before[1], "a text that became a custom is a new widget");
    }

    #[test]
    fn two_custom_widgets_with_different_kinds_are_different_widgets() {
        let mut t = Tree::new();
        go(&mut t, &column(vec![custom(1, Size::new(40, 40))])).expect("ok");
        let before = ids(&t);
        go(&mut t, &column(vec![custom(2, Size::new(40, 40))])).expect("ok");
        assert_ne!(ids(&t)[0], before[0], "a different kind is a different widget");
    }

    #[test]
    fn a_custom_widget_that_only_resized_is_the_same_widget() {
        let mut t = Tree::new();
        go(&mut t, &column(vec![custom(1, Size::new(40, 40))])).expect("ok");
        let before = ids(&t);
        go(&mut t, &column(vec![custom(1, Size::new(80, 40))])).expect("ok");
        assert_eq!(ids(&t)[0], before[0]);
    }

    #[test]
    fn a_removed_child_damages_where_it_was() {
        let mut t = Tree::new();
        go(&mut t, &column(vec![text("a"), text("b"), text("c")])).expect("ok");
        let d = go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        assert!(d.is_some());
        let d = d.unwrap();
        assert!(d.bottom() >= 48, "covers the vacated third row, got {d:?}");
    }

    fn ids(t: &Tree) -> Vec<u64> {
        t.root().expect("a root").children.iter().map(|w| w.id).collect()
    }

    #[test]
    fn a_keyed_reorder_moves_widgets_where_a_positional_one_would_not() {
        // **This test distinguishes the two pairing modes at all**: only the stable id records
        // which old widget each new element carried forward.
        let keyed = |s: &str, k: u64| text(s).key(k);
        let mut t = Tree::new();
        go(&mut t, &column(vec![keyed("a", 1), keyed("b", 2), keyed("c", 3)])).expect("ok");
        let before = ids(&t);
        assert_eq!(before.len(), 3);
        let minted = t.next_id();

        go(&mut t, &column(vec![keyed("c", 3), keyed("b", 2), keyed("a", 1)])).expect("ok");
        let after = ids(&t);
        assert_eq!(
            after,
            [before[2], before[1], before[0]],
            "each widget followed its key; positional pairing would leave them in place"
        );
        assert_eq!(t.next_id(), minted, "no new ids were minted");
    }

    #[test]
    fn deleting_a_keyed_row_takes_that_widget_and_no_other() {
        let keyed = |s: &str, k: u64| text(s).key(k);
        let mut t = Tree::new();
        go(&mut t, &column(vec![keyed("a", 1), keyed("b", 2), keyed("c", 3)])).expect("ok");
        let before = ids(&t);

        go(&mut t, &column(vec![keyed("b", 2), keyed("c", 3)])).expect("ok");
        assert_eq!(ids(&t), [before[1], before[2]], "b and c kept their own widgets");
    }

    #[test]
    fn a_node_that_moves_without_changing_damages_where_it_was_too() {
        let mut t = Tree::new();
        let before = column(vec![sized(Size::new(8, 32), text("")), text("b")]);
        go(&mut t, &before).expect("ok");

        // The first child **shrinks**, so "b" slides *up* and the area it vacated lies below
        // everything the sibling damaged; growing instead hides the bug.
        let after = column(vec![sized(Size::new(8, 16), text("")), text("b")]);
        let d = go(&mut t, &after).expect("ok").expect("something moved");
        assert!(d.origin.y == 0, "covers the shrunken sibling, got {d:?}");
        assert!(d.bottom() >= 48, "and the row \"b\" vacated, got {d:?}");
    }

    #[test]
    fn mixing_keyed_and_unkeyed_children_is_an_error() {
        let mut t = Tree::new();
        let e = column(vec![text("a").key(1), text("b")]);
        assert_eq!(go(&mut t, &e), Err(DiffError::MixedKeying { path: vec![] }));
    }

    #[test]
    fn keyedness_changing_between_frames_is_an_error() {
        let mut t = Tree::new();
        go(&mut t, &column(vec![text("a").key(1), text("b").key(2)])).expect("ok");
        let e = column(vec![text("a"), text("b")]);
        assert_eq!(go(&mut t, &e), Err(DiffError::KeyingChanged { path: vec![] }));

        let mut t = Tree::new();
        go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        let e = column(vec![text("a").key(1), text("b").key(2)]);
        assert_eq!(go(&mut t, &e), Err(DiffError::KeyingChanged { path: vec![] }));
    }

    #[test]
    fn an_emptied_list_may_come_back_with_either_keying() {
        let mut t = Tree::new();
        go(&mut t, &column(vec![])).expect("ok");
        go(&mut t, &column(vec![text("a").key(1)])).expect("keyed after empty");

        let mut t = Tree::new();
        go(&mut t, &column(vec![])).expect("ok");
        go(&mut t, &column(vec![text("a")])).expect("unkeyed after empty");
    }

    #[test]
    fn duplicate_keys_in_one_parent_are_an_error() {
        let mut t = Tree::new();
        let e = column(vec![text("a").key(7), text("b").key(7)]);
        assert_eq!(go(&mut t, &e), Err(DiffError::DuplicateKey { path: vec![], key: 7 }));
    }

    #[test]
    fn the_same_key_under_different_parents_is_fine() {
        let mut t = Tree::new();
        let e = row(vec![column(vec![text("a").key(1)]), column(vec![text("b").key(1)])]);
        assert!(go(&mut t, &e).is_ok());
    }

    #[test]
    fn an_error_names_the_offending_parent() {
        let mut t = Tree::new();
        let e = column(vec![text("top"), row(vec![text("x").key(1), text("y")])]);
        assert_eq!(go(&mut t, &e), Err(DiffError::MixedKeying { path: vec![1] }));
    }

    #[test]
    fn a_rejected_frame_leaves_the_tree_untouched() {
        let mut t = Tree::new();
        go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        let before = t.root().cloned().expect("a root");

        let bad = column(vec![text("a").key(1), text("b")]);
        assert!(go(&mut t, &bad).is_err());
        let after = t.root().expect("still a root");
        assert_eq!(before.children.len(), after.children.len());
        assert_eq!(after.children[0].print, Fingerprint::Text("a".into()));
        assert_eq!(after.children[0].key, None, "not the rejected frame's key");

        assert_eq!(go(&mut t, &column(vec![text("a"), text("b")])).expect("ok"), None);
    }

    #[test]
    fn every_child_is_contained_by_its_parent() {
        fn check(l: &Layout) {
            for c in &l.children {
                if c.rect.size.w == 0 || c.rect.size.h == 0 {
                    continue; // covers no pixel; `union_opt` filters it either way
                }
                assert!(
                    c.rect.origin.x >= l.rect.origin.x
                        && c.rect.origin.y >= l.rect.origin.y
                        && c.rect.right() <= l.rect.right()
                        && c.rect.bottom() <= l.rect.bottom(),
                    "child {:?} escapes parent {:?}",
                    c.rect,
                    l.rect
                );
                check(c);
            }
        }
        let e: Element<Msg> = dock(
            vec![docked(Edge::Top, sized(Size::new(0, 16), text("bar")))],
            stack(vec![
                padding(Insets::all(4), column(vec![text("a"), text("b").flex(1)])),
                sized(Size::new(12, 20), custom(1, Size::new(4, 4))),
            ]),
        );
        check(&layout(&e, SCREEN, &CELL));

        // **`Offset` deliberately, and far enough to overhang**: the one node kind that can escape
        // its parent by construction.
        let overhang: Element<Msg> = stack(vec![sized(
            Size::new(100, 100),
            stack(vec![fill(libdraw::format::Rgb::new(0, 0, 0)), offset(90, 90, sized(Size::new(50, 50), text("x")))]),
        )]);
        let l = layout(&overhang, SCREEN, &CELL);
        check(&l);
        // ...and it is clipped rather than merely reported as contained: a zero-size rect would
        // satisfy `check`.
        let placed = l.children[0].children[0].children[1].rect;
        assert_eq!(placed, Rect::new(90, 90, 10, 10), "the visible part of the overhang");
    }

    #[test]
    fn container_properties_are_part_of_a_nodes_fingerprint() {
        assert_ne!(
            Fingerprint::of(&padding::<Msg>(Insets::all(1), text("x"))),
            Fingerprint::of(&padding::<Msg>(Insets::all(2), text("x")))
        );
        assert_ne!(
            Fingerprint::of(&with_spacing(column::<Msg>(vec![text("x")]), 1)),
            Fingerprint::of(&with_spacing(column::<Msg>(vec![text("x")]), 2))
        );
        assert_eq!(
            Fingerprint::of(&column::<Msg>(vec![text("x")])),
            Fingerprint::of(&column::<Msg>(vec![text("y"), text("z")]))
        );
    }

    #[test]
    fn clear_makes_the_next_frame_repaint_everything() {
        let mut t = Tree::new();
        let e = column(vec![text("a")]);
        go(&mut t, &e).expect("ok");
        assert_eq!(go(&mut t, &e).expect("ok"), None);
        t.clear();
        assert_eq!(go(&mut t, &e).expect("ok"), Some(SCREEN));
    }

    #[test]
    fn recolouring_a_fill_damages_it() {
        // **The whole widget set's hover and press feedback rests on this.** A `button` differs
        // between rest and hover *only* in its face's colour, so a fingerprint ignoring the colour
        // never repaints it. Under a `Stack` on purpose: that is the shape `button` builds.
        let mut t = Tree::new();
        let red = Rgb::new(0xC0, 0x10, 0x10);
        let blue = Rgb::new(0x10, 0x10, 0xC0);
        let at_rest: Element<Msg> = stack(vec![fill(red), text("OK")]);
        go(&mut t, &at_rest).expect("ok");
        assert_eq!(go(&mut t, &at_rest).expect("ok"), None, "an unchanged frame is clean");

        let hovered: Element<Msg> = stack(vec![fill(blue), text("OK")]);
        assert!(
            go(&mut t, &hovered).expect("ok").is_some(),
            "a recoloured fill reported no damage — hover and press feedback never repaints"
        );
    }

    #[test]
    fn moving_an_offset_child_damages() {
        // A menu popup opened under a *different* item moves without anything inside it changing,
        // and must repaint. **The damage comes from the rect, not the fingerprint**: `reconcile`
        // damages on `w.rect != l.rect`, and an offset *is* a rect change.
        let mut t = Tree::new();
        let at = |dx: i32| -> Element<Msg> { stack(vec![fill(Rgb::new(1, 2, 3)), offset(dx, 20, text("File"))]) };
        go(&mut t, &at(0)).expect("ok");
        assert_eq!(go(&mut t, &at(0)).expect("ok"), None, "an unchanged frame is clean");
        assert!(go(&mut t, &at(40)).expect("ok").is_some(), "a popup that moved reported no damage");
    }

    #[test]
    fn a_zero_extent_change_does_not_drag_damage_to_the_origin() {
        let mut d = Damage::new();
        d.add(Rect::new(100, 100, 10, 10));
        d.add(Rect::new(0, 0, 0, 0));
        assert_eq!(d.take(), Some(Rect::new(100, 100, 10, 10)));
    }
}
