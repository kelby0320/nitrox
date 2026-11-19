# remove — delete files and directories

## Synopsis

```
remove [--recursive] [--force] PATH...
```

## Description

`remove` deletes each PATH. A directory is refused, even an empty one, unless `--recursive` is
given: removing is not undone, so it does not default to taking a tree.

`remove` works on the filesystem only. A PATH that names a namespace binding, a mount point, is
refused, and `remove` never unbinds one. The root cannot be removed.

A failure stops the run and writes no rows, not even for what was removed before it; `list` shows
what is left.

## Options

- `-r`, `--recursive`: remove directories and everything under them.
- `-f`, `--force`: a PATH that does not exist is not an error. Other failures still are.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`Table<{path: String, kind: String}>`

One row per entry actually removed, those inside a `--recursive` descent included. `kind` is
`"file"` or `"directory"`.

## Exit status

0 when every PATH was removed, or was not there under `--force`. 1 when a PATH does not exist, or
an entry could not be removed. 2 for an option it does not know, no PATH, the root, a namespace
binding, or a directory given without `--recursive`.

## Examples

```
remove old.txt                # one file
remove --force maybe.txt      # no complaint if it is not there
remove --recursive build      # a directory and all it holds
```

## See also

`copy(1)`, `move(1)`, `list(1)`, `nxsh(1)`
