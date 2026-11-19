# whoami — show the session's user

## Synopsis

```
whoami
```

## Description

`whoami` reports the user this session was opened for. It reads the name from `/session/user`,
which the session's login put in its namespace. The system itself has no user identity; this is the
session's.

With no session there is nothing at `/session/user`, and `whoami` fails rather than invent a
name.

## Options

- `--help`: show the usage and exit.
- `--version`: show the version and exit.

`whoami` takes no other option and no operand.

## Output

`Table<{user: String}>`

One row.

## Exit status

0 when the name was written. 1 when `/session/user` is not bound or is empty, or the write failed.
2 for an option it does not know or an operand.

## Examples

```
whoami            # the session's user
```

## See also

`terminal(1)`, `nxsh(1)`, `shell(7)`
