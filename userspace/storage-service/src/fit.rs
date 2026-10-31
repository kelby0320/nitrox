//! **What fits**: a page of a directory listing in one reply, and a device read in the scratch.

use alloc::string::String;
use alloc::vec::Vec;
use librsproto::RS_MAX_BODY;
use librsproto::file::DirReplyWriter;

/// The entries from `cursor` that fit one `ReadDir` reply, and the cursor to ask from next (`0` at
/// the end). **Sized to `RS_MAX_BODY`**: a body the encoder cannot frame is a reply never sent.
pub fn dir_page(entries: &[(String, u8, u64)], cursor: usize) -> Option<(Vec<u8>, u64)> {
    let mut out = [0u8; RS_MAX_BODY];
    let mut w = DirReplyWriter::new(&mut out)?;
    let mut i = cursor;
    while let Some((name, kind, size)) = entries.get(i) {
        if !w.push(i as u32 + 1, *kind, 0, *size, 0, name.as_bytes()) {
            break;
        }
        i += 1;
    }
    let next = if i >= entries.len() { 0 } else { i as u64 };
    let n = w.finish(next);
    Some((out[..n].to_vec(), next))
}

/// The scratch every device read passes through: enough for a partition table's front in one.
pub const SCRATCH: usize = 64 * 1024;

/// A device read: `span` bytes from `start`, a block boundary, the `want` asked for at `intra`.
#[derive(Debug, PartialEq)]
pub struct ScratchRead {
    pub start: u64,
    pub intra: usize,
    pub want: usize,
    pub span: u64,
}

/// The read bringing the next of `left` bytes at `at` into the scratch, on `block`-byte blocks;
/// `None` for a block that does not divide the scratch, as a device's record may say anything.
pub fn scratch_read(at: u64, left: usize, block: u64) -> Option<ScratchRead> {
    if !(SCRATCH as u64).is_multiple_of(block) {
        return None;
    }
    let start = at / block * block;
    let intra = (at - start) as usize;
    let want = left.min(SCRATCH - intra);
    let span = ((intra + want) as u64).div_ceil(block) * block;
    Some(ScratchRead { start, intra, want, span })
}

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::file::{DIRENT_KIND_FILE, parse_read_dir_reply};
    use librsproto::{OP_FILE_READ_DIR, RS_FLAG_REPLY, encode};
    use std::format;

    #[test]
    fn a_full_gpts_listing_pages_into_replies_that_encode() {
        let names = core::iter::once(String::from("all.tsm")).chain((0..129).map(|n| format!("blk-{n}.tsm")));
        let entries: Vec<_> = names.map(|n| (n, DIRENT_KIND_FILE, 1 << 40)).collect();
        let (mut cursor, mut listed) = (0, Vec::new());
        loop {
            let (body, next) = dir_page(&entries, cursor).unwrap();
            let mut payload = [0u8; 4096 - 24];
            assert!(encode(&mut payload, OP_FILE_READ_DIR, 1, RS_FLAG_REPLY, &body, 0).is_some());
            let (header, page) = parse_read_dir_reply(&body).unwrap();
            assert_eq!(header.next_cursor, next);
            listed.extend(page.map(|e| (e.inode, String::from_utf8(e.name.to_vec()).unwrap())));
            if next == 0 {
                break;
            }
            cursor = next as usize;
        }
        let expected: Vec<_> = entries.iter().enumerate().map(|(i, e)| (i as u32 + 1, e.0.clone())).collect();
        assert_eq!(listed, expected, "every entry once, in order, each with its cookie");
    }

    #[test]
    fn a_read_never_spans_more_than_the_scratch() {
        for block in [512, 2048, 4096, 65_536] {
            for at in [0, 1, 511, 3000, 65_535, 70_000, 1 << 33] {
                for left in [1, 512, 65_536, 200_000] {
                    let r = scratch_read(at, left, block).unwrap();
                    assert!(r.want > 0 && r.intra + r.want <= r.span as usize, "{block} {at} {left}: {r:?}");
                    assert!(r.span as usize <= SCRATCH && r.span.is_multiple_of(block), "{block} {at} {left}: {r:?}");
                    assert_eq!(r.start + r.intra as u64, at);
                }
            }
        }
    }

    /// **A block that does not divide the scratch is refused**: 520, 3000 or 128 KiB overrun it.
    #[test]
    fn a_block_that_does_not_divide_the_scratch_is_refused() {
        for block in [520, 3000, 131_072, 0] {
            assert_eq!(scratch_read(0, SCRATCH, block), None, "{block}");
            assert_eq!(scratch_read(70_000, SCRATCH, block), None, "{block}");
        }
    }
}
