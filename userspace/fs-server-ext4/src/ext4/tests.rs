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
}
