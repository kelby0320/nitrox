# fs-server-fat

**Status: built — Phase 6 Part E, 2026-10-06; PR #365's review fixes 2026-10-07; last checked
2026-10-07.** `userspace/fs-server-fat`
serves FAT12, FAT16 and FAT32 read-write, with long names, over `libfsserver`'s protocol. The
storage service spawns it for a FAT on a removable disk, and for any FAT an administrator mounts
([`storage.md`](storage.md) §6). Its host tests build images with `mformat` and `mkfs.fat`, and
require `fsck.fat -n` to find each one clean after every change. `check-storage` writes a FAT32
stick through it, and the host reads the stick back with mtools.

A FAT server is the second user of the Model A data path, after ext4. Read
[`filesystem-data-path.md`](filesystem-data-path.md) for the contract first. This document covers
how FAT backs it: cluster chains, a table of them, directory entries and long names. The kernel
and the protocol know nothing about those.

## 1. The pieces

| Piece | Where | What it does |
|---|---|---|
| The protocol | `userspace/libfsserver/` | The bootstrap, the loop, directory sessions, `File::Forget` and `File::Touch`, `Meta::Unmount` — shared with `fs-server-ext4` since Part E.1 |
| The device | `libfsserver::disk::SectorDisk` | Sector-granular reads and writes, up to 64 KiB a submit, through a 64 KiB scratch. ext4 keeps its 4 KiB `Disk` |
| The boot sector | `userspace/fs-server-fat/src/bpb.rs` | The geometry, the type, and what the server refuses |
| The table | `userspace/fs-server-fat/src/table.rs` | FAT entries of 12, 16 and 28 bits, through a cache of the FAT's sectors |
| Names | `userspace/fs-server-fat/src/names.rs` | Long names in UTF-16, short names in 8.3, and the rules for both |
| Directories | `userspace/fs-server-fat/src/dir.rs` | Entries read and written, long names assembled, paths resolved, free slots found |
| Times | `userspace/fs-server-fat/src/time.rs` | FAT's dates and times, as UTC |
| The volume | `userspace/fs-server-fat/src/volume.rs` | `Fat`: every operation, and its `Volume` impl for the loop |
| The binary | `userspace/fs-server-fat/src/main.rs` | `_start`: the loop over `Fat`, through `ReadOnly` for a read-only mount |

The library is `no_std` with no `alloc`, as ext4's is. Every buffer is the caller's, or a bounded
one on the stack.

## 2. What it serves, and what it refuses

**The check** runs before `Ready`, and the storage service's probe runs the same one:
- **512-byte sectors.** A FAT with other sectors is refused, with its size in the reason.
- **The type is read as Linux and the host tools read it**: FAT32 when the boot sector's 16-bit
  FAT size is zero, else FAT12 or FAT16 by cluster count, below 4,085 being FAT12. The
  specification decides by count alone. The two disagree only on a FAT32 with too few clusters,
  which `mkfs.fat` writes, with a warning, below about 257 MiB.
- **Every field is checked before it is used**: a malformed one is refused with what made no
  sense, and a chain or entry naming a cluster outside the volume reads as `Corrupt`. A FAT is
  anyone's bytes. **A host test holds it as a class**: garbage over the boot sector, the FAT, the
  root and the first clusters, and real entries' fields made random or turned into long-name parts,
  then every operation, on all three kinds — which found a long-name part of ordinal 0 underflowing
  (PR #365 review). **A server that panics anyway exits**, saying where: a forwarded resolve has no
  deadline, and one waiting on a server spinning in its handler waited for ever, where an exit
  closes the endpoint and the kernel fails it `PeerClosed`.
- **Clusters of at least a page** — the server's rule, not the library's, which works at any
  size, so its tests run on small images (§3).
- **The volume's last sector reads**, so it is not larger than its device.

A refusal carries its reason, which the storage service prints: `512-byte clusters, smaller than a
page`, say. **Every Nitrox ESP is refused this way**: the release disk's and `nxinstall`'s have
512-byte clusters, the live stick's 1 KiB.

## 3. The map, and a file's id

**A file is mapped in 512-byte sectors** — `block_size` 512, each run's start in sectors from the
volume's start — a run per contiguous stretch of its cluster chain. A FAT's data region need not
begin on a 4 KiB boundary: `mformat -F -c 8` leaves it where the FATs end. The kernel fills and
writes a page as one device range, so **no page may span two runs**. A cluster of at least a page
guarantees that, wherever the data region starts. That is why smaller clusters are refused.

**A file in more than 64 fragments is refused `TooLarge`**, as ext4's is: a reply carries 64 runs.
A grow takes clusters next to the file's last one where it can, so a file this server writes stays
in few runs.

**A file's id is its first cluster.** It stays the same through a rename or a move, which is what
the kernel's one page-cache object per id needs. **An empty file has no cluster, so its id is `0`**,
which the kernel does not cache. It holds nothing to write back. **A truncate to zero ends the id**:
the entry's cluster becomes `0`, and the server sends `File::Forget` for the old id before the
chain is freed, as for a removal.

**`File::Touch` arrives by id**, and FAT has no table from a first cluster to its entry, so the
volume keeps one. It holds 256 entries, filled at each block-file resolve and kept through
renames, the oldest dropped first. Before a stamp, the entry at that place is read and must still
be a live file with that cluster. A miss costs the file its `mtime`, which `File::Touch` is allowed
to lose.

## 4. Names

- **Long names are UTF-16 on the disk and UTF-8 in the system**, up to 255 units. A long name's
  entries carry a checksum of their short entry's name. One whose checksum does not match is
  stale, left by a system that renamed the file without knowing long names, and is ignored.
- **Case-insensitive and case-preserving, for ASCII letters**, as FAT is read on every system that
  reads it. Other characters compare exactly: Unicode's case rules are not FAT's to apply.
- **A valid upper-case 8.3 name gets a short entry alone.** Any other name gets long-name entries
  and a short name generated from it, with the lowest numeric tail no entry in the directory has:
  `LONGNA~1.TXT`, then `LONGNA~2.TXT`. Windows' case bits are read, so another system's
  `readme.txt` lists that way, and never written: a lower-case 8.3 name gets a long name instead.
- **A name a FAT cannot hold is refused**, `InvalidArgument`: empty, `.` or `..`, a control
  character or any of `"*/:<>?\|`, longer than 255 units, or ending in a space or a dot, which
  other systems strip.
- **A name over 255 bytes of UTF-8 is listed by its short name**, since a listing entry's
  `name_len` carries 255 and a long name of 255 units can take 765 bytes. **A lookup compares long
  names in UTF-16**, as they are stored, so such a file answers to either name, and its directory
  can be emptied and removed. It used to be left out of the listing and unreachable, and its
  directory then refused removal as not empty (PR #365 review).
- **`.` and `..` are not listed**, as on ext4, nor is the volume label's entry. A directory is
  `0o755`, a file `0o644`, a read-only one `0o444`.

## 5. Writing

**Data, then the chain, then the entry.** A crash between them loses clusters, which a check
reclaims, and never gives one cluster to two files — the one exception a rename's, below, which
leaves one file under two names. **A host test crashes each change after every write it makes**,
and checks what a crash there would leave: each flush that orders the writes is one its removal
fails (PR #365 review, finding 2):
- **A grow** zeroes what it adds on the device first: what the file's own clusters held past its
  end, and every cluster it takes. Then it writes the new chain to the FAT, links it to the file's
  last cluster and writes that, and only then writes the entry's size and first cluster.
- **A truncate** writes the entry first, then cuts the chain and writes the cut **before** freeing
  what was past it, so no crash leaves the file's chain running into free clusters.
- **A removal** marks the entries deleted and frees nothing: the kernel may hold the file's pages
  and be writing them. The clusters are held for `release`, which the loop calls once the kernel
  has answered `File::Forget`.

**The write path batches**, as the throughput deferral requires
([`deferred-decisions.md`](../rationale/deferred-decisions.md), `TODO(fs-throughput)`):
- **Allocation is one pass**: first-fit from a hint — FAT32's next-free, then past the last
  allocation, or past the file's own last cluster — so a grow's clusters are contiguous wherever
  the free space is. **All or nothing**: a volume that runs out gives back what it took, and the
  grow is `TooLarge`, as a full ext4's is.
- **Zeroing is in 64 KiB writes**, contiguous clusters merged.
- **The FAT's sectors are cached**, 64 of them. A change marks its sectors dirty, and they are
  written at the points the order above needs and at the end of each request: adjacent sectors in
  one write, to every copy of the FAT. A dirty sector is evicted only by a write path, which writes
  it out first. A 1 MiB grow on a fresh FAT32 costs 19 writes: sixteen of zeroes, the FAT once per
  copy, and the entry.

**Directories**:
- **A directory grows by a zeroed cluster** when it has no run of free entries long enough for a
  name: the cluster is written before it is linked. **FAT12 and FAT16's fixed root does not grow**,
  and is `TooLarge` full, as is any directory at the specification's 65,536 entries.
- **`mkdir`** writes its cluster — `.`, `..` and zeroes — before the entry that names it. **One
  flush is the rule for every entry written**: the FAT's changes go to the disk before any entry,
  which covers a grown directory's link and a new directory's cluster alike. **`rmdir`** takes an
  empty directory; a file is `Unsupported`, as is a directory to `unlink`.
- **A directory removed is no directory to a session still holding it** (PR #365 review): a
  session names its directory by id, its first cluster, which the removal frees and a grow may
  take. Every operation by id first checks that cluster is still allocated and still begins with a
  `.` naming it, and refuses `NotFound` otherwise.
- **A rename** writes the new name first — new entries, or a replaced file's entry pointed at the
  source — then deletes the old one, so a crash leaves the file under both names rather than
  neither. A directory moved to another parent has its `..` repointed: `0` for the root,
  FAT32's included. **A directory cannot move into itself**: its `..` is followed from the target
  to the root. A directory can neither replace nor be replaced (`Unsupported`), as on ext4 here.
  A replaced file's id comes back for the loop to forget and release.

**Times are UTC** both ways. FAT stores no zone, and the system has none either.

## 6. How a filesystem was left

**The dirty bit is the boot sector's state byte**: bit 0 of the byte at `0x25` on FAT12 and FAT16,
`0x41` on FAT32, which `fsck.fat` and Linux read. A writable mount sets it before `Ready`, and an
unmount clears it as its last write.

**A filesystem found dirty is reported, served, and left dirty by its unmount**, as Linux leaves
one. Nothing here repairs a filesystem (`TODO(fs-repair)`), so clearing the bit would hide that it
needs a check from the next system that can make one. ext4's server clears it; that is ext4's to
revisit.

**FAT32's FSInfo free count is unknown while mounted** — set to `0xFFFFFFFF` at mount, which
`fsck.fat` accepts — and counted off the FAT at the unmount, before the state byte, with the
next-free hint beside it. A wrong count is one `fsck.fat` reports.

## 7. Read-only mounts

**A read-only mount is served through `ReadOnly`**, as ext4's is: every write refused, and every
file it replies marked `FILE_BLOCKS_READ_ONLY`. **The refusal comes before anything is read**, so a
change that would have done nothing — a create of a file that exists, a grow to less — is refused
too, rather than answered as if it could have been made.

## 8. What the tests and gates hold

- **Host tests** (`userspace/fs-server-fat/src/volume/`): reading images `mformat`, `mkfs.fat` and
  `mcopy` built at FAT12, FAT16 and FAT32, and the type by count and by boot sector. **Every change
  followed by `fsck.fat -n`**, which must find the image clean, and mtools reading back what was
  written: long and Unicode names, unique short names, a grow reading zeroes over a deleted file's
  clusters, a full volume and a full fixed root, a directory moved, the dirty bit both ways, a
  1 MiB grow's writes counted, and a read-only mount refusing everything. **Each change crashed
  after every write** and the image checked as the crash would leave it (§5); **garbage in every
  structure** met with an error, never a panic (§2); a change bigger than the FAT cache, which
  evicts dirty sectors; and a removed directory refused to a session. And through
  `libfsserver`'s request core: a file replied in sectors with its first cluster as its id.
- **`test-qemu`**: the test stick's FAT16, with 512-byte clusters, is reported as not served for
  them, so the boot's mounts are as they were.
- **`check-storage`**: a 300 MiB FAT32 stick, its data region off a 4 KiB boundary, auto-mounted
  read-only. The host's long, Unicode and nested names are listed in the guest, and its pattern
  is read through a mapping. Then it is remounted writable and written: a directory, a copy to a
  long Unicode name, a rename, a removal, and a file through a mapping. Ejected and pulled, it is
  `fsck.fat -n` clean on the host, and mtools reads what the guest did. An internal FAT on the
  SATA disk is left unmounted, not being removable.

## 9. Not built

- **Clusters under 4 KiB.** Part G's `disk --format` makes such a stick servable. Serving one
  needs a page filled from several runs, or a second data path, for a stick someone needs.
- **More than 64 fragments**, which `MapRange` would lift for both servers.
- **Sectors other than 512 bytes**, which no stick this phase meets uses.
- **Attributes beyond read-only and directory** — hidden, system, archive are kept, not shown.
- **Repair**: a dirty filesystem is served as found.
- **exFAT.**

See [`deferred-decisions.md`](../rationale/deferred-decisions.md) for each one's trigger.
