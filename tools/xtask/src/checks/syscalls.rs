//! `check-syscalls`: the raw syscalls each userspace crate makes only fall, against
//! `tools/xtask/raw-syscalls.toml`.
//!
//! A raw syscall is a `syscallN(` as `unsafe-inventory` counts it, a call of a free function of
//! `libkern` that makes one, or the making of a `libkern` type one of whose own methods makes one,
//! as `debug::Line`'s `end` does: each `Line::new(` counts once. That list is derived from
//! `libkern`'s source each run: a `pub fn` at the top level of a file whose non-test body calls
//! `syscallN(`, issues one by inline `asm!` naming a `SYS_` number, or calls another function on the
//! list, and a type with an inherent `impl` whose body does any of those.
//! `libkern` and `libos`, the syscall layer itself, are not counted.
//!
//! A wrapper call counts when it is written `libkern::name(` or `libkern::module::name(`; as
//! `module::name(` in a file importing that module; or as a bare `name(` in a file importing it or
//! globbing its module, when the crate defines no free function of that name. A method call, a
//! definition and a longer identifier are not calls. What this does not resolve: a `use ... as`
//! rename, a re-export of `libkern`'s items through another crate's path, a private helper that
//! makes the syscall for a public function (its callers count only if it is itself on the list),
//! a macro that expands to a call, and `libkern` called under another name for the crate.
//!
//! An entry is a crate (`userspace/init`) or a module of one (`userspace/x/src/m`, which is
//! `m.rs` and `m/**`). A file counts toward the longest entry that holds it.

use super::comment_budget::git;
use super::unsafe_inventory::{SYSCALLS, blanked, in_group, is_test_file, non_test_lines, occurrences};
use crate::*;
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;

const BASELINE: &str = "tools/xtask/raw-syscalls.toml";
const HEADER: &str = "# The raw syscalls each userspace crate or module makes: a `syscallN(` or a call of one of\n\
                      # libkern's own wrappers, counted by `cargo xtask check-syscalls`. A count only falls: reach the\n\
                      # kernel through libos. `--update` lowers or removes entries; a raise is by hand, in a reviewed\n\
                      # commit of its own that says why. `[agreed]` names the crates the maintainer agreed may stay\n\
                      # on libkern, each with its reason; their counts are still held here.\n\
                      [counts]\n";

/// The checked-in baseline: a count per entry, and the reason for each agreed exception.
#[derive(Default, Debug, PartialEq)]
struct Baseline {
    counts: BTreeMap<String, usize>,
    agreed: BTreeMap<String, String>,
}

fn parse_baseline(text: &str) -> R<Baseline> {
    let mut out = Baseline::default();
    let mut section = "";
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if t == "[counts]" || t == "[agreed]" {
            section = if t == "[counts]" { "counts" } else { "agreed" };
            continue;
        }
        let bad = || {
            format!("{BASELINE}: not `\"path\" = {}`: {line}", if section == "agreed" { "\"reason\"" } else { "count" })
        };
        let (path, value) = t.strip_prefix('"').and_then(|r| r.split_once("\" = ")).ok_or_else(bad)?;
        match section {
            "counts" => {
                out.counts.insert(path.to_string(), value.parse().map_err(|_| bad())?);
            }
            "agreed" => {
                let reason = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')).filter(|v| !v.is_empty());
                out.agreed.insert(path.to_string(), reason.ok_or_else(bad)?.to_string());
            }
            _ => return Err(format!("{BASELINE}: an entry outside `[counts]` and `[agreed]`: {line}").into()),
        }
    }
    Ok(out)
}

fn format_baseline(b: &Baseline) -> String {
    let mut out = HEADER.to_string();
    for (path, n) in &b.counts {
        out.push_str(&format!("\"{path}\" = {n}\n"));
    }
    out.push_str("\n[agreed]\n");
    for (path, reason) in &b.agreed {
        out.push_str(&format!("\"{path}\" = \"{reason}\"\n"));
    }
    out
}

/// A free function of `libkern` that makes a syscall, or a type whose method does, counted where
/// it is made.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Wrapper {
    /// The file it is in, without `.rs`; empty in `lib.rs`.
    module: String,
    name: String,
    /// Whether `libkern`'s root re-exports it, so `libkern::name(` and a glob of `libkern` reach it.
    top: bool,
    /// Whether `name` is a type, each `name::new(` a call.
    ctor: bool,
}

impl Wrapper {
    fn path(&self) -> String {
        let item = if self.ctor { format!("{}::new", self.name) } else { self.name.clone() };
        if self.module.is_empty() { item } else { format!("{}::{item}", self.module) }
    }
}

/// What `libkern` offers: its wrappers, and the names of its modules.
struct Api {
    wrappers: Vec<Wrapper>,
    modules: BTreeSet<String>,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// The start of each call of `name` in `line`: `name(` with no identifier or `.` before it and not
/// the `fn` that defines it.
fn call_sites(line: &str, name: &str) -> Vec<usize> {
    let marker = format!("{name}(");
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = line[from..].find(&marker) {
        let start = from + at;
        let before = &line[..start];
        if !before.chars().next_back().is_some_and(|c| is_ident(c) || c == '.') && !before.ends_with("fn ") {
            out.push(start);
        }
        from = start + marker.len();
    }
    out
}

/// The `::`-separated path written right before `start`, as `libkern::control` for
/// `libkern::control::recv(`. Empty for a bare name.
fn path_before(line: &str, start: usize) -> Vec<&str> {
    let mut segs = Vec::new();
    let mut rest = &line[..start];
    while let Some(r) = rest.strip_suffix("::") {
        let begin = r.rfind(|c: char| !is_ident(c)).map_or(0, |i| i + 1);
        segs.push(&r[begin..]);
        rest = &r[..begin];
    }
    segs.reverse();
    segs
}

/// The free functions declared at the top level of `lines`, each with its body's lines.
fn top_level_fns(lines: &[String]) -> Vec<(String, Vec<&str>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        let Some(rest) = line.strip_prefix("pub fn ").or_else(|| line.strip_prefix("pub unsafe fn ")) else {
            i += 1;
            continue;
        };
        let name: String = rest.chars().take_while(|&c| is_ident(c)).collect();
        let (mut depth, mut opened, mut body) = (0i64, false, Vec::new());
        while i < lines.len() && !(opened && depth <= 0) {
            body.push(lines[i].as_str());
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
            i += 1;
        }
        out.push((name, body));
    }
    out
}

/// Each inherent `impl` at the top level of `lines`: its type's name and its body's lines.
fn inherent_impls(lines: &[String]) -> Vec<(String, Vec<&str>)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let Some(rest) = lines[i].strip_prefix("impl") else {
            i += 1;
            continue;
        };
        let head = lines[i].split('{').next().unwrap_or("");
        let after = match rest.strip_prefix('<') {
            Some(g) => g.split_once("> ").map_or("", |(_, t)| t),
            None => rest.trim_start(),
        };
        let name: String = after.chars().take_while(|&c| is_ident(c)).collect();
        let (mut depth, mut opened, mut body) = (0i64, false, Vec::new());
        while i < lines.len() && !(opened && depth <= 0) {
            body.push(lines[i].as_str());
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
            i += 1;
        }
        if !name.is_empty() && !head.contains(" for ") {
            out.push((name, body));
        }
    }
    out
}

/// The `use` statements in `lines`, each as its tree and whether it is `pub`, joined across lines.
fn use_statements(lines: &[String]) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let mut open: Option<(bool, String)> = None;
    for line in lines {
        let t = line.trim();
        if open.is_none() {
            let (public, rest) = match t.strip_prefix("pub use ").or_else(|| t.strip_prefix("pub(crate) use ")) {
                Some(r) => (true, r),
                None => match t.strip_prefix("use ") {
                    Some(r) => (false, r),
                    None => continue,
                },
            };
            open = Some((public, rest.to_string()));
        } else if let Some((_, text)) = open.as_mut() {
            text.push(' ');
            text.push_str(t);
        }
        if open.as_ref().is_some_and(|(_, text)| text.contains(';'))
            && let Some((public, text)) = open.take()
        {
            out.push((public, text.split(';').next().unwrap_or("").trim().to_string()));
        }
    }
    out
}

/// Every path a `use` tree names, a glob as a last segment `*`. An `as` rename names nothing.
fn expand(prefix: &[String], tree: &str) -> Vec<Vec<String>> {
    let tree = tree.trim();
    if let Some(inner) = tree.strip_prefix('{').and_then(|t| t.strip_suffix('}')) {
        let (mut depth, mut start, mut parts) = (0, 0, Vec::new());
        for (i, c) in inner.char_indices() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                ',' if depth == 0 => {
                    parts.push(&inner[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        parts.push(&inner[start..]);
        return parts.into_iter().filter(|p| !p.trim().is_empty()).flat_map(|p| expand(prefix, p)).collect();
    }
    if tree.contains(" as ") {
        return Vec::new();
    }
    match tree.split_once("::") {
        Some((head, rest)) => {
            let mut next = prefix.to_vec();
            next.push(head.trim().to_string());
            expand(&next, rest)
        }
        None => {
            let mut path = prefix.to_vec();
            if tree != "self" {
                path.push(tree.to_string());
            }
            vec![path]
        }
    }
}

/// `libkern`'s wrappers and modules from its source files, each a path and its text.
fn derive_api(files: &[(String, String)]) -> R<Api> {
    let mut fns = Vec::new();
    let mut impls = Vec::new();
    let (mut modules, mut reexports) = (BTreeSet::new(), Vec::new());
    for (path, text) in files.iter().filter(|(p, _)| p.ends_with(".rs") && !is_test_file(p)) {
        let stem = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".rs");
        let module = if stem == "lib" { "" } else { stem };
        let lines = non_test_lines(blanked(text)).map_err(|e| format!("{path}: {e}"))?;
        if module.is_empty() {
            for (public, tree) in use_statements(&lines) {
                if public {
                    reexports.extend(expand(&[], &tree));
                }
            }
        } else {
            modules.insert(module.to_string());
        }
        for (name, body) in top_level_fns(&lines) {
            if !(name.starts_with("syscall") && name[7..].chars().all(|c| c.is_ascii_digit())) {
                fns.push((module.to_string(), name, body.join("\n")));
            }
        }
        for (name, body) in inherent_impls(&lines) {
            impls.push((module.to_string(), name, body.join("\n")));
        }
    }
    let makes_syscall = |body: &str| {
        body.lines().any(|l| SYSCALLS.iter().any(|m| occurrences(l, m) > 0))
            || (body.contains("asm!(") && body.contains("SYS_"))
    };
    let mut listed: BTreeSet<(String, String)> = BTreeSet::new();
    loop {
        let before = listed.len();
        for (module, name, body) in &fns {
            let calls_listed = || {
                let names: BTreeSet<&str> = listed.iter().map(|(_, n)| n.as_str()).collect();
                names.iter().any(|n| body.lines().any(|l| !call_sites(l, n).is_empty()))
            };
            if makes_syscall(body) || calls_listed() {
                listed.insert((module.clone(), name.clone()));
            }
        }
        if listed.len() == before {
            break;
        }
    }
    let names: BTreeSet<&str> = listed.iter().map(|(_, n)| n.as_str()).collect();
    let types: BTreeSet<(String, String)> = impls
        .iter()
        .filter(|(_, _, body)| {
            makes_syscall(body) || names.iter().any(|n| body.lines().any(|l| !call_sites(l, n).is_empty()))
        })
        .map(|(m, n, _)| (m.clone(), n.clone()))
        .collect();
    let reexported = |module: &str, name: &str| {
        module.is_empty() || reexports.iter().any(|p| p[..] == [module, name] || p[..] == [module, "*"])
    };
    let wrappers = listed
        .into_iter()
        .map(|(module, name)| (module, name, false))
        .chain(types.into_iter().map(|(module, name)| (module, name, true)))
        .map(|(module, name, ctor)| Wrapper { top: reexported(&module, &name), module, name, ctor })
        .collect();
    Ok(Api { wrappers, modules })
}

/// The name of the type a line declares: a `struct`, an `enum` or a `type`.
fn defined_type(line: &str) -> Option<String> {
    let mut tokens = line.split_whitespace().skip_while(|t| *t == "pub" || t.starts_with("pub("));
    matches!(tokens.next()?, "struct" | "enum" | "type")
        .then(|| tokens.next().map(|t| t.chars().take_while(|&c| is_ident(c)).collect()))
        .flatten()
        .filter(|n: &String| !n.is_empty())
}

/// The name of the free function a line declares, if it declares one rather than a method.
fn defined_fn(line: &str) -> Option<String> {
    let mut tokens = line.split_whitespace().peekable();
    while let Some(t) = tokens.peek() {
        if matches!(*t, "pub" | "const" | "async" | "unsafe" | "extern") || t.starts_with("pub(") || t.starts_with('"')
        {
            tokens.next();
        } else {
            break;
        }
    }
    if tokens.next()? != "fn" {
        return None;
    }
    let sig = line.split_once("fn ")?.1;
    let name: String = sig.chars().take_while(|&c| is_ident(c)).collect();
    let params = sig.split(')').next().unwrap_or("");
    (!name.is_empty() && !params.split(|c: char| !is_ident(c)).any(|w| w == "self")).then_some(name)
}

/// How a file reaches `libkern`'s wrappers: the modules it imports, the wrapper paths it imports
/// by name, and the modules whose names it globs.
#[derive(Default)]
struct Imports {
    modules: BTreeSet<String>,
    names: BTreeSet<String>,
    globs: BTreeSet<String>,
    root_glob: bool,
}

fn imports(lines: &[String], api: &Api) -> Imports {
    let mut out = Imports::default();
    for (_, tree) in use_statements(lines) {
        for path in expand(&[], &tree) {
            let [first, rest @ ..] = path.as_slice() else { continue };
            if first != "libkern" {
                continue;
            }
            match rest {
                [m] if m == "*" => out.root_glob = true,
                [m] if api.modules.contains(m) => {
                    out.modules.insert(m.clone());
                }
                [name] => {
                    out.names.insert(name.clone());
                }
                [m, last] if last == "*" => {
                    out.globs.insert(m.clone());
                }
                [m, name] if api.modules.contains(m) => {
                    out.names.insert(format!("{m}::{name}"));
                }
                _ => {}
            }
        }
    }
    out
}

/// The raw syscalls in `text`, a crate's file, given the crate's own free functions.
fn count_file(text: &str, api: &Api, locals: &BTreeSet<String>) -> Result<usize, String> {
    let lines = non_test_lines(blanked(text))?;
    let imp = imports(&lines, api);
    let mut n = 0;
    for line in &lines {
        n += SYSCALLS.iter().map(|m| occurrences(line, m)).sum::<usize>();
        for w in &api.wrappers {
            for at in call_sites(line, if w.ctor { "new" } else { &w.name }) {
                let mut path = path_before(line, at);
                if w.ctor && path.pop() != Some(w.name.as_str()) {
                    continue;
                }
                let counts = match path.as_slice() {
                    [] => {
                        !locals.contains(&w.name)
                            && (imp.names.contains(&w.name)
                                || imp.names.contains(&format!("{}::{}", w.module, w.name))
                                || (imp.root_glob && w.top)
                                || imp.globs.contains(&w.module))
                    }
                    ["libkern"] => w.top,
                    ["libkern", m] => *m == w.module,
                    [m] => *m == w.module && imp.modules.contains(*m),
                    _ => false,
                };
                n += usize::from(counts);
            }
        }
    }
    Ok(n)
}

/// The raw syscalls per entry, from `files`, each a tracked Rust path and its text, and `crates`,
/// the counted crates. An entry holding no file has no key; a crate with no entry has one only if
/// it makes a raw syscall.
fn measure(
    files: &[(String, String)],
    crates: &[String],
    api: &Api,
    entries: &BTreeSet<String>,
) -> Result<BTreeMap<String, usize>, String> {
    let mut out = BTreeMap::new();
    for krate in crates {
        let own: Vec<&(String, String)> =
            files.iter().filter(|(p, _)| in_group(p, krate) && p.ends_with(".rs") && !is_test_file(p)).collect();
        let mut locals = BTreeSet::new();
        for (path, text) in &own {
            let lines = non_test_lines(blanked(text)).map_err(|e| format!("{path}: {e}"))?;
            locals.extend(lines.iter().filter_map(|l| defined_fn(l).or_else(|| defined_type(l))));
        }
        let mut here: BTreeMap<String, usize> = BTreeMap::new();
        for (path, text) in &own {
            let n = count_file(text, api, &locals).map_err(|e| format!("{path}: {e}"))?;
            let owner = entries.iter().filter(|e| in_group(path, e)).max_by_key(|e| e.len()).unwrap_or(krate);
            *here.entry(owner.clone()).or_default() += n;
        }
        for (g, n) in here {
            if n > 0 || entries.contains(&g) {
                out.insert(g, n);
            }
        }
        if entries.contains(krate) {
            out.entry(krate.clone()).or_insert(0);
        }
    }
    for entry in entries {
        let held = files.iter().any(|(p, _)| in_group(p, entry) && p.ends_with(".rs") && !is_test_file(p));
        if !held {
            out.remove(entry);
        }
    }
    Ok(out)
}

fn failures(actual: &BTreeMap<String, usize>, baseline: &Baseline) -> Vec<String> {
    let mut out = Vec::new();
    for (group, &n) in actual.iter().filter(|(_, n)| **n > 0) {
        match baseline.counts.get(group) {
            None => out.push(format!(
                "{group}: {n} raw syscall(s) and no entry: a new raw syscall; reach the kernel through libos"
            )),
            Some(&e) if n > e => out.push(format!(
                "{group}: {n} raw syscall(s), over its entry of {e}: it may not rise; reach the kernel through libos"
            )),
            Some(&e) if n < e => out.push(format!(
                "{group}: {n} raw syscall(s), under its entry of {e}: run `cargo xtask check-syscalls --update`"
            )),
            _ => {}
        }
    }
    for group in baseline.counts.keys() {
        match actual.get(group) {
            None => out.push(format!(
                "{group}: listed, but no counted crate or module is there (libkern and libos are not counted): \
                 run `cargo xtask check-syscalls --update`"
            )),
            Some(0) => out.push(format!("{group}: counts zero: run `cargo xtask check-syscalls --update`")),
            Some(_) => {}
        }
    }
    for group in baseline.agreed.keys().filter(|g| !baseline.counts.contains_key(*g)) {
        out.push(format!("{group}: agreed, but has no entry in [counts]: add its count or drop the agreement"));
    }
    out
}

/// `baseline` with each count lowered to the one measured and each entry that counts zero or is
/// gone removed; never raised, and `[agreed]` as it was.
fn updated(actual: &BTreeMap<String, usize>, baseline: &Baseline) -> Baseline {
    let counts = baseline
        .counts
        .iter()
        .filter_map(|(g, &e)| actual.get(g).filter(|&&n| n > 0).map(|&n| (g.clone(), e.min(n))))
        .collect();
    Baseline { counts, agreed: baseline.agreed.clone() }
}

/// `check-syscalls [--update | --list]`: `--list` prints the derived wrappers and every current
/// count in the baseline's format, and changes nothing.
pub(crate) fn cmd_check_syscalls(args: &[String]) -> R<()> {
    let root = repo_root();
    let tracked = git(&["ls-files", "--", "userspace"])?;
    let read = |path: &str| fs::read_to_string(root.join(path)).map_err(|e| format!("check-syscalls: {path}: {e}"));
    let mut kern = Vec::new();
    let mut files = Vec::new();
    let mut crates = Vec::new();
    for path in tracked.lines() {
        let in_kern = path.starts_with("userspace/libkern/src/");
        if let ["userspace", krate, "Cargo.toml"] = path.split('/').collect::<Vec<_>>().as_slice()
            && !matches!(*krate, "libkern" | "libos")
        {
            crates.push(format!("userspace/{krate}"));
        }
        if path.ends_with(".rs") && (in_kern || !path.starts_with("userspace/libos/")) {
            let text = read(path)?;
            if in_kern {
                kern.push((path.to_string(), text.clone()));
            }
            files.push((path.to_string(), text));
        }
    }
    let api = derive_api(&kern)?;
    let file = root.join(BASELINE);
    let mut baseline = parse_baseline(&crate::read_text(&file)?)?;
    let entries: BTreeSet<String> = baseline.counts.keys().cloned().collect();
    let actual = measure(&files, &crates, &api, &entries)?;
    if args.iter().any(|a| a == "--list") {
        let names: Vec<String> = api.wrappers.iter().map(Wrapper::path).collect();
        println!("# wrappers: {}", names.join(", "));
        let current =
            Baseline { counts: actual.into_iter().filter(|(_, n)| *n > 0).collect(), agreed: baseline.agreed };
        print!("{}", format_baseline(&current));
        return Ok(());
    }
    if args.iter().any(|a| a == "--update") {
        let new = updated(&actual, &baseline);
        for (group, old) in &baseline.counts {
            match new.counts.get(group) {
                Some(n) if n == old => {}
                Some(n) => println!("check-syscalls: {group}: {old} -> {n}"),
                None => println!("check-syscalls: {group}: removed"),
            }
        }
        fs::write(&file, format_baseline(&new))?;
        baseline = new;
    }
    let failed = failures(&actual, &baseline);
    if !failed.is_empty() {
        for f in &failed {
            eprintln!("check-syscalls: {f}");
        }
        return Err(format!("check-syscalls: {} failure(s)", failed.len()).into());
    }
    let total: usize = baseline.counts.values().sum();
    println!("check-syscalls: {total} raw syscalls in {} entries, none over its entry ✓", baseline.counts.len());
    Ok(())
}
