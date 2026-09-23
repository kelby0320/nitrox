# Administration: views, devices, and the tools an installed system needs

**Status: scoped, not started (2026-09-22; revised after the PR #326 review).** Scheduled after
[the desktop refresh](desktop-refresh.md), which is complete, and before Phase 6. The scope and the
architecture below were agreed with the maintainer on 2026-09-22. The review then found that
several mechanisms depend on things the code does not have, and **the maintainer took the four
resolutions that needed a decision the same day** (the last item under *Decisions*). The parts are
sketched, and **Part A's detail pass is next**. The plan began as a stub on 2026-09-16, written while building the
installer — the first program that needed authority an ordinary session cannot have.

## Scope

| | |
|---|---|
| **In** | **Views**: running a program in a namespace with more — or, later, less — visibility and capability than its caller's, granted by a broker from a policy file. **The device manager**, with coldplug. **Storage**: detection, mount and unmount, `/storage`, auto-mount. **Accounts**: list, add, remove, passwords, and offline recovery. **Services**: list, start, stop, restart. **Power**: an orderly shutdown and reboot. **The clock.** **Reading the log**, and an audit record of every request. **The installer** becoming the broker's first client. |
| **Out** | **Networking** (the stack is yet to come). **Software installs** (not yet discussed; probably after the Rust `std` port). |
| **Deferred** | **Multiple disks** — the AHCI driver takes the *first implemented port with a SATA disk* and ignores the rest (deferred-decisions, *AHCI driver scope*). **Hot-plug event sources**, **module matching**, **formatting and partitioning**, and **FAT** — all to Phase 6, where USB makes them necessary; this phase builds what they plug into. **Power-off** through ACPI S5, which needs AML (ACPICA). **The graphical prompt** is designed here and built when something needs it. |

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
- **One broker**, with the authority it grants kept, where possible, by the services that own each
  domain. What that does and does not buy is stated plainly under *Why one broker*.
- **A general device manager**, not a storage-only mount daemon — keyboards and mice arrive as well
  as disks. It is built now, with **coldplug**, and it is the same component Phase 6's driver
  manager extends.
- **Mounted filesystems appear under `/storage/<label>`**, and new ones **auto-mount**.
- **One command per domain, with flags as verbs** — `disk`, `account`, `service` — matching the
  existing `clip --copy`.
- **Shutdown is old school.** Flush everything, then *"It is now safe to turn off your computer."*
- **A complete pass.** Everything the 2026-09-22 review of this plan found missing is in scope.
- **The initramfs holds only what it takes to boot and mount the root filesystem**; everything else
  comes up from root. **The live image is not an exception to this**, despite appearances. It mounts
  a root too — the `nitrox-live` partition inside `root.img`, which the kernel publishes as a RAM disk —
  and its initramfs is the release one with only `init.toml` changed, which `check-images` enforces.
  The one live-only file is `etc/install-allowed`, a policy marker kept in the initramfs precisely
  because `root.img` must match the release root file for file.
- **After the PR #326 review, four more**, each argued where it appears below: **views are derived**
  from the caller's namespace by a new kernel operation, not rebuilt from a session's ingredients;
  **`/storage` is built on `OBJECT_KIND_SUBNAMESPACE`**; **the installer's boot entry stays, and its
  session becomes an ordinary one** running `with admin nxinstall`; and **`services.toml` moves onto
  the root filesystem** in Part E.

## What the 5.1 design said

`docs/archive/os-design-v5.1.md` sketched most of this:

- **A Privilege Broker**: "escalation is **handle acquisition**, not state change … authenticates,
  constructs a new namespace with admin resources, spawns a new process with elevated handles."
- **Tiered `/dev` namespaces** (`minimal_dev` … `full_dev`), and namespace recipes per role —
  standard user, administrator, and a sandboxed application.
- **A Device Manager** ("device tree watcher, kernel module loader, `/dev` namespace population") —
  *one* component for arrivals and for matching drivers — a **Mount Daemon** for post-boot mounts, and
  non-critical mounts owned by `service-mgr` rather than `init`.
- **A chained, tamper-evident audit log**, and **a system-control handle** in `init`'s boot grant —
  which was never built.

Phase 3 deferred the device manager, the mount daemon and the audit subsystem for want of a
consumer. `session-and-auth.md` defers "a privilege broker." This phase is the consumer for most of
them.

## What exists to build on — and what does not (checked 2026-09-22, and by the PR #326 review)

- **`auth-service`** answers one op, `Authenticate`. `/system/users` is one line per account —
  `name:salt:iterations:verifier:home` — seeded by the build and read once at startup. Nothing
  writes it, and the auth spec defers management ops "with their consumers."
- **`libsession::build_namespace(NamespaceSpec)`** builds a namespace from a recipe, and `bind_blk`'s
  own doc anticipates this phase. **But a recipe is handles, not data**: `NamespaceSpec` carries the
  *whole-tree* fs-server endpoint and scopes `/home/<user>` at bind time, plus the profile, tty,
  clipboard and (graphical) draw endpoints. Whoever rebuilds a session must hold all of them — which
  is why `desktop-shell` already holds the whole-tree fs endpoint. Namespace *layering* is designed
  and deferred (`profiles-and-namespace-projection.md`).
- **Per-device `/dev/blk` binding** (Phase 5 H.1): a supervisor cannot rebind the `/dev/blk` kernel
  server, so `rebind_block_devices` resolves each device from **the grantor's own namespace** and
  binds it. The grantor must see every disk it grants.
- **No program reads a terminal.** A stage's stdin is a typed stream. The terminal is a handle in
  the setup message, not a name: `/dev/tty` in an application namespace *mints a fresh terminal*,
  and binding a minted tty channel into a namespace produces an entry that answers `Resolve` with
  `Unsupported` (`libsession`). While a pipeline runs, `nxsh`'s `drain_tty_interrupt` receives
  **every** message on its terminal channel and discards what is not an interrupt, so a stage handed
  a duplicate of that channel would lose its replies. This is why `nxinstall` takes its confirmation
  as an operand rather than asking.
- **`service-mgr`** has a control channel per service it spawns, and **no client-facing endpoint**.
  Its declarations are one TOML file **inside the initramfs**, `/initramfs/etc/services.toml` — a
  boot archive on the FAT EFI partition that nothing here can write.
- **The initramfs's programs follow the rule above; its configuration does not.** `xtask`'s
  `INITRAMFS_PROGRAMS` lists four programs, each with the reason it cannot come from the filesystem,
  and a 384 KiB tripwire fails the build if a fifth creeps in. Three configuration files ride along:
  `init.toml` has a bootstrap reason (it names the root mount); `services.toml` has none
  (`service-mgr` itself runs from `/bin`, on root); and `profiles/system.toml` probably has none
  either, since `profile-server` runs after root is mounted and the packages it lists live there.
- **A namespace binding shares its server's registration**: one registration serves every binding of
  it, and its kernel end is released only when the last binding goes.
- **A resolve cannot continue into another namespace.** `OBJECT_KIND_SUBNAMESPACE` is defined in
  `librsproto` and marked deferred in the namespace-ops spec, with no kernel handling. And a lazily
  filled `FileObject` fills through *the registration it was resolved through*.
- **`input-server`** already owns input hotplug by design (`input-subsystem.md` §2: "merge ·
  policy · hotplug", holding every raw node). Today it only sees what exists at boot.
- **`logging-service`** receives and does not serve: **no read-back op**.
- **`SysCaps::SYSTEM_CLOCK` and `AUDIT_CONTROL` are defined and not wired.**
- **No power syscall, no system-control handle, and FADT is not parsed** (`rtc.rs` says so). No
  orderly shutdown.
- **No durability point, and the page cache is part of the problem.** `sys_file_sync` is the *only*
  writeback trigger. When a `FileObject`'s last reference goes, it frees its frames without writing
  them back — so data written through a mapping and never synced is lost when its writer exits.
  Nothing loses data today, because every file writer (`libfs`, `nxsh`) syncs before it lets go.
  `fs-server-ext4` has no whole-filesystem sync, no unmount, and no clean/dirty state (`mkfs` writes
  "clean" and nothing changes it), and the drive's cache is never flushed (`TODO(ahci-flush)`).
- **`/dev` lists, but `/dev/blk` does not.** Phase 4's D3 made `list` show namespace bindings
  beside filesystem entries (2026-07-29), but a kernel server owning a subtree is one binding, so
  `blk` lists as empty: nothing can ask the kernel what it would serve there.
- **The installable ESP rides on the installer's boot entry alone** — a 33 MiB module Limine loads
  only for that entry — and `nxinstall` fails without it.
- **An installed system inherits the build's demo account**, because `nxinstall` copies the release
  root as it is.

## Views and the view broker

### Identity is the endpoint you call on

There are no UIDs, and a name the caller supplies proves nothing. **At login, the supervisor that
builds the session has the broker mint an endpoint for that session's principal, and binds it at
`/dev/views`** — `session-mgr` for the serial column and `desktop-session-mgr` for the graphical one.
**Both must**, or a session exists that the broker cannot see. A request arriving on that endpoint
is from that principal, and nothing else in the session can produce one — the logging service
already works this way, with identity set by the supervisor. The password then confirms the person
at the keyboard is that principal.

**When a session ends, its supervisor tells the broker** — it already waits on the session leader.
The endpoint closing cannot be the signal: a view includes the caller's `/dev/views` (so `with`
works inside `with admin nxsh`), and a binding keeps its registration alive, so an elevated program
would keep its session's endpoint open after logout — exactly the programs the broker is meant to
end. With the supervisor's notice, the broker knows which sessions are live: it ends what it started
for a session when that session ends, and it can answer "who is logged in" without a registry.

### A view is your namespace plus a profile

The program still needs your files and your terminal: an editor opened with `with admin` still
opens `notes.txt`, and `with admin disk --mount …` runs where you are. So **a view is the caller's
namespace plus a profile's grants** — not 5.1's standalone administrator namespace, which had no
`/home`.

**How it is built decides what the broker must hold.** Rebuilding the caller's session from a recipe
means holding everything the recipe is made of — the whole-tree filesystem endpoint above all, which
reaches `/system/users`. **Decided instead: a kernel operation that derives a new namespace from an
existing one**, a copy of its bindings. `with` passes its own namespace, the broker derives a view
from it and binds the profile's grants in. The broker then holds **only what it adds**, never a
session's ingredients, and there is no recipe to record. A caller can only pass a namespace it
already holds, so the most it can gain is the profile's grants. This is the first step of the
namespace layering already designed and deferred, and not all of it.

**One vocabulary, three builders.** `session-mgr` builds sessions, `desktop-shell` builds each
application's namespace, and the broker builds views. They should agree on what the pieces of a
namespace are, through `libsession`'s spec, so a sandbox view later is `desktop-shell` asking for
one rather than a fourth implementation.

### Policy: `/system/views.toml`

Conceptually `sudoers` — who may use which view, for which programs, proved how — but not its syntax:

```toml
# A profile is a named set of grants: bindings the broker adds, and capabilities it passes on.
[profile.admin]
grants = ["disks", "storage", "accounts", "services", "power", "clock", "logs", "views"]

[profile.power]
grants = ["power"]

# Rules: the first that matches decides. None matching is a denial.
[[rule]]
who  = ["kelby"]     # accounts that may ask, or "*" for any that can log in
use  = ["admin"]     # profiles
run  = ["*"]         # programs, resolved in the view being built
auth = "password"    # password · none

[[rule]]
who  = ["*"]
use  = ["power"]
run  = ["shutdown"]
auth = "none"        # the person at the machine may power it off
```

- **"Admin" lives only in this file.** `/system/users` stays credentials-only.
- **An *administrator* is an account a rule lets use `admin` with `run = ["*"]`** — narrow on purpose.
  A rule granting one program does not make an administrator, which is why powering off has a
  profile of its own rather than a rule on `admin`.
- **The last-administrator guard lives with the policy.** The broker is the only component that
  reads this file, so it is the one that can answer "would this leave no administrator". `with
  --check` and `with --edit` refuse a policy that does, and `auth-service` asks the broker before it
  removes an account. A policy with no administrator is recoverable only from the live image.
- **A policy that fails to parse denies everything**, and says why in the log. **`with --edit`**
  edits a copy, checks it, and replaces the file atomically (`visudo`'s job); **`with --check
  <file>`** validates without installing.
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

**A device the storage service has mounted is not granted raw.** Granting `disks` for it first asks
the storage service to unmount it, and fails if it cannot. Otherwise `with admin nxinstall` could
rewrite a disk underneath a live `fs-server`, whose next write — at the latest, unmount's "mark
clean" — would land on the new filesystem.

### Why one broker

To grant something the broker must hold it. Several brokers, one per domain, would contain a bug to
one domain, at the cost of several policies, prompts and endpoints, and a person who has to know
which broker to ask. **This plan has one broker, and should say plainly what that means.**

**For most grants, the authority stays with a domain service.** What the broker holds is an endpoint
to it, and the service keeps enforcing its own rules: the storage service refuses to unmount `/`,
`service-mgr` refuses to stop itself, and `auth-service` removes an account only after the broker
confirms an administrator remains. A broker bug there means "can make admin requests of those
services."

**Two grants are raw, and a broker bug reaches them directly.** `disks` needs every block device in
the broker's own namespace, because a device is granted by resolving it there. The clock is a syscap
it holds. **It does not hold the whole filesystem only because views are derived** rather than
rebuilt from a session (above), which would mean holding each session's fs endpoint. So the honest
summary: a broker bug reaches the disks, the clock, and whatever the domain services will do on
request. It is the most trusted process in userspace after `init`, and should be small.

### The prompt, and a terminal to prompt on

- **No program reads a terminal today** (above), so this is new mechanism, not wiring. Part A needs
  a way for a stage to *own the terminal* for a while. Either the tty server mints a second client
  channel onto the same terminal, or the shell steps aside — stops draining its channel — while a
  stage holds it. Both the password prompt and an interactive program under `with` need it, and
  the choice is Part A's detail pass. It also retires the reason `nxinstall` takes its
  confirmation as an operand.
- **The prompt itself**: echo off, as the login prompt. **A failed attempt is slow and counted** — a
  delay after each failure, a cap per request, and every failure in the log. Any program in a session
  can call `/dev/views` in a loop, and this is what stops it becoming a password-guessing service.
- **Graphical — designed here, built when needed.** A UAC-style window asking for permission. What
  makes it more than a dialog is that **only the broker can open it**, and the compositor draws it in
  a way no application can imitate — the desktop dimmed behind it, as Windows does. **Trigger:** the
  first desktop surface needing an action the policy will not allow without a password. *Shut down*
  is not one: the `power` rule lets the person at the machine power off, as every desktop does. The
  first real candidates are unmounting a USB stick from Files (Phase 6) and a Settings application.

### The spawn

- **Streams**: `with` passes its stdin, stdout and stderr handles, and the program writes straight
  into the caller's pipeline. `with admin disk --list | filter …` composes like any other stage.
- **The terminal**: an interactive program — `with admin nxsh`, an elevated editor — gets the
  terminal through the same handoff the prompt uses.
- **Stopping it**: the program's parent is the broker, not the shell, and the shell interrupts a
  pipeline through the processes *it* spawned. So **the broker hands back a handle the shell can stop
  it through**, and Ctrl-C reaches an elevated program as it reaches any other.
- **Its lifetime**: when the session that asked ends, the broker ends what it started for it.
- **What the caller never gets is the authority.** The program runs in a namespace the caller holds
  no handle to. The caller receives its output and its exit status.

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

**Coldplug, built now.** At startup, every device present is announced as an arrival — how Linux's
`udev` treats devices present at boot. Every consumer is written against "a device arrived" from the
start, so Phase 6 adds an event source without rewriting any consumer. Today `input-server` and
`init` both assume a device set fixed at boot.

**It is the component Phase 6's "userspace driver manager" extends**, not a sibling of it. 5.1 had
one component for both jobs: learning what arrived, and matching it to a driver module. Phase 6 adds
the matching and the USB event source to the component this phase builds.

**It is the one component that sees all of `/dev`**, and **it serves read-only device information to
anyone**: `disk --list` needs no view, and only the raw device does. It is also the one component
whose bugs reach every device, so it should do as little as possible.

**Kernel work:** a way to enumerate the device registry — which is also what finally lists
`/dev/blk`'s children — and, with its first event source in Phase 6, a notification when a device
node is published or withdrawn.

## Storage

**Raw devices and filesystems are different things.** `/dev/blk/<n>` is bytes, for tools under a
view such as the installer. What a person browses is a *filesystem*, which appears once mounted:

- **`/storage/<label>`**, served by the **storage service** — the class owner the device manager
  hands disks to — and bound into every session and application namespace.
- **"Nothing re-bound" needs kernel work.** A single `/storage` binding that gains children as
  filesystems mount means a resolve under it has to continue in the mounted filesystem's server.
  **Decided: build `OBJECT_KIND_SUBNAMESPACE`** — the reply a server gives to say "continue in this
  namespace" — which is also what makes a lazily filled file under `/storage` fill through the
  right registration. The alternatives were worse: proxying the whole
  file protocol, fills included, through the storage service; or re-binding into every session and
  application namespace on each mount, which means holding them all.
- **Names are labels, not numbers.** `/dev/blk/<n>` is discovery order, so a stick that is `2` today
  is `3` tomorrow. `/storage/<label>` is the stable name anything persistent should use, as
  `init.toml` already identifies devices by label or UUID. A clash of labels gets a suffix.
- **Every account sees every mounted filesystem**, and that is intended for now: a laptop with one
  person at it. What narrows it later is visibility per session — the per-session endpoint again —
  and a sandbox view simply omits `/storage`.
- **Auto-mount**: a disk arrives, the storage service reads its filesystem header, and a filesystem
  it can serve that is not already mounted appears under `/storage`. **A live boot auto-mounts
  read-only**, since the machine it is booted on is the one being installed. `init`'s boot mounts
  stay `init`'s — reported, never auto-mounted or unmounted.
- **Mount** spawns the server for the filesystem it found — `fs-server-ext4` today, and
  `fs-server-fat` when Phase 6 builds it. The service is not ext4-shaped.
- **Unmount is a chain with three links, and the first is in the kernel.**
  1. Every `FileObject` resolved through that registration writes back its dirty pages — **new
     kernel work**. `sys_file_sync` is the only trigger today, and a dropped `FileObject` frees its
     frames unwritten.
  2. The filesystem syncs its metadata — a new `fs-server` op.
  3. The drive's cache is flushed (`TODO(ahci-flush)`, triggered now).

  Only then is the server torn down, and the superblock marked clean.
- **A filesystem knows how it was left.** Mounting marks the ext4 superblock *in use* and unmounting
  marks it *clean*. `disk --list` reports a filesystem that was not unmounted cleanly, which until
  this phase ships `shutdown` is every one. A repair tool (`fsck`) stays deferred; the state it would
  need does not.
- **Formatting and partitioning** are Phase 6's, when a USB stick is the first thing outside the
  installer that wants them. `nxinstall` already does both through `libgpt` and `fs-server-ext4`'s
  library, so they will be thin.

## Accounts

- **`auth-service` gains admin ops** — list, add, remove, set a password — with an atomic rewrite of
  `/system/users` and a reload. The credential store stays one service's, and the file has one
  writer. `auth-service` stays a credential oracle: **the tool makes the home**, through the
  `accounts` grant's `/home` binding.
- **Your own password needs no view**: the proof is the current password.
- **Removing an account** keeps its home unless asked, is refused if the broker says it would leave
  no administrator, and ends its live sessions — which the broker can see.
- **Recovery is the live image.** Boot it, mount the installed disk, and reset a password in **that
  disk's** `/system/users`, not the running system's. So `account` needs a small **offline mode**
  that operates on a users file named by path. It gets its own gate, because it is the one path
  nobody exercises until they need it.

## Services

`service-mgr` gains an admin endpoint: **list** — each service's name, state and restart count, for
anyone — and **start, stop and restart** under the `services` grant.

**Moving the declarations onto the root filesystem — decided, for Part E.** `services.toml` has no
reason to be in the initramfs (above), and it cannot be edited there: the initramfs is a boot archive
on the FAT EFI partition, which nothing here writes. On root, `service-mgr` reads it after `init` has
mounted the filesystem it lives on, and **enabling and disabling become a small edit** — still
deferred, since no service wants disabling yet. `profiles/system.toml` probably follows it. The live
image then gets it through `root.img` like everything else on root, with no special case.
**`check-images` changes shape**: test and release images will then differ in a root-filesystem file
rather than an initramfs one, which its allow-list has to learn.

## Power

- **`shutdown`**: `service-mgr` stops the services, the storage service unmounts everything it
  mounted (the full chain above), `init`'s filesystems are written back, synced and marked clean, the
  drive is flushed, the kernel stops, and the framebuffer console — already built to draw after the
  desktop has gone — says *"It is now safe to turn off your computer."*
- **`shutdown --reboot`**: the same sequence, then a reset, in the conventional order: the FADT
  reset register when FADT advertises one (`RESET_REG_SUP`), then the i8042 reset pulse, then a
  triple fault, which resets any x86 CPU. **None of it needs AML.** Whether the laptop's FADT
  advertises a reset register can be read from the hardware report once FADT is parsed.
- **Kernel work — more than a syscall.** 5.1's system-control handle was never built, so gating power
  behind it means **a new kernel object, added to `init`'s boot grant**, then delegated to
  `service-mgr`, plus FADT parsing. **Power-off** (S5) stays deferred with ACPICA.

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
| `disk --mount <device> [<label>]` · `disk --unmount <label>` | mount at `/storage/<label>` · write back, sync, flush, unmount | `storage` |
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

## Kernel work in this phase

The review's main lesson is that this is not only a userspace phase. Collected in one place:

| Work | For | Part |
|---|---|---|
| Deriving a namespace from an existing one | views without holding a session's ingredients | A |
| Enumerating the device registry | coldplug; listing `/dev/blk` | B |
| `OBJECT_KIND_SUBNAMESPACE` — a resolve continuing in another namespace | `/storage` with nothing re-bound | C |
| Writing back every `FileObject` under a registration | unmount and shutdown without losing mapped writes | C |
| `FLUSH CACHE` in the AHCI driver (`TODO(ahci-flush)`) | the last link of every unmount | C |
| A system-control object in `init`'s boot grant, and a power operation | `shutdown` | E |
| FADT parsing, and a reset (FADT → i8042 → triple fault) | `shutdown --reboot` | E |
| `SYSTEM_CLOCK` wired, and the RTC written back | `date --set` | E |

## Parts — sketched

- [ ] **A — the view broker, on a terminal.** The broker service; `/dev/views`, minted per session and
      bound by **both** login supervisors, which also tell the broker when a session ends; namespace
      derivation in the kernel; `with`, `--list` and `--check`; `views.toml`, its schema
      spec, and the last-administrator guard; **a terminal a stage can own** — the handoff the prompt
      and interactive programs both need; the prompt, with echo off and delayed, counted failures;
      **one grant end to end, `disks`**; streams, and a stop handle for the shell; programs ended with
      their session; an audit record per request. The build images carry a seeded `views.toml` that
      makes the demo account an administrator.
- [ ] **B — the device manager, with coldplug.** Enumeration of the device registry; arrivals
      announced for everything present at boot; `input-server` taking its devices from it; read-only
      device information for anyone; `/dev/blk`'s children listable at last.
- [ ] **C — storage.** Write-back of a registration's `FileObject`s; a whole-filesystem sync and
      clean/dirty state in `fs-server-ext4`; `TODO(ahci-flush)`; `OBJECT_KIND_SUBNAMESPACE`; the
      storage service — mount, unmount, auto-mount (read-only on a live boot), `/storage` bound into
      sessions and application namespaces, refusing a raw grant of a mounted device; `disk`.
- [ ] **D — accounts.** `auth-service`'s admin ops and atomic rewrite; `account`, including the
      offline mode; removal refused when the broker says it would leave no administrator; ending a
      removed account's sessions.
- [ ] **E — services, power, the clock and the log.** `services.toml` moved onto the root
      filesystem; `service-mgr`'s admin endpoint and `service`; the system-control object, FADT, the
      power operation, and `shutdown`; `SYSTEM_CLOCK` and `date --set`; the log's read op and `log`.
- [ ] **F — the desktop's share.** `desktop-shell` building application namespaces in the same
      vocabulary; the graphical prompt's design written down, with its trigger.
- [ ] **G — the installer, the broker's first client** (decided 2026-09-17). **The installer's boot
      entry stays** — it is still the one that loads the 33 MiB installable ESP, which is H.1's
      reasoning and still sound — **but its session stops being special**. It becomes an ordinary
      session in which the person types `with admin nxinstall`, with the binary unchanged (decided
      2026-09-22, over moving the ESP module onto the default entry). The installer
      creates the first account and the `views.toml` that makes it an administrator, instead of
      inheriting the build's demo account.

**A comes first**, since everything else is a grant it hands out. **B before C**, since storage is
the device manager's first consumer. D and E are independent, **F can go anywhere after A**, and **G
closes the phase**.

## Gates

| Part | What proves it |
|---|---|
| A | `test-interactive`: a request allowed, one denied by policy, wrong passwords delayed and capped, an audit record for each, and Ctrl-C stopping a program started with `with`. **And that the grant arrived**: under `with admin` a program sees `/dev/blk/0`, and the same command without it does not |
| B | every boot device announced as an arrival, in `test-qemu` and `check-live`, and keyboard and mouse still reaching a window |
| C | **`check-install`'s topology**: a live boot, whose root is a RAM disk, with a SATA disk attached — the second disk QEMU *can* supply. Auto-mounted (read-only, being a live boot), remounted writable, written through a mapping *without* a sync, unmounted — then `e2fsck` and the file's **contents** checked on the host. A RAM disk cannot be checked there: the guest's writes never reach a host file |
| D | `account --add`, `--password` and `--remove` at a real prompt; and **a recovery gate**, on demand like `check-install`: boot the live image, reset a password on the installed disk offline, boot that disk, and log in with the new one |
| E | **a shutdown gate**: write through a mapping without syncing, run `shutdown`, read the message off the screen with `check-fbcon`'s reader, then check on the host — `e2fsck` clean, the superblock marked clean, **and the file's contents present**. `shutdown --reboot` seen as a second boot |
| G | `check-install` driving `with admin nxinstall` from an ordinary session, **onto a disk that already holds a Nitrox install** — a reinstall, not a blank disk, so the auto-mount rule is exercised |

## Deferred from this phase

To Phase 6 (and noted in its plan): hot-plug event sources, module matching, FAT, and formatting and
partitioning. Also deferred: networking; software installs; multiple disks and controllers;
power-off through ACPI S5; the graphical prompt's build, until its trigger; enabling and disabling
services persistently (a small edit once `services.toml` is on root); a filesystem repair tool; the
chained audit subsystem; policy groups, argument constraints, `deny` rules and prompt caching; the
rest of namespace layering; and per-session visibility under `/storage`.
