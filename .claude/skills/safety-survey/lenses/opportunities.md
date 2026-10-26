# Lens: opportunities

The `unsafe` and raw pointers that could be removed, narrowed or put behind an abstraction. Start
from the scope's inventory: each column with sites is a pattern to account for.

1. **Sort each kind.** For every column the inventory counts, read a sample of its sites, at least
   three and more where they differ, and sort them: inherent (assembly, MMIO, the user-memory
   copies themselves), already behind an abstraction, or an opportunity.
2. **Each opportunity**: the abstraction, its home, the sites it covers — counted, with the command
   — and three representative `file:line`s it fits, checked.
3. **Userspace.** The raw-syscall column is the largest. libos is the intended layer
   (`docs/architecture/libos.md`): `no_std`, no `alloc`, typed `Handle<T, M>`, and a seam that
   makes code above it host-testable. Say what this subsystem would need from it — which typed
   wrappers, which owning types — and whether anything here keeps it from libos: an allocator, a
   critical-path rule in its `CLAUDE.md`, a hot path.
4. **Kernel.** Type-erased object pointers dereferenced at each use; `unsafe fn` entry points whose
   contract is "the caller holds X"; raw-pointer dereferences: which could be a typed reference, a
   guard parameter, or a smaller `unsafe` core with a safe face?
5. **Tests.** Which of this subsystem's `unsafe` a host test reaches, and which could be reached
   under Miri or behind libos's seam.

Return opportunities. A defect you meet on the way goes in `defects`, as the rules define one.
