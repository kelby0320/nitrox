# Lens: logic and boundaries

What the code does at the edges of its inputs.

- Each bound and its neighbour: a limit at N and N + 1, nothing, one, the most.
- Arithmetic: overflow, truncation (`as u32` on a value that can exceed it), signedness, division
  by a value that can be zero.
- Units: bytes against pages or blocks, a count against an index.
- Concurrency: two threads or CPUs here at once, a lock held across a wait, a check separated from
  its use.
- State machines: every transition the code allows, and one it should not.
- Input another process or a device supplies: a length trusted, an offset unchecked.

Demonstrate with a host test where the code has one, or with a constructed input.
