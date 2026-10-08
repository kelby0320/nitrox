//! `disk --partition` and `disk --format`, decided: every refusal before a byte is written, the
//! table at 2 TiB's neighbours, and where the one partition and the filesystem go.

use super::*;
use alloc::string::ToString;

/// 2 TiB, in sectors: the first disk an MBR cannot describe.
const TWO_TIB: u64 = 1 << 32;

fn row(name: &str, kind: &str, bytes: i64, parent: Option<&str>, driver: Option<&str>, boot: bool) -> Row {
    Row {
        name: name.to_string(),
        kind: kind.to_string(),
        size: Some(bytes),
        parent: parent.map(str::to_string),
        driver: driver.map(str::to_string),
        boot,
    }
}

/// The laptop with the live stick in it, an internal disk with a partition, and a 2 GiB stick
/// holding one partition.
fn rows() -> Vec<Row> {
    let usb = Some(USB_STORAGE);
    std::vec![
        row("blk-0", "disk", 500 << 30, None, Some("ahci"), false),
        row("blk-1", "partition", 499 << 30, Some("blk-0"), None, false),
        row("blk-2", "disk", 8 << 30, None, usb, true),
        row("blk-3", "partition", 100 << 20, Some("blk-2"), None, false),
        row("blk-4", "disk", 2 << 30, None, usb, false),
        row("blk-5", "partition", (2 << 30) - (1 << 20), Some("blk-4"), None, false),
        row("blk-6", "ramdisk", 64 << 20, None, None, false),
        row("blk-7", "partition", 60 << 20, Some("blk-6"), Some("gpt"), false),
    ]
}

fn t(name: &str) -> Target {
    target(&rows(), name).unwrap()
}

/// **A target folds in its disk's driver and flag**: a partition of a stick is removable, and a
/// partition of the boot stick is on it.
#[test]
fn a_partition_is_removable_and_on_the_boot_disk_as_its_disk_is() {
    let p = t("blk-5");
    assert_eq!((p.kind, p.removable, p.boot, p.sectors), (Kind::Partition, true, false, ((2 << 30) - (1 << 20)) / 512));
    assert!(t("blk-3").boot, "the boot stick's partition");
    assert!(!t("blk-1").removable, "an internal disk's partition");
    assert_eq!(t("blk-6").kind, Kind::RamDisk);
    assert_eq!(target(&rows(), "blk-9"), None);
    assert_eq!(partitions_of(&rows(), "blk-4"), ["blk-5"]);
    assert_eq!(partitions_of(&rows(), "blk-6"), ["blk-7"], "a RAM disk's too");
    assert_eq!(partitions_of(&rows(), "blk-5"), [] as [&str; 0], "a partition has none");
}

/// **The default table follows the filesystem, and is a GPT from 2 TiB** — at its neighbours, for
/// `--partition` as for `--format` — where `--mbr` is refused, naming `--gpt` (PR #368 review).
#[test]
fn the_default_table_follows_the_filesystem_and_is_a_gpt_from_two_tib() {
    for fs in [None, Some(Fs::Fat)] {
        assert_eq!(scheme(fs, TWO_TIB - 1, false, false), Ok(Scheme::Mbr), "{fs:?}");
        assert_eq!(scheme(fs, TWO_TIB, false, false), Ok(Scheme::Gpt), "{fs:?}");
    }
    assert_eq!(scheme(Some(Fs::Ext4), 4 << 21, false, false), Ok(Scheme::Gpt));
    assert_eq!(scheme(Some(Fs::Ext4), 4 << 21, true, false), Ok(Scheme::Mbr), "--mbr chooses");
    assert_eq!(scheme(Some(Fs::Fat), 4 << 21, false, true), Ok(Scheme::Gpt), "--gpt chooses");
    assert_eq!(scheme(None, TWO_TIB - 1, true, false), Ok(Scheme::Mbr));
    assert_eq!(scheme(None, TWO_TIB, true, false), Err(Refusal::MbrTooLarge));
    assert!(Refusal::MbrTooLarge.to_string().contains("use --gpt"));
    assert_eq!(scheme(None, 4 << 21, true, true), Err(Refusal::BothTables));
}

/// **The one partition runs from 1 MiB to the end** — an MBR's to the disk's last sector, a GPT's
/// to its last usable one — and each table builder takes exactly that, and refuses a sector more.
#[test]
fn the_partition_runs_from_one_mib_to_the_end_or_to_the_gpts_backup() {
    let disk = 4_194_304; // 2 GiB
    assert_eq!(extent(Scheme::Mbr, disk), Some((2048, disk - 2048)));
    let (first, n) = extent(Scheme::Gpt, disk).unwrap();
    assert_eq!((first, n), (2048, disk - 33 - 2048));

    let mut sector = [0u8; mbr::LEN];
    let part = |n: u64| [mbr::Partition { kind: mbr::TYPE_FAT32_LBA, first_lba: 2048, blocks: n }];
    assert!(mbr::build(disk, 1, &part(disk - 2048), &mut sector).is_ok());
    assert!(mbr::build(disk, 1, &part(disk - 2047), &mut sector).is_err(), "one past the end");

    let (mut front, mut back) = (std::vec![0u8; table::FRONT_BYTES], std::vec![0u8; table::BACK_BYTES]);
    let gpt = |last: u64| [table::Partition::new(table::TYPE_BASIC_DATA, [1; 16], first, last, b"")];
    assert!(table::build(disk, [2; 16], &gpt(first + n - 1), &mut front, &mut back).is_ok());
    assert!(table::build(disk, [2; 16], &gpt(first + n), &mut front, &mut back).is_err(), "the backup array");

    assert_eq!(extent(Scheme::Mbr, 2048), None, "nothing after 1 MiB");
    assert_eq!(extent(Scheme::Mbr, 2049), Some((2048, 1)));
    assert_eq!(extent(Scheme::Gpt, 2048 + 33), None);
}

/// **The wipes are the first two mebibytes and the last**, clamped to a disk smaller than that.
#[test]
fn the_wipes_are_the_first_two_mebibytes_and_the_last() {
    assert_eq!(wipes(4_194_304), [(0, 2 << 20), ((2 << 30) - (1 << 20), 1 << 20)]);
    assert_eq!(wipes(2048), [(0, 1 << 20), (0, 1 << 20)], "a 1 MiB disk, wiped whole");
}

/// **Every refusal before a byte is written**: anything on the boot stick, a table or a whole-disk
/// format on an internal disk or a RAM disk, a table on a partition, a table chosen for a
/// partition's format, other sector sizes — and what is taken.
#[test]
fn what_is_refused_is_refused_before_anything_is_written() {
    assert_eq!(partition(&t("blk-2"), false, false), Err(Refusal::BootDisk));
    assert_eq!(format(&t("blk-3"), Fs::Fat, None, false, false), Err(Refusal::BootDisk), "its partition");
    assert_eq!(partition(&t("blk-0"), false, false), Err(Refusal::NotRemovable));
    assert_eq!(format(&t("blk-0"), Fs::Ext4, None, false, false), Err(Refusal::NotRemovable), "whole");
    assert_eq!(partition(&t("blk-6"), false, false), Err(Refusal::InMemory), "a RAM disk");
    assert_eq!(format(&t("blk-7"), Fs::Ext4, None, false, false), Err(Refusal::InMemory), "a RAM disk's partition");
    assert!(t("blk-7").in_memory && !t("blk-5").in_memory);
    assert_eq!(partition(&t("blk-5"), false, false), Err(Refusal::NotADisk));
    assert_eq!(format(&t("blk-5"), Fs::Fat, None, true, false), Err(Refusal::TableOnPartition));
    let mut wide = t("blk-4");
    wide.sector_bytes = 4096;
    assert_eq!(partition(&wide, false, false), Err(Refusal::SectorSize(4096)));

    assert!(format(&t("blk-1"), Fs::Ext4, None, false, false).is_ok(), "an internal disk's partition");
    assert!(partition(&t("blk-4"), false, false).is_ok());
    assert!(format(&t("blk-5"), Fs::Ext4, None, false, false).is_ok());
}

/// **A filesystem too small for its partition, or a label it cannot hold, is refused before the
/// table is written for it** — the FAT formatter's own words.
#[test]
fn a_filesystem_too_small_or_a_bad_label_is_refused_before_a_table() {
    let stick = |mib: i64| {
        let rows = [row("blk-9", "disk", mib << 20, None, Some(USB_STORAGE), false)];
        target(&rows, "blk-9").unwrap()
    };
    let refused = format(&stick(16), Fs::Fat, None, false, false).unwrap_err();
    assert_eq!(refused.to_string(), "15360 KiB is too small for a FAT; it needs 16 MiB", "the partition is 15 MiB");
    assert!(format(&stick(17), Fs::Fat, None, false, false).is_ok());
    assert!(format(&stick(16), Fs::Ext4, None, false, false).is_ok(), "an ext4 fits");
    let tiny = format(&stick(1), Fs::Ext4, None, false, false).unwrap_err();
    assert_eq!(tiny, Refusal::TooSmall(String::from("it is too small for a partition at 1 MiB")));

    let fat = |l: &str| format(&stick(64), Fs::Fat, Some(l), false, false).map(|p| p.fs.unwrap().1);
    assert_eq!(fat("photos"), Ok(Label::Fat(*b"PHOTOS     ")), "FAT keeps labels uppercase");
    assert_eq!(fat("a.b").unwrap_err().to_string(), "a FAT label cannot hold '.'");
    assert_eq!(fat("twelve chars").unwrap_err().to_string(), "a FAT label is at most 11 characters");
    let ext4 = |l: &str| format(&stick(64), Fs::Ext4, Some(l), false, false).map(|p| p.fs.unwrap().1);
    assert_eq!(ext4("My Drive"), Ok(Label::Ext4(*b"My Drive\0\0\0\0\0\0\0\0")));
    assert_eq!(ext4("sixteen-chars-ok"), Ok(Label::Ext4(*b"sixteen-chars-ok")));
    assert_eq!(ext4("seventeen-chars-x").unwrap_err().to_string(), "an ext4 label is at most 16 characters");
    assert_eq!(ext4("a/b").unwrap_err().to_string(), "an ext4 label here cannot hold '/'");
    assert_eq!(ext4("é").unwrap_err().to_string(), "an ext4 label here cannot hold the byte 0xc3");
    // **What the storage service could not name a mount by** (PR #369 review), for either.
    for bad in [".data", " data", "data "] {
        assert!(matches!(ext4(bad), Err(Refusal::Label(_))), "ext4 {bad:?}");
        assert!(matches!(fat(bad), Err(Refusal::Label(_))), "FAT {bad:?}");
    }
    assert!(ext4("my.data").is_ok() && fat("MY DATA").is_ok(), "a dot or a space within is a label");
}

/// **What each verb plans**: `--partition`, an MBR typed for FAT; `--format DISK`, the table the
/// filesystem calls for with the filesystem at 1 MiB filling its partition; `--format PART`, no
/// table and the partition whole.
#[test]
fn a_plan_puts_the_filesystem_in_its_one_partition() {
    let disk = 4_194_304;
    assert_eq!(partition(&t("blk-4"), false, false), Ok(Plan { table: Some((Scheme::Mbr, 2048, disk - 2048)), fs: None }));
    let gpt_n = disk - 33 - 2048;
    let fat = Label::Fat(*b"NITROX     ");
    let ext4 = Label::Ext4(*b"nitrox\0\0\0\0\0\0\0\0\0\0");
    assert_eq!(
        format(&t("blk-4"), Fs::Ext4, None, false, false),
        Ok(Plan { table: Some((Scheme::Gpt, 2048, gpt_n)), fs: Some((Fs::Ext4, ext4, 1 << 20, gpt_n * 512)) })
    );
    assert_eq!(
        format(&t("blk-4"), Fs::Fat, None, false, false),
        Ok(Plan { table: Some((Scheme::Mbr, 2048, disk - 2048)), fs: Some((Fs::Fat, fat, 1 << 20, (disk - 2048) * 512)) })
    );
    let part = (2u64 << 30) - (1 << 20);
    assert_eq!(format(&t("blk-5"), Fs::Fat, None, false, false), Ok(Plan { table: None, fs: Some((Fs::Fat, fat, 0, part)) }));
}

/// The type a partition is given: by the FAT to be made, for an MBR; basic data or Linux, for a GPT.
#[test]
fn a_partition_is_typed_for_what_it_holds() {
    use fs_server_fat::bpb::Kind as Fat;
    assert_eq!(mbr_type(None, None), mbr::TYPE_FAT32_LBA, "--partition alone");
    assert_eq!(mbr_type(Some(Fs::Fat), Some(Fat::Fat32)), mbr::TYPE_FAT32_LBA);
    assert_eq!(mbr_type(Some(Fs::Fat), Some(Fat::Fat16)), mbr::TYPE_FAT16_LBA);
    assert_eq!(mbr_type(Some(Fs::Ext4), None), mbr::TYPE_LINUX);
    assert_eq!(gpt_type(None), table::TYPE_BASIC_DATA);
    assert_eq!(gpt_type(Some(Fs::Fat)), table::TYPE_BASIC_DATA);
    assert_eq!(gpt_type(Some(Fs::Ext4)), table::TYPE_LINUX_FS);
}
