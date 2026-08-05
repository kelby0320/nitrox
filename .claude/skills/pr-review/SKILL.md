---
name: pr-review
description: Review a Nitrox pull request or branch from a fresh session — build an independent model, check project invariants, break the tests to prove they are not vacuous, and post verified findings to the PR. Use when asked to review a PR, review a branch, or check work before merge.
---

# Reviewing a Nitrox PR

## The one rule

**You are not the author.** This session exists because the author's session cannot see
its own blind spots. Your value comes entirely from reconstructing what the change does
from the diff and the source — not from being told.

The PR description is **a claim to be checked**, not evidence. When it says "this fixes
X", your job is to find out whether it fixes X.

**Review only. Do not fix anything.** Findings go back to the working session, which holds
the context to act on them. A reviewer that edits code creates changes nobody reviewed.

## Phase 0 — orient

```
gh pr view <N>                      # or: git branch --show-current
gh pr checks <N>                    # CI must be green before a review means anything
git fetch origin main
git log --oneline origin/main..HEAD
```

If **CI is red**, stop and report that. Reviewing a branch that does not build wastes the
pass; the author needs the failure, not your opinion.

If no PR number was given, review the current branch against `origin/main`.

## Phase 1 — scope the diff

```
git diff origin/main...HEAD --stat
git diff origin/main...HEAD
```

**Three dots, and the whole branch — not the last commit.** A regression introduced three
commits back and papered over by commit five is invisible in `HEAD~1..HEAD`, and catching
exactly that is half the reason this review exists.

Read the per-commit history too (`git log -p origin/main..HEAD`) when the branch is long:
a change that appears and is partly reverted is a signal that something was harder than it
looked.

## Phase 2 — build your own model

Before judging anything, understand it independently.

1. **Load the local rules for every directory the diff touches.** `CLAUDE.md` exists at the
   repo root and in ~12 subdirectories. Several carry a **"Things that bit, and should not
   bite twice"** section — hard-won failure modes specific to that crate. Read the ones
   covering the changed files. These are the highest-yield checks in the repository and
   they are maintained where the code is, so they stay true.
2. **Read the surrounding code, not just the diff.** A diff shows what changed, never what
   it broke. Read whole functions, the callers, and the type definitions involved.
3. **Read the doc that governs the change.** `docs/spec/` for contracts, `docs/architecture/`
   for subsystem behaviour, `docs/reference/` for catalogues. Note: **`docs/history/` is a
   record and `docs/planning/` contains designs for unbuilt things** — neither describes
   current behaviour, so do not judge code against them.

When source and a spec disagree, the source wins and **the spec is a finding** (project
rule, root `CLAUDE.md`).

## Phase 3 — the checks

### Invariants CI cannot see

`check-arch`, `check-nightly`, `check-deferrals`, `check-irq-scope` and `abi-sync-check`
already run in CI. **Do not re-run them.** Check the rules that have no gate:

- **`SAFETY` comments that actually justify.** A comment that restates the operation
  (`// SAFETY: this is safe`) is a missing comment. It must say *why the precondition holds
  here*.
- **`#[repr(C)]` on every type crossing the kernel/userspace boundary**, and on both sides.
- **Async-first.** No syscall that blocks. Potentially-blocking operations return a
  `PendingOperation`; the thread blocks in `sys_wait`, never inside another syscall.
- **No ambient authority.** Authority comes from a handle held, never from identity, a PID,
  or a name. Any new syscall taking a process/resource *by name or number* rather than by
  handle is a design finding, not a nit.
- **No UID/GID model** anywhere.
- **No `panic!()` on the critical path** — `init` and `eshell` specifically.
- **Public items have doc comments.**
- **Every `TODO(tag)` is real**: gated for existence, not for honesty. Check the
  `deferred-decisions.md` entry actually describes this deferral.

### Test quality — the flagship check

This project has shipped **four vacuous tests** that were caught only by deliberately
breaking the code. Do not eyeball tests. Break them.

For each test added or changed that claims to cover new behaviour:

1. Find the production lines that implement the behaviour.
2. Break them minimally — invert a condition, return early, comment out the branch.
3. Run **only that test** (`cargo test -p <crate> --lib <name>`, or the targeted harness
   step). The full suite is too slow to do this per-test.
4. **If it still passes, the test is vacuous.** That is a blocking finding.
5. `git checkout -- <file>` to restore. Verify you restored it before moving on.

Known ways a test looks real and is not, all seen in this repo:

- The assertion matches the **echo of the input** rather than the output (an in-guest
  expect test asserting on a string the harness itself typed).
- The assertion holds **for both the correct and the broken implementation** — an
  off-by-one that cancels, an increment ordered so either behaviour gives the same answer.
- The negative control was never run, so nobody knows the test *can* fail.
- The pattern used to count failures does not match the harness's actual output format.
  If you grep test output for failures, **prove the pattern matches a real failure first**.

Also check: **does any new test take an absurd amount of time?** A guard test that spun for
41 seconds shipped once. Runaway limits belong in an injectable parameter, not a wall clock.

### Docs the change should have updated

The reason this review exists in part. Ask whether the change makes any statement in
`docs/spec/`, `docs/reference/`, `docs/architecture/` or a `CLAUDE.md` **false**. If so,
and the PR does not update it, that is a finding — a stale contract costs more than the bug.

New convention discovered → `docs/conventions/`. New design decision →
`docs/history/decision-log.md`. Deferred item implemented → `docs/rationale/deferred-decisions.md`.

## Phase 4 — verify before you report

**A confidently wrong finding is worse than no finding**, because it costs the author a
round-trip and it teaches them to skim you. Diagnoses in this project have been wrong on
first pass more than once.

For every candidate finding, do one of:

- **Demonstrate it.** Run the test, construct the input, break the code, point at the
  execution path. State what you ran and what happened.
- **Label it.** Prefix with `unverified:` and say exactly what would confirm or kill it.

Never state a mechanism as fact because it is plausible. If you cannot say how you know,
say that you cannot.

## Phase 5 — report

Rank by severity, most severe first. **Keep the blocking set small and defensible.**

- **Blocking** — a correctness bug, a violated architectural invariant, a vacuous test
  covering a claimed feature, or a contract doc the change makes false.
- **Worth fixing** — real, but does not have to hold the merge.
- **Optional** — style, naming, a suggestion.

Each finding: `file:line`, one sentence on the defect, and a concrete failure scenario
(inputs or state → wrong behaviour). No finding without a scenario — if you cannot describe
how it goes wrong, it is not yet a finding.

Post to the PR so the working session can read it without the user relaying anything:

```
gh pr comment <N> --body-file <file>
```

Then print the same summary. If there is no PR, print only.

**Say so plainly when the change is sound.** "No blocking findings; two optional notes" is
a complete and useful review. A reviewer that always finds something gets ignored, which
costs more than the issues it invents.

## Reviewer failure modes

- **Nitpick flooding.** Twenty style notes bury the one real bug. Cut anything you would
  not raise in person.
- **Reviewing the description.** If your findings would be identical without reading the
  diff, you reviewed the wrong artifact.
- **Re-running CI.** It is green; that is what green means. Spend the time on what gates
  cannot see.
- **Judging code against `docs/history/` or `docs/planning/`.** Those describe the past and
  the unbuilt. Neither is a contract.
- **Scope creep.** Pre-existing problems the diff merely touches are worth *naming* as
  optional, never blocking. The author is not obliged to fix the neighbourhood.
