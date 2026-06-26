# Resource Server Protocol Wire Format

This document specifies the wire format of the resource server protocol — the binary protocol spoken by every userspace resource server over IPC. The protocol rides on top of the [IPC message format](ipc-message-format.md): each protocol message occupies the payload portion of an `IpcMsg`, with handles transferred via the message's handle list.

**Status:** Pre-stabilization. The envelope and Meta operations are committed; per-category operations (Stream, Block, Control, etc.) will be specified as their resource server implementations land.

## Envelope

Every protocol message begins with a `RsMsgHeader`:

```rust
#[repr(C)]
pub struct RsMsgHeader {
    pub magic:        u32,    // 0x52534D47 = "RSMG"; offset 0
    pub version:      u16,    // negotiated per-channel; offset 4
    pub op:           u16,    // operation discriminant; offset 6
    pub request_id:   u64,    // caller-chosen correlation id; offset 8
    pub flags:        u32,    // RsFlags bitfield; offset 16
    pub body_len:     u32,    // bytes of body following header; offset 20
    pub handle_count: u16,    // handles in IpcMsg.handles[]; offset 24
    pub _reserved:    u16,    // must be 0; offset 26
}
```

Total header size: 28 bytes. Aligned to 4 bytes.

The header is followed by `body_len` bytes of operation-specific body data, encoded as a packed C struct with fields specific to the operation. See per-operation specifications below.

### Magic value

`0x52534D47` reads as ASCII `"RSMG"` (Resource Server MesSaGe). Any IPC payload not beginning with this magic is rejected by both clients and servers as malformed. The magic catches channel confusion and helps diagnose mis-routed messages.

### Flags bitfield

```rust
bitflags! {
    pub struct RsFlags: u32 {
        const REPLY            = 1 << 0;  // this is a reply, not a request
        const ERROR            = 1 << 1;  // this is an error reply
        const LAST_IN_STREAM   = 1 << 2;  // last message in a streaming response
        // bits 3..31 reserved
    }
}
```

A reply has `REPLY` set and (optionally) `ERROR`. A streaming response has `REPLY` set on each message; the last message also has `LAST_IN_STREAM` set.

## Operation discriminant

The 16-bit `op` field decomposes:

```
 15            8 7              0
┌──────────────┬──────────────────┐
│ category (8) │  specific op (8) │
└──────────────┴──────────────────┘
```

| Category | Range | Meaning |
|---|---|---|
| `Meta` | `0x00xx` | Version handshake, capability query, lifecycle |
| `Namespace` | `0x01xx` | Lookup, enumerate, bind-forward |
| `Stream` | `0x02xx` | Read, write, seek (for streamable resources) |
| `Block` | `0x03xx` | Extent query, block allocate (filesystem-specific) |
| `Control` | `0x04xx` | Ioctl-style, opaque to the protocol |
| `Power` | `0x05xx` | Suspend, resume, device power |
| `File` | `0x06xx` | Positioned, stateless file-content reads (page-cache fill) |
| (reserved) | `0x07xx` – `0xFExx` | Future categories |
| `Vendor` | `0xFFxx` | Server-specific or experimental |

A resource server must implement at least the Meta category. Each server declares which other categories it supports via `Meta::QueryCaps`.

## Version negotiation

Every channel begins with a `Meta::Hello` exchange. Until the handshake completes, no other messages may be sent.

### Meta::Hello (`0x0000`)

Request body:
```rust
#[repr(C, packed)]
pub struct HelloRequest {
    pub client_min_version: u16,
    pub client_max_version: u16,
    pub flags: u32,
}
```

Reply body:
```rust
#[repr(C, packed)]
pub struct HelloReply {
    pub agreed_version: u16,    // chosen version, in client's [min, max]
    pub server_caps: u32,       // bitmask of supported categories
    pub server_flags: u32,
}
```

If no version overlap exists, the server replies with `RsFlags::ERROR` and an `ErrorBody` (see below). The channel is unusable thereafter.

The agreed version applies to all subsequent messages on the channel. `libos` caches the negotiated version so applications don't see it.

### Meta::Goodbye (`0x0001`)

Request body: empty (0 bytes).

Reply body: empty.

Either party may send `Goodbye` to indicate intent to close the channel. The receiver responds with a `Goodbye` reply, then both parties may close their channel handles. The control channel between a supervisor and its resource server typically uses this for clean shutdown.

### Meta::QueryCaps (`0x0002`)

Request body: empty.

Reply body:
```rust
#[repr(C, packed)]
pub struct QueryCapsReply {
    pub supported_categories: u32,  // bitmask: bit N set if category N supported
    pub server_name_len: u16,       // length of server name string
    pub server_version_len: u16,    // length of server version string
    // Followed by server_name (UTF-8) then server_version (UTF-8)
}
```

Returns the list of categories this server supports, plus identifying strings. Used for diagnostics and capability discovery.

### Meta::Ping (`0x0003`)

Request body: 8 bytes of opaque caller-chosen nonce.

Reply body: same 8 bytes echoed back.

Liveness check. Useful for the supervisor's health-check loop.

### Meta::Ready (`0x0004`)

Special: this is the message a resource server sends on its **control channel** (not its data channel) to its supervisor at startup, indicating that the resource server is ready to accept requests. Body:

```rust
#[repr(C, packed)]
pub struct ReadyMessage {
    pub server_name_len: u16,       // length of server name (informational)
    pub _reserved: u16,
    // Followed by server name (UTF-8)
    // The endpoint handle is in IpcMsg.handles[0]
}
```

The supervisor binds `handles[0]` into the appropriate namespace location after receiving this message. See [why-supervisor-registration.md](../rationale/why-supervisor-registration.md).

## Error replies

Any operation can produce an error reply by setting `RsFlags::ERROR`:

```rust
#[repr(C, packed)]
pub struct ErrorBody {
    pub kerror: i32,         // KError discriminant as i32
    pub server_code: u32,    // server-specific error code, 0 if not applicable
    pub msg_len: u16,        // length of optional UTF-8 human message
    pub _reserved: u16,
    // Followed by msg (UTF-8) of length msg_len
}
```

`kerror` aligns with the system-wide `KError` enum so callers can map errors uniformly. `server_code` lets servers report finer-grained errors when needed.

## Body encoding rules

Operation bodies are packed C-style structs (`#[repr(C, packed)]`). Fixed field offsets per operation. No schema in the message — the `op` is the schema discriminant.

Encoding rules:
- Multi-byte integers: little-endian
- Strings: length-prefixed UTF-8, no null terminator
- Arrays: length-prefixed, elements packed
- Optional fields: indicator byte (0 = absent, 1 = present) followed by the value if present

Each operation's body is documented per category. See:
- [Namespace operations spec](rsproto-namespace-ops.md)
- [File operations spec](rsproto-file-ops.md)
- [Stream operations spec](rsproto-stream-ops.md) (TBD when implemented)
- [Block operations spec](rsproto-block-ops.md) (TBD when implemented)

The Meta, Namespace, and File operations are specified; the rest land with their
consumers.

## Bulk data transfer

Three mechanisms, selected per-operation:

### Inline

For data ≤ ~3500 bytes (after subtracting header and operation-specific body fields). The data is in the `IpcMsg.payload` buffer following the `RsMsgHeader` and operation body.

Used by: small reads, lookup results, metadata responses, error messages.

### Companion MemoryObject

For data up to a few MiB. The client (or the server, depending on direction) allocates a `MemoryObject` of the required size, transfers it via `IpcMsg.handles[]`, and the operation body indicates "the data is in the attached memory object." The receiver maps or reads the memory object.

Used by: medium-size file reads, directory listings for large directories, log streaming.

### IoRing

For continuous high-volume streaming. The client establishes an `IoRing` via `sys_ring_create` and passes the ring handle in a setup message. Subsequent operations submit through the ring, bypassing per-message IPC. The server writes completions into the ring's completion queue.

Used by: high-throughput sockets, tail-style log following, streaming bulk reads.

The choice between mechanisms is per-operation and typically declared in the request body (e.g., a flag indicating "respond inline" vs. "respond via memory object"). Servers may refuse mechanisms they don't support and reply with an error.

## Schema evolution

The protocol versions per channel via `Meta::Hello`. The initial version is `1`. Rules for protocol evolution:

- **Adding a new operation** to an existing category: minor version bump. Older clients ignore the new operation (don't issue it); newer clients can use it.
- **Adding fields to the end of an existing operation's body**: minor version bump. Older clients parsing on the old version see only the original fields; newer clients see all fields.
- **Adding a new category**: minor version bump.
- **Changing existing field semantics or layout**: major version bump. Explicit migration; both old and new servers may need to be supported transitionally.
- **Removing operations**: major version bump.

Major version changes break compatibility. Minor version changes preserve it.

## Tooling

A `librsproto` crate provides client and server utilities:

```rust
// Server side
pub trait RsHandler {
    fn handle_meta(&mut self, op: u16, req: &[u8]) -> Result<Vec<u8>, ErrorBody>;
    fn handle_namespace(&mut self, op: u16, req: &[u8], handles: &[RawHandle]) -> ...;
    // ... per category
}

// Client side
pub struct RsClient {
    channel: Handle<IpcChannel, SendRecv>,
    version: u16,
}

impl RsClient {
    pub async fn hello(&mut self, min: u16, max: u16) -> Result<HelloReply>;
    pub async fn query_caps(&self) -> Result<QueryCapsReply>;
    pub async fn ping(&self, nonce: u64) -> Result<u64>;
    // ... per category
}
```

Application code uses the typed wrapper; it doesn't see wire format directly.

## Validation and security

- **Magic check:** any payload not beginning with `0x52534D47` is rejected. Catches channel confusion.
- **Body length validation:** `body_len` must match the actual body bytes following the header. Bodies extending past the IPC payload are rejected.
- **Operation validation:** the receiver verifies that `op` is recognized and that the body length matches the expected size for that operation. Unknown operations produce an error reply with `KError::Unsupported`.
- **Handle count validation:** `handle_count` in the header must match the actual handle count in `IpcMsg.header.handle_count`. Mismatch is treated as protocol error.
- **No pointers in messages:** all data is inline or via transferred handle. No user pointers cross the channel; the kernel's IPC payload copy semantics handle TOCTOU concerns.

## Endianness and alignment

All multi-byte integers little-endian. `RsMsgHeader` is 4-byte aligned within the IPC payload. Body alignment depends on the operation's struct definition; standard C alignment rules apply with `#[repr(C, packed)]` or `#[repr(C)]` as appropriate per operation.

## Where to read more

- [Resource server model architecture](../architecture/namespace-and-resource-servers.md)
- [IPC message format](ipc-message-format.md)
- [Why supervisor-mediated registration](../rationale/why-supervisor-registration.md)
