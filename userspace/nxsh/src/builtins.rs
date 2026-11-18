//! The builtins (§3): what acts on the shell itself, as no program can, and `help`.

use crate::ast::{Call, CallKind};
use alloc::format;
use alloc::string::String;

/// One builtin, and what `help` says of it.
#[derive(Copy, Clone, Debug)]
pub struct Builtin {
    /// What it is called.
    pub name: &'static str,
    /// It changes nothing, so it may head a pipeline: a stage failing after `cd` comes too late.
    pub heads: bool,
    /// How it is written.
    pub signature: &'static str,
    /// What it does, in a line.
    pub does: &'static str,
    /// Its use at a prompt, as `operators::Operator::example`; empty where none can run.
    pub example: &'static str,
    /// What the example prints.
    pub output: &'static str,
}

/// Every builtin.
pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "cd",
        heads: false,
        signature: "cd [PATH]",
        does: "Go to PATH, or to HOME; the directory changes only if PATH is real.",
        example: "cd /\n$env.PWD",
        output: "/\n",
    },
    Builtin {
        name: "exit",
        heads: false,
        signature: "exit [STATUS]",
        does: "Leave the shell; a script ends with STATUS, or 0.",
        example: "",
        output: "",
    },
    Builtin {
        name: "help",
        heads: true,
        signature: "help [NAME]",
        does: "What an operator, a builtin or a keyword does, or a list of all.",
        example: "",
        output: "",
    },
];

/// The builtin called `name`, if one is.
pub fn find(name: &str) -> Option<&'static Builtin> {
    BUILTINS.iter().find(|b| b.name == name)
}

/// Whether `name` is a builtin's.
pub fn is_builtin(name: &str) -> bool {
    find(name).is_some()
}

/// Whether `c` is a builtin that may head a pipeline, its value going on.
pub fn heads_a_pipeline(c: &Call) -> bool {
    c.kind == CallKind::Builtin && find(&c.name).is_some_and(|b| b.heads)
}

/// Why the builtin `name` cannot be where a pipeline put it.
pub fn not_a_stage(name: &str) -> String {
    match find(name) {
        Some(b) if b.heads => format!("`{name}` is a builtin: nothing is piped into it"),
        _ => format!("`{name}` is a builtin, which acts on the shell: it takes no pipe, and heads none"),
    }
}
