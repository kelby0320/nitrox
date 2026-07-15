# service.toml Schema Specification

This document specifies the schema of service declaration files read by the service manager. Each service is declared in a TOML file describing what to run, what handles to grant, what dependencies exist, and how to supervise it.

**Status:** Pre-stabilization. The fields defined here are the initial set.

## Location and discovery

Service declarations live in the system profile (typically `/store/<hash>-system-services/services/*.toml` projected into `/etc/services/*.toml` via the system profile namespace).

The service manager scans this directory at startup, parses each file, and builds a dependency graph. Each file declares one service. The filename is informational; the service name comes from the `[service.<name>]` table inside.

## File structure

Each file contains one `[service.<name>]` top-level table, with sub-tables for handle grants, restart policy, and other concerns:

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

System capabilities granted to the service. Must be a subset of capabilities the service manager itself holds — the kernel rejects spawn requests that try to amplify.

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

Names of services that must reach "ready" state before this service is started. The service manager builds a dependency graph from these entries and topologically sorts startup.

Cycles in the graph are rejected at parse time.

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

A log channel handle. The service manager constructs (or reuses) a log channel for the named log subsystem (`"foo"` here). The channel has `WRITE` right; the service manager retains the read end and forwards records to the logging service.

```toml
[service.foo.handles.control]
kind = "ipc-channel"
```

An IPC channel pair created at spawn time. The service receives one end (granted with `SEND | RECV | TRANSFER`); the service manager retains the other end. Used for lifecycle management (shutdown requests, health checks, configuration reloads).

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

- Required fields present
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
