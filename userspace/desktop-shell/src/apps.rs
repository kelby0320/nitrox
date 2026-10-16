use super::*;

/// Read the desktop entries `/applications` projects, as the Applications menu's entries.
///
/// **`/applications` is a forwarded directory, not a set of bindings**, so `SYS_NS_ENUMERATE`
/// does not see inside it — that walks the namespace's own bindings and this is one of them. The
/// names come from a directory session, the same way `list /bin` gets `/bin`'s; each is then read
/// and parsed.
///
/// **An entry that will not parse is skipped and said so**, rather than failing the menu: one
/// broken package should lose its own applications, not everybody's — the same rule the profile
/// server applies to a package whose `bin/` will not open.
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

/// Construct the namespace one application runs in: [`libsession::build`], from the same
/// [`libsession::NamespaceSpec`] the login supervisors build sessions from (administration Part
/// F.1). This shell had a builder of its own until F.1, and it had drifted: it never bound
/// `/session/user`, so `whoami` in a desktop terminal said "no session identity".
///
/// **The load-bearing part of the shell**, and the reason it holds `BIND_NAMESPACE` at all.
/// `ui-composition-model.md` §5a rests the guarantee that *an application cannot compose other
/// applications* on the shell being the process that built their namespaces — so authority is
/// what the shell binds, not what an application asks for.
///
/// **`/dev/draw/new` is bound as its own path, with subtree base `/new`** — not the
/// `/dev/draw` subtree. That single choice is what closed the `manage-ungated` deferral, and it needed
/// no protocol change and no second endpoint:
///
/// - Resolving `/dev/draw/new` is an **exact match** against the binding, so the forwarded
///   suffix is empty, the base supplies the whole of it, and the compositor classifies `new`
///   and mints a session.
/// - Resolving `/dev/draw/manage` is **not a component-boundary prefix match** against a
///   binding of `/dev/draw/new` (`kernel/src/object/namespace.rs`, `match_suffix_offset`), so
///   nothing in this namespace answers it.
///
/// A first draft of this milestone specified a second forwarding endpoint for management,
/// couriered `init` → `service-mgr` → `desktop-session-mgr`, reasoning that the compositor
/// classifies by suffix with no caller identity so binding could not distinguish. Both
/// premises are true and the conclusion does not follow: what a namespace can *reach* is
/// decided by what it **binds**, not by how the server on the far side dispatches
/// (PR #225 review, finding 1).
///
/// **The caveat, because it decides how long this lasts.** A narrow bind expresses "`new` and
/// not `manage`". It cannot express "the `/dev/draw` subtree *minus* `manage`" — so the first
/// application that needs `/dev/draw/<id>/info` for ids it does not know in advance forces a
/// subtree bind, and `manage` comes back with it. Today nothing in `libsurface`, `libui`,
/// `libdraw` or `nxterm` resolves anything but `new`. The second endpoint is the fallback and
/// that is its trigger.
/// **What an application gets, and why each** — the reasons this shell's builder carried, kept
/// with the choice now that `libsession` does the binding:
/// - **`/dev/draw/new`, narrowly**, above.
/// - **`/home`, the user's subtree**, because the environment names it: `session_env` sets `HOME`
///   and `PWD` to `/home`, and a namespace where that resolves to nothing gives a shell whose every
///   relative path fails (PR #238 review, finding 3). Required when there is a home.
/// - **`/bin`, scoped**, because a terminal has to host a shell; scoped, so `/bin/applications`
///   does not reach the applications projection (PR #279 review, blocking 1).
/// - **No `/applications`**: nothing in an application reads it, and an application holds no
///   authority to spawn from a menu — `Desktop::Open` exists because of that.
/// - **`/dev/tty`**, the tty server, where each resolve mints a fresh terminal — so two emulators
///   share nothing, and the binding need not be per window.
/// - **`/system/fonts`**, read-only, so it can render text.
/// - **`/dev/desktop` and `/dev/clipboard` are capability decisions**, granted deliberately for v1:
///   every application in the session can create, switch and name desktops, and read what anything
///   else copied (M12 decision 1). Bound here rather than into the session, whose namespace is this
///   shell's own and has nothing else running in it (PR #239 review, finding 1).
/// - **`/dev/views` at the session's base**, so `with` in a terminal launched here reaches the view
///   broker as this session; an application cannot choose another base.
/// - **`/dev/devices`, `/storage`, `/dev/storage` and `/dev/services`**, the session's own binds of
///   info-only endpoints: none reaches a class, a mount or a start.
/// - **`/session/user`**, since F.1: who the session is for, as the session has it.
/// - **No disks.** A program reaches one only through the view broker's `disks` grant, as `with
///   admin nxinstall` does (administration Part G.3). Until then an installer session's disks were
///   rebound into every application from here.
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

/// **The session's user**, read from its `/session/user` as `whoami` reads it: the name up to the
/// first NUL or newline. Empty if the session has none, which binds none into an application.
pub(super) fn session_user(session_ns: u64) -> alloc::string::String {
    let Ok(bytes) = libfs::read_file(session_ns, b"/session/user") else {
        kprint(b"desktop-shell: the session has no /session/user\n");
        return alloc::string::String::new();
    };
    let end = bytes.iter().position(|&b| b == 0 || b == b'\n').unwrap_or(bytes.len());
    alloc::string::String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Check the application namespace grants `new` and withholds `manage`, before anything runs
/// in it.
///
/// **Verified rather than assumed, and by the process that built it.** The narrow bind is the
/// whole of the `manage-ungated` deferral's answer, and it rests on a kernel matching rule
/// (`match_suffix_offset`) that this file does not own. A shell that constructed the namespace
/// wrongly and launched into it anyway would hand an application the manager channel — the
/// exact thing the deferral is about — and nothing downstream would notice, because an
/// application that *can* reach `manage` simply never says so.
///
/// Returns `false` if the namespace is not what it should be; the caller declines to launch.
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
    // **A refusal is not the same as an absence, and treating them alike made this check
    // pass for the exact mis-construction it exists to catch.**
    //
    // Once this shell holds the manager channel, the compositor answers a *second* `manage`
    // resolve with `WouldBlock` — the first-come rule, nothing to do with whether the path is
    // bound. So a namespace that wrongly bound the whole `/dev/draw` subtree looked identical
    // to one that bound `new` alone, and the launch-time check announced "withholds manage"
    // while handing an application the subtree. Demonstrated in review by widening the
    // namespace on the launch path only: the gate went green (PR #237 review, finding 3).
    //
    // `WouldBlock` means the resolve **reached the compositor**, which is precisely what must
    // not happen. Only `NotFound` — nothing in this namespace answers that path — is the
    // property being checked.
    if manage_st != KError::NotFound.as_i32() {
        Line::new()
            .s(b"desktop-shell: application namespace can REACH /dev/draw/manage (status ")
            .i(manage_st as i64)
            .s(b") -- refusing")
            .end();
        return false;
    }
    // **`/home` too, because the environment names it.** An application starts with
    // `PWD=/home`, so a namespace where that does not resolve gives a shell whose every
    // relative path fails — and nothing downstream reports it, since a terminal's output goes
    // to the grid and the grid renders only under `test-harness`. Checked by the process that
    // built the namespace, for the same reason `manage` is (PR #238 review, finding 3).
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
    // **`/dev/desktop` is checked by *enumerating* the namespace, not by resolving it.**
    //
    // The shell serves that path, and a resolve is forwarded to whoever serves it — so asking
    // the kernel to resolve its own endpoint blocks this process inside `ns_lookup` waiting for
    // an answer only it could send. That is real, and it is the same self-deadlock the bottom
    // bar hit in Part C by another route.
    //
    // **But "cannot resolve it" is not "cannot check it", which is where the first version of
    // this stopped.** `SYS_NS_ENUMERATE` walks the caller's *own* namespace and copies out one
    // entry per call: local, no IPC, nothing forwarded — `nxsh`'s `binding_at_or_under` uses the
    // same idiom. So the binding gets a check on the same footing as `new` granted and `manage`
    // withheld, rather than the bind call's return value standing in for one, which was a proxy
    // where a real check was available (PR #245 review, finding 3).
    if expect_desktop && !bound_in(ns, "/dev/desktop") {
        kprint(b"desktop-shell: application namespace has no /dev/desktop -- refusing\n");
        return false;
    }
    // **`/dev/devices` is reported, not required** (administration Part B.4). It is information —
    // an application that cannot list the machine's devices still runs — so its absence is said
    // and the launch goes on. Resolved rather than enumerated: it is the device manager that
    // answers, not this shell, so there is no deadlock to avoid, and a resolve proves the binding
    // reaches a manager serving the tables, where a binding's presence would not.
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
    // **`/storage` is reported, not required** (administration Part C.6), for `/dev/devices`'
    // reason: an application with no filesystems beyond its home still runs. Both of its binds are
    // resolved, since each is a different base on the one endpoint and either could be wrong.
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
    // **`/session/user` is reported, not required** (administration Part F.1), for `/dev/devices`'
    // reason: an application that cannot say whose session it is in still runs. Resolved: it is a
    // snapshot bound directly, so nothing is forwarded and nothing can deadlock.
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

/// Whether `path` is bound in `ns` — asked of the namespace itself, not of whoever serves it.
///
/// **Local and cheap**: the kernel walks the caller's own namespace and copies out one entry per
/// call, with no forwarding anywhere in it. That is what makes it usable on a path this process
/// *serves*, where a resolve would deadlock.
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

/// Resolve `path` in `ns`, returning `(status, handle)`.
///
/// **Async, like every potentially-blocking syscall here**: `SYS_NS_LOOKUP` returns a
/// `PendingOperation` to wait on, and the status and handle are read out of the wait result —
/// not an out-param. A first version of this wrote it synchronously and every resolve
/// "failed", which is what a `PendingOperation` handle looks like when you read it as a
/// status.
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

/// Spawn args for a launched application: the namespace this shell constructed, and **no
/// syscaps at all**.
///
/// An application constructs nothing and registers nothing. Whatever it can reach was bound
/// into its namespace by this process — `ui-composition-model.md` §5a's guarantee that an
/// application cannot compose other applications is exactly this line plus
/// [`build_app_namespace`].
static mut SPAWN_APP: SpawnArgs = SpawnArgs {
    image: 0,
    // One handle: the setup channel. An application is a **Tier-1** stage — it receives its
    // `argv` and its environment the way every pipeline stage does, rather than through a
    // special case.
    handle_count: 1,
    move_mask: 1,
    arg0: 0,
    handles: [0; 4],
    rights: [RIGHT_SEND | RIGHT_RECV | RIGHT_WAIT, 0, 0, 0],
    namespace: 0, // set per launch = the namespace built for it
    syscaps: 0,   // empty, and it stays empty
};

/// Everything a launch needs, gathered once.
///
/// **A struct because there is a second caller now.** The applications modal was the only one
/// until M10 Part D added `Desktop::Open` — a client asking the shell to open a path — and the
/// nine values a launch needs were about to be threaded through the desktop session handler as
/// nine more parameters. What they actually are is one thing: the authority this shell holds to
/// start an application.
pub(super) struct Launcher<'a> {
    /// The session's namespace, which is where `/bin/<program>` is resolved from.
    pub(super) session_ns: u64,
    /// The compositor endpoint every application is given.
    pub(super) draw: u64,
    /// The filesystem, the terminal server and the profile server, bound the same way.
    pub(super) fs: u64,
    /// See [`Launcher::fs`].
    pub(super) tty: u64,
    /// See [`Launcher::fs`].
    pub(super) profile: u64,
    /// This shell's own `/dev/desktop`, bound into what it builds.
    pub(super) desktop: u64,
    /// The clipboard server, bound into what it builds — see [`build_app_namespace`].
    pub(super) clipboard: u64,
    /// The view broker, bound at `/dev/views` with [`Launcher::views_base`] (administration Part A).
    pub(super) views: u64,
    /// The session's view base, `/s/<id>`; empty binds no `/dev/views`.
    pub(super) views_base: &'a str,
    /// The device manager, bound at `/dev/devices` with the base `/info` (administration Part B).
    pub(super) devices: u64,
    /// The storage service's session endpoint, bound at `/storage` and `/dev/storage`
    /// (administration Part C.6).
    pub(super) storage: u64,
    /// `service-mgr`'s services endpoint, bound at `/dev/services` (administration Part E.2b).
    pub(super) services: u64,
    /// The user's home, bound as `/home` in an application's namespace.
    pub(super) home: &'a str,
    /// The session's user, bound as `/session/user` in an application's namespace (administration
    /// Part F.1). Empty binds nothing.
    pub(super) user: &'a str,
    /// The environment record an application reads its `HOME` from.
    pub(super) env: &'a libstream::wire::Record,
    /// Whether a namespace this shell builds actually gates. False disables launching outright.
    pub(super) enabled: bool,
}

impl Launcher<'_> {
    /// Launch `program` into a namespace built for it, with `args` after `argv[0]`.
    ///
    /// **The namespace is verified before anything runs in it**, and a shell that finds the gate
    /// open declines to launch. See [`verify_app_namespace`] for why that is behaviour rather
    /// than a test: an application that *can* reach `manage` never says so, and nothing
    /// downstream would notice.
    pub(super) fn launch(&self, program: &str, args: &[&str]) -> bool {
        if !self.enabled {
            kprint(b"desktop-shell: launching is disabled; ignoring\n");
            return false;
        }
        launch(self, program, args)
    }
}

/// The body of [`Launcher::launch`], kept a free function so the long sequence of handle
/// bookkeeping reads as it did before the context was gathered.
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
    // The image comes from the **session's** `/bin`, not the application's: the shell resolves
    // what to run, and the namespace it built is what the program will run *in*.
    let mut path = alloc::string::String::from("/bin/");
    path.push_str(program);
    let (st, image) = ns_lookup(session_ns, path.as_bytes(), RIGHT_MAP_READ);
    if st != 0 || image == 0 {
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" not found in /bin").end();
        // SAFETY: closing the namespace we built for a launch that will not happen.
        unsafe { syscall1(SYS_HANDLE_CLOSE, app_ns) };
        return false;
    }
    // The setup channel this application will read its `argv` and environment from. Depth
    // one, because one message is what goes down it — sized from the payload, which is the
    // lesson `spawn_leader`'s `pipe(4)` taught when a fifth message was dropped silently.
    let Ok((setup_ours, setup_theirs)) = libstream::setup::pipe(1) else {
        kprint(b"desktop-shell: application setup channel FAIL\n");
        // SAFETY: closing handles for a launch that will not happen.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, image);
            syscall1(SYS_HANDLE_CLOSE, app_ns);
        }
        return false;
    };
    // SAFETY: SPAWN_APP is a valid writable arg block.
    let h = unsafe {
        SPAWN_APP.image = image;
        SPAWN_APP.namespace = app_ns;
        SPAWN_APP.handles[0] = setup_theirs;
        SPAWN_APP.arg0 = libstream::setup::bootstrap_arg0(true);
        syscall1(SYS_PROCESS_SPAWN, (&raw const SPAWN_APP) as u64)
    };
    // The kernel copied the ELF during spawn, and the namespace is the child's now.
    // SAFETY: closing our own handles.
    unsafe {
        syscall1(SYS_HANDLE_CLOSE, image);
        syscall1(SYS_HANDLE_CLOSE, app_ns);
    }
    if h < 0 {
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" spawn FAIL").end();
        // **Both ends, because nothing else will take them.** A spawn that fails leaves the
        // child's end still ours — `handles[0]` transfers on success only — so returning here
        // without closing leaks two handles per failed launch, in a process that lives for the
        // whole session and can be asked to launch again and again (PR #238 review, finding 7).
        // SAFETY: closing a setup channel pair for a process that does not exist.
        unsafe {
            syscall1(SYS_HANDLE_CLOSE, setup_ours);
            syscall1(SYS_HANDLE_CLOSE, setup_theirs);
        }
        return false;
    }
    // **Not reaped here.** This shell is not a supervisor of the applications it launches —
    // `desktop-session-mgr` reaps *it*, and an application's exit is the compositor noticing
    // its windows go away. Holding the process handle would make the shell responsible for a
    // lifecycle it has no opinion about.
    // SAFETY: closing the process handle; the child runs independently.
    unsafe { syscall1(SYS_HANDLE_CLOSE, h as u64) };
    // `argv[0]` is the program name, as every Tier-1 stage's is, and `args` follows it — which
    // for an editor is the file it was asked to open. No streams: an application is not a
    // pipeline stage with a parent reading its output — a terminal makes its own.
    let mut argv: alloc::vec::Vec<&str> = alloc::vec::Vec::with_capacity(1 + args.len());
    argv.push(program);
    argv.extend_from_slice(args);
    let sent = libstream::setup::send_setup_env(setup_ours, &libstream::setup::Streams::default(), &argv, env).is_ok();
    // SAFETY: closing our end of the setup channel.
    unsafe { syscall1(SYS_HANDLE_CLOSE, setup_ours) };
    if !sent {
        // Reported rather than swallowed: the application is already running and will find no
        // setup message, which presents as a program with no environment rather than as a
        // failure to launch.
        Line::new().s(b"desktop-shell: ").s(program.as_bytes()).s(b" got no setup message").end();
    }
    Line::new().s(b"desktop-shell: launched ").s(program.as_bytes()).s(b" into its own namespace").end();
    true
}
