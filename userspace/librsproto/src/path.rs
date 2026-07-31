//! Namespace path syntax: resolving a relative path against a working directory.
//!
//! Milestone 3.5 Part B. Pure — no syscalls, no allocation beyond the result — so it
//! host-tests, which matters because every rule here is a place a naive implementation is
//! quietly wrong.
//!
//! ## Why this exists in userspace at all
//!
//! The kernel does not accept relative paths. `validate_path` refuses a leading non-`/`
//! and refuses `.` and `..` **by name**:
//!
//! ```text
//! if path.is_empty() || path.len() > NS_PATH_MAX || path[0] != b'/' { return Err(InvalidPath) }
//! for comp in path[1..].split(|&c| c == b'/') {
//!     if comp.is_empty() || comp == b"." || comp == b".." { return Err(InvalidPath) }
//! }
//! ```
//!
//! That stays. A relative path is a *userspace* convenience, expanded before any syscall,
//! so the kernel keeps one unambiguous naming rule and nothing has to resolve `..` while
//! holding a namespace lock.
//!
//! It lives in `librsproto` because namespace paths are what its `namespace` ops address,
//! and because it is the one crate both the coreutils and `nxsh` already depend on — so
//! the convention has a single implementation rather than one per program.
//!
//! **Buffer-based, no `alloc`.** `librsproto` is `core`-only with no dependencies, and
//! that constraint turns out to suit this: resolution is a fold over components, so it can
//! be done in place in a caller's buffer with no allocation at all — the same shape
//! `Dir::open` already uses for its message buffer.
//!
//! ## Lexical `..` is correct here, unusually
//!
//! Popping `..` textually is *wrong* in a system with symlinks: `/a/link/..` is not
//! necessarily `/a`, because `link` may point elsewhere. Nitrox has no symlinks — the
//! fs-server rejects them — so there is no discrepancy to be wrong about, and the cheap
//! implementation is also the correct one. If symlinks ever land, this comment is the
//! thing that should stop someone assuming this still holds.


/// Why a path could not be resolved.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PathError {
    /// The path was empty.
    Empty,
    /// A relative path was given but no working directory is known.
    NoWorkingDirectory,
    /// `..` climbed above the root.
    AboveRoot,
    /// The working directory itself was not an absolute path.
    BadWorkingDirectory,
    /// The resolved path did not fit the caller's buffer.
    TooLong,
}

impl PathError {
    /// A message for a person, not a discriminant.
    pub fn message(self) -> &'static str {
        match self {
            PathError::Empty => "the path is empty",
            PathError::NoWorkingDirectory => {
                "this is a relative path and no working directory is set — a process \
                 spawned without a setup message has no environment, so it has no `PWD`"
            }
            PathError::AboveRoot => "the path climbs above the root with `..`",
            PathError::BadWorkingDirectory => "the working directory is not an absolute path",
            PathError::TooLong => "the resolved path is too long",
        }
    }
}

/// Resolve `path` against `cwd` into an absolute, `.`/`..`-free namespace path, written
/// into `out`.
///
/// `cwd` is `None` when the process has no working directory — a Tier-0 program, or one
/// whose environment carries no `PWD`. A relative path then **fails** rather than being
/// silently resolved against `/`: guessing a root would make `remove ./x` delete something
/// in a directory the caller never named.
///
/// Returns the populated prefix of `out`. `out` should be at least `NS_PATH_MAX` (1024).
pub fn resolve<'a>(
    cwd: Option<&[u8]>,
    path: &[u8],
    out: &'a mut [u8],
) -> Result<&'a [u8], PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    let mut len = 0usize;
    if path[0] != b'/' {
        let cwd = cwd.ok_or(PathError::NoWorkingDirectory)?;
        if cwd.is_empty() || cwd[0] != b'/' {
            return Err(PathError::BadWorkingDirectory);
        }
        len = fold(cwd, out, len)?;
    }
    len = fold(path, out, len)?;
    if len == 0 {
        // Everything cancelled out: the root.
        if out.is_empty() {
            return Err(PathError::TooLong);
        }
        out[0] = b'/';
        len = 1;
    }
    Ok(&out[..len])
}

/// Fold `path`'s components into `out`, applying `.` and `..` in place.
///
/// `..` is handled by truncating back to the previous `/`, which is what lets this run
/// with no component stack and therefore no allocation.
fn fold(path: &[u8], out: &mut [u8], mut len: usize) -> Result<usize, PathError> {
    for comp in path.split(|&c| c == b'/') {
        match comp {
            b"" | b"." => {}
            b".." => {
                // Refusing beats clamping. Clamping to `/` would make `../../..` a valid
                // way to say "root", so a mistyped path would silently name a real place
                // instead of failing.
                if len == 0 {
                    return Err(PathError::AboveRoot);
                }
                len = out[..len]
                    .iter()
                    .rposition(|&c| c == b'/')
                    .ok_or(PathError::AboveRoot)?;
            }
            c => {
                if len + 1 + c.len() > out.len() {
                    return Err(PathError::TooLong);
                }
                out[len] = b'/';
                len += 1;
                out[len..len + c.len()].copy_from_slice(c);
                len += c.len();
            }
        }
    }
    Ok(len)
}

/// Whether `path` needs [`resolve`] at all — i.e. is not already an absolute, plain path.
///
/// Lets a caller skip the allocation on the common case without duplicating the rules.
pub fn needs_resolution(path: &[u8]) -> bool {
    if path.is_empty() || path[0] != b'/' {
        return true;
    }
    path.split(|&c| c == b'/')
        .skip(1)
        .any(|c| c.is_empty() || c == b"." || c == b"..")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve into a scratch buffer and hand back a `String` for readable assertions.
    fn r(cwd: &str, path: &str) -> String {
        let mut buf = [0u8; 1024];
        let out = resolve(Some(cwd.as_bytes()), path.as_bytes(), &mut buf)
            .unwrap_or_else(|e| panic!("{}: {}", path, e.message()));
        String::from_utf8_lossy(out).into_owned()
    }

    fn err_of(cwd: Option<&str>, path: &str) -> PathError {
        let mut buf = [0u8; 1024];
        match resolve(cwd.map(|c| c.as_bytes()), path.as_bytes(), &mut buf) {
            Ok(p) => panic!("expected an error, got {}", String::from_utf8_lossy(p)),
            Err(e) => e,
        }
    }

    fn ok_of(cwd: Option<&str>, path: &str) -> String {
        let mut buf = [0u8; 1024];
        let out = resolve(cwd.map(|c| c.as_bytes()), path.as_bytes(), &mut buf).expect("resolves");
        String::from_utf8_lossy(out).into_owned()
    }

    #[test]
    fn an_absolute_path_passes_through() {
        assert_eq!(r("/home/alice", "/system/x"), "/system/x");
        assert_eq!(r("/home/alice", "/"), "/");
    }

    #[test]
    fn a_relative_path_joins_the_working_directory() {
        assert_eq!(r("/home/alice", "notes.txt"), "/home/alice/notes.txt");
        assert_eq!(r("/home/alice", "./notes.txt"), "/home/alice/notes.txt");
        assert_eq!(r("/home/alice", "a/b"), "/home/alice/a/b");
    }

    #[test]
    fn dot_dot_pops_lexically() {
        assert_eq!(r("/home/alice", "../bob"), "/home/bob");
        assert_eq!(r("/home/alice", "../../system"), "/system");
        assert_eq!(r("/a", "b/../c"), "/a/c");
        // …in an absolute path too, since the kernel refuses `..` either way.
        assert_eq!(r("/home/alice", "/a/../b"), "/b");
    }

    /// Refusing beats clamping: were `..` to stop at the root, `../../..` would be a
    /// valid way to say `/`, and a mistyped path would name a real place instead of
    /// failing.
    #[test]
    fn climbing_above_the_root_is_an_error() {
        assert_eq!(err_of(Some("/"), ".."), PathError::AboveRoot);
        assert_eq!(err_of(Some("/a"), "../.."), PathError::AboveRoot);
        assert_eq!(err_of(Some("/a/b"), "../../../x"), PathError::AboveRoot);
    }

    #[test]
    fn a_bare_dot_is_the_working_directory() {
        assert_eq!(r("/home/alice", "."), "/home/alice");
        assert_eq!(r("/", "."), "/");
        assert_eq!(r("/home/alice", "./"), "/home/alice");
    }

    /// The kernel refuses empty components and trailing slashes, and both are things a
    /// person types — so they are normalised away rather than passed on to fail.
    #[test]
    fn redundant_separators_are_normalised_away() {
        assert_eq!(r("/a", "b//c"), "/a/b/c");
        assert_eq!(r("/a", "/x/y/"), "/x/y");
        assert_eq!(r("/a/", "b"), "/a/b");
    }

    /// A relative path with no working directory fails rather than being resolved against
    /// `/`. Guessing a root would make `remove ./x` delete something in a directory the
    /// caller never named.
    #[test]
    fn a_relative_path_without_a_working_directory_is_refused() {
        assert_eq!(err_of(None, "x"), PathError::NoWorkingDirectory);
        assert_eq!(err_of(None, "./x"), PathError::NoWorkingDirectory);
        // …but an absolute one still works: no working directory is needed.
        assert_eq!(ok_of(None, "/x"), "/x");
    }

    #[test]
    fn a_malformed_working_directory_is_refused() {
        assert_eq!(err_of(Some("relative"), "x"), PathError::BadWorkingDirectory);
        assert_eq!(err_of(Some(""), "x"), PathError::BadWorkingDirectory);
    }

    #[test]
    fn an_empty_path_is_refused() {
        assert_eq!(err_of(Some("/a"), ""), PathError::Empty);
    }

    /// Every successful result is something `validate_path` accepts: absolute, no empty
    /// components, no `.` or `..`, no trailing slash. That is the whole contract.
    #[test]
    fn every_result_is_a_path_the_kernel_would_accept() {
        let cases = [
            ("/home/alice", "notes.txt"),
            ("/home/alice", "../bob/./x"),
            ("/a/b/c", "../../d//e/"),
            ("/", "x"),
            ("/a", "."),
        ];
        for (cwd, path) in cases {
            let out = r(cwd, path);
            assert!(out.starts_with('/'), "{out} is not absolute");
            assert!(out == "/" || !out.ends_with('/'), "{out} has a trailing slash");
            for comp in out.split('/').skip(1) {
                if out == "/" {
                    continue;
                }
                assert!(!comp.is_empty(), "{out} has an empty component");
                assert_ne!(comp, ".", "{out} kept a `.`");
                assert_ne!(comp, "..", "{out} kept a `..`");
            }
        }
    }

    #[test]
    fn needs_resolution_skips_the_common_case() {
        assert!(!needs_resolution(b"/system/x"));
        assert!(needs_resolution(b"x"));
        assert!(needs_resolution(b"./x"));
        assert!(needs_resolution(b"/a/../b"));
        assert!(needs_resolution(b"/a//b"));
        assert!(needs_resolution(b"/a/"));
    }
}
