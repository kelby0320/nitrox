# clip — read and write the clipboard from a pipeline

## Synopsis

```
clip [INDEX]
clip --copy
clip --list
```

## Description

`clip` is the shell's way to the clipboard the windows share, so a pipeline can reach what a
window copied and a window can paste what a pipeline produced.

With no operand, `clip` writes the newest entry. INDEX reaches further back: 0 is the newest, 1 the
one before it. An empty clipboard, or an INDEX past the last entry, gives an empty output and
succeeds, so a script need not treat "nothing copied yet" as a failure.

`clip --copy` reads its input and pushes it onto the clipboard as a new entry. A clipboard holds
text, so each row of a table becomes a line, its fields joined by tabs; text comes back out as it
went in. It needs something piped into it.

`clip --list` describes the entries and says how long each is, not what it
says.

## Options

- `-c`, `--copy`: read the input and push it onto the clipboard.
- `-l`, `--list`: describe the entries: index, kind and length.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

`--copy` and `--list` are different jobs, and neither takes an INDEX.

## Output

A paste is text, one row per line, so every operator works on it and the shell prints what was
copied.

The listing is `Table<{index: Int, kind: Int, bytes: Int}>`.

## Exit status

0 when the work was done, an empty paste included. 1 when there is no `/dev/clipboard` in this
namespace or it did not answer, when the entry is not text or not valid UTF-8, when what is copied
is larger than one entry, or when the clipboard refused. 2 for an option it does not know, for
`--copy` and `--list` together, for an INDEX with either, for an INDEX that is not a number, and
for `--copy` with nothing piped into it.

## Examples

```
clip                       # the newest entry
clip 1                     # the one before it
list /system | clip --copy # copy a listing, a line a row
clip --list                # how many entries, and how long each is
```

## See also

`nxterm(1)`, `nxedit(1)`, `nxfiles(1)`, `shell-operators(7)`
