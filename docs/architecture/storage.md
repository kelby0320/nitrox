# Storage

**Status: built as administration Part C drew it, C.1–C.8 — 2026-09-25; started and bound by
`service-mgr` since Part E.1a, its session endpoint handed to sessions as `service-mgr`'s route
since Part E.1b; its shutdown unmount built with Part E.4a; the installer's source passed over
since Part G.1; a `description` column since the laptop polish's Part C; devices that arrive and
depart, and the disk the machine started from, since Phase 6 Part D; FAT read by its server's check
and served by `fs-server-fat` on a removable disk since Phase 6 Part E; removable media for a
session — a stick writable on any boot, `Eject` on a media session, the watch and the `removable`
column — since Phase 6 Part F; `Reread`, a USB disk's rescan, a watch pinged for the devices too,
and the `note` column since Phase 6 Part G; last checked 2026-10-07.**
What exists:
- `storage-service`, the owner of `block`. It reads what each disk, partition and RAM disk holds,
  and which of them `init` mounted, and serves that as TSM1 tables at `/svc/storage/info` (C.5a).
- **Mounting** (C.5b): every ext4 `init` did not mount is auto-mounted, and every FAT on a
  removable disk (Phase 6 Part E) — an internal disk's read-only on a live boot, a removable one's
  writable on any (Part F) — with the server for its kind —
  `fs-server-ext4` or `fs-server-fat` — and a namespace of its own, and named by its label — bar the
  installer's source, a partition named `nitrox-source` (§6).
  `/svc/storage/fs/<label>/…` answers with `SUBNAMESPACE`, so a resolve continues in that
  namespace.
- **The session endpoint** (C.5b): minted at `/svc/storage/session-endpoint`, answering the
  filesystems and the tables — and since Phase 6 Part F a media session and a watch — and nothing
  else.
- **Removable media for a session** (Phase 6 Part F, §8a): a **media session** carries `Eject`, the
  unmount chain on a removable disk's mount, with no grant; a **watch** is pinged whenever the
  mounts change.
- **The admin endpoint and `Storage`** (C.5c): `Mount`, `Unmount` and `InUse` on an admin session
  ([`rsproto-storage-ops.md`](../spec/rsproto-storage-ops.md)). An unmount writes back every dirty
  file, is refused while a file is still held, and flushes the drive. **And `Reread`** (Phase 6 Part
  G, §8b): a device `disk` has written, read again — a USB disk rescanned by the kernel first — and
  mounted by an arrival's rules.

- **Sessions and views** (C.6): every session and application has `/storage` and `/dev/storage`,
  the view broker's `storage` grant binds the admin endpoint at `/dev/storage/admin`, and its
  `disks` grant leaves out what `InUse` names.
- **`disk`** (C.7), the coreutil a person runs: `disk --list` and, since Phase 6 Part F,
  `disk --eject` from any session, and `disk --mount` and `disk --unmount` in a view with the
  `storage` grant — and since Part G `disk --partition` and `disk --format`, with the `disks` grant
  too ([`shell-language.md`](../spec/shell-language.md) §10d).
- **`check-storage`** (C.8), the gate whose verdict is a disk the host reads (§11).

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
| The FAT checks | `userspace/fs-server-fat/src/volume.rs` | `Fat::check`, the label and `was_left_clean`, for the same reason; the check's refusal is the reason a FAT is not served (Phase 6 Part E.5) |
| The partition-table reader | `userspace/libgpt/` | Each disk's entries, for `init.toml`'s UUID sources |
| The `Storage` codec | `userspace/librsproto/src/storage.rs` | `Mount`, `Unmount` and `InUse` bodies ([`rsproto-storage-ops.md`](../spec/rsproto-storage-ops.md)) |
| The busy check | `kernel/src/syscall/table.rs` (`sys_ns_held`) | How many of a mount's files something still holds, after the finished IRPs have let go |

## 3. A boot, end to end

1. **`init` mounts its critical path**, and `service-mgr` spawns the device manager and, **straight
   after it**, the storage service, in the declarations' order (`init` spawned both until
   administration Part E.1a; this step said so until 2026-09-29). A class's owner is whoever
   subscribes first, so the service must come before anything else that could subscribe
   (`TODO(svc-auth-ungated)` in [`deferred-decisions.md`](../rationale/deferred-decisions.md)).
2. **The service takes `block`**: it resolves `/svc/devices/block`, and the manager has already
   queued an `Arrived` per disk, partition and RAM disk, each carrying the service's own duplicate
   of the device's node, then `Settled` ([`device-manager.md`](device-manager.md) §4). The
   subscription stays open for the life of the service; closing it would free the class.
3. **It reads each device** (§4), and each disk's partition table.
4. **It reads `/initramfs/etc/init.toml`** and matches each of `init`'s mounts to its device (§5).
5. **It mounts what it can serve** (§6), and keeps every device's node, mounted or not.
6. **It says what it found**, a line per device, then answers `Meta::Ready`, and `service-mgr`
   binds it at `/svc/storage` (`init` did until administration Part E.1a). A resolve there is
   continued twice — into `service-mgr`'s registry, then into a mount's namespace — two of the
   four continuations the kernel allows.

On a `test-qemu` boot the log reads, the RAM disk being the test image's scratch filesystem and the
last two the USB stick's disk and partition
([`qemu-integration-tests.md`](../conventions/qemu-integration-tests.md)):

```
storage-service: 6 block device(s)
storage-service: blk-0 (disk QEMU HARDDISK (QM00001)): no filesystem; on the disk the machine started from, passed over
storage-service: blk-1 (ramdisk module 1 (/boot/scratch.img)): ext4 'nitrox-scratch'; mounted at /storage/nitrox-scratch (rw)
storage-service: blk-2 (partition NITROX_ESP): fat 'NITROX_ESP', left clean; not served: 512-byte clusters, smaller than a page
storage-service: blk-3 (partition nitrox-root): ext4; init's at / (rw)
storage-service: blk-4 (disk QEMU QEMU HARDDISK (1-0000:00:03.0-3)): no filesystem
storage-service: blk-5 (partition partition 1 (unlabelled)): fat 'NXSTICK', left clean; not served: 512-byte clusters, smaller than a page
storage-service: not a live boot
```

(This showed four devices, and the ESP as `fat 'NITROX_ESP'` alone, until Phase 6 Part E; the
stick and the disk's line came with Part D.)

## 4. What a device holds

- **ext4**, if `fs-server-ext4`'s `check_device` accepts it: the superblock parses and inode 2 is
  a directory. That is the check the server runs before it says Ready, so a device the service
  calls ext4 is one a server would serve. Its label (`s_volume_name`, empty when `mke2fs` was
  given no `-L`, as the image builder's is) and whether it was left clean come with it.
- **FAT**, read by `fs-server-fat`'s own check (Phase 6 Part E.5), as ext4 is by its server's:
  its label, whether it was left clean, and, if the server would refuse it, why — in the refusal's
  own words ([`fat-fs-server.md`](fat-fs-server.md) §2). **A boot sector the library cannot parse is
  still a FAT if it carries the four marks every formatter writes**: the `0x55AA` signature, a
  sector size from 512 to 4096, the extended boot signature `0x29`, and the type string after it.
  So a FAT with 4 KiB sectors is reported refused for them rather than as nothing. A GPT disk's
  protective MBR carries the signature and none of the rest, so a whole disk is not mistaken for a
  filesystem.
- **Nothing**, otherwise: a disk holding a partition table, a blank one, or a filesystem neither
  reader can read. An ext4 its server would refuse, a 64-bit one say, is also "nothing" today (§12).

**A whole disk whose first sector carries a partition entry is never a filesystem of its own**
(PR #365 review, finding 4): signed `0x55AA`, every entry's status `0x00` or `0x80`, one entry in
use on the disk (`probe::probe_record`). Partitioning a stick that held a filesystem whole leaves
that filesystem's bytes, since `sfdisk` and `parted` write the entries into sector 0 and keep the
rest. **An ext4** keeps its superblock at byte 1024, beside the partitions the kernel publishes, and
was mounted whole over them; such a disk now holds nothing. **A FAT**'s boot sector is sector 0
itself, which the kernel reads as no table (`kernel/src/drivers/partitions.rs`), so no partition is
published; mounted, the stale FAT would allocate clusters inside the partition nothing can see. It
is reported, `not served: its first sector holds partition entries too, so this FAT may be stale`,
and mounted by nothing. Linux reads such a sector as a partition table; this kernel keeps Phase 6
Part D's reading, by the maintainer's call (2026-10-07): what FAT is for here is a stick formatted
FAT32 as sold, an MBR and a partition, and nothing requires the older shapes Linux reads.

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

**`init`'s mounts stay `init`'s**: reported, never mounted again, never unmounted by this service.
`init` unmounts them itself at a shutdown (Part E.4a).

**If they cannot all be placed, this service mounts nothing.** Sometimes `init.toml` does not read,
names no mount, or names one that matches no device. Then a device this service takes for free
could be the running root. So it auto-mounts nothing, refuses an administrator's `Mount`
(`NoAccess`), and does not answer `InUse`, which makes the view broker refuse `disks` (PR #336
review, finding 2). Every image names its root by partition label, so a boot that gets here has
already gone wrong somewhere.

**A live boot is one whose root is on a RAM disk**, through a partition of one or as the disk
itself. That is the fact that makes the machine's own disks the install target, and it is what
makes an internal disk's auto-mount read-only (§6). A root matched to no device is not a live boot.

## 6. Mounting

**What is mounted at boot is every ext4 that is not already mounted, and every FAT on a removable
disk** (Phase 6 Part E.5): `init`'s mounts are never mounted again. **Removable** is a disk behind
USB mass storage — its record's driver is `usb-storage` — or a partition of one; a SATA disk never
is, whatever its bay. **An internal disk's FAT is most likely its ESP**, which nobody asked to have
mounted; the report says `not removable, so not mounted`, and an administrator's `disk --mount`
takes one (§8). A FAT its server would refuse is mounted by neither, and the report says why:
`not served: 512-byte clusters, smaller than a page`, which every Nitrox ESP is.

**Nor anything on the disk the machine started from** (Phase 6 Part D): the disk the kernel flags
`BOOT` in its record, its GPT's GUID being the one Limine loaded the modules from, and its
partitions. On a live boot that is the stick, which holds the system running from RAM; on an
installed machine the internal disk, whose root is `init`'s anyway. The service's report says
`on the disk the machine started from, passed over`.

**Bar one more: the installer's source** (administration Part G.1). A partition named
`nitrox-source` (`libgpt::INSTALL_SOURCE_LABEL`) is `install-root.img`'s, the pristine root the live
stick's install entry loads for `nxinstall` to copy. Mounted, it would be in use, and the `disks`
grant would withhold it from the installer. **The rule is the name, not "a RAM disk"**: a test
image's scratch filesystem is a RAM disk this service mounts for `boot-probe`. The service's report
of the device says so: `the installer's source, left unmounted`. **On a live boot an internal disk's
auto-mount is read-only**, since the machine's own disks are the install target and nothing written
to one by accident could be taken back. **A removable disk's is writable on any boot** (Phase 6
Part F): a stick is not the install target, and a person plugs one in to write to it. (Every
auto-mount was read-only on a live boot until Part F.) An administrator's explicit mount (C.5c) is
writable either way; it is the automatic one that has to be careful.

**A mount is named by a label**: the filesystem's own, else its partition's name, else `blk-<n>`,
taking the first that is valid. A valid label is 1 to 64 bytes of printable ASCII with no `/`, not
beginning with `.` or a space and not ending with one. A clash takes `-2`, `-3`, … in registry
order, so the first device found keeps the plain name.

**Mounting a device**:
1. Duplicate its node for the server, **narrowed to the mode**: a read-only mount's server holds
   no `WRITE` on the device, whatever its own read-only mode does above that.
2. Spawn the server for what it holds — `/bin/fs-server-ext4` or `/bin/fs-server-fat`, the store's
   copies since the root is mounted by now — and send it the setup message: the device, and the
   read-only flag for `ro` ([`ext4-fs-server-rw.md`](ext4-fs-server-rw.md),
   [`fat-fs-server.md`](fat-fs-server.md)). Both speak `libfsserver`'s protocol.
3. Wait, bounded as `init` waits, for its `Meta::Ready`, and take the endpoint it carries.
4. Create a namespace and bind that endpoint at its `/`. This is why the service holds
   `BIND_NAMESPACE`: it binds only into namespaces it created (§10).
5. **Keep the server's control channel.** It carries `Meta::Unmount` (§8). The service waits on
   it meanwhile, since its closing means the server has gone, and the mount goes with it rather
   than leaving a label every resolve under would fail.

**The service keeps every device's node**, not only a mounted one's: an administrator may mount
any of them (§8), and an unmount flushes the drive through it.

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

## 6a. Devices that arrive and depart

*(Phase 6 Part D.)* **After `Settled`, the device manager sends an `Arrived` or a `Departed` for a
disk that comes or goes** — a USB stick, its disk and each of its partitions as records of their own
([`device-manager.md`](device-manager.md) §3a).

**An arrival is read as at boot**: what it holds, through its node, which the service keeps. It is
mounted by the boot's rules — ext4, or FAT on a removable disk, not on the boot medium, not the
installer's source, an internal disk read-only on a live boot — beside the mounts there are: **only
the new device is planned**, so one an administrator unmounted stays unmounted, and a label in use
is numbered past as at boot. It is reported in the same line. Past `MAX_MOUNTS`, eight, it is said
and left unmounted.

**A departure takes the device out of the table and closes the service's node.** A mount on it is
torn down as an unmount is, but with nothing kept:
1. its label leaves `fs`;
2. `sys_ns_sync` over the mount: every dirty file's write-back is refused `PeerClosed`, and the
   kernel lets each such file go rather than keeping it for the boot
   ([`filesystem-data-path.md`](filesystem-data-path.md));
3. `Meta::Unmount`: the server's attempt to record the filesystem clean fails, and it answers and
   exits — **not a terminate**, which is a request neither filesystem server reads;
4. the namespace is closed.

The log says `<label> left while mounted; what had not been written back is gone`, then
`blk-<n> departed`. A stick pulled without an eject is left marked in use, which is what an eject
(§8a) exists to prevent. A departure that took a mount pings every watch, as any change does.

## 7. The session endpoint

**Sessions reach the service through an endpoint of their own**, as they reach the device manager.
Resolving `/svc/storage/session-endpoint` on the root endpoint mints a forwarding endpoint, and on
it the service answers `info…` and `fs…` and nothing else — `info/media` and `info/watch` among them
since Phase 6 Part F (§8a), neither of which mounts anything. `service-mgr` resolves one each time
the service comes up, binds it in its registry, and hands the login supervisors a **route** to it
(administration Part E.1b; the supervisors resolved one each until then). They bind that twice
into every session:
- at `/storage` with the base `/fs`, so `/storage/<label>/…` is the filesystem;
- at `/dev/storage` with the base `/info`, so `/dev/storage/all.tsm` is the table.

**The endpoint, not the base, is the boundary.** A holder with `BIND_NAMESPACE` could bind it with
no base, and on the root endpoint that would let it resolve `session-endpoint` and mint more. On a
session endpoint that suffix is `NotFound`, however it is bound, and the route in front of it
reaches that endpoint and nothing else. Four session endpoints can exist at once; since Part E.1b
`service-mgr`'s is the only one, the rest headroom.

## 8. Mounting and unmounting by request

**An admin session carries `Storage` requests** ([`rsproto-storage-ops.md`](../spec/rsproto-storage-ops.md)).
The root endpoint mints an admin endpoint at `admin-endpoint`, and any resolve on that endpoint
opens a session. A session endpoint refuses the suffix, so a session cannot reach mounting at all.
Who holds an admin endpoint is the view broker's `storage` grant to decide (C.6); the service
answers every request on a session.

- **`Mount`** names a device as the tables do, `blk-<n>`, and optionally a label. It is always
  writable, on a live boot too. It is refused for a device already mounted, `init`'s included, for
  one holding nothing the service serves — a FAT its server would refuse among them — and for a
  label that is invalid or taken. **An internal disk's servable FAT is mounted here**, the rule
  that leaves it alone being the automatic mount's.
- **`InUse`** is every mounted device and the disk that holds it: what the `disks` grant must not
  hand out raw, since a raw write to a disk reaches its partitions. **And the disk the machine
  started from** (Phase 6 Part D), whether or not anything on it is mounted: on a live boot the
  stick, which `nxinstall` then names as holding the running system.
- **`Unmount`** is a chain, each link only once the one before it held:
  1. The label leaves `fs`.
  2. Every dirty file is written back: `sys_ns_sync` on the mount's namespace. That includes a
     file a writer mapped, wrote and let go of without a sync.
  3. **Refused while a file is still held**: `sys_ns_held`, asked after the sync, when what is
     left is someone's. A handle or a mapping could write after the filesystem is marked clean.
     A refusal here puts the label back and changes nothing.
  4. `Meta::Unmount`: the server records the filesystem clean and exits. A read-only mount records
     nothing, since it never marked the filesystem mounted, and leaves it as it was found. The
     service's line says what the device says, read again after the server has gone.
  5. `IoOpcode::Flush` on the device, for a writable mount.
  6. The namespace is dropped.

**The count is asked up to three times, 5 ms apart, before it is believed.** A block IRP pins the
file whose frames it moves, and a finished IRP's box is freed only in thread context. The first
unmount after a write was refused because the write-back's IRPs still held the file they had just
written. The kernel now frees finished IRPs before it counts. But an IRP whose completion is
still running on another CPU wakes the sync before it parks its box, and for that moment its file
counts. A real holder is still holding milliseconds later; that moment has passed.

## 8a. Ejecting, and following the mounts

*(Phase 6 Part F.)* **A session can eject a stick and learn when the mounts change**, through the
session endpoint every session already holds, with no grant. Neither channel mounts anything.

**A media session**, `/dev/storage/media` (`info/media`), is opened for a request and closed after.
It carries **`Eject`**, naming a mount by **the name it is mounted under** — the table's `mounted`
column after `/storage/`, never its `label`, which two sticks can share and one can lack
([`rsproto-storage-ops.md`](../spec/rsproto-storage-ops.md)). **The drive goes whole** (PR #367
review): a stick is pulled whole, so every filesystem the service mounted on the same disk goes, or
none does. Each is written back and asked whether a file is held before any is unmounted; then
each runs §8's unmount chain, and the service answers, with every name it unmounted, once the
stick can be pulled. Its log says `ejected <name>, <name>…, safe to remove`. (Until the review an
eject unmounted the one filesystem named and said the stick could be pulled, with its other
partitions still mounted.) It refuses:
- a name nothing is mounted under, by this service, `NotFound` — `init`'s mounts included, wherever
  they sit;
- a mount on a disk that is not **removable** (§6), `NoAccess`, naming `with admin disk --unmount`:
  an internal disk stays the `storage` grant's to unmount;
- a stick a file on which — on any of its filesystems — is still open or mapped, `WouldBlock`, with
  every one of them left mounted.

The service waits on each media session, so they take slots in its wait set: **two**, leaving nine
for directory sessions. With both in use a resolve is answered `WouldBlock`, and `disk --eject`
says to try again. **An ejected stick stays unmounted until it is plugged in again**: a session
cannot mount one (§12). Eject is the chain and the flush; no `START STOP UNIT` goes to the device.

**A watch**, `/dev/storage/watch` (`info/watch`), is a channel the service **only sends on**: a bare
`Changed`, whenever the names mounted under `/storage` differ after a turn of its loop from before
it — an auto-mount, an eject, an administrator's mount or unmount, a departure, a shutdown's
unmount — **or the devices there are** (Phase 6 Part G): a partition a rescan published holding
nothing changes no mount, and `disk --partition` waits for its row (§8b). The client then reads
`all.tsm` again; the table stays the one answer to what is mounted.
- **It takes no slot in the wait set**, so a client holds one for its life at no cost to anyone
  else's. Files does, one per process.
- **A full queue is a ping already waiting**, since a watch carries nothing else, so a watcher that
  falls behind loses nothing.
- **A watcher that has gone is found by the ping that fails** `PeerClosed`, and dropped then.
- **Thirty-two are held at most**, the machine's and not a session's, since the service cannot tell
  sessions apart. With all held, a new watch pings every one first, which costs the living a
  needless read of the table and frees the dead; it is refused `WouldBlock` if none had gone, and
  Files then reads the table at a window's opening and focus instead.

Comparing the names and the devices once per turn, rather than at each place that mounts, unmounts
or follows a device, is what keeps a path from missing its ping; a refusal changes nothing, and
pings nobody.

## 8b. Formatting, and reading a device again

*(Phase 6 Part G.)* **`disk --partition` and `disk --format` write the device themselves**, through
the raw device the `disks` grant gives, as `nxinstall` does, so the long writes stay out of this
service, which is one thread ([`shell-language.md`](../spec/shell-language.md) §10d). Then `disk`
sends **`Reread`** on an admin session, naming the device as the tables do, and this service reads
it again and mounts what it finds **by an arrival's rules** (§6a): a stick writable, an internal
ext4 as at boot, an internal FAT not at all.

- **A partition** is probed again: its window is unchanged, so what is in it is all that is new.
- **A disk is rescanned first**: `IoOpcode::Rescan` on its node
  ([`io-operation.md`](../spec/io-operation.md)), which a USB disk answers by retiring its
  partitions' windows, letting what they forwarded drain, departing their records, reading its table
  as at its arrival, and publishing what it finds ([`usb.md`](usb.md)). The new partitions reach
  this service as arrivals, **after the reply**, and are mounted as arrivals are. Every other disk
  answers `Unsupported`: an internal disk is partitioned by `nxinstall`, which reboots after.
- **The reply names what was mounted**, one per line, and nothing for a disk, whose partitions come
  later.

**A mounted device is never read again**, nor formatted: unmount it, format it, and this service
mounts it again, the procedure everywhere. `Reread` is refused:
- `WouldBlock` while the partition is mounted, or, for a disk, while anything on it is — a
  partition beside a mounted sibling is read again, as the `disks` grant gives it out (PR #368
  review);
- `NoAccess` for anything on the disk the machine started from — mounted is asked first, so the
  disk holding `init`'s root says `WouldBlock`;
- `NotFound` for a name nothing has, and `NoAccess` until `init`'s mounts are known (§5);
- `Unsupported` where the kernel cannot rescan, and `IoError` where the rescan fails — the disk
  never quiet, or its table unreadable, the old partitions departed either way.

**`disk` waits for a disk's new partition on a watch** (§8a), bounded at 30 seconds, since the
rescan's arrivals come after the reply: after `--format`, its row mounted or saying why not; after
`--partition`, its row holding nothing, whose `blk-<n>` it prints for the `--format` that follows.
The watch is pinged for a change in the devices as well as the mounts for this.

## 9. The tables

`/svc/storage/info` is a directory of TSM1 tables, like `/dev/devices`: `all.tsm` with a row per
block device in registry order, then one `<name>.tsm` per device. **A name is `/dev/devices`' own**,
`blk-<n>` for `/dev/blk/<n>`, so a row in either table names the same device.

| Column | Type | Holds |
|---|---|---|
| `name` | string | `blk-<n>` |
| `kind` | string | `disk`, `partition` or `ramdisk` |
| `description` | string, nullable | what the device calls itself: a SATA disk's model and serial, a RAM disk's module, a partition's name in its table (2026-10-01) |
| `size` | int, nullable | bytes |
| `filesystem` | string, nullable | `ext4` or `fat` |
| `label` | string, nullable | the filesystem's own label |
| `mounted` | string, nullable | where |
| `by` | string, nullable | `init` or `storage` |
| `mode` | string, nullable | `ro` or `rw` |
| `clean` | bool, nullable | whether an ext4 or a FAT was left cleanly unmounted |
| `removable` | bool | whether the device is a disk behind USB mass storage, or a partition of one (§6): what Files decides an eject button by (Phase 6 Part F) |
| `note` | string, nullable | **why a filesystem found is not mounted**, in the words of the service's log line: `not served: <its server's reason>`, `not removable, so not mounted`, `on the disk the machine started from, passed over`, `the installer's source, left unmounted`. Null for a mounted filesystem, one nothing kept from being mounted but a person, and a device holding nothing (Phase 6 Part G) |

**`clean` is `Null` for a filesystem mounted writable.** A writable mount marks the filesystem in
use before it answers Ready ([`ext4-fs-server-rw.md`](ext4-fs-server-rw.md),
[`fat-fs-server.md`](fat-fs-server.md) §6), so that state says "in use",
because it is, and nothing about how the filesystem was left. The service's log line follows the
same rule.

**`note` and the log line come from one function** (`storage_service::mounts::unmounted_why`), so
they cannot disagree. The log gives its reason for a device holding nothing too — the boot stick's
disk is passed over — where the table's row already says it holds nothing.

**A table reads the disks again.** When a table is read, and before an administrator's mount,
every device nothing has mounted is probed afresh. Anything may have written it since the boot:
this service's own mounts, or a raw writer through the `disks` grant. Until PR #336's review the
boot's probe was the only one, so a disk unmounted clean went on reading as not clean, and one
whose writable server exited read as it had at boot. A read-only mount writes nothing, so a
filesystem it found not clean is still not clean after it. `check-storage` starts from a disk
marked not clean to show both.

## 10. Who can reach what

| Holder | Reaches |
|---|---|
| The root namespace: `init`, `service-mgr`, both login supervisors, the view broker, declared services | `/svc/storage` whole: the tables, every mounted filesystem, and a session or admin endpoint to mint |
| A holder of a session endpoint | the tables and every mounted filesystem, never another endpoint |
| A holder of an admin endpoint | admin sessions: `Mount`, `Unmount`, `InUse`, `Reread` |
| A session, and every application `desktop-shell` launches | `/storage` (base `/fs`) and `/dev/storage` (base `/info`): every mounted filesystem and the table, through a session endpoint, so nothing to mount with — and since Phase 6 Part F a media session, which ejects a removable disk and nothing else, and a watch |
| A view with the `storage` grant | also `/dev/storage/admin`: the admin endpoint the view broker resolved |
| A view with the `disks` grant | every block device **not** in use: the broker asks `InUse` first, and refuses the request if the service cannot answer |

**The root namespace reaches mounting**, since anything holding it can mint an admin endpoint. That
is the same ungated boundary `/svc/devices` and `/svc/views` have, the same trusted set of system
services, and the same fix to come (`TODO(svc-auth-ungated)` in
[`deferred-decisions.md`](../rationale/deferred-decisions.md)).

**Every mounted filesystem is writable by whoever reaches it**, read-only mounts aside: a mount's
namespace binds its server with no narrowing, as `init`'s mounts are bound. That is the plan's
choice for one laptop with one person at it ([`administration.md`](../planning/administration.md)
§ *Storage*), and per-session visibility under `/storage` is what narrows it later.

**The service holds `BIND_NAMESPACE`**, since C.5b. It builds a namespace per mount and binds into
nothing it did not create, which is the view broker's reconciliation
([`userspace/CLAUDE.md`](../../userspace/CLAUDE.md) § Capability discipline). `service-mgr`
binds the service itself at `/svc/storage`.

## 11. What the gates prove

| Gate | What it asserts |
|---|---|
| `check-live` | The storage service says the boot is a live one. It is the only boot whose root is on a RAM disk, so the only one where the rule's input is real. **And it passes the stick over** (Phase 6 Part D): mass storage makes the stick a disk, the kernel flags it as the one the machine started from, and the service reports it so |
| `check-storage` | **The whole chain, with the host holding the result.** The test live image boots as a USB stick beside a copy of the release disk on the AHCI controller, **its root marked not cleanly unmounted first**, as an installed machine's is. The disk's `nitrox-root` is reported not clean and auto-mounted read-only, the boot being a live one, and `test-pattern --write` there is refused `NoAccess`; **`disk --eject nitrox-root` is refused**, naming `with admin disk --unmount`, since it is not removable (Phase 6 Part F). **The table's `clean` says no, and still says no after the read-only unmount**, which the service logs as "not left clean (read-only, so as it was found)". `with admin disk` mounts it writable. `test-pattern --write` writes a pattern through a mapping and exits without a sync, and **the host, reading the disk meanwhile, finds the file at its size without the pattern and the superblock marked mounted**. `test-pattern --check` reads it back through `/storage`, and `with admin disk --unmount` runs the chain, **after which the table says clean**. With the machine stopped, the host carves the partition out: `e2fsck -fn` clean, `s_state` clean read from the superblock's bytes, and the file holding the pattern, read with `debugfs`. **Then sticks plugged in over QMP** (Phase 6 Part D): the boot stick reported passed over; an MBR stick with **two** ext4 partitions, each auto-mounted **writable** with no remount (read-only until Phase 6 Part F) and written without a sync, **an eject of the first refused while `test-pattern --eject-held` holds a file on the second**, both still mounted after it, then both ejected by one `disk --eject` with no password, and pulled, its records departing and the service letting it go — and on the host, carved by its MBR, clean and holding the pattern; a whole-disk stick pulled while mounted writable with a file dirty, the teardown's write-back and the server's marking each answered at once — the kernel letting the dirty file go, the server unable to record the filesystem clean, the service's line — its label gone, a command after it running, and on the host still marked in use; and that stick plugged in again, at a new index, mounted again. **And FAT** (Phase 6 Part E): the copy of the release disk carries a third partition, an internal FAT that `fs-server-fat` could serve, **reported `not removable, so not mounted`**, and the disk's ESP reported refused for its 512-byte clusters; then a 300 MiB FAT32 stick, its data region off a 4 KiB boundary, which `xtask` asserts before the boot, is plugged in and auto-mounted writable by `fs-server-fat`, its long, Unicode and nested names listed and its pattern read through a mapping; a directory made, a copy to a long Unicode name, a rename and a removal, and a file written through a mapping; ejected with `disk --eject` and pulled. On the host, carved by its MBR, `fsck.fat -n` finds it clean with its dirty bit clear, `mdir` lists the guest's names and `mtype` reads its copy, its rename and its pattern. **And formatting** (Phase 6 Part G): `disk --list`'s `note` names the internal FAT's reason; `with admin disk --format` of a partition of the boot stick is refused before anything is written; then a blank 2 GiB stick plugged in is **formatted ext4 whole** — a GPT, the default for ext4, its partition published by the kernel's rescan and mounted writable, written and ejected, and copied — **partitioned again**, an MBR, the GPT's partition departed and a new one arriving holding nothing, and **that partition formatted FAT**, read again and mounted writable, written, the same command refused while it is mounted, ejected and pulled. On the host, the copy holds a GPT with one Linux partition from 1 MiB to the last usable sector, its ext4 clean with the pattern; the stick holds an MBR with one FAT32 partition from 1 MiB, no GPT header at either end, and its FAT clean with the pattern |
| `check-media` | **Removable media on the desktop a person uses** (Phase 6 Part F): the release live image as the boot stick beside a copy of the release disk, logged in at the graphical greeter. Files lists the internal disk's `nitrox-root` in Drives with no eject button; a FAT stick plugged in over QMP is auto-mounted writable and **its row and eject button appear on the screen with nothing typed or moved after the plug**, so the watch woke Files; the editor saves onto it through Save As — Up to `/`, aimed where `libui` lays the button out, then `storage/<stick>/<file>`; **a click on the eject button ejects it**, the chain recording it clean with no password asked, and its row goes. With the machine stopped, `fsck.fat -n` finds the stick clean and `mtype` reads back what was typed |
| `check-install` | The live stick, a disk since Phase 6 Part D and the one the machine started from, is named as the installer's target and refused: it holds the running system, since the service names it in use |
| `test-qemu` (`boot-probe`) | `block` is held, so a subscription to it is refused. `/svc/storage/info/all.tsm` has a row per block record in registry order, which is the manager's replay reaching its owner whole. `nitrox-root` is the one row mounted at `/`, `init`'s, writable ext4, with `clean` `Null`. **The service mounted the scratch disk and nothing else**, writable, at `/storage/nitrox-scratch`. The ESP reads as FAT and the disk as holding no filesystem. **The USB stick's MBR partition reads as FAT `NXSTICK`**, unmounted (Phase 6 Part D), and `xtask` holds the service's line to its reason: `not served: 512-byte clusters, smaller than a page` (Part E). The directory lists `all.tsm` and a file per device, and a suffix the service does not serve is `NotFound` |
| `test-qemu` (`boot-probe`), admin | Through an admin session opened as the view broker will open one: `InUse` names the scratch disk, `init`'s root and its disk, and not the ESP. **An unmount is refused while the `README` is held**, and leaves the mount as it was. **A file written through a mapping and never synced is on the device after the unmount**, which also left the filesystem clean. The label is then gone, a hidden label is refused, and a `Mount` by name brings the filesystem back writable, with the file. `init`'s root, the mounted scratch disk and the ESP are refused, each for its own reason, as is an unknown label. A session endpoint answers `admin-endpoint` with `NotFound`. **And through a session endpoint, a watch and a media session open** (Phase 6 Part F): the watch is pinged by the unmount and by the remount, and not by the unmount refused — checked after a later request's reply, which a ping owed for the earlier turn would precede; `Eject` of the scratch disk, a RAM disk, is refused `NoAccess`, of an unknown name `NotFound`, and neither pings or unmounts. **`Reread`** (Phase 6 Part G): the mounted scratch disk and the disk holding `init`'s root `WouldBlock`, a partition of the boot disk `NoAccess`, an unknown name `NotFound`, none pinging; and the USB stick read again, **its partition replaced by the kernel's rescan and the watch pinged though no mount changed** |
| `test-qemu` (`boot-probe`), grants | **`disks` leaves out what is in use**: `nxinstall`'s listing in the admin view, read back through a stdout pipe, holds the ESP and not the disk holding `init`'s root, the root, or the mounted scratch disk. Not an exit code, since `nxinstall` refuses each of those by its own rules whether granted or not |
| `test-interactive` | The serial session is built with `/storage`. `list /dev/storage` names a table per device, and `open /dev/storage/all.tsm \| filter mounted == "/"` prints the root's row, `init`'s. `list /storage` lists filesystems, not tables, and on a release boot none. **`with admin nxinstall` lists the ESP and never `/dev/blk/0` or the root**, where before C.6 it listed the disk under a live server; since administration Part G.2 a line naming `/dev/blk/0` is only its message on `stderr`, that it holds the running system, and that message must be there |
| `test-qemu` (`boot-probe`), `disk` | **`disk` in the admin view, run as `with admin disk` runs it**: `--unmount nitrox-scratch` writes its one-row table to stdout and exits 0, and the service's table then shows the scratch disk unmounted. `--mount /dev/blk/<n>` writes `blk-<n>`, `nitrox-scratch` and `/storage/nitrox-scratch`, and the table shows it mounted writable again. **With every admin session taken, `disk --unmount` exits 1 and its `stderr` names the service's `WouldBlock`**, not the grant, and the mount stays |
| `test-interactive`, `disk` | `disk --list \| filter mounted == "/"` prints the root's row, `init`'s, in a session with no grant. **`disk --mount` there fails naming the `storage` grant and `with`.** Through `with admin` and a typed password, the same mount reaches the service and is refused by it: the ESP holds a FAT its server refuses for its 512-byte clusters. A release boot has nothing it could mount, so the service's own refusal is the evidence the grant arrived |
| `check-login` | The graphical session has `/storage`, and so does every application namespace the shell builds. In a desktop terminal, `with admin nxinstall /dev/blk/0` is refused: that disk is not in the view at all, and since administration Part G.2 the installer says why — it holds the running system, as the tables report `init`'s mount |
| `test-qemu` (`boot-probe`), mounts | Through `/svc/storage`: `fs` lists `nitrox-scratch` as a directory. Its `README` reads. **A file created, written through a mapping and synced there is on the device**, read back from the RAM disk raw, since a re-resolve would only read the page cache. A session endpoint bound at `/storage` with the base `/fs` and at `/dev/storage` with `/info` reaches the same file and the same table, and bound with no base it mints nothing. An unknown label is `NotFound` |

Host tests hold the rest: FAT against sectors `mformat` wrote and a real protective MBR, and
through its server's check against images `mkfs.fat` made — served, refused for its clusters,
refused for its sectors, found dirty; ext4 against a filesystem `mkfs` made (and one whose root
inode `check_device` refuses); each source scheme, the live-boot rule, every column's rule, every
label rule and clash, what a boot mounts, by which server, and a FAT on a removable disk alone;
what each suffix asks for where it arrives; and since Phase 6 Part G each `Reread` refusal at the
device it is about, a device read again planned as an arrival, and each `note`.

## 12. Not built, and what that costs

- **A session cannot mount a stick again after ejecting it** (Phase 6 Part F): it stays unmounted
  until it is plugged in again, or an administrator mounts it. A second way to mount is filed, with
  a person who ejected by mistake as the trigger (`session-remount` in
  [`deferred-decisions.md`](../rationale/deferred-decisions.md)).
- **Any session can eject any stick**, and every session sees every mount: the service cannot tell
  sessions apart, and nothing gives each its own view of `/storage`.
- **Why a stick did not mount is in the table's `note`** since Phase 6 Part G, which `disk --list`
  shows (`unmounted-why`, answered). **Files does not show it**: its Drives lists mounts, so a stick
  that needs formatting is not in it, and is formatted with `with admin disk --format`.
- **One partition, spanning the disk**: `disk --partition` writes no more, and no sizes. **An
  internal disk is not partitioned** outside `nxinstall`, nor rescanned. **A disk of 4096-byte
  logical sectors is refused** by `disk`, and its partitions are not read. Each is filed in
  [`deferred-decisions.md`](../rationale/deferred-decisions.md).

- **A resolve already on its way to a mount's server can outlive the busy check.** One the kernel
  forwarded before the unmount began may complete after `sys_ns_held` said zero, and hand out a
  file of a filesystem about to be marked clean. It needs a resolve in flight at the moment the
  unmount starts. A lazy unmount, which would drain those, is not built.
- **An open directory session is not a held file.** A client holding one when its filesystem is
  unmounted finds the channel closed.
- **A shutdown unmounts everything** (administration Part E.4). On `CTRL_OP_SHUTDOWN` this service
  runs the chain on every mount it made, last first and without the held check, then exits; `init`
  does the same for its own mounts on the terminal channel's `Finish`. `service-mgr`'s shutdown
  sends both (Part E.4b), when a person runs `with power shutdown` (Part E.4d). `check-shutdown`
  gates it for `init`'s root. A machine turned off without one is still left not clean.
- **An ext4 its server would refuse reads as "no filesystem"**, not as "ext4, which this system
  cannot serve". Nothing distinguishes the two until a person needs to be told why a disk did not
  mount. A FAT does say (Phase 6 Part E).
- **Supervision.** `service-mgr` keeps the service's process handle, and nothing restarts it: its
  policy is `never`, and it is `essential`, so `service --stop` refuses it. If it exited, its
  subscription would close and `block` would be free for anything in the root namespace to take
  (`TODO(svc-auth-ungated)`). (`init` kept the handle until administration Part E.1a; this said so
  until 2026-09-29.)

## Where to read more

- [`administration.md`](../planning/administration.md) § *Storage* and § *Part C in detail* — the
  design and its reasons
- [`device-manager.md`](device-manager.md) — where the disks come from
- [`filesystem-data-path.md`](filesystem-data-path.md) — what a mounted filesystem's files become
- [`init-toml-schema.md`](../spec/init-toml-schema.md) — the manifest both `init` and this service read
