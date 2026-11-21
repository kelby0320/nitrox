use super::*;
use crate::probe::{STALE_FAT, partition_entries, probe_record};

#[test]
fn fat_is_recognised_from_what_mformat_wrote() {
    assert_eq!(fat_label(FAT32).as_deref(), Some("NITROX_ESP"));
    assert_eq!(fat_label(FAT16).as_deref(), Some(""), "mformat's NO NAME is no label");
    assert_eq!(fat_label(PROTECTIVE_MBR), None, "a GPT disk's sector 0 is not a filesystem");
    assert_eq!(fat_label(&[0u8; 512]), None);
}

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

#[test]
fn ext4_is_found_with_its_label_and_state() {
    let img = ext4(b"nitrox-root");
    assert_eq!(
        probe(&img),
        Found::Ext4 { label: String::from("nitrox-root"), clean: Some(true), read_only: None, refused: None }
    );
    fs_server_ext4::ext4::mark_mounted(&img).unwrap();
    assert_eq!(
        probe(&img),
        Found::Ext4 { label: String::from("nitrox-root"), clean: Some(false), read_only: None, refused: None }
    );
    assert_eq!(
        probe(&ext4(b"")),
        Found::Ext4 { label: String::new(), clean: Some(true), read_only: None, refused: None }
    );
}

/// **An ext4 its server would only read is mounted read-only**, by the boot's rules and an
/// administrator's alike, and its row says why: one whose checksums a write would leave stale.
#[test]
fn an_ext4_its_server_would_only_read_is_mounted_read_only() {
    let img = ext4(b"csum");
    let mut word = [0u8; 4];
    img.read_at(1024 + 100, &mut word).unwrap();
    img.0.borrow_mut()[1124..1128].copy_from_slice(&(u32::from_le_bytes(word) | 0x400).to_le_bytes());
    let found = probe(&img);
    let why = String::from(found.read_only().expect("found, and only read"));
    assert!(why.contains("metadata_csum"), "{why}");
    let stick = Device { record: rec(30, DeviceKind::Disk, 11, 29, "a stick", 16_384), found };
    let all = std::vec![stick.clone()];
    assert_eq!(automount(&all, &[], false, true).iter().map(|p| p.mode).collect::<Vec<_>>(), [Mode::Ro]);
    let name = table::name(&stick.record);
    assert_eq!(explicit(&all, &[], &[], true, true, &name, "").map(|p| p.mode), Ok(Mode::Ro));
    let mounted = Mounted { device: 30, at: at("csum"), by: By::Storage, mode: Mode::Ro };
    assert_eq!(crate::mounts::note(&stick, &all, Some(&mounted)), Some(why));
}

/// **An ext4 its server would refuse is found, not served, and says why**, as a FAT does: one whose
/// layout is not what its superblock says, and one whose journal holds changes not yet replayed.
/// It was reported as no filesystem, which reads as a stick safe to format.
#[test]
fn an_ext4_its_server_would_refuse_is_found_and_not_served() {
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
    let refused = |f: &Found| match f {
        Found::Ext4 { refused: Some(why), .. } => why.clone(),
        other => panic!("{other:?}"),
    };
    assert!(refused(&probe(&img)).starts_with("inode 2 is not a directory"));
    let journal = ext4(b"journal");
    let mut word = [0u8; 4];
    journal.read_at(1024 + 96, &mut word).unwrap();
    journal.0.borrow_mut()[1120..1124].copy_from_slice(&(u32::from_le_bytes(word) | 0x4).to_le_bytes());
    let found = probe(&journal);
    assert!(refused(&found).starts_with("its journal holds changes not yet replayed"));
    assert_eq!(found.server(), None, "not served");
    let stick = Device { record: rec(30, DeviceKind::Disk, 11, 29, "a stick", 16_384), found };
    let why = crate::mounts::unmounted_why(&stick, core::slice::from_ref(&stick)).unwrap();
    assert!(why.starts_with("not served: its journal holds"), "{why}");
}

#[test]
fn fat_and_nothing_are_found_on_a_device() {
    let esp = Found::Fat { label: String::from("NITROX_ESP"), clean: Some(true), refused: Some(String::from(SMALL)) };
    assert_eq!(probe(&with_sector(FAT32)), esp, "the image builder's ESP, which fs-server-fat refuses");
    assert_eq!(probe(&with_sector(PROTECTIVE_MBR)), Found::Nothing);
    assert_eq!(probe(&Image(RefCell::new(std::vec![0u8; 8192]))), Found::Nothing);
    assert_eq!(probe(&Image(RefCell::new(std::vec![0u8; 100]))), Found::Nothing, "too short to read");
}

fn mbr_with(entries: &[(u8, u8, u32, u32)]) -> [u8; 512] {
    let mut s = [0u8; 512];
    s[510] = 0x55;
    s[511] = 0xAA;
    for (slot, &(status, kind, first, count)) in entries.iter().enumerate() {
        let e = 0x1BE + 16 * slot;
        s[e] = status;
        s[e + 4] = kind;
        s[e + 8..e + 12].copy_from_slice(&first.to_le_bytes());
        s[e + 12..e + 16].copy_from_slice(&count.to_le_bytes());
    }
    s
}

/// **A sector is a partition table only if signed, clean in every status and holding a used entry.**
#[test]
fn a_partition_table_needs_its_signature_and_a_used_entry_on_the_disk() {
    let ok = mbr_with(&[(0x80, 0x83, 2048, 100)]);
    assert!(partition_entries(&ok, 4096));
    let mut cases: Vec<(&str, [u8; 512])> = Vec::new();
    for (what, at, value) in [("the 0xAA half of the signature", 511usize, 0x00u8), ("the 0x55 half", 510, 0)] {
        let mut s = ok;
        s[at] = value;
        cases.push((what, s));
    }
    let mut s = ok;
    s[0x1BE + 16 * 3] = 0x01;
    cases.push(("a status that is neither 0x00 nor 0x80", s));
    cases.push(("an entry of type 0", mbr_with(&[(0, 0, 2048, 100)])));
    cases.push(("an entry starting at 0", mbr_with(&[(0, 0x83, 0, 100)])));
    cases.push(("an empty entry", mbr_with(&[(0, 0x83, 2048, 0)])));
    for (what, s) in cases {
        assert!(!partition_entries(&s, u64::MAX), "{what}");
    }
}

/// **A partition entry ending past the disk is no entry**, and one ending at its last block is.
#[test]
fn a_partition_entry_must_end_on_the_disk() {
    let s = mbr_with(&[(0, 0x83, 100, 50)]);
    assert!(partition_entries(&s, 150), "ends exactly at the disk's size");
    assert!(!partition_entries(&s, 149), "one block past");
    let both = mbr_with(&[(0, 0x83, 100, 500), (0, 0x83, 10, 5)]);
    assert!(partition_entries(&both, 150), "any entry on the disk counts");
}

/// **A whole disk or RAM disk with a partition entry holds no filesystem of its own**; a
/// partition does.
#[test]
fn a_ram_disk_with_partition_entries_is_not_a_filesystem() {
    let mut sector = *FAT32;
    sector[0x1BE + 4] = 0x0C;
    sector[0x1BE + 8..0x1BE + 12].copy_from_slice(&1u32.to_le_bytes());
    sector[0x1BE + 12..0x1BE + 16].copy_from_slice(&10u32.to_le_bytes());
    let img = with_sector(&sector);
    for kind in [DeviceKind::Disk, DeviceKind::RamDisk] {
        let found = probe_record(&img, &rec(1, kind, 0, 0, "d", 1000));
        assert!(matches!(&found, Found::Fat { refused: Some(why), .. } if why == STALE_FAT), "{kind:?}: {found:?}");
    }
    let part = probe_record(&img, &rec(2, DeviceKind::Partition, 0, 1, "p", 1000));
    assert!(matches!(part, Found::Fat { refused: Some(ref why), .. } if why != STALE_FAT), "{part:?}");
}

/// **A FAT12/16 boot sector must carry its extended signature, type string and a sane sector size.**
#[test]
fn a_fat16_boot_sector_missing_a_mark_or_with_a_wrong_sector_size_is_not_one() {
    assert_eq!(fat_label(FAT16).as_deref(), Some(""));
    let mut s = *FAT16;
    s[0x26] = 0x28;
    assert_eq!(fat_label(&s), None, "the extended boot signature");
    let mut s = *FAT16;
    s[0x36..0x39].copy_from_slice(b"XYZ");
    assert_eq!(fat_label(&s), None, "the type string");
    for size in [0u16, 256, 8192] {
        let mut s = *FAT32;
        s[11..13].copy_from_slice(&size.to_le_bytes());
        assert_eq!(fat_label(&s), None, "{size}-byte sectors");
    }
}

/// **A FAT whose type string alone is cleared is still a FAT**, its label read by `Fat` itself.
#[test]
fn a_fat_with_its_type_string_cleared_is_found_with_fats_own_label() {
    let mut s = *FAT16;
    s[0x36..0x39].copy_from_slice(b"\0\0\0");
    s[0x2B..0x2B + 11].copy_from_slice(b"STICK      ");
    assert_eq!(fat_label(&s), None);
    let Found::Fat { label, .. } = probe(&with_sector(&s)) else { panic!("a FAT with no type string is a FAT") };
    assert_eq!(label, "STICK");
}

/// **A FAT with 4 KiB sectors gives its label.**
#[test]
fn a_fat_with_4k_sectors_gives_its_label() {
    let mut wide = *FAT32;
    wide[11..13].copy_from_slice(&4096u16.to_le_bytes());
    assert_eq!(fat_label(&wide).as_deref(), Some("NITROX_ESP"));
    let Found::Fat { label, .. } = probe(&with_sector(&wide)) else { panic!("a FAT") };
    assert_eq!(label, "NITROX_ESP");
}
