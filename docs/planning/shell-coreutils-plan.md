# Nitrox Shell & Coreutils — Subproject Plan

**Status:** planning (2026-07-22). Not started. This is a large, multi-slice subproject that will
run in its own Claude Code session(s). This document is the entry point for that work.

## What this is

A build plan and gap analysis for the Nitrox shell + coreutils **language subproject** — the
interpreter, the coreutils, and the (minimal) REPL. Derived from the design docs and cross-checked
against the implemented system:

- **Design (semantics/grammar):** `docs/history/nitrox-shell-design-v1.1.md`
- **Design (UI composition, upstream where they touch):** `docs/history/nitrox-ui-composition-model-v1.md`
- **This plan** sequences the subproject's own work (language → coreutils → minimal REPL) and
  records the design gaps it must resolve as it goes.

**Scope boundary.** This subproject covers the shell and coreutils *only*. The general substrate it
depends on — directory ops, `Value` collection types, the stdio/pipe convention — is **not** part
of this plan: it is built first as Phase 4 infrastructure, tracked in
[`phase-4-desktop.md`](phase-4-desktop.md) → "CLI substrate prereqs" (directory ops in particular
unblock far more than the shell). This plan **assumes those three prereqs are already in.** Their
full gap analysis stays here (§1C) because it is the authoritative reference for whoever builds
them; the Phase 4 checklist points back to it.

The design docs are the source of truth for *what the shell should be*. This plan is the source of
truth for *the order the shell/coreutils get built*.

## Governing decisions (set with the maintainer, 2026-07-22)

1. **Substrate prerequisites first, and outside this plan.** Directory ops, `Value` collections, and
   the stdio/pipe convention are built first as Phase 4 substrate (tracked in `phase-4-desktop.md`),
   because they are general infrastructure, not shell-specific. This subproject does not begin until
   they are in. It de-risks the pipeline model on the cheapest surface and keeps this plan focused.
2. **Split the REPL; defer the rich part.** The language + non-interactive script execution + a
   minimal line-reader on the raw console is this subproject. The rich interactive REPL (§11:
   reverse-search, Shift-Enter key events, schema-aware completion) is a *separate later milestone*
   gated on the console/tty server + compositor terminal, which are later in Phase 4. Building it
   now would be a dependency inversion.
3. **The design doc is trustworthy as v1.1.** The two real inconsistencies found in v1 (the `librt`
   reference; §9d presenting the `Value` collection types as already-existing) are corrected there.
   This plan carries the full analysis.

## How this depends on work already done

The completed **FP enablement slice** (Phase 4, decision log 2026-07-21) is a genuine prerequisite,
not a coincidence: the shell's `Float` values and their `format`/`display` output need hardware
floating point. That is now in place. The three CLI substrate prereqs (§1C, built in Phase 4) are
the remaining dependency before this subproject can start.

---

## Part 1 — Gap analysis

Three kinds of gap. The **system prerequisites (1C)** are the ones that gate the start of building;
the **design gaps (1B)** are decisions still owed but can mostly be made as their slice comes up.

### 1A. Design-doc inconsistencies — RESOLVED in v1.1

- `librt` (§1) → `libos`. `librt` was cut (decision log 2026-07-13).
- §9d presented `Value` as already having `Table`/`List`/`Record` variants "unchanged." It does
  not; reworded to "planned representation," with the implementation reality called out.
- Section numbering made contiguous (old §12→§11, §13→§12).
- Companion UI doc confirmed present in `docs/history/`.

### 1B. Design gaps — decisions still owed

| # | Gap | Where | When it must be resolved |
|---|---|---|---|
| B1 | **The stdio/stream wiring convention is undesigned.** The whole pipeline model presumes each stage is a process with stdin/stdout/stderr streams, but nothing specifies *which handles carry them, the spawn contract, how the shell builds the channels, or how `stderr` routes separately from the pipe.* | §1, §3, companion §3 (`form` writes to "its actual `stdout`") | **Before** Milestone 1 — it *is* the substrate. See C3. |
| B2 | **`whoami` has no identity to report.** Nitrox has no kernel user identity (capability model). Identity is a session concept (session-mgr builds the per-user namespace + home). The source of truth is unspecified — a namespace-bound `/proc/self/user`? a session-provided value? | §10c/§10d | When `whoami` is built (late; low-priority coreutil). |
| B3 | **"Env vars as namespace-scoped resources"** is the design's philosophical anchor but the mechanism is undesigned and unbuilt. The companion doc §7 also lists it as open. | §5a and passim | When the shell first needs env at all. Can be deferred past Milestone 1; must be designed before scripts rely on it. |
| B4 | **`~=` regex needs an engine.** The design says "the gap was a missing *operator*, not a missing *program*" — true, but the operator needs a regex engine, and none exists (no external-crate precedent in userspace; would be hand-rolled). `grep`'s replacement is gated on it. | §10a, §10b | When `filter ~=` / the `grep` story is built. Deferrable; scope it explicitly so it isn't mistaken for free. |
| B5 | **`save`/`open` format inference** (`.csv`/`.json`/`.txt`/`.tsm`) needs a serializer/deserializer per format. `.tsm` (native TSM1) is nearly free; the others are real work. | §4 | Per-format, incrementally. Start with `.tsm` + `.txt`. |
| B6 | **REPL interactivity depends on the deferred terminal stack.** History/reverse-search/Shift-Enter need a key-*event* channel; today's console is raw bytes over `/dev/console`. Per governing decision #2 this is split out. | §11 | Deferred milestone, gated on console/tty + compositor terminal. |

### 1C. System prerequisites — built in Phase 4 *before* this subproject

These are substrate, independently testable, and **not part of this subproject** — they are built
first as Phase 4 infrastructure (checklist in [`phase-4-desktop.md`](phase-4-desktop.md) → "CLI
substrate prereqs"). This analysis is the authoritative detail for whoever builds them; the Phase 4
checklist points here. C1–C3 gate the start of the subproject; C5/C6 are the subproject's own
interpreter work, listed here for completeness.

These are substrate, independently testable, and each is roughly a slice of its own.

| # | Prerequisite | Blocks | Current status |
|---|---|---|---|
| **C1** | **Directory operations** — `readdir`/`mkdir`/`rmdir`/`unlink`/`rename` across the stack: `librsproto` op codes + `fs-server-ext4` handlers + any kernel/syscall surface + a `libos` client wrapper. | Every file coreutil: `list`, `mkdir`, `remove`, `move`, `copy`, `rename`, `touch`. | **Not built.** `librsproto` defines only `ReadRange`; `docs/spec/rsproto-file-ops.md` marks `stat`/`readdir` as "land later." `fs-server-ext4` has internal inode `link` logic but no directory-op protocol. |
| **C2** | **`Value` collection types** — extend the in-memory `libstream` `Value` enum with `List`/`Record`/`Table` (Arc-backed, persistent), and implement the wire codecs for the reserved `List` (0x07) / `Record` (0x08) `TypeTag`s (currently `Unsupported`). Also **drop the `REC_WIDGET` (0x03) stub** — the companion doc §1 removed `widget_tag`; TSM1 is data-only. | The entire interpreter data model (§5c/§6/§9d/§9f). | **Not built.** Wire tags reserved but codec returns `Unsupported`; in-memory `Value` is scalar + `Str`/`Bytes`/`Handle` only. `REC_WIDGET` still present in `table.rs`. |
| **C3** | **stdio / pipe substrate** — a convention + library for wiring `stdin`/`stdout`/`stderr` channels across spawned stages. Includes resolving the **bootstrap-capacity collision** (see below) and a `libstream` **stdin reader** + `libos` pipe-wiring helpers. | All pipelines; the shell's ability to spawn and connect stages. | **Not built.** No stdio concept exists; today spawn passes handles ad hoc via bootstrap registers. |
| **C4** | **TSM1 stdin *reader* pattern** — a reusable pattern for a stage *consuming* a structured stdin stream. Today only the *produce* side is exercised (heartbeat → log channel). | Every non-source pipeline stage. | Partially there — `TableReader` exists; the wiring pattern does not. Folds into C3. |
| **C5** | **Interpreter foundation** — lexer, parser (the grammar is fully specified in §8/§9), tree-walking evaluator, the Arc-backed `Value` tree, and the generic operators (`filter`/`sort`/`select`/…). | The language. | **Greenfield.** `init/toml_lite` is the only parser precedent and is tiny; no reuse. |
| **C6** | **Float formatting** (`f64` → decimal string) for `format`/`display` of numeric data. | Readable output of any numeric pipeline. | Now *feasible* (FP landed); unwritten. |

#### The bootstrap-capacity collision (detail for C3)

A pipeline stage minimally needs to know: notification channel, root namespace, `stdin`, `stdout`,
`stderr` = **5** values. Today spawn delivers only **4** bootstrap handle-registers
(`rdi`/`rsi`/`rdx`/`rcx`) and `SPAWN_MAX_HANDLES = 4`. Two options, both anticipated by the kernel
code (`object/thread.rs:132` already notes "a later phase replaces this with a stack-resident
bootstrap block"):

1. **Raise the limit** — bump `SPAWN_MAX_HANDLES` and the bootstrap-register count (ABI change,
   touches the spawn hash), and route `stderr` as a real handle; or
2. **Stack-resident bootstrap block** — pass a `#[repr(C)]` bootstrap struct on the new process's
   stack, freeing the register budget entirely and giving room to grow (env, syscaps, more streams).

Option 2 is the more future-proof and is the direction the code comments point at; decide at the
top of C3. Either way `stderr` may alternatively be a **namespace-bound** sink (`/dev/stderr`),
which sidesteps the handle budget — evaluate against the design's "stderr is a separate channel"
requirement (§1).

---

## Part 2 — Build sequence

### Prerequisite (built in Phase 4, before this subproject starts)

The three CLI substrate prereqs — directory ops (C1), `Value` collections (C2), and the stdio/pipe
convention (C3/C4) — are Phase 4 infrastructure, tracked in
[`phase-4-desktop.md`](phase-4-desktop.md). Each self-validates (host codec tests; a throwaway
producer/consumer pair in QEMU). **Do not start the milestones below until they are in.** The ABI
call in C3 (raise `SPAWN_MAX_HANDLES` vs. stack-resident bootstrap block) must be recorded in the
decision log when it's made.

### Milestone 1 — the first coreutils (first integrated proof)

Two **native** coreutils that exercise the whole substrate end to end — the first real subproject
deliverable, and the first time the prereqs are proven *integrated* rather than in isolation:

- **`list`** — reads a directory via C1, emits `Table<{name, size, kind, modified}>` as TSM1 on
  stdout.
- **`copy`** — exercises C1's mutation path and multi-path args.

Proof: `list` piped into a trivial consumer over a real channel, output correct, backpressure and
`PeerClosed` (early-consumer close) both exercised. Validates the pipeline model **before a line of
interpreter is written**.

### Milestone 2 — coreutils breadth

The rest of §10c: `move`, `remove`, `mkdir`, `touch`, `rename`, `date`, `sleep`, `whoami` (resolve
B2 here). Each native, each a TSM1 stage. Aliasing (§10e) is namespace-bind data, not a program.

### Milestone 3 — the interpreter (C5/C6)

Lexer → parser (grammar §8/§9) → tree-walker → generic operators → `Value` tree. Deliver **non-
interactive script execution** first (`nx script.nx`), plus a **minimal** line-reader on the raw
console for a basic interactive loop (no reverse-search/Shift-Enter). Float formatting (C6) lands
here. Resolve B3 (env) and B5 (`save`/`open` formats, starting with `.tsm`/`.txt`) as they come up;
scope B4 (regex for `~=`) explicitly as its own piece rather than absorbing it silently.

### Deferred — the rich REPL (§11) and its dependencies

Gated on the console/tty server + compositor terminal (later in Phase 4). Covers reverse-search,
Shift-Enter continuation (needs a key-event channel), job control's `fg`/`&`, schema-aware
completion, and the prompt's live `PipelineStatus` glyph. Tracked but out of this subproject.

### Explicitly out of scope (design §10a/§13, carried forward)

Process management (`ps`/`kill` — needs the "how does a command acquire a capability handle to a
process it didn't spawn" design pass), networking tools (netstack deferred), user-definable aliases
with baked-in arguments, package system beyond single-file `use`, circular-import resolution.

---

## Part 3 — First-session checklist (for the forked work)

**First confirm the prerequisites are in.** The three CLI substrate prereqs (§1C) are built in
Phase 4 *before* this subproject — check them off in [`phase-4-desktop.md`](phase-4-desktop.md).
If they are not done, that is the work to do first, not this plan.

With the prereqs in, read, in order: this plan → `nitrox-shell-design-v1.1.md` →
`nitrox-ui-composition-model-v1.md` (for `form`/stdout only) → `docs/spec/typed-stream-format.md`
(TSM1 wire) → `docs/spec/rsproto-*.md` (the protocol the fs-server speaks). Then start at
**Milestone 1 (`list` + `copy`)** — the first integrated proof that the substrate composes.
