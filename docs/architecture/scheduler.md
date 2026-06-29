# Scheduler

The kernel scheduler: how runnable threads are chosen, how that scales from one
CPU to many, and the per-CPU substrate SMP stands on. This document is the design
contract for the Phase 3 kernel-first slices (0–3); it supersedes the high-level
summary in [overview.md](overview.md#scheduling) and the staging notes in the
decision log (2026-05-29).

Scope: the scheduling *policy* (classes, fairness), the *per-CPU* execution model,
and the *SMP* primitives scheduling depends on (per-CPU data, IPIs, TLB shootdown).
The mechanical AP-startup sequence is elaborated in the slice-1 work; this doc fixes
the model it targets.

## Current state (single-CPU, pre-Phase-3)

The scheduler is **single-CPU preemptive** with one global lock and one flat
runqueue. Concretely:

- `static SCHED: IrqSpinLock<SchedState>` (`sched.rs:299`) guards all scheduling
  state — the rank-1 kernel lock.
- `SchedState.ready: KVec<ObjectRef>` (`sched.rs:258`) is a **single
  round-robin queue**; `switch_to_next` (`sched.rs:1515`) dequeues the front, runs
  it, and re-enqueues the outgoing thread at the tail. **No classes, no priorities.**
- Preemption *is* wired: the timer IRQ calls `on_timer_tick` (`sched.rs:474`), which
  decrements `quantum` and reschedules on expiry (`QUANTUM_TICKS = 1`, every 10 ms
  tick). The tick is the **per-CPU LAPIC timer** in periodic mode, armed at
  `main.rs:355` with `TICK_NS` (`timer.rs:149`); the PIT only *calibrates* it at boot
  (`ioapic.rs:283`) and is then masked. A dormant one-shot path (`arm_oneshot_in`,
  `timer.rs:161`) and a TSC monotonic clock (`read_ns`) already exist.
- `SchedState` already has the seeds of the Idle class: `idle` / `idle_addr` (a
  dedicated idle thread parked off `ready`, run only when nothing else is ready) and
  a `deadlines` **binary min-heap** (`mod deadline`, `sched.rs:107`) — the ordered
  structure we reuse for vruntime.
- Wakeup is **immediate**, not tick-quantized: `complete_pending_op` /
  `fire_expired_deadlines` move a thread to `ready` under `SCHED`, and the same tick
  path reschedules. (The page-cache fault path blocks on a PO this way.)

SMP is a **complete stub**:

- `arch/x86_64/smp.rs`: `cpu_count() → 1`, `current_cpu() → 0`,
  `send_ipi() → unimplemented!()`.
- Limine's SMP request is **not wired** (`limine.rs` has no SMP references); APs are
  never started.
- One shared GDT/TSS/IST (`arch/x86_64/gdt.rs`); one `CPU0` per-CPU block reached via
  `IA32_KERNEL_GS_BASE` (`arch/x86_64/syscall.rs:75`, commented "becomes one-per-CPU
  under SMP").
- The local APIC runs in **xAPIC (MMIO)** mode (`arch/x86_64/apic.rs`); `send_ipi` and
  TLB shootdown do not exist. `flush_tlb_page` / `flush_tlb_all` are **local-only**.
- `sys_thread_set_affinity` is a validating **no-op** (`syscall/table.rs:384`).

The good news: the spinlocks are already **SMP-correct** (atomic CAS,
`spinlock.rs:72`), so the locking substrate carries over unchanged. The single-CPU
assumptions that must be lifted are all in the *using* code — the single `current`,
the single runqueue, the ctx-0 handle grace-period shim, and the lack of per-CPU
page-table-root tracking.

## Target model

### Scheduling classes

Three classes, checked in strict precedence (a runnable thread in a higher class
always preempts a lower one):

| Class | Discipline | Selection | Gate |
|-------|-----------|-----------|------|
| **RealTime** | Fixed priority 0–99; FIFO within a priority level | Highest-priority runnable thread; ties run FIFO; **no preemption by lower priorities or by TimeShared** | Requires the `REAL_TIME` syscap to *enter* the class (set at spawn) |
| **TimeShared** | CFS-like virtual-runtime fair scheduling (the default) | Smallest `vruntime` among runnable TimeShared threads | None (default class) |
| **Idle** | One per-CPU idle thread running `hlt` | Runs only when no RealTime or TimeShared thread is runnable on this CPU | Kernel-internal; not user-selectable |

The per-CPU pick is therefore: *if any RealTime runnable → highest-priority FIFO;
else if any TimeShared runnable → min-vruntime; else the idle thread.* This collapses
to today's behavior when every thread is TimeShared with equal weight.

### TimeShared: virtual-runtime fairness

Each TimeShared thread carries a `vruntime` (virtual runtime, nanoseconds-scaled).
On each tick a running thread accrues `vruntime += slice_ns / weight(nice)`, so
lower-priority (higher-nice) threads accrue faster and thus get picked less often;
fairness is "everyone advances their vruntime at a rate inversely proportional to
weight." The scheduler always runs the **smallest** vruntime.

- **Structure:** a **binary min-heap keyed on vruntime**, reusing the
  `mod deadline` heap pattern (`sched.rs:107`) — *not* a red-black tree. libkern has
  no ordered tree, and at our thread counts a heap's O(log n) insert / extract-min is
  ample; Linux uses an rbtree only because it also needs efficient arbitrary removal,
  which we handle by lazy invalidation (a blocked thread is skipped, not eagerly
  removed). If profiles ever show heap churn dominating, an rbtree is a drop-in
  behind the same "pick min vruntime" interface.
- **New-thread / wake vruntime:** a thread joining (or re-joining after blocking)
  is seeded to `max(its vruntime, min_vruntime - slice)` so it can't hoard CPU by
  having slept with a tiny vruntime, nor be starved by a stale-large one. `min_vruntime`
  is the per-runqueue floor (monotonic).
- **Slice / granularity:** there is no universal "right" slice — Linux CFS targets a
  ~6 ms scheduling *latency* divided across runnable threads, floored at a ~0.75 ms
  minimum granularity (newer EEVDF uses a ~0.75 ms base slice); Windows uses much
  coarser quanta (tens of ms). Our enforceable granularity is bounded by the
  **periodic tick (10 ms / 100 Hz today** — `sched.rs:81`, `ioapic.rs:290`): vruntime
  accrues per tick and the running thread is preempted at a tick boundary once a
  smaller-vruntime thread is runnable, so the *effective* slice is the tick (10 ms).
  A CFS-style `target_latency / nr_running` slice floored at a sub-tick minimum —
  which needs either a faster tick or a per-switch one-shot LAPIC timer
  (tickless-style arming; `arm_oneshot_in` already exists) — is a **deferred**
  refinement (see §Higher-resolution timing under Deferred); 10 ms-granular vruntime
  fairness is correct, just coarser, and is ample for the Phase-3 milestone. Nice
  weighting uses a small fixed weight table (Linux-style), not a formula.

RealTime threads do **not** accrue vruntime (they are not fair-scheduled); Idle is a
sentinel, never enqueued in either structure.

### Per-CPU execution model

Each CPU owns: its **current** thread, its **idle** thread, its **runqueues** (the
RealTime priority array + the TimeShared vruntime heap), and a stable **`cpu_id`**.

**Per-CPU access is arch-abstracted.** "Which CPU am I" and "reach this CPU's data"
are x86-specific mechanisms kept **behind `crate::arch`** (arch-boundary rule). Neutral
code calls **`crate::arch::Smp::current_cpu() -> u32`** (a dense CPU index) and indexes a
neutral `CPUS[current_cpu()]` array for its scheduler state; it never reads an APIC id,
an MSR, or `gs:` directly. The x86 implementation reads the index via **`RDTSCP`**, with
`IA32_TSC_AUX` set to the dense CPU index as each CPU initializes — one instruction, no
mapping table, and independent of the `swapgs`/`KERNEL_GS_BASE` convention (retained for
the syscall stub). The mechanism is **swappable behind the abstraction**: x86 could later
adopt GS-relative access (kernel running with `GS_BASE = &CPUS[i]`) for hot-path speed,
and an aarch64 port implements the same `current_cpu()` via `MPIDR_EL1`/`TPIDR_EL1`, with
no neutral-code change. (LAPIC-id + an id→index map was the considered alternative;
`RDTSCP`/`TSC_AUX` wins — no MMIO read, no sparse-id table.) Two per-CPU structures sit in
their proper layers: the **arch** `CpuLocal` (syscall `rsp_scratch`/`kstack_top`,
`gs:`-reached via `KERNEL_GS_BASE`) becomes one-per-CPU; the **neutral** `CPUS[]`
(scheduler `current`/`idle`/runqueue + page-table-root bookkeeping) is indexed by
`current_cpu()`. `MAX_CPUS` (8) sizes both.

Rollout is **incremental** (decision log, 2026-06-26): slice 1 brings APs up pulling
from the *existing single global runqueue* (all CPUs contend on `SCHED`), proving AP
startup against a scheduler we already trust; slice 3 then splits the runqueue
per-CPU and adds load-balancing. So "per-CPU runqueue" is the slice-3 end state;
slices 0–1 keep the one global queue, only the *current/idle/cpu_id* go per-CPU.

### SMP bring-up

- **AP startup via Limine.** Wire Limine's SMP request; for each AP, Limine jumps it
  to a kernel entry with its own stack. Each AP loads its **per-CPU GDT/TSS/IDT**,
  programs its **GS base** to its `CPUS[i]` block, enables required features
  (SMEP/SMAP, x2APIC — below), starts its **per-CPU APIC timer**, and enters its
  per-CPU idle loop until the scheduler hands it work.
- **`MAX_CPUS`** is fixed at **8** for now (sizes the `CPUS` array and bitmasks);
  raising it is a constant change. QEMU is exercised with `-smp 4`.
- The boot CPU (BSP) keeps running the existing init path; APs are activated after
  the scheduler and per-CPU substrate are up.

### x2APIC (committed; replaces xAPIC)

The local APIC is driven in **x2APIC** mode — committed, not dual-mode (decision log,
2026-06-26). This matches the ≈2014 / x86-64-v2 + SMEP/SMAP baseline, which
*guarantees* x2APIC ([deferred-decisions.md](../rationale/deferred-decisions.md) §x2APIC),
and the project philosophy of assuming modern features rather than carrying fallback
paths.

- **Access:** registers via `RDMSR`/`WRMSR` at `0x800 + (xapic_offset >> 4)` instead
  of MMIO — no mapped APIC page, and MSR access is serializing (cleaner ordering than
  MMIO). The change is localized to the accessors in `apic.rs` (`read_reg`/`write_reg`
  → MSR), the 32-bit `id()`, and the ICR.
- **Enable:** firmware hands off in xAPIC mode; each CPU (BSP + every AP) sets the
  **EXTD** bit (bit 10) alongside the global-enable bit (11) in `IA32_APIC_BASE`
  (MSR `0x1B`) to enter x2APIC. The only residual xAPIC code is this transition.
- **IPI:** a single atomic `WRMSR` to the ICR MSR (`0x830`) carrying destination
  (32-bit APIC id) + vector + delivery flags — no ICR-high/ICR-low two-step, no
  delivery-status poll. This is strictly simpler than xAPIC's MMIO IPI and is why
  x2APIC *reduces* slice-1 complexity.
- **Timer mode is unchanged** — we keep periodic LVT timing (`timer.rs:156`), not
  TSC-deadline; x2APIC is orthogonal to the timer mode.
- **Dev loop:** TCG only emulates x2APIC from **QEMU ≥ 9.0**; the `xtask qemu` `-cpu`
  line gains `+x2apic` and the QEMU floor rises to 9.0 (the alternative, KVM
  `-cpu host`, is not adopted — the loop stays deterministic TCG). This is a slice-1
  prerequisite, not a slice-0 one.

### TLB shootdown

Once two CPUs share an address space, a page unmap on one CPU must invalidate the
others' TLBs or stale translations become a correctness bug. The kernel gains:

- `flush_tlb_range(asid_or_root, va, len)` (local) plus a **shootdown IPI**: the
  initiator flushes locally, then sends a fixed-vector IPI to every *other* CPU in the
  address space's **`active_cpus`** mask and waits for acknowledgement before reusing
  the freed frames.
- `active_cpus` is a per-`AddressSpace` bitmask of CPUs that currently have its root
  loaded in `CR3` — maintained on context switch — so a shootdown only disturbs CPUs
  that actually hold the mapping.
- This lands in **slice 1** (mandatory the moment APs run), even though per-CPU
  runqueues are slice 3.

### Affinity and work stealing (slice 3)

- **Per-CPU runqueues**: `SchedState` is instantiated per-CPU; each CPU schedules
  from its own RealTime array + TimeShared heap.
- **Work stealing**: a CPU that would otherwise go idle steals a runnable thread from
  the busiest peer's runqueue (respecting affinity), keeping cores busy without a
  global lock on the common path.
- **Affinity placement on wake**: a woken thread is enqueued on a CPU permitted by its
  affinity mask, preferring its last CPU (cache warmth) then the least-loaded
  permitted CPU.
- **`sys_thread_set_affinity`** becomes functional (replaces the `table.rs:384`
  no-op), validating the mask against `active_cpus`/`MAX_CPUS` and migrating the
  thread if its current CPU is excluded.

## Data-structure changes

- **`Thread`** (`object/thread.rs:86`) gains scheduling parameters: `class`
  (RealTime/TimeShared/Idle), `rt_priority: u8` (RealTime only), `nice` /
  `vruntime: u64` (TimeShared only), and `affinity` (a `MAX_CPUS`-bit mask). Today the
  struct has only `state` and no scheduling parameters; `quantum` lives on
  `SchedState` as policy. `vruntime` is per-thread persistent, so it goes on `Thread`;
  the per-runqueue `min_vruntime` floor stays on `SchedState`.
- **`SchedState`** (`sched.rs:255`) evolves from one `ready: KVec` to per-class
  structures (RealTime priority buckets + the TimeShared vruntime heap), and becomes
  **per-CPU** in slice 3 (`CPUS[i].sched`). The `idle`/`idle_addr` slots already model
  the Idle class.
- **`ThreadArgs`** (`libkern/thread.rs:26`) consumes part of its `_reserved: [u8; 40]`
  block (offset 24) for `class`, `rt_priority`, `nice`, and `cpu_mask`. This is an
  **ABI change** (the spec `docs/spec/thread-args.md` is updated and the layout
  asserts bump) but a forward-compatible one — the bytes are validated-zero today, so
  old callers keep working as "TimeShared, default nice, all-CPU affinity". The
  `REAL_TIME` syscap is checked when `class == RealTime`.

## Slice plan (kernel-first)

| Slice | Delivers | Verify |
|-------|----------|--------|
| **0** | This doc + the **per-CPU substrate**: `CPU0`→`CPUS[N]`, GS-based per-CPU block, neutral `current_cpu()` (RDTSCP), per-CPU scheduler `current`/`idle`, and lifting the ctx-0 handle grace-period shim. **Still single-CPU; no APs, no x2APIC, no IPIs.** | Boots exactly as today on the current QEMU; host tests for the per-CPU structures. |
| **1** ✅ | **SMP bring-up** *(landed; correctness items → slice 3)*: Limine AP startup; per-CPU GDT/TSS + shared IDT; **x2APIC** (MSR accessors + single-`WRMSR` IPI); per-CPU timer; APs run a per-CPU idle thread and pull from the **shared** global runqueue. Fixed a per-CPU `reap` UAF. **Deferred to slice 3:** TLB shootdown + `active_cpus`, and **user-thread-migration safety**. | `-smp 4`: 4 CPUs online, APs executing, full userspace boot clean 6/6. |
| **2** | **Scheduler classes**: RealTime/TimeShared/Idle dispatch; the `Thread`/`ThreadArgs` fields; vruntime fairness; the `REAL_TIME` gate. | A RealTime thread preempts TimeShared; vruntime fairness across TimeShared threads. |
| **3** | **Per-CPU runqueues + work stealing + affinity** + **SMP-correctness hardening** (TLB shootdown + `active_cpus`; user-thread-migration safety — per-CPU runqueues remove the cross-CPU churn that triggers the `syscall_entry` kstack UAF; fix `has_live_siblings`/`exit_process`; audit single-CPU assumptions); functional `sys_thread_set_affinity`. | Load balances; pinned thread stays put; aggressive cross-CPU thread-churn stress test runs clean. |

### Slice 0 in detail

Slice 0 is **single-CPU throughout** — it makes the codebase per-CPU-*shaped* without
starting any AP, so it is fully verifiable on the existing QEMU 8.2.2 / xAPIC loop
(no QEMU upgrade needed yet). It:

1. Generalizes the `CPU0` per-CPU block to a `CPUS: [CpuLocal; MAX_CPUS]` array, with
   each CPU's `GS` base pointing at its own entry. Only index 0 is live this slice.
2. Implements `arch::Smp::current_cpu()` for real — x86 via `RDTSCP`, with `IA32_TSC_AUX`
   set to the CPU's dense index at init (`0` on the BSP this slice) — replacing the
   hardcoded `→ 0`, so neutral code can index `CPUS[current_cpu()]`. The mechanism stays
   arch-internal (see §Per-CPU access).
3. Moves `current`, `idle`, and `idle_addr` from scalar `SchedState` fields to per-CPU
   arrays indexed by `current_cpu()` (the scheduler reads "this CPU's current" via small
   `cur_slot`/`idle_slot` accessors), keeping the single global `ready` and the one
   `SCHED` lock. Behavior-preserving on one CPU; each AP gets its own `current`/`idle`
   for slice 1.
4. Lifts the **ctx-0 handle grace-period shim** (`handle/mod.rs`, the
   `current_ctx_id()` that returned 0 single-CPU) to key on `current_cpu()`.

Nothing here changes observable behavior; the milestone is "identical boot, per-CPU
plumbing in place, host tests green."

**Deferred to slice 1** (refined 2026-06-26): the per-CPU **page-table-root /
`active_cpus`** tracking originally listed here. It has no slice-0 consumer — only the
TLB shootdown reads it — and adds context-switch bookkeeping best landed *with* the
shootdown that exercises it, so it moves to slice 1.

## Decisions (locked 2026-06-26)

- **All three classes** in the first cut (RealTime FIFO is simple; building the
  dispatch framework once avoids re-touching the core). RealTime is `REAL_TIME`-gated.
- **TimeShared = vruntime fairness** via the min-heap pattern (matches the v5.1 design;
  no rbtree needed at our scale).
- **x2APIC-only**, adopted in slice 1; dev loop moves to QEMU ≥ 9.0 + `+x2apic` (TCG
  retained, not KVM).
- **Incremental SMP**: APs on the shared runqueue first (slice 1); per-CPU runqueues
  and load-balancing in slice 3.

## Deferred

- **NUMA-aware placement** — designed not to preclude (affinity + per-CPU runqueues are
  the substrate), not implemented initially ([overview.md](overview.md), deferred list).
- **x2APIC dual-mode / xAPIC fallback** — not built; the baseline guarantees x2APIC. A
  fallback is only revisited for real hardware lacking it.
- **Gang / co-scheduling, deadline (EDF) RealTime, priority inheritance** — the
  RealTime class is fixed-priority FIFO only; richer real-time disciplines are future
  work gated by a consumer.
- **Higher-resolution / tickless scheduler timing** — the tick is a 100 Hz periodic
  LAPIC timer, so the effective slice is 10 ms. Finer granularity (CFS
  `target_latency / nr_running` and EEVDF-style per-task slices, lower preempt latency,
  power-saving idle) comes from **one-shot / tickless arming** — the primitives are
  already in place (`arm_oneshot_in`, dormant; a high-res TSC clock) — optionally
  upgraded to **TSC-deadline mode** (MSR-programmed, composes with the x2APIC work) for
  precision and lower arming jitter. Deferred as one future item (the "sub-tick slice"
  refinement and a tickless idle are the same change); trigger: an interactive /
  latency-sensitive consumer (shell, audio, media). Merely raising the periodic
  frequency (e.g. 1000 Hz) is the cheap stopgap, trading steady IRQ overhead for
  granularity.
- **Post-spawn class / priority change syscall** — class, nice, and affinity are set at
  spawn via `ThreadArgs`; affinity can already change post-spawn via
  `sys_thread_set_affinity` (a no-op today, functional in slice 3). A `renice`-style
  `sys_thread_set_priority` / `sys_thread_set_class` for changing class or nice *after*
  spawn (e.g. a shell `renice`, a supervisor retuning a service, a thread self-elevating
  to RealTime) is a real but non-urgent case — added when a consumer needs it.

See also: [overview.md](overview.md#scheduling),
[memory-management.md](memory-management.md) (TLB / address spaces),
[deferred-decisions.md](../rationale/deferred-decisions.md) (x2APIC, TLB shootdown),
decision log 2026-05-29 (scheduler staging) and 2026-06-26 (Phase 3 sequencing).
