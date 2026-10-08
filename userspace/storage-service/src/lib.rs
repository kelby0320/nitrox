//! The storage service's decisions (administration Part C.5): everything that can be wrong about a
//! disk without a boot to show it.
//!
//! - [`probe`] — what a device holds: ext4 or FAT, each read the way its server reads it; or
//!   nothing;
//! - [`sources`] — which devices `init` mounted, from `init.toml`, and whether this is a live boot;
//! - [`table`] — the TSM1 tables `/svc/storage/info` serves;
//! - [`labels`] — what a mount is called under `/storage`;
//! - [`mounts`] — what is mounted at boot, and how;
//! - [`suffix`] — what a resolve that reached the service asked for, and where it may ask it;
//! - [`watch`] — the sessions following the mounts, and who is told when they change.
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
    //! - A **FAT** filesystem (Phase 6 Part E.5), read by `fs-server-fat`'s own check, with its
    //!   label, how it was left, and — if its server would refuse it — why. A boot sector the
    //!   library cannot parse is still a FAT if [`fat_label`] recognises it, so a FAT with 4 KiB
    //!   sectors is reported as one that is not served rather than as nothing.
    //! - **Nothing** either recognises: a disk holding a partition table, a blank one, or a
    //!   filesystem neither can read.

    use alloc::format;
    use alloc::string::String;
    use fs_server_ext4::BlockReader;
    use fs_server_ext4::ext4::{check_device, volume_label, was_left_clean};
    use fs_server_fat::Fat;

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
            /// Whether it was left cleanly unmounted; `None` if the state would not read.
            clean: Option<bool>,
            /// **Why `fs-server-fat` would refuse it**, in the words its refusal would carry —
            /// `512-byte clusters, smaller than a page` — or `None` if it would serve it.
            refused: Option<String>,
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
                Found::Ext4 { label, .. } | Found::Fat { label, .. } if !label.is_empty() => Some(label),
                _ => None,
            }
        }

        /// Whether the filesystem was left cleanly unmounted, if it says.
        pub fn clean(&self) -> Option<bool> {
            match self {
                Found::Ext4 { clean, .. } | Found::Fat { clean, .. } => *clean,
                Found::Nothing => None,
            }
        }

        /// **The server that would serve it**, or `None` for nothing, or a FAT its server would
        /// refuse.
        pub fn server(&self) -> Option<crate::mounts::Server> {
            use crate::mounts::Server;
            match self {
                Found::Ext4 { .. } => Some(Server::Ext4),
                Found::Fat { refused: None, .. } => Some(Server::Fat),
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
        let recognised = r.read_at(0, &mut sector).ok().and_then(|()| fat_label(&sector));
        let fat = Fat::new(r);
        if fat.geometry().is_err() && recognised.is_none() {
            return Found::Nothing;
        }
        let label = recognised.unwrap_or_else(|| String::from_utf8_lossy(fat.label()).into_owned());
        let refused = fat.check().err().map(|why| format!("{why}"));
        Found::Fat { label, clean: fat.was_left_clean().ok(), refused }
    }

    /// **What a device holds, by its record** (PR #365 review, finding 4): [`probe`], and for a
    /// **whole disk whose first sector carries a partition entry**, never a filesystem of its own.
    /// Partitioning a stick that held a filesystem whole leaves that filesystem's bytes: `sfdisk`
    /// and `parted` write the entries into sector 0 and keep the rest. An ext4's superblock at byte
    /// 1024 then reads as ever, beside the partitions the kernel publishes, and the disk would be
    /// mounted whole over them — so such a disk holds nothing. A FAT's boot sector is sector 0
    /// itself, which the kernel reads as no table (`kernel/src/drivers/partitions.rs`), so no
    /// partition is published; mounted, the stale FAT would allocate clusters inside the partition
    /// nothing can see. It is reported, and refused.
    pub fn probe_record<R: BlockReader>(r: &R, record: &libkern::device::DeviceRecord) -> Found {
        use libkern::device::DeviceKind;
        let found = probe(r);
        let whole = matches!(record.kind(), DeviceKind::Disk | DeviceKind::RamDisk);
        let mut sector = [0u8; 512];
        if !whole || r.read_at(0, &mut sector).is_err() || !partition_entries(&sector, record.block_count) {
            return found;
        }
        match found {
            Found::Fat { label, clean, .. } => Found::Fat { label, clean, refused: Some(String::from(STALE_FAT)) },
            _ => Found::Nothing,
        }
    }

    /// Why a whole disk's FAT beside partition entries is not served.
    pub const STALE_FAT: &str = "its first sector holds partition entries too, so this FAT may be stale";

    /// **Whether a disk's first sector carries a partition entry**: signed `0x55AA`, every entry's
    /// status `0x00` or `0x80` — the check Linux makes, which a filesystem's boot code rarely
    /// passes — and one entry in use that lies on the disk. GPT's protective entry counts.
    pub fn partition_entries(sector: &[u8; 512], blocks: u64) -> bool {
        if sector[510] != 0x55 || sector[511] != 0xAA {
            return false;
        }
        let entry = |slot: usize| &sector[0x1BE + 16 * slot..0x1BE + 16 * (slot + 1)];
        if !(0..4).all(|slot| matches!(entry(slot)[0], 0x00 | 0x80)) {
            return false;
        }
        (0..4).any(|slot| {
            let e = entry(slot);
            let first = u32::from_le_bytes([e[8], e[9], e[10], e[11]]) as u64;
            let count = u32::from_le_bytes([e[12], e[13], e[14], e[15]]) as u64;
            e[4] != 0 && first != 0 && count != 0 && first.checked_add(count).is_some_and(|end| end <= blocks)
        })
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
    //! service auto-mounts an internal disk read-only there (a removable one writable, since
    //! Phase 6 Part F).

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

    /// **Whether `init`'s mounts are known**: its manifest was read (`Some`), named a mount, and
    /// every mount it named matched a device. Anything less, and a device this service took for
    /// free could be `init`'s root, so it mounts nothing, refuses an administrator's mount, and
    /// does not answer `InUse` (PR #336 review, finding 2).
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
    //! block device, then one `<name>.tsm` each. A name is `/dev/devices`' own, `blk-<n>` for
    //! `/dev/blk/<n>`, so a row here and a row there are the same device.
    //!
    //! Columns: `name`, `kind`, `size`, `filesystem` (`ext4` or `fat`), `label` (the filesystem's
    //! own), `mounted` (where), `by` (`init` or `storage`), `mode` (`ro` or `rw`), `clean`,
    //! whether the filesystem was left cleanly unmounted. **`clean` is `Null` for anything but
    //! ext4 and FAT, and for a filesystem mounted writable**: that one's state says it is in use,
    //! because it is, and says nothing about how it was left. What a device does not have is
    //! `Null`. And `removable` (Phase 6 Part F): whether the device is a disk behind USB mass
    //! storage, or a partition of one — what a session may eject, and what Files puts an eject
    //! button on. And `note` (Phase 6 Part G): **why a filesystem found is not mounted**, in the
    //! words the service's log line gives ([`note`](crate::mounts::note)).

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

    /// `d`'s row, mounted as `mount` says, among `all` the devices — which a partition's disk is
    /// one of, to say whether it is removable.
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
        // **What the device calls itself** (the laptop polish's Part C): the model and serial a
        // SATA disk reports, a RAM disk's module, a partition's name in its table — what `nxinstall`
        // shows, so `disk --list` names a disk the way the installer does. `blk-<n>` is where it
        // is, and this is what it is.
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
    //! **What a mount is called: `/storage/<label>`.** A name anything persistent can use, since
    //! `/dev/blk/<n>` is discovery order and changes as disks come and go.
    //!
    //! The label is the filesystem's own, else its partition's name, else `blk-<n>`, taking the
    //! first that is valid. A clash takes `-2`, `-3`, … in registry order, so the first device
    //! found keeps the plain name.

    use alloc::format;
    use alloc::string::String;
    use libkern::device::DeviceKind;

    use crate::table::{Device, name};

    /// Longest label accepted, in bytes. Every label this service reads is shorter: GPT's 36,
    /// ext4's 16, FAT's 11.
    pub const MAX: usize = 64;

    /// Whether `s` may be a label: 1 to [`MAX`] bytes of printable ASCII, with no `/`, not
    /// beginning with `.` or a space and not ending with one. **A name beginning with `.` is
    /// refused** because `.` and `..` are not path components and a hidden name is not one a
    /// person looking at `/storage` would find. A space at either end is refused because it
    /// cannot be seen. Anything else — a non-ASCII label included — falls back to the next source.
    pub fn valid(s: &str) -> bool {
        let b = s.as_bytes();
        !b.is_empty()
            && b.len() <= MAX
            && b.iter().all(|&c| (0x20..0x7F).contains(&c) && c != b'/')
            && b[0] != b'.'
            && b[0] != b' '
            && b[b.len() - 1] != b' '
    }

    /// The label `d` would be mounted under, before clashes: its filesystem's own label, else its
    /// partition's name, else `blk-<n>`.
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
    //! **What this service mounts at boot.** Every device holding a filesystem it can serve that
    //! is not already mounted: every ext4 `init` did not mount, and **a FAT on a removable disk**
    //! (Phase 6 Part E.5) — one behind USB mass storage. An internal disk's FAT is its ESP, most
    //! likely, which nobody asked to have mounted: an administrator's `disk --mount` takes one.
    //! Whatever it mounts, it spawns the server for its kind ([`Server`]).
    //!
    //! **A live boot mounts the machine's own disks read-only**: the root is on a RAM disk, which
    //! makes them the install target, and nothing written to one of them by accident could be taken
    //! back. **A removable disk mounts writable on any boot** (Phase 6 Part F): a stick is the
    //! session's, which may eject it ([`eject`]). An administrator's explicit mount (C.5c) is
    //! writable either way; it is the automatic one that has to be careful.
    //!
    //! **A device `disk` has written is read again** (Phase 6 Part G, [`reread`]) and mounted by an
    //! arrival's rules; and **why a filesystem is not mounted** is said in one place,
    //! [`unmounted_why`], which the log line and the table's `note` both give.

    use alloc::string::String;
    use alloc::vec::Vec;
    use libinittoml::manifest::Mode;

    use crate::labels;
    use crate::probe::Found;
    use crate::table::{Device, Mounted};

    /// **The driver a disk behind USB mass storage is published by**, as its registry record names
    /// it (`kernel/src/drivers/xhci/storage.rs`). What makes a disk removable, in this phase's
    /// sense: a SATA disk never is, whatever its bay.
    pub const USB_STORAGE: &[u8] = b"usb-storage";

    /// **The server that serves a filesystem** (Phase 6 Part E.5), spawned from the store's copy,
    /// since the root is mounted by now.
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
        /// `ro` for an internal disk on a live boot, `rw` otherwise (Phase 6 Part F; every mount of
        /// a live boot was `ro` until then).
        pub mode: Mode,
        /// The server for what it holds.
        pub server: Server,
    }

    /// Why an administrator's mount was refused: a [`KError`](libkern::KError) and the words the
    /// refusal carries.
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
    }

    impl Refusal {
        /// The error a client is answered with.
        pub fn kerror(self) -> libkern::KError {
            use libkern::KError;
            match self {
                Refusal::NoSuchDevice => KError::NotFound,
                Refusal::AlreadyMounted | Refusal::LabelTaken => KError::AlreadyExists,
                Refusal::NothingToServe => KError::Unsupported,
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
            }
        }
    }

    /// **An administrator's mount** of the device called `name` (`blk-<n>`), under `label`, or
    /// under the label the service would choose if `label` is empty. `mounted` is everything
    /// mounted, `init`'s included; `taken` is the labels in use; `room` is whether a slot is free;
    /// `init_known` is [`init_known`](crate::sources::init_known)'s answer, and nothing is mounted
    /// without it. **Always writable**, on a live boot too: the auto-mount is the careful one.
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
    /// partitions. Registry ids, ascending. **The holder is the parent only if it is a block
    /// device**: a partition's is its disk or RAM disk, while a whole disk's is the PCI function
    /// of its controller, which is not one and not grantable.
    ///
    /// **And the disk the machine started from** (Phase 6 Part D): it holds the running system,
    /// whether or not anything on it is mounted, so `disks` withholds it and `nxinstall` says why.
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

    /// The boot's auto-mounts, in registry order. `already` is what is mounted, `init`'s mounts
    /// among it, and none of those devices is mounted again. **None at all without
    /// `init_known`**: a device whose `init` mount went unmatched would look free here, and it could
    /// be the running root.
    ///
    /// **Nor the installer's source** (administration Part G.1): a partition named
    /// [`libgpt::INSTALL_SOURCE_LABEL`], the pristine root the install entry loads. Mounted, it
    /// would be in use, and the `disks` grant would withhold it from `nxinstall`, which copies it.
    /// **The rule is the name, not "a RAM disk"**: a test image's scratch filesystem is a RAM disk
    /// this service mounts, for `boot-probe`.
    ///
    /// **Nor anything on the disk the machine started from** (Phase 6 Part D): on a live boot the
    /// stick, which holds the system running from RAM ([`on_boot_medium`]).
    pub fn automount(devices: &[Device], already: &[Mounted], live: bool, init_known: bool) -> Vec<Plan> {
        plan(devices, devices, already, &[], live, init_known)
    }

    /// **The mounts devices that arrived after the boot get** (Phase 6 Part D): `new`, by the boot's
    /// rules, among `all` the service knows, with `taken` the labels of the mounts there are. Only
    /// the new are planned: one an administrator unmounted stays so.
    pub fn arrival(new: &[Device], all: &[Device], already: &[Mounted], taken: &[String], live: bool, init_known: bool) -> Vec<Plan> {
        plan(new, all, already, taken, live, init_known)
    }

    fn plan(candidates: &[Device], all: &[Device], already: &[Mounted], taken: &[String], live: bool, init_known: bool) -> Vec<Plan> {
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

    /// **Why a session's eject was refused** (Phase 6 Part F).
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum EjectRefusal {
        /// Nothing is mounted under that name.
        NotMounted,
        /// What is mounted there is not on a removable disk.
        NotRemovable,
    }

    impl EjectRefusal {
        /// The error a client is answered with.
        pub fn kerror(self) -> libkern::KError {
            match self {
                EjectRefusal::NotMounted => libkern::KError::NotFound,
                EjectRefusal::NotRemovable => libkern::KError::NoAccess,
            }
        }

        /// The reason, as the refusal says it.
        pub fn why(self) -> &'static [u8] {
            match self {
                EjectRefusal::NotMounted => b"nothing is mounted under that name",
                EjectRefusal::NotRemovable => {
                    b"it is not removable: an internal disk is unmounted with the storage grant (with admin disk --unmount)"
                }
            }
        }
    }

    /// **A session's eject of the drive holding the mount named `name`**: the names of every mount
    /// this service made on that disk, in the order they were made — `name` among them — if
    /// something is mounted under `/storage/<name>` on a removable disk.
    ///
    /// **The drive, not the one filesystem** (PR #367 review): a stick is pulled whole, so an
    /// eject that unmounted one partition and said the stick could be pulled left its other
    /// partitions mounted, their unwritten files lost and their filesystems marked in use — the
    /// outcome an eject exists to prevent. **The name is the mount's**, what [`at`] made of a
    /// [`Plan`]'s label — the table's `mounted` column — never the filesystem's own label, which
    /// two sticks can share and a stick can lack. An internal disk's mount is the `storage`
    /// grant's to unmount, as it always was; `init`'s mounts are not this service's.
    pub fn eject(devices: &[Device], mounted: &[Mounted], name: &str) -> Result<Vec<String>, EjectRefusal> {
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
        Ok(mounted
            .iter()
            .filter(|m| m.by == By::Storage && on_disk(m.device))
            .filter_map(|m| m.at.strip_prefix("/storage/").map(String::from))
            .collect())
    }

    /// **What a `Reread` reads again** (Phase 6 Part G), by registry id: a partition, probed again,
    /// or a disk, which the kernel rescans first.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Reread {
        /// A partition: its window is unchanged, so what is in it is read again.
        Partition(u32),
        /// A disk, a RAM disk among them: its table is read again by the kernel.
        Disk(u32),
    }

    /// **Why a `Reread` was refused** (Phase 6 Part G).
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
    }

    impl RereadRefusal {
        /// The error a client is answered with.
        pub fn kerror(self) -> libkern::KError {
            use libkern::KError;
            match self {
                RereadRefusal::InitUnknown | RereadRefusal::BootDisk => KError::NoAccess,
                RereadRefusal::NoSuchDevice => KError::NotFound,
                RereadRefusal::Mounted => KError::WouldBlock,
                RereadRefusal::CannotRescan => KError::Unsupported,
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
            }
        }
    }

    /// **An administrator's `Reread` of the device called `name`** (Phase 6 Part G), once `disk` has
    /// written it: what to read again, or why not. **Refused while mounted** — a partition while it
    /// is, a disk while anything on it is, since its partitions are about to be replaced; a
    /// partition beside a mounted sibling is read again, as the `disks` grant gave it (PR #368
    /// review). **And anything on the disk the machine started from**, mounted or not; mounted is
    /// asked first, so a boot disk holding `init`'s root says it is mounted.
    pub fn reread(devices: &[Device], mounted: &[Mounted], init_known: bool, name: &str) -> Result<Reread, RereadRefusal> {
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
        if on_boot_medium(d, devices) {
            return Err(RereadRefusal::BootDisk);
        }
        Ok(if partition { Reread::Partition(id) } else { Reread::Disk(id) })
    }

    /// **Why `d`, not mounted, is not**, as the service's log line says it, or `None` for no reason
    /// but that nothing mounted it: a FAT its server refuses, the installer's source, anything on the
    /// disk the machine started from, and a FAT on an internal disk. In that order, the log's.
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

    /// **The table's `note`** (Phase 6 Part G, `unmounted-why`): [`unmounted_why`] for a filesystem
    /// found and not mounted; `None` for a mounted one, and for a device holding nothing — a disk
    /// holding a table, or a blank stick — whose row says so already.
    pub fn note(d: &Device, all: &[Device], mount: Option<&Mounted>) -> Option<String> {
        if mount.is_some() || d.found.filesystem().is_none() {
            return None;
        }
        unmounted_why(d, all)
    }

    /// **Whether `d` is on a removable disk** (Phase 6 Part E.5): a disk behind USB mass storage
    /// ([`USB_STORAGE`]), or one of its partitions.
    pub fn removable(d: &Device, all: &[Device]) -> bool {
        let usb = |r: &libkern::device::DeviceRecord| r.driver() == USB_STORAGE;
        usb(&d.record) || all.iter().any(|p| p.record.id == d.record.parent && usb(&p.record))
    }

    /// **Whether `d` is on the disk the machine started from** (Phase 6 Part D): that disk, flagged
    /// in its record, or one of its partitions.
    pub fn on_boot_medium(d: &Device, all: &[Device]) -> bool {
        d.record.is_boot_medium() || all.iter().any(|p| p.record.id == d.record.parent && p.record.is_boot_medium())
    }

    /// Whether `d` is the installer's pristine source: a partition named
    /// [`libgpt::INSTALL_SOURCE_LABEL`] (administration Part G.1). A partition's record carries
    /// its GPT name, which is what the build wrote and what `nxinstall` looks for.
    pub fn is_install_source(d: &Device) -> bool {
        d.record.kind() == libkern::device::DeviceKind::Partition
            && d.record.name() == libgpt::INSTALL_SOURCE_LABEL.as_bytes()
    }
}

pub mod suffix {
    //! What a resolve that reached the service asked for, and what it may ask for where it came
    //! from.
    //!
    //! **A session reaches the service through an endpoint of its own** (C.5b), which the login
    //! supervisors bind at `/storage` with the base `/fs` and at `/dev/storage` with the base
    //! `/info` (C.6). A resolve arriving there is [`session_only`]: the filesystems and the tables,
    //! and nothing that would mint another endpoint. The base alone would not be enough, since a
    //! holder with `BIND_NAMESPACE` could bind the endpoint with no base; an endpoint that cannot
    //! mint is a capability it can be handed, as `/dev/devices`' is.

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
        /// answered with an admin session — a channel for `Storage` requests (C.5c).
        AdminEndpoint,
        /// `info/media`: a media session, carrying `Eject` (Phase 6 Part F).
        Media,
        /// `info/watch`: a watch, on which the service sends a ping per change in the mounts
        /// (Phase 6 Part F).
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

    /// What a resolve arriving on a session endpoint gets: the tables and the filesystems as
    /// asked, a media session and a watch (Phase 6 Part F), and **any endpoint answered as if it
    /// did not exist**: another session endpoint, whose holder could then mint more, and above all
    /// the admin endpoint, which mounts and unmounts. A media session ejects a stick and mounts
    /// nothing, which is what a session may do.
    pub fn session_only(asked: Asked<'_>) -> Asked<'_> {
        match asked {
            Asked::SessionEndpoint | Asked::AdminEndpoint => Asked::Unknown,
            other => other,
        }
    }
}

pub mod watch {
    //! **The sessions following the mounts** (Phase 6 Part F): each a channel the service holds one
    //! end of and **only sends on** — a bare `Changed` per change — so none takes a slot in the
    //! service's wait set, and a client holds one for its life at no cost to another's.
    //!
    //! **A full queue is a ping already waiting**, since a watch carries nothing else: nothing is
    //! owed, and a watcher that falls behind loses no change. **A watcher that has gone is found by
    //! the ping that fails**, `PeerClosed`, and dropped then; when the list is full, every watcher is
    //! pinged first, which costs the living a needless read of the table and frees the dead.
    //!
    //! Nothing here makes a syscall: a ping is the caller's, as an [`Sent`] it reports, so every rule
    //! is a host test.

    use alloc::vec::Vec;

    /// How many watches can be held: the machine's, not a session's, since the service cannot tell
    /// sessions apart. The shell starts a Files per Places pick and per launch, each holding one.
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
mod tests;
