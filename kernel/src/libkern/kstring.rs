//! [`KString`] — the kernel's fallible UTF-8 string, plus [`kformat!`].

use core::fmt;
use core::ops::Deref;

use crate::libkern::AllocError;
use crate::libkern::KVec;

/// A fallible, growable, heap-backed UTF-8 string — the kernel's
/// analogue of `alloc::string::String`.
///
/// `KString` is a thin wrapper over [`KVec<u8>`] whose contents are
/// guaranteed to be valid UTF-8 at all times: every mutator accepts only
/// `&str` input. Growth is fallible, so the kernel uses `KString` in
/// place of `alloc`'s `String` — see the decision log entry of
/// 2026-05-20.
///
/// Pair it with [`kformat!`] for `format!`-style construction that
/// reports allocation failure instead of aborting.
pub struct KString {
    /// Backing bytes. Always well-formed UTF-8.
    buf: KVec<u8>,
}

impl KString {
    /// Create an empty string. No allocation happens until the first
    /// push, so this is usable in `const` context.
    pub const fn new() -> Self {
        KString { buf: KVec::new() }
    }

    /// Build a `KString` holding a copy of `s`.
    pub fn try_from_str(s: &str) -> Result<Self, AllocError> {
        let mut ks = KString::new();
        ks.try_push_str(s)?;
        Ok(ks)
    }

    /// Borrow the contents as a string slice.
    pub fn as_str(&self) -> &str {
        // SAFETY: `buf` is only ever appended to from `&str` inputs, so
        // its bytes are always well-formed UTF-8.
        unsafe { core::str::from_utf8_unchecked(&self.buf) }
    }

    /// Borrow the raw UTF-8 bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Length in bytes (not characters).
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// `true` when the string holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Append a string slice.
    pub fn try_push_str(&mut self, s: &str) -> Result<(), AllocError> {
        self.buf.try_extend_from_slice(s.as_bytes())
    }

    /// Append a single character.
    pub fn try_push(&mut self, c: char) -> Result<(), AllocError> {
        let mut utf8 = [0u8; 4];
        self.try_push_str(c.encode_utf8(&mut utf8))
    }
}

impl Deref for KString {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl Default for KString {
    fn default() -> Self {
        Self::new()
    }
}

/// Appends formatted output. `write_str` fails with [`fmt::Error`] when
/// the backing [`KVec`] cannot grow; [`kformat!`] turns that back into an
/// [`AllocError`].
impl fmt::Write for KString {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.try_push_str(s).map_err(|_| fmt::Error)
    }
}

impl fmt::Display for KString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for KString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

/// `format!` for the kernel: build a [`KString`] from format arguments,
/// reporting heap exhaustion as `Err(`[`AllocError`]`)` instead of
/// aborting.
///
/// ```ignore
/// let msg = kformat!("fault at {:#x}, thread {}", addr, tid)?;
/// ```
///
/// `#[macro_export]` places the macro at the crate root; reference it as
/// `crate::kformat!` from kernel code.
#[macro_export]
macro_rules! kformat {
    ($($arg:tt)*) => {{
        let mut s = $crate::libkern::KString::new();
        match ::core::fmt::Write::write_fmt(&mut s, ::core::format_args!($($arg)*)) {
            ::core::result::Result::Ok(()) => ::core::result::Result::Ok(s),
            ::core::result::Result::Err(_) => {
                ::core::result::Result::Err($crate::libkern::AllocError)
            }
        }
    }};
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::test_support::init_global_heap;

    #[test]
    fn new_is_empty() {
        let s = KString::new();
        assert!(s.is_empty());
        assert_eq!(s.len(), 0);
        assert_eq!(s.as_str(), "");
    }

    #[test]
    fn from_str_round_trips() {
        init_global_heap();
        let s = KString::try_from_str("hello, nitrox").unwrap();
        assert_eq!(s.as_str(), "hello, nitrox");
        assert_eq!(s.len(), 13);
    }

    #[test]
    fn push_str_appends() {
        init_global_heap();
        let mut s = KString::new();
        s.try_push_str("ni").unwrap();
        s.try_push_str("trox").unwrap();
        assert_eq!(s.as_str(), "nitrox");
    }

    #[test]
    fn push_char_handles_multibyte() {
        init_global_heap();
        let mut s = KString::new();
        s.try_push('a').unwrap();
        s.try_push('\u{2603}').unwrap(); // snowman, 3 UTF-8 bytes
        assert_eq!(s.as_str(), "a\u{2603}");
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn deref_exposes_str_methods() {
        init_global_heap();
        let s = KString::try_from_str("Nitrox Kernel").unwrap();
        assert!(s.starts_with("Nitrox"));
        assert_eq!(s.split_whitespace().count(), 2);
    }

    #[test]
    fn kformat_builds_formatted_string() {
        init_global_heap();
        let s = kformat!("fault at {:#x}, thread {}", 0xdead_beefu32, 7).unwrap();
        assert_eq!(s.as_str(), "fault at 0xdeadbeef, thread 7");
    }

    #[test]
    fn write_trait_appends_via_write_macro() {
        init_global_heap();
        use core::fmt::Write;
        let mut s = KString::new();
        write!(&mut s, "{}+{}={}", 2, 2, 4).unwrap();
        assert_eq!(s.as_str(), "2+2=4");
    }
}
