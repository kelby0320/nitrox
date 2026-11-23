//! The scrubs of a typed password, seen through an allocator that counts the blocks freed with one
//! still in them.

use super::*;
use libos::{Carried, Handle, Msg as WireMsg, mock};
use librsproto::{OP_TTY_INTERRUPT, RS_FLAG_ERROR};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// A password no other test types.
const MARK: &[u8] = b"scrub-mark-5f1c";

std::thread_local! {
    static MARKED: Cell<usize> = const { Cell::new(0) };
}

/// The system's allocator, its blocks zeroed when made, which counts each byte buffer this thread
/// frees with [`MARK`] in it. A password's copies are `Vec<u8>`s, of alignment 1.
struct Counting;

// SAFETY: each call is the system allocator's, made with the caller's own arguments.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's layout, passed on.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // Only a byte buffer is read: a struct's padding, copied in, is uninitialised however the
        // block was made, as Miri finds.
        if layout.align() == 1 {
            // SAFETY: `ptr` is a block of `layout.size()` bytes this allocator made and has not
            // freed, zeroed when made and, as a buffer of bytes, written only with bytes.
            let block = unsafe { core::slice::from_raw_parts(ptr, layout.size()) };
            if block.windows(MARK.len()).any(|w| w == MARK) {
                let _ = MARKED.try_with(|n| n.set(n.get() + 1));
            }
        }
        // SAFETY: the caller's block and layout, passed on.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// The blocks this thread freed holding [`MARK`] while `f` ran.
fn marked_by(f: impl FnOnce()) -> usize {
    let before = MARKED.with(Cell::get);
    f();
    MARKED.with(Cell::get) - before
}

#[test]
fn a_block_freed_holding_the_mark_is_counted() {
    assert_eq!(marked_by(|| drop(MARK.to_vec())), 1);
}

/// **`confirm` frees no copy it does not return**: the second always zeroed, the first when it is
/// refused, as unmatched or too long.
#[test]
fn confirm_frees_no_copy_it_does_not_return() {
    let mut kept = None;
    assert_eq!(marked_by(|| kept = Some(confirm(MARK.to_vec(), MARK.to_vec()))), 0, "a match");
    scrub(kept.take().unwrap().as_mut().unwrap());
    assert_eq!(marked_by(|| assert!(confirm(MARK.to_vec(), b"other".to_vec()).is_err())), 0, "unmatched");
    let long = [MARK, &[b'x'; libusers::PASSWORD_MAX]].concat();
    assert_eq!(marked_by(|| assert!(confirm(long.clone(), long.clone()).is_err())), 0, "too long");
}

type End = Handle<Channel, Only>;

/// A terminal whose replies, one a reply each to the prompt's requests, are already queued.
fn terminal(replies: &[(u16, u32, &[u8])]) -> (End, End) {
    mock::reset();
    let (a, b) = End::pair(16).unwrap();
    for &(op, flags, body) in replies {
        let id = if op == OP_TTY_INTERRUPT { 0 } else { 1 };
        let mut m = WireMsg::new();
        let n = librsproto::encode(m.payload_mut(), op, id, flags, body, 0).unwrap();
        m.set_len(n).unwrap();
        b.send(&m, &mut Carried::new()).unwrap();
    }
    (a, b)
}

/// **A password read and then not given is zeroed**: one a `Ctrl-C` cancelled, and a first copy
/// whose second was not typed. Each is measured against a read that gives its password, the
/// terminal's own copies of the line freed in both.
#[test]
fn a_password_not_given_is_zeroed() {
    let line = |flags| [(0, 0, &b""[..]), (0, 0, b""), (0, flags, MARK), (0, 0, b""), (0, 0, b"")];
    let (a, _b) = terminal(&line(0));
    let mut kept = None;
    let given = marked_by(|| kept = ask_password_on(&a, b"pw: "));
    scrub(kept.as_mut().expect("the password"));
    let interrupted =
        [(0, 0, &b""[..]), (OP_TTY_INTERRUPT, 0, b""), (0, 0, b""), (0, 0, MARK), (0, 0, b""), (0, 0, b"")];
    let (a, _b) = terminal(&interrupted);
    assert_eq!(marked_by(|| assert_eq!(ask_password_on(&a, b"pw: "), None)), given, "Ctrl-C");
    let mut first_only = line(0).to_vec();
    first_only.extend([(0, 0, &b""[..]), (0, 0, b""), (0, RS_FLAG_ERROR, b"eof"), (0, 0, b""), (0, 0, b"")]);
    let (a, _b) = terminal(&first_only);
    let cancelled = marked_by(|| assert_eq!(ask_new_password_on(&a, b"new: "), Err(NewPassword::Cancelled)));
    assert_eq!(cancelled, given, "the second not typed");
}
