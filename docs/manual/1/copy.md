# copy — copy files and directories

## Synopsis

```
copy [--force] SOURCE... DEST
```

## Description

`copy` duplicates each SOURCE to DEST. A directory is copied with everything under it, with no
flag to ask for it.

When DEST is an existing directory, each SOURCE is copied into it under its own name. Otherwise
DEST names the copy, and only one SOURCE may be given.

A DEST that already exists is an error unless `--force` is given. A failure stops the run; the rows
already written say what was copied.

## Options

- `-f`, `--force`: overwrite an existing destination.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`Table<{source: String, destination: String, bytes: Int}>`

One row per file copied.

## Exit status

0 when everything was copied. 1 when a copy failed: a source not found, a destination that exists
and no `--force`, a directory copied into itself, a tree deeper than the limit, or an I/O error. 2
for an option it does not know, too few operands, several sources and a DEST that is not a
directory, or a source and destination that are the same file.

## Examples

```
copy notes.txt backup.txt     # one file to a new name
copy a.txt b.txt /home/docs   # two files into a directory
copy --force a.txt b.txt      # replace b.txt if it is there
copy src dst | display        # a directory, and the table of what was copied
```

## See also

`move(1)`, `remove(1)`, `rename(1)`, `list(1)`, `nxsh(1)`
