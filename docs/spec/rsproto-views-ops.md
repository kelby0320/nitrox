# rsproto — Views operations (`0x0Exx`)

**Status: normative for what is built (2026-09-23).** Every op below is implemented in
`userspace/view-broker/` and encoded by `userspace/librsproto/src/views.rs`. Written with
administration Part A.3; see [`administration.md`](../planning/administration.md) § *Part A in detail*
for the design and why each piece is shaped as it is.

## The shape

The **view broker** runs a program in a *view* — its caller's namespace plus a profile's grants —
when [`/system/views.toml`](views-toml-schema.md) says the caller may. It is spawned by `init`,
which binds its forwarding endpoint at `/svc/views` in the root namespace.

**Identity is the path a channel was resolved through.** Nothing a client sends names a
principal.

| Role | Resolved as | Suffix the broker sees | Speaks |
|---|---|---|---|
| forwarding endpoint | bound by `init` at `/svc/views`; by a login supervisor at `/dev/views` in each session, with the subtree base `/s/<session>` | — | `Namespace::Resolve` |
| supervisor channel | `/svc/views/session`, from the root namespace | `session` | `OpenSession`, `CloseSession` |
| client channel | `/dev/views`, from inside a session | `s/<session>` | `Request`, `Password`, `Stop`, `List`, `Check`; receives `Exited` |

A session id is decimal, non-zero, with no leading zero; any other suffix is `NotFound`, as is the
base of a session that is not open. **Ids increase and are never reused within a boot** — a program
that ignored its session's end still holds a namespace with that base in it.

**A resolve the broker has no room for is `WouldBlock`.** It waits on every channel in one wait
set of `MAX_WAIT_HANDLES`, and **counts a client channel as two slots from the moment it is let
in** — the channel, and the life channel of the program it may start (`view_broker::slots`) — so a
client it admits can always start its program with its exit heard.

**Only `Request` carries handles.** Any handle sent with another op is closed unread.

**The boundary.** Anything holding the unscoped root namespace can resolve `/svc/views/session` and
`/svc/views/s/<id>`, and so act as any session — the same boundary `/svc/auth` has
(`TODO(svc-auth-ungated)`), and the same fix. A program in a session cannot: its namespace is
built, and binds only its own base. **One process in a session can**: `desktop-shell`, the
graphical session's leader, holds the raw forwarding endpoint and `BIND_NAMESPACE` so that it can
bind `/dev/views` into the applications it launches, and could bind any base. That adds no one to
the trusted set — it already holds the whole-tree filesystem endpoint — but the graphical
session's identity rests on it ([`graphical-session.md`](../architecture/graphical-session.md) §3).

## Operations

All bodies are little-endian. Outcome replies share one layout:

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | kind: `0` Started, `1` NeedPassword, `2` Denied |
| 1 | 1 | retry: for Denied, `1` if another `Password` on this request will be heard; else `0` |
| 2 | 2 | reason length |
| 4 | n | reason, UTF-8, for a person to read |

### `OpenSession` (`0x0E00`) — supervisor

Request: the principal's name (1–64 bytes, UTF-8). Reply: the new session id, 8 bytes.
Sent by a login supervisor after it has authenticated someone.

### `CloseSession` (`0x0E01`) — supervisor

Request: a session id, 8 bytes. Reply: empty.
The broker asks every program it started for the session to stop (`sys_process_terminate` — a
request), unbinds their grants, and answers nothing further under the session's base. Closing a
session that is not open is not an error.

### `Request` (`0x0E02`) — client

Run a program in a view. Request body:

| Field | Encoding |
|---|---|
| handles | 1 byte: bit 0 stdin, bit 1 stdout, bit 2 stderr, bit 3 terminal |
| view | u16 length + bytes |
| program | u16 length + bytes — a bare name |
| argc | u16 |
| each argument | u16 length + bytes |
| env | u32 length + a TSM1 `Record` ([typed-stream-format](typed-stream-format.md)), opaque to the codec |

Transferred handles, in order: **a namespace** (always — a copy of the caller's own, from
`sys_ns_derive`, since the one a process is spawned with cannot be transferred), then each stream
whose bit is set, then the terminal. A count that disagrees with the bits is refused.

Reply: an outcome — `NeedPassword` when the rule asks for one, `Started`, or `Denied` with the
policy's reason. On `Started`:

- the broker has **copied the namespace again** and built the view in the copy, so the caller holds
  no handle to it;
- it has bound the profile's grants there;
- it has resolved `/bin/<program>` in **its own** namespace — a caller can prune what it sent, and a
  name must not fall through to a shorter binding;
- it has spawned the program in the view with no syscaps beyond the profile's, and a setup message
  carrying the arguments, the streams, the terminal, and the environment with `view` set to the
  view's name.

A channel carries one request.

### `Password` (`0x0E03`) — client

Request: the password's bytes. Reply: `Started`, or `Denied` with `retry` set while the request
has failures left. **The broker holds the check** until the session's delay from its last failure
has passed — on whichever request that failure was, **and whenever the check arrived**: passwords
waiting on several requests are checked oldest first, one at a time, and a failure holds the rest
(`view_broker::pacing::Held`). So a session makes at most one guess per delay however many
requests it opens. Each request is capped at three failures. See
[`administration.md`](../planning/administration.md) § *The shape* for why the delay is the
session's and the cap the request's.

### `Exited` (`0x0E04`) — broker → client

**Unsolicited, `request_id` 0.** Body: the exit code (i32) and a crashed flag (1 byte), 5 bytes.
Sent when the program exits.

The code is matched to the program by **which program's life channel closed** — a handle the
program never learns of, moved to it at spawn — and taken from the notification queue in arrival
order: exact about *which*, and able to swap two codes only if two of the broker's programs exit
in one wake (`TODO(child-exit-attribution)`). **The close can come first.** A process's handles
are closed before its `ChildExited` is queued, so the broker holds a closed life until its code
arrives, and does not wait on it meanwhile (`view_broker::exits`).

### `Stop` (`0x0E05`) — client

Request: empty. Reply: empty. Asks the program to exit — what `with` sends when its shell asks
*it* to. A client channel that closes while its program runs is treated the same way.

### `List` (`0x0E06`) — client

Request: empty. Reply: a u16 row count, then per row: view (u16 length + bytes), the programs
(u16 length + bytes — `*`, or names separated by spaces), and a password flag (1 byte). What the
session's principal may use, one row per view a rule lets them use, in the policy's order. A policy
that does not read is an error reply (`InvalidArgument`).

### `Check` (`0x0E07`) — client

Request: a policy's text. Reply: `Started` if it reads and leaves someone able to administer the
system; `Denied` with the reason otherwise. Installs nothing, so it needs no grant.

## References

- [`views-toml-schema.md`](views-toml-schema.md) — the policy file
- [`rsproto-wire-format.md`](rsproto-wire-format.md) — framing, request ids, error replies
- [`rsproto-namespace-ops.md`](rsproto-namespace-ops.md) — how a resolve mints a channel
- [`pipeline-stdio.md`](pipeline-stdio.md) — the setup message the program receives
