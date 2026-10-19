# Before Phase 7: the harness, the code's hygiene, and what the laptop found

**Status: planned 2026-10-13, after Phase 6 closed (the maintainer's call); Part A detailed
2026-10-14; A.1, A.2 and A.6 built 2026-10-14, and A.3, A.4, A.5 and A.7 the same day — Part A
built; Part B detailed 2026-10-14; B.1 built 2026-10-15; B.2 built 2026-10-16; B.5 built
2026-10-16; B.3 built 2026-10-16; B.4 built 2026-10-17, the kernel and xtask over their
budgets (the maintainer's call) — Part B built; Part C detailed 2026-10-19.** Each other part
gets its detail pass when it is next. Seven parts, A–G, in the order below. Part E is the
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
- **B.2 `clippy`.** The errors fixed — 44 dereferences of raw pointers in 24 public safe
  functions, a soundness question for each — and the warnings; `-D warnings` in CI, with
  `undocumented_unsafe_blocks` added so a `SAFETY` comment's presence is checked by a tool.
- **B.3 CI in parallel.** The QEMU job split across runners, aiming at about 6 minutes; each
  gate's duration recorded; and each gate asked what it catches that no other does, merging or
  retiring only where the answer is nothing.
- **B.4 The comments.** Every crate to **0.15** or under, or the floor its public items' one-line
  doc comments set, doc comments counted and `SAFETY` comments not: no references to PRs,
  reviews, phases or parts, no narrated history, comments only where the code cannot say it.
  A one-line doc comment on each public item stays — 0.04 of the code — and so do `SAFETY`
  comments, `TODO(tag)`s and the invariants a comment states, shortened; what goes is mostly the
  long doc blocks and the inline essays. One PR, a commit a crate.
- **B.5 The long files.** `tools/xtask/src/main.rs` first, into modules for the gates, the
  static checks, and what they share — QEMU, QMP, the serial console, reading the screen; then the
  others over 5,000 lines after the reformat (`libui/widget.rs`, `nxfiles`, `bootprobe.rs`,
  `desktop-shell`). Moves only, no behaviour changed, each held by its gates.

Order within: B.1, then B.2, then B.5, then B.4 — the reformat first so nothing else is mixed
into it, and the moves before the comment cleanup so the cleanup reads each module once. B.3
needs none of them.

## Part B in detail

*Detailed 2026-10-14, every number measured that day on `main` after PR #375; reviewed in PR #376,
whose decisions are folded in.* Where the outline above and this differ, this is the plan. Part
A's checks measure it: B.4 by `check-comments`, B.5 by `check-size`. In order, one PR each unless
said: B.1, with no other branch open; B.2 as two; B.3 at any point, needing none of them; B.5 as
two; B.4 last.

**How it was measured**, so a later count can be compared with it:

- `rustfmt`: `cargo fmt` in each workspace of a throwaway worktree, with the `rustfmt.toml` below,
  then `git diff --numstat`.
- `clippy`: `cargo clippy --keep-going --message-format=json --
  -W clippy::undocumented_unsafe_blocks --cap-lints warn` in each workspace, on stable 1.95 and
  userspace's pinned nightly, default features, library and binary targets; a finding counted once
  per file and line. Uncapped, `libfsserver`'s error hides every crate built on it, which is how a
  first count came to 335.
- Comments: `check-comments`' classification over each crate's tracked Rust files, a `SAFETY`
  comment and a `# Safety` section counted as neither comment nor code (B.2).

**The size baseline goes up only by hand, in a reviewed commit that says why** (the maintainer's
framing: the 1,000 lines is a guideline held as a ratchet, so that nothing grows unremarked).
`--update` and `--rename` never raise an entry; B.1's reformat and B.2's `SAFETY` comments are
the two raises planned, and B.5's splits move an allowance with `--split`.

### B.1 `rustfmt`

- **The kernel's and tools' toolchain pinned first**: a `rust-toolchain.toml` for the kernel and
  the tools names one exact stable, as userspace's names its nightly, with `clippy` and `rustfmt`
  among its components; CI installs it instead of whatever stable is current. A floating stable
  turns `main` red with no code change once `-D warnings` is on: the kernel gives 131 findings on
  1.95 and 133 on CI's 1.99. A bump is a PR of its own, fixing what the new release flags. B.2's
  counts are taken again on the pinned version.
- **`rustfmt.toml` at the root**, stable options only — `max_width = 120`,
  `use_small_heuristics = "Max"`. Measured on a throwaway copy: 22,458 diff lines in 292 files,
  7,675 added and 14,783 removed, so the tree is 7,108 lines shorter.
- **One PR, four commits, merged with a merge commit** so their hashes stand:
  1. **The tables whose rows mean something** marked `#[rustfmt::skip]`, each looked at first: the
     HID report descriptors in `kernel/src/drivers/hid/descriptor.rs`'s tests, the ACPI table bytes
     in `kernel/src/arch/x86_64/acpi.rs`, SHA-256's round constants, ChaCha's test vectors, and the
     GPT type GUIDs.
  2. **The reformat, alone**: `cargo fmt` in each workspace and nothing else. Not reviewed, the
     maintainer's call; held by the host tests and the full gate set.
  3. **The size baseline, reviewed**: the reformat shortens 55 of the 64 listed files — 54 stay
     over 1,000 and are lowered, and `compositor/src/server.rs` falls to 978 and leaves the list —
     and lengthens nine past their entries, by 13 lines (`libfsserver/src/server.rs`) to 146
     (`bootprobe.rs`). This commit raises those nine, by hand, and names them.
  4. **`.git-blame-ignore-revs`** naming the reformat's hash, which GitHub's blame honours, and
     `cargo fmt --check` for each workspace in CI.
- **The rule never to run `cargo fmt` ends** in each place it is written: root `CLAUDE.md` says to
  run it, the `implementer`'s line forbidding it is reversed, and the author's memory of it is
  removed.
- **With no other branch open**: a reformat conflicts with every open branch on nearly every file.

### B.2 `clippy`

Measured as above: **390 findings across 62 lints** — the kernel 129, the tools 26, userspace 235 —
besides the `unsafe` blocks below.

- **The errors, the author's**, each judged before it is changed:
  - **44 `not_unsafe_ptr_arg_deref`**, on 42 lines in 24 public safe functions of
    `kernel/src/sched.rs`, each dereferencing a raw pointer its caller gives it: made `unsafe fn`
    with a `# Safety` section its callers answer, or changed to take what its callers can vouch
    for. A soundness question for each.
  - **`uninit_assumed_init`** at `kernel/src/mm/user_access.rs:240`: the zero-sized branch of
    `copy_from_user`, sound for every inhabited zero-sized `T` and undefined only for an
    uninhabited one, which no caller passes. Allowed at that line with its reason, or the branch
    closed to uninhabited types; not rewritten as if it were a bug.
  - **`mut_from_ref`** at `userspace/libfsserver/src/disk.rs:420`, a `&mut` made from a `&`.
  - **Two `while_immutable_condition`** at `userspace/service-mgr/src/main.rs:2132` and `:2148`:
    loops that leave by `break` while their condition reads as if it could end them, rewritten as
    `loop`.
- **`undocumented_unsafe_blocks` becomes an error**: 233 blocks today — 142 in the kernel, 91 in
  userspace — have no `SAFETY` comment `clippy` recognizes. Some have none, some explain without
  the `SAFETY:` mark, some have it a statement away. Each needs its justification judged, not just
  a comment moved: the kernel's are the author's, userspace's the `implementer`'s from a brief,
  read before commit.
- **`SAFETY` comments pass Part A's checks** (the maintainer's call): **`check-comments` counts a
  `// SAFETY:` comment and a doc comment's `# Safety` section as neither comment nor code**, since
  the rules require them, built in the first PR's first commit with a test of each; and the size
  entries those comments push up — `sched.rs` among them — are raised by hand in that PR.
- **The warnings, the `implementer`'s**, crate by crate: `needless_range_loop` 43,
  `deref_addrof` 39, `needless_borrow` 39, `collapsible_if` 37, `manual_is_multiple_of` and
  `new_without_default` 16 each, and the long tail. A lint whose fix would make the code worse is
  allowed rather than fixed, with its reason — `too_many_arguments` (15) and `type_complexity` are
  the likely ones — decided case by case and named in the PR.
- **What CI runs**: `[workspace.lints.clippy]` in each workspace's `Cargo.toml` for the `allow`s,
  `[lints] workspace = true` in each crate — and the denials on the command line too,
  `-D clippy::undocumented_unsafe_blocks` with the error lints, so a crate that forgets the
  `[lints]` line is still checked: a scratch workspace showed the workspace setting skipping a
  member without it. **The first PR adds that step**, so the denials hold from the moment they are
  met; the second adds `-D warnings`.
- **Test code too**: plain `cargo clippy` lints library and binary targets only. `--all-targets`
  adds userspace's test code (about 90 findings, 18 of them undocumented `unsafe`), and five
  `no_std` binaries — `eshell`, `hello`, `heartbeat`, `clipboard-server`, `profile-server` — do
  not build as tests (E0152), so CI runs `--all-targets` with those five excluded and lints them
  as they build. In the second PR. The kernel's test targets hold 106 undocumented `unsafe`
  blocks (`--lib --tests` on the host target, measured in PR #379's review).
- **A `SAFETY` comment only on `unsafe` code**: `unnecessary_safety_comment` and
  `unnecessary_safety_doc` denied, which flag 44 sites (PR #379's review) — several a
  `// SAFETY:` on an `unsafe fn` that wants a `# Safety` section, some one-line `if … { unsafe
  {…} }` the lint reads as the statement's. In the second PR, each judged.
- **Two PRs**: the errors, the `unsafe` comments and the `SAFETY` exemption, reviewed with
  `/pr-review`; then the warnings, the test targets and `-D warnings`.

### B.3 CI in parallel

- **Today** (PR #375's last run): the QEMU job takes 12.9 minutes — `check-images` 2.6, which
  builds every image, then thirteen gates in series at 20–75 seconds each, 9.6 in all — beside the
  5.6-minute build job, whose host tests take 2.6.
- **The QEMU job becomes a matrix** of runners, each running a group of gates, the groups balanced
  by measured duration. **Whether each runner builds its images or takes them from one build is
  measured first**: the job's images are 128 MiB disks and live images of about 100 MB (the
  512 MiB target is `check-install`'s, which CI does not run), so passing them as artifacts may be
  cheaper than building each time; xtask would then need a way to use images it did not build. The
  measurement also decides three groups or four. The aim stays about six minutes.
- **Each gate prints its duration**, so the groups are rebalanced from the log rather than guessed.
- **The audit**: a table in [`qemu-integration-tests.md`](../conventions/qemu-integration-tests.md),
  beside the gate catalogue A.5 moved there — each gate, what it catches that no other does, and
  its duration. One whose answer is nothing is merged or retired, the maintainer's call each time.
  `display.yml` and `input.yml`, path-filtered, stay.

### B.5 The long files

- **`tools/xtask/src/main.rs`**: 20,496 lines, 257 functions; 36 commands make 7,357 of them, and
  two test modules 1,283. It becomes a module a gate under `src/gates/`, the static checks under
  `src/checks/` (nightly, arch, deferrals, IRQ scope, ABI, images, beside A's docs, comments and
  size), and shared modules for QEMU and its devices, QMP, the serial session, reading the screen,
  building images, and the host's disk checks.
- **What names `main.rs` follows the move**: `display.yml` and `input.yml` filter on
  `tools/xtask/src/main.rs` alone, so a change to a gate's new module would run neither gate —
  they filter on `tools/xtask/src/**` instead; and the docs and comments that place an item in
  `main.rs` (`boot-flow.md`, `usb.md`, `userspace-build.md`, `nxfiles`, `nxterm`, `bootprobe.rs`)
  name its new module.
- **Two functions no move can bring under 1,000**: `cmd_check_login`, 2,870 lines, and
  `run_interactive_scenarios`, `test-interactive`'s 37 steps in 1,065. Each is split into its steps
  first — the only changes in B.5 that are not moves — in a commit of its own, held by its gate.
- **Then the files still over 5,000 after the reformat**: `libui/widget.rs` (6,966),
  `nxfiles/lib.rs` (5,989), `test-harness/bootprobe.rs` (5,500), `desktop-shell/main.rs` (5,316) —
  tests into a sibling `tests.rs` first, then by responsibility.
- **Moves only, shown to be**: each move is a commit of its own, before any formatting, and its
  diff must be moved blocks — `git diff --color-moved=blocks
  --color-moved-ws=allow-indentation-change`, failing on any line not in a moved block besides
  `mod` and `use` lines and a visibility widened to `pub(crate)`. A line-by-line match is not
  enough: a `continue` deleted in one place and added in another passed one. `cargo fmt` follows
  in the next commit, since moving tests out of a module changes their indentation and `rustfmt`
  rejoins lines. Built as `cargo xtask check-moves`, with a test that an edit inside a move fails
  it. The `implementer` moves the code from a module map the author writes; held by the host
  tests and the gates each crate reaches.
- **The size baseline follows the splits**: `--update` lowers a file as it shrinks, and a new piece
  still over 1,000 lines needs an entry nothing adds. **`check-size --split OLD NEW…`**, built with
  B.5, gives each such piece an entry at its length and lowers `OLD`'s to its own, or removes it at
  1,000 or fewer; refused if they sum past `OLD`'s old entry, the leading `mod`, `use` and
  attribute lines each new file needs not counted. Tested on a split of `widget.rs` before it is
  used: the allowance moves and never grows.
- **Two PRs**: xtask; then the rest.

### B.4 The comments

**The target is excess comment, not documentation** (the maintainer's framing): a doc comment on
each public item is required, and longer only where a contract needs it; a `SAFETY` comment on each
`unsafe` block is required. What goes is the essay inside a function, the comment that restates the
code, narrated history, and a doc comment longer than its item needs.

- **Measured**, `SAFETY` comments aside: 75,211 comment lines to 177,526 code, 0.42; another 4,772
  are `SAFETY` comments and `# Safety` sections. All 50 crates are over 0.15, from `libsession` and
  `session-mgr` down to `fs-server-fat` at 0.18.
- **Each crate's target is 0.15, or the floor its required doc comments set** where that is
  higher: a one-line doc comment on each public item and field, and its `TODO(tag)`s, over its
  code. Only `libkern`, the raw syscall layer, is above today, at 0.29 — its 457 public items and
  fields on 1,595 lines; `librsproto`, at 0.145, has no room above its doc comments. A trial
  (the maintainer's call): small crates whose comments are not the problem showed it, and the
  floors are rechecked when each crate's brief is written. **Reaching every target removes about
  48,400 lines.**
- **What stays**: those doc comments; `SAFETY` comments; `TODO(tag)`s; an invariant the code
  cannot state. **What goes**: references to PRs, reviews, phases and parts; narrated history;
  comments that restate the code; essays.
- **The `implementer` does it, a crate a brief**, with the crate's target and these rules. **The
  author checks each crate's diff touches only comments, mechanically** — its code lines before and
  after, comments stripped, compared — then reads what was cut from `SAFETY` comments and
  invariants, the one loss that matters. **`check-comments --ratio DIR`**, built with B.4, reports
  a directory's whole ratio by the check's own classification, so each crate's target is checked
  by the tool that holds it. Both checks get a negative control: a code edit inside a comment-only
  commit fails the first, and a crate one line over its target fails the second.
- **One PR, a commit a crate**, its code unchanged by construction; how it is reviewed is the
  maintainer's call. The `implementer` on Sonnet reads every Rust line once.

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

## Part C in detail

*Detailed 2026-10-19, measured on `main` at PR #384's merge, the maintainer's calls on `use`, `~`,
line editing, the eject, `nxterm`'s rows and the maximize folded in.* Where the outline above and
this differ, this is the plan. Five sub-parts, a PR each. C.1 comes before C.2, since both change
`nxsh`'s `main.rs`; C.3, C.4 and C.5 need none of the others, and C.4 waits on the laptop for its
last word.

### C.1 The shell's paths

Measured: **every relative path the shell hands the namespace itself fails**, whatever `PWD` is.
`sys_ns_lookup` refuses a path that is not absolute (`kernel/src/object/namespace.rs:369`), and
three of the shell's own lookups pass on what was typed:

- `use "./h3.nx"`: `exec_use` (`userspace/nxsh/src/eval.rs:1149`) reads through the host's
  `read_file` (`main.rs:446`). Its host tests pass only because `MockHost` matches the string.
- The script in `nxsh ./run.nx` (`main.rs:824`).
- A program written as a path, `./tool` (`resolve_program`, `main.rs:190`).

`open`, `save` and `cd` already resolve against `PWD`, with `resolve_path` (`eval.rs:212`) over
`librsproto::path::resolve`.

- **`use` resolves against the directory of the file it is in** (the maintainer's call). That is
  a module import's rule, the one Python's `import` and Node's `require('./x')` follow. bash's
  `source` resolves against the working directory instead, which is why scripts carry
  `$(dirname "${BASH_SOURCE[0]}")`.
  - **At the prompt and with `-c`** there is no file, so it is the working directory, and `.` and
    `..` mean what they mean everywhere else.
  - **The interpreter keeps the directory of the file it is running**: the script's, resolved
    against `PWD` when it is read. While a module runs it keeps the module's own, so a `use` inside
    the module resolves against the module's directory.
  - **It is fixed when the file is read**, so a `cd` in a script moves nothing `use` sees.
  - `open` and every other path go on resolving against `PWD`.
- **A file already being loaded is refused**, with the chain named, rather than recursing until
  the shell's stack runs out. `exec_use` has no guard today, and §9h leaves cycles "not yet
  decided" (`docs/spec/shell-language.md:1130`).
- **The script and a program written as a path resolve against `PWD`** the same way, before the
  host looks them up. A program's arguments still pass as written, by `nxsh/CLAUDE.md`'s rule. Its
  own path is the shell's lookup, like `open`'s.
- **`~` is `$env.HOME`** (the maintainer's call) where `~` or `~/…` starts an unquoted word.
  - It is read where `cd` with no argument reads `HOME` (`env_str`, `eval.rs:1328`).
  - **An unset `HOME` is an error** naming it. There is no fallback like bash's to the password
    database, since there is no user identity to fall back on.
  - **`~name` is an error too**, where bash leaves it as literal text. A quoted `"~"` stays
    literal.
  - **It is the same in a script as at the prompt**: the environment differs, not the mode.
  - **Today `~` is a lex error** wherever it starts the first argument, and literal text after it:
    `parse.rs:901` peeks in expression mode, where `~` alone is no token (`lex.rs:481`).
  - **The lexer change**: in expression mode, `~` followed by `/`, a space, a closer or the end
    reads as a path word, beside `/x` and `./x` (`lex.rs:417-424`). `~=` stays the match operator,
    and a `~` inside a word (`a~b`) stays a character.
  - **It is expanded where a word becomes a string**, which keeps a quoted `"~"` apart:
    - the evaluator's `Expr::Word` (`eval.rs:673`);
    - `cd`'s argument (`:1270`) and an operator's (`:1397`);
    - completion's word (`:270`), so `cd ~/Pic` then Tab completes and writes the `~` back.
  - **A program's argument is expanded before it is passed**, as in bash. That is not the
    rewriting `nxsh/CLAUDE.md` forbids, which stops a relative path meaning one thing to the shell
    and another to the stage; `HOME` is the same on both sides. The rule gets a line saying so.
- **An error naming a path quotes it** in backticks, as most of the shell's errors do already, so
  `"h3.nx "`'s trailing space shows.
  - Seven do not today: `cannot open`, `cannot size`, `cannot read`, `cannot create` (twice) and
    `cannot map` (`main.rs:447-499`), and `use`'s parse error (`eval.rs:1153`).
  - `test-interactive` expects two of them as written (`gates/interactive/mod.rs:218`, `:221`), so
    those change with them.
- **The spec**: §9h says what a relative `use` path is relative to, and that a cycle is refused.
  `~` gets a paragraph beside §8c-1's argument words. Neither is in the spec today.
- **Tests**:
  - **`MockHost` takes absolute keys** and the tests set `PWD`, so a test fails if `use` stops
    resolving. Today's `./lib.nx` keys would pass a `use` that resolved nothing.
  - **Host tests**:
    - a module that `use`s its neighbour, run from another directory;
    - a `cd` in a script before a `use`;
    - a cycle;
    - `~`, `~/x`, `"~"`, `~name`, an unset `HOME`, `a~b`, `~=`, and `~` in a program's argument;
    - completion after `~/`.
  - **`test-interactive`** types `cd ~` and checks `PWD` against `HOME`, and runs a script that
    `use`s its neighbour from another directory.
- **The `implementer`'s**, from a brief naming those tests. The lexer change is read against
  `nxsh/CLAUDE.md`'s rule that every scanner path advances.

### C.2 Line editing

Measured: Linux splits line editing between the kernel's line discipline, which knows Backspace,
Ctrl-U and Ctrl-W, and a shell's own readline, which does the rest. Nitrox splits it the same way.

- **The editing is `tty_server::Discipline`** (`userspace/tty-server/src/lib.rs:53`), which
  `nxsh` links after turning the server's echo off (`nxsh/src/main.rs:896-901`).
- **Its line has no cursor**, only an end.
- **Its escape parser keeps no parameters**, so Delete (`ESC[3~`) cannot be told from Insert. It
  reports Up and Down alone and drops the rest (`lib.rs:126-153`).
- **Every redraw in `nxsh` assumes the cursor** at the end of the line, on one row.
- **The terminal side is ready**: `libterm` sends `ESC[D`, `ESC[C`, `ESC[H`, `ESC[F` and `ESC[3~`
  for Left, Right, Home, End and Delete (`libterm/src/encode.rs:29-48`), as a serial terminal
  does.

The plan:

- **The cursor is the `Discipline`'s**: a position in the line.
  - Typing, Backspace, Ctrl-U and Ctrl-W act at it, as readline's do; Ctrl-U and Ctrl-W take what
    is before it.
  - Enter submits the whole line, wherever the cursor is.
- **The escape parser keeps a sequence's parameter** and reports Left, Right, Home, End and Delete
  as it reports Up and Down.
  - It takes the forms other terminals send too: `ESC[1~` and `ESC[7~` for Home, `ESC[4~` and
    `ESC[8~` for End.
  - An `ESC O` sequence ends at its final byte. Today that byte is typed (`lib.rs:122-125`).
- **Canonical mode does not change** (the maintainer's call, as Linux's line discipline does
  not). It drops the new keys as it drops Up and Down (`lib.rs:497`), so its cursor stays at the
  end. The login prompt and `with`'s, `account`'s and `nxinstall`'s prompts behave as now.
- **The redraw stays a dumb terminal's**: Backspace and text, no escape sequences.
  - Left writes `\x08`, and Right writes the character it passes.
  - An insertion writes the character and the rest of the line, then backs over that rest.
  - A deletion writes the rest and a space, then backs over both.
  - So it needs no terminal width, and draws alike in `nxterm`, on a serial terminal and on
    `fbcon`, which swallows every CSI (`kernel/src/fbcon/text.rs:174`).
- **The cursor moves by character**, though only ASCII can be typed (`lib.rs:234`): a completion
  or a recalled line can hold UTF-8, and `libterm` draws one cell a character.
- **Every redraw in `nxsh` learns the cursor**:
  - Up, Down and Ctrl-R put it at the end of the line they write.
  - Tab completes what is before it and keeps what is after.
  - Ctrl-C writes the rest of the line first, so `^C` lands after it.
  - A cursor key during a Ctrl-R search ends the search keeping the match, as readline does. Today
    the key's `ESC` cancels the search and its `[` and `A` are typed (`main.rs:1160`).
- **A line longer than its row is not edited across the wrap.** `libterm` and `fbcon` both clamp
  a Backspace at the row's start (`libterm/src/grid.rs:613`), so moving back across a wrap draws
  wrong, as Backspace does today.
  - This is recorded as `TODO(tty-size)` with its deferral entry. The tty protocol carries no
    size, and `complete.rs:230` wraps its listing at 80 for the same reason. A size is its trigger.
  - A recalled statement of several lines is edited on its last line.
- **Tests**:
  - **The `Discipline`'s host tests**: each key, and each edit at the start, the middle and the
    end; and canonical mode ignoring the keys.
  - **In `libterm`**: each key is encoded, fed through a `Discipline`, and its echo drawn into a
    `Grid`, asserting the row and the cursor's column. The redraw is judged on what a screen shows,
    not on its bytes.
  - **`test-interactive`** types a line, moves into it, inserts and deletes, and checks what runs.
  - **`check-terminal`** sends Left and a letter over QMP and reads the grid line: the path through
    `nxterm`'s encoder.
- **Docs**: `console-and-tty.md`, whose passage saying the discipline drops `ESC` (`:352-358`) is
  stale already, and a key table in `shell-language.md` §11, which has none.
- **The author's**: the `Discipline` is in every prompt, passwords' included. Reviewed with
  `/pr-review`.

### C.3 `nxterm` shorter, and the top edge maximizes

Measured at the laptop's 1366×768, which the gates boot as 1360×768. The work area is 708 high,
between a 30-pixel top bar and a 30-pixel bottom bar.

- **A session's `nxterm` is a 596×585 window**: 80×24 cells of 7×20 at 12 px
  (`nxterm/src/main.rs:41-43`). 105 of its height is the menu bar, tabs, title bar and padding
  (`lib.rs:69-80`).
- **New windows cascade** from (24, 54) in steps of 24, and wrap only past the *screen's* height
  (`desktop-shell/src/placement.rs:413-416`). So from the sixth window on, a terminal placed there
  runs under the bottom bar.

The plan:

- **24 rows to 20** (the maintainer's call). The window becomes 596×505: 71 per cent of
  the work area's height, where 585 is 83.
  - Eight cascade places fit above the bottom, where 24 rows allow five. 80 columns stay.
  - `check-login`'s check that a maximized grid grew compares against a literal 80×24
    (`tools/xtask/src/gates/login/dialogs.rs:219`). It reads the starting grid from `nxterm`'s
    log instead.
  - `check-terminal`'s click at (397, 295) needs ten rows, which 20 keeps.
- **The cascade wraps on the window, not the screen**: a window goes where it fits above the work
  area's bottom, or at the first place again.
- **A drag to the top maximizes**: zone 7's target becomes the work area, where today it is the
  top half (`placement.rs:39`).
- **Its trigger reaches the screen's top.** The band today is the 24 rows below the bar (rows
  30–53), so a pointer dragged *onto* the bar, as the outline says, is in no zone and drops an
  ordinary move. The top zone and the two top corners grow up to row 0.
- **A real maximize, not a fill** (the maintainer's call).
  - **Today a window knows it is maximized only by its own button.** Each of `nxterm`, `nxedit`
    and `nxfiles` flips a flag on the click (`nxterm/src/lib.rs:576-583`), and a `Configure`
    carries no state (`librsproto/src/surface.rs:1678`).
  - **The shell also drops a window's restore rectangle on any drop**
    (`desktop-shell/src/main.rs:749-752`).
  - **So a window dropped at the top would fill the work area** with its button still offering to
    maximize, and nothing to restore.
  - **So the shell tells a window its state**: a manager request that the compositor forwards as
    an event, the reverse of `RequestState`.
    - **It says maximized** when the shell maximizes a window.
    - **It says normal** when the shell restores one, or a window is resized, snapped elsewhere or
      moved.
    - **The three apps' buttons follow it** rather than their own click. That also mends their
      showing a maximize the shell refused.
    - **A drop on the top zone keeps** the window's rectangle from before the drag as its restore
      rectangle.
    - **It is a wire change** in `rsproto-surface-ops.md`, not a change to the kernel ABI.
  - **The fill was the other answer**: the top zone filling the work area as the halves do, a line
    in `placement.rs`, with the button out of step and nothing to restore.
- **Tests**:
  - `register_snap_zones` takes a channel today and has no host test. Its table becomes a function
    of the work area, with one.
  - The compositor's zone tests stand.
  - `check-login`'s snap step (`gates/login/overview.rs:345-415`) drags a window onto the top
    bar, and asserts the shell's maximize and the window's state, then restores it with the
    button.
- **Docs**: `desktop-shell.md`'s snapping paragraph (`:93`) and the surface spec.
- **The `implementer`'s for the rows and the cascade; the author's for the state**, which is a
  wire change.

### C.4 An ejected stick stops

Measured:

- **The eject sends the stick nothing beyond the flush.** `disk --eject` and Files' eject run the
  unmount chain and its flush, and nothing more reaches the stick
  (`storage-service/src/main.rs:920-966`). `storage.md` §8a says so (`:339`).
- **The kernel's stick driver has no `START STOP UNIT`**: its commands are at
  `kernel/src/drivers/xhci/storage.rs:127-190`.
- **Nothing tells the kernel a stick was ejected**: unmounting is userspace's.
- **The storage service keeps reading it.** Each read of its table re-probes every unmounted device
  (`refresh_unmounted`, `main.rs:776`). So `disk --list`, or Files' refresh after an eject, would
  wake a stopped stick.

The plan:

- **`IoOpcode::Eject = 4`**, the fifth opcode on `sys_io_submit`, added as `Flush` (administration
  C.2) and `Rescan` (Phase 6 Part G) were.
  - It takes no buffer and no range, needs the `WRITE` right, and acts on a block device.
  - **No new syscall, but the ABI hash changes**, since the opcodes are one of its inputs
    (`abi-version-hash.md`). `IrpOp`, libkern's constant and `abi-sync-check`'s family follow.
  - **A partition refuses it `Unsupported`**, as it does `Rescan`. Today a partition rebases an op
    it does not know and forwards it to its disk (`kernel/src/io/block.rs:491-508`).
  - **AHCI and the RAM disk refuse it `Unsupported` too**, explicitly. Today AHCI fails an op it
    never named with `InvalidArgument` (`kernel/src/drivers/ahci.rs:798`).
- **The stick driver sends `START STOP UNIT`** with LoEj set and Start clear
  (`1B 00 00 00 02 00`), as util-linux's `eject` does, on the no-data path `Flush` uses.
  - **A stick that answers it `ILLEGAL REQUEST`** completes `Unsupported`, and the eject still
    succeeds: the unmount is what keeps the data.
  - **The unit is then ejected**: every later IRP on it is refused `PeerClosed`, as a pulled
    stick's are, until it is plugged in again and arrives as new.
- **The storage service submits it** on the disk's node once the chain has unmounted everything,
  as it submits `Rescan`. It marks the disk and its partitions ejected:
  - it reads them no more;
  - `disk --list` shows them with the note "ejected; remove it";
  - a `disk --mount` or `Reread` of one is refused, saying to plug it in again.
- **Whether the light goes out is the laptop's to say**, as the definition of done has it.
  Sticks differ, and Linux's `udisksctl power-off` also switches the port off. If `START STOP
  UNIT` leaves it lit, the driver goes on, on the same opcode, to deconfigure the device or switch
  off its port, decided with the laptop's answer.
- **The gates**: `check-media` and `check-storage` trace QEMU's SCSI commands
  (`-trace scsi_disk_new_request`), a gate's window onto a device as `check-terminal`'s PS/2 trace
  is.
  - **They assert** the `START STOP UNIT` with LoEj, and that no read reaches the stick after it.
  - **`check-media`'s stick is plugged `removable=on`**, as real sticks report, so QEMU opens its
    tray (`query-block`'s `tray_open`).
  - **Each is checked on CI's QEMU 8.2 first.**
- **Docs**:
  - `io-operation.md`, `irp-layout.md` and `abi-version-hash.md`;
  - `syscall-abi.md`, whose opcode list lacks `Rescan` already (`:197`);
  - `drivers-and-irps.md`, `usb.md`, and `storage.md` §8a;
  - `rsproto-storage-ops.md`'s Eject, which says it "sends nothing further to the device"
    (`:142-143`).
- **The author's**: a driver and the ABI. Reviewed with `/pr-review`.

### C.5 A README and a licence

Neither exists.

- **`LICENSE`**: the MIT licence, "Copyright (c) 2026 Kelby Madal", the name the commits carry.
- **`README.md`**, for a person arriving at the repository:
  - what Nitrox is, and what it rejects;
  - its state: pre-v0.1, with Phases 0–6 built;
  - what building needs:
    - the two pinned toolchains, which rustup installs from the two `rust-toolchain.toml`s;
    - `qemu-system-x86`, `ovmf`, `gdisk`, `mtools`, `dosfstools` and `e2fsprogs`, as CI installs
      them;
  - the commands that build, boot and test it;
  - where the docs start: `docs/architecture/overview.md`;
  - the licence:
    - the project's is MIT;
    - the fonts in `assets/fonts` keep theirs, DejaVu's and Terminus's (SIL OFL 1.1), and travel
      with the image;
    - userspace's crates.io dependencies keep theirs.
- `check-docs` holds its links and paths.
- **The author's.**

### Found on the way

The measuring found three defects no sub-part fixes. They are in
[`bug-bash.md`](bug-bash.md) for Part D to triage:

- a press on a maximized window's title bar that moves at all snaps it;
- assigning into `$env` changes the binding alone, so a spawned program never sees the assignment;
- `cd` ignores every argument after its first.

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
