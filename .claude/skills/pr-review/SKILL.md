---
name: pr-review
description: Review a Nitrox PR with the review workflow — a scope agent, reviewers with one lens each, a QEMU boot for findings that need one, and one comment posted to the PR. Started by the maintainer as /pr-review <N> [head].
disable-model-invocation: true
---

# /pr-review

Arguments: `$ARGUMENTS` — a PR number, and optionally a commit to review in place of the PR's
latest head.

Run the saved workflow `pr-review-workflow` with `args: {pr: <number>}`, adding `head: "<commit>"`
when one was given. **Pass nothing else**: no summary of the change, no hints, nothing from this
session. The workflow's agents share none of the author's conversation, which is what makes them a
review.

While it runs, change nothing on the branch and run no `cargo` and no QEMU: its agents build and
test in worktrees of their own, and one of them may boot QEMU.

When it returns, print the review it posted. The author acts on it with `/address-review`.

The rules its agents follow are in this directory: `reviewer.md` for all of them, then `scope.md`,
`lenses/`, `verify.md` and `report.md` by stage.
