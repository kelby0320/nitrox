# rsproto — Storage operations (`0x10xx`)

**Status: normative for what is built (2026-09-25; `InUse` and the boot medium 2026-10-06; the media
session, `Eject`, the watch and `Changed` 2026-10-07; `Reread`, and `Changed` for the devices too,
2026-10-07).** `Mount`, `Unmount` and `InUse` are
implemented in `userspace/storage-service/` and encoded by `userspace/librsproto/src/storage.rs`
(administration Part C.5c). The view broker's `storage` grant binds the admin endpoint into a view
(C.6), and `disk --mount` and `disk --unmount` speak these requests there (C.7). **`Eject` and
`Changed`** (Phase 6 Part F) are a session's: `disk --eject` and Files speak them through the
session endpoint every login binds. **`Reread`** (Phase 6 Part G) is the admin session's again:
`disk --partition` and `disk --format` send it once they have written a device. See
[`storage.md`](../architecture/storage.md) for the service,
and [`administration.md`](../planning/administration.md) § *Part C in detail* for the design and its
reasons.

## The shape

The **storage service** owns every block device, reports what each holds, and mounts what it can
serve. `service-mgr` starts it and binds `/svc/storage` in the root namespace to reach its
forwarding endpoint, through `service-mgr`'s registry (administration Part E.1a; `init` did both
until then). Mounting and unmounting are asked for on an **admin session**, and nothing else speaks
this category but a session's `Eject`, on a media session, and the service's `Changed`, on a watch.

| Role | Resolved as | Suffix the service sees | Answer |
|---|---|---|---|
| forwarding endpoint | `/svc/storage`, bound by `service-mgr` | — | `Namespace::Resolve` |
| admin endpoint | `/svc/storage/admin-endpoint`, from the root namespace | `admin-endpoint` | a forwarding endpoint of the service's own |
| admin session | any resolve on an admin endpoint — the view broker's `/dev/storage/admin` | any | a channel carrying `Mount`, `Unmount`, `InUse` and `Reread` |
| media session | `/dev/storage/media`, through a session endpoint or `/svc/storage` | `info/media` | a channel carrying `Eject` (Phase 6 Part F) |
| watch | `/dev/storage/watch`, likewise | `info/watch` | a channel the service sends `Changed` on, and nothing else (Phase 6 Part F) |

**Who holds an admin endpoint decides who mounts.** The service answers every request on an admin
session, and gates nothing itself. The view broker resolves one admin endpoint the first time a
view needs it, and binds it at `/dev/storage/admin` in a view whose profile has the `storage` grant
(C.6). `disk` is what a person runs there ([`shell-language.md`](shell-language.md) §10d). A **session
endpoint**, the one sessions get for `/storage` and `/dev/storage`, answers `admin-endpoint` with
`NotFound`, however it is bound. Two admin endpoints can exist at once, and four admin sessions.
Past either, the resolve is `WouldBlock`.

**A session ejects, and follows the mounts, without one** (Phase 6 Part F). A session endpoint
answers `info/media` with a media session and `info/watch` with a watch, as it answers the tables;
neither mounts anything, and neither suffix is listed in `info`. Two media sessions can be open at
once, each held for a request; past that the resolve is `WouldBlock`, and `disk --eject` says to try
again. Thirty-two watches can be held, the machine's; with all held, a resolve pings every watcher,
lets go of the ones whose clients have gone, and is refused `WouldBlock` if none had.

The filesystems and the table of what is on each disk are ordinary `Namespace` and `File`
operations under `/svc/storage/fs` and `/svc/storage/info`
([`storage.md`](../architecture/storage.md) §§6–8), not these.

## Requests

Every request is sent on an admin session — `Eject` on a media session — and answered on it,
echoing its `request_id`. A refusal is the standard
[`ErrorBody`](rsproto-wire-format.md#error-replies), with a reason that names the step that refused.

### `Mount` (`0x1000`)

**Mount a device, writable.** Body:

| Offset | Size | Field |
|---|---|---|
| 0 | 2 | `device_len` |
| 2 | 2 | `label_len` |
| 4 | `device_len` | the device's name as the tables name it, `blk-<n>` |
| 4 + `device_len` | `label_len` | the label to mount it under, or nothing to let the service choose |

A body whose lengths do not account for it exactly is refused with `InvalidArgument`. The reply's
body is the label the filesystem was mounted under, and it appears at `/svc/storage/fs/<label>`.

**Always writable, on a live boot too.** The automatic mount of an internal disk on a live boot is
read-only because nobody chose it; an administrator's mount is a choice.

| Refusal | When |
|---|---|
| `NotFound` | no block device has that name |
| `AlreadyExists` | it is mounted already, by `init` or by the service; or another mount has that label |
| `Unsupported` | it holds no filesystem the service can serve: nothing, or a FAT its server would refuse (Phase 6 Part E) |
| `InvalidArgument` | the label is not a valid one ([`storage.md`](../architecture/storage.md) §6), or the body is malformed |
| `WouldBlock` | every mount slot is in use |
| `IoError` | the filesystem's server did not come up; the reason says how |
| `NoAccess` | `init`'s mounts are not all known, so the device could be `init`'s root ([`storage.md`](../architecture/storage.md) §5) |

### `Unmount` (`0x1001`)

**Unmount a filesystem, by its label.** Body: the label's bytes. Reply body: empty. It is a chain,
and each link runs only once the one before it held:

1. **The label leaves `fs`**, so nothing new resolves under it.
2. **Every dirty file is written back** (`sys_ns_sync`), whether or not anything still holds it.
3. **It is refused while a file is still held** (`sys_ns_held`): a handle or a mapping could
   write after the filesystem is marked clean. The count is asked after the sync, and asked
   again after a few milliseconds before it is believed, because an IRP that just finished holds
   its file until its completion ends (`syscall-abi.md` § `sys_ns_held`). **A refusal here leaves
   the mount exactly as it was.**
4. **The server records the filesystem clean and exits**, on
   [`Meta::Unmount`](rsproto-wire-format.md). From here the mount is gone, whatever the answer. A
   read-only mount records nothing and leaves the filesystem as it was found, clean or not.
5. **The drive's cache is flushed** (`IoOpcode::Flush`), for a writable mount. A read-only mount
   wrote nothing.
6. **The mount's namespace is dropped.**

| Refusal | When |
|---|---|
| `NotFound` | nothing is mounted with that label. `init`'s mounts have no label and are never unmounted here |
| `WouldBlock` | a file on it is still open or mapped |
| `IoError` | a write-back failed (the mount is left as it was); or the server did not record the filesystem clean, or the flush failed (the mount is gone either way) |

### `InUse` (`0x1002`)

**The devices in use, which must not be granted raw.** Body: empty. Reply body:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `count` |
| 4 | 4 × `count` | registry ids, ascending |

Every mounted filesystem's device, `init`'s included, **and the disk that holds it**, since a raw
write to a disk reaches its partitions. A RAM disk holding a partition counts as its disk. **And the
disk the machine started from** (Phase 6 Part D), its record's `BOOT` flag
([`device-node.md`](device-node.md)), mounted or not: a live stick holds the kernel and the
modules, which nothing mounts. The view
broker asks this before it binds the `disks` grant's devices (C.6), so a view cannot be handed the
root's disk raw. **Refused `NoAccess` when `init`'s mounts are not all known**: an answer would
leave out a root the service could not place, and the broker refuses `disks` when this is not
answered.

### `Eject` (`0x1003`)

**Unmount a removable disk, for a session** (Phase 6 Part F). Sent on a media session. Body: **the
name a filesystem on it is mounted under**, `<name>` of `/storage/<name>` — the tables' `mounted`
column, never their `label`, which two sticks can share and one can lack. Reply body: **the names
of every filesystem it unmounted**, one per line, sent once the stick can be pulled.

**The drive goes whole** (PR #367 review). A stick is pulled whole, so every filesystem the service
mounted on the same disk — the named one's partition siblings, or the one filesystem that fills it
— goes, or none does. Each is written back and asked whether a file is held **before any is
unmounted**, so a file held on one partition refuses the eject with every partition still mounted.
Then each runs `Unmount`'s chain; a file opened between the check and its chain refuses there,
leaving the ones before it unmounted, and the refusal still says the stick cannot be pulled. The
disk must be **removable** — behind USB mass storage, or a partition of one
([`storage.md`](../architecture/storage.md) §6). Eject sends nothing further to the device: the
chain's flush is what a stick needs.

| Refusal | When |
|---|---|
| `NotFound` | nothing the service mounted has that name — `init`'s mounts included, wherever they are |
| `NoAccess` | the disk is not removable: an internal disk's mount is `Unmount`'s, on an admin session (`with admin disk --unmount`) |
| `WouldBlock` | a file on any filesystem of the stick is still open or mapped |
| `IoError` | as for `Unmount` |
| `Unsupported` | any other request on a media session |
| `InvalidArgument` | the name is not UTF-8 |

### `Changed` (`0x1004`)

**The mounts changed, or the devices** (Phase 6 Parts F and G). Sent by the service on a watch,
never by a client: a bare message, `request_id` 0 and an empty body, whenever the names mounted
under `/storage`, or the devices the service knows, differ after a turn of its loop from before it.
The devices since Part G: a partition a rescan published holding nothing changes no mount, and
`disk --partition` waits for its row. **The client reads the tables again**; a `Changed` says
nothing about which mount or device, and the table is the one answer.

**A full queue is a ping already waiting**, so the service drops a send that finds one full and
nothing is lost. A watch whose client has gone is let go when a send to it fails `PeerClosed`.

### `Reread` (`0x1005`)

**Read a device again**, once `disk` has written it (Phase 6 Part G). Sent on an admin session.
Body: the device's name as the tables name it, `blk-<n>`. Reply body: **the names it was mounted
under**, one per line — `Eject`'s form — and empty for none.

- **A partition** is probed again, and mounted by the rules an arriving device meets
  ([`storage.md`](../architecture/storage.md) §6a): its window is unchanged.
- **A disk** — a RAM disk among them — is first **rescanned by the kernel**, `IoOpcode::Rescan` on
  its node ([`io-operation.md`](io-operation.md)): its partitions depart and those its table now
  holds are published. **They reach the service as arrivals after the reply**, and are mounted as
  arrivals are; the reply names only the disk's own mount, if it holds a filesystem whole.

| Refusal | When |
|---|---|
| `NotFound` | no block device has that name |
| `WouldBlock` | the partition is mounted; or, for a disk, anything on it is, since its partitions are about to be replaced. A partition beside a mounted sibling is read again |
| `NoAccess` | it is on the disk the machine started from — asked after `WouldBlock`, so the disk holding `init`'s root says it is mounted; or `init`'s mounts are not all known |
| `Unsupported` | the kernel cannot rescan the disk: every disk but a USB one's |
| `InvalidArgument` | the name is not UTF-8 |
| `IoError` | the rescan failed |
