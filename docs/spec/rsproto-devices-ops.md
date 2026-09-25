# rsproto — Devices operations (`0x0Fxx`)

**Status: normative for what is built (2026-09-24).** `Arrived` and `Settled` are implemented in
`userspace/device-mgr/` and encoded by `userspace/librsproto/src/devices.rs`; `Departed` is
encoded there and **sent by nothing until Phase 6** gives the kernel an event source. `input-server`
subscribes to `input` from boot on (Part B.3), and every session reads the tables at `/dev/devices`
through an info-only endpoint (Part B.4). Written with administration Part B.2; see [`administration.md`](../planning/administration.md) § *Part B in
detail* for the design and why each piece is shaped as it is.

## The shape

The **device manager** learns what devices the machine has from
[`/dev/registry`](device-node.md#the-registry-devregistry) and hands each to the service that owns
its class. It does not drive devices. It is spawned by `init`, which binds its forwarding endpoint
at `/svc/devices` in the root namespace.

| Role | Resolved as | Suffix the manager sees | Answer |
|---|---|---|---|
| forwarding endpoint | bound by `init` at `/svc/devices` | — | `Namespace::Resolve` |
| class owner | `/svc/devices/<class>`, from the root namespace | `input` or `block` | a channel; receives `Arrived`, `Settled`, `Departed` |
| directory | `/svc/devices/info` | `info` | a channel answering `File::ReadDir` |
| table | `/svc/devices/info/<name>.tsm` | `info/<name>.tsm` | a read-only memory object: a TSM1 table |
| info-only endpoint | `/svc/devices/info-endpoint`, asked for once by `init` | `info-endpoint` | a forwarding endpoint of the manager's own — see below |

Any other suffix is `NotFound`, as is a table for a name no device has. **A directory session the
manager has no room for is `WouldBlock`**: it waits on every channel in one wait set of
`MAX_WAIT_HANDLES`, less its endpoint, two info-only endpoints and a slot per class. A third
info-only endpoint is `WouldBlock` too.

**The classes are the manager's**, derived from a record's kind rather than the kernel's
`DeviceClass`, which calls the console, the keyboard and the mouse all `Char`:

| Class | Kinds | Owner |
|---|---|---|
| `input` | keyboard, mouse | `input-server`, from boot on |
| `block` | disk, partition, RAM disk | `storage-service`, from boot on (administration Part C.5a) |

A console or a PCI function is in no class, and is only ever information.

### Subscribing

**Resolving `/svc/devices/<class>` is the subscription.** The answer is a channel, and on it the
manager sends, unsolicited and with `request_id` 0:

1. an `Arrived` for every present device of the class, in registry order, **each carrying the
   owner's own duplicate of the device's node** — so an owner that exits cannot take a device
   from the next one;
2. one `Settled`, with how many `Arrived` it sent.

That replay is coldplug. **It is queued before the resolve completes**, so an owner holding the
channel holds the whole replay: `Settled` is already waiting, and its count never depends on how
soon the owner reads. The channel is deep enough for the replay and its `Settled` with room to
spare. Later arrivals and `Departed` follow on the same channel, so an owner is written once,
against events.

**A class has one owner at a time.** While an owner's channel is open, a second resolve of the
class is refused with `AlreadyExists`. That is the kernel's rule kept at the manager: a raw device
has one ring and one parked reader, so a second reader would drain events meant for the first.
When the owner's channel closes the class is free again. **The manager learns of the close in its
own time**, so a subscription sent straight after one may still be refused; a caller that has just
closed its own subscription and wants the class back retries.

An owner sends nothing on its channel. A message it does send is answered `Unsupported`, and any
handle it carries is closed unread.

### The information side

`info` is a directory of TSM1 tables ([typed-stream-format](typed-stream-format.md)), for anyone
to read: `all.tsm`, every device a row in registry order, then one `<name>.tsm` per device.

**A session reaches it through an info-only endpoint** (Part B.4). Resolving `info-endpoint` on
the root endpoint answers a channel that is itself a **forwarding endpoint**: bound in a namespace,
the kernel forwards resolves on it to the manager like any server's. **On it the manager answers
the directory and the tables, and nothing else** — a class, or `info-endpoint` again, is
`NotFound` whatever the suffix — so its holder cannot subscribe and cannot mint an endpoint that
could. `init` asks for one at boot and couriers it down Part A's chain; both login supervisors bind
it at `/dev/devices` with the subtree base `/info`, and `desktop-shell` binds it the same way into
each application. The base names what a session reaches — `/dev/devices` is the directory,
`/dev/devices/all.tsm` a table — and **the endpoint is the boundary**. The base alone would not be:
`desktop-shell` holds the endpoint and `BIND_NAMESPACE`, so it could bind it with no base, where
`block` on the root endpoint is a subscription to every disk.

**A device's name is its path's**, so a name says which binding would reach it:

| Kind | Name | Path |
|---|---|---|
| disk, partition, RAM disk | `blk-<n>` | `/dev/blk/<n>` |
| keyboard, mouse | `input-<n>` | `/dev/input/raw/<n>` |
| console | `console` | `/dev/console` |
| PCI function | `pci-<bus>.<dev>.<fn>` in hex, `pci-<seg>.<bus>.<dev>.<fn>` off segment 0 | — |
| unknown | `dev-<id>` | — |

`<n>` is the record's served index — the one the kernel resolves the path through — not a count
within the class.

Each table has these columns. What a device does not have is `Null`, not zero or empty: a
keyboard has no size, which is different from a size of nothing.

| Column | Type | Value |
|---|---|---|
| `name` | String | as above |
| `kind` | String | `disk`, `partition`, `ramdisk`, `keyboard`, `mouse`, `console`, `pci`, `unknown` |
| `path` | String, nullable | as above |
| `size` | Int, nullable | bytes, a block device's |
| `description` | String, nullable | a disk's model and serial, a partition's label, a RAM disk's module and path; a PCI function's `vendor:device class cc.ss.pi` |
| `parent` | String, nullable | the name of the device it belongs to: a partition's disk, a disk's controller |
| `driver` | String, nullable | the driver that registered it; a PCI function a driver declined reads `<driver> (declined)` |

The memory object is page-sized; the table ends at its terminator, which is where a reader stops.

## Operations

All bodies are little-endian.

### `Arrived` (`0x0F00`) — manager → owner

**Unsolicited, `request_id` 0.** Body: the device's registry record, exactly 144 bytes, as
`/dev/registry` serves it (`DeviceRecord` in
[`device-node.md`](device-node.md#the-registry-devregistry)). `handles[0]`: the device's node,
with `READ | WRITE | DUPLICATE | INSPECT | TRANSFER` — `/dev/blk`'s authority, the most any class
needs. A body of any other length is malformed.

### `Settled` (`0x0F01`) — manager → owner

**Unsolicited, `request_id` 0.** Body: a u32, how many `Arrived` the replay sent. Sent once, after
the replay. An owner that serves only once it has its devices waits for this, not for a count it
cannot know.

### `Departed` (`0x0F02`) — manager → owner

**Unsolicited, `request_id` 0.** Body: a u32, the departed device's registry id — the `id` of the
record its `Arrived` carried, which is stable within a boot. **Nothing sends one until Phase 6**;
it is specified now so an owner is written against it from the start.

## References

- [`device-manager.md`](../architecture/device-manager.md) — the manager, its owners and readers, and what the gates prove
- [`device-node.md`](device-node.md) — the registry and the record
- [`rsproto-wire-format.md`](rsproto-wire-format.md) — framing, request ids, error replies
- [`rsproto-namespace-ops.md`](rsproto-namespace-ops.md) — how a resolve mints a channel or an object
- [`typed-stream-format.md`](typed-stream-format.md) — TSM1, what the tables are
