# IPC

Inter-process communication in Nitrox is **message passing over capability
endpoints**, not shared memory and not Unix pipes. A channel is a pair of
**endpoints**; a send on one endpoint enqueues a fixed-size message into the
other's receive queue, and the receiver drains it with `sys_channel_recv`.
Endpoints are kernel objects reached through handles, so they carry rights and
can (eventually) be transferred across the channel itself — the mechanism by
which capabilities propagate between processes. The wire envelope (`IpcMsg`: a
one-page, 4096-byte record — 24-byte header, 4008-byte inline payload, 8-entry
transfer-handle array) is specified in `docs/spec/ipc-message-format.md`.

This is the backbone for everything above the kernel: resource servers, the
namespace, and process spawn (which hands a child its initial endpoints *through*
IPC) all ride on it — which is why IPC lands before spawn.

## The endpoint pair

A channel is **two `IpcChannel` endpoint kobjects** (both
`KObjectType::IpcChannel`), each owning its own receive ring and recv-waiter
list, linked by a mutual `peer` pointer. `sys_channel_create` builds the pair and
returns one handle per endpoint, each with full rights
(`SEND | RECV | DUPLICATE | TRANSFER | INSPECT | WAIT`); the creator attenuates
before handing an endpoint to another party.

The spec describes "two endpoint handles, separate queues per direction"; this is
exactly that, with **one kobject per endpoint**. A single shared object cannot be
used because a handle→object pointer carries no per-handle tag to tell the two
ends apart, and the routing is asymmetric:

- **send on endpoint S** → push into **S's peer's** receive ring; wake the peer's
  blocked receivers. `WouldBlock` if the ring is full (`NoBlock`); `PeerClosed`
  if the peer has gone.
- **recv on endpoint R** → pop from **R's own** receive ring. `WouldBlock` if
  empty and the peer is open; `PeerClosed` if empty and the peer has closed.

Each endpoint's receive ring is a fixed-capacity ring buffer (depth chosen at
create, default 16) of message slots, pre-allocated up front — so send/recv move
a message with one in-place copy, never reallocating or shifting. All endpoint
state lives under the single rank-1 scheduler lock for Phase 1 (single-CPU); the
user-memory copies happen *outside* the lock (a faulting copy must never run
under it). See `kernel/docs/lock-ordering.md`.

## Waitable

An endpoint is `sys_wait`-able: it signals when its receive ring is non-empty
**or** its peer has closed (so a blocked receiver always wakes — to a message or
to `PeerClosed`). This reuses the same wait-queue machinery as `Timer` and
`NotificationChannel` — `sys_wait` dispatches the waitable operations by the
kobject type at the object header. The async model holds: a receiver that finds
`WouldBlock` blocks in `sys_wait` on the endpoint, never inside `sys_channel_recv`.

## Dead peer

When an endpoint's last handle closes, its destructor (under the scheduler lock)
nulls the surviving peer's back-pointer and wakes the peer's blocked receivers.
The survivor's subsequent operations then observe the closed peer: `recv` drains
any already-queued messages first, then returns `PeerClosed`; `send` returns
`PeerClosed` immediately. The two endpoints are freed independently; the
second-to-close sees its own peer pointer already null and does nothing — no
use-after-free, because the survivor is always alive (pinned by its handle or a
blocked waiter's reference) at the moment the first one nulls it.

## Status (this slice)

- **Implemented:** `IpcChannel` endpoint pairs, `sys_channel_create` /
  `sys_channel_send` / `sys_channel_recv`, `sys_wait` over an endpoint, and the
  dead-peer **error** path.
- **Deferred to process spawn:** **handle transfer** (the `handles[]` array /
  move + duplicate paths — their value is cross-process capability propagation,
  which needs a second process). The send/recv ABI keeps the `handles`/`count`
  parameters; `count` must be `0` for now. Also deferred: the async
  **`Notification::PeerClosed`** (the error half ships now; delivering the
  notification needs the channel→peer-process-notification-channel link).
- **Deferred to the async-I/O slice:** `Block` / `BlockBounded` send modes and
  the `PendingOperation`-returning send. `NoBlock` ships now (a bidirectional
  endpoint cannot express both a readable and a writable wait edge through
  `sys_wait`'s single signaled bit; blocking send wants a `PendingOperation`).
- **Demo:** `hello` creates a channel, holds both endpoints, sends a message to
  itself end0→end1, blocks on end1 via `sys_wait`, receives and verifies it,
  observes `WouldBlock` on an empty endpoint, then closes one end and observes
  `PeerClosed` on the other — a full ring-3 round-trip. (Cross-process IPC
  arrives with spawn.)
