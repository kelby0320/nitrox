use std::io::Write;
use std::process::{Command, Stdio};

fn run_hook(input: &str) -> (bool, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("hook-comments")
        .env_remove("CARGO_MANIFEST_DIR")
        .current_dir(std::env::temp_dir())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    let out = child.wait_with_output().unwrap();
    (out.status.success(), String::from_utf8(out.stdout).unwrap())
}

#[test]
fn the_built_binary_answers_an_edit_as_the_hook_runs_it() {
    let edit = r#"{"tool_name":"Edit","tool_input":{"file_path":"/r/a.rs","old_string":"","new_string":"// one\n// two\n// three\nlet x = 1;"}}"#;
    let (ok, out) = run_hook(edit);
    assert!(ok);
    assert!(out.contains("\"additionalContext\":\"This edit adds 3 comment lines"), "{out}");
}

#[test]
fn the_built_binary_is_silent_and_succeeds_on_anything_else() {
    for input in ["", "not json", r#"{"tool_name":"Read","tool_input":{"file_path":"/r/a.rs"}}"#] {
        assert_eq!(run_hook(input), (true, String::new()), "{input:?}");
    }
}
