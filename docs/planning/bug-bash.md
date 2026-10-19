# The bug bash: findings and their triage

**Status: started 2026-10-14; four findings untriaged, three from Part C's detail pass and one
from its PR's CI.** Part D of
[`pre-phase-7.md`](pre-phase-7.md) fills it: the two bugs found on the laptop, what PR #365's two
trial reviews found that `main` still has, and each `/bug-bash` run, a section a subsystem.

## How a finding is kept

The author appends each run's findings as its report gives them, a checkbox each:

- `[ ] **ID** file:line — the defect in a sentence. Scenario. Evidence. Lens. Severity.`

Each run's findings come as one section, headed with the subsystem and the commit of `main` it
hunted, in the shape `.claude/skills/bug-bash/report.md` gives.

The ID is the subsystem's short name and a number, such as `kobj-3`. Triage, with the maintainer,
adds one word after the severity: **fix** (in Part D), **later** (with a `TODO(tag)` and its
`deferred-decisions.md` entry), or **no** (with the reason). A finding is reproduced before it is
fixed, and its box is ticked with the commit that fixed it and the test that fails without it.

## Findings

### Found detailing Part C — main at 226a54f6, 2026-10-19

These were found by the author while measuring Part C, not by a run. No lens hunted, and none
was demonstrated: each is read from the source, and is reproduced before it is fixed.

- [ ] **desk-1** `userspace/compositor/src/input.rs:449` — a press on a maximized window's title
  bar that moves at all snaps it.
  - Scenario:
    - at 1360×768, a maximized window's title bar is rows 31–61;
    - the top snap zones' trigger band is rows 30–53 (`desktop-shell/src/placement.rs:28-41`);
    - a move starts on the press, with no threshold (`input.rs:449-461`);
    - a zone is entered on any motion (`:605-623`).
  - So a click with a pixel of jitter snaps the window to the top half, or to a quarter at either
    end of the band, and the shell drops its restore rectangle
    (`desktop-shell/src/main.rs:749-752`).
  - After Part C's C.3 the top zone maximizes instead, but the corners still quarter the window.
  - Unverified: a press, a one-pixel motion and a release over QMP on a maximized `nxterm` would
    settle it. Logic. Medium.
- [ ] **nxsh-1** `userspace/nxsh/src/eval.rs:636` — assigning to an existing `$env` field changes
  the `$env` binding alone: a spawned program still gets the old value, and the next `cd` reverts
  the binding.
  - Scenario, with `PWD=/` and `HOME=/home/a`: after `$env.HOME = "/x"`, `$env.HOME` reads `/x`,
    but a program then run gets `HOME=/home/a`; after `cd /tmp`, `$env.HOME` reads `/home/a`.
  - An assignment updates the binding's slot (`:636-655`). `cd`, the prompt and every spawned
    stage read the interpreter's own record instead (`cwd()` `:203`, `env_str` `:1328`,
    `host.run(…, &self.env)` `:1680`). Only `set_env_entry` (`:230`) updates that record, and it
    republishes the record over the binding (`rebind_env`, `:220`).
  - That contradicts `rebind_env`'s promise that "the binding and the record never disagree"
    (`:219`), and the field's doc (`:141-144`), which says assigning to `$env` needs nothing beyond
    field access. A field the record lacks cannot be assigned at all: `$env.FOO = "x"` fails with
    "record has no field `FOO`" and runs nothing.
  - Demonstrated by PR #385's review, and again by a temporary host test: `/x` from `$env.HOME`,
    `HOME=/home/a` in the stage's environment, `/home/a` after a `cd`, and the new field refused.
    Logic. Medium.
- [ ] **nxsh-2** `userspace/nxsh/src/eval.rs:1268` — `cd` and `exit` each read their first
  positional argument and ignore the rest.
  - `cd a b` goes to `a`, and `exit 1 2` exits with 1, each saying nothing, against the shell's
    fail-loud rule.
  - `` `cd` takes one path `` fires only for an argument that is not positional (`:1280`), and so
    does `exit`'s check (`:1293-1318`).
  - Demonstrated by a temporary host test: `cd /a /b` left `PWD` at `/a`, and `exit 1 2` exited
    with 1, neither with an error. Logic. Low.

### Found on PR #385's CI — main at 226a54f6, 2026-10-19

A gate failed on a branch that changes only docs. It is the gate's own defect.

- [ ] **gate-1** `tools/xtask/src/guest_input.rs:30` — `click_at` counts a press that reached the
  guest off its exact coordinates as a miss, and discards the transcript, though that press may
  have hit its target and acted.
  - Scenario: `check-login --kvm`'s unsaved-buffer step (`gates/login/dialogs.rs:114`):
    - the press at the dialog's Discard button arrived at x=266 rather than 223, still on the
      button, and `nxedit` discarded and closed;
    - `click_at` matches only `press at x=223 y=448` (`:17`), so it called `skip_to_end` (`:30`),
      which consumed `nxedit: discarding the unsaved buffer`;
    - it clicked twice more, on the Files window behind;
    - the step then waited 45 s for the line already consumed (`dialogs.rs:115`).
  - Its doc comment (`:5-9`) names a retry's hazard for the bar's toggles, not for a button that
    ends a dialog.
  - Demonstrated: the transcript of PR #385's CI, run 39291063887, job 118191523426. Tests.
    Medium: it fails CI on branches that touch none of it.
