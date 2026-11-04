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
    "[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12] | save ./big.txt",
    "copy --force ./a.txt ./big.txt",
];

/// The process whose calls a program's file operations cost, by the name it logs under.
const FS_SERVER: &str = "fs-server";

/// One `SC` line: a process, and its count of each syscall, by number or as `kprint` or `other`.
#[derive(Debug, PartialEq)]
struct Row {
    pid: u32,
    name: String,
    counts: BTreeMap<String, u64>,
}

impl Row {
    fn total(&self) -> u64 {
        self.counts.values().sum()
    }
}

/// One dump: why, whose call made it, its `seq`, and where in the transcript it starts.
#[derive(Debug, PartialEq)]
struct Dump {
    exit: bool,
    who: u32,
    seq: u64,
    at: usize,
    rows: Vec<Row>,
}

/// The word after `key=` in `words`, the next one.
fn field<'a>(words: &mut impl Iterator<Item = &'a str>, key: &str) -> Option<&'a str> {
    words.next()?.strip_prefix(key)?.strip_prefix('=')
}

/// A row and its dump's `seq`.
fn parse_row(line: &str) -> Option<(u64, Row)> {
    let mut words = line[line.find("SC seq=")? + 3..].split_whitespace();
    let seq = field(&mut words, "seq")?.parse().ok()?;
    let pid = field(&mut words, "pid")?.parse().ok()?;
    let name = field(&mut words, "name")?.to_string();
    let total: u64 = field(&mut words, "total")?.parse().ok()?;
    let counts: BTreeMap<String, u64> = words
        .map(|w| w.split_once(':').and_then(|(k, n)| Some((k.to_string(), n.parse().ok()?))))
        .collect::<Option<_>>()?;
    (counts.values().sum::<u64>() == total).then_some((seq, Row { pid, name, counts }))
}

/// Every dump in `transcript`, in order, each with the rows that carry its `seq`. A row that does
/// not parse, or names no dump still open, is refused: a lost row would be a count quietly short.
fn parse(transcript: &str) -> Result<Vec<Dump>, String> {
    let mut out: Vec<Dump> = Vec::new();
    let mut open = std::collections::BTreeSet::new();
    let mut at = 0;
    for line in transcript.split_inclusive('\n') {
        if let Some(i) = line.find("SCDUMP ") {
            let mut w = line[i + 7..].split_whitespace();
            let exit = w.next() == Some("exit");
            let (who, seq) = (field(&mut w, "pid").and_then(|p| p.parse().ok()), field(&mut w, "seq"));
            let (Some(who), Some(Ok(seq))) = (who, seq.map(str::parse)) else {
                return Err(format!("a dump's head does not parse: {line:?}"));
            };
            out.push(Dump { exit, who, seq, at: at + i, rows: Vec::new() });
            open.insert(seq);
        } else if let Some(i) = line.find("SCEND seq=") {
            open.remove(&line[i + 10..].trim().parse::<u64>().map_err(|_| format!("an end does not parse: {line:?}"))?);
        } else if line.contains("SC seq=") {
            let (seq, row) = parse_row(line).ok_or_else(|| format!("a row does not parse: {line:?}"))?;
            match out.iter_mut().rev().find(|d| d.seq == seq) {
                Some(d) if open.contains(&seq) => d.rows.push(row),
                _ => return Err(format!("a row of no open dump: {line:?}")),
            }
        }
        at += line.len();
    }
    Ok(out)
}

/// The syscall names by number, from `libkern`'s own list, and the counting kernel's two names.
fn names(syscall_rs: &str) -> BTreeMap<String, String> {
    let mut out =
        BTreeMap::from([("kprint".to_string(), "debug_kprint".to_string()), ("other".into(), "other".into())]);
    for line in syscall_rs.lines() {
        let Some(rest) = line.trim().strip_prefix("pub const SYS_") else { continue };
        let Some((name, value)) = rest.split_once(": u64 = ") else { continue };
        if let Ok(n) = value.trim_end_matches(';').parse::<usize>() {
            out.insert(n.to_string(), name.to_lowercase());
        }
    }
    out
}

/// The report: a row per command that ran a program, then the session's calls by syscall, then
/// each process's.
fn report(dumps: &[Dump], marks: &[(&str, usize)], names: &BTreeMap<String, String>) -> String {
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
    let mut by_nr: BTreeMap<&str, u64> = BTreeMap::new();
    let mut by_name: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
    for row in dumps.iter().filter(|d| d.at >= session).flat_map(|d| &d.rows) {
        for (nr, &n) in &row.counts {
            *by_nr.entry(nr).or_default() += n;
            *by_name.entry(&row.name).or_default().entry(nr).or_default() += n;
        }
    }
    let total: u64 = by_nr.values().sum();
    let name = |nr: &&str| names.get(*nr).cloned().unwrap_or_else(|| format!("#{nr}"));
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
    print!("{}", report(&parse(&transcript)?, &marks, &names));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "boot\nSCDUMP spawn pid=4 seq=0\nSC seq=0 pid=4 name=nxsh total=3 10:2 15:1\nSCEND seq=0\n\
                          noise SCDUMP exit pid=9 seq=1\nSC seq=1 pid=9 name=- total=5 0:1 10:4\n\
                          SC seq=1 pid=3 name=fs-server total=7 28:7\nSC seq=1 pid=4 name=nxsh total=1 kprint:1\n\
                          SCEND seq=1\n";

    #[test]
    fn dumps_parse_with_their_rows_and_where_they_start() {
        let d = parse(SAMPLE).unwrap();
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].exit, d[0].who, d[0].at), (false, 4, 5));
        assert_eq!((d[1].exit, d[1].who, d[1].seq, d[1].rows.len()), (true, 9, 1, 3));
        assert!(SAMPLE[d[1].at..].starts_with("SCDUMP exit"));
        assert_eq!(d[1].rows[1], Row { pid: 3, name: FS_SERVER.into(), counts: [("28".into(), 7)].into() });
    }

    #[test]
    fn a_command_gets_its_programs_exit_row_and_the_servers() {
        let d = parse(SAMPLE).unwrap();
        let names = names("pub const SYS_HANDLE_CLOSE: u64 = 0;\npub const SYS_WAIT: u64 = 10;\n");
        let r = report(&d, &[("whoami", 0), ("list .", SAMPLE.len())], &names);
        assert!(r.contains("| `whoami` | 5 | 7 |"), "{r}");
        assert!(!r.contains("`list .`"), "no program ran");
        assert!(r.contains("The session's 16 calls"), "{r}");
        assert!(r.contains("wait") && r.contains("#28") && r.contains("debug_kprint"), "{r}");
    }

    /// **Two dumps at once, and another CPU's line between rows**: each row goes to the dump its
    /// `seq` names, and a row cut short is refused rather than dropped.
    #[test]
    fn rows_find_their_dump_by_seq_and_a_broken_one_is_refused() {
        let both = "SCDUMP spawn pid=4 seq=5\nSCDUMP exit pid=9 seq=6\nuser: a line\n\
                    SC seq=6 pid=9 name=- total=1 0:1\nSC seq=5 pid=4 name=nxsh total=2 10:2\nSCEND seq=5\nSCEND seq=6\n";
        let d = parse(both).unwrap();
        assert_eq!((d[0].rows[0].pid, d[1].rows[0].pid), (4, 9));
        for broken in [
            "SCDUMP exit pid=9 seq=1\nSC seq=1 pid=9 name=whoami total=5 0:1\nSCEND seq=1\n",
            "SCDUMP exit pid=9 seq=1\nSC seq=1 pid=9 name=- total=1 0:1 user: x\nSCEND seq=1\n",
            "SCDUMP exit pid=9 seq=1\nSCEND seq=1\nSC seq=1 pid=9 name=- total=1 0:1\n",
        ] {
            assert!(parse(broken).is_err(), "{broken:?}");
        }
    }

    #[test]
    fn the_kernels_own_row_parses() {
        let src = fs::read_to_string(repo_root().join("kernel/src/syscall/counts.rs")).unwrap();
        let example = src.lines().find_map(|l| l.split_once("EXAMPLE_ROW: &str = \"")).unwrap().1;
        let (seq, row) = parse_row(example.trim_end_matches("\";")).unwrap();
        assert_eq!((seq, row.pid, row.name.as_str(), row.total()), (3, 12, FS_SERVER, 6));
        assert_eq!((row.counts["kprint"], row.counts["other"]), (2, 1));
    }

    #[test]
    fn the_names_come_from_libkern_and_the_kernels_two_are_its_own() {
        let n = names(&fs::read_to_string(repo_root().join("userspace/libkern/src/syscall.rs")).unwrap());
        assert_eq!(n["10"], "wait");
        assert_eq!((n["kprint"].as_str(), n["other"].as_str()), ("debug_kprint", "other"));
        assert!(!n.contains_key("42"), "no number past the last is named");
    }
}
