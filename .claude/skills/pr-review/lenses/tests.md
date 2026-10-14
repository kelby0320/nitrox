# Lens: tests

**This project has shipped vacuous tests that only breaking the code caught.** Do not read a test
and judge it. Break what it covers.

1. **Each test the diff adds or changes.** Find the production lines it claims to cover, break
   them minimally — invert a condition, return early, delete the branch — and run that test alone.
   If it still passes, the test is vacuous: a finding. Restore, and check the restore, before the
   next.
2. **Each line of error handling the diff adds** — an undo, a cleanup, a rollback, a write-back
   on failure. Delete it and run the crate's tests. A line nothing fails without is untested: a
   finding, unless you show it cannot be reached.
3. **Each behaviour the PR claims with no test at all**, where it could break silently.

Ways a test looks real and is not, each seen here:

- It asserts on the echo of its input, not the output: a gate matching text it typed itself.
- It holds for the broken code too: an off-by-one that cancels, an order either way satisfies.
- Its negative control was never run.
- It counts failures with a pattern that does not match the harness's output. Prove the pattern
  matches a real failure first.
- It takes absurdly long. A runaway limit belongs in a parameter, not a wall clock.

A gate step you cannot run is a `needs_boot` finding: say what breaking the code should show.
