# Lens: concurrency under `unsafe`

The locks, atomics, interrupt state and per-CPU data the kernel's `unsafe` relies on. Read
`kernel/docs/lock-ordering.md` first.

1. **What each `unsafe` assumes about other CPUs and interrupts**: a lock held, interrupts off, a
   per-CPU structure only this CPU touches. Is that true at every caller, interrupt and DPC context
   included?
2. **Atomics.** Is the ordering enough for what the other side reads: a flag published before the
   data it guards, a `Relaxed` load that gates a dereference?
3. **Lifetimes across CPUs.** An object freed on one CPU while another holds its raw pointer; a
   TLB entry, a deferred drop or a wait-queue entry outliving what it names.
4. **The lock ranks.** A path taking locks out of rank order, freeing under `SCHED`, or holding a
   spinlock across a call that may allocate or block.

Opportunities here are types that prove what a comment asserts: a guard parameter showing a lock
is held, per-CPU access through a type that cannot leave its CPU, an object reference that cannot
outlive its count.

Demonstrate a defect with a host test that interleaves the steps, or a probe that widens the window
(a delay where the race needs one), restored after; or label it `unverified:` with the
interleaving that would show it.
