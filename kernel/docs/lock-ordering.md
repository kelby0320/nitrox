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
| 1    | Scheduler runqueue                           | not yet present                          |
| 2    | Wait queue                                   | not yet present                          |
| 3    | Handle-table segment allocation              | not yet present                          |
| 4    | Kernel-object internal locks (`AddressSpace`)| live as of Phase 1 slice 5 (item 3)      |
| 5    | IPC channel                                  | not yet present                          |
| 6a   | Slab cache lock (per `SlabCache`)            | live as of Phase 1 slice 2 (slab)        |
| 6b   | Buddy allocator (single global `BUDDY`)      | live as of Phase 1 slice 2 (slab)        |
| 7    | Serial port (`SERIAL`)                       | live as of Phase 1 slice 4 (diagnostics) |

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

The panic handler and the CPU exception handlers do **not** take this
lock. They write through `serial::emergency_writer()`, an unsynchronised
path that drives the UART directly. `SpinLock` cannot be force-unlocked,
so a handler that tried to lock `SERIAL` after a fault that struck while
the lock was held would deadlock. Bypassing the lock is sound only
because Phase 1 is single-CPU with interrupts masked: at fault time no
other context can be driving the UART. This must be revisited at SMP.

## Interrupt semantics

The `SpinLock` (`kernel/src/libkern/spinlock.rs`) does **not** mask
interrupts. As of the Phase 1 diagnostics slice an IDT is installed, but
it handles only CPU exceptions — non-maskable faults — and the kernel
still never executes `sti`, so no hardware IRQ ever fires. The exception
handlers take no lock at all (they use the unsynchronised emergency
serial path described above and then halt), so there is still exactly
one lock-taking execution context and masking remains unnecessary.

When hardware interrupts are eventually enabled (a later slice, with
PIC/APIC bring-up), every spin lock that protects data touched by an IRQ
handler must switch to an `IrqSpinLock` variant that saves and restores
`RFLAGS` on lock/unlock. The audit will visit each call site and decide;
both allocator locks (6a and 6b) are likely candidates because
allocations from IRQ context are forbidden but the locks themselves may
be observed during shutdown paths that race with interrupts.

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
