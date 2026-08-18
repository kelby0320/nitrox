# Mutation campaigns

A mutation campaign answers a question no pass count can: **would this test suite notice if
this guard were deleted?** Nitrox uses them to check assertion strength, not to chase a score.
This document is the method and the one rule that makes the results trustworthy.

## Why we run them

`cargo test` reports whether the code passes the tests. A campaign reports whether the tests
constrain the code. The two came apart badly in the August 2026 audit: `handle/table.rs` had
twenty guards that tests **executed** and none of them **checked** — deleting any one left the
whole suite green. Three consecutive reviews before that had each found an assertion that
passed for both the correct and the broken implementation. Reading tests does not find this;
breaking the code does.

## The method

For each candidate guard in the file under audit:

1. **Kill pass.** Replace the condition with `false` (`if <cond> {` → `if false {`) and run the
   suite. Red is `KILLED`; green is `SURVIVED`.
2. **Reachability pass**, for survivors only. Replace the same condition with
   `(|| -> bool { panic!("AUDIT-REACH") })()`, which panics *iff* control reaches the line.
   This splits survivors into two populations that need opposite fixes:
   - **UNREACHED** — no test runs the line. A coverage hole; write a test that gets there.
   - **REACHED** — a test runs it and cannot tell the two implementations apart. An
     assertion-strength hole; the test that reaches it is asserting the wrong thing.
3. **Attribute each new kill by name.** A guard is pinned when deleting it fails a *named*
   test. "The suite went red" is not attribution — run the new test alone, and run the suite
   with it skipped.

Record what the campaign could not reach as explicitly as what it found. The operator above
mutates `if <cond> {` only: not `if let`, `match` arms, multi-line conditions, `debug_assert!`,
`?`, or loop conditions. A campaign's candidate list is a lower bound on the guards in scope,
never an enumeration of them.

## The rule: the baseline comes from the committed blob

> Read the file under mutation with `git show HEAD:<path>`, never from the working tree, and
> restore it with `git checkout -- <path>` from a handler that runs on **every** exit path.

This is not hygiene, it is correctness, and it is written down because ignoring it silently
inverted twelve results (PR #208).

A mutant can **hang** — deleting a loop's exit condition does not fail a test, it fails to
finish. `handle/table.rs`'s defer-ring backpressure loop and `sched.rs`'s heap sift-down both
do exactly this. If the runner has no per-mutant timeout it stops there; if it dies without
restoring, it leaves a **mutated file on disk**. A later run that seeds its baseline from the
working tree then treats that mutant as pristine, and every subsequent kill/survive verdict is
measured against broken code — a permanently-stubbed guard makes unrelated mutants look killed
by the test it actually broke. Green and red swap places, which is worse than no campaign,
because the output still looks like data.

So a runner needs three things, and all three come from the same failure:

- baseline from `git show HEAD:`, so a stray mutant on disk cannot become the reference;
- a per-mutant timeout, so a hanging mutant is scored `HUNG` rather than ending the run;
- restore-on-exit, so a crash cannot leave the tree dirty.

Verify the tree is clean (`git status`) after a campaign, before believing any of it.

## Interpreting a hang

`HUNG` is a result, not an error. It means the guard's behaviour is pinned **only** by the
suite failing to terminate. In CI that surfaces as a job timeout — detected, but the signal is
"the runner died" rather than "this loop does not terminate". Treat it like a survivor when
deciding what to strengthen.

The same caveat applies to a guard whose deletion dereferences a null pointer: the mutant dies
by `SIGABRT`, taking the harness down before it names a test. The suite is red, so it counts as
killed, but attribution has to be established by elimination rather than read off the output.
