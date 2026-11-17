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

## Files

Each page is `/system/manual/<section>/<name>.md`, in the format `manual-page(5)` describes.

## See also

`intro(7)`, `manual-page(5)`
