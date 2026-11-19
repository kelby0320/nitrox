# terminal — show what is known of the terminal

## Synopsis

```
terminal
```

## Description

`terminal` reports what is known of the terminal the command was handed: its size, as its window
last said. Where the size is not known, as on a serial line, the columns and rows are null, not an
invented 80 by 24.

With no terminal at all, `terminal` fails.

## Options

- `--help`: show the usage and exit.
- `--version`: show the version and exit.

`terminal` takes no other option and no operand.

## Output

`Table<{columns: Int?, rows: Int?}>`

One row. `?` marks a column that can be null.

## Exit status

0 when the row was written. 1 when the command was handed no terminal, or the write failed. 2 for
an option it does not know or an operand.

## Examples

```
terminal                      # the size, as a row
terminal | display            # the same, drawn as a table
```

## See also

`whoami(1)`, `nxsh(1)`, `shell(7)`
