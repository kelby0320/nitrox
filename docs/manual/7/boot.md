# boot — what starts when the machine is turned on, in order, and who supervises what

## Description

A boot is a chain, each link starting the next and handing it only what it needs. Nothing in it
runs as a superuser: every program holds the handles it was given, and the chain hands authority
down, never up.

1. **Firmware and Limine.** The UEFI firmware loads Limine from the disk's EFI System Partition.
   Limine reads its configuration and loads the kernel and the initial archive of
   programs, the initramfs, beside it.
2. **The kernel.** It sets up memory, the processors and its drivers, then starts exactly one
   program, `init`, as process 1, with the machine's resources as the handles that program holds.
   The kernel's own messages are written to the serial console, and to the screen until the
   graphical session takes it.
3. **`init`.** It reads its manifest, `init.toml(5)`, from the initramfs, starts a filesystem
   server for each mount it names and binds each at its mount point. In the release image that is
   one: `fs-server-ext4`, which serves the root. It then starts `profile-server` and binds it at
   `/bin`, so that every program the system runs after this is found by name. It starts
   `service-mgr`, hands it the means to start and bind servers, and stays resident.
4. **`service-mgr`.** It reads `/system/services.toml`, which `services.toml(5)` describes, and
   starts each service in the order the file lists them, waiting for each to say it is ready
   before the next. Then it starts the two session supervisors.
5. **The sessions.** `session-mgr` presents the login on the serial console, and runs `nxsh` for
   whoever logs in. `desktop-session-mgr` draws the graphical login, the greeter, and runs
   `desktop-shell` for whoever logs in, which starts that person's applications.

`init` mounts the root before it starts `service-mgr` because `service-mgr`, its declarations and
every server are files on that root. The services, and what each one does, are in `services(8)`.

## The services, in order

`service-mgr` starts these, one at a time, in this order. A later one may use any earlier one, so
the order is not tidiness:

`auth-service`, `logging-service`, `tty-server`, `clipboard-server`, `view-broker`, `device-mgr`,
`storage-service`, `input-server`, `compositor`.

Three of these orders are the ones that matter. The log comes before the view broker, which writes
its audit to it. The device manager comes before the storage service and the input server, which
take their devices from it. And the input server comes before the compositor, which connects to it
as it starts.

A server that does not say it is ready within 30 seconds is not waited for further. If it is one of
the two critical ones, the boot goes to the emergency shell instead (below).

The login supervisors start after the last server, so every server a session needs is already up.
`session-mgr` is started first, then `desktop-session-mgr`; neither knows of the other, so a fault
in the graphical login leaves the serial one, and a machine whose screen is unusable still has its
serial console.

## Who restarts what

Nothing does, in the release image. Every service's restart policy there is `never`: a service
that exits stays down until a person starts it, with `with admin service --start NAME`.

- `service-mgr` restarts none of its services, and refuses to stop or restart the ones marked
  essential, whoever asks.
- `service-mgr` does not restart the two session supervisors. A session that ends, because the
  person logged out, is not a supervisor's exit: it presents its login again.
- `init` does not restart `service-mgr`. If `service-mgr` exits, `init` says so on the console, and
  the services are unsupervised from then on; the machine needs a restart.
- `init` does not restart the filesystem server or the profile server it started either. It
  reports the exit of each program it started. Nothing takes over a program whose parent has gone:
  its exit is told to no one.

## The emergency path

If the boot cannot reach a working system, `init` starts `eshell`, the emergency shell, on the
serial console instead of going on. That happens when:

- `init.toml` cannot be read or does not parse, or a mount it names cannot be made;
- the profile server does not start, or does not say it is ready;
- `service-mgr` cannot be started; or
- `service-mgr` asks for it, because a service marked critical did not come up, or because the
  declarations file has no usable critical service. Only `auth-service` and `logging-service` are
  critical in the release image.

`eshell` is small on purpose, so that as little as possible has to work for it to. It reads the
serial console directly, not through the terminal server. It has five commands: `help`, `echo`,
`lsblk` to list the block devices, `cat` to print a file, and `mounts` to list what is bound in
the namespace it was started with. It cannot change anything and has no way to reboot; the way
out is the machine's own power or reset.

It takes its input from the serial console, so a machine without one has nothing to type at.

## Seeing a boot afterwards

- `service --list` shows each service the system declares, whether it is running, and how often it
  has restarted. It works from any session; see `service(1)`.
- `with admin log` reads the service log, the records programs sent to the logging service, with
  the view broker's audit among them; `with admin log --kernel` reads the kernel's own lines. See
  `log(1)`.
- `init` and `service-mgr` write what they do, line by line and each prefixed with its own name, to
  the serial console as it happens: that is where to see which step a boot stopped at, and which
  server said what.

## The live image

The live image is the same boot with its root in memory, for a USB stick: its `init.toml` names
that memory disk as the root, and the storage service mounts the machine's own disks read-only:
they are what an install writes over, and a write made to one by accident could not be taken
back. Everything after the root is as above.

## See also

`services(8)`, `service(1)`, `log(1)`, `with(1)`, `init.toml(5)`, `services.toml(5)`, `desktop(7)`,
`intro(7)`
