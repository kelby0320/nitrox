//! Virtual memory manager — per-process address-space layer.
//!
//! The third layer of the kernel's memory design (see the table at the
//! top of `docs/architecture/memory-management.md`). The VMM owns the
//! address-space view of memory: virtual address ranges, their
//! protection, what backs them, and the red-black tree that stores them
//! per process. It does not touch hardware page tables directly; that
//! goes through [`arch::Paging`](crate::arch::Paging).
//!
//! This file lands the address-spaces-and-paging slice incrementally.
//! Today it holds the leaf data types ([`VAddrRange`], [`Protection`],
//! [`MappingKind`], [`Vma`]) and the [`VmaTree`] — an intrusive
//! interval-augmented red-black tree of `Vma`s with overlap-detecting
//! insert, point lookup, and iterative teardown. Range-overlap
//! iteration, removal, and the `AddressSpace` owner land in the
//! following sub-items.

use core::ptr::NonNull;

use crate::libkern::KBox;
use crate::mm::{PAGE_SIZE, VirtAddr};
use crate::object::ObjectRef;

/// A half-open range of virtual addresses, `[start, end)`.
///
/// Both endpoints are 4 KiB aligned and `end > start`. The range is the
/// unit a [`Vma`] covers, but the type is dumber than a `Vma`: it carries
/// no protection or backing information, so the VMM can pass a
/// `VAddrRange` to the tree's overlap queries without manufacturing a
/// fake `Vma`.
///
/// Half-open intervals were chosen for the same reason most Unix VMM
/// code uses them: `len()` is `end - start` with no off-by-one, and two
/// "adjacent" ranges (one ends where the next begins) compose by
/// endpoint equality rather than `+ 1`.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct VAddrRange {
    start: VirtAddr,
    end: VirtAddr,
}

impl VAddrRange {
    /// Construct a range covering `[start, end)`.
    ///
    /// Returns `None` if either endpoint is not 4 KiB aligned, or if
    /// `end <= start`. An empty range has no meaning at the VMA layer:
    /// every `Vma` covers at least one page.
    pub const fn new(start: VirtAddr, end: VirtAddr) -> Option<Self> {
        if !start.is_page_aligned() || !end.is_page_aligned() {
            return None;
        }
        if end.as_u64() <= start.as_u64() {
            return None;
        }
        Some(Self { start, end })
    }

    pub const fn start(self) -> VirtAddr {
        self.start
    }

    pub const fn end(self) -> VirtAddr {
        self.end
    }

    /// Length of the range in bytes; always a non-zero multiple of
    /// [`PAGE_SIZE`].
    pub const fn len(self) -> u64 {
        self.end.as_u64() - self.start.as_u64()
    }

    /// Number of 4 KiB pages the range covers.
    pub const fn pages(self) -> u64 {
        self.len() / (PAGE_SIZE as u64)
    }

    /// `true` if `addr` lies within `[start, end)`.
    pub const fn contains(self, addr: VirtAddr) -> bool {
        addr.as_u64() >= self.start.as_u64() && addr.as_u64() < self.end.as_u64()
    }

    /// `true` if `self` and `other` share at least one byte. Adjacent
    /// (touching) ranges do **not** overlap under the half-open
    /// convention.
    pub const fn overlaps(self, other: VAddrRange) -> bool {
        self.start.as_u64() < other.end.as_u64() && other.start.as_u64() < self.end.as_u64()
    }

    /// The intersection of `self` and `other`, or `None` if they are
    /// disjoint — including the merely adjacent case.
    pub fn intersect(self, other: VAddrRange) -> Option<VAddrRange> {
        let start = core::cmp::max(self.start, other.start);
        let end = core::cmp::min(self.end, other.end);
        if end <= start {
            None
        } else {
            Some(VAddrRange { start, end })
        }
    }
}

/// VMA-level access policy: who may reach the mapping and what may they
/// do with it.
///
/// `Protection` is a narrower abstraction than
/// [`PageFlags`](crate::arch::paging::PageFlags). Not every PTE flag is
/// meaningful at the VMA layer: a VMA never carries `GLOBAL` (a user
/// mapping cannot be global; a kernel-image mapping is global by
/// construction at install time, not via a per-VMA decision), and
/// cache-attribute bits are per-mapping policy decided by the code that
/// installs the PTE (driver MMIO, framebuffer), not a property of the
/// address range. The VMM translates `Protection` to `PageFlags` when
/// populating a leaf.
///
/// "Readable" is not a separate flag: a `Vma` existing in the tree
/// implies the range is readable. x86_64 has no separate read bit
/// (present implies readable); we surface that uniformly. The
/// `mprotect`-style distinction between "no access" and "read-only" is
/// expressed by removing the VMA entirely, not by clearing a flag.
///
/// Hand-rolled bitflags: the kernel uses no `bitflags` crate (see
/// `kernel/CLAUDE.md`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Protection(u8);

impl Protection {
    /// The mapping may be written. Without it, the mapping is read-only.
    pub const WRITE: Protection = Protection(1 << 0);
    /// The mapping may be executed. Without it, instruction fetches fault.
    pub const EXEC: Protection = Protection(1 << 1);
    /// The mapping is reachable from ring 3. Without it, kernel-only.
    pub const USER: Protection = Protection(1 << 2);

    /// No flags: kernel-only, read-only, non-executable — the safe
    /// default. Contrast
    /// [`PageFlags::empty`](crate::arch::paging::PageFlags::empty), which
    /// is *executable* by default because `NO_EXECUTE` is opt-in at the
    /// hardware level. The VMM presents the safer logical default and
    /// translates to the inverted PTE encoding at install time.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// `true` if every flag set in `other` is also set in `self`.
    pub const fn contains(self, other: Protection) -> bool {
        (self.0 & other.0) == other.0
    }

    /// The union of two flag sets.
    pub const fn union(self, other: Protection) -> Self {
        Protection(self.0 | other.0)
    }

    /// The raw bit pattern, for tests and debugging.
    pub const fn bits(self) -> u8 {
        self.0
    }
}

impl core::ops::BitOr for Protection {
    type Output = Protection;

    fn bitor(self, rhs: Protection) -> Protection {
        self.union(rhs)
    }
}

/// What backs a [`Vma`]'s pages.
///
/// `FileBacked(Handle)` lands with the page-cache and fs-server integration
/// in Phase 2; `Device(PhysAddr)` lands with the driver MMIO mapper. The enum
/// is a `Copy` marker — the per-mapping owning reference for [`Object`] lives
/// in [`Vma::object`], not here, so the enum stays `Copy`/`Eq`. Adding a
/// variant only touches the call sites that need to act on the new backing
/// kind (notably the `match` in `AddressSpace::free_vma_pages`).
///
/// [`Object`]: MappingKind::Object
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MappingKind {
    /// Zero-initialised; backed by anonymous physical frames owned by the
    /// VMA itself (allocated at map time, freed on unmap).
    Anonymous,
    /// Backed by a [`MemoryObject`](crate::object::MemoryObject)'s own,
    /// pre-allocated frames. The owning [`ObjectRef`] is held in
    /// [`Vma::object`]; the frames are freed by the object on its last-ref
    /// drop, **not** by unmap.
    Object,
}

/// Red-black tree node colour for the intrusive link in [`Vma`].
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum RbColor {
    Red,
    Black,
}

/// `true` if `node` is a real, red node. The delete fixup needs to
/// treat `None` (a missing leaf) as black per the RB-tree convention;
/// this helper makes that the natural single-expression check.
fn is_red(node: Option<NonNull<Vma>>) -> bool {
    // SAFETY: when `Some`, the link pointer references a live tree
    // node by the invariants documented on `VmaTree`.
    node.map(|n| unsafe { n.as_ref().link.color } == RbColor::Red)
        .unwrap_or(false)
}

/// Intrusive red-black-tree link embedded in every [`Vma`].
///
/// Raw pointers because the tree forms cycles — every non-root node's
/// parent slot points back through the parent's child slot at the same
/// node — which Rust's borrow checker correctly refuses to express with
/// references. The pointers are non-aliasing in the safe sense: every
/// `Vma` is reached through exactly one tree node. Expressing that as
/// `&mut Vma` is still impossible because a tree walk needs the parent
/// reference live while it visits a child, and the parent's child slot
/// aliases the child reference.
///
/// `NonNull` rather than `*mut Vma` for the nullable-but-aligned shape
/// (`Option<NonNull<Vma>>` is one machine word with a niche-optimised
/// `None`) and the soundness invariant that a non-`None` link is never
/// dangling-by-construction.
///
/// The link is private to `mm::vmm`; callers outside this module neither
/// see nor construct one. It carries no methods today — accessors and
/// the structural mutators arrive with the tree operations.
#[derive(Debug)]
struct RbLink {
    parent: Option<NonNull<Vma>>,
    left: Option<NonNull<Vma>>,
    right: Option<NonNull<Vma>>,
    color: RbColor,
    /// The largest `range.end` in the subtree rooted at this node — the
    /// interval-tree augmentation. Maintained on every structural
    /// mutation by the tree operations.
    subtree_max_end: VirtAddr,
}

impl RbLink {
    /// Construct a link for a freshly-allocated `Vma` whose own range
    /// ends at `end`. New nodes are red — the standard RB-tree insertion
    /// convention; the insert fixup recolours as needed.
    const fn new(end: VirtAddr) -> Self {
        Self {
            parent: None,
            left: None,
            right: None,
            color: RbColor::Red,
            subtree_max_end: end,
        }
    }
}

/// A virtual memory area: a contiguous virtual address range with
/// uniform protection and a single backing kind.
///
/// The smallest unit the VMM tracks. An address space is a tree of
/// non-overlapping `Vma`s. `mprotect`-style operations that change
/// protection on only a sub-range, and merges of adjacent compatible
/// VMAs, are tree operations rather than field mutations: a `Vma` is
/// conceptually immutable once installed.
///
/// Once a `Vma` has been wired into a tree, its address must not change
/// — the intrusive links of its parent and children hold raw pointers
/// at it. Storing `Vma`s in `KBox` (and never moving the box) satisfies
/// this. The non-`Send` / non-`Sync` status that follows from the link
/// field is intentional: synchronisation is provided by the owning
/// `AddressSpace`'s lock, not by `Vma` itself.
#[derive(Debug)]
pub struct Vma {
    pub range: VAddrRange,
    pub prot: Protection,
    pub mapping: MappingKind,
    /// `Some(_)` iff `mapping == MappingKind::Object`: the owning reference to
    /// the backing [`MemoryObject`](crate::object::MemoryObject), held so its
    /// frames outlive this mapping. Dropped when the VMA is freed (unmap or
    /// address-space teardown), releasing one object reference. `None` for
    /// anonymous mappings.
    pub object: Option<ObjectRef>,
    link: RbLink,
}

impl Vma {
    pub const fn new(range: VAddrRange, prot: Protection, mapping: MappingKind) -> Self {
        Self {
            range,
            prot,
            mapping,
            object: None,
            link: RbLink::new(range.end()),
        }
    }

    /// Construct an object-backed VMA holding `object` alive for its lifetime.
    /// Not `const` because it stores a (non-`const`) [`ObjectRef`].
    pub fn new_object(range: VAddrRange, prot: Protection, object: ObjectRef) -> Self {
        Self {
            range,
            prot,
            mapping: MappingKind::Object,
            object: Some(object),
            link: RbLink::new(range.end()),
        }
    }
}

/// An interval-augmented red-black tree of [`Vma`]s, keyed on
/// `range.start`.
///
/// The tree owns the boxed `Vma`s it stores. Insertion takes a
/// [`KBox<Vma>`] and consumes it on success; on overlap it returns the
/// box back so the caller decides what to do. Removal (next sub-item)
/// returns the box. On drop the tree iteratively frees every node
/// without allocating.
///
/// ## Invariants
///
/// 1. **BST ordering by `range.start`.** Sub-trees are uniquely ordered
///    because no two stored `Vma`s overlap, so no two share a start.
/// 2. **Red-black properties.** Root is black; no red node has a red
///    parent; every root-to-leaf path crosses the same number of black
///    nodes.
/// 3. **Interval augmentation.** Each node's `subtree_max_end` equals
///    the maximum `range.end` over all `Vma`s in its subtree.
/// 4. **Non-overlap.** Inserts that would create an overlap with an
///    existing VMA are rejected.
///
/// All link pointers (`parent`, `left`, `right`) refer to live `Vma`s
/// owned by this tree, or are `None`. The `Vma` at the address held in
/// any link is live until either the tree drops or the node is removed
/// — both events run through the tree, so external code cannot
/// invalidate them.
pub struct VmaTree {
    root: Option<NonNull<Vma>>,
    len: usize,
}

impl VmaTree {
    pub const fn new() -> Self {
        Self { root: None, len: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    /// Insert `boxed` into the tree.
    ///
    /// On success the tree takes ownership of the box. On overlap with
    /// an existing VMA the box is returned untouched so the caller can
    /// drop it, modify it, or retry with a different range. The
    /// rejection is decided by walking the BST: if the new range
    /// overlaps any VMA in the tree it necessarily overlaps one on the
    /// insertion path, so a single walk catches every conflict (proof
    /// in the architecture doc).
    pub fn insert(&mut self, boxed: KBox<Vma>) -> Result<(), KBox<Vma>> {
        // Find the parent slot via a BST walk, checking for overlap at
        // every visited node. `parent` ends up at the future leaf's
        // parent; `parent_left` records which side to hook into.
        let new_range = boxed.range;
        let mut parent: Option<NonNull<Vma>> = None;
        let mut parent_left = false;
        let mut current = self.root;
        while let Some(node) = current {
            // SAFETY: `node` is a tree-link pointer, valid for the tree's
            // lifetime per the type invariants above.
            let v = unsafe { node.as_ref() };
            if new_range.overlaps(v.range) {
                return Err(boxed);
            }
            parent = Some(node);
            if new_range.start() < v.range.start() {
                parent_left = true;
                current = v.link.left;
            } else {
                parent_left = false;
                current = v.link.right;
            }
        }

        // Hook the new node in. KBox::into_raw consumes the box and
        // hands ownership to the tree; from here on the tree is
        // responsible for freeing it.
        let new_node = KBox::into_raw(boxed);
        // SAFETY: `new_node` is the just-allocated node, no other reference
        // exists; we initialise its link to point at its parent.
        unsafe {
            let n = new_node.as_ptr();
            (*n).link.parent = parent;
            (*n).link.left = None;
            (*n).link.right = None;
            (*n).link.color = RbColor::Red;
            (*n).link.subtree_max_end = (*n).range.end();
        }
        match parent {
            None => self.root = Some(new_node),
            Some(p) => unsafe {
                // SAFETY: `p` is a live tree node by the walk invariants.
                let pr = p.as_ptr();
                if parent_left {
                    (*pr).link.left = Some(new_node);
                } else {
                    (*pr).link.right = Some(new_node);
                }
            },
        }
        self.len += 1;

        // Update interval augmentation up the path. This is independent
        // of RB fixup: the set of nodes in each ancestor's subtree
        // changed, so each ancestor's subtree_max_end may have grown.
        self.update_max_end_up_from(parent);

        // RB fixup: restore the no-red-red property. Rotations inside
        // the fixup separately maintain augmentation on the rotated
        // nodes (see `rotate_left` / `rotate_right`).
        self.insert_fixup(new_node);
        Ok(())
    }

    /// Find the `Vma` whose range contains `addr`, if any. The interval
    /// augmentation isn't needed for a point query — a plain BST walk
    /// is O(log n).
    pub fn find_covering(&self, addr: VirtAddr) -> Option<&Vma> {
        let mut current = self.root;
        while let Some(node) = current {
            // SAFETY: link pointer; live by the type invariants.
            let v = unsafe { node.as_ref() };
            if v.range.contains(addr) {
                return Some(v);
            } else if addr < v.range.start() {
                current = v.link.left;
            } else {
                current = v.link.right;
            }
        }
        None
    }

    /// Find the lowest-start `Vma` that overlaps `query`, if any.
    ///
    /// O(log n) plain BST walk, no backtracking, no augmentation
    /// consumed. At each node:
    /// - If `node.start >= query.end`, the node and its right subtree
    ///   lie entirely after the query — descend left.
    /// - Otherwise, if `node.range` overlaps `query`, record it as the
    ///   best candidate so far and descend left to look for an earlier
    ///   one (`best` is overwritten only by smaller `start`s).
    /// - Otherwise (`node.end <= query.start`, node entirely before
    ///   query), descend right.
    ///
    /// The `subtree_max_end` augmentation is maintained by the tree
    /// but not consulted here: the leftmost-overlap query is already
    /// O(log n) without it. Augmentation pays off for disjoint-range
    /// stabbing queries (a later sub-item) where subtree pruning
    /// avoids visiting entire branches.
    pub fn find_first_overlapping(&self, query: VAddrRange) -> Option<&Vma> {
        let mut current = self.root;
        let mut best: Option<NonNull<Vma>> = None;
        while let Some(node) = current {
            // SAFETY: live link.
            let v = unsafe { node.as_ref() };
            if v.range.start() >= query.end() {
                current = v.link.left;
            } else if v.range.overlaps(query) {
                best = Some(node);
                current = v.link.left;
            } else {
                current = v.link.right;
            }
        }
        // SAFETY: live link, lifetime tied to `&self`.
        best.map(|n| unsafe { n.as_ref() })
    }

    /// Find the lowest page-aligned gap of `size` bytes within `[min, max)`
    /// not covered by any existing VMA, for `sys_memory_map(hint = 0)`.
    ///
    /// O(n) scan of the VMAs in ascending order (the common Phase 1 case has
    /// a handful of mappings). Returns `None` if no gap fits. The produced
    /// range is page-aligned: the cursor starts at `min` (a page-aligned
    /// base) and only ever advances to a VMA's `range.end()`, which the
    /// [`VAddrRange`] invariant keeps page-aligned.
    pub fn find_free_range(
        &self,
        min: VirtAddr,
        max: VirtAddr,
        size: u64,
    ) -> Option<VAddrRange> {
        let max = max.as_u64();
        let mut cursor = min.as_u64();
        for v in self.iter() {
            let s = v.range.start().as_u64();
            let e = v.range.end().as_u64();
            if e <= cursor {
                // Entirely behind the cursor; skip.
                continue;
            }
            // The gap [cursor, s) precedes this VMA. Does `size` fit in it?
            if s >= cursor.saturating_add(size) {
                break;
            }
            // Overlaps the cursor — jump past it and keep looking.
            cursor = e;
            if cursor.saturating_add(size) > max {
                return None;
            }
        }
        if cursor.saturating_add(size) <= max {
            VAddrRange::new(VirtAddr::new(cursor), VirtAddr::new(cursor + size))
        } else {
            None
        }
    }

    /// In-order iterator over every `Vma` in the tree, by ascending
    /// `range.start`.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            cur: leftmost_of(self.root),
            _tree: core::marker::PhantomData,
        }
    }

    /// In-order iterator over every `Vma` whose range overlaps `query`,
    /// yielded by ascending `range.start`.
    ///
    /// Overlapping VMAs are necessarily contiguous in the in-order
    /// sequence: if `A` and `B` both overlap `query` and `A` precedes
    /// `B`, any `C` strictly between them has `C.start >= A.end >
    /// query.start`, so `C.start < query.end` would also overlap. The
    /// iterator therefore terminates at the first node with `start >=
    /// query.end` — no further candidates can exist.
    pub fn iter_overlapping(&self, query: VAddrRange) -> OverlapIter<'_> {
        OverlapIter {
            cur: self.find_first_overlapping(query).map(NonNull::from),
            end: query.end(),
            _tree: core::marker::PhantomData,
        }
    }

    /// Find and remove the `Vma` whose range contains `addr`, returning
    /// the owning [`KBox`] so the caller may inspect, drop, or reinsert
    /// a modified version. Returns `None` if no VMA covers `addr`; the
    /// tree is unchanged in that case.
    ///
    /// The deletion is BST-delete with successor swap when the target
    /// has two children, followed by an RB delete-fixup when a black
    /// node was structurally removed (restores the equal-black-height
    /// property). Interval augmentation is refreshed along the affected
    /// path before the fixup runs.
    pub fn remove_covering(&mut self, addr: VirtAddr) -> Option<KBox<Vma>> {
        // Locate the target node (z) via the same BST walk as
        // find_covering. Working with a raw pointer (rather than a
        // borrow) lets us mutate the tree afterwards.
        let mut current = self.root;
        let z = loop {
            let n = current?;
            // SAFETY: live link.
            let v = unsafe { n.as_ref() };
            if v.range.contains(addr) {
                break n;
            } else if addr < v.range.start() {
                current = v.link.left;
            } else {
                current = v.link.right;
            }
        };

        // SAFETY: every link pointer touched below addresses a live
        // tree node (root, ancestor, child, or successor reached via
        // the standard CLRS traversal). The successor `y` is leftmost
        // in `z`'s right subtree so its `.left` is necessarily `None`.
        // Each structural mutation maintains the tree invariants
        // declared at the type level.
        let (x, x_parent, x_is_left, y_original_color) = unsafe {
            let zp = z.as_ptr();
            let z_left = (*zp).link.left;
            let z_right = (*zp).link.right;

            if z_left.is_none() {
                // 0 or 1 (right) child: x = z.right takes z's spot.
                let x = z_right;
                let x_parent = (*zp).link.parent;
                let x_is_left = matches!(
                    x_parent.map(|p| (*p.as_ptr()).link.left == Some(z)),
                    Some(true),
                );
                self.transplant(z, x);
                (x, x_parent, x_is_left, (*zp).link.color)
            } else if z_right.is_none() {
                // 1 (left) child: x = z.left takes z's spot.
                let x = z_left;
                let x_parent = (*zp).link.parent;
                let x_is_left = matches!(
                    x_parent.map(|p| (*p.as_ptr()).link.left == Some(z)),
                    Some(true),
                );
                self.transplant(z, x);
                (x, x_parent, x_is_left, (*zp).link.color)
            } else {
                // Two children: in-order successor y = leftmost of
                // z.right. y has no left child by construction.
                let mut y = z_right.unwrap();
                while let Some(yl) = (*y.as_ptr()).link.left {
                    y = yl;
                }
                let yp = y.as_ptr();
                let y_original_color = (*yp).link.color;
                let x = (*yp).link.right;
                let (x_parent, x_is_left);
                if (*yp).link.parent == Some(z) {
                    // y is z's direct right child. x stays as y.right;
                    // y rises to z's spot. After fixup, x_parent is y.
                    x_parent = Some(y);
                    x_is_left = false;
                } else {
                    // y is deeper. Transplant x for y first, then move
                    // y up to z's spot inheriting z's right subtree.
                    let yp_parent = (*yp).link.parent.unwrap();
                    x_parent = Some(yp_parent);
                    x_is_left = true; // y was leftmost, so y was its parent's left child.
                    self.transplant(y, x);
                    (*yp).link.right = z_right;
                    let zr = z_right.unwrap();
                    (*zr.as_ptr()).link.parent = Some(y);
                }
                // Move y to z's slot. y inherits z's left subtree and
                // z's color (so the only colour that "vanished" is y's
                // own original colour, captured above).
                self.transplant(z, Some(y));
                (*yp).link.left = z_left;
                let zl = z_left.unwrap();
                (*zl.as_ptr()).link.parent = Some(y);
                (*yp).link.color = (*zp).link.color;
                (x, x_parent, x_is_left, y_original_color)
            }
        };

        // Augmentation: the structural change altered subtree contents
        // on the path from x_parent up to root. One walk fixes them all.
        self.update_max_end_up_from(x_parent);

        // RB-fixup only when we removed a black node (paths through the
        // removed position now have one fewer black, which the fixup
        // either absorbs or rotates away).
        if y_original_color == RbColor::Black {
            self.delete_fixup(x, x_parent, x_is_left);
        }

        // Detach z's link state so the returned box looks fresh. Then
        // reconstitute the owning KBox and return it.
        // SAFETY: z came from KBox::into_raw in `insert`, has been
        // structurally removed from the tree, and is no longer reached
        // by any link pointer.
        unsafe {
            let zp = z.as_ptr();
            (*zp).link = RbLink::new((*zp).range.end());
            self.len -= 1;
            Some(KBox::from_raw(z))
        }
    }

    /// Replace `u`'s slot in its parent (or the root) with `v`, and
    /// update `v`'s parent pointer. `u`'s own parent pointer is not
    /// cleared — the caller either reuses `u` (the successor-swap
    /// path) or discards `u` afterwards.
    ///
    /// # Safety
    /// `u` is a live tree node; `v` is `None` or a live tree node.
    unsafe fn transplant(&mut self, u: NonNull<Vma>, v: Option<NonNull<Vma>>) {
        // SAFETY: forwarded from this function's contract.
        unsafe {
            let up = u.as_ptr();
            match (*up).link.parent {
                None => self.root = v,
                Some(parent) => {
                    let pp = parent.as_ptr();
                    if (*pp).link.left == Some(u) {
                        (*pp).link.left = v;
                    } else {
                        (*pp).link.right = v;
                    }
                }
            }
            if let Some(vv) = v {
                (*vv.as_ptr()).link.parent = (*up).link.parent;
            }
        }
    }

    /// Restore the equal-black-height property after structurally
    /// removing a black node. `x` (possibly `None`) replaced the
    /// removed node at the spot that lost a black; `x_parent` is its
    /// parent (or `None` if `x` is the root); `x_is_left` says which
    /// side of `x_parent` `x` sits on, only consulted on the first
    /// iteration since `x` may be `None`.
    ///
    /// Four CLRS-textbook cases, mirrored for left vs. right. The
    /// loop terminates because every iteration either rotates +
    /// recolours to a configuration that locally absorbs the missing
    /// black (cases 1/3/4 → break), or moves the "doubly-black" mark
    /// one level up (case 2). When the mark reaches a red node or the
    /// root, it is absorbed.
    fn delete_fixup(
        &mut self,
        mut x: Option<NonNull<Vma>>,
        mut x_parent: Option<NonNull<Vma>>,
        mut x_is_left: bool,
    ) {
        // SAFETY of the loop body: every link pointer dereferenced is
        // either `x_parent`, `x_parent`'s sibling, or those siblings'
        // children — all of which are live tree nodes. The fixup
        // invariant guarantees the sibling `w` exists (a black `None`
        // x has a non-`None` sibling, else the parent's black-height
        // would already differ before our removal).
        unsafe {
            loop {
                let Some(parent) = x_parent else { break };
                if matches!(x, Some(n) if n.as_ref().link.color == RbColor::Red) {
                    break;
                }

                let pp = parent.as_ptr();
                if x_is_left {
                    let mut w = (*pp).link.right.expect("delete fixup: sibling must exist");
                    let wp = w.as_ptr();
                    if (*wp).link.color == RbColor::Red {
                        // Case 1: red sibling. Rotate parent left and
                        // swap colours; new sibling is one of w's
                        // children, which is black.
                        (*wp).link.color = RbColor::Black;
                        (*pp).link.color = RbColor::Red;
                        self.rotate_left(parent);
                        w = (*pp).link.right.expect("rotation must leave new sibling");
                    }
                    let wpp = w.as_ptr();
                    let w_left_black = !is_red((*wpp).link.left);
                    let w_right_black = !is_red((*wpp).link.right);
                    if w_left_black && w_right_black {
                        // Case 2: black sibling with two black
                        // children — recolour sibling red and push the
                        // missing-black mark up to the parent.
                        (*wpp).link.color = RbColor::Red;
                        x = x_parent;
                        x_parent = (*pp).link.parent;
                        if let Some(new_parent) = x_parent {
                            x_is_left =
                                (*new_parent.as_ptr()).link.left == x;
                        }
                    } else {
                        if w_right_black {
                            // Case 3: outer (right) nephew black, inner
                            // (left) red — rotate sibling right so the
                            // outer nephew becomes red.
                            if let Some(wl) = (*wpp).link.left {
                                (*wl.as_ptr()).link.color = RbColor::Black;
                            }
                            (*wpp).link.color = RbColor::Red;
                            self.rotate_right(w);
                            w = (*pp).link.right.expect("rotation leaves sibling");
                        }
                        // Case 4: outer nephew red — rotate parent and
                        // recolour to absorb the missing black.
                        let wpp = w.as_ptr();
                        (*wpp).link.color = (*pp).link.color;
                        (*pp).link.color = RbColor::Black;
                        if let Some(wr) = (*wpp).link.right {
                            (*wr.as_ptr()).link.color = RbColor::Black;
                        }
                        self.rotate_left(parent);
                        break;
                    }
                } else {
                    // Mirror: x is its parent's right child.
                    let mut w = (*pp).link.left.expect("delete fixup: sibling must exist");
                    let wp = w.as_ptr();
                    if (*wp).link.color == RbColor::Red {
                        (*wp).link.color = RbColor::Black;
                        (*pp).link.color = RbColor::Red;
                        self.rotate_right(parent);
                        w = (*pp).link.left.expect("rotation must leave new sibling");
                    }
                    let wpp = w.as_ptr();
                    let w_right_black = !is_red((*wpp).link.right);
                    let w_left_black = !is_red((*wpp).link.left);
                    if w_right_black && w_left_black {
                        (*wpp).link.color = RbColor::Red;
                        x = x_parent;
                        x_parent = (*pp).link.parent;
                        if let Some(new_parent) = x_parent {
                            x_is_left =
                                (*new_parent.as_ptr()).link.left == x;
                        }
                    } else {
                        if w_left_black {
                            if let Some(wr) = (*wpp).link.right {
                                (*wr.as_ptr()).link.color = RbColor::Black;
                            }
                            (*wpp).link.color = RbColor::Red;
                            self.rotate_left(w);
                            w = (*pp).link.left.expect("rotation leaves sibling");
                        }
                        let wpp = w.as_ptr();
                        (*wpp).link.color = (*pp).link.color;
                        (*pp).link.color = RbColor::Black;
                        if let Some(wl) = (*wpp).link.left {
                            (*wl.as_ptr()).link.color = RbColor::Black;
                        }
                        self.rotate_right(parent);
                        break;
                    }
                }
            }
            // Absorb any remaining missing-black into x by colouring it
            // black. If `x` is `None`, the missing-black is at the root
            // and disappears with nothing to absorb.
            if let Some(n) = x {
                (*n.as_ptr()).link.color = RbColor::Black;
            }
        }
    }

    // ----- Internals -----

    /// Recompute the `subtree_max_end` for the single node `n` from its
    /// own range end and its children's `subtree_max_end`.
    ///
    /// # Safety
    /// `n` is a live tree node; its child links (which we read) are
    /// either `None` or themselves live nodes per the tree invariants.
    unsafe fn recompute_max_end(n: NonNull<Vma>) {
        // SAFETY: forwarded from this function's contract.
        unsafe {
            let v = n.as_ptr();
            let mut m = (*v).range.end();
            if let Some(l) = (*v).link.left {
                let lm = l.as_ref().link.subtree_max_end;
                if lm > m {
                    m = lm;
                }
            }
            if let Some(r) = (*v).link.right {
                let rm = r.as_ref().link.subtree_max_end;
                if rm > m {
                    m = rm;
                }
            }
            (*v).link.subtree_max_end = m;
        }
    }

    /// Walk from `start` up to the root, recomputing each ancestor's
    /// `subtree_max_end`.
    fn update_max_end_up_from(&mut self, start: Option<NonNull<Vma>>) {
        let mut cur = start;
        while let Some(n) = cur {
            // SAFETY: tree-link pointer, live by invariants.
            unsafe {
                Self::recompute_max_end(n);
                cur = n.as_ref().link.parent;
            }
        }
    }

    /// Rotate left at `x`: x's right child y becomes the parent, x
    /// becomes y's left child. The set of nodes in the subtree is
    /// unchanged, so y inherits x's old `subtree_max_end`; x's gets
    /// recomputed because its children changed.
    ///
    /// # Safety
    /// `x` is a live tree node with a non-`None` right child. Every
    /// link the rotation touches (the right child, its left subtree,
    /// `x`'s parent) is therefore live or `None`.
    unsafe fn rotate_left(&mut self, x: NonNull<Vma>) {
        // SAFETY: forwarded from this function's contract.
        unsafe {
            let xp = x.as_ptr();
            let y = (*xp)
                .link
                .right
                .expect("rotate_left precondition: right child");
            let yp = y.as_ptr();

            // y's left subtree becomes x's right subtree.
            (*xp).link.right = (*yp).link.left;
            if let Some(yl) = (*yp).link.left {
                (*yl.as_ptr()).link.parent = Some(x);
            }

            // y takes x's slot in the parent.
            (*yp).link.parent = (*xp).link.parent;
            match (*xp).link.parent {
                None => self.root = Some(y),
                Some(xparent) => {
                    let p = xparent.as_ptr();
                    if (*p).link.left == Some(x) {
                        (*p).link.left = Some(y);
                    } else {
                        (*p).link.right = Some(y);
                    }
                }
            }

            // x becomes y's left child.
            (*yp).link.left = Some(x);
            (*xp).link.parent = Some(y);

            // Augmentation: x first (its children changed), then y.
            Self::recompute_max_end(x);
            Self::recompute_max_end(y);
        }
    }

    /// Rotate right at `y`: mirror of [`rotate_left`].
    ///
    /// # Safety
    /// `y` is a live tree node with a non-`None` left child.
    unsafe fn rotate_right(&mut self, y: NonNull<Vma>) {
        // SAFETY: forwarded from this function's contract.
        unsafe {
            let yp = y.as_ptr();
            let x = (*yp)
                .link
                .left
                .expect("rotate_right precondition: left child");
            let xp = x.as_ptr();

            (*yp).link.left = (*xp).link.right;
            if let Some(xr) = (*xp).link.right {
                (*xr.as_ptr()).link.parent = Some(y);
            }

            (*xp).link.parent = (*yp).link.parent;
            match (*yp).link.parent {
                None => self.root = Some(x),
                Some(yparent) => {
                    let p = yparent.as_ptr();
                    if (*p).link.right == Some(y) {
                        (*p).link.right = Some(x);
                    } else {
                        (*p).link.left = Some(x);
                    }
                }
            }

            (*xp).link.right = Some(y);
            (*yp).link.parent = Some(x);

            Self::recompute_max_end(y);
            Self::recompute_max_end(x);
        }
    }

    /// Restore the no-red-red property after inserting `z` as a red
    /// leaf. CLRS-textbook fixup: while `z`'s parent is red, look at
    /// `z`'s uncle and either recolour (red uncle) or rotate (black
    /// uncle), mirrored for left vs. right parent.
    fn insert_fixup(&mut self, z: NonNull<Vma>) {
        let mut z = z;
        // SAFETY of the loop body: every pointer dereferenced is either
        // `z` itself or reached through `z`'s `parent` / grandparent /
        // uncle chain. Each is a live tree node — `z` because we just
        // inserted it, the rest because they exist as ancestors. The
        // loop condition guarantees `z.parent` is `Some(_)` and red;
        // since the root is always black on entry (invariant), a red
        // parent implies a grandparent exists.
        unsafe {
            while let Some(zparent) = z.as_ref().link.parent {
                if zparent.as_ref().link.color != RbColor::Red {
                    break;
                }
                let zgp = zparent
                    .as_ref()
                    .link
                    .parent
                    .expect("red parent implies grandparent exists");
                let gpp = zgp.as_ptr();
                if (*gpp).link.left == Some(zparent) {
                    let uncle = (*gpp).link.right;
                    if matches!(uncle.map(|u| u.as_ref().link.color), Some(RbColor::Red)) {
                        // Case 1 (left): red uncle — recolour, push up.
                        (*zparent.as_ptr()).link.color = RbColor::Black;
                        (*uncle.unwrap().as_ptr()).link.color = RbColor::Black;
                        (*gpp).link.color = RbColor::Red;
                        z = zgp;
                    } else {
                        if (*zparent.as_ptr()).link.right == Some(z) {
                            // Case 2 (left): rotate parent left, then fall through.
                            z = zparent;
                            self.rotate_left(z);
                        }
                        // Case 3 (left): recolour and rotate grandparent right.
                        let zp_after = z.as_ref().link.parent.unwrap();
                        (*zp_after.as_ptr()).link.color = RbColor::Black;
                        (*gpp).link.color = RbColor::Red;
                        self.rotate_right(zgp);
                    }
                } else {
                    let uncle = (*gpp).link.left;
                    if matches!(uncle.map(|u| u.as_ref().link.color), Some(RbColor::Red)) {
                        // Case 1 (right, mirror).
                        (*zparent.as_ptr()).link.color = RbColor::Black;
                        (*uncle.unwrap().as_ptr()).link.color = RbColor::Black;
                        (*gpp).link.color = RbColor::Red;
                        z = zgp;
                    } else {
                        if (*zparent.as_ptr()).link.left == Some(z) {
                            z = zparent;
                            self.rotate_right(z);
                        }
                        let zp_after = z.as_ref().link.parent.unwrap();
                        (*zp_after.as_ptr()).link.color = RbColor::Black;
                        (*gpp).link.color = RbColor::Red;
                        self.rotate_left(zgp);
                    }
                }
            }
            // Recolour the root black. The fixup may have left a red
            // root in the case-1 recursion path.
            if let Some(root) = self.root {
                (*root.as_ptr()).link.color = RbColor::Black;
            }
        }
    }
}

impl Default for VmaTree {
    fn default() -> Self {
        Self::new()
    }
}

/// Walk left from `start` to the leftmost descendant in its subtree.
/// Returns `start` itself if it has no left child, or `None` if `start`
/// is itself `None`.
fn leftmost_of(start: Option<NonNull<Vma>>) -> Option<NonNull<Vma>> {
    let mut cur = start?;
    // SAFETY: link pointers refer to live tree nodes by invariants.
    unsafe {
        while let Some(l) = cur.as_ref().link.left {
            cur = l;
        }
    }
    Some(cur)
}

/// In-order successor of `node` in the tree it sits in: the next-larger
/// node by `range.start`. Walks the right subtree's leftmost path if
/// `node` has a right child; otherwise climbs via parent pointers until
/// arriving from a left child. Returns `None` if `node` is the
/// rightmost in the tree.
fn successor(node: NonNull<Vma>) -> Option<NonNull<Vma>> {
    // SAFETY: link pointers refer to live tree nodes by invariants.
    unsafe {
        if let Some(r) = node.as_ref().link.right {
            return leftmost_of(Some(r));
        }
        let mut child = node;
        let mut parent = child.as_ref().link.parent;
        while let Some(p) = parent {
            if p.as_ref().link.left == Some(child) {
                return Some(p);
            }
            child = p;
            parent = p.as_ref().link.parent;
        }
        None
    }
}

/// In-order iterator over every `Vma` in a [`VmaTree`], yielded by
/// ascending `range.start`.
pub struct Iter<'a> {
    cur: Option<NonNull<Vma>>,
    _tree: core::marker::PhantomData<&'a VmaTree>,
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a Vma;

    fn next(&mut self) -> Option<&'a Vma> {
        let n = self.cur?;
        self.cur = successor(n);
        // SAFETY: link pointer; live as long as the borrowed tree is.
        Some(unsafe { n.as_ref() })
    }
}

/// In-order iterator over every `Vma` in a [`VmaTree`] that overlaps a
/// query range, yielded by ascending `range.start`. Terminates at the
/// first node whose `range.start >= query.end` — past that point no
/// further overlap is possible (proof on [`VmaTree::iter_overlapping`]).
pub struct OverlapIter<'a> {
    cur: Option<NonNull<Vma>>,
    end: VirtAddr,
    _tree: core::marker::PhantomData<&'a VmaTree>,
}

impl<'a> Iterator for OverlapIter<'a> {
    type Item = &'a Vma;

    fn next(&mut self) -> Option<&'a Vma> {
        let n = self.cur?;
        // SAFETY: link pointer; live as long as the borrowed tree is.
        let v = unsafe { n.as_ref() };
        if v.range.start() >= self.end {
            self.cur = None;
            return None;
        }
        self.cur = successor(n);
        Some(v)
    }
}

impl Drop for VmaTree {
    /// Iterative post-order teardown using parent pointers. Each leaf
    /// is freed and its parent's pointing slot cleared, so the next
    /// iteration finds a new leaf one level up. O(n), no allocation.
    fn drop(&mut self) {
        let mut current = self.root;
        while let Some(n) = current {
            // SAFETY: tree-link pointer to a live, owned node.
            let next = unsafe {
                let np = n.as_ptr();
                if let Some(l) = (*np).link.left {
                    Some(l)
                } else if let Some(r) = (*np).link.right {
                    Some(r)
                } else {
                    // Leaf: clear the parent's slot and free.
                    let parent = (*np).link.parent;
                    if let Some(p) = parent {
                        let pp = p.as_ptr();
                        if (*pp).link.left == Some(n) {
                            (*pp).link.left = None;
                        } else {
                            (*pp).link.right = None;
                        }
                    }
                    // SAFETY: `n` came from `KBox::into_raw` in `insert`,
                    // has not been reconstructed yet, and is no longer
                    // referenced from the tree (we just cleared the
                    // parent slot, and it has no children).
                    drop(KBox::from_raw(n));
                    parent
                }
            };
            current = next;
        }
        self.root = None;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
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
        KBox::try_new(Vma::new(r, Protection::empty(), MappingKind::Anonymous))
            .expect("test heap exhausted")
    }

    fn insert_or_panic(tree: &mut VmaTree, r: VAddrRange) {
        tree.insert(anon_box(r)).expect("test insert must succeed");
    }

    // ----- Invariant checkers -----

    /// Walk the subtree rooted at `node` and verify:
    /// - BST ordering by `range.start` (and consequently no overlaps,
    ///   since we also reject overlapping inserts up front).
    /// - Interval-augmentation invariant: `subtree_max_end` at every
    ///   node equals the true maximum `range.end` over the subtree.
    ///
    /// Returns the actual maximum `range.end` of the subtree, panics
    /// with a descriptive message on any violation.
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
        assert_eq!(
            v.link.subtree_max_end, max_end,
            "augmentation violation at node starting {:?}",
            v.range.start()
        );
        max_end
    }

    /// Walk the subtree rooted at `node` and verify the red-black
    /// properties: no red node has a red parent, every root-to-leaf
    /// path crosses the same number of black nodes. Returns the
    /// black-height (counting black nodes on any path from `node` down
    /// to a `None` leaf, exclusive of `node`).
    fn check_rb(node: Option<NonNull<Vma>>, parent_color: RbColor) -> usize {
        let Some(n) = node else {
            return 0;
        };
        // SAFETY: live link.
        let v = unsafe { n.as_ref() };
        if v.link.color == RbColor::Red {
            assert_ne!(
                parent_color,
                RbColor::Red,
                "red-red violation at node starting {:?}",
                v.range.start()
            );
        }
        let lbh = check_rb(v.link.left, v.link.color);
        let rbh = check_rb(v.link.right, v.link.color);
        assert_eq!(
            lbh, rbh,
            "black-height mismatch at node starting {:?}: left={lbh}, right={rbh}",
            v.range.start()
        );
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

        // Adjacent (touching at PAGE) — half-open, so no overlap.
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

        // Adjacent counts as disjoint under half-open semantics.
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
        // Self-containment.
        assert!(rw_user.contains(rw_user));
        // Empty is contained in everything.
        assert!(rw_user.contains(Protection::empty()));
    }

    // ----- VmaTree -----

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

        // Identical range.
        assert!(tree.insert(anon_box(range(PAGE * 4, PAGE * 8))).is_err());
        // Starts inside.
        assert!(tree.insert(anon_box(range(PAGE * 6, PAGE * 10))).is_err());
        // Ends inside.
        assert!(tree.insert(anon_box(range(PAGE * 2, PAGE * 6))).is_err());
        // Strictly nested inside existing.
        assert!(tree.insert(anon_box(range(PAGE * 5, PAGE * 7))).is_err());
        // Existing nested inside new.
        assert!(tree.insert(anon_box(range(PAGE * 2, PAGE * 10))).is_err());

        // Tree unchanged after every rejection.
        assert_eq!(tree.len(), 1);
        verify(&tree);
    }

    #[test]
    fn adjacent_ranges_do_not_overlap() {
        init_global_heap();
        let mut tree = VmaTree::new();
        // Touching at PAGE*8 — half-open semantics: no overlap.
        insert_or_panic(&mut tree, range(PAGE * 4, PAGE * 8));
        insert_or_panic(&mut tree, range(PAGE * 8, PAGE * 12));
        insert_or_panic(&mut tree, range(0, PAGE * 4));
        assert_eq!(tree.len(), 3);
        verify(&tree);
    }

    #[test]
    fn ascending_inserts_balance_and_keep_invariants() {
        // Pure ascending order is the degenerate input for an unbalanced
        // BST: it builds a right-leaning list. RB fixup must rebalance
        // it. Verify the invariants after every insert.
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
        // Mirror of ascending: pure descending builds a left-leaning
        // list without rebalancing.
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
        // Generate a sequence of non-overlapping ranges, shuffle the
        // insertion order with a fixed-seed LCG (deterministic for
        // reproducibility), and verify after each insert.
        init_global_heap();
        let n: usize = 200;
        let mut order: [usize; 200] = core::array::from_fn(|i| i);

        // Fisher-Yates with the LCG.
        let mut lcg: u64 = 0xDEAD_BEEF_CAFE_F00D;
        for i in (1..n).rev() {
            lcg = lcg
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
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

        // Lookups across the resulting tree.
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
        // Address before, after, and exactly at the exclusive end.
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
        // Insert ascending, then remove ascending. Each remove pulls a
        // leaf-or-near-leaf (after early rebalancing); collectively
        // they exercise root removal once the rest of the tree is gone.
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
        // The torture test: 200 ranges inserted in shuffled order and
        // then removed in a *different* shuffled order, with full
        // invariant verification after every single operation. This is
        // the most-exercised path for the delete fixup's case mix.
        init_global_heap();
        let n: usize = 200;
        let mut insert_order: [usize; 200] = core::array::from_fn(|i| i);
        let mut remove_order: [usize; 200] = core::array::from_fn(|i| i);

        fn shuffle(arr: &mut [usize], mut lcg: u64) {
            for i in (1..arr.len()).rev() {
                lcg = lcg
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
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
            let removed = tree
                .remove_covering(va(start + PAGE))
                .expect("must find every inserted range");
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

    // ----- Iteration -----

    #[test]
    fn iter_on_empty_tree_yields_nothing() {
        let tree = VmaTree::new();
        assert_eq!(tree.iter().count(), 0);
        assert_eq!(
            tree.iter_overlapping(range(0, PAGE * 16)).count(),
            0
        );
    }

    #[test]
    fn iter_yields_every_vma_in_ascending_order() {
        init_global_heap();
        let mut tree = VmaTree::new();
        // Insert in a non-sorted order so the tree isn't trivially linear.
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
        assert_eq!(
            collected,
            [va(0), va(10 * PAGE), va(20 * PAGE), va(30 * PAGE), va(40 * PAGE)]
        );
    }

    #[test]
    fn find_first_overlapping_returns_leftmost_when_multiple_overlap() {
        init_global_heap();
        let mut tree = VmaTree::new();
        // Three non-overlapping VMAs at offsets 10, 20, 30 (each
        // PAGE*2 wide: ends at 12, 22, 32). A query covering 11..21
        // overlaps the 10 VMA (at 11..12) and the 20 VMA (at 20..21);
        // the leftmost must be at 10.
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
        // Before all VMAs.
        assert!(tree.find_first_overlapping(range(0, 8 * PAGE)).is_none());
        // In a gap.
        assert!(
            tree.find_first_overlapping(range(14 * PAGE, 18 * PAGE))
                .is_none()
        );
        // After all VMAs.
        assert!(
            tree.find_first_overlapping(range(30 * PAGE, 40 * PAGE))
                .is_none()
        );
        // Exactly adjacent (half-open): no overlap.
        assert!(
            tree.find_first_overlapping(range(12 * PAGE, 14 * PAGE))
                .is_none()
        );
    }

    #[test]
    fn iter_overlapping_yields_contiguous_run_in_order() {
        init_global_heap();
        let mut tree = VmaTree::new();
        // Five VMAs at starts 0, 10, 20, 30, 40 (PAGE*4 each: ends at
        // 4, 14, 24, 34, 44). A query covering 12..32 overlaps the 10
        // VMA (12..14), all of 20 (20..24), and 30 (30..32) — but not
        // the 0 or 40 VMA.
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
        assert_eq!(
            got,
            [va(10 * PAGE), va(20 * PAGE), va(30 * PAGE)]
        );
    }

    #[test]
    fn iter_overlapping_single_hit_nested_inside_one_vma() {
        init_global_heap();
        let mut tree = VmaTree::new();
        insert_or_panic(&mut tree, range(0, PAGE * 16));
        // A query entirely inside the lone VMA yields just that VMA.
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
        // A query covering the full extent of every VMA should yield
        // the same sequence as `iter()`.
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
        // Remove the middle one.
        let _ = tree.remove_covering(va(30 * PAGE)).expect("present");
        verify(&tree);
        // A query spanning what was 25..45 should now yield only 40
        // (the 30 VMA is gone; 20 ends at 22; 50 starts at 50).
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
        // Indirect verification: drop a large tree, then allocate the
        // same number of fresh Vmas. With a leak, this would gradually
        // exhaust the 16 MiB host heap across the test run. The hard
        // verification of "every box drops exactly once" lives in
        // KBox's own tests; this confirms the tree threads boxes
        // through KBox::from_raw on teardown.
        init_global_heap();
        for _ in 0..4 {
            let mut tree = VmaTree::new();
            for i in 0..256u64 {
                insert_or_panic(&mut tree, range(i * 2 * PAGE, (i * 2 + 1) * PAGE));
            }
            verify(&tree);
            // Tree drops here.
        }
    }
}
