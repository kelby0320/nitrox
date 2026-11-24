//! **A suffix is held to `MAX_SUFFIX` on each path that copies it**, through a volume that records
//! what reaches it: the longest is served, and one byte more is refused before the volume sees it.

use super::*;
use crate::serve::session_tests::Spy;
use librsproto::error::parse_error;
use librsproto::file::read_range_request;
use librsproto::namespace::{RESOLVE_FILE_LAZY, RESOLVE_GROW, resolve_request};
use std::vec;
use std::vec::Vec;

fn suffix(len: usize) -> Vec<u8> {
    vec![b'n'; len]
}

fn message(op: u16, body: &[u8]) -> Vec<u8> {
    let mut req = vec![0u8; body.len() + 64];
    let len = encode(&mut req, op, 3, 0, body, 0).unwrap();
    req.truncate(len);
    req
}

fn read_range(suffix: &[u8], len: u32) -> Vec<u8> {
    let mut body = vec![0u8; suffix.len() + 64];
    let n = read_range_request(&mut body, 0, len, suffix).unwrap();
    message(OP_FILE_READ_RANGE, &body[..n])
}

fn resolve_of(suffix: &[u8], flags: u32, size: Option<u32>) -> Vec<u8> {
    let mut body = vec![0u8; suffix.len() + 64];
    let mut n = resolve_request(&mut body, 0, flags, suffix).unwrap();
    if let Some(size) = size {
        body[n..n + 4].copy_from_slice(&size.to_le_bytes());
        n += 4;
    }
    message(OP_NS_RESOLVE, &body[..n])
}

/// The error a reply carries, or `None` for a success.
fn refused(served: Served, reply: &[u8]) -> Option<i32> {
    match served {
        Served::Error { reply_len } => Some(parse_error(decode(&reply[..reply_len]).unwrap().body).unwrap().kerror),
        _ => None,
    }
}

#[test]
fn a_range_read_is_served_at_the_longest_suffix_and_refused_past_it() {
    let vol = Spy { file: true, ..Spy::default() };
    let (mut content, mut reply) = ([0u8; 4096], [0u8; 4096]);
    let served = serve(&vol, &read_range(&suffix(MAX_SUFFIX), 100), &mut content, &mut reply);
    assert_eq!(refused(served, &reply), None);
    let served = serve(&vol, &read_range(&suffix(MAX_SUFFIX + 1), 100), &mut content, &mut reply);
    assert_eq!(refused(served, &reply), Some(KError::TooLarge.as_i32()));
    assert_eq!(*vol.calls.borrow(), [std::format!("read_range {} 100", MAX_SUFFIX + 1)]);
}

/// **A range read asks the volume for no more than the content buffer holds**, whatever the
/// request says: the kernel asks a page, and the buffer is the server's.
#[test]
fn a_range_read_is_cut_to_the_content_buffer() {
    let vol = Spy { file: true, ..Spy::default() };
    let (mut content, mut reply) = ([0u8; 4096], [0u8; 4096]);
    match serve(&vol, &read_range(b"f", 8192), &mut content, &mut reply) {
        Served::File { content_len, .. } => assert_eq!(content_len, 4096),
        _ => panic!("expected a File reply"),
    }
    assert_eq!(*vol.calls.borrow(), ["read_range 2 4096"]);
}

#[test]
fn a_resolve_is_served_at_the_longest_suffix_and_refused_past_it() {
    let vol = Spy { file: true, ..Spy::default() };
    let (mut content, mut reply) = ([0u8; MAX_FILE], [0u8; 4096]);
    for flags in [0, RESOLVE_FILE_LAZY] {
        let served = serve(&vol, &resolve_of(&suffix(MAX_SUFFIX), flags, None), &mut content, &mut reply);
        assert_eq!(refused(served, &reply), None, "flags {flags}");
        let served = serve(&vol, &resolve_of(&suffix(MAX_SUFFIX + 1), flags, None), &mut content, &mut reply);
        assert_eq!(refused(served, &reply), Some(KError::TooLarge.as_i32()), "flags {flags}");
    }
}

/// **A directory opens at the longest suffix, and not one byte past it**: the walk is handed the
/// suffix where it lies, so no copy's panic stands in for the bound.
#[test]
fn a_directory_opens_at_the_longest_suffix_and_not_past_it() {
    let vol = Spy::default();
    assert_eq!(directory_open(&vol, &resolve_of(&suffix(MAX_SUFFIX), 0, None)), Some((3, Ok(7))));
    assert_eq!(directory_open(&vol, &resolve_of(&suffix(MAX_SUFFIX + 1), 0, None)), None);
}

/// **A resolve's size change reaches the volume at the longest suffix, and not past it**, where
/// the resolve that follows refuses it.
#[test]
fn a_size_change_reaches_the_volume_at_the_longest_suffix_only() {
    let vol = Spy::default();
    let grow = |len| apply_size(&vol, &resolve_of(&suffix(len), RESOLVE_GROW, Some(10)), 0);
    assert_eq!(grow(MAX_SUFFIX), Err(FsError::NotFound), "the volume's answer");
    assert_eq!(grow(MAX_SUFFIX + 1), Ok(Resized::default()));
    assert_eq!(*vol.calls.borrow(), [std::format!("grow {} 10", MAX_SUFFIX + 1)]);
}
