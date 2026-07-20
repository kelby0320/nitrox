# userspace/libcrypto/CLAUDE.md

Constraints for the hand-rolled crypto primitives. Loaded when working under
`userspace/libcrypto/`.

## What this is

`no_std`, no-`alloc`, `core`-only, **no dependencies** cryptographic primitives —
SHA-256, HMAC-SHA256, PBKDF2-HMAC-SHA256, plus a `password` helper and a
constant-time compare. Built for the auth-service's password verification (and,
later, the audit subsystem's hash-chained records — "build the hash once, share
it"). Follows the `kernel/src/libkern/chacha.rs` precedent: **the project forbids
external crates, so crypto is hand-rolled** (`kernel/CLAUDE.md`, `userspace/CLAUDE.md`).

See `docs/architecture/session-and-auth.md`.

## Rules

- **No external crates. No `alloc`.** Everything works on caller-provided slices and
  fixed stack buffers. This is load-bearing: the crate must also compile as pure
  host `core` so `tools/xtask` can link it to seed image password hashes with the
  *exact* code the on-target verifier runs. Pulling in `alloc` or a dependency
  breaks that and violates the no-crates rule.
- **`#![cfg_attr(not(test), no_std)]`.** Under `cargo test` it builds as host `std`
  so the published vectors run on the host; on target it is `no_std`.
- **Every routine is verified against a published test vector** (NIST FIPS 180-4 for
  SHA-256, RFC 4231 for HMAC, RFC 7914 / RFC 6070-style for PBKDF2). A new primitive
  without a standard vector does not land — hand-rolled crypto is only trustworthy
  against known-answer tests. Add the vector in a `#[cfg(test)]` module.
- **No secrets in the source tree** — not even in tests. Tests use published vectors
  or locally-derived values, never a real password/hash (`userspace/CLAUDE.md`
  forbidden patterns).
- **Constant-time comparison for secrets.** Compare verifiers/MACs with `ct_eq`,
  never `==` on the raw bytes, so verification leaks no timing.
- **Don't add protocol or policy here.** This crate is primitives only. The user-DB
  format, the auth wire protocol, and the KDF *cost policy's* home live in the
  auth-service, not here (the `DEFAULT_ITERATIONS` constant is a convenience default,
  not the source of truth — the cost is stored per record).

## Testing

Host tests only (`cargo xtask test` runs `-p libcrypto`). Keep the known-answer
vectors; add one for any new primitive.
