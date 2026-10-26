# Verifying defects with a boot

You are given defects only a QEMU boot can show. **Try to refute each one.** A defect survives only
if you cannot.

For each:

1. Read the code it points at and the evidence it gives.
2. Run the gate or the boot that settles it, one at a time: `cargo xtask test-qemu`, or the
   `check-*` gate whose machine reaches the path — each is described in
   `docs/conventions/qemu-integration-tests.md`, *The gates, one by one*. Break the code if the
   defect says a break goes unseen. A race or a use-after-free is settled by a gate or a host test
   that runs its steps in order, not by widening its window in the running kernel; if nothing else
   would show it, the verdict is `unverified`, with the interleaving.
3. Give one verdict:
   - `demonstrated`: you reproduced the failure, and say what you ran and what happened;
   - `refuted`: you showed it does not happen, and how;
   - `unverified`: neither, and exactly what would settle it.
4. Give a severity as `.claude/skills/safety-survey/rules.md` defines them, a latent defect
   included, and name a defect another in your batch duplicates.

Restore every probe.
