---
name: safety-survey
description: Survey one Nitrox subsystem as main has it for defects of sizes, ownership and lifetimes held by convention, and for unsafe code that could be removed, narrowed or encapsulated — a scope with an inventory, reviewers with one lens each, a QEMU boot for defects that need one, and a section returned for docs/planning/safety-survey.md. Started by the maintainer as /safety-survey <subsystem>.
disable-model-invocation: true
---

# /safety-survey

Arguments: `$ARGUMENTS` — a subsystem's `name` or `id` from `subsystems.toml` in this directory.

1. Read `subsystems.toml`. With no argument, or one naming no entry, print the entries' names and
   what each covers, and stop.
2. Run the saved workflow `safety-survey-workflow` with `args: {subsystem, id, what, paths}`, copied
   from that entry. **Pass nothing else**: no hints and nothing from this session. Its agents start
   from `main` and share none of this conversation.
3. While it runs, change nothing in this checkout and run no `cargo` and no QEMU: its agents build
   and test in worktrees of their own, and one of them may boot QEMU.
4. When it returns a `section`, append it to `docs/planning/safety-survey.md`, after any earlier
   subsystem's, as it gives it, and print it. Triage is the maintainer's: add no verdict.
   - **`stopped`**: its scope found nothing to survey. Print why, and append nothing.
   - **`section: null`**: its report agent died. Write the section from the `defects`,
     `opportunities`, `inventory`, `counts`, `lenses_missing` and `held_up` it returned, as
     `report.md` says, and remove the worktrees its `error` names.
   - **An error**: every lens returned nothing, so there is no section. Print the error, and
     remove any worktree of the run that `git worktree list` still shows.

The survey is the bug bash's machinery aimed at one class and at what could be better, which the
bash's rules leave out. The rules its agents follow are in this directory: `rules.md` for all of
them, then `scope.md`, the lenses, `verify.md` and `report.md`.
