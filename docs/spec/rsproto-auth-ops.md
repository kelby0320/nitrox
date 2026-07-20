# Resource Server Protocol — Auth operations

The `Auth` category (`op = 0x08xx`) of the resource-server protocol
([rsproto-wire-format.md](rsproto-wire-format.md)). These operations let a client
ask an **authentication server** to validate a credential and, on success, learn
the caller's *principal* — the userspace identity used to construct a session. The
kernel knows nothing of users or credentials; this is a pure userspace
request/reply between two userspace processes (typically session-mgr → auth-service).

**Status:** Pre-stabilization. Introduced with the Auth + session-mgr slice
(`docs/architecture/session-and-auth.md`). Only `Authenticate` is defined;
credential *management* ops (add/remove user, change password) are deferred with
their consumers.

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

## Deferred

- User *management* ops (create/delete user, change password) — the DB is read-only
  in the introducing slice.
- Roles / group membership in the reply (the principal is a bare identity today);
  role-to-capability mapping is a session-mgr/privilege-broker concern.
- Session *tokens* (a reusable post-login credential) — each login re-authenticates.
- `Meta::QueryCaps` advertising the `Auth` category bit — added when a client
  negotiates categories dynamically; today the session manager knows its
  auth-service statically.
