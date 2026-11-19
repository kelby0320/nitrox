# list — list a directory as a table

## Synopsis

```
list [--recursive] [PATH...]
```

## Description

`list` lists each PATH, or the working directory when there is none, as a table with one row per
entry. Several PATHs are listed in turn into one table. `.` and `..` are not listed.

The listing is the directory's files together with anything the namespace binds beneath it.

Its output is a table, so the shell's operators narrow it:

```
list /system | filter size > 1000 | sort name
```

## Options

- `-r`, `--recursive`: list subdirectories too. `name` is then the path relative to the directory
  listed.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`Table<{name: String, size: Int, kind: String, modified: Int}>`

- `name` is the entry's name, or its path relative to the directory listed under `--recursive`.
- `size` is in bytes; a directory reports the size of its own directory data.
- `kind` is `"file"`, `"dir"`, `"symlink"` or `"unknown"`.
- `modified` is Unix epoch seconds.

## Exit status

0 when the listing was written, or its reader went away first. 1 when a PATH could not be listed,
a recursive listing went deeper than 32 levels, or the write failed. 2 for an option it does not
know.

## Examples

```
list                       # the working directory
list /system | sort name   # a directory, by name
list --recursive /home     # everything under /home
```

## See also

`copy(1)`, `move(1)`, `remove(1)`, `nxsh(1)`, `shell-operators(7)`
