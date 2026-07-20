# Sessions and authentication

**Status:** in progress (Phase 3, "Auth + session-mgr" slice). Living document. This
describes the architecture; the build sequence lives in the
[implementation plan](../planning/implementation-plan.md).

This is how a human logs in and gets a running, *sandboxed* shell — the first time
Nitrox exercises its defining property end to end: **authority is constructed, not
assumed, and a sandbox is a namespace you were handed, not a set of permissions you
were denied.**

The user-visible outcome: log in, and get a shell whose namespace names exactly the
resources of that session — the user's home is writable, the system profile and
store are readable, and nothing else (no block devices, no other home, no raw
filesystem root) is even *nameable*.

## No user model in the kernel

The kernel knows nothing of users, sessions, passwords, or login
(`docs/history/os-design-v5.1.md`: "no UIDs, no GIDs, … no session IDs"). It provides
only mechanisms — handles ([`Rights`](handle-system.md)), ambient
[`SysCaps`](syscaps.md), and [namespaces](namespace-and-resource-servers.md). The
entire human-facing model is **policy in userspace**: who a principal is, what its
session contains, and what authority its processes hold are decided by userspace
supervisors composing those mechanisms.

## The cast

| Component | Role | Holds |
|---|---|---|
| **auth-service** | Credential oracle: "is this password right, and who is this?" | a read handle to the user DB; **no** `BIND_NAMESPACE` |
| **session-mgr** | Session supervisor: login, per-user namespace construction, user-shell lifecycle | `BIND_NAMESPACE` (re-delegated from service-mgr); the endpoint handles it composes sessions from |
| **user shell** | The session leaf: the process the human actually drives | only what its session namespace + (empty) syscaps grant |
| **`libcrypto`** | Shared hand-rolled crypto (SHA-256 / HMAC / PBKDF2) | — (a pure library) |

Splitting auth-service from session-mgr is deliberate and matches v5.1's role table:
**credential validation** (who are you) is separate from **session lifecycle +
namespace construction** (what authority you get). The password database lives with
the oracle, never in the `BIND_NAMESPACE`-holding supervisor; the supervisor never
sees a plaintext password except to forward it once to the oracle.

## The authority chain

```
kernel ─spawns→ init (full SysCaps)
  init ─spawns, delegates BIND_NAMESPACE→ service-mgr
    service-mgr ─spawns→ auth-service        (no caps; endpoint bound at /svc/auth)
    service-mgr ─spawns, re-delegates BIND_NAMESPACE→ session-mgr
       │   (+ a channel to auth, + the fs-server & console endpoints to compose sessions)
       │
       ▼   on a successful login:
    session-mgr ─sys_ns_create + attenuated sys_ns_bind→ a fresh session namespace
    session-mgr ─spawns, empty SysCaps, SpawnArgs.namespace = session ns→ user shell
                                              │
                                              ▼
                        user shell writes /home/<user>/<file>   (fs-server RW)
```

Every arrow only ever *attenuates* authority. `BIND_NAMESPACE` is concentrated in the
three supervisors (init, service-mgr, session-mgr — the v5.1 concentration) and
reaches no leaf. The user shell holds **empty** syscaps and a namespace that names
only its session's resources, so it cannot *name* `/dev/blk` or another user's home —
there is nothing to deny.

This is the same supervisor-mediated shape used everywhere else in the system
([why-supervisor-registration](../rationale/why-supervisor-registration.md)): a leaf
never constructs its own authority; a supervisor holding the relevant capability
constructs it and hands down an attenuated view.

## Credential validation

auth-service is an ordinary userspace resource server that answers one question. It
holds a read handle to the user DB and nothing else — no namespace-construction
authority, no device access. session-mgr reaches it over an rsproto channel (its
endpoint is bound at `/svc/auth`).

The exchange is the `Auth` category of the resource-server protocol —
`Authenticate { username, password } → { AUTHENTICATED, principal, home } | DENIED` —
specified in [rsproto-auth-ops.md](../spec/rsproto-auth-ops.md). Auth is a
first-class rsproto category (not an opaque `Control` ioctl) precisely because it is
a stable, semantically-defined contract.

Design properties (the spec is normative):

- **The server stores only a one-way verifier**, never a password: a
  PBKDF2-HMAC-SHA256 derivation over a per-record salt + iteration count. It verifies
  a candidate with a constant-time comparison.
- **A denied credential is a normal reply, not an error** — the protocol worked; the
  answer was "no." Errors are reserved for malformed requests.
- **No enumeration oracle**: an unknown user and a wrong password are
  indistinguishable — same reply, equivalent work (a dummy verify for the unknown
  user).
- **No kernel identity leaks in**: the returned `principal` is a userspace string
  identity and `home` is a path; there is no UID.

### The password primitive

`libcrypto` is the shared, hand-rolled crypto behind this: SHA-256, HMAC-SHA256, and
PBKDF2-HMAC-SHA256, plus a constant-time compare — `#![no_std]`, no `alloc`,
`core`-only, no dependencies, following the `kernel/src/libkern/chacha.rs`
"hand-rolled crypto, no external crates" precedent
([libcrypto CLAUDE.md](../../userspace/libcrypto/CLAUDE.md)). Because it is pure
`core` it is the *same* code on two sides of a trust boundary: the on-target
auth-service verifies with it, and host tooling seeds the DB's verifiers with it, so
the two agree by construction rather than by convention.

PBKDF2 is the recognised standard, its iteration count is a stored, tunable cost, and
it has published test vectors — the only basis on which hand-rolled crypto is
trustworthy. The same primitive is intended to serve the future audit subsystem's
hash-chained tamper-evident records ("build the hash once").

### The user database

A read-only credential store — one record per principal: a salt, an iteration count,
the one-way verifier, and the principal's home path. It is not user-facing
configuration (so it is not TOML), and it contains **no plaintext secret**: the
stored verifier is one-way, and it is populated by the build tooling from a build
input, never committed to the source tree (the "no embedded secrets" rule,
`userspace/CLAUDE.md`).

## Session construction — subtree-scoped namespaces

The heart of the slice. On a successful login, session-mgr builds a **fresh
namespace** for the session (`sys_ns_create`) and binds into it exactly the view the
session should have (`sys_ns_bind`, each with attenuated rights):

| Bound path | Target | Rights |
|---|---|---|
| `/dev/console` | the console device node | READ (the shell's I/O) |
| `/home` | the fs-server endpoint, **scoped to the user's home subtree** | LOOKUP · READ · WRITE · MAP_READ · MAP_WRITE |
| `/bin` | the system profile endpoint | read-only (program names resolve) |
| `/store` | the store | read-only (shared artifacts) |

Deliberately **absent**: `/dev/blk`, other users' homes, admin resources, the raw
filesystem root. *Absence is the sandbox* — this is Nitrox's "sandboxing by namespace
construction, not permission denial." The user shell is then spawned with this
namespace (`SpawnArgs.namespace`; the child receives a LOOKUP-only handle to it) and
**empty `SysCaps`** — a fully unprivileged leaf.

### Subtree scoping

A namespace binding resolves a path by longest-prefix match, yielding a covering
binding and the remaining **suffix**; for a resource-server target the suffix is
forwarded to the server ([namespace-and-resource-servers](namespace-and-resource-servers.md)).
A plain binding therefore exposes the server's *whole* tree from its root — too much
for a home directory.

**Subtree scoping** attaches a *base path* to a server binding: the server is asked
to resolve `base + suffix` instead of the bare suffix. session-mgr binds the
fs-server at the session's `/home` with base `/home/<user>`, so a lookup of
`/home/notes.txt` reaches the fs-server as `/home/<user>/notes.txt`, and nothing above
`/home/<user>` is nameable through that binding. This is v5.1's "subtree handle scoped
to `/home/alice`," and it is what makes the writable home a genuine sandbox boundary
rather than a naming convention.

Path components that could escape the subtree — `..`, `.`, empty segments — are
rejected at the resolution boundary, so a server always receives an already-safe path
under its base. The wire protocol is unchanged: the server still receives one
absolute path and resolves it, unaware a base was prepended. (The mechanism is a
property of the namespace object; see
[namespace-and-resource-servers](namespace-and-resource-servers.md).)

### Where the building-block endpoints come from

Constructing a namespace means binding *endpoint handles*, so the process that
composes per-user views must hold those handles. session-mgr receives them by
delegation: the fs-server forwarding endpoint (init holds it from the mount and hands
it down through service-mgr) and the console node. This is the "supervisors hold
resource-server endpoints and compose namespaces from them" model made concrete — the
counterpart, for namespace *construction*, of the supervisor-mediated *registration*
every resource server already relies on.

## The user shell

The process the human drives. In the introducing slice it is an explicit
**throwaway** — the real shell arrives in Phase 4 — whose only job is to demonstrate
that the constructed session works: it runs in the session namespace, writes to and
reads back a file under `/home`, and cannot reach anything outside its namespace (a
lookup of `/dev/blk` simply fails — the name is not bound). It is intentionally
minimal and disposable.

The interactive entry point of a healthy system is session-mgr's `login:` prompt on
the console; `eshell` reverts to what its name means — the **emergency** shell a
supervisor drops to on a critical-path failure — no longer the normal console.

## Deferred

- Roles / role-to-capability mapping beyond a single principal; a privilege broker
  for escalation (v5.1: "escalation is handle acquisition," not a state change).
- Per-user **profile overlays** (a user profile layered over the system profile) —
  designed in [profiles-and-namespace-projection](profiles-and-namespace-projection.md);
  a session binds the system `/bin` only.
- Session **tokens** and multi-session bookkeeping; logout / switch-user; concurrent
  logins (one console, one session at a time).
- User *creation* and password *change* (the DB is read-only); persisted per-user
  state beyond the seeded home directory.
- The real user shell (Phase 4).

## References

- `docs/history/os-design-v5.1.md` §§ Session Manager / Authentication Service,
  "Capability Bootstrap", "Policy vs. Mechanism," boot step 14.
- [rsproto-auth-ops](../spec/rsproto-auth-ops.md) — the `Authenticate` wire contract.
- [syscaps](syscaps.md) — `BIND_NAMESPACE` delegation, `child = parent & args`.
- [namespace-and-resource-servers](namespace-and-resource-servers.md) — binding +
  suffix forwarding, extended here with subtree scoping.
- [service-manager](service-manager.md) — the supervisor that spawns this slice's
  processes and re-delegates `BIND_NAMESPACE`.
- [profiles-and-namespace-projection](profiles-and-namespace-projection.md) — the
  `/bin` projection and the deferred per-user overlay.
- [process-spawn-args](../spec/process-spawn-args.md) — `SpawnArgs.namespace` +
  `.syscaps`.
