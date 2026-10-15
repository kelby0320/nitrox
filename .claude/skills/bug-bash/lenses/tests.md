# Lens: tests

**Would the subsystem's tests catch its worst bugs?** Do not read a test and judge it. Break what it
should cover.

1. Choose the behaviours whose failure would cost most: a bound on data from outside, a step of a
   multi-step write, a right checked, a lock held. At most ten, the most dangerous first.
2. For each, break the production code minimally — invert a condition, return early, delete a
   check — and run the crate's tests. A break nothing fails is a finding: say what you broke and
   what the untested behaviour risks.
3. Restore, and check the restore, before the next.

Ways a test looks real and is not, each seen here:

- It holds for the broken code too: an off-by-one that cancels, an order either way satisfies.
- It round-trips through the code's own encoder, so a reader is never handed bad bytes.
- It counts failures with a pattern that does not match the harness's output.
- It tests a handler that no real caller can reach with the input it covers.

A gate that alone could catch a break is a `needs_boot` finding: say what breaking the code should
show.
