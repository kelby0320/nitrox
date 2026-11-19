# disk — the disks, and their filesystems

## Synopsis

```
disk --list
disk --mount DEVICE [LABEL]
disk --unmount LABEL
disk --eject NAME
disk --partition DISK [--mbr | --gpt]
disk --format DEVICE fat|ext4 [LABEL] [--mbr | --gpt]
```

## Description

`disk` shows the machine's disks, mounts and unmounts their filesystems, and makes new ones. One
verb is given at a time.

A DEVICE is written `/dev/blk/N` or `blk-N`. A mounted filesystem is under `/storage/LABEL`.

`disk --list` works from any session. Mounting and unmounting need the storage grant, and
partitioning and formatting need the disks grant too: run them with `with admin`. Ejecting a
removable drive needs neither.

Partitioning and formatting lose everything on the device. Each refuses before it writes anything
when the device is on the disk the machine started from, is on a RAM disk, is mounted, or is a disk
that is not removable; or the filesystem is too small for its partition, or the label cannot be
held or could not name its mount. The refusal says which.

## Options

- `--list`: every block device, what it holds, where it is mounted, and whether it was left clean.
- `--mount DEVICE [LABEL]`: mount DEVICE at `/storage/LABEL`, or under the label the storage
  service chooses. It is mounted read-only, and says why, when its filesystem's server only reads
  it.
- `--unmount LABEL`: write back every file, flush the drive and unmount. It is refused while a
  file on it is open.
- `--eject NAME`: the same for every filesystem on the removable drive that holds `/storage/NAME`,
  after which the drive can be pulled out. NAME is the name in the `mounted` column, not the
  label. An internal disk is refused, and so is a drive with a file open.
- `--partition DISK`: write one partition spanning a removable DISK, in an MBR or a GPT, a GPT from
  2 TiB.
- `--format DEVICE FS [LABEL]`: make a `fat` or an `ext4` filesystem on DEVICE. On a disk it writes
  the table first, an MBR for `fat` and a GPT for `ext4`. The default label is `NITROX`, and
  `nitrox` for ext4. A FAT label holds 11 characters, an ext4 label 16.
- `--mbr`: with `--partition` or `--format` of a disk, an MBR.
- `--gpt`: with `--partition` or `--format` of a disk, a GPT.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`--list` writes the storage service's table, `Table<{name, kind, description, size, filesystem,
label, mounted, by, mode, clean, removable, note}>`. `note` says why a filesystem found was not
mounted, or why a mounted one is only read.

`--mount` writes one row with its `device`, `label`, `mounted` name, `mode` and `note`. `--unmount`
writes `Table<{label, unmounted}>`, and `--eject` `Table<{name, ejected}>` with a row for each
filesystem it unmounted.

`--partition` and `--format` write `Table<{device, table, filesystem, partition, mounted, note}>`:
what was written, the new partition's name, and where it was mounted or why not.

## Grants

| Verb | Needs |
|---|---|
| `--list`, `--eject` | nothing |
| `--mount`, `--unmount` | the `storage` grant |
| `--partition`, `--format` | the `storage` and `disks` grants |

Without a grant the command says so and names `with`.

## Exit status

0 when it was done. 1 when the request was refused or failed, or the write failed. 2 for an option
it does not know, operands that do not fit one verb, a DEVICE it cannot read, or `--mbr` or
`--gpt` without `--partition` or `--format`.

## Examples

```
disk --list                          # every device, and where it is mounted
with admin disk --mount /dev/blk/2   # mount a device
with admin disk --unmount usb        # unmount it
disk --eject usb                     # make a removable drive safe to pull out
with admin disk --format /dev/blk/3 fat BACKUP   # a FAT filesystem on a device
```

## See also

`with(1)`, `list(1)`, `shell-operators(7)`
