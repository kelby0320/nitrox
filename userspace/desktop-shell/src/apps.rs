//! Launching applications, each in a namespace built and checked for it.

use super::*;

/// Read the desktop entries `/applications` projects; an entry that will not parse is skipped and
/// said so, losing only its own application.
pub(super) fn read_applications(ns: Root<'_>) -> alloc::vec::Vec<Application> {
    use librsproto::session::Dir;
    let mut names = alloc::vec::Vec::new();
    let mut msg = Msg::new();
    let Ok(mut dir) = Dir::resolve(ns, "/applications", &mut msg) else {
        log::write(b"desktop-shell: /applications did not open; the Applications menu will be empty\n");
        return alloc::vec::Vec::new();
    };
    let _ = dir.read_dir(|e| {
        if e.name != b"." && e.name != b".." {
            names.push(alloc::string::String::from_utf8_lossy(e.name).into_owned());
        }
        true
    });
    dir.close();
    names.sort();

    let mut apps = alloc::vec::Vec::new();
    for file in &names {
        let mut path = alloc::string::String::from("/applications/");
        path.push_str(file);
        let Ok(bytes) = libfs::read(ns, &path) else {
            Line::new().s(b"desktop-shell: application entry unreadable: ").untrusted(file.as_bytes()).end();
            continue;
        };
        match core::str::from_utf8(&bytes).ok().and_then(parse_entry) {
            Some(a) => apps.push(a),
            None => {
                Line::new().s(b"desktop-shell: application entry malformed: ").untrusted(file.as_bytes()).end();
            }
        }
    }
    apps.sort_by(|a, b| a.name.cmp(&b.name));
    apps
}

/// Construct the namespace one application runs in: [`libsession::build_namespace`] from the same
/// members the login supervisors use.
///
/// The load-bearing part of the shell and the reason it holds `BIND_NAMESPACE`:
/// `ui-composition-model.md` §5a rests "an application cannot compose other applications" on this
/// process building their namespaces, so authority is what the shell binds, not what an
/// application asks for.
///
/// `/dev/draw/new` is bound as its own path, with subtree base `/new`: resolving it is an exact
/// match, so the compositor mints a session, while `/dev/draw/manage` is not a component-boundary
/// prefix match against that binding (`match_suffix_offset`), so nothing in the namespace answers
/// it. A narrow bind cannot express "the subtree minus `manage`": the first application that
/// needs `/dev/draw/<id>/info` for unknown ids forces a subtree bind, and `manage` comes back
/// with it. A second forwarding endpoint is the fallback, and that is its trigger.
///
/// What an application gets, and why:
/// - `/dev/draw/new`, narrowly.
/// - `/home`, the user's subtree: `HOME` and `PWD` name it, and without it every relative path
///   fails.
/// - `/bin`, scoped, so a terminal can host a shell but `/bin/applications` does not reach the
///   applications projection.
/// - No `/applications`: an application holds no authority to spawn from a menu (`Desktop::Open`
///   exists for that).
/// - `/dev/tty`, minting a fresh terminal per resolve; `/system/fonts`, read-only.
/// - `/dev/desktop` and `/dev/clipboard`, granted deliberately: every application in the session
///   can create, switch and name desktops and read what anything else copied. Bound here, not
///   into the session.
/// - `/dev/views` at the session's base, so `with` reaches the broker as this session; an
///   application cannot choose another base.
/// - `/dev/devices`, `/storage`, `/dev/storage`, `/dev/services`: info-only, none reaching a
///   class, a mount or a start. `/session/user`: who the session is for.
/// - No disks: a program reaches one only through the broker's `disks` grant, as `with admin
///   nxinstall` does.
///
/// `None` if the namespace could not be built with what it cannot do without.
pub(super) fn build_app_namespace(l: &Launcher<'_>) -> Option<libsession::Constructed> {
    let built = libsession::build_namespace(&libsession::Members {
        root_ns: Some(l.session_ns),
        fs_endpoint: l.fs,
        profile_endpoint: l.profile,
        tty_endpoint: l.tty,
        clipboard_endpoint: l.clipboard,
        home: l.home,
        user: l.user.as_bytes(),
        bind_fonts: true,
        bind_console: false,
        views_endpoint: l.views,
        views_base: l.views_base,
        devices_endpoint: l.devices,
        storage_endpoint: l.storage,
        services_endpoint: l.services,
        bind_applications: false,
        draw_endpoint: l.draw,
        desktop_endpoint: l.desktop,
    })?;
    if built.bound.desktop {
        log::write(b"desktop-shell: application /dev/desktop bound\n");
    }
    Some(built)
}

/// The session's user, read from `/session/user` as `whoami` does; empty binds none.
pub(super) fn session_user(session_ns: Root<'_>) -> alloc::string::String {
    let Ok(bytes) = libfs::read(session_ns, "/session/user") else {
        log::write(b"desktop-shell: the session has no /session/user\n");
        return alloc::string::String::new();
    };
    let end = bytes.iter().position(|&b| b == 0 || b == b'\n').unwrap_or(bytes.len());
    alloc::string::String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Check the application namespace grants `new` and withholds `manage`, before anything runs in
/// it, by the process that built it: an application that can reach `manage` never says so.
/// `false` means the caller declines to launch.
pub(super) fn verify_app_namespace(
    ns: &Handle<Namespace, NsMutable>,
    expect_home: bool,
    expect_desktop: bool,
    expect_devices: bool,
    expect_storage: bool,
    expect_user: bool,
) -> bool {
    let session = Rights::SEND | Rights::RECV | Rights::WAIT;
    let new_st = status_of(ns, "/dev/draw/new", session);
    let manage_st = status_of(ns, "/dev/draw/manage", session);
    if new_st != 0 {
        log::write(b"desktop-shell: application namespace cannot reach /dev/draw/new\n");
        return false;
    }
    // A refusal is not an absence. Once this shell holds the manager channel, the compositor
    // answers a second `manage` resolve with `WouldBlock` whether or not the path is bound, so a
    // whole-subtree bind looked like `new` alone. `WouldBlock` means the resolve reached the
    // compositor; only `NotFound` passes.
    if manage_st != KError::NotFound.as_i32() {
        Line::new()
            .s(b"desktop-shell: application namespace can REACH /dev/draw/manage (status ")
            .i(manage_st as i64)
            .s(b") -- refusing")
            .end();
        return false;
    }
    // `/home` too: nothing downstream would report an unresolvable `PWD`.
    if expect_home {
        let home_st = status_of(ns, "/home", session);
        if home_st != 0 {
            Line::new()
                .s(b"desktop-shell: application namespace cannot reach /home (status ")
                .i(home_st as i64)
                .s(b") -- refusing")
                .end();
            return false;
        }
    }
    // `/dev/desktop` is enumerated, not resolved: the shell serves that path, so a resolve would
    // block it on an answer only it could send. Enumerating walks the caller's own namespace
    // with no IPC.
    if expect_desktop && !bound_in(ns, "/dev/desktop") {
        log::write(b"desktop-shell: application namespace has no /dev/desktop -- refusing\n");
        return false;
    }
    // `/dev/devices` is reported, not required: absence is said and the launch goes on. Resolved,
    // as the device manager answers.
    let devices = expect_devices && {
        let st = status_of(ns, "/dev/devices", session);
        if st != 0 {
            Line::new()
                .s(b"desktop-shell: application namespace cannot reach /dev/devices (status ")
                .i(st as i64)
                .s(b")")
                .end();
        }
        st == 0
    };
    // `/storage` is reported, not required; both binds are resolved, being different bases on one
    // endpoint.
    let storage = expect_storage
        && ["/storage", "/dev/storage"].iter().all(|&at| {
            let st = status_of(ns, at, session);
            if st != 0 {
                Line::new()
                    .s(b"desktop-shell: application namespace cannot reach ")
                    .s(at.as_bytes())
                    .s(b" (status ")
                    .i(st as i64)
                    .s(b")")
                    .end();
            }
            st == 0
        });
    // `/session/user` is reported, not required; a directly bound snapshot, so nothing is
    // forwarded.
    let user = expect_user && {
        let st = status_of(ns, "/session/user", Rights::MAP_READ);
        if st != 0 {
            Line::new()
                .s(b"desktop-shell: application namespace cannot reach /session/user (status ")
                .i(st as i64)
                .s(b")")
                .end();
        }
        st == 0
    };
    Line::new()
        .s(b"desktop-shell: application namespace grants new + /home")
        .s(if user { b" + /session/user" } else { b"" })
        .s(if devices { b" + /dev/devices" } else { b"" })
        .s(if storage { b" + /storage" } else { b"" })
        .s(b", withholds manage")
        .end();
    true
}

/// Whether `path` is bound in `ns`, asked of the namespace itself: local, so it works on a path
/// this process serves, where a resolve would deadlock.
fn bound_in(ns: &Handle<Namespace, NsMutable>, path: &str) -> bool {
    ns.bindings().any(|entry| {
        entry.is_ok_and(|e| {
            let len = (e.path_len as usize).min(libkern::abi::NS_ENTRY_PATH_MAX);
            e.path.get(..len) == Some(path.as_bytes())
        })
    })
}

/// Resolve `path` in `ns` and give the handle straight back, so a resolve that mints a session
/// ends it; the status is `0` for a path that resolved, else why it did not.
fn status_of(ns: &Handle<Namespace, NsMutable>, path: &str, rights: Rights) -> i32 {
    match block_on(ns.lookup::<Resource, ReadOnly>(path, rights)) {
        Ok(_) => 0,
        Err(e) => e.code().as_i32(),
    }
}

/// Everything a launch needs: the authority this shell holds to start an application.
pub(super) struct Launcher<'a> {
    /// The session's namespace, which is where `/bin/<program>` is resolved from.
    pub(super) session_ns: Root<'a>,
    /// The compositor endpoint every application is given.
    pub(super) draw: Peer<'a>,
    /// The filesystem, the terminal server and the profile server, bound the same way.
    pub(super) fs: Peer<'a>,
    pub(super) tty: Peer<'a>,
    pub(super) profile: Peer<'a>,
    /// This shell's own `/dev/desktop`, bound into what it builds.
    pub(super) desktop: Peer<'a>,
    /// The clipboard server, bound into what it builds — see [`build_app_namespace`].
    pub(super) clipboard: Peer<'a>,
    /// The view broker, bound at `/dev/views` with [`Launcher::views_base`].
    pub(super) views: Peer<'a>,
    /// The session's view base, `/s/<id>`; empty binds no `/dev/views`.
    pub(super) views_base: &'a str,
    /// The device manager, bound at `/dev/devices` with the base `/info`.
    pub(super) devices: Peer<'a>,
    /// The storage service's session endpoint, bound at `/storage` and `/dev/storage`.
    pub(super) storage: Peer<'a>,
    /// `service-mgr`'s services endpoint, bound at `/dev/services`.
    pub(super) services: Peer<'a>,
    /// The user's home, bound as `/home` in an application's namespace.
    pub(super) home: &'a str,
    /// The session's user, bound as `/session/user` in an application's namespace; empty binds
    /// nothing.
    pub(super) user: &'a str,
    /// The environment record an application reads its `HOME` from.
    pub(super) env: &'a libstream::wire::Record,
    /// Whether a namespace this shell builds actually gates. False disables launching outright.
    pub(super) enabled: bool,
}

impl Launcher<'_> {
    /// Launch `program` into a namespace built for it, with `args` after `argv[0]`. The namespace
    /// is verified first, and a shell that finds the gate open declines
    /// ([`verify_app_namespace`]).
    pub(super) fn launch(&self, program: &str, args: &[&str]) -> bool {
        if !self.enabled {
            log::write(b"desktop-shell: launching is disabled; ignoring\n");
            return false;
        }
        launch(self, program, args)
    }
}

fn launch(l: &Launcher<'_>, program: &str, args: &[&str]) -> bool {
    let (session_ns, env) = (l.session_ns, l.env);
    if l.draw.is_none() {
        log::write(b"desktop-shell: no compositor endpoint; cannot launch\n");
        return false;
    }
    let Some(built) = build_app_namespace(l) else {
        return false;
    };
    let app_ns = built.ns;
    if !verify_app_namespace(
        &app_ns,
        !l.home.is_empty(),
        l.desktop.is_some(),
        l.devices.is_some(),
        l.storage.is_some(),
        !l.user.is_empty(),
    ) {
        drop(app_ns);
        log::write(b"desktop-shell: application namespace is not gated; refusing to launch\n");
        return false;
    }
    // The image comes from the session's `/bin`, not the application's namespace.
    let mut path = alloc::string::String::from("/bin/");
    path.push_str(program);
    let Ok(image) = block_on(session_ns.lookup::<File, MapRead>(&path, Rights::MAP_READ)) else {
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" not found in /bin").end();
        return false;
    };
    let Ok((setup_ours, setup_theirs)) = libstream::setup::open_pipe(1) else {
        log::write(b"desktop-shell: application setup channel FAIL\n");
        return false;
    };
    // The namespace this shell constructed, and no syscaps: the child's stay empty.
    let mut spawn = Spawn::new(image.as_ref());
    spawn.namespace(app_ns.as_ref()).arg0(libstream::setup::bootstrap_arg0(true));
    if spawn.grant(setup_theirs, Rights::SEND | Rights::RECV | Rights::WAIT).is_err() {
        log::write(b"desktop-shell: application setup channel FAIL\n");
        return false;
    }
    let spawned = spawn.spawn();
    // The kernel copied the ELF during spawn, and the namespace is the child's now.
    drop(image);
    drop(app_ns);
    // A failed spawn hands back the child's end, which closes with `Refused`: nothing else will
    // take it.
    let Ok(process) = spawned else {
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" spawn FAIL").end();
        return false;
    };
    // Not reaped here: this shell is not a supervisor of the applications it launches
    // (`desktop-session-mgr` reaps it). The child runs independently.
    drop(process);
    // `argv[0]` is the program name, then `args`. No streams: a terminal makes its own.
    let mut argv: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(1 + args.len());
    argv.push(program);
    argv.extend_from_slice(args);
    let sent = libstream::setup::send_stage_setup(
        &setup_ours,
        libstream::setup::StageStreams::default(),
        None,
        &argv,
        env,
        Deadline::NEVER,
    )
    .is_ok();
    drop(setup_ours);
    if !sent {
        // Reported: the application is already running and will find no setup message.
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" got no setup message").end();
    }
    Line::new().s(b"desktop-shell: launched ").s(program.as_bytes()).s(b" into its own namespace").end();
    true
}
