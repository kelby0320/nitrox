# Nitrox: Surviving a Kernel Fault — Design Notes (v1)

## Status

**Not built.** Today the kernel does the opposite of this document: a ring-0 fault stops the
machine deliberately (decision log, 2026-08-19). This describes what it would take to *survive*
one instead, so the long-term intent is written down rather than implied. It graduates to
`architecture/` if and when it is built.

## What we do today, and why

A ring-0 fault or a failed kernel invariant **stops the machine**. That is a deliberate choice
between two coherent options, taken because the third — what the kernel actually did before —
is worse than either:

- **Survive:** the fault kills something smaller than the machine and every CPU stays in
  service. This document.
- **Fail fast:** the machine stops, loudly, with a dump. What we do.
- **Neither:** one CPU halts and the rest run on its debris. What we had.

The third is not a partial implementation of the first. Neither Linux nor Windows enters it:
Linux's oops path never halts a CPU (it kills the *task* and returns the CPU to service, tainting
the kernel — `panic_on_oops` is standard in production precisely because that continuation is
best-effort), and Linux's `panic()` stops every CPU. Windows bugchecks: all processors frozen, a
dump written. Both pick one of the two coherent answers.

## What survival requires

Four things, none of which exist. They are listed in dependency order — the first two are the
hard ones and the third is a prerequisite for the result being useful.

### 1. Distinguish task context from interrupt context

Only a fault taken on behalf of a *thread* — inside a syscall, or in a kernel thread body — has
anything smaller than the machine to kill. A fault in an interrupt handler or a DPC has no task
to blame and no context to return to; that case stays fatal even in a surviving kernel.

The frame already carries enough to tell (`ExceptionFrame`, plus whether a DPC is running), so
this is bookkeeping rather than research.

### 2. Release what the faulting context held

This is the whole problem. The kernel is `panic = "abort"` — there is no unwinding, so nothing
runs a destructor on the way out. Killing the thread therefore leaks whatever it was holding, and
the audit measured three consequences that each wedge the machine (findings C.4a–c, 2026-08-14):

- a **grace-tracker read section** keyed on the CPU index, which blocks handle reclamation for
  every process and hangs `sys_handle_close` machine-wide 256 closes later;
- **any lock** — `SCHED` at rank 1, the slab and buddy caches, the TLB serialiser, the handle
  table's `inner`;
- the thread's **slot in `g.current[]`**, which keeps its process alive forever, so no peer
  blocked on its IPC endpoints is ever released.

So survival needs recovery infrastructure, not just diagnostics:

- locks that record an owner and can be **broken** by a recovery path, rather than only a rank;
- grace contexts that can be **force-quiesced** when their owner is declared dead;
- a way to account for the refcounts a dead thread held.

`lockrank` is the closest thing that exists and it is debug-only and tracks ranks, not owners.
Treat this section as the real cost estimate: it is a subsystem, not a patch.

### 3. I/O routable to any core

`install_isa_irq` routes every ISA GSI to the boot CPU in physical destination mode
(`kernel/src/arch/x86_64/ioapic.rs`). Surviving the loss of the BSP is meaningless while losing
it also means losing disk, serial and PS/2. Needs logical destination mode or per-GSI spreading,
an affinity policy, and re-routing when a CPU leaves the online set.

This is independent of §1 and §2 and is worth having on its own for throughput.

### 4. An honest account of what it buys

Survival buys **debuggability**, not reliability: the machine stays up long enough to be
inspected, and the kernel is thereafter in a state it cannot fully vouch for. Linux's own answer
is that a survived oops taints the kernel and production configurations should panic anyway.

A surviving Nitrox should therefore still offer "stop instead" as a policy, and should say
plainly in the dump that the machine is tainted.

## Trigger

Real-hardware bring-up, or a workload where losing the machine to a single driver bug is
unacceptable. Not Phase 4: the desktop arm needs the *promise* to be clear, which the fail-fast
decision gives it, and does not need survival.
