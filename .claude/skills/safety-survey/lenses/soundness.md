# Lens: `unsafe` soundness

Each `unsafe` block's `SAFETY` argument, checked against the code as a claim to refute.

1. **Read the argument, then the code it covers, its callers and the types.** Does each
   precondition hold on every path in: the reference alive at the use, the index in bounds, no
   other `&mut` alive, the memory initialised? An argument that only asserts ("this cannot fail")
   has nothing behind it until you find what makes it true.
2. **`unsafe fn` contracts.** Is the `# Safety` section complete? Does each caller meet it, and
   say how?
3. **Safe functions over `unsafe`.** Does the safe signature let a caller break what the `unsafe`
   needs? A `pub fn` returning `&'static mut`; a safe function taking an address as a `u64` and
   dereferencing it; a `Send` or `Sync` impl on a type holding raw pointers; a safe function whose
   correctness rests on its caller's order of calls.
4. **Undefined behaviour the hardware would forgive.** A reference to uninitialised or misaligned
   memory; a `&` to memory another thread or a device writes; a data race on a non-atomic; a
   reference to a `static mut`; a value read as a type it does not inhabit.

Opportunities here are narrowings: an `unsafe` block around more than the operation that needs
it, an `unsafe fn` whose contract a type could carry, a safe wrapper that would let a whole module
drop its `unsafe`.

Demonstrate a defect with Miri on one host test wherever the code is host-testable; otherwise with
a host test, a break of the code, or the path pointed at.
