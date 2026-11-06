//! No type check of an `ObjectRef` is debug-only.

/// **No type check before a cast is debug-only**: a release build skips a `debug_assert`, and the
/// cast after it reads another kind of object as this one.
#[test]
fn no_object_type_check_is_debug_only() {
    fn scan(dir: &std::path::Path, hits: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() && path.file_name().is_some_and(|n| n != "tests") {
                scan(&path, hits);
            } else if path.extension().is_some_and(|e| e == "rs") && path.file_name().is_some_and(|n| n != "tests.rs") {
                for (i, line) in std::fs::read_to_string(&path).unwrap().lines().enumerate() {
                    if line.contains("debug_assert") && line.contains("object_type()") {
                        hits.push(format!("{}:{}", path.display(), i + 1));
                    }
                }
            }
        }
    }
    let mut hits = Vec::new();
    scan(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut hits);
    assert!(hits.is_empty(), "debug-only type checks: {hits:?}");
}
