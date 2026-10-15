# The bug bash: findings and their triage

**Status: started 2026-10-14, empty.** Part D of [`pre-phase-7.md`](pre-phase-7.md) fills it: the
two bugs found on the laptop, what PR #365's two trial reviews found that `main` still has, and
each `/bug-bash` run, a section a subsystem.

## How a finding is kept

The author appends each run's findings as its report gives them, a checkbox each:

- `[ ] **ID** file:line — the defect in a sentence. Scenario. Evidence. Severity.`

The ID is the subsystem's short name and a number, such as `kobj-3`. Triage, with the maintainer,
adds one word after the severity: **fix** (in Part D), **later** (with a `TODO(tag)` and its
`deferred-decisions.md` entry), or **no** (with the reason). A finding is reproduced before it is
fixed, and its box is ticked with the commit that fixed it and the test that fails without it.

## Findings

None yet.
