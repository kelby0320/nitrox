# The manual's page format — Markdown, in the subset `man` draws

**Status:** current. The contract between a page, `man`'s renderer (`userspace/coreutils/src/manual.rs`)
and the checks that hold every page to it.

## Where a page is

A page is `/system/manual/<name>.<section>.md`, as `man.1.md` and `init.toml.5.md` are: the <!-- check-docs: allow-missing -->
profile server's projection of each package's `manual/`, flat as its projections are, which no
session can change. The build stages two kinds, into the coreutils package beside `man`
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
lists and `man --keyword` searches. A page whose first heading does not name it before a dash, as
a spec's, is summarised by the whole heading.

## The blocks

| Block | Written | Drawn |
|---|---|---|
| Heading | `#` to `######`, then a space; a closing run of `#` after a space is dropped | At the margin, in bold |
| Paragraph | Lines run together until a blank line or another block | Wrapped, four columns in |
| List | `-`, `*` or `+`, or a number and a dot; a second level indented under an item | Its lines hung under its text; the second level two columns further in |
| Code | From a line of three or more backticks or tildes to a line of as many or more of the same and nothing else | Eight columns in, **never wrapped**, a tab four spaces |
| Table | A row of cells between `\|`, a separator row of dashes, then rows | See below |
| Quote | Lines beginning `>` | One paragraph, each line led by `\| ` |
| Rule | Three or more `-`, `*` or `_` alone | A line of dashes |
| Comment | `<!--` to `-->`, over any lines | Nothing; text after a one-line comment is the line's |

A table or a code block indented under a list item ends the list, and is drawn as a block of its
own.

## Inside a line

- `` `code` ``, in as many backticks as it needs: drawn in cyan.
- `**bold**`: drawn bold. `*emphasis*`: underlined. An asterisk, or a pair, that does not open
  before a word and close after one later in the line is itself: `a * b`, `*.md`, `2 ** 3`.
- `[text](address)`: the text alone. `![text](image)`: the text. `[text][ref]` is as written: a
  page has no reference definitions.
- `<https://…>`: the address.
- The HTML tags `a`, `br`, `sup`, `sub`, `kbd`, `span`, `div`, `details`, `summary` and `img`, and
  comments: nothing. Any other `<…>` is text, as a placeholder such as `<name>` is meant.
- `&amp;`, `&lt;`, `&gt;`, `&quot;` and `&nbsp;`: the character.
- A backslash before punctuation: that character, as itself.
- A tab: a space. **Any other control character is dropped**, an escape among them, so a page
  cannot move the cursor, clear the screen or retitle the terminal it is read in.

## A table

A table's columns are as wide as their widest cell, the header's included, two columns apart.
- **Too wide for the line**, the widest column is narrowed a column at a time, each cell wrapping
  within its own, down to ten columns each.
- **Too wide even then**, the table is drawn **one record per row**: each cell on its own line,
  after its column's name, rows a blank line apart.
- **A cell is split** at each `|` outside a code span, which a run of as many backticks as opened
  it closes; `\|` is a `|` in a cell, in a code span or not.
- **A row with more cells than the header** widens the table, the extra column unnamed.

## Width and style

- **The width is the terminal's**, as `Tty::Size` answers it, and **80 columns** where the size is
  unknown, as on a serial line.
- **A word longer than a line is broken where the line ends.** No line is wider than the width
  but a code block's.
- **Plain or styled**: styled, bold is `ESC[1m`, code `ESC[36m` and emphasis `ESC[4m`, each
  ended by `ESC[0m`. `man` draws plain.

## What holds a page to this

`every_page_and_spec_renders_within_eighty_columns`, in the renderer's tests, draws every page in
`docs/manual` and every spec at 80 columns, and fails on any line wider outside a code block.
