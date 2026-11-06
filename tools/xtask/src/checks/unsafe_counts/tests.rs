use super::*;

fn map(entries: &[(&str, usize)]) -> BTreeMap<String, usize> {
    entries.iter().map(|(g, n)| (g.to_string(), *n)).collect()
}

fn actual(counts: &[(&str, usize, usize)]) -> BTreeMap<String, (usize, usize)> {
    counts.iter().map(|(g, b, f)| (g.to_string(), (*b, *f))).collect()
}

fn base(blocks: &[(&str, usize)], fns: &[(&str, usize)]) -> Baseline {
    Baseline { blocks: map(blocks), fns: map(fns) }
}

#[test]
fn the_baseline_parses_and_formats_back() {
    let b = base(&[("kernel/src/arch", 207), ("kernel/src/mm", 3)], &[("kernel/src/arch", 142)]);
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
    assert_eq!(got.blocks, map(&[("kernel/src/lower", 4), ("kernel/src/grew", 5)]));
    assert_eq!(got.fns, map(&[("kernel/src/lower", 5)]));
}

#[test]
fn counting_follows_the_inventorys_rules() {
    let file = |p: &str, t: &str| (p.to_string(), t.to_string());
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
    let got = kernel_unsafe(&files).unwrap();
    assert_eq!(got, actual(&[("kernel/src/x", 3, 1), ("kernel/src/x_other", 0, 1)]));
}
