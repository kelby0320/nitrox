use std::cell::RefCell;
use std::string::String;
use std::vec::Vec;

use fs_server_ext4::{BlockReader, BlockWriter, FsError};
use libinittoml::manifest::{self, Mode};
use libkern::device::{DeviceKind, DeviceRecord, NO_PARENT};
use libstream::wire::{Table, Value};

use crate::probe::{Found, fat_label, probe};
use crate::sources::{DiskTable, InitMount, TableEntry, init_known, init_mounts, live_boot, partuuid, source_device};
use crate::labels;
use crate::mounts::{Plan, Refusal, Server, at, automount, explicit, in_use};
use crate::suffix::{self, Asked, session_only};
use crate::table::{self, By, Device, Mounted};

/// Boot sectors real formatters wrote: `mformat -F -v NITROX_ESP`, which is how the image builder
/// makes the ESP; `mformat` with no options on a 16 MiB image, a FAT16 with no label; and sector 0
/// of the image builder's own disk, a GPT's protective MBR. **Real bytes, not a reader's idea of
/// them**: a recogniser tested only on sectors it was written alongside could share its writer's
/// mistake.
const FAT32: &[u8; 512] = include_bytes!("../fixtures/mformat-fat32.bin");
const FAT16: &[u8; 512] = include_bytes!("../fixtures/mformat-fat16.bin");
const PROTECTIVE_MBR: &[u8; 512] = include_bytes!("../fixtures/gpt-protective-mbr.bin");
/// What `fs-server-fat` refuses a Nitrox ESP for: its clusters are a sector.
const SMALL: &str = "512-byte clusters, smaller than a page";

/// An in-memory device.
struct Image(RefCell<Vec<u8>>);

impl BlockReader for Image {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), FsError> {
        let img = self.0.borrow();
        let at = offset as usize;
        let src = img.get(at..at + buf.len()).ok_or(FsError::Io)?;
        buf.copy_from_slice(src);
        Ok(())
    }
}

impl BlockWriter for Image {
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<(), FsError> {
        let mut img = self.0.borrow_mut();
        let at = offset as usize;
        img.get_mut(at..at + buf.len()).ok_or(FsError::Io)?.copy_from_slice(buf);
        Ok(())
    }
}

/// An ext4 filesystem laid out by `mkfs`, labelled `label`.
fn ext4(label: &[u8]) -> Image {
    let blocks = 4096u64;
    let img = Image(RefCell::new(std::vec![0u8; (blocks * 4096) as usize]));
    let mut raw = [0u8; 16];
    raw[..label.len()].copy_from_slice(label);
    fs_server_ext4::mkfs::format(
        &img,
        &fs_server_ext4::mkfs::Params {
            blocks,
            block_size: 4096,
            bytes_per_inode: 16384,
            uuid: *b"storage-testuuid",
            label: raw,
            now: 1_700_000_000,
        },
        &mut |_, _| {},
    )
    .unwrap();
    img
}

/// **A FAT `mkfs.fat` made** of `kib` KiB with `args`, read into memory.
fn mkfs_fat(kib: u64, args: &[&str]) -> Image {
    static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(std::format!("nitrox-storage-{}-{n}.img", std::process::id()));
    std::fs::File::create(&path).unwrap().set_len(kib * 1024).unwrap();
    let out = std::process::Command::new("mkfs.fat")
        .args(args)
        .arg(&path)
        .output()
        .expect("mkfs.fat must be installed (dosfstools) to run storage-service's tests");
    assert!(out.status.success(), "mkfs.fat {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    let bytes = std::fs::read(&path).unwrap();
    let _ = std::fs::remove_file(&path);
    Image(RefCell::new(bytes))
}

/// A device whose first sector is `sector` and the rest zero.
fn with_sector(sector: &[u8; 512]) -> Image {
    let mut img = std::vec![0u8; 64 * 1024];
    img[..512].copy_from_slice(sector);
    Image(RefCell::new(img))
}

fn rec(id: u32, kind: DeviceKind, served: u32, parent: u32, name: &str, blocks: u64) -> DeviceRecord {
    let mut r = DeviceRecord {
        id,
        class: 1,
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
        logical_block_size: 512,
        name_len: name.len() as u32,
        block_count: blocks,
        driver: [0; 16],
        name: [0; 72],
    };
    r.name[..name.len()].copy_from_slice(name.as_bytes());
    r
}

/// A release boot's block devices: the SATA disk, its ESP and its root, then a RAM disk holding a
/// partition of its own — the ids the registry gives them, and `/dev/blk/<n>` in the same order.
fn release_records() -> Vec<DeviceRecord> {
    std::vec![
        rec(3, DeviceKind::Disk, 0, 1, "QEMU HARDDISK", 262_144),
        rec(5, DeviceKind::RamDisk, 1, NO_PARENT, "root.img", 131_072),
        rec(6, DeviceKind::Partition, 2, 3, "ESP", 65_536),
        rec(7, DeviceKind::Partition, 3, 3, "nitrox-root", 196_541),
        rec(8, DeviceKind::Partition, 4, 5, "nitrox-live", 131_000),
    ]
}

fn guid(first: u8) -> [u8; 16] {
    let mut g = [0u8; 16];
    for (i, b) in g.iter_mut().enumerate() {
        *b = first.wrapping_add(i as u8);
    }
    g
}

/// The two disks' tables, as `libgpt` reads them: entries in use, in array order.
fn release_tables() -> Vec<DiskTable> {
    let entry = |g: u8, name: &str, blocks: u64| TableEntry { guid: guid(g), name: name.as_bytes().to_vec(), blocks };
    std::vec![
        DiskTable { disk: 3, entries: std::vec![entry(0x10, "ESP", 65_536), entry(0x20, "nitrox-root", 196_541)] },
        DiskTable { disk: 5, entries: std::vec![entry(0x30, "nitrox-live", 131_000)] },
    ]
}

fn mount(point: &str, source: &str, mode: Mode, device: Option<u32>) -> InitMount {
    InitMount { mount_point: String::from(point), source: String::from(source), mode, device }
}

// --- probe -------------------------------------------------------------------------------------

/// **A real FAT32 and a real FAT16 are recognised**, with their labels — `NO NAME` is none — and a
/// protective MBR, which carries the same `0x55AA` signature, is not.
#[test]
fn fat_is_recognised_from_what_mformat_wrote() {
    assert_eq!(fat_label(FAT32).as_deref(), Some("NITROX_ESP"));
    assert_eq!(fat_label(FAT16).as_deref(), Some(""), "mformat's NO NAME is no label");
    assert_eq!(fat_label(PROTECTIVE_MBR), None, "a GPT disk's sector 0 is not a filesystem");
    assert_eq!(fat_label(&[0u8; 512]), None);
}

/// **Each of the four marks is needed**: a FAT32 sector that loses any one of them is not FAT.
#[test]
fn a_fat_boot_sector_missing_any_mark_is_not_one() {
    for (what, at, value) in [
        ("the signature", 510usize, 0x00u8),
        ("the sector size", 12, 0x03), // 0x0300 = 768
        ("the extended boot signature", 0x42, 0x28),
        ("the type string", 0x52, b'X'),
    ] {
        let mut s = *FAT32;
        s[at] = value;
        assert_eq!(fat_label(&s), None, "{what}");
    }
}

/// **An ext4 filesystem is found as its server would find it**, with its label and how it was
/// left; one mounted writable and never unmounted reads as not clean.
#[test]
fn ext4_is_found_with_its_label_and_state() {
    let img = ext4(b"nitrox-root");
    assert_eq!(probe(&img), Found::Ext4 { label: String::from("nitrox-root"), clean: Some(true) });
    fs_server_ext4::ext4::mark_mounted(&img).unwrap();
    assert_eq!(probe(&img), Found::Ext4 { label: String::from("nitrox-root"), clean: Some(false) });
    assert_eq!(probe(&ext4(b"")), Found::Ext4 { label: String::new(), clean: Some(true) });
}

/// **What `check_device` refuses is not ext4 here either**, even with the magic in place: a root
/// inode that is not a directory would be refused by the server, so it is not offered.
#[test]
fn an_ext4_its_server_would_refuse_is_nothing() {
    let img = ext4(b"broken");
    // The root is inode 2: clear its type bits, at the start of the inode table's second slot.
    let table = {
        let mut gd = [0u8; 4];
        img.read_at(4096 + 8, &mut gd).unwrap(); // group 0's descriptor, `bg_inode_table_lo`
        u32::from_le_bytes(gd) as usize * 4096
    };
    let mut sb = [0u8; 2];
    img.read_at(1024 + 88, &mut sb).unwrap(); // `s_inode_size`
    let inode = table + u16::from_le_bytes(sb) as usize;
    img.0.borrow_mut()[inode + 1] &= 0x0F; // `i_mode`'s type nibble
    assert_eq!(probe(&img), Found::Nothing);
}

#[test]
fn fat_and_nothing_are_found_on_a_device() {
    let esp = Found::Fat {
        label: String::from("NITROX_ESP"),
        clean: Some(true),
        refused: Some(String::from(SMALL)),
    };
    assert_eq!(probe(&with_sector(FAT32)), esp, "the image builder's ESP, which fs-server-fat refuses");
    assert_eq!(probe(&with_sector(PROTECTIVE_MBR)), Found::Nothing);
    assert_eq!(probe(&Image(RefCell::new(std::vec![0u8; 8192]))), Found::Nothing);
    assert_eq!(probe(&Image(RefCell::new(std::vec![0u8; 100]))), Found::Nothing, "too short to read");
}

// --- sources -----------------------------------------------------------------------------------

/// **The kernel's own vector**: `format_partuuid_mixed_endian` in `kernel/src/drivers/gpt.rs`.
/// `init` looks a UUID up by that path, so a service formatting it differently would never match.
#[test]
fn a_guid_is_formatted_as_the_kernel_names_it() {
    let g = [0x78, 0x56, 0x34, 0x12, 0xbc, 0x9a, 0xf0, 0xde, 0x12, 0x34, 0x56, 0x78, 0x9a, 0xbc, 0xde, 0xf0];
    assert_eq!(partuuid(&g), "12345678-9abc-def0-1234-56789abcdef0");
}

/// **A label is a partition's name**: the first partition published with it, and never a disk
/// or a RAM disk of the same name.
#[test]
fn a_label_source_is_the_first_partition_with_that_name() {
    let mut records = release_records();
    records.insert(0, rec(2, DeviceKind::Disk, 9, NO_PARENT, "nitrox-root", 1)); // a disk, not a partition
    records.push(rec(9, DeviceKind::Partition, 5, 5, "nitrox-root", 1)); // published later
    let tables = release_tables();
    assert_eq!(source_device("gpt-partlabel:nitrox-root", &records, &tables), Some(7));
    assert_eq!(source_device("gpt-partlabel:ESP", &records, &tables), Some(6));
    assert_eq!(source_device("gpt-partlabel:nothing", &records, &tables), None);
    assert_eq!(source_device("gpt-partlabel:", &records, &tables), None);
    assert_eq!(source_device("device-path:/dev/blk/3", &records, &tables), None, "not a scheme init accepts");
}

/// **A UUID is found through the parent disk's table**, by position among the entries in use —
/// on whichever disk holds it.
#[test]
fn a_uuid_source_is_found_through_its_disks_table() {
    let (records, tables) = (release_records(), release_tables());
    let uuid = |g: u8| std::format!("gpt-partuuid:{}", partuuid(&guid(g)));
    assert_eq!(source_device(&uuid(0x10), &records, &tables), Some(6));
    assert_eq!(source_device(&uuid(0x20), &records, &tables), Some(7));
    assert_eq!(source_device(&uuid(0x30), &records, &tables), Some(8), "the RAM disk's partition");
    assert_eq!(source_device(&uuid(0x40), &records, &tables), None, "no table has it");
    let upper = uuid(0x20).to_uppercase().replace("GPT-PARTUUID", "gpt-partuuid");
    assert_eq!(source_device(&upper, &records, &tables), None, "init's lookup is exact, and so is this");
}

/// **A position that does not name the partition it points at is no match**: a name or a size
/// that disagrees means the kernel published something the table does not describe.
#[test]
fn a_uuid_whose_partition_disagrees_is_unmatched() {
    let records = release_records();
    let uuid = std::format!("gpt-partuuid:{}", partuuid(&guid(0x20)));
    let mut renamed = release_tables();
    renamed[0].entries[1].name = b"other".to_vec();
    assert_eq!(source_device(&uuid, &records, &renamed), None, "the name differs");
    let mut resized = release_tables();
    resized[0].entries[1].blocks += 1;
    assert_eq!(source_device(&uuid, &records, &resized), None, "the size differs");
    // A table with more entries than the kernel published partitions for: nothing at position 2.
    let mut longer = release_tables();
    longer[0].entries.insert(0, TableEntry { guid: guid(0x50), name: b"lost".to_vec(), blocks: 1 });
    assert_eq!(source_device(&uuid, &records, &longer), None);
}

/// **The release image's own manifest** matches its root to the SATA disk's `nitrox-root`, and
/// the mount keeps its mode and mount point.
#[test]
fn init_toml_is_matched_to_its_devices() {
    let toml = "[[mount]]\nfs_server = \"fs-server-ext4\"\ndevice = \"gpt-partlabel:nitrox-root\"\n\
                mount_point = \"/\"\nmode = \"rw\"\nrequired_for = \"boot\"\n";
    let m = manifest::parse(toml).unwrap();
    let mounts = init_mounts(&m, &release_records(), &release_tables());
    assert_eq!(mounts, [mount("/", "gpt-partlabel:nitrox-root", Mode::Rw, Some(7))]);
}

/// **A live boot is one whose root is on a RAM disk**, through its partition or as the disk
/// itself. A root on a SATA disk, or one that matched nothing, is not.
#[test]
fn a_live_boot_is_a_root_on_a_ram_disk() {
    let records = release_records();
    let on = |device| std::vec![mount("/", "x", Mode::Rw, device), mount("/home", "y", Mode::Rw, Some(8))];
    assert!(live_boot(&on(Some(8)), &records), "a RAM disk's partition");
    assert!(live_boot(&on(Some(5)), &records), "the RAM disk itself");
    assert!(!live_boot(&on(Some(7)), &records), "the SATA disk's partition, though /home is on the RAM disk");
    assert!(!live_boot(&on(None), &records), "a root nothing matched");
    assert!(!live_boot(&[], &records), "no root at all");
}

// --- table -------------------------------------------------------------------------------------

fn devices() -> Vec<Device> {
    let found = |r: &DeviceRecord| match r.id {
        6 => Found::Fat { label: String::from("NITROX_ESP"), clean: Some(true), refused: Some(String::from(SMALL)) },
        7 => Found::Ext4 { label: String::new(), clean: Some(false) },
        8 => Found::Ext4 { label: String::from("nitrox-live"), clean: Some(true) },
        _ => Found::Nothing,
    };
    release_records().into_iter().map(|record| Device { found: found(&record), record }).collect()
}

fn decode(bytes: &[u8]) -> Table {
    Table::decode(bytes).unwrap()
}

fn column<'a>(t: &'a Table, row: usize, name: &str) -> &'a Value {
    let i = t.schema.fields.iter().position(|f| f.name == name).unwrap();
    &t.rows[row][i]
}

fn s(v: &str) -> Value {
    Value::Str(String::from(v))
}

/// **Every block device is a row**, in registry order, named as `/dev/devices` names it, with
/// what it holds and who mounted it.
#[test]
fn all_tsm_has_a_row_per_device() {
    let mounts = [Mounted { device: 7, at: String::from("/"), by: By::Init, mode: Mode::Rw }];
    let t = decode(&table::all(&devices(), &mounts));
    let names: Vec<&Value> = (0..t.rows.len()).map(|i| column(&t, i, "name")).collect();
    assert_eq!(names, [&s("blk-0"), &s("blk-1"), &s("blk-2"), &s("blk-3"), &s("blk-4")]);
    assert_eq!(column(&t, 0, "kind"), &s("disk"));
    // **What each kind calls itself** (the laptop polish's Part C).
    assert_eq!(column(&t, 0, "description"), &s("QEMU HARDDISK"), "a disk's model and serial");
    assert_eq!(column(&t, 1, "description"), &s("root.img"), "a RAM disk's module");
    assert_eq!(column(&t, 3, "description"), &s("nitrox-root"), "a partition's name in its table");
    // A device that names nothing has an empty cell, not an empty string.
    let unnamed = Device { record: rec(9, DeviceKind::Disk, 5, NO_PARENT, "", 8), found: Found::Nothing };
    let i = table::schema().fields.iter().position(|f| f.name == "description").unwrap();
    assert_eq!(table::row(&unnamed, None)[i], Value::Null);
    assert_eq!(column(&t, 0, "size"), &Value::Int(262_144 * 512));
    assert_eq!(column(&t, 0, "filesystem"), &Value::Null, "a disk holding a table holds no filesystem");
    assert_eq!(column(&t, 2, "filesystem"), &s("fat"));
    assert_eq!(column(&t, 2, "label"), &s("NITROX_ESP"));
    assert_eq!(column(&t, 2, "clean"), &Value::Bool(true), "clean is FAT's too (Phase 6 Part E.5)");
    assert_eq!(column(&t, 3, "mounted"), &s("/"));
    assert_eq!(column(&t, 3, "by"), &s("init"));
    assert_eq!(column(&t, 3, "mode"), &s("rw"));
    assert_eq!(column(&t, 3, "label"), &Value::Null, "an empty label is none");
    assert_eq!(column(&t, 4, "mounted"), &Value::Null);
}

/// **`clean` says how a filesystem was left, so it is `Null` while one is mounted writable**,
/// whose state says "in use" because it is. Unmounted, or mounted read-only, the state is what
/// the last writer left.
#[test]
fn clean_is_null_while_mounted_writable() {
    let ds = devices();
    let at = |mode| Mounted { device: 7, at: String::from("/x"), by: By::Storage, mode };
    // By name, not position: a column added in front of these would otherwise move them.
    let col = |name: &str| table::schema().fields.iter().position(|f| f.name == name).unwrap();
    let clean = |m: Option<&Mounted>| table::row(&ds[3], m)[col("clean")].clone();
    assert_eq!(clean(Some(&at(Mode::Rw))), Value::Null);
    assert_eq!(clean(Some(&at(Mode::Ro))), Value::Bool(false));
    assert_eq!(clean(None), Value::Bool(false));
    assert_eq!(table::row(&ds[4], None)[col("clean")], Value::Bool(true));
    assert_eq!(table::row(&ds[3], Some(&at(Mode::Ro)))[col("by")], s("storage"));
}

#[test]
fn a_devices_table_and_the_directory_are_by_name() {
    let ds = devices();
    let t = decode(&table::one(&ds, &[], "blk-3").unwrap());
    assert_eq!(t.rows.len(), 1);
    assert_eq!(column(&t, 0, "name"), &s("blk-3"));
    assert!(table::one(&ds, &[], "blk-9").is_none());
    assert!(table::one(&ds, &[], "all").is_none(), "all is served by all(), not as a device");
    assert_eq!(table::entries(&ds), ["all.tsm", "blk-0.tsm", "blk-1.tsm", "blk-2.tsm", "blk-3.tsm", "blk-4.tsm"]);
}

// --- suffix ------------------------------------------------------------------------------------

#[test]
fn suffixes_name_the_directory_and_its_tables() {
    assert_eq!(suffix::parse(b"info"), Asked::Directory);
    assert_eq!(suffix::parse(b"info/all.tsm"), Asked::File("all"));
    assert_eq!(suffix::parse(b"info/blk-3.tsm"), Asked::File("blk-3"));
    for other in [&b""[..], b"info/", b"info/.tsm", b"info/a/b.tsm", b"info/all", b"block", b"infox"] {
        assert_eq!(suffix::parse(other), Asked::Unknown, "{:?}", core::str::from_utf8(other));
    }
}

// --- labels ------------------------------------------------------------------------------------

/// **A label is a name a person can type and see**: printable ASCII with no `/`, not hidden, with
/// no invisible space at either end, and not longer than any label a disk carries.
#[test]
fn a_label_is_printable_ascii_a_person_can_type() {
    for ok in ["nitrox-root", "NITROX_ESP", "My Disk", "a", &"x".repeat(labels::MAX)] {
        assert!(labels::valid(ok), "{ok:?}");
    }
    for bad in ["", ".hidden", "..", " lead", "trail ", "a/b", "tab\there", "\u{7f}", "ümlaut", &"x".repeat(labels::MAX + 1)] {
        assert!(!labels::valid(bad), "{bad:?}");
    }
}

/// **The filesystem's label, else the partition's name, else `blk-<n>`** — each only if valid. A
/// RAM disk's record name is the module's path, which is not a partition's name and is never used.
#[test]
fn a_label_comes_from_the_filesystem_then_the_partition_then_the_index() {
    let ds = devices();
    let mut own = ds[3].clone();
    own.found = Found::Ext4 { label: String::from("data"), clean: None };
    assert_eq!(labels::preferred(&own), "data", "the filesystem's own, over its partition's name");
    assert_eq!(labels::preferred(&ds[4]), "nitrox-live", "the filesystem's own");
    assert_eq!(labels::preferred(&ds[3]), "nitrox-root", "no filesystem label: the partition's name");
    let mut hidden = ds[3].clone();
    hidden.found = Found::Ext4 { label: String::from(".hidden"), clean: None };
    assert_eq!(labels::preferred(&hidden), "nitrox-root", "an invalid label falls back");
    assert_eq!(labels::preferred(&ds[1]), "blk-1", "a RAM disk's name is its module's path");
    let mut nameless = ds[3].clone();
    nameless.record = rec(7, DeviceKind::Partition, 3, 3, "a/b", 1);
    assert_eq!(labels::preferred(&nameless), "blk-3");
}

#[test]
fn a_clash_takes_the_next_free_suffix() {
    let taken = |v: &[&str]| v.iter().map(|s| String::from(*s)).collect::<Vec<_>>();
    assert_eq!(labels::unique("a", &taken(&[])), "a");
    assert_eq!(labels::unique("a", &taken(&["a"])), "a-2");
    assert_eq!(labels::unique("a", &taken(&["a", "a-2"])), "a-3");
    assert_eq!(labels::unique("a", &taken(&["a", "a-3"])), "a-2", "the first free one");
}

// --- mounts ------------------------------------------------------------------------------------

/// **Every ext4 not already mounted, writable, in registry order** — never `init`'s, never FAT.
#[test]
fn a_boot_mounts_every_ext4_that_is_not_inits() {
    let init = [Mounted { device: 7, at: String::from("/"), by: By::Init, mode: Mode::Rw }];
    assert_eq!(
        automount(&devices(), &init, false, true),
        [Plan { device: 8, label: String::from("nitrox-live"), mode: Mode::Rw, server: Server::Ext4 }]
    );
    assert_eq!(automount(&devices(), &[], false, true).len(), 2, "with no init mount, both ext4s");
}

/// **`init`'s mounts are known only if the manifest read, named a mount, and every mount it named
/// matched a device** (PR #336 review, finding 2). Anything less could leave `init`'s root
/// looking free.
#[test]
fn inits_mounts_are_known_only_when_every_one_matched() {
    let at = |device: Option<u32>| InitMount {
        mount_point: String::from("/"),
        source: String::from("gpt-partlabel:nitrox-root"),
        mode: Mode::Rw,
        device,
    };
    assert!(init_known(Some(&[at(Some(7))])));
    assert!(!init_known(None), "the manifest did not read");
    assert!(!init_known(Some(&[])), "it named no mount, so nothing says which is the root");
    assert!(!init_known(Some(&[at(Some(7)), at(None)])), "one mount matched no device");
}

/// **Nothing is auto-mounted while `init`'s mounts are not known**: the case above that plans
/// both ext4s writable is the one this prevents, since one of them would be the running root.
#[test]
fn a_boot_that_cannot_place_inits_mounts_mounts_nothing() {
    assert_eq!(automount(&devices(), &[], false, false), []);
    assert_eq!(automount(&devices(), &[], true, false), [], "a live boot neither");
}

/// **A live boot mounts read-only.**
#[test]
fn a_live_boot_mounts_read_only() {
    let plan = automount(&devices(), &[], true, true);
    assert!(plan.iter().all(|p| p.mode == Mode::Ro));
}

/// **A clash is settled in registry order**: the first device keeps the name.
#[test]
fn two_filesystems_with_one_label_are_told_apart() {
    let mut ds = devices();
    ds[3].found = Found::Ext4 { label: String::from("data"), clean: Some(true) };
    ds[4].found = Found::Ext4 { label: String::from("data"), clean: Some(true) };
    let labels: Vec<String> = automount(&ds, &[], false, true).into_iter().map(|p| p.label).collect();
    assert_eq!(labels, ["data", "data-2"]);
}

/// **The installer's source is passed over, by its name** (administration Part G.1). An install
/// boot beside a disk holding an older install: the live root, which `init` mounted; the
/// installable ESP; the pristine root, `install-root.img`'s `nitrox-source`; and the SATA disk's
/// `nitrox-root`. And a test image's scratch filesystem, a RAM disk holding ext4 with no partition
/// table, which `boot-probe` needs mounted. Mounted, the source would be in use, and `disks` would
/// withhold it from `nxinstall`.
#[test]
fn the_installers_source_is_passed_over_and_nothing_else() {
    let dev = |record: DeviceRecord, found: Found| Device { record, found };
    let ext4 = |label: &str| Found::Ext4 { label: String::from(label), clean: Some(true) };
    let install_boot = |source_name: &str| {
        std::vec![
            dev(rec(3, DeviceKind::Disk, 0, 1, "QEMU HARDDISK", 1_048_576), Found::Nothing),
            dev(rec(5, DeviceKind::RamDisk, 1, NO_PARENT, "root.img", 57_344), Found::Nothing),
            dev(rec(6, DeviceKind::RamDisk, 2, NO_PARENT, "install-esp.img", 67_584), Found::Fat { label: String::new(), clean: Some(true), refused: Some(String::from(SMALL)) }),
            dev(rec(7, DeviceKind::RamDisk, 3, NO_PARENT, "install-root.img", 24_576), Found::Nothing),
            dev(rec(8, DeviceKind::Partition, 4, 3, "nitrox-root", 196_541), ext4("")),
            dev(rec(9, DeviceKind::Partition, 5, 5, "nitrox-live", 53_248), ext4("")),
            dev(rec(10, DeviceKind::Partition, 6, 7, source_name, 20_480), ext4("")),
            dev(rec(11, DeviceKind::RamDisk, 7, NO_PARENT, "scratch.img", 16_384), ext4("nitrox-scratch")),
        ]
    };
    let init = [Mounted { device: 9, at: String::from("/"), by: By::Init, mode: Mode::Rw }];
    let planned = |ds: &[Device]| automount(ds, &init, true, true).into_iter().map(|p| p.device).collect::<Vec<_>>();
    assert_eq!(
        planned(&install_boot(libgpt::INSTALL_SOURCE_LABEL)),
        [8, 11],
        "the older install and the scratch RAM disk, and not the source"
    );
    // **The name is the rule**: one letter off and the same partition is mounted, which is what
    // proves the skip above was the name's doing and not something else about that device.
    assert_eq!(planned(&install_boot("nitrox-sourcf")), [8, 10, 11]);
}

#[test]
fn a_mount_is_under_storage() {
    assert_eq!(at("nitrox-root"), "/storage/nitrox-root");
}

// --- suffixes of the mounts --------------------------------------------------------------------

/// **`fs/<label>` answers for exactly `fs/<label>`**, alone or with a path after it, which is the
/// `consumed` the `SUBNAMESPACE` reply carries and the kernel requires to end a component.
#[test]
fn a_mounts_suffix_answers_for_its_label() {
    assert_eq!(suffix::parse(b"fs"), Asked::Mounts);
    assert_eq!(suffix::parse(b"fs/nitrox-root"), Asked::Mount { label: "nitrox-root", consumed: 14 });
    assert_eq!(suffix::parse(b"fs/nitrox-root/home/a"), Asked::Mount { label: "nitrox-root", consumed: 14 });
    for bad in [&b"fs/"[..], b"fs//x", b"fsx", b"fs/\xff"] {
        assert_eq!(suffix::parse(bad), Asked::Unknown, "{bad:?}");
    }
}

/// **A session endpoint mints no endpoint**, session or admin, and answers everything else as asked.
#[test]
fn a_session_endpoint_answers_all_but_another_endpoint() {
    assert_eq!(suffix::parse(b"session-endpoint"), Asked::SessionEndpoint);
    assert_eq!(suffix::parse(b"admin-endpoint"), Asked::AdminEndpoint);
    assert_eq!(session_only(Asked::SessionEndpoint), Asked::Unknown);
    assert_eq!(session_only(Asked::AdminEndpoint), Asked::Unknown, "a session cannot mount");
    for kept in [Asked::Directory, Asked::File("all"), Asked::Mounts, Asked::Mount { label: "x", consumed: 4 }] {
        assert_eq!(session_only(kept), kept);
    }
}

// --- an administrator's mount, and what is in use -----------------------------------------------

fn init_root() -> Vec<Mounted> {
    std::vec![Mounted { device: 7, at: String::from("/"), by: By::Init, mode: Mode::Rw }]
}

/// **A mount by name, writable, under the label asked for or the one the service would choose.**
#[test]
fn an_administrator_mounts_by_name() {
    let ds = devices();
    let plan = explicit(&ds, &init_root(), &[], true, true, "blk-4", "").unwrap();
    assert_eq!(plan, Plan { device: 8, label: String::from("nitrox-live"), mode: Mode::Rw, server: Server::Ext4 });
    let plan = explicit(&ds, &init_root(), &[], true, true, "blk-4", "stick").unwrap();
    assert_eq!(plan.label, "stick");
    let taken = [String::from("nitrox-live")];
    assert_eq!(explicit(&ds, &init_root(), &taken, true, true, "blk-4", "").unwrap().label, "nitrox-live-2");
}

/// **Each refusal, for its own reason**: no such device, one already mounted (`init`'s
/// included), one holding nothing this service serves, a bad label, a taken one, no room, and
/// `init`'s mounts not all known.
#[test]
fn an_administrators_mount_is_refused_for_each_reason() {
    let ds = devices();
    let taken = [String::from("taken")];
    let mounted = [
        init_root()[0].clone(),
        Mounted { device: 8, at: String::from("/storage/nitrox-live"), by: By::Storage, mode: Mode::Ro },
    ];
    assert_eq!(explicit(&ds, &init_root(), &[], true, true, "blk-9", ""), Err(Refusal::NoSuchDevice));
    assert_eq!(explicit(&ds, &init_root(), &[], true, true, "blk-3", ""), Err(Refusal::AlreadyMounted), "init's root");
    assert_eq!(explicit(&ds, &mounted, &[], true, true, "blk-4", ""), Err(Refusal::AlreadyMounted), "the service's own");
    assert_eq!(explicit(&ds, &init_root(), &[], true, true, "blk-2", ""), Err(Refusal::NothingToServe), "FAT");
    assert_eq!(explicit(&ds, &init_root(), &[], true, true, "blk-0", ""), Err(Refusal::NothingToServe), "a disk");
    assert_eq!(explicit(&ds, &init_root(), &[], true, true, "blk-4", ".x"), Err(Refusal::BadLabel));
    assert_eq!(explicit(&ds, &init_root(), &taken, true, true, "blk-4", "taken"), Err(Refusal::LabelTaken));
    assert_eq!(explicit(&ds, &init_root(), &[], false, true, "blk-4", ""), Err(Refusal::Full));
    assert_eq!(
        explicit(&ds, &init_root(), &[], true, false, "blk-4", ""),
        Err(Refusal::InitUnknown),
        "a device that could be init's root, however free it looks"
    );
    assert_eq!(Refusal::InitUnknown.kerror(), libkern::KError::NoAccess);
}

/// **A live stick, booted from**: its disk flagged the boot medium, and an ext4 on a partition of
/// it — as a stick holding a filesystem beside the system would be. Neither is planned, and the
/// disk is in use with nothing mounted; another stick's ext4 is planned as ever.
#[test]
fn nothing_on_the_boot_medium_is_mounted_and_its_disk_is_in_use() {
    let dev = |record: DeviceRecord, found: Found| Device { record, found };
    let ext4 = |label: &str| Found::Ext4 { label: String::from(label), clean: Some(true) };
    let mut stick = rec(20, DeviceKind::Disk, 8, 19, "QEMU QEMU HARDDISK (1-0000:00:04.0-1)", 65_536);
    stick.flags |= libkern::device::BOOT;
    let ds = std::vec![
        dev(stick, Found::Nothing),
        dev(rec(21, DeviceKind::Partition, 9, 20, "NITROX_ESP", 32_768), Found::Fat { label: String::new(), clean: Some(true), refused: Some(String::from(SMALL)) }),
        dev(rec(22, DeviceKind::Partition, 10, 20, "data", 16_384), ext4("data")),
        dev(rec(30, DeviceKind::Disk, 11, 29, "another stick", 16_384), ext4("other")),
    ];
    assert!(crate::mounts::on_boot_medium(&ds[0], &ds) && crate::mounts::on_boot_medium(&ds[2], &ds));
    assert!(!crate::mounts::on_boot_medium(&ds[3], &ds));
    let planned: Vec<u32> = automount(&ds, &[], true, true).into_iter().map(|p| p.device).collect();
    assert_eq!(planned, [30], "the other stick alone");
    assert_eq!(in_use(&ds, &[]), [20], "the boot disk, with nothing mounted");
    let mut unflagged = ds.clone();
    unflagged[0].record.flags = 0;
    assert_eq!(automount(&unflagged, &[], true, true).len(), 2, "the flag is what passes it over");
}

/// **An arrival is planned alone, by the boot's rules, beside the mounts there are**: a device an
/// administrator unmounted stays unmounted, and a label in use is numbered past.
#[test]
fn an_arrival_is_planned_alone_beside_the_mounts_there_are() {
    let dev = |record: DeviceRecord, found: Found| Device { record, found };
    let ext4 = |label: &str| Found::Ext4 { label: String::from(label), clean: Some(true) };
    let unmounted = dev(rec(11, DeviceKind::RamDisk, 7, NO_PARENT, "scratch.img", 16_384), ext4("nitrox-scratch"));
    let new = dev(rec(40, DeviceKind::Partition, 12, 39, "partition 1 (unlabelled)", 16_384), ext4("stick"));
    let all = std::vec![unmounted, new.clone()];
    let taken = std::vec![String::from("stick")];
    let plan = crate::mounts::arrival(core::slice::from_ref(&new), &all, &[], &taken, true, true);
    assert_eq!(plan, [Plan { device: 40, label: String::from("stick-2"), mode: Mode::Ro, server: Server::Ext4 }]);
    assert_eq!(crate::mounts::arrival(core::slice::from_ref(&new), &all, &[], &[], false, false), [], "init's mounts unknown");
}

/// **What is in use is every mounted device and the disk that holds it** — never a partition's
/// sibling, and a RAM disk counts as the disk it is.
#[test]
fn in_use_is_each_mount_and_its_disk() {
    let ds = devices();
    assert_eq!(in_use(&ds, &init_root()), [3, 7], "the root and the SATA disk, not the ESP");
    let mut both = init_root();
    both.push(Mounted { device: 8, at: String::from("/storage/nitrox-live"), by: By::Storage, mode: Mode::Ro });
    assert_eq!(in_use(&ds, &both), [3, 5, 7, 8], "and the live partition's RAM disk");
    assert_eq!(in_use(&ds, &[]), [] as [u32; 0]);
    // A whole disk holding a filesystem: its parent is its controller's PCI function, which is not
    // a block device, so the disk alone is in use.
    let bare = [Mounted { device: 3, at: String::from("/storage/bare"), by: By::Storage, mode: Mode::Rw }];
    assert_eq!(in_use(&ds, &bare), [3], "the disk's controller (id 1) is not a device to withhold");
}

// --- FAT (Phase 6 Part E.5) ---------------------------------------------------------------------

/// **A FAT is read by its server's own check**: one with 4 KiB clusters is served, with its label
/// and how it was left; one with 512-byte clusters is a FAT its server refuses, and says why; and
/// one whose sectors are 4 KiB, which the library cannot parse but [`fat_label`] recognises, is
/// still a FAT, refused for its sectors. A FAT found not cleanly unmounted says so.
#[test]
fn a_fat_is_read_by_its_servers_own_check() {
    let served = mkfs_fat(2048, &["-F", "12", "-s", "8", "-n", "NXSTICK"]);
    assert_eq!(probe(&served), Found::Fat { label: String::from("NXSTICK"), clean: Some(true), refused: None });
    assert_eq!(probe(&served).server(), Some(Server::Fat));
    let small = mkfs_fat(2048, &["-F", "12", "-s", "1"]);
    assert_eq!(probe(&small), Found::Fat { label: String::new(), clean: Some(true), refused: Some(String::from(SMALL)) });
    assert_eq!(probe(&small).server(), None, "refused, so nothing would serve it");
    let mut wide = *FAT32;
    wide[11..13].copy_from_slice(&4096u16.to_le_bytes());
    let Found::Fat { refused: Some(why), .. } = probe(&with_sector(&wide)) else {
        panic!("a FAT with 4 KiB sectors is a FAT");
    };
    assert_eq!(why, "4096-byte sectors; only 512-byte sectors are served");
    served.0.borrow_mut()[0x25] |= 1;
    assert_eq!(probe(&served).clean(), Some(false), "the state byte's bit, as fsck.fat reads it");
}

/// A record published by USB mass storage: a stick's disk.
fn on_usb(mut r: DeviceRecord) -> DeviceRecord {
    r.driver[..crate::mounts::USB_STORAGE.len()].copy_from_slice(crate::mounts::USB_STORAGE);
    r
}

/// **A FAT is auto-mounted on a removable disk alone** — a stick's partition, or a stick holding
/// one whole — by `fs-server-fat`, read-only on a live boot. An internal disk's servable FAT is not,
/// nor a stick's FAT its server would refuse; an ext4 is mounted wherever it is, by
/// `fs-server-ext4`.
#[test]
fn a_fat_is_auto_mounted_on_a_removable_disk_alone() {
    let dev = |record: DeviceRecord, found: Found| Device { record, found };
    let fat = |label: &str| Found::Fat { label: String::from(label), clean: Some(true), refused: None };
    let refused = Found::Fat { label: String::from("SMALL"), clean: Some(true), refused: Some(String::from(SMALL)) };
    let ext4 = |label: &str| Found::Ext4 { label: String::from(label), clean: Some(true) };
    let ds = std::vec![
        dev(rec(3, DeviceKind::Disk, 0, 1, "QEMU HARDDISK", 1_048_576), Found::Nothing),
        dev(rec(4, DeviceKind::Partition, 1, 3, "internal", 131_072), fat("INTERNAL")),
        dev(rec(5, DeviceKind::Partition, 2, 3, "data", 131_072), ext4("data")),
        dev(on_usb(rec(20, DeviceKind::Disk, 3, 19, "a stick", 614_400)), Found::Nothing),
        dev(rec(21, DeviceKind::Partition, 4, 20, "partition 1 (unlabelled)", 614_000), fat("NXSTICK")),
        dev(on_usb(rec(30, DeviceKind::Disk, 5, 29, "a whole stick", 65_536)), fat("WHOLE")),
        dev(on_usb(rec(40, DeviceKind::Disk, 6, 39, "an old stick", 4_096)), refused),
    ];
    assert!(!crate::mounts::removable(&ds[1], &ds) && crate::mounts::removable(&ds[4], &ds));
    let plan = automount(&ds, &[], true, true);
    let got: Vec<(u32, &str, Mode, Server)> = plan.iter().map(|p| (p.device, p.label.as_str(), p.mode, p.server)).collect();
    assert_eq!(
        got,
        [(5, "data", Mode::Ro, Server::Ext4), (21, "NXSTICK", Mode::Ro, Server::Fat), (30, "WHOLE", Mode::Ro, Server::Fat)]
    );
    let mut on_sata = ds.clone();
    on_sata[3].record.driver = [0; 16];
    let planned: Vec<u32> = automount(&on_sata, &[], false, true).into_iter().map(|p| p.device).collect();
    assert_eq!(planned, [5, 30], "the driver is what makes the partition's disk removable");
}

/// **An administrator mounts an internal FAT**, writable and by `fs-server-fat`: the rule above is
/// the automatic mount's. One its server would refuse is refused here too.
#[test]
fn an_administrator_mounts_an_internal_fat_but_not_one_its_server_refuses() {
    let mut ds = devices();
    ds[2].found = Found::Fat { label: String::from("BIGESP"), clean: Some(true), refused: None };
    let plan = explicit(&ds, &init_root(), &[], true, true, "blk-2", "").unwrap();
    assert_eq!(plan, Plan { device: 6, label: String::from("BIGESP"), mode: Mode::Rw, server: Server::Fat });
    assert_eq!(explicit(&devices(), &init_root(), &[], true, true, "blk-2", ""), Err(Refusal::NothingToServe), "512-byte clusters");
}

/// The servers are spawned from the store's copies.
#[test]
fn each_server_is_spawned_from_the_store() {
    assert_eq!(Server::Ext4.path(), b"/bin/fs-server-ext4");
    assert_eq!(Server::Fat.path(), b"/bin/fs-server-fat");
}

/// Write one MBR entry into `img`'s first sector, as `sfdisk` does over whatever was there: status
/// `0x00`, `kind`, from `first` for `count` blocks, and the signature.
fn mbr_entry_over(img: &Image, kind: u8, first: u32, count: u32) {
    let mut s = img.0.borrow_mut();
    let e = &mut s[0x1BE..0x1CE];
    e.fill(0);
    e[4] = kind;
    e[8..12].copy_from_slice(&first.to_le_bytes());
    e[12..16].copy_from_slice(&count.to_le_bytes());
    s[510] = 0x55;
    s[511] = 0xAA;
}

/// **A whole disk whose first sector carries a partition entry is never mounted whole** (PR #365
/// review, finding 4). A stick formatted FAT whole and then partitioned keeps its FAT's boot sector
/// beside the entry: reported, and refused, so neither the automatic mount nor an administrator's
/// takes it. The same bytes as a partition are a FAT like any other. An ext4 made whole and then
/// partitioned keeps its superblock at 1024: that disk holds nothing, its partitions being what
/// holds filesystems.
#[test]
fn a_whole_disk_with_partition_entries_is_never_mounted_whole() {
    use crate::probe::{STALE_FAT, probe_record};
    let blocks = 131_072u64;
    let stale = mkfs_fat(64 * 1024, &["-F", "32", "-s", "8", "-n", "OLDFAT"]);
    mbr_entry_over(&stale, 0x83, 2048, (blocks - 2048) as u32);
    let disk = on_usb(rec(20, DeviceKind::Disk, 3, 19, "a stick", blocks));
    let found = probe_record(&stale, &disk);
    assert_eq!(found, Found::Fat { label: String::from("OLDFAT"), clean: Some(true), refused: Some(String::from(STALE_FAT)) });
    let ds = std::vec![Device { record: disk, found }];
    assert_eq!(automount(&ds, &[], false, true), [], "not mounted by itself");
    assert_eq!(explicit(&ds, &[], &[], true, true, "blk-3", ""), Err(Refusal::NothingToServe), "nor as asked");
    let part = rec(21, DeviceKind::Partition, 4, 20, "partition 1", blocks);
    assert_eq!(probe_record(&stale, &part).server(), Some(Server::Fat), "a partition's own sector 0 is its own");

    let whole = ext4(b"whole");
    mbr_entry_over(&whole, 0x83, 2048, 2048);
    assert!(matches!(probe(&whole), Found::Ext4 { .. }), "the superblock is still there");
    let disk = rec(30, DeviceKind::Disk, 5, 1, "a disk", 4096 * 8);
    assert_eq!(probe_record(&whole, &disk), Found::Nothing);
}

/// **A FAT made whole on a stick is still served**: `mkfs.fat` leaves zeros where a table's
/// entries would be, and Windows' boot code puts text there, whose bytes are no entry's status.
#[test]
fn a_fat_made_on_a_whole_stick_is_still_served() {
    use crate::probe::probe_record;
    let disk = on_usb(rec(20, DeviceKind::Disk, 3, 19, "a stick", 4096));
    let zeros = mkfs_fat(2048, &["-F", "12", "-s", "8", "-n", "WHOLE"]);
    assert_eq!(probe_record(&zeros, &disk).server(), Some(Server::Fat));
    let text = mkfs_fat(2048, &["-F", "12", "-s", "8", "-n", "WINDOWS"]);
    let mut code = [0u8; 0x1FE - 0x1AC];
    let words = b"Remove disks or other media.\xFF\r\nDisk error\xFF\r\nPress any key to restart\r\n";
    code[..words.len()].copy_from_slice(words);
    text.0.borrow_mut()[0x1AC..0x1FE].copy_from_slice(&code);
    assert_eq!(probe_record(&text, &disk).server(), Some(Server::Fat));
    // Boot code that happens to hold an entry fitting the disk, but with a status no MBR has.
    let lucky = mkfs_fat(2048, &["-F", "12", "-s", "8", "-n", "LUCKY"]);
    mbr_entry_over(&lucky, 0x0C, 8, 64);
    lucky.0.borrow_mut()[0x1BE] = 0x33;
    assert_eq!(probe_record(&lucky, &disk).server(), Some(Server::Fat), "status 0x33 is no entry's");
}
