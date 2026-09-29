# rsproto — Log operations (`0x07xx`)

**Status: normative for what is built (2026-09-29).** `Read` is implemented in
`userspace/logging-service/` and encoded by `userspace/librsproto/src/log.rs` (administration Part
E.6). The ring it answers from is `userspace/logging-service/src/ring.rs`; the view broker's `logs`
grant binds a read endpoint into a view, and `log` reads through it. See
[`logging.md`](../architecture/logging.md) for the service, and
[`administration.md`](../planning/administration.md) § *Part E in detail* for the design.

The `Log` category was reserved for reply-bearing logging ops when the service was built
(2026-07-31). Appending is still **not** an op: it is a raw send on a log channel
([`rsproto-wire-format.md`](rsproto-wire-format.md) § *Log records*).

## The shape

The logging service serves **`/log`** from an endpoint of its own, bound in the root namespace.
A log channel is resolved under it by path; reading back is asked for on a **read session**.

| Role | Resolved as | Suffix the service sees | Answer |
|---|---|---|---|
| serving endpoint | `/log`, bound by `service-mgr` | — | `Namespace::Resolve` |
| log channel | `/log/<tier>/<principal>[/<source>]` | the path | a channel the resolver appends to; a principal or source longer than 64 bytes is `InvalidArgument` |
| read endpoint | `/log/read-endpoint`, from the root namespace | `read-endpoint` | a forwarding endpoint of the service's own; at most **2** held at once, a third `WouldBlock` |
| read session | any resolve on a read endpoint | any | a channel carrying [`Read`](#read-0x0700); at most **2** open at once, a third `WouldBlock` |

**Who reaches `read-endpoint`**: a holder of the root namespace, as with every server's admin
endpoint (`TODO(svc-auth-ungated)`). No session binds `/log`. The view broker binds its read
endpoint at `/dev/logs` in a view with the `logs` grant
([`views-toml-schema.md`](views-toml-schema.md)), and keeps it for the boot, so one of the two is
the broker's once a `logs` grant has been used.

**A read endpoint or session let go is retired in the same wake** as a resolve that follows it:
the service answers closes before resolves, so a reader that lets one go and asks for another at
once is not refused a slot that is free.

## Requests

Enveloped as every rsproto request is. A session answers `Read`, and refuses any other op
`Unsupported`.

### `Read` (`0x0700`)

**The records the service still keeps after a sequence number**, oldest first.

Request body, exactly 12 bytes, or `InvalidArgument`:

| Offset | Field | |
|---|---|---|
| 0 | `after: u64` | only records whose sequence is greater; `0` from the start |
| 8 | `max: u32` | at most this many; `0` for as many as one reply holds |

Reply body:

| Offset | Field | |
|---|---|---|
| 0 | `count: u32` | records that follow |
| 4 | `flags: u32` | reserved, `0` |
| 8 | `oldest: u64` | the oldest sequence the ring still holds, `0` for none |
| 16 | records | `count` of them, each as below, with nothing after the last |

Each record:

| Offset | Field | |
|---|---|---|
| 0 | `sequence: u64` | the service's count, which orders records; never `0`, and rising through a reply |
| 8 | `time: u64` | the wall clock at ingest, nanoseconds since the epoch; `0` when the clock was not set |
| 16 | `timestamp: u64` | the monotonic clock at ingest |
| 24 | `tier: u8` | `0` kernel, `1` system, `2` app |
| 25 | `level: u8` | `0` trace … `5` critical |
| 26 | `principal_len: u16` | never `0` |
| 28 | `source_len: u16` | `0` for no source |
| 30 | `message_len: u16` | |
| 32 | `principal`, `source`, `message` | UTF-8, in that order |

- **`principal` and `tier` are the service's**, from the channel the record came on; `level`,
  `source` and `message` are the emitter's claims ([`logging.md`](../architecture/logging.md) §
  *Identity is capability-derived*).
- **None left is an empty reply**, `count` `0`. That is how a reader knows it has read to the end,
  so a reader asks again after the last sequence it was given until one comes back empty.
- **`oldest` says what was dropped.** The ring keeps the most recent records, bounded at a
  megabyte, and numbers every record, so a reader that last read `after` and is answered with
  `oldest` past `after + 1` lost what lies between: from the start of a read, the records before
  the oldest; in the middle of one, those the ring dropped while it was read.
  `librsproto::log::dropped` is that arithmetic, and `log` says either on stderr.
- **Every kept record fits an empty reply**: the ring keeps a message's first 1024 bytes, cut on
  a character boundary and ending in `…`, a principal and a source are at most 64, and a source
  claimed empty is kept as none, since an empty `source_len` means no source. So a reply with
  anything to give holds at least one record. Until the PR #344 review an empty source claim was
  kept as it came, which no reply could carry, and every `Read` stopped at it. The serial console
  has every message whole.
- **`time` can step** — the clock can be set (administration Part E.5) — so `sequence` is the
  order. A record is dropped from the ring only as the oldest, so sequences are never reused.

`librsproto::log::parse_read_reply` checks a reply whole before handing out a record: a count
past the records or short of them, a string past the body, a sequence that does not rise, an empty
principal, or a reserved flag set is refused rather than read.

## References

- [`logging.md`](../architecture/logging.md) — the service, its ring and its sinks
- [`rsproto-wire-format.md`](rsproto-wire-format.md) — the envelope, and the append body
- [`views-toml-schema.md`](views-toml-schema.md) — the `logs` grant
- [`shell-language.md`](shell-language.md) §10d — `log`
