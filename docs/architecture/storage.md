# Storage

**Status: partly built — administration Parts C.5a and C.5b, 2026-09-25; last checked 2026-09-25.**
What exists:
- `storage-service`, the owner of `block`. It reads what each disk, partition and RAM disk holds,
  and which of them `init` mounted, and serves that as TSM1 tables at `/svc/storage/info` (C.5a).
- **Mounting** (C.5b): every ext4 `init` did not mount is auto-mounted, read-only on a live boot,
  with an `fs-server-ext4` and a namespace of its own, and named by its label.
  `/svc/storage/fs/<label>/…` answers with `SUBNAMESPACE`, so a resolve continues in that
  namespace.
- **The session endpoint** (C.5b): minted at `/svc/storage/session-endpoint`, answering the
  filesystems and the tables and nothing else.

What does not exist yet, in the order Part C builds it
([`administration.md`](../planning/administration.md) § *Part C in detail*):
- **The admin endpoint**, the `Storage` protocol, and the unmount chain (C.5c).
- **`/storage` and `/dev/storage` in sessions** (C.6), **`disk`** (C.7), and **`check-storage`** (C.8).

## 1. What it is for

**Raw devices and filesystems are different things.** `/dev/blk/<n>` is bytes, for tools such as
the installer. What a person browses is a filesystem, which will appear at `/storage/<label>` once
mounted. The storage service is where the second comes from: the owner the device manager hands
disks to, which knows what is on each one.

It knows and says, and it mounts what it can serve. Its table is the answer to "what disks does
this machine have, and what is on them", which nothing else can give: `/dev/devices` lists devices,
not filesystems, and `init`'s bindings do not name the devices behind them.

**It does not stand between a program and a mounted filesystem.** A resolve under
`/svc/storage/fs/<label>` reaches the service once, to be answered with the mount's namespace; the
kernel continues it there, to the mount's own server. The file the program gets is installed by
that server's reply into its registration's page cache, and faults and write-backs never pass
through the service ([`namespace-and-resource-servers.md`](namespace-and-resource-servers.md)
§ *A server can hand back a namespace*).

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
3. **It reads each device** (§4), and each disk's partition table.
4. **It reads `/initramfs/etc/init.toml`** and matches each of `init`'s mounts to its device (§5).
5. **It mounts what it can serve** (§6). A device it does not mount has its node closed: the class
   stays the service's through the subscription alone.
6. **It says what it found**, a line per device, then answers `Meta::Ready`, and `init` binds it
   at `/svc/storage`.

On a `test-qemu` boot the log reads, the RAM disk being the test image's scratch filesystem
([`qemu-integration-tests.md`](../conventions/qemu-integration-tests.md)):

```
storage-service: 4 block device(s)
storage-service: blk-0 (disk QEMU HARDDISK (QM00001)): no filesystem
storage-service: blk-1 (ramdisk module 1 (/boot/scratch.img)): ext4 'nitrox-scratch'; mounted at /storage/nitrox-scratch (rw)
storage-service: blk-2 (partition NITROX_ESP): fat 'NITROX_ESP'
storage-service: blk-3 (partition nitrox-root): ext4; init's at / (rw)
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
  reader can read. An ext4 its server would refuse, a 64-bit one say, is also "nothing" today (§11).

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
makes the auto-mount read-only (§6). A root matched to no device is not a live boot.

## 6. Mounting

**What is mounted at boot is every ext4 that is not already mounted**: `init`'s mounts are never
mounted again, and FAT waits for Phase 6's server. **On a live boot every auto-mount is read-only**,
since the machine's own disks are the install target and nothing written to one by accident could
be taken back. An administrator's explicit mount (C.5c) will be writable either way; it is the
automatic one that has to be careful.

**A mount is named by a label**: the filesystem's own, else its partition's name, else `blk-<n>`,
taking the first that is valid. A valid label is 1 to 64 bytes of printable ASCII with no `/`, not
beginning with `.` or a space and not ending with one. A clash takes `-2`, `-3`, … in registry
order, so the first device found keeps the plain name.

**Mounting a device**:
1. Duplicate its node for the server, **narrowed to the mode**: a read-only mount's server holds
   no `WRITE` on the device, whatever its own read-only mode does above that.
2. Spawn `/bin/fs-server-ext4`, the store's copy since the root is mounted by now, and send it the
   setup message: the device, and the read-only flag for `ro`
   ([`ext4-fs-server-rw.md`](ext4-fs-server-rw.md)).
3. Wait, bounded as `init` waits, for its `Meta::Ready`, and take the endpoint it carries.
4. Create a namespace and bind that endpoint at its `/`. This is why the service holds
   `BIND_NAMESPACE`: it binds only into namespaces it created (§9).
5. **Keep the server's control channel.** It will carry C.5c's `Meta::Unmount`. The service waits
   on it meanwhile, since its closing means the server has gone, and the mount goes with it rather
   than leaving a label every resolve under would fail.

**The `fs` side.** `fs` is a directory session listing a subdirectory per mount. `fs/<label>`,
alone or with a path after it, is answered with `SUBNAMESPACE`:
- the mount's namespace, duplicated with `LOOKUP` and the `TRANSFER` every moved handle needs;
- `consumed` covering `fs/<label>`, which ends a component as the kernel requires;
- the base `/`.

So `fs/nitrox-scratch/README` continues as `/README` in the mount's namespace. A label nothing is
mounted under is `NotFound`.

**A reply the service cannot send is answered with an error instead.** A resolve the kernel
forwarded waits for its answer with no deadline, so a reply that silently failed would leave its
caller blocked for good. That is how the first `SUBNAMESPACE` reply failed: its namespace
lacked `TRANSFER`.

## 7. The session endpoint

**Sessions reach the service through an endpoint of their own**, as they reach the device manager.
Resolving `/svc/storage/session-endpoint` on the root endpoint mints a forwarding endpoint, and on
it the service answers `info…` and `fs…` and nothing else. C.6's login supervisors will bind it
twice into every session:
- at `/storage` with the base `/fs`, so `/storage/<label>/…` is the filesystem;
- at `/dev/storage` with the base `/info`, so `/dev/storage/all.tsm` is the table.

**The endpoint, not the base, is the boundary.** A holder with `BIND_NAMESPACE` could bind it with
no base, and on the root endpoint that would let it resolve `session-endpoint` and mint more. On a
session endpoint that suffix is `NotFound`, however it is bound. Four session endpoints can exist
at once: one for each login supervisor, and headroom.

## 8. The tables

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

## 9. Who can reach what

| Holder | Reaches |
|---|---|
| The root namespace: `init`, `service-mgr`, both login supervisors, the view broker, declared services | `/svc/storage` whole: the tables, every mounted filesystem, and a session endpoint to mint |
| A holder of a session endpoint | the tables and every mounted filesystem, never another endpoint |
| A session or an application | nothing yet: C.6 binds the session endpoint into them at `/storage` and `/dev/storage` |

**Every mounted filesystem is writable by whoever reaches it**, read-only mounts aside: a mount's
namespace binds its server with no narrowing, as `init`'s mounts are bound. That is the plan's
choice for one laptop with one person at it ([`administration.md`](../planning/administration.md)
§ *Storage*), and per-session visibility under `/storage` is what narrows it later.

**The service holds `BIND_NAMESPACE`**, since C.5b. It builds a namespace per mount and binds into
nothing it did not create, which is the view broker's reconciliation
([`userspace/CLAUDE.md`](../../userspace/CLAUDE.md) § Capability discipline). `init` binds the
service itself at `/svc/storage`.

## 10. What the gates prove

| Gate | What it asserts |
|---|---|
| `check-live` | The storage service says the boot is a live one. It is the only boot whose root is on a RAM disk, so the only one where the rule's input is real |
| `test-qemu` (`boot-probe`) | `block` is held, so a subscription to it is refused. `/svc/storage/info/all.tsm` has a row per block record in registry order, which is the manager's replay reaching its owner whole. `nitrox-root` is the one row mounted at `/`, `init`'s, writable ext4, with `clean` `Null`. **The service mounted the scratch disk and nothing else**, writable, at `/storage/nitrox-scratch`. The ESP reads as FAT and the disk as holding no filesystem. The directory lists `all.tsm` and a file per device, and a suffix the service does not serve is `NotFound` |
| `test-qemu` (`boot-probe`), mounts | Through `/svc/storage`: `fs` lists `nitrox-scratch` as a directory. Its `README` reads. **A file created, written through a mapping and synced there is on the device**, read back from the RAM disk raw, since a re-resolve would only read the page cache. A session endpoint bound at `/storage` with the base `/fs` and at `/dev/storage` with `/info` reaches the same file and the same table, and bound with no base it mints nothing. An unknown label is `NotFound` |

Host tests hold the rest: FAT against sectors `mformat` wrote and a real protective MBR, ext4
against a filesystem `mkfs` made (and one whose root inode `check_device` refuses), each source
scheme, the live-boot rule, every column's rule, every label rule and clash, what a boot mounts and
how, and what each suffix asks for where it arrives.

## 11. Not built, and what that costs

- **Nothing unmounts.** A mount lasts until its server exits (C.5c).
- **A live boot's read-only auto-mount has no gate yet.** `check-live` has nothing to mount, and
  `check-storage`, which boots a live image beside a SATA disk, is C.8's.
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
