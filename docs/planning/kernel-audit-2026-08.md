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

- [ ] **`online_mask` vs `cpu_online[]` has exactly three consumers, and they disagree on
      purpose.** Giving work needs both bits (`cpu_accepts_work`); taking work needs only
      `cpu_online[]` (`steal_one`, `steal_available`); `tlb::shootdown` reads the mask alone
      and treats departure as "never". Enumerate every reader of each and confirm no fourth
      has appeared with a fourth opinion. `sched.rs` § `ONLINE_MASK` doc.
- [ ] **Every permanent park routes through `Cpu::halt_loop`.** Grep every `hlt` in the tree;
      confirm the others (`Cpu::halt`, the idle `sti; hlt`) are genuinely resumable. A park
      that bypasses `halt_loop` skips `leave_online` and reintroduces the 08-13 deadlock.
- [ ] **`leave_online`'s identity guard cannot be defeated.** It must refuse for a CPU whose
      `TSC_AUX` index does not map back to its own APIC id. Check the boundary: a rebound
      identity, an index at `MAX_CPUS`, and the BSP before `bind_cpu_identity`.
- [ ] **No IPI target waits on a CPU that cannot answer.** Shootdown is fixed; check
      `send_reschedule_ipi` and any wake IPI for the same shape — is any of them
      acknowledgement-based rather than fire-and-forget?
- [ ] **The grace-tracker claim in `TODO(smp)`.** The deferral says it is safe because *every*
      handle syscall routes through a `HandleTable` method that takes and drops a read guard.
      That is falsifiable: enumerate the syscalls and find one that touches the table without
      one. If it holds, say so with the enumeration.
- [ ] **`MAX_WAIT_HANDLES` kernel-stack budget.** A compile-time check exists in `thread.rs`;
      confirm it covers every array that scales with it, not just the ones it names.

## B. Interrupt and entry paths

- [x] **Every interrupt entry stub opens a lock-order scope.** `check-irq-scope` gates this —
      audit the gate itself: does it actually see every stub, including the newest?
      *Audited 2026-08-14: it did not. Two escapes (a guard bound to `_`; a renamed operand)
      plus a cross-file name collision, all closed; the gate's remaining boundary is written
      up in its own doc comment. Decision log, 2026-08-14.*
- [ ] **Nothing allocates or frees in IRQ/DPC context**, remembering that an `ObjectRef` drop
      reaches the allocator. The PS/2 driver's `to_drop` hand-off exists for this; check every
      other DPC for the same hazard.
- [ ] **`ps2::poll` from `timer_dispatch`.** It takes an `IrqSpinLock` in interrupt context
      ahead of the DPC drain. Confirm the rank is right and that a drain cannot exceed its
      budget with interrupts masked.
- [ ] **EOI ordering and spurious interrupts.** Is an EOI ever issued twice, or skipped on an
      early return? What happens on a spurious vector?
- [ ] **Preempt-disable/enable balance.** Every `preempt_disable` has a matching enable on
      every path including early returns; `RESCHED_PENDING` is replayed rather than dropped.

## C. Cross-CPU lifetime and ownership

- [ ] **`deferred_drops` cannot grow unbounded**, and every producer has a drainer that runs.
- [ ] **The switch-out race (`on_cpu` guard) still holds** — it was fixed once (2026-07-01);
      confirm no path re-reads `current` without it.
- [ ] **`place_thread`'s `Err(r)` capacity path** hands the ref back for a drop outside the
      lock, on every caller.
- [ ] **The thread stranded on a parked CPU.** Known and unfixed: it is on no queue and cannot
      be rescued. Confirm it cannot hold anything the rest of the machine needs (a lock, a
      channel endpoint others block on) — if it can, that is a finding.
- [ ] **Cross-CPU TSC comparisons** in the deadline heap (F10). Saturating arithmetic is the
      stated mitigation; check it holds for the largest plausible skew, not just a small one.

## D. The evidence layer — highest value, least glamorous

- [ ] **Negative-control every host test that claims to pin an invariant.** Delete the guard,
      confirm the named test fails. Three of the last three reviews found an assertion that
      passed for both the correct and the broken implementation.
- [ ] **No test writes a process-global** that another test reads. `ONLINE_MASK` did, and
      produced a 33 % failure rate at `--test-threads=16` that CI never saw. Sweep for others;
      the fix shape is to pass state as a parameter, not to add a lock.
- [ ] **Every gate assertion can fail.** Check each `session.expect` is reachable and not
      satisfied by earlier output — `expect` advances a cursor, so verify that is true of the
      gates that rely on it.
- [ ] **Promote `check-terminal` to CI** once it has a clean run of ~10, and build
      `check-input --no-ps2-irq` (boot with the controller's IRQ bits cleared so the recovery
      sweep is the only path). Both are filed in `deferred-decisions.md`. These convert "one
      run in six" into "every run" and are the only things that have caught this class.
- [ ] **Audit `deferred-decisions.md` against reality.** At least one entry ("Debug-build
      lock-ordering enforcement") describes as missing a mechanism that landed 2026-07-29 and
      demonstrably works. Find the others; a deferral list nobody trusts is worse than none.

---

## Reporting

One findings file per session, appended to rather than replaced, each finding carrying: the
claim, what was run, the output, and whether it is **confirmed**, **refuted**, or
**unchecked**. Fixes land as separate PRs — an audit that edits as it goes cannot be reviewed
as an audit.
