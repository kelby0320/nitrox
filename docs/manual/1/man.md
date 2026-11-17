# man — read the manual

## Synopsis

```
man [SECTION] NAME
man [--keyword WORD]
```

## Description

`man NAME` prints the page named NAME: the first of that name in sections 1, 8, 2, 5 and 7, or
the one in SECTION when a number comes before the name.

With no NAME, `man` lists every page as a table, which the shell's operators narrow like any
other:

```
man | filter section == 1
```

## Options

- `-k`, `--keyword WORD`: list only the pages whose name or summary holds WORD, case aside.

## Sections

| Section | Holds |
|---|---|
| 1 | Commands |
| 2 | System calls |
| 5 | File formats |
| 7 | Overviews: the system, the shell, the protocols |
| 8 | Services |

## Output

Listing, `Table<{name: String, section: Int, summary: String}>`. A page is text, 80 columns wide.

## Exit status

0 when the page or the listing was written, or its reader went away first. 1 when there is no
such page, a page could not be read, or the write failed; a page that could not be read is
listed with why, and asking for it says so rather than that it is not there. 2 for operands it
cannot take.

## Files

Each page is `/system/manual/<name>.<section>.md`, as `man.1.md` is, in the format <!-- check-docs: allow-missing -->
`manual-page(5)` describes. No session can change one: the directory is the system profile's,
which serves each package's pages read-only.

## See also

`intro(7)`, `manual-page(5)`
