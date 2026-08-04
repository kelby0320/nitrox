# Nitrox Shell & Coreutils — Subproject Plan

**Status:** 🚧 active (started 2026-07-24; Milestones 1, 2, 3 and 3.5 complete — the shell is the
login leaf as of 2026-07-31 — and **Milestone 4, language completeness, planned 2026-08-04**). The three CLI substrate prereqs (§1C) are
all in, so the milestones below are unblocked. This is a large, multi-slice subproject running in
its own Claude Code session(s); this document is the entry point for that work.

## What this is

A build plan and gap analysis for the Nitrox shell + coreutils **language subproject** — the
interpreter, the coreutils, and the (minimal) REPL. Derived from the design docs and cross-checked
against the implemented system:

- **Design (semantics/grammar):** `docs/history/nitrox-shell-design-v1.2.md`
- **Design (UI composition, upstream where they touch):** `docs/history/nitrox-ui-composition-model-v2.md`
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
   **Amended 2026-08-04:** it is now v1.2, and that pass *did* change semantics. Auditing the built
   language against the doc turned up gaps in the design itself (no conversion, no `break`, no way
   to raise an error, no answer to `Ctrl-C`) and two places where a mechanism the doc specified was
   unreachable in the implementation. Both directions were possible because v1.1 checked the doc
   against the system by reading; this one checked by running. See Milestone 4.

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
- [x] **Part C — `touch`** ✅ (2026-07-30)
- [x] **Part D — `date`, `sleep`** ✅ (2026-07-30)
- [x] **Part E — `whoami`** ✅ (2026-07-30) — B2 resolved

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
  branch on the error to decide whether a path already exists. Both established the fact directly
  instead — `mkdir --parents` tested each component with `is_dir`, `remove` established emptiness by
  listing. Filed as `fs-error-granularity`; fixing it was an ABI change. **Fixed 2026-07-30** in the
  batched ABI pass: `KError` gained `AlreadyExists`/`NotEmpty`, and `mkdir` now probes only on a
  collision rather than ahead of every component.
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

The predicted finding held: **the test image had exactly one writable mount**, so no cross-mount move
ran end to end. `/initramfs` (a read-only kernel server) gave the *detection* path a target, and the
demo asserted the half that was testable and most worth protecting — a failed move leaves the source
intact. The successful fallback had never executed, so a cross-mount *directory* move was refused
rather than written blind.

**Closed 2026-07-30.** The trigger was met the cheap way the entry predicted: binding the one
fs-server a second time at `/scratch` with its own subtree base, which the kernel classifies as
another mount (`same server && same subtree base` is the rename test) while both sides stay
writable. The successful file fallback and the recursive directory case are both exercised now, on
`fs::copy_tree`/`fs::remove_tree` — the walks hoisted out of `copy` and `remove` so there is one of
each rather than three.

The original sketch, kept for the record:

`rename` is the thin one — `coreutils::fs::rename` already exists and Slice C2 made it work across
directories and over an existing destination. It emits `Table<{from, to}>`.

`move` is `rename` plus the fallback: when the two paths are on different mounts the kernel reports
`CrossDevice`, which `coreutils::fs` already documents as *"a caller's cue to fall back to
`copy_file` + unlink rather than an error to report"*. So `move` is the first utility that composes
two existing helpers rather than wrapping one, and the first whose correctness depends on a
failure path — which means the harness demo must exercise the cross-mount case, not just the
same-mount one. If there is no second mount in the test image, that is itself the finding.

#### Part C — `touch` ✅ (2026-07-30)

**Landed, and the prediction was right in outline and wrong in degree.** `File::Touch` did already
exist — but *kernel-only by construction*, not merely un-wrapped: it lives on the kernel↔server
control channel, is path-addressed, and is fire-and-forget with no reply, because its one caller is
the kernel reporting a Model A write the server cannot otherwise observe. A client session sending
it got `Unsupported`. So this was not "expose an existing op" but "give an existing opcode a
session-scoped *form*": `ext4::touch_at(dir_ino, name)` beside `mkdir_at`/`unlink_at`/`rmdir_at`, one
arm in the session dispatch, and `Dir::touch`. Name-addressed inside an open directory, so
confinement stays structural, and it returns a status like the other mutations.

Worth noting what it is *not*: an rsproto addition, entirely in userspace. No `KError` discriminants,
no ABI hash move, `abi-sync-check` untouched — so it did not belong in the batched kernel-ABI pass,
which is why it was taken now rather than deferred with `fs-error-granularity`.

The utility itself has no `--date`, and that is deliberate rather than unfinished: the wire carries
no timestamp because one a caller could choose would be forgeable metadata, so the server reads its
own clock. `touch` therefore cannot express "set mtime to X" and does not pretend to.

The original sketch, kept for the record:

Two behaviours in one verb: create the file if absent, and stamp `mtime` to "now" if present.

Creation is `SYS_FILE_CREATE`. The stamp is `File::Touch`, which **already exists on the wire and
in the fs-server** — Slice C4 added it so the kernel could report a Model A in-place overwrite —
but its only caller today is the kernel's post-writeback path, and `librsproto::session::Dir` has
no client wrapper for it. So this part probably starts by exposing an existing op rather than
designing a new one. Confirm that before writing the utility; if the op turns out to be
kernel-only by construction, that is a gap to file, not to work around.

#### Part D — `date` and `sleep` ✅ (2026-07-30)

**Landed, and the plan's instinct about where the tests belong held.** The two pieces that can be
wrong without a kernel — civil-from-days and duration parsing — went into `coreutils::time` and are
covered by six **host** tests; the boot demo only carries the halves that touch syscalls. The leap
table pins the epoch, an ordinary leap year, 2000 (leap, divisible by 400), and **2100** (*not*
leap, divisible by 100 but not 400) — the case a hand-written rule usually gets wrong, and the
reason the implementation is Hinnant's era arithmetic rather than a chain of special cases.

Two design points worth recording, both about refusing to invent:

- **`date` emits fields, not a string** — `Table<{unix, year, month, day, hour, minute, second}>` —
  because a formatted string forces every consumer to parse it back apart, which is the Unix habit
  the typed-stream model exists to avoid. `--unix` *narrows the schema* to one field rather than
  emitting all seven and letting the consumer pick.
- **No `--format` and no timezone**, and **no `--date` on `touch`** for the same family of reason:
  there is no tz database and no locale, so an offset would be a fiction. An unset clock is an
  error rather than a printed 1970.

`sleep` arms an **absolute** monotonic deadline computed once, so time between the clock read and
the arm is inside the wait rather than added to it. Its demo asserts a lower bound only —
deliberately: a `sleep` that returned immediately would still exit zero, so the bound is the whole
assertion, while an upper bound under TCG on a loaded host would be a flaky test rather than a real
property.

The original sketch, kept for the record:

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

#### Part E — `whoami` ✅ (2026-07-30) — B2 resolved

**Resolved in favour of the namespace.** `session-mgr` publishes `/session/user` when it builds the
session namespace; `whoami` reads it. Not a syscall (the kernel has no identity to report) and not a
service call (which would be closer to ambient lookup than to capabilities). The deciding argument
was consistency: the namespace is already how this system answers questions of this shape — the
shell does not ask where home is, it sees `/home` — and identity is the same kind of fact, known by
the same component, at the same moment.

**Absence is an error, not a default.** A process outside any session has nothing bound there and
`whoami` says so and exits non-zero. Reporting `root` or an empty name would be a fabricated fact,
the same reason `date` refuses to print 1970 when the clock is unset.

**Staged deliberately, with a checkable trigger.** The binding is a direct handle to a memory object
— a snapshot, correct because a session's user is immutable for its lifetime. Session metadata will
grow toward tty and job state, and *the first genuinely mutable member* is the trigger to put a
resource server behind `/session/*`. That migration touches no client: a server answers a resolve
with `OBJECT_KIND_MEMOBJ`, so `lookup + map + read` is byte-identical either way, and the namespace
is precisely what hides the difference. Filed as `TODO(session-metadata-server)`, along with the
coupling worth watching — **B3 (env) is the same problem one milestone earlier** (mutable,
namespace-scoped values, due in Milestone 3), so `/session/*` should probably migrate onto whatever
B3 builds rather than onto a bespoke session server.

The original sketch, kept for the record:

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

**Milestone 2 is complete** (2026-07-30): `mkdir`, `remove`, `rename`, `move`, `touch`, `date`,
`sleep`, `whoami`.

**The batched fs-server ABI pass landed the same day**, closing `fs-error-granularity`. It ran wider
than the one scheduled item: the same collapse turned out to exist in the *kernel*
(`sys_ns_bind` on an occupied path was `InvalidArgument`, indistinguishable from a malformed path),
which is what makes `AlreadyExists`/`NotEmpty` kernel errors rather than filesystem ones; three
further arms of the server's error mapping were already reaching for a vaguer error than existed;
and `libkern` had never decoded `IoError` at all. See the decision log, 2026-07-30.

**`cross-mount-move` closed the same day**, which clears the last item Milestone 2 owed. The blocker
was the fixture rather than the feature — a second writable mount, met by binding the existing
fs-server again at its own subtree base — and with somewhere to test it, the recursive directory
case took shared tree walks rather than a third copy of the loop.

### Milestone 3 — the interpreter (`nxsh`)

Lexer → parser (§8/§9) → tree-walking evaluator → generic operators → the process boundary → a
minimal REPL. Float formatting (C6) lands here. This is the largest milestone in the subproject and
is broken into seven parts, sequenced so that **a real script runs at Part C** rather than at the
end.

#### Names — settled 2026-07-30

**Binary `nxsh`, scripts `.nx`.** §9h left the extension explicitly undecided, so both were checked
for collisions before committing:

- **`nx` as a binary was rejected.** [Nx](https://nx.dev) is a widely-used monorepo build system
  whose CLI is exactly `nx` — plausibly on a developer's `$PATH`, and it dominates every search for
  "nx shell" or "nx scripting".
- **`nxsh` collides only with an obscure package** — the Next Scripting Framework's Tcl-based
  `nxsh(1)` in Debian/Ubuntu, plus a GUI terminal client called NxShell. Same category, exact name,
  negligible traffic. A small collision traded for a large one, and it reads as "Nitrox Shell".
- **`nsh` was struck**: it is NuttShell, the NuttX RTOS's shell — same name, same category, an OS
  shell, which is the worst kind of collision available.
- **`.nx` collides only outside our domain** (MapleStory packages, LowRes NX game files, a PKG4
  archive format). Nothing a developer would confuse with a script, no tooling conflict, and it
  reads like a language extension (`.rs`, `.py`, `.go`) rather than a shell one — which matches
  §3's insistence that this is a real scripting language, not a command launcher. It also leaves
  every `use "./lib/utils.nx"` example in the design doc correct as written.

#### Shape: one crate, and a `Host` trait so the language is host-tested

`userspace/nxsh/`, lib + bin, following `fs-server-ext4`'s split exactly:

- **`src/lib.rs` and friends — the language.** Lexer, parser, AST, evaluator, `Value` operations,
  generic operators, the regex engine. `no_std` + `alloc`, **no syscalls**, 100% host-tested.
- **`src/main.rs` — the host.** `_start`, the syscall plumbing, spawning stages, wiring pipes via
  `libstream::setup`, the console line-reader.

The seam is a **`Host` trait** covering everything the evaluator does that touches the OS — spawn a
stage, open a path, read a stream, resolve a name. This is the same move that made the ext4 parser
testable behind `BlockReader`/`BlockWriter`, and it is worth more here than there: an interpreter
is mostly pure logic, and pure logic tested on the host is tested in a second rather than a
90-second boot. The mock host also makes pipeline *semantics* (ordering, backpressure, error
propagation) testable without a kernel.

#### Ordering rationale

Parse **all** of §8/§9 in Part A, then let evaluation catch up. The alternative — finish the whole
language in-process, wire the process boundary last — layers more cleanly on paper but puts the
riskiest work at the end. The process boundary is where Milestones 1 and 2 both produced their real
surprises (the Tier-0/Tier-1 split, the fs-server ABI gaps, the session-reclaim flake), and it is
the part with the least precedent to lean on.

---

#### Decisions owed, with proposed resolutions

These are gaps the design doc leaves open that the *implementation* cannot leave open. Each is
resolved in the part that first needs it; the proposals here are the starting position, not a
settled answer.

**D1 — word mode vs expression mode. The big one, and it must be settled in Part A.**

§8b says `filter size > 1000` desugars to `filter { |it| it.size > 1000 }`, so `size` is an
expression. §5b says `list --long /some/path` passes barewords to an external program, so
`/some/path` is a string. These cannot both be parsed by one rule:

- `README.md` as an external argument is a filename; as an expression it is field access on
  `README`.
- `/system` as an external argument is a path; as an expression it is a division operator with no
  left operand.

**Proposal: the head token's category selects the argument grammar, resolved at parse time.**
§3's four-way categorization is already a parse-relevant distinction, not just documentation. The
parser holds the closed set of keywords, shell-state builtins and generic operators, plus the
`def`s visible in the file (which are hoisted per §5a, so they *are* known at parse time). A head
token in that set takes **expression-mode** arguments; anything else is an external program and
takes **word-mode** arguments, where barewords lex as strings and operators are not special. `^`
forces word mode (§3).

Residual risk to watch: §5c says the generic-operator category is deliberately *open* — a
user-defined `def` doing generic dispatch gets the same capability. That stays true semantically,
but such a `def` is called with the parens convention (§5b), which is syntactically distinct, so
the open category does not need to be open *at parse time*. If that turns out to be wrong, this is
the decision to revisit first.

**D2 — newline policy.** The grammar is newline-delimited with no semicolons (§9a), which the
design doc states but never specifies. Proposal: a newline terminates a statement, **except**
after a trailing `|`, `&&`, `||`, `,` or an open delimiter, and **except** when the next
non-comment token is `|` — the last being what makes leading-pipe style parse in a file, which
§11b asserts but does not explain. Inside `(`/`[` a newline is plain whitespace.

**D3 — path and regex literals share a lexer hazard.** Both start with `/` in prefix position:
`list /system` and `name ~= /\.rs$/`. Proposal: **a regex literal is lexed only immediately after
`~=`** — the only operator that takes one — and a prefix `/` anywhere else begins a path word.
`./` and `../` are unambiguous already, since `.` is otherwise strictly infix. This is the same
shape of fix JS uses for regex-vs-divide, but with a far narrower trigger.

**D4 — command resolution order.** Proposal: keyword → shell-state builtin → generic operator →
`def` in scope → external program, with `^` skipping straight to external. Resolution failure is a
fail-loud error naming what was searched, not a silent fallthrough to "command not found".

**D5 — recursion depth.** A recursive-descent parser and a tree-walking evaluator both recurse on
user input, on a userspace stack. Both need an explicit depth bound with a clean error, the same
discipline `MAX_TREE_DEPTH` applies in the coreutils. A deeply-nested expression must not be a
stack overflow.

**D6 — env (B3), and its `/session/*` coupling.** Due in Part E. The design's anchor is "env vars
as namespace-scoped resources" (§5a). Milestone 2 Part E found this is *the same problem* as
`/session/*` metadata, one milestone earlier: mutable, namespace-scoped values behind a path.
`TODO(session-metadata-server)` records the coupling. **Design B3 with `/session/*` in view and
migrate it onto whatever B3 builds** — two mechanisms and two migrations is the outcome to avoid.

**D7 — a script's exit status.** Proposal: the status of the last statement if it is a pipeline,
`0` otherwise; an uncaught error exits non-zero with the error rendered on `stderr`. Needs stating
because `nxsh script.nx` will be adjudicated by `test-qemu` exit codes.

**D8 — auto-display differs by mode** (§11e): the REPL appends `| display` to an unassigned
top-level pipeline; a script discards it silently. One flag on the evaluator, decided in Part C and
honoured from then on, not bolted on at Part F.

---

#### The parts

- [x] **Part A — lexer, AST, and parser (all of §8/§9)** ✅ (2026-07-30)
      Tokens including path words, regex literals, `#` comments, `1_000_000`/`0x`/`0b` numerics.
      Full expression grammar with §8a precedence, statement grammar, patterns, `type_expr`.
      Resolves **D1**, **D2**, **D3**, **D5**. Host tests: a corpus of every example in §7–§11 of
      the design doc must parse, plus fail-loud cases for each ambiguity D1–D3 names.
      *Deliverable: the design doc's own examples parse. Nothing evaluates yet.*

- [x] **Part B — evaluator core, in-process** ✅ (2026-07-30)
      `Value` (already complete in `libstream::wire` — `List`/`Record`/`Table` are `Arc`-shared and
      persistent, so C2 is genuinely done), scopes, `let`/`mut`/`const`, assignment and
      field/index mutation (§9d), arithmetic/comparison/logical/range operators, `++`, blocks as
      expressions, `if`/`for`/`while`. Float formatting (C6) lands here.
      *Deliverable met: 85 host tests, and the binary parses + evaluates a mixed
      Int/Float script in ring 3, checking its own result so a wrong answer fails the
      boot.* Two decisions landed here that the design implies rather than states: **there
      is no truthiness** (a non-`Bool` condition is an error, per §6's fail-loud rule), and
      **overflow and division by zero are errors, not wrapped values or `inf`** — a
      fabricated number is the thing this system keeps refusing to produce.

- [x] **Part C — the process boundary. First real script.** ✅ (2026-07-30)
      Pipelines: spawn a stage per external command, wire `Streams` via `libstream::setup::pipe`,
      stream TSM1 between them, collect `StageStatus`/`PipelineStatus` (§1). `stderr` routed
      separately from the pipe. Early-consumer cancellation via `PeerClosed`. `strict { }` blocks
      terminating remaining stages through handles the shell already holds. Resolves **D4**, **D7**,
      **D8**.
      *Deliverable met: `nxsh -c 'let t = list /system'` spawns the program, wires a pipe,
      reads its TSM1 stream and indexes the resulting table, in guest.* Two findings.
      **No coreutil reads `stdin`** — §10a dissolved every classic filter into an
      in-process operator, so with the shipped programs a pipeline has *at most one*
      external stage, always at the head; §5c's "one process boundary" is stronger than it
      reads. And **per-stage `PipelineStatus` attribution needs an ABI change**: spawn
      returns a handle, `ChildExited` carries a pid, nothing maps them, and `sys_wait`
      does not take a process handle. Exact for one stage; filed as
      `TODO(pipeline-stage-attribution)`.

- [x] **Part D — generic value operators** ✅ (2026-07-30)
      `filter`, `sort`, `select`, `take`, `last`, `skip`, `dedupe`, `each`, `map`, `count`,
      `display`, `format`, `expect`, `assert`, `save`, `open`. Generic dispatch over `Value` shape
      (§5c) — these run in-process, so the dense middle of a pipeline costs no spawns.
      **B5 lands here as `.tsm` + `.txt` only**; `.csv`/`.json` are separate, later, and neither is
      free. Ascription/`expect` checks a §6 schema against a TSM1 header — once, at header-read
      time, not per row.
      *Deliverable met, in guest against a real `list /system`.* One finding worth
      carrying: **an operator's bareword argument means different things per operator**.
      `sort size` names a *column* (§8b's reading, where a bare name is a field on `it`)
      while `display files` names a *binding*. A single global rule gets one of them
      wrong — both were tried — so arguments are kept unevaluated and each operator
      decides. Part D also needed **closure invocation**, since §8b's sugar *is* a
      closure; Part E extends the same primitive with `def`'s named args and defaults.

- [x] **Part E — functions, closures, match, null handling, modules** ✅ (2026-07-30)
      `def` with defaults evaluated per call, variadics, named args, the `_` pipeline-fill rule
      (§5b); closures capturing by value at creation; `def` hoisting for mutual recursion;
      `match` with guards, or-patterns, ranges, `@` capture, record subset patterns (§9f);
      `T?`/`?.`/`??` (§9e); `try`/`catch` and `?` propagation (§2); `use` imports (§9h). Resolves
      **D6** (env).
      *Deliverable adjusted, and the adjustment is a finding.* §7 **parses** verbatim (a
      Part A test) but cannot *run* verbatim: it calls `validate_schema`, which nothing
      defines, and reads `line.level` from a file `open` turns into a single-column
      `Table<String>`. It is illustrative pseudo-code, not a conformance suite. Every
      construct in it is exercised instead, in guest and on the host.

      Three findings. **A `def` must see other `def`s while capturing no `let`s** (§5a) —
      declarations and values are different things, so hoisted functions live in their own
      always-visible scope. **`f()` is a call, not a mention** — an empty argument *list*
      was hitting the bare-name shortcut and evaluating to the function itself. And **`try`
      is a statement, not an expression**, so `let x = try { … } catch { … }` does not
      parse; filed as `TODO(try-in-expression-position)`.

      **D6 (env) is not resolved here** — see the entry below.

- [x] **Part F — minimal REPL** ✅ (2026-07-30)
      A line-reader on the raw console: read, parse, evaluate, print. Automatic continuation on the
      *provable* cases only (unclosed delimiter, trailing `|`) per §11b. `$last` (§11d), `cd`/`exit`
      builtins, auto-display. **No** reverse-search, Shift-Enter, completion, or job control —
      those are the deferred rich REPL below, gated on the console/tty server.
      *Deliverable met for the prompt; the `usersh` swap is deliberately **not** taken
      here.* `nxsh` needs `cd` before it is a better login leaf than the throwaway it would
      replace, and `cd` turned out to be a design question rather than a builtin — a
      shell-side position would apply to the shell's own lookups and silently not to the
      programs it spawns, which each resolve `argv` in their own namespace. Filed as
      `TODO(shell-cwd)`; likely lands with B3, since "what a child inherits about where it
      is" and "what it inherits about its environment" are one question.

      Continuation is decided by **lexing, not counting** — a brace inside a string or a
      comment opens nothing, and getting that wrong hangs the prompt on a finished command.

- [x] **Part G — the regex engine and `~=`** ✅ (2026-07-30) — B4 closed
      Predicate-only, since §10b never asks `~=` for anything but a boolean — which removes
      submatch extraction, the largest source of complexity in a regex implementation. Pattern
      parser → instruction program → **Pike VM** (Thompson NFA): linear time, no backtracking, no
      catastrophic blowup.
      **Supported:** literals, `.`, `*`, `+`, `?`, `|`, `()`, `[abc]`/`[a-z]`/`[^…]`, `^`/`$`,
      escapes, `\d`/`\w`/`\s`. **Rejected loudly at compile time:** backreferences (they are
      precisely what would force backtracking — excluding them is what *permits* the Pike VM, the
      same call Go's `regexp` and RE2 make), `{n,m}`, lazy quantifiers, lookaround.
      The property that matters: **no pattern's meaning ever changes when the engine grows.** Every
      excluded construct is an error today, not a silently-different match — which is why literal
      substring matching was rejected as a starting point.
      *Deliverable met, in guest.* One bug worth recording: the first compiler let a
      fragment's exit be patched by **overwriting** the instruction slot with a `Jump`,
      which destroyed the `AssertStart` it was patching — so `^bc` became `Jump; b; c` and
      matched anywhere. Instructions now carry their successor explicitly, making an exit a
      *field to fill* rather than an instruction to clobber. It passed several simpler
      tests first, which is why anchors and the pathological-pattern case both earn their
      place.

**Milestone 3 is complete** (Parts A–G). `nxsh` parses and evaluates the language, spawns
and pipes real programs, runs the generic operators in-process, and matches with `~=`. What
it does **not** do is `cd` — see Milestone 3.5 below, which is where the `usersh` swap
also lands.

#### What Milestone 3 does not do

`.csv`/`.json` for `save`/`open` (B5 beyond `.tsm`/`.txt`), the rich REPL (§11), job control,
schema-aware completion, and everything in §12. `usersh` stays the login leaf until Part F proves
`nxsh` in-guest; switching `session-mgr` over is the last step, not the first.

### Milestone 3.5 — what a child inherits (B3 + `cd`)

**Sequenced after Part G, and designed with the maintainer 2026-07-30.** B3 was nominally
Milestone 3 scope ("resolve as it comes up") and never came up. Three items filed
independently — B3 (env), `TODO(session-metadata-server)` from M2's `whoami`, and
`TODO(shell-cwd)` from Part F — turned out to be one question asked from three directions:
**what does a child inherit, and how is it named?**

The design below is settled; what remains is building it.

#### The constraint that decides most of it

`sys_process_spawn`: *"The child **always** gets a **LOOKUP-only** handle — it resolves
names but cannot rebind its root."* Unconditional, in the kernel. **A process that is not a
supervisor can never bind into its own namespace**, and a login shell is spawned with
`syscaps: 0` precisely so it is not one.

So `cd` as "rebind `/env/PWD`" is structurally impossible, and so is any shell-mutable
namespace entry. That is not a policy to argue with; it is the capability model working.

#### There are two spawn contracts, and only one of them is scary

- **`SpawnArgs`** — kernel ABI. `#[repr(C)]`, 104 bytes, offset-asserted, mirrored in
  `libkern`, covered by `abi-sync-check` and the ABI version hash. Changing it is a real
  ABI event.
- **The Tier-1 setup message** — [`pipeline-stdio.md`](../spec/pipeline-stdio.md). Pure
  userspace TSM1, already carrying `streams` and `argv`, invisible to the kernel.

Env and cwd belong in the **second**. They are in exactly the company they belong in: the
channel that already answers "what does this stage start with".

#### The line: capability or configuration

Not "session state vs process state" — that does not predict the answer. The line that does:

- A **namespace binding** is how you hold something *unforgeable*. A process cannot
  fabricate one because it cannot bind at all. That property costs a lookup, and is worth
  paying for things that must not be lied about.
- The **setup message** is how you hand over *data*: cheap, explicit, snapshot.

So `/session/user` stays in the namespace — not because it is session-scoped, but because
it is **identity**. If `USER` were an env string, any process could hand a child a
different one and the child could not tell. `PATH`/`EDITOR`/`LANG` are configuration, and a
process handing its child a different `EDITOR` is not an attack — it is the point of env.

#### Why the cheap option's semantics are also the right ones

*Snapshot is correct, not a compromise.* Unix copies env at exec; changing yours does not
reach running children, and everything relies on that. The namespace's late binding is a
feature nobody wants here.

*§5a's reasoning survives even though its mechanism does not.* §5a objects to **implicit**
inheritance — a child silently getting whatever the parent had. A setup-message field is
explicitly constructed by the parent, per spawn, visible in the contract. Same legibility,
no IPC. **This diverges from §5a's literal "env vars as namespace-scoped resources" anchor
and is recorded as a divergence, not drift** (decision log, 2026-07-30).

*And the IPC cost was never about the namespace.* `/session/user` is a direct-handle bind
today: `lookup` + `map`, no server, no round trip. What makes `/session/*` expensive is the
migration to a userspace server, whose documented trigger is the first **mutable** member.
Env is mutable — so putting env there would not merely pay that cost, it would *create* it,
for everything else under `/session` too.

#### Env is a TSM1 `Record`, not `key=value`

The setup payload is already a TSM1 Record, so this is one more field in a record that
exists — not a new encoding. What it buys:

- **`PATH` as `List<String>`** rather than a colon-joined string, which removes the
  colon-splitting bug class outright (a path *containing* a colon is unrepresentable in the
  Unix form, and every parser disagrees slightly).
- **Types are part of the contract**: a program expecting `String` and given `List<String>`
  gets §6's schema diff, not a silent misparse.
- **The shell needs no new machinery at all.** §6's "`Value` is exactly what TSM1 can
  represent" means env is an ordinary value, so `display $env`, `$env.PATH`,
  `$env.PATH | count` and `env.EDITOR = "vi"` all fall out of Part B's field assignment and
  Part D's operators. The see-and-manipulate story is already built.

The universality argument for `key=value` — any language can parse it — does not apply:
every program here already links `libstream`.

**`cwd` is a conventional entry in that record**, as `PWD` is in Unix — and it is *safer*
here. Unix has two sources of truth, `$PWD` and `getcwd()`, and the interesting bugs live
in the gap. Nitrox has no kernel cwd, so the entry is the truth by definition and has
nothing to disagree with. `cd` must **verify the path resolves before setting it**, so the
invariant is "PWD named something real" rather than "PWD is whatever you typed".

#### Relative paths: a library wrapper, not a kernel change

The kernel rejects relative paths outright — `validate_path` refuses a leading non-`/` and
refuses `.` and `..` by name. That stays. A wrapper in `coreutils::fs`/`libos` expands
relative → absolute against `cwd` before any syscall, and every program already routes
through those helpers (`lookup_wait`, `Dir::open`), so the convention has one enforcement
point rather than being per-program discipline.

Lexical `..` normalisation is *correct* here, unusually: it is wrong in general because of
symlinks, and Nitrox has none (the fs-server rejects them).

**Known gap this closes:** `open ./data.csv` — used throughout design §4 and §7 — does not
work in guest today. Part D's test for it passed against `MockHost` and the in-guest demo
used absolute paths, so the claim went untested.

#### Carried constraints

- **~3.9 KB.** `IPC_PAYLOAD_SIZE` caps the whole setup payload including `argv`, and a
  Record encodes its schema alongside its values. The escape is a memory-object handle
  holding the block — which is how Unix does it, and the setup message already transfers
  handles for the streams.
- **Decode compatibility — confirmed, not assumed.** `SetupPayload::decode` reads
  **positionally** and guards with `record.values.len() < 2`, so appending a third field is
  backward-compatible by construction: an existing stage reads `values[0]`/`values[1]` and
  ignores the rest. Appending is safe; *reordering* or *inserting* would not be.

#### What this does to the filed items

- **B3** — resolved as above.
- **`TODO(shell-cwd)`** — resolved as above.
- **`TODO(session-metadata-server)`** — **shrinks**. It is now about tty and job state:
  things that are *handles* and genuinely shared-mutable. Env leaves that story entirely,
  which also removes what would have forced the server migration.

#### Build order

The design above is settled. This is the order to build it in, sized so each part ends
verified and committed on its own — the same slice convention Milestones 2 and 3 used.

Two facts established by inspection, which shape the steps:

- `SetupPayload::decode` reads **positionally** with a `len() < 2` guard, so **appending**
  `env` is backward-compatible — every existing stage keeps working untouched.
- `session-mgr` spawns the shell with `arg0: 0`, i.e. **Tier 0**. Giving the shell an env
  means moving that spawn to Tier 1, which is a real step rather than a detail.

#### Part A — `env` on the wire ✅ (2026-07-31)

- [x] Append `env: Record` to `SetupPayload` (append only — reordering breaks the
      positional decode).
- [x] `send_setup` takes it; `Setup` exposes it. Absent field ⇒ empty Record, so a sender
      that does not set env is not a special case.
- [x] Update [`pipeline-stdio.md`](../spec/pipeline-stdio.md): the payload's third field,
      and the statement that Tier 0 has no env *because* it has no setup message — the
      same reason it has no `argv`.
- [x] Host tests in `libstream`, including **an old-shaped payload decoding under the new
      decoder** — the compatibility claim above, asserted rather than reasoned about.
- [x] Refuse a payload over `IPC_PAYLOAD_SIZE` with a message naming the limit, not
      `SinkFull`.

*Deliverable met.* Two notes for the parts that follow. `send_setup_env` is a **separate
entry point** rather than a changed signature, so every existing spawn site stayed correct
without edits — a spawn with no environment to pass is not obliged to say so. And the size
check lives in `SetupPayload::encode`, not at the send site: the limit is a property of the
message, which keeps it host-testable and gives every sender the same refusal.
`SETUP_PAYLOAD_MAX` is defined in `libstream` rather than imported, because the wire core
deliberately has no dependencies; a `const` assertion under the `io` feature makes drift
from `IPC_PAYLOAD_SIZE` a compile error.

#### Part B — relative paths resolve ✅ (2026-07-31)

- [x] `coreutils::fs::resolve(cwd, path) -> String`: absolute passes through; relative
      joins; `.` drops; `..` pops lexically.
- [x] `..` above the root is an **error**, not a silent clamp to `/` — a path that escapes
      is a mistake worth hearing about, and clamping would let `../../..` mean `/`.
- [x] Route the existing helpers through it (`lookup_wait`, `Dir::open`, `create_file`,
      `rename`), so the convention has one enforcement point rather than per-program
      discipline.
- [x] Host tests: absolute, `./x`, `../x`, `a/../b`, escape-above-root, and a path that is
      exactly `.`.

*Deliverable adjusted, honestly.* The mechanism is in and fully host-tested, but nothing
sets `PWD` until Part D — so `open ./data.csv` cannot yet *succeed* in guest, and claiming
otherwise would be claiming a test that does not exist. What Part B asserts in guest is the
other half: a relative path with no working directory **fails loud** with a usage error
rather than being resolved against an arbitrary root, because guessing `/` would make
`remove ./x` delete something in a directory the caller never named.

Two placements worth carrying forward. The resolver lives in **`librsproto::path`**, not
`coreutils::fs` — `nxsh` needs it too and does not depend on `coreutils`, and `librsproto`
is the one crate both already share. It is **buffer-based** because `librsproto` is
`core`-only with no `alloc`; that constraint turned out to suit the problem, since
resolution is a fold and `..` is a truncation, so it needs no component stack at all.
Resolution happens **once, where a path enters from `argv`** (`Stage::path`), not threaded
through every filesystem helper — one place per program, which is also the right place to
report a bad path.

#### Part C — `nxsh`: `$env`, `cd`, and passing it down ✅ (2026-07-31)

- [x] Read env from the shell's own setup message at startup; bind it as `$env` (a `mut`
      Record).
- [x] `cd PATH` as a shell-state builtin: **resolve first, set `PWD` only if it resolved**
      — so the invariant is "PWD named something real", not "PWD is what you typed".
- [x] `cd` with no argument goes to `HOME`, and says so if `HOME` is unset rather than
      silently doing nothing.
- [x] Pass `$env` in `send_setup` for every spawned stage, so a child inherits *explicitly*
      what the parent chose to hand it.
- [x] The prompt shows `PWD` (§11a already reserves the position).
- [x] In-guest demo: `cd /system` then `list .` and `open ./x` **agree** — the split-brain
      the whole design exists to prevent, asserted directly.

*Deliverable met, in guest.* Three findings. **`$` had to become an identifier start
character** — Part F bound `$last` and nothing could name it, because the lexer could not
produce that token: a silent dead end. Fixing it exposed a second bug, that `scan_ident`
started at the first byte and so made no progress on a start character that is not also a
continue character — an infinite loop, which showed up as a hung test run rather than a
failure. And **a builtin's argument is a path, not an expression**: `cd ..` and `cd /system`
both choke on expression mode, where `..` is a range and `/system` a division, so builtins
take word-mode arguments like an external program does.

The shell **does not rewrite a stage's arguments**. It passes `.` through as written and
hands over the same `PWD`, so `list` resolves it in its own process against the same
directory. Pre-resolving would make the in-guest demo pass while proving nothing about the
half that can actually disagree.

#### Parts D and E — `session-mgr` seeds the session, and `nxsh` becomes the login leaf ✅ (2026-07-31)

**Merged, and the merge is a finding.** The plan separated them, assuming Part D's
deliverable — "a login session starts with env set" — was observable on its own. It is not:
nothing consumes an environment until the shell that reads it *is* the login leaf, and
proving D against `usersh` would have meant writing verification into a program about to be
deleted.

- [x] Build the initial env Record: `HOME`, `PATH` (as `List<String>`), `PWD` = home.
- [x] **Not `USER`.** Identity stays at `/session/user`, unforgeable because a process
      cannot bind at all. Copying it into env would hand that away for nothing.
- [x] Move the shell spawn from Tier 0 to Tier 1.
- [x] `session-mgr` spawns `nxsh` as the login leaf.
- [x] `usersh` removed, in the commit that proved the replacement.
- [x] `userspace/session-mgr/CLAUDE.md` updated; `userspace/nxsh/CLAUDE.md` written.

**A documented constraint had to be lifted, and it was the maintainer's call.**
`session-mgr` was `no alloc` — fixed `.bss` buffers, no `#[global_allocator]` — because "it
is a supervisor (its death is a system fault), so keep it minimal and robust". But handing
a child an environment needs a heap at every step: a `Record` holds `Vec`s, `send_setup`
builds a `Vec<String>`, encoding returns a `Vec<u8>`. The alternative was a second,
allocation-free encoding path in `libstream` — a worse thing to own than an allocator, and
exactly the duplication this project keeps designing away from. `#![no_std]`/`#![no_main]`
stayed: `std` is unported and there is no runtime to hand a `main`.

**The login proof is now a script**, which is a stronger proof than the one it replaced:

```
if $env.PWD != $env.HOME { bad }
[1, 2] | save ./nx-login.txt
if (open ./nx-login.txt | count) != 2 { bad }
```

The environment arrived, a relative path resolved against it, and home is writable — three
properties `usersh`'s hardcoded home-write could not distinguish between.

*Deliverable met: a login lands in the real shell, with a real environment.*

#### Decisions owed at build time

Small, but they should be made deliberately rather than by whoever types first:

- **Does `cd` accept a path that resolves to a file?** Refusing is consistent with `touch`
  refusing a directory; accepting silently would make every later relative path fail.
- **Is `PWD` writable directly (`env.PWD = "/x"`), or only through `cd`?** Direct writing
  bypasses the resolve-first check, which is the only thing keeping `PWD` honest.
- **What does a stage do with an env entry whose type it did not expect?** §6 says schema
  diff and fail loud; worth confirming that is what actually happens rather than a
  `SchemaMismatch` with no detail.

#### Not doing yet, deliberately

Plain bullets, not checkboxes: these are decisions to *not* build, not work owed.

- **Env larger than the payload.** The escape is a memory-object handle holding the block,
  which the setup message can already transfer. Not built until something needs it — and
  Part A's explicit refusal is what makes the limit visible rather than mysterious when it
  arrives.
- **`/env` as a namespace binding.** The Record already gives see-and-manipulate inside the
  shell; a binding would buy inspecting *another* session's env, and nothing wants that
  yet.

#### Unblocks

`nxsh` replacing `usersh` as the login leaf, which Part F deliberately left untaken.

#### Part F — the session can reach its programs ✅ (2026-07-31)

Parts A–E gave a login a real shell with a real environment, and driving one interactively
found the thing that made it a *toy*: every external command failed with "`list` is not a
program". The session namespace held the user's home and `/dev/console` and nothing else.

The fix is namespace construction, not a shell change, and the shape was a real decision.
Binding `/initramfs/sbin` would be one line and wrong — it hands a session the boot image
rather than a profile, and "absence is the sandbox" stops meaning anything once every
session sees every binary. So: the profile server's `/bin`, which is the design's intended
answer (`docs/architecture/profiles-and-namespace-projection.md`).

**System profile only.** Per-user profiles are the eventual shape, but nothing yet needs two
users to see different programs, and building the projection before the requirement would be
guessing at it.

**Step 1 — package the coreutils** ✅ (2026-07-31)

The binding is useless while the profile projects nothing a shell can run: before this,
`/bin` was bound and contained exactly `heartbeat`. The coreutils existed only in the
initramfs.

- [x] One `coreutils` store package holding all ten coreutils **plus `nxsh`** — a shell
      that cannot invoke itself is a strange thing to hand someone.
- [x] A second `[[package]]` in the generated system-profile manifest.
- [x] The package hash covers **every** ELF in it, not the first. A content-addressed path
      that moves when one of eleven binaries changes is worth something; one that tracks
      `list` alone would let the other ten change under a path claiming they had not.
- [x] One list (`COREUTILS` + `profile_programs()`) feeding the build, the initramfs, and
      the store, so a new coreutil cannot be built-but-unreachable or packaged-but-unbuilt.
- [x] `bin_projection_demo`: all eleven resolve through the real forwarding chain, and an
      unknown name still misses.

**The negative control is the load-bearing half.** "`/bin/list` resolved" says nothing about
projection unless some `/bin/<name>` fails to resolve — a `/bin` answering everything would
pass the positive check while projecting nothing. Verified the demo is not vacuous the only
way that counts: removed the `coreutils` entry from the manifest, confirmed the run fails at
"a profiled program did not resolve on /bin", and restored it.

**Step 2 — hand the endpoint down and bind it** ✅ (2026-07-31)

- [x] init keeps a duplicate of the profile-server endpoint *before* binding `/bin`, so a
      failure is a failure to bind rather than a bound `/bin` no session can be given.
- [x] init → service-mgr → session-mgr; `build_session_namespace` binds it at `/bin`,
      whole-tree, sharing init's registration as `/home` shares the fs-server's.
- [x] The login proof script runs `list .` — not a builtin, and the session holds no
      `/initramfs`, so only `/bin/list` can satisfy it.
- [x] Driven at a real interactive prompt: `list /`, `whoami`, `date`.

**The hand-down needed a channel, and that is the one contract this touched.** Only
`handles[0]` reaches a child — the kernel seeds `rdx` with it, and there is no register
left for `handles[1]` nor any documented way to learn its handle value. Rather than invent
one, service-mgr's `rdx` became a **handoff channel**: the mechanism the boot chain already
used one link further down, where service-mgr hands *its* children endpoints over a control
channel. A third endpoint is now one more `send_handle`, not another ABI question.

A zero handle sends an **empty message** rather than nothing, at both links. The receives
are positional, so a skipped send would shift every later handoff up a slot and quietly
hand session-mgr the auth channel where it expects the profile endpoint.

Verified the bind is load-bearing by removing it: the run fails with
"``list`` is not a program" — the same sentence a real user hit at the prompt. That run
also caught a log line announcing a `/bin` that was not there; it now reports what it
actually bound.

**Step 3 — `list /` did not parse** ✅ (2026-07-31)

Not planned work. It surfaced one command after step 2 made programs reachable, which is
the point: the first thing anyone types at a new shell is `list /`, and it died with
"expected an expression".

A lone `/` after a command head was read as division. `/system` was never affected — it
lexes as a single path word — so every existing test passed. The rule is decidable rather
than a preference: a lone `Slash` in argument position means what follows is whitespace or
a closer, so there is no right operand and division is *impossible*.

- [x] `Lexer::no_operand_follows` — lookahead that deliberately does not skip newlines.
- [x] `starts_an_argument` reads a spaced `/` with nothing after it as the root path.
- [x] Host tests for both halves, and an in-guest pair: `list /` **and** `6 / 2 != 3`. A
      rule that made every `/` a path would pass the first and break arithmetic.

*Deliverable met: a login session can run the programs its profile gives it, and only
those. `list /` at the prompt shows `home`, `bin`, `session`, `dev` — the four bindings
the session was built with, and nothing else.*

### Milestone 4 — language completeness (planned 2026-08-04)

**Where this came from.** Not a feature wish-list: an audit of the *built* language against the
design doc, run by driving the interpreter rather than reading the grammar — which is how items
like "§6's own examples do not parse" surfaced at all. The design decisions it produced are
recorded in `nitrox-shell-design-v1.2.md` (§2, §6, §8c/§8e, §9c, §10b, §11f, §11h); **this section
is the build order, not the design.** Read the design first; the parts below assume it.

**The through-line is two sentences.** The language could *test* values but not *transform* them —
every conversion ran toward text and nothing consumed one — and it could not *finish a loop*, since
`return` was the only early exit and it leaves the whole function. Everything here is one of those
two, or a mechanism the design already specified that turned out to be unreachable from where it
was meant to be used.

**Ordering rationale.** A and B first: they are the two that make scripts *impossible* rather than
awkward, and B is what lets a number read from a file be a number. C next, before the operator
families, so that every operator added afterwards raises errors in the final vocabulary rather than
being revised into it. D before E because E is breadth on the substrate D lays. F is isolated
because it is the only part with real engine work behind it. G is last: it carries the only kernel
dependency in the milestone, and A has by then removed the commonest way to need it.

Two things hold for every part and are not repeated in each:

- **The library half stays syscall-free and host-tested.** Anything spanning two prompt lines uses
  the `repl(&[…])` helper — three interactive-only bugs got in before it existed (`userspace/nxsh/CLAUDE.md`).
- **Each part ends green on all four gates**: `cargo xtask test`, `test-qemu`, `test-interactive`,
  and the `check-*` gates. A part that changes the grammar re-runs the in-guest pair that pins
  `list /` against `6 / 2` — the two only mean anything together.

Parts, in order (tick as they land):

- [x] **Part A — `break` / `continue`** ✅ (2026-08-04)
- [x] **Part B — `parse T`, and `expect`/`assert`/`parse` as pipeline stages** ✅ (2026-08-04)
- [x] **Part C — errors: `fail`, the `kind` vocabulary, `e.stages`, `?` retired, `exit N`** ✅ (2026-08-04)
- [x] **Part D — sequences generalise, and reduction** ✅ (2026-08-04)
- [x] **Part E — strings, records, numbers, `in`** ✅ (2026-08-04)
- [x] **Part F — `capture`: regex submatches** ✅ (2026-08-04)
- [x] **Part G1 — interrupt (`Ctrl-C`) in the shell** ✅ (2026-08-04)
- [x] **Part G2 — `sys_process_terminate`** ✅ (2026-08-04) — and `Ctrl-C` now reaches a running stage

**Expect each part to surface substrate gaps, and file them rather than paper over them** — the
rule Milestone 2 earned. Part G already has one before it starts (below).

#### Part A — `break` / `continue` ✅ (2026-08-04)

**Landed.** New tokens, two statement forms (§9c), and a third and fourth variant beside
`Normal`/`Return` in the evaluator's `Flow`. `for` and `while` consume them; every other construct
propagates them the way it already propagates `Return`.

Three things it turned up that the plan did not predict:

- **A new statement-ending token must join `Tok::ends_statement`.** It is a whitelist of *enders*,
  so anything new defaults to "continues": the lexer swallowed the newline after `break` and the
  parser reported "expected a newline between statements" on the line *after*. The default is
  right — a wrong "ends" splits one statement in two silently — but the symptom points one line
  past the cause. Recorded in `nxsh/CLAUDE.md`.
- **Control flow cannot leave an expression**, because `eval` returns a value and has no channel
  to carry `Flow` back through. `break` in expression position is refused with the
  statement-position form named in the message; the same wall makes `return` there *silently*
  yield instead of leaving the function, which predates this part. Filed together as
  `TODO(control-flow-in-expression-position)` — watch it at Part C, where `try` becomes an
  expression.
- **The runaway backstop had no test**, and proving it at ten million iterations costs more
  wall-clock than the whole suite. The limit is now a field on `Interp` (still `MAX_ITERATIONS`
  everywhere else), so `while`'s guard and the range guard both got their first coverage, in
  microseconds.

- **The loop-only and closure-boundary rules are one mechanism**: the parser tracks loop depth and
  **resets it on entering a `def` or closure body**. That reset *is* "`break` does not cross a
  function boundary" — one line in the parser instead of a runtime check in the wrong place, and it
  makes both errors parse-time, which is the better of the two available diagnostics.
- **The trap is `Flow`**: every `match` on it is a place a `break` can be silently swallowed.
  `exec_block`, `scoped_block`, the loop bodies and `call_function` all need to be revisited
  together, not one at a time.
- Tests: `break`/`continue` in both loop forms; inside a nested `if`; `break` inside
  `filter { |it| … }` is a parse error; `break` at top level is a parse error; and a `def`
  containing a loop containing a `break` still returns normally.

#### Part B — `parse T`, and the keyword stages ✅ (2026-08-04)

**Landed, and the parser change turned out not to be the one the plan predicted.**

One parser change carries both halves: `pipeline_stage` admits `stage_keyword` (§8c), which is what
makes `expect`, `assert` **and** `parse` legal mid-pipeline. `expect`/`assert` were specified that
way in v1 and never worked; `parse` is new and would have had the same problem on day one.

- **`parse` reuses the lexer's numeric scanner** rather than carrying a second one, so "what a
  number looks like" has exactly one definition (§8e's `int_lit`/`float_lit`, hex/binary/`_`
  included). Factor the scanner out; do not reimplement it.
- Strict about surrounding whitespace, deliberately (§6) — `trim` arrives in Part E, so say so in
  the error message until it does.
- **Ordering note:** `ParseError` as a `kind` lands in Part C. Until then a parse failure is an
  ordinary error with the right *message*; Part C gives it the right *kind*.
- Tests: **§6's own examples, verbatim** — they are the thing that did not run, so they are the
  regression. Plus round-trips (`42 | format("{}") | parse Int`), and each refusal:
  `" 42 "`, `"abc"`, `"1.5" | parse Int`, `parse Bool` on `"yes"`.

**What was actually wrong, and it was not the pipeline rule.** The parser had accepted `expect`
in stage position since Milestone 3 — it even builds `Expr::Expect(Underscore, T)` there, which is
the shape a stage needs. **The evaluator** was refusing it: any non-`Call` stage past the head got
"a value cannot be a pipeline stage". So the fix landed in `pipeline()`, not in the grammar, and
the `stage_keyword` production added to §8c documents what the parser already did.

The mechanism is `_`: the operand is bound to it for the length of the stage, so
`Expr::Underscore` picks it up with no separate operand channel. Two consequences worth keeping:

- **`assert` had to stop clearing `head_ok`.** §6 writes `ls | assert (count > 0)`, and with a
  command head refused inside the predicate, `count` parsed as a plain identifier and died as "not
  a binding, and running it as a program failed". D4 still puts a local binding first for a bare
  argument-free name, so `assert (n > 0)` over a `let n` is unaffected — which is its own test.
- **`assert` passes its value through** in stage position. §6 puts it in the same slot as `expect`;
  returning `Null` (what the expression form correctly does) would end every chain containing one.

**`parse` reuses the lexer rather than sharing a factored-out scanner.** It runs `tokenize_expr`
over the text and accepts a lone `Int`/`Float` token, so "what a number looks like to `parse` is
what it looks like to the lexer" is literally true — radix prefixes, `_` separators, exponents and
the no-octal rule all arrive without a second implementation, and cannot drift. The sign is handled
in `parse` because a literal is unsigned in the grammar, which also means `parse Int` cannot read
`i64::MIN` — exactly as no literal can write it.

#### Part C — errors: `fail`, kinds, `e.stages`, `?` retired, `exit N`

The part that makes the error path a designed surface rather than whatever the interpreter happened
to produce.

- **`fail <String>` / `fail <Record>`**, and one place that constructs an error record so the `kind`
  vocabulary (§2) cannot drift between raising sites.
- **`e.stages` on a failed pipeline**, and the undocumented `__status` binding is deleted with it.
- **`?` loses its postfix-expression form only** — two parser arms. **Nullable ascription (`Int?`,
  `Table<{…}>?`), the `size?: T` record shorthand, `?.` and `??` are untouched**; they are
  type-position or their own tokens (§2's table). A bare postfix `?` becomes a parse error naming
  §2 rather than silently accepting a no-op.
- **`try`/`catch` becomes an expression** — `let cfg = try { … } catch { default_config() }`. This
  is the recovery form that has to arrive *with* `?`'s retirement rather than after it, or the
  language spends a release with no way to default on failure in expression position. Closes
  `TODO(try-in-expression-position)`, which named this §9c pass as its trigger and the fix as "add
  `try` to `primary`" — and it is exactly that: `parse.rs` already has `Tok::If` and `Tok::Match`
  arms there and no `Tok::Try`.
  **Collapse to one node while doing it**: retiring `?` frees `Expr::Try`, so statement-position
  `try` becomes an `expr` statement and `Stmt::Try` goes away. Two forms of one construct is how
  the `run_line`/`exec_block` divergence happened.
  Tests: the binding case (`let x = try …`), a catch branch with no value yielding `Null`, `try` as
  a pipeline head, and — the one that would have caught the original gap — a `catch` whose value is
  used two lines later without a `mut`.
- **`exit` becomes a real builtin**: `Flow::Exit(status)`, default `0`. The interactive driver stops
  comparing the line against `"exit"` — after this, **the loop intercepts nothing**, and the
  standing warning in `userspace/nxsh/CLAUDE.md` about the driver being a second implementation of
  the language gets to be deleted rather than reworded.
- **Decide and record: `try`/`catch` does not catch `exit`.** `Flow::Exit` is control flow, not an
  error, so leaving is not something a `catch` can accidentally swallow. (Part G's `Interrupted`
  *is* catchable, deliberately, so cleanup runs — the asymmetry is intentional and belongs in the
  commit message.)
- Tests: every raising site's `kind`; `e.stages` after a two-stage failure names the stage that
  failed; `exit 3` really is the process's status (in-guest — this one cannot be host-tested).

**Three things the plan did not predict.**

- **`try` needed one implementation with two entry points, not one node.** Making it an expression
  would have silently broken `for x in xs { try { … } catch { continue } }` and a `return` inside a
  `try` body, because control flow cannot leave an expression (Part A's wall). `exec_try` returns
  `Flow`: `exec` propagates it, `eval` reduces it. Both directions are tests. This is exactly the
  bite `TODO(control-flow-in-expression-position)` said to watch for here.
- **`exit` travels the error channel.** It is not a failure, but that channel is the only one that
  already crosses a `def`, a closure and a loop — and `catch` re-raising it is a one-line rule that
  keeps "leave the shell" from being something a script prevents by accident. `exit 3`'s argument
  arrives as a *bareword* (a builtin's argument is not an expression), so it is read with `parse`'s
  scanner rather than a second one.
- **The bare-name fallback was burying failures.** D4 wraps a failed bare name as "`x` is not a
  binding, and running it as a program failed: …" — correct when the program is missing, wrong when
  it ran and exited non-zero, and it flattened the error, losing the `kind` and the per-stage report
  in the one case they exist for. It now propagates a `PipelineFailed` unchanged.

#### Part D — sequences generalise, and reduction ✅ (2026-08-04)

**Landed.**

- `rows()`/`rebuild()` accept a `String` (characters) and a `Range` (values), and rebuild in the
  shape they were handed (§10b). This is where length, substring and slicing come from.
- `sum`, `min`, `max`, `avg`, `reduce` (both forms), with the empty-input answers from §10b — each
  one forced rather than chosen, and each one a test.
- Two corrections ride along: **`sort` uses every key** it is given, and **`format` in stage
  position formats its operand**.
- **Trap:** `"abc" | count` currently *errors*, and after this it is `3`. Check nothing leans on the
  refusal. Characters, not bytes — `"abc"[0]` already indexes characters, and two answers to "what
  is an element of a String" is one too many.

**Notes from building it.** The trap was empty: the one test that pins the refusal uses `Val::int(5)`,
which is still a scalar, so it kept passing unchanged and now documents the narrower rule. Three
things worth recording:

- **`sort` and the reductions share one key extractor.** `sum size` and `sort size` ask the same
  question — "which column?" — so `key_of` answers it once, and a row with no such field fails the
  same way for both. Renaming it out of `sort_key` was the whole change.
- **The multi-key sort test only means something as a pair.** `sort d n` and `sort d` over the same
  input give *different* answers (`["z","a","b"]` vs `["z","b","a"]`, the second because a stable
  sort keeps input order within a tie) — and the single-key answer is exactly what the multi-key
  case would produce if the second key were being dropped. Asserting one without the other would
  have passed against the bug.
- **`format`'s operand goes in as argument 0**, right after the template, because in stage position
  the value flowing past *is* the subject: `… | format("{}")`.

#### Part E — strings, records, numbers, `in` ✅ (2026-08-04)

**Landed — and the "mostly mechanical" part found a user-reachable hang that predates all of
Milestone 4.** Breadth on Part D's substrate: `split`/`join`/`trim`/`replace`/`upper`/`lower`,
`keys`/`values`/`merge`, `round`/`floor`/`ceil`/`trunc`/`abs`, and `in` as an infix comparison
(§8a). The lexer already has `Tok::In`.

- **Trap:** `for x in xs` and `if x in xs` in the same script, as one test. The `for` rule consumes
  its own `in` before any expression is parsed (§8c), and that is exactly the kind of claim that is
  true until someone reorders a parser function.
- `upper`/`lower` are ASCII-only and say so where a user meets it, not only in the design doc.

**The trap was real, and it was worse than predicted.** The two `in`s do not collide — but `in`
became the **first operator that can follow a command head**, because `for x in xs` consumes its own
before any expression is parsed. `starts_an_argument` had no arm for it, so `x in [1, 2]` read as the
*program* `x` with word-mode arguments — and word mode on `[` **hung the lexer**: `]`/`)`/`}` are
structural, their openers are not, so `scan_bareword` scanned zero characters, left `pos` alone, and
the argument loop bumped the same empty `Word` forever.

That hang **predates Milestone 4 entirely**: `list [x]` locks the shell up on `main`, with no Ctrl-C
to escape it (§11h). Both halves are fixed — `in` continues an expression, and an empty bareword is
now a lexical error, which makes the hang impossible for *any* character rather than for the ones
somebody thought of. With the lexer fixed, the parser bug would have been a test **failure** instead
of a hung suite, which is the whole argument for the second fix.

Two other notes: `merge` needed the `{`-argument permission that predicate operators have (a closure
and a record both open with `{`; `brace_expr` already tells them apart), and `in` parses its right
operand a tier lower than the other comparisons, because §8a puts ranges *below* comparison and
`5 in 0..10` would otherwise be `(5 in 0) .. 10`.

#### Part F — `capture`: regex submatches ✅ (2026-08-04)

**Landed — and writing its tests found that `a|b` never matched `"b"`.** The only part with engine
work behind it: submatch slots in the Pike VM — `(`/`)` compile to save
instructions, and each thread carries its slot vector.

- **Do not change what matches.** Slots record *where*; the existing semantics (and the deliberate
  exclusions — no backreferences, no lookaround, §10b's `~=` note) stay exactly as they are. The
  anchor bug the engine already has a regression test for is the reminder of what a "small" change
  to instruction handling can do.
- **`~=` must not pay for it.** It runs per row inside `filter`; keep the slot-free `is_match` path
  and let only `capture` allocate.
- Tests: groups; a non-participating group is `null`; no match is `null` overall; nested groups; and
  the existing pathological-pattern case still terminates.

**The bug the tests found, which is not in the new code.** `Regex::new` discarded the compiled
fragment's `start` and both VMs began at instruction zero. Instructions are emitted as fragments are
parsed and a combinator emits its own *after* its operands, so `a|b` compiles to
`Char(a); Char(b); Split(0,1)` — the entry point is the **last** instruction. Starting at zero ran
the first branch only, so **`"b" ~= /a|b/` was false for the entire life of the engine**. A
concatenation happens to start at zero, which is why every existing test passed; no test used a
top-level alternation whose second branch had to win. `Regex` now records its entry point.

Also: a regex literal only lexes where a pattern belongs (D3), so `capture` had to join `~=` as a
trigger — otherwise `capture /(\d+)/` reads as a path. That keeps `list /` and `6 / 2` working,
which is the pair D3 exists for.

#### Part G — interrupt (`Ctrl-C`) — and `strict` becomes real

**Split in two while building it, and the order reversed.** The plan put the kernel syscall first
"because it is a prerequisite for the other two". That turned out to be wrong: the hazard §11h
actually names — `while true { }` at a prompt, with no way out — is an **in-shell** loop, and
stopping it needs no kernel work at all. G1 closes it; G2 is what stops a running external *stage*
and makes `strict` honest.

**G1 ✅ (2026-08-04) — the shell half.** The tty server sees `Ctrl-C` and sends an out-of-band
event; the evaluator asks the host at statement boundaries and between loop iterations; an
interrupt unwinds as `kind: "Interrupted"`, which `try`/`catch` can catch so cleanup runs. At a
prompt the same event discards the line. Three things it turned up are below.

**G2 ✅ (2026-08-04) — the kernel half.** Built as a **request**, at the maintainer's direction, and
that reframing dissolved the decision the plan had been holding: if terminate is a notification the
target handles, the kernel never touches the target's execution, so there is no safe point to pick
and no second teardown path. `sys_process_terminate` enqueues `TerminateRequested` on the target's
own notification channel and wakes it — that is the whole syscall.

**Proven end to end (2026-08-04, follow-up).** It did not connect at first, and the first diagnosis
was wrong: the tty server emits the event every time. The shell was **sleeping on a message already
in its queue** — a channel signals its waiters at enqueue time, so a waiter that arrives afterwards
never sees the edge, and the interrupt is enqueued before the pipeline starts because the tty reads
`Ctrl-C` in the same console read as the line. Both blocking points poll before blocking now. The
second half was `run_line`, which had no interrupt checkpoint at all — `exec_block` had the only
one, so a line typed at a prompt was checked inside its loops and never between its statements.

Three pieces, in this order, because the first is a prerequisite for the other two *and* fixes a
standing divergence on its own (§1: `strict` claims to terminate the remaining stages and today
only relabels them).

1. **`sys_process_terminate` — a kernel gap this milestone surfaced.** `Rights::TERMINATE` is
   defined, granted on every spawned `Process` handle, and enforced by the type/rights table; no
   syscall consumes it. The right was reserved and the operation never built.
   - It must reuse the existing exit path — the reaper thread and exit-context teardown — rather
     than growing a second one. Reclamation is the area of the kernel that has cost the most to get
     right; a parallel teardown is the way to undo that.
   - **The real question to settle at build time:** the target may be blocked inside a syscall.
     Terminating a thread mid-syscall is the classic hazard; the likely answer is to mark the
     process terminated and tear it down at a safe point, which is a design decision to make
     deliberately and record, not to discover.
   - This is the riskiest single item in the milestone. It is also the one that makes `strict`
     honest, so it earns its place either way.
2. **An out-of-band interrupt event on the tty protocol.** Input is request/response today — a line
   or raw bytes — so the server has no way to tell a client something it did not ask for, and the
   *evaluating* shell is not reading. The design says notification (§11h); confirm that against the
   tty server's actual shape before building.
3. **`Host::interrupted()` and the evaluator's checkpoints** — between statements and between loop
   iterations, unwinding as `kind: "Interrupted"`. The prompt-level half (Ctrl-C discards the line
   being edited) is pure line discipline and can land first.

Tests: host-side, a `MockHost` that reports an interrupt after N statements — the loop unwinds with
the right kind and `try`/`catch` still runs. In guest, the one that matters: **`test-interactive`
types `while true { }`, sends `0x03`, and expects the prompt back.** That test is unwritable today,
which is the point.

**What G1 turned up.**

- **The harness could never send `Ctrl-C`.** QEMU's `stdio` chardev defaults to `signal=on`, where
  `0x03` on stdin is QEMU's own interrupt and never reaches the guest. It is now an explicit
  `-chardev stdio,signal=off`; `-serial stdio` has no way to say it. Every key *except* the one
  §11h is about had been reachable.
- **A raw read swallowed the interrupt.** `Ctrl-C` is checked at the front of the input queue, but a
  raw read takes everything available in one go — so when the byte arrived in the same chunk as the
  keystrokes before it, the shell got it as ordinary input and the front-of-queue check never saw
  it. A raw read now stops at an interrupt.
- **An interrupt has to be able to end a read that has not started yet.** Arriving while nobody was
  reading, the event was delivered and then nothing happened: the shell's *next* read blocked for a
  keystroke that had not been typed, so `Ctrl-C` at a prompt only took effect once the user pressed
  something else. The server now remembers it and completes the next read empty.
- **The interrupt is edge-triggered, and that is a contract, not an implementation detail.** The
  `catch` block handling an interrupt runs statements, so it reaches the checkpoint too — a host
  that kept answering "yes" would interrupt the recovery as fast as it started. Found by a test that
  expected the catch to run and watched it be interrupted instead.
- **The in-guest test sends the loop and the interrupt in one write.** Separately it is a race the
  test loses: an empty loop reaches the ten-million-iteration backstop in well under a second, so
  the run ends on its own and the step passes without the interrupt ever arriving.

#### Part G2 — `sys_process_terminate` ✅ (2026-08-04)

**The decision the plan was holding turned out not to exist**, once the maintainer reframed
terminate as a *request*: the kernel enqueues a notification and does nothing else, so no thread is
stopped, no safe point is needed, and `exit_process` keeps its monopoly on teardown. What follows is
the analysis that led there, kept because the reasoning is the useful part:

**The precedent is already in the tree.** `sys_exception_resume` terminates a *suspended* thread of
another process, and it does not tear anything down from the caller: it sets a **disposition** on
the target and makes it runnable, and the target reads that disposition and exits itself, at the
point it was suspended. That is exactly the "mark it, let it tear itself down at a safe point"
shape — already written, already tested.

So `sys_process_terminate(handle)` should be: `lookup_typed(handle, pid, Rights::TERMINATE,
KObjectType::Process)` — the whole authority check, since the right is granted only to a spawner —
then mark the process terminated and make its threads run to notice.

**The decision that dissolved:** where the safe point is. It only exists if the kernel has to stop a
thread that is not cooperating. A request never does, so the question was an artifact of assuming
forcible teardown — which this system does not have and did not ask for.

**What the build did turn up: the shell was closing every stage handle immediately after spawn.**
§1 says the shell "already holds process handles for everything it spawned, so abort the rest is an
ordinary capability-mediated call on handles it already owns." It did not hold them. `strict` could
only relabel because it had **no authority**, not because a syscall was missing — the syscall was
the second half of a two-part gap, and the first half was one line of `close`.

**Do not grow a second teardown.** `exit_process` already reaps sibling threads across every
per-CPU queue and hands the handle table to the reaper; whatever terminate does must go through
that, not beside it. Reclamation is the area of this kernel that has cost the most to get right.

#### What Milestone 4 does not do

Labelled `break`/`continue`, named capture groups, regex-pattern `replace`, bitwise operators and
the `0o` literal, `.csv`/`.json` for `save`/`open`, schema-aware completion, and job control
(`&`/`jobs`/`fg`). All are in design §12 or the deferred REPL section below, each with its trigger.

### Deferred — the rich REPL (§11) and its dependencies

Gated on the console/tty server + compositor terminal (later in Phase 4). Covers reverse-search,
Shift-Enter continuation (needs a key-event channel), job control's `fg`/`&`, schema-aware
completion, and the prompt's live `PipelineStatus` glyph. Tracked but out of this subproject.
**Partly delivered ahead of schedule** (2026-08-03): the console/tty server landed, and with it raw
mode, history recall and reverse-search. What remains gated is completion (needs schema work),
Shift-Enter (needs a key-event channel), and job control (needs process groups — and, as Milestone
4 Part G found, a terminate syscall).

### Explicitly out of scope (design §10a/§13, carried forward)

Process management (`ps`/`kill` — needs the "how does a command acquire a capability handle to a
process it didn't spawn" design pass), networking tools (netstack deferred), user-definable aliases
with baked-in arguments, package system beyond single-file `use`, circular-import resolution.

---

## Part 3 — First-session checklist (for the forked work)

**First confirm the prerequisites are in.** The three CLI substrate prereqs (§1C) are built in
Phase 4 *before* this subproject — check them off in [`phase-4-desktop.md`](phase-4-desktop.md).
If they are not done, that is the work to do first, not this plan.

With the prereqs in, read, in order: this plan → `nitrox-shell-design-v1.2.md` →
`nitrox-ui-composition-model-v2.md` (for `form`/stdout only) → `docs/spec/typed-stream-format.md`
(TSM1 wire) → `docs/spec/rsproto-*.md` (the protocol the fs-server speaks). Then start at
**Milestone 1 (`list` + `copy`)** — the first integrated proof that the substrate composes.
