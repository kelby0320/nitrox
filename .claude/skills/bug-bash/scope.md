# Scope

You are the first agent of a bug bash. You read and change nothing.

1. `git fetch origin main`. The commit hunted is `git rev-parse origin/main`; `git checkout
   --detach` it.
2. Check each of the subsystem's paths exists at that commit. If none does, stop: return `ok:
   false` and why.
3. Within the paths:
   - **the entry points**: syscall handlers, message and request dispatch, interrupt and
     completion handlers, a program's `main` — each as `file:function` and what calls it;
   - **the trust boundaries**: where data from another process, a device or a disk comes in —
     each as `file:function` and what comes in, such as "a directory entry's name length, from
     disk";
   - **the deferrals already known**: each `TODO(tag)` in the paths, with the title of its entry
     in `docs/rationale/deferred-decisions.md`;
   - **the host tests**: the command that runs each crate's or module's tests;
   - **the `CLAUDE.md` files** governing the paths: each path's directory and its parents'.
4. What the subsystem does, in your own words, in three to six sentences, for the reviewers.
5. This worktree's absolute path (`pwd`): the other agents' worktrees are named beside it, and the
   report removes them by it.
