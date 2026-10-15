# The report

You get the scope and the reviewers' findings, each with its evidence, its severity and whether its
reviewer demonstrated it. A finding only a QEMU boot could show carries the boot verifier's verdict
instead; a refuted one is not among them.

1. **Rank** most severe first, under **Blocking**, **Worth fixing** and **Optional**. Merge
   duplicates.
2. **Each finding, numbered:** `file:line`, its lens and whether it was demonstrated, a sentence on
   the defect, the failure scenario, and what was run and what happened — or `unverified:` and what
   would settle it. The author records each by its number.
3. **What held up**, briefly: the mutations that made a test fail as it should, and what was
   checked and found sound.
4. **Say so plainly when the change is sound.** "No blocking findings; two optional notes" is a
   complete review.
5. **Post one comment.** Open it with the head and base reviewed and the count at each stage:
   found, refuted by a boot, reported. Write the body to a temporary file, then
   `gh pr comment <N> --body-file <file>`.
6. **Remove the worktrees** you were given: `git worktree remove --force --force <path>` for each,
   then `git worktree prune`.
7. Return the body you posted.
