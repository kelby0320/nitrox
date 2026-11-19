# services — what the system runs, each service in turn, and what breaks without it

## Description

A service is a program the system keeps running for the other programs to use: it serves a path,
and a program that opens that path is talking to it. `auth-service` serves `/svc/auth`, and the
login asks it whether a password is right. A service is not started by the program that needs it,
and not by whoever is logged in: `service-mgr` starts it, and binds its path where the programs
that need it will find it. A server never registers itself.

`service-mgr` reads what to run from one file, `/system/services.toml`, one `[service.NAME]` table
per service, and starts them in the order the file lists them. A service that serves a path names
it as its `endpoint`, and `service-mgr` waits until the service says it is ready before it starts
the next, so a service may use any listed before it. `services.toml(5)` is the file's format.

Each table also says three things about a service:

- **Its restart policy**, what `service-mgr` does when the service exits: `never` leaves it down,
  `on-failure` starts it again after an abnormal exit, and `always` after any exit, each after the
  pause and up to the number of times the table sets. Every service in the release image says
  `never`: one that exits stays down until a person starts it.
- **Whether it is essential.** An essential service cannot be stopped or restarted by anyone,
  `with admin` or not, since stopping it would lock the administrator out, lose state nothing
  rebuilds, or leave programs holding a path to a server that will not come back: the clients of
  the terminal, input and display servers do not reconnect. It still stops at a shutdown.
- **Whether it is critical.** A critical service is one the boot cannot go on without. If it does
  not come up at boot, `service-mgr` starts nothing more and asks `init` for the emergency shell;
  see `boot(7)`. Once the system is up, a critical service's exit is its restart policy's, like
  any other's.

In the release image every service but `clipboard-server` is essential, and `auth-service` and
`logging-service` are critical.

Not everything the system runs is a service. `init` starts a few programs before `service-mgr`
exists, and `service-mgr` starts the two session supervisors itself. Those are below, after the
services, and `service` does not list or control them.

## Seeing, stopping and starting

`service --list` shows each service the file declares, whether it is starting, running, stopped or
failed, and how many times it has restarted. It works in any session.

```
service --list
```

`service --start NAME`, `--stop NAME` and `--restart NAME` need the `services` grant, which the
`admin` view carries: `with admin service --stop clipboard-server`. NAME is the service's name as
the headings below spell it. Each waits until the change has happened. A stop is a request: a
service that is asked and does not exit within five seconds is reported as still running, and is
stopped if it exits later. There is no way to kill one.

An essential service's stop or restart is refused whoever asks, and `service-mgr`'s reason is
printed. So the only service the release image lets a person stop or restart is
`clipboard-server`; any service that is not running, essential or not, can be started. A service
that has stopped stays down, the policy being `never`, until `--start` or `--restart`.

`service(1)` has the options and the exit status. `with admin log PRINCIPAL` reads what one
service sent to the log; see `log(1)`.

## The services

### auth-service

Serves `/svc/auth`: the credential oracle. It holds the user database, answers "is this password
right, and who is this?" for both logins, and writes the running system's database when `account`
adds or removes an account or sets a password, through the view broker.

Without it nobody can log in, on the serial console or the screen. It is critical: if it does not
come up at boot, the boot goes to the emergency shell. It is essential, and its restart policy is
`never`. It is the first service started.

### logging-service

Serves `/log`: the service log. A service that serves no path of its own is given a channel to it
by `service-mgr`, stamped with its name, so what it logs is attributed to it and not to what it
claims; a server opens its own. The records go to the serial console and into a memory ring that
keeps the most recent ones, which `log` reads back. In the release image the view broker is the
server that logs there, its audit of who was given which view among it; the other servers write
their lines to the console alone.

Without it nothing is logged and `log` has nothing to read. It is critical, essential and `never`,
and it starts second, before the view broker that audits to it.

### tty-server

Serves `/dev/tty`: terminals. It owns the serial console, and a shell's terminal, a terminal
window's and the login prompt's are all a terminal of its. It does a terminal's line editing and
echo, and keeps each session's input from every other's.

Without it no shell has a terminal, and the programs that held one do not reconnect to a new
one. It is essential and `never`, and not critical.

### clipboard-server

Serves `/dev/clipboard`: the clipboard, a ring of what was last copied that `clip` and the desktop's
applications read and write, in either session. A session that has no `/dev/clipboard` bound has
no clipboard at all.

Without it copying and pasting fail, and nothing else does. It is the one service in the release
image that is not essential, so it is the one a person can stop and restart:
`with admin service --restart clipboard-server`. Its policy is `never`, and it is not critical.

### view-broker

Serves `/svc/views`: the view broker. When `with` asks for a view, such as `admin`, the broker
reads `/system/views.toml`, decides whether this account may have it and whether to ask for its
password, and builds the view, a namespace holding what the view grants. It audits each request to
the log. It fronts the account operations and the changes to the policy, and the desktop's power
menu asks it to run `shutdown` in the `power` view. It holds `BIND_NAMESPACE`, to build views, and
the clock, so that the `clock` grant can bind it into a view.

Without it `with` fails, so nothing can be administered from a session, and the desktop cannot
restart or shut down. It is essential and `never`, and not critical.

### device-mgr

Serves `/svc/devices`: the device manager. It learns what devices the machine has and hands each
to the service that owns its class: keyboards and pointers to `input-server`, disks to
`storage-service`. It answers a table of the devices to every session, at `/dev/devices`.

Without it no device that arrives later is handed on, and `/dev/devices` fails; an owner keeps what
it already holds. It is essential and `never`, and not critical.

### storage-service

Serves `/svc/storage`: disks, and the filesystems on them. It knows what is on each disk and
mounts the ones it can serve, other than the root `init` mounted: every ext4 filesystem, and a FAT
filesystem on a removable disk such as a USB stick. It starts a filesystem server for each it
mounts, `fs-server-ext4` for ext4 and `fs-server-fat` for FAT, and a session sees the result as
`/storage/LABEL`, and its table as `/dev/storage`. `disk` mounts, unmounts and ejects through it,
and asks it to read a disk again after writing a partition table or a filesystem itself. It holds
`BIND_NAMESPACE`, to bind each mount.

On a live boot the machine's own disks are mounted read-only; a removable disk is mounted
writable on any boot.

A stop writes back and unmounts every filesystem it mounted, so it is given 60 seconds, not five,
and a shutdown gives it as long. Without it no disk but the root is reachable, and a stick shows
nowhere. It is essential and `never`, and not critical.

### input-server

Serves `/dev/input/new`: the keyboards and pointers of the machine merged into one stream of
events, which the compositor reads. It takes its devices from `device-mgr`, which is why it is
started after it, and it is started before the compositor, which connects to it as it starts.

Without it the desktop has no keyboard and no mouse, and its client, the compositor, does not
reconnect to a new one. It is essential and `never`, and not critical.

### compositor

Serves `/dev/draw`: the display. It owns the screen, draws each application's windows onto it,
decides which window has the focus, and sends the input it reads from `input-server` to it. It is
the last server started.

Without it there is no screen to log in on, and its clients do not reconnect to a new one. The
serial login is unaffected. It is essential and `never`, and not critical.

## Started by init

`init` starts these before `service-mgr` exists, in this order in a boot. They are not services:
`service` does not list them, and cannot stop or start them. `init` closes its handle to each once
it has started it, and reports an exit it reaps on the console as `init: reaped pid=N code=C`;
it does not restart one.

### fs-server-ext4

Serves the root filesystem, mounted at `/`, from the disk partition `init.toml(5)` names: in the
release image, the partition labelled `nitrox-root`, read-write. `init` waits up to 30 seconds for
it to say it is ready; if it cannot be started, or refuses the device, the boot goes to the
emergency shell. `storage-service` starts more copies of this server for each other ext4
filesystem. If this one exits, the files under `/` can no longer be reached.

### profile-server

Serves `/bin`: the programs of the system profile, which a program is found in by its bare name.
It is started once the root is mounted, since the profile's programs live in the store on it, and
the boot goes to the emergency shell without it: `service-mgr` and every service are started from
`/bin`. If it exits, a program that is not already running cannot be found by name.

### service-mgr

Starts and supervises everything in `/system/services.toml`, and the two session supervisors; it
serves `/svc/services`. `service` reads its table at `/dev/services` in a session, and asks it to
start and stop through the `services` grant. It is the program `init` hands the boot on to, and
`init` stays resident beside it. `init` does not restart it. If it exits, `init` writes that
nothing supervises the services now and that the machine needs a restart: the paths of the
servers go with it, and a second `service-mgr` would start a second copy of every server.

### eshell

The emergency shell. It is started only on the emergency path, when the boot cannot go on, and is
not part of a boot that works. See `boot(7)`. It runs on the serial console and has five commands.
Nothing restarts it.

## The sessions

`service-mgr` starts these two after the last service, and each runs for the length of the boot:
it presents a login, runs a session for whoever logs in, closes it when its leader exits, and
presents the login again. They are not services: `service` does not list them and cannot stop
them, and nothing restarts one that exits. `service-mgr` asks each to end its session at a
shutdown.

### session-mgr

The login on the serial console. It asks for a name and a password, has `auth-service` check
them, builds the session's namespace, and runs `nxsh` in it for the account that logged in. It
holds `BIND_NAMESPACE`, to build namespaces. A denied login is asked again. If it exits, the serial
console has no login, and the graphical one is unaffected.

### desktop-session-mgr

The login on the screen: the greeter. It draws the greeter as a window on the compositor, has
`auth-service` check the password, builds the session's namespace, and runs `desktop-shell` in it,
which starts the person's applications. When the person logs out it presents the greeter again.
If it exits, the screen has no login, and the serial one is unaffected.

## See also

`boot(7)`, `service(1)`, `with(1)`, `log(1)`, `services.toml(5)`, `init.toml(5)`, `views.toml(5)`,
`rsproto-services(7)`, `desktop(7)`
