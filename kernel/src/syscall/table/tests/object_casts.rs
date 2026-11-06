//! The type checks before each cast of an `ObjectRef`: each one present, and none debug-only.

use super::*;

/// **Every cast of an `ObjectRef` stops at its type check** when handed another kind of object,
/// and [`no_object_type_check_is_debug_only`] keeps each check in a release build.
#[test]
fn every_cast_of_an_object_ref_stops_another_kind_at_its_type_check() {
    use crate::mm::PAGE_SIZE;
    use crate::object::FileObject;
    use crate::object::file_object::Producer;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    init_global_heap();
    let file = ObjectRef::new(FileObject::try_new(PAGE_SIZE, Producer::Stub { base: 0 }).unwrap());
    type Site<'a> = (&'static str, Box<dyn FnOnce() + 'a>);
    let sites: Vec<Site<'_>> = vec![(
        "io_buffer_size",
        Box::new(|| {
            let _ = submit::io_buffer_size(&file);
        }),
    )];
    let missed: Vec<&str> = sites
        .into_iter()
        .filter_map(|(name, site)| {
            let stopped = catch_unwind(AssertUnwindSafe(site));
            let at_check = stopped.is_err_and(|e| e.downcast_ref::<String>().is_some_and(|m| m.contains(name)));
            (!at_check).then_some(name)
        })
        .collect();
    assert!(missed.is_empty(), "no type check stopped: {missed:?}");
}

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
