# mkdir — create directories

## Synopsis

```
mkdir [--parents] PATH...
```

## Description

`mkdir` creates each PATH as a directory. A trailing slash is ignored.

Without `--parents`, the directory above PATH must exist and PATH must not. With it, missing
directories above PATH are created too, and a PATH that is already a directory is not an error.

A failure stops the run and writes no rows, not even for the directories made before it; `list`
shows them.

## Options

- `-p`, `--parents`: create missing parent directories, and treat an existing PATH as success
  rather than an error.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`Table<{path: String, created: Bool}>`

One row per PATH. `created` is false when `--parents` found the directory already there.

## Exit status

0 when every PATH was made or, with `--parents`, found. 1 when a directory could not be created: the
path exists (without `--parents`) or is not a directory, or its parent is missing or cannot be
opened. 2 for an option it does not know, no PATH, an empty path, or the root.

## Examples

```
mkdir projects                 # one directory
mkdir --parents a/b/c          # a, a/b and a/b/c, as far as they are missing
mkdir -p /home/docs | display  # the table says whether it was made
```

## See also

`touch(1)`, `remove(1)`, `list(1)`, `nxsh(1)`
