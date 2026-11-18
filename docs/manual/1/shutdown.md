# shutdown — stop the machine, or restart it

## Synopsis

```
shutdown [--reboot]
```

## Description

`shutdown` asks every session and service to stop, writes back and unmounts every filesystem, and
then stops the machine. It says when it is safe to turn the machine off. With `--reboot` it
restarts the machine instead of halting it.

`shutdown` only asks. `service-mgr` does the rest: it asks the sessions to end, this one included,
stops the services, and has the filesystems unmounted. The session ends with the machine, so
nothing is written afterwards.

It needs the `power` grant, which the system's policy gives everyone for `shutdown` alone, with no
password: run it as `with power shutdown`. Without the grant it says to use `with`.

## Options

- `--reboot`: restart the machine rather than halting it.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

`shutdown` takes no operand.

## Output

Nothing is written to the output. Its line goes to the terminal and the console.

## Grants

`power`, which binds `/dev/power`: `with power shutdown`.

## Exit status

0 when the shutdown has begun, though the session usually ends before it can be read. 1 when
`/dev/power` is missing, or the service manager opened no session or would not begin. 2 for an
option it does not know, or an operand.

## Examples

```
with power shutdown             # stop the machine
with power shutdown --reboot    # restart it
```

## See also

`with(1)`, `service(1)`, `disk(1)`
