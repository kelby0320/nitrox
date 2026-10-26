# The safety survey: defects, opportunities and their triage

**Status: planned 2026-10-26 (the maintainer's call); no subsystem surveyed yet.** Part I of
[`pre-phase-7.md`](pre-phase-7.md) fills it: each `/safety-survey` run, a section a subsystem.

## What it looks for

Two kinds of item, as `.claude/skills/safety-survey/rules.md` defines them:

- **A defect**: a size, an ownership or a lifetime held by convention where no type holds it, and
  wrong — memory written past its end or used after it is freed, a handle or a mapping leaked or
  released twice, a `SAFETY` argument that does not hold, a safe function that lets its caller
  break the `unsafe` beneath it. A latent one counts, with the ordinary change that would make it
  bite.
- **An opportunity**: `unsafe`, or a raw pointer, correct today, that a type could hold instead —
  removed where a safe way exists, narrowed to the operation that needs it, or put behind one
  abstraction for every site. One item is a pattern, its sites counted.

Each section starts with the subsystem's inventory, `cargo xtask unsafe-inventory` over its paths,
so two surveys of one tree count alike.

## How an item is kept

The author appends each run's section as its report gives it, a checkbox an item:

- `[ ] **<id>-d<n>** file:line — the defect in a sentence. Scenario. Evidence. Lens. Severity.`
- `[ ] **<id>-o<n>** kind: the pattern — its sites, counted. The abstraction, its home, what it
  closes, what it depends on. Lens. Value, effort.`

The ID is the subsystem's short name, `d` or `o`, and a number, such as `kobj-d3` or `rt-o1`,
apart from the bash's `kobj-3`. Triage, with the maintainer, adds one word after the last field:
**fix** (in Part I), **plan** (an opportunity that goes into Part E's detail pass, or a part of
its own), **later** (past Phase 7, with the maintainer's agreement), or **no** (with the reason).
**This file is an item's record**, as `bug-bash.md` is a finding's: a part of the plan schedules
it, so it takes no `TODO(tag)` and no `deferred-decisions.md` entry. A defect is reproduced before
it is fixed, and its box is ticked with the commit that fixed it and the test that fails without
it; an opportunity's, with the commit that finished it.

## Items
