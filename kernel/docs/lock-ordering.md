# Kernel lock ordering

This document records the rank ordering for every long-lived lock in the
Nitrox kernel. Acquisition must follow the order from rank 1 down to
rank 6; taking a lock at rank N while holding any lock at rank M < N
inverts the order and risks deadlock. The kernel's CLAUDE.md references
this document; the architecture overview alludes to the ranking but does
not enumerate it.

Debug builds will eventually track acquisition order and panic on
violations. That mechanism is not yet implemented — for now the order is
enforced by code review.

## Ranks (top to bottom acquisition)

| Rank | Lock                                         | Status                                   |
|------|----------------------------------------------|------------------------------------------|
| 1    | Scheduler runqueue                           | not yet present                          |
| 2    | Wait queue                                   | not yet present                          |
| 3    | Handle-table segment allocation              | not yet present                          |
| 4    | Kernel-object internal locks (VMA tree, etc.)| not yet present                          |
| 5    | IPC channel                                  | not yet present                          |
| 6a   | Slab cache lock (per `SlabCache`)            | live as of Phase 1 slice 2 (slab)        |
| 6b   | Buddy allocator (single global `BUDDY`)      | live as of Phase 1 slice 2 (slab)        |

A lock at a lower rank may not be taken while a lock at a higher rank is
held. Locks at the same rank are independent — they may not be nested in
either order — with one exception, called out below.

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

## Interrupt semantics

The Phase 1 `SpinLock` (`kernel/src/libkern/spinlock.rs`) does **not**
mask interrupts. Phase 1 runs with interrupts disabled throughout — no
IDT, PIC, or APIC has been brought up — so masking is unnecessary today.

When interrupts are enabled (the upcoming IDT slice), every spin lock
that protects data touched by an IRQ handler must switch to an
`IrqSpinLock` variant that saves and restores `RFLAGS` on lock/unlock.
The audit will visit each call site and decide; both allocator locks
(6a and 6b) are likely candidates because allocations from IRQ context
are forbidden but the locks themselves may be observed during shutdown
paths that race with interrupts.

Until that variant exists: no IRQ handler may take any `SpinLock`.

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
