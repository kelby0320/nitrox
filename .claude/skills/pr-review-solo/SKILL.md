---
name: pr-review-solo
description: Review a Nitrox PR as a single agent in a second session, on a worktree at the PR's head — the review workflow's stages done in turn. Cheaper than /pr-review. Started by the maintainer as /pr-review-solo <N>.
disable-model-invocation: true
---

# /pr-review-solo

One agent does each stage of `/pr-review` in turn, in this session's checkout: a worktree at the
PR's head that nothing else uses. Arguments: `$ARGUMENTS` — the PR number, or none for the current
branch against `main`.

1. **Scope** as `.claude/skills/pr-review/scope.md` says. With no PR, the head is `HEAD` and the
   base `git merge-base origin/main HEAD`.
2. **Read the rules**: `.claude/skills/pr-review/reviewer.md`.
3. **Review through each lens** in `.claude/skills/pr-review/lenses/`: logic, error paths, tests,
   rules and docs, and architecture when the scope says the change reaches the kernel, a syscall,
   the ABI or a wire format. You may boot QEMU: nothing else runs on this machine during the
   review.
4. **Verify your own findings** as `.claude/skills/pr-review/verify.md` says, as their sceptic.
5. **Report** as `.claude/skills/pr-review/report.md` says: post it with `gh pr comment` when
   there is a PR, and print it. There are no worktrees to remove.
