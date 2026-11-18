# page — show the input a page at a time

## Synopsis

```
page
```

## Description

`page` shows its input a page at a time on the terminal, drawn as the shell would draw it: text as
lines, and a table as columns under its header. It pages only where its output is what the shell
shows at the prompt, as the last stage of a line typed there:

```
list /system/bin | page
```

Anywhere else, piped on, assigned, or in a script, it passes its input on unchanged, as it does
input one page holds. `man` pages through the same pager.

The page is drawn on the terminal's alternate screen, so leaving it puts back what was there. The
bottom row says which lines are shown, and `(END)` on the last page.

## Keys

| Keys | Move |
|---|---|
| Space, `f`, Page Down | A page on |
| `b`, Page Up | A page back |
| `j`, Down, Enter | A line on |
| `k`, Up | A line back |
| `g`, Home | The top |
| `G`, End | The end |
| `/word` then Enter | The next line holding the word |
| `n` | The next line after that |
| `q`, `Ctrl-C` | Leave |

A search is a word as typed, upper and lower case apart; Backspace edits it, and leaving it empty
goes back to the keys.

## Output

The input, unchanged, when it is not paged; nothing when it is.

## Exit status

0 when the input was paged or passed on. 1 when it could not be read, is over a megabyte, or the
write failed. 2 for an operand.

## See also

`man(1)`
