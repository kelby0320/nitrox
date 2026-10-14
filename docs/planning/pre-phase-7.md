# Before Phase 7: the harness, the code's hygiene, and what the laptop found

**Status: planned 2026-10-13, after Phase 6 closed (the maintainer's call).** Not detailed: each
part gets its detail pass when it is next. Seven parts, A–G, in the order below. Part E is the
syscall-surface consolidation, planned separately in [`syscall-surface.md`](syscall-surface.md).

## Why

Six phases in, three costs had grown that no single feature would fix:

- **Reviewing is manual.** The maintainer runs `/pr-review` in a second session on a second
  worktree, and carries its findings back. Its findings have been worth it — PR #371's review
  found five, and a sweep after it found two more of the same kind — but it reviews from one
  angle, nothing checks its findings but itself, and nothing measures it.
- **The code has rules nothing enforces.** No formatter, no lint, and nothing that notices a
  comment growing into an essay or a file growing past twenty thousand lines.
- **The laptop and the throughput runs left a list** of bugs and missing pieces a person meets in
  the first ten minutes.

## Where things stand (measured 2026-10-13, on `main` at PR #371's merge)

| Measure | Today |
|---|---|
| Comment lines to code lines, all Rust | 79,986 to 176,452 — **0.45** |
| Comment lines citing a PR number / naming a phase, part or milestone | 1,089 / 2,799 |
| Comment blocks of ten lines or more | 1,454 (the longest 108) |
| `rustfmt`, at `max_width = 120` and `use_small_heuristics = "Max"` | about 22,000 diff lines in 290 files |
| `clippy` | about 210 warnings; 46 errors from default-deny lints, 44 of them `not_unsafe_ptr_arg_deref` in the kernel |
| Files over 1,000 / 2,000 / 5,000 lines | 64 / 24 / 7 of 418; `tools/xtask/src/main.rs` 21,106 |
| CI, wall clock | about 15 min, nearly all one QEMU job running fourteen gates in series; `check-images` alone 3.6 min |
| A worktree's first host-test build | 2–12 s and 70–190 MB, measured for `fs-server-ext4`, `nxsh` and the kernel |

## Part A — the harness

**A.1 A review in this session, by agents that share none of its context.** A skill the author
session runs with a PR number. It starts a saved workflow in `.claude/workflows/`, which is given
the PR number and nothing else, so the author cannot steer it. **At most ten agents** a review
(the maintainer's token budget):

1. **Scope**, one agent: the diff, the `CLAUDE.md` files that govern it, the tests it adds, and
   which reviewers below apply.
2. **Reviewers**, in parallel, each told to investigate every suspicious pattern: logic and
   boundaries; **error paths and partial state** — what a failure at each step leaves behind,
   which is where PR #371's findings 2, 3 and 5 were; **tests** — break the production code under
   each new test, and delete each new line of error handling, and require something to fail;
   rules and docs — the `CLAUDE.md` rules, and any current-behaviour doc the change makes false;
   architecture and the ABI, only when the kernel, a syscall or a wire format is touched.
3. **Verification**: each finding handed to an agent told to refute it, which must demonstrate it
   or mark it unverified; duplicates merged. Batched so the review stays within ten agents.
4. **Report**: ranked, the blocking set held to defects that must not merge, one comment on the
   PR as today.

**Each agent that changes code works in a worktree of its own** (`isolation: 'worktree'`), so
mutations stay independent; a first host-test build there costs seconds. **One agent at a time
boots QEMU**, which needs the machine to itself. Each finding's fate — fixed, refuted or deferred
— is recorded after the review, so whether the new reviewer beats the old is measured rather
than assumed. The current skill reviews A.1's own PR.

**A.2 An author's side.** A skill for addressing a review: reproduce each finding before acting
on it, push back with evidence where it is wrong, defer an optional one with a `TODO` and its
entry, and run the control that shows each fix's test fails without it.

**A.3 `cargo xtask check-comments`**, in CI, on a branch's diff against `main`: added comment
lines over **0.15** of added code lines fail it, and so does a reference to a PR, a review, a
phase or a part in an added comment. **A hook after each edit** of a Rust file nudges the author
above **0.1**. Every failure says how to fix it.

**A.4 `cargo xtask check-size`**, in CI: a Rust file over **1,000 lines** fails it, unless it is on
a baseline list of today's 64, which may shrink and not grow. The message says how to split:
by responsibility, and tests into a sibling `tests.rs`, as `fs-server-fat` does.

**A.5 A shorter root `CLAUDE.md`.** About 150 lines, from 536: the gate catalogue moves to
[`qemu-integration-tests.md`](../conventions/qemu-integration-tests.md), every subdirectory
`CLAUDE.md` is indexed, and the rules A.3, A.4 and Part B make mechanical are stated once,
briefly. `check-docs` holds the index complete.

## Part B — the code's hygiene

Each touches nearly every crate, which is why all of it comes before the syscall work does.

- **B.1 `rustfmt`.** A `rustfmt.toml`, one mechanical reformat commit — the maintainer will not
  review it, so it carries nothing else and is held by the full gate set — listed in
  `.git-blame-ignore-revs`, and `cargo fmt --check` for all three workspaces in CI. The working
  rule never to run `cargo fmt` ends with it.
- **B.2 `clippy`.** The 46 errors fixed — the 44 are public safe functions dereferencing raw
  pointers, a soundness question for each — and the warnings; `-D warnings` in CI, with
  `undocumented_unsafe_blocks` added so a `SAFETY` comment's presence is checked by a tool.
- **B.3 CI in parallel.** The QEMU job split across runners sharing one image build, aiming at
  about 6 minutes; each gate's duration recorded; and each gate asked what it catches that no
  other does, merging or retiring only where the answer is nothing.
- **B.4 The comments.** Every crate to **0.15** or under: no references to PRs, reviews, phases or
  parts, no narrated history, comments only where the code cannot say it. `SAFETY` comments,
  `TODO(tag)`s and the invariants a comment states stay, shortened. One PR, a commit a crate.
- **B.5 The long files.** `tools/xtask/src/main.rs` first, into a module a gate and shared modules
  for QEMU, QMP, the serial console and reading the screen; then the six others over 5,000 lines
  (`libui/widget.rs`, `nxfiles`, `desktop-shell`, `bootprobe.rs`, `nxedit`, `nxsh/eval.rs`).
  Moves only, no behaviour changed, each held by its gates.

Order within: B.1, then B.2, then B.5, then B.4 — the reformat first so nothing else is mixed
into it, and the moves before the comment cleanup so the cleanup reads each module once. B.3
needs none of them.

## Part C — small fixes

- `use "./h3.nx"` resolves against the working directory, as `open` does; an error naming a path
  quotes it, so `"h3.nx "`'s trailing space is visible.
- `~` in a path: `cd ~`, `open ~/notes.txt`.
- Line editing: Left, Right, Home, End and Delete in a line being typed.
- `nxterm`'s default size, a little shorter.
- A window dragged to the top bar maximizes, where it now takes half the screen.
- A `README.md`, and an MIT `LICENSE`.
- `disk --eject` of a USB stick stops it (SCSI `START STOP UNIT`), so its light goes out; the
  laptop confirms it.

## Part D — two bugs to find

- **`nxterm` showing output only after the pointer moves**: every time in the laptop's runs 3
  and 4, so the QEMU dry-run tooling from Phase 6 Part H can make it happen on purpose.
- **The Applications menu's search clearing while typing**: intermittent; a reliable way to make
  it happen comes first.

Each closes with its cause shown and a test that fails without the fix.

## Part E — the syscall surface

As [`syscall-surface.md`](syscall-surface.md) plans it, after its own detail pass.

## Part F — context menus

A right-click menu: a menu widget in the toolkit, a popup surface the compositor places and
dismisses, and menus in Files, the editor, the terminal and on the desktop.

## Part G — a manual for a person

The system's workings written down for the person using it — the shell language, each program,
the desktop — read with a `man`-style program from pages on the root filesystem, whose source is
in the repository so there is one copy.

## Not here

A Windows-style layout picker on dragging to the top; `Super` alone opening a launcher, until
there is one (the Applications menu stays `Super+A`); merging without a human; and authoring
several branches at once, which waits on the gates leaving this machine.

## Definition of done

- A PR is reviewed from the author's session by the A.1 workflow, within ten agents, and its
  findings' outcomes recorded.
- CI fails a branch that is not `rustfmt`-clean, has a `clippy` warning, adds comments over 0.15
  of its code, cites a PR or phase in a comment, or grows a file over 1,000 lines.
- Comments at 0.15 of code or under in every crate; no file over 5,000 lines.
- CI's wall clock about 6 minutes.
- Part C's fixes in, the stick's light confirmed on the laptop; Part D's causes found and fixed;
  Part E done as its plan says; Parts F and G built.
