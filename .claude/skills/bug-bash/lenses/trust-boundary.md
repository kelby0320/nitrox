# Lens: the trust boundary

Data another process, a device or a disk supplies, used as if it had been checked.

1. **Find where data comes in**: a syscall's arguments and the user memory they point at; a
   message, its lengths, offsets, counts and handles; a device's registers, descriptors, rings and
   DMA buffers; a disk's superblock, tables, directory entries and extents.
2. **Follow each value to its uses**: an index, a length, an offset, a loop bound, an allocation
   size, a handle. Is it checked before each use, against the right bound? Do the check and the
   use read the same copy, or is memory another party can write read twice?
3. **Be the sender that breaks it**: zero, the largest value, a length past the buffer, a count
   whose product overflows, an offset that wraps, a cycle in a linked structure, a handle of the
   wrong type or without the right.
4. **Authority**: does an operation check the rights the caller's handle carries, or only that it
   has one? Does a server believe an identity the client asserted, or one it learned over a
   channel the client could have supplied?

Demonstrate with a host test that hands the code's own parser or handler bytes a correct writer
would never produce. A round trip through the matching encoder tests only the encoder.
