# `views.toml` — the view broker's policy

**Status: normative for what is built (2026-09-29, Part E.6).** Read by `userspace/view-broker/`
(`view_broker::policy`) from `/system/views.toml`, **for every request**. The build seeds one;
an installed system's comes from the installer (administration Part G).

Conceptually `sudoers` — who may use which view, for which programs, proved how — but not its
syntax. A view is a profile's grants added to the caller's own namespace; see
[`administration.md`](../planning/administration.md) § *Views and the view broker*.

## Example

```toml
[profile.admin]
grants = ["disks", "storage", "views", "accounts", "services", "power", "clock", "logs"]

[profile.install]
grants = ["disks"]

[profile.power]
grants = ["power"]

[[rule]]
who  = ["alice"]      # accounts that may ask, or ["*"] for any
use  = ["admin"]      # profiles
run  = ["*"]          # programs by bare name, or ["*"] for any
auth = "password"     # "password" or "none"

[[rule]]
who  = ["alice"]
use  = ["install"]
run  = ["nxinstall"]
auth = "password"

# The person at the machine may power it off, as with a desktop's power button.
[[rule]]
who  = ["*"]
use  = ["power"]
run  = ["shutdown"]
auth = "none"
```

## Grammar

**A focused reader, not TOML.** What is accepted is exactly:

- `#` comments, whole-line or trailing, except inside a string;
- `[profile.<name>]` — a profile, named with letters, digits, `-`, `_` and `.`;
- `[[rule]]` — a rule;
- `key = value`, where a value is a string `"…"` or an array of strings `["…", "…"]` **on one
  line**, with no escapes: a string may not contain `"` or `\`.

Anything else — another section, a key outside a section, a multi-line array — is an error that
names its line.

## Profiles

| Key | Type | Meaning |
|---|---|---|
| `grants` | array | What the profile adds to the caller's namespace. |

**Grants** this broker knows, and a policy naming any other is refused rather than ignored:

| Grant | What it binds | Since |
|---|---|---|
| `disks` | every block device **not in use when the view is built**, raw: `/dev/blk/<n>` and its `info`, one binding each. The broker asks the storage service's `InUse` first, and leaves out a mounted filesystem's device, `init`'s root included, and the disk under it; if the service cannot answer, the request is refused rather than granted blind. **Asked once, at the start**: a device mounted later in the view's life stays bound raw. That is the administrator's own doing in a view with `storage` too, such as `disk --mount` from `with admin nxsh` | Part A; `InUse` since Part C.6 |
| `storage` | the storage service's admin endpoint at `/dev/storage/admin`: mounting and unmounting ([`rsproto-storage-ops.md`](rsproto-storage-ops.md)) | Part C.6 |
| `views` | the broker's policy endpoint at `/dev/policy`: reading this file and installing a new one, which `with --show` and `with --install` use ([`rsproto-views-ops.md`](rsproto-views-ops.md) § `Show`, `Install`). **Not** `/system/views.toml` writable: an install is judged first, so a policy that leaves no administrator never reaches the disk | Part D.2 |
| `accounts` | the broker's accounts endpoint at `/dev/accounts`: adding and removing accounts and setting their passwords ([`rsproto-views-ops.md`](rsproto-views-ops.md) § `AddAccount`, `RemoveAccount`, `SetPassword`). **Not** `/system/users` or `/home` writable: the broker checks the guards — a removal waits for logout and must leave an administrator — makes and removes homes, and asks `auth-service`, the file's only writer. Listing accounts and changing one's own password need no grant | Part D.3 |
| `services` | `service-mgr`'s admin endpoint at `/dev/services/admin`: starting, stopping and restarting services ([`rsproto-services-ops.md`](rsproto-services-ops.md)). `service-mgr` refuses an `essential` service's stop and restart itself, whoever holds this. Listing the services needs no grant: every session's `/dev/services` is the table | Part E.2b |
| `power` | `service-mgr`'s power endpoint at `/dev/power`: a session there takes `Shutdown`, to halt or reboot the machine, and nothing else ([`rsproto-services-ops.md`](rsproto-services-ops.md) § `Shutdown`). **The seeded policy gives it to everyone**, for `shutdown` alone and with no password, as a desktop's power button would; `admin` has it too | Part E.4d |
| `clock` | **nothing**: it is `SYSTEM_CLOCK` in the program's spawn, the kernel capability `sys_clock_set` asks for, so `date --set` can set the wall clock and the machine's hardware clock ([`syscall-abi.md`](syscall-abi.md) § `sys_clock_set`). The one grant that is not a binding. **The program's children get it only if it passes it on.** `nxsh` does, passing its stages whatever it holds, so `with admin nxsh` is a shell whose `date --set` works, as its stages reach the view's bindings | Part E.5 |
| `logs` | the logging service's read endpoint at `/dev/logs`: a resolve there opens a read session, whose `Read` answers from the service's ring — every service's records, and the view broker's audit among them ([`rsproto-log-ops.md`](rsproto-log-ops.md)). **Not** `/dev/log`, the kernel's own log. The broker holds the endpoint for the boot once it has been granted, as `power`'s | Part E.6 |

Each later part of the administration phase adds its grant to this table.

## Rules

| Key | Type | Meaning |
|---|---|---|
| `who` | array | The accounts that may ask, or `["*"]`. `*` with other names is an error. |
| `use` | array | Profiles, each defined in the file. |
| `run` | array | Programs by bare name — no `/` — or `["*"]`. |
| `auth` | string | `"password"`: the person's own password, checked by `auth-service`. `"none"`: nothing beyond being in the session. |

All four are required, and each may be given once.

## Decisions

**The first rule that matches the principal, the view and the program decides**, and none matching
is a denial. There are no `deny` rules yet, so "first" matters only for which `auth` applies.

A denial says what was missing: a view that does not exist, a view no rule lets the principal
use, or a program that view does not let them run.

**A policy that does not read denies everything**, and the reason — with its line — is logged and
returned to whoever asked.

## Administrators, and the guard

**An administrator is an account that exists, which a rule lets use a profile granting `views`,
with `run = ["*"]`** (`view_broker::policy::Policy::administrators`). To administer is to be able
to change this file, from any program:

- **The grant decides, not the profile's name.** A profile called `admin` without `views` makes
  no administrator; one under another name with it does.
- **`run = ["*"]` and nothing narrower**, so that a rule letting someone run one program with
  `views` does not count.
- **An account that exists**, as `auth-service` lists them. `who = ["*"]` counts every one of
  them; a rule naming only accounts that do not exist counts none.
- `auth` does not enter into it: `"none"` makes an administrator as `"password"` does.

A policy with no administrator reads, and the broker would decide requests by it, but
**`with --check` and `Install` both refuse it**, with the same list and the same definition,
because nothing could change the file again short of the live image. The file on the disk is left
as it was.

**The same definition guards the accounts** (Part D.3): `RemoveAccount` refuses to remove an
account when no account left could administer, so the last administrator cannot be removed any
more than the policy can be changed to leave none.

## References

- [`rsproto-views-ops.md`](rsproto-views-ops.md) — the protocol the broker speaks
