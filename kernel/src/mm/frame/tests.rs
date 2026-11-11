use std::panic::{AssertUnwindSafe, catch_unwind};

use super::*;
use crate::mm::test_support::init_global_heap;

/// A fresh frame and a view of it, which the test frees.
fn frame() -> (PhysAddr, FrameView<'static>) {
    let f = alloc_zeroed(0).expect("a frame");
    // SAFETY: a frame just allocated for this test, which frees it only once done with the view.
    (f, unsafe { FrameView::new(f) })
}

fn stops(f: impl FnOnce()) -> bool {
    catch_unwind(AssertUnwindSafe(f)).is_err()
}

/// **Each access reaches the frame's last byte and stops one past it**, or at an end that overflows.
#[test]
fn an_access_reaches_the_last_byte_and_stops_one_past_it() {
    init_global_heap();
    let (f, page) = frame();
    page.write(PAGE_SIZE - 1, &[7]);
    let mut b = [0u8];
    page.read(PAGE_SIZE - 1, &mut b);
    assert_eq!(b, [7]);
    page.fill(PAGE_SIZE - 1, 1, 9);
    page.read(PAGE_SIZE, &mut []);
    page.write(PAGE_SIZE, &[]);
    page.fill(PAGE_SIZE, 0, 1);
    page.copy_from(PAGE_SIZE, page, PAGE_SIZE, 0);

    assert!(stops(|| page.read(PAGE_SIZE, &mut [0])), "a read one past");
    assert!(stops(|| page.read(PAGE_SIZE - 1, &mut [0; 2])), "a read running one past");
    assert!(stops(|| page.write(PAGE_SIZE, &[0])), "a write one past");
    assert!(stops(|| page.write(PAGE_SIZE - 8, &[0; 9])), "a word's write running one past");
    assert!(stops(|| page.fill(PAGE_SIZE + 1, 0, 0)), "an empty fill past the end");
    assert!(stops(|| page.fill(usize::MAX, 2, 0)), "a fill whose end overflows");
    assert!(stops(|| page.copy_from(0, page, PAGE_SIZE - 3, 4)), "a copy from past the source");
    assert!(stops(|| page.copy_from(PAGE_SIZE - 3, page, 0, 4)), "a copy past the destination");
    page.read(PAGE_SIZE - 1, &mut b);
    assert_eq!(b, [9], "nothing a refused access began was written");
    heap::buddy_free(f, 0);
}

/// **A run from an odd offset across words reads back whole, and in pieces at other alignments.**
#[test]
fn a_run_across_words_reads_back_at_any_alignment() {
    init_global_heap();
    let (f, page) = frame();
    let run: std::vec::Vec<u8> = (1..=40).collect();
    page.write(3, &run);
    let mut back = [0u8; 46];
    page.read(0, &mut back);
    assert_eq!(back[..3], [0; 3]);
    assert_eq!(back[3..43], run[..]);
    assert_eq!(back[43..], [0; 3]);
    for at in 0..16 {
        let mut piece = [0u8; 13];
        page.read(3 + at, &mut piece);
        assert_eq!(piece[..], run[at..at + 13], "13 bytes from {at}");
    }
    page.fill(5, 17, 0xEE);
    page.read(0, &mut back);
    assert!(back[5..22].iter().all(|&b| b == 0xEE) && back[..5] == [0, 0, 0, 1, 2] && back[22] == 20);
    heap::buddy_free(f, 0);
}

/// **A copy between frames moves its bytes, aligned alike or not.**
#[test]
fn a_copy_between_frames_moves_its_bytes_at_any_alignment() {
    init_global_heap();
    let (fa, a) = frame();
    let (fb, b) = frame();
    let src: std::vec::Vec<u8> = (0..=255).collect();
    a.write(0, &src);
    for (to, from) in [(0, 0), (8, 16), (1, 9), (3, 0), (0, 5)] {
        b.fill(0, PAGE_SIZE, 0);
        b.copy_from(to, a, from, 100);
        let mut got = [0u8; 102];
        b.read(to, &mut got[..100]);
        assert_eq!(got[..100], src[from..from + 100], "{from} to {to}");
        b.read(to + 100, &mut got[100..]);
        assert_eq!(got[100..], [0, 0], "{from} to {to}: nothing past the run");
    }
    heap::buddy_free(fa, 0);
    heap::buddy_free(fb, 0);
}

/// **A block freed dirty comes back zeroed**, at every order the test reaches.
#[test]
fn a_block_freed_dirty_comes_back_zeroed() {
    init_global_heap();
    for order in [0, 2] {
        for _ in 0..4 {
            let f = alloc_zeroed(order).expect("a block");
            let bytes = PAGE_SIZE << order;
            // SAFETY: the block this test just allocated, `bytes` long, which it alone reaches.
            let block = unsafe { core::slice::from_raw_parts_mut(hhdm::<u8>(f), bytes) };
            assert!(block.iter().all(|&b| b == 0), "order {order}: zeroed");
            block.fill(0xA5);
            heap::buddy_free(f, order);
        }
    }
}
