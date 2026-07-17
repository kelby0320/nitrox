# Logging service

The userspace **logging service** collects structured log records from any process
that holds a logging capability, stamps each with trusted provenance, and fans them
out to sinks. It is the concrete implementation of the `log` handle seam that
service-mgr has carried as a stub since slice A (`docs/architecture/service-manager.md`
— "Slice-1 `stdout`/`stderr`/`log` route to `sys_kprint` / the kernel log; the logging
service is a later backlog item").

It is **not** the kernel log. `kernel/src/klog.rs` + `/dev/log` capture *kernel*
`kprint!` output (the boot log, `dmesg`); the logging service captures *userspace*
records. The two are distinct streams a future unification may merge (see
[Relationship to the kernel log](#relationship-to-the-kernel-log)).

For the record shape's origin see `docs/history/os-design-v5.1.md` § Logging. This doc
supersedes that sketch's wiring: there is **no** per-service broker/relay through
service-mgr — see [Identity is capability-derived](#identity-is-capability-derived) — and
**no bespoke logging protocol**: connecting reuses namespace resolve, appending is a
generic channel send — see [Transport](#transport-no-bespoke-protocol).

## What it is

A namespace-reachable **resource server**, bound at a well-known logging path by a
supervisor (init/service-mgr) the same way the profile server and fs-servers are. To
log, a process resolves that path — or uses a logging **endpoint** already in its
namespace — and appends records. Any holder of the endpoint may log; a process without
it in its namespace simply cannot (sandboxing by namespace construction, not a
permission check). There is **no `LOG` SysCap** — authority is the endpoint handle.

```
any process ──(logging endpoint = capability carrying principal+tier)──▶ logging service
                                                                              │
                                                                              ▼
                                                              sinks: serial, in-memory ring
                                                                     [disk DB / network — deferred]
```

This is the capability-gated form of syslog's "open the socket and write" and Event
Viewer's "a provider registers, then writes events" — open to any holder, but the
identity is **not** the emitter's to assert (below).

## Identity is capability-derived

The problem with pure self-registration is that a process that both *opens* a source
and *names itself* can claim to be `auth-service` and write anything under that name.
So identity is **not** taken from what the emitter says — it is a property of **which
logging endpoint the record arrived on**. Each endpoint is a capability the logging
service minted, tagged at mint time with a `principal` (who) and a `tier` (what kind of
emitter). Whoever built a process's namespace chose which endpoint it holds; that choice
*is* the vouch, folded into ordinary namespace construction — no separate broker step,
no per-record relay. A process cannot log as a principal whose endpoint it does not
hold, and cannot self-declare its way into another principal.

Concretely, per record:

| Field | Set by | Trusted? |
|---|---|---|
| `principal` | logging service, from the endpoint the record arrived on | **yes** |
| `tier` | logging service, from the endpoint | **yes** |
| `timestamp` | logging service, at ingest (monotonic clock) | **yes** |
| `sequence` | logging service, a global monotonic counter | **yes** |
| `level`, `message`, `source` (sub-label), `span_id`, `trace_id`, `fields` | the emitter, in the wire body | no — the emitter's claim |

An emitter may attach a self-declared `source` sub-label (e.g. `foo.worker`) — but it is
recorded *under* the endpoint's `principal`, never as a principal, so it can organize
its own streams without impersonating anyone.

### Why this is the right shape (and the only consistent one)

The usual Unix path to trusted log identity is the kernel attesting the sender's UID —
systemd-journald derives `_SYSTEMD_UNIT`/`_UID`/`_PID` from the socket's kernel
credentials and marks those trusted fields with a leading `_` to separate them from the
sender's claims. That underscore split is exactly the trusted/claimed line above. But
Nitrox **rejects kernel-level ambient identity** ("no user identity at the kernel
level"), so we cannot lean on a kernel UID — the identity *must* live in a capability.
Journald's model, re-expressed with capabilities instead of kernel credentials, is this
design. Windows Event Viewer confirms the tiering: writing the **Security** channel
requires `SeAuditPrivilege` (only LSASS can) — the high-trust tier is capability-gated,
not self-asserted, which is our `Security = audit subsystem` mapping below.

## The record

The **stored** record (what a sink sees):

```rust
struct LogRecord {
    // Trusted — supplied by the logging service, not the emitter:
    principal: String,   // capability-derived identity (subsumes v5.1's `service`)
    tier:      Tier,     // Kernel | System | Application
    timestamp: u64,      // monotonic clock, at ingest
    sequence:  u64,      // global monotonic
    // Claimed — from the emitter's wire body:
    level:     LogLevel, // Trace=0 Debug=1 Info=2 Warn=3 Error=4 Critical=5
    message:   String,
    source:    Option<String>,       // self-declared sub-label under `principal`
    span_id:   Option<u64>,
    trace_id:  Option<u64>,
    fields:    Vec<(String, Value)>, // structured k/v (deferred; see scope)
}
```

The **wire** body of an append carries only the claimed fields;
`principal`/`tier`/`timestamp`/`sequence` are absent because the emitter does not set them:

```
off 0   u8   level
off 1   u8   flags        bit0 has_span, bit1 has_trace, bit2 has_source
off 2   u16  field_count  (0 in slice 1 — see scope)
off 4   u32  message_len
off 8   u64  span_id      (0 if !has_span)
off 16  u64  trace_id     (0 if !has_trace)
off 24  message[message_len]
off ..  source_len:u16 + source[]   (if has_source)
off ..  fields[field_count]         (absent in slice 1)
```

This is a **payload format, not a protocol** — it is the raw body of a `sys_channel_send`
on a dedicated log channel (see [Transport](#transport-no-bespoke-protocol)). Appending is
**fire-and-forget** — no request id, no reply, no op discriminator — so a slow sink never
blocks an emitter beyond channel backpressure.

## Transport — no bespoke protocol

Logging is *"connect to a resource server, then stream to it,"* and Nitrox already has
both halves. There are **no logging-specific wire ops**:

- **Connect = namespace resolve.** Obtaining a log endpoint is `sys_ns_lookup` of a path
  under the logging service — the path *is* the identity (`system/<principal>`, or a
  named source `<principal>/<label>`). The logging service answers the forwarded resolve
  by minting a channel pair, keeping the **read** end tagged with that principal/tier, and
  **transferring the write end** as the resolved capability — structurally identical to how
  the profile server answers a resolve, but returning a *live connection* instead of a
  file. The client keeps that channel and reuses it (resolve once, log many).
- **Append = channel send.** Writing a record is `sys_channel_send` of the
  [record body](#the-record) on that channel. On a dedicated log channel there is nothing
  to discriminate, so **no op is needed** — the channel's identity is the op.

The only new kernel surface is a small, general relaxation: a **resolve reply may transfer
a channel-endpoint** capability, not just a `MemoryObject`/`FileObject` (same shape as the
`FileObject` relaxation the profile-server slice made). This is not a logging hack — it is
the general *"resolve a service path → get a channel to that service"* primitive; logging
is simply its first consumer.

### Identity, tiers, and authority — all via the namespace

Because the read end is **tagged at resolve time** and the logging service `sys_wait`s
across all of them, a record's identity is *which channel it arrived on* — established
once, at connect. There is no per-record identity field to forge. Everything else falls
out of ordinary namespace attenuation:

| Tier | Endpoint from | Guard |
|---|---|---|
| **Kernel** | the kernel `klog`/audit rings (not resolved) | intrinsic; a future source the logging service may ingest |
| **System** | resolving `system/<principal>` | only bindings held by service-mgr/init permit the `system/*` subtree |
| **Application** | resolving `<session>/<app>/…` (session-mgr, later), or inherited | an app's binding is scoped to its own subtree — it cannot resolve `system/*` |

- **Cross-tier impersonation guard = namespace scoping.** A process can only resolve paths
  its namespace binding permits; a service cannot reach `system/auth-service` because that
  subtree is not bound for it. No separate "mint authority" — it is the same
  sandboxing-by-namespace mechanism used everywhere.
- **Inheritance is the default.** A process whose spawner sets up nothing new inherits the
  log endpoint already in its namespace and logs under that parent's principal —
  syslog-tag-like attribution.
- **Named sources are unprivileged self-labelling.** Resolving `<principal>/<label>` (or
  attaching `source` to a record) sub-labels *your own* principal — the app-facing
  "register a provider, then log" experience, safe because it can never change
  `principal`/`tier`.

**`Security` = the audit subsystem, not this service.** Security-significant events go to
the kernel audit ring + the audit service (`SysCaps::AUDIT_CONTROL`, chained records for
tamper-evidence) — the analog of Event Viewer's Security channel. Cross-referenced here,
built in its own slice.

## service-mgr's role

Reduced to namespace construction: at spawn, service-mgr resolves `system/<principal>`
under the logging service (which it can, holding a `system/*`-scoped binding) and binds
the returned channel as the service's `log` handle — and routes `stdout`/`stderr` to it
per the schema default. That single resolve is the vouch. service-mgr is **out of the
data path** — not a per-record relay (the rejected v5.1 sketch), and not even a per-open
broker: once the channel is bound, the service logs directly to the logging service.
`docs/spec/service-toml-schema.md` (which currently describes the relay wiring) is updated
to match as part of this slice.

## Sinks

Each stamped record routes to an ordered set of sinks behind one `Sink` trait
(`fn write(&mut self, rec: &LogRecord)`). Slice 1 ships two:

- **Serial** — formats the record to a line and emits it (via `sys_kprint`), so logs are
  visible on the console as the stub did, now structured and stamped.
- **In-memory ring** — a bounded keep-recent ring of recent records for later read-back
  (`journalctl`-style). Slice 1 populates it; the read-back path is a later part.

**Deferred — persistent DB on disk** (needs fs-server *write*, a later Phase-3 slice)
and **network** (needs netstack). Both slot in behind the same `Sink` trait; sequencing
Logging before fs-server RW is deliberate — serial + ring need no write capability.

## Relationship to the kernel log

`/dev/log` (kernel `klog`) and the logging service are separate: the former is the
kernel's own `kprint!` capture (a read-only `MemoryObject` snapshot, `dmesg`), the latter
aggregates userspace records. Not merged in this slice; `/dev/log` keeps working. A
future unification — the logging service ingesting the kernel ring as a Kernel-tier
source — is possible but out of scope.

## Client ergonomics

Emitting should be a one-liner, not hand-rolled record encoding or raw `sys_channel_send`.
A small client surface — `log!`/`info!`/`warn!`/`error!` over a held endpoint, plus an
`open_source(label)` helper that resolves `<principal>/<label>` and returns a named-stream
handle — lives alongside the syscall layer so any process can use it. Exact crate placement
(a `liblog`, or a module in libos) is settled when the client is written; the only
constraint is that it depend no higher than the layer its callers sit at.

## Slice 1 scope

**In:** the logging service as a namespace-bound RS; the `LogRecord` body codec in
librsproto; the kernel relaxation letting a resolve reply transfer a channel endpoint;
capability-derived identity + the trusted/claimed split; the System tier via service-mgr
resolving a per-service `system/<principal>` endpoint and routing `stdout`/`stderr`/`log`
to it (retiring the `sys_kprint` stub); the serial sink + the in-memory ring sink; a
minimal client; a **self-registration demo** — a supervised program that resolves a named
source under its principal and logs, proving the app-facing register-then-log path end to
end.

**Deferred:** the Application tier's *session-mgr-vouched* `principal` (session-mgr is a
later spine slice — until then App-tier endpoints aren't resolved, only the
self-open-under-System-principal demo runs); the disk-DB sink (fs-server RW) and network
sink (netstack); structured `fields`/`Value` (typed-I/O / `TableWriter` — the wire reserves
`field_count`); the ring read-back / `journalctl` path (its first *reply-bearing* op is
where a real logging op namespace would be introduced); and any kernel-log unification.

## See also

- `docs/architecture/service-manager.md` — the `log` handle seam this replaces
- `docs/spec/service-toml-schema.md` — `[service.<name>.handles.log]`, stdio routing
- `docs/spec/rsproto-wire-format.md` — the `LogRecord` body codec (append is a raw channel send, no op)
- `docs/history/os-design-v5.1.md` § Logging — the original record sketch
- `kernel/src/klog.rs`, `/dev/log` — the distinct kernel log
- The audit subsystem (`SysCaps::AUDIT_CONTROL`) — the Security-tier analog, its own slice
