//! Paths the shell resolves itself: where a `use` reads from and the chain of files being loaded.
//!
//! Pure: the host only reads what these name. The kernel refuses a relative path in a namespace
//! lookup, so every file the shell opens goes through [`resolve`] first.

use crate::ast::Script;
use crate::eval::{EvalError, Interp};
use crate::parse::{ParseError, Parser};
use alloc::string::String;
use alloc::vec::Vec;
use libstream::wire::Record;

/// The working directory in `env`: its conventional `PWD` entry, if a string.
pub fn pwd_of(env: &Record) -> Option<&str> {
    let i = env.schema.fields.iter().position(|f| f.name == "PWD")?;
    env.values.get(i)?.as_str()
}

/// `path` made absolute against `cwd` by `librsproto::path::resolve`, the shell's one resolver.
///
/// The error names the path as typed: `` `{path}`: {message} ``.
pub fn resolve(cwd: Option<&str>, path: &str) -> core::result::Result<String, String> {
    let mut buf = [0u8; 1024];
    let out = librsproto::path::resolve(cwd.map(str::as_bytes), path.as_bytes(), &mut buf)
        .map_err(|e| alloc::format!("`{path}`: {}", e.message()))?;
    Ok(String::from_utf8_lossy(out).into_owned())
}

/// The directory of an absolute file path: `/` for a file in the root.
pub fn dir_of(abs: &str) -> &str {
    match abs.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &abs[..i],
    }
}

/// Parse `src`, stamping `base` on every `use` in it as the directory its path resolves against.
///
/// `None` is a source with no file behind it (the prompt, `-c`), where a `use` resolves against
/// `PWD` when it runs.
pub fn parse_script_in(src: &str, base: Option<&str>) -> core::result::Result<Script, ParseError> {
    let mut p = Parser::new(src);
    p.base = base.map(String::from);
    p.script()
}

/// The refusal for a `use` of `abs` while it is already in `chain`, naming both.
pub fn cycle_message(typed: &str, chain: &[String], abs: &str) -> String {
    let mut m = alloc::format!("`{typed}` is already being loaded: ");
    for p in chain {
        m.push_str(p);
        m.push_str(" → ");
    }
    m.push_str(abs);
    m
}

impl Interp {
    /// Record `abs` as a file being loaded, so a `use` of it from inside is refused.
    pub fn mark_loading(&mut self, abs: &str) {
        self.loading.push(String::from(abs));
    }

    /// `use "./lib/utils.nx" { helper }` / `… as utils`, from a file whose directory is `base`.
    ///
    /// `path` resolves against `base`, or against `PWD` where there is no file (the prompt, `-c`).
    /// The module is in the loading chain while it runs and leaves it on every exit, an error
    /// included, so a `use` that failed can be tried again.
    pub(crate) fn exec_use(
        &mut self,
        path: &str,
        names: &Option<Vec<String>>,
        alias: &Option<String>,
        base: Option<&str>,
    ) -> core::result::Result<(), EvalError> {
        let abs = resolve(base.or(self.cwd()), path).map_err(EvalError::new)?;
        if self.loading.contains(&abs) {
            return Err(EvalError::new(cycle_message(path, &self.loading, &abs)));
        }
        self.loading.push(abs.clone());
        let r = self.load_module(path, &abs, names, alias);
        self.loading.pop();
        r
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_of_keeps_the_root() {
        assert_eq!(dir_of("/home/a/main.nx"), "/home/a");
        assert_eq!(dir_of("/main.nx"), "/");
        assert_eq!(dir_of("/a/b"), "/a");
    }

    #[test]
    fn resolve_folds_dots_and_quotes_its_failures() {
        assert_eq!(resolve(Some("/home/a"), "./lib/../m.nx").unwrap(), "/home/a/m.nx");
        assert_eq!(resolve(Some("/home/a"), "/x/y.nx").unwrap(), "/x/y.nx");
        assert!(resolve(None, "./m.nx ").unwrap_err().starts_with("`./m.nx `: "));
        assert!(resolve(Some("/a"), "").is_err());
    }

    #[test]
    fn cycle_message_names_the_path_and_the_chain() {
        let chain = [String::from("/s/main.nx"), String::from("/s/a.nx")];
        assert_eq!(
            cycle_message("./a.nx", &chain, "/s/a.nx"),
            "`./a.nx` is already being loaded: /s/main.nx → /s/a.nx → /s/a.nx"
        );
    }

    #[test]
    fn a_base_is_stamped_on_every_use_wherever_it_is() {
        let src = "use \"./a.nx\" as a\ndef f() { use \"./b.nx\" as b }\nif true { use \"./c.nx\" as c }";
        let bases = |s: &Script| {
            let mut out = alloc::vec::Vec::new();
            fn walk(stmts: &[crate::ast::Stmt], out: &mut alloc::vec::Vec<Option<String>>) {
                for s in stmts {
                    match s {
                        crate::ast::Stmt::Use { base, .. } => out.push(base.clone()),
                        crate::ast::Stmt::Def { body, .. } => walk(body, out),
                        crate::ast::Stmt::If { then, .. } => walk(then, out),
                        _ => {}
                    }
                }
            }
            walk(&s.stmts, &mut out);
            out
        };
        let with = bases(&parse_script_in(src, Some("/d")).unwrap());
        assert_eq!(with, alloc::vec![Some(String::from("/d")); 3]);
        assert_eq!(bases(&parse_script_in(src, None).unwrap()), alloc::vec![None; 3]);
    }
}
