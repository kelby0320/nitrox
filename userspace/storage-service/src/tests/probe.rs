use super::*;

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
