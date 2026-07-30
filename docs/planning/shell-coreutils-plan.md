# Nitrox Shell & Coreutils — Subproject Plan

**Status:** 🚧 active (started 2026-07-24, Milestone 1). The three CLI substrate prereqs (§1C) are
all in, so the milestones below are unblocked. This is a large, multi-slice subproject running in
its own Claude Code session(s); this document is the entry point for that work.

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
| **C1** | **Directory operations** — `readdir`/`mkdir`/`rmdir`/`unlink`/`rename` across the stack: `librsproto` op codes + `fs-server-ext4` handlers + any kernel/syscall surface + a `libos` client wrapper. | Every file coreutil: `list`, `mkdir`, `remove`, `move`, `copy`, `rename`, `touch`. | **Built** (2026-07-23, PR #112). Direct client↔fs-server RPC; `librsproto` `ReadDir`/`Mkdir`/`Unlink`/`Rmdir`/`Rename`; four e2fsck-clean ext4 mutation ops. **Deferred within:** the client wrapper (landed 2026-07-24 as `librsproto::session::Dir`; `libos` was the wrong home — it is below the protocol and `alloc`-free), cross-dir/overwrite `rename`, full-directory grow. |
| **C2** | **`Value` collection types** — extend the in-memory `libstream` `Value` enum with `List`/`Record`/`Table` (Arc-backed, persistent), and implement the wire codecs for the reserved `List` (0x07) / `Record` (0x08) `TypeTag`s (currently `Unsupported`). Also **drop the `REC_WIDGET` (0x03) stub** — the companion doc §1 removed `widget_tag`; TSM1 is data-only. | The entire interpreter data model (§5c/§6/§9d/§9f). | **Built** (2026-07-23, branch `phase-4/value-collections`). `Value::List(Arc<[Value]>)`/`Record(Arc<Record>)`/`Table(Arc<Table>)`; self-describing `List` + sub-schema `Record` codecs; `Table` is a stream, not a cell (`type_tag()` → `Option`, `write_value` refuses it). Shared `wire::write_row_values`/`read_row_values`. `REC_WIDGET` removed. |
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

**Decisions taken at the start of the milestone (2026-07-24):** entry metadata rides in the
`ReadDir` reply rather than a per-name `Stat` op (Part A below); the integrated proof lives in
`test-harness`, which init runs to completion and adjudicates serially; the coreutils are one
`userspace/coreutils` crate with a bin per program plus a shared lib for the pieces every stage
needs. Slice branch: `phase-4/coreutils-m1`.

- [x] **Part A — `ReadDir` entries carry inode metadata** (2026-07-24). `list`'s `size` and
  `modified` columns had **no source**: the dir-ops reply carried `{inode, kind, name}` only, and
  no `File::Stat` op exists. The entry prefix widens 8 → 24 bytes (`mode: u16`, `size: u64`,
  `mtime: i64` added — `mode` fits in what was alignment padding), keeping a listing at **one round
  trip per reply** instead of `1 + N`. `ext4::read_dir` keeps its metadata-free form (so `rmdir`'s
  emptiness scan pays nothing) alongside a new `read_dir_stat`; `mtime` decodes ext4's post-2038
  `i_mtime_extra` epoch bits. `rsproto-file-ops.md` refreshed — it still documented only
  `ReadRange`. Host tests + `test-qemu` PASS, negative-controlled (zeroed metadata fails the run).
  See the decision log (2026-07-24).
- [x] **Part B — the directory client + the `coreutils` crate** (2026-07-24). The deferred
  "`libos` `open_dir`/`read_dir` wrapper" **moved to `librsproto`** behind an `io` feature:
  `libos` is *below* `librsproto` in the layering and is `alloc`-free, so it cannot host a
  client for a protocol defined above it. `session::Dir` owns encode → send → wait → recv →
  decode plus cursor-following, over a caller-provided buffer, with three-way errors
  (`Server`/`Transport`/`Protocol`) so "no such entry" is distinguishable from "the pipe broke".
  New `userspace/coreutils` crate: shared `stage` prologue (Tier 0 *and* Tier 1 — a coreutil must
  be spawnable before the shell exists) and `args` (GNU §10f conventions, declarative, no bare
  `-`). The harness's two directory demos now drive `Dir` (~150 lines of hand-rolled syscall code
  deleted) — the client's integration proof, plus a new error-path case. Host suite 752 green,
  `test-qemu` PASS. See the decision log (2026-07-24).
- [x] **Part C — `list`, through a real pipe** (2026-07-24). The first coreutil:
  `Table<{name, size, kind, modified}>` on stdout, `--recursive` with parent-relative names,
  a plain-text fallback when there is no `stdout` (Tier 0), and `PeerClosed` as a **clean**
  exit. The harness spawns it as a Tier-1 stage over a **depth-1** pipe and asserts the
  stream exceeded one IPC payload — so backpressure was provably exercised, not assumed —
  plus the schema field by field, the row contents, `--recursive` descent, and a clean exit
  after an early consumer close (negative-controlled). Caught two defects (`--recursive`
  reported bare names; the crate's host test build was broken by a bin-only `build.rs`) and
  surfaced the **no-wall-clock** gap: nothing can stamp a new inode, so OS-created files
  report `modified: 0` (filed in `deferred-decisions.md`). See the decision log (2026-07-24).
- [x] **Part D — `copy`, the mutation side** (2026-07-24). File and recursive-directory copy,
  `--force`, emitting `Table<{source, destination, bytes}>`. File *contents* bypass the directory
  protocol entirely (a file is a mapped page-cache object), so the helpers live in `coreutils::fs`.
  Turned up **two gaps**, both filed rather than papered over: (1) **no truncate** — `--force` onto
  a *longer* destination is refused, since the old tail would survive; (2) **a dead process's
  handles are never reclaimed**, so a pipe endpoint held by an exited stage never closes and its
  peer never sees `PeerClosed` — which is the mechanism the pipeline model needs for a stage that
  dies early. Negative-controlled. See the decision log (2026-07-24).

**Milestone 1 is complete**, and so are the three substrate gaps it surfaced — exit-time handle
reclamation (PR #117), the wall clock (PR #118), and file truncate (2026-07-24). Both assertions
the milestone had to weaken are restored and now serve as those fixes' regression tests: `copy`'s
demo drains a stage's stream before reaping it, and `list`'s demo requires a plausible `modified`
date. The remaining known timestamp gap is `mtime` on an **in-place overwrite**, which Model A
hides from the fs-server (`deferred-decisions.md`).

### Milestone 2 — coreutils breadth

The rest of §10c: `move`, `remove`, `mkdir`, `touch`, `rename`, `date`, `sleep`, `whoami` (resolve
B2 here). Each native, each a TSM1 stage. Aliasing (§10e) is namespace-bind data, not a program.

Milestone 1 proved one pipeline end to end — a reader (`list`) and a mutator (`copy`) over a real
pipe. Milestone 2 is deliberately *not* another integration proof; it is **breadth on a substrate
that is now finished**, and the parts below are ordered so the mechanical ones land first and the
one genuine design question lands last.

Two things hold for every part, and are not repeated in each:

- **Each utility is a TSM1 stage that must also work at Tier 0.** Every one uses the
  `coreutils::stage` prologue, which reports which tier it was spawned in. **Tier 1** is the
  shell-spawned case: `arg0` marks a setup message pending, and that message carries `argv` and
  the three stream handles, so the utility writes its `Table`/`Record` as TSM1 to the `stdout`
  channel. **Tier 0** is everything else — today init and the test harness, since the shell is
  not built until Milestone 3 — where `Stage::enter` yields no `argv` and all three streams
  `None`.

  Two consequences, and they are separate. First, a Tier-0 stage has no `argv`, so it can only
  run its argument-free default; harness demos are limited to that. Second, it has no `stdout`
  to put a typed stream on, so it needs a **plain-text fallback**: `list` matches on
  `stage.streams.stdout` and, for `None`, renders one human-readable line per row to the kernel
  log via `kprint` instead of a TSM1 table. Plain text rather than the same bytes elsewhere,
  because the reader in that position is a person on the serial console, not a decoder.

  **The kernel log is the wrong destination long-term, and this is scaffolding**
  (`TODO(tier0-output-sink)`, `deferred-decisions.md`). `kprint` is a kernel *diagnostic* path
  and the klog is a bounded ring, so program output evicts kernel diagnostics. It is acceptable
  while init and the test harness are the only spawners; it should not survive the shell. New
  utilities should follow `list`'s existing shape for now rather than inventing a second answer
  — one fallback to change later is better than eight.

  `PeerClosed` on the output side is a **clean** exit, not an error.
- **Flags follow the declarative `coreutils::args` conventions** (GNU §10f, no bare `-`), and the
  mutating verbs take `--force` with the same meaning `copy` gave it.

Parts, in order (tick as they land):

- [x] **Part A — `mkdir`, `remove`** ✅ (2026-07-30)
- [x] **Part B — `rename`, `move`** ✅ (2026-07-30)
- [ ] **Part C — `touch`** (create-if-absent + `mtime` stamp; likely needs `File::Touch` exposed)
- [ ] **Part D — `date`, `sleep`** (no filesystem; mostly host-testable formatting and parsing)
- [ ] **Part E — `whoami`** (blocked on open question B2 — a design call, deliberately last)

The substrate these lean on is in place: Slice C delivered cross-directory + overwrite `rename`
(which is what unblocks `move`), full-directory growth, and `mtime` on in-place overwrite; the
wall clock landed in PR #118; `librsproto::session::Dir` already exposes `mkdir` / `unlink` /
`rmdir` / `rename` / `read_dir`; `coreutils::fs` already has `copy_file`, `rename`, `file_size`,
`ns_children` and the `basename`/`parent`/`join` helpers.

**Expect each part to surface substrate gaps, and file them rather than paper over them.** That is
what Milestone 1 did — it turned up no-truncate, no-wall-clock and unreclaimed handles, all of
which became their own fixes — and it is the main reason to build breadth before the interpreter.

#### Part A — the directory verbs: `mkdir`, `remove` ✅ (2026-07-30)

**Landed.** Both are Tier-1 stages emitting tables (`Table<{path, created: Bool}>` and
`Table<{path, kind}>`), with `fs::is_dir` hoisted out of `copy` as the third caller. Three things
the part turned up, none of them the ones the sketch below predicted:

- **The filesystem collapses `Exists` and `NotEmpty` into `InvalidArgument`**, so neither utility can
  branch on the error to decide whether a path already exists. Both establish the fact directly
  instead — `mkdir --parents` tests each component with `is_dir`, `remove` establishes emptiness by
  listing. Filed as `TODO(fs-error-granularity)`; fixing it is an ABI change.
- **`remove` must walk the filesystem only, never the namespace union.** `list`'s descent merges
  namespace bindings with filesystem entries, which is right for looking and wrong for deleting: a
  binding is a mount point, and `remove --recursive /` must not unbind `/dev/console`. So the
  descent was *not* hoisted from `list`, contrary to the guess below — the semantics differ at
  exactly the point that matters.
- **The first two negative controls were vacuous, and nearly passed as verification.** Asserting
  that `remove --force /dev/console` is refused proves nothing: it fails with or without the binding
  check, because `/dev` is not a filesystem directory to open. The isolating case is `/dev` itself —
  a binding directly beneath a real filesystem directory, which without the check classifies as
  "missing" and turns into a silent `--force` success. Re-designed until each control actually
  failed the demo.

The original sketch, kept for the record:

Both are thin wrappers over session ops that already exist, which makes this the natural first
part: it exercises the `Dir` client on the *mutating* side without needing anything new.

- `mkdir` — `Dir::mkdir`, plus `--parents` (create intermediate components, succeed if the leaf
  already exists). Emits `Table<{path, created}>` so a pipeline can tell "made it" from "already
  there".
- `remove` — `Dir::unlink` for files, `Dir::rmdir` for empty directories, `--recursive` to walk
  and remove depth-first, `--force` to ignore a missing target. Emits `Table<{path, kind}>` of
  what was actually removed.

The interesting case is `remove --recursive`: `rmdir` only removes an *empty* directory, so the
walk order is load-bearing, and the natural implementation re-uses `list`'s recursive descent.
Worth checking whether that descent belongs in `coreutils::fs` rather than being written twice.

#### Part B — `rename` and `move` ✅ (2026-07-30)

**Landed.** `rename` is the thin one and deliberately has *no* fallback — a cross-mount rename is an
error, so a caller who wants the cheap identity-preserving operation can ask for it and be told when
it is impossible. `move` adds the fallback and **reports which method it used**
(`Table<{from, to, method}>`, `method` ∈ `rename` | `copy`). That field is not decoration: the two
differ in cost and in whether the file keeps its identity, and it is the only way to *prove* the
same-mount path is not silently copying — an assertion on "the file arrived" would pass an
implementation that copied every time.

The predicted finding held: **the test image has exactly one writable mount**, so no cross-mount move
runs end to end. `/initramfs` (a read-only kernel server) gives the *detection* path a target, and
the demo asserts the half that is testable and most worth protecting — a failed move leaves the
source intact. The successful fallback has never executed. A cross-mount *directory* move is
therefore refused rather than written blind; both are filed as `TODO(cross-mount-move)`, whose
trigger is a second writable mount (a second binding of the existing server at its own subtree base
would do — `us_forward_existing_reg` already supports one endpoint under many names).

The original sketch, kept for the record:

`rename` is the thin one — `coreutils::fs::rename` already exists and Slice C2 made it work across
directories and over an existing destination. It emits `Table<{from, to}>`.

`move` is `rename` plus the fallback: when the two paths are on different mounts the kernel reports
`CrossDevice`, which `coreutils::fs` already documents as *"a caller's cue to fall back to
`copy_file` + unlink rather than an error to report"*. So `move` is the first utility that composes
two existing helpers rather than wrapping one, and the first whose correctness depends on a
failure path — which means the harness demo must exercise the cross-mount case, not just the
same-mount one. If there is no second mount in the test image, that is itself the finding.

#### Part C — `touch`

Two behaviours in one verb: create the file if absent, and stamp `mtime` to "now" if present.

Creation is `SYS_FILE_CREATE`. The stamp is `File::Touch`, which **already exists on the wire and
in the fs-server** — Slice C4 added it so the kernel could report a Model A in-place overwrite —
but its only caller today is the kernel's post-writeback path, and `librsproto::session::Dir` has
no client wrapper for it. So this part probably starts by exposing an existing op rather than
designing a new one. Confirm that before writing the utility; if the op turns out to be
kernel-only by construction, that is a gap to file, not to work around.

#### Part D — `date` and `sleep`

Neither touches the filesystem, which makes them the two most host-testable utilities in the set —
and the first that are mostly *formatting and arithmetic*, so the bulk of each should be unit
tests rather than boot demos.

- `date` — reads `CLOCK_REALTIME` via `SYS_CLOCK_READ` (live since PR #118) and formats it. There
  is no `std`, so civil-from-days conversion is hand-rolled and is exactly the kind of code that
  earns a host test table (epoch, leap years, the 2100 non-leap case). Emits a `Record` rather than
  a bare string so a pipeline can select fields.
- `sleep` — `SYS_TIMER_CREATE` + `SYS_TIMER_SET` + `sys_wait`, the pattern the test harness's
  `timer_sleep_ms` already uses. The parsing (`5`, `1.5s`, `200ms`) is the testable part; the
  waiting is three syscalls. Emits nothing.

#### Part E — `whoami`, and open question B2

**Deferred to when the part is reached, deliberately.** Nitrox has no kernel user identity —
authority is held in handles, not derived from a UID — so identity is a *session* concept that
session-mgr constructs along with the per-user namespace and home. The likely answer is that
`whoami` asks **session-svc**, which is the component that actually knows; the alternative shape is
a namespace-bound `/proc/self/user` that session-mgr populates per session, which keeps the
utility a plain reader and puts the knowledge in the namespace where the rest of the system already
looks.

Both are consistent with the capability model, and the choice is about where the truth lives rather
than about `whoami` itself, so it is worth a short design discussion at the time rather than a
guess now. Nothing else in Milestone 2 depends on it, which is why it is last.

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
