# nxinstall — install Nitrox onto a disk

## Synopsis

```
with admin nxinstall
with admin nxinstall DEVICE
```

## Description

`nxinstall` installs this system onto a disk, erasing it, with a first account of its own. It is
run from the live stick's "install to this machine" entry, through `with admin`: the disks come
with the view, and `nxinstall` alone has none. A disk the view lacks is in use, and `nxinstall`
says so, and how to free it where the storage service has mounted it.

With no DEVICE it lists the disks it can install to, and says why any other is in use.

With a DEVICE, a disk as the listing names it, such as `/dev/blk/0`, it shows what installing
would do and asks first. The question gives the disk, the sizes of the boot partition and the root
partition, that everything on the disk is lost, and `type yes to go ahead:`. Anything but `yes`
writes nothing, and that is not a failure.

It then asks for the first account: a name, which is echoed, and a password twice, which is not.
**Nothing is written until every question is answered.**

Then it writes a GPT with two partitions: the boot partition, and a root partition with a new
filesystem that holds the system's files but not the build's accounts. `/home` is made empty. The
new machine's accounts and `/system/views.toml` are its own: one account, which is the one that
administers, and its home. When it says it is done, remove the installation medium and restart.

It refuses a DEVICE that is a partition, one that is memory published as a disk, one that does
not say what it is, and one that reports no model or serial, since the plan could not say which
disk it is. It needs a terminal to ask on.

## Options

- `--help`: print the usage and exit.

`nxinstall` takes no other option. Naming more than one DEVICE is an error.

## Output

With no DEVICE, the table of disks the view can use, `Table<{path: String, kind: String, size:
String, name: String}>`. Messages, and the progress of an install, go to the error output.

## Grants

The `disks` grant, which `with admin` and `with install` have.

## Exit status

0 when the disks were listed, the help was shown, the install was declined, or the install is
done. 1 when the disk could not be installed to: no disks in the view, a device the view cannot
reach or will not write, no terminal, an account that the rules do not allow, or a failure while
writing. 2 for an option it does not know, or more than one operand.

## Examples

```
with admin nxinstall               # the disks it can install to
with admin nxinstall /dev/blk/2    # plan an install to one; asks before it writes
```

## See also

`with(1)`, `disk(1)`, `account(1)`
