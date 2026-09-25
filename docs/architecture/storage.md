# Storage

**Status: partly built — administration Part C.5a, 2026-09-25; last checked 2026-09-25.** What
exists: `storage-service`, the owner of `block`. It reads what each disk, partition and RAM disk
holds, and which of them `init` mounted, and serves that as TSM1 tables at `/svc/storage/info`.

What does not exist yet, in the order Part C builds it
([`administration.md`](../planning/administration.md) § *Part C in detail*):
- **Mounting**: auto-mount, a namespace per mount, and `/storage/<label>/…` answered with
  `SUBNAMESPACE` (C.5b).
- **The session and admin endpoints**, the `Storage` protocol, and the unmount chain (C.5b, C.5c).
- **`/storage` and `/dev/storage` in sessions** (C.6), **`disk`** (C.7), and **`check-storage`** (C.8).

## 1. What it is for

**Raw devices and filesystems are different things.** `/dev/blk/<n>` is bytes, for tools such as
the installer. What a person browses is a filesystem, which will appear at `/storage/<label>` once
mounted. The storage service is where the second comes from: the owner the device manager hands
disks to, which knows what is on each one.

Today it knows and says; it does not yet mount. Its table is already the answer to "what disks does
this machine have, and what is on them", which nothing else can give: `/dev/devices` lists devices,
not filesystems, and `init`'s bindings do not name the devices behind them.

## 2. The pieces

| Piece | Where | What it does |
|---|---|---|
| The service | `userspace/storage-service/` | The library decides what a device holds, which devices are `init`'s, whether this is a live boot, and the tables. The binary is the event loop |
| The manifest reader | `userspace/libinittoml/` | `init.toml`'s schema and parser, shared with `init` so the two cannot read it differently |
| The ext4 checks | `userspace/fs-server-ext4/src/ext4.rs` | `check_device`, `volume_label` and `was_left_clean`: what the server itself runs, so the service never offers a filesystem the server would refuse |
| The partition-table reader | `userspace/libgpt/` | Each disk's entries, for `init.toml`'s UUID sources |

## 3. A boot, end to end

1. **`init` mounts its critical path**, then spawns the device manager and, **straight after it**,
   the storage service. A class's owner is whoever subscribes first, so the service must come
   before anything declared (`TODO(svc-auth-ungated)` in
   [`deferred-decisions.md`](../rationale/deferred-decisions.md)).
2. **The service takes `block`**: it resolves `/svc/devices/block`, and the manager has already
   queued an `Arrived` per disk, partition and RAM disk, each carrying the service's own duplicate
   of the device's node, then `Settled` ([`device-manager.md`](device-manager.md) §4). The
   subscription stays open for the life of the service; closing it would free the class.
3. **It reads each device** (§4), and each disk's partition table, then closes the node: nothing
   in this part writes to a device.
4. **It reads `/initramfs/etc/init.toml`** and matches each of `init`'s mounts to its device (§5).
5. **It says what it found**, a line per device, then answers `Meta::Ready`, and `init` binds it
   at `/svc/storage`.

On a `test-qemu` boot the log reads:

```
storage-service: 3 block device(s)
storage-service: blk-0 (disk QEMU HARDDISK (QM00001)): no filesystem
storage-service: blk-1 (partition NITROX_ESP): fat 'NITROX_ESP'
storage-service: blk-2 (partition nitrox-root): ext4; init's at / (rw)
storage-service: not a live boot
```

## 4. What a device holds

- **ext4**, if `fs-server-ext4`'s `check_device` accepts it: the superblock parses and inode 2 is
  a directory. That is the check the server runs before it says Ready, so a device the service
  calls ext4 is one a server would serve. Its label (`s_volume_name`, empty when `mke2fs` was
  given no `-L`, as the image builder's is) and whether it was left clean come with it.
- **FAT**, from its boot sector: the `0x55AA` signature, a sector size from 512 to 4096, the
  extended boot signature `0x29`, and the type string after it. Recognised and reported, never
  mounted: there is no `fs-server-fat` until Phase 6. A GPT disk's protective MBR carries the
  signature and none of the rest, so a whole disk is not mistaken for a filesystem.
- **Nothing**, otherwise: a disk holding a partition table, a blank one, or a filesystem neither
  reader can read. An ext4 its server would refuse, a 64-bit one say, is also "nothing" today (§9).

## 5. `init`'s mounts, and a live boot

`init` records its mounts as bindings, and nothing names the device behind one. What does is
`init.toml`: every `[[mount]]` is critical-path, so on a running system each one succeeded. Its
source names a partition by one of the two schemes `init` accepts
([`init-toml-schema.md`](../spec/init-toml-schema.md)):
- **`gpt-partlabel:<label>`** is the first partition record with that name, as the kernel binds
  `/dev/disk/by-partlabel/<label>` to the first partition published with it.
- **`gpt-partuuid:<uuid>`** is read from the parent disk's own table, since the registry does not
  carry a partition's GUID. The kernel publishes a disk's partitions in table order, skipping unused
  entries, so the k-th entry in use is the k-th partition record under that disk. Its name and size
  confirm the match, and a position they contradict is no match. The UUID is compared in the form
  the kernel names the path with, exactly, as `init`'s lookup is.

**`init`'s mounts stay `init`'s**: reported, never mounted again, never unmounted.

**A live boot is one whose root is on a RAM disk**, through a partition of one or as the disk
itself. That is the fact that makes the machine's own disks the install target, and it is what
will make the auto-mount read-only (C.5b). A root matched to no device is not a live boot.

## 6. The tables

`/svc/storage/info` is a directory of TSM1 tables, like `/dev/devices`: `all.tsm` with a row per
block device in registry order, then one `<name>.tsm` per device. **A name is `/dev/devices`' own**,
`blk-<n>` for `/dev/blk/<n>`, so a row in either table names the same device.

| Column | Type | Holds |
|---|---|---|
| `name` | string | `blk-<n>` |
| `kind` | string | `disk`, `partition` or `ramdisk` |
| `size` | int, nullable | bytes |
| `filesystem` | string, nullable | `ext4` or `fat` |
| `label` | string, nullable | the filesystem's own label |
| `mounted` | string, nullable | where |
| `by` | string, nullable | `init` or `storage` |
| `mode` | string, nullable | `ro` or `rw` |
| `clean` | bool, nullable | whether an ext4 was left cleanly unmounted |

**`clean` is `Null` for a filesystem mounted writable.** A writable mount clears the bit before
it answers Ready ([`ext4-fs-server-rw.md`](ext4-fs-server-rw.md)), so that state says "in use",
because it is, and nothing about how the filesystem was left. The service's log line follows the
same rule.

## 7. Who can reach what

| Holder | Reaches |
|---|---|
| The root namespace: `init`, `service-mgr`, both login supervisors, the view broker, declared services | `/svc/storage`, which today is the tables |
| A session or an application | nothing yet: `/dev/storage` and `/storage` are C.6's |

The service holds no syscaps and binds nothing; `init` binds it. It will need `BIND_NAMESPACE`
when it builds a namespace per mount (C.5b).

## 8. What the gates prove

| Gate | What it asserts |
|---|---|
| `check-live` | The storage service says the boot is a live one. It is the only boot whose root is on a RAM disk, so the only one where the rule's input is real |
| `test-qemu` (`boot-probe`) | `block` is held, so a subscription to it is refused. `/svc/storage/info/all.tsm` has a row per block record in registry order, which is the manager's replay reaching its owner whole. `nitrox-root` is the one row mounted at `/`, `init`'s, writable ext4, with `clean` `Null`. The service mounted nothing. The ESP reads as FAT and the disk as holding no filesystem. The directory lists `all.tsm` and a file per device, and a suffix the service does not serve is `NotFound` |

Host tests hold the rest: FAT against sectors `mformat` wrote and a real protective MBR, ext4
against a filesystem `mkfs` made (and one whose root inode `check_device` refuses), each source
scheme, the live-boot rule and every column's rule.

## 9. Not built, and what that costs

- **Nothing is mounted.** A second disk's filesystem is reported and not reachable (C.5b).
- **An ext4 its server would refuse reads as "no filesystem"**, not as "ext4, which this system
  cannot serve". Nothing distinguishes the two until a person needs to be told why a disk did not
  mount.
- **Arrivals after `Settled` are logged and ignored.** Nothing sends one until Phase 6's USB driver.
- **Supervision.** `init` keeps the service's process handle, and nothing restarts it. If it
  exited, its subscription would close and `block` would be free for anything in the root
  namespace to take (`TODO(svc-auth-ungated)`).

## Where to read more

- [`administration.md`](../planning/administration.md) § *Storage* and § *Part C in detail* — the
  design and its reasons
- [`device-manager.md`](device-manager.md) — where the disks come from
- [`filesystem-data-path.md`](filesystem-data-path.md) — what a mounted filesystem's files become
- [`init-toml-schema.md`](../spec/init-toml-schema.md) — the manifest both `init` and this service read
