//! The storage service's decisions: everything that can be wrong about a disk without a boot to
//! show it. The binary is the event loop: taking `block` from the device manager, reading each
//! device, and serving.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod probe;

pub mod sources {
    //! **Which devices `init` mounted, and whether this is a live boot.**
    //!
    //! `init` records its mounts as namespace bindings, and nothing names the device behind one;
    //! `init.toml` does: every `[[mount]]` is critical-path, so on a running system each one
    //! succeeded, and its source names a partition by one of two schemes.
    //! - `gpt-partlabel:<label>`: the first partition record whose name is the label, as the
    //!   kernel binds `/dev/disk/by-partlabel/<label>`.
    //! - `gpt-partuuid:<uuid>`: a partition's unique GUID, which the registry does not carry, so it
    //!   is read from the parent disk's table. The kernel publishes partitions in table order,
    //!   skipping unused entries, so the k-th entry in use is the k-th partition record under that
    //!   disk; its name and size confirm it, and it is unmatched if they differ.
    //!
    //! **A live boot is one whose root is on a RAM disk**, which makes the machine's own disks the
    //! install target: an internal disk is auto-mounted read-only there (a removable one writable).

    use alloc::string::String;
    use alloc::vec::Vec;
    use libinittoml::manifest::{Manifest, Mode};
    use libkern::device::{DeviceKind, DeviceRecord, NO_PARENT};

    /// One entry in use in a disk's partition table, as much of it as matching needs.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct TableEntry {
        /// The partition's unique GUID, in the table's byte order.
        pub guid: [u8; 16],
        /// Its label.
        pub name: Vec<u8>,
        /// Blocks it covers.
        pub blocks: u64,
    }

    /// A disk's partition table: the disk's registry id, and its entries in use, in array order.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct DiskTable {
        /// The disk's registry id.
        pub disk: u32,
        /// Its entries in use.
        pub entries: Vec<TableEntry>,
    }

    /// One of `init`'s mounts, and the device it is on.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct InitMount {
        /// Where `init` bound it.
        pub mount_point: String,
        /// The `device` it names, as `init.toml` spells it.
        pub source: String,
        /// `ro` or `rw`.
        pub mode: Mode,
        /// The registry id of the device the source names, or `None` if no device matches.
        pub device: Option<u32>,
    }

    /// `init`'s mounts, each matched to its device.
    pub fn init_mounts(manifest: &Manifest, records: &[DeviceRecord], tables: &[DiskTable]) -> Vec<InitMount> {
        manifest
            .mounts
            .iter()
            .map(|m| InitMount {
                mount_point: m.mount_point.clone(),
                source: m.device.clone(),
                mode: m.mode,
                device: source_device(&m.device, records, tables),
            })
            .collect()
    }

    /// The registry id of the partition `source` names, found as `init` would resolve it.
    pub fn source_device(source: &str, records: &[DeviceRecord], tables: &[DiskTable]) -> Option<u32> {
        let partitions = || records.iter().filter(|r| r.kind() == DeviceKind::Partition);
        if let Some(label) = source.strip_prefix("gpt-partlabel:") {
            if label.is_empty() {
                return None;
            }
            return partitions().find(|r| r.name() == label.as_bytes()).map(|r| r.id);
        }
        let uuid = source.strip_prefix("gpt-partuuid:")?;
        for t in tables {
            let Some(k) = t.entries.iter().position(|e| partuuid(&e.guid) == uuid) else {
                continue;
            };
            let entry = &t.entries[k];
            let part = partitions().filter(|r| r.parent == t.disk).nth(k)?;
            return (part.name() == entry.name.as_slice() && part.block_count == entry.blocks).then_some(part.id);
        }
        None
    }

    /// A GUID in the form the kernel names `/dev/disk/by-partuuid/<uuid>` with: lowercase, the
    /// first three fields byte-swapped from the table's little-endian order.
    pub fn partuuid(guid: &[u8; 16]) -> String {
        const ORDER: [usize; 16] = [3, 2, 1, 0, 5, 4, 7, 6, 8, 9, 10, 11, 12, 13, 14, 15];
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut out = String::with_capacity(36);
        for (i, &o) in ORDER.iter().enumerate() {
            out.push(HEX[(guid[o] >> 4) as usize] as char);
            out.push(HEX[(guid[o] & 0xF) as usize] as char);
            if matches!(i, 3 | 5 | 7 | 9) {
                out.push('-');
            }
        }
        out
    }

    /// **Whether `init`'s mounts are known**: its manifest was read, named a mount, and every
    /// mount matched a device. Anything less, and a device taken for free could be `init`'s root,
    /// so the service mounts nothing, refuses an administrator's mount, and does not answer
    /// `InUse`.
    pub fn init_known(mounts: Option<&[InitMount]>) -> bool {
        mounts.is_some_and(|l| !l.is_empty() && l.iter().all(|m| m.device.is_some()))
    }

    /// Whether this is a **live boot**: `init`'s root is on a RAM disk.
    pub fn live_boot(mounts: &[InitMount], records: &[DeviceRecord]) -> bool {
        let Some(root) = mounts.iter().find(|m| m.mount_point == "/").and_then(|m| m.device) else {
            return false;
        };
        let Some(r) = records.iter().find(|r| r.id == root) else {
            return false;
        };
        let ram = |id: u32| records.iter().any(|p| p.id == id && p.kind() == DeviceKind::RamDisk);
        ram(r.id) || (r.parent != NO_PARENT && ram(r.parent))
    }
}

pub mod table {
    //! **`/svc/storage/info` is typed tables**, as `/dev/devices` is: `all.tsm` with a row per
    //! block device, then one `<name>.tsm` each, named `blk-<n>` for `/dev/blk/<n>` as
    //! `/dev/devices` names them.
    //!
    //! Columns: `name`, `kind`, `size`, `filesystem` (`ext4` or `fat`), `label` (the filesystem's
    //! own), `mounted` (where), `by` (`init` or `storage`), `mode` (`ro` or `rw`), `clean`
    //! (left cleanly unmounted), `removable` (a disk behind USB mass storage, or a partition of
    //! one: what a session may eject), and `note` (**why a filesystem found is not mounted**, in
    //! the words of the log line, [`note`](crate::mounts::note)). What a device does not have is
    //! `Null`; **`clean` is `Null` too for a filesystem mounted writable**, whose state says it is
    //! in use, not how it was left.

    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use libinittoml::manifest::Mode;
    use libkern::device::{DeviceKind, DeviceRecord};
    use libstream::wire::{Schema, StreamFlags, Table, TypeModifiers, TypeTag, Value};

    use crate::probe::Found;

    /// A block device, and what it holds.
    #[derive(Clone, Debug)]
    pub struct Device {
        /// Its registry record.
        pub record: DeviceRecord,
        /// What the service found on it.
        pub found: Found,
    }

    /// Who mounted a filesystem.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum By {
        /// `init`, from `init.toml`: reported, never mounted or unmounted here.
        Init,
        /// This service.
        Storage,
    }

    impl By {
        fn word(self) -> &'static str {
            match self {
                By::Init => "init",
                By::Storage => "storage",
            }
        }
    }

    /// A mounted filesystem.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Mounted {
        /// The registry id of the device it is on.
        pub device: u32,
        /// Where it is mounted.
        pub at: String,
        /// Who mounted it.
        pub by: By,
        /// `ro` or `rw`.
        pub mode: Mode,
    }

    /// A device's name: `/dev/devices`' own, `blk-<n>` for `/dev/blk/<n>`.
    pub fn name(r: &DeviceRecord) -> String {
        format!("blk-{}", r.served)
    }

    fn kind_word(kind: DeviceKind) -> &'static str {
        match kind {
            DeviceKind::Disk => "disk",
            DeviceKind::Partition => "partition",
            DeviceKind::RamDisk => "ramdisk",
            _ => "unknown",
        }
    }

    /// The columns every table here has.
    pub fn schema() -> Schema {
        let nullable = TypeModifiers::NULLABLE;
        Schema::new()
            .field("name", TypeTag::String, TypeModifiers::NONE)
            .field("kind", TypeTag::String, TypeModifiers::NONE)
            .field("description", TypeTag::String, nullable)
            .field("size", TypeTag::Int, nullable)
            .field("filesystem", TypeTag::String, nullable)
            .field("label", TypeTag::String, nullable)
            .field("mounted", TypeTag::String, nullable)
            .field("by", TypeTag::String, nullable)
            .field("mode", TypeTag::String, nullable)
            .field("clean", TypeTag::Bool, nullable)
            .field("removable", TypeTag::Bool, TypeModifiers::NONE)
            .field("note", TypeTag::String, nullable)
    }

    /// `d`'s row, mounted as `mount` says; `all` holds a partition's disk, for `removable`.
    pub fn row(d: &Device, mount: Option<&Mounted>, all: &[Device]) -> Vec<Value> {
        let r = &d.record;
        let text = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Str(String::from(s)));
        let size = (r.logical_block_size != 0)
            .then(|| Value::Int((r.logical_block_size as u64).saturating_mul(r.block_count) as i64));
        let clean = match (d.found.clean(), mount) {
            (_, Some(m)) if m.mode == Mode::Rw => Value::Null,
            (Some(c), _) => Value::Bool(c),
            (None, _) => Value::Null,
        };
        // What the device calls itself: a SATA disk's model and serial, a RAM disk's module, a
        // partition's name in its table, as `nxinstall` shows them.
        let described = String::from_utf8_lossy(r.name()).into_owned();
        vec![
            Value::Str(name(r)),
            Value::Str(String::from(kind_word(r.kind()))),
            if described.is_empty() { Value::Null } else { Value::Str(described) },
            size.unwrap_or(Value::Null),
            text(d.found.filesystem()),
            text(d.found.label()),
            text(mount.map(|m| m.at.as_str())),
            text(mount.map(|m| m.by.word())),
            text(mount.map(|m| if m.mode == Mode::Ro { "ro" } else { "rw" })),
            clean,
            Value::Bool(crate::mounts::removable(d, all)),
            text(crate::mounts::note(d, all, mount).as_deref()),
        ]
    }

    fn encode(rows: Vec<Vec<Value>>) -> Vec<u8> {
        let table = Table { flags: StreamFlags::NONE, schema: schema(), rows };
        let mut out = Vec::new();
        // A `Vec` sink cannot fail, and every row has the schema's shape by construction.
        let _ = table.encode(&mut out);
        out
    }

    fn mount_of<'a>(d: &Device, mounts: &'a [Mounted]) -> Option<&'a Mounted> {
        mounts.iter().find(|m| m.device == d.record.id)
    }

    /// `all.tsm`: every block device, a row each, in registry order.
    pub fn all(devices: &[Device], mounts: &[Mounted]) -> Vec<u8> {
        encode(devices.iter().map(|d| row(d, mount_of(d, mounts), devices)).collect())
    }

    /// `<name>.tsm`: the one device called `name`, if there is one.
    pub fn one(devices: &[Device], mounts: &[Mounted], name: &str) -> Option<Vec<u8>> {
        let d = devices.iter().find(|d| self::name(&d.record) == name)?;
        Some(encode(vec![row(d, mount_of(d, mounts), devices)]))
    }

    /// The directory's entries: `all.tsm`, then each device's, in registry order.
    pub fn entries(devices: &[Device]) -> Vec<String> {
        let mut out = vec![String::from("all.tsm")];
        out.extend(devices.iter().map(|d| format!("{}.tsm", name(&d.record))));
        out
    }
}

pub mod labels {
    //! **What a mount is called: `/storage/<label>`**, a name that outlasts `/dev/blk/<n>`'s
    //! discovery order. The filesystem's own label, else its partition's name, else `blk-<n>`,
    //! taking the first that is valid; a clash takes `-2`, `-3`, … in registry order.

    use alloc::format;
    use alloc::string::String;
    use libkern::device::DeviceKind;

    use crate::table::{Device, name};

    /// Longest label accepted, in bytes: more than any a disk carries (GPT 36, ext4 16, FAT 11).
    pub const MAX: usize = 64;

    /// Whether `s` may be a label: 1 to [`MAX`] bytes of printable ASCII, with no `/`, not
    /// beginning with `.` or a space and not ending with one. `.` and `..` are not path
    /// components and a hidden name is not one a person would find; a space at either end cannot
    /// be seen. Anything else, a non-ASCII label included, falls back to the next source.
    pub fn valid(s: &str) -> bool {
        let b = s.as_bytes();
        !b.is_empty()
            && b.len() <= MAX
            && b.iter().all(|&c| (0x20..0x7F).contains(&c) && c != b'/')
            && b[0] != b'.'
            && b[0] != b' '
            && b[b.len() - 1] != b' '
    }

    /// The label `d` would be mounted under, before clashes.
    pub fn preferred(d: &Device) -> String {
        if let Some(l) = d.found.label().filter(|l| valid(l)) {
            return String::from(l);
        }
        if d.record.kind() == DeviceKind::Partition
            && let Ok(n) = core::str::from_utf8(d.record.name())
            && valid(n)
        {
            return String::from(n);
        }
        name(&d.record)
    }

    /// `want`, or the first of `want-2`, `want-3`, … that `taken` does not hold.
    pub fn unique(want: &str, taken: &[String]) -> String {
        if !taken.iter().any(|t| t == want) {
            return String::from(want);
        }
        (2..).map(|k| format!("{want}-{k}")).find(|c| !taken.contains(c)).unwrap_or_default()
    }
}

pub mod mounts {
    //! **What this service mounts at boot**: every ext4 `init` did not mount, and **a FAT on a
    //! removable disk** (one behind USB mass storage). An internal disk's FAT is its ESP, most
    //! likely, which nobody asked to have mounted: an administrator's `disk --mount` takes one.
    //! Each mount spawns the server for its kind ([`Server`]).
    //!
    //! **A live boot mounts the machine's own disks read-only**: they are the install target, and
    //! nothing written by accident could be taken back. **A removable disk mounts writable on any
    //! boot**: a stick is the session's, which may eject it ([`eject`]). An administrator's mount
    //! is writable either way; it is the automatic one that has to be careful.
    //!
    //! A device `disk` has written is read again ([`reread`]) and mounted by an arrival's rules;
    //! why a filesystem is not mounted is said in one place, [`unmounted_why`], for the log line
    //! and the table's `note` alike.

    use alloc::string::String;
    use alloc::vec::Vec;
    use libinittoml::manifest::Mode;

    use crate::labels;
    use crate::probe::Found;
    use crate::table::{Device, Mounted};

    /// The driver a disk behind USB mass storage is published by (`kernel/src/drivers/xhci/
    /// storage.rs`): what makes a disk removable. A SATA disk never is, whatever its bay.
    pub const USB_STORAGE: &[u8] = b"usb-storage";

    /// The server that serves a filesystem, spawned from the store's copy: the root is mounted by
    /// now.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Server {
        /// `fs-server-ext4`.
        Ext4,
        /// `fs-server-fat`.
        Fat,
    }

    impl Server {
        /// Where it is spawned from.
        pub fn path(self) -> &'static [u8] {
            match self {
                Server::Ext4 => b"/bin/fs-server-ext4",
                Server::Fat => b"/bin/fs-server-fat",
            }
        }
    }

    /// One mount to make: the device, its label, its mode and its server.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Plan {
        /// The registry id of the device.
        pub device: u32,
        /// Its label, unique among the plan's.
        pub label: String,
        /// `ro` for an internal disk on a live boot, `rw` otherwise.
        pub mode: Mode,
        /// The server for what it holds.
        pub server: Server,
    }

    /// Why an administrator's mount was refused.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Refusal {
        /// No block device has that name.
        NoSuchDevice,
        /// It is mounted already, by `init` or by this service.
        AlreadyMounted,
        /// It holds no filesystem this service can serve.
        NothingToServe,
        /// The label asked for is not a valid one.
        BadLabel,
        /// The label asked for names another mount.
        LabelTaken,
        /// Every mount slot is in use.
        Full,
        /// `init`'s mounts are not all known ([`init_known`](crate::sources::init_known)), so the
        /// device could be `init`'s root.
        InitUnknown,
        /// It was ejected, with the rest of its disk.
        Ejected,
    }

    impl Refusal {
        /// The error a client is answered with.
        pub fn kerror(self) -> libkern::KError {
            use libkern::KError;
            match self {
                Refusal::NoSuchDevice => KError::NotFound,
                Refusal::AlreadyMounted | Refusal::LabelTaken => KError::AlreadyExists,
                Refusal::NothingToServe | Refusal::Ejected => KError::Unsupported,
                Refusal::BadLabel => KError::InvalidArgument,
                Refusal::Full => KError::WouldBlock,
                Refusal::InitUnknown => KError::NoAccess,
            }
        }

        /// The reason, as the refusal says it.
        pub fn why(self) -> &'static [u8] {
            match self {
                Refusal::NoSuchDevice => b"no block device has that name",
                Refusal::AlreadyMounted => b"it is already mounted",
                Refusal::NothingToServe => b"it holds no filesystem this service can serve",
                Refusal::BadLabel => b"that is not a valid label",
                Refusal::LabelTaken => b"another mount has that label",
                Refusal::Full => b"no more filesystems can be mounted at once",
                Refusal::InitUnknown => {
                    b"init's mounts are not all known, so this service mounts nothing: the device could be init's root"
                }
                Refusal::Ejected => b"it was ejected: plug it in again",
            }
        }
    }

    /// **An administrator's mount** of the device called `name` (`blk-<n>`), under `label`, or
    /// the label the service would choose if it is empty. `mounted` is everything mounted, `init`'s
    /// included; `taken` the labels in use; `room` whether a slot is free; `init_known` is
    /// [`init_known`](crate::sources::init_known)'s answer, and nothing is mounted without it.
    /// **Always writable**, on a live boot too: the auto-mount is the careful one.
    pub fn explicit(
        devices: &[Device],
        mounted: &[Mounted],
        taken: &[String],
        room: bool,
        init_known: bool,
        name: &str,
        label: &str,
    ) -> Result<Plan, Refusal> {
        if !init_known {
            return Err(Refusal::InitUnknown);
        }
        let d = devices.iter().find(|d| crate::table::name(&d.record) == name).ok_or(Refusal::NoSuchDevice)?;
        if mounted.iter().any(|m| m.device == d.record.id) {
            return Err(Refusal::AlreadyMounted);
        }
        if d.found == Found::Ejected {
            return Err(Refusal::Ejected);
        }
        let server = d.found.server().ok_or(Refusal::NothingToServe)?;
        let label = if label.is_empty() {
            labels::unique(&labels::preferred(d), taken)
        } else if !labels::valid(label) {
            return Err(Refusal::BadLabel);
        } else if taken.iter().any(|t| t == label) {
            return Err(Refusal::LabelTaken);
        } else {
            String::from(label)
        };
        if !room {
            return Err(Refusal::Full);
        }
        Ok(Plan { device: d.record.id, label, mode: Mode::Rw, server })
    }

    /// **The devices in use, which must not be granted raw**: every mounted filesystem's device,
    /// `init`'s included, and the disk that holds it, since a raw write to a disk reaches its
    /// partitions; registry ids, ascending. **The holder is the parent only if it is a block
    /// device**: a whole disk's parent is its controller's PCI function, not grantable. **And the
    /// disk the machine started from**, mounted or not: it holds the running system.
    pub fn in_use(devices: &[Device], mounted: &[Mounted]) -> Vec<u32> {
        use libkern::device::NO_PARENT;
        let mut ids: Vec<u32> = devices.iter().filter(|d| d.record.is_boot_medium()).map(|d| d.record.id).collect();
        for m in mounted {
            ids.push(m.device);
            let parent = devices.iter().find(|d| d.record.id == m.device).map(|d| d.record.parent);
            if let Some(p) = parent.filter(|&p| p != NO_PARENT)
                && devices.iter().any(|d| d.record.id == p)
            {
                ids.push(p);
            }
        }
        ids.sort_unstable();
        ids.dedup();
        ids
    }

    /// Where a label is mounted.
    pub fn at(label: &str) -> String {
        let mut s = String::from("/storage/");
        s.push_str(label);
        s
    }

    /// The boot's auto-mounts, in registry order; `already` is what is mounted, `init`'s among it.
    /// **None at all without `init_known`**: a device whose `init` mount went unmatched would look
    /// free here, and could be the running root.
    ///
    /// **Nor the installer's source**: a partition named [`libgpt::INSTALL_SOURCE_LABEL`], which
    /// `nxinstall` copies and the `disks` grant would withhold once it was mounted. **The rule is
    /// the name, not "a RAM disk"**: a test image's scratch filesystem is a RAM disk this service
    /// mounts, for `boot-probe`. **Nor anything on the disk the machine started from**
    /// ([`on_boot_medium`]): on a live boot, the stick the system runs from.
    pub fn automount(devices: &[Device], already: &[Mounted], live: bool, init_known: bool) -> Vec<Plan> {
        plan(devices, devices, already, &[], live, init_known)
    }

    /// **The mounts devices that arrived after the boot get**: `new`, by the boot's rules, among
    /// `all` the service knows, with `taken` the labels in use. Only the new are planned: one an
    /// administrator unmounted stays so.
    pub fn arrival(
        new: &[Device],
        all: &[Device],
        already: &[Mounted],
        taken: &[String],
        live: bool,
        init_known: bool,
    ) -> Vec<Plan> {
        plan(new, all, already, taken, live, init_known)
    }

    fn plan(
        candidates: &[Device],
        all: &[Device],
        already: &[Mounted],
        taken: &[String],
        live: bool,
        init_known: bool,
    ) -> Vec<Plan> {
        if !init_known {
            return Vec::new();
        }
        let mut taken: Vec<String> = taken.to_vec();
        let mut plan = Vec::new();
        for d in candidates {
            let Some(server) = d.found.server() else {
                continue;
            };
            if server == Server::Fat && !removable(d, all) {
                continue;
            }
            if already.iter().any(|m| m.device == d.record.id) || is_install_source(d) || on_boot_medium(d, all) {
                continue;
            }
            let label = labels::unique(&labels::preferred(d), &taken);
            taken.push(label.clone());
            let mode = if live && !removable(d, all) { Mode::Ro } else { Mode::Rw };
            plan.push(Plan { device: d.record.id, label, mode, server });
        }
        plan
    }

    /// **Why a session's eject was refused.**
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum EjectRefusal {
        /// Nothing is mounted under that name.
        NotMounted,
        /// What is mounted there is not on a removable disk.
        NotRemovable,
        /// It is on the disk the system runs from, which an eject would stop whole.
        SystemDisk,
    }

    impl EjectRefusal {
        /// The error a client is answered with.
        pub fn kerror(self) -> libkern::KError {
            match self {
                EjectRefusal::NotMounted => libkern::KError::NotFound,
                EjectRefusal::NotRemovable | EjectRefusal::SystemDisk => libkern::KError::NoAccess,
            }
        }

        /// The reason, as the refusal says it.
        pub fn why(self) -> &'static [u8] {
            match self {
                EjectRefusal::NotMounted => b"nothing is mounted under that name",
                EjectRefusal::NotRemovable => {
                    b"it is not removable: an internal disk is unmounted with the storage grant (with admin disk --unmount)"
                }
                EjectRefusal::SystemDisk => {
                    b"the system runs from its disk: the machine started from it, or init mounted a filesystem on it"
                }
            }
        }
    }

    /// **A session's eject of the drive holding the mount named `name`**: the disk, to be stopped,
    /// and every mount this service made on it, in the order made, `name` among them, if
    /// `/storage/<name>` is on a removable disk the system does not run from. **The drive, not the one filesystem**: a stick is pulled whole, so
    /// unmounting one partition would leave the others mounted, their unwritten files lost.
    /// **The name is the mount's** (the table's `mounted` column), never the filesystem's own
    /// label, which two sticks can share and a stick can lack. An internal disk's mount is the
    /// `storage` grant's to unmount; `init`'s mounts are not this service's.
    pub fn eject(devices: &[Device], mounted: &[Mounted], name: &str) -> Result<Ejection, EjectRefusal> {
        use crate::table::By;
        let want = at(name);
        let m = mounted.iter().find(|m| m.by == By::Storage && m.at == want).ok_or(EjectRefusal::NotMounted)?;
        let d = devices.iter().find(|d| d.record.id == m.device).ok_or(EjectRefusal::NotMounted)?;
        if !removable(d, devices) {
            return Err(EjectRefusal::NotRemovable);
        }
        // The disk: the device itself when a filesystem fills it, else the partition's parent.
        let partition = d.record.kind() == libkern::device::DeviceKind::Partition;
        let disk = if partition { d.record.parent } else { d.record.id };
        let on_disk = |id: u32| devices.iter().any(|x| x.record.id == id && (id == disk || x.record.parent == disk));
        if on_boot_medium(d, devices) || mounted.iter().any(|m| m.by == By::Init && on_disk(m.device)) {
            return Err(EjectRefusal::SystemDisk);
        }
        let names = mounted
            .iter()
            .filter(|m| m.by == By::Storage && on_disk(m.device))
            .filter_map(|m| m.at.strip_prefix("/storage/").map(String::from))
            .collect();
        Ok(Ejection { disk, names })
    }

    /// **Mark `disk` and its partitions ejected**, to be read no more: any read wakes a stopped stick.
    pub fn mark_ejected(devices: &mut [Device], disk: u32) {
        for d in devices.iter_mut().filter(|d| d.record.id == disk || d.record.parent == disk) {
            d.found = Found::Ejected;
        }
    }

    /// Whether device `id` is marked ejected.
    pub fn ejected(devices: &[Device], id: u32) -> bool {
        devices.iter().any(|d| d.record.id == id && d.found == Found::Ejected)
    }

    /// How a stick's stop went, in the eject line's words.
    pub fn stop_said(stopped: Result<(), libkern::KError>) -> &'static str {
        use libkern::KError;
        match stopped {
            Ok(()) => "stopped",
            Err(KError::Unsupported) => "did not stop: it cannot be stopped",
            Err(KError::IoError) => "did not stop: it failed the command",
            Err(KError::PeerClosed) => "did not stop: it went away",
            Err(_) => "did not stop: the kernel refused it",
        }
    }

    /// What an eject takes: the disk, by registry id, and the mounts to unmount first, by name.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub struct Ejection {
        /// The disk the medium is stopped on, once nothing on it is mounted.
        pub disk: u32,
        /// Every mount this service made on it, in the order made.
        pub names: Vec<String>,
    }

    /// **What a `Reread` reads again**, by registry id: a partition, probed again, or a disk, which
    /// the kernel rescans first.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Reread {
        /// A partition: its window is unchanged, so what is in it is read again.
        Partition(u32),
        /// A disk, a RAM disk among them: its table is read again by the kernel.
        Disk(u32),
    }

    /// **Why a `Reread` was refused.**
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum RereadRefusal {
        /// `init`'s mounts are not all known, so nothing is mounted.
        InitUnknown,
        /// No block device has that name.
        NoSuchDevice,
        /// The partition is mounted, or something on the disk is.
        Mounted,
        /// It is on the disk the machine started from.
        BootDisk,
        /// The kernel cannot rescan the disk: the binary's, from the kernel's `Unsupported`.
        CannotRescan,
        /// It was ejected, with the rest of its disk.
        Ejected,
    }

    impl RereadRefusal {
        /// The error a client is answered with.
        pub fn kerror(self) -> libkern::KError {
            use libkern::KError;
            match self {
                RereadRefusal::InitUnknown | RereadRefusal::BootDisk => KError::NoAccess,
                RereadRefusal::NoSuchDevice => KError::NotFound,
                RereadRefusal::Mounted => KError::WouldBlock,
                RereadRefusal::CannotRescan | RereadRefusal::Ejected => KError::Unsupported,
            }
        }

        /// The reason, as the refusal says it.
        pub fn why(self) -> &'static [u8] {
            match self {
                RereadRefusal::InitUnknown => Refusal::InitUnknown.why(),
                RereadRefusal::NoSuchDevice => Refusal::NoSuchDevice.why(),
                RereadRefusal::Mounted => b"it is mounted, or something on its disk is: unmount it first",
                RereadRefusal::BootDisk => b"it is on the disk the machine started from",
                RereadRefusal::CannotRescan => {
                    b"its partitions cannot be read again: only a USB disk's can, and an internal disk is partitioned by nxinstall"
                }
                RereadRefusal::Ejected => Refusal::Ejected.why(),
            }
        }
    }

    /// **An administrator's `Reread` of the device called `name`**, once `disk` has written it.
    /// **Refused while mounted**: a partition while it is, a disk while anything on it is, since
    /// its partitions are about to be replaced; a partition beside a mounted sibling is read
    /// again, as the `disks` grant gave it. **And anything on the disk the machine started from**;
    /// mounted is asked first, so a boot disk holding `init`'s root says it is mounted.
    pub fn reread(
        devices: &[Device],
        mounted: &[Mounted],
        init_known: bool,
        name: &str,
    ) -> Result<Reread, RereadRefusal> {
        use libkern::device::DeviceKind;
        if !init_known {
            return Err(RereadRefusal::InitUnknown);
        }
        let d = devices.iter().find(|d| crate::table::name(&d.record) == name).ok_or(RereadRefusal::NoSuchDevice)?;
        let id = d.record.id;
        let partition = d.record.kind() == DeviceKind::Partition;
        let on_it = |m: &Mounted| {
            m.device == id || (!partition && devices.iter().any(|x| x.record.id == m.device && x.record.parent == id))
        };
        if mounted.iter().any(on_it) {
            return Err(RereadRefusal::Mounted);
        }
        if d.found == Found::Ejected {
            return Err(RereadRefusal::Ejected);
        }
        if on_boot_medium(d, devices) {
            return Err(RereadRefusal::BootDisk);
        }
        Ok(if partition { Reread::Partition(id) } else { Reread::Disk(id) })
    }

    /// **Why `d`, not mounted, is not**, in the log line's words, or `None` if nothing mounted it:
    /// a FAT its server refuses, the installer's source, anything on the disk the machine started
    /// from, a FAT on an internal disk — in that order. QEMU gates match these words.
    pub fn unmounted_why(d: &Device, all: &[Device]) -> Option<String> {
        if let Found::Fat { refused: Some(why), .. } = &d.found {
            return Some(alloc::format!("not served: {why}"));
        }
        let why = if is_install_source(d) {
            "the installer's source, left unmounted"
        } else if on_boot_medium(d, all) {
            "on the disk the machine started from, passed over"
        } else if d.found.server() == Some(Server::Fat) && !removable(d, all) {
            "not removable, so not mounted"
        } else {
            return None;
        };
        Some(String::from(why))
    }

    /// **The table's `note`**: [`unmounted_why`] for a filesystem found and not mounted; `None` for
    /// a mounted one, and for a device holding nothing, whose row says so already.
    pub fn note(d: &Device, all: &[Device], mount: Option<&Mounted>) -> Option<String> {
        if d.found == Found::Ejected {
            return Some(String::from("ejected; remove it"));
        }
        if mount.is_some() || d.found.filesystem().is_none() {
            return None;
        }
        unmounted_why(d, all)
    }

    /// Whether `d` is a disk behind USB mass storage ([`USB_STORAGE`]), or one of its partitions.
    pub fn removable(d: &Device, all: &[Device]) -> bool {
        let usb = |r: &libkern::device::DeviceRecord| r.driver() == USB_STORAGE;
        usb(&d.record) || all.iter().any(|p| p.record.id == d.record.parent && usb(&p.record))
    }

    /// Whether `d` is the disk the machine started from, flagged in its record, or one of its
    /// partitions.
    pub fn on_boot_medium(d: &Device, all: &[Device]) -> bool {
        d.record.is_boot_medium() || all.iter().any(|p| p.record.id == d.record.parent && p.record.is_boot_medium())
    }

    /// Whether `d` is the installer's pristine source: a partition named
    /// [`libgpt::INSTALL_SOURCE_LABEL`], the GPT name the build wrote and `nxinstall` looks for.
    pub fn is_install_source(d: &Device) -> bool {
        d.record.kind() == libkern::device::DeviceKind::Partition
            && d.record.name() == libgpt::INSTALL_SOURCE_LABEL.as_bytes()
    }
}

pub mod suffix {
    //! What a resolve that reached the service asked for, and what it may ask for where it came
    //! from. **A session reaches the service through an endpoint of its own**, which the login
    //! supervisors bind at `/storage` with the base `/fs` and at `/dev/storage` with the base
    //! `/info`. A resolve arriving there is [`session_only`]. The base alone would not be
    //! enough: a holder with `BIND_NAMESPACE` could bind the endpoint with no base, so an endpoint
    //! that cannot mint is a capability it can be handed, as `/dev/devices`' is.

    /// A forwarded suffix, classified.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Asked<'a> {
        /// `info`: the directory of tables.
        Directory,
        /// `info/<name>.tsm`: one table, `all` or a device's.
        File(&'a str),
        /// `fs`: the directory of mounted filesystems, a subdirectory per label.
        Mounts,
        /// `fs/<label>`, alone or with more after it: a resolve to continue in that mount's
        /// namespace. `consumed` is how many bytes of the suffix `fs/<label>` is — what the
        /// `SUBNAMESPACE` reply answers for.
        Mount {
            /// The label.
            label: &'a str,
            /// The bytes the reply answers for.
            consumed: usize,
        },
        /// `session-endpoint`: a forwarding endpoint of the service's own, every resolve on which
        /// is [`session_only`].
        SessionEndpoint,
        /// `admin-endpoint`: a forwarding endpoint of the service's own, on which any resolve is
        /// answered with an admin session — a channel for `Storage` requests.
        AdminEndpoint,
        /// `info/media`: a media session, carrying `Eject`.
        Media,
        /// `info/watch`: a watch, on which the service sends a ping per change in the mounts.
        Watch,
        /// Anything else.
        Unknown,
    }

    /// Classify a suffix.
    pub fn parse(suffix: &[u8]) -> Asked<'_> {
        if suffix == b"info" {
            return Asked::Directory;
        }
        if suffix == b"fs" {
            return Asked::Mounts;
        }
        if suffix == b"session-endpoint" {
            return Asked::SessionEndpoint;
        }
        if suffix == b"admin-endpoint" {
            return Asked::AdminEndpoint;
        }
        if suffix == b"info/media" {
            return Asked::Media;
        }
        if suffix == b"info/watch" {
            return Asked::Watch;
        }
        if let Some(file) = suffix.strip_prefix(b"info/") {
            return match file.strip_suffix(b".tsm").map(core::str::from_utf8) {
                Some(Ok(name)) if !name.is_empty() && !name.contains('/') => Asked::File(name),
                _ => Asked::Unknown,
            };
        }
        if let Some(rest) = suffix.strip_prefix(b"fs/") {
            let end = rest.iter().position(|&c| c == b'/').unwrap_or(rest.len());
            return match core::str::from_utf8(&rest[..end]) {
                Ok(label) if !label.is_empty() => Asked::Mount { label, consumed: 3 + end },
                _ => Asked::Unknown,
            };
        }
        Asked::Unknown
    }

    /// What a resolve arriving on a session endpoint gets: the tables, the filesystems, a media
    /// session and a watch as asked, and **any endpoint answered as if it did not exist**: another
    /// session endpoint, whose holder could then mint more, and above all the admin endpoint,
    /// which mounts and unmounts. A media session only ejects, which a session may do.
    pub fn session_only(asked: Asked<'_>) -> Asked<'_> {
        match asked {
            Asked::SessionEndpoint | Asked::AdminEndpoint => Asked::Unknown,
            other => other,
        }
    }
}

pub mod watch {
    //! **The sessions following the mounts**: each a channel the service holds one end of and
    //! **only sends on** — a bare `Changed` per change — so none takes a slot in the service's wait
    //! set. **A full queue is a ping already waiting**: a watcher that falls behind loses no
    //! change. **A watcher that has gone is found by the ping that fails**, `PeerClosed`, and
    //! dropped then; when the list is full, every watcher is pinged first, which costs the living
    //! a needless read of the table and frees the dead. A ping is the caller's, as a [`Sent`] it
    //! reports, so every rule is a host test.

    use alloc::vec::Vec;

    /// How many watches can be held: the machine's, since the service cannot tell sessions apart;
    /// the shell starts a Files per Places pick and per launch, each holding one.
    pub const MAX_WATCHES: usize = 32;

    /// What a ping did.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Sent {
        /// It is queued.
        Queued,
        /// The queue is full — of pings, since a watch carries nothing else.
        Full,
        /// The client has gone.
        Gone,
    }

    /// The watches held: this service's ends of them.
    #[derive(Default, Debug)]
    pub struct Watches {
        ends: Vec<u64>,
    }

    impl Watches {
        /// None held.
        pub fn new() -> Watches {
            Watches { ends: Vec::new() }
        }

        /// The ends held, in the order they were taken.
        pub fn ends(&self) -> &[u64] {
            &self.ends
        }

        /// **Ping every watcher**, `ping` sending to each end, and let go of the ones that have
        /// gone — returned, for the caller to close.
        pub fn changed(&mut self, mut ping: impl FnMut(u64) -> Sent) -> Vec<u64> {
            let mut gone = Vec::new();
            self.ends.retain(|&e| match ping(e) {
                Sent::Gone => {
                    gone.push(e);
                    false
                }
                Sent::Queued | Sent::Full => true,
            });
            gone
        }

        /// **Take a new watch's end.** With the list full, every watcher is pinged first, freeing
        /// the gone; `Err` if it is full still, and the caller refuses the watch. The ends of the
        /// gone, for the caller to close.
        #[allow(clippy::result_unit_err, reason = "the only error is a watch list that is still full")]
        pub fn add(&mut self, end: u64, ping: impl FnMut(u64) -> Sent) -> Result<Vec<u64>, ()> {
            let gone = if self.ends.len() >= MAX_WATCHES { self.changed(ping) } else { Vec::new() };
            if self.ends.len() >= MAX_WATCHES {
                return Err(());
            }
            self.ends.push(end);
            Ok(gone)
        }
    }
}

#[cfg(test)]
mod eject_tests;
#[cfg(test)]
mod tests;
