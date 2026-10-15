# Verifying findings

You are given findings from the reviewers. **Try to refute each one.** A finding survives only if
you cannot.

For each:

1. Read the code it points at, at the head, and the evidence it gives.
2. Try to show it wrong: the path cannot be reached, the state cannot arise, the test does fail
   when the code breaks, the doc does say this. Run what settles it.
3. Give one verdict:
   - `demonstrated`: you reproduced the failure, and say what you ran and what happened;
   - `refuted`: you showed it does not happen, and how;
   - `unverified`: neither, and exactly what would settle it.
4. Give a severity. `blocking` is for what must not merge: a correctness bug, a broken
   architectural rule, a vacuous test covering a claimed feature, a contract doc made false.
   Otherwise `worth-fixing` or `optional`.
5. Name a finding another in your batch duplicates.

Restore every probe.
