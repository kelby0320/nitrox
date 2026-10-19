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
- [ ] **nxsh-1** `userspace/nxsh/src/eval.rs:636` — assigning into `$env` changes the `$env`
  binding alone, so `$env.FOO = "x"` then a program leaves the program without `FOO`.
  - An assignment updates the binding's slot (`:636-655`).
  - `cd`, the prompt and every spawned stage read the interpreter's own record instead (`cwd()`
    `:203`, `env_str` `:1328`, `host.run(…, &self.env)` `:1680`). Only `set_env_entry` (`:230`)
    updates that record.
  - That contradicts `rebind_env`'s promise that "the binding and the record never disagree"
    (`:219`). `$env.HOME = "/x"` then a bare `cd` goes to the old `HOME`.
  - Unverified: a host test assigning `$env.FOO`, then running a `MockHost` program that reports
    its environment, would settle it. Logic. Medium.
- [ ] **nxsh-2** `userspace/nxsh/src/eval.rs:1268` — `cd` reads its first positional argument
  and ignores the rest.
  - `cd a b` goes to `a` and says nothing, against the shell's fail-loud rule.
  - `` `cd` takes one path `` fires only for an argument that is not positional (`:1280`).
  - Unverified: a host test of `cd a b` would settle it. Logic. Low.

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
