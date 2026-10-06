//! `device-mgr` — the part of the device manager that can be wrong about a device.
//!
//! The manager learns what devices the machine has and hands each to the service that owns its
//! class: the keyboard and mouse to `input-server`, disks to the storage service (administration
//! Part C). It does not drive devices. `docs/planning/administration.md` § *Part B in detail* is
//! the design; `docs/spec/rsproto-devices-ops.md` the protocol.
//!
//! This library is everything it decides with, apart from the syscalls, so it can be tested on
//! the host:
//!
//! - [`names`] — what `/dev/devices` calls a device, and the path that serves it;
//! - [`classes`] — which owner a device goes to, the replay a new owner is sent, and **one owner
//!   per class**;
//! - [`table`] — the TSM1 tables `/dev/devices` serves;
//! - [`suffix`] — what a forwarded resolve asked for.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod names {
    //! **A device's name is its path's**: `blk-<n>` is `/dev/blk/<n>`, `input-<n>` is
    //! `/dev/input/raw/<n>`. So a name in `/dev/devices` says which binding a view would need. The
    //! `<n>` is the record's served index, which the kernel resolves the path through.

    use alloc::format;
    use alloc::string::String;
    use libkern::device::{DeviceKind, DeviceRecord};

    /// What `/dev/devices` calls `r`.
    pub fn name(r: &DeviceRecord) -> String {
        match r.kind() {
            DeviceKind::Disk | DeviceKind::Partition | DeviceKind::RamDisk => format!("blk-{}", r.served),
            DeviceKind::Keyboard | DeviceKind::Mouse => format!("input-{}", r.served),
            DeviceKind::Console => String::from("console"),
            DeviceKind::PciFunction if r.seg == 0 => format!("pci-{:02x}.{:02x}.{}", r.bus, r.dev, r.func),
            DeviceKind::PciFunction => format!("pci-{:04x}.{:02x}.{:02x}.{}", r.seg, r.bus, r.dev, r.func),
            // **After its id, never its port** (PR #353 review): a port is reused when one device
            // leaves and another arrives, while the first's record stays; a connector even has two
            // port numbers. The id is never reused, as `blk-<served>`'s index is not.
            DeviceKind::UsbDevice => format!("usb-{}", r.id),
            DeviceKind::Unknown => format!("dev-{}", r.id),
        }
    }

    /// The path that serves `r`, if one does.
    pub fn path(r: &DeviceRecord) -> Option<String> {
        match r.kind() {
            DeviceKind::Disk | DeviceKind::Partition | DeviceKind::RamDisk => Some(format!("/dev/blk/{}", r.served)),
            DeviceKind::Keyboard | DeviceKind::Mouse => Some(format!("/dev/input/raw/{}", r.served)),
            DeviceKind::Console => Some(String::from("/dev/console")),
            DeviceKind::PciFunction | DeviceKind::UsbDevice | DeviceKind::Unknown => None,
        }
    }

    /// The word for `kind` in a table.
    pub fn kind_word(kind: DeviceKind) -> &'static str {
        match kind {
            DeviceKind::Unknown => "unknown",
            DeviceKind::PciFunction => "pci",
            DeviceKind::Disk => "disk",
            DeviceKind::Partition => "partition",
            DeviceKind::RamDisk => "ramdisk",
            DeviceKind::Keyboard => "keyboard",
            DeviceKind::Mouse => "mouse",
            DeviceKind::Console => "console",
            DeviceKind::UsbDevice => "usb",
        }
    }
}

pub mod classes {
    //! **The manager's classes are its own**, derived from a device's kind: the kernel's
    //! `DeviceClass` calls the console, the keyboard and the mouse all `Char`.
    //!
    //! **A class has one owner at a time.** A raw input device has one ring and one parked reader
    //! in the kernel, so a second reader would drain events meant for the first and stall its
    //! reads. The second subscription is refused, and taken once the first owner goes.

    use alloc::vec::Vec;
    use libkern::device::{DeviceKind, DeviceRecord};

    /// A class the manager hands devices of to an owner.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Class {
        /// Keyboards and mice — `input-server`'s.
        Input,
        /// Disks, partitions and RAM disks — the storage service's, from Part C.
        Block,
    }

    impl Class {
        /// Every class, in the order the manager keeps them.
        pub const ALL: [Class; 2] = [Class::Input, Class::Block];

        /// The class a device of `kind` belongs to, if any owner takes it.
        pub fn of(kind: DeviceKind) -> Option<Class> {
            match kind {
                DeviceKind::Keyboard | DeviceKind::Mouse => Some(Class::Input),
                DeviceKind::Disk | DeviceKind::Partition | DeviceKind::RamDisk => Some(Class::Block),
                // A USB device is no class's until a driver binds an interface of it (Phase 6 Parts
                // B and D): what is handed out then is the keyboard or the disk, not the device.
                DeviceKind::Console | DeviceKind::PciFunction | DeviceKind::UsbDevice | DeviceKind::Unknown => None,
            }
        }

        /// The class `/svc/devices/<name>` names.
        pub fn from_name(name: &[u8]) -> Option<Class> {
            match name {
                b"input" => Some(Class::Input),
                b"block" => Some(Class::Block),
                _ => None,
            }
        }

        /// Its name in a path.
        pub fn name(self) -> &'static str {
            match self {
                Class::Input => "input",
                Class::Block => "block",
            }
        }
    }

    /// The devices a new owner of `class` is sent, in table order — the replay that is coldplug.
    /// **A departed device is left out** (Phase 6 Part C): it is not there to hand over.
    pub fn replay(records: &[DeviceRecord], class: Class) -> Vec<&DeviceRecord> {
        records.iter().filter(|r| !r.is_departed() && Class::of(r.kind()) == Some(class)).collect()
    }

    /// Who owns each class: a channel the manager holds, or nobody.
    #[derive(Debug, Default)]
    pub struct Owners {
        owner: [Option<u64>; 2],
    }

    fn slot(class: Class) -> usize {
        match class {
            Class::Input => 0,
            Class::Block => 1,
        }
    }

    impl Owners {
        /// Nobody owns anything.
        pub const fn new() -> Owners {
            Owners { owner: [None, None] }
        }

        /// Make `channel` the owner of `class`. `false` if the class already has one — the
        /// refusal a second subscription gets.
        pub fn claim(&mut self, class: Class, channel: u64) -> bool {
            let s = &mut self.owner[slot(class)];
            if s.is_some() {
                return false;
            }
            *s = Some(channel);
            true
        }

        /// The owner behind `channel` has gone: free its class, and say which it was.
        pub fn release(&mut self, channel: u64) -> Option<Class> {
            for class in Class::ALL {
                let s = &mut self.owner[slot(class)];
                if *s == Some(channel) {
                    *s = None;
                    return Some(class);
                }
            }
            None
        }

        /// `class`'s owner, if it has one.
        pub fn owner(&self, class: Class) -> Option<u64> {
            self.owner[slot(class)]
        }

        /// Every owner's channel, for the wait set.
        pub fn channels(&self) -> impl Iterator<Item = u64> + '_ {
            self.owner.iter().filter_map(|o| *o)
        }
    }
}

pub mod table {
    //! **`/dev/devices` is typed tables**, which is what makes it more than a listing: `open
    //! /dev/devices/all.tsm | filter kind == "disk"` is a query, with no new shell code, because
    //! `open` decodes a `.tsm` path into a `Table`.
    //!
    //! Columns: `name`, `kind`, `path`, `size` (bytes, a block device's), `description` (a disk's
    //! model and serial, a partition's label, a RAM disk's module and path, a PCI function's ids, a
    //! USB device's name, ids, class, port and speed),
    //! `parent` (the name of the device it belongs to) and `driver`. What a device does not have is
    //! `Null`, not zero or empty: a keyboard has no size, which is different from a size of nothing.

    use alloc::format;
    use alloc::string::String;
    use alloc::vec;
    use alloc::vec::Vec;
    use libkern::device::{DeviceKind, DeviceRecord, NO_PARENT, OUTCOME_DECLINED};
    use libstream::wire::{Schema, StreamFlags, Table, TypeModifiers, TypeTag, Value};

    use crate::names;

    /// The columns every table here has.
    pub fn schema() -> Schema {
        let nullable = TypeModifiers::NULLABLE;
        Schema::new()
            .field("name", TypeTag::String, TypeModifiers::NONE)
            .field("kind", TypeTag::String, TypeModifiers::NONE)
            .field("path", TypeTag::String, nullable)
            .field("size", TypeTag::Int, nullable)
            .field("description", TypeTag::String, nullable)
            .field("parent", TypeTag::String, nullable)
            .field("driver", TypeTag::String, nullable)
    }

    fn text(bytes: &[u8]) -> Option<Value> {
        (!bytes.is_empty()).then(|| Value::Str(String::from_utf8_lossy(bytes).into_owned()))
    }

    /// `r`'s row. `all` is the whole table, for its parent's name.
    pub fn row(r: &DeviceRecord, all: &[DeviceRecord]) -> Vec<Value> {
        let or_null = |v: Option<Value>| v.unwrap_or(Value::Null);
        let size = (r.logical_block_size != 0)
            .then(|| Value::Int((r.logical_block_size as u64).saturating_mul(r.block_count) as i64));
        let description = if r.kind() == DeviceKind::PciFunction {
            Some(Value::Str(format!(
                "{:04x}:{:04x} class {:02x}.{:02x}.{:02x}",
                r.vendor, r.device, r.pci_class, r.subclass, r.prog_if
            )))
        } else if r.kind() == DeviceKind::UsbDevice {
            // Its name, then where and what it is: the ids and the class a driver will match on,
            // and the port and speed a person can see from the outside of the machine. A device
            // with no strings is named by its ids, which this says once.
            let ids = format!("{:04x}:{:04x}", r.vendor, r.device);
            let mut d = String::from_utf8_lossy(r.name()).into_owned();
            if d == ids {
                d.clear();
            }
            if !d.is_empty() {
                d.push_str(", ");
            }
            d.push_str(&format!(
                "{ids} class {:02x}/{:02x}/{:02x}, port {}, {}",
                r.pci_class,
                r.subclass,
                r.prog_if,
                r.port,
                usb_speed(r.speed)
            ));
            Some(Value::Str(d))
        } else if matches!(r.kind(), DeviceKind::Keyboard | DeviceKind::Mouse | DeviceKind::Console) {
            // The name *is* the kind for these; a description repeating it says nothing.
            None
        } else {
            text(r.name())
        };
        let parent = (r.parent != NO_PARENT)
            .then(|| all.iter().find(|p| p.id == r.parent))
            .flatten()
            .map(|p| Value::Str(names::name(p)));
        let driver = text(r.driver()).map(|d| match (d, r.outcome) {
            (Value::Str(s), OUTCOME_DECLINED) => Value::Str(format!("{s} (declined)")),
            (v, _) => v,
        });
        vec![
            Value::Str(names::name(r)),
            Value::Str(String::from(names::kind_word(r.kind()))),
            or_null(names::path(r).map(Value::Str)),
            or_null(size),
            or_null(description),
            or_null(parent),
            or_null(driver),
        ]
    }

    /// A USB device's speed, by the xHCI's default speed IDs, as the record carries it.
    pub fn usb_speed(id: u8) -> &'static str {
        match id {
            1 => "full-speed",
            2 => "low-speed",
            3 => "high-speed",
            4 => "SuperSpeed",
            5 => "SuperSpeedPlus",
            _ => "an unknown speed",
        }
    }

    fn encode(rows: Vec<Vec<Value>>) -> Vec<u8> {
        let table = Table { flags: StreamFlags::NONE, schema: schema(), rows };
        let mut out = Vec::new();
        // A `Vec` sink cannot fail, and every row has the schema's shape by construction.
        let _ = table.encode(&mut out);
        out
    }

    /// `all.tsm`: every present device, a row each, in table order. **A departed device has no
    /// row** (Phase 6 Part C): the tables say what the machine has.
    pub fn all(records: &[DeviceRecord]) -> Vec<u8> {
        encode(records.iter().filter(|r| !r.is_departed()).map(|r| row(r, records)).collect())
    }

    /// `<name>.tsm`: the one present device called `name`, if there is one.
    pub fn one(records: &[DeviceRecord], name: &str) -> Option<Vec<u8>> {
        let r = records.iter().find(|r| !r.is_departed() && names::name(r) == name)?;
        Some(encode(vec![row(r, records)]))
    }

    /// The directory's entries: `all.tsm`, then each present device's, in table order.
    pub fn entries(records: &[DeviceRecord]) -> Vec<String> {
        let mut out = vec![String::from("all.tsm")];
        out.extend(records.iter().filter(|r| !r.is_departed()).map(|r| format!("{}.tsm", names::name(r))));
        out
    }
}

pub mod follow {
    //! **What changed in the table between two reads** (Phase 6 Part C). The manager keeps the
    //! records of its last read and, each time `/dev/registry/changes` answers, reads the table again
    //! and diffs. An id is a record's place, so a record is found in the old read by its id.

    use alloc::vec::Vec;
    use libkern::device::DeviceRecord;

    /// One thing to tell a class's owner.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Change {
        /// A device the manager did not hold, present now: an `Arrived`.
        Arrived(u32),
        /// A device the manager held present, departed now: a `Departed`.
        Departed(u32),
    }

    /// **What changed from `held` to `now`**, in table order:
    /// - a record present now and not held is an arrival;
    /// - a record held present and departed now is a departure;
    /// - **a record that arrived and departed between the two reads is told to no one**: it was
    ///   never handed over, so there is nothing to take back.
    ///
    /// A departure is final, so a record held departed and present now cannot be, and is ignored.
    pub fn diff(held: &[DeviceRecord], now: &[DeviceRecord]) -> Vec<Change> {
        let at = |id: u32| held.get(id as usize).filter(|r| r.id == id);
        now.iter()
            .filter_map(|r| match (r.is_departed(), at(r.id)) {
                (false, None) => Some(Change::Arrived(r.id)),
                (true, Some(h)) if !h.is_departed() => Some(Change::Departed(r.id)),
                _ => None,
            })
            .collect()
    }
}

pub mod suffix {
    //! What a resolve that reached the manager asked for, and what it may ask for where it came
    //! from.
    //!
    //! **A session reaches the manager through an endpoint of its own**, not the one bound at
    //! `/svc/devices` (administration Part B.4). `service-mgr` resolves `info-endpoint` once and
    //! the supervisors bind what it gets at `/dev/devices`, with the base `/info`; a resolve
    //! arriving there is [`info_only`] — **the information and nothing else**, whatever its suffix.
    //! The base alone would not be enough: `desktop-shell` holds the endpoint and `BIND_NAMESPACE`,
    //! so it could bind it with no base, and a suffix like `block` would then be a subscription to
    //! every disk. An endpoint that cannot subscribe is a capability the shell can be handed.

    use crate::classes::Class;

    /// A forwarded suffix, classified.
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub enum Asked<'a> {
        /// `<class>`: become its owner.
        Subscribe(Class),
        /// `info-endpoint`: a forwarding endpoint of its own, every resolve on which is
        /// [`info_only`] — what `service-mgr` couriers to the supervisors for `/dev/devices`.
        InfoEndpoint,
        /// `info`: the directory.
        Directory,
        /// `info/<name>.tsm`: one file — `all`, or a device's name.
        File(&'a str),
        /// Anything else — answered `NotFound`.
        Unknown,
    }

    /// Classify `suffix`.
    pub fn parse(suffix: &[u8]) -> Asked<'_> {
        if suffix == b"info" {
            return Asked::Directory;
        }
        if suffix == b"info-endpoint" {
            return Asked::InfoEndpoint;
        }
        if let Some(file) = suffix.strip_prefix(b"info/") {
            return match file.strip_suffix(b".tsm").map(core::str::from_utf8) {
                Some(Ok(name)) if !name.is_empty() && !name.contains('/') => Asked::File(name),
                _ => Asked::Unknown,
            };
        }
        match Class::from_name(suffix) {
            Some(c) => Asked::Subscribe(c),
            None => Asked::Unknown,
        }
    }

    /// What a resolve that arrived on an info-only endpoint gets: the directory and the tables
    /// as asked, and **anything else answered as if it did not exist** — a subscription, and
    /// another endpoint, whose holder could then mint more.
    pub fn info_only(asked: Asked<'_>) -> Asked<'_> {
        match asked {
            Asked::Subscribe(_) | Asked::InfoEndpoint => Asked::Unknown,
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::classes::{Class, Owners, replay};
    use super::names::{name, path};
    use super::suffix::{self, Asked, info_only};
    use super::table;
    use libkern::device::{
        DeviceKind, DeviceRecord, NO_PARENT, NOT_SERVED, OUTCOME_CLAIMED, OUTCOME_DECLINED,
    };
    use libstream::wire::{Table, Value};

    fn rec(id: u32, kind: DeviceKind, served: u32, parent: u32, name: &str) -> DeviceRecord {
        let mut r = DeviceRecord {
            id,
            class: 0,
            kind: kind as u32,
            served,
            parent,
            outcome: 0,
            vendor: 0xFFFF,
            device: 0,
            pci_class: 0,
            subclass: 0,
            prog_if: 0,
            revision: 0,
            seg: 0,
            bus: 0,
            dev: 0,
            func: 0,
            port: 0,
            speed: 0,
            flags: 0,
            logical_block_size: 0,
            name_len: name.len() as u32,
            block_count: 0,
            driver: [0; 16],
            name: [0; 72],
        };
        r.name[..name.len()].copy_from_slice(name.as_bytes());
        r
    }

    /// A boot's table, as the kernel's registry reports it: an AHCI controller that claimed
    /// itself a disk, a function nobody claimed, the disk and its partition, a RAM disk, the
    /// console, the keyboard and the mouse.
    fn boot() -> Vec<DeviceRecord> {
        let mut ahci = rec(0, DeviceKind::PciFunction, NOT_SERVED, NO_PARENT, "");
        (ahci.vendor, ahci.device, ahci.pci_class, ahci.subclass, ahci.prog_if) = (0x8086, 0x2922, 1, 6, 1);
        (ahci.bus, ahci.dev, ahci.func, ahci.outcome) = (0, 0x1f, 2, OUTCOME_CLAIMED);
        ahci.driver[..4].copy_from_slice(b"ahci");
        let mut other = rec(1, DeviceKind::PciFunction, NOT_SERVED, NO_PARENT, "");
        (other.vendor, other.bus, other.dev, other.outcome) = (0x1234, 0, 2, OUTCOME_DECLINED);
        other.driver[..4].copy_from_slice(b"ahci");
        let mut disk = rec(2, DeviceKind::Disk, 0, 0, "QEMU HARDDISK (QM00001)");
        (disk.logical_block_size, disk.block_count) = (512, 1 << 20);
        let mut part = rec(3, DeviceKind::Partition, 1, 2, "nitrox-root");
        (part.logical_block_size, part.block_count) = (512, 1 << 19);
        vec![
            ahci,
            other,
            disk,
            part,
            rec(4, DeviceKind::RamDisk, 2, NO_PARENT, "module 0 (root.img)"),
            rec(5, DeviceKind::Console, NOT_SERVED, NO_PARENT, "console"),
            rec(6, DeviceKind::Keyboard, 0, NO_PARENT, "keyboard"),
            rec(7, DeviceKind::Mouse, 1, NO_PARENT, "mouse"),
        ]
    }

    /// **A name is its path's**, and the `<n>` is the served index — the keyboard is `input-0`
    /// though the console sits between it and the disks.
    #[test]
    fn a_device_is_named_by_the_path_that_serves_it() {
        let b = boot();
        let named: Vec<String> = b.iter().map(name).collect();
        assert_eq!(
            named,
            ["pci-00.1f.2", "pci-00.02.0", "blk-0", "blk-1", "blk-2", "console", "input-0", "input-1"]
        );
        assert_eq!(path(&b[3]).as_deref(), Some("/dev/blk/1"));
        assert_eq!(path(&b[7]).as_deref(), Some("/dev/input/raw/1"));
        assert_eq!(path(&b[0]), None, "a PCI function has no path");
        let mut far = b[0];
        far.seg = 1;
        assert_eq!(name(&far), "pci-0001.00.1f.2", "a second segment is named");
    }

    /// **A USB device** (Phase 6 Part A.3): named by its id, never its port; no path; the kind
    /// `usb`; no class's until a driver binds an interface of it; and described by its name, ids,
    /// class, port and speed, with the controller as its parent.
    #[test]
    fn a_usb_device_is_named_by_its_id_and_described_by_where_it_is() {
        let b = boot();
        let mut kbd = rec(8, DeviceKind::UsbDevice, NOT_SERVED, 1, "QEMU USB Keyboard (42)");
        (kbd.vendor, kbd.device) = (0x0627, 0x0001);
        (kbd.pci_class, kbd.subclass, kbd.prog_if) = (3, 1, 1);
        (kbd.port, kbd.speed) = (9, 3);
        assert_eq!(name(&kbd), "usb-8");
        let mut moved = kbd;
        moved.port = 14;
        assert_eq!(name(&moved), "usb-8", "the name does not follow the port");
        assert_eq!(path(&kbd), None);
        assert_eq!(crate::names::kind_word(DeviceKind::UsbDevice), "usb");
        assert_eq!(Class::of(DeviceKind::UsbDevice), None);
        let mut all = b.to_vec();
        all.push(kbd);
        let row = table::row(&kbd, &all);
        assert_eq!(
            row[4],
            Value::Str(String::from("QEMU USB Keyboard (42), 0627:0001 class 03/01/01, port 9, high-speed"))
        );
        assert_eq!(row[5], Value::Str(name(&b[1])), "the controller is its parent");
        // The kernel names a device with no strings by its ids, `vvvv:pppp`.
        let unnamed = rec(8, DeviceKind::UsbDevice, NOT_SERVED, 1, "0627:0001");
        let unnamed = DeviceRecord { vendor: kbd.vendor, device: kbd.device, port: 9, speed: 3, ..unnamed };
        let unnamed = DeviceRecord { pci_class: 3, subclass: 1, prog_if: 1, ..unnamed };
        assert_eq!(
            table::row(&unnamed, &all)[4],
            Value::Str(String::from("0627:0001 class 03/01/01, port 9, high-speed")),
            "a device with no strings is described by its ids, once"
        );
    }

    /// **The replay is the class, in table order** — and the console, a `Char` device like the
    /// keyboard, is nobody's.
    #[test]
    fn a_new_owner_is_sent_its_class_in_table_order() {
        let b = boot();
        let ids = |c| replay(&b, c).iter().map(|r| r.id).collect::<Vec<_>>();
        assert_eq!(ids(Class::Input), [6, 7]);
        assert_eq!(ids(Class::Block), [2, 3, 4]);
        assert_eq!(Class::of(DeviceKind::Console), None);
        assert_eq!(Class::of(DeviceKind::PciFunction), None);
    }

    /// **One owner per class**, and a class is taken again once its owner goes.
    #[test]
    fn a_class_has_one_owner_at_a_time() {
        let mut o = Owners::new();
        assert!(o.claim(Class::Input, 10));
        assert!(!o.claim(Class::Input, 11), "a second owner would stall the first's reads");
        assert!(o.claim(Class::Block, 11), "another class is another owner");
        assert_eq!(o.release(99), None, "a channel that owns nothing frees nothing");
        assert_eq!(o.release(10), Some(Class::Input));
        assert!(o.claim(Class::Input, 12), "taken once the owner has gone");
        assert_eq!(o.owner(Class::Input), Some(12));
        let mut chans: Vec<u64> = o.channels().collect();
        chans.sort();
        assert_eq!(chans, [11, 12]);
    }

    /// **The rows decode as the shell's `open` decodes them**, with `Null` where a device has no
    /// value — a keyboard's size is absent, not zero.
    #[test]
    fn all_tsm_is_a_table_a_row_per_device() {
        let b = boot();
        let bytes = table::all(&b);
        let t = Table::decode(&bytes).unwrap();
        assert_eq!(t.rows.len(), 8);
        let col = |name: &str| t.schema.fields.iter().position(|f| f.name == name).unwrap();
        let cell = |row: usize, name: &str| t.rows[row][col(name)].clone();
        assert_eq!(cell(2, "kind"), Value::Str("disk".into()));
        assert_eq!(cell(2, "size"), Value::Int(512 << 20));
        assert_eq!(cell(2, "description"), Value::Str("QEMU HARDDISK (QM00001)".into()));
        assert_eq!(cell(2, "parent"), Value::Str("pci-00.1f.2".into()), "a disk's controller");
        assert_eq!(cell(3, "parent"), Value::Str("blk-0".into()), "a partition's disk");
        assert_eq!(cell(6, "size"), Value::Null, "a keyboard has no size");
        assert_eq!(cell(6, "path"), Value::Str("/dev/input/raw/0".into()));
        assert_eq!(cell(0, "description"), Value::Str("8086:2922 class 01.06.01".into()));
        assert_eq!(cell(0, "driver"), Value::Str("ahci".into()));
        assert_eq!(cell(1, "driver"), Value::Str("ahci (declined)".into()));
        assert_eq!(cell(0, "path"), Value::Null);
    }

    /// **The file the shell opens is page-padded**, as a memory object is: the table still
    /// decodes to its rows. `Table::decode` stops at the terminator — pinned here, where the
    /// padding is real, because a round trip over the exact bytes cannot see it.
    #[test]
    fn a_padded_table_decodes_to_its_rows() {
        let b = boot();
        let mut bytes = table::one(&b, "blk-1").unwrap();
        let rows = Table::decode(&bytes).unwrap().rows.len();
        bytes.resize(4096, 0);
        assert_eq!(Table::decode(&bytes).unwrap().rows.len(), rows);
        assert_eq!(rows, 1);
        assert!(table::one(&b, "blk-9").is_none());
    }

    /// **`all.tsm` first, then a file per device** — so `list /dev/devices` names every device.
    #[test]
    fn the_directory_lists_all_then_each_device() {
        let b = boot();
        let e = table::entries(&b);
        assert_eq!(e[0], "all.tsm");
        assert_eq!(e.len(), 9);
        assert!(e.contains(&"input-1.tsm".to_string()));
    }

    /// **What a session can reach is the information and nothing else**: its base is `/info`, so
    /// every resolve from a session arrives under it, and a class is a bare name.
    #[test]
    fn a_suffix_asks_for_a_class_the_directory_or_a_file() {
        assert_eq!(suffix::parse(b"input"), Asked::Subscribe(Class::Input));
        assert_eq!(suffix::parse(b"block"), Asked::Subscribe(Class::Block));
        assert_eq!(suffix::parse(b"info"), Asked::Directory);
        assert_eq!(suffix::parse(b"info/all.tsm"), Asked::File("all"));
        assert_eq!(suffix::parse(b"info/blk-0.tsm"), Asked::File("blk-0"));
        assert_eq!(suffix::parse(b"info-endpoint"), Asked::InfoEndpoint);
        for bad in [&b""[..], b"inputs", b"info/", b"info/.tsm", b"info/blk-0", b"info/a/b.tsm", b"info/input"] {
            assert_eq!(suffix::parse(bad), Asked::Unknown, "{:?}", core::str::from_utf8(bad));
        }
    }

    /// **An info-only endpoint answers the information and nothing else** — whatever suffix
    /// reaches it, which is what makes it safe to hand to a process that could bind it with any
    /// base. Every suffix the root endpoint would subscribe or mint on is refused here, and the
    /// information passes as asked.
    #[test]
    fn an_info_only_endpoint_answers_the_information_and_nothing_else() {
        for asked in [b"input".as_slice(), b"block", b"info-endpoint"] {
            let whole = suffix::parse(asked);
            assert_ne!(whole, Asked::Unknown, "the root endpoint would act on {:?}", core::str::from_utf8(asked));
            assert_eq!(info_only(whole), Asked::Unknown, "{:?}", core::str::from_utf8(asked));
        }
        assert_eq!(info_only(suffix::parse(b"info")), Asked::Directory);
        assert_eq!(info_only(suffix::parse(b"info/all.tsm")), Asked::File("all"));
        assert_eq!(info_only(suffix::parse(b"nonsense")), Asked::Unknown);
    }

    fn departed(mut r: DeviceRecord) -> DeviceRecord {
        r.flags |= libkern::device::DEPARTED;
        r
    }

    /// A USB keyboard plugged in after the boot's table: its device, then its keyboard node.
    fn plugged(table: &mut Vec<DeviceRecord>) -> (u32, u32) {
        let usb = table.len() as u32;
        table.push(rec(usb, DeviceKind::UsbDevice, NOT_SERVED, 0, "QEMU USB Keyboard"));
        table.push(rec(usb + 1, DeviceKind::Keyboard, 2, usb, "keyboard"));
        (usb, usb + 1)
    }

    /// **What changed between two reads** (Phase 6 Part C): a device plugged in arrives, the same
    /// device unplugged departs, and both in one read — a device that came and went between two
    /// reads — is told to no one. A read with nothing new says nothing.
    #[test]
    fn a_diff_finds_arrivals_and_departures_and_skips_what_came_and_went() {
        use super::follow::{Change, diff};
        let held = boot();
        assert!(diff(&held, &held).is_empty(), "nothing changed");
        let mut now = boot();
        let (usb, kbd) = plugged(&mut now);
        assert_eq!(diff(&held, &now), vec![Change::Arrived(usb), Change::Arrived(kbd)]);
        let mut gone = now.clone();
        gone[usb as usize] = departed(gone[usb as usize]);
        gone[kbd as usize] = departed(gone[kbd as usize]);
        assert_eq!(diff(&now, &gone), vec![Change::Departed(usb), Change::Departed(kbd)]);
        assert!(diff(&held, &gone).is_empty(), "came and went between two reads: told to no one");
        assert!(diff(&gone, &gone).is_empty(), "a departure is told once");
    }

    /// **A departed device is in no replay and no table** (Phase 6 Part C), so an owner subscribing
    /// after it went is not handed it, and `/dev/devices` does not list it.
    #[test]
    fn a_departed_device_is_in_no_replay_and_no_table() {
        let mut t = boot();
        let (usb, kbd) = plugged(&mut t);
        assert_eq!(replay(&t, Class::Input).len(), 3, "the i8042's two and the USB keyboard");
        t[usb as usize] = departed(t[usb as usize]);
        t[kbd as usize] = departed(t[kbd as usize]);
        assert_eq!(replay(&t, Class::Input).len(), 2);
        assert_eq!(Table::decode(&table::all(&t)).unwrap().rows.len(), 8, "the boot's eight");
        assert!(table::one(&t, &name(&t[kbd as usize])).is_none());
        assert_eq!(table::entries(&t).len(), 9);
    }
}
