use super::*;

fn kind(c: &Counts, name: &str) -> usize {
    c.kinds[KINDS.iter().position(|(n, _)| *n == name).unwrap()]
}

fn counted(text: &str) -> Counts {
    count(text).unwrap()
}

#[test]
fn a_marker_in_a_comment_or_a_literal_is_not_counted_and_one_in_code_is() {
    let c = counted(
        "// unsafe { x }\nlet a = unsafe { f() }; // static mut B\n/// unsafe fn g()\nlet s = \"unsafe { }\";\n",
    );
    assert_eq!((kind(&c, "unsafe block"), kind(&c, "static mut"), kind(&c, "unsafe fn")), (1, 0, 0));
}

#[test]
fn a_test_module_and_a_test_item_are_left_out() {
    let text = "fn a() { unsafe { b() } }\n#[cfg(test)]\nmod tests {\n    fn t() {\n        unsafe { c() };\n    }\n}\n\
                #[cfg(test)]\nmod more;\nfn z() { unsafe { q() } }\n#[cfg(test)]\n#[allow(dead_code)]\nfn helper() {\n    unsafe { d() };\n}\n\
                fn e() { unsafe { f() } }\n";
    let c = counted(text);
    assert_eq!((kind(&c, "unsafe block"), c.code), (3, 3), "a brace-less item ends at its `;`");
}

#[test]
fn a_brace_in_a_literal_does_not_move_a_test_items_end() {
    let text = "#[cfg(test)]\nfn t() { let s = \"{\"; let c = '{'; let r = r#\"}}\"#; }\nfn real() { unsafe { x() } }\nunsafe fn y() {}\n";
    let c = counted(text);
    assert_eq!((kind(&c, "unsafe block"), kind(&c, "unsafe fn"), c.code), (1, 1, 2));
}

#[test]
fn a_test_field_ends_at_its_comma() {
    let c = counted("struct S {\n    #[cfg(test)]\n    f: u8,\n}\nunsafe impl Send for S {}\n");
    assert_eq!((kind(&c, "unsafe impl"), c.code), (1, 3));
}

#[test]
fn a_test_item_that_never_ends_is_an_error() {
    assert!(count("fn a() {}\n#[cfg(test)]\nmod tests {\n    fn t() {}\n").is_err());
}

#[test]
fn an_identifier_running_into_a_marker_is_not_it() {
    let c =
        counted("my_syscall1(a);\nlibkern::syscall1(SYS_X, a);\nsyscall4(1, 2, 3, 4);\nlet t = transmute_copy(x);\n");
    assert_eq!((kind(&c, "raw syscall"), kind(&c, "transmute")), (2, 1));
}

#[test]
fn each_kind_is_found() {
    let text = "pub unsafe fn f() {}\npub unsafe extern \"C\" fn g() {}\nunsafe impl Send for S {}\nstatic mut B: u8 = 0;\n\
                let s = core::slice::from_raw_parts_mut(p, n);\nlet r = unsafe { &mut *(p as *mut T) };\n\
                core::arch::asm!(\"nop\");\nnaked_asm!(\"ret\");\nglobal_asm!(\"x\");\nlet v = ptr.read_volatile();\n";
    let c = counted(text);
    let want =
        [("unsafe fn", 2), ("unsafe impl", 1), ("static mut", 1), ("raw slice", 1), ("raw deref", 1), ("asm", 3)];
    for (name, n) in want {
        assert_eq!(kind(&c, name), n, "{name}");
    }
    assert_eq!(kind(&c, "volatile"), 1);
    let calls: String = (0..=6).map(|n| format!("syscall{n}(a);\n")).collect();
    assert_eq!(kind(&counted(&calls), "raw syscall"), 7);
}

#[test]
fn a_raw_deref_counts_inside_an_unsafe_block_only() {
    let text = "let a = &*boxed;\nlet k = match &*m { _ => 0 };\nlet r = unsafe { &mut *frame };\nlet d = unsafe { &*disk };\nunsafe {\n    (*c.irp).buffer = b;\n    \
                f(*x);\n    let e = &*(p as *const T);\n}\nlet g = (*h).len();\n";
    assert_eq!(kind(&counted(text), "raw deref"), 4, "two one-line, a place and a cast; not a call's argument");
}

#[test]
fn test_files_are_known_by_name() {
    for p in
        ["kernel/src/sched/tests.rs", "a/b/write_tests.rs", "x/thread/test_support.rs", "k/src/sched/tests/timers.rs"]
    {
        assert!(is_test_file(p), "{p}");
    }
    assert!(!is_test_file("kernel/src/sched/testing.rs"));
}

#[test]
fn a_tally_leaves_test_files_out_and_refuses_a_group_with_no_code() {
    let file = |p: &str, t: &str| (p.to_string(), t.to_string());
    let files = [
        file("k/a.rs", "unsafe fn a() {}\n"),
        file("k/tests.rs", "unsafe fn t() {}\n"),
        file("k/notes.md", "unsafe fn\n"),
    ];
    assert_eq!(kind(&tally("k", &files).unwrap(), "unsafe fn"), 1);
    assert!(tally("k", &[file("k/tests.rs", "fn t() {}\n")]).is_err());
}

#[test]
fn a_trailing_slash_names_the_same_group() {
    let args = ["kernel/src/mm/".to_string(), "userspace/libos".to_string()];
    assert_eq!(named_groups(&args), ["kernel/src/mm", "userspace/libos"]);
}

#[test]
fn the_default_groups_are_tracked_crates_and_kernel_entries() {
    let tracked = [
        "userspace/libos/Cargo.toml",
        "userspace/libos/src/lib.rs",
        "userspace/.cargo/config.toml",
        "userspace/Cargo.toml",
        "kernel/src/sched/exit.rs",
        "kernel/src/tlb.rs",
        "kernel/src/fbcon/font.psf",
    ];
    assert_eq!(default_groups(&tracked), ["kernel/src/sched", "kernel/src/tlb.rs", "userspace/libos"]);
}
