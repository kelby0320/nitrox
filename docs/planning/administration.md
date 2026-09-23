# Administration: views, devices, and the tools an installed system needs

**Status: scoped, not started (2026-09-22).** Scheduled after [the desktop refresh](desktop-refresh.md),
which is complete, and before Phase 6. The scope and the architecture below were agreed with the
maintainer on 2026-09-22. The parts are sketched, and **Part A's detail pass is next**. It began as a
stub on 2026-09-16, written while building the installer — the first program that needed authority
an ordinary session cannot have.

## Scope

| | |
|---|---|
| **In** | **Views**: running a program in a namespace with more — or, later, less — visibility and capability than its caller's, granted by a broker from a policy file. **The device manager**, with coldplug. **Storage**: detection, mount and unmount, `/storage`, auto-mount. **Accounts**: list, add, remove, passwords, and offline recovery. **Services**: list, start, stop, restart. **Power**: an orderly shutdown and reboot. **The clock.** **Reading the log**, and an audit record of every request. **The installer** becoming the broker's first client. |
| **Out** | **Networking** (the stack is yet to come). **Software installs** (not yet discussed; probably after the Rust `std` port). |
| **Deferred** | **Multiple disks** — the AHCI driver takes the *first implemented port with a SATA disk* and ignores the rest (deferred-decisions, *AHCI driver scope*). **Hot-plug event sources** — USB, Phase 6; this phase builds what they will feed. **Power-off** through ACPI S5, which needs AML (ACPICA). **The graphical prompt** is designed here and built when something needs it (below). |

## Decisions, 2026-09-22

- **Elevation is handle acquisition, not state change** — 5.1's principle, kept. A broker
  authenticates against `auth-service`, constructs a namespace with the right visibility, and spawns
  the program into it. In this system most permissions *are* what a namespace makes visible.
- **No "run as another user."** That is a Unix idea, and this system largely has no users. The
  target of a request is never an account. It is a **view**: a named set of visibility and
  capability. Accounts appear only on the *who may ask* side.
- **The same mechanism narrows as well as widens** — a sandboxed application is a view too. So the
  word is not "elevate". **The command is `with`**: `with admin disk --mount /dev/blk/1` today, and
  `with sandbox nxedit notes.txt` later.
- **One broker**, with the authority it grants kept by the services that own each domain.
- **A general device manager**, not a storage-only mount daemon — keyboards and mice arrive as well
  as disks. It is built now, with **coldplug**.
- **Mounted filesystems appear under `/storage/<label>`**, and new ones **auto-mount**.
- **One command per domain, with flags as verbs** — `disk`, `account`, `service` — matching the
  existing `clip --copy`.
- **Shutdown is old school.** Flush everything, then *"It is now safe to turn off your computer."*
- **A complete pass.** Everything the 2026-09-22 review of this plan found missing is in scope.
- **The initramfs holds only what it takes to boot and mount the root filesystem**; everything else
  comes up from root. The live image is the one exception: it has to run entirely from memory
  because no root filesystem is attached — a special case, not a precedent.

## What the 5.1 design said

`docs/archive/os-design-v5.1.md` sketched most of this:

- **A Privilege Broker**: "escalation is **handle acquisition**, not state change … authenticates,
  constructs a new namespace with admin resources, spawns a new process with elevated handles."
- **Tiered `/dev` namespaces** (`minimal_dev` … `full_dev`), and namespace recipes per role —
  standard user, administrator, and a sandboxed application.
- **A Device Manager** ("device tree watcher, kernel module loader, `/dev` namespace population"), a
  **Mount Daemon** for post-boot mounts, and non-critical mounts owned by `service-mgr` rather than
  `init`.
- **A chained, tamper-evident audit log**, and **a system-control handle** in `init`'s boot grant.

Phase 3 deferred the device manager, the mount daemon and the audit subsystem for want of a
consumer. `session-and-auth.md` defers "a privilege broker." This phase is the consumer for most of
them.

## What exists to build on — and what does not (checked 2026-09-22)

- **`auth-service`** answers one op, `Authenticate`. `/system/users` is one line per account —
  `name:salt:iterations:verifier:home` — seeded by the build and read once at startup. Nothing
  writes it, and the auth spec defers management ops "with their consumers."
- **`libsession::build_namespace(NamespaceSpec)`** builds a namespace from a recipe of flags, and
  `bind_blk`'s own doc anticipates this phase. **That is the grant mechanism.** Namespace
  *layering* — one namespace over another — is designed and deferred
  (`profiles-and-namespace-projection.md`), so a view is **built, not layered**.
- **Per-device `/dev/blk` binding** (Phase 5 H.1). A supervisor cannot rebind the `/dev/blk`
  kernel server, so devices are bound one at a time.
- **`service-mgr`** has a control channel per service it spawns, and **no client-facing endpoint**.
  Its declarations are one TOML file **inside the initramfs**, `/initramfs/etc/services.toml` — a
  boot archive on the EFI partition that nothing here can write.
- **The initramfs's programs already follow the rule above; its configuration does not.**
  `xtask`'s `INITRAMFS_PROGRAMS` lists four programs, each with the reason it cannot come from the
  filesystem, and a size tripwire fails the build if a fifth creeps in. But three files ride along:
  `init.toml` has a bootstrap reason (it names the root mount); `services.toml` has none
  (`service-mgr` itself runs from `/bin`, on root); and `profiles/system.toml` probably has none
  either, since `profile-server` runs after root is mounted and the packages it lists live there.
- **`input-server`** already owns input hotplug by design (`input-subsystem.md` §2: "merge ·
  policy · hotplug", holding every raw node). Today it only sees what exists at boot.
- **`logging-service`** receives and does not serve: **no read-back op**.
- **`SysCaps::SYSTEM_CLOCK` and `AUDIT_CONTROL` are defined and not wired.**
- **No power syscall; FADT is not parsed** (`rtc.rs` says so); **no orderly shutdown**.
- **No durability point.** `sys_file_sync` works per file. `fs-server-ext4` has no whole-filesystem
  sync, no unmount, and no clean/dirty state — `mkfs` writes "clean" and nothing ever changes it.
  The drive's cache is never flushed (`TODO(ahci-flush)`).
- **Listing `/dev` was deferred** "until a device manager or a real enumeration."
- **An installed system inherits the build's demo account**, because `nxinstall` copies the release
  root as it is.

## Views and the view broker

### Identity is the endpoint you call on

There are no UIDs, and a name the caller supplies proves nothing. **At login, the supervisor that
builds the session has the broker mint an endpoint for that session's principal, and binds it at
`/dev/views`** — `session-mgr` for the serial column and `desktop-session-mgr` for the graphical one.
**Both must**, or a session exists that the broker cannot see. A request
arriving on it is from that principal, and nothing else in the session can produce one — the
logging service already works this way, with identity set by the supervisor. The password then
confirms the person at the keyboard is that principal.

The endpoint also records **the recipe the session was built from**, which a view extends (below).
And because every session's endpoint is minted by the broker and closes when the session ends, **the
broker knows which sessions are live**: it can end a session's programs when that session ends, and
it can answer "who is logged in" without a separate registry.

### A view is your session plus a profile

The elevated program still needs your files and your terminal: an editor opened with `with admin`
still opens `notes.txt`, and `with admin disk --mount …` runs where you are. So **a view is the
caller's session recipe plus a profile's grants** — not 5.1's standalone administrator namespace,
which had no `/home`. With no layering in the kernel, the broker *rebuilds*: the session's recipe,
with the profile's grants added, through the same `NamespaceSpec`.

**One recipe, three builders.** `session-mgr` builds sessions, `desktop-shell` builds each
application's namespace, and now the broker builds views. If each keeps its own recipe, they drift.
All three should build from `libsession`'s spec — which is also what makes a sandbox view a small
step later, since `desktop-shell` would then just ask for one.

### Policy: `/system/views.toml`

Conceptually `sudoers` — who may use which view, for which programs, proved how — but not its syntax:

```toml
# A profile is a named set of grants: bindings the broker adds, and capabilities it passes on.
[profile.admin]
grants = ["disks", "storage", "accounts", "services", "power", "clock", "logs", "views"]

[profile.storage]
grants = ["disks", "storage"]

# Rules: the first that matches decides. None matching is a denial.
[[rule]]
who  = ["kelby"]     # accounts that may ask, or "*" for any that can log in
use  = ["admin"]     # profiles
run  = ["*"]         # programs, resolved in the view being built
auth = "password"    # password · none

[[rule]]
who  = ["*"]
use  = ["admin"]
run  = ["shutdown"]
auth = "none"        # the person at the machine may power it off
```

- **"Admin" lives only in this file.** `/system/users` stays credentials-only. "Is kelby an
  administrator" means "does a rule let kelby use `admin`."
- **A policy that fails to parse denies everything**, and says why in the log. **`with --edit`**
  edits a copy, checks it, and replaces the file atomically (`visudo`'s job), and **`with --check
  <file>`** validates without installing. A lockout is recovered from the live image (*Accounts*).
- **For later:** groups, constraints on arguments, `deny` rules, and a timestamp that skips the
  prompt for a second request within minutes.

### Grants

| Grant | Bound or passed as | Owned by |
|---|---|---|
| `disks` | `/dev/blk/<n>` and its `info`, per device (H.1's mechanism) | the kernel |
| `storage` | the storage service's admin endpoint | the storage service |
| `accounts` | `auth-service`'s admin endpoint, and the root of `/home` | `auth-service` |
| `services` | `service-mgr`'s admin endpoint | `service-mgr` |
| `power` | the endpoint that runs the shutdown sequence | `service-mgr` → `init` |
| `clock` | `SysCaps::SYSTEM_CLOCK`, passed on at spawn | the kernel |
| `logs` | `logging-service`'s read endpoint | `logging-service` |
| `views` | `/system/views.toml`, writable | the filesystem |

### Why one broker

To grant something the broker must hold it, so a single broker is one process that is, collectively,
root — the reason a `sudo` vulnerability on Unix is severe. Several brokers, one per domain, would
contain a bug to one domain, at the cost of several policies, prompts and endpoints, and a person who
has to know which broker to ask.

**Neither: one broker, with the authority kept by the services that own each domain.** The broker
does what must be in one place — identity, the password, the policy, the audit, and the spawn. What
it grants is mostly an endpoint to a domain service, which keeps enforcing its own rules:
`auth-service` refuses to remove the last account that can use `admin`, the storage service refuses
to unmount `/`, and `service-mgr` refuses to stop itself. A broker bug then means "can make admin
requests of those services", not "holds the disks and the password file." The one raw grant is
`disks`, which can name a single device. One front door, several vaults.

### The prompt

- **On a terminal**, `with` hands the broker its tty, and the broker prompts with echo off, as the
  login prompt does. **A failed attempt is slow and counted**: a delay after each failure, a cap per
  request, and every failure in the log. Any program in the session can call `/dev/views` in a loop,
  and this is what stops that becoming a password-guessing service.
- **Graphical — designed here, built when needed.** A UAC-style window asking for permission.
  What makes it more than a dialog is that **only the broker can open it**, and the compositor
  draws it in a way no application can imitate — the desktop dimmed behind it, as Windows does.
  **Trigger:** the first desktop surface needing an action the policy will not allow without a
  password. *Shut down* is not one: the rule above lets the person at the machine power off, as
  every desktop does. The first real candidates are unmounting a USB stick from Files (Phase 6)
  and a Settings application.

### The spawn

- **Streams**: `with` passes its stdin, stdout and stderr handles, and the program writes straight
  into the caller's pipeline. `with admin disk --list | filter …` composes like any other stage.
- **The terminal**: an interactive program — `with admin nxsh`, an elevated editor — needs
  `/dev/tty`, not only streams, so the caller's tty is bound into the view.
- **Stopping it**: the program's parent is the broker, not the shell, and the shell interrupts a
  pipeline through the processes *it* spawned. So **the broker hands back a handle the shell can
  stop it through**, and Ctrl-C reaches an elevated program as it reaches any other.
- **Its lifetime**: when the session that asked ends, the broker ends what it started for it.
- **What the caller never gets is the authority.** The program runs in a namespace the caller cannot
  name. The caller receives its output and its exit status.

### The audit

Every request — who, which view, which program, allowed or denied, and why, failures included — is a
record in the log. 5.1's chained, tamper-evident audit subsystem stays deferred.

### The word `with`

`as`, `in` and `use` are already `nxsh` keywords, which rules out the obvious phrasings. `with` is not
reserved, and **the shell spec should record that it is taken by `/bin`**, so a future keyword does
not claim it. If one ever must, `nxsh`'s force-external prefix still reaches the program: `^with`.

## The device manager

**It learns that a device arrived or left, and hands it to whatever owns that class.** It does not
drive devices. A keyboard or mouse goes to `input-server`, which already owns input hotplug and keeps
one merged stream while devices come and go. A disk goes to the storage service. Each class's
lifetime logic stays with that class's owner.

**Coldplug, built now.** At startup, every device present is announced as an arrival, which is how
Linux's `udev` treats devices present at boot. Every consumer is written against "a device arrived"
from the start, so Phase 6 adds an event source — USB — without rewriting any consumer. Today
`input-server` and `init` both assume a device set fixed at boot.

**It is the one component that sees all of `/dev`**, which answers the deferred `/dev` listing
("until a device manager or a real enumeration"), and **it serves read-only device information to
anyone**. `disk --list` therefore needs no elevation, and only the raw device does. It is also the
one component whose bugs reach every device, so it should do as little as possible.

**Kernel work:** a way to enumerate the device registry, and — built with its first event source in
Phase 6 — a notification when a device node is published or withdrawn.

## Storage

**Raw devices and filesystems are different things.** `/dev/blk/<n>` is bytes, for elevated tools
such as the installer. What a person browses is a *filesystem*, which appears once mounted:

- **`/storage/<label>`** is bound into every session and every application namespace, and served by
  the **storage service** — the class owner the device manager hands disks to. A mount makes
  `/storage/<label>` appear, and nothing is re-bound.
- **Names are labels, not numbers.** `/dev/blk/<n>` is discovery order, so a stick that is `2` today
  is `3` tomorrow. `/storage/<label>` is also the stable name anything persistent should use, as
  `init.toml` already identifies devices by label or UUID rather than position. A clash of labels
  gets a suffix.
- **Auto-mount**: a disk arrives, the storage service reads its filesystem header, and a filesystem
  it can read that is not already mounted appears under `/storage`. `init`'s boot mounts stay
  `init`'s: they are reported, and never auto-mounted or unmounted.
- **Mount** spawns an `fs-server-ext4` over the device. **Unmount** syncs that filesystem — a new
  `fs-server` op, since today there is only `sys_file_sync` per file — flushes the drive
  (`TODO(ahci-flush)`, triggered now), and only then tears the server down.
- **A filesystem knows how it was left.** Mounting marks the ext4 superblock *in use* and unmounting
  marks it *clean*. `disk --list` reports a filesystem that was not unmounted cleanly, which, until
  this phase ships `shutdown`, is every one. A repair tool (`fsck`) stays deferred; the state it
  would need does not.
- **Testing without a second SATA disk.** The AHCI driver takes one disk, so QEMU cannot supply a
  second. A second boot module can, as a RAM disk — `check-live` already turns one into a disk. That
  exercises detection, auto-mount and unmount now.
- **Formatting and partitioning** are left to Phase 6, when a USB stick is the first thing outside
  the installer that wants them. `nxinstall` already does both through `libgpt` and
  `fs-server-ext4`'s library, so they will be thin.

## Accounts

- **`auth-service` gains admin ops** — list, add, remove, set a password — with an atomic rewrite of
  `/system/users` and a reload. The credential store stays one service's, and the file has one
  writer. `auth-service` stays a credential oracle: **the tool makes the home**, through the
  `accounts` grant's `/home` binding.
- **Your own password needs no view**: the proof is the current password.
- **Removing an account** keeps its home unless asked, refuses the last account a rule lets use
  `admin`, and ends its live sessions — which the broker can now see.
- **Recovery is the live image.** Boot it, mount the installed disk, and reset a password in **that
  disk's** `/system/users`, not the running system's. So `account` needs a small **offline mode**
  that operates on a users file named by path. It gets its own gate, because it is the one path
  nobody exercises until they need it.

## Services

`service-mgr` gains an admin endpoint: **list** — each service's name, state and restart count, for
anyone — and **start, stop and restart** under the `services` grant.

**Moving the declarations onto the root filesystem — proposed for Part E.** `services.toml` has
no reason to be in the initramfs (above), and it cannot be edited there: the initramfs is a boot
archive on the EFI partition, which is FAT, and nothing here writes FAT. On root, `service-mgr`
reads it after `init` has mounted the filesystem it lives on, and **enabling and disabling become a
small edit to it** — still deferred, since no service wants disabling yet, but no longer a
different problem. `profiles/system.toml` probably follows it, for the same reason. The live image
keeps its copy in memory, where it has nowhere else to be. **`check-images` changes shape with
it**: test and release images would then differ in a root-filesystem file rather than an initramfs
one, which the gate's allow-list has to learn.

## Power

- **`shutdown`**: `service-mgr` stops the services, the storage service unmounts everything it
  mounted, `init`'s filesystems are synced and marked clean, the drive is flushed, the kernel stops,
  and the framebuffer console — already built to draw after the desktop has gone — says *"It is now
  safe to turn off your computer."*
- **`shutdown --reboot`**: the same sequence, then a reset. **This needs no AML.** The FADT reset
  register is a plain table field, though FADT is not parsed yet. The keyboard controller's reset
  pulse is the fallback, and the laptop's keyboard is on the i8042 we already drive.
- **Kernel work:** a power operation gated behind the system-control handle 5.1 gave `init`, rather
  than a new syscap. **Power-off** (S5) stays deferred with ACPICA.

## The clock

`date --set`, with `SYSTEM_CLOCK` wired: the kernel sets the wall clock and writes it back to the RTC.
Network time belongs to networking.

## Logs

`logging-service` gains a **read op on its in-memory ring**. `log [<service>]` reads it under the
`logs` grant, since the log holds every service's and session's output. The audit records live here.

## The commands

| Command | Does | Needs |
|---|---|---|
| `with <view> <program> [args]` | run a program in a view | a rule's say-so, and its proof |
| `with --list` · `with --check <file>` · `with --edit` | what I may use · validate a policy · edit it safely | nothing · nothing · `views` |
| `disk --list` | devices, partitions, filesystems, where each is mounted, and whether each was left clean | nothing |
| `disk --mount <device> [<label>]` · `disk --unmount <label>` | mount at `/storage/<label>` · sync, flush, unmount | `storage` |
| `account --list` | accounts, and which have a live session | nothing |
| `account --add <name>` · `account --remove <name>` | with a home · refusing the last administrator | `accounts` |
| `account --password [<name>]` | your own (proof: the current one) · someone else's | nothing · `accounts` |
| `account --password <name> --users <file>` | the offline mode, for recovery | the file |
| `service --list` | each service's state and restart count | nothing |
| `service --start\|--stop\|--restart <name>` | | `services` |
| `shutdown` · `shutdown --reboot` | the orderly sequence, then a message or a reset | `power` |
| `date --set <time>` | set the clock | `clock` |
| `log [<service>]` | read the system log, audit records included | `logs` |

Every `--list` produces a typed table, as `list` does, so it composes with `filter`, `sort` and the
rest.

## Parts — sketched

- [ ] **A — the view broker, on a terminal.** The broker service; the per-session `/dev/views` that
      both login columns bind at login, carrying the session's recipe; `with`, `--list` and `--check`;
      `views.toml` and its schema spec; the prompt — echo off, delayed and counted failures; views
      built from the shared recipe; **one grant end to end, `disks`**; streams, the tty, and a stop
      handle for the shell; programs ended with their session; an audit record per request. The
      build images carry a seeded `views.toml` that makes the demo account an administrator.
- [ ] **B — the device manager, with coldplug.** Enumeration of the device registry; arrivals
      announced for everything present at boot; `input-server` taking its devices from it; read-only
      device information for anyone; the `/dev` listing deferral closed.
- [ ] **C — storage.** A whole-filesystem sync and clean/dirty state in `fs-server-ext4`;
      `TODO(ahci-flush)`; the storage service — mount, unmount, auto-mount, `/storage` bound into
      sessions and application namespaces; `disk`.
- [ ] **D — accounts.** `auth-service`'s admin ops and atomic rewrite; `account`, including the
      offline mode; the last-administrator guard; ending a removed account's sessions.
- [ ] **E — services, power, the clock and the log.** `services.toml` moved from the initramfs onto
      the root filesystem (proposed); `service-mgr`'s admin endpoint and `service`;
      the kernel power operation, FADT's reset register or the i8042 pulse, and `shutdown`;
      `SYSTEM_CLOCK` and `date --set`; the log's read op and `log`.
- [ ] **F — the desktop's share.** `desktop-shell` builds application namespaces from the shared
      recipe; the graphical prompt's design written down, with its trigger.
- [ ] **G — the installer, the broker's first client** (decided 2026-09-17). `with admin nxinstall`
      from an ordinary desktop, with the binary unchanged; the installer creating the first account
      and the `views.toml` that makes it an administrator, instead of inheriting the build's demo
      account.

**A comes first**, since everything else is a grant it hands out. **B before C**, since storage is
the device manager's first consumer. D and E are independent, **F can go anywhere after A**, and **G
closes the phase**.

## Gates

| Part | What proves it |
|---|---|
| A | `test-interactive`: a request allowed, one denied by policy, wrong passwords delayed and capped, an audit record for each, and Ctrl-C stopping a program started with `with` |
| B | every boot device announced as an arrival, in `test-qemu` and `check-live`, and keyboard and mouse still reaching a window |
| C | a boot with a second boot-module disk: auto-mounted under `/storage`, written, unmounted, and then found clean by `e2fsck` on the host, as `check-install` checks |
| D | `account --add`, `--password` and `--remove` at a real prompt; and **a recovery gate**, on demand like `check-install`: boot the live image, reset a password on the installed disk offline, boot that disk, and log in with the new one |
| E | **a shutdown gate**: run `shutdown`, read the message off the screen with `check-fbcon`'s reader, then `e2fsck` the disk on the host — clean, and marked clean. `shutdown --reboot` seen as a second boot |
| G | `check-install` driving `with admin nxinstall` from the desktop, in place of the installer's boot entry |

## Deferred from this phase

Networking. Software installs. Multiple disks and controllers. Hot-plug event sources (Phase 6,
USB). Power-off through ACPI S5. The graphical prompt's build, until its trigger. Formatting and
partitioning (Phase 6). Enabling and disabling services persistently (a small edit once
`services.toml` is on root). A filesystem repair tool. The
chained audit subsystem. Policy groups, argument constraints, `deny` rules and prompt caching.
Namespace layering, which views would use if it existed.
