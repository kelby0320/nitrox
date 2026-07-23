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
  - **Next: the CLI substrate prereqs** — directory ops, `Value` collection types, and the
    stdio/pipe convention — which unblock the typed shell + coreutils subproject. See
    [phase-4-desktop.md](phase-4-desktop.md) → "CLI substrate prereqs" and
    [shell-coreutils-plan.md](shell-coreutils-plan.md).

---

## Cross-cutting workstreams

Things that need ongoing attention across all phases, not phase-specific:

### Testing

- [ ] Host-side unit tests for everything that doesn't require the kernel runtime (allocators, parsers, data structures, ABI encoding)
- [ ] QEMU integration tests via `isa-debug-exit` for everything that does
- [ ] CI runs both on every push
- [ ] Add a test for any non-trivial bug fix

### Documentation

- [ ] Architecture deep-dive docs in `docs/architecture/` written alongside the corresponding implementation
- [ ] Reference catalogues (`docs/reference/`) — kernel objects, syscalls, error codes, syscaps, rights — grown as the kernel grows
- [ ] Convention docs (`docs/conventions/`) — code style, unsafe policy, testing — written from observed patterns

### Decision log

- [ ] `docs/history/decision-log.md` updated whenever a significant decision is made during implementation — what was decided, why, what alternatives were considered

### Conventions enforcement

- [ ] `unsafe` blocks have SAFETY comments (clippy lint where possible)
- [ ] No external crate dependencies introduced into the kernel
- [ ] Lock ordering documented in `kernel/docs/lock-ordering.md` updated as new locks are added

---

## Where this document lives

Recommended location: `docs/planning/implementation-plan.md` or `IMPLEMENTATION.md` at the repo root. The repo root has the advantage of being easy to find; `docs/planning/` keeps the docs tree tidy. Either is fine — pick one and stick with it.
