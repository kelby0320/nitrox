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
