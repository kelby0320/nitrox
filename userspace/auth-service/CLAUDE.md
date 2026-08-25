# userspace/auth-service/CLAUDE.md

Constraints for the authentication service. Loaded when working under
`userspace/auth-service/`.

## What this is

The credential **oracle** for the auth + session-mgr slice: it answers the `Auth`
rsproto category (`Authenticate { username, password } → { AUTHENTICATED, principal,
home } | DENIED`, `docs/spec/rsproto-auth-ops.md`) over a plain IPC channel. It
decides *who you are*; it does **not** construct namespaces or issue authority — that
is session-mgr's job. Keeping the two split keeps the password DB out of the
`BIND_NAMESPACE`-holding supervisor. See `docs/architecture/session-and-auth.md`.

**A namespace forwarder as of M7 Part C**, like fs-server / profile-server: `init` binds its
endpoint at `/svc/auth`, it answers `Namespace::Resolve` there, and each caller gets a session
channel of its own to send `Authenticate` on. It still holds **no** `BIND_NAMESPACE` and no
device access — binding is init's, and answering a resolve is not the same authority as making
one.

This paragraph said the opposite until 2026-08-25, and the reason it changed is worth keeping:
one channel pair minted at startup made it a **one-client** oracle by construction, which
`desktop-session-mgr` could not share. See `docs/architecture/session-and-auth.md`.

## Structure

- **`src/lib.rs` — the credential logic (host-tested).** Pure, `#![no_std]`,
  no-`alloc`: parse the `passwd`-style user DB and verify `(username, password)`
  against a stored PBKDF2 verifier (`authenticate` / `serve_authenticate`). No
  syscalls — the bin supplies the DB bytes + buffers. Host-tested against a DB built
  with the real KDF (`cargo xtask test` runs `-p auth-service --lib`).
- **`src/main.rs` — the server `[[bin]]`.** Bare-target `_start` + syscall plumbing
  only: read `/system/users` into a fixed buffer, create a client channel, send
  `Meta::Ready` handing the supervisor the client endpoint, then serve. **Alloc-free**
  — fixed `.bss` buffers, no `#[global_allocator]`.

## Rules

- **No `alloc`.** Fixed buffers (the DB is one page; messages are bounded). Do not add
  `#[global_allocator]` or `extern crate alloc`.
- **Never store or log a password.** The DB holds only one-way PBKDF2 verifiers
  (`libcrypto::password`); the request password is hashed and dropped. Don't `kprint`
  a password or a verifier.
- **No secrets in the source tree.** The demo credential is *seeded into the image by
  `tools/xtask`* from a fixture password (a build input) — the tree holds only the
  one-way verifier, on the ext4, never in source. Host tests derive their own values.
- **Deny safely.** A missing / unreadable DB must authenticate **no one** (the bin
  exits rather than serve an empty DB). An unknown user runs an equivalent dummy
  verify so it is timing- and shape-indistinguishable from a wrong password (no
  enumeration oracle) — keep it that way.
- **Constant-time verifier compare** (via `libcrypto::ct_eq`), never `==` on secrets.

## Forbidden

- `alloc` / `#[global_allocator]`.
- Storing, logging, or returning a plaintext password.
- Committing a password or verifier to the source tree (even in tests).
- Holding `BIND_NAMESPACE` or constructing namespaces. **Answering `Namespace::Resolve` is
  no longer forbidden** — M7 Part C made it a forwarder so two supervisors can each hold a
  session — but *binding* remains init's, and this server must never acquire the capability
  to bind its own path.
- Disclosing *why* a credential was denied (unknown user vs. wrong password).
