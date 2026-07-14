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

---

## 2026-06-08 — Phase 1, slice 15: arch boundary normalization

Pure, behaviour-preserving refactor applying the arch-boundary convention (set
in slice 14) to the legacy free functions that pre-dated it. The neutral
`crate::arch` surface now exposes paging and CPU operations uniformly as
trait methods, matching `ArchPaging::map_page`.

- **Paging companions → `ArchPaging`.** `translate`, `active_root`, and
  `init_kernel_template` move from free functions in `arch/x86_64/paging.rs`
  into the `impl ArchPaging for X86Paging` block (their bodies use the
  module-private table-walk helpers, so they stay in that file). Reached as
  `Paging::translate` / `Paging::active_root` / `Paging::init_kernel_template`.
- **CPU control → `ArchCpu`.** `init_cpu_tables` (→ `Cpu::init_tables`, dropping
  the redundant `cpu_` under `Cpu::`), `set_kernel_stack`, `halt_loop`, and
  `init_protections` become `ArchCpu` methods in `arch/x86_64/cpu.rs`. The NX/
  SMAP/SMEP helpers (`ensure_nxe`, `ensure_smap_smep`) and their EFER/CR4/CPUID
  consts move from `paging.rs` to `cpu.rs` (they are CPU-feature enables, used
  only by `init_protections`).
- **`arch/mod.rs`** drops the folded free-fn re-exports; the neutral surface for
  these ops is now the `Paging`/`Cpu` aliases. Callers across `main`/`sched`/
  `thread`/`mm`/in-arch import the trait and use the method form.

Unchanged, per the convention: naked-asm entry/switch glue (`context_switch`,
`enter_user`, syscall entry), the `serial` singleton, and `abi` data stay free
fns/modules. No logic changes anywhere.

**ABI-hash impact: none** — internal arch surface only; no `export!` symbols or
hashed type layouts/discriminants change.

Verified: `cargo xtask check-arch` clean; `build` clean; `test` (293 host tests,
now exercising `Paging::translate`/`active_root` through the trait) green; `qemu`
boot trace byte-for-byte identical (`local APIC up (xAPIC, id 0)` → hello ring-3
`memory: roundtrip ok` → `handle-ops ok` → `memory: unmap ok`).

## 2026-06-08 — Phase 1, slice 16: timekeeping foundation (timers & clocks)

Scoped this slice to the **bare minimum that unblocks the next slice (preemptive
scheduling)**: a monotonic clock plus an `ArchTimer` device. The richer timer
features were deferred because their consumers all arrive later (see below).

- **`ArchTimer` trait + `X86Timer`.** New neutral `arch::timer::ArchTimer`
  (re-exported as `crate::arch::Timer`), backed by the TSC (monotonic ns) and the
  local-APIC timer, both **calibrated against the legacy PIT** (channel 2,
  software-gated and output-pollable — no IRQ needed, which matters because
  interrupts stay masked, IF=0, this whole slice). No HPET/ACPI: the PIT is at
  fixed ports and the LAPIC at its MSR-reported base. Surface: `read_ns`,
  `start_periodic` (preemptive tick), `arm_oneshot_in` (wait-queue deadlines),
  `stop`, `monotonic_hz`/`timer_hz`. The arming methods program the hardware but
  are **dormant** (IF=0) — the countdown is observable via the current-count
  register but never fires; that observable-but-dormant countdown is the QEMU
  smoke test.
- **LAPIC timer in count-down mode, not TSC-deadline.** The QEMU/TCG dev loop
  does not emulate the TSC-deadline timer (the same reason `apic.rs` uses xAPIC,
  not x2APIC — see slice 14's "xAPIC (MMIO), not x2APIC" entry above). Confirmed
  working under TCG: calibration
  reports ~2.3 GHz TSC and ~62 MHz LAPIC timer (≈ QEMU's 1 GHz APIC bus ÷ 16).
- **Invariant TSC: warn, not fail.** `CPUID.80000007H:EDX.8` is checked; a CPU
  that doesn't advertise it (QEMU/TCG doesn't, though its TSC is in fact stable)
  gets a warning, not a halt. On bare metal without the bit the monotonic clock
  could drift with P-states — acceptable for Phase 1, revisit if it bites.
- **TSC→ns conversion** uses a precomputed multiply-shift (`compute_ns_mul_shift`)
  with a u128 intermediate, so `read_ns` needs no per-call 128-bit division and
  cannot overflow for any `u64` TSC delta ((2⁶⁴−1)² < 2¹²⁸−1). Host-tested
  against a u128 reference.
- **`sys_clock_read` = syscall 7, `Monotonic` only.** `Realtime`/`ProcessCpu`/
  `ThreadCpu` return `Unsupported` (Realtime needs a wall-clock offset service;
  the per-CPU clocks need scheduler CPU accounting — neither exists yet). New
  `#[repr(u32)] ClockId` boundary type in `libkern::clock`.
- **Deferred to the wait-queues slice:** the `Timer` kernel object, the kernel
  deadline **min-heap**, and `sys_timer_create`/`sys_timer_set` (which take
  syscall numbers 8/9 on landing). Rationale: a `Timer` can't fire (IF=0 until
  preemptive owns the IRQ stub + IF=1), be waited on (no `sys_wait`), or signal
  (no notification queue) until those mechanisms exist — building them now would
  be untested scaffolding the wait-queues slice has to wire up regardless, so
  they ship with their consumers. Preemptive scheduling (the *next* slice) needs
  only `ArchTimer::start_periodic` + `read_ns`, both of which land here.
- **Naming.** `crate::arch::Timer` (the hardware timer) is a distinct namespace
  from the future `crate::object::Timer` (the waitable kernel object); flagged in
  both module docs.

**ABI-hash impact: none** — a new syscall number (not hashed) and a fresh
`#[repr(u32)] ClockId` value type (not an `export!` symbol, not a hashed
`KObjectType`/`Notification`/`Irp`/`KObjectHeader` layout/discriminant).

Verified: `cargo xtask check-arch` clean (the LAPIC MMIO shims `apic.rs` exposes
to `timer.rs` are `pub(crate)`, never crossing the arch boundary); `build` clean;
`test` (302 host tests — the new mul-shift/`ns_to_ticks`/`ClockId`/`sys_clock_read`
cases included) green; `qemu` shows `timer up: monotonic 2302 MHz, per-CPU timer
62 MHz (clock t0=… ns)` and the hello ring-3 demo prints `clock: monotonic
advancing`.

## 2026-06-08 — Phase 1, slice 17: preemptive scheduling (single-CPU)

Flipped the kernel from "interrupts masked everywhere (IF=0), cooperative" to
single-CPU **preemptive** scheduling driven by the periodic LAPIC timer. The
cooperative `yield_now`/`exit` path is retained; preemption adds a second,
IRQ-driven entry into the same switch core.

- **IF=1 after boot.** Added interrupt-flag control to `ArchCpu`
  (`interrupts_enabled`/`interrupts_disable`/`interrupts_enable`/
  `interrupts_restore`) over new `regs::{read_rflags, cli, sti}`. Boot arms the
  periodic tick (`Timer::start_periodic`) then raises IF, right after the
  scheduler (with its idle thread) is up.
- **`IrqSpinLock`.** New `SpinLock` variant that captures the prior IF and
  `cli`s **before** spinning, restoring on drop (release-before-restore).
  **Audit:** only `SCHED` (rank 1) and `SERIAL` (rank 7) are reachable from the
  timer handler (reschedule + the occasional `kprintln`), and the handler never
  allocates, so only those two migrated; all other locks stay plain `SpinLock`.
  Single-CPU deadlock-freedom: a thread holding an `IrqSpinLock` can't be
  preempted, so the handler never finds it held by the interrupted context.
- **IRQ reuses `context_switch`.** Rather than a separate IRQ-level switcher,
  the returning timer stub builds the full interrupt frame on the kernel stack
  and the handler calls the *same* `context_switch`. The interrupted frame sits
  below the switch's parked callee-saved frame, so a later resume returns into
  the stub epilogue and `iretq`s the original context (incl. IF) back. The
  cooperative and preemptive paths share one `switch_to_next` core.
- **Interrupts masked across the switch.** A timer IRQ mid-`context_switch`
  would corrupt a half-swapped stack. Reconciled with the cardinal "lock dropped
  before the switch" rule via `IrqSpinLockGuard::release_keeping_irqs_masked`
  (release the lock, keep IF=0); the cooperative path restores IF on resume, the
  preemptive path's `iretq` does. Fresh threads (reached via `thread_trampoline`,
  not an `iretq`) `sti` for themselves; user threads get IF=1 via `enter_user`'s
  RFLAGS `0x202` (SFMASK already re-masks IF on the `syscall` back-edge).
- **Quantum + idle.** Scheduler-side `quantum` (one 10 ms tick, `QUANTUM_TICKS`)
  → round-robin reschedule; kept off `Thread` (no layout/ABI change). An idle
  thread (`hlt` with IF=1) runs when nothing is ready and reaps the boot thread;
  it is kept out of the ready/reap sets. `kernel_main` ends by `exit`ing the
  boot thread into idle (not `halt_loop`, which would `cli` and freeze the tick).
- **Spurious vector.** Installed a `0xFF` stub that just `iretq`s (a spurious
  LAPIC interrupt takes no EOI), now that IF=1 makes it possible.
- **Deferred (no consumer):** **FPU/`ArchFpu`** save-restore — kernel is
  soft-float and the single user thread is soft-float, so no thread touches the
  FPU and a preempt→switch→resume cannot corrupt FPU/XMM state; lands when a
  userspace thread can touch the FPU. **`Blocked` state + block/unblock** —
  moved to the wait-queues slice (its only consumer is `sys_wait`); adding it
  here would be dead code. `ThreadState` keeps `Ready/Running/Exited`.

**ABI-hash impact: none** — no `export!` change; no `KObjectType`/`Notification`/
`Irp`/`KObjectHeader` layout or discriminant change; `ThreadState` gains no
variant; `Thread` `#[repr(C)]` unchanged (quantum/idle live in `SchedState`);
new `ArchCpu` methods + `IrqSpinLock` are internal; `ExceptionFrame` is
arch-internal.

Verified: `cargo xtask check-arch` clean; `build` clean; `test` (309 host tests —
new `IrqSpinLock` save/restore + `tick_quantum` cases included) green; `qemu`
shows `preemption armed (IF=1, 100 Hz tick)`, then three CPU-bound workers that
never yield **interleave** their output (`worker 1/2/3 round 0`, `… round 1`, …
— cooperatively worker 1 would have finished first), the ring-3 `hello` runs
preemptibly, and the system idles cleanly at end of boot (no panic, no freeze).

## 2026-06-09 — Phase 1, slice 18: wait queues (`sys_wait` + Timer kobject)

Built the blocking machinery: the `Blocked` thread state + block/unblock,
per-object wait queues, `sys_wait`, and the `Timer` kernel object (deferred from
the timers slice) as the first demonstrable waitable.

- **`Blocked` state + parking.** `ThreadState` gains `Blocked`; a blocking
  thread moves its `ObjectRef` from `current` into a new `SchedState::blocked`
  list (pinning it + its kernel stack), via `block_current_and_switch` — which
  mirrors `switch_to_next`'s IF-bracket exactly but does **not** re-enqueue the
  outgoing thread. `make_runnable` moves it back to `ready` on wake.
- **Single lock domain; direct wakeup (DPC deferred).** Wait-queue state, the
  deadline min-heap, and the blocked parking all live under the rank-1 `SCHED`
  `IrqSpinLock`. The one LAPIC timer is already periodic for preemption, so
  deadlines are checked on the existing 100 Hz tick (~10 ms granularity;
  `arm_oneshot_in` stays dormant) and `on_timer_tick` fires expired timers and
  wakes their waiters **directly** under `SCHED`. A DPC queue would only be
  exercised by this path today and its real consumer (deferring work out of a
  *device* IRQ) doesn't exist until drivers — **deferred**. Rank 2 stays
  reserved in `lock-ordering.md` for the SMP split.
- **No lost wakeup.** Registration (on each object's waiter list + the deadline
  heap) and the block happen under one uninterrupted `SCHED` hold (IF masked by
  the `IrqSpinLock`), so on single-CPU a waker cannot run between them. A
  per-thread `wait_phase: AtomicU8` CAS (`Waiting→Woken`) dedups multiple
  signals for one multi-handle wait (and is the SMP-future backstop); the woken
  thread reads its per-slot `wait_signaled` flags to decide `Signaled` vs
  `TimedOut`, and always unregisters from all objects + the heap on resume.
- **`Timer` kobject + tagged deadline min-heap.** `object/timer.rs`: a waitable
  with `deadline_ns`/`interval_ns` + a pre-reserved waiter list, all interior
  state under `SCHED` (`UnsafeCell`, accessed only via `*mut Timer` while the
  lock is held). The heap (`KVec` binary min-heap in `SchedState`) keys on
  absolute monotonic ns; each `Entry` has an `is_thread` flag distinguishing a
  Timer fire from a `sys_wait` thread-deadline (woken directly → timeout). All
  reserves (blocked, heap, per-timer waiters, per-thread wait slots) are
  pre-allocated — no allocation under the lock.
- **Syscalls 8/9/10.** `sys_timer_create` (mints `WAIT|DUPLICATE|INSPECT|
  TRANSFER`), `sys_timer_set` (absolute deadline; `0` disarms), `sys_wait`
  (`Timer`-only this slice; others → `Unsupported`; `count ≤ MAX_WAIT_HANDLES`
  = 8; poll → `WouldBlock`, deadline → `TimedOut`). New `IoResult` value type
  (`#[repr(C)]`, 16 B) and `TimerFlags`; new `KError::{WouldBlock=-11,
  TimedOut=-12}`.
- **Deferred (no consumer):** the **DPC mechanism**; **Process-exit
  waitability**, **IPC**, **notifications**, **`PendingOperation`** as waitables
  (their objects don't exist); the **intrusive wait-list** (Phase 1 uses
  pre-reserved `KVec` waiter lists + per-thread wait slots).

**ABI-hash impact: yes (per spec; not yet enforced).** Per
`docs/spec/abi-version-hash.md`, the new **`IoResult` layout** (§ "IoOp and
IoResult layouts") and the two new **`KError` variants** (§ "KError enum
layout") would change the kernel ABI hash. The hash is not yet computed in code
(no `export!`/SHA-256 machinery), so nothing is enforced today — noted here for
when it lands. **Unchanged:** `KObjectType` (`Timer = 7` already present — no
discriminant change), `Rights` (`WAIT` already present), `KObjectHeader`.
`ThreadState`/`Thread`/`TimerFlags`/`ClockId`/syscall numbers are
kernel-internal / not hashed.

Verified: `cargo xtask check-arch` clean; `build` clean; `test` (326 host tests —
deadline-heap ordering/remove/over-reserve, `IoResult`/`TimerFlags` layout, the
two `KError` discriminants, `Timer` accessors + dispatch, `Thread` wait
round-trip, `timer_rights` allocatable, `sys_wait`/`sys_timer` arg validation)
green; `qemu` shows the ring-3 `hello` block on a +50 ms timer and wake
(`timer: waited and woke ok`), a poll return `WouldBlock` (`wait: poll empty as
expected`), and a near-deadline `TimedOut` (`wait: timed out as expected`), with
preemption still interleaving and a clean idle at end of boot.

## 2026-06-10 — Phase 1, slice 19: notifications (NotificationChannel + exception delivery)

Built the async notification primitive that replaces Unix signals, plus its
first real producer — CPU exceptions. **Headline:** a ring-3 fault used to
`dump_and_halt` the whole kernel; now it delivers a `Notification` and
terminates just the faulting thread, so the kernel survives.

- **`Notification` value type** (`libkern/notification.rs`): a flat 64-byte
  `#[repr(C)] { kind: u32, body: [u8; 60] }` record with typed constructors that
  write the spec field offsets — **not** a fieldful `#[repr(C, u32)]` enum. The
  flat form *is* the wire bytes (one `copy_to_user`, userspace decodes by
  discriminant), mirroring the `IoResult` precedent.
- **`NotificationChannel` kobject** (`object/notification_channel.rs`, mirrors
  `Timer`): a bounded FIFO (64) + drop counter + waiter list, all under the
  rank-1 `SCHED` lock; waitable (signals empty→non-empty). Overflow:
  exception-category notifications evict the oldest non-exception entry;
  otherwise a drop counter increments and the next recv returns a synthetic
  `NotificationsDropped`.
- **Second `sys_wait` waitable.** `wait_on` now dispatches its three waitable
  ops (`already_signaled`/`add_waiter`/`remove_waiter`) by the object type read
  from the `KObjectHeader` at offset 0 (Timer | NotificationChannel) — no
  signature change. Channel enqueue wakes waiters via the same path as a timer
  fire.
- **Post-mortem exception delivery (decided with the user).** A ring-3 fault
  (`cs & 3 == 3`) → build a `Notification` from the vector (#PF→SegFault with
  the CR2 address + a FaultKind from the error code; #DE→DivideByZero;
  #UD→IllegalInsn; others→SegFault) → enqueue on the faulting process's channel
  + wake its waiters → **terminate the faulting thread by reusing `exit()`**.
  No IDT returning-stub refactor, no suspended state, no `sys_exception_resume`.
  Sound because the exception runs on the faulting thread's own kernel stack, so
  `current` *is* the faulter, and `exit()` already handles
  terminate-current + reap (loading the boot root before the AS is freed). The
  supervisor's channel `ObjectRef` keeps the channel (and the enqueued
  notification) alive past the faulter's reap (the channel does not
  back-reference the `Process` → no cycle). Kernel-mode faults still
  `dump_and_halt`.
- **`sys_notif_recv` = syscall 11.** Ownership-gated (no special right; `WAIT`
  gates blocking via `sys_wait`); pops one notification under `SCHED`, copies the
  64 bytes out **after** releasing the lock; `WouldBlock` if empty. `Process`
  gains a `notification_channel: Option<ObjectRef>` field (kernel-internal, not
  hashed; no cycle).
- **Demo:** the kernel boot thread is a stand-in supervisor — it owns the
  first user process's channel, blocks on it via the in-kernel `wait_on`, and
  reports the `SegFault` when `hello` deliberately faults.
- **Deferred:** the debugger model (suspend + `sys_exception_resume` +
  `Disposition` + `sys_thread_get_registers` + auto-terminate timeout +
  exception-channel priority chain) → needs a userspace supervisor (spawn);
  `ChildExited` producer (spawn + real exit); `PeerClosed` producer (IPC);
  per-process queue-capacity spawn flag; a process recv-ing on its own channel
  from ring 3 (spawn provides the channel handle). The deferred producers'
  discriminants are defined now (ABI).

**ABI-hash impact: yes (per spec; not yet enforced).** The new **`Notification`
layout** is a hash input (`abi-version-hash.md` § "Notification enum layout"), so
it would move the kernel ABI hash — but the hash is not yet computed in code, so
nothing is enforced; noted here (same posture as `IoResult`). **Unchanged:**
`KObjectType::NotificationChannel = 6` already present (no discriminant change),
`KError` (reuses `WouldBlock`), syscall numbers (not hashed), `Process`/`Thread`
(kernel-internal).

Verified: `cargo xtask check-arch` clean; `build` clean (no warnings); `test`
(342 host tests — `Notification` layout/constructors, channel
enqueue/recv/overflow/eviction/dropped-synth/waiters/dispatch, `pf_fault_kind` +
`fault_shape` mapping, `sys_notif_recv` bad-ptr + channel-rights allocatable)
green; `qemu` shows `hello` run its demo, then `hello: triggering a deliberate
fault` → `supervisor: caught SegFault (tid 4, pid 1) @ 0x1000` → `init: user
process faulted and was contained; kernel alive` — the kernel survives a
userspace fault that previously halted it.

## 2026-06-10 — Phase 1, slice 20: IPC (`IpcChannel` + `sys_channel_create`/`send`/`recv`)

Built the IPC channel primitive and its three syscalls — the backbone for
resource servers, the namespace, and (next) process spawn, which passes a child
its initial endpoints *through* IPC, so IPC lands first. Single-process-demoable:
`hello` round-trips a message to itself across a channel it holds both ends of.

- **Endpoint model: two endpoint kobjects, mutual peer pointers.** A channel is a
  *pair* of `IpcChannel` endpoint objects (both `KObjectType::IpcChannel = 5`),
  each owning its own receive ring + recv-waiter list, linked by a mutual raw
  `peer` pointer. Send on S → push into S's peer's ring; recv on R → pop R's own
  ring. **Why two objects, not one:** the spec phrases it as "two endpoint
  handles, separate queues per direction," but a single shared object can't be
  used — a handle→object pointer carries no per-handle tag to tell the two ends
  apart for the asymmetric routing. (The spec is pre-stabilization; this is a
  compatible implementation choice, not a contract change.)
- **`IpcMsg` size reconciliation.** The spec listed `IPC_PAYLOAD_SIZE = 4032`,
  making header + payload + handles sum to `4120 ≠ 4096` — internally
  inconsistent. Pinned the clean one-page layout: `payload = 4096 − 24 − 64 =
  **4008**`, asserted at compile time in `libkern/ipc.rs`. Source wins; the spec
  doc is updated to match. The kernel queues messages in a byte-identical,
  natural-alignment `StoredMsg` twin (page alignment matters only to the
  userspace buffer, not the kernel's heap slots).
- **NoBlock-only send (architecture-forced, not a fork).** The spec has
  `sys_channel_send` return a `PendingOperation` and `Block`/`BlockBounded` block
  via it — but `PendingOperation` is the async-I/O slice, and a bidirectional
  endpoint can't express both a readable and a writable wait edge through
  `sys_wait`'s single signaled bit. So send is `NoBlock` now (returns `0` /
  `WouldBlock`); `Block`/`BlockBounded` → `Unsupported`, deferred to the async
  slice. `recv` fits today's model: `WouldBlock` if empty + `sys_wait` for the
  readable edge, exactly like `sys_notif_recv`.
- **Third `sys_wait` waitable.** Added an `IpcChannel` arm to the three `wait_on`
  dispatch helpers; an endpoint signals when its receive ring is non-empty **or**
  its peer has closed (so a blocked receiver always wakes — to a message or to
  `PeerClosed`). The user copies (4096 bytes each way) run *outside* `SCHED`; the
  empty-poll path peeks under the lock and allocates no bounce buffer.
- **Dead peer: errors now, notification deferred (decided with the user).**
  `send`/`recv` on a peer-closed endpoint return the new `PeerClosed = -13`, and a
  blocked receiver wakes to return it. On close, the endpoint's destructor (under
  `SCHED`) nulls the surviving peer's back-pointer and wakes its receivers; the
  second-to-close sees its own peer already null and skips — no use-after-free,
  serialized by the single-CPU lock. `IpcChannel::Drop` therefore takes `SCHED`;
  sound because endpoint refs are released only outside `SCHED` (handle close,
  the `sys_wait`/lookup `ObjectRef`s). The async `Notification::PeerClosed` is
  deferred to spawn (it needs the channel→peer-process-notification-channel link,
  which only matters cross-process).
- **Handle transfer deferred to spawn (decided with the user).** The `handles[]`
  array / move + duplicate paths exist for cross-process capability propagation,
  which can't be exercised with one process. The send/recv ABI keeps the
  `handles`/`count` parameters for stability, but `sys_channel_send` requires
  `count == 0` (non-zero → `Unsupported`). The handle-table primitives
  (`allocate`/`lookup`/`close` over an arbitrary `owner_pid`) already support it
  when spawn lands.
- **Syscalls 12/13/14** (`sys_channel_create`/`send`/`recv`); send forwards the
  `a4` register (mode). `sys_channel_create` writes two endpoint handles with full
  rights, rolling back on a partial-failure (alloc/copy) without leaking
  endpoints.
- **Demo:** `hello` creates a channel (depth 4), sends `ipc: ping` end0→end1,
  confirms `WouldBlock` polling the empty end0, blocks on end1 via `sys_wait`,
  receives + verifies the payload and `sender_pid == 1`, confirms `WouldBlock` on
  the drained endpoint, closes end0, and confirms `PeerClosed` recv-ing on end1 —
  then proceeds to its existing deliberate fault (still contained).

**ABI-hash impact: yes (per spec; not yet enforced).** `KError` gains
`PeerClosed = -13` (enum discriminants are hash inputs) → moves the hash.
`IpcMsg`/`IpcMsgHeader`/`SendMode` are cross-boundary layouts (hash inputs like
`Notification`/`IoResult`). The hash is still not computed in code, so nothing is
enforced; noted here (same posture as prior slices). **Unchanged:**
`KObjectType::IpcChannel = 5` already present (no discriminant change); syscall
numbers (not hashed).

Verified: `cargo xtask check-arch` clean; `build` clean (no warnings); `test`
(358 host tests — `IpcMsg`/`SendMode` layout, `MsgRing` FIFO/full/wrap,
`IpcChannel` pair/route/drain/already-signaled/waiters/dispatch-destroy/peer-close,
and the `sys_channel_*` validation + endpoint-rights tests) green; `qemu` shows
`hello`'s full IPC round-trip (`ipc: empty-endpoint would-block ok` → `received
message from pid 1 ok` → `drained would-block ok` → `peer-closed detected ok` →
`ipc: ok`), then the deliberate fault still contained — kernel alive.

## 2026-06-10 — Phase 1, slice 21: process spawn + lifecycle + `ChildExited`

The keystone of the Phase-1 finale: `sys_process_spawn` + real exit + the
`ChildExited` producer, delivering the milestone's process/IPC clauses — **two
userspace processes communicate over IPC, both spawned by a parent that learns
of their exits**. (The remaining clause, `sys_exception_resume`, is slice 23; the
IPC-message handle-transfer deferral is slice 22.) This is the first of three
final Phase-1 slices (agreed structure: ① spawn → ② handle transfer + PeerClosed
notification → ③ threads + minimal exception resume/terminate).

- **`sys_process_spawn = 15`** (`SpawnArgs` block; `docs/spec/process-spawn-args.md`,
  new). Builds the child (pid from a new monotonic `SchedState.next_pid`; a fresh
  `AddressSpace` + `load_elf`; a `Process`; its own notification channel), installs
  the parent-supplied initial handles into the **child's** table (cross-pid
  `allocate`, already supported), enqueues the main thread, and returns a child
  `Process` handle (`SIGNAL | TERMINATE`). Atomic-or-fail: any failure before the
  thread is enqueued rolls back every child-side allocation; **move** is
  implemented as duplicate-then-close-source-on-commit, so a failed spawn never
  consumes the parent's handles.
- **Child image = kernel-embedded `ImageId` (decided with the user).** No
  filesystem yet, so spawn-able images are `include_bytes!`d (`embedded_images`)
  and selected by id. Phase 2 swaps the selector for an initramfs path /
  `MemoryObject`.
- **Bootstrap ABI = three argument registers (decided with the user).** No
  argc/argv/auxv exists; `enter_user` is extended to seed `rdi`/`rsi`/`rdx`
  (notification-channel handle / first installed handle / `arg0`) at the child's
  first ring-3 entry, read directly as the child `_start`'s `extern "C"` params.
  Phase 2 replaces this with a stack-resident bootstrap block.
- **Real exit + `ChildExited`.** `sys_process_exit = 16` / `sys_thread_exit = 17`
  (stable; the debug `0xFFFF_0001` is retired) carry a status. `sched::exit` now
  takes an `ExitStatus` and, **before parking the thread**, delivers
  `ChildExited { pid, status }` to the parent's notification channel (stored on
  the child `Process` as `parent_notif`) — immediate, not at reap, so a parent
  blocked in `sys_wait` for its *second* child wakes (deferred-to-reap delivery
  would deadlock). Reuses the `deliver_fault_and_exit` lock discipline. The boot
  parent (pid 1, the root) has no `parent_notif`.
- **Multi-user-thread correctness — two CPU-state bugs the slice surfaced** (a
  single user thread never exercised either):
  - The per-CPU **kernel/syscall stack** (TSS.RSP0 + the syscall-entry stack) was
    set only by `thread_enter` on first descent. A *resumed* user thread then
    trapped onto a stale (sibling's) stack → `#DF`. Fix: re-arm it on **every**
    switch-in (`arm_kernel_stack_for`, called at all three switch sites).
  - A thread that **blocks mid-syscall** (`sys_wait`) is switched away with the
    entry `swapgs` still in effect (`GS_BASE = &CPU0`, `KERNEL_GS_BASE =` its user
    GS). `enter_user` does no `swapgs`, so a different thread's *first* descent
    inherited a stale `KERNEL_GS_BASE`, and its first `syscall`'s `swapgs` loaded
    `GS_BASE = 0` → the entry stub's `gs:` access faulted (`#PF` → `#DF`). Fix:
    re-assert `KERNEL_GS_BASE = &CPU0` at first descent (`arm_user_entry_cpu_base`,
    via the neutral `crate::arch` interface).
- **Boot rework:** `run_first_userspace` now boots the **parent** (pid 1) with a
  notification-channel handle in `rdi` and hands off; the parent is the
  supervisor (not the kernel boot thread). New `userspace/parent` + `userspace/child`
  crates (embedded; built by `xtask`); `hello` remains a workspace member but is
  no longer the boot image.

**Punted past Phase 1 (decided with the user):** FPU `XSAVE` save/restore + TLS
(`sys_thread_set_tls`) — userspace is soft-float, so nothing touches the FPU even
with many processes (no consumer); DPC, the `test-qemu` harness. Documented as
consumer-gated, out of the Phase-1 milestone.

**ABI-hash impact: yes (per spec; not yet enforced).** `SpawnArgs` is a new
cross-boundary layout (hash input, like `IpcMsg`/`Notification`); `ChildExited` /
`ExitStatus` were already defined. Syscall numbers 15–18 are not hashed. The
debug `SYS_PROCESS_EXIT = 0xFFFF_0001` is retired for stable 16. The hash is still
not computed in code → not enforced; noted (same posture as prior slices).

Verified: `check-arch` clean; `build` clean (no warnings); `test` 362 host tests
(`SpawnArgs`/`ImageId` layout, `child_exited` offsets, spawn bad-pointer reject)
green; `qemu` shows the parent spawn two children that exchange an IPC message
(`child[recv]: got a message: child: ping from the sender`) and the parent reap
both via `sys_wait` (`child exited pid=2 code=0`, `pid=3 code=1`), then exit —
the Phase-1 milestone's spawn/IPC/`ChildExited` clauses, end to end.

## 2026-06-10 — Phase 1 finale, slice ②: IPC handle transfer

The second of the three final Phase-1 slices: message-borne **handle transfer**,
the capability-propagation core of IPC. Slice ① deferred the `sys_channel_send`
`count > 0` path (`Unsupported`); now that spawn exists, two real processes can
exchange capabilities, so this implements it end to end.

- **Always move (per the spec).** There is no move/duplicate flag in a message —
  a sender that wants to keep a copy `sys_handle_duplicate`s first and sends the
  duplicate. So `sys_channel_send` moves the listed handles to the receiver.
- **References pinned "in flight"; installed at recv.** At send time the receiver
  isn't known (any process holding the peer endpoint may recv), so the kernel
  can't install into the receiver's table at send. Instead each transferred
  object's reference is carried **in the queued message** and installed at
  `sys_channel_recv` (the receiver is `current` then — `allocate(recv_pid, …)`,
  reusing the spawn slice's cross-pid install). Mechanically: `MsgRing`'s element
  became a `RingSlot { msg: StoredMsg, transfers: [Option<TransferRef>; 8] }`
  (non-`Copy`); `push_from`/`pop_into` copy the bytes and **move** the
  `TransferRef`s (a `mem::take`) — sound under `SCHED` because only `ObjectRef`
  *Drop* is forbidden there, and a move never touches a refcount. The receiver
  installs (and any error/undelivered transfer drops the refs) **outside** `SCHED`.
- **Atomic-or-fail, move-on-commit.** Send validates + pins every handle
  (`lookup` with `TRANSFER`) before touching anything; the sender's source
  handles are closed only **after** the message is queued, so a `WouldBlock` /
  `PeerClosed` send loses no capability. Recv installs each transfer and rolls
  back (closes what it installed) on a partial failure; a recv-install OOM/fault
  loses the already-dequeued message (a documented Phase-1 edge). Undelivered
  transfers (endpoint destroyed with messages queued) release their refs when the
  ring drops, in destructor context outside `SCHED`.
- **`sys_channel_recv`** now writes the receiver-side handle values into both the
  separate `handles` out-buffer and the in-message `handles[]`, plus the count.
- **`PeerClosed` notification deferred to Phase 2 (decided with the user).** The
  dead-peer *error* path already ships; the async notification needs a per-endpoint
  holder→notification-channel link that updates on every endpoint transfer, and
  the spec's "every holder" delivery wants handle duplication + a holder registry
  — a Phase-2-shaped design with no Phase-1 milestone driver.
- **Demo:** child A creates a `MemoryObject`, maps it, writes `0xC0FFEE`, and
  **transfers the handle** to sibling B over their channel; B receives the handle,
  maps the same object, and reads the marker back — proving the capability crossed
  the process boundary and aliases shared frames.

**ABI-hash impact: none.** `TransferRef`/`RingSlot` are kernel-internal; `IpcMsg`
already defines `handles`/`handle_count`; no `KError` change; syscall numbers
unchanged.

Verified: `check-arch` clean; `build` clean (no warnings); `test` 364 host tests
(+ `MsgRing` moves `TransferRef`s and releases an undelivered transfer on destroy;
the send mode/oversize-count rejects) green; `qemu` shows
`child[send]: transferred a memory object to the sibling` →
`child[recv]: mapped transferred object, marker=0xc0ffee ok`, both children exit
`0`, the parent reaps both `ChildExited` — handle transfer end to end.

## 2026-06-10 — Phase 1 finale, slice ③: threads + exception suspend/resume (Phase 1 complete)

The last Phase-1 slice closes the milestone's final clause: a ring-3 fault now
**suspends** the faulting thread (rather than terminating it post-mortem), and a
supervisor resumes or terminates it via `sys_exception_resume`. It also adds the
threads a supervisor needs (`sys_thread_create`) and makes multi-threaded process
exit correct.

- **Suspend = block-and-switch reuse.** A ring-3 fault enters the exception stub
  on the faulting thread's own kernel stack; the stub builds the `ExceptionFrame`
  there and `call`s the dispatcher. "Suspend" is then just parking the thread (a
  new `Suspended` scheduler state) and context-switching away — mechanically
  identical to `block_current_and_switch` — leaving the `ExceptionFrame` on the
  frozen kernel stack. `sys_exception_resume` makes the thread runnable; it
  resumes *up through the dispatcher* to the stub, which pops the frame and
  `iretq`s. No special trampoline; `suspend_with_fault` returns the supervisor's
  `ResumeDisposition` to the dispatcher when the thread next runs.
- **Uniform suspend — all user faults.** The general exception stubs used to end
  in `ud2` (dispatcher `-> !`). They now share the `#PF`/timer stubs' pop+`iretq`
  epilogue, `exception_dispatch` returns `()`, and both dispatchers route the
  user-fault branch through one helper (`user_fault` → `suspend_with_fault`). The
  epilogue is only *reached* on the Resume path (kernel faults diverge in
  `dump_and_halt`; Terminate diverges in `exit_thread`), so it is a no-op for
  everything else. Every user-fault vector is suspendable, matching the
  os-design "faulting thread suspended" model.
- **`sys_thread_create` + the thread-as-supervisor model.** A process can now
  hold more than one thread; the returned `Thread` handle
  (`SIGNAL | TERMINATE | INSPECT | DUPLICATE`) is the capability a sibling uses to
  inspect/resume a faulted thread. The caller owns the user stack (allocate +
  map, pass the top). `spawn_user` now returns the new `ObjectRef` (a clone for
  the handle) instead of just a tid.
- **`exit_thread` / `exit_process` split.** `exit_thread` terminates the current
  thread and delivers `ChildExited` only if it is the process's **last** thread
  (a sibling scan finds none). `exit_process` (behind `sys_process_exit`) tears
  down the siblings first — an `owner_pid` scan of `ready`/`blocked`/`suspended`,
  unregistering blocked siblings from their wait objects + the deadline heap
  before reaping — then exits the current thread with `ChildExited`. The `reap`
  slot became a `KVec` (a process exit reaps many threads at once), drained
  outside `SCHED`.
- **Full teardown via an `owner_pid` scan, not a per-process thread list.** The
  intrusive per-process thread list (with its enumeration / external-process-kill
  consumer) lands in Phase 2; the scan is correct for self-exit now (single-CPU,
  all sibling threads are parked off the run queue while this one runs) and is not
  a dead-end (decided with the user).
- **Disposition = Resume + Terminate only.** `sys_exception_resume` takes a raw
  `u64` disposition (`0` Resume / `2` Terminate); `ResumeSkip` / `ModifyAndResume`
  (`1` / `3`), the 30 s auto-terminate timeout, and the debugger
  exception-channel priority chain stay Phase 2. `Resume` is return-to-retry, so
  the **demo uses Terminate** (without fault-fixing, Resume re-faults); Resume is
  exercised at the data-structure level.
- **`ArchRegisters` trait for the register snapshot.** `RegisterValues` is both
  an ABI type (written to userspace by `sys_thread_get_registers`) *and*
  arch-specific in content (the x86_64 register set), so it lives behind the arch
  boundary, not in `libkern`: the neutral `arch::registers::ArchRegisters` trait
  (`type Values; fn read_from_exception_frame(frame_ptr) -> Values`), with the
  x86_64 `RegisterValues` type + `X86Registers` marker in
  `arch/x86_64/registers.rs` (re-exported as `crate::arch::{RegisterValues,
  Registers}`). The `impl` stays in `idt.rs` — where the private `ExceptionFrame`
  it decodes is defined — so reading a suspended thread's registers needs no
  widening of that frame's visibility. `ThreadArgs` stays in `libkern` (it is
  arch-neutral: `entry`/`user_sp` are user VAs, `arg0` opaque). Register
  *writeback* (for the deferred `ModifyAndResume`) joins this trait in Phase 2.
  (The reader was first landed as a free `arch::user_registers_from_frame`
  function with the type in `libkern`; promoted to the trait the same day so the
  arch-specific type sits with its arch logic.)
- **Demo:** the `parent` process (PID 1) maps a worker stack, `sys_thread_create`s
  a worker whose entry deliberately faults, receives the `SegFault`, reads the
  worker's registers (`sys_thread_get_registers`), prints the faulting `rip`, and
  terminates it (`sys_exception_resume` with Terminate) — then runs the existing
  2-child spawn/transfer demo and `sys_process_exit`s.

**ABI-hash impact: yes (per spec; not yet enforced).** New cross-boundary layouts
`RegisterValues` (144 B) and `ThreadArgs` (64 B) are hash inputs (like
`SpawnArgs` / `IpcMsg`). Syscall numbers 19–21 are not hashed; no `KError` /
discriminant change. The hash is still not computed in code, so this is not
enforced — same posture as prior slices.

**Phase 1 milestone met.** The kernel substrate is complete: capability handles,
per-process address spaces, async-shaped syscalls, IPC with handle transfer,
process spawn + lifecycle, and now multi-threading with supervised exception
handling.

Verified: `check-arch` clean; `build` clean (no warnings); `test` 372 host tests
(+ `ThreadState::Suspended` transitions, resume sets disposition, the
`exit_process` sibling-scan/teardown, the reap-list draining many, and the
`sys_thread_create` / `sys_thread_get_registers` / `sys_exception_resume`
validation rejects) green; `qemu` shows
`parent: worker faulted @ rip=0x…400005 ; terminating` →
`parent: worker terminated`, then the spawn/transfer demo
(`child[recv]: …marker=0xc0ffee ok`, both `ChildExited`), and the kernel stays
alive past `sys_process_exit`.

---

## 2026-06-11 — Phase 1 stock-take: Phase 1.5 hardening pass + Phase 2 re-sequencing

A take-stock pass after Phase 1 completed: a four-subsystem code-quality audit
(memory, arch, scheduler/objects, handle/syscall) plus a Phase 2 dependency
analysis. The audit found Phase 1 structurally sound — no CRITICAL/HIGH
soundness bug, ~100% `SAFETY`-comment coverage, correct lock discipline and
atomic orderings — so this is a polish-and-harden pass, not a rescue. Two
outputs: a **Phase 1.5 hardening pass** (this code change) and a **Phase 2 plan
restructure** (doc only; see `docs/planning/implementation-plan.md`).

### Phase 1.5 — code-quality hardening

Each item is a fix the audit surfaced; all gates stayed green throughout.

- **Handle value can no longer alias a `KError` (`RawHandle` encoding).** The
  generation counter occupied bits 63:32, so once a slot's generation reached
  `0x8000_0000` the handle's bit 63 set and `handle.bits() as isize` read back
  *negative* — indistinguishable from a `KError` in the syscall result
  register. **Bit 63 is now reserved zero; the generation is 31 bits**
  (`GENERATION_MAX = 0x7FFF_FFFF`), so every issued handle is a non-negative
  `isize`. Normative spec updated (`docs/spec/handle-encoding.md`).

- **Generation overflow is now design-enforced, not assumed.** The old counter
  wrapped (`wrapping_add`), so after `2³²` reuses of one slot a stale handle
  could re-validate and alias a different object (classic generation ABA). The
  audit/user flagged that wraparound was never actually handled. New policy:
  **a slot is retired — removed from the freelist permanently — when its
  generation reaches `GENERATION_MAX`** (`HandleTable::drain_expired`, the sole
  point a used slot returns to the freelist). The generation is therefore
  *strictly monotonic and non-repeating per slot*, a toolchain-checkable
  invariant rather than a probabilistic argument. Cost: one slot leaked per
  `2³¹` reuses (tracked in per-segment `retired` so live-handle accounting
  stays exact) — negligible. Reserving the sign bit *halved* the reuse budget
  (`2³²`→`2³¹`), so the aliasing fix and the overflow policy are coupled and
  landed together. Host test `slot_is_retired_at_generation_max_and_not_reissued`
  drives a slot to the cap (via a test-only generation poke) and proves
  retirement + non-negativity; the prior `generation_wraps_at_u32_max` test
  (which asserted the *removed* wrap behavior) is replaced.
  **ABI-hash impact:** `RawHandle` layout (still `repr(transparent)` `u64`) is
  unchanged, but the *semantics* of bit 63 and the generation width changed —
  a pre-stabilization ABI-semantics change, noted here per the spec posture
  (hash still not computed in code).

- **`kernel/CLAUDE.md` FPU claim corrected.** It stated "User FPU state is
  saved/restored on context switch" — false: `context.rs` saves no FPU state
  and userspace is soft-float. Reworded to match reality (XSAVE is
  consumer-gated, lands with the first hard-float userspace thread). The claim
  would have misled the first real userspace work in Phase 2.

- **Buddy bitmap region over-skip (`mm/buddy.rs`).** `find_bitmap_region`
  validated a candidate against `bitmap_bytes` while `new()` reserved the
  page-rounded `bitmap_pages * PAGE_SIZE`; a region sized between the two left
  `bitmap_phys_end` past the entry, and the pass-2 skip check then stripped
  frames from the *next* usable entry. Now searches for the page-rounded
  reservation size.

- **`sys_channel_recv` handle-count guard (`syscall/table.rs`).** The handler
  trusted the stored message's `handle_count` before indexing fixed-size
  `[_; IPC_HANDLE_MAX]` buffers and slicing `hbytes[..n*8]`; a corrupted count
  `> IPC_HANDLE_MAX` would panic the kernel (the `.min()` masked but did not
  reject it). Now explicitly rejects with `KError::KernelError` after dequeue
  (the in-flight `transfers[]` drop on return, reclaiming references).

- **`cld` added to the user-copy asm (`arch/x86_64/user_access.rs`).**
  `copy_bytes_raw`/`copy_cstr_raw` use `rep movsb`/`lodsb`/`stosb`, which
  require `DF=0`. This held only because the syscall entry path clears DF via
  `SFMASK`; a Phase-2 `#PF` handler or DPC copying user memory runs with
  arbitrary DF and could copy backwards, corrupting memory. A `cld` at the top
  of each block closes the window cheaply (verified safe today; hardening for
  the new callers Phase 2 introduces).

- **Refactor: `lookup_typed` helper (`syscall/table.rs`).** The
  lookup-then-type-check idiom (`lookup(...).map_err(map_handle_err)?; if
  object_type != EXPECTED { InvalidArgument }`) was copy-pasted across 8
  handlers. Folded into one `lookup_typed(h, pid, required, expected)` so the
  type-confusion check cannot be forgotten on a new handler. No behavior change.

- **Refactor: `switch_into` scheduler switch-core (`sched.rs`).** The
  IF-bracket + re-arm-kernel-stack + CR3-load + `context_switch` +
  interrupt-restore tail was duplicated across `block_current_and_switch`,
  `switch_to_next`, `finish_exit`, and `suspend_with_fault` — the most
  safety-critical code in the kernel, where a future edit updating three sites
  but not the fourth is a latent `#DF`/corruption. Factored into one
  `unsafe fn switch_into(g, out_slot, next_obj)` that consumes the guard;
  callers now only re-home the outgoing thread and install the incoming one.
  `finish_exit`'s terminal path simply never reaches the (now-shared) restore.
  Behavior-preserving — **verified by the QEMU round-trip**, which is
  byte-identical to the prior Phase 1 output and exercises all four paths
  (preemptive worker round-robin → `switch_to_next`/`finish_exit`; the
  fault→suspend→resume(Terminate) → `suspend_with_fault`; `sys_wait` on the
  channel → `block_current_and_switch`).

**Deliberately deferred (low value / poor risk-reward):** the triplicated IDT
GPR-save/`iretq` epilogue (`idt.rs` stub macro / `vec14` / timer stub) — risky
exception-entry asm surgery for a maintainability-only win, rated LOW by the
audit; and lifting the duplicated `FakeMem` test helper (`buddy.rs`/`slab.rs`)
into `mm::test_support` — trivial but low-value test-only churn. Both remain as
tracked cleanup if a future change touches that code.

Verified: `check-arch` clean; `build` clean (no warnings); `test` **375** host
tests (was 372; +3 handle-generation tests, the wrap test replaced) green;
`qemu` round-trip byte-identical to Phase 1 (`worker faulted @
rip=0x…400005 ; terminating` → `worker terminated` → spawn/transfer demo →
both `ChildExited` → alive past `sys_process_exit`).

### Phase 2 re-sequencing (plan doc only)

The dependency analysis found the Phase 2 plan assumed infrastructure that was
never scheduled, plus one internal inversion. Corrected in
`docs/planning/implementation-plan.md`:

- **The "async-I/O slice" was referenced but undefined.** Multiple slices defer
  `PendingOperation` + blocking IPC send "to the async-I/O slice," but no slice
  built it. Every block-device read (AHCI → fs-server → page cache) needs it.
- **Device IRQs need an IOAPIC, which needs ACPI MADT parsing**; PCI ECAM needs
  the ACPI MCFG table. Phase 1 shipped LAPIC-only and deferred IOAPIC "to Phase
  2" without a slice. (This is the small pure-Rust table-parsing layer, distinct
  from the ACPICA/AML work gated separately in `why-phased-acpi.md`.)
- **The DPC/softirq queue** (Phase-1-deferred until a device-IRQ consumer
  exists) and a **demand-paging `#PF` handler + `MappingKind::FileBacked`**
  (the current `#PF` is exception-table-only; page cache is impossible without
  fault-in) were both assumed but unscheduled.
- **Entropy** was both its own slice and an item inside the in-kernel-RS slice
  (`/dev/entropy`) — a forward self-reference.
- **FAT was justified as "required to boot Limine"** — false (UEFI/Limine read
  the ESP, not Nitrox); nothing in the Phase 2 milestone consumes it.

Fix: a **prerequisite band** (architecture docs for namespace/RS and
drivers/IRPs; ACPI table parser; IOAPIC; DPC queue; demand-paging `#PF` +
`FileBacked`; `PendingOperation`/async-I/O + `Block` IPC modes; DMA allocation)
now precedes slice 1; Entropy moved ahead of the in-kernel resource servers;
slices renumbered with explicit dependency notes; the FAT justification
corrected; init clarified as a *bootstrapping* form (milestone-complete only
once storage/fs/page-cache land). Milestone unchanged. These prerequisites are
genuine Phase 2 feature work, distinct from the Phase 1.5 code hardening above.

No commit/push/PR performed — left for the user to review.

### 2026-06-11 addendum — generation overflow: retirement → wrap

On review, the generation-overflow decision above (retire a slot at
`GENERATION_MAX`) was reconsidered and **reversed in favor of wrapping**. The
trade was re-examined with the actual numbers and threat model:

- **Retirement is a permanent, global, unprivileged resource leak.** The handle
  table is global; any process can drive a slot's generation with a trivial
  `open`/`close` loop, and the LIFO freelist concentrates that on one slot. At
  an aggressive sustained 1M handle-ops/s a slot retires every ~36 min, a
  4096-slot segment in ~102 days, the whole 256-segment table in ~71 years.
  Not a *practical* exhaustion, but it is permanent global degradation reachable
  by unprivileged code — the wrong shape for a long-lived system.
- **Wrapping's ABA is far weaker than it first appears.** A wrapped generation
  can only re-validate a stale handle after `2³¹` reuses of the *same* slot
  while that handle is held unused, **and** the `owner_pid` check (step 10)
  confines the confusion to the *same owning process* — a within-process
  correctness hazard, not a cross-process escalation. It is outside the
  capability threat model and unreachable in practice.

So the residual hazards are asymmetric: retirement gives a reachable-by-trivial-
code global leak; wrapping gives a non-reachable, within-process-only ABA.
Wrapping is steady-state (never loses a slot), removes code (the retirement
check + the per-segment `retired` counter), and matches what production
capability systems do. **Decision: wrap the generation modulo `2³¹`** (mask to
31 bits, which also keeps the reserved bit 63 clear). The bit-63 reservation /
non-negative-`isize` fix is independent and stays. `docs/spec/handle-encoding.md`
§ "Wraparound at `GENERATION_MAX`" updated; the retirement test replaced by
`generation_wraps_at_max_without_retiring_the_slot`. Host tests green (374).

---

## 2026-06-11 — Phase 2 prereq: drivers-and-IRPs architecture doc

First Phase 2 prerequisite item (doc-only). Wrote
`docs/architecture/drivers-and-irps.md`, the IRP / interrupt / completion
contract that the rest of the prerequisite band (IOAPIC, DPC queue,
`PendingOperation`) and the storage slice (PCI/AHCI/GPT, `DeviceNode`,
`InterruptObject`) implement against. The plan listed it as a prereq because the
storage slice cites the file but it never existed. It distills the authoritative
design in `os-design-v5.1.md` § "Driver Subsystem" and reconciles it with what
Phase 1 actually built.

Load-bearing decisions recorded:

- **Execution contexts `IRQ > DPC > Thread`.** ISR does the minimum (ack +
  queue a DPC / signal a waitable); DPCs run completion work non-blocking, above
  thread priority, with inline `DpcNode`s (no heap alloc on the completion fast
  path); threads initiate IRPs and block in `sys_wait`.
- **`InterruptObject` is a waitable.** The ISR signals it and a driver thread
  blocked in `sys_wait` wakes — one programming model for in-kernel (Tier 1) and
  future userspace (Tier 2) drivers. Two patterns over the same ISR→DPC base:
  block-on-`InterruptObject` (primary) and DPC completion routine (in-kernel).
- **Async completion via `PendingOperation`.** The IRP owns one; completing the
  IRP signals it; immediate completions return a pre-signalled handle so callers
  have one code path. It is a new waitable on the **Phase-1** wait machinery, not
  v5.1's intrusive `WaitNode` list.
- **Discovery: ACPI MCFG/MADT → `DeviceNode` → Tier-1 matching** (a built-in
  table) for Phase 2. `sys_device_map_mmio` returns a `MemoryObject` over a BAR.
- **Two Phase-1 reconciliations made explicit in the doc:** the waiter mechanism
  (fixed-array waiter list + 8-slot `Thread` wait array, `match KObjectType` in
  `sched.rs`), and the DPC (deferred in Phase 1 — timer tick wakes directly;
  migrates onto the DPC queue when `phase-2/dpc` lands).

Deferrals recorded in `docs/rationale/deferred-decisions.md` (new "Drivers and
interrupts" section): Tier 2 / LKM loading, MSI/MSI-X, shared INTx chaining,
IOMMU + userspace drivers, IRP cancellation + 30 s timeout, filter drivers,
NVMe, writeback IRPs. Phase 2 uses IOAPIC-routed non-shared interrupts and
shallow IRP stacks.

No code; no ABI impact (Markdown only). Branch `phase-2/drivers-irps-doc` off
`main` (which now includes the merged phase-1.5, PR #29). This is item 1 of the
Phase 2 prerequisite band; the implementing items (`phase-2/acpi-tables`,
`phase-2/ioapic`, `phase-2/dpc`, `phase-2/pending-operation`, then the storage
slice) follow per `docs/planning/implementation-plan.md`.

---

## 2026-06-11 — Phase 2 prereq: ACPI table parser behind `ArchPlatform`

Second Phase 2 prerequisite item (`phase-2/acpi-tables`). Adds the pure-Rust
ACPI static-table parser (RSDP → XSDT/RSDT → MADT + MCFG; no AML), the firmware
discovery the IOAPIC and PCI-enumeration items depend on.

**The boundary decision (the substance).** ACPI is x86-specific (aarch64 uses a
Device Tree Blob), so it lives behind the arch boundary. The load-bearing
question was *what crosses as neutral facts vs. what stays arch-internal*. The
split:

- **Neutral** (new `ArchPlatform` trait, `arch/platform.rs`, re-exported as
  `arch::Platform`): only the **PCIe ECAM regions** (`EcamRegion { base, segment,
  bus_start, bus_end }`), via `Platform::pcie_ecam_regions()`. PCIe config space
  is a PCI-SIG standard identical across arches; only *where the ECAM window is*
  differs (ACPI MCFG vs. DTB), so the neutral PCI enumerator (storage slice) can
  consume this and build arch-independent `DeviceNode`s. ("ECAM"/"PCIe" are
  bus-standard terms, not arch jargon.)
- **Arch-internal** (inside `arch/x86_64/acpi.rs`, NOT on the trait): the MADT
  interrupt-routing facts — IOAPIC base(s), GSI base, ISA-IRQ→GSI source
  overrides, CPU APIC ids. These have no aarch64 (GIC) analogue; the x86 ACPI
  parser caches them and the (next) x86 IOAPIC code reads them via `pub(crate)`
  accessors. IOAPIC / GSI / MADT never appear in neutral names (check-arch
  clean). This is the "arch-neutral device discovery" shape: neutral PCI
  enumeration over neutral ECAM regions; arch-specific interrupt routing stays
  in the arch layer.

**Other decisions.** `ArchPlatform::init()` takes no firmware argument and
sources its own RSDP from a Limine request it owns — consistent with every other
`ArchX::init()` (`Irq`/`Timer`), so `main.rs` stays arch-agnostic. The
`RsdpRequest`/`RsdpResponse` protocol bindings sit in `limine.rs`; the request
*static* lives in `arch/x86_64/acpi.rs` (`.limine_requests` section — confirmed
the linker collects a submodule's request, the bootloader populates it). The
byte-level parsers are pure functions over `&[u8]` (host-tested against
synthetic RSDP/XSDT/MADT/MCFG blobs); results cache in fixed-size static arrays
+ atomic counts (no allocation; "write once at boot, read after", the
`apic::LAPIC_BASE` discipline). Tables are read through the HHDM (with an
already-virtual-pointer guard for older Limine). Missing/malformed tables are
logged, not fatal, at this stage.

ABI impact: none (no `#[repr(C)]` boundary type or syscall). Verified:
`check-arch` clean; `build` clean (no warnings); `test` 382 host tests (+8 ACPI
parser tests); `qemu` on q35 logs `acpi: RSDP rev 2 (XSDT); 1 IOAPIC, 5
src-override, 1 CPU; 1 ECAM region` / `IOAPIC0 @0xfec00000 gsi_base 0` / `ECAM0
@0xe0000000 seg 0 bus 0-255`, and the userspace demo runs unchanged. Branch
`phase-2/acpi-tables` off `main` (includes merged PR #30). Next: `phase-2/ioapic`
consumes the cached MADT facts.

---

## 2026-06-11 — Phase 2 prereq: IOAPIC bring-up + `ArchIrqRouter`

Third Phase 2 prerequisite (`phase-2/ioapic`). Brings up the IOAPIC (located by
the ACPI item's MADT facts) so external device interrupts can be delivered — the
path AHCI and the future `InterruptObject` build on: route a hardware line → an
IDT vector → a registered ISR → LAPIC EOI.

**The trait decision (the substance).** The IOAPIC could have been folded into
`ArchIrq`, but on reflection it is a *distinct controller* from the per-CPU local
controller `ArchIrq` models (the local APIC / GIC CPU interface). The hardware
splits cleanly on both arches into a **per-CPU local controller** (LAPIC ↔ GIC
CPU interface/redistributor: EOI, id, IPIs, the local timer) and a **system
interrupt router** (IOAPIC ↔ GIC distributor: map an external line → CPU+vector,
trigger/polarity, mask, affinity). They have different cardinality (local =
per-CPU; router = once) and the project already splits even the *one* LAPIC chip
across `ArchIrq` (delivery/ack) and `ArchTimer` (timer) by concern. So the router
gets its own sibling trait, **`ArchIrqRouter`** (`arch/irq_router.rs`,
re-exported `arch::IrqRouter`, x86 impl `X86IoApic`), consistent with the
one-trait-per-divergent-subsystem convention. Long-term this is where SMP IRQ
**affinity** and **MSI/MSI-X** routing ("route an external source → CPU+vector")
naturally live, and it mirrors the GICv3 distributor/redistributor split on
aarch64. (An earlier sketch used thin `init_syscall_entry`-style free fns — too
ad-hoc; rejected.)

**Mechanism (all arch-internal behind the trait):**
- `X86IoApic::init` maps the IOAPIC MMIO uncached (mirroring `apic.rs`), reads
  the version for the entry count, **masks every redirection entry**, and **masks
  the legacy 8259 PICs** (`0x21`/`0xA1` ← `0xFF`) so external IRQs flow only via
  the IOAPIC.
- `route`/`mask`/`unmask` program redirection entries; the pure `encode_rte`
  (RTE bit layout) and `resolve_isa_irq` (ISA IRQ → GSI via the MADT source
  overrides, ISA edge/high defaults) are host-tested.
- IDT gains a device-IRQ vector range (`0x30..=0x37`, macro-generated returning
  stubs like `timer_stub`) → one `device_irq_dispatch` that reads `frame.vector`,
  runs the registered handler from a lock-free `[AtomicUsize; N]` registry, then
  EOIs. `register_device_handler(fn) -> vector` wires a driver without touching
  the IDT. Edge-triggered only for now (the level-triggered IOAPIC-EOI path lands
  with the first level device).
- `self_test()` (a neutral trait diagnostic) routes the legacy **PIT** (IRQ0 →
  GSI2) to a device vector in a brief interrupt-enabled window — safe because the
  LAPIC timer LVT is still masked and the scheduler isn't running yet, so only
  the PIT fires — counts a few ticks, masks the line, and logs. Proves the full
  path before any device driver exists; replaced by real device IRQs at AHCI.

ABI impact: none. Verified: `check-arch` clean (no IOAPIC/GSI/RTE/8259 jargon in
neutral names); `build` clean (no warnings); `test` 385 host tests (+3:
`encode_rte`, `resolve_isa_irq` ×2); `qemu` on q35 logs `ioapic: up (24 entries),
8259 masked` then `ioapic: routed PIT IRQ0→GSI2→vec0x30; took 3 interrupts`, and
the userspace demo runs unchanged (the scheduler's LAPIC tick is undisturbed).
Branch `phase-2/ioapic` off `main` (includes merged PR #31). Next: `phase-2/dpc`.

---

## 2026-06-11 — APIC mode strategy (x2APIC) + minimum-hardware baseline

A take-stock decision prompted by the IOAPIC work: we use **xAPIC** (MMIO) and
deferred x2APIC because "QEMU/TCG doesn't emulate x2APIC". That premise needed
re-checking, and the answer shapes both the local-APIC plan and the project's
hardware floor. No code change — two recorded decisions plus comment fixes
(`apic.rs`, the `xtask qemu` CPU-model comment) and a deferred-decisions entry.

**QEMU fact (corrected).** TCG *does* emulate x2APIC, but only since **QEMU 9.0**
(commit `b5ee0468`, "apic: add support for x2APIC mode", Feb 2024 — after the 8.2
branch). The local dev loop runs older QEMU (8.2.x) under TCG, where x2APIC is
*not* available; the `+x2apic` CPUID bit is accepted but the userspace APIC won't
service MSR access. KVM's in-kernel irqchip has emulated x2APIC for years,
independent of QEMU version. So x2APIC is testable under emulation only via
**QEMU ≥ 9.0 (TCG) or KVM**. The stale "TCG does not emulate x2APIC" comments in
`apic.rs` and `xtask` are corrected to say "TCG before 9.0".

**Decision 1 — minimum-hardware baseline (no legacy).** The kernel already
requires SMEP and SMAP (enabled + asserted; dev loop passes `+smep,+smap`). SMAP
is Broadwell, so the de-facto x86 floor is **≈ 2014**. We adopt that explicitly:
the baseline is roughly **x86-64-v2 ISA + SMEP/SMAP**, and on any CPU meeting it,
an **invariant TSC and x2APIC are guaranteed**. Recorded as a permanent non-goal
("legacy / pre-2014 hardware") in `deferred-decisions.md`. (This also explains
the `timer: no invariant TSC` warning under `-cpu qemu64` — an ancient model; a
real target or `-cpu host`/`max` would not warn.)

**Decision 2 — local APIC: dual-mode, prefer x2APIC, deferred.** Since every
supported CPU has x2APIC, and it is *mandatory* for SMP beyond 255 logical CPUs
(8-bit xAPIC IDs can't address them), the plan is **dual-mode**: auto-detect
(`CPUID.01H:ECX[21]`) and prefer x2APIC, keeping xAPIC for the early-boot
transition (firmware hands off in xAPIC mode), as a fallback, and for the
pre-9.0 TCG dev loop. The xAPIC↔x2APIC difference is confined to `apic.rs`'s
register accessors (`read_reg`/`write_reg`) plus the 32-bit `id()` and the
single-MSR ICR write for IPIs — everything else (IOAPIC, timer, IDT) is
unaffected — so it is a small, contained change. **Deferred** to Phase 3 SMP /
real-hardware bring-up; implement alongside a dev-QEMU floor bump to ≥ 9.0 or an
opt-in `xtask qemu --kvm`. Tracked in `deferred-decisions.md` ("x2APIC mode").

**Dev loop posture.** Keep TCG + xAPIC as the default (portable, no `/dev/kvm`
needed, deterministic serial output for the byte-identical qemu checks). KVM
stays a future *opt-in* accelerator (faster, exercises x2APIC), not a
requirement. The current QEMU/CPU-model pin is unchanged.

---

## 2026-06-12 — Phase 2 prereq: DPC (deferred procedure call) queue

Fourth Phase 2 prerequisite (`phase-2/dpc`). Builds the **IRQ > DPC > Thread**
deferral mechanism (`kernel/src/dpc.rs`): an ISR does the minimum and `enqueue`s
a `Dpc`; the deferred completion work (run an IRP's completion routines, signal a
`PendingOperation` → wake its waiters) runs when the queue is drained at the
interrupt-dispatch tail. The primitive the storage slice (AHCI completion → wake
the driver) and `phase-2/pending-operation` consume.

**Design.** `Dpc { handler: fn(*mut()), ctx, queued: AtomicBool }` embedded
inline in an owning struct (no heap alloc to queue). A global pre-reserved
`IrqSpinLock<KVec<usize>>` (node addresses; `*mut Dpc` isn't `Send`) — drained
into a stack buffer, looped until empty. Runs with **IF=0** (the gate masks
interrupts), so no re-entrancy guard. Mirrors the deadline-heap / waiter-list
pattern. Drain hooks in `idt.rs`: `device_irq_dispatch` (after handler + EOI) and
`timer_dispatch` (after EOI, before `on_timer_tick`, so device-DPC wakeups land
in `ready` before the tick's reschedule). Lock is a **leaf** (held alone; nothing
acquired while held — the drain releases it *before* a handler may take `SCHED`).

**Decision — the timer wakeup stays inline (corrects `drivers-and-irps.md`).**
That doc said the timer-tick wakeup would "migrate onto the DPC queue." On tracing
the code, that's the wrong migration: the timer's own deadline-firing
(`fire_expired_deadlines` → wake) is the *timekeeping subsystem's* tick work,
already at the right point (under `SCHED`, before the reschedule, bounded + fast).
It stays inline; the DPC queue serves **device-ISR** deferred work — the
substantive thing the IRQ>DPC>Thread model exists for. The doc is corrected.

**SMP.** This is SMP-neutral. At SMP the whole tick is rebuilt around per-CPU
runqueues / timers / `SchedState`, so `on_timer_tick` is rewritten regardless and
inline-vs-DPC is decided fresh then — no migration debt now. The hard SMP problem
(cross-CPU thread placement — a wake targeting another CPU's runqueue, via that
CPU's lock or an IPI) is identical whether the wake is inline or a DPC. DPCs are
inherently per-CPU, so the single global `DPC_QUEUE` is a **single-CPU stand-in**
(like today's global `SCHED`/`current`): the Phase-3 per-CPU refactor changes its
storage, not the `enqueue`/`run_pending` API.

*(Deferred: IF=1 draining for responsiveness under heavy IRQ load; level-triggered
IOAPIC-EOI — both land with their first consumer.)*

ABI impact: none. Verified: `check-arch` clean (`dpc.rs` is neutral kernel code);
`build` clean (no warnings); `test` 390 host tests (+5: enqueue/drain order/
dedup/requeue/empty); `qemu` on q35 logs `ioapic: routed PIT IRQ0→GSI2→vec0x30;
took 3 interrupts (3 via DPC)` — the ISR→enqueue→drain→handler path end-to-end —
and the scheduler + userspace demo run unchanged. Branch `phase-2/dpc` off `main`
(includes merged PRs #32, #33). Next: `phase-2/demand-paging`.

## 2026-06-12 — Phase 2 prereq: demand paging (`#PF` fault-in + lazy anonymous + `FileBacked`)

Fifth Phase 2 prerequisite (`phase-2/demand-paging`). Closes two Phase-1 stubs the
page cache depends on: the `#PF` handler did no fault-in (any genuine ring-3 fault →
SegFault), and anonymous VMAs were eager (`map_vma` allocated + zeroed every page up
front).

**Design.** A neutral `AddressSpace::fault_in(addr, access) -> FaultIn` (`mm/addr_space.rs`):
under the rank-4 AS lock, look up the covering VMA (`VmaTree::find_covering`), check
the attempted access (`FaultAccess::{Read,Write,Execute}`) against the VMA's
`Protection`, and for an `Anonymous` page allocate + zero one frame, install the leaf
PTE (`Paging::map_page`), and **flush** the stale not-present TLB entry
(`Paging::flush_tlb_page`) — the faulting AS *is* live in the MMU, unlike eager
`map_vma`. `FaultIn` is `{Mapped, NoVma, Protection, Oom, NoPageCache}`; only `Mapped`
retries. `map_vma_lazy` reserves an anonymous VMA with **no** frames (same structural
rejections as `map_vma`, allocates nothing). The x86 `#PF` handler (`pf_dispatch`)
decodes the error code (bit0 present, bit1 write, bit4 insn) into a neutral
`FaultAccess` and, for a **not-present ring-3** fault, calls `fault_in` *before* the
fatal `user_fault` path; success `iretq`s and the instruction retries.

**Decisions.**
- **Lazy scope = the ELF stack.** The loader reserves the user stack via
  `map_vma_lazy`; PT_LOAD segments stay eager (the loader copies file bytes into their
  frames via the HHDM, so the frames must exist at load time). The stack is the clean
  candidate — pure zero-fill, register-based bootstrap, no argv/envp/auxv written to
  it — so every userspace process now demand-faults its stack on first use, exercising
  the real path with no extra wiring. (Demonstrated live: the `parent` demo runs to
  completion on a demand-faulted stack; plus a boot smoke test `demand-paging:
  on-fault path OK`.)
  - *Lazy vs eager for today's tiny stack (raised 2026-06-12).* The stack is only 4
    pages (`DEFAULT_USER_STACK_SIZE`), so eager would also be fine — the memory/zeroing
    savings from lazy are negligible at this size. Kept lazy anyway because: (1) it is
    the only *live* exerciser of the real hardware `#PF → pf_dispatch → fault_in → iretq`
    path (the boot smoke test calls `fault_in` directly on a non-active AS; the host
    tests are pure logic — neither triggers an actual fault); (2) it is the design a
    realistic stack must adopt — a larger grow-down reservation with an unmapped **guard
    page** below it requires demand paging, and cannot be expressed with full eager
    commit; (3) the cost is a one-time first-touch fault per page actually used (a
    context switch never touches the user stack, so it does not refault) — not a
    per-switch cost. A guard page is a natural follow-up.
- **`MappingKind::FileBacked` added now** (variant + `fault_in` arm `→ NoPageCache` +
  `free_vma_pages` arm), even though no producer constructs one yet — it establishes
  the dispatch shape the page-cache slice fills in. Tracked in `deferred-decisions.md`
  with TODOs; dead but documented.
- **Ring-3 only.** `fault_in` is reached solely for `cs & 3 == 3` faults, so the
  faulting thread holds no kernel locks (no self-deadlock on the AS lock), and kernel
  copy-primitive faults are caught earlier by the user-access recovery table.
  Corollary: kernel access to a not-yet-faulted user page is **not** auto-populated;
  nothing does that today (the stack carries no loader-written content) — revisit when
  argv-on-stack / the page cache need it.
- **`regs::invlpg` is a no-op under `cfg(test)`.** It is the first host-tested code
  path to flush the TLB; `invlpg` `#GP`s in the ring-3 test process. The kernel always
  runs ring 0 and the eviction has no host-observable effect, so a test build elides
  it — letting `fault_in` be exercised host-side.

This unblocks lazy `MemoryObject` backing (lifting the `MemoryObject::MAX_SIZE` DoS
cap — still needs a sparse per-object frame table + `Process` accounting) and the page
cache.

ABI impact: none. `MappingKind` is an internal mm enum (not an ABI-boundary type);
`FileBacked` is appended last, preserving existing discriminants. Verified: `check-arch`
clean (`fault_in`/`FaultAccess`/`FileBacked` are neutral mm code; the arch handler
reaches them via `crate::mm`/`crate::arch::Paging`); `build` clean (no warnings);
`test` 397 host tests (+7 demand-paging: lazy-no-PTE / per-page fault-in / no-VMA /
write-to-RO / exec-from-NX / partial-drop-frees / repeated-drop-no-leak; the elf
stack test rewritten for lazy reservation); `qemu` on q35 logs `demand-paging:
on-fault path OK — 2 anonymous pages backed lazily` and the scheduler + userspace
`parent` demo run unchanged on demand-faulted stacks. Branch `phase-2/demand-paging`
off `main` (includes merged PR #34). Next: `phase-2/pending-operation` (async-I/O) or
`phase-2/dma-alloc`.

## 2026-06-12 — Phase 2 prereq: `PendingOperation` + async-I/O `sys_wait` + IPC `Block`

Sixth Phase 2 prerequisite (`phase-2/pending-operation`) — the async-first blocking
primitive (CLAUDE.md: *every potentially-blocking op returns a `PendingOperation`;
the thread blocks on `sys_wait`, never inside another syscall*). `KObjectType::
PendingOperation = 9` was already reserved but unimplemented; `sys_wait` rejected it
and the IPC `Block`/`BlockBounded` send modes were `Unsupported`.

**The object.** `object/pending_op.rs`: a one-shot waitable mirroring `Timer`
(`#[repr(C)]`, `KObjectHeader` first, `UnsafeCell<Inner { signaled, status, waiters }>`
touched only under the rank-1 `SCHED` lock). `signal(status)` is idempotent (first
completion wins); `status` is stable once signalled. Destructor + `test_probe` arms
added.

**Wait/wake.** The wait machinery is already generic — three `KObjectType::
PendingOperation` dispatch arms (`obj_already_signaled`/`add_waiter`/`remove_waiter`)
plus `signal_pending_op` (modelled on `signal_ipc_endpoint`). `sys_wait` gained the
type-check arm and now populates `IoResult.status` (which already existed) from a
signaled PO via `pending_op_status` — Timer/channel/notification stay status 0.

**First consumer — IPC `Block`, commit-message model.** A blocking send returns a PO
that completes when the message is *delivered* (the submit→complete shape storage
will reuse), not a "space-available, retry" readiness signal. `IpcChannel::Inner`
gained a bounded `pending_sends: KVec<PendingSend { msg, transfers, po: ObjectRef }>`
on the **receiving** endpoint (whose recv frees space). `send_or_queue` delivers into
the peer ring if there's room (PO pre-signalled) or holds the message + a cloned PO
ref; `promote_pending_send` (run from the recv path) moves the oldest held message
into the freed slot and completes its PO; endpoint close completes held senders
`PeerClosed`. `ObjectRef` discipline: refs are *moved* in/out under `SCHED` and only
*dropped* outside it (the recv syscall drops the promoted ref; the closing endpoint's
`Inner` drop reclaims held entries). `sys_channel_send(Block)` always returns a PO
handle (honoring "never block inside a syscall"); `PendingFull`/`PeerClosed` are
synchronous errors.

**Scope decisions.**
- **One PR; `Block` only, `BlockBounded` deferred.** `Block` fully exercises the
  primitive (object + `sys_wait` + commit-message + completion). `BlockBounded`'s
  deadline-bounded variant needs deadline-heap surgery (`is_thread: bool` → a 3-way
  kind + channel back-pointer), a timer-tick timeout-cancel arm, and a
  `sys_channel_send` deadline arg — carved out to a focused follow-up to keep this PR
  off the safety-critical timeout paths. `BlockBounded` stays `Unsupported`; the
  anticipatory `cancelled` field on `PendingSend` was stripped (returns with its
  producer). See `deferred-decisions.md`.
- **`Block` = commit the message, PO completes on delivery** (not writable-edge
  readiness) — the shape the storage/fs/page-cache slices reuse.

**No DPC needed.** IPC completion is signalled synchronously on peer-receive (under
`SCHED`); the DPC queue (built earlier) is the path device I/O will use to signal a
PO from an ISR's deferred work — same `signal_pending_op`, a later consumer.

ABI impact: **none to the version hash** — `PendingOperation = 9` already existed,
`IoResult` layout is unchanged (its `status` field already existed), `SendMode` was
already defined. (`IoResult::completed(handle, status)` is a new constructor over the
same layout; a `sys_channel_send` deadline arg arrives with `BlockBounded`.) Verified:
`check-arch` clean; `build` clean (no warnings); `test` 405 host tests (+13: PO object
signal/one-shot/waiters/destroy, IPC pending-send immediate / queue-promote-FIFO /
close-release); `qemu` logs `parent: blocking send completed via PendingOperation
(4 queued, 1 blocked-then-delivered)` and the rest of the parent/child demo runs
unchanged. (Found + fixed a self-inflicted bug en route: an edit split
`#[unsafe(no_mangle)]` from the parent's `_start`, GC-ing the binary to an empty
664-byte ELF.) Branch `phase-2/pending-operation` off `main` (includes merged PR #35).
Next: `phase-2/dma-alloc`, then the IPC `BlockBounded` follow-up.

## 2026-06-12 — IPC `BlockBounded` (deadline-bounded blocking send)

The follow-up carved out of the PendingOperation slice (#37). `BlockBounded` is
`Block` plus a *delivery deadline*: a held (undelivered) send whose deadline
elapses is cancelled — its `PendingOperation` completes `TimedOut` and the message
is reclaimed.

**Deadline heap.** The scheduler `deadline::Entry`'s `is_thread: bool` grew to a
3-way `kind: DeadlineKind { Thread, Timer, PendingSend }` plus a `channel: usize`
back-pointer; `remove(target, kind)` keys on the pair. `fire_expired_deadlines`
branches on `kind`: the new `PendingSend` arm cancels the held send
(`IpcChannel::cancel_pending_send`, a flag-set only — **no `ObjectRef` drop under
`SCHED`**) and completes its PO `TimedOut`. The send registers its deadline (target
= PO, channel = the receiving peer) on a `Queued` outcome; it is removed when the
send is delivered early (in `ipc_recv_pop_into`) or its endpoint closes
(`ipc_endpoint_closing`) — the latter mandatory, else a stale deadline could fire
against a freed channel. A 6th `sys_channel_send` arg carries the deadline
(absolute monotonic ns; the dispatch already forwarded the register).

**Timeout reclaim = reclaim-on-recv** (the recommendation from the planning
discussion). The timeout only tombstones the held send (`cancelled` flag);
`promote_pending_send` sweeps cancelled entries on the next recv into a
`ReclaimedSend` buffer the recv **syscall** drops outside `SCHED`, and delivers the
oldest live send. Close reclaims any remainder via the endpoint's `Inner` drop. So
no `ObjectRef` is ever dropped under `SCHED`, and no permanent wedge exists for a
channel still being received on. The **general** deferred-free mechanism (a list
drained via the DPC queue) is the long-term home for `SCHED`/IRQ-context
reclamation — deferred until a consumer with no natural drain (device-I/O cancel)
needs it. See `deferred-decisions.md`.

ABI impact: the `sys_channel_send` **deadline arg** is a syscall-surface change (a
previously-ignored 6th register), not a hashed type layout — **no version-hash
impact**. Verified: `check-arch` clean; `build` clean (no warnings); `test` 408
host tests (+3: deadline-kind coexistence, `cancel_pending_send`, recv-sweep-then-
promote); `qemu` logs `parent: blocking send timed out via PendingOperation
(BlockBounded)` alongside the existing `Block` line, and the rest of the
parent/child demo runs unchanged. Branch `phase-2/block-bounded` off `main`
(includes merged PRs #36, #37). Next: `phase-2/dma-alloc` (the last prereq).

## 2026-06-12 — Phase 2 prereq: DMA-capable allocation (`DmaBuffer`)

The **last** Phase-2 prerequisite (`phase-2/dma-alloc`). Bus-mastering devices (the
coming AHCI storage driver) need physically-contiguous, aligned buffers and their
**physical address** — none of which `KBox`/`kmalloc` exposes (the slab even
rejects `align > SLAB_SIZE`). The buddy already provides contiguity + alignment
(order-`k` blocks span `2^k × PAGE_SIZE` and are aligned to it); this slice adds a
thin RAII wrapper over it.

**`DmaBuffer`** (`mm/dma.rs`): `alloc(size)` rounds up to a power-of-two page block
(`order ≤ MAX_ORDER`), `buddy_alloc`s it, zeroes it through the HHDM, and stores
`{ virt, phys, order }`; `phys()` / `virt()` / `as_mut_slice()` / `len()`; `Drop`
→ `buddy_free`. Page-aligned base, block-size-aligned for `order > 0` — enough for
AHCI's 1 KiB cmd list / 256 B FIS / 128 B tables laid out within one buffer (no
explicit `align` param; larger alignment = larger order). Arch-neutral (buddy +
HHDM only).

**Decisions.**
- **No DMA zones.** A below-16 MiB / below-4 GiB zone only matters for a device
  that can't do 64-bit DMA — excluded by the no-legacy ≈2014 / x86-64-v2 baseline
  (modern AHCI sets `CAP.S64A`), and the dev loop's 256 MiB RAM is sub-4 GiB
  anyway. `DmaBuffer` returns whatever block the buddy gives; a DMA-mask param +
  zoned free-list land only if a constrained device ever appears. (deferred-
  decisions updated.)
- **No cache maintenance.** x86 DMA to/from write-back HHDM memory is
  snoop-coherent. A non-coherent arch (aarch64) will add an `ArchDma`
  clean/invalidate hook — deferred.

ABI impact: none (internal allocation machinery; no syscall/boundary/`KObjectType`).
Verified: `check-arch` clean (`dma.rs` is neutral `mm`); `build` clean (no
warnings); `test` 414 host tests (+6: order rounding, page-aligned/zeroed alloc,
multi-page contiguity + block-alignment, write-through-to-phys, oversize error,
no-leak); `qemu` logs `dma: 2-page buffer @ phys 0x808000 (contiguous,
page-aligned, zeroed)` (the active page tables translate `virt()` back to `phys()`)
and the scheduler + userspace demos run unchanged. Branch `phase-2/dma-alloc` off
`main` (includes merged PR #38).

**The Phase-2 prerequisite band is complete** — all seven items (drivers-and-IRPs
doc, ACPI, IOAPIC, DPC, demand paging, `PendingOperation`/async-I/O + IPC
`Block`/`BlockBounded`, DMA allocation) have landed. Next: **Phase 2 proper** — the
storage slice (AHCI/GPT) → fs-server → page cache.

## 2026-06-14 — Phase 2 slice 1: namespace design (Part A) + lookup-as-PendingOperation

Started Phase 2 proper slice 1 (per-process namespaces). It's large, so it's split
into a docs-first design pass + three code parts (B: object+resolver, C: syscalls +
async lookup + per-process namespace, D: cache + spawn inheritance), each its own PR.
This entry covers **Part A**, the design doc
`docs/architecture/namespace-and-resource-servers.md` (the contract the plan required
before slice 1, per 2026-06-11). It pins the data model, path grammar, longest-prefix
resolution + rights attenuation, binding kinds (DirectHandle now; ResourceServer /
SubNamespace / Rewrite later), the capability model, the lookup cache, and the
kernel/userspace split, with an explicit slice-1-vs-slice-3 scope table.

**Decision — `sys_ns_lookup` is async (`PendingOperation`) from the start.** A real
lookup forwards over IPC to a resource server (blocking) → it must be async per the
async-first rule (the spec already specified a PO). Building it synchronous now and
breaking the ABI later — on a syscall every fs client uses — is worse. Implication
fixed now: a PO must convey a **result handle**, so `IoResult` grows a `result: u64`
word (the payload `io_result.rs` already anticipated for "richer waitables that report
payloads") and a PO can complete with a result. In slice 1, direct-handle bindings
resolve in-context and return a **pre-signalled** PO carrying the resolved handle —
the full async result path without any resource-server machinery; IPC-forwarding and
the cross-context handle install land in slice 3.

**Decision — bind gating.** Slice 1 enforces the `BIND` **handle right** (you can only
bind into a namespace you hold a `BIND`-righted handle to; supervisors hand clients
`LOOKUP`-only handles). The system-wide `BIND_NAMESPACE` **syscap** is an additional
gate that lands with the process-capability model (not yet designed); both apply in
the final design (`why-supervisor-registration.md`).

Slice-3 work (designed, not built): the `ResourceServer` trait, `OpStatus`, the
registry, IPC-forwarded lookup, the rsproto namespace ops. Spec updated
(`syscall-abi.md`: `SYS_NS_*` = 22–25 reserved, the `IoResult.result` word) and the
plan's slice-1 section re-expressed as the A/B/C/D breakdown. No code yet — Part A is
docs-only. Branch `phase-2/namespace-design` off `main` (includes merged PR #39).
Next: Part B (the `Namespace` object + resolver).

## 2026-06-14 — Phase 2 slice 1: Namespace object (Part B) + syscalls/async lookup (Part C)

**Part B (PR #41)** built the `Namespace` kernel object (`KObjectType::Namespace`):
a rank-4 `SpinLock<Inner>` over a `KVec<Binding { path: KVec<u8>, target: ObjectRef,
rights }>` (the `AddressSpace` model — syscall-accessed, not scheduler-touched), with
`bind`/`unbind`/`resolve` (longest-prefix, component-boundary match) and path
validation. Drop-under-lock hazard avoided: `try_reserve` before the committing push,
the target `ObjectRef` handed back on every error path, `resolve` only *clones*. Host
tests only (no syscalls).

**Part C (this PR)** wired it to userspace — the four `sys_ns_*` syscalls (22–25), the
async-lookup result plumbing, and `Process::namespace`.

**Decision — `IoResult` grows to 24 bytes (ABI break).** A `result: u64` word is
appended at offset 16 (earlier offsets unchanged) so a completion can return a value,
not just an `i32` status. `PendingOperation` gains a matching `result` payload
(`signal_with_result` / `pending_op_completion`). `sys_ns_lookup` returns a
**pre-signalled** PO carrying the resolved handle — direct-handle resolution finishes
in the caller's syscall context, so `complete_pending_op` records the outcome in place
(no waiters yet) and the caller's `sys_wait` takes the already-signalled fast path. All
three userspace wait buffers (`parent`/`child`/`hello`) grew to 24 bytes in lockstep.
The ABI version hash is invalidated (noted in `abi-version-hash.md`).

**Decision — lookup error delivery splits sync vs. via-PO.** *Resolution* failures
(no covering binding, or a non-empty suffix on a direct-handle leaf) complete the PO
with a `NotFound` status — "you are not told *why* a path doesn't resolve." *Argument /
permission / allocation* failures (bad `ns` handle, missing `LOOKUP`, malformed path,
PO/handle exhaustion) return **synchronously** as a negative isize with **no** PO
created. The PO + its handle are allocated *before* resolution so every resolution
outcome flows through the PO uniformly.

**Decision — `Process::namespace` lands now, populated at boot.** The design has each
process resolve names against a root `Namespace` it holds (`Process::namespace`). Part C
adds the field (mirroring `notification_channel`) and the boot code gives **pid 1** a
root namespace: the `Process` owns one reference, a handle is installed in pid 1's table
and passed to the parent in `rsi` (slot [1], alongside the notif handle in `rdi`). The
four syscalls still operate on an **explicit** namespace handle (per the spec); the
root is what a process resolves against and what children will inherit. Deriving a
*child's* `Process::namespace` from its parent at spawn is **Part D**.

**Decision — `bind` rights, `UNBIND` allocatability.** `bind` clones the resource
handle's reference into the binding (the caller's handle stays valid) and records the
handle's **current rights** as the binding's cap; `lookup` attenuates to `requested ∩
binding.rights`. `namespace_rights()` mints `LOOKUP | BIND | UNBIND` + the generic band;
`UNBIND` is a modifier-band right (bit 35), already allocatable on a `Namespace`, so
`type_rights.rs` needed no change.

The full `create → bind → lookup → wait → use → unbind` round-trip needs the scheduler,
so it is verified by the QEMU `ns_demo` in `userspace/parent` (host tests cover the
rights-allocatability, the `NsError`→`KError` map, the path-length bounds, the PO result
payload, and `Process::namespace` ownership/release). Branch `phase-2/namespace-syscalls`
off `main` (includes merged PR #41). Next: Part D (lookup cache + spawn inheritance).

## 2026-06-14 — Phase 2 slice 1: lookup cache + spawn inheritance (Part D — slice complete)

Part D closes the namespace slice: the lookup cache, spawn-time namespace inheritance
(sandbox-by-construction), and the boot banner now states **Phase 2**.

**Decision — uniform 4-register bootstrap ABI.** A spawned child needs a handle to its
root namespace, but the three bootstrap registers (`rdi`/`rsi`/`rdx`) were full. Rather
than a `sys_ns_self()` query (mild ambient-authority flavor, not in the design), the
hand-off grew to **four** registers with one consistent meaning across pid 1,
`sys_process_spawn`, and `sys_thread_create`: `rdi`=notification channel, `rsi`=root
namespace, `rdx`=first installed handle, `rcx`=`arg0`. This keeps the explicit-handoff
(no-ambient-authority) model and matches Part C's pid-1 `rsi`=namespace. `enter_user`
gained a 4th arg (seeds user `rcx` from kernel `r9`); `Thread::user_boot_args` and
`spawn_user` are `[u64; 4]`. `child.rs` moved its endpoint read `rsi`→`rdx` and role
`rdx`→`rcx`.

**Decision — inheritance + `SpawnArgs.namespace` (sandbox-by-construction).** `SpawnArgs`
gained a `namespace: RawHandle` (size 88→96): `0` ⇒ the child **inherits** a
`LOOKUP`-only handle to the parent's namespace (shared object); non-null ⇒ a namespace
the parent holds a `LOOKUP`-righted handle to (typically a more-restricted one it
constructed) — the child gets a `LOOKUP`-only handle to *that*. The child resolves names
but cannot rebind its own root; **restriction is by namespace contents**, chosen by the
parent. `spawn_rollback_child_handles` extended to also close the child's namespace
handle.

**Decision — the lookup cache.** Each `Namespace` keeps a bounded (`NS_CACHE_MAX = 8`),
pre-reserved cache of **positive** resolutions as `path → binding-index`. It holds **no**
`ObjectRef` (so a flush — and round-robin eviction — is a byte-only `KVec` drop, never an
`ObjectRef` drop under the rank-4 lock). The whole cache is flushed on every `bind`/
`unbind`, so a cached index always refers to the same binding. Insertion is best-effort
(skips on alloc failure). A pure optimization — no contract change.

`SpawnArgs` grew by 8 bytes (boundary type; spec + asserts updated). The bootstrap
register count grew 3→4 (a hand-off convention, not a hashed type). Verified by host
tests (cache hit/flush/evict, `SpawnArgs` layout) + the QEMU demo: the parent builds a
restricted `/store` namespace and hands it to both children, which resolve `/store`
(inheritance) and get `NoAccess` on a bind (LOOKUP-only restriction). Branch
`phase-2/namespace-inherit-cache` off `main` (includes merged PR #42).

**Namespace foundation (Phase 2 slice 1) is complete.** Next: the resource-server
protocol + the first filesystem (slice 3 machinery designed in Part A).

## 2026-06-15 — Phase 2 slice 2: entropy design (Part A)

Started Phase 2 slice 2 (the kernel CSPRNG). Pulled ahead of the in-kernel resource
servers because slice 3's `/dev/entropy` consumes it. Like the namespace slice, it's
**docs-first + three code parts** (A: this design doc; B: ChaCha20 CSPRNG + arch
HW-RNG; C: pool + boot seeding + interrupt-jitter mixing + reseed + kernel-PRNG
re-seed; D: `EntropyObject` + the two syscalls + demo). This entry covers **Part A**,
the design doc `docs/architecture/entropy.md`.

**Decision — read is async by contract, but seeded before userspace.** `sys_entropy_read`
returns bytes synchronously when the pool is seeded and a `PendingOperation` when it
isn't (honoring async-first — no blocking inside a syscall). In practice `RDSEED`/
`RDRAND` cross the 256-bit gate in microseconds at boot, well before pid 1 runs, so
the blocking path is a never-hit-in-practice safety net for hardware lacking both HW
RNG instructions (jitter-only seeding). Worth building correctly; rarely exercised.

**Decision — ChaCha20, hand-rolled, mix-don't-trust.** The CSPRNG is ChaCha20 (RFC
8439), built from scratch in `libkern` (no external crates), with fast-key-erasure
for forward secrecy and periodic + byte-threshold reseed. ChaCha20 over AES-CTR
because the kernel is soft-float and must not assume AES-NI (ChaCha is integer-only,
no XSAVE state). Every source — HW RNG included — is **absorbed into the pool, never
used as output directly**: a backdoored `RDSEED` can't weaken output below the jitter
contribution.

**Decision — `EntropyObject` is a capability token onto a kernel singleton.** The
random source is one global CSPRNG; `sys_entropy_create` mints a `READ`-righted
handle (a view), `sys_entropy_read` draws through it. Slice 3's `/dev/entropy` just
binds such a handle into namespaces — no ABI change.

Sources fixed: RDSEED preferred / RDRAND fallback (CPUID-detected) + TSC jitter at
interrupt dispatch; HHDM/boot-params/deterministic-at-boot are explicit non-sources.
Deferred (noted in the doc): fork/VM-snapshot reseed, aarch64 `RNDR`/SMCCC TRNG,
depleting-estimate blocking semantics. Spec updated (`syscall-abi.md`: numbers 26/27
reserved + the numbering table extended through 22–27) and the plan's Entropy slice
re-expressed as A/B/C/D. No code — Part A is docs-only. Branch `phase-2/entropy-design`
off `main` (includes merged PR #43). Next: Part B (ChaCha20 CSPRNG + arch HW-RNG).

## 2026-06-22 — Phase 2 slice 2: entropy CSPRNG/HW-RNG (Part B) + pool/seeding (Part C)

**Part B (PR #45)** landed the standalone primitives: `ChaCha20Rng` (`libkern/chacha.rs`,
RFC 8439, fast key erasure) and the `arch::Entropy` HW source (RDSEED-preferred /
RDRAND-fallback, CPUID-detected). Host-tested; no consumers yet (carried
`TODO(entropy)` `#[allow(dead_code)]` markers).

**Part C (this PR)** is the integration — `kernel/src/entropy.rs`, a global
subsystem that seeds the CSPRNG at boot and keeps absorbing interrupt jitter,
making the Part B primitives live (markers removed).

**Decision — one `IrqSpinLock<EntropyState>` leaf for pool + CSPRNG.** Sampling runs
in IRQ context (`on_irq_sample`, from the timer tick + device IRQs) and draws run in
syscall/boot context (`fill`); a single `IrqSpinLock` (a leaf, like `DPC_QUEUE`)
makes the two mutually safe via IF-masking, avoiding a two-lock order. No alloc, no
nested locks; the seeded-latch PO-waiter wake (Part D) runs outside it under `SCHED`.

**Decision — CSPRNG always keyed at boot; the gate only gates userspace.** `init`
draws a hardware burst (8× `try_seed_u64`, 64 estimated bits each), mixes early
clock jitter, and **always** keys the CSPRNG from the pool — so kernel draws (the
handle-table shuffle) work even with no HW RNG. The 256-bit `seeded` latch only
gates the userspace read contract (Part D). The pool absorb is a SplitMix-style
diffusion (mixing only; ChaCha's key schedule is the cryptographic conditioning);
jitter credits 1 bit / 8 samples; reseed folds fresh pool entropy into the key
periodically (byte-threshold). The handle table's fixed `PHASE1_SEED` is gone — it
now seeds from `entropy::seed_u64()` (only affects free-list scan order; handle
unforgeability is still the owner-PID + generation counter).

**Decision — QEMU opts in `+rdrand,+rdseed`.** Added to the xtask `-cpu` flags so the
boot CSPRNG seeds from the hardware source (TCG emulates both); boot now logs
`entropy: source RDSEED, 8 hw draws, seeded=true` before the handle table. Without
the flags the kernel correctly falls back to jitter-only seeding (`seeded=false`) —
both paths verified. cargo xtask test: 453 passed; build + check-arch clean.
Branch `phase-2/entropy-pool-seeding` off `main` (includes merged PR #45). Next:
Part D (`EntropyObject` + `sys_entropy_create`/`sys_entropy_read` + the PO-waiter
wake on a runtime seed latch).

## 2026-06-22 — Phase 2 slice 2: EntropyObject + sys_entropy_* (Part D — slice complete)

Part D exposes the entropy subsystem to userspace and closes the slice: the
`EntropyObject` kobject (a stateless capability token onto the singleton CSPRNG),
`sys_entropy_create`/`sys_entropy_read` (26/27), the true-blocking unseeded-read
path, and a QEMU demo.

**Decision — read returns `0` on synchronous fill, a positive `PendingOperation`
handle when unseeded.** Part A said "returns len"; refined to `0` because a byte
count would be ambiguous with a PO handle on the same `isize` (handles are always
≥ 1, so `0` / `>0` / `<0` cleanly mean filled / wait-on-PO / error). A seeded read
fills the user buffer and wipes the kernel bounce buffer; an unseeded read leaves
the buffer untouched and hands back a PO to wait on, then the caller re-reads.

**Decision — true-block unseeded path via a seed-waiter list (user choice).** The
entropy subsystem owns a bounded `KVec<ObjectRef>` of parked PO refs (the IPC-`Block`
pattern); `register_seed_waiter` queues them (or hands one back as `AlreadySeeded` /
`Full`). `on_timer_tick` → `wake_entropy_seed_waiters`, gated by a lock-free
`SEED_WAKE_PENDING` atomic, drains them once `is_seeded()` and signals each under
`SCHED`. Dropping the spent PO refs under `SCHED` is sound: a `PendingOperation`'s
`Drop` touches only the allocator (rank 6, the legal below-`SCHED` direction), never
re-entering `SCHED` — documented in `lock-ordering.md`. In QEMU (`+rdrand,+rdseed`)
the pool seeds at boot, so this path isn't exercised by the demo; it's covered by
host tests of `register`/`drain`.

**Bug found + fixed in the demo:** `a != b` on `[u8; 32]` in the freestanding parent
emitted a `memcmp` intrinsic the binary doesn't provide → silent userspace hang. The
demo now compares with a manual byte loop. (No kernel impact; a userspace-runtime
note for future no_std demos.)

cargo xtask test: 452 passed; build + check-arch clean; the QEMU demo prints two
differing 32-byte reads + "entropy ok". Branch `phase-2/entropy-object-syscalls` off
`main` (includes merged PR #46).

**Entropy subsystem (Phase 2 slice 2) is complete.** Next: the in-kernel resource
servers (slice 3), where `/dev/entropy` binds an `EntropyObject` into namespaces.

## 2026-06-22 — Phase 2 slice 3, Part 0: ring-3 fault surfacing (+ two misdiagnoses corrected)

Part 0 of slice 3 (the prerequisites the entropy demo's "hang" motivated). Going in,
the plan named two pieces — (1) provide userspace mem intrinsics, (2) surface
unhandled ring-3 faults. **Measuring before building overturned both premises**;
the part shrank to one kernel change plus two documented findings.

**Finding 1 — userspace mem intrinsics are NOT missing.** `compiler_builtins`
already supplies `memcpy`/`memset`/`memcmp`/`memmove` on-demand for the
`x86_64-unknown-none` target: the kernel ELF defines all four (`nm`: `t memcpy` …),
and a parent build that references `memcmp` links it cleanly with **zero undefined
symbols**. So init's future `memcpy`/`memset` (TOML parsing, marshalling) resolve
automatically — **no intrinsics crate is needed**, and the planned Piece 2 was
dropped. (The slice-3 plan's Part-0 "mem intrinsics" bullet is superseded by this
entry; it'll be corrected when the scoping PR #48 lands.)

**Finding 2 — the original `a != b` "hang" is a codegen quirk, not an intrinsics
gap.** On this `-sse,+soft-float` target, inlined `[u8; N]` equality (`a != b`)
compiles to code that **infinite-loops in userspace** — confirmed: it inlines (no
`memcmp` call, no SSE in the binary) and the new fault diagnostic stays silent
(it's a true loop, not a fault). **Decision: document as a known issue and keep the
manual-byte-loop idiom** (`.iter().eq()` / explicit loop) in userspace until a real
userspace runtime exists; root-causing the LLVM/target interaction is deep and
low-value now. The entropy demo's manual loop carries a comment pointing here.

**Finding 3 — the fault-surfacing condition had to change.** The initial idea
("print when the faulting process has no notification channel") **misses the actual
case**: pid 1 *has* a channel (created at boot for its children's `ChildExited` +
its own faults), so `chan == None` never fires for it. Verified by faulting pid 1
deliberately — no diagnostic. The real hang is "the fault notification lands in a
channel that no *other* runnable thread will service," and the kernel has no clean
signal for "will anyone resume this."

**Decision — surface a fault via *scheduler-stranding*, not channel inspection.** In
`sched::suspend_with_fault`, after delivering the notification, if dequeuing the next
thread finds **nothing runnable** (falls through to the idle thread), the fault left
no thread to resume it → emit a last-ditch diagnostic (`report_stranded_fault`):
`*** unhandled ring-3 fault (no thread left to resume it): pid N tid M <kind> @ 0x… ***`
via the unsynchronized emergency serial writer (lock-free, mirroring `dump_and_halt`;
sound — a userspace fault never holds `SERIAL`, and `SCHED` is held so IF is masked).
This fires **only** for genuinely-stranded faults: a serviced fault (the worker demo)
wakes the supervisor's waiter via `signal_channel` *before* the dequeue, so the
`Some` branch is taken and it stays silent — no per-fault noise. Reads only neutral
data (pid/tid from the `Thread`, kind/addr from the `Notification` via a new
`Notification::fault_addr` getter), staying out of the arch-private `ExceptionFrame`.
Behavior is otherwise unchanged (the thread still suspends; a real supervisor still
resumes/terminates). Verified: a deliberate pid-1 fault now prints the line instead
of hanging; the worker-fault demo stays quiet. Branch
`phase-2/slice3-userspace-rt-fault-diag` off `main`.

## 2026-06-22 — Phase 2 slice 3, Part A: in-kernel resource-server framework design

Docs-only. Formalized the **in-kernel** resource-server framework that slice 3 builds,
by extending the living RS doc (`docs/architecture/namespace-and-resource-servers.md`)
in place rather than spawning a competing doc — it already holds the RS model, the
binding-targets table, and the scope summary.

**Naming convention (made explicit).** "**Resource server**" is the **umbrella**
term (the `lookup → OpStatus` contract); it has exactly two children — a **Kernel
Server** (binding target `KernelServer`, slice 3) and a **Userspace Server** (binding
target `UserspaceServer`, slice 7). No binding kind is named `ResourceServer` — the
earlier draft used that for the userspace child, colliding with the umbrella; renamed
to `UserspaceServer` so the two variant names mirror the two children and neither
shadows the parent. The doc shows the hierarchy as a tree. The convention is applied
to the **code** names too: the reserved kobject `KObjectType::ResourceServerReg` (=13)
→ `UserspaceServerReg` (it tags the userspace-server registration object, slice 7),
and the slice-7 registry `ResourceServerRegistry` → `UserspaceServerRegistry`. The
discriminant value (13) is unchanged, so **no ABI-hash impact** (the hash is over
discriminant values + layout, not identifiers). The umbrella stays a prose term with
no code entity of its own.

**Decision — `KernelServer` binding target + synchronous dispatch.** An in-kernel
server is a `lookup(suffix, rights) -> OpStatus::{Completed(handle) | Rejected(err)}`
function in a small kernel registry; a `KernelServer` binding holds its dispatch id.
`sys_ns_lookup` calls it **in the caller's syscall context**, installs the resolved
handle, and **pre-signals the lookup's `PendingOperation`** — reusing the slice-1
direct-handle delivery path verbatim. So in-kernel lookups are synchronous (no IPC,
no cross-context install, no new ABI); `OpStatus::Pending` is **reserved** for the
slice-7 userspace path. The `BindingTarget` enum (`DirectHandle` + `KernelServer`)
replaces slice 1's bare `ObjectRef` target.

**Decision — content model + boot binding.** A lookup yields a *handle to a kernel
object* the server computes per call (`/dev/entropy` → an `EntropyObject`;
`/proc/self/status` → a synthesized read-only `MemoryObject`). In-kernel servers are
always present, so the **kernel binds them into pid 1's root namespace at boot** — no
Ready handshake, no `BIND_NAMESPACE` holder (that's the userspace-server path);
children inherit via slice-1 namespace inheritance.

**Decision — `/proc/self` is self-reference, not ambient authority** (the design
point raised in scoping). Reachability is by namespace construction (a sandbox may
omit it; not a kernel-forced universal); the result is strictly the caller's own
process/thread/namespace, derived from the running context, **with no pid parameter
to forge**. Cross-process introspection (`/proc/<pid>`, enumeration) is a separate,
narrowly-bound capability with its own process registry — **deferred** (it is the
ambient-authority-sensitive surface); slice 3 ships only `/proc/self`.

**Doc reconciliation.** Re-marked the slice split throughout: the **in-kernel**
framework (`KernelServer`, dispatch registry, the servers, boot binding) is **slice
3**; the **userspace** path (`UserspaceServer` IPC target, forwarded lookup,
cross-context install, `librsproto`, Ready handshake) moved to **slice 7** (with the
fs-server, its first consumer). Updated the phasing note, binding-targets table, the
"Lookup is asynchronous" install cases, the kernel/userspace split, and the scope
summary (now a slice-1 / slice-3 / slice-7 three-column table). No code, no ABI
change. Branch `phase-2/slice3-rs-framework-design` off `main`. Next: Part B (the
`BindingTarget` enum + `KernelServer` dispatch + lookup wiring + boot binding).

## 2026-06-22 — Phase 2 slice 3, Part B: in-kernel resource-server framework (code)

Implemented the Part-A design. New module `kernel/src/object/kernel_server.rs` holds
the registry: `KernelServerId` (a `Copy` dispatch id, `Entropy` to start), `OpStatus`
(`Completed(ObjectRef)` | `Rejected(KError)`), and `dispatch(id, suffix, requested)`
fanning out by `match` (the kobject dispatch-by-match idiom). `OpStatus::Pending` is
**not** added yet — it is the userspace (slice-7) state; an in-kernel server answers
synchronously, so representing it now would be an unconstructed variant.

**`BindingTarget` replaces the bare `ObjectRef` binding target.** `namespace.rs`'s
`Binding.target` is now `BindingTarget::{DirectHandle(ObjectRef) | KernelServer(id)}`.
`bind` keeps its signature (wraps `DirectHandle` internally, still hands the
`ObjectRef` back on error for outside-lock drop); new `bind_kernel_server` takes a
`Copy` id (nothing to hand back). `unbind` now returns `Option<BindingTarget>` (the
caller drops it outside the lock — a `KernelServer` drop is a no-op). `resolve`
returns a `ResolvedTarget` (a **clone** of the `ObjectRef` for a direct handle, the
**copied** id for a server) — both lock-safe (no `ObjectRef` drop under the lock,
preserving the slice-1 mutation discipline).

**`sys_ns_lookup` dispatch.** A resolved `KernelServer` calls
`kernel_server::dispatch` *in the caller's syscall context*; `Completed(obj)` installs
the handle with `requested ∩ binding.rights` (the same install path direct handles
take, factored into a local `install` closure), `Rejected(err)` is delivered through
the **pre-signalled `PendingOperation`**. Synchronous: no IPC, no cross-context
install. The leaf-vs-subtree decision moved into the server (the entropy server
rejects a non-empty suffix), so `resolve` no longer applies the direct-handle leaf
policy to server targets.

**`/dev/entropy` is the first Kernel Server** (the whole server, folded into Part B as
the demonstrator — entropy is complete and it closes the loop that motivated landing
entropy first; `/proc/self` + the `/dev` stub remain Part C). A lookup mints an
`EntropyObject` (the object `sys_entropy_create` returns). The **kernel binds it into
pid 1's root namespace at boot** (`main.rs`, rights = the `entropy_rights` band), and
children inherit it via slice-1 namespace inheritance. The `parent` QEMU demo resolves
`/dev/entropy` from `rsi` and reads from the handed-back handle — verified end-to-end
(`/dev/entropy resolved+read ok`), distinct from the existing `sys_entropy_create`
demo.

**No ABI-hash impact:** `BindingTarget`/`ResolvedTarget`/`OpStatus`/`KernelServerId`
are internal kernel types; `sys_ns_lookup`'s args + PO contract, `entropy_rights`, and
the `KObjectType` set are unchanged. Verified: `cargo xtask build` / `check-arch` /
`test` (456, +6) / `qemu` all clean. Branch `phase-2/slice3-kernel-server-framework` off
`main`. Next: Part C (`/proc/self` self-reference servers + the `/dev` `DeviceNode`
stub).

## 2026-06-22 — Phase 2 slice 3, Part C: `/proc/self` self-reference servers

Shipped the `/proc/self/{process, thread, namespace}` Kernel Servers — a lookup returns
a handle to the **caller's own** `Process` / `Thread` / root `Namespace`, derived from
the running syscall context (no pid parameter to forge), reachable only if a supervisor
bound `/proc/self/*` into the caller's namespace (withholdable — a sandbox omits it). No
ambient authority.

**Per-leaf bindings + per-leaf `KernelServerId` (not one `/proc/self` prefix).** Forced
by the rights model: a lookup installs `requested ∩ binding.rights`, and `allocate`
rejects rights not valid for the resolved type (`is_rights_compatible`); `Process`/
`Thread`/`Namespace` carry **disjoint** principal rights, so no single prefix-binding
rights set works. Each leaf is its own exact binding with type-correct rights —
`process`/`thread` → `SIGNAL | TERMINATE` + generic band; `namespace` → `LOOKUP` +
generic, **no `BIND`** (a resolve view; self already holds a full root-ns handle via
`rsi`, and ambient self-`BIND` is an escalation smell) — and its own dispatch id so the
empty-suffix server knows which object to mint. **Zero framework change** (Part B's
`dispatch`/`OpStatus`/`install`/`BindingTarget` untouched). The servers return a
**clone** of an existing `ObjectRef` (not a freshly-minted object like entropy);
`None` (a kernel/boot thread with no process) → `Rejected(NotFound)`.

Added `sched::current_thread() -> Option<ObjectRef>` (clones `SCHED.current` under the
rank-1 lock; mirrors `current_process`). The three leaves are bound into pid 1's root
namespace at boot (`main.rs`); descendants inherit them — the binding is a dispatch id,
the *answer* is resolved per-caller, so one shared binding is correct.

**Deferred — numeric pid/tid (`/proc/self/status`).** pid/tid are attributes of objects
the caller now holds, so the mechanism is an open choice (synthesized read-only
`MemoryObject` snapshot, needing a HHDM-write *synthesis primitive*, vs. extending
`sys_handle_stat` which today returns only type/rights/generation). **Rejected**: a
lookup-contract extension returning a scalar in `IoResult.result` (permanent per-path
handle-vs-value ambiguity). **Deferred — the `/dev` `DeviceNode` stub** (no struct, no
enumeration syscall, no consumer). Both recorded in `deferred-decisions.md`.

**No ABI-hash impact:** `KernelServerId` is an internal enum; no boundary type, syscall,
or `KObjectType` change. Host tests: per-server leaf-suffix rejection (the success arms
need syscall context — covered by QEMU). The `parent` demo resolves all three leaves,
stats `process`/`thread` to assert their types, and *uses* the namespace handle to
resolve `/dev/entropy` through it. Verified: `cargo xtask build` / `check-arch` /
`test` (457, +1) / `qemu` all clean (`/proc/self/{process,thread,namespace} ok`). Branch
`phase-2/slice3-proc-self` off `main`. Next: slice 4 (Init).

## 2026-06-23 — Phase 2 slice 4, Part 1: real `libkern` + demo migration

Built the bottom userspace runtime layer for real (`userspace/libkern`: `syscall`,
`error`, `handle`, `abi`, `debug` modules — the canonical userspace mirror of the
kernel ABI, `#![no_std]`/no-alloc/core-only, `cfg_attr(not(test), no_std)` so the pure
logic is host-testable). Migrated `parent`/`child`/`hello` onto it, deleting ~485 lines
of triplicated syscall/ABI/debug boilerplate; fixed a latent `hello` bug
(`SYS_PROCESS_EXIT` was `0xFFFF_0001`, kernel expects `16`). `cargo xtask test` now also
runs libkern's host tests. No kernel ABI change (libkern mirrors it; compile-time
`offset_of!`/`size_of` asserts pin both sides). `abi-sync-check` deferred. Pulled
forward from Phase 3 because Init is libkern's first consumer (it's forbidden from the
higher layers). Branch `phase-2/slice4-libkern` (PR #53).

## 2026-06-23 — Phase 2 slice 4, Part 2: initramfs substrate (kernel)

Greenfield initramfs plumbing — Limine module + in-kernel CPIO parser + an
`/initramfs` resource server — so Init (and, here, the parent demo) can read
`init.toml` and files from a boot blob. Kernel-only; reuses the slice-3 KernelServer
framework.

- **Limine module request** (`kernel/src/limine.rs`: `ModuleRequest`/`ModuleResponse`/
  `LimineFile`) + a `module_path: boot():/boot/initramfs` line in `boot/limine.conf`;
  xtask packs a **CPIO-newc** archive (a placeholder `etc/init.toml`) into the ESP via
  a small in-Rust `newc` writer (`build_initramfs`/`cpio_entry`). The kernel records the
  module at boot (`init_initramfs` → `initramfs::set_blob`; the module's `address` is an
  HHDM-virtual pointer in never-reclaimed `MEMMAP_KERNEL_AND_MODULES` memory).
- **CPIO-newc parser** (`kernel/src/initramfs.rs`): pure over a byte slice, host-tested;
  `lookup(blob, path) -> Option<&[u8]>` (4-byte alignment, `./`-prefix normalisation,
  trailer/garbage → `None`).
- **`/initramfs` KernelServer** (`KernelServerId::Initramfs` + `initramfs_server`): the
  first **subtree** server (uses the lookup suffix, unlike the entropy/`proc/self`
  leaves) — resolves `suffix` in the blob and returns a fresh read-only `MemoryObject`
  **copy** of the file bytes (the caller maps it `MAP_READ`). Bound into pid 1's root
  namespace at boot with `MAP_READ` + generic rights.
- **`MemoryObject::try_new_filled(bytes)`** — the first *synthesised read-only
  MemoryObject* primitive (allocate → copy bytes into the zeroed frames via the HHDM →
  hand back), which the deferred `/proc/self/status` will reuse. A **copy**, not an
  alias of the blob's frames: CPIO-newc aligns file data to 4 bytes, not pages, so
  frame-aliasing isn't possible; copy-per-lookup is fine for a bootstrapping init.

Reclamation (`sys_release_initramfs`) intentionally **not** built — deferred to the
general resource-server lifecycle work; the blob stays mapped. No new syscall; no ABI
change (internal `KernelServerId` variant + Limine struct). Verified: `cargo xtask
build` / `check-arch` / `test` (kernel 462 +4 initramfs +1 `try_new_filled`; libkern 8) /
`qemu` all clean — the parent resolves+maps `/initramfs/etc/init.toml` and prints
`"# Nitrox init ma..."`; all prior demos pass. Branch `phase-2/slice4-initramfs` off
`main`. Next: Part 3 (init crate skeleton — bare target, allocator, handle reception).

## 2026-06-23 — Phase 2 slice 4, Part 3: init crate skeleton (+ two userspace-runtime fixes)

Converted `userspace/init` from a host stub to the bare-target PID-1 skeleton:
`#![no_std]` + `alloc`, libkern-only. It's a **library + binary** crate — the lib
(`src/lib.rs` + `src/heap.rs`) holds host-testable logic, the bin (`src/main.rs`) is the
entry point. `_start` receives the bootstrap registers (notif, root namespace), reports
the handle set, proves the allocator, and exits cleanly; the `#[panic_handler]` reports
+ spins (never a bare `panic!`, per `userspace/init/CLAUDE.md`).

**Allocator: a fixed static-arena bump allocator** (`init::heap::BumpAlloc`,
`#[global_allocator]`) over a 64 KiB arena; `dealloc` is a no-op (init's working set is
bounded and freed wholesale on exit). The pure offset math (`bump`) is host-tested
(`cargo xtask test` now runs `cargo test -p init --lib`). Arena size is the open
question flagged in the plan — 64 KiB for now.

**Demo + spawn:** `ImageId::Init` (=1) added to `spawn.rs` + `embedded_images.rs`
(kernel embeds the init ELF; xtask builds it before the kernel); `IMAGE_INIT` mirror in
libkern. The `parent` demo spawns init via `ImageId::Init` and reaps its `ChildExited`
as a regression check (init becomes the real kernel-loaded PID 1 in Part 5).

**Two real userspace-runtime bugs surfaced by init's first `alloc` use** (the long-
flagged 2026-06-22 hazards, now hit for real):
1. **Mis-placed `compiler_builtins` `memcpy`.** On this `x86_64-unknown-none`,
   `-sse,+soft-float` target the `memcpy` symbol resolved into the *middle* of an
   unrelated function — a call jumped into garbage (`mov 0x8(%rbx),…` → the `@0x8`
   fault). Fix: **`libkern::mem`** provides strong `#[no_mangle]` `memcpy`/`memmove`/
   `memset`/`memcmp` (volatile byte loops, so LLVM can't refold them into a self-call),
   which override `compiler_builtins`' weak versions for every userspace bin. Gated
   `#[cfg(not(test))]` so the host `std` build of libkern doesn't redefine libc's `mem*`.
   This is the deferred Part-0 Piece-2 mem-intrinsics work, landed where its first
   consumer needed it.
2. **`/DISCARD/`-ed `.got`.** `alloc`'s `__rust_realloc` shim tail-calls `memcpy` via
   `jmp *[GOT]`; `user.ld` discarded `.got`, so the indirect jump pointed at null (the
   `@0x0` fault). Fix: **keep `.got`/`.got.plt` in a loaded segment** (the linker fills
   them with absolute addresses for a static non-PIE binary); still discard `.plt`.
   Applied to all four userspace `user.ld` (init + the three demos) for consistency,
   though only init currently exercises the path.

No kernel ABI hash impact: `ImageId::Init` is a new enum value (1 was previously
rejected), not a layout/discriminant change to a hashed type. Verified: `cargo xtask
build` / `check-arch` / `test` (kernel 462; libkern 8; **init 3**) / `qemu` all clean —
parent spawns init → `init: alloc ok (sum…=140)` → `init exited pid=2 code=0`; all prior
demos pass. Branch `phase-2/slice4-init-skeleton` off `main`. Next: Part 4 (minimal TOML
parser + `init.toml` parsing).

## 2026-06-23 — Phase 2 slice 4, Part 4: minimal TOML parser + init.toml manifest

Added init's hand-rolled config front end (lib-side, host-tested — per
`userspace/init/CLAUDE.md`, init parses TOML itself rather than pulling an ecosystem
crate):

- **`init::toml_lite`** — a minimal, line-oriented TOML parser for exactly the subset
  `docs/spec/init-toml-schema.md` uses: table arrays `[[name]]`, one-level subtables
  `[name.sub]` (attached to the most recent `[[name]]` element), scalar values (basic
  strings without escapes, decimal integers with `_` separators, booleans),
  whole-line + trailing `#` comments (quote-aware), blank lines. Returns a `Document`
  (`array(name) -> &[ArrayEntry]`, each with a `Table` + named subtables) or a
  `ParseError` carrying the 1-based line number. Unsupported forms (bare `[table]`,
  deeper nesting, arrays/inline-tables/multiline) are rejected with a clear error —
  upgrade the parser if the schema ever needs them.
- **`init::manifest`** — validates the `[[mount]]` entries into `Vec<MountSpec>`
  (`fs_server`/`device`/`mount_point`/`mode`/`required_for` required; `mode` ∈
  {ro, rw}; `required_for` == "boot"; `mount_point` absolute; `[mount.options]` kept
  verbatim for the slice-7 Ready handshake) and **topologically sorts by mount-point
  depth** (shallowest first, stable so equal depths keep file order). Typed
  `ManifestError` variants (missing field, wrong type, bad mode, …) for the eventual
  eshell diagnostics.

The mount *processing* loop (spawn fs-server → Ready → `sys_ns_bind`) stays Part 5 /
slice 7; this is the pure front half. Host tests are the deliverable: 15 new (`cargo
xtask test` → init 18). A small on-target smoke test in init's bin parses an embedded
out-of-order sample and prints the sorted mounts (`/` before `/store`), proving the
parser + `String`/`Vec` run under the bump allocator. Reading the *real*
`/initramfs/etc/init.toml` is Part 5.

No kernel/ABI change (userspace-only). Verified: `cargo xtask build` (no warnings) /
`check-arch` / `test` (kernel 462; libkern 8; **init 18**) / `qemu` all clean. Branch
`phase-2/slice4-toml` off `main`. Next: Part 5 (init becomes PID 1 + reaping loop +
bootstrap-flow skeleton, reading the real init.toml).

## 2026-06-23 — Fix: intermittent #DF from KERNEL_GS_BASE poisoning (syscall GS model)

Bringing up init as PID 1 (Part 5) reliably exposed a pre-existing intermittent kernel
double-fault. Root cause: the x86_64 syscall entry stub ran the **whole kernel body**
with `GS_BASE = &CPU0` and the user GS parked in `KERNEL_GS_BASE` (one `swapgs` on
entry, undone only by a matching `swapgs` before `sysretq`). A thread that **blocks
mid-body** (`sys_wait`) is switched away with `KERNEL_GS_BASE = 0` (the parked user GS);
a *sibling's* subsequent `syscall` entry `swapgs`es that `0` into `GS_BASE`, and the next
instruction `mov gs:[scratch], rsp` writes through address 0 → `#PF` on the entry path →
`#DF`. `enter_user` and the IDT traps already assumed `GS_BASE = 0` /
`KERNEL_GS_BASE = &CPU0`; only the syscall body diverged. init-as-PID-1 added a third
blocked-in-syscall process while parent's worker thread faulted — the interleaving that
hit the window ~1/3 of boots.

**Decision — hold `KERNEL_GS_BASE = &CPU0` at all times.** The entry stub now `swapgs`es
**back** to the userspace GS state right after grabbing the kernel stack (reading the
stashed user RSP before the swap-back), so the body runs with `GS_BASE = 0`,
`KERNEL_GS_BASE = &CPU0` — the state userspace, `enter_user`, and the trap path expect.
The exit `swapgs` before `sysretq` is dropped (the body already holds that state). A
blocked thread can no longer leave `KERNEL_GS_BASE = 0` to poison a sibling's entry; the
body never touches `gs:` (only the stub does), and the IDT entries never `swapgs`, so
both rings are consistent at `GS_BASE = 0`. ~2 instructions + doc rewrite; no ABI change.
Verified 10/10 clean `qemu` runs under the reproducer (was ~1/3 `#DF`). Branch
`phase-2/gs-base-fix` (PR #57).

## 2026-06-23 — Phase 2 slice 4, Part 5: init becomes PID 1 (reaping loop + bootstrap skeleton)

The integration step: init is now the real first userspace process. The kernel boots it
(`run_first_userspace` loads the embedded init ELF via `embedded_images::image_bytes(
ImageId::Init)` instead of the parent ELF); init's root namespace carries the boot
kernel-server bindings at full rights. init:
1. reports its handle set;
2. reads + parses the **real** `/initramfs/etc/init.toml` (namespace lookup → map the
   read-only `MemoryObject` → trim the zero-padded tail → `manifest::parse`) and logs
   the topo-sorted mount plan;
3. spawns `parent` (now `ImageId::Parent`, embedded) as the slice-1/2/3 demo chain;
4. enters the reaping loop (`sys_wait` → `ChildExited` → close the child handle).

So the process tree is now **init (1) → parent (2) → child (3/4)**, the shape it should
have. The mount *processing* (spawn fs-server → Ready → `sys_ns_bind`) stays slice 7 —
there are no fs-servers or block devices yet, so init logs the plan + "deferred to slice
7" rather than mounting; it does **not** drop to eshell. `parent`'s `ns_demo` was changed
to bind into a **fresh** namespace it creates (its inherited root is now LOOKUP-only
under init, so binding into root would be denied — a process binds into namespaces it
owns); all other parent demos use root only via LOOKUP and are unaffected.

`ImageId::Parent` (= 2) added (`spawn.rs` + `embedded_images.rs`; `IMAGE_PARENT` mirror
in libkern) — a new enum value, not a layout change, so no ABI-hash impact. This part
depends on the GS fix above (the multi-thread fault demo would otherwise `#DF`). Verified:
`cargo xtask build` (no warnings) / `check-arch` / `test` (kernel 462; libkern 8; init
18) / `qemu` (full `init → parent → child` transcript, init reaps parent, no `#DF` across
repeated runs). Branch `phase-2/slice4-init-pid1` off `main`. Next: slice 5 (storage
drivers) toward the milestone where init actually mounts a root fs.

## 2026-06-23 — Phase 2 slice 5, Part 0: storage-slice specs & decisions

The paper layer for the storage slice — the ABI and object contracts settled
before any of it gets baked into the kernel ABI hash. New specs:
[`docs/spec/io-operation.md`](../spec/io-operation.md) (`IoOp`/`IoOpcode`),
[`docs/spec/irp-layout.md`](../spec/irp-layout.md) (`Irp` + sub-types), and
[`docs/spec/device-node.md`](../spec/device-node.md) (the `DeviceNode` object,
resource descriptor, and block-device naming). `syscall-abi.md`,
`abi-version-hash.md`, and `deferred-decisions.md` updated to match. The
consequential decisions:

- **Block I/O goes through the existing generic `sys_io_submit(resource, &IoOp)`,
  not a bespoke `sys_block_read`.** The async-I/O core (`sys_io_submit` /
  `sys_io_cancel` / `IoOp` / `IoOpcode`) was already sketched in `syscall-abi.md`
  and already named in the ABI hash; Part 0 makes it concrete rather than adding a
  parallel surface. The future `IoRing` submits the same `IoOp`. Numbers assigned:
  `sys_io_submit = 28`, `sys_io_cancel = 29` (the latter `Unsupported` in Phase 2
  — cancellation is deferred).

- **`sys_io_submit` targets a `DeviceNode` handle; there is no separate
  "BlockDevice" `KObjectType`.** A block device *is* a `DeviceNode` (class
  `Block`), whether a whole disk (AHCI) or — slice 6 — a partition (GPT, a second
  IRP stack frame over the disk). This matches the plan's existing "Partition
  DeviceNode registration" (slice 6) and avoids a new hashed `KObjectType`
  (`DeviceNode = 12` already exists as a reserved tag). Rejected: a dedicated
  `BlockDevice` object (proliferates types; partitions wouldn't be DeviceNodes).

- **Dynamic disks resolve through one `KernelServerId::BlockDevice` backed by a
  kernel block-device registry, keyed by lookup suffix.** The RS framework is a
  static enum but devices are discovered at runtime; one binding at `/dev/blk`
  resolves `/dev/blk0`, `/dev/blk1`, … by consulting an enumeration-time table.
  Whole disks get **enumeration-order** names (`/dev/blkN`); content-stable
  `/dev/disk/by-partuuid/*` names are slice 6 (they need GPT metadata and are
  what `init.toml` references). The `/dev/blk` binding is **read-only** in
  Phase 2 (RO `fs-server-ext4`).

- **`Irp` is hashed though userspace never sees it** — Tier 2 driver modules walk
  `&mut Irp`, so the layout is fixed now (`IRP_MAX_STACK = 4`: AHCI = 1 frame,
  GPT-over-AHCI = 2). `IrpStatus` maps directly onto `IoResult.status` (0 /
  negative `KError`), and `IrpOp` is kept numerically aligned with `IoOpcode`.

- **In-kernel MMIO, not `sys_device_map_mmio`.** Phase 2's AHCI is Tier 1
  (in-kernel), so it maps its ABAR in *kernel* space uncached via the existing
  `PageFlags::NO_CACHE` paging path. `sys_device_map_mmio` (the userspace-driver
  path) stays deferred with userspace drivers + IOMMU.

- **`InterruptObject` is built in slice 5** (decided with the user): the waitable
  mechanics + signal-from-DPC land in Part 2 (exercised by a synthetic DPC
  signal), and the AHCI ISR signals a real `InterruptObject` in Part 3 — even
  though a Tier 1 driver could complete IRPs purely via the DPC-completion-routine
  pattern. Building it now completes the design and pre-builds the
  userspace-driver (Tier 2) programming model.

- **Kernel-server liveness model clarified** (in response to a design question).
  The set of Kernel Server *implementations* stays a static enum (it is kernel
  code); the set of *resources/binding points* is dynamic. `BlockDevice` is the
  first **registry-backed instance server** — one static variant owning the
  `/dev/blk` subtree, resolving the suffix against a runtime registry. `/dev/blk`
  is bound **unconditionally** and the registry carries liveness (no per-server
  enable switch). What is conditional is **drivers, not servers**: AHCI/NVMe are
  *drivers* enabled by device *matching* (hardware presence = enable), feeding one
  block server. End-state, driver-to-node matching and server binding graduate to a
  userspace device manager + supervisors; substitutability makes the move
  client-invisible. Documented in `namespace-and-resource-servers.md` §§ "Kernel
  Server shapes" / "Liveness" and `device-node.md`.

Implementation staging (also recorded in `implementation-plan.md`): Part 1 — PCI
enumeration + `DeviceNode`; Part 2 — IRP framework + `InterruptObject` + the I/O
core, proven on a RAM-backed test block device (de-risks the async spine before
AHCI register work, decided with the user); Part 3 — AHCI Tier 1 driver + the
QEMU AHCI test disk; Part 4 — block resource server + `/dev/blk` namespace
binding. No code yet — Part 0 is docs only. Next: Part 1.

## 2026-06-23 — Phase 2 slice 5, Part 1: PCI(e) enumeration + DeviceNode

The first code of the storage slice. The kernel now enumerates PCI(e) at boot and
represents each function as a `DeviceNode` kernel object.

- **`DeviceNode` object** (`kernel/src/object/device_node.rs`): header-first
  `#[repr(C)]` object (`KObjectType::DeviceNode`, the previously-reserved tag =
  12) holding a `DeviceClass` (`Other`/`Block`), a `ResourceDescriptor`
  (`DeviceIdentity`, six `BarWindow`s, `InterruptSpec`, bus address), and
  `BlockGeometry` (zeroed until a block driver claims it). Wired into
  `dispatch_destroy` + the test-probe; the type-rights arm
  (`DEVICE_NODE_PRINCIPALS = READ`) already existed.

- **PCI enumerator** (`kernel/src/pci/mod.rs`, neutral — PCI config space is a
  PCI-SIG standard): walks every bus/dev/func in each ECAM region, decodes
  identity, sizes BARs (32-bit, 64-bit, and I/O, with decode disabled during
  sizing), reads the legacy interrupt line/pin, and builds a `DeviceNode` per
  present function. Multi-function devices are expanded via the header-type bit.
  The decode/sizing logic is behind a `Cfg` trait so it is host-tested against a
  synthetic config space that faithfully models the write-all-ones/read-back
  sizing protocol (incl. a 64-bit prefetchable BAR).

- **Config-space access**: each function's 4 KiB config space is 4 KiB-aligned
  MMIO. Since the vmap bump allocator never reclaims VA, enumeration reserves
  **one** vmap page and repoints it per function with the new
  `mm::kvmap::remap_mmio_page` (uncached map + TLB flush) — rather than mapping
  the multi-hundred-MiB ECAM window or leaking a reservation per function. This
  is the reusable uncached-MMIO primitive AHCI's ABAR mapping builds on (Part 3).

- **Device table** (`kernel/src/device.rs`): a neutral global `SpinLock<KVec<
  ObjectRef>>` populated once at boot by `device::init()` (called after the
  handle table); holds an owning reference per device (devices live for the
  kernel's lifetime). Part 3 (driver matching) iterates it; Part 4 (block server)
  resolves through it.

`InterruptSpec` gained raw `line`/`pin` fields beyond the Part 0 sketch (they are
the inputs to Part 3's interrupt routing); `device-node.md` updated to match
(source wins — the descriptor is not an ABI-hash input). No ABI-hash impact (no
new boundary layout; `DeviceNode`'s discriminant already existed).

Verified: `cargo xtask build` (no warnings) / `check-arch` / `test` (kernel
**471**, +9: 6 `pci` + 3 `device_node`; libkern 8; init 18). QEMU q35 boot logs
the ICH9 AHCI controller (`8086:2922`, class `01.06.01`) with its ABAR (BAR5,
`0x810c4000`), plus the host bridge, VGA, e1000, LPC and SMBus functions — 6
nodes registered — and proceeds through init→parent→child cleanly (no `#DF`).
Branch `phase-2/slice5-pci-enum`. Next: Part 2 (IRP framework + InterruptObject +
the I/O core, on a ramdisk).

## 2026-06-23 — Phase 2 slice 5, Part 2: IRP framework + InterruptObject + the I/O core

The async I/O spine, proven on a RAM-backed device before any real driver.

- **`Irp`** (`kernel/src/io/irp.rs`): the kernel-internal I/O request packet per
  `docs/spec/irp-layout.md`, with the hashed leading-field offsets pinned by
  compile-time asserts (`IrpOp`/`IrpStatus`/`IrpBuffer`/`IrpStackFrame`/`PhysFrag`).
  Owning references (PO, buffer, device) live in a kernel-internal `#[repr(C)]`
  `IrpBox` wrapper with `irp` first, so `*mut Irp == *mut IrpBox` and a Tier 2
  module only ever sees the clean hashed `Irp`.

- **`InterruptObject`** (`kernel/src/object/interrupt_object.rs`): a waitable IRQ
  source (`KObjectType::InterruptObject`, the reserved tag = 8), modelled as a
  **latching edge counter** mirroring `PendingOperation`'s scheduler-only pattern.
  An ISR/DPC `signal`s it (latches a pending count, woken with no waiter is not
  lost); a `sys_wait` return **consumes** one, so a driver's wait→service→wait
  loop wakes once per interrupt. Added as the third waitable to the three sched
  dispatch arms + `signal_interrupt` (DPC-callable, takes `SCHED`) + `interrupt_
  consume` (called at `sys_wait` result-build). Built this slice per the earlier
  decision, though Tier 1 AHCI could complete purely via the DPC routine.

- **Block I/O core** (`kernel/src/io/block.rs`): a `BlockBackend` (a `submit` fn
  pointer + ctx) installed on a block `DeviceNode`; `dispatch_block_irp` builds
  the IRP (a PRDT-style `PhysFrag` list cut from the buffer `MemoryObject`'s
  frames over `[buf_offset, +length)`), arms the inline completion DPC, and hands
  it to the backend. The completion DPC signals the request's PO (result = bytes
  transferred) and reclaims the box.

- **Syscalls**: `sys_io_submit`(28) / `sys_io_cancel`(29) wired
  (`docs/spec/io-operation.md`); submit mirrors `sys_ns_lookup`'s PO-create +
  resolve + rollback, dispatching through `dispatch_block_irp` (zero-length is a
  pre-signalled no-op); cancel is `Unsupported` (deferred). `IoOp`/`IoOpcode`
  added to the kernel and userspace `libkern` mirrors (offsets asserted both
  sides). `InterruptObject` added to `sys_wait`'s accepted types.

- **Ramdisk** (`kernel/src/io/ramdisk.rs`): a RAM-backed block device whose
  `submit` does the transfer (a `memcpy` across the frags via the HHDM) and queues
  the completion DPC — standing in for a controller's DMA + completion ISR. Boot
  bug found + fixed: building the backing as `KBox::<[u8; 64K]>::try_new([0; 64K])`
  materialised a 64 KiB array on the kernel stack → stack overflow → boot hang;
  rebuilt via a `KVec` push loop (no large stack temporary).

Proof is a kernel boot self-test (`io::self_test`, called after `device::init`):
a ramdisk read of 8 KiB completes through IRP→DPC→PO with the buffer content
verified, and a DPC-signalled `InterruptObject` latches then consumes. The
userspace `sys_io_submit` path is exercised once `/dev/blk` exists (Part 4).

No ABI-hash impact in practice (no module loaded against the hash yet); `IoOp`
(40 B) and the `Irp` layout are now concrete in code as the specs fixed them.
Verified: `cargo xtask build` (no warnings) / `check-arch` / `test` (kernel
**481**, +10: `io::irp` 3, `io::block` 2, `io_op` 1, `interrupt_object` 4; libkern
8; init 18). QEMU q35: both self-tests print OK and boot proceeds through
init→parent→child cleanly (no `#DF`). Branch `phase-2/slice5-irp-iocore`. Next:
Part 3 (AHCI Tier 1 driver).

## 2026-06-25 — Phase 2 slice 5, Part 3: AHCI Tier 1 driver

The first real hardware driver — SATA reads through the Part 2 I/O spine.

- **AHCI driver** (`kernel/src/drivers/ahci.rs`, neutral kernel code — PCI/AHCI
  are standards): maps the controller's ABAR (BAR5) uncached via the new
  `mm::kvmap::map_mmio`; enables AHCI mode; finds the first implemented port with
  a SATA disk; brings the port up (stop → point PxCLB/PxFB at `DmaBuffer`s → clear
  errors → start); runs `IDENTIFY DEVICE` **polled** (bring-up runs with
  interrupts masked) for the LBA48 sector count; and publishes the disk as a
  block `DeviceNode` (the Part 1 object) with an AHCI `BlockBackend` (the Part 2
  seam). Reads issue `READ DMA EXT` with a PRDT built directly from the IRP's
  buffer fragments — the controller DMAs straight into the client's
  `MemoryObject` frames (no bounce buffer).

- **Driver matching** (`kernel/src/drivers/mod.rs`): `probe` snapshots the device
  table (`device::snapshot`, cloned refs so no lock is held across driver alloc)
  and brings up class-`01.06.01` controllers. `drivers/` is the Tier 1 driver
  home.

- **Real IRQ path**: `crate::arch::install_pci_irq(gsi, handler)` — a **neutral
  free function** (not a method) that composes three hardware abstractions the
  arch traits intentionally keep separate: the device-vector **handler registry**
  (the IDT on x86, which `ArchIrqRouter` keeps off itself), the **local
  controller** (`ArchIrq::id` for the dest CPU), and the **router**
  (`ArchIrqRouter::route`, the resolved-`(line, vector)` primitive). It routes
  level-triggered/active-low INTx on the BSP. Belonging to none of those single
  abstractions, it stays a composite helper rather than a method on any of them.
  A `TODO(msi)` marks the trigger to promote the device-interrupt *family* (MSI
  install, shared-INTx chaining, IRQ teardown) into a dedicated `ArchIrqInstall`
  trait once it has a second member (see `deferred-decisions.md`). The AHCI ISR
  acks the
  controller, completes the in-flight IRP via its DPC (the Tier 1
  DPC-completion-routine pattern), and queues a DPC that signals the controller's
  `InterruptObject` (exercises the signal-from-real-ISR path; no waiter in
  Phase 2). **GSI from the PCI interrupt-line register** (config 0x3C) —
  firmware-programmed on QEMU (observed GSI 10); ACPI `_PRT` routing stays
  deferred (needs AML).

- **Proof**: rather than a new disk, read the **existing AHCI boot disk** (OVMF
  booted Limine from it). `drivers::self_test` reads sector 0 and verifies the
  `0x55AA` boot signature, mirroring the IOAPIC PIT self-test — issue the read,
  briefly enable interrupts so the completion IRQ can fire, with a bounded polled
  fallback (`ahci::poll_complete_inflight`) if the GSI is unrouted. The dedicated
  `xtask build-disk` + ext4 disk move to Part 4/7, where the fs-server needs real
  filesystem data.

- **`KError::IoError` (-40)** added (kernel + userspace `libkern`) for
  device/medium errors (the ATA task-file-error path), per `io-operation.md`. This
  is a `KError`-layout change → an ABI-hash input, but no module is loaded against
  the hash yet, so no practical impact (like the slice's other ABI additions).

Phase 2 scope: single controller, single SATA disk, one outstanding command
(slot 0). Multi-port/multi-disk, NCQ, port multipliers, and MSI/MSI-X are
deferred (`deferred-decisions.md`).

Verified: `cargo xtask build` (no warnings) / `check-arch` (the driver is neutral;
MMIO/IRQ via `crate::arch` + `mm`) / `test` (kernel 481; libkern 8; init 18 — AHCI
is hardware, exercised on-target not host). QEMU q35: `ahci: HBA up`, `port 0 disk
ready (131072 sectors, 64 MiB)`, `INTx GSI10 -> vec 0x31`, `read self-test OK
(sector 0 boot sig 0x55AA, via IRQ)` — **4/4 clean boots**, completion via the
real IRQ, boot proceeds through init→parent→child (no `#DF`). Branch
`phase-2/slice5-ahci`. Next: Part 4 (block resource server + `/dev/blk`).

## 2026-06-25 — Phase 2 slice 5, Part 4: block resource server + /dev/blk (slice complete)

The slice's close-out: `sys_io_submit` wired to userspace through the namespace.

- **`KernelServerId::BlockDevice`** + `block_device_server` (`object/kernel_server.rs`):
  a **subtree** Kernel Server bound once at `/dev/blk`; the lookup suffix is a
  decimal index (`/dev/blk/0` → `b"0"`, parsed by `parse_index`) resolving to the
  n-th block-class `DeviceNode`. The registry is `device::find_block_device(n)` —
  a query over the existing device table (block-class entries in
  driver-publish order), not a separate structure. Returns the node handle via
  `OpStatus::Completed`; out-of-range / non-numeric / empty suffix → `NotFound`.

- **Boot binding** (`main.rs`): the supervisor binds `/dev/blk` →
  `KernelServerId::BlockDevice` **read-only** (`READ` + generic band),
  **unconditionally** — the device-table registry carries liveness (an empty
  registry just yields `NotFound`), so there is no per-server enable switch. This
  is the Part 0 design realised: one static enum variant owning a subtree backed
  by a runtime registry.

- **Naming correction.** Namespace prefix matching is **component-boundary**
  (`match_suffix_offset`: the prefix must end on a `/`), so `/dev/blk` covers
  `/dev/blk/0` (suffix `0`), **not** `/dev/blk0`. The disks therefore live at
  `/dev/blk/0`, `/dev/blk/1`, … — corrected from the Part 0 spec's `/dev/blk0`
  (source wins). `device-node.md` / `namespace-and-resource-servers.md` /
  `deferred-decisions.md` updated; the historical Part 0 log entry is left as-is
  (append-only).

- **Userspace proof** (`userspace/parent`): a `block_demo` resolves `/dev/blk/0`
  (`ns_lookup_wait`), creates + maps a page buffer (`MAP_READ | MAP_WRITE` — the
  controller DMAs into it), builds an `IoOp` read of 512 bytes at LBA 0, calls
  `sys_io_submit`, `sys_wait`s the returned PO (`po_wait`), and verifies the
  `0x55AA` boot signature. This is the full userspace `sys_io_submit` path the
  Part 2/3 kernel self-tests stood in for.

**Slice 5 (storage drivers) is complete.** The end-to-end result: a userspace
process resolves a block device through its namespace and reads real disk sectors
asynchronously — PCI enumeration → `DeviceNode` → IRP/`InterruptObject`/I/O core →
AHCI driver (real DMA + IRQ) → block resource server → `/dev/blk/0`. Read-only,
single disk; GPT/partitions (slice 6) and the userspace fs-server (slice 7,
with the dedicated ext4 test disk) are next.

No ABI-hash impact (a new `KernelServerId` value, not a hashed layout — `dispatch`
is internal). Verified: `cargo xtask build` (no warnings) / `check-arch` / `test`
(kernel **482**, +1 `parse_index`; libkern 8; init 18). QEMU q35: `parent:
/dev/blk/0 read OK (sector 0 boot sig 0x55AA)`, 4/4 clean boots, init→parent→child
reaped (no `#DF`). Branch `phase-2/slice5-block-server`.

## 2026-06-25 — Phase 2 slice 6: GPT partitions (the first two-layer block stack)

Partition handling — the first **two-layer block IRP stack**.

- **The partition layer** (`io::block::Partition`): a partition is a block
  `DeviceNode` whose `BlockBackend::submit` (`partition_submit`) bounds-checks the
  partition-relative request, **rebases** the offset to disk-absolute
  (`partition_rebase`, pure + unit-tested), and forwards the IRP to the parent
  disk's backend. The two layers are realised by **backend delegation** (partition
  → disk), not formal `IrpStackFrame`/`stack_index` descent — simpler and correct
  for Phase 2's shallow stacks; the formal stack machinery (with per-frame
  completion routines) stays designed-ahead for filter drivers (encryption/LVM).
  Completion flows back through the one IRP DPC unchanged. The Part 2 `BlockBackend`
  seam made this a ~30-line addition.

- **Synchronous boot read**: the GPT driver must read the disk during
  `drivers::probe`, when interrupts are masked. Added `BlockBackend::poll` (AHCI
  wraps its existing `poll_complete_inflight`; a partition delegates to the disk's;
  the ramdisk drains its DPC) + `io::block::read_blocking(device, lba, count, dst)`
  — submit a normal IRP, drive it to completion by polling, copy out. No new async
  machinery.

- **GPT driver** (`drivers/gpt.rs`, Tier 1, neutral): validates the `EFI PART`
  signature + bounds (header/array CRC32 deferred), walks the entry array
  sector-by-sector (bounded, no large buffers), and publishes each used partition.
  Records `(node, by-partuuid path, by-partlabel path)` for the namespace bindings.

- **Naming**: partitions appear at `/dev/blk/<n>` automatically (they are
  block-class `DeviceNode`s in the device-table registry — the ESP at
  `/dev/blk/1`). Plus stable content-derived **direct-handle** bindings created at
  boot by `gpt::bind_partition_names`: `/dev/disk/by-partuuid/<uuid>` (GPT GUID
  formatted mixed-endian — first three fields LE, last two BE) and
  `/dev/disk/by-partlabel/<label>` (UTF-16 → ASCII). Read-only (`READ` + generic
  band). DirectHandle, not a new Kernel Server: the partitions exist at boot, so
  the supervisor binds them directly (snapshotting the registry first to avoid
  holding it across the namespace lock).

- **No new disk**: the existing GPT boot disk (`xtask image`: `sgdisk` ESP at LBA
  2048, label `NITROX_ESP`) is parsed directly. The dedicated ext4 test disk stays
  a slice-7 (fs-server) concern.

Proof: QEMU logs `gpt: partition 0 lba 2048..131038 (128991 sectors)`; the `parent`
demo reads sector 0 of `/dev/blk/0` (disk), `/dev/blk/1` (partition — proving the
rebase, sector 0 of the partition is disk LBA 2048), and
`/dev/disk/by-partlabel/NITROX_ESP`, all verifying `0x55AA`. `partition_rebase`
unit-tested (LBA 0 → 2048 + bounds rejection).

No ABI-hash impact (no hashed-layout change; `BlockBackend`/`Partition` are
kernel-internal). Verified: `cargo xtask build` (no warnings) / `check-arch` (the
GPT driver is neutral) / `test` (kernel **486**, +4: 3 `gpt` + 1 `partition_rebase`;
libkern 8; init 18). QEMU q35: GPT parsed, three reads OK, init→parent→child
reaped (no `#DF`). Branch `phase-2/slice6-gpt`. Next: slice 7 (userspace fs-server).

## 2026-06-25 — Phase 2 slice 7, Part 1: librsproto wire codec

The first piece of the first userspace resource server. `userspace/librsproto/`
— the resource-server protocol wire codec — built as a **pure `no_std`, no-`alloc`,
no-dependency** crate: it serialises/parses messages in a caller-provided buffer
(the `IpcMsg.payload`); transferred handles ride out-of-band in `IpcMsg.handles[]`
(the codec only tracks `handle_count`); `kerror` is a plain `i32`. So librsproto
needs neither libkern nor alloc — it is a self-contained byte codec.

- **Envelope** (`lib.rs`): the 28-byte `RsMsgHeader` (`encode`/`decode` with magic
  + `body_len` validation), the op/flag constants, and `RsError`. Bodies are
  serialised with **explicit little-endian byte helpers** (`put/get_u16/32/64`),
  not `#[repr(C, packed)]` field references — robust regardless of alignment.
- **Meta bodies** (`meta.rs`): Hello (request/reply), Ping, Ready. Goodbye/QueryCaps
  are constants for now (no body codec until a consumer needs them).
- **Namespace::Resolve** (`namespace.rs`) — the new op slice 7 defines (the
  kernel-forwarded lookup): `ResolveRequest { requested_rights, flags, suffix }`
  → `ResolveReply { object_kind, content_len }` with the resource handle in
  `handles[0]`. Spec: `docs/spec/rsproto-namespace-ops.md` (fills the TBD the wire
  spec reserved). `RESOLVE_FILE_AS_MEMOBJ` + a 64 KiB content cap are the Phase-2
  mode (slice 8 lifts the cap with the lazy page cache).
- **ErrorBody** (`error.rs`): the standard error reply (`kerror`/`server_code`/
  optional message).

This is a kernel↔server ABI (the kernel hand-codes Resolve in Part 3); librsproto
is the userspace mirror the fs-server uses. No `RsClient` (sync client) — slice 7
has no userspace client (the kernel forwards; init hand-parses Ready); eshell
(slice 9) is its first consumer.

No kernel changes, no ABI-hash impact. Verified: `cargo xtask build` (no warnings;
librsproto also builds for the bare `x86_64-unknown-none` target) / `check-arch` /
`test` — librsproto **11** host round-trip tests pass (kernel 486; libkern 8; init
18 unchanged). Branch `phase-2/slice7-librsproto`. Next: Part 2 (ext4 read-only
reader, host-testable).

## 2026-06-25 — Phase 2 slice 7, Part 2: ext4 read-only reader

The filesystem parsing core, as a host-testable library. `userspace/fs-server-ext4/`
(lib-only for now; the server `[[bin]]` is Part 4).

- **`BlockReader` trait** (`read_at(offset, buf)`) is the seam: the parser is
  written entirely against it, so 100% of the logic is host-tested against an
  in-memory image; the real fs-server (Part 4) implements it over `sys_io_submit`.
- **No `alloc`**: `read_file(path, out) -> size` reads into a caller-provided
  buffer (the fs-server passes a bounded scratch ≤ 64 KiB = `ext4::MAX_FILE`, the
  slice-7 read-model cap); parsing uses bounded stack scratch (≤ one 4 KiB block).
  So the reader stays buffer-based like libkern/librsproto.
- **Minimal RO ext4** (`ext4.rs`): superblock (`0xEF53`; reject 64-bit feature +
  > 4 KiB blocks), block-group descriptors (inode-table location), inode fetch,
  the **extent tree** (`0xF30A`, depth-0 leaves + index-node recursion), a linear
  `ext4_dir_entry_2` directory walk, and path resolution from the root inode (2).
  All structure reads are bounds-checked → a malformed image yields `FsError`,
  never a panic/OOB. Skips: journal, bigalloc, inline-data (rejected), htree
  (linear walk is backward-compatible), 64-bit blocks, RW, xattrs, checksums.

**Tests run against real ext4.** The host tests build a fixture with **`mke2fs -d`**
(populate-at-creation, no root/mount; features `^has_journal,^64bit,^metadata_csum,
^resize_inode` mirroring the Part-5 disk) and parse it — validating the reader
against e2fsprogs output, not just a self-consistent hand-built image. 6 tests:
read `/system/current-generation` at 1 K and 4 K block sizes, missing-path /
not-a-regular-file → `NotFound`, oversize buffer → `TooLarge`, non-ext4 →
`Corrupt`. (`mke2fs` is a project dependency — also used by Part 5.)

No kernel changes, no ABI impact. Verified: `cargo xtask build` (no warnings; the
reader also builds bare `x86_64-unknown-none`) / `check-arch` / `test` —
fs-server-ext4 **6**, librsproto 11, kernel 486, libkern 8, init 18. Branch
`phase-2/slice7-ext4-reader`. Next: Part 3 (kernel transparent-forwarding, proven
with a stub server) — the hard part.

## 2026-06-25 — Phase 2 slice 7, Part 3: kernel transparent namespace forwarding

The slice's hard core: a client's `sys_ns_lookup` of a path bound to a **Userspace
Server** is forwarded by the kernel over IPC — the kernel becomes an async IPC
*client* of the server, sending a `Namespace::Resolve` and later completing the
lookup's `PendingOperation` from the reply. This realizes the `UserspaceServer`
binding target + `OpStatus::Pending` the slice-3 framework reserved.

**Decision — the `UserspaceServerReg` kobject (type 13) is the registration's
home.** Type 13 was reserved at slice-3 design time "to tag the userspace-server
registration object." It now exists (`kernel/src/object/userspace_server.rs`): an
internal (never user-facing) kobject owning the kernel's IPC endpoint `ObjectRef`
plus the **pending-lookup table** `{ request_id, po, owner_pid, requested }`. A
`BindingTarget::UserspaceServer(ObjectRef)` holds it. The alternative — boxing the
forwarding state onto every `IpcChannel::Inner` — was rejected: it bloats the
common channel and contradicts the reserved tag. The only `IpcChannel` addition is
a single `us_reg: *mut ()` back-pointer (null on ordinary channels) so a reply
*reaching* the kernel endpoint can find its registration. The registration owns the
endpoint's only reference, so the back-pointer is valid for the endpoint's whole
life. Sized **N = 1** (one outstanding lookup per server) for slice 7 — trivial
correlation; raising it is a localized change.

**Decision — inline-in-send completion, not a reply DPC.** The biggest design call
(flagged as risk #1). When the server replies (its `sys_channel_send` to the kernel
endpoint), the kernel must drain the reply, cross-context-install the transferred
`MemoryObject`, and complete the lookup PO. Two candidate sites; chosen by checking
where `run_pending()` (the DPC drain) actually runs: **only at the interrupt-
dispatch tail** (`arch/x86_64/idt.rs`), *not* on syscall return. So a reply DPC
enqueued in the server's send syscall would not drain until the next device IRQ /
timer tick — a latency hole. **Inline-in-send** instead: `sys_channel_send` detects
(via `us_forward_reg_for_send`) that the send's peer is a kernel forwarding
endpoint and completes the waiting PO right there — one code path, no deferral. The
`SCHED`-discipline holds: take the pending entry under `SCHED`, then `allocate`
cross-context + `complete_pending_op` + drop the PO ref **outside** `SCHED`.

**Decision — rights = `requested ∩ (rights the server granted on the transfer)`,
not `requested ∩ binding.rights`.** For a Kernel Server / direct handle the bound
object's rights are a sensible cap; for a Userspace Server the bound object is the
*IPC endpoint*, whose `SEND`/`RECV` rights are meaningless as a cap on a returned
`MemoryObject` (AND-ing them would zero out `MAP_READ`). The trusted server
attenuates the transferred handle to the content's rights; the binding's role —
gating *whether* a client may resolve through the mount — is already enforced by the
namespace handle's `LOOKUP` right. `rsproto-namespace-ops.md` updated to match
(source-wins; the spec's earlier wording fit the in-kernel paths).

**Decision — `ImageId::FsServerExt4` + an embedded stub server defer to Part 4.**
The plan had Part 3 add an embedded stub fs-server *process* to prove the loop. But
the loop needs no second process: a **single-process self-forwarding demo in
`parent`** plays both client and server — bind an endpoint as a Userspace Server,
look a path up through it, recv the kernel-forwarded `Resolve`, reply transferring a
`MemoryObject::`-equivalent (`b"STUB\n"`), then `sys_wait` + map + verify. This
isolates the highest-risk kernel mechanism behind a stub with *less* machinery, and
defers `ImageId::FsServerExt4 = 3` + the embedded ELF to their first real consumer
(Part 4). `parent` gains a `librsproto` dependency (it is not `init`, so this is
allowed) — which also cross-checks the kernel's hand-coded rsproto mirror against
the library codec.

**Refinement — `OpStatus::Pending` is used, not just reserved.** The forwarding arm
of `sys_ns_lookup` models a left-pending lookup as `OpStatus::Pending`; the lookup
result became `Option<(status, result)>` (`None` ⇒ leave the PO pending). Kernel
Servers still never produce `Pending` (debug-asserted).

**Races handled.** Dead server mid-lookup: `ipc_endpoint_closing` now fails any
pending forwarded lookup `PeerClosed` (checking both the closing endpoint and its
peer for a registration), dropping the PO ref outside `SCHED`. Dead client: the
cross-context `allocate` fails cleanly (no panic — the global handle table just
stores `owner_pid`); the reply path drops the object + completes the PO with the
error. Duplicate / stale reply: `take_pending_matching` correlates by `request_id`;
an unmatched reply is dropped (transfer released, sender handles still closed).
One-shot `complete_pending_op` covers any double-completion.

**Kernel rsproto mirror.** `kernel/src/rsproto.rs` is a tiny hand-coded mirror of
the `librsproto` wire codec (the kernel may not depend on a userspace crate): build
the `Resolve` request (request_id stamped under `SCHED` once assigned), parse the
reply. Host tests pin the documented offsets from the kernel side; `librsproto`'s
own tests pin them from the other — the two cannot drift.

**ABI.** No layout changes. New enum *variants* (`BindingTarget`/`ResolvedTarget`
`::UserspaceServer`, `OpStatus::Pending`) are kernel-internal. `KObjectType` value
13 (`UserspaceServerReg`) was already reserved → no hash impact. The rsproto Resolve
wire format is a pre-stabilization kernel↔server ABI (`rsproto-namespace-ops.md`).

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**500** (rsproto 7, userspace_server 4, + namespace/ipc additions), librsproto 11,
fs-server-ext4 6, libkern 8, init 18. **QEMU milestone:** `parent` logs
`forwarded lookup returned 'STUB' via fs-server ok`, and the rest of the boot chain
(IPC children, reaping) stays clean — `init: reaped pid=2 code=0`, no `#DF`/panic.
Branch `phase-2/slice7-fwd`. Next: Part 4 (the real `fs-server-ext4` process).

## 2026-06-25 — Phase 2 slice 7, Part 4: the `fs-server-ext4` server process

The first **userspace resource server**: the `fs-server-ext4` `[[bin]]` — the
process the kernel forwards lookups to (Part 3). It wires the librsproto codec
(Part 1) + the ext4 reader (Part 2) + a `BlockReader` over `sys_io_submit`.

**Decision — alloc-free.** The CLAUDE.md left the server free to use `alloc` (init's
static-arena `#[global_allocator]`). It turns out it needs none: librsproto and the
ext4 reader are both no-alloc, and the serve loop's working set is fixed (a 4 KiB
recv buffer, a 4 KiB reply buffer, a 64 KiB content buffer, a one-page device
scratch). So the server is `#![no_std]` + `#![no_main]` with `.bss` statics and **no
global allocator** — simpler, and it sidesteps sizing an arena against the 64 KiB
read-model cap. (If a future feature needs `alloc`, init's `BumpAlloc` is the
pattern to copy.)

**Decision — host-test the request→reply seam, not the syscall loop.** `libkern`
has no syscall mock (the CLAUDE.md "test mode" is still aspirational), so the syscall
plumbing can only be integration-tested (Part 6, under QEMU). To keep the bulk of the
logic verifiable now, the parse→read→reply core is a `serve_resolve` function in the
**library** (generic over `BlockReader`, exactly like the parser), with the syscall
plumbing confined to `main.rs`. Five host tests run it against the same `mke2fs`
fixture Part 2 uses (extracted into a shared `test_support` module): success
(MEMOBJ reply + content), missing path → NotFound, a directory → NotFound, a
non-Resolve op → Unsupported, and a garbage request → InvalidArgument with a
recovered `request_id` of 0. `serve_resolve` echoes the request's `request_id` so
the kernel correlates the reply; `FsError` maps to a `KError` (`Io`/`Corrupt` →
`IoError`, the rest pass through).

**Decision — the bootstrap handle dance.** A spawned child learns only `installed[0]`'s
handle *value* by register (`rdx`), so init installs exactly **one** handle at spawn —
the **control channel** — and ships the **block-device** handle in a follow-up *setup
message* over it (transferring `handles[0]`). The server then creates the **forwarding
channel** pair itself, keeps the serving end, and returns the **kernel end** to init by
**transferring it in the `Meta::Ready` message** — so init never has to mint the
endpoint, and binds exactly what the server handed back. The `BlockReader` reads a
sector at a time into a one-page scratch `MemoryObject` (`sys_io_submit` READ + wait);
each served file is materialised into a fresh `MemoryObject` (create → map R/W → copy →
`sys_memory_unmap` → `sys_handle_restrict` to `MAP_READ | TRANSFER`), so no mapping
leaks across requests and the transferred handle is read-only content.

**Decision — `ImageId::FsServerExt4 = 3` lands here (deferred from Part 3).** The Part-3
proof was the inline `parent` demo, so the embedded server ELF had no consumer until
now. Added: the kernel `ImageId` variant + `from_u32`, the libkern `IMAGE_FS_SERVER_EXT4`
mirror, the `embedded_images.rs` `include_bytes!` + dispatch arm, and the xtask build
step (the bin builds before the kernel embeds it). A new enum *value* on an existing
`#[repr(u32)]` — no layout change, but the discriminant set is an ABI-hash input
(`docs/spec/abi-version-hash.md`); pre-stabilization, so no version bump.

**Not yet runnable end to end.** The server has no disk to read (Part 5) and nothing
spawns/binds it (Part 6) — so there is no QEMU proof in this part; the embedded ELF
rides along unused until Part 6. Verified: `cargo xtask build` (no warnings; the bin
builds bare `x86_64-unknown-none` and the kernel embeds it) / `check-arch` / `test` —
kernel **500**, **fs-server-ext4 11** (6 reader + 5 serve), librsproto 11, libkern 8,
init 18. Branch `phase-2/slice7-fs-server`. Next: Part 5 (the ext4 test disk).

## 2026-06-25 — Phase 2 slice 7, Part 5: the ext4 test disk

The boot disk gains a second GPT partition — `nitrox-root`, ext4 — so the
fs-server has something real to read (Part 6 mounts it). Build-tooling only
(`tools/xtask`); no kernel/userspace change.

**Decision — splice separate partition-sized images, don't format in place.** The
old single-partition image `mformat`s the FAT at `out@@1M` — mtools treats
everything from the offset to EOF as the device, so the FAT spans the whole disk.
With a second partition that is wrong: the FAT's total-sectors would exceed its
GPT partition and overlap `nitrox-root`. Rather than fight mtools to bound the FAT
in place, both partitions are now built as **separate files sized exactly to the
partition** (`mformat` on a 48 MiB file → FAT bounded to 48 MiB; `mke2fs` on a
file of partition-2's size) and **spliced** into the GPT disk at the byte offsets
**queried from `sgdisk -i`** (robust to GPT's end-of-disk reserve, vs. hard-coding
LBAs). The splice is a plain seek-and-write (no `dd` dependency).

**Decision — sizes.** Disk 128 MiB; ESP 48 MiB (comfortably above the FAT32
cluster-count floor, so `mformat -F` is always valid — a 32 MiB ESP risks
degrading to FAT16); `nitrox-root` fills the rest (~79 MiB). The ext4 is built with
`mke2fs -d` (populate-at-creation, no root/mount) with the **same feature flags the
reader supports** (`^has_journal,^64bit,^metadata_csum,^resize_inode`, 4 KiB
blocks — extent-based, matching the Part-2 fixture), staging
`/system/current-generation` (the milestone file).

**Confirmed — no separate QEMU drive needed (risk #3 resolved).** The slice-6 GPT
driver enumerates *every* non-empty entry (it filters only the all-zero type GUID,
not by type), decodes the UTF-16 partition name to an ASCII
`/dev/disk/by-partlabel/<label>`, and binds it at boot. So `nitrox-root` rides the
existing boot disk. Verified in QEMU: `gpt: 2 partition(s)` (ESP + nitrox-root both
become block nodes), the 128 MiB disk comes up on AHCI, the smaller 48 MiB ESP
still FAT32-boots through Limine/OVMF, and `/dev/disk/by-partlabel/NITROX_ESP`
resolves in `parent`'s block demo (the same bind path serves `nitrox-root`). The
ext4 image checks out under `debugfs`/`dumpe2fs`: `/system/current-generation`
present, extent-based, no journal/64-bit/metadata_csum — exactly the reader's set.
Boot stays clean (children reap, `init: reaped pid=2 code=0`, no `#DF`/panic).

Branch `phase-2/slice7-ext4-disk`. Next: Part 6 (init mount loop + the milestone) —
the slice's end-to-end payoff, where init spawns the fs-server against this disk,
binds it, and logs `/system/current-generation`.

## 2026-06-25 — Phase 2 slice 7, Part 6: init mount loop + the milestone (slice complete)

The payoff: init mounts the ext4 root and reads a file through it, exercising the
whole stack — **ext4 on disk → fs-server `sys_io_submit` → librsproto reply →
kernel cross-context handle install → init maps + logs**. This completes slice 7
(the first userspace resource server, reached transparently through the namespace).

**What init does** (`mount_one`, the Resource Server Startup Protocol from init's
side): `manifest::device_ns_path` maps `gpt-partlabel:nitrox-root` →
`/dev/disk/by-partlabel/nitrox-root`; per `MountSpec` (topo order) init resolves the
device handle (`READ | TRANSFER`), `sys_channel_create`s a control channel, spawns
`fs-server-ext4` (moving the control endpoint in via spawn → the child's `rdx`),
sends a **setup message** transferring the device handle, awaits **Ready** (bounded
30 s; hand-parsed `"RSMG"` magic + `Ready` op — init never speaks `librsproto`,
per its CLAUDE.md), and `sys_ns_bind`s the forwarding endpoint at the mount point.
The kernel sees an `IpcChannel` and adopts it as a Userspace Server (Part 3). Then
the milestone: `ns_lookup_wait("/system/current-generation", MAP_READ)` — the kernel
forwards the Resolve, the fs-server reads the file and replies a `MemoryObject`, the
kernel installs it into init, init maps + logs it.

**Decision — bind the root fs-server at `/`.** init's root namespace already holds
the boot kernel-server bindings (`/dev`, `/initramfs`, `/proc/self`). Binding the
fs-server at `/` is safe: namespace resolution is longest-prefix, so
`/initramfs/...` still hits the (more specific) kernel server while
`/system/current-generation` falls through to `/` → the fs-server. No exact-path
collision (nothing is bound at `/` yet). The async forwarding makes the
cross-process handoff work despite init being single-threaded: init blocks on the
lookup PO while the scheduler runs the fs-server (which serves the request, then its
own `sys_io_submit` blocks on the AHCI IRQ), and the fs-server's reply completes
init's PO — no circular wait.

**Bug found + fixed — the fs-server linked as a PIE.** The Part-4 spawn failed with
`KernelError` (`load_elf` rejects `ET_DYN`). The `fs-server-ext4` crate was missing
the `.cargo/config.toml` (`relocation-model=static`, `-no-pie`, `-static`) +
`build.rs` + `user.ld` that the other userspace bins use to force a static
**ET_EXEC** at a fixed low address. Because fs-server is a **lib + bin** crate (host
lib tests), it needs init's variant: `build.rs` emits `rustc-link-arg-bins` (not
`rustc-link-arg`), so the fixed-address linker script reaches the `[[bin]]` but
**not** the host lib-test link (which would otherwise be corrupted). Lesson for the
next bare-target bin: copy the linker-config trio up front.

**Decision — mode is informational in slice 7.** The init.toml mount is `rw`, but
the fs-server is read-only and the lookup requests `MAP_READ`, so the returned
content is read-only regardless; `mode`-derived bind rights are a Phase-3 (RW)
concern. Likewise the eshell handoff on a failed mount is slice 9 — for now a failed
mount logs and is skipped.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**500**, **init 19** (+`device_ns_path`), fs-server-ext4 11, librsproto 11, libkern 8.
**QEMU milestone:** `gpt: 2 partition(s)` → `fs-server: ready (ext4, read-only)` →
`init: mounted fs-server-ext4 at /` →
`init: /system/current-generation = nitrox-rootfs generation 1`, and boot stays
clean afterward (`parent` demos + children reap, `init: reaped pid=3 code=0`, no
`#DF`/panic). Branch `phase-2/slice7-mount-milestone`.

**Slice 7 is complete.** Phase 2's namespace + resource-server story is real
end to end: a userspace process reads a file from an on-disk ext4 filesystem,
served by another userspace process, reached transparently through the namespace
with the kernel forwarding the lookup and installing the result cross-context — no
filesystem code in the kernel.

## 2026-06-25 — Phase 2 slice 8 design: page-cache fill model (Model B now, Model A → Phase 3)

Docs-only. Scoping slice 8 (the page cache). The v5.1 design specifies the
**extent** data path (the fs-server returns LBA extents; the kernel reads blocks into
page-cache pages; the fs-server never touches file data — call it **Model A**).
Slice 7's interim model has the fs-server read the whole file (≤ 64 KiB) and return
a populated `MemoryObject`.

**Key realization — the fork is only the page-fill leaf.** The page cache, the lazy
`FileBacked` VMA, the lazy `sys_memory_map`, and the async fault path are *identical*
no matter how a missing page is filled — both candidates end at "fill page-cache page
for file F, offset X." So slice 8 builds those behind a **fill-producer seam**, and
the A-vs-B choice is just which fill to implement (and is swappable later).

**The two fills.**
- **Model A (extent):** the fs-server returns LBA extents; the kernel reads blocks
  **zero-copy** straight into cache pages (DMA target = cache page = client page);
  faults stay in-kernel (no per-fault userspace round-trip); the block layer sees
  disk layout (merge / readahead). Block-filesystems only — it assumes "file =
  sequence of device LBAs."
- **Model B (range-read):** the fs-server reads the bytes itself (its slice-7
  `sys_io_submit` path) and returns them; the kernel copies into a cache page. Costs
  +1 copy and an IPC round-trip per miss, but works for **any** fs-server (network /
  synthetic / transforming filesystems have no LBA map) and is a tiny delta over the
  slice-7 fs-server.

Note both models trust the fs-server's **device handle** equally — a compromised
fs-server can read any block on its device either way (mis-return bytes in B, or
mis-direct the kernel to wrong LBAs in A). v5.1's "fs-server never holds mappable
memory" is about keeping it out of the *memory-manager* role, not data secrecy.

**Decision — Model B for slice 8; Model A deferred to Phase 3.** Model A is the
correct long-term **primary** for block filesystems (zero-copy, in-kernel faults,
I/O-scheduling visibility, and — decisively — writeback symmetry: Phase-3 RW *forces*
the extent machinery, since the kernel must know a dirty page's LBAs to flush it). It
is **not** dropped. But it is not exclusive: Model B is the necessary general fallback
for non-block fs-servers, and a mature OS keeps both (cf. Linux `iomap`/direct-I/O vs
`readpage`/`readahead`). For slice 8, Model B de-risks the genuinely-hard part (the
async fault path) with a trivial fill, reuses the fs-server we just built, and ships
the slice milestone (lazy reads + the 64 KiB cap lift).

**Placement of Model A — Phase 3, with `fs-server-ext4` read-write.** Writeback
already needs the extent machinery (kernel-knows-LBAs) and the write path already
needs extent updates, so the extent infrastructure gets built there for writes
regardless; converting reads to it (Model A) is then a small, symmetric addition.
And no Phase-2 milestone consumes Model A's performance, so building it in Phase 2
would be a producer-less optimization — against the project's discipline (the
`FileBacked` variant itself was added stubbed for exactly this reason). The slice-8
page cache must keep the fill-producer seam clean so Model A slots in without a
redesign. Implementation-plan slice 8 + Phase-3 § "fs-server-ext4 read-write" updated;
no code yet.

## 2026-06-25 — Phase 2 slice 8 scope: FileObject, stateless ReadRange, the async fault path

Builds on the fill-model entry above. The slice-8 build decisions, settled in scoping:

- **A new `FileObject` kobject** (next free type, **14**) for a mapped file —
  deliberately distinct from `MemoryObject`. The line is *who owns the frames and when
  they appear*: `MemoryObject` is anonymous/shared RAM the kernel commits **eagerly**
  (buffers, shared memory, IPC-transfer/DMA targets — used broadly); `FileObject` is a
  file's content **paged in on demand** from a producer, frames owned by the page
  cache. Both map, but `sys_memory_map` branches: `MemoryObject` → an `Object` VMA
  (frames present), `FileObject` → a `FileBacked` VMA (lazy). `FileObject` holds
  `(fs-server endpoint, path suffix, size)` + a sparse per-page cache. Resolve returns
  a `FileObject` **universally** (every file map is lazy, like Linux), retiring the
  eager `MemoryObject` fill and the 64 KiB cap.
- **Stateless fill protocol — `File::ReadRange(suffix, offset, len) → bytes`** (a new
  `File` rsproto category). The `FileObject` re-sends the suffix per fault; the
  fs-server re-resolves suffix→inode (cacheable) and reads the range — **no open-file
  table, no close op, no lifecycle**. A stateful `file_id` handle is a later
  optimization (tracked in `deferred-decisions.md`).
- **`File::ReadRange` is not throwaway when Model A lands.** Model A (Phase 3) *adds*
  `File::MapExtents` (return LBA extents; kernel reads blocks zero-copy) as the
  block-fs fast path **alongside** `ReadRange`, which **survives as the general
  fallback** for non-block / network / transforming fs-servers (no extents to hand
  back). The `FileObject`, page cache, lazy VMA, and async fault path are all reused —
  Model A swaps only the fill leaf — so slice-8 Model-B work is the general data path,
  not a detour.
- **The async fault path** (the hard part): `fault_in` (under the rank-4 AS lock)
  returns `NeedFill` on a `FileBacked` miss **without blocking**; `pf_dispatch` (a
  blockable user-thread context — a ring-3 fault holds no kernel locks) **drops the AS
  lock**, submits the fill, **blocks the faulting thread** on the fill PO exactly as
  `sys_wait` does (SCHED is rank-1, so it must be acquired *outside* the rank-4 AS
  lock — blocking while holding it would invert the order), and **re-validates**
  `fault_in` on wake (the VMA may have been torn down on process exit) before the
  instruction retries. Not host-testable (needs a live thread + scheduler + a real
  fault); verified by a boot self-test with a stub *async* producer (timer-DPC fill) so
  the thread genuinely parks + resumes.
- **Per-file cache, no eviction** in slice 8 (grows to the mapped extent; freed on
  unmap / `FileObject` drop). Global inode-keyed sharing + the clock-reclaim daemon are
  deferred (`deferred-decisions.md`).

Built as five Parts (implementation plan). Detailed contracts
(`rsproto-file-ops.md`, the `FileObject` handle-encoding entry, the
memory-management page-cache section) are written in their respective Parts, as in
slice 7. No code yet.

## 2026-06-25 — Phase 2 slice 8, Part 1: the FileObject kobject + its page cache

The mechanical foundation: a new **`FileObject`** kernel object (type **14**) owning a
sparse, per-page **page cache**. No fault path, no producer wiring — just the object
and its cache data structure + lifecycle, fully host-tested.

- **`FileObject` (`kernel/src/object/file_object.rs`)** holds `size` (exact file
  bytes) + a `SpinLock<Inner>` over a `KVec<CachePage>` — each `CachePage` is
  `{ index, frame, state: Loading | Ready }` and **owns** its frame (freed in `Drop`,
  like `MemoryObject`). The cache is its own rank-4 object lock (not the `SCHED` cell
  pattern) because it is shared across every mapping of the object, in potentially
  several address spaces; the fault path (Parts 2–3) takes it *after* dropping the AS
  lock (both rank 4, never nested) and never blocks under it.
- **API the fault path will use, built + tested now:** `reserve(index)` →
  `Ready(frame)` (hit) / `Loading(frame)` (a fill is in flight) / `New(frame)` (a
  fresh **zeroed** Loading frame the caller fills — zeroing guarantees a partial tail
  page's padding is zero) / `Oom`; `mark_ready(index)` (Loading→Ready after the fill);
  `lookup(index)`. Linear-scan lookup (O(n) in resident pages) — fine for slice-8 file
  sizes; a sorted/tree index is a later optimization.
- **Deviation from the scope's field list (deliberate):** the `FileObject` does *not*
  yet hold the producer reference (fs-server endpoint + path suffix) the scope names —
  those are added in **Part 3** (the fill), their first consumer, so Part 1 ships no
  unused fields. `try_new(size)` grows to take the producer there.
- **Wiring:** `KObjectType::FileObject = 14` (kernel + libkern mirror + `from_u32` +
  `KOBJ_FILE_OBJECT`); `type_rights` allows the `MAP_*` band (mapped like a
  `MemoryObject`; `MAP_WRITE` in the *type* mask for the Phase-3 RW path, attenuated
  to read-only at resolve); `dispatch_destroy` + `test_probe` arms; `object/mod`
  export; `handle-encoding.md` rights table + the architecture overview's fixed
  object list.

**ABI.** A **new `KObjectType` discriminant (14)** — an ABI-version-hash input
(`docs/spec/abi-version-hash.md`); pre-stabilization, the hash is not yet computed in
code, so nothing is enforced, but record the impact. No layout change.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**506** (+6 `file_object` tests: size/round-up, reserve→mark_ready lifecycle, distinct
frames, no-op mark, **drop frees frames no-leak**, dispatch_destroy arm). Branch
`phase-2/slice8-file-object`. Next: Part 2 (the lazy `FileBacked` mmap + the async
fault path — the hard part).

## 2026-06-25 — Phase 2 slice 8, Part 2a: the FileBacked VMA + fault wiring

**Decision — split Part 2.** Investigating the async fault path confirmed it is sound
but intricate, so Part 2 is split: **2a** (this entry) is the mechanical, fully
host-testable half — the lazy mapping + the fault *signal* + installing a resident
page — and **2b** is the scary async half (block the faulting thread on the fill,
proven with a stub). Splitting isolates 2b for a focused review.

- **`sys_memory_map` now branches on object type.** It looks the handle up
  generically (a non-mappable type lacks the `MAP_*` right and fails the lookup),
  then: a `MemoryObject` maps eagerly (`map_object`); a `FileObject` maps **lazily**
  (`map_file` — a `MappingKind::FileBacked` VMA holding the object, **no PTEs**).
- **`fault_in` does not touch the file cache.** Its FileBacked arm returns the unit
  signal `FaultIn::FileBacked` (renamed from the `NoPageCache` stub). The decisive
  reason: the `FileObject`'s cache lock is **rank 4, the same as the AS lock**, and the
  two must never nest. So `fault_in` only signals; the caller fetches the backing and
  fills **outside** the AS lock. `FaultIn` stays an all-unit, `Eq` enum (the fill data
  is fetched separately), so the existing fault tests are unchanged.
- **Two helper entry points, each a fresh AS-lock acquisition** (never nested with the
  cache lock): `file_backing(addr) → (FileObject ref, page index)` (the index is
  `(page_base − vma.start)/PAGE`; the mapping covers the file from offset 0), and
  `map_file_page(addr, &object, frame) → bool` — install the leaf PTE for a now-
  resident cache frame, **re-validating** under the lock that a FileBacked VMA for the
  *same* object still covers `addr` (the fault may have raced an unmap while it blocked
  in 2b). A racing `AlreadyMapped` counts as success (the page is present). The frame
  is owned by the `FileObject`, so `unmap` removes the PTE without freeing it (the
  existing `free_vma_pages` FileBacked arm already does this).

**Reachability.** Nothing constructs a FileBacked VMA in QEMU yet (a `FileObject` only
comes from a lazy resolve — Part 4), and a file fault is still fatal until 2b wires the
fill — so 2a is host-tested-but-unreachable infrastructure, exactly like the
demand-paging prereq that added the stubbed `FileBacked` variant. No ABI change.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**511** (+5 `addr_space` file-backed tests: lazy-no-PTEs, fault signal + `file_backing`
index, `map_file_page` installs/idempotent, rejects wrong-object/unmapped, unmap keeps
the object's frames). Branch `phase-2/slice8-fault-path`. Next: Part 2b (the async fill
+ block-on-fault + the stub proof).

## 2026-06-25 — Phase 2 slice 8, Part 2b: the async fault path (block-on-fault, proven)

The hard part: a file page fault **blocks the faulting thread** on an asynchronous
fill and resumes it when the page arrives — demand paging, the async-first way.
Proven in QEMU with a real user fault.

**The flow.** `try_fault_in` (the `#PF` handler) on `FaultIn::FileBacked` →
`AddressSpace::file_backing(addr)` (the object + page index, a fresh AS-lock
acquisition) → `FileObject::fault_in_page(&obj, index)` → `AddressSpace::map_file_page`
on wake. `fault_in_page` loops over `reserve`: a hit returns the frame; a miss
(`New`) creates a fill `PendingOperation`, calls `start_fill` (the producer), and
**parks the thread** on the PO via the scheduler's `wait_on` primitive; on wake it
loops → the page is now `Ready` → returns the frame.

**Decision — the `#PF` handler may block (confirmed sound).** The concern was
blocking inside an interrupt-gate (`IF=0`) fault handler. It is fine: a ring-3 fault
holds **no kernel locks** (the thread was in user mode), `wait_on` →
`block_current_and_switch` **switches to another thread** (which runs with its own
`IF`), and the timer keeps ticking → the fill DPC drains at the interrupt-dispatch
tail → `complete_pending_op` wakes the faulting thread. The faulting thread's kernel
stack (the `pf_dispatch` frame) is preserved across the switch exactly as a syscall's
is across `sys_wait`; on resume it returns up to `pf_dispatch`, and the stub `iretq`s
into the user instruction (which retries with the page now present). No new
`pf_dispatch` return path is needed — `try_fault_in` just takes longer. Lock
discipline: the fill touches only the FileObject cache lock (rank 4), released before
`wait_on` takes `SCHED` (rank 1) — never nested, and the AS lock is already released
(`file_backing` returned) before the block.

**Decision — the producer lives on the `FileObject` (`Producer::Stub`).** This is the
field Part 1 deferred; Part 2b is its first consumer. `Stub { base }` fills page `i`
with the byte `base + i` **asynchronously** — `start_fill` heap-boxes a `StubFillBox`
(owning clones of the object + PO so they outlive the deferred fill), arms its `Dpc`
at the box, and enqueues it; the DPC writes the frame, `mark_ready`s the page,
`complete_pending_op`s the PO, and frees the box. Part 3 adds `Producer::FsServer`
(the real IPC range-read). The timer-tick latency of the stub (~10 ms/fault) is a
self-test artifact, not the real path.

**Decision — concurrent same-page faults yield (deferred proper wait).** On a single
CPU with the milestone (one faulter per object), a `Reserve::Loading` (another fault
filling the same page) cannot arise; it is handled by `yield_now` + retry so the
in-flight fill's DPC can run. A proper "block on the in-flight fill's PO" needs a
per-page fill PO in the cache and is deferred (`deferred-decisions.md`). An OOM while
*starting* a fill rolls the reserved page back (`cancel_reserve`) so a retry is clean.

**The proof.** A boot fixture binds a stub `FileObject` (3 pages, `base 0xA0`) at
`/dev/test/pagecache` in pid-1's namespace; `parent` resolves it, maps it read-only
(a lazy `FileBacked` VMA), and reads one byte from each page — three **real user
faults** that park + resume. QEMU: `parent: page-cache demand-faulted 3 pages ok
(0xA0,0xA1,0xA2)`, boot clean afterward (children reap, `init: reaped pid=3 code=0`,
no `#DF`/panic). The `FileObject::try_new` signature gained the `producer` argument
(Part-1/2a test call sites updated).

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**511** (the new fault path is QEMU-proven, not host-testable — it needs a live
thread + scheduler + a real fault). Branch `phase-2/slice8-fault-fill`. Next: Part 3
(the `File::ReadRange` op — the real Model-B fill producer).

## 2026-06-26 — Phase 2 slice 8, Part 3: the `File::ReadRange` wire op + a Part 3/4 re-split

The Model-B fill protocol's **wire contract** — the bytes a demand fault asks the
fs-server for. Codec only this part; the kernel send-side moves to Part 4 (below).

**Decision — a new `File` category at `0x06`, not `Stream`/`Block`.** The
wire-format category table (`rsproto-wire-format.md`) already partitions `Stream`
(`0x02`, cursor-based read/write/seek) and `Block` (`0x03`, extent/block-level,
fs-specific). A page-cache fill is a **positioned, stateless byte read** — it fits
neither cleanly: `Stream` implies a cursor we don't have, and `Block` is where
**Model A**'s extent query will live (Phase 3). So `File::ReadRange` takes the first
"Future categories" slot, `0x06`, grouping file-content access (`ReadRange` now;
`stat`/`readdir` later) without contaminating `Stream`'s cursor semantics. Recorded
in `docs/spec/rsproto-file-ops.md`; the category table's reserved range shrinks to
`0x07–0xFE`.

**The contract.** `File::ReadRange(offset, len, suffix) → bytes`: a 16-byte request
prefix (offset `u64`, len `u32`, suffix_len `u16`, pad) + the path suffix; an 8-byte
success reply (`content_len u32`, pad) with the filled bytes riding in `handles[0]`
as a ≤1-page read-only `MemoryObject`. `content_len < len` is a short EOF tail (the
page-cache frame starts zeroed, so the tail needs no padding on the wire). The fill
is **stateless** — each `ReadRange` re-identifies its file by the same `suffix` the
lazy `Resolve` used (no server-side open-file cookie; a possible Phase-3
optimization). Paired `Namespace` additions: the `RESOLVE_FILE_LAZY` flag (`1 << 1`)
and `OBJECT_KIND_FILE` (`4`) reply kind — a lazy resolve returns the file **size**
and no handle, and the kernel builds the page-cache object itself. Mirrored in
`librsproto` (`file.rs`) and the kernel (`rsproto.rs`); host tests pin the offsets
on both sides. A `reply_op` router lets the (Part-4) completion path send a reply to
the right parser (`Resolve` vs `ReadRange`) since the two reply bodies differ.

**Decision — re-split Parts 3/4 around the faulter-vs-filler constraint.** The plan
had Part 3 = "the op + wire the fault fill to IPC" and Part 4 = "the fs-server side."
But a page fault **blocks the faulting thread**, so the thread that faults cannot
also serve its own fill — the *filler* must be a **separate process**, and the only
real one is the fs-server (Part 4). Proving the kernel send-side in isolation would
need throwaway two-process scaffolding. So Part 3 is now the **wire contract only**
(this entry), and Part 4 absorbs the kernel send-side (`Producer::FsServer` +
`start_fill` over the slice-7 forwarding endpoint + the reply→frame copy + the
`RESOLVE_FILE_LAZY`→build-`FileObject` path) **and** the fs-server side, proven by the
existing slice-7 `/system/current-generation` lookup **going lazy** (init faults, the
real fs-server fills — two processes, no scaffolding). Part 5 stays the large-file
milestone.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**514** (+3 ReadRange codec tests), librsproto `file::` round-trips green. No QEMU
(pure codec). Branch `phase-2/slice8-readrange`. Next: Part 4 (the kernel send-side +
the fs-server, end to end).

## 2026-06-26 — Phase 2 slice 8, Part 4a: the kernel send-side + lazy-resolve plumbing (dormant)

The kernel half of the real Model-B fill: build the page-cache object on a lazy
resolve and fill its pages over IPC. Split out from the fs-server half (4b) so each
is independently reviewable.

**Decision — split Part 4 into 4a (kernel) + 4b (fs-server) around faulter-vs-filler.**
A page fault blocks the faulting thread, so only the fs-server can serve a fill —
the kernel send-side can't be QEMU-proven without it. Rather than build throwaway
two-process scaffolding, 4a lands the kernel machinery in a **dormant, runtime-safe**
state and 4b activates + proves it. 4a flips the kernel to request `RESOLVE_FILE_LAZY`,
but the **unchanged fs-server ignores the flag and still replies `MEMOBJ`**, so boot
stays on the eager slice-7 path (the kernel handles both reply kinds permanently).
QEMU regression confirms: the eager milestone and the Part-2b stub demo both still
work, no faults.

**The machinery.** (1) `FileObject` gains `Producer::FsServer { reg, suffix }` (the
field Part 1 reserved) — it pins the server's `UserspaceServerReg` and names the file;
`start_fill` builds a `File::ReadRange` for the faulted page's byte range and
originates it over the forwarding endpoint, leaving the fault parked (the reply
completes it). (2) `UserspaceServerReg` gains a second N=1 slot, `pending_fill`
(`PendingFill { po, file_obj, frame, index }`), independent of the lookup slot and
correlated by its own `request_id`; `PendingLookup` gains the lookup's **suffix**
stored **inline** (`[u8; 256]`, not a `KString` — `begin` runs under `SCHED`, where
heap alloc/free is a lock-order violation; a memcpy is not), so a `FILE` reply can
name the file. (3) `sched::us_forward_originate_fill` / `us_forward_take_pending_fill`
mirror the lookup originate/take. (4) The reply-completion path now **routes by op**
(`rsproto::reply_op`): a `Resolve` reply runs `complete_resolve_reply` — and on
`OBJECT_KIND_FILE` *builds a `FileObject`* (no transferred handle; `content_len` is
the file size; producer points back at the reg + suffix) and installs it instead of a
`MemoryObject`; a `ReadRange` reply runs `complete_read_range_reply` — copies the
transferred ≤1-page `MemoryObject` into the cache frame (zeroed tail stays padding),
marks the page ready, completes the fill PO.

**Decision — the fill is stateless; the suffix lives kernel-side.** The fill re-sends
the path `suffix` on every `ReadRange` (stored on the `FsServer` producer at build
time, copied from the lookup's inline suffix), so no server-side open-file cookie is
needed (deferred — see `deferred-decisions.md`). A suffix longer than the 256-byte
inline buffer is recorded with its true length but a truncated buffer: an eager
`MEMOBJ` reply (suffix unused) is unaffected, but a `FILE` reply for such a path fails
`TooLarge` — no milestone path is near the cap; a heap-backed suffix is a later
concern.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**516** (+2: `begin_records_the_lookup_suffix`, `fill_slot_is_independent_and_correlates_by_id`).
QEMU regression clean (eager milestone + stub demo both work, no `#DF`/panic). Branch
`phase-2/slice8-fill-integration`. Next: Part 4b (the fs-server serves
`RESOLVE_FILE_LAZY` + `File::ReadRange`; the lazy path activates and is proven by the
milestone going lazy — retiring the Part-2b stub fixture).

## 2026-06-26 — Phase 2 slice 8, Part 4b: the fs-server serves the lazy path (Model B proven end to end)

The fs-server half that activates 4a's dormant kernel machinery: serve the lazy
resolve and the page-cache fill. The whole Model-B path now runs end to end.

**The fs-server side.** (1) The ext4 reader gains two functions beside the eager
`read_file`: `stat_file` (resolve a regular file → its size, **no content read, no
`MAX_FILE` cap** — lazy faulting is what lifts the 64 KiB cap) and `read_file_range`
(read `[offset, offset+len)` into a buffer, clamped to the file size, walking extents
per block — the fill primitive). Both share a new `resolve_regular_file` helper
factored out of `read_file`. (2) `serve` now **dispatches by op**: a
`Namespace::Resolve` with `RESOLVE_FILE_LAZY` replies `OBJECT_KIND_FILE` + the file
size and **no handle** (`serve_resolve`'s lazy branch; the eager `AS_MEMOBJ` path
stays for a non-lazy client); a `File::ReadRange` reads the range and replies a
`MemoryObject` of the bytes (`serve_read_range`). (3) `main.rs`'s serve loop calls
`serve` and stages the new `Served::Lazy` (a reply with no transferred handle).

**Decision — an error reply carries its request's op.** `encode_error` /
`error_reply` gained an `op` parameter: a `ReadRange` failure must reply with the
`ReadRange` op so the kernel's `reply_op` router sends it to the pending **fill**
(not a lookup). With the slice-7 hard-coded `OP_NS_RESOLVE`, a failed fill would
route to `complete_resolve_reply`, find no matching lookup, drop silently — and the
**faulting thread would hang forever**. A host test pins this
(`read_range_error_carries_the_read_range_op`).

**Proven end to end (QEMU).** The kernel sends `RESOLVE_FILE_LAZY`; the fs-server
replies `OBJECT_KIND_FILE` + size; the kernel builds the `FileObject`; init maps it
and **faults**; the kernel sends `File::ReadRange`; the fs-server range-reads ext4 and
replies the bytes; the kernel lands them in the cache frame and resumes init — which
logs `init: /system/current-generation = nitrox-rootfs generation 1`, now entirely via
the lazy fill (init faults, the real fs-server fills — two processes, no scaffolding).
Boot clean (children reap, no `#DF`/panic). The **Part-2b stub fixture** (the kernel's
`/dev/test/pagecache` `FileObject` + `parent`'s `pagecache_demo`) is **removed** — the
real milestone supersedes it; `Producer::Stub` stays for host tests.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**516**, fs-server-ext4 **18** (+7: `stat_file` / `read_file_range` (3) + the lazy /
range / EOF / error-op serve tests (4)). QEMU: the milestone runs via the lazy fill,
no stub demo, clean. Branch `phase-2/slice8-fill-integration`. **Slice 8's core
(Model-B page cache, end to end) is complete**; Part 5 (the large multi-extent file +
the > 64 KiB milestone) remains.

## 2026-06-26 — Phase 2 slice 8, Part 5: the large-file milestone (slice 8 complete)

The capstone: a file **well past the old 64 KiB eager cap**, read entirely through the
page cache across many demand faults. Closes slice 8.

**The fixture + proof.** xtask stages `system/large.bin` in the ext4 image — **256 KiB
(64 pages)**, with **position-sensitive** content (`byte[i] = ((i >> 12) ^ i) as u8`,
folding the page index into each byte) so a mis-faulted or mis-ordered page is caught,
not just a wrong byte value. init maps the whole file lazily and reads **every** byte
(`read_large_file`), verifying each against the same `fill_byte`; the first touch of
each page is a demand fault the kernel serves by a `File::ReadRange` to the fs-server.
QEMU: `init: large.bin verified 262144 bytes across 64 demand-faulted pages ok`, boot
clean (current-gen still reads, children reap, no `#DF`/panic). The 64 KiB cap is gone
for the lazy path (`stat_file` / `read_file_range` carry no cap; only the unused eager
`read_file` keeps it).

**Decision — a shared fixture constant now; size discovery via `sys_handle_stat`
deferred to eshell `cat`.** init needs the file size to map + verify the right extent.
`sys_handle_stat`'s `HandleInfo` carries no size today (rights/type/generation only),
and the lazy resolve consumes `content_len` to build the `FileObject` — so init does
not learn the size from the lookup. For this milestone init and the xtask generator
share a `LARGE_FILE_BYTES` constant + `fill_byte` (kept in sync by comment, exactly as
the `current-generation` path is hardcoded) — a deliberate **temporary bridge**.

The clean fix is **`sys_handle_stat` + a `HandleInfo.size` field**: the size is already
kernel-local metadata (`FileObject.size`, set from the resolve; `MemoryObject.size`),
so discovery is synchronous and needs no fs-server round-trip. `HandleInfo` is **not in
the ABI version hash** (`abi-version-hash.md`), so growing it 16 → 24 bytes is cheap;
the lazy resolve would additionally grant `INSPECT` (today it grants only `MAP_READ`,
and `stat` requires `INSPECT`). Rejected alternatives: a `Stat` **IoOpcode** (none is
planned — `io-operation.md` reserves only `Flush`/`Trim`/control — and `io_submit`
targets block devices, not `FileObject`s; it would be a pre-signalled PO doing no I/O,
overkill for local metadata) and the rsproto **`File::Stat`** (wrong layer — a
client↔fs-server op, and a needless round-trip for a size the kernel already holds).
Deferred to its first real consumer, **eshell `cat` (slice 9)**, which is the natural
place to wire `HandleInfo.size`. Recorded in `deferred-decisions.md`.

**Scope honesty — multi-page, not provably multi-extent.** The win is **multi-page**
demand faulting (64 distinct logical blocks, each faulted + read via the extent walk
+ an IPC round-trip). `mke2fs -d` lays a 256 KiB file contiguously (a single extent),
so this fixture does not exercise the extent tree's **interior-node** path on disk —
that path (depth > 0 recursion in `extent_find`, block-boundary spans in
`read_file_range`) stays covered by the fs-server host tests. Forcing a multi-extent
on-disk layout needs fragmentation `mke2fs` won't deterministically produce; not worth
contriving for this milestone.

**Verified.** `cargo xtask build` (no warnings) / `image` / `check-arch` / `test` —
kernel **516**, fs-server-ext4 **18** (unchanged; Part 5 is fixture + init demo). QEMU:
the 64-page large file verifies end to end. Branch `phase-2/slice8-large-file`.
**Phase 2 slice 8 (the kernel page cache) is complete** — file-backed mappings are
lazy and demand-paged from a userspace fs-server over IPC, at scale.

## 2026-06-27 — Phase 2 slice 9, Part 1: serial console input (the universal char-device path)

The first **user input**: COM1 receive, reached through the **universal device
interface** (`sys_io_submit` + `sys_wait`), not a console-specific syscall.

**Decision — the console is a char-class `DeviceNode` read via `sys_io_submit`.**
Per the user's direction, input must use the same device interface as block I/O, not
a bespoke `sys_console_read`. So `DeviceClass` gains `Char`, `DeviceNode` gains a
`CharBackend` (the char analogue of `BlockBackend`), and `sys_io_submit` branches on
device class: **Block** → the IRP path (today); **Char** → a stream read (no block
alignment, `offset` ignored, `length` = max bytes) dispatched through the backend.
The read returns a `PendingOperation` completed the same way a block read is — one
waitable, one completion, identical to the model the rest of userspace already
speaks. `sys_io_submit(Read)` on the console is the whole userspace surface. (The
long-term userspace **console/tty server** — cooked line discipline, multiplexing —
layers on this raw char device; deferred. Line editing/echo live in eshell; the
kernel delivers raw bytes.)

**Decision — no internal `InterruptObject`.** The read's PO *is* the wait target, so
the RX DPC completes it directly (mirroring `irp_complete_dpc`) — no separate
latching interrupt object is needed (the earlier design sketch's one is dropped).

**The driver** (`kernel/src/drivers/console.rs`, neutral): a 256-byte RX ring + a
single parked-read slot, behind one `IrqSpinLock`. The ISR (`console_isr`) drains
the UART FIFO into the ring (via `crate::arch::serial::console_rx_*`, the neutral
surface) and queues a DPC; the DPC copies the ring into the parked read's
`MemoryObject` (HHDM — kernel memory, sound from a DPC) and completes its PO.
`submit_read` satisfies immediately from the ring (pre-signalling) or parks
(single-reader → `WouldBlock` if busy). The lock is released before the memobj copy
or `complete_pending_op` (rank-1 `SCHED`) — never nested. The console `DeviceNode` is
leaked `'static`; `/dev/console` (a new `KernelServerId::Console`, `READ` rights)
hands out counted references.

**arch boundary.** RX register access + `MCR` loopback + COM1 IRQ4 routing stay in
`arch/x86_64`; the neutral driver reaches them only through `crate::arch::serial::*`.

*Decision — `install_isa_irq` is arch-internal, not a neutral name.* "ISA" is an
x86-only concept (ARM has no ISA IRQs — a UART there is a DTB device on a GIC SPI), so
exposing `crate::arch::install_isa_irq` would leak arch jargon (the boundary convention
forbids it). Instead, a **fixed legacy device wires its own interrupt inside the arch
layer** — exactly as the PIT does (`resolve_isa_irq`, never neutral). The serial
console arms its RX interrupt through a **console-named** neutral function,
`arch::serial::console_arm_rx(handler) -> vector`, which internally calls the now
`pub(crate)` `ioapic::install_isa_irq(COM1_IRQ=4, …)`. The neutral concept is "arm the
console's RX interrupt" — *more* neutral than a generic installer, since the console
driver need not know its own IRQ number; an aarch64 port maps the same call onto its
UART's GIC interrupt. (`install_pci_irq` is left as-is: PCI is cross-arch, and its
`TODO(msi)` already plans promoting the device-interrupt family into an
`ArchIrqInstall` trait taking a neutral `InterruptSpec` when MSI/teardown land — the
home for a generic spec-driven installer later.) `check-arch` clean.

**Proof.** A boot self-test exercises the RX register path deterministically via the
UART's **internal loopback** (transmit a byte, poll it back) — `console: RX loopback
self-test OK` — then arms the IRQ: `console: RX armed (COM1 IRQ4→vec0x32)`. Runs with
interrupts masked, before RX IRQs are armed, so the polled read (not an ISR) consumes
the test byte. The ISR firing on real input is proven in Part 2 (scripted serial
input). Boot stays clean (AHCI, current-generation, large.bin all still pass, no
`#DF`).

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**516** (`DeviceClass::Char` added to the round-trip test; the driver + io_submit char
path are QEMU-proven, like AHCI, not host-tested). Branch `phase-2/slice9-eshell`.
Next: Part 2 (the eshell crate + line editor + interactive launch).

## 2026-06-27 — Phase 2 slice 9, Part 2: the eshell crate + the first interactive shell

**The first interactive userspace program** — a serial command shell, proving the
Part-1 console-input path end to end with real typed input.

**The crate** (`userspace/eshell`, a new workspace member): bin-only, bare target,
mirroring `userspace/parent`'s build (`.cargo/config.toml` static/non-PIE,
`build.rs` → `user.ld`). `#![no_std]` + `#![no_main]`, **no `alloc`** (fixed `.bss`
buffers — a 128-byte line buffer, a one-page read buffer), **`libkern` only** (no
rsproto/libos/librt) — the init family's rules. `ImageId::Eshell = 4` (+ `from_u32`,
the `IMAGE_ESHELL` libkern mirror, the embedded ELF, the xtask build step).

**I/O is the universal path.** eshell reads input by looking up `/dev/console`
through its **inherited root namespace** and looping `sys_io_submit(console, {Read,
buf, 256})` → `sys_wait(po)` → process the raw bytes (echo, backspace, CR/LF → end
of line) → dispatch. Output is `sys_kprint`. Commands: `help`, `echo`, `lsblk`
(probes `/dev/blk/0..` until `NotFound`).

**Decision — eshell resolves `/dev/console` itself, not handed by init.** The plan
had init pass the console handle via spawn; instead eshell looks it up through the
inherited namespace (which already grants the `/dev/console` binding). Simpler, more
capability-clean (no handle threading), and matches how `parent` resolves `/dev/blk`
etc. So init's spawn is minimal: `SPAWN_ESHELL` with `handle_count = 0`,
`namespace = 0` (inherit). init spawns it in `supervise` after `parent`, as the
persistent interactive console (it never exits; init keeps no handle).

**Proof (QEMU, scripted serial input).** `-serial stdio` is bidirectional, so piping
CR-terminated commands to the QEMU process stdin (after a delay so eshell is ready)
drives real input: COM1 RX IRQ → the Part-1 ISR → ring → eshell's `io_submit` read
completes. The captured session shows the prompt, echoed input, and correct output
for `help` / `echo hello-from-eshell` / `lsblk` (→ `/dev/blk/0,1,2`) / an unknown
command. The Part-1 ISR→ring→PO path is now proven with real input; the slice-8
milestones and console self-test still pass, no faults.

**Note — parent demo / eshell console interleaving (cosmetic).** init still spawns
the Phase-1/2 `parent` demo (regression value); its output and eshell's prompt
interleave on the shared serial console. Functionally fine; a clean interactive
console would gate or retire the `parent` demo — a follow-up decision.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**516**, libkern `image_id_round_trips` updated for `Eshell`. QEMU: a full interactive
session over serial. Branch `phase-2/slice9-eshell-crate`. Next: Part 3 (`cat` +
`HandleInfo.size`).

## 2026-06-27 — Phase 2 slice 9, Part 3: `cat` + `HandleInfo.size` (+ retire the concurrent demo)

eshell `cat`, and the file-**size discovery** it needs — closing the slice-8
deferral. Plus a real bug the interactive shell exposed.

**`HandleInfo.size`.** `HandleInfo` gains a `size: u64` (16 → 24 bytes; both the
kernel `libkern/handle.rs` and the userspace `libkern/abi.rs` copy, layout asserts
updated; **not** in the ABI hash). `stat_on` does one `INSPECT` lookup and reads the
per-type byte size from the object — a `MemoryObject`'s page-rounded size, a
`FileObject`'s exact file size — keeping the object-type logic in the syscall layer,
not the type-agnostic handle table. The lazy resolve (`build_and_install_file`) now
grants `INSPECT` alongside the requested rights (a generic right, benign on a file
the client already maps) so a client can `stat` the size before mapping. This is the
fix the slice-8 deferral named ("a `size` field on `HandleInfo`, first consumer eshell
`cat`").

**`cat`** (eshell): lookup (`MAP_READ | INSPECT`) → `sys_handle_stat` (size) →
`sys_memory_map(size)` → write the bytes (a `FileObject` demand-faults its pages from
the fs-server here — the slice-8 lazy page cache). Trailing NULs are trimmed (a
`MemoryObject` snapshot's page-rounded size leaves padding past the content). Generic
over any mappable, sized resource — so it reads both lazy files and eager memobjs
(e.g. Part 5's `/dev/log`).

**Decision — the `parent` demo now runs to completion *before* eshell, not
concurrently.** The interactive shell exposed a real concurrency bug: `parent`'s demo
chain and eshell both do disk I/O, and the AHCI driver is **single-outstanding-command**
(Phase 2). Overlapping `parent`'s block reads with the fs-server's ext4 reads (driven
by eshell `cat`) corrupted the fs-server's reads → intermittent `NotFound` on
`cat /system/current-generation`. So init now spawns `parent`, waits for it to exit
(in the reap loop), and *then* launches eshell — sequential, so eshell has the disk
and console to itself. This both fixes the flakiness and gives a **clean console** (no
demo output interleaving the prompt), resolving the Part-2 cosmetic follow-up. The
underlying AHCI single-command limitation is recorded as deferred (proper IRP queuing
/ NCQ — `deferred-decisions.md`).

**Proof (QEMU).** After `parent` reaps, eshell launches on a clean console:
`cat /system/current-generation` → `nitrox-rootfs generation 1` (reliably, twice),
`help` / `lsblk` unaffected, boot clean.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**516** (HandleInfo layout/round-trip tests updated on both sides). Branch
`phase-2/slice9-cat`. Next: Part 4 (`mounts` + `sys_ns_enumerate`).

## 2026-06-27 — Phase 2 slice 9, Part 4: `sys_ns_enumerate` + `mounts`

A namespace can be **looked up** but not **listed** — `sys_ns_enumerate` fills that
gap, and eshell `mounts` is its first consumer.

**Decision — `sys_ns_enumerate` is the right shape (a namespace-family syscall).**
Namespaces already have `sys_ns_lookup` / `sys_ns_bind` / `sys_ns_unbind`;
`enumerate` (`= 30`) is the missing sibling — it operates on a `Namespace` handle
(requires `LOOKUP`), synchronously (the kernel holds the bindings; no forwarding).
It is **not** in tension with the "device I/O goes through `io_submit`" rule (that's
the *device* family; namespaces are their own). `sys_ns_enumerate(ns, index, out)`
writes the `index`-th binding (insertion order) to a user `NsEntry`, returning
`NotFound` past the end (the iteration terminator). `NsEntry` (`#[repr(C)]`, kernel
`libkern/handle.rs` + userspace `libkern/abi.rs`, not in the ABI hash): `path_len`,
`kind` (`NS_KIND_DIRECT`/`KERNEL`/`MOUNT`), `rights`, and the path inline
(`[u8; 256]`, truncated past that — every real path is far shorter).

**Scope — bindings, not `readdir`.** `enumerate` lists the namespace's own bindings
(mount points + kernel resources), **not** the files inside a mounted filesystem
(that is an fs-server `readdir`, deferred). `Namespace::enumerate` fills the `NsEntry`
under the binding lock with **no allocation** (a fixed-buffer path copy + a target
`match`); the user copy-out happens in the syscall, outside the lock.

**Proof (QEMU).** `mounts` lists the inherited root namespace: `/dev/entropy`,
`/dev/console`, `/proc/self/*`, `/initramfs`, `/dev/blk` (kernel resources);
`/dev/disk/by-*` (direct handles); and `/` (mount). The kind classification is
correct, and it confirms eshell's inherited namespace *does* carry init's `/` mount
(the Part-3 `cat` flakiness was disk contention, not a missing binding).

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**517** (+1: `enumerate_lists_bindings_in_order_with_kind`). Branch
`phase-2/slice9-mounts`. Next: Part 5 (kernel log ring + `/dev/log`).

## 2026-06-27 — Phase 2 slice 9, Part 5: kernel log ring + `/dev/log` (dmesg)

The kernel's `dmesg`: capture `kprint!` output into a ring and serve it as a
resource, read with `cat /dev/log` — no bespoke command (Part-3 `cat` is generic
over mappable resources).

**The ring** (`kernel/src/klog.rs`): a 16 KiB linear append buffer behind an
`IrqSpinLock`, teed from the serial `write_str` path (the `kprint!`/`kprintln!`
macros **and** the panic/exception emergency writer — so a panic is captured too).
It captures **kernel** diagnostics (boot: `acpi`/`ioapic`/`ahci`/`gpt`/`console`/
`sched`/panic), *not* userspace `sys_kprint` output (that is userspace stdout, not
the kernel log — correct dmesg semantics). Linear (keep-early, drop-when-full): an
emergency inspection wants the boot/failure context, and 16 KiB holds a full boot
log; a keep-recent ring is a later refinement (`deferred-decisions.md`).

**Decision — `IrqSpinLock::try_lock`, and `push` uses it.** The tee runs from
`write_str`, which the panic/exception path also uses, so a fault striking while the
ring lock is held would re-enter and deadlock. `push` takes the ring lock with a new
`try_lock` (skip the line if contended) — logging is best-effort and must never
deadlock the panic path. The reader (`len` / `copy_into_frames`) is syscall context
and blocks normally.

**`/dev/log`** (`KernelServerId::Log`, `MAP_READ` binding): a leaf server that mints
a fresh read-only `MemoryObject` sized to the bytes logged so far and copies the ring
into its frames (under the ring lock, via the HHDM — no allocation under the lock; the
memobj is allocated outside it). `cat /dev/log` reads it (sized by `stat`, NUL-trims
the page tail).

**Decision — `sys_kprint` now translates `\n` → `\r\n`.** It wrote raw bytes, so
userspace output (init, eshell, and now the raw-`\n` kernel log via `cat /dev/log`)
staircased on a real serial terminal. `sys_kprint` now translates (the terminal
convention, as the kernel's own `kprint!` already does), fixing *all* userspace
rendering — a bonus beyond `/dev/log`. eshell's explicit `\r\n` becomes a harmless
`\r\r\n`; left as-is (it works).

**Proof (QEMU).** `cat /dev/log` dumps the kernel boot log — `acpi`/`ioapic`/`ahci`/
`gpt`/`console`/`sched` lines, each correctly on its own line — no faults.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**517** (the klog ring + `/dev/log` are QEMU-proven, like the console driver, not
host-tested). Branch `phase-2/slice9-klog`. Next: Part 6 (init failure → eshell).

## 2026-06-27 — Phase 2 slice 9, Part 6: init failure → eshell (slice 9 complete)

Implement the path `userspace/init/CLAUDE.md` has documented since slice 4 but never
had: on a critical-path failure, init drops to the emergency shell instead of running
the boot milestones. Until now a failed required mount was logged but init soldiered
on into the `parent` demo regardless — exactly the "soldier on past a misconfiguration"
the CLAUDE.md forbids.

**The change** (`userspace/init/src/main.rs`). `mount_all` now returns `bool` (a failed
`required_for = "boot"` mount is critical-path → `false`). `_start` computes
`booted = match read_manifest(root_ns) { Some(m) => mount_all(.., &m), None => false }`
and branches: `!booted` → `emergency(notif)`; otherwise the existing milestones
(`read_current_generation`, `read_large_file`) + `supervise(notif)`. `supervise` was
factored into three functions sharing one reaping loop:
- `supervise(notif)` — healthy: spawn `parent`, then `reap_loop(notif, parent_h)`
  (and if the `parent` spawn itself fails, fall through to `spawn_eshell` + `reap_loop`).
- `emergency(notif)` — failure: log `init: critical-path failure -- dropping to
  emergency shell`, `spawn_eshell()`, `reap_loop(notif, 0)`. No demo, no milestones.
- `reap_loop(notif, mut parent_h)` — the shared wait/drain/reap loop; when `parent_h`
  is non-zero and that child exits (the demo finished), it closes the handle and
  `spawn_eshell()` (the existing healthy-path hand-off to the interactive shell).

So both paths converge on the *same* eshell + reaping loop; the only difference is
whether the boot milestones and `parent` demo run first. The emergency shell is a
fully-capable inspector — it gets the same console + root namespace, so `mounts`,
`lsblk`, and `cat /dev/log` all work for diagnosing why the boot failed.

**Decision — emergency drops *before* the milestones, not after a degraded boot.**
A required-mount failure means `/` is absent; running `read_current_generation` /
`read_large_file` (which read through the root fs) would just produce more failures
before reaching the shell. The operator wants the prompt and the log, immediately.
The non-required-mount story (degraded-but-continue) doesn't exist yet — every mount
in the manifest is `required_for = "boot"` — so "any mount failed → emergency" is
correct today; per-mount `required` gating is a later refinement when optional mounts
appear.

**Proof (QEMU, both paths).** Temporarily forcing the manifest device to
`gpt-partlabel:does-not-exist`: the boot logs `device /dev/disk/by-partlabel/
does-not-exist not found` → `mount FAILED for /` → `init: critical-path failure --
dropping to emergency shell` → `init: starting interactive console (eshell)` → a live
`eshell>` prompt, with **no** `parent` demo and **no** milestone lines; `mounts` from
the emergency shell lists every binding *except* `/` (correct — it never mounted), and
`cat /dev/log` dumps the boot log including the failure. The forced label was then
reverted; the healthy boot is unchanged (milestones → `parent` → `reaped pid=3` →
eshell). No faults either way.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — kernel
**517**, all suites green (Part 6 is init-only; the behaviour is QEMU-proven, not
host-tested). Branch `phase-2/slice9-init-failure`.

**Slice 9 (emergency shell + the first user input) is complete.** The system now boots
to a live `eshell>` over the serial console; the operator can run `help`, `echo`,
`lsblk`, `cat <path>` (lazy page-cache reads), `mounts` (`sys_ns_enumerate`), and
`cat /dev/log` (dmesg); and a critical-path boot failure drops to the same shell for
recovery. Input flows through the universal `sys_io_submit(Read)` + `sys_wait` char-
device path, not a bespoke syscall. Next: slice 10 (FAT read-only for completeness).

## 2026-06-26 — Phase 2 (filesystem and namespace) complete

A close-out stock-take after slice 9. Phase 2's milestone is **met and QEMU-proven**:
`xtask qemu` boots Limine → kernel comes up + enumerates PCI → init starts from the
initramfs → reads `init.toml`, spawns fs-server-ext4 for the ext4 root, waits for
`Ready`, binds `/` → reads `/system/current-generation` and logs it → enters the
reaping loop (and now drops to `eshell` on a critical-path failure). Slices 1–9 and
the prerequisite band are all done.

**Slice 10 (fs-server-fat, read-only) is deferred to Phase 3.** No Phase 2 milestone
clause consumes FAT — the ESP's FAT32 is read by UEFI firmware + Limine, never by
Nitrox — and ext4 already proves the userspace-filesystem path end to end. It is
parity/completeness work; it lands when an in-OS FAT consumer appears. Phase 2 closes
at 9/10 slices by design, not by omission.

**Stock-take finding — the demand-fault fill is slow, and why.** Profiling the boot
(per-line serial timestamps) showed a **silent ~20.8 s gap** with no output, which
turned out to be init's slice-8 `large.bin` milestone (64 demand-faulted pages), *not*
the `parent` demo that follows it — `parent` runs in ~40 ms and its output streams
fine; the gap simply lands immediately before `init: spawning parent`, so the pause
got misattributed to `parent`. Root cause: the kernel fills **one 4 KiB page per
fault** (no read-ahead) and each fill is a **stateless** `File::ReadRange` round-trip
where the fs-server re-reads the superblock, re-resolves the path, and re-walks the
extent tree from disk (~6–8 emulated AHCI reads) — **~325 ms/page** under QEMU. This
is the *documented* cost of the deliberately-simple Phase-2 fill path (see
`deferred-decisions.md`: stateless fill, page-cache scope), not a regression in
behaviour — but it is new since slice 8 (before which there was no large-file
milestone), which is why "it used to feel faster."

**Decision — defer the fix to Phase 3; mitigate now by trimming the fixture.** Two
composing Phase-3 levers close it: **kernel read-ahead (clustered fill)** — fill a
page *cluster* in one `ReadRange`, the single biggest lever and the natural completion
of the slice-8 cache (the wire op already carries `offset`/`len`) — and an **fs-server
open-file cookie** (resolve returns a cookie; `ReadRange` carries it) to make each
fill O(1). Both optimize a path that is *correct* today, and the milestone only needs
to **prove** multi-page demand-faulting, not do it fast; pulling either forward is
Phase-3 storage-hardening work (it also interacts with the AHCI single-command limit).
As a stopgap, `large.bin` was trimmed **64 → 8 pages** (`tools/xtask` generator +
init's `LARGE_FILE_BYTES`, kept in sync): it still spans 8 position-sensitive pages
(still proves the cache lifts the old 64 KiB eager cap), and boot now reaches `eshell`
in **~7.9 s** instead of ~25.8 s (the large-file read dropped from ~20.8 s to ~2.8 s).
Recorded in `deferred-decisions.md` (demand-fault fill latency).

**Stock-take finding — no functional Phase-2 gaps.** An audit of the plan, the
deferred-decisions register, and source `TODO`/`FIXME` markers found every open item
to be genuinely Phase-3 foundational work (SMP bring-up, scheduler classes, runtime
libraries `libos`/`librt`/`libstream`, service-manager skeleton, fs RW + Model-A
extent fill + writeback, reclaim daemons, MSI/MSI-X, AHCI NCQ, intermediate-page-table
reclaim) — not holes in Phase 2's stated scope. Phase 3 must begin with that
foundational infrastructure before the service-manager / "system idle" milestones are
reachable. No new Phase-2 slices were added.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` all green;
boot re-profiled in QEMU after the fixture trim (eshell reached at ~7.9 s, healthy
path; the emergency path unchanged). **Phase 2 is complete; Phase 3 (service
ecosystem) is next.**

## 2026-06-26 — Phase 3 scope analysis + kernel-first sequencing

Opening stock-take of Phase 3 ("service ecosystem"), which is much larger and less
defined than Phase 2. The plan's Phase 3 section was a **flat checklist grouped into
~12 categories with no slice sequencing and no prerequisite band** — the structural
gap that made it not-startable as written (Phase 2 only worked once it was
re-sequenced into ordered slices). Two parallel investigations mapped (a) the current
kernel scheduler/SMP state and (b) the design-doc coverage of every Phase 3
workstream.

**Readiness tiers.** Workstreams sort into: *ready to build* — a spec already
exists: libstream (`typed-stream-format.md`), service-mgr schema
(`service-toml-schema.md`); *partially sketched* — concept committed but details
open: scheduler classes, SMP, libos `Handle<T,M>`/executor, profile server, content
store, fs-server RW + Model-A extent fill, auth+session; *just a checkbox* — no
design at all: librt fiber scheduler, logging service, audit subsystem, and the
OOM / mount / crash-reporter / namespace-manager / device-manager daemons. ~8
architecture docs referenced by `overview.md` (`scheduler.md`, `userspace-runtime.md`,
`content-addressed-store.md`, `init-and-services.md`, logging/audit/session docs, …)
**do not exist**; they are written per-workstream, just-in-time, as each is reached —
not all upfront. These are the Phase-3 "pause points."

**Kernel-first state (verified in source).** Preemption is wired (`sched.rs:474`
timer-tick reschedule), but the scheduler is a **single global `SCHED` lock + flat
round-robin runqueue** (`sched.rs:299,258`) with **no classes / priorities / per-CPU
anything**. SMP is a **complete stub**: `cpu_count→1`, `current_cpu→0`,
`send_ipi→unimplemented!` (`arch/x86_64/smp.rs`), Limine's SMP request **unwired**
(`limine.rs`), shared GDT/TSS/IST, one `CPU0` per-CPU block, xAPIC-only shared APIC
MMIO, local-only `flush_tlb_*` (no IPI shootdown), `sys_thread_set_affinity` a
validating no-op (`table.rs:384`). The good news: the spinlocks are already
SMP-correct (atomic CAS, `spinlock.rs:72`), so the locking substrate holds; the
single-CPU stand-ins to convert are the scheduler's single `current`, the
handle-table grace-period ctx-0 shim, and the absence of per-CPU page-table-root
tracking. The committed design (`os-design-v5.1.md:922-953`: RealTime fixed-priority
FIFO, TimeShared CFS-like vruntime, Idle; per-CPU runqueues; work stealing;
affinity-on-wake) and the 3-step staging (decision log 2026-05-29) are intact.

**Decision — sequence the kernel-first work into slices 0–3, add the missing
slice 0, and roll SMP out incrementally** (chosen with the user):
- **Slice 0 — per-CPU foundation + `docs/architecture/scheduler.md`.** A design slice:
  write the doc (settling the open decisions — class scope, TimeShared fidelity,
  x2APIC now-or-later, `MAX_CPUS`, per-CPU layout, IPI vectors, shootdown algorithm)
  *and* build the per-CPU substrate (`CPU0`→`CPUs[N]`, GS-based per-CPU area,
  `cpu_id()`) + convert the single-CPU stand-ins, **still on one CPU**. Foundational
  code and its design land together and stay verifiable (boots as today).
- **Slice 1 — SMP bring-up**, APs pulling from the **existing single global
  runqueue** (incremental: AP startup is proven against a scheduler we already
  trust). Includes IPI + TLB shootdown (mandatory the moment two CPUs share an AS).
- **Slice 2 — scheduler classes** (RealTime/TimeShared/Idle).
- **Slice 3 — per-CPU runqueues + work stealing + affinity**; `sys_thread_set_affinity`
  made functional.
Rejected: per-CPU runqueues from the start (couples AP bring-up with load-balancing
into one un-bisectable slice); a docs-only slice 0 (would bloat slice 1 with the
per-CPU substrate). The userspace backlog stays unsequenced for now and is sliced
as we reach it. No code written yet — slice 0 begins with the design doc, whose open
decisions are the next pause point.

## 2026-06-26 — Phase 3 slice 0: per-CPU foundation + scheduler/SMP design doc

The first kernel-first Phase-3 slice. Wrote `docs/architecture/scheduler.md` (the
design contract: three classes — RealTime fixed-priority FIFO + TimeShared vruntime +
Idle; x2APIC; incremental SMP) and built the per-CPU substrate that the rest stands
on, **all still single-CPU** (no APs). Decisions settled while writing the doc:
RealTime+TimeShared+Idle all in the first cut; TimeShared = vruntime fairness on the
existing deadline min-heap pattern (no rbtree); x2APIC-only adopted in slice 1 (dev
loop → QEMU ≥ 9.0 + `+x2apic`); incremental SMP (APs on the shared runqueue first).

**Decision — per-CPU access is arch-abstracted (`current_cpu()` + RDTSCP/`TSC_AUX`).**
The doc had not pinned *how* kernel code identifies its CPU, and the kernel runs with
`GS_BASE = 0` (the per-CPU `CpuLocal` is only reachable via `KERNEL_GS_BASE` inside the
syscall stub), so a mechanism had to be chosen. Per the arch-boundary rule, neutral
code calls **`arch::Smp::current_cpu() -> u32`** (a dense index) and indexes
`CPUS[current_cpu()]`; the x86 mechanism is internal: `current_cpu()` reads a dense
index from `IA32_TSC_AUX` via **`RDTSCP`** (one instruction, no map, GS-convention-
neutral), set per CPU by a new `ArchSmp::init_this_cpu(index)`. The mechanism is
swappable behind the abstraction (x86 could later go GS-relative; aarch64 uses
`MPIDR_EL1`/`TPIDR_EL1`). Rejected: LAPIC-id + an id→index map (MMIO read + sparse-id
table). `RDTSCP` is universal on the ≈2014 baseline but **not** in `qemu64`'s default
feature set — it `#UD`'d until the dev loop opted in `+rdtscp`. Under `cargo test`,
`current_cpu()` returns `0` (host RDTSCP would yield the unbounded host id and overflow
the per-CPU arrays).

**What landed.** `MAX_CPUS = 8` (neutral, sizes both per-CPU arrays). The arch
`CpuLocal` GS block became `CPUS: [CpuLocal; MAX_CPUS]`, each CPU's `KERNEL_GS_BASE`
pointing at its own slot (`this_cpu_block()`); the syscall/`swapgs` invariant is now
stated per-CPU. The scheduler's `current`/`idle`/`idle_addr` became per-CPU arrays
behind small `cur_slot`/`idle_slot`/`idle_addr` accessors (~27 sites), keeping the
**single global `ready` queue + one `SCHED` lock** (per-CPU runqueues are slice 3).
`handle::current_ctx_id()` now returns `current_cpu()` (one grace-tracker context per
CPU; `MAX_CPUS ≤ MAX_CTX`). The BSP calls `init_this_cpu(0)` at boot and logs a `smp:`
line.

**Decision — page-table-root / `active_cpus` tracking refined to slice 1.** Originally
listed in slice 0, it has no slice-0 consumer (only the TLB shootdown reads it) and
adds context-switch bookkeeping best landed *with* the shootdown that exercises it, so
it moves to slice 1 alongside AP bring-up.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` — all 8 host
suites green (the per-CPU current/idle paths run host-side at cpu 0; the RDTSCP/`wrmsr`
ops are QEMU-proven, not host-testable). Boots identically to today — `smp: cpu 0
online`, the full `parent` demo (threads created/switched/blocked/woken/exited/reaped),
and the `eshell` prompt — no `#DF`/`#GP`/panic. Branch
`phase-3/slice0-percpu-foundation`. Next: slice 1 (SMP bring-up).

## 2026-06-26 — Phase 3 slice 1: SMP bring-up (APs online; correctness hardening deferred to slice 3)

The application processors come up and run kernel code; the full userspace system
boots **reliably on `-smp 4`**. Sequenced as four chunks — A) x2APIC, B) per-CPU
GDT/TSS, C) AP bring-up, D) TLB shootdown — of which A–C landed and D (plus the
broader SMP-correctness hardening) is deferred to slice 3.

**A — x2APIC (committed).** Rewrote `apic.rs` from xAPIC MMIO to **x2APIC** (MSR
accessors at `0x800 + reg>>4`; 32-bit id; EXTD-enable via the SDM enabled→x2APIC
two-step, guarded so an already-x2APIC CPU isn't driven through the illegal `11→10`
transition; `send_ipi` as a single 64-bit ICR `WRMSR`). The dev loop opts in
`+x2apic` (TCG needs QEMU ≥ 9.0). x2APIC-only, no xAPIC fallback (the ≈2014 baseline
guarantees it).

**B — per-CPU GDT/TSS.** `GDT`/`TSS`/`#DF`-stack became `[…; MAX_CPUS]`; each CPU loads
its own (the TSS holds per-CPU stacks). The IDT stays a single shared table — only the
`lidt` is per-CPU (`idt::load`).

**C — AP bring-up.** Wired Limine's MP request (magic verified against the protocol).
Limine starts + parks the APs; the kernel launches each by an **atomic write to
`goto_address`** (no INIT/SIPI, no real-mode trampoline) — the AP jumps to an ordinary
`extern "C"` entry with its `MpInfo*` in RDI. The BSP assigns **dense** logical indices
(0 = BSP; APs 1…) via `extra_argument` (robust to sparse ACPI ids). Each AP:
`init_this_cpu(idx)` → `arch::ap_cpu_init` (per-CPU GDT/TSS, shared IDT, NX/SMEP/SMAP,
x2APIC enable, syscall MSRs) → arm its LAPIC timer → `sched::ap_run`, which creates that
CPU's boot+idle threads and retires the boot thread into the scheduler — the AP then
pulls runnable threads from the **shared** global runqueue. *Verified `-smp 4`:* `smp: 4
CPU(s) online (1 BSP + 3 AP)`, each AP logs `cpu N online (AP)` from its own entry, and
the full userspace boot (init → ext4 mount → parent demo → eshell) is clean **6/6
runs**; `-smp 1` unchanged.

**Two real SMP bugs found + fixed during C:**
1. **Reap use-after-free (cross-CPU).** `finish_exit` parked a dying thread in a
   **shared** `reap` list and then `switch_into`'d off its own stack; another CPU's
   `reap_pending` could free that kernel stack mid-switch. Fix: `reap` is now **per-CPU**
   (`reap[MAX_CPUS]`) — a thread is reclaimed only by the CPU it died on, after its
   switch completed (so the stack is provably off-CPU). Host-tested.
2. **Stale `KERNEL_GS_BASE` defense.** Re-assert this CPU's `KERNEL_GS_BASE` on every
   user-thread switch-in (`arm_kernel_stack_for`), so a migrated thread's next syscall
   `swapgs`es into the CPU it now runs on.

**Decision — chunk D (TLB shootdown) + user-thread-migration safety deferred to slice 3
(option B, chosen with the user).** An aggressive stress test (8 kernel threads exiting
simultaneously across CPUs + a busy-wait barrier) exposed, *after* the per-CPU-reap fix,
a second hazard: a kernel-stack UAF surfacing as a `#DF` **inside `syscall_entry`** when
a **user thread is forced to bounce between CPUs**. Ruled out as causes: an idle-thread
leak into `ready` (instrumented, never fired) and a wrong `KERNEL_GS_BASE`. This is the
shared-runqueue model's intrinsic cross-CPU **migration** hazard — exactly what the plan
defers by putting **per-CPU runqueues in slice 3** (which keep threads on one CPU,
removing the churn) — so chasing the last UAF in a model slice 3 reworks is poor
sequencing. The *real* workload does not trigger it (no pathological migration); the
stress test was removed and `-smp 4` is reliable. Slice 3 gains explicit entry criteria:
TLB shootdown + `active_cpus`; user-thread-migration safety (re-add the churn stress
test as the gate); fix `has_live_siblings`/`exit_process` to see siblings on other CPUs'
`current[]`; and audit the remaining "single-CPU" assumptions in `sched.rs`.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` (8 host suites);
`-smp 4` 6/6 clean full boots; `-smp 1` unchanged. Dev-loop default stays `-smp 1`
(deterministic); `-smp N` exercises SMP. Branch `phase-3/slice1-smp-bringup`. Next:
slice 2 (scheduler classes), then slice 3 (per-CPU runqueues + the SMP hardening above).

## 2026-06-29 — Phase 3 slice 2: scheduler classes (RealTime / TimeShared / Idle + vruntime)

The flat round-robin run queue becomes **class-aware**. Each `Thread` gains a
`SchedClass` (`RealTime` / `TimeShared` / `Idle`) plus `rt_priority` (`0..=99`, RealTime),
`nice` (`-20..=19`, TimeShared) and `vruntime`. The dispatch precedence is strict:
any runnable RealTime thread preempts any TimeShared, which preempts Idle (the per-CPU
fallback). Defaults are behaviour-preserving — every thread is `TimeShared`/`nice 0`,
so the existing workload schedules exactly as before.

**Pick by policy, not per-class structures.** Rather than split `ready` into priority
buckets + a vruntime tree, `dequeue_front` keeps the single shared `ready` list and
**scans it for the best thread by key** `(class_rank, rt_priority, vruntime)` — RealTime
(rank 0, lowest priority value, FIFO within a priority via a strict `<`) ahead of
TimeShared (rank 1, smallest vruntime). An O(n) scan is ample at our thread counts and
keeps enqueue + every other `ready` site untouched; per-class heaps/buckets are a later
optimization, noted in `scheduler.md`.

**TimeShared fairness = CFS-lite.** A running TimeShared thread accrues vruntime each
10 ms tick, scaled by the Linux nice-weight table (`slice * 1024 / weight(nice)`), so a
lower nice accrues slower and is picked more often. A monotonic `min_vruntime` floor (set
to each picked thread's vruntime) seeds newcomers (a spawned thread jumps to the floor so
a `vruntime == 0` newcomer can't hoard the CPU) and boosts wakers (`floor - slice`, a
latency credit) — both clamped so a thread already ahead keeps its own vruntime. RealTime
threads do not accrue; Idle never enqueues.

**Decision — defer the `REAL_TIME` syscap gate + the user-facing `ThreadArgs` class/nice/
affinity ABI to the SysCaps slice (option B, chosen with the user).** The slice-0 design
put a `REAL_TIME` syscap gate in slice 2, but **`SysCaps` is documented only** (service-
toml-schema, supervisor-registration) — there is no capability bitmask on `Process` yet,
and building one is a service-mgr/auth concern, not a scheduler one. So slice 2 delivers
the *dispatch* (demonstrated with trusted kernel threads via `spawn_with_class`); user
threads default to TimeShared; and the gate + the user ABI (relaxing `ThreadArgs._reserved`
to carry class/nice/affinity per `thread-args.md`'s deferred "richer attributes form")
land when the capability system is built. Avoids pulling the cap system into the scheduler
slice.

**Verified.** A boot-time `sched_class_demo` (a RealTime worker + two TimeShared workers,
nice 0 and nice 10) prints the RealTime worker finishing *before any* TimeShared round,
then the nice-0 worker completing all three rounds while nice-10 is still on round 1 —
RealTime-preempts-TimeShared and vruntime fairness, both visible in the serial trace.
`cargo xtask build` (no warnings) / `check-arch` / `test` (8 host suites, +4 new = 521);
`-smp 1` and `-smp 4` (2/2) boot clean to eshell with the demo, 0 faults. Branch
`phase-3/slice2-scheduler-classes`. Next: slice 3 (per-CPU runqueues + work stealing +
affinity + the SMP-correctness hardening deferred from slice 1).

## 2026-06-29 — Phase 3 slice 3: per-CPU runqueues + work stealing + affinity (TLB shootdown → 3b)

The single shared run queue becomes **per-CPU** (`ready: [KVec; MAX_CPUS]`, mirroring the
existing `current`/`idle`/`reap` arrays), guarded by the one `SCHED` lock (per-CPU
*queues*, not per-CPU *locks* — that's a later step). This both delivers the headline
feature and is the structural fix for the slice-1 user-thread-migration hazard. Built as
four chunks (A–D); TLB shootdown (chunk E) was split to **slice 3b** (option B, chosen
with the user) since it is orthogonal page-table-coherence work not triggered by today's
workload.

**A — Per-CPU runqueues + placement.** `dequeue_front` picks from *this* CPU's queue; a
preempted thread re-homes to **its own** CPU (it no longer lands on a shared queue any CPU
can drain). A `place_thread` policy chooses a target at spawn/wake: kernel threads go to the
**least-loaded** CPU, a waking thread to its **home** CPU (`last_cpu`, recorded each
switch-in). `min_vruntime` is now per-CPU (a placed thread is seeded against the target
CPU's floor). `ready_is_empty`/`reap_matching`/`has_live_siblings` sweep all CPUs. A
boot-time demo distributes 8 kernel workers and reports a CPU mask of `0b1111` — work runs
on every AP.

**Bug found + fixed (placement).** The first cut tracked online CPUs as a count `n_online`
and assumed CPUs `0..n_online` were a dense initialized prefix. But **APs run `ap_init` in
arbitrary order**, so the online set can be `{0,3}` while `n_online == 2`; placement then
targeted an uninitialized CPU whose `ready` queue had capacity 0, and `spawn` failed
intermittently. Fixed by tracking a **`cpu_online: [bool; MAX_CPUS]` mask** (each CPU sets
its own bit in the same critical section that reserves its queues), scanned by every
placement/steal site.

**B — Work stealing.** When a CPU's queue is empty it **steals** from the busiest peer
rather than idling: the block/exit/suspend switch paths route through `pick_next`
(local-queue → steal), and an idle CPU's timer tick triggers a steal when a busier CPU has
runnable work. A stolen thread is re-seeded against the stealer's vruntime floor.

**C — Affinity.** A `cpu_mask: u8` on `Thread` (default all-ones) is honoured by placement
and stealing; `sys_thread_set_affinity` (a no-op before) now writes it (gated by `SIGNAL`
on the handle — **no SysCaps needed**; affinity-at-creation via `ThreadArgs` stays with the
capability work). A demo pins one worker per CPU and confirms each runs on exactly its
pinned CPU. (Affinity-at-creation for *user* threads is moot today — see below.)

**D — SMP-correctness hardening + the migration decision.** `has_live_siblings` now also
scans other CPUs' `current[]` (a sibling running elsewhere must keep the process alive).
The slice-1 `#DF`/illegal-instruction **user-thread-migration UAF** (a running user thread
corrupting after moving CPUs; the `syscall_entry` per-CPU-stack hazard, never fully
root-caused) reappeared once stealing/placement moved user threads — at ~1/10 vs the old
~1/4. Rather than chase a hazard we couldn't pin, **slice 3 prevents user-thread migration
entirely** (chosen with the user): a new user thread is placed on its **creating CPU**, a
preempted user thread re-homes there, a wake returns it home, and stealing **skips user
threads** (kernel-only). User threads therefore never change CPU, structurally avoiding the
hazard. **Result: 12/12 clean `-smp 4` full boots** (was intermittent). The cost — userspace
currently runs on the BSP; letting user threads use the APs needs the `syscall_entry`
hazard root-caused (deferred, with TLB shootdown, to slice 3b). Also noted: `exit_process`
does not terminate a sibling *running* on another CPU (needs a cross-CPU deschedule IPI —
slice 3b); not triggered by today's single-threaded-`process_exit` workloads.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` (8 host suites, +6
new = 524); `-smp 4` **12/12** clean full boots (class + distribution + affinity demos →
init → ext4 mount → parent demo → eshell, 0 faults); `-smp 1` unchanged. Branch
`phase-3/slice3-percpu-runqueues`. Next: slice 3b (TLB shootdown + `active_cpus`; the
cross-CPU deschedule IPI; root-cause user-thread migration so userspace can use the APs).

### 2026-07-01 — Phase 3 slice 3b (in progress): user-thread migration hazard root-caused; SCE-arm + APIC-id-index fixes

The slice-1/3 "`syscall_entry` per-CPU-stack hazard" that forced userspace onto the BSP was
run down (largely) via extensive **KVM** reproduction — the hazard is a **bring-up timing
race** so sensitive that any in-path instrumentation suppresses it, so it was observed with
fault-path-only probes, `-d cpu_reset`, and non-perturbing atomics. It is not one bug but a
family of **per-CPU-state hazards that bite when a user thread runs on a CPU that isn't yet
fully/correctly initialised**; the symptom cascade is `#UD` (EFER.SCE=0) → or `#DF` (fault
delivered onto a bad kernel stack) → triple-fault → **VM reboot** (which can loop). Two
distinct causes were fixed:

1. **Syscall MSRs not armed at descent.** A CPU could reach a ring-3 descent before its
   bring-up `init_syscall_entry` was in effect → the thread's first `syscall` `#UD`s
   (`EFER.SCE=0`). Fix: `arm_user_entry_cpu_base` (already re-asserts `KERNEL_GS_BASE` on
   every descent for migration) now also **ensures the syscall MSRs are armed** — a cheap
   `rdmsr(EFER)` gate every descent, with the full re-arm only in the (should-never-happen)
   unarmed case, so no steady-state cost.

2. **Dense-index collision.** The dense CPU index was handed to APs via Limine
   `extra_argument` and relied on `IA32_TSC_AUX`'s reset-default 0 for the BSP — a racy
   scheme where a core could run with a **colliding** index and thus share another core's
   per-CPU **GDT/TSS/scheduler slots** (loading the wrong TSS → exception delivery onto a
   shared `RSP0` → `#DF`). Fix: dense indices are now derived from the **hardware APIC id**
   (`smp::bind_cpu_identity` / `adopt_dense_index`, re-exported neutrally) — unique **by
   construction**; a core whose APIC id was never bound **parks** rather than colliding.
   The `extra_argument` channel is no longer used for identity.

**Residual — the dominant one, a cross-CPU kernel-vmap coherence gap (no TLB shootdown
yet).** A thread's *first exception* can be delivered onto a kernel stack whose translation
is **stale on the running CPU**: an AP mutating the **shared kernel-vmap** page tables for
its own kstacks leaves other cores' cached paging structures stale, so init's `RSP0` push
faults → `#DF`. **This is pre-existing (since slice 1) and NOT migration-specific** — it
reproduces even with user threads pinned to the BSP, because APs churn kstacks in the shared
vmap regardless. **Correction to the slice-3 log:** the "12/12 clean `-smp 4`" claim was
TCG / few-boots; under heavy **KVM** boot-looping the pinned config fails ~**33%** with this
`#DF`. The two fixes above cut that to ~**15%** (they close the SCE and dense-index causes;
the coherence cause remains). Closing it is the **next 3b step** — the TLB-shootdown
machinery (broadcast IPI + synchronous ack) drafted this session, re-aimed at the kernel-vmap
*mutation* sites (kstack map/unmap); re-enabling user-thread distribution rides on it.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` (18 host suites)
green. `-smp 4` KVM boot-loop: **~33% → ~15%** init `#DF` vs HEAD (a strict improvement, not
a regression — HEAD was never KVM-clean); `-smp 1` unaffected. Committing this partial
progress by explicit decision (fixes are correct and independently valuable); the coherence
residual is tracked as the immediate next step. Branch `phase-3/slice3b-tlb-shootdown`.

### 2026-07-01 — Phase 3 slice 3b: migration hazard fully root-caused + fixed (KVM 0/150)

Continuing the 3b investigation above, the remaining hazard was root-caused to **two
independent bugs** and both fixed; `-smp 4` under **KVM** now boots **0 failures / 150** (was
~13% at the previous 3b checkpoint, ~33% at slice-3 HEAD). Work-stealing stays enabled.

**Bug A — the switch-out race (a stolen thread resumed from a not-yet-committed context).**
In `sched::switch_to_next`, the outgoing thread `prev` is enqueued into its CPU's `ready`
queue and the `SCHED` lock is released (inside `switch_into`) **before** `context_switch`
commits `prev`'s `saved_sp`. Another CPU could `steal_one` `prev` in that window and resume
it from a **stale `saved_sp`** → the thread runs on two CPUs → `current[]` desyncs from the
actually-running thread. Observed end effect: the boot thread's `exit_thread` reaping **CPU
0's idle thread** (because `current[0]` had been left pointing at idle), freeing the live
idle kernel stack → `#DF` on the idle thread's next interrupt. Fix (Linux `task::on_cpu`
model): a per-`Thread` `on_cpu` `AtomicBool`, **set** under `SCHED` in `switch_into` before
the lock release and **cleared by `context_switch`** (arch asm, new `prev_on_cpu: *mut u8`
param) immediately **after** it commits `saved_sp`; `stealable_to` skips a thread whose
`on_cpu` is set. x86-TSO orders the sp-commit store before the flag-clear store, so a stealer
that observes the clear also sees the final `saved_sp` — no fence needed.

**Bug B — a dense-index collision from a mis-placed BSP-identity call.** `init_this_cpu(0)`
(hard-codes dense index 0 via `wrmsr(IA32_TSC_AUX, 0)`) lived inside `run_first_userspace()`,
which `kernel_main` calls **after** `bring_up_aps()`. Once APs are online the (stealable) boot
thread can migrate onto an AP; running `init_this_cpu(0)` there overwrites **that AP's**
TSC_AUX with 0, so the AP's `current_cpu()` (RDTSCP) returns 0 and it **aliases dense 0**,
sharing the BSP's per-CPU `current[0]` / `idle[0]` scheduler slots. This is the same
slot-sharing class as the slice-3 dense-index collision, re-introduced by call placement.
Fix: moved the BSP-identity block to **before** `bring_up_aps()` (and before the scheduler
first reads `current_cpu()`), where the boot thread is still pinned to the BSP; each AP still
derives its own index from hardware in `adopt_dense_index`.

**Method.** KVM (`-accel kvm`) reproduces; TCG hides/perturbs it. It is a bring-up timing
heisenbug — hot-path instrumentation (reading `CPUID`/APIC-id every switch, logging every
ring-3 descent) suppresses it. It was pinned with fault-path-only probes: a non-perturbing
kernel-stack **drop-ring**, a `#[track_caller]` reap marker (showed the reaper was the boot
thread's `main.rs` exit), a per-switch **trace ring** (showed the boot thread migrating), and
finally a marker printing `this_cpu()` **vs** the true hardware APIC id (`cpu=0` but
`hw_apic=3` — the TSC_AUX desync, proving Bug B). All diagnostics were removed after the fix.

**Also fixed (pre-existing, same slice).** `KernelStack::Drop` → `tlb::shootdown_all()` →
`Paging::flush_tlb_*` executed privileged `invlpg` / `mov cr3`, which `#GP` (SIGSEGV) under
host `cargo test`; `mm::kstack::tests::drop_unmaps_stack_pages` had been crashing the suite
since the TLB-shootdown commit. The two flush primitives now have `#[cfg(test)]` no-op stubs
(mirroring `smp.rs`'s `current_cpu`/`init_this_cpu`), since host tests exercise the page-table
*memory* edits (via HHDM) and have no TLB.

**Verified.** `cargo xtask build` (no warnings) / `check-arch` / `test` (all host suites,
incl. the previously-crashing kstack tests) green. `-smp 4` KVM boot-loop **0/150**; `-smp 1`
unaffected. User-thread *migration* is still disabled (`stealable_to` excludes `is_user`;
`place_thread` pins new user threads to their creating CPU) — the hazards that blocked it are
now all resolved, so re-enabling it is the next 3b step. Branch
`phase-3/slice3b-tlb-shootdown`.

### 2026-07-01 — Phase 3 slice 3b: user-thread migration enabled

With the migration hazard fixed (above), user threads are now allowed to distribute across
and migrate between CPUs. Two scheduler changes: `place_thread` places a newly spawned user
thread on the least-loaded permitted CPU (`pick_target_cpu`) instead of pinning it to its
creating CPU; `stealable_to` no longer excludes user threads (the `is_user` guard is gone, and
`Thread::is_user` was removed as its last consumer). Correctness rests on the per-switch
re-arm already present in `switch_into`: `resolve_root` (CR3) and `arm_kernel_stack_for`
(TSS.RSP0 + syscall stack + KERNEL_GS_BASE) run for the incoming thread on every switch, so a
user thread resuming on a different CPU always has that CPU's kernel-entry state pointed at its
own stack; syscall MSRs are additionally re-armed at each ring-3 descent.

Verified: -smp 4 KVM boot-loop 0/150 with userspace on the APs; a 50-boot scripted eshell
stress (help / lsblk / mounts / cat /system/current-generation / cat /dev/log) 50/50 clean —
user threads doing console + fs syscalls while migratable. -smp 1 unaffected; host
test/check-arch green.

Remaining for slice 3b: a cross-CPU deschedule IPI (so exit_process/kill can stop a sibling
running on another CPU) and per-AddressSpace active_cpus (targeted shootdown). Neither is
exercised yet — every userspace process is single-threaded — so they land with the first
multi-threaded user process rather than as consumer-less infrastructure.

## 2026-07-13 — Phase 3 userspace-runtime sequencing (allocator → libs → SysCaps → librt) + std deferred

Stock-take at the start of the Phase 3 userspace work, after the kernel-first band (slices
0–3b) closed. Two decisions.

**1. Defer a real `std` port; invest in Nitrox-native runtime libraries instead.** The
services want standard-Rust ergonomics, and the tempting move is to bring up a partial `std`.
Rejected for now on two grounds. (a) **ABI coupling** — `std` sits on the syscall ABI, which
is explicitly pre-stabilization (`docs/spec/syscall-abi.md` § Stability; syscall numbers
aren't even in the ABI hash). Forking rust-std against a moving ABI is the same anti-pattern
we avoid elsewhere. (b) **Philosophical mismatch** — `std::fs`/`net` assume *ambient
authority* (path-based open), `std::io` is *synchronous blocking*, and `std` carries errno,
signals, and `thread_local!`; all of these contradict Nitrox's capability + async-first +
no-signals model. A faithful port would either reintroduce the Unix patterns the OS rejects
or stub half its surface. The parts that *do* map (`core`, `alloc`, `collections`, most of
`sync`, `fmt`) come nearly free via `alloc` + a native runtime; the parts that don't are
exactly `fs`/`net`/`io`/`thread`/`process`/`env` — the POSIX core. `std`'s real payoff is
building *unmodified* crates.io crates, which we don't need yet. Kept in Phase 4+, gated on a
stabilizing ABI + a concrete external crate that justifies it. Design the runtime libs
std-shaped where free (an `io::Error`-shaped error type) so the eventual port is re-exports,
not a rewrite.

**2. Sequence the userspace-runtime band by ABI coupling: allocator → libos core + libstream
→ SysCaps → librt + authority wrappers → services** (Phase 3 slices 4–7 in the impl plan). The
syscall surface these libs wrap is mostly *solid* today (handles, memory, `sys_wait`, IPC,
notifications, ns, `io_submit`, entropy). The parts that will still move are **SysCaps**
(unimplemented — no type exists in the kernel; authority is faked with handle `Rights`
stand-ins) and the **`SpawnArgs`/`ThreadArgs`** growth (class/nice/affinity + syscap
inheritance, deferred *to* the SysCaps slice by slice 2). So: **(4)** a freeing userspace heap
(`libheap`) replaces init's bump arena — ABI-independent, leads; **(5)** libos core + libstream
wrap only the solid surface; **(6)** SysCaps lands the capability model + finalizes
`ThreadArgs`/`SpawnArgs` (ABI-hash bump); **(7)** librt (Go-style fibers over the libos
executor) + the thread-spawn/authority wrappers held back from slice 5. This is the same
ABI-stability discipline that deferred std, applied one level down — wrap the solid core first,
hold the authority-facing wrappers until their ABI settles.

**SysCaps: scope stub now, full design after slice 5.** Its architecture doc
(`docs/architecture/syscaps.md`) is written at slice-6 start rather than now, because building
libos/libstream first surfaces which authorities the services actually gate on — so the syscap
set is derived, not guessed. Placed **before** the service backlog (service-mgr, auth/session,
audit all assume it), so services are built capability-correct rather than retrofitted.

**Dogfood via init/eshell.** Each library's first consumer is init (then eshell): converting
them de-hacks the existing critical-path code, validates the lib against real code before any
service depends on it, and honours "no code without a consumer." Constraint: init is
critical-path (no `panic!`/`unwrap`), so every conversion rides behind the existing gate —
still boots to a live `eshell>` and passes the scripted `help`/`lsblk`/`mounts`/`cat` stress.

Also refreshed the impl plan's stale "Current status" block (Phase 2 was still marked "not
started" despite completing 2026-06-26; Phase 3 now reflects the kernel-first band done +
the userspace band next). No code changes in this entry — planning only.

## 2026-07-13 — Cut `librt`: no green-thread crate, no standalone sync-wrapper crate

Follow-on to the userspace-runtime sequencing (above). The planned `librt` crate had two
jobs — a Go-style **fiber (green-thread) scheduler** and **synchronous/blocking wrappers**.
Both are cut; there is no `librt` crate. The runtime library set is **five**: libkern,
libheap, libos, libstream, librsproto.

**Fibers rejected.** A stackful fiber scheduler adds no *concurrency capability* the libos
async executor doesn't already provide — a single-threaded executor multiplexes many `async`
tasks over `sys_wait`, which already covers "N concurrent clients on one OS thread." Fibers
only add a blocking-*style* syntax (dodging async's function-colouring). Against that thin
ergonomic win: a second, non-standard cooperative-scheduling runtime over the same primitive;
the same composition hazards with a future `std` that `async` has (`thread_local!` is
per-OS-thread, `std::sync::Mutex` blocks the OS thread) but in a form crates aren't written to
tolerate; stackful cost (per-fiber stacks, context-switch asm, awkward FFI); and swimming
against Rust's own history (green threads / `libgreen` were removed pre-1.0, RFC 230, for
exactly these reasons). Nitrox's async-first syscall ABI makes `async`/`await` the
grain-aligned model, not fibers. If a genuine "synchronous code that must yield" need ever
appears (e.g. ported non-async code), it can be an optional higher-level crate then — not a
base system library.

**The standalone sync-wrapper crate rejected — it fails the std-port durability test.** The
test (from the sequencing entry): a runtime library earns its place if it either **feeds
std's platform layer** (survives *below* std) or **provides what std never will** (survives
*beside* std). A sync-wrapper crate does neither. std's pal implements a blocking `File::read`
directly as `io_submit` + `sys_wait` (raw plumbing in libkern/libos-low-level), *not* via a
`block_on`-over-futures convenience — so sync wrappers don't feed the pal. And the synchronous
blocking API *is* `std::io`/`std::thread` — so a sync-wrapper crate is precisely what the port
supersedes, and it would be deprecated the day std lands. Running the test across the set:
libkern / libheap engine / libos plumbing feed the pal; libos async runtime / libstream /
librsproto are things std never provides; a sync-wrapper crate is the only stopgap → cut.

**What we keep.** The *capability* to write blocking-style code before std exists is a trivial
`block_on(future)` + a few blocking helper methods, folded into **libos** as a small,
clearly-marked pre-std corner (superseded by `std::io`), not a separate crate — and largely
optional, since init/eshell already do direct submit+wait. Phase 3 **slice 7** survives, minus
librt: it becomes "the SysCaps-coupled libos surface" (the `thread_create`/`process_spawn`/
authority wrappers held back from slice 5, wrapping the SysCaps-finalized `ThreadArgs`/
`SpawnArgs`). In-process concurrency until real OS threads exist is `async` tasks on the libos
executor, which also cleanly defers the kernel TLS (`FS_BASE`) / FPU (`XSAVE`) work until a
workload genuinely needs multicore parallelism within a process.

Docs updated: `overview.md` (five crates; the no-librt rationale; the feeds-pal / beside-std /
stopgap framing of the std port), the impl plan (slice 5 gains the libos `block_on` note;
slice 7 reframed; the band preamble + net-order + back-reference de-librt'd). Planning only —
no code changes.

## 2026-07-13 — Phase 3 slice 4: `libheap`, the freeing userspace heap

First userspace-runtime slice built. `userspace/libheap` replaces init's fixed bump arena
(`init/src/heap.rs`, a 64 KiB static that never freed) with a real freeing `#[global_allocator]`.

**Design (matches `docs/architecture/libheap.md`).** A segregated size-class allocator over
multiple discontiguous **arenas**, each a mapped `MemoryObject` — the SLUB-over-buddy split
re-expressed for userspace. Small requests (≤ 2048 B) round up to a size class (16/32/…/2048)
and are carved from arenas; freed slots return to a per-class LIFO freelist (no coalescing —
the jemalloc/tcmalloc/SLUB family, not dlmalloc's coalescing arena). Large requests get a
dedicated mapping, unmapped **and the object closed** on free (real reclamation). `GlobalAlloc`
hands the `Layout` back to `dealloc`, so the small path needs no per-slot size header; the
large path stores a small header just below the returned pointer to recover the mapping.

**The engine/registration split (std-port seam).** `HeapEngine<S: ArenaSource>` is the reusable
allocator; `Heap` is the thin `GlobalAlloc` newtype forwarding to a process-global engine. A
future std port's `std::sys::alloc` forwards to the same engine rather than fighting for the
single `#[global_allocator]` slot. `ArenaSource` (the map/unmap provider) is the seam that also
makes the engine host-testable: the target `SyscallSource` (create+map a `MemoryObject`) is
`cfg(not(test))`; under `cargo test` a `std::alloc`-backed source runs the same logic with no
kernel (9 tests). Arenas are 64 KiB (modest because `MemoryObject` frames are **eager** —
allocated+zeroed up front); a real CAS spinlock guards the shared state (uncontended today,
but correct for future std OS-threads); `panic = "abort"`, OOM → null (no panic), honoring
init's critical-path rules.

**Consumer.** init is the sole heap consumer and now runs on libheap (its `heap.rs` deleted;
the bump-math tests moved into libheap's suite). **eshell needs no allocator** — it is `no_std`
*without* `alloc` (fixed buffers), so the plan's "eshell follows" was moot; nothing to migrate.

**Verified.** 9 libheap host tests; full host suite (libkern/libheap/init/librsproto/
fs-server-ext4) + `check-arch` green; bare build clean (no warnings). QEMU: init's
allocation-heavy bootstrap (init.toml parse → `Vec<MountSpec>` + TOML strings) runs on libheap,
mounts ext4, drives the full parent demo chain, reaps, and reaches a live `eshell>`; scripted
`help`/`lsblk`/`mounts`/`cat /system/current-generation` all correct; `-smp 4` boots clean
(4 CPUs online, no faults). Not committed pending review.

## 2026-07-13 — Phase 3 slice 5 scoped down: libos core only (defer libstream + the multi-task executor)

Slice-5 kickoff. A survey of current userspace (init/eshell/parent/fs-server all call raw
`libkern`; the `po_wait` submit→`sys_wait`→decode→close idiom is copy-pasted into every binary;
`Handle<T,M>` has an authoritative paper design in `os-design-v5.1.md`; eshell/parent/fs-server
are all `alloc`-free) drove two scope cuts, both the same "build what has a consumer" discipline
applied to librt/libstream/std.

**1. Cut `libstream` from slice 5.** It's typed *pipeline* I/O (the `TSM1` wire format,
`TableWriter`/`TableReader`, `#[derive(TypedRecord)]`) and has **no consumer now**: init does
sequential bootstrap (no typed records), and eshell's line editor is *byte* I/O (raw console
bytes + text), not typed streams — the plan's "eshell becomes a libstream client" conflated the
two. libstream's first real consumer is the shell/pipeline era or the service-mgr milestone
("a test program produces typed TableWriter output to its log channel"). It also drags in a
`#[derive(TypedRecord)]` proc-macro (first userspace external-crate decision, or a hand-rolled
one). Deferred to a just-in-time slice with its first consumer — and flagged for a **dedicated
design pass on the `TSM1` wire protocol + streaming model** before implementation.

**2. Scope libos to an alloc-free core + `block_on`; defer the multi-task executor.** Neither
current consumer needs multi-task concurrency: init is sequential (mount→mount→reaping loop),
eshell is a single read loop. Both need only `block_on` (drive one future). A multi-task
`spawn`/run-loop executor needs `alloc` (heterogeneous task storage) *and* has no concurrent
consumer (today's fs-server is also sequential) — it lands with the first concurrency-heavy
service. So slice 5 builds: `Handle<T,M>` typestate wrappers (from the v5.1 design — `T` object
marker, `M` mode marker, `extra: Rights` runtime band, sealed `CanRead`/`CanWrite` op gating,
attenuation-consumes-self via `sys_handle_restrict`); the `Op` future over `sys_wait`;
single-op async methods; `block_on`; an `io::Error`-shaped error. All **`#![no_std]`, no
`alloc`** — the reach that matters: the alloc-free binaries (eshell/parent/fs-server) can adopt
`Handle` + `block_on` too, not just init. The multi-task executor, when built, is an
`alloc`-gated addition, not a core change. `Op` is a real `core::future::Future` (so
`async`/`await` and a later executor drive it); `block_on` is the degenerate single-task driver.

Design captured in `docs/architecture/libos.md` (slice-5 Part A). Plan reframed: slice 5 is
"libos core," libstream is a separate deferred entry. Next: implement libos (Part B: the `Op`
future + `block_on` + error over a mock syscall seam; Part C: the `Handle<T,M>` wrappers;
Part D: dogfood init + eshell). Planning + design doc only in this entry — no code yet.

## 2026-07-13 — Phase 3 slice 5 complete: libos core (+ the recovery-shell dependency line)

libos core landed in four commits (Parts A–D). B: the `Op` future + `block_on` (single-task
driver, alloc-free) + `io::Error`-shaped error, over a mock syscall seam (6 tests). C: the
`Handle<T,M>` typestate wrappers — sealed `CanRead`/`CanWrite`/… op-gating (misuse is a compile
error), RAII close, attenuation-consumes-self; async `Namespace::lookup`/`Resource::read,write`,
`Memory::create/map`, `Notify::recv` (+7 tests). D: dogfood + a borrowed (non-owning) `Handle`.
`#![no_std]`, no `alloc` throughout; 15 host tests; bare-build clean.

**The dogfood surfaced a governance question worth recording: should the recovery shell take a
runtime-library dependency?** Both init and eshell `CLAUDE.md` had *forbidden* `libos` — a rule
predating the alloc-free libos-core. Resolved by looking at Linux precedent, and the two land
differently:

- **init → uses libos** (and, later, full `std`). PID 1 on Linux *universally* links a full
  libc — systemd pulls in glibc + dozens of shared libraries; even minimal inits (SysV,
  BusyBox, runit, s6) link libc. Nobody writes PID 1 against raw syscalls. So init is not meant
  to stay minimal: its `CLAUDE.md` now permits libkern/libheap/libos-core (the line it draws is
  *stateful runtime* + *unstarted services*, e.g. libstream/librsproto, not the syscall
  surface), with full `std` as the trajectory. init's `read_current_generation` is the first
  consumer (`ns.lookup(...).block_on()` + `map()`).

- **eshell → stays `libkern`-only, deliberately.** It's the *recovery surface* (the shell init
  drops to on failure), so it follows the statically-linked-`busybox` / `sash` ethos: a rescue
  tool minimizes the layers between itself and the syscall so there are the fewest ways for it
  to fail to come up (a dynamically-linked shell won't even start if libc is broken). eshell
  recovers from *init* failure — the kernel/syscalls are fine — so libos-core (stateless,
  alloc-free, no bootstrap) would be a *defensible* exception, but we keep eshell at the raw
  surface on purpose. Its drafted libos migration was reverted; `CLAUDE.md` now states the *why*.

The distinction: libos-core is safe for critical-path use because it has **no runtime
bootstrap** (stack-only handles/futures — nothing to initialize or corrupt); the recovery-shell
restriction is a *minimize-surface* choice, not a correctness one.

Verified: full host suite + check-arch green; QEMU — init reads current-generation through
libos ("nitrox-rootfs generation 1"), boots to `eshell`, scripted `help`/`lsblk`/`mounts`/`cat`
correct; `-smp 1` + `-smp 4` clean, no faults. Slice 5 done; next is slice 6 (SysCaps).

## 2026-07-14 — Phase 3 slice 6 Part A: SysCaps design doc

Wrote `docs/architecture/syscaps.md`. SysCaps are the second axis of authority — ambient
*per-process* capabilities (a `SysCaps(u64)` field on `Process`), complementing per-handle
`Rights` (per-object). The 6-cap set is v5.1's (`LOAD_MODULE`/`BIND_NAMESPACE`/`PHYSICAL_MEMORY`/
`REAL_TIME`/`SYSTEM_CLOCK`/`AUDIT_CONTROL`); granted at spawn with `child = parent & args.syscaps`
(⊆-parent, no amplification), immutable after spawn, init boots the full set. Enforced by a
one-line `require_syscap(cap)` right after `current_process()` in a handler; missing cap →
`NoAccess`. Type lives in `libkern` (kernel + userspace mirror), like `Rights`.

Three decisions worth recording:

1. **Define all 6, wire only 2 gates now.** All six are defined and flow through inheritance,
   but only `BIND_NAMESPACE` (real consumer: `sys_ns_bind`) and `REAL_TIME` (the committed
   slice-2 deferral: the RT scheduling class) get an actual gate this slice. `LOAD_MODULE`/
   `PHYSICAL_MEMORY`/`SYSTEM_CLOCK`/`AUDIT_CONTROL` have no operation to gate yet (no loader/
   phys-map/clock-offset/audit) — defined and inherited, their `require_syscap` added by the
   slice that builds each operation. Same "wire what has a consumer" discipline as the rest of
   Phase 3.

2. **`BIND_NAMESPACE` is an *additional* gate on `ns_bind`, atop the existing `BIND` handle
   right — making namespace *construction* supervisor-only.** A process without it can't
   `ns_bind` even into a namespace it created itself (sandboxes receive namespaces; they don't
   build them — the strict v5.1 reading). Cost: the `parent` demo (`ns_create`+`ns_bind`) now
   needs init to grant it `BIND_NAMESPACE`.

3. **Corrections vs the plan stub.** Affinity **stays a handle right** (`SIGNAL` on the Thread
   handle) — not a syscap; the survey + decision-log:5871 confirm it. Only the RT *class* is
   `REAL_TIME`-gated; `nice`/affinity-at-creation are ungated (not privileged). And the ABI:
   `SpawnArgs` grows `syscaps: u64` (96→104); `ThreadArgs` uses its existing `_reserved[40]`
   for class/nice/affinity (size stays 64). These are *syscall*-ABI (self-pinned by asserts +
   spec docs), not module-boundary types — so the source comments calling them "ABI-hash
   inputs" get corrected, not added to `abi-version-hash.md`.

Committed as Part A (design doc only). Next: Part B — the `SysCaps` type + `Process` field +
inheritance + boot grant; Part C — wire the two gates + the `ThreadArgs`/`SpawnArgs` ABI +
dogfood the demos.

## 2026-07-14 — Phase 3 slice 6 complete: SysCaps enforced

SysCaps landed in three commits (Parts A–C). The kernel now has its second, defining axis of
authority — ambient per-process capabilities — enforced at the syscall boundary.

**Part B (plumbing, behavior-neutral):** `SysCaps(u64)` hand-rolled bitmask (kernel
`libkern/syscaps.rs` + userspace mirror, like `Rights`); immutable `Process.syscaps`; `SpawnArgs`
grown 96→104 with a `syscaps: u64`; `sys_process_spawn` attenuates `child = parent &
args.syscaps`; init's boot grant is `SysCaps::all()`. No gate enforced yet, so the system booted
identically — verified.

**Part C (enforcement):** `require_syscap(cap)` (resolve `current_process` → `contains` →
`NoAccess`). Two gates wired:
- **`BIND_NAMESPACE` on `sys_ns_bind`** — an *additional* gate atop the existing `BIND` handle
  right, so namespace *construction* is supervisor-only: a process cannot bind even into a
  namespace it created without the cap. Proven both ways under QEMU: parent *with* the grant →
  `ns_bind /store ok`; parent *without* → `ns_create ok` (creating the object is ungated) but
  `ns_bind FAIL`.
- **`REAL_TIME` on the RT scheduling class** — closes the slice-2 deferral. `ThreadArgs`'
  reserved block became `class`/`rt_priority`/`nice`/`cpu_affinity` (size unchanged, 64; a
  zeroed block = the historical TimeShared/nice-0/no-affinity default). `sys_thread_create`
  parses them, gates `RealTime` on `REAL_TIME` (`NoAccess`), and applies class/nice/affinity via
  a new `spawn_user_sched` (set before enqueue, mirroring `spawn_inner`). `nice`/affinity are
  ungated (renicing/pinning your own thread isn't privileged); affinity-at-runtime stays the
  `SIGNAL` handle-right gate. No userspace requests RT today — the gate is defensive but the
  mechanism is now reachable.

Grants: init (full set) grants `parent` `BIND_NAMESPACE` (its ns-demo constructs namespaces);
fs-server/eshell/child get none.

**ABI reconciliation.** `SpawnArgs`/`ThreadArgs` are *syscall*-ABI (passed by `UserPtr`,
self-pinned by `size_of`/`offset_of` asserts + the spec docs), **not** Tier-2 module-boundary
types — so growing them is a pre-v1 syscall-ABI change, not a module-ABI-hash bump. The source
comments that called them "ABI-hash inputs like IpcMsg" were corrected to say so; they are not
added to `abi-version-hash.md`.

**Deferred (defined, not gated):** `LOAD_MODULE`, `PHYSICAL_MEMORY`, `SYSTEM_CLOCK`,
`AUDIT_CONTROL` — no operation to gate yet; each gets its `require_syscap` when its slice builds
the operation. This doc's cap table is the registry.

Verified: SysCaps + layout host tests; full host suite (528 kernel) + check-arch green; bare
build clean; QEMU gate-allows + gate-bites as above; boots to `eshell`; `-smp 1` + `-smp 4`
clean, no faults. Docs updated (syscaps.md → implemented; scheduler.md's REAL_TIME gate now
wired; process-spawn-args.md/thread-args.md specs grown). Slice 6 done; next is slice 7 (the
SysCaps-coupled libos surface). Then the PR.
