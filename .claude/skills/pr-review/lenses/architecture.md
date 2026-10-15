# Lens: architecture and the ABI

For a change to the kernel, a syscall, the ABI or a wire format.

- **Authority is a handle.** An operation that takes a process, a resource or a service by name or
  number rather than by handle is a design finding.
- **Async first.** Nothing that can block blocks inside a syscall: it returns a
  `PendingOperation`, and the thread waits in `sys_wait`.
- **No signals, no UID or GID.**
- **A few general syscalls.** A call whose name binds it to one kind of object is a finding.
- **The ABI.** A layout changed on one side of the boundary only. `abi-sync-check` covers part of
  this; say what it does not.
- **Wire formats.** A layout changed with no version bump; a reader that trusts a length.
- **The kernel's rules.** No external crates; nothing from `arch::x86_64` outside
  `kernel/src/arch/`; the lock order in `kernel/docs/lock-ordering.md`; no allocating or freeing in
  a DPC or an interrupt handler.
