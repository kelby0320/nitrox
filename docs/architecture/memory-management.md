# Memory management

Three layers, each owning a single concern:

| Layer  | Code                                  | Owns                                   |
|--------|---------------------------------------|----------------------------------------|
| Buddy  | `kernel/src/mm/buddy.rs`              | Physical page frames                   |
| Slab   | `kernel/src/mm/slab.rs`               | Sub-page kernel-object allocation      |
| VMM    | `kernel/src/mm/vmm.rs`                | Per-process address spaces, VMAs       |

The boot facade in `kernel/src/mm/heap.rs` glues layers 1 and 2: it owns
the single global `BuddyAllocator` and exposes `buddy_alloc` /
`buddy_free` / `hhdm_offset` for callers above it. The slab calls into
this facade through a small `BuddyPager` trait so tests can inject a
local buddy without touching the global statics.

Phase 1 implements layers 1 and 2 in full. Layer 3 — the VMM — is being
built up incrementally. In today: the arch-level page-table primitive
(see [Arch paging layer](#arch-paging-layer)) and the [VMA tree](#vma-tree)
that stores per-address-space mappings. Still ahead: the `AddressSpace`
owner that pairs the VMA tree with a page-table root under one lock, the
page-table integration that turns VMA mutations into PTE installs and
removals, and the `mprotect`-style split / merge operations.

## Buddy allocator

Single allocator over every Limine `Usable` region above 1 MiB. The
boot path skips frames below 1 MiB wholesale — legacy DMA, the BIOS
data area, and the AP bring-up trampoline live there.

- **Block size:** `PAGE_SIZE << order`, where `order ∈ 0..=MAX_ORDER`.
  `MAX_ORDER = 10`, giving 4 KiB to 4 MiB blocks.
- **Free lists:** per-order, intrusive — the first 8 bytes of each free
  frame hold the next-free pointer, accessed through the higher-half
  direct map.
- **Coalescing:** a bitmap carved out of physical memory at init time
  tracks buddy-pair occupancy. XOR-on-each-free naturally merges
  adjacent halves into the next order up.
- **Alignment:** the bit-index formula assumes `base_frame` aligns with
  the natural buddy-pair structure at every order, so `base_frame` is
  rounded down to a `2^(MAX_ORDER+1)`-frame boundary. The frames below
  the real usable range become "phantoms" with bitmap bits but never
  enter the free lists — at most ~2047 frames (8 MiB) of bookkeeping
  overhead, paid once.

Out of scope today: DMA / Normal zone split; per-NUMA arenas; reservation
of contiguous-DMA pools. Each is filed in `docs/rationale/deferred-decisions.md`.

## Slab allocator

SLUB-inspired single-lock-per-cache slab. See [why-slub-not-buddy-only](../rationale/why-slub-not-buddy-only.md)
for the rationale and rejected alternatives.

- **Slab geometry:** every slab is exactly one 4 KiB page from the
  buddy allocator. The `SlabDescriptor` header sits at byte 0; objects
  occupy `[obj_offset, SLAB_SIZE)`, where `obj_offset` is the header
  size rounded up to the cache's alignment.
- **Embedded freelist:** free slots store the next-free pointer in
  their own first 8 bytes. The cache's `partial` list head points to
  the first slab with free slots; allocation pops from that slab's
  freelist; freeing pushes back to the head (LIFO).
- **O(1) free:** given any object pointer `p`, the descriptor is at
  `(p as usize) & SLAB_MASK`. The `owner` field of the descriptor
  identifies the cache; `kfree` derefs it and calls `cache.free(ptr)`.
- **Two intrusive lists per cache:** `partial` (slabs with at least
  one free slot) and `full` (no free slots). A slab moves between
  lists when its first slot is taken (partial → full) or its last
  slot is returned (full → partial).
- **kmalloc size buckets:** seven caches at 32, 64, 128, 256, 512,
  1024, 2048 bytes. `kmalloc(size, align)` rounds `size.max(align)`
  up to the smallest bucket that fits.

### Objs per slab (32-byte `SlabDescriptor`)

| Bucket | `obj_offset` | `objs_per_slab` |
|--------|--------------|-----------------|
| 32     | 32           | 127             |
| 64     | 64           | 63              |
| 128    | 128          | 31              |
| 256    | 256          | 15              |
| 512    | 512          | 7               |
| 1024   | 1024         | 3               |
| 2048   | 2048         | 1               |

If `SlabDescriptor` grows past 32 bytes the 32-byte bucket loses one
slot. The `init` assertion catches `objs_per_slab < 1`.

### Large allocations

Requests larger than 2048 bytes bypass the size buckets and route
directly to the buddy via `large_alloc`. The trick: a stub
`SlabDescriptor` still lives at byte 0 of the returned buddy block,
but its `owner` field is `null` — that's the sentinel that tells
`kfree` to route the free through `large_free` rather than into a
size-class cache.

- The stub descriptor's `obj_size` stores the full buddy block size in
  bytes; `large_free` recovers the buddy order via
  `BuddyAllocator::order_for_size(obj_size)`.
- The user pointer sits at offset `align_up(sizeof::<SlabDescriptor>(),
  align)` past the header. As long as `align ≤ SLAB_SIZE`, the user
  pointer remains within the first page of the block, so the
  `ptr & SLAB_MASK` recovery trick still lands on the descriptor.
- Alignments greater than `SLAB_SIZE` are rejected by `kmalloc`
  (returns null). DMA buffers in Phase 2 will need a real answer.

## Heap facade

The single owner of the buddy allocator. Three responsibilities:

1. Hold the static `SpinLock<Option<BuddyAllocator>>` and the
   `AtomicU64` HHDM offset, both populated by `init_buddy` during boot.
2. Expose `buddy_alloc` / `buddy_free` / `hhdm_offset` for everyone
   else.
3. Provide the `BuddyPager` trait so the slab can be tested against a
   local buddy in `#[cfg(test)]` builds without touching the production
   statics.

## Initialisation order

1. Limine writes responses into our request statics, then jumps to `_start`.
2. `kernel_main` checks the base-revision marker.
3. `init_memory` reads `MEMMAP_REQUEST.response` and `HHDM_REQUEST.response`,
   then calls `heap::init_buddy(memmap, hhdm_offset)` followed by `slab::slab_init()`.
4. From here on `kmalloc` / `kfree` work, and with them the fallible
   `libkern` containers (`KBox`, `KVec`, `KString`).

Calling `kmalloc` / `kfree` before `slab_init` panics with a clear
message — there is no silent "not ready" mode.

The kernel registers no `#[global_allocator]` and does not use the
`alloc` crate: every `alloc` type aborts on allocation failure, which
the kernel cannot tolerate. `KBox` / `KVec` / `KString` call `kmalloc` /
`kfree` directly and surface exhaustion as `AllocError`. See the
decision log entry of 2026-05-20.

## Arch paging layer

The VMM (layer 3) does not touch hardware page tables directly — it goes
through `ArchPaging`, the kernel's first cross-architecture trait
(`kernel/src/arch/paging.rs`). The trait abstracts the operations whose
implementation genuinely differs between x86_64 and aarch64; the active
architecture's implementation is re-exported as `arch::Paging`.

- **Surface:** `map_page`, `unmap_page`, `flush_tlb_page`,
  `flush_tlb_all`, `set_page_table` — all `unsafe`. `map_page` /
  `unmap_page` install and remove a 4 KiB leaf and allocate intermediate
  tables from the buddy on demand; neither flushes the TLB, so the
  caller batches one flush over many changes. `unmap_page` returns the
  freed `PhysAddr` for the VMM to reclaim.
- **Permissions:** the arch-neutral `PageFlags` (writable, user,
  no-execute, global, cache attributes) is translated to page-table-entry
  bits by each architecture's implementation.
- **x86_64:** `kernel/src/arch/x86_64/paging.rs` — 4-level (48-bit)
  paging, 4 KiB leaves only. `translate` additionally understands 2 MiB
  and 1 GiB pages so it is correct against the bootloader's live tables.
  The kernel enables `EFER.NXE` at boot so `NO_EXECUTE` is usable.

Page tables are reached through the higher-half direct map: a table at
physical `p` is addressed at `p + hhdm_offset()`. Out of scope today:
reclaiming intermediate tables on unmap, range TLB flush, and cross-CPU
shootdown — all filed in `docs/rationale/deferred-decisions.md`.

## VMA tree

[`mm/vmm.rs`](../../kernel/src/mm/vmm.rs) holds the leaf data types
(`VAddrRange`, `Protection`, `MappingKind`, `Vma`) and `VmaTree` — an
interval-augmented intrusive red-black tree of `Vma`s, keyed on
`range.start`.

- **Intrusive linkage.** The `RbLink` (parent / left / right / colour
  / `subtree_max_end`) lives as a private field on `Vma`. The tree
  owns the boxed VMAs through `KBox<Vma>`: insert takes a box, remove
  returns one. Slab-backed allocation matches Linux's `vm_area_cachep`
  — every VMA goes through `kmalloc`, no per-address-space arena. The
  arena alternative was considered and rejected; see the decision log
  entry of 2026-05-27.
- **Interval augmentation.** Each node carries `subtree_max_end`, the
  maximum `range.end` over its subtree. It is maintained on every
  structural mutation (insert, remove, rotation). Today's queries are
  already O(log n) without consuming it; the augmentation is in place
  for future disjoint-range stabbing queries that need subtree
  pruning to skip whole branches.
- **`Protection` is narrower than `arch::paging::PageFlags`.** A VMA
  carries WRITE / EXEC / USER only; `GLOBAL` and cache-attribute bits
  are per-PTE policy decided at install time, not a property of the
  address range. The VMM will translate `Protection` into `PageFlags`
  when it actually populates a leaf. `Protection::empty()` is the safe
  default (kernel-only, read-only, non-executable), the inverse of
  `PageFlags::empty()`'s hardware-friendly default.
- **Queries.** `find_covering(addr)` for point lookup, `iter()` for
  full in-order traversal, `find_first_overlapping(range)` for the
  leftmost overlapping VMA, and `iter_overlapping(range)` for the
  contiguous overlap run. Iterators advance through parent pointers
  for the in-order successor — no allocation, no recursion.
- **Operations.** `insert` rejects any overlap with an existing VMA
  and returns the box back to the caller. `remove_covering(addr)`
  unlinks the VMA containing `addr` and returns the box. Both
  fixups follow CLRS, iterative throughout (parent pointers make
  recursion unnecessary; see the 2026-05-27 deviation note).
- **Send / Sync.** `Vma` is non-`Send` / non-`Sync` because the link
  field holds `NonNull` pointers. That's intentional: a `Vma` in a
  tree is bound to its address-space lock. The `AddressSpace` will
  carry the `unsafe impl Send` when it lands, with a SAFETY comment
  pointing at the lock.

## AddressSpace

[`mm/addr_space.rs`](../../kernel/src/mm/addr_space.rs) pairs the VMA
tree with a top-level page-table root under a single
`SpinLock<Inner>` (rank 4 per
[kernel/docs/lock-ordering.md](../../kernel/docs/lock-ordering.md)).
It is the bridge between the VMM's bookkeeping (the tree) and the
hardware MMU's actual translations (the page tables): `map_vma`
updates both atomically; `unmap_covering` does the inverse.

- **Construction.** `new()` allocates a fresh 4 KiB PML4 frame from
  the buddy, zeros every entry, then calls
  `ArchPaging::inherit_kernel_mappings` to populate the kernel half
  from a boot-captured template (see
  [Kernel-half PML4 sharing](#kernel-half-pml4-sharing) below). The
  resulting AS is loadable: switching `CR3` to its root preserves
  every higher-half mapping the running kernel relies on (its own
  code, the kernel stack, the HHDM, the framebuffer).
- **`map_vma(KBox<Vma>)`.** Validates the range (canonical, within
  `[0, USER_VIRT_END)`), pre-checks tree overlap, then walks the
  range one page at a time: allocate a frame, zero it through the
  HHDM, install the PTE via `ArchPaging::map_page`. On failure
  mid-walk, the partial install is rolled back (every installed PTE
  uninstalled, every allocated frame freed) and the box returned to
  the caller with a `MapError`. The whole sequence runs under the AS
  lock so the tree and the page tables can never disagree about
  what's mapped.
- **Eager anonymous allocation.** One frame per page is allocated at
  `map_vma` time; on-demand allocation via the page-fault handler
  would be the real-OS pattern but needs an upgraded `#PF` handler
  (today's is dump-and-halt). The switch lands when the PF handler
  does.
- **Frame ownership lives in the page tables.** No per-VMA list of
  owned physical frames: `ArchPaging::unmap_page` returns the
  `PhysAddr` it freed, which `unmap_covering` and `Drop` hand straight
  to `buddy_free`. For `MappingKind::Anonymous` the frame is freed;
  the dispatch will branch on backing kind when `FileBacked` /
  `Device` arrive (file-backed pages belong to the page cache, device
  pages were never allocated by the kernel).
- **`Drop` drains the tree leftmost-first**, uninstalls every PTE,
  frees every leaf frame, then frees the PML4. Intermediate
  page-table frames (PDPT / PD / PT) leak per the deferred decision
  documented for `ArchPaging::unmap_page` — for a single Phase 1 AS
  the cost is negligible.
- **No TLB flushing.** No `AddressSpace` is ever loaded onto a CPU
  today; the TLB never holds entries for the new mappings. When the
  scheduler arrives it will own the `set_active` entry point and
  the flush policy that comes with it.

## Kernel-half PML4 sharing

Every process address space shares the boot kernel's higher-half
mappings — kernel code, kernel stack, HHDM, framebuffer, future
kernel-vmap allocations. Without that sharing, switching `CR3` to a
process AS would unmap the executing kernel and triple-fault.

The mechanism is template-and-copy:

1. **Boot capture.** At boot,
   [`arch::init_kernel_template`](../../kernel/src/arch/x86_64/paging.rs)
   reads Limine's live PML4 (via `arch::active_root`) and copies
   entries 256..512 — the canonical kernel-half — into a static
   `SpinLock<Option<[u64; 256]>>`. Must run after `init_memory` (the
   HHDM is required to reach the PML4 frame) and before any
   `AddressSpace::new`.
2. **AS inheritance.** `AddressSpace::new` allocates a fresh PML4,
   zeros it, then calls
   `ArchPaging::inherit_kernel_mappings(root)`. On x86_64 this
   copies the template's 256 entries into the new PML4's kernel
   half. The user half (entries 0..256) stays zero.
3. **Shared intermediate tables.** The copied entries are PML4
   slots, which point at PDPTs — and through those at PDs and
   PTs. All address spaces hold the *same* `PhysAddr` values in
   their kernel-half PML4 slots, so they reach the *same*
   intermediate tables. Modifications at the leaf level (a future
   kernel-vmap allocation, for instance) become visible to every
   AS without a cross-AS update step or a TLB shootdown of any
   kind beyond the local invalidation.

The rule that follows: **PML4 entries for the kernel half are
immutable post-boot**. The intermediate tables they point at can
grow leaves freely; the top-level entries cannot change without
visiting every AS to update its template-copy. Linux preallocates
intermediate tables for the kernel-vmap region for the same
reason; we will do the same when the vmap allocator lands (the
next sub-item).

On a future aarch64 port, `inherit_kernel_mappings` is a no-op:
the TTBR0/TTBR1 split makes the kernel half live in a separate
translation register that process address spaces don't manage.
The arch-neutral caller (`AddressSpace::new`) is unchanged.

## ELF loader

[`mm/elf.rs`](../../kernel/src/mm/elf.rs) populates a fresh
`AddressSpace` from a static ELF64 binary.
`load_elf(asp, bytes) -> Result<EntryInfo, ElfLoadError>` parses the
header (hand-rolled `repr(C)`-free reader; no external crates),
walks the program headers, and for each `PT_LOAD` segment allocates
a page-aligned VMA covering `[align_down(p_vaddr), align_up(p_vaddr
+ p_memsz))`, then copies file bytes `p_offset..p_offset + p_filesz`
into the newly-allocated frames via the HHDM. BSS (the `p_memsz -
p_filesz` tail) comes for free from `map_vma`'s zero-init step.
After segments, an initial 4-page stack VMA is installed at the
host architecture's `arch::abi::DEFAULT_USER_STACK_TOP` (on x86_64,
`0x7FFF_FFFF_0000`); the returned [`EntryInfo`] carries the entry
point and stack top for whatever launches the user thread later.

The loader itself is architecture-neutral: it lives outside
`kernel/src/arch/` and consults [`arch::abi`](../../kernel/src/arch/x86_64/abi.rs)
for the three values that vary across machines — the expected
`e_machine`, the user-half upper bound, and the default stack
placement. An aarch64 port supplies its own `abi.rs`; this file is
unchanged.

- **Static binaries only.** `ET_DYN` is rejected pending PIE
  base-address randomization. `PT_INTERP` is rejected: dynamic
  linking is a userspace concern matching the universal
  kernel/userspace boundary used by Linux (`binfmt_elf` →
  `ld.so`), Windows (kernel loader → NTDLL), and macOS (kernel
  Mach-O → `dyld`). A future Nitrox `ld.so` equivalent will live
  in userspace and use normal syscalls to map shared libraries.
- **`p_vaddr % PAGE == p_offset % PAGE`** is enforced (the ELF spec
  requires it); without it the file bytes couldn't be laid into the
  VMA contiguously through HHDM.
- **Protection mapping.** `PF_X` → `Protection::EXEC`, `PF_W` →
  `Protection::WRITE`. Every loaded segment gets `Protection::USER`.
  `PF_R` is implicit: every architecture Nitrox targets makes a
  present mapping readable.
- **No argv / envp / auxv on the stack.** Nitrox passes argv / env
  as typed structural values; the handoff format belongs to "first
  userspace process" where the userspace runtime defines it. The
  stack VMA today is just 16 KiB of writable, zero-initialised
  memory at a known address.
- **No partial-load rollback.** A segment failure mid-load leaves
  the address space in a partial state; the caller drops it,
  which `AddressSpace::Drop` cleans up.

## Locking

Both allocator locks sit at rank 6 (see [kernel/docs/lock-ordering.md](../../kernel/docs/lock-ordering.md)):

- 6a — `SlabCache`'s `state: SpinLock<SlabCacheState>` (one per cache)
- 6b — `BUDDY: SpinLock<Option<BuddyAllocator>>`

Slab `grow_locked` holds 6a while calling `buddy_alloc` (rank 6b).
That nesting is allowed because the buddy never recurses into the slab.
The opposite direction (taking a slab cache lock while holding the
buddy lock) is forbidden and would be caught at design review.

The current `SpinLock` (`kernel/src/libkern/spinlock.rs`) is a plain
test-and-set primitive with no IRQ masking. Phase 1 runs with
interrupts disabled, so the lock is sound today. The IDT slice will
introduce an `IrqSpinLock` variant for locks that must mask interrupts;
allocator locks are likely candidates.

## Phase 1 limitations

- No per-CPU caching in the slab. The single global lock per cache is
  cheap on a single CPU and will be the natural target of the per-CPU
  optimisation when SMP arrives.
- No empty-slab reclamation. Once a slab is grown, the cache holds
  onto it.
- No alignment greater than `SLAB_SIZE`.
- No DMA / Normal zone split in the buddy (see TODO in `buddy.rs`).
- No allocator-rank-checker in debug builds (still on the to-do list
  in CLAUDE.md).
- No TLB flushing on map / unmap. No AS is "active" today, so the
  TLB doesn't cache its entries. The scheduler will own flushing
  once `AddressSpace::set_active` exists.
- No kernel-vmap allocator yet, and no preallocation of
  intermediate page tables for the kernel-vmap region. The
  kernel-half template currently inherits whatever Limine left in
  the higher half; future kernel-half allocations (per-thread
  kernel stacks, per-CPU data, driver MMIO) need a vmap allocator
  plus the preallocated PDPTs/PDs that make leaf modifications
  visible to every AS. Lands with the next sub-item (per-thread
  kernel stack with guard page).
- ELF loader handles **static** binaries only: `ET_DYN` (PIE) and
  `PT_INTERP` (dynamic linking) are rejected. PIE needs base
  randomization; dynamic linking needs a userspace `ld.so`
  equivalent. Both arrive later.
- ELF loader does **not** set up argv / envp / auxv on the stack
  yet — the handoff format is defined by the userspace runtime and
  belongs to the "first userspace process" milestone.
