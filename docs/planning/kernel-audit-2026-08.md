# Kernel correctness audit — August 2026

A driving checklist for auditing SMP, interrupt and scheduler correctness. Written after two
latent whole-machine bugs (the i8042 losing interrupts, 2026-08-13; a parked CPU wedging TLB
shootdown, 2026-08-13/14) that **appeared in no backlog** and surfaced only through a gate
that had been written off as flaky.

## Method — this matters more than the list

**Every claim gets a falsification attempt, not a reading.** The three reviews that found real
defects in those fixes did it by running negative controls: delete the guard, confirm the
named test fails; compile the fix out, confirm the gate notices. A claim that survives only
because nobody tried to break it is not a finding of correctness.

Three rules, each learned by getting them wrong:

1. **Prove an instrument can fire before trusting its silence.** A probe grepping a gate's
   stdout counted zero every time — that stream holds only xtask's own lines. A `cargo test`
   run without the host `--target` produced no results at all and was scored as 25/25
   failures. If a check cannot produce a positive, its negative means nothing.
2. **Assert where implementations differ.** `clear_online_bit(true, MAX_CPUS)` with
   `MAX_CPUS == 8` is a no-op mask, so it passed with the guard deleted. Pick the input where
   the guard actually bites (type width, the sentinel, the boundary).
3. **Label every probe with its principal.** An unlabelled `acquire blocked` probe reported a
   *different* client's window and sent an investigation after a bug that did not exist.

**Record what you could not check**, and why, as explicitly as what you did. A gap you have
named is worth more than a claim you have not tested.

## Scope

In: `kernel/src/sched.rs`, `tlb.rs`, `dpc.rs`, `arch/x86_64/{idt,smp,apic,ioapic,syscall}.rs`,
`libkern/{lockrank,spinlock}.rs`, `object/{thread,handle}.rs`, plus the host tests and gates
that claim to cover them.

Out: userspace, the display arm, filesystems. Feature gaps with recorded triggers (MSI/MSI-X,
IRP cancellation, deschedule IPI) are **not** audit targets — they are absent, not wrong.

---

## A. Invariants that are stated somewhere and might not hold

- [x] **`online_mask` vs `cpu_online[]` has exactly three consumers, and they disagree on
      *Audited 2026-08-14, session 1 (§ A.1). Fix in PR #201.*
      purpose.** Giving work needs both bits (`cpu_accepts_work`); taking work needs only
      `cpu_online[]` (`steal_one`, `steal_available`); `tlb::shootdown` reads the mask alone
      and treats departure as "never". Enumerate every reader of each and confirm no fourth
      has appeared with a fourth opinion. `sched.rs` § `ONLINE_MASK` doc.
- [x] **Every permanent park routes through `Cpu::halt_loop`.** Grep every `hlt` in the tree;
      *Audited 2026-08-14, session 1 (§ A.2). Holds; the park that did not was fixed in PR #199.*
      confirm the others (`Cpu::halt`, the idle `sti; hlt`) are genuinely resumable. A park
      that bypasses `halt_loop` skips `leave_online` and reintroduces the 08-13 deadlock.
- [x] **`leave_online`'s identity guard cannot be defeated.** It must refuse for a CPU whose
      *Audited 2026-08-14, session 1 (§ A.3). Holds; guard added in PR #199, its vacuous assertion fixed in PR #201.*
      `TSC_AUX` index does not map back to its own APIC id. Check the boundary: a rebound
      identity, an index at `MAX_CPUS`, and the BSP before `bind_cpu_identity`.
- [x] **No IPI target waits on a CPU that cannot answer.** Shootdown is fixed; check
      *Audited 2026-08-14, session 1 (§ A.4). No finding.*
      `send_reschedule_ipi` and any wake IPI for the same shape — is any of them
      acknowledgement-based rather than fire-and-forget?
- [x] **The grace-tracker claim in `TODO(smp)`.** The deferral says it is safe because *every*
      *Audited 2026-08-14, session 1 (§ A.5). The enumeration holds; the entry's premise was false and was corrected in PR #209, and `restrict`'s guards pinned in PR #208.*
      handle syscall routes through a `HandleTable` method that takes and drops a read guard.
      That is falsifiable: enumerate the syscalls and find one that touches the table without
      one. If it holds, say so with the enumeration.
- [x] **`MAX_WAIT_HANDLES` kernel-stack budget.** A compile-time check exists in `thread.rs`;
      *Audited 2026-08-14, session 1 (cross-cutting). `MAX_CPUS` had the same gap and no protection; assertion added in PR #201.*
      confirm it covers every array that scales with it, not just the ones it names.

## B. Interrupt and entry paths

- [x] **Every interrupt entry stub opens a lock-order scope.** `check-irq-scope` gates this —
      audit the gate itself: does it actually see every stub, including the newest?
      *Audited 2026-08-14: it did not. Two escapes (a guard bound to `_`; a renamed operand)
      plus a cross-file name collision, all closed; the gate's remaining boundary is written
      up in its own doc comment. Decision log, 2026-08-14.*
- [x] **Nothing allocates or frees in IRQ/DPC context**, remembering that an `ObjectRef` drop
      *Audited 2026-08-14, session 2 (§ B.2). Fixes in PR #205.*
      reaches the allocator. The PS/2 driver's `to_drop` hand-off exists for this; check every
      other DPC for the same hazard.
- [x] **`ps2::poll` from `timer_dispatch`.** It takes an `IrqSpinLock` in interrupt context
      *Audited 2026-08-14, session 2 (§ B.3). No finding.*
      ahead of the DPC drain. Confirm the rank is right and that a drain cannot exceed its
      budget with interrupts masked.
- [x] **EOI ordering and spurious interrupts.** Is an EOI ever issued twice, or skipped on an
      *Audited 2026-08-14, session 2 (§ B.4). No finding.*
      early return? What happens on a spurious vector?
- [x] **Preempt-disable/enable balance.** Every `preempt_disable` has a matching enable on
      every path including early returns; `RESCHED_PENDING` is replayed rather than dropped.
      *Audited 2026-08-14. (a) balance and (b) latch-not-dropped both hold — confirmed, no
      change. (c) the invariant was enforced on the involuntary paths only and (e)
      `preempt_enable` could wrap the counter to `u32::MAX`: both closed, plus a third guard
      at the ring-3 boundary for a leaked disable that never reaches a switch. (d) is not a
      finding but the biggest one here — `switch_into` is reached by **no** host test, so the
      whole switch path is covered only by booting; it belongs to § D. Decision log,
      2026-08-14.*

## C. Cross-CPU lifetime and ownership

- [x] **`deferred_drops` cannot grow unbounded**, and every producer has a drainer that runs.
      *Audited 2026-08-14, session 3 (§ C.1). Fixes in PR #205.*
- [x] **The switch-out race (`on_cpu` guard) still holds** — it was fixed once (2026-07-01);
      *Audited 2026-08-14, session 3 (§ C.2). Holds.*
      confirm no path re-reads `current` without it.
- [x] **`place_thread`'s `Err(r)` capacity path** hands the ref back for a drop outside the
      *Audited 2026-08-14, session 3 (§ C.3). Fix in PR #204.*
      lock, on every caller.
- [x] **The thread stranded on a parked CPU.** Known and unfixed: it is on no queue and cannot
      be rescued. Confirm it cannot hold anything the rest of the machine needs (a lock, a
      channel endpoint others block on) — if it can, that is a finding.
      *Audited 2026-08-14, session 3 (§ C.4) — it can hold a lock the rest of the machine
      needs, three ways, all confirmed. Escalated as a design question rather than fixed, and
      decided 2026-08-19: a ring-0 fault stops the machine, which makes all three states
      unreachable rather than fixed. Policy in this change; the machine-wide stop follows.
      Survival is written up in `docs/design/fault-survival.md`. Decision log, 2026-08-19.*
- [x] **Cross-CPU TSC comparisons** in the deadline heap (F10). Saturating arithmetic is the
      *Audited 2026-08-14, session 3 (§ C.5). Fix in PR #204.*
      stated mitigation; check it holds for the largest plausible skew, not just a small one.

## D. The evidence layer — highest value, least glamorous

- [x] **Negative-control every host test that claims to pin an invariant.** Delete the guard,
      *Audited 2026-08-14, session 4 (§ D.1). 162 mutants; `handle/table.rs`'s ladder pinned in PR #208. The wider coverage gap in `sched.rs` is recorded there, not closed.*
      confirm the named test fails. Three of the last three reviews found an assertion that
      passed for both the correct and the broken implementation.
- [x] **No test writes a process-global** that another test reads. `ONLINE_MASK` did, and
      produced a 22 % failure rate at `--test-threads=16` that CI never saw. Sweep for others;
      the fix shape is to pass state as a parameter, not to add a lock.
      *Audited and closed 2026-08-18. Three instances, not one: `MOCK_IF` and the `ONLINE_MASK`
      residual (PR #207), then `PREEMPT_OFF`/`RESCHED_PENDING` (PR #210) — which the first sweep
      missed because it asked which statics are written from inside a `#[cfg(test)] mod`, and
      those are written by production code the tests call. All three now have a `cfg(test)`
      per-thread backing; production codegen unchanged. The payoff was the two preempt guards
      that had no host test precisely because of the shared counter. Decision log, 2026-08-18.*
- [x] **Every gate assertion can fail.** Check each `session.expect` is reachable and not
      *Audited 2026-08-14, session 4 (§ D.3). Two vacuous steps fixed in PR #206; `check-terminal`'s press assertion added in PR #212.*
      satisfied by earlier output — `expect` advances a cursor, so verify that is true of the
      gates that rely on it.
- [x] **Two gate items**, both done. These convert "one run in six" into "every
      run" and are the only things that have caught this class.
      - [x] **Promote `check-terminal` to CI.** *Done 2026-08-18 on 64 consecutive passes. The
        stated bar (~10 clean runs) was never the blocker; the audit's one unreproduced failure
        at the click step was, and it is still unexplained. What made promotion defensible is
        that the gate now asserts where the press landed before asserting the click, so a
        recurrence reports coordinates. Decision log, 2026-08-18.*
      - [x] **Build `check-input --no-ps2-irq`** — boot with the controller's IRQ bits cleared
        so the recovery sweep is the only path. *Done 2026-08-19. The kernel's `no-ps2-irq`
        feature skips only the IRQ-enable write in `arch::ps2::arm`; the gate's assertions are
        unchanged. Measured with `ps2::poll()` deleted: this fails on the first injected key
        while `check-input`, `check-terminal`, `check-display` and `test-qemu` all still pass —
        so it is the only gate in the tree that can. Decision log, 2026-08-19.*
- [x] **Audit `deferred-decisions.md` against reality.** At least one entry ("Debug-build
      lock-ordering enforcement") describes as missing a mechanism that landed 2026-07-29 and
      demonstrably works. Find the others; a deferral list nobody trusts is worse than none.
      *Audited and closed 2026-08-18. Five entries were wrong: two finished and still filed as
      open (one outliving its mechanism by three weeks), two resting on premises falsified by
      commits weeks earlier, and a syscall specified in five current-behaviour documents under
      two spellings that has never existed (PR #209). The gate's own overclaim was the sixth —
      it enforced code→doc only, and 9 of 28 open entries bound to nothing; both directions are
      now enforced, with an exemption for entries that genuinely have no code site (PR #211).
      Decision log, 2026-08-18.*

---

## Reporting

One findings file per session, appended to rather than replaced, each finding carrying: the
claim, what was run, the output, and whether it is **confirmed**, **refuted**, or
**unchecked**. Fixes land as separate PRs — an audit that edits as it goes cannot be reviewed
as an audit.
