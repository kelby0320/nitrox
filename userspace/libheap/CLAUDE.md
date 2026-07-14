# userspace/libheap/CLAUDE.md

`libheap` workspace constraints. Loaded when Claude Code reads files under `userspace/libheap/`.

## What libheap is

The freeing userspace heap: the `#[global_allocator]` backing `extern crate alloc`
for `no_std` userspace. A segregated size-class allocator over multiple discontiguous
arenas (each a mapped `MemoryObject`) plus a dedicated-mapping large path. It replaced
init's fixed bump arena (slice 4). Full design: `docs/architecture/libheap.md`.

## Build environment

- **`#![no_std]`** for the bare build; `std` under `cargo test` (like `libkern`) so the
  engine is host-tested with a `std`-backed `ArenaSource`.
- **`core` + `libkern` only. No `alloc`** — libheap *provides* alloc's backing, so it
  must not use it. **No external crates** (same rule as the kernel/libkern).
- **Stable Rust only.** `panic = "abort"` (workspace profile); this stays true across a
  future std port, so no unwind-safety obligations.

## Layering

Sits directly on `libkern` (raw syscalls), below `libos`. The top-level binary
registers `libheap::Heap` as its `#[global_allocator]`. Don't reach above libkern.

## The two hard invariants

1. **No self-reentrancy.** Growing the heap (mapping an arena) must not allocate
   through libheap. `ArenaSource::map`/`unmap` impls issue only raw syscalls / stack
   work — never anything that could call back into `alloc`. A reentrant grow while
   holding the engine lock would deadlock.
2. **OOM → null, never panic.** `GlobalAlloc::alloc` returns null on failure (→ the
   caller's `handle_alloc_error`). init is critical-path; a panic here is a kernel
   panic. No `panic!`/`unwrap` on the allocation paths.

## The engine / registration split (keep it)

`HeapEngine<S: ArenaSource>` (the reusable allocator) is deliberately separate from
`Heap` (the thin `GlobalAlloc` newtype) and the `ENGINE` static. This is the std-port
seam: a future `std::sys::alloc` forwards to the same `HeapEngine`. **Don't collapse
the engine into the `GlobalAlloc` impl** — that would re-weld the allocator to the
single global-allocator slot std needs to own.

`ArenaSource` is the other half of the seam: the target `SyscallSource` is
`cfg(not(test))`; a `std::alloc`-backed source under `test` runs the identical engine
logic with no kernel. Keep new allocator logic in the engine (host-testable), not in
the syscall source.

## `unsafe` policy

Allocator internals are `unsafe`-heavy (raw pointers, freelist links in free slots,
the large-alloc header). Every `unsafe` block needs a `// SAFETY:` comment. The engine
is `Sync` via a hand-rolled spinlock over `UnsafeCell<Inner>`; all `Inner` access goes
through `lock()`.

## Testing

Host-tested via `cargo xtask test` (`-p libheap`): the engine against a `std`-backed
`ArenaSource`. Cover alloc/free reuse, size-class boundaries + alignment, multi-arena
growth without overlap, and the large path (map + unmap). Add a test for any new
allocator behavior or bug fix.

## Forbidden patterns

- `Vec`/`Box`/`String`/`alloc` types (libheap backs `alloc`; it can't use it)
- External crates beyond `core` + `libkern`
- Allocating inside `ArenaSource` / the grow path (reentrancy)
- Panicking on OOM or on a corrupt-free (debug-assert, then degrade)
- Collapsing the engine/registration split
