# Notification Format Specification

This document specifies the wire format of `Notification` values delivered through `sys_notif_recv`. The format is fixed-size (64 bytes per notification) with a `#[repr(C, u32)]` discriminant and category-organized variant numbering.

**Status:** Pre-stabilization. Variant additions are forward-compatible; structural changes are not.

## Wire format

Every notification is exactly 64 bytes on the wire:

```
 0                   4                                              63
┌────────────────────┬─────────────────────────────────────────────┐
│  discriminant (u32)│              variant body (60 bytes)        │
└────────────────────┴─────────────────────────────────────────────┘
```

Layout in Rust:

```rust
#[repr(C, u32)]
pub enum Notification {
    Unknown { kind: u32, _reserved: [u8; 60] } = 0,
    // ... variants
}
```

The fixed 64-byte size means sender and receiver don't need matching size knowledge — short variants are zero-padded. The `Unknown` variant's `_reserved` field sets the envelope size; all other variants must fit within the 60-byte body region.

## Discriminant numbering

Discriminants are 32-bit, organized into 256-value ranges per category:

| Range | Category | Purpose |
|---|---|---|
| `0x0000` – `0x00FF` | Reserved | `Unknown` (forward-compat fallback) |
| `0x0100` – `0x01FF` | Hardware exceptions | Faults caused by user code |
| `0x0200` – `0x02FF` | Process lifecycle | Child exits, peer disconnects |
| `0x0300` – `0x03FF` | External | Termination requests, user-initiated events |
| `0x0400` – `0x04FF` | Resource | Handle invalidation, resource-related events |
| `0x0500` – `0x05FF` | Power and system | Power events, memory pressure |
| `0x0600` – `0xFEFF` | Reserved | Future categories |
| `0xFF00` – `0xFFFF` | Vendor | Project-specific or experimental |

Adding a new variant within an existing category uses a new value in that category's range. Adding a new category uses a fresh range.

## Variant catalog

### `Unknown` (`0x0000`)

Forward-compatibility fallback. Returned by `sys_notif_recv` when the kernel queued a notification with a discriminant the userspace doesn't recognize at translation time.

```rust
Unknown {
    kind: u32,           // the original discriminant
    _reserved: [u8; 60], // padding to 64 bytes
}
```

The kernel's translation at copy time substitutes `Unknown { kind: <original> }` for any notification whose recipient process was built against a kernel ABI that doesn't include the variant. This allows old userspace to coexist with new kernels without crashing.

### Hardware Exceptions (`0x0100` range)

Delivered when a thread in the process faults. The thread is suspended pending a `sys_exception_resume` call.

```rust
SegFault {
    thread: ThreadId,    // 4 bytes
    addr:   VAddr,       // 8 bytes; faulting address
    kind:   FaultKind,   // 4 bytes; see below
    _pad:   [u8; 44],
} = 0x0100,

IllegalInsn {
    thread: ThreadId,
    addr:   VAddr,       // PC at fault
    _pad:   [u8; 48],
} = 0x0101,

DivideByZero {
    thread: ThreadId,
    addr:   VAddr,
    _pad:   [u8; 48],
} = 0x0102,

StackOverflow {
    thread: ThreadId,
    _pad:   [u8; 56],
} = 0x0103,
```

`FaultKind`:
```rust
#[repr(u32)]
pub enum FaultKind {
    NotMapped       = 0,  // page not present
    NotReadable     = 1,
    NotWritable     = 2,
    NotExecutable   = 3,  // SMEP-style violation
    Misaligned      = 4,
    UnknownFault    = 0xFFFF_FFFF,
}
```

### Process Lifecycle (`0x0200` range)

```rust
ChildExited {
    child:  ProcessId,   // 4 bytes
    status: ExitStatus,  // 8 bytes (kind: u32, code: i32)
    _pad:   [u8; 48],
} = 0x0200,

PeerClosed {
    handle: RawHandle,   // 8 bytes; the channel handle whose peer closed
    _pad:   [u8; 52],
} = 0x0201,
```

`ExitStatus`:
```rust
#[repr(C)]
pub struct ExitStatus {
    pub kind: ExitKind,  // 4 bytes
    pub code: i32,       // 4 bytes
}

#[repr(u32)]
pub enum ExitKind {
    Normal  = 0,  // exit code in `code`
    Killed  = 1,  // termination signal-equivalent in `code`
    Crashed = 2,  // fault kind in `code`
}
```

### External (`0x0300` range)

```rust
TermRequest = 0x0300,
// Body is all zero padding (60 bytes).
```

Delivered when a privileged party (typically the service manager or session manager) requests cooperative termination.

### Resource (`0x0400` range)

```rust
HandleInvalidated {
    handle: RawHandle,   // 8 bytes; handle that became invalid
    _pad:   [u8; 52],
} = 0x0400,

NotificationsDropped {
    count:  u32,         // 4 bytes; number of notifications dropped due to overflow
    _pad:   [u8; 56],
} = 0x0401,
```

`HandleInvalidated` is delivered when a handle becomes invalid through some action other than the process's own close — e.g., the underlying object was destroyed by another holder, or the handle was revoked.

`NotificationsDropped` is delivered when the per-process notification queue overflowed and dropped notifications. The count is approximate (the kernel may coalesce multiple drops into a single notification).

### Power and System (`0x0500` range)

```rust
PowerEvent {
    kind: PowerEventKind,  // 4 bytes
    _pad: [u8; 56],
} = 0x0500,

MemoryPressure {
    level:      PressureLevel,  // 1 byte
    _pad1:      [u8; 7],         // alignment
    free_pages: u64,             // 8 bytes
    _pad2:      [u8; 44],
} = 0x0501,
```

`PowerEventKind`:
```rust
#[repr(u32)]
pub enum PowerEventKind {
    BatteryLow      = 0,
    BatteryCritical = 1,
    AcConnected     = 2,
    AcDisconnected  = 3,
    LidOpen         = 4,
    LidClose        = 5,
    PowerButton     = 6,
    SleepButton     = 7,
    ThermalAlert    = 8,
}
```

Power events are delivered only to the registered power management daemon (deferred to Phase 2 ACPI).

`PressureLevel`:
```rust
#[repr(u8)]
pub enum PressureLevel {
    Low      = 1,  // reclaim is starting to work hard
    Medium   = 2,  // reclaim can't keep up
    Critical = 3,  // allocation failures imminent
}
```

`MemoryPressure` is delivered only to the registered OOM daemon.

## Queue capacity and overflow

The per-process notification queue has a default capacity of 64 entries. When the queue is full at delivery time:

- **Exception variants** (`0x0100` range) evict the oldest non-exception entry and enqueue the new exception. This ensures fault information is preserved even under pressure.
- **Other variants** are dropped, and the queue's overflow counter is incremented. The next `sys_notif_recv` returns a synthetic `NotificationsDropped { count }` notification.

The capacity is tunable per-process via spawn flags (deferred — not in initial implementation).

## Receive semantics

```rust
fn sys_notif_recv(queue: RawHandle, out: UserMutPtr<Notification>) -> isize
```

Behavior:
- If the queue is non-empty, copies the oldest notification to `*out`, removes it from the queue, returns `0`.
- If the queue is empty, returns `WouldBlock`. The caller should add the queue handle to a `sys_wait` list to block.
- The queue handle is waitable; it signals when the queue transitions from empty to non-empty.

The notification structure is copied byte-for-byte from kernel memory to user memory via the standard copy-to-user discipline. Discriminant-based variant decoding happens entirely in userspace after the copy.

## Forward-compatibility translation

When the kernel queues a notification with discriminant `D` in process P's queue, and P was built against a kernel ABI version that doesn't include `D`, the kernel translates at copy time:

```rust
fn translate_for_recipient(notif: Notification, recipient_abi: AbiVersion) -> Notification {
    if recipient_abi.knows_discriminant(notif.discriminant()) {
        notif
    } else {
        Notification::Unknown {
            kind: notif.discriminant(),
            _reserved: [0; 60],
        }
    }
}
```

The translation is transparent to userspace. Old code receiving an unknown discriminant sees `Unknown` and can ignore it without UB.

The recipient ABI version is recorded at process spawn time based on the linked `libkern` version. (Implementation detail: in the initial scope, all processes are assumed to be built against the current kernel ABI; the translation is a no-op. The mechanism is reserved for future use.)

## Endianness

All multi-byte fields are little-endian on x86_64 (native) and aarch64 (native little-endian assumed).

## Field alignment and packing

Variant bodies are not packed. Compiler alignment rules apply:
- `u32`, `i32`, `f32`: 4-byte aligned
- `u64`, `i64`, `f64`, `RawHandle`, `VAddr`: 8-byte aligned
- Larger fixed-size arrays: byte-aligned

The `_pad` fields ensure each variant is exactly 64 bytes total. Specifying explicit padding is required because Rust's default enum layout doesn't guarantee fixed-size variants without it.

## Where to read more

- [Notification queue architecture](../architecture/notifications.md) — implementation, delivery, exception priority chain
- [Why no signals](../rationale/why-no-signals.md) — design rationale
- [Process model architecture](../architecture/process-model.md) — `ChildExited` semantics, reaping
