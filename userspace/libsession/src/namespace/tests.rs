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

/// A root namespace with a console bound that a lookup of `READ | TRANSFER` finds.
fn root_with_console() -> Handle<Namespace, NsMutable> {
    let root = Handle::<Namespace, NsMutable>::create().unwrap();
    let console = mock::with_rights(mock::device(), Rights::READ | Rights::TRANSFER);
    // SAFETY: a handle the mock has just made, which nothing else holds.
    let console = unsafe { Handle::<Resource, ReadOnly>::from_raw(RawHandle(console), Rights::empty()) };
    root.bind("/dev/console", &console).unwrap();
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
    ] {
        if tag >= 0 {
            assert_eq!(rights_of(&es, path), 1 << tag, "{path}");
        }
        assert_eq!(mock::bound_base(built.ns.raw().0, path).as_deref(), Some(base.as_bytes()), "{path}'s base");
    }
    assert_eq!(rights_of(&es, "/dev/console"), (Rights::READ | Rights::TRANSFER).bits());
    let want = Bound {
        bin: true,
        tty: true,
        console: true,
        clipboard: true,
        views: true,
        devices: true,
        storage: true,
        desktop: true,
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

/// The raw spec binds what the typed members do, and hands the namespace out unclosed.
#[test]
fn the_raw_build_binds_the_same_and_gives_the_namespace_up() {
    mock::reset();
    let e = eps();
    let root = root_with_console();
    let raw = |h: &Handle<Channel, Only>| h.raw().0;
    let spec = NamespaceSpec {
        root_ns: root.raw().0,
        fs_endpoint: raw(&e.fs),
        profile_endpoint: raw(&e.profile),
        tty_endpoint: raw(&e.tty),
        clipboard_endpoint: raw(&e.clipboard),
        home: b"/home/alice",
        user: b"alice",
        bind_fonts: true,
        bind_console: true,
        views_endpoint: raw(&e.views),
        views_base: b"/s/3",
        devices_endpoint: raw(&e.devices),
        storage_endpoint: raw(&e.storage),
        services_endpoint: raw(&e.services),
        bind_applications: true,
        draw_endpoint: raw(&e.draw),
        desktop_endpoint: raw(&e.desktop),
    };
    let built = build(&spec).unwrap();
    assert!(mock::is_open(built.ns), "the caller's to close");
    let typed = build_namespace(&full(&e, Some(lookup_only(&root)))).unwrap();
    // SAFETY: the namespace `build` made, which the caller owns.
    let ns = unsafe { Handle::<Namespace, NsMutable>::from_raw(RawHandle(built.ns), Rights::empty()) };
    assert_eq!(entries(ns.as_ref()), entries(typed.ns.as_ref()));
    let flags = (built.bin, built.tty, built.console, built.clipboard, built.views, built.devices, built.storage);
    assert_eq!((flags, built.desktop), ((true, true, true, true, true, true, true), true));
    // A member given as `0` is not bound.
    let bare = NamespaceSpec { fs_endpoint: 0, profile_endpoint: 0, home: b"", ..spec };
    let none = build(&NamespaceSpec { root_ns: 0, bind_console: true, ..bare }).unwrap();
    // SAFETY: as above.
    let none = unsafe { Handle::<Namespace, NsMutable>::from_raw(RawHandle(none.ns), Rights::empty()) };
    assert!(!paths(&entries(none.as_ref())).contains(&"/home") && !paths(&entries(none.as_ref())).contains(&"/bin"));
    let bad_home = build(&NamespaceSpec { home: &[0xFF, 0xFE], ..spec });
    assert!(bad_home.is_none(), "a home that is not text cannot be a base");
}
