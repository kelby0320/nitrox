# Scope

You are the first agent of a review. You read and change nothing, and you check nothing out.

1. `gh pr view <N> --json number,title,state,headRefOid,baseRefOid`, those fields and no others:
   never its comments or reviews. The head under review is `headRefOid`, unless the workflow named
   one. `git fetch origin pull/<N>/head`, and `git fetch origin <head>` when the head is an older
   commit.
2. **The base is `git merge-base <baseRefOid> <head>`** — not `origin/main`, where a head already
   merged has an empty diff.
3. **CI on the head**: `gh api repos/{owner}/{repo}/commits/<head>/check-runs`. Every run must be
   completed and successful. If one is not, stop: return `ok: false` and why. A review of a head
   that does not build wastes the review.
4. From `git diff --stat <base> <head>` and the diff:
   - the files changed;
   - the `CLAUDE.md` files governing them: each changed file's directory and its parents';
   - the tests added or changed, each with the command that runs it alone;
   - whether the change touches the kernel, a syscall, the ABI or a wire format: `kernel/`,
     `userspace/libkern/`, `userspace/librsproto/`, a `#[repr(C)]` type, or `docs/spec/`.
5. What the change does, in your own words, in three to six sentences, for the reviewers.
