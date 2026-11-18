# rename — change a file's name in place

## Synopsis

```
rename [--force] OLD NEW
```

## Description

`rename` points the name NEW at the file called OLD. It never copies: the file stays the same file.
A rename across mounts is an error, and `move(1)` is the command that falls back to a copy there.

Both paths are taken from the working directory. A NEW that already exists is an error unless
`--force` is given.

## Options

- `-f`, `--force`: replace an existing NEW.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`Table<{from: String, to: String}>`

One row.

## Exit status

0 when the name was changed. 1 when OLD does not exist, OLD and NEW are on different mounts, or the
rename failed. 2 for an option it does not know, any number of operands but two, or the same name
twice.

## Examples

```
rename draft.txt final.txt   # the same file, a new name
rename -f new.txt old.txt    # replace old.txt
```

## See also

`move(1)`, `copy(1)`, `nxsh(1)`
