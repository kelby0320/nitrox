# Resource Server Protocol — Auth operations

The `Auth` category (`op = 0x08xx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). These operations let a client
ask an **authentication server** to validate a credential and, on success, learn
the caller's *principal* — the userspace identity used to construct a session. The
kernel knows nothing of users or credentials; this is a pure userspace
request/reply between two userspace processes (typically session-mgr → auth-service).

**Status:** Pre-stabilization. Introduced with the Auth + session-mgr slice
(`docs/architecture/session-and-auth.md`). `Authenticate` is the oracle's op. **The
management ops — `List`, `Add`, `Remove`, `SetPassword` — arrived with administration Part
D.1 (2026-09-25)**, on an admin session of their own (§ *Administration*). The view broker asks
`List` from Part D.2, to judge a policy, and fronts the rest for people from Part D.3.

## Why a dedicated category

Credential validation is a first-class, semantically-defined request/reply — not an
opaque `Control` (`0x04xx`) ioctl and not resource I/O. It has a stable contract
(`(username, password) → principal | deny`), so it gets its own category rather than
being tunnelled through another.

## Trust and transport

- The `password` field crosses the channel **in cleartext**. This is acceptable: an
  rsproto channel is a kernel-mediated local IPC endpoint (no network), and the
  client already holds the password to check it. The server hashes it (never stores
  or logs it) and the client should zero its copy after sending. **The server stores
  only a one-way verifier** (PBKDF2-HMAC-SHA256; see
  [session-and-auth.md](../architecture/session-and-auth.md)), never the password.
- A **denied** credential is a normal outcome, not a protocol error: it is a
  successful reply carrying `result = DENIED` (below), *not* an `RsFlags::ERROR`
  reply. `ERROR` is reserved for a malformed request or an internal server fault.
- To avoid a user-enumeration / timing oracle, the server performs an equivalent
  verification whether or not the username exists (a dummy verify for an unknown
  user) and returns the same `DENIED` reply either way.

## Authenticate (`op = 0x0800`)

Validate a `(username, password)` pair.

### Request body

```rust
#[repr(C, packed)]
pub struct AuthenticateRequest {
    pub username_len: u16,   // offset 0 — bytes of username that follow
    pub password_len: u16,   // offset 2 — bytes of password that follow
    // followed by `username` (UTF-8, username_len bytes),
    //   then      `password` (UTF-8, password_len bytes)
}
```

`handle_count = 0`. Body length = `4 + username_len + password_len`. Both lengths are
bounded by the server's configured maxima (a request exceeding them is answered
`DENIED`, not an error — an over-long field is a failed credential, not a fault).

### Reply body (success — the request was well-formed)

`RsFlags::REPLY` set, `RsFlags::ERROR` clear, `handle_count = 0`:

```rust
#[repr(C, packed)]
pub struct AuthenticateReply {
    pub result: u16,         // offset 0 — 1 = AUTHENTICATED, 0 = DENIED
    pub principal_len: u16,  // offset 2 — bytes of principal (0 if DENIED)
    pub home_len: u16,       // offset 4 — bytes of home path (0 if DENIED)
    pub _reserved: u16,      // offset 6 — must be 0
    // on AUTHENTICATED: `principal` (UTF-8, principal_len bytes),
    //   then            `home`      (UTF-8, home_len bytes; absolute path)
}
```

Body length = `8 + principal_len + home_len`. On `DENIED`, `principal_len =
home_len = 0` and body length is `8`.

| `result` | Value | Meaning |
|---|---|---|
| `AUTHENTICATED` | `1` | The credential is valid. `principal` names the canonical identity; `home` is the absolute path to the principal's home directory (the session's writable root). |
| `DENIED` | `0` | The credential is invalid (wrong password, unknown user, or a malformed/over-long field). No detail is returned — the reason is deliberately not disclosed. |

- **`principal`** is the canonical username the session is built for. It need not
  equal the request's `username` byte-for-byte (a server may canonicalise), but for
  this slice it does. There is **no** UID/GID — the principal is a string identity,
  scoped entirely to userspace.
- **`home`** is what the session manager binds as the writable session root (the
  `/home/<user>` subtree). The server, not the client, is authoritative for it.

### Error reply

An `RsFlags::ERROR` reply (per the [envelope spec](rsproto-wire-format.md)) is used
**only** for a request the server could not process as an authentication attempt at
all — a truncated/malformed body, or an internal fault. A wrong or unknown
credential is **not** an error; it is `result = DENIED`.

## Administration (`op = 0x0801`–`0x0804`, administration Part D.1)

**An admin session is a second kind of session**, opened by resolving `/svc/auth/admin` — the
suffix `admin` — where `/svc/auth` itself opens an oracle session. An admin session answers only
the four ops below; `Authenticate` there is refused `Unsupported`, and an admin op on an oracle
session is an error reply too. Any other suffix is `NotFound`, answered with the whole twelve-byte
`ErrorBody`.

**Who may open one.** The view broker, which fronts every account operation and applies the guards
(`administration.md` § *Part D in detail*), is the one client meant to. It holds one from Part
D.2, opened on first need, and asks `List` to learn which accounts exist when it judges a policy
([`rsproto-views-ops.md`](rsproto-views-ops.md) § `Check`). The session is resolved
from the root namespace, so **anything holding the root namespace can open one** — the boundary
`/svc/auth` has always had (`TODO(svc-auth-ungated)` in
[`deferred-decisions.md`](../rationale/deferred-decisions.md)). It adds no authority there: a root
holder can already map `/system/users` writable.

**A refusal is an error reply**, unlike `Authenticate`'s `DENIED`: the standard
[`ErrorBody`](rsproto-wire-format.md#error-replies), with a reason.

| Op | Request body | Reply body |
|---|---|---|
| `List` (`0x0801`) | empty | `count: u16`, then per account `name_len: u8`, the name, `home_len: u8`, the home, in the file's order |
| `Add` (`0x0802`) | a name and a password, laid out as `Authenticate`'s request, **accounted for exactly** | empty. The home is `/home/<name>`, not sent |
| `Remove` (`0x0803`) | the name's bytes | empty |
| `SetPassword` (`0x0804`) | a name and a password, as `Add`'s | empty. The record keeps its name, its place in the file and its home |

| Refusal | When |
|---|---|
| `InvalidArgument` | a name that is not 1 to 32 bytes of a lowercase letter or `_` then lowercase letters, digits, `_` or `-`; a password not 1 to 128 bytes; a body that does not account for itself exactly |
| `AlreadyExists` | `Add` of a name an account has |
| `NotFound` | `Remove` or `SetPassword` of a name no account has |
| `TooLarge` | the file would be larger than `libusers::MAX_FILE`, 4 KiB, the most the service loads at boot; or the account list does not fit in one message |
| `IoError` | the write did not hold; the reason names the step. The file and the service's copy are as they were |
| `Unsupported` | `Authenticate`, or any other op, on an admin session |

**A write is atomic.** The new file is written to `/system/users.new`, synced, then renamed over
`/system/users`. A leftover `users.new`, from a write that died, is cut to nothing first. The
service's copy in memory changes only once the rename has held. A new password gets a fresh
16-byte salt from the kernel's entropy source and `libcrypto`'s `DEFAULT_ITERATIONS`; each record
keeps its own count.

**The file's format is `libusers`'**, the crate `auth-service`, the build's seeder and `account`
all write it through.

## Deferred

- Roles / group membership in the reply (the principal is a bare identity today);
  role-to-capability mapping is a session-mgr/privilege-broker concern.
- Session *tokens* (a reusable post-login credential) — each login re-authenticates.
- `Meta::QueryCaps` advertising the `Auth` category bit — added when a client
  negotiates categories dynamically; today the session manager knows its
  auth-service statically.
