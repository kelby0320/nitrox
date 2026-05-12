# init.toml Schema Specification

This document specifies the schema of `init.toml`, the bootstrap mount manifest read by init from the initramfs during early boot. The file describes the critical-path mounts that init must establish before handing off to the service manager.

**Status:** Pre-stabilization. The fields defined here are the initial set; additions are likely.

## Location

The file is at `/etc/init.toml` within the initramfs. Path is hard-coded; init looks it up via the in-kernel initramfs resource server bound at `/initramfs/`.

## File format

Standard TOML. UTF-8 encoded. No external imports or includes.

## Top-level structure

The file is an array of `[[mount]]` tables, one per critical-path mount. Order in the file does not matter for processing; init topologically sorts mounts by mount point depth (shallower paths processed first).

```toml
[[mount]]
fs_server    = "fs-server-ext4"
device       = "gpt-partuuid:01234567-89ab-cdef-0123-456789abcdef"
mount_point  = "/"
mode         = "rw"
required_for = "boot"

[[mount]]
fs_server    = "fs-server-xfs"
device       = "gpt-partlabel:store"
mount_point  = "/store"
mode         = "ro"
required_for = "boot"
```

## Mount table fields

### `fs_server` (required, string)

Name of the fs-server binary, resolved within `/initramfs/sbin/`. The initramfs build process must have included this binary in the closure.

Examples:
- `"fs-server-ext4"`
- `"fs-server-fat"`
- `"fs-server-btrfs"` (if compiled and included in initramfs)

If the named binary is not found in the initramfs, init drops to emergency shell with an error indicating the missing binary.

### `device` (required, string)

Stable identifier for the block device to mount. Format: `<scheme>:<value>` where `<scheme>` is one of:

| Scheme | Value | Resolves via |
|---|---|---|
| `gpt-partuuid` | UUID string (lowercase hex with hyphens) | `/dev/disk/by-partuuid/<uuid>` |
| `gpt-partlabel` | Partition label string (UTF-8) | `/dev/disk/by-partlabel/<label>` |
| `fs-uuid` | Filesystem UUID (post-mount; not for boot use) | (not initially supported) |
| `device-path` | Direct path under `/dev/` (e.g., `nvme0n1p2`) | `/dev/<path>` |

For the initial implementation, `gpt-partuuid` and `gpt-partlabel` are the recommended schemes — they're stable across reboots and don't depend on enumeration order. `device-path` is supported but discouraged because device naming is enumeration-order-dependent and can change between reboots.

If the device cannot be resolved, init drops to emergency shell.

### `mount_point` (required, string)

Path in the system namespace at which to bind the fs-server's resource server endpoint. Must be an absolute path. Common values:

- `/` for the root filesystem
- `/store` for a content-addressed store on a separate partition
- `/home` for user home directories on a separate partition

The path does not need to "exist" in any prior sense — init binds the fs-server endpoint at this path, creating the namespace entry. If a binding already exists at this path (e.g., from a previous mount in dependency order), behavior is implementation-defined; the current implementation rejects the duplicate.

Mount points are processed in shallowest-first order (`/` before `/store`, `/store` before `/store/data`). This ensures parent paths exist before children are bound.

### `mode` (required, string)

Either `"ro"` (read-only) or `"rw"` (read-write). Determines the rights init grants when binding the fs-server's endpoint:

| Mode | Rights granted |
|---|---|
| `"ro"` | `LOOKUP \| READ \| MAP_READ` |
| `"rw"` | `LOOKUP \| READ \| WRITE \| MAP_READ \| MAP_WRITE` |

Both modes grant `LOOKUP`. Neither grants `BIND` to the binding's destination process by default — `BIND` is a supervisor capability and is granted explicitly via service.toml service declarations, not implicitly via mount declarations.

For `/store`, `"ro"` is conventional even when the underlying filesystem is writable. The package manager has a different namespace route to write to the store; ordinary processes see `/store` as read-only.

### `required_for` (required, string)

When this mount must succeed. Currently the only supported value is `"boot"`, meaning the mount is on the critical path and init must complete it before proceeding.

Future values (planned but deferred):
- `"emergency-only"`: mount only when entering emergency mode
- `"lazy"`: mount on first access rather than at boot
- A list of service names: mount before those services start

For initial implementation, all `[[mount]]` entries are `"boot"`. The field is required to make future expansion explicit.

### `options` (optional, table)

Per-fs-server options. Format depends on the `fs_server`. Examples (illustrative; specifics depend on the fs-server):

```toml
[[mount]]
fs_server   = "fs-server-ext4"
device      = "gpt-partuuid:01234567-89ab-cdef-0123-456789abcdef"
mount_point = "/"
mode        = "rw"
required_for = "boot"

[mount.options]
data_journal = true
discard      = true
```

Init passes the `options` table verbatim to the fs-server via its control channel during the Ready handshake. The fs-server interprets the options. Unknown options are an fs-server-defined error (typically logged-and-ignored or reported via the Ready exchange).

## Processing semantics

Init processes the manifest as follows:

1. Parse the file. Reject if invalid TOML or missing required fields.
2. Topologically sort mount entries by `mount_point` depth (shallowest first).
3. For each mount, in order:
   a. Look up the fs-server binary in the initramfs.
   b. Look up the device handle.
   c. Create a control IPC channel.
   d. Spawn the fs-server with appropriate handles and the spawn-time control channel handle.
   e. Wait on the control channel for a `Meta::Ready` message.
   f. Extract the endpoint handle from the Ready message.
   g. Call `sys_ns_bind(system_namespace, mount_point, endpoint, derived_rights)`.
4. If all mounts succeed, proceed to read `/system/current-generation` and continue normal boot.
5. If any mount fails, log the failure to the kernel log and spawn the emergency shell. Wait for eshell exit (typically a reboot).

## Failure modes and emergency mode

A mount fails if any of these occur:

- The named fs-server binary doesn't exist in the initramfs
- The named device cannot be resolved
- Spawning the fs-server fails (e.g., out of memory, bad ELF)
- The fs-server doesn't send a Ready message within the timeout (default 30 seconds)
- The fs-server sends an error reply instead of Ready
- The `sys_ns_bind` call fails (rights mismatch, naming conflict, etc.)

On any failure, init writes a structured error message to the kernel log indicating which mount failed and why, then spawns eshell with a pre-populated context describing the failure. The user can inspect the situation, edit `init.toml` from within eshell, and reboot.

## Examples

### Single ext4 root partition (the common case)

```toml
[[mount]]
fs_server    = "fs-server-ext4"
device       = "gpt-partuuid:01234567-89ab-cdef-0123-456789abcdef"
mount_point  = "/"
mode         = "rw"
required_for = "boot"
```

Once mounted, `/store`, `/home`, `/system`, etc., are all subtrees of the same root fs-server. Namespace composition (by service-manager and session-manager) provides the per-process scoping.

### Multi-partition layout

```toml
[[mount]]
fs_server    = "fs-server-ext4"
device       = "gpt-partlabel:root"
mount_point  = "/"
mode         = "rw"
required_for = "boot"

[[mount]]
fs_server    = "fs-server-xfs"
device       = "gpt-partlabel:store"
mount_point  = "/store"
mode         = "ro"
required_for = "boot"

[[mount]]
fs_server    = "fs-server-btrfs"
device       = "gpt-partlabel:home"
mount_point  = "/home"
mode         = "rw"
required_for = "boot"
```

Each partition gets its own fs-server instance. The initramfs must include `fs-server-ext4`, `fs-server-xfs`, and `fs-server-btrfs` binaries.

### Single root, separate /home for safety

```toml
[[mount]]
fs_server    = "fs-server-ext4"
device       = "gpt-partlabel:root"
mount_point  = "/"
mode         = "rw"
required_for = "boot"

[[mount]]
fs_server    = "fs-server-ext4"
device       = "gpt-partlabel:home"
mount_point  = "/home"
mode         = "rw"
required_for = "boot"
```

Two ext4 partitions, both served by separate `fs-server-ext4` processes (same binary, different instances).

## Future fields (deferred)

These are expected to be added but are not in the initial scope:

- `passphrase_prompt`: trigger LUKS passphrase prompt before mount (encrypted root)
- `crypto_setup`: cryptsetup-equivalent block-device-filter setup
- `lvm_volume_group`: activate LVM volume group before mount
- `network_address`: NFS / iSCSI mount source (when networking is available at boot)

Adding any of these requires the corresponding kernel module or userspace tool to be in the initramfs.

## Where to read more

- [Bootstrap mount topology](../architecture/bootstrap-mount-topology.md)
- [Boot flow](../architecture/boot-flow.md)
- [Why supervisor-mediated registration](../rationale/why-supervisor-registration.md)
