# Memory management

Three layers, each owning a single concern:

| Layer  | Code                                  | Owns                                   |
|--------|---------------------------------------|----------------------------------------|
| Buddy  | `kernel/src/mm/buddy.rs`              | Physical page frames                   |
| Slab   | `kernel/src/mm/slab.rs`               | Sub-page kernel-object allocation      |
| VMM    | `kernel/src/mm/vmm.rs` (not yet)      | Per-process address spaces, VMAs       |

The boot facade in `kernel/src/mm/heap.rs` glues layers 1 and 2: it owns
the single global `BuddyAllocator` and exposes `buddy_alloc` /
`buddy_free` / `hhdm_offset` for callers above it. The slab calls into
this facade through a small `BuddyPager` trait so tests can inject a
local buddy without touching the global statics.

Phase 1 implements layers 1 and 2; layer 3 lands in the next slice
alongside paging.

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
