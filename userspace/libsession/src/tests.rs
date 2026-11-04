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

/// The environment names a home and a path, and no user: identity is `/session/user`'s.
#[test]
fn the_environment_has_no_user() {
    let env = session_env();
    assert_eq!(env.values.len(), 3);
    let names: std::vec::Vec<_> = env.schema.fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["HOME", "PWD", "PATH"]);
}
