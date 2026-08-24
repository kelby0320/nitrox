# service.toml Schema Specification

This document specifies the schema of service declaration files read by the service manager. Each service is declared in a TOML file describing what to run, what handles to grant, what dependencies exist, and how to supervise it.

**Status:** Pre-stabilization. The fields defined here are the initial set.

## Location and discovery

Service declarations live in **one file**, read by the service manager at startup. Today that is `/initramfs/etc/services.toml`; it will move into the system profile when profile projection can carry something other than a package's `bin/`.

**One file holding many services — changed 2026-08-21.** This section previously read: declarations live at `/store/<hash>-system-services/services/*.toml`, projected into `/etc/services/*.toml`, and "the service manager scans this directory at startup". Nothing in Nitrox can enumerate a directory of `.toml` files, so that scan was never implementable:

- The initramfs is a CPIO archive the kernel looks up **by name** (`kernel/src/initramfs.rs`, `lookup`); there is no iteration and no listing op.
- `sys_ns_enumerate` lists a namespace's **bindings** — mount points and kernel resources — and says in its own documentation that it is "not a filesystem `readdir`".
- `profile-server` does serve `File::ReadDir`, but projects only each package's `bin/`.

Building enumeration to serve the schema was rejected against changing the schema, which is pre-stabilization. See the decision log, 2026-08-21.

The file contains one `[service.<name>]` table per service, in any order. The service name comes from the table header.

**Malformed input, since one bad table must not cost the file.** A `[service.<name>]` with no `executable` is skipped and does not consume the next declaration. A service name that appears again after another service's table closed it is dropped rather than started twice. A `[service.<other>.restart]` table belongs to `<other>` and never leaks into the declaration being parsed.

**A repeated key keeps the first value** — `executable` and every `[restart]` key — so a declarations file cannot be steered by appending to it. That includes appending a whole second `[service.<name>.restart]` table, which re-enters the section rather than starting a new one. (Until 2026-08-21 this held for `executable` only, and the schema said otherwise; the restart keys were last-wins.)

**This is also how a test image differs from a release image.** The retrofit's rule is that a service under test is the service that ships (`docs/planning/test-path-retrofit.md`): the same `service-mgr` binary reads a file that, in a selftest image, carries one extra `[service.boot-probe]` table. Code identical, data different.

## File structure

Each `[service.<name>]` top-level table has sub-tables for handle grants, restart policy, and other concerns:

```toml
[service.network-manager]
executable = "/store/abc123-network-manager/bin/network-manager"
syscaps    = []
after      = ["device-manager", "logging"]

[service.network-manager.handles]
namespace  = { rights = ["lookup", "bind"], subtree = "/net" }
device     = { path = "/dev/net/eth0" }
log        = { channel = "network" }
control    = { kind = "ipc-channel" }

[service.network-manager.restart]
policy       = "on-failure"
max_attempts = 5
backoff      = "exponential"
```

The service name (`"network-manager"` here) is used for cross-references in `after`, `before`, etc.

## `[service.<name>]` fields

### `executable` (required, string)

Absolute path to the service executable. Resolved via the service manager's namespace. The path must point to a regular file with the `EXECUTE` right available.

Convention: store paths via the system profile (`/bin/network-manager` or directly `/store/<hash>-network-manager/bin/network-manager`).

### `syscaps` (optional, array of strings; default `[]`)

**Parsed and requested since 2026-08-24.** System capabilities to grant the service. Must be a subset of what the service manager itself holds.

**The kernel enforces that by attenuation, not by refusal**, and the difference matters when writing a declaration: `sys_process_spawn` computes `child = parent & requested`, so asking for more than the supervisor holds spawns the service successfully with the extra capabilities silently absent. Nothing reports the difference — not the kernel, and not the service manager, which has no way to read its own capability set. A service that needs a capability its supervisor lacks therefore fails at the point it tries to *use* it. See `TODO(spawn-syscap-attenuation)`.

A name the service manager does not recognise is **reported and not granted**, rather than dropped silently: a service that starts with less authority than it declared fails somewhere else entirely, which is how this key came to be implemented — the demo chain stopped at `session user bind FAIL` when it was moved into a declaration that could not yet say `BIND_NAMESPACE`.

Recognized values:

| String | SysCap |
|---|---|
| `"LOAD_MODULE"` | `SysCaps::LOAD_MODULE` |
| `"BIND_NAMESPACE"` | `SysCaps::BIND_NAMESPACE` |
| `"PHYSICAL_MEMORY"` | `SysCaps::PHYSICAL_MEMORY` |
| `"REAL_TIME"` | `SysCaps::REAL_TIME` |
| `"SYSTEM_CLOCK"` | `SysCaps::SYSTEM_CLOCK` |
| `"AUDIT_CONTROL"` | `SysCaps::AUDIT_CONTROL` |

Most services should hold zero syscaps. Granting `BIND_NAMESPACE` is reserved for services that themselves act as supervisors (e.g., a sub-service-manager). Granting `LOAD_MODULE` is for the device manager. Granting others requires equally strong justification.

### `after` (optional, array of strings; default `[]`)

**Parsed since 2026-08-24, with the narrow meaning that is implementable today.** Names of services that must have **finished** — exited — before this one is started.

For a service that exits (a one-shot), finishing *is* readiness. There is no readiness protocol for a service that keeps running, so naming one here would wait forever; the service manager bounds the wait, reports it, and starts the dependent anyway. A name matching no declaration, and a dependency that failed to spawn, are likewise reported and not fatal — refusing to start a service because of a mis-typed name would turn a typo into a silently missing service.

**Ordinary start order does not need `after`.** Declarations are started in file order, so "start B after A" is written by putting A first. `after` is for the stronger claim that A has already *finished* — which is what orders the boot self-test: the substrate checks that fire the verdict must not run until the demo chain has exited.

**`after` orders backwards only.** A dependency is matched against the services already started, so naming one declared *later* in the file does not wait for it — it is reported and the service starts. The file's order is the start order; `after` strengthens it rather than reordering it.

A dependency graph with topological sorting is the general answer and is not built; nothing yet needs one. Cycles are therefore not rejected at parse time, and they do not deadlock either: of two services naming each other, the first does not wait at all (its dependency has not started) and the second waits out the bound.

### `before` (optional, array of strings; default `[]`)

Inverse of `after`. Naming services that should be started after this one. Equivalent to those services declaring `after = [<this service>]`. Provided for ergonomic flexibility.

### `wants` (optional, array of strings; default `[]`)

Soft dependency. The service manager tries to start the named services first but doesn't fail if they don't start. Useful for non-critical optional services.

### `description` (optional, string)

Human-readable description. Shown in service status listings.

## `[service.<name>.handles]` table

Each key in this table specifies a handle to grant the service at spawn time. The slot name (the table key) is a convention — the service's `libos` startup code looks up handles by these names.

Convention slot names:

| Slot name | Meaning |
|---|---|
| `stdin` | Input stream |
| `stdout` | Output stream |
| `stderr` | Error/diagnostic stream |
| `log` | Log channel handle |
| `notification` | Notification channel (typically auto-provided; not needed in declaration) |
| `namespace` | Process namespace handle (typically auto-provided; not needed in declaration) |
| `control` | Control IPC channel — service manager keeps the other end |
| (custom) | Service-specific resources (block devices, configuration files, etc.) |

Each handle entry is a sub-table with one of several "kind" indicators determining what's granted.

### Handle entry by source

```toml
[service.foo.handles.namespace]
rights  = ["lookup", "bind"]
subtree = "/some/path"
```

A namespace subtree handle scoped to `subtree`, with the listed rights. The service manager looks up `subtree` in its own namespace, attenuates the rights, and grants the resulting handle. If the path doesn't exist, the service fails to start.

Recognized rights for namespace handles: `"lookup"`, `"bind"`, `"unbind"`, `"enumerate"`.

```toml
[service.foo.handles.config]
path   = "/etc/foo/foo.conf"
rights = ["read"]
```

A handle obtained by namespace lookup of `path` in the service manager's namespace. Granted with the listed rights (subset of what the lookup returns).

Recognized rights for resource handles: `"read"`, `"write"`, `"execute"`, plus modifiers `"seek"`, `"append"`, `"truncate"` as applicable.

```toml
[service.foo.handles.device]
path   = "/dev/something"
rights = ["read", "write"]
```

Same as above but conventionally for device nodes. Identical mechanism.

```toml
[service.foo.handles.log]
channel = "foo"
```

A log channel handle. At spawn the service manager resolves `system/<principal>` under the
logging service (a capability its own namespace permits) and binds the returned channel as
this handle — the service then logs **directly** to the logging service, which stamps
trusted provenance (`principal`/`tier`/`timestamp`/`sequence`) from the channel the record
arrived on. The service manager is not in the log data path: it establishes the channel at
namespace-construction time and steps out. (Earlier drafts had the service manager retain
the read end and *forward* records; that relay was dropped — see
`docs/architecture/logging.md` § Identity is capability-derived.) The `"foo"` here names the
log subsystem / `principal`.

```toml
[service.foo.handles.control]
kind = "ipc-channel"
```

An IPC channel pair created at spawn time. The service receives one end; the service manager retains the other. Used for lifecycle management (shutdown requests, health checks, configuration reloads).

**A service must hold its control endpoint until it exits.** This is a contract, not advice, and it is load-bearing for something a service cannot see: the service manager reads *that endpoint closing* as "this child is gone", because `KIND_CHILD_EXITED` names a child by pid and nothing maps a process handle back to a pid (`TODO(child-exit-attribution)`). A service that closes the handle early — a reasonable-looking thing to do if it serves no lifecycle protocol — is reported dead while it runs, and under `policy = "always"` gets a **second live copy** of itself. Found exactly that way in `boot-probe`, whose whole job was to demonstrate the opposite.

If a service has no use for the channel, the correct handling is to ignore it and let process teardown close it. There is no way for the manager to distinguish an early close from an exit.

### Handle ergonomics

Some handles are auto-provided without declaration:

- `namespace`: every process gets a namespace handle. The declaration's optional sub-fields specify scope/attenuation if non-default.
- `notification`: every process gets a notification channel.
- `stdin`, `stdout`, `stderr`: provided by service manager based on declared output routing (default: stdout/stderr go to the service's log channel; stdin is `/dev/null`).

Only handles that need explicit specification (custom subtrees, specific devices, custom config files) need to appear in the declaration.

## `[service.<name>.restart]` table

Controls supervisor behavior when the service exits.

### `policy` (required, string)

| Value | Behavior |
|---|---|
| `"never"` | Don't restart on any exit |
| `"on-failure"` | Restart only if exit was abnormal (non-zero exit code, crash, or killed) |
| `"always"` | Restart on any exit |

### `max_attempts` (optional, integer; default unlimited)

Maximum number of restarts before the service manager gives up and marks the service as failed. After giving up, the service manager logs a failure record and does not attempt further restarts unless explicitly requested.

### `backoff` (optional, string; default `"exponential"`)

Time-between-restarts strategy:

| Value | Behavior |
|---|---|
| `"none"` | Restart immediately |
| `"linear"` | Wait N seconds between attempts (N configurable via `backoff_initial`) |
| `"exponential"` | Double the wait each time, up to `backoff_max` |

### `backoff_initial` (optional, duration string; default `"1s"`)

Initial wait between restarts. Used as-is for `"linear"`, doubled for `"exponential"`.

### `backoff_max` (optional, duration string; default `"5min"`)

Maximum wait for `"exponential"` backoff.

## `[service.<name>.environment]` table (optional)

Environment variables to pass to the service via the typed envmap. Keys are environment variable names; values are the values:

```toml
[service.foo.environment]
LOG_LEVEL = "info"
WORKERS   = 4
```

Values may be strings, integers, booleans, floats, or arrays. They map onto `Value` types in the typed envmap.

## `[service.<name>.argv]` table (optional)

Command-line arguments to pass to the service. An array of values:

```toml
[service.foo]
executable = "/bin/foo"

[service.foo.argv]
args = ["--config", "/etc/foo.conf", "--workers", 4]
```

Values are typed: strings, integers, etc., per the `Value` enum. The service receives them as a `Value::List` in the spawn args.

## Examples

### Minimal service

```toml
[service.hello]
executable = "/bin/hello"
syscaps    = []
after      = []

[service.hello.restart]
policy = "on-failure"
```

This service has only what it absolutely needs: executable, no syscaps, no dependencies, no special handles, restart-on-failure with default backoff. It receives the auto-provided handles (namespace, notification, stdin/stdout/stderr, control).

### Logging service

```toml
[service.logging]
executable  = "/bin/logging"
syscaps     = []
after       = []
description = "Structured log aggregator"

[service.logging.handles.namespace]
rights  = ["lookup", "bind"]
subtree = "/var/log"

[service.logging.handles.storage]
path   = "/var/log/storage"
rights = ["read", "write"]

[service.logging.handles.control]
kind = "ipc-channel"

[service.logging.restart]
policy       = "always"
backoff      = "exponential"
backoff_max  = "1min"
```

Logging needs to bind into `/var/log` (so other services can address it) and write to its storage backend.

### Privileged supervisor: device manager

```toml
[service.device-manager]
executable  = "/bin/device-manager"
syscaps     = ["LOAD_MODULE"]
after       = ["logging"]
description = "Hardware device manager and Tier 2 driver loader"

[service.device-manager.handles.namespace]
rights  = ["lookup", "bind"]
subtree = "/dev"

[service.device-manager.handles.acpi]
path   = "/dev/acpi"
rights = ["read"]

[service.device-manager.handles.log]
channel = "device-manager"

[service.device-manager.handles.control]
kind = "ipc-channel"

[service.device-manager.restart]
policy       = "always"
max_attempts = 3
```

Device manager has `LOAD_MODULE` because that's its job. It binds into `/dev` to register newly-discovered device nodes. It reads the kernel's ACPI resource server. It logs to its own log subsystem.

## Validation

The service manager validates declarations at parse time:

- Required fields present (a table with no `executable` is skipped, not fatal to the file)
- `syscaps` are a subset of those the service manager itself holds
- Dependency graph (`after`, `before`, `wants`) has no cycles
- Restart policy values are recognized
- Handle entries reference valid kinds
- Paths in handle entries are syntactically valid (resolvable validity is checked at start time, not parse time)

Parse-time validation failures cause the service manager to log an error and skip the service. The service is reported as "misconfigured" in status listings.

Start-time failures (path doesn't resolve, executable not found, spawn fails) cause the service to be reported as "failed-to-start" with the reason logged.

## Where to read more

- [Service manager design](../architecture/service-manager.md)
- [Why supervisor-mediated registration](../rationale/why-supervisor-registration.md)
- [Why capabilities](../rationale/why-capabilities.md) — the structural enforcement of handle grants
