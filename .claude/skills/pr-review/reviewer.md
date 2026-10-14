# The rules every reviewer follows

Read first by every agent of the review workflow (`/pr-review`) and by `/pr-review-solo`.

## You are not the author

The review exists because the author's session cannot see its own blind spots. Reconstruct what
the change does from the diff and the source. The PR description is a claim to check, not
evidence.

**Review only.** Findings go back to the author. You change code only to probe it — break a line,
add a temporary test — and you restore every probe before you return. Your worktree is yours
alone, so `git checkout -- <file>` restores safely there; check that it did.

## The diff

Review `git diff <base> <head>`, the two commits the scope named: the whole branch, not its last
commit. Never `gh pr diff`, which shows the PR's latest head rather than the one under review. On
a long branch read `git log -p <base>..<head>` too: a change that appears and is partly reverted
marks something harder than it looked.

## Build your own model

1. Read the `CLAUDE.md` of every directory the diff touches: several hold rules for that crate
   alone.
2. Read whole functions, their callers and the types involved, not just the changed lines.
3. Judge against the docs that describe today: `docs/spec/`, `docs/architecture/`,
   `docs/conventions/`, `docs/reference/`. `docs/design/`, `docs/planning/` and `docs/archive/` do
   not, and `docs/decision-log.md` is a dated record, true as of each entry. When the source and a
   current-behaviour doc disagree, the source wins and the doc is a finding.

## Demonstrate or label

A confidently wrong finding costs the author a round trip and teaches them to skim you. For each
finding, either:

- **demonstrate it**: run the test, build the input, break the code, point at the path, and say
  what you ran and what happened; or
- **label it** `unverified:` and say what would confirm or kill it.

Every finding has a file and line, one sentence on the defect, and a failure scenario: the inputs
or state, then the wrong behaviour. Without a scenario it is not yet a finding.

## Builds and gates

- Run one test, not a suite: `cargo test -p <crate> --lib <name>` in `userspace/`, `cargo test
  --lib --target x86_64-unknown-linux-gnu <name>` in `kernel/`, `cargo test <name>` in `tools/`.
  A first build in a fresh worktree takes seconds to a minute.
- CI ran every gate on this head. Do not run them again.
- Boot QEMU only when your instructions say you may: one agent at a time can, or the gates' timing
  fails them.

## What not to do

- Flood with notes: twenty bury the one bug. Cut what you would not raise in person.
- Review the description instead of the diff.
- Judge code against `docs/design/`, `docs/planning/` or `docs/archive/`.
- Block on what the diff only touches: a problem that was already there is worth naming as
  optional, never blocking.
- Leave a probe in place.
