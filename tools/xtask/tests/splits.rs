use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The name a top-level item line declares: `pub(super) unsafe fn x(` is `x`.
fn item_name(line: &str) -> Option<&str> {
    let mut rest = line;
    if let Some(r) = rest.strip_prefix("pub") {
        rest = r.strip_prefix(' ').or_else(|| r.find(") ").map(|i| &r[i + 2..]))?;
    }
    loop {
        rest = match rest.split_once(' ') {
            Some(("unsafe" | "async", r)) => r,
            Some(("extern", r)) => r.split_once(' ')?.1,
            Some(("const", r)) if r.starts_with("fn ") => r,
            Some(("static", r)) => return ident(r.strip_prefix("mut ").unwrap_or(r)),
            Some(("fn" | "const" | "struct" | "enum" | "union" | "type" | "trait", r)) => return ident(r),
            _ => return None,
        };
    }
}

fn ident(s: &str) -> Option<&str> {
    let end = s.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(s.len());
    (end > 0).then(|| &s[..end])
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// The first line of `text` that is not blank or a module doc.
fn opening(text: &str) -> Option<&str> {
    text.lines().find(|l| !l.is_empty() && !l.starts_with("//!"))
}

/// The file declaring `child` as a module, and whether it declares it for tests only.
fn declarer(child: &Path) -> Option<(PathBuf, bool)> {
    let name = child.file_stem()?.to_str()?;
    let dir = child.parent()?;
    let candidates = [dir.with_extension("rs"), dir.join("mod.rs"), dir.join("main.rs"), dir.join("lib.rs")];
    candidates.into_iter().filter(|p| p != child && p.is_file()).find_map(|p| {
        let text = std::fs::read_to_string(&p).ok()?;
        let lines: Vec<&str> = text.lines().collect();
        let at = lines.iter().position(|l| {
            ["mod ", "pub mod ", "pub(crate) mod ", "pub(super) mod "]
                .iter()
                .any(|v| l.strip_prefix(v).and_then(|r| r.strip_suffix(';')) == Some(name))
        })?;
        Some((p, at > 0 && lines[at - 1] == "#[cfg(test)]"))
    })
}

/// The crate root above `file`: its `src/`'s `main.rs` or `lib.rs`.
fn crate_root(file: &Path) -> Option<PathBuf> {
    let src = file.ancestors().find(|a| a.file_name().is_some_and(|n| n == "src"))?;
    ["main.rs", "lib.rs"].into_iter().map(|r| src.join(r)).find(|p| p.is_file())
}

/// A split module's pieces glob-import the module they came from, so a name declared in two of
/// them is never E0428: an item added beside a piece's same-named one silently takes the callers
/// that reach it through the glob. Within one file a name may repeat (a `cfg(test)` pair).
#[test]
fn a_split_modules_pieces_declare_no_name_twice() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    for tree in ["kernel/src", "userspace", "tools/xtask/src"] {
        rust_files(&root.join(tree), &mut files);
    }
    let mut families: BTreeMap<PathBuf, Vec<PathBuf>> = BTreeMap::new();
    for file in &files {
        let text = std::fs::read_to_string(file).unwrap();
        let parent = match opening(&text) {
            Some("use super::*;") => match declarer(file) {
                Some((_, true)) | None => continue,
                Some((parent, false)) => parent,
            },
            Some("use crate::*;") => match crate_root(file) {
                Some(r) => r,
                None => continue,
            },
            _ => continue,
        };
        families.entry(parent).or_default().push(file.clone());
    }
    assert!(families.keys().any(|p| p.ends_with("kernel/src/sched.rs")), "{:?}", families.keys());
    for (parent, pieces) in &families {
        let mut seen: BTreeMap<String, PathBuf> = BTreeMap::new();
        for file in std::iter::once(parent).chain(pieces) {
            let text = std::fs::read_to_string(file).unwrap();
            for name in text.lines().filter_map(item_name) {
                if let Some(first) = seen.get(name)
                    && first != file
                {
                    panic!("`{name}` is declared in {} and in {}", first.display(), file.display());
                }
                seen.insert(name.to_string(), file.clone());
            }
        }
    }
}
