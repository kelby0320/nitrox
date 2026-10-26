# Scope

You are the first agent of a safety survey. You read and change nothing.

1. `git fetch origin main`. The commit surveyed is `git rev-parse origin/main`; `git checkout
   --detach` it.
2. Check each of the subsystem's paths exists at that commit. If none does, stop: return `ok:
   false` and why.
3. Run `cargo xtask unsafe-inventory <each path>` in this worktree and return its table, whole, as
   `inventory`.
4. Within the paths:
   - **the entry points**: syscall handlers, message and request dispatch, interrupt and
     completion handlers, a program's `_start` — each as `file:function` and what calls it;
   - **the boundaries**: where a pointer, a length, a handle or a buffer crosses into or out of the
     subsystem — a syscall's user memory, a message's payload and handles, a device's rings and
     DMA buffers, a disk's structures, a mapping — each as `file:function` and what crosses;
   - **the hot spots**: the five files with the most `unsafe`, each with its count;
   - **how it reaches the kernel**, for userspace: raw `syscallN` calls, `libkern` helpers, or libos,
     with a count of each;
   - **the deferrals already known**: each `TODO(tag)` in the paths, with the title of its entry
     in `docs/rationale/deferred-decisions.md`;
   - **the host tests**: the command that runs each crate's or module's tests;
   - **the `CLAUDE.md` files** governing the paths: each path's directory and its parents'.
5. What the subsystem does, in your own words, in three to six sentences, for the reviewers.
6. This worktree's absolute path (`pwd`): the other agents' worktrees are named beside it, and the
   report removes them by it.
