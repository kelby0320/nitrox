# Lens: ABI and buffers

Every pointer and length that crosses a boundary: a syscall's arguments, a message between
processes, a device's rings and DMA buffers, a disk's structures read into memory.

1. **Find each crossing.** In userspace, every syscall and every send and receive. In the kernel,
   every syscall handler's user pointers, every copy to or from user memory, and each structure a
   device or a disk supplies.
2. **For each pointer, what is written through it, and how much.** Does the buffer hold what the
   other side writes — a whole message, a whole `HandleInfo`, `count` handles — or only what the
   caller expected? A buffer sized by expectation is the defect desktop-session-mgr had: a 256-byte
   receive the kernel filled with 4,096 bytes. What reads it, and does it read only what was
   written?
3. **For each length, offset and count the other side supplies**: is it checked against the right
   bound before every use, and read once, not twice from memory another party can write?
4. **Layout.** A `#[repr(C)]` type on both sides of a boundary agrees field by field and in size;
   `cargo xtask abi-sync-check` holds libkern to the kernel, but not what a caller passes. A byte
   offset parsed by hand (`buf[4..8]`) agrees with the type that defines it.
5. **Alignment and validity.** A pointer cast to a type its buffer does not align; bytes read as a
   type for which not every bit pattern is valid — a `bool`, an enum, a reference, a `char`.

Opportunities here are where a type would carry the size or the layout: a wrapper whose
out-parameter is `&mut` of the exact ABI type; a message type with typed header accessors in place
of hand-parsed offsets; a parser that returns a bounded view.

Demonstrate a defect with a host test that hands the code bytes or a buffer a correct peer would
never produce, or with Miri on one test.
