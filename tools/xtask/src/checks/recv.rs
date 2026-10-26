//! `check-recv`: userspace receives a message only through `libkern`'s `channel_recv`, whose
//! `Received` closes every handle the message brought and no one took.

use crate::*;

/// The files that may name the raw receive: where it is defined, and the one place that makes it.
const ALLOWED: &[&str] = &["userspace/libkern/src/syscall.rs", "userspace/libkern/src/channel.rs"];

/// Each line of `text`, the tracked file `path`, that names `SYS_CHANNEL_RECV` in code.
fn violations(path: &str, text: &str) -> Vec<String> {
    if ALLOWED.contains(&path) {
        return Vec::new();
    }
    text.lines()
        .enumerate()
        .filter(|(_, line)| line.split("//").next().unwrap_or("").contains("SYS_CHANNEL_RECV"))
        .map(|(i, line)| format!("{path}:{}: {}", i + 1, line.trim()))
        .collect()
}

/// Fails on any tracked userspace Rust file but `libkern`'s own that names `SYS_CHANNEL_RECV`: a
/// raw receive leaves the handles a message brought in the receiver's table.
pub(crate) fn cmd_check_recv() -> R<()> {
    let root = repo_root();
    let out = Command::new("git").args(["ls-files", "--", "userspace/*.rs"]).current_dir(&root).output()?;
    if !out.status.success() {
        return Err(format!("check-recv: git ls-files: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    let mut found = Vec::new();
    for path in String::from_utf8_lossy(&out.stdout).lines() {
        found.extend(violations(path, &fs::read_to_string(root.join(path))?));
    }
    if found.is_empty() {
        println!("check-recv: no raw SYS_CHANNEL_RECV outside libkern's channel_recv ✓");
        return Ok(());
    }
    for f in &found {
        eprintln!("  {f}");
    }
    Err(format!(
        "check-recv: {} raw receive(s); receive through libkern::channel_recv or channel_recv_raw, whose \
         Received closes what the message brought",
        found.len()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_raw_receive_in_code_is_found_and_one_named_in_a_comment_is_not() {
        let text = "/// Never a raw `SYS_CHANNEL_RECV`.\nlet r = syscall4(SYS_CHANNEL_RECV, ch, m, h, c); // receive\n\
                    // SYS_CHANNEL_RECV once\nuse libkern::{SYS_CHANNEL_RECV, SYS_WAIT};\n";
        let v = violations("userspace/x/src/main.rs", text);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].starts_with("userspace/x/src/main.rs:2: ") && v[1].starts_with("userspace/x/src/main.rs:4: "));
    }

    #[test]
    fn libkern_may_define_and_make_the_receive() {
        for path in ALLOWED {
            assert!(violations(path, "pub const SYS_CHANNEL_RECV: u64 = 14;\n").is_empty());
        }
        assert_eq!(violations("userspace/libkern/src/control.rs", "syscall4(SYS_CHANNEL_RECV, 0, 0, 0, 0)").len(), 1);
    }
}
