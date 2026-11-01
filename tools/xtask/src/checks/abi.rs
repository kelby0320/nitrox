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
    user_file: &'static str,
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
}

/// The ABI surfaces `userspace/libkern` mirrors by hand, and therefore the ones that can
/// silently drift. `#[repr(C)]` layout is deliberately **not** here: both sides carry
/// `offset_of!`/`size_of` compile-time asserts, a stronger check than text comparison.
const ABI_FAMILIES: &[AbiFamily] = &[
    AbiFamily {
        what: "syscall numbers",
        kernel_file: "kernel/src/syscall/table.rs",
        user_file: "userspace/libkern/src/syscall.rs",
        shape: AbiShape::U64Const,
        one_sided: &[("SYS_TEST_EXIT", "kernel test-harness builds only; userspace mirrors it for the harness")],
    },
    AbiFamily {
        what: "KError discriminants",
        kernel_file: "kernel/src/syscall/error.rs",
        user_file: "userspace/libkern/src/error.rs",
        shape: AbiShape::EnumVariant,
        one_sided: &[],
    },
    AbiFamily {
        what: "Rights bits",
        kernel_file: "kernel/src/libkern/handle.rs",
        user_file: "userspace/libkern/src/handle.rs",
        shape: AbiShape::RightsBit,
        one_sided: &[],
    },
    AbiFamily {
        what: "input event classes and codes",
        kernel_file: "kernel/src/libkern/input.rs",
        user_file: "userspace/libkern/src/abi.rs",
        shape: AbiShape::U16Const,
        one_sided: &[],
    },
    AbiFamily {
        what: "KObjectType discriminants",
        kernel_file: "kernel/src/libkern/handle.rs",
        user_file: "userspace/libkern/src/handle.rs",
        shape: AbiShape::EnumVariant,
        one_sided: &[],
    },
    // The device registry: its own file on each side, so the
    // enum-variant sweep sees `DeviceKind` and nothing else.
    AbiFamily {
        what: "device registry kinds",
        kernel_file: "kernel/src/libkern/device.rs",
        user_file: "userspace/libkern/src/device.rs",
        shape: AbiShape::EnumVariant,
        one_sided: &[],
    },
    AbiFamily {
        what: "device registry constants",
        kernel_file: "kernel/src/libkern/device.rs",
        user_file: "userspace/libkern/src/device.rs",
        shape: AbiShape::U32Const,
        one_sided: &[],
    },
    // The `sys_io_submit` opcodes. The kernel
    // states each value once as an `IO_OPCODE_*` const and builds `IoOpcode` from them, so the
    // names pair with `libkern`'s; the other `u32` consts in `abi.rs` are never looked up.
    AbiFamily {
        what: "I/O opcodes",
        kernel_file: "kernel/src/libkern/io_op.rs",
        user_file: "userspace/libkern/src/abi.rs",
        shape: AbiShape::U32Const,
        one_sided: &[],
    },
    // `sys_power`'s operations. The kernel states them in a file of their
    // own, so the sweep there finds nothing else; `libkern` keeps them beside `SYS_POWER`.
    AbiFamily {
        what: "power operations",
        kernel_file: "kernel/src/libkern/power.rs",
        user_file: "userspace/libkern/src/syscall.rs",
        shape: AbiShape::U64Const,
        one_sided: &[],
    },
    // What `sys_clock_set` says: the clock set and kept by the RTC, or
    // set for this boot only. The kernel's `u64` consts in `clock.rs` are these alone; `libkern`
    // keeps them beside `CLOCK_REALTIME`.
    AbiFamily {
        what: "clock-set results",
        kernel_file: "kernel/src/libkern/clock.rs",
        user_file: "userspace/libkern/src/abi.rs",
        shape: AbiShape::U64Const,
        one_sided: &[],
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
    ("kernel/src/object/namespace.rs", "NS_SUBTREE_BASE_MAX", "userspace/libusers/src/lib.rs", "HOME_MAX"),
];

fn extract_consts(text: &str, shape: AbiShape) -> BTreeMap<String, i128> {
    let mut out = BTreeMap::new();
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
                if let Some(v) = parse_int(val) {
                    out.insert(name.trim().to_string(), v);
                }
            }
            AbiShape::U32Const => {
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                let Some((ty, val)) = tail.split_once('=') else { continue };
                if ty.trim() != "u32" {
                    continue;
                }
                if let Some(v) = parse_int(val) {
                    out.insert(name.trim().to_string(), v);
                }
            }
            AbiShape::U16Const => {
                // pub const NAME: u16 = <int>;  (and the i32 `KEY_*` values alongside them)
                let Some(rest) = t.strip_prefix("pub const ") else { continue };
                let Some((name, tail)) = rest.split_once(':') else { continue };
                let Some((ty, val)) = tail.split_once('=') else { continue };
                if ty.trim() != "u16" && ty.trim() != "i32" {
                    continue;
                }
                if let Some(v) = parse_int(val) {
                    out.insert(name.trim().to_string(), v);
                }
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
                if let Some(v) = parse_int(val) {
                    out.insert(name.to_string(), v);
                }
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
                    out.insert(name.trim().to_string(), 1i128 << k);
                }
            }
        }
    }
    out
}

fn parse_int(s: &str) -> Option<i128> {
    let t = s.trim().trim_end_matches([';', ',']).trim().replace('_', "");
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        return i128::from_str_radix(hex, 16).ok();
    }
    t.parse::<i128>().ok()
}

fn extract_named(text: &str, want: &str) -> Option<i128> {
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("pub const ") else { continue };
        let Some((name, tail)) = rest.split_once(':') else { continue };
        if name.trim() != want {
            continue;
        }
        let Some((_, val)) = tail.split_once('=') else { continue };
        return parse_int(val);
    }
    None
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
        let ut = fs::read_to_string(root.join(fam.user_file)).map_err(|e| format!("read {}: {e}", fam.user_file))?;
        let k = extract_consts(&kt, fam.shape);
        let u = extract_consts(&ut, fam.shape);
        if k.is_empty() {
            problems.push(format!(
                "{}: extracted nothing from {} — the checker's pattern has gone stale, \
                 which silently disables this family",
                fam.what, fam.kernel_file
            ));
            continue;
        }
        for (name, kv) in &k {
            match u.get(name) {
                Some(uv) if uv == kv => compared += 1,
                Some(uv) => problems.push(format!(
                    "{}: {} is {} in {} but {} in {}",
                    fam.what, name, kv, fam.kernel_file, uv, fam.user_file
                )),
                None => {
                    if !fam.one_sided.iter().any(|(n, _)| n == name) {
                        problems.push(format!(
                            "{}: {} exists in {} but not in {} — mirror it, or record it in \
                             the checker's `one_sided` list with a reason",
                            fam.what, name, fam.kernel_file, fam.user_file
                        ));
                    }
                }
            }
        }
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
            "userspace/libkern must mirror the kernel ABI exactly (docs/spec/syscall-abi.md); \
             these disagree:\n",
        );
        for p in &problems {
            msg.push_str("  ");
            msg.push_str(p);
            msg.push('\n');
        }
        return Err(msg.into());
    }
    println!("abi-sync-check: {compared} ABI value(s) agree between the kernel and libkern ✓");
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

    let kernel = extract_consts(&kt, AbiShape::EnumVariant);
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
