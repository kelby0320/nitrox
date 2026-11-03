use super::*;

fn api(wrappers: &[(&str, &str, bool)], modules: &[&str]) -> Api {
    Api {
        wrappers: wrappers
            .iter()
            .map(|(m, n, top)| Wrapper { module: m.to_string(), name: n.to_string(), top: *top, ctor: false })
            .collect(),
        modules: modules.iter().map(|m| m.to_string()).collect(),
    }
}

fn std_api() -> Api {
    let mut a = api(&[("debug", "kprint", true), ("control", "recv", false)], &["debug", "control", "abi"]);
    a.wrappers.push(Wrapper { module: "debug".into(), name: "Line".into(), top: false, ctor: true });
    a
}

fn count(text: &str) -> usize {
    count_file(text, &std_api(), &BTreeSet::new()).unwrap()
}

fn real_libkern() -> Vec<(String, String)> {
    let dir = repo_root().join("userspace/libkern/src");
    let mut out = Vec::new();
    for e in fs::read_dir(&dir).unwrap() {
        let path = e.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        out.push((format!("userspace/libkern/src/{name}"), fs::read_to_string(&path).unwrap()));
    }
    out
}

#[test]
fn the_wrapper_list_is_derived_from_libkern() {
    let got = derive_api(&real_libkern()).unwrap();
    let mut names: Vec<String> = got.wrappers.iter().map(Wrapper::path).collect();
    names.sort();
    let want = [
        "channel::channel_create",
        "channel::channel_recv",
        "channel::channel_recv_raw",
        "channel::pause_held",
        "clock::clock_read",
        "control::recv",
        "debug::Line::new",
        "debug::exit",
        "debug::kprint",
        "debug::kprint_hex",
        "debug::kprint_u64",
    ];
    assert_eq!(names, want);
    let recv = got.wrappers.iter().find(|w| w.name == "recv").unwrap();
    assert!(!recv.top, "libkern's root does not re-export control::recv");
    assert!(got.wrappers.iter().find(|w| w.name == "kprint").unwrap().top);
}

#[test]
fn a_wrapper_is_found_through_a_chain_of_calls() {
    let files = [
        file("k/src/lib.rs", "pub mod a;\npub use a::*;\n"),
        file(
            "k/src/a.rs",
            "pub fn low() {\n    unsafe { syscall1(1, 2) };\n}\npub fn mid() {\n    low();\n}\npub fn top() {\n    mid();\n}\n\
             pub fn pure() {\n    helper();\n}\n#[cfg(test)]\nmod tests {\n    pub fn t() { syscall1(1, 2); }\n}\n\
             impl S {\n    pub fn m(&self) { syscall1(1, 2); }\n}\n",
        ),
    ];
    let got = derive_api(&files).unwrap();
    let mut names: Vec<String> = got.wrappers.iter().map(Wrapper::path).collect();
    names.sort();
    assert_eq!(
        names,
        ["a::S::new", "a::low", "a::mid", "a::top"],
        "a type by its method; no test item or pure function"
    );
    assert!(got.wrappers.iter().all(|w| w.top), "`pub use a::*` re-exports them");
}

#[test]
fn a_type_counts_by_its_own_methods_and_not_a_trait_impls() {
    let files = [file(
        "k/src/a.rs",
        "pub fn low() {\n    unsafe { syscall1(1, 2) };\n}\nimpl<const N: usize> G<N> {\n    pub fn m(&self) { low(); }\n}\n\
         impl Drop for D {\n    fn drop(&mut self) { low(); }\n}\nimpl P {\n    pub fn m(&self) {}\n}\n",
    )];
    let mut names: Vec<String> = derive_api(&files).unwrap().wrappers.iter().map(Wrapper::path).collect();
    names.sort();
    assert_eq!(names, ["a::G::new", "a::low"], "a generic type by its call; a drop or a pure type not");
}

#[test]
fn making_a_type_whose_method_makes_a_syscall_counts_once() {
    assert_eq!(count("use libkern::debug::Line;\nfn f() { Line::new().s(b\"a\").end(); }\n"), 1);
    assert_eq!(count("fn f() { libkern::debug::Line::new().end(); }\n"), 1);
    assert_eq!(count("use libkern::debug;\nfn f() { debug::Line::new().end(); }\n"), 1);
    assert_eq!(count("use libkern::debug::*;\nfn f() { Line::new().end(); }\n"), 1);
    assert_eq!(count("use libos::log::Line;\nfn f() { Line::new().end(); }\n"), 0, "libos's own");
    assert_eq!(count("use libkern::*;\nfn f() { Line::new().end(); }\n"), 0, "the root does not re-export it");
    assert_eq!(count("use libkern::debug::Line;\nfn f() { Other::new(); new(); x.new(); }\n"), 0);
    let local = "use libkern::debug::*;\nstruct Line;\nfn f() { Line::new().end(); }\n";
    let locals = BTreeSet::from(["Line".to_string()]);
    assert_eq!(count_file(local, &std_api(), &locals).unwrap(), 0, "a type of the crate's own shadows a glob");
    assert_eq!(defined_type("pub(crate) struct Line<'a> {"), Some("Line".to_string()));
}

#[test]
fn the_syscall_functions_themselves_are_not_wrappers() {
    let files =
        [file("k/src/syscall.rs", "pub unsafe fn syscall0(nr: u64) -> i64 {\n    syscall6(nr, 0, 0, 0, 0, 0, 0)\n}\n")];
    assert!(derive_api(&files).unwrap().wrappers.is_empty());
}

#[test]
fn a_raw_syscall_counts_and_a_longer_identifier_does_not() {
    assert_eq!(count("unsafe { syscall1(A, b) };\nlibkern::syscall4(1, 2, 3, 4, 5);\nmy_syscall1(a);\n"), 2);
}

#[test]
fn a_qualified_call_counts() {
    assert_eq!(
        count("libkern::kprint(b\"x\");\nlibkern::debug::kprint(b\"x\");\nlibkern::control::recv(&mut c);\n"),
        3
    );
    assert_eq!(count("libkern::recv(&mut c);\nlibkern::abi::kprint(b\"x\");\nother::kprint(b\"x\");\n"), 0);
}

#[test]
fn a_module_call_counts_only_where_the_module_is_imported() {
    assert_eq!(count("use libkern::control;\nfn f() { control::recv(&mut c); }\n"), 1);
    assert_eq!(count("use libkern::{abi, control};\nfn f() { control::recv(&mut c); }\n"), 1);
    assert_eq!(count("fn f() { control::recv(&mut c); }\n"), 0);
    assert_eq!(count("use libkern::abi;\nfn f() { control::recv(&mut c); }\n"), 0);
}

#[test]
fn a_bare_call_counts_where_the_name_is_imported_or_globbed() {
    for header in [
        "use libkern::kprint;",
        "use libkern::{exit, kprint};",
        "use libkern::debug::kprint;",
        "use libkern::debug::{Line, kprint};",
        "use libkern::*;",
        "use libkern::debug::*;",
        "use libkern::{\n    abi::IPC_MSG_SIZE,\n    kprint,\n};",
    ] {
        assert_eq!(count(&format!("{header}\nfn f() {{ kprint(b\"x\"); }}\n")), 1, "{header}");
    }
    for header in ["", "use libkern::abi::*;", "use libkern::debug::Line;", "use other::kprint;", "use libkern::exit;"]
    {
        assert_eq!(count(&format!("{header}\nfn f() {{ kprint(b\"x\"); }}\n")), 0, "{header:?}");
    }
    assert_eq!(count("use libkern::*;\nfn f() { recv(&mut c); }\n"), 0, "the root does not re-export it");
    assert_eq!(count("use libkern::control::recv;\nfn f() { recv(&mut c); }\n"), 1);
    assert_eq!(count("use libkern::control::*;\nfn f() { recv(&mut c); }\n"), 1);
}

#[test]
fn a_method_a_definition_and_a_longer_name_are_not_calls() {
    let text = "use libkern::*;\nfn f(l: L) {\n    l.kprint(b\"x\");\n    my_kprint(b\"x\");\n    kprint_u64(1);\n}\n";
    assert_eq!(count(text), 0);
    assert_eq!(count("use libkern::*;\npub fn kprint(m: &[u8]) {}\n"), 0, "a definition");
}

#[test]
fn a_local_function_shadows_a_glob_and_its_body_counts() {
    let text = "use libkern::*;\nfn kprint(m: &[u8]) {\n    unsafe { syscall4(1, 0, 0, 0, 0) };\n}\nfn f() { kprint(b\"x\"); }\n";
    let locals: BTreeSet<String> = ["kprint".to_string()].into();
    assert_eq!(count_file(text, &std_api(), &locals).unwrap(), 1, "the syscall in the body, not the call");
    assert_eq!(count_file(text, &std_api(), &BTreeSet::new()).unwrap(), 2, "without the shadow");
    assert_eq!(defined_fn("    pub(crate) unsafe fn kprint(m: &[u8]) {"), Some("kprint".to_string()));
    assert_eq!(defined_fn("    fn kprint(&self, m: &[u8]) {"), None, "a method");
    assert_eq!(defined_fn("let kprint = 1;"), None);
}

#[test]
fn a_call_in_test_code_a_literal_or_a_comment_is_not_counted() {
    let text = "use libkern::*;\n// kprint(b\"x\");\nlet s = \"kprint(1) syscall1(\";\n#[cfg(test)]\nmod tests {\n    fn t() { kprint(b\"x\"); syscall1(1, 2); }\n}\nfn real() { kprint(b\"x\"); }\n";
    assert_eq!(count(text), 1);
}

fn file(path: &str, text: &str) -> (String, String) {
    (path.to_string(), text.to_string())
}

fn measured(files: &[(String, String)], entries: &[&str]) -> BTreeMap<String, usize> {
    let entries = entries.iter().map(|e| e.to_string()).collect();
    measure(files, &["userspace/x".to_string()], &std_api(), &entries).unwrap()
}

#[test]
fn a_test_file_is_not_counted() {
    let files = [
        file("userspace/x/src/a.rs", "fn f() { syscall1(1, 2); }\n"),
        file("userspace/x/src/tests.rs", "fn f() { syscall1(1, 2); }\n"),
        file("userspace/x/tests/it.rs", "fn f() { syscall1(1, 2); }\n"),
    ];
    assert_eq!(measured(&files, &[]), BTreeMap::from([("userspace/x".to_string(), 1)]));
}

#[test]
fn a_module_entry_takes_its_files_from_the_crate() {
    let files = [
        file("userspace/x/src/lib.rs", "fn f() { syscall1(1, 2); syscall1(1, 2); }\n"),
        file("userspace/x/src/m.rs", "fn f() { syscall1(1, 2); }\n"),
        file("userspace/x/src/m/deep.rs", "fn f() { syscall1(1, 2); syscall1(1, 2); syscall1(1, 2); }\n"),
        file("userspace/x/src/mm.rs", "fn f() { syscall1(1, 2); }\n"),
    ];
    let got = measured(&files, &["userspace/x/src/m"]);
    assert_eq!(got["userspace/x/src/m"], 4);
    assert_eq!(got["userspace/x"], 3, "lib.rs and mm.rs; `m` does not hold `mm.rs`");
    let nested = measured(&files, &["userspace/x/src/m", "userspace/x/src/m/deep"]);
    assert_eq!((nested["userspace/x/src/m"], nested["userspace/x/src/m/deep"]), (1, 3), "the longest entry wins");
}

#[test]
fn an_entry_with_no_file_has_no_count() {
    let files = [file("userspace/x/src/lib.rs", "fn f() {}\n")];
    let got = measured(&files, &["userspace/x/src/gone", "userspace/x"]);
    assert_eq!(got, BTreeMap::from([("userspace/x".to_string(), 0)]));
}

#[test]
fn a_listed_crate_whose_code_is_gone_has_no_count() {
    let files = [file("userspace/x/src/lib.rs", "fn f() { syscall1(1, 2); }\n")];
    let crates = ["userspace/x".to_string(), "userspace/y".to_string()];
    let entries = BTreeSet::from(["userspace/x".to_string(), "userspace/y".to_string()]);
    let got = measure(&files, &crates, &std_api(), &entries).unwrap();
    assert_eq!(got, BTreeMap::from([("userspace/x".to_string(), 1)]), "y has a manifest and no Rust");
    let failed = failures(&got, &base(&[("userspace/x", 1), ("userspace/y", 3)], &[]));
    assert!(failed.len() == 1 && failed[0].contains("no counted crate or module is there"), "{failed:?}");
}

fn base(counts: &[(&str, usize)], agreed: &[(&str, &str)]) -> Baseline {
    Baseline {
        counts: counts.iter().map(|(g, n)| (g.to_string(), *n)).collect(),
        agreed: agreed.iter().map(|(g, r)| (g.to_string(), r.to_string())).collect(),
    }
}

fn actual(list: &[(&str, usize)]) -> BTreeMap<String, usize> {
    list.iter().map(|(g, n)| (g.to_string(), *n)).collect()
}

fn only(b: &Baseline, a: &[(&str, usize)]) -> String {
    let f = failures(&actual(a), b);
    assert_eq!(f.len(), 1, "{f:?}");
    f.into_iter().next().unwrap()
}

#[test]
fn each_failure_says_what_to_do() {
    let b = base(&[("userspace/a", 5)], &[]);
    assert!(failures(&actual(&[("userspace/a", 5)]), &b).is_empty());
    let new = only(&b, &[("userspace/a", 5), ("userspace/b", 1)]);
    assert!(new.contains("userspace/b") && new.contains("no entry") && new.contains("through libos"), "{new}");
    let rise = only(&b, &[("userspace/a", 6)]);
    assert!(rise.contains("over its entry of 5") && rise.contains("may not rise") && rise.contains("libos"), "{rise}");
    let low = only(&b, &[("userspace/a", 4)]);
    assert!(low.contains("under its entry of 5") && low.contains("check-syscalls --update"), "{low}");
    let zero = only(&b, &[("userspace/a", 0)]);
    assert!(zero.contains("counts zero") && zero.contains("--update"), "{zero}");
    let gone = only(&b, &[]);
    assert!(gone.contains("userspace/a") && gone.contains("no counted crate or module"), "{gone}");
    let agreed = only(&base(&[("userspace/a", 5)], &[("userspace/e", "why")]), &[("userspace/a", 5)]);
    assert!(agreed.contains("userspace/e") && agreed.contains("no entry in [counts]"), "{agreed}");
}

#[test]
fn update_lowers_and_removes_and_never_raises_and_keeps_agreed() {
    let b = base(&[("lower", 5), ("zero", 3), ("gone", 2), ("grew", 4)], &[("lower", "a reason")]);
    let a = actual(&[("lower", 3), ("zero", 0), ("grew", 9), ("new", 7)]);
    assert_eq!(updated(&a, &b), base(&[("lower", 3), ("grew", 4)], &[("lower", "a reason")]));
}

#[test]
fn the_baseline_parses_formats_and_refuses_a_bad_line() {
    let text = format!(
        "{HEADER}\"userspace/a\" = 5\n\"userspace/b/src/m\" = 2\n\n[agreed]\n\"userspace/a\" = \"by choice\"\n"
    );
    let parsed = parse_baseline(&text).unwrap();
    assert_eq!(parsed, base(&[("userspace/a", 5), ("userspace/b/src/m", 2)], &[("userspace/a", "by choice")]));
    assert_eq!(format_baseline(&parsed), text);
    for bad in [
        "[counts]\na = 1",
        "[counts]\n\"a\" = x",
        "[counts]\n\"a\" 1",
        "[agreed]\n\"a\" = why",
        "[agreed]\n\"a\" = \"\"",
        "\"a\" = 1",
    ] {
        assert!(parse_baseline(bad).is_err(), "{bad}");
    }
}

#[test]
fn the_checked_in_baseline_parses() {
    let text = fs::read_to_string(repo_root().join(BASELINE)).unwrap();
    let b = parse_baseline(&text).unwrap();
    assert!(b.agreed.keys().all(|g| b.counts.contains_key(g)));
}
