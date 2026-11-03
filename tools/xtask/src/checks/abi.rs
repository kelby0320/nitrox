//! `abi-sync-check`: userspace's copy of the kernel ABI has not drifted from it.

use crate::*;

/// One family of ABI constants mirrored between the kernel and `userspace/libkern`.
///
/// `pattern` is matched per line on both sides; capture group semantics are handled by
/// [`extract_consts`] (name, then value). The two sides use the *same* shape within a
/// family — `pub const SYS_X: u64 = N;` on both, `Name = -N,` enum variants on both — which
/// is what makes a line-based comparison sound here rather than needing a Rust parser.
struct AbiFamily {
    /// Human name for the report.
    what: &'static str,
    kernel_file: &'static str,
    /// The user-side files, whose constants are merged: one kernel file may mirror names that
    /// the user side keeps in several.
    user_files: &'static [&'static str],
    /// Which line shape to extract. See [`extract_consts`].
    shape: AbiShape,
    /// Names legitimately present on **one** side only, each with the reason. An
    /// unexplained one-sided name is a finding; a listed one is a documented asymmetry.
    /// Keeping this explicit is what stops the check from being noisy enough to disable.
    one_sided: &'static [(&'static str, &'static str)],
}

#[derive(Copy, Clone, PartialEq)]
enum AbiShape {
    /// `pub const NAME: u64 = <int>;`
    U64Const,
    /// `    Name = <int>,` — an enum variant with an explicit discriminant.
    EnumVariant,
    /// `pub const NAME: Rights = Rights(1 << k);`
    RightsBit,
    /// `pub const NAME: u16 = <int>;` — the input event classes and codes.
    U16Const,
    /// `pub const NAME: u32 = <int>;` — the device registry's constants.
    U32Const,
    /// `const NAME: u16|u32|usize = <int>;` or `<int> << <int>`, with or without `pub` — the
    /// resource-server protocol's constants. A value naming another constant is an alias and is
    /// not read.
    WireConst,
}

/// The ABI surfaces `userspace/libkern` mirrors by hand, and therefore the ones that can
/// silently drift. `#[repr(C)]` layout is deliberately **not** here: both sides carry
/// `offset_of!`/`size_of` compile-time asserts, a stronger check than text comparison.
const ABI_FAMILIES: &[AbiFamily] = &[
    AbiFamily {
        what: "syscall numbers",
        kernel_file: "kernel/src/syscall/table.rs",
        user_files: &["userspace/libkern/src/syscall.rs"],
        shape: AbiShape::U64Const,
        one_sided: &[("SYS_TEST_EXIT", "kernel test-harness builds only; userspace mirrors it for the harness")],
    },
    AbiFamily {
        what: "KError discriminants",
        kernel_file: "kernel/src/syscall/error.rs",
        user_files: &["userspace/libkern/src/error.rs"],
        shape: AbiShape::EnumVariant,
        one_sided: &[],
    },
    AbiFamily {
        what: "Rights bits",
        kernel_file: "kernel/src/libkern/handle.rs",
        user_files: &["userspace/libkern/src/handle.rs"],
        shape: AbiShape::RightsBit,
        one_sided: &[],
    },
    AbiFamily {
        what: "input event classes and codes",
        kernel_file: "kernel/src/libkern/input.rs",
        user_files: &["userspace/libkern/src/abi.rs"],
        shape: AbiShape::U16Const,
        one_sided: &[],
    },
    AbiFamily {
        what: "KObjectType discriminants",
        kernel_file: "kernel/src/libkern/handle.rs",
        user_files: &["userspace/libkern/src/handle.rs"],
        shape: AbiShape::EnumVariant,
        one_sided: &[],
    },
    // The device registry: its own file on each side, so the
    // enum-variant sweep sees `DeviceKind` and nothing else.
    AbiFamily {
        what: "device registry kinds",
        kernel_file: "kernel/src/libkern/device.rs",
        user_files: &["userspace/libkern/src/device.rs"],
        shape: AbiShape::EnumVariant,
        one_sided: &[],
    },
    AbiFamily {
        what: "device registry constants",
        kernel_file: "kernel/src/libkern/device.rs",
        user_files: &["userspace/libkern/src/device.rs"],
        shape: AbiShape::U32Const,
        one_sided: &[],
    },
    // The `sys_io_submit` opcodes. The kernel
    // states each value once as an `IO_OPCODE_*` const and builds `IoOpcode` from them, so the
    // names pair with `libkern`'s; the other `u32` consts in `abi.rs` are never looked up.
    AbiFamily {
        what: "I/O opcodes",
        kernel_file: "kernel/src/libkern/io_op.rs",
        user_files: &["userspace/libkern/src/abi.rs"],
        shape: AbiShape::U32Const,
        one_sided: &[],
    },
    // `sys_power`'s operations. The kernel states them in a file of their
    // own, so the sweep there finds nothing else; `libkern` keeps them beside `SYS_POWER`.
    AbiFamily {
        what: "power operations",
        kernel_file: "kernel/src/libkern/power.rs",
        user_files: &["userspace/libkern/src/syscall.rs"],
        shape: AbiShape::U64Const,
        one_sided: &[],
    },
    // What `sys_clock_set` says: the clock set and kept by the RTC, or
    // set for this boot only. The kernel's `u64` consts in `clock.rs` are these alone; `libkern`
    // keeps them beside `CLOCK_REALTIME`.
    AbiFamily {
        what: "clock-set results",
        kernel_file: "kernel/src/libkern/clock.rs",
        user_files: &["userspace/libkern/src/abi.rs"],
        shape: AbiShape::U64Const,
        one_sided: &[],
    },
    // TODO(rsproto-offsets): the reply bodies' field offsets are literals on both sides, which a
    // family cannot compare.
    // The resource-server protocol. The kernel keeps its copy in one file; `librsproto` spreads
    // the same names over four. The comparison runs from the kernel's names: a name the kernel
    // reads that `librsproto` lacks or has changed is the drift that matters, while the many
    // names only `librsproto` uses are not the kernel's to mirror.
    AbiFamily {
        what: "resource-server protocol constants",
        kernel_file: "kernel/src/rsproto.rs",
        user_files: &[
            "userspace/librsproto/src/lib.rs",
            "userspace/librsproto/src/namespace.rs",
            "userspace/librsproto/src/file.rs",
            "userspace/librsproto/src/error.rs",
        ],
        shape: AbiShape::WireConst,
        one_sided: &[
            (
                "REQUEST_ID_OFFSET",
                "the kernel stamps the request id itself; librsproto's encoder writes it at a fixed offset",
            ),
            ("FORGET_LEN", "librsproto names it FORGET_REQUEST_LEN; paired in ABI_PAIRS"),
            ("TOUCH_BY_ID_LEN", "librsproto names it TOUCH_REQUEST_LEN; paired in ABI_PAIRS"),
        ],
    },
];

/// Individually-named constants that mirror across the boundary under *different* names or
/// in unrelated files, so a family sweep cannot pair them. Each is `(kernel file, kernel
/// name, userspace file, userspace name)`. These bit in practice: `MAX_WAIT_HANDLES` and the
/// IPC limits are hand-copied with no compile-time tie between the two sides.
const ABI_PAIRS: &[(&str, &str, &str, &str)] = &[
    ("kernel/src/object/thread.rs", "MAX_WAIT_HANDLES", "userspace/libkern/src/abi.rs", "MAX_WAIT_HANDLES"),
    ("kernel/src/libkern/ipc.rs", "IPC_HANDLE_MAX", "userspace/libkern/src/abi.rs", "IPC_HANDLE_MAX"),
    ("kernel/src/libkern/ipc.rs", "IPC_MAX_QUEUE_DEPTH", "userspace/libkern/src/abi.rs", "IPC_MAX_QUEUE_DEPTH"),
    ("kernel/src/rsproto.rs", "FORGET_LEN", "userspace/librsproto/src/file.rs", "FORGET_REQUEST_LEN"),
    ("kernel/src/rsproto.rs", "TOUCH_BY_ID_LEN", "userspace/librsproto/src/file.rs", "TOUCH_REQUEST_LEN"),
    ("kernel/src/object/namespace.rs", "NS_SUBTREE_BASE_MAX", "userspace/libusers/src/lib.rs", "HOME_MAX"),
];

/// What a file declares in one shape: each value read, and each name whose value was not.
#[derive(Debug, Default)]
struct Consts {
    read: BTreeMap<String, i128>,
    /// Names whose value is neither an integer nor another constant's name, which nothing compares.
    unread: Vec<String>,
}

fn extract_consts(text: &str, shape: AbiShape) -> Consts {
    let mut out = Consts::default();
    let mut take = |name: &str, val: &str| match parse_int(val) {
        Some(v) => {
            out.read.insert(name.to_string(), v);
        }
        None if !is_alias(val) => out.unread.push(name.to_string()),
        None => {}
    };
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        match shape {
            AbiShape::U64Const => {
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                let Some((ty, val)) = tail.split_once('=') else { continue };
                if ty.trim() != "u64" {
                    continue;
                }
                take(name.trim(), val);
            }
            AbiShape::U32Const => {
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                let Some((ty, val)) = tail.split_once('=') else { continue };
                if ty.trim() != "u32" {
                    continue;
                }
                take(name.trim(), val);
            }
            AbiShape::U16Const => {
                // pub const NAME: u16 = <int>;  (and the i32 `KEY_*` values alongside them)
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                let Some((ty, val)) = tail.split_once('=') else { continue };
                if ty.trim() != "u16" && ty.trim() != "i32" {
                    continue;
                }
                take(name.trim(), val);
            }
            AbiShape::WireConst => {
                let rest = t.strip_prefix("pub ").unwrap_or(t);
                let Some(rest) = rest.strip_prefix("const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                let Some((ty, val)) = tail.split_once('=') else { continue };
                if !matches!(ty.trim(), "u16" | "u32" | "usize") {
                    continue;
                }
                take(name.trim(), val);
            }
            AbiShape::EnumVariant => {
                let Some(body) = t.strip_suffix(',') else { continue };
                let Some((name, val)) = body.split_once('=') else { continue };
                let name = name.trim();
                if name.is_empty()
                    || !name.chars().next().is_some_and(|c| c.is_ascii_uppercase())
                    || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    continue;
                }
                take(name, val);
            }
            AbiShape::RightsBit => {
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                if !tail.contains("Rights(") {
                    continue;
                }
                let Some((_, shifted)) = tail.split_once("1 <<") else { continue };
                let digits: String = shifted.trim_start().chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(k) = digits.parse::<u32>() {
                    take(name.trim(), &format!("1 << {k}"));
                }
            }
        }
    }
    out
}

/// The value as written, without its trailing `;` or `,` and any comment after it.
fn bare(s: &str) -> &str {
    s.split_once("//").map_or(s, |(v, _)| v).trim().trim_end_matches([';', ',']).trim()
}

/// Whether the value names another constant, which is compared under its own name.
fn is_alias(s: &str) -> bool {
    let t = bare(s);
    t.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

fn parse_int(s: &str) -> Option<i128> {
    let t = bare(s).replace('_', "");
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i128::from_str_radix(hex, 16).ok();
    }
    if let Some((base, shift)) = t.split_once("<<") {
        let (base, shift) = (base.trim().parse::<i128>().ok()?, shift.trim().parse::<u32>().ok()?);
        return base.checked_shl(shift);
    }
    t.parse::<i128>().ok()
}

fn extract_named(text: &str, want: &str) -> Option<i128> {
    for line in text.lines() {
        let t = line.trim();
        let rest = t.strip_prefix("pub ").unwrap_or(t);
        let Some(rest) = rest.strip_prefix("const ") else { continue };
        let Some((name, tail)) = rest.split_once(':') else { continue };
        if name.trim() != want {
            continue;
        }
        let Some((_, val)) = tail.split_once('=') else { continue };
        return parse_int(val);
    }
    None
}

/// Compare one family's kernel text with its user texts (one per entry of `user_files`),
/// appending each disagreement to `problems`. Returns how many names agree.
fn compare_family(fam: &AbiFamily, kernel_text: &str, user_texts: &[&str], problems: &mut Vec<String>) -> usize {
    let user_files = fam.user_files.join(", ");
    let Consts { read: k, unread } = extract_consts(kernel_text, fam.shape);
    for name in unread {
        problems.push(format!(
            "{}: {} in {} has a value the check cannot read — write it as an integer",
            fam.what, name, fam.kernel_file
        ));
    }
    // A name two user files give different values would compare against whichever came last.
    let mut u = BTreeMap::new();
    for (file, ut) in fam.user_files.iter().zip(user_texts) {
        for (name, v) in extract_consts(ut, fam.shape).read {
            if let Some((was, other)) = u.insert(name.clone(), (v, *file))
                && was != v
            {
                problems.push(format!("{}: {} is {} in {} but {} in {}", fam.what, name, was, other, v, file));
            }
        }
    }
    if k.is_empty() {
        problems.push(format!(
            "{}: extracted nothing from {} — the checker's pattern has gone stale, \
             which silently disables this family",
            fam.what, fam.kernel_file
        ));
        return 0;
    }
    let mut compared = 0;
    for (name, kv) in &k {
        match u.get(name) {
            Some((uv, _)) if uv == kv => compared += 1,
            Some((uv, file)) => {
                problems.push(format!("{}: {} is {} in {} but {} in {}", fam.what, name, kv, fam.kernel_file, uv, file))
            }
            None => {
                if !fam.one_sided.iter().any(|(n, _)| n == name) {
                    problems.push(format!(
                        "{}: {} exists in {} but not in {} — mirror it, or record it in \
                         the checker's `one_sided` list with a reason",
                        fam.what, name, fam.kernel_file, user_files
                    ));
                }
            }
        }
    }
    compared
}

/// `cargo xtask abi-sync-check` — verify `userspace/libkern` still mirrors the kernel ABI.
///
/// `libkern` is a **hand-maintained** copy of the kernel's syscall numbers, error and object
/// discriminants, rights bits, and shared limits, and nothing in the build ties the two. This
/// fails on a mismatch, a name the kernel has and userspace lacks, or an undocumented one-sided
/// name.
pub(crate) fn cmd_abi_sync_check() -> R<()> {
    let root = repo_root();
    let mut problems: Vec<String> = Vec::new();
    let mut compared = 0usize;

    for fam in ABI_FAMILIES {
        let kt =
            fs::read_to_string(root.join(fam.kernel_file)).map_err(|e| format!("read {}: {e}", fam.kernel_file))?;
        let mut uts = Vec::new();
        for f in fam.user_files {
            uts.push(fs::read_to_string(root.join(f)).map_err(|e| format!("read {f}: {e}"))?);
        }
        let user: Vec<&str> = uts.iter().map(String::as_str).collect();
        compared += compare_family(fam, &kt, &user, &mut problems);
    }

    match check_kerror_decode_table(&root) {
        Ok(n) => compared += n,
        Err(mut found) => problems.append(&mut found),
    }

    for (kf, kn, uf, un) in ABI_PAIRS {
        let kt = fs::read_to_string(root.join(kf)).map_err(|e| format!("read {kf}: {e}"))?;
        let ut = fs::read_to_string(root.join(uf)).map_err(|e| format!("read {uf}: {e}"))?;
        match (extract_named(&kt, kn), extract_named(&ut, un)) {
            (Some(a), Some(b)) if a == b => compared += 1,
            (Some(a), Some(b)) => problems.push(format!("shared limit: {kn} is {a} in {kf} but {un} is {b} in {uf}")),
            (a, _) => problems.push(format!(
                "shared limit: could not read {} — the checker's pattern has gone stale",
                if a.is_none() { format!("{kn} in {kf}") } else { format!("{un} in {uf}") }
            )),
        }
    }

    if !problems.is_empty() {
        let mut msg = String::from(
            "userspace/libkern and librsproto must mirror the kernel ABI exactly (docs/spec/syscall-abi.md); \
             these disagree:\n",
        );
        for p in &problems {
            msg.push_str("  ");
            msg.push_str(p);
            msg.push('\n');
        }
        return Err(msg.into());
    }
    println!("abi-sync-check: {compared} ABI value(s) agree between the kernel and userspace ✓");
    Ok(())
}

/// Verify `libkern::KError::from_i32` decodes **every** kernel `KError` variant.
///
/// Mirroring the enum is not enough: `from_i32` is a second, hand-written copy of the same
/// table, and its `_ => KernelError` catch-all — deliberate forward-compat — makes a *missing*
/// arm indistinguishable from an unknown code at runtime. A variant with matching
/// discriminants and no arm once decoded silently as `KernelError`, and the round-trip test in
/// `error.rs` missed it because that test enumerates variants by hand too. So this derives the
/// expected set from the **kernel's** enum.
fn check_kerror_decode_table(root: &Path) -> Result<usize, Vec<String>> {
    const KERNEL: &str = "kernel/src/syscall/error.rs";
    const USER: &str = "userspace/libkern/src/error.rs";

    let read = |p: &str| fs::read_to_string(root.join(p)).map_err(|e| vec![format!("KError decode: read {p}: {e}")]);
    let kt = read(KERNEL)?;
    let ut = read(USER)?;

    let kernel = extract_consts(&kt, AbiShape::EnumVariant).read;
    if kernel.is_empty() {
        return Err(vec![format!(
            "KError decode: extracted no variants from {KERNEL} — the checker's pattern has \
             gone stale, which silently disables this check"
        )]);
    }

    // `    -40 => KError::IoError,` → (-40, "IoError"). The wildcard arm and every other
    // line shape fall out on the integer parse.
    let mut arms: BTreeMap<i128, String> = BTreeMap::new();
    // The variant the `_` arm yields. It needs no explicit arm of its own: an unlisted
    // code already lands on it, including its own. Every *other* variant does.
    let mut catch_all: Option<String> = None;
    for line in ut.lines() {
        let t = line.trim();
        if t.starts_with("//") {
            continue;
        }
        let Some((lhs, rhs)) = t.split_once("=>") else { continue };
        let Some(name) = rhs.trim().trim_end_matches(',').strip_prefix("KError::") else {
            continue;
        };
        match lhs.trim() {
            "_" => catch_all = Some(name.to_string()),
            n => {
                let Ok(code) = n.parse::<i128>() else { continue };
                arms.insert(code, name.to_string());
            }
        }
    }
    if arms.is_empty() {
        return Err(vec![format!(
            "KError decode: found no `<int> => KError::Name` arms in {USER} — either \
             `from_i32` was rewritten in another shape, or the checker's pattern has gone \
             stale. Either way this check is no longer checking anything"
        )]);
    }

    let mut problems = Vec::new();
    for (name, value) in &kernel {
        if catch_all.as_deref() == Some(name.as_str()) {
            continue;
        }
        match arms.get(value) {
            Some(mapped) if mapped == name => {}
            Some(mapped) => problems.push(format!(
                "KError decode: {USER} decodes {value} as {mapped}, but {KERNEL} defines \
                 {value} as {name}"
            )),
            None => problems.push(format!(
                "KError decode: {KERNEL} defines {name} = {value}, but {USER}'s `from_i32` \
                 has no arm for it — it would decode as KernelError, silently"
            )),
        }
    }
    if problems.is_empty() { Ok(kernel.len()) } else { Err(problems) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn family(one_sided: &'static [(&'static str, &'static str)]) -> AbiFamily {
        AbiFamily {
            what: "test",
            kernel_file: "k.rs",
            user_files: &["a.rs", "b.rs"],
            shape: AbiShape::WireConst,
            one_sided,
        }
    }

    #[test]
    fn wire_const_reads_each_integer_form() {
        let text = "const A: u32 = 0x5253_4D47;\npub const B: u16 = 7;\n    const C: usize = 24;\n\
                    pub const D: u32 = 1 << 2;\nconst E: u16 = OP_A;\nconst F: u64 = 3;\n// const G: u16 = 1;\n";
        let got = extract_consts(text, AbiShape::WireConst);
        assert_eq!(got.read.get("A"), Some(&0x5253_4D47));
        assert_eq!(got.read.get("B"), Some(&7));
        assert_eq!(got.read.get("C"), Some(&24));
        assert_eq!(got.read.get("D"), Some(&4));
        assert_eq!(got.read.len(), 4, "an alias, a u64 and a comment are not read: {got:?}");
        assert!(got.unread.is_empty(), "{got:?}");
    }

    /// A kernel value the check cannot read is named, never left out of the comparison: a comment
    /// after it is read past, and an expression or a cast is a finding.
    #[test]
    fn a_kernel_value_the_check_cannot_read_is_a_finding() {
        let user = ["pub const A: u32 = 64;\npub const B: u16 = 3;\n", ""];
        let mut problems = Vec::new();
        compare_family(&family(&[]), "const A: u32 = 1 << 6; // moved\nconst B: u16 = 3;\n", &user, &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
        compare_family(&family(&[]), "const A: u32 = 1 << 7; // moved\nconst B: u16 = 3;\n", &user, &mut problems);
        assert_eq!(problems.len(), 1, "the value past the comment is compared: {problems:?}");
        for kernel in ["const A: u32 = (1 << 6) | 0;\n", "const A: u32 = 64 as u32;\n"] {
            let mut problems = Vec::new();
            compare_family(&family(&[]), &[kernel, "const B: u16 = 3;\n"].concat(), &user, &mut problems);
            assert_eq!(problems.len(), 1, "{kernel}");
            assert!(problems[0].contains("A in k.rs has a value the check cannot read"), "{problems:?}");
        }
    }

    #[test]
    fn user_files_are_merged_so_a_name_may_live_in_either() {
        let fam = family(&[]);
        let mut problems = Vec::new();
        let n = compare_family(
            &fam,
            "const A: u16 = 1;\nconst B: u16 = 2;\n",
            &["pub const A: u16 = 1;\n", "pub const B: u16 = 2;\n"],
            &mut problems,
        );
        assert_eq!((n, problems.len()), (2, 0), "{problems:?}");
    }

    #[test]
    fn a_value_differing_on_the_user_side_is_named() {
        let fam = family(&[]);
        let mut problems = Vec::new();
        compare_family(&fam, "const A: u16 = 1;\n", &["", "pub const A: u16 = 2;\n"], &mut problems);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains(" A ") && problems[0].ends_with("in b.rs"), "{problems:?}");
    }

    #[test]
    fn a_name_two_user_files_disagree_on_is_a_finding_whichever_the_kernel_matches() {
        for user in [
            ["pub const A: u16 = 1;\n", "pub const A: u16 = 2;\n"],
            ["pub const A: u16 = 2;\n", "pub const A: u16 = 1;\n"],
        ] {
            let mut problems = Vec::new();
            compare_family(&family(&[]), "const A: u16 = 1;\n", &user, &mut problems);
            assert!(problems.iter().any(|p| p.contains("a.rs") && p.contains("b.rs")), "{problems:?}");
        }
    }

    #[test]
    fn an_unlisted_kernel_only_name_is_a_finding_and_a_listed_one_is_not() {
        let kernel = "const A: u16 = 1;\nconst ONLY: usize = 8;\n";
        let user = ["pub const A: u16 = 1;\n", ""];
        let mut problems = Vec::new();
        compare_family(&family(&[]), kernel, &user, &mut problems);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("ONLY"));
        let mut problems = Vec::new();
        compare_family(&family(&[("ONLY", "reason")]), kernel, &user, &mut problems);
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_user_only_name_is_not_a_finding() {
        let mut problems = Vec::new();
        compare_family(
            &family(&[]),
            "const A: u16 = 1;\n",
            &["pub const A: u16 = 1;\npub const EXTRA: u16 = 9;\n", ""],
            &mut problems,
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[test]
    fn a_kernel_file_yielding_nothing_is_a_finding() {
        let mut problems = Vec::new();
        compare_family(&family(&[]), "fn f() {}\n", &["", ""], &mut problems);
        assert_eq!(problems.len(), 1);
    }
}
