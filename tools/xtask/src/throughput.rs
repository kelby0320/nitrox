//! `throughput-stick`: the stick copies are timed with, written and read back.

use crate::*;

/// The label `throughput-stick` gives its FAT.
const THROUGHPUT_LABEL: &str = "THROUGHPUT";

/// **What the throughput stick holds**, as `(directory, name, bytes)` — the tree Phase 6 Part H's
/// laptop runs copy, the same every run (`docs/planning/phase-6-usb.md` § *Part H in detail*):
/// - `big/`: 32 files of 8 MiB — the per-byte cost, at the copy limit;
/// - `small/`: 4,096 files of 16 KiB in one directory — the per-file cost, and the directory scan;
/// - `tree/`: 16 directories of 64 files of 64 KiB — a home's shape.
///
/// **Every name is unique**, so a file's pattern can be its name's: a copy checks wherever it was
/// put and whatever its directory was called. **And every name is a FAT short name already** — eight
/// characters, a dot, three, as a camera names its photos — because `mcopy` given four thousand long
/// names alike searches for a free `~N` alias for each, scanning the directory for every candidate:
/// with `small-0000.bin` and its kind, writing the image ran past ten minutes. The check compares
/// names without case, since a short name's case is a matter of display.
pub(crate) fn throughput_files() -> Vec<(String, String, u64)> {
    const KIB: u64 = 1024;
    let mut out = Vec::new();
    for i in 0..32 {
        out.push(("big".to_string(), format!("B{i:02}.BIN"), 8 * 1024 * KIB));
    }
    for i in 0..4096 {
        out.push(("small".to_string(), format!("S{i:04}.BIN"), 16 * KIB));
    }
    for d in 0..16 {
        for i in 0..64 {
            out.push((format!("tree/d{d:02}"), format!("T{:04}.BIN", d * 64 + i), 64 * KIB));
        }
    }
    out
}

/// **A file's bytes, by its name**: xorshift64 seeded by the name's FNV-1a hash, eight bytes a step.
pub(crate) fn throughput_pattern(name: &str, len: u64) -> Vec<u8> {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    let mut x = h | 1;
    let mut out = Vec::with_capacity(len as usize + 8);
    while (out.len() as u64) < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len as usize);
    out
}

/// **`cargo xtask throughput-stick`** (Phase 6 Part H.2): write `tools/build-cache/throughput-stick.img`
/// — a GiB, sparse, an MBR naming one FAT32 partition from 1 MiB with 4 KiB clusters, holding
/// [`throughput_files`] and [`THROUGHPUT_SCRIPTS`] — to write to a stick. Half of it is free, for the
/// run that copies the tree back onto the stick. **`--check PATH`** reads an image — or the stick
/// read back into one, its device given the permission, or the stick mounted — and checks every
/// pattern file on it, wherever it is.
pub(crate) fn cmd_throughput_stick(args: &[String]) -> R<()> {
    if let Some(i) = args.iter().position(|a| a == "--check") {
        let path = args.get(i + 1).ok_or("`--check` takes the image or device to read")?;
        return check_throughput_stick(Path::new(path));
    }
    for tool in ["mformat", "mcopy"] {
        require_tool(tool)?;
    }
    const MIB: u64 = 1024 * 1024;
    let path = build_cache().join("throughput-stick.img");
    let _ = fs::remove_file(&path);
    fs::create_dir_all(build_cache())?;
    fs::File::create(&path)?.set_len(1024 * MIB)?;
    let count = (1024 * 2048 - 2048) as u32;
    write_mbr(&path, 2048, count, 0x0C)?;
    let at = format!("{}@@{MIB}", path.display());
    run(mtools_cmd("mformat")
        .arg("-i")
        .arg(&at)
        .arg("-F")
        .arg("-c")
        .arg("8")
        .arg("-T")
        .arg(count.to_string())
        .arg("-v")
        .arg(THROUGHPUT_LABEL)
        .arg("::"))?;
    let src = build_cache().join("throughput-src");
    let _ = fs::remove_dir_all(&src);
    let files = throughput_files();
    for (dir, name, len) in &files {
        fs::create_dir_all(src.join(dir))?;
        fs::write(src.join(dir).join(name), throughput_pattern(name, *len))?;
    }
    for (name, body) in THROUGHPUT_SCRIPTS {
        fs::write(src.join(name), body)?;
    }
    let mut mcopy = mtools_cmd("mcopy");
    mcopy.arg("-s").arg("-i").arg(&at).arg(src.join("big")).arg(src.join("small")).arg(src.join("tree"));
    for (name, _) in THROUGHPUT_SCRIPTS {
        mcopy.arg(src.join(name));
    }
    run(mcopy.arg("::"))?;
    fs::remove_dir_all(&src)?;
    let total: u64 = files.iter().map(|f| f.2).sum();
    println!(
        "xtask: the throughput stick at {}: {} files, {} MiB — big/ 32 of 8 MiB, small/ 4,096 of 16 KiB, \
         tree/ 16 directories of 64 of 64 KiB — and h3.nx and linux.sh, which run the sets.\n  Write it to a \
         stick with `sudo dd if={} of=/dev/sdX bs=4M conv=fsync,nocreat`, and check the device is a block \
         device first.",
        path.display(),
        files.len(),
        total / MIB,
        path.display()
    );
    Ok(())
}

/// **The scripts the stick carries** (Phase 6 Part H.3), as `(name on the stick, text)`: `h3.nx`
/// runs the laptop's sets in `nxsh`, one a boot, and `linux.sh` the same four under Linux on the
/// same laptop, writing its times to `RESULTS/` on the stick. The check counts them among the other
/// files and prints what `RESULTS/` holds.
const THROUGHPUT_SCRIPTS: [(&str, &str); 2] =
    [("h3.nx", include_str!("throughput/h3.nx")), ("linux.sh", include_str!("throughput/linux.sh"))];

/// **Every pattern file on a throughput stick, checked** (Phase 6 Part H.2): `img`'s first MBR
/// partition — or the whole of it, with no MBR — copied off by `mcopy`, every file whose name is
/// the stick's checked against its pattern wherever it is, and the tree `throughput-stick` wrote
/// required whole where it wrote it. Copies the laptop's runs made are checked too, and counted.
/// **`img` may be the stick mounted** (Part H.3) — a directory, read where the desktop put it, so
/// the check needs no `sudo` — and what `linux.sh` wrote to `RESULTS/` is printed either way.
fn check_throughput_stick(img: &Path) -> R<()> {
    use std::io::Read;
    let extracted = !img.is_dir();
    let out = if extracted { build_cache().join("throughput-check") } else { img.to_path_buf() };
    if extracted {
        require_tool("mcopy")?;
        let mut mbr = [0u8; 512];
        fs::File::open(img)?.read_exact(&mut mbr)?;
        let first = u32::from_le_bytes([mbr[454], mbr[455], mbr[456], mbr[457]]) as u64;
        let offset = if mbr[510..512] == [0x55, 0xAA] && mbr[450] != 0 && first != 0 { first * 512 } else { 0 };
        let _ = fs::remove_dir_all(&out);
        fs::create_dir_all(&out)?;
        run(mtools_cmd("mcopy").arg("-s").arg("-i").arg(format!("{}@@{offset}", img.display())).arg("::/*").arg(&out))?;
    }
    let mut found: Vec<PathBuf> = Vec::new();
    let mut stack = vec![out.clone()];
    while let Some(dir) = stack.pop() {
        for e in fs::read_dir(&dir)? {
            let p = e?.path();
            if p.is_dir() { stack.push(p) } else { found.push(p) }
        }
    }
    let lens: std::collections::HashMap<String, u64> =
        throughput_files().into_iter().map(|(_, name, len)| (name, len)).collect();
    let (mut good, mut bad, mut others) = (0usize, Vec::new(), 0usize);
    for p in &found {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_ascii_uppercase();
        let Some(&len) = lens.get(&name) else {
            others += 1;
            continue;
        };
        if fs::read(p)? == throughput_pattern(&name, len) {
            good += 1;
        } else {
            bad.push(p.strip_prefix(&out).unwrap_or(p).display().to_string());
        }
    }
    let present: std::collections::HashSet<String> =
        found.iter().filter_map(|p| p.strip_prefix(&out).ok()?.to_str().map(str::to_ascii_uppercase)).collect();
    let missing: Vec<String> = throughput_files()
        .iter()
        .map(|(dir, name, _)| format!("{dir}/{name}"))
        .filter(|path| !present.contains(&path.to_ascii_uppercase()))
        .collect();
    let mut results: Vec<&PathBuf> = found
        .iter()
        .filter(|p| p.parent().and_then(|d| d.file_name()).is_some_and(|d| d.eq_ignore_ascii_case("RESULTS")))
        .collect();
    results.sort();
    for p in results {
        println!("xtask: {}:\n{}", p.strip_prefix(&out).unwrap_or(p).display(), String::from_utf8_lossy(&fs::read(p)?));
    }
    if extracted {
        let _ = fs::remove_dir_all(&out);
    }
    if !bad.is_empty() || !missing.is_empty() {
        return Err(format!(
            "the throughput stick does not check: {} files differ from their patterns (first: {:?}), \
             {} of the tree it was written with are missing (first: {:?})",
            bad.len(),
            bad.first(),
            missing.len(),
            missing.first()
        )
        .into());
    }
    let tree = throughput_files().len();
    println!(
        "xtask: the throughput stick checks: {good} pattern files, every byte as written — the tree's \
         {tree} where it was written and {} copies — and {others} other files ✓",
        good - tree
    );
    Ok(())
}
