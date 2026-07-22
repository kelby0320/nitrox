# CLAUDE.md

Project-level instructions for Claude Code working on Nitrox.

## What this project is

Nitrox is a hobby operating system written in Rust. Successor to Latte (kelby0320/latte, an earlier Unix-like OS in C). Targets x86_64 UEFI primarily; aarch64 designed in via the architecture abstraction layer but not yet implemented. The system architecture rejects POSIX, Unix signals, ambient authority, and synchronous syscalls; it preserves Unix's composable pipelines, everything-as-a-resource philosophy, and powerful shell environment, on a foundation of capability-based access control plus per-process namespaces.

For the full architecture: read `docs/architecture/overview.md` first. For specific decisions and their rationale: `docs/rationale/`. For exact contracts (ABIs, formats): `docs/spec/`.

## Core architectural rules

These shape every decision; deviation requires explicit discussion:

- **Capability-based, not identity-based.** Authority is held in handles, not derived from a UID/GID. There is no "user identity" at the kernel level.
- **Per-process namespaces.** Different processes see different namespace contents. Sandboxing is by namespace construction, not by permission denial.
- **Async-first syscalls.** Every potentially-blocking operation returns a `PendingOperation` handle. The thread blocks on `sys_wait`, never inside another syscall.
- **No signals.** Async events are delivered via the notification queue. See `docs/rationale/why-no-signals.md`.
- **Resource servers don't self-register.** A supervisor (init, service-mgr, session-mgr) holding `BIND_NAMESPACE` does the registration. See `docs/rationale/why-supervisor-registration.md`.
- **Filesystems are userspace processes.** No filesystem code in the kernel.

## Language and toolchain rules

- **Rust throughout.** Kernel, userspace services, and runtime libraries.
- **No nightly language or library features.** No `#![feature(...)]` anywhere in
  `kernel/` or `userspace/` — enforced by `cargo xtask check-nightly` in CI. The
  `Handle<T, M>` design uses typestate markers rather than const-generic bitflags
  specifically to honour this.
- **Toolchain: stable, with one narrow exception.** The kernel and tools build on
  **stable** against the built-in `x86_64-unknown-none`. **Userspace** pins a nightly
  (`userspace/rust-toolchain.toml`) for one reason: it targets
  `x86_64-unknown-nitrox`, a custom spec, because hardware floating point needs a
  hard-float ABI and stable rustc ships no freestanding x86_64 target that has one. A
  custom spec has no precompiled sysroot, so `core`/`alloc` are rebuilt with
  `-Z build-std`, which is nightly-only. The pin is exact, not floating. This buys a
  *target*, not a licence — see `docs/history/decision-log.md` (2026-07-21
  floating-point).
- **Assembly is emitted from Rust**, not NASM: `core::arch::asm!`, `global_asm!`, and `#[unsafe(naked)]` + `naked_asm!` (all stable since Rust 1.88). The exception entry stubs, the GDT/TSS load, the user-memory copy routines, and the thread context switch are all in-tree Rust asm. There is no assembler in the build — `build.rs` only passes the linker script. (Earlier drafts reserved NASM for the entry stub and context switch; both turned out cleaner as Rust-emitted asm — see `docs/history/decision-log.md` 2026-05-13 and 2026-05-29 — so NASM is not used. Re-evaluate only if a routine genuinely cannot be expressed via `asm!`/`naked_asm!`.)
- **Cargo + cargo xtask** for builds. The `xtask` workspace provides higher-level commands (`xtask qemu`, `xtask image`, etc.).
- **Limine** as the bootloader.

## Build commands

Standard development loop:

```
cargo xtask build          # build the kernel ELF
cargo xtask image          # build + assemble the UEFI-bootable disk image
cargo xtask qemu           # build, assemble the image, and launch under QEMU
cargo xtask qemu --selftest # …with the boot self-tests / demos compiled in
cargo xtask qemu-debug     # launch QEMU with the GDB stub enabled
cargo xtask test           # host-side unit tests
cargo xtask test-qemu      # boot a headless self-test image; pass/fail via isa-debug-exit
```

`cargo xtask test-qemu` boots the self-test build (`test-harness` feature)
headless and adjudicates the whole boot (kernel → init → mount → userspace demos)
from QEMU's exit code: the guest writes a verdict to the `isa-debug-exit` device
(init on success, the kernel panic handler on failure), a hang is caught by a
wall-clock timeout. See `docs/conventions/qemu-integration-tests.md`.

Don't run kernel code on the host. Don't run `cargo build` directly in the kernel workspace without the custom target — it will fail.

## Repository layout

```
kernel/         no_std kernel; custom target x86_64-unknown-none
userspace/      userspace services and libraries; std target
tools/          host-native build utilities (xtask, image builder)
docs/           project documentation (see structure below)
```

Documentation structure under `docs/`:

```
docs/
  architecture/  what the subsystems do and how they relate
  rationale/     why decisions were made (read here when puzzled)
  spec/          exact contracts (ABIs, wire formats, schemas)
  reference/     catalogues (kernel objects, syscalls, errors, syscaps)
  conventions/   how to write code in this project
  history/       v5.1 design doc, decision log
```

When uncertain why something is the way it is, check `docs/rationale/rejected-approaches.md` first — many "obvious" alternatives were considered and rejected for specific reasons.

## Subdirectory rules

Per-subdirectory `CLAUDE.md` files exist for the major workspaces. Read the relevant one before significant work:

- `kernel/CLAUDE.md` — `#![no_std]`, no external crates, unsafe policy
- `userspace/CLAUDE.md` — crate layering, async-first
- `userspace/libkern/CLAUDE.md` — `#![no_std]` + no alloc; raw syscall surface
- `userspace/init/CLAUDE.md` — critical-path code, special constraints

When working in a subdirectory, Claude Code lazily loads the subdirectory's `CLAUDE.md`. Trust those files over this one for subdirectory-specific guidance.

## Cross-cutting conventions

- **Markdown for documentation.** No Sphinx, no MkDocs. Plain `.md` files with Mermaid for diagrams where helpful. Cross-link via relative paths.
- **TOML for configuration.** `init.toml`, service declarations, profile manifests. No YAML, no JSON5.
- **All public items have doc comments.** Use `cargo doc` for code-level reference.
- **`#[repr(C)]` for any type crossing the kernel/userspace boundary.** Layout must be predictable.
- **Document `unsafe` blocks.** Every `unsafe` block needs a `// SAFETY:` comment explaining why the operation is sound.

## Forbidden patterns

Things that should not appear in code, period:

- External crates in the kernel (one planned exception: ACPICA in Phase 2; not yet active)
- Nightly Rust features
- `unsafe` blocks without `SAFETY` comments
- Sync syscalls that block (the `read()`/`write()` Unix-style pattern)
- Code that assumes a UID/GID model
- Direct `panic!()` in init or eshell — these are critical-path
- Adding "for now" code without a TODO and a tracking entry
- Referencing architecture internals (`arch::x86_64::*`, future
  `arch::aarch64::*`) from kernel code outside `kernel/src/arch/` — go
  through the neutral `crate::arch` interface. Enforced by a private arch
  submodule and `cargo xtask check-arch`. See
  `docs/conventions/arch-boundary.md`.

If you find yourself writing one of these, stop and ask.

## When to update which doc

- **Implementation produces new conventions** → `docs/conventions/`
- **Implementation reveals a subtlety in an architecture doc** → update the architecture doc; the docs are living
- **A new design decision is made** → append to `docs/history/decision-log.md` with date and reasoning
- **A deferred item is being implemented** → update `docs/rationale/deferred-decisions.md`
- **A spec contract changes** → update the spec doc; bump version markers as needed

## Status

The project is pre-v0.1. The syscall ABI, wire formats, and kernel internals are pre-stabilization. The `docs/spec/` documents are the canonical contracts within this pre-stabilization period; if a spec doc and the source disagree, the source wins and the spec is updated to match (filed against the decision log).

Phases 0–3 (foundation, kernel substrate, boot-to-userspace, service ecosystem) are **complete** (Phase 3 closed 2026-07-21). Phase 4 (toward a usable windowed desktop) is next. See `docs/history/decision-log.md` for the current implementation phase and `docs/planning/implementation-plan.md` for the slice-by-slice breakdown.
