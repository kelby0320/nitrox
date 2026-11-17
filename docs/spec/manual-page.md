# The manual's page format — Markdown, in the subset `man` draws

**Status:** current. The contract between a page, `man`'s renderer (`userspace/coreutils/src/manual.rs`)
and the checks that hold every page to it.

## Where a page is

A page is `/system/manual/<section>/<name>.md`. The build stages two kinds
(`tools/xtask/src/manual.rs`):

- **The written pages**, from `docs/manual/<section>/<name>.md` in the repository; <!-- check-docs: allow-missing -->
- **The specifications that are pages too**, from `docs/spec`, each given a section and a name
  by `STAGED_SPECS`. A spec is staged there or listed in `NOT_STAGED` with a reason; a test holds
  that every spec is one or the other.

| Section | Holds |
|---|---|
| 1 | Commands, and the applications |
| 2 | System calls |
| 5 | File formats |
| 7 | Overviews: the system, the shell, the protocols |
| 8 | Services |

`man` reads these five. A name is looked for in sections 1, 8, 2, 5 and 7, in that order, when no
section is named.

## Its first line

`# name — what it is in a few words`. What follows the dash is the page's summary, which `man`
lists and `man --keyword` searches. A page whose first heading has no dash, as a spec's, is
summarised by the whole heading.

## The blocks

| Block | Written | Drawn |
|---|---|---|
| Heading | `#` to `######`, then a space | At the margin, in bold |
| Paragraph | Lines run together until a blank line or another block | Wrapped, four columns in |
| List | `-`, `*` or `+`, or a number and a dot; a second level indented under an item | Its lines hung under its text; the second level two columns further in |
| Code | Between two lines of three backticks, or three tildes | Eight columns in, **never wrapped** |
| Table | A row of cells between `\|`, a separator row of dashes, then rows | See below |
| Quote | Lines beginning `>` | One paragraph, each line led by `\| ` |
| Rule | Three or more `-`, `*` or `_` alone | A line of dashes |
| Comment | `<!--` to `-->`, over any lines | Nothing |

A table or a code block indented under a list item ends the list, and is drawn as a block of its
own.

## Inside a line

- `` `code` ``, in as many backticks as it needs: drawn in cyan.
- `**bold**`: drawn bold. `*emphasis*`: underlined. A `*` with a space either side is an asterisk.
- `[text](address)` and `[text][ref]`: the text alone. `![text](image)`: the text.
- `<https://…>`: the address.
- The HTML tags `a`, `br`, `sup`, `sub`, `kbd`, `span`, `div`, `details`, `summary` and `img`, and
  comments: nothing. Any other `<…>` is text, as a placeholder such as `<name>` is meant.
- `&amp;`, `&lt;`, `&gt;`, `&quot;` and `&nbsp;`: the character.
- A backslash before punctuation: that character, as itself.

## A table

A table's columns are as wide as their widest cell, the header's included, two columns apart.
- **Too wide for the line**, the widest column is narrowed a column at a time, each cell wrapping
  within its own, down to ten columns each.
- **Too wide even then**, the table is drawn **one record per row**: each cell on its own line,
  after its column's name, rows a blank line apart.
- **A cell is split** at each `|` outside a code span; `\|` is a `|` in a cell, in a code span or
  not.

## Width and style

- **The width is 80 columns** while a program cannot learn its terminal's (`TODO(tty-size)`).
- **A word longer than a line is broken where the line ends.** No line is wider than the width
  but a code block's.
- **Plain or styled**: styled, bold is `ESC[1m`, code `ESC[36m` and emphasis `ESC[4m`, each
  ended by `ESC[0m`. `man` draws plain.

## What holds a page to this

`every_page_and_spec_renders_within_eighty_columns`, in the renderer's tests, draws every page in
`docs/manual` and every spec at 80 columns, and fails on any line wider outside a code block.
