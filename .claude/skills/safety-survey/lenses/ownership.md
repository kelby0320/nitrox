# Lens: ownership and lifetimes

Who owns each handle, mapping, buffer and kernel object, on every path, and whether a type knows.

1. **Handles.** Every handle a call creates, a message brings or a spawn hands over: closed on
   every path, the error paths included, exactly once; moved, not copied, when sent; never used
   after it is closed. A `u64` handle passed around is ownership by convention.
2. **Mappings.** Every map has its unmap on every path; no slice over a mapping outlives the unmap;
   no range is mapped twice.
3. **Buffers.** A `static mut` buffer lent out while a later call can write it: a function that
   returns a `'static` view of a buffer the next receive overwrites, or two live `&mut` to one
   static. A buffer handed to the kernel or a device and reused before the operation completes.
4. **Kernel objects.** Each type-erased pointer's reference: taken, held across the use, given
   back once. A raw pointer from an `ObjectRef` used after the reference drops; a count's increment
   and decrement on every path; a drop where the lock order forbids freeing
   (`kernel/docs/lock-ordering.md`).
5. **Construction and teardown.** On each failure partway through building something, what was
   already taken is given back once — not twice, not never.

Opportunities here are owning types: a handle that closes on drop and moves when sent, a mapping
that unmaps on drop and lends slices tied to its lifetime, a kernel reference that derefs safely,
a guard that gives a slot or a count back once.

Demonstrate a defect by driving the path: a host test, Miri on one test, or a probe counting
handles, restored after.
