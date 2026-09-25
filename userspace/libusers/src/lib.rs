//! **`/system/users`, the credential file** — its format, the rules for what may go in it, and
//! the edits that change it (administration Part D.1).
//!
//! ```text
//! # comment and blank lines are kept as they are
//! name:salt_hex:iterations:verifier_hex:home
//! ```
//!
//! The verifier is `PBKDF2-HMAC-SHA256(password, salt, iterations)` (`libcrypto::password`), a
//! one-way value: the file never holds a password.
//!
//! **Three writers, so the format lives below all of them.** `auth-service` reads the file at boot
//! and rewrites it for an administrator's request; `xtask` seeds the build's account; `account`'s
//! offline mode edits an installed disk's file from the live image. Each writes a record with
//! [`write_record`] and each edit goes through [`add`], [`remove`] or [`set_password`], so none
//! can write a line the service would read differently.
//!
//! **No `alloc`.** An edit writes the whole new file into a buffer its caller owns, and refuses
//! one longer than [`MAX_FILE`] — the most `auth-service` loads at boot. A file over it is one the
//! service will not load, and then nobody logs in, so the bound is the file's rather than the
//! service's (PR #337 review).
//!
//! `#![no_std]`, and `std` under `cargo test`.

#![cfg_attr(not(test), no_std)]

/// The longest file `auth-service` loads — one page.
pub const MAX_FILE: usize = 4096;
/// The longest account name.
pub const NAME_MAX: usize = 32;
/// The longest password.
pub const PASSWORD_MAX: usize = 128;
/// The salt a new password is given.
pub const SALT_LEN: usize = 16;
/// The longest salt a record may hold.
pub const SALT_MAX: usize = 32;
/// A verifier's length: one SHA-256 block.
pub const VERIFIER_LEN: usize = libcrypto::password::VERIFIER_LEN;
/// The PBKDF2 iteration count a new password is given. Each record keeps its own, so raising
/// this changes new passwords and leaves old ones readable.
pub const ITERATIONS: u32 = libcrypto::password::DEFAULT_ITERATIONS;

/// Why an edit was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The name is not a valid one ([`valid_name`]).
    BadName,
    /// The password is empty, or longer than [`PASSWORD_MAX`].
    BadPassword,
    /// An account has that name already.
    Exists,
    /// No account has that name.
    NoSuchAccount,
    /// The file would be longer than [`MAX_FILE`], or than the buffer given for it.
    TooLarge,
}

impl Refusal {
    /// The reason, as a refusal says it.
    pub fn why(self) -> &'static [u8] {
        match self {
            Refusal::BadName => {
                b"a name is 1 to 32 bytes: a lowercase letter or `_`, then lowercase letters, digits, `_` or `-`"
            }
            Refusal::BadPassword => b"a password is 1 to 128 bytes",
            Refusal::Exists => b"an account has that name already",
            Refusal::NoSuchAccount => b"no account has that name",
            Refusal::TooLarge => b"the user database would be larger than the 4 KiB auth-service loads",
        }
    }
}

/// One record, its fields borrowing from the line.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Record<'a> {
    pub name: &'a [u8],
    pub salt_hex: &'a [u8],
    pub iterations: u32,
    pub verifier_hex: &'a [u8],
    pub home: &'a [u8],
}

impl<'a> Record<'a> {
    /// Parse `name:salt_hex:iterations:verifier_hex:home`, already trimmed. `None` if a field is
    /// missing, the name or home is empty, or `iterations` is not a decimal number.
    pub fn parse(line: &'a [u8]) -> Option<Record<'a>> {
        let mut it = line.splitn(5, |&b| b == b':');
        let name = it.next()?;
        let salt_hex = it.next()?;
        let iterations = parse_u32(it.next()?)?;
        let verifier_hex = it.next()?;
        let home = it.next()?;
        if name.is_empty() || home.is_empty() {
            return None;
        }
        Some(Record { name, salt_hex, iterations, verifier_hex, home })
    }

    /// The salt, decoded into `out`; its length. `None` if it is not hex or will not fit.
    pub fn salt(&self, out: &mut [u8; SALT_MAX]) -> Option<usize> {
        hex_decode(self.salt_hex, out)
    }

    /// The verifier, decoded. `None` if it is not hex of exactly [`VERIFIER_LEN`] bytes.
    pub fn verifier(&self) -> Option<[u8; VERIFIER_LEN]> {
        let mut v = [0u8; VERIFIER_LEN];
        (hex_decode(self.verifier_hex, &mut v)? == VERIFIER_LEN).then_some(v)
    }

    /// Whether `password` is this account's, in constant time over the verifier. A record whose
    /// salt or verifier does not decode matches nothing.
    pub fn verifies(&self, password: &[u8]) -> bool {
        let mut salt = [0u8; SALT_MAX];
        let (Some(n), Some(v)) = (self.salt(&mut salt), self.verifier()) else {
            return false;
        };
        libcrypto::password::verify(password, &salt[..n], self.iterations, &v)
    }
}

/// The file's lines, each with its end: `(start, end)` where `end` includes the `\n`, if there
/// is one.
fn lines(file: &[u8]) -> impl Iterator<Item = (usize, usize)> + '_ {
    let mut at = 0;
    core::iter::from_fn(move || {
        if at >= file.len() {
            return None;
        }
        let start = at;
        let end = file[at..].iter().position(|&b| b == b'\n').map_or(file.len(), |i| at + i + 1);
        at = end;
        Some((start, end))
    })
}

/// A line's record, if it holds one: not blank, not a comment, and well-formed. A malformed line
/// is skipped rather than fatal, as `auth-service` has always read the file.
fn record_of(line: &[u8]) -> Option<Record<'_>> {
    let line = trim(line);
    if line.is_empty() || line[0] == b'#' {
        return None;
    }
    Record::parse(line)
}

/// Every record in the file, in order.
pub fn records(file: &[u8]) -> impl Iterator<Item = Record<'_>> + '_ {
    lines(file).filter_map(move |(s, e)| record_of(&file[s..e]))
}

/// The first record named `name`. The first, because that is the one `auth-service` has always
/// authenticated against.
pub fn find<'a>(file: &'a [u8], name: &[u8]) -> Option<Record<'a>> {
    records(file).find(|r| r.name == name)
}

/// Where the record named `name` is: its line's `(start, end)`.
fn line_of(file: &[u8], name: &[u8]) -> Option<(usize, usize)> {
    lines(file).find(|&(s, e)| record_of(&file[s..e]).is_some_and(|r| r.name == name))
}

/// **Whether `name` may be an account's name**: 1 to [`NAME_MAX`] bytes, a lowercase letter or
/// `_`, then lowercase letters, digits, `_` or `-`. So never `*`, the policy's wildcard, and never
/// a byte the file or a path would read differently: `:`, `/`, `\n`.
pub fn valid_name(name: &[u8]) -> bool {
    let first = |c: u8| c.is_ascii_lowercase() || c == b'_';
    let rest = |c: u8| first(c) || c.is_ascii_digit() || c == b'-';
    (1..=NAME_MAX).contains(&name.len()) && first(name[0]) && name[1..].iter().all(|&c| rest(c))
}

/// Whether `password` may be one: 1 to [`PASSWORD_MAX`] bytes, of anything. Only its verifier is
/// stored, so no byte of it can break the file.
pub fn valid_password(password: &[u8]) -> bool {
    (1..=PASSWORD_MAX).contains(&password.len())
}

/// An account's home, `/home/<name>`, written into `out`; its length.
pub fn home_for(name: &[u8], out: &mut [u8]) -> Option<usize> {
    let n = 6 + name.len();
    let out = out.get_mut(..n)?;
    out[..6].copy_from_slice(b"/home/");
    out[6..].copy_from_slice(name);
    Some(n)
}

/// **Write one record's line**, `\n` included, into `out`: `password` derived under `salt` with
/// [`ITERATIONS`]. Its length, or why not.
pub fn write_record(
    out: &mut [u8],
    name: &[u8],
    home: &[u8],
    password: &[u8],
    salt: &[u8],
) -> Result<usize, Refusal> {
    write_record_with(out, name, home, password, salt, ITERATIONS)
}

/// [`write_record`], at `iterations` — for a test that cannot afford the real count, and for a
/// seeder that wants a record's cost fixed.
pub fn write_record_with(
    out: &mut [u8],
    name: &[u8],
    home: &[u8],
    password: &[u8],
    salt: &[u8],
    iterations: u32,
) -> Result<usize, Refusal> {
    if !valid_name(name) {
        return Err(Refusal::BadName);
    }
    if !valid_password(password) {
        return Err(Refusal::BadPassword);
    }
    let verifier = libcrypto::password::derive(password, salt, iterations);
    let mut w = Writer { out, at: 0 };
    w.put(name)?;
    w.put(b":")?;
    w.hex(salt)?;
    w.put(b":")?;
    let mut digits = [0u8; 10];
    w.put(fmt_u32(iterations, &mut digits))?;
    w.put(b":")?;
    w.hex(&verifier)?;
    w.put(b":")?;
    w.put(home)?;
    w.put(b"\n")?;
    Ok(w.at)
}

/// **The file with an account added**: `name`, home `/home/<name>`, `password` under `salt`,
/// appended after every line already there, which are kept byte for byte.
pub fn add(file: &[u8], out: &mut [u8], name: &[u8], password: &[u8], salt: &[u8]) -> Result<usize, Refusal> {
    add_with(file, out, name, password, salt, ITERATIONS)
}

/// [`add`], at `iterations`.
pub fn add_with(
    file: &[u8],
    out: &mut [u8],
    name: &[u8],
    password: &[u8],
    salt: &[u8],
    iterations: u32,
) -> Result<usize, Refusal> {
    if !valid_name(name) {
        return Err(Refusal::BadName);
    }
    if find(file, name).is_some() {
        return Err(Refusal::Exists);
    }
    let mut home = [0u8; 6 + NAME_MAX];
    let hn = home_for(name, &mut home).ok_or(Refusal::BadName)?;
    let mut line = [0u8; MAX_FILE];
    let ln = write_record_with(&mut line, name, &home[..hn], password, salt, iterations)?;
    let gap: &[u8] = if file.is_empty() || file.ends_with(b"\n") { b"" } else { b"\n" };
    splice(out, &[file, gap, &line[..ln]])
}

/// **The file with `name`'s record taken out**, every other line kept byte for byte.
pub fn remove(file: &[u8], out: &mut [u8], name: &[u8]) -> Result<usize, Refusal> {
    let (s, e) = line_of(file, name).ok_or(Refusal::NoSuchAccount)?;
    splice(out, &[&file[..s], &file[e..]])
}

/// **The file with `name`'s password replaced**: a new verifier under `salt`, in the record's own
/// place, keeping its name and home. Every other line is kept byte for byte. The line grows when
/// the new salt is longer than the old — the build's 8 bytes against [`SALT_LEN`]'s 16 — so this
/// can be refused as [`Refusal::TooLarge`] too.
pub fn set_password(
    file: &[u8],
    out: &mut [u8],
    name: &[u8],
    password: &[u8],
    salt: &[u8],
) -> Result<usize, Refusal> {
    set_password_with(file, out, name, password, salt, ITERATIONS)
}

/// [`set_password`], at `iterations`.
pub fn set_password_with(
    file: &[u8],
    out: &mut [u8],
    name: &[u8],
    password: &[u8],
    salt: &[u8],
    iterations: u32,
) -> Result<usize, Refusal> {
    let (s, e) = line_of(file, name).ok_or(Refusal::NoSuchAccount)?;
    let old = record_of(&file[s..e]).ok_or(Refusal::NoSuchAccount)?;
    let mut line = [0u8; MAX_FILE];
    let ln = write_record_with(&mut line, old.name, old.home, password, salt, iterations)?;
    // The record's line had no `\n` only if it was the file's last; keep it that way.
    let ln = if file[..e].ends_with(b"\n") { ln } else { ln - 1 };
    splice(out, &[&file[..s], &line[..ln], &file[e..]])
}

/// Concatenate `parts` into `out`; its length, or [`Refusal::TooLarge`] past [`MAX_FILE`] or `out`.
fn splice(out: &mut [u8], parts: &[&[u8]]) -> Result<usize, Refusal> {
    let n: usize = parts.iter().map(|p| p.len()).sum();
    if n > MAX_FILE || n > out.len() {
        return Err(Refusal::TooLarge);
    }
    let mut at = 0;
    for p in parts {
        out[at..at + p.len()].copy_from_slice(p);
        at += p.len();
    }
    Ok(n)
}

/// Bytes appended to a caller's buffer, refusing past its end.
struct Writer<'a> {
    out: &'a mut [u8],
    at: usize,
}

impl Writer<'_> {
    fn put(&mut self, b: &[u8]) -> Result<(), Refusal> {
        let end = self.at + b.len();
        self.out.get_mut(self.at..end).ok_or(Refusal::TooLarge)?.copy_from_slice(b);
        self.at = end;
        Ok(())
    }

    fn hex(&mut self, b: &[u8]) -> Result<(), Refusal> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        for &c in b {
            self.put(&[HEX[(c >> 4) as usize], HEX[(c & 0xF) as usize]])?;
        }
        Ok(())
    }
}

/// `v` in decimal, written into `buf`; the digits.
fn fmt_u32(mut v: u32, buf: &mut [u8; 10]) -> &[u8] {
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            return &buf[i..];
        }
    }
}

/// Trim leading and trailing spaces, tabs and `\r`, and a trailing `\n`.
fn trim(s: &[u8]) -> &[u8] {
    let ws = |c: u8| c == b' ' || c == b'\t' || c == b'\r' || c == b'\n';
    let a = s.iter().position(|&c| !ws(c)).unwrap_or(s.len());
    let b = s.iter().rposition(|&c| !ws(c)).map_or(a, |i| i + 1);
    &s[a..b]
}

/// A decimal `u32`; `None` if empty, not all digits, or overflowing.
fn parse_u32(s: &[u8]) -> Option<u32> {
    if s.is_empty() {
        return None;
    }
    let mut n: u32 = 0;
    for &c in s {
        let d = c.checked_sub(b'0').filter(|&d| d < 10)?;
        n = n.checked_mul(10)?.checked_add(d as u32)?;
    }
    Some(n)
}

/// Hex into `out`; the byte count. `None` on an odd length, a non-hex digit, or too little room.
fn hex_decode(hex: &[u8], out: &mut [u8]) -> Option<usize> {
    if hex.len() % 2 != 0 || hex.len() / 2 > out.len() {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    for (i, pair) in hex.chunks_exact(2).enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(hex.len() / 2)
}

#[cfg(test)]
mod tests;
