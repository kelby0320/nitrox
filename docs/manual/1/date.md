# date — show or set the time

## Synopsis

```
date [--unix]
date --set TIME [--unix]
```

## Description

`date` reports the wall-clock time, in UTC, as a one-row table of its parts, so nothing has to be
parsed back out of a string. There is no format option, as there is no time zone or locale to
format for.

A machine whose clock is not set says so and fails, rather than report 1970.

With `--set`, `date` first sets the clock, and the machine's hardware clock, to TIME, and then
reports the time as the clock reads after the set. TIME is UTC, written exactly as
`2026-09-28T14:30:00Z`, from the year 2000 to 2099. If the hardware clock did not take the time,
`date` says so on stderr: the time then lasts only until the next boot.

## Options

- `-u`, `--unix`: report the bare Unix time only, as a table of the one column.
- `--set TIME`: set the clock to TIME first.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Grants

`--set` needs the `clock` grant: `with admin date --set TIME`.

## Output

`Table<{unix: Int, year: Int, month: Int, day: Int, hour: Int, minute: Int, second: Int}>`

One row, in UTC. With `--unix`, only `unix`, in seconds.

## Exit status

0 when the time was written. 1 when the clock is not set, cannot be set here, or was refused a time
outside 2000 to 2099, or the write failed. 2 for an option it does not know, an operand without
`--set`, a `--set` without exactly one TIME, or a TIME not in that form.

## Examples

```
date                                  # the time, as a row of its parts
date --unix                           # seconds since 1970
with admin date --set 2026-09-28T14:30:00Z   # set the clock, then show it
```

## See also

`sleep(1)`, `touch(1)`, `nxsh(1)`
