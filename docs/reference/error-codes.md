# Error Codes

The complete `KError` set: every negative value a syscall or a resource-server reply can
carry, what each means, and who produces it.

**Status:** Pre-stabilization. Values are the contract *within* this period — once
mirrored, a value never changes meaning; new variants take new values.

Canonical source: `kernel/src/syscall/error.rs`. Mirrored by hand in
`userspace/libkern/src/error.rs` and checked by `cargo xtask abi-sync-check`. This page is
the human-readable catalogue the [syscall ABI spec](../spec/syscall-abi.md) refers to; if
it and the source disagree, the source wins and this page is wrong.

## Read this before assigning a new value

**The v5.1 design document's numbering is not the implemented numbering, and the two
disagree in the range most likely to be reached for.** `docs/archive/os-design-v5.1.md`
reserved `-11` for `AlreadyExists` and placed the blocking errors at `-20..-23`. The
implementation instead put `WouldBlock`/`TimedOut`/`PeerClosed` at `-11..-13`, so when
`AlreadyExists` was finally needed (2026-07-30) its documented slot was long since
occupied and it took `-14`.

The design doc is history, not a reservation table. **This page is the reservation
table.** Take the next free value in the appropriate block; do not reuse one because a
document from before the implementation said so.

## The set

Values are grouped by kind. The grouping is a readability convention, not something any
code depends on — nothing computes a category from a range.

### Handles and resources

| Value | Name | Meaning |
|---|---|---|
| `-1` | `InvalidHandle` | The supplied handle is not live in the caller's table. |
| `-2` | `NoAccess` | The handle is live but lacks a right the operation requires. |
| `-3` | `OutOfHandles` | The caller's handle table is full. |
| `-4` | `OutOfMemory` | The kernel heap is exhausted. |

### Naming, waiting, and existence

| Value | Name | Meaning |
|---|---|---|
| `-10` | `NotFound` | A named resource does not exist. |
| `-11` | `WouldBlock` | A non-blocking operation could not complete immediately — `sys_wait` with `deadline == 0` found nothing signaled. |
| `-12` | `TimedOut` | A deadline elapsed before the operation completed. |
| `-13` | `PeerClosed` | An IPC channel's peer endpoint has closed; no further traffic is possible on this endpoint. |
| `-14` | `AlreadyExists` | The name the operation would create is already taken. |
| `-15` | `NotEmpty` | A container still has members and the operation requires it to be empty. |

### Arguments

| Value | Name | Meaning |
|---|---|---|
| `-30` | `InvalidArgument` | An argument was malformed or out of range. |
| `-31` | `FaultFromUser` | A user buffer was inaccessible — bad address, or a fault mid-copy. |
| `-32` | `TooLarge` | A length or size exceeded the permitted maximum. |

### Devices

| Value | Name | Meaning |
|---|---|---|
| `-40` | `IoError` | A device or medium error — an ATA task-file error, a structurally malformed filesystem. |

### Everything else

| Value | Name | Meaning |
|---|---|---|
| `-52` | `Unsupported` | The operation is not implemented. |
| `-255` | `KernelError` | An unexpected internal condition, **and** the value an unrecognised code decodes to (see below). |

## `AlreadyExists` and `NotEmpty`: why they are kernel errors

Both look like filesystem concepts, and a reasonable first instinct is that the kernel —
which has no filesystem — should not carry them. The kernel does need them:

- `sys_ns_bind` onto an occupied path is `NsError::AlreadyBound`. Until 2026-07-30 it
  shared `InvalidArgument` with a malformed path, so a supervisor could not tell "that
  name is taken" from "that name is nonsense".
- `fs-server-ext4` had the identical collapse: `FsError::Exists` and `FsError::NotEmpty`
  both became `InvalidArgument`.

Two independent components needing the same distinction is what makes it a system-wide
error rather than a server-specific one. Which is also the rule for the next such
question:

> **A `KError` is for a condition more than one component can produce and any client can
> act on. Everything else belongs in `server_code`.**

The rsproto `ErrorBody` carries `kerror: i32` *and* `server_code: u32`
([wire format](../spec/rsproto-wire-format.md)) precisely so a server can be more
specific without spending a system-wide discriminant. `FsError::Corrupt` is the standing
example: it shares `IoError` with a genuine device failure because no client can act on
the difference, and the distinction — real, and useful in a log — is what `server_code`
is there to carry when something finally consumes it.

## Decoding, and the trap in it

`libkern::KError::from_i32` maps an unrecognised value to `KernelError` rather than
failing. That is deliberate forward-compatibility: a kernel newer than the `libkern` a
process was built against may return an error that build cannot name, and decoding it as
"something went wrong internally" beats a panic.

The cost is that a **missing** arm is indistinguishable at runtime from an unknown code.
`IoError` was in both enums with matching discriminants and had no arm in `from_i32` from
2026-06 until 2026-07-30: every device error silently decoded as `KernelError`,
`abi-sync-check` passed (the *enums* agreed), and the round-trip test in `error.rs` missed
it because that test enumerates variants by hand as well.

`abi-sync-check` now derives the expected arm set from the kernel's enum and fails on any
kernel variant `from_i32` cannot decode. The `_` arm's own target — `KernelError` — is
exempt, since an unlisted value already reaches it.

**So: adding a variant means editing three places** — the kernel enum, the `libkern`
enum, and `from_i32`. The guard catches the third; nothing but review catches a variant
added to `libkern` alone, which is why the kernel is the canonical side.

## Where these are used

- **Syscall returns.** A negative `isize` is a discriminant, sign-extended. See
  [syscall-abi.md](../spec/syscall-abi.md) § Return values.
- **`IoResult.status`.** A completed `PendingOperation` carries `0` or a negative
  discriminant.
- **rsproto error replies.** `ErrorBody.kerror`, alongside the optional `server_code`.
  Resource servers map their internal error types onto this set — see `fs_kerror` in
  `userspace/fs-server-ext4/src/main.rs` for the reference mapping.
- **`libos::ErrorKind`.** A coarse classification over these, named to match
  `std::io::ErrorKind` so a future `std` facade maps straight onto it.

## Where to read more

- [Syscall ABI](../spec/syscall-abi.md) — the return-value encoding
- [rsproto wire format](../spec/rsproto-wire-format.md) — `ErrorBody`, `server_code`
- `kernel/src/syscall/error.rs` — the canonical definition
