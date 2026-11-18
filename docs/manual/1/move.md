# move — move files and directories

## Synopsis

```
move [--force] SOURCE... DEST
```

## Description

`move` relocates each SOURCE to DEST. When both are on the same mount it re-points the name, which
costs nothing and keeps the file the same file. When they are not, it copies the SOURCE and then
removes the original.

When DEST is an existing directory, each SOURCE moves into it under its own name. Otherwise DEST
is the new name, and only one SOURCE may be given.

A DEST that already exists is an error unless `--force` is given. A namespace binding is refused as
the source of a move that has to copy, one across mounts: it would copy through the mount and then
remove another server's files.

If the copy works but the original cannot be removed, both now exist: `move` fails and writes no
row. If the copy fails part way, what was copied stays, and the SOURCE is untouched.

## Options

- `-f`, `--force`: replace an existing destination.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`Table<{from: String, to: String, method: String}>`

One row per SOURCE. `method` is `"rename"` when the name was re-pointed and `"copy"` when the
file was copied and the original removed.

## Exit status

0 when everything was moved. 1 when a SOURCE does not exist, a copy or the removal of the original
failed, or the move failed. 2 for an option it does not know, too few operands, several sources and
a DEST that is not a directory, or a source and destination that are the same file.

## Examples

```
move a.txt /home/docs         # into a directory
move old.txt new.txt          # a new name
move --force a.txt b.txt      # replace b.txt if it is there
```

## See also

`rename(1)`, `copy(1)`, `remove(1)`, `list(1)`, `nxsh(1)`
