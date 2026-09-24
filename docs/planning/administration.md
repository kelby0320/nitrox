# Administration: views, devices, and the tools an installed system needs

**Status: in progress — Part A complete (2026-09-23), Part B detailed (2026-09-23); scoped
2026-09-22 and revised after the PR #326 review.** Scheduled after
[the desktop refresh](desktop-refresh.md), which is complete, and before Phase 6. The scope and the
architecture below were agreed with the maintainer on 2026-09-22. The review then found that
several mechanisms depend on things the code does not have, and **the maintainer took the four
resolutions that needed a decision the same day** (the last item under *Decisions*). **Part A has
had its detail pass** (*Part A in detail*, below) **and is built (2026-09-23)**. **Part B has had
its detail pass** (*Part B in detail*) and is next to build; the other parts are sketched. The plan began as a stub on 2026-09-16, written while building the
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
  policy · hotplug", holding every raw node). Today it only sees what exists at boot — **and needs
  both**: it opens a fixed keyboard and mouse and exits without either (*Part B in detail*).
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
builds the session opens a session with the broker for its principal, and binds the broker's
forwarding endpoint at `/dev/views` with a subtree base naming that session** — `session-mgr` for
the serial column and `desktop-session-mgr` for the graphical one. **Both must**, or a session
exists that the broker cannot see. A request arriving through that binding is from that session,
and nothing in the session can choose another base — the logging service already works this way,
with identity set by the supervisor. **One exception: the graphical column's leader.**
`desktop-shell` holds the raw forwarding endpoint and `BIND_NAMESPACE`, because it binds
`/dev/views` into the applications it launches, so it could bind any base. That adds nothing to
what it already holds: the whole-tree filesystem endpoint. It does mean the graphical session's
identity rests on `desktop-shell`, where a serial session's rests on the binding alone. (This said *minted per session* until Part A's detail pass
found the path already carries identity; see *Part A in detail*.) The password then confirms the person
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
run  = ["*"]         # programs, by bare name, resolved in the broker's /bin
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
  a way for a stage to *own the terminal* for a while. **Decided in its detail pass: every external
  stage gets a sibling terminal on the shell's backend**, so the shell keeps its own and still sees
  `Ctrl-C`. Both the password prompt and an interactive program under `with` need it. It also
  retires the reason `nxinstall` takes its confirmation as an operand.
- **The prompt itself**: echo off, as the login prompt. **A failed attempt is slow and counted** — a
  delay after each failure held for the whole session, a cap per request, and every failure in the
  log. Any program in a session
  can call `/dev/views` in a loop, and this is what stops it becoming a password-guessing service.
- **What the prompt does not stop: a program on the same backend reading the password.** Every
  stage holds a sibling terminal, and input goes to the *oldest* terminal on the backend with a
  read pending (`tty-server`'s routing). So a program that keeps a read pending — a stage's child
  that outlived its pipeline, or an earlier stage of the same pipeline — receives the line typed at
  `with`'s prompt, password included. Pacing does nothing here: this is not a guess. `sudo` has the
  same limit, since anything holding the tty can read it. **Accepted for Part A and recorded**
  (PR #329 review); the remedy is a prompt nothing in the session can read, which is what the
  graphical design below is.
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
drive devices. A keyboard or mouse goes to `input-server`, which keeps one merged stream while
devices come and go — **once Part B gives it a changing set of devices**: today it opens a fixed
keyboard and mouse and exits without both (*Part B in detail*; this sentence said it already
owned hotplug until that pass read the code). A disk goes to the storage service. Each class's
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

**Kernel work:** a way to enumerate the device registry — `/dev/registry`, which Part B's detail
pass chose over making `/dev/blk`'s children listable — and, with its first event source in
Phase 6, a notification when a device node is published or withdrawn.

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
| `/dev/registry` — a snapshot of the device table, and each node by id | coldplug; `/dev/devices` | B |
| `OBJECT_KIND_SUBNAMESPACE` — a resolve continuing in another namespace | `/storage` with nothing re-bound | C |
| Writing back every `FileObject` under a registration | unmount and shutdown without losing mapped writes | C |
| `FLUSH CACHE` in the AHCI driver (`TODO(ahci-flush)`) | the last link of every unmount | C |
| A system-control object in `init`'s boot grant, and a power operation | `shutdown` | E |
| FADT parsing, and a reset (FADT → i8042 → triple fault) | `shutdown --reboot` | E |
| `SYSTEM_CLOCK` wired, and the RTC written back | `date --set` | E |

## Parts — sketched

- [x] **A — the view broker, on a terminal** — *detailed below, A.1–A.6; complete 2026-09-23.* The broker service;
      `/dev/views`, one forwarding endpoint bound into each session with its base by **both** login
      supervisors, which also tell the broker when a session ends, and by `desktop-shell` into the
      applications it launches; namespace derivation in the kernel; `with`, `--list` and `--check`; `views.toml`, its schema
      spec, and the last-administrator guard; **a terminal a stage can own** — the handoff the prompt
      and interactive programs both need; the prompt, with echo off and delayed, counted failures;
      **one grant end to end, `disks`**; streams, and a stop handle for the shell; programs ended with
      their session; an audit record per request. The build images carry a seeded `views.toml` that
      makes the demo account an administrator.
- [ ] **B — the device manager, with coldplug** — *detailed below, B.1–B.5.* `/dev/registry`;
      `device-mgr`, with subscriptions by class that replay every present device (coldplug);
      `input-server` taking a changing set of devices from it; `/dev/devices`, typed tables anyone
      can read, in place of listing `/dev/blk`.
- [ ] **C — storage.** Write-back of a registration's `FileObject`s; a whole-filesystem sync and
      clean/dirty state in `fs-server-ext4`; `TODO(ahci-flush)`; `OBJECT_KIND_SUBNAMESPACE`; the
      storage service — mount, unmount, auto-mount (read-only on a live boot), `/storage` bound into
      sessions and application namespaces, refusing a raw grant of a mounted device; `disk`.
- [ ] **D — accounts.** `auth-service`'s admin ops and atomic rewrite; `account`, including the
      offline mode; removal refused when the broker says it would leave no administrator; ending a
      removed account's sessions; **`with --edit` and the `views` grant** (moved here from A by its
      detail pass — who administers the system is what this part lets a person change).
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

## Part A in detail *(2026-09-23)*

### The spike: what already exists, and what is missing

- **Copying a namespace is small kernel work.** A binding's target is a cloneable `ObjectRef`, a
  `Copy` kernel-server id, or a registration's `ObjectRef` plus its `SubtreeBase`
  (`kernel/src/object/namespace.rs`), so a copy is a loop under the lock. Creating a namespace is
  already unprivileged (`sys_ns_create`); only *binding* needs `BIND_NAMESPACE`.
- **A process cannot send its own namespace.** The handle a child gets at spawn is `LOOKUP`-only,
  with no `TRANSFER`. It can *copy* it, though, since a new namespace comes with full rights.
- **So the broker must copy again.** A derived handle carries `DUPLICATE`, so a caller that sent
  the broker a namespace could keep a duplicate and resolve through it. It could also spawn a
  child into it. Either way, whatever the broker bound there would reach the caller. **The broker
  binds only into a namespace it created**, derived from the one it was sent.
- **The path is already an identity.** The logging service knows a record's principal from the
  path its channel was resolved through (`/log/system/<name>`), and `desktop-shell` confines an
  application to `/dev/draw/new` with a narrow bind. So `/dev/views` needs no endpoint minted per
  session. One forwarding endpoint, bound into each session with a **subtree base naming that
  session**, reaches the broker with a suffix no program in the session can choose — the
  graphical leader excepted, above.
- **`init` binds what services cannot.** A service `service-mgr` starts inherits a `LOOKUP`-only
  root, which is why `init` spawns `auth-service` and binds it at `/svc/auth`. The broker has the
  same need and gets the same treatment, with the same boundary: anything holding the root
  namespace can reach it, which is `TODO(svc-auth-ungated)`'s reasoning and its fix.
- **The endpoint chain exists.** `init` → `service-mgr` → the two login supervisors →
  `desktop-shell` already carries four or five endpoints positionally; the clipboard's was the last added
  (M12 Part E).
- **The tty server already routes by backend.** Input goes to "the first terminal waiting on this
  backend", `Ctrl-C` goes to every terminal on it, and a terminal is retired when its holder exits.
  `routing::move_to` exists for exactly the shared case, and nothing outside its tests calls it. So a terminal a
  stage can own is **one new operation**: another terminal on the shell's backend.
- **The setup message already carries a terminal.** `SetupPayload::terminal` is an appended
  field, and its handle follows the stream handles (`send_setup_full`). It was built in Milestone 5
  Part C so `nxterm` could hand `nxsh` its window's terminal. It is **not** a `streams` bit: a
  fourth bit would not decode, because `Streams::from_bitmap` refuses anything outside the three
  streams. What is missing is `nxsh` *sending* one to its stages, and coreutils' `Stage` exposing
  it — `Stage` has no terminal today. (`pipeline-stdio.md` never gained the field, which is how the
  first draft of this pass proposed it again as a bit. It is fixed in the same change.)
- **The shell already stops stages by request.** `nxsh` calls `sys_process_terminate` on each
  child. `with` is a child like any other, so it can pass the request on to the broker.
- **There is no forcible kill.** `sys_process_terminate` is a request. The kernel reserves the
  `TERMINATE` right for a forcible kill, should one be built.
- **The gate's probes exist.** `nxinstall` with no operand lists the block devices it can see or
  says there are none, and `sleep` honours a stop request through coreutils' `Stage`.
- **Missing and small:** a reader for `views.toml` (the house style is a focused reader per file,
  as `init`'s `toml_lite` and `service-mgr`'s `service_toml` are), and an rsproto category —
  `0x0Exx` is free.

### The shape

**The maintainer's calls:**

- **Every external stage gets a terminal of its own**: a sibling of the shell's, on the shell's
  backend, passed in the setup message. Any program can prompt — `with`, a confirmation in
  `nxinstall`, an elevated `nxsh`. The shell keeps its own terminal and still sees `Ctrl-C`. The
  cost is a tty round trip per stage and a tty-server channel while it runs, against the fan-out
  limit already recorded as `TODO(server-fanout)`.
- **`with` works in desktop terminals in Part A**, not only on the serial console. The broker's
  endpoint travels on to `desktop-shell`, which binds `/dev/views` into the applications it
  launches with the session's base. Without it, `with` would do nothing on the laptop's desktop
  until Part F.
- **A session's end is asked for, not forced.** The broker asks each program it started for that
  session to exit, and unbinds the grants from each view, so nothing new can be resolved through
  them. A program that ignores the request keeps what it already holds. A forcible kill is
  deferred (`TODO(forcible-kill)`), triggered by the first program that must not outlive its
  session.
- **`with` reads the password**, on its terminal with echo off, and sends it in the request. The
  broker never touches a terminal, which keeps it small. The broker paces and caps failures, as
  set out below. Prompting from the broker would prove no more: it could not tell a real terminal
  from a channel that pretends to be one.

**Derived from the spike and the plan:**

- **`sys_ns_derive(ns)`, syscall 37.** It requires `LOOKUP` on `ns` and returns a new namespace
  with the same bindings and full namespace rights. It is a **snapshot**: later binds in either
  namespace do not reach the other, so a long-running elevated shell does not see a session's
  later changes.
- **One forwarding endpoint, two kinds of channel.** `init` binds the broker at `/svc/views`. A
  supervisor resolves `/svc/views/session` for a **supervisor channel** (`OpenSession` and
  `CloseSession`), and binds the forwarding endpoint at `/dev/views` in each session with base
  `/s/<session>`. A process in the session resolves `/dev/views` and gets a **client channel**,
  which the broker tags with that session. **Session ids increase and are never reused within a
  boot.** A program that ignores its session's end keeps a namespace with `/s/<old>` in it, and a
  reused id would hand that program's requests the next login's identity.
- **A program is a bare name, resolved at `/bin/<name>` in the broker's own namespace**, not in the
  view. It reaches the same profile server as a session's `/bin`. No paths are accepted: a rule's
  `run` names programs, and a path would let "the same name" mean something else. **Not the
  view, because a caller can prune its copy.** `sys_ns_unbind` needs only the `UNBIND` right, which
  a derived namespace carries. So a caller can remove `/bin` from the copy it sends, and a name
  would then resolve through any shorter binding that covers it. Today's namespaces have none, but
  the image must not depend on that. Pruning elsewhere can widen what the program sees the same way,
  to whatever part of a broader binding a narrower one covered — which is why no namespace may
  rely on covering to hide anything
  ([`namespace-and-resource-servers.md`](../architecture/namespace-and-resource-servers.md)). The
  program's own later lookups happen in the view, as they would without `with`.
- **The delay after a failure is per session; the cap is per request.** A failure holds that
  session's *next* password check until the delay has passed, on whichever request it arrives.
  Otherwise a program could open several requests at once and guess on each in parallel. A request
  that fails three times is refused, as a login would be. **The delay is a deadline in the broker's
  wait set, never a sleep.** A single-threaded broker that slept would stall every other session's
  requests, and the supervisors' `OpenSession` with them, so logins would wait on someone else's
  typing. (This refines the call as first put, "paces and caps failures per session". A cap
  per session would let one program's wrong guesses lock its person out of `with` for the rest of
  the session; the per-session *delay* is what stops parallel guessing — PR #328 review.)
- **The policy is read for every request.** It is small; a stale copy is a bug; and a policy that
  fails to parse then denies everything straight away, as the plan says it must.
- **Part A knows one grant, `disks`, and a policy naming any other is refused.** Each later part
  adds its grant to the vocabulary and to the seeded profile. A grant the broker does not know is a
  mistake to report, not something to ignore.
- **`with --edit` and the `views` grant move to Part D**, which is where who administers the system
  becomes something a person changes.
- **`TODO(admin-visibility)` is answered by this part.** It asked whether an administrator is a
  second account or a mode, and how a person moves between them. The answer is a mode — a view —
  reached with `with`. **Its code marker is on something that stands on its own**: application
  namespaces omit `/applications` because nothing in an application reads it. So when A lands,
  three things change:
  - the entry moves to Resolved;
  - `desktop-shell`'s comment keeps its reason and drops the tag;
  - `graphical-session.md` §6.1 stops calling the asymmetry a symptom of the deferral.

  `check-deferrals` would not notice a stale marker, because it only asks whether the tag appears
  in the doc, so this has to be done by hand.

### A request, end to end

1. `with admin nxinstall`: `with` derives a copy of its own namespace and resolves `/dev/views`.
2. It sends `Request { view, program, argv, env }` with the copy, its three streams and its
   terminal.
3. The broker reads the policy and evaluates it for the session's principal. A denial is answered
   and logged, and nothing is prompted for.
4. If the rule says `password`, the broker answers `NeedPassword`. `with` prompts, and sends the
   password. The broker checks it against `auth-service`, but not before the session's delay from
   its last failure has passed. After a third failure in a request it refuses that request. Every
   failure is logged.
5. The broker derives the view from the copy and binds the profile's grants into it (`disks` is
   `rebind_block_devices` from its own namespace). It resolves `/bin/<program>` in its own
   namespace and spawns it into the view, with no syscaps beyond the profile's. The setup message carries the argv, the environment with
   `view = "admin"` added, the streams and the terminal. It answers `Started`, and logs it.
6. The program's output goes straight into the caller's pipeline. When it exits, the broker sends
   `Exited { code }` and closes the view, and `with` exits with that code.
7. When the shell asks `with` to stop, `with` sends `Stop`, and the broker passes the request on to
   the program.
8. `CloseSession`: the broker asks every program it started for that session to exit, unbinds their
   grants, and refuses anything further that arrives under that session's base. Since ids are
   never reused, that base can never be anyone else's.

### The pieces, in dependency order

- [x] **A.1 — `sys_ns_derive`** *(2026-09-23)*. `Namespace::try_derive`, the syscall, `libkern`'s constant, the ABI
      spec and `abi-sync-check`. Host tests: the copy resolves what the source resolves; a bind or
      an unbind in either one leaves the other alone; a subtree base survives the copy; and
      dropping one namespace leaves the other's registrations alive. **Plus a `boot-probe` check
      through the syscall itself**, which the host tests cannot reach: the copy resolves, can be
      sent and pruned, pruning it leaves the root alone, and a handle without `LOOKUP` is refused.
- [x] **A.2 — a terminal per stage** *(2026-09-23)*. A tty op minting a sibling terminal on the caller's backend,
      and its spec. Echo is per terminal, so a password prompt does not turn the shell's echo off.
      `nxsh` mints one for each external stage and moves it in through the **existing** `terminal`
      field (`send_setup_full`), and coreutils' `Stage` exposes what it receives. Routing tests: input reaches whichever
      sibling is waiting; `Ctrl-C` reaches both; and retiring one leaves the other on its backend.
- [x] **A.3 — the broker** *(2026-09-23)*. `view-broker`, a lib and bin split like `auth-service`, and host-tested:
      - the `views.toml` reader, rule evaluation, and the last-administrator guard, each tested at
        its neighbours;
      - the `Views` protocol (`0x0Exx`), with `rsproto-views-ops.md` and `views-toml-schema.md`;
      - `init` spawning it with `BIND_NAMESPACE` and binding `/svc/views`;
      - per-session pacing, with a host test that two requests in one session share the delay a
        failure on either one starts, while another session's do not wait;
      - the audit records, and the spawn;
      - `Exited`, `Stop`, and the end of a session;
      - **a `boot-probe` check through the broker's own protocol**, in `test-qemu`, before any shell
        or `with` drives it — which needed A.6's seeded policy early, so the seed landed here.
- [x] **A.4 — the supervisors** *(2026-09-23)*. The forwarding endpoint couriered from `init` to both login
      supervisors and on to `desktop-shell`. A supervisor opens a session at login, binds
      `/dev/views` with its base, and closes the session when its leader exits. `desktop-shell`
      binds the same thing into every application namespace.
- [x] **A.5 — `with`** *(2026-09-23)*. A coreutil: `with <view> <program> [args]`, `--list` (a typed table) and
      `--check <file>`. The last sends the file's text for the broker to judge, so there is one
      parser. It prompts with echo off, relays the exit status, and passes on a stop request. The
      shell spec records the name as taken by `/bin`.
- [x] **A.6 — the seed and the gates** *(2026-09-23)*. The build seeds `/system/views.toml` *(landed with A.3,
      which needed it to test the broker in a boot)*:
      - an `admin` profile holding `disks`, which the demo account may use for any program with a
        password;
      - a narrower profile allowing one program, so a gate can see a request refused by policy.

      What is left is the gates: `test-interactive`'s `with` steps and `check-login`'s.
- [x] **Docs** *(2026-09-23)*:
      - `session-and-auth.md` gains the broker, and its deferred "privilege broker" line is closed;
      - `namespace-and-resource-servers.md` gains derivation;
      - `console-and-tty.md` gains sibling terminals, and `pipeline-stdio.md` says who sends a
        terminal (its field was specified by this pass);
      - `deferred-decisions.md`: `TODO(admin-visibility)` is resolved, with its `desktop-shell`
        comment and `graphical-session.md` §6.1 updated as above, and `TODO(forcible-kill)` and
        the broker's share of `TODO(svc-auth-ungated)` are recorded.

### What to compare on the day

**`test-interactive`**, on the serial console:

- `nxinstall` says there are no block devices, and `with admin nxinstall` lists `/dev/blk/0`.
- A wrong password is refused, and the pause before the answer is measured host-side. The right
  password succeeds. Three wrong ones end the request.
- A program outside a rule's `run` is refused without a prompt.
- `with --list` shows `admin`.
- `with admin sleep 60`, then `Ctrl-C`, is back at the prompt well inside a minute.
- There is an audit record for each request, and a password appears in none of them.

**`check-login`** runs one `with` request from the terminal it opens through the Applications
menu. That is the release image, after a real login, with `nxterm` in the namespace `desktop-shell`
built for it, so the endpoint chain to `desktop-shell` and the session's base are both exercised.
It asserts on the broker's `Started` and audit lines, since a release image does not narrate the
grid. `check-terminal` cannot carry this: its `nxterm` is a boot-probe service in the root
namespace, with no login and no session.

### Left alone

- **Every grant but `disks`**, each with its part. `--edit` and `views` are Part D's.
- **The graphical prompt**, until its trigger.
- **A forcible kill.**
- **Showing the view in `nxsh`'s prompt.** The environment carries it from A.3 on. A `#`-style
  marker is a small follow-up once someone wants it.
- **Groups, argument constraints, `deny` rules, and prompt caching.**

## Part B in detail *(2026-09-23)*

### The spike: what already exists, and what is missing

- **The kernel's device table is half the devices.** `kernel/src/device.rs` holds the PCI
  functions `device::init` enumerated and the block devices drivers registered after them —
  disks, partitions and the RAM disk — in discovery order, append-only. **The keyboard, the mouse
  and the console are not in it**: the i8042 driver keeps its two nodes in a table of its own,
  and the console's node is the console driver's. Nothing lists the table outside the kernel;
  the hardware report and `drivers:` lines read it from inside.
- **Every enumeration today is a probe.** `eshell`'s `lsblk`, `nxinstall`'s scan and
  `libsession::rebind_block_devices` each look up `/dev/blk/0`, `1`, … until the first miss,
  relying on the table being dense. **The last has three callers and two kinds of source**:
  `build_namespace` (for an installer boot's session) and the view broker rebind from the root
  namespace, and `desktop-shell` rebinds from an **installer session's** namespace into each
  application it launches — the laptop's only install path, since it has no serial port. A kernel server answers lookups only, so `list /dev/blk` in
  the root namespace shows an empty directory — `libfs::ns_children` says so and says why.
  **Inside a view it already lists**, because the broker binds each device and its `info` one by
  one; only the root namespace is blind.
- **`input-server` has no hotplug.** This plan said it "already owns input hotplug and keeps one
  merged stream while devices come and go". It does not: it opens `/dev/input/raw/0` and `/1` at
  fixed paths into a fixed pair, and **exits if either is missing** — so a machine whose i8042
  has no aux port would lose its keyboard too. The merge itself is per-device and would take more
  devices; the device set is what is fixed. (The laptop does have an aux port — its firmware
  exposes the trackpad there, `ps2: keyboard mouse armed` — so this is latent, not live.)
- **Phase 6 says how devices should move.** Its driver manager is this component extended, and it
  hands a driver process a `Handle<DeviceNode>`. So a class owner should receive its devices **as
  handles**, not as paths to go and look up.
- **A userspace server can already serve a read-only file and a directory.** A resolve answered
  `OBJECT_KIND_MEMOBJ` hands over a memory object — the kind of object `/dev/log` and
  `/session/user` already are — which `libfs::read_file` looks up, maps and reads by its size; and
  a directory is a channel answering `File::ReadDir`, as `profile-server` does for `/bin`.
- **The shell already opens a typed file.** `open` decodes a path ending `.tsm` as a TSM1 stream
  into a `Table` (`nxsh`'s `decode_from`), and `Table::decode` stops at the terminator, so the
  zero padding of a page-sized memory object is harmless.
- **The endpoint chain Part A extended reaches every namespace a person uses**: `init` →
  `service-mgr` → both login supervisors → `desktop-shell`, positionally.
- **Missing:** a way to read the table from userspace, the device manager itself, an rsproto
  category (`0x0Fxx` is free), and `input-server` taking a changing set of devices.

### The shape

**The maintainer's calls:**

- **The table is read through `/dev/registry`**, a kernel server bound in the root namespace only.
  The bare path is a read-only snapshot: **a header carrying the record count**, then one
  fixed-size `DeviceRecord` per node — its id, its `DeviceClass`, **its kind** (disk, partition,
  RAM disk, keyboard, mouse, console, PCI function), **the index its path serves it at**, parent,
  size, PCI identity, what its driver did with it, and a name. The count is what a reader
  trusts, not the object's size: a memory object is page-rounded, and zero padding read as
  records would be phantom devices of class `Other`. `/dev/registry/<id>` is that node's handle. It is `/dev/log`'s shape: no syscall, and the
  binding is the authority. **The keyboard, the mouse and the console join the table**, so the
  snapshot is every `DeviceNode` the kernel has.
- **A class owner subscribes by path.** The device manager is bound at `/svc/devices`, and a
  resolve of `/svc/devices/<class>` — `input`, `block` — *is* the subscription: the channel it
  returns replays every present device of that class as `Arrived`, each carrying its handle, then
  `Settled`. That replay is coldplug. Phase 6's arrivals and departures follow on the same channel,
  so no consumer changes when a real event source appears. `init` wires nothing, and the manager
  knows no consumers.
- **Anyone can read `/dev/devices`, as typed tables.** The manager serves a directory of `.tsm`
  files: `all.tsm`, every device a row, and one per device — `blk-0.tsm`, `input-1.tsm`,
  `console.tsm`, `pci-00.1f.2.tsm`. So `list /dev/devices` lists them and
  `open /dev/devices/all.tsm | filter kind == "disk"` is a typed query with no new shell code. It
  reaches sessions and applications through Part A's endpoint chain, bound with the base `/info`,
  so a session can reach the information and nothing else. Part C's `disk --list` reads it.
- **"`/dev/blk`'s children listable" is replaced by `/dev/devices`.** The root namespace is the
  only one where `/dev/blk` lists empty, and nothing there needs it once the probes read the
  registry. `libfs` keeps its documented limitation, now pointing at `/dev/devices`.

**Derived from the spike and the calls:**

- **`device-mgr`**, a lib and bin split like `view-broker`. **`init` spawns it before
  `input-server` and binds `/svc/devices`**, as it binds `/svc/auth` and `/svc/views`. It needs no
  syscap: it binds nothing. It reads the registry once at start, and holds each node's handle to
  hand out.
- **A device's name is its path's**: `blk-<n>` is `/dev/blk/<n>`, and `input-<n>` is
  `/dev/input/raw/<n>`, so a name in `/dev/devices` says which binding a view would need. **The
  `<n>` is the record's served index, set by the kernel from the same source its server resolves
  through** — not a count within `DeviceClass`, where the console and both i8042 nodes are all
  `Char` and the console registers first, so counting would call the keyboard `input-1`. **The
  manager's classes are its own**, derived from the kind: `input` is keyboards and mice, `block`
  is disks, partitions and RAM disks. The registry id is stable within a boot and is what
  `Departed` will name.
- **A class has one owner at a time.** A second subscription is refused while the first is held,
  and taken once it goes. That is the kernel's rule, not a policy: each raw input device has one
  ring and one parked reader, so a second reader would drain events meant for the first, and its
  own read would stall the owner's (`ps2`'s `submit_read` answers `WouldBlock` to a second).
  **The owner gets a duplicate of each handle**, so an owner that exits cannot take a device from
  the next one. `/svc/devices/input` is bound where `/dev/input/raw` is, in the root namespace
  only, which is `input-subsystem.md` §5's exclusivity kept at the manager as well.
- **`input-server` takes a changing set of devices.** It subscribes to `/svc/devices/input`, reads
  up to eight devices, and serves from `Settled` on — **including with none, or a keyboard alone**,
  where today it exits. `Departed` retires a device's slot; nothing sends one until Phase 6, so its
  handling is host-tested in the library rather than left unwritten.
- **No fallback to the raw paths.** If the manager does not start, `input-server` has no devices
  and says so. A second path that only runs when the first is broken is a path nobody tests, and
  the manager is small enough to be as reliable as `input-server` itself.
- **The probes read what their source can see.** `eshell`'s `lsblk` runs in the root namespace
  and reads the registry. `libsession::rebind_block_devices` reads the registry when its source
  has one, and otherwise **enumerates the source's own `/dev/blk/<n>` bindings**
  (`sys_ns_enumerate`) — which is `desktop-shell`'s case, rebinding from an installer session,
  where the registry is deliberately absent and each device is bound one by one. Unlike the probe,
  that survives a gap. `nxinstall` runs in a view (`with admin nxinstall`) or in an installer
  session's application namespace; in both, what it may write is exactly what is bound, so it
  lists its own namespace (`libfs::ns_children`) instead of probing.

### A device, end to end

1. At boot the drivers register their nodes; the i8042 driver and the console now register too.
2. `init` spawns `device-mgr`, which reads `/dev/registry`, takes each node's handle from
   `/dev/registry/<id>`, and answers `Meta::Ready`. `init` binds `/svc/devices`.
3. `init` spawns `input-server`, which resolves `/svc/devices/input`. The manager replays the
   keyboard and the mouse as `Arrived`, each with a duplicated handle, then `Settled`.
   `input-server` arms a read on each and answers `Meta::Ready`, and `init` binds
   `/dev/input/new` as today.
4. A person types `open /dev/devices/all.tsm | filter kind == "disk"`. The resolve reaches the
   manager as `info/all.tsm`, it mints the snapshot as a memory object, and the shell decodes and
   filters the table.

### The pieces, in dependency order

- [x] **B.1 — the registry** *(2026-09-24)*. The i8042 driver's two nodes and the console's join the device table.
      The snapshot's header and `DeviceRecord` in `libkern`, mirrored in the kernel, with layout
      asserts; the `/dev/registry` kernel server — the snapshot and `<id>`; bound in the root
      namespace only; the ABI spec and `abi-sync-check`. Host tests:
      - a record per node in table order, with kind, served index and parent right for a disk and
        its partition, **and for the console, the keyboard and the mouse**: the keyboard's served
        index is 0 and the mouse's 1, with the console registered before them;
      - **a padded snapshot reads exactly the header's count** — the reader handed a page with
        zeros after the last record, since a reader that divided the size would pass a round trip.

      **A `boot-probe` check through the binding**: the snapshot decodes; its block records are
      exactly what probing `/dev/blk` finds — each served index resolving to a device of the
      record's size, whose `info` gives the record's name; the keyboard and mouse sit at raw 0
      and 1; every `/dev/registry/<id>` is a device node; and **each record's id is its place**,
      which a phantom record read past the count cannot be. (That two paths give *the same node*
      is the host tests' to show: a handle carries no object identity a process can compare, so
      the probe compares what it can — size and name.)
- [x] **B.2 — `device-mgr`** *(2026-09-24)*. The `Devices` protocol (`0x0Fxx`) and
      [`rsproto-devices-ops.md`](../spec/rsproto-devices-ops.md): `Arrived`, `Settled`,
      `Departed`. The subscription and its replay; `info/` as a directory of `.tsm` files.
      Host-tested in its library: records to rows, names, replay order, and **the reader side of
      the table** — a padded buffer, as the kernel hands it over, decodes to the rows (a round
      trip would only test the encoder), and one owner per class. `init` spawns it and binds
      `/svc/devices`. **A `boot-probe` check**: `/svc/devices/block` replays the disks and settles,
      a second subscription to `block` is refused while the first is held, and is taken once it
      is closed; `info` lists `all.tsm` and a file per device, and `all.tsm` decodes to a row per
      registry record. (`input` is `input-server`'s from boot on, so a probe cannot subscribe to it
      without stalling the keyboard — which is the rule working.)
- [x] **B.3 — `input-server` from the manager** *(2026-09-24)*. Subscribe; a device table of up
      to eight; serve from `Settled`, with none or one; retire on `Departed`. Host tests on the
      library: arrivals in any order, a keyboard alone, a departure mid-stream — and the merge
      over any number of devices, forwarded as batches that end on group boundaries, since eight
      devices' reads are more than one message holds. `check-input` (and its `--no-ps2-irq`
      variant) is the regression gate, unchanged. **`boot-probe` now asserts `input` is held**:
      a subscription to it is refused, which is how a probe sees that the input server took its
      devices from the manager.
- [x] **B.4 — `/dev/devices` for anyone** *(2026-09-24)*. The manager's endpoint couriered
      along Part A's chain; both login supervisors bind `/dev/devices` with the base `/info`, and
      `desktop-shell` binds it into application namespaces. **What travels is an info-only
      endpoint, not the one bound at `/svc/devices`** — found while building it: the base keeps an
      ordinary session's suffixes under `info`, but `desktop-shell` holds what is couriered and
      `BIND_NAMESPACE`, so it could bind it with no base and subscribe to `block`, every disk. The
      manager mints an endpoint on which it answers only its tables, `init` resolves one at
      `/svc/devices/info-endpoint`, and that is what the chain carries. `test-interactive` lists
      and filters the tables from a serial login and finds `/dev/devices/block` and
      `/dev/registry` open nothing; `check-login` asserts the session and each application
      namespace reach `/dev/devices`; `boot-probe` sends `block` down an info-only endpoint.
- [ ] **B.5 — the probes.** `eshell`'s `lsblk` reads the registry;
      `libsession::rebind_block_devices` reads the registry when its source has one and the
      source's own `/dev/blk` bindings when it does not; `nxinstall` lists its own namespace.
- [ ] **Docs**: a new architecture doc for the device manager; `input-subsystem.md` (devices from
      the manager, and the hotplug premise corrected); `namespace-and-resource-servers.md` and the
      kernel-server list (`/dev/registry`); `libfs`'s limitation note; `device-node.md`, whose
      *Deferred* list still names a device-enumeration syscall and a `/dev` listing (B.1 delivers
      the first as a path). **`kernel_server.rs` says the `/dev` listing "is deferred", and it is
      resolved** — `deferred-decisions.md`'s Resolved table has it (Phase 4 D3), with the
      `/dev/blk` limitation noted as carried by Part B — so the comment becomes a pointer to
      `/dev/devices`, which is how this pass answers that limitation.

### What to compare on the day

- **`test-qemu`**: `boot-probe`'s registry and subscription checks, above.
- **`test-interactive`**, in a serial session: `list /dev/devices` names the disks and the input
  devices; `open /dev/devices/all.tsm | filter kind == "disk"` prints a disk row, matched on a
  model the command does not contain; and `/dev/registry` does not resolve in a session.
- **`check-input`** and **`check-input --no-ps2-irq`**, unchanged: every key and click in them now
  arrives through the manager.
- **`check-live`**: the live boot's module disk is the one RAM disk any gate has, and
  `device-mgr` reports it with that kind.
- **`check-login`**: `desktop-session-mgr` says its session has `/dev/devices`, as it does for
  `/dev/views`.
- **`check-install`**, on demand and not in CI, **because B.5 changes the path it drives**:
  `desktop-shell` passing an installer session's disks on to the terminal a person opens, and
  `nxinstall` finding them by listing its own namespace. It is the laptop's only install path,
  and nothing else boots it.

### Left alone

- **A real event source**, and with it `Departed` ever being sent — Phase 6.
- **The storage service**, the block class's owner — Part C. Until then nothing subscribes to
  `block` but the probe.
- **Narrowing `/dev/input/raw` and `/dev/registry` to the manager's own namespace.** Every
  service shares `init`'s root today; giving the manager a namespace of its own is a separate
  change to how services are spawned.
- **The framebuffer**, which is not a `DeviceNode`, and whose owner `init` hands it to directly.

## Gates

| Part | What proves it |
|---|---|
| A | `test-interactive`: a request allowed, one denied by policy, wrong passwords delayed and capped, an audit record for each, and Ctrl-C stopping a program started with `with`. **And that the grant arrived**: under `with admin` a program sees `/dev/blk/0`, and the same command without it does not. `check-login`: one `with` request from the terminal the Applications menu opens |
| B | `/dev/registry` and the subscriptions, in `test-qemu`; `/dev/devices` from a session, in `test-interactive`; every key and click through the manager, in `check-input` and its `--no-ps2-irq` variant; the RAM disk's record, in `check-live`; the session line, in `check-login`; and the installer's graphical path, in `check-install` on demand |
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
rest of namespace layering; per-session visibility under `/storage`; and a **forcible kill**, so
that a program ignoring its session's end cannot outlive it (Part A's detail pass).
