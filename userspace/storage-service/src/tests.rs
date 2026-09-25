use std::cell::RefCell;
use std::string::String;
use std::vec::Vec;

use fs_server_ext4::{BlockReader, BlockWriter, FsError};
use libinittoml::manifest::{self, Mode};
use libkern::device::{DeviceKind, DeviceRecord, NO_PARENT};
use libstream::wire::{Table, Value};

use crate::probe::{Found, fat_label, probe};
use crate::sources::{DiskTable, InitMount, TableEntry, init_mounts, live_boot, partuuid, source_device};
use crate::labels;
use crate::mounts::{Plan, at, automount};
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
        _pad: [0; 3],
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
    assert_eq!(probe(&with_sector(FAT32)), Found::Fat { label: String::from("NITROX_ESP") });
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
        6 => Found::Fat { label: String::from("NITROX_ESP") },
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
    assert_eq!(column(&t, 0, "size"), &Value::Int(262_144 * 512));
    assert_eq!(column(&t, 0, "filesystem"), &Value::Null, "a disk holding a table holds no filesystem");
    assert_eq!(column(&t, 2, "filesystem"), &s("fat"));
    assert_eq!(column(&t, 2, "label"), &s("NITROX_ESP"));
    assert_eq!(column(&t, 2, "clean"), &Value::Null, "clean is ext4's");
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
    let clean = |m: Option<&Mounted>| table::row(&ds[3], m)[8].clone();
    assert_eq!(clean(Some(&at(Mode::Rw))), Value::Null);
    assert_eq!(clean(Some(&at(Mode::Ro))), Value::Bool(false));
    assert_eq!(clean(None), Value::Bool(false));
    assert_eq!(table::row(&ds[4], None)[8], Value::Bool(true));
    assert_eq!(table::row(&ds[3], Some(&at(Mode::Ro)))[6], s("storage"));
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
        automount(&devices(), &init, false),
        [Plan { device: 8, label: String::from("nitrox-live"), mode: Mode::Rw }]
    );
    assert_eq!(automount(&devices(), &[], false).len(), 2, "with no init mount, both ext4s");
}

/// **A live boot mounts read-only.**
#[test]
fn a_live_boot_mounts_read_only() {
    let plan = automount(&devices(), &[], true);
    assert!(plan.iter().all(|p| p.mode == Mode::Ro));
}

/// **A clash is settled in registry order**: the first device keeps the name.
#[test]
fn two_filesystems_with_one_label_are_told_apart() {
    let mut ds = devices();
    ds[3].found = Found::Ext4 { label: String::from("data"), clean: Some(true) };
    ds[4].found = Found::Ext4 { label: String::from("data"), clean: Some(true) };
    let labels: Vec<String> = automount(&ds, &[], false).into_iter().map(|p| p.label).collect();
    assert_eq!(labels, ["data", "data-2"]);
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

/// **A session endpoint cannot mint another**, and answers everything else as asked.
#[test]
fn a_session_endpoint_answers_all_but_another_endpoint() {
    assert_eq!(suffix::parse(b"session-endpoint"), Asked::SessionEndpoint);
    assert_eq!(session_only(Asked::SessionEndpoint), Asked::Unknown);
    for kept in [Asked::Directory, Asked::File("all"), Asked::Mounts, Asked::Mount { label: "x", consumed: 4 }] {
        assert_eq!(session_only(kept), kept);
    }
}
