# rsproto — Services operations (`0x11xx`)

**Status: normative for what is built (2026-09-28).** `Start`, `Stop` and `Restart`, and the table
`all.tsm`, are implemented in `userspace/service-mgr/` and encoded by
`userspace/librsproto/src/services.rs` (administration Part E.2a); the session endpoint every login
binds at `/dev/services`, the view broker's `services` grant and `service` itself are Part E.2b's.
The decisions — what each request does, and what refuses it — are `service_mgr::services`,
host-tested. See [`service-manager.md`](../architecture/service-manager.md) for the manager, and
[`administration.md`](../planning/administration.md) § *Part E in detail* for the design and its
reasons.

## The shape

`service-mgr` serves **`/svc/services`** from an endpoint of its own, bound in the root namespace.
Starting and stopping are asked for on an **admin session**; the list is a table anyone reachable
to it can read.

| Role | Resolved as | Suffix `service-mgr` sees | Answer |
|---|---|---|---|
| forwarding endpoint | `/svc/services`, bound by `service-mgr` | — | `Namespace::Resolve` |
| table | `/svc/services/all.tsm` | `all.tsm` | a read-only memory object: a TSM1 table |
| admin endpoint | `/svc/services/admin-endpoint`, from the root namespace | `admin-endpoint` | a forwarding endpoint of `service-mgr`'s own |
| session endpoint | handed to both login supervisors at spawn, bound at `/dev/services` in every session and application | `all.tsm` | the table; any other suffix, `admin-endpoint` among them, is `NotFound` |
| admin session | any resolve on an admin endpoint | any | a channel carrying the requests below |

Any other suffix is `NotFound`.

**The list is a table, not a request.** `all.tsm` has a row per declaration, in the declarations'
order:

| Column | Type | |
|---|---|---|
| `name` | `String` | the declaration's name |
| `state` | `String` | `starting` (a server whose `Meta::Ready` has not come), `running`, `stopped` (asked to stop, finished with `0`, or never started) or `failed` (any other exit, one whose code never came, or a server still running that refused or sent no `Ready` in time) |
| `restarts` | `Int` | restarts over the boot, by the service's policy or asked for |

A table is minted fresh per resolve, as the device manager's and the storage service's are.

**Who holds an admin endpoint decides who starts and stops.** `service-mgr` answers every request
on an admin session. It gates one thing itself: an `essential` service's stop and restart
([`service-toml-schema.md`](service-toml-schema.md)). At most two admin endpoints and four admin
sessions exist at once; one more is refused `WouldBlock`, and a closed one frees its slot.

## Requests

Each body is the service's name, as its declaration spells it. A reply's body is empty. A refusal
is the standard `ErrorBody`, whose reason says why.

**Each is answered once it has happened**, never by a wait inside `service-mgr` — every resolve on
a server's path waits on that process. A request waits for the service, and is answered then; one
request per service at a time, and a second is refused `WouldBlock`. **An admin session that closes
before its answer loses the answer, and what it asked goes on** — a restart's start included (PR
#341 review).

### `Start` (`0x1100`)

Start a service that is not running. It starts afresh: no earlier stop stands, and its restart
attempts begin again. Answered when it is up — a server once its `Meta::Ready` has been bound —
or refused.

| Refusal | Why |
|---|---|
| `NotFound` | no service is declared by that name |
| `AlreadyExists` | it is already running |
| `KernelError` | it could not be spawned |
| `TimedOut` | a server that did not come up: no `Ready` within 30 s, a refusal, or an exit first |
| `WouldBlock` | it is being started or stopped already |

### `Stop` (`0x1101`)

Stop a running service, by `CTRL_OP_SHUTDOWN` on its control channel. **A stop is a request**:
there is no forcible kill. A service that exits is not restarted, whatever its policy. Answered once
it has exited.

**A service in its restart backoff is not running, and a stop cancels the restart**, answered at
once (PR #341 review). Refusing it "not running" would let a crash-looping service start again.

| Refusal | Why |
|---|---|
| `NotFound` | no service is declared by that name |
| `NoAccess` | it is `essential` |
| `InvalidArgument` | it is not running |
| `Unsupported` | it has no control channel to ask on |
| `TimedOut` | it was asked, and is still running 5 s later; it stays asked, and its exit is still a stop |
| `WouldBlock` | it is being started or stopped already |

**Which services may be stopped**: of the servers, **the clipboard alone**; every other is
`essential`. The terminal server, the input server and the compositor exit when asked too — a
shutdown will ask them — but their clients do not reconnect, so on a machine with no serial port
stopping one leaves nothing to type at (PR #341 review). Of the other services, `heartbeat` exits
when asked. A server's registry entry goes with it, so its path answers `NotFound` until it is
started again.

### `Restart` (`0x1102`)

`Stop`, then `Start`, answered once the new instance is up; or a `Start`, for a service that is not
running. Refused as those are, and `NoAccess` for an `essential` service whether it is running or
not. Every binding of a restarted server's path reaches the new instance
([`service-manager.md`](../architecture/service-manager.md) § *Servers, and the registry*); a client
holding a channel to the old one sees it close.
