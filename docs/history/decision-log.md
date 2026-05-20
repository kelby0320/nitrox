# Decision Log

A chronological record of project decisions that aren't captured in source
code or commit messages. Each entry includes the date, the decision, and the
reasoning behind it. Entries are append-only — supersession is recorded as a
new entry that points back to the previous one.

For the broader rationale documents that explain *why* the architecture is
shaped the way it is, see `docs/rationale/`. The decision log is for
incremental, dated choices made during implementation; the rationale
documents are for the standing positions those choices reinforce.

---

## 2026-05-13 — Phase 0 (foundation boot) complete

The kernel boots end-to-end on QEMU+OVMF and renders a recognisable text
indicator on the framebuffer. This proves the toolchain, the Limine
integration, and the higher-half loading work as designed.

What's in the build:

- `kernel/` — `#![no_std]`, `#![no_main]`, built against the built-in
  `x86_64-unknown-none` rustc target. Single bin `nitrox-kernel`.
- `kernel/src/limine.rs` — hand-rolled `#[repr(C)]` Limine boot-protocol
  bindings, pinned to base revision 6 (matching limine-protocol trunk as of
  Limine 12.2.0). No external Rust crates.
- `kernel/src/framebuffer.rs` + `kernel/src/font.rs` — minimal framebuffer
  console with an inline 8x16 bitmap font (uppercase letters and digits
  only — Phase 0 doesn't need more).
- `tools/xtask` — host build orchestrator: pulls the pinned
  `limine-binary.tar.gz` into `tools/build-cache/`, builds the kernel,
  assembles a 64 MiB GPT/FAT32 UEFI image via `sgdisk` + `mtools`, runs
  QEMU with OVMF. Subcommands: `build`, `image`, `qemu`, `qemu-debug`,
  `fetch-limine`, `clean`.

Decisions worth recording:

- **Built-in `x86_64-unknown-none` over a custom JSON target.** The
  built-in target already implies soft-float, no MMX/SSE, no red zone —
  exactly the semantics the kernel CLAUDE.md describes. Using it keeps us
  on stable Rust without `-Z build-std`, which is nightly-only. We can
  switch to a custom JSON later if we need a feature the built-in doesn't
  expose. (Supersedes the literal "x86_64-unknown-none.json" wording in
  `kernel/CLAUDE.md` — that file should be updated in a doc-sync pass.)
- **No NASM stub in Phase 0.** Limine drops the kernel into long mode
  with paging, a GDT, and a 64 KiB stack already set up; calling into a
  `pub extern "C" fn _start()` is sufficient. The architecture overview's
  mention of a "NASM boot stub" remains accurate for the context-switch
  path, which is a Phase 1+ concern.
- **Limine fetched on first build, not vendored.** xtask pulls the
  pinned `v12.2.0` release tarball into `tools/build-cache/limine/` (in
  `.gitignore`) the first time it's needed. Keeps the repo binary-clean;
  reproducibility is anchored by the `LIMINE_VERSION` constant in
  `tools/xtask/src/main.rs`.
- **Raw GPT+FAT32 image via `sgdisk`/`mtools` rather than xorriso/ISO.**
  Matches the Limine C template's UEFI image path and the project's
  UEFI-only target. ISO support can be added later if BIOS bring-up
  becomes interesting.
- **Top-level `cargo xtask` alias.** `/.cargo/config.toml` aliases
  `xtask` to `run --manifest-path tools/Cargo.toml -p xtask --`. The
  kernel and tools workspaces are intentionally separate Cargo workspaces
  because they target different triples.
- **Directory renamed `xtools/` → `tools/`** to match the layout
  declared in the project CLAUDE.md.

Verification: a headless QEMU launch with a monitor-driven `screendump`
captured a framebuffer whose colour histogram exactly matches our
declared `Rgb::BG`, `Rgb::FG`, and `Rgb::ACCENT` palette, with the FG
and ACCENT pixel counts consistent with the rendered "NITROX KERNEL" and
"PHASE 0: BOOT OK" strings.

What's deferred to Phase 1+:

- IDT/IST setup, exception handling, double-fault stack
- Physical-memory bookkeeping (parsing Limine's memory map, buddy
  allocator, slab)
- Virtual-memory manager and the HHDM/kernel-half ranges
- IPC, notifications, scheduler, handle table
- Userspace (libkern, init)
- Logging surface beyond the framebuffer (kernel log handle, serial)
- Custom JSON target if we need anything the built-in doesn't expose
- Replacement of the hand-coded font with a PSF loader once an allocator
  exists

---

## 2026-05-19 — Phase 0 CI and host-test scope

Added GitHub Actions CI plus an opening set of host-side unit tests
under `tools/xtask`. Scope was deliberately narrowed after auditing the
Phase 0 codebase for what is actually testable today.

What's in the build:

- `.github/workflows/ci.yml` — runs on pushes and PRs against `main`.
  One job: `rustup` stable + `rustup target add x86_64-unknown-none`,
  cached cargo registry/build outputs, then `cargo xtask build`
  followed by `cargo test --manifest-path tools/Cargo.toml`.
- `tools/xtask/src/main.rs` `#[cfg(test)]` module — covers
  `walk_for`, the three branches of `find_bootx64` (known location,
  recursive fallback, error path), and `format_cmd`. Uses a small
  `TmpDir` RAII helper rather than pulling in `tempfile`, to keep
  xtask's "no external crates" stance.

Decisions worth recording:

- **No `xtask test` subcommand yet.** The convention from
  `kernel/CLAUDE.md` is that `cargo xtask test` runs host-side unit
  tests for the OS we are building (kernel + userspace where the code
  can run on the host). In Phase 0 there is no such code — the kernel
  is entirely bare-metal, and `userspace/init` and `userspace/libkern`
  are `cargo new` placeholders. Adding a stub subcommand that runs
  nothing is ceremony; Phase 1 adds the real subcommand once there is
  something to run. xtask's own tests are invoked via plain
  `cargo test --manifest-path tools/Cargo.toml` from CI, treating
  xtask as a normal host crate rather than as part of the OS-under-test.
- **CI builds, but does not assemble images or run QEMU.** Build
  catches the regressions worth catching today: kernel target spec,
  `code-model=kernel` rustflags, linker script wiring, `build.rs` path
  emission. Image assembly (sgdisk + mtools) and the eventual QEMU
  smoke test are deferred to Phase 1 CI, by which point there will be
  something past `kernel_main` for a smoke test to actually assert on.
- **Kernel host unit tests deferred to Phase 1.** The Phase 0 kernel's
  testable surface is roughly thirty lines of arithmetic helpers
  (`pick_scale`, `text_width`, `text_height`, the `Rgb::pack` shifts)
  that will be replaced when the PSF loader and a real console land on
  top of an allocator. Splitting the kernel crate into `lib + bin`
  with conditional `#![no_main]` to expose those helpers to host
  `cargo test` is real surgery for negative net value at this scale.
- **Userspace placeholder test left alone.** `userspace/libkern`'s
  `pub fn add` + `it_works` test is the `cargo new` boilerplate. It
  is not run by CI (CI only invokes the `tools/` workspace) and will
  be deleted wholesale when libkern is rewritten in Phase 1.

---

## 2026-05-19 — Phase 1, slice 1: physical-memory buddy allocator

First Phase 1 slice. Implements `kernel/src/mm/buddy.rs`: a single
`BuddyAllocator` covering every Limine `Usable` region above 1 MiB,
backed by intrusive free-list pointers stored in each free frame's first
8 bytes and a coalesce bitmap carved out of physical memory at init.
Orders 0..=10 (4 KiB to 4 MiB blocks). Host-tested. Boot integration
deferred to the next slice (slab + heap), so the new memmap/HHDM Limine
bindings are present but their request statics are not yet declared in
`main.rs`.

What's in the build:

- `kernel/src/mm/{mod.rs,buddy.rs}` — `PhysAddr`, `PAGE_SIZE`/`PAGE_SHIFT`,
  the buddy allocator and its host-side `#[cfg(test)]` suite.
- `kernel/src/limine.rs` — extended with `MemoryMapRequest`/`Response`
  /`Entry` (with `MEMMAP_USABLE` and the other kind constants) and
  `HhdmRequest`/`Response`.
- `kernel/src/lib.rs` + `[lib]` in `kernel/Cargo.toml` — the kernel
  crate now exposes a lib alongside the `nitrox-kernel` bin so
  `cargo test` can compile kernel modules against the host's std.
  `main.rs` lost its `mod` declarations and imports from
  `nitrox_kernel::*`.
- `kernel/build.rs` — only emits the freestanding linker script when
  building for `x86_64-unknown-none`; host test binaries take the
  std-linked path.
- `tools/xtask` — new `test` subcommand runs the tools workspace's
  tests followed by `cargo test --lib --target $host` inside `kernel/`
  to override the kernel's pinned target. Adds a `host_triple` helper
  that parses `rustc -vV` (tested host-side alongside the existing
  helpers).
- `.github/workflows/ci.yml` — single test step is now
  `cargo xtask test` (replacing the previous tools-workspace-only run).

Decisions worth recording:

- **Kernel split into `lib + bin`.** Supersedes the 2026-05-19 Phase 0
  decision to defer this. The buddy allocator has substantive host-
  testable algorithmic content (coalesce bitmap math, free-list
  splicing, split/coalesce paths), enough that the cost-benefit
  trade-off inverts. Future testable kernel code (handle table,
  namespace resolution, ABI codecs) inherits the same infrastructure.
  The lib uses `#![cfg_attr(not(test), no_std)]`; the bin keeps
  `#![no_std]` + `#![no_main]` and imports modules from the lib.
- **Single flat allocator, not zoned.** The architecture overview's
  DMA / Normal zone split is staged for later; a `// TODO: zone split`
  comment in `buddy.rs` marks the insertion point. ISA-DMA-bound
  allocations will need that, but Phase 1 has no ISA-DMA consumers.
- **Skip the first 1 MiB of physical memory.** The bitmap and the
  free-frame walk both refuse frames below `0x10_0000`. Low memory
  holds legacy DMA buffers, the BIOS data area under UEFI, and the
  AP bring-up trampoline that Phase 1.5+ will place there. 256 frames
  is a cheap reservation; allocating-and-freeing into low memory
  invites bugs that are tedious to debug.
- **`BootloaderReclaimable` left alone.** The kernel still runs on
  Limine's 64 KiB stack and reads from bootloader-reclaimable
  framebuffer descriptors. Reclamation arrives once the kernel owns
  its stack; tracked here for the next slice.
- **`base_frame` rounded down to `2^(MAX_ORDER+1)`-frame alignment.**
  The bit-index formula `(frame - base_frame) >> (order + 1)` assumes
  `base_frame` aligns with the natural buddy-pair structure at every
  order. Misaligned bases bucket non-buddies into the same pair and
  corrupt coalescing (host tests caught this on the first run with
  arbitrary `Vec<u8>` addresses). Rounding down introduces "phantom"
  frames below the usable range that have bitmap bits but are never
  fed in — they stay marked allocated and out of reach. The overhead
  is at most ~2047 frames (8 MiB) per allocator and only one allocator
  instance exists.
- **Coalesce-bitmap sentinel = 0 in the free-list next slot.** Frame 0
  never enters the allocator (covered by the 1 MiB skip), so `0` is
  a safe "end of list" marker that needs no extra null check.
- **Memmap/HHDM request statics deferred.** The bindings compile, but
  `main.rs` does not declare static instances yet. Wiring them up
  alongside `BuddyAllocator::new` belongs in the slab/heap slice; doing
  it here would grow the diff without adding observable behaviour at
  this scale (no allocator consumer exists yet).
- **`cargo xtask test` subcommand.** Implements the convention
  `kernel/CLAUDE.md` already documents (`run via cargo xtask test`).
  CI now invokes it instead of `cargo test --manifest-path
  tools/Cargo.toml` directly. The kernel's `.cargo/config.toml` pins
  the target to `x86_64-unknown-none`, so xtask forces the host
  triple via `--target` — derived at runtime from `rustc -vV` rather
  than hard-coded.

Verification:

- `cargo xtask build` succeeds: kernel ELF still links against the
  freestanding target with the higher-half linker script.
- `cargo xtask test` runs ten host tests in `tools/xtask` and six in
  `nitrox-kernel`'s lib; all pass.

---

## 2026-05-19 — Phase 1, slice 2: slab allocator and global-allocator wiring

Second Phase 1 slice. Wires the buddy allocator into boot, builds a
SLUB-inspired slab allocator on top, and registers a `#[global_allocator]`
so `extern crate alloc` is usable from kernel code from here onward. Also
adds the kernel's own `SpinLock` primitive (a prerequisite, not optional)
and creates `kernel/docs/lock-ordering.md`, which the kernel CLAUDE.md
already referenced.

What's in the build:

- `kernel/src/libkern/{mod.rs,spinlock.rs}` — kernel-internal primitives
  module. `SpinLock<T>` / `SpinLockGuard<'_, T>` are test-and-set
  (`AtomicBool` + `UnsafeCell<T>`) with `const fn new`. No IRQ masking
  (Phase 1 has interrupts disabled throughout; an `IrqSpinLock` variant
  will land in the IDT slice).
- `kernel/src/mm/heap.rs` — buddy facade. Owns
  `BUDDY: SpinLock<Option<BuddyAllocator>>` and `HHDM_OFFSET: AtomicU64`,
  populated by `init_buddy` at boot. Exposes `buddy_alloc` /
  `buddy_free` / `hhdm_offset` plus a small `BuddyPager` trait that the
  slab module uses (production impl `HeapBuddy`; tests inject a
  `LocalBuddy` wrapping a per-test `BuddyAllocator`).
- `kernel/src/mm/slab.rs` — the slab. `SlabDescriptor` at byte 0 of each
  4 KiB slab page; embedded-freelist allocation; intrusive partial/full
  lists; O(1) free via `(ptr & SLAB_MASK) as *mut SlabDescriptor`. Seven
  size buckets (32..2048 in `×2` steps); requests larger than 2048 B
  bypass to the buddy and carry an `owner = null` sentinel descriptor.
  Exports `kmalloc`/`kfree`/`kzalloc` plus a `KernelAllocator` unit
  struct implementing `core::alloc::GlobalAlloc`.
- `kernel/src/main.rs` — declares `MEMMAP_REQUEST` and `HHDM_REQUEST`
  statics next to the existing framebuffer one; registers
  `#[global_allocator] static ALLOCATOR: mm::slab::KernelAllocator`;
  factors out `init_memory()` to extract the Limine responses, call
  `heap::init_buddy`, then `slab::slab_init`. Boot screen text updates
  to `PHASE 1: ALLOCATORS UP`.
- `kernel/docs/lock-ordering.md` — first version. Documents ranks 1..6,
  with slab as rank 6a and buddy as rank 6b. Calls out the slab → buddy
  nesting as the only allowed allocator-on-allocator pattern.
- `docs/architecture/memory-management.md` — first version. Three-layer
  overview, slab geometry / large-alloc routing / init order /
  locking / Phase 1 limitations.
- `docs/rationale/why-slub-not-buddy-only.md` — rationale doc.

Decisions worth recording:

- **Plain spin lock, not IRQ-saving.** Interrupts are never enabled in
  Phase 1 (no IDT, PIC, or APIC). The `SpinLock` does not mask
  interrupts. When the IDT slice arrives, an `IrqSpinLock` variant will
  be added and call sites audited; allocator locks are likely
  candidates. Today's `SpinLock` becomes the "no-IRQ-needed" choice
  rather than the only choice.
- **Slab returns HHDM-mapped kernel-virtual pointers, not raw `PhysAddr`.**
  Lets `kfree` recover the `SlabDescriptor` via `ptr & !0xFFF` directly,
  with no per-allocation external table. Costs us the ability to return
  a raw `PhysAddr` from `kmalloc`, which no kmalloc consumer wants
  anyway.
- **Untyped `SlabCache`, not `SlabCache<T>`.** The seven kmalloc buckets
  live in a single `[SlabCache; 7]`; that's not possible with a generic
  parameter. A typed `KCache<T>` wrapper for object pools can sit on
  top of the same machinery later.
- **Single global lock per cache, no per-CPU fast path yet.** Phase 1
  has no SMP and no preemption. The cost of an uncontested
  `compare_exchange_weak` on an `AtomicBool` is negligible. Per-CPU
  caching is the natural Phase 3 (SMP) follow-up; the existing
  alloc/free state machine becomes the slow path then, unchanged.
- **Slab → buddy is the sanctioned allocator-on-allocator nesting.**
  Slab's `grow_locked` holds the cache lock (rank 6a) across a
  `buddy_alloc` call (rank 6b). Buddy never recurses into slab, so the
  cycle is impossible. Documented in `kernel/docs/lock-ordering.md`.
- **Large allocations route via an `owner = null` sentinel.** Requests
  larger than 2048 B go directly to the buddy; a stub `SlabDescriptor`
  at byte 0 of the buddy block stores the total block size in
  `obj_size` for `large_free` to recover the order. Routing is O(1) and
  requires no external state. Alternative — a global slab-descriptor
  registry — was rejected because it would add a synchronisation point
  on every alloc and free.
- **`KernelAllocator` panics loudly if called before `slab_init`.** A
  silent "not ready" mode would mask premature-use bugs in any code
  that happens to allocate before `init_memory` runs. With `panic` =
  `abort`, the kernel halts cleanly and the framebuffer never displays
  the boot screen, which is the right tripwire.
- **`obj_offset` rounded up from `size_of::<SlabDescriptor>()` to cache
  alignment, asserted at init.** Three asserts: header fits, alignment
  is honoured, at least one object per slab. Each catches a different
  geometry mistake before any allocation runs.
- **Buddy facade in `mm/heap.rs`, separate from `mm/slab.rs`.** The
  buddy is also the source of pages for the page-table and VMM layers
  that come next; routing those callers through `slab::*` would couple
  unrelated subsystems. Keeping the facade in `heap.rs` keeps slab as
  one client among future others.
- **Test isolation via `BuddyPager` trait + `LocalBuddy`.** Slab's
  hot paths take `&P: BuddyPager` so tests build per-test buddies
  without touching the global `BUDDY` / `SLAB_CACHES` statics. Slight
  cost: production code dispatches through a trait method (one indirect
  call inside `grow_locked` only).

Verification:

- `cargo xtask test` — 23 host tests pass: 4 spinlock + 1 heap +
  12 slab + 6 buddy (existing).
- `cargo xtask build` — kernel ELF builds clean for the
  `x86_64-unknown-none` target.
- `cargo xtask qemu` — boot reaches `kernel_main`, allocator init
  runs without panicking, and the boot screen renders
  "PHASE 1: ALLOCATORS UP". (Adding an `extern crate alloc` smoke
  test in `main.rs` was considered but not landed — the host tests
  cover the `kmalloc`/`kfree` paths the `GlobalAlloc` impl forwards
  to, and the boot-screen render is itself evidence that init
  succeeded.)

Deferred:

- `IrqSpinLock` variant — IDT slice.
- Per-CPU slab caching — SMP slice (Phase 3).
- Empty-slab reclamation back to the buddy — no trigger yet.
- Alignment greater than `SLAB_SIZE` — Phase 2 (DMA buffers).
- DMA / Normal zone split in the buddy — already TODO in
  `mm/buddy.rs`.
- Debug-build lock-ordering checker — code review enforces today.

All filed in `docs/rationale/deferred-decisions.md`.

---

## 2026-05-20 — Phase 1, slice 3: drop the `alloc` crate; `libkern` heap containers

Third Phase 1 slice. Reverses part of slice 2: the kernel no longer
registers a `#[global_allocator]` and will not use the `alloc` crate.
In its place, `kernel/src/libkern/` gains the first hand-rolled,
fallible heap containers — `KBox`, `KVec`, `KString` — that the rest of
the kernel will build on. Supersedes the slice-2 decision
"**registers a `#[global_allocator]`** so `extern crate alloc` is
usable from kernel code from here onward."

What's in the build:

- `kernel/src/libkern/` — new `kbox.rs`, `kvec.rs`, `kstring.rs`
  modules, plus an `AllocError` type in `mod.rs`. `KBox<T>` is a
  fallible owned heap pointer; `KVec<T>` a fallible growable array;
  `KString` a fallible UTF-8 string wrapping `KVec<u8>`. All three
  call `mm::slab::{kmalloc, kfree}` directly. A `kformat!` macro
  (`core::fmt::Write` on `KString`) gives `format!`-style output that
  returns `Result<KString, AllocError>` instead of aborting.
- `kernel/src/main.rs` — the `#[global_allocator] static ALLOCATOR`
  is removed.
- `kernel/src/mm/slab.rs` — `KernelAllocator` and its
  `core::alloc::GlobalAlloc` impl are removed. `kmalloc` / `kzalloc` /
  `kfree` remain as the slab's public surface; doc comments updated.
- `kernel/src/mm/test_support.rs` — `#[cfg(test)]` helper that boots
  the global `BUDDY` / `SLAB_CACHES` statics against a leaked host
  buffer (via `std::sync::Once`), so the `libkern` containers can be
  host-tested through the real `kmalloc` / `kfree` path.
- `docs/architecture/memory-management.md` — initialisation-order
  section updated to drop the `extern crate alloc` claim.

Decisions worth recording:

- **No `#[global_allocator]`; no `alloc` crate.** `alloc`'s every
  type (`Box::new`, `Vec::push`, `BTreeMap::insert`, …) aborts the
  process on allocation failure. A kernel must propagate OOM as a
  `Result`. On stable Rust — which the project is committed to — there
  is no fallible `Box::new` and no fallible `BTreeMap::insert` at all,
  so `alloc` cannot meet the kernel's allocation contract. The kernel
  CLAUDE.md already named `KBox` / `KVec` / `KString` as the kernel's
  containers; enabling `alloc` was the deviation, and slice 2's
  registration is now reverted.
- **The registration is *removed*, not merely left unused.** With no
  registered global allocator, a future `extern crate alloc` plus any
  allocating type fails to *link* ("no global memory allocator
  found"). That is linker-enforced discipline, strictly stronger than
  a clippy lint or a code-review rule — consistent with the project's
  preference (e.g. typestate over const-generics) for letting the
  toolchain enforce invariants.
- **`KBox` / `KVec` bypass `GlobalAlloc` entirely.** They call
  `kmalloc` / `kfree` as plain functions. `kfree` recovers the size
  class from the slab descriptor, so the containers store no `Layout`
  and `Drop` is just `kfree(ptr)`. There is no `krealloc`, so `KVec`
  growth is allocate-copy-free; fixed slab size classes preclude
  in-place growth regardless.
- **Zero-sized types never touch the allocator.** `KBox<T>` /
  `KVec<T>` for a ZST use `NonNull::dangling()` and never call
  `kmalloc` / `kfree`, mirroring `core`/`std` practice. This is also
  why `kfree`'s ZST-sentinel hazard (noted in slab.rs) is safe: the
  containers never hand a ZST pointer to `kfree`.
- **`KBox::try_new` aborts, not `Err`s, if called before
  `slab_init`.** That path is a use-before-init bug, not an
  out-of-memory condition; `kmalloc`'s existing pre-init panic (slice
  2) is the right tripwire and is left in place. The container docs
  state this explicitly.
- **Deferred the intrusive list, red-black tree, and `KArc`.** The
  implementation plan grouped six structures into one memory-
  foundation item. Only `KBox` / `KVec` / `KString` are built now:
  they have zero design risk and a consumer within one or two slices.
  The other three are scheduled just-in-time, because each one's API
  is defined by a consumer that does not exist yet — the intrusive
  list by the scheduler run queue / wait queues; the tree by the VMA
  manager (which needs the *interval-augmented* variant, so a plain
  `RbTree` would be built twice or wrong); `KArc` / `ObjectRef` by
  `KObjectHeader` and the seqlock protocol. Building them speculatively
  now would be guessing. The plan's grouping is annotated inline.
- **`mm::test_support` drives the *global* allocator for tests.** The
  buddy and slab test suites use *local* allocators (`FakeMem` +
  `LocalBuddy`) to stay hermetic. The `libkern` containers have no
  allocator-injection seam — they call the global `kmalloc` / `kfree`
  by design — so their host tests need the real statics live. A
  `Once`-guarded init against a leaked 16 MiB host buffer provides
  that; the slab/buddy locks make sharing it across parallel tests
  sound. An allocator-injection seam on `KBox` / `KVec` was rejected:
  it would complicate every signature to serve tests only, and the
  whole point of these types is that they are *not* generic over an
  allocator.

Verification:

- `cargo xtask test` — host tests pass, including the new `libkern`
  suites (`KBox`, `KVec`, `KString`).
- `cargo xtask build` — kernel ELF builds clean for the
  `x86_64-unknown-none` target with no `#[global_allocator]`.
