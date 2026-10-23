use super::*;

#[test]
fn every_op_has_its_command_and_nothing_else_does() {
    use crate::io::irp::IrpOp;
    assert_eq!(ata_command(IrpOp::Read as u32, 0xEA), Some((ATA_READ_DMA_EXT, false)));
    assert_eq!(ata_command(IrpOp::Write as u32, 0xEA), Some((ATA_WRITE_DMA_EXT, true)));
    assert_eq!(ata_command(IrpOp::Flush as u32, 0xEA), Some((0xEA, false)), "the drive's flush");
    assert_eq!(ata_command(IrpOp::Flush as u32, 0xE7), Some((0xE7, false)));
    assert_eq!(ata_command(3, 0xEA), None, "not a read, which a count of 0 makes 65,536 sectors");
}

#[test]
fn a_drive_with_flush_cache_ext_is_flushed_with_it() {
    assert_eq!(flush_command(1 << 13), ATA_FLUSH_CACHE_EXT);
    assert_eq!(flush_command(0xFFFF), ATA_FLUSH_CACHE_EXT);
    assert_eq!(flush_command(!(1 << 13)), ATA_FLUSH_CACHE, "every other bit set, not this one");
    assert_eq!(flush_command(0), ATA_FLUSH_CACHE);
}

#[test]
fn the_prdt_capacity_is_what_fits_in_a_command_table() {
    assert_eq!(MAX_PRDT_ENTRIES, 248);
    assert!(PRDT_OFFSET + MAX_PRDT_ENTRIES as usize * PRDT_ENTRY <= CMD_TABLE_BYTES);
    assert!(PRDT_OFFSET + (MAX_PRDT_ENTRIES as usize + 1) * PRDT_ENTRY > CMD_TABLE_BYTES);
}

/// What that capacity is worth as a transfer, pinning the figure in `deferred-decisions.md`.
#[test]
fn one_command_covers_just_under_a_megabyte() {
    let bytes = MAX_PRDT_ENTRIES as usize * crate::mm::PAGE_SIZE;
    assert_eq!(bytes, 1_015_808);
    assert_eq!(bytes / 1024, 992);
}

/// Lay `text` into an IDENTIFY field as a drive does: byte-swapped within each word,
/// space-padded.
fn put_field(words: &mut [u16; 256], first: usize, width_words: usize, text: &[u8]) {
    let mut bytes = [b' '; 40];
    bytes[..text.len()].copy_from_slice(text);
    for i in 0..width_words {
        words[first + i] = u16::from(bytes[i * 2]) << 8 | u16::from(bytes[i * 2 + 1]);
    }
}

fn identity_of(model: &[u8], serial: &[u8]) -> ([u8; MAX_DEVICE_NAME], usize) {
    let mut words = [0u16; 256];
    put_field(&mut words, 27, 20, model);
    put_field(&mut words, 10, 10, serial);
    let mut out = [0u8; MAX_DEVICE_NAME];
    // SAFETY: `words` is a full 256-word IDENTIFY result, which is the contract.
    let n = unsafe { format_identity(words.as_ptr(), &mut out) };
    (out, n)
}

fn assert_identity(model: &[u8], serial: &[u8], want: &[u8]) {
    let (out, n) = identity_of(model, serial);
    assert_eq!(
        &out[..n],
        want,
        "model {:?} serial {:?} produced {:?}",
        core::str::from_utf8(model),
        core::str::from_utf8(serial),
        core::str::from_utf8(&out[..n])
    );
}

/// **Padding is trimmed at both ends**, because the standard does not say which end a drive
/// pads.
#[test]
fn a_right_justified_field_loses_its_padding_too() {
    assert_identity(b"ST1000LM035-1RK172", b"            WDELGVZ3", b"ST1000LM035-1RK172 (WDELGVZ3)");
    assert_identity(b"QEMU HARDDISK", b"QM00001", b"QEMU HARDDISK (QM00001)");
    // Padding on both ends of both fields, interior spaces kept: `QEMU HARDDISK` has one.
    assert_identity(b"  A B  ", b"   C D  ", b"A B (C D)");
}

/// A field that is nothing but padding is empty, and a disk with no serial is named by its
/// model alone.
#[test]
fn an_all_padding_field_comes_back_empty() {
    assert_identity(b"ST1000LM035", b"                    ", b"ST1000LM035");
    assert_identity(b"                                        ", b"", b"");
}

#[test]
fn a_rescan_and_an_eject_are_refused_without_touching_a_disk() {
    use crate::io::irp::{IrpBuffer, IrpOp};
    crate::mm::test_support::init_global_heap();
    crate::dpc::init().unwrap();
    for op in [IrpOp::Rescan, IrpOp::Eject] {
        let mut irp = Irp::new_block(op, core::ptr::null(), 0, 0, IrpBuffer::NONE, core::ptr::null_mut(), 0);
        submit(&mut irp, core::ptr::null_mut());
        crate::dpc::run_pending();
        assert_eq!(irp.status, crate::syscall::error::KError::Unsupported as i32, "{op:?}");
    }
}

/// A command the HBA says moved less than its length fails, and its pages count as nothing moved:
/// one byte short is a failure, the whole length a success, and a flush moves nothing and asks for
/// nothing.
#[test]
fn a_command_that_moved_less_than_its_length_fails() {
    let io_error = crate::syscall::error::KError::IoError as i32;
    let ok = IrpStatus::Success as i32;
    assert_eq!(completion(false, 8192, 8192), (ok, 8192));
    assert_eq!(completion(false, 8192, 8191), (io_error, 0), "a read stopped a byte early");
    assert_eq!(completion(false, 8192, 0), (io_error, 0));
    assert_eq!(completion(false, 0, 0), (ok, 0), "a flush");
    assert_eq!(completion(true, 8192, 8192), (io_error, 0), "the drive's error, whatever moved");
}
