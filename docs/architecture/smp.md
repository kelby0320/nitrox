# SMP: multi-core execution, migration, and work-stealing

How Nitrox runs threads on more than one CPU: the per-core substrate, the exact
boot-to-userspace sequence, how a thread moves between cores (context switch,
placement, work-stealing, migration), the invariants that keep it correct, and a
**postmortem of every bug** the SMP bring-up hit — what it was, how it was found,
and the fix.

This is the runtime-mechanics companion to [scheduler.md](scheduler.md), which is
the scheduling *policy* contract (classes, vruntime fairness, the slice plan).
Where scheduler.md says *what the scheduler decides*, this doc says *how a thread
physically gets onto, off of, and between cores without corrupting state*. Read
scheduler.md first for the policy; read this for the SMP machinery and the war
stories. Anchors are `file:function` (line numbers drift; symbol names are
stable).

Status: as of 2026-07-01 the system boots to a userspace `eshell` on `-smp 4`
with **user threads distributing across all cores and migrating via
work-stealing**, verified 0 failures over 150 KVM boot-loops plus a scripted
`eshell` interaction stress. The two remaining slice-3b items (a cross-CPU
deschedule IPI and per-`AddressSpace` `active_cpus`) are deferred until a
multi-threaded user process exists to exercise them.

---

## Part 1 — How it works

### 1. The two per-CPU substrates

Per-CPU state lives in two layers, split by the arch boundary:

- **Arch `CpuLocal`** (`arch/x86_64/`) — the machine's own per-core state: the
  per-CPU **GDT/TSS** (so each core has its own `RSP0` and IST stacks), the
  **syscall-entry scratch** (`rsp_scratch` / `kstack_top`) reached through
  `IA32_KERNEL_GS_BASE` via `swapgs`, and the per-core **LAPIC timer**. Each core
  loads its own on bring-up.
- **Neutral `SchedState`** (`sched.rs`) — the scheduler's per-core state: this
  core's **`current`** thread, its **`idle`** thread (`idle` / `idle_addr`), its
  **`ready` runqueue**, its **`reap`** list, and its `min_vruntime` floor. These
  are `[_; MAX_CPUS]` arrays indexed by a **dense CPU index**, all under the one
  global `SCHED` lock (`IrqSpinLock<SchedState>`, the rank-1 kernel lock).

`MAX_CPUS = 8` sizes the arrays and the CPU bitmasks. QEMU is exercised with
`-smp 4`.

Neutral code never reads an APIC id, an MSR, or `gs:` directly — it asks
`crate::arch::Smp::current_cpu() -> u32` (the dense index) and indexes its arrays.
That keeps the "which core am I" mechanism swappable behind `crate::arch` (x86
uses `RDTSCP`; an aarch64 port would use `MPIDR_EL1`/`TPIDR_EL1`).

### 2. The dense CPU index (identity)

The dense index is a small, contiguous logical id `0..N` — the array subscript for
all per-CPU state. **It must be unique per core and stable for the core's
lifetime**, or two cores share `current[i]`/`idle[i]` and corrupt each other. It
is established as follows (`arch/x86_64/smp.rs`):

- The x86 mechanism is **`RDTSCP`**, which returns `IA32_TSC_AUX` in `ecx`. Each
  core programs its own dense index into `IA32_TSC_AUX` once at bring-up; every
  later `current_cpu()` is a single cheap `RDTSCP` (`X86Smp::current_cpu`,
  `regs::rdtscp_aux`). `IA32_TSC_AUX` resets to 0, so the BSP reads dense 0 by
  default.
- Indices are assigned by **hardware APIC id**, *unique by construction*. Before
  launching any AP the BSP populates a `DENSE_TO_APIC[]` map — `bind_cpu_identity(0,
  bsp_apic)` then `bind_cpu_identity(idx, ap_apic)` per AP (`main.rs:bring_up_aps`).
  Each core then **adopts its own** index: `adopt_dense_index()` reads its hardware
  APIC id (`CPUID.01H:EBX[31:24]`), finds the matching `DENSE_TO_APIC[]` slot, and
  `wrmsr(IA32_TSC_AUX, that_index)`. A core whose APIC id was never bound **parks**
  rather than guessing an index that could collide.

> **Why not hand the index to the AP?** An earlier scheme passed the index through
> Limine's `extra_argument` and leaned on the TSC_AUX reset-default for the BSP.
> That was racy — a core could run with a colliding index and share another's
> GDT/TSS/scheduler slots. Deriving from the hardware APIC id removed the race
> (Bug 2 below). The **placement** of the BSP's own `init_this_cpu(0)` also matters
> — see Bug 5.

### 3. Boot to userspace, across cores

The BSP runs `kernel_main` (`main.rs`) as the **boot thread** — a real schedulable
thread that *adopts* the `_start` context (`Thread::try_new_boot`, tid 0, no
`KernelStack` of its own). The relevant ordering:

1. **Single-core init** — serial, GDT/IDT tables, paging, initramfs, IRQ routing,
   entropy, device/IO self-tests, drivers, the console.
2. **BSP identity** — `arch::Smp::init_this_cpu(0)` sets the BSP's `IA32_TSC_AUX`
   to 0. **This runs before the scheduler first reads `current_cpu()` and before
   any AP is online** — critical: it must execute while the boot thread is still
   pinned to the BSP (Bug 5).
3. **Scheduler up (single-core)** — `sched::init` builds `SchedState`, the BSP's
   `current` (the boot thread) and `idle` thread; demo kernel threads prove the
   context switch, classes, and vruntime while still single-core.
4. **AP bring-up** — `bring_up_aps` (`main.rs`) binds the identity map, then
   releases each parked AP by storing `ap_entry` into its Limine `goto_address`.
   Each AP runs `ap_entry` → `adopt_dense_index()` (or parks) → `ap_cpu_init`
   (its GDT/TSS/IDT, x2APIC, its LAPIC timer) → `ap_init` (`sched.rs`: its own
   `current` (a transient AP boot thread) + `idle` thread + `ready`/`reap`
   reserves, sets its `cpu_online` bit and the lock-free `ONLINE_MASK`). The AP's
   boot thread then `exit_thread`s, switching the AP into its idle loop; the AP
   runs `idle_body` (`reap_pending(); hlt();`) until the scheduler gives it work.
   After this point **any online CPU may pick up runnable work**.
5. **First userspace** — `run_first_userspace` (`main.rs`) loads the init ELF
   (`ImageId::Init`), builds its address space, creates `Process` pid 1, and
   `sched::spawn_user`s init's first thread — which `place_thread` puts on the
   **least-loaded** core (§6), so userspace starts on an AP as readily as the BSP.
   init mounts the ext4 fs-server, reads `/system/current-generation`, and spawns
   `eshell`; those are more user threads, likewise distributed.
6. **Boot thread retires** — the BSP boot thread draws the boot screen and calls
   `sched::exit_thread` (`main.rs`). It must *not* fall through to `_start`'s
   `halt_loop` (which `cli`s and would freeze preemption); `exit_thread` switches
   to the idle thread, which `hlt`s with interrupts enabled so the tick keeps
   driving the scheduler. The boot thread is parked in `reap` and reclaimed by the
   next `reap_pending`.

### 4. The context switch — `switch_into`

Every voluntary or involuntary switch funnels through **`switch_into`**
(`sched.rs`), the shared tail of `switch_to_next` (preempt/yield),
`block_current_and_switch` (blocking wait), `finish_exit` (thread exit), and
`suspend_with_fault`. The caller, **under the held `SCHED` guard**, has already
re-homed the outgoing thread (into `ready`/`blocked`/`reap`/`suspended`), set the
incoming thread `Running`, and installed it as `current`. `switch_into` then, in
order:

1. Reads the incoming thread's saved RSP and **page-table root** (`resolve_root` →
   the process's CR3, or the boot root for kernel threads).
2. Records `last_cpu = this_cpu()` on the incoming thread (for cache-warm wakeups).
3. **Raises the outgoing thread's `on_cpu` guard** (`Thread::set_on_cpu(out, true)`)
   — while still holding `SCHED` (§7, invariant I2).
4. **Releases `SCHED`** but keeps interrupts masked (`release_keeping_irqs_masked`).
5. **Arms the incoming thread's kernel-entry state on this core**
   (`arm_kernel_stack_for(next)`): sets **TSS.RSP0** (`Cpu::set_kernel_stack`) and
   the **syscall entry stack** to the incoming thread's `kstack_top`, and
   re-asserts this core's **`KERNEL_GS_BASE`** (`arm_user_entry_cpu_base`). This is
   what makes migration safe (§5).
6. **Loads CR3** (`Paging::set_page_table(next_root)`) — before the stack swap, so
   a dying thread leaves CR3 on the incoming root before its own address space is
   torn down.
7. **`context_switch(out_slot, next_sp, out_on_cpu)`** — the arch asm
   (`arch/x86_64/context.rs`): pushes the six callee-saved registers onto the
   outgoing stack, `mov [rdi], rsp` **commits** the outgoing thread's resume RSP,
   then `mov byte [rdx], 0` **clears its `on_cpu` guard** (only now is its parked
   context valid to resume elsewhere — the commit store precedes the clear store,
   and x86-TSO keeps that order visible to other cores), then `mov rsp, rsi` swaps
   to the incoming stack, pops its callee-saved registers, and `ret`s into either
   the incoming thread's `context_switch` caller (a resuming thread) or
   `thread_trampoline` (a never-run thread).
8. On resume, restores this thread's saved interrupt state.

**First run — `thread_trampoline` → `thread_enter`.** A never-run thread's
fabricated frame `ret`s into `thread_trampoline` (`context.rs`): `sti` (it did not
arrive via an `iretq` that would restore IF), then `call thread_enter`
(`sched.rs`). `thread_enter` reads the current thread under `SCHED`; if it is a
**user** thread it points TSS.RSP0 + the syscall stack at this thread's kernel
stack, re-arms `KERNEL_GS_BASE`, and `enter_user`s to ring 3 (seeding
`rdi/rsi/rdx/rcx` with the spawn hand-off — notification channel, root namespace,
etc.). A **kernel** thread runs its body then `exit_thread`s.

### 5. Cross-CPU migration — what must be re-armed, and where

A thread migrates when it runs on a different core than last time (via placement,
wake, or a steal). Everything a thread needs that is **per-core** must be
re-pointed at *this* core's copy / *this* thread's resources on the way in. All of
it happens on **every** switch-in, so migration needs no special path:

| Per-core / per-thread state | Where re-armed | Consequence if stale |
|---|---|---|
| **CR3** (address space) | `switch_into` step 6, every switch | user thread faults on its own code/data |
| **TSS.RSP0** (ring3→ring0 trap stack) | `arm_kernel_stack_for`, every switch (+ `thread_enter` at first descent) | a trap/IRQ from ring 3 pushes onto the wrong core's stack → `#DF` |
| **Syscall entry stack** | `arm_kernel_stack_for`, every switch | first `syscall` after migration lands on a stale stack |
| **`KERNEL_GS_BASE`** | `arm_kernel_stack_for` (`arm_user_entry_cpu_base`) | `swapgs` in the syscall stub reads the wrong core's block |
| **Syscall MSRs (`EFER.SCE`, `STAR`/`LSTAR`)** | re-armed at each ring-3 **descent** (`arm_user_entry_cpu_base`, cheap `rdmsr(EFER)` gate) | first `syscall` `#UD`s (Bug 1) |
| **Dense index (`TSC_AUX`)** | set once at bring-up, never per-thread | see Bugs 2 & 5 |

The principle: **a thread carries no per-core assumptions across a switch** — the
switch re-establishes them from the incoming thread + the running core. This is
why enabling user-thread migration was ultimately a two-line change (§6): the
machinery was already correct once the hazards below were fixed.

### 6. Placement and work-stealing

- **Placement** (`place_thread`, `sched.rs`): a newly spawned thread — user or
  kernel — goes on the **least-loaded** affinity-permitted core (`pick_target_cpu`),
  so userspace uses the APs from the start. A **woken** thread re-homes to its
  `last_cpu` when permitted (`pick_wake_cpu`), else the least-loaded core —
  cache-warm, avoiding needless migration.
- **Pick** (`pick_next`): this core's own `ready` queue first (`dequeue_front`, an
  O(n) class-aware scan: RealTime by priority, else TimeShared by min-vruntime),
  else **steal** from the busiest peer.
- **Work-stealing** (`steal_one` / `stealable_to`): an otherwise-idle core takes
  one runnable thread from the busiest other core's `ready` queue. A thread is
  stealable to core `me` if its **affinity** includes `me` **and** its **`on_cpu`
  guard is clear** (I2). User and kernel threads alike are stealable.
- **Affinity**: a per-thread `cpu_mask` (`sys_thread_set_affinity`); placement and
  stealing honour it.

### 7. The invariants that keep it correct

These are the load-bearing rules. Every bug in Part 2 was a violation of one.

- **I1 — the running thread *is* `current[this_cpu()]`.** The whole scheduler
  assumes the code executing on a core equals that core's `current` slot. Violated
  transitively by Bugs 4 and 5 (a double-run, and a core with the wrong dense
  index), which is why both manifested as "the wrong thread got reaped."
- **I2 — the `on_cpu` guard: a switched-out thread is not resumable until its
  context is committed.** Set under `SCHED` before the switch releases the lock;
  cleared by `context_switch` *after* it commits `saved_sp`. Stealers skip guarded
  threads. Without it, the window between "enqueue `prev` into `ready` + release
  `SCHED`" and "`context_switch` writes `saved_sp`" lets another core steal `prev`
  and resume it from a **stale `saved_sp`** — a double-run (Bug 4). Modeled on
  Linux's `task_struct::on_cpu` + `smp_cond_load_acquire(&p->on_cpu)`.
- **I3 — the dense index is unique and stable.** Enforced by APIC-id derivation
  (§2). Any code that writes `IA32_TSC_AUX` must run on the core it is labelling,
  before that core can migrate (Bug 5).
- **I4 — the idle thread is never enqueued into `ready` and never reaped.** It
  lives only in `idle_slot`; the reap sweeps exclude `tid == IDLE_TID`. Its kernel
  stack is live for the core's lifetime; freeing it is a use-after-free (Bug 3, and
  the downstream symptom of Bug 4).
- **I5 — the shared kernel vmap stays TLB-coherent across cores.** Kernel stacks
  live in a vmap region mapped in every address space; unmapping/​freeing one must
  invalidate other cores' TLBs before the frame is reused (§8).

### 8. TLB shootdown

`crate::tlb` is the architecture-neutral coordinator; `arch::send_shootdown_ipi`
(vector `0x40`) is the transport. On a kernel-vmap free (`KernelStack::Drop`):
clear the PTEs, then `shootdown_all()` — broadcast to every *other* online core
(`sched::online_mask() & !(1<<me)`), each of which invalidates and acknowledges,
and spin until all acknowledge — *then* return the frames to the allocator. The
lock is a plain (non-IRQ-masking) spinlock and callers run with interrupts
enabled, so a core spinning for acks still services an incoming shootdown IPI —
two initiators cannot deadlock. **Map-side installs are not shot down** (adding a
mapping needs no cross-core invalidation on x86; the vmap PDPT is pre-allocated so
intermediate structures are already present) — matching Linux, which shoots down
only on unmap / permission-restrict.

Broadcast-to-all is correct but unoptimized; per-`AddressSpace` `active_cpus`
(target only cores holding the root) is deferred (§Deferred).

---

## Part 2 — The bugs: what, how found, fix

Bringing SMP from "APs online, userspace pinned to the BSP" to "userspace migrates
freely" surfaced a family of bugs. They shared a signature — an intermittent
`#DF` / ring-3 segfault when threads ran on APs — and a personality: a **bring-up
timing heisenbug**.

### Methodology (how any of this was findable)

- **KVM reproduces; TCG hides.** The race is sensitive to real hardware timing
  (true concurrency, real MSR/EFER semantics). Under QEMU **TCG** it rarely or
  never fired; under **`-accel kvm`** it fired ~13–33 % of boots. The dev loop is
  TCG, so the whole investigation moved to a KVM boot-loop harness (boot N times,
  grep the serial log for `eshell>` vs `CPU EXCEPTION`/`unhandled ring-3`/`PANIC`).
- **Instrumentation suppresses it.** Because it is a timing race, anything on the
  hot path perturbs it away: logging every context switch, reading `CPUID` (a
  serializing instruction) every switch, or printing on every ring-3 descent all
  drove the failure rate to zero without fixing anything. The rule that worked:
  **never probe the hot path** — record into non-perturbing atomic ring buffers and
  print **only from the fault path**, or gate a probe on the rare buggy condition.
- **The probe toolkit** (all removed after the fix): a per-`KernelStack::Drop`
  **drop-ring** (atomic stores; dumped on fault) to see which stack was freed; a
  `#[track_caller]` marker on the reap path to name the caller that reaped a live
  thread; a per-switch **trace ring** (dense index + thread ids) to reconstruct
  `current` history; and finally a fault-path marker printing `this_cpu()`
  **vs the true hardware APIC id** — the one read that had to be non-perturbing, so
  it fired once, at the fault.

### Bug 1 — syscall MSRs not armed at a ring-3 descent (`#UD`)

**What.** A CPU could reach a ring-3 descent before its bring-up
`init_syscall_entry` was in effect (`EFER.SCE = 0`), so the thread's first
`syscall` raised `#UD`.

**How found.** QEMU `-d cpu_reset` (TCG) dumped CPU state including `EFER` on every
reset and showed the triple-fault reboots correlated with `SCE=0`.

**Fix.** `arm_user_entry_cpu_base` (already re-asserts `KERNEL_GS_BASE` on every
descent for migration) now also **ensures the syscall MSRs are armed** — a cheap
`rdmsr(EFER)` gate on every descent, with the full re-arm only in the
should-never-happen unarmed case, so no steady-state cost.

### Bug 2 — dense-index collision via a handed-off index

**What.** Dense indices were handed to APs via Limine `extra_argument` and relied
on the `TSC_AUX` reset-default for the BSP — racy. A core could run with a
**colliding** index and share another core's GDT/TSS/scheduler slots; loading the
wrong TSS delivered an exception onto a shared `RSP0` → `#DF`.

**How found.** Reasoning from the "wrong TSS / shared per-CPU slot" symptom plus
the observation that the failure was tied to going full-SMP, not to any one thread.

**Fix.** Derive dense indices from the **hardware APIC id**, unique by construction
(`bind_cpu_identity` / `adopt_dense_index`, §2); a core whose APIC id was never
bound **parks** instead of colliding. `extra_argument` is no longer used for
identity.

### Bug 3 — idle-thread reap use-after-free

**What.** The reap sweeps could reclaim a per-core **idle** thread, freeing a
kernel stack that is live for the core's lifetime (I4).

**How found.** A reap-path probe showed an `IDLE_TID` thread being pushed to the
reap list; the drop-ring then showed its stack freed while in use.

**Fix.** Never reap `tid == IDLE_TID`: the process-teardown sweeps
(`reap_matching` / `reap_blocked_matching`) skip idle threads (they carry
`owner_pid == 0`, so a `pid 0` teardown would otherwise sweep them all).

### Bug 4 — the switch-out race (the big one)

**What.** In `switch_to_next` the outgoing thread `prev` is enqueued into its
core's `ready` queue and `SCHED` is released (inside `switch_into`) **before**
`context_switch` commits `prev`'s `saved_sp`. Another core could `steal_one` `prev`
in that window and resume it from a **stale/uninitialized `saved_sp`** → `prev`
runs on two cores → `current[]` desyncs from the actually-running thread (I1
violated). The end symptom: the boot thread, double-run, reached its `exit_thread`
while its core's `current` had been left pointing at the **idle** thread, so
`cur_slot.take()` returned idle and `finish_exit` **reaped a live idle stack** →
`#DF` on that idle thread's next interrupt (it `hlt`s, an IRQ fires, the frame
push lands on the now-unmapped stack).

**How found.** A KVM boot-loop pinned the failure at ~13 %. The full `#DF` register
dump showed `rip` in `idle_body` and `rsp` in a kernel-vmap stack page that a
fault re-walk found **not present** — a *freed* idle stack. The drop-ring named the
exact freed range as CPU 0's idle stack (cross-referenced against a boot-time
"idle stack top per core" log). A `#[track_caller]` reap marker named the reaper:
the boot thread's `exit_thread` at `main.rs`. A per-switch trace ring then showed
the boot thread **migrating** across cores just before the reap. A clean
"disable work-stealing" experiment dropped the rate to 0/80, confirming the steal
path as the vector (an earlier, contaminated version of that experiment had hit an
unrelated idle-fallback panic and been inconclusive — worth calling out, because it
briefly pointed the wrong way).

**Fix.** The **`on_cpu` guard** (I2): a per-`Thread` `AtomicBool` set under `SCHED`
in `switch_into` before the lock release, and cleared by `context_switch`'s asm
*after* it commits `saved_sp`; `stealable_to` skips guarded threads. x86-TSO orders
the sp-commit store before the flag-clear store, so a stealer that observes the
clear also sees the final `saved_sp` — no explicit fence. This cut the rate 13 % →
~1.7 % (one bug down, one to go).

### Bug 5 — dense-index collision, take two (misplaced `init_this_cpu(0)`)

**What.** `init_this_cpu(0)` — which hard-codes dense index 0 via
`wrmsr(IA32_TSC_AUX, 0)` — lived inside `run_first_userspace()`, **called after
`bring_up_aps()`**. Once APs are online the (now-migratable) boot thread could run
that line **on an AP**, zeroing *that AP's* `TSC_AUX`. The AP's `current_cpu()`
then returned 0, aliasing it onto dense 0 and sharing the BSP's `current[0]` /
`idle[0]` slots (I3 violated) — the same slot-sharing class as Bug 2, reintroduced
purely by call placement.

**How found.** After Bug 4, ~1.7 % still failed with the same idle-reap signature.
The decisive probe was a fault-path marker printing `this_cpu()` **and** the true
hardware APIC id: it read **`cpu=0` but `hw_apic=3`** — the boot thread was
physically on APIC-3 but `this_cpu()` said 0, so APIC-3's `TSC_AUX` had been zeroed
after adoption. A boot-time dump of the `DENSE_TO_APIC` map and each core's
adoption confirmed the map itself was a correct identity — so the corruption had to
be a later `wrmsr`, which led straight to the misplaced `init_this_cpu(0)`.

**Fix.** Move the BSP-identity block to **before `bring_up_aps()`** (and before the
scheduler first reads `current_cpu()`), where the boot thread is still pinned to
the BSP. Each AP still derives its own index from hardware in `adopt_dense_index`.
Result: **0 failures / 150** KVM boots.

### Bug 6 — host-test SIGSEGV from privileged TLB flushes

**What.** `KernelStack::Drop` → `tlb::shootdown_all` → `Paging::flush_tlb_*` runs
privileged `invlpg` / `mov cr3`, which `#GP` (SIGSEGV) under host `cargo test`
(ring 3). `mm::kstack::tests::drop_unmaps_stack_pages` had been crashing the suite
since the TLB-shootdown commit.

**How found.** Running `cargo xtask test` during cleanup; the crash was isolated to
the kstack drop test and traced to the flush primitives.

**Fix.** `flush_tlb_page` / `flush_tlb_all` gain `#[cfg(test)]` no-op stubs
(mirroring `smp.rs`'s `current_cpu` / `init_this_cpu` test stubs) — host tests
exercise the page-table *memory* edits via the HHDM and have no TLB to flush.

### Enabling user-thread migration (the payoff)

With Bugs 1–5 fixed, migrating a running user thread was safe (§5), so enabling it
was minimal: `place_thread` distributes new user threads with `pick_target_cpu`
(instead of pinning to the creating core), and `stealable_to` no longer excludes
user threads (`Thread::is_user` removed as its last consumer). Verified 0/150 KVM
boots with userspace on the APs, plus a 50-boot scripted `eshell` stress
(`help`/`lsblk`/`mounts`/`cat …`) clean — user threads doing console and fs
syscalls while migratable.

---

## Deferred

- **Cross-CPU deschedule IPI** — so `exit_process` / kill can stop a sibling thread
  running as another core's `current`. Not yet triggerable: every userspace process
  is single-threaded today, so `exit_process` never has a live off-core sibling
  (the `has_live_siblings` scan already *sees* other cores' `current[]`; it just
  can't *stop* one). Lands with the first multi-threaded user process.
- **Per-`AddressSpace` `active_cpus`** — a bitmask of cores holding the root in CR3,
  maintained at the CR3 load in `switch_into`, so a shootdown targets only those
  cores instead of broadcasting to all online cores. A correctness-neutral
  optimization; broadcast-to-all is already correct.
- **`sys_thread_set_affinity`-driven active migration** — changing affinity to
  exclude a thread's current core does not yet forcibly migrate it mid-slice
  (it moves on its next reschedule).

## References

- [scheduler.md](scheduler.md) — scheduling policy (classes, vruntime, slice plan).
- [memory-management.md](memory-management.md) — address spaces, the kernel vmap,
  TLB.
- [boot-flow.md](boot-flow.md) — the boot sequence this doc's §3 slots into.
- Decision log ([../history/decision-log.md](../history/decision-log.md)),
  2026-07-01 entries — the SCE / APIC-id fixes, the switch-out race + dense-index
  collision fixes, and user-thread migration.
- Deferred decisions
  ([../rationale/deferred-decisions.md](../rationale/deferred-decisions.md)) —
  TLB-shootdown scope, x2APIC.
