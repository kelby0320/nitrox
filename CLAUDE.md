# CLAUDE.md

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

- **Rust throughout**, built with Cargo and `cargo xtask`, booted by **Limine**.
- **No nightly language or library features**: no `#![feature(...)]` in `kernel/` or `userspace/`,
  held by `cargo xtask check-nightly`.
- **Stable, with one narrow exception.** The kernel and tools build on an exact stable, pinned
  in `rust-toolchain.toml`, for the built-in `x86_64-unknown-none`. Userspace pins an exact nightly
  only to `-Z build-std` its hard-float custom target, `x86_64-unknown-nitrox`: a target, not a
  licence (decision log, 2026-07-21).
- **Assembly is emitted from Rust**, not NASM: `core::arch::asm!`, `global_asm!`, and `#[unsafe(naked)]` + `naked_asm!` (all stable since Rust 1.88). The exception entry stubs, the GDT/TSS load, the user-memory copy routines, and the thread context switch are all in-tree Rust asm. There is no assembler in the build — `build.rs` only passes the linker script. (Earlier drafts reserved NASM for the entry stub and context switch; both turned out cleaner as Rust-emitted asm — see `docs/decision-log.md` 2026-05-13 and 2026-05-29 — so NASM is not used. Re-evaluate only if a routine genuinely cannot be expressed via `asm!`/`naked_asm!`.)

## Build commands

```
cargo xtask build          # build the kernel ELF
cargo xtask image          # build + assemble the UEFI-bootable disk image
cargo xtask qemu           # build, assemble the image, and launch under QEMU
cargo xtask qemu --grab    # …with the window holding the pointer and keyboard
cargo xtask qemu --selftest # …with the boot self-tests / demos compiled in
cargo xtask qemu-debug     # launch QEMU with the GDB stub enabled
cargo xtask test           # host-side unit tests
cargo xtask test-qemu      # boot a headless self-test image; pass/fail via isa-debug-exit
cargo xtask test-interactive # boot the RELEASE image and drive a real login + shell
cargo xtask preview        # render the toolkit on the host to a PNG — no boot, no QEMU
cargo xtask tune           # …and the compositing preview: shadows and the overview's opacity
cargo xtask shot           # boot the release image and photograph the whole desktop
cargo xtask check-display  # boot + screendump; compare the screen to a libdraw render
cargo xtask check-terminal # click into nxterm, type, and check the shell's answer renders
cargo xtask check-input    # inject a key + a click over QMP; check they reach a window
cargo xtask check-input --usb # …on a machine with no i8042: a USB keyboard and mouse only
cargo xtask check-images   # test vs release initramfs and root: differ only on a short allow-list
cargo xtask check-login    # boot the RELEASE image and drive the graphical greeter to a session
cargo xtask check-login --usb # …typed and clicked on USB, with no i8042
cargo xtask check-logout   # the power menu: log out past the editor's question; restart; shut down
cargo xtask check-fbcon    # boot with NO serial port; read the boot and a panic off the screen
cargo xtask image --live   # the live image: release root as a RAM-disk module, for a USB stick
cargo xtask check-live     # boot the live image as a USB stick with no disk; mount, greeter, a write
cargo xtask check-report   # choose the live menu's hardware report, no serial port; read its pages
cargo xtask check-report --usb # …its pages turned on a USB keyboard, with no i8042
cargo xtask check-install  # install to a blank disk from the live menu, then boot that disk
cargo xtask check-recovery # reset a password on an installed disk from the live image, then boot it
cargo xtask image --live --selftest # the test live image: the live stick with the test packages
cargo xtask check-storage  # that stick beside a copy of the release disk; the host checks the disk
cargo xtask check-media    # the live desktop: a stick plugged in, shown in Files, saved to, ejected
cargo xtask check-shutdown # `with power shutdown` on a test disk; the host checks it; then a reboot
cargo xtask check-resolutions # four display gates at five screen sizes — on demand, not in CI
cargo xtask throughput-stick # the stick Part H's copies are timed with, and the scripts that time them
cargo xtask throughput-stick --check PATH # read it back: an image, or the stick where it is mounted
```

**Use `--grab` to touch the mouse or press a chord**, and read a gate's paragraph before changing
what it boots or matches on: *Running QEMU by hand* and *The gates, one by one*, in
[`qemu-integration-tests.md`](docs/conventions/qemu-integration-tests.md).

Don't run kernel code on the host. Don't run `cargo build` directly in the kernel workspace without the custom target — it will fail.

## Review workflow

Every PR is reviewed by agents that share none of the author's conversation. **`/pr-review <N>`**,
which only the maintainer starts, runs `.claude/workflows/pr-review-workflow.js` given the PR
number alone: a scope, reviewers with one lens each — Opus for logic, error paths and
architecture, Sonnet for the rest — a QEMU boot for findings that need one, and a report posted to
the PR: at most eight agents, in worktrees of their own. While it runs the author changes nothing
on the branch and runs no `cargo` or QEMU. **`/pr-review-solo`** is the same review by one agent.
The author acts on a review with **`/address-review`**: reproduce, fix with a control, sweep the
class, and record the outcome in `.claude/review-outcomes.tsv`. **`/bug-bash <subsystem>`**, also
the maintainer's, aims the same machinery at one subsystem of `main` and returns its findings for
`docs/planning/bug-bash.md`. Their rules are in `.claude/skills/`; CI tests each workflow's control
flow with its agents stubbed (`node .claude/workflows/<name>.test.mjs`).

## Who does the work

The session the maintainer talks to runs on Opus and stays the author: it designs, decides and
commits. **It hands bounded work to the `implementer` subagent** (`.claude/agents/implementer.md`,
Sonnet): `clippy` warnings, comment cleanup, code moved between files, docs updates, and small fixes
whose spec and test are clear. **It keeps** design and detail passes, the kernel's `unsafe`, memory
and concurrency code, filesystem write paths, drivers, syscall and ABI changes, debugging, and
review findings. A brief names the files, the change, the rules that apply, the tests to run and
what done means. The author reads every delegated diff and runs its tests and gates before
committing, and a delegated commit says so: `Implemented-By: implementer (Sonnet)`.

## Repository layout

```
kernel/            no_std kernel; target x86_64-unknown-none
userspace/         services and libraries; target x86_64-unknown-nitrox
tools/             host-native build utilities (xtask, image builder)
docs/
  architecture/    subsystems that EXIST — what they do and how they relate
  design/          subsystems DESIGNED BUT NOT BUILT
  spec/            exact contracts (ABIs, wire formats, schemas, the shell language)
  reference/       catalogues (today: error codes only — see deferred-decisions.md)
  rationale/       why decisions were made (read here when puzzled)
  conventions/     how to write code in this project
  planning/        phase and subproject plans, with checkboxes
  archive/         superseded artifacts, kept for the record (the v5.1 design doc)
  decision-log.md  the running record of decisions and their reasoning
```

**Which describe the system as it is today** — reading the wrong class as current is how you end
up confidently wrong:

- **`spec/`, `reference/`, `architecture/`, `conventions/` describe current behaviour.** When one
  disagrees with the source, the source wins and the doc is a bug: fix it in the same change.
- **`rationale/` explains why. `design/`, `planning/` and `archive/` do not describe current
  behaviour**: never conclude "the system does X" from them.
- **`decision-log.md` is append-only**, each entry true as of its date. **Every `architecture/`
  doc has a Status line**: trust it over the body's tense. The rest of the rules, and what
  `cargo xtask check-docs` holds: [`documentation.md`](docs/conventions/documentation.md).

When uncertain why something is the way it is, check `docs/rationale/rejected-approaches.md` first — many "obvious" alternatives were considered and rejected for specific reasons.

## Subdirectory rules

Each loads when Claude Code reads a file under its directory, and wins there over this one. Read
the relevant one before significant work. `cargo xtask check-docs` holds this list complete.

- `kernel/CLAUDE.md` — `#![no_std]`, no external crates, the `unsafe` policy
- `tools/CLAUDE.md` — host tooling: which rules do not reach it, and the bar for a dependency
- `userspace/CLAUDE.md` — the target, crate layering, external dependencies, async-first
- `userspace/auth-service/CLAUDE.md` — credentials: no `alloc`, the user database's format
- `userspace/desktop-session-mgr/CLAUDE.md` — the greeter's supervisor: never keep a password
- `userspace/eshell/CLAUDE.md` — the emergency shell: `libkern` only, no `alloc`, no panics
- `userspace/fs-server-ext4/CLAUDE.md` — the ext4 server: parser, write path, extent tree
- `userspace/fs-server-fat/CLAUDE.md` — the FAT server: FAT12, 16 and 32, read-write, long names
- `userspace/init/CLAUDE.md` — PID 1: critical-path code, special constraints
- `userspace/libcrypto/CLAUDE.md` — test vectors, constant-time comparison, no secrets in source
- `userspace/libfsserver/CLAUDE.md` — the filesystem servers' library: blocks, volumes, cache
- `userspace/libheap/CLAUDE.md` — the userspace heap and its two hard invariants
- `userspace/libkern/CLAUDE.md` — the raw syscall layer: `#![no_std]`, no `alloc`
- `userspace/nxsh/CLAUDE.md` — the shell and its language, and what bit before
- `userspace/service-mgr/CLAUDE.md` — never block on anything that can wait on `service-mgr`
- `userspace/session-mgr/CLAUDE.md` — the session loop and the supervisor discipline

## Cross-cutting conventions

- **Markdown for documentation.** No Sphinx, no MkDocs. Plain `.md` files with Mermaid for diagrams where helpful. Cross-link via relative paths. <!-- check-docs: allow-missing -->
- **`cargo fmt`** in each workspace you change, with the root `rustfmt.toml`; CI checks it.
- **TOML for configuration.** `init.toml`, service declarations, profile manifests. No YAML, no JSON5.
- **All public items have doc comments.** Use `cargo doc` for code-level reference.
- **`#[repr(C)]` for any type crossing the kernel/userspace boundary.** Layout must be predictable.
- **Document `unsafe` blocks.** Every `unsafe` block needs a `// SAFETY:` comment explaining why
  the operation is sound. CI runs `clippy` with `undocumented_unsafe_blocks` denied.
- **Comments say what the code cannot** — a non-obvious why, an invariant, a `SAFETY` argument —
  and cite no PR, review, phase or part. **A Rust file stays at 1,000 lines or fewer.** CI checks
  both: [`comments-and-size.md`](docs/conventions/comments-and-size.md).

## Forbidden patterns

Things that should not appear in code, period:

- External crates in the kernel (one planned exception: ACPICA in Phase 2; not yet active)
- Nightly Rust features
- `unsafe` blocks without `SAFETY` comments
- Sync syscalls that block (the `read()`/`write()` Unix-style pattern)
- Code that assumes a UID/GID model
- Direct `panic!()` in init or eshell — these are critical-path
- Adding "for now" code without a TODO and a tracking entry
- Referencing `arch::x86_64::*` (or a future `arch::aarch64::*`) from kernel code outside
  `kernel/src/arch/` — go through `crate::arch`. Held by `cargo xtask check-arch`; see
  `docs/conventions/arch-boundary.md`.

If you find yourself writing one of these, stop and ask.

## When to update which doc

- **Implementation produces new conventions** → `docs/conventions/`
- **Implementation reveals a subtlety in an architecture doc** → update the architecture doc; the docs are living
- **A new design decision is made** → append to `docs/decision-log.md` with date and reasoning
- **A deferred item is being implemented** → update `docs/rationale/deferred-decisions.md`
- **A spec contract changes** → update the spec doc; bump version markers as needed

## Status

The project is pre-v0.1. The syscall ABI, wire formats, and kernel internals are pre-stabilization. The `docs/spec/` documents are the canonical contracts within this pre-stabilization period; if a spec doc and the source disagree, the source wins and the spec is updated to match (filed against the decision log).

Phases 0–6 are **complete**, the last (USB) on 2026-10-13. Now comes the work planned in
`docs/planning/pre-phase-7.md` — the review harness, hygiene, a bug bash, the syscall surface —
then 7–9: the portable runtime, networking, the browser. Phases are **not renumbered**. The
current phase is in `docs/decision-log.md`, the slices in `docs/planning/implementation-plan.md`.
