# Nitrox Implementation Plan — Phase 0 — Foundation

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams. Phases 0–3 are
complete; Phase 4 is active.

---

## Phase 0: Foundation

**Goal:** kernel boots in QEMU, prints to the serial console, halts. The development loop is working.

**Why this phase matters:** every subsequent piece of work is dramatically easier with a working dev loop. Investing extra time here pays off across the entire project. Resist the temptation to perfect, but get to "kernel boots and prints" before going further.

### Tasks

- [x] Monorepo set up with three workspaces (`kernel/`, `userspace/`, `tools/`) per the structure in [docs/architecture/overview.md]
- [x] Top-level repo structure (`docs/`, `.cargo/`, `.gitignore`, `README.md`, `LICENSE`)
- [x] `CLAUDE.md` files in place (root, `kernel/`, `userspace/`, `userspace/libkern/`, `userspace/init/`)
- [x] `.claude/settings.json` configured
- [x] Custom target JSON for `x86_64-unknown-none` in `kernel/.cargo/config.toml`
- [x] `cargo build-std` configuration working for the kernel target
- [x] Kernel entry point — pure-Rust `extern "C" fn _start` in `kernel/src/main.rs` (Limine sets up long mode, paging, GDT, and a stack, so no NASM boot stub is needed in Phase 0 — see deviation note)
- [x] Limine boot protocol integration: request structs in kernel binary, response handling in `kernel_main`
- [x] Minimal `kernel_main` that renders a boot screen to the framebuffer (serial output deferred — see deviation note)
- [x] Limine configuration file builds correctly
- [x] `tools/xtask/` workspace with the `xtask` binary crate
- [x] `xtask build` — builds kernel, assembles disk image
- [x] `xtask qemu` — runs the kernel under QEMU with serial console captured
- [x] `xtask qemu-debug` — runs QEMU with GDB stub enabled
- [x] `xtask test` — runs host-side unit tests (stub OK; will grow)
- [ ] `xtask test-qemu` — QEMU integration tests via `isa-debug-exit` (not built in Phase 0 — see deviation note)
- [x] GitHub Actions CI running `cargo build` and `xtask test` on every push
- [x] `docs/` populated with the foundational documents (overview, rationale, spec)
- [x] v5.1 design doc archived at `docs/history/design-doc-v5.1.md`
- [x] Decision log started at `docs/history/decision-log.md`

### Milestone

`xtask qemu` boots Limine, the kernel renders a boot screen to the framebuffer, then halts. CI is green. (Serial output was deferred to Phase 1 — see the deviation note below.)

### Notes / deviations

- No NASM anywhere. Limine drops the kernel into long mode with paging,
  a GDT, and a stack already set up, so a pure-Rust `extern "C" fn _start`
  is sufficient. The context switch, originally slated for NASM, also
  landed as Rust-emitted `naked_asm!` — consistent with every other piece
  of kernel assembly and free of any assembler in the build. (Decision
  log, 2026-05-13 and 2026-05-29.)
- No serial output. Phase 0 renders to the framebuffer instead; the
  serial console was deferred. It lands in the Phase 1 "Kernel
  diagnostics" slice. (Decision log, 2026-05-13.)
- `xtask test-qemu` was not built — there is no QEMU integration-test
  harness yet. It lands when the first test that needs it does (serial
  output is a prerequisite). Tracked in the cross-cutting Testing
  workstream.
- Arch directory is `kernel/src/arch/x86_64/`, matching the Rust target
  triple `x86_64-unknown-none` and `cfg(target_arch = "x86_64")`. The
  `x86_64` naming is standardized across `CLAUDE.md` and the `docs/`
  tree (2026-05-20 doc-sync; see the decision log).
