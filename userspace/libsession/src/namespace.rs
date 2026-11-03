//! The session namespace: which members it binds, and with what.

use crate::{borrow, resolve};
use libkern::Rights;
use libos::{
    Channel, Handle, MapReadWrite, Mapping, Memory, Namespace, NsMutable, NsReadOnly, Only, ReadOnly, Ref, Resource,
    log,
};

/// What a session's namespace — **or an application's** — is built from: both login supervisors
/// and `desktop-shell` (per application) build with this, so they cannot drift.
pub struct NamespaceSpec<'a> {
    /// The namespace direct handles are resolved from: the supervisor's own — the root namespace —
    /// for a session, and **the session's** for an application.
    pub root_ns: u64,
    /// The fs-server endpoint, bound at `/home` scoped to the user's subtree.
    pub fs_endpoint: u64,
    /// The profile-server endpoint, bound whole-tree at `/bin`.
    pub profile_endpoint: u64,
    /// The tty server's, bound at `/dev/tty`. **This and every server endpoint below is
    /// `service-mgr`'s route to the server**, not its own, so a session survives a restart. Each
    /// route reaches one server and nothing else, which is what lets `desktop-shell` hold them.
    pub tty_endpoint: u64,
    /// The clipboard server's, bound at `/dev/clipboard` in **both columns**, as a pipeline uses it
    /// as a path. `0` binds nothing.
    pub clipboard_endpoint: u64,
    /// The user's home directory, as the subtree base for `/home`.
    pub home: &'a [u8],
    /// The user's name, snapshotted at `/session/user`.
    pub user: &'a [u8],
    /// Bind `/system/fonts` read-only; **a flag** because a member bound for nothing is still
    /// authority held (`session-mgr/CLAUDE.md`).
    pub bind_fonts: bool,
    /// Bind `/dev/console`. **False for a graphical session** (`graphical-session.md`, governing
    /// decision 3); the serial column binds it as its terminal.
    pub bind_console: bool,
    /// The view broker's, bound at `/dev/views` with [`views_base`](Self::views_base) as its base.
    /// `0` binds nothing. **The base is the session's identity**: the broker learns whose request
    /// it is from the suffix, and no program here can choose another — except `desktop-shell`,
    /// handed this endpoint to bind into applications.
    pub views_endpoint: u64,
    /// `/s/<session>`, from [`open_views_session`](crate::open_views_session). Empty binds nothing.
    pub views_base: &'a [u8],
    /// An **info-only** endpoint of the device manager's, bound at `/dev/devices` with the base
    /// `/info`. `0` binds nothing. **The endpoint keeps a session from taking a device**: the
    /// manager answers only its tables on it, whatever suffix arrives.
    pub devices_endpoint: u64,
    /// A **session endpoint** of the storage service's, bound at `/storage` (base `/fs`) and
    /// `/dev/storage` (base `/info`). `0` binds nothing. **The endpoint keeps a session from
    /// mounting**: `admin-endpoint` is `NotFound` there, so even `desktop-shell` cannot mint one.
    pub storage_endpoint: u64,
    /// `service-mgr`'s **session endpoint**, bound at `/dev/services`; it answers only the table.
    /// `0` binds nothing.
    pub services_endpoint: u64,
    /// Bind `/applications`: **true for a session**, **false for an application**.
    pub bind_applications: bool,
    /// The compositor's forwarding endpoint, bound at **`/dev/draw/new` alone**, with the base
    /// `/new`: an application may open windows, and `manage` is unreachable. `0` binds nothing
    /// (the graphical supervisor binds `/dev/draw` whole for `desktop-shell`). **Required when
    /// asked for**: the build fails if it does not bind.
    pub draw_endpoint: u64,
    /// `desktop-shell`'s own endpoint, bound at `/dev/desktop`. `0` binds nothing.
    pub desktop_endpoint: u64,
}

/// **What a namespace built from a [`NamespaceSpec`] holds**: it, and which optional members bound.
#[derive(Clone, Copy, Debug, Default)]
pub struct Built {
    /// The namespace handle.
    pub ns: u64,
    /// `/bin` bound.
    pub bin: bool,
    /// `/dev/tty` bound.
    pub tty: bool,
    /// `/dev/console` bound.
    pub console: bool,
    /// `/dev/clipboard` bound.
    pub clipboard: bool,
    /// `/dev/views` bound, at the session's base.
    pub views: bool,
    /// `/dev/devices` bound.
    pub devices: bool,
    /// `/storage` and `/dev/storage` both bound.
    pub storage: bool,
    /// `/dev/desktop` bound.
    pub desktop: bool,
}

/// [`NamespaceSpec`] with its endpoints borrowed: `None` binds nothing. Each is as that field of
/// the spec says.
#[derive(Clone, Copy)]
pub struct Members<'a> {
    /// The namespace direct handles are resolved from; `None` binds no console.
    pub root_ns: Option<Ref<'a, Namespace, NsReadOnly>>,
    /// The fs-server endpoint.
    pub fs_endpoint: Option<Ref<'a, Channel, Only>>,
    /// The profile-server endpoint.
    pub profile_endpoint: Option<Ref<'a, Channel, Only>>,
    /// The tty server's.
    pub tty_endpoint: Option<Ref<'a, Channel, Only>>,
    /// The clipboard server's.
    pub clipboard_endpoint: Option<Ref<'a, Channel, Only>>,
    /// The user's home directory, as the subtree base for `/home`.
    pub home: &'a str,
    /// The user's name, snapshotted at `/session/user`.
    pub user: &'a [u8],
    /// Bind `/system/fonts` read-only.
    pub bind_fonts: bool,
    /// Bind `/dev/console`.
    pub bind_console: bool,
    /// The view broker's.
    pub views_endpoint: Option<Ref<'a, Channel, Only>>,
    /// `/s/<session>`. Empty binds nothing.
    pub views_base: &'a str,
    /// The device manager's info-only endpoint.
    pub devices_endpoint: Option<Ref<'a, Channel, Only>>,
    /// The storage service's session endpoint.
    pub storage_endpoint: Option<Ref<'a, Channel, Only>>,
    /// `service-mgr`'s session endpoint.
    pub services_endpoint: Option<Ref<'a, Channel, Only>>,
    /// Bind `/applications`.
    pub bind_applications: bool,
    /// The compositor's forwarding endpoint, required when given.
    pub draw_endpoint: Option<Ref<'a, Channel, Only>>,
    /// `desktop-shell`'s own endpoint.
    pub desktop_endpoint: Option<Ref<'a, Channel, Only>>,
}

/// Which optional members a namespace built from [`Members`] bound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Bound {
    /// `/bin` bound.
    pub bin: bool,
    /// `/dev/tty` bound.
    pub tty: bool,
    /// `/dev/console` bound.
    pub console: bool,
    /// `/dev/clipboard` bound.
    pub clipboard: bool,
    /// `/dev/views` bound, at the session's base.
    pub views: bool,
    /// `/dev/devices` bound.
    pub devices: bool,
    /// `/storage` and `/dev/storage` both bound.
    pub storage: bool,
    /// `/dev/desktop` bound.
    pub desktop: bool,
}

/// A namespace built from [`Members`], owned and closed on drop, and what bound into it.
pub struct Constructed {
    /// The namespace.
    pub ns: Handle<Namespace, NsMutable>,
    /// Which optional members bound.
    pub bound: Bound,
}

/// **Build a namespace from `members`** and say what bound. `None` if a member it cannot do without
/// did not: the namespace, `/home` when there is a home, or `/dev/draw/new` when asked for. Every
/// other member is reported and skipped on failure; a session without it still runs.
pub fn build_namespace(members: &Members<'_>) -> Option<Constructed> {
    let Members {
        root_ns,
        fs_endpoint,
        profile_endpoint,
        tty_endpoint,
        clipboard_endpoint,
        home,
        user,
        bind_fonts,
        bind_console,
        views_endpoint,
        views_base,
        devices_endpoint,
        storage_endpoint,
        services_endpoint,
        bind_applications,
        draw_endpoint,
        desktop_endpoint,
    } = *members;
    let Ok(ns) = Handle::<Namespace, NsMutable>::create() else {
        log::write(b"libsession: ns_create FAIL\n");
        return None;
    };
    let give_up = |why: &[u8]| {
        log::write(why);
        None
    };
    let mut bound = Bound::default();
    // `/dev/draw/new` → the compositor, **narrowly**: the base `/new` keeps `manage` unreachable;
    // `desktop-shell` checks that before it launches (`verify_app_namespace`).
    if let Some(draw) = draw_endpoint
        && ns.bind_server("/dev/draw/new", draw, "/new").is_err()
    {
        return give_up(b"libsession: /dev/draw/new bind FAIL\n");
    }
    // `/home` → the fs-server scoped to the user's subtree: the kernel prepends `home` to every
    // forwarded suffix. **Required when there is a home**: `PWD` starts at `/home`.
    if !home.is_empty() && !fs_endpoint.is_some_and(|fs| ns.bind_server("/home", fs, home).is_ok()) {
        return give_up(b"libsession: /home subtree bind FAIL\n");
    }
    // `/bin` → the profile server. **Not `/initramfs/sbin`**: that hands every session every boot
    // binary, the ambient authority namespaces refuse; a profile is a *choice* of what a user may
    // run. Non-fatal: a session with no `/bin` still has the in-process language.
    if let Some(profile) = profile_endpoint {
        // **Scoped**: the server tells package directories apart by the suffix's first component.
        bound.bin = ns.bind_server("/bin", profile, "/bin").is_ok();
        if !bound.bin {
            log::write(b"libsession: /bin bind FAIL (session has no programs)\n");
        }
        // `/applications` → the same server, projecting each package's `applications/`. Non-fatal.
        // **A session's alone**: an application launches nothing from a menu.
        // **Absolute**: `SubtreeBase::from_path` rejects a bare component; `profile-server`'s
        // `split_suffix` strips the leading slash.
        if bind_applications && ns.bind_server("/applications", profile, "/applications").is_err() {
            log::write(b"libsession: /applications bind FAIL (the modal will be empty)\n");
        }
    }

    // `/session/user` → who this session belongs to; the kernel has no user identity. A memory
    // object is a snapshot, right while a session's user is immutable: the first mutable
    // `/session/*` member is the trigger for `TODO(session-metadata-server)`. **An application's
    // too**: `desktop-shell` passes the name on. An empty name binds nothing.
    if !user.is_empty() {
        bind_session_user(&ns, user);
    }

    // `/dev/tty` → the terminal server's **forwarding** endpoint, so each resolve mints the
    // program its *own* terminal. A minted channel bound instead would answer `Namespace::Resolve`
    // with `Unsupported`: the kernel adopts *any* bound channel as a server.
    if let Some(tty) = tty_endpoint {
        bound.tty = ns.bind_server("/dev/tty", tty, "").is_ok();
        if !bound.tty {
            log::write(b"libsession: /dev/tty bind FAIL\n");
        }
    }

    // `/dev/clipboard` → the clipboard server. **This binding is the authority**: there is no
    // ambient clipboard, and an endpoint attenuated to `RIGHT_SEND` would copy but not read.
    // The forwarding endpoint, for `/dev/tty`'s reason.
    if let Some(clipboard) = clipboard_endpoint {
        bound.clipboard = ns.bind_server("/dev/clipboard", clipboard, "").is_ok();
        if !bound.clipboard {
            log::write(b"libsession: /dev/clipboard bind FAIL (no copy and paste)\n");
        }
    }

    // `/dev/views` → the view broker **at this session's base**, so a resolve reaches it as
    // `s/<session>`. Non-fatal.
    if let Some(views) = views_endpoint
        && !views_base.is_empty()
    {
        bound.views = ns.bind_server("/dev/views", views, views_base).is_ok();
        if !bound.views {
            log::write(b"libsession: /dev/views bind FAIL (no `with` in this session)\n");
        }
    }

    // `/dev/devices` → the device manager's info-only endpoint **at the base `/info`**, the
    // directory of tables. Non-fatal.
    if let Some(devices) = devices_endpoint {
        bound.devices = ns.bind_server("/dev/devices", devices, "/info").is_ok();
        if !bound.devices {
            log::write(b"libsession: /dev/devices bind FAIL (no device listing in this session)\n");
        }
    }

    // `/storage` (base `/fs`) and `/dev/storage` (base `/info`), both from the one endpoint.
    // Non-fatal.
    if let Some(storage) = storage_endpoint {
        bound.storage = [("/storage", "/fs"), ("/dev/storage", "/info")]
            .iter()
            .all(|&(at, base)| ns.bind_server(at, storage, base).is_ok());
        if !bound.storage {
            log::write(b"libsession: /storage bind FAIL (no filesystems in this session)\n");
        }
    }

    // `/dev/services` → `service-mgr`'s session endpoint, with no base. Non-fatal.
    if let Some(services) = services_endpoint
        && ns.bind_server("/dev/services", services, "").is_err()
    {
        log::write(b"libsession: /dev/services bind FAIL (no service list in this session)\n");
    }

    // `/system/fonts` → the fs-server scoped to that subtree. Read-only: no writable handle.
    if bind_fonts
        && let Some(fs) = fs_endpoint
        && ns.bind_server("/system/fonts", fs, "/system/fonts").is_err()
    {
        log::write(b"libsession: /system/fonts bind FAIL (clients cannot render text)\n");
    }

    // `/dev/console` → a direct-handle bind, resolved from our own namespace. **Skipped for a
    // graphical session** (see [`NamespaceSpec::bind_console`]): the console is shared.
    if bind_console
        && let Some(root) = root_ns
        && let Ok(console) = resolve::<Resource, ReadOnly, _>(root, "/dev/console", Rights::READ | Rights::TRANSFER)
    {
        bound.console = ns.bind("/dev/console", &console).is_ok();
        if !bound.console {
            log::write(b"libsession: /dev/console bind FAIL (shell has no console)\n");
        }
    }
    // `/dev/desktop` → `desktop-shell`'s own endpoint, for an application. No base. Non-fatal.
    if let Some(desktop) = desktop_endpoint {
        bound.desktop = ns.bind_server("/dev/desktop", desktop, "").is_ok();
        if !bound.desktop {
            log::write(b"libsession: /dev/desktop bind FAIL\n");
        }
    }
    Some(Constructed { ns, bound })
}

/// [`build_namespace`] from a [`NamespaceSpec`] of raw handles the caller keeps open; the namespace
/// is the caller's to close. For callers not yet on libos; the typed form is [`build_namespace`].
// TODO(raw-handle-shims): goes when its last caller is on libos.
pub fn build(spec: &NamespaceSpec<'_>) -> Option<Built> {
    fn text(bytes: &[u8]) -> Option<&str> {
        core::str::from_utf8(bytes).ok()
    }
    let members = Members {
        root_ns: borrow(spec.root_ns),
        fs_endpoint: borrow(spec.fs_endpoint),
        profile_endpoint: borrow(spec.profile_endpoint),
        tty_endpoint: borrow(spec.tty_endpoint),
        clipboard_endpoint: borrow(spec.clipboard_endpoint),
        // A home that is not text cannot be a base, and a session with a home needs its bind.
        home: text(spec.home)?,
        user: spec.user,
        bind_fonts: spec.bind_fonts,
        bind_console: spec.bind_console,
        views_endpoint: borrow(spec.views_endpoint),
        views_base: text(spec.views_base).unwrap_or(""),
        devices_endpoint: borrow(spec.devices_endpoint),
        storage_endpoint: borrow(spec.storage_endpoint),
        services_endpoint: borrow(spec.services_endpoint),
        bind_applications: spec.bind_applications,
        draw_endpoint: borrow(spec.draw_endpoint),
        desktop_endpoint: borrow(spec.desktop_endpoint),
    };
    let Constructed { ns, bound } = build_namespace(&members)?;
    Some(Built {
        ns: ns.into_raw().0,
        bin: bound.bin,
        tty: bound.tty,
        console: bound.console,
        clipboard: bound.clipboard,
        views: bound.views,
        devices: bound.devices,
        storage: bound.storage,
        desktop: bound.desktop,
    })
}

/// Publish `user` at `/session/user` in `ns`, as a memory object. Non-fatal.
fn bind_session_user(ns: &Handle<Namespace, NsMutable>, user: &[u8]) {
    let Ok(mem) = Handle::<Memory, MapReadWrite>::create(4096) else {
        log::write(b"libsession: /session/user create FAIL\n");
        return;
    };
    // The trailing NUL is the terminator a reader stops at.
    let n = user.len().min(255);
    let mut name = [0u8; 256];
    name[..n].copy_from_slice(&user[..n]);
    let written = mem.map(4096).and_then(|mut map: Mapping<MapReadWrite>| map.write_from(0, &name[..=n]));
    if written.is_err() {
        log::write(b"libsession: /session/user map FAIL\n");
        return;
    }
    if ns.bind("/session/user", &mem).is_err() {
        log::write(b"libsession: /session/user bind FAIL (whoami will report no identity)\n");
    }
}

#[cfg(test)]
mod tests;
