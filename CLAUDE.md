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
  *target*, not a licence — see `docs/decision-log.md` (2026-07-21
  floating-point).
- **Assembly is emitted from Rust**, not NASM: `core::arch::asm!`, `global_asm!`, and `#[unsafe(naked)]` + `naked_asm!` (all stable since Rust 1.88). The exception entry stubs, the GDT/TSS load, the user-memory copy routines, and the thread context switch are all in-tree Rust asm. There is no assembler in the build — `build.rs` only passes the linker script. (Earlier drafts reserved NASM for the entry stub and context switch; both turned out cleaner as Rust-emitted asm — see `docs/decision-log.md` 2026-05-13 and 2026-05-29 — so NASM is not used. Re-evaluate only if a routine genuinely cannot be expressed via `asm!`/`naked_asm!`.)
- **Cargo + cargo xtask** for builds. The `xtask` workspace provides higher-level commands (`xtask qemu`, `xtask image`, etc.).
- **Limine** as the bootloader.

## Build commands

Standard development loop:

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
cargo xtask check-display  # boot + screendump; compare the screen to a libdraw render
cargo xtask check-terminal # click into nxterm, type, and check the shell's answer renders
cargo xtask check-input    # inject a key + a click over QMP; check they reach a window
cargo xtask check-images   # test vs release initramfs: differ only on a short allow-list
cargo xtask check-login    # boot the RELEASE image and drive the graphical greeter to a session
```

**Use `--grab` whenever you are going to touch the mouse or press a chord.** The guest has a
**relative** pointing device and no absolute one — a PS/2 mouse reports movement, and there is no
USB or virtio input driver for a tablet device to talk to — so nothing ever tells it where the
host's pointer is. Ungrabbed, the guest's cursor and yours are two independent cursors whose
offset is permanent and *cannot* be corrected by pushing into a corner: your pointer leaves the
window, and stops producing motion, before the guest's cursor reaches the edge. Every chord is
`Super`-something, and GNOME, KDE and COSMIC all bind `Super` at the host compositor, so those
keystrokes are your desktop's rather than the guest's. `--grab` confines the pointer and takes
the keyboard (Ctrl-Alt-G releases it); on a Wayland session it also runs the window through
XWayland, because a grab is an X operation. `Super` alone is deliberately unbound in the guest —
the chords are `Super+H`, `Super+1..4`, `Super+Shift+1..4`, `Super+R`.

When a chord seems dead, the debug console says which half is at fault: the compositor logs
`Super down` / `Super up` per transition (the modifier only — never the key beside it, which at a
password prompt would be the password). No line means the keystroke never left your desktop.

`cargo xtask test-qemu` boots the self-test build (`test-harness` feature)
headless and adjudicates the whole boot (kernel → init → mount → userspace demos)
from QEMU's exit code: the guest writes a verdict to the `isa-debug-exit` device
(init on success, the kernel panic handler on failure), a hang is caught by a
wall-clock timeout. See `docs/conventions/qemu-integration-tests.md`.

`cargo xtask test-interactive` is the one gate that boots the **release image**. It types at
the real prompt over the serial console and matches on what comes back — 78 expectations across
25 steps, expect-driven rather than sleep-driven.

**Why it exists, in the past tense since 2026-08-21.** `session-mgr` used to auto-log-in and run
a fixed script under `test-harness`, so the `login:` prompt, the typed password, the real shell
prompt and the whole `tty_*` layer were `#[cfg(not(feature = "test-harness"))]` code that CI
compiled and never ran. Retrofit Part B deleted that: `session-mgr` has **one** `login()` in
every build and zero test cfgs, and its login proof lives here as steps 5a–5c.

**Prefer this shape for anything user-facing**: a service should behave in a test image the way
it behaves in a release one. `docs/planning/test-path-retrofit.md` is the plan that made that
true — `session-mgr` went from 31 build-mode `cfg` sites to zero and `init` from 41 to one — and
it is complete. The one left is `init`'s `/subtreetest` binding, which needs a **bind-mount
concept in `init.toml`** and is deferred past that plan as capability work; the box naming it is
still open there.

`cargo xtask check-images` is what keeps the property: it fails if a test image and a release
image start differing in anything new.

`cargo xtask check-terminal` is the **compositor-to-shell round trip** — a click that raises
`nxterm`, keys travelling to `nxsh` and echoing back into the grid, and the shell's answer
rendered there. It runs unconditionally in CI's QEMU job (promoted 2026-08-18); `check-input`
stops at the test client's event log and `check-display` never types, so nothing else covers it.

`cargo xtask check-login` is the **graphical login gate**, and the second of the two that boot a
release image. It drives the greeter with the PS/2 injection `check-input` and `check-terminal`
use — a wrong password, then a right one, then a session — and it is the only gate where the
display arm exists for a person rather than for a test: everything else display-side boots
`--selftest`. It runs unconditionally in CI's QEMU job. Landed with M7 Part D, deliberately
*before* the shell it will eventually show, so Parts E and F land against a gate that exists.

**It must boot the release image**, not the test one. In a `--selftest` boot the greeter is
bottom-most — `service-mgr` brings the login chain up before declared services, which is what
keeps `check-display`'s reference windows undisturbed — so it holds no keyboard and nothing
typed reaches it.

`cargo xtask preview` writes `tools/build-cache/preview-{ui,term}.png` — the same renders
`check-display` compares the guest against, drawn here and made viewable. **It exists so that a
judgement about how something looks costs a glance rather than a boot** (M11 Part A), which is
what makes a polish loop affordable at all. It shows the *toolkit's* surfaces only: anything the
compositor draws — the cursor, the drag outline, the background between windows — and the
arrangement of real windows are composed in the guest and still need one.

`cargo xtask check-display` is the display arm's **smoke gate**, not a per-commit one:
it boots an image and compares the guest's screen against a `libdraw` render over QMP
`screendump`. It catches what a self-hash structurally cannot — a wrong base address,
a wrong stride, or swapped channels — because the guest stays consistent with itself.
CI runs it automatically via `.github/workflows/display.yml`, path-filtered to the
files that could break it.

Don't run kernel code on the host. Don't run `cargo build` directly in the kernel workspace without the custom target — it will fail.

## Review workflow

Every PR gets reviewed by a **separate, fresh Claude Code session** — `/pr-review <N>`,
defined in `.claude/skills/pr-review/SKILL.md`. The point of the split is that the
reviewer does not inherit the author's context, so it cannot inherit the author's blind
spots. The reviewer reports and does not edit; findings come back to the working session.

If you are the working session, do not run it on your own work — you are the author.

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
  architecture/    subsystems that EXIST — what they do and how they relate
  design/          subsystems DESIGNED BUT NOT BUILT (today: the display arm above M1)
  spec/            exact contracts (ABIs, wire formats, schemas, the shell language)
  reference/       catalogues (today: error codes only — see deferred-decisions.md)
  rationale/       why decisions were made (read here when puzzled)
  conventions/     how to write code in this project
  planning/        phase and subproject plans, with checkboxes
  archive/         superseded artifacts, kept for the record (the v5.1 design doc)
  decision-log.md  the running record of decisions and their reasoning
```

**Which of these describe the system as it is today**, and which do not — this matters more
than it looks, because reading the wrong class as current is how you end up confidently
wrong about how something works:

- **`spec/`, `reference/`, `architecture/`, `conventions/` describe current behaviour.**
  If one disagrees with the source, the source wins and the doc is a bug — fix it in the
  same change.
- **`rationale/` explains why**, and is largely timeless.
- **`design/`, `planning/` and `archive/` do not describe current behaviour.** `design/`
  is what a subsystem *will* be. Today it holds exactly one document: `fault-survival.md`
  (added 2026-08-19), which is not a display document at all — it is where the kernel's
  fault-survival intent is written down. What is built has moved out —
  `input-subsystem.md` and `widget-toolkit.md` graduated on 2026-08-12, `desktop-shell.md` and
  `graphical-session.md` on 2026-08-25 with Milestone 7, `ui-composition-model.md` on
  2026-08-26 with Milestone 8, and `display-substrate.md` on 2026-08-30 (owed by Milestone 9 and
  paid a milestone late) — so the rule to apply is simply "`design/` means not built".
  **Two docs graduated while still outrunning their code** — `desktop-shell.md` (its tray is
  v2) and `ui-composition-model.md` (its ports are unscheduled) — and each says in its Status
  line which sections are behaviour and which are intent. That is the pattern to copy when a
  document is mostly true, rather than one to spread.
  `planning/` is what is intended, with checkboxes for what is done. `archive/` is superseded.
  **Never conclude "the system does X" from any of them.**
- **`decision-log.md` is a dated record and is append-only.** Entries are true as of their
  date; correcting one to match today's code destroys the evidence. Append a new entry.

**A `design/` doc graduates to `architecture/` when the code lands** — the milestone that
builds it should carry that move as a checkbox, because the doc is otherwise the thing
nobody remembers to update.

**Filenames carry no version number.** `foo-design-v1.2.md` guarantees link rot on every
revision — renaming it breaks every inbound reference, which is a self-inflicted source of
exactly the drift these rules exist to prevent. The version belongs in the doc's Status
line; git carries the history. (`archive/os-design-v5.1.md` is the exception: there the
version is the artifact's identity and the file is frozen.)

Every doc under `architecture/` carries a **Status** line naming what is actually built and
when it was last checked. Trust it over the body's tense, and correct it when you find it
wrong.

`cargo xtask check-docs` (in CI) enforces the mechanical part: every relative doc link
resolves, every backticked `kernel/…`/`userspace/…`/`tools/…` path cited by a
current-behaviour doc exists, and every `architecture/` doc has a Status line. It cannot
tell whether prose is *true* — that part is on review. A deliberate reference to a path
that does not exist (an honest forward reference, or a record of a deletion) is exempted by
marking the line `<!-- check-docs: allow-missing -->`.

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
- **A new design decision is made** → append to `docs/decision-log.md` with date and reasoning
- **A deferred item is being implemented** → update `docs/rationale/deferred-decisions.md`
- **A spec contract changes** → update the spec doc; bump version markers as needed

## Status

The project is pre-v0.1. The syscall ABI, wire formats, and kernel internals are pre-stabilization. The `docs/spec/` documents are the canonical contracts within this pre-stabilization period; if a spec doc and the source disagree, the source wins and the spec is updated to match (filed against the decision log).

Phases 0–3 (foundation, kernel substrate, boot-to-userspace, service ecosystem) are **complete** (Phase 3 closed 2026-07-21). Phase 4 (toward a usable windowed desktop) is next. See `docs/decision-log.md` for the current implementation phase and `docs/planning/implementation-plan.md` for the slice-by-slice breakdown.
