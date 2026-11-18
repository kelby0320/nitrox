//! `shell(7)`, the guide: every `nxsh` block in it is typed at one prompt, in order, as a person
//! reading it would type them, and prints the block that follows it.

use crate::eval::{Interp, Mode};
use crate::host::{MockHost, MockLog};
use crate::repl::{Continue, diagnostic, needs_continuation};
use alloc::boxed::Box;
use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use libstream::wire::{Record, Schema, StreamFlags, Table, TypeModifiers, TypeTag, Value};

const GUIDE: &str = include_str!("../../../docs/manual/7/shell.md");

/// A fenced block: its tag, its text, and whether only blank lines part it from the block before.
struct Block {
    tag: String,
    body: String,
    follows: bool,
}

/// The page's blocks. A fence this reader would not see, indented under a list item or of tildes,
/// is refused, or an example in it would go unrun.
fn blocks(page: &str) -> Vec<Block> {
    let (mut out, mut follows) = (Vec::new(), false);
    let mut lines = page.lines();
    while let Some(line) = lines.next() {
        let fence = line.trim_start();
        if (fence.starts_with("```") || fence.starts_with("~~~")) && !line.starts_with("```") {
            panic!("a fence this test cannot read: {line:?}");
        }
        if let Some(tag) = line.strip_prefix("```") {
            let mut body = String::new();
            for l in lines.by_ref().take_while(|l| *l != "```") {
                body.push_str(l);
                body.push('\n');
            }
            out.push(Block { tag: tag.to_string(), body, follows });
            follows = true;
        } else if !line.trim().is_empty() {
            follows = false;
        }
    }
    out
}

/// Two texts: an example and its output, or a path and its file.
type Pairs = Vec<(String, String)>;

/// The guide's examples, each with what it prints, and the files its `text` blocks headed
/// `# /PATH` hold. An untagged block is an example's output, and nothing else.
fn read(page: &str) -> (Pairs, Pairs) {
    let (mut examples, mut files) = (Vec::new(), Vec::new());
    let blocks = blocks(page);
    let mut i = 0;
    while i < blocks.len() {
        let b = &blocks[i];
        match b.tag.as_str() {
            "nxsh" => {
                let typed = b.body.lines().map(|l| l.strip_prefix("> ").unwrap_or_else(|| panic!("no `> `: {l}")));
                let source: String = typed.flat_map(|l| [l, "\n"]).collect();
                let output = blocks.get(i + 1).filter(|o| o.tag.is_empty() && o.follows);
                examples.push((source, output.map_or(String::new(), |o| o.body.clone())));
                i += usize::from(output.is_some());
            }
            "text" => {
                if let Some((head, body)) = b.body.split_once('\n')
                    && let Some(path) = head.strip_prefix("# /")
                {
                    files.push((format!("/{path}"), body.to_string()));
                }
            }
            tag => panic!("a block tagged {tag:?} follows no example: {}", b.body),
        }
        i += 1;
    }
    (examples, files)
}

/// A table of `columns`, a row each of `rows`.
fn table(columns: &[(&str, TypeTag)], rows: Vec<Vec<Value>>) -> Vec<u8> {
    let schema = columns.iter().fold(Schema::new(), |s, (name, ty)| s.field(name, *ty, TypeModifiers::NONE));
    let mut buf = Vec::new();
    Table { flags: StreamFlags::NONE, schema, rows }.encode(&mut buf).expect("encodes");
    buf
}

/// The machine the guide's examples ran on: `alice` at home, and a `/bin` of six programs.
fn machine(files: &[(String, String)]) -> MockHost {
    let bin = [("copy", 412_512), ("list", 498_760), ("man", 604_104), ("nxsh", 1_203_456), ("page", 523_880)]
        .into_iter()
        .chain([("whoami", 301_224)])
        .map(|(name, size)| {
            let modified = Value::Int(1_763_400_000);
            vec![Value::Str(name.into()), Value::Int(size), Value::Str("file".into()), modified]
        })
        .collect();
    let columns =
        [("name", TypeTag::String), ("size", TypeTag::Int), ("kind", TypeTag::String), ("modified", TypeTag::Int)];
    let user = table(&[("user", TypeTag::String)], vec![vec![Value::Str("alice".into())]]);
    let mut host = ["/", "/bin", "/home", "/home/alice"]
        .into_iter()
        .fold(MockHost::new(), MockHost::with_dir)
        .with_program("whoami", Some(user))
        .with_program("list", Some(table(&columns, bin)));
    for (path, body) in files {
        host = host.with_file(path, body);
    }
    host
}

/// Every example's source and output, each as the guide shows it and as the shell printed it.
fn run_guide() -> Vec<(String, String, String)> {
    let (examples, files) = read(GUIDE);
    let host = machine(&files);
    let log = host.log();
    let mut shell = Interp::with_host(Box::new(host), Mode::Repl);
    let home = Value::Str("/home/alice".into());
    let env = Schema::new().field("PWD", TypeTag::String, TypeModifiers::NONE).field(
        "HOME",
        TypeTag::String,
        TypeModifiers::NONE,
    );
    shell.set_env(Record { schema: env, values: vec![home.clone(), home] });
    examples
        .into_iter()
        .map(|(source, shown)| {
            let printed = typed(&mut shell, &log, &source);
            (source, shown, printed)
        })
        .collect()
}

/// What `source` prints at the prompt of `shell`, whose host logs to `log`: what a line writes, to
/// `stdout` and to `stderr`, then its value or its error, as the REPL draws them.
fn typed(shell: &mut Interp, log: &Rc<RefCell<MockLog>>, source: &str) -> String {
    let (mut printed, mut statement) = (String::new(), String::new());
    for line in source.lines() {
        statement.push_str(line);
        statement.push('\n');
        if needs_continuation(&statement) != Continue::No {
            continue;
        }
        let before = (log.borrow().output.len(), log.borrow().diagnostics.len());
        let result = shell.run_line(statement.trim_end());
        printed.push_str(&log.borrow().output[before.0..].concat());
        printed.push_str(&log.borrow().diagnostics[before.1..].concat());
        match result {
            Ok(text) => printed.push_str(text.as_deref().unwrap_or("")),
            Err(e) => printed.push_str(&format!("{}\n", diagnostic(&e.message, false))),
        }
        statement.clear();
    }
    assert!(statement.is_empty(), "an example ends mid-statement: {source}");
    printed
}

/// A line's `stderr` is shown, as `time`'s, before its value.
#[test]
fn what_a_line_writes_to_stderr_is_shown() {
    let host = MockHost::new();
    let log = host.log();
    let mut shell = Interp::with_host(Box::new(host), Mode::Repl);
    assert_eq!(typed(&mut shell, &log, "time 1"), "time: unknown\n1\n");
}

#[test]
fn every_example_in_the_guide_prints_what_follows_it() {
    let wrong: Vec<String> = run_guide()
        .into_iter()
        .filter(|(_, shown, printed)| shown != printed)
        .map(|(source, shown, printed)| format!("{source}  shows {shown:?}\n  printed {printed:?}"))
        .collect();
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The reader cannot miss an example: an output with none before it, or a block of another tag,
/// fails `read`; and a guide read as having few examples has been misread.
#[test]
fn the_guide_is_read_whole() {
    let (examples, files) = read(GUIDE);
    assert!(examples.len() >= 20, "{} examples", examples.len());
    assert_eq!(files.iter().map(|f| f.0.as_str()).collect::<Vec<_>>(), ["/home/alice/lib.nx"]);
    for unseen in ["- an item\n\n  ```nxsh\n  > 1\n  ```\n", "~~~nxsh\n> 1\n~~~\n"] {
        assert!(std::panic::catch_unwind(|| read(unseen)).is_err(), "{unseen:?} would go unrun");
    }
    let stray = "```nxsh\n> 1\n```\n\nprose\n\n```\n1\n```\n";
    assert!(std::panic::catch_unwind(|| read("```nxsh\n1\n```\n")).is_err(), "a line typed with no `> `");
    assert!(std::panic::catch_unwind(|| read(stray)).is_err(), "an output after prose was taken as one");
}
