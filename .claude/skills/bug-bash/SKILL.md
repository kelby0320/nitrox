---
name: bug-bash
description: Hunt for bugs in one Nitrox subsystem as main has it — a scope, reviewers with one lens each, a QEMU boot for findings that need one, and the findings returned for docs/planning/bug-bash.md. Started by the maintainer as /bug-bash <subsystem>.
disable-model-invocation: true
---

# /bug-bash

Arguments: `$ARGUMENTS` — a subsystem's `name` or `id` from `subsystems.toml` in this directory.

1. Read `subsystems.toml`. With no argument, or one naming no entry, print the entries' names and
   what each covers, and stop.
2. Run the saved workflow `bug-bash-workflow` with `args: {subsystem, id, what, paths}`, copied from
   that entry. **Pass nothing else**: no hints and nothing from this session. Its agents start from
   `main` and share none of this conversation.
3. While it runs, change nothing in this checkout and run no `cargo` and no QEMU: its agents build
   and test in worktrees of their own, and one of them may boot QEMU.
4. When it returns, append its `section` to `docs/planning/bug-bash.md`, after any earlier
   subsystem's, as it gives it, and print it. Triage is the maintainer's: add no verdict. If it
   returns no `section`, its report agent died: write the section from the `findings` it returned
   as `report.md` says, and remove the worktrees its `error` names.

The rules its agents follow are in this directory: `rules.md` for all of them, then `scope.md`,
the lenses, `verify.md` and `report.md` by stage. The logic, error-path and architecture lenses are
the review's, in `../pr-review/lenses/`; the trust-boundary and tests lenses are this directory's.
