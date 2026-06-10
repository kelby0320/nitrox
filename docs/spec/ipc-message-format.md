# IPC Message Format Specification

This document specifies the on-wire format of IPC messages — `IpcMsg` and its header. This is the kernel-level message envelope. Higher-level protocols (such as the resource server protocol) ride on top of this; see [rsproto-wire-format.md](rsproto-wire-format.md) for that layer.

**Status:** Pre-stabilization. Implemented (kernel `IpcChannel` + `sys_channel_create`/`send`/`recv`) with documented deferrals — see [§ Implementation status](#implementation-status). Subject to change before v1.0.

## Constants

| Constant | Value | Purpose |
|---|---|---|
| `IPC_MSG_SIZE` | 4096 | Total size of an `IpcMsg` in bytes |
| `IPC_PAYLOAD_SIZE` | 4008 | Bytes of payload per message |
| `IPC_HANDLE_MAX` | 8 | Maximum transferable handles per message |
| `IPC_HEADER_SIZE` | 24 | Size of `IpcMsgHeader` in bytes |

`IPC_MSG_SIZE = IPC_HEADER_SIZE + IPC_PAYLOAD_SIZE + (IPC_HANDLE_MAX × 8)` — exactly `24 + 4008 + 64 = 4096`. The three regions tile one page with no interior padding; `IpcMsg` is one page on x86_64 by design.

> **Reconciliation note.** An earlier draft listed `IPC_PAYLOAD_SIZE = 4032`, which made the regions sum to `4120 ≠ 4096` — the document was internally inconsistent (it claimed both a one-page envelope and a 4120-byte struct). The implementation pins the clean one-page layout below (`payload = 4096 − 24 − 64 = 4008`); the source is authoritative and this spec is updated to match (decision log, 2026-06-10).

## IpcMsg layout

```rust
#[repr(C, align(4096))]
pub struct IpcMsg {
    pub header:   IpcMsgHeader,                 // 24 bytes   @ offset 0
    pub payload:  [u8; IPC_PAYLOAD_SIZE],       // 4008 bytes @ offset 24
    pub handles:  [RawHandle; IPC_HANDLE_MAX],  // 64 bytes   @ offset 4032
}
```

The wire layout, exactly one page:

```
Offset  Size  Field
─────── ────  ─────────────
   0     24   header
  24    4008  payload
4032      64  handles
─────── ────
        4096
```

Userspace and kernel share this layout via `#[repr(C)]`; the exact byte offsets are pinned by compile-time asserts in `kernel/src/libkern/ipc.rs`. (The kernel stores queued messages in a byte-identical, natural-alignment `StoredMsg` twin — the page alignment matters only to the userspace-facing buffer, not to the kernel's queue slots.)

## IpcMsgHeader

```rust
#[repr(C)]
pub struct IpcMsgHeader {
    pub sender_pid:    ProcessId,    //  4 bytes  offset 0
    pub payload_len:   u32,          //  4 bytes  offset 4
    pub handle_count:  u8,           //  1 byte   offset 8
    pub flags:         u16,          //  2 bytes  offset 9 (alignment: actually offset 10 with padding)
    pub _pad:          u8,           //  1 byte   offset 11
    pub timestamp:     u64,          //  8 bytes  offset 16 (8-byte aligned)
}
```

Wait — alignment requires the actual layout to be:

```
Offset  Size  Field
─────── ────  ─────────────
   0      4   sender_pid       (u32, 4-byte aligned)
   4      4   payload_len      (u32, 4-byte aligned)
   8      1   handle_count     (u8)
   9      1   _pad1            (u8)
  10      2   flags            (u16, 2-byte aligned)
  12      4   _pad2            (4 bytes of padding for u64 alignment)
  16      8   timestamp        (u64, 8-byte aligned)
─────── ────
        24
```

The padding bytes are zeroed by the kernel at message construction.

### Field semantics

**`sender_pid`** — set by the kernel at send time to the sending process's PID. The receiving process can trust this value; userspace cannot forge it.

**`payload_len`** — number of valid bytes in `payload[0..payload_len]`. Must be ≤ `IPC_PAYLOAD_SIZE`. Bytes beyond `payload_len` are zero-filled by the kernel but the receiver should not rely on them.

**`handle_count`** — number of valid handles in `handles[0..handle_count]`. Must be ≤ `IPC_HANDLE_MAX`. Slots beyond `handle_count` are `RawHandle::NULL`.

**`flags`** — bitfield of `IpcMsgFlags`:

```rust
bitflags! {
    pub struct IpcMsgFlags: u16 {
        const URGENT     = 1 << 0;  // hint: deliver before non-urgent (reserved; not initially honored)
        const REPLY      = 1 << 1;  // this message is a reply to a previous one
        // bits 2..15 reserved
    }
}
```

**`timestamp`** — monotonic nanoseconds at the moment the kernel enqueued the message. Set by kernel; cannot be forged.

## Send modes

```rust
#[repr(u32)]
pub enum SendMode {
    Block        = 0,  // block via PendingOperation until queue has space
    NoBlock      = 1,  // signal WouldBlock immediately if queue is full
    BlockBounded = 2,  // block up to deadline, then signal TimedOut
}
```

`BlockBounded` mode requires the caller to also pass a deadline, communicated via `IoOp` parameters (see `sys_io_submit`).

> **Implemented:** `NoBlock` only. `Block` / `BlockBounded` return `Unsupported` until the async-I/O slice lands — they block via a `PendingOperation`, which does not exist yet, and a bidirectional endpoint cannot ride `sys_wait`'s single signaled bit for both "readable" and "writable". See [§ Implementation status](#implementation-status).

## Handle transfer in messages

A sender passes the handles to transfer via `sys_channel_send`'s separate `handles`/`count` arguments (not the in-message `handles[]`, which the kernel fills with the *receiver's* values on receive). Transfer is **always move**, and the install happens at the receiver:

1. Sender calls `sys_channel_send(ch, msg, handles, count, mode)` with `count` handles. Each must be valid in the sender's table with the `TRANSFER` right.
2. Kernel validates + pins each handle (`lookup` with `TRANSFER`); any bad handle fails the whole send (atomic-or-fail) with the sender's table untouched.
3. Kernel queues the message slot, **moving** the pinned references into it ("in flight"). Only then does it **commit the move** by closing the sender's source handles — so a `WouldBlock`/`PeerClosed` send loses no capability.
4. Receiver's `sys_channel_recv` dequeues the message and **installs** each in-flight reference into its **own** table (with the rights the sender's handle carried), writing the new handle values into both the `handles` out-buffer and the in-message `handles[]`, and the count into `*count`. The handles are now valid in the receiver's table; the install is atomic-or-fail (a partial failure reclaims what it installed).

If the receiving endpoint is destroyed with the message still queued, the undelivered in-flight references are released (the objects lose that reference).

If transfer fails partway through (e.g., receiver is out of handle table space), the entire send fails atomically — no handles are transferred and the source's handles remain valid.

By default, handles in messages are **moved** to the receiver. The sender loses access. To duplicate (sender retains access), the sender first calls `sys_handle_duplicate` and includes the duplicate in the message.

## Receive semantics

```rust
fn sys_channel_recv(
    ch:      RawHandle,
    msg:     UserMutPtr<IpcMsg>,
    handles: UserMutPtr<RawHandle>,
    count:   UserMutPtr<usize>,
) -> isize
```

Behavior:

1. If channel queue is empty, returns `WouldBlock`. Caller should `sys_wait` on the channel handle.
2. If queue is non-empty, the oldest message is dequeued.
3. Message body is copied to `*msg` (4096 bytes). The kernel zeroes any portions of `payload` beyond `payload_len`.
4. Handles are written to `handles[0..msg.header.handle_count]`. The kernel writes `handle_count` to `*count`.
5. The handles in `*msg.handles` are kernel-internal copies of the same values; they refer to the same handles now in the receiver's table. (Userspace typically reads from `handles[]` rather than `msg.handles[]`; the duplication accommodates languages that prefer the in-message location.)

Note: the user-supplied `handles` buffer must be large enough to hold `IPC_HANDLE_MAX` (8) entries. The kernel does not bounds-check this against a smaller buffer; passing a smaller buffer is undefined.

## Channel creation

```rust
fn sys_channel_create(
    end0:        UserMutPtr<RawHandle>,
    end1:        UserMutPtr<RawHandle>,
    queue_depth: u32,
) -> isize
```

Creates an IPC channel with two endpoint handles, returned via `*end0` and `*end1`. The channel has separate queues in each direction; sending from `end0` enqueues to `end1`'s receive queue, and vice versa. Each direction has `queue_depth` slots (default 16; max 1024 — TBD).

Each endpoint has full rights initially: `SEND | RECV | DUPLICATE | TRANSFER | INSPECT | WAIT`. The creator typically attenuates rights (via `sys_handle_restrict` or by transferring with reduced rights) when handing endpoints to other parties.

## Dead peer behavior

When one endpoint's last handle is closed, the channel transitions to "peer-closed":

- Pending sends on the still-open endpoint signal with `PeerClosed` error.
- Future sends return `PeerClosed` immediately.
- Pending receives signal with `PeerClosed` error.
- Future receives return `PeerClosed` immediately.
- A `Notification::PeerClosed { handle }` is delivered to every process holding a handle to the still-open endpoint with `WAIT` right.

The channel object is freed when both endpoints' handle counts reach zero.

> **Implemented:** the **error** half (send/recv on a peer-closed endpoint return `PeerClosed = -13`, and a blocked receiver wakes to return it). The async **`Notification::PeerClosed`** is deferred to **Phase 2**: delivering it to *every* holder of the still-open endpoint wants handle duplication + a per-endpoint holder registry (a single holder→notification-channel link only covers the common one-holder case and would have to be rebuilt as a registry), which is Phase-2-shaped. See [§ Implementation status](#implementation-status).

## Implementation status

The channel mechanism (`IpcChannel`, `sys_channel_create` = 12, `sys_channel_send` = 13, `sys_channel_recv` = 14, `sys_wait` integration) **and message-borne handle transfer** are implemented. Internally a channel is a **pair of endpoint kobjects** (each `KObjectType::IpcChannel`), each owning its own receive ring + recv-waiter list and linked by a mutual `peer` pointer — "two endpoint handles, separate queues per direction" with one kobject per endpoint. (A single shared object can't be used: a handle→object pointer carries no per-handle tag to distinguish the two ends for the asymmetric routing.)

**Handle transfer** is **always move** (a sender that wants to keep a copy `sys_handle_duplicate`s first and sends the duplicate — there is no move/duplicate flag). The kernel pins the transferred objects "in flight" in the queued message slot and installs them into the **receiver's** table at `sys_channel_recv` (the receiver is `current` then), writing their receiver-side handle values into both the separate `handles` out-buffer and the in-message `handles[]`. The move is committed (the sender's handles closed) only after the message is queued, so a `WouldBlock`/`PeerClosed` send loses no capability; install is atomic-or-fail.

Deferred to **Phase 2**:

- **`Notification::PeerClosed`** delivery (the error half ships now; the multi-holder "every holder" delivery wants handle duplication + a holder registry).

Deferred to the **async-I/O slice**: `Block` / `BlockBounded` send modes and the `PendingOperation`-returning send. `NoBlock` ships now.

## Bulk data: companion memory objects

For payloads larger than 4 KiB, the convention is:

1. Sender creates a `MemoryObject` containing the bulk data via `sys_memory_create` or by using an existing memory object.
2. Sender includes the `MemoryObject` handle in `msg.handles[]`.
3. The `payload` of the IPC message contains a small descriptor: the memory object handle index, length, and any metadata.
4. Receiver maps the memory object via `sys_memory_map` (or reads it via `sys_io_submit` with a `Read` opcode).

Whether the memory object handle is moved (sender loses access) or duplicated (both retain access) is the sender's choice via the duplicate-before-include pattern.

The IPC channel itself is **not** a bulk data transport. It carries control messages and references to bulk data, not the bulk data itself.

## Endianness and alignment

All multi-byte fields are little-endian (native on x86_64 and aarch64). The `IpcMsg` is page-aligned; field alignments follow standard `#[repr(C)]` rules. No packing.

## Where to read more

- [IPC architecture](../architecture/ipc.md) — channel implementation, queueing, backpressure
- [Handle encoding](handle-encoding.md) — handle transfer mechanics
- [Resource server protocol wire format](rsproto-wire-format.md) — the higher-level protocol that rides on IPC messages
