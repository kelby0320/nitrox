//! `unsafe-inventory`: what each crate or kernel module holds of `unsafe` and raw pointers, by kind,
//! counted the same way every run so two surveys of one tree agree. Test code is left out.

use super::comment_budget::{code_lines, git};
use crate::*;

/// Each kind, and the text that marks one in a line of code with its comments removed.
const KINDS: &[(&str, &[&str])] = &[
    ("unsafe block", &["unsafe {"]),
    ("unsafe fn", &["unsafe fn "]),
    ("unsafe impl", &["unsafe impl"]),
    ("static mut", &["static mut "]),
    ("raw syscall", &["syscall0(", "syscall1(", "syscall2(", "syscall3(", "syscall4(", "syscall5(", "syscall6("]),
    ("raw slice", &["from_raw_parts"]),
    ("raw deref", &["&*(", "&mut *("]),
    ("transmute", &["transmute"]),
    ("asm", &["asm!"]),
    ("volatile", &["read_volatile", "write_volatile"]),
];

/// The counts for one path: lines of code, then one per [`KINDS`] entry.
#[derive(Default, Debug, PartialEq)]
struct Counts {
    code: usize,
    kinds: [usize; KINDS.len()],
}

/// Whether `path` holds only test code by its name: a `tests.rs`, a `*_tests.rs`, a
/// `test_support.rs`, or anything under a `tests/` directory.
fn is_test_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "tests.rs" || name.ends_with("_tests.rs") || name == "test_support.rs" || path.contains("/tests/")
}

/// `text`'s code lines with every `#[cfg(test)]` item taken out: the attribute's next item, to its
/// `;` or to the `}` that closes it. A brace inside a string literal miscounts.
fn non_test_lines(text: &str) -> Vec<String> {
    let lines = code_lines(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if lines[i].trim() != "#[cfg(test)]" {
            out.push(lines[i].clone());
            i += 1;
            continue;
        }
        i += 1;
        while i < lines.len() && lines[i].trim_start().starts_with("#[") {
            i += 1;
        }
        let mut depth = 0i64;
        let mut opened = false;
        while i < lines.len() {
            for c in lines[i].chars() {
                match c {
                    '{' => {
                        depth += 1;
                        opened = true;
                    }
                    '}' => depth -= 1,
                    _ => {}
                }
            }
            let ends = if opened { depth <= 0 } else { lines[i].trim_end().ends_with(';') };
            i += 1;
            if ends {
                break;
            }
        }
    }
    out
}

/// How often `marker` occurs in `line` where no identifier runs into it from the left, so
/// `my_syscall1(` is not a raw syscall and `transmute_copy` still is a transmute.
fn occurrences(line: &str, marker: &str) -> usize {
    let mut n = 0;
    let mut from = 0;
    while let Some(at) = line[from..].find(marker) {
        let start = from + at;
        let before = line[..start].chars().next_back();
        let word_start = marker.starts_with(|c: char| c.is_alphanumeric() || c == '_');
        if !(word_start && before.is_some_and(|c| c.is_alphanumeric() || c == '_')) {
            n += 1;
        }
        from = start + marker.len();
    }
    n
}

fn count(text: &str) -> Counts {
    let mut c = Counts::default();
    for line in non_test_lines(text) {
        c.code += 1;
        for (k, (_, markers)) in KINDS.iter().enumerate() {
            c.kinds[k] += markers.iter().map(|m| occurrences(&line, m)).sum::<usize>();
        }
    }
    c
}

/// Every userspace crate and every entry of `kernel/src`: the groups with no paths given.
fn default_paths(root: &Path) -> R<Vec<String>> {
    let mut paths = Vec::new();
    for (dir, files_too) in [("userspace", false), ("kernel/src", true)] {
        let mut entries: Vec<_> = fs::read_dir(root.join(dir))?
            .filter_map(|e| e.ok())
            .filter(|e| {
                let p = e.path();
                (p.is_dir() && (files_too || p.join("Cargo.toml").exists()))
                    || (files_too && p.extension().is_some_and(|x| x == "rs"))
            })
            .map(|e| format!("{dir}/{}", e.file_name().to_string_lossy()))
            .collect();
        entries.sort();
        paths.extend(entries);
    }
    Ok(paths)
}

/// The counts for one path: its tracked Rust files, tests left out.
fn count_path(root: &Path, path: &str) -> R<Counts> {
    let mut total = Counts::default();
    for f in git(&["ls-files", "--", path])?.lines().filter(|f| f.ends_with(".rs") && !is_test_file(f)) {
        let c = count(&fs::read_to_string(root.join(f))?);
        total.code += c.code;
        for (t, n) in total.kinds.iter_mut().zip(c.kinds) {
            *t += n;
        }
    }
    Ok(total)
}

/// `unsafe-inventory [PATH...]`: a Markdown table, a row per path (every userspace crate and kernel
/// module when none is given), and their total.
pub(crate) fn cmd_unsafe_inventory(args: &[String]) -> R<()> {
    let root = repo_root();
    let paths = if args.is_empty() { default_paths(&root)? } else { args.to_vec() };
    let mut rows = Vec::new();
    let mut total = Counts::default();
    for p in &paths {
        let p = p.trim_end_matches('/');
        let c = count_path(&root, p)?;
        if c.code == 0 {
            return Err(format!("unsafe-inventory: no tracked Rust code under {p}").into());
        }
        total.code += c.code;
        for (t, n) in total.kinds.iter_mut().zip(c.kinds) {
            *t += n;
        }
        rows.push((p.to_string(), c));
    }
    rows.push(("total".to_string(), total));
    let head: Vec<&str> = KINDS.iter().map(|(name, _)| *name).collect();
    println!("| path | code lines | {} |", head.join(" | "));
    println!("|---|---:|{}", "---:|".repeat(KINDS.len()));
    for (p, c) in &rows {
        let cells: Vec<String> = c.kinds.iter().map(usize::to_string).collect();
        println!("| `{p}` | {} | {} |", c.code, cells.join(" | "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(c: &Counts, name: &str) -> usize {
        c.kinds[KINDS.iter().position(|(n, _)| *n == name).unwrap()]
    }

    #[test]
    fn a_marker_in_a_comment_is_not_counted_and_one_in_code_is() {
        let c = count("// unsafe { x }\nlet a = unsafe { f() }; // static mut B\n/// unsafe fn g()\n");
        assert_eq!((kind(&c, "unsafe block"), kind(&c, "static mut"), kind(&c, "unsafe fn")), (1, 0, 0));
    }

    #[test]
    fn a_test_module_and_a_test_item_are_left_out() {
        let text = "fn a() { unsafe { b() } }\n#[cfg(test)]\nmod tests {\n    fn t() {\n        unsafe { c() };\n    }\n}\n\
                    #[cfg(test)]\nmod more;\n#[cfg(test)]\n#[allow(dead_code)]\nfn helper() {\n    unsafe { d() };\n}\n\
                    fn e() { unsafe { f() } }\n";
        let c = count(text);
        assert_eq!((kind(&c, "unsafe block"), c.code), (2, 2));
    }

    #[test]
    fn an_identifier_running_into_a_marker_is_not_it() {
        let c =
            count("my_syscall1(a);\nlibkern::syscall1(SYS_X, a);\nsyscall4(1, 2, 3, 4);\nlet t = transmute_copy(x);\n");
        assert_eq!((kind(&c, "raw syscall"), kind(&c, "transmute")), (2, 1));
    }

    #[test]
    fn each_kind_is_found() {
        let text = "pub unsafe fn f() {}\nunsafe impl Send for S {}\nstatic mut B: u8 = 0;\n\
                    let s = core::slice::from_raw_parts_mut(p, n);\nlet r = &mut *(p as *mut T);\n\
                    core::arch::asm!(\"nop\");\nlet v = ptr.read_volatile();\n";
        let c = count(text);
        for name in ["unsafe fn", "unsafe impl", "static mut", "raw slice", "raw deref", "asm", "volatile"] {
            assert_eq!(kind(&c, name), 1, "{name}");
        }
    }

    #[test]
    fn test_files_are_known_by_name() {
        for p in [
            "kernel/src/sched/tests.rs",
            "a/b/write_tests.rs",
            "x/thread/test_support.rs",
            "k/src/sched/tests/timers.rs",
        ] {
            assert!(is_test_file(p), "{p}");
        }
        assert!(!is_test_file("kernel/src/sched/testing.rs"));
    }
}
