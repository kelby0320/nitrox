use super::*;
use std::string::String;
use std::vec::Vec;

/// A cheap iteration count: the tests are about the file, and `libcrypto` has its own vectors.
const IT: u32 = 2;

/// The build's header, as `xtask` seeds it.
const HEADER: &str = "# Nitrox user database (auth-service).\n# name:salt_hex:iterations:verifier_hex:home\n";

fn line(name: &str, password: &str, salt: &[u8]) -> String {
    let mut out = [0u8; MAX_FILE];
    let n = write_record_with(&mut out, name.as_bytes(), format!("/home/{name}").as_bytes(), password.as_bytes(), salt, IT)
        .unwrap();
    String::from_utf8(out[..n].to_vec()).unwrap()
}

/// Run an edit into a buffer **larger than [`MAX_FILE`]**, so that the bound, not the buffer, is what
/// refuses a file one byte too long. With a buffer of exactly `MAX_FILE` the buffer's own check
/// refused first, and a bound moved off by one passed every test (D.1's first control).
fn edit(f: impl FnOnce(&mut [u8]) -> Result<usize, Refusal>) -> Result<String, Refusal> {
    let mut out = [0u8; 2 * MAX_FILE];
    f(&mut out).map(|n| String::from_utf8(out[..n].to_vec()).unwrap())
}

// --- the format --------------------------------------------------------------------------------

/// **A record written here reads back, and its password verifies** — the property that makes the
/// three writers agree with the one reader.
#[test]
fn a_written_record_reads_back_and_verifies() {
    let l = line("alice", "correct horse", b"\x01\x02\x03\x04\x05\x06\x07\x08");
    assert!(l.ends_with('\n'));
    assert_eq!(l.matches(':').count(), 4);
    let r = Record::parse(l.trim_end().as_bytes()).unwrap();
    assert_eq!((r.name, r.home, r.iterations), (&b"alice"[..], &b"/home/alice"[..], IT));
    assert!(r.verifies(b"correct horse"));
    assert!(!r.verifies(b"correct horsf"));
    assert!(!r.verifies(b""));
}

/// **A record written by hand, as the build's always was, reads the same way** — the format
/// this crate took over is the one `auth-service` already served.
#[test]
fn a_record_in_the_old_hand_written_form_still_verifies() {
    let salt = [0x9e, 0x3f, 0xa2, 0x5c, 0x71, 0x0b, 0xd4, 0x86];
    let v = libcrypto::password::derive(b"pw", &salt, IT);
    let hex = |b: &[u8]| b.iter().map(|c| format!("{c:02x}")).collect::<String>();
    let text = format!("{HEADER}alice:{}:{IT}:{}:/home/alice\n", hex(&salt), hex(&v));
    let r = find(text.as_bytes(), b"alice").unwrap();
    assert!(r.verifies(b"pw"));
}

/// **Comments, blanks and malformed lines are not records**, and the first of two records with
/// one name is the one found.
#[test]
fn only_well_formed_lines_are_records() {
    let text = format!(
        "{HEADER}\n  \nnot a record\nx:zz:1:zz\n{}{}",
        line("alice", "a", b"s1"),
        line("alice", "b", b"s2")
    );
    let names: Vec<&[u8]> = records(text.as_bytes()).map(|r| r.name).collect();
    assert_eq!(names, [&b"alice"[..], b"alice"]);
    assert!(find(text.as_bytes(), b"alice").unwrap().verifies(b"a"), "the first");
    assert!(find(text.as_bytes(), b"bob").is_none());
}

/// **In a file naming someone twice, the edits act on the record a login reads** — the first — and
/// leave the other as it was (PR #338 review: nothing held `line_of` and `find` to the same one).
#[test]
fn a_duplicated_name_is_edited_where_it_is_read() {
    let first = line("alice", "a", b"s1");
    let second = line("alice", "b", b"s2");
    let text = format!("{HEADER}{first}{second}");
    let set = edit(|o| set_password_with(text.as_bytes(), o, b"alice", b"c", b"s3", IT)).unwrap();
    assert!(find(set.as_bytes(), b"alice").unwrap().verifies(b"c"), "the record a login reads");
    assert!(set.ends_with(&second), "the second line, byte for byte");
    let removed = edit(|o| remove(text.as_bytes(), o, b"alice")).unwrap();
    assert!(find(removed.as_bytes(), b"alice").unwrap().verifies(b"b"), "the first went");
    assert_eq!(removed, format!("{HEADER}{second}"));
}

/// **A salt or verifier that is not hex matches no password**, rather than panicking or matching.
#[test]
fn a_record_whose_hex_is_broken_verifies_nothing() {
    let r = Record::parse(b"alice:zz:2:00:/home/alice").unwrap();
    assert!(!r.verifies(b""));
    assert!(!r.verifies(b"anything"));
    let short = Record::parse(b"alice:0102:2:00ff:/home/alice").unwrap();
    assert!(short.verifier().is_none(), "a verifier must be exactly 32 bytes");
}

// --- the rules ---------------------------------------------------------------------------------

/// **A name, at each edge of the rule.**
#[test]
fn a_name_is_lowercase_and_at_most_thirty_two_bytes() {
    for ok in ["alice", "_x", "a", "bob-2", "a_b-c9", &"a".repeat(NAME_MAX)] {
        assert!(valid_name(ok.as_bytes()), "{ok}");
    }
    for bad in ["", "*", "Alice", "2bob", "-x", "a b", "a:b", "a/b", "a\nb", "é", &"a".repeat(NAME_MAX + 1)] {
        assert!(!valid_name(bad.as_bytes()), "{bad:?}");
    }
}

/// **A password is 1 to 128 bytes, of anything.**
#[test]
fn a_password_is_one_to_one_hundred_and_twenty_eight_bytes() {
    assert!(!valid_password(b""));
    assert!(valid_password(b"x"));
    assert!(valid_password(&[b':'; PASSWORD_MAX]), "a colon is stored as a verifier, not as itself");
    assert!(!valid_password(&[b'x'; PASSWORD_MAX + 1]));
}

#[test]
fn a_home_is_under_home() {
    let mut out = [0u8; 64];
    let n = home_for(b"bob", &mut out).unwrap();
    assert_eq!(&out[..n], b"/home/bob");
    assert!(home_for(b"bob", &mut [0u8; 8]).is_none(), "too little room");
}

// --- the edits ---------------------------------------------------------------------------------

/// **An add appends, and keeps everything before it byte for byte** — comments included.
#[test]
fn an_add_appends_and_keeps_the_rest() {
    let file = format!("{HEADER}{}", line("alice", "a", b"s1"));
    let out = edit(|o| add_with(file.as_bytes(), o, b"bob", b"bpw", b"0123456789abcdef", IT)).unwrap();
    assert!(out.starts_with(&file), "the old file, then the new line");
    let r = find(out.as_bytes(), b"bob").unwrap();
    assert_eq!(r.home, b"/home/bob");
    assert!(r.verifies(b"bpw"));
}

/// **A file whose last line has no `\n` gets one before the new record**, so the two do not run
/// together into one line.
#[test]
fn an_add_to_a_file_without_a_final_newline_starts_a_line() {
    let file = line("alice", "a", b"s1");
    let file = file.trim_end();
    let out = edit(|o| add_with(file.as_bytes(), o, b"bob", b"b", b"s2", IT)).unwrap();
    assert!(find(out.as_bytes(), b"alice").unwrap().verifies(b"a"));
    assert!(find(out.as_bytes(), b"bob").unwrap().verifies(b"b"));
}

#[test]
fn an_add_is_refused_for_each_reason() {
    let file = format!("{HEADER}{}", line("alice", "a", b"s1"));
    let f = file.as_bytes();
    assert_eq!(edit(|o| add_with(f, o, b"alice", b"x", b"s", IT)), Err(Refusal::Exists));
    assert_eq!(edit(|o| add_with(f, o, b"Bob", b"x", b"s", IT)), Err(Refusal::BadName));
    assert_eq!(edit(|o| add_with(f, o, b"bob", b"", b"s", IT)), Err(Refusal::BadPassword));
}

/// **Every edit refuses a bad name as one, before it looks** — even a name a hand-edited file
/// holds a record under — and `set_password` a bad password too (PR #338 review: `remove` and
/// `set_password` used to look first, and answer `NoSuchAccount`).
#[test]
fn an_edit_refuses_the_rules_before_it_looks() {
    let hand_edited = line("bob", "b", b"s2").replacen("bob", "Bob", 1);
    let file = format!("{HEADER}{}{hand_edited}", line("alice", "a", b"s1"));
    let f = file.as_bytes();
    assert!(find(f, b"Bob").is_some(), "the file does hold a record under the bad name");
    assert_eq!(edit(|o| remove(f, o, b"Bob")), Err(Refusal::BadName));
    assert_eq!(edit(|o| set_password_with(f, o, b"Bob", b"x", b"s", IT)), Err(Refusal::BadName));
    assert_eq!(edit(|o| set_password_with(f, o, b"Bad Name", b"x", b"s", IT)), Err(Refusal::BadName));
    assert_eq!(edit(|o| set_password_with(f, o, b"alice", b"", b"s", IT)), Err(Refusal::BadPassword));
    assert_eq!(edit(|o| set_password_with(f, o, b"dave", b"x", b"s", IT)), Err(Refusal::NoSuchAccount));
    // The order, where it shows: a bad password for someone absent is still a bad password.
    assert_eq!(edit(|o| set_password_with(f, o, b"dave", b"", b"s", IT)), Err(Refusal::BadPassword));
}

/// **A removal takes out that record's line and nothing else.**
#[test]
fn a_removal_takes_one_line_out() {
    let (a, b, c) = (line("alice", "a", b"s1"), line("bob", "b", b"s2"), line("carol", "c", b"s3"));
    let file = format!("{HEADER}{a}{b}{c}");
    let out = edit(|o| remove(file.as_bytes(), o, b"bob")).unwrap();
    assert_eq!(out, format!("{HEADER}{a}{c}"));
    assert_eq!(edit(|o| remove(file.as_bytes(), o, b"dave")), Err(Refusal::NoSuchAccount));
    // The last line, with no `\n` after it.
    let unended = format!("{HEADER}{a}{}", c.trim_end());
    assert_eq!(edit(|o| remove(unended.as_bytes(), o, b"carol")).unwrap(), format!("{HEADER}{a}"));
}

/// **A new password replaces the record in its own place**: the old one stops verifying, the new
/// one does, the name and home are kept, and every other line is as it was.
#[test]
fn a_new_password_replaces_the_record_in_place() {
    let (a, b, c) = (line("alice", "a", b"s1"), line("bob", "old", b"12345678"), line("carol", "c", b"s3"));
    let file = format!("{HEADER}{a}{b}{c}");
    let out = edit(|o| set_password_with(file.as_bytes(), o, b"bob", b"new", b"0123456789abcdef", IT)).unwrap();
    assert!(out.starts_with(&format!("{HEADER}{a}")));
    assert!(out.ends_with(&c));
    let r = find(out.as_bytes(), b"bob").unwrap();
    assert!(r.verifies(b"new"));
    assert!(!r.verifies(b"old"));
    assert_eq!(r.home, b"/home/bob");
    assert_eq!(records(out.as_bytes()).count(), 3);
    assert_eq!(
        edit(|o| set_password_with(file.as_bytes(), o, b"dave", b"x", b"s", IT)),
        Err(Refusal::NoSuchAccount)
    );
    assert_eq!(
        edit(|o| set_password_with(file.as_bytes(), o, b"bob", b"", b"s", IT)),
        Err(Refusal::BadPassword)
    );
}

/// **The record's home is kept as the file has it**, not recomputed from the name: a home an
/// administrator placed elsewhere by hand stays there.
#[test]
fn a_new_password_keeps_a_home_that_is_not_the_default() {
    let mut out = [0u8; MAX_FILE];
    let n = write_record_with(&mut out, b"bob", b"/home/elsewhere", b"old", b"s", IT).unwrap();
    let file = String::from_utf8(out[..n].to_vec()).unwrap();
    let new = edit(|o| set_password_with(file.as_bytes(), o, b"bob", b"new", b"t", IT)).unwrap();
    assert_eq!(find(new.as_bytes(), b"bob").unwrap().home, b"/home/elsewhere");
}

// --- the bound, at its neighbours --------------------------------------------------------------

/// A file of exactly `len` bytes: a padding comment, then `tail`.
fn padded_to(len: usize, tail: &str) -> String {
    let pad = len - tail.len() - 2; // "#" and "\n"
    format!("#{}\n{tail}", "x".repeat(pad))
}

/// **An add that makes the file exactly `MAX_FILE` bytes is taken, and one byte more is refused.**
#[test]
fn an_add_is_bounded_at_exactly_max_file() {
    let bob = line("bob", "b", b"0123456789abcdef");
    let alice = line("alice", "a", b"s1");
    // The add appends `bob`'s line; make the rest exactly `MAX_FILE - bob.len()` bytes.
    let fits = padded_to(MAX_FILE - bob.len(), &alice);
    let out = edit(|o| add_with(fits.as_bytes(), o, b"bob", b"b", b"0123456789abcdef", IT)).unwrap();
    assert_eq!(out.len(), MAX_FILE);
    let over = padded_to(MAX_FILE - bob.len() + 1, &alice);
    assert_eq!(
        edit(|o| add_with(over.as_bytes(), o, b"bob", b"b", b"0123456789abcdef", IT)),
        Err(Refusal::TooLarge)
    );
}

/// **A longer salt grows the record, and the bound holds for a password change too** — the case
/// the review named: the build's 8-byte salt replaced by a 16-byte one is 16 more hex characters.
#[test]
fn a_new_password_is_bounded_at_exactly_max_file() {
    let old = line("alice", "a", b"12345678"); // an 8-byte salt
    let grown = 16; // 8 more salt bytes, as hex
    let fits = padded_to(MAX_FILE - grown, &old);
    let out = edit(|o| set_password_with(fits.as_bytes(), o, b"alice", b"n", b"0123456789abcdef", IT)).unwrap();
    assert_eq!(out.len(), MAX_FILE);
    let over = padded_to(MAX_FILE - grown + 1, &old);
    assert_eq!(
        edit(|o| set_password_with(over.as_bytes(), o, b"alice", b"n", b"0123456789abcdef", IT)),
        Err(Refusal::TooLarge)
    );
}

/// **An output buffer shorter than the result is refused**, not overrun.
#[test]
fn a_short_buffer_is_refused() {
    let file = line("alice", "a", b"s1");
    let mut out = [0u8; 16];
    assert_eq!(remove(file.as_bytes(), &mut out, b"nobody"), Err(Refusal::NoSuchAccount));
    assert_eq!(add_with(file.as_bytes(), &mut out, b"bob", b"b", b"s2", IT), Err(Refusal::TooLarge));
}
