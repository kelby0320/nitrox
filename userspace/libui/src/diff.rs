//! The retained tree, and pairing it against a fresh `Element` tree.
//!
//! This is where the declarative model pays for itself. The application rebuilds a
//! description; the diff pairs each node of that description with the widget it already
//! has, and **what differs is the damage**. Nothing calls `invalidate()`, so nothing can
//! forget to.
//!
//! ## Identity
//!
//! Two nodes pair if they are at the same position under the same parent and have the same
//! *kind*. A kind change destroys and rebuilds, which is right: a button is not a scrollbar
//! with different properties.
//!
//! Positional identity is wrong for one common case — a list whose items are reordered,
//! inserted into, or removed from. Deleting the first of five rows makes every later row
//! pair with its neighbour, so row 2's widget state belongs to row 3 from then on. Hence
//! [`Element::key`](crate::element::Element::key): within a parent, keyed children pair by
//! key rather than by position.
//!
//! ## The failures are loud
//!
//! Silent mispairing presents as "the wrong row remembered my selection", which nobody
//! diagnoses from the symptom. Three things are therefore errors rather than fallbacks:
//! keying that is **mixed within a frame**, keying that **changes between frames**, and
//! **duplicate keys** in one parent. See [`DiffError`].

use alloc::vec::Vec;

use libdraw::geom::Rect;

use crate::damage::union_opt;
use crate::element::{Element, Insets, Node};
use crate::layout::Layout;

/// What a node is, and everything about it that can change its own pixels.
///
/// Children are deliberately **not** part of this: they are diffed separately, and folding
/// them in would make every ancestor of a changed leaf compare unequal, damaging the whole
/// window for a one-character edit.
///
/// Compared exactly rather than hashed. A 64-bit fingerprint collision is unlikely and
/// silent — it presents as a region that never repaints — and these values are small enough
/// that comparing them costs less than the argument about whether the hash is safe.
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
            Node::Custom { kind, size } => Fingerprint::Custom(*kind, *size),
        }
    }

    /// Whether two nodes are the same *kind*, ignoring their contents.
    ///
    /// Pairing uses this; damage uses full equality. A `Text` whose string changed is still
    /// the same widget — it repaints, it is not rebuilt — whereas a `Text` that became a
    /// `Row` is a different thing that happens to sit in the same slot.
    ///
    /// **`Custom` compares its discriminator too**, because for that node the payload *is*
    /// the identity: `Custom { kind }` is documented as "so a diff can tell two custom nodes
    /// apart", and a discriminant comparison cannot. Swapping a terminal grid for a
    /// different custom widget in the same slot would otherwise keep the same
    /// [`Widget::id`] — so it would keep focus, and answer "is this widget new?" with no
    /// (PR #183 review, finding 1).
    ///
    /// Its `size` is *not* part of the kind: a custom widget that resized is the same
    /// widget, the same way a `Text` that changed its string is.
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
    /// Stable identity, assigned once when this widget is created and carried through every
    /// frame it survives.
    ///
    /// **This is what "retained" means**, and without it the diff retains nothing: every
    /// field below is derived from the new frame, so which old widget a new element paired
    /// with would be unobservable — a keyed reorder and a positional one produce identical
    /// trees *and* identical damage when the nodes are the same kind. A first version had
    /// exactly that hole, and its reorder test passed against pairing by position.
    ///
    /// It is also the hook the later parts need: a focus path is a path to an id, and
    /// "is this widget new?" is a question animation asks.
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
///
/// Each carries the index path from the root to the *parent* whose children are at fault,
/// so a message can name the container rather than leaving the reader to find it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DiffError {
    /// Some of a parent's children are keyed and some are not.
    MixedKeying {
        /// Index path from the root to the offending parent.
        path: Vec<usize>,
    },
    /// A parent's children were keyed last frame and are not this frame, or the reverse.
    ///
    /// The case that internal-consistency checks miss: each frame is fine on its own, so
    /// neither the mixed-keying check nor a uniqueness assert fires, and the diff quietly
    /// falls back to positional pairing. `view` being a pure function of state makes this
    /// easy to reach — `if state.compact { column(rows) } else { column(keyed_rows) }`.
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

    /// Whether a parent may go from `self` to `other` between frames.
    ///
    /// `Empty` is compatible with everything: a list that emptied and refilled has no
    /// widgets to mispair, so calling that a keying change would reject the ordinary case
    /// of a list arriving.
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
    /// Delegates to [`Tree::new`].
    ///
    /// **Not `#[derive(Default)]`.** That starts `next_id` at 0 while `new()` starts at 1, so
    /// a defaulted tree would mint a widget with id 0 that `new()` never produces — awkward
    /// the moment Part B wants 0 as a "no focus" sentinel. The same trap `WindowStack`
    /// documents in the compositor (PR #183 review, finding 7).
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
    /// Returns the region that must be repainted, or `None` if nothing changed. On a
    /// [`DiffError`] the tree is left **unchanged** and no damage is reported: a rejected
    /// frame must not leave half a tree paired, or the next frame diffs against a state
    /// neither the application nor the toolkit believes in.
    pub fn update<Msg>(
        &mut self,
        element: &Element<Msg>,
        layout: &Layout,
    ) -> Result<Option<Rect>, DiffError> {
        let mut damage = Damage::new();
        let mut path = Vec::new();
        // `next_id` advances into a local and is committed only on success, alongside the
        // tree — a rejected frame must not burn ids either, or the counter tells a story
        // about widgets that were never created.
        let mut ids = self.next_id;
        let next =
            reconcile(self.root.as_ref(), element, layout, &mut damage, &mut path, &mut ids)?;
        self.root = Some(next);
        self.next_id = ids;
        Ok(damage.take())
    }

    /// Forget everything, so the next update damages the whole window.
    ///
    /// What a resize does: every rectangle is stale, and re-deriving that node by node is
    /// slower and no more correct than repainting.
    pub fn clear(&mut self) {
        self.root = None;
    }

    /// The id the next created widget will take. Test and diagnostic use.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }
}

/// A union of rectangles, accumulated as one.
///
/// One, because `Surface::Commit` carries exactly one damage rectangle — a decision the
/// wire format already made. The union is coarse: two small changes at opposite corners
/// damage everything between them. If that ever stops being acceptable the fix is a
/// multi-rectangle `Commit`, which belongs in the protocol rather than here.
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
        // Same kind: keep the widget, note what changed about it.
        Some(w) if w.print.same_kind(&print) => {
            if w.print != print {
                // Content changed in place — repaint where it is.
                damage.add(l.rect);
            }
            if w.rect != l.rect {
                // Moved or resized: both the vacated area and the new one.
                damage.add(w.rect);
                damage.add(l.rect);
            }
            let children = reconcile_children(&w.children, e, l, damage, path, ids)?;
            // The id survives: this is the same widget, wherever it moved to.
            Ok(Widget { id: w.id, key: e.key, print, rect: l.rect, children })
        }
        // Different kind, or nothing there: a new widget. Damage what it replaces and what
        // it becomes.
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
        // The retained tree can only be mixed if a previous frame was, which this same
        // check rejects — so this is unreachable rather than defensive. Reported rather
        // than asserted, because a panic in a toolkit takes the application with it.
        return Err(DiffError::MixedKeying { path: path.clone() });
    };
    if !old_keying.compatible_with(new_keying) {
        return Err(DiffError::KeyingChanged { path: path.clone() });
    }

    if new_keying == Keying::Keyed {
        // Checked on every build, not only in debug. A `cfg`-divergent check means release
        // behaves differently from what the tests exercise, and the scan is O(n²) over a
        // container's direct children — a list long enough to matter would have to be long
        // enough to lay out, which is the bigger cost.
        for (i, a) in elems.iter().enumerate() {
            for b in &elems[i + 1..] {
                if a.key == b.key {
                    return Err(DiffError::DuplicateKey {
                        path: path.clone(),
                        key: a.key.expect("keyed"),
                    });
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

    // Anything the new frame did not claim is gone: damage where it was.
    for (w, &m) in old.iter().zip(matched.iter()) {
        if !m {
            // The subtree needs no walk: every container arranges its children inside its
            // own rectangle, so a parent's rect covers all of them. An earlier version
            // walked it and justified that with "nothing clips a `Sized` larger than its
            // slot" — which was true then and is not now, and was inconsistent anyway with
            // the different-kind branch, which never walked (PR #183 review, finding 8).
            // `every_child_is_contained_by_its_parent` is what holds this up.
            damage.add(w.rect);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Part A's tests carry no messages, and `()` is the simplest inhabited `Msg`. Part B's
    /// routing tests use a real enum; these are about shape, not about what a click means.
    type Msg = ();
    use crate::element::{Edge, Insets, column, custom, dock, docked, padding, row,
                         sized, stack, text, with_spacing};
    use crate::layout::{FixedCell, Layout, layout};
    use alloc::vec;
    use libdraw::geom::Size;

    const CELL: FixedCell = FixedCell { w: 8, h: 16 };
    const SCREEN: Rect = Rect::new(0, 0, 640, 480);

    /// Lay `e` out and diff it into `t`.
    fn go(t: &mut Tree, e: &Element<Msg>) -> Result<Option<Rect>, DiffError> {
        let l = layout(e, SCREEN, &CELL);
        t.update(e, &l)
    }

    #[test]
    fn the_first_frame_damages_everything_it_draws() {
        let mut t = Tree::new();
        let d = go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        assert_eq!(d, Some(Rect::new(0, 0, 640, 480)));
    }

    #[test]
    fn an_identical_frame_damages_nothing() {
        // The property the whole design rests on: rebuilding the description is free when
        // the description did not change.
        let mut t = Tree::new();
        let e = column(vec![text("a"), text("b")]);
        go(&mut t, &e).expect("ok");
        assert_eq!(go(&mut t, &e).expect("ok"), None);
    }

    #[test]
    fn changed_text_damages_only_that_node() {
        // Children are excluded from a node's fingerprint precisely so this holds: a
        // one-character edit must not repaint the window.
        let mut t = Tree::new();
        go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        let d = go(&mut t, &column(vec![text("a"), text("Z")])).expect("ok");
        assert_eq!(d, Some(Rect::new(0, 16, 640, 16)), "the second row only");
    }

    #[test]
    fn a_kind_change_rebuilds_the_widget_and_damages_both_rectangles() {
        // **Asserted through the id**, not only the damage. The union of the old and new
        // rectangles is the same whether the widget was rebuilt or merely repainted, so a
        // damage assertion passes with `same_kind` hardcoded to `true` — which is how the
        // `Custom` hole below survived (PR #183 review, finding 1).
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
        // `kind` is documented as the discriminator "so a diff can tell two custom nodes
        // apart", and a `mem::discriminant` comparison cannot: both are `Custom`. Swapping
        // a terminal grid for another custom widget in the same slot would keep the same
        // id, and with it focus and "am I new?".
        let mut t = Tree::new();
        go(&mut t, &column(vec![custom(1, Size::new(40, 40))])).expect("ok");
        let before = ids(&t);
        go(&mut t, &column(vec![custom(2, Size::new(40, 40))])).expect("ok");
        assert_ne!(ids(&t)[0], before[0], "a different kind is a different widget");
    }

    #[test]
    fn a_custom_widget_that_only_resized_is_the_same_widget() {
        // The other half of the rule: `size` is content, not identity, exactly as a
        // `Text`'s string is. Rebuilding on resize would drop state on every window resize.
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
        // Two rows remain; the third's area must be repainted or its pixels persist.
        let d = go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        assert!(d.is_some());
        let d = d.unwrap();
        assert!(d.bottom() >= 48, "covers the vacated third row, got {d:?}");
    }

    /// The stable ids of a parent's children, top to bottom.
    fn ids(t: &Tree) -> Vec<u64> {
        t.root().expect("a root").children.iter().map(|w| w.id).collect()
    }

    #[test]
    fn a_keyed_reorder_moves_widgets_where_a_positional_one_would_not() {
        // **This is the test that distinguishes the two pairing modes at all.** With
        // same-kind children, keyed and positional pairing produce identical trees *and*
        // identical damage — the only observable difference is which old widget each new
        // element carried forward, which is exactly what the stable id records. Asserting
        // `print`s or `key`s instead passes against pairing by position, as a first version
        // of this test did.
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
        // Nothing was rebuilt — a reorder must not destroy widget state. Compared against
        // the counter itself rather than against a child's id: the root takes an id too,
        // after its children, so arithmetic on `before` is off by the ancestors.
        assert_eq!(t.next_id(), minted, "no new ids were minted");
    }

    #[test]
    fn deleting_a_keyed_row_takes_that_widget_and_no_other() {
        // Positionally, dropping row 1 makes every later row pair with its neighbour, so
        // row 2's widget state belongs to row 3 from then on. Visible only through the ids.
        let keyed = |s: &str, k: u64| text(s).key(k);
        let mut t = Tree::new();
        go(&mut t, &column(vec![keyed("a", 1), keyed("b", 2), keyed("c", 3)])).expect("ok");
        let before = ids(&t);

        go(&mut t, &column(vec![keyed("b", 2), keyed("c", 3)])).expect("ok");
        assert_eq!(ids(&t), [before[1], before[2]], "b and c kept their own widgets");
    }

    #[test]
    fn a_node_that_moves_without_changing_damages_where_it_was_too() {
        // Leaving the old rectangle undamaged strands a copy of the node at its previous
        // position. Every other test changes content when something moves, so the damage is
        // added for the other reason and the omission hides — which is how a first version
        // of this passed against damaging only the new rectangle.
        let mut t = Tree::new();
        let before = column(vec![sized(Size::new(8, 32), text("")), text("b")]);
        go(&mut t, &before).expect("ok");

        // The first child **shrinks**, so "b" — byte-identical — slides *up* from y=32 to
        // y=16, and the area it vacated (32..48) lies below everything the shrinking sibling
        // damaged (0..32). Growing instead hides the bug entirely: the sibling's own damage
        // then covers the vacated area, which is why a first version of this test passed
        // against damaging only the new rectangle.
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
        // The case each frame's own consistency check misses: both frames are internally
        // fine, and the diff would silently fall back to positional pairing. `view` being a
        // pure function of state makes this easy to reach.
        let mut t = Tree::new();
        go(&mut t, &column(vec![text("a").key(1), text("b").key(2)])).expect("ok");
        let e = column(vec![text("a"), text("b")]);
        assert_eq!(go(&mut t, &e), Err(DiffError::KeyingChanged { path: vec![] }));

        // ...and the reverse direction too.
        let mut t = Tree::new();
        go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        let e = column(vec![text("a").key(1), text("b").key(2)]);
        assert_eq!(go(&mut t, &e), Err(DiffError::KeyingChanged { path: vec![] }));
    }

    #[test]
    fn an_emptied_list_may_come_back_with_either_keying() {
        // `Empty` is compatible with both, because there are no widgets to mispair. Calling
        // it a change would reject the ordinary case of a list that starts empty.
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
        // Keys are scoped to a parent. Requiring global uniqueness would make every list
        // need to know about every other list.
        let mut t = Tree::new();
        let e = row(vec![
            column(vec![text("a").key(1)]),
            column(vec![text("b").key(1)]),
        ]);
        assert!(go(&mut t, &e).is_ok());
    }

    #[test]
    fn an_error_names_the_offending_parent() {
        let mut t = Tree::new();
        let e = column(vec![
            text("top"),
            row(vec![text("x").key(1), text("y")]),
        ]);
        // Path is to the parent: child 1 of the root.
        assert_eq!(go(&mut t, &e), Err(DiffError::MixedKeying { path: vec![1] }));
    }

    #[test]
    fn a_rejected_frame_leaves_the_tree_untouched() {
        // Half a paired tree is worse than none: the next frame would diff against a state
        // neither side believes in.
        let mut t = Tree::new();
        go(&mut t, &column(vec![text("a"), text("b")])).expect("ok");
        let before = t.root().cloned().expect("a root");

        let bad = column(vec![text("a").key(1), text("b")]);
        assert!(go(&mut t, &bad).is_err());
        let after = t.root().expect("still a root");
        assert_eq!(before.children.len(), after.children.len());
        assert_eq!(after.children[0].print, Fingerprint::Text("a".into()));
        assert_eq!(after.children[0].key, None, "not the rejected frame's key");

        // And the next good frame still diffs cleanly against it.
        assert_eq!(go(&mut t, &column(vec![text("a"), text("b")])).expect("ok"), None);
    }

    #[test]
    fn every_child_is_contained_by_its_parent() {
        // The invariant that lets a removed widget be damaged by its own rectangle alone.
        // Asserted over every container at once, because it is the sort of thing one node
        // kind quietly stops honouring.
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
    }

    #[test]
    fn container_properties_are_part_of_a_nodes_fingerprint() {
        // Changing a `Padding`'s insets or a `Column`'s spacing moves children, which damages
        // for that reason anyway — so dropping either from the fingerprint left every test
        // passing. Belt and braces, asserted directly rather than through a side effect.
        assert_ne!(
            Fingerprint::of(&padding::<Msg>(Insets::all(1), text("x"))),
            Fingerprint::of(&padding::<Msg>(Insets::all(2), text("x")))
        );
        assert_ne!(
            Fingerprint::of(&with_spacing(column::<Msg>(vec![text("x")]), 1)),
            Fingerprint::of(&with_spacing(column::<Msg>(vec![text("x")]), 2))
        );
        // ...and children are *not* part of it, or a one-character edit in a leaf would
        // compare every ancestor unequal and repaint the window.
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
    fn a_zero_extent_change_does_not_drag_damage_to_the_origin() {
        // Unioning an empty rect at (0,0) would grow every damage region back to the corner,
        // which repaints the whole window for a one-line edit far from it.
        let mut d = Damage::new();
        d.add(Rect::new(100, 100, 10, 10));
        d.add(Rect::new(0, 0, 0, 0));
        assert_eq!(d.take(), Some(Rect::new(100, 100, 10, 10)));
    }

}
