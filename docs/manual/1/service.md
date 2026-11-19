# service — see and control the system's services

## Synopsis

```
service --list
service --start NAME
service --stop NAME
service --restart NAME
```

## Description

`service` shows what each service the system declares is doing, and starts, stops and restarts
them. One verb is given at a time.

`--list` works from any session. `--start`, `--stop` and `--restart` need the `services` grant:
run them with `with admin`. Without it `service` says so and names `with`. Each waits until the
change has happened. `service-mgr` does the refusing that matters, and its reason is printed as it
gave it: a service nothing declares, a stop that was not honoured, and an essential service, which
cannot be stopped or restarted by anyone.

## Options

- `--list`: every declared service, whether it is starting, running, stopped or failed, and how
  often it has restarted.
- `--start NAME`: start a service that is not running, and wait until it is up.
- `--stop NAME`: ask a running service to stop, and wait until it has.
- `--restart NAME`: stop it and start it again.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`--list` writes `Table<{name, state, restarts}>`, `service-mgr`'s own. Each of the other verbs
writes a one-row `Table<{name, state}>`. Where there is no output to write to, a table is printed
as lines.

## Grants

`--list` needs none. `--start`, `--stop` and `--restart` need `services`: `with admin service ...`.

## Exit status

0 when it was done. 1 when `service-mgr` refused, or the request failed or the write failed. 2 for
an option it does not know, or none of the verbs.

## Examples

```
service --list                       # every service and its state
service --list | sort restarts       # the busiest restarters last
with admin service --restart NAME    # restart the service called NAME
```

## See also

`with(1)`, `shutdown(1)`, `log(1)`, `shell-operators(7)`
