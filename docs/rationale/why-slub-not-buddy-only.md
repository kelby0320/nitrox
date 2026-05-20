# Why a slab allocator, and why SLUB-inspired

A buddy allocator alone is not a kernel-grade memory allocator. The
buddy's smallest block is a 4 KiB page. Every `KBox<u32>`, every linked
list node, every `KArc` header would consume a full page; the kernel
would run out of frames in seconds. Some structure between the buddy
and the rest of the kernel is mandatory.

Three classical answers exist: SLAB (Bonwick 1994), SLOB (Linux's
size-list compactor for tiny systems), and SLUB (Christoph Lameter,
2007, now the Linux default). Nitrox picks SLUB-inspired for the
reasons below.

## What the slab buys us

The slab takes a 4 KiB page from the buddy and subdivides it into many
small objects, all of a single size class. A `kmalloc(64, 8)` then
costs one freelist pop, not one buddy-allocator call. The cost of the
buddy's bookkeeping is paid once per (objs_per_slab) allocations
rather than once per object — a factor of ~63 for the 64-byte bucket.

It also gives kernel code a `core::alloc::GlobalAlloc` impl, which
unlocks `extern crate alloc` and the standard `Box`/`Vec`/`Arc` types.
That's worth a lot of mileage even before SLUB-specific optimisations.

## Why SLUB over SLAB

SLAB carries its book-keeping in dedicated control structures outside
the slab pages. Each cache has per-CPU and per-NUMA-node arrays of
"free object pointers" and "shared object lists." It works, and the
Linux kernel ran on it for over a decade, but the data structures are
intricate and grow non-linearly with CPU count.

SLUB simplifies: the freelist is **embedded in the free objects
themselves**, a single descriptor sits at the start of each slab page,
and lookup from object pointer to descriptor is one mask operation.
The per-CPU fast path becomes a single freelist pointer — no array
arithmetic, no shared lists between CPUs. The code is shorter and
easier to audit.

For Nitrox specifically:

- **Smaller core to audit.** Every `unsafe` block in the slab needs a
  SAFETY comment. SLUB's hot path is ~20 lines; SLAB's is several
  hundred.
- **O(1) free without an external table.** The descriptor-at-byte-0
  trick is a SLUB invention that maps perfectly onto our page-aligned
  buddy output.
- **Per-CPU optimisation is structural, not architectural.** Phase 1
  ships a single global lock per cache. Phase 3 (SMP) adds a per-CPU
  `current_slab` pointer without rewriting the cache state machine.
- **No cross-CPU object migration in the hot path.** SLAB shuttles
  objects between per-CPU arrays and a shared list; SLUB's slow path
  takes the cache lock once and walks the partial list, which is
  conceptually simpler.

## Why not SLOB

SLOB is a single freelist of variable-sized chunks. It's tiny — ~600
lines of C in Linux — and is sensible for very small embedded systems
where a few KB of memory matter. It pays for that simplicity in
allocation latency (`O(free-chunks)` worst case) and external
fragmentation.

Nitrox is not memory-constrained at this scale. We will run with
hundreds of MiB to tens of GiB. The SLUB structural cost is paid in
predictable allocation latency and density; we get both.

## Why a single global lock per cache today

The plan agent and the user both considered per-CPU slab caches up
front. They were rejected for Phase 1 for three reasons:

1. **No SMP yet.** Phase 1 has a single CPU and interrupts disabled.
   The cost of a `compare_exchange_weak` on an uncontended `AtomicBool`
   is one or two cycles. There is no contention to optimise away.
2. **No per-CPU infrastructure exists.** GS-base, per-CPU areas, and
   the per-CPU `current_cpu` accessor all land later in Phase 1
   (scheduler) or Phase 3 (SMP). Bolting per-CPU caching onto the slab
   before that infrastructure is upside-down.
3. **The structural cost is small.** SLUB's per-CPU optimisation slots
   into the same alloc/free state machine as the single-lock version.
   A `current_slab` pointer per CPU, a fallback to the partial list
   under the cache lock — the existing code becomes the slow path,
   unchanged.

The cache lock acquires across a `buddy_alloc` call during slab `grow`.
See [kernel/docs/lock-ordering.md](../../kernel/docs/lock-ordering.md)
for why that is safe and documented as the only allocator-to-allocator
nesting permitted.

## Rejected alternatives

- **Object-pool-per-type only (no untyped `kmalloc`).** The Linux
  kernel's `kmem_cache_create` API style. Cleaner per-type lifetimes,
  but every type needs an explicit pool declaration. Nitrox callers
  expecting `Box::new(x)` to "just work" via the `GlobalAlloc` impl
  need a generic untyped backing layer. Object pools can sit on top of
  the same `SlabCache` later (a typed `KCache<T>` wrapper) — they
  don't need to be the only entry point.
- **Direct `core::alloc::GlobalAlloc` over the buddy with a header
  table.** Considered briefly. Every allocation would need a separate
  metadata record tracking its size and origin page. SLUB's
  descriptor-at-byte-0 trick gets this for free. The header-table
  variant is the same complexity for worse density.

Filed under deferred decisions: empty-slab reclaim, per-CPU caching,
DMA / Normal zone split. See `docs/rationale/deferred-decisions.md`.
