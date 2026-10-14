---
name: address-review
description: Act on a posted review of a Nitrox PR as its author — reproduce each finding, fix it with a control, sweep its class, record outcomes, push, and answer on the PR. Use when told a review is up.
---

# Addressing a review

Arguments: `$ARGUMENTS` — the PR number, or none for the current branch's PR.

1. **Read the review** (`gh pr view <N> --comments`) and each finding's evidence.
2. **Reproduce each finding before changing anything**: run what the review ran, or what its
   scenario implies. A finding that does not reproduce is answered with what was run, not fixed.
3. **Fix each that does**, with a test that fails without the fix. Run that control, and say so.
   Then **sweep its class**: the finding's siblings in the file and its callers; for an error path,
   every step failing in turn.
4. **An optional finding** may be deferred, with a `TODO(tag)` and its entry in
   `docs/rationale/deferred-decisions.md`, or declined with a reason. A finding that is the
   maintainer's to decide is asked, not guessed.
5. **Record each finding's outcome** in `.claude/review-outcomes.tsv`, a row a finding, in its
   columns: the date, the PR, the finding's number in the review, its lens (`solo` for a
   `/pr-review-solo` review), its severity, the verifier's verdict, the outcome — `fixed`,
   `refuted`, `deferred` or `declined` — the fixing commit, and a short note.
6. **Run what the fixes reach**: the host tests, and the gates their files touch.
7. **Commit and push**, then answer on the PR (`gh pr comment`): each finding, what was done, and
   the control that shows it.

Never mark a finding fixed without its control, and never reword a finding to fit its fix.
