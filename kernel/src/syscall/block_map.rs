//! The block map a block-file reply carries, checked for what the page cache can use.

use crate::mm::PAGE_SIZE;
use crate::syscall::error::KError;

/// **A block-file reply's map**: its header and runs, or `KernelError` for one the page cache
/// cannot use: a body too short for its runs, and
///
/// - **a block size that is not a power of two from a sector to a page** (the fill divides by
///   it; `0` would divide by zero);
/// - **a run whose ends do not fit a `u64`**: its file blocks' end, its device blocks' end, and
///   that end in bytes. The fill and write-back add within a run and multiply by the block
///   size, and a kernel built with overflow checks panics on either;
/// - **a run that does not begin and end on a page boundary**.
///
/// `OutOfMemory` if the runs cannot be held.
pub(super) fn block_map(
    body: &[u8],
) -> Result<(crate::rsproto::FileBlocksHeader, crate::libkern::KVec<crate::object::BlockRun>), KError> {
    use crate::object::BlockRun;
    use crate::rsproto::{file_blocks_reply_header, file_blocks_run};
    let header = file_blocks_reply_header(body).ok_or(KError::KernelError)?;
    let bs = header.block_size;
    if !bs.is_power_of_two() || !(512..=PAGE_SIZE as u32).contains(&bs) {
        return Err(KError::KernelError);
    }
    let mut runs = crate::libkern::KVec::new();
    runs.try_reserve(header.run_count as usize).map_err(|_| KError::OutOfMemory)?;
    for i in 0..header.run_count as usize {
        let (file_block, device_lba, length, flags) = file_blocks_run(body, i).ok_or(KError::KernelError)?;
        let fits = file_block.checked_add(length as u64).is_some()
            && device_lba.checked_add(length as u64).and_then(|end| end.checked_mul(bs as u64)).is_some();
        // A page is filled and written back as one range from its first block, so in one run.
        let per_page = (PAGE_SIZE as u32 / bs) as u64;
        let whole_pages =
            length == 0 || (file_block.is_multiple_of(per_page) && (length as u64).is_multiple_of(per_page));
        if !fits || !whole_pages {
            return Err(KError::KernelError);
        }
        let _ = runs.try_push(BlockRun { file_block, device_lba, length, flags });
    }
    Ok((header, runs))
}
