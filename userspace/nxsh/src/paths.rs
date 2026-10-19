//! Paths the shell resolves itself: where a `use` reads from and the chain of files being loaded.
//!
//! Pure: the host only reads what these name. The kernel refuses a relative path in a namespace
//! lookup, so every file the shell opens goes through [`resolve`] first.

use crate::ast::{Expr, Script};
use crate::eval::{EvalError, Interp};
use crate::exec::is_path_head;
use crate::parse::{ParseError, Parser};
use crate::value::Val;
use alloc::borrow::Cow;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use libstream::wire::{Record, Value};

/// The working directory in `env`: its conventional `PWD` entry, if a string.
pub fn pwd_of(env: &Record) -> Option<&str> {
    env_field(env, "PWD")
}

/// The field `name` of `env`, if a string.
fn env_field<'a>(env: &'a Record, name: &str) -> Option<&'a str> {
    let i = env.schema.fields.iter().position(|f| f.name == name)?;
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

/// The program a stage or a `#!` line names: a path is expanded and made absolute against `env`'s
/// `PWD`, as Linux resolves a relative `#!` against the working directory; a bare name is left for
/// the host to find in `/bin`.
pub fn program_path(program: &str, env: &Record) -> core::result::Result<String, String> {
    if !is_path_head(program) {
        return Ok(String::from(program));
    }
    resolve(pwd_of(env), &expand_home(program, env_field(env, "HOME"))?)
}

/// `word` with a leading `~` replaced by `home`.
///
/// Only `~` and `~/…` name a home: `~name` is refused rather than left as text, and so is any `~`
/// when `home` is unset, since there is no user identity to fall back on. A word that does not
/// start with `~` comes back borrowed and unchanged.
pub fn expand_home<'a>(word: &'a str, home: Option<&str>) -> core::result::Result<Cow<'a, str>, String> {
    let Some(rest) = word.strip_prefix('~') else {
        return Ok(Cow::Borrowed(word));
    };
    if !rest.is_empty() && !rest.starts_with('/') {
        return Err(alloc::format!("`{word}`: only `~` or `~/…` names a home, not `~name`"));
    }
    let home = home.ok_or_else(|| String::from("`~` needs HOME, which is not set"))?;
    Ok(Cow::Owned(alloc::format!("{}{rest}", if rest.is_empty() { home } else { home.trim_end_matches('/') })))
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
    /// `word` with a leading `~` expanded from the interpreter's `HOME`.
    ///
    /// For an unquoted word only; a quoted string is never passed here.
    pub(crate) fn expand_tilde<'a>(&self, word: &'a str) -> core::result::Result<Cow<'a, str>, EvalError> {
        if !word.starts_with('~') {
            return Ok(Cow::Borrowed(word));
        }
        expand_home(word, env_field(&self.env, "HOME")).map_err(EvalError::new)
    }

    /// The directory `dir`, as typed in the word being completed, resolved to list; a leading `~`
    /// is expanded here and stays in the candidates, which are written back as typed.
    pub(crate) fn completion_dir(&self, dir: &str) -> Option<String> {
        let dir = self.expand_tilde(dir).ok()?;
        resolve(self.cwd(), if dir.is_empty() { "." } else { &dir }).ok()
    }

    /// The program a stage named `name` asks for: a path is expanded and made absolute against
    /// `PWD`, any other name is left for the host to find.
    pub(crate) fn stage_program(&self, name: &str) -> core::result::Result<String, EvalError> {
        program_path(name, &self.env).map_err(EvalError::new)
    }

    /// `self` with `$args` bound to the empty list.
    pub(crate) fn with_empty_args(mut self) -> Interp {
        self.set_args(Vec::new());
        self
    }

    /// Bind `$args`, the arguments this shell's script or `-c` source was given.
    pub fn set_args(&mut self, args: Vec<String>) {
        let v = Val::Data(Value::List(Arc::from(args.into_iter().map(Value::Str).collect::<Vec<_>>())));
        let _ = self.bind("$args", v, false, false);
    }

    /// The paths named by `raw` for `open` and `save`: words are tilde-expanded, strings and
    /// names are taken as written.
    pub(crate) fn path_args(&self, raw: &[&Expr]) -> core::result::Result<Vec<String>, EvalError> {
        raw.iter()
            .filter_map(|e| match e {
                Expr::Word(w) => Some(self.expand_tilde(w).map(Cow::into_owned)),
                Expr::Ident(n) | Expr::Str(n) => Some(Ok(n.clone())),
                _ => None,
            })
            .collect()
    }

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
    fn expand_home_replaces_only_a_leading_tilde() {
        assert_eq!(expand_home("~", Some("/home/a")).unwrap(), "/home/a");
        assert_eq!(expand_home("~/x/y", Some("/home/a")).unwrap(), "/home/a/x/y");
        assert_eq!(expand_home("~/x", Some("/")).unwrap(), "/x");
        assert_eq!(expand_home("~/x", Some("/h/")).unwrap(), "/h/x");
        assert!(matches!(expand_home("a~b", None).unwrap(), Cow::Borrowed("a~b")));
        assert!(expand_home("~bob/x", Some("/h")).unwrap_err().contains("~bob/x"));
        assert!(expand_home("~", None).unwrap_err().contains("HOME"));
    }

    #[test]
    fn a_program_path_is_expanded_and_resolved_and_a_name_is_left_alone() {
        let env = |pairs: &[(&str, &str)]| {
            let mut r = Record { schema: libstream::wire::Schema::new(), values: Vec::new() };
            for (k, v) in pairs {
                r.schema = r.schema.clone().field(k, libstream::wire::TypeTag::String, Default::default());
                r.values.push(Value::Str(String::from(*v)));
            }
            r
        };
        let e = env(&[("PWD", "/p/sub"), ("HOME", "/h")]);
        for (named, want) in [
            ("/bin//nxsh", "/bin/nxsh"),
            ("/bin/./nxsh", "/bin/nxsh"),
            ("./tool", "/p/sub/tool"),
            ("../bin/tool", "/p/bin/tool"),
            ("~/bin/x", "/h/bin/x"),
            ("nxsh", "nxsh"),
        ] {
            assert_eq!(program_path(named, &e).as_deref(), Ok(want), "{named}");
        }
        assert!(program_path("./tool", &env(&[])).unwrap_err().contains("no working directory"));
    }

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
