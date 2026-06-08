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

---

## 2026-05-20 — Plan stock-take: x86_64 arch naming, diagnostics slice

A take-stock pass before the address-spaces-and-paging slice: the
implementation plan was reviewed against what is actually built and
against what the paging work will need. No code changed. The outcomes
are a corrected plan, a synced set of `CLAUDE.md` files, and the two
decisions below.

What changed:

- `docs/planning/implementation-plan.md` — Phase 0 checklist corrected
  to match reality (deviations below); a new "Kernel diagnostics and
  early fault handling" slice inserted into Phase 1 ahead of "Address
  spaces and paging"; `amd64` path and prose references switched to
  `x86_64`.
- `CLAUDE.md` (root) and `kernel/CLAUDE.md` — doc-sync: `amd64` prose,
  the stale `xtask` command list, and the `test-qemu` /
  `tests/qemu-tests/` references brought in line with reality.
- `docs/spec/`, `docs/architecture/`, `docs/rationale/` — `amd64` prose
  references swept to `x86_64` (8 files). `docs/history/` left
  untouched.

Decisions worth recording:

- **Architecture directory is `x86_64`, not `amd64`.** `amd64` is the
  standard name in the OS-distribution world (Debian, the BSDs, Go's
  `GOARCH`); `x86_64` is the standard name in the toolchain world (the
  `x86_64-unknown-none` target triple, `cfg(target_arch = "x86_64")`,
  LLVM triples, `uname -m`). The toolchain name wins because Rust bakes
  it into the source irrevocably: a directory named `arch/amd64/` would
  sit behind a `cfg(target_arch = "x86_64")` gate — a permanent spelling
  mismatch at the exact site that selects it. `x86_64` also pairs
  consistently with `aarch64`, already the Rust name for the other
  architecture. Scope: this governs source identifiers — the
  `kernel/src/arch/x86_64/` directory, `cfg` gates, the target triple.
  Prose in `docs/spec/`, `docs/architecture/`, and `docs/rationale/`
  also used "amd64" as the architecture's common name; all such
  references were swept to `x86_64`. Two things were deliberately left:
  the proper noun "System V AMD64 ABI" in `docs/spec/syscall-abi.md`
  (the external specification's actual name), and `docs/history/` (the
  decision log is append-only; `os-design-v5.1.md` is an archived
  snapshot).

- **A "kernel diagnostics" slice is pulled in ahead of paging.** Paging
  is the most fault-prone subsystem in a kernel, and the kernel today
  has no observable output: `panic!` / `expect()` halt silently (the
  panic handler has no logging surface), and with no IDT a CPU fault
  triple-faults with zero diagnostics. A small slice — polled COM1
  serial, `kprintln!`, a `PanicInfo`-dumping panic handler, and a
  minimal dump-and-halt IDT for `#UD` / `#GP` / `#PF` / `#DF` — makes
  the paging work debuggable at low cost. Serial and the dump-and-halt
  IDT are one unit: a fault handler with nowhere to print is useless.
  The real exception-table-consulting `#PF` handler stays later, under
  "User memory access" — the diagnostics slice only installs the
  dump-and-halt version.

Plan-vs-reality deviations corrected. The Phase 0 checklist had items
checked off that were never built, or built differently:

- No NASM boot stub was written; the entry point is a pure-Rust
  `extern "C" fn _start`. The 2026-05-13 entry already records this —
  the plan checklist simply disagreed with it.
- No serial output exists; Phase 0 renders to the framebuffer. Serial
  was deferred (2026-05-13 entry) and now has a home in the diagnostics
  slice.
- `xtask test-qemu` was never built — there is no QEMU integration-test
  harness. It remains a deferred item
  (`docs/rationale/deferred-decisions.md`).

---

## 2026-05-20 — Phase 1, slice 4: kernel diagnostics and early fault handling

The fourth Phase 1 slice, pulled in ahead of address spaces and paging so
the paging work is debuggable. The kernel gains a serial console, a
`kprintln!` macro, a `PanicInfo`-dumping panic handler, the kernel's own
GDT + TSS, and an IDT with dump-and-halt handlers for every CPU
exception. Hardware IRQs are still not handled — the kernel never
executes `sti`.

What's in the build:

- `kernel/src/arch/x86_64/regs.rs` — port-I/O wrappers (`inb`/`outb` and
  the 16-/32-bit forms) and `read_cr2`. Per `kernel/CLAUDE.md`, raw
  hardware-register access lives here rather than as scattered `asm!`.
- `kernel/src/arch/x86_64/serial.rs` — a polled 16550 UART driver for
  COM1: `init` (115200 8N1) and `write_byte`, a `core::fmt::Write` impl,
  the `SERIAL` spin-locked global, and the `kprint!` / `kprintln!`
  macros. No interrupts, no allocation, no buffering.
- `kernel/src/arch/x86_64/gdt.rs` — the kernel's own GDT (null, kernel
  code, kernel data, TSS descriptor), a 64-bit TSS, and a 16 KiB
  double-fault IST stack.
- `kernel/src/arch/x86_64/idt.rs` — a 256-entry IDT with handlers on all
  32 CPU exception vectors; naked-function entry stubs; a common
  `exception_dispatch` that dumps the register frame to serial and halts.
- `kernel/src/main.rs` — the `#[panic_handler]` now dumps `PanicInfo`;
  `kernel_main` brings up serial → GDT/TSS → IDT before `init_memory`.
- `kernel/docs/lock-ordering.md` — `SERIAL` added as a rank-7 leaf lock.

Decisions worth recording:

- **Naked-function exception stubs — not NASM, not `x86-interrupt`.** The
  `x86-interrupt` calling convention (`abi_x86_interrupt`) is nightly and
  is barred by the stable-only rule. The stubs are instead naked Rust
  functions (`#[unsafe(naked)]` + `core::arch::naked_asm!`, stable since
  1.88), generated by a `macro_rules!` macro. NASM was not used: it stays
  reserved for the context-switch path, and keeping exception entry as
  in-tree Rust avoids an assembler invocation for 32 near-identical
  stubs. Each stub normalises the stack — vectors without a CPU error
  code push a dummy 0 — so all 32 yield one `ExceptionFrame` layout.

- **All 32 CPU exception vectors handled, not the 4 the plan named.** A
  uniform 32-stub macro is no more code than 4 hand-written stubs and
  catches `#DE`, `#BP`, NMI, `#MC`, and the rest. Vectors 32–255 are left
  not-present; nothing generates them (no `sti`, no PIC/APIC).

- **The kernel's own GDT + TSS were pulled in with the IST.** A reliable
  `#DF` handler needs an IST stack; an IST lives in a TSS; a TSS needs a
  TSS descriptor in a GDT the kernel controls. Limine's GDT cannot be
  extended, so the kernel installs its own (kernel code `0x08`, data
  `0x10`, TSS `0x18`). User-mode selectors are deliberately omitted —
  they must be ordered for `syscall`/`sysret`, which is a later slice.

- **Emergency unsynchronised serial path for the fault handlers.**
  `SpinLock` has no `try_lock` / `force_unlock`, so a panic or exception
  that struck while `SERIAL` was held would deadlock if the handler tried
  to lock it. The panic handler and `exception_dispatch` instead use
  `serial::emergency_writer()`, which drives the fixed COM1 port with no
  lock. Sound only because Phase 1 is single-CPU with interrupts masked;
  flagged in `lock-ordering.md` for revisiting at SMP.

- **`kprintln!` formats with zero allocation.** It writes `format_args!`
  output straight into the locked `SerialPort` via `core::fmt::Write`,
  never through a `KString` — a logging macro must not call the slab
  allocator, least of all from a context near a fault.

- **Boot order: serial → GDT/TSS → IDT → allocators.** Serial has no
  dependency on the allocator or the IDT, so it goes first and makes
  every later step's failures visible. GDT before IDT (the gates
  reference kernel CS `0x08`, and `#DF` needs the TSS's IST1). IDT before
  `init_memory`, the first code that can fault.

- **Host tests scoped to descriptor arithmetic.** `IdtEntry::set_handler`
  and the TSS-descriptor encoder are pure bit-twiddling and are unit
  tested; the `ExceptionFrame` layout is locked with `const` `offset_of!`
  assertions. The UART register sequence (a fixed `outb` list) and the
  `kprintln!` formatting path (`core::fmt`'s own code) were judged
  low-value to host-test and are verified on target instead.

Verification:

- `cargo xtask test` — host tests pass, including the five new `gdt` /
  `idt` cases.
- `cargo xtask build` — kernel ELF builds clean for `x86_64-unknown-none`.
- `cargo xtask qemu` — the serial console shows the boot banner, `CPU
  tables installed (GDT/TSS/IDT)`, and `allocators up`.
- A deliberate `panic!` printed `*** KERNEL PANIC ***` with the file,
  line, column, and message.
- A deliberate read of an unmapped address printed a `#PF` dump: vector
  `0x0e`, the correct `CR2`, and `cs=0x0008` / `ss=0x0010` — confirming
  the IDT, the error-code normalisation, the `ExceptionFrame` layout, and
  the GDT/segment reload all work.

---

## 2026-05-22 — Phase 1, slice 5 (item 1): `ArchPaging` trait and x86_64 implementation

The first item of the "Address Spaces and Paging" slice: the raw
arch-level page-table primitive that the later items of the slice (the
VMA tree, address-space construction, higher-half sharing, kernel
stacks) will build on. No VMM yet — this slice item delivers `map_page`
/ `unmap_page` / `flush_tlb_*` / `set_page_table` and nothing above them.

What's in the build:

- `kernel/src/arch/paging.rs` — new, architecture-neutral: the
  `ArchPaging` trait, `PageFlags` (hand-rolled bitflags), and the
  `MapError` / `UnmapError` enums.
- `kernel/src/arch/x86_64/paging.rs` — new: `X86Paging` (the
  `ArchPaging` impl), the `Pte` newtype and bit constants, the
  9-9-9-9-12 index split, the 4-level table walk, `translate`,
  `active_root`, and `ensure_nxe`. Host-tested.
- `kernel/src/arch/x86_64/regs.rs` — `read_cr3` / `write_cr3` /
  `invlpg` / `rdmsr` / `wrmsr` added alongside the existing port-I/O
  and `read_cr2` wrappers.
- `kernel/src/mm/mod.rs` — `VirtAddr` newtype (mirrors `PhysAddr`),
  with `is_canonical`.
- `kernel/src/arch/{mod.rs,x86_64/mod.rs}` — register the modules;
  `arch` re-exports `X86Paging as Paging` plus `translate` /
  `active_root` / `ensure_nxe`.
- `kernel/src/main.rs` — a read-only paging smoke test in
  `kernel_main`: enable NX, then `translate` the kernel's own code
  address against Limine's live tables.

Decisions worth recording:

- **`ArchPaging` is the first arch *trait*; `gdt`/`idt`/`regs`/`serial`
  remain cfg-gated free-function modules.** GDT/IDT are x86-only
  concepts with no aarch64 analogue, so they need no cross-arch
  contract. Paging does: aarch64's translation-table format genuinely
  differs, and the VMM (later this slice) must be written against an
  abstraction, not against x86 PTEs. The trait *is* that contract.
- **ZST + associated functions, not `&self` / `&mut self`.** The v5.1
  design doc sketched paging methods on `&mut self`, implying per-CPU
  or per-address-space arch state. There is none: the page-table root
  is an explicit `PhysAddr` argument to every method, so the same code
  maps into any address space. `X86Paging` is a unit struct.
- **`map_page` / `unmap_page` do not flush the TLB; the caller does.**
  Flushing is exposed separately (`flush_tlb_page` / `flush_tlb_all`).
  This keeps the map/unmap paths free of privileged instructions — so
  they are fully host-testable — and lets a future bulk mapper amortise
  one flush over many entries.
- **`map_page` returns `AlreadyMapped` on a present leaf; never
  silently replaces.** Remap semantics belong to the VMM, which will
  own the policy. `unmap_page` returns the freed `PhysAddr` so the VMM
  can reclaim or refcount the frame.
- **`EFER.NXE` is enabled by the kernel (`ensure_nxe`).** A PTE with
  the NX bit faults as a reserved-bit violation unless `EFER.NXE` is
  set, and Limine does not guarantee it. Enabling it now keeps
  `PageFlags::NO_EXECUTE` honest — the VMA slice will want W^X
  immediately. `rdmsr` / `wrmsr` wrappers were added to `regs.rs` for
  this; they are general MSR primitives, not EFER-specific.
- **`translate` understands huge pages; `map_page` / `unmap_page` do
  not.** This module only ever creates 4 KiB leaves, so the map/unmap
  walks assume no `PS` bit. `translate`, however, is run against
  Limine's live tables — which may use 2 MiB or 1 GiB pages — so it
  checks the `PS` bit at the PDPT and PD levels. `translate` is `pub`
  (not `pub(crate)`): the boot smoke test lives in the `main.rs` binary
  crate, which is separate from the library crate.
- **Intermediate page-table frames are not reclaimed on `unmap_page`.**
  Tracking emptiness needs a per-table populated-entry count or a
  512-slot scan per unmap. Deferred — see
  `docs/rationale/deferred-decisions.md`. Phase 1 has a single address
  space; the leak is negligible.

Verification:

- `cargo xtask test` — host tests pass, including 11 new
  `arch::x86_64::paging` cases (index split, flag/PTE encode-decode,
  map→translate→unmap round-trips, already-mapped / not-mapped errors,
  misaligned / non-canonical rejection, multi-level table allocation)
  and 3 `VirtAddr` cases.
- `cargo xtask build` — kernel ELF builds clean for `x86_64-unknown-none`.
- `cargo xtask qemu` — boot reaches the smoke test, which prints
  `paging: NX enabled; translate <virt> -> <phys>` and continues to
  the boot screen.

## 2026-05-27 — Phase 1, slice 5 (item 2): VMA tree (interval-augmented intrusive RB-tree)

The second item of the "Address Spaces and Paging" slice: the per-process
data structure that the rest of the VMM will manipulate. No address-space
owner yet, no page-table integration yet — this item delivers the leaf
data types (`VAddrRange`, `Protection`, `MappingKind`, `Vma`) and the
`VmaTree` itself (insert, remove, point lookup, overlap iteration).

What's in the build:

- `kernel/src/mm/vmm.rs` — new: the leaf types, `RbColor` / `RbLink`
  (private), `VmaTree` with `insert` / `remove_covering` / `find_covering`
  / `find_first_overlapping` / `iter` / `iter_overlapping`, an iterative
  post-order `Drop`, and 30+ host-side tests including 200-element
  shuffled-insert + shuffled-remove torture tests with full BST + RB +
  augmentation invariant verification after every operation.
- `kernel/src/libkern/kbox.rs` — `into_raw` / `from_raw` for intrusive
  ownership transfer; `Debug` forwarded to the contained `T`.
- `kernel/src/mm/mod.rs` — registers `mod vmm`.
- `docs/architecture/memory-management.md` — drops the "(not yet)" on
  the VMM row, rewrites the intro paragraph, adds a `## VMA tree`
  section, adds the `AddressSpace`-not-yet Phase 1 limitation.

Decisions worth recording:

- **Tree built tailored to `Vma`, not as a generic container in
  `libkern`.** The `RbLink` is embedded directly in `Vma`; the tree
  operations consume `Vma` fields by name. A generic `RbTree<T>` would
  have to abstract the key extraction and the interval augmentation
  through trait dispatch, paying complexity for a single consumer. The
  only other RB-tree consumer on the horizon (the namespace binding
  tree) is keyed by path components, not address intervals, so it
  wouldn't share an implementation anyway. Revisit if a third consumer
  appears.
- **Iterative RB-tree operations, not recursive.** Insert/delete fixup
  walks *up* the tree from the inserted/deleted node, which is iterative
  with parent pointers regardless of style. Search and in-order
  traversal are iterative trivially. Removes kernel-stack-depth as a
  real concern. Matches Linux's `lib/rbtree.c`.
- **`KBox<Vma>` ownership, not a per-address-space arena.** VMAs come
  and go constantly during a process's life (every `mprotect` boundary
  crossing splits a VMA), so an arena either needs an internal free-list
  (which is just the slab again) or fragments. The slab cache gives
  good locality and O(1) alloc/free without the fragmentation. Matches
  Linux's `vm_area_cachep`. `KBox::into_raw` / `from_raw` thread the
  allocation through the tree's intrusive links.
- **`Protection` is narrower than `arch::paging::PageFlags`.** A VMA
  carries WRITE / EXEC / USER only; `GLOBAL` and the cache-attribute
  bits are per-PTE policy decided at install time (driver MMIO,
  framebuffer), not a property of the address range. The VMM will
  translate `Protection` to `PageFlags` when populating leaves.
  `Protection::empty()` is kernel-only / read-only / non-executable —
  the inverse of `PageFlags::empty()`, which is executable by default
  because `NO_EXECUTE` is opt-in at the hardware level. The VMM
  presents the safer logical default; the translation inverts it.
- **`MappingKind` ships with `Anonymous` only.** `FileBacked(Handle)`
  needs the handle table; `Device(PhysAddr)` needs the driver MMIO
  mapper. Both arrive with their consumers. The enum is open to
  extension and adding a variant only touches the call sites that
  need to act on the new backing kind.
- **Interval augmentation maintained, not consumed.** `subtree_max_end`
  is updated on every structural mutation (insert path walk, rotations,
  remove path walk) but no query reads it today: the leftmost-overlap
  query is already O(log n) without it, and `iter_overlapping` is just
  in-order successor advance terminated at the query end. The
  augmentation pays off for future disjoint-range stabbing queries
  where subtree pruning lets the walk skip whole branches.
- **`Vma` is `!Send` / `!Sync` by composition.** Holding `NonNull` in
  the link field demotes the type's auto-traits. This is intentional —
  a `Vma` in a tree is bound to its `AddressSpace`'s lock. The
  `AddressSpace` will carry `unsafe impl Send` when it lands, with a
  SAFETY comment pointing at the lock.
- **`insert` rejects overlap rather than splitting / merging /
  replacing.** The VMM's `mprotect`-style operations (split a VMA on a
  protection-change boundary; merge adjacent compatible VMAs) belong at
  a higher layer than the tree; the tree's invariant is just "no
  overlap." Returning the rejected `KBox<Vma>` back to the caller keeps
  ownership clean and lets the higher layer decide what to do.

Verification:

- `cargo xtask test` — host tests pass (99 total, +24 in `mm::vmm`):
  range arithmetic and protection bitflag operations; insert
  invariants under ascending / descending / 200-element shuffled
  sequences with full BST + RB + augmentation verification after every
  insert; overlap rejection across identical / starts-inside /
  ends-inside / nested-both-ways shapes; adjacent-range acceptance;
  remove invariants under ascending / descending / 200-element
  shuffled removes (different shuffle seeds) with full verification
  after every remove; iterator correctness on empty, single-node, and
  multi-node trees, post-remove queries, and full-tree-coverage
  comparison between `iter` and `iter_overlapping`; iterative `Drop`
  across repeated 256-node tree teardowns.
- `cargo xtask build` — kernel ELF builds clean for `x86_64-unknown-none`.

## 2026-05-27 — Phase 1, slice 5 (item 3): `AddressSpace` skeleton

The third item of the "Address Spaces and Paging" slice: pair the VMA
tree with a page-table root so VMA mutations actually update hardware
translations. No higher-half kernel mapping yet (the next item) and no
ELF loader yet (the item after) — this lands the bridge layer that
both later items consume.

What's in the build:

- `kernel/src/mm/addr_space.rs` — new: `AddressSpace` (a
  `SpinLock<Inner>` wrapping `VmaTree` + PML4 `PhysAddr`), `new`,
  `map_vma`, `unmap_covering`, `root`, `is_empty`, `len`, `Drop`,
  the `MapError` enum, plus private `free_vma_pages` /
  `rollback_partial_map` / `protection_to_page_flags` helpers and 8
  host-side tests.
- `kernel/src/mm/mod.rs` — registers `mod addr_space`.
- `docs/architecture/memory-management.md` — new `## AddressSpace`
  section; updates Phase 1 limitations from "no AddressSpace yet" to
  the more specific "exists but no kernel-half mapping / no TLB flush
  / no ELF loader."
- `kernel/docs/lock-ordering.md` — rank 4 (kernel-object internal
  locks) flips from "not yet present" to "live as of Phase 1 slice 5
  (item 3): AddressSpace."

Decisions worth recording:

- **`SpinLock<Inner>` wrapping, not flat fields + separate lock.**
  Linux's `mm_struct` keeps fields directly addressable and uses
  `mmap_lock` as a separate semaphore — that's what C allows. In
  Rust, wrapping the inner state in `SpinLock<Inner>` makes "field
  access requires the lock" a type-system guarantee, not a code-review
  convention. There is no way to read or modify `vma_tree` or `root`
  without going through `lock()`.
- **Eager per-page anonymous allocation.** `map_vma` allocates and
  zeros one frame per page up front. Lazy on-fault allocation is the
  real-OS pattern (Linux only commits frames when the page is first
  touched), but it requires a page-fault handler that can service VMA
  faults — the current `#PF` handler is the dump-and-halt one from
  the diagnostics slice. Eager works today and the switch to lazy is
  a local change to `map_vma` plus PF-handler upgrade when that lands.
- **Per-page allocate-and-install in lockstep, with rollback on
  failure.** The alternative was pre-allocate-then-commit using a
  temporary `KVec<PhysAddr>` to stage frames. Rejected: a 100 MiB
  anonymous mapping would need a 25,600-entry temporary vector. The
  lockstep loop walks the same number of pages but never holds more
  than one allocated-but-uninstalled frame at a time. Rollback walks
  back through the installed range uninstalling and freeing —
  symmetric work, no extra storage.
- **Frame ownership tracked by the page tables themselves.** No
  per-VMA list of owned `PhysAddr`s.
  `ArchPaging::unmap_page(root, virt)` already returns the
  `PhysAddr` it freed; `unmap_covering` and `Drop` hand each one
  straight to `buddy_free`. Adding a per-VMA frame list would
  duplicate state. For `MappingKind::Anonymous` we always free the
  returned frame; the call site will branch on backing kind when
  `FileBacked` (page cache owns the frame) and `Device` (kernel never
  allocated it) arrive.
- **`unreachable!()` for `ArchPaging` errors that pre-validation
  makes impossible.** `map_page` can return `AlreadyMapped` (we held
  the AS lock and pre-checked tree overlap) and `Misaligned`
  (VAddrRange enforces page alignment; our per-page address is
  `start + i*PAGE_SIZE`). Both would indicate kernel-internal
  invariant violations. Per `kernel/CLAUDE.md`'s
  "`panic!()` outside of explicitly-unrecoverable error paths"
  carve-out, panicking is the correct response.
- **`Drop` drains the tree leftmost-first via `iter().next() +
  remove_covering`** rather than a dedicated `pop_first` on
  `VmaTree`. The iter borrow is scoped to a block that ends before
  the mutating call, so the borrow checker accepts it without
  ceremony. Adding `pop_first` for one consumer would be premature;
  revisit if a second consumer appears.
- **Higher-half kernel mapping deferred to its own sub-item.** A
  fresh AS has an all-zero PML4: switching to it would triple-fault
  because the running kernel's code wouldn't be mapped. We could
  have built the kernel-half template inheritance into `new()`, but
  it needs the kernel image's actual PML4 entries (which depend on
  what Limine handed us) and is a distinct architectural concern.
  Keeping it separate gives that work its own design-and-test cycle.
- **No TLB flushing.** No CPU has any `AddressSpace` loaded today;
  there is nothing in the TLB to invalidate. The scheduler will
  introduce `set_active` and inherit responsibility for flush
  policy.
- **ELF loader split per the universal kernel/userspace boundary.**
  Linux / Windows / macOS all draw the same line: the kernel handles
  parsing the executable header, mapping LOAD segments, setting up
  the initial stack, and (when present) loading the dynamic linker
  interpreter. Symbol resolution and library loading run in
  userspace via `ld.so` / NTDLL / dyld. We will follow this split.
  This sub-item lands the AS skeleton; the next sub-item adds the
  kernel-half mapping; the sub-item after lands the in-kernel ELF
  loader for static binaries. PT_INTERP + a userspace dynamic linker
  come later when a binary actually needs them — init and the early
  userspace will be statically linked.

Verification:

- `cargo xtask test` — host tests pass (107 total, +8 in
  `mm::addr_space`): `new` builds a real empty AS with a
  page-aligned PML4; single-page `map_vma` installs a PTE that
  `translate` finds; multi-page `map_vma` installs every PTE in the
  range and nothing outside it; `unmap_covering` returns the box
  and removes every PTE; overlap is rejected with the original
  mapping untouched and the rejected box returned; kernel-half
  ranges are rejected; `unmap_covering` returns `None` on a miss;
  `Drop` cleanly tears down repeated populated address spaces
  without exhausting the 16 MiB host heap.
- `cargo xtask build` — kernel ELF builds clean for
  `x86_64-unknown-none`.

## 2026-05-27 — Phase 1, slice 5 (item 4): in-kernel ELF loader (static binaries)

The fourth item of the "Address Spaces and Paging" slice: take a
static ELF64 binary as a `&[u8]` and populate an `AddressSpace` with
its LOAD segments + an initial stack VMA. Closes out "Address space
construction from an ELF image" as a parent item.

What's in the build:

- `kernel/src/mm/elf.rs` — new: hand-rolled ELF64 parser (`Ehdr` /
  `Phdr` reader functions using `u{16,32,64}::from_le_bytes`),
  `load_elf(asp, bytes)`, `EntryInfo`, `ElfLoadError`,
  `map_load_segment` helper, plus a `STACK_TOP` / `STACK_SIZE`
  pair and 12 host-side tests including a Vec-based test ELF
  builder.
- `kernel/src/mm/mod.rs` — registers `mod elf`.
- `docs/architecture/memory-management.md` — adds `## ELF loader`
  section; updates Phase 1 limitations to reflect "loader exists,
  static-only / no argv setup."
- `docs/planning/implementation-plan.md` — checks off both the
  ELF-loader sub-item and the parent "Address space construction
  from an ELF image."

Decisions worth recording:

- **Universal kernel/userspace ELF loader split.** Linux
  (`binfmt_elf` → `ld.so`), Windows (kernel loader → NTDLL), and
  macOS (kernel → `dyld`) all draw the line at the same place: the
  kernel handles parsing the executable header, mapping LOAD
  segments, setting up the initial stack, and (when present)
  loading the dynamic linker interpreter. Symbol resolution,
  library loading, and relocation run entirely in userspace. We
  follow the same split. PT_INTERP support and a userspace dynamic
  linker arrive when a binary actually needs them — init and the
  early Nitrox userspace will be statically linked, same as Linux's
  early userspace historically was.
- **Static binaries only in this commit.** Both `ET_DYN` (PIE) and
  `PT_INTERP` (dynamic) are rejected. PIE handling needs base
  randomization — a separate sub-item. The dynamic-linker
  interpreter cannot be loaded without a userspace `ld.so`
  equivalent — also separate. Restricting to `ET_EXEC` gets us
  what init needs without preempting either future design call.
- **Hand-rolled parser, not `goblin` or `xmas-elf`.** Per
  `kernel/CLAUDE.md`'s no-external-crates rule, the ELF parser is
  hand-rolled. The footprint is small (validate `e_ident`, read
  ~20 fields total across `Ehdr` and `Phdr`); a crate dependency
  would be heavier-weight than the parser itself.
- **`load_elf(asp, bytes)` as a free function, not
  `AddressSpace::from_elf(bytes)`.** The function composes (build
  AS via `AddressSpace::new`, then populate via `load_elf`) rather
  than baking ELF knowledge into the AS constructor. The AS type
  stays format-agnostic; future loaders (PE for testing? raw
  blobs?) can sit alongside `load_elf` in `mm/elf.rs` or its
  successors without rippling into `addr_space.rs`.
- **Bytes copied via the HHDM, not via `UserPtr` copy primitives.**
  The frames we're writing into are freshly-allocated kernel-owned
  memory; the `UserPtr` discipline (which exists for a different
  reason — protecting against bad user pointers during syscalls)
  doesn't apply yet. HHDM access is the natural way: `translate`
  the just-mapped virtual address to find the physical frame, then
  write through `phys + hhdm_offset()`.
- **Page-by-page copy loop, not bulk-copy-then-fixup.** Each
  iteration covers `min(remaining_in_page, remaining_in_file)`
  bytes starting at the current `va` / `file_off` pair. The
  alternative (compute every (virt, phys, len) triplet up front,
  then bulk-copy) needs a temporary vector. The per-page loop
  works in fixed memory and is no slower for the volumes Phase 1
  cares about.
- **No partial-load rollback.** A segment failing mid-load leaves
  the AS in a partial state. The caller drops the AS;
  `AddressSpace::Drop` cleans up. The alternative — walking back
  through already-installed segments to unmap them — adds
  significant code for a path that's only exercised on malformed
  ELFs or true OOM. Both are rare and the cleanup-by-Drop strategy
  is already correct.
- **No argv / envp / auxv on the stack.** Nitrox passes argv and
  env as typed structural values rather than C strings (per the
  v5.1 design doc). The handoff format is defined by the userspace
  runtime, which doesn't exist yet — the "first userspace process"
  milestone is where the format gets decided. Until then,
  `load_elf` just maps an empty 16 KiB stack at a known address.
- **Fixed stack placement at `STACK_TOP = 0x7FFF_FFFF_0000`.**
  Picked to be page-aligned, canonical, and well below
  `USER_VIRT_END`. ASLR for the stack is a future hardening pass,
  alongside the kernel-image and mmap-arena ASLR slots.
- **`map_load_segment` reports overlap-or-canonical-failure
  uniformly**, even though `MapError` from `map_vma` distinguishes
  them. From the loader's perspective, both are "this ELF places a
  segment somewhere it can't go" — the user (the developer who
  built the binary) cares whether the binary is malformed, not
  about the internal subdivision. The granular `MapError` stays
  available for future callers that want it.

Verification:

- `cargo xtask test` — host tests pass (119 total, +12 in
  `mm::elf`): truncated input, bad magic, wrong class / data /
  version / machine / type, `PT_INTERP` present, single-page
  LOAD segment maps and copies bytes (with BSS verified zero),
  non-zero in-page offset segment, multi-page segment span,
  alignment violation, kernel-half segment range, overlapping
  segments, stack VMA at the right address and zero-initialised.
  Tests use a Vec-based `ElfBuilder` to construct minimal valid
  ELFs in-memory; no external binaries needed.
- `cargo xtask build` — kernel ELF builds clean for
  `x86_64-unknown-none`.

## 2026-05-27 — Phase 1, slice 5 (item 5): higher-half kernel mapping shared across address spaces

Until now, `AddressSpace::new()` produced an all-zero PML4 —
"installable but not loadable." Switching `CR3` to it would
triple-fault the moment the kernel tried to fetch its next
instruction. This item closes that gap, making every freshly-
constructed AS share the boot kernel's higher-half mappings.

What's in the build:

- `kernel/src/arch/paging.rs` — new `unsafe fn
  inherit_kernel_mappings(root)` on the `ArchPaging` trait, with
  the contract: callers pass a writable top-level page table they
  own; the impl populates whatever kernel-half mappings the active
  architecture's process ASes need to inherit.
- `kernel/src/arch/x86_64/paging.rs` — new private
  `KERNEL_TEMPLATE: SpinLock<Option<[u64; 256]>>`, public
  `init_kernel_template(boot_root)` that snapshots PML4 entries
  256..512 into it via private `read_kernel_half_entries`, and
  the `inherit_kernel_mappings` impl that copies them into a
  fresh PML4 via private `write_kernel_half_entries`. The two
  helpers carry the unsafe work; the trait method is a thin
  template + write wrapper.
- `kernel/src/arch/mod.rs` — re-exports `init_kernel_template`
  alongside the existing `active_root` / `ensure_nxe` / `translate`.
- `kernel/src/mm/addr_space.rs` — `AddressSpace::new` calls
  `Paging::inherit_kernel_mappings(root)` after zeroing the new
  PML4. The doc on `new` is updated to remove the "installable but
  not loadable" caveat.
- `kernel/src/main.rs` — boot path gains a `paging_init()` step
  that runs `ensure_nxe` and `init_kernel_template(active_root())`
  before the existing translate smoke test.
- `kernel/src/mm/test_support.rs` — `init_global_heap` now also
  initialises the template from an all-zero fake PML4 so the
  existing `AddressSpace` tests' `inherit_kernel_mappings` call
  doesn't panic.
- `kernel/docs/lock-ordering.md` — `KERNEL_TEMPLATE` slots in at
  rank 6c, alongside the allocator locks; documented as a
  no-nest leaf taken only at boot and at AS construction.
- `docs/architecture/memory-management.md` — new
  `## Kernel-half PML4 sharing` section describing the
  template-and-copy mechanism, the shared-intermediate-tables
  consequence, and the "PML4 entries for the kernel half are
  immutable post-boot" rule.

Decisions worth recording:

- **Template-and-copy at AS construction, not
  shared-PML4-by-reference.** Each AS owns its own PML4 frame
  (so CR3 holds a per-AS value, which is required for any
  future ASID tagging and for cleanliness). What's shared are
  the *entries* (and through them the intermediate tables they
  point at). The alternative — a single shared PML4 with
  per-AS PML4 entries swapped in on CR3 load — would require
  modifying global state on every context switch, which is
  worse on every axis.
- **Snapshot at boot, not "always read live."** The kernel's
  higher-half PML4 entries are populated by Limine before
  `_start` runs and never change post-boot (per the
  "kernel-half PML4 entries are immutable post-boot" rule
  this item establishes). A static snapshot avoids paying the
  CR3-read-and-walk cost per `AddressSpace::new`, and makes
  the source of truth explicit. If a future change really
  needs the kernel half to grow new PML4 entries at runtime,
  it has to also visit every AS — the design wants that to
  be obviously expensive.
- **`SpinLock<Option<[u64; 256]>>` over `Once`-style init.**
  The `Once` pattern matches the buddy/slab init style, but
  the kernel-half template benefits from being re-initialisable
  in tests (the `init_global_heap` helper writes a zero
  template; a future on-target ASLR-style rebuild might want
  to re-snapshot). `SpinLock<Option<...>>` allows that without
  adding a test-only escape hatch.
- **`init_kernel_template` is `unsafe`.** It reads through
  HHDM into a raw `u64` array; the `boot_root` argument
  carries the unsafe invariants (real PML4, page-aligned,
  HHDM-reachable). Marking it `unsafe` shifts those to the
  caller — `paging_init` in `main.rs`, where the invariants
  obviously hold given `arch::active_root()` returns the
  live CR3.
- **Rank 6c for `KERNEL_TEMPLATE`, alongside the allocators.**
  The lock is a leaf: held briefly, no nesting, no other lock
  acquired while inside it. Could have been a fresh rank, but
  grouping with the other constant-time leaves (6a, 6b) makes
  the lock-ordering table easier to reason about.
- **No `inherit_kernel_mappings` on aarch64 (when implemented):
  no-op.** TTBR0/TTBR1 split keeps the kernel half in a
  separate translation register that process ASes never touch.
  Putting the responsibility on `ArchPaging` rather than baking
  the x86_64 mechanism into `AddressSpace::new` keeps the
  arch-neutral caller unchanged.
- **Test the read/write helpers, not the global template.**
  Host tests for `read_kernel_half_entries` and
  `write_kernel_half_entries` exercise the byte-shuffling
  against fake PML4 frames with marker patterns. The trait
  method that reads the global template is implicitly tested
  by every existing `AddressSpace` test (which now calls
  `inherit_kernel_mappings` against `test_support`'s zeroed
  template and would panic on a use-before-init bug).

Verification:

- `cargo xtask test` — host tests pass (122 total, +3 in
  `arch::x86_64::paging::tests`): `read_kernel_half_entries`
  captures only the kernel half; `write_kernel_half_entries`
  preserves the user half and writes the kernel half;
  read-then-write round-trips. Every existing AS test still
  passes (the `inherit_kernel_mappings` call against
  `test_support`'s zeroed template is a no-op for tests that
  don't validate kernel-half entries).
- `cargo xtask build` — kernel ELF builds clean for
  `x86_64-unknown-none`.

## 2026-05-27 — Phase 1, slice 5 (item 6): kernel vmap + per-thread kernel stack

The sixth item closes out the address-spaces-and-paging slice. It
lands the first kernel-half post-boot allocator (a bump-pointer vmap)
and its first consumer (`KernelStack`), exercising end-to-end the
shared-PDPT machinery that item 5 set up.

What's in the build:

- `kernel/src/arch/paging.rs` — new `unsafe fn
  ensure_kernel_intermediate(root, virt) -> Result<(), MapError>`
  on `ArchPaging`. The contract: pre-allocate whatever top-level
  kernel-half intermediate page tables are needed so post-boot
  leaf installs at `virt` propagate to every AS via the captured
  template. On x86_64 (4-level paging) this allocates a PDPT under
  the PML4 entry covering `virt`; on aarch64 (split TTBR) this
  will be a no-op.
- `kernel/src/arch/x86_64/paging.rs` — `ensure_kernel_intermediate`
  impl uses `pml4_index(virt)`, `alloc_page_table`, and
  `Pte::new_table`. Idempotent: returns `Ok` if the entry is
  already present.
- `kernel/src/mm/kvmap.rs` — new module. `KERNEL_VMAP_START` /
  `KERNEL_VMAP_END` constants per the architecture overview
  (16 TiB at `0xFFFF_C000_0000_0000`). `VMAP_NEXT: SpinLock<u64>`
  bump cursor (lock rank 6d, leaf). `vmap_alloc_pages(n)` returns
  a page-aligned virtual address and advances the cursor.
  `init()` calls `Paging::ensure_kernel_intermediate` for the
  vmap start so the captured template includes the PDPT.
- `kernel/src/mm/kstack.rs` — new module. `KernelStack` carries
  the stack top, base, backing frames, and the install root.
  `KernelStack::new(root)` reserves `KERNEL_STACK_PAGES + 1`
  vmap pages (one guard + N stack), allocates frames, installs
  PTEs writable / NX / kernel-only. Drop unmaps and frees.
  `KERNEL_STACK_PAGES = 4` (16 KiB).
- `kernel/src/mm/mod.rs` — registers `mod kstack` and `mod kvmap`.
- `kernel/src/main.rs` — `paging_init` now does `ensure_nxe` →
  `kvmap::init` → `init_kernel_template`. The ordering is
  load-bearing: `kvmap::init` modifies the live PML4 in ways the
  template snapshot must capture.
- `kernel/docs/lock-ordering.md` — adds rank 6d for `VMAP_NEXT`
  and a leaf-no-nest note.
- `docs/architecture/memory-management.md` — new `## Kernel vmap
  and per-thread kernel stacks` section; Phase 1 limitations
  updated.

Decisions worth recording:

- **Bump-pointer allocator, no freelist.** The vmap region is
  16 TiB. Each kernel stack consumes 5 pages = 20 KiB of vmap
  (the bump never reclaims). To run out we'd need ~800 million
  stack allocations. The freelist isn't worth the complexity for
  Phase 1. If kernel stacks ever churn heavily (they shouldn't —
  a stack lives as long as its thread), a freelist is a local
  addition to `kvmap.rs` only.
- **Pre-allocate only one PDPT, not the full 16 TiB of PDPTs.**
  32 PDPTs covering the whole vmap region would cost 128 KiB at
  boot. One PDPT covers 512 GiB — well past anything Phase 1
  will use. The cost is "if vmap ever crosses 512 GiB we have to
  add the next PDPT at boot." That's enforced by the
  immutable-post-boot rule from item 5: late additions are
  impossible without visiting every AS.
- **`ensure_kernel_intermediate` is on `ArchPaging`, not a free
  function.** The concept ("pre-allocate the top-level
  kernel-half intermediate tables") generalises across
  architectures even though the implementation differs (PDPT on
  x86_64, no-op on aarch64). Putting it on the trait keeps
  arch-neutral callers (`kvmap::init`) unchanged across ports.
- **`KernelStack::new` takes `root` explicitly.** Because the
  shared PDPT means any AS can be used as the install target
  with the same observable result, we could plausibly default to
  `active_root()`. But making it explicit (a) keeps the function
  testable host-side (tests pass a test AS's root, not a real
  CR3 value), (b) documents that this stack is associated with a
  specific AS for `Drop`'s benefit, and (c) avoids hiding the
  arch::active_root dependency.
- **Guard page at the **bottom** of the stack region.** Stacks
  grow down on x86_64; overflow happens at low addresses.
  Placing the guard one page below `base` (the lowest mapped
  byte) catches it. Some kernels (notably FreeBSD) also have a
  guard above the top for "underflow" detection, but that's
  pointless for normal stack use — there's no way to underflow a
  stack you allocated.
- **`PageFlags::WRITABLE | PageFlags::NO_EXECUTE` only.** Kernel
  stacks need W (we write to them) and NX (no instruction fetch).
  USER is deliberately absent — these are kernel-only. GLOBAL
  could be set in principle (kernel-only mappings persist across
  CR3 reloads), but Phase 1 doesn't have a global-bit story yet;
  leaving it absent matches every other kernel mapping the
  template captures from Limine. Revisit during the Phase 3
  global-bit / PCID hardening pass.
- **No production consumer yet.** Threading lands in a later
  slice. `KernelStack` is built now because the kvmap allocator
  + guard-page primitive belong with the memory subsystem; the
  first consumer is incidentally a stack, but per-CPU data and
  driver MMIO will use the same vmap allocator when they arrive.

Verification:

- `cargo xtask test` — host tests pass (130 total, +8 in the new
  modules): kvmap allocations are page-aligned, in the vmap
  region, and monotonically increasing; `KernelStack::new`
  installs the stack pages and leaves the guard unmapped (verified
  via `translate`); `top = base + KERNEL_STACK_BYTES`; the guard
  is exactly one page below `base`; multiple stacks have disjoint
  ranges; `Drop` unmaps the stack pages.
- `cargo xtask build` — kernel ELF builds clean for
  `x86_64-unknown-none`.

---

## 2026-05-27 — Phase 1, slice 6 (item 1): user-access exception table + `#PF` recovery

First commit of the user-memory-access-discipline slice. Lands the
plumbing the copy primitives will hang off of in slice 2: a linker
section that holds `(fault_pc, recovery_pc)` pairs, a lookup function
that walks it, and a `#PF` handler that consults the table and
`iretq`s to the recovery PC on a match. Nothing registers entries
yet — the copy primitives are slice 2 — so the section is empty in
this commit. The handler still dump-and-halts on a miss, matching
the diagnostics IDT's behaviour for every other vector.

What's in the build:

- `kernel/linker.ld` — new `.user_access_table` output section in
  the rodata segment, bracketed by `__start_user_access_table` and
  `__stop_user_access_table`. `ALIGN(8)` matches the `u64`-pair
  entry layout. `KEEP` because nothing in Rust source refers to
  the entries by name — they're indirectly reachable only through
  the start/stop symbols, so `--gc-sections` would otherwise drop
  them. Empty section is well-defined: `start == stop` means "no
  recovery sites registered."
- `kernel/src/mm/user_access.rs` — new module. `ExtableEntry`
  (`#[repr(C)]`, two `u64`s) is the layout slice 2's inline asm
  will emit with `.quad` directives. `lookup_recovery(fault_pc)
  -> Option<u64>` walks the bracketed slice linearly and returns
  the recovery PC on match. A small `lookup_recovery_in(table,
  fault_pc)` helper takes an explicit slice so the lookup is
  host-testable without the linker symbols (`cfg(test)` returns
  an empty table).
- `kernel/src/mm/mod.rs` — registers `mod user_access`.
- `kernel/src/arch/x86_64/idt.rs` — splits vector 14 (`#PF`) off
  the common `exception_stub!` macro. The macro stubs end in
  `ud2` because their dispatcher is `-> !`; the new `vec14` stub
  ends in `iretq` because `pf_dispatch` is allowed to return.
  Stub flow: push vector + 15 GPRs (the CPU pushed the error
  code), `call pf_dispatch`, on return pop everything and
  `iretq` to the patched RIP. `pf_dispatch` reads the saved RIP,
  calls `lookup_recovery`, writes the recovery PC back to
  `frame.rip` and returns on match, or falls through to a
  factored-out `dump_and_halt(&ExceptionFrame) -> !`
  (previously the body of `exception_dispatch`) on miss.

Decisions worth recording:

- **Absolute 64-bit PCs in entries, not 32-bit relative offsets.**
  Linux uses relative offsets to be KASLR-friendly. Nitrox has no
  KASLR and isn't planning any in Phase 1. The per-entry cost
  difference (16 vs 8 bytes) is negligible at the entry counts
  we expect (one per copy primitive, ~5 in total for slice 2),
  and absolute PCs simplify both the asm emitter and the lookup.
  Revisit if KASLR ever lands.
- **Linear scan over sorted-table + binary search.** Lookup runs
  only on the rare (faulting) path. The full table from slice 2
  fits well inside a single cacheline. A linear scan with no
  sorting requirement also keeps the asm-side `.pushsection`
  emitter trivial — no need to maintain ordering across
  translation units. Revisit if Phase 2 ever pushes the count
  past a few dozen.
- **`pf_dispatch` returns; the fatal path calls `dump_and_halt`
  itself.** Two alternatives were considered: (a) make
  `pf_dispatch` return an enum (`Recovered`/`Fatal`) and have the
  stub branch on it, and (b) make `pf_dispatch` always return
  and let the stub conditionally `iretq` vs `ud2`. (a) needs a
  second register output from the stub; (b) needs a branch in
  asm. The chosen path keeps the stub straight-line and pushes
  the conditional into Rust where it belongs. The trade-off is
  the dispatcher has two effective return modes — return normally
  on recovery, or never return — which is exactly what `-> ()`
  vs `-> !` already encodes, applied at the call boundary inside
  the function rather than at the function signature.
- **Vector 14 is hand-written, not added to the macro.** The
  macro's contract is "uniform stub, `ud2` at the end, dispatcher
  is `-> !`." Generalising the macro to support both ending
  shapes would mean either a second macro arm or a runtime branch
  in every stub — both worse than the duplication of one
  hand-written stub. The duplication is bounded by `#PF` being
  the only recoverable CPU exception (everything else really is
  fatal in Phase 1).
- **The "VMA lookup" branch the plan mentions is not in this
  commit.** Today there's no concept of an active address space:
  the kernel still runs on Limine's tables, and the scheduler
  that will own `set_active` hasn't landed. Until it does, a
  `#PF` whose RIP is not in the exception table is either a
  kernel bug (today's behaviour: dump-and-halt is correct) or a
  user-space fault that can't be delivered anywhere meaningful
  (no process, no notification channel). When the scheduler
  arrives, `pf_dispatch` will grow a second decision step
  between "exception-table lookup" and `dump_and_halt`. Noted
  as a follow-up in the slice-2 wrap-up.
- **Host-tested via a sliced-out pure function.** The real
  `lookup_recovery` reads the linker symbols, which don't exist
  under `cargo test`. Factoring out `lookup_recovery_in(table,
  fault_pc)` and providing a `cfg(test)` empty-table shim for
  the wrapper keeps every interesting case (empty / single hit
  / multiple entries / duplicate fault_pc) host-testable, and
  the layout-assertions test pins down the asm contract
  (`offset_of!(fault_pc) == 0`, `offset_of!(recovery_pc) == 8`,
  `size_of == 16`, `align_of == 8`) so slice 2's `.quad` writes
  can't drift away from the Rust struct.

Verification:

- `cargo xtask test` — host tests pass (136 total, +6 in
  `mm::user_access`): empty table, single-entry hit/miss,
  multi-entry recovery routing, duplicate-fault-pc determinism,
  the host-build empty-table shim, and the layout-contract
  assertions.
- `cargo xtask build` — kernel ELF builds clean for
  `x86_64-unknown-none`.
- `readelf -S` + `nm` confirm the `.user_access_table` section
  exists in the kernel ELF with size 0 and that
  `__start_user_access_table == __stop_user_access_table` —
  the well-defined "no entries yet" state.
- Boot under QEMU+OVMF reaches the existing diagnostics
  milestones ("diagnostics online", "CPU tables installed",
  "allocators up", paging smoke-test passes) — the IDT install
  picks up the new vec14 stub without disturbing the existing
  boot path.

---

## 2026-05-27 — Phase 1, slice 6 (items 2-4): UserPtr + copy primitives + SMAP/SMEP

Second commit of the user-memory-access-discipline slice. Lands what
slice 1's exception table existed to support: the opaque user-pointer
types, the five copy primitives, and the boot-time SMAP/SMEP enable.
Closes out the entire user-memory-access section of the implementation
plan (items 1, 3, 4, plus the slice-1 items 2 and 5).

What's in the build:

- `kernel/src/arch/x86_64/regs.rs` — new `read_cr4` / `write_cr4`
  and `cpuid(leaf, subleaf)`. The cpuid impl avoids LLVM's
  rbx-as-operand restriction by routing the EBX result through a
  swap register (`xchg rbx, {tmp:r}` before/after the cpuid).
  `stac` and `clac` are deliberately *not* exposed as Rust-visible
  wrappers — they live only inside the inline asm of
  `arch::user_access`, where the "only inside copy primitives"
  SMAP discipline is structurally enforced.
- `kernel/src/arch/x86_64/paging.rs` — new `ensure_smap_smep`.
  Reads CPUID 7.0:EBX, panics if either feature bit (7 = SMEP,
  20 = SMAP) is missing, otherwise ORs `CR4.SMEP | CR4.SMAP` and
  writes CR4 back. Bundled with `ensure_nxe` under the new
  arch-neutral `arch::init_protections()` entry point so
  `main.rs::paging_init` calls a single function rather than
  touching x86 feature names directly. `ensure_nxe` and
  `ensure_smap_smep` are now `pub(crate)`; only
  `init_protections` is re-exported through `arch::`.
- `kernel/src/mm/user_access.rs` — the arch-neutral half.
  - `UserPtr<T>` / `UserMutPtr<T>`: `#[repr(transparent)]` over a
    `u64` with `PhantomData<*const T>` / `PhantomData<*mut T>` for
    type tagging. `new(addr)` validates user-half (`addr <
    USER_VIRT_END`) and alignment for `T`. The held `u64` is
    `pub(crate)` only — outside this module there is no way to
    read a user address.
  - `UserAccessError`: `BadAddress`, `Misaligned`, `Fault`,
    `NoTerminator`.
  - Five public copy primitives: `copy_from_user<T: Copy>`,
    `copy_to_user<T: Copy>`, `copy_slice_from_user`,
    `copy_slice_to_user`, `copy_cstr_from_user`. Each runs
    validate-range → dispatch into `arch::user_access::*` →
    map the arch signal into `Result<_, UserAccessError>`.
- `kernel/src/arch/x86_64/user_access.rs` — the x86_64 arch half.
  - `pub(crate) unsafe fn copy_bytes_raw(dst, src, len) -> bool`
    (true = faulted). Inline asm with `stac` / `rep movsb` /
    `clac`. Exception-table entry emitted via `.pushsection
    .user_access_table` registers the `rep movsb` PC as the
    fault site and a recovery label as the resume PC.
  - `pub(crate) enum CstrCopyOutcome { Ok(usize), Fault, NoTerminator }`
    and `pub(crate) unsafe fn copy_cstr_raw(...) -> CstrCopyOutcome`.
    The asm uses a `lodsb`/`stosb` byte loop with `lodsb` as the
    single registered fault site.
  - Host-test stubs: under `cfg(test)` both raw primitives fall
    back to `core::ptr::copy_nonoverlapping` (and a byte loop for
    cstr) so the mm-layer validation tests exercise the full
    wrapper plumbing without privileged instructions.
- `kernel/src/arch/mod.rs` — re-export `ensure_smap_smep`
  alongside the existing `ensure_nxe`.
- `kernel/src/main.rs` — `paging_init` calls
  `arch::init_protections()` and logs the arch-neutral
  `"memory protections enabled"`. Updated the fn-level doc to
  describe the abstraction.
- `tools/xtask/src/main.rs` — QEMU command bumped to
  `-cpu qemu64,+smap,+smep`. Default `qemu64` (no opt-ins) lacks
  SMAP, so `arch::init_protections` would panic at boot; the
  `+smap,+smep` flags add the two features the kernel actually
  requires today. Named CPU models like `Haswell-v4` or
  `Broadwell-v4` were considered but rejected: TCG silently
  drops five features they advertise (PCID, x2APIC, TSC-deadline,
  INVPCID, SPEC-CTRL), generating a noisy warning on every
  boot. See the "minimal CPU model" decision below for the
  underlying principle.
- `docs/spec/user-memory-access.md` — new spec doc covering the
  contract for `UserPtr` types, copy primitives (signatures and
  partial-completion semantics), exception table layout /
  encoding / registration / lookup, SMAP/SMEP discipline, and
  aarch64 portability notes.

Decisions worth recording:

- **`UserPtr::new` returns `Result`, not `Option`.** A misaligned
  address and an out-of-range address are different syscall
  failures (`BadAddress` vs `Misaligned`). Squashing both into
  `None` would lose information that the syscall layer wants to
  forward to the user. The slight verbosity at construction
  sites is fine; both errors stay in the same `UserAccessError`
  type the copy primitives already return.
- **Tags are `PhantomData<*const T>` / `PhantomData<*mut T>`, not
  `PhantomData<T>`.** Auto-trait inference would otherwise make
  `UserPtr<T>` `Send`/`Sync` whenever `T` is, which is more
  permissive than warranted for a kernel-side handle to an
  unverified user address. Callers that need to ferry a
  `UserPtr<T>` across threads must opt in explicitly.
- **The held `u64` is `pub(crate)` only.** Public `as_ptr` /
  `into_raw` methods would let any kernel code dereference user
  memory through ordinary pointer ops, bypassing the SMAP
  window. The discipline that this is the *only* path to user
  bytes lives in `kernel/CLAUDE.md` and is enforced by code
  review; making the field private is a real type-level
  constraint that backs it up.
- **Hard SMAP/SMEP requirement, panic on missing.** Two
  alternatives were rejected: (a) detect at boot and disable
  enforcement on older hardware (each copy primitive gets a
  runtime branch around `stac` / `clac`), and (b) #UD silently
  in the copy primitive (the asm always emits `stac`, the
  panic surfaces only on the first call). (a) trades simplicity
  for hardware support we don't currently care about; (b) gives
  a worse failure mode (cryptic #UD instead of a clear panic at
  boot). The hard requirement is the cleanest choice for a
  Phase 1 OS targeting modern hardware (SMAP shipped on client
  Broadwell and server Haswell-EP, both 2014). Revisit if we
  ever care about pre-Broadwell client CPUs.
- **`-cpu qemu64,+smap,+smep` — minimal CPU model.** Three
  candidates were on the table:
  1. `qemu64,+smap,+smep` — the chosen form. Carries long mode,
     NX, basic SSE; the opt-ins add exactly the user-access
     protections the kernel requires. No TCG warnings.
  2. `Haswell-v4,+smap,+smep` (or `Broadwell-v4,...`) — a
     "realistic" CPU model. Same kernel-visible CPUID modulo
     the dropped features, but emits five "TCG doesn't support
     requested feature" warnings on every boot. The dropped
     features (PCID, x2APIC, TSC-deadline, INVPCID, SPEC-CTRL)
     are things we will want eventually but don't use today.
  3. `max` — "every feature TCG implements". Warning-free
     too, but commits us to "whatever QEMU version X happens
     to expose" — a future TCG improvement could give the
     kernel an implicit feature dependency we didn't intend.
  
  The principle: the xtask command should be a self-
  documenting record of "these are the CPU features the
  kernel currently requires". As future slices add
  dependencies, the command grows by one flag at a time
  (`ArchTimer` will add `+tsc-deadline`, `ArchIrq` will add
  `+x2apic`, etc.) and the commit message explains why.
  This matches the same minimalist discipline the kernel
  applies elsewhere — fallible `KBox` over `alloc::Box`,
  hand-rolled bitflags over the `bitflags` crate, depend on
  what you use and nothing more.
- **`arch::init_protections()` bundles NX + SMEP + SMAP behind
  one arch-neutral entry point.** `main.rs` previously called
  `arch::ensure_nxe()` and `arch::ensure_smap_smep()` directly,
  leaking x86 feature names into the boot path. The wrapper
  hides those names; an aarch64 port's `init_protections`
  will configure `SCTLR_EL1.SPAN` and check `FEAT_PAN`
  instead, with no change to `main.rs`. Cost: one trivial
  function in `arch::x86_64::paging`. Spec doc reworded along
  the same axis ("user-access window" / "user-access
  protection" instead of "SMAP window" / "SMAP / SMEP" in
  generic discussion; the per-arch instructions appear as
  concrete realisations).
- **Inline asm bodies live in `arch/x86_64/user_access.rs`, not
  in `mm/user_access.rs` and not behind a trait.** The mm layer
  is fully arch-neutral; everything x86-specific (`stac`,
  `clac`, `rep movsb`, `lodsb`, the `.pushsection` exception-
  table emission) sits behind the arch boundary. When aarch64
  lands, its raw primitives drop into
  `arch/aarch64/user_access.rs` with the same `copy_bytes_raw`
  / `copy_cstr_raw` shapes and the mm-layer wrappers are
  unchanged. An `ArchUserAccess` trait would add indirection
  without paying back portability today; once two arches exist
  the trait is a small local refactor.
- **Arch primitives return simple signals (`bool` / `CstrCopyOutcome`),
  not `Result<(), UserAccessError>`.** Keeping `UserAccessError`
  in the mm layer means the arch layer has zero upward
  dependencies. The mm-layer wrappers do the `bool → Result`
  / `CstrCopyOutcome → Result<&[u8], _>` translation — a handful
  of lines per primitive, trivial to read.
- **Linear scan in `lookup_recovery`, single-entry per primitive.**
  `rep movsb` is one architectural instruction with one fault
  PC, so one exception-table entry per copy primitive suffices.
  The cstr variant's `lodsb`/`stosb` loop has a single
  user-side fault instruction (`lodsb`) — `stosb` writes
  kernel memory which can't fault in well-formed code, so it
  doesn't need its own entry.
- **`copy_cstr_from_user` returns the slice including the NUL.**
  Linux's `strncpy_from_user` returns the length excluding the
  NUL. Either is a valid convention; including the NUL keeps
  the slice "what was actually written into `dst`" which is
  what callers usually want — `&dst[..k-1]` for the body is
  cheap, and the caller never wonders whether the buffer is
  NUL-terminated.
- **Zero-length copy is `Ok` even at `addr == USER_VIRT_END`.**
  A zero-length range accesses no bytes; it is vacuously valid.
  The bounds check (`addr + len <= USER_VIRT_END`) accepts the
  boundary case because there's no byte at `USER_VIRT_END` for
  it to violate. `UserPtr::new` rejects `addr == USER_VIRT_END`
  for non-zero T sizes; for `UserPtr<u8>` with subsequent
  zero-length copies, the access is genuinely empty.

Verification:

- `cargo xtask test` — host tests pass (152 total, +16 over
  slice 1): `UserPtr` / `UserMutPtr` construction validation
  (user-half rejection, alignment rejection, larger-alignment
  types), `validate_user_range` (boundary handling, overflow,
  zero-length acceptance), and the host stubs round-tripping
  bytes / cstring data through the wrappers.
- `cargo xtask build` — kernel ELF builds clean for
  `x86_64-unknown-none`.
- `readelf -S` + `objdump -s` confirm the
  `.user_access_table` section is now 80 bytes (5 × 16-byte
  entries — one per public copy primitive that the compiler
  monomorphises through the arch raw functions). The recorded
  `fault_pc` and `recovery_pc` values match the `.text` ranges
  of the arch primitives via `nm`.
- Boot under QEMU+OVMF with `-cpu qemu64,+smap,+smep`
  reaches the new diagnostic line "memory protections enabled"
  and continues past it through the paging smoke test —
  `init_protections` accepts the CPU model and the CR4 write
  completed without taking the kernel down. No TCG warnings.

## 2026-05-28 — Phase 1, slice 7: handle table

Phase 1's handle-table slice is in. The handle layer is the
capability-lookup substrate every later syscall will route
through; the kernel-object layer (`KObjectHeader`, `Process`,
`Thread`) that lives behind the entries is the next slice.

Scope landed:

- `kernel/src/libkern/handle.rs` — `RawHandle`, `Rights`,
  `KObjectType` value types per `docs/spec/handle-encoding.md`.
  Pure `const fn` API, no allocator dependency, ready to be
  mirrored to userspace later.
- `kernel/src/handle/` — `HandleTable` with the segmented
  storage, lock-free directory + per-entry seqlocks,
  Fisher-Yates-shuffled freelists, RCU-style deferred
  reclamation, and owner-PID enforcement on every lookup. Public
  API: `try_new`, `allocate`, `lookup`, `close`, `restrict`,
  `duplicate`, `stat`, `quiesce`. 12-step validation algorithm
  implemented in `HandleTable::lookup` matches the spec
  step-for-step.
- `docs/architecture/handle-system.md` — implementation
  overview (referenced from `kernel/CLAUDE.md` and the spec but
  previously missing; now written alongside the
  implementation, same pattern as `user-memory-access.md`).
- `kernel/docs/lock-ordering.md` — rank 3 ("Handle-table
  segment allocation") marked live. New section documents the
  drop-rank-3 → take-rank-6 → reacquire-rank-3 segment-grow
  protocol; this is the only path in the kernel that legitimately
  crosses the boundary.
- `docs/planning/implementation-plan.md` — handle-table
  checklist ticked through; current-status prose updated to
  point at the next slice (kernel-object substrate).

Decisions worth recording:

- **Handles live in their own module, not under `object/`.**
  Handles point at kernel objects but are not kernel objects
  themselves; the handle table is the capability lookup layer.
  Co-locating with the future `KObjectHeader` would muddy that
  distinction. `kernel/src/handle/` is the home; the
  kernel-object substrate gets its own sibling module in the
  next slice.
- **All metadata fields atomic, seqlock on top.** The spec
  describes the entry fields as plain integers under a seqlock.
  We use atomics for every field (`AtomicU32`, `AtomicU64`,
  `AtomicPtr`) and keep the seqlock for *snapshot atomicity
  across multiple fields*. Cost on x86_64: zero (atomic load of
  aligned data is a single `mov`). Benefit: zero `unsafe` in
  the seqlock writer body; the only pointer-through-reference
  writes that remain are in the segment initialiser, which is
  bounded and fully audited.
- **Per-entry `AtomicPtr<()>` `object` field is separate from
  the seqlock-guarded metadata.** Lookup step 6 ("is the
  object non-null?") becomes a single `Acquire` load outside
  the seqlock retry loop, paying for itself by skipping the
  whole snapshot dance for the (very common) closed-handle
  case.
- **`ObjectRef::try_acquire` is a free-function seam, not a
  trait or fn-ptr per entry.** `kernel/CLAUDE.md` requires
  kernel-object dispatch via `match KObjectType` rather than
  `dyn Trait` to keep `HandleEntry::object` 8 bytes (not 16).
  This slice ships `handle::try_acquire_refcount(*mut (),
  KObjectType) -> bool` and `release_refcount(...)` as no-op
  stubs; the next slice rewrites them to dispatch on
  `KObjectType` and bump `KObjectHeader::refcount`. The
  handle-table code itself never changes — the rewrite is
  mechanical, contained to two free functions.
- **Per-segment metadata lives in `Inner`, not inline in
  `SegmentEntries`.** Inlining would make a segment 256 KiB +
  8 bytes, which the buddy would round up to a 512 KiB
  allocation — half wasted per segment, 64 MiB across a full
  256-segment table. The metadata array is 256 × 8 = 2 KiB
  inline in `Inner`, paid once per table.
- **Defer ring capacity 256, fixed at construction.** Allocates
  the backing `KVec<Option<DeferredClose>>` once in `try_new`
  so `close` never calls `kmalloc` under the rank-3 lock (per
  `kernel/CLAUDE.md` § "Forbidden patterns"). On overflow,
  `close` forces an extra `drain_expired` and retries; in
  Phase 1's single-CPU world that always frees because the
  only context that could be in-flight at the snapshot epoch
  is the closing thread itself, which is by construction
  quiescent at the close-syscall boundary.
- **`GraceTracker` keyed by an opaque `current_ctx_id()` shim.**
  Phase 1 (single CPU, no preemption, no `Process`) returns
  0; SMP will return `arch::cpu_id()`; the `Process` slice
  will return `Process::current().ctx_id()`. The mechanism
  doesn't care; replacing the shim is a one-function change
  that doesn't touch the rest of the handle module.
- **PRNG is xorshift64 seeded by the caller.** Production code
  will seed from `RDTSC` at boot; the entropy slice will
  re-seed from `RDRAND/RDSEED` once it lands. Seed quality
  affects only the visible distribution of slot indices —
  never correctness or safety, both of which rely on the
  owner-PID check and 32-bit generation counter.
- **`ClosedObject` wrapper for `close`'s return.** Rust 2021+
  disjoint-closure-captures infers the *field*, not the whole
  struct, when a closure references `tok.0`. With a bare
  `*mut ()` return from `close`, the `Result<*mut (),
  HandleError>` produced inside a closure was enough to mark
  the closure `!Send` because `*mut ()` is `!Send`. Wrapping
  the pointer in `ClosedObject` (with `unsafe impl Send`)
  keeps the API ergonomic and the multi-thread tests
  compiling. Same applied to `LookupOk` for the same reason.

Verification:

- `cargo xtask test` — 223 tests pass (+~70 over the prior
  slice). New tests cover: `RawHandle` encode/decode at field
  corners, `Rights` subset semantics, `HandleEntry` exact 64-
  byte layout, `Xorshift64` distribution, `Segment` Fisher-
  Yates freelist invariants, `GraceTracker` quiesce-and-
  release flow, type-rights compatibility matrix (every type),
  `HandleTable` allocate/lookup round-trip across rights and
  pids, close + reallocate generation bump, generation wrap
  at `u32::MAX`, segment grow at the 4097th handle, restrict
  cannot amplify, duplicate intersects rights and requires
  `DUPLICATE`, the `FAIL_NEXT_ACQUIRE` step-7 failure branch,
  freelist-length-matches-`free_count` invariant, an 8-thread
  cross-pid isolation stress test, and a torn-read torture
  test (one churning writer, four spinning readers, sentinel
  metadata tuple, 50 ms run — no internally-inconsistent
  snapshot ever observed).
- `cargo xtask build` — kernel ELF builds clean for
  `x86_64-unknown-none`. The handle module compiles `no_std`,
  no external crates, only `libkern` primitives.
- Compile-time `const _ = assert!` in `kernel/src/handle/entry.rs`
  pins `HandleEntry` at exactly 64 bytes / 64-byte alignment;
  any future field reorder that drifts this fails the build.
- No QEMU integration test for this slice: handle table is
  pure logic with no hardware dependency. The substrate-works
  milestone (first userspace process via `sys_kprint`) several
  slices later will exercise the handle table end-to-end at
  that point.

---

## 2026-05-28 — Phase 1, slice 8: kernel object infrastructure

The kernel-object substrate that handle entries point at: a new
`kernel/src/object/` module with `KObjectHeader` (atomic refcount +
type tag), the `ObjectRef` RAII reference holder, `match`-on-
`KObjectType` destructor dispatch, and the first two concrete object
types `Process` and `Thread`. This also rewires the handle-table
slice's no-op refcount seam to real refcounting, closing the
`HandleTable::duplicate` TOCTOU.

Decisions worth recording:

- **Refcount ownership model: one ref per live handle, one per
  `ObjectRef`.** The invariant is `refcount == (live handles to O) +
  (live ObjectRefs to O)`; it reaches zero exactly once, firing exactly
  one destroy. `KObjectHeader::new` starts at 1 (the creating handle's
  reference). `allocate` is refcount-agnostic — it *adopts* one
  caller-supplied reference rather than bumping, so the create path
  (`KBox::into_raw` → `allocate`) and the duplicate path (lookup's
  `ObjectRef::into_raw` → `allocate`) are symmetric.

- **`lookup` returns an `ObjectRef`, not a bare `*mut ()`.** Step 7
  bumps the header refcount (`Arc`-upgrade semantics, fail-at-zero);
  step 12 wraps it in an `ObjectRef` that releases on drop. This is what
  closes the `duplicate` TOCTOU: the `ObjectRef` pins the object across
  the `lookup`→`allocate` gap, so a concurrent `close` can drop at most
  the source handle's reference, never the last one. `duplicate`
  transfers that reference straight into the new handle via `into_raw`
  (no decrement in the gap); on `allocate` failure it reclaims via
  `from_raw` + drop. Verified by a multi-thread duplicate-vs-close
  torture test and per-operation refcount-accounting tests.

- **`close` transfers the reference; it does not decrement.** It nulls
  the slot under the seqlock and returns `ClosedObject(*mut (),
  KObjectType)`; the caller reconstructs an `ObjectRef` and drops it to
  release. Keeping the decrement (and therefore the destructor, which
  calls `kfree` under a rank-6 lock) out of `close` keeps it off the
  rank-3 handle-table lock, and makes a racing `lookup` SMP-safe: the
  slot's reference is conceptually live until the caller takes it, so a
  concurrent `try_acquire` sees a positive count (pins) or zero (dying).
  This is the same soundness argument as `Arc`/`Weak::upgrade`, with the
  header's `refcount` atomic as the synchronization object.

- **Destructor dispatch by `match`, not `dyn`.** Because every kernel
  object is `#[repr(C)]` with `KObjectHeader` first, a type-erased
  `*mut ()` reads the header at offset 0 for refcount ops without
  knowing the concrete type; only destruction needs the type, dispatched
  through `match KObjectType` (per `kernel/CLAUDE.md`). Reconstitutes the
  owning `KBox<T>` and drops it.

- **`Process` / `Thread` kept minimal.** Header + identity fields only
  (`Process` also carries a `magic` self-check sentinel for the torture
  tests). No register/FPU state, address space, sched params, or
  Process↔Thread graph — those land with the threading and
  process-management slices, per the "no for-now fields" rule. Rather
  than give them artificial heap-owning fields, destructor-dispatch
  correctness is verified by a `#[cfg(test)]` per-type counter probe in
  `object::header`.

- **ABI hash impact.** `KObjectHeader` is an ABI-critical type
  (`docs/spec/abi-version-hash.md` § "KObjectHeader layout"): `#[repr(C)]`,
  `AtomicUsize` (8) + `KObjectType`/`repr(u32)` (4) + 4 pad = 16 bytes.
  Introducing it changes the kernel ABI version hash; noted here and in
  the commit message.

Verification:

- `cargo xtask test` — 237 host tests pass, including the reworked
  handle-table tests (now using real `Process`/`Thread` objects),
  per-operation refcount-accounting tests, destructor-dispatch tests,
  the `ObjectRef`/`KObjectHeader` unit tests (acquire/release, clone,
  `into_raw`/`from_raw`, fail-at-zero, overflow-guard panic), and
  `concurrent_duplicate_vs_close_toctou_torture`. The concurrency
  torture tests also pass repeatedly under `--release`.
- `cargo xtask build` — kernel ELF builds clean for
  `x86_64-unknown-none`; the `object` module is `no_std`, no external
  crates, only `libkern` primitives.

## 2026-05-29 — Phase 1, slice 9: threading and context switch

Makes the kernel multi-threaded (cooperatively). The `Thread` object gains
its saved kernel register state, owned kernel stack, lifecycle state, and
entry point; a Rust-emitted context switch performs the swap; and a minimal
round-robin scheduler (`kernel/src/sched.rs`) runs kernel threads,
demonstrated by a boot-time worker round-robin on the serial console.

Decisions worth recording:

- **Context switch emitted from Rust (`#[unsafe(naked)]` + `naked_asm!`),
  not NASM.** This supersedes the NASM mention in `CLAUDE.md` and the
  `context_switch.asm` checklist item in the implementation plan.
  Rationale: (a) every other piece of kernel assembly is already
  Rust-emitted (`idt.rs` naked exception stubs, `gdt.rs`, `regs.rs`,
  `user_access.rs`); (b) the build has no assembler integration and adding
  one for a single ~15-instruction function is unjustified; (c) the
  scheduler reaches the saved context through typed Rust accessors on the
  arch layer rather than the hand-maintained numeric offset a separate
  `.asm` file would require; (d) a cooperative switch
  needs no FPU/CR3/RFLAGS handling in Phase 1, so it is short and
  auditable inline. Continues the same reasoning that kept the boot stub
  and exception stubs in Rust (2026-05-13, 2026-05-20).

- **Cooperative, not preemptive.** Phase 1 runs single-CPU with interrupts
  masked everywhere (IF=0, no timer/APIC). A thread runs until it calls
  `yield_now`/`exit`. The switch is the standard xv6/Linux `swtch`: park
  the six callee-saved registers on the outgoing stack, save RSP into the
  outgoing thread, load the incoming RSP, restore, `ret`. Caller-saved
  registers and RFLAGS are not saved — they are caller-clobbered across
  any `call` by the SysV ABI, and there is no interrupt state to preserve.
  The future preemptive path (timer IRQ) is a separate entry that saves a
  full interrupt frame; it does not go through `context_switch`.

- **New threads bootstrap via a fabricated frame + trampoline.** A
  never-run thread's stack top is hand-built so the first switch-in pops
  six zeroed callee-saved slots and `ret`s into a naked `thread_trampoline`
  (which re-aligns and `call`s `thread_enter`); `thread_enter` reads the
  now-current thread's entry/arg and runs it, then calls `exit`. The boot
  context is adopted as the first thread with no fabricated frame — its
  saved stack pointer is written by the first switch-out before it is ever read.

- **Run-queue lock (rank 1) is never held across the switch.** Dropped
  before every `context_switch`, re-acquired fresh on resume; allocation
  and stack reclamation (an exited thread parks itself in `reap` for the
  next scheduler entry to drop) happen outside it. See
  `kernel/docs/lock-ordering.md`.

- **`Thread` field mutation discipline.** `arch`/`state`/`entry`/`arg` are
  mutated through a shared `ObjectRef` via raw-pointer field accessors that
  never form a `&mut Thread` (which would alias the atomically-accessed
  header); sound because single-CPU + only-under-the-runqueue-lock
  serialises all access. No atomics added this slice.

- **FPU/XSAVE and TLS deferred.** The kernel is soft-float and never uses
  the FPU, and userspace (the first real FPU/TLS user) is two slices away,
  so eager XSAVE and `fs_base`/`sys_thread_set_tls` land with the
  first-userspace-thread slice rather than here.

- **ABI hash impact: none.** `KObjectHeader` is unchanged and stays first;
  the new `Thread` fields are internal and not part of the ABI hash
  (`Thread`'s layout never crosses to userspace).

Verification:

- `cargo xtask test` — 252 host tests pass (+14): `KVec::remove`
  FIFO/no-double-drop, the fabricated-frame arithmetic and layout,
  `Thread` constructor/state/saved-sp accessors, and scheduler
  data-structure tests (round-robin dequeue, queue-drop releases each
  thread's reference exactly once via the destructor probe). The real
  `context_switch`/trampoline are not host-tested — they manipulate live
  registers/stacks and are validated under QEMU.
- `cargo xtask build` — kernel ELF builds clean, no warnings.
- `cargo xtask qemu` — serial shows three workers interleaving round-robin
  across three rounds, each exiting cleanly, then `boot thread halting` —
  proving switch-in, rotation, cooperative yield, clean exit, and stack
  reclamation without UAF (the boot thread resumes and runs to the final
  line). Integration coverage is the serial trace until `xtask test-qemu`
  exists.

## 2026-05-29 — Scheduler evolution: cooperative → preemptive (single-CPU) → SMP

Recording the staged plan for the scheduler so the steps are explicit
rather than folded into a single "scheduler matures" item.

The scheduler ships cooperative and single-CPU (slice 9). It must become
preemptive, and eventually multi-CPU. **Preemption and multiple CPUs are
separate concerns and are staged separately:**

1. **Cooperative, single-CPU** — current. A thread runs until it yields or
   exits; correct under Phase 1's interrupts-masked model.
2. **Preemptive, single-CPU** — a new Phase 1 slice (added to
   `docs/planning/implementation-plan.md`), sequenced *after* Timers/clocks
   and `ArchIrq`/APIC because preemption's prerequisites are a periodic
   timer interrupt and an enabled interrupt controller. It introduces the
   `IrqSpinLock` (the `cli`/save-`RFLAGS` variant `spinlock.rs` already
   anticipates), flips the kernel to `IF=1`, and adds a timer-IRQ-driven
   switch path that saves the full interrupt frame and returns via `iretq`.
3. **SMP, multi-CPU** — stays in Phase 3 (per-CPU runqueues, work stealing,
   per-CPU `current` via GS-based per-CPU data, per-CPU APIC timers).

Decisions worth recording:

- **The cooperative `context_switch` is retained, not replaced.**
  Preemption *adds* a second switch path (entered from the timer IRQ, full
  register frame, `iretq` return); voluntary yield/blocking continues to
  use the cooperative naked `context_switch`. Two paths, one `Thread`
  representation.

- **Preemption before SMP, deliberately.** Getting preemption correct on
  one CPU (IRQ-safe locking, quantum, idle thread, the full-frame switch)
  is a self-contained problem; doing it before adding CPUs avoids debugging
  preemption and cross-CPU races simultaneously. The single-CPU
  interrupts-masked model is the thing being retired in step 2; SMP in
  step 3 is then "more of the same, per CPU."

- **Today's global `SCHED`/`current` are explicit single-CPU stand-ins.**
  Step 3 refactors `SchedState` into per-CPU instances, `current` into
  per-CPU data, and points `current_ctx_id()` (the handle-table grace
  shim, currently constant 0) at `arch::cpu_id()`. The cooperative switch
  and the `Thread` layout do not change for that — confirmation the
  current factoring is sound.

- **FPU/XSAVE wiring lands with preemption-era work, into both paths.**
  Eager save/restore (deferred from slice 9, arriving with `ArchFpu`) must
  be added to both the cooperative and the preemptive switch once userspace
  threads can touch the FPU.

No code in this entry — it records the staging only. The preemptive-
scheduling slice is tracked in `docs/planning/implementation-plan.md`
under Phase 1.

## 2026-05-29 — Phase 1, slice 10: syscall entry/exit (+ first ring-3 code)

The x86_64 `syscall`/`sysretq` fast path, a dispatcher + table, the first
syscall `sys_kprint`, and a throwaway ring-3 bootstrap that runs the
project's first userspace code. New `kernel/src/syscall/{mod,table,error}.rs`
and `kernel/src/arch/x86_64/syscall.rs`; GDT and boot wiring updated.

Decisions worth recording:

- **Assembly is Rust-emitted (naked), not NASM** — same as the context
  switch (prior 2026-05-29 entry). The `syscall_entry` stub, `enter_user`,
  and `syscall_debug_exit` are `#[unsafe(naked)] + naked_asm!`, coupled to
  `SyscallFrame`/`CpuLocal` via `offset_of!`.

- **GDT reordered for the SYSRET selector constraint.** `sysretq` forces
  `CS = STAR[63:48]+16`, `SS = STAR[63:48]+8` (RPL 3), so the GDT now runs
  null, kernel code (0x08), kernel data (0x10), **user data (0x18)**, **user
  code (0x20)**, TSS (**moved 0x18 → 0x28**); `GDT_LEN = 7`. `STAR =
  (0x10<<48)|(0x08<<32)`. **ABI-hash impact: none** — GDT selector values
  are not in the kernel ABI version hash.

- **Per-CPU `CpuLocal` + `swapgs` introduced.** `syscall` doesn't switch
  RSP, so the stub `swapgs`es to a per-CPU block (via `IA32_KERNEL_GS_BASE`)
  to find the kernel stack. Phase 1 has a single global `CPU0`. GS model:
  ring 0 runs with `GS_BASE = 0`, `KERNEL_GS_BASE = &CPU0`; the swapgs pair
  brackets the entry window. Nothing else in the kernel uses `gs:`. The
  pre-`swapgs` entry-window #PF/NMI hazard is **accepted** for single-CPU /
  no-IRQ (interrupts masked; the NMI handler uses no `gs:`); the paranoid
  fix (GS-base sign check / IST) is deferred to the IRQ/SMP slice. This is
  the first per-CPU data — the SMP foundation the handle-table grace shim
  (`current_ctx_id`) anticipated.

- **`sysretq` register discipline.** RCX (user RIP) and R11 (user RFLAGS)
  are saved in `SyscallFrame` and preserved across the dispatch `call`;
  caller-saved scratch (RDX/RSI/RDI/R8–R10) is zeroed before `sysretq` to
  avoid leaking kernel values to ring 3; user FS_BASE is never touched.

- **`KError` is `#[repr(i32)]`** in `kernel/src/syscall/error.rs`,
  discriminants per `os-design-v5.1`. Syscalls return `isize` (negative =
  `KError`, non-negative = success). **ABI-hash impact: none** yet — `KError`
  does not cross an `export!` boundary; when userspace `libkern` mirrors it,
  the discriminants become an ABI commitment.

- **Debug syscalls in a high, non-stable range.** `SYS_DEBUG_KPRINT =
  0xFFFF_0000`, `SYS_DEBUG_EXIT = 0xFFFF_0001` — excluded from the v1.0 ABI
  freeze, kept out of the stable sequential (`0..`) space, and removable
  with the throwaway harness.

- **Throwaway ring-3 round-trip harness.** `enter_user` saves a kernel
  resume point, switches CR3 to a user `AddressSpace`, and `iretq`s to a
  hand-assembled blob; `sys_debug_exit` restores the resume point (with a
  manual `swapgs` rebalance, since it skips `sysretq`) and returns into the
  kernel, which restores the boot CR3 before dropping the user AS and
  continues to the framebuffer. Chosen over a halt-on-exit so the demo
  proves a full user→kernel→exit→kernel cycle and pre-stages the scheduler
  return path. Replaced next slice by the ELF `Process` + user thread.

Verification:

- `cargo xtask test` — 260 host tests pass (+8): `KError`/`encode`
  round-trips, `UserAccessError→KError`, `table::dispatch` pure paths
  (unknown→Unsupported, `len==0`→0, `len>max`→TooLarge), GDT user-descriptor
  + STAR-derivation encodings, `offset_of!` asserts on `SyscallFrame`/
  `CpuLocal`. The `syscall`/`iretq`/`sysretq`/`swapgs` round trip is not
  host-testable.
- `cargo xtask build` — kernel ELF builds clean, no warnings.
- `cargo xtask qemu` — serial shows `syscall fast-path armed`, then
  `hello, ring3` (printed by `sys_kprint` from ring 3), then `user demo:
  returned from ring 3 (status 0)` — proving entry, dispatch, the SMAP user
  copy, `sysretq`, and the debug-exit round trip.

## 2026-05-29 — Arch-abstraction boundary made enforceable

Architecture-specific names had leaked into kernel code repeatedly
(`arch::gdt::init`, `arch::idt::init`, `arch::gdt::set_kernel_stack`,
`arch::syscall::init`, and an x86 register `SyscallFrame` in the neutral
`crate::syscall`). `arch::x86_64` was a `pub mod`, so nothing prevented it.
This change makes the boundary compiler-enforced and adds a lint + a
documented convention (`docs/conventions/arch-boundary.md`).

Decisions:

- **Private arch submodule (compiler-enforced).** `arch/mod.rs` now declares
  `mod x86_64;` (private). `crate::arch::x86_64::…` no longer resolves
  outside `arch/` — a hard compile error. The neutral surface is whatever
  `arch/mod.rs` re-exports; re-exporting from a private module is allowed.
  This achieves the "impossible to import arch internals" goal **without a
  separate crate**.

- **Separate-crate option considered and deferred.** A `kernel-arch` crate
  would give a crate-level privacy boundary, but requires first extracting
  shared types (`PhysAddr`/`VirtAddr`/`KBox`/`SpinLock`) into a base crate to
  break the kernel↔arch dependency cycle. Overkill while single-arch; the
  private-module boundary is sufficient. Revisit if/when aarch64 lands.

- **Curated neutral re-exports; jargon wrapped.** Dropped the x86-jargon
  module re-exports (`gdt`, `syscall`). Added neutral free-function wrappers
  `arch::set_kernel_stack` (was `arch::gdt::set_kernel_stack`) and
  `arch::init_syscalls` (was `arch::syscall::init`) in `arch/x86_64/mod.rs`.
  Kept the already-neutral module names `arch::abi`, `arch::user_access`,
  `arch::serial`. **Scope: identifiers/paths only** — x86 terms in *comments*
  (PML4/CR3/RSP) are left as-is; they describe the concrete impl.

- **`SyscallFrame` moved into the arch layer.** The x86 register snapshot
  (r15…rax) and its frame-unpacking `syscall_dispatch` moved from the neutral
  `kernel/src/syscall/mod.rs` into `kernel/src/arch/x86_64/syscall.rs`; the
  dispatcher calls the neutral `syscall::table::dispatch(nr, args)`. The
  neutral syscall module now sees only `(number, args) -> isize`.

- **`cargo xtask check-arch` lint.** Walks `kernel/src/` (skipping `arch/`),
  fails on any non-comment line naming `arch::x86_64` / `arch::aarch64`.
  Wired into CI before build/test. Regression net for comments/doc-links the
  compiler can't catch. Verified it fails on an injected leak and ignores the
  same text in a comment.

- **Dead code surfaced by the boundary.** Making `x86_64` private revealed
  that the word/dword port-I/O helpers (`outw`/`inw`/`outl`/`inl`) were
  unused — they had been "live" only via the leaky public path. Removed them
  (only the byte variants `outb`/`inb`, used by the serial driver, remain);
  trivially re-added when a device driver needs wider port I/O.

Verification: `cargo xtask check-arch` passes (and fails on an injected
leak); `cargo xtask build` clean, no warnings; `cargo xtask test` 260 pass;
`cargo xtask qemu` ring-3 trace unchanged — the `SyscallFrame` move and
neutral wrappers did not change behaviour.

## 2026-05-29 — Phase 1, slice 11: first userspace process (substrate-works)

The Phase-1 milestone: an ELF-loaded process runs in ring 3, calls
`sys_kprint`, and exits — via the real scheduler, replacing the throwaway
ring-3 harness. New `userspace/hello` crate; `Process` owns an
`AddressSpace`; `Thread` gains user-thread launch; the scheduler manages
per-thread CR3.

Decisions worth recording:

- **First program is a throwaway `userspace/hello` crate**, not `init`. A
  minimal `#![no_std] #![no_main]` raw-syscall program (kprint + exit). It is
  the proof of substrate; the real `init` (PID 1, loaded from initramfs)
  comes later. Built as a **static, non-PIE `ET_EXEC`** at a low VA
  (`relocation-model=static` + `-no-pie` + `code-model=small` + a
  `user.ld`/`build.rs`): the kernel ELF loader rejects PIE/`ET_DYN` and
  dynamic interpreters. `xtask` builds it **before** the kernel; the kernel
  `include_bytes!`s the result (`kernel/build.rs` adds `rerun-if-changed`).

- **`Process` owns an optional `AddressSpace`.** `try_new(pid)` keeps making
  an AS-less process (used by the many handle-table/torture-test kernel
  objects — forcing a PML4 per `Process` there would churn the test heap);
  `try_new_user(pid, address_space)` builds a userspace one. `Drop` frees the
  AS automatically via `dispatch_destroy`. (Deviation from the plan's
  non-optional field, for the test-heap reason.)

- **User threads launch through the existing thread/scheduler path.** A
  `Thread` gains a `user_entry: Option<(entry, user_sp)>`, an owning
  `Option<ObjectRef>` to its `Process`, and an `addr_space_root:
  Option<PhysAddr>` (`None` = kernel/boot root). On first run `thread_enter`
  branches: a user thread sets `TSS.RSP0` + the per-CPU syscall stack to its
  own kernel stack and descends via `arch::enter_user(entry, user_sp) -> !`
  (iretq). No refcount cycle: the `Thread` holds the `Process`, not vice
  versa, so reaping the thread frees the process + AS.

- **Scheduler manages CR3 (the linchpin).** On every switch-in the scheduler
  loads the incoming thread's page-table root (process root for a user
  thread, the boot root cached at `init` for kernel threads) before
  `context_switch`. Safe because all kernel stacks live in the shared kernel
  half, mapped identically across roots. This both runs user threads in
  their AS and **restores the boot root before a dying user thread is
  reaped** — its `AddressSpace::Drop` frees the PML4 CR3 would otherwise
  reference. Reaping runs on the boot thread (next scheduler entry), never on
  the dying stack.

- **`SYS_DEBUG_EXIT` repurposed to `sys_process_exit`** (same `0xFFFF_0001`
  debug number): the handler calls `sched::exit()`. The throwaway
  `enter_user(cr3)` / `syscall_debug_exit` / `CpuLocal.resume_rsp` are
  removed; `init(kstack_top)` split into `init_syscall_entry()` (boot) +
  `set_syscall_kernel_stack(top)` (per-thread). New arch ops are exposed via
  neutral `crate::arch` names (`enter_user`, `init_syscall_entry`,
  `set_syscall_kernel_stack`, reused `Paging::set_page_table`); `check-arch`
  stays green.

- **Host-test limits.** `Thread::try_new_user` (like `try_new_runnable`) calls
  `fabricate_frame`, which writes to a kernel-vmap virtual stack top that is
  not real host memory — so it is QEMU-only. The host tests build the
  user-thread fixtures with no kernel stack (the test module names the
  private fields) to exercise the bookkeeping + the no-cycle refcount
  property; the ring-3 run itself is validated by the QEMU serial trace.

- **ABI-hash impact: none** (no `KObjectHeader`/export/discriminant changes;
  `Process`/`Thread` internal layout is not in the hash).

Verification: `cargo xtask check-arch` ✓; `cargo xtask build` clean (builds
`hello` as `ET_EXEC` then the kernel); `cargo xtask test` 265 pass (+5:
`Process` with-AS construction/teardown, user-thread bookkeeping, the
Thread→Process no-cycle release); `cargo xtask qemu` serial shows
`hello from ring 3 (pid 1)` then `init: user process exited; boot thread
resuming`. `readelf -h userspace/target/.../hello` → `Type: EXEC`.

## 2026-06-04 — Phase 1 re-sequencing: infrastructure before blocking subsystems

Reordered the remaining Phase 1 slices in `docs/planning/implementation-plan.md`
after noticing wait queues were ordered before the timers they depend on —
and that the same inversion ran through the whole blocking cluster.

The problem: the async-first model makes `sys_wait` the one blocking
primitive, and **wait queues**, **blocking IPC** (`Block`/`BlockBounded`
send), and **notification/exception delivery** all funnel through it. Those
need: a periodic **timer** (deadlines, `Timer` as a waitable), an
**interrupt controller** (IRQ-driven / DPC wakeup), and a **`Blocked` thread
state** (descheduling). All three were ordered *after* the subsystems that
consume them (arch traits + timers were last; preemptive scheduling, which
brings `IF=1`/`IrqSpinLock`/the `Blocked` state, was dead last).

Decision: move the infrastructure ahead of the blocking cluster. New order
for the remaining slices: handle ops → memory objects → **architecture trait
completion → timers → preemptive scheduling** → wait queues → notifications →
IPC → other syscalls. Handle ops and memory objects stay first (synchronous,
no blocking deps); notifications precede IPC so IPC's dead-peer path has its
`PeerClosed` variant; `process_spawn`/real exit go last (they need IPC
handle-passing and `ChildExited`). The handle table stays **global** (a
single globally-numbered table with per-entry `owner_pid`; per-process
tables are explicitly rejected — `docs/rationale/rejected-approaches.md`);
the handle-ops slice only adds caller-pid resolution in the dispatcher and
wires the `next_owned` owned-handle list for release-at-exit. The Phase 1
milestone is unchanged.

- **`Blocked` state placement.** The `Blocked` thread state + block/unblock
  belong with the preemptive-scheduling slice (it already overhauls the
  scheduler and enables IRQs); wait queues consume them. Pure cooperative
  cross-thread wake (IPC, process-exit) doesn't strictly need preemption,
  but `sys_wait` deadlines need the timer IRQ, so bundling the IRQ-enable +
  `IrqSpinLock` + reschedule machinery before wait queues is the clean split.

- **ArchIrq/Timers scoped to the local APIC for Phase 1 (no ACPI).** Full
  APIC/IOAPIC/HPET enumeration needs ACPI (MADT), which is Phase 2. Phase 1
  only needs a timer interrupt, so `ArchIrq`/`ArchTimer` use the **local
  APIC** (discovered via the `IA32_APIC_BASE` MSR) + **LAPIC timer + TSC,
  PIT-calibrated** — no ACPI. IOAPIC + external-device IRQ routing + HPET are
  deferred to Phase 2 (Phase 1 has no IRQ-driven devices; the UART is
  polled). Recorded in the affected plan slices.

Planning-only change; no code. The plan's existing 2026-05-29 preemptive-
scheduling note is updated to reflect the new position.

---

## 2026-06-04 — Phase 1, slice 12: handle operation syscalls

The four handle operations — `sys_handle_close`, `sys_handle_duplicate`,
`sys_handle_restrict`, `sys_handle_stat` — are now reachable from userspace,
backed by the existing **global** handle table. The table's operations already
existed and were tested; this slice added the plumbing: a single global table
instance, caller-pid resolution, the four handlers, and the `HandleInfo`
boundary type. (`kernel/src/{handle/global.rs, syscall/table.rs,
sched.rs::current_owner_pid, libkern/handle.rs::HandleInfo, main.rs}`.)

Decisions worth recording:

- **First stable syscalls.** These are the first syscalls on the stable,
  sequential-from-`0` ABI numbers (`close=0, duplicate=1, restrict=2,
  stat=3`); the debug syscalls stay in their high `0xFFFF_0000+` range.
  Syscall numbers are not part of the kernel ABI version hash.

- **Global-table singleton without `Box::leak` or a coarse lock.** The one
  `HandleTable` lives inline in a once-init cell (`handle::global` — an
  `AtomicU8` state + `UnsafeCell<MaybeUninit<HandleTable>>`, published
  `Release`/read `Acquire`), initialised in boot after the heap is up and
  before userspace. `Box::leak` is forbidden (`kernel/CLAUDE.md`), and wrapping
  the table in a `SpinLock` would serialise its lock-free seqlock lookups — so
  neither was used. The table hands out `&'static HandleTable`; its own `&self`
  methods provide all interior synchronisation.

- **`restrict` is in-place (spec correction).** `docs/spec/syscall-abi.md`
  previously described `sys_handle_restrict` as "consumes `h`, returns a new
  handle." The implementation (`HandleTable::restrict`) attenuates rights **in
  place** and returns `0`; `h` keeps its value. Source wins
  (`CLAUDE.md` § Status); the spec was corrected to match.

- **`NotOwner → InvalidHandle` for capability hygiene.** The handler error map
  collapses the table's precise `NotOwner` to `InvalidHandle` so a handle owned
  by another process is indistinguishable from one that never existed — a
  caller cannot probe other processes' handle existence by error code. The
  table itself keeps `NotOwner` for telemetry.

- **`next_owned` release-at-exit deferred to the Process slice.** The
  2026-06-04 re-sequencing entry above said this slice would wire the
  `HandleEntry::next_owned` owned-handle list; that is **superseded** — it
  needs a `Process` list-head field and an exit-path walk, which are
  process-lifecycle work. The field stays `RawHandle::NULL` (written on
  allocate, ignored on close) until the Process/`sys_process_spawn` slice. The
  one bounded consequence: any handle pid 1 holds at exit keeps its object
  refcounted until then — none are minted this slice.

- **Sequencing: userspace's first handle is minted by object creation.** The
  handle ops' deliverable is "the operations exist and are correct"
  (host-tested), not a userspace-capability milestone. Userspace obtains its
  first handle by *creating* an object (`sys_memory_create`, next slice), not
  by bootstrap delivery — so the memory slice is where these syscalls first run
  in ring 3 (create → stat/duplicate/restrict/close a real memory handle).
  Inter-process handle delivery (`SpawnArgs.handles`) stays in the final
  "other syscalls" slice, which depends on handle ops + IPC + notifications and
  so cannot move earlier. No re-ordering was needed; no throwaway
  bootstrap-handle code was added (so no arch `enter_user` change).

- **Known ABI tension (not fixed).** `sys_handle_duplicate` returns the new
  handle's bits as `isize`. A handle's top bit is the high bit of the 32-bit
  generation counter; after ~2³¹ reuses of one slot it would set the sign bit
  and userspace would misread a valid handle as a `KError`. Unreachable in
  Phase 1 (and `encode`'s `debug_assert!(v >= 0)` would catch it), but the
  "non-negative = value, negative = error" return convention is in tension with
  a full 64-bit handle. To resolve before v1.0 (e.g. cap generation to 31 bits,
  or change the handle-returning convention).

**ABI-hash impact: none.** `KError`, `KObjectType`, `Rights` bit positions,
and `KObjectHeader` are unchanged; `HandleInfo` is a new boundary type but is
not among the hashed types; syscall numbers are not a hash input.

Verified: `cargo xtask check-arch` clean, `build` clean, `test` (278 host
tests, incl. 13 new handle-syscall/`HandleInfo` tests) green, and `qemu` boots
through `global handle table up` to `hello from ring 3 (pid 1)` with no
regressions.

---

## 2026-06-05 — Phase 1, slice 13: memory objects

The first kernel object userspace can *create*: `MemoryObject` plus
`sys_memory_create` (4) / `sys_memory_map` (5) / `sys_memory_unmap` (6). This
reaches the "userspace can allocate memory now" milestone and is where the
handle-operation syscalls (0–3) first run end-to-end in ring 3 — `hello` now
creates a memory object, maps it, round-trips a byte through the mapped page,
then `stat`/`duplicate`/`restrict`/`close`es the handle. (`kernel/src/object/
memory_object.rs`, `libkern/memory.rs`, `mm/{vmm,addr_space}.rs`,
`syscall/table.rs`, `sched.rs`, `object/{process,thread}.rs`, `userspace/hello`.)

Decisions worth recording:

- **A `MemoryObject` owns its frames.** `create` eagerly allocates + zeroes the
  object's physical frames (a `KVec<PhysAddr>`); the object owns them and frees
  them on its last-ref drop. `map` installs PTEs pointing at *those* frames
  (`AddressSpace::map_object`) and records a `Vma` holding an `ObjectRef`;
  `unmap` removes the PTEs but never frees the frames. So mapping the same
  object twice — or, once a second process exists, in two address spaces —
  aliases the same physical memory. The alternative (anonymous per-mapping
  frames, reusing `map_vma`) was rejected: it would make the handle a mere
  "give me fresh memory" token with no real sharing, requiring rework when
  `sys_process_spawn`/IPC arrive.

- **Per-frame buddy allocation, not one contiguous block.** The buddy caps at
  `MAX_ORDER` (4 MiB) and contiguous allocation rounds to powers of two;
  per-frame (`buddy_alloc(0)` × npages) supports the 16 MiB `MAX_SIZE` and a
  fragmented heap. The map/unmap loops index `frames[i]` per page.

- **`MappingKind::Object` + a `Vma.object` field.** The owning `ObjectRef` lives
  in a new `Vma.object: Option<ObjectRef>` rather than inside `MappingKind`, so
  the enum stays `Copy`/`Eq`. `free_vma_pages` branches on the kind: anonymous
  frees frames, object does not (the `Vma`'s `ObjectRef` drop releases them via
  the object). `unmap_covering` and the address-space `Drop` are thus correct
  for both kinds with no separate unmap path.

- **`find_free_range` + an mmap window.** `hint == 0` ("anywhere") scans the VMA
  tree for the first gap in `[MMAP_BASE = 0x1_0000_0000, DEFAULT_USER_STACK_TOP
  − stack)`, above the ELF image and below the stack.

- **`current_process()` resolution.** A small `sched::current_process()` (clones
  the current thread's `Process` `ObjectRef`) + `Process::address_space()` lets
  the map/unmap handlers reach the calling process's interior-mutable
  `AddressSpace`. This is the shared primitive the sequencing note in the
  handle-ops slice anticipated.

- **TLB flush in the handlers, not the AS layer.** `map_object`/`free_vma_pages`
  issue no privileged instructions (so they stay host-testable against a real
  PML4 via `translate`); the syscall handlers `flush_tlb_all()` after a
  successful map/unmap, since the calling process's address space is active.

- **Syscall-ABI fix: preserve the argument registers across `syscall`.** The
  x86_64 entry stub previously zeroed `RDX`/`RSI`/`RDI`/`R8`/`R9`/`R10` just
  before `sysretq`. Those had already been restored to the user's own saved
  values by the preceding pops, so the zeroing leaked nothing — but it
  destroyed the caller's registers and broke any userspace code making
  sequential syscalls with register reuse (it surfaced as garbage once `hello`
  issued many calls). It also violated the documented contract
  (`docs/spec/syscall-abi.md`: "the kernel saves and restores all other
  general-purpose registers"). Removed the zeroing; every GPR handed to ring 3
  is now the user's own saved value or an intended return (`RAX`/`RCX`/`R11`).
  Userspace syscall wrappers need only declare `RCX`/`R11` clobbered.

- **`MemFlags` reserved.** `#[repr(transparent)] u64`; no flags defined yet, and
  `create` rejects any unknown bit (`from_bits` → `InvalidArgument`) so the slot
  stays forward-compatible.

- **`unmap` ignores `size` (Phase 1).** It unmaps the whole VMA covering `addr`;
  partial/splitting unmap is a TODO. Documented in the spec.

**ABI-hash impact: none.** `KObjectType::MemoryObject = 4` discriminant is
unchanged; `MemFlags` and `Vma` are not hashed types; syscall numbers and the
syscall register convention are not hash inputs.

Verified: `cargo xtask check-arch` clean; `build` clean; `test` (292 host
tests, incl. new `MemoryObject`/`MemFlags`/`find_free_range`/`map_object`
aliasing/`round_up_page` tests) green; `qemu` boots through `global handle
table up` → `hello from ring 3 (pid 1)` → `memory: roundtrip ok` →
`handle-ops ok` → `memory: unmap ok` with no regressions.

---

## 2026-06-08 — Phase 1, slice 14: architecture trait completion

Added four architecture traits the timer/preemption infrastructure depends on,
mirroring the `ArchPaging` pattern (neutral trait module `kernel/src/arch/
<name>.rs`, x86_64 `X86<Name>` impl, neutral re-export in `arch/mod.rs`):
`ArchIrq` (local interrupt controller), `ArchCpu` (feature detection + halt),
`ArchUserAccess` (the SMAP copy discipline as a trait), and `ArchSmp` (a
single-CPU stub). Interrupts stay masked (IF=0) for the whole slice — the LAPIC
is brought up but armed with no source and no IDT IRQ handler.

Decisions worth recording:

- **xAPIC (MMIO), not x2APIC.** The plan first chose x2APIC (MSR-based) for its
  simplicity (no MMIO/uncached mapping). That turned out to be **unusable in
  the project's dev loop**: QEMU under TCG does not emulate x2APIC at all
  (`warning: TCG doesn't support requested feature: CPUID.01H:ECX.x2apic`), and
  the kernel deliberately keeps the QEMU dev loop on TCG (no `/dev/kvm`
  dependency for CI/sandboxes). So `ArchIrq` uses the legacy **xAPIC** MMIO
  controller, which TCG fully supports: read `IA32_APIC_BASE` for the register
  page's physical base, ensure the global-enable bit, map that page uncached
  (`PageFlags::NO_CACHE`) into the shared kernel vmap (`kvmap::vmap_alloc_pages`
  + `Paging::map_page`), and access registers (SVR `0xF0` / EOI `0xB0` / ID
  `0x20` / TPR `0x80`) by volatile MMIO. Still no ACPI/MADT — the controller is
  found from the MSR. (Lesson: validate emulator feature support before picking
  the "simpler" hardware path.)

- **`ArchFpu` deferred** to the preemptive-scheduling slice, alongside its only
  consumer — FPU save/restore in the context switch. Phase-1 userspace is
  soft-float with one user thread, so nothing uses the FPU yet.

- **Refined arch-boundary convention.** A *trait* for each behavioural
  subsystem with genuinely divergent per-arch logic and a neutral consumer
  (paging, cpu, irq, user-access, smp; future timer, fpu); *free functions /
  modules* for naked-asm entry/switch glue (`context_switch`, `enter_user`, the
  syscall entry), stateful singletons (`serial`), and pure data (`abi`). This
  resolves the question of whether everything should be a trait: no — wrapping
  naked stubs and singletons in all-static "namespace traits" is ceremony
  without payoff. `ArchUserAccess` became a trait under this rule (SMAP-vs-PAN
  is divergent, with a neutral consumer in `mm::user_access`); the SMAP asm +
  exception table are unchanged — only the call surface is formalised.

- **Arch-boundary normalization deferred to its own slice.** The existing
  paging-companion free fns (`translate`/`active_root`/`init_kernel_template`)
  and CPU free fns (`init_cpu_tables`/`init_protections`/`set_kernel_stack`/
  `halt_loop`) are *not* folded into `ArchPaging`/`ArchCpu` here — that churny,
  behaviour-preserving refactor of `sched`/`mm`/`main` callers is a dedicated
  "Arch boundary normalization" slice (added to the plan). `ArchCpu` ships
  additive-only this slice (`has_apic`, `halt`). A temporary, tracked split of
  "cpu"/"paging" surface across a trait and free fns is accepted until then.

- **`ArchSmp` is a single-CPU stub** (`cpu_count()==1`, `current_cpu()==0`,
  `send_ipi` → `unimplemented!`). It is *not* wired into
  `handle::current_ctx_id()` or the syscall `CpuLocal` — those stay Phase-3.

**ABI-hash impact: none** — all four traits are internal arch surface; no
`export!` symbols or hashed type layouts/discriminants change. (`CstrCopyOutcome`
was widened from `pub(crate)` to `pub` so the neutral `ArchUserAccess` trait can
name it; it is not a hashed type.)

Verified: `cargo xtask check-arch` clean (incl. the `ArchUserAccess` refactor —
no `arch::x86_64` path in `mm/`); `build` clean; `test` (293 host tests, incl.
the new `ArchSmp` stub test) green; `qemu` boots through `local APIC up (xAPIC,
id 0)` and the hello ring-3 flow (`memory: roundtrip ok` → `handle-ops ok` →
`memory: unmap ok`) with no regressions.
