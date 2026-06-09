# Kernel lock ordering

This document records the rank ordering for every long-lived lock in the
Nitrox kernel. Acquisition must follow the order from rank 1 down to
rank 7; taking a lock at rank N while holding any lock at rank M < N
inverts the order and risks deadlock. The kernel's CLAUDE.md references
this document; the architecture overview alludes to the ranking but does
not enumerate it.

Debug builds will eventually track acquisition order and panic on
violations. That mechanism is not yet implemented — for now the order is
enforced by code review.

## Ranks (top to bottom acquisition)

| Rank | Lock                                         | Status                                   |
|------|----------------------------------------------|------------------------------------------|
| 1    | Scheduler runqueue (`SCHED`, **`IrqSpinLock`**)| live as of Phase 1 slice 9 (scheduler) |
| 2    | Wait queue                                   | not yet present                          |
| 3    | Handle-table segment allocation              | live as of Phase 1 slice 7 (handle table)|
| 4    | Kernel-object internal locks (`AddressSpace`)| live as of Phase 1 slice 5 (item 3)      |
| 5    | IPC channel                                  | not yet present                          |
| 6a   | Slab cache lock (per `SlabCache`)            | live as of Phase 1 slice 2 (slab)        |
| 6b   | Buddy allocator (single global `BUDDY`)      | live as of Phase 1 slice 2 (slab)        |
| 6c   | Kernel-half PML4 template (`KERNEL_TEMPLATE`)| live as of Phase 1 slice 5 (item 5)      |
| 6d   | Kernel vmap bump pointer (`VMAP_NEXT`)       | live as of Phase 1 slice 5 (item 6)      |
| 7    | Serial port (`SERIAL`, **`IrqSpinLock`**)    | live as of Phase 1 slice 4 (diagnostics) |

`SCHED` and `SERIAL` are [`IrqSpinLock`]s (they mask interrupts while held);
every other lock is a plain `SpinLock`. See § Interrupt semantics for why only
these two need it.

A lock at a lower rank may not be taken while a lock at a higher rank is
held. Locks at the same rank are independent — they may not be nested in
either order — with one exception, called out below.

## Scheduler runqueue lock is dropped before every context switch

`SCHED` (`kernel/src/sched.rs`) is the rank-1 runqueue lock, an
[`IrqSpinLock`]. The cardinal rule: **the lock is released before every
`context_switch` and re-acquired fresh on resume — it is never held across
a stack switch.** The cooperative `yield_now`/`exit` and the preemptive
`on_timer_tick` all funnel through `switch_to_next`, which mutates the queue
and the current-thread pointer, captures the `(prev_sp_slot, next_sp)` pair,
drops the guard, and only then calls the switch. Every point a thread
*resumes* at (the instruction after `context_switch`, and `thread_enter` for a
freshly scheduled thread) therefore runs with the lock not held.

If the lock were carried across the switch, the *resumed* thread — which
parked at its own `context_switch` call site, lock-free — would eventually
drop a guard it never acquired, releasing a lock another thread still
believes it holds.

**Interrupts must stay masked across the switch**, though — a timer IRQ that
fired mid-`context_switch` would run the handler (and possibly reschedule) on a
half-swapped stack. The cardinal rule (drop the lock first) and this invariant
(keep IF=0 across the switch) are reconciled by
`IrqSpinLockGuard::release_keeping_irqs_masked`: it releases the lock but
*keeps interrupts masked*, returning the prior interrupt state. The switch core
then runs the `context_switch` with IF=0 and restores the prior state on resume
(the cooperative path), while the preemptive path is already IF=0 from the timer
interrupt gate and restores IF via the `iretq` in the timer-stub epilogue. A
freshly scheduled thread (reached via `thread_trampoline`, not an `iretq`)
`sti`s for itself before running its body.

Two corollaries:

- **No allocation under rank 1.** `init` installs a pre-reserved run
  queue (built outside the lock), and `spawn` does all heavy work
  (`KernelStack::new`, `Thread` allocation, frame fabrication) before the
  brief enqueue lock; the enqueue itself stays within the reserve
  (debug-asserted). This keeps the otherwise-legal rank-1 → rank-6
  descent out of the hot path entirely.
- **Reaping runs outside rank 1.** An exited thread cannot free its own
  kernel stack (it is still running on it), so `exit` parks itself in
  `reap`; the next scheduler entry's `reap_pending` `take`s it under the
  lock and drops it *after* releasing — `KernelStack`'s `Drop` unmaps and
  returns frames to the buddy (rank 6), which must not happen under
  rank 1.

## Handle-table segment growth releases rank 3 before rank 6

`HandleTable::grow_one` (`kernel/src/handle/table.rs`) needs to call
the slab/buddy allocators — segments are 256 KiB plus a 16 KiB
scratch shuffle buffer, both routed through `kmalloc` and thence
through the buddy at order 6 and order 2 respectively. Rank 3 cannot
be held across those calls (the rule is rank N may not be held while
acquiring rank M < N is permitted, but acquiring rank 6 while
holding rank 3 inverts the ranking when the allocator wakes up
ranks 6a/6b internally).

The sequence is therefore:

1. Take the rank-3 lock; snapshot the next segment id to grow and
   the PRNG seed to use; release the lock.
2. Without any handle-table lock, call
   `segment::try_alloc_initialised(seed)`, which acquires the slab/
   buddy locks at rank 6.
3. Reacquire the rank-3 lock and either publish the new segment into
   the directory or, on a race, free the spare and return — the
   caller's outer retry loop will observe the racer's segment on
   the next pass.

Phase 1 is single-CPU so step-3 races are impossible; they are
documented because the same mechanism must work under SMP, where two
CPUs may simultaneously decide to grow the same segment id and one
will lose.

## Allocator nesting: slab → buddy is permitted

`SlabCache::grow_locked` holds the cache lock (rank 6a) while calling
`buddy_alloc` (rank 6b). This is the only allocator → allocator nesting
permitted in the kernel.

It is safe because:

- The buddy allocator is a self-contained data structure. It does not
  call into any other kernel subsystem, and in particular does not call
  back into the slab. There is no path from rank 6b back up to rank 6a.
- Phase 1 has no SMP, no preemption, and no interrupts enabled, so
  contention is impossible: at most one CPU is ever inside the slab
  cache at a time, and that same CPU is the only caller into the buddy.

The opposite direction (taking a slab cache lock while holding the
buddy lock) is forbidden. The buddy allocator does not need slab memory
during alloc or free: free-list pointers are stored intrusively in the
free frames themselves, and the coalesce bitmap was allocated once at
buddy init time.

When SMP arrives (Phase 3), the nesting still works because the rank
ordering remains slab → buddy and the lower lock cannot block waiting
for the upper. If a future change makes the buddy depend on the slab,
that closes the cycle and must be rejected at design review.

## Kernel vmap bump pointer is a leaf

`VMAP_NEXT` (`kernel/src/mm/kvmap.rs`) is a `SpinLock<u64>` holding
the next free virtual address in the kernel vmap region. Acquired
briefly per allocation in `vmap_alloc_pages`; no other lock is taken
inside, no other lock is held outside it during the acquire. Rank 6d
keeps it grouped with the other constant-time leaves; like them, it
may be acquired while holding any lock at rank 1–5.

## Kernel-half PML4 template is a leaf

`KERNEL_TEMPLATE` (`kernel/src/arch/x86_64/paging.rs`) is a
`SpinLock<Option<[u64; 256]>>` holding the kernel-half PML4 entries
captured at boot. It is acquired in exactly two places:

- `init_kernel_template(boot_root)` at boot, with no other lock held.
- `X86Paging::inherit_kernel_mappings(root)` inside
  `AddressSpace::new` — at the point of acquisition, the freshly
  allocated PML4 frame is the only AS-related state; `new` has not
  yet wrapped it in its own `SpinLock<Inner>`, so the rank-4
  `AddressSpace` lock is not held.

It nests with nothing and never recurses. Rank 6c keeps it grouped
with the other constant-time leaf-style locks (the allocators);
calling `inherit_kernel_mappings` while holding any lock at rank
1–5 is allowed and expected.

## Paging allocates page-table frames from the buddy

`ArchPaging::map_page` (`kernel/src/arch/x86_64/paging.rs`) calls
`buddy_alloc` to obtain frames for intermediate page tables. The paging
layer holds no lock of its own — the page-table root is passed in by the
caller — so it introduces no new rank and no new nesting. It does
acquire rank 6b transitively: `map_page` must not be called while
holding the rank-7 `SERIAL` lock. The future VMM will call `map_page`
while holding the rank-4 VMA-tree lock, which is correctly ordered (rank
4 is above rank 6b).

## The serial lock is a leaf

`SERIAL` (`kernel/src/arch/x86_64/serial.rs`) guards the COM1 UART. It is
a leaf at rank 7: `write_byte` does nothing but poll an I/O port and emit
a byte — it allocates nothing, calls into no other subsystem, and takes
no other lock. It may therefore be acquired while holding any
higher-ranked lock, and nothing is ever acquired while holding it.

`SERIAL` is an [`IrqSpinLock`]: it masks interrupts while held, so a thread
printing via `kprintln!` cannot be preempted mid-write, and the timer handler
(were it ever to print) could not find the lock held by the context it
interrupted. See § Interrupt semantics.

The panic handler and the CPU exception handlers do **not** take this
lock. They write through `serial::emergency_writer()`, an unsynchronised
path that drives the UART directly. The lock cannot be force-unlocked,
so a handler that tried to lock `SERIAL` after a fault that struck while
the lock was held would deadlock. Bypassing the lock is sound only
because Phase 1 is single-CPU: at fault time no other context can be driving
the UART. This must be revisited at SMP.

The syscall path (`sys_kprint`) takes only `SERIAL`, at rank 7, and holds
**no** lock across the user-memory copy: `copy_slice_from_user` runs its
SMAP window (`stac`/`clac`) and exception-table fault recovery before the
serial lock is acquired. So a faulting user buffer unwinds to a `KError`
without any lock held, and the serial write that follows is a clean leaf
acquisition.

## Interrupt semantics

As of the **preemptive-scheduling slice** the kernel runs with interrupts
enabled (IF=1) after boot, and the periodic timer IRQ drives the scheduler.
Two lock families coexist:

- **Plain [`SpinLock`]** — does not mask interrupts. Correct for data **never**
  touched from an interrupt handler. The timer handler touches only the
  scheduler and the local-APIC EOI register (and performs no allocation), so a
  timer IRQ that preempts a thread holding any of these never tries to take
  them, and there is no reentrancy: the allocators (6a/6b), the kernel vmap
  (6d), the PML4 template (6c), the `AddressSpace` locks (4), and the
  handle-table segment locks (3) all stay plain `SpinLock`.
- **[`IrqSpinLock`]** — captures the prior interrupt state and `cli`s **before**
  acquiring, restoring it after releasing. Required for data shared between
  thread and IRQ context: `SCHED` (the timer reschedules) and `SERIAL` (a
  thread may be printing when the timer fires). These are the **only** two.

**Single-CPU deadlock-freedom.** Because an `IrqSpinLock` masks interrupts for
its whole hold window, a thread holding `SCHED`/`SERIAL` cannot be preempted, so
the timer handler can never find one of them already held by the context it
interrupted — the self-deadlock (handler spins on a lock the preempted code
holds) is impossible. **No IRQ nesting:** the timer/spurious gates are interrupt
gates (they clear IF) and the handler never `sti`s, so handler depth is
single-level and kernel-stack growth is bounded (one interrupt frame).

The audit conclusion (only `SCHED` + `SERIAL` need `IrqSpinLock`) holds as long
as the timer handler keeps doing only EOI + reschedule with no allocation; a
future handler that touches another locked subsystem must re-run this audit.
Allocation from any interrupt/DPC context remains forbidden.

## Adding a new lock

1. Decide where in the ranking the lock belongs by asking: "what does
   the code path that takes this lock need to do while holding it?"
   - If it touches a higher-ranked subsystem, the lock ranks below
     that subsystem (greater rank number).
   - If only lower-ranked subsystems are touched, the lock ranks above
     them.
2. Update this document and the table above.
3. If the new lock participates in a same-rank nesting that is not
   already documented above, that needs a design discussion before
   coding starts.
