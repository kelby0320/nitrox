# The report

You get the scope and the reviewers' findings, each with its evidence, its severity and whether its
reviewer demonstrated it. A finding only a QEMU boot could show carries the boot verifier's verdict
instead; a refuted one is not among them. You write the section the author appends to
`docs/planning/bug-bash.md`. Post nothing and change no file.

1. **Merge duplicates**: one defect two lenses found is one finding, naming both lenses.
2. **Rank** `high`, then `medium`, then `low`, and number in that order with the subsystem's id:
   `kobj-1`, `kobj-2`.
3. **The section**, in this shape:

   ```
   ### <subsystem> — main at <commit, 8 characters>, <`date +%F`>

   Lenses: <those that returned>; returned nothing: <those that did not, or none>.
   Found <n>, refuted by a boot <n>, reported <n>.

   - [ ] **<id>-<n>** `<file>:<line>` — <the defect in a sentence>. <The scenario>. <Demonstrated:
     what was run and what happened, or Unverified: what would settle it>. <Lens(es)>. <Severity>.

   What held up: <briefly, the breaks a test caught and what was checked and found sound>.
   ```

   Wrap at 100 columns, continuation lines indented two spaces. **A lens that returned nothing**
   makes the hunt incomplete: say so in the section's first line after the heading.
4. **Remove this run's worktrees**, and nothing else: each in `git worktree list` whose path starts
   with the prefix you were given, with `git worktree remove --force --force <path>`, then its
   branch, `git branch -D worktree-<the path's last component>`, then `git worktree prune`. A
   branch whose worktree is already gone is deleted too. Given no prefix, remove nothing.
5. Your final message is the section, with nothing before or after it.
