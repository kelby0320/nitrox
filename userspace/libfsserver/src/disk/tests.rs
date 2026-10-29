use super::*;

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

fn submitted() -> Vec<sys::Submit> {
    sys::with(|f| f.submitted.clone())
}

fn created() -> Vec<u64> {
    sys::with(|f| f.created.clone())
}

/// A disk whose first wait times out answers `Io` to that write, and to every later transfer
/// without submitting one, and leaves the scratch as the failed transfer left it.
fn assert_lost_after_a_timeout<D: BlockReader + BlockWriter>(disk: &D) {
    let scratch = created()[0];
    script_waits(&[Some(TIMED_OUT)]);
    assert_eq!(disk.write_at(0, &[1; 4096]), Err(FsError::Io));
    assert_eq!(submitted().len(), 1);
    let snapshot = sys::peek(scratch);
    assert_eq!(snapshot[..4096], [1; 4096], "the failed write was staged in the scratch");

    assert_eq!(disk.write_at(4096, &[2; 4096]), Err(FsError::Io));
    assert_eq!(disk.read_at(0, &mut [0; 4096]), Err(FsError::Io));
    assert_eq!(submitted().len(), 1, "nothing is submitted through a lost scratch");
    assert_eq!(sys::peek(scratch), snapshot, "nothing is copied into it");
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
    Disk::new(7).ok().unwrap()
}

fn sector_disk() -> SectorDisk {
    SectorDisk::new(7).ok().unwrap()
}

fn partition() -> PartitionIo {
    PartitionIo::new(7, 0, 1 << 20).unwrap()
}

#[test]
fn a_disk_is_lost_after_a_wait_that_fails() {
    assert_lost_after_a_timeout(&disk());
}

#[test]
fn a_sector_disk_is_lost_after_a_wait_that_fails() {
    assert_lost_after_a_timeout(&sector_disk());
}

#[test]
fn a_partition_is_lost_after_a_wait_that_fails() {
    assert_lost_after_a_timeout(&partition());
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
    assert_eq!((sent.len(), sent[0].buffer), (1, created()[1]), "the zero span was what went");
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
    sys::with(|f| f.maps.push_back(Some(-1)));
    assert!(Scratch::new(4096).is_none());
    assert_eq!(sys::with(|f| f.closes.clone()), created());
    assert_eq!(created().len(), 1);
}

#[test]
fn a_disk_whose_zero_span_fails_gives_back_its_scratch() {
    sys::with(|f| f.creates.extend([None, Some(-1)]));
    assert!(Disk::new(7).is_err());
    let made = created();
    assert_eq!(sys::with(|f| f.closes.clone()), made);
    assert_eq!(made.len(), 1);
    assert_eq!(sys::with(|f| f.unmaps.iter().map(|u| u.1).collect::<Vec<_>>()), [PAGE]);
}

fn closed_and_unmapped(handles: usize, span: u64) {
    let mut closes = sys::with(|f| f.closes.clone());
    closes.sort();
    assert_eq!(closes, created());
    assert_eq!(closes.len(), handles);
    assert_eq!(sys::with(|f| f.unmaps.iter().map(|u| u.1).collect::<Vec<_>>()), [span]);
}

#[test]
fn dropping_a_disk_unmaps_its_scratch_and_closes_it_and_its_zero_span() {
    drop(disk());
    closed_and_unmapped(2, PAGE);
}

#[test]
fn dropping_a_sector_disk_unmaps_its_scratch_and_closes_it() {
    drop(sector_disk());
    closed_and_unmapped(1, SPAN as u64);
}

#[test]
fn dropping_a_partition_unmaps_its_scratch_and_closes_it() {
    drop(partition());
    closed_and_unmapped(1, WINDOW_SPAN as u64);
}
