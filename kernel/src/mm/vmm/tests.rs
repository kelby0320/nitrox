use super::*;
use crate::mm::test_support::init_global_heap;

const PAGE: u64 = PAGE_SIZE as u64;

fn va(v: u64) -> VirtAddr {
    VirtAddr::new(v)
}

fn range(start: u64, end: u64) -> VAddrRange {
    VAddrRange::new(va(start), va(end)).expect("test range must be valid")
}

fn anon_box(r: VAddrRange) -> KBox<Vma> {
    KBox::try_new(Vma::new(r, Protection::empty(), MappingKind::Anonymous)).expect("test heap exhausted")
}

fn insert_or_panic(tree: &mut VmaTree, r: VAddrRange) {
    tree.insert(anon_box(r)).expect("test insert must succeed");
}

/// Verify the subtree at `node`: BST ordering by `range.start` (so no overlaps, as inserts
/// reject them) and the augmentation invariant. Returns the true maximum `range.end`; panics
/// with a descriptive message on a violation.
fn check_bst_and_augmentation(node: Option<NonNull<Vma>>) -> VirtAddr {
    let Some(n) = node else {
        return VirtAddr::new(0);
    };
    // SAFETY: link pointer; live as long as the tree is.
    let v = unsafe { n.as_ref() };
    let mut max_end = v.range.end();
    if let Some(l) = v.link.left {
        let l_max_end = check_bst_and_augmentation(Some(l));
        // SAFETY: live link.
        let lv = unsafe { l.as_ref() };
        assert!(
            lv.range.start() < v.range.start(),
            "BST violation: left child start {:?} not < node start {:?}",
            lv.range.start(),
            v.range.start()
        );
        if l_max_end > max_end {
            max_end = l_max_end;
        }
    }
    if let Some(r) = v.link.right {
        let r_max_end = check_bst_and_augmentation(Some(r));
        // SAFETY: live link.
        let rv = unsafe { r.as_ref() };
        assert!(
            rv.range.start() > v.range.start(),
            "BST violation: right child start {:?} not > node start {:?}",
            rv.range.start(),
            v.range.start()
        );
        if r_max_end > max_end {
            max_end = r_max_end;
        }
    }
    assert_eq!(v.link.subtree_max_end, max_end, "augmentation violation at node starting {:?}", v.range.start());
    max_end
}

/// Verify the red-black properties of the subtree at `node`: no red node has a red parent,
/// every root-to-leaf path has the same number of black nodes. Returns the black-height.
fn check_rb(node: Option<NonNull<Vma>>, parent_color: RbColor) -> usize {
    let Some(n) = node else {
        return 0;
    };
    // SAFETY: live link.
    let v = unsafe { n.as_ref() };
    if v.link.color == RbColor::Red {
        assert_ne!(parent_color, RbColor::Red, "red-red violation at node starting {:?}", v.range.start());
    }
    let lbh = check_rb(v.link.left, v.link.color);
    let rbh = check_rb(v.link.right, v.link.color);
    assert_eq!(lbh, rbh, "black-height mismatch at node starting {:?}: left={lbh}, right={rbh}", v.range.start());
    lbh + match v.link.color {
        RbColor::Black => 1,
        RbColor::Red => 0,
    }
}

fn verify(tree: &VmaTree) {
    check_bst_and_augmentation(tree.root);
    if let Some(root) = tree.root {
        // SAFETY: live link.
        let rc = unsafe { root.as_ref().link.color };
        assert_eq!(rc, RbColor::Black, "root must be black");
    }
    check_rb(tree.root, RbColor::Black);
}

fn count_nodes(node: Option<NonNull<Vma>>) -> usize {
    let Some(n) = node else { return 0 };
    // SAFETY: live link.
    let v = unsafe { n.as_ref() };
    1 + count_nodes(v.link.left) + count_nodes(v.link.right)
}

#[test]
fn vrange_rejects_misaligned_endpoints() {
    assert!(VAddrRange::new(va(0x1), va(PAGE)).is_none());
    assert!(VAddrRange::new(va(0), va(PAGE + 1)).is_none());
    assert!(VAddrRange::new(va(0xFFF), va(PAGE * 2)).is_none());
}

#[test]
fn vrange_rejects_empty_and_inverted() {
    assert!(VAddrRange::new(va(PAGE), va(PAGE)).is_none());
    assert!(VAddrRange::new(va(PAGE * 2), va(PAGE)).is_none());
}

#[test]
fn vrange_len_and_pages() {
    let r = range(0, PAGE * 4);
    assert_eq!(r.len(), PAGE * 4);
    assert_eq!(r.pages(), 4);
}

#[test]
fn vrange_contains_is_half_open() {
    let r = range(PAGE, PAGE * 3);
    assert!(r.contains(va(PAGE)));
    assert!(r.contains(va(PAGE * 2)));
    assert!(r.contains(va(PAGE * 3 - 1)));
    assert!(!r.contains(va(PAGE * 3)));
    assert!(!r.contains(va(PAGE - 1)));
}

#[test]
fn vrange_overlaps_disjoint_and_adjacent() {
    let a = range(0, PAGE);
    let b = range(PAGE * 2, PAGE * 3);
    assert!(!a.overlaps(b));
    assert!(!b.overlaps(a));

    let c = range(0, PAGE);
    let d = range(PAGE, PAGE * 2);
    assert!(!c.overlaps(d));
    assert!(!d.overlaps(c));
}

#[test]
fn vrange_overlaps_partial_and_nested() {
    let a = range(0, PAGE * 3);
    let b = range(PAGE * 2, PAGE * 4);
    assert!(a.overlaps(b));
    assert!(b.overlaps(a));

    let outer = range(0, PAGE * 4);
    let inner = range(PAGE, PAGE * 2);
    assert!(outer.overlaps(inner));
    assert!(inner.overlaps(outer));
}

#[test]
fn vrange_intersect_disjoint_is_none() {
    let a = range(0, PAGE);
    let b = range(PAGE * 2, PAGE * 3);
    assert_eq!(a.intersect(b), None);

    let c = range(0, PAGE);
    let d = range(PAGE, PAGE * 2);
    assert_eq!(c.intersect(d), None);
}

#[test]
fn vrange_intersect_partial_and_nested() {
    let a = range(0, PAGE * 3);
    let b = range(PAGE * 2, PAGE * 4);
    assert_eq!(a.intersect(b), Some(range(PAGE * 2, PAGE * 3)));
    assert_eq!(b.intersect(a), Some(range(PAGE * 2, PAGE * 3)));

    let outer = range(0, PAGE * 4);
    let inner = range(PAGE, PAGE * 2);
    assert_eq!(outer.intersect(inner), Some(inner));
    assert_eq!(inner.intersect(outer), Some(inner));
}

#[test]
fn protection_empty_is_zero() {
    assert_eq!(Protection::empty().bits(), 0);
    assert!(!Protection::empty().contains(Protection::WRITE));
    assert!(!Protection::empty().contains(Protection::EXEC));
    assert!(!Protection::empty().contains(Protection::USER));
}

#[test]
fn vma_new_initializes_link_unlinked_red_with_self_max() {
    let r = range(PAGE, PAGE * 4);
    let v = Vma::new(r, Protection::empty(), MappingKind::Anonymous);
    assert!(v.link.parent.is_none());
    assert!(v.link.left.is_none());
    assert!(v.link.right.is_none());
    assert_eq!(v.link.color, RbColor::Red);
    assert_eq!(v.link.subtree_max_end, r.end());
}

#[test]
fn protection_union_and_contains() {
    let rw_user = Protection::WRITE | Protection::USER;
    assert!(rw_user.contains(Protection::WRITE));
    assert!(rw_user.contains(Protection::USER));
    assert!(!rw_user.contains(Protection::EXEC));
    assert!(rw_user.contains(rw_user));
    assert!(rw_user.contains(Protection::empty()));
}

#[test]
fn empty_tree_is_valid() {
    let tree = VmaTree::new();
    assert!(tree.is_empty());
    assert_eq!(tree.len(), 0);
    verify(&tree);
    assert!(tree.find_covering(va(PAGE)).is_none());
}

#[test]
fn single_insert_finds_and_misses_correctly() {
    init_global_heap();
    let mut tree = VmaTree::new();
    insert_or_panic(&mut tree, range(PAGE * 4, PAGE * 8));
    assert_eq!(tree.len(), 1);
    verify(&tree);

    assert!(tree.find_covering(va(PAGE * 4)).is_some());
    assert!(tree.find_covering(va(PAGE * 6)).is_some());
    assert!(tree.find_covering(va(PAGE * 8 - 1)).is_some());
    assert!(tree.find_covering(va(PAGE * 8)).is_none()); // half-open
    assert!(tree.find_covering(va(PAGE * 3)).is_none());
}

#[test]
fn insert_rejects_overlap_in_every_shape() {
    init_global_heap();
    let mut tree = VmaTree::new();
    insert_or_panic(&mut tree, range(PAGE * 4, PAGE * 8));

    assert!(tree.insert(anon_box(range(PAGE * 4, PAGE * 8))).is_err());
    assert!(tree.insert(anon_box(range(PAGE * 6, PAGE * 10))).is_err());
    assert!(tree.insert(anon_box(range(PAGE * 2, PAGE * 6))).is_err());
    assert!(tree.insert(anon_box(range(PAGE * 5, PAGE * 7))).is_err());
    assert!(tree.insert(anon_box(range(PAGE * 2, PAGE * 10))).is_err());

    assert_eq!(tree.len(), 1);
    verify(&tree);
}

#[test]
fn adjacent_ranges_do_not_overlap() {
    init_global_heap();
    let mut tree = VmaTree::new();
    insert_or_panic(&mut tree, range(PAGE * 4, PAGE * 8));
    insert_or_panic(&mut tree, range(PAGE * 8, PAGE * 12));
    insert_or_panic(&mut tree, range(0, PAGE * 4));
    assert_eq!(tree.len(), 3);
    verify(&tree);
}

#[test]
fn ascending_inserts_balance_and_keep_invariants() {
    init_global_heap();
    let mut tree = VmaTree::new();
    let n: u64 = 64;
    for i in 0..n {
        let start = (i * 2) * PAGE;
        insert_or_panic(&mut tree, range(start, start + PAGE));
        verify(&tree);
        assert_eq!(tree.len() as u64, i + 1);
    }
    assert_eq!(count_nodes(tree.root) as u64, n);
}

#[test]
fn descending_inserts_balance_and_keep_invariants() {
    init_global_heap();
    let mut tree = VmaTree::new();
    let n: u64 = 64;
    for i in (0..n).rev() {
        let start = (i * 2) * PAGE;
        insert_or_panic(&mut tree, range(start, start + PAGE));
        verify(&tree);
    }
    assert_eq!(count_nodes(tree.root) as u64, n);
}

#[test]
fn shuffled_inserts_maintain_invariants_throughout() {
    // Shuffle the insertion order with a fixed-seed LCG and verify after each insert.
    init_global_heap();
    let n: usize = 200;
    let mut order: [usize; 200] = core::array::from_fn(|i| i);

    let mut lcg: u64 = 0xDEAD_BEEF_CAFE_F00D;
    for i in (1..n).rev() {
        lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let j = (lcg as usize) % (i + 1);
        order.swap(i, j);
    }

    let mut tree = VmaTree::new();
    for (count, &idx) in order.iter().enumerate() {
        let start = (idx as u64 * 4) * PAGE;
        insert_or_panic(&mut tree, range(start, start + PAGE * 2));
        verify(&tree);
        assert_eq!(tree.len(), count + 1);
    }

    for &idx in order.iter() {
        let start = (idx as u64 * 4) * PAGE;
        assert!(tree.find_covering(va(start)).is_some());
        assert!(tree.find_covering(va(start + PAGE)).is_some());
        assert!(tree.find_covering(va(start + PAGE * 2)).is_none()); // exclusive end
    }
}

#[test]
fn remove_on_empty_returns_none() {
    let mut tree = VmaTree::new();
    assert!(tree.remove_covering(va(PAGE)).is_none());
    assert!(tree.is_empty());
    verify(&tree);
}

#[test]
fn remove_missing_returns_none_and_leaves_tree_unchanged() {
    init_global_heap();
    let mut tree = VmaTree::new();
    insert_or_panic(&mut tree, range(PAGE * 4, PAGE * 8));
    assert!(tree.remove_covering(va(PAGE)).is_none());
    assert!(tree.remove_covering(va(PAGE * 16)).is_none());
    assert!(tree.remove_covering(va(PAGE * 8)).is_none()); // half-open
    assert_eq!(tree.len(), 1);
    verify(&tree);
}

#[test]
fn single_node_remove_leaves_empty_tree() {
    init_global_heap();
    let mut tree = VmaTree::new();
    insert_or_panic(&mut tree, range(PAGE * 4, PAGE * 8));
    let removed = tree.remove_covering(va(PAGE * 6)).expect("must be there");
    assert_eq!(removed.range, range(PAGE * 4, PAGE * 8));
    assert!(tree.is_empty());
    assert_eq!(tree.len(), 0);
    verify(&tree);
}

#[test]
fn removing_all_in_insertion_order_maintains_invariants() {
    init_global_heap();
    let mut tree = VmaTree::new();
    let n: u64 = 64;
    for i in 0..n {
        let start = (i * 2) * PAGE;
        insert_or_panic(&mut tree, range(start, start + PAGE));
    }
    verify(&tree);
    for i in 0..n {
        let start = (i * 2) * PAGE;
        let removed = tree.remove_covering(va(start)).expect("must be there");
        assert_eq!(removed.range.start(), va(start));
        verify(&tree);
    }
    assert!(tree.is_empty());
}

#[test]
fn removing_all_in_reverse_order_maintains_invariants() {
    init_global_heap();
    let mut tree = VmaTree::new();
    let n: u64 = 64;
    for i in 0..n {
        let start = (i * 2) * PAGE;
        insert_or_panic(&mut tree, range(start, start + PAGE));
    }
    for i in (0..n).rev() {
        let start = (i * 2) * PAGE;
        let removed = tree.remove_covering(va(start)).expect("must be there");
        assert_eq!(removed.range.start(), va(start));
        verify(&tree);
    }
    assert!(tree.is_empty());
}

#[test]
fn shuffled_insert_then_shuffled_remove_maintains_invariants() {
    // The torture test: 200 ranges inserted and removed in different shuffled orders with
    // full invariant checks after every operation, the delete fixup's most-exercised path.
    init_global_heap();
    let n: usize = 200;
    let mut insert_order: [usize; 200] = core::array::from_fn(|i| i);
    let mut remove_order: [usize; 200] = core::array::from_fn(|i| i);

    fn shuffle(arr: &mut [usize], mut lcg: u64) {
        for i in (1..arr.len()).rev() {
            lcg = lcg.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = (lcg as usize) % (i + 1);
            arr.swap(i, j);
        }
    }
    shuffle(&mut insert_order, 0xDEAD_BEEF_CAFE_F00D);
    shuffle(&mut remove_order, 0xFACE_FEED_5EA1_0DDD);

    let mut tree = VmaTree::new();
    for &idx in insert_order.iter() {
        let start = (idx as u64 * 4) * PAGE;
        insert_or_panic(&mut tree, range(start, start + PAGE * 2));
        verify(&tree);
    }
    assert_eq!(tree.len(), n);

    for (count, &idx) in remove_order.iter().enumerate() {
        let start = (idx as u64 * 4) * PAGE;
        let removed = tree.remove_covering(va(start + PAGE)).expect("must find every inserted range");
        assert_eq!(removed.range.start(), va(start));
        verify(&tree);
        assert_eq!(tree.len(), n - count - 1);
    }
    assert!(tree.is_empty());
}

#[test]
fn find_no_longer_finds_removed_range() {
    init_global_heap();
    let mut tree = VmaTree::new();
    insert_or_panic(&mut tree, range(PAGE * 4, PAGE * 8));
    insert_or_panic(&mut tree, range(PAGE * 12, PAGE * 16));
    assert!(tree.find_covering(va(PAGE * 6)).is_some());
    let _ = tree.remove_covering(va(PAGE * 6)).unwrap();
    assert!(tree.find_covering(va(PAGE * 6)).is_none());
    assert!(tree.find_covering(va(PAGE * 14)).is_some()); // other range still present
}

#[test]
fn iter_on_empty_tree_yields_nothing() {
    let tree = VmaTree::new();
    assert_eq!(tree.iter().count(), 0);
    assert_eq!(tree.iter_overlapping(range(0, PAGE * 16)).count(), 0);
}

#[test]
fn iter_yields_every_vma_in_ascending_order() {
    init_global_heap();
    let mut tree = VmaTree::new();
    let starts: [u64; 5] = [40, 10, 30, 0, 20];
    for s in starts.iter() {
        insert_or_panic(&mut tree, range(s * PAGE, (s + 4) * PAGE));
    }
    let collected: [VirtAddr; 5] = {
        let mut arr = [va(0); 5];
        for (i, v) in tree.iter().enumerate() {
            arr[i] = v.range.start();
        }
        arr
    };
    assert_eq!(collected, [va(0), va(10 * PAGE), va(20 * PAGE), va(30 * PAGE), va(40 * PAGE)]);
}

#[test]
fn find_first_overlapping_returns_leftmost_when_multiple_overlap() {
    init_global_heap();
    let mut tree = VmaTree::new();
    for s in [10u64, 20, 30] {
        insert_or_panic(&mut tree, range(s * PAGE, (s + 2) * PAGE));
    }
    let q = range(11 * PAGE, 21 * PAGE);
    let first = tree.find_first_overlapping(q).expect("must overlap");
    assert_eq!(first.range.start(), va(10 * PAGE));
}

#[test]
fn find_first_overlapping_returns_none_when_disjoint() {
    init_global_heap();
    let mut tree = VmaTree::new();
    insert_or_panic(&mut tree, range(10 * PAGE, 12 * PAGE));
    insert_or_panic(&mut tree, range(20 * PAGE, 22 * PAGE));
    assert!(tree.find_first_overlapping(range(0, 8 * PAGE)).is_none());
    assert!(tree.find_first_overlapping(range(14 * PAGE, 18 * PAGE)).is_none());
    assert!(tree.find_first_overlapping(range(30 * PAGE, 40 * PAGE)).is_none());
    assert!(tree.find_first_overlapping(range(12 * PAGE, 14 * PAGE)).is_none());
}

#[test]
fn iter_overlapping_yields_contiguous_run_in_order() {
    init_global_heap();
    let mut tree = VmaTree::new();
    for s in [0u64, 10, 20, 30, 40] {
        insert_or_panic(&mut tree, range(s * PAGE, (s + 4) * PAGE));
    }
    let q = range(12 * PAGE, 32 * PAGE);
    let mut got = [va(0); 3];
    let mut n = 0;
    for v in tree.iter_overlapping(q) {
        got[n] = v.range.start();
        n += 1;
    }
    assert_eq!(n, 3);
    assert_eq!(got, [va(10 * PAGE), va(20 * PAGE), va(30 * PAGE)]);
}

#[test]
fn iter_overlapping_single_hit_nested_inside_one_vma() {
    init_global_heap();
    let mut tree = VmaTree::new();
    insert_or_panic(&mut tree, range(0, PAGE * 16));
    let mut hits = 0;
    for v in tree.iter_overlapping(range(PAGE * 4, PAGE * 8)) {
        assert_eq!(v.range, range(0, PAGE * 16));
        hits += 1;
    }
    assert_eq!(hits, 1);
}

#[test]
fn iter_overlapping_covering_the_entire_tree_matches_iter() {
    init_global_heap();
    let mut tree = VmaTree::new();
    for s in [5u64, 50, 25, 75, 100, 10, 60] {
        insert_or_panic(&mut tree, range(s * PAGE, (s + 2) * PAGE));
    }
    let big = range(0, 200 * PAGE);
    let from_iter: [VirtAddr; 7] = {
        let mut arr = [va(0); 7];
        for (i, v) in tree.iter().enumerate() {
            arr[i] = v.range.start();
        }
        arr
    };
    let from_overlap: [VirtAddr; 7] = {
        let mut arr = [va(0); 7];
        for (i, v) in tree.iter_overlapping(big).enumerate() {
            arr[i] = v.range.start();
        }
        arr
    };
    assert_eq!(from_iter, from_overlap);
}

#[test]
fn iter_overlapping_after_removes_skips_removed_ranges() {
    init_global_heap();
    let mut tree = VmaTree::new();
    for s in [10u64, 20, 30, 40, 50] {
        insert_or_panic(&mut tree, range(s * PAGE, (s + 2) * PAGE));
    }
    let _ = tree.remove_covering(va(30 * PAGE)).expect("present");
    verify(&tree);
    let q = range(25 * PAGE, 45 * PAGE);
    let mut hits = [va(0); 4];
    let mut n = 0;
    for v in tree.iter_overlapping(q) {
        hits[n] = v.range.start();
        n += 1;
    }
    assert_eq!(n, 1);
    assert_eq!(hits[0], va(40 * PAGE));
}

#[test]
fn drop_releases_every_owned_box() {
    // Drop a large tree, then allocate as many fresh Vmas: a leak would exhaust the 16 MiB
    // host heap. KBox's own tests cover each box dropping exactly once.
    init_global_heap();
    for _ in 0..4 {
        let mut tree = VmaTree::new();
        for i in 0..256u64 {
            insert_or_panic(&mut tree, range(i * 2 * PAGE, (i * 2 + 1) * PAGE));
        }
        verify(&tree);
    }
}

/// **A file-backed VMA over another kind of object stops at its drop**, rather than counting
/// an unmapped writer into that object's fields.
#[test]
#[should_panic(expected = "left == right")]
fn a_file_backed_vma_over_another_kind_of_object_stops_at_its_drop() {
    use crate::libkern::handle::KObjectType;
    use crate::object::MemoryObject;
    init_global_heap();
    // SAFETY: a fresh `MemoryObject` under its own tag.
    let mo = unsafe { crate::drivers::adopt(MemoryObject::try_new(PAGE_SIZE).unwrap(), KObjectType::MemoryObject) };
    let mut v = Vma::new_object(range(0x40_0000, 0x40_1000), Protection::WRITE, mo, crate::mm::Caching::Normal);
    v.mapping = MappingKind::FileBacked;
    drop(v);
}
