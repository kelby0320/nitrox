# Before Phase 7: the harness, the code's hygiene, and what the laptop found

**Status: planned 2026-10-13, after Phase 6 closed (the maintainer's call); Part A detailed
2026-10-14; A.1, A.2 and A.6 built 2026-10-14.** Each other part gets its detail pass when it is
next. Seven parts, A–G, in the order below. Part E is the syscall-surface consolidation, planned
separately in [`syscall-surface.md`](syscall-surface.md).

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
| Comment lines to code lines, all Rust | 79,986 to 176,452 — **0.45**: doc comments 0.29, the rest 0.16 |
| Public items and fields, a one-line doc comment each | about 7,200 — 0.04 of the code |
| Comment lines citing a PR number / naming a phase, part or milestone | 1,089 / 2,799 |
| Comment blocks of ten lines or more | 1,454 (the longest 108) |
| `rustfmt`, at `max_width = 120` and `use_small_heuristics = "Max"` | about 22,000 diff lines in 290 files |
| `clippy` | at least 180 warnings, userspace's count stopping at a crate that fails; 46 errors from default-deny lints, 44 of them `not_unsafe_ptr_arg_deref` in the kernel |
| Files over 1,000 / 2,000 / 5,000 lines | 64 / 24 / 7 of 418; `tools/xtask/src/main.rs` 21,106 |
| CI, wall clock | about 15 min, nearly all one QEMU job running fourteen gates in series; `check-images` alone 3.6 min |
| A worktree's first host-test build | 2–12 s and 70–190 MB, measured for `fs-server-ext4`, `nxsh` and the kernel |

## Part A — the harness

**A.1 A review in this session, by agents that share none of its conversation.** A skill the author
session runs with a PR number. It starts a saved workflow in `.claude/workflows/`, which is given
the PR number and nothing else, so the author cannot steer it. **At most ten agents** a review (the
maintainer's token budget):

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

**A.3 `cargo xtask check-comments`**, in CI, on a branch's diff against `main`. **Every comment in a
Rust source file counts, doc comments included**, as in the table above; the markdown under `docs/`
is documentation, not code, and is not measured. The check measures what a diff introduces: a line
moved or only re-indented is not new, so the reformat and the splits are not read as new comments.
New comment lines over **0.15** of new code lines fail it, with an allowance of a few lines so a
small fix's one `SAFETY` comment passes; a diff that adds no more comment lines than it removes
passes whatever its ratio; and a reference to a PR, a review, a phase or a part in a new comment
fails it. **A hook after each edit** of a Rust file nudges the author above **0.1**. Every failure
says how to fix it. The details are the detail pass's.

**A.4 `cargo xtask check-size`**, in CI: a Rust file over **1,000 lines** fails it, unless it is on
a baseline list of today's 64, **each at its length today**, which it may not pass and which falls
as the file shrinks; a file leaves the list under 1,000. The message says how to split: by
responsibility, and tests into a sibling `tests.rs`, as `fs-server-fat` does.

**A.5 A shorter root `CLAUDE.md`.** At most 200 lines, from 537: the gate catalogue moves to
[`qemu-integration-tests.md`](../conventions/qemu-integration-tests.md), every subdirectory
`CLAUDE.md` is indexed, and the rules A.3, A.4 and Part B make mechanical are stated once, briefly.
`check-docs` holds the index complete.

**A.6 Who does the work** (added 2026-10-14, after the review's first trial). The session the
maintainer talks to stays on Opus and the author. It hands bounded work — mechanical, or checked by
a named test — to an `implementer` subagent on Sonnet, and reads every diff before committing. The
review splits the same way: Opus for the lenses that find subtle defects, Sonnet for the rest.

**A.7 A bug bash** (added 2026-10-14, after the review's second trial found bugs still in `main`).
The review's machinery aimed at a subsystem rather than a diff, for Part D: a saved workflow the
maintainer starts per subsystem, its findings a triage list.

## Part A in detail

*Detailed 2026-10-14.* Where the outline above and this differ, this is the plan. **Two PRs.** The
first holds A.1 and A.2, and the old skill reviews it from a second session one last time. The
second holds A.3, A.4, A.5 and A.7, and is the new reviewer's first real review, started from the
author's session. A commit a sub-part.

### A.1 The review

**Where it lives.**

- `.claude/skills/pr-review/SKILL.md` — the launcher, `/pr-review <N>`. It sets
  `disable-model-invocation: true`, so only the maintainer starts a review: starting one is the
  opt-in to a multi-agent run. It runs the saved workflow `pr-review-workflow` with `args: {pr: N}`
  and nothing of the author's, and the author session waits — no `cargo`, no QEMU — until the review
  is posted.
- `.claude/workflows/pr-review-workflow.js` — the workflow: the four stages below. Not `pr-review`:
  a saved workflow is also listed as a skill of its name, which would take the launcher's.
- `.claude/skills/pr-review/reviewer.md` — what every one of its agents reads first: today's
  skill's rules (the reviewer is not the author, demonstrate or label, which docs describe today,
  the failure modes), rewritten for an agent given one lens.
- `.claude/skills/pr-review/lenses/` — one file a lens, and `verify.md` and `report.md`. <!-- check-docs: allow-missing -->
- `.claude/skills/pr-review-solo/SKILL.md` — today's way, kept: one agent in a second session,
  reading `reviewer.md` and every lens itself. It reviews the first PR, whose own `pr-review` is <!-- check-docs: allow-missing -->
  the new launcher, and it stays the cheaper review when tokens are short.

**Each agent in its own worktree.** A workflow's agent starts in `.claude/worktrees/wf_<run>-<n>`,
on a branch `worktree-wf_<run>-<n>` at `main` — not at the author session's commit, as this said
until PR #374's review: the first probe ran with the session on `main` — and with no build output.
So the rules a review's agents read are `main`'s: a PR that changes them is reviewed by the rules it
would replace. So each agent's first step is to fetch `pull/<N>/head` and check out the head the
scope agent named, and its first host-test build costs seconds. `.claude/worktrees/` goes in
`.gitignore`, and **`check-docs` lists its files with git**
(`git ls-files --cached --others --exclude-standard`) rather than walking the disk, where it reads
`.claude/` and a review's worktree under it as the repo's own docs (found by PR #373's review, a
worktree failing it in the author's checkout). The listing brings in the root `CLAUDE.md`, which the
walk never read; its two deliberate examples of a missing doc take the `allow-missing` marker. The
report agent removes the run's worktrees, found by the scope's own path, and their branches.

**The stages, at most ten agents.**

1. **Scope**, one agent. It fetches the PR and checks CI is green on the head it reviews, stopping
   otherwise as today. It returns the head and base commits — **the base being the merge base of the
   head with the PR's `baseRefOid`**, which every agent diffs against, never `gh pr diff`, which
   shows the PR's last head — and the files changed, the `CLAUDE.md` files governing them, the tests
   added or changed, and whether the kernel, a syscall or a wire format is touched.
2. **Reviewers**, four or five in parallel: logic and boundaries; error paths and partial state;
   tests; rules and docs; and architecture and the ABI when the scope says so. Each reports
   everything suspicious with its evidence — precision is the verifiers' job — as findings: a
   file and line, one sentence, a failure scenario, the evidence, and whether it was
   demonstrated.
   - **Tests** carries today's flagship check, breaking the production code under each new test,
     and a second: delete each new line of error handling and run the crate's tests. A line
     nothing fails without is a finding (PR #371's finding 5).
   - **Error paths** asks, of each operation that writes in steps, what a failure at each step
     leaves behind, and uses a crate's failure injection where it has one (`fs-server-ext4`'s
     `Failing`).
   - No reviewer boots QEMU. A finding only a boot can show says so.
3. **Verification**, *dropped after the second run, 2026-10-14 (A.6)*: across both runs it refuted
   none of 59 findings, so only a finding that needs a QEMU boot still goes to a verifier, one agent
   after the reviewers. Planned as: up to three agents, once every reviewer has returned — the one
   barrier, so duplicates across lenses merge first. The findings go out in batches, and each
   verifier is told to refute each one in its batch: demonstrated, refuted, or unverified with what
   would settle it. Findings that need a boot go to one verifier that runs after the others, so a
   single agent boots QEMU and nothing runs beside it.
4. **Report**, one agent. It ranks what survived, holds the blocking set to defects that must
   not merge, posts one comment (`gh pr comment`), removes leftover worktrees, and returns the
   summary.

One scope agent, five reviewers, a boot verifier and a report make eight, under the ten the
maintainer allowed.

**Measured.** Each finding's fate goes in `.claude/review-outcomes.tsv`: the PR, the finding, its
lens and severity, the verifier's verdict, and what became of it — fixed, refuted or deferred.
A.2 writes it. A fix rate per lens says which lenses earn their tokens.

**Acceptance: PR #371 again.** The workflow also takes `args: {pr, head}`, to review a past head.
Run on PR #371 as it stood before its review (`3cdd0c28`), its base is `822a42e5`, the merge base
with #371's base, giving the 86 files the review saw — `origin/main` would give none, the head being
on it now — and it should find that review's five findings; the two the sweep after it found are a
bonus. It costs one review's tokens, so it is the maintainer's call.

**Root `CLAUDE.md`'s review rule** changes with it, in the first PR: the reviewer is still not the
author, but the author starts it — a workflow given only the PR number, sharing none of the author's
conversation — and the second worktree retires.

### A.2 Addressing a review

`.claude/skills/address-review/SKILL.md`, which the author runs when a review is posted:

1. Read the review and each finding's evidence.
2. **Reproduce each finding before changing anything**, or show that it does not reproduce. A
   refuted finding is answered with what was run.
3. Fix it with a test that fails without the fix — run that control — and sweep its class: the
   finding's siblings, and for an error path every failing write.
4. Defer an optional finding with a `TODO(tag)` and its `deferred-decisions.md` entry, or decline
   it with a reason.
5. Record each finding's outcome in `.claude/review-outcomes.tsv`.
6. Commit, push, and answer on the PR with what was done about each.

It may not mark a finding fixed without its control, or reword a finding to fit its fix.

### A.3 `check-comments`

**What counts.** A comment line is one whose trimmed text starts with `//`, doc comments included. A
code line is any other line that is not blank, a line of code with a trailing comment among them.
Rust files only. The tree has no multi-line `/* */` comment to count, and a `/*` it does hold is in
a string or labels the code beside it.

**What is new.** The diff from the merge base with `main`, or on a push to `main` from the merge's
first parent. An added line is new unless the same trimmed text was removed elsewhere in the diff,
by a move or a re-indent; each removed line answers one added line.

**The rules.** Each failure names the file and line and says how to fix it.

- New comment lines may be at most 0.15 of new code lines, plus 3, so that a small fix's
  `SAFETY` comment passes.
- A diff that adds no more comment lines than it removes passes, whatever its ratio.
- A new comment line holding a reference fails: `PR #12`, `(#123)`, `Phase 6`, `Part H.4`,
  `M14 Part`, `slice 3`, `finding 2`. Today 1,079 comment lines cite `PR #N` and 2,485 a part.

**In CI**, in the build-and-test job, whose checkout then needs the history (`fetch-depth: 0`).

**The hook.** A `PostToolUse` hook on `Edit|MultiEdit|Write` runs the built
`tools/target/release/xtask hook-comments` on the edit's text, so the classification is the check's
own, no `cargo` stands in the way, and an unbuilt xtask means silence. Above 0.1 with at least three
new comment lines, or on a reference, it answers through `hookSpecificOutput.additionalContext`: the
ratio, and to cut the comment to what the code cannot say. It never blocks. **Run directly, today's
xtask panics before it dispatches anything**: `main()` reads the commit through `repo_root()`, which
needs `CARGO_MANIFEST_DIR`, set only by `cargo` (PR #373's review). So `hook-comments` is dispatched
first and finds the repository from git, not from `cargo`'s environment — or the hook would fail
exactly as quietly as it succeeds. The input's field names, which the documentation does not give,
are confirmed by capturing an edit's.

**Tests, on the host:** each rule on either side of its line — 0.15 exactly and one line over, the
allowance at 3 and 4, as many comment lines added as removed and one more; a move and a re-indent
not counted; each reference and a near miss of each (a part in prose, a hex colour); a string
holding `/*` or `//`; the hook's input read; and **the built binary run as the hook runs it**, on a
captured input, answering with its `additionalContext`.

The code goes in `tools/xtask/src/comments.rs`, since A.4 holds `main.rs` where it is.

### A.4 `check-size`

`tools/xtask/src/size.rs`, and a baseline, `tools/xtask/size-baseline.toml`: each tracked Rust
file over 1,000 lines today, with its length. It reads `git ls-files`, so build output and
worktrees are never seen.

- A file over 1,000 lines and not listed fails, with how to split it: by responsibility, and
  tests into a sibling `tests.rs`.
- A listed file longer than its entry fails.
- A listed file shorter than its entry, or at 1,000 lines or fewer, fails until the entry is
  lowered or removed. `cargo xtask check-size --update` does both and never raises an entry.

In CI beside A.3. **Tests, on the host:** a file at 1,000 lines and at 1,001, one at its entry
and one over, a stale entry, and `--update` lowering and refusing to raise.

### A.5 A shorter root `CLAUDE.md`

**At most 200 lines**, held by `check-docs`, from 537. `check-docs` first moves out of `main.rs`
into a module of its own, since A.4 holds `main.rs` at its length and A.5's checks would otherwise
grow it.

- **Moved:** every paragraph about a gate, verbatim, to a new section of
  [`qemu-integration-tests.md`](../conventions/qemu-integration-tests.md). The command list stays,
  a line a command.
- **Added:** an index of every `CLAUDE.md` in the tree, a line each — sixteen today, where it
  names five — which `check-docs` holds complete; and the comment and size rules, a line each.

### A.6 Who does the work

- **`.claude/agents/implementer.md`**, `model: sonnet`: one work item from the author's brief, in
  the files it names; the directory's `CLAUDE.md` rules and the comment rules; the host tests of
  what it changed; no commit, no push, no QEMU. Its last message says what it changed, what it ran,
  and what it was unsure of.
- **Root `CLAUDE.md`, *Who does the work***: what the author hands over — `clippy` warnings, comment
  cleanup, moves, docs, small fixes with a clear spec and test — and what it keeps: design and
  detail passes, the kernel's `unsafe`, memory and concurrency, filesystem write paths, drivers,
  syscall and ABI changes, debugging, review findings. A delegated commit says
  `Implemented-By: implementer (Sonnet)`, so the outcomes file can compare its findings with the
  rest.
- **The review by model**: the logic, error-path and architecture lenses on the session's model; the
  scope (low effort), the tests and rules-and-docs lenses, the verifiers and the report (medium
  effort) on Sonnet. Reviewers are told to go after what matters most first and stop when digging
  turns up only optional notes; verifiers, to reproduce a finding's own demonstration first. **The
  second run** (Sonnet 5.5 where above): 19 minutes against 48, 83 million cache-read tokens against
  209, the same four of the original seven found; its Sonnet verifiers ran three to six commands
  each, so verification went too.
- **Measured against the all-Opus trial**: a second run on PR #365 at `55e6bd18`, against what the
  first found.

### A.7 A bug bash

- **`/bug-bash <subsystem>`**, the maintainer's to start as `/pr-review` is, runs a saved workflow
  with the subsystem's name and directories alone, from a list kept beside the skill. Its agents
  start from `main` in worktrees of their own.
- **A scope**, on Sonnet: the subsystem's entry points, where data from another process, a device or
  a disk comes in, the `CLAUDE.md` files that govern it, and the deferrals already known — a
  `TODO(tag)` with its `deferred-decisions.md` entry is not a finding.
- **Reviewers**: logic, error paths, and a new **trust boundary** lens — data another process, a
  device or a disk supplies, trusted unchecked, which most of PR #365's worst findings were — on
  Opus; architecture on Opus for the kernel alone; tests on Sonnet. No rules-and-docs: a doc that
  drifted is not a bug. With no diff to cover, each starts at the entry points and the trust
  boundaries.
- **A QEMU boot**, one agent, for findings that need one; **a report**, on Sonnet, returning the
  subsystem's findings — file and line, scenario, evidence, severity — which the author appends to
  `docs/planning/bug-bash.md`, so no agent writes in the author's checkout.
- **At most eight agents** a run, and one run a subsystem, so the maintainer watches the spend.

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
- **B.4 The comments.** Every crate to **0.15** or under, doc comments counted: no references to
  PRs, reviews, phases or parts, no narrated history, comments only where the code cannot say it.
  A one-line doc comment on each public item stays — 0.04 of the code — and so do `SAFETY`
  comments, `TODO(tag)`s and the invariants a comment states, shortened; what goes is mostly the
  long doc blocks and the inline essays. One PR, a commit a crate.
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

## Part D — a bug bash

After Part B, so the reformat and the splits do not move the lines a finding names, and so
`clippy`'s soundness errors are fixed before the bash would find them again; before Part E, whose
changes reach the same code. **Every finding goes in `docs/planning/bug-bash.md`** and is triaged
with the maintainer — fix, schedule, or won't fix — and reproduced before it is fixed, with a test
that fails without the fix.

- **The two bugs from the laptop.** `nxterm` showing output only after the pointer moves, every time
  in the laptop's runs 3 and 4: the QEMU dry runs of Phase 6 Part H may reproduce it if run from
  `nxterm` — those recorded were typed on serial, which never passes through it — and trying that
  comes first. The Applications menu's search clearing while typing, intermittent: a reliable way to
  make it happen comes first.
- **What PR #365's two trial reviews found and `main` still has**: the kernel reserving a server's
  run count unchecked, a rename leaving two names on one chain, FAT16's cluster count unbounded, and
  the rest — each checked against `main` first.
- **The bash, with A.7, riskiest first**: the kernel's syscall boundary and its objects (handles,
  IPC, namespaces, the page cache); kernel memory (`mm`, address spaces, copies to and from user
  memory); the filesystems (`fs-server-ext4`, `fs-server-fat`, `libfsserver`); the drivers (xHCI and
  HID parsing, AHCI, PS/2); protocol parsing (`librsproto`, `libstream`); the security services
  (`auth-service`, the view broker, `session-mgr`, `libusers`); the scheduler and waiting; storage
  and supervision (`storage-service`, `device-mgr`, `init`, `service-mgr`). Eight runs at first,
  each the maintainer's to start; more later as the budget allows.

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

- A PR is reviewed from the author's session by the A.1 workflow, within eight agents, and its
  findings' outcomes recorded.
- CI fails a branch that is not `rustfmt`-clean, has a `clippy` warning, adds comments over 0.15
  of its code, cites a PR or phase in a comment, or grows a file past 1,000 lines or past its
  listed length.
- Comments at 0.15 of code or under in every crate; no file over 5,000 lines.
- CI's wall clock about 6 minutes.
- Part C's fixes in, the stick's light confirmed on the laptop; Part D's bash run on its eight
  subsystems and every finding in `bug-bash.md` triaged, the ones marked fix fixed; Part E done as
  its plan says; Parts F and G built.
