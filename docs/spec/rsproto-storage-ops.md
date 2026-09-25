# rsproto — Storage operations (`0x10xx`)

**Status: normative for what is built (2026-09-25).** `Mount`, `Unmount` and `InUse` are
implemented in `userspace/storage-service/` and encoded by `userspace/librsproto/src/storage.rs`
(administration Part C.5c). The view broker's `storage` grant binds the admin endpoint into a view
(C.6), and `disk --mount` and `disk --unmount` speak these requests there (C.7). See
[`storage.md`](../architecture/storage.md) for the service, and [`administration.md`](../planning/administration.md) § *Part C in detail*
for the design and its reasons.

## The shape

The **storage service** owns every block device, reports what each holds, and mounts what it can
serve. `init` binds its forwarding endpoint at `/svc/storage` in the root namespace. Mounting and
unmounting are asked for on an **admin session**, and nothing else speaks this category.

| Role | Resolved as | Suffix the service sees | Answer |
|---|---|---|---|
| forwarding endpoint | bound by `init` at `/svc/storage` | — | `Namespace::Resolve` |
| admin endpoint | `/svc/storage/admin-endpoint`, from the root namespace | `admin-endpoint` | a forwarding endpoint of the service's own |
| admin session | any resolve on an admin endpoint — the view broker's `/dev/storage/admin` | any | a channel carrying the requests below |

**Who holds an admin endpoint decides who mounts.** The service answers every request on an admin
session, and gates nothing itself. The view broker resolves one admin endpoint the first time a
view needs it, and binds it at `/dev/storage/admin` in a view whose profile has the `storage` grant
(C.6). `disk` is what a person runs there ([`shell-language.md`](shell-language.md) §10d). A **session
endpoint**, the one sessions get for `/storage` and `/dev/storage`, answers `admin-endpoint` with
`NotFound`, however it is bound. Two admin endpoints can exist at once, and four admin sessions.
Past either, the resolve is `WouldBlock`.

The filesystems and the table of what is on each disk are ordinary `Namespace` and `File`
operations under `/svc/storage/fs` and `/svc/storage/info`
([`storage.md`](../architecture/storage.md) §§6–8), not these.

## Requests

Every request is sent on an admin session and answered on it, echoing its `request_id`. A
refusal is the standard [`ErrorBody`](rsproto-wire-format.md#error-replies), with a reason that
names the step that refused.

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

**Always writable, on a live boot too.** The automatic mount of a live boot is read-only because
nobody chose it; an administrator's mount is a choice.

| Refusal | When |
|---|---|
| `NotFound` | no block device has that name |
| `AlreadyExists` | it is mounted already, by `init` or by the service; or another mount has that label |
| `Unsupported` | it holds no filesystem the service can serve — FAT until Phase 6, or nothing |
| `InvalidArgument` | the label is not a valid one ([`storage.md`](../architecture/storage.md) §6), or the body is malformed |
| `WouldBlock` | every mount slot is in use |
| `IoError` | the filesystem's server did not come up; the reason says how |

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
   [`Meta::Unmount`](rsproto-wire-format.md). From here the mount is gone, whatever the answer.
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
write to a disk reaches its partitions. A RAM disk holding a partition counts as its disk. The view
broker asks this before it binds the `disks` grant's devices (C.6), so a view cannot be handed the
root's disk raw.
