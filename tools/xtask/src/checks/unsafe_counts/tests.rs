use super::*;

fn map(entries: &[(&str, usize)]) -> BTreeMap<String, usize> {
    entries.iter().map(|(g, n)| (g.to_string(), *n)).collect()
}

/// Counts with only unsafe blocks and `unsafe fn`s set, the others zero.
fn actual(counts: &[(&str, usize, usize)]) -> Counts {
    counts
        .iter()
        .map(|(g, b, f)| {
            let mut c = [0; KIND_COUNT];
            (c[UNSAFE_BLOCK], c[UNSAFE_FN]) = (*b, *f);
            (g.to_string(), c)
        })
        .collect()
}

fn base(blocks: &[(&str, usize)], fns: &[(&str, usize)]) -> Baseline {
    let mut b = Baseline::default();
    (b.sections[0], b.sections[1]) = (map(blocks), map(fns));
    b
}

fn file(p: &str, t: &str) -> (String, String) {
    (p.to_string(), t.to_string())
}

#[test]
fn the_baseline_parses_and_formats_back() {
    let mut b = base(&[("kernel/src/arch", 207), ("kernel/src/mm", 3)], &[("kernel/src/arch", 142)]);
    b.sections[5] = map(&[("kernel/src/arch", 11)]);
    let text = format_baseline(&b);
    assert_eq!(parse_baseline(&text).unwrap(), b);
    assert_eq!(format_baseline(&parse_baseline(&text).unwrap()), text);
}

#[test]
fn a_malformed_line_and_a_stray_entry_are_errors() {
    for bad in [
        "[unsafe-blocks]\nkernel/src/a = 1\n",
        "[unsafe-blocks]\n\"kernel/src/a\" = many\n",
        "[unsafe-fns]\n\"kernel/src/a\" 1\n",
        "\"kernel/src/a\" = 1\n",
        "[other]\n\"kernel/src/a\" = 1\n",
    ] {
        assert!(parse_baseline(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn a_section_is_checked_against_its_own_entry() {
    let g = "kernel/src/x";
    for (n, want) in [(4, 1), (5, 0), (6, 1)] {
        let f = section_failures("unsafe block(s)", &map(&[(g, n)]), &map(&[(g, 5)]));
        assert_eq!(f.len(), want, "{n} against 5: {f:?}");
        assert!(f.iter().all(|l| l.contains(g)));
    }
    let over = section_failures("unsafe block(s)", &map(&[(g, 6)]), &map(&[(g, 5)]));
    assert!(over[0].contains("over its entry of 5"));
    let stale = section_failures("unsafe block(s)", &map(&[(g, 4)]), &map(&[(g, 5)]));
    assert!(stale[0].contains("--update"));
}

#[test]
fn each_failure_names_its_module_in_each_section() {
    let (a, g) = ("kernel/src/a", "kernel/src/gone");
    let cases = [
        (actual(&[(a, 6, 6)]), base(&[(a, 5)], &[(a, 5)])),
        (actual(&[(a, 4, 4)]), base(&[(a, 5)], &[(a, 5)])),
        (actual(&[(a, 1, 1)]), base(&[], &[])),
        (actual(&[(a, 0, 0)]), base(&[(a, 5)], &[(a, 5)])),
        (actual(&[]), base(&[(g, 5)], &[(g, 5)])),
    ];
    for (act, b) in &cases {
        let f = failures(act, b);
        assert_eq!(f.len(), 2, "one per section: {f:?}");
        assert!(f.iter().all(|l| l.contains("kernel/src/a") || l.contains(g)));
    }
    assert!(failures(&actual(&[(a, 5, 5)]), &base(&[(a, 5)], &[(a, 5)])).is_empty());
    // One section off does not fail the other.
    let f = failures(&actual(&[(a, 6, 5)]), &base(&[(a, 5)], &[(a, 5)]));
    assert_eq!(f.len(), 1);
    assert!(f[0].contains("unsafe block"));
    let f = failures(&actual(&[(a, 5, 6)]), &base(&[(a, 5)], &[(a, 5)]));
    assert_eq!(f.len(), 1);
    assert!(f[0].contains("unsafe fn"));
    // Zero in a section with no entry in it is a match.
    assert!(failures(&actual(&[(a, 0, 2)]), &base(&[], &[(a, 2)])).is_empty());
}

#[test]
fn every_kind_held_fails_a_new_one_with_no_entry() {
    for (i, (_, kind, what)) in SECTIONS.iter().enumerate() {
        let mut c = [0; KIND_COUNT];
        c[*kind] = 1;
        let f = failures(&BTreeMap::from([("kernel/src/a".to_string(), c)]), &Baseline::default());
        assert_eq!(f.len(), 1, "{what}: {f:?}");
        assert!(f[0].contains(what) && f[0].contains("no entry"), "{f:?}");
        let mut held = Baseline::default();
        held.sections[i] = map(&[("kernel/src/a", 1)]);
        assert!(failures(&BTreeMap::from([("kernel/src/a".to_string(), c)]), &held).is_empty(), "{what}");
    }
}

#[test]
fn update_lowers_removes_and_never_raises() {
    let b = base(
        &[("kernel/src/lower", 5), ("kernel/src/grew", 5), ("kernel/src/zero", 5), ("kernel/src/gone", 5)],
        &[("kernel/src/lower", 5)],
    );
    let act = actual(&[
        ("kernel/src/lower", 4, 5),
        ("kernel/src/grew", 6, 0),
        ("kernel/src/zero", 0, 1),
        ("kernel/src/new", 2, 2),
    ]);
    let got = updated(&act, &b);
    assert_eq!(got.sections[0], map(&[("kernel/src/lower", 4), ("kernel/src/grew", 5)]));
    assert_eq!(got.sections[1], map(&[("kernel/src/lower", 5)]));
}

#[test]
fn a_listing_leaves_out_what_counts_zero() {
    let m = measured(&actual(&[("kernel/src/a", 0, 2), ("kernel/src/b", 3, 0)]));
    assert_eq!(m.sections[0], map(&[("kernel/src/b", 3)]));
    assert_eq!(m.sections[1], map(&[("kernel/src/a", 2)]));
    let listed = run(&["--list".to_string()], &actual(&[("kernel/src/a", 0, 2)]), None).unwrap();
    assert!(listed.write.is_none() && listed.failures.is_empty());
    assert_eq!(parse_baseline(&listed.printed[0]).unwrap(), base(&[], &[("kernel/src/a", 2)]));
    assert!(!listed.printed[0].contains("= 0"), "{}", listed.printed[0]);
}

#[test]
fn an_update_writes_the_lowered_table_and_still_fails_a_rise() {
    let (a, b) = ("kernel/src/a", "kernel/src/b");
    let table = format_baseline(&base(&[(a, 5), (b, 5)], &[]));
    let act = actual(&[(a, 4, 0), (b, 6, 0)]);
    let plain = run(&[], &act, Some(&table)).unwrap();
    assert!(plain.write.is_none());
    assert_eq!(plain.failures.len(), 2, "{:?}", plain.failures);
    let up = run(&["--update".to_string()], &act, Some(&table)).unwrap();
    let written = up.write.expect("an update writes the table");
    assert_eq!(parse_baseline(&written).unwrap(), base(&[(a, 4), (b, 5)], &[]));
    assert_eq!(up.printed, vec![format!("check-unsafe: {a}: unsafe-blocks 5 -> 4")]);
    assert_eq!(up.failures.len(), 1);
    assert!(up.failures[0].contains(b) && up.failures[0].contains("over its entry"));
    let again = run(&[], &act, Some(&written)).unwrap();
    assert_eq!(again.failures, up.failures);
    assert!(run(&[], &act, None).is_err(), "no table, and not a listing");
}

#[test]
fn counting_follows_the_inventorys_rules() {
    let files = [
        file("kernel/src/x.rs", "fn a() { unsafe { f() } }\nunsafe fn b() {}\n"),
        file("kernel/src/x/y.rs", "fn c() { unsafe { g() }; unsafe { h() } }\n// unsafe { in a comment }\n"),
        file("kernel/src/x/tests.rs", "fn t() { unsafe { z() } }\n"),
        file(
            "kernel/src/x_other.rs",
            "unsafe fn o() {}\nfn p() { let s = \"unsafe {\"; }\n#[cfg(test)]\nmod t {\n    fn q() { unsafe { r() } }\n}\n",
        ),
        file("userspace/init/src/main.rs", "fn m() { unsafe { n() } }\n"),
    ];
    let got = kernel_counts(&files).unwrap();
    let pick = |g: &str| got.get(g).map(|c| (c[UNSAFE_BLOCK], c[UNSAFE_FN]));
    assert_eq!(got.len(), 2);
    assert_eq!(pick("kernel/src/x"), Some((3, 1)));
    assert_eq!(pick("kernel/src/x_other"), Some((0, 1)));
}

#[test]
fn a_module_of_only_test_code_counts_zero() {
    let got = kernel_counts(&[
        file("kernel/src/a.rs", "fn f() { unsafe { g() } }\n"),
        file("kernel/src/test_support.rs", "pub fn helper() {}\n"),
        file("kernel/src/b.rs", "//! docs\n#[cfg(test)]\nmod tests;\n"),
        file("kernel/src/b/tests.rs", "fn t() { unsafe { u() } }\n"),
    ])
    .unwrap();
    assert_eq!(got["kernel/src/a"][UNSAFE_BLOCK], 1);
    assert_eq!(got["kernel/src/test_support"], [0; KIND_COUNT]);
    assert_eq!(got["kernel/src/b"], [0; KIND_COUNT]);
    assert!(failures(&got, &base(&[("kernel/src/a", 1)], &[])).is_empty());
}

#[test]
fn a_test_item_ends_where_its_brackets_and_comments_say() {
    let got = kernel_counts(&[
        file(
            "kernel/src/c.rs",
            "#[cfg(test)]\nfn helper(\n    a: u32,\n    b: u32,\n) -> u32 {\n    unsafe { g() }\n}\nfn prod() {}\n",
        ),
        file(
            "kernel/src/d.rs",
            "impl X {\n    #[cfg(test)]\n    fn t() { /* { */ }\n    unsafe fn prod() {\n        unsafe { y() }\n    }\n}\n",
        ),
    ])
    .unwrap();
    assert_eq!((got["kernel/src/c"][UNSAFE_BLOCK], got["kernel/src/c"][UNSAFE_FN]), (0, 0));
    assert_eq!((got["kernel/src/d"][UNSAFE_BLOCK], got["kernel/src/d"][UNSAFE_FN]), (1, 1));
}

#[test]
fn each_unsafe_claim_is_counted_and_a_function_pointer_type_is_not() {
    let got = kernel_counts(&[file(
        "kernel/src/e.rs",
        "struct P;\nunsafe impl Sync for P {}\nstatic mut X: u8 = 0;\nunsafe extern \"C\" {\n    fn ext();\n}\n\
         #[unsafe(no_mangle)]\npub extern \"C\" fn probe() {}\npub struct Q {\n    f: unsafe extern \"C\" fn(*mut u8),\n}\n",
    )])
    .unwrap();
    let c = got["kernel/src/e"];
    assert_eq!(
        (c[UNSAFE_IMPL], c[STATIC_MUT], c[UNSAFE_EXTERN], c[UNSAFE_ATTR], c[UNSAFE_FN], c[UNSAFE_BLOCK]),
        (1, 1, 1, 1, 0, 0)
    );
}
