# Kernel audit — findings

Findings from the checklist in [kernel-audit-2026-08.md](kernel-audit-2026-08.md). **Append to
this file; do not replace it.** Each finding carries the claim, what was run, the output, and a
verdict of **confirmed** / **refuted** / **unchecked**.

No fixes were applied. Every experiment below was reverted; `git status` was clean before and
after, and `cargo xtask test` was green at the end of the session.

---

## Session 1 — 2026-08-14 — Section A (invariants stated somewhere that might not hold)

### Method notes

All work was host-side and static: `cargo test --lib --target x86_64-unknown-linux-gnu` in
`kernel/` (622 tests), `cargo check --target x86_64-unknown-none`, and `cargo xtask test`. No
guest was booted; see [What I could not check](#what-i-could-not-check).

**Positive control first.** Before any negative control, the three tests this section leans on
were confirmed present and passing by name, so a later "FAILED" means the mutation and not a
missing test:

```
test sched::tests::a_park_clears_its_own_bit_and_only_with_an_identity_of_its_own ... ok
test sched::tests::a_parked_cpu_takes_no_new_work_but_can_still_be_drained ... ok
test tlb::tests::waiting_ends_on_acknowledgement_or_departure_but_never_on_lateness ... ok
test result: ok. 622 passed; 0 failed
```

**Two of my own probes were broken and reported "pass" while measuring nothing** — the same
failure mode rule 1 exists for. Both are written up in place (A.1 §e and the cross-cutting
finding) rather than quietly re-run, because the shape is worth keeping.

---

### A.1 — `online_mask` vs `cpu_online[]` consumers

> **Claim.** "has exactly three consumers, and they disagree on purpose … confirm no fourth has
> appeared with a fourth opinion." (`sched.rs` § `ONLINE_MASK`, lines 431–464.)

#### a. REFUTED — there are five readers, not three

Enumerated by grepping every occurrence of `cpu_online` and `online_mask()` across the tree.

| # | Reader | Reads | Question it asks |
|---|---|---|---|
| 1 | `cpu_accepts_work` (`sched.rs:3214`), via `pick_target_cpu` / `pick_wake_cpu` | both | give work |
| 2 | `steal_one` (`sched.rs:3326`), `steal_available` (`sched.rs:3351`) | `cpu_online[]` | take work |
| 3 | `tlb::shootdown` (`tlb.rs:123`, `tlb.rs:190`) | `ONLINE_MASK` | can it ever ack |
| **4** | **`sched::online_cpus()` (`sched.rs:955`)** | **`cpu_online[]`** | **give work** |
| **5** | **`sched::stats_snapshot()` (`sched.rs:1243`)** | **`cpu_online[]`** | report to an operator |

Readers 4 and 5 are the fourth and fifth opinions the checklist asks about.

**Reader 4 is a giving-work decision made from `cpu_online[]` alone** — exactly the class the
doc says needs both bits. Both call sites derive a *placement target* from the count:

- `boot_selftest.rs:260` — `let cpu = (sched::online_cpus().min(arch::MAX_CPUS)).saturating_sub(1);`
  then `spawn_with_affinity(..., 1u8 << cpu)` twice. A parked last CPU is still counted, so the
  pair is pinned to a CPU that will never run them.
- `boot_selftest.rs:493` — `let n = sched::online_cpus()...;` then one `spawn_with_affinity`
  per `i in 0..n`, and the demo spins on `AFFINITY_DONE` until it reaches `n`.

Both are `selftest`-gated, so the blast radius is the harness rather than production. That is
not the same as harmless: the harness is the instrument this whole audit depends on, and the
failure it produces is a 20-billion-spin timeout or a `MISMATCH` print, neither of which fails
a run.

**Reader 5 is the operator-visible surface, and it reports a parked CPU as healthy.**
`stats_snapshot` copies `online: g.cpu_online[c]`, and `stats::format` (`sched.rs:401–407`)
skips `!cpu.online` and then hard-codes the string `online=1`. So `/proc/sched/stats` shows a
CPU that halted in `dump_and_halt` as online, with its last-known counters frozen. Given that
the 08-13 bug survived two milestones because *nothing noticed*, the one place that could have
shown it says everything is fine.

**Verdict: refuted** (as an enumeration). Neither reader is a correctness defect on its own;
both are opinions the `ONLINE_MASK` doc does not account for.

#### b. CONFIRMED, with an evidence gap — the `cpu_online[]` half of `cpu_accepts_work` cannot change an outcome

The doc says giving work "requires *both*". Negative control M4 — delete the `cpu_online[]`
conjunct so placement decides on the mask alone:

```rust
-    g.cpu_online[cpu] && online & (1u64 << cpu) != 0
+    let _ = g;
+    online & (1u64 << cpu) != 0
```

```
test result: ok. 622 passed; 0 failed
```

Nothing notices. The reason is structural, not a missing test: both setters write the pair
inside one `SCHED` critical section (`sched.rs:867–868` for the BSP, `sched.rs:942–943` for each
AP), and only the mask is cleared asymmetrically (`leave_online`). So the state
`cpu_online[c] == false && mask bit c set` is unreachable at any `SCHED`-holding observation,
and the mask bit is a strict subset of `cpu_online[]`. The conjunct is defensive, not
load-bearing.

For contrast, M3 — delete the *mask* half, i.e. compile out the 2026-08-14 fix:

```
test sched::tests::a_parked_cpu_takes_no_new_work_but_can_still_be_drained ... FAILED
panicked at src/sched.rs:3931
test result: FAILED. 621 passed; 1 failed
```

That half is genuinely pinned. **Verdict: the invariant holds; the claim that it takes both
bits is stronger than anything the code or the tests can distinguish.** It becomes load-bearing
the moment the setting order changes (mask first, or the mask set outside `SCHED`) — and
nothing would notice that change.

#### c. CONFIRMED — `pick_target_cpu`'s fallback places on a CPU that does not accept work

`pick_target_cpu` (`sched.rs:3238`) ends `if best == usize::MAX { 0 } else { best }`. The
fallback does not ask whether CPU 0 accepts work, and `place_thread` re-checks only queue
capacity before pushing. `dequeue_front` (`sched.rs:3168`) applies no affinity filter, so
whatever lands there runs there.

Reachable when the BSP parks (a ring-0 fault → `dump_and_halt`) while APs keep running.
Temporary test, thread pinned to CPU 3, CPUs 0 and 3 both parked:

```
test sched::tests::audit_fallback_lands_on_a_parked_cpu_zero ... FAILED
panicked at src/sched.rs:3926: fallback picked cpu 0, which does not accept work
```

This is the hang the `cpu_accepts_work` fix exists to prevent, reintroduced by the fallback
one line below it. **Verdict: confirmed.**

#### d. CONFIRMED (doc bug) — `pick_target_cpu`'s doc states a validation that does not exist

`sched.rs:3220–3222`:

> "(defensive; `set_affinity` rejects an affinity with no online CPU — but a permitted CPU can
> *park* after the fact, which makes this fallback reachable in a way it was not before)"

`sys_thread_set_affinity` (`syscall/table.rs:621–629`) clamps to `MAX_CPUS` bits and rejects
only an **empty** mask; it never consults the online set. `docs/architecture/scheduler.md:220–227`
says so explicitly ("it is **not** checked against the *online* set"). So the fallback was
always reachable, not newly so, and the parenthetical understates how ordinary the path is —
which matters because it is the stated reason the fallback is considered defensive.

#### e. Instrument failure worth recording

My first probe for finding (c)'s sibling (the affinity scan) asserted against a CPU that was
**not** brought online, so `cpu_accepts_work` short-circuited and the expression under test
never ran. It printed `ok` and meant nothing. Corrected by bringing the CPU online and loading
every other queue so the scan had to consider it. Rule 2, from the other side: pick the input
where the code under test actually executes, not merely where the assertion is true.

---

### A.2 — every permanent park routes through `Cpu::halt_loop`

> **Claim.** "Grep every `hlt` in the tree; confirm the others (`Cpu::halt`, the idle
> `sti; hlt`) are genuinely resumable."

**CONFIRMED.** Three `hlt` instructions exist in the whole tree, all in
`arch/x86_64/cpu.rs`:

| Site | Form | Resumable |
|---|---|---|
| `cpu.rs:67` (`halt_loop`) | `cli; hlt` in a loop | no — the permanent park, and it calls `leave_online` first (`cpu.rs:61`) |
| `cpu.rs:81` (`halt`) | bare `hlt` | yes, per caller IF |
| `cpu.rs:92` (`idle_halt`) | `sti; hlt` | yes — parks with IF=1 by construction |

Five `halt_loop` call sites, each checked:

- `main.rs:111` — `_start` after `kernel_main` returns. Only reachable via the
  unsupported-base-revision early return, before `sched::init`; mask is 0 and identity unbound.
- `main.rs:392` — an AP whose APIC id was never bound. `identity_bound()` is false, so
  `leave_online` correctly no-ops; the AP never set a bit (`ap_init` sets it last).
- `main.rs:1121` — the panic handler, after `debug_exit` returns.
- `sched.rs:897` — `ap_run` on `ap_init` failure. Every fallible step in `ap_init`
  (`try_reserve` at `sched.rs:931`) precedes the bit-setting at `sched.rs:942–943`, so there is
  no bit to clear.
- `idt.rs:917` — `dump_and_halt`, on any ring-0 fault. The live case; identity is bound and the
  bit is cleared.

No park uses a non-`hlt` shape (checked every bare `loop {` in `kernel/src/`; the rest are drain
and spin loops, not parks). `debug_exit` (`arch/x86_64/qemu.rs:37`) now returns instead of
parking, which is what closed the 08-13 hole.

**One note, not a defect.** `ArchCpu::halt()` has **zero call sites** — confirmed by grepping
`\bhalt()` across the tree (the only hits are `userspace/eshell`'s own local `halt`). It is a
park primitive that by design does not call `leave_online`, and its doc comment
(`arch/cpu.rs:50–60`) reads as an invitation ("the caller owns the interrupt-flag state"). A
future caller that uses it as a park re-creates the 08-13 deadlock, and nothing gates that.

---

### A.3 — `leave_online`'s identity guard cannot be defeated

> **Claim.** "It must refuse for a CPU whose `TSC_AUX` index does not map back to its own APIC
> id. Check the boundary: a rebound identity, an index at `MAX_CPUS`, and the BSP before
> `bind_cpu_identity`."

**CONFIRMED — both guards are load-bearing.** `clear_online_bit` (`sched.rs:512–517`) is
`if !identity_bound || me >= MAX_CPUS { return; }`. Each conjunct deleted separately:

M1, identity guard removed (range guard kept):
```
test sched::tests::a_park_clears_its_own_bit_and_only_with_an_identity_of_its_own ... FAILED
panicked at src/sched.rs:3709   ← "an unbound CPU cleared someone else's bit"
```

M2, range guard removed (identity guard kept):
```
test sched::tests::a_park_clears_its_own_bit_and_only_with_an_identity_of_its_own ... FAILED
panicked at src/sched.rs:516:28  ← the `1u64 << 64` shift itself
```

M2's panic lands on the shift, exactly as that test's comment predicts. The test asserts at
**64**, not at `MAX_CPUS`, which is the input where the two implementations differ — rule 2
honoured.

Boundaries:

- **Rebound identity — confirmed absent.** `bind_cpu_identity` has exactly two callers
  (`main.rs:432` for the BSP, `main.rs:452` per AP), both inside `bring_up_aps` and both before
  the corresponding `goto_address` store. No path rebinds a dense index after a core has adopted
  it.
- **Index at `MAX_CPUS` — confirmed covered twice.** `apic_of_dense` returns `None` for
  `cpu >= MAX_CPUS` (`smp.rs:69`), so `identity_bound()` is already false there; the explicit
  range check is a second, independent guard.
- **BSP before `bind_cpu_identity` — confirmed, and the doc's line references are accurate.**
  `sched::init` sets bit 0 at `main.rs:353`; `bind_cpu_identity(0, bsp)` is at `main.rs:432`. In
  that window the BSP's `TSC_AUX` is the reset default 0 and `DENSE_TO_APIC[0]` is `APIC_UNSET`,
  so `identity_bound()` is false and the guard refuses — the documented harmless case, since no
  AP exists yet to initiate a shootdown.

#### UNCHECKED — the guard rests on a hardware-identity equivalence nothing verifies

`identity_bound()` (`smp.rs:96–102`) compares `apic_of_dense(rdtscp_aux())` against
`hw_apic_id()`, which reads **`CPUID.01H:EBX[31:24]` — the 8-bit *initial xAPIC* id**
(`smp.rs:33–36`). `DENSE_TO_APIC` is filled from Limine's `lapic_id` (`main.rs:432`, `:452`).
The guard is correct only if those two are the same number for every core.

They coincide under QEMU at `-smp ≤ 8`, which is every configuration this is tested in. If they
ever diverge — real x2APIC hardware where the x2APIC id is not the initial xAPIC id — the
consequences split:

- Every AP fails `adopt_dense_index()` and parks at `main.rs:392`. Fail-safe: the machine runs
  single-CPU rather than colliding per-CPU slots.
- **The BSP's `identity_bound()` becomes permanently false.** A BSP that then panics into
  `dump_and_halt` while APs are live keeps its bit in `online_mask` forever, and the next
  shootdown from any AP waits on it — the exact 08-13 whole-machine deadlock.

So the guard is fail-closed against silent memory corruption and **fail-open against the
deadlock it was written to prevent**. I could not test this: it needs hardware, or a QEMU CPU
model where the two ids differ. Recorded as a named gap rather than a claim either way.

---

### A.4 — no IPI target waits on a CPU that cannot answer

> **Claim.** "check `send_reschedule_ipi` and any wake IPI for the same shape — is any of them
> acknowledgement-based rather than fire-and-forget?"

**CONFIRMED — no.** The kernel defines exactly two IPI vectors, and there is no separate wake
IPI (`place_thread` and `resched_if_idle` both use the reschedule vector):

| Vector | Sender | Shape |
|---|---|---|
| `0x40` `TLB_SHOOTDOWN_VECTOR` | `tlb::shootdown` | acknowledgement-based — the fixed one |
| `0x41` `RESCHEDULE_VECTOR` | `sched::place_thread` (`sched.rs:3304`) | **fire-and-forget** |

`send_reschedule_ipi` (`arch/x86_64/resched.rs:24–30`) resolves the target's APIC id, writes
the ICR, and returns. There is no ack state to wait on, no counter, and no caller spins after
it. A parked CPU keeps its `DENSE_TO_APIC` entry, so it is still poked — harmlessly, since
nothing waits.

M5 re-confirms the shootdown side is genuinely pinned. Departure clause removed from
`wait_satisfied`:

```
test tlb::tests::waiting_ends_on_acknowledgement_or_departure_but_never_on_lateness ... FAILED
panicked at src/tlb.rs:255   ← "CPU 2 has not acked and has left the online set"
```

The one other cross-CPU wait is `bring_up_aps`' AP census (`main.rs:465–471`), which is bounded
at 2×10⁹ spins and degrades to a warning. Not an IPI wait, and not unbounded.

**Adjacent, and out of section A's scope — flagging for C.4.** `halt_loop` releases nothing. A
CPU that faults into `dump_and_halt` while holding a spinlock leaves every other CPU spinning
on it forever, and the `SpinLock` acquire loop (`libkern/spinlock.rs:93`, `:236`) has no bound.
That is "a wait on a CPU that cannot answer" in every sense except that it is not an IPI. The
C.4 checklist item ("confirm it cannot hold a lock the rest of the machine needs") should treat
this as its starting point.

---

### A.5 — the grace-tracker claim in `TODO(smp)`

> **Claim** (`syscall/table.rs:253–257`): "every handle syscall below routes through a
> `HandleTable` method that takes and drops a read guard, which marks the calling context
> quiescent on drop, so deferred closes are reclaimed on the next allocate/close."

**REFUTED as stated. The conclusion survives, but for a different reason than the one given.**

`HandleTable::lookup` (`handle/table.rs:461`) is the **only** method that calls
`grace.enter_read`. Enumerating every arm of `dispatch` against that:

**Routes through `lookup` (guard taken and dropped)** — `sys_handle_stat` (via `stat_on` →
`t.lookup`), `sys_handle_duplicate` (via `duplicate_on` → `t.duplicate` → `self.lookup`),
`sys_memory_map`, `sys_timer_set`, `sys_wait`, `sys_notif_recv`, `sys_channel_send`,
`sys_channel_recv`, `sys_thread_set_affinity`, `sys_thread_get_registers`,
`sys_exception_resume`, `sys_process_terminate`, `sys_ns_lookup` (and the four file ops that
share it), `sys_ns_bind`, `sys_ns_unbind`, `sys_ns_enumerate`, `sys_entropy_read`,
`sys_io_submit`, `sys_file_sync`.

**Touches the table with no read guard — the counterexamples:**

1. **`sys_handle_restrict`** → `restrict_on` → `HandleTable::restrict` (`table.rs:649–687`).
   It reads `entry.generation`, `entry.owner_pid` and `entry.object` and then *writes*
   `entry.rights`, all under `self.inner.lock()` and with no `enter_read` anywhere. This is the
   cleanest refutation: a handle syscall that mutates a table entry and never enters a read-side
   critical section.
2. **`sys_handle_close`** → `close_on` → `HandleTable::close` (`table.rs:565`). No `lookup`, no
   guard.
3. **Seven allocate-only syscalls** — `sys_memory_create`, `sys_timer_create`,
   `sys_channel_create`, `sys_thread_create`, `sys_ns_create`, `sys_entropy_create`,
   `sys_process_spawn`. Each reaches `allocate` without ever entering a guard.

**Why the conclusion nevertheless holds.** Two mechanisms the deferral does not mention do the
work:

- `restrict` and `close` are excluded from reclamation by the **segment lock**, not by the read
  guard: `drain_expired` takes `inner: &mut Inner` (`table.rs:891`), so it can only run with
  that lock held.
- `GraceTracker::new()` starts every context **quiescent** (`grace.rs:63–70`,
  `INITIAL_OBSERVED = QUIESCED_BIT`), so a CPU that never performs a lookup never blocks a grace
  period. Reclamation is driven by `allocate`/`close` reaching `drain_expired`, not by
  quiescence marking as such.

So the deferral's reasoning is wrong and its conclusion is right. Worth correcting rather than
leaving, because the reasoning is what a future reader will rely on.

#### The load-bearing SMP precondition is unstated

`current_ctx_id()` returns the **running CPU's dense id** (`handle/mod.rs:145–149`). One slot
per CPU is only sound if a second thread cannot enter and leave a read section on that CPU while
a first thread is still inside its own — the second thread's `ReadGuard::drop` would mark the
slot quiescent and free a slot the first is still reading.

Today that cannot happen, for two reasons neither `grace.rs` nor the `TODO(smp)` records:

- Syscall bodies run **interrupts-masked end-to-end**: `SFMASK_VALUE = RFLAGS_IF | RFLAGS_DF |
  RFLAGS_AC` (`arch/x86_64/syscall.rs:60`), so `syscall` clears IF on entry and there is no
  preemption point inside a read section.
- The `ReadGuard` never escapes `HandleTable::lookup` — `let _read_guard = …` at `table.rs:461`
  is bound for that function only, so no path holds one across a block. `sys_wait` in particular
  drops every guard before it blocks.

Either of those changing (an IF-enabled syscall window, or a guard held across a
`block_current_and_switch`) turns the per-CPU slot into a use-after-free, and the `TODO(smp)`
comment points at the wrong thing to check.

Also noted: `HandleTable::quiesce` (`table.rs:766`) — the very call the `TODO(smp)` proposes
adding — has **no callers at all** today, in production or in tests.

---

### A.6 — `MAX_WAIT_HANDLES` kernel-stack budget

> **Claim.** "A compile-time check exists in `thread.rs`; confirm it covers every array that
> scales with it, not just the ones it names."

**The check is real, and its arithmetic is wrong.** No live defect; the current value still
fits.

**Positive control first — the check can fire** (M6a, `MAX_WAIT_HANDLES = 64`):

```
error[E0080]: evaluation panicked: MAX_WAIT_HANDLES too large for the kernel stack —
              grow KERNEL_STACK_PAGES or move the wait arrays off the stack
```

Not vacuous. Now the per-element table (`thread.rs:70–72`):

```rust
// handles (8) + copied bytes (8) + refs (16) + objs (8) + types (1) + IoResult out (24)
// + the scheduler's `(usize, bool)` snapshot (16) + signaled bits (1).
const PER_HANDLE: usize = 8 + 8 + 16 + 8 + 1 + 24 + 16 + 1;   // = 82
```

Sizes measured by rustc (temporary `const _: [(); 0] = [(); size_of::<T>()];` probes, which
report the real value in the mismatch):

| Type | Budget assumes | rustc reports |
|---|---|---|
| `Option<ObjectRef>` | 16 | **16** ✓ |
| **`KObjectType`** | **1** | **4** ✗ |
| `IoResult` | 24 | **24** ✓ |
| `(usize, bool)` | 16 | **16** ✓ |
| `WaitResult` | (counted as 1/handle) | **32** total, i.e. 1/handle for its payload |
| `[Option<ObjectRef>; 32]` | 512 | **512** ✓ |

Two errors, both in the permissive direction:

1. **`types` is 4 bytes per handle, not 1.** `types: [KObjectType; MAX_WAIT_HANDLES]`
   (`syscall/table.rs:2118`) is 128 B, not 32 B.
2. **`signaled bits (1)` is counted once for up to four arrays.** The wait path materialises
   `wait_on`'s `signaled` (`sched.rs:2390`), its `bits` (`sched.rs:2469`), the
   `WaitResult::Signaled` payload it returns, and the copy `sys_wait` binds in its match arm.

True cost is ≈88 B/handle against the stated 82. At `MAX_WAIT_HANDLES = 32` that is ~2816 B
against a 4096 B budget, so **today's value is safe** — confirmed by M6b, which corrected the
`types` term to 4 and still builds clean. The consequence is only that the check would wave
through 47–49 where the real budget allows 46.

**Arrays it does not cover at all:**

- `reap_matching`'s `snap = [(0usize, false); MAX_WAIT_HANDLES]` (`sched.rs:2706`) — 512 B of
  `MAX_WAIT_HANDLES`-scaled kernel stack on the **process-exit** path, outside the check's
  stated "wait path" scope. Not currently reachable as an overflow (the wait-path sum trips
  first), so this is scope drift rather than a hole.
- **Nested interrupt frames.** Device IRQs and IPIs have no separate stack and nest onto
  whatever thread stack is current (`TODO(irq-stack)`, `mm/kstack.rs:171`). The budget's "a
  quarter of the stack, leaving the rest for the syscall frame and everything the wait path
  calls into" does not name that as one of the things the remaining three quarters must hold.
  The `test-harness` watermark is what actually covers it, empirically.

---

### Cross-cutting — `MAX_CPUS` has A.6's problem and none of A.6's protection

Not a checklist line, but it is the same question A.6 asks, applied to the other scaling
constant, and it bears directly on A.1 and A.3 (both of which shift by a CPU index).

**`Thread::cpu_mask` is a `u8`** (`object/thread.rs:235`). `MAX_CPUS <= 8` is therefore a hard
requirement, and it is written down nowhere and asserted nowhere. The only compile-time guard in
the tree is `tlb.rs:80` — `assert!(MAX_CPUS <= 64, "ACKED is a u64 bitmask …")` — which is
stated for a different structure and would leave with it.

Proven by bumping `MAX_CPUS` from 8 to 9:

- Kernel builds clean. `cargo test --lib` → **622 passed, 0 failed**. Nothing in the tree
  objects.
- A temporary test that brings all 9 CPUs online and forces `pick_target_cpu` to consider
  CPU 8:

```
test sched::tests::audit_affinity_mask_width_at_max_cpus ... FAILED
panicked at src/sched.rs:3229:54: attempt to shift left with overflow
```

`sched.rs:3229` is `mask & (1 << c)` with `mask: u8` — `1u8 << 8`. In release that wraps to
bit 0 instead of panicking, i.e. CPU 8's affinity silently reads as CPU 0's. `stealable_to`
(`sched.rs:3371`) has the identical expression. `sys_thread_set_affinity` (`table.rs:624`) is
separately wrong at 9: `(cpu_mask & ((1u64 << 9) - 1)) as u8` truncates bit 8 away, so pinning
to CPU 8 returns `InvalidArgument`.

At `MAX_CPUS = 16` the build *does* fail — but only incidentally, on
`let valid = ((1u16 << crate::arch::MAX_CPUS) - 1) as u8;` (`table.rs:682`) tripping the
`arithmetic_overflow` lint, with a message that says nothing about affinity masks. The window
9–15 has no guard at all.

**Instrument note.** My first version of this probe brought only 8 CPUs online, so
`cpu_accepts_work` short-circuited and the shift never executed. It printed `ok`. The probe only
became valid once CPU 8 was online *and* every other queue was loaded so the least-loaded scan
had to reach it — and then it failed. A probe that cannot reach the expression under test is
worth exactly as much as no probe.

---

## Summary

| Item | Verdict |
|---|---|
| A.1 three consumers | **Refuted** — five readers; two undocumented, one of them a giving-work decision, one an operator-facing report that shows a parked CPU as online |
| A.1 (b) both-bits claim | Holds, but the `cpu_online[]` half is unreachable-redundant and untested |
| A.1 (c) CPU-0 fallback | **Confirmed defect** — places on a CPU that does not accept work |
| A.1 (d) `set_affinity` doc claim | **Confirmed doc bug** — the stated validation does not exist |
| A.2 every park via `halt_loop` | **Confirmed** — 3 `hlt` sites, 5 park sites, all correct; `ArchCpu::halt` is an uncalled bypass |
| A.3 identity guard | **Confirmed** — both conjuncts load-bearing (M1/M2); all three named boundaries covered |
| A.3 identity equivalence | **Unchecked** — fail-open against the deadlock if initial-xAPIC ≠ Limine `lapic_id` |
| A.4 IPI ack shape | **Confirmed** — reschedule IPI is fire-and-forget; only shootdown acks, and it is fixed |
| A.5 grace-tracker claim | **Refuted** — `restrict`, `close` and 7 create syscalls take no guard; conclusion survives via the segment lock |
| A.6 wait-handle budget | **Confirmed present and non-vacuous; arithmetic wrong** (`KObjectType` is 4 B, not 1) — no live defect |
| Cross-cutting `MAX_CPUS` | **Confirmed gap** — `cpu_mask: u8` caps it at 8, unasserted; 622 tests pass at 9 |

### What I could not check

Recorded as explicitly as the rest, per the checklist's method note.

- **No guest was booted.** Everything above is host tests plus static reading. `cargo xtask
  test-qemu`, `check-display` and `check-terminal` were not run, so nothing here is confirmed
  against real SMP timing, real interrupt delivery, or a real park.
- **The initial-xAPIC-id vs Limine-`lapic_id` equivalence (A.3)** needs hardware or a QEMU CPU
  model where they differ. Untested in either direction.
- **Release-mode behaviour of the `MAX_CPUS` overflow.** I observed the debug panic; the
  release wrap-to-bit-0 is inferred from Rust's defined semantics, not observed.
- **`stats_snapshot`'s effect on any consumer.** I confirmed the format string prints `online=1`
  for a parked CPU; I did not check what, if anything, in userspace acts on that.
- **Whether reader 4's selftest miscount actually fails a harness run.** I traced it to a
  20-billion-spin timeout and a `MISMATCH` print, neither of which sets a failure verdict — but
  I did not run the selftest with a parked CPU to see what the run reports.
- **Section A only.** B (interrupt and entry paths), C (cross-CPU lifetime) and D (the evidence
  layer) were not examined, except where A.4 brushed against C.4 and is flagged there.

---

## Session 2 — 2026-08-14 — Section B (interrupt and entry paths)

### Method notes

Host-side and static, plus mutation of the gate and of the kernel source. Tools:
`cargo xtask check-irq-scope`, `cargo test --lib --target x86_64-unknown-linux-gnu` in
`kernel/` (622 tests), `cargo check --target x86_64-unknown-none`, and two standalone
`rustc` programs for language-semantics claims I did not want to assert from memory. No
guest was booted; see [What I could not check](#what-i-could-not-check-1).

**Baselines, taken first so a later "green" means something:**

```
check-irq-scope: 7 entry stub(s) → 6 scoped dispatcher(s) + 1 ring-3 entry point(s) ✓
test result: ok. 622 passed; 0 failed
```

Every mutation below was reverted and both baselines re-confirmed at the end of the
session; `git status` shows only this file.

**One of my own probes was broken and reported "pass" while measuring nothing** (B.5), and
this time the positive control caught it. It is written up in place, because what it
revealed — that the whole context-switch path is unreached by the host suite — is a larger
finding than the one I was chasing.

---

### B.1 — every interrupt entry stub opens a lock-order scope; audit the gate itself

> **Claim.** "`check-irq-scope` gates this — audit the gate itself: does it actually see
> every stub, including the newest?" (`tools/xtask/src/main.rs:3460–3614`.)

The gate states two rules (its own doc comment, `main.rs:3473–3476`): (1) every
`dispatch = sym NAME` in a naked stub must name a function `irq_dispatcher!` generated;
(2) that macro must still call `enter_interrupt`.

#### a. CONFIRMED — it sees every stub that exists today, and rule 1 is load-bearing

The 7 it counts are the complete set. Five are in `idt.rs` — the `exception_stub!` macro
(one source line, 31 expansions), `vec14`, `timer_stub`, `tlb_shootdown_stub`,
`reschedule_ipi_stub`, `device_irq_stub!` (one source line, 8 expansions) — plus
`syscall_entry` (`arch/x86_64/syscall.rs:326`). That is 7 `dispatch = sym` sites, 6
macro-generated dispatchers, and 1 ring-3 entry, matching the gate's output exactly.
`spurious_stub` (`idt.rs:461–464`) has no dispatcher at all (bare `iretq`), so there is
nothing for it to scope.

Negative control **M-B1a** — take `tlb_shootdown_dispatch` out of the macro and declare it
a plain `extern "C" fn`:

```
xtask: every interrupt entry must open a lock-ordering scope — …
  …/idt.rs:530: entry stub dispatches to `tlb_shootdown_dispatch`, which is neither
  defined by `irq_dispatcher!` nor asserts `assert_user_entry_safe()` — it would run
  without a lock-ordering scope
EXIT=1
```

Rule 1 is real and names the offender.

#### b. REFUTED — rule 2 is a substring match, and the scope can be opened and closed before the handler body runs

`main.rs:3546–3551` checks that the twelve lines after `macro_rules! irq_dispatcher`
*contain the text* `enter_interrupt`. Calling it and **keeping the guard** are different
things, and only the second is what the tracker needs.

Mutation **M-B1b** — `idt.rs:178`, `let _lock_scope = …` → `let _ = …`:

```
check-irq-scope: 7 entry stub(s) → 6 scoped dispatcher(s) + 1 ring-3 entry point(s) ✓
GATE_EXIT=0
cargo check --target x86_64-unknown-none:  Finished `dev` profile   (no warnings)
```

Green gate, silent compiler. `#[must_use]` — the one other defence, spelled out at
`lockrank.rs` with the message "dropping it immediately leaves the handler checked against
the interrupted context" — does not fire, because `let _ =` is the sanctioned way to
suppress it.

That the two bindings differ is a Rust drop-timing fact, so I ran it rather than assert it.
A standalone program mirroring the macro (guard, `Drop` restoring a floor, handler body
reading it):

```
floor seen by handler body, `let _lock_scope =`: 5
floor seen by handler body, `let _            =`: 0
```

`let _ =` restores the floor **before** the body executes — i.e. exactly the flat-tracker
behaviour that got the first tracker withdrawn from the Slice D PR (decision log
2026-07-29). rustc emitted no warning for either form.

#### c. REFUTED — the gate keys on one literal operand name, and a stub can leave its view silently

`main.rs:3500` is `line.split("dispatch = sym").nth(1)`. The operand name is a local
choice in `naked_asm!`, not a language requirement, and the tree already contains a `sym`
operand under a different name — `enter = sym crate::sched::thread_enter`
(`arch/x86_64/context.rs:203`).

Mutation **M-B1c** — rename `tlb_shootdown_stub`'s operand `dispatch` → `handler` (both
the `call {…}` and the binding) *and* un-macro its dispatcher, so it genuinely runs with no
scope:

```
check-irq-scope: 6 entry stub(s) → 5 scoped dispatcher(s) + 1 ring-3 entry point(s) ✓
GATE_EXIT=0
cargo check --target x86_64-unknown-none:  Finished `dev` profile
```

The escape is complete: an unscoped dispatcher, a green gate, a clean build. The only trace
is the count falling from 7 to 6 in a success line nobody diffs. The gate's emptiness guard
(`main.rs:3560`, "a check that silently finds nothing to check is worse than no check")
fires only at **zero** — it has no notion of an expected number.

#### d. Two smaller observations on the gate

- It walks `kernel/src/arch` only. Defensible (entry stubs belong there, and `check-arch`
  polices the boundary in the other direction), but the coverage claim is "arch-generic",
  not "tree-wide", and nothing stops a naked stub elsewhere.
- The `assert_user_entry_safe` exemption (`main.rs:3524–3544`) is matched **by function
  name, pooled across all files**, and by textual presence anywhere in the function's
  lines — including its comments. A dispatcher whose doc comment merely mentions the
  assertion is exempted.

#### e. CONFIRMED (and worth recording as a positive) — the tracker is live in every image xtask builds

Worth pinning because it is the precondition for any of this mattering. The tracker is
`#[cfg(all(debug_assertions, not(test)))]`. `cmd_build` runs bare `cargo build` with no
`--release` (`main.rs:311–316`) and `kernel_elf()` reads from `target/…/debug/`
(`main.rs:196–203`); `[profile.dev]` in `kernel/Cargo.toml` does not set
`debug-assertions = false`. So `xtask qemu`, `image`, `test-qemu` and `check-display` all
boot a kernel with the tracker compiled in.

---

### B.2 — nothing allocates or frees in IRQ/DPC context

> **Claim.** "remembering that an `ObjectRef` drop reaches the allocator. The PS/2 driver's
> `to_drop` hand-off exists for this; check every other DPC for the same hazard."

#### a. CONFIRMED — the premise is real

`ObjectRef::drop` (`object/header.rs:232`) → on last reference → `dispatch_destroy`
(`:259`) → `KBox::from_raw` drop → slab free. `SlabCacheState` is a plain `SpinLock` at
`LockRank::SlabCache` (`mm/slab.rs:144`), `BUDDY` a plain `SpinLock` at `LockRank::Buddy`
(`mm/heap.rs:29`). Plain, so interrupts stay enabled while held — which is exactly what
makes a same-CPU DPC-context free a self-deadlock.

#### b. CONFIRMED — all nine DPC handlers are clean, and the one exception's excuse checks out

| # | Handler | Site | Allocates / drops? |
|---|---|---|---|
| 1 | `pit_dpc_handler` | `ioapic.rs:186` | no — one `fetch_add` |
| 2 | `ps2_intr_dpc` | `ps2/mod.rs:219` | no — publishes to `to_drop` |
| 3 | `console_intr_dpc` | `console.rs:175` | no — publishes to `to_drop` |
| 4 | `ahci_intr_dpc` | `ahci.rs:639` | no — `signal_interrupt` only |
| 5 | `ahci_drain_dpc` | `ahci.rs:648` | no — `drain_queue` → MMIO |
| 6 | `irp_complete_dpc` | `block.rs:285` | no — pushes the box to `RECLAIM` |
| 7 | `noop_dpc` | `irp.rs:183` | no — empty |
| 8 | `intr_test_dpc` | `io/mod.rs:26` | no — `signal_interrupt` only |
| 9 | **`stub_fill_dpc`** | `file_object.rs:735` | **yes — drops 2 `ObjectRef`s + frees its `KBox`** |

Handler 9 carries a comment claiming it is safe "only because `Producer::Stub` is
constructed solely from `#[cfg(test)]` code". **Verified**: all eight construction sites
(`file_object.rs:784`, `userspace_server.rs:600`, `addr_space.rs:1173/1187/1211/1234/1244/1257`)
lie inside `#[cfg(test)] mod` blocks opening at `:775`, `:395` and `:832` respectively. The
claim holds and the exception is honest.

The shared wake path is clean too: `complete_pending_op` (`sched.rs:1549`) →
`signal_pending_op_with_result` → `make_runnable` (`sched.rs:2347`), which on placement
failure **panics** rather than dropping the ref (`sched.rs:2358`) — so no `ObjectRef` drop
leaks into DPC context through the one call every completion DPC makes.

#### c. REFUTED as evidence — nothing anywhere tests the hand-off

`drivers/ps2/mod.rs`, `drivers/console.rs` and `drivers/ahci.rs` contain **zero** `#[test]`
functions. `io/block.rs` has three, covering partition rebasing and fragment building —
none touches `RECLAIM` or `irp_complete_dpc`.

Mutation **M-B2** — reinstate the 2026-08-06 deadlock: replace `ps2_intr_dpc`'s publish to
`to_drop` with a plain `drop(pr)`:

```
cargo check --target x86_64-unknown-none:  Finished `dev` profile     (clean)
check-irq-scope: 7 entry stub(s) → 6 scoped dispatcher(s) + 1 ring-3 entry point(s) ✓
test result: ok. 622 passed; 0 failed
```

Nothing objects. The invariant that three separate PR reviews and a decision-log entry were
spent establishing is held in place by comments alone, in three drivers with the identical
shape, and a regression would surface only as an intermittent same-CPU hang under real
input load.

#### d. Adjacent, and unaccounted for — the interrupt-tail stack budget

A device IRQ nests onto whatever kernel stack is current (`TODO(irq-stack)`,
`mm/kstack.rs:171`); `KERNEL_STACK_PAGES = 4`, so 16 KiB total. On that stack a single tail
can place, in sequence: the stub's `ExceptionFrame` (176 B), `run_pending_in`'s
`[0usize; DPC_RESERVE]` (512 B, `dpc.rs:125`), and then `ps2_intr_dpc`'s
`[0u8; DRAIN_MAX]` — `DRAIN_MAX = RING_EVENTS × INPUT_EVENT_LEN = 128 × 16 = 2048 B`
(`ps2/mod.rs:158`, `:222`). Roughly 2.8 KiB of interrupt-context stack, on top of whatever
the interrupted syscall was using — which A.6 measured at ~2.8 KiB for a 32-handle
`sys_wait`. A.6's compile-time check is scoped to "the wait path" and covers none of this.
Not shown to overflow; recorded because nothing accounts for it and nothing would notice.

---

### B.3 — `ps2::poll` from `timer_dispatch`: rank, and the drain budget

> **Claim.** "It takes an `IrqSpinLock` in interrupt context ahead of the DPC drain.
> Confirm the rank is right and that a drain cannot exceed its budget with interrupts
> masked."

#### a. CONFIRMED — the rank is right

`PS2` is `LockRank::Leaf` (`ps2/mod.rs:105`), and Leaf's contract is "taking nothing while
held". Checked every call under the guard: `crate::arch::ps2::read_byte` is port I/O;
`Timer::read_ns` (`timer.rs:138`) is lock-free (`rdtsc` plus three relaxed loads) **and** is
called before the acquire in all three of `drain_controller` (`:282`), `ps2_intr_dpc`
(`:220`) and `submit_read` (`:191`); the ring and decoder operations are array writes. The
guard is also released before `dpc::enqueue` in every caller — `drain_controller` returns
its `bool` and drops `g` at the function boundary, so `PS2` (Leaf) is never held across
`DPC_QUEUE` (also Leaf). That matters: the tracker forbids same-rank nesting outright
(`lockrank.rs:250–284`, `(rank as u8) <= held`), so a nested pair would panic rather than
deadlock.

The ordering `poll` → `run_pending` → `on_timer_tick` (`idt.rs:493, :496, :497`) is as
documented, and the `SCHED` acquire inside `on_timer_tick` happens at depth 0 within the
interrupt scope, so no inversion.

#### b. REFUTED — the drain has no budget. The named invariant does not exist

The *controller* drain is bounded: `MAX_DRAIN_PER_IRQ = 64` (`ps2/mod.rs:164`, enforced at
`:297`), with a comment spelling out why an unbounded interrupts-off window stops the
machine. The *DPC* drain that follows it, at every interrupt tail, is
`loop { … }` until the queue is empty (`dpc.rs:123–145`) with no iteration cap — and
`run_pending_in` clears `queued` **before** invoking each handler (`dpc.rs:141`)
specifically so a handler may re-enqueue itself.

Probe (temporary test in `dpc::tests`, a DPC that re-enqueues itself; the 10 000 bound is
the *handler's*, added so the suite terminates):

```
test dpc::tests::audit_dpc_drain_has_no_budget ... ok
```

One `run_pending_in` call ran the handler 10 000 times. Negative control — put a 4-round
cap in `run_pending_in` and re-run the same probe:

```
assertion `left == right` failed: a single run_pending_in ran the handler 10_000 times —
the drain has no budget
  left: 4
 right: 10000
```

So the probe is not vacuous in either direction. **No production DPC re-enqueues today**
(checked all nine in B.2b), so this is latent, not live. But the property the checklist asks
me to confirm is absent, and `drain_controller`'s own comment thirty lines away makes
precisely the argument for why it is needed.

#### c. Adjacent — the console's drain is unbounded, on the same reasoning the PS/2 driver rejects

`console_isr` (`console.rs:228–240`) holds `CONSOLE` (Leaf, interrupts masked) across
`while console_rx_ready() { … }` with no bound. `drain_controller`'s comment names this
explicitly — "The console's equivalent loop is unbounded and gets away with it because a
UART with nothing arriving stops immediately; an input controller with two attached devices
is not that." That is an assumption about the *backend*, and under QEMU the console's
backend is a host chardev, not a 115200-baud line. I did not test it; recorded as a stated
assumption with no test behind it.

---

### B.4 — EOI ordering and spurious interrupts

> **Claim.** "Is an EOI ever issued twice, or skipped on an early return? What happens on a
> spurious vector?"

#### a. CONFIRMED — exactly four EOI sites, none doubled, none skippable

`Irq::eoi` has one implementation (`apic.rs:204`) and exactly four callers, one per
returning-interrupt dispatcher:

| Vector | Dispatcher | EOI position |
|---|---|---|
| `0x20` timer | `timer_dispatch` | **first** (`idt.rs:484`), before `poll` / DPC drain / tick |
| `0x40` TLB IPI | `tlb_shootdown_dispatch` | **last** (`idt.rs:541`), after `tlb::on_ipi` |
| `0x41` resched IPI | `reschedule_ipi_dispatch` | **first** (`idt.rs:585`) |
| `0x30–0x37` device | `device_irq_dispatch` | after the handler (`idt.rs:635`) |

No exception vector EOIs — correct, they are not APIC deliveries. In `device_irq_dispatch`
the EOI sits **outside** the `if slot < DEVICE_IRQ_COUNT` / `if h != 0` block, so an
unregistered or out-of-range vector still acknowledges; there is no early return anywhere in
the four. Nothing EOIs twice.

The shootdown dispatcher acking before its EOI cannot lose an IPI: a second shootdown
requires the first to have been acked by every target and `tlb::LOCK` re-taken, so at most
one 0x40 delivery is ever pending in the IRR, which one bit represents exactly.

#### b. CONFIRMED — the LAPIC spurious vector is handled correctly

`SPURIOUS_VECTOR = 0xFF` (`arch/irq.rs:17`) is programmed into the SVR alongside the
software-enable (`apic.rs:166`), and its stub is a bare `iretq` with **no EOI**
(`idt.rs:461–464`) — which is right: a spurious interrupt is the controller reporting it had
nothing to deliver, and EOI-ing it would retire an unrelated in-service interrupt. Ordering
is safe too: `idt::init` (and therefore the 0xFF gate) runs from `cpu.rs:44` during early CPU
init, long before `Irq::init` software-enables the APIC at `main.rs:174`.

#### c. Divergence, sound today but on an undocumented contract

Three dispatchers EOI **first**, each with a comment giving the reason ("the handler may
switch away and not return to this frame"). `device_irq_dispatch` EOIs **after** running the
registered handler. That is fine for the five handlers that exist — `pit_tick`, `kbd_isr`,
`aux_isr`, `console_isr`, `ahci::isr` all drain and return without blocking or
rescheduling — but `register_device_handler` (`idt.rs:660–665`) states no such requirement,
and a future handler that blocks would defer this CPU's EOI for as long as it ran, silently
gating every interrupt at or below that priority.

#### d. UNCHECKED — the 8259s are masked but never remapped

`IrqRouter::init` writes `0xFF` to both PIC data ports (`ioapic.rs:243–247`) and masks every
IOAPIC RTE (`:232–241`), but never reprograms the PICs' vector base. It is left wherever
firmware put it — conventionally `0x08` for the master, which is the **CPU exception range**
(`#DF` at 8, `#GP` at 13). Any 8259 delivery would therefore land on `vec8`/`vec13`/`vec15`
and `dump_and_halt` reporting a fault that did not occur.

The practical window is closed, for two independent reasons, and I checked both: IF is 0
from boot until `sched_bringup` (`main.rs:360`), which runs long after `IrqRouter::init`
(`main.rs:224`) — the PIT self-test's brief `interrupts_enable` (`ioapic.rs:295`) is inside
`IrqRouter` and after the masking, as its own comment says; and a fully-masked 8259 never
raises INTR, so it cannot enter the acknowledge cycle that produces a spurious IR7 in the
first place. Recorded as a defence-in-depth gap rather than a defect: the cost is not a
lost interrupt but a **misdiagnosis** if the assumption about the firmware-left vector base
is ever wrong. I could not test it — it needs hardware, or a model that will glitch a PIC
line.

#### e. Instrument note — an APIC assertion that cannot fire on an AP

`apic.rs`'s `read_reg`/`write_reg` both open with
`debug_assert!(ENABLED.load(…), "LAPIC accessed before init")` (`:88`, `:99`), but `ENABLED`
is a **single global** `AtomicBool` (`:75`) while `enable_this_cpu` runs per CPU. Once the
BSP sets it, the assertion is satisfied for every AP regardless of that AP's actual APIC
state, so it can only ever catch a BSP-ordering mistake. Rule 1, in the small.

---

### B.5 — preempt-disable/enable balance and `RESCHED_PENDING` replay

> **Claim.** "Every `preempt_disable` has a matching enable on every path including early
> returns; `RESCHED_PENDING` is replayed rather than dropped."

#### a. CONFIRMED — the four pairs are balanced, and there is no unbalanced shape to find

Four call sites, each read end to end:

| Site | Region | Early return between? |
|---|---|---|
| `tlb.rs:146` / `:200` | the shootdown window | no — the only `return` is the sole-online-CPU fast path at `:129`, **before** the disable |
| `handle/global.rs:130` / `:137` | a batch of `ObjectRef` drops | no — a `for` loop; the `return` is after the enable |
| `sched.rs:3102` / `:3106` | `reap_pending`'s drop loop | no — the `return` at `:3107` is after the enable |
| `spinlock.rs:92` / `:148` | every plain `SpinLock` section | `lock()` cannot fail (it spins); the enable is in `Drop` |

`SpinLock` has **no `try_lock`** — only `IrqSpinLock` does (`spinlock.rs:256`), and that one
never touches the preempt counter, so its contended path (`:267`) has nothing to unbalance.
`panic = "abort"` means there is no unwind path to leak a disable either.

#### b. CONFIRMED, with the limit stated — the latch is not dropped at depth > 0

`preempt_enable` (`sched.rs:568–584`) is
`PREEMPT_OFF[me].fetch_sub(1) == 1 && RESCHED_PENDING[me].swap(false)`. Rust `&&`
short-circuits, so the latch is only consumed when the depth actually reaches zero — a
nested inner enable cannot eat an outer window's pending reschedule. That is the part the
checklist asks about, and it holds.

What *is* dropped: at depth zero the latch is swapped to `false` unconditionally, but the
replay only switches if this CPU is running its idle thread. A quantum expiry latched during
a preempt-critical window on a **busy** CPU is therefore consumed and discarded, backstopped
by that CPU's next tick — `TICK_NS = 10_000_000` (`sched.rs:85`), so up to 10 ms.
Documented behaviour, not a defect. (Minor doc drift while I was there: `resched_if_idle`'s
comment calls the same wait "(~5 ms)" where `idt.rs:645` calls it "the next 10 ms tick".)

#### c. Gap — the invariant is enforced on the involuntary paths only

`PREEMPT_OFF` has exactly two readers outside the disable/enable pair: `on_timer_tick`
(`sched.rs:1154`) and `resched_if_idle_locked` (`:1184`). Both correctly latch instead of
descheduling. Nothing checks it on the **voluntary** paths: `yield_now` (`sched.rs:1119`)
calls `switch_to_next` with no such check, and `switch_into` (`sched.rs:2189`) — the single
choke point all five switch call sites pass through, which already hosts
`assert_switch_safe()` at `:2252` for the neighbouring invariant — does not assert it
either.

`assert_switch_safe` covers the case only incidentally: it fires on a non-empty *lock*
stack, so it catches a preempt-critical window that also holds a lock, and misses one raised
by a bare `preempt_disable()` (the `tlb`, `handle::global` and `reap_pending` regions). I
found no reachable violation today — none of those three regions yields or blocks — so this
is a missing guard, not a live bug.

#### d. My probe for (c) was broken, and what that exposed is bigger than (c)

I added the missing assertion next to `assert_switch_safe`:

```
test result: ok. 622 passed; 0 failed
```

Which looks like confirmation and is worth nothing. Positive control — invert it to
`debug_assert_ne!(…, 0, "POSITIVE CONTROL: this line is reached and evaluated")`, which
must fail if the line ever executes:

```
test result: ok. 622 passed; 0 failed
```

**`switch_into` is never reached by the host suite.** It is not `cfg`-gated; host tests
simply never perform a context switch (`context_switch` is naked asm). So my 622-green
result said nothing about the assertion — and, more importantly, the entire switch path has
**zero host-test coverage**: the FPU save/restore ordering, the `on_cpu` switch-out guard,
the CR3-before-stack-swap ordering, and `assert_switch_safe` itself — the check the lock
tracker names as its per-CPU model's soundness precondition (`lockrank.rs` § Per-CPU, not
per-thread). All of it is covered only by booting.

#### e. `PREEMPT_OFF` underflow is silent, and the dev profile does not catch it

`PREEMPT_OFF` is `[AtomicU32; MAX_CPUS]` (`sched.rs:522`) decremented by a bare `fetch_sub`
with no floor. An unbalanced enable at depth 0 wraps it to `u32::MAX`, and that CPU never
preempts again — the failure `preempt_disable`'s own comment names ("sticking it nonzero —
that CPU would never preempt again"), in the other direction. `[profile.dev]` sets
`overflow-checks = true`, which does not help: atomic RMW ops wrap by definition. Shown
rather than assumed, under `-C overflow-checks=yes -C debug-assertions=yes`:

```
prev = 0, depth now = 4294967295
`prev == 1` (the replay condition) = false
```

No path today is unbalanced (b, above), so this is a missing guard on an invariant that
currently holds.

---

### Cross-cutting — the lock-tracker's host tests test a copy, and the copy has already drifted

Bears on B.1 and B.3, since both rest on the tracker doing what it says.

`lockrank.rs`'s test module states openly that it "reproduces" the arithmetic "over a plain
local struct" because the real tracker's per-CPU state is invalid on the host. That is a
sound trade — but it means the nine tests exercise `tests::State::acquired` (`:518`), not
`tracker::acquired` (`:250`), and any drift between them is invisible.

The drift is not hypothetical; it exists now. `tracker::acquired` has a branch the copy does
not:

```rust
// tracker::acquired, lockrank.rs:266–270
if matches!(rank, LockRank::TlbShootdown) {
    contract = Some(held);
    break;
}
if (rank as u8) <= held { violation = Some(held); break; }
```

versus the copy's single `if held != 0 && (rank as u8) <= held`. That branch enforces the
F1 caller contract — nothing may be held when `tlb::LOCK` is taken — which the *rank* alone
does not express: `TlbShootdown` is 85, so taking it under `Sched` (10) or `HandleTable`
(30) is not a rank inversion and only this check catches it. It has no test in either copy,
and `report_contract` has no test either.

---

## Summary — Section B

| Item | Verdict |
|---|---|
| B.1 (a) gate sees every stub | **Confirmed** for today's 7; rule 1 load-bearing (M-B1a fails correctly) |
| B.1 (b) rule 2 ("still calls `enter_interrupt`") | **Refuted** — substring match; `let _ =` closes the scope before the body, gate green, no warning |
| B.1 (c) stub coverage | **Refuted** — keyed on the literal `dispatch = sym`; a renamed operand drops a stub silently (7 → 6, still ✓) |
| B.1 (e) tracker live in shipped images | **Confirmed** — xtask always builds the dev profile |
| B.2 (a,b) no alloc/free in IRQ or DPC context | **Confirmed** — 9 handlers clean; the one exception's `cfg(test)`-only claim verified across all 8 construction sites |
| B.2 (c) evidence for it | **Refuted** — no test anywhere; M-B2 reinstates the 2026-08-06 deadlock and 622/622 still pass |
| B.2 (d) interrupt-tail stack budget | **Unaccounted** — ~2.8 KiB of DPC-context stack outside A.6's check |
| B.3 (a) `PS2` rank | **Confirmed** — Leaf honoured; never held across `DPC_QUEUE` or `SCHED` |
| B.3 (b) drain budget | **Refuted** — `run_pending_in` has no cap at all (10 000 runs from one call); latent, no DPC re-enqueues today |
| B.3 (c) console drain | **Unchecked** — unbounded by a stated assumption about the UART backend, with no test |
| B.4 (a) EOI exactly once, never skipped | **Confirmed** — 4 sites, one per returning dispatcher |
| B.4 (b) LAPIC spurious vector | **Confirmed** — bare `iretq`, no EOI, gate installed before the APIC is enabled |
| B.4 (c) device EOI-after-handler | **Confirmed sound today**, on a contract `register_device_handler` does not state |
| B.4 (d) 8259 vector base | **Unchecked** — masked but never remapped; unreachable today, misdiagnoses if it ever is not |
| B.5 (a) disable/enable balance | **Confirmed** — 4 pairs, no early return, no `SpinLock::try_lock` to unbalance |
| B.5 (b) `RESCHED_PENDING` replayed | **Confirmed** — `&&` short-circuit keeps the latch across nesting |
| B.5 (c) invariant enforced at the switch | **Confirmed gap** — involuntary paths only; `switch_into`/`yield_now` unchecked |
| B.5 (d) host coverage of the switch path | **Confirmed gap** — positive control proves `switch_into` is never reached by any host test |
| B.5 (e) `PREEMPT_OFF` underflow | **Confirmed gap** — silent wrap; `overflow-checks` does not cover atomics |
| Cross-cutting lockrank tests | **Confirmed gap** — tests a copy; the copy is already missing the `TlbShootdown` contract branch |

### What I could not check

- **No guest was booted.** `test-qemu`, `check-display` and `check-terminal` were not run.
  Nothing here is confirmed against real interrupt delivery, real EOI behaviour, or real
  timing.
- **That the interrupt scope is actually open at runtime.** B.1(b) shows the gate cannot
  tell; proving the live behaviour needs a boot with a deliberate inversion.
- **The 8259 spurious path (B.4d)** — needs hardware or a PIC-glitch model.
- **The interrupt-tail stack high-water mark (B.2d).** I summed the arrays statically; I did
  not measure a real tail against the `test-harness` watermark.
- **The console's unbounded drain (B.3c)** under a fast host chardev.
- **Whether the unbounded DPC drain (B.3b) is reachable at all in a future slice.** I
  confirmed no handler re-enqueues today; I did not analyse what a per-CPU DPC queue (the
  stated SMP trajectory) would change.
- **Anything concurrent.** Every mutation was adjudicated by a single-threaded host suite
  and a static gate. Section B's subject is interrupts, and neither instrument has any.
- **Section B only.** A was covered in session 1; C (cross-CPU lifetime) and D (the evidence
  layer) were not examined — though B.2(c), B.5(d) and the cross-cutting finding are all
  D-shaped and should be read alongside D when it is done.

---

## Session 3 — 2026-08-14 — Section C (cross-CPU lifetime and ownership)

### Method notes

Host-side and static, as sessions 1 and 2: `cargo test --lib --target x86_64-unknown-linux-gnu`
in `kernel/` (624 tests) and `cargo xtask test` (1679 across the workspace). No guest was
booted; see [What I could not check](#what-i-could-not-check-2).

**Positive control first.** The four tests this section leans on were confirmed present and
passing by name before any mutation, so a later "FAILED"/hang is the mutation and not a
missing test:

```
test sched::tests::drain_pending_drops_moves_all_and_preserves_capacity ... ok
test sched::tests::stealable_respects_affinity ... ok
test sched::tests::reap_matching_moves_only_same_pid_threads ... ok
test sched::tests::has_live_siblings_scans_all_parked_lists ... ok
test result: ok. 624 passed; 0 failed
```

Baseline `cargo xtask test`: `total passed: 1679  total failed: 0`. Every mutation below was
reverted; `git status` showed only this findings file before and after, and both suites were
green again at the end.

Mutations used, by name: **M-C2a** (delete `stealable_to`'s `on_cpu` clause), **M-C2b**
(force `Thread::is_on_cpu` to `true`), **M-C3** (delete `place_thread`'s capacity guard),
**M-C4a/M-C4b** (+ control) (a read guard that never drops), **M-C5** (replay `read_ns`
outside the tree).

---

### C.1 — `deferred_drops` cannot grow unbounded, and every producer has a drainer

> **Claim.** "**`deferred_drops` cannot grow unbounded**, and every producer has a drainer
> that runs."

#### a. CONFIRMED — one producer, and the bound is 4 against a reserve of 8, for the life of the machine

`deferred_drops` (`sched.rs:756`) is written at exactly one site — `wake_entropy_seed_waiters`
(`sched.rs:1331`). Grep over the whole tree finds no other `try_push`/`push` into it.

The bound argument holds, and it is stronger than the doc comment claims. `DEFERRED_DROP_RESERVE`
is `2 * SEED_WAITERS_MAX` = 8 (`sched.rs:126`, `entropy.rs:64`). The producer can only ever move
in refs that came out of `entropy::seed_waiters`, and that list is governed by two rules that
together cap the *lifetime* total at 4:

- `register_waiter` (`entropy.rs:176`) refuses once `seeded` — it returns `AlreadySeeded(po)`,
  handing the ref back — and otherwise refuses past `SEED_WAITERS_MAX`.
- `drain_waiters` (`entropy.rs:192`) returns 0 while `!seeded`.

So queuing happens only before the latch and draining only after it, and `seeded` is a one-shot
latch: the only write is `self.seeded = true` inside `if !self.seeded` in `maybe_latch`
(`entropy.rs:157–160`), and nothing anywhere clears it. Total refs that can ever reach
`deferred_drops` over a boot: ≤ 4. Verdict: **confirmed**, with the reserve at 2× the true
worst case.

#### b. CONFIRMED for the drain, UNCHECKED for "runs"

`drain_pending_drops` (`sched.rs:3095`) is genuinely pinned:
`drain_pending_drops_moves_all_and_preserves_capacity` asserts both lists empty *and* both
capacities unchanged — the second pair is the assertion that catches the F11 regression
(`mem::take` swapping in a zero-capacity `KVec`), which is the failure the test exists for.
That is a test with teeth.

What is not pinned is the "**runs**" half. `reap_pending` — the function that actually calls
the drain, and that also drives `io::block::reclaim_completed`,
`drivers::console::reclaim_completed` and `drivers::ps2::reclaim_completed` — is reached by no
host test at all. It is the same coverage hole B.5(d) found for `switch_into`, one layer up.
Verdict: **confirmed** that the drain moves everything; **unchecked** that any caller invokes
it on a running machine.

#### c. REFUTED as a general invariant — the reserve discipline that protects `deferred_drops` protects four sibling lists by assertion only, and one by nothing

This is the finding, and it is a class rather than an instance. Nine pushes happen under `SCHED`
(rank-1, IRQ-acquired). They split three ways:

| Site | Guard against a full list |
|---|---|
| `place_thread` (`sched.rs:3399`) | **by construction** — `if len >= capacity { return Err(r) }` |
| `deadline::push` (`sched.rs:194`) | **by construction** — `if len >= capacity { return Err }` |
| `ended_pids` (`sched.rs:2915`) | **by construction** — `if len < capacity` before the push |
| `blocked` (`sched.rs:2397`) | `debug_assert!` only |
| `ready_slot` in `switch_to_next` (`sched.rs:2640`) | `debug_assert!` only |
| `suspended` (`sched.rs:3008`) | `debug_assert!` only |
| `wake_reaper` → `ready[cpu]` (`sched.rs:3732`) | `debug_assert!` only |
| `reap` ×3 (`sched.rs:2740`, `2788`, `2804`) | **nothing** |
| `deferred_drops` (`sched.rs:1331`) | **nothing** |

The last six do not merely risk a panic at the boundary. `KVec::try_push` **grows**:

```rust
// libkern/kvec.rs:70
pub fn try_push(&mut self, val: T) -> Result<(), AllocError> {
    if self.len == self.cap {
        self.grow(1)?;          // -> realloc_to -> kmalloc(...) + kfree(old)
    }
```

So at the boundary the `.expect("… within reserve")` does not fire first — `kmalloc`/`kfree`
run **under the rank-1 IRQ-acquired `SCHED` lock**, and the `expect` only fires if that
allocation *fails*. That is F11 verbatim (decision log 2026-07-21), and `reap_pending`'s own
doc comment at `sched.rs:3092` names it: "every later exit-path push *allocated under `SCHED`*
via `KVec::try_push` growth — the F11 deadlock hazard". The fix that comment describes removed
one route to that state; it did not make the pushes refuse.

Nothing today is known to exceed a reserve, so this is a missing guard on an invariant that
currently holds — the same shape as B.5(e). It is worth recording because the reserves are
what the invariant rests on, and two of them have been raised under fire already
(`BLOCKED_RESERVE` 16 → 64 on 2026-08-13, at a boot panic; `deadline::HEAP_RESERVE` is still
16 with a `sys_wait` deadline per blocked thread). `blocked` is the one with a live precedent
and it has only a `debug_assert`.

#### d. The sibling parking pools, enumerated

C.1 asks about `deferred_drops`, but it is one of six pools of "released here, dropped later":

| Pool | Producer | Drainer | Bounded by |
|---|---|---|---|
| `SchedState::deferred_drops` | entropy seed wake | `reap_pending` | 4 for the boot (a) |
| `SchedState::reap[cpu]` | exit / sweep paths | `reap_pending`, **same CPU only** | `REAP_RESERVE` (c) |
| `io::block::RECLAIM` | `irp_complete_dpc` | `reap_pending` | outstanding IRPs |
| `console`/`ps2` `to_drop` | the completion DPCs | `reap_pending` + `submit_read` | one slot each |
| `SchedState::ended_pids` | `exit_process` | the reaper thread | explicit capacity check |
| `IpcChannel::pending_sends` cancelled entries | the `PendingSend` deadline | **the peer's next `recv`, or close** | the endpoint's array |

Two are worth a note.

The `to_drop` single-slot hand-off carries a `debug_assert!(g.to_drop.is_none(), "a second
completion before a reclaim")` (`console.rs:206`, `ps2/mod.rs:246`) — and if that ever did
happen, the assignment `g.to_drop = Some(pr)` would **drop the previous `ParkedRead` in place,
in DPC context**, which is the exact hazard the mechanism exists to prevent. I traced the
claim rather than trusting it and it holds: `submit_read` calls `reclaim_completed()` before
taking the lock (`console.rs:145`), a second read cannot park while one is parked (single
reader, `WouldBlock` at `console.rs:154`), and the DPC only publishes after *taking*
`g.parked`. So the slot is genuinely single-occupancy — but the protection is the parking
protocol, not the assert, and the assert is what a reader sees.

The IPC row is the only pool whose drainer is **not guaranteed to run**: a `BlockBounded` send
that times out is flagged `cancelled` (`ipc_channel.rs:604`) and its `po` + transfers are
reclaimed only when the peer next receives or the endpoint closes. A peer that does neither
holds them indefinitely. Bounded by the endpoint's `pending_sends` array, so it is a stall and
a leak rather than unbounded growth — but "every producer has a drainer that **runs**" is
false for this one.

#### e. The entropy pre-reserve is best-effort, and its stated failure mode is the wrong one

```rust
// entropy.rs:229-230 — "Best-effort: a failed reserve just means no waiters can be
// queued (they'll be told to retry) …"
let mut waiters = KVec::new();
let _ = waiters.try_reserve(SEED_WAITERS_MAX);
```

That is not what a failed reserve does. `register_waiter` checks `len() >= SEED_WAITERS_MAX`,
which is `0 >= 4` = false on an unreserved list, so it falls straight through to
`self.seed_waiters.try_push(po).expect("within reserved seed-waiter capacity")` — which grows
(§c), i.e. **allocates under the `ENTROPY` leaf lock**, which is the D4 inversion the comment
two lines above says it is avoiding; and if that allocation fails, the `expect` panics inside
`sys_entropy_read`. Not reachable in practice (a 32-byte reserve at early boot), so this is a
comment that is wrong rather than a bug, but the comment is the only thing standing between
the code and the inversion.

---

### C.2 — the switch-out race (`on_cpu` guard) still holds

> **Claim.** "it was fixed once (2026-07-01); confirm no path re-reads `current` without it."

#### a. CONFIRMED (statically) — four consumers of a parked context, four guards

Every place that reads a thread's parked context — its `saved_sp`, its FP image, or its kernel
stack — checks the guard:

| # | Consumer | Guard |
|---|---|---|
| 1 | `switch_into` on the incoming thread (`sched.rs:2269`) | spins `while is_on_cpu(next_obj)` |
| 2 | `stealable_to`, via `steal_one`/`steal_available` (`sched.rs:3483`) | `!is_on_cpu(obj) &&` |
| 3 | `reap_matching` (`sched.rs:2734`) | spins before `list.remove(i)` |
| 4 | `reap_blocked_matching` (`sched.rs:2764`) | spins before `blocked.remove(i)` |

The two that read `current[]` and do **not** guard are correct not to: `has_live_siblings`
(`sched.rs:2701`) reads only `owner_pid`, which is immutable after construction, and
`stats_snapshot` (`sched.rs:1284`) compares pointers and copies `Copy` counters.
`dequeue_front` does not filter either, which is a deliberate hole closed downstream by
consumer 1 — `switch_into`'s comment states exactly this and names the third-CPU
affinity-diverted wake that reaches it.

#### b. One reader of the parked context is unguarded, and sound only by an argument nobody wrote down

`thread_exception_frame` (`sched.rs:3078`) hands `sys_thread_get_registers` the *kernel-stack
address* of a suspended thread's `ExceptionFrame`, and the caller then reads it with `SCHED`
released (`syscall/table.rs:741–746`). The window is real: `suspend_with_fault` sets
`Suspended` and pushes into `suspended` **before** `switch_into` raises `on_cpu`
(`sched.rs:3002–3010`), so another CPU can observe `Suspended` and read the frame while the
faulting CPU is still executing on that stack.

It is sound today — the trap frame sits at the top of the stack and the switch pushes
callee-saved registers *below* it, so the bytes being read are never written. But the doc
comment justifies it differently and incorrectly: "the thread stays parked while suspended, so
the frame is stable to read after this returns". The thread is not yet parked at that moment;
what makes it stable is the stack layout, and that is stated nowhere. Verdict: **sound, on an
unstated invariant** — one stack-frame change away from being wrong, with the guard that would
have caught it right there and unused.

#### c. REFUTED as evidence — no host test ever sets `on_cpu`, so every one of the four guards is inert

`Thread::set_on_cpu` has exactly one caller in the tree: `switch_into` (`sched.rs:2288`).
B.5(d) already established that `switch_into` is reached by no host test. Therefore `on_cpu`
is `false` for every thread in every host test, and all four guards above are evaluated only
on the branch where they do nothing. Rule 2 ("assert where implementations differ") is
violated by construction here, not by oversight.

**M-C2a** — delete the guard from the one consumer a test does exercise:

```rust
// sched.rs:3483
- unsafe { !Thread::is_on_cpu(obj) && Thread::cpu_mask(obj) & (1 << me) != 0 }
+ unsafe { Thread::cpu_mask(obj) & (1 << me) != 0 }
```

```
test result: ok. 624 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

`stealable_respects_affinity` (`sched.rs:4177`) sets `cpu_mask` and never touches `on_cpu`, so
it passes for both the correct and the broken implementation — the third instance of the
pattern the audit's method section describes.

**M-C2b, the complementary positive control** — force `Thread::is_on_cpu` to return `true`
and confirm the call sites are actually *reached* (as opposed to dead):

```
test sched::tests::reap_matching_moves_only_same_pid_threads ...   [hangs; killed at 100 s]
```

`reap_matching`'s spin loop is entered and never leaves. So the guards are live code that host
tests reach — they are simply never handed the input where the two implementations differ.
Between M-C2a and M-C2b the situation is precise: **the guard is executed on every relevant
path and its `true` branch is unreachable from the host suite.**

---

### C.3 — `place_thread`'s `Err(r)` capacity path, on every caller

> **Claim.** "`place_thread`'s `Err(r)` capacity path hands the ref back for a drop outside the
> lock, on every caller."

#### a. REFUTED — four callers; two hand the ref back, two `panic!`

| Caller | `Err` handling |
|---|---|
| `spawn_inner` (`sched.rs:1070`) | carries `returned` out of the locked block, `drop(leftover)` after release ✓ |
| the user-thread spawn (`sched.rs:1150`) | same, plus the cloned `handle` ✓ |
| `make_runnable` (`sched.rs:2423`) | `if place_thread(g, r, true).is_err() { panic!(…) }` |
| `resume_suspended` (`sched.rs:3068`) | identical shape |

The two wake callers never hand the ref back — they take the machine down. That is a deliberate
choice, stated at both sites and again in `pick_target_cpu`'s comment ("Erroring instead is not
available: both wake callers `panic!` on `Err`"), so the checklist's claim is simply not what
the code does. As an enumeration: **refuted**.

#### b. CONFIRMED, on a property of the profile rather than of the code

In `if place_thread(g, r, true).is_err() { panic!(…) }` the `Result` is a temporary that lives
to the end of the `if` statement, so on the `Err` branch it holds an `ObjectRef` at the moment
`panic!` runs. Nothing drops it, because `kernel/Cargo.toml` sets `panic = "abort"` in **both**
profiles (`:30`, `:36`) — no unwinding, no destructors. Under an unwinding profile that
temporary would drop an `ObjectRef` — reaching `dispatch_destroy` → `SlabCache::free` — while
`SCHED` is held, which is F2 exactly. The `panic = "abort"` line is load-bearing for a lock-order
invariant, and neither site says so.

#### c. The `Err` is reachable on the wake paths, contrary to the "only if every permitted queue is full" reading

`pick_wake_cpu`'s comment says the fallback "has room unless *every* permitted queue is full —
the only case `place_thread` still refuses". `pick_target_cpu`'s final fallback is
`SchedState::this_cpu()` (`sched.rs:3357`), returned when **no** CPU accepts work — and that
path checks nothing about the chosen queue's length. So a wake that lands there with this CPU's
queue at `READY_RESERVE` panics, and "every permitted queue is full" is not the only way in.
The `spare` fallback one line above is load-aware (PR #201 review, finding 2); the last-resort
one below it is not.

#### d. REFUTED as evidence — the `Err` path is exercised by no host test

**M-C3** — delete the capacity guard entirely, so `place_thread` can never return `Err`:

```rust
// sched.rs:3398
- if g.ready[cpu].len() >= g.ready[cpu].capacity() {
+ if false {
      return Err(r);
```

```
test result: ok. 624 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

`wake_placement_falls_back_when_home_queue_is_full` fills a queue to its reserve, but it calls
`pick_wake_cpu` directly and never reaches `place_thread`. No test calls `place_thread` at all.
The whole subject of C.3 — the hand-back, the drop-outside-the-lock, and the two panics — has
no host coverage, and (per §b) the *safety* of the two panics rests on a Cargo profile that no
test reads either.

#### e. There is a fifth placement site that bypasses `place_thread` entirely

`wake_reaper` (`sched.rs:3726–3733`) pushes the reaper onto `ready[this_cpu()]` directly:
no affinity check, no `cpu_accepts_work`, no capacity check beyond a `debug_assert!`. It is
the only path that makes a thread runnable without going through the placement policy, and it
does it for the one thread whose starvation costs the machine its handle-table reclamation
(§C.4c). Sound today — a CPU executing `wake_reaper` is by definition not parked — but it is
a fifth opinion on placement, in the same shape as A.1's fourth and fifth opinions on
`online_mask`.

---

### C.4 — the thread stranded on a parked CPU

> **Claim.** "Known and unfixed: it is on no queue and cannot be rescued. Confirm it cannot
> hold anything the rest of the machine needs (a lock, a channel endpoint others block on) —
> **if it can, that is a finding**."

It can, three ways. All three are findings.

The setup is the same each time. A ring-0 fault reaches `dump_and_halt` → `Cpu::halt_loop()`
(`idt.rs:772`, `idt.rs:917`), and `halt_loop` runs `leave_online()` then `cli; hlt` forever
(`arch/x86_64/cpu.rs:56–70`). A kernel `panic!` reaches the same halt (`main.rs:1121`), and
there is no stop-IPI (deferred-decisions, F8), so **the rest of the machine keeps running** —
that is the whole premise of this checklist item. The thread that was executing stays in
`g.current[c]` forever, and nothing releases what it held. Note that ring-**3** faults do not
take this path (`exception_dispatch` routes them to `user_fault` → `suspend_with_fault`,
`idt.rs:766–772`), so the stranded thread is always one that was executing *kernel* code:
inside a syscall, an exception handler, or a kernel thread body.

The existing deferral for the parked-CPU class explicitly does **not** cover this: "The parked
CPU was running its *idle* thread, so this is interrupt routing and not a stranded thread"
(`deferred-decisions.md:1179`).

#### a. CONFIRMED FINDING — it can hold the handle table's RCU read context, and that wedges `HandleTable::close` for the whole machine

The strongest of the three, because the mechanism is entirely inside the parked thread's own
data and needs no lock to be held.

`HandleTable::lookup` opens a grace-tracker read section for the duration of its body
(`table.rs:461`), keyed on `current_ctx_id()` — which is the **dense CPU index**
(`handle/mod.rs:146–150`). The section is closed by `ReadGuard::drop` (`grace.rs:145`). With
`panic = "abort"` there is no unwinding, so a fault or panic anywhere inside `lookup` leaves
that context permanently non-quiescent, and `is_grace_period_past` (`grace.rs:122`) requires
**every** context to be quiescent or at a strictly later epoch.

The handle table is **global** — one table, one `GraceTracker`, for the whole kernel
(`handle/global.rs:1–13`). So one CPU's parked read section poisons reclamation for every
process.

**M-C4a** (temporary test in `handle::table::tests`, since reverted): forget a read guard on
context 5, then bump the epoch a thousand times.

```
M-C4a: after 1000 epoch bumps, is_grace_period_past(0) = false
M-C4a: and for the newest epoch (1000) = false
```

Permanent, and not escapable by advancing the epoch — the parked context's stored value never
moves, so every close deferred at or after that epoch is blocked forever.

**M-C4b** — the consequence. `close` pushes each deferred close into a `DEFER_RING_CAPACITY`
= 256 ring (`table.rs:32`), and when the ring is full it **spins without bound**:

```rust
// table.rs:630-639
loop {
    self.drain_expired(&mut guard);
    if guard.defer_ring.push(deferred).is_ok() { break; }
    drop(guard);
    yield_for_grace();          // production: core::hint::spin_loop()
    guard = self.inner.lock();
}
```

With one context parked, `drain_expired` frees nothing (it `break`s at the first blocked entry,
`table.rs:893`), so the ring never empties:

```
M-C4b: close #0
…
M-C4b: close #255
[no further output; killed by `timeout 60` — exit 124]
```

Close #256 — the 257th — never returns. Its negative control, byte-identical minus the
forgotten guard:

```
M-C4b control: completed 258 closes
test handle::table::tests::audit_mc4b_control_no_parked_ctx ... ok
```

So: after a ring-0 fault inside a handle lookup, the machine keeps running normally for exactly
256 more handle closes, and then **every `sys_handle_close` on every process hangs forever**,
in a spin loop, with no diagnostic. `close_owned_batch` degrades more gracefully (it returns
`(n, true)` rather than spinning, `table.rs:862–867`) — but its caller re-enters, and the slots
it could not close stay open, so process teardown stops making progress too.

The window is narrow — `lookup`'s body — but it is not exotic: the body dereferences a raw
`entries_ptr` from the directory and calls `try_acquire_refcount` on an object pointer read out
of the table, which is precisely what faults when a handle-table bug is what you are
diagnosing. And one path into it is *deliberately coded*: the
`debug_assert!(retries < 1024, "handle table lookup spinning past 1024 retries — logic bug")`
at `table.rs:523` panics **inside** the read section, and debug assertions are live in shipped
images (finding B.1(e): xtask always builds the dev profile). The assert that exists to
announce a logic bug is also the thing that converts it into a machine-wide hang 256 closes
later.

#### b. CONFIRMED FINDING — it can hold `SCHED` itself, and any plain spinlock

`dump_and_halt` releases nothing. It cannot: it has an `&ExceptionFrame` and no knowledge of
what the interrupted code held. The function's own header comment shows the problem was seen
for exactly one lock and generalised no further — "The emergency writer bypasses `SERIAL`'s
lock: the fault may have occurred while that lock was held. Sound under Phase 1's **single-CPU,
interrupts-masked model**" (`idt.rs:889–893`). That model is gone; the comment is stale.

If the fault lands while the thread holds `SCHED` (rank 1, `IrqSpinLock`), every other CPU's
next timer tick spins on it with interrupts masked and the machine stops entirely — no
diagnostic beyond the register dump already printed. If it lands while holding a plain
`SpinLock` — a slab or buddy cache (rank 6), the TLB-shootdown serialiser, the handle table's
rank-3 `inner` — every CPU that wants that lock wedges, which for the allocator is every CPU.
`leave_online()` fixes the one case where the machine only needed an *acknowledgement* from
the parked CPU; it cannot fix the cases where the machine needs something *released*.

Verdict: **the stranded thread can hold a lock the rest of the machine needs**, which is what
the checklist asked. Unlike (a) this is a wedge at the moment of the fault rather than 256
operations later, so it is more likely to be noticed — but it is also the case where the
diagnostic that just printed is the last thing that will ever print.

#### c. CONFIRMED FINDING — it holds its whole process open, so no peer blocked on its channels is ever released

`has_live_siblings` (`sched.rs:2701`) scans `g.current[]` by design — under SMP a sibling may
be running on another CPU, and tearing the process down under it would be a use-after-free. A
stranded thread is indistinguishable from a running one by that test, and stays in `current[c]`
forever. So `exit_thread` (`sched.rs:2850`) can never take the "last thread out" branch for
that process: `set_process_ended` is never called, `deliver_child_exited` never fires, the pid
never reaches `ended_pids`, and the reaper never runs `close_all_owned_by(pid)`.

The consequences are exactly the ones the checklist names:

- **Channel endpoints others block on stay open.** Closing an IPC endpoint is what nulls its
  peer and wakes blocked receivers (`sched::ipc_endpoint_closing`, `sched.rs:2098`; the
  comment at `sched.rs:2910–2913` calls this "the half that peers are waiting on"). A client
  blocked in `sys_wait` on a `recv` from a stranded server never sees `PeerClosed` and never
  times out — `SendMode::Block` passes `deadline_ns = u64::MAX` (`syscall/table.rs:2405–2409`)
  and `wait_on` heaps no deadline entry for `u64::MAX` (`sched.rs:2475`).
- **The parent never learns.** No `ChildExited` notification is delivered, so a supervisor
  blocked waiting for the child waits forever.

Two things bound this, and neither rescues the common case. A *queued* thread on a parked CPU
**is** rescuable — `steal_one`/`steal_available` deliberately do not gate victims on
`cpu_accepts_work` precisely so a parked CPU's queue drains (`sched.rs:3268–3272`), which is
why the checklist says "on no queue". And `exit_process` (`sched.rs:2874`) does **not** consult
`has_live_siblings` — it sweeps and ends the process unconditionally — so a *sibling* calling
`sys_process_exit` is a rescue path. But resource servers are single-threaded, so for the
processes whose endpoints other processes actually block on, there is no sibling to call it,
and no external killer exists ("The per-process thread list that would let an external killer
find these threads without a scan lands later", `sched.rs:2866–2867`).

---

### C.5 — cross-CPU TSC comparisons in the deadline heap (F10)

> **Claim.** "Saturating arithmetic is the stated mitigation; check it holds for the largest
> plausible skew, not just a small one."

#### a. REFUTED — the stated mitigation is not in the deadline path

The claim is stated at `docs/architecture/smp.md:487–491`: "the deadline heap compares
`Timer::read_ns()` values captured on different cores; **saturating arithmetic** makes a small
skew merely delay a firing."

Enumerating the arithmetic that actually runs:

- **Arming.** `sys_timer_set` takes an *absolute* `deadline_ns` from userspace and hands it
  straight to `deadline::push` (`sched.rs:2560–2570`). No kernel-side arithmetic, saturating
  or otherwise. Same for `sys_wait` (`syscall/table.rs:2139–2140`) and the `BlockBounded`
  send deadline.
- **Firing.** `fire_expired_deadlines` compares `if top.deadline_ns > now { break }`
  (`sched.rs:1345`). A plain comparison; nothing clamps.
- **The heap.** `push`/`pop_min`/`sift_*` compare `deadline_ns` directly (`sched.rs:194–280`).

There is exactly one `saturating_add` in the whole path — `let next = now.saturating_add(interval)`
in `fire_timer` (`sched.rs:1404`) — and it guards a *periodic interval* overflowing `u64`, not
cross-CPU skew. Verdict: **refuted as stated**; the doc names a mitigation that is not there.

#### b. The real behaviour is *wrapping*, and the tolerance is the elapsed time since the BSP's calibration — which is ~0 at AP bring-up

`X86Timer::read_ns` (`arch/x86_64/timer.rs:138`):

```rust
let now = regs::rdtsc();
let base = TSC_BASE.load(Ordering::Relaxed);
let delta = now.wrapping_sub(base);
(((delta as u128) * (mult as u128)) >> shift) as u64
```

`TSC_BASE` is a single global captured once, on the BSP, at the end of `Timer::init`
(`timer.rs:135`, called at `main.rs:188`); APs only `start_periodic` from the BSP's
calibration (`main.rs:398`), never re-`init`. So every CPU's `read_ns()` is
`(its own TSC − the BSP's TSC at calibration) × scale`, and skew enters unfiltered.

**M-C5** — the same two functions replayed verbatim outside the tree, at a 2 GHz TSC, with
`TSC_BASE` 25 ms into boot and the read 1 ms later:

```
tsc_hz=2000000000  mult=4611686018427387904  shift=63
TSC_BASE = 50000000 ticks (= 25 ms into boot)

skew (AP TSC - BSP TSC) -> read_ns() on the AP
  +0    (perfect sync)                   ->                1000000 ns
  +1 us                                  ->                1001000 ns
  +1 ms                                  ->                2000000 ns
  +1 s                                   ->             1001000000 ns
  -1 us                                  ->                 999000 ns
  -999 us (still < elapsed)              ->                   1000 ns
  -1 ms  (== elapsed: first underflow)   ->                      0 ns
  -2 ms                                  ->    9223372036853775808 ns  (292.271 years)
  -1 s                                   ->    9223372035855775808 ns  (292.271 years)
```

Three things fall out.

**The tolerance is not a fixed skew budget — it is `now_tsc − TSC_BASE`.** A negative skew is
absorbed only while it is smaller than the time elapsed since the BSP captured the base. That
quantity is *smallest exactly when the APs come up*, milliseconds after calibration, which is
also when a cold AP's TSC is most likely to disagree. A skew that the machine tolerates fine
at t = 10 s is a wrap at t = 30 ms.

**Past that point the failure is not graceful.** `wrapping_sub` puts `read_ns()` at ~9.2×10¹⁸ ns
(≈ 292 years), the top of the range rather than the bottom. "A small skew merely delays a
firing" is true either side of zero for skews inside the tolerance; outside it the value is not
delayed, it is meaningless.

**And one CPU's bad `now` corrupts the shared heap for the healthy ones.** The heap is global,
under `SCHED`, and both consumers of `now` write back into it:

```
fire_expired_deadlines: `top.deadline_ns > now` with now=9223372036853775808
  -> every armed entry is 'expired' (fires the whole heap at once)
fire_timer periodic re-arm: now.saturating_add(interval) = 9223372036854775808
  -> saturates to u64::MAX? false
```

So a skewed CPU's tick drains every armed timer and `sys_wait` deadline in one pass, and then
**re-arms each periodic timer 292 years out** — a value the `saturating_add` does not clamp,
because 9.2×10¹⁸ + an interval is nowhere near `u64::MAX`. Healthy CPUs then compare their own
sane `now` against those entries and never fire them again. One CPU's wrap is a permanent,
machine-wide loss of periodic timers. (`sys_wait` with no timeout is unaffected: `u64::MAX`
means "no deadline" and is never heaped, `sched.rs:2475`.)

#### c. The direction of a small skew is also mis-stated

Even inside the tolerance, "merely delay a firing" describes one of two cases. A deadline armed
on a CPU whose TSC runs *ahead* is compared against a *behind* CPU's `now` and fires late; the
reverse pairing fires **early**, by the skew. For `sys_wait` that is a premature `TimedOut`
returned to userspace, which is a correctness result rather than a latency one.

#### d. Verdict and scope

**Refuted** as written; the underlying deferral (verify or gate at real-hardware bring-up) is
still the right call, but the sentence that makes it sound already-mitigated should go. Under
QEMU/TCG and KVM with an invariant, synchronised TSC — the only configurations this project
boots — none of the above is reachable, and I did not attempt to construct a skewed guest.

---

### Cross-cutting — three of Section C's five items rest on a `#[cfg]`-invisible property

Worth stating once because it recurs: C.2's guards are inert in host tests because
`Thread::set_on_cpu` has one caller and it is `switch_into`; C.3's two panics are lock-safe
only because `panic = "abort"` is set in `kernel/Cargo.toml`; C.4(a)'s poisoning exists
*because* `panic = "abort"` is set (no unwinding means `ReadGuard::drop` never runs). The same
profile setting is load-bearing in opposite directions two items apart, and neither site
mentions it. A test cannot observe any of it, which is why all three needed a mutation.

---

## Summary — Section C

| Item | Verdict |
|---|---|
| C.1 (a) `deferred_drops` bounded | **Confirmed** — one producer; ≤ 4 for the boot against a reserve of 8 |
| C.1 (b) the drain is pinned | **Confirmed** for `drain_pending_drops`; **unchecked** that `reap_pending` ever runs (no host test reaches it) |
| C.1 (c) the reserve discipline generally | **Refuted** — 6 of 9 `SCHED`-held pushes have no capacity check; at the boundary `try_push` *grows*, i.e. `kmalloc` under rank-1 (F11), and the `expect` fires only if that allocation fails |
| C.1 (d) sibling pools | **Confirmed** for five; **refuted** for cancelled IPC pending-sends, whose drainer is the peer's cooperation |
| C.1 (e) entropy pre-reserve comment | **Refuted** — a failed reserve allocates under the leaf lock, it does not refuse |
| C.2 (a) four parked-context consumers all guard | **Confirmed** (static) |
| C.2 (b) `thread_exception_frame` | **Sound on an unstated invariant** — reads the parked context unguarded; safe by stack layout, not by the stated reason |
| C.2 (c) evidence for the guard | **Refuted** — M-C2a deletes it, 624/624 pass; M-C2b proves the sites are reached but never with `on_cpu` set |
| C.3 (a) every caller hands the ref back | **Refuted** — 2 of 4; the two wake callers `panic!` |
| C.3 (b) the panic path is lock-safe | **Confirmed**, but only via `panic = "abort"`; unwinding would drop an `ObjectRef` under `SCHED` (F2) |
| C.3 (c) `Err` reachable only when every permitted queue is full | **Refuted** — `pick_target_cpu`'s last-resort `this_cpu()` is not load-checked |
| C.3 (d) evidence | **Refuted** — M-C3 removes the guard entirely, 624/624 pass; nothing calls `place_thread` |
| C.3 (e) `wake_reaper` | **Confirmed gap** — a fifth placement site bypassing the policy entirely |
| C.4 (a) grace-tracker read context | **Confirmed finding** — machine-wide: every handle close hangs 256 closes after the park (M-C4b + control) |
| C.4 (b) locks | **Confirmed finding** — `dump_and_halt` releases nothing; `SCHED` or any plain lock wedges the machine |
| C.4 (c) channel endpoints | **Confirmed finding** — `has_live_siblings` sees `current[]`, so the process never ends and no peer is ever released |
| C.5 (a) saturating arithmetic is the mitigation | **Refuted** — the one `saturating_add` guards interval overflow, not skew |
| C.5 (b) largest plausible skew | **Refuted** — `wrapping_sub`; tolerance is time-since-calibration (~0 at AP bring-up), and past it one CPU poisons the shared heap permanently |
| C.5 (c) "merely delay a firing" | **Refuted** — the other pairing fires early (a premature `TimedOut` to userspace) |

### What I could not check

- **No guest was booted.** `test-qemu`, `check-display` and `check-terminal` were not run.
  Nothing here is confirmed against real scheduling, real faults, or real timing.
- **C.4 end to end.** I proved the grace-tracker wedge on the host and traced the fault path
  statically. I did **not** boot a guest with a deliberate ring-0 fault inside `lookup` and
  watch handle closes stop — which is the experiment that would settle it, and which the
  existing parked-CPU deferral already has a harness shape for (it parks a CPU mid-boot).
- **C.4(b) with a lock actually held.** Reading `dump_and_halt` establishes that it releases
  nothing; I did not construct a fault under `SCHED` and observe the wedge.
- **Whether a fault inside `lookup` is reachable in practice.** I identified one deliberately
  coded path (the 1024-retry `debug_assert`) and argued the raw dereferences; I did not
  measure how much of a real boot's time is spent inside that window.
- **C.5 against a skewed guest.** QEMU can be made to present unsynchronised TSCs, and I did
  not try it. Everything in C.5 is arithmetic replayed outside the kernel.
- **C.1(c) at the boundary.** I showed `try_push` grows and that six sites do not check; I did
  not drive any list past its reserve to watch the allocation happen under `SCHED` — that
  needs a guest, since `SCHED` is not taken by host tests.
- **Anything concurrent.** As in sessions 1 and 2: every mutation was adjudicated by a
  single-threaded host suite and static reading. Section C's subject is cross-CPU lifetime,
  and the instruments have no CPUs.
- **Section C only.** A (session 1) and B (session 2) are done; **D (the evidence layer) is
  not** — and C.1(b), C.2(c), C.3(d) and the cross-cutting note above are all D-shaped
  findings that belong alongside it. Between the three sessions, five of the guards examined
  (A.1's, B.2's, B.5's, C.2's, C.3's) have been shown to pass for both the correct and the
  broken implementation; that is now the dominant pattern and D should treat it as the
  hypothesis rather than the surprise.

---

## Session 4 — 2026-08-14 — Section D (the evidence layer)

### Method notes

**This is the first session that booted a guest.** Sessions 1–3 were host-only and each named
that as their largest gap; D.3 and D.4 cannot be answered any other way. Five gates were run for
real, and four of them were then deliberately broken to see whether they notice.

The tree has moved since session 3: the A/B/C fixes landed as PRs #203–#205, so the host suite
is **628 tests**, not the 622/624 those sessions baselined against. Everything below is against
`475ea6d`.

Baselines, taken first:

```
cargo test --lib --target x86_64-unknown-linux-gnu   → 628 passed; 0 failed
cargo xtask test-qemu        → integration tests PASSED (qemu exit 33)      15.7 s
cargo xtask test-interactive → interactive tests PASSED (22 steps)
cargo xtask check-terminal   → terminal gate PASSED                         33.5 s
cargo xtask check-input      → input gate PASSED
cargo xtask check-display    → display gate PASSED
cargo xtask check-irq-scope / check-deferrals / check-docs → all ✓
```

Guest runs are **TCG** unless marked `--kvm`; CI runs `--kvm`, which is not the same timing and
is called out where it matters. Every mutation below was reverted; the closing state is
`git status` clean but for this file, 628/628 host tests, and `test-qemu` PASSED again.

**Three of my own instruments were broken and reported clean while measuring almost nothing.**
All three are written up in place (D.1 §a) rather than quietly fixed: a sweep that reports
coverage it does not have is the exact failure this section exists to find, and I hit it three
times in one session.

---

### D.1 — negative-control every host test that claims to pin an invariant

> **Claim.** "Delete the guard, confirm the named test fails. Three of the last three reviews
> found an assertion that passed for both the correct and the broken implementation."

Sessions 1–3 did this by hand for eight named guards. Doing it for *every* guard needs a
campaign, so I built one: for each `if <cond> {` outside the test modules in the audit's scope
files, replace the condition with `false` — the checklist's "delete the guard" — and run the
628-test suite.

A surviving mutant is not yet a finding: the line may never execute. So every survivor gets a
second run with the same condition replaced by `(|| -> bool { panic!("AUDIT-REACH") })()`,
which panics **iff** control reaches it. That splits survivors into two very different
populations — **REACHED** (a test runs the line and cannot tell the two implementations apart:
an assertion-strength hole) and **UNREACHED** (no test runs it at all: a coverage hole).

162 mutants, run across five `git worktree` copies with separate target dirs.

#### a. Three instrument failures, recorded because the shape *is* the finding

1. **The sweep cut each file at the first `#[cfg(test)]`.** `sched.rs` carries inline
   `#[cfg(test)]` *expression* blocks at lines 709 and 723 (host-vs-guest branches), long before
   its test module at 3930. The first run therefore examined 708 of 4741 lines — and printed a
   confident per-file summary for all 15 files. Fixed by brace-counting only
   `#[cfg(test)] mod … { }` regions.
2. **The candidate filter matched nothing containing an underscore.** I filtered conditions to
   those mentioning guard vocabulary — `\b(len|cap|deadline|…)\b`. `\bdeadline\b` does not match
   `deadline_ns`, because `_` is a word character. That silently dropped 85 of `sched.rs`'s 102
   guard lines. Removed the filter entirely; a `\b` vocabulary filter over Rust identifiers is
   not salvageable.
3. **No timeout, and a mutant hung.** The campaign sat on one mutant for 37 minutes of CPU
   before I noticed. Runs are now killed at 90 s and scored `HUNG`.

Failure 3 is also a result. The hang is `sched.rs:290`, deleting `if smallest == i { break }`
from the deadline heap's sift-down: **the heap's termination condition is pinned only by
hanging.** No assertion fails, the suite never finishes. In CI that is a job timeout — detected,
but the signal is "the runner died", not "sift_down does not terminate". `dpc.rs` has a second
mutant of the same kind.

#### b. Results — a third of the guards are pinned; half are never executed at all

```
162 mutants:  53 killed   107 survived   2 hung
              survivors: 82 UNREACHED (no test executes the line)
                         25 REACHED   (a test executes it and cannot tell)
```

| File | n | killed | survived | of which REACHED |
|---|---|---|---|---|
| `sched.rs` | 79 | 20 | 58 (+1 hang) | 5 |
| `handle/table.rs` | 37 | 17 | 20 | **20** |
| `libkern/lockrank.rs` | 13 | 3 | 10 | 0 |
| `arch/x86_64/idt.rs` | 13 | 3 | 10 | 0 |
| `arch/x86_64/ioapic.rs` | 8 | 4 | 4 | 0 |
| `arch/x86_64/smp.rs` | 3 | 0 | 3 | 0 |
| `dpc.rs` | 3 | 2 | 0 (+1 hang) | 0 |
| `tlb.rs` | 2 | 1 | 1 | 0 |
| `object/thread.rs` | 2 | 2 | 0 | 0 |
| `handle/grace.rs` | 1 | 1 | 0 | 0 |
| `arch/x86_64/apic.rs` | 1 | 0 | 1 | 0 |

Two shapes, and they need opposite fixes:

- **`sched.rs` is a coverage hole.** 53 of its 58 survivors are never executed by any test. That
  is the same wall sessions 2 and 3 hit from other directions (B.5d `switch_into`, C.1b
  `reap_pending`, C.3d `place_thread`), now measured: `place_thread`'s capacity refusal
  (`sched.rs:3522`) is still **UNREACHED** *after* the PR #205 fix landed on it.
- **`handle/table.rs` is an assertion-strength hole.** All 20 of its survivors are REACHED —
  tests run every one of those guards and none of them fails when the guard is deleted. This is
  rule 2's failure mode, in the subsystem that enforces capability security.

#### c. CONFIRMED FINDING — `HandleTable::close` will act on a stale handle, and 628 tests agree it is fine

The sharpest instance, verified with both controls rather than left as a mutation score.

`close` (`handle/table.rs:565`) validates under the lock: generation, then owner. Delete the
generation check (`:586`) and the whole suite is content:

```
test result: ok. 628 passed; 0 failed
```

There is a test that *looks* like it covers this — `t.close(h, 1)` on a closed handle expecting
`InvalidHandle` (`table.rs:1283`). It passes with the guard deleted, because after a close the
slot is free and an earlier check rejects it. The case where the generation check is the **only**
guard standing is a slot that has been *reused*: the handle is stale, the slot is live, and the
object in it belongs to someone else. Nothing exercises that.

Temporary probe (since reverted) doing exactly that — allocate, close, allocate again into the
same slot, then close with the stale handle:

```
(a) guard intact:  test audit_close_rejects_a_stale_handle_whose_slot_was_reused ... ok
(b) guard deleted: audit_close_rejects_a_stale_handle_whose_slot_was_reused --- FAILED
                   panicked at src/handle/table.rs:1318: a stale handle closed the object
                   now living in its slot
                   test result: FAILED. 628 passed; 1 failed
```

628 passed **and** 1 failed, in the same run: the existing suite is unmoved, and only the probe
notices. `restrict`'s generation check (`:671`) and its null-object check (`:677`) are the same
shape and the same verdict, which matters because session 1 (A.5) already singled out `restrict`
as the syscall that mutates a table entry outside the read guard.

#### d. CONFIRMED FINDING — the preempt replay path is executed by tests and pinned by none

`sched.rs:688`, `if replay { … }` — the B.5(b) invariant, "`RESCHED_PENDING` is replayed rather
than dropped". The mutant survives, and the reachability probe says **REACHED**: tests do run
`preempt_enable`, but never at depth 1 with a pending reschedule, so the replay body is dead to
them. Confirmed independently: `RESCHED_PENDING`, `preempt_enable` and `preempt_disable` appear
**nowhere** in `sched.rs`'s test module.

#### e. CONFIRMED — the audit's own two most recent fixes *are* pinned, and `lockrank`'s copy problem is fixed

Worth stating plainly, because the rest of this section is negative. My operator does not match
`if let Err(…)` or `saturating_*`, so the PR #203–#205 guards needed hand controls:

```
M-D1a  timer.rs  saturating_sub → wrapping_sub   (undo the C.5 fix)
       → FAILED: arch::x86_64::timer::tests::a_tsc_behind_the_calibration_base_reads_zero…
M-D1b  kvec.rs   push_within_capacity grows again (undo the C.1c fix)
       → FAILED: libkern::kvec::tests::push_within_capacity_refuses_instead_of_growing,
                 …::push_within_capacity_on_an_unreserved_vector_allocates_nothing,
                 entropy::tests::register_reports_full_when_the_reserve_never_happened
```

Both fixes fail loudly and by name. Likewise session 2's cross-cutting finding — that
`lockrank`'s tests exercised a *copy* of the tracker's arithmetic — has been closed by commit
`73ce1da`, which introduced the shared `classify(rank, held)`: mutants on all three of its
branches (`:173`, `:176`, `:179`, including the `TlbShootdown` contract branch session 2 called
untested) are now **killed**. The remaining 10 `lockrank` survivors are the per-CPU tracker state
machine, which is UNREACHED for the reason its own test module gives.

#### f. What the arch numbers mean

7 of 25 arch mutants are killed, and they are all in pure decode/encode logic — the page-fault
error-code split in `idt.rs` (`!present`, `write`, `insn`) and the IOAPIC RTE encoding in
`ioapic.rs` (polarity, trigger mode, mask bit, override matching). Everything else in
`arch/x86_64` is UNREACHED, which is expected rather than damning: it needs a CPU. It does mean
a mutation score over these files is not a coverage claim, and `smp.rs`'s three survivors
include `apic_of_dense`'s `cpu >= MAX_CPUS` bound — the guard session 1 (A.3) relied on as the
second of two independent checks.

---

### D.2 — no test writes a process-global that another test reads

> **Claim.** "`ONLINE_MASK` did, and produced a 33 % failure rate at `--test-threads=16` that CI
> never saw. Sweep for others; the fix shape is to pass state as a parameter, not to add a lock."

Method: enumerate every `static` in `kernel/src` from the **committed** blobs (`git show HEAD:…`,
so the concurrent mutation campaign could not confuse the reading), then look for writes to any
of them from inside a `#[cfg(test)] mod` region.

**133 process-global statics; three candidates; one real.**

#### a. Dismissed on inspection — two of the three

- **`S`** (`spinlock.rs:378`, `:468`) — my grep matched a name, not a global. Both are
  `static S: …` declared *inside* a test function body, one per test. No sharing.
- **`init_global_heap`** (`mm/test_support.rs:32`) — genuinely shared, and deliberately so: a
  `std::sync::Once` behind a documented "idempotent and thread-safe: every test may call it
  unconditionally… internally locked, exactly as on the real kernel". Sound. The one thing it
  does share is a *budget* — a single leaked 16 MiB heap for all 628 tests, so an allocation
  failure would be parallelism-dependent. Not observed; recorded because nothing bounds it.

#### b. CONFIRMED — `ONLINE_MASK` itself is fixed, and the fix is the shape the checklist asks for

`sched.rs:4137` now reads:

```rust
/// The online set a placement test passes in: `n` CPUs up, none parked.
///
/// A value rather than the `ONLINE_MASK` global, deliberately — see [`cpu_accepts_work`].
/// Writing the global here made these tests race each other.
fn all_up(n: usize) -> u64 { (1u64 << n) - 1 }
```

Eight test call sites take the mask as a parameter. One test still writes the real global
(`sched.rs:3946`, the park/identity test) and says why — the three cases live in one `#[test]`
"on purpose", since separate ones would run concurrently. I checked the residual: no other test
in the tree reads `ONLINE_MASK`, directly or through `online_mask()`, so the consolidation holds.
It holds **by convention**, though: a future test calling `online_mask()`, `clear_online_bit()`
or `tlb::shootdown` re-creates the race, and nothing would say so.

#### c. CONFIRMED FINDING — `MOCK_IF` is the same hazard, and its comment asserts the opposite

`irq_backend::MOCK_IF` (`libkern/spinlock.rs:183`) is a process-global `AtomicBool` standing in
for the interrupt flag. Five `IrqSpinLock` tests write it through `reset_mock_if()` and then
assert on it. The module says why that is safe:

- `spinlock.rs:384` — "`cargo test` runs single-threaded here"
- `spinlock.rs:394` — "The tests run serially and reset MOCK_IF first, since they share the one
  global mock flag"
- `spinlock.rs:462` — "Tidy up the shared mock flag for any later test."

**Neither claim is true.** There is no `--test-threads` setting anywhere in the repository — not
in `.cargo/config.toml` (any of them), not in `Cargo.toml`, not in the workflows, not in
`cmd_test`. `cargo test` uses one thread per CPU; this host runs 16.

The hazard does not fire on its own — 20 consecutive full-suite runs at `--test-threads=16` were
clean, which is why nobody has noticed. So I forced the interleaving instead of waiting for it:
a 300 ms sleep after one test's `reset_mock_if(true)` and a 100 ms sleep before another's
`reset_mock_if(false)`, so the second write lands inside the first test's window.

```
libkern::spinlock::tests::irq_lock_masks_while_held_and_restores_on_drop --- FAILED
panicked at src/libkern/spinlock.rs:418:9:
IF must be restored to its prior state on drop
test result: FAILED. 627 passed; 1 failed
```

A false failure, in a test about interrupt-flag save/restore, caused entirely by another test's
write. **These tests are isolated by being fast, not by any mechanism** — and the first version
of that experiment (a sleep in only one of the two) passed, which is worth recording: the window
is real but narrow, and a single-sided probe misses it.

Note the direction of the risk. Today it produces a *spurious* failure, which is merely
expensive; the same sharing could as easily mask a real one, since a test asserting `!mock_if()`
is satisfied by any other test having just masked.

---

### D.3 — every gate assertion can fail

> **Claim.** "Check each `session.expect` is reachable and not satisfied by earlier output —
> `expect` advances a cursor, so verify that is true of the gates that rely on it."

#### a. The cursor is sound. It is also not the hazard

`Session::expect` (`tools/xtask/src/main.rs:2054`) searches `g[self.cursor..]` and advances
`self.cursor += i + pat.len()`, so a step cannot match an occurrence it has already consumed. The
one way that could still break — a byte cursor into a concurrently appended `String` — holds:
the reader thread appends `String::from_utf8_lossy` output, so the buffer is always valid UTF-8
and every advance lands on a char boundary.

The hazard is one layer up. **The cursor stops a pattern matching *old* output; nothing stops it
matching the *wrong new* output.** Two sources of wrong-new-output exist in these gates and both
are live: the guest echoes what the harness types, and the guest's own services keep logging into
the same serial stream.

#### b. The instrument

Rather than read 70 `expect` calls and reason about them, I made the harness answer. A temporary
`audit_record` on every successful match writes the matched offset, the matched line, and three
classifications: `pre_existing` (the match was already complete in the buffer when the step
*began* waiting), `on_echo_line` (the matched line also contains the text just typed), and
`cursor_dep` (the pattern also occurs before the cursor, so the cursor is the only thing keeping
this step off older output).

`on_echo_line` is a heuristic and produced two false positives — `\npassword:` and
`shell exit 3`, both of which matched genuine server output that merely *contains* the typed
text. Every flagged record below was read back in the transcript before being called a finding,
per rule 3.

#### c. CONFIRMED FINDING — `test-interactive` checks the shell's error handling against the shell's own echo

```
pat=boom  abs=6581  pre_existing=false  on_echo_line=true  cursor_dep=false
          last_sent=try { fail "boom" } catch (e) { e.message }
          line=/home> try { fail "boom" } catch (e) { e.message }
```

The step's subject is `try`/`catch` in expression position. The word `boom` reaches the harness
twice — once because the terminal echoes what was typed, once because the shell evaluates the
expression — and `expect` takes the first. The echo always wins: it is emitted as the line is
sent, long before the parser sees it.

Negative control **NC-D3a** — replace the command with one that computes nothing and merely
contains the letters:

```rust
-    s.send("try { fail \"boom\" } catch (e) { e.message }")?;
+    s.send("let boomless = 1")?;
     s.expect("boom")?;
```

```
pat=boom  abs=6977  on_echo_line=true  last_sent=let boomless = 1
          line=/home> let boomless = 1
…
xtask: interactive tests PASSED (22 steps)
```

Same 22 steps, same PASSED. The `expect("/home>")` that follows is satisfied by the prompt every
command returns to, error or not, so **nothing in the pair depends on `try`/`catch` existing.**

#### d. CONFIRMED FINDING — `expect("5")` is satisfied by any `5` the machine happens to log

The user-defined-function step is `s.send("add(2, 3)")` then `s.expect("5")`. A one-character
pattern, in a stream that also carries heartbeat, logging-service and service-mgr output. Run 1
of 2, unprompted:

```
pat=5  abs=4957  pre_existing=true  on_echo_line=false
       line=[4 t=514690841] system/heartbeat.typed INFO: seq=2 uptime_ns=1501346 healthy=true
```

`pre_existing=true`: the match was already in the buffer when the step began waiting — the shell
had not answered and was never asked to. Run 2 matched the real `5`, so the step is
**nondeterministically vacuous**, which is worse than reliably vacuous because reading it once
cannot find it. Control **NC-D3b**, replacing the command with `mut unused_q = 1` (no output, no
`5` in the echo), passes on another heartbeat line: `uptime_ns=304225453`.

Both of these are in **`test-interactive`, which CI runs on every push**.

#### e. CONFIRMED — `check-terminal`'s central assertion is load-bearing

Its post-Enter step re-expects a line identical to one it already matched, which is the shape
that *looks* vacuous. Removing the Enter injection:

```
run 1: xtask: timed out after 45s waiting for "nxterm: grid> /> whoami"   EXIT=1
run 2: xtask: timed out after 45s waiting for "nxterm: grid> /> whoami"   EXIT=1
```

Twice, idle. The terminal does not spontaneously reprint the line, so the second match really is
a consequence of Enter. All twelve of its `expect`s matched their intended lines, and the
per-character echo loop is as strong as its comment claims.

#### f. CONFIRMED — `check-input`'s twenty assertions each matched their intended line

Every pattern is a full structured event (`input-testclient: ev kind=2 code=1 value=3`). There is
nothing else in the stream they could match. This is the gate the other two should look like.

#### g. CONFIRMED — `check-display` catches a channel swap, as `CLAUDE.md` claims

```rust
-  if got != (want.r, want.g, want.b) {
+  if got != (want.b, want.g, want.r) {
```
```
xtask: display gate FAILED: 2041 of 2048 scene pixels differ.
```

2041 of 2048; the seven that agree are greys.

#### h. CONFIRMED — `test-qemu` fails on a kernel that dies at boot

The CI gate with no `expect` at all, adjudicated from `isa-debug-exit`. A `panic!` at the top of
`kernel_main`:

```
xtask: integration tests FAILED (qemu exit 35; expected 33)
```

with the panic's stack dump. Failure propagates to the process exit code — the NC-D3e runs above
exited 1.

---

### D.4 — promote `check-terminal` to CI; build `check-input --no-ps2-irq`

> **Claim.** "Promote `check-terminal` to CI once it has a clean run of ~10, and build
> `check-input --no-ps2-irq`. Both are filed in `deferred-decisions.md`."

The one checklist item that asks for work rather than a verdict. I measured the precondition and
audited both; I did not build anything.

#### a. The stated precondition is met — 24 consecutive clean runs

```
10 runs, host idle                     10 passed, 0 failed   33.4–33.6 s each   (TCG)
 6 runs, 8-way CPU load                 6 passed, 0 failed                      (TCG)
 3 runs, concurrent cargo builds        3 passed, 0 failed                      (TCG)
 5 runs, --kvm                          5 passed, 0 failed   26.4–30.9 s each
```

The load runs are deliberate: a CI runner is not an idle laptop, and the one prior flake in this
family reproduced only under TCG plus host load. The KVM runs matter because **CI passes
`--kvm`** and the default without the flag is `Accel::Tcg` — a gate measured under one
accelerator has not been measured under the other, and PS/2 injection timing is exactly the sort
of thing that differs.

#### b. One anomalous failure, unreproduced, recorded rather than dismissed

During the NC-D3e work one run failed at `timed out after 45s waiting for "nxterm: clicked"` — a
step *upstream* of the mutation that run carried, so the mutation cannot explain it. It happened
while a `cargo` build ran concurrently. Nine subsequent loaded runs did not reproduce it. That is
1 failure in 25 invocations, at a step whose own comments record two previous timing bugs (a
click landing during window churn; the pointer's relative-motion arithmetic). It is not evidence
the gate is flaky; it is evidence that nobody has ruled it out, and the promotion decision should
own that rather than inherit "10 of 10".

#### c. CONFIRMED — it is genuinely absent from CI, and its coverage is genuinely unique

`.github/workflows/{ci,display,input}.yml` run `build`, `test`, `test-qemu`, `test-interactive`,
`check-input`, `check-display`, `check-irq-scope`, `check-docs`, `check-deferrals`, `check-arch`,
`check-nightly` and `abi-sync-check`. `check-terminal` appears in none. The deferral's claim that
its coverage "is not duplicated by `check-input` or `check-display`" is right on this session's
evidence: `check-input` stops at the client's event log, and `check-display` never types.

#### d. CONFIRMED — `check-input --no-ps2-irq` does not exist, and its entry is accurate

`grep -rn "no-ps2-irq\|no_ps2_irq"` over `*.rs`, `*.toml` and `*.yml` outside `docs/` returns
nothing; `cmd_check_input(accel)` takes no flag. The entry (`deferred-decisions.md:1195`) is
still true, and it is **the strongest single argument in this section**: it is the one place
where the project has already worked out that a gate cannot fail for the right reason — the
recovery sweep runs only when the hardware misbehaves, so no pass count catches its deletion —
and written down the fix. Everything D.1 and D.3 found this session is a further instance of that
same problem, discovered later and more expensively.

---

### D.5 — audit `deferred-decisions.md` against reality

> **Claim.** "At least one entry ('Debug-build lock-ordering enforcement') describes as missing a
> mechanism that landed 2026-07-29. Find the others; a deferral list nobody trusts is worse than
> none."

#### a. CONFIRMED — the named entry is stale, and the document contradicts itself about it

`deferred-decisions.md:1152`:

> **Debug-build lock-ordering enforcement.** … **The mechanism doesn't yet exist**; the only
> lock-ordering enforcement today is code review and `kernel/docs/lock-ordering.md`.

`kernel/src/libkern/lockrank.rs` is 778 lines of exactly that mechanism, live in every image
xtask builds (session 2, B.1e), gated by `check-irq-scope` in CI, and — per D.1(e) above — now
covered by tests that fail when its arithmetic is broken. The same document's Resolved table 100
lines below records "Per-interrupt-context lock-order tracking | 2026-07-29", an entry that only
makes sense as a refinement *of the tracker the open entry says does not exist*.

#### b. CONFIRMED FINDING — `check-deferrals` can only ever catch one of the two ways this rots

The gate walks `kernel/src`, `userspace` and `tools/xtask/src`, extracts every `TODO(tag)`, and
fails if the tag is absent from `deferred-decisions.md`. It never asks the reverse question. The
document claims more: "`cargo xtask check-deferrals` enforces the `TODO(tag)` half of that
mechanically" (`:1270`).

```
18 distinct TODO(tag) markers in code  → all 18 present in the document      ✓ (gate passes)
28 distinct TODO(tag) names in the doc →  9 open entries have no code marker  (unchecked)
```

The nine: `atomic-log-lines`, `history-pager`, `regex-named-captures`, `regex-replace`,
`shell-bitwise`, `shell-labelled-break`, `stack-attribution`, `tty-server`, `unicode-case`.
Six of them exist **nowhere in the repository outside this document**; the other three survive
only in `docs/planning/` and the decision log, neither of which the gate scans. So for a third of
the tagged open deferrals the advertised mechanical enforcement is an empty set — and it fails
silently in the direction that matters, since a deferral whose marker is gone is one nobody will
trip over while editing the code.

#### c. CONFIRMED FINDING — the `TODO(smp)` entry rests on a premise falsified six weeks before it was last revised

`deferred-decisions.md:1067`:

> …`current_ctx_id()` returns **0 in production builds** — every CPU shares one context. Today
> nothing depends on the distinction… The `TODO(smp)` marks the case that would break it — … **a
> real per-CPU context id** where one CPU's quiescence no longer implies another's.

```rust
// kernel/src/handle/mod.rs:145
pub(crate) fn current_ctx_id() -> u32 {
    use crate::arch::smp::ArchSmp;
    crate::arch::Smp::current_cpu()
}
```
```
git log -S : ef47861  2026-06-29  Phase 3 slice 0 — per-CPU foundation
             -    0
             +    crate::arch::Smp::current_cpu()
```

The case the entry names as the future hazard **is the current implementation**, and has been
since 2026-06-29 — while the entry itself was revised at least as late as 2026-07-24 (it cites
that date's exit-time reclamation sweep as "a new writer on this path"). Not cosmetic: session 1
(A.5) and session 3 (C.4a) both turn on per-CPU context ids, and C.4(a) is a machine-wide wedge.
The deferral sends a reader looking for a hazard that has already arrived.

#### d. CONFIRMED — the `tty-server` entry describes as *designed* what has been shipping since 2026-08-13

The entry (`:698`) reads "Designed 2026-08-03 in console-and-tty.md: a userspace resource server
owning the line discipline and the raw device, handing each session an IPC channel bound at
`/dev/tty`". Reality:

- `userspace/tty-server` is a workspace member (`userspace/Cargo.toml:3`);
- `init` spawns `/bin/tty-server` and binds its endpoint at `/dev/tty`
  (`userspace/init/src/main.rs:888`, `:936`);
- `docs/architecture/console-and-tty.md` opens "**Status: stages 1–4 built (2026-08-13).** The
  server exists, `/dev/tty` is a capability, and its clients have moved";
- `check-terminal` drives a real `nxsh` through it — 24 times this session.

Half the entry is still true: `nxsh` prints via `kprint` (`userspace/nxsh/src/main.rs:406`,
`:452`), so the *output* direction is still the ambient syscall the entry complains about. That
is precisely why it needed revising rather than leaving — as written, a reader cannot tell which
half is owed.

#### e. CONFIRMED — one entry is filed in both the open section and the Resolved table

"**`xtask test-qemu` integration harness**" appears at `:1156` in the open "Testing and CI"
section, struck through with "**Implemented 2026-07-14**" appended in place, *and* as a row of
the Resolved table at `:1257`. The document's own instruction (`:1263`) is that a resolved entry
"should **move to the Resolved table**, not be left in place with a status note appended — an
open section that mixes finished work with owed work cannot be scanned, which is how three
deferrals went unnoticed until a consumer tripped over them".

#### f. The list is also *more* accurate than the current-behaviour docs, in at least one place

This inverts the checklist's assumption, so it is worth recording. The entry at `:687` notes in
passing that "`sys_release_initramfs` is referenced in the docs but does not exist". Correct —
there is no such syscall in `kernel/src`, and the `AlreadyReleased` error it promises exists
nowhere either. But `docs/spec/syscall-abi.md:497` still specifies it:

```rust
fn sys_release_initramfs() -> isize
```

> Unbinds `/initramfs` from the root namespace and frees the initramfs physical pages. One-shot —
> succeeds once, returns `AlreadyReleased` thereafter. Requires `BIND_NAMESPACE`.

— under `### Kernel Objects`, with **no** deferred marker, three lines below `sys_device_map_mmio`
which carries one ("**Deferred** — … the syscall is unimplemented until…"). So the spec knows how
to say "not built" and does not say it here. `docs/architecture/overview.md:249` likewise
describes init calling it as boot step 8. Both are current-behaviour classes, where "the source
wins and the doc is a bug".

`check-docs` reports "39 syscalls + 16 error codes — all agree ✓" and is not wrong: its syscall
cross-check reads *numbered* table rows and numbered prose bullets (`main.rs:3363–3395`), and
this signature is an unnumbered code block, invisible to it. A gate that cannot see the claim
cannot contradict it.

#### g. Not a finding, but noted — the checklist's own Scope names a file that does not exist

`kernel-audit-2026-08.md`'s Scope says `object/{thread,handle}.rs`. `kernel/src/object/thread.rs`
exists; `kernel/src/object/handle.rs` does not and never has — the handle code is
`kernel/src/handle/{mod,table,global,grace}.rs` plus `kernel/src/libkern/handle.rs`. I hit it when
the campaign's file list failed to open it. Sections A–C read the right files regardless.

---

## Summary — Section D

| Item | Verdict |
|---|---|
| D.1 campaign | **162 mutants: 53 killed, 107 survived, 2 hung.** 82 survivors are never executed by any test; 25 are executed and undetected |
| D.1 `handle/table.rs` | **Confirmed finding** — 20 of 20 survivors REACHED; `close` accepts a stale handle into a reused slot with 628/628 still green (probe + control) |
| D.1 `sched.rs` | **Confirmed gap** — 53 of 58 survivors unreached; `place_thread`'s capacity refusal is still unreached *after* the PR #205 fix |
| D.1 preempt replay | **Confirmed gap** — `sched.rs:688` reached, undetected; no test names `RESCHED_PENDING` or `preempt_*` |
| D.1 heap termination | **Detected only by hanging** — deleting `if smallest == i { break }` never fails, it never finishes |
| D.1 the audit's own fixes | **Confirmed pinned** — M-D1a (TSC wrap) and M-D1b (no-grow-under-`SCHED`) both fail by name; `lockrank`'s copy problem is closed by `classify()` |
| D.2 `ONLINE_MASK` | **Confirmed fixed** — parameterised via `all_up(n)`; the one remaining global write has no concurrent reader. Holds by convention, not by construction |
| D.2 `MOCK_IF` | **Confirmed finding** — five tests share it; the module's "runs single-threaded"/"run serially" claims are false (no `--test-threads` setting exists); forced interleaving produces a false failure at `spinlock.rs:418` |
| D.2 shared test heap | **Sound** — `Once`-guarded and documented; a single 16 MiB budget shared by 628 tests is unbounded but unobserved |
| D.3 cursor semantics | **Confirmed sound** — and not where the risk is |
| D.3 `test-interactive` `boom` | **Confirmed finding** — satisfied by the guest's echo; passes with `try`/`catch` removed from the command entirely |
| D.3 `test-interactive` `5` | **Confirmed finding** — satisfied by a background heartbeat log line; observed in 1 of 2 unmutated runs |
| D.3 `check-terminal` | **Confirmed non-vacuous** — suppressing Enter fails it twice |
| D.3 `check-input` | **Confirmed** — 20 of 20 expects matched their intended line |
| D.3 `check-display` | **Confirmed non-vacuous** — an R/B swap fails 2041 of 2048 pixels |
| D.3 `test-qemu` | **Confirmed non-vacuous** — a boot panic gives `qemu exit 35; expected 33` |
| D.4 `check-terminal` precondition | **Met** — 24 consecutive passes (10 idle, 9 loaded, 5 `--kvm`); one unreproduced click-step failure in 25 |
| D.4 `check-input --no-ps2-irq` | **Confirmed absent**; the entry describing it is accurate and is the best-stated instance of this whole section's problem |
| D.5 lock-ordering entry | **Confirmed stale** — and the document contradicts itself 100 lines apart |
| D.5 `check-deferrals` | **Confirmed finding** — one-directional by construction; 9 open entries bind to no code marker, 6 of them exist only in that file |
| D.5 `TODO(smp)` premise | **Confirmed false** — `current_ctx_id()` has been per-CPU since 2026-06-29; the entry was revised a month later |
| D.5 `tty-server` entry | **Confirmed stale (headline)** — the server ships; the output half of the complaint is still true |
| D.5 `test-qemu` entry | **Confirmed duplicate** — open section and Resolved table, against the document's own rule |
| D.5 `sys_release_initramfs` | **Deferral right, spec wrong** — specified with no deferred marker, invisible to `check-docs`' numbered-row parser |

### What I could not check

- **Mutation operator coverage.** The campaign mutates `if <cond> {` only. It does not touch
  `if let`, `match` arms, multi-line conditions, `debug_assert!`, `?`, or loop conditions — which
  is why `spinlock.rs` and `libkern/handle.rs` produced **zero** candidates and why only 1 of 162
  mutants landed on a line PRs #203–#205 added (those two needed the hand controls in D.1e). The
  162 are a lower bound on the guards in scope, not an enumeration of them.
- **`killed_by` names for the campaign's 53 kills.** `cargo test -q` does not print per-test
  FAILED lines in the format my parser expected, so the per-mutant kill list records the verdict
  without the test name. The verdict itself (suite red/green) is what the classification uses.
- **Whether the 25 REACHED survivors are all real defects.** I proved one to the standard the
  checklist asks for (D.1c, probe plus control). The other 24 are mutation results, not verified
  findings — `restrict`'s two are argued by analogy and should be confirmed the same way.
- **`--kvm` for anything except `check-terminal`.** `test-interactive`, `check-input` and
  `check-display` were run under TCG only, and D.3's two vacuous steps were demonstrated there.
  They are text comparisons, so KVM should not change them — but "should not" is not "checked".
- **How often `expect("5")` really loses the race.** I observed 1 of 2 clean runs and forced the
  third. I did not run `test-interactive` enough times to put a rate on it.
- **The `check-terminal` click-step failure (D.4b).** Observed once, not reproduced in 9 further
  loaded runs. Its cause is unknown, and "unreproduced" is not "absent".
- **`MOCK_IF` in the wild.** 20 full-suite runs at `--test-threads=16` were clean; the failure I
  produced needed injected sleeps. The hazard is proven, its natural rate is not measured.
- **Userspace host tests.** `cargo xtask test` runs the whole workspace; my mutation campaign and
  the global sweep covered `kernel/src` only, per the audit's scope.
- **The other 24 orphan-tag deferrals' true status.** I verified `tty-server` (built) and
  `stack-attribution`, `history-pager`, `atomic-log-lines` (markers survive only in planning docs).
  The five `nxsh` language entries I did not check against the shell's grammar — userspace, out of
  scope.
- **Section D only**, and it closes the checklist: A (session 1), B (session 2), C (session 3),
  D here. The one item D leaves genuinely open is its own §D.4 — the promotion and the
  `--no-ps2-irq` gate are work, not findings, and both are now measured rather than assumed.
