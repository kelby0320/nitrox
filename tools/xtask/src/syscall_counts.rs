//! `syscall-counts`: what each command typed at `nxsh` costs in syscalls, and in which.
//!
//! Boots the release image over a kernel built with `syscall-counts`, which writes every process's
//! counts at each spawn and exit and zeroes them (`kernel/src/syscall/counts.rs`), logs in, types
//! [`COMMANDS`], and reads the dumps back. A program's own row is its exit dump's; each dump falls
//! to the command typed last before it. On demand and not in CI: a count pinned there would fail on
//! any honest change to a path.

use crate::*;
use std::collections::BTreeMap;

/// What is typed, in order, the plan's table and the files it needs.
const COMMANDS: &[&str] = &[
    "whoami",
    "whoami",
    "[1, 2] | save ./a.txt",
    "copy ./a.txt ./b.txt",
    "copy ./a.txt ./c.txt",
    "list .",
    "list .",
    "open ./a.txt | count",
    "rename ./c.txt ./d.txt",
    "remove ./d.txt",
    "mkdir ./e",
];

/// The process whose calls a program's file operations cost, by the name it logs under.
const FS_SERVER: &str = "fs-server";

/// One `SC` line: a process, and its count of each syscall number.
#[derive(Debug, PartialEq)]
struct Row {
    pid: u32,
    name: String,
    counts: BTreeMap<usize, u64>,
}

impl Row {
    fn total(&self) -> u64 {
        self.counts.values().sum()
    }
}

/// One dump: why, whose call made it, and where in the transcript it starts.
#[derive(Debug, PartialEq)]
struct Dump {
    exit: bool,
    who: u32,
    at: usize,
    rows: Vec<Row>,
}

fn parse_row(line: &str) -> Option<Row> {
    let rest = &line[line.find("SC pid=")? + 7..];
    let mut words = rest.split_whitespace();
    let pid = words.next()?.parse().ok()?;
    let name = words.next()?.strip_prefix("name=")?.to_string();
    words.next()?.strip_prefix("total=")?;
    let counts = words
        .map(|w| w.split_once(':').and_then(|(nr, n)| Some((nr.parse().ok()?, n.parse().ok()?))))
        .collect::<Option<_>>()?;
    Some(Row { pid, name, counts })
}

/// Every dump in `transcript`, in order.
fn parse(transcript: &str) -> Vec<Dump> {
    let mut out: Vec<Dump> = Vec::new();
    let mut open = false;
    let mut at = 0;
    for line in transcript.split_inclusive('\n') {
        if let Some(i) = line.find("SCDUMP ") {
            let mut w = line[i + 7..].split_whitespace();
            let exit = w.next() == Some("exit");
            let who = w.next().and_then(|p| p.strip_prefix("pid=")).and_then(|p| p.parse().ok()).unwrap_or(0);
            out.push(Dump { exit, who, at: at + i, rows: Vec::new() });
            open = true;
        } else if line.contains("SCEND") {
            open = false;
        } else if open && let (Some(row), Some(d)) = (parse_row(line), out.last_mut()) {
            d.rows.push(row);
        }
        at += line.len();
    }
    out
}

/// The syscall names by number, from `libkern`'s own list.
fn names(syscall_rs: &str) -> BTreeMap<usize, String> {
    let mut out = BTreeMap::new();
    for line in syscall_rs.lines() {
        let Some(rest) = line.trim().strip_prefix("pub const SYS_") else { continue };
        let Some((name, value)) = rest.split_once(": u64 = ") else { continue };
        if let Ok(n) = value.trim_end_matches(';').parse::<usize>() {
            out.insert(n, name.to_lowercase());
        }
    }
    out.insert(out.keys().max().map_or(0, |m| m + 1), "debug_kprint".into());
    out
}

/// The report: a row per command that ran a program, then the session's calls by syscall, then
/// each process's.
fn report(dumps: &[Dump], marks: &[(&str, usize)], names: &BTreeMap<usize, String>) -> String {
    let mut out = String::from("| A command | Its program's syscalls | The filesystem server's |\n|---|---:|---:|\n");
    for (i, (cmd, from)) in marks.iter().enumerate() {
        let to = marks.get(i + 1).map_or(usize::MAX, |m| m.1);
        let here = || dumps.iter().filter(|d| d.at >= *from && d.at < to);
        let own: u64 =
            here().filter(|d| d.exit).flat_map(|d| d.rows.iter().filter(|r| r.pid == d.who)).map(Row::total).sum();
        let fs: u64 =
            here().filter(|d| d.exit).flat_map(|d| &d.rows).filter(|r| r.name == FS_SERVER).map(Row::total).sum();
        if here().any(|d| d.exit) {
            out.push_str(&format!("| `{cmd}` | {own} | {fs} |\n"));
        }
    }
    let session = marks.first().map_or(0, |m| m.1);
    let mut by_nr: BTreeMap<usize, u64> = BTreeMap::new();
    let mut by_name: BTreeMap<String, BTreeMap<usize, u64>> = BTreeMap::new();
    for row in dumps.iter().filter(|d| d.at >= session).flat_map(|d| &d.rows) {
        for (&nr, &n) in &row.counts {
            *by_nr.entry(nr).or_default() += n;
            *by_name.entry(row.name.clone()).or_default().entry(nr).or_default() += n;
        }
    }
    let total: u64 = by_nr.values().sum();
    let name = |nr: &usize| names.get(nr).cloned().unwrap_or_else(|| format!("#{nr}"));
    out.push_str(&format!("\nThe session's {total} calls, from the first command:\n"));
    let mut ranked: Vec<_> = by_nr.iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(a.1));
    for (nr, n) in ranked {
        out.push_str(&format!("  {:<22} {n:>6}  {:>3}%\n", name(nr), n * 100 / total.max(1)));
    }
    out.push_str("\nEach process's, by name:\n");
    for (process, counts) in &by_name {
        let calls: Vec<String> = counts.iter().map(|(nr, n)| format!("{}:{n}", name(nr))).collect();
        out.push_str(&format!("  {process}: {} — {}\n", counts.values().sum::<u64>(), calls.join(" ")));
    }
    out
}

/// `cargo xtask syscall-counts`.
pub(crate) fn cmd_syscall_counts(accel: Accel) -> R<()> {
    preflight_accel(accel)?;
    cmd_image(BuildMode::SyscallCounts)?;
    let ovmf = locate_ovmf()?;
    let mut cmd = Command::new("qemu-system-x86_64");
    qemu_base_args(&mut cmd, &ovmf, accel, None)?;
    cmd.arg("-drive")
        .arg(format!("format=raw,file={}", image_path().display()))
        .args(["-display", "none", "-serial", "stdio", "-smp", "4", "-no-reboot"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    println!("xtask: syscall counts (release image, counting kernel)…\n");
    let mut s = Session::spawn(cmd, "syscall-counts")?;
    let mut marks = Vec::new();
    let run = (|| -> R<()> {
        s.expect("nitrox login:")?;
        s.send(DEMO_USER)?;
        s.expect("\npassword:")?;
        s.send(DEMO_PASSWORD)?;
        s.expect("/home>")?;
        for &c in COMMANDS {
            marks.push((c, s.transcript().len()));
            s.send(c)?;
            s.expect("/home>")?;
        }
        marks.push(("exit", s.transcript().len()));
        s.send("exit")?;
        s.expect("nitrox login:")
    })();
    let transcript = s.finish();
    if let Err(e) = run {
        println!("--- serial transcript ---\n{transcript}\n--- end ---");
        return Err(e);
    }
    let names = names(&fs::read_to_string(repo_root().join("userspace/libkern/src/syscall.rs"))?);
    print!("{}", report(&parse(&transcript), &marks, &names));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "boot\nSCDUMP spawn pid=4\nSC pid=4 name=nxsh total=3 10:2 15:1\nSCEND\n\
                          noise SCDUMP exit pid=9\nSC pid=9 name=- total=5 0:1 10:4\n\
                          SC pid=3 name=fs-server total=7 28:7\nSC pid=4 name=nxsh total=1 14:1\nSCEND\n";

    #[test]
    fn dumps_parse_with_their_rows_and_where_they_start() {
        let d = parse(SAMPLE);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].exit, d[0].who, d[0].at), (false, 4, 5));
        assert_eq!((d[1].exit, d[1].who, d[1].rows.len()), (true, 9, 3));
        assert!(d[1].at > d[0].at && SAMPLE[d[1].at..].starts_with("SCDUMP exit"));
        assert_eq!(d[1].rows[1], Row { pid: 3, name: FS_SERVER.into(), counts: [(28, 7)].into() });
    }

    #[test]
    fn a_command_gets_its_programs_exit_row_and_the_servers() {
        let d = parse(SAMPLE);
        let names = names("pub const SYS_HANDLE_CLOSE: u64 = 0;\npub const SYS_WAIT: u64 = 10;\n");
        let r = report(&d, &[("whoami", 0), ("list .", SAMPLE.len())], &names);
        assert!(r.contains("| `whoami` | 5 | 7 |"), "{r}");
        assert!(!r.contains("`list .`"), "no program ran");
        assert!(r.contains("The session's 16 calls"), "{r}");
        assert!(r.contains("wait") && r.contains("#28"), "{r}");
    }

    #[test]
    fn the_names_come_from_libkern_and_kprint_follows_the_last() {
        let n = names(&fs::read_to_string(repo_root().join("userspace/libkern/src/syscall.rs")).unwrap());
        assert_eq!(n[&10], "wait");
        assert_eq!(n.get(&42).map(String::as_str), Some("debug_kprint"));
    }
}
