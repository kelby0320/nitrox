//! **What fits**: a page of a directory listing in one reply.

use alloc::string::String;
use alloc::vec::Vec;
use librsproto::RS_MAX_BODY;
use librsproto::file::DirReplyWriter;

/// The entries from `cursor` that fit one `ReadDir` reply's body, and the cursor to ask from next,
/// `0` at the end. **Sized to `RS_MAX_BODY`**: a body the encoder cannot frame is a reply never
/// sent, and a client waiting on it for good.
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

#[cfg(test)]
mod tests {
    use super::*;
    use librsproto::file::DIRENT_KIND_FILE;
    use librsproto::{OP_FILE_READ_DIR, RS_FLAG_REPLY, encode};
    use std::format;

    /// **A full GPT's listing pages into replies that encode**: a disk, its 128 partitions and
    /// `all.tsm`, as `table::entries` names them, where the first page was a body too long to send.
    #[test]
    fn a_full_gpts_listing_pages_into_replies_that_encode() {
        let names = core::iter::once(String::from("all.tsm")).chain((0..129).map(|n| format!("blk-{n}.tsm")));
        let entries: Vec<_> = names.map(|n| (n, DIRENT_KIND_FILE, 1 << 40)).collect();
        let mut cursor = 0;
        loop {
            let (body, next) = dir_page(&entries, cursor).unwrap();
            let mut payload = [0u8; 4096 - 24];
            assert!(encode(&mut payload, OP_FILE_READ_DIR, 1, RS_FLAG_REPLY, &body, 0).is_some());
            if next == 0 {
                break;
            }
            assert!(next as usize > cursor);
            cursor = next as usize;
        }
        assert_eq!(entries.len(), 130);
    }
}
