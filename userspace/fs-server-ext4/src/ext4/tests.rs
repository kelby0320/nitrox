/// The group geometry the allocators work from, and the **last group's short tail**: a bitmap's
/// spare bits address blocks past the end of the device.
#[test]
fn a_short_final_group_reports_only_the_blocks_it_has() {
    use super::Superblock;
    let sb = Superblock {
        block_size: 1024,
        inodes_per_group: 2048,
        inode_size: 256,
        desc_size: 32,
        first_gdt_block: 2,
        blocks_per_group: 8192,
        first_data_block: 1,
        blocks_count: 24576,
        inodes_count: 6144,
    };
    assert_eq!(sb.group_count(), 3);
    assert_eq!(sb.blocks_in_group(0), 8192);
    assert_eq!(sb.blocks_in_group(1), 8192);
    assert_eq!(sb.blocks_in_group(2), 8191, "the tail group is short by one");
    let total: usize = (0..3).map(|g| sb.blocks_in_group(g)).sum();
    assert_eq!(total as u64, sb.blocks_count - sb.first_data_block as u64);
    assert_eq!(sb.group_start(0), 1);
    assert_eq!(sb.group_start(2), 16385);
    assert_eq!(sb.inodes_in_group(2), 2048);
    let odd = Superblock { inodes_count: 5000, ..sb };
    assert_eq!(odd.inodes_in_group(2), 5000 - 2 * 2048);
    assert_eq!(odd.inodes_in_group(9), 0, "a group past the end has nothing");
}

/// Descriptor offsets are a flat array, so a table spanning blocks needs no special case.
#[test]
fn group_descriptors_are_a_flat_array_across_block_boundaries() {
    use super::Superblock;
    let sb = Superblock {
        block_size: 1024,
        inodes_per_group: 2048,
        inode_size: 256,
        desc_size: 32,
        first_gdt_block: 2,
        blocks_per_group: 8192,
        first_data_block: 1,
        blocks_count: 8 * 1024 * 1024,
        inodes_count: 262_144,
    };
    assert_eq!(sb.group_desc_offset(0), 2048);
    assert_eq!(sb.group_desc_offset(31), 2048 + 31 * 32);
    assert_eq!(sb.group_desc_offset(32), 2048 + 1024);
}

/// **A directory record is read only where its length holds its header and name**, is a
/// multiple of 4 and ends in its block: each refusal, beside a record that passes.
#[test]
fn a_record_is_refused_unless_its_length_holds_it() {
    use super::record;
    let block = |len: u16, name_len: u8| {
        let mut b = [0u8; 32];
        b[0..4].copy_from_slice(&11u32.to_le_bytes());
        b[4..6].copy_from_slice(&len.to_le_bytes());
        b[6] = name_len;
        b
    };
    assert_eq!(record(&block(12, 4), 0).map(|r| (r.ino, r.len, r.name_len)), Some((11, 12, 4)));
    assert!(record(&block(12, 5), 0).is_none(), "a name past its record");
    assert!(record(&block(14, 1), 0).is_none(), "a length not a multiple of 4");
    assert!(record(&block(36, 1), 0).is_none(), "a record past its block");
    assert!(record(&block(32, 1), 28).is_none(), "a header past its block");
}

/// `decode_time` takes the epoch-extension bits from the *matching* `extra` word.
#[test]
fn decode_time_applies_the_epoch_extension() {
    assert_eq!(super::decode_time(1_784_900_730, 0), 1_784_900_730);
    assert_eq!(super::decode_time(0, 1), 1i64 << 32);
    assert_eq!(super::decode_time(7, 2), (2i64 << 32) | 7);
    assert_eq!(super::decode_time(5, 0xFFFF_FFFC), 5);
}

/// A stamped inode places each timestamp at its own offset.
#[test]
fn stamp_writes_each_field_at_its_own_offset() {
    use super::{I_ATIME, I_CRTIME, I_CTIME, I_MTIME, I_MTIME_EXTRA, Stamp, stamp};
    let now: i64 = (1i64 << 32) | 1_784_900_730; // past 2038: epoch bit set
    let mut inode = [0u8; 256];
    stamp(&mut inode, now, 256, Stamp::Created);

    let at = |off: usize| u32::from_le_bytes(inode[off..off + 4].try_into().unwrap());
    assert_eq!(at(I_MTIME), 1_784_900_730);
    assert_eq!(at(I_CTIME), 1_784_900_730);
    assert_eq!(at(I_ATIME), 1_784_900_730);
    assert_eq!(at(I_CRTIME), 1_784_900_730);
    // The epoch bit lands in mtime's own extra word, and `decode_time` recovers the full value.
    assert_eq!(at(I_MTIME_EXTRA) & 0x3, 1);
    assert_eq!(super::decode_time(at(I_MTIME), at(I_MTIME_EXTRA)), now);
}

/// A metadata-only change moves ctime and leaves mtime alone.
#[test]
fn metadata_only_stamp_leaves_mtime_untouched() {
    use super::{I_CTIME, I_MTIME, Stamp, stamp};
    let mut inode = [0u8; 256];
    stamp(&mut inode, 1000, 256, Stamp::Created);
    stamp(&mut inode, 2000, 256, Stamp::MetadataOnly);
    let at = |off: usize| u32::from_le_bytes(inode[off..off + 4].try_into().unwrap());
    assert_eq!(at(I_MTIME), 1000, "contents did not change");
    assert_eq!(at(I_CTIME), 2000, "the inode did");
}

/// A base-size (128-byte) inode has no `*_extra` words.
#[test]
fn a_small_inode_gets_no_extra_words() {
    use super::{I_CTIME_EXTRA, Stamp, stamp};
    let mut inode = [0u8; 256];
    stamp(&mut inode, 1_784_900_730, 128, Stamp::Created);
    assert_eq!(&inode[I_CTIME_EXTRA..I_CTIME_EXTRA + 4], &[0, 0, 0, 0]);
}

use super::{Unservable, check_device, read_file, stat_file};
use crate::FsError;
use crate::test_support::{ImageReader, fixture};

/// Set a little-endian field in a fixture image's superblock (byte 1024 onwards).
fn poke_sb(img: &mut [u8], off: usize, bytes: &[u8]) {
    img[1024 + off..1024 + off + bytes.len()].copy_from_slice(bytes);
}

#[test]
fn a_real_filesystem_passes_the_check_at_both_block_sizes() {
    for bs in [1024, 4096] {
        assert_eq!(check_device(&ImageReader(fixture(bs, b"g\n"))), Ok(()), "{bs}-byte blocks");
    }
}

#[test]
fn a_zeroed_device_is_refused_for_its_magic_and_a_short_one_as_unreadable() {
    let len = fixture(4096, b"g\n").len();
    assert_eq!(check_device(&ImageReader(vec![0; len])), Err(Unservable::NoMagic { found: 0 }));
    assert_eq!(check_device(&ImageReader(vec![0; 1500])), Err(Unservable::Unreadable));
}

#[test]
fn each_superblock_check_names_what_it_found() {
    let good = fixture(4096, b"g\n");
    let refused = |off: usize, bytes: &[u8]| {
        let mut img = good.clone();
        poke_sb(&mut img, off, bytes);
        check_device(&ImageReader(img)).unwrap_err()
    };
    assert_eq!(refused(56, &0x1234u16.to_le_bytes()), Unservable::NoMagic { found: 0x1234 });
    assert_eq!(refused(96, &0x80u32.to_le_bytes()), Unservable::Wide64Bit);
    assert_eq!(refused(24, &3u32.to_le_bytes()), Unservable::BlockTooLarge { log: 3 });
    assert_eq!(refused(88, &0u16.to_le_bytes()), Unservable::ZeroField("s_inode_size"));
    assert_eq!(refused(40, &0u32.to_le_bytes()), Unservable::ZeroField("s_inodes_per_group"));
    assert_eq!(refused(32, &0u32.to_le_bytes()), Unservable::ZeroField("s_blocks_per_group"));
}

/// **A right superblock over the wrong blocks**: the superblock alone passes it.
#[test]
fn a_superblock_whose_layout_is_not_there_is_refused_at_the_root() {
    for (bs, gdt) in [(4096usize, 4096usize), (1024, 2048)] {
        let good = fixture(bs as u32, b"g\n");

        let mut img = good.clone();
        img[gdt..gdt + bs].fill(0);
        assert_eq!(
            check_device(&ImageReader(img)),
            Err(Unservable::RootNotDirectory { mode: 0 }),
            "{bs}-byte blocks, descriptors zeroed"
        );

        let mut img = good.clone();
        img[gdt + 8..gdt + 12].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
        assert_eq!(
            check_device(&ImageReader(img)),
            Err(Unservable::RootUnreadable),
            "{bs}-byte blocks, inode table past the end"
        );
    }
}

/// The startup check and a request share one parser, so a refused device fails a request with
/// the error the check's reason maps to.
#[test]
fn a_request_over_a_refused_device_fails_with_the_error_its_reason_names() {
    let good = fixture(4096, b"g\n");
    let mut out = [0u8; 64];

    let zeroed = ImageReader(vec![0; good.len()]);
    let why = check_device(&zeroed).unwrap_err();
    assert_eq!(read_file(&zeroed, b"/system/current-generation", &mut out), Err(why.fs_error()));
    assert_eq!(why.fs_error(), FsError::Corrupt);

    let mut wide = good.clone();
    poke_sb(&mut wide, 96, &0x80u32.to_le_bytes());
    let wide = ImageReader(wide);
    let why = check_device(&wide).unwrap_err();
    assert_eq!(stat_file(&wide, b"/system/current-generation"), Err(why.fs_error()));
    assert_eq!(why.fs_error(), FsError::Unsupported);
}

#[test]
fn each_reason_reads_as_a_sentence_naming_what_was_found() {
    assert_eq!(
        format!("{}", Unservable::NoMagic { found: 0 }),
        "no ext4 filesystem: superblock magic 0x0000, not 0xef53"
    );
    assert_eq!(
        format!("{}", Unservable::RootNotDirectory { mode: 0o100644 }),
        "inode 2 is not a directory (mode 0o100644): the group descriptors or the inode table \
         are not what the superblock describes"
    );
    assert_eq!(
        format!("{}", Unservable::BlockTooLarge { log: u32::MAX }),
        "blocks of 2^4294967295 bytes, over the 4096 this server reads"
    );
    assert_eq!(
        format!("{}", Unservable::Incompatible { features: 0x10 | 0x8000 | 0x80_0000 }),
        "features this server does not read: meta_bg, inline_data, 0x800000"
    );
    assert_eq!(
        format!("{}", Unservable::NotWritable { ro_compat: 0x400 | 0x10, lacking: 0x40 }),
        "it can only be read here: this server does not keep its uninit_bg, metadata_csum; it lacks \
         extent, which this server's writes make"
    );
}

/// **A feature this server cannot read refuses a filesystem, and one its writes cannot keep refuses
/// a writable mount**, as Linux refuses them: a checksum a write left stale makes the filesystem
/// corrupt to `e2fsck` and Linux. A compatible feature, a clean journal's, refuses nothing.
#[test]
fn a_feature_this_server_cannot_keep_refuses_a_writable_mount_and_one_it_cannot_read_any() {
    use crate::ReadOnly;
    use crate::test_support::RwImage;
    let with = |word: usize, set: u32, clear: u32| {
        let mut img = fixture(4096, b"g\n");
        let at = 1024 + word;
        let v = u32::from_le_bytes(img[at..at + 4].try_into().unwrap());
        img[at..at + 4].copy_from_slice(&((v | set) & !clear).to_le_bytes());
        RwImage(std::cell::RefCell::new(img))
    };
    for ro_compat in [0x400, 0x10, 0x200, 0x100] {
        let rw = with(100, ro_compat, 0);
        assert_eq!(check_device(&rw), Err(Unservable::NotWritable { ro_compat, lacking: 0 }), "{ro_compat:#x}");
        assert_eq!(check_device(&ReadOnly(&rw)), Ok(()), "{ro_compat:#x}, read-only");
    }
    for lacking in [0x2, 0x40] {
        let rw = with(96, 0, lacking);
        assert_eq!(check_device(&rw), Err(Unservable::NotWritable { ro_compat: 0, lacking }), "{lacking:#x}");
        assert_eq!(check_device(&ReadOnly(&rw)), Ok(()), "{lacking:#x}, read-only");
    }
    assert_eq!(check_device(&ReadOnly(&with(96, 0x4, 0))), Err(Unservable::NeedsRecovery));
    for features in [0x10, 0x8000, 0x2_0000] {
        assert_eq!(check_device(&ReadOnly(&with(96, features, 0))), Err(Unservable::Incompatible { features }));
    }
    assert_eq!(check_device(&with(92, 0x4, 0)), Ok(()), "has_journal, its journal clean");
    let made = RwImage(std::cell::RefCell::new(vec![0u8; 4096 * 4096]));
    let params = crate::mkfs::Params {
        blocks: 4096,
        block_size: 4096,
        bytes_per_inode: 16384,
        uuid: [7; 16],
        label: [0; 16],
        now: 0,
    };
    crate::mkfs::format(&made, &params, &mut |_, _| {}).unwrap();
    assert_eq!(check_device(&made), Ok(()), "what mkfs makes");
}

/// **A rename to a name its file has already does nothing**, as POSIX's does: a file under two
/// names with one link, as a crash under an older writer left one, would lose that link to the
/// rename and come back as an orphan for the server to free while a name still reaches it.
#[test]
fn a_rename_to_a_name_its_file_has_already_does_nothing() {
    use super::{EXT4_FT_REG_FILE, dir_insert, read_superblock, rename_path, resolve_path_ino};
    let rw = crate::test_support::RwImage(std::cell::RefCell::new(fixture(4096, b"g\n")));
    let sb = read_superblock(&rw).unwrap();
    let (sys, mut dir) = resolve_path_ino(&rw, &sb, b"/system").unwrap();
    let (ino, _) = resolve_path_ino(&rw, &sb, b"/system/current-generation").unwrap();
    dir_insert(&rw, &sb, sys, &mut dir, b"twin", ino, EXT4_FT_REG_FILE, 0).unwrap();
    assert_eq!(rename_path(&rw, b"/system/twin", b"/system/current-generation", true, 0), Ok(None));
    assert_eq!(resolve_path_ino(&rw, &sb, b"/system/twin").map(|(i, _)| i), Ok(ino), "both names stay");
}

/// **The allocator keeps to the filesystem and to its counts**: a run stops at a short last group's
/// end where another tool left the bitmap's spare bits clear; a group whose count says fewer blocks
/// free than its bitmap gives no more than the count; and a run freed twice is counted free once.
#[test]
fn the_allocator_keeps_to_the_filesystem_and_to_its_counts() {
    use super::{alloc_run, free_run, read_superblock};
    use crate::test_support::{RwImage, fixture_blocks};
    use crate::{rd_u16, rd_u32};
    let rw = RwImage(std::cell::RefCell::new(fixture_blocks(1024, 24576, 0, b"g\n")));
    let sb = read_superblock(&rw).unwrap();
    let gd = |g: u32| sb.group_desc_offset(g) as usize;
    let group = |b: u64| ((b - sb.first_data_block as u64) / sb.blocks_per_group as u64) as u32;

    let (spare, last) = (sb.blocks_in_group(2), sb.blocks_count - 1);
    let bitmap = rd_u32(&rw.0.borrow(), gd(2)) as usize * 1024;
    rw.0.borrow_mut()[bitmap + spare / 8] &= !(1 << (spare % 8));
    assert_eq!(alloc_run(&rw, &sb, last, 4), Ok((last, 1)), "nothing past the filesystem's end");

    rw.0.borrow_mut()[gd(1) + 12..gd(1) + 14].copy_from_slice(&2u16.to_le_bytes());
    let (phys, len) = alloc_run(&rw, &sb, sb.group_start(1) + 100, 10).unwrap();
    assert_eq!((group(phys), len, rd_u16(&rw.0.borrow(), gd(1) + 12)), (1, 2, 0), "no more than counted");

    let (phys, len) = alloc_run(&rw, &sb, sb.group_start(0) + 500, 3).unwrap();
    let free = || (rd_u16(&rw.0.borrow(), gd(group(phys)) + 12), rd_u32(&rw.0.borrow(), 1024 + 12));
    free_run(&rw, &sb, phys, len).unwrap();
    let once = free();
    free_run(&rw, &sb, phys, len).unwrap();
    assert_eq!(free(), once, "freed twice, counted once");
}

/// **A rename refuses a file whose link count it would take past its range**, before it writes: the
/// count it adds while the file has both names would wrap a full one to nothing.
#[test]
fn a_rename_refuses_a_file_whose_links_are_full() {
    use super::{inode_offset, read_superblock, rename_path, resolve_path_ino};
    let rw = crate::test_support::RwImage(std::cell::RefCell::new(fixture(4096, b"g\n")));
    let sb = read_superblock(&rw).unwrap();
    let (ino, _) = resolve_path_ino(&rw, &sb, b"/system/current-generation").unwrap();
    let at = inode_offset(&rw, &sb, ino).unwrap() as usize + 26;
    rw.0.borrow_mut()[at..at + 2].copy_from_slice(&u16::MAX.to_le_bytes());
    let before = rw.0.borrow().clone();
    assert_eq!(rename_path(&rw, b"/system/current-generation", b"/system/g", false, 0), Err(FsError::Corrupt));
    assert!(*rw.0.borrow() == before, "refused, it wrote nothing");
}

/// **A directory Linux indexed is made linear before a name goes in**, as an older writer makes
/// one: an insert wrote over the hash tree's root, and `e2fsck` called the directory corrupt.
#[test]
fn a_directory_linux_indexed_is_made_linear_before_a_name_goes_in() {
    use super::{INDEX_FL, create_file, mkdir_at, read_superblock, rename_path, resolve_path_ino};
    use crate::test_support::{RwImage, assert_e2fsck_clean};
    let dir = std::env::temp_dir().join(format!("nitrox-ext4-htree-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("stage/big")).unwrap();
    for i in 0..400 {
        std::fs::write(dir.join(format!("stage/big/a file with a longish name {i}")), b"x").unwrap();
    }
    let img = dir.join("img");
    let mke2fs = std::process::Command::new("mke2fs")
        .args(["-q", "-F", "-t", "ext4", "-O", "^has_journal,^64bit,^metadata_csum,^resize_inode", "-d"])
        .args([dir.join("stage").as_os_str(), img.as_os_str(), "4096".as_ref()])
        .status();
    assert!(mke2fs.unwrap().success());
    let indexed = std::process::Command::new("e2fsck").args(["-fyD".as_ref(), img.as_os_str()]).status();
    assert!(indexed.unwrap().code().is_some_and(|c| c <= 1), "e2fsck -D indexes the directory");
    let rw = RwImage(std::cell::RefCell::new(std::fs::read(&img).unwrap()));
    let _ = std::fs::remove_dir_all(&dir);
    let sb = read_superblock(&rw).unwrap();
    let flags = || crate::rd_u32(&resolve_path_ino(&rw, &sb, b"/big").unwrap().1, 32);
    assert_ne!(flags() & INDEX_FL, 0, "Linux's tools indexed it");
    let (big, _) = resolve_path_ino(&rw, &sb, b"/big").unwrap();
    create_file(&rw, b"/big", b"new", 0).unwrap();
    assert_eq!(flags() & INDEX_FL, 0, "made linear");
    mkdir_at(&rw, big, b"made", 0).unwrap();
    rename_path(&rw, b"/big/a file with a longish name 7", b"/big/renamed", false, 0).unwrap();
    assert_e2fsck_clean(&rw.0.into_inner(), "after names went into an indexed directory");
}

/// **A rename keeps the type its record had**: a symlink renamed within a directory and across
/// directories stays a symlink's record. The type was made from the mode, as a directory or a file.
#[test]
fn a_rename_keeps_the_type_its_record_had() {
    use super::{read_superblock, rename_at, rename_path, resolve_path_ino};
    use crate::test_support::{RwImage, assert_e2fsck_clean};
    let dir = std::env::temp_dir().join(format!("nitrox-ext4-symlink-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("stage/d")).unwrap();
    std::os::unix::fs::symlink("d", dir.join("stage/link")).unwrap();
    std::os::unix::fs::symlink("..", dir.join("stage/d/up")).unwrap();
    let img = dir.join("img");
    let made = std::process::Command::new("mke2fs")
        .args(["-q", "-F", "-t", "ext4", "-O", "^has_journal,^64bit,^metadata_csum,^resize_inode", "-d"])
        .args([dir.join("stage").as_os_str(), img.as_os_str(), "4096".as_ref()])
        .status();
    assert!(made.unwrap().success());
    let rw = RwImage(std::cell::RefCell::new(std::fs::read(&img).unwrap()));
    let _ = std::fs::remove_dir_all(&dir);
    rename_at(&rw, 2, b"link", b"link2", 0).unwrap();
    rename_path(&rw, b"/d/up", b"/up2", false, 0).unwrap();
    let sb = read_superblock(&rw).unwrap();
    assert!(resolve_path_ino(&rw, &sb, b"/link2").is_ok());
    assert_e2fsck_clean(&rw.0.into_inner(), "after renaming symlinks");
}

/// **A rename of a name that is not there is `NotFound`**, to itself too; and **a new name too long
/// is refused before anything is written**, though a later insert would refuse it and be undone.
#[test]
fn a_rename_is_refused_before_it_writes() {
    use super::{rename_at, rename_path};
    use crate::test_support::RwImage;
    let rw = RwImage(std::cell::RefCell::new(fixture(4096, b"g\n")));
    assert_eq!(rename_at(&rw, 2, b"missing", b"missing", 0), Err(FsError::NotFound));
    assert_eq!(rename_path(&rw, b"/missing", b"/missing", false, 0), Err(FsError::NotFound));
    let none = crate::test_support::Failing::after(rw.0.into_inner(), 0);
    let long = [b"/system/" as &[u8], &[b'a'; 256]].concat();
    let refused = rename_path(&none, b"/system/current-generation", &long, false, 0);
    assert_eq!(refused, Err(FsError::Unsupported), "refused before a write, which would have answered Io");
}

/// **A directory's count of 1 means too many subdirectories to count**, under `dir_nlink`: a mkdir
/// or an rmdir in it leaves 1, a count past 65,000 becomes 1, and an rmdir never takes a directory
/// below its own two. An rmdir took 1 to 0, and a live directory then answered `NotFound`.
#[test]
fn a_directory_count_of_one_means_too_many_to_count() {
    use super::{inode_offset, mkdir_at, read_superblock, resolve_path_ino, rmdir_at};
    use crate::test_support::RwImage;
    let rw = RwImage(std::cell::RefCell::new(fixture(4096, b"g\n")));
    let sb = read_superblock(&rw).unwrap();
    mkdir_at(&rw, 2, b"d", 0).unwrap();
    let d = resolve_path_ino(&rw, &sb, b"/d").unwrap().0;
    mkdir_at(&rw, d, b"s", 0).unwrap();
    let at = inode_offset(&rw, &sb, d).unwrap() as usize + 26;
    let links = |rw: &RwImage| u16::from_le_bytes([rw.0.borrow()[at], rw.0.borrow()[at + 1]]);
    let set = |rw: &RwImage, n: u16| rw.0.borrow_mut()[at..at + 2].copy_from_slice(&n.to_le_bytes());
    set(&rw, 1);
    rmdir_at(&rw, d, b"s", 0, |_| ()).unwrap();
    assert_eq!(links(&rw), 1, "an rmdir leaves 1");
    mkdir_at(&rw, d, b"t", 0).unwrap();
    assert_eq!(links(&rw), 1, "a mkdir leaves 1");
    set(&rw, 65_000);
    mkdir_at(&rw, d, b"u", 0).unwrap();
    assert_eq!(links(&rw), 1, "past 65,000, 1");
    set(&rw, 2);
    rmdir_at(&rw, d, b"u", 0, |_| ()).unwrap();
    assert_eq!(links(&rw), 2, "never below a directory's own two");
}

/// **An unlink of one of two names answers what it did**, with each write refused once: `Ok`
/// exactly when the name is gone, though the other name's count is a write after the removal.
#[test]
fn an_unlink_of_one_of_two_names_answers_what_it_did() {
    use super::{EXT4_FT_REG_FILE, dir_insert, inode_offset, read_superblock, resolve_path_ino, unlink_at};
    use crate::test_support::{Failing, RwImage};
    let rw = RwImage(std::cell::RefCell::new(fixture(4096, b"g\n")));
    let sb = read_superblock(&rw).unwrap();
    let (sys, mut dir) = resolve_path_ino(&rw, &sb, b"/system").unwrap();
    let (ino, _) = resolve_path_ino(&rw, &sb, b"/system/current-generation").unwrap();
    dir_insert(&rw, &sb, sys, &mut dir, b"twin", ino, EXT4_FT_REG_FILE, 0).unwrap();
    let at = inode_offset(&rw, &sb, ino).unwrap() as usize + 26;
    rw.0.borrow_mut()[at..at + 2].copy_from_slice(&2u16.to_le_bytes());
    let base = rw.0.into_inner();
    for n in 0..3 {
        let f = Failing::once(base.clone(), n);
        let answered = unlink_at(&f, sys, b"twin", 0);
        let gone = resolve_path_ino(&f.image, &sb, b"/system/twin").is_err();
        assert_eq!(answered.map(|o| o.is_none()), if gone { Ok(true) } else { Err(FsError::Io) }, "write {n}");
    }
}
