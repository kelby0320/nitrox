//! The storage service's decisions (administration Part C.5): everything that can be wrong about a
//! disk without a boot to show it.
//!
//! - [`probe`] — what a device holds: ext4, read the way its server reads it; FAT, recognised; or
//!   nothing;
//! - [`sources`] — which devices `init` mounted, from `init.toml`, and whether this is a live boot;
//! - [`table`] — the TSM1 tables `/svc/storage/info` serves;
//! - [`suffix`] — what a resolve that reached the service asked for.
//!
//! The binary is the event loop: taking `block` from the device manager, reading each device, and
//! serving.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod probe {
    //! **What a device holds**, read the way the server that would serve it reads it.
    //!
    //! - An **ext4** filesystem, if `fs-server-ext4`'s own `check_device` accepts it. That is the
    //!   check its server runs before it says Ready, so the service never offers to mount what the
    //!   server would refuse. Its label and whether it was left clean come with it.
    //! - A **FAT** filesystem, recognised from its boot sector and reported, never mounted: there is
    //!   no `fs-server-fat` until Phase 6.
    //! - **Nothing** either recognises: a disk holding a partition table, a blank one, or a
    //!   filesystem neither can read.

    use alloc::string::String;
    use fs_server_ext4::BlockReader;
    use fs_server_ext4::ext4::{check_device, volume_label, was_left_clean};

    /// What a device holds.
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub enum Found {
        /// An ext4 filesystem `fs-server-ext4` can serve.
        Ext4 {
            /// Its own label, empty when it has none.
            label: String,
            /// Whether it was left cleanly unmounted; `None` if the state would not read.
            clean: Option<bool>,
        },
        /// A FAT filesystem.
        Fat {
            /// Its own label, empty when it has none.
            label: String,
        },
        /// Nothing this service recognises.
        Nothing,
    }

    impl Found {
        /// The filesystem, as the table names it.
        pub fn filesystem(&self) -> Option<&'static str> {
            match self {
                Found::Ext4 { .. } => Some("ext4"),
                Found::Fat { .. } => Some("fat"),
                Found::Nothing => None,
            }
        }

        /// The filesystem's own label, if it has one.
        pub fn label(&self) -> Option<&str> {
            match self {
                Found::Ext4 { label, .. } | Found::Fat { label } if !label.is_empty() => Some(label),
                _ => None,
            }
        }
    }

    /// What `r` holds.
    pub fn probe<R: BlockReader>(r: &R) -> Found {
        if check_device(r).is_ok() {
            let mut raw = [0u8; 16];
            let n = volume_label(r, &mut raw).unwrap_or(0);
            let label = String::from_utf8_lossy(&raw[..n]).into_owned();
            return Found::Ext4 { label, clean: was_left_clean(r).ok() };
        }
        let mut sector = [0u8; 512];
        if r.read_at(0, &mut sector).is_ok()
            && let Some(label) = fat_label(&sector)
        {
            return Found::Fat { label };
        }
        Found::Nothing
    }

    /// **A FAT boot sector's volume label**, or `None` if `sector` is not a FAT boot sector.
    ///
    /// Recognised by four things every FAT formatter writes: the `0x55 0xAA` signature, a
    /// power-of-two sector size from 512 to 4096, the extended boot signature `0x29`, and the
    /// filesystem-type string after it — `FAT32` at `0x52`, or `FAT12`, `FAT16` or `FAT` at `0x36`.
    /// The specification calls the type string informational, but every formatter writes it. A
    /// protective MBR, which is what a GPT disk holds at sector 0, has the same signature and none
    /// of the other three. `NO NAME` is FAT's word for no label.
    pub fn fat_label(sector: &[u8; 512]) -> Option<String> {
        if sector[510] != 0x55 || sector[511] != 0xAA {
            return None;
        }
        if !matches!(u16::from_le_bytes([sector[11], sector[12]]), 512 | 1024 | 2048 | 4096) {
            return None;
        }
        let label_at = if sector[0x42] == 0x29 && &sector[0x52..0x57] == b"FAT32" {
            0x47
        } else if sector[0x26] == 0x29 && &sector[0x36..0x39] == b"FAT" {
            0x2B
        } else {
            return None;
        };
        let raw = &sector[label_at..label_at + 11];
        let end = raw.iter().rposition(|&b| b != b' ' && b != 0).map_or(0, |i| i + 1);
        let label = String::from_utf8_lossy(&raw[..end]).into_owned();
        Some(if label == "NO NAME" { String::new() } else { label })
    }
}

pub mod sources {
    //! **Which devices `init` mounted, and whether this is a live boot.**
    //!
    //! `init` records its mounts as namespace bindings, and nothing names the device behind one.
    //! What does is `init.toml`: every `[[mount]]` is critical-path, so on a running system each
    //! one succeeded, and its source names a partition by one of the two schemes `init` accepts.
    //! - `gpt-partlabel:<label>`: the first partition record whose name is the label, as the
    //!   kernel binds `/dev/disk/by-partlabel/<label>` to the first partition published with it.
    //! - `gpt-partuuid:<uuid>`: a partition's unique GUID, which the registry does not carry, so it
    //!   is read from the parent disk's own table. The kernel publishes a disk's partitions in its
    //!   table's order, skipping unused entries, so the k-th entry in use is the k-th partition
    //!   record under that disk. Its name and size confirm it, and it is unmatched if they differ.
    //!
    //! **A live boot is one whose root is on a RAM disk**: the root's partition belongs to one, or
    //! the root is one. That is what makes the machine's own disks the install target, so the
    //! service auto-mounts read-only there.

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
    //! block device, then one `<name>.tsm` each. A name is `/dev/devices`' own, `blk-<n>` for
    //! `/dev/blk/<n>`, so a row here and a row there are the same device.
    //!
    //! Columns: `name`, `kind`, `size`, `filesystem` (`ext4` or `fat`), `label` (the filesystem's
    //! own), `mounted` (where), `by` (`init` or `storage`), `mode` (`ro` or `rw`) and `clean`,
    //! whether the filesystem was left cleanly unmounted. **`clean` is `Null` for anything but
    //! ext4, and for a filesystem mounted writable**: that one's state says it is in use, because
    //! it is, and says nothing about how it was left. What a device does not have is `Null`.

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
            .field("size", TypeTag::Int, nullable)
            .field("filesystem", TypeTag::String, nullable)
            .field("label", TypeTag::String, nullable)
            .field("mounted", TypeTag::String, nullable)
            .field("by", TypeTag::String, nullable)
            .field("mode", TypeTag::String, nullable)
            .field("clean", TypeTag::Bool, nullable)
    }

    /// `d`'s row, mounted as `mount` says.
    pub fn row(d: &Device, mount: Option<&Mounted>) -> Vec<Value> {
        let r = &d.record;
        let text = |s: Option<&str>| s.map_or(Value::Null, |s| Value::Str(String::from(s)));
        let size = (r.logical_block_size != 0)
            .then(|| Value::Int((r.logical_block_size as u64).saturating_mul(r.block_count) as i64));
        let clean = match (&d.found, mount) {
            (_, Some(m)) if m.mode == Mode::Rw => Value::Null,
            (Found::Ext4 { clean: Some(c), .. }, _) => Value::Bool(*c),
            _ => Value::Null,
        };
        vec![
            Value::Str(name(r)),
            Value::Str(String::from(kind_word(r.kind()))),
            size.unwrap_or(Value::Null),
            text(d.found.filesystem()),
            text(d.found.label()),
            text(mount.map(|m| m.at.as_str())),
            text(mount.map(|m| m.by.word())),
            text(mount.map(|m| if m.mode == Mode::Ro { "ro" } else { "rw" })),
            clean,
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
        encode(devices.iter().map(|d| row(d, mount_of(d, mounts))).collect())
    }

    /// `<name>.tsm`: the one device called `name`, if there is one.
    pub fn one(devices: &[Device], mounts: &[Mounted], name: &str) -> Option<Vec<u8>> {
        let d = devices.iter().find(|d| self::name(&d.record) == name)?;
        Some(encode(vec![row(d, mount_of(d, mounts))]))
    }

    /// The directory's entries: `all.tsm`, then each device's, in registry order.
    pub fn entries(devices: &[Device]) -> Vec<String> {
        let mut out = vec![String::from("all.tsm")];
        out.extend(devices.iter().map(|d| format!("{}.tsm", name(&d.record))));
        out
    }
}

pub mod suffix {
    //! What a resolve that reached the service asked for.

    /// A forwarded suffix, classified.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Asked<'a> {
        /// `info`: the directory of tables.
        Directory,
        /// `info/<name>.tsm`: one table, `all` or a device's.
        File(&'a str),
        /// Anything else.
        Unknown,
    }

    /// Classify a suffix.
    pub fn parse(suffix: &[u8]) -> Asked<'_> {
        if suffix == b"info" {
            return Asked::Directory;
        }
        if let Some(file) = suffix.strip_prefix(b"info/") {
            return match file.strip_suffix(b".tsm").map(core::str::from_utf8) {
                Some(Ok(name)) if !name.is_empty() && !name.contains('/') => Asked::File(name),
                _ => Asked::Unknown,
            };
        }
        Asked::Unknown
    }
}

#[cfg(test)]
mod tests;
