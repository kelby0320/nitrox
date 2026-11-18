# log — read the system's logs

## Synopsis

```
log [PRINCIPAL]
log --kernel
```

## Description

`log` reads a log back, oldest first. With no option it reads the service log: every record the
logging service still holds, or only those of PRINCIPAL. With `--kernel` it reads the kernel's log,
a row for each line.

The service log needs the `logs` grant, and the kernel's the `kernel-log` grant: run it as
`with admin log`. Without the grant `log` says which one to ask `with` for.

The logging service keeps the most recent records. That the oldest are no longer kept is said on
the error output, and so is any record the log dropped while `log` was reading it.

A PRINCIPAL names whose records to keep: its own, and those of its named sources, which follow a
dot, as `heartbeat.worker`.

## Options

- `--kernel`: read the kernel's log instead of the service log. It takes no PRINCIPAL.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`Table<{sequence: Int, time: Int?, principal: String, tier: String?, level: String?, message:
String}>`, oldest first. `?` marks a column that can be null.

- `sequence` is the order of the records, which `time` cannot give, since the clock can be set.
- `time` is Unix epoch seconds, as `list`'s `modified` is. It is null for a record taken while the
  clock was not set.
- `principal` is whose channel the record came on, which its emitter cannot choose.
- `tier`, `level` and `message` are the emitter's own claim.

A kernel row's `principal` is `kernel`, and its `sequence` is its line's place in the log. Its
`time`, `tier` and `level` are null, which the kernel's lines do not carry.

## Grants

`logs` for the service log, and `kernel-log` for `--kernel`: `with admin log`.

## Exit status

0 when the log was written, or its reader went away first. 1 when the grant is missing, the log
could not be read, or the write failed. 2 for an option it does not know, more than one PRINCIPAL,
or a PRINCIPAL with `--kernel`.

## Examples

```
with admin log                          # every record the service log holds
with admin log heartbeat                # one principal's records and its sources'
with admin log --kernel | last 20       # the kernel's last twenty lines
```

## See also

`with(1)`, `service(1)`, `shell-operators(7)`
