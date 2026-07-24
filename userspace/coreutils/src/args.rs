//! Command-line argument parsing, shared by every coreutil.
//!
//! Follows the GNU conventions the shell design adopts as its baseline (§10f): long
//! `--flag`, short `-f`, clustered shorts (`-rf`), `--` to end option parsing, and
//! `--help`/`--version` on every program.
//!
//! **One deliberate omission:** GNU's bare `-` meaning "read from stdin" has no
//! equivalent here. Piping is structural in this system — a stage's input *is* its
//! `stdin` stream, not a flag-selected mode — so `-` is just an operand like any other,
//! and a program that sees one treats it as a path.
//!
//! Parsing is **declarative**: a program lists the flags it accepts, and anything else is
//! an error rather than a silently ignored argument (the fail-loud default, §1).

use alloc::string::String;
use alloc::vec::Vec;

/// One flag a program accepts.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Flag {
    /// The long name, without `--` (e.g. `"recursive"`).
    pub long: &'static str,
    /// The short name, without `-`, or `'\0'` for a long-only flag.
    pub short: char,
    /// One-line help text.
    pub help: &'static str,
}

impl Flag {
    /// A flag with both a long and a short spelling.
    pub const fn new(long: &'static str, short: char, help: &'static str) -> Flag {
        Flag { long, short, help }
    }
    /// A long-only flag.
    pub const fn long_only(long: &'static str, help: &'static str) -> Flag {
        Flag { long, short: '\0', help }
    }
}

/// Why parsing failed. Each carries enough to render a specific message — "unknown flag
/// `--recusive`" beats "bad arguments".
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgError {
    /// A `--flag` the program does not accept.
    UnknownLong(String),
    /// A `-f` the program does not accept.
    UnknownShort(char),
    /// An empty `--` -prefixed name (a bare `--=value` or similar).
    Malformed(String),
}

/// A parsed command line: the flags that were set, and the operands in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Args {
    set: Vec<&'static str>,
    /// Positional operands, in command-line order (paths, names, …).
    pub operands: Vec<String>,
}

impl Args {
    /// Whether `--long` (or its short form) was given.
    pub fn has(&self, long: &str) -> bool {
        self.set.iter().any(|f| *f == long)
    }

    /// Whether `--help` was given.
    pub fn help(&self) -> bool {
        self.has("help")
    }

    /// Whether `--version` was given.
    pub fn version(&self) -> bool {
        self.has("version")
    }
}

/// `--help` and `--version`, accepted by every program (§10f).
pub const UNIVERSAL_FLAGS: [Flag; 2] = [
    Flag::long_only("help", "show this help and exit"),
    Flag::long_only("version", "show version information and exit"),
];

/// Parse `argv[1..]` against `flags`, which need not include [`UNIVERSAL_FLAGS`] — those
/// are always accepted.
///
/// `argv[0]` is the program name by convention (the setup message's `argv`), and is
/// skipped. Everything after a bare `--` is an operand, even if it looks like a flag.
pub fn parse(argv: &[String], flags: &[Flag]) -> Result<Args, ArgError> {
    let mut out = Args::default();
    let mut operands_only = false;

    for arg in argv.iter().skip(1) {
        let bytes = arg.as_bytes();
        if operands_only {
            out.operands.push(arg.clone());
            continue;
        }
        if arg == "--" {
            operands_only = true;
            continue;
        }
        if bytes.len() > 2 && bytes[0] == b'-' && bytes[1] == b'-' {
            let name = &arg[2..];
            if name.is_empty() {
                return Err(ArgError::Malformed(arg.clone()));
            }
            match lookup_long(name, flags) {
                Some(f) => set(&mut out, f.long),
                None => return Err(ArgError::UnknownLong(String::from(name))),
            }
            continue;
        }
        // A lone `-` is an operand, not a flag: see the module note on GNU's `cat -`.
        if bytes.len() > 1 && bytes[0] == b'-' {
            // Clustered shorts: `-rf` is `-r -f`.
            for c in arg[1..].chars() {
                match lookup_short(c, flags) {
                    Some(f) => set(&mut out, f.long),
                    None => return Err(ArgError::UnknownShort(c)),
                }
            }
            continue;
        }
        out.operands.push(arg.clone());
    }
    Ok(out)
}

fn set(out: &mut Args, long: &'static str) {
    if !out.set.contains(&long) {
        out.set.push(long);
    }
}

fn lookup_long(name: &str, flags: &[Flag]) -> Option<Flag> {
    flags
        .iter()
        .chain(UNIVERSAL_FLAGS.iter())
        .find(|f| f.long == name)
        .copied()
}

fn lookup_short(c: char, flags: &[Flag]) -> Option<Flag> {
    flags
        .iter()
        .chain(UNIVERSAL_FLAGS.iter())
        .find(|f| f.short == c && c != '\0')
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const RECURSIVE: Flag = Flag::new("recursive", 'r', "recurse into subdirectories");
    const FORCE: Flag = Flag::new("force", 'f', "overwrite without asking");
    const LONG: Flag = Flag::new("long", 'l', "long listing");

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| String::from(*s)).collect()
    }

    #[test]
    fn parses_long_short_and_operands() {
        let a = parse(
            &argv(&["copy", "--recursive", "-f", "/src", "/dst"]),
            &[RECURSIVE, FORCE],
        )
        .unwrap();
        assert!(a.has("recursive") && a.has("force"));
        assert_eq!(a.operands, vec![String::from("/src"), String::from("/dst")]);
    }

    #[test]
    fn clustered_shorts_expand() {
        let a = parse(&argv(&["remove", "-rf", "/tmp/x"]), &[RECURSIVE, FORCE]).unwrap();
        assert!(a.has("recursive") && a.has("force"));
        assert_eq!(a.operands.len(), 1);
    }

    #[test]
    fn double_dash_ends_option_parsing() {
        // The whole point: a file legitimately named `--long` must be reachable.
        let a = parse(&argv(&["list", "--", "--long", "-r"]), &[LONG, RECURSIVE]).unwrap();
        assert!(!a.has("long") && !a.has("recursive"));
        assert_eq!(a.operands, vec![String::from("--long"), String::from("-r")]);
    }

    #[test]
    fn a_lone_dash_is_an_operand() {
        // Deliberate deviation from GNU: `-` is not "read stdin", it is a path.
        let a = parse(&argv(&["list", "-"]), &[LONG]).unwrap();
        assert_eq!(a.operands, vec![String::from("-")]);
    }

    #[test]
    fn unknown_flags_fail_loud() {
        // A typo must not be silently ignored — the fail-loud default (design §1).
        assert_eq!(
            parse(&argv(&["list", "--recusive"]), &[RECURSIVE]),
            Err(ArgError::UnknownLong(String::from("recusive")))
        );
        assert_eq!(
            parse(&argv(&["list", "-z"]), &[RECURSIVE]),
            Err(ArgError::UnknownShort('z'))
        );
        // A cluster fails on the offending letter even if the others are valid.
        assert_eq!(
            parse(&argv(&["remove", "-rz"]), &[RECURSIVE]),
            Err(ArgError::UnknownShort('z'))
        );
    }

    #[test]
    fn universal_flags_need_no_declaration() {
        let a = parse(&argv(&["list", "--help"]), &[]).unwrap();
        assert!(a.help() && !a.version());
        let a = parse(&argv(&["list", "--version"]), &[]).unwrap();
        assert!(a.version());
    }

    #[test]
    fn repeating_a_flag_is_idempotent() {
        let a = parse(&argv(&["remove", "-r", "--recursive", "-r"]), &[RECURSIVE]).unwrap();
        assert!(a.has("recursive"));
        assert_eq!(a.set.len(), 1);
    }

    #[test]
    fn argv0_is_never_an_operand() {
        // argv[0] is the program name (setup-message convention), not an argument.
        let a = parse(&argv(&["list"]), &[]).unwrap();
        assert!(a.operands.is_empty());
    }
}
