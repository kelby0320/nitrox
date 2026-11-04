use super::*;
use crate::testing::lookup_only;
use libos::{MapRead, MapReadWrite, Memory, NsMutable, mock};

fn root_with_image() -> Handle<Namespace, NsMutable> {
    mock::reset();
    let root = Handle::<Namespace, NsMutable>::create().unwrap();
    root.bind("/bin/nxsh", &Handle::<Memory, MapReadWrite>::create(4096).unwrap()).unwrap();
    root
}

/// A path resolves to a handle the caller owns, with at most the rights asked.
#[test]
fn a_path_resolves_to_an_owned_handle() {
    let root = root_with_image();
    let image: Handle<Memory, MapRead> = resolve(lookup_only(&root), "/bin/nxsh", Rights::MAP_READ).unwrap();
    assert_eq!(image.extra_rights(), Rights::MAP_READ);
    let missing = resolve::<Memory, MapRead, _>(lookup_only(&root), "/bin/absent", Rights::MAP_READ);
    assert!(missing.err().unwrap().is_not_found());
}

/// The raw lookup answers `(status, handle)` as it always has: a handle to close on success, a
/// negative status and none on a miss or a path no kernel would take.
#[test]
fn the_raw_lookup_answers_a_status_and_a_handle() {
    let root = root_with_image();
    let before = mock::open_handles();
    let (st, h) = ns_lookup(root.raw().0, b"/bin/nxsh", Rights::MAP_READ.bits());
    assert_eq!(st, 0);
    assert!(mock::is_open(h) && mock::open_handles() == before + 1, "the caller's to close");
    assert_eq!(ns_lookup(root.raw().0, b"/bin/absent", Rights::MAP_READ.bits()), (KError::NotFound as i32, 0));
    assert_eq!(ns_lookup(root.raw().0, &[b'/', 0xFF], Rights::MAP_READ.bits()), (KError::InvalidArgument as i32, 0));
    assert_eq!(ns_lookup(0, b"/bin/nxsh", Rights::MAP_READ.bits()).1, 0, "no namespace, no handle");
    assert!(mock::is_open(root.raw().0));
    assert_eq!(mock::stray_closes(), 0);
}

/// The environment names a home and a path, and no user: identity is `/session/user`'s.
#[test]
fn the_environment_has_no_user() {
    let env = session_env();
    assert_eq!(env.values.len(), 3);
    let names: std::vec::Vec<_> = env.schema.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["HOME", "PWD", "PATH"]);
}
