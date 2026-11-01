//! How many tabs a process holds. Each is a handle in one `sys_wait`, which the kernel refuses past
//! `MAX_WAIT_HANDLES`, and a refused wait ends the process and every shell in it.

use super::tests::{app, line};
use super::*;

fn refused(a: &App) -> bool {
    (0..a.grid().rows()).any(|r| line(a, r).contains("no more tabs"))
}

/// Holding `Ctrl+Shift+T` reaches it.
#[test]
fn a_window_holds_at_most_max_tabs() {
    let mut a = app();
    for _ in 0..MAX_TABS * 2 {
        a.update(Msg::NewTab);
    }
    assert_eq!(a.tabs().len(), MAX_TABS, "the cap did not hold");
    const { assert!(MAX_TABS < libkern::abi::MAX_WAIT_HANDLES, "…and it leaves room for the window") };
    assert!(refused(&a), "the refusal was silent");
}

#[test]
fn the_cap_counts_the_process_s_other_windows() {
    let mut a = app();
    a.set_tabs_elsewhere(MAX_TABS - 2);
    a.update(Msg::NewTab);
    assert_eq!(a.tabs().len(), 2, "one under the cap is room");
    assert!(!refused(&a));
    a.update(Msg::NewTab);
    assert_eq!(a.tabs().len(), 2, "the cap did not count the other windows");
    assert!(refused(&a), "the refusal was silent");
    a.set_tabs_elsewhere(MAX_TABS - 3);
    a.update(Msg::NewTab);
    assert_eq!(a.tabs().len(), 3, "a tab closed elsewhere is room here");
}
