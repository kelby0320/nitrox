# Nitrox Implementation Plan

Working document tracking implementation progress. Updated as work proceeds — this is meant to be edited freely, not preserved as a snapshot.

This file is the **index**. Each phase's detailed checklist lives in its own file (see the
phase table below); this page holds the current status, the phase map, and the cross-cutting
workstreams that span all phases.

## How to use this document

- Each phase has a goal, a checklist of work items, and a milestone definition ("how do I know this phase is done?").
- Check items off (`- [x]`) as they're completed.
- Items can be reordered within a phase if dependencies allow. The order shown is a suggested execution order, not a strict requirement.
- Add sub-items under any task if it grows complex enough to need breakdown.
- When deviating from the plan, note it inline (`Note: ...`) rather than rewriting silently — the reasons matter later.
- Phases overlap in practice. "Phase 1" being the focus doesn't mean nothing from Phase 2 can be touched; it means Phase 1's milestone is the next target.
- **The phase detail is split across files.** Edit the phase file, not this index, for checklist changes; update the Current status here when a phase's headline changes.

## Cross-references

Throughout the phase documents, links to `docs/architecture/`, `docs/spec/`, and `docs/rationale/` point to specific documents that contain the design and rationale. The architecture overview at `docs/architecture/overview.md` is the recommended entry point if context is needed.

## Phases

| Phase | Detail | Status |
|---|---|---|
| 0 — Foundation | [phase-0-foundation.md](phase-0-foundation.md) | ✅ complete |
| 1 — Kernel substrate | [phase-1-kernel-substrate.md](phase-1-kernel-substrate.md) | ✅ complete |
| 2 — Filesystem and namespace | [phase-2-filesystem-namespace.md](phase-2-filesystem-namespace.md) | ✅ complete (2026-06-26) |
| 3 — Service ecosystem | [phase-3-service-ecosystem.md](phase-3-service-ecosystem.md) | ✅ complete (2026-07-21) |
| 4 — A usable windowed desktop | [phase-4-desktop.md](phase-4-desktop.md) | 🚧 active |

**Subproject plans** (detailed breakdowns that hang off a phase):

- [shell-coreutils-plan.md](shell-coreutils-plan.md) — the typed shell + coreutils (a Phase 4
  subproject; assumes the Phase 4 CLI substrate prereqs are built first).

## Current status

- **Phase 0 (Foundation):** ✅ complete — kernel boots under QEMU+OVMF and renders a framebuffer
  boot screen. See the Phase 0 deviation notes.
- **Phase 1 (Kernel substrate):** ✅ complete — memory foundation, kernel diagnostics, paging +
  `AddressSpace`, the ELF loader, user-memory-access discipline, the handle table, the
  kernel-object substrate, threading + the context switch, the syscall fast path, the first
  userspace process, and the full syscall surface (handles, memory objects, clocks/timers,
  `sys_wait`, notifications, IPC with handle transfer, spawn + lifecycle, `sys_thread_create` +
  supervised exception suspend/resume).
- **Phase 2 (Filesystem and namespace):** ✅ complete (2026-06-26) — boots Limine → kernel/PCI →
  init → ext4 mount (userspace fs-server) → demand-paged reads → a live `eshell>`. Slice 10 (FAT,
  read-only) deferred to Phase 4.
- **Phase 3 (Service ecosystem):** ✅ complete (2026-07-21) — the kernel-first and userspace-runtime
  bands, the service-ecosystem machinery (service-mgr + supervision, RS startup protocol,
  path-based ELF spawn, RW ext4, the auth/session login chain, logging + profile servers), and the
  Definition-of-Done clauses (libstream + typed-log demo; the `/proc` scheduler-stats surface). The
  remaining backlog services are consumer-driven and defer to Phase 4. `std` is a serious
  compatibility target (2026-07-20 std stance).
- **Phase 4 (a usable windowed desktop → browser, networking, sysadmin):** 🚧 active.
  - **Substrate hardening** (the concurrency-review gate into Phase 4) — ✅ done (Parts A–F, F1–F12
    fixed; decision log 2026-07-21).
  - **Floating-point + SIMD enablement** — ✅ done (Parts A–D; per-thread XSAVE, hard-float userspace
    target `x86_64-unknown-nitrox`, proven in ring 3; decision log 2026-07-21, PR #110).
  - **CLI substrate prereqs** — directory ops, `Value` collection types, and the stdio/pipe
    convention (plus the `parent`/`child` retirement into a conforming test harness) — ✅ done
    (2026-07-23/24). See [phase-4-desktop.md](phase-4-desktop.md) → "CLI substrate prereqs".
  - **The typed shell + coreutils subproject** — 🚧 active from 2026-07-24. **Milestones 1,
    2, 3 and 3.5 complete**: the coreutils, the `nxsh` interpreter, and the shell as the
    login leaf (2026-07-31). **Milestone 4 — language completeness — planned 2026-08-04**
    from an audit of the built language against the design doc (now v1.2); it carries one
    kernel item, `sys_process_terminate`. Tracked in
    [shell-coreutils-plan.md](shell-coreutils-plan.md).
  - **Substrate gaps it surfaced** — exit-time handle reclamation, the wall clock, and file
    truncate — all ✅ (2026-07-24). See [phase-4-desktop.md](phase-4-desktop.md) → "Substrate
    gaps surfaced by the coreutils subproject".
  - **The display arm** — 🚧 in progress (**M1–M4 complete**: pixels + the gate 2026-08-05,
    a client with a surface 2026-08-06, input end to end 2026-08-10, the widget toolkit
    2026-08-11. **M5 — the GUI terminal — ✅ complete (2026-08-13)**: Part A (terminal semantics —
    `libterm`'s parser, grid, render and encoder, plus the blend that unblocked antialiasing)
    and Part B (`nxterm` itself: window, chrome, scrollback, key repeat, and the display gate's
    third region) both landed 2026-08-12, and Part C — the tty server's second backend, with
    `nxterm` hosting a real `nxsh` — on 2026-08-13, closing M5. **M6 — window management — ✅
    complete (2026-08-20)**: Part A (placement, restacking, move damage and the initial-configure
    handshake), Part B (the manager seam and four of its five events), Part C (popups positioned
    by their creator and clipped to the screen) and Part D (the spec). **M7 — the graphical
    session — was planned in detail 2026-08-21** and carries a prerequisite outside the display
    arm: [test-path-retrofit.md](test-path-retrofit.md), which lands first.)
    **Re-scoped 2026-08-12** after a gap surfaced while planning Part C: nothing said who logs a
    *graphical* user in or spawns the desktop shell. That column is specified now in
    [docs/design/graphical-session.md](../design/graphical-session.md), and the milestones above
    M5 changed shape — M6 is window management, **M7 is the graphical session** (new), M8 is
    desktops and the overview, M9 applications and drag-and-drop between them. (M8/M9 were
    rescoped 2026-08-21 when durable window-to-window wiring was cut — see the decision log.)
    Design in
    [docs/design/display-substrate.md](../design/display-substrate.md) +
    [docs/design/ui-composition-model.md](../design/ui-composition-model.md);
    build order in [display-arm-plan.md](display-arm-plan.md). Milestone 1 is the test
    gate: the compositor composites a known scene and host and guest agree on the hash.
  - **Now: pre-CLI substrate hardening** (the deferral audit, 2026-07-24) — four slices
    landing before coreutils Milestone 2: trustworthy deferral docs + CI running the QEMU
    gate; the demand-fault path (fill cookie, read-ahead, blocking second faulter, shared
    file-backed text); fs/ext4 completeness for M2; and the now-triggered hygiene items.
    See [phase-4-desktop.md](phase-4-desktop.md) → "Pre-CLI substrate hardening".

---

## Cross-cutting workstreams

Things that need ongoing attention across all phases, not phase-specific:

### Testing

- [x] Host-side unit tests for everything that doesn't require the kernel runtime (allocators, parsers, data structures, ABI encoding) — 784 and growing, via `cargo xtask test`
- [x] QEMU integration tests via `isa-debug-exit` for everything that does — `cargo xtask test-qemu` adjudicates the whole boot (`docs/conventions/qemu-integration-tests.md`)
- [x] CI runs both on every push (2026-07-24, PR #120) — a second job builds the image and runs `test-qemu` under `--kvm`, since the kernel is x2APIC-only and the runner's QEMU predates TCG x2APIC support
- [x] Add a test for any non-trivial bug fix — established practice; each fix lands with the test that fails without it, and the boot-loop campaigns cover what unit tests structurally cannot

### Documentation

- [ ] Architecture deep-dive docs in `docs/architecture/` written alongside the corresponding implementation
- [ ] Reference catalogues (`docs/reference/`) — kernel objects, syscalls, error codes, syscaps, rights — grown as the kernel grows
- [ ] Convention docs (`docs/conventions/`) — code style, unsafe policy, testing — written from observed patterns

### Decision log

- [ ] `docs/decision-log.md` updated whenever a significant decision is made during implementation — what was decided, why, what alternatives were considered

### Conventions enforcement

- [ ] `unsafe` blocks have SAFETY comments (clippy lint where possible)
- [ ] No external crate dependencies introduced into the kernel
- [ ] Lock ordering documented in `kernel/docs/lock-ordering.md` updated as new locks are added

---

## Where this document lives

Recommended location: `docs/planning/implementation-plan.md` or `IMPLEMENTATION.md` at the repo root. <!-- check-docs: allow-missing --> The repo root has the advantage of being easy to find; `docs/planning/` keeps the docs tree tidy. Either is fine — pick one and stick with it.
