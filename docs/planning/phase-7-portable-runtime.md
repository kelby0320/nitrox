# Nitrox Implementation Plan — Phase 7 — The portable runtime

Part of the [Nitrox Implementation Plan index](implementation-plan.md), which holds the
current status, the full phase list, and the cross-cutting workstreams. This phase is
**planned, not built** — nothing below describes current behaviour.

---

## Phase 7: The portable runtime

**Goal:** ordinary Rust programs run on Nitrox, and Nitrox programs run elsewhere.

**Agreed 2026-08-25 as "Phase 5"** and renumbered when bare metal and USB moved ahead of it
(decision log, 2026-09-10). Nothing about its content changed.

**Why these four things are one phase and not a grab-bag: TLS is the shared prerequisite.**
`std::thread` needs thread-local storage, and so does `ld.so` — a dynamically linked program's
thread-locals are resolved by the loader. Doing either alone builds most of the other. The
phase has a payoff independent of the browser it eventually serves, which is what keeps it from
being speculative work.

### Tasks

- [ ] **Thread-local storage** — `FS_BASE` / `sys_thread_set_tls`.
- [ ] **Real `std::thread`** — multi-threaded user processes. This is the **first consumer of
      the slice-3b cross-CPU deschedule IPI**, which has been built and unexercised since Phase
      3, plus per-thread FPU and TLS state.
- [ ] **A `std` subset over the native ABI** — `std::{fs, io, sync, thread}`; `net` waits for
      [Phase 8](phase-8-networking.md). No kernel change is needed: `std::fs` resolves paths
      through the process's root namespace (bounded ambient, capability-safe) and `std::io`
      blocking maps to `sys_io_submit` + `block_on`. See the decision log, 2026-07-20, which
      supersedes 2026-07-13.
- [ ] **Dynamic linking** — a userspace `ld.so`: map segments over the file-backed path, walk
      the dependency graph, apply relocations, resolve symbols.

### The dynamic-linking premise has expired, and that is the argument for doing it

The deferral was written on the finding that **static linking is correct at 13–73 KB per
binary**. Measured 2026-09-10 from the release build, the shipped binaries are **176–539 KB**:
the smallest is 2.4× the *top* of that range and the largest 7.4×, and against its bottom they
are 13× and 41×. Whichever end you take, the range the decision rested on no longer contains
them — and there are five binaries each embedding the whole widget toolkit and the font
rasteriser. Its own note said "build the loader at the second or third app"; there are five.

That is not an argument that static linking was wrong. It is an argument that the condition the
decision rested on no longer holds, which is exactly when a deferral is supposed to be
revisited.

### Definition of Done

A non-trivial external Rust crate ported **unmodified**, and a Nitrox program cross-built and
run on Linux. The second half is the real test: it proves the portability claim points both
ways rather than being a Nitrox-shaped `std`.

### What Phase 7 does not do

`std::net` (no network yet), `std::process` beyond what the capability model allows — there is
no `fork`, and there will not be one — and the POSIX C shim, which stays deferred until a
must-have C dependency forces it.
