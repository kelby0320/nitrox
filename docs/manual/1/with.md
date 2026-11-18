# with — run a program in a view that grants more

## Synopsis

```
with VIEW PROGRAM [ARG...]
with --list
with --forget
with --check FILE
with --show [FILE]
with --install FILE
```

## Description

A session starts with the authority its account was given and no more. `with` runs one program with
more: in a view, which is this session's namespace plus what the view grants. A program that
administers the machine, such as `disk` or `account`, says in its page which view it needs.

`with VIEW PROGRAM [ARG...]` asks the view broker for VIEW. The broker answers from
`/system/views.toml`, which says which views the person asking may use, and for which a password
is wanted. PROGRAM is a bare name; it runs in the view, and its input and output are `with`'s
own. `with` ends with the program's exit status.

```
with admin disk --list
```

Where a password is wanted, `with` asks for it on the terminal, with the echo off, and tries up to
three times. The password is remembered for five minutes on this terminal, for that view alone;
`--forget` forgets every one the session has remembered. With no terminal to ask on, `with` fails.

`with` never holds the view's authority itself: the broker builds the view, the program runs in a
namespace `with` holds no handle to, and `with` only relays the streams and the exit.

## Options

- `--list`: list the views this account may use, as a table.
- `--forget`: forget every password this session has remembered.
- `--check FILE`: say whether FILE is a valid policy.
- `--show [FILE]`: print the policy, or write a copy of it to FILE for editing. It needs the
  `views` grant.
- `--install FILE`: check FILE and install it as the policy. It needs the `views` grant.
- `--help`: show the usage and exit.
- `--version`: show the version and exit.

An option stands alone, and `--list` and `--forget` take no operand.

`--show` and `--install` change the policy and are how it is edited: `--show FILE` writes a copy,
an editor changes it, `--install FILE` has the broker check it and replace the policy. The broker
refuses a policy that does not read, or that would leave no account able to administer. Outside a
view with the `views` grant they say so, and name `with admin with`.

## Output

The listing is `Table<{view: String, run: String, password: Bool}>`: the view's name, what it
runs, and whether it asks for a password. Where there is no output to write to, it is printed as
lines.

`--check` and `--install` say on the error output what the broker found. `--show` prints the
policy as text, or says where it wrote the copy.

A program run in a view writes what it writes.

## Grants

`with` needs none: it asks the view broker on `/dev/views`, which every session has. What the
views grant is the point of it. The programs that need one:

| Program | Grant | Run as |
|---|---|---|
| `disk` | `storage`, and `disks` to partition or format | `with admin disk ...` |
| `account` | `accounts` | `with admin account ...` |
| `service` | `services` | `with admin service ...` |
| `log` | `logs`, and `kernel-log` for `--kernel` | `with admin log ...` |
| `shutdown` | `power` | `with power shutdown` |
| `date --set` | `clock` | `with admin date --set ...` |
| `with --show`, `with --install` | `views` | `with admin with --show` |

## Exit status

The program's own exit status, once it ran. 1 when the program crashed, the broker did not answer,
the view was refused, no password was given, a password was wanted and there is no terminal, or the
policy was not valid. 2 for operands and options it cannot take.

## Examples

```
with admin disk --list           # run disk in the admin view
with power shutdown --reboot     # restart the machine
with --list                      # the views this account may use
with --forget                    # forget the remembered passwords
with admin with --show policy    # a copy of the policy, to edit
```

## See also

`views.toml(5)`, `disk(1)`, `account(1)`, `service(1)`, `log(1)`, `shutdown(1)`, `nxsh(1)`
