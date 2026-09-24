# The Device Manager

**Status: built — administration Part B, B.1–B.5, 2026-09-24; last checked 2026-09-24.** What
exists:
- the kernel's device table, readable at `/dev/registry` (B.1);
- `device-mgr`, handing each device to the owner of its class and serving the table as TSM1
  tables (B.2);
- `input-server` as the owner of `input` (B.3);
- `/dev/devices` in every session and application, through an info-only endpoint (B.4);
- every enumeration of block devices reading the registry or its namespace's own bindings (B.5).

What does not exist yet:
- **Anything that sends a later `Arrived` or a `Departed`.** Phase 6's USB driver is the first
  event source.
- **An owner for `block`.** That is Part C's storage service.

The design, and why each piece is shaped as it is, is
[`administration.md`](../planning/administration.md) § *Part B in detail*. The contracts are
[`device-node.md`](../spec/device-node.md) § *The registry* and
[`rsproto-devices-ops.md`](../spec/rsproto-devices-ops.md).

## 1. What it is for

**The manager learns what devices the machine has and hands each to the service that owns its
class.** It does not drive them. It gives the keyboard and the mouse to `input-server`, and it will
give disks to the storage service. Everyone else can read what exists.

Before it, every consumer found its own devices:
- `input-server` opened `/dev/input/raw/0` and `/1` by path, and exited if either was missing;
- three programs probed `/dev/blk/0`, `1`, … until the first miss;
- nothing outside the kernel could list the device table at all.

**Devices move as handles, not paths.** An owner is handed its own duplicate of each device's node,
so an owner that exits cannot take a device from the next one. Phase 6's driver manager is this
component extended: it hands a driver process a `Handle<DeviceNode>` the same way.

## 2. The pieces

| Piece | Where | What it does |
|---|---|---|
| The device table | `kernel/src/device.rs` | Every `DeviceNode`, with its kind, the index its path serves it at, its parent, and what its driver did |
| `/dev/registry` | `kernel/src/object/kernel_server.rs` (`registry_server`) | Root namespace only. The bare path is a snapshot (a header with the record count, then a 144-byte record per node); `/<id>` is that node |
| The reader | `userspace/libkern/src/device.rs` | Reads a snapshot by its count, not its page-rounded size; `DeviceRecord::block_index` gives a block record's `/dev/blk` index |
| `device-mgr` | `userspace/device-mgr/` | The library decides: names, classes, owners, tables, and what a suffix may ask for where it arrived. The binary is the event loop |
| The `Devices` protocol | `userspace/librsproto/src/devices.rs` | `Arrived`, `Settled` and `Departed`, category `0x0Fxx` |
| The input owner | `userspace/input-server/` | Subscribes to `input`, and reads up to eight devices |
| `/dev/devices` in a session | `userspace/libsession/`, `userspace/desktop-shell/` | Bound at the base `/info`, in every session and every application |

## 3. A boot, end to end

1. **The drivers register their nodes.** The PCI functions come first, then the disks, partitions
   and RAM disk as the drivers publish them, then the console and the i8042's keyboard and mouse.
   The table is append-only, and a block node's served index is the number of block nodes before
   it.
2. **`init` spawns `device-mgr`**, after the view broker and before the display arm. The manager
   reads `/dev/registry` once. Every node registers before userspace starts, so one read is
   complete coldplug. It then takes each class device's node from `/dev/registry/<id>` and answers
   `Meta::Ready`. A manager with no registry to read refuses instead, and `init` prints its reason.
3. **`init` binds `/svc/devices`**, then resolves `/svc/devices/info-endpoint` for the endpoint it
   will courier to the sessions (§5).
4. **`init` spawns `input-server`, which resolves `/svc/devices/input`.** The manager has already
   queued the whole replay on the channel it answers with: an `Arrived` per keyboard and mouse,
   each carrying the owner's duplicate of its node, then `Settled`. So `input-server` holds every
   device the moment its resolve completes. It arms a read on each, answers `Meta::Ready`, and
   `init` binds `/dev/input/new`, as before the manager existed.
5. **A person types `list /dev/devices`**, or opens `/dev/devices/all.tsm` and filters it. The
   resolve reaches the manager as `info` or `info/all.tsm`, and the shell decodes the table with no
   device code of its own.

## 4. Classes and owners

**The classes are the manager's**, derived from a record's kind, because the kernel's `DeviceClass`
calls the console, the keyboard and the mouse all `Char`:

| Class | Kinds | Owner |
|---|---|---|
| `input` | keyboard, mouse | `input-server`, from boot on |
| `block` | disk, partition, RAM disk | nobody until Part C's storage service |

A console or a PCI function is in no class, and is only ever information.

**A class has one owner at a time.** While an owner's channel is open, a second subscription is
refused with `AlreadyExists`, and the class is free once the channel closes. This is the kernel's
rule kept at the manager: each raw input device has one ring and one parked reader, so a second
reader would drain events meant for the first. The manager learns of a close from its own wait, so
a subscription sent straight after one may still be refused, and the caller retries.

**Settling is part of subscribing.** The manager queues the replay and its `Settled` *before* it
answers the resolve. A completed subscription is therefore whole, and `Settled`'s count never
depends on how soon the owner reads. The first version replied first, and a probe that closed at
once was counted as having been sent nothing. The channel is sized from the replay (its devices, the
`Settled`, and 32 spare, up to `IPC_MAX_QUEUE_DEPTH`), because the sends do not block. A fixed
depth would have cut a large class's replay short, and `Settled` would have counted only what fit.

**`input-server` serves from `Settled` with whatever arrived.** That includes none, or a keyboard
alone, where it used to exit. It keeps up to eight devices in slots, merges them by group start,
and forwards a wakeup's merge as batches that end on group boundaries. A `Departed` retires a
slot; nothing sends one until Phase 6, so that path is host-tested. **There is no fallback to the
raw paths**, since a path that runs only when the first is broken is a path nobody tests.
[`input-subsystem.md`](input-subsystem.md) §5 has the input side.

## 5. The information side

`info` is a directory of TSM1 tables: `all.tsm` with every device a row in registry order, then one
`<name>.tsm` per device. **A device's name is its path's**, so a name says which binding would
reach it:
- disks, partitions and RAM disks are `blk-<n>` for `/dev/blk/<n>`;
- keyboards and mice are `input-<n>` for `/dev/input/raw/<n>`;
- the console is `console`;
- a PCI function is `pci-<bus>.<dev>.<fn>`.

The `<n>` is the record's served index, never a count within a class: the console registers first,
so counting within `Char` would have called the keyboard `input-1`.

The columns are `name`, `kind`, `path`, `size`, `description`, `parent` and `driver`. **What a
device does not have is `Null`, not zero**, because a keyboard has no size, which is different from
a size of nothing. A table is minted as a fresh read-only memory object per resolve and is
page-sized, and `Table::decode` stops at its terminator.

**A session reaches the tables through an info-only endpoint.** Resolving
`/svc/devices/info-endpoint` answers a channel that is itself a forwarding endpoint, and on it the
manager answers the directory and the tables and nothing else, whatever suffix arrives. `init`
resolves one at boot and couriers it down the same chain as the view broker's: `service-mgr`,
both login supervisors, then `desktop-shell`. Each binds it at `/dev/devices` with the base
`/info`.

**The endpoint, not the base, is the boundary.** The base keeps an ordinary session's suffixes
under `info`. But `desktop-shell` holds what is couriered, and `BIND_NAMESPACE`, so it could bind it
with no base. On the endpoint bound at `/svc/devices`, a bare `block` is a subscription to every
disk: raw write access to the ESP and every partition, which nothing else the shell holds gives. On
the info-only endpoint it is `NotFound`, however it is bound.
[`namespace-and-resource-servers.md`](namespace-and-resource-servers.md) § *Userspace Servers*
records the shape: attenuation by construction, for authority no right on a handle can express.

## 6. Who can reach what

| Holder | Reaches |
|---|---|
| The root namespace: `init`, `service-mgr`, both login supervisors, the view broker, declared services | `/svc/devices` whole, which is every class to subscribe to, and `/dev/registry` |
| `input-server` | the `input` class, which it owns |
| A session, an application, and `desktop-shell` | the tables, through the info-only endpoint; never a class, never the registry |

The first row is the same ungated boundary `/svc/auth` and `/svc/views` have
(`TODO(svc-auth-ungated)` in [`deferred-decisions.md`](../rationale/deferred-decisions.md)), with
the same fix to come. The manager holds no syscaps and binds nothing; `init` binds it.

## 7. The rest of the system reads the table, not a probe

- **`eshell`'s `lsblk`** reads `/dev/registry`, since the recovery shell runs in the root
  namespace. It prints each block device's path, kind, size and name.
- **`libsession::rebind_block_devices`** hands disks to an installer session or a view. It reads
  the registry when its source has one, which is the root namespace, where `/dev/blk` is one
  kernel-server binding whose children no enumeration can see. Otherwise it reads the source's own
  `/dev/blk/<n>` bindings, which is `desktop-shell` rebinding an installer session's disks.
- **`nxinstall`** lists its own namespace, in which what it may write is exactly what is bound.

None of them stops at a gap, and each takes its indices from what exists rather than from a
counter. `libfs::ns_children` still reports a kernel server's subtree as one binding, and says that
`/dev/devices` is what lists the devices.

## 8. What the gates prove

| Gate | What it asserts |
|---|---|
| `test-qemu` (`boot-probe`) | The registry decodes, and its ids, sizes, names and served indices match the paths that serve them. `block` replays and settles before its resolve completes. A second owner is refused and the class is taken again once closed. `input` is held. An info-only endpoint refuses `block`. `all.tsm` has a row per record. A view's `nxinstall` finds its granted disks |
| `test-interactive` | The manager mints the info-only endpoint before the first login. In a serial session, `list /dev/devices` names the disk and both input devices, a `filter kind == "disk"` prints the disk's model, and `/dev/devices/block` and `/dev/registry` open nothing |
| `check-login` | The graphical session has `/dev/devices`, and each application namespace the shell builds reaches it |
| `check-live` | `/dev/devices` lists the live image's module as a `ramdisk`, the one RAM disk any gate has |
| `check-install` | `desktop-shell` rebinds an installer session's disks from the session's own bindings, and `nxinstall` finds them by listing its namespace |
| `check-input`, with and without `--no-ps2-irq` | Unchanged — and every key and click in them is now read from a node the manager handed over; the events themselves never pass through it |

`eshell` is reached only when the critical path fails, so no gate runs `lsblk`. It was checked on a
one-off boot of a release disk whose root would not mount.

## 9. Not built, and what that costs

- **An event source.** The kernel registers every node before userspace, and nothing registers one
  later. `Arrived` after `Settled` and `Departed` are specified and handled, `input-server`'s side
  in host tests, and sent by nothing until Phase 6.
- **An owner for `block`.** Until Part C, anything in the root namespace can take the class. The
  probe takes it briefly to test the replay.
- **Supervision.** `init` keeps the manager's process handle, and nothing restarts it. If it
  exited, an owner would keep the devices it holds (`input-server` logs that the manager has gone
  and keeps reading), and every `/dev/devices` resolve would fail.

## Where to read more

- [`administration.md`](../planning/administration.md) § *Part B in detail* — the design and its
  reasons
- [`device-node.md`](../spec/device-node.md) — the `DeviceNode`, the registry, the record
- [`rsproto-devices-ops.md`](../spec/rsproto-devices-ops.md) — the protocol and the tables
- [`input-subsystem.md`](input-subsystem.md) — the input owner
- [`session-and-auth.md`](session-and-auth.md) — what a session namespace holds
- [`drivers-and-irps.md`](drivers-and-irps.md) — how the table is filled
