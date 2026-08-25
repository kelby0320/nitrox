# Sessions and authentication

**Status:** implemented (Phase 3, "Auth + session-mgr" slice, 2026-07-20; last checked
2026-08-25). **`/svc/auth` is real as of M7 Part C** — the binding this document described
before 2026-08-21, found then to have never existed and removed, now exists. The paragraph
under "Credential validation" is the current shape; the history is kept because a doc that
quietly starts being right again teaches nobody why it was wrong. The full
path — login → authenticate → per-user namespace → sandboxed user shell → home write —
runs end to end. Living document; describes the architecture, with the build sequence
in the [implementation plan](../planning/implementation-plan.md). What remains is
deferred polish (roles, profile overlays, session tokens, the real Phase-4 shell) — see
Deferred.

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
(`docs/archive/os-design-v5.1.md`: "no UIDs, no GIDs, … no session IDs"). It provides
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
    service-mgr ─spawns→ auth-service        (no caps; a client channel, handed on — see below)
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
authority, no device access. session-mgr reaches it over an rsproto channel.

**That channel is resolved from a namespace, as of M7 Part C.** `init` spawns
`auth-service` and binds its forwarding endpoint at `/svc/auth`; a supervisor resolves that
path and gets a session channel of its own, minted per caller — the same shape
`profile-server` serves `/bin` and the tty server serves `/dev/tty` with. `session-mgr` does
this at startup, and so does `desktop-session-mgr` — once, not per attempt: the oracle's
lifetime is the machine's.

**Bound by `init`, not by the supervisor that used to spawn it.** `service-mgr` spawned
`auth-service` until Part C, and the resource-server protocol says the supervisor that starts a
server registers it — but a declared service is spawned with `namespace: 0`, an inherited
**LOOKUP-only** root, so it cannot bind into it. The bind was written in `service-mgr` first
and came back `FAIL`, which is how the constraint was found rather than deduced. init owns the
root namespace and already binds `/bin`, `/log`, `/dev/tty`, `/dev/input/new` and `/dev/draw`;
this is the sixth.

**What this replaced, and why it had to go.** `auth-service` created **one** channel pair at
startup and transferred the client end in its `Meta::Ready`, which `service-mgr` couriered to
`session-mgr` positionally alongside the fs, profile and tty endpoints — one server, one
client, by construction. A second supervisor could not reach the oracle at all, which is what
made this Milestone 7's problem rather than a tidiness question.

**The binding is unscoped among the processes that hold the root namespace**, and that is
tracked — `TODO(svc-auth-ungated)` in
[`deferred-decisions.md`](../rationale/deferred-decisions.md).

**That set does not include a user's shell**, and saying otherwise would contradict the whole
point of §Session construction below. A session gets a **constructed** namespace holding
`/home`, `/bin`, `/session/user`, `/dev/tty` and `/dev/console` and nothing else; a shell has no
root-namespace handle and cannot name `/svc/auth` at all. *Absence is the sandbox.* What can
reach the oracle is what is spawned with `namespace: 0` — init's children and `service-mgr`'s
declared services — which is exactly why `boot-probe` can test it.

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
- Session **tokens** and multi-session bookkeeping; logout / switch-user. **Concurrent logins
  are no longer deferred and no longer "one console, one session at a time"** — M7 Part D
  built the graphical column, and `session-mgr` and `desktop-session-mgr` each authenticate
  and run a session unaware of the other. Neither arbitrates and there is no registry, which
  is what keeps serial the recovery path by construction; `cargo xtask check-login` logs in
  on both in one boot and requires that neither session ended while the other started. The
  accepted cost is that the same user may be logged in twice with two namespaces
  ([graphical-session.md](../design/graphical-session.md) §6.2).
- User *creation* and password *change* (the DB is read-only); persisted per-user
  state beyond the seeded home directory.
- The real user shell (Phase 4).

## References

- `docs/archive/os-design-v5.1.md` §§ Session Manager / Authentication Service,
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
