# Administration: elevation, admin tools, and who is allowed to

**Status: a stub, and scheduled** — after [the desktop refresh](desktop-refresh.md) and before
Phase 6 (2026-09-17), because its tools are UI surfaces and the refresh decides what those look
like. **Its internal scope is still open**; the questions at the end of this file are what a
detail pass would answer. Written 2026-09-16 while planning Phase 5 Part H (the installer),
because the installer is the first program that needs authority an ordinary session cannot have, and
building it without a stated destination is how you build something that has to be removed later.
This file exists so
that decision starts from a page rather than a blank one, and so today's installer can be shaped
towards it.

## What is missing

The system has **one kind of account**: an unprivileged one that logs in, gets a session namespace,
and runs applications. There is no way for it to do anything administrative, and no tools to do it
with:

- **Elevation.** No way to run one program with more authority than the session has.
- **An administrative account.** `/system/users` records credentials and a home; nothing records
  that an account may administer anything.
- **Disks.** No partitioning, no formatting, no mounting or unmounting at runtime. `init.toml`
  mounts are decided at boot and nothing changes them afterwards.
- **Accounts.** No adding or removing users, no password changes — not even for your own.
- **Everything else an installed system eventually wants**: services started and stopped by hand,
  the clock, the network once it exists, software installed or removed. To be scoped, not listed
  exhaustively here.

## The shape the architecture already dictates

**Elevation is not "become root", because there is no root to become.** There are no UIDs
(`CLAUDE.md`, core rules) and authority is what a supervisor put in your namespace. So the
equivalent of `sudo` is a **broker**: a service that

1. is spawned by `service-mgr` holding the privileged bindings (`/dev/blk`, `/system/users`, a
   mount-capable endpoint) the way `session-mgr` holds the root filesystem's;
2. **authenticates the person itself**, through `auth-service`, in a prompt it owns — a caller that
   drew the password prompt is a caller that has the password;
3. checks a **policy** about who may elevate and to what; and
4. **spawns the program**, into a namespace it constructs.

The caller never receives the authority. It gets a running process and its output, exactly as
`service-mgr` gives a client a server's endpoint and never its capability. This is the
supervisor-registration rule (`why-supervisor-registration.md`) applied to programs a person starts
rather than to services a system declares.

**"Admin" is therefore policy, not identity**: a fact recorded in `/system/users` about which
accounts the broker will elevate, read by the broker, and meaning nothing to the kernel.

**Every tool receives its authority; none asks for it.** A partitioner, a mount tool and the
installer all resolve what they need in their own namespace and work with what is there. That is
what makes the same binary usable from an installer environment today and from an elevated session
later.

## What already exists to build on

- `auth-service` — credentials, and the only thing that reads the password file.
- `service-mgr`, `session-mgr`, `init` — supervisors holding `BIND_NAMESPACE`, already constructing
  namespaces per session and per application.
- `libsession::build_namespace` — the session sandbox, whose deliberate omissions (`/dev/blk` among
  them) are what make elevation meaningful rather than decorative.
- `/dev/blk/<n>` with `READ | WRITE`, and `fs-server-ext4`'s library, which any program can link to
  write a filesystem without mounting one.
- `/system/users`, and the profile projection that already decides what `/bin` contains per session.

## The installer is the first client

Phase 5 Part H builds `nxinstall` against the invariant above: it never obtains authority for
itself. Today an **installer environment** — a boot-menu entry that starts a session whose namespace
includes the disks — provides it. When this phase exists, `elevate nxinstall /dev/blk/0` from an
ordinary desktop provides the same namespace after authentication, and the binary does not change.

The full installer this phase makes possible is the familiar one: guided partitioning, a filesystem
of your choosing, an administrative account and an unprivileged account created during the install,
and a password set for each. Part H deliberately builds none of that — it installs a fixed layout
from a live image — but it is built so that each of those becomes an addition rather than a
replacement.

## Open questions, for when this is scoped

- **How policy is expressed.** A flag per account, a list of programs, something with roles. The
  smallest thing that answers "may this person run this program elevated" is probably right.
- **Per-program or a session.** `sudo <cmd>` or `sudo -i`: one is a spawn, the other is a second
  session with a wider namespace, and they have different blast radii.
- **How a graphical caller asks.** The prompt must belong to the broker, which on this system means
  a window the compositor trusts — a question the display arm has not had to answer yet.
- **Audit.** Every elevation is a thing that happened and probably belongs in `/log`.
- **Recovery.** What an installed system does when no account can elevate — an admin account with a
  forgotten password is the ordinary case, and the live image is the ordinary answer.
- **Whether the broker is one service or several.** Disks, accounts and services are different
  authorities, and one process holding all of them is the thing this architecture usually splits.
