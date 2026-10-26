# The report

You get the scope, its inventory, and the reviewers' defects and opportunities. A defect carries
its evidence, its severity and whether its reviewer demonstrated it; one only a QEMU boot could
show carries the boot verifier's verdict instead, and a refuted one is not among them. You write
the section the author appends to `docs/planning/safety-survey.md`. Post nothing and change no
file.

1. **Merge duplicates**: one defect two lenses found is one defect, naming both lenses; one pattern
   two lenses proposed is one opportunity, its sites counted once.
2. **Rank and number.** Defects `high`, then `medium`, then `low`, numbered `<id>-d1`, `<id>-d2`;
   opportunities by value, then by sites, numbered `<id>-o1`, `<id>-o2`.
3. **The section**, in this shape:

   ```
   ### <subsystem> — main at <commit, 8 characters>, <`date +%F`>

   Lenses: <those that returned>; returned nothing: <those that did not, or none>.
   Defects: found <n>, refuted by a boot <n>, reported <n>. Opportunities: <n>.

   The inventory, `cargo xtask unsafe-inventory <paths>`:

   <the table, as the scope gave it>

   **Defects**

   - [ ] **<id>-d<n>** `<file>:<line>` — <the defect in a sentence>. <The scenario>.
     <Demonstrated: what was run and what happened, or Unverified: what would settle it>.
     <Lens(es)>. <Severity>.

   **Opportunities**

   - [ ] **<id>-o<n>** <kind>: <the pattern> — <n> sites (<how counted>), such as `<file>:<line>`,
     `<file>:<line>`, `<file>:<line>`. <The abstraction>, in <its home>. Closes: <the defect
     class>. Depends on: <what, or nothing>. <Lens(es)>. Value <value>, effort <effort>.

   What held up: <briefly, the arguments checked and found sound, and the breaks a test caught>.
   ```

   Wrap at 100 columns, continuation lines indented two spaces. **A lens that returned nothing**
   makes the survey incomplete: say so in the section's first line after the heading. With no
   defects or no opportunities, say `none` under its heading.
4. **Remove this run's worktrees**, and nothing else: each in `git worktree list` whose path starts
   with the prefix you were given, with `git worktree remove --force --force <path>`, then its
   branch, `git branch -D worktree-<the path's last component>`, then `git worktree prune`. A
   branch whose worktree is already gone is deleted too. Given no prefix, remove nothing.
5. Your final message is the section, with nothing before or after it.
