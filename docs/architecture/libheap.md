# libheap — the userspace heap allocator

**Status:** Implemented (Phase 3 slice 4, 2026-07-13). This document is the design
contract for the crate; it is **living** — update it as implementation reveals
subtleties, per the project docs rule. The implementation in `userspace/libheap`
follows this design (size classes 16–2048 B, 64 KiB arenas, engine/registration split,
`ArenaSource` host-test seam).

`libheap` is the userspace **freeing heap**: it provides the `#[global_allocator]`
that backs `extern crate alloc` (`Box`/`Vec`/`String`/`BTreeMap`/`Arc`/…) for every
`no_std` userspace crate. It replaces the fixed-arena **bump** allocator init carries
today (`init::heap::BumpAlloc`, a 64 KiB static arena whose `dealloc` is a no-op and
whose memory is reclaimed only at process exit), which cannot support the churn of a
long-lived service.

## Position in the stack

```
core  ────────────────────────────────────────────────┐
alloc (Box/Vec/String/BTreeMap/Arc)  ── needs ──▶  #[global_allocator]
                                                       │
libheap  ──────────────────────────────────────────────┘   (this crate)
  │  backing memory via
  ▼
libkern (raw syscalls: sys_memory_create / sys_memory_map / sys_memory_unmap)
  │
kernel  (MemoryObject, demand paging)
```

`libheap` is `#![no_std]`, depends only on `core` + `libkern`, and defines **no**
`alloc` dependency of its own — it *provides* alloc's backing. It sits below
libos/libstream (which are `alloc`-using) and is the first userspace-runtime
slice precisely because it is the one library with no dependence on the syscall-ABI
churn ahead (SysCaps, `ThreadArgs`/`SpawnArgs` growth): it consumes only the memory
syscalls, which are solid.

## Design goals

1. **Free and reuse.** Unlike the bump arena, freed memory returns to a freelist and
   is reused; large allocations return their pages to the kernel on free.
2. **Grow on demand.** No fixed arena size baked in; the heap requests more backing
   memory from the kernel as needed.
3. **Std-port-ready (the engine/registration split).** The allocator *engine* is
   decoupled from the `#[global_allocator]` static that registers it, so a future
   `std` port's `std::sys::alloc` can forward to the same engine instead of fighting
   it for the single global-allocator slot. See "The std-port seam" below.
4. **Host-testable.** The freelist/size-class logic is exercised by host unit tests
   through an abstract backing source, with no kernel runtime — matching the project
   rule and init's bump-math precedent.
5. **Allocation-free control paths.** Growing the heap must not itself allocate
   through libheap (no reentrancy); it uses only stack locals + syscalls.

## Backing-memory model: multiple discontiguous arenas

The single most important structural fact: **Nitrox has no `sbrk`/`brk` contiguous
growable heap region.** Backing memory is obtained by creating a `MemoryObject`
(`sys_memory_create(size, MAP_READ|MAP_WRITE)`) and mapping it
(`sys_memory_map`). Each growth yields a **separate, discontiguous** mapped region.

Therefore libheap is a **multi-arena** allocator: it holds a list of *arenas*, each
arena being one mapped `MemoryObject`, and carves allocations out of them. This
differs from a classic Unix allocator that assumes one contiguous region it can
extend with `sbrk`. Consequences:

- Arena metadata (base, length, the `MemoryObject` handle for later unmap) lives in a
  small intrusive structure at the head of each arena (or in a bootstrap arena), so
  the allocator needs no external allocation to track arenas.
- `MemoryObject::MAX_SIZE` caps a single object; a request larger than an arena
  becomes a dedicated large mapping (below), never a contiguous multi-object span.
- Pointer-in-heap tests must consult the arena list, not a single `[base, brk)`
  range.

## Allocator structure

A **segregated free-list (size-class) allocator over arena-backed slabs**, with a
separate large path. This is deliberately the same shape as the kernel's own memory
split — SLUB-inspired **slab over buddy** (`docs/architecture/memory-management.md`)
— re-expressed for userspace: mapped `MemoryObject` arenas are the "page supply"
(buddy's role), and per-size-class freelists carve them (slab's role). We share the
**design**, not the code: the kernel slab is `no_std` + no-alloc and coupled to
`PhysAddr`/buddy internals, while libheap deals in `VirtAddr` mappings + syscalls and
must stay std-portable, so a literal code share would couple the two wrongly.

- **Small allocations** (≤ a threshold, e.g. 2 KiB): rounded up to a size class
  (a small set of classes, power-of-two or a denser tuned set). Each class has a
  freelist of same-sized slots carved from arenas. `alloc` pops the freelist (or
  carves a fresh slot from an arena, mapping a new arena if none has room);
  `dealloc` pushes back onto the class freelist. Arenas are **retained** once mapped
  (see reclamation).
- **Large allocations** (> threshold): rounded up to a page multiple and given their
  **own dedicated `MemoryObject` mapping**. `dealloc` **unmaps** it
  (`sys_memory_unmap`), returning the pages to the kernel — real reclamation for the
  allocations big enough to matter. A dedicated mapping also naturally satisfies
  page-granular alignment.
- **Alignment.** Layout alignment is honored: size classes are aligned to their
  class size; large allocations are page-aligned by construction. Over-aligned small
  requests (align > class) are promoted to the next class that satisfies alignment,
  or to the large path.
- **realloc.** Grow/shrink in place if the current size class still fits; otherwise
  `alloc` new + `copy` + `dealloc` old.

> **v1 chosen over alternatives.** A userspace buddy allocator or a full dlmalloc/
> tcmalloc port were considered and deferred: the size-class-over-arenas design is
> the smallest thing that frees correctly, mirrors machinery we already understand
> (the kernel slab), and is trivially host-testable. Revisit only if profiling shows
> fragmentation or contention that a more sophisticated allocator would fix.

## The std-port seam (engine ↔ registration split)

To keep a future `std` port clean (see `docs/architecture/overview.md` and the
2026-07-13 decision-log entry on deferring std), libheap is two layers:

```rust
// The engine: a reusable, thread-safe allocator. malloc-shaped; no global state
// beyond its own instance. Callable directly OR via GlobalAlloc.
pub struct HeapEngine { /* arena list + size-class freelists, behind a lock */ }
impl HeapEngine {
    pub fn alloc(&self, layout: Layout) -> *mut u8;
    pub unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout);
    pub unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8;
}

// The registration: a thin newtype that forwards to a single process-global engine.
pub struct Heap;
unsafe impl GlobalAlloc for Heap { /* forwards to the global HeapEngine */ }
```

`#![no_std]` userspace registers `#[global_allocator] static A: Heap = Heap;`. When
`std` eventually lands, `std::sys::alloc` for the `nitrox` target forwards to the
same `HeapEngine` (std may own the `#[global_allocator]` slot and route its `System`
allocator through the engine, or we register the engine and std routes to it) — no
rewrite, no two-allocators-fighting-for-the-slot. **This split is the whole reason
libheap is not simply an `impl GlobalAlloc`.**

## Concurrency, reentrancy, initialization

- **Locking.** The engine's arena list + freelists are guarded by a CAS-based
  spinlock. Userspace today is single-threaded per process and Nitrox has **no
  signals** (so no async reentry into an interrupted `malloc`), meaning the lock is
  uncontended now — but it is a *real* lock so that future std OS-threads are correct
  without a redesign.
- **No self-reentrancy.** Growing the heap (create + map a new `MemoryObject`) must
  not allocate through libheap. The grow path uses only stack locals and syscalls;
  arena metadata is stored in-arena. This invariant is load-bearing — a heap-growth
  path that recursed into `alloc` while holding the lock would deadlock.
- **Initialization.** A `#[global_allocator]` static cannot run constructor code, so
  the engine is **lazily initialized on first `alloc`**, guarded by an atomic state:
  the first allocation maps the initial arena. (Alternatively the runtime entry can
  force-init it; lazy-on-first-use is the default so no caller ordering is required.)
  Unlike init's bump arena, there is **no large static BSS buffer** — backing memory
  is mapped `MemoryObject`s.

## Free and reclamation policy (v1 simplifications)

- Small `dealloc` returns the slot to its size-class freelist; the arena stays
  mapped. **Arenas, once mapped, are retained** for the process lifetime (empty-arena
  unmapping is *not* done in v1) — mirroring the kernel kstack's "bump vmap, no
  freelist" pragmatism (`docs/architecture/memory-management.md`); userspace heap
  arena churn is low and a whole-arena reclaimer is a local later addition.
- Large `dealloc` **unmaps** its dedicated `MemoryObject` immediately — the case
  where prompt reclamation matters.
- All backing memory is reclaimed unconditionally at process exit (the kernel tears
  down the address space), so retained arenas are not a leak across processes.

Deferred (record here as we implement): empty-arena unmapping; a large-alloc mapping
cache to avoid map/unmap churn on repeated large alloc/free; per-size-class stats for
a future `/proc`-style heap introspection.

## Host testability

The engine is parameterized over an **arena source** so the allocator logic runs
under `cargo test` with no kernel:

```rust
pub trait ArenaSource {
    fn map(&self, size: usize) -> Option<(*mut u8, ArenaHandle)>;   // create+map
    unsafe fn unmap(&self, ptr: *mut u8, size: usize, h: ArenaHandle);
}
```

- **Target impl:** `sys_memory_create` + `sys_memory_map` / `sys_memory_unmap`.
- **Host-test impl:** `std::alloc`/`mmap`-backed, so `alloc`/`dealloc`/`realloc`,
  size-class boundaries, alignment, fragmentation/reuse, multi-arena growth, and the
  small-vs-large split are all unit-tested host-side. This matches the "host-test
  everything that doesn't need the kernel runtime" rule and init's host-tested
  `bump()` precedent.

## The init/eshell cutover (first consumers)

- **init** drops `#[global_allocator] static ALLOC: BumpAlloc` + its 64 KiB static
  arena (`init/src/heap.rs`, now deleted) and depends on `libheap` instead. init is
  the ideal first consumer: it already exercises `alloc` (parsing `init.toml` into a
  `Vec<MountSpec>` + TOML strings), and it is the critical-path proving ground.
- **eshell needs no allocator** — it is `no_std` *without* `alloc` (fixed buffers), so
  there was nothing to migrate. init is currently libheap's sole consumer.
- **Critical-path constraint.** init forbids `panic!`/`unwrap`
  (`userspace/init/CLAUDE.md`); libheap must therefore surface OOM as a null return
  from `GlobalAlloc` (the standard `alloc` error path → `handle_alloc_error`), not a
  panic, and init keeps handling allocation failure explicitly. The cutover rides
  behind the existing gate: still boots to a live `eshell>` and passes the scripted
  `help`/`lsblk`/`mounts`/`cat` stress, `-smp 1` and `-smp 4`.

## Panic strategy

libheap is written **abort-only** (`panic = "abort"`), and the project intends to
keep `panic = "abort"` even after a future `std` port (a supported std config), so
the lock-holding paths never need retroactive unwind-safety. See the std-port
conflict notes in `overview.md`.

## ABI and dependencies

- **Not part of the kernel ABI version hash.** libheap is pure userspace built on the
  public memory syscalls via `libkern`; it crosses no kernel/module boundary.
- Depends on: `core`, `libkern`. No `alloc` dependency (it backs `alloc`).
- Consumes syscalls: `sys_memory_create`, `sys_memory_map`, `sys_memory_unmap`
  (all Layer-2 solid; `sys_memory_unmap` currently unmaps the whole VMA at an address
  — fine, since libheap unmaps whole dedicated large mappings, never partial ranges).

## References

- `docs/architecture/memory-management.md` — the kernel buddy/slab split libheap's
  design mirrors, and `MemoryObject`.
- `docs/architecture/overview.md` §"Runtime libraries" — the library stack and the
  std-port seam.
- `docs/planning/implementation-plan.md` — Phase 3 slice 4 (this crate) and the
  userspace-runtime band.
- `docs/history/decision-log.md` (2026-07-13) — userspace-runtime sequencing + std
  deferral rationale.
- `userspace/init/src/heap.rs` — the bump allocator this replaces.
