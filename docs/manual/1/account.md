# account — the accounts on this machine

## Synopsis

```
account --list
account --password
account --add NAME
account --remove NAME [--home]
account --password NAME [--users FILE]
```

## Description

`account` shows the accounts, adds and removes them, and changes passwords. One verb is given at a
time.

`account --list` and `account --password` with no name need no grant, in any session. `--password`
changes the person's own password: it asks for the current one, then a new one twice.

`--add`, `--remove` and `--password NAME` need the `accounts` grant: run them with
`with admin`. Without it `account` says so and names `with`. The view broker does the refusing that
matters, and its reason is printed as it gave it: an account that is logged in cannot be removed,
and neither can one whose removal would leave nobody able to administer.

`--add NAME` makes the account with a home at `/home/NAME`, and asks for its password twice.
`--remove NAME` keeps the account's home unless `--home` is given.

`--password NAME --users FILE` sets NAME's password in FILE, a users file, directly, with no
service. It is for recovery from the live image, where an installed disk's users file can be
reached once `with admin disk` has mounted it. The running system's own file is never reached this
way.

Passwords are asked for on the terminal, with the echo off. They are never read from a pipe,
echoed or logged. What the change did is said as a sentence on the error output.

## Options

- `--list`: every account, its home, how many sessions it has open, and whether it could
  administer.
- `--add NAME`: add NAME, asking for its password twice.
- `--remove NAME`: remove NAME.
- `--home`: with `--remove`, remove the account's home too.
- `--password [NAME]`: change your own password, or with a NAME set that account's.
- `--users FILE`: with `--password NAME`, set it in FILE directly.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

## Output

`--list` writes `Table<{name: String, home: String, sessions: Int, administers: Bool}>`. The other
verbs write no table.

## Grants

`--list` and `--password` for one's own account need none. `--add`, `--remove` and `--password NAME`
need `accounts`, so `with admin account ...`. `--users FILE` needs none.

## Exit status

0 when it was done. 1 when it was refused or failed, or a password was not given. 2 for an option
it does not know, or a combination of verbs, flags and operands that fits none of the forms above.

## Examples

```
account --list                       # every account
account --password                   # change your own password
with admin account --add dana        # add an account
with admin account --remove dana --home   # remove it and its home
```

## See also

`with(1)`, `disk(1)`, `views.toml(5)`
