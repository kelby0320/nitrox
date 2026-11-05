use super::*;
use libkern::{RawHandle, Rights};
use libos::mock;

#[test]
fn zeroes_split_into_a_ragged_head_whole_blocks_and_a_ragged_tail() {
    const B: u64 = IO_BLOCK as u64;
    assert_eq!(split_zeroes(B, 32 * B), (B..B, B..33 * B, 33 * B..33 * B), "aligned: whole blocks alone");
    assert_eq!(split_zeroes(B + 100, 2 * B), (B + 100..2 * B, 2 * B..3 * B, 3 * B..3 * B + 100));
    assert_eq!(split_zeroes(100, 200), (100..300, 300..300, 300..300), "inside one block: a head alone");
    assert_eq!(split_zeroes(B - 1, 2), (B - 1..B, B..B, B..B + 1), "across one boundary, no whole block");
    assert_eq!(split_zeroes(5 * B, 0), (5 * B..5 * B, 5 * B..5 * B, 5 * B..5 * B));
    for (offset, len) in [(0, 1), (4095, 4097), (12_345, 1_000_000), (B, B - 1)] {
        let (h, w, t) = split_zeroes(offset, len);
        assert!(h.start == offset && h.end == w.start && w.end == t.start && t.end == offset + len);
        assert!(w.start % B == 0 && w.end % B == 0 && h.end - h.start < B && t.end - t.start < B);
    }
}

#[test]
fn a_request_is_covered_by_aligned_chunks_of_at_most_a_span() {
    for (offset, len) in [(0, 512), (100, 70_000), (512, SPAN), (511, 2), (1000, SPAN + 1), (4096, 3 * SPAN), (7, 1)] {
        let mut next = offset - offset % SECTOR;
        let mut covered = 0usize;
        SectorDisk::chunks(offset, len, |at, n, from, take| {
            assert_eq!(at, next, "contiguous");
            assert_eq!(at % SECTOR, 0);
            assert_eq!(n as u64 % SECTOR, 0);
            assert!(n <= SPAN && from + take <= n && take > 0);
            assert_eq!(at + from as u64, offset + covered as u64, "the request's bytes in order");
            covered += take;
            next = at + n as u64;
            Ok(())
        })
        .unwrap();
        assert_eq!(covered, len, "({offset}, {len})");
    }
}

#[test]
fn a_window_refuses_past_its_end_and_moves_sector_aligned_spans() {
    let (base, len) = (1 << 20, 10 * 512);
    assert_eq!(window(base, len, WINDOW_SPAN, 0, len as usize + 1), Err(FsError::Io));
    assert_eq!(window(base, len, WINDOW_SPAN, len, 1), Err(FsError::Io), "the first byte past it");
    assert_eq!(window(base, len, WINDOW_SPAN, u64::MAX, 2), Err(FsError::Io), "no overflow");
    assert_eq!(window(base, len, WINDOW_SPAN, len - 1, 1), Ok((base + len - 512, 512, 511)));
    assert_eq!(window(base, len, WINDOW_SPAN, 100, 1000), Ok((base, 1536, 100)), "three sectors");
    let big = 4 * WINDOW_SPAN as u64;
    let (start, span, intra) = window(base, big, WINDOW_SPAN, 3, big as usize - 3).unwrap();
    assert_eq!((start, span, intra), (base, WINDOW_SPAN as u64, 3), "at most a scratch's worth");
}

const TIMED_OUT: i64 = libkern::KError::TimedOut as i64;

fn script_waits(script: &[Option<i64>]) {
    sys::with(|f| f.waits.extend(script.iter().copied()));
}

fn submitted() -> Vec<libkern::IoOp> {
    mock::submitted()
}

/// A device handle the disk owns, as the setup message brings one.
fn device() -> u64 {
    mock::device()
}

/// A device of the model's, owned, as a setup message hands a server one.
fn owned_device() -> Handle<Resource, ReadWrite> {
    // SAFETY: fresh from the model and named nowhere else.
    unsafe { Handle::from_raw(RawHandle(device()), Rights::empty()) }
}

/// A disk whose first wait times out answers `Io` to that write, and to every later transfer
/// without submitting one, and leaves the scratch as the failed transfer left it.
fn assert_lost_after_a_timeout<D: BlockReader + BlockWriter>(disk: &D, scratch: &Scratch) {
    script_waits(&[Some(TIMED_OUT)]);
    assert_eq!(disk.write_at(0, &[1; 4096]), Err(FsError::Io));
    assert_eq!(submitted().len(), 1);
    let snapshot = scratch.bytes();
    assert_eq!(snapshot[..4096], [1; 4096], "the failed write was staged in the scratch");

    assert_eq!(disk.write_at(4096, &[2; 4096]), Err(FsError::Io));
    assert_eq!(disk.read_at(0, &mut [0; 4096]), Err(FsError::Io));
    assert_eq!(submitted().len(), 1, "nothing is submitted through a lost scratch");
    assert_eq!(scratch.bytes(), snapshot, "nothing is copied into it");
}

/// With every wait succeeding the same calls move the bytes.
fn assert_moves_bytes<D: BlockReader + BlockWriter>(disk: &D) {
    assert_eq!(disk.write_at(0, &[1; 4096]), Ok(()));
    assert_eq!(disk.write_at(4096, &[2; 4096]), Ok(()));
    let mut back = [0u8; 4096];
    assert_eq!(disk.read_at(0, &mut back), Ok(()));
    assert_eq!(back, [1; 4096]);
    assert_eq!(sys::with(|f| f.device[4096..8192].to_vec()), [2; 4096]);
}

impl Scratch {
    fn bytes(&self) -> Vec<u8> {
        let view = self.view.borrow();
        let mut all = vec![0; view.len()];
        view.read_into(0, &mut all).unwrap();
        all
    }
}

/// **A transfer that completes failed, or short, answers `Io`** but leaves the disk usable: its
/// buffer is the disk's again, so the next transfer goes.
#[test]
fn a_failed_or_short_transfer_is_an_error_but_not_a_loss() {
    for (status, moved) in [(libkern::KError::IoError as i32, 4096), (0, 512)] {
        let disk = sector_disk();
        sys::with(|f| f.completions.push_back((status, moved)));
        assert_eq!(disk.write_at(0, &[1; 4096]), Err(FsError::Io), "status {status}, {moved} moved");
        assert_eq!(disk.write_at(0, &[1; 4096]), Ok(()), "status {status}, {moved} moved: then usable");
    }
}

fn disk() -> Disk {
    Disk::over(owned_device()).ok().unwrap()
}

fn sector_disk() -> SectorDisk {
    SectorDisk::over(owned_device()).ok().unwrap()
}

/// The test's own device handle, lent to a window that must leave it open.
fn borrowed_device(raw: u64) -> Device<'static> {
    // SAFETY: the mock's device, open for the test, which outlives the window.
    unsafe { Device::from_raw(RawHandle(raw), Rights::empty()) }
}

fn partition() -> PartitionIo<'static> {
    PartitionIo::window(borrowed_device(device()), 0, 1 << 20).unwrap()
}

#[test]
fn a_disk_is_lost_after_a_wait_that_fails() {
    let disk = disk();
    assert_lost_after_a_timeout(&disk, &disk.scratch);
}

#[test]
fn a_sector_disk_is_lost_after_a_wait_that_fails() {
    let disk = sector_disk();
    assert_lost_after_a_timeout(&disk, &disk.scratch);
}

#[test]
fn a_partition_is_lost_after_a_wait_that_fails() {
    let disk = partition();
    assert_lost_after_a_timeout(&disk, &disk.scratch);
}

#[test]
fn every_disk_moves_bytes_while_its_waits_succeed() {
    assert_moves_bytes(&disk());
    assert_moves_bytes(&sector_disk());
    assert_moves_bytes(&partition());
}

#[test]
fn zeroes_are_not_written_through_a_lost_scratch() {
    let disk = disk();
    script_waits(&[Some(TIMED_OUT)]);
    assert_eq!(disk.write_at(0, &[1; 4096]), Err(FsError::Io));
    assert_eq!(disk.write_zeroes(0, 8192), Err(FsError::Io));
    assert_eq!(submitted().len(), 1, "the zeroes were not submitted");
}

#[test]
fn a_zero_write_that_is_abandoned_loses_the_scratch_too() {
    let disk = disk();
    script_waits(&[Some(TIMED_OUT)]);
    assert_eq!(disk.write_zeroes(0, 8192), Err(FsError::Io));
    let sent = submitted();
    assert_eq!((sent.len(), sent[0].buffer), (1, disk.zeroes.raw().0), "the zero span was what went");
    assert_eq!(disk.write_at(0, &[3; 4096]), Err(FsError::Io));
    assert_eq!(submitted().len(), 1, "the later write is refused, not submitted");
}

#[test]
fn zeroes_reach_the_device_while_waits_succeed() {
    let disk = disk();
    assert_eq!(disk.write_at(0, &[9; 8192]), Ok(()));
    assert_eq!(disk.write_zeroes(0, 8192), Ok(()));
    assert_eq!(sys::with(|f| f.device[..8192].to_vec()), [0; 8192]);
}

#[test]
fn a_scratch_that_cannot_be_mapped_closes_its_handle() {
    let before = mock::open_handles();
    sys::with(|f| f.maps.push_back(Some(-1)));
    assert!(Scratch::new(4096).is_none());
    assert_eq!(mock::open_handles(), before);
    assert_eq!(mock::mappings(), 0);
}

#[test]
fn a_disk_whose_zero_span_fails_gives_back_its_scratch_and_its_device() {
    let before = mock::open_handles();
    sys::with(|f| f.creates.extend([None, Some(-1)]));
    assert!(Disk::over(owned_device()).is_err());
    assert_eq!(mock::open_handles(), before, "the device the disk was handed is closed with it");
    assert_eq!(mock::mappings(), 0);
}

#[test]
fn dropping_a_disk_unmaps_its_scratch_and_closes_it_its_zero_span_and_its_device() {
    let before = mock::open_handles();
    let disk = disk();
    assert_eq!((mock::open_handles(), mock::mappings()), (before + 3, 1));
    drop(disk);
    assert_eq!((mock::open_handles(), mock::mappings(), mock::stray_closes()), (before, 0, 0));
}

#[test]
fn dropping_a_sector_disk_unmaps_its_scratch_and_closes_it_and_its_device() {
    let before = mock::open_handles();
    drop(sector_disk());
    assert_eq!((mock::open_handles(), mock::mappings(), mock::stray_closes()), (before, 0, 0));
}

/// A window borrows its device: the caller's handle is still open when the window is gone.
#[test]
fn dropping_a_partition_unmaps_its_scratch_and_closes_it_but_not_its_device() {
    let dev = device();
    let before = mock::open_handles();
    drop(PartitionIo::window(borrowed_device(dev), 0, 1 << 20).unwrap());
    assert_eq!((mock::open_handles(), mock::mappings(), mock::stray_closes()), (before, 0, 0));
    assert!(mock::is_open(dev));
}

/// What the device is asked to do reaches libos's model of the kernel: the submit names the
/// device's handle and the scratch object, and a refusal of it is `Io`, not a loss.
#[test]
fn a_transfer_is_submitted_to_the_device_through_the_scratch() {
    let disk = sector_disk();
    assert_eq!(disk.read_at(1000, &mut [0; 100]), Ok(()));
    let sent = submitted();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].opcode, sent[0].buffer, sent[0].offset, sent[0].length),
        (libkern::IO_OPCODE_READ, disk.scratch.mem.raw().0, 512, 1024)
    );
    mock::fail_next_submit(libkern::KError::IoError as i32);
    assert_eq!(disk.read_at(0, &mut [0; 1]), Err(FsError::Io));
    assert_eq!(disk.read_at(0, &mut [0; 1]), Ok(()), "a refused submit leaves the scratch usable");
}

/// **A transfer the device completes `TimedOut` is finished**, however its status reads: only a wait
/// that fails leaves the scratch the device's.
#[test]
fn a_transfer_the_device_fails_keeps_the_scratch() {
    let disk = sector_disk();
    sys::with(|f| f.completions.push_back((libkern::KError::TimedOut as i32, 0)));
    assert_eq!(disk.write_at(0, &[1; 512]), Err(FsError::Io));
    assert_eq!(disk.write_at(0, &[1; 512]), Ok(()));
    assert_eq!(submitted().len(), 2, "the scratch was not lost");
}

/// A wait refused for good is a transfer that may still run, its scratch lost.
#[test]
fn a_transfer_whose_wait_is_refused_loses_the_scratch() {
    let disk = sector_disk();
    mock::fail_next_wait(libkern::KError::InvalidHandle as i64);
    sys::with(|f| f.completions.push_back((0, 512)));
    assert_eq!(disk.write_at(0, &[1; 512]), Err(FsError::Io));
    assert_eq!(disk.write_at(0, &[1; 512]), Err(FsError::Io));
    assert_eq!(submitted().len(), 1, "nothing is submitted through a lost scratch");
}

/// A completion is reported as it came, whether or not it succeeded: the device is done with the
/// buffer either way.
#[test]
fn a_finished_transfer_reports_its_status_and_bytes_moved() {
    let dev = device();
    let op =
        libkern::IoOp { opcode: libkern::IO_OPCODE_READ, flags: 0, buffer: 0, buf_offset: 0, offset: 0, length: 8 };
    for (status, moved) in [(0, 8), (libkern::KError::IoError as i32, 0)] {
        mock::script_next(status, moved, true);
        let sent = libos::Op::submit(libkern::RawHandle(dev), &op).unwrap();
        assert_eq!(wait_transfer(sent), Some((status, moved)));
    }
    assert_eq!(mock::stray_closes(), 0);
}
