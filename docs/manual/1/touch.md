# touch — create a file or stamp its time

## Synopsis

```
touch [--no-create] PATH...
```

## Description

`touch` creates each PATH as an empty file if it is not there, and stamps its modification time if
it is. The time is the filesystem's own clock; it cannot be supplied.

A PATH that is a directory is refused.

## Options

- `-c`, `--no-create`: stamp only. A PATH that does not exist is skipped, with no row and no
  error.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`Table<{path: String, created: Bool}>`

One row per PATH touched. `created` is true when the file was made.

## Exit status

0 when every PATH was touched or skipped. 1 when a file could not be created or stamped. 2 for an
option it does not know, no PATH, an empty path, or a directory.

## Examples

```
touch notes.txt               # make it, or stamp it
touch --no-create notes.txt   # stamp it only if it is there
```

## See also

`mkdir(1)`, `list(1)`, `date(1)`, `nxsh(1)`
