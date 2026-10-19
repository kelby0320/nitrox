//! What runs when a file is named as a program.
//!
//! The kernel loads only ELF and reads no `#!`, so the shell decides: a function of the file's
//! first bytes, with the host only reading them.

use alloc::string::String;
use alloc::vec::Vec;

/// How many bytes of a file the host reads to decide what runs.
///
/// A `#!` line longer than this is refused rather than cut.
pub const HEAD_LEN: usize = 256;

/// What a file asks to be run by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runs {
    /// An ELF image: spawned as it is.
    Program,
    /// A `#!` line: the interpreter it names and the one optional argument after it.
    Interpreter { program: String, arg: Option<String> },
    /// Anything else: a script for a new `nxsh`.
    Script,
}

/// What to spawn instead of a file: the program to resolve and the argv to send it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// A name or a path for the host to resolve, as it resolves any stage's program.
    pub program: String,
    /// The argv the spawned process receives.
    pub argv: Vec<String>,
}

/// Whether `word` names a program by its path: it starts with `/`, `./`, `../` or `~/`.
pub fn is_path_head(word: &str) -> bool {
    ["/", "./", "../", "~/"].iter().any(|p| word.starts_with(p))
}

/// What the file whose first bytes are `head` runs as.
///
/// `head` is at most [`HEAD_LEN`] bytes; one that fills it with no newline in sight is a `#!` line
/// that may be cut. The program is the first word after `#!`, and the rest of the line, trimmed,
/// is one argument, as Linux passes it.
pub fn what_runs(head: &[u8]) -> Result<Runs, String> {
    if head.starts_with(b"\x7fELF") {
        return Ok(Runs::Program);
    }
    let Some(line) = head.strip_prefix(b"#!") else { return Ok(Runs::Script) };
    let end = match line.iter().position(|&b| b == b'\n') {
        Some(i) => i,
        None if head.len() >= HEAD_LEN => {
            return Err(alloc::format!("the `#!` line is longer than {HEAD_LEN} bytes"));
        }
        None => line.len(),
    };
    let line = core::str::from_utf8(&line[..end]).map_err(|_| String::from("the `#!` line is not text"))?.trim();
    let (program, rest) = match line.split_once(char::is_whitespace) {
        Some((p, r)) => (p, r.trim()),
        None => (line, ""),
    };
    if program.is_empty() {
        return Err(String::from("the `#!` line names no interpreter"));
    }
    Ok(Runs::Interpreter { program: String::from(program), arg: (!rest.is_empty()).then(|| String::from(rest)) })
}

/// What to spawn for the file at `path` that `runs` says, given the stage's `argv`.
///
/// `None` for a program, which is spawned as it is. An interpreter gets its argument, then the
/// file's path, then the stage's arguments; a script runs under `nxsh` the same way.
pub fn launch(runs: &Runs, path: &str, argv: &[String]) -> Option<Launch> {
    let (program, lead) = match runs {
        Runs::Program => return None,
        Runs::Interpreter { program, arg } => (program.clone(), arg.clone()),
        Runs::Script => (String::from("nxsh"), None),
    };
    let mut out = alloc::vec![program.clone()];
    out.extend(lead);
    out.push(String::from(path));
    out.extend(argv.iter().skip(1).cloned());
    Some(Launch { program, argv: out })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn s(x: &str) -> String {
        String::from(x)
    }

    fn interp(p: &str, a: Option<&str>) -> Runs {
        Runs::Interpreter { program: s(p), arg: a.map(s) }
    }

    #[test]
    fn elf_is_a_program() {
        assert_eq!(what_runs(b"\x7fELF\x02\x01"), Ok(Runs::Program));
    }

    #[test]
    fn a_shebang_names_a_program_and_one_argument() {
        assert_eq!(what_runs(b"#!/bin/nxsh\necho hi\n"), Ok(interp("/bin/nxsh", None)));
        assert_eq!(what_runs(b"#! /bin/x -v\n"), Ok(interp("/bin/x", Some("-v"))));
        assert_eq!(what_runs(b"#!/bin/x   -v\n"), Ok(interp("/bin/x", Some("-v"))));
        assert_eq!(what_runs(b"#!/bin/x -a -b  \r\n"), Ok(interp("/bin/x", Some("-a -b"))));
        assert_eq!(what_runs(b"#!nxsh\n"), Ok(interp("nxsh", None)));
        assert_eq!(what_runs(b"#!/bin/nxsh"), Ok(interp("/bin/nxsh", None)));
    }

    #[test]
    fn a_shebang_with_no_interpreter_is_refused() {
        assert!(what_runs(b"#!\nx").is_err());
        assert!(what_runs(b"#!   \n").is_err());
        assert!(what_runs(b"#!").is_err());
    }

    #[test]
    fn a_shebang_line_that_fills_the_bytes_read_is_refused() {
        let mut long = vec![b'#', b'!'];
        long.resize(HEAD_LEN, b'a');
        assert!(what_runs(&long).unwrap_err().contains("longer"));
        long.pop();
        assert!(what_runs(&long).is_ok());
        long.resize(HEAD_LEN, b'a');
        long[HEAD_LEN - 1] = b'\n';
        assert!(what_runs(&long).is_ok());
    }

    #[test]
    fn a_shebang_that_is_not_text_is_refused() {
        assert!(what_runs(b"#!/bin/\xffx\n").is_err());
    }

    #[test]
    fn anything_else_is_a_script() {
        assert_eq!(what_runs(b""), Ok(Runs::Script));
        assert_eq!(what_runs(b"echo hi\n"), Ok(Runs::Script));
        assert_eq!(what_runs(b"# a comment\n"), Ok(Runs::Script));
        assert_eq!(what_runs(b"#"), Ok(Runs::Script));
        assert_eq!(what_runs(b"\x7fEL"), Ok(Runs::Script));
    }

    #[test]
    fn the_argv_for_each() {
        let argv = vec![s("./t"), s("x"), s("y")];
        assert_eq!(launch(&Runs::Program, "/p/t", &argv), None);
        assert_eq!(
            launch(&interp("/bin/x", Some("-v")), "/p/t", &argv),
            Some(Launch { program: s("/bin/x"), argv: vec![s("/bin/x"), s("-v"), s("/p/t"), s("x"), s("y")] })
        );
        assert_eq!(
            launch(&interp("nxsh", None), "/p/t", &argv),
            Some(Launch { program: s("nxsh"), argv: vec![s("nxsh"), s("/p/t"), s("x"), s("y")] })
        );
        assert_eq!(
            launch(&Runs::Script, "/p/t", &argv),
            Some(Launch { program: s("nxsh"), argv: vec![s("nxsh"), s("/p/t"), s("x"), s("y")] })
        );
        assert_eq!(launch(&Runs::Script, "/p/t", &[s("./t")]).unwrap().argv, vec![s("nxsh"), s("/p/t")]);
    }

    #[test]
    fn a_path_head_starts_with_a_slash_or_a_dot_path_or_a_home() {
        for w in ["/bin/x", "./x", "../x", "~/x"] {
            assert!(is_path_head(w), "{w}");
        }
        for w in ["x", "~", ".", "..", "~x", ".x", "a/b"] {
            assert!(!is_path_head(w), "{w}");
        }
    }
}
