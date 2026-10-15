# Verifying findings with a boot

You are given findings only a QEMU boot can show. **Try to refute each one.** A finding survives
only if you cannot.

For each:

1. Read the code it points at and the evidence it gives.
2. Run the gate or the boot that settles it, one at a time: `cargo xtask test-qemu` or the
   `check-*` gate named in the root `CLAUDE.md` whose machine reaches the path. Break the code if
   the finding says a break goes unseen.
3. Give one verdict:
   - `demonstrated`: you reproduced the failure, and say what you ran and what happened;
   - `refuted`: you showed it does not happen, and how;
   - `unverified`: neither, and exactly what would settle it.
4. Give a severity as `rules.md` defines them, and name a finding another in your batch
   duplicates.

Restore every probe.
