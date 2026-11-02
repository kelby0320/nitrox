//! Launching applications, each in a namespace built and checked for it.

use super::*;

/// Read the desktop entries `/applications` projects; an entry that will not parse is skipped and
/// said so, losing only its own application.
pub(super) fn read_applications(ns: u64) -> alloc::vec::Vec<Application> {
    use librsproto::session::Dir;
    let mut names = alloc::vec::Vec::new();
    let mut buf = [0u8; 4096];
    let Ok(mut dir) = Dir::open(ns, b"/applications", &mut buf) else {
        kprint(b"desktop-shell: /applications did not open; the Applications menu will be empty\n");
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
        let Ok(bytes) = libfs::read_file(ns, path.as_bytes()) else {
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

/// Construct the namespace one application runs in: [`libsession::build`] from the same spec the
/// login supervisors use.
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
pub(super) fn build_app_namespace(l: &Launcher<'_>) -> Option<libsession::Built> {
    let built = libsession::build(&libsession::NamespaceSpec {
        root_ns: l.session_ns,
        fs_endpoint: l.fs,
        profile_endpoint: l.profile,
        tty_endpoint: l.tty,
        clipboard_endpoint: l.clipboard,
        home: l.home.as_bytes(),
        user: l.user.as_bytes(),
        bind_fonts: true,
        bind_console: false,
        views_endpoint: l.views,
        views_base: l.views_base.as_bytes(),
        devices_endpoint: l.devices,
        storage_endpoint: l.storage,
        services_endpoint: l.services,
        bind_applications: false,
        draw_endpoint: l.draw,
        desktop_endpoint: l.desktop,
    })?;
    if built.desktop {
        kprint(b"desktop-shell: application /dev/desktop bound\n");
    }
    Some(built)
}

/// The session's user, read from `/session/user` as `whoami` does; empty binds none.
pub(super) fn session_user(session_ns: u64) -> alloc::string::String {
    let Ok(bytes) = libfs::read_file(session_ns, b"/session/user") else {
        kprint(b"desktop-shell: the session has no /session/user\n");
        return alloc::string::String::new();
    };
    let end = bytes.iter().position(|&b| b == 0 || b == b'\n').unwrap_or(bytes.len());
    alloc::string::String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Check the application namespace grants `new` and withholds `manage`, before anything runs in
/// it, by the process that built it: an application that can reach `manage` never says so.
/// `false` means the caller declines to launch.
pub(super) fn verify_app_namespace(
    ns: u64,
    expect_home: bool,
    expect_desktop: bool,
    expect_devices: bool,
    expect_storage: bool,
    expect_user: bool,
) -> bool {
    let (new_st, new_h) = ns_lookup(ns, b"/dev/draw/new", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if new_h != 0 {
        // SAFETY: closing a session this check minted; the application will make its own.
        unsafe { syscall1(SYS_HANDLE_CLOSE, new_h) };
    }
    let (manage_st, manage_h) = ns_lookup(ns, b"/dev/draw/manage", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
    if manage_h != 0 {
        // SAFETY: closing a handle this check should never have obtained.
        unsafe { syscall1(SYS_HANDLE_CLOSE, manage_h) };
    }
    if new_st != 0 || new_h == 0 {
        kprint(b"desktop-shell: application namespace cannot reach /dev/draw/new\n");
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
        let (home_st, home_h) = ns_lookup(ns, b"/home", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
        if home_h != 0 {
            // SAFETY: closing a session this check minted; the application will make its own.
            unsafe { syscall1(SYS_HANDLE_CLOSE, home_h) };
        }
        if home_st != 0 || home_h == 0 {
            Line::new()
                .s(b"desktop-shell: application namespace cannot reach /home (status ")
                .i(home_st as i64)
                .s(b") -- refusing")
                .end();
            return false;
        }
    }
    // `/dev/desktop` is enumerated, not resolved: the shell serves that path, so a resolve would
    // block it on an answer only it could send. `SYS_NS_ENUMERATE` walks the caller's own
    // namespace with no IPC.
    if expect_desktop && !bound_in(ns, "/dev/desktop") {
        kprint(b"desktop-shell: application namespace has no /dev/desktop -- refusing\n");
        return false;
    }
    // `/dev/devices` is reported, not required: absence is said and the launch goes on. Resolved,
    // as the device manager answers.
    let devices = expect_devices && {
        let (st, dir) = ns_lookup(ns, b"/dev/devices", RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
        if dir != 0 {
            // SAFETY: closing a directory session this check minted.
            unsafe { syscall1(SYS_HANDLE_CLOSE, dir) };
        }
        if st != 0 || dir == 0 {
            Line::new()
                .s(b"desktop-shell: application namespace cannot reach /dev/devices (status ")
                .i(st as i64)
                .s(b")")
                .end();
        }
        st == 0 && dir != 0
    };
    // `/storage` is reported, not required; both binds are resolved, being different bases on one
    // endpoint.
    let storage = expect_storage
        && [&b"/storage"[..], b"/dev/storage"].iter().all(|at| {
            let (st, dir) = ns_lookup(ns, at, RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT);
            if dir != 0 {
                // SAFETY: closing a directory session this check minted.
                unsafe { syscall1(SYS_HANDLE_CLOSE, dir) };
            }
            if st != 0 || dir == 0 {
                Line::new()
                    .s(b"desktop-shell: application namespace cannot reach ")
                    .s(at)
                    .s(b" (status ")
                    .i(st as i64)
                    .s(b")")
                    .end();
            }
            st == 0 && dir != 0
        });
    // `/session/user` is reported, not required; a directly bound snapshot, so nothing is
    // forwarded.
    let user = expect_user && {
        let (st, h) = ns_lookup(ns, b"/session/user", RIGHT_MAP_READ);
        if h != 0 {
            // SAFETY: closing a handle this check obtained.
            unsafe { syscall1(SYS_HANDLE_CLOSE, h) };
        }
        if st != 0 || h == 0 {
            Line::new()
                .s(b"desktop-shell: application namespace cannot reach /session/user (status ")
                .i(st as i64)
                .s(b")")
                .end();
        }
        st == 0 && h != 0
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
fn bound_in(ns: u64, path: &str) -> bool {
    let mut entry = libkern::abi::NsEntry::zeroed();
    for index in 0u64.. {
        // SAFETY: `entry` is a valid writable out-param of exactly `NsEntry`'s layout.
        let r = unsafe { syscall3(SYS_NS_ENUMERATE, ns, index, (&raw mut entry) as *mut _ as u64) };
        if r != 0 {
            return false; // NotFound ends the walk
        }
        let len = (entry.path_len as usize).min(libkern::abi::NS_ENTRY_PATH_MAX);
        // SAFETY: the kernel wrote the binding path; `len` is clamped to the buffer.
        let bound = unsafe { core::slice::from_raw_parts((&raw const entry.path) as *const u8, len) };
        if bound == path.as_bytes() {
            return true;
        }
    }
    false
}

/// Resolve `path` in `ns`, returning `(status, handle)`. `SYS_NS_LOOKUP` returns a
/// `PendingOperation`; the status and handle are read from the wait result, not an out-param.
fn ns_lookup(ns: u64, path: &[u8], rights: u64) -> (i32, u64) {
    // SAFETY: valid path pointer + namespace handle.
    let po = unsafe { syscall4(SYS_NS_LOOKUP, ns, path.as_ptr() as u64, path.len() as u64, rights) };
    if po < 0 {
        return (po as i32, 0);
    }
    if !wait_one(po as u64) {
        // SAFETY: closing our own PO.
        unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
        return (-1, 0);
    }
    // SAFETY: the kernel wrote one 24-byte IoResult: status at 8, handle at 16.
    let (status, handle) = unsafe {
        (
            i32::from_le_bytes([WAIT_RESULTS[8], WAIT_RESULTS[9], WAIT_RESULTS[10], WAIT_RESULTS[11]]),
            u64::from_le_bytes([
                WAIT_RESULTS[16],
                WAIT_RESULTS[17],
                WAIT_RESULTS[18],
                WAIT_RESULTS[19],
                WAIT_RESULTS[20],
                WAIT_RESULTS[21],
                WAIT_RESULTS[22],
                WAIT_RESULTS[23],
            ]),
        )
    };
    // SAFETY: closing our own PO.
    unsafe { syscall1(SYS_HANDLE_CLOSE, po as u64) };
    (status, if status == 0 { handle } else { 0 })
}

/// Everything a launch needs: the authority this shell holds to start an application.
pub(super) struct Launcher<'a> {
    /// The session's namespace, which is where `/bin/<program>` is resolved from.
    pub(super) session_ns: u64,
    /// The compositor endpoint every application is given.
    pub(super) draw: u64,
    /// The filesystem, the terminal server and the profile server, bound the same way.
    pub(super) fs: u64,
    pub(super) tty: u64,
    pub(super) profile: u64,
    /// This shell's own `/dev/desktop`, bound into what it builds.
    pub(super) desktop: u64,
    /// The clipboard server, bound into what it builds — see [`build_app_namespace`].
    pub(super) clipboard: u64,
    /// The view broker, bound at `/dev/views` with [`Launcher::views_base`].
    pub(super) views: u64,
    /// The session's view base, `/s/<id>`; empty binds no `/dev/views`.
    pub(super) views_base: &'a str,
    /// The device manager, bound at `/dev/devices` with the base `/info`.
    pub(super) devices: u64,
    /// The storage service's session endpoint, bound at `/storage` and `/dev/storage`.
    pub(super) storage: u64,
    /// `service-mgr`'s services endpoint, bound at `/dev/services`.
    pub(super) services: u64,
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
            kprint(b"desktop-shell: launching is disabled; ignoring\n");
            return false;
        }
        launch(self, program, args)
    }
}

fn launch(l: &Launcher<'_>, program: &str, args: &[&str]) -> bool {
    let (session_ns, draw, desktop, home, env) = (l.session_ns, l.draw, l.desktop, l.home, l.env);
    let (devices, storage) = (l.devices, l.storage);
    if draw == 0 {
        kprint(b"desktop-shell: no compositor endpoint; cannot launch\n");
        return false;
    }
    let Some(built) = build_app_namespace(l) else {
        return false;
    };
    let app_ns = built.ns;
    if !verify_app_namespace(app_ns, !home.is_empty(), desktop != 0, devices != 0, storage != 0, !l.user.is_empty()) {
        // SAFETY: closing the namespace; nothing was launched into it.
        unsafe { syscall1(SYS_HANDLE_CLOSE, app_ns) };
        kprint(b"desktop-shell: application namespace is not gated; refusing to launch\n");
        return false;
    }
    // The image comes from the session's `/bin`, not the application's namespace.
    let mut path = alloc::string::String::from("/bin/");
    path.push_str(program);
    let (st, image) = ns_lookup(session_ns, path.as_bytes(), RIGHT_MAP_READ);
    if st != 0 || image == 0 {
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" not found in /bin").end();
        // SAFETY: closing the namespace we built for a launch that will not happen.
        unsafe { syscall1(SYS_HANDLE_CLOSE, app_ns) };
        return false;
    }
    let Ok((setup_ours, setup_theirs)) = libstream::setup::pipe(1) else {
        kprint(b"desktop-shell: application setup channel FAIL\n");
        // SAFETY: closing handles for a launch that will not happen.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, image);
            syscall1(SYS_HANDLE_CLOSE, app_ns);
        }
        return false;
    };
    // The namespace this shell constructed, and no syscaps: the child's stay empty.
    let spawn = SpawnArgs {
        image,
        handle_count: 1,
        move_mask: 1,
        arg0: libstream::setup::bootstrap_arg0(true),
        handles: [setup_theirs, 0, 0, 0],
        rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT, 0, 0, 0],
        namespace: app_ns,
        syscaps: 0,
    };
    // SAFETY: `spawn` is a valid arg block on this frame.
    let h = unsafe { syscall1(SYS_PROCESS_SPAWN, (&raw const spawn) as u64) };
    // The kernel copied the ELF during spawn, and the namespace is the child's now.
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, image);
        syscall1(SYS_HANDLE_CLOSE, app_ns);
    }
    if h < 0 {
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" spawn FAIL").end();
        // Both ends, because nothing else will take them: a failed spawn leaves the child's end
        // ours (`handles[0]` transfers on success only).
        // SAFETY: closing a setup channel pair for a process that does not exist.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, setup_ours);
            syscall1(SYS_HANDLE_CLOSE, setup_theirs);
        }
        return false;
    }
    // Not reaped here: this shell is not a supervisor of the applications it launches
    // (`desktop-session-mgr` reaps it).
    // SAFETY: closing the process handle; the child runs independently.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
    // `argv[0]` is the program name, then `args`. No streams: a terminal makes its own.
    let mut argv: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(1 + args.len());
    argv.push(program);
    argv.extend_from_slice(args);
    let sent = libstream::setup::send_setup_env(setup_ours, libstream::setup::Streams::default(), &argv, env).is_ok();
    // SAFETY: closing our end of the setup channel.
    unsafe { syscall1(SYS_HANDLE_CLOSE, setup_ours) };
    if !sent {
        // Reported: the application is already running and will find no setup message.
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" got no setup message").end();
    }
    Line::new().s(b"desktop-shell: launched ").s(program.as_bytes()).s(b" into its own namespace").end();
    true
}
