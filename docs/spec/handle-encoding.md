# Handle Encoding Specification

This document specifies the bit-level encoding of `RawHandle` values, the layout of handle table entries, and the rights bitmask. This is a normative contract; both kernel and userspace must agree on every detail.

**Status:** Pre-stabilization. Subject to change before v1.0 ABI freeze.

## RawHandle layout

A handle is a 64-bit opaque integer:

```rust
#[repr(transparent)]
pub struct RawHandle(u64);
```

The 64 bits decompose into a 32-bit slot identifier and a 31-bit generation counter, with bit 63 reserved zero:

```
 63 62                           32 31                              0
┌──┬──────────────────────────────┬─────────────────────────────────┐
│ 0│   generation counter (31)    │        slot identifier (32)     │
└──┴──────────────────────────────┴─────────────────────────────────┘
  ▲
  └─ reserved zero
```

The slot identifier further decomposes:

```
 31              20 19                                              0
┌──────────────────┬──────────────────────────────────────────────────┐
│  segment id (12) │              index in segment (20)               │
└──────────────────┴──────────────────────────────────────────────────┘
```

| Field | Bits | Range | Purpose |
|---|---|---|---|
| reserved | 63 | 0 | Reserved zero (see below) |
| segment id | 31:20 | 0 – 4095 | Top-level directory index |
| index in segment | 19:0 | 0 – 1,048,575 | Slot index within segment |
| generation | 62:32 | 0 – 2,147,483,647 | Detects use-after-close |

**Bit 63 is reserved zero.** Syscalls return handles in the result register, which also encodes error values (`KError`) as *negative* `isize`. A full 32-bit generation would let a handle whose generation reached `0x8000_0000` set bit 63 and read back as a negative `isize`, aliasing an error code. Capping the generation at 31 bits (`GENERATION_MAX = 0x7FFF_FFFF`) keeps every issued handle a non-negative `isize`, so the value and error spaces never collide. This is why the generation field is 31 bits, not 32, and why a slot is *retired* rather than wrapped at the cap (see [Generation counter behavior](#generation-counter-behavior)).

Encoding and decoding helpers in `libkern`:

```rust
impl RawHandle {
    pub const NULL: RawHandle = RawHandle(0);

    /// Largest generation a slot may be issued with (31 bits; bit 63 reserved).
    pub const GENERATION_MAX: u32 = (1 << 31) - 1; // 0x7FFF_FFFF

    pub fn encode(seg_id: u32, slot_id: u32, generation: u32) -> Self {
        debug_assert!(seg_id < 4096);
        debug_assert!(slot_id < (1 << 20));
        debug_assert!(generation <= Self::GENERATION_MAX); // bit 63 reserved zero
        let slot = ((seg_id as u64) << 20) | (slot_id as u64);
        Self(((generation as u64) << 32) | slot)
    }

    pub fn decode(self) -> (u32, u32, u32) {
        let slot = self.0 as u32;
        let seg_id = slot >> 20;
        let slot_id = slot & ((1 << 20) - 1);
        let generation = (self.0 >> 32) as u32;
        (seg_id, slot_id, generation)
    }
}
```

## Reserved values

| Value | Meaning |
|---|---|
| `RawHandle(0)` | Reserved; always invalid. Used to signal "no handle." |

The kernel never issues a handle equal to `RawHandle::NULL`. Userspace may use `RawHandle::NULL` as a sentinel.

## Default capacity

| Parameter | Default | Configurable |
|---|---|---|
| `DIRECTORY_LEN` | 256 | Yes (compile-time constant) |
| `SEGMENT_LEN` | 4096 | Yes (compile-time constant) |
| Maximum segment id | 4095 | No (limited by 12-bit field) |
| Maximum slot index | 1,048,575 | No (limited by 20-bit field) |
| Maximum system handles | ~1,048,576 | Yes via `DIRECTORY_LEN` × `SEGMENT_LEN` |
| Maximum handles per process (soft) | 65,536 | Yes |

## Handle table entry layout

This is kernel-internal but normative for the kernel's own implementation:

```rust
#[repr(C, align(64))]
struct HandleEntry {
    seq:          AtomicU32,      // seqlock; even = stable, odd = mid-update
    generation:   u32,            // bumped on each slot reuse
    owner_pid:    u32,
    rights:       Rights,         // 8 bytes
    object_type:  KObjectType,    // 4 bytes
    _pad1:        u32,
    object:       AtomicPtr<()>,  // 8 bytes; type-erased; dispatch via object_type
    next_owned:   RawHandle,      // 8 bytes; intrusive list for per-process cleanup
    free_next:    u32,            // index of next free slot (valid only when free)
    _pad2:        u32,
}
```

Total size: 64 bytes (one cache line on x86_64). The 64-byte alignment is enforced.

## Validation algorithm

When a syscall receives a `RawHandle`, the kernel performs the following checks in order:

1. Decode `(seg_id, slot_id, gen_expected)` from the handle value.
2. If `seg_id >= DIRECTORY_LEN`, return `InvalidHandle`.
3. Load `segment = directory[seg_id]`. If `null`, return `InvalidHandle`.
4. If `slot_id >= SEGMENT_LEN`, return `InvalidHandle`.
5. Read the entry under the seqlock protocol (loop until even seq before and after).
6. If `entry.object` is null, return `InvalidHandle`.
7. Try to acquire object refcount via `ObjectRef::try_acquire`. If fails (refcount was 0), return `InvalidHandle`.
8. Re-read seq; if changed, release refcount and retry.
9. If `entry.generation != gen_expected`, release refcount, return `InvalidHandle`.
10. If `entry.owner_pid != caller_pid`, release refcount, return `InvalidHandle`.
11. If `(entry.rights & required_rights) != required_rights`, release refcount, return `NoAccess`.
12. Return `ObjectRef` (caller releases on syscall return).

The owner check (step 10) is the security-critical one. See [why-capabilities.md](../rationale/why-capabilities.md) for the rationale.

## Generation counter behavior

- Generation starts at `0` for a freshly-allocated segment slot.
- On every allocation that reuses a slot, generation is incremented by one, **modulo `GENERATION_MAX + 1`** (i.e. masked to 31 bits — see wraparound below).
- Generation is **not** incremented on close; it's incremented on the next allocation that reuses the slot.
- This means: a closed handle's last-known-valid generation matches the entry's current generation. Lookups against the closed handle fail because `entry.object` is null (step 6) and because the next allocation (which bumps the generation) hasn't happened yet.
- After the slot is reused, the new handle has a new generation; the old handle value with the old generation will fail the generation check (step 9).

### Wraparound at `GENERATION_MAX`

The generation field is 31 bits (bit 63 reserved zero, per [RawHandle layout](#rawhandle-layout)), so it is bumped modulo `GENERATION_MAX + 1 = 0x8000_0000`: a slot is issued generations `1, 2, …, GENERATION_MAX, 0, 1, …`. The mask both performs the wrap and guarantees bit 63 stays clear.

This admits a bounded ABA: a stale handle for slot `S` at generation `G`, held **unused** across exactly `2³¹` reuses of *that same slot*, would re-validate against whatever object occupies `S` at generation `G` then. This is accepted, because:

- It requires `2³¹` (~2.1 billion) reuses of one specific slot while a single handle value is retained and unused — unreachable in practice.
- The owner-PID check (step 10) confines it: the re-validated object must currently be owned by the **same process** issuing the stale handle. So the worst case is a process confusing two of *its own* handles — a within-process correctness hazard, **not** a cross-process privilege escalation. It is outside the threat model capabilities exist to enforce.

The rejected alternative — *retiring* a slot at `GENERATION_MAX` (never recycling it) — would make the generation strictly non-repeating but turns a trivial unprivileged `open`/`close` loop into a slow, permanent, global handle-table leak (the table is global; any process can drive it). The wrap is steady-state and never loses a slot, which is the better property for a long-lived system. See the decision log (2026-06-11) for the full analysis. Implementation: `kernel/src/handle/table.rs` (the generation bump in `allocate`).

## Rights bitmask

```rust
bitflags! {
    pub struct Rights: u64 {
        // Generic rights (apply to all handle types)
        const DUPLICATE          = 1 << 0;
        const TRANSFER           = 1 << 1;
        const INSPECT            = 1 << 2;
        const WAIT               = 1 << 3;

        // Type-specific principal rights
        const READ               = 1 << 8;
        const WRITE              = 1 << 9;
        const EXECUTE            = 1 << 10;
        const SIGNAL             = 1 << 11;
        const TERMINATE          = 1 << 12;
        const LOOKUP             = 1 << 13;
        const BIND               = 1 << 14;
        const MAP_READ           = 1 << 15;
        const MAP_WRITE          = 1 << 16;
        const MAP_EXEC           = 1 << 17;
        const SEND               = 1 << 18;
        const RECV               = 1 << 19;

        // Type-specific modifier rights
        const SEEK               = 1 << 32;
        const APPEND             = 1 << 33;
        const TRUNCATE           = 1 << 34;
        const UNBIND             = 1 << 35;
        const ENUMERATE          = 1 << 36;
        const INSPECT_MEMORY     = 1 << 37;

        // Reserved bits
        // 4..7   reserved for future generic rights
        // 20..31 reserved for future principal rights
        // 38..63 reserved for future modifier rights
    }
}
```

Rights validity by handle type is enforced at allocation time (the kernel rejects nonsensical right combinations like `MAP_WRITE` on a `Process` handle).

## Rights subset semantics

`r1 ⊆ r2` (read: "r1 is a subset of r2") means `(r1 & r2) == r1`.

- `sys_handle_restrict(h, new_rights)`: result rights = `h.rights & new_rights`. Cannot amplify.
- `sys_handle_duplicate(h, new_rights)`: result rights = `h.rights & new_rights`. Cannot amplify. Requires `DUPLICATE` on `h`.
- `sys_handle_grant` (during process spawn or IPC transfer): destination rights = `source.rights & requested_rights`. Cannot amplify.

The kernel never grants rights that the source handle does not possess, regardless of what the caller requests.

## Type-rights compatibility matrix

The kernel rejects allocations of handles with rights that are not meaningful for the object type. A summary of valid principal rights per type:

| Object type | Valid principal rights |
|---|---|
| `Process` | `SIGNAL`, `TERMINATE` |
| `Thread` | `SIGNAL`, `TERMINATE` |
| `Namespace` | `LOOKUP`, `BIND` |
| `MemoryObject` | `MAP_READ`, `MAP_WRITE`, `MAP_EXEC` |
| `IpcChannel` | `SEND`, `RECV` |
| `NotificationChannel` | `WAIT` (receive end only) |
| `Timer` | `WAIT` |
| `InterruptObject` | `WAIT` |
| `PendingOperation` | `WAIT` |
| `IoRing` | `READ`, `WRITE` (for SQE/CQE access) |
| `EntropyObject` | `READ` |
| `DeviceNode` | `READ`, `INSPECT` |
| `UserspaceServerReg` | (internal; not user-accessible) |

Resource handles (returned by namespace lookups) receive principal rights (`READ`/`WRITE`/`EXECUTE`) per the resource server's metadata and the rights requested by the caller.

Generic rights (`DUPLICATE`, `TRANSFER`, `INSPECT`, `WAIT`) are valid on any handle type but may be stripped at issuance for handles intended to remain process-local or non-transferable.

## Endianness

All multi-byte integers in handle table entries and the `RawHandle` value are little-endian on x86_64 (native). On aarch64, native little-endian is also assumed; big-endian variants are out of scope.

## Where to read more

- [Handle system architecture](../architecture/handle-system.md) — implementation details, lookup path, allocation, close
- [Why capabilities](../rationale/why-capabilities.md) — design rationale
- [Kernel objects reference](../reference/kernel-objects-catalogue.md) — per-type rights and operations
