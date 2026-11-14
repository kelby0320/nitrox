use super::*;
use crate::testing::{endpoint, entries, lookup_only};
use libkern::{KError, RawHandle};
use libos::{MapRead, NsReadOnly, mock};
use std::string::String;
use std::vec::Vec;

/// One tagged endpoint per member, and their peers.
struct Eps {
    draw: Handle<Channel, Only>,
    fs: Handle<Channel, Only>,
    profile: Handle<Channel, Only>,
    tty: Handle<Channel, Only>,
    clipboard: Handle<Channel, Only>,
    views: Handle<Channel, Only>,
    devices: Handle<Channel, Only>,
    storage: Handle<Channel, Only>,
    services: Handle<Channel, Only>,
    desktop: Handle<Channel, Only>,
    _peers: Vec<Handle<Channel, Only>>,
}

fn eps() -> Eps {
    let made: Vec<_> = (40..50).map(endpoint).collect();
    let (mut eps, peers): (Vec<_>, Vec<_>) = made.into_iter().unzip();
    let mut next = || eps.remove(0);
    Eps {
        draw: next(),
        fs: next(),
        profile: next(),
        tty: next(),
        clipboard: next(),
        views: next(),
        devices: next(),
        storage: next(),
        services: next(),
        desktop: next(),
        _peers: peers,
    }
}

/// Every member asked for, `root` as the source of the console.
fn full<'a>(e: &'a Eps, root: Option<Ref<'a, Namespace, NsReadOnly>>) -> Members<'a> {
    Members {
        root_ns: root,
        fs_endpoint: Some(e.fs.as_ref()),
        profile_endpoint: Some(e.profile.as_ref()),
        tty_endpoint: Some(e.tty.as_ref()),
        clipboard_endpoint: Some(e.clipboard.as_ref()),
        home: "/home/alice",
        user: b"alice",
        bind_fonts: true,
        bind_console: true,
        views_endpoint: Some(e.views.as_ref()),
        views_base: "/s/3",
        devices_endpoint: Some(e.devices.as_ref()),
        storage_endpoint: Some(e.storage.as_ref()),
        services_endpoint: Some(e.services.as_ref()),
        bind_applications: true,
        draw_endpoint: Some(e.draw.as_ref()),
        desktop_endpoint: Some(e.desktop.as_ref()),
    }
}

/// Nothing asked for.
fn bare<'a>() -> Members<'a> {
    Members {
        root_ns: None,
        fs_endpoint: None,
        profile_endpoint: None,
        tty_endpoint: None,
        clipboard_endpoint: None,
        home: "",
        user: b"",
        bind_fonts: false,
        bind_console: false,
        views_endpoint: None,
        views_base: "",
        devices_endpoint: None,
        storage_endpoint: None,
        services_endpoint: None,
        bind_applications: false,
        draw_endpoint: None,
        desktop_endpoint: None,
    }
}

/// A root namespace with a console and a generator bound that a lookup of `READ | TRANSFER` finds.
fn root_with_console() -> Handle<Namespace, NsMutable> {
    let root = Handle::<Namespace, NsMutable>::create().unwrap();
    for (path, raw) in [("/dev/console", mock::device()), ("/dev/entropy", mock::entropy_node())] {
        let node = mock::with_rights(raw, Rights::READ | Rights::TRANSFER);
        // SAFETY: a handle the mock has just made, which nothing else holds.
        let node = unsafe { Handle::<Resource, ReadOnly>::from_raw(RawHandle(node), Rights::empty()) };
        root.bind(path, &node).unwrap();
    }
    root
}

fn paths(es: &[(String, u64)]) -> Vec<&str> {
    es.iter().map(|(p, _)| p.as_str()).collect()
}

fn rights_of(es: &[(String, u64)], path: &str) -> u64 {
    es.iter().find(|(p, _)| p == path).unwrap().1
}

/// A session holds exactly these members, each bound to its own endpoint and no other, in this order.
#[test]
fn a_session_binds_each_member_to_its_own_endpoint() {
    mock::reset();
    let e = eps();
    let root = root_with_console();
    let built = build_namespace(&full(&e, Some(lookup_only(&root)))).unwrap();
    let es = entries(built.ns.as_ref());
    assert_eq!(
        paths(&es),
        [
            "/dev/draw/new",
            "/home",
            "/bin",
            "/applications",
            "/session/user",
            "/dev/tty",
            "/dev/clipboard",
            "/dev/views",
            "/dev/devices",
            "/storage",
            "/dev/storage",
            "/dev/services",
            "/system/fonts",
            "/dev/console",
            "/dev/entropy",
            "/dev/desktop",
        ]
    );
    for (path, tag, base) in [
        ("/dev/draw/new", 40, "/new"),
        ("/home", 41, "/home/alice"),
        ("/system/fonts", 41, "/system/fonts"),
        ("/bin", 42, "/bin"),
        ("/applications", 42, "/applications"),
        ("/dev/tty", 43, ""),
        ("/dev/clipboard", 44, ""),
        ("/dev/views", 45, "/s/3"),
        ("/dev/devices", 46, "/info"),
        ("/storage", 47, "/fs"),
        ("/dev/storage", 47, "/info"),
        ("/dev/services", 48, ""),
        ("/dev/desktop", 49, ""),
        ("/dev/console", -1, ""),
        ("/dev/entropy", -1, ""),
    ] {
        if tag >= 0 {
            assert_eq!(rights_of(&es, path), 1 << tag, "{path}");
        }
        assert_eq!(mock::bound_base(built.ns.raw().0, path).as_deref(), Some(base.as_bytes()), "{path}'s base");
    }
    assert_eq!(rights_of(&es, "/dev/console"), (Rights::READ | Rights::TRANSFER).bits());
    assert_eq!(rights_of(&es, "/dev/entropy"), (Rights::READ | Rights::TRANSFER).bits());
    let want = Bound {
        bin: true,
        tty: true,
        console: true,
        clipboard: true,
        views: true,
        devices: true,
        storage: true,
        desktop: true,
        entropy: true,
    };
    assert_eq!(built.bound, want);
}

/// A member not given, or given as nothing, binds nothing: authority held is authority.
#[test]
fn a_member_not_asked_for_is_not_bound() {
    mock::reset();
    let e = eps();
    let root = root_with_console();
    let only_home = Members { home: "/home/alice", fs_endpoint: Some(e.fs.as_ref()), ..bare() };
    let built = build_namespace(&only_home).unwrap();
    assert_eq!(paths(&entries(built.ns.as_ref())), ["/home"]);
    assert_eq!(built.bound, Bound::default());
    let none = build_namespace(&Members { bind_fonts: true, bind_applications: true, ..bare() }).unwrap();
    assert!(entries(none.ns.as_ref()).is_empty(), "fonts need the fs endpoint, applications the profile's");
    let no_console = Members { bind_console: false, ..full(&e, Some(lookup_only(&root))) };
    let built = build_namespace(&no_console).unwrap();
    assert!(!paths(&entries(built.ns.as_ref())).contains(&"/dev/console") && !built.bound.console);
    let no_source = Members { root_ns: None, ..full(&e, None) };
    assert!(!build_namespace(&no_source).unwrap().bound.console);
    let empty_root = Handle::<Namespace, NsMutable>::create().unwrap();
    let no_device = build_namespace(&full(&e, Some(lookup_only(&empty_root)))).unwrap();
    assert!(!no_device.bound.console, "a root with no console binds none");
    assert!(!no_device.bound.entropy, "nor a generator it has none of");
    assert!(build_namespace(&no_console).unwrap().bound.entropy, "a column with no console has one");
    let no_base = Members { views_base: "", ..full(&e, None) };
    assert!(!paths(&entries(build_namespace(&no_base).unwrap().ns.as_ref())).contains(&"/dev/views"));
    let no_user = Members { user: b"", ..full(&e, None) };
    assert!(!paths(&entries(build_namespace(&no_user).unwrap().ns.as_ref())).contains(&"/session/user"));
}

/// Without the draw endpoint's bind, a home's, or the namespace itself, there is no session; and
/// the namespace made is closed.
#[test]
fn a_required_member_that_will_not_bind_fails_the_build() {
    mock::reset();
    let e = eps();
    let before = mock::open_handles();
    mock::fail_next_bind(KError::NoAccess as i32);
    assert!(build_namespace(&Members { draw_endpoint: Some(e.draw.as_ref()), ..bare() }).is_none());
    assert!(mock::logged().ends_with(b"libsession: /dev/draw/new bind FAIL\n"));
    assert_eq!(mock::open_handles(), before, "the namespace is closed");
    mock::fail_next_bind(KError::NoAccess as i32);
    let home = Members { home: "/home/alice", fs_endpoint: Some(e.fs.as_ref()), ..bare() };
    assert!(build_namespace(&home).is_none());
    assert!(mock::logged().ends_with(b"libsession: /home subtree bind FAIL\n"));
    assert_eq!(mock::open_handles(), before);
    let no_fs = Members { home: "/home/alice", ..bare() };
    assert!(build_namespace(&no_fs).is_none(), "a home with no fs endpoint to serve it");
    assert_eq!(mock::open_handles(), before);
    // A home the kernel will not take as a base: the bind is refused.
    assert!(build_namespace(&Members { home: "home", ..home }).is_none());
}

/// An optional member that will not bind is reported and left out; the rest still bind.
#[test]
fn an_optional_member_that_will_not_bind_is_skipped() {
    mock::reset();
    let e = eps();
    mock::fail_next_bind(KError::NoAccess as i32);
    let only_profile = Members { profile_endpoint: Some(e.profile.as_ref()), bind_applications: true, ..bare() };
    let built = build_namespace(&only_profile).unwrap();
    assert_eq!(paths(&entries(built.ns.as_ref())), ["/applications"]);
    assert!(!built.bound.bin);
    assert!(mock::logged().ends_with(b"libsession: /bin bind FAIL (session has no programs)\n"));
    mock::fail_next_bind(KError::NoAccess as i32);
    let tty = Members { tty_endpoint: Some(e.tty.as_ref()), desktop_endpoint: Some(e.desktop.as_ref()), ..bare() };
    let built = build_namespace(&tty).unwrap();
    assert_eq!((built.bound.tty, built.bound.desktop), (false, true));
    mock::fail_next_bind(KError::NoAccess as i32);
    let storage = Members { storage_endpoint: Some(e.storage.as_ref()), ..bare() };
    let built = build_namespace(&storage).unwrap();
    assert!(!built.bound.storage && entries(built.ns.as_ref()).is_empty(), "both or neither is reported");
}

/// The user is published at `/session/user` as a NUL-ended name, cut at 255 bytes.
#[test]
fn the_user_is_published_nul_ended() {
    mock::reset();
    let name = |user: &[u8]| {
        let built = build_namespace(&Members { user, ..bare() }).unwrap();
        let mem: Handle<Memory, MapRead> =
            crate::resolve(built.ns.as_ref(), "/session/user", Rights::MAP_READ | Rights::INSPECT).unwrap();
        let mut page = [0xAAu8; 300];
        mem.map(4096).unwrap().read_into(0, &mut page).unwrap();
        page
    };
    let alice = name(b"alice");
    assert_eq!((&alice[..5], alice[5], alice[299]), (&b"alice"[..], 0, 0));
    let long = name(&[b'a'; 300]);
    assert_eq!((long[254], long[255], long[256]), (b'a', 0, 0), "255 bytes and the terminator");
}

/// **No process in the session can rewrite who it is for**: `/session/user` resolves for reading
/// and stating, and not for writing.
#[test]
fn the_published_user_cannot_be_rewritten() {
    mock::reset();
    let built = build_namespace(&Members { user: b"alice", ..bare() }).unwrap();
    let rw: Handle<Memory, MapReadWrite> =
        crate::resolve(built.ns.as_ref(), "/session/user", Rights::MAP_READ | Rights::MAP_WRITE).unwrap();
    assert_eq!(rw.map(4096).err().map(|e| e.code()), Some(KError::NoAccess));
    let ro: Handle<Memory, MapRead> =
        crate::resolve(built.ns.as_ref(), "/session/user", Rights::MAP_READ | Rights::INSPECT).unwrap();
    assert_eq!(ro.stat().map(|s| s.size).ok(), Some(4096), "a reader sizes it");
}

/// Everything the build opened on the way is closed, and the namespace is the caller's alone.
#[test]
fn a_build_leaves_only_its_namespace_open() {
    mock::reset();
    let e = eps();
    let root = root_with_console();
    let before = mock::open_handles();
    let built = build_namespace(&full(&e, Some(lookup_only(&root)))).unwrap();
    assert_eq!(mock::open_handles(), before + 1, "the namespace: the console and the name are closed");
    drop(built);
    assert_eq!(mock::open_handles(), before);
    assert_eq!(mock::stray_closes(), 0);
}
